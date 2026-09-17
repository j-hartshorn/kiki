# Plan: Persisted session trees, Phase 2: Navigate and branch within a session

## Source

- [Persisted session trees spec](spec.md), Phase 2: Navigate and branch within a session.
- Follow the spec's navigation, correction, branch-scoped subagent and accounting requirements. This plan adds implementation sequencing and the approved tree-picker presentation; it does not change the durable schema.
- Phase 1 is merged at `8db9c0a`. Implementation uses `work/session-trees-phase-2` and its matching worktree.

## Acceptance criteria

1. While fully idle, `/tree` and `Ctrl-R` open the same searchable session-tree picker. `/tree <text>` opens it with that initial filter; `Ctrl-R` preserves any existing editor draft. Foreground or background work causes an immediate busy notice without queueing navigation or cancelling work.
2. The picker renders a stable depth-first tree with siblings ordered chronologically. It uses tree connectors, marks the current point with `●`, accents its ancestor path and mutes alternative branches. Filtering is case-insensitive and retains the ancestor chain of each match; only matching entries are focusable.
3. Rows are whitespace-normalised, single-line and display-cell-safe:
   - `Session start`
   - `User: <request preview>`
   - `Python`
   - `Python: <commentary preview>`
   - `Python: [tools: N] <optional commentary preview>`
   - `Python: [tools: S✓ F✗] <optional commentary preview>`
   - `Python: [tools: F✗] <optional commentary preview>`
   - `Kiki: <reply preview>`
   - `Cancelled`
   - `Failed`

   Tool counts cover every recorded host-tool call. `✗` means an actual host-tool failure; valid non-zero or signalled `run()` results count as successful calls. When every call succeeded, show only the total, as `[tools: N]`; when all calls failed, omit the zero-success component. Omit the tools section when no tools ran. Separate any commentary from the preceding `Python` or tools prefix with one space after the colon/prefix as shown above.
4. Enter restores the selected model context, Arena checkpoint, display history, task projection and context usage; Esc closes without changing state. Session start restores empty context. Selecting a user request restores its parent and offers that request to an empty editor. Other entry kinds restore through themselves and leave editor text unchanged.
5. A successful selection durably changes the active leaf without creating, modifying or deleting session entries. The new display appears only after restoration and persistence succeed. Immediate exit and resume return to the selected point; the next accepted request creates a child there while retaining every previous future.
6. Navigation preserves editor text already present or entered while restoration is pending. A selected request fills the editor only if it is still empty when restoration completes. A non-durable `Session restored.` card appears immediately below the applicable history and disappears only when a new request is durably accepted.
7. Retained terminal subagents remain available in memory across navigation, but root access and displayed task views are restricted to tasks whose originating settled step belongs to the selected history. The active root run retains access to its own tasks while its originating step is pending. Existing descendant permissions remain required. Returning to a branch restores its task handles and latest results without restarting children. Token and cost totals remain session-wide; context usage follows the selected history.
8. Failed target validation or Arena restoration leaves the previous durable and visible selection intact. A persistence failure restores the previous Arena where possible and disables further work under the existing storage-failure policy. Navigation never falls back to an empty Arena or rewrites session-wide name, directory, model or thinking preferences.
9. Esc pressed at most two seconds after submission requests ordinary foreground cancellation plus correction intent, measured from the submission keypress. After cancelled settlement, automatic restoration occurs only if background work is already settled. Otherwise Kiki keeps ordinary cancellation, explains the skipped restoration, does not queue a rewind and does not implicitly cancel background work.
10. Automatic correction retains the cancelled path and its accounting, restores the request's parent, follows the same draft/card rules as manual navigation and makes a corrected submission a sibling branch. Late Esc, repeated Esc and already-settled runs retain their existing cancellation behaviour.
11. A deterministic cross-layer scenario can rewind, continue on another branch, return to the original branch, recover its exact model/Arena/task state and resume the selected branch after process restart. Filesystem changes and other external effects remain unchanged throughout.

## Overall validation

