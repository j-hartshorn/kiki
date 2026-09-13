use std::ops::Range;

use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) use crate::config::constants::MAX_PROMPT_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptEdit {
    Insert(char),
    InsertNewline,
    Left,
    Right,
    WordLeft,
    WordRight,
    Up,
    Down,
    PromptStart,
    PromptEnd,
    LineStart,
    LineEnd,
    DeletePreviousGrapheme,
    DeleteNextGrapheme,
    DeletePreviousWord,
    DeleteNextWord,
    DeleteToLineStart,
    DeleteToLineEnd,
}

#[derive(Default)]
pub(crate) struct PromptEditor {
    text: String,
    caret: usize,
    preferred_column: Option<usize>,
    viewport_top: usize,
}

pub(crate) struct PromptLayout {
    rows: Vec<PromptRow>,
    cursor: Option<(usize, usize)>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PromptRow {
    text: String,
    source: Range<usize>,
}

struct VisualRow {
    start: usize,
    end: usize,
    width: usize,
}

impl PromptEditor {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn caret_at_end(&self) -> bool {
        self.caret == self.text.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn take(&mut self) -> String {
        self.caret = 0;
        self.preferred_column = None;
        self.viewport_top = 0;
        std::mem::take(&mut self.text)
    }

    pub(crate) fn apply(&mut self, edit: PromptEdit, width: u16) -> bool {
        match edit {
            PromptEdit::Insert(character) if !character.is_control() => self.insert(character),
            PromptEdit::InsertNewline => self.insert('\n'),
            PromptEdit::Left => self.move_left(),
            PromptEdit::Right => self.move_right(),
            PromptEdit::WordLeft => self.move_word_left(),
            PromptEdit::WordRight => self.move_word_right(),
            PromptEdit::Up => self.move_vertical(width, true),
            PromptEdit::Down => self.move_vertical(width, false),
            PromptEdit::PromptStart => self.move_to(0),
            PromptEdit::PromptEnd => self.move_to(self.text.len()),
            PromptEdit::LineStart => self.move_to(self.line_start()),
            PromptEdit::LineEnd => self.move_to(self.line_end()),
            PromptEdit::DeletePreviousGrapheme => self.delete_previous_grapheme(),
            PromptEdit::DeleteNextGrapheme => self.delete_next_grapheme(),
            PromptEdit::DeletePreviousWord => self.delete_previous_word(),
            PromptEdit::DeleteNextWord => self.delete_next_word(),
            PromptEdit::DeleteToLineStart => self.delete_to_line_start(),
            PromptEdit::DeleteToLineEnd => self.delete_to_line_end(),
            PromptEdit::Insert(_) => false,
        }
    }

    pub(crate) fn layout(&mut self, width: u16, height: usize) -> PromptLayout {
        let rows = visual_rows(&self.text, usize::from(width));
        let cursor = cursor_location(&self.text, &rows, self.caret, usize::from(width));
        if height == 0 {
            return PromptLayout {
                rows: Vec::new(),
                cursor: None,
            };
        }

        let total_rows = rows.len();
        let caret_row = cursor.map_or(0, |(row, _)| row);
        let max_top = total_rows.saturating_sub(height);
        self.viewport_top = self.viewport_top.min(max_top);
        if caret_row < self.viewport_top {
            self.viewport_top = caret_row;
        } else if caret_row >= self.viewport_top.saturating_add(height) {
            self.viewport_top = caret_row.saturating_add(1).saturating_sub(height);
        }

        let bottom = self.viewport_top.saturating_add(height).min(total_rows);
        PromptLayout {
            rows: rows[self.viewport_top..bottom]
                .iter()
                .map(|row| PromptRow {
                    text: self.text[row.start..row.end].to_owned(),
                    source: row.start..row.end,
                })
                .collect(),
            cursor: cursor.and_then(|(row, column)| {
                (row >= self.viewport_top && row < bottom && column < usize::from(width))
                    .then_some((row - self.viewport_top, column))
            }),
        }
    }

    pub(crate) fn insert_paste(&mut self, text: &str) -> bool {
        let mut end = text
            .len()
            .min(MAX_PROMPT_BYTES.saturating_sub(self.text.len()));
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == 0 {
            return false;
        }
        self.text.insert_str(self.caret, &text[..end]);
        self.caret += end;
        self.preferred_column = None;
        true
    }

