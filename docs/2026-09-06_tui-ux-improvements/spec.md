# Spec: TUI UX improvements

## Context

Kiki’s prompt editor already stores and renders multiline text, but newline entry is gated behind Crossterm’s keyboard-enhancement capability probe. In Zed 1.18.1 with `TERM_PROGRAM=zed` and `TERM=xterm-256color`, the user reports that `Shift-Enter` works in Pi but not Kiki. The source establishes Kiki’s probe gate and the contrasting enable-first strategies in Maki and Codex; whether removing that gate fixes this Zed case remains a manual acceptance-test hypothesis.

The footer renders directory/session identity and model information directly against its rule characters, reducing readability. Slash commands execute only after exact submission, with no completion or visual distinction from ordinary prompts. Transcript scrolling stores distance from the bottom, so appended or streamed rows move a manually scrolled viewport instead of preserving what the user was reading. `Ctrl-End` already returns history to the bottom.

## Goals

- Make multiline prompt entry work with `Shift-Enter` in Zed where enhancement can be enabled, with `Ctrl-J` as a portable fallback; keep plain `Enter` submission.
- Add visible spacing between footer identity, rule, and model information without weakening narrow-terminal truncation.
- Show filtered slash-command suggestions automatically, support keyboard selection/completion, and visibly style an exact recognised command before execution.
- Keep a manually scrolled transcript at the same numeric document-top row during append-only updates; continue following only when already at the bottom.
- Retain `Ctrl-End` as the existing jump-to-bottom shortcut.

## Non-goals

- Implement Pi/Codex-style terminal probing, input replay, `modifyOtherKeys`, or a custom Crossterm parser.
- Add configurable keybindings or another jump-to-bottom shortcut.
- Autocomplete slash-command arguments, add commands, or change command behaviour.
- Treat unknown slash-prefixed text as a command; it remains an ordinary prompt.
- Build a general-purpose completion or viewport framework.
- Change transcript selection, prompt history, or footer content beyond the requested spacing.

## Key discoveries

- `PromptEditor` already supports newline insertion, multiline layout, caret movement, and byte limits; the missing behaviour is in terminal setup and key dispatch, not the editor model (`src/prompt_editor.rs:6-26`, `src/prompt_editor.rs:64-119`).
- Kiki skips the Kitty enable sequence when Crossterm’s capability query returns false, then separately requires the resulting flag before accepting `Shift-Enter` (`src/tui.rs:1007-1011`, `src/tui.rs:1296-1305`). Maki attempts the same enable sequence without probing and includes `Ctrl-J` (`.research/maki/source/maki-ui/src/terminal.rs:323-329`, `.research/maki/source/maki-ui/src/text_buffer.rs:5-13`). Codex also enables first and keeps newline bindings independent of its support hint (`.research/codex/source/codex-rs/tui/src/tui/keyboard_modes.rs:123-150`, `.research/codex/source/codex-rs/tui/src/keymap.rs:1437-1453`).
- Slash-command recognition currently lives in one private parser and covers only `/model`, `/name`, and `/thinking`; exact recognised text is checked only after `Enter` takes the prompt (`src/tui.rs:380-399`, `src/tui.rs:1070-1100`).
- Prompt rendering currently receives unstyled row strings, so recognised-token styling needs a narrow addition to the prompt render projection rather than changing stored prompt text (`src/prompt_editor.rs:36-39`, `src/prompt_editor.rs:88-118`, `src/tui.rs:1680-1695`).
- Footer layout already centralises identity, statistics, model truncation, and exact width accounting. Its rule renderer inserts no padding between identity/rule/model, although a non-rule test projection does (`src/transcript.rs:1475-1520`, `src/transcript.rs:1541-1626`).
- Transcript position is represented solely as rows from the bottom and clamped after each layout. `Ctrl-End` already sets that offset to zero before prompt editing (`src/tui.rs:150-184`, `src/tui.rs:915-929`, `src/tui.rs:1631-1655`).

## Proposed approach

### Multiline input and footer

Attempt Crossterm’s Kitty keyboard enhancement unconditionally during configured TUI startup and restore it through the existing terminal guard. Keep enhancement success local to terminal restoration rather than application input state. Accept reported `Shift-Enter` and portable `Ctrl-J` as newline edits; keep unmodified `Enter` as submission. Always show `Shift-Enter/Ctrl-J newline` in the normal input header.

