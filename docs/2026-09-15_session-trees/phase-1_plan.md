# Plan: Persisted session trees, Phase 1: Persist and resume sessions

## Source

- [Persisted session trees spec](spec.md), Phase 1: Persist and resume sessions.
- Follow the spec's durable model, schema, commit rules, CLI/metadata and recovery requirements. This plan adds implementation sequencing, not another source of product requirements.
- Implementation references describe baseline `8cc5bff`. Follow [the worktree instructions](../../worktrees/README.md) for implementation; planning documentation may be written on `main`.

## Acceptance criteria

1. A fresh process can continue a saved session with matching model context, Python arena state, whole-file authorisations and retained child results. The saved active leaf wins over newer sibling entries.
2. Gated tests demonstrate that accepted requests commit before provider calls, settled steps commit before subsequent work, and task admission commits before child startup. Accepted requests appear exactly once in live and restored context, including cancellation before the first provider call.
3. Crashes during generation, execution and background-only work recover committed state without rerunning work. Unfinished operations settle once; background-only recovery does not invent a foreground outcome or move the active leaf.
4. New sessions receive distinct IDs. CLI listing works without credentials or Monty; malformed/unknown IDs, stored-ID mismatches, ownership conflicts and directory mismatches fail clearly without silently creating a replacement session. Opened sessions print their IDs after terminal restoration, including handled runtime failures.
5. Equivalent resolved directory paths are accepted. Successful `--here` deliberately changes tool roots and saves the directory without rewriting retained data. Names and model/thinking choices survive restart with the agreed precedence; successful effective startup choices, including environment overrides, become saved session preferences. Failed startup preserves saved choices. Ctrl-S continues to save user defaults separately.
6. Only one process writes a session. Unsupported storage formats remain unchanged. Required invalid or incompatible state never produces an empty-arena fallback; readable history remains inspectable in the existing disabled TUI. Saving failures stop foreground/background work and prevent further submissions and state-changing navigation.
7. Saved history retains UTC timestamps and measured durations without counting time between processes as execution. Recorded attempts contribute to accounting once. Session directories/files are owner-only, and configured API credentials are not serialised as session configuration.

## Overall validation

- Use real temporary, file-backed SQLite databases for round-trips, transaction rollback, constraints, ownership, compatibility and saved-leaf projections. Do not substitute a mock database for these tests.
- Use real Monty `0.0.22` and scripted models for arena continuation, authorisations, retained Python task handles, provider replay and accounting. No live-provider tests are required.
- Use controlled subprocess termination and explicit commit/runner gates for crashes and races, not sleep-based readiness. Cover early child completion, late descendants, background-only crashes, blocked commits, cancellation, deadlines and shutdown. Test helpers stay test-only; do not add production test switches.
- Use CLI process tests against the actual `kiki` binary and existing TUI terminal helpers for argument handling, listing, restored/disabled history, metadata and exit ordering. Share the production schema with CLI fixtures without introducing a library target.
- Each implementation task starts with its named first-red behavioural test, then adds its remaining cases at the stated layers. These are planned tests, not claims about existing test names. Task 9 is documentation-only and does not manufacture a failing test.
- Build branch fixtures through storage test setup to exercise saved-leaf restoration and global accounting. Do not introduce navigation controls or a branch-switching interface just to create fixtures. Interactive branching, access-policy changes on navigation and the two-second correction belong to Phase 2.
- Checkpoint growth remains the spec's documented risk. No checkpoint-size benchmarks, limits, compression, deduplication or other size-mitigation work.

Run commands from the implementation worktree. Before focussed tests:

```sh
just monty
export MONTY_BIN=/tmp/kiki-monty/bin/monty
```

Every task runs these checks plus its listed test commands, all of which must pass:

```sh
cargo fmt --all -- --check
cargo check --all-targets
```