    fn insert(&mut self, character: char) -> bool {
        if self.text.len().saturating_add(character.len_utf8()) > MAX_PROMPT_BYTES {
            return false;
        }
        self.text.insert(self.caret, character);
        self.caret += character.len_utf8();
        self.preferred_column = None;
        true
    }

    fn move_left(&mut self) -> bool {
        let Some((start, _)) = self.text[..self.caret].grapheme_indices(true).next_back() else {
            return false;
        };
        self.move_to(start)
    }

    fn move_right(&mut self) -> bool {
        let Some(grapheme) = self.text[self.caret..].graphemes(true).next() else {
            return false;
        };
        self.move_to(self.caret + grapheme.len())
    }

    fn move_word_left(&mut self) -> bool {
        let start = self
            .text
            .split_word_bound_indices()
            .take_while(|(index, _)| *index < self.caret)
            .filter(|(_, word)| !word.chars().all(char::is_whitespace))
            .map(|(index, _)| index)
            .last()
            .unwrap_or(0);
        self.move_to(start)
    }

    fn move_word_right(&mut self) -> bool {
        self.move_to(self.next_word_end())
    }

    fn next_word_end(&self) -> usize {
        self.text
            .split_word_bound_indices()
            .filter(|(_, word)| !word.chars().all(char::is_whitespace))
            .find_map(|(index, word)| {
                let end = index + word.len();
                (end > self.caret).then_some(end)
            })
            .unwrap_or(self.text.len())
    }

    fn move_vertical(&mut self, width: u16, up: bool) -> bool {
        let rows = visual_rows(&self.text, usize::from(width));
        let Some((row, column)) =
            cursor_location(&self.text, &rows, self.caret, usize::from(width))
        else {
            return false;
        };
        let Some(target_row) = (if up {
            row.checked_sub(1)
        } else {
            (row + 1 < rows.len()).then_some(row + 1)
        }) else {
            return false;
        };
        let preferred_column = self.preferred_column.unwrap_or(column);
        self.caret = byte_at_column(&self.text, &rows[target_row], preferred_column);
        self.preferred_column = Some(preferred_column);
        true
    }

    fn move_to(&mut self, caret: usize) -> bool {
        if self.caret == caret {
            return false;
        }
        self.caret = caret;
        self.preferred_column = None;
        true
    }

    fn delete_previous_grapheme(&mut self) -> bool {
        let Some((start, _)) = self.text[..self.caret].grapheme_indices(true).next_back() else {
            return false;
        };
        self.delete(start..self.caret)
    }

    fn delete_next_grapheme(&mut self) -> bool {
        let Some(grapheme) = self.text[self.caret..].graphemes(true).next() else {
            return false;
        };
        self.delete(self.caret..self.caret + grapheme.len())
    }

    fn delete_previous_word(&mut self) -> bool {
        let mut start = self.caret;
        while let Some((index, grapheme)) = self.text[..start].grapheme_indices(true).next_back() {
            if !grapheme.chars().all(char::is_whitespace) {
                break;
            }
            start = index;
        }
        let word_start = self.text[..start]
            .split_word_bound_indices()
            .next_back()
            .map_or(start, |(index, _)| index);
        self.delete(word_start..self.caret)
    }

    fn delete_next_word(&mut self) -> bool {
        self.delete(self.caret..self.next_word_end())
    }

    fn delete_to_line_start(&mut self) -> bool {
        self.delete(self.line_start()..self.caret)
    }

    fn delete_to_line_end(&mut self) -> bool {
        self.delete(self.caret..self.line_end())
    }

    fn delete(&mut self, range: std::ops::Range<usize>) -> bool {
        if range.is_empty() {
            return false;
        }
        let caret = range.start;
        self.text.replace_range(range, "");
        self.caret = caret;
        self.preferred_column = None;
        true
    }

    fn line_start(&self) -> usize {
        self.text[..self.caret]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.caret..]
            .find('\n')
            .map_or(self.text.len(), |index| self.caret + index)
    }
}

impl PromptLayout {
    pub(crate) fn rows(&self) -> &[PromptRow] {
        &self.rows
    }

