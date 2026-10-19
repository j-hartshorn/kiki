# Spec: Google Gemini provider with Gemini 4 Argon and Gemini 3.8 Flash

## Context

Kiki supports OpenAI and Anthropic (`src/config/models.rs:41-59`, `src/provider.rs:21-57`), with `openai/gpt-5.6-sol` hard-coded as the built-in default model (`src/config/models.rs:448`). The provider seam (`Provider`, `ProviderModel`, `ProviderReplay`, `ForeignPart`, and `~/.kiki/auth.toml`) was built so additional providers plug into the same selection, credential, history, compaction, subagent, and pricing paths (`src/model.rs:265-285,400-445`; `src/config/auth.rs:38-72`).

Adding Google's Gemini API (`google/gemini-4-argon` and `google/gemini-3.8-flash`) lets sessions and subagents run on Gemini 4 Argon and Gemini 3.8 Flash and switch between OpenAI, Anthropic, and Google between runs. For a new user with no configured model override, Kiki should also pick its built-in default model from the authenticated providers in priority order: `google/gemini-4-argon`, `openai/gpt-6.1-sol`, then `anthropic/claude-opus-5-5`.

A credentialed probe on 14-15 October 2026 verified the Gemini Interactions API behaviour and pricing this spec relies on (see Key discoveries).

## Goals

- `google/gemini-4-argon` and `google/gemini-3.8-flash` are selectable everywhere catalogue models are: `config.toml`, `KIKI_MODEL`, `/model`, the model picker, favourites, subagents, and session persistence.
- When no model is set by user/project `config.toml`, session preferences, or `KIKI_MODEL`, Kiki selects the built-in default model from the authenticated providers in priority order: `google/gemini-4-argon`, `openai/gpt-6.1-sol`, then `anthropic/claude-opus-5-5` (falling back to `google/gemini-4-argon` when no provider is authenticated).
- `GEMINI_API_KEY` and `[google]` in `~/.kiki/auth.toml` (via `kiki login google` / `kiki logout google`) follow the same optional-credential rules as OpenAI and Anthropic: startup requires only the selected model's provider key, and keyless Google models are hidden from the picker and rejected by `/model` and subagents with remediation naming `GEMINI_API_KEY`.
- A new `GoogleModel` adapter (`src/google.rs`) streams steps and compaction summaries over `POST /v1beta/interactions?alt=sse` (`store: false`), mapping `thought`, `model_output`, and `function_call` steps to Kiki's progress, actions, and `google`-tagged replay.
- Sessions can switch freely between OpenAI, Anthropic, and Google between runs, honouring `cross_provider_reasoning = "inline" | "drop"`.
- Usage and cost for both Google models are computed accurately from Interactions API `usage` totals, including implicit cache hits and thinking tokens.
- Retries, stream restarts, authentication failures, secret redaction, and the 16 MiB content budget match the OpenAI and Anthropic guarantees.

## Non-goals

- Vertex AI, Google Application Default Credentials, OAuth, or explicit context caching (`cachedContents` / `caches.create`, which Gemini 4 Argon does not support).
- Background execution (`background: true`), server-stored interactions (`store: true` / `previous_interaction_id`), or long decode past 65,536 output tokens.
- Server-side Gemini tools (`code_execution`, `google_search`, `url_context`, `computer_use`, `mcp_server`) or multimodal inputs.

## Key discoveries

