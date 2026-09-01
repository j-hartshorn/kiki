# Plan: Live OpenAI agent — Phase 2: Inspectable streaming transcript

## Source

- Spec: `docs/2026-08-27_live-openai-agent/spec.md`, Phase 2

## Acceptance criteria

- Completed readable reasoning summaries are retained beside opaque provider replay in model history, then carried through reliable runtime records. Replay order and encrypted continuation behaviour remain unchanged.
- Missing or dropped progress cannot erase a completed summary. Partial summaries remain presentation evidence only when a step never completes.
- User prompts render as neutral Pi-style cards without a `you:` label. RunId and ModelStepId remain authoritative but are not displayed; live display uses activity instead. Assistant text and reasoning render streaming GitHub-Flavoured Markdown, including tables, task lists, links, blockquotes, strikethrough, and fenced code. Raw HTML is literal and images use text fallbacks.
- Reasoning is retained, shown by default in a subtly grey italic card with two-cell horizontal and one-row top-and-bottom padding, and globally hidden or shown with `Ctrl-T`; hidden blocks leave a discoverable placeholder.
- Each Python action and its output share one full-width rounded outer arena frame with its existing muted blue active, green settled, or red error/cancellation lifecycle background and foreground. Ordinary settled frames have no heading; live activity and error/cancellation diagnostics remain concise and appear exactly once. Available `code`, settled `tool calls`, `stdout`, non-empty `stderr`, `result` or `error`, interrupted `read`, `failure`, `warning`, and current `stdout preview` sections are separately closed and labelled in that order, with muted borders and one-cell body padding where width permits. `tool calls` is authoritative, reliable display-only evidence, never model context: one path-free `read · N lines`, `read · failed`, or `read · pending` row for each valid host read, with no arguments, paths, error text, or user text. Empty sections are omitted; accounting remains unboxed, muted, right-aligned, and follows its blank separator inside the outer frame. Final assistant Markdown remains a neutral page-background block with one-row top-and-bottom padding and no side padding, label, or border.
- Arena Python is Tree-sitter highlighted during and after generation. Incomplete Python and highlighting errors preserve every character through a plain-code fallback.
- Automatic collapse occurs only above 32 logical code lines or 16 combined execution-content lines. Collapsed code keeps its head and collapsed output keeps its tail with an explicit omission marker; retained visible execution sections keep their own labels and boxes. `tool calls` has an independent automatic preview of its latest 16 rows, with `[N earlier tool calls omitted]` inside the box before them. This is display-only: the user explicitly chose unbounded authoritative retention per execution despite the memory trade-off. Borders and titles, live activity, diagnostics, display-gap, and accounting do not consume the source or execution budgets. `Ctrl-O` globally expands or restores source, tool-call, and execution output presentation together. Current live `read` activity remains status; detailed interrupted `read` evidence remains available.
- Execution content is ordered stdout, non-empty stderr, result or Python error, interruption details, then cancellation warning.
- Per-step accounting aggregates reported fields across that step's attempts into one muted right-aligned line after a blank separator; multiple attempts may be identified compactly. The session footer shows available session-wide input, output, cache-read, cache-write, reasoning, total-token, and cost totals right-aligned inside the prompt's always-present bottom rule. It never adds or moves a row, truncates its left side with `…` on narrow terminals to preserve rightmost totals and cost, and is absent without metrics and in configuration-only mode.
- Costs use ordinary `$…` with no lower-bound marker; unavailable metrics are omitted, including no invented zero. Each attempt appears in exactly one reliable display record so session totals need no duplicate suppression. Documentation retains the caveat that opaque retries may make displayed cost lower than billing.
- The configured prompt is bottom anchored between full-width top and bottom rules, wraps and preserves explicit newlines, grows upward to roughly one-third of terminal height with a three-row prompt-box minimum where dimensions permit, then shows its newest tail. `Enter` submits. The `Shift-Enter newline` header hint appears only when enhanced modified-key reporting is available; then `Shift-Enter` inserts a newline. Disabled and configuration-only views remain read-only.
- Terminal normalisation, stale-event rejection, authoritative settlement, progress-gap indication, follow-at-bottom scrolling, preserved manual scroll, tiny-terminal safety, dirty-only redraws, terminal restoration, and three-row mouse-wheel history scrolling remain intact.
- `tui-markdown` is used without Syntect. Provider, Arena, retry, cancellation, pricing, and model-context replay semantics otherwise remain unchanged.

