# Spec: Settings menu and themes

> **Approved** — spec review READY, simplification findings folded in, user-approved.

## Context

Kiki's colours are hard-coded constants and named ANSI colours copied from Pi's former hex `dark.json`, spread across the transcript, session tree, prompt rule, slash completion and Python highlighter (see Key discoveries). There is no theme abstraction, no light theme, and no interactive settings menu: settings come only from `~/.kiki/config.toml` and `.kiki/config.toml`.

Pi 1.2.0 (vendored at `.research/pi/source`, identical to the installed release) has replaced both built-in themes with okhsl palettes and offers a `/settings` menu whose Theme item supports a fixed theme or automatic light/dark selection. Kiki should follow: both of its themes move to Pi's new palettes, a light theme becomes available, and users can choose a theme interactively or let Kiki follow the terminal.

## Goals

- Kiki's dark and light themes reproduce Pi 1.2.0's `dark.json` and `light.json` palettes for every element Kiki currently colours.
- `theme = "dark" | "light" | "automatic"` in user settings, defaulting to `automatic`; automatic picks light or dark from the terminal background at startup.
- A `/theme` picker and a new `/settings` menu with a Theme item; moving through themes previews live, Enter applies and saves, Esc restores.
- Markdown uses Pi's `md*` tokens instead of `tui-markdown`'s ANSI defaults; fenced code in a recognised language is syntax-highlighted with Pi's `syntax*` tokens on the card background (no grey code background).
- Colours are supplied to renderers explicitly, so each theme is testable without global state, and theme changes re-render cached transcript blocks.

## Non-goals

- Following terminal light/dark switches while Kiki runs (mode 2031 notifications); appearance is detected once at startup.
- Custom or user-defined themes, Pi's `system` theme generated from terminal colours, and Pi's `"<light>/<dark>"` theme pairs.
- Any `/settings` item other than Theme; theme environment variables or command-line flags.
- Colouring unrecognised, language-less or indented code blocks: with `highlight-code` enabled, `tui-markdown` 0.3.9 applies no style to blocks it cannot highlight (`tui-markdown-0.3.9/src/renderer/code.rs:57-61,117-129`), so they render in the card's `text` colour (agreed after implementation found this; a fork or post-processing was rejected as not worth carrying). Styling code fences or list bullets separately.
- Colouring elements that currently use the terminal's default colours (the prompt editor text, unstyled picker rows).
- Painting the page background: Kiki keeps the terminal's own background, as today.

## Key discoveries