Add one display-cell space between footer identity and its rule, and between the rule and the first right-aligned field. Include those spaces in fitting and truncation calculations so every footer width remains exact.

### Slash-command completion

Create one private slash-command catalogue containing each command’s token, purpose, and existing action. Use it as the source for parsing, autocomplete, and recognised-command styling so those behaviours cannot drift.

When the editable prompt consists of a command-name prefix at the caret, show matching commands in a compact overlay immediately above the prompt. The catalogue order and purpose text are `/model` “Choose model”, `/name` “Name session”, and `/thinking` “Choose thinking level”. The first match is initially selected, `Up`/`Down` clamp at the list bounds, and the selected row uses reverse video. The overlay does not resize history. It is keyboard-only: primary mouse events inside it do nothing and cannot select obscured history. Unmodified `Tab` or `Enter` completes the selected token; `Shift-Tab` retains thinking preview. A partial command completed with `Enter` requires a second `Enter` to execute.

Once a command token is exact, hide completion and render that token in cyan plus bold. Arguments remain normally styled. Unknown slash-prefixed text and multiline text remain ordinary prompts.

### Stable history position

Replace the single bottom-relative scroll offset with a private history position that is either:

- following the bottom; or
- anchored to an absolute document row.

Scrolling away from the newest page creates an anchor. Append-only transcript growth preserves that numeric top row. Scrolling back to the newest page or pressing the existing `Ctrl-End` returns to bottom-following. Reflow or shrinkage clamps an invalid anchor; if clamping reaches the newest page, the state becomes bottom-following for subsequent updates. No shortcut or hint is added.

Keep these states inside the TUI rather than introducing generic autocomplete or viewport modules.

## Key decisions & tradeoffs

- Follow Maki’s small Crossterm-compatible enhancement strategy rather than importing Pi/Codex terminal negotiation and parsing machinery.
- Keep `Ctrl-J` as the reliable fallback and advertise both newline keys; an old terminal may still collapse `Shift-Enter` to ordinary submission.
- Complete a partial command on the first `Enter` and require a second `Enter` to execute, favouring safety over one-keystroke execution.
- Overlay completion above the prompt instead of reserving rows, preventing autocomplete from moving history.
- Style only an exact command token, not its arguments, so executable recognition remains visually precise.
- Represent manual history position from the document top rather than compensating a bottom-relative offset after every update.
- Keep one phase and one PR, with atomic commits for the independently testable behaviours.

## Risks / unknowns

- Writing the Kitty enable sequence can report success even when a terminal ignores it. `Ctrl-J` remains functional, but unsupported terminals cannot distinguish `Shift-Enter` from submission.
- Prompt styling must retain source ranges across wrapping so exact commands remain styled and the cursor remains correct on very narrow terminals.
- The completion overlay must clip safely when little or no history area exists and must not interfere with the existing full-screen settings picker.
- An absolute row is stable for append-only growth, not durable content identity. Streamed Markdown can reflow the growing message, while width changes, visibility toggles, and authoritative replacement can reflow the whole document; an anchor inside affected content may therefore show different text at the same numeric row before clamping.

## Dependencies

- Ratatui `0.30.2` and its Crossterm re-export; no new crate is required (`Cargo.toml:13`).
- Existing prompt editor and transcript layout modules (`src/prompt_editor.rs`, `src/transcript.rs`).
- Existing live-agent input and scrolling requirements, superseded where this spec explicitly changes newline capability gating and history following (`docs/2026-08-27_live-openai-agent/spec.md:192`).
- Existing history selection behaviour must remain intact (`docs/2026-09-02_history-text-selection/spec.md:50-148`).
- Local reference snapshots of Maki `e0d87e35` and Codex `2b7c2797` under `.research/`.

## Plan

### Acceptance criteria

