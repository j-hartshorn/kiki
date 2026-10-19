# Plan: Settings menu and themes — Phase 1: Pi themes and the `theme` setting

> **Approved** — spec review READY, simplification findings folded in, user-approved.

## Source
- Spec: `docs/2026-10-14_settings-and-themes/spec.md`, Phase 1.
- Deferred to phase 2 (not needed until the picker exists): `SettingsContext::save_theme` (spec Seams §3) and theme-keyed transcript rasters (Seams §4). In phase 1 the theme is fixed for the life of the process.

## Acceptance criteria
- With `theme = "dark"`, every element in the spec's element → token table (except the phase-2 pickers) renders with Pi 1.2.0 `dark.json` colours; with `theme = "light"`, with `light.json` colours.
- With `theme` unset or `"automatic"`, Kiki starts light on a light terminal and dark on a dark one (OSC 11, then `COLORFGBG`, then dark). Detection runs once per process, before the first TUI screen, and only for commands that open the TUI.
- Detection never blocks longer than 500 ms and never fails startup. Kiki doesn't query the terminal when stdin or stdout is not a terminal.
- Recognised fenced code is syntax-highlighted with the theme's `syntax*` tokens. Inline code and unrecognised fences use `mdCode` with no background. No markdown element keeps a `tui-markdown` ANSI default colour.
- An unsupported `theme` value (`theme = "blue"`, `theme = 1`) is a settings problem shown on the existing configuration screen, with remediation naming `dark`, `light` and `automatic`. A project value overrides the user value.
- Text selection keeps reverse video; the page background stays the terminal's own.

## Overall validation
- `just test` — all tests pass.
- `just lint` — clean (it is clean on `main` today).
- `cargo fmt --all -- --check` — no diff.
- `node docs/2026-10-14_settings-and-themes/pi-theme-hex.mjs /opt/homebrew/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/@earendil-works/pi-tui/dist/colors.js .research/pi/source/packages/coding-agent/src/modes/interactive/theme/dark.json .research/pi/source/packages/coding-agent/src/modes/interactive/theme/light.json` — prints Pi's resolved hex for every token; every token used in `src/theme.rs` matches.
- Manual (user):
  - `just run` in a dark terminal and in a light terminal with `theme` unset. Each should pick the matching theme, with no stray characters in the prompt.
  - Repeat inside tmux.
  - Set `theme = "light"`, then `"dark"`, in `~/.kiki/config.toml`.
  - Check a reply containing Rust and Python fences, a table, a blockquote and a `> [!NOTE]` alert. Check the tree (`/tree`), the model picker, slash completion and the thinking rule across levels.

## Planning context
- Settings precedent: `cross_provider_reasoning` is a top-level string with project-over-user precedence, validated in `validate_document` (`src/config/settings.rs:734-775`). The unknown-field message lists allowed keys and must gain `theme` (`settings.rs:767-770`).
- Render blocks are built from normalised sources on each layout (`src/transcript.rs:2943-3010` → `build_render_block`). Threading `theme` through `layout`/`build_render_block` reaches every transcript style; no style is stored at push time.
- Transcript palette helpers `page_style`, `muted_page_style`, `card_style`, `muted_card_style` and `dim_card_style` (`src/transcript.rs:434-462`) have about 100 call sites. `page_style`/`muted_page_style` are also used by `src/tui.rs:2374,2807,3140` and `src/tui/subagents.rs:745`.
- `src/transcript.rs:4284-4286` is the context-usage footer (red at ≥90%, yellow at ≥80%). The spec calls it "configuration error / warning"; it maps to `error`/`warning`.
- Picker default focus is reverse video (`src/tui/picker.rs:289-293`); the tree supplies its own `focused_row` (`src/tui/tree.rs:420-436`).
- `markdown()` builds `Options` on every call (`src/transcript.rs:52-62`); the style sheet overrides only `heading` and `link` (`:39-50`). The other `StyleSheet` defaults are in `tui-markdown-0.3.9/src/style_sheet.rs:44-160`.
- TUI startup has three terminal sessions, each via `ratatui::try_init` (`src/tui.rs:2215,2250,2268`): the project-skill trust prompt, the configuration screen and the main loop. They are reached from `run_fresh` and `run_resume` (`src/main.rs:316-420,424-570`). `List`, `Login`, `Logout` and `Version` never open a TUI (`src/main.rs:580-592`).
- crossterm reads stdin when it is a tty, else `/dev/tty` (`crossterm-0.29.0/src/terminal/sys/file_descriptor.rs:124-139`). `rustix` 1 is already a dependency with only `fs` (`Cargo.toml`). A deadline read needs its `event` (poll) and `stdio` features.
- No build or CI check asserts syntect's absence; the old decision exists only in docs.
- Conventions: test-first; tests live in `#[cfg(test)] mod tests` in the same file; commits signed off (`git commit -s`); work in `worktrees/settings-and-themes` on `work/settings-and-themes`.