Whole-phase gates:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```

`just test` ensures the pinned Monty executable and runs `cargo test --all-targets` (`justfile:7`, `justfile:22`). Existing Rust tests live in the binary target, so unit-test commands use `cargo test --bin kiki`, not `--lib`. The new CLI integration target uses `cargo test --test sessions_cli`.

## Planning context

### Composition and persistence

- There is no library target or CLI dispatch today. `main` constructs tracing, configuration, the model, tool host, arena, coordinator and TUI (`Cargo.toml:1`, `src/main.rs:52`). Dispatch listing and argument failures before runtime configuration, credentials, prompt loading, Monty or tracing startup. Keep the small dispatcher in `src/main.rs`; do not split a library merely to test it.
- Agent progress callbacks are synchronous, and execution reporting currently does not await controller processing (`src/agent.rs:502`, `src/agent.rs:643`, `src/runtime.rs:776`). Foreground and child execution must directly await the independent session-storage module. Runtime events notify observers; they are not the persistence transport.
- Shutdown awaits child cleanup outside normal event consumption (`src/runtime.rs:464`). Writes and acknowledgements must not depend on that loop continuing to pump events. Keep blocking SQLite work off async executors, preserve cancellation/deadline servicing while commits are pending, and do not hold coordinator locks or database transactions across waits for child work.
- A pre-cancelled standalone agent intentionally leaves its context unchanged (`src/agent.rs:506`, `src/agent.rs:2100`). A durably accepted runtime request is different: preserve it exactly once in live and restored context even if cancelled before the first provider call. Preserve staged-final admission versus cancellation decisions (`src/agent.rs:480`, `src/agent.rs:496`).
- Keep foreground admission, settled steps and closure in one integration task. Persisting admission without closure would violate the one-open-run constraint on subsequent requests. Foundation tasks need not each expose a complete CLI capability, but must not require throwaway lifecycle behaviour or production no-op persistence.

### Codecs, tasks and projections

- Checkpoint fields and whole-file authorisation internals are private (`src/arena.rs:66`, `src/tools.rs:832`). Add narrow conversion access where needed; keep wire representations and SQL private to the session module. Preserve paths losslessly rather than converting them through lossy display strings.
- `ToolValue` includes arbitrary `f64` values, and arena result conversion accepts non-finite floats (`src/tools.rs:41`, `src/arena.rs:1299`, `src/arena.rs:1961`). Plain JSON float conversion is insufficient. Round-trip the supported value set, including non-finite values and signed zero, without loss. Duration conversion must also preserve its original precision (`src/subagents.rs:1246`).
- Completed-code history validates call/result identity, and OpenAI request construction validates replay (`src/model.rs:230`, `src/openai.rs:161`, `src/openai.rs:786`, `src/openai.rs:819`). Reuse those rules before enabling a restored session; retain opaque replay bytes, feedback and commentary continuation rather than rebuilding them from display text.
- The coordinator allocates `OriginId` per root run and charges start budgets against it (`src/subagents.rs:1942`, `src/subagents.rs:2014`). Do not replace this with per-model-step budget accounting. Carry the originating foreground model step separately, including for descendants admitted after root settlement.
- Child admission currently reserves state under the coordinator lock, then publishes and starts its runner (`src/subagents.rs:1956`). Durable admission and terminal publication require awaited writes without holding that lock across storage waits. `TaskReporter` callbacks remain observations, not sufficient durability acknowledgements (`src/subagents.rs:465`).
- Python `SubagentTask` handles retain an ID and cached status, while the host supplies current observations/results (`src/tools.rs:697`, `src/subagents.rs:904`). Reconnect retained IDs to fresh invocation/coordinator bindings. Do not persist process-local caller nonces, child arenas or checkpoint bindings for resuming children.
- Final foreground attempts are cumulative; execution-step attempts are local (`src/agent.rs:702`, `src/runtime.rs:883`). Child terminal reporting also replaces a cumulative attempt list, while evidence identifies model step/provider attempt (`src/subagents.rs:533`). Do not count both representations or deduplicate by equal usage values. Task cost already derives from recorded attempts (`src/subagents.rs:1783`); retain those inputs losslessly and recompute estimates through existing pricing code, not a second stored cost authority. Task elapsed time currently comes from `Instant`; persist measured durations and UTC observations, not process-local instants (`src/subagents.rs:1773`).
- The footer currently derives totals from displayed foreground records and visible task records (`src/transcript.rs:2704`). Restored accounting needs the entire session, while context usage follows the selected history. The task monitor clears terminal tasks on new requests and rejects later records for cleared IDs (`src/tui/subagents.rs:95`, `src/tui/subagents.rs:114`, `src/tui/subagents.rs:166`). Do not seed restored views by pretending old lifecycle notifications are new activity; preserve chronological cards and existing monitor semantics.
- `/name` is currently frontend-only (`src/tui.rs:744`). Metadata changes need durable acknowledgement, preserving current settings ordering and accepted-selection semantics. Startup binds the real workspace root (`src/config.rs:419`, `src/tools.rs:1658`); resolve saved preferences before construction and publish successful startup choices only after restoration succeeds. Existing configuration/disabled views and the terminal restoration guard provide the inspection and exit seams (`src/tui.rs:1340`, `src/tui.rs:1357`).

### Storage choices

- Pin `rusqlite` to `=0.40.2`, with default features disabled and `bundled` enabled. Its bundled dependency is `libsqlite3-sys 0.38.2`, providing SQLite `3.53.2`. Use `DELETE` rollback journalling, full synchronisation and foreign keys. See the [crate features](https://docs.rs/crate/rusqlite/0.40.2/features), [bundled SQLite source](https://docs.rs/crate/libsqlite3-sys/0.38.2/source/sqlite3/sqlite3.h) and [rollback-journal behaviour](https://www.sqlite.org/lockingv3.html#the_rollback_journal).
- Use 128 random bits rendered as 32 lowercase hexadecimal characters, with exact full-ID lookup. Reuse the existing randomness and locking dependencies (`Cargo.toml:12`, `Cargo.toml:14`). Keep a persistent `<id>.lock` file, never unlink it, and retain ownership until SQLite and pending writes have closed.
- Perform non-modifying compatibility preflight before opening an existing database through SQLite. The documented header carries `user_version` at offset 60; reject unsupported versions and unexpected WAL header modes. Finish raw-header access before opening a same-process SQLite connection to that file: closing a separate descriptor can release POSIX SQLite locks. See the [file header](https://www.sqlite.org/fileformat.html#user_version_number), [WAL read-only constraints](https://www.sqlite.org/wal.html#read_only_databases) and [descriptor-close hazard](https://www.sqlite.org/howtocorrupt.html#_posix_advisory_locks_canceled_by_a_separate_thread_doing_close_).
- Let SQLite perform native recovery after compatibility preflight; do not parse journals or add a migration framework. Read-only metadata access can return `SQLITE_READONLY_ROLLBACK` for a hot journal and must leave it unchanged. Listing can briefly wait or warn about an individual busy, recovery-needed or unreadable file while continuing with healthy sessions. Do not use `immutable=1` to read live sessions. See [read-only rollback errors](https://www.sqlite.org/rescode.html#readonly_rollback).
- `time 0.3.55` is already present transitively (`Cargo.lock:3205`). Make it a direct dependency with default features disabled and `formatting` enabled for UTC CLI timestamps, rather than implementing calendar conversion. Use RFC 3339 formatting; see [timestamp conversion](https://docs.rs/time/0.3.55/time/struct.OffsetDateTime.html#method.from_unix_timestamp_nanos) and [formatting](https://docs.rs/time/0.3.55/time/struct.OffsetDateTime.html#method.format).

## Tasks

Each task is one atomic, signed-off commit. Do not add `/tree`, correction/rewind controls, in-client session switching, speculative storage backends or compaction interfaces.

### Task 1: Add owned SQLite session roots

**Changes**

Implement the approved complete schema, session IDs, ownership, compatibility checks and initial checkpoint storage. Wire new-session creation into startup. Keep SQL and codecs private in the session module, with the fixed schema shared by production and later CLI test fixtures.

**Tests** (`src/session.rs`, storage/process integration; `src/session/codec.rs`, codec unit tests; real Monty checkpoint restoration)

First red: `session::tests::reopen_restores_initial_checkpoint`.

- Create, close and reopen a file-backed session; restore the initial checkpoint with its exact identity/state/authorisations.
- Cover lossless paths, distinct IDs, collision protection, the complete schema and its structural constraints, and owner-only directories/database/lock/journal files.
- Use competing processes to prove exclusive ownership, retained lock-file identity and ownership release only after SQLite closes. Metadata readers do not take the writer ownership lock.
- Reject unsupported/invalid headers without changing database content; keep format checks separate from creating a genuinely new database.
- Verify initial metadata contains only the permitted session configuration, not configured API credentials.

**Files**

- Create: `src/session.rs`, `src/session/codec.rs`, `src/session/schema.sql`.
- Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/arena.rs`, `src/tools.rs`.