- Use real temporary file-backed SQLite sessions and the production schema for tree projection, active-leaf commits, branching, reopen/resume, rollback and accounting tests. Do not add a migration, alternate store or mock persistence boundary.
- Use real Monty `0.0.22` with scripted models to prove model context and Python state move together. No live-provider tests.
- Use explicit storage, runner and operation gates for navigation races and background settlement. Do not use sleeps or production test switches.
- Exercise tree rendering with Ratatui's test backend, including Unicode connectors, styles, filtering, ancestor retention, one-line normalisation, grapheme-safe truncation and narrow terminal sizes.
- Exercise correction timing through injected `Instant` values at 1.999 seconds, exactly two seconds and after two seconds. Gate cancellation settlement independently so keypress time cannot be confused with completion time.
- Build branches through the production persistence APIs. Verify both same-process navigation and fresh-process resume, including returning to previously hidden terminal subagent results.
- Preserve all Phase 1 recovery, CLI, metadata, accounting, cancellation, picker, transcript and terminal-lifecycle tests.
- Every behavioural task begins with its named first-red test. Documentation-only Task 10 does not manufacture a failing test.

Before focussed tests:

```sh
just monty
export MONTY_BIN=/tmp/kiki-monty/bin/monty
```

Every task runs these shared checks plus its listed test commands:

```sh
cargo fmt --all -- --check
cargo check --all-targets
```

Whole-phase gates:

```sh
cargo fmt --all -- --check
git diff --check main...HEAD
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```

`just test` ensures the exact Monty runtime and runs `cargo test --all-targets` (`justfile:6`, `justfile:21`). Existing unit and runtime tests remain in the binary target, so focussed commands use `cargo test --bin kiki`.

## Planning context

- The Phase 1 schema already has a mutable durable active leaf plus immutable parent-linked entries and checkpoints; no Phase 2 schema change is required (`src/session/schema.sql:10`, `src/session/schema.sql:22`, `src/session/schema.sql:93`, `src/session/schema.sql:105`).
- Request and settled-step transactions already branch from `session.active_entry_id`, so a successful pointer change is sufficient to make the next request a retained alternative branch (`src/session.rs:955`, `src/session.rs:1004`, `src/session.rs:1062`).
- Restoration currently reads only the saved active leaf. Its projection already rebuilds selected ancestry, model history, checkpoint, branch-visible tasks, global accounting and readable fallback; parameterise this path rather than adding another restoration model (`src/session.rs:1652`, `src/session.rs:1748`, `src/session.rs:1927`).
- Startup restoration constructs the Agent and initial display projection once. Live navigation needs an explicit replacement seam while retaining the existing model-history and Arena validation (`src/runtime.rs:622`, `src/agent.rs:461`, `src/arena.rs:1100`).
- Navigation cannot be a true transaction across SQLite and Monty. The approved order is to validate the target, retain the old Arena checkpoint, restore the target Arena while leaving model context unchanged, commit the active leaf, install target model context, then publish the display projection. Persistence failure restores the old Arena where possible and disables the session.
- The runtime command/event vocabulary has no tree operations, and the controller's foreground state does not establish background settlement (`src/runtime.rs:37`, `src/runtime.rs:142`, `src/runtime.rs:281`, `src/subagents.rs:2875`).
- Fresh-process restoration currently loads only selected tasks into the coordinator. Same-process branch switching instead retains all valid terminal tasks and gates access by selected durable origin steps, without weakening existing caller-origin and descendant checks (`src/runtime.rs:639`, `src/subagents.rs:2291`, `src/subagents.rs:3374`).
- A task started during a pending step remains accessible throughout its active root run. Once that run settles, future root callers use the task's durable originating step and selected ancestry rather than process-local origin identity (`docs/2026-09-15_session-trees/spec.md:131`, `src/subagents.rs:210`, `src/subagents.rs:2455`).
- Child operations are tracked from admission through terminal persistence. A zero operation count is the authoritative immediate settlement boundary; after it, drain already-published coordinator events before changing branch visibility (`src/subagents.rs:1928`, `src/subagents.rs:2433`, `src/runtime.rs:1061`).
- The generic picker already owns filtering, focus, scrolling and modal geometry, but assumes static uniformly selectable rows. Extend that picker narrowly for dynamic styled tree rows and non-focusable matching ancestors rather than creating an unrelated navigation widget (`src/tui/picker.rs:9`, `src/tui/picker.rs:25`).
- The TUI already receives injected monotonic key times. Add correction qualification at submission and Esc handling without measuring runtime completion time (`src/tui.rs:349`, `src/tui.rs:374`, `src/tui.rs:427`).
- The editor owns drafts independently of runtime history. `Ctrl-R` provides the draft-preserving tree entry point; request prefill must be checked when the restoration acknowledgement arrives (`src/prompt_editor.rs:31`, `src/prompt_editor.rs:55`).
- Transcript and subagent-monitor restoration are constructor-only and currently replay saved lifecycle items into fresh projections. Navigation can reuse that reconstruction, preserve the two display toggles, then swap whole projections rather than mutate the old instances (`src/transcript.rs:2222`, `src/tui/subagents.rs:94`).
- OpenAI permits commentary immediately before one Python invocation and stores both as one completed-code action; final assistant text remains a separate terminal action (`src/openai.rs:853`, `src/openai.rs:899`, `src/agent.rs:638`).
- Existing branch fixtures and fresh-process CLI coverage provide regression seams for saved-leaf selection, hidden sibling accounting, identifier continuation and resume (`src/runtime.rs:10220`, `tests/sessions_cli.rs:393`).

