# Plan: Model context compaction — Phase 3: Automatic compaction and recovery

## Source

- [Model context compaction spec](spec.md), Phase 3: Automatic compaction and recovery.
- Requires completed Phases 1 and 2, including canonical Session context, format-v2 Compaction Entries, tree/transcript projection, manual orchestration, and non-Run foreground lifecycle.
- Current code baseline before those phases: `45583c0`; re-check line references after Phase 2.
- [Session-tree merge impact analysis](../research/2026-09-21_session-tree-compaction-impact.md).
- This phase adds only `enabled`, three threshold boundaries, OpenAI overflow classification, and one compact-and-retry of the same pending Model Step.

## Acceptance criteria

1. `[compaction] enabled` is a strict user/project boolean with default `true`. `false` suppresses threshold compaction and overflow recovery but leaves `/compact` available.
2. Automatic threshold uses `context_tokens > context_window - reserve_tokens` with saturating subtraction and Phase 2 measurement. Equality does not compact.
3. Threshold checks run before request admission, between completed execution and the next Model Step, and after a committed final response before `RunSettled`. No-progress preparation skips silently.
4. Non-cancellation threshold failure persists Failed and continues the scheduled workflow with unchanged Model Context. Cancellation and late cancellation follow the boundary-specific matrix in the spec.
5. Threshold preparation captures one currently accepted `ModelSelection`; model changes after capture do not alter that summary request or its budgets. A later normal Model Step follows existing accepted-selection semantics.
6. Pre-request and post-final work reuse Phase 2's non-Run foreground state: Submit/Compact/tree are busy, cancellation/shutdown cannot invent a Run ID, and held requests are admitted at most once.
7. OpenAI overflow classification implements the exact code/status/message rules without classifying generic 400/413, authentication, rate limiting, broad token text, local response-size failures, or summary failures.
8. A recognised normal-step overflow compacts and retries the same pending durable Model Step at most once. Failed normal attempts, summary attempts, and retry attempts remain distinct and count once.
9. Second overflow, different retry failure, failed/no-op overflow compaction, disabled automation, persistence failure, interruption, and cancellation settle the pending Run exactly as specified without loops or orphaned steps.
10. Automatic/overflow Compaction Entries render, search, copy, navigate, branch, account, and restore through the Phase 2 transcript/tree projections. Applied work does not immediately retrigger from stale pre-compaction usage.

## Overall validation

Use real temporary format-v2 SQLite sessions and existing write/runner/operation gates for pending-step provenance, compaction ancestry, settlement, branch navigation, restart, and accounting. Use scripted models for threshold boundaries, setting changes, and retry sequences. Use the local OpenAI fixture server for exact overflow diagnostics. No live-provider tests are required.

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

Each task is one atomic signed-off commit. Affected unit tests live in the binary target, so focused commands use `cargo test --bin kiki`; the whole-phase gate also covers integration targets.

## Planning context

- Phase 2 supplies one preparation path, one guarded outcome commit, one Compaction Entry projection, and one foreground compaction lifecycle. Automatic triggers reuse them with reasons `Threshold` or `Overflow`.
- Effective settings resolve file layers field by field (`src/config/settings.rs:508`, `src/config/settings.rs:590`). Phase 2 carries resolved compaction policy on the existing startup/settings context; `enabled` joins that policy without a new authority.
- Catalogue models expose `context_window_tokens` and `max_output_tokens` (`src/config/models.rs:27`). Thresholds use the context window, never the long-context pricing threshold.
- Runtime publishes accepted selection through its existing watch channel and OpenAI samples it for each normal step (`src/runtime.rs:335`, `src/openai.rs:757`). Automatic preparation clones that receiver and samples once; it does not add selection state.
- Phase 2 broadens foreground state beyond `ActiveRun`. Preflight and post-final compaction use that operation protocol; inter-step and overflow compaction remain inside the persisted Agent worker.
- The persisted Agent loop allocates a Model Step before the provider call and currently closes it immediately on failure (`src/agent.rs:675`, `src/agent.rs:705`). Overflow recovery intercepts before closure, leaves that row pending, appends Compaction beneath the current Active Leaf, and settles the same step after retry.
- OpenAI transport/API failures become bounded `ProviderFailure { status, code, message }` (`src/model.rs:290`, `src/openai.rs:616`). Classification belongs in provider failure vocabulary, not Runtime string matching.
- Interrupted-operation recovery already appends one terminal beneath the current Active Leaf, settles pending Model Steps, and closes the open Run without replaying work (`src/session.rs:1418`). An Applied Overflow Compaction Entry therefore needs codec/projection support and regression coverage, not a second recovery algorithm.
- Summary failures, even when their provider diagnostic resembles overflow, settle that Compaction outcome. Only the normal Model Step path invokes overflow recovery.

