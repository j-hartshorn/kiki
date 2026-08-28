# Plan: Live OpenAI agent — Phase 1: Live OpenAI agent

## Source

- Spec: `docs/2026-08-27_live-openai-agent/spec.md`, Phase 1

## Acceptance criteria

- With valid startup configuration, Kiki submits TUI requests to OpenAI's standard Responses endpoint using fixed `gpt-5.6-sol`; with missing or invalid startup configuration, it opens a read-only configuration view and cannot submit.
- Every provider step produces exactly one completely validated Python action or final message. Partial, mixed, multiple, malformed, refused, incomplete, unknown, or oversized output never executes.
- Explicit `store: false` history replays completed provider items, encrypted reasoning, and every function-call/result pair in exact order while preserving persistent Monty state across steps and requests.
- Reasoning summaries, assistant text, decoded Python, retry state, and Python stdout use fixed-capacity, non-blocking progress channels into a plain structured transcript. Dropped previews are marked, while the separate reliable path always settles with complete authoritative records.
- Retries occur only for the specified pre-event transient failures, stop after three total attempts, honour bounded `Retry-After`, and stop immediately on cancellation.
- Cancellation settles once, preserves earlier completed work, records bounded interrupted-step evidence, never leaves a dangling function call, restores an interrupted arena checkpoint, and permits a later request after successful recovery.
- Failed checkpoint recovery produces one failed terminal record, retains bounded original and recovery evidence, and disables the affected Agent.
- Provider failures and every dynamic terminal string are bounded and sanitised; API keys and executable terminal controls never appear in records, events, configuration, or rendering.
- Per-step usage and estimated `gpt-5.6-sol/default` cost are inspectable. Unknown or unpriceable attempts make cumulative cost unavailable or a labelled lower bound, never zero or a misleading complete total.
- The deterministic suite requires no OpenAI credentials. A separate ignored live test demonstrates several persistent `read`-backed Sol steps, streaming, final response, cancellation recovery, and a later request.
- OpenAI SDK types remain confined to the OpenAI adapter. Existing Monty, filesystem-confinement, execution-time, memory, observation, ordering, and terminal-restoration guarantees remain intact.