## Tasks

Each task is one atomic, signed-off commit. Keep `/tree` limited to selection and search. Do not add branch names, labels, summaries, folding, bookmarks, merging, pruning, export, cloning, keyboard configurability or session switching. Foundational private APIs are integrated by following tasks without temporary production test switches or dead-code allowances.

### Task 1: Project arbitrary session-tree points

**Changes**

Expose a typed, front-end-independent catalogue of every durable session entry and generalise the existing restore projection to a requested tree point. Keep the saved-active-leaf restore as the same projection with its existing caller. Decode each entry once into a shared typed kind used by catalogue and restoration; do not expose SQL rows or payload JSON.

Return the requested entry, its effective active entry, the active entry observed by the same read transaction and an optional original request for editor prefill. Session start projects empty model context and its initial checkpoint. A user-request target projects its parent; settled execution and terminal targets project through themselves. Return all validated terminal tasks separately from the selected visible task projection so branch changes can reveal retained results without restarting work. Task 1 only reshapes the restore result: startup continues loading the selected tasks until Task 3 installs full retention and its visibility check in the same commit.

Preserve complete global validation, readable/executable separation, selected-history context usage, session-wide accounting and whole-database identifier continuation. Keep synchronous startup restoration and add live projection/catalogue reads through `SessionWriter`'s existing `spawn_blocking` pattern, so later runtime commands never hold the SQLite mutex on the async controller. A catalogue fails as a unit if any required row cannot produce the approved typed tree kind; do not invent an `Unreadable` picker row outside the approved vocabulary. Do not format picker labels or change the schema.

**Tests** (`src/session.rs`, real SQLite projections and branch fixtures)

First red: `session::tests::restore_at_projects_selected_history_checkpoint_and_tasks`.

- Build two branches with different requests, executions, checkpoints, terminal outcomes and tasks. Project root, request, execution, final, cancelled and failed points and assert exact readable records, executable model history, checkpoint and request-prefill semantics.
- Prove the saved active leaf still wins for ordinary resume and that projecting a requested sibling does not alter it.
- Keep all validated terminal tasks available while selected tasks and hidden-task accounting change with ancestry. Retain a visible task's latest terminal result even when it settled after the selected foreground entry; reject broken task ancestry without an executable fallback.
- Derive one typed tree node per entry, including commentary, recorded host-tool summaries and terminal kind. Preserve entry/parent identity and chronological revision data without leaking provider replay or checkpoint bytes.
- Preserve Phase 1 corruption handling for malformed ancestry, run/step membership, payloads, replay, checkpoints and global revisions. A corrupt non-selected user-request payload, which Phase 1 need not decode for accounting, fails catalogue loading without changing the saved selection or disabling an otherwise executable current projection.

**Files**

- Modify: `src/session.rs`, `src/session/codec.rs`, `src/runtime.rs`.

**Verification**

```sh
cargo test --bin kiki session::tests::restore_at -- --test-threads=1
cargo test --bin kiki session::tests::restore_ -- --test-threads=1
```

The arbitrary-target and existing saved-target projections pass against the same validation path, followed by the shared checks.

### Task 2: Persist idle active-leaf selection

**Changes**

Add one serialised writer operation which receives the projection's already-normalised effective entry and selects it by updating only the active leaf, activity timestamp and monotonic session revision. Require the active leaf observed by the projection read to remain current. Defensively enforce entry existence plus the relational absence of unfinished foreground runs, model steps and subagent tasks, but do not decode or normalise the target a second time. Selecting the current effective point is a no-op before any Arena or database work.

