# Plan: Anthropic provider — Phase 1: Selection per run

## Source
- Spec: `docs/2026-09-23_anthropic-provider/spec.md`, Phase 1 (Proposed approach › Phase 1: selection per run)

## Acceptance criteria
- Every model step in a run uses the model and thinking selection that was current when the run was submitted. This includes steps after tool executions and restarted stream attempts.
- `/model`, `/thinking` or Shift-Tab during a run are committed and acknowledged straight away (`SettingsAccepted`), and the next run's first model step uses them.
- A change accepted during a run that is then cancelled still drives the next run.
- Restore and `/tree` navigation validate history against the session's current selection.
- Subagents keep the selection fixed when they are admitted. A child with no override and no favourites gets the selection of the run that invoked it, not a change made later in that run.
- No `watch` channel carries `ModelSelection` anywhere in `src/`.

## Overall validation
- `cargo fmt --all -- --check`: no diff.
- `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets -- --test-threads=1`: all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `rg -n "watch::(Sender|Receiver|channel)" src | rg -i selection` and `rg -n "watch::channel\((selection|effective|initial_selection)" src`: no matches.
- Manual: `just run`, submit a request that runs several code steps, and change `/model` while it runs. The acknowledgement appears immediately, the in-flight run's attempts (trace or accounting) keep the old model, and the next request uses the new one.

## Planning context
- Selection is currently read in these places:
  - `OpenAiModel` reads its `watch::Receiver` in `validate_history` and on each `step` (`src/openai.rs:53,1109,1128`).
  - Constructors built around the receiver: `with_settings`, `with_fixed_selection` and the test-only builders (`src/openai.rs:84-171`).
  - `main.rs` creates the channel and hands the receiver to the model (`src/main.rs:46-51,243,342`).
- The runtime stores the current selection as a `watch::Sender`:
  - the field is at `src/runtime.rs:377`
  - it is updated with `send_replace` in `ApplySettings` (`src/runtime.rs:1169`)
  - it is read for compaction (`src/runtime.rs:1516`), restore (`src/runtime.rs:809-812`) and navigation (`src/runtime.rs:1261`)
- The run entry points are:
  - `Agent::run_persisted`, called at `src/runtime.rs:1742`
  - `Agent::run_with_progress`, called at `src/subagents.rs:1419`, and the test-only `Agent::run` wrapper (`src/agent.rs:693`)
  - the `Agent::validate_restored_history` wrapper (`src/agent.rs:581-586`)
  - model steps at `src/agent.rs:773,1097`
- `ModelStep.selection: Option<ModelSelection>` (`src/model.rs:32`) exists only because the model was the one that knew the selection:
  - OpenAI sets it at `src/openai.rs:769`
  - the agent copies it into pending steps and `ExecutionStarted` (`src/agent.rs:933,1159,1183,1262`)
  - the invocation context passes it on to subagent resolution (`src/subagents.rs:893`)
- The child side:
  - `ChildModelFactory::create(prompt, selection)` is at `src/subagents.rs:1228-1232`
  - the child runner already has the admitted selection (`src/subagents.rs:1315`)
  - `OpenAiChildModelFactory` is at `src/openai.rs:60-78`
- Tests that pin today's mid-run behaviour:
  - `settings_committed_during_a_started_step_apply_to_the_later_step` (`src/runtime.rs:7408`)
  - `an_accepted_command_before_the_step_snapshot_applies_to_that_step` (`src/runtime.rs:8624`)
  - the OpenAI receiver tests (`src/openai.rs:2412-2610`)
  - `active_settings_survive_cancellation_and_drive_a_later_run` (`src/runtime.rs:8676`) stays valid and must keep passing
- About 60 test `Model` impls live in `src/model.rs`, `src/agent.rs`, `src/arena.rs`, `src/runtime.rs` and `src/subagents.rs`, and many runtime tests build `watch::channel(selection)`. The signature changes in these are mechanical.
- Conventions:
  - Tests live in each module's `#[cfg(test)] mod tests`.
  - Provider fixture tests use a loopback SSE server in `src/openai.rs`.
  - Commits are signed off.

## Tasks

