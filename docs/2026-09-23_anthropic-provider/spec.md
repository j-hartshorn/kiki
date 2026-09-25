# Spec: Anthropic provider with Opus 5.5 and Fable 5.1

## Context

Kiki talks to one provider. `main.rs` always builds `OpenAiModel` (`src/main.rs:46-50`), `OPENAI_API_KEY` is mandatory at startup (`src/config.rs:528-568`), and the catalogue has no provider field (`src/config/models.rs:27-37`). The `Model` seam (`src/model.rs:497-519`) and the Kiki-shaped history with opaque, provider-tagged replay (`src/model.rs:91-218`) were built so that more providers could follow (`docs/VISION.md:48`). But OpenAI rejects non-`openai` replay (`src/openai.rs:1130`), so a session can't change provider.

Model selection is also re-read on every model step (`src/openai.rs:1114`). So `/model` or Shift-Tab during a run changes the model partway through a tool loop. That's harder to reason about, and it becomes a hazard once steps can cross providers.

A credentialed probe on 23 September 2026 confirmed the Anthropic facts this spec relies on (see Key discoveries).

## Goals

- Model selection (model plus thinking) is captured once per run. Changes made mid-run are accepted and saved, and apply from the next run.
- The transcript shows where the effective selection changed between runs. The model never sees these cards.
- `anthropic/claude-opus-5-5` and `anthropic/claude-fable-5-1` are selectable everywhere catalogue models are: settings, environment, pickers, favourites, subagents and session persistence.
- Each provider's API key is optional. Kiki only requires the key for the effective selected model. Selecting a model without its key keeps the current model and shows an error; it never panics.
- A session can switch between OpenAI and Anthropic between runs. Foreign steps are rendered from Kiki's semantic history. Reasoning goes inline as assistant text by default, or is dropped with `cross_provider_reasoning = "drop"`.
- Anthropic requests use prompt caching (TTL `5m` by default, `1h` via settings), and their cost is priced exactly from Anthropic's usage fields.
- Anthropic retries and stream restarts behave like OpenAI's and honour `retry-after-ms` / `retry-after`.

## Non-goals

- Changing the default model; it stays `openai/gpt-5.6-sol`.
- OAuth or Claude subscription auth, Bedrock, Vertex, or other Anthropic-compatible endpoints.
- Anthropic server-side code execution and Anthropic-defined client tools (`bash`, `text_editor`). Both providers get the same custom `execute_python` tool, which runs in the local arena.
- Batch, priority tier or fast mode; Anthropic server-side context management (`clear_tool_uses`, `compact_*`); citations; images.
- Environment variables, slash commands or picker controls for the two new settings. A settings menu comes later.
- A dyn-compatible provider trait, or dynamic model discovery.
- Mid-run model or thinking changes of any kind.
- Making the selection branch state. `/tree` navigation doesn't change the current selection.
- Showing model changes in `/tree`, or a card for a session's initial selection.

## Key discoveries

