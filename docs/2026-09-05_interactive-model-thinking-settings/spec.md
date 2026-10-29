# Spec: Interactive model, thinking, and session settings

## Context

Kiki currently chooses one unqualified OpenAI model at startup through `KIKI_MODEL`, hard-codes medium thinking effort, and requires a restart to change either. It has no slash-command dispatch, persistent user/project settings, session naming, or terminal-title management. Its prompt footer is increasingly dense, while user-facing text still calls model thinking “reasoning”.

This change introduces qualified models with model-owned thinking capabilities, layered TOML selections, and live `/model` and `/thinking` controls whose updates apply at model-step boundaries. It also adds debounced Shift+Tab cycling, a compact Pi-style footer, session-only `/name`, and `✷` terminal titles.

## Goals

- Represent selectable models canonically as `provider/model`, with each catalogue model defining its supported and default thinking levels.
- Resolve startup selections through built-in defaults, user TOML, project TOML, then environment variables.
- Provide Pi-style `/model` and `/thinking` selectors, direct command arguments, and explicit Ctrl+S persistence to user settings.
- Allow model/thinking changes during an active run while keeping the in-flight model step unchanged; debounce Shift+Tab cycling for two seconds.
- Replace the footer with a compact, width-aware display of cwd/name, usage, context, model, and thinking; colour prompt rules by thinking intensity.
- Add session-only `/name`, exact `✷ - <dirname|name>` terminal titles, and consistent user-facing “thinking” terminology. (Title format superseded; see [Session name, terminal title, and terminology](#session-name-terminal-title-and-terminology).)

## Non-goals

- Adding a second provider, dynamic model discovery, custom model definitions, or cross-provider replay conversion.
- Configuring model capabilities or supported thinking levels through TOML.
- Storing API keys, Monty paths, or system-prompt paths in Kiki configuration files.
- Durable session storage, session resumption, or persisting `/name` across restarts.
- Building a general slash-command plugin framework or command autocomplete system beyond the three specified commands.
- ACP integration or clickable transcript/file links; links are covered by a separate specification.

## Key discoveries

- Startup configuration currently reads `MONTY_BIN`, `KIKI_MODEL`, `OPENAI_API_KEY`, `KIKI_SYSTEM_PROMPT`, and the current directory directly; it has no file-backed settings layer (`src/config.rs:16-18`, `src/config.rs:360-368`, `src/prompt.rs:55-89`).
- The closed catalogue contains three unqualified model IDs and one global default; selected model metadata is passed into one concrete `OpenAiModel` at startup (`src/config/models.rs:27-134`, `src/main.rs:30-35`).
- OpenAI request construction uses the startup model and hard-codes `reasoning.effort` to `medium`; one request body is reused across retries (`src/openai.rs:87-165`, `src/openai.rs:168-214`).
- A run may perform several sequential model steps, but `RuntimeController` moves the `Agent` into an active worker and cannot directly mutate its model while the run is active (`src/agent.rs:462-500`, `src/agent.rs:522-652`, `src/runtime.rs:185-201`, `src/runtime.rs:418-521`).
- Runtime control currently exposes only submit, cancel, and shutdown. The TUI submits every accepted prompt as a user request and has no slash-command classifier or picker (`src/runtime.rs:16-20`, `src/tui.rs:237-318`).
- Shift+Tab is unhandled. Ctrl+T only changes visibility of retained reasoning blocks; it does not change provider effort (`src/tui.rs:259-270`, `src/tui.rs:519-600`, `src/transcript.rs:1935-1958`).
- The prompt footer is assembled inside its bottom rule from cumulative `↑`, `↓`, `R`, `W`, `r`, and `Σ` fields plus context, cache-hit, and cost fields; configuration-only mode omits it (`src/transcript.rs:1529-1567`, `src/transcript.rs:2839-2955`, `src/tui.rs:1006-1053`).
- The TUI already has a 50 ms poll and deterministic timer seam used for transient UI effects, so a two-second trailing debounce needs no background task (`src/tui.rs:27-28`, `src/tui.rs:385-419`, `src/tui.rs:819-916`).
- Pi keeps Ctrl+T as thinking-block visibility, uses Shift+Tab for level cycling, stores user and exact-cwd project settings separately, and writes picker defaults to the user file (`/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/keybindings.md:147-155`, `/opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/docs/settings.md:1-10`).
- Maki’s useful contrasting choices are qualified `provider/model` identities and model-specific thinking support. Its model selection persists automatically, its thinking setting is captured per queued run rather than per model step, and its executable Lua configuration is broader than Kiki needs (`.research/maki/source/src/setup.rs:23-64`, `.research/maki/source/maki-providers/src/types.rs:555-607`, `.research/maki/source/maki-agent/src/agent/run.rs:200-220`, `.research/maki/source/maki-lua/src/loader.rs:198-246`).

## Proposed approach

### Qualified catalogue and model-owned thinking

Make a qualified model identity the canonical value throughout configuration, runtime selection, usage reporting, and provider construction. The current catalogue entries become `openai/gpt-5.6-sol`, `openai/gpt-5.6-terra`, and `openai/gpt-5.6-luna`. Configuration and environment values must use this form; the old unqualified `KIKI_MODEL` values are rejected with constructive remediation.

Each catalogue entry references an ordered, non-empty list of supported thinking values and owns one default from that list. Kiki has no global cross-provider enum which is reconciled after selection. The three current OpenAI models share one immutable provider-value slice containing their documented sequence `none`, `low`, `medium`, `high`, `xhigh`, and `max`, with `medium` as each catalogue default. Kiki displays the selected slice value directly rather than aliasing `none` to `off`, inventing `minimal`, duplicating per-model arrays, or maintaining a translation table. The OpenAI adapter writes that validated value to the provider’s required `reasoning.effort` field; provider wire terminology remains `reasoning`.

Session selection remembers one thinking value per qualified model. Switching models atomically selects the target model and restores its current-session value, otherwise its resolved persisted value, otherwise its catalogue default. A pending Shift+Tab preview belongs to its source model and is discarded by a model switch rather than mapped by label.

### Hierarchical TOML settings

Load optional `~/.kiki/config.toml`, then optional `<cwd>/.kiki/config.toml`, then environment overrides. The project location is exactly Kiki’s workspace/launch directory; Kiki does not walk ancestors and does not add a trust prompt for inert model selections. Missing files are normal. Project values override matching user values, while per-model thinking entries merge independently:

```toml
model = "openai/gpt-5.6-sol"

[thinking]
"openai/gpt-5.6-sol" = "high"
"openai/gpt-5.6-luna" = "low"
```

`KIKI_MODEL` overrides the startup model with a qualified identity. Resolve and validate it before `KIKI_THINKING`; if the model is invalid, report that problem and do not validate thinking against a fallback model. With a valid effective model, `KIKI_THINKING` overrides its startup selection. Empty, non-Unicode, unknown, or model-unsupported values are typed startup problems whose remediation names the variable, qualified model, and valid values. Environment variables determine startup only; they neither lock nor override later session changes. API keys, Monty, and system-prompt resolution retain their current environment-only behaviour.

Malformed TOML, unknown fields, unknown models, non-string selections, and levels unsupported by their keyed model become bounded startup problems and retain the existing configuration-only screen. Diagnostics identify the file, field, expected form or supported values, and a concrete remediation without echoing arbitrary file contents or credentials.

Ctrl+S in `/model` writes the selected qualified model to the user file. Ctrl+S in `/thinking` writes the selected value under the current qualified model in the user thinking table. Project files are edited manually. Saving updates only the selected key, preserves comments and unrelated fields, creates `~/.kiki` when needed, and atomically replaces the settings file from a complete same-directory temporary file. A save must not rewrite malformed input. If project or environment state will still win at the next launch, the confirmation states that the user default was saved but is currently overridden.

### Live model-step settings

Create one current session selection in a latest-value watch channel shared by the runtime controller and `OpenAiModel`. Runtime settings commands are accepted while idle or active and update model plus thinking atomically. Every validated command carries a monotonically increasing session-local revision; the controller replaces the watched value before reliably acknowledging only that revision. The accepted pair cannot differ from the validated pair retained by the TUI. An older acknowledgement never changes visible selection, and the TUI clears pending state only when the acknowledgement matches the latest sent revision and no newer preview exists. Settings changes do not create user records or model-context entries.

At the beginning of every `OpenAiModel::step`, copy the latest accepted selection. That immutable copy supplies model ID, service tier, thinking effort, retry request bodies, and usage/pricing identity for the whole step. An in-flight request and its retries never change. A command affects the first model step whose snapshot occurs after the runtime accepts it; a boundary race may therefore apply to the following step, but no request can mix old and new fields.

Price every reported attempt under the qualified model and pricing identity captured for that attempt, then sum the resulting dollar amounts even when a session contains several model identities. Per-attempt estimates retain their pricing identity; the session aggregate is amount-only because no single identity describes it. Mixed supported models therefore retain cumulative cost. Any unavailable/unpriceable attempt or untracked retry preserves the existing lower-bound semantics for the aggregate rather than discarding known cost.

Keep the generic `Model` interface unchanged. Live selection is currently an OpenAI adapter capability backed by the catalogue; a provider router or provider-neutral settings parameter is deferred until a second provider earns that interface.

### Slash commands, selectors, and cycling

Intercept only these complete command forms before ordinary submission, while idle or active:

- `/model` opens a provider-grouped model picker. `/model <text>` selects a case-insensitive exact qualified match or opens the picker filtered by the text.
- `/thinking` opens a picker containing only the active model’s ordered levels. `/thinking <level>` selects a case-insensitive exact supported value or reports the model and valid values.
- `/name <remaining text>` updates the session name. `/name` reports the current name or `Usage: /name <name>` when unnamed.

Recognised commands never enter display history or model context. Other slash-leading text retains existing ordinary-prompt behaviour. Opening a picker consumes the command draft. Its initial row is the current selection when visible, otherwise the first match. Typing and Backspace update the filter; Up/Down move one row; PageUp/PageDown move one visible page; Home/End jump to the first/last match; Enter applies the highlighted selection; Ctrl+S applies it and updates the relevant user default; Escape closes to an empty prompt without a change. An empty result has no highlight and Enter/Ctrl+S report `No matching models` or `No matching thinking levels`. Selector failures and save results are concise transient statuses, not agent messages.

Handle both terminal representations of Shift+Tab. Each accepted press immediately cycles a local preview through the active model’s ordered levels, updates the footer and prompt-rule colour, and resets a trailing two-second deadline. Intermediate values never reach runtime. After two quiet seconds, send only the final value. An explicit `/thinking` selection replaces the preview and applies immediately; a model switch cancels it. If the user submits a new prompt while a preview is pending, send the final thinking selection before Submit so the new run starts with the visible level. Automatic model steps which start before the deadline continue with the previously accepted value. Key handling and timer expiry share an injected monotonic timestamp so production and deterministic tests use the same exact deadline rule: no commit before `pressed_at + 2s`, commit on the first tick at or after it.

### Footer and thinking presentation

Keep the footer in the prompt’s bottom rule, but make it always available in configured runtime mode. Its left side shows the current working directory, replacing the home prefix with `~`, and appends ` (<name>)` when named. Preserve the full cwd while space permits; under pressure, truncate from the front so the basename and name remain visible.

The right side contains, in order when available:

```text
↑60 ↓11k R483k W77k CH77.1% $0.7940 28.5%/272k  gpt-5.6-sol • thinking high
```

Fields use one ASCII space; the statistics and model/thinking groups use at least two. `CH` is one cache-hit field, context deliberately has no `C` prefix, and only the footer drops the existing reasoning-token `r` and total `Σ`; per-step accounting remains unchanged.

Token counts round to at most three significant figures, strip redundant zeroes, use lowercase `k`/`m`, and carry across a suffix boundary: `999`, `1k`, `1.05k`, `11.5k`, `999k`, `1m`, and `1.23m`. Cache hit and context always use one decimal place. Cost uses the greater precision of four significant figures or two decimal places and retains zeroes required by that precision: `$0.7940`, `$0.01235`, `$12.35`, `$123.45`, and `$1234.50`.

Under width pressure, first front-truncate cwd while retaining its basename and `(<name>)` where possible. Then remove `↑`, `↓`, `R`, and `W` from left to right, followed by `CH`, cost, and context. Preserve model/thinking last; if that group alone is too wide, front-truncate the model ID before clipping the thinking label safely at cell boundaries. The footer always remains one row. Because only one provider exists, it abbreviates the qualified model to its model ID; once more than one provider is available it displays the qualified identity.

Colour both prompt rules from a six-stop dark-theme scale, from dim grey through blue and violet to vivid magenta. Map the active model’s complete ordered values across that scale by ordinal position, so visual intensity remains meaningful without inventing capability levels. A future model with fewer values uses evenly spaced stops including both endpoints.

### Session name, terminal title, and terminology

(Superseded for the title format by commit `65a4e56`: titles are now `✷ <cwd basename>`, or `✷ <name> - <cwd basename>` once named, with `kiki` standing in for an unavailable cwd. Zed terminal threads lift a leading non-letter into the tab icon, which left the old format reading `- kiki`; the name now joins the folder rather than replacing it, matching pi.)

Initialise the terminal title as `✷ - <cwd basename>` in configured and configuration-only modes, sanitising the basename with the same single-line rule as a name. If cwd is unavailable, use `✷ - kiki`. After `/name <name>`, use exactly `✷ - <name>` and update the footer immediately. Trim the remaining command text, collapse every whitespace run to one ASCII space, reject an empty result through the existing no-argument behaviour, and reject names wider than 80 display cells with a constructive status rather than truncating them. Names are session-only and do not alter cwd or model history. Terminal-title publication is best effort: failure keeps the accepted name/footer and reports that the title could not be changed without terminating or weakening terminal cleanup.

Use “thinking” for all user-facing startup fields, statuses, placeholders, controls, errors, tests, and README text, including the existing Ctrl+T visibility hint. Retain `reasoning` only where it is an external OpenAI wire term or an internal provider-facing concept whose rename would add no user value.

## Key decisions & tradeoffs

- Use two separate specifications: this settings/session-control change and a later clickable-links change.
- Deliver this specification as one PR despite its size; reject horizontal implementation phases because the selected configuration, runtime, and TUI behaviour form one user-facing control loop.
- Use qualified model identities and derive provider selection from the model; reject redundant provider and model settings.
- Keep supported thinking values and defaults in the compiled model catalogue; reject configurable capability definitions and a Kiki-global level enum reconciled after selection.
- Remember and persist thinking per qualified model; reject resetting on every switch or carrying a pending old-model preview by matching labels.
- Use exact-cwd project settings and user-only picker persistence, matching Pi’s scope and save target; reject ancestor discovery and a project trust system for inert enum values.
- Require qualified environment values consistently; reject compatibility aliases for existing unqualified `KIKI_MODEL` values.
- Sample shared selection inside the OpenAI adapter at each model-step start; reject widening every `Model` implementation or building a provider router before another provider exists.
- Accept settings during active runs but isolate in-flight requests and retries; reject mutating an active step.
- Debounce only Shift+Tab for two seconds; explicit picker choices apply immediately, and explicit prompt submission flushes the visible preview first.
- Preserve model history and the persistent arena when switching current OpenAI models; defer cross-provider replay policy.
- Keep Ctrl+T as thinking-block visibility and Shift+Tab as thinking-effort cycling.
- Save only user TOML through selectors while allowing manual project overrides; reject automatic persistence on every selection.
- Use a compact Pi-style footer with at most three significant figures for tokens and the greater of four significant figures or two decimal places for cost, removing `r` and `Σ`; preserve model/thinking before lower-priority metrics.
- Set session names only in memory and derive the terminal title from name or cwd; reject durable session work in this change.
- Keep user-facing “thinking” while preserving OpenAI’s required `reasoning` vocabulary at its adapter seam.

## Risks / unknowns

- The current OpenAI replay contains opaque encrypted reasoning items. This change assumes replay compatibility among the three supported GPT-5.6 catalogue models; a future provider or incompatible model needs an explicit canonical-history/replay policy.
- A runtime command racing a model-step snapshot cannot guarantee which later step receives it. The acceptance point, revisioned acknowledgement, and immutable per-step snapshot must make either ordering coherent and prevent mixed requests.
- One PR crosses configuration, provider, runtime, TUI, and transcript presentation seams. Atomic tasks and focused regression suites are required to keep failures local.
- Project settings can select a costly model and thinking level without a trust prompt. Qualified startup/footer display and environment precedence make the effective selection visible, but Kiki does not add a confirmation gate.
- Ctrl+S updates user settings even when project or environment values mask them. The save result must explain this rather than imply the next launch will use the saved value.
- Modified Tab may arrive as `BackTab` or `Tab` plus Shift depending on terminal capability. Both forms need deterministic coverage, while unsupported terminals may not report the chord distinctly.
- Footer identity now renders without usage, reducing history width on very small terminals. Existing tiny-terminal safety and one-row truncation must remain intact.
- Terminal title support varies by emulator. Title failures must not corrupt terminal restoration or expose unsanitised control text.

## Dependencies

- Existing configuration and catalogue modules: `src/config.rs`, `src/config/models.rs`, and `src/prompt.rs`.
- Existing model-step, runtime-command, and OpenAI request seams: `src/model.rs`, `src/agent.rs`, `src/runtime.rs`, `src/openai.rs`, and `src/main.rs`.
- Existing prompt, footer, timer, terminal-lifecycle, and display-projection seams: `src/prompt_editor.rs`, `src/tui.rs`, and `src/transcript.rs`.
- Direct `toml_edit` and `tempfile` dependencies for targeted, comment-preserving updates and same-directory temporary publication; both versions are already present in `Cargo.lock`, with `tempfile` currently test-only.
- Ratatui’s existing Crossterm integration for modified keys and terminal titles.
- Current model-catalogue contract: `docs/2026-09-03_model-catalog-and-context-measurement/spec.md`.
- Current live-agent and prompt/footer contract: `docs/2026-08-27_live-openai-agent/spec.md`.
- OpenAI’s current model documentation for provider-supported reasoning effort values: <https://developers.openai.com/api/docs/guides/latest-model>.

## Plan

### Acceptance criteria

- The three built-in models have canonical qualified identities and each declares exactly its documented ordered values `none`, `low`, `medium`, `high`, `xhigh`, and `max`, with `medium` as a valid default. OpenAI requests use the selected model ID without the provider prefix and the selected provider-supported thinking value.
- Startup precedence is built-in defaults, `~/.kiki/config.toml`, `<cwd>/.kiki/config.toml`, then `KIKI_MODEL`/`KIKI_THINKING`. Project thinking entries override matching user entries without replacing unrelated models. Environment values must be qualified where applicable and affect startup only.
- Missing config files are accepted. Invalid TOML, unknown fields, unknown qualified models, wrong value types, and unsupported per-model thinking selections disable submission through constructive, bounded, non-secret startup diagnostics. Empty, non-Unicode, unknown, or unsupported `KIKI_THINKING` values do the same after the effective model is valid; an invalid `KIKI_MODEL` is reported without validating thinking against a fallback.
- Ctrl+S in the model or thinking picker changes only the relevant user setting, preserves comments and unrelated settings, creates the user directory when absent, and never overwrites malformed input. A complete file is published atomically, and masked saves report their lower effective precedence.
- `/model`, `/thinking`, and `/name` are accepted while idle or active and never become user records or model context. Their no-argument, exact-argument, filtered-picker, cancellation, validation, and persistence behaviours match the Proposed approach.
- A model change preserves display/model history and arena state, cancels any pending old-model thinking preview, and atomically restores the target model’s session, persisted, or catalogue-default thinking selection in that order.
- Runtime selection accepted during a model step cannot affect that step or its retries. The next model step snapshot accepted afterwards uses the complete new model/thinking pair, including correct usage model identity and pricing. Mixed-model attempts remain independently priced and sum into one complete or lower-bound session cost.
- Settings commands carry increasing revisions and acknowledgements return only the accepted revision. An older acknowledgement never changes visible selection, and only the latest matching acknowledgement without a newer preview clears pending state.
- Shift+Tab previews immediately, wraps through only the active model’s ordered levels, resets a two-second trailing deadline, and emits one final settings update. Explicit thinking selection replaces it; model selection cancels it; user submission flushes it before Submit; automatic steps before expiry retain the accepted level. An injected monotonic timestamp proves no commit before 1.999 seconds and one on the first tick at or after two seconds.
- The configured footer always renders cwd and optional `(<name>)` on the left and compact metrics plus model/thinking on the right. Counts use at most three significant figures and lowercase suffixes; `CH` is one field; context has no `C`; cost uses the greater of four significant figures or two decimal places; only footer `r` and `Σ` are absent; width pressure follows the exact priority and fallback rules without adding a row.
- Both prompt rules use the active or preview thinking-intensity colour. Existing context warning/error colour remains distinguishable within footer text.
- `/name <name>` accepts only a whitespace-collapsed value no wider than 80 display cells, updates only in-memory display state, footer, and exact title `✷ - <name>`. Unnamed startup uses a sanitised `✷ - <cwd basename>` or `✷ - kiki` when cwd is unavailable. `/name` reports current state or usage, no name reaches model history, and title publication failure is nonfatal and visible.
- Every user-visible occurrence uses “thinking”; Ctrl+T continues to toggle retained thinking visibility independently of provider effort.
- Existing cancellation, retry, transcript chronology, prompt editing, tiny-terminal rendering, configuration-only mode, and credential-free tests remain unchanged except where this specification explicitly replaces presentation or configuration behaviour.

### Overall validation

Run from the feature worktree:

```sh
cargo fmt --all -- --check
cargo check --all-targets
test_home="$(mktemp -d)"
trap 'rm -rf "$test_home"' EXIT
env -u OPENAI_API_KEY -u KIKI_MODEL -u KIKI_THINKING -u KIKI_SYSTEM_PROMPT HOME="$test_home" MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l '(cap_std|rustix|blake3|getrandom)' src --glob '*.rs')" = "src/tools.rs"
! rg -n '\breasoning\b' README.md
git diff --check main...HEAD
```

All commands must pass. Focused task verification runs before the full suite. The credential-gated live OpenAI test remains opt-in; manual validation uses a configured account to observe actual model/thinking changes at model-step boundaries.

### Planning context

- `StartupInput` already separates raw acquisition from validation, so file/environment precedence belongs before the existing readiness and redacted-diagnostic seam (`src/config.rs:313-371`). Default tests must inject home, cwd, files, and environment rather than call production `config::load`; Task 2 replaces the current TUI test calls to `config::load`, leaving production loading only at composition and the ignored live path (`src/tui.rs:2429`, `src/tui.rs:2464`, `src/tui.rs:2518`, `src/tui.rs:3816`, `src/tui.rs:3832`, `src/tui.rs:4897`, `src/main.rs:39`, `src/openai.rs:5055`).
- `OpenAiModel` owns model ID, request construction, retries, and usage accounting. A copied step-start selection must replace every use of its immutable startup model, including accounting, without exposing settings through model history (`src/openai.rs:39-44`, `src/openai.rs:137-214`, `src/openai.rs:577-608`).
- The runtime controller continues receiving commands while its agent is owned by an active worker, making a latest-value shared selection narrower than a worker-control protocol or model replacement (`src/runtime.rs:307-361`, `src/runtime.rs:383-416`, `src/runtime.rs:418-523`).
- TUI time effects already run through `App::tick` and the 50 ms poll. The debounce remains deterministic when time is injected through the existing key/tick test seam (`src/tui.rs:385-419`, `src/tui.rs:819-916`).
- The footer’s fields and truncation are owned by `TranscriptLayout`, while header, prompt rules, and terminal lifecycle are owned by the TUI. Session identity must not be pushed into retained display records (`src/transcript.rs:1529-1567`, `src/transcript.rs:2839-2955`, `src/tui.rs:935-1053`).
- `PromptEditor::take` is the current submission seam. Recognised slash commands must be classified before constructing `RuntimeCommand::Submit`, while unrecognised slash-leading prompts retain current behaviour (`src/prompt_editor.rs:39-54`, `src/tui.rs:305-318`).

### Task 1: Add qualified model-specific thinking

**Changes** — Replace catalogue identities with canonical qualified values and add each model’s ordered supported thinking values and default. Make startup lookup reject unqualified identities constructively. Build OpenAI requests from an explicit model/thinking selection, stripping only the known provider qualifier for the wire model ID, and use the same selection for retries, usage identity, and pricing lookup. Replace hard-coded medium effort while leaving provider wire vocabulary unchanged.

**Tests** (`src/config/models.rs`, catalogue/domain; `src/config.rs`, startup validation; `src/openai.rs`, adapter request fixtures; `src/pricing.rs`, qualified usage lookup) — First fail on a qualified Sol request with `high` effort. Cover exact qualified lookup, rejection of unqualified/unknown identities, the shared exact six-value OpenAI slice and valid default on every current model, every supported effort serialisation, unsupported effort rejection before networking, qualified usage pricing, amount-only mixed-model cost summation, and complete/lower-bound propagation.

**Files** — Modify: `src/config/models.rs`, `src/config.rs`, `src/openai.rs`, `src/pricing.rs`.

**Verification** — `cargo fmt --all -- --check && cargo test config::models::tests && cargo test config::tests && cargo test openai::tests && cargo test pricing::tests && cargo check --all-targets` passes.

**Commit** — `Add model-specific thinking settings`

### Task 2: Add hierarchical Kiki configuration

**Changes** — Add a settings module which reads exact user and cwd-project TOML documents, validates only model and per-model thinking selections, overlays them with environment input, and reports source-specific constructive startup problems through the existing configuration-only path. Add targeted user-setting updates through `toml_edit`, preserving unrelated text and publishing a complete same-directory `tempfile` through atomic replacement. Pass resolved startup model/thinking and settings paths through the existing redacted startup construction without moving secrets into TOML. Replace default-test calls to production `config::load` with injected startup snapshots so focused and full tests cannot observe a developer’s home settings.

**Tests** (`src/config/settings.rs`, settings parsing/merging/persistence; `src/config.rs`, startup integration; `src/transcript.rs`, configuration diagnostics; `src/tui.rs`, injected configuration fixtures) — First fail on project model overriding a user model while inheriting another model’s thinking selection. Cover every precedence layer, exact-cwd lookup, absent files, map merging, qualified environment requirements, startup-only environment behaviour, empty/non-Unicode/unknown/unsupported `KIKI_THINKING`, invalid-model precedence over thinking validation, malformed/unknown/wrong-type/unsupported file values, simultaneous user/project errors, bounded redacted constructive remediation, comment-preserving single-key updates, missing-directory creation, malformed-file refusal, complete replacement, temporary-file cleanup, and masked-save reporting; replace every default TUI call to production `config::load` with an injected fixture.

**Files** — Create: `src/config/settings.rs`. Modify: `Cargo.toml`, `Cargo.lock`, `src/config.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification** — `cargo fmt --all -- --check && cargo test config::settings::tests && cargo test config::tests && cargo test transcript::tests -- --test-threads=1 && cargo test tui::tests -- --test-threads=1 && ! rg -n 'crate::config::load\(' src/tui.rs && cargo check --all-targets` passes.

**Commit** — `Add hierarchical Kiki configuration`

### Task 3: Apply live settings between model steps

**Changes** — Introduce one latest-value session-selection watch channel shared at composition time by the runtime controller and OpenAI adapter. Add runtime commands and revision-only reliable acknowledgements for atomic model/thinking changes while idle or active. Snapshot once at each OpenAI model-step start and retain that selection across attempts. Preserve Agent history and arena ownership; do not add settings entries to model context or records.

**Tests** (`src/openai.rs`, per-step adapter isolation; `src/runtime.rs`, command/race integration) — First fail with a gated two-step run where a settings command during step one must affect step two only. Cover active and idle acceptance, latest-value wins, complete pair atomicity, immutable in-flight request/retries, usage identity, command-versus-step boundary races, cancellation and settlement retention, later-run use, increasing revisions, revision-only acknowledgement ordering, and absence from model/display records.

**Files** — Modify: `src/main.rs`, `src/openai.rs`, `src/runtime.rs`, `src/config/models.rs`.

**Verification** — `cargo fmt --all -- --check && cargo test openai::tests && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests -- --test-threads=1 && cargo check --all-targets` passes.

**Commit** — `Apply model settings between steps`

### Task 4: Add model and thinking selectors

**Changes** — Add one reusable filtered picker for qualified models and model-owned thinking values. Classify recognised slash commands before ordinary submission, support active-run invocation and Pi-style direct arguments, persist explicit user defaults on Ctrl+S, and surface concise statuses. Add immediate local Shift+Tab preview for both terminal event forms, a resettable two-second trailing debounce, ordered flush-before-submit, explicit-selection replacement, model-switch cancellation, and runtime acknowledgement reconciliation.

**Tests** (`src/tui/picker.rs`, picker filtering/navigation; `src/tui.rs`, command/input/timer integration) — First fail with `/model terra` opening a filtered picker without emitting Submit. Cover provider grouping, initial focus, typed filtering and Backspace, Up/Down/PageUp/PageDown/Home/End, empty results, exact and filtered model arguments, model-specific thinking rows, unsupported values, Enter/Ctrl+S/Escape with consumed prompt text, active-run model/thinking commands, unrecognised slash prompts, no record/context pollution, Shift+Tab wrapping and both key encodings, monotonic key timestamps, immediate preview, no command before 1.999 seconds, one command at two seconds, deadline reset, explicit replacement, model cancellation, stale acknowledgement, newer-preview retention, release-event no-op, settlement retention, and settings-before-Submit ordering. `/name` remains Task 5.

**Files** — Create: `src/tui/picker.rs`. Modify: `src/tui.rs`.

**Verification** — `cargo fmt --all -- --check && cargo test tui::picker::tests && cargo test tui::tests && cargo check --all-targets` passes.

**Commit** — `Add model and thinking selectors`

### Task 5: Refresh session identity and footer

**Changes** — Add session-only `/name`, terminal-safe best-effort title updates, and cwd/name footer state without adding session records. Replace footer accounting fields and formatting with the exact compact token/cache/context syntax and four-significant-figure-or-two-decimal cost rule, qualified-model abbreviation policy, width priorities, and front-truncated cwd. Keep per-step accounting unchanged. Colour both prompt rules by active or preview thinking ordinal. Rename every user-facing reasoning label to thinking and update startup output, controls, tests, and README.

**Tests** (`src/transcript.rs`, footer formatting/layout; `src/tui.rs`, naming/title and one footer/rule wiring path) — First fail on the exact unnamed title `✷ - kiki` and named footer `~/…/kiki (Refactor auth)`. At the transcript seam, cover exact token examples and suffix carries, cost examples at both precision rules, cache/context one-decimal output, single `CH`, unlabeled context, footer-only removal of `r`/`Σ`, mixed-model cumulative cost, model/thinking display with one provider, every thinking colour and reduced-level interpolation, cwd home abbreviation, exact front-truncation/removal priority, model-only fallback, tiny widths, and missing usage. At the TUI seam, cover `/name` during idle/active runs, no-argument/trimmed-empty status or usage, whitespace collapsing, the 80-cell rejection boundary, no model-history effects, sanitised cwd titles, unavailable-cwd fallback, configured/configuration-only titles, nonfatal title failure and cleanup, one active/preview rule-colour case, one complete footer delegation case, and explicit rendered/README terminology assertions.

**Files** — Modify: `src/tui.rs`, `src/transcript.rs`, `src/config.rs`, `README.md`.

**Verification** — `cargo fmt --all -- --check && cargo test transcript::tests::footer -- --test-threads=1 && cargo test tui::tests -- --test-threads=1 && ! rg -n '\breasoning\b' README.md && cargo check --all-targets` passes; then run Overall validation.

**Commit** — `Refresh session identity and footer`