## Overall validation

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l 'cap_std' src --glob '*.rs')" = "src/tools.rs"
```

Expected: every command exits successfully; all ordinary tests pass without `OPENAI_API_KEY`; foreign provider, Monty, and capability-filesystem types remain behind their owning modules.

Opt-in live validation:

```sh
OPENAI_API_KEY=... MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty just live-test
```

Expected: the ignored Sol scenarios complete within their explicit bounds and print no credential or raw provider payload.

## Planning context

- The production composition root constructs `ScriptedModel` directly after fallible worker and arena setup, so missing configuration currently exits before Ratatui starts (`src/main.rs:22-48`).
- `Agent` owns the concrete model, persistent Arena, and model context; it currently invokes the model synchronously and mutates final-message context before runtime commitment (`src/agent.rs:40-118`).
- Current model history is a flat enum without provider call IDs, opaque replay items, usage, or structural call/result pairing (`src/model.rs:1-68`).
- The runtime controller already serialises commands and worker messages and commits records before publishing matching events, but cancellation is only a token and terminal failures are flattened to strings (`src/runtime.rs:118-189`, `src/runtime.rs:234-259`, `src/runtime.rs:376-388`).
- The Arena commits a successful execution and checkpoint before returning; cancellations and infrastructure failures replace the worker and restore the pre-execution checkpoint (`src/arena.rs:216-295`, `src/arena.rs:363-412`).
- stdout accumulators currently live inside the abandoned Monty driver future, while the existing progress path reports only `read` lifecycle (`src/arena.rs:86-95`, `src/arena.rs:454-588`).
- `ReadTool` permits one bounded regular-file read through `spawn_blocking`; dropping its future does not guarantee the underlying blocking read stops (`src/tools.rs:84-153`). Phase 1 therefore tracks only the one current read and does not introduce a general call scheduler.
- Worker and TUI event publication are currently non-blocking and unbounded (`src/runtime.rs:191-226`, `src/tui.rs:295-342`). Phase 1 separates fixed 256-slot ephemeral progress at both hops from the reliable command/record/settlement path; 4 KiB event limits, explicit gap markers, and authoritative terminal replacement bound previews without blocking canonical settlement.
- The TUI currently stores flat committed records plus one status string and renders dynamic text without terminal-control normalisation (`src/tui.rs:54-73`, `src/tui.rs:347-445`).
- Existing tests are inline module tests; the crate has no library target or external integration-test convention (`src/model.rs:89`, `src/agent.rs:215`, `src/arena.rs:1026`, `src/runtime.rs:393`, `src/tui.rs:450`). Keep the opt-in live test inline rather than widening the public interface.
- `async-openai` 0.41.3 is the compatibility-gate candidate. Its crate metadata declares Rust 1.75 support and separate `responses` and `rustls` features; the gate must prove the exact required Responses fields and event coverage on Kiki's pinned Rust 1.98 toolchain before implementation depends on it.
- Existing project verification is format check, all-target check, serialised tests with `MONTY_BIN`, and warning-denying Clippy (`README.md:33-40`, `justfile:1-13`, `rust-toolchain.toml:1-3`).

## Tasks

Each task ends in exactly one atomic `git commit --signoff` using its listed subject, and only after that task's verification passes.

### Task 1: Prove the OpenAI SDK transport

**Changes** — Pin the smallest `async-openai` 0.41.3 feature set which provides Responses over Rustls, plus only directly used support crates. Add the isolated OpenAI module and permanent local HTTP/SSE compatibility fixtures. Prove the exact request fields and event data even where the SDK may require its BYOT or middleware path, custom fixture endpoint injection, zero implicit retries, and cancellation by dropping a foreground stream. Stop for a transport decision if the SDK cannot carry any required field without leaking its types.

**Tests** (`src/openai.rs`, adapter compatibility) — First fail on the absent dependency and module. Then cover the strict closed `execute_python` schema; `stream`, `store`, default service tier, tool choice, disabled parallel calls, medium reasoning, `summary: "auto"`, `context: "all_turns"`, and encrypted-content inclusion; completed/failed/incomplete/error terminal events; reasoning-summary and function-argument deltas; input, cached-input, cache-write, output, and reasoning usage; proof that one transient response produces one SDK request; an abruptly closed stream; and a dropped stream which closes the fixture connection. No test contacts OpenAI.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`; Create: `src/openai.rs`

**Commit** — `Prove OpenAI Responses transport`

**Verification** — `cargo test openai::tests::sdk_compatibility -- --test-threads=1` — all local transport fixtures pass; `cargo check --all-targets` — the selected features compile on Rust 1.98.

### Task 2: Introduce the asynchronous Model port

**Changes** — Replace Agent's concrete synchronous model dependency with the narrow asynchronous Kiki Model port from the spec. The port has one model-step operation over history, cancellation, and progress; bind all model-specific configuration at adapter construction and add no run-reset lifecycle method. Convert `ScriptedModel` to detect a newly appended user request and rewind its own cursor internally without changing deterministic behaviour. Replace flat execution context with invariant-preserving compound completed-code history containing one call identifier, opaque replay payload, and exactly one result; pending calls remain local to the active step.

**Tests** (`src/model.rs`, unit; `src/agent.rs`, orchestration; `src/runtime.rs`, integration) — First fail on an asynchronously delayed scripted action. Then prove internal scripted rewind/exhaustion, complete action delivery, persistent request/message history, structural call/result pairing, duplicate call-ID rejection, code feedback before the next model step, and unchanged multi-request Arena persistence. Assert the Model port exposes no scripted lifecycle hook.