- The `Model` trait (`src/model.rs:497-519`) returns `impl Future`, so it isn't dyn-compatible. `Agent<M>` and `RuntimeController<M>` are generic over it, and `ChildModelFactory` builds child `M`s (`src/subagents.rs:1228-1231`).
- Selection reaches OpenAI through a `watch::Receiver` that is re-read every step (`src/openai.rs:1114`). The runtime publishes accepted settings with `send_replace` (`src/runtime.rs:1169`). Fixture tests pin mid-run changes (`src/openai.rs:2400-2590`). `SummaryRequest` already carries its own selection (`src/model.rs:39-44`).
- History entries hold semantic fields (source, call ID, commentary, reasoning summary, result, text) plus `ProviderReplay { provider, opaque items }` (`src/model.rs:91-218`). Replay is persisted separately and hex-encoded (`src/session/codec.rs:1477-1498`). The OpenAI adapter builds `function_call_output` from semantic fields rather than replay (`src/openai.rs:185-211`).
- The catalogue has no provider field. `openai/` is stripped in two places (`src/openai.rs:162-171,763-768`). The production picker group is hard-coded to `"OpenAI"` (`src/tui.rs:911-929`), and remediation strings list model IDs by hand (`src/config/settings.rs:26-27,750-760`).
- Pricing assumes `input_tokens` includes cached and cache-write input, applies a long-context band, requires a matching `service_tier`, and has a single cache-write rate (`src/pricing.rs:68-139`). The footer's context denominator uses the long-context threshold (`src/transcript.rs:3968-3993`). Usage is persisted per attempt (`src/session/codec.rs:333,1457-1470`), and the session format is version 2 (`src/session.rs:31`).
- Settings already use tables that reject unknown keys, such as `[subagents]` and `[compaction]` (`src/config/settings.rs:669-683,825,854`).
- Credentialed probe, 23 September 2026, both models:
  - Model IDs are `claude-opus-5-5` and `claude-fable-5-1`, with 1,000,000 context tokens and 128,000 maximum output tokens (`GET /v1/models`). The response's `model` echoes the request.
  - Adaptive thinking is always on. `thinking.type` `disabled` and `enabled` both return 400. `output_config.effort` accepts `low`, `medium`, `high`, `xhigh` and `max`; `none` returns 400.
  - A strict `execute_python` tool works with `tool_choice: {type: "auto", disable_parallel_tool_use: true}`. `any` and `tool` return 400.
  - Replay is accepted with signed thinking, with thinking stripped, with a foreign `call_*` tool ID, and with cross-model signatures (Opus to Fable and back). OpenAI Responses also accepted a `toolu_*` call ID preceded by plain assistant text and no reasoning items.
  - `input_tokens` excludes `cache_read_input_tokens` and `cache_creation_input_tokens`. `cache_creation.ephemeral_5m_input_tokens` and `ephemeral_1h_input_tokens` split writes by TTL. `output_tokens_details.thinking_tokens` is part of `output_tokens`. `service_tier` is `"standard"`.
  - Streaming emits `message_start`, `content_block_start`/`delta`/`stop`, `message_delta`, `message_stop` and `ping`. Delta types are `text_delta`, `thinking_delta`, `signature_delta` and `input_json_delta`. Usage appears in `message_start` and cumulatively in `message_delta`.
  - Pricing per million tokens, from `https://docs.anthropic.com/en/docs/about-claude/pricing`:

    | Model | Input | Output | 5m cache write | 1h cache write | Cache read |
    |---|---:|---:|---:|---:|---:|
    | Opus 5.5 | $4.00 | $20.00 | $5.00 | $8.00 | $0.20 |
    | Fable 5.1 | $10.00 | $50.00 | $12.50 | $20.00 | $0.25 |

    There is no long-context band. The docs give default effort as `medium` for Opus and `high` for Fable.

## Proposed approach

### Phase 1: selection per run

The runtime captures the current `ModelSelection` when a run starts and gives it to the agent. `Model::step` and `Model::validate_history` take the selection explicitly, and models no longer hold a watch receiver. Restore and navigation validate history against the session's current selection (`src/runtime.rs:809,1261`; `src/agent.rs:585`). `ApplySettings` during a run still commits and acknowledges straight away; the new value is used from the next run. Compaction keeps using the selection in its `SummaryRequest`, and child agents keep their fixed selection.

### Phase 1b: model change cards

Each run saves the selection it used, which `start_run` already captures. It is passed to `SessionWriter::commit_request`, stored as `selection` on the `user_request` payload, and carried on `Record::UserRequest` and the restored user-request record as `Option<ModelSelection>`, where `None` means unknown. The model context builder never reads it. Every new run writes it. Only decoding an old payload that lacks the field (`#[serde(default)]`) produces `None`. The session format stays at version 2. `PersistedCode.selection` is removed: nothing reads it (it is only encoded and decoded, `src/session/codec.rs:1012,1024`), and since Phase 1 it always equals the run's selection. Old payloads that contain it still decode, because `StoredCode` accepts unknown fields.

The transcript in `src/transcript.rs` derives **model change cards** with no stored card state. History cards come from the branch's user-request records as they are appended or restored. The pending card is evaluated at render time in `src/tui.rs` from the branch's last run selection and the current selection. Each card compares a previous selection with a next one, and is shown only when both are known and differ:

- A **history card** sits above a run's request card. Previous is the prior run's selection on the branch, and next is this run's.
- A **pending card** is attached to the prompt area rather than the history: it sits in its own rows below the subagent monitor and above the throbber and prompt, and the history viewport shrinks to make room, as it does for the monitor. Previous is the last run's selection on the branch, and next is the current selection the footer shows (`settings_selection`, which includes a Shift-Tab preview).

