# Spec: Persisted session trees

## Context

Kiki currently keeps model context, display history and arena checkpoints in memory. Exiting loses that state, and there is no way to select an earlier point and continue along another branch (`src/runtime.rs:239`, `src/agent.rs:435`). Arena checkpoint restoration already exists, but is not connected to durable history (`src/arena.rs:1067`).

This change implements the session-tree direction in `docs/VISION.md:28`: preserve history and Python state together, resume sessions after restart, and explore alternative paths without deleting previous work.

The selected history determines both model context and arena state. Rewinding does not undo filesystem changes, commands or other external effects. Compaction and branch summaries remain separate follow-up work.

## Goals

- Persist accepted requests, completed model steps, arena checkpoints and the active leaf, retaining every alternative branch.
- Resume sessions by CLI ID; provide timestamped, optionally named CLI history listings and print the session ID on exit.
- Navigate `/tree` and continue from earlier points with matching model context and arena state.
- Restore earlier requests for correction, including the safe two-second cancellation shortcut, without overwriting editor drafts.
- Recover interrupted sessions without automatically rerunning Python or subagents; stop further work if persistence fails.
- Preserve subagent results while restricting agent access to the selected history and existing descendant permissions.

## Non-goals

- Compaction, branch summaries, branch merging and bookmarks.
- In-client switching between sessions, separate-session forks/clones, export or sharing.
- Reversing external effects or rewriting retained paths when using `--here`.
- Resuming unfinished work or restoring live subagent execution. Preserve child history and results, not resumable child sessions.
- Automatic Monty installation or checkpoint migration.
- Saving unsent editor drafts or automatically pruning retained history.
- Adding a host-filesystem sandbox. Subagent APIs enforce branch visibility, but `run` retains its existing host authority.

## Key discoveries

- History has separate in-memory owners. The runtime retains display records, the agent owns model context, and the TUI maintains its own display projection (`src/runtime.rs:239`, `src/agent.rs:435`, `src/transcript.rs:2217`).
- Checkpoints contain more than Python variables. They include the Monty runtime identity and whole-file write authorisations. Restoration already rejects incompatible runtime identities (`src/arena.rs:66`, `src/arena.rs:1067`).
- Display records are insufficient for resume. Future model requests also need preserved provider replay, call IDs and execution feedback, including cancellation/failure feedback (`src/model.rs:90`, `src/openai.rs:173`, `src/agent.rs:663`).
- Completion currently has no durability barrier. The worker reports an execution and can continue before the controller processes it. Persistence must introduce acknowledgement before further work (`src/agent.rs:643`, `src/runtime.rs:776`).
- Subagent results live outside checkpoints. The host retains task outcomes, and root lookup currently permits every task in the session. Selected-history access checks are new behaviour (`src/subagents.rs:1326`, `src/subagents.rs:2559`).
- Workspace selection affects real tool access. Startup takes the current directory and opens the tool host there. `--here` therefore changes where tools operate, not just session metadata (`src/config.rs:425`, `src/tools.rs:1658`).

## Proposed approach

### Durable session model

A front-end-independent session model owns immutable, parent-linked entries, the active leaf, arena checkpoints and retained subagent history. Model context and display history are separate projections of that state.

Store each session in `~/.kiki/sessions/<id>.sqlite3`. Keep checkpoint bytes inside the database so history, checkpoint references and active-leaf changes can commit atomically.

Persist an accepted request before its first model call. Persist each settled model step before further model or Python work begins. Never hold a database transaction open during model calls or execution.

Store code and its result together, including cancellation/failure feedback. Each restorable point references the arena state it actually leaves behind. Requests and final replies reuse the preceding checkpoint. Preserve provider replay and accounting alongside human-facing records. Streamed prefixes remain display-only, never executable history or branch points.

Persist active-leaf changes even when navigation is followed immediately by exit. Navigating does not create a conversation entry; submitting afterwards creates a child of the selected point.

Keep session name, saved working directory and latest model/thinking preferences as session-wide metadata, unaffected by rewind.

Allow one writable Kiki process per session. A second attempt to resume that session fails clearly; history listing can still read its metadata. Version the storage format and reject unsupported versions without modifying them.

### Database schema

Use six tables. `runs` and `model_steps` record operations that may not yet have a history entry; they are operational bookkeeping, not mutable conversation history. This is the logical schema, not final migration SQL. `?` means nullable and `→` marks a foreign key.

#### Session metadata

```text
session                          single row
  id                    TEXT     public session ID
  created_at_ms         INTEGER
  updated_at_ms         INTEGER
  cwd                   TEXT
  name                  TEXT?
  active_entry_id       INTEGER  → entries.id
  preferences_json      TEXT
  revision              INTEGER
```

