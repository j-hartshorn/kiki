# Spec: Model Catalogue, Pricing, and Context Measurement

## Context

Kiki currently hard-codes `FIXED_MODEL: &'static str = "gpt-5.6-sol"` in `src/config/constants.rs:3`, restricts pricing in `src/pricing.rs:36` to a single Sol schedule, and reports only cumulative session-wide token totals (`↑`, `↓`, `R`, `W`, `r`, `Σ`) in the footer.

This creates three key limitations:
1. Users cannot select other GPT-5.6 family models (`gpt-5.6-terra`, `gpt-5.6-luna`).
2. Pricing estimation rejects any model identifier other than `gpt-5.6-sol`.
3. The session footer does not report latest-request context window occupancy or prompt-cache hit efficiency, making it difficult to see when a conversation approaches OpenAI's 272,000-token long-context pricing shift or the 1,050,000-token technical context window limit.

## Goals

- Introduce a closed, authoritative model catalogue in `src/config/models.rs` supporting `gpt-5.6-sol` (default), `gpt-5.6-terra`, and `gpt-5.6-luna`, complete with verified token limits and versioned pricing references.
- Support startup model selection via `KIKI_MODEL` with bounded, secret-safe diagnostic errors for invalid or unknown values.
- Update pricing estimation in `src/pricing.rs` to compute exact USD nanodollar costs across all three models for both short- and long-context rate bands using verified 3 September 2026 data.
- Add latest-settled-request context occupancy (`C<percent>/<capacity>`) and prompt-cache read percentage (`H<percent>`) to the session footer.
- Dynamically switch the `C` capacity denominator between the model's low-context pricing threshold (`272k`) and its full context capacity (`1.05m`) based on latest settled `total_tokens`:
  - When `total_tokens <= long_context_threshold`: percentage is computed relative to the short-context threshold (`272k`), alerting the user as the conversation approaches the 2x pricing boundary.
  - When `total_tokens > long_context_threshold`: percentage is computed relative to the technical context window (`1.05m`), showing headroom toward the hard technical limit.
  - Apply yellow styling for percentage values in `[80%, 90%)` and red for `≥ 90%` against whichever threshold tier is active.
- Encapsulate footer formatting, styling, and narrow-terminal truncation inside `TranscriptLayout::footer_rule`, returning a styled Ratatui `Line<'static>` to eliminate string-scraping in `src/tui.rs`.
- Present model identity compactly at startup: `Model: <id> (context: 1.05m, max output: 128k)`.

## Non-goals

- In-session interactive `/model` command and TUI selection modal (deferred to a future change with its own spec and planning).
- Automatic model fallback on rate limits or network failures.
- In-flight speculative context estimation while a response is streaming.
- Local tokeniser implementation (measurements rely strictly on provider-reported usage).

## Key discoveries

- `src/config/constants.rs:3` defines `FIXED_MODEL: &str = "gpt-5.6-sol"`, consumed directly by `src/openai.rs:59` and `src/pricing.rs:38`.
- `src/pricing.rs:9-32` already defines `PricingIdentity`, `Rates`, and `ModelPricing`. Unifying these directly with `src/config/models.rs` avoids duplicate pricing structs.
- `src/transcript.rs:2276-2287` builds `session_footer` as a flat `Option<String>`, and `src/tui.rs:1104-1134` wraps it into a single Ratatui `Span` with `muted_page_style()`, lacking per-span semantic colouring.
- `src/transcript.rs:1894-1907` renders the startup card lines; keeping the model metadata to a single compact line preserves the 9-line card height and prevents viewport overflow on 15-row terminals (`src/tui.rs:2054`).
- `docs/research/2026-09-03_gpt-5-6-pricing.md` contains verified technical limits (1,050,000 context window, 128,000 max output tokens) and pricing schedules for Sol, Terra, and Luna, including the 272,000 input-token threshold.

## Proposed approach

