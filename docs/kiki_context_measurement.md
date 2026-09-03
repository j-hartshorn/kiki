# Model catalog, context measurement, and cache-hit reporting plan

## Status

Implementation plan only. This document does not change runtime behaviour or assert unverified Terra or Luna limits or prices.

## Goal

Replace Kiki's single `FIXED_MODEL: &str` with a small, authoritative OpenAI model catalog that supports:

- `gpt-5.6-sol` (the default),
- `gpt-5.6-terra`, and
- `gpt-5.6-luna`;

and records each model's service tier, context capacity, output limit, and versioned pricing schedule. Use the selected model's metadata to add latest-request context usage and cache-hit percentage to the existing session footer without confusing either value with cumulative token totals.

## Current behaviour

- `src/config/constants.rs` contains only `FIXED_MODEL: &str = "gpt-5.6-sol"`.
- `src/openai.rs` uses that string and hard-codes `service_tier: "default"`.
- `src/pricing.rs` has one Sol/default schedule and rejects every other model.
- `src/transcript.rs` folds all reliable attempts into session-wide `↑`, `↓`, `R`, `W`, `r`, and `Σ` values.
- The cumulative `Σ` value is not current context occupancy: context replayed in multiple requests is counted multiple times.
- Provider usage arrives only when an attempt settles, so Kiki cannot report authoritative live context usage while a response is streaming.

## Product decisions

### Supported-model selection

Introduce a closed catalog of the three supported models. Select a model at startup with `KIKI_MODEL`, accepting the exact OpenAI IDs above. An unset variable selects Sol. An unknown, empty, or non-Unicode value becomes a bounded startup configuration problem and disables submission, following the existing API-key and path configuration pattern.

This provides a usable path for Terra and Luna without adding a TUI selector in the same change. A later selector can consume the same catalog. The selected model is immutable for the process lifetime, which prevents one `ModelHistory` from mixing models with incompatible replay or accounting semantics.

### Authoritative model metadata

Each catalog entry must contain:

- API model ID;
- service tier (`default` initially);
- context-window capacity in tokens;
- maximum output tokens;
- model-reference URL and effective/version date;
- pricing schedule and pricing-reference URL/version;
- any pricing threshold that changes all-request rates.

Do not infer context capacity from a pricing threshold. In particular, Sol's 272,000-input-token pricing threshold is not its context limit. The existing live-agent specification identifies Sol's context window as 1,050,000 tokens and maximum output as 128,000 tokens; verify those values again when implementing the catalog.

Before implementation, obtain Terra and Luna limits and all three current price schedules from official OpenAI model and pricing references. Record exact URLs and effective dates. Do not copy prices from examples, search snippets, third-party sites, or infer them from Sol. If OpenAI does not publish a required cache-write category for a model, resolve that contract explicitly rather than treating it as zero.

### Footer semantics

Keep existing fields cumulative across all reported session attempts:

- `↑`: input tokens;
- `↓`: output tokens;
- `R`: cached input tokens;
- `W`: cache-write input tokens;
- `r`: reasoning output tokens (a subset of output);
- `Σ`: provider-reported total tokens;
- `$`: local estimated cost.

Add two fields based only on the most recently settled attempt:

- `C<percent>/<capacity>`: latest request's provider-reported `total_tokens` as a percentage of the selected model's context-window capacity, followed by that capacity;
- `H<percent>`: latest request's `cached_input_tokens / input_tokens * 100`.

Example shape (illustrative numbers only):

```text
↑330k ↓12k R210k W40k r8k Σ342k C32%/1.05m H87.3% $4.21
```

`C` is the latest completed request's measured input-plus-output usage percentage and the configured total context capacity. For example, `C32%/274k` means the latest request used 32% of a 274,000-token context window. It is not cumulative session usage, a live streaming estimate, a guarantee of the next request's size, or tokens remaining after reserving a future response. `H` is a prompt-cache read ratio. Cache writes and ordinary uncached input are misses, and reasoning is not added again.