**Verification**

Run the shared checks and the following commands. All tests pass, including existing checkpoint and authorisation regressions.

```sh
for filter in session:: arena::tests tools::tests; do
  cargo test --bin kiki "$filter" || exit
done
```

### Task 2: Persist complete foreground runs

**Changes**

Add awaited request, model-step start, settlement and terminal commits independent of progress delivery. Persist foreground payloads and replay losslessly. Keep request acceptance, active-leaf advancement and once-only run/step closure consistent. Saving failures stop work and disable further submissions.

**Tests** (`src/runtime.rs`, runtime integration with scripted models, real Monty and gated SQLite commits; `src/session/codec.rs`, codec unit tests)

First red: `runtime::tests::request_commit_precedes_provider_call`.

- Hold the request commit and prove no provider call occurs; hold step allocation and prove that step does not begin. Rejected busy/disabled requests create no entries.
- A durably accepted request cancelled before its first provider call appears once in live and restored context. Preserve the existing standalone pre-cancelled-agent behaviour.
- Hold settlement and prove the next model/Python step cannot begin. Atomically persist the settled entry, checkpoint reference, step link and active leaf; prove rollback leaves the preceding state intact.
- Cover generation/execution cancellation, provider/arena failures, final-message admission versus cancellation, and cancellation between steps. A final reply creates one terminal entry; closed runs have no pending steps and settled links are not rewritten.
- Preserve full code/result pairs, opaque replay, call IDs, feedback, reasoning and local attempt evidence. Include non-finite floats and signed zero in codec round-trips; do not promote streamed prefixes into model context.
- Saving failures prevent further work. Cancellation, child-deadline servicing and shutdown remain responsive while a commit is pending. Preserve completed code before a later cancellation check.

