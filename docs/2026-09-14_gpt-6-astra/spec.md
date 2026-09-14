# Spec: GPT-6 Astra model support

## Context

Kiki has a closed model catalogue containing `openai/gpt-5.6-sol`, `openai/gpt-5.6-terra`, and `openai/gpt-5.6-luna`. Sol remains the default. Every catalogue entry owns its supported thinking levels, context and output limits, Standard-tier pricing, and pricing provenance; the catalogue drives startup/settings validation, the interactive model and thinking pickers, OpenAI request construction, subagent model overrides, context reporting, and local cost estimates.

This change adds the active OpenAI GPT-6 Astra model under the canonical Kiki identity `openai/gpt-6-astra` and provider wire identity `gpt-6-astra`. Astra is a flagship frontier model specialized in computer/browser use, multi-step software engineering, scientific reasoning, and document artifact creation. It has the same 1,050,000-token context window, 128,000-token maximum output, and 272,000-input-token long-context pricing boundary as the existing models, but has its own rates and thinking policy: `low`, `medium`, `high`, `xhigh`, and `max`, defaulting to `low`; `none` is unsupported.

A credentialed compatibility probe on 14 September 2026 tested the request shape Kiki already emits. A non-streaming request with `service_tier: "default"`, nested `reasoning.effort: "low"`, `reasoning.summary: "auto"`, `reasoning.context: "all_turns"`, `include: ["reasoning.encrypted_content"]`, `store: false`, and Kiki's function-tool shape completed with HTTP 200. The response reported model `gpt-6-astra`, service tier `default`, and the existing Responses API usage field names `input_tokens_details.cache_write_tokens`, `input_tokens_details.cached_tokens`, and `output_tokens_details.reasoning_tokens`. A smaller probe using the same nested reasoning object without `context` also succeeded. These observations supersede preliminary metadata that described a `standard` returned tier, a dated returned alias, top-level reasoning parameter names, and `cache_creation_input_tokens`.

## Goals

- Add `openai/gpt-6-astra` above Sol in the authoritative model catalogue without changing `openai/gpt-5.6-sol` as the default.
- Submit Astra to OpenAI as `gpt-6-astra` using Kiki's existing Responses API request schema and `default` service-tier wire value.
- Define Astra's exact supported thinking sequence as `low`, `medium`, `high`, `xhigh`, and `max`, with `low` as its default, while preserving `none` for the GPT-5.6 models.
- Price Astra Standard/default-tier token usage with the verified 4 September 2026 short- and long-context schedules and existing integer nanodollar arithmetic.
- Make Astra available everywhere the shared catalogue is used, including foreground selection, settings, interactive controls, inherited child selection, and explicit subagent model overrides.
- Preserve exact model lookup, existing usage validation, unavailable-cost behavior, and all Sol, Terra, and Luna behavior.
- Document account eligibility, non-Standard modes, and action surcharges without claiming Kiki estimates or controls them.

## Non-goals

- Making Astra the built-in default or migrating existing user/project settings to it.
- Adding separate Batch, Flex, or Fast-mode catalogue entries, request controls, or pricing schedules. Batch and Flex are documented at 50% of Standard rates and Fast at 200%, but Kiki continues to request only `default` and price only matching returned `default` usage.
- Restricting Astra as a subagent model. Catalogue models remain uniformly selectable; a future restriction can add an explicit capability and policy when required.
- Implementing computer-use or browser-execution tools. Kiki continues to expose only `execute_python` and its workspace-rooted host functions.
- Estimating separately billed computer/browser action surcharges or presenting token estimates as complete tool billing.
- Adding model-specific rate-limit admission, latency warnings, automatic fallback, or retries beyond existing provider behavior for `xhigh` and `max`.
- Granting or bypassing account eligibility. Fast-mode limits and Daybreak Blue / Trusted Access requirements remain provider/account concerns.
- Adding aliases such as `openai/gpt-6-astra-2026-09-03` as user-selectable model IDs.
- Changing model discovery from a closed static catalogue to a dynamic provider query.
- Reworking the Responses API adapter for preliminary field names that the live compatibility probe did not observe.
- Requiring a credentialed Astra smoke test in the ordinary or opt-in acceptance suite.

## Key discoveries