## Overall validation

```sh
cargo fmt --all -- --check
cargo check --all-targets

env -u OPENAI_API_KEY \
  MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty \
  cargo test --all-targets -- --test-threads=1

cargo clippy --all-targets -- -D warnings

test "$(rg -l 'tui_markdown' src --glob '*.rs')" = "src/transcript.rs"
test "$(rg -l 'tree_sitter_(highlight|python)' src --glob '*.rs')" = "src/highlight.rs"
! cargo tree -e normal | rg -q '(^| )syntect v'

test "$(rg -l 'async_openai' src --glob '*.rs')" = "src/openai.rs"
test "$(rg -l 'monty_(pool|types)' src --glob '*.rs')" = "src/arena.rs"
test "$(rg -l 'cap_std' src --glob '*.rs')" = "src/tools.rs"
```

Expected: formatting, compilation, all credential-free tests, Clippy, and dependency-isolation checks pass; Syntect is absent.

Manual live validation:

```sh
OPENAI_API_KEY=... \
MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty \
cargo run
```

Expected: a multi-step request streams GitHub-Flavoured Markdown, retained reasoning, highlighted Python, live stdout, and settled card colours. `Ctrl-T`, `Ctrl-O`, scrolling, and the session footer work without changing agent behaviour.

## Planning context

- `ModelStep` returns action, opaque replay, and attempt accounting, but no authoritative readable reasoning summary (`src/model.rs:14-30`). OpenAI replay already preserves completed reasoning items, including encrypted content (`src/openai.rs:591-637`, `src/openai.rs:755-800`).
- Model history stores replay beside completed code or final messages (`src/model.rs:140-178`). Adding the readable summary there keeps model state complete while OpenAI request construction can continue replaying only the opaque provider items (`src/openai.rs:78-120`).
- Successful execution and final-message records currently omit reasoning (`src/runtime.rs:38-64`); interrupted terminal evidence already retains partial reasoning (`src/agent.rs:16-50`). Terminal attempt vectors are cumulative while completed-execution attempts are step-local, forcing the current TUI to recover terminal-step accounting by slicing prior attempts (`src/tui.rs:1057-1082`).
- Reliable records and lossy progress are intentionally separate. Completed summaries must follow the reliable path; streamed prefixes remain presentation-only (`src/runtime.rs:139-179`, `src/runtime.rs:439-496`).
- `App` currently mixes transcript authority, live state, event acceptance, input, scrolling, and terminal layout. Rendering reconstructs one flat string and repeatedly scans preceding records for accounting, making a private Display Projection the narrowest seam for Phase 2 (`src/tui.rs:129-167`, `src/tui.rs:853-886`).
- Phase 2 introduces `src/transcript.rs` as that private Display Projection. It owns transcript entries, live-to-settled replacement, stale-event rejection, presentation preferences, Markdown and card projection, wrapped-height measurement, and session accounting. `src/tui.rs` retains terminal ownership, prompt editing, command dispatch, viewport scrolling, and drawing.
- Terminal normalisation remains the final safety rule before dynamic text enters Markdown, highlighting, or Ratatui (`src/tui.rs:1238-1287`).
- `tui-markdown 0.3.9` supports Ratatui 0.30 and the selected GitHub-Flavoured Markdown features. Its Syntect-backed default feature is disabled.
- Tree-sitter highlighting is isolated in `src/highlight.rs` using `tree-sitter-highlight 0.26.13` and `tree-sitter-python 0.25.0`; plain text remains the display fallback.
- Ratatui's pinned `0.30.2` exposes matching wrapped-row measurement through `unstable-rendered-line-info`. Maki uses the same feature rather than maintaining duplicate wrapping rules.
- Pi's reference cards use one-cell-padded, borderless state backgrounds and the dark colours `#282832`, `#283228`, and `#3c2828` (`.research/pi/source/packages/coding-agent/src/modes/interactive/components/tool-execution.ts:56-73`, `.research/pi/source/packages/coding-agent/src/modes/interactive/theme/dark.json:17-19`).
- Existing tests are colocated module tests. TUI tests already provide fake input, TestBackend rendering, scrolling, stale-event, dirty-redraw, tiny-terminal, and terminal-restoration coverage (`src/tui.rs:1289-1349`, `src/tui.rs:1490-2790`).