- In the reported Zed 1.18.1 environment (`TERM_PROGRAM=zed`, `TERM=xterm-256color`), manual validation confirms whether attempting keyboard enhancement without the probe gate makes `Shift-Enter` insert a newline without submission. `Ctrl-J` inserts a newline independently of terminal enhancement support; plain `Enter` retains existing submission and slash-command execution behaviour.
- The normal editable header advertises `Shift-Enter/Ctrl-J newline`; configuration-only and input-disabled headers do not.
- Footer identity and the right-aligned field group each have one space separating them from the rule whenever present. Footer output remains exactly one terminal row wide and retains its current truncation priorities at every width.
- Typing `/` or a partial command token shows `/model` “Choose model”, `/name` “Name session”, and `/thinking` “Choose thinking level” matches in that catalogue order directly above the prompt without changing history dimensions. The first match is initially selected with reverse video. Primary mouse input inside the overlay is inert and never selects or copies obscured history; history outside it remains selectable.
- `Up`/`Down` selects a suggestion and clamps at either bound. Unmodified `Tab` or `Enter` replaces a partial token with the selected complete token; completion does not execute, and `Shift-Tab` retains thinking preview. Exact recognised command tokens are cyan plus bold, and their arguments are not.
- Exact commands retain existing behaviour. Unknown, no-match, argument-position, and multiline slash-prefixed text remain normal editable prompts and do not show completion.
- Append-only transcript rows preserve the numeric top document row while history is manually scrolled. Streaming reflow may change the text occupying an anchor inside the growing message. When following the bottom, new rows remain visible. Page/mouse scrolling to the newest page, clamping an invalid anchor onto the newest page, and `Ctrl-End` resume bottom-following.
- Existing prompt editing, settings pickers, history selection/copying, reasoning/arena toggles, configuration-only scrolling, tiny-terminal rendering, and terminal restoration continue to work.

### Overall validation

Run from the implementation worktree:

```sh
cargo fmt --all -- --check
cargo check --all-targets
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
git diff --check main...HEAD
```

All commands must pass. Then run Kiki in Zed 1.18.1 with `TERM_PROGRAM=zed`, `TERM=xterm-256color`, and a configured account. Verify whether `Shift-Enter` works after the enable-first change, plus `Ctrl-J`, slash completion/highlighting, numeric-row stability while reading older history during append-only streaming, `Ctrl-End`, and footer spacing at normal and narrow widths. If Zed still collapses `Shift-Enter` to submission, the acceptance criterion fails and requires a newly agreed terminal strategy rather than silently widening this plan. The credential-gated live OpenAI test remains opt-in.

### Planning context

- Configured input event precedence is picker, global controls, history navigation, disabled-input guard, thinking preview, cancellation, submission/slash dispatch, then prompt editing (`src/tui.rs:318-408`). Completion handling must fit before submission without weakening global cancellation or the full-screen settings picker.
- `PromptEditor` owns text, caret, wrapping, and visible-row projection. Completion applies only when the whole prompt is a command-name prefix, so the TUI can reuse `take` and `insert_paste`; the editor needs only narrow read/caret eligibility and source-row information for styling. Command policy remains in the TUI (`src/prompt_editor.rs:28-39`, `src/prompt_editor.rs:47-135`).
- Transcript layout already reports total wrapped height and renders from a document-top row, making top anchoring possible without changing transcript records or render blocks (`src/transcript.rs:1815-1825`, `src/tui.rs:1631-1669`).
- Keyboard, mouse wheel, drag-edge scrolling, prompt growth, authoritative transcript replacement, and temporarily unavailable history all share the current scroll fields and therefore must migrate together (`src/tui.rs:241-300`, `src/tui.rs:800-929`, `src/tui.rs:1031-1067`).
- Footer width calculations are centralised in `FooterLayout`, `footer_fits`, and `right_width`; spacing must be represented there rather than patched into the final rendered string (`src/transcript.rs:1469-1626`).

### Task 1: Make multiline input portable

**Changes** — Attempt keyboard enhancement without a capability-query gate, retain fail-soft startup and ordered restoration, and remove application-level capability state that no longer controls input or hints. Accept `Shift-Enter` or `Ctrl-J` as newline edits while preserving plain `Enter` submission. Make the normal editable header advertise both bindings and update the user-facing control documentation.

**Tests** (`src/tui.rs`, app input and terminal lifecycle) — First fail on `Ctrl-J` inserting between two prompt fragments without emitting a runtime command. Extend existing input, header, and restoration tests for `Ctrl-J`, unconditional `Shift-Enter`, the fixed header, enhancement push failure, and pop-before-terminal-restore ordering rather than duplicating their existing submission, mode, active-run, and release-event matrices.

**Files** — Modify: `src/tui.rs`, `README.md`.

**Verification** — `cargo fmt --all -- --check && cargo test tui::tests::shift_enter -- --test-threads=1 && cargo test tui::tests::control_j -- --test-threads=1 && cargo test tui::tests::keyboard_enhancement -- --test-threads=1 && cargo check --all-targets` passes.

