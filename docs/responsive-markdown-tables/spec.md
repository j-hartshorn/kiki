# Spec: Responsive Markdown tables

> Approved for implementation by the user, including the separator hierarchy and final grapheme-safe fallback.

## Problem

Markdown tables currently render at their natural width, then wrap as ordinary paragraph text. Borders and cells break independently and the relationship between headers and values becomes hard to follow. Some wrapping is useful, however: a grid with two or three lines per row can still be more readable and compact than records.

## Proposed experience

Choose between three presentations, independently for each table:

1. **Natural grid:** keep the current table unchanged when it fits.
2. **Wrapped grid:** constrain column widths and wrap cells, provided every row needs at most **three content lines**.
3. **Records:** if the constrained grid would need more than three lines in any row, show the entire table as YAML-like records:

```text
- Name: Kiki
  State: ready
  Description: A programmable coding harness

- Name: Worker
  State: running
  Description: Longer values can wrap without being
  confused with a neighbouring column.
```

The user agrees that this is a reading layout rather than strict YAML serialization. Make ordinary cases YAML-compatible where straightforward, but do not sacrifice readability or lose data to guarantee it.

## Scope

Apply to every surface that already renders Markdown through the transcript helper:

- Assistant messages, including live commentary and final messages.
- Visible reasoning.
- Expanded Markdown compaction summaries.

User requests, tool output, code fences, skill source, and other literal-text surfaces remain literal. Tables inside Markdown lists and blockquotes retain their surrounding structure.

## Behavior

### Choosing the layout

1. Use actual content width after effective card padding and enclosing Markdown indentation/prefixes, not terminal width or a fixed breakpoint.
2. Measure the natural table as currently displayed, including headers, cells, column padding, separators, and outer borders. Count terminal display columns, not bytes or character counts. Include displayed link destinations and other visible inline decorations.
3. If the natural grid fits, keep it unchanged.
4. Otherwise, allocate narrower columns within the available width and measure word-wrapped cells using the same policy used to draw them. Do not split words to obtain a smaller row height.
5. A row's height is the maximum number of content lines needed by any cell in that row, not their sum. Count the header as a row too. Borders and separator lines do not count toward the limit.
6. Keep the wrapped grid if every row needs at most three content lines. If any row needs four or more, use records for the entire table. Do not truncate a cell at three lines.
7. If borders, padding, and minimally displayable columns cannot fit, use records immediately.

Three lines is the approved starting limit based on the user's “2 or 3” preference; it remains a fixed internal policy, not a setting. The threshold applies to the deterministic column allocation below, not an exhaustive search over every possible layout.

### Wrapped grid

- Reserve the existing grid's borders, separators, and cell padding before allocating cell widths.
- Keep naturally short columns short and distribute remaining space evenly among wider columns, capped at each column's natural width. Each column must be at least as wide as its longest unbreakable word/token, including header content. If those minimum widths plus grid overhead cannot fit, use records. Break allocation ties in column order, so the same source/width always produces the same layout.
- Wrap within each cell at word boundaries only, preserving inline styles and existing column alignment. Do not split a word, URL, code identifier, or grapheme; insert hyphens; or treat a styling-span boundary as permission to break a word. Use appropriate Unicode word/line-break opportunities for scripts without spaces, not arbitrary character or byte boundaries.
- Pad shorter cells to the height of the tallest cell in the row. Keep vertical borders aligned on every physical line, and use horizontal separators between body rows in the wrapped grid so row boundaries remain clear.
- Do not let the outer paragraph renderer wrap the completed grid a second time. Every generated grid line must fit its usable width.

### Record layout and YAML friendliness

- Emit one dash-led record per parsed body row, with a blank line between records. Emit every parsed column in source order and repeat the header for each row. Duplicate headers remain separate visible fields; never turn rows into dictionaries and overwrite values.
- An empty header becomes `Column N`, using its one-based position. Represent an empty cell as `""`, not an omitted field or `null`.
- Use YAML-compatible scalar quoting when a label or value would otherwise change meaning or structure: for example `true`, `null`, numeric-looking strings, or text containing `: ` or a comment introducer. Treat table values as text; do not infer YAML types.
- Preserve rendered value text, inline code styling, emphasis, links, Unicode, and literal escaped pipes. Quotes/escapes are presentation syntax around that text, not a reason to discard its styles.
- Every field starts on a new logical line. Prefer word-boundary wrapping; later fields still start separately. Move a whole word/token to the next line when it cannot fit in the remaining space, including after a field label. Only a token wider than a full usable record line may use the separator hierarchy below. Do not split a token just to fill the remaining part of a line.
- Use existing bold header styling for labels and ordinary body styling for values. Do not add a code fence or YAML syntax highlighting.
- If a header-only table cannot satisfy the grid policy, show column labels vertically followed by `(no rows)`, without inventing a record.
- Honor the existing parser's treatment of short/excess rows. This is not a Markdown repair pass.

Simple records should be valid YAML where their displayed scalars can be represented straightforwardly. Strict parseability is not an acceptance criterion: duplicate keys, arbitrary wrapped display text, and the header-only presentation remain legitimate reading layouts. No promise of round-trip export or typed YAML data.

### Overlong tokens: separator hierarchy

The user permits punctuation-based breaks for a token wider than the full view. The approved order is:

1. Path separators: `/` and `\`.
2. URL query/fragment separators: `?`, `&`, and `#`.
3. Identifier separators: `_` and `-`.
4. Other structured-token separators: `.`, `:`, and `=`.

Ordinary whitespace/word boundaries always take precedence. Apply this hierarchy only in records, after the table has already fallen back, and only to a token that cannot fit on its own full usable record line. Do not split tokens at punctuation just to keep a grid under the three-line limit. Full line width here excludes any mandatory continuation indentation, but not the label on a previous line.