## Tasks

### Task 1: Add automatic policy enablement

**Changes**

Add `enabled = true` to effective/file compaction settings with strict boolean validation and field-wise user/project precedence. Preserve Phase 2 numeric policy and immutable Runtime carrier; keep environment/session/runtime overrides absent.

Add a pure threshold decision to the compaction module. It takes measured context and the captured selection's context window, subtracts reserve with saturation, and returns true only for strict greater-than when enabled. Keep orchestration out of this task.

**Tests** (`src/config/settings.rs`, resolution; `src/compaction.rs`, policy unit tests)

First red: `config::settings::tests::compaction_enabled_defaults_true_and_project_overrides_user`.

- Cover default, user/project precedence, explicit false, wrong type, unknown neighbouring fields, Runtime-facing resolved policy, and unchanged numeric precedence.
- Cover below/equal/above threshold, reserve equal/greater than window, zero measured context, large checked values, enabled false, and context versus pricing thresholds.
- Manual preparation remains available regardless of `enabled`.

**Files**

- Modify: `src/config/settings.rs`, `src/compaction.rs`.

**Verification**

```sh
cargo test --bin kiki config::settings::tests
cargo test --bin kiki compaction::tests
```

### Task 2: Trigger threshold compaction at all boundaries

**Changes**

Reuse Phase 2 preparation/orchestration with reason `Threshold`:

- **Pre-request:** Runtime owns the submitted request in a preflight compaction operation before any Run/request entry exists. Measure and prepare only the current durable Active Leaf; the held request contributes zero bytes and cannot be summarised. Applied, non-cancellation Failed, or NoOp admits it exactly once; cancellation, shutdown, or disablement discards it without a Run.
- **Inter-step:** after durable execution settlement, Agent compacts before allocating/calling the next normal Model Step.
- **Post-final:** Runtime publishes the durable final record, runs post-final compaction while withholding `RunSettled`, then settles the already successful Run.

Clone the accepted-selection receiver into foreground workers and sample once per preparation. Use Phase 2's non-Run busy submission event (`active_run_id = None`) for preflight/post-final work. Reject Submit/Compact/tree while active. Shutdown cancels/awaits current work and never admits the held request or starts post-final work. A pending manual compaction suppresses automatic post-final work. It never runs in the pre-`RunSettled` automatic slot: when Final or Failed wins its cancellation race, publish that terminal, emit `RunSettled`, then start the retained manual operation from the terminal Active Leaf with unchanged focus, exactly as Phase 2 requires.

No-progress emits no automatic notice/card. Applied continues from the new projection. Non-cancellation Failed persists and continues with the parent context. Apply the complete cancellation/late-commit matrix. Exclude pre-compaction and summary usage from later baseline search so Applied cannot immediately retrigger.

**Tests** (`src/agent.rs`, inter-step scripted tests; `src/runtime.rs`, pre/post integration; `src/session.rs`, ancestry/accounting; `src/compaction.rs`, measurement regressions; `src/transcript.rs` and `src/tui.rs`, foreground lifecycle)

First red: `agent::tests::threshold_compacts_between_execution_and_next_model_step`.

