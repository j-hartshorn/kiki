# Spec: Live OpenAI agent

## Context

Kiki's first vertical slice proves the internal path from TUI input through a scripted model, persistent Monty arena, bounded `read` capability, runtime records, and terminal projection. The binary still constructs `ScriptedModel` directly, so it does not prove provider networking, streamed model output, action framing, reasoning continuation, retry behaviour, authentication, or live-model prompting (`src/main.rs:39`, `src/model.rs:24-59`).

The principal unknown identified by the first-slice specification is whether a real provider can reliably produce complete Kiki actions while partial output remains presentation-only (`docs/2026-08-26_first-vertical-slice/spec.md:138-164`). OpenAI is the first provider named by the Vision (`docs/VISION.md:40-48`).

This change adds a real, read-only OpenAI path using `gpt-5.6-sol` and the Responses API. OpenAI's native `execute_python` function call encodes Kiki's `Code` action; ordinary assistant text encodes its final `Message` action. Completed Python continues to execute in Kiki's persistent arena, where `read(path)` is available as an async function. Partial provider output never becomes executable.

The work is split into two independently demonstrable phases:

1. **Live OpenAI agent** establishes provider correctness, prompting, configuration, stateless reasoning-aware context, retries, cancellation records, usage accounting, and a basic streamed projection.
2. **Inspectable streaming transcript** adds the intended highlighted, paired, collapsible code-and-output presentation without changing provider or execution semantics.

Durable sessions, effectful tools, model selection, multiple providers, interactive authentication, and general theme configuration remain later work.

## Goals

- Replace the binary's scripted model with a live `gpt-5.6-sol` adapter while retaining `ScriptedModel` for deterministic tests.
- Teach the model, through an overridable Markdown prompt resource, to use persistent arena state and async Python capabilities effectively.
- Translate exactly one completed OpenAI `execute_python` call or one completed assistant-text response into Kiki's existing typed `Code` or `Message` action.
- Stream reasoning summaries, Python generation, assistant text, and arena stdout for presentation without allowing partial data to execute or enter authoritative model context.
- Keep Kiki authoritative for model history by replaying explicit local context, including opaque encrypted reasoning and function-call relationships, with OpenAI storage disabled.
- Make authentication, retries, cancellation, usage, estimated cost, and configuration state explicit, bounded, inspectable, and independently testable.
- Present each Python action together with its output in a highlighted, stateful, collapsible arena box, followed by the final assistant response.

## Non-goals

- New arena capabilities beyond the existing read-only `read(path)` function, including `write`, `edit`, `list`, `glob`, `grep`, `run`, skills, plugins, or subagents.
- Providers other than OpenAI, OpenAI-compatible endpoint claims, custom base URLs, model selection, or provider fallback.
- Durable sessions, restart recovery, branch navigation, rewind, snapshot migration, or multiple Monty runtime bundles.
- Interactive authentication, credential storage, OAuth, or editing configuration inside Kiki.
- Automatic context compaction, summarisation, model-step limits, run-cost limits, or user-configurable resource limits.
- Provider retries after any response event has arrived, retries of semantic failures, or background Responses API cancellation.
- Mouse interaction, per-box selection, per-box expansion, configurable themes, or persistent TUI preferences.
- Python variable-change detection, arena inspection, or claiming that filesystem and other external effects can be rolled back.
- Intentionally truncating accepted model output. A response whose counted application content exceeds 16 MiB fails rather than producing a partial action; although this is far above typical 128,000-token output, pathological valid tokenisation could still reach the guard.
- Increasing the existing ten-second Monty deadline, 64 MiB memory limit, or 256 KiB / 1,000-line execution-observation limit.
- Displaying empty stderr sections while Monty cannot produce Python stderr; the underlying typed boundary remains.

## Key discoveries