**Files** — Modify: `src/model.rs`, `src/agent.rs`, `src/runtime.rs`

**Commit** — `Add asynchronous model port`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test model::tests -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test agent::tests -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests -- --test-threads=1 && cargo check --all-targets` — the port and scripted implementation pass with existing orchestration and runtime integration.

### Task 3: Add the read-only system prompt

**Changes** — Add the shipped named Markdown prompt and a small resolver for the bundled resource or `KIKI_SYSTEM_PROMPT` path. Record source provenance without retaining secrets. Make unreadable or invalid UTF-8 overrides explicit errors. Include only the persistent-arena, async `read`, focused-output, action-framing, read-only, Python-error, and untrusted-content instructions approved by the spec.

**Tests** (`src/prompt.rs`, unit) — First fail because no prompt resource exists. Then prove bundled resolution, exact override precedence, unreadable and invalid UTF-8 failures, source provenance, and the presence of every required behavioural instruction without coupling assertions to prose formatting.

**Files** — Modify: `src/main.rs`; Create: `src/prompt.rs`, `prompts/read_only_agent.md`

**Commit** — `Add read-only agent prompt`

**Verification** — `cargo test prompt::tests -- --test-threads=1 && cargo fmt --all -- --check` — prompt resolution and resource checks pass.

### Task 4: Describe startup configuration safely

**Changes** — Add the immutable concrete startup snapshot for worker path, workspace root, fixed model, prompt source, and API-key presence, with typed bounded startup problems and derived submission availability. Treat absent, empty, or whitespace-only `OPENAI_API_KEY` as missing. Keep later authentication rejection as a separate runtime-disabled reason. Expose no credential value through the snapshot or diagnostics.

**Tests** (`src/config.rs`, unit) — First fail on missing snapshot types. Then cover ready configuration, each missing/invalid field, prompt override failure, empty-key handling, deterministic problem ordering, bounded remediation, and an adversarial value resembling a credential which must not appear in debug or display data.

**Files** — Modify: `src/main.rs`; Create: `src/config.rs`

**Commit** — `Describe startup configuration`

**Verification** — `cargo test config::tests -- --test-threads=1 && cargo check --all-targets` — all concrete startup states are deterministic and secret-free.

### Task 5: Account for Sol usage and estimated cost

**Changes** — Add the provider-neutral usage and attempt-accounting values plus one versioned `gpt-5.6-sol/default` pricing constant and pure estimator. Derive complete, lower-bound, or unavailable cumulative Run estimates from attempt data. Keep reasoning as an output-token subset and reject inconsistent or unpriceable usage.

**Tests** (`src/pricing.rs`, unit) — First fail without an estimator. Then cover uncached, cached, and cache-write categories; 272,000 and 272,001 input-token thresholds; reasoning non-duplication; inconsistent totals; incomplete breakdowns; non-default returned tier; unknown possibly billed attempts; and known estimates before an unknown attempt.

**Files** — Modify: `src/main.rs`, `src/model.rs`; Create: `src/pricing.rs`

**Commit** — `Estimate Sol usage cost`

**Verification** — `cargo test pricing::tests -- --test-threads=1` — exact decimal estimates and completeness labels match the fixed pricing identity.

### Task 6: Validate complete OpenAI actions and replay

**Changes** — Implement one-attempt `OpenAiModel` behaviour behind the Model port. Build the exact fixed-model request from the resolved prompt and Kiki-owned history, preserving provider output-item order and rendering each compound code entry with one matching function output. Buffer and locally validate only completed responses into `Code` or `Message`; enforce allowed output items and the 16 MiB per-attempt application-content guard. Keep SDK and wire types inside `src/openai.rs`.

**Tests** (`src/openai.rs`, adapter fixtures) — First fail because the compatibility transport cannot produce a Kiki action. Then prove exact first and subsequent request bodies, encrypted reasoning replay, function-call correlation, message and code success, and rejection of mixed, multiple, unknown, malformed, refused, failed, incomplete, EOF-without-terminal, oversized, duplicate-ID, and unexpected output-item responses. Assert no action is returned before terminal validation.

**Files** — Modify: `src/openai.rs`, `src/model.rs`

**Commit** — `Validate OpenAI model actions`

**Verification** — `cargo test openai::tests::action_protocol -- --test-threads=1 && test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"` — valid actions translate exactly and every invalid terminal shape fails closed.