- `Provider` (`src/config/models.rs:41-59`) is a closed Copy enum (`OpenAi`, `Anthropic`). Extending it with `Google` touches `auth.rs` (`src/config/auth.rs:38-72,383-385`), `config.rs` (`src/config.rs:214-255,395-435,520-635`), `provider.rs` (`src/provider.rs:21-155`), `runtime.rs` (`src/runtime.rs:158-173`), `transcript.rs` (`src/transcript.rs:152-166,3760-3765`), `compaction.rs` (`src/compaction.rs:277-281`), and CLI usage in `main.rs` (`src/main.rs:91`).
- `resolve_startup_with_session_preferences` (`src/config.rs:536-565`) resolves provider API keys before calling `settings::resolve`, so `settings::resolve` can receive the default model resolved from those keys while keeping the `BuiltIn < User < Project < Session < Environment` precedence (`src/config/settings.rs:491-565`).
- `SettingsContext` and `[thinking]` validation already check each model's own `thinking_levels` slice (`src/config/settings.rs:845-875,1062-1076`), and `StartupProblemKind::ThinkingUnsupported` already handles a known thinking level (`xhigh`, `max`) that the selected model does not support (`src/config/settings.rs:594-598`).
- `ProviderFailure::is_authentication` currently checks only `self.status == Some(401)` (`src/model.rs:624-626`), whereas Google returns HTTP 400 with `reason: "API_KEY_INVALID"` for an invalid `x-goog-api-key` (in addition to HTTP 401). Runtime run settlement disables the runtime on `is_authentication()` (`src/agent.rs:179-184`, `src/runtime.rs:2050-2065`), while compaction summary failures surface as compaction failure diagnostics (`src/agent.rs:688-701`, `src/runtime.rs:2202-2245`).
- `estimate_usage` in `src/pricing.rs:69-135` requires `usage.service_tier` to match `model.pricing.identity.service_tier`, subtracts `cached_input_tokens` and `cache_write_input_tokens` from `input_tokens`, requires `reasoning_output_tokens <= output_tokens` and `input_tokens + output_tokens == total_tokens`, and charges `output_tokens` once at `output_nanos`.
- Credentialed probe against `https://generativelanguage.googleapis.com/v1beta/interactions`, 14-15 October 2026, both models:
  - `google/gemini-4-argon` and `google/gemini-3.8-flash` both have a `1,048,576`-token context window and accept synchronous `max_output_tokens` up to `65,536` (`65537` with `background=false` returns HTTP 400).
  - Both models support `thinking_level` values `low`, `medium`, and `high` (default `medium`), and reject `minimal`, `xhigh`, and `max` with HTTP 400.
  - Streaming `POST /v1beta/interactions?alt=sse` with header `x-goog-api-key`, `store: false`, `stream: true`, and `generation_config.thinking_summaries: "auto"` emits SSE events `interaction.created`, `interaction.status_update`, `step.start`, `step.delta`, `step.stop`, `interaction.completed`, and `done` (`data: [DONE]`).
  - `step.start` opens `thought`, `model_output`, or `function_call` steps by `index`. `step.delta` carries `thought_summary` (`delta.content.text`), `thought_signature` (`delta.signature`), `text` (`delta.text`), and `arguments_delta` (`delta.arguments`).
  - Function tools use `{"type": "function", "name": "execute_python", "description": ..., "parameters": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}}` with `generation_config.tool_choice: "validated"`. `parameters` rejects `additionalProperties: false` with HTTP 400.
  - On `interaction.completed`, `interaction.status` is `"requires_action"` for a tool call, `"completed"` for a message, `"incomplete"` when `max_output_tokens` is hit, and `"failed"` or `"cancelled"` on terminal failure.
  - Stateless replay (`store: false`) accepts `user_input`, `thought` (with `signature` and optional `summary`), `model_output`, `function_call`, and `function_result` steps, including adjacent `user_input` steps, cross-model signatures between `gemini-4-argon` and `gemini-3.8-flash`, and retained signed `thought` + `function_call` steps after the preceding prefix is replaced by a compaction summary (`user_input`).
  - Stateless replay rejects a `function_call` turn unless it begins with a signed `thought` step (`Model turns with thought summaries must start with a thought block in thinking models`), even in earlier turns. Emitting `{"type": "thought", "signature": "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I="}` (base64 of Google's documented `skip_thought_signature_validator` bypass token) at the start of a foreign `CompletedCode` entry, before any `model_output` text and the `function_call` step, succeeds with HTTP 200 on both models. Gemini `call_*` IDs pass through to OpenAI and Anthropic unchanged.
  - In `interaction.completed`, `usage.total_input_tokens` includes `total_cached_tokens`, `total_output_tokens` excludes `total_thought_tokens`, and `total_tokens = total_input_tokens + total_output_tokens + total_thought_tokens`. `gemini-3.8-flash` echoes `"service_tier": "standard"` on `interaction.completed`; `gemini-4-argon` echoes `"service_tier": "standard"` on non-streaming responses but omits `service_tier` on streaming `interaction.completed` events.
  - Pricing per million tokens, from `https://ai.google.dev/gemini-api/docs/pricing` and `https://ai.google.dev/gemini-api/docs/latest-model` (Standard tier):

    | Model | Input | Output (incl. thoughts) | Cache read (implicit) | Cache write |
    |---|---:|---:|---:|---:|
    | Gemini 4 Argon | $2.00 | $10.00 | $0.10 | $0.00 |
    | Gemini 3.8 Flash | $0.75 | $3.75 | $0.075 | $0.00 |

    Neither model has a long-context pricing band.

## Proposed approach

**Catalogue and default model.** `Provider` gains `Google` (`display_name`: `"Google"`, `api_key_variable`: `"GEMINI_API_KEY"`), and `ModelConfiguration::qualified_id` maps the `google/` prefix to `Provider::Google`. Add `GOOGLE_THINKING_LEVELS = &["low", "medium", "high"]` and two models at the end of `CATALOGUE`, both with default thinking `medium`, service tier `standard`, `1_048_576` context tokens (`1m`), `65_536` maximum output tokens (`64k`), and long-context threshold `1_048_576` (`1m`) with identical short and long rates:

- `google/gemini-4-argon` (`GEMINI_4_ARGON`)
- `google/gemini-3.8-flash` (`GEMINI_3_8_FLASH`)

The static `DEFAULT_MODEL` becomes `&GEMINI_4_ARGON`. When no model is set by user/project `config.toml`, session preferences, or `KIKI_MODEL`, startup selects the first model in `[&GEMINI_4_ARGON, &GPT6_1_SOL, &OPUS_5_5]` whose provider has a valid API key, or `DEFAULT_MODEL` when none are authenticated.

**Credentials and selection surfaces.** `GEMINI_API_KEY`, `OPENAI_API_KEY`, and `ANTHROPIC_API_KEY` are each optional. `auth.rs` accepts `[google]` in `~/.kiki/auth.toml`, and `kiki login google` / `kiki logout google` manage it. Startup requires only the key for the effective selected model's provider. The startup card shows `OpenAI API key`, `Anthropic API key`, and `Google API key` lines. The model picker lists usable models under `Favourites`, `OpenAI`, `Anthropic`, and `Google` headings, omitting keyless models and empty groups.

**Router and authentication status.** `ProviderModel` holds `openai: Option<OpenAiModel>`, `anthropic: Option<AnthropicModel>`, and `google: Option<GoogleModel>`, dispatching `validate_history`, `summarise`, `step`, and `ChildModelFactory::create` by `selection.model().provider()`. `RuntimeDisabledReason::OpenAiAuthentication` and `AnthropicAuthentication` collapse into `RuntimeDisabledReason::Authentication(Provider)`, rendered as `"<Provider> authentication failed; restart Kiki"`. `ProviderFailure` gains an explicit `authentication` flag alongside its existing HTTP 401 check so Google's HTTP 400 `API_KEY_INVALID` error is marked as an authentication failure on both `step` and `summarise` (disabling the runtime on model step settlement, and formatting as an `authentication failure` in compaction diagnostics, matching OpenAI and Anthropic).

**Google adapter (`src/google.rs`).**

- Transport is `reqwest` (with redirects disabled) plus `eventsource-stream`, posting to `{api_base}/v1beta/interactions?alt=sse` with header `x-goog-api-key`.
- Each step request sends `model`, `service_tier: "standard"`, `store: false`, `stream: true`, `system_instruction` set to the prompt, `input` built from `ModelHistory`, one `execute_python` function tool (`parameters` without `additionalProperties`), and `generation_config` with `max_output_tokens`, `thinking_level` from the selection, `thinking_summaries: "auto"`, and `tool_choice: "validated"`.
- Summarisation sends the same request shape without `tools` or `tool_choice`: `system_instruction` is `request.instructions`, `input` is a single `user_input` step with `request.input`, and `max_output_tokens` is `request.output_limit`.
- Streaming events map as follows:
  - `interaction.created` and `interaction.status_update` validate any echoed `model` against `selection.model().wire_id()`.
  - `step.start`, `step.delta`, and `step.stop` assemble ordered `thought`, `model_output`, and `function_call` steps. `thought_summary` text deltas emit `ReasoningSummaryChunk`; `model_output` text deltas emit `AssistantTextChunk`; `function_call` `arguments_delta` strings feed `PythonPreviewDecoder`.
  - `interaction.completed` records `status`, `service_tier`, and `usage`. `done` (`[DONE]`) or EOF after `interaction.completed` finishes the stream.
- Completed steps map to actions:
  - `status: "requires_action"` with one `execute_python` `function_call` step becomes `ModelAction::Code`, with any `model_output` text as `commentary`.
  - `status: "completed"` with non-empty `model_output` text and no `function_call` becomes `ModelAction::Message`.
  - `status: "incomplete"`, `"failed"`, or `"cancelled"` returns a terminal `ModelError::Provider` failure; empty message text, multiple tool calls, or a status/content mismatch returns `ModelError::InvalidResponse`.
- Replay stores the completed assistant steps (`thought`, `model_output`, `function_call`) as `OpaqueReplayItem` JSON values tagged `"google"`. On replay, `GoogleModel` verifies the stored steps match the semantic `CompletedCode` or `FinalMessage` before emitting them, followed for `CompletedCode` by `{"type": "function_result", "call_id": ..., "name": "execute_python", "result": feedback}`.
- Foreign entries (`entry.foreign_parts("google", self.cross_provider_reasoning)`) encode `ForeignPart::Text` as `model_output` steps and `ForeignPart::ToolCall` as `function_call` plus `function_result`, prepending `{"type": "thought", "signature": "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I="}` at the start of a foreign `CompletedCode` turn so Gemini's stateless signature validator accepts the foreign tool call.
- Usage maps `total_input_tokens` to `input_tokens`, `total_cached_tokens.unwrap_or(0)` to `cached_input_tokens`, `0` to `cache_write_input_tokens` and `cache_write_1h_input_tokens`, `total_thought_tokens` to `reasoning_output_tokens`, `total_output_tokens + total_thought_tokens` to `output_tokens`, and `total_tokens` to `total_tokens`. Because `gemini-4-argon` omits `service_tier` on streaming `interaction.completed` events, `service_tier` defaults to `selection.model().service_tier` (`"standard"`) when omitted while preserving any explicit tier returned by the server.
- Retries, stream restarts, secret redaction, raw SSE frame bounding (`SseFrameLimit`, shared with `src/anthropic.rs`), and the 16 MiB `ACCEPTED_MODEL_CONTENT_LIMIT` content budget mirror `src/anthropic.rs`: 2 HTTP retries on 408, 429, and 5xx honouring `retry-after-ms` and `retry-after` (capped at 60s), up to 3 stream attempts on transport drops or mid-stream retryable errors (`overloaded`, `unavailable`, `internal`, `resource_exhausted`), and full redaction of `api_key` from diagnostics.

## Seams & interfaces

### 1. `Provider` and catalogue (`src/config/models.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAi,
    Anthropic,
    Google,
}