## Tasks

### Task 1: Dark theme values and transcript on `&Theme`
**Changes** — New `src/theme.rs` with `Theme` and `theme::DARK` (spec Seams §1), holding Pi 1.2.0 dark values as hex-derived `Color::Rgb` for only the tokens this task's transcript changes read. Tasks 2 and 3 add their fields alongside their consumers, so no commit carries unread fields. Transcript layout and its palette helpers take `&Theme`; `App` holds `&'static Theme` (`&DARK` for now) and passes it to layout, `subagents.rs` and the `page_style` users in `tui.rs`. Mappings: page/card text, muted, dim; user card `userMessageBg`; Arena and compaction cards `toolPendingBg`/`toolSuccessBg`/`toolErrorBg`; reasoning `thinkingText` italic on `export.cardBg`; context footer `error`/`warning`.
**Tests** (`src/theme.rs` unit; `src/transcript.rs` unit) — Red first: update `cards_use_pi_palette_and_full_width_fills` (`src/transcript.rs:10885`) and other existing transcript colour assertions to Pi 1.2.0 dark values. Extend the existing footer threshold test (`src/transcript.rs:8637-8707`) to theme tokens and add a 79.9% (muted) case. Add a reasoning card check (italic, `thinkingText` on `cardBg`). In `theme.rs`, pin five or more `DARK` tokens to the generator's hex strings.
**Files** — Create: `src/theme.rs`. Modify: `src/main.rs` (module), `src/transcript.rs`, `src/tui.rs`, `src/tui/subagents.rs`.
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass. The generator command's dark values match `src/theme.rs`.

### Task 2: Tree, pickers, prompt and Arena highlighter on `&Theme`
**Changes** — Add the `Theme` fields these renderers read (`accent`, `success`, `error`, `warning`, `border`, `borderAccent`, `selectedBg`, the seven `thinking*` stops, and `syntaxComment`/`syntaxKeyword`/`syntaxFunction`/`syntaxString`/`syntaxNumber`). Remove the colour constants in `src/tui/tree.rs:29-38`, `src/tui.rs:3261-3266` and `src/highlight.rs:98-107`. Tree uses `text`/`accent`/`success`/`error`/`warning`/`muted`/`dim`/`border`/`selectedBg`, with compaction rows in `borderAccent`. The picker's default focused row and the slash-completion selected row use `text` on `selectedBg`, not reverse video. The slash-command token is `accent` bold. The thinking rule keeps its position-based interpolation over the seven stops `thinkingOff`…`thinkingMax`. The Python highlighter maps comment/keyword/function/string/number to the `syntax*` tokens.
**Tests** (`src/tui/tree.rs`, `src/tui/picker.rs`, `src/tui.rs`, `src/highlight.rs` unit) — Red first: update the tree colour assertions (`tree.rs:1559,1804,2562-2640`) and picker focus assertions (`picker.rs:637-692,805`) to the new tokens, with no `REVERSED` on focus. Add:
- the existing slash selected-row test (`src/tui.rs:5414-5446`) expects `text` on `selectedBg` with no `REVERSED`;
- thinking rule colours for 1, 3, 5 and 7 levels: the lowest level is always `thinkingOff`, and with more than one level the highest is `thinkingMax`;
- the existing highlighter test (`src/highlight.rs:166-191`) expects each Python capture's `syntax*` colour.

The selection reverse-video tests stay unchanged.
**Files** — Modify: `src/theme.rs`, `src/tui/tree.rs`, `src/tui/picker.rs`, `src/tui.rs`, `src/highlight.rs`, `src/transcript.rs` (highlighter call site).
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass.

### Task 3: Markdown on `md*` tokens with syntect-highlighted fences
**Changes** — Add the remaining `Theme` fields (`md*`, `borderMuted`, the other `syntax*` tokens). Enable `tui-markdown`'s `highlight-code` feature. `MarkdownStyleSheet` holds `&'static Theme` and maps every style in the spec's markdown rows (headings, links, quotes, table header and border, alerts, math, metadata, dim elements). Inline code and unrecognised fences use `mdCode` with no background. A TextMate theme is generated from the `syntax*` tokens. Suggested scope groups:
- comment;
- keyword and storage;
- entity.name.function and support.function;
- variable;
- string;
- constant.numeric and constant.language;
- entity.name.type, support.type and storage.type;
- keyword.operator;
- punctuation.