The consequences need no extra rules. The pending card disappears when the selection is switched back. It re-compares after `/tree` navigation. It settles into the history when the new run's `Record::UserRequest` arrives, as that run's history card. There is none before a branch has a run, and none after old runs with an unknown selection.

Every card reads `Model selected: openai/gpt-6-sol · high`. The pending card is prefixed `[pending] ` while the change hasn't applied yet: during a Shift-Tab preview, or while the runtime hasn't accepted the settings change (`src/tui.rs` `pending_settings`). The middle dot and missing "thinking" word are deliberate: the card is a compact marker, not the footer identity. It uses the footer's dark grey text, has no background, and uses the standard card padding. History cards scroll with history like other cards; the pending card doesn't scroll. It is not a session entry, doesn't appear in `/tree`, and never reaches the model.

### Phase 2: Anthropic provider

**Catalogue.** A model's provider (`OpenAi | Anthropic`) and its wire ID are both derived from the qualified ID prefix; there is no separate stored field. Add these two models after the OpenAI models, both with the shared `low` to `max` thinking levels, service tier `standard`, 1,000,000 context tokens (`1m`) and 128,000 output tokens (`128k`):

- `anthropic/claude-opus-5-5`, defaulting to `medium`
- `anthropic/claude-fable-5-1`, defaulting to `high`

The model picker lists configured favourites under a `Favourites` heading, in their written order. It then lists every catalogue model, favourites included, under `OpenAI` and `Anthropic` headings in catalogue order. A favourite therefore appears twice, and picking either entry selects the same model. With no favourites set, only the provider groups appear. Models whose provider has no key are left out everywhere in the picker, including under `Favourites`, and a provider group with no usable models doesn't appear. Remediation lists are generated from the catalogue.

**Credentials.** `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` are each optional. Startup requires only the key for the effective selected model's provider. If that key is missing, Kiki reports a startup problem as it does today, including on resume when the session's saved model needs the missing key. The remediation names the missing variable and mentions `KIKI_MODEL` as a way to override the saved selection. The startup card shows whether each provider's key is present.

Which providers have keys comes from the router, which is the single source of truth. `main.rs` hands that provider set to the TUI and to subagent selection, so both check it before a selection is used. The following are rejected with remediation that names the missing variable, and the current selection stays in place:

- a `/model <id>` selection whose provider has no key, reported in the notice line
- a subagent model, whether an explicit override or the favourite default, whose provider has no key; the rejection is returned to the calling Python as a tool error

**Router.** A `ProviderModel` holds an optional OpenAI adapter and an optional Anthropic adapter. It dispatches `step`, `summarise` and `validate_history` on the selection's provider. A provider's adapter exists only when its key is present. Both new settings are resolved once at startup and passed straight to the adapter constructors. The child model factory hands child agents a clone of the parent's router.

**Anthropic adapter (`src/anthropic.rs`).**

- Transport is `reqwest` plus `eventsource-stream`, and requests always stream.
- Each request sends:
  - headers `x-api-key` and `anthropic-version: 2023-06-01`
  - the system prompt as one system block
  - one strict `execute_python` tool, with `tool_choice: auto` and parallel tool use disabled
  - `thinking: {type: "adaptive", display: "summarized"}`
  - `output_config.effort` from the selection
  - `max_tokens` set to the model's maximum output
- Streaming deltas map as follows:
  - thinking deltas become reasoning-summary progress
  - text deltas become assistant text
  - `input_json_delta` feeds the Python preview
- Responses map to actions:
  - a `tool_use` block becomes `Code`, with any text blocks as its commentary
  - `end_turn` with text becomes `Message`
  - `max_tokens`, `refusal` or an empty message fail the step, as OpenAI's refusal and incomplete responses do (`src/openai.rs:569-612`)
- Replay stores the assistant content blocks verbatim, tagged `anthropic`.
- Adjacent user-role content, such as a compaction summary followed by a user request, or a tool result followed by the next user request, is merged into one user message, so roles alternate.
- Summarisation uses the same transport and has no tools and no cache breakpoints. `SummaryRequest.instructions` becomes the system block, `input` becomes a single user message, `max_tokens` is `output_limit`, and thinking is adaptive and summarised, with effort from the summary's selection. Text deltas form the summary. `max_tokens`, `refusal` or empty text is an error, matching the compaction spec's failure rules (`docs/2026-09-20_compaction/spec.md`).
- A restarted stream resends the request built from the history as it was before the attempt, and partial output is discarded, the same as OpenAI (`src/openai.rs:263-342`).