## Tasks

Each task ends in one atomic `git commit --signoff` using its listed subject, after its verification passes.

### Task 1: Prove GitHub-Flavoured Markdown rendering

**Changes** — Add exact `tui-markdown 0.3.9` without default features and enable Ratatui's pinned `unstable-rendered-line-info` feature. Create the private transcript module with a narrow Markdown adapter. Prove selected GitHub-Flavoured Markdown rendering, custom styling, partial-prefix reparsing, and wrapped-height measurement. Raw HTML remains literal and images become text fallbacks.

**Tests** (`src/transcript.rs`, adapter compatibility) — First fail without the dependency. Cover headings, tables, task lists, links, blockquotes, strikethrough, fenced code, raw HTML, images, incomplete delimiters, fences and tables, valid UTF-8 prefixes, and agreement between measured and rendered wrapped rows.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`; Create: `src/transcript.rs`

**Commit** — `Prove GFM transcript rendering`

**Verification** — `cargo test transcript::tests::markdown_compatibility -- --test-threads=1 && cargo check --all-targets && test "$(rg -l 'tui_markdown' src --glob '*.rs')" = "src/transcript.rs" && ! cargo tree -e normal | rg -q '(^| )syntect v'` — the selected GitHub-Flavoured Markdown and wrapping fixtures pass without Syntect.

### Task 2: Add Tree-sitter Python highlighting

**Changes** — Add exact compatible Tree-sitter highlighter and Python grammar dependencies. Create one display-only Python highlighter which converts complete or incomplete source into Ratatui spans and preserves all normalised text. Any initialisation or highlighting error falls back to plain code.

**Tests** (`src/highlight.rs`, adapter unit) — First fail without the highlighter. Cover complete Python, every valid UTF-8 prefix of representative incomplete Python, comments, keywords, functions, strings and numbers, exact text reconstruction, newline preservation, and forced fallback.

**Files** — Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`; Create: `src/highlight.rs`

**Commit** — `Add Tree-sitter Python highlighting`

**Verification** — `cargo test highlight::tests -- --test-threads=1 && cargo check --all-targets && test "$(rg -l 'tree_sitter_(highlight|python)' src --glob '*.rs')" = "src/highlight.rs"` — complete, partial, and fallback fixtures preserve text and apply expected styles.

### Task 3: Preserve completed reasoning summaries

**Changes** — Add authoritative readable reasoning to `ModelStep` and store it beside opaque replay in completed-code and final-message model-history entries. OpenAI extracts summaries from the validated completed output in provider order; streamed chunks are not the authoritative source. Scripted steps and test models default to no summary. Carry the summary through Agent pending-code and staged-message values into model-history admission, without widening runtime records yet. OpenAI history construction continues replaying only the original opaque items.

**Tests** (`src/model.rs`, model history; `src/openai.rs`, adapter fixtures; `src/agent.rs`, context admission) — First fail when a completed fixture's readable summary is unavailable. Prove code and message summaries coexist with unchanged encrypted replay, multiple summary parts retain order, missing summaries are empty, malformed responses still fail closed, Agent admission retains the summary, and the next OpenAI request body is unchanged.

**Files** — Modify: `src/model.rs`, `src/openai.rs`, `src/agent.rs`, affected `ModelStep` fixtures in `src/runtime.rs`

**Commit** — `Preserve completed reasoning summaries`

**Verification** — `cargo test model::tests -- --test-threads=1 && cargo test openai::tests::action_protocol -- --test-threads=1 && cargo test openai::tests::streaming -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test agent::tests -- --test-threads=1` — readable summaries pass through Agent admission as authoritative application state without altering opaque replay or request construction.

### Task 4: Record reasoning in reliable outcomes

**Changes** — Widen the reliable presentation path established by Task 3: carry completed summaries through execution completion, interruption evidence, worker settlement, and canonical runtime records. Successful execution and final-message records receive authoritative summaries even when no progress arrived. Cancellation after a completed code action retains that summary in execution evidence; failures before completion retain only bounded progress evidence.