- `src/config/models.rs` owns `ModelConfiguration`, model-specific thinking levels, token capacities, pricing identity/rates, `CATALOGUE`, `DEFAULT_MODEL`, and exact `model_by_id` lookup. `CATALOGUE` currently has a fixed length of three and orders Sol, Terra, then Luna.
- Catalogue-wide tests currently assume every model shares `OPENAI_THINKING_LEVELS`, defaults to `medium`, and uses the same limits, pricing date, and source. Astra invalidates the thinking/default/date assumptions, so tests must separate universal invariants from family-specific facts.
- `src/openai.rs` strips the `openai/` qualifier before submission. No Astra-specific wire-ID field is necessary: `openai/gpt-6-astra` naturally becomes `gpt-6-astra`.
- The OpenAI adapter already emits the Astra-compatible nested request shape: `reasoning.effort`, `reasoning.summary: "auto"`, `reasoning.context: "all_turns"`, and `include: ["reasoning.encrypted_content"]`.
- The 14 September live probes returned `service_tier: "default"`, not `standard`; therefore both `ModelConfiguration.service_tier` and `PricingIdentity.service_tier` should remain the exact wire/accounting value `default`. “Standard” is the commercial pricing label only.
- The probes returned stable model ID `gpt-6-astra`, not the preliminary dated alias `gpt-6-astra-2026-09-03`. Kiki's accounting deliberately records the selected qualified catalogue identity, so returned provider aliases do not drive pricing lookup.
- The observed usage object used Kiki's existing fields: `input_tokens`, `input_tokens_details.cached_tokens`, `input_tokens_details.cache_write_tokens`, `output_tokens`, `output_tokens_details.reasoning_tokens`, and `total_tokens`. No deserializer alias is needed from current evidence.
- `src/pricing.rs` chooses short rates when `input_tokens <= long_context_threshold` and long rates otherwise, then applies the chosen band to the entire request. This exactly represents Astra's non-inclusive long-context threshold: 272,000 uses short rates and 272,001 uses long rates.
- Pricing subtracts cached plus cache-write tokens from total input to derive ordinary uncached input. Reasoning output is already included in output and is validated, not charged a second time.
- `src/subagents.rs` resolves explicit child overrides through `model_by_id`; adding Astra to the shared catalogue therefore makes it explicitly selectable and also permits ordinary inherited Astra selection without another code path.
- Startup and settings remediation strings in `src/config.rs` and `src/config/settings.rs` enumerate the three existing qualified IDs and must include Astra.
- Model and thinking pickers iterate the catalogue and each model's own thinking slice, so their production paths should pick up Astra automatically, but deterministic tests and fixtures may assume three models or `medium` defaults.
- Compact capacity labels are lowercase throughout Kiki (`1.05m`, `128k`, `272k`); Astra should follow that convention rather than introducing uppercase suffixes.

## Proposed approach

### Catalogue identity and capabilities

Add a dedicated `ASTRA_THINKING_LEVELS` static slice containing `low`, `medium`, `high`, `xhigh`, and `max`. Keep `OPENAI_THINKING_LEVELS` unchanged for Sol, Terra, and Luna so `none` remains valid for those models. Add an `ASTRA` `ModelConfiguration` with qualified ID `openai/gpt-6-astra`, default thinking `low`, service tier `default`, 1,050,000 context tokens, 128,000 maximum output tokens, and the existing lowercase compact labels.

Place `&ASTRA` first in `CATALOGUE`, followed by Sol, Terra, and Luna. Keep `DEFAULT_MODEL` pointing to Sol. Exact lookup accepts only `openai/gpt-6-astra`; the unqualified provider ID, case variants, whitespace variants, and dated aliases remain invalid configuration values.

The catalogue remains the single policy surface for foreground and subagent selection. Astra receives no special subagent flag or branch. An explicit `model="openai/gpt-6-astra"` child override and inheritance from an Astra parent resolve through existing behavior.

### Request and response compatibility

Use the existing provider qualifier stripping and request construction unchanged. The Astra request sends `model: "gpt-6-astra"`, `service_tier: "default"`, `store: false`, nested reasoning fields, encrypted reasoning inclusion, and the normal function-tool schema. A deterministic request-body test pins these properties and confirms that a valid Astra selection cannot carry unsupported `none` effort.

