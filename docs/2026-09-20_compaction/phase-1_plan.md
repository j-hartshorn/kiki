# Plan: Model context compaction — Phase 1: Canonical Session Model context

## Source

- [Model context compaction spec](spec.md), Phase 1: Canonical Session Model context.
- Current implementation baseline: `45583c0`, including interactive session-tree navigation and correction.
- [Session-tree merge impact analysis](../research/2026-09-21_session-tree-compaction-impact.md).
- This phase changes ownership without adding compaction entries, prompts, settings, commands, provider operations, file tracking, format changes, or UI.
- Follow [the worktree instructions](../../worktrees/README.md); implement on `work/compaction` in `worktrees/compaction`.

## Acceptance criteria

1. The Session Model is the sole owner of root Model Context during fresh, restored, and interactively navigated sessions. It exposes immutable history snapshots; `Agent` has no `ModelHistory` field or independently mutable root history.
2. Request, execution, and terminal commits publish their context change only after durable success. A blocked commit prevents the next provider call from observing that entry, and a failed commit leaves the prior projection intact.
3. Startup and `/tree` navigation activate one Session projection only after model/Arena validation. Navigation persists the guarded Active Leaf and publishes its matching projection as one Session operation; failure changes neither.
4. Live continuation, same-process branch return, and fresh-process restoration produce equal Model Context, including user requests, completed code/result pairs, cancellation/failure feedback, final replies, call identifiers, reasoning summaries, and opaque provider replay.
5. Root multi-Run operation, duplicate-call protection, early-cancellation correction, branch-scoped subagent access, child final-result cancellation arbitration, usage accounting, and existing durability barriers behave as before.
6. Ephemeral subagent runs retain task-local multi-step context for one child Run without moving that context into SQLite or reintroducing Agent-owned history.
7. Invalid restored or navigated history retains the existing disabled/unchanged Runtime behaviour rather than activating empty, stale, or partial context.

## Overall validation

Use temporary file-backed SQLite sessions and existing write/restore gates to prove commit and activation ordering. Use scripted models for exact provider-observed context and real Monty for multi-step code/result and navigation continuity. Extend current startup, navigation, correction, branching, and fresh-process tests rather than adding a parallel restoration harness.

Before tests which execute Python:

```sh
just monty
export MONTY_BIN=/tmp/kiki-monty/bin/monty
```

Every task runs:

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

Affected unit tests live in the binary target, so focused commands use `cargo test --bin kiki`; the whole-phase gate also covers integration targets. Each task is one atomic signed-off commit.

## Planning context

- `Agent` constructs, restores, reads, replaces, clones, and mutates `model_context` (`src/agent.rs:537`, `src/agent.rs:560`, `src/agent.rs:602`, `src/agent.rs:634`). The persisted loop already publishes context changes only after durable commits.
- `SessionState` is shared by cloned `SessionWriter` handles and currently owns the SQLite connection plus active-execution marker (`src/session.rs:480`, `src/session.rs:801`). A private immutable history snapshot can share this state without moving the Session Model through the worker or retaining the SQLite lock across awaits.
- Session restoration projects arbitrary selected ancestry into records, tasks, checkpoint, and `ModelHistory` (`src/session.rs:1870`, `src/session.rs:2056`, `src/session.rs:2754`). This remains the one reconstruction algorithm.
- Runtime startup validates projected history and restores Arena before enabling Agent (`src/runtime.rs:701`). Interactive navigation additionally restores the target Arena, commits the observed-leaf-guarded selection, replaces selected subagent origins, then installs Agent history and display records (`src/runtime.rs:1092`).
- Request, execution, and closure transactions derive their parent from the durable Active Leaf and advance it atomically (`src/session.rs:1129`, `src/session.rs:1190`, `src/session.rs:1247`). Context publication must correspond to that exact committed entry.
- `run_persisted` allocates each durable Model Step, checks duplicate call IDs, and calls the provider against Agent-owned history (`src/agent.rs:634`, `src/agent.rs:675`, `src/agent.rs:707`). It can instead read a Session snapshot from the existing cloned writer handle.
- `run_with_progress` remains the ephemeral child/test path and currently reuses the same Agent field (`src/agent.rs:1041`). One invocation-local history preserves its multi-step behaviour without a storage port.
- Session writers are cloned into subagent persistence (`src/subagents.rs:2464`). Task admission/history writes must not read or alter root Model Context.

## Tasks

### Task 1: Make Session own persisted root context

**Changes**

Add one private live Session value containing the effective selected entry and an immutable `ModelHistory`. Fresh sessions initialise it at the root; opened sessions keep it unavailable until Runtime activates the validated restored candidate. Expose only an immutable history snapshot to consumers, without an exported projection type, identity getter, SQLite guard, or transaction crossing an await.