**Files**

- Modify: `src/session.rs`, `src/session/codec.rs`, `src/agent.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/main.rs`.

**Verification**

Run the shared checks and the following commands. The new durability tests and existing cancellation/accounting tests all pass.

```sh
for filter in session:: agent::tests runtime::tests transcript::tests; do
  cargo test --bin kiki "$filter" || exit
done
```

### Task 3: Persist child admission and terminal results

**Changes**

Commit task admission before child startup and terminal results before exposing them. Carry originating foreground-step provenance independently of run-scoped start budgets. Retain task parameters, timing, histories and results, without retaining child arenas. Use the same storage-failure handling as foreground execution.

**Tests** (`src/subagents.rs`, coordinator/runner integration with real SQLite and controlled runners; `src/arena.rs` and `src/runtime.rs`, real Monty integration; `src/session/codec.rs`, task codec tests)

First red: `subagents::tests::task_admission_commit_precedes_runner_start`.

- Gate admission and terminal commits. A child cannot start before admission commits, and waiters cannot obtain an uncommitted terminal result.
- Preserve cancellation-before-admission semantics and result-admission arbitration without holding coordinator locks across writes. Failed saves stop both foreground and background work rather than leaving capacity or waiters stuck.
- Cover a child settling before its originating foreground step commits and a descendant admitted after that root run settles. Keep run-level budgets unchanged across multiple model steps.
- Round-trip task hierarchy, fixed selections, supported result values, attempt accounting, UTC timestamps and measured durations. Reuse the lossless value codec; keep structural SQL facts authoritative rather than duplicating them in task JSON. Derive task cost from retained attempts through existing pricing code and verify the resulting estimates; do not serialise derived cost separately.
- Shutdown completes with pending child saves without requiring the runtime event loop to pump write acknowledgements. No child arena/checkpoint is stored.

**Files**

- Modify: `src/session.rs`, `src/session/codec.rs`, `src/subagents.rs`, `src/agent.rs`, `src/arena.rs`, `src/runtime.rs`, `src/main.rs`.

**Verification**

Run the shared checks and the following commands. Admission, result publication, budget and shutdown tests all pass.

```sh
for filter in session:: subagents::tests arena::tests runtime::tests; do
  cargo test --bin kiki "$filter" || exit
done
```

### Task 4: Persist settled child-step history

**Changes**

Await child-step history commits before subsequent work. Preserve accumulated execution/attempt evidence and observations through guarded task revisions. Keep transient progress callbacks as observations, not synchronous database writes.

**Tests** (`src/subagents.rs` and `src/runtime.rs`, gated runner/runtime integration; `src/session.rs`, storage transition tests)

First red: `subagents::tests::child_step_commit_precedes_next_model_call`.

- Hold a settled child-step commit and prove the next model/Python step cannot begin. Persist recorded step/attempt evidence at durable steps, not through runtime-event consumption.
- Reject stale snapshots and prevent late updates from changing terminal state, duration or results.
- Count distinct attempts with identical usage values separately. Reconcile cumulative terminal attempts with step evidence without double counting or value-based deduplication.
- Child deadlines and cancellation remain serviced while commits are pending; failures use the established session-wide stop behaviour.
- Assert that child execution/history updates add no arena checkpoints.