Add a sanitized Astra response fixture using the observed stable returned model, `default` service tier, and existing usage-detail names. The adapter continues attributing usage to the immutable model selection captured for the attempt rather than accepting arbitrary response model text as a pricing identity. Preserve absence semantics: missing provider usage or required detail fields keeps cost unavailable rather than inventing zero.

### Pricing and context behavior

Use pricing identity version `2026-09-04`, source `https://openai.com/api/pricing/`, and service tier `default`. Store rates as integer nano-dollars per token:

| Context band | Uncached input | Cached input | Cache write | Output |
|---|---:|---:|---:|---:|
| Input tokens `<= 272,000` | `10_000` | `1_000` | `12_500` | `50_000` |
| Input tokens `> 272,000` | `20_000` | `2_000` | `25_000` | `75_000` |

The threshold is based on total provider-reported input, including cached and cache-write input, and crossing it reprices the entire request. Existing estimator semantics already implement that rule. Reasoning and tool-call arguments are billed within output tokens; tool results are billed within later input tokens. They require no extra token category.

Kiki's displayed estimate remains a local direct-API Standard/default-tier token estimate. It excludes any separately billed computer/browser action surcharge and does not estimate Batch, Flex, or Fast pricing. A response whose `service_tier` is absent or differs from `default` remains unpriceable under this entry.

### Settings, interaction, and documentation

Extend hard-coded supported-model remediation text and settings fixtures to include Astra. Test environment, user TOML, project TOML, picker, direct slash-command, persistence, model switching, and Shift-Tab behavior with Astra's distinct default and lack of `none`. Switching to Astra restores its accepted per-model value or `low`; switching back preserves the other model's independent value, including `none` where supported.

Update README catalogue/setup examples, settings tables, cost language, controls, and footer descriptions. State that Astra is first in the picker but Sol remains the default. Explain the account and latency constraints for higher effort levels without turning them into local admission rules.

## Key decisions & tradeoffs

- *Use observed wire values*: Store and send `default`, because two successful live probes returned `default`. Keep “Standard” only as the human pricing-tier description.
- *Reuse the current reasoning schema*: The exact nested Kiki shape succeeded against Astra, avoiding speculative model-specific request configuration.
- *No cache-field compatibility branch*: The live response used `cache_write_tokens`; adding an unobserved alias would broaden accounting semantics without evidence. A later real payload can justify an alias with a fixture.
- *Selected identity controls accounting*: Preserve the current qualified selection as `ModelUsage.model`; do not make a returned stable or dated provider alias part of the user catalogue or pricing lookup.
- *Separate Astra thinking levels*: Do not remove `none` globally or send it and rely on a provider error. Invalid Astra thinking is rejected by configuration and interaction before a request is built.
- *Uniform subagent availability*: Keep the catalogue simple. Astra is available for explicit and inherited children just like the other models.
- *Catalogue order is not default policy*: Put Astra above Sol for visibility while retaining `DEFAULT_MODEL = &SOL`, so existing startup behavior does not change.
- *Model-specific tests over weakened assertions*: Preserve exact GPT-5.6 expectations and add exact Astra expectations rather than reducing all tests to loose nonempty checks.
- *Token-only cost honesty*: Include verified token rates but explicitly exclude action surcharges and non-default modes instead of pretending the displayed amount is complete provider billing.
- *Lowercase compact labels*: Follow existing terminal conventions and avoid a global display-format change as part of model onboarding.

## Risks / unknowns

- OpenAI may later return the dated `gpt-6-astra-2026-09-03` alias described in preliminary metadata. Current accounting remains stable because it uses the selected catalogue identity, but response-validation changes must not accidentally reject a legitimate alias.
- `xhigh` and `max` may fail for accounts without the required rate-limit tier. Kiki validates model capability, not account entitlement, so provider diagnostics remain the source of truth.
- Pricing pages can change without versioned URLs. The dated spec and `PricingIdentity.version` preserve the point-in-time schedule but do not automatically detect later price changes.
- Computer/browser action surcharges can make Kiki's displayed token estimate materially lower than an external bill if such actions are introduced later.
- The preliminary restriction involving `store: false` and persistent logging hooks was not exercised because Kiki does not request persistent logging hooks. A future logging feature must test that combination independently.
- The live probes were non-streaming for concise inspection, while Kiki operates streaming requests. Existing models and supplied metadata establish streaming support, and deterministic Astra stream fixtures must cover Kiki's parser without making credentials part of the test suite.

