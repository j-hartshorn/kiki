# Spec: Display-history text selection

## Context

Kiki captures mouse input to support display-history scrolling, which prevents ordinary terminal text selection. Left-button press, drag, and release events are currently ignored.

Display history is rendered as styled, wrapped cards. Copying terminal cells directly would include card decoration and terminal-width line breaks. Kiki therefore needs application-managed selection that follows the visible content while reconstructing its logical rendered text before writing it to the local system clipboard.

This change supersedes the earlier display-history spec's exclusion of mouse selection, without changing the underlying session or model context.

## Goals

- Select display-history text by dragging the primary mouse button forwards or backwards across visible cards.
- Highlight only selectable content and auto-scroll the display history when a drag rests at its top or bottom edge.
- Copy automatically on release to the local system clipboard, preserving logical line breaks while removing terminal soft wraps, card borders, padding, colours, and hidden content.
- Give non-blocking success and failure feedback while retaining a completed selection until it becomes stale or a new selection begins.
- Preserve current keyboard controls, mouse-wheel scrolling, display projection, and demand-driven redraw behaviour.

## Non-goals

- Recovering raw Markdown syntax or other pre-render source from a partial visual selection.
- OSC 52, SSH/tmux clipboard forwarding, clipboard reads, rich-text clipboard formats, or clipboard configuration.
- Prompt-editor selection, keyboard-driven history selection, double-click word selection, triple-click line selection, right-click actions, or a context menu.
- Copying collapsed, omitted, hidden, or otherwise undisplayed content.
- Persisting selections across display-history content changes, expansion toggles, prompt-driven viewport geometry changes, terminal resize, or process restart.
- Changing session records, model context, display-history content, Markdown styling, card layout, or collapse policy.

## Key discoveries

- Kiki enables terminal mouse capture for both configured and configuration-only modes, and restores it on every exit path (`src/tui.rs:370`, `src/tui.rs:399`, `src/tui.rs:416`).
- Mouse handling currently consumes only three-row wheel scrolling; button presses, drags, and releases are discarded (`src/tui.rs:118`, `src/tui.rs:322`).
- The display-history viewport starts below the header and derives a virtual top row from total layout height, viewport height, and `scroll_from_bottom` (`src/tui.rs:642`, `src/tui.rs:677`). This is the existing screen-to-document coordinate seam.
- `TranscriptLayout` already owns block measurement, clipping, and rendering behind a small private interface (`src/transcript.rs:901`, `src/transcript.rs:1010`, `src/transcript.rs:1238`). Cards and arena sections retain their pre-wrap Ratatui `Text`, so selection can reconstruct logical rendered text without changing Markdown rendering (`src/transcript.rs:482`, `src/transcript.rs:532`, `src/transcript.rs:950`).
- Ratatui's enabled rendered-line feature supplies height measurement, while current rendering uses the same `Paragraph` and `Wrap { trim: false }` path for measurement and drawing (`Cargo.toml:10`, `src/transcript.rs:482`, `src/transcript.rs:950`). Selection extraction must reuse that rendering path rather than duplicate its wrapping rules.
- Existing tests can inject mouse events through `FakeInput`, inspect `TestBackend` cells, and verify redraw counts and mouse-capture restoration (`src/tui.rs:919`, `src/tui.rs:942`, `src/tui.rs:1748`, `src/tui.rs:2106`). Display-history tests already cover wrapped-row scrolling, CJK/emoji measurement, and vertical clipping (`src/tui.rs:1974`, `src/transcript.rs:3700`, `src/transcript.rs:3719`).
- The previous live-agent spec intentionally limited mouse interaction to wheel scrolling; this spec explicitly supersedes that non-goal for display-history text selection only (`docs/2026-08-27_live-openai-agent/spec.md:27`, `docs/2026-08-27_live-openai-agent/spec.md:37`).

## Proposed approach