`preferences_json` holds the selected model and accepted per-model thinking levels. Name, directory and preferences are session-wide, so navigation changes only the active leaf, not these preferences. `revision` increases across committed changes, never rewinds, and provides ordering without relying on timestamps being distinct.

#### Arena checkpoints

```text
checkpoints                      immutable
  id                    INTEGER PK
  runtime_identity      TEXT
  arena_state           BLOB
  authorisations_json   TEXT
```

Store the complete restorable checkpoint: Monty bytes, compatible runtime identity and whole-file write authorisations. Multiple entries can reference the same checkpoint. User requests and final replies do not duplicate arena dumps.

#### Session entries

```text
entries                          immutable
  id                    INTEGER PK
  parent_id             INTEGER? → entries.id
  run_id                INTEGER? → runs.id
  kind                  TEXT
  checkpoint_id         INTEGER  → checkpoints.id
  payload_json          TEXT
  provider_replay        BLOB?
  created_at_ms         INTEGER
  revision              INTEGER
```

One `session_start` root entry holds the initial checkpoint. Only that entry has no parent or run. Subsequent kinds represent user requests, settled executions, final replies and terminal failures/cancellations/interruptions.

`payload_json` stores typed Kiki content: source, output, errors, feedback, reasoning summaries and accounting as applicable. `provider_replay` preserves opaque provider data separately from human-facing content. Model context and display history are derived independently; do not store a complete transcript or model-history copy at every node.

There is no `branches` table: branches are the parent links, and the active leaf selects one path.

#### Foreground run lifecycle

```text
runs
  id                    INTEGER PK
  terminal_entry_id     INTEGER? → entries.id
```

A row without a terminal entry represents unfinished foreground work. Its user request is an entry belonging to that run. Creating the run and its request happens in one transaction. Closing it and recording its terminal outcome also happen together. Do not introduce a second status field that could disagree with the recorded outcome.

#### Model-step lifecycle and provenance

```text
model_steps
  id                    INTEGER PK
  run_id                INTEGER  → runs.id
  settled_entry_id      INTEGER? → entries.id
```

Allocate this row durably before starting the step. While it is pending, the active leaf remains its preceding committed history: no later foreground step or navigation may advance it. When the step settles, its new entry and `settled_entry_id` commit together; the entry's parent identifies that preceding history.

A subagent can start before its originating Python execution has a completed history entry. Suppose step 7 starts task `00`, then finishes as entry E2:

```text
E1 ─┬─ E2 [step 7 started task 00] ─ E3
    └─ E4
```

Task `00` can reference step 7 immediately, before E2 exists. Afterwards, access checks use E2's membership in the selected ancestry: accessible from E3, inaccessible from E4 or E1. During step 7, the task is accessible to its authorised active callers. Checking against E1 instead would incorrectly expose it to the sibling branch.

#### Subagent tasks

```text
subagent_tasks
  id                    INTEGER PK
  parent_task_id        INTEGER? → subagent_tasks.id
  origin_step_id        INTEGER  → model_steps.id
  state                 TEXT
  admitted_revision     INTEGER
  terminal_revision     INTEGER?
  task_revision         INTEGER
  details_json          TEXT
```

IDs retain the existing hexadecimal presentation outside SQL. `details_json` holds task parameters, retained child execution/attempt history, observations, terminal results, admission/terminal UTC timestamps and recorded elapsed durations. Timing information remains available for inspection; it does not need separate SQL columns. Structural columns remain authoritative rather than being duplicated inside that JSON.

Retain measured elapsed durations rather than recomputing them from wall-clock differences. For interrupted tasks, elapsed time is the last recorded observation, not a claim about the exact time the process stopped; the recovery timestamp identifies when interruption was recorded. Do not count time between processes as child runtime.

Keep the latest committed task state and accumulated history, not every transient status snapshot. `admitted_revision` and `terminal_revision` order Spawned and terminal history cards against entry revisions in the session-wide timeline. `task_revision` instead identifies the task producer's latest snapshot and rejects stale updates; it cannot order records from different tasks or foreground runs.

Descendants inherit the originating foreground step, even if they start after that step's run has finished. Existing child-descendant permissions remain in force in addition to selected-history visibility.

#### Commit rules and constraints