### Task 7: Stream bounded OpenAI progress

**Changes** — Emit provider-neutral attempt, reasoning-summary, assistant-text, decoded-Python, and settled-usage progress from the adapter. Decode split JSON string escapes and Unicode for presentation only. Coalesce ordered deltas before enqueueing: emit the first pending fragment when the 50 ms rate window is open, then at the next producer event after the window, at 4 KiB, or at settlement. The complete buffered response remains authoritative and subject to the 16 MiB guard.

**Tests** (`src/openai.rs`, adapter streaming) — First fail because fixture deltas are invisible. Then cover event order, an immediate first fragment, rate-limited subsequent fragments, 4 KiB flushes, short terminal flushes, split escapes and Unicode, malformed partial JSON, content-budget exhaustion, and proof that streamed prefixes neither enter model history nor execute. Delivery rejection and display gaps belong to Task 11's channel seams and cannot alter the adapter's authoritative result. The 50 ms window is a rate limit checked on the next producer event, not a time-alone background flush.

**Files** — Modify: `src/openai.rs`, `src/model.rs`

**Commit** — `Stream bounded OpenAI progress`

**Verification** — `cargo test openai::tests::streaming -- --test-threads=1` — progress is ordered and bounded while completed action validation remains authoritative.

### Task 8: Add explicit retries and safe provider failures

**Changes** — Disable implicit SDK retry behaviour and implement the specified three-attempt pre-event retry policy with cancellation-aware exponential jitter and `Retry-After` capped at 30 seconds. Classify authentication separately, retain compact attempt summaries and potentially-billed-unknown accounting, and convert terminal provider failures to Kiki diagnostics whose provider message is UTF-8-safely capped at 4 KiB without request bodies, headers, SDK debug output, or credentials. Emit attempt-start progress so retry presentation resets explicitly.

**Tests** (`src/openai.rs`, adapter policy) — First fail on a transient fixture response. Then cover connection failure and HTTP `408`, `409`, `429`, and `5xx`; exactly three attempts; `Retry-After` seconds/date handling and cap; no retry for authentication, permission, validation, refusal, content filter, incomplete output, post-event failure, or cancellation; immediate cancellation during backoff; attempt-start reset signals and fresh presentation/content state; exact 4 KiB UTF-8-safe diagnostic truncation; and an adversarial response containing the configured key before and beyond the truncation boundary.

**Files** — Modify: `src/openai.rs`, `src/model.rs`, `src/pricing.rs`

**Commit** — `Add safe OpenAI retry policy`

**Verification** — `cargo test openai::tests::retry_policy -- --test-threads=1 && cargo test openai::tests::safe_failures -- --test-threads=1` — attempt counts, delays, classifications, accounting state, and redaction match the spec.

### Task 9: Retain interrupted Arena evidence

**Changes** — Move the authoritative stdout/stderr accumulators above the cancellable Monty driver future and let the driver borrow them. Track only the current single `read` path and its settled outcome. Return bounded interruption evidence through cancellation and infrastructure errors while preserving successful execution, Python-error commitment, checkpoint replacement, and recovery behaviour. Add only test-gated read and recovery fault controls needed to stop at deterministic race points; do not add a production call scheduler, a public test interface, or change `ReadTool` limits.