Use the latest chronological attempt, not merely the latest attempt with known usage. If the newest attempt has unavailable usage, omit `C` and `H` rather than silently displaying an older request as current. While a new attempt is active but has not settled, retain the latest settled measurement; update or remove it when that attempt settles.

### Pricing semantics

Pricing remains a local estimate, not billing. For each attempt:

1. Resolve the pricing schedule by the usage model ID and returned service tier.
2. Require all categories needed by that schedule.
3. Validate cached plus cache-write input does not exceed total input.
4. Validate reasoning does not exceed output and total equals input plus output.
5. Compute uncached input as input minus cached input minus cache-write input.
6. Select the short- or long-context rate set using that model's pricing threshold, if it has one.
7. Charge output once; reasoning remains an informational subset.

Unknown models, unsupported tiers, missing categories, inconsistent usage, and arithmetic overflow remain unpriceable. Run estimates remain `Complete`, `LowerBound`, or `Unavailable`; opaque SDK retries can still make a displayed estimate a lower bound. Footer presentation continues to omit unavailable cost and does not invent zero.

## Proposed design

### 1. Model catalog

Create a model-focused module, preferably `src/config/models.rs`, rather than expanding the unrelated operational constants file.

Conceptual types:

```rust
pub(crate) struct ModelConfiguration {
    pub id: &'static str,
    pub service_tier: &'static str,
    pub context_window_tokens: u64,
    pub max_output_tokens: u64,
    pub reference: VersionedSource,
    pub pricing: PricingSchedule,
}

pub(crate) struct VersionedSource {
    pub version: &'static str,
    pub url: &'static str,
}

pub(crate) struct PricingSchedule {
    pub reference: VersionedSource,
    pub short_context: TokenRates,
    pub long_context: Option<LongContextRates>,
}

pub(crate) struct LongContextRates {
    pub threshold_input_tokens: u64,
    pub rates: TokenRates,
}

pub(crate) struct TokenRates {
    // USD nanodollars per token, preserving exact integer arithmetic.
    pub uncached_input_nanos: u64,
    pub cached_input_nanos: u64,
    pub cache_write_nanos: u64,
    pub output_nanos: u64,
}
```

Expose narrow lookups rather than the catalog representation:

```rust
pub(crate) const DEFAULT_MODEL_ID: &str = "gpt-5.6-sol";
pub(crate) fn model_by_id(id: &str) -> Option<&'static ModelConfiguration>;
pub(crate) fn default_model() -> &'static ModelConfiguration;
```

The catalog must reject duplicate IDs and invalid metadata in tests. Keep operational limits such as Arena memory and tool concurrency in `src/config/constants.rs`.

### 2. Startup configuration and selection

Extend `StartupInput`, `StartupConfiguration`, and `StartupSnapshot` to carry the selected model. Add a `ModelUnknown`/`ModelInvalidUnicode` startup problem with bounded diagnostics. Never place credentials in model diagnostics.

The startup configuration view should show at least:

```text
Model: gpt-5.6-terra
Context window: <verified token count>
Maximum output: <verified token count>
Service tier: default
```

Pass the selected `&'static ModelConfiguration` through runtime construction into `OpenAiModel` and the transcript. Do not let `openai.rs`, `pricing.rs`, or `transcript.rs` independently look up a different default after startup.

### 3. OpenAI adapter

Change `OpenAiModel` from a model-name field to the selected model configuration (or the exact request fields copied from it). Build request `model` and `service_tier` from that configuration.

Continue assigning the requested model ID to `ModelUsage`, because the current wire response accounting does not supply a separate response model. Preserve the returned service tier. Add tests for each supported model request and ensure a selected tier mismatch makes pricing unavailable rather than being silently priced as default.

Do not add automatic fallback between Sol, Terra, and Luna. A fallback would mix model semantics and make context/pricing attribution ambiguous.

### 4. Pricing refactor

Move model-specific schedules into the catalog and make `src/pricing.rs` a model-neutral calculator. Replace the single `GPT_5_6_SOL_DEFAULT_2026_08_27` branch with catalog resolution.