Navigation creates no entry, checkpoint, run, step or task and leaves session name, directory and preferences unchanged. Use the existing storage-failure and write-gate machinery; do not add a branch table, status field or transaction spanning live Arena work.

**Tests** (`src/session.rs`, file-backed transaction and branch tests)

First red: `session::tests::select_active_entry_persists_and_next_request_branches_from_it`.

- Select an older execution, close and reopen, then commit another request. The new request is a child of the selected point while the previous future remains intact.
- Select a user request and prove its projected parent becomes the active leaf, avoiding a duplicated request in later context.
- Assert exactly one revision advance and no conversation-row count changes for a real selection; a current-point no-op changes neither revision nor timestamp.
- Reject stale observed leaves, unknown targets, open runs, pending model steps and unfinished tasks without changing any persisted field.
- Gate and fail the selection transaction to prove rollback and immediate-exit durability.

**Files**

- Modify: `src/session.rs`.

**Verification**

```sh
cargo test --bin kiki session::tests::select_active_entry -- --test-threads=1
cargo test --bin kiki session::tests::foreground_writes_settle_entries_and_runs_atomically -- --exact --test-threads=1
```

Selection and subsequent branching pass with unchanged lifecycle constraints, followed by the shared checks.

### Task 3: Scope retained subagents to selected history

**Changes**

Retain every validated terminal task in the session coordinator and maintain the set of settled originating model steps visible from the active history. Root task access permits selected durable origins and tasks owned by that same active root run across its model steps. Seed process-local root-origin allocation beyond every restored origin so numeric collisions cannot grant the active-run exception. Preserve session identity, caller origin and proper-descendant checks for child callers; do not replace them with a global task allow-list.

Use the existing tracked-operation count as the authoritative immediate settled-state check rather than rescanning task state. Its zero invariant must imply no pending admission, runner, descendant cleanup or terminal persistence. Before a branch change, require zero operations and drain the coordinator event queue; no operation can publish a later stale event after that boundary. Then replace selected durable origins from the target projection. On ordinary run settlement, add every model step settled onto the active path before another root run can begin.

Hidden tasks remain retained, while lookup, waiting, cancellation, events and display projections cannot expose them. Hidden attempt accounting remains owned by the session projection. Continue identifiers above the whole stored task set.

**Tests** (`src/subagents.rs`, coordinator access and gated-operation tests; `src/runtime.rs`, startup restoration)

First red: `subagents::tests::root_access_is_limited_to_selected_origin_steps`.

- A root retrieves a task from a selected settled step and receives the existing denial for a sibling-branch task. Returning to that sibling changes visibility without reconstructing or restarting the task.
- A task admitted in step N remains accessible to its root caller in step N+1 even before the controller processes step-N display events. Future root callers use settled selected provenance.
- Restored and live process-origin numbers cannot collide into access. Child callers still access only proper descendants from their own origin; hidden parents cannot expose descendants.
- The settled-state check remains false through pending admission, active execution, stopping, terminal persistence and descendant cleanup, then becomes true without waiting, scanning or cancelling.
- Events published before operation settlement are drained before branch visibility changes; no abandoned-branch event appears afterwards.
- Selected tasks can change while the complete retained task set and next task identifier remain stable.

**Files**

- Modify: `src/subagents.rs`, `src/runtime.rs`.

**Verification**

```sh
cargo test --bin kiki subagents::tests::root_access_is_limited_to_selected_origin_steps -- --exact --test-threads=1
cargo test --bin kiki subagents::tests -- --test-threads=1
cargo test --bin kiki runtime::tests::resume_restores_context_arena_and_task_handles -- --exact --test-threads=1
```

Selected-history and descendant access pass without changing child lifecycle behaviour, followed by the shared checks.

### Task 4: Restore an idle runtime at a selected point

**Changes**

Add one inline controller navigation path with an explicit completion outcome for every command: replaced projection, unchanged current point, rejected busy/read-only request, or failed restoration which kept the prior selection. The command guard requires foreground work to be absent and the coordinator operation count to be zero, then drains settled coordinator events before reading the target. Keep the core restore operation separate from that command guard: it requires the Agent to be back in the controller and child operations to be settled, allowing Task 9 to reuse it while the just-finished foreground run still owns its `ActiveRun` bookkeeping. Controller commands remain serial and wait behind the inline operation rather than introducing a second concurrent navigation task.