    pub(crate) fn cursor(&self) -> Option<(usize, usize)> {
        self.cursor
    }
}

impl PromptRow {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn source(&self) -> Range<usize> {
        self.source.clone()
    }
}

fn visual_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let mut rows = Vec::new();
    let mut line_start = 0;
    loop {
        let line_end = text[line_start..]
            .find('\n')
            .map_or(text.len(), |index| line_start + index);
        append_line_rows(&mut rows, text, line_start, line_end, width);
        if line_end == text.len() {
            break;
        }
        line_start = line_end + 1;
    }
    if width > 0 && rows.last().is_some_and(|row| row.width == width) {
        rows.push(VisualRow {
            start: text.len(),
            end: text.len(),
            width: 0,
        });
    }
    rows
}

fn append_line_rows(rows: &mut Vec<VisualRow>, text: &str, start: usize, end: usize, width: usize) {
    if start == end || width == 0 {
        rows.push(VisualRow {
            start,
            end,
            width: 0,
        });
        return;
    }

    let mut row_start = start;
    let mut row_width = 0_usize;
    for (offset, grapheme) in text[start..end].grapheme_indices(true) {
        let grapheme_start = start + offset;
        let grapheme_width = Line::raw(grapheme).width();
        if grapheme_start > row_start && row_width.saturating_add(grapheme_width) > width {
            rows.push(VisualRow {
                start: row_start,
                end: grapheme_start,
                width: row_width,
            });
            row_start = grapheme_start;
            row_width = 0;
        }
        row_width = row_width.saturating_add(grapheme_width);
    }
    rows.push(VisualRow {
        start: row_start,
        end,
        width: row_width,
    });
}

fn cursor_location(
    text: &str,
    rows: &[VisualRow],
    caret: usize,
    width: usize,
) -> Option<(usize, usize)> {
    if let Some((row, _)) = rows
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, row)| row.start == caret)
    {
        return Some((row, 0));
    }
    if rows
        .iter()
        .any(|row| row.end == caret && row.width == width)
        && let Some((row, _)) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.start == caret.saturating_add(1))
    {
        return Some((row, 0));
    }
    rows.iter().enumerate().find_map(|(index, row)| {
        (row.start <= caret && caret <= row.end).then(|| {
            (
                index,
                text[row.start..caret]
                    .graphemes(true)
                    .map(|grapheme| Line::raw(grapheme).width())
                    .sum(),
            )
        })
    })
}

fn byte_at_column(text: &str, row: &VisualRow, target: usize) -> usize {
    let mut column = 0_usize;
    let mut caret = row.start;
    for (offset, grapheme) in text[row.start..row.end].grapheme_indices(true) {
        let grapheme_start = row.start + offset;
        let grapheme_end = grapheme_start + grapheme.len();
        let next_column = column.saturating_add(Line::raw(grapheme).width());
        if target < next_column {
            return caret;
        }
        caret = grapheme_end;
        column = next_column;
        if target == column {
            return caret;
        }
    }
    caret
}

#[cfg(test)]
mod tests {
    use super::{MAX_PROMPT_BYTES, PromptEdit, PromptEditor, PromptLayout};

    fn from_text(text: &str) -> PromptEditor {
        let mut editor = PromptEditor::default();
        for character in text.chars() {
            let edit = if character == '\n' {
                PromptEdit::InsertNewline
            } else {
                PromptEdit::Insert(character)
            };
            assert!(editor.apply(edit, 80));
        }
        editor
    }

    fn row_texts(layout: &PromptLayout) -> Vec<&str> {
        layout.rows().iter().map(|row| row.text()).collect()
    }

    fn rows_with_sources(layout: &PromptLayout) -> Vec<(&str, std::ops::Range<usize>)> {
        layout
            .rows()
            .iter()
            .map(|row| (row.text(), row.source()))
            .collect()
    }