- Current palette: `src/transcript.rs:434-461` (page, text, muted, dim, user, arena-state and reasoning backgrounds and card styles), `src/tui/tree.rs:29-38` (text, accent, success, error, warning, compaction, muted, dim, selected background, border), `src/tui.rs:3254-3266` (slash-command token colour; six-step thinking-rule gradient), `src/highlight.rs:98-107` (comment, keyword, function, string, number), `src/transcript.rs:48` (links), `src/transcript.rs:4284-4286` (configuration error/warning). These match Pi's former `dark.json` hex values.
- Pi 1.2.0 tokens: `.research/pi/source/packages/coding-agent/src/modes/interactive/theme/dark.json` and `light.json`, defining every token Kiki needs (`text`, `muted`, `dim`, `accent`, `border`, `borderAccent`, `success`, `error`, `warning`, `selectedBg`, `userMessageBg`, `toolPendingBg`, `toolSuccessBg`, `toolErrorBg`, `thinkingText`, `mdLink`, `syntax*`, `thinking{Off..Max}`). Pi colours compaction tree rows with `borderAccent` (`.research/pi/source/packages/coding-agent/src/modes/interactive/components/tree-selector.ts:829`).
- Pi detects appearance by querying OSC 10/11/4 colours and classifying the background, falling back to `COLORFGBG` (`.research/pi/source/packages/tui/src/terminal-colors.ts:39-91`; `.research/pi/source/packages/coding-agent/src/modes/interactive/theme/theme-controller.ts:188-246`; `theme.ts:608`).
- Kiki reads input through crossterm 0.29 `poll`/`read` (`src/tui.rs:90-103`). Its Unix parser turns an unsolicited `ESC ] …` OSC reply into an Alt-key press followed by literal characters (`crossterm-0.29.0/src/event/sys/unix/parse.rs:78-87`), parses DA1 replies (`parse.rs:180-184`) and silently drops other `CSI ? … n` reports.
- Markdown renders through `tui-markdown` 0.3.9 with default features off (`Cargo.toml:35`); Kiki's stylesheet overrides only headings and links (`src/transcript.rs:39-50`), so inline and fenced code use the crate default `white().on_black()` (`tui-markdown-0.3.9/src/style_sheet.rs:60-62`; `renderer/code.rs:37-58`), and blockquotes, tables and alerts use ANSI defaults. Its `highlight-code` feature highlights recognised fences through syntect with a TextMate theme accepted from a string (`code_theme.rs:36-50`), without painting the theme background (`renderer/code.rs:95-100`); it enables syntect 5 with default features, including the Oniguruma C regex backend.
- Leaving syntect out was an earlier deliberate decision with a dependency check (`docs/2026-08-27_live-openai-agent/phase-2_plan.md:21,37`).
- Kiki's thinking levels are `low`, `medium`, `high`, `xhigh`, `max` (`src/config/models.rs:84`), matching Pi's `thinkingLow`…`thinkingMax` tokens.
- Pi classifies a background as dark when its OKLCH lightness is below 0.5 and waits 100 ms for colour replies (`.research/pi/source/packages/coding-agent/src/modes/interactive/theme/theme.ts:164-177`; `theme-controller.ts:25`).
- Settled transcript blocks are cached as rasters keyed only by width (`src/transcript.rs:2973-2983`); a theme change must invalidate them.
- Pickers apply on Enter and save with Ctrl-S (`src/tui/picker.rs:166-172`; `src/tui.rs:1059-1075`); user-default writes go through `SettingsContext` with masked-save reporting (`src/tui.rs:1253-1260`; `src/config/settings.rs:272-300`).

## Key decisions & tradeoffs