1. **Model Catalogue (`src/config/models.rs`)**:
   Define `ModelConfiguration` containing model ID, service tier, token capacities (`u64` and static `&'static str` display strings), and `ModelPricing` (reusing `PricingIdentity` and `Rates` from `src/pricing.rs`). Expose static `SOL`, `TERRA`, and `LUNA` configurations, `DEFAULT_MODEL` (`SOL`), and `model_by_id(&str) -> Option<&'static ModelConfiguration>`.
2. **Startup Configuration (`src/config.rs`)**:
   Read `KIKI_MODEL`. If unset, use `DEFAULT_MODEL`. If non-Unicode or unknown, emit `StartupProblemKind::ModelInvalidUnicode` or `ModelUnknown` with redacted diagnostics, retain `DEFAULT_MODEL` in `StartupSnapshot`, and disable submission. Assign problem ordering in `StartupProblemKind::order()` (order 4, shifting prompt to 5 and API key to 6). Pass `&'static ModelConfiguration` through `ReadyStartup` and `StartupSnapshot`.
3. **OpenAI Adapter (`src/openai.rs`)**:
   Update `OpenAiModel` constructor to require `model: &'static ModelConfiguration`. Use `model.id` and `model.service_tier` when constructing the request payload. Update live test harness to forward `ready.model()`.
4. **Pricing Engine (`src/pricing.rs`)**:
   Replace static Sol schedule with lookup via `model_by_id(&usage.model)`. Apply short- or long-context rates depending on `input_tokens <= model.pricing.long_context_threshold`. Preserve integer nanodollar arithmetic and exact consistency validations. In `estimate_run_cost`, attach the `PricingIdentity` of the resolved schedule.
5. **Footer Metrics & Rendering Seam (`src/transcript.rs`, `src/tui.rs`)**:
   - Resolve the active model from the latest settled attempt (via `model_by_id(&usage.model)`), falling back to `DEFAULT_MODEL` when no usage or snapshot is present.
   - Format `C<percent>/<capacity>` using checked `u128` arithmetic:
     - If `total_tokens <= model.pricing.long_context_threshold`: compute `(u128::from(total) * 100 + u128::from(threshold) / 2) / u128::from(threshold)` formatted as `C<percent>/<threshold_display>` (e.g. `C50%/272k`).
     - If `total_tokens > model.pricing.long_context_threshold`: compute `(u128::from(total) * 100 + u128::from(capacity) / 2) / u128::from(capacity)` formatted as `C<percent>/<capacity_display>` (e.g. `C29%/1.05m`).
     - Style the `<percent>` span yellow when in `[80%, 90%)`, red when `≥ 90%`, and muted otherwise.
   - Format `H<percent>` from `(u128::from(cached_input) * 1_000 + u128::from(input) / 2) / u128::from(input)`: display as whole integer percentage if `tenths % 10 == 0` (`H75%`), or with one decimal place (`H87.3%`). Omit `H` when `input == 0`, `cached_input > input`, or usage is missing.
   - Encapsulate the entire footer line assembly, styling, and truncation inside `TranscriptLayout::footer_rule(&self, width: u16) -> Line<'static>`. `tui.rs` delegates bottom rule rendering directly to this method.

## Key decisions & tradeoffs

- *Unified pricing structures*: Define rates and pricing identities once in `src/config/models.rs` and reuse directly in `src/pricing.rs`, eliminating parallel duplicate structs.
- *Static capacity display strings*: Store pre-formatted string slices (`"1.05m"`, `"128k"`, `"272k"`) on `ModelConfiguration` instead of writing a complex, general-purpose floating-point number formatting engine.
- *Dynamic capacity denominator in `C`*: Switch between `272k` (low-context pricing threshold) and `1.05m` (technical context window) to provide actionable warning before crossing into 2x pricing rates, while clearly displaying full window headroom once in the higher tier.
- *Total tokens as `C` numerator*: Use `total_tokens` from the latest settled request as the proxy for accumulated conversation context that feeds the next turn.
- *Encapsulated footer line*: `TranscriptLayout` produces the complete Ratatui `Line<'static>` with styled spans, avoiding brittle string-parsing in `tui.rs`.
- *Compact startup line*: Render `Model: <id> (context: 1.05m, max output: 128k)` on a single row to preserve viewport vertical budget on standard terminal heights.
- *Safe u128 metric arithmetic*: Cast all token values to `u128` during intermediate multiplication to guarantee overflow safety.