If the target's effective entry equals the current active leaf, acknowledge unchanged without touching Arena or SQLite while still carrying an optional request prefill. For a real move, validate model history, retain only the old Arena checkpoint, restore the target Arena while leaving old model context installed, commit the effective active leaf against the active entry observed by the same projection read, then install target model context, runtime records, selected task origins and accounting. Publish the replacement projection only after all steps succeed. Treat an unexpected active-leaf conflict as a storage/invariant failure.

Projection or model-history validation failure and an incompatible checkpoint cannot have displaced live Arena state, so return with the prior selection without compensation. If a compatible target Arena load may have displaced its checkout, recover the prior Arena from the retained checkpoint before returning. If prior Arena recovery fails, disable execution. If active-leaf persistence fails after target restoration, recover the prior Arena where possible, latch the existing session-wide storage failure and disable work even when compensation succeeds. Preserve metadata and accepted model/thinking state. This task owns runtime state replacement and produces a fresh projection; TUI reconstruction remains Task 8.

**Tests** (`src/runtime.rs`, scripted-model integration with real SQLite/Monty and explicit write/runner gates; `src/agent.rs`, state replacement)

First red: `runtime::tests::navigation_restores_arena_and_context_before_publishing_history`.

- Navigate between checkpoints containing distinct Python values and model histories; the next model step observes one matching target state, never a mixed state.
- Hold active-leaf persistence and prove no replacement outcome or later submission is accepted early. Successful navigation survives immediate shutdown and fresh-process resume.
- Reject navigation immediately during request admission, foreground generation/execution/closure and child admission/execution/terminal persistence. Do not queue, cancel or move the pointer. A later controller command waits behind inline navigation and observes its completed state.
- Short-circuit a current-point request before Arena restoration while preserving an optional request prefill. Force invalid target history, an incompatible checkpoint, compatible target Arena load failure, pointer conflict and write failure; prove compensation occurs only after a load could displace the checkout and exercise disabled recovery paths.
- Restore a root, request parent, execution and terminal point. Preserve external file effects while Python values rewind.
- Replace selected task projections and context usage while session-wide accounting and task identifiers remain global. No child runner starts.
- In one real branch scenario, rewind, continue on a sibling, return to the original leaf and verify exact model/Arena/task state before resuming the final selected leaf in a fresh process.

**Files**

- Modify: `src/agent.rs`, `src/runtime.rs`.

**Verification**

```sh
cargo test --bin kiki runtime::tests::navigation_ -- --test-threads=1
cargo test --bin kiki agent::tests::restored_state_replaces_arena_and_context_atomically -- --exact --test-threads=1
cargo test --bin kiki runtime::tests::resume_ -- --test-threads=1
```

Runtime navigation and Phase 1 resume share one validated projection and pass all compensation cases, followed by the shared checks.

### Task 5: Support dynamic contextual picker rows

**Changes**

Narrowly extend the existing picker so callers can supply owned styled rows, direct-match search text and precomputed descendant/context search text. This lets filtering include non-focusable context rows without teaching the picker about parent links. Expose the picker's canonically matched item values and an item-replacement operation which preserves focus by value; the tree caller will use that exact matched subset after each filter change to recompute visible connectors, avoiding a second case/whitespace matching implementation. Add a per-picker save policy so Ctrl-S can remain settings-only rather than producing a tree selection. Preserve existing focus, case-insensitive search, scrolling, paging, empty-result and modal geometry behaviour for model/thinking pickers. Do not add a second general picker implementation.

**Tests** (`src/tui/picker.rs`, picker unit/render tests)

First red: `tui::picker::tests::dynamic_matches_keep_non_focusable_context_rows`.

- Render dynamic Unicode labels and distinct normal/accent/muted styles without leaking style to adjacent rows.
- A direct match remains focusable while included context rows cannot be selected. Up/down/page/boundary navigation skips context rows and keeps the focussed direct match visible.
- Clearing and changing a filter restores valid focus deterministically. No-match behaviour and tiny viewports remain safe.
- Existing static grouped model/thinking rows, Enter, Ctrl-S and Esc behaviour remain unchanged; a picker with saving disabled ignores Ctrl-S.

**Files**

- Modify: `src/tui/picker.rs`, `src/tui.rs`.

**Verification**

```sh
cargo test --bin kiki tui::picker::tests -- --test-threads=1
cargo test --bin kiki tui::tests::picker -- --test-threads=1
```

Dynamic context rows and existing settings pickers pass, followed by the shared checks.

