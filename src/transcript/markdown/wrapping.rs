use std::ops::Range;

use ratatui::text::{Line, Span, Text};
use unicode_segmentation::UnicodeSegmentation;

use super::{LineBreak, MarkdownLayout};

pub(super) fn minimum_width(line: &Line<'_>, protected: &[Range<usize>]) -> usize {
    word_ranges(&line.to_string(), protected)
        .into_iter()
        .map(|range| slice(line, range).width())
        .max()
        .unwrap_or(0)
}

pub(super) fn wrap_words(
    line: &Line<'_>,
    width: usize,
    protected: &[Range<usize>],
) -> Option<MarkdownLayout> {
    let content = line.to_string();
    if width == 0 && !content.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    let mut breaks = vec![LineBreak::Hard];
    let mut start = None;
    let mut end = 0;
    for token in word_ranges(&content, protected) {
        if slice(line, token.clone()).width() > width {
            return None;
        }
        if let Some(first) = start {
            if slice(line, first..token.end).width() <= width {
                end = token.end;
                continue;
            }
            lines.push(slice(line, first..end));
            breaks.push(if end < token.start {
                LineBreak::Word
            } else {
                LineBreak::Token
            });
        }
        start = Some(token.start);
        end = token.end;
    }
    lines.push(slice(line, start.unwrap_or(0)..end));
    Some(MarkdownLayout {
        text: Text::from(lines),
        breaks,
    })
}

fn word_ranges(content: &str, protected: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut chunks = Vec::new();
    let mut start = None;
    for (offset, grapheme) in content.grapheme_indices(true) {
        if grapheme.chars().all(char::is_whitespace) {
            if let Some(first) = start.take() {
                chunks.push(first..offset);
            }
        } else {
            start.get_or_insert(offset);
        }
    }
    if let Some(first) = start {
        chunks.push(first..content.len());
    }
    let mut words = Vec::new();
    for chunk in chunks {
        let text = &content[chunk.clone()];
        // Unicode word boundaries must not split URLs or punctuation-bearing identifiers.
        if text.chars().any(|ch| ch.is_ascii_punctuation())
            || protected
                .iter()
                .any(|range| range.start < chunk.end && chunk.start < range.end)
        {
            words.push(chunk);
        } else {
            words.extend(text.split_word_bound_indices().map(|(offset, word)| {
                let first = chunk.start + offset;
                first..first + word.len()
            }));
        }
    }
    words
}