**Tests** (`src/agent.rs`, orchestration; `src/runtime.rs`, controller integration) — First fail with a model returning a completed summary but emitting no summary progress. Cover successful code, final message, cancellation before and during execution, Arena failure, queued-final cancellation, missing progress, unchanged context admission, and reliable record and event ordering.

**Files** — Modify: `src/agent.rs`, `src/runtime.rs`, affected fixtures in `src/tui.rs`

**Commit** — `Record reasoning in reliable outcomes`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test agent::tests -- --test-threads=1 && MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests -- --test-threads=1` — every completed or interrupted outcome carries the correct reliable summary without changing settlement semantics.

### Task 5: Scope terminal attempt accounting

**Changes** — Make reliable display accounting disjoint before the new projection depends on it. Keep each completed-execution record's step-local attempts; when committing a final, cancelled, or failed terminal record, retain only cumulative attempts not already represented by that Run's completed-execution records. In the same commit, update the Phase 1 terminal formatter to treat terminal attempts as step-local and combine preceding execution attempts with them only when calculating the temporary cumulative Run row. Keep Agent and pricing semantics unchanged.

**Tests** (`src/runtime.rs`, controller integration; `src/tui.rs`, compatibility rendering) — First fail when a multi-step terminal record repeats earlier execution attempts. Cover final message, cancellation, provider failure, Arena failure, cancellation immediately after a committed execution, multiple earlier executions, and exact record and event order. Prove every attempt appears in one reliable record, the terminal step still displays its residual attempts, and the Phase 1 cumulative estimate remains unchanged when folded.

**Files** — Modify: `src/runtime.rs`, `src/tui.rs`

**Commit** — `Scope terminal attempt accounting`

**Verification** — `MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test runtime::tests::terminal -- --test-threads=1 && cargo test tui::tests::transcript_renders_per_step_and_cumulative_costs_without_double_counting -- --exact --test-threads=1` — terminal records contain only uncommitted step attempts and the Phase 1 display remains correct.

### Task 6: Extract the transcript Display Projection

**Changes** — Move committed display entries, live-step state, run and step authority, stale-event rejection, progress-gap ownership, live-to-settled replacement, and presentation preferences from `App` into `src/transcript.rs`. Keep the current plain presentation temporarily. Replace repeated prefix scans with one forward fold. `App` retains terminal ownership, prompt editing, command dispatch, viewport scrolling, and drawing.

**Tests** (`src/transcript.rs`, projection unit; `src/tui.rs`, integration) — Start with characterisation tests for current behaviour. Then prove reliable records beat lossy progress, obsolete progress is ignored without visible change, retries reset only live attempt content, completed summaries survive replacement, old runs remain scrollable, gaps settle correctly, and current input and runtime behaviour is unchanged.

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`

**Commit** — `Extract transcript display projection`

**Verification** — `cargo test transcript::tests::projection -- --test-threads=1 && cargo test tui::tests -- --test-threads=1` — transcript authority is local to the new module with no visible regression.

### Task 7: Render structured streaming blocks

**Changes** — Replace the flat history string with typed transcript blocks rendered and measured independently. Assistant messages and reasoning use the GitHub-Flavoured Markdown adapter while streaming and after settlement. Execution output remains plain terminal-safe text. Render each block through the same wrapping rules used for height measurement, allowing partial viewport rendering without building an unbounded off-screen buffer. Preserve user, reasoning, arena, and final-message chronology plus every partial-generation form.

Move the single terminal normaliser to the final transcript-rendering seam so raw content is normalised before Markdown or highlighting and never normalised twice.