## Risks / unknowns

- *Terminal truncation boundary*: When narrow terminals truncate the footer with `…`, rightmost fields (`C`, `H`, `$`) must be preserved in order without broken or orphaned styling spans.
- *Live streaming stability*: Provider usage only arrives on settlement; while a request is actively streaming, the footer must cleanly retain the previous settled measurement without flickering.

## Dependencies

- `docs/research/2026-09-03_gpt-5-6-pricing.md` (authoritative point-in-time rates and limits).
- Ratatui text and styling primitives (`Line`, `Span`, `Color`, `Style`).

## Plan

### Task 1: Model Catalogue & Shared Pricing Structures
**Changes**
- Define `ModelPricing`, `Rates`, `PricingIdentity`, and `ModelConfiguration` in `src/config/models.rs`.
- Expose static configurations for `SOL`, `TERRA`, `LUNA`, `DEFAULT_MODEL` (`SOL`), and `model_by_id(id: &str) -> Option<&'static ModelConfiguration>`.
- Include static display strings on `ModelConfiguration`: `context_window_display: "1.05m"`, `max_output_display: "128k"`, `long_context_threshold_display: "272k"`.
- Remove `FIXED_MODEL` from `src/config/constants.rs` and re-export catalogue and pricing types in `src/config.rs`.

**Tests** (`src/config/models.rs`, unit)
- Verify catalogue completeness: exactly three models, unique IDs, default model is Sol.
- Verify invariants: context window (1,050,000) ≥ max output (128,000), long-context threshold (272,000) ≤ context window.
- Verify exact short and long rates for Sol, Terra, and Luna in integer nanodollars per token against the 3 September 2026 research record.

**Files**
- Create: `src/config/models.rs`
- Modify: `src/config.rs`, `src/config/constants.rs`

**Verification**
- `cargo test config::models::` — all catalogue invariant and rate tests pass.

---

### Task 2: Startup Configuration and Model Resolution
**Changes**
- Update `StartupInput`, `StartupSnapshot`, and `ReadyStartup` in `src/config.rs` to resolve and carry `&'static ModelConfiguration`.
- Resolve `KIKI_MODEL`: default to `DEFAULT_MODEL` when unset; emit `StartupProblemKind::ModelUnknown` or `ModelInvalidUnicode` on invalid input with redacted API keys, retaining `DEFAULT_MODEL` in `StartupSnapshot` and disabling submission.
- Add ordering priority for model problems in `StartupProblemKind::order()` (order 4, shifting prompt to 5 and API key to 6).
- Update `startup_block` in `src/transcript.rs` to render `Model: <id> (context: <context_display>, max output: <max_output_display>)`.

**Tests** (`src/config.rs`, unit)
- Unset `KIKI_MODEL` defaults to `gpt-5.6-sol`.
- Exact IDs `gpt-5.6-terra` and `gpt-5.6-luna` select their respective models with `SubmissionAvailability::Available`.
- Unknown model ID and non-Unicode values create typed problems, disable submission, and redact API keys from diagnostics.
- Startup snapshot rendering tests verify the compact single-line model format.

**Files**
- Modify: `src/config.rs`, `src/transcript.rs`

**Verification**
- `cargo test config::` — all configuration resolution and startup tests pass.

---

### Task 3: Multi-Model Pricing Engine
**Changes**
- Update `src/pricing.rs` to look up pricing schedules dynamically via `model_by_id(&usage.model)`.
- Apply short- vs long-context rates based on `usage.input_tokens <= model.pricing.long_context_threshold`.
- Update `estimate_run_cost` to aggregate prices across attempts by resolving each attempt's schedule via `model_by_id(&usage.model)`, preserving `Complete`, `LowerBound`, and `Unavailable` states.

**Tests** (`src/pricing.rs`, unit)
- Existing Sol regression totals remain identical.
- Terra and Luna price accurately under both short-context and long-context (>272,000 input tokens) rate bands.
- Inconsistent breakdowns, unknown models, and non-default service tiers return typed `PricingError`s.
- Multi-attempt runs correctly aggregate priceable steps and reflect lower-bound / unavailable states.