Add a private selection module whose interface accepts document-space anchor and focus positions, projects the selected range into the current viewport, highlights its intersections with selectable content regions, and extracts logical text from rendered cells. It owns terminal-column, Unicode-width, forwards/backwards range, clipping, and soft-wrap reconstruction rules so those details do not spread through the TUI or display projection.

Extend display-history layout blocks with private selectable-region metadata for content that they already render. Regions identify content rectangles and real logical-line starts. `TranscriptLayout` remains the display-projection seam: it renders the existing cards unchanged, applies a selection only to content regions, and extracts selected text block by block through the same Ratatui paragraph rendering path. Extraction joins soft-wrapped rows without a newline, preserves explicit logical line breaks, separates touched cards with one blank line, and never allocates a terminal buffer for the complete display history.

Store the active selection in virtual display-history row and terminal-column coordinates captured from the last rendered viewport. Primary-button drag updates the focus in document space. Resting at the top or bottom viewport row scrolls one row per existing 50 ms event-loop cadence and extends the focus without blocking input. Mouse release marks a non-empty selection for copy; the next render has the valid layout and cell data needed to extract it.

Use a narrow clipboard outbound port with a persistent `arboard` text-only adapter created at TUI startup and a fake adapter in tests. Clipboard initialisation and writes are fallible, synchronous local desktop operations. A failure remains nonfatal and does not clear the prior clipboard.

After a successful write, retain the selection, show `Copied` in the header, and toggle the completed highlight off and on twice at 100 ms intervals before leaving it highlighted. On failure, retain the ordinary highlight without flashing and show a generic `Copy failed` header notice. Notices clear on the next input or visible display-history change. A new primary-button press replaces the selection. Mouse-wheel or keyboard scrolling preserves a completed selection; any content or geometry change clears it before stale document coordinates are rendered.

The behaviour applies in configured, active-run, runtime-disabled, and configuration-only views. Existing wheel scrolling remains unchanged when no primary-button drag is active.

## Key decisions & tradeoffs

- Copy logical rendered text, not raw Markdown. Exact partial source recovery would require replacing `tui-markdown` or maintaining a second Markdown renderer; avoiding artificial soft-wrap breaks is the higher-value requirement.
- Keep `tui-markdown` and Ratatui as the only presentation authorities. A custom selectable paragraph renderer was rejected because it would duplicate unstable wrapping behaviour.
- Manage selection inside Kiki rather than disabling mouse capture. Native terminal selection cannot provide the agreed auto-scroll, decoration filtering, feedback, or reliable soft-wrap reconstruction.
- Allow one drag to span cards. Each touched card contributes its visible logical content in display order, with one blank line between cards.
- Use local native clipboard writes only. OSC 52 and remote forwarding are deferred until an actual remote requirement exists.
- Store document rows and terminal columns rather than screen coordinates so ordinary scrolling and edge auto-scroll do not move the selected range.
- Clear stale selections instead of inventing stable source identities across live projection replacement or reflow.
- Keep clipboard failure nonfatal and generic; execution records and display history must not be polluted by a local presentation-side effect.

## Risks / unknowns

- Ratatui word wrapping may consume or move whitespace at a soft break. Extraction needs focused tests that distinguish word-boundary wraps from forced mid-word wraps and preserve the logical text.
- Wide graphemes occupy multiple terminal cells and leave continuation cells. Selection must snap extraction to complete graphemes without adding spaces or splitting UTF-8.
- Streaming updates can invalidate virtual rows during interaction. The selection must be cleared atomically with any visible display-history change before rendering.
- Long off-screen selections could cause excessive temporary allocation if extraction rendered the whole history. Block-local, vertically chunked extraction avoids that shape; tests must cover off-screen cross-card selection.
- Native clipboard initialisation can fail on headless systems and writes can fail when the desktop clipboard is unavailable or occupied. Both paths need deterministic fake-adapter tests and generic user-facing feedback.
- Animation and edge scrolling add timer-driven redraws. They must stop exactly at completion, release, cancellation, or scroll bounds so idle rendering remains demand-driven.

