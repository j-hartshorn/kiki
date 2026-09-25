# Plan: Anthropic provider — Phase 2: Anthropic provider

## Source
- Spec: `docs/2026-09-23_anthropic-provider/spec.md`, Phase 2 (Proposed approach › Phase 2: Anthropic provider), plus the Goals, Key decisions and Risks that name Anthropic.

## Acceptance criteria
- `anthropic/claude-opus-5-5` (default `medium`) and `anthropic/claude-fable-5-1` (default `high`) work end to end: `model` in TOML, `KIKI_MODEL`, the picker, `/model`, favourites, subagent `model=`, and session save and resume.
- With only `ANTHROPIC_API_KEY` set and an Anthropic model selected, Kiki starts and runs. With only `OPENAI_API_KEY` set and the default model, it behaves as it does today.
- Startup with the selected provider's key missing is a startup problem naming the missing variable. On resume, the remediation also mentions `KIKI_MODEL`. The startup card shows `OpenAI API key` and `Anthropic API key`, each `present` or `missing`.
- The picker lists only models whose provider has a key, under `Favourites`, `OpenAI` and `Anthropic` headings (a heading with no usable models is absent). `/model <id>` for a keyless model shows a notice naming the variable and keeps the current selection. A subagent whose resolved model is keyless gets a tool error naming the variable. None of these panic.
- An Anthropic run streams thinking, text and the Python preview; runs code through `execute_python`; finishes with a message; and survives manual compaction.
- A session can alternate OpenAI and Anthropic runs. Foreign steps reach the model as plain text and tool calls with results; reasoning is inline by default and omitted with `cross_provider_reasoning = "drop"`.
- Anthropic requests carry two cache breakpoints; `[anthropic] cache_ttl = "1h"` sends `ttl: "1h"`, and cost is priced from the 5m/1h split.
- Anthropic 408/429/5xx are retried twice, honouring `retry-after-ms` then `retry-after` (capped at 60s); streams get 3 attempts; 401 disables the runtime with an Anthropic-specific authentication status.

## Overall validation
- `cargo fmt --all -- --check`: no diff.
- `env -u OPENAI_API_KEY -u ANTHROPIC_API_KEY MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets -- --test-threads=1`: all pass.
- `cargo clippy --all-targets -- -D warnings`: clean.
- Inspect `rg -n 'openai/gpt|anthropic/claude' src --glob '!src/config/models.rs'`: every production match is a single ID in a message or test, never a hand-written list of catalogue IDs.
- `just live-test-anthropic` (needs `ANTHROPIC_API_KEY`): passes. `just live-test` still passes.
- Manual:
  - `KIKI_MODEL=anthropic/claude-opus-5-5 just run`: a request that reads a file and answers streams thinking and code, and the footer shows cost and `R`/`W` cache counts on the second step.
  - `/model openai/gpt-5.6-sol`, submit, then `/model anthropic/claude-fable-5-1`, submit: both runs succeed and the model change cards show.
  - `/compact` on an Anthropic model succeeds.
  - `env -u ANTHROPIC_API_KEY KIKI_MODEL=openai/gpt-5.6-sol just run` (with `OPENAI_API_KEY`): the picker has no Anthropic group; `/model anthropic/claude-opus-5-5` shows the notice.

