mod wrapping;

use std::ops::Range;

use pulldown_cmark::{Alignment, Event, Options as ParseOptions, Parser, Tag, TagEnd};
use ratatui::text::{Line, Span, Text};

use super::{MarkdownOptions, from_str_with_options};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LineBreak {
    Hard,
    Word,
    Token,
}

#[derive(Debug)]
pub(super) struct MarkdownLayout {
    pub(super) text: Text<'static>,
    pub(super) breaks: Vec<LineBreak>,
}

impl MarkdownLayout {
    fn hard(text: Text<'static>) -> Self {
        let breaks = vec![LineBreak::Hard; text.lines.len()];
        Self { text, breaks }
    }
}

#[derive(Clone)]
struct Cell {
    line: Line<'static>,
    protected: Vec<Range<usize>>,
}

impl Cell {
    fn plain(line: Line<'static>) -> Self {
        Self {
            line,
            protected: Vec::new(),
        }
    }
}

struct Table {
    alignments: Vec<Alignment>,
    header: Vec<Cell>,
    rows: Vec<Vec<Cell>>,
}

struct SourceTable {
    range: Range<usize>,
    header_end: usize,
    alignments: Vec<Alignment>,
    header: Vec<Range<usize>>,
    rows: Vec<Vec<Range<usize>>>,
    prefix_width: usize,
}

pub(super) fn render(
    source: &str,
    content_width: u16,
    options: &MarkdownOptions,
) -> MarkdownLayout {
    render_with(source, content_width, options, |table, width| {
        let natural = natural_columns(table);
        let natural_width = natural.iter().sum::<usize>() + natural.len() * 3 + 1;
        if natural_width <= usize::from(width) {
            None
        } else {
            constrained_grid(table, width, options)
        }
    })
}

fn parse_options() -> ParseOptions {
    ParseOptions::ENABLE_STRIKETHROUGH
        | ParseOptions::ENABLE_TASKLISTS
        | ParseOptions::ENABLE_HEADING_ATTRIBUTES
        | ParseOptions::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | ParseOptions::ENABLE_SUPERSCRIPT
        | ParseOptions::ENABLE_SUBSCRIPT
        | ParseOptions::ENABLE_MATH
        | ParseOptions::ENABLE_FOOTNOTES
        | ParseOptions::ENABLE_DEFINITION_LIST
        | ParseOptions::ENABLE_GFM
        | ParseOptions::ENABLE_TABLES
}

fn extract(source: &str) -> (Vec<SourceTable>, String, String) {
    let parser = Parser::new_ext(source, parse_options());
    let mut definitions = parser.reference_definitions().iter().collect::<Vec<_>>();
    definitions.sort_by_key(|(_, definition)| definition.span.start);
    let mut references = String::new();
    for (label, definition) in definitions {
        let destination = definition
            .dest
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('\\', "&#92;")
            .replace('\n', "&#10;")
            .replace('\r', "&#13;");
        references.push_str(&format!("[{label}]: <{destination}>\n"));
    }
    let mut marker_text = source.to_owned();
    let mut tables = Vec::new();
    let mut active: Option<SourceTable> = None;
    let mut row = Vec::new();
    let mut lists = Vec::new();
    let mut items = Vec::new();
    let mut quotes = 0;
    for (event, range) in parser.into_offset_iter() {
        match &event {
            Event::Text(text) | Event::Code(text) => marker_text.push_str(text),
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                marker_text.push_str(dest_url)
            }
            _ => {}
        }
        match event {
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                let indent = lists.len() * 4 - 3;
                let width = match lists.last_mut().unwrap() {
                    Some(index) => {
                        let width = index.to_string().len().max(indent) + 2;
                        *index += 1;
                        width
                    }
                    None => indent + 1,
                };
                items.push(width);
            }
            Event::End(TagEnd::Item) => {
                items.pop();
            }
            Event::Start(Tag::BlockQuote(_)) => quotes += 1,
            Event::End(TagEnd::BlockQuote(_)) => quotes -= 1,
            Event::Start(Tag::Table(alignments)) => {
                active = Some(SourceTable {
                    header_end: range.start,
                    range,
                    alignments,
                    header: Vec::new(),
                    rows: Vec::new(),
                    prefix_width: items.last().copied().unwrap_or(0)
                        + quotes
                        + usize::from(quotes > 0),
                });
            }
            Event::Start(Tag::TableCell) => row.push(range),
            Event::End(TagEnd::TableHead) => {
                let table = active.as_mut().unwrap();
                table.header = std::mem::take(&mut row);
                table.header_end = range.end;
            }
            Event::End(TagEnd::TableRow) => {
                active.as_mut().unwrap().rows.push(std::mem::take(&mut row));
            }
            Event::End(TagEnd::Table) => tables.push(active.take().unwrap()),
            Event::FootnoteReference(label) if active.is_some() => {
                references.push_str(&format!("\n[^{label}]: note\n"));
            }
            _ => {}
        }
    }
    (tables, references, marker_text)
}