### Task 6: Return session-tree catalogues through runtime

**Changes**

Add a correlated runtime command/event exchange for requesting the current typed tree catalogue. Return exactly one outcome: catalogue, busy, read-only, or load failure. Check foreground and tracked background settlement at handling time; do not wait, queue work or cancel anything. Perform the live projection read through the session handle's blocking-worker path rather than locking its synchronous SQLite connection on the async controller. Catalogue loading is read-only and does not change the active leaf, revision, Arena, model context, metadata or task visibility.

Pin the UI-facing outcomes for Task 8: slash purpose `Navigate session history`; picker title `Session tree`; loading row `Loading…`; no-match text `No matching session entries`; busy notice `Session tree is unavailable while work is active.`; read-only notice `Session tree is unavailable while the session is read-only.`; load/selection failure notice `Session could not be restored.` Correlation lets the TUI discard a catalogue response after its loading picker was closed or superseded.

**Tests** (`src/runtime.rs`, command/event integration with real session catalogues and gated child operations)

First red: `runtime::tests::tree_request_returns_catalogue_only_when_fully_settled`.

- Return the complete typed catalogue and current/effective identity while idle without changing any persisted or live state.
- Reject foreground work and each tracked background phase immediately with the specified reason. Settling later does not emit a delayed catalogue.
- Return read-only and decode/load failures distinctly. A corrupt non-selected entry leaves the current executable projection unchanged.
- Correlate consecutive requests and preserve command/event ordering with settings and shutdown.

**Files**

- Modify: `src/runtime.rs`.

**Verification**

```sh
cargo test --bin kiki runtime::tests::tree_request_ -- --test-threads=1
cargo test --bin kiki runtime::tests::command_ -- --test-threads=1
```

Catalogue outcomes are immediate, correlated and side-effect free, followed by the shared checks.

### Task 7: Render the searchable session tree

**Changes**

Build the TUI-owned tree projection from the typed catalogue. Traverse depth-first and order siblings by durable revision with stable entry-ID tie-breaking. After every picker filter change, read its canonically matched entry values, recompute `├─`/`└─` connectors for that exact visible subset, then replace picker items while preserving focus by entry value. Mark the current point with `●`, use the existing accent style for its ancestors and mute alternatives. The picker's case-insensitive filtering matches complete whitespace-normalised row text through direct and descendant/context search text, thereby retaining each direct match's ancestor chain while leaving those contextual ancestors non-focusable.

Render the approved exact one-line row vocabulary. Normalise terminal text before display and truncate previews at display-cell/grapheme boundaries. Count recorded host calls once. Treat only `ToolCallSummary::Failed` as failed; all other settled summaries, including non-zero/signal command results, are successful. Construct the tree picker with saving disabled, so Ctrl-S neither selects a point nor writes user defaults.

**Tests** (`src/tui/tree.rs`, pure tree derivation and Ratatui rendering; `src/tui/picker.rs`, contextual item integration; `src/tui.rs`, module wiring)

First red: `tui::tree::tests::renders_chronological_depth_first_tree_with_active_path`.

- Assert exact rows for start, request, Python with no details, commentary only, all-success tools, mixed/all-failed tools, final reply, cancellation and failure.
- Normalise multiline/control text and truncate Unicode previews without splitting graphemes or creating another row.
- Build repeated branch returns where global creation order differs from depth-first order; keep each subtree together and siblings chronological.
- Filter a descendant while retaining correctly recomputed connectors and non-focusable ancestors. Filter by text beyond the currently clipped preview.
- Assert current/path/alternative buffer styles, initial current focus, empty results, ignored Ctrl-S and narrow/short terminal rendering. Generic paging mechanics remain covered by Task 5.

**Files**

- Create: `src/tui/tree.rs`.
- Modify: `src/tui.rs`, `src/tui/picker.rs`.

**Verification**

```sh
cargo test --bin kiki tui::tree::tests -- --test-threads=1
cargo test --bin kiki tui::picker::tests -- --test-threads=1
```

Tree order, rows, filtering, styles and width safety pass, followed by the shared checks.

### Task 8: Wire interactive tree navigation

**Changes**

Add `/tree` and `/tree <initial filter>` plus `Ctrl-R` to request the same runtime catalogue. `Ctrl-R` leaves an existing draft untouched. Open a correlated `Session tree` loading picker immediately, before the runtime round trip, showing `Loading…`; while loading, only Esc acts, and Enter is a no-op without a notice. Esc makes the eventual response stale. Populate the picker on success with `No matching session entries` as its no-match text, or close it with the exact Task 6 notice. Disabled/busy requests never cancel or queue work.