- Exercise one above-threshold case at each boundary and prove the next provider sees summary plus exact retained tail. Task 1 owns exhaustive arithmetic.
- Preflight owns the request across Applied/Failed/NoOp and admits once. Tiny and very large held requests produce the same below/equal/above decision for identical selected context; vary selected context itself across all three boundaries. Cancellation by the linearisation point admits none; late cancellation keeps Applied but still admits none.
- Preflight shutdown/storage disablement discards the request, returns Agent, creates no Run, and clears non-Run foreground state. Further Submit/Compact/tree receives busy while active.
- Inter-step cancellation settles the Run cancelled; late cancellation keeps Applied and starts no next normal step.
- Post-final publishes final record before Applied/Failed/Cancelled card, then `RunSettled`; NoOp settles directly. Storage failure cannot undo the durable final and still emits settlement plus disabled state without a successful compaction card.
- Pending manual compaction suppresses redundant post-final automatic work. For both Final-wins and Failed-wins races, assert immediate `Accepted` when the command arrives, then terminal publication, `RunSettled`, and exactly one `Started` against that terminal leaf with unchanged focus. If settlement disables Runtime, assert no `Started`.
- Non-cancellation provider/validation failure persists Failed and the original scheduled call/settlement continues unchanged.
- Disabled/no-progress performs no summary, entry, event, or delay.
- A settings change before capture affects selection/window/budget; a later change leaves the summary fixed.
- Applied does not retrigger from retained-ancestor usage: only ordinary durable descendants of the latest Applied entry are baseline candidates. With no eligible descendant usage, measure the complete projected summary/tail by byte fallback. Summary attempts count once but never drive context percentage.
- Automatic rows/cards use existing reason, search, conversation, copy, branch-order, and navigation projection.

**Files**

- Modify: `src/agent.rs`, `src/compaction.rs`, `src/runtime.rs`, `src/session.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification**

```sh
for filter in agent::tests compaction::tests runtime::tests session:: transcript::tests tui::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 3: Classify OpenAI context overflow

**Changes**

Classify raw OpenAI status/code/message before diagnostic redaction or length bounding, then store the semantic overflow flag alongside the existing bounded/redacted provider diagnostic. Expose that provider-neutral flag on model failures and implement the exact OpenAI rules from the spec:

- exact ASCII case-insensitive code `context_length_exceeded` or `model_context_window_exceeded`, except status 401/429;
- otherwise status 400/413 plus one approved ASCII case-insensitive message phrase;
- no generic status-only, broad token matching, or local `ContentTooLarge` classification.

The semantic flag remains stable even when the approved phrase occurs beyond the retained diagnostic prefix or redaction changes stored text. Classification is otherwise a predicate only in this task; ordinary failure settlement remains unchanged until Task 4.

**Tests** (`src/model.rs`, predicate forwarding; `src/openai.rs`, diagnostic fixture/table tests)

First red: `openai::tests::provider_failure_classifies_approved_context_overflow_signatures`.

- Cover exact codes with absent/400/413 status, an otherwise ineligible status such as 500 plus unrelated message, and mixed case.
- Cover every approved phrase under 400 and 413.
- Exclude 401/429 even with matching code/message; approved phrases under absent/500 status; generic 400/413; `too many tokens`; unrelated maximum text; rate limits; authentication; transport failure; local `ContentTooLarge`; and near-match codes.
- Expose the stored semantic flag through `ModelFailure` without adapter parsing in Agent/Runtime.
- Put an approved phrase beyond the bounded diagnostic limit and around redacted material; classification remains true while stored diagnostics remain bounded/redacted.

**Files**

- Modify: `src/model.rs`, `src/openai.rs`.

**Verification**

```sh
cargo test --bin kiki model::tests
cargo test --bin kiki openai::tests
```

### Task 4: Compact and retry one overflowed step

**Changes**