**Tests** (`src/transcript.rs`, projection and rendering; `src/tui.rs`, viewport integration) — First fail on styled GitHub-Flavoured Markdown output. Cover live-prefix convergence, authoritative replacement, final response below arena activity, interrupted reasoning, code and text forms, hostile controls in every source, CJK and emoji wrapping, progress gaps, viewport clipping, bottom-following, manual-scroll preservation, and tiny terminals.

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`

**Commit** — `Render structured streaming transcript`

**Verification** — `cargo test transcript::tests::rendering -- --test-threads=1 && cargo test tui::tests::transcript -- --test-threads=1 && cargo test tui::tests::normalise -- --test-threads=1` — typed blocks stream, settle, wrap, scroll, and normalise safely.

### Task 8: Add Pi-style cards and controls

**Changes** — Apply the fixed Pi-derived dark palette and borderless full-width cards: neutral padded user cards; two-cell-padded, subtly grey, italic reasoning cards; one-cell-padded blue, green, or red arena cards; and unboxed final assistant Markdown.

Add `Ctrl-T` reasoning visibility and placeholder behaviour. Add `Ctrl-O` global arena expansion. Automatic mode keeps short sections open, shows the first 32 code lines, and shows the last 16 combined execution-content lines with omission markers. Title, status, display-gap, usage, and cost metadata do not consume preview lines. Integrate Tree-sitter for live and settled arena Python.

**Tests** (`src/transcript.rs`, visual policy; `src/tui.rs`, input and render integration) — First fail on card styles and shortcuts. Assert TestBackend cell colours, padding, ordering, exact 32/33 and 16/17 boundaries, output-tail selection, Monty truncation plus display collapse, short-card behaviour, global toggles, hidden-reasoning placeholders, incomplete highlighted Python, dirty redraws, and scroll clamping after height-reducing toggles.

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`

**Commit** — `Add Pi-style transcript controls`

**Verification** — `cargo test transcript::tests::cards -- --test-threads=1 && cargo test tui::tests::controls -- --test-threads=1 && cargo test tui::tests::transcript_keyboard_scrolling -- --test-threads=1` — card states, preview policy, highlighting, shortcuts, and scroll behaviour match the specification.

### Task 9: Show compact transcript accounting

**Changes** — Replace verbose accounting rows with compact Pi-style per-step card metadata; remove the Phase 1 cumulative Run-cost row. Add one non-wrapping session-footer row immediately beneath the prompt, ordered input, output, cache-read, cache-write, reasoning, total tokens, then estimated cost. Truncate the row with an ellipsis at terminal width. Show it only when at least one metric is available and the terminal has at least three rows; header and prompt take priority at smaller heights. Dynamically reserve its row from the history viewport.

Fold disjoint completed-execution and terminal attempt records once, adding only the current live step's settled attempts while it remains provisional. Omit unavailable fields. Render complete and lower-bound known estimates identically as ordinary `$…`, as approved; never invent zero. Keep following the bottom when `scroll_from_bottom` is zero; otherwise preserve and safely clamp the offset when the footer appears or disappears.