**Commit** — `Make multiline prompt input portable`

### Task 2: Space footer fields from the rule

**Changes** — Add visible spacing on both identity/rule and rule/right-field seams, and account for it in fit, truncation, and fallback decisions without changing field contents or priority.

**Tests** (`src/transcript.rs`, footer layout) — First fail on the full-width footer containing `identity ─` and `─ model`. At the owning footer seam, cover both-sided and each one-sided spacing, exact display width at normal and tiny widths, and retained truncation priority; update existing rendered expectations where required.

**Files** — Modify: `src/transcript.rs`; update changed black-box expectations in `src/tui.rs` without changing TUI production logic.

**Verification** — `cargo fmt --all -- --check && cargo test transcript::tests::footer -- --test-threads=1 && cargo check --all-targets` passes.

**Commit** — `Space footer fields from the rule`

### Task 3: Add slash-command completion

**Changes** — Centralise the three existing commands and their approved purpose text in catalogue order `/model`, `/name`, `/thinking`, used for exact parsing, prefix matches, and recognised-token ranges. Add purpose-built completion selection state, expose only prompt read/caret eligibility and source-row projection needed for the feature, and reuse the editor’s existing whole-draft `take`/`insert_paste` operations to complete. Initially select the first match, clamp `Up`/`Down`, use reverse video for selection, and route unmodified `Tab` and `Enter` completion before ordinary submission while preserving `Shift-Tab`. Render the clipped overlay above the prompt, retain its rendered area for hit-testing so primary mouse input cannot reach obscured history, style exact command tokens cyan plus bold, and update README controls with suggestions, selection, first-Enter/Tab completion, and second-Enter execution.

**Tests** (`src/tui/slash.rs`, command matching; `src/prompt_editor.rs`, source-aware visible rows; `src/tui.rs`, input/render integration) — First fail on `/mo` producing `/model` after one `Enter` with no runtime command, followed by command execution on the second `Enter`. Test matching and selection policy at the slash seam, source-aware wrapping at the editor seam, and the approved completion/rendering behaviour once through the App surface. Retain focused edge coverage for no matches, unknown and multiline prompts, tiny overlay clipping, existing picker precedence, and release events. Prove that clicking or dragging inside the overlay cannot create or copy a history selection while selection outside it still works.

**Files** — Create: `src/tui/slash.rs`. Modify: `src/tui.rs`, `src/prompt_editor.rs`, `README.md`.

**Verification** — `cargo fmt --all -- --check && cargo test tui::slash::tests -- --test-threads=1 && cargo test prompt_editor::tests -- --test-threads=1 && cargo test tui::tests::slash -- --test-threads=1 && cargo test tui::tests::prompt -- --test-threads=1 && cargo check --all-targets` passes.

**Commit** — `Add slash command completion`

### Task 4: Anchor manually scrolled history

**Changes** — Replace bottom-relative-only scroll state with explicit bottom-following or numeric-top-row-anchored state. Migrate keyboard, mouse, drag, render, prompt-height, disabled/configuration-only, and selection interactions to that state. Preserve the numeric anchor on append-only growth, clamp it after reflow or shrinkage, and change a clamped anchor to bottom-following when it reaches the newest page. Resume following through incremental scrolling or existing `Ctrl-End`. Update README controls to describe manual anchoring and `Ctrl-End` resumption.

**Tests** (`src/tui.rs`, app and `TestBackend` integration) — First fail on a manually scrolled viewport retaining the same numeric top row and top-row text while append-only rows increase total history. Migrate the existing keyboard, mouse, drag, geometry, replacement, mode, and selection regression tests to the new state. Add only the novel transitions: anchored append stability, following append visibility, streamed Markdown reflow at an anchor inside the growing message, and reflow/shrink clamping onto the newest page followed by an append that proves bottom-following resumed.

**Files** — Modify: `src/tui.rs`, `README.md`.

**Verification** — `cargo fmt --all -- --check && cargo test tui::tests::transcript_ -- --test-threads=1 && cargo test tui::tests::history_ -- --test-threads=1 && cargo test tui::tests::mouse_wheel -- --test-threads=1 && cargo test tui::tests::edge_drag -- --test-threads=1 && cargo test tui::tests::prompt_growth -- --test-threads=1 && cargo check --all-targets` passes; then run Overall validation.

**Commit** — `Anchor manually scrolled history`