**Cross-provider rendering.** Each adapter replays entries whose replay provider matches its own. One shared helper, beside the history types, turns a foreign entry into provider-neutral parts, and each adapter only encodes those parts in its wire format. The parts, taken from semantic fields, are:

- The reasoning summary and the commentary become separate assistant text parts, reasoning first. The reasoning summary is left out when it is empty or when `cross_provider_reasoning = "drop"`.
- Then comes either the tool call, using Kiki's call ID, followed by its result, or the final message text.

Call IDs pass through unchanged.

**Caching.** Each request carries two `ephemeral` cache breakpoints:

- one on the system block, which also covers the tool definition that comes before it in the cache prefix
- one on the last block of the newest message

The TTL comes from `[anthropic] cache_ttl = "5m" | "1h"`, default `5m`. For `5m`, `ttl` is left out of `cache_control` (the API default). For `1h`, `ttl: "1h"` is sent.

**Usage and pricing.** The adapter maps Anthropic usage as follows:

- total input = `input_tokens` + cache read + cache creation
- cached input = cache read
- cache-write input = cache creation
- reasoning output = `thinking_tokens`
- total = input + output

`ModelUsage` gains a 1h cache-write subset, and pricing gains a matching rate:

- OpenAI reports 0 for the subset.
- Pricing rejects a subset larger than total cache writes.
- In stored sessions that predate the field, it decodes as 0, so existing cost estimates are unchanged and the session format version stays at 2.

Anthropic has no long-context band, so each Anthropic model's long-context threshold equals its context window (`1_000_000`, displayed as `1m`) and its long-context rates equal its short-context rates. Pricing and the footer need no code changes: pricing always uses the single rate set, and the footer's denominator is the full window.

**Retries.** One retry-and-restart path in the Anthropic adapter serves both `step` and `summarise`. Requests are retried twice on 408, 429 and 5xx (529 included). The delay comes from `retry-after-ms`, then `retry-after` (a server delay is capped at 60 seconds), and otherwise from backoff. Streams get up to 3 attempts when the transport drops or a mid-stream `overloaded_error` arrives. HTTP 401 maps to the existing authentication failure.

**Settings.** Both are user/project TOML only and read at startup:

- top-level `cross_provider_reasoning = "inline" | "drop"`, default `inline`
- `[anthropic] cache_ttl = "5m" | "1h"`, default `5m`

**Tests.** Credential-free SSE fixtures on a loopback server, following `src/openai.rs`, plus an opt-in ignored live test mirroring `live_sol`.

## Key decisions & tradeoffs

