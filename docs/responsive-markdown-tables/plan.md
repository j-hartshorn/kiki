# Plan: Responsive Markdown tables

> Approved for implementation. Source: [spec](spec.md).
> Revised to allow up to three content lines per grid row, whole-word wrapping by default, and hierarchical separator breaks only for overlong record tokens. The user approved the separator-free emergency fallback.

## Approach

Keep the existing Markdown renderer for ordinary content, inline styling, and naturally fitting tables. Add a private structural table adapter that can render constrained, cell-wrapped grids and YAML-like records as owned Ratatui text.

**The earlier source-to-records rewrite is no longer sufficient:** a fixed-width table must now be laid out cell by cell before the outer paragraph wrapper sees it. Do not simulate this by wrapping entire grid lines or by truncating a cell at three lines.

### Interface

A private `src/transcript/markdown.rs` module exposes one operation to its parent:

```rust
pub(super) fn render(
    source: &str,
    content_width: u16,
    options: &MarkdownOptions,
) -> MarkdownLayout
```

Use the parent's concrete `MarkdownOptions`; no renderer trait, plugin port, or new configuration. The existing `markdown` helper gains content width, keeps construction-count instrumentation, and delegates here. Callers continue applying base/card styles. `MarkdownLayout` owns the rendered text and the line-break provenance needed to distinguish logical field/grid lines from word-wrap continuations. Refine its concrete shape alongside existing `RowBreaks` integration; a bare `Text` result is insufficient if prewrapped record lines would become hard breaks during copy. Do not discard this information or let a later paragraph pass choose additional token breaks. Track ordinary word-wrap continuations separately from intra-token separator/emergency continuations so copy can join them with a space versus no space.

Internally, separate structural extraction/context handling, a pure column/layout policy, and grid/record formatting. These are implementation details, not additional interfaces for callers. Most behavioral tests exercise `render`; focused policy tests are justified for allocation and the three-line decision.

### Rendering integration: first-task gate

`tui-markdown 0.3.9` has no public width parameter or event-rendering interface, and its table builder is private. The proposed local integration is:

1. Discover tables structurally, retaining their source ranges, enclosing context, and document reference definitions.
2. Keep naturally fitting tables on the unchanged rendering path. For other tables, render their cells with the existing inline/style behavior and retained references, then construct the wrapped grid or records locally.
3. Replace those table blocks with temporary, collision-free header-only carrier tables in the otherwise complete document. Keep the original delimiter row so the pinned renderer retains list/quote placement; identify the rendered header by marker identity, not border-glyph parsing. Render that document with the existing renderer, preserving surrounding reference definitions, list numbering, and quotes.
4. Expand marker locations into prepared table text. Preserve enclosing prefixes and list-marker versus continuation indentation; markers must never appear in visible text, selection, or copy. Respect contextual styles when splicing spans.

Markers are private, per-render bookkeeping, checked absent from the input; do not use persistent IDs, raw-source instructions, or box-drawing detection. Independently rendering arbitrary document slices is not an acceptable shortcut.

**Task 1 proved this integration through rendered container/reference fixtures.** The original gate required that evidence before proceeding. If the marker approach cannot preserve it simply, stop and revise the plan rather than silently forking the dependency or dropping nested-table support.

### Dependencies and policy

- Add a direct `pulldown-cmark 0.13` dependency while retaining the currently locked `0.13.4` resolution. Match the existing renderer's enabled parser extensions. Do not split table rows on `|`.
- Use one explicit wrapping policy for measurement and drawing. Grid cells preserve whole tokens. Records preserve whole tokens unless a token is wider than a full usable record line; only then use separator tiers `/` or `\`, then `? & #`, then `_ -`, then `. : =`, as detailed in the spec. Choose the highest-priority eligible tier, then its rightmost fitting separator, and break after it. Protect schemes, graphemes, and escape sequences. Reuse existing Unicode/display-width facilities where suitable; do not assume the outer paragraph wrapper respects these rules. No implicit byte/character fallback, inserted hyphens, or breaks at style-span boundaries.
- The existing grid costs `3 * column_count + 1` columns beyond its cell-content widths. Subtract that and enclosing prefixes before allocation. Minimum column width is the longest unbreakable rendered word/token in that column, including its header; if the sum cannot fit, choose records.
- Keep short columns at natural width and distribute remaining space evenly among wider columns, capped at their natural widths, with column-order tie-breaking. No exhaustive optimization over width combinations.
- Row height is the maximum wrapped cell height, including the header row. At most three content lines qualifies; four in any row triggers records for the entire table. Border/separator rows are excluded.
- No persistent parse cache or remembered layout mode. Reuse existing settled-raster caching.
- The user approved the spec's final grapheme-safe fallback. It applies only when an overlong record token has no eligible separator that can make progress, not to ordinary words that could move to the next line. Do not silently enable forced splitting everywhere.