pub static GOOGLE_THINKING_LEVELS: &[&str] = &["low", "medium", "high"];
pub static GEMINI_4_ARGON: ModelConfiguration = /* google/gemini-4-argon */;
pub static GEMINI_3_8_FLASH: ModelConfiguration = /* google/gemini-3.8-flash */;
pub static DEFAULT_MODEL: &ModelConfiguration = &GEMINI_4_ARGON;
pub static DEFAULT_MODELS: [&ModelConfiguration; 3] = [&GEMINI_4_ARGON, &GPT6_1_SOL, &OPUS_5_5];

pub fn default_model_for_providers(providers: &[Provider]) -> &'static ModelConfiguration;
```

- **Invariants**: `model.provider()` and `model.wire_id()` split on `'/'` (`"google"` -> `Provider::Google`). `default_model_for_providers` returns the first entry of `DEFAULT_MODELS` whose `provider()` is in `providers`, or `DEFAULT_MODEL` when none match.

### 2. Authentication failure marker (`src/model.rs`, `src/runtime.rs`)

```rust
impl ProviderFailure {
    pub(crate) fn with_authentication(mut self, authentication: bool) -> Self;
    pub fn is_authentication(&self) -> bool {
        self.authentication || self.status == Some(401)
    }
}
```

- **Error modes**: HTTP 401 on any provider, or HTTP 400 with `reason == "API_KEY_INVALID"` on Google, sets `is_authentication() == true`. On a model step, this disables the runtime with `RuntimeDisabledReason::Authentication(provider)` (`src/runtime.rs:2050-2065`); on a compaction summary, it formats as `authentication failure` in the compaction error diagnostic (`src/agent.rs:688-701`).

### 3. `GoogleModel` and `ProviderModel` (`src/google.rs`, `src/provider.rs`)

```rust
#[derive(Clone)]
pub(crate) struct GoogleModel { /* prompt, api_key, endpoint, cross_provider_reasoning, client, frame_limit */ }