Intercept recognised normal Model Step overflow before terminal closure when automation is enabled. Keep its durable `model_steps` row pending, retain failed-attempt accounting in memory, and run Phase 2 compaction with reason `Overflow`. On Applied, retry the normal provider call under the same step ID once. Retry uses the accepted selection at retry through existing normal-step semantics; compaction uses its captured selection.

Replace the domain's exclusive terminal-evidence enum with a composable record containing optional ordinary primary evidence (`Generation` or `Execution`) and optional `OverflowRecovery`. Keep the format-v2 wire additive: preserve the existing optional `evidence` enum exactly for primary evidence and add an omitted-by-default optional `overflow_recovery` sibling to Final, Cancelled, and Failed payloads. Phase-2-shaped v2 records must decode and newly encoded ordinary terminals retain their old shape.

`OverflowRecovery` is one initial-overflow record containing the recognised bounded structured provider diagnostic, optional bounded partial `GenerationEvidence`, and the initial failed call's `Vec<AttemptAccounting>`. The generation field is absent/`None` when overflow arrives before any content; do not manufacture an empty evidence value. Retry code/final content or ordinary terminal generation/execution evidence remains the sole retry representation; retry attempts remain exclusively on the retried code, Final, Failed, or Cancelled terminal's existing ordinary attempt field. Session totals traverse both owners once, while deterministic context measurement categorically ignores initial-overflow attempts and considers only eligible ordinary retry/result attempts. Keep this as one Agent-owned `Option<OverflowRecovery>` and derive the once-per-Run recovery guard from its presence; do not add tags, a collection, or a separate retry flag.

Before issuing retry generation, increment a per-step generation epoch from 0 to 1. Add exact reliable `RuntimeEvent::ModelGenerationRestarted { run_id, step_id, epoch }` and add `epoch` to `RuntimeEvent::ModelProgress`; ordinary generation uses 0 and retry progress uses 1. Emit the reset before any epoch-1 progress. Transcript resets partial reasoning, assistant text, decoded Python, and provider attempt number while retaining settled live accounting, then ignores delayed progress whose epoch is older than the live step. Retry provider attempt numbering may restart at one without concatenating initial output.

Carry the initial-overflow evidence until the Run terminal. Every non-crash terminal after recovery, including a successful final retry or successful recovered code followed by a later final, persists it. If recovered code execution fails or is cancelled, compose the same initial evidence with primary `ExecutionEvidence`; that execution outcome is the final surrounding failure/cancellation. A later ordinary generation failure composes its primary Generation evidence likewise. The interruption-recovery exception is explicit: after process loss, only durable rows survive, so the generic interruption terminal has no reconstructed in-memory overflow evidence or attempts.

For Failed/NoOp compaction, construct the exact terminal message `context overflow: {original}\ncompaction recovery failed: {recovery}`. Truncate each substituted component independently to its first 2,000 UTF-8 bytes at a scalar boundary without a marker; the total is at most 4,047 bytes. `{original}` is the bounded/redacted provider-failure display. `{recovery}` is the Failed Compaction diagnostic or literal `compaction could not remove a valid prefix`; the Failed Compaction Entry still owns its complete independently bounded 4,096-byte diagnostic, while the Run deliberately repeats the 2,000-byte excerpt. Second/different retry failure uses the existing bounded retry diagnostic; recovered-code execution failure uses the execution diagnostic. The initial structured overflow remains inspectable beside ordinary retry evidence. Restore/render this evidence through terminal inspection without adding it to Model Context or tree-copy payload.

Never recover recursively from a summary request. Second overflow or a different retry failure closes the pending Run without another compaction. Cancellation closes Cancelled according to the spec, including late Applied with no retry. Disabled automation follows the ordinary failure path.

**Tests** (`src/agent.rs`, scripted failure/summary/retry; `src/runtime.rs`, settlement/events; `src/session.rs` and `src/session/codec.rs`, evidence/provenance/accounting/recovery; `src/transcript.rs` and `src/tui.rs`, restored evidence)