## Dependencies

- OpenAI model documentation: `https://platform.openai.com/docs/models/gpt-6-astra`.
- OpenAI pricing documentation: `https://openai.com/api/pricing/`, effective 4 September 2026 and verified 14 September 2026.
- OpenAI reasoning guide: `https://platform.openai.com/docs/guides/reasoning`.
- OpenAI Responses API reference: `https://platform.openai.com/docs/api-reference/responses`.
- Existing closed-catalogue, settings, OpenAI adapter, pricing, transcript, TUI, and subagent selection infrastructure.
- Credentialed 14 September 2026 compatibility probes summarized in this spec; credentials, prompts beyond minimal probe text, and response IDs are intentionally not retained.

## Plan

### Acceptance criteria

1. `openai/gpt-6-astra` appears above Sol in the model catalogue and interactive picker, while a fresh Kiki still defaults to `openai/gpt-5.6-sol`.
2. Selecting Astra submits `gpt-6-astra`, `service_tier: "default"`, and Kiki's existing nested reasoning schema to the Responses API.
3. Astra supports exactly `low`, `medium`, `high`, `xhigh`, and `max`, defaults to `low`, and rejects `none` locally; GPT-5.6 models retain their existing thinking behavior.
4. Astra Standard/default token usage is priced using the exact verified rates. Exactly 272,000 input tokens uses short rates; 272,001 uses long rates for the entire request.
5. Missing usage, missing cache-write data, inconsistent breakdowns, and non-`default` returned tiers retain existing unavailable/error semantics and never become invented zero cost.
6. Astra is selectable for foreground and subagent work, including explicit child overrides and normal inherited selection.
7. Settings files, environment selection, `/model`, `/thinking`, persistence, footer identity, context display, and pricing display handle Astra without regressions.
8. Documentation identifies the estimate as Standard/default token-only pricing and excludes action surcharges and other service modes.
9. Sol, Terra, and Luna metadata, defaults, request behavior, settings, and pricing remain unchanged.
10. The complete credential-free deterministic suite, formatting, checks, and Clippy pass; no live Astra test is required for acceptance.