**Tests** (`src/arena.rs`, worker-backed integration; `src/tools.rs`, test adapter) — First fail with `print("before")` followed by cancellation. Then use test-only gates to prove partial stdout survives cancellation, short pending output flushes into evidence, no-read/pending-read/completed-read states are distinguished, a completed read outcome survives later interruption, final observations still use the existing head/tail bounds, successful output is unchanged, checkpoint recovery restores prior state, and original interruption evidence survives deterministic recovery failure.

**Files** — Modify: `src/arena.rs`, `src/tools.rs`

**Commit** — `Retain interrupted arena evidence`

**Verification** — `cargo test tools::tests -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test arena::tests -- --test-threads=1` — the test-only read control, all existing Arena guarantees, and new interruption evidence pass against the exact worker.

### Task 10: Stage terminal model outcomes

**Changes** — Make Agent return a staged final message, provider failure, or interruption instead of admitting terminal model history first. Keep successfully completed code entries committed before the next model step, but hold a valid unexecuted code call locally until it receives one execution, cancellation, or failure result. Provide one synchronous admission operation for the runtime's later terminal decision.

**Tests** (`src/agent.rs`, orchestration; `src/model.rs`, history invariants) — First fail when a completed final message is cancelled before admission. Then prove final messages remain staged, valid Code calls cannot enter history without a result, successful and Python-error executions enter compound history once, execution failure receives one matching result, partial generation never enters history, and earlier completed steps survive a later staged outcome.

**Files** — Modify: `src/model.rs`, `src/agent.rs`

**Commit** — `Stage terminal model outcomes`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test agent::tests -- --test-threads=1 && cargo test model::tests -- --test-threads=1` — staged outcomes preserve every model-history invariant.

### Task 11: Bound runtime progress delivery

**Changes** — Separate ephemeral progress from reliable worker completion, commands, canonical records, and settlement. Use fixed 256-slot progress channels at worker-to-runtime and runtime-to-TUI hops, cap every event at 4 KiB, and publish with non-blocking sends. Each channel sender alone owns its pending display-gap state: the next accepted progress carries it, while an unresolved worker-side gap travels on the reliable finished message and an unresolved TUI-side gap travels on the authoritative terminal event. Do not put delivery state in provider outcomes, model actions, or canonical model history, and do not bound or block canonical runtime history as a side effect.

**Tests** (`src/runtime.rs`, publisher/controller integration; `src/tui.rs`, receiver wiring) — First fail by filling an undrained progress receiver. Then prove fixed capacity, 4 KiB rejection, FIFO accepted events, per-hop gap-marker propagation, unresolved worker gaps on reliable finish, unresolved TUI gaps on terminal events, terminal completion over the independent reliable path, no deadlock during cancellation/shutdown, and settlement across more than 256 progress emissions spanning several model steps while the TUI progress receiver remains undrained.

**Files** — Modify: `src/runtime.rs`, `src/tui.rs`, `src/main.rs`

**Commit** — `Bound runtime progress delivery`

**Verification** — `cargo test runtime::tests::bounded_progress -- --test-threads=1 && cargo test tui::tests::progress_channels -- --test-threads=1` — progress memory is fixed while terminal outcomes remain reliable.

### Task 12: Serialise Run terminal outcomes

**Changes** — Track accepted cancellation in the active Run and choose exactly one `FinalMessage`, `CancelledRun`, or `FailedRun` at the runtime controller's existing serialisation point. Enrich terminal records with optional typed generation/execution evidence and attempt accounting while keeping completed executions separate. Admit the staged final context change and append its terminal record in one synchronous controller turn; terminal records carry any progress-gap marker.

**Tests** (`src/runtime.rs`, controller integration; `src/agent.rs`, orchestration) — First fail on cancellation racing an already queued final message. Then deterministically cover cancellation before any provider event, during generation, after valid message, after valid code but before execution, during execution, after successful Arena completion, during provider/Arena failure, and after terminal commitment; duplicate cancellation; exactly one terminal record; exact record/event order; no dangling call; preserved earlier steps; and a later request after successful recovery.

**Files** — Modify: `src/agent.rs`, `src/model.rs`, `src/runtime.rs`

**Commit** — `Serialise Run terminal outcomes`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests::terminal -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test agent::tests::cancellation -- --test-threads=1` — every race chooses one terminal state and valid subsequent context.

