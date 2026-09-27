# Plan: Model context compaction — Phase 3: Automatic compaction and recovery

## Source

- [Model context compaction spec](spec.md), Phase 3. Phases 1 and 2 are merged on `main`.
- Baseline reviewed at `d5212a0`, after the Anthropic provider merge. Re-check code locations if `main` advances.
- [Anthropic provider spec](../2026-09-23_anthropic-provider/spec.md) defines one selection per Run and cross-provider reasoning policy.
- [Anthropic API errors](https://platform.claude.com/docs/en/api/errors#thinking-block-no-longer-matches-the-conversation) document signed-thinking prefix binding; HTTP 413 `request_too_large` is a byte-size limit, not a token-overflow signal.

## Acceptance criteria

1. `[compaction] enabled` is a strict user/project boolean, defaults to `true` even with existing numeric-only settings, and uses field-wise precedence. `false` disables automatic thresholds and overflow recovery, not `/compact`.
2. Just before every root normal Model Step, after the request or prior execution commits and before step allocation, estimate the selected context against the captured Run model: strictly `context_tokens > context_window.saturating_sub(reserve_tokens)`. Equality skips. A first-step check includes the committed request; there is no automatic pre-admission or post-final operation.
3. Applied threshold compaction changes the projected context before the next provider call; no-progress does nothing and a provider-started non-cancellation Failed outcome persists without blocking that call. Cancellation settles the already admitted Run as Cancelled, including at the first check; a late cancellation cannot roll back an Applied entry or start a Model Step.
4. Every normal step, threshold summary, overflow compaction and retry uses the Run's immutable `ModelSelection`. A mid-Run accepted setting affects the next Run; existing queued/idle manual compaction captures the accepted selection when it starts.
5. Measurement never bases a new model's threshold on another model's reported usage. Foreign-provider replay uses the existing shared semantic parts consumed by both adapters, respecting `cross_provider_reasoning = "drop"`; the same policy removes foreign reasoning from new summary inputs. Cache read/write fields are not added twice. Below-threshold checks read only selected ancestry, reuse indexed ancestor validation and skip prompt construction; abandoned replay blobs are not loaded.
6. Anthropic normal steps retain native replay while requesting documented `drop_block` thinking-binding behaviour and its required beta header. A summary followed by retained signed thinking must not create an avoidable prefix-binding 400; ordinary unchanged replay still works.
7. OpenAI recognises only the spec's exact raw code/status/message overflow signatures; authentication, rate limits, generic 400/413, broad token text, local response-size and summary failures remain ordinary. Anthropic recognises only a verified positive prompt-too-long signature; if none can be verified, it fails closed with no Anthropic overflow recovery, without disabling Anthropic threshold or manual compaction. Byte-size 413, output `max_tokens`, overload and signature-binding errors never masquerade as context overflow.
8. A recognised normal-step overflow compacts and retries the same pending durable Model Step at most once with the same Run selection. Initial failure, summary and retry attempt accounting have disjoint owners and count once on selected and abandoned branches; ordinary attempts remain eligible for context measurement while initial overflow attempts are billable-only and never supply a post-compaction baseline.
9. Applied/Failed/Cancelled overflow Compaction Entries remain branchable, searchable, restorable, copyable and inspectable. Final/Failed/Cancelled terminal evidence composes the initial overflow with ordinary retry evidence; Phase-2-shaped format-v2 records remain readable and ordinary new records omit the new field.
10. Second overflow, different retry failure, Failed/NoOp/pre-start-failed compaction, interruption, persistence failure and Esc settle or recover the open Run/step without another retry, missing settlement, or invented provider work. A retry begins a new live generation epoch before lossy progress, without erasing settled usage or allowing delayed normal-step progress to repaint automatic compaction or the retry.
11. Final replies publish and settle without automatic post-final summary latency. Existing manual acknowledgement/draft, retained-manual/correction ordering, and non-Run lifecycle stay unchanged. No automatic compaction is added to task-local subagent histories.

## Overall validation

Use scripted `Model` tests with real temporary format-v2 SQLite sessions, existing writer/operation gates and both providers' local HTTP/SSE fixtures. Verify the affected production adapter with an opt-in live Anthropic manual-compaction smoke test if credentials are available. Do not manufacture an Anthropic overflow signature from a synthetic fixture or run an expensive over-window live prompt as a required test; an unverified signature remains unrecognised and documented.

Before tests that execute Python:

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

Each task is one atomic signed-off commit. Unit tests live in the binary target, so focused commands use `cargo test --bin kiki`; the whole-phase gate also covers integration targets.

## Planning context

- `SessionWriter::prepare_compaction` snapshots ancestry and `settle_compaction` guards the selected leaf, checkpoint and Run provenance before publishing projected context (`src/session.rs:987`, `src/session.rs:1009`). The current snapshot reads all Session Entries and provider replay blobs (`src/session.rs:2422`); for each Compaction Entry, it rebuilds payloads and linearly looks up ancestors (`src/session.rs:2589`, `src/session.rs:3422`). Pre-step preparation must read only the selected chain and validate against indexed selected rows.
- Manual summary orchestration currently hard-codes `CompactionReason::Manual` (`src/agent.rs:617`); its summary loop and settlement should serve all reasons. `FailedBeforeStart` loses its diagnostic (`src/agent.rs:662`). Transcript enters its *manual* state on `/compact` acknowledgement (`src/transcript.rs:2660`); automatic work needs a Run-scoped notification for status and spinner (`src/transcript.rs:2442`), not a fake acknowledgement.
- Runtime captures the accepted selection at `start_run` (`src/runtime.rs:1590`). `Agent::run_persisted` commits the request, allocates a Model Step, and loops after durable execution settlement (`src/agent.rs:704`, `src/agent.rs:742`, `src/agent.rs:940`). The normal failure path closes the step immediately (`src/agent.rs:779`); intercept overflow before it closes.
- `SettingsContext` already carries resolved compaction numbers and `cross_provider_reasoning` (`src/config/settings.rs:136`, `src/config/settings.rs:189`). Runtime receives both through `with_session` (`src/runtime.rs:756`); provider adapters already use the reasoning policy (`src/provider.rs:21`, `src/model.rs:253`). No new configuration authority is needed.
- `src/compaction.rs:293` does not verify the reported-usage model; `src/compaction.rs:510` counts opaque replay even when both adapters render shared `ModelContextEntry::foreign_parts` (`src/model.rs:253`), and `src/compaction.rs:559` includes foreign reasoning in summary input under `drop`. Ordinary Anthropic usage already normalises uncached, cache-read and cache-write input (`src/anthropic.rs:622`).
- Anthropic replays native signed thinking verbatim (`src/anthropic.rs:99`, `src/anthropic.rs:794`). The documented binding-control beta and `drop_block` field belong in that normal-step adapter, not in Session history or the summary request.
- Both adapters produce bounded `ProviderFailure` only after seeing raw provider diagnostics (`src/model.rs:407`, `src/openai.rs:577`, `src/anthropic.rs:644`). `ProviderModel` routes both operations by selection (`src/provider.rs:66`). The official Anthropic error reference documents `invalid_request_error` as broad and `request_too_large` as a byte-size limit; Pi's prompt-too-long example alone does not verify current Anthropic positive wording.
- The existing `model_steps` row has one settled entry, while attempts are stored on completed execution, terminal or Compaction Entries (`src/session/schema.sql:25`, `src/session/codec.rs:594`). `codec::decoded_attempts` serves both context facts (`src/session.rs:2553`) and off-branch footer adjustments (`src/session.rs:2994`); initial overflow attempts need a separate billable view. Seven `Agent::run_persisted` paths close the Run (`src/agent.rs:732–1032`). Interruption recovery already closes a pending step and Run without replay (`src/session.rs:1955`).

## Tasks

### Task 1: Make compaction input and measurement provider-aware

**Changes**

Carry the resolved cross-provider reasoning policy through Agent's existing manual-compaction caller and Runtime into Session preparation and its pure compaction planner. New manual and automatic summaries omit foreign reasoning under `drop` while preserving same-provider reasoning and stored history. Retained-tail and threshold byte estimates count native replay as before, but use `ModelContextEntry::foreign_parts`, the same decision consumed by both provider adapters, to count foreign text and tool-call values (including ID, `execute_python`, source and feedback) once. Do not reimplement adapter wire rendering, count opaque foreign replay or estimate provider JSON syntax.

Use a normal usage baseline only when its recorded model matches the selected model, its owning response follows the latest Applied compaction, and projected ordinary replay has no foreign-provider entries. When the newest known usage belongs to another model, or any projected replay is foreign, use the full projected estimate rather than searching back to an older baseline. Keep summary/initial-overflow attempts ineligible. Do not add persisted rendering fingerprints or a new provider-estimator trait.

**Tests** (`src/compaction.rs`, unit; `src/session.rs`, real SQLite; `src/runtime.rs`, resolution)

First red: `compaction::tests::foreign_history_uses_semantic_bytes_and_drops_reasoning_for_summary`.

- OpenAI→Anthropic and Anthropic→OpenAI with large opaque replay but small semantic output: threshold/tail measurement uses the destination view derived from shared foreign parts. Compare both adapters' existing foreign-rendering fixtures under `inline` and `drop`; same-provider summary keeps reasoning, foreign summary with `drop` omits a sentinel.
- Latest known usage from another model, same provider or foreign, forces fallback rather than an older matching baseline; own-model usage after an Applied entry can baseline again when eligible. A changed reasoning setting on resume cannot reuse a baseline over foreign replay. Cache read/write counts are subsets, not extra context tokens.
- Root and restored branch projections agree; prior summaries can still contain historical text, and no stored Session Entry is rewritten.

**Files** — Modify: `src/agent.rs`, `src/compaction.rs`, `src/session.rs`, `src/runtime.rs`.

**Verification**

```sh
cargo test --bin kiki compaction::tests
cargo test --bin kiki session::tests
cargo test --bin kiki agent::tests
cargo test --bin kiki runtime::tests
cargo test --bin kiki anthropic_entries_replay_as_assistant_text_and_function_calls_with_their_results
cargo test --bin kiki openai_entries_replay_as_text_and_tool_use_merged_into_alternating_roles
```

### Task 2: Preserve Anthropic continuation through compaction

**Changes**

Set the documented `thinking.block_binding.prefix_mismatch_behavior = "drop_block"` and `thinking-binding-controls-2026-08-01` beta header on Anthropic normal steps, leaving summary requests without signed replay. Do not rewrite durable native replay or change foreign semantic rendering. Keep adapter error sanitisation and retry behaviour intact.

**Tests** (`src/anthropic.rs`, local SSE fixture)

First red: `anthropic::tests::normal_step_requests_drop_only_mismatched_bound_thinking`.

- A normal request with valid prior signed thinking, and a canonical summary followed by a retained signed-thinking/tool-use tail, both carry the field and header. Step replay keeps its original blocks and tool-result pairing; summary requests remain tool-free with no binding control.
- An `invalid_request_error` about a signed-thinking prefix remains an ordinary failure, not a future overflow signal. Add an opt-in adapter-level live summary then retained signed-thinking continuation smoke check if credentials are available; existing in-process Runtime tests cover `/compact` orchestration. Do not require live calls in credential-free CI.

**Files** — Modify: `src/anthropic.rs`.

**Verification**

```sh
cargo test --bin kiki anthropic::tests
```

### Task 3: Enable the automatic threshold policy

**Changes**

Add `enabled = true` with strict boolean user/project resolution and unchanged numeric precedence. Keep environment/session/picker overrides absent. Put strict threshold arithmetic and no-progress gating behind existing compaction preparation, not in Runtime string checks; manual preparation ignores `enabled`, overflow preparation honours it. On a below-threshold check, return after context measurement, before retained-tail selection and summary-input assembly; do not call a summary provider or introduce a second context estimator.

**Tests** (`src/config/settings.rs`, layered settings; `src/compaction.rs`, pure policy; `src/session.rs`, temporary SQLite)

First red: `config::settings::tests::compaction_enabled_defaults_true_and_project_overrides_user`.

- Omitted field (including Phase-2-shaped config), explicit false, user/project precedence, wrong type, unknown neighbouring field, numeric precedence and Runtime-facing resolved value.
- Below/equal/above strict threshold; reserve equal to or larger than window; zero context and checked large values. Use context-window limits, not the pricing-band threshold. Below-threshold preparation skips summary-input assembly. Disabled/no-progress have no provider attempt or Compaction Entry; `/compact` still prepares.

**Files** — Modify: `src/config/settings.rs`, `src/compaction.rs`, `src/session.rs`.

**Verification**

```sh
cargo test --bin kiki config::settings::tests
cargo test --bin kiki compaction::tests
cargo test --bin kiki session::tests
```

### Task 4: Compact immediately before each root Model Step

**Changes**

Make Agent's existing manual summary loop, provider-started signal, cancellation choice and settlement one internal reason-neutral operation for manual, threshold and (in Task 7) overflow compaction. In `run_persisted`, check after committed request and after each committed execution, before allocating the next Model Step, with the selection captured at Run admission. Only a triggered provider attempt emits a reliable Run-scoped Started event and committed outcome card; do not emit a manual `/compact` acknowledgement or consume the editor draft. Applied continues from the new Session projection; provider-started Failed persists and proceeds with the old context; no-progress skips silently. Cancellation before or during summary settles the admitted Run as Cancelled, and late cancellation after Applied still starts no next step. A final commits and reports `RunSettled` immediately, with no automatic post-final slot. Manual queue/correction semantics remain as in Phase 2; subagents remain task-local.

Make Session's compaction snapshot fetch only the selected parent chain in a consistent read transaction, reusing an index of those rows for validation rather than rebuilding and linearly scanning payloads per Compaction Entry. Retain the guarded commit. Check the selected-only SQL shape or query plan and below-threshold behaviour with large abandoned branches; avoid elapsed-time assertions or a test-only fetch port. A long selected branch still costs work to measure; defer broader history storage changes until that remaining cost is demonstrated. Do not add a context cache or generic storage port.

**Tests** (`src/agent.rs`, scripted Run; `src/runtime.rs`, controller; `src/session.rs`, SQLite; `src/transcript.rs` and `src/tui.rs`, state)

First red: `agent::tests::threshold_compacts_after_committed_request_before_first_step`.

- Request whose size crosses the threshold triggers before the first step, retains that request exactly, and is admitted once; the same earlier context with a small request does not trigger. Inter-step tool feedback triggers before allocation, never splits code/feedback, and the next provider sees summary plus tail.
- Cancel before attempt: no Compaction Entry but a durable Cancelled Run; after attempt: Cancelled Compaction Entry and Run; after Applied is chosen: Applied remains and no next Model Step. Check settlement and absence of synthetic Run/step IDs. Storage failure disables Runtime and does not publish an uncommitted card.
- Failed threshold provider attempt persists Failed and continues unchanged; NoOp/disabled issues no event, entry or summary call. `RunSettled` follows Final promptly. A mid-Run `/model` changes neither threshold window, summary selection nor the next normal step's selection; the next Run uses it.
- Run-scoped Started sets `compacting context` and spinner independently of manual acknowledgement or draft clearing; delayed lossy `ModelStepStarted` and `ModelProgress` chunks from the preceding step, delivered before a retry reset, cannot switch either back to generating or repaint old text. Settled attempt accounting stays visible. Committed card, terminal settlement or disabled event clears presentation. Submit/Compact/tree are busy during active work; a retained manual request cancels and follows Run/correction settlement once.
- Repeated checks after an Applied entry ignore old normal/summary usage, with no immediate retrigger from that usage. Assert the selected-only SQL/query plan and below-threshold behaviour with large abandoned replay blobs and several Compaction Entries; no abandoned reads, repeated full-chain validation scans or summary-input construction.

**Files** — Modify: `src/agent.rs`, `src/runtime.rs`, `src/session.rs`, `src/compaction.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification**

```sh
for filter in agent::tests runtime::tests session::tests transcript::tests tui::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 5: Classify provider context overflow narrowly

**Changes**

Expose a semantic overflow predicate on shared `ModelFailure`/`ProviderFailure`. Each adapter classifies raw provider fields before redaction or bounding, then retains only the semantic result and existing safe diagnostic. Implement the spec's exact OpenAI code/status/message rules. For Anthropic, verify a positive HTTP 400 `invalid_request_error` prompt-too-long diagnostic from a current provider response before adding a narrow matcher; if it cannot be verified without an expensive live over-window call, keep all Anthropic failures ordinary and record that limitation in README. Never classify HTTP 413 `request_too_large`, output `max_tokens`, signature-binding, local `ContentTooLarge` or summary failures. This task does not yet alter ordinary failed-Run settlement.

**Tests** (`src/model.rs`, predicate; `src/openai.rs` and `src/anthropic.rs`, local fixtures)

First red: `openai::tests::provider_failure_classifies_approved_context_overflow_signatures`.

- OpenAI exact codes (including status absent/500), approved phrases under 400/413, mixed case, and a positive phrase beyond diagnostic bounding or near redacted material. Exclude 401/429 even with a code; code near-misses, phrase without 400/413, generic 400/413, broad token text, auth/rate limiting, transport and local response-size errors.
- Anthropic negatives: generic `invalid_request_error`, byte-size 413, rate limit, overload, signed-thinking prefix mismatch, output `max_tokens` and local content limit. Add the verified positive fixture only if the signature is supported by observed provider evidence; never treat a synthetic fixture as proof of the contract.
- Normal and summary failures preserve safe bounded diagnostics; only the normal-step caller will consult the semantic flag in Task 7.

**Files** — Modify: `src/model.rs`, `src/openai.rs`, `src/anthropic.rs`.

**Verification**

```sh
cargo test --bin kiki model::tests
cargo test --bin kiki openai::tests
cargo test --bin kiki anthropic::tests
```

### Task 6: Persist composable overflow evidence without changing format version

**Changes**

Keep the existing optional ordinary `Generation`/`Execution` evidence wire shape. Add an omitted-by-default optional `overflow_recovery` sibling to v2 Final, Cancelled and Failed terminal payloads, carrying one bounded structured initial diagnostic, optional partial Generation evidence and initial failed-call attempts. A Final continues to own its ordinary replay/result; retried code or terminal owns only retry attempts. Restore and inspect both evidence parts without projecting the initial failure into Model Context or tree-copy content. Keep the existing decoded ordinary attempts for context facts; expose initial-overflow attempts separately for billing on both selected terminal records and off-branch restore totals. The Transcript footer counts ordinary, initial-overflow and summary attempts once without treating initial usage as latest normal context occupancy. No pending-attempt journal, overloaded attempt accessor, or format bump.

**Tests** (`src/session/codec.rs`, literal old-v2 payloads; `src/session.rs`, temporary SQLite; `src/runtime.rs`, live/restored terminal mapping; `src/transcript.rs`, inspection)

First red: `session::codec::tests::old_v2_terminals_and_additive_overflow_evidence_round_trip`.

- Open literal Phase-2-shaped Final/Failed/Cancelled payloads, including existing execution evidence and file effects. New ordinary payloads omit the sibling. New recovered terminals round-trip with immediate failure (`generation = None`) and bounded partial-generation evidence, including a successful Final and a later failed/cancelled execution.
- Same-valued initial and retry attempt vectors count twice for billing (two calls) but each owner exactly once on selected and abandoned branches, live and restored; summary attempts on Compaction Entries count once. Initial known usage with unavailable retry usage changes cost but neither the compaction baseline nor the latest normal footer occupancy. Take a recovered Final through both live and restored Runtime terminal mappings into Transcript: neither drops initial evidence, and inspection renders it separately from the last surrounding failure.

**Files** — Modify: `src/agent.rs`, `src/session.rs`, `src/session/codec.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`, `src/subagents.rs` (terminal construction sites).

**Verification**

```sh
cargo test --bin kiki session::codec::tests
cargo test --bin kiki session::tests
cargo test --bin kiki runtime::tests
cargo test --bin kiki transcript::tests
```

### Task 7: Recover one pending Model Step

**Changes**

Intercept the first recognised normal-step overflow before ordinary terminal closure, retaining one Run-long initial observation outside ordinary attempt slices and clearing the ordinary generation evidence builder immediately after capturing its partial output. Reuse Task 4's reason-neutral Agent summary operation for `ContextOverflow`; on Applied and absent cancellation, send one reliable generation restart with epoch 1 before retry progress and call the model again under the same step ID and captured selection. Tag lossy generation progress with its epoch; after the reliable reset, reject delayed epoch-0 chunks at presentation even if Runtime already forwarded them to the TUI buffer. Reset partial live output/attempt number without clearing settled accounting. Keep a small terminal-sealing path in Agent to attach the observation once after each existing late-cancellation closure chooses Final, Failed or Cancelled, including after later successful code; do not add a Run state machine. Never retry an overflowed summary or recover a second/different retry failure. Failed/NoOp/pre-start-failed recovery uses the spec's exact two-part bounded diagnostic, retaining a safely redacted pre-start error without inventing a Compaction Entry; cancellation and persistence follow existing Run/Session rules.

**Tests** (`src/agent.rs`, scripted sequence; `src/runtime.rs`, events/gates; `src/session.rs`, provenance/restart; `src/transcript.rs` and `src/tui.rs`, progress/inspection)

First red: `agent::tests::context_overflow_compacts_and_retries_same_pending_step_once`.

- One step allocation spans initial failed call, Applied Compaction Entry and successful code/final retry; capture both provider histories and assert retry uses the new canonical summary plus exact retained tail, not the discarded prefix or the first call's snapshot. Settlement entry descends from compaction, and a later final carries the same initial evidence. Use one selection despite accepted mid-Run changes.
- Failed/NoOp/pre-start-failed recovery closes once with exact 2,000-byte UTF-8 component bounds and 4,047-byte total; pre-start failure retains a safely redacted diagnostic but has no Compaction Entry. Partial initial generation followed by retry failure without any progress or `AttemptStarted` preserves the initial output only in overflow evidence, not ordinary retry evidence. Second overflow or different retry error makes no new summary. Disabled/unrecognised failure and summary failure never recover. A failed Compaction Entry retains its separately bounded 4,096-byte diagnostic.
- Esc before provider attempt creates no Compaction outcome; Esc after attempt persists Cancelled; Esc after Applied prevents retry; Esc during retry preserves initial and ordinary retry evidence separately. Error at outcome persistence disables without a false successful card or retry.
- During overflow summarisation, delayed epoch-0 chunks and `ModelStepStarted` cannot replace the compaction spinner or status. Reliable epoch reset precedes retry progress; epoch-0 chunks already buffered at Runtime or forwarded to the TUI cannot repaint epoch 1 even under saturation or reordering across reliable/lossy channels. Settled accounting remains visible; first generation remains epoch 0.
- Every terminal route after overflow carries the initial observation once: immediate retry Final/Failed/Cancelled, execution failure/cancellation, cancellation between steps, later provider failure, duplicate call ID and a later Final after recovered code. Verify billing once on each route. Restart after Applied before retry leaves the Compaction Entry, settles pending step/Run with the generic interruption terminal, and never reconstructs lost in-memory attempts or replays work. Restart after success restores context, branches, attempt totals and terminal inspection. Direct selection of Applied and a sibling branch preserve their own projections.

**Files** — Modify: `src/agent.rs`, `src/runtime.rs`, `src/session.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification**

```sh
for filter in agent::tests runtime::tests session::tests transcript::tests tui::tests; do
  cargo test --bin kiki "$filter" -- --test-threads=1 || exit
done
```

### Task 8: Document the enabled behaviour

**Changes**

Update README to state default `enabled = true`, root-only pre-step triggers including the new request, no pre-admission/post-final work, model selection captured for the Run, disabled/manual behaviour, both providers' threshold support, narrow recognised-overflow recovery and any unverified Anthropic limitation. Preserve Arena, file-path privacy, approximation, branch/tree, session-v2 and no-history-deletion caveats. Remove Phase 2's statement that automation is unavailable.

**Tests** (documentation review)

No first-red test applies. Compare every setting, trigger, status, cancellation, retry limit and provider claim with the preceding behavioural tests.

**Files** — Modify: `README.md`.

**Verification** — Run the whole-phase gates above.