## Tasks

### 1. Prove structural extraction and full-document integration

**Files:** new `src/transcript/markdown.rs`; module declaration in `src/transcript.rs`; `Cargo.toml` and its root dependency entry in `Cargo.lock`.

**Red first:** Verify unchanged styled output for no tables and naturally fitting tables. Use narrow-table fixtures to exercise extraction/substitution with code spans, escaped pipes, references defined after a table, surrounding paragraphs/footnotes, table-first and table-after-text list items, ordered lists, blockquotes, nested containers, and content following each table. Include input resembling a proposed marker so collision avoidance is tested.

**Implement:** Structural table/cell extraction and temporary table placement behind `render`. Preserve document references when rendering cell contents. Ensure literal code and pipe-containing prose are not recognized as tables. Prove that local table layout can be substituted without changing surrounding content or leaking markers, and that measurement, drawing, height, and selection can share its word-wrap/soft-break metadata. This task need not complete width allocation yet. Verify how to prevent the outer paragraph pass from forcing word splits; the existing all-text wrapping path cannot be assumed correct.

**Gate:** Inspect final rendered text/styles, context prefixes, numbering, and copy output, not only an intermediate source string. If the integration fails, revise the design before production wiring. Avoid rerendering/highlighting the whole document once per candidate table.

### 2. Implement constrained grids and the row-height policy

**Files:** `src/transcript/markdown.rs`.

**Red first:** Cover natural fit, exact constrained fit, rows of one/two/three lines, and a fourth-line fallback decision. Count the header too, and show that many short rows are not rejected merely because the table is tall. Cover a short ID column plus long prose, multiple wide columns, deterministic ties, long words/URLs, alignment, zero/tiny widths, CJK, combining characters, and emoji. Include words crossing emphasis/code styling spans, a long token that forces a wider column or records, and a sentence whose height exceeds three lines under word-only wrapping but not under character splitting. Include a fixture where measuring and drawing with different wrap rules would produce an overflow.

**Implement:** Render inline cells, determine longest-word column minima, allocate widths, and wrap using the shared word-only policy. Never split a token to make a grid qualify for the three-line threshold. Keep fitting grids unchanged. For constrained grids, pad each cell to row height, align it within its column, draw matching vertical borders on every physical line, and separate body rows horizontally. Preserve cell styles. Each completed grid line must fit the content width so `Paragraph` never rewraps it. Return the whole-table record decision if minimum widths cannot fit or any row exceeds three content lines; never clip at the threshold.

**Verify:** Focused policy tests and adapter tests. Compare physical grid widths, height, and styled spans, not just substrings. The allocation is deterministic rather than an optimal-layout search.

### 3. Implement YAML-friendly reading records

**Files:** `src/transcript/markdown.rs`.

**Red first:** Multiple records and field/record breaks; empty/duplicate headers; empty cells; header-only tables; parser-normalized ragged rows; long labels/values; styled values; labels with punctuation; literal quotes/backslashes; text such as `true`, `null`, `123`, `a: b`, or `x # y`; displayed links, images/HTML as already supported; both themes.

