# Research: Maki Anthropic provider integration

## Scope

This report looks at the Maki source snapshot at commit `e0d87e35a45a313b8c3e03dca21ae5aef3d35050`, vendored at `.research/maki/source`; all citations are relative to that directory. It covers the built-in Anthropic Messages API path plus the shared history, agent, UI, pricing and provider-selection code that affects it. (`maki-providers/src/providers/anthropic/mod.rs:384-431`; `maki-providers/src/provider.rs:254-329`)

## Summary

- Anthropic thinking configuration depends on the model version. Newer Claude IDs use adaptive thinking with summarised display; older IDs use adaptive or token-budget request fields. (`maki-providers/src/types.rs:403-405,615-649`)
- History uses shared message types and is kept when the selected model changes, but each provider adapter serialises thinking differently. (`maki-providers/src/types.rs:141-176,212-220`; `maki-agent/src/headless.rs:374-375,399-465`)
- Prompt-cache breakpoints are placed in code and use the `ephemeral` type. None of the cache-control or provider configuration types examined has a TTL field. (`maki-providers/src/providers/anthropic/mod.rs:1-2`; `maki-providers/src/providers/anthropic/shared.rs:49-58,168-227`; `maki-config/src/providers.rs:167-183`)
- Anthropic usage is recorded as input, output, cache-creation and cache-read tokens. Model pricing is applied to those counters, using static rates that discovered pricing metadata can override. (`maki-providers/src/providers/anthropic/shared.rs:50-78`; `maki-providers/src/model.rs:537-624,263-279`)
- The direct provider uses `isahc` for HTTP and parses SSE lines in Maki's own code. Retryable API errors include 429 and 5xx; the explicit retry cap only applies to timeouts. (`maki-providers/src/providers/anthropic/mod.rs:11-13,496-526`; `maki-providers/src/error.rs:34-51`; `maki-agent/src/agent/streaming.rs:124-148`)

## Findings

### Thinking controls and display

The shared thinking setting supports `Off`, `Adaptive`, effort levels and explicit token budgets. Maki picks the adaptive API path by parsing the Claude model ID: Opus from 4.7 and other Claude families from 5 use it. (`maki-providers/src/types.rs:403-435,615-649`)

For those adaptive models, `Off` leaves the thinking fields out of the request. Every other setting sends `thinking: {type: "adaptive", display: "summarized"}`, and Maki adds `output_config.effort` where it applies. Maki's Anthropic effort dialect accepts only `low`, `medium` and `high`. Unsupported Maki levels snap to the nearest lower supported level, or to the lowest one. (`maki-providers/src/types.rs:533-538,580-638`; `maki-storage/src/sessions.rs:313-357`)

On an adaptive model, a budget is converted to an effort level based on the model's maximum thinking budget, and is never sent as `budget_tokens`. On older model IDs, adaptive mode sends `thinking: {type: "adaptive"}`, effort settings are converted to token budgets, and explicit budgets are clamped when the model's output limit is known. Maki sets the maximum thinking budget to half the output-token limit, with a floor. (`maki-providers/src/types.rs:580-639`; `maki-providers/src/model.rs:369-376`; `maki-storage/src/sessions.rs:313-340`)

Anthropic `thinking_delta` events become shared `ThinkingDelta` events, which the chat UI passes to its message panel. The UI turns saved thinking blocks into separate thinking display messages, and its `show_thinking` setting controls whether they start collapsed. (`maki-providers/src/providers/anthropic/shared.rs:308-336`; `maki-ui/src/chat.rs:81-84,420-431`; `maki-ui/src/components/messages/mod.rs:86-87,133-135,168-170`)

### Thinking-block replay and cross-provider history

The shared content model has `Thinking` blocks with an optional signature, `RedactedThinking` blocks, tool-use blocks and tool-result blocks. The Anthropic parser stores thinking and redacted-thinking blocks, appends signature deltas to the thinking block, and serialises content blocks through a flattened shared representation. (`maki-providers/src/types.rs:141-176`; `maki-providers/src/providers/anthropic/shared.rs:94-116,147-160,273-295,308-336`)

For a response with no tool call, the agent only adds it to history if it contains text; a thinking-only response goes through stalled-turn recovery instead. In the tool-call path, both the assistant response and the tool results that follow are added to history. (`maki-agent/src/agent/run.rs:349-353,434-449`; `maki-agent/src/agent/tool_dispatch.rs:382-398,464-468`)

Changing model doesn't convert the history by itself. The headless agent keeps one `History` while it swaps the provider and model; loading a session selects the stored model and respawns the agent with the loaded messages. (`maki-agent/src/headless.rs:374-375,399-465`; `maki-ui/src/event_loop.rs:1306-1319,1396-1412`)

Provider adapters do transform or leave out some blocks:

- The OpenAI-compatible chat adapter serialises shared thinking as `reasoning_content` and leaves out redacted-thinking blocks. (`maki-providers/src/providers/openai_compat.rs:327-367`)
- The OpenAI Responses adapter ignores thinking and redacted-thinking blocks when building input items. Its streamed reasoning text becomes a shared `Thinking` block with no signature. (`maki-providers/src/providers/openai/responses.rs:39-114,404-424,486-492`)
- Tool IDs pass through these conversions as `id`, `tool_call_id` or `call_id`. The agent's canonicalisation code rewrites a prefixed tool name, not the ID. (`maki-providers/src/providers/openai_compat.rs:297-305,338-348`; `maki-providers/src/providers/openai/responses.rs:106-112`; `maki-agent/src/agent/streaming.rs:15-25`)

So common history can be reused across providers, but reasoning blocks are not replayed the same way by every adapter. The source shows where blocks are kept or left out; it doesn't show whether every remote provider will accept them.

### Prompt caching

The Anthropic provider documents four intended breakpoints: the system prompt, the last tool definition, and the last eligible block in each of the two most recent messages. A message made up only of thinking blocks gets no breakpoint. (`maki-providers/src/providers/anthropic/mod.rs:1-2`; `maki-providers/src/providers/anthropic/shared.rs:168-227`)

A custom system prefix is sent without a breakpoint, while the actual system prompt gets an `ephemeral` breakpoint. The last tool definition gets the same marker. (`maki-providers/src/providers/anthropic/mod.rs:396-423`; `maki-providers/src/providers/anthropic/shared.rs:228-239`)

The cache-control type has only a `type` field, set to `ephemeral`, and the provider configuration fields shown have no cache TTL setting. (`maki-providers/src/providers/anthropic/shared.rs:49-58`; `maki-config/src/providers.rs:167-183`)

### Usage and pricing

Anthropic's response usage fields map to four counters: non-cached input, output, cache creation and cache reads. Maki's total input is the sum of the three input counters, and context tokens add output on top. This usage structure has no separate thinking-token counter. (`maki-providers/src/providers/anthropic/shared.rs:50-78`; `maki-providers/src/model.rs:537-581`)

The static Anthropic table holds per-million-token rates: the cost calculation divides each counter's charge by one million, and the UI formats a known cost with a dollar sign. Values below are in the order **input / output / cache write / cache read**. (`maki-providers/src/model.rs:22,607-624`)

| Model entry | Static rates | Source |
|---|---:|---|
| Haiku 4.5 | 1 / 5 / 1.25 / 0.10 | `maki-providers/src/providers/anthropic/shared.rs:409-422` |
| Sonnet 4, 4.5, and 4.6 | 3 / 15 / 3.75 / 0.30 | `maki-providers/src/providers/anthropic/shared.rs:425-487` |
| Sonnet 5 | 2 / 10 / 2.50 / 0.20 | `maki-providers/src/providers/anthropic/shared.rs:457-471` |
| Opus 4.5–4.7 | 5 / 25 / 6.25 / 0.50 | `maki-providers/src/providers/anthropic/shared.rs:490-537` |
| Opus 4.8 and 5 | 5 / 25 / 6.25 / 0.50; fast input/output 10 / 50 | `maki-providers/src/providers/anthropic/shared.rs:540-575` |
| Fable 5 | 10 / 50 / 12.50 / 1.00 | `maki-providers/src/providers/anthropic/shared.rs:578-591` |
| Opus 4.0 and 4.1 | 15 / 75 / 18.75 / 1.50 | `maki-providers/src/providers/anthropic/shared.rs:594-607` |

Fast-mode cache rates are derived from the fast input rate: 1.25× for cache writes and 0.10× for cache reads. Maki only sends `speed: "fast"` when it's requested and the model/provider combination supports it. (`maki-providers/src/model.rs:107-109,379-385,607-624`; `maki-providers/src/providers/anthropic/mod.rs:48-56`)

When discovered pricing metadata exists, it takes precedence over the static model-table pricing. (`maki-providers/src/model.rs:263-279`)

The provider also fetches Anthropic account usage, separately from per-turn token costs. It calls `/api/oauth/usage` with the OAuth beta header, but only when the resolved auth contains an `Authorization` header and the target is the first-party API or a matching configured override. (`maki-providers/src/providers/anthropic/mod.rs:30-32,220-239,463-475`)

### Request construction and authentication

The request body contains `max_tokens`, system blocks, messages, tools and any thinking fields added by the shared builder. The Anthropic provider then sets the model ID and `stream: true`, stripping a `-1m` suffix before sending. (`maki-providers/src/providers/anthropic/shared.rs:239-269`; `maki-providers/src/providers/anthropic/mod.rs:417-428`; `maki-providers/src/providers/anthropic/shared.rs:20-30`)