Options are built once per theme and borrowed per render (spec Seams §1, §4). `markdown(source, theme)` replaces the current builder.
**Tests** (`src/transcript.rs` unit) — Red first, against `DARK`:
- `markdown_compatibility` keeps its text expectations;
- each table row has its style (heading `mdHeading` bold, link `mdLink` underlined, blockquote `mdQuote`, table border `borderMuted`, NOTE alert `mdLink`, CAUTION alert `error`);
- inline code `mdCode` with `bg` unset;
- `rust` fence: `let` is `syntaxKeyword` and `1` is `syntaxNumber`;
- unknown-language fence: `mdCode` with no background;
- the generated TextMate theme parses.

Also an `#[ignore]` timing test, `markdown_highlight_cost`, rendering a 400-line Rust fence and printing ms per render.
**Files** — Modify: `src/theme.rs`, `Cargo.toml`, `Cargo.lock`, `src/transcript.rs`.
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass. `cargo test --release markdown_highlight_cost -- --ignored --nocapture` prints the per-render time; record it in the task summary and flag it if it exceeds 5 ms.

### Task 4: Light theme and the `theme` setting (fixed or automatic via `COLORFGBG`)
**Changes** — Add `theme::LIGHT`, `ThemeSetting` (`parse`, `as_str`, `resolve`) and `Appearance` (spec Seams §1). `SettingsContext::theme()` and validation follow spec Seams §3, with `theme` added to the unknown-field message. Add a pure `COLORFGBG` → `Option<Appearance>` rule (spec Proposed approach, Detection). For TUI commands only, `main.rs` computes the startup appearance from `COLORFGBG`, falling back to dark, and passes it to all three TUI entry points. `App` keeps the appearance for phase 2 and renders with `settings.theme().resolve(appearance)`. `Automatic` applies only when no valid layer supplies a theme. An invalid layer is reported and skipped as for other fields, so user `light` with project `blue` still renders light.
**Tests** — Red first:
- `src/theme.rs` unit:
  - pin five or more `LIGHT` tokens, and extend the Task 3 generated-TextMate-theme parse test to `LIGHT`;
  - the `resolve` table (3 settings × 2 appearances);
  - `parse`/`as_str` round-trip, with `"Dark"`, `""` and `"blue"` rejected;
  - the `COLORFGBG` table: `"15;0"` dark, `"0;15"` light, `"0;7"` light, `"0;8"` dark, `"default;default"` none, `"0;16"` none, `""` none.
- `src/config/settings.rs` unit: default `Automatic`; user `light`; project `dark` over user `light`; `"blue"` and `1` each produce one problem naming the three values; project `"blue"` with user `light` reports the project problem and resolves `Light`.
- `src/tui.rs` unit: with `theme = "light"` the user card background is `LIGHT.user_message_bg`; with `automatic` and `Appearance::Light`, the same.
**Files** — Modify: `src/theme.rs`, `src/config/settings.rs`, `src/config.rs` (if the snapshot needs an accessor), `src/main.rs`, `src/tui.rs`.
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass. The generator's light values match. Manual: `theme = "light"` renders light.

### Task 5: OSC 11 startup detection
**Changes** — `detect_appearance(&mut impl TerminalQuery, colorfgbg)` and the `TerminalQuery` seam (spec Seams §2): write OSC 11 then DA1; read until the DA1 reply or 500 ms; parse `rgb:` replies (1–4 hex digits per channel, BEL or ST); light when OKLCH L ≥ 0.5. Otherwise fall back to the Task 4 `COLORFGBG` rule, then dark. The production `TerminalQuery` puts the tty in raw mode for the query only, polls stdin with a deadline (rustix `event`/`stdio`) and restores the mode on drop. `main.rs` replaces the Task 4 `COLORFGBG`-only computation with `detect_appearance`, skipping the query when stdin or stdout is not a terminal.
**Tests** (`src/theme.rs` unit, scripted `TerminalQuery`) — Red first:
- black and white backgrounds, in 4-digit and 2-digit forms, with BEL and ST terminators;
- a mid-grey pair either side of L = 0.5;
- a reply split across three reads;
- DA1 only → `COLORFGBG` used;
- silence → returns at the deadline using the fallback (deadline injected, so the test does not sleep 500 ms);
- garbage before DA1 → fallback;
- bytes queued after DA1 are not read;
- a write error → fallback.
**Files** — Modify: `src/theme.rs`, `src/main.rs`, `Cargo.toml` (rustix features), `Cargo.lock`.
**Verification** — `just test`, `just lint` and `cargo fmt --all -- --check` all pass. Manual: `just run` in light and dark terminals and inside tmux picks the right theme with no stray prompt input. `just run < /dev/null` or under a pipe starts without the query.