fn slice(line: &Line<'_>, range: Range<usize>) -> Line<'static> {
    let mut offset = 0;
    let mut spans = Vec::new();
    for span in &line.spans {
        let end = offset + span.content.len();
        let first = range.start.max(offset);
        let last = range.end.min(end);
        if first < last {
            spans.push(Span::styled(
                span.content[first - offset..last - offset].to_owned(),
                span.style,
            ));
        }
        offset = end;
        if offset >= range.end {
            break;
        }
    }
    Line {
        spans,
        style: line.style,
        alignment: line.alignment,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Alignment;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Span, Text};

    use super::super::LineBreak;

    fn minimum_width(line: &Line<'_>) -> usize {
        super::minimum_width(line, &[])
    }

    fn wrap_words(line: &Line<'_>, width: usize) -> Option<MarkdownLayout> {
        super::wrap_words(line, width, &[])
    }

    fn assert_wrap(
        line: &Line<'_>,
        width: usize,
        rows: &[&str],
        breaks: &[LineBreak],
    ) -> MarkdownLayout {
        let layout = wrap_words(line, width).expect("whole tokens fit");
        assert_eq!(
            layout
                .text
                .lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>(),
            rows
        );
        assert_eq!(layout.breaks, breaks);
        assert!(layout.text.lines.iter().all(|line| line.width() <= width));
        layout
    }

    #[test]
    fn wraps_exactly_at_word_boundaries() {
        assert_wrap(
            &Line::from("one two three four"),
            7,
            &["one two", "three", "four"],
            &[LineBreak::Hard, LineBreak::Word, LineBreak::Word],
        );
        assert_wrap(&Line::from("one two"), 7, &["one two"], &[LineBreak::Hard]);
    }

    #[test]
    fn moves_a_short_word_intact_to_the_next_line() {
        assert_wrap(
            &Line::from("abcde cat"),
            7,
            &["abcde", "cat"],
            &[LineBreak::Hard, LineBreak::Word],
        );
    }

    #[test]
    fn longest_whole_token_is_the_minimum_width() {
        for (text, width) in [
            ("a elephant cat", 8),
            ("extraordinary", 13),
            ("cafe\u{301} tea", 4),
            ("東京京都", 2),
            ("👩‍💻 ok", 2),
            ("", 0),
            ("   \t ", 0),
        ] {
            assert_eq!(minimum_width(&Line::from(text)), width, "{text:?}");
        }
    }

    #[test]
    fn rejects_a_word_wider_than_the_whole_line() {
        let line = Line::from("a extraordinary word");
        assert!(wrap_words(&line, 12).is_none());
        assert_wrap(
            &line,
            13,
            &["a", "extraordinary", "word"],
            &[LineBreak::Hard, LineBreak::Word, LineBreak::Word],
        );
    }

    #[test]
    fn style_boundaries_do_not_split_words_and_styles_are_retained() {
        let bold = Style::new().bold();
        let code = Style::new().fg(Color::Yellow);
        let line = Line::from(vec![
            Span::raw("go "),
            Span::styled("ele", bold),
            Span::styled("phant", code),
            Span::raw(" now"),
        ]);
        assert_eq!(minimum_width(&line), 8);
        assert!(wrap_words(&line, 7).is_none());
        let layout = assert_wrap(
            &line,
            8,
            &["go", "elephant", "now"],
            &[LineBreak::Hard, LineBreak::Word, LineBreak::Word],
        );
        assert_eq!(
            layout.text.lines[1].spans,
            vec![Span::styled("ele", bold), Span::styled("phant", code)]
        );
    }

    #[test]
    fn urls_and_punctuation_identifiers_are_indivisible() {
        for token in [
            "https://example.com/a?b=c#d",
            "https://example.com/東京",
            "snake_case",
            "hyphenated-token",
            "module::function=value",
            "path/to/file.txt",
        ] {
            let width = Line::from(token).width();
            let line = Line::from(format!("a {token} z"));
            assert_eq!(minimum_width(&line), width, "{token}");
            assert!(wrap_words(&line, width - 1).is_none(), "{token}");
            assert_wrap(
                &line,
                width,
                &["a", token, "z"],
                &[LineBreak::Hard, LineBreak::Word, LineBreak::Word],
            );
        }
    }

    #[test]
    fn cjk_uses_unicode_boundaries_without_adding_copy_spaces() {
        let layout = assert_wrap(
            &Line::from("東京京都 大阪"),
            4,
            &["東京", "京都", "大阪"],
            &[LineBreak::Hard, LineBreak::Token, LineBreak::Word],
        );
        assert_eq!(layout.text.lines[0].width(), 4);
        assert!(wrap_words(&Line::from("東京"), 1).is_none());
    }

    #[test]
    fn combining_accents_and_zwj_emoji_remain_whole() {
        for (text, width, rows) in [
            ("a cafe\u{301} b", 4, vec!["a", "cafe\u{301}", "b"]),
            ("x 👩‍💻 y", 2, vec!["x", "👩‍💻", "y"]),
            ("x 🇯🇵 y", 2, vec!["x", "🇯🇵", "y"]),
        ] {
            assert_wrap(
                &Line::from(text),
                width,
                &rows,
                &[LineBreak::Hard, LineBreak::Word, LineBreak::Word],
            );
        }
        assert!(wrap_words(&Line::from("👩‍💻"), 1).is_none());
    }

    #[test]
    fn line_style_and_alignment_survive_on_every_owned_row() {
        let style = Style::new().bg(Color::Blue).italic();
        for alignment in [Alignment::Left, Alignment::Center, Alignment::Right] {
            let line = Line::from("one two").style(style).alignment(alignment);
            let layout = assert_wrap(
                &line,
                3,
                &["one", "two"],
                &[LineBreak::Hard, LineBreak::Word],
            );
            assert_eq!(layout.text.style, Text::default().style);
            assert_eq!(layout.text.alignment, None);
            for row in layout.text.lines {
                assert_eq!(row.style, style);
                assert_eq!(row.alignment, Some(alignment));
                for span in row.spans {
                    assert!(matches!(span.content, std::borrow::Cow::Owned(_)));
                }
            }
        }
    }

    #[test]
    fn whitespace_is_retained_inside_lines_and_trimmed_at_wraps() {
        assert_wrap(
            &Line::from("  one  two   three  "),
            8,
            &["one  two", "three"],
            &[LineBreak::Hard, LineBreak::Word],
        );
        assert_wrap(
            &Line::from("a\tb\u{2003}c"),
            1,
            &["a", "b", "c"],
            &[LineBreak::Hard, LineBreak::Word, LineBreak::Word],
        );
        assert_wrap(&Line::from(" \t  "), 4, &[""], &[LineBreak::Hard]);
    }

    #[test]
    fn zero_width_is_safe_and_empty_input_is_one_stable_line() {
        assert!(wrap_words(&Line::from("a"), 0).is_none());
        assert!(wrap_words(&Line::from(" "), 0).is_none());
        for width in [0, 1, 10] {
            let line = Line::default().style(Style::new().bold()).right_aligned();
            let layout = assert_wrap(&line, width, &[""], &[LineBreak::Hard]);
            assert_eq!(layout.text.lines[0].style, line.style);
            assert_eq!(layout.text.lines[0].alignment, line.alignment);
        }
    }
}