- *Cross-provider switching from day one*: rejected phasing it in later. The probe shows foreign history needs only plain semantic rendering and has no special edge cases.
- *Selection per run, for both model and thinking*: rejected per-step selection and a split rule. One immutable choice per run means the provider can't change inside a tool loop, and the watch plumbing goes away. The cost is losing mid-run effort bumps.
- *Enum router*: rejected a boxed dyn trait (speculative, and it rewrites the seam) and a single multi-provider struct (it tangles two wire formats and breaks dependency boundaries).
- *Optional per-provider keys*: rejected requiring both keys, and keeping OpenAI mandatory. A missing key on selection keeps the current model and reports an error.
- *`display: summarized`*: rejected `omitted`. It matches the OpenAI experience and gives cross-provider rendering something to carry. Pi and Maki both use it.
- *Reasoning inline by default, with a configurable `drop`*: this is pi's carry-over behaviour, with Maki-style dropping available as a TOML switch. Rejected a custom `<previous_reasoning>` marker.
- *Replay matches on provider, not model*: the probe accepted cross-model signatures within Anthropic, and OpenAI already replays across its own models.
- *Fixed cache breakpoints with a TTL setting*: rejected unverified top-level automatic caching. Two breakpoints are enough because context only grows within a run.
- *`reqwest` plus `eventsource-stream`*: rejected a hand-rolled SSE parser (user choice) and community SDKs (they lag the API and carry supply-chain risk).
- *Retries mirror OpenAI and honour server delays*: rejected ignoring the headers, and a larger budget just for 529.
- *A 1h cache-write subset in `ModelUsage`*: keeps pricing exact even if the TTL setting later changes, for example on resume. Rejected pricing cache writes from the current setting.
- *No long-context band via threshold equal to the context window with identical rates*: behaves the same as an optional band, with no code change. Rejected making the band optional in pricing and the footer.
- *Provider derived from the ID prefix*: rejected a stored `provider` field, which would duplicate the prefix.
- *One shared foreign-entry helper*: the reasoning, commentary and drop rules are written and tested once; each adapter only encodes them.
- *Model change cards derived from each run's saved selection*: rejected a new session entry kind for model changes. Pi does this, and it has to reconcile tree entries with a session-wide live model: `/tree` navigation doesn't restore the model, but resume does (`docs/research/2026-09-23_pi-model-change-entries.md`). Also rejected deriving the cards from step data, because final-message steps lack the thinking level and runs with no attempts have no model at all. Whether the selection should become branch state is left for later.
- *Settings in TOML only*: a settings menu comes later.
- *Picker: `Favourites` group, then the full catalogue under provider groups*: keeps the user's cross-provider favourite order and gives clean headings. Rejected provider-only groups (loses the favourite order) and a flat list (loses the provider context).
- *Keyless models hidden from the picker; typed `/model <id>` gets a notice*: the check lives in the TUI and subagent selection, using the router's provider set. Rejected showing keyless models marked unavailable (needs a disabled-item state) and a runtime `SettingsRejected` event with a revert path.

## Risks / unknowns

- The default effort values come from the docs, not from observation. They only matter if effort is omitted, and Kiki always sends it.
- Foreign call IDs pass through unchanged. Current OpenAI `call_*` and Anthropic `toolu_*` IDs both passed the probe. A future ID format with unusual characters would show up as a provider 400 rather than failing silently.
- Fable's cache-read rate ($0.25, 0.025× input) breaks the usual 0.1× pattern. It was read from the docs and is recorded with the pricing identity's date.
- Anthropic may change its adaptive-thinking or display defaults. Kiki always sends both explicitly.
- `reqwest` must match the major version `async-openai` uses, to avoid two HTTP stacks.
- The probes were small. Behaviour near 1M context, and latency at `xhigh` and `max`, are untested.
- The `user_request` payload gains a selection field. `StoredPayload` uses `deny_unknown_fields` (`src/session/codec.rs:72`), so an older Kiki binary can't open sessions written by the new one. New Kiki reads old sessions through `#[serde(default)]`.
- Stored sessions gain an optional usage field. Old sessions must still decode, ideally without bumping the session format version.

## Dependencies

- Research: `docs/research/2026-09-23_pi-anthropic-provider.md`, `docs/research/2026-09-23_maki-anthropic-provider.md`, `docs/research/2026-09-23_pi-model-change-entries.md`.
- Anthropic documentation: Messages API (`https://docs.anthropic.com/en/api/messages`), pricing (`https://docs.anthropic.com/en/docs/about-claude/pricing`), models overview (`https://docs.anthropic.com/en/docs/about-claude/models/overview`).
- The credentialed probe of 23 September 2026 is summarised in Key discoveries. Credentials, response IDs and raw artefacts are deliberately not kept.
- Crates: `reqwest` (rustls, streaming) and `eventsource-stream`.
- Existing specs: `docs/2026-09-14_gpt-6-astra/spec.md` (catalogue pattern) and `docs/2026-09-20_compaction/spec.md` (summarisation seam).

## Phase outline

1. **Selection per run.** Today's OpenAI models get one immutable model-and-thinking selection per run. Changes made mid-run apply from the next run, and the per-step selection plumbing is removed.
1b. **Model change cards.** Each run saves its selection on its request entry. The transcript shows a card between runs where the selection changed, and a pending card attached to the prompt area while a change hasn't yet been used, marked `[pending]` until the change applies. The unused per-step selection is removed.
2. **Anthropic provider.** Opus 5.5 and Fable 5.1 work end to end. This covers the catalogue entries with provider derived from the ID prefix, optional per-provider keys with fallback on selection, the router, the Anthropic adapter with caching and its TTL setting, exact pricing, cross-provider switching with the `cross_provider_reasoning` setting, and documentation.