Close the populated picker on Enter and hold a manual-navigation pending state until the runtime's replaced, unchanged, rejected or failed outcome arrives. Prompt editing remains available, but Enter cannot submit during that interval and reports `Session restoration is still in progress.` Esc on the picker changes nothing. Selecting the current effective point closes cleanly without restoration or a card.

On replacement, reconstruct fresh `Transcript` and `SubagentMonitor` instances through their existing restored-projection path, preserve reasoning/expansion toggles, then swap them. Do not replay restoration into the old live instances or build a parallel projection algorithm. Preserve session metadata, accepted settings, terminal ownership and current editor text; clear stale history text selection, follow the new bottom and fill a selected request only when the editor is still empty at acknowledgement time. An unchanged outcome applies the same optional empty-editor prefill but creates no restoration card.

Append the non-durable `Session restored.` card after every applicable restored record and late visible terminal-task card. Remove it only when `RecordAppended(UserRequest)` confirms durable request acceptance. Busy, rejected or failed submissions leave it visible.

**Tests** (`src/tui/slash.rs`, command parsing; `src/tui.rs`, input/loading/picker/draft/event integration; `src/transcript.rs`, fresh replacement/card; `src/tui/subagents.rs`, fresh monitor reconstruction)

First red: `tui::tests::tree_picker_navigates_without_losing_the_editor_draft`.

- `/tree` and `/tree auth` are recognised, never become model requests and open the same picker as `Ctrl-R`; slash completion, unknown commands and multiline prompts remain unchanged.
- The loading picker appears synchronously with the exact title/row. Enter and other non-Esc keys do nothing while loading. Esc, stale/superseded catalogue replies, busy, read-only and failed replies cannot later open it. Loaded no-match and outcome notices use the exact Task 6 text.
- `Ctrl-R` retains a pre-existing draft. A draft entered after selection but before replacement also wins. An empty editor receives only a selected user request; other targets leave it empty.
- Enter closes the tree and sends one selection. While pending, editing works and submission is withheld with the exact notice. Every completion outcome clears pending state; unchanged applies request prefill when present, while unchanged/rejected/failed outcomes retain the old projection and create no card.
- A successful replacement uses fresh transcript/monitor reconstruction, preserves display toggles, invalidates old scroll/text selection and places `Session restored.` after late visible task history.
- Busy/rejected/failed request attempts retain the card. A committed user-request event removes it before continuing the new branch.
- Picker Esc and Ctrl-S write nothing. Existing model/thinking picker Enter/Ctrl-S behaviour remains unchanged.
- Existing actual-binary resume tests remain green as regression coverage for the active leaf selected by Task 4.

**Files**

- Modify: `src/tui/slash.rs`, `src/tui.rs`, `src/transcript.rs`, `src/tui/subagents.rs`.

**Verification**

```sh
cargo test --bin kiki tui::slash::tests -- --test-threads=1
cargo test --bin kiki tui::tests::tree_ -- --test-threads=1
cargo test --bin kiki transcript::tests::session_restored -- --test-threads=1
cargo test --test sessions_cli -- --test-threads=1
```

Interactive loading, selection, replacement, drafts and cards pass; the CLI command is regression-only and does not imply a change to `tests/sessions_cli.rs`. All shared checks follow.

### Task 9: Restore early-cancelled requests for correction

**Changes**

Record the submission keypress time in the TUI and carry explicit correction intent on the first foreground-cancellation command when Esc is pressed no later than two seconds afterwards. Ordinary cancellation remains the first action. Associate the intent with that active run and carry the committed request entry identity from request admission. Feed that request target through Task 1's canonical effective-target and prefill logic rather than carrying a separate parent/prefill path. Do not infer eligibility from cancellation completion time or issue a separate rewind command which could race settlement.

Keep correction pending in the TUI through `RunSettled`, so no submission can race a replacement or refill; withheld Enter reuses `Session restoration is still in progress.` After a qualifying run durably settles as cancelled and the Agent has returned to the controller, append its terminal record and perform Task 3's ordinary settled-step origin update, then call Task 4's core restore operation with the request target only when the coordinator operation count is already zero and its published events have been drained. The core replacement is the final selected-origin write for that settlement, so cancelled-branch tasks do not leak onto the restored parent. Emit the replacement or skip/failure result before `RunSettled`, and keep `RunSettled` as the final foreground acknowledgement. If the terminal outcome is final or failed, emit no replacement; `RunSettled` clears pending correction normally. Drop progress for that settled run after replacement until its `RunSettled` arrives, so queued progress cannot activate the fresh transcript.