- Accepting a request atomically creates its run and user-request entry beneath the current active leaf, then advances `session.active_entry_id` to the request before model work begins.
- Step completion commits the checkpoint, entry, `model_steps.settled_entry_id` and active-leaf update together.
- Closing a run atomically records its terminal entry, sets `runs.terminal_entry_id` and advances the active leaf to that entry. A final message can settle both its step and run in this transaction; do not create a duplicate terminal entry.
- If cancellation, failure or interruption ends a pending step, set its `settled_entry_id` to the terminal entry in that same transaction. This includes generation failures without a complete model response; streamed prefixes still do not enter model context. A closed run has no pending steps. Cancellation between steps does not create a new step or change an already-settled step's link.
- Task admission persists its row before starting the child.
- Root interruption uses the last committed checkpoint. Background-only interruption settles unfinished tasks as failed with an interruption diagnostic, without inventing another foreground outcome or moving the active leaf.
- Enforce foreign keys, one root, older-parent links, one open foreground run and guarded once-only settlement transitions.
- Add only the indexes needed for parent traversal, run/step lookup and task ancestry. No full-text index, branch catalogue, deduplication layer or migration framework in this change.

### Navigation and correction

#### Tree navigation

- `/tree` opens the searchable, indented picker once foreground and background work have settled. Otherwise, show a busy notice; do not queue navigation or cancel work.
- Show the session start, user requests, settled execution/result pairs and terminal outcomes. Mark the active path and current point.
- Enter restores the highlighted point; Esc closes without changes. Selecting a point never starts model work automatically.
- Selecting a user request restores its parent's context and arena. Populate the original request only if the editor is empty; otherwise keep the existing draft.
- Selecting a settled step or terminal outcome restores history through that entry. Selecting session start restores the initial arena and empty model context. These selections leave editor text unchanged.
- Preserve editor text present when restoration is applied, including edits made while restoration is pending. An existing draft does not block navigation.
- Publish the new display history only after restoration and the active-leaf update succeed. On failure, retain the previous selection; disable further work if its arena cannot be recovered.

#### Two-second correction shortcut

- Esc during an active run qualifies when pressed at most two seconds after submission. Measure the keypress time, not when cancellation finishes.
- Cancel normally first. Once cancellation settles, restore the state before that request only if all background work is also settled. Populate the original request only if the editor is empty; preserve any existing draft, including edits made while cancellation or restoration was pending.
- If background work is not settled, keep ordinary cancellation behaviour and explain why automatic restoration was skipped. No delayed rewind or implicit background cancellation.
- Preserve the cancelled path, completed work and accounting in the tree. Correcting and resubmitting creates an alternative branch.
- Already-settled runs do not qualify. Later or repeated Esc presses retain existing cancellation behaviour.

### CLI and session metadata

```text
kiki                           Start a new session
kiki sessions                  List sessions for this directory
kiki --resume <id>              Resume the saved active leaf
kiki --resume <id> --here       Resume against the current directory
```

- `kiki sessions` shows session ID, last saved activity timestamp and name, falling back to a first-request preview when unnamed or `(empty session)` when there is no request. Sort newest first. Listing requires neither credentials nor Monty and does not modify sessions.
- Resume accepts a session identifier, not a database path. Validate identifiers before filesystem lookup, reject path separators/traversal, and require the database's stored ID to match the requested identifier.
- Normal resume requires the saved working directory. Compare resolved directory paths so equivalent symlink spellings do not cause a false mismatch.
- `--here` explicitly relocates the same session. After successful restoration, save the new directory and warn that retained paths/data and external effects have not been rewritten. No Git-specific behaviour.
- Persist `/name` and accepted model/thinking changes. Navigating the tree does not revert them. `Ctrl-S` continues to save user defaults separately.
- On resume, saved model/thinking preferences override ordinary startup defaults; explicit `KIKI_MODEL` and `KIKI_THINKING` environment overrides still win.
- After an opened session exits, print `Session: <id>` after terminal restoration, including handled runtime failures. Forced termination cannot guarantee this message.
- Unknown IDs, directory mismatches and sessions already owned by another process produce clear errors. They never silently start a different session.

### Recovery and projections

#### Recovery and failures

- Resume from the saved active leaf and its committed checkpoint, not whichever entry was written most recently.
- If a foreground run was unfinished, record its interruption and close any pending model step using the last committed arena state. Never replay unfinished work.
- Independently settle unfinished subagents using the existing `failed` state with an `Interrupted by previous process exit` diagnostic. Do not add a new Python-facing task state. If only background work was interrupted, leave the foreground history and active leaf unchanged.
- Missing, corrupt or incompatible required checkpoint/replay data must never trigger an empty-arena fallback. Where records remain readable, project the saved active branch into the existing disabled/read-only TUI, retaining history scrolling and expansion but disabling execution and state-changing navigation. Do not add another inspection command or read-only tree browser.
- A persistence failure cancels ongoing work and disables submissions and state-changing navigation. Previously completed external effects remain unchanged.