Replace the public select-then-install protocol with one guarded Session activation operation. After Runtime validates model history and restores Arena, activation verifies the observed leaf, persists selection when the target differs, and publishes the matching candidate history only after commit. Startup uses the same operation against the already selected leaf. A stale guard, write failure, or invalid candidate changes neither Active Leaf nor live history; the lower-level selection write remains private to Session.

For request, execution, and terminal writes, require the private live identity to equal the transaction's durable Active Leaf before mutation. Reject unavailable/mismatched state before mutation. Validate proposed context, commit against that parent, then publish the matching immutable history. Rollback leaves it unchanged. Model-step allocation and other context-neutral writes do not alter it.

Route root provider calls and duplicate-call checks in `run_persisted` through Session snapshots. Stop reading or mutating Agent history for persisted root work while preserving commit acknowledgement before later provider/Python work and keeping existing progress/event payloads unchanged. Runtime retains its shared Session handle and gives the worker a clone.

On startup, complete model/Arena validation and existing preference/relocation persistence before activation. On navigation and early-cancellation correction, preserve current Arena compensation order, then use atomic Session activation before replacing selected subagent origins and Display History. Remove root use of `Agent::replace_idle_state`. A failed path retains the previous live history and existing disabled/compensation behaviour.

**Tests** (`src/session.rs`, file-backed transaction/activation tests; `src/agent.rs`, persisted scripted tests; `src/runtime.rs`, startup/navigation integration)

First red: `runtime::tests::persisted_agent_reads_context_from_session_model`.

- Fresh root state and each context-bearing commit produce the exact provider-observed history through cloned Session handles.
- Request/execution/closure write gates keep the preceding history visible until durable commit; rollback, stale identity, unavailable history, and invalid transition publish nothing.
- Atomic activation covers startup's already-selected leaf, arbitrary navigation, direct Compaction-free branch points, stale observed leaf, selection rollback, and same-process branch return. Active Leaf and live history never expose a mixed pair.
- Model-step allocation and representative task/metadata/active-execution writes leave provider-observed history unchanged.
- Preload Session context, construct an otherwise fresh Agent, and prove the next provider call receives only that context. Multi-Run, duplicate-call, storage-failure, completed-code, and final-message barriers remain unchanged.
- Extend `navigation_restores_arena_and_context_before_publishing_history`: failed model validation, Arena restore, stale leaf, or selection write keeps provider-observed context and Display History on the prior branch; success publishes both only after activation.
- Extend startup preference/relocation gate tests: no provider work or activation occurs during blocked persistence or after validation/Arena/persistence failure; successful startup activates once before normal work.
- Return to a sibling branch in the same process and reopen it in a fresh process; provider-observed context, checkpoint, task visibility, accounting, and Display History align.
- Early-cancellation correction uses the same activation seam and cannot expose cancelled-branch context after selecting the request parent.

**Files**

- Modify: `src/agent.rs`, `src/runtime.rs`, `src/session.rs`.

**Verification**

```sh
cargo test --bin kiki session::tests -- --test-threads=1
cargo test --bin kiki agent::tests -- --test-threads=1
cargo test --bin kiki runtime::tests -- --test-threads=1
```

### Task 2: Localise ephemeral Run history

**Changes**

Give each `run_with_progress` invocation one local `ModelHistory` spanning its complete model/Python loop and drop it at settlement. Remove `Agent.model_context`, the restored constructor, idle-history replacement, and root-history test helpers. Keep `Agent::new(arena, model)` as the production constructor and leave the Agent interface no broader than root Runtime plus one-shot child Runs.

Return `FinalMessage` directly from `run_with_progress` and `run`; Rust ownership provides one-time resolution. Remove `AgentRunOutcome`, staged-final state, its replay, admit/discard methods, unresolved-state guard, and staged-final errors. The child caller retains the existing cancellation check before accepting the returned message and collecting result values. Keep subagent task-history persistence, result collection, budgets, invocation context, and child Agent construction otherwise unchanged.

**Tests** (`src/agent.rs`, ephemeral loop/final-result regressions; `src/subagents.rs`, child-run integration; `src/arena.rs`, child Agent integration; `src/runtime.rs`, compile/regression)

First red: `agent::tests::ephemeral_run_keeps_multi_step_context_without_agent_state`.

- One ephemeral Run preserves every model/code step; a later invocation starts without leaked context.
- A returned final is accepted exactly once by ownership, and cancellation winning before child result admission still rejects it without retaining replay or Agent state.
- Existing pre-cancelled, failure, timeout, accounting, subagent spawning, result retrieval, result-value collection, nested-child, navigation, and correction tests remain green.
- Code review and compilation confirm the Agent history/staged-final fields, `AgentRunOutcome`, restored constructor, admit/discard methods, staged-final errors, and `replace_idle_state` call sites are gone.

**Files**

- Modify: `src/agent.rs`, `src/arena.rs`, `src/runtime.rs`, `src/subagents.rs`.

**Verification**

```sh
for filter in agent::tests subagents::tests arena::tests runtime::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```