**Tests** (`src/transcript.rs`, accounting projection; `src/tui.rs`, layout) — First fail on verbose rows. Cover all token categories, multiple attempts and Runs, active-to-settled replacement, one forward fold over disjoint reliable records, partially available usage, omitted unavailable metrics, ordinary-dollar lower-bound rendering, session persistence across prompts, exact footer ordering, one-row truncation with ellipsis, dynamic history-height reservation, footer appearance while following the bottom or manually scrolled, height-zero-to-three terminals, and no invented zero.

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`

**Commit** — `Show compact transcript accounting`

**Verification** — `cargo test transcript::tests::accounting -- --test-threads=1 && cargo test tui::tests::footer -- --test-threads=1` — card and session accounting remain accurate to reported data and fit the terminal layout.

### Task 10: Document transcript presentation

**Changes** — Document GitHub-Flavoured Markdown rendering, retained reasoning, card states, collapse thresholds, `Ctrl-T`, `Ctrl-O`, session footer fields, estimated-cost caveat, Tree-sitter fallback, and the manual live demonstration. Update visible TUI key hints to match.

**Tests** — No new behavioural test applies; controls and rendering are covered in Tasks 8 and 9. Review the documentation diff against the listed behaviour rather than coupling tests to prose.

**Files** — Modify: `README.md`, `src/tui.rs`

**Commit** — `Document transcript presentation`

**Verification** — `git diff --check -- README.md src/tui.rs && cargo fmt --all -- --check` — the reviewed documentation diff covers every listed behaviour and source remains formatted.

### Final test-drive refinement (approved)

**Changes** — Refine the transcript to remove user, RunId, and ModelStepId labels while retaining activity and event authority. Give reasoning two-cell horizontal and one-row top-and-bottom padding, and final assistant Markdown one-row top-and-bottom padding with no side padding, label, or border. Separate arena source, execution content, and accounting vertically. Aggregate reported fields across each step's attempts into one muted right-aligned accounting line after a blank separator, retaining it with its arena or final assistant step. Instruct the read-only agent not to repeat executed Python in its final reply unless the user asks to see it.

Use a fixed, ruled, bottom-anchored multiline prompt box that wraps and preserves explicit newlines, grows upward to roughly one-third of terminal height with a three-row prompt-box minimum where dimensions permit, and shows its newest tail. Enable `Shift-Enter` newline input only through enhanced modified-key reporting, with no fallback mapping. Embed the right-aligned session footer in the always-present bottom rule, truncating its left side with `…` to preserve the rightmost totals and cost. Update the README, Phase 2 spec, and acceptance criteria to describe the refined presentation.

Keep `set dotenv-load` in `justfile`, ignore root `/.env` in tracked `.gitignore`, and document that users may export variables for `cargo run` or put them in `.env` and use `just run`; plain `cargo run` does not load dotenv. Keep the opt-in live-test command explicit. No Phase 2 source-spec change is needed for this convenience.

**Tests** (`src/transcript.rs`, projection and rendering; `src/tui.rs`, controls and layout; `src/prompt.rs`, unit) — First fail on displayed labels, card padding and section separation, per-step accounting ownership and alignment, multiline prompt layout, footer placement or suffix preservation, modified Enter input, and the bundled instruction not to repeat executed Python unless asked. Cover unlabelled/id-free cards and concise activity without weakening stale-event rejection; reasoning and assistant padding; source, execution, and accounting separators; live and settled per-step accounting exactly once; wrapped, explicit-empty, trailing, and newest-tail prompt lines; prompt growth with bottom-following and manual scrolling; ruled small terminals and unchanged configuration-only rendering; enhanced-terminal `Shift-Enter`, byte limits, disabled input, soft enhancement failure, and restoration order; embedded footer alignment, absence, and left-side ellipsis truncation.

**Files** — Modify: `.gitignore`, `justfile`, `README.md`, `docs/2026-08-27_live-openai-agent/spec.md`, `docs/2026-08-27_live-openai-agent/phase-2_plan.md`, `prompts/read_only_agent.md`, `src/prompt.rs`, `src/transcript.rs`, `src/tui.rs`

**Commit** — `Refine transcript presentation`

**Verification** — Focused: `cargo test transcript::tests::feedback -- --test-threads=1 && cargo test tui::tests::shift_enter_edits_a_multiline_prompt_without_submitting -- --exact --test-threads=1 && cargo test tui::tests::keyboard_enhancement_pops_before_terminal_restore_and_fails_soft -- --exact --test-threads=1 && cargo test tui::tests::prompt_ -- --test-threads=1 && cargo test tui::tests::footer -- --test-threads=1 && cargo test tui::tests::status_header_uses_concise_activity_without_identifiers -- --exact --test-threads=1`. Full: `cargo fmt --all -- --check && cargo check --all-targets && env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1 && cargo clippy --all-targets -- -D warnings`. Documentation: `git diff --check -- README.md docs/2026-08-27_live-openai-agent/spec.md docs/2026-08-27_live-openai-agent/phase-2_plan.md` plus stale-phrase scans — the refined transcript, prompt, input, footer, and documentation remain correct. Dotenv convenience: `just --list && git check-ignore .env && git diff --check -- .gitignore justfile README.md docs/2026-08-27_live-openai-agent/phase-2_plan.md`, plus README review — dotenv launch is valid, root `.env` is ignored, and `cargo run` is not represented as dotenv-loading.

### Nested arena-box follow-up (approved)

**Changes** — Replace the arena's borderless presentation with one full-width rounded outer lifecycle frame. Inside it, render separately closed, labelled available sections in order: `code`, `stdout`, non-empty `stderr`, `result` or `error`, interrupted `read`, `failure`, `warning`, and current `stdout preview`. Keep ordinary settled frames untitled; retain concise live/error headings, statuses, and diagnostics exactly once. Keep live `read` activity as status. The historical instruction not to retain successful settled reads as a fabricated call list is superseded by the approved persistent tool-call readout below. Keep accounting unboxed, muted, and right-aligned after its blank separator. Use only the existing lifecycle fills and muted borders; degrade nesting safely at narrow widths. Keep the renderer private behind `TranscriptLayout::{total_height, footer, render}` without recreating scrolling policy in `tui.rs` or changing runtime, model, or arena authority.

**Tests** (`src/transcript.rs`, rendering and projection; `src/tui.rs`, viewport scrolling) — First replace stale flat/borderless assertions with a failing TestBackend fixture proving a closed rounded outer frame and closed `code`, `stdout`, `stderr`, and `result` boxes in canonical order with unprefixed bodies. Cover untitled ordinary settlement; one live heading/status; error and interrupted labels; omission of empty sections; unboxed right-aligned accounting and its separator; the 32-source and 16-execution budgets with `Ctrl-O`; highlighted normalised Python and literal execution text; lifecycle fills and muted borders; CJK/emoji wrapping with repeated rails; direct viewport clipping and scrolling; and widths 0 through narrow nesting without panic or loss of renderable text. Keep semantic projection tests meaningful.

**Files** — Modify: `README.md`, `docs/2026-08-27_live-openai-agent/spec.md`, `docs/2026-08-27_live-openai-agent/phase-2_plan.md`, `src/transcript.rs`, `src/tui.rs`

**Verification** — Focused: `cargo test transcript::tests -- --test-threads=1 && cargo test tui::tests::transcript -- --test-threads=1 && cargo test tui::tests::controls -- --test-threads=1`. Full: `cargo fmt --all -- --check && cargo check --all-targets && env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1 && cargo clippy --all-targets -- -D warnings`. Dependency isolation: `test "$(rg -l 'tui_markdown' src --glob '*.rs')" = "src/transcript.rs" && test "$(rg -l 'tree_sitter_(highlight|python)' src --glob '*.rs')" = "src/highlight.rs" && ! cargo tree -e normal | rg -q '(^| )syntect v'`. Diff: `git diff --check`.

### Persistent tool-call readout follow-up (approved)

**Decision** — Supersede the stale no-successful-read-retention direction above. The user explicitly chose unbounded authoritative retention for each Python execution despite the memory trade-off. Display collapse, not storage, is the bound. Historical tasks remain historical.

**Changes** — Add the smallest Arena semantic trace: one ordered `ReadCall` row for every valid host `read`, recorded as `Pending` at start and replaced from authoritative `ReadOutcome` on settlement. Carry it in completed `Execution` and `InterruptedExecution`, then copy interrupted traces into `ExecutionEvidence`, including recovery failures. Do not add runtime events, progress events, model-history fields, `CodeResult` feedback, or provider wire fields.

Project the typed trace through `ArenaBlock` and `NormalisedArenaBlock`, then privately format a closed `tool calls` section immediately after `code`. Rows are only `read · N lines`, `read · failed`, or `read · pending`; they store and display no paths, arguments, error text, or user text. On settled cards, automatic mode shows the newest 16 rows and `[N earlier tool calls omitted]` before them inside that section. `Ctrl-O` uses the existing shared arena expansion state to show every retained row while the 32-line source and 16-line combined execution-content budgets remain independent. Existing live read status/path and detailed interrupted-read evidence stay unchanged.

**Tests** — Arena tests cover ordered success/failure rows, cancellation while a read awaits, cancellation after settlement before progress, and 20 retained calls. Agent tests cover interrupted and recovery evidence copying with unchanged model feedback. Runtime tests prove a reliable completed-execution record retains the trace after lossy tool progress drops. Transcript and TUI tests cover section order, exact path-free rows, live-to-reliable replacement, the independent last-16 preview and omission marker, `Ctrl-O`, and nested-frame wrapping and scrolling.

**Files** — Modify: `README.md`, `docs/2026-08-27_live-openai-agent/spec.md`, `docs/2026-08-27_live-openai-agent/phase-2_plan.md`, `src/arena.rs`, `src/agent.rs`, `src/runtime.rs`, `src/transcript.rs`, `src/tui.rs`.

**Verification** — Focused Arena, Agent, Runtime, Transcript, and TUI tests; credential-free all-target tests with `MONTY_BIN`; `cargo fmt --all -- --check`; `cargo check --all-targets`; `cargo clippy --all-targets -- -D warnings`; dependency-isolation checks; and `git diff --check`.

### Post-08df866 approved feedback (#12, #13, #14)

**Decision** — Supersede the earlier source-head and standalone code/output omission-marker directions above. Historical tasks remain historical; tool-call omission remains an in-body tool-call row.

**#12** — In automatic mode, retain the latest 32 logical code lines rather than the first 32. `Ctrl-O` restores the exact full source.

**#13** — Keep the latest 16 combined execution-content lines in canonical section order. Attribute skipped code and execution lines to their original sections and title each affected closed section `<label> · N earlier line(s) omitted`. Retain a non-empty execution section as a closed two-border box when the shared tail budget hides its whole body. Do not render standalone or body omission markers for code or execution content. Keep lifecycle fills, muted borders, body padding, direct clipping, terminal normalisation, syntax highlighting, accounting, and `Ctrl-O` expansion unchanged; narrow widths fall back to wrapped labels without overwriting a section corner.

**#14** — Replace the direct prompt string with a private, focused `PromptEditor`. It owns UTF-8 text, a grapheme-boundary caret, preferred visual column, Unicode deletion, hard-wrap measurement, and its caret-visible prompt viewport. `App` only maps terminal events to semantic edits and retains history, runtime, and terminal policy. The configured prompt has a hardware cursor; arrows edit, Page keys scroll history, and read-only views keep history navigation.

**Tests** — First fail on a 33-line source retaining its 32-line tail with a singular border-owned count and `Ctrl-O` restoration. Cover combined output-tail attribution through stdout, stderr, result, and a fully omitted retained section; per-section counts sum to the prior global omission count. Cover pluralisation, narrow fallback/wrapping, intact corners, and the unchanged in-body tool-call marker. Add `src/prompt_editor.rs` seam tests for middle insertion/take, extended grapheme movement and deletion, middle 16 KiB rejection, UAX#29 word and logical-line operations, visual CJK/emoji navigation, explicit empty/trailing rows, exact full-row cursor placement, and viewport following. Extend `src/tui.rs` TestBackend tests for cursor visibility and coordinates, modifier and Ctrl fallbacks, editable versus read-only navigation, enhanced `Shift-Enter`, terminal-safe input, transcript scrolling, and nested-arena coexistence.

**Files** — Add: `src/prompt_editor.rs`. Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/transcript.rs`, `src/tui.rs`, `README.md`, `docs/2026-08-27_live-openai-agent/spec.md`, and this plan.