### Task 13: Disable unusable Agents

**Changes** — Add the runtime-disabled state for classified OpenAI authentication failure and every Arena checkpoint-recovery failure. Recovery failure overrides concurrent cancellation, preserves bounded cancellation/original/recovery evidence in `FailedRun`, and permanently removes the uncertain Agent from submission. Use the test-only Arena recovery fault control from Task 9 to exercise the controller path; add no production fault interface.

**Tests** (`src/runtime.rs`, controller integration; `src/arena.rs`, worker recovery) — First fail by submitting after deterministic recovery failure. Then cover authentication disablement until restart, recovery failure after timeout/infrastructure error/cancellation, cancellation arriving during failed recovery, `FailedRun` precedence, one terminal settlement, bounded diagnostics, and rejection of later submissions without reusing the uncertain Arena.

**Files** — Modify: `src/agent.rs`, `src/arena.rs`, `src/runtime.rs`

**Commit** — `Disable unusable Agents`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests::disabled -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test arena::tests::recovery -- --test-threads=1` — unusable Agents cannot accept work and failure precedence is deterministic.

### Task 14: Stream bounded Arena stdout

**Changes** — Extend the existing Arena print accumulator to emit coalesced stdout deltas from the same authoritative source, with UTF-8-safe payloads, one live truncation marker, and no forwarding beyond the 256 KiB / 1,000-line live budget. Emit the first pending fragment immediately when the rate window is open; otherwise flush at the next producer event after 50 ms, at 4 KiB, or at completion/interruption. Carry deltas through Agent and the bounded progress route with run/model-step identity; expose no Monty types and add no timer task or output source.

**Tests** (`src/arena.rs`, accumulator/integration; `src/runtime.rs`, event integration) — First fail because print output appears only at completion. Then cover immediate first emission, rate-limited subsequent fragments, next-event and 4 KiB flushes, short terminal flushes, maximum payload, fragmented Unicode and CRLF, exactly one truncation marker, no post-budget deltas, unchanged final head/tail, stale step identity, a full progress channel with a gap marker, cancellation responsiveness, and terminal settlement with an undrained receiver.

**Files** — Modify: `src/arena.rs`, `src/agent.rs`, `src/runtime.rs`

**Commit** — `Stream bounded Arena stdout`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test arena::tests::stdout -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests::progress -- --test-threads=1` — stdout previews remain bounded without changing authoritative execution data.

### Task 15: Project the plain streaming transcript

**Changes** — Replace the TUI's flat live status with the final private Phase 1 transcript state for current model step, attempts, reasoning, assistant text, decoded code, execution stdout, usage/cost, interruption, display gaps, and settlement. Apply accepted progress in place and replace it with authoritative records. Retain committed order, scroll-from-bottom, stale-run, tiny-terminal, dirty-redraw, and terminal-restoration behaviour. Add one display-only normaliser for all currently available dynamic strings using the exact control escaping from the spec. Render plainly; do not add Phase 2 highlighting, boxes, colours, or collapse controls.

**Tests** (`src/tui.rs`, projection/rendering) — First fail on a reasoning/code/stdout sequence. Then cover chronological updates, retry reset, progress-gap display, same-run stale-step and post-settlement rejection, completed and interrupted terminal replacement, final message below executions, per-step and cumulative complete/lower-bound/unavailable cost rows, follow-at-bottom versus preserved manual scroll, dirty-only redraws, tiny terminals, and terminal restoration. Exhaustively test LF/CRLF, four-space tabs, lone CR, C0/C1, ESC/OSC, and DEL once, plus one hostile projection fixture covering user, provider, source, stdout, error, path, and usage strings. Configuration rendering is tested in Task 16.