### Overall validation

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3|getrandom)' src --glob '*.rs')" = "src/tools.rs"
```

The probes used to settle wire compatibility are exploratory evidence, not an acceptance dependency. Tests must use sanitized local fixtures and require no OpenAI credential or network access.

### Planning context

Implementation follows the repository worktree policy: create a dedicated `work/<slug>` branch and matching `worktrees/<slug>` worktree, implement and test there, then merge only after review. This specification is planning documentation and may be committed independently on `main`.

### Task 1: Add Astra to the authoritative catalogue

**Changes**
- Add `ASTRA_THINKING_LEVELS` with `low`, `medium`, `high`, `xhigh`, and `max`.
- Add `ASTRA` with qualified ID, `default` service tier, `low` default thinking, exact token limits, compact labels, pricing provenance, threshold, and rates.
- Expand and reorder `CATALOGUE` to Astra, Sol, Terra, Luna.
- Keep `DEFAULT_MODEL` pointing to Sol.
- Refactor catalogue tests into universal invariants plus exact GPT-5.6 and Astra assertions.

**Tests**
- Catalogue contains exactly four unique IDs in the required order.
- Exact Astra lookup succeeds; unqualified, case-changed, whitespace, and dated variants fail.
- Astra accepts each of its five levels and rejects `none`; existing models still accept their documented sequence.
- Astra limits, labels, provenance, and all eight rates match this spec.

**Files**
- Modify: `src/config/models.rs`.

**Verification**
- `cargo test config::models::`.

### Task 2: Extend startup and persisted settings

**Changes**
- Add Astra to startup/settings remediation that enumerates supported qualified IDs.
- Ensure built-in resolution chooses Astra's `low` only when Astra is selected and no higher-precedence accepted value exists.
- Preserve independent per-model thinking values through user/project TOML and environment precedence.
- Keep existing settings file shape and migration behavior unchanged.

**Tests**
- Resolve Astra from environment, user settings, and project settings.
- Validate precedence and Astra's default `low`.
- Reject Astra `none` with bounded constructive remediation while retaining `none` for GPT-5.6 entries.
- Save and reload Astra model and thinking defaults without rewriting unrelated settings.
- Update exact diagnostic fixtures to list all four IDs without leaking hostile input or credentials.

**Files**
- Modify: `src/config.rs`, `src/config/settings.rs`.

**Verification**
- `cargo test config::`.

### Task 3: Pin Astra request and response compatibility

**Changes**
- Reuse qualifier stripping so the provider receives `gpt-6-astra`.
- Reuse existing request fields and service tier without a model-ID branch.
- Add sanitized request assertions and completed streaming response fixtures for Astra.
- Preserve selected qualified identity in attempt accounting and existing usage-detail deserialization.

**Tests**
- Astra request includes `default`, `store: false`, streaming, tool configuration, `reasoning.effort: low`, `summary: auto`, `context: all_turns`, and encrypted reasoning inclusion.
- Fixture with returned `gpt-6-astra` and observed usage fields produces qualified Astra accounting.
- A fixture using a dated returned model alias, if retained as forward-compatibility coverage, does not change selected-model pricing attribution.
- Missing or malformed usage remains unavailable/failing under existing typed semantics.

**Files**
- Modify: `src/openai.rs`.

**Verification**
- `cargo test openai::`.

### Task 4: Add Astra pricing boundary coverage

**Changes**
- Rely on catalogue-driven pricing lookup; no separate estimator branch should be required.
- Add exact Astra calculations for short and long bands and service-tier matching.
- Confirm cached and cache-write input partition total input and reasoning remains included in output rather than separately charged.

**Tests**
- Exact representative Astra cost in each band.
- Boundary tests at 272,000 and 272,001 input tokens.
- Fully cached, cache-write, and mixed input cases.
- Reasoning-heavy output is charged once at the output rate.
- Missing category, inconsistent total, inconsistent breakdown, arithmetic overflow, unknown model, and wrong service tier retain current behavior.

**Files**
- Modify: `src/pricing.rs`.

**Verification**
- `cargo test pricing::`.

### Task 5: Integrate interactive and subagent selection

**Changes**
- Let catalogue iteration place Astra first in `/model`.
- Ensure `/thinking` and Shift-Tab use Astra's five-level slice and `low` default.
- Preserve model-specific thinking when switching between Astra and GPT-5.6 models.
- Keep shared `model_by_id` behavior for explicit and inherited subagent selections; add no restriction flag.
- Update fixtures that assume a three-model catalogue or universal `medium` default.

**Tests**
- Picker order, direct case-insensitive selection, persistence, and footer identity for Astra.
- `none` is absent from Astra's thinking picker and direct `/thinking none` is rejected for Astra.
- Cycling wraps across Astra's ordered levels.
- Switching models restores independent levels.
- Explicit Astra child override succeeds with accepted Astra thinking; inherited Astra child selection succeeds; existing child overrides remain unchanged.

**Files**
- Modify as required: `src/tui.rs`, `src/transcript.rs`, `src/subagents.rs`, `src/tui/subagents.rs`.

**Verification**
- Run focused TUI, transcript, and subagent tests, then the complete suite.

### Task 6: Update user documentation and finish verification

**Changes**
- Add Astra to README introduction, setup, supported IDs, and TOML examples.
- Document Astra's purpose, `low` default, unsupported `none`, and higher-effort account/latency caveats.
- Update privacy/cost text for four models, the 4 September 2026 Astra pricing identity, token-only estimate scope, and excluded service modes/action surcharges.
- Preserve Sol as the documented default and avoid implying that selecting Astra grants provider eligibility.

**Tests and review**
- Search for hard-coded three-model lists, catalogue lengths, model IDs, and universal thinking assumptions.
- Check README examples against accepted configuration syntax.
- Run overall validation exactly as listed above.

**Files**
- Modify: `README.md` and any directly affected checked-in documentation.

**Verification**
- `rg 'gpt-5\.6-(sol|terra|luna)|CATALOGUE.*3|medium' src README.md` and review every model-list/default assumption.
- Complete formatting, check, test, Clippy, and dependency-boundary commands pass.