**Implement:** Emit dash-led records and repeated labels directly as styled text with hard field breaks. Use `Column N` for empty labels and `""` for empty values. Apply scalar quoting/escaping for common ambiguous strings while preserving inline styling and the visible text's meaning. Quote numeric/boolean/null-looking values as strings rather than infer types. Retain duplicate fields even when that prevents strict YAML compatibility. Use the shared word-first policy for long fields, including after labels; never rely on the existing paragraph wrapper's forced character splitting. Preserve soft-break provenance for selection/copy. For a token wider than a full record line, use the separator hierarchy and keep every separator. Track these continuations as no-added-space soft breaks, including when spans change style. A token that fits a fresh line must move there intact. Implement the approved separator-free emergency fallback.

**Verify:** Exact expected YAML-compatible text for representative simple cases, plus lossless field/value display outside the guarantee. Add URL/path/query/identifier examples for every separator tier, competing-tier/rightmost choices, scheme and escape protection, repeated splitting, styled tokens, and a separator-free hash. Assert copied tokens exactly match the original rendered token with no inserted spaces or lost punctuation. Full YAML parsing is not the acceptance oracle; no export command or complete YAML serializer is required.

### 4. Supply usable widths to all Markdown surfaces

**Files:** `src/transcript.rs` and its tests.

**Red first:** Assistant content, visible reasoning, expanded compaction Markdown, and live commentary. At the same outer width, different padding must affect column widths and the grid/record decision. Cover widths where `effective_padding` reduces padding.

**Implement:** Pass content width into `markdown` and `assistant_text`, including test-only callers. Reuse the `effective_padding` calculation from `RenderBlock::card`: assistant padding 0, reasoning 2 per side, compaction 1 per side, subject to tiny-width reduction. The adapter accounts for enclosing Markdown prefixes; do not subtract twice. Continue applying base/card styling and appending accounting as today.

**Verify:** Existing Markdown compatibility/style, theme, accounting, reasoning-toggle, compaction-expansion, and transcript tests. Literal user/tool/code/skill surfaces remain unchanged.

### 5. Verify reflow, streaming, caches, and interaction

**Files:** tests in `src/transcript.rs`; targeted `src/tui.rs` tests if needed. Change production selection code only if a regression test demonstrates a defect.

**Red first:**

- Live and settled wide → medium → narrow → wide selects natural grid → wrapped grid → records → natural grid from unchanged source. One document can have all three presentations.
- Cached and uncached buffers, heights, clipping, and extracted selections agree after each switch.
- A streamed header/delimiter/body becomes a recognized table; later text can cause wrapped cells and then records without stale rows. Settlement preserves the presentation at the same width.
- Copy preserves physical grid lines and borders, including wrapped-cell rows. It is not expected to reconstruct each original cell. Record copy preserves field/record hard breaks and joins generated word-wrap continuations correctly, even when wrapping was performed before paragraph rendering. For separator-wrapped tokens, join continuations with no added space and preserve the original token exactly; test the confirmed separator-free fallback the same way. Markers never appear in copy.
- Resize clears selection through the existing rule. Bottom-follow/manual scroll remain in bounds, and long records can be clipped/scrolled without missing final fields.
- Existing construction counters show settled raster reuse at unchanged width/theme and appropriate rebuilding when either changes. Include zero/one-column views and both themes.

**Implement:** Reuse existing width/theme raster invalidation and normal raster/selection construction. Do not store table presentation in records or session data.

## Final validation

From the implementation worktree:

- Focused adapter, word-boundary/allocation, Markdown compatibility, cache, selection, and resize tests.
- Regression cases proving normal words/tokens remain intact, overlong-record exceptions follow the separator hierarchy, and copy distinguishes word-wrap spaces from intra-token no-space joins. Include the separator-free fallback as approved.
- `just test`.
- `just lint`.
- `cargo fmt --all -- --check`.
- Manual: resize a table through natural grid, two-/three-line wrapped grid, and records in both directions and both themes. Include another compact table in the message. Repeat with reasoning and expanded compaction; scroll and copy both wrapped grids and records.

Use `work/responsive-markdown-tables` and matching `worktrees/responsive-markdown-tables` for implementation. No session codec, model prompt, or unrelated renderer changes. Implementation verification is recorded below; interactive real-terminal validation remains a user check.