**Files** — Modify: `src/tui.rs`, `src/runtime.rs`

**Commit** — `Project plain streaming transcript`

**Verification** — `cargo test tui::tests::transcript -- --test-threads=1 && cargo test tui::tests::normalise -- --test-threads=1 && cargo clippy --all-targets -- -D warnings` — the transcript is inspectable, terminal-safe, and preserves interaction behaviour.

### Task 16: Show startup configuration

**Changes** — Start Ratatui with the immutable startup snapshot even when startup problems prevent Agent construction. Add a configuration-only TUI mode without a runtime channel, show non-secret remediation and the `store: false` privacy boundary, and disable prompt editing/submission. Preserve normal shutdown and terminal restoration in configured and configuration-only modes.

**Tests** (`src/config.rs`, composition; `src/tui.rs`, configuration) — First fail because one representative invalid startup snapshot exits before Ratatui. Reuse Task 4's validation matrix; here prove that invalid composition opens Ratatui, controls remain read-only, no runtime channel is required, privacy wording and hostile configuration text render safely, and shutdown/terminal restoration work in configured and configuration-only modes.

**Files** — Modify: `src/main.rs`, `src/config.rs`, `src/prompt.rs`, `src/tui.rs`

**Commit** — `Show startup configuration`

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test config::tests -- --test-threads=1 && env -u OPENAI_API_KEY cargo test tui::tests::configuration -- --test-threads=1` — missing configuration is a safe read-only TUI state.

### Task 17: Select the production OpenAI model

**Changes** — Replace production `ScriptedModel` composition with `OpenAiModel` using only the fixed model, standard endpoint, resolved prompt, and environment key. Keep any custom endpoint constructor test-only and retain `ScriptedModel` for deterministic tests. Connect runtime authentication disablement to the TUI's submission state, and keep shutdown settlement unchanged.

**Tests** (`src/config.rs`, composition; `src/runtime.rs`, authentication; `src/tui.rs`, disabled state) — First fail because ready composition still selects `ScriptedModel`. Then cover provider construction without exposing the key, standard-endpoint-only production configuration, authentication failure record plus disabled submission until restart, ignored late progress, and clean shutdown during an active provider request.

**Files** — Modify: `src/main.rs`, `src/openai.rs`, `src/runtime.rs`, `src/tui.rs`

**Commit** — `Select OpenAI production model`

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1 && cargo check --all-targets && test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"` — ordinary tests stay credential-free and production composition is provider-safe.

### Task 18: Document and smoke-test the live agent

**Changes** — Add one ignored inline `live_sol` test and a `just live-test` recipe. Give the complete scenario a five-minute outer deadline, cancel and fail the first request after eight model steps, and give each provider wait a two-minute deadline. Require at least two completed Python steps including `read` before a non-empty final message. For the cancellation scenario, wait at most 30 seconds for `AttemptStarted`, cancel generation, require settlement within 30 seconds, then require a later request to complete within two minutes. Always request shutdown and allow ten seconds for cleanup. Document environment setup, fixed model, privacy boundary, controls, configuration states, estimated-cost caveat, deterministic verification, and exact live invocation. Never print credentials or opaque replay payloads.

**Tests** (`src/openai.rs`, ignored live integration; existing deterministic suite) — First fail because no opt-in live path exists. Require `OPENAI_API_KEY` and `MONTY_BIN` only when selected explicitly, with concise non-secret setup guidance. Assert behavioural record/event shape rather than exact model prose. The test-only watchdog may cancel and fail the smoke test but does not add a production model-step limit.

**Files** — Modify: `src/openai.rs`, `README.md`, `justfile`

**Commit** — `Document live OpenAI workflow`

**Verification** — `env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1` — deterministic suite passes without credentials; `OPENAI_API_KEY=... MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty just live-test` — bounded opt-in live demonstration passes and cleans up.