**Files**
- Modify: `src/pricing.rs`

**Verification**
- `cargo test pricing::` — all pricing calculation and aggregation tests pass.

---

### Task 4: OpenAI Client Adapter & Live Test Harness
**Changes**
- Update `OpenAiModel` in `src/openai.rs` to require `model: &'static ModelConfiguration`, eliminating the redundant 2-argument constructor.
- Build request payload `model` and `service_tier` directly from the configured model.
- Update `src/main.rs` to pass `ready.model()` to `OpenAiModel`.
- Update `start_live_controller` in `src/openai.rs` to forward `ready.model()`.

**Tests** (`src/openai.rs`, unit)
- Request serialization fixtures verify exact `model` and `service_tier` fields across Sol, Terra, and Luna configurations.
- Adapter tests compile cleanly with no dead code warnings.

**Files**
- Modify: `src/openai.rs`, `src/main.rs`

**Verification**
- `cargo test openai::` and `cargo clippy --all-targets -- -D warnings` — client tests pass with zero Clippy warnings.

---

### Task 5: Context and Cache-Hit Metrics Calculation & Footer Formatting
**Changes**
- In `src/transcript.rs`, calculate latest-settled-attempt metrics using `model_by_id(&usage.model)` (falling back to `DEFAULT_MODEL` when no usage or snapshot is present):
  - `C<percent>/<capacity>`: if `total_tokens <= model.pricing.long_context_threshold`, compute `(u128::from(total) * 100 + u128::from(threshold) / 2) / u128::from(threshold)` with denominator from `threshold_display` (`272k`); if `total_tokens > threshold`, compute `(u128::from(total) * 100 + u128::from(capacity) / 2) / u128::from(capacity)` with denominator from `context_window_display` (`1.05m`).
  - Style `<percent>` span yellow for `[80%, 90%)`, red for `≥ 90%`, and muted otherwise.
  - `H<percent>`: compute `(u128::from(cached_input) * 1_000 + u128::from(input) / 2) / u128::from(input)` formatted as integer percentage if tenths is zero (`H75%`) or with 1 decimal place (`H87.3%`). Omit `H` if `input == 0`, `cached_input > input`, or usage is missing.
- Implement `TranscriptLayout::footer_rule(&self, width: u16) -> Line<'static>` which assembles cumulative fields, `C`, `H`, and `$`, applies Ratatui span styles, and performs left-truncation (`…`) for narrow terminals.

**Tests** (`src/transcript.rs`, unit)
- Context calculation tests: dynamic capacity switching at threshold boundary, 80%/90% rounding and styling boundaries.
- Cache hit rate tests: whole vs fractional percentages, omission on zero input or invalid `cached > input`.
- Chronology tests: latest settled attempt wins, active generation retains previous settled metrics, unavailable usage suppresses current metrics without stale fallback.
- Narrow terminal truncation preserves rightmost `C`, `H`, and `$` spans.

**Files**
- Modify: `src/transcript.rs`

**Verification**
- `cargo test transcript::` — all transcript metrics and layout tests pass.

---

### Task 6: TUI Footer Integration & Documentation
**Changes**
- Update `src/tui.rs` to render the bottom rule by calling `layout.footer_rule(area.width)`, removing legacy flat string parsing and slicing.
- Update `README.md` to describe `KIKI_MODEL`, supported models (Sol, Terra, Luna), versioned pricing schedules, and footer metric semantics (`C` dynamic threshold, `H` cache hit rate).

**Tests** (`src/tui.rs`, unit)
- TUI render tests verify bottom rule output with styled metrics, narrow terminal ellipsis truncation, and configuration-only view behavior.
- Ensure viewport height calculations and snapshot tests pass on standard terminal sizes (15-row and larger).

**Files**
- Modify: `src/tui.rs`, `README.md`

**Verification**
- `cargo test` (all unit tests pass).
- `cargo clippy --all-targets -- -D warnings` (clean).
- `cargo fmt --all -- --check` (clean).