Never cancel background work as part of correction and never queue restoration for later. If background work remains, keep the cancelled terminal active and show exactly `Session was not restored because background work is active.` A repeated Esc retains the existing explicit background-cancellation behaviour; if that work has actually settled by foreground settlement, normal eligibility applies.

Preserve the cancelled branch, code/result feedback, task records and accounting. Apply the same conditional request prefill and exact `Session restored.` card as manual navigation. A corrected request commits as a sibling of the cancelled request. Navigation/storage failures retain Task 4's compensation and disabled behaviour.

**Tests** (`src/tui.rs`, deterministic key-time and draft tests; `src/runtime.rs`, cancellation/navigation integration with gated provider, Arena, child and storage operations; `src/session.rs`, request-parent and sibling assertions; `src/agent.rs`, admission evidence)

First red: `tui::tests::early_escape_carries_correction_intent_but_late_escape_only_cancels`.

- Qualify Esc at 1.999 seconds and exactly two seconds; reject correction after two seconds. Use submission keypress time while delaying request admission and cancellation settlement independently.
- Preserve the existing first-Esc foreground and repeated-Esc background cancellation commands. Release key events and already-observed settled runs do nothing new.
- Carry the exact committed request entry even when cancellation arrives before request admission, then use its canonical projected parent. Restore after cancellation before the first provider call, during generation and during Python execution, retaining the exact cancelled terminal/evidence and prior checkpoint semantics in the abandoned branch.
- Skip immediately for active, stopping or persistence-pending background work, explain once and never restore when that work later settles. Do not implicitly cancel it.
- Cover explicit repeated-Esc background cancellation settling before and after the foreground terminal boundary.
- If final or failed settlement wins the race, keep that terminal active. If correction restoration fails, retain the cancelled point or disable according to the established failure class.
- Preserve a draft present at replacement; otherwise refill the original request. Keep correction pending and drop queued progress for the corrected run through the following `RunSettled`, which clears the pending state and must be harmless after fresh transcript reconstruction.
- A task originating under the cancelled run is denied to the root after parent restoration, remains retained, and reappears on manual return to the cancelled leaf.
- Remove the restoration card only after the corrected request commits, then prove old and corrected requests are sibling branches. Task 4 remains the owner of reopen durability coverage.

**Files**

- Modify: `src/session.rs`, `src/agent.rs`, `src/runtime.rs`, `src/tui.rs`.

**Verification**

```sh
cargo test --bin kiki tui::tests::early_escape_ -- --test-threads=1
cargo test --bin kiki runtime::tests::correction_ -- --test-threads=1
cargo test --bin kiki session::tests::corrected_request_ -- --test-threads=1
cargo test --bin kiki agent::tests::persisted_ -- --test-threads=1
```

The exact timing boundary, settlement ordering, skipped-background behaviour and retained sibling branches pass, followed by the shared checks.

### Task 10: Document navigation and correction

**Changes**

Document `/tree`, `/tree <filter>`, `Ctrl-R`, tree ordering/search/row vocabulary, idle-only navigation, request correction, draft preservation and the temporary restoration card. Explain the two-second Esc shortcut, its background-work skip, retained cancelled paths, branch-scoped child access/views and session-wide accounting. Repeat that Arena/model context rewind does not reverse filesystem, command, network or other external effects.

Replace the Phase 1 statement that interactive tree navigation, correction and rewind are unavailable, and update the existing Esc controls. Keep CLI/session-storage guidance intact and do not document deferred branch management features.

**Tests** (documentation review)

No first-red test applies. Check examples and exact UI strings against the implemented behaviour, including `Session restored.`, `User`, `Python`, `Kiki`, `Cancelled`, `Failed`, `/tree` and `Ctrl-R`.

**Files**

- Modify: `README.md`.

**Verification**

```sh
rg -n '/tree|Ctrl-R|Session restored\.|external effects|background work' README.md
cargo fmt --all -- --check
git diff --check main...HEAD
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```

The documentation matches the shipped controls and all whole-phase gates pass.