For an overlong token, choose the first tier with an eligible break that fits, then the rightmost fitting break within that tier. Break after the separator, leaving it visible with the preceding segment. Apply the same policy to the remainder as necessary. This intentionally prefers recognizable structural boundaries over perfectly filled lines; no extra fill-ratio heuristics or language-specific URL parsers in this phase.

Never split inside a grapheme or an escape sequence. Treat a URL scheme such as `https://` as an indivisible prefix, and do not strand that prefix alone on a line just because it contains slashes/colon. Do not add hyphens, remove separators, or insert spaces into the underlying token. Inline styling boundaries are not break opportunities. Copy must rejoin separator-wrapped continuations with **no added space**, unlike ordinary word wraps.

**Approved final fallback:** when no eligible separator can make progress, split the overlong token at the last whole Unicode grapheme that fits. This is an emergency exception only for a token wider than a full record line, such as a separator-free hash; ordinary words still move intact to the next line. If one indivisible grapheme itself is wider than the view, preserve it in source and handle the non-renderable width safely. This proposal avoids horizontal scrolling and losing the rest of a value to visual clipping. This last-resort exception was approved when the user authorized implementation.

### Resize, streaming, and copy

- Recompute from original Markdown whenever content width changes. Resize can move between natural grid, wrapped grid, and records in either direction; never persist a layout choice.
- During streaming, apply the same policy to the recognized table so far. Before recognition, render the fragment normally. Later cells can change column allocation or trigger records; no additional debounce or mode locking.
- Keep existing bottom-follow/manual-scroll behavior. Exact semantic row anchoring across reflow is out of scope.
- Copy the displayed representation, not original Markdown or a separate YAML export. Preserve physical lines for a grid, including its borders. In records, preserve field/record hard breaks and continue joining ordinary soft wraps according to existing copy behavior.
- Preserve the existing rule that resizing invalidates screen-coordinate selections.
- Zero-width views render no text. Tiny views remain safe without panics, subtraction underflow, or lost stored content. Ordinary word wrapping must not silently become character wrapping; overlong record tokens use only the explicitly agreed exception policy.

## Acceptance criteria

1. A naturally fitting table is unchanged. Narrowing can produce an aligned wrapped grid before records are needed.
2. Fixtures needing exactly two and three content lines per row stay grids; a fixture needing four lines in any one row switches the entire table to records. Test the same rule for the header, and exclude separators from the count.
3. Column allocation is deterministic, keeps short columns compact, and includes border/padding/prefix costs. No completed grid line overflows, even at an exact-fit boundary.
4. One message can contain tables in all three presentations. Live and settled wide → medium → narrow → wide reflow does not change stored Markdown.
5. Reasoning and expanded compaction summaries use their padded content widths, including tiny-width padding reduction.
6. Escaped pipes, styled values, reference links defined elsewhere, CJK, combining characters, and emoji work across presentations. Lists, quotes, references, footnotes, surrounding content, and literal code/prose remain intact.
7. Duplicate/empty headers, empty cells, header-only tables, ragged rows, and incomplete streamed tables have regression coverage. Common scalar cases use safe YAML quoting without losing fields or inferring types.
8. Height, clipping, selection, copy, and cached/uncached layouts agree after switches. Unchanged settled content still reuses its width/theme raster.
9. Both built-in themes remain readable without new theme keys.
10. Normal grid/record wrapping preserves whole words/tokens, including across style spans. An overlong grid token forces column widening or records, not punctuation splitting to retain the grid.
11. A token wider than a full record line uses the approved separator order with deterministic rightmost-within-tier breaks and lossless, no-added-space copy. Cover scheme/escape protection and a token that fits a full line but not the space after its label. Test the approved separator-free final fallback.

## Non-goals

- Guaranteed valid YAML, round-trip export, copy-as-source, or a new export command.
- User settings, manual layout toggles, configurable thresholds, or new shortcuts.
- Session/schema/model-context changes or asking the model to change formats.
- A general Markdown renderer replacement, dependency fork, or horizontal scrolling. A shared, Unicode-aware wrapping policy, the scoped overlong-token exception, and a private cell-aware grid formatter are in scope; do not create a configurable general-purpose wrapping framework.

## Existing implementation and proposed seam

- `src/transcript.rs:182–192`: `markdown` calls `tui_markdown::from_str_with_options` without width.
- `src/transcript.rs:1573–1604`: `RenderBlock::card` applies effective padding and ordinary `Paragraph` wrapping.
- `src/transcript.rs:3595–3760`: `build_render_block` knows block width and constructs Markdown-bearing blocks.
- `src/transcript.rs:2998–3056`: settled rasters already invalidate by width and theme.
- `src/tui.rs:2795–2798,2890–2893`: resize already clears layout-dependent selections.

Add a private Markdown module behind the existing helper. It accepts source, usable content width, and existing styling options and returns owned text/layout information, including soft-break provenance where needed. Parsing, word wrapping, allocation, grid construction, and record formatting stay behind this seam. Drawing, height measurement, and selection must use the same wrapping result rather than allowing a later paragraph pass to choose unapproved token breaks. See [the implementation plan](plan.md).

## Agreement and remaining defaults

- Agreed: allow modest wrapping before falling back; prefer whole-word wrapping, with character-separator breaks permitted for a token wider than the view; YAML-like readability is the goal, not strict serialization.
- Approved defaults: three content lines maximum for any row, including the header; capped-even column allocation; YAML-friendly scalar quoting; the separator tiers and rightmost-within-tier rule.
- Approved exception: last-resort grapheme-safe breaking of a token wider than a full record line when no eligible separator can make progress.