The returned `PricingIdentity` should continue to include model, service tier, version, and source. Its values come from the resolved catalog entry. `estimate_run_cost` may sum different supported schedules defensively, but a normal process session should contain only the startup-selected model. If preserving one identity on a multi-model aggregate is impossible, reject a mixed-model aggregate rather than label it with the wrong model; add a dedicated error/estimate state if required.

Populate a verified implementation table before coding:

| Model | Context window | Max output | Short rates (uncached/cached/write/output) | Long threshold and rates | Source/version |
|---|---:|---:|---|---|---|
| `gpt-5.6-sol` | verify 1,050,000 | verify 128,000 | verify existing $4/$0.40/$5/$20 per million | verify 272,000 and $8/$0.80/$10/$30 | official OpenAI docs |
| `gpt-5.6-terra` | research required | research required | research required | research required or none | official OpenAI docs |
| `gpt-5.6-luna` | research required | research required | research required | research required or none | official OpenAI docs |

No `research required` placeholder may survive in executable configuration.

### 5. Current-request metrics

Add a pure calculation separate from `SessionAccounting`:

```rust
struct CurrentRequestMetrics {
    used_context_tokens: Option<u64>,
    context_window_tokens: u64,
    cache_hit_basis_points: Option<u64>,
}
```

Rules:

- Resolve only from the latest chronological settled attempt.
- Both `KnownUsage` and `KnownUsageWithUntrackedRetries` are eligible.
- `UsageUnavailable` produces no current metrics.
- Use `total_tokens` for `C`.
- Use cached input divided by input for `H`.
- Omit `H` when input is zero or either field is absent.
- Omit invalid metrics when cached input exceeds input.
- Do not clamp context usage at capacity; an over-capacity report should remain visible for diagnosis.
- Use checked integer arithmetic and basis points (or a similarly explicit integer scale), not floating point.

Keep cumulative accounting's saturating sum behaviour unchanged unless separately reviewed.

### 6. Projection and footer formatting

Change footer construction to receive both the disjoint reliable attempt stream and the selected model configuration. Keep cumulative folding and latest-attempt selection as separate functions so a future refactor cannot accidentally compare cumulative `Σ` with context capacity.

Format context as `C<whole-percent>/<capacity>`, for example `C32%/274k`. Compute the percentage from latest `total_tokens / context_window_tokens` with checked integer arithmetic and deterministic nearest-whole-percent rounding. Do not clamp values above 100%. Format the capacity with lowercase `k` or `m` and three significant figures for values of at least 1,000 tokens; keep smaller values as exact integers. Representative formatting boundaries must include `1.23k`, `12.3k`, `123k`, and `1.05m`, including carry across the `k`/`m` boundary. Do not include the latest raw used-token count in `C`; the percentage is the usage value and the number after `/` is the configured total capacity.

Format cache hit percentage with at most one decimal place. Add focused formatter tests around unit, significant-figure, carry, and percentage-rounding boundaries. Values must pass through the existing display normalisation path.

Recommended field order:

```text
↑ ↓ R W r Σ C H $
```

The existing narrow-terminal behaviour truncates the left side, preserving current context, cache hit rate, and cost. Verify that all output remains one display row for ASCII and Unicode ellipsis cases.

### 7. Documentation

Update `README.md` to:

- list Sol, Terra, and Luna and explain `KIKI_MODEL` plus the Sol default;
- explain that model limits and prices are versioned local metadata;
- define `C` as latest settled request total over selected capacity;
- define `H` as latest settled request cached input over input;
- distinguish `C` from cumulative `Σ`;
- retain the estimated-cost and opaque-retry caveats.

Historical specifications under dated directories should remain historical. Add a short note only if the repository convention requires supersession links; do not rewrite old decisions as though they originally supported multiple models.

## Test plan

### Catalog and configuration