#### Subagent views and accounting

- Rebuild subagent views from tasks whose originating step belongs to the selected history.
- A visible task retains its latest recorded outcome, even if it finished after the selected foreground entry. Children do not travel back to an earlier running state.
- Results enter model context only when explicitly retrieved and exposed through Python, as today.
- Token/cost totals remain session-wide across all branches, counting each recorded attempt once. Context-usage information follows the selected history rather than an abandoned future.

#### Local data

- Session files contain prompts, outputs, provider replay and arena contents, potentially including sensitive project data. Restrict session directories/files to the owning user.
- Do not persist configured API credentials as session configuration. Credentials included in ordinary user content or arena values are still content.
- This provides local persistence and crash recovery, not encryption or protection against disk loss.

## Key decisions & tradeoffs

- SQLite over JSONL plus sidecars: atomic history/checkpoint commits without a custom multi-file recovery protocol.
- One database per session over a shared database: independent ownership and recovery, at the cost of scanning files for history listings.
- Branch at settled model steps, not just run boundaries: finer navigation, with greater checkpoint storage.
- Relational structure, typed content payloads: enforce relationships without normalising every provider or execution detail into separate tables.
- Separate immutable entries from run/step bookkeeping: record unfinished work and give subagents stable provenance before their originating step settles.
- Gate task access by the settled originating entry: a pre-execution ancestor would incorrectly expose sibling-branch tasks.
- Require idle agents before navigation: no implicit cancellation or detached old-branch work.
- Keep model/thinking preferences session-wide: rewinding preserves the model chosen for a retry.
- Explicit `--here` over Git-based inference: permit deliberate relocation without pretending retained paths were migrated.
- Stop on saving failures: prefer a visible interruption over silently losing persistence guarantees.
- Defer compaction and summaries: separate projections leave room for them without adding speculative interfaces.

## Risks / unknowns

- Checkpoint growth increases disk usage beyond message-only history. The arena has a 64 MiB memory limit, but serialised checkpoint size is not explicitly bounded (`src/config/constants.rs:10`, `src/arena.rs:66`). This remains a documented risk only; no benchmarking or size-mitigation work is required for this change.
- Effects can precede durability. A command or provider request may complete before its step commits. Recovery must preserve that uncertainty, including potentially incomplete cost accounting, rather than imply nothing happened.
- Compatibility can change. Monty dumps, Python helper bindings and provider replay may become incompatible with future versions. Reject unsupported state explicitly; never reconstruct it by rerunning code.
- Lifecycle races need deliberate tests: children finishing before their parent step commits, generation/execution cancellation, late descendant admission and crashes with only background work active.
- Relocation preserves stale assumptions. `--here` changes tool roots, not retained Python values or absolute paths. Existing content-fingerprint checks may require files to be read again before replacement.

## Dependencies

- Existing domain direction: [CONTEXT.md](../../CONTEXT.md), [Vision](../VISION.md) and [Pi architectural lessons](../research/2026-08-25_pi-architectural-lessons.md).
- Existing runtime and subagents: the current Agent/Arena implementation, exact Monty `0.0.22`, and the [subagent spec](../2026-09-05_subagents/spec.md). This change extends task retention across restart and narrows lookup by selected history; other delegation semantics remain unchanged.
- Existing settings and TUI: the [interactive model/thinking settings spec](../2026-09-05_interactive-model-thinking-settings/spec.md) and current prompt, picker, transcript and terminal-lifecycle implementations.
- SQLite through [rusqlite](https://github.com/rusqlite/rusqlite): add its bundled SQLite feature, giving Kiki a controlled SQLite version rather than depending on the host's system library. Keep database details inside the session-storage module.

## Phase outline

Each phase delivers a capability across the necessary storage, runtime and front-end changes, not a horizontal implementation layer.

1. **Persist and resume sessions**: Save requests, completed steps, checkpoints, metadata and subagent history. Deliver CLI history, resume-by-ID, `--here`, exit IDs and crash recovery. Use the complete tree-shaped schema from the start, even before interactive branching is available. Demonstrate exiting, restarting and continuing with the same state.
2. **Navigate and branch within a session**: Deliver `/tree`, context/arena restoration, branch-scoped subagent access and views, branch-aware context indicators, and the two-second correction shortcut. Reuse phase 1's storage and restoration machinery. Demonstrate rewinding, taking another path and returning to the original.