- Both themes adopt Pi 1.2.0's okhsl palettes; the hex-era colours are replaced.
- Scope **B**: `/settings` with a Theme item, `/theme` shortcut, and automatic light/dark mode.
- Appearance is detected **once at startup**, whatever the initial setting, and reused when automatic is chosen in the picker: an OSC 11 background query followed by a DA1 (`CSI c`) sentinel, read before the input loop starts, with a 500 ms deadline that ends early on the DA1 reply; best effort, with a documented residual late-reply risk. Rejected: a one-shot post-timeout input filter (special-case code on the input path); Pi's 100 ms deadline (less headroom on slow links). Rejected: live following (needs a raw-input filter in front of crossterm for every key, paste and mouse event); `COLORFGBG`-only (unset or unreliable in common terminals).
- Setting `theme = "dark" | "light" | "automatic"`, default `automatic`; automatic falls back to `COLORFGBG`, then dark. Rejected: Pi's theme pairs (Kiki has two built-in themes, no custom ones); default `dark`.
- Theme picker follows **Pi's convention**: focus previews live, Enter applies and saves to `~/.kiki/config.toml`, Ctrl-S behaves like Enter, Esc restores the theme active when the picker opened. Theme is not a session preference. Rejected: Kiki's Enter-applies/Ctrl-S-saves convention.
- Renderers receive the theme **explicitly** as `&Theme`; the transcript raster cache is keyed on theme as well as width. Rejected: a thread-local current theme.
- Markdown code uses `tui-markdown`'s syntect `highlight-code` feature with a TextMate theme generated from each Kiki theme's `syntax*` tokens. This reverses the earlier no-syntect decision for visual cohesion with Pi and Arena highlighting. Rejected: one `mdCode` colour for all code (no highlighting); tree-sitter highlighting of fenced blocks (Python only, needs fences taken out of `tui-markdown`).
- Reasoning cards keep a background, using Pi's `export.cardBg` (the only Pi card tone distinct from the tool backgrounds). Rejected: Pi's card-less thinking.
- The thinking rule keeps Kiki's position-based gradient over each model's levels, with Pi's seven `thinking*` tokens as stops. Rejected: Pi's name-based mapping (needs a fallback for unknown level names; a model's top level may never reach the strongest colour).
- Picker focus and slash-completion selection use `text` on `selectedBg`; text selection keeps reverse video. Rejected: reverse video everywhere (heavy and patchy on coloured rows, inconsistent with the tree).

## Proposed approach


**Theme values.** A new `src/theme.rs` defines `Theme` (name plus the Pi tokens Kiki uses, as `ratatui` RGB colours) and two constants, `DARK` and `LIGHT`, whose values are Pi 1.2.0's `dark.json`/`light.json` resolved from okhsl to sRGB with Pi's own conversion. Values are written into the source once; a unit test pins a few against Pi's resolved hex output so a transcription slip fails.

**Element → token mapping.**

| Kiki element | Pi token |
|---|---|
| Transcript/tree text, muted, dim | `text`, `muted`, `dim` |
| User card background | `userMessageBg` |
| Arena / compaction card background: in progress, complete, error | `toolPendingBg`, `toolSuccessBg`, `toolErrorBg` |
| Reasoning card | `thinkingText` italic on Pi's `export.cardBg` (dark grey in dark, very light grey in light) — agreed to keep the card |
| Markdown headings (all levels), links, blockquotes | `mdHeading` bold, `mdLink` underlined, `mdQuote` |
| Markdown table header, table border | `text` bold, `borderMuted` |
| Markdown alerts: note, tip, important, warning, caution | `mdLink`, `success`, `accent`, `warning`, `error` |
| Markdown math (inline italic, display) | `mdCode` |
| Markdown metadata block | `muted` |
| Markdown heading meta, raw HTML, footnotes, image alt text | `dim` (keeping the crate's italic where it has one) |
| Inline code | `mdCode`, no background |
| Unrecognised, language-less or indented code blocks | card `text` (crate applies no style; agreed) |
| Recognised fenced code | `syntaxComment`, `syntaxKeyword`, `syntaxFunction`, `syntaxVariable`, `syntaxString`, `syntaxNumber`, `syntaxType`, `syntaxOperator`, `syntaxPunctuation` via generated TextMate theme |
| Arena Python highlighter (comment, keyword, function, string, number) | matching `syntax*` tokens |
| Tree accent, success, error, warning, border, selected row | `accent`, `success`, `error`, `warning`, `border`, `selectedBg` |
| Tree compaction rows | `borderAccent` |
| Slash-command token | `accent` bold |
| Picker focused row and slash-completion selected row | `text` on `selectedBg`, replacing reverse video (matches the tree and Pi; explicit `text` keeps the row readable whatever the terminal's default colours) — agreed |
| Prompt thinking rule | Kiki's position-based gradient (each model's levels spread evenly across the stops, lowest level always the first stop), with Pi's seven `thinkingOff`…`thinkingMax` tokens as the stops — agreed |
| Configuration error / warning lines | `error`, `warning` |
| Prompt and transcript text selection | unchanged reverse video (only highlight guaranteed visible over arbitrary styled content) — agreed |

**Setting.** `theme` joins the existing settings document as a top-level string, with the usual project-over-user precedence and validation (an unsupported value is a settings problem with remediation listing `dark`, `light`, `automatic`). Saving writes the user file through `SettingsContext`; a project value that masks the save is reported like a masked model save.

**Detection** (agreed: startup only; choosing automatic in the picker reuses the startup result, because a mid-session OSC reply would reach crossterm as keystrokes). Before the input loop starts, whatever the configured setting, Kiki writes an OSC 11 query followed by DA1 to the terminal and reads the tty until the DA1 reply or 500 ms, whichever comes first (Pi waits 100 ms; Kiki allows more for slow links because the DA1 reply ends the wait early on responsive terminals). A parsed background with OKLCH lightness ≥ 0.5 means light; otherwise dark. With no background reply, `COLORFGBG`'s last field decides using Pi's rule: indices 0–6 and 8 are dark, 7 and 9–15 light, anything else ignored (`.research/pi/source/packages/coding-agent/src/modes/interactive/theme/theme.ts:608-621`); with neither, dark. The detected appearance is kept for the process. Detection is skipped when stdout is not a terminal.

**Pickers.** `/theme` opens a picker with `automatic (currently <dark|light>)`, `dark`, `light`, focused on the current setting. Focus changes apply the focused theme to the whole screen immediately; Enter or Ctrl-S applies and saves; Esc restores the theme active on opening. `/settings` opens a one-row settings list (`Theme  <setting>`); Enter on the row opens the same theme picker, and closing the theme picker returns to the list; Esc closes the list. Both commands appear in slash completion.

**Rendering.** `App` owns the resolved `&'static Theme` and passes it to the transcript layout, tree, pickers, prompt rule, slash completion and both highlighters. Settled transcript rasters record the theme they were drawn with and are rebuilt when it differs, as they are today for a width change.

## Seams & interfaces

Names are illustrative where marked; shapes and invariants are the contract.

**1. Theme values — new `src/theme.rs`.**
- `Theme`: a plain struct of named `ratatui::style::Color` fields, one per Pi token in the mapping table, plus `name: &'static str`. Two statics, `theme::DARK` and `theme::LIGHT`; no other instances exist, so `&'static Theme` identity (or `name`) is a valid cache key.
- `ThemeSetting { Dark, Light, Automatic }` with `parse(&str) -> Option<Self>` and `as_str()`; `Appearance { Dark, Light }`.
- `ThemeSetting::resolve(self, Appearance) -> &'static Theme` — total; `Automatic` follows the appearance.
- Each theme's markdown rendering options (Kiki's style sheet plus a `CodeTheme` generated as TextMate source from the theme's `syntax*` tokens) are built once per theme and borrowed per render, because `Options` owns its `CodeTheme` (`tui-markdown-0.3.9/src/options.rs:127-131`) and `from_str_with_options` borrows `&Options` (`renderer/mod.rs:72`). Generation from fixed inputs cannot fail at runtime; a unit test parses both.

```rust
let theme = settings.theme().resolve(appearance); // &'static Theme
Style::new().fg(theme.muted).bg(theme.tool_success_bg)
```

**2. Appearance detection — in `src/theme.rs`, behind a small terminal seam.**
- `detect_appearance(terminal: &mut impl TerminalQuery, colorfgbg: Option<&str>) -> Appearance` — never fails; any I/O error, timeout or unparseable reply falls through to `COLORFGBG`, then `Dark`.
- `TerminalQuery` (illustrative name): write bytes; read bytes until a deadline. Production implementation wraps the raw-mode tty before the input loop starts; tests supply scripted replies (OSC 11 in `rgb:` 4- and 2-digit forms, BEL or ST terminators, DA1 only, silence, garbage).
- Invariant: on return, every byte up to and including the DA1 reply has been consumed; bytes after the DA1 reply are not read. When DA1 never arrives within 500 ms the function returns without further reads. Detection is best effort: replies arriving after the deadline are not consumed and reach crossterm (an OSC 11 reply then appears once as stray prompt input; a late DA1 reply is swallowed by crossterm). Tests cover replies split across reads, a reply after the deadline, and DA1 without OSC 11.

```rust
let appearance = theme::detect_appearance(&mut tty, env::var("COLORFGBG").ok().as_deref()); // once, at startup
let theme = settings.theme().resolve(appearance);
```

**3. Settings — `src/config/settings.rs` (`SettingsContext`).**
- `theme() -> ThemeSetting` — resolved project-over-user value, `Automatic` when unset.
- `save_theme(ThemeSetting) -> Result<SaveOutcome, SaveError>` — same semantics as `save_model` (user file, comment-preserving single-key write, `SaveOutcome` reports masking by a project value).
- Validation: a non-string or unsupported `theme` is a `Problem` naming `dark`, `light`, `automatic`, handled like other invalid fields in the same layer.

**4. Rendering — `&Theme` parameter on render entry points.**
- Transcript layout, session tree, pickers, slash completion, prompt rule and the Python highlighter take `theme: &Theme` (or `&'static Theme` where stored). No renderer reads a global.
- Markdown: Kiki's `MarkdownStyleSheet` holds `&'static Theme` (satisfies `StyleSheet: Clone + Send + Sync + 'static`); `markdown(source, theme)` renders with that theme's prebuilt options.
- Transcript cache: a settled raster is reused only when both its width and its theme match the current layout; otherwise it is rebuilt, exactly as for a width change today.

**5. Commands and pickers — `src/tui/slash.rs`, `src/tui.rs`.**
- `slash::Action` gains `Theme` and `Settings`; catalogue entries `/settings` ("Open settings") and `/theme` ("Choose theme"). Neither takes an argument.
- `PickerValue` gains `Theme(ThemeSetting)` and `OpenThemePicker`; `/settings` is an ordinary `Picker` whose single row carries `OpenThemePicker`, so there is no new list component or settings-item type until a second item exists.
- App invariants: while the theme picker is open, the rendered theme is the focused item's resolved theme; Enter/Ctrl-S saves via `save_theme` and keeps it; Esc restores the theme active when the picker opened; a theme picker opened from `/settings` returns to the settings picker on Enter or Esc. A failed or masked save keeps the applied theme and shows the existing save notice.

## Risks / unknowns

- **Colour fidelity.** Kiki's RGB values must equal Pi's okhsl conversion (`.research/pi/source/packages/tui/src/oklab.ts`, used via `packages/tui/src/colors.ts`); values are generated from Pi's own code once and pinned by tests rather than re-derived by hand.
- **Fixed theme on a mismatched terminal.** `light` on a dark terminal (or the reverse) leaves unthemed text in the terminal's default colour against themed cards; this is accepted, and is why `automatic` is the default.
- **Detection edge cases.** Terminals or multiplexers that drop OSC 11 fall back to `COLORFGBG`/dark; a terminal that answers neither OSC 11 nor DA1 costs a 500 ms startup delay. An OSC 11 reply slower than 500 ms leaks once into the prompt as stray characters — accepted residual risk. Detection is skipped when not attached to a terminal.
- **Syntect cost.** Enabling `highlight-code` adds syntect 5 with default features (Oniguruma C build), its bundled syntax definitions, and `ansi-to-tui`: longer clean builds, a larger binary, and a one-time syntax-set load on the first highlighted fence. Live (streaming) assistant blocks are re-rendered every frame, so highlighting cost on large fenced blocks while streaming must be measured; settled blocks are cached.
- **Truecolour.** All themed colours are 24-bit, as today's palette already is; terminals without truecolour support keep today's degraded rendering.

## Dependencies

- Pi 1.2.0 themes, vendored at `.research/pi/source` commit `63035734f` (`packages/coding-agent/src/modes/interactive/theme/{dark,light}.json`), and Pi's okhsl conversion (`packages/tui/src/oklab.ts`).
- `tui-markdown` 0.3.9 `highlight-code` feature (syntect 5, `ansi-to-tui` 8); crossterm 0.29 via ratatui 0.30.
- Reverses the no-syntect decision in `docs/2026-08-27_live-openai-agent/spec.md:211,257`.

## Phase outline

1. **Pi themes and the `theme` setting** — `src/theme.rs` with both Pi 1.2.0 palettes; every renderer takes `&Theme`; markdown `md*` styling and syntect-highlighted fences; Arena Python highlighter, tree, prompt rule, slash completion and picker focus on theme tokens; `theme = "dark" | "light" | "automatic"` (default automatic) with startup detection. Demonstrable by setting `theme` in `config.toml` and launching in light and dark terminals.
2. **Theme picker and settings menu** — `/theme` with live preview, Enter/Ctrl-S save and Esc restore; `/settings` with a Theme row that opens the same picker; theme-keyed transcript re-rendering on change. Demonstrable interactively without editing files.