## Planning context
- **Conventions:** `thiserror` errors; statics for catalogue entries; settings validated by hand over `toml_edit` with match arms that reject unknown keys (`src/config/settings.rs:660-694,790-858`); tests live in the same file's `mod tests`; every commit `--signoff`.
- **HTTP stack:** `reqwest 0.13.4` and `eventsource-stream 0.2.3` are already in `Cargo.lock` through `async-openai`. Add them as direct dependencies at those versions (rustls, streaming), used only in `src/anthropic.rs`.
- **Catalogue:** `ModelConfiguration` fields and statics (`src/config/models.rs:27-37,63-263`); `CATALOGUE` and `model_by_id` (`:261-273`); tests assert order and invariants (`:281-495`). OpenAI strips `openai/` in two places (`src/openai.rs:33,110-114,709-713`). Hand-written ID lists: `src/config/settings.rs:26-27,753-755`.
- **Pricing and usage:** `Rates` (`src/config/models.rs:1-24`); `estimate_usage` (`src/pricing.rs:68-139`) subtracts cached and cache-write from input and requires an exact tier match. `ModelUsage` (`src/model.rs:52-61`) is persisted as `StoredUsage` (`src/session/codec.rs:330-339`), which has no `deny_unknown_fields`; the outer `StoredPayload` does (`:72`).
- **Model seam:** `Model` (`src/model.rs:496-519`); `ModelError` has no auth/refusal variants, and `ProviderFailure::is_authentication` checks 401 (`:357-380`). OpenAI maps refusal and incomplete responses to provider failures (`src/openai.rs:569-612`) and empty text to `InvalidResponse` (`:1432`). Replay is `ProviderReplay { provider, items: Vec<OpaqueReplayItem> }` (`src/model.rs:91-118`); OpenAI rejects foreign replay in `replay_values` (`src/openai.rs:1085-1098`) and builds tool outputs from semantic fields (`:185-211`). The `execute_python` schema is inline JSON (`:189-203`).
- **OpenAI retry pattern:** SDK `OpenAIRetryLayer::new(2)` does HTTP retries (`src/openai.rs:84-86`); Kiki restarts streams up to 3 attempts with a 100ms delay and emits `AttemptStarted`/`AttemptSettled` (`:34-35,205-342`). Anthropic has no SDK, so both layers live in `src/anthropic.rs`.
- **Fixture server:** `FixtureReply`, `CapturedRequest`, `Fixture::start`/`start_sequence`, `read_request`, `write_reply` and chunk helpers (`src/openai.rs:2388-2633`) are provider-neutral apart from their home. Anthropic fixtures are synthetic, shaped from the spec's Key discoveries (event and delta types, usage fields).
- **Credentials and startup:** `config::load` reads `OPENAI_API_KEY` (`src/config.rs:20-22,420-436`); `resolve_api_key` makes it mandatory before model resolution (`:448-467,528-568`); `ApiKeyPresence` (`:29,200,233`); `ReadyStartup::api_key` (`:312-366`). Startup card: `startup_block` (`src/transcript.rs:3572-3616`), test at `:7084-7103`.
- **Wiring:** `production_model` and `OpenAiChildModelFactory` in `src/main.rs:46-48,228-265,296-367`; `ChildModelFactory` (`src/subagents.rs:1225-1229`). Auth failure disables the runtime as `RuntimeDisabledReason::OpenAiAuthentication` (`src/runtime.rs:142,2049`), shown by `disabled_status` (`src/transcript.rs:147-156`).
- **Selection surfaces:** `open_model_picker` hard-codes group `"OpenAI"` (`src/tui.rs:912-939`; test `:5310-5336`); the picker already renders multiple groups (`src/tui/picker.rs:513-554`). `set_notice` (`src/tui.rs:1063`). Subagent resolution: `resolve_selection` (`src/subagents.rs:892-911`) and `AcceptedThinking::resolve` (`:790-816`).
- **Live test:** `live_sol` (`src/openai.rs:6500-7197`), run by `just live-test`.
- **Staging:** the PR lands whole. Before routing (Task 5), Anthropic selections fail their step with `InvalidSelection`; before Task 10, switching provider after a completed step fails with `InvalidHistory`. Both are clean errors, never panics. `src/anthropic.rs` is declared `#[cfg(test)]` in Task 4 and made unconditional in Task 5 when it's wired.

## Tasks