Requests send `anthropic-version: 2023-06-01`, a user-agent and a JSON content type. Fast mode and long-context mode each add their own value to `anthropic-beta`. (`maki-providers/src/providers/anthropic/mod.rs:25-32,299-333`)

The built-in provider reads `ANTHROPIC_API_KEY` and sends it in `x-api-key`. Key resolution checks that environment variable, then saved provider credentials, then the `api_key` value in `providers.toml`. If none is found, construction returns a configuration error with a login instruction. (`maki-providers/src/providers/anthropic/mod.rs:37-45,243-270`; `maki-providers/src/providers/mod.rs:204-232`)

A shared `ResolvedAuth` can also apply arbitrary headers and has a bearer-token helper. Custom Anthropic-protocol providers reuse the same `Anthropic` implementation with their own auth. (`maki-providers/src/providers/mod.rs:69-80`; `maki-providers/src/providers/custom.rs:76-82`)

### HTTP, SSE, errors and retries

The provider crate depends on `isahc` and `futures-lite`. The Anthropic path uses an `isahc` client and reads SSE through a line reader. (`maki-providers/Cargo.toml:16-18`; `maki-providers/src/providers/anthropic/mod.rs:11-13,496-526`)

The parser recognises `event:` and `data:` lines and hands event data to an `EventParser`. That parser handles message start, content-block start/delta/stop, message delta, error and message stop events. Thinking and text deltas are forwarded, and usage and stop reasons from message deltas are recorded. (`maki-providers/src/providers/anthropic/mod.rs:496-526`; `maki-providers/src/providers/anthropic/shared.rs:254-386`)

Anthropic SSE error types map to internal statuses, including overloaded errors to 529 and rate-limit errors to 429. Non-200 HTTP responses keep the HTTP status and response body as an API error. (`maki-providers/src/providers/mod.rs:127-145`; `maki-providers/src/providers/anthropic/mod.rs:336-343`; `maki-providers/src/error.rs:118-124`)

The agent retries 429 and 5xx API errors, plus I/O, HTTP transport and timeout errors. For retryable errors that ask for it, it rotates the key, and the Anthropic key pool can rotate when it holds several keys. Timeout retries stop after the configured limit of 10, but this loop puts no such limit on other retryable errors. (`maki-providers/src/error.rs:34-51,92-94`; `maki-agent/src/agent/streaming.rs:124-148`; `maki-providers/src/retry.rs:1-23`; `maki-providers/src/providers/mod.rs:237-253`)

Retry delays grow linearly from a two-second base, cap at eight seconds, and add jitter between half the capped delay and the full delay. The HTTP error conversion reads the status and body, not a `Retry-After` header. (`maki-providers/src/retry.rs:1-23`; `maki-providers/src/error.rs:118-124`)

### Architecture and source map

`Model::from_spec` splits a `provider/model` string, `provider_for_slug` resolves the provider, and `from_model` returns a `Box<dyn Provider>`. The Anthropic slug picks the direct Anthropic provider unless Bedrock is enabled. (`maki-providers/src/model.rs:490-504`; `maki-providers/src/provider.rs:39-55,225-232,254-329`)

For the direct path:

- `maki-providers/src/providers/anthropic/mod.rs`: HTTP requests and auth handling.
- `maki-providers/src/providers/anthropic/shared.rs`: request-body construction, cache placement and SSE event parsing.
- `maki-agent/src/agent/streaming.rs`: retry behaviour.
- `maki-ui/src/chat.rs` and `maki-ui/src/components/messages/mod.rs`: thinking display.

(`maki-providers/src/providers/anthropic/mod.rs:299-333,384-431,496-526`; `maki-providers/src/providers/anthropic/shared.rs:168-269`; `maki-agent/src/agent/streaming.rs:87-148`; `maki-ui/src/chat.rs:79-85`; `maki-ui/src/components/messages/mod.rs:86-87,168-170`)

## Gaps

- This is a source-level account. It doesn't show how Anthropic's service behaves today, whether it accepts replayed cross-provider thinking/signature blocks, or whether the snapshot's static rates match current billing. There is no live API response or billing result in the code examined. (`maki-providers/src/providers/anthropic/shared.rs:147-160,273-295`; `maki-providers/src/providers/anthropic/shared.rs:406-607`)
- The cache representation and provider configuration examined have no TTL field, so this report can't state a cache lifetime from the source. (`maki-providers/src/providers/anthropic/shared.rs:49-58`; `maki-config/src/providers.rs:167-183`)
- Error handling doesn't read `Retry-After`, so the retry timing described here is only the local retry policy. (`maki-providers/src/error.rs:118-124`; `maki-providers/src/retry.rs:1-23`)