## Evidence

- Existing helper/options: `src/transcript.rs:43–192`; padded layout: `:580–584,1573–1604`; Markdown callers: `:1017–1042,3646–3741`.
- Width/theme cache: `src/transcript.rs:2998–3056`; regression entry points: `markdown_compatibility`, `markdown_element_styles_use_pi_tokens`, `settled_cache_appends_and_invalidates_only_affected_rasters`.
- Resize invalidation: `src/tui.rs:2795–2798,2890–2893`; existing selection/scroll tests include `selection_invalidates_when_history_geometry_or_layout_changes` and `transcript_following_bottom_and_manual_scroll_survive_live_replacement`.
- Installed `tui-markdown 0.3.9` source was inspected (`renderer/mod.rs`, `renderer/table.rs`, `renderer/list.rs`, `options.rs`); Context7 did not index it. Public source: https://docs.rs/crate/tui-markdown/0.3.9/source/src/renderer/.
- Parser source-offset interface: https://docs.rs/pulldown-cmark/0.13.4/pulldown_cmark/struct.Parser.html. Context7's closest indexed docs were 0.13.5; installed 0.13.4 source was checked for offset iteration/reference-definition spans.

## Implementation notes

- Task 1: structural integration uses header-only carrier tables and original delimiter rows. Cell rendering reuses the pinned renderer through a one-cell table with document references; output structure is covered by compatibility fixtures. The module stays test-only until Task 4.
- Task 2: capped-even allocation uses whole-token minima and a bounded level search; cell wrapping lives in a private helper and preserves styled word boundaries, including Unicode no-space continuations. Completed grid rows are all hard breaks for copy.
- Task 2 review fix: cells carry structural inline-code ranges, preserved with collision-free inline-code carriers during synthetic cell rendering. The wrapper treats code identifiers as whole tokens even in scripts where prose can break without spaces; matching text/style alone is not used as code identity.
- Task 3: record formatting is private to `markdown/records.rs`; scalar safety uses the existing YAML scalar classifier plus conservative JSON-compatible quoting, not arbitrary YAML document loading. Record wrapping preserves field indentation and distinct word/token continuation metadata; full-token separator and final grapheme policies are isolated from grid wrapping.
- Task 4: production Markdown cards carry optional boxed row metadata: untouched `Original` lines retain Paragraph wrapping; prepared grid/record rows render once. Explicit copy-prefix widths remove repeated list/quote prefixes only from soft continuations. `RowBreaks::single` bridges prepared rows to existing extraction. A test-only unconstrained `markdown` helper remains for renderer compatibility/style tests; production uses `markdown_layout` with usable widths.
- Approved implementation clarification: schemes and escape units remain protected when they fit a full usable line. If the unit itself is wider, emergency grapheme splitting may override that protection; prepared rows must fit and rendered copy must remain lossless. This replaces clipping of an overwide protected sequence, with an explicit exception only for an individual grapheme wider than the view.
- Task 5: added transcript/cache/streaming/chunked-copy regressions and independent TUI reflow/scroll/resize/mouse-copy tests. This was a verification-only task against implemented behavior: tests were run first without manufacturing a failing production change. A test-coordinate error involving UTF-8 byte offsets was corrected in the fixture, not production code.

## Verification evidence

- `just test`: exit 0, 1840 unit tests passed, 8 pre-existing ignored; 5 credentials and 17 sessions integration tests passed.
- `just lint`: exit 0.
- `cargo fmt --all -- --check`: exit 0.
- `git diff --check`: exit 0.
- Reviewed rendered previews at natural-grid, wrapped-grid, record, and restored-wide widths. Automated TUI coverage includes both themes, bottom-follow/manual scroll, resize selection invalidation, and mouse-copy of wrapped URLs.
- Still requires user validation: actual-terminal appearance and interactive resize/copy with real assistant output, visible reasoning, and expanded compaction. No live provider request or rollout was performed.
- Final simplification: `MarkdownLayout::push_original` and `prepend_original` own line/break/prefix synchronization for accounting and compaction headings; parent rendering no longer edits those parallel collections directly.