**Files**

- Modify: `src/session.rs`, `src/subagents.rs`, `src/agent.rs`, `src/runtime.rs`.

**Verification**

Run the shared checks and the following commands. Child durability and existing foreground behaviour all pass.

```sh
for filter in session:: subagents::tests agent::tests runtime::tests; do
  cargo test --bin kiki "$filter" || exit
done
```

### Task 5: Recover interrupted operations

**Changes**

Settle unfinished foreground runs/steps and children from committed state. Preserve recorded timing and accounting without inventing offline duration or replaying work. Keep background-only recovery independent of foreground history.

**Tests** (`src/session.rs`, file-backed recovery and controlled subprocess tests; `src/session/codec.rs`, recovered-content round-trips)

First red: `session::tests::recovery_closes_pending_run_and_step`.

- Kill subprocesses at explicit generation, execution, committed-step and background-only milestones. Verify the last committed checkpoint, once-only foreground closure and existing failed child state with the specified interruption diagnostic.
- Preserve the active leaf when only background work was unfinished. Keep completed task results and close pending step links before projecting interrupted task provenance.
- Repeat recovery without duplicating outcomes, moving already-settled links or changing frozen elapsed observations. Recovery timestamps record detection, not an invented process-stop time; preserve uncertainty about unrecorded work/cost.
- Force a bounded hot rollback journal with a gated transaction, terminate its process, then exercise native SQLite recovery. Read-only access leaves it unchanged; writable recovery returns committed state. Unsupported-format preflight leaves database and journal bytes untouched.
- Verify process death releases OS ownership while the stable lock file remains. No sleep-based readiness or checkpoint benchmarks.

**Files**

- Modify: `src/session.rs`, `src/session/codec.rs`.

**Verification**

Run the shared checks and the following command. Recovery, native rollback and repeated-open assertions all pass.

```sh
cargo test --bin kiki session::
```

### Task 6: Restore executable state and readable history

**Changes**

Restore selected model context, the arena checkpoint and retained task handles. Rebuild display history and task views without replaying old notifications as new activity. Keep whole-session accounting separate from selected-history context usage. Failed executable restoration retains readable history in the existing disabled TUI.

**Tests** (`src/runtime.rs`, fresh-process continuation with scripted models and real Monty; `src/session.rs`, projection/integrity tests; `src/openai.rs`, replay validation; `src/transcript.rs`, `src/tui.rs` and `src/tui/subagents.rs`, existing projection/terminal helpers)

First red: `runtime::tests::resume_restores_context_arena_and_task_handles`.

- Continue in a fresh process using saved Python values, whole-file authorisations and old task handles connected to fresh host/coordinator bindings. Rehydration starts no child execution and retains the latest committed results.
- Select an earlier leaf in a branched fixture with newer sibling entries. Restore exactly that context/checkpoint, including cancellation/failure feedback and commentary continuation. No provider/Python work occurs until a new submission.
- Seed subsequent run/step/task allocation from the complete stored session, not just its selected projection, so recorded IDs are never reused.
- Validate code/result identities and provider replay before execution is enabled. Missing, corrupt or incompatible required state must not fall back to an empty arena or silently altered history.
- Where records remain readable, retain history scrolling and expansion while execution and state-changing navigation remain disabled.
- Preserve task admission/terminal chronology and monitor behaviour using recorded revisions. Latest task outcomes and durations stay frozen; restoration neither revives children nor clears records as though old requests were newly submitted.
- Include sibling-branch attempts in global totals once, while context usage comes from the selected history. Keep child results out of model context until explicitly retrieved/exposed through Python.

**Files**

- Modify: `src/session.rs`, `src/agent.rs`, `src/arena.rs`, `src/openai.rs`, `src/runtime.rs`, `src/subagents.rs`, `src/transcript.rs`, `src/tui.rs`, `src/tui/subagents.rs`, `src/main.rs`.

**Verification**

Run the shared checks and the following commands. Fresh-process continuation, exact replay, accounting and disabled-view tests all pass.

```sh
for filter in session:: agent::tests arena::tests subagents::tests runtime::tests openai::tests transcript::tests tui::; do
  cargo test --bin kiki "$filter" || exit
done
```

### Task 7: Persist session metadata and preferences

**Changes**