impl GoogleModel {
    pub(crate) fn new(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self;
    pub(crate) fn with_prompt(&self, prompt: String) -> Self;
    pub(crate) fn with_endpoint(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        api_base: &str,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self;
}

impl Model for GoogleModel { /* validate_history, summarise, step */ }
```

Caller-side usage in `src/provider.rs`:

```rust
let model = ProviderModel::new(
    &prompt,
    ready.api_key(Provider::OpenAi).map(ApiKey::expose),
    ready.api_key(Provider::Anthropic).map(ApiKey::expose),
    ready.api_key(Provider::Google).map(ApiKey::expose),
    anthropic_cache_ttl,
    cross_provider_reasoning,
);
```

## Key decisions & tradeoffs

- **Agreed: `google` provider slug, `Google` display name, `GEMINI_API_KEY` environment variable.** Matches Pi, Maki, and the local subagent skill catalogue while keeping Google's standard environment variable. Rejected a `gemini/` prefix (breaks cross-tool model ID compatibility) and `GOOGLE_API_KEY` (not the Gemini API standard).
- **Agreed: `POST /v1beta/interactions?alt=sse` (`store: false`) over `streamGenerateContent`.** Gives first-class `thought`, `model_output`, and `function_call` steps, explicit SSE lifecycle framing (`interaction.created`, `step.start`/`delta`/`stop`, `interaction.completed`), and avoids `streamGenerateContent`'s part-level signature attachment quirks. Rejected `streamGenerateContent` (requires preserving empty signature-bearing text parts) and stateful `store: true` (couples Kiki's local session tree and branching to server-stored interaction IDs).
- **Agreed: `65_536` `max_output_tokens` for both `google/gemini-4-argon` and `google/gemini-3.8-flash`.** Matches the Interactions API's synchronous output cap verified by live probe (`65536` succeeds; `65537` returns HTTP 400) and leaves headroom because Gemini counts `thought` tokens inside `max_output_tokens`. Rejected the `32_768` cap from `GET /v1beta/models/gemini-4-argon`.
- **Agreed: Built-in default model priority `google/gemini-4-argon` -> `openai/gpt-6.1-sol` -> `anthropic/claude-opus-5-5` based on authenticated providers.** A new user with any one provider configured gets that provider's frontier model out of the box without editing `config.toml`, while explicit `config.toml`, session preferences, and `KIKI_MODEL` still take precedence.
- **Agreed: Prepend `{"type": "thought", "signature": "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I="}` on foreign `CompletedCode` turns.** The Interactions API rejects stateless `function_call` steps that do not start with a signed `thought` step. Using Google's documented `skip_thought_signature_validator` token (base64-encoded for the `signature` bytes field) lets foreign OpenAI and Anthropic tool turns replay cleanly without inventing a separate text-only fallback.
- **Agreed: Default omitted `service_tier` on `interaction.completed` to the requested tier (`"standard"`).** `gemini-4-argon` omits `service_tier` on streaming `interaction.completed` events even when `service_tier: "standard"` is sent in the request, while `estimate_usage` requires `usage.service_tier` to match the pricing identity's tier. Preserving any explicit tier returned by the server still catches unexpected non-standard tiers.
- **Agreed: Implicit caching only (no `[google]` settings table).** Gemini 4 Argon does not support explicit `cachedContents`, and both models enable implicit caching automatically with a 90-95% discount reported in `total_cached_tokens`.

## Risks / unknowns

- **Scheduled price changes.** Google's published rates for `gemini-4-argon` ($2/$10/$0.10 per 1M) are introductory through 30 November 2026 ($4/$20/$0.20 from 1 December 2026), and `gemini-3.8-flash` ($0.75/$3.75/$0.075 per 1M) is introductory through 31 December 2026. Rates are pinned with version `2026-10-14` and will need a catalogue bump when the schedule changes.
- **Streaming `step.start` initial payloads.** Although the live probe delivered `thought_summary`, `thought_signature`, `text`, and `arguments_delta` via `step.delta`, Google's SDK examples also check initial `summary`, `content`, `signature`, and `arguments` on `step.start`. `GoogleModel` will accept initial content on `step.start` as well as incremental `step.delta` events.

## Dependencies

- Existing crate dependencies (`reqwest`, `eventsource-stream`, `serde`, `serde_json`, `time`, `tokio`, `tokio-util`); no new Cargo dependencies.
- Official Gemini API documentation: `https://ai.google.dev/gemini-api/docs/latest-model`, `https://ai.google.dev/gemini-api/docs/thinking`, `https://ai.google.dev/gemini-api/docs/pricing`, and `https://ai.google.dev/static/api/interactions.md.txt`.

## Plan

### Acceptance criteria

- `google/gemini-4-argon` and `google/gemini-3.8-flash` (both with `low`, `medium`, `high` thinking levels and default `medium`) work end to end: `model` in `config.toml`, `KIKI_MODEL`, the picker, `/model`, `/thinking`, Shift-Tab, favourites, subagent `model=`, and session save/resume.
- With no model set in `config.toml`, session preferences, or `KIKI_MODEL`, startup selects `google/gemini-4-argon` when Google has a key, else `openai/gpt-6.1-sol` when OpenAI has a key, else `anthropic/claude-opus-5-5` when Anthropic has a key, and falls back to `google/gemini-4-argon` when no provider is configured.
- `GEMINI_API_KEY` and `[google]` in `~/.kiki/auth.toml` (`kiki login google` / `kiki logout google`) are optional unless a Google model is effectively selected. Startup card shows `OpenAI API key`, `Anthropic API key`, and `Google API key` (`present` or `missing`).
- Picker lists only models whose provider has a key under `Favourites`, `OpenAI`, `Anthropic`, and `Google` headings. `/model` and subagent selection for a keyless Google model fail cleanly naming `GEMINI_API_KEY`.
- A Google run streams thinking summaries, assistant text, and Python preview over `POST /v1beta/interactions?alt=sse` (`store: false`), executes code in the Arena, finishes with a message, prices usage accurately (including implicit cache reads and thinking tokens), and continues cleanly after manual and automatic threshold compaction (including when compacted history retains a signed Google `CompletedCode` tail).
- Sessions can switch between OpenAI, Anthropic, and Google between runs, honouring `cross_provider_reasoning = "inline" | "drop"`.
- Google 408/429/5xx retry twice honouring `retry-after-ms` and `retry-after` (capped at 60s); streams get up to 3 attempts on transport drops or retryable SSE errors; HTTP 401 and HTTP 400 `API_KEY_INVALID` on a model step disable the runtime with `Google authentication failed; restart Kiki` (and format as `authentication failure` on compaction summary errors); diagnostics redact `GEMINI_API_KEY`; each attempt enforces the 16 MiB application-content budget.

### Overall validation

- `cargo fmt --all -- --check`: no diff.
- `cargo build && env -u OPENAI_API_KEY -u ANTHROPIC_API_KEY -u GEMINI_API_KEY MONTY_BIN=$(pwd)/target/debug/kiki cargo test --all-targets -- --test-threads=1`: all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- `just live-test-google` (with `GEMINI_API_KEY` set): passes.
- `cargo test google::tests::live_argon_continues_after_compaction_with_retained_signed_thinking -- --exact --ignored --test-threads=1` (with `GEMINI_API_KEY` set): passes.

### Planning context

- **Conventions:** `thiserror` error types; static `ModelConfiguration` entries in `src/config/models.rs`; hand-written `toml_edit` validation in `src/config/auth.rs` and `src/config/settings.rs`; unit and loopback SSE fixture tests in each module's `mod tests`; every commit `--signoff`.
- **Staging the default model switch:** Task 1 keeps `DEFAULT_MODEL = &SOL` while adding `Provider::Google`, `GOOGLE_THINKING_LEVELS`, `GEMINI_4_ARGON`, `GEMINI_3_8_FLASH`, and collapsing `RuntimeDisabledReason` authentication variants into `RuntimeDisabledReason::Authentication(Provider)`. Task 2 adds `DEFAULT_MODELS` and `default_model_for_providers`, switches `DEFAULT_MODEL = &GEMINI_4_ARGON`, includes `Provider::Google` in the default test provider sets in `src/tui.rs:290` and `src/subagents.rs:757`, and wires `default_model_for_providers` into `config.rs` and `settings.rs`. Because `ready_snapshot_for_test` and `ready_input` (`src/config.rs:884-905,971-983`) set only `openai_api_key`, they resolve `openai/gpt-6.1-sol` (`GPT6_1_SOL`) in Task 2; tests in `src/config.rs`, `src/tui.rs`, and `src/transcript.rs` that assert the snapshot's default model ID update from `openai/gpt-5.6-sol` to `openai/gpt-6.1-sol`, OpenAI-specific test helpers in `src/openai.rs:1933`, `src/compaction.rs:913`, and `src/pricing.rs:254` bind `&SOL` directly, `non_unicode_api_key_is_invalid_without_retaining_its_bytes` (`src/config.rs:1909`) pins `openai.model = Some(OsString::from(SOL.id))`, and `DEFAULT_MODEL`-based authentication tests in `src/runtime.rs:8837,8866,15047,15066` expect `RuntimeDisabledReason::Authentication(Provider::Google)`.
- **Startup card height:** Adding `Google API key: <present|missing>` in Task 2 adds one line to `startup_block` (`src/transcript.rs:3750-3766`), so `configuration_configured_mode_renders_a_ready_snapshot_and_remains_editable` (`src/tui.rs:8231`) renders at height 17 instead of 16.

### Tasks

#### Task 1: Extend `Provider` and add the Google catalogue models and pricing
**Changes** — Add `Provider::Google` (`"Google"`, `"GEMINI_API_KEY"`), `GOOGLE_THINKING_LEVELS`, `GEMINI_4_ARGON`, and `GEMINI_3_8_FLASH` in `src/config/models.rs` (keeping `DEFAULT_MODEL = &SOL` until Task 2). Collapse `RuntimeDisabledReason::OpenAiAuthentication` and `AnthropicAuthentication` into `RuntimeDisabledReason::Authentication(Provider)` (`src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`). Update exhaustive `Provider` match arms across the crate with placeholder `missing_adapter(Provider::Google)` in `src/provider.rs` until Task 4.
**Tests** (`src/config/models.rs`, `src/pricing.rs`, `src/openai.rs`, `src/anthropic.rs`, `src/transcript.rs`, unit) — Red first: catalogue order and count (12 models); `Provider::Google` display name, env var, and `google/` prefix parsing; `GEMINI_4_ARGON` and `GEMINI_3_8_FLASH` limits (`1_048_576`/`1m`, `65_536`/`64k`), thinking levels (`low`, `medium`, `high`, default `medium`), and pricing rates; `estimate_usage` for both Google models with uncached, cached, and thinking tokens; `OpenAiModel` and `AnthropicModel` reject `Google` selections with `InvalidSelection` before networking; `RuntimeDisabledReason::Authentication(Provider::Google)` renders `"Google authentication failed; restart Kiki"`.
**Files** — Modify: `src/config/models.rs`, `src/config.rs`, `src/config/auth.rs`, `src/provider.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`, `src/compaction.rs`, `src/pricing.rs`, `src/openai.rs`, `src/anthropic.rs`
**Verification** — Overall validation `cargo build && env -u ... cargo test` and `cargo clippy --all-targets -- -D warnings` — all pass, zero warnings.

#### Task 2: Store and resolve `GEMINI_API_KEY`, select the built-in default from authenticated providers, and update selection surfaces
**Changes** — Support `[google]` in `~/.kiki/auth.toml` (`src/config/auth.rs`) and `kiki login google` / `kiki logout google` (`src/main.rs`). Add `DEFAULT_MODELS` and `default_model_for_providers` and switch `DEFAULT_MODEL = &GEMINI_4_ARGON` in `src/config/models.rs`. Include `Provider::Google` in default test provider sets in `src/tui.rs:290` (updating existing picker group expectations at `src/tui.rs:6548-6573`) and `src/subagents.rs:757`, bind `&SOL` in the OpenAI-specific test helpers in `src/openai.rs`, `src/compaction.rs`, and `src/pricing.rs`, and update `DEFAULT_MODEL` authentication-reason assertions in `src/runtime.rs`. In `src/config.rs` and `src/config/settings.rs`, resolve `GEMINI_API_KEY` and stored `[google]` key, compute the built-in default model via `default_model_for_providers` from the providers with valid keys (falling back to `DEFAULT_MODEL`), pass it into `settings::resolve`, expose `Provider::Google` presence and `ReadyStartup::api_key`, and redact all three keys. Add `Google API key` to `startup_block` (`src/transcript.rs`).
**Tests** (`src/config/models.rs`, `src/config/auth.rs`, `src/config.rs`, `src/config/settings.rs`, `src/transcript.rs`, `src/tui.rs`, `src/subagents.rs`, unit) — Red first: `default_model_for_providers` priority (`Google` -> `OpenAi` -> `Anthropic` -> `GEMINI_4_ARGON`); `auth::store`/`load`/`remove` for `Provider::Google`; unknown table error lists `[openai]`, `[anthropic]`, and `[google]`; startup with only `GEMINI_API_KEY` selects `google/gemini-4-argon` and is ready; startup with only `OPENAI_API_KEY` selects `openai/gpt-6.1-sol`; startup with only `ANTHROPIC_API_KEY` selects `anthropic/claude-opus-5-5`; startup with multiple keys follows `Google > OpenAi > Anthropic`; startup with no keys selects `google/gemini-4-argon` and reports `GEMINI_API_KEY is missing or empty.`; `KIKI_THINKING=xhigh` or `max` with a Google model reports `ThinkingUnsupported`; diagnostics redact all three keys; startup card renders `Google API key`; picker groups include `Google` (`google/gemini-4-argon`, `google/gemini-3.8-flash`) when keyed and hide keyless Google models and favourites; `/model` and subagent selection without `GEMINI_API_KEY` report `GEMINI_API_KEY`; `/thinking` and Shift-Tab on a Google model cycle `low -> medium -> high -> low`.
**Files** — Modify: `src/config/models.rs`, `src/config/auth.rs`, `src/config.rs`, `src/config/settings.rs`, `src/main.rs`, `src/transcript.rs`, `src/tui.rs`, `src/subagents.rs`, `src/runtime.rs`, `src/openai.rs`, `src/compaction.rs`, `src/pricing.rs`
**Verification** — Overall validation `cargo build && env -u ... cargo test` and `cargo clippy --all-targets -- -D warnings` — all pass, zero warnings.

#### Task 3: Implement `GoogleModel` (`src/google.rs`)
**Changes** — Move `SseFrameLimit` (and its unit tests) from `src/anthropic.rs` to `src/presentation.rs` so both SSE adapters share the raw frame and CR/CRLF line normaliser. Add `ProviderFailure::with_authentication` in `src/model.rs` (moved from Task 1 so it lands with its first caller), temporarily marked `#[cfg_attr(not(test), expect(dead_code))]` while `google.rs` is test-only. `ContentBudget` moves with `SseFrameLimit` to `src/presentation.rs`, and `retry_delay`, `redact`, and `bounded_body` (with their tests) move from `src/anthropic.rs` to `src/provider.rs` so both adapters share them. Wire text parses into lenient wire types, separate from the strict replay `Step` types; `step.start` indices must strictly increase per attempt (not necessarily from 0), and `step.delta`/`step.stop` must match the open step's index. Create `src/google.rs` (declared `#[cfg(test)] mod google;` in `src/main.rs` until Task 4) implementing `GoogleModel` and `Model` (`validate_history`, `summarise`, `step`) over `POST {api_base}/v1beta/interactions?alt=sse` per the spec's Google adapter and Seams & interfaces sections. Include SSE framing via `SseFrameLimit`, `thought`/`model_output`/`function_call` step assembly, `google`-tagged `ProviderReplay` validation, foreign-entry encoding with the `c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I=` bypass `thought` step on foreign `CompletedCode` turns, usage mapping with `"standard"` fallback for omitted `service_tier`, HTTP 400 `API_KEY_INVALID` and HTTP 401 authentication marking, 2 HTTP retries with `retry-after-ms`/`retry-after` (capped at 60s), up to 3 stream attempts on transport drops or retryable SSE `error` events, secret redaction, and the 16 MiB `ACCEPTED_MODEL_CONTENT_LIMIT` per-attempt content guard. Extend cross-provider replay tests in `src/openai.rs` and `src/anthropic.rs` to cover `google`-tagged replay entries.
**Tests** (`src/google.rs`, `src/presentation.rs`, `src/openai.rs`, `src/anthropic.rs`, loopback `Fixture`) — Red first: exact step and summary request bodies and `x-goog-api-key` header; tool-call stream yields `ModelAction::Code` with reasoning summary, commentary, and Python preview chunks; text stream yields `ModelAction::Message`; replay round-trips `thought`, `model_output`, `function_call`, and `function_result` (both fresh and after a `CompactionSummary` with a retained signed `CompletedCode` tail) and rejects mismatched replay; foreign OpenAI and Anthropic entries replay with the bypass `thought` step before `model_output` and `function_call`, respecting `cross_provider_reasoning = "inline" | "drop"`; usage maps input, cached, output + thought, and omitted vs explicit `service_tier`, and prices via `estimate_usage`; `incomplete`, `failed`, `cancelled`, empty text, multiple tool calls, HTTP 401, and HTTP 400 `API_KEY_INVALID` fail as specified; retries, stream restarts, cancellation during wait, secret redaction, `SseFrameLimit` enforcement, and the 16 MiB content budget match `src/anthropic.rs`.
**Files** — Create: `src/google.rs`; Modify: `src/main.rs`, `src/model.rs`, `src/presentation.rs`, `src/provider.rs`, `src/openai.rs`, `src/anthropic.rs`
**Verification** — Overall validation `cargo build && env -u ... cargo test` and `cargo clippy --all-targets -- -D warnings` — all pass, zero warnings.

#### Task 4: Wire `GoogleModel` into `ProviderModel`, runtime, live tests, and README
**Changes** — Wire `GoogleModel` into `ProviderModel` (`src/provider.rs`) and `production_model` (`src/main.rs`), make `mod google` unconditional, and remove the temporary `expect(dead_code)` on `ProviderFailure::with_authentication`. Add `live_argon` and `live_argon_continues_after_compaction_with_retained_signed_thinking` ignored live tests in `src/google.rs`, `LIVE_SETUP_GOOGLE` in `src/openai.rs`, and `live-test-google` in `justfile`. Update `README.md` for `GEMINI_API_KEY`, `kiki login google`, the two Google models, the authenticated-provider default model order, and `just live-test-google`.
**Tests** (`src/provider.rs`, `src/runtime.rs`, `src/google.rs`, unit/fixture/ignored live) — Red first: `ProviderModel` dispatches `step`, `summarise`, `validate_history`, and child creation across OpenAI, Anthropic, and Google, and returns `InvalidSelection` naming `GEMINI_API_KEY` when missing; Google step authentication failure (including HTTP 400 `API_KEY_INVALID`) disables the runtime as `RuntimeDisabledReason::Authentication(Provider::Google)` even after a mid-run provider change; a Google run triggers automatic threshold compaction, retains a signed `CompletedCode` tail, and continues on the next step; a saved OpenAI/Anthropic session reopens and continues on Google from semantic history.
**Files** — Modify: `src/main.rs`, `src/model.rs`, `src/provider.rs`, `src/runtime.rs`, `src/openai.rs`, `src/google.rs`, `justfile`, `README.md`
**Verification** — All Overall validation commands pass.
