use std::collections::VecDeque;
use std::ops::Range;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Text;
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct DocumentPosition {
    row: usize,
    column: u16,
}

impl DocumentPosition {
    pub(crate) const fn new(row: usize, column: u16) -> Self {
        Self { row, column }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Selection {
    anchor: DocumentPosition,
    focus: DocumentPosition,
}

impl Selection {
    pub(crate) const fn new(anchor: DocumentPosition, focus: DocumentPosition) -> Self {
        Self { anchor, focus }
    }

    pub(crate) fn from_inclusive_cells(
        anchor: DocumentPosition,
        focus: DocumentPosition,
    ) -> Option<Self> {
        if anchor == focus {
            return None;
        }
        if anchor < focus {
            Some(Self::new(
                anchor,
                DocumentPosition::new(focus.row, focus.column.saturating_add(1)),
            ))
        } else {
            Some(Self::new(
                DocumentPosition::new(anchor.row, anchor.column.saturating_add(1)),
                focus,
            ))
        }
    }

    pub(crate) fn project(self, viewport: Viewport) -> Option<ProjectedSelection> {
        if self.anchor == self.focus || viewport.area.is_empty() {
            return None;
        }
        let (start, end) = self.normalised();
        let selection = ProjectedSelection {
            start,
            end,
            viewport,
        };
        (viewport.document_top..viewport.document_bottom())
            .any(|row| {
                selection
                    .columns_on(row, viewport.area.x..viewport.area.right())
                    .is_some()
            })
            .then_some(selection)
    }

    fn normalised(self) -> (DocumentPosition, DocumentPosition) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Viewport {
    area: Rect,
    document_top: usize,
}

impl Viewport {
    pub(crate) const fn new(area: Rect, document_top: usize) -> Self {
        Self { area, document_top }
    }

    fn document_bottom(self) -> usize {
        self.document_top
            .saturating_add(usize::from(self.area.height))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProjectedSelection {
    start: DocumentPosition,
    end: DocumentPosition,
    viewport: Viewport,
}

impl ProjectedSelection {
    fn columns_on(self, row: usize, columns: Range<u16>) -> Option<Range<u16>> {
        if row < self.start.row
            || row > self.end.row
            || row < self.viewport.document_top
            || row >= self.viewport.document_bottom()
        {
            return None;
        }
        let start = if row == self.start.row {
            self.start.column.max(columns.start)
        } else {
            columns.start
        }
        .max(self.viewport.area.x);
        let end = if row == self.end.row {
            self.end.column.min(columns.end)
        } else {
            columns.end
        }
        .min(self.viewport.area.right());
        (start < end).then_some(start..end)
    }

    fn document_row(self, y: u16) -> usize {
        self.viewport
            .document_top
            .saturating_add(usize::from(y.saturating_sub(self.viewport.area.y)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CardId(usize);

impl CardId {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RowBreak {
    Hard,
    Soft,
    SoftWord,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RowBreaks {
    start: usize,
    breaks: Vec<RowBreak>,
}

impl RowBreaks {
    pub(crate) fn single(break_before: RowBreak) -> Self {
        Self {
            start: 0,
            breaks: vec![break_before],
        }
    }

    #[cfg(test)]
    pub(crate) fn from_text(text: &Text<'_>, width: u16) -> Self {
        let mut breaks = Vec::new();
        for line in &text.lines {
            let height = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(width);
            if height == 0 {
                continue;
            }
            let start = breaks.len();
            breaks.push(RowBreak::Hard);
            visit_soft_breaks(line, width, |break_before| breaks.push(break_before));
            breaks.truncate(start.saturating_add(height));
            breaks.resize(start.saturating_add(height), RowBreak::Soft);
        }
        Self { start: 0, breaks }
    }

    pub(crate) fn for_rows(text: &Text<'_>, width: u16, start: usize, height: usize) -> Self {
        let end = start.saturating_add(height);
        let mut breaks = vec![RowBreak::Soft; height];
        let mut line_top = 0_usize;
        for line in &text.lines {
            let line_height = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(width);
            let line_bottom = line_top.saturating_add(line_height);
            if line_top < end && start < line_bottom {
                if start <= line_top && line_top < end {
                    breaks[line_top.saturating_sub(start)] = RowBreak::Hard;
                }
                let mut row = 1_usize;
                visit_soft_breaks(line, width, |break_before| {
                    if row < line_height {
                        let row_in_text = line_top.saturating_add(row);
                        if start <= row_in_text && row_in_text < end {
                            breaks[row_in_text.saturating_sub(start)] = break_before;
                        }
                    }
                    row = row.saturating_add(1);
                });
            }
            line_top = line_bottom;
        }
        Self { start, breaks }
    }

    fn before(&self, row: usize) -> RowBreak {
        row.checked_sub(self.start)
            .and_then(|row| self.breaks.get(row))
            .copied()
            .unwrap_or(RowBreak::Hard)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SelectableRegion {
    area: Rect,
    document_top: usize,
    breaks: RowBreaks,
    card: CardId,
}

impl SelectableRegion {
    pub(crate) fn new(area: Rect, document_top: usize, breaks: RowBreaks, card: CardId) -> Self {
        Self {
            area,
            document_top,
            breaks,
            card,
        }
    }

    pub(crate) fn rebase(
        &self,
        viewport: Rect,
        scroll_top: usize,
        block_top: usize,
        card: CardId,
    ) -> Option<Self> {
        let region_top = block_top.saturating_add(usize::from(self.area.y));
        let region_bottom = region_top.saturating_add(usize::from(self.area.height));
        let top = region_top.max(scroll_top);
        let bottom = region_bottom.min(scroll_top.saturating_add(usize::from(viewport.height)));
        (top < bottom).then(|| Self {
            area: Rect::new(
                viewport.x.saturating_add(self.area.x),
                viewport.y.saturating_add(
                    u16::try_from(top.saturating_sub(scroll_top)).unwrap_or(u16::MAX),
                ),
                self.area.width,
                u16::try_from(bottom.saturating_sub(top)).unwrap_or(u16::MAX),
            ),
            document_top: block_top.saturating_add(self.document_top),
            breaks: self.breaks.clone(),
            card,
        })
    }
}

pub(crate) fn highlight(
    buffer: &mut Buffer,
    selection: ProjectedSelection,
    regions: &[SelectableRegion],
) {
    for region in regions {
        let area = region
            .area
            .intersection(buffer.area)
            .intersection(selection.viewport.area);
        for y in area.top()..area.bottom() {
            let row = selection.document_row(y);
            let Some(columns) = selection.columns_on(row, region.area.x..region.area.right())
            else {
                continue;
            };
            let content_right = content_right(buffer, area, y);
            let mut x = area.x;
            while x < area.right() {
                let width = buffer[(x, y)].cell_width().max(1);
                let end = x.saturating_add(width).min(area.right());
                if x < content_right && x < columns.end && columns.start < end {
                    for column in x..end {
                        let cell = &mut buffer[(column, y)];
                        cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                    }
                }
                x = end;
            }
        }
    }
}

pub(crate) struct Extraction {
    text: String,
    previous: Option<(CardId, usize)>,
}

impl Extraction {
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            previous: None,
        }
    }

    pub(crate) fn append(
        &mut self,
        buffer: &Buffer,
        selection: ProjectedSelection,
        regions: &[SelectableRegion],
    ) {
        let mut regions = regions.iter().collect::<Vec<_>>();
        regions.sort_by_key(|region| (region.document_top, region.area.y, region.area.x));

        for region in regions {
            let area = region
                .area
                .intersection(buffer.area)
                .intersection(selection.viewport.area);
            for y in area.top()..area.bottom() {
                let row = selection.document_row(y);
                let Some(columns) = selection.columns_on(row, region.area.x..region.area.right())
                else {
                    continue;
                };
                let row_text = extract_row(buffer, area, y, columns);
                let break_before = region
                    .breaks
                    .before(row.saturating_sub(region.document_top));
                let blank_line = row_text.is_empty()
                    && break_before == RowBreak::Hard
                    && row_is_blank(buffer, area, y);
                if row_text.is_empty() && !blank_line {
                    continue;
                }
                if let Some((card, previous_row)) = self.previous {
                    if card != region.card {
                        self.text.push_str("\n\n");
                    } else if previous_row != row {
                        match break_before {
                            RowBreak::Hard => self.text.push('\n'),
                            RowBreak::Soft => {}
                            RowBreak::SoftWord => self.text.push(' '),
                        }
                    }
                }
                self.text.push_str(&row_text);
                self.previous = Some((region.card, row));
            }
        }
    }

    pub(crate) fn finish(self) -> String {
        self.text
    }
}

#[cfg(test)]
pub(crate) fn extract(
    buffer: &Buffer,
    selection: ProjectedSelection,
    regions: &[SelectableRegion],
) -> String {
    let mut extraction = Extraction::new();
    extraction.append(buffer, selection, regions);
    extraction.finish()
}

fn visit_soft_breaks(line: &ratatui::text::Line<'_>, width: u16, mut visit: impl FnMut(RowBreak)) {
    let mut line_width = 0_u16;
    let mut word_width = 0_u16;
    let mut whitespace_width = 0_u16;
    let mut pending_line_empty = true;
    let mut pending_whitespace = VecDeque::new();
    let mut non_whitespace_previous = false;

    for grapheme in line.styled_graphemes(Style::default()) {
        let is_whitespace = grapheme.is_whitespace();
        let symbol_width = grapheme.symbol.cell_width();
        if symbol_width > width {
            continue;
        }

        let word_found = non_whitespace_previous && is_whitespace;
        let untrimmed_overflow = pending_line_empty
            && word_width
                .saturating_add(whitespace_width)
                .saturating_add(symbol_width)
                > width;
        if word_found || untrimmed_overflow {
            line_width = line_width.saturating_add(whitespace_width);
            line_width = line_width.saturating_add(word_width);
            whitespace_width = 0;
            word_width = 0;
            pending_whitespace.clear();
            pending_line_empty = false;
        }

        let line_full = line_width >= width;
        let pending_word_overflow = symbol_width > 0
            && line_width
                .saturating_add(whitespace_width)
                .saturating_add(word_width)
                >= width;
        if line_full || pending_word_overflow {
            visit(if is_whitespace || !pending_whitespace.is_empty() {
                RowBreak::SoftWord
            } else {
                RowBreak::Soft
            });
            let mut remaining_width = width.saturating_sub(line_width);
            while let Some(next) = pending_whitespace.front().copied() {
                if next > remaining_width {
                    break;
                }
                whitespace_width = whitespace_width.saturating_sub(next);
                remaining_width = remaining_width.saturating_sub(next);
                pending_whitespace.pop_front();
            }
            line_width = 0;
            pending_line_empty = true;
            if is_whitespace && pending_whitespace.is_empty() {
                continue;
            }
        }

        if is_whitespace {
            whitespace_width = whitespace_width.saturating_add(symbol_width);
            pending_whitespace.push_back(symbol_width);
        } else {
            word_width = word_width.saturating_add(symbol_width);
        }
        non_whitespace_previous = !is_whitespace;
    }
}

fn extract_row(buffer: &Buffer, area: Rect, y: u16, columns: Range<u16>) -> String {
    let mut text = String::new();
    let mut x = area.x;
    while x < area.right() {
        let cell = &buffer[(x, y)];
        let width = cell.cell_width().max(1);
        let end = x.saturating_add(width).min(area.right());
        if x < columns.end && columns.start < end {
            text.push_str(cell.symbol());
        }
        x = end;
    }
    // Non-breaking spaces are source content, including on emergency token wraps.
    text.truncate(
        text.trim_end_matches(|ch: char| {
            ch.is_whitespace() && !matches!(ch, '\u{00a0}' | '\u{2007}' | '\u{202f}')
        })
        .len(),
    );
    text
}

fn content_right(buffer: &Buffer, area: Rect, y: u16) -> u16 {
    let mut x = area.x;
    let mut right = area.x;
    while x < area.right() {
        let cell = &buffer[(x, y)];
        let end = x.saturating_add(cell.cell_width().max(1)).min(area.right());
        if cell.symbol() != " " {
            right = end;
        }
        x = end;
    }
    right
}

fn row_is_blank(buffer: &Buffer, area: Rect, y: u16) -> bool {
    content_right(buffer, area, y) == area.x
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Text;
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    use super::{
        CardId, DocumentPosition, Extraction, RowBreaks, SelectableRegion, Selection, Viewport,
        extract, highlight,
    };

    fn position(row: usize, column: u16) -> DocumentPosition {
        DocumentPosition::new(row, column)
    }

    fn rendered(text: &Text<'_>, area: Rect) -> Buffer {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 8));
        Paragraph::new(text.clone())
            .style(Style::new().fg(Color::Cyan))
            .wrap(Wrap { trim: false })
            .render(area, &mut buffer);
        buffer
    }

    #[test]
    fn converts_inclusive_mouse_cells_to_half_open_ranges() {
        let anchor = position(3, 4);
        let focus = position(5, 6);

        assert_eq!(
            Selection::from_inclusive_cells(anchor, focus),
            Some(Selection::new(anchor, position(5, 7)))
        );
        assert_eq!(
            Selection::from_inclusive_cells(focus, anchor),
            Some(Selection::new(position(5, 7), anchor))
        );
        assert_eq!(Selection::from_inclusive_cells(anchor, anchor), None);
    }

    #[test]
    fn projects_forward_and_backward_through_a_scrolled_viewport() {
        let area = Rect::new(4, 3, 5, 3);
        let text = Text::raw("alpha\nbravo\ncharlie");
        let viewport = Viewport::new(area, 11);
        let region = SelectableRegion::new(
            area,
            11,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );

        for selection in [
            Selection::new(position(10, 5), position(13, 7)),
            Selection::new(position(13, 7), position(10, 5)),
        ] {
            let selection = selection.project(viewport).expect("selection is visible");
            let mut buffer = rendered(&text, area);

            highlight(&mut buffer, selection, std::slice::from_ref(&region));

            assert_eq!(
                extract(&buffer, selection, std::slice::from_ref(&region)),
                "alpha\nbravo\ncha"
            );
            for (row, end) in [(3, 9), (4, 9), (5, 7)] {
                for column in area.x..end {
                    let cell = &buffer[(column, row)];
                    assert!(cell.modifier.contains(Modifier::REVERSED));
                    assert_eq!(cell.fg, Color::Cyan);
                }
            }
        }
    }

    #[test]
    fn preserves_hard_breaks_but_removes_forced_soft_wraps() {
        let area = Rect::new(0, 0, 5, 3);
        let text = Text::raw("helloworld\nagain");
        let viewport = Viewport::new(area, 0);
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(0, 0), position(3, 0))
            .project(viewport)
            .expect("selection is visible");
        let buffer = rendered(&text, area);

        assert_eq!(extract(&buffer, selection, &[region]), "helloworld\nagain");
    }

    #[test]
    fn restores_a_word_break_across_bounded_chunks() {
        let area = Rect::new(0, 0, 5, 1);
        let text = Text::raw("hello world");
        let selection = Selection::new(position(0, 0), position(2, 0));
        let mut extraction = Extraction::new();
        let mut first = Buffer::empty(area);
        Paragraph::new(text.clone())
            .wrap(Wrap { trim: false })
            .render(area, &mut first);
        let first_region = SelectableRegion::new(
            area,
            0,
            RowBreaks::for_rows(&text, area.width, 0, 1),
            CardId::new(0),
        );
        extraction.append(
            &first,
            selection
                .project(Viewport::new(area, 0))
                .expect("first chunk is visible"),
            &[first_region],
        );

        let mut second = Buffer::empty(area);
        Paragraph::new(text.clone())
            .wrap(Wrap { trim: false })
            .scroll((1, 0))
            .render(area, &mut second);
        let second_region = SelectableRegion::new(
            area,
            0,
            RowBreaks::for_rows(&text, area.width, 1, 1),
            CardId::new(0),
        );
        extraction.append(
            &second,
            selection
                .project(Viewport::new(area, 1))
                .expect("second chunk is visible"),
            &[second_region],
        );

        assert_eq!(extraction.finish(), "hello world");
    }

    #[test]
    fn restores_a_space_at_a_word_boundary_soft_wrap() {
        let area = Rect::new(0, 0, 5, 2);
        let text = Text::raw("hello world");
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(0, 0), position(2, 0))
            .project(Viewport::new(area, 0))
            .expect("selection is visible");
        let buffer = rendered(&text, area);

        assert_eq!(extract(&buffer, selection, &[region]), "hello world");
    }

    #[test]
    fn clips_a_projected_selection_above_and_below_its_viewport() {
        let viewport_area = Rect::new(1, 1, 5, 2);
        let region_area = Rect::new(1, 0, 5, 5);
        let text = Text::raw("above\nfirst\ntwo\nbelow\nlast");
        let region = SelectableRegion::new(
            region_area,
            10,
            RowBreaks::from_text(&text, region_area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(10, 2), position(14, 4))
            .project(Viewport::new(viewport_area, 11))
            .expect("selection overlaps the viewport");
        let mut buffer = rendered(&text, region_area);

        highlight(&mut buffer, selection, std::slice::from_ref(&region));

        assert_eq!(extract(&buffer, selection, &[region]), "first\ntwo");
        assert!(!buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(1, 3)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn extracts_partial_first_and_last_rows_without_terminal_fill() {
        let area = Rect::new(0, 0, 8, 3);
        let text = Text::raw("alpha\nbravo\ncharlie");
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(0, 1), position(2, 3))
            .project(Viewport::new(area, 0))
            .expect("selection is visible");
        let buffer = rendered(&text, area);

        assert_eq!(extract(&buffer, selection, &[region]), "lpha\nbravo\ncha");
    }

    #[test]
    fn distinguishes_empty_and_one_cell_selections() {
        let area = Rect::new(0, 0, 5, 1);
        let text = Text::raw("hello");
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );

        assert!(
            Selection::new(position(0, 2), position(0, 2))
                .project(Viewport::new(area, 0))
                .is_none()
        );

        let selection = Selection::new(position(0, 2), position(0, 3))
            .project(Viewport::new(area, 0))
            .expect("one cell is visible");
        let buffer = rendered(&text, area);
        assert_eq!(extract(&buffer, selection, &[region]), "l");
    }

    #[test]
    fn highlights_content_without_touching_card_decoration() {
        let viewport_area = Rect::new(0, 0, 9, 3);
        let content_area = Rect::new(2, 1, 5, 1);
        let text = Text::raw("hello");
        let region = SelectableRegion::new(
            content_area,
            1,
            RowBreaks::from_text(&text, content_area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(1, 0), position(2, 0))
            .project(Viewport::new(viewport_area, 0))
            .expect("selection is visible");
        let mut buffer = rendered(&text, content_area);
        let frame_style = Style::new().fg(Color::Red);
        buffer.set_string(0, 0, "╭───────╮", frame_style);
        buffer.set_string(0, 1, "│", frame_style);
        buffer.set_string(8, 1, "│", frame_style);
        buffer.set_string(0, 2, "╰───────╯", frame_style);

        highlight(&mut buffer, selection, std::slice::from_ref(&region));

        assert_eq!(extract(&buffer, selection, &[region]), "hello");
        for (column, row) in [(0, 0), (0, 1), (1, 1), (7, 1), (8, 1), (0, 2)] {
            assert!(!buffer[(column, row)].modifier.contains(Modifier::REVERSED));
        }
        for column in content_area.x..content_area.right() {
            assert!(
                buffer[(column, content_area.y)]
                    .modifier
                    .contains(Modifier::REVERSED)
            );
            assert_eq!(buffer[(column, content_area.y)].fg, Color::Cyan);
        }
    }

    #[test]
    fn expands_endpoints_to_complete_wide_and_combining_graphemes() {
        let area = Rect::new(0, 0, 7, 1);
        let text = Text::raw("a你🦀e\u{301}b");
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );
        let buffer = rendered(&text, area);

        let wide = Selection::new(position(0, 2), position(0, 4))
            .project(Viewport::new(area, 0))
            .expect("continuation cells are visible");
        assert_eq!(
            extract(&buffer, wide, std::slice::from_ref(&region)),
            "你🦀"
        );

        let combining = Selection::new(position(0, 5), position(0, 6))
            .project(Viewport::new(area, 0))
            .expect("combining grapheme is visible");
        assert_eq!(extract(&buffer, combining, &[region]), "e\u{301}");
    }

    #[test]
    fn joins_disjoint_regions_and_separates_card_identities() {
        let area = Rect::new(0, 0, 6, 5);
        let first = Text::raw("first");
        let middle = Text::raw("middle");
        let last = Text::raw("last");
        let mut buffer = Buffer::empty(area);
        Paragraph::new(first.clone())
            .wrap(Wrap { trim: false })
            .render(Rect::new(0, 0, 6, 1), &mut buffer);
        Paragraph::new(middle.clone())
            .wrap(Wrap { trim: false })
            .render(Rect::new(0, 2, 6, 1), &mut buffer);
        Paragraph::new(last.clone())
            .wrap(Wrap { trim: false })
            .render(Rect::new(0, 4, 6, 1), &mut buffer);
        let selection = Selection::new(position(0, 0), position(5, 0))
            .project(Viewport::new(area, 0))
            .expect("selection is visible");
        let regions = [
            SelectableRegion::new(
                Rect::new(0, 4, 6, 1),
                4,
                RowBreaks::from_text(&last, 6),
                CardId::new(1),
            ),
            SelectableRegion::new(
                Rect::new(0, 0, 6, 1),
                0,
                RowBreaks::from_text(&first, 6),
                CardId::new(0),
            ),
            SelectableRegion::new(
                Rect::new(0, 2, 6, 1),
                2,
                RowBreaks::from_text(&middle, 6),
                CardId::new(0),
            ),
        ];

        assert_eq!(
            extract(&buffer, selection, &regions),
            "first\nmiddle\n\nlast"
        );
    }

    #[test]
    fn does_not_highlight_trailing_terminal_fill() {
        let area = Rect::new(0, 0, 5, 1);
        let text = Text::raw("two");
        let region = SelectableRegion::new(
            area,
            0,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(0, 0), position(1, 0))
            .project(Viewport::new(area, 0))
            .expect("selection is visible");
        let mut buffer = rendered(&text, area);

        highlight(&mut buffer, selection, &[region]);

        for column in 0..3 {
            assert!(buffer[(column, 0)].modifier.contains(Modifier::REVERSED));
        }
        for column in 3..5 {
            assert!(!buffer[(column, 0)].modifier.contains(Modifier::REVERSED));
        }
    }

    #[test]
    fn uses_logical_row_breaks_for_a_clipped_region() {
        let area = Rect::new(0, 0, 5, 2);
        let text = Text::raw("helloworld\nagain");
        let region = SelectableRegion::new(
            area,
            10,
            RowBreaks::from_text(&text, area.width),
            CardId::new(0),
        );
        let selection = Selection::new(position(11, 0), position(13, 0))
            .project(Viewport::new(area, 11))
            .expect("selection is visible");
        let mut buffer = Buffer::empty(area);
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((1, 0))
            .render(area, &mut buffer);

        assert_eq!(extract(&buffer, selection, &[region]), "world\nagain");
    }
}