## Dependencies

- Existing display projection and TUI: `src/transcript.rs`, `src/tui.rs`.
- Existing historical specification, superseded only where stated: `docs/2026-08-27_live-openai-agent/spec.md`.
- Ratatui 0.30.2 rendered-line measurement and Crossterm mouse events: <https://docs.rs/ratatui/0.30.0/>.
- `arboard` 3.6.1 text clipboard adapter with image features disabled: <https://docs.rs/arboard/3.6.1/arboard/>.

## Plan

### Acceptance criteria

- A primary-button drag beginning inside the display-history viewport highlights selectable content in either direction and never highlights card borders, padding, prompt text, header text, or footer text.
- Dragging at the top or bottom display-history edge scrolls one row per 50 ms cadence, extends the range into off-screen history, stops at history bounds, and stops immediately on release or when the drag leaves the edge.
- Releasing a non-empty selection copies once to the local native clipboard. An empty click does not write. Other mouse buttons do not start or copy a selection.
- Copied text matches the selected rendered content, removes terminal-width soft breaks, preserves explicit logical line breaks, handles word-boundary and forced mid-word wrapping, and never splits or duplicates CJK, emoji, or combining graphemes.
- Cross-card selection preserves display order and inserts one blank line between touched cards. Borders, padding, colours, collapsed/hidden content, and terminal fill spaces are absent.
- A successful write shows `Copied`, flashes the selection off/on twice at 100 ms intervals without blocking the event loop, then leaves it highlighted. A failed write shows `Copy failed`, does not flash, remains nonfatal, and leaves the selection highlighted.
- The header notice clears on the next input or visible display-history change. A completed selection survives ordinary history scrolling, but a new selection, display-history content change, expansion toggle, prompt-driven viewport change, or terminal resize clears stale selection state.
- Selection and copy work while idle, during an active run, when runtime input is disabled, and in configuration-only mode. Existing prompt editing, cancellation, keyboard history controls, wheel scrolling, mouse-capture restoration, and dirty-only redraw behaviour remain unchanged.
- Clipboard tests do not require a desktop session, and production keeps the native clipboard adapter alive for the TUI lifetime.

### Overall validation