### Task 1: Runs pass one captured selection to every step
**Changes**
- `Model::step` and `Model::validate_history` receive the selection explicitly.
- `Agent::run_persisted`, `Agent::run_with_progress` and the test `Agent::run` take the run's selection and pass it to every step.
- The runtime captures the current selection when it starts a submitted run. Restore and navigation validate against the current selection.
- `ModelStep` no longer carries a selection. The agent fills pending steps and `ExecutionStarted` from the run's selection.
- Child runs pass their admitted selection. `ChildModelFactory::create` no longer takes a selection.
- `OpenAiModel` no longer holds a receiver, and its constructors take just the prompt and API key (plus the endpoint in tests).
- `main.rs` builds the model without the channel receiver. The runtime keeps its sender for now, although nothing receives from it.
- `README.md` states that selection changes made during a run apply from the next run. The subagent paragraph (`README.md:165`) says "invoking run's selection".

**Tests**
- Red first, in the `src/runtime.rs` controller tests: rewrite `:7408`. A change accepted during the first step is acknowledged immediately, both of the run's steps observe `a`, and a follow-up run observes `b`.
- Delete `:8624` and its `SnapshotAfterGateModel`. With the selection captured when the run starts, "before the step snapshot" is the same path as `:7408`.
- `src/agent.rs:2276`: the test model reports no selection, and `ExecutionStarted` carries the run's selection.
- `src/subagents.rs`: a default child admitted after a mid-run `ApplySettings` gets the invoking run's selection.
- `src/openai.rs`: replace the receiver tests at `:2412-2610` with one fixture test asserting that the request body uses the selection passed to `step`.
- Keep `src/runtime.rs:8676` passing unchanged.
- Update the test models' signatures and `ModelStep` literals mechanically.

**Files** — Modify: `src/model.rs`, `src/agent.rs`, `src/openai.rs`, `src/runtime.rs`, `src/subagents.rs`, `src/arena.rs`, `src/main.rs`, `README.md`

**Verification**
- `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets -- --test-threads=1`: all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `rg -n "watch::Receiver<ModelSelection>" src`: no matches.

### Task 2: In-memory execution selection is always present
**Changes**
- The in-memory execution selection becomes a plain `ModelSelection` rather than `Option`. This covers:
  - `AgentProgress::ExecutionStarted` (`src/agent.rs:102`)
  - `PendingCode` (`src/agent.rs:455`)
  - the subagent invocation and trusted contexts (`src/subagents.rs:852,871`)
- In the arena (`src/arena.rs:987-997`), the selection travels with the invocation context. An execution with an invocation always has a selection, and a standalone execution has neither. No selection is fabricated.
- `PersistedAgentEvent::ExecutionStarted` carries the pending step's selection, so the runtime can build `AgentProgress::ExecutionStarted` from it (`src/runtime.rs:1660`, `src/agent.rs:142,893`).
- The "subagent model selection is unavailable" error branch (`src/subagents.rs:893-895`) is removed.
- Persisted `Option` selections (`src/session.rs:223`) are unchanged.

**Tests** — This is a pure refactor, so there is no red test. Update the tests that construct these types, and delete any test that only asserts the removed error branch.

**Files** — Modify: `src/agent.rs`, `src/arena.rs`, `src/subagents.rs`, `src/runtime.rs`

**Verification**
- The test and clippy commands above: all pass and clean.
- `rg -n "selection is unavailable" src`: no matches.

### Task 3: Runtime holds the current selection as a plain value
**Changes** — The runtime controller stores the current `ModelSelection` directly instead of a `watch::Sender`, and its constructors take a selection. `main.rs` creates no channel. This is a pure refactor with no change in behaviour.

**Tests** — Update existing runtime tests mechanically (`watch::channel(selection)` becomes the selection, and the test `with_settings` builder takes a selection). No new test, because behaviour is unchanged and Task 1's tests cover it.

**Files** — Modify: `src/runtime.rs`, `src/main.rs`

**Verification**
- The test and clippy commands above: all pass and clean.
- `rg -n "watch::(Sender|Receiver)<ModelSelection>|watch::channel\((selection|effective|initial_selection)" src`: no matches.