    #[test]
    fn inserts_at_the_caret_and_take_returns_the_exact_text() {
        let mut editor = from_text("acd");
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::Insert('b'), 80));
        assert_eq!(editor.text(), "abcd");
        assert_eq!(editor.take(), "abcd");
        assert!(editor.is_empty());
        assert_eq!(editor.layout(4, 2).cursor(), Some((0, 0)));
    }

    #[test]
    fn moves_and_deletes_extended_graphemes() {
        let mut editor = from_text("e\u{301}🧑\u{200d}💻x");
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::Right, 80));
        assert!(editor.apply(PromptEdit::DeletePreviousGrapheme, 80));
        assert_eq!(editor.text(), "e\u{301}🧑\u{200d}💻");
        assert!(editor.apply(PromptEdit::DeletePreviousGrapheme, 80));
        assert_eq!(editor.text(), "e\u{301}");
        assert!(editor.apply(PromptEdit::DeletePreviousGrapheme, 80));
        assert!(editor.is_empty());

        let mut editor = from_text("e\u{301}🧑\u{200d}💻x");
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::DeleteNextGrapheme, 80));
        assert_eq!(editor.text(), "e\u{301}🧑\u{200d}💻");
        assert!(editor.apply(PromptEdit::DeletePreviousGrapheme, 80));
        assert_eq!(editor.text(), "e\u{301}");
    }

    #[test]
    fn rejects_middle_insertions_that_exceed_the_utf8_limit() {
        let mut editor = from_text(&"a".repeat(MAX_PROMPT_BYTES - '🦀'.len_utf8()));
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::Insert('🦀'), 80));
        assert_eq!(editor.text().len(), MAX_PROMPT_BYTES);
        assert!(!editor.apply(PromptEdit::Insert('x'), 80));
        assert_eq!(
            editor.text(),
            format!("{}🦀a", "a".repeat(MAX_PROMPT_BYTES - '🦀'.len_utf8() - 1))
        );
    }

    #[test]
    fn bulk_paste_splices_a_fitting_utf8_prefix_at_the_caret() {
        let mut interior = from_text("α\nγ");
        assert!(interior.apply(PromptEdit::Left, 80));
        assert!(interior.insert_paste("β\n"));
        assert!(interior.apply(PromptEdit::Insert('δ'), 80));
        assert_eq!(interior.text(), "α\nβ\nδγ");

        let mut exact = PromptEditor::default();
        assert!(exact.insert_paste(&"x".repeat(MAX_PROMPT_BYTES)));
        assert_eq!(exact.text().len(), MAX_PROMPT_BYTES);

        let mut truncated = from_text(&"a".repeat(MAX_PROMPT_BYTES - 3));
        assert!(truncated.insert_paste("é🦀z"));
        assert_eq!(
            truncated.text(),
            format!("{}é", "a".repeat(MAX_PROMPT_BYTES - 3))
        );

        let mut no_room = from_text(&"a".repeat(MAX_PROMPT_BYTES - 1));
        assert!(!no_room.insert_paste("é"));
        assert!(!PromptEditor::default().insert_paste(""));
    }

    #[test]
    fn word_movement_preserves_uax_context_around_interior_carets() {
        for text in ["can't", "12,345"] {
            let punctuation = text.find(['\'', ',']).unwrap();

            let mut editor = from_text(text);
            assert!(editor.apply(PromptEdit::LineStart, 80));
            for _ in 0..=punctuation {
                assert!(editor.apply(PromptEdit::Right, 80));
            }
            assert!(editor.apply(PromptEdit::WordLeft, 80));
            assert!(editor.apply(PromptEdit::Insert('>'), 80));
            assert_eq!(editor.text(), format!(">{text}"));

            let mut editor = from_text(text);
            assert!(editor.apply(PromptEdit::LineStart, 80));
            for _ in 0..punctuation {
                assert!(editor.apply(PromptEdit::Right, 80));
            }
            assert!(editor.apply(PromptEdit::WordRight, 80));
            assert!(editor.apply(PromptEdit::Insert('>'), 80));
            assert_eq!(editor.text(), format!("{text}>"));
        }
    }

    #[test]
    fn deletes_previous_uax_word_bound_and_to_logical_line_start() {
        let mut editor = from_text("one café   ");
        assert!(editor.apply(PromptEdit::DeletePreviousWord, 80));
        assert_eq!(editor.text(), "one ");

        let mut editor = from_text("one\n  two");
        assert!(editor.apply(PromptEdit::DeleteToLineStart, 80));
        assert_eq!(editor.text(), "one\n");
    }

    #[test]
    fn homes_and_ends_within_the_current_logical_line() {
        let mut editor = from_text("abc\ndef");
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::LineStart, 80));
        assert!(editor.apply(PromptEdit::Insert('>'), 80));
        assert_eq!(editor.text(), "abc\n>def");
        assert!(editor.apply(PromptEdit::Left, 80));
        assert!(editor.apply(PromptEdit::LineEnd, 80));
        assert!(editor.apply(PromptEdit::Insert('!'), 80));
        assert_eq!(editor.text(), "abc\n>def!");
    }

    #[test]
    fn moves_vertically_through_hard_wrapped_and_explicit_rows() {
        let mut editor = from_text("ab日本\n🙂cdef");
        let layout = editor.layout(4, 10);
        assert_eq!(row_texts(&layout), ["ab日", "本", "🙂cd", "ef"]);
        assert_eq!(editor.layout(4, 10).cursor(), Some((3, 2)));
        assert!(editor.apply(PromptEdit::Up, 4));
        assert_eq!(editor.layout(4, 10).cursor(), Some((2, 2)));
        assert!(editor.apply(PromptEdit::Up, 4));
        assert_eq!(editor.layout(4, 10).cursor(), Some((1, 2)));
        assert!(editor.apply(PromptEdit::Up, 4));
        assert_eq!(editor.layout(4, 10).cursor(), Some((0, 2)));
        assert!(editor.apply(PromptEdit::Down, 4));
        assert_eq!(editor.layout(4, 10).cursor(), Some((1, 2)));
        assert!(editor.apply(PromptEdit::Down, 4));
        assert_eq!(editor.layout(4, 10).cursor(), Some((2, 2)));
    }

    #[test]
    fn preserves_explicit_empty_and_trailing_rows_and_exposes_full_row_end() {
        let mut editor = from_text("a\n\nb\n");
        let layout = editor.layout(4, 10);
        assert_eq!(row_texts(&layout), ["a", "", "b", ""]);
        assert_eq!(layout.rows().len(), 4);
        assert_eq!(layout.cursor(), Some((3, 0)));

        let mut editor = from_text("abcd");
        let layout = editor.layout(4, 10);
        assert_eq!(row_texts(&layout), ["abcd", ""]);
        assert_eq!(layout.cursor(), Some((1, 0)));

        let mut editor = from_text("abcd\nx");
        assert!(editor.apply(PromptEdit::LineStart, 4));
        assert!(editor.apply(PromptEdit::Up, 4));
        assert!(editor.apply(PromptEdit::LineEnd, 4));
        assert_eq!(editor.layout(4, 10).cursor(), Some((1, 0)));
    }

    #[test]
    fn keeps_the_tail_and_moved_caret_visible_in_its_viewport() {
        let mut editor = from_text("abcdefghij");
        let layout = editor.layout(3, 2);
        assert_eq!(row_texts(&layout), ["ghi", "j"]);
        assert_eq!(layout.cursor(), Some((1, 1)));

        assert!(editor.apply(PromptEdit::Up, 3));
        let layout = editor.layout(3, 2);
        assert_eq!(row_texts(&layout), ["ghi", "j"]);
        assert_eq!(layout.cursor(), Some((0, 1)));

        assert!(editor.apply(PromptEdit::Up, 3));
        let layout = editor.layout(3, 2);
        assert_eq!(row_texts(&layout), ["def", "ghi"]);
        assert_eq!(layout.cursor(), Some((0, 1)));
    }

    #[test]
    fn visible_rows_keep_absolute_sources_through_wrapping_newlines_and_viewports() {
        let mut editor = from_text("ab界cd\n\nxyz\n");

        let layout = editor.layout(3, 6);
        assert_eq!(
            rows_with_sources(&layout),
            [
                ("ab", 0..2),
                ("界c", 2..6),
                ("d", 6..7),
                ("", 8..8),
                ("xyz", 9..12),
                ("", 13..13),
            ]
        );

        let layout = editor.layout(3, 2);
        assert_eq!(rows_with_sources(&layout), [("xyz", 9..12), ("", 13..13)]);

        assert!(editor.apply(PromptEdit::PromptStart, 3));
        let layout = editor.layout(3, 2);
        assert_eq!(rows_with_sources(&layout), [("ab", 0..2), ("界c", 2..6)]);
    }

    #[test]
    fn layout_does_not_change_the_caret() {
        let mut editor = from_text("abcdef");
        assert!(editor.caret_at_end());
        assert!(editor.apply(PromptEdit::Left, 3));
        assert!(!editor.caret_at_end());

        let _ = editor.layout(3, 1);
        assert!(!editor.caret_at_end());
        assert!(editor.apply(PromptEdit::Insert('X'), 3));

        assert_eq!(editor.text(), "abcdeXf");
    }

    #[test]
    fn rejects_control_input() {
        let mut editor = PromptEditor::default();
        assert!(!editor.apply(PromptEdit::Insert('\t'), 80));
        assert!(!editor.apply(PromptEdit::Insert('\u{009b}'), 80));
        assert!(editor.is_empty());
    }
}