- `Agent` is currently coupled to a synchronous concrete `ScriptedModel`; the first real provider creates the first justified model interface and asynchronous model-step boundary (`src/agent.rs:40-91`, `src/model.rs:24-59`).
- Kiki already separates typed model actions, Agent sequencing, runtime commitment, and TUI projection. Provider-specific SDK and wire types can therefore remain inside one adapter (`src/model.rs:1-19`, `src/runtime.rs:12-85`).
- Current model context records user requests, completed Python source and feedback, and final messages, but lacks provider output items, action IDs, encrypted reasoning, usage, and cancelled-step outcomes (`src/model.rs:7-12`).
- OpenAI stateless continuation with `store: false` requires replaying every prior output item in order. Reasoning models require `reasoning.encrypted_content`; completed function calls must be paired with matching `function_call_output` items ([OpenAI stateless reasoning guidance](https://developers.openai.com/api/docs/guides/deployment-checklist)).
- `gpt-5.6-sol` supports Responses streaming and function calling, with a 1,050,000-token context window and 128,000 maximum output tokens ([model reference](https://developers.openai.com/api/docs/models/gpt-5.6-sol)).
- OpenAI returns token usage, including cached and cache-write details, but not billed cost. Cost therefore requires a versioned local pricing constant for the fixed model and tier ([usage reference](https://developers.openai.com/api/docs/guides/prompt-caching), [pricing](https://developers.openai.com/api/docs/pricing)).
- `async-openai` provides typed Responses API streaming and function-call events but is unofficial. Keeping it behind a narrow adapter limits dependency and protocol churn.
- Current arena progress reports `read` lifecycle only; Monty print callbacks feed final observation accumulators without publishing stdout deltas. Live results require extending the existing accumulator path rather than adding a second execution source (`src/arena.rs:86-95`, `src/arena.rs:459-588`, `src/arena.rs:662-680`).
- Current cancellation discards an active execution record and restores its pre-execution arena checkpoint. That hides partial output and would hide possible future external effects; the terminal cancellation record needs typed interrupted-step evidence (`src/agent.rs:80-115`, `src/runtime.rs:22-40`).
- Pi's parallel tool execution passes one cancellation signal to every started call, waits for them to settle, retains completed results, converts cooperative cancellation to explicit error results, emits completion events in actual order, and stores tool results in source order (`.research/pi/source/packages/agent/src/agent-loop.ts:489-552`, `.research/pi/source/packages/agent/src/agent-loop.ts:670-710`).
- The TUI currently renders one flat string history. Highlighted paired cards, thinking visibility, expansion state, streamed output, and generic visual states require a structured display projection rather than parsing formatted text (`src/tui.rs:54-73`, `src/tui.rs:347-445`).
- Ratatui already receives runtime events within a bounded 50 ms poll and redraws only on visible changes; provider and arena streaming can reuse this wake path (`src/tui.rs:17`, `src/tui.rs:295-342`).

## Proposed approach

### Model and provider boundary

Introduce one asynchronous Kiki-owned Model port. `ScriptedModel` implements it without streaming; `OpenAiModel` implements it through `async-openai`. Prompt, client, model, endpoint, and credentials are bound when constructing the adapter. The port receives only ordered Kiki model history, cancellation, and a provider-neutral progress sink.

A completed outcome is either `Code`, carrying source, call identifier, opaque replay payload, and optional usage, or `Message`, carrying text, opaque replay payload, and optional usage. Cancelled and failed port errors carry bounded public diagnostics and presentation-only partial content. Terminal wire state, provider identity, retries, and display projection remain implementation details rather than port fields. OpenAI SDK types, credentials, SSE events, and response-item conversion remain inside the OpenAI adapter. The Agent owns model history; the runtime commits Kiki typed records.

A completed `Message` becomes eligible for future model context atomically after terminal response validation. A completed `Code` call and its opaque provider output become context-eligible only together with exactly one associated execution result or cancellation result. No caller can observe or replay a dangling function call.

The production binary uses the standard OpenAI endpoint, `OPENAI_API_KEY`, and the hard-coded `gpt-5.6-sol` model. Other providers, base URLs, and model selection remain outside the interface until a second implementation requires them.

### Action protocol

Every request sets `stream: true`, `store: false`, `service_tier: "default"`, `tool_choice: "auto"`, and `parallel_tool_calls: false`. It requests medium reasoning with `summary: "auto"` and `context: "all_turns"`, and requests `reasoning.encrypted_content` in returned output items.

OpenAI receives one strict native function definition, `execute_python`, whose closed object schema has one required string property, `code`, and `additionalProperties: false`.

A `response.completed` terminal response is valid only when its ordered output contains reasoning items plus exactly one of:

- one completed `execute_python` function-call item and no assistant text, producing `Code`;
- one completed non-empty assistant message and no function-call items, producing `Message`.

Other output-item kinds, multiple calls or messages, mixed text and calls, unknown calls, malformed arguments, refusals, incomplete or failed responses, stream error events, and EOF without a terminal event fail closed. Function argument deltas may be decoded into a presentation-only Python preview, but only the completed locally validated call may execute.

Accepted model output is never truncated. The 16 MiB application-content guard counts accumulated UTF-8 bytes from assistant text, reasoning summaries, function arguments, and opaque encrypted replay content as typed events arrive; HTTP and SSE framing are not counted. Crossing it cancels and fails the model step without producing an action. Each retry has an independent budget.

### Prompt and configuration

Ship the system instructions as a named Markdown resource with one explicit file-path override. Resolve it once at startup and report its source in a typed configuration snapshot.

The prompt teaches:

- persistent arena state across model steps and requests;
- async `read(path)` usage;
- retaining large values in Python and printing focused selections;
- execution feedback and Python-error behaviour;
- `execute_python` versus final assistant text;
- read-only host capabilities;
- untrusted file and output content.

Before constructing the Agent, Kiki builds one immutable startup snapshot containing the Monty worker, workspace root, fixed OpenAI model, prompt source, API-key presence, and typed startup problems with bounded non-secret remediation text. Submission is enabled exactly when the problem list is empty. The prompt override path comes from `KIKI_SYSTEM_PROMPT`; an unreadable or invalid UTF-8 override is a startup problem rather than silently falling back to the shipped prompt.

Startup problems open a read-only TUI configuration view and disable submission. A later OpenAI authentication response is a separate runtime-disabled reason until restart; it does not mutate the startup snapshot. Kiki commits a bounded sanitised failure. Public provider failures may retain HTTP status, provider error code, and request ID plus at most 4 KiB of provider message; they never retain request bodies, headers, SDK debug output, or credential values. Tests inject the configured key into hostile error fields and prove it is absent from configuration, records, events, and rendering.

The configuration view states that `store: false` disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.

### Explicit model context

Kiki remains authoritative for context and sends `store: false`. Model history contains user requests, final messages, and compound completed-code entries. One completed-code entry structurally contains its canonical source, call identifier, provider-tagged opaque replay items, and exactly one execution, cancellation, or failure result. A pending call exists only as local active-step state, so admitted history cannot contain a dangling call.

The OpenAI adapter requests encrypted reasoning in response output, preserves all completed output items in provider order, and renders each compound code entry as those items followed by its matching `function_call_output`. Opaque continuation data remains local and hidden from Monty and the display. Canonical `CancelledRun` and `FailedRun` records remain runtime history; only protocol-required code-call cancellation or failure results enter OpenAI model history. A future provider switch may discard incompatible opaque data while retaining canonical visible history.

Context admission is atomic at the model-step boundary. Partial generation and failed provider attempts never enter model history. A completed function call remains local until execution settles, then its compound entry is appended once. Fixture tests assert exact subsequent request-item order; construction prevents dangling outputs, while provider call identifiers are still checked for duplication.

### Streaming and cancellation

Provider reasoning summaries, assistant text, Python previews, retry state, and usage flow as ordered, coalesced deltas through the runtime route, identified by run and model-step. Ephemeral progress uses fixed 256-slot channels from worker to runtime and runtime to TUI, separate from the reliable command, canonical-record, and settlement path. Every progress event is at most 4 KiB and producers use non-blocking sends. If a progress channel is full, the producer drops that presentation-only delta and marks the step as having a display gap; the next retained update or authoritative terminal record exposes the gap. Retained progress remains FIFO. An attempt-start event clears prior retry presentation, and the terminal typed record replaces live state with complete authoritative content. Existing run and step identity rejects late events after settlement.

The provider adapter keeps the complete accepted response in its own 16 MiB per-attempt authoritative buffer. Its presentation batcher emits a first pending fragment immediately when the 50 ms rate window is open, then emits at the next producer event after the window, when 4 KiB accumulates, or at terminal settlement; 50 ms is an emission rate limit, not a background timer guarantee. Arena print fragments extend the same caller-owned bounded observation accumulator used for the final execution and follow the same batching rule. The Monty driver only borrows this accumulator, so cancellation can finalise its partial contents after abandoning the driver future. After the 256 KiB / 1,000-line live observation budget is reached, Arena forwarding pauses with a truncation indicator and the terminal record supplies the authoritative bounded head and tail. A stalled TUI can lose explicitly marked live previews but cannot grow an unbounded progress queue, delay terminal settlement, or lose canonical results.

The runtime serialises cancellation commands and worker outcomes. Appending exactly one terminal `FinalMessage`, `CancelledRun`, or `FailedRun` record is the Run terminal linearisation point. A cancellation accepted before that point marks the Run cancellation-requested and wins over any pending final message or provider failure; a cancellation received after a terminal record is a no-op. The worker returns proposed final model-context changes rather than admitting them first, so the chosen terminal context entry and terminal record become visible together.

Acceptance of cancellation is also the linearisation point for provider generation, retry waits, and an execution which has not yet completed. Successful Arena completion is the execution commit point: if the Arena has already returned its final state, that execution remains committed and cancellation prevents the next model step instead of retroactively undoing it.

The cases are:

- before any provider event, the request and retry wait stop, no model-step content record is added, and the Run is cancelled;
- during generation, local streaming stops and bounded partial text, reasoning summary, or decoded code becomes display-only generation evidence inside `CancelledRun`; no provider output enters future model context;
- after a valid `Message` but before terminal commitment, the complete text is retained only as generation evidence inside `CancelledRun` and is not admitted as a final model-context message;
- after a valid `Code` call but before execution, Kiki places a result stating that execution did not begin in the compound model-history entry and places the same evidence inside `CancelledRun`;
- during Python execution, Kiki cancels the single possible in-flight `read`, retains its outcome if it settles, abandons the Monty execution, restores its pre-execution arena checkpoint, and places the matching cancelled result in compound model history and `CancelledRun`;
- after successful Arena completion, Kiki atomically admits the completed action/result pair even if cancellation arrives before the next model request, then cancels the Run;
- after a provider or non-cancellation Arena failure but before terminal commitment, an accepted cancellation chooses `CancelledRun` with bounded failed-step evidence rather than appending `FailedRun`;
- after `FinalMessage` or `FailedRun` commitment, a later cancellation changes nothing;
- earlier completed model steps remain committed in every case.

`CancelledRun` carries an optional typed generation or execution interruption. Execution evidence includes source, bounded partial stdout, the current `read` outcome when one completed, whether execution began, and this warning: `Execution was cancelled and its Python arena changes were discarded. External side effects completed before cancellation may remain even though pending actions were cancelled.` Its model-visible cancellation result carries the same bounded evidence. Earlier completed executions remain separate canonical records; no second cancellation record is added.

A non-cancellation Arena failure after a valid `Code` call places exactly one bounded failure `function_call_output` in compound model history and matching optional failed-step evidence inside `FailedRun`, recording whether checkpoint recovery succeeded. It never leaves a dangling call identifier or claims that uncertain state committed. Normal Python exceptions remain completed execution results under the existing commit policy.

Any checkpoint-recovery failure overrides a concurrent or initiating cancellation, regardless of whether recovery began after cancellation, timeout, worker failure, or another infrastructure error. Kiki appends `FailedRun` with interrupted-execution evidence marked `arena recovery failed`, disables further submission for that Agent, and preserves cancellation plus original and recovery errors in bounded diagnostics. The arena is never reused in an uncertain state. A race fixture covers cancellation arriving during failed recovery.

### Retry and provider errors

Disable implicit SDK retries. Each model step permits three total attempts, numbered from one in live progress and diagnostics. Retry only connection failures and HTTP `408`, `409`, `429`, or `5xx` responses before any provider event arrives. Backoff is exponential, jittered, cancellation-aware, and honours `Retry-After` up to 30 seconds.

Authentication, validation, refusal, content-filter, incomplete-response, post-event stream, and cancellation outcomes never retry. A retry begins with empty partial presentation state and a fresh 16 MiB budget. Sanitised terminal failures become inspectable failed-run records; compact bounded summaries of earlier attempts stay inside the settled model-step record rather than model history. Any attempt without returned usage may still have been billed and makes the Run cost a lower bound.

### Usage and pricing

Request `service_tier: "default"` and record the returned tier. Record total input, cached-input, cache-write, output, reasoning, and total token counts for each response that supplies usage. Reasoning tokens are an informational subset of output tokens and are never charged twice.

One versioned `gpt-5.6-sol/default` pricing constant records its effective identity and official source. A pure estimator uses it. For at most 272,000 input tokens, per-million rates are $4.00 uncached input, $0.40 cached input, $5.00 cache writes, and $20.00 output. Above 272,000 input tokens, the full request uses $8.00, $0.80, $10.00, and $30.00 respectively. Uncached input is total input minus cached and cache-write tokens; internally inconsistent usage fails accounting rather than producing a cost. Estimated cost is the sum of each token category multiplied by its applicable rate and divided by one million.

Phase 1 shows per-step usage, estimated cost, and the cumulative known Run cost as plain rows. Phase 2 moves the same data into completed boxes. Any potentially billable attempt with absent usage, internally inconsistent usage, an incomplete category breakdown, or a returned non-default tier is unpriceable. If other attempts have known estimates, the cumulative figure is labelled a lower bound; if none do, cost is unavailable. It is never calculated with default-tier rates for an unknown tier or presented as zero. Fixture tests cover cache categories, exactly 272,000 versus 272,001 input tokens, reasoning non-duplication, returned-tier mismatch, inconsistent breakdowns, and unknown interrupted attempts.

### Transcript presentation

Use a structured display projection rather than parsing formatted record text.

Each Python action appears in one arena box containing, in order:

1. complete Python source;
2. stdout when non-empty;
3. final expression result or Python error;
4. cancellation warning when applicable.

No empty stderr section is shown. The final assistant response always appears below preceding arena boxes.

Arena boxes map to three visual states:

- in progress: muted blue;
- complete: muted green;
- error or cancellation: muted red.

In collapsed mode, short boxes remain fully visible. Longer boxes show up to 12 code lines and 20 output lines. One keyboard shortcut toggles all arena boxes between collapsed and expanded presentation.

Python uses Tree-sitter highlighting, including while partial code streams. Plain code is the accepted fallback only if Tree-sitter cannot build on the pinned toolchain or safely render the incomplete valid UTF-8 streaming fixtures.

Reasoning summaries appear in a separate subtly grey box with horizontal padding and subdued text. They have an independent show/hide keyboard toggle and do not use lifecycle colours.

Live stdout extends the same bounded observation which becomes the final execution record. The existing ten-second execution deadline, 64 MiB memory limit, and 256 KiB / 1,000-line retained observation remain unchanged. There is no model-step limit.

Every dynamic display string passes through one display-only terminal normaliser before reaching Ratatui, including user requests, provider text, reasoning summaries, Python source, stdout, results, errors, paths, usage labels, and configuration diagnostics. It treats LF and CRLF as structural line breaks, expands each tab to four spaces, and renders lone CR plus every remaining C0, C1, escape, OSC, and delete control as uppercase ASCII `\\xNN` notation, for example ESC as `\\x1B`. Raw canonical content and model context remain unchanged. Hostile control-sequence fixtures cover every display source.

## Key decisions and trade-offs

- Use OpenAI Responses native function calling for `Code`; reject whole-response JSON and tagged Markdown as less natural or less robust.
- Keep `execute_python` as provider wire encoding, not a Kiki Tool; the Agent still receives `Code` or `Message`.
- Use `async-openai` behind one adapter; reject custom SSE handling until the SDK proves insufficient.
- Target only OpenAI's standard endpoint; compatible-provider claims wait for concrete testing.
- Hard-code `gpt-5.6-sol`; model configuration waits for the future TUI selector.
- Read `OPENAI_API_KEY` from the environment; reject coupling to Pi credentials or adding OAuth now.
- Use explicit Kiki-owned context with `store: false`; reject `previous_response_id` as canonical state.
- Retain opaque encrypted reasoning items locally so stateless reasoning can continue correctly.
- Ship one Markdown system prompt with a path override; reject a full resource registry before durable sessions and plugins.
- Show missing configuration inside Kiki rather than exiting or waiting for a failed submission.
- Stream decoded Python previews for the sole known function schema; the final complete parse remains authoritative.
- Stream and retain valid model output without truncation; use 16 MiB only as a fail-closed transport guard.
- Give reasoning its own subdued display and visibility toggle rather than treating it as an execution-state box.
- Use only three generic arena-box states: in progress, complete, and error.
- Collapse arena boxes only when their code or output exceeds fixed preview limits; reject terminal-relative limits as unpredictable.
- Pair every Python action with its stdout and result; keep final assistant text below all arena activity.
- Preserve earlier completed model steps when cancelling later work; reject whole-Run rollback.
- Keep partial cancelled generation or execution evidence inside the one canonical in-memory `CancelledRun` record, including pre-cancellation output and an external-side-effect warning; durable persistence remains deferred.
- Restore only the active execution's arena checkpoint; do not imply that external effects were reversed.
- Retry only explicit provider-transient failures before any event, for at most three attempts; reject implicit SDK retries.
- Force OpenAI's default service tier and estimate cost with one versioned model/tier pricing constant because OpenAI does not return billed cost; other models and tiers wait for configuration work.
- Keep Monty's existing execution, memory, and output limits unchanged.
- Allow unbounded model steps for autonomous work; record runaway cost and context growth as an accepted risk.
- Use deterministic local HTTP/SSE fixtures plus an opt-in live smoke test; never require credentials in the ordinary suite.
- Deliver two vertical phases rather than combining provider protocol and rich transcript presentation in one review.

## Risks and unknowns

- `async-openai` is unofficial and its exact support for Sol's latest Responses events, strict functions, encrypted reasoning, and stateless output-item replay must be proven with an early spike.
- Retrying a request before receiving an event can still duplicate provider work or billing if the server accepted the request but the response was lost.
- Foreground Responses streams have no confirmed server-side cancellation operation. Dropping the stream guarantees only that Kiki ignores further output.
- Function-call argument deltas are JSON fragments. The incremental Python decoder must handle split escapes and Unicode correctly, though any defect remains presentation-only.
- Capturing partial stdout after cancellation requires the observation accumulator to outlive the abandoned Monty future without weakening checkpoint recovery.
- The current single in-flight `read` can settle around cancellation; its completed outcome must remain inspectable. General concurrent-effect settlement waits for concurrent or effectful tools.
- Coalescing bounds event frequency and retained content, but a single already-deserialised SDK event can allocate before Kiki applies its 16 MiB application-content guard.
- No model-step or aggregate context limit means a model can accumulate substantial cost and memory before OpenAI rejects the context or the user cancels.
- Opaque encrypted reasoning and provider output items complicate future model switching. Only canonical visible context is portable.
- The local pricing constant can become stale or disagree with promotional or account-specific billing. Every cost remains labelled estimated and carries a pricing identity.
- Prompt wording is a behavioural dependency. Deterministic tests can verify construction and protocol rules, but only an opt-in live evaluation can show that Sol uses persistent arena state and inner Python tools effectively.
- Tree-sitter highlighting remains required unless it cannot build on the pinned Rust toolchain or safely render the incomplete valid UTF-8 fixtures used by streaming tests; those concrete failures permit the approved plain-code fallback.
- The concrete startup snapshot may need replacement when another provider arrives; this slice deliberately avoids a provider registry.
- Monty still cannot produce Python stderr in this version; the typed stderr boundary remains unproven with worker-backed Python.

## Dependencies

- The completed first vertical slice and its runtime, arena, observation, cancellation, and TUI boundaries: `docs/2026-08-26_first-vertical-slice/spec.md`.
- Product direction for provider-neutral models, Prompt Resources, persistent arenas, context/display separation, and self-hosting: `docs/VISION.md`.
- Existing architectural lessons from Pi: `docs/research/2026-08-25_pi-architectural-lessons.md`.
- Existing Monty constraints and checkpoint behaviour: `docs/research/2026-08-26_monty-feasibility.md`.
- OpenAI Responses API streaming, function calling, encrypted reasoning continuation, usage, and error contracts: [OpenAI API documentation](https://developers.openai.com/api/docs).
- Current Sol capabilities and limits: [GPT-5.6 Sol model reference](https://developers.openai.com/api/docs/models/gpt-5.6-sol).
- Current Sol token pricing and long-context threshold: [OpenAI pricing](https://developers.openai.com/api/docs/pricing).
- `async-openai` with only the Responses and Rustls features needed by the adapter.
- Serde JSON support for strict completed-action validation and opaque provider-item retention.
- Tree-sitter highlighting and the Python grammar for Phase 2, subject to the approved plain-text fallback.
- Existing Tokio, cancellation-token, Ratatui, and exact Monty `0.0.21` dependencies.
- `OPENAI_API_KEY` only for the opt-in live test and demonstration; ordinary tests use local deterministic fixtures.
- The exact `monty-runtime 0.0.21` worker configured by the existing `MONTY_BIN` development setting.

## Phase outline

### Phase 1: Live OpenAI agent

Begin with an `async-openai` compatibility gate. It must compile on the pinned toolchain and prove, against local fixtures, strict Responses function requests, every required stream terminal outcome, encrypted reasoning retention/replay, reasoning-summary deltas, usage fields, disabled implicit retries, and cancellation by dropping a foreground stream. These fixtures become the permanent OpenAI adapter suite, not a throwaway spike. If the SDK cannot meet any requirement without leaking its types past the adapter, stop for a transport decision rather than silently replacing it.

Then deliver a complete read-only live-model path:

- asynchronous provider-neutral model boundary;
- `OpenAiModel` using `async-openai`;
- fixed `gpt-5.6-sol`, standard endpoint, and environment API key;
- Markdown prompt resource and explicit override;
- immutable startup snapshot, runtime-disabled authentication state, and read-only configuration TUI;
- native `execute_python` action encoding and fail-closed validation;
- explicit `store: false` context with encrypted reasoning replay;
- three-attempt provider-specific retry policy;
- cancellation-wins semantics;
- typed optional generation or execution interruption evidence inside terminal `CancelledRun` and `FailedRun` records;
- streamed reasoning, assistant text, decoded Python, arena stdout, usage, and settlement in the final private TUI transcript state, rendered plainly;
- per-step usage and versioned estimated cost data;
- deterministic HTTP/SSE fixtures and an opt-in live Sol demonstration.

Verification stays at module interfaces: adapter fixtures cover wire protocol and retries; Agent/runtime tests cover atomic history admission and cancellation races; one exhaustive normaliser suite plus one projection fixture covers terminal safety; one adversarial provider-error path proves API-key absence across configuration, records, events, and rendering.

**Demonstration:** ask Sol to inspect Kiki's cancellation recovery. It performs several persistent Python steps using `read`, streams progress, and returns a grounded final answer. Cancelling generation or execution leaves inspectable partial records and Kiki accepts a later request.

### Phase 2: Inspectable streaming transcript

Retain Phase 1's private transcript state and runtime records, replacing only its plain rendering with the intended structured presentation:

- paired Python-and-output arena boxes;
- stdout before expression result or Python error;
- final assistant response below all arena boxes;
- Tree-sitter Python highlighting during and after generation;
- muted blue, green, and red generic arena states;
- collapsed mode with 12 code lines and 20 output lines;
- automatic collapse only after a section crosses its limit;
- global expand/collapse-arena shortcut;
- subdued padded reasoning boxes;
- independent show/hide-thinking shortcut;
- per-step token/cost presentation and cumulative Run total;
- cancelled cards showing partial content and the external-side-effect warning;
- the existing scroll rule: follow new content while at the bottom, otherwise preserve `scroll_from_bottom`;
- stale-event rejection, tiny-terminal rendering, and dirty-only redraws.

**Demonstration:** the same live request produces a chronological, highlighted transcript whose active cards update in place, settle into their final visual states, and remain fully inspectable within the established retention bounds.