- The catalog contains exactly one entry for each supported ID and defaults to Sol.
- Every model has nonzero context/output limits, a valid service tier, complete rates, and nonempty versioned official sources.
- Context capacity is at least maximum output.
- Pricing thresholds, when present, do not exceed context capacity.
- Unset `KIKI_MODEL` selects Sol.
- Each exact supported ID selects its model.
- Empty, unknown, and non-Unicode values disable submission with bounded diagnostics.
- Startup rendering shows selected model, tier, context capacity, and output limit.

### OpenAI requests

- Fixtures for Sol, Terra, and Luna contain the selected model ID and configured service tier.
- No model fallback occurs after provider or transport failure.
- Usage is tagged with the selected model and returned tier.

### Pricing

For every model and each applicable rate band:

- uncached, cached, cache-write, and output-only examples produce exact USD nanodollar totals;
- the threshold boundary and first token above it choose the intended rates;
- reasoning is not charged twice;
- inconsistent and incomplete usage is rejected;
- wrong/unknown model and service tier are unpriceable;
- mixed attempts preserve complete/lower-bound/unavailable semantics;
- pricing identity reports the selected schedule's exact model, tier, version, and source.

Retain the current Sol regression totals so the refactor cannot silently change existing estimates.

### Context and cache metrics

Given two reported requests, prove cumulative and current values differ correctly. For example:

```text
request 1: input=100, cached=20, output=10, total=110
request 2: input=200, cached=150, output=20, total=220
```

The cumulative fields are `↑300 ↓30 R170 Σ330`, while current fields are `C<rounded percentage>/<three-significant-figure selected capacity> H75%`.

Also cover:

- latest attempt wins;
- latest unavailable attempt suppresses stale `C` and `H`;
- known usage with opaque retries remains usable for these metrics;
- missing total suppresses only `C`;
- missing input/cache or zero input suppresses only `H`;
- cache writes do not count as hits;
- cached input greater than input is invalid;
- over-capacity totals are displayed without clamping;
- context percentage uses deterministic nearest-whole-percent rounding;
- context capacity uses lowercase `k`/`m` with three significant figures, including unit-boundary carry;
- active-to-settled and retry replacement preserve chronology;
- a new user prompt in the same process uses the latest request rather than cumulative totals.

### TUI

- Full-width footer contains cumulative values, `C`, `H`, and cost in the chosen order.
- Narrow widths retain a single row and preserve the rightmost metrics according to existing truncation policy.
- Footer remains absent without metrics and in configuration-only mode.
- Live generation shows the prior settled measurement until new usage settles.
- New unavailable usage removes current metrics.
- Scroll position and history height remain unchanged.

## Implementation sequence

1. Research and record official Terra/Luna model limits and all model prices; re-verify Sol.
2. Add catalog types, entries, invariants, and exact-rate tests.
3. Add `KIKI_MODEL` resolution and expose selected metadata in `StartupSnapshot`.
4. Pass selected configuration into `OpenAiModel`, runtime, and transcript; add three-model request fixtures.
5. Refactor pricing into catalog lookup while retaining Sol regression tests.
6. Implement pure latest-request context and cache-hit calculations.
7. Add `C` and `H` to footer projection and TUI tests.
8. Update README and configuration documentation.
9. Run formatting, Clippy, the full test suite, and the existing live test manually for at least the default Sol model; run controlled smoke requests for Terra and Luna if credentials/account access permit.

## Acceptance criteria

- A user can select Sol, Terra, or Luna at startup; Sol remains the no-configuration default.
- The OpenAI request, usage identity, context denominator, and price schedule all derive from one selected catalog entry.
- All three models have official, versioned limits and complete exact pricing data in executable configuration.
- Existing cumulative footer totals and Sol cost estimates remain correct.
- `C` reports the latest settled request as a whole percentage followed by `/` and the selected model capacity formatted with lowercase `k` or `m` at three significant figures.
- `H` reports only the latest settled request's cache-read share of input.
- Missing or invalid usage is omitted rather than represented as zero or stale current data.
- Cost remains explicitly documented as a local estimate that can understate opaque retries.
- No TUI selector, automatic model fallback, local tokenizer, or speculative in-flight context estimate is introduced.