Acknowledge names and model/thinking choices only after persistence succeeds. Restore accepted per-model thinking choices and preserve settings-before-submit ordering. Apply built-in, user, project, session and explicit-environment precedence. After successful startup/restoration, save the effective choice, including environment overrides, as the latest session preference. Failed metadata writes use the established session-wide stop behaviour, not merely withheld acknowledgements. Keep Ctrl-S's user-default writes separate.

**Tests** (`src/runtime.rs`, gated metadata/runtime integration; `src/config.rs` and `src/config/settings.rs`, configuration unit tests; `src/tui.rs`, command/acknowledgement tests; `src/session.rs`, persisted metadata assertions)

First red: `runtime::tests::settings_ack_follows_preference_commit`.

- Block a preference commit: the accepted watch, per-model accepted choices and acknowledgement do not advance early; a following submission waits for the accepted selection. Already-started steps retain their fixed selection.
- Cover stale acknowledgements and existing name normalisation. `/name` without an argument remains a read-only name/usage query; do not introduce a clearing command.
- Failed name/preference writes cancel active foreground/background work, disable later submissions and state-changing navigation, and do not publish unsaved accepted metadata. A failed startup-preference write leaves readable history disabled rather than enabling execution.
- Restore previously accepted thinking choices when switching back to another model, independent of changed ordinary defaults.
- Verify the full precedence order. A successfully applied environment override becomes the saved choice used by a later resume without that variable; failed startup leaves saved choices unchanged.
- Ctrl-S changes the user-default file through the existing path, not session history. Session naming/preferences do not alter user/project configuration.

**Files**

- Modify: `src/session.rs`, `src/session/codec.rs`, `src/config.rs`, `src/config/settings.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`, `src/main.rs`.

**Verification**

Run the shared checks and the following commands. Metadata acknowledgement, precedence and existing settings behaviour all pass.

```sh
for filter in session:: config:: runtime::tests transcript::tests tui::; do
  cargo test --bin kiki "$filter" || exit
done
```

### Task 8: Expose the session CLI lifecycle

**Changes**

Add argument dispatch, listing, resume, directory guards, `--here` and exit IDs. Dispatch listing before runtime configuration. Compose the existing restoration path rather than adding a second one. Add the already-locked `time 0.3.55` dependency for readable UTC timestamps. CLI fixtures reuse the production schema without a library split or production test switches.

**Tests** (`tests/sessions_cli.rs`, actual binary process tests; `src/session.rs`, directory/metadata assertions; `src/tui.rs`, terminal-lifecycle helpers)

First red: `sessions_lists_without_runtime_configuration` in `tests/sessions_cli.rs`.

- List without credentials, Monty, valid model/prompt configuration or tracing startup. Cover newest-first sorting, UTC timestamps, name/first-request/empty fallbacks and bounded, terminal-safe previews. Mix sessions from two workspace roots and assert that only the current resolved root is listed, including equivalent symlink spellings.
- Check full canonical identifiers and invalid argument combinations before session lookup. Cover unknown IDs, stored/requested ID mismatches and ownership conflicts; failures never create a different session.
- Read listings while another process owns a session. Busy/recovery-needed/unreadable files produce individual warnings without suppressing healthy results or mutating sessions.
- Accept symlink-equivalent directories. Normal resume refuses a different root; successful `--here` saves the new root, binds actual tools there and warns about unchanged retained paths/data/effects. Failed restoration does not save relocation or startup preferences. A failed relocation write uses the session-wide stop behaviour and leaves readable history disabled, with neither relocation nor further execution accepted.
- Verify session-ID output follows terminal restoration on normal exit and handled errors after a session opens. Exercise startup, TUI and controller error paths rather than only the happy path.

**Files**

- Create: `tests/sessions_cli.rs`.
- Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/session.rs`, `src/config.rs`, `src/tui.rs`.

**Verification**

Run the shared checks and the following commands. CLI process and terminal-lifecycle assertions all pass.

```sh
cargo test --test sessions_cli
cargo test --bin kiki session::
cargo test --bin kiki tui::
```

### Task 9: Document persisted-session workflows

**Changes**

Document CLI usage, saved environment-driven choices, relocation, interruption, exit IDs, privacy and compatibility limits. Keep the spec's exclusions explicit; do not add backup/export workflows or checkpoint-size work.

**Tests** (documentation review)

This is documentation-only, so no first-red test applies. Check the examples against the implemented CLI and verify that the documentation makes no claim to undo external effects, resume child execution or protect against disk loss.

**Files**

- Modify: `README.md`.

**Verification**

Examples match the tested CLI. All whole-phase gates pass:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```
