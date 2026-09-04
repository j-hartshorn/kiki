# Spec: TUI responsiveness

## Context

Kiki already permits editing a new prompt while a Run is active, but the interaction becomes sluggish as display history grows or live progress arrives. Every redraw reconstructs the complete transcript layout, including settled Markdown parsing, Python highlighting and wrapped-height measurement. Prompt-only edits and viewport scrolling therefore pay for unchanged history.

Paste has a separate amplification problem. Kiki does not enable bracketed-paste mode and currently discards `Event::Paste` payloads. In terminals that deliver pasted content as ordinary key events, each character is applied and redrawn separately.

This change makes prompt editing, bulk paste and display-history scrolling responsive without replacing Kiki’s synchronous, demand-driven terminal loop. It adds one bulk paste path and caches settled transcript render blocks while keeping live content mutable.

## Goals

- Insert bracketed paste into the prompt as one bulk edit while idle or during an active Run, without submitting it.
- Raise prompt capacity to 64 KiB; when necessary, insert the longest complete UTF-8 prefix that fits.
- Normalise pasted line endings, expand tabs to four spaces, and render unsupported controls as visible `\xNN` text.
- Reuse settled transcript parsing, highlighting, wrapping and height measurement across prompt edits, scrolling and live progress; rebuild only changed live content.
- Target 8.3 ms key and scroll frames, tolerate up to 16.7 ms, and keep a maximum-size paste below 50 ms in the opt-in release benchmark.
- Preserve current transcript appearance, selection extraction, active-draft behaviour and demand-driven redraw semantics.

## Non-goals

- Replacing the synchronous TUI loop with Codex’s asynchronous event broker, frame scheduler or terminal architecture.
- Adding an input-reader thread, event-drain fairness budget, frame-rate limiter or background render worker.
- Detecting unbracketed paste through timing heuristics on terminals that do not support bracketed paste.
- Incrementally parsing the mutable live Markdown tail; this slice only prevents unchanged settled history from rebuilding.
- Changing scroll anchoring when new output arrives, history navigation controls, or selection behaviour.
- Queuing prompts or changing Enter’s no-op behaviour while a Run is active.
- Guaranteeing terminal-emulator or transport latency; benchmarks cover Kiki processing with Ratatui’s test backend.
- Changing existing rendering behaviour for a single transcript block taller than Ratatui’s `u16` rectangle limit.

## Key discoveries

- Active Runs do not disable prompt editing; only plain Enter submission requires the transcript to be idle (`src/tui.rs:187-304`).
- Both TUI loops discard `Event::Paste` content, and terminal setup enables mouse and keyboard modes but not bracketed paste (`src/tui.rs:654-742`, `src/tui.rs:794`, `src/tui.rs:842`).
- `PromptEditor` currently accepts one character per edit and enforces the inherited 16 KiB byte limit at that insertion seam (`src/prompt_editor.rs:4`, `src/prompt_editor.rs:64-129`).
- Every history render requests a newly owned `TranscriptLayout`; constructing it projects every retained record, normalises content, parses Markdown, highlights Python and measures wrapping (`src/tui.rs:1000-1038`, `src/transcript.rs:1672-1783`).
- Settled records and the current live step are already distinct in the display projection, providing a natural seam for stable and mutable render caches (`src/transcript.rs:1764-1783`, `src/transcript.rs:2108-2165`).
- Selection highlighting and extraction operate on `TranscriptLayout` after construction, but their current region-building path repeats wrapped-line measurement; a reusable settled raster must therefore retain selection row-break metadata as well as cells and height (`src/transcript.rs:499-541`, `src/transcript.rs:1351-1480`, `src/selection.rs:153-202`).
- Existing fake input, runtime channels and Ratatui `TestBackend` support deterministic assertions about paste mutation, redraws and cache reuse without a real terminal (`src/tui.rs:1190-1290`, `src/tui.rs:3167-3210`).
- Codex confirms the narrow bracketed-paste lifecycle and width-keyed render-cache patterns, but its composer and transcript implementation use different state models (`.research/codex/source/codex-rs/tui/src/tui.rs:230`, `.research/codex/source/codex-rs/tui/src/tui/event_stream.rs:273`, `.research/codex/source/codex-rs/tui/src/pager_overlay.rs:404-435`).