fn render_cell(source: &str, header: bool, references: &str, options: &MarkdownOptions) -> Cell {
    let cell_source = if header {
        format!("| {source} |\n| --- |\n\n{references}")
    } else {
        format!("| Cell |\n| --- |\n| {source} |\n\n{references}")
    };
    let events = Parser::new_ext(&cell_source, parse_options())
        .into_offset_iter()
        .collect::<Vec<_>>();
    let mut marker_text = cell_source.clone();
    for (event, _) in &events {
        match event {
            Event::Text(text) | Event::Code(text) => marker_text.push_str(text),
            Event::Start(Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. }) => {
                marker_text.push_str(dest_url)
            }
            _ => {}
        }
    }
    let mut nonce = 0;
    while marker_text.contains(&format!("KikiInline{nonce}")) {
        nonce += 1;
    }
    let mut codes = Vec::new();
    let mut rewritten = String::new();
    let mut cursor = 0;
    for (event, range) in events {
        if let Event::Code(code) = event {
            let marker = format!("KikiInline{nonce}Code{}", codes.len());
            rewritten.push_str(&cell_source[cursor..range.start]);
            rewritten.push_str(&format!("`{marker}`"));
            cursor = range.end;
            codes.push((marker, code.into_string()));
        }
    }
    rewritten.push_str(&cell_source[cursor..]);
    let text = owned(from_str_with_options(&rewritten, options));
    let line = &text.lines[if header { 1 } else { 3 }];
    // The pinned renderer emits border, padding, inline spans, padding, border.
    let mut line = Line::from(line.spans[2..line.spans.len() - 2].to_vec());
    let mut offset = 0;
    let mut protected = Vec::new();
    for span in &mut line.spans {
        if let Some((_, code)) = codes.iter().find(|(marker, _)| span.content == *marker) {
            span.content = code.clone().into();
            protected.push(offset..offset + code.len());
        }
        offset += span.content.len();
    }
    Cell { line, protected }
}

struct Placement {
    marker: String,
    carrier_width: usize,
    layout: MarkdownLayout,
}