### Task 1: Share the loopback SSE fixture
**Changes** — Move the fixture server and its reply/capture types out of `src/openai.rs` tests into a test-only module both adapters can use. No behaviour change.
**Tests** — None new: pure move; the existing OpenAI fixture tests are the check.
**Files** — Create: `src/sse_fixture.rs`; Modify: `src/main.rs` (module declaration), `src/openai.rs`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 2: Derive the provider from the model ID and add the Anthropic models
**Changes**
- Each catalogue model reports its provider (OpenAI or Anthropic, with display name and key variable) and wire ID, both from the ID prefix. OpenAI uses the wire ID and still rejects a non-OpenAI selection with `InvalidSelection` before networking.
- Add Opus 5.5 and Fable 5.1 after the OpenAI models per the spec's Catalogue paragraph and pricing table (the 1h write rate arrives in Task 3, which tests it): tier `standard`, threshold `1_000_000`, long-context rates equal to short, dated pricing identity.
- Model ID lists in settings remediation are generated from the catalogue.
**Tests** (`src/config/models.rs`, `src/config/settings.rs`, `src/openai.rs`, unit) — Red first: an Anthropic selection given to the OpenAI adapter fails with `InvalidSelection` and sends no request; catalogue order includes both Anthropic IDs with the spec's defaults, limits and rates; provider and wire ID derivation for every catalogue model; the invariant tests cover the new entries; remediation for an unknown `model`, `KIKI_MODEL` and `[thinking]` key lists every catalogue ID.
**Files** — Modify: `src/config/models.rs`, `src/config/settings.rs`, `src/openai.rs`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 3: Price 1h cache writes
**Changes** — `ModelUsage` gains a 1h cache-write subset and `Rates` a 1h write rate. Pricing prices 1h writes at that rate and the rest at the 5m rate, and rejects a subset larger than total cache writes. OpenAI reports 0. Stored usage without the field decodes as 0; format stays at version 2.
**Tests**
- `src/pricing.rs`: Red first: an Anthropic usage with a 5m/1h split prices exactly; a subset above total writes is rejected; OpenAI estimates are unchanged.
- `src/session/codec.rs`: a stored usage payload without the field decodes and prices as before.
**Files** — Modify: `src/model.rs`, `src/config/models.rs`, `src/pricing.rs`, `src/session/codec.rs`, `src/openai.rs`, test fixtures that build `ModelUsage`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 4: Anthropic model steps
**Changes** — `src/anthropic.rs` implements `step` and `validate_history` per the spec's adapter bullets and Usage paragraph, as a single attempt with no caching. Tool results come from semantic fields, 401 is an authentication provider failure, and foreign replay is `InvalidHistory` until Task 10.
**Tests** (`src/anthropic.rs`, loopback fixture) — Red first: the exact request body and headers for a fresh request; a tool-call stream yields `Code` with commentary, reasoning summary and preview chunks; a text stream yields `Message`; replay round-trips into the next request with the tool result; a compaction summary followed by a user request becomes one user message; usage from `message_start` is replaced by the cumulative `message_delta` values (fields a later event omits keep their earlier value) and the final 5m/1h split prices via `estimate_usage`; `max_tokens`, `refusal`, empty text and 401 each fail as specified.
**Files** — Create: `src/anthropic.rs`; Modify: `src/main.rs` (module), `Cargo.toml`, `Cargo.lock`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 5: Route by provider with optional keys
**Changes**
- Both keys are optional at startup. Only the effective selection's provider key is required; its absence is a startup problem naming the variable, and on resume the remediation mentions `KIKI_MODEL`. The startup card shows both keys. Startup diagnostics redact every present key, not just the selected provider's (today one key is redacted, `src/config.rs:105-173,453-515`).
- `ProviderModel` holds an optional adapter per provider (present only with its key), dispatches `step`, `summarise` and `validate_history` on the selection's provider, returns `InvalidSelection` naming the variable when the adapter is missing, and reports which providers it has. `main.rs` builds it for fresh and resumed sessions; the child factory hands children a clone. `src/anthropic.rs` stops being test-only.
- An authentication failure disables the runtime with a status naming the provider that failed.
**Tests**
- `src/config.rs`: Red first: only `ANTHROPIC_API_KEY` with an Anthropic `KIKI_MODEL` is ready; only `OPENAI_API_KEY` with the default is ready; the selected provider's key missing is a problem naming it; resume with a saved Anthropic model and no key mentions `KIKI_MODEL`; with both keys set, a diagnostic containing either key shows it redacted.
- `src/transcript.rs`: the startup card shows both key lines; an Anthropic auth failure shows the Anthropic status.
- `src/provider.rs`: dispatch reaches the selected provider's adapter (fixture-backed or scripted); a missing adapter returns `InvalidSelection` without panicking; the provider set matches the keys.
**Files** — Create: `src/provider.rs`; Modify: `src/config.rs`, `src/main.rs`, `src/openai.rs` (factory), `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs` (tests using the disabled reason)
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 6: Keep keyless models out of selection
**Changes** — `main.rs` hands the router's provider set to the TUI and subagent selection.
- Picker: `Favourites` then `OpenAI` and `Anthropic` groups per the spec, leaving out keyless models and empty groups.
- `/model <id>` for a keyless model: notice naming the variable, selection unchanged, nothing sent.
- Subagent: an explicit or favourite-default model whose provider is keyless returns a tool error naming the variable, checked on the result of `resolve_selection`.
**Tests**
- `src/tui.rs`: Red first: with both providers and favourites, groups and order match the spec and a favourite appears twice; with only OpenAI, no Anthropic group or favourite entry; `/model anthropic/claude-opus-5-5` without the key shows the notice and sends no `ApplySettings`.
- `src/subagents.rs`: explicit keyless model and keyless first favourite each return the tool error; a keyed model resolves as before.
**Files** — Modify: `src/main.rs`, `src/tui.rs`, `src/subagents.rs`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 7: Anthropic retries and stream restarts
**Changes** — One path for step and summarise: 2 request retries on 408, 429, 5xx including 529, delayed by `retry-after-ms`, then `retry-after` (server delays capped at 60s), else backoff; up to 3 stream attempts on transport drops and mid-stream `overloaded_error`, resending the pre-attempt request and discarding partial output; cancellation-aware waits; `AttemptStarted`/`AttemptSettled` as OpenAI emits them.
**Tests** (`src/anthropic.rs`, loopback fixture) — Red first: 429 then success is one attempt and two requests; `retry-after-ms` beats `retry-after`; a 120s `retry-after` waits at most 60s (use paused tokio time if the fixture allows, otherwise assert the computed delay); 400 is not retried; a dropped stream and a mid-stream `overloaded_error` restart with the same request body; the settled step after a restart contains none of the failed attempt's text, reasoning, preview or replay; three failed attempts exhaust; cancelling during a wait sends no further request.
**Files** — Modify: `src/anthropic.rs`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 8: Anthropic summarisation
**Changes** — `summarise` per the spec's summarisation bullet, using the Task 7 retry path.
**Tests** (`src/anthropic.rs`, loopback fixture) — Red first: exact summary request body; text deltas form the summary with usage; each failure case errors; a dropped stream restarts.
**Files** — Modify: `src/anthropic.rs`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 9: Cache Anthropic prompts
**Changes** — Step requests carry the breakpoints and TTL per the spec's Caching paragraph. `[anthropic] cache_ttl` is validated like other tables and passed to the adapter at construction.
**Tests**
- `src/config/settings.rs`: Red first: default, `1h`, invalid value and unknown `[anthropic]` key.
- `src/anthropic.rs`: breakpoints on the system block and the newest message's last block, for a fresh request and after a tool result; `ttl` omitted for `5m`, present for `1h`; summary requests have none.
**Files** — Modify: `src/config/settings.rs`, `src/config.rs`, `src/anthropic.rs`, `src/provider.rs` or `src/main.rs` (construction)
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 10: Replay history across providers
**Changes** — The shared foreign-entry helper and both adapters' encoding per the spec's Cross-provider rendering paragraph, replacing the foreign-replay rejection. Top-level `cross_provider_reasoning` is validated and passed to both adapters at construction.
**Tests**
- `src/model.rs` (or the helper's home): Red first: reasoning then commentary then tool call and result; empty reasoning omitted; `drop` omits reasoning; final-message entries give text.
- `src/openai.rs` and `src/anthropic.rs`: an Anthropic entry replays into an OpenAI request as assistant text plus `function_call`/`function_call_output` with the `toolu_*` ID, and an OpenAI entry replays into an Anthropic request as text plus `tool_use`/`tool_result` with the `call_*` ID, merged into alternating roles; own-provider replay is unchanged.
- `src/runtime.rs`: a session with a completed OpenAI step is saved, reopened and continued on an Anthropic selection (scripted or fixture model), and the restored foreign entry reaches the request.
- `src/config/settings.rs`: default, `drop`, invalid value.
**Files** — Modify: `src/model.rs`, `src/openai.rs`, `src/anthropic.rs`, `src/config/settings.rs`, `src/config.rs`, `src/provider.rs` or `src/main.rs`
**Verification** — Overall test and clippy commands: all pass, clean.

### Task 11: Live Anthropic smoke test
**Changes** — An ignored live test mirroring `live_sol` on `anthropic/claude-opus-5-5`, plus a `live-test-anthropic` justfile recipe.
**Tests** (`src/anthropic.rs`, ignored) — The same run, cancellation and follow-up checks as `live_sol`.
**Files** — Modify: `src/anthropic.rs`, `justfile`
**Verification** — `just live-test-anthropic`: passes with `ANTHROPIC_API_KEY` set; the overall test command skips it.

### Task 12: Document the Anthropic provider
**Changes** — README: provider-neutral intro, both keys optional with the required-key rule, the two models, `[anthropic] cache_ttl`, `cross_provider_reasoning`, picker hiding, pricing identity, and live test (`README.md:3,13-22,109-142,193-199,235,242-274`).
**Tests** — None: documentation only.
**Files** — Modify: `README.md`
**Verification** — `rg -n "OPENAI_API_KEY" README.md` shows each mention alongside `ANTHROPIC_API_KEY` where both apply.