## Proposed approach

### Bulk paste

After terminal initialisation succeeds for the configured TUI, extend its existing terminal-mode lifecycle to enable bracketed paste before mouse and keyboard modes, then disable those modes in reverse order on every exit path. Keep the terminal and mutable UI state on the current thread. If bracketed-paste enablement fails, attempt bracketed-paste cleanup and return a terminal error without entering later modes or the loop; if terminal initialisation itself fails, no bracketed-paste command has run. The separate read-only configuration TUI does not enable bracketed paste.

Route each `Event::Paste` payload through one prompt operation rather than synthesising key events. Convert CRLF and lone CR to LF, then reuse the existing terminal-text normaliser to expand tabs to four spaces and convert remaining C0/C1 and delete controls to uppercase `\xNN`. Insert the longest complete UTF-8 prefix that fits within the 64 KiB prompt capacity.

Insert at the current caret and redraw once. Pasting remains available while a Run is active, but remains ignored in configuration-only or runtime-disabled views. It never submits or queues the draft. Empty or wholly uninsertable normalised payloads are visible no-ops apart from clearing existing transient copy feedback consistently with other input.

### Settled transcript caching

Normalise each immutable startup or settled-record block once when it enters the display projection. Retain that source plus one width-specific rendered raster containing final styled cells, total height and block-local selectable-region geometry with row-break metadata.

Build the raster through the existing Markdown, Python-highlighting and Ratatui rendering path on a miss. A cacheable raster is one block-local `Buffer`, so only blocks whose measured height fits Ratatui’s `u16` rectangle height use this path. A taller block retains the existing uncached renderer and behaviour; fixing or caching that pathological case is outside this change.

Normal frames copy only the visible rows of cacheable settled blocks into the destination buffer; they do not invoke `Paragraph`, Markdown parsing, Python highlighting, wrapping or height measurement for those blocks. Cache only one current-width raster per block. Width changes discard and rebuild every raster from its normalised source; reasoning visibility rebuilds only reasoning rasters; arena expansion rebuilds only arena rasters.

Keep the current live step separate and transient. A live progress event rebuilds only its mutable render blocks and cheap combined metadata such as total height and footer; unchanged settled rasters are reused. When an immutable record is appended, normalise and cache only its new blocks while retaining earlier entries. Live-to-settled replacement removes the transient live suffix and appends the resulting immutable blocks; there is no general settled-record mutation mechanism.

The composed layout holds shared handles to settled rasters rather than cloning complete render trees. Scroll offsets and selections remain outside cache identity. Selection rendering and extraction translate cached block-local row-break metadata into current document coordinates and copy only required raster rows, preserving existing output and invalidation rules. A test-only cache bypass rebuilds the same state freshly so cached pixels, heights, footer, highlighting and extraction can be compared with the canonical path.

### Regression verification

Add deterministic default tests proving:

- one successfully inserted paste payload causes one bulk mutation and one redraw, while a no-op paste redraws only when clearing feedback changes visible state;
- normalisation, caret insertion, UTF-8 truncation and the 64 KiB limit;
- active-Run paste without submission;
- settled blocks are reused across prompt edits, scrolling and live progress;
- live, width and display-setting changes invalidate only the required entries;
- rendered output, heights and selection extraction remain unchanged.

Add an ignored release-mode benchmark using one persistent 120×40 Ratatui `TestBackend`, terminal and configured app. Build settled runs 1 through 128 by applying, in order, `RunStarted`, a user record, a completed-execution record, a final-message record and `RunSettled`. For run `NNN`:

- user text is exactly `benchmark request NNN`;
- step 1 source contains 40 lines generated as `value_NNN_LL = LL\n` for `LL` from `00` through `39`;
- step 1 reasoning is exactly `settled reasoning NNN`, stdout is `authoritative stdout\n`, result is `42`, and stderr, tool calls and accounting are empty;
- step 2 reasoning and accounting are empty;
- step 2 assistant Markdown is exactly 8,192 ASCII bytes, formed by repeating complete ``- stable **markdown** [link](https://example.test) `inline`\n`` lines while they fit and padding the remainder with `x`.

The 128 assistant messages total exactly 1,048,576 source bytes. Start run 129 and model step 1, then apply the initial assistant chunk `# live assistant\n\n`; do not settle it. Every later live delta is generated as `live tick NNNN: 1234567890\n`, which is exactly 27 ASCII bytes for the sequence numbers used below.

Draw once to populate caches. Then run these eight unmeasured warm-up frames in exact order: character `w`, PageUp, character `w`, PageDown, character `w`, PageUp, character `w`, PageDown. All key events have no modifiers. Each frame applies its input, applies live delta 0001 through 0008 respectively, then draws; the sequence ends at the newest viewport.

Measure 50 key frames for `i` in `0..50`. Sample `i` uses the unmodified ASCII character `a + (i mod 26)`, the four-digit zero-padded live delta sequence `0009 + i` (`0009..=0058`), and one draw, in that order. Then measure 50 scroll frames from the newest viewport for `i` in `0..50`. Sample `i` uses PageUp when `i` is even and PageDown when it is odd, live delta sequence `0059 + i` (`0059..=0108`), and one draw, in that order. Time each sample from immediately before input handling until `draw_frame` returns, retaining every duration without outlier removal. Nearest-rank p95 sorts the 50 durations and selects the 48th one-based value, `sorted[47]`. Report the 8.3 ms target and fail only above the inclusive 16.7 ms tolerance.

For paste, clear the draft and construct the raw payload before timing by repeating `\t12345\r\n` 6,553 times and appending `abcdef`; normalisation yields exactly 65,536 bytes. Time the production paste handler, live delta 0109 and one draw, in that order, and fail only above the inclusive 50 ms limit. Fixture construction, terminal construction, event and payload construction, warm-up, sorting and printing remain outside timings. The ignored test refuses to measure a debug build. Deterministic ordinary tests cover nearest-rank calculation and inclusive thresholds; benchmark setup asserts the exact settled and paste fixture sizes before timing. Terminal-emulator, polling, channels and clipboard latency are explicitly excluded.

No Codex source is copied. Its bracketed-paste and cache patterns are design inputs; the implementation remains native to Kiki’s retained transcript model.

## Key decisions & tradeoffs

- Keep one unphased specification and one implementation PR because bulk input and settled-layout reuse jointly provide the first measurable responsiveness slice.
- Keep Kiki’s synchronous, demand-driven TUI loop; reject an async Codex event-loop transplant before the measured rendering work is removed.
- Implement Kiki-native caching rather than copy Codex transcript code, whose history-cell and terminal-scrollback model does not match Kiki’s retained viewport.
- Cache settled render blocks separately from mutable live blocks; reject a single whole-layout cache because every live delta would invalidate unchanged history.
- Cache one width-specific cell raster and selection metadata per settled block; reject text-only caches because Ratatui would still wrap on every draw, reject a second prepared representation because it would optimise only resize, and reject multi-width caches until resize behaviour demonstrates a need.
- Raise the prompt limit from 16 KiB to 64 KiB rather than remove it, preserving a bound on editor layout and model input.
- Accept the longest complete UTF-8 prefix that fits, preserving the byte bound and UTF-8 validity rather than rejecting an otherwise useful paste.
- Normalise unsafe paste content before enforcing the byte limit so the stored prompt is exactly the safe text the user sees.
- Keep selection and scrolling outside cache identity; they alter the viewport treatment of existing render data, not the underlying parsed and measured content.
- Use deterministic cache/repaint tests in the default suite and wall-clock thresholds only in an opt-in release benchmark to avoid platform-sensitive ordinary test failures.
- Treat 8.3 ms as the target and 16.7 ms as the hard key/scroll tolerance; allow 50 ms for one maximum-size paste transaction.

## Risks / unknowns

- Width-specific cell rasters increase retained memory relative to source-only history. Keeping one current raster per block bounds cache multiplication, but total cached cells still grow with unbounded display history and must be copied only for visible or extracted rows.
- A settled block taller than Ratatui’s `u16` rectangle height remains uncached and can still make interaction slow. This preserves the current rendering path rather than expanding the change into custom tall-block wrapping.
- Rust ownership changes could accidentally replace repeated parsing with repeated deep cloning. Shared-raster composition, cache-bypass equivalence tests and the release benchmark must exercise the rendered path, not only cache bookkeeping.
- Incorrect invalidation could display stale width, reasoning, arena, accounting or live content. Focused invalidation tests and the existing transcript rendering suite remain authoritative.
- Selection extraction depends on layout block identity, heights and wrapping. Reusing blocks must not preserve selection state or alter extraction separators, soft-wrap removal or Unicode handling.
- A 64 KiB prompt still requires whole-prompt visual layout on redraw. The maximum-paste benchmark will expose immediate regressions, but optimising the prompt editor itself remains outside scope unless the agreed threshold cannot be met.
- The live block is still reparsed as it grows. Very long single live responses may remain expensive and could require incremental Markdown rendering in a later change.
- Bracketed paste depends on terminal support. Unsupported terminals may continue delivering pasted text as ordinary keys; heuristic paste-burst detection remains explicitly deferred.
- Opt-in wall-clock results remain machine-sensitive even in release mode. The tolerance is a local engineering gate, while deterministic reuse tests prevent the core regression in the default suite.

## Dependencies

- Existing prompt, TUI, transcript and selection modules: `src/prompt_editor.rs`, `src/tui.rs`, `src/transcript.rs`, `src/selection.rs`.
- Existing terminal lifecycle and Ratatui `TestBackend` seams in `src/tui.rs`.
- Ratatui `0.30.2` and its Crossterm integration already present in `Cargo.toml`.
- Existing display-history selection contract: `docs/2026-09-02_history-text-selection/spec.md`.
- Existing live display and prompt contract: `docs/2026-08-27_live-openai-agent/spec.md`.
- Maki’s demand-driven rendering and segment-cache research: `docs/research/2026-08-25_maki-tui-architecture.md`.
- Checked-in Codex bracketed-paste and cache implementations as design references only: `.research/codex/source/codex-rs/tui/src/tui.rs`, `.research/codex/source/codex-rs/tui/src/tui/event_stream.rs`, `.research/codex/source/codex-rs/tui/src/pager_overlay.rs`.

No new runtime dependency, copied Codex source, licence notice, background worker or persistent cache format is expected.

## Plan

### Acceptance criteria

- After successful configured-TUI terminal initialisation, Kiki enables bracketed paste before mouse and keyboard modes and disables the enabled modes in reverse order on normal return, loop error and unwinding. A bracketed-paste enable failure attempts its own cleanup and enters no later mode or loop; terminal initialisation failure issues no paste command. The read-only configuration TUI does not enable bracketed paste.
- In an editable prompt, one successful `Event::Paste` inserts one normalised payload at the caret and owes one redraw. It works while idle and during an active Run, preserves surrounding Unicode text, and never submits, cancels or queues a request. An empty or full-capacity no-op paste redraws only if clearing transient feedback changes visible state.
- Paste converts CRLF and lone CR to LF, tabs to four spaces, and remaining C0/C1/delete controls to uppercase `\xNN`. The resulting prompt never exceeds 64 KiB and ends at a complete UTF-8 boundary; an over-limit paste retains the longest fitting prefix.
- Configuration-only and runtime-disabled views ignore pasted prompt content. Existing copy feedback clears consistently with other input, and existing selections follow their current invalidation contract.
- Repeated prompt edits and history scrolling at an unchanged width reuse every compatible settled render block. A live progress update rebuilds live render blocks without rebuilding compatible settled blocks.
- Appending an immutable record rasterises only its new blocks; live-to-settled replacement removes the transient suffix and appends its immutable blocks. A width change rebuilds all rasters from normalised source, including Markdown parsing and Python highlighting; reasoning visibility rebuilds only reasoning rasters; arena expansion rebuilds only arena rasters. No stale retained variant remains reachable after replacement.
- Compatible settled blocks whose measured height fits Ratatui’s `u16` rectangle limit render by copying visible cached cells and selection metadata without invoking `Paragraph`, Markdown parsing, Python highlighting, wrapping or height measurement. Taller blocks retain the existing uncached path. Cached and test-only bypass construction produce the same transcript pixels, total height, footer, viewport behaviour, selection highlighting and extracted text across existing narrow-width, Markdown, Unicode, collapsed-arena and active-progress fixtures.
- The ignored release benchmark uses the fixed 128-run, 1,048,576-byte settled assistant fixture, active run 129, eight warm-up frames, 50 key samples, 50 scroll samples and one exact 65,536-byte normalised paste described above. Nearest-rank p95 targets 8.3 ms and fails only above 16.7 ms; paste fails only above 50 ms.
- Default tests remain deterministic and do not enforce wall-clock latency. Existing demand-driven redraw counts and runtime progress semantics remain unchanged.
- README documents bracketed bulk paste, normalisation, active-Run draft editing and the 64 KiB prompt limit without claiming support for unbracketed paste detection.

### Overall validation

Run from the feature worktree:

```sh
cargo fmt --all -- --check
cargo check --all-targets
cargo test prompt_editor::tests
cargo test transcript::tests
cargo test tui::tests
cargo test --release tui::tests::responsiveness_benchmark -- --exact --ignored --nocapture --test-threads=1
env -u OPENAI_API_KEY MONTY_BIN=/tmp/kiki-monty-0.0.21/bin/monty cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets -- -D warnings
git diff --check main...HEAD
```

All commands must pass. The benchmark must print its target, tolerance and observed key, scroll and paste measurements. The ordinary full suite skips both the responsiveness benchmark and the credential-gated live OpenAI test; the preceding explicit release command is required to run the responsiveness benchmark. Verification fixes stay within the files and non-goals already named here; any defect receives the narrowest regression test at its owning module seam.

### Planning context

- `PromptEditor::layout` derives wrapped rows and caret position from the complete stored string, so paste normalisation must happen before storage and capacity enforcement (`src/prompt_editor.rs:88-118`).
- Normal and configuration-only entry points share some terminal setup helpers but run separate loops. Bracketed-paste mode belongs only to the configured entry point because the configuration loop has no editable prompt (`src/tui.rs:639-751`, `src/tui.rs:762-875`).
- `TranscriptLayout` is currently owned and rebuilt per frame, and its settled render path clones `Text` and invokes Ratatui wrapping again during drawing. The caching change must retain block-local cell rasters and selection row-break metadata behind the transcript presentation seam rather than copy layouts in `App` (`src/tui.rs:1000-1038`, `src/transcript.rs:499-541`, `src/transcript.rs:1250-1265`, `src/transcript.rs:1351-1480`, `src/transcript.rs:1672-1758`).
- Python highlighting is stateful mutable work during arena block construction. Compatible settled rasters avoid re-entering it during ordinary frames; width or arena-expansion invalidation rebuilds it from normalised source (`src/transcript.rs:836-913`).
- The transcript footer aggregates settled and live attempt accounting during projection. Reusing blocks must not leave footer values stale when attempts change (`src/transcript.rs:1764-1783`).
- Display-history selection already renders and extracts from layout blocks in vertically bounded chunks. Cache identity must exclude selections while retaining the exact block metadata those operations require (`src/transcript.rs:1398-1480`).
- The existing fake event source and TestBackend helpers are private to `src/tui.rs`; keeping the opt-in benchmark there avoids widening production interfaces solely for measurement (`src/tui.rs:1190-1290`).

### Task 1: Add bulk bracketed paste

**Changes** — Raise prompt capacity to 64 KiB and add one editor bulk splice which inserts the longest fitting complete UTF-8 prefix at the caret. In the TUI paste path, normalise CR line endings before passing content through the existing terminal-text normaliser and then the editor. Enable and restore bracketed-paste mode only in the configured TUI through the existing guarded lifecycle. Route paste events through the bulk path in editable views, including during active Runs, while preserving read-only behaviour, selection invalidation and one-redraw demand semantics. Reuse the production prompt-limit constant in editor tests and document the new capacity and paste behaviour.

**Tests** (`src/prompt_editor.rs`, editor behaviour; `src/tui.rs`, input and terminal integration) — First fail on multiline paste at an interior Unicode caret. At the editor seam, cover interior splicing, exact-limit input and multi-byte truncation. At the TUI seam, use a representative mixed payload to prove CRLF and lone-CR conversion, tab expansion, visible control notation and normalisation before byte accounting; rely on the existing exhaustive terminal-normaliser suite for every C0/C1/delete value. Cover empty and wholly uninsertable payloads, idle and active-Run insertion, no submission or command emission, configuration-only and runtime-disabled no-ops, copy-notice handling, successful one-redraw behaviour, and no-op dirty behaviour. Add guarded lifecycle call-order tests proving configured-mode enable-before-mouse, reverse cleanup on success, loop error and unwind, paste cleanup after later mouse-enable failure without keyboard setup or loop entry, self-cleanup after paste-enable failure, no paste command after terminal-initialisation failure, and no paste command in configuration-only mode. Update the existing test that currently requires paste content to be ignored.

**Files** — Modify: `src/prompt_editor.rs`, `src/tui.rs`, `README.md`.

**Verification** — `cargo fmt --all -- --check && cargo test prompt_editor::tests && cargo test tui::tests && cargo check --all-targets` passes.

**Commit** — `Add bulk bracketed paste`

### Task 2: Cache settled transcript rendering

**Changes** — Normalise each immutable display block once and retain one current width-specific styled-cell raster with height and block-local selectable-region geometry plus row-break metadata. Compose shared settled rasters with separately rebuilt live blocks and current footer metadata. Normal draws and extraction copy only needed raster rows without deep-cloning render trees or rerunning Ratatui wrapping. Preserve compatible settled entries across prompt redraws, viewport scrolling, live progress and appended records; invalidate all rasters for width, only reasoning rasters for visibility, and only arena rasters for expansion. Keep selection and scroll state outside cache identity, and add a test-only fresh-build bypass at the same transcript interface.

**Tests** (`src/transcript.rs`, projection and cache behaviour; `src/tui.rs`, render integration) — First fail with test-only construction accounting showing that a second unchanged layout performs no settled rasterisation, parsing, highlighting, wrapping or selection-break construction. Cover prompt-only and scroll-only frames, live deltas, live settlement, newly appended immutable blocks, attempt/footer changes, width changes rebuilding all rasters, reasoning and arena toggles with unaffected block reuse, one retained variant after repeated resize/toggle cycles, an over-`u16` block retaining the uncached path, and stale-progress no-ops. Compare cached and bypassed pixels, total height, footer, selection highlighting and extraction at the same widths and state transitions, then re-run narrow-width, Markdown, highlighting, Unicode, collapse and footer fixtures.

**Files** — Modify: `src/transcript.rs`, `src/tui.rs`.

**Verification** — `cargo fmt --all -- --check && cargo test transcript::tests && cargo test tui::tests && cargo check --all-targets` passes.

**Commit** — `Cache settled transcript rendering`

### Task 3: Add the responsiveness benchmark

**Changes** — Add one ignored release benchmark at the existing private TUI test seam using the exact persistent terminal/app, 128 settled runs, 8,192-byte assistant messages, active run 129, 27-byte live deltas, eight warm-ups, 50 key samples, 50 alternating scroll samples, timer boundaries, nearest-rank p95 and exact 65,536-byte normalised paste fixture specified above. Retain all samples, print targets and observations before assertions, fail only at the agreed inclusive limits, and refuse a debug benchmark run. Leave ordinary tests free of wall-clock assertions.

**Tests** (`src/tui.rs`, deterministic benchmark helpers and opt-in release performance regression) — Prove the 48th one-based value (`sorted[47]`) is p95 for 50 unsorted samples, four-sample p95 selects the maximum, and exact threshold values pass while one nanosecond above fails. Assert the settled assistant-source total and normalised paste size during untimed benchmark setup. Run the wall-clock harness to confirm it exercises changing live output, settled-raster reuse, key editing, viewport scrolling and the production bulk-paste path.

**Files** — Modify: `src/tui.rs`.

**Verification** — `cargo test tui::tests && cargo test --release tui::tests::responsiveness_benchmark -- --exact --ignored --nocapture --test-threads=1` passes and reports p95 key/scroll values at or below 16.7 ms and paste at or below 50 ms.

**Commit** — `Add TUI responsiveness benchmark`