fn render_with(
    source: &str,
    content_width: u16,
    options: &MarkdownOptions,
    mut place: impl FnMut(&Table, u16) -> Option<MarkdownLayout>,
) -> MarkdownLayout {
    let (tables, references, marker_text) = extract(source);
    let mut nonce = 0;
    while marker_text.contains(&format!("KikiTable{nonce}")) {
        nonce += 1;
    }
    let mut rewritten = String::new();
    let mut cursor = 0;
    let mut placements = Vec::new();
    for table in tables {
        let rendered = Table {
            alignments: table.alignments.clone(),
            header: table
                .header
                .iter()
                .map(|range| render_cell(&source[range.clone()], true, &references, options))
                .collect(),
            rows: table
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|range| {
                            render_cell(&source[range.clone()], false, &references, options)
                        })
                        .collect()
                })
                .collect(),
        };
        let width = usize::from(content_width).saturating_sub(table.prefix_width) as u16;
        let Some(layout) = place(&rendered, width) else {
            continue;
        };
        assert_eq!(layout.text.lines.len(), layout.breaks.len());
        let marker = format!("KikiTable{nonce}Header{}", placements.len());
        let columns = table.alignments.len();
        let header = format!("| {marker} |{}\n", " |".repeat(columns - 1));
        let delimiter = &source[table.header_end..table.range.end];
        let delimiter_end = delimiter
            .find(['\r', '\n'])
            .map_or(table.range.end, |offset| {
                table.header_end
                    + offset
                    + if delimiter[offset..].starts_with("\r\n") {
                        2
                    } else {
                        1
                    }
            });
        rewritten.push_str(&source[cursor..table.range.start]);
        rewritten.push_str(&header);
        rewritten.push_str(&source[table.header_end..delimiter_end]);
        cursor = table.range.end;
        placements.push(Placement {
            carrier_width: marker.len() + columns - 1 + 3 * columns + 1,
            marker,
            layout,
        });
    }
    if placements.is_empty() {
        return MarkdownLayout::hard(owned(from_str_with_options(source, options)));
    }
    rewritten.push_str(&source[cursor..]);
    let mut text = owned(from_str_with_options(&rewritten, options));
    let mut breaks = vec![LineBreak::Hard; text.lines.len()];
    for placement in placements.into_iter().rev() {
        let header = text
            .lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|span| span.content == placement.marker)
            })
            .expect("table carrier marker");
        let first = header - 1;
        let first_prefix = prefix(&text.lines[first], placement.carrier_width);
        let continuation = prefix(&text.lines[header], placement.carrier_width);
        let context_style = text.lines[first].style;
        let lines = placement
            .layout
            .text
            .lines
            .into_iter()
            .enumerate()
            .map(|(index, mut line)| {
                let mut spans = if index == 0 {
                    first_prefix.clone()
                } else {
                    continuation.clone()
                };
                spans.extend(line.spans);
                line.spans = spans;
                line.style = context_style
                    .patch(placement.layout.text.style)
                    .patch(line.style);
                line
            });
        text.lines.splice(first..first + 4, lines);
        breaks.splice(first..first + 4, placement.layout.breaks);
    }
    MarkdownLayout { text, breaks }
}

fn prefix(line: &Line<'static>, carrier_width: usize) -> Vec<Span<'static>> {
    let mut remaining = line.width() - carrier_width;
    let mut spans = Vec::new();
    for span in &line.spans {
        if remaining == 0 {
            break;
        }
        assert!(span.width() <= remaining, "carrier prefix span boundary");
        remaining -= span.width();
        spans.push(span.clone());
    }
    assert_eq!(remaining, 0);
    spans
}

fn owned(text: Text<'_>) -> Text<'static> {
    Text {
        style: text.style,
        alignment: text.alignment,
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                style: line.style,
                alignment: line.alignment,
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style))
                    .collect(),
            })
            .collect(),
    }
}

fn natural_columns(table: &Table) -> Vec<usize> {
    let mut widths = vec![1; table.alignments.len()];
    for row in std::iter::once(&table.header).chain(&table.rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.line.width());
        }
    }
    widths
}

fn allocate_columns(natural: &[usize], minimum: &[usize], budget: usize) -> Option<Vec<usize>> {
    if minimum.iter().sum::<usize>() > budget {
        return None;
    }
    if natural.iter().sum::<usize>() <= budget {
        return Some(natural.to_vec());
    }
    let mut low = 0;
    let mut high = budget;
    while low < high {
        let level = low + (high - low).div_ceil(2);
        let used: usize = natural
            .iter()
            .zip(minimum)
            .map(|(&cap, &floor)| cap.min(floor.max(level)))
            .sum();
        if used <= budget {
            low = level;
        } else {
            high = level - 1;
        }
    }
    let mut widths: Vec<_> = natural
        .iter()
        .zip(minimum)
        .map(|(&cap, &floor)| cap.min(floor.max(low)))
        .collect();
    let mut remaining = budget - widths.iter().sum::<usize>();
    for (width, &cap) in widths.iter_mut().zip(natural) {
        if remaining > 0 && *width <= low && *width < cap {
            *width += 1;
            remaining -= 1;
        }
    }
    Some(widths)
}