First red: `agent::tests::context_overflow_compacts_and_retries_same_pending_step_once`.

- One Model Step allocation spans initial failure, Applied Overflow Compaction Entry, and successful code/final retry; settled entry descends from compaction.
- Initial normal attempts live only on the initial OverflowRecovery evidence, summary attempts only on the Compaction Entry, and retry attempts only on the retried ordinary result. Assert vectors/call counts on every branch and full totals once using deliberately identical values. With known initial usage followed by unavailable/zero retry usage, measurement skips Initial and uses full fallback rather than stale pre-compaction usage.
- Second overflow, different retry failure, or Failed/NoOp compaction closes once and starts no additional summary/retry. Assert exact 2,000-byte component truncation, scalar boundaries, separator, 4,047-byte maximum, no-progress literal, and the Failed Compaction entry's independent 4,096-byte diagnostic; initial overflow and ordinary retry evidence remain distinct.
- Immediate overflow with no streamed chunks stores `generation = None`; partial initial output stores bounded evidence. Both round-trip and render without a synthetic empty block.
- `ModelGenerationRestarted { epoch: 1 }` is reliable and precedes epoch-1 progress: partial/delayed epoch-0 progress cannot reappear, retry attempt 1 starts a clean live view, and settled live accounting remains visible even when the progress channel is saturated. All normal constructors/observers use epoch 0.
- Esc before summary writes no Compaction outcome and cancels with initial overflow/optional partial-generation evidence. Esc after an attempt persists Cancelled Compaction plus the same Run evidence. Late Esc after Applied prevents retry; cancellation during retry retains initial overflow plus ordinary primary generation evidence and stores retry attempts only on the ordinary Cancelled terminal.
- Exercise normal selection A, summary capture B, change to C while B is in flight, and retry C.
- Disabled, non-overflow, authentication, local response-size failure, and summary overflow never enter recovery.
- A successful final retry persists the initial OverflowRecovery evidence but no duplicate retry primary evidence because Final already stores the retry result. A recovered code response whose execution fails/cancels composes initial overflow with primary Execution evidence and uses execution as the final diagnostic.
- Open literal Phase-2-shaped format-v2 Final, Cancelled, and Failed fixtures using the old `evidence` enum (including Phase 2 execution file vectors), then round-trip new additive recovery fields without bumping format version.
- Outcome persistence failure leaves the step for existing interruption recovery, disables work, and reports no retry.
- Reopen after Applied but before retry: generic recovery retains the Compaction Entry, appends one interruption terminal beneath it, settles the same step once, keeps tree/catalogue/context readable, and never replays provider work. Lost in-memory overflow evidence/attempts are not reconstructed; no pending-attempt journal is added.
- Directly select the Applied Overflow entry, branch from it, return to the retry descendant, and verify selected/off-branch normal and summary attempts count once while latest normal context usage remains correct.
- Reopen after successful code/final retry restores one settled step beneath the Overflow entry, exact context/card/tree row, initial-overflow evidence, and once-counted attempts.

**Files**

- Modify: `src/agent.rs`, `src/openai.rs`, `src/runtime.rs`, `src/session.rs`, `src/session/codec.rs`, `src/subagents.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification**

```sh
for filter in agent::tests runtime::tests session:: transcript::tests tui::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 5: Document automatic compaction

**Changes**

Update compaction documentation with `enabled = true`, all threshold boundaries, strict arithmetic, disabled/manual behaviour, one recognised-overflow retry, cancellation outcomes, branch/tree presentation, and recovery limits. Remove Phase 2's statement that automation is unavailable. Keep estimator, Arena, path privacy, format, and no-deletion caveats accurate.

**Tests** (documentation review)

No first-red test applies. Check every setting/default, trigger, status/card/tree row, retry bound, and failure claim against implemented tests.

**Files**

- Modify: `README.md`.

**Verification**

Run the whole-phase gates:

```sh
cargo fmt --all -- --check
git diff --check main...HEAD
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
just test
```