Run from the feature worktree:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo test selection::tests
cargo test transcript::tests
cargo test tui::tests
cargo test clipboard::tests
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
git diff --check main...HEAD
```

All commands must pass. The live OpenAI test remains ignored without explicit credentials.

### Planning context

- `App` currently owns display-history scroll measurements but not the rendered viewport rectangle or virtual top row (`src/tui.rs:61`, `src/tui.rs:677`). Mouse hit testing must retain that last-rendered geometry without moving display-projection policy into the TUI.
- Both configured and configuration-only loops share `App::handle_mouse` but have separate loop functions (`src/tui.rs:466`, `src/tui.rs:500`). The clipboard port and timer tick must be exercised in both paths without duplicating behaviour.
- Dynamic display text is terminal-normalised before layout (`src/transcript.rs:2073`). Selection therefore copies the safe rendered form, not unnormalised control bytes.
- `TranscriptLayout` is rebuilt at render width and already contains every block's measured height (`src/transcript.rs:1010`, `src/transcript.rs:1238`). Selection metadata belongs in that ephemeral layout, not durable `Transcript` state.
- `arboard::Clipboard` must remain alive for the TUI lifetime because Linux/X11 clipboard ownership may be process-backed. Its interface remains behind a one-method outbound port so tests never initialise a real desktop clipboard.

### Task 1: Add the display-history selection module

**Changes** — Add a private module for document-space anchor/focus ranges, viewport projection, selectable content regions, rendered-cell highlighting, and logical-text extraction. Keep Unicode cell-width handling, continuation-cell handling, forwards/backwards normalisation, clipping, soft-wrap classification, and hard-line reconstruction behind this module's interface. Do not add mouse, timer, or clipboard effects.

**Tests** (`src/selection.rs`, module behaviour) — First fail on forwards and backwards selections projected through a scrolled viewport. Cover clipping above/below the viewport, empty ranges, partial first/last rows, explicit versus soft line breaks, word-boundary versus forced mid-word wraps, trailing terminal fill, disjoint regions, blank-line card separation, and complete CJK/emoji/combining graphemes when endpoints land on continuation cells.

**Files** — Create: `src/selection.rs`. Modify: `src/main.rs`.

**Verification** — `cargo test selection::tests` passes; `cargo check --all-targets` passes.

### Task 2: Make display-history layout selectable

**Changes** — Attach selectable-region and logical-line metadata to existing card and arena layout blocks without changing their rendered content or dimensions. Extend the private `TranscriptLayout` interface to highlight a document-space selection and extract its selected logical text using block-local, vertically chunked temporary buffers. Include visible Markdown output, user text, startup/status text, arena headings, labels, code, tool calls, execution content, and accounting; exclude decorative frames, padding, and undisplayed content.

**Tests** (`src/transcript.rs`, display-projection behaviour) — First fail on a narrow wrapped Markdown card whose copied text has no soft newline. Cover hard newlines, generated Markdown presentation such as links/lists/tables, card padding, nested arena frames and labels, collapsed and hidden content, cross-card ordering and separators, backwards and off-screen ranges, clipping, tiny widths, long chunked extraction, and wide/combining graphemes. Assert existing rendered buffers and heights remain unchanged when no selection is present.

**Files** — Modify: `src/selection.rs`, `src/transcript.rs`.

**Verification** — `cargo test transcript::tests` passes; `cargo test selection::tests` remains green; `cargo check --all-targets` passes.

### Task 3: Wire mouse selection to the native clipboard

**Changes** — Add a persistent text-only `arboard` adapter behind a one-method clipboard outbound port and a fake adapter for tests. Retain the last rendered display-history viewport/top in `App`; add primary-button selection lifecycle, pending extraction/copy, one-row edge scrolling on the 50 ms loop cadence, invalidation on content or geometry changes, retained completion highlighting, two success flashes, and transient header notices. Share the behaviour across configured and configuration-only loops. Preserve existing wheel and keyboard handling.

**Tests** (`src/clipboard.rs`, adapter policy; `src/tui.rs`, input/render integration) — First fail on primary down/drag/up producing one fake clipboard write with unwrapped logical text. Cover empty and non-primary clicks, backwards selection, cross-card/off-screen drag, edge-scroll cadence/direction/bounds/release, active-run and read-only modes, selection-only content highlighting, success flash transitions and final retained highlight, generic nonfatal failure feedback, notice clearing, stale selection invalidation, viewport resize/prompt growth, mouse-wheel compatibility, tiny terminals, redraw counts, and error/quit cleanup. Real desktop clipboard access is excluded from tests.

**Files** — Create: `src/clipboard.rs`. Modify: `Cargo.toml`, `Cargo.lock`, `src/main.rs`, `src/tui.rs`.

**Verification** — `cargo test clipboard::tests` passes; `cargo test tui::tests` passes; `cargo test selection::tests` and `cargo test transcript::tests` remain green; `cargo check --all-targets` passes.

### Task 4: Document and verify display-history selection

**Changes** — Document primary-button drag selection, automatic local clipboard copy, logical-line behaviour, edge auto-scroll, success flash/header feedback, failure feedback, selection invalidation, and the local-desktop-only clipboard constraint. Remove the statement that all non-wheel mouse input is ignored.

**Tests** — No separate documentation test applies. The behavioural coverage is owned by Tasks 1–3.

**Files** — Modify: `README.md`.

**Verification** — Run every command in Overall validation; expect all checks and non-live tests to pass with only the credential-gated live test ignored.