fn constrained_grid(
    table: &Table,
    width: u16,
    options: &MarkdownOptions,
) -> Option<MarkdownLayout> {
    let natural = natural_columns(table);
    if natural.is_empty() {
        return None;
    }
    let budget = usize::from(width).checked_sub(natural.len() * 3 + 1)?;
    let mut minimum = vec![1; natural.len()];
    for row in std::iter::once(&table.header).chain(&table.rows) {
        for (floor, cell) in minimum.iter_mut().zip(row) {
            *floor = (*floor).max(wrapping::minimum_width(&cell.line, &cell.protected));
        }
    }
    let widths = allocate_columns(&natural, &minimum, budget)?;
    let mut rows = Vec::new();
    for row in std::iter::once(&table.header).chain(&table.rows) {
        let mut cells = Vec::new();
        for (index, &width) in widths.iter().enumerate() {
            let empty = Cell::plain(Line::default());
            let cell = row.get(index).unwrap_or(&empty);
            let cell = wrapping::wrap_words(&cell.line, width, &cell.protected)?;
            if cell.text.lines.len() > 3 {
                return None;
            }
            cells.push(cell);
        }
        rows.push(cells);
    }
    let border_style = from_str_with_options("| |\n| - |", options).lines[0].spans[0].style;
    let border = |left: char, middle: char, right: char| {
        let mut text = left.to_string();
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                text.push(middle);
            }
            text.push_str(&"─".repeat(width + 2));
        }
        text.push(right);
        Line::styled(text, border_style)
    };
    let mut lines = vec![border('┌', '┬', '┐')];
    for (row_index, cells) in rows.iter().enumerate() {
        let height = cells
            .iter()
            .map(|cell| cell.text.lines.len())
            .max()
            .unwrap_or(1);
        for y in 0..height {
            let mut spans = vec![Span::styled("│", border_style)];
            for (column, (&width, cell)) in widths.iter().zip(cells).enumerate() {
                let empty = Line::default();
                let line = cell.text.lines.get(y).unwrap_or(&empty);
                let padding = width.saturating_sub(line.width());
                let left = match table.alignments[column] {
                    Alignment::Right => padding,
                    Alignment::Center => padding / 2,
                    _ => 0,
                };
                spans.push(Span::raw(" ".repeat(left + 1)));
                spans.extend(
                    line.spans.iter().map(|span| {
                        Span::styled(span.content.clone(), line.style.patch(span.style))
                    }),
                );
                spans.push(Span::raw(" ".repeat(padding - left + 1)));
                spans.push(Span::styled("│", border_style));
            }
            lines.push(Line::from(spans));
        }
        if row_index + 1 < rows.len() {
            lines.push(border('├', '┼', '┤'));
        }
    }
    lines.push(border('└', '┴', '┘'));
    Some(MarkdownLayout::hard(Text::from(lines)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{
        CardId, DocumentPosition, Extraction, RowBreaks, SelectableRegion, Selection, Viewport,
    };
    use crate::theme::{DARK, LIGHT};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::widgets::{Paragraph, Widget};

    const TABLE: &str = "| H |\n| --- |\n| v |";

    fn text_table(header: &[&str], rows: &[&[&str]]) -> Table {
        Table {
            alignments: vec![Alignment::None; header.len()],
            header: header
                .iter()
                .map(|value| Cell::plain(Line::raw((*value).to_owned())))
                .collect(),
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| Cell::plain(Line::raw((*value).to_owned())))
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn constrained_grid_wraps_rows_at_two_and_three_word_lines() {
        let options = super::super::markdown_options(&DARK);
        for (value, lines) in [("one two", 2), ("one two six", 3)] {
            let source = format!("| H |\n| --- |\n| {value} |");
            let layout = render(&source, 7, options);
            assert!(
                layout.text.lines.iter().all(|line| line.width() <= 7),
                "{:?}",
                layout.text
            );
            assert_eq!(layout.text.lines.len(), lines + 4);
            assert_eq!(layout.text.lines[3].to_string(), "│ one │");
            assert_eq!(layout.text.lines[4].to_string(), "│ two │");
            assert!(layout.breaks.iter().all(|br| *br == LineBreak::Hard));
        }
    }

    #[test]
    fn inline_code_identifiers_remain_whole_while_cjk_prose_can_wrap() {
        let options = super::super::markdown_options(&DARK);
        for (value, eligible) in [("東京京都大阪", true), ("`東京京都大阪`", false)] {
            let source = format!("| H |\n| --- |\n| {value} |");
            let mut checked = false;
            render_with(&source, 8, options, |table, width| {
                checked = true;
                assert_eq!(
                    constrained_grid(table, width, options).is_some(),
                    eligible,
                    "{value}"
                );
                Some(replacement())
            });
            assert!(checked);
        }
    }

    #[test]
    fn code_protection_comes_from_syntax_not_matching_text_or_styles() {
        let source =
            "| H |\n| --- |\n| 東京京都大阪 `東京京都大阪` [marker](KikiInline&#48;Code0) |";
        let options = super::super::markdown_options(&DARK);
        render_with(source, 8, options, |table, _| {
            let cell = &table.rows[0][0];
            let text = cell.line.to_string();
            let start = "東京京都大阪 ".len();
            assert_eq!(cell.protected, vec![start..start + "東京京都大阪".len()]);
            assert_eq!(text, "東京京都大阪 東京京都大阪 marker (KikiInline0Code0)");
            Some(replacement())
        });
    }

    #[test]
    fn wrapped_grids_keep_inline_styles_unicode_and_container_widths() {
        for theme in [&DARK, &LIGHT] {
            let options = super::super::markdown_options(theme);
            let source = "| Name |\n| --- |\n| **one** *two* `six` |";
            let grid = render(source, 8, options);
            assert!(grid.text.lines.iter().all(|line| line.width() <= 8));
            assert!(
                style_at(&grid.text, "one")
                    .add_modifier
                    .contains(Modifier::BOLD)
            );
            assert!(
                style_at(&grid.text, "two")
                    .add_modifier
                    .contains(Modifier::ITALIC)
            );
            assert_eq!(style_at(&grid.text, "six").fg, Some(theme.md_code));
            assert_eq!(style_at(&grid.text, "┌").fg, Some(theme.border_muted));
            for value in ["東京京都大阪", "cafe\u{301} 👩‍💻"] {
                let source = format!("| H |\n| --- |\n| {value} |");
                let grid = render(&source, 8, options);
                assert!(grid.text.lines.iter().all(|line| line.width() <= 8));
            }
            let source = "> | H |\n> | --- |\n> | one two six |";
            let grid = render(source, 9, options);
            assert!(grid.text.lines.iter().all(|line| line.width() <= 9));
            assert!(
                grid.text
                    .lines
                    .iter()
                    .all(|line| line.to_string().starts_with("> "))
            );
        }
    }

    #[test]
    fn grid_allocation_caps_short_columns_and_obeys_whole_token_minima() {
        assert_eq!(allocate_columns(&[2, 20], &[1, 4], 16), Some(vec![2, 14]));
        assert_eq!(allocate_columns(&[9, 9], &[3, 3], 9), Some(vec![5, 4]));
        assert_eq!(allocate_columns(&[9, 9], &[6, 4], 9), None);
        assert_eq!(allocate_columns(&[9, 9], &[6, 4], 10), Some(vec![6, 4]));
    }

    #[test]
    fn grid_policy_counts_content_lines_not_row_separators_or_table_height() {
        let options = super::super::markdown_options(&DARK);
        for (header, value, eligible) in [
            ("H", "one two six", true),
            ("H", "one two six ten", false),
            ("one two six", "v", true),
            ("one two six ten", "v", false),
        ] {
            let table = text_table(&[header], &[&[value]]);
            assert_eq!(
                constrained_grid(&table, 7, options).is_some(),
                eligible,
                "{header} / {value}"
            );
        }
        let table = text_table(&["H"], &[&["a"], &["b"], &["c"], &["d"], &["e"]]);
        let grid = constrained_grid(&table, 5, options).unwrap();
        assert_eq!(grid.text.lines.len(), 13);
        assert_eq!(grid.text.lines[4].to_string(), "├───┤");
    }

    #[test]
    fn grid_respects_alignment_borders_and_indivisible_tokens() {
        let options = super::super::markdown_options(&DARK);
        let mut table = text_table(&["L", "R", "C"], &[&["a bb", "a bb", "a bb"]]);
        table.alignments = vec![Alignment::Left, Alignment::Right, Alignment::Center];
        let grid = constrained_grid(&table, 19, options).unwrap();
        assert_eq!(grid.text.lines[3].to_string(), "│ a   │   a │  a  │");
        assert_eq!(grid.text.lines[4].to_string(), "│ bb  │  bb │ bb  │");
        assert!(grid.text.lines.iter().all(|line| line.width() == 19));
        let table = text_table(&["H"], &[&["long_identifier"]]);
        assert!(constrained_grid(&table, 10, options).is_none());
        assert!(constrained_grid(&table, 0, options).is_none());
    }

    fn styles(text: &Text<'_>) -> Vec<Vec<(char, Style)>> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .flat_map(|span| {
                        let style = text.style.patch(line.style).patch(span.style);
                        span.content.chars().map(move |ch| (ch, style))
                    })
                    .collect()
            })
            .collect()
    }

    fn style_at(text: &Text<'_>, needle: &str) -> Style {
        for (line, styled) in text.lines.iter().zip(styles(text)) {
            if let Some(offset) = line.to_string().find(needle) {
                return styled[line.to_string()[..offset].chars().count()].1;
            }
        }
        panic!("missing {needle:?} in {text:?}");
    }

    fn replacement() -> MarkdownLayout {
        MarkdownLayout::hard(Text::from(vec![
            Line::from(Span::styled("FIRST", Style::new().bold())),
            Line::from(Span::styled("SECOND", Style::new().italic())),
        ]))
    }

    #[test]
    fn unchanged_documents_and_fitting_tables_keep_exact_styles() {
        for theme in [&DARK, &LIGHT] {
            let options = super::super::markdown_options(theme);
            for source in [
                "# Heading\n\nA **bold** [guide][g].\n\n> quote\n\n[g]: https://example.com",
                "before\n\n| H |\n| --- |\n| `v` |\n\nafter",
                "- | H |\n  | --- |\n  | v |\n- next",
            ] {
                let actual = render(source, 120, options);
                assert_eq!(actual.text, from_str_with_options(source, options));
                assert_eq!(
                    actual.breaks,
                    vec![LineBreak::Hard; actual.text.lines.len()]
                );
            }
        }
    }

    #[test]
    fn literal_pipe_content_is_not_extracted() {
        let source = "Prose | not a table\n\n```text\n| H |\n| --- |\n| v |\n```\n\n    | H |\n    | --- |\n    | v |\n\n`a|b` and a \\| b";
        let options = super::super::markdown_options(&DARK);
        let actual = render_with(source, 1, options, |_, _| panic!("literal table extracted"));
        assert_eq!(actual.text, from_str_with_options(source, options));
    }

    #[test]
    fn extracted_cells_keep_table_inline_semantics_and_document_references() {
        let source = "before [guide][g][^n]\n\n| **Name** | *Value* |\n| :--- | ---: |\n| a\\|b | `c\\|d` [**guide**][g] ![pic](pic.png) ~~old~~[^n] |\n\nafter [guide][g]\n\n[g]: https://example.com/path \"title\"\n\n[^n]: note **body**";
        for theme in [&DARK, &LIGHT] {
            let options = super::super::markdown_options(theme);
            let mut calls = 0;
            let actual = render_with(source, 16, options, |table, width| {
                calls += 1;
                assert_eq!(width, 16);
                assert_eq!(table.alignments, vec![Alignment::Left, Alignment::Right]);
                assert_eq!(
                    table
                        .header
                        .iter()
                        .map(|cell| cell.line.to_string())
                        .collect::<Vec<_>>(),
                    ["Name", "Value"]
                );
                assert_eq!(table.rows.len(), 1);
                assert_eq!(table.rows[0][0].line.to_string(), "a|b");
                assert_eq!(
                    table.rows[0][1].line.to_string(),
                    "c|d guide (https://example.com/path) [img] pic old[n]"
                );
                let cell = Text::from(table.rows[0][1].line.clone());
                assert_eq!(style_at(&cell, "c|d").fg, Some(theme.md_code));
                assert!(
                    style_at(&cell, "guide")
                        .add_modifier
                        .contains(Modifier::BOLD | Modifier::UNDERLINED)
                );
                assert_eq!(style_at(&cell, "guide").fg, Some(theme.md_link));
                assert!(
                    style_at(&cell, "old")
                        .add_modifier
                        .contains(Modifier::CROSSED_OUT)
                );
                Some(MarkdownLayout::hard(Text::from(vec![
                    table.header[0].line.clone(),
                    table.rows[0][1].line.clone(),
                ])))
            });
            assert_eq!(calls, 1);
            let displayed = actual.text.to_string();
            assert!(displayed.contains("before guide (https://example.com/path)[n]"));
            assert!(displayed.contains("after guide (https://example.com/path)"));
            assert!(displayed.contains("[n]: note body"));
            assert!(displayed.contains("c|d guide (https://example.com/path)"));
            assert_eq!(style_at(&actual.text, "body").fg, Some(theme.dim));
        }
    }

    #[test]
    fn substitution_preserves_container_prefixes_styles_numbering_and_following_content() {
        let fixtures = [
            (format!("before\n\n{TABLE}\n\nafter"), "", "", 0),
            ("- | H |\n  | --- |\n  | v |\n- next".into(), "- ", "  ", 2),
            ("- intro\n\n  | H |\n  | --- |\n  | v |\n\n  following\n\n- next".into(), "  ", "  ", 2),
            ("9. | H |\n   | --- |\n   | v |\n10. next".into(), "9. ", "   ", 3),
            ("> | H |\n> | --- |\n> | v |\n>\n> following\n\nafter".into(), "> ", "> ", 2),
            ("> - | H |\n>   | --- |\n>   | v |\n> - next\n\nafter".into(), "> - ", ">   ", 4),
            ("- outer\n  - | H |\n    | --- |\n    | v |\n  - next\n- last".into(), "    - ", "      ", 6),
            ("> > 12. intro\n> >\n> >     | H |\n> >     | --- |\n> >     | v |\n> >\n> >     following\n> >\n> > 13. next\n\nafter".into(), ">>     ", ">>     ", 7),
        ];
        for theme in [&DARK, &LIGHT] {
            let options = super::super::markdown_options(theme);
            for (source, first_prefix, continuation, cost) in &fixtures {
                let original = from_str_with_options(source, options);
                let header = original
                    .lines
                    .iter()
                    .position(|line| line.to_string().contains('H'))
                    .unwrap();
                let first = header - 1;
                let mut expected = styles(&original);
                let prefix_chars = first_prefix.chars().count();
                assert!(
                    original.lines[first].to_string().starts_with(first_prefix),
                    "{source:?}: {original:?}"
                );
                let first_styles = expected[first][..prefix_chars].to_vec();
                let continuation_styles = expected[header][..continuation.chars().count()].to_vec();
                let quote_style = original.lines[first].style;
                let mut first_line = first_styles;
                first_line.extend(
                    "FIRST"
                        .chars()
                        .map(|ch| (ch, quote_style.patch(Style::new().bold()))),
                );
                let mut second_line = continuation_styles;
                second_line.extend(
                    "SECOND"
                        .chars()
                        .map(|ch| (ch, quote_style.patch(Style::new().italic()))),
                );
                expected.splice(first..first + 5, [first_line, second_line]);
                let mut calls = 0;
                let actual = render_with(source, 20, options, |_, width| {
                    calls += 1;
                    assert_eq!(width, 20 - cost);
                    Some(replacement())
                });
                assert_eq!(calls, 1, "{source}");
                assert_eq!(styles(&actual.text), expected, "{source}");
                assert_eq!(
                    actual.breaks,
                    vec![LineBreak::Hard; actual.text.lines.len()]
                );
            }
        }
    }

    #[test]
    fn collision_free_markers_and_multiple_substitutions_do_not_leak_into_copy() {
        let source = format!(
            "KikiTable0Header KikiTable0Body KikiTable1Header\n\n{TABLE}\n\nbetween\n\n{TABLE}\n\nafter"
        );
        let options = super::super::markdown_options(&DARK);
        let mut calls = 0;
        let actual = render_with(&source, 80, options, |_, _| {
            calls += 1;
            Some(replacement())
        });
        assert_eq!(calls, 2);
        let expected = "KikiTable0Header KikiTable0Body KikiTable1Header\n\nFIRST\nSECOND\n\nbetween\n\nFIRST\nSECOND\n\nafter";
        assert_eq!(actual.text.to_string(), expected);
        let area = Rect::new(0, 0, 80, actual.text.lines.len() as u16);
        let paragraph = Paragraph::new(actual.text.clone());
        assert_eq!(paragraph.line_count(area.width), actual.text.lines.len());
        let mut buffer = Buffer::empty(area);
        paragraph.render(area, &mut buffer);
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&actual.text, area.width),
            CardId::new(0),
        );
        let selection = Selection::new(
            DocumentPosition::new(0, 0),
            DocumentPosition::new(actual.text.lines.len(), 0),
        )
        .project(Viewport::new(area, 0))
        .unwrap();
        let mut extraction = Extraction::new();
        extraction.append(&buffer, selection, &[region]);
        assert_eq!(extraction.finish(), expected);
    }

    #[test]
    fn decoded_marker_like_text_cannot_be_mistaken_for_a_carrier() {
        let source = format!("[marker](KikiTable&#48;Header0)\n\n{TABLE}\n\nafter");
        let options = super::super::markdown_options(&DARK);
        let actual = render_with(&source, 10, options, |_, _| Some(replacement()));
        assert_eq!(
            actual.text.to_string(),
            "marker (KikiTable0Header0)\n\nFIRST\nSECOND\n\nafter"
        );
    }

    #[test]
    fn cells_resolve_reference_definitions_inside_document_containers() {
        let source = "| H |\n| --- |\n| [guide][g] |\n\n> [g]:\n>   <https://example.com/a?x=1&y=2>\n>   \"a multiline\n>   title\"\n\nafter [guide][g]";
        let options = super::super::markdown_options(&DARK);
        let mut calls = 0;
        let actual = render_with(source, 10, options, |table, _| {
            calls += 1;
            assert_eq!(
                table.rows[0][0].line.to_string(),
                "guide (https://example.com/a?x=1&y=2)"
            );
            Some(MarkdownLayout::hard(Text::from(
                table.rows[0][0].line.clone(),
            )))
        });
        assert_eq!(calls, 1);
        assert!(
            actual
                .text
                .to_string()
                .contains("after guide (https://example.com/a?x=1&y=2)")
        );
    }

    #[test]
    fn unselected_tables_remain_on_the_original_full_document_path() {
        let source = format!(
            "{TABLE}\n\nbetween [guide][g]\n\n{TABLE}\n\nafter\n\n[g]: https://example.com"
        );
        let options = super::super::markdown_options(&DARK);
        let original = from_str_with_options(&source, options);
        let mut calls = 0;
        let actual = render_with(&source, 20, options, |_, _| {
            calls += 1;
            if calls == 1 {
                Some(replacement())
            } else {
                None
            }
        });
        assert_eq!(calls, 2);
        let mut expected = styles(&original);
        expected.splice(0..5, styles(&replacement().text));
        assert_eq!(styles(&actual.text), expected);
    }

    #[test]
    fn substitution_preserves_content_with_all_markdown_line_endings() {
        let options = super::super::markdown_options(&DARK);
        for ending in ["\n", "\r", "\r\n"] {
            let source = [
                "before", "", "| H |", "| --- |", "| a |", "| b |", "", "after",
            ]
            .join(ending);
            let actual = render_with(&source, 10, options, |_, _| Some(replacement()));
            assert_eq!(
                actual.text.to_string(),
                "before\n\nFIRST\nSECOND\n\nafter",
                "{ending:?}"
            );
        }
    }

    #[test]
    fn completed_lines_must_bypass_the_outer_word_wrapper() {
        use ratatui::widgets::Wrap;
        let options = super::super::markdown_options(&DARK);
        let actual = render_with(TABLE, 4, options, |_, _| {
            Some(MarkdownLayout::hard(Text::from("wholeword")))
        });
        assert_eq!(actual.text.to_string(), "wholeword");
        let paragraph = Paragraph::new(actual.text.clone());
        assert_eq!(paragraph.line_count(4), 1);
        assert!(paragraph.wrap(Wrap { trim: false }).line_count(4) > 1);
    }

    #[test]
    fn prepared_lines_retain_break_provenance_without_paragraph_rewrapping() {
        let options = super::super::markdown_options(&DARK);
        let actual = render_with(TABLE, 8, options, |_, _| {
            Some(MarkdownLayout {
                text: Text::from("field:\nword\ntoken"),
                breaks: vec![LineBreak::Hard, LineBreak::Word, LineBreak::Token],
            })
        });
        assert_eq!(actual.text.to_string(), "field:\nword\ntoken");
        assert_eq!(
            actual.breaks,
            [LineBreak::Hard, LineBreak::Word, LineBreak::Token]
        );
        let paragraph = Paragraph::new(actual.text.clone());
        assert_eq!(paragraph.line_count(8), actual.text.lines.len());
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, 3));
        paragraph.render(buffer.area, &mut buffer);
        assert_eq!(
            buffer
                .content
                .chunks(8)
                .map(|row| row
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned())
                .collect::<Vec<_>>(),
            ["field:", "word", "token"]
        );
    }
}