**Verification** — Run focused transcript-collapse, prompt-editor, TUI control/history/terminal, and nested-scrolling tests; transcript and full TUI suites; `cargo fmt --all -- --check`; `cargo check --all-targets`; credential-free all-target tests with `MONTY_BIN`; `cargo clippy --all-targets -- -D warnings`; dependency isolation including `unicode_segmentation` only in `src/prompt_editor.rs`; and `git diff --check`.

**Commit** — `Improve transcript editing`

### Post-100b064 approved feedback (mouse-wheel transcript scrolling)

**Decision** — Supersede only the earlier blanket mouse-interaction non-goal. Historical tasks remain historical: wheel scrolling changes neither prompt editing nor runtime input authority; other mouse events remain ignored.

**Changes** — Enable Crossterm mouse capture after terminal initialisation for configured and configuration-only sessions. Disable it before normal Ratatui restoration on success, loop error, and enable failure, and via `Drop` during panic unwind. Report mouse-capture enable I/O errors as `TuiError::Terminal`. Route `ScrollUp` and `ScrollDown` in both loops through `App`'s existing clamped scroll update, by three rows per event.

**Tests** — At the configured and configuration-only loop seams, cover exact three-row up scrolling, down scrolling to newest history, and ignored mouse input. At the capture lifecycle seam, cover success, loop error, enable failure, and panic cleanup.

**Files** — Modify: `README.md`, `docs/2026-08-27_live-openai-agent/spec.md`, this plan, `src/tui.rs`.

**Verification** — Focused mouse-wheel and capture-lifecycle TUI tests; `cargo fmt --all -- --check`; `cargo check --all-targets`; and `git diff --check`.
