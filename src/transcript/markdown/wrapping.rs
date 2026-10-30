use std::ops::Range;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use unicode_segmentation::UnicodeSegmentation;

use super::{LineBreak, MarkdownLayout};

fn grapheme_width(symbol: &str) -> usize {
    if symbol.chars().any(char::is_control) {
        0
    } else {
        usize::from(symbol.cell_width())
    }
}

pub(super) fn display_width(line: &Line<'_>) -> usize {
    line.styled_graphemes(Style::default())
        .map(|grapheme| grapheme_width(grapheme.symbol))
        .sum()
}

pub(super) fn minimum_width(line: &Line<'_>, protected: &[Range<usize>]) -> usize {
    let content = line.to_string();
    let widths = RecordWidths::new(line, &content);
    word_ranges(&content, protected)
        .into_iter()
        .map(|range| widths.width(range))
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
    let widths = RecordWidths::new(line, &content);
    let mut lines = Vec::new();
    let mut breaks = vec![LineBreak::Hard];
    let mut start = None;
    let mut end = 0;
    for token in word_ranges(&content, protected) {
        if widths.width(token.clone()) > width {
            return None;
        }
        if let Some(first) = start {
            if widths.width(first..token.end) <= width {
                end = token.end;
                continue;
            }
            lines.push(widths.slice(first..end));
            breaks.push(if end < token.start {
                LineBreak::Word
            } else {
                LineBreak::Token
            });
        }
        start = Some(token.start);
        end = token.end;
    }
    lines.push(widths.slice(start.unwrap_or(0)..end));
    Some(MarkdownLayout {
        text: Text::from(lines),
        breaks,
        prefixes: Vec::new(),
    })
}

pub(super) fn wrap_records(
    line: &Line<'_>,
    width: usize,
    protected: &[Range<usize>],
) -> MarkdownLayout {
    if width == 0 {
        return MarkdownLayout {
            text: Text::default(),
            breaks: Vec::new(),
            prefixes: Vec::new(),
        };
    }
    let content = line.to_string();
    let mut tokens = word_ranges(&content, protected);
    if let Some(first) = tokens.first() {
        if first.start > 0 {
            tokens.insert(0, 0..first.start);
        }
    } else if !content.is_empty() {
        tokens.push(0..content.len());
    }
    let widths = RecordWidths::new(line, &content);
    let mut protected_breaks = vec![false; content.len() + 1];
    let mut protected_separators = vec![false; content.len()];
    for unit in record_units(&content, &tokens) {
        #[cfg(test)]
        UNIT_VISITS.with(|visits| visits.set(visits.get() + unit.len()));
        protected_separators[unit.clone()].fill(true);
        if widths.width(unit.clone()) <= width {
            protected_breaks[unit.start + 1..unit.end].fill(true);
        }
    }
    let mut lines = Vec::new();
    let mut breaks = Vec::new();
    let mut current: Option<Range<usize>> = None;
    let mut next_break = LineBreak::Hard;
    for mut token in tokens {
        if let Some(range) = current.take() {
            if widths.width(range.start..token.end) <= width {
                current = Some(range.start..token.end);
                continue;
            }
            lines.push(widths.slice(range.clone()));
            breaks.push(next_break);
            next_break = if range.end < token.start {
                LineBreak::Word
            } else {
                LineBreak::Token
            };
        }
        while widths.width(token.clone()) > width {
            let end = record_break(
                &widths,
                &content,
                token.clone(),
                width,
                &protected_breaks,
                &protected_separators,
            );
            if end == token.end {
                break;
            }
            lines.push(widths.slice(token.start..end));
            breaks.push(next_break);
            next_break = LineBreak::Token;
            token.start = end;
        }
        current = Some(token);
    }
    lines.push(widths.slice(current.unwrap_or(0..0)));
    breaks.push(next_break);
    MarkdownLayout {
        text: Text::from(lines),
        breaks,
        prefixes: Vec::new(),
    }
}

fn record_units(content: &str, tokens: &[Range<usize>]) -> Vec<Range<usize>> {
    let bytes = content.as_bytes();
    let mut units = Vec::new();
    let mut quoted = false;
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'"' {
            quoted = !quoted;
            offset += 1;
        } else if quoted && bytes[offset] == b'\\' && offset + 1 < bytes.len() {
            let start = offset;
            offset += 1;
            if bytes[offset] == b'u'
                && offset + 5 <= bytes.len()
                && bytes[offset + 1..offset + 5]
                    .iter()
                    .all(u8::is_ascii_hexdigit)
            {
                offset += 5;
            } else {
                offset += content[offset..].chars().next().unwrap().len_utf8();
            }
            units.push(start..offset);
        } else if bytes[offset] == b'%'
            && offset + 3 <= bytes.len()
            && bytes[offset + 1..offset + 3]
                .iter()
                .all(u8::is_ascii_hexdigit)
        {
            units.push(offset..offset + 3);
            offset += 3;
        } else {
            offset += content[offset..].chars().next().unwrap().len_utf8();
        }
    }
    let mut schemes = Vec::new();
    for token in tokens {
        let text = &content[token.clone()];
        let prefix = text.trim_start_matches(['"', '(']);
        if let Some(scheme) = prefix.find("://")
            && scheme > 0
            && prefix.as_bytes()[0].is_ascii_alphabetic()
            && prefix[..scheme]
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'+' | b'-' | b'.'))
        {
            let end = token.start + text.len() - prefix.len() + scheme + 3;
            let host = content[end..token.end].graphemes(true).next().unwrap_or("");
            let mut host_end = end + host.len();
            let index = units.partition_point(|unit| {
                #[cfg(test)]
                UNIT_VISITS.with(|visits| visits.set(visits.get() + 1));
                unit.end <= host_end
            });
            if let Some(unit) = units.get(index)
                && unit.start < host_end
            {
                host_end = unit.end;
            }
            // Keep the first host unit whole, including an overlapping escape.
            schemes.push(token.start..host_end);
        }
    }
    units.extend(schemes);
    units
}

struct RecordWidths<'a> {
    line: &'a Line<'a>,
    span_ends: Vec<usize>,
    prefixes: Vec<(usize, usize)>,
}

impl<'a> RecordWidths<'a> {
    fn new(line: &'a Line<'a>, content: &str) -> Self {
        let mut offset = 0;
        let span_ends = line
            .spans
            .iter()
            .map(|span| {
                offset += span.content.len();
                offset
            })
            .collect();
        let mut widths = Self {
            line,
            span_ends,
            prefixes: vec![(0, 0)],
        };
        let mut total = 0;
        for (start, grapheme) in content.grapheme_indices(true) {
            let end = start + grapheme.len();
            total += widths.exact(start..end);
            widths.prefixes.push((end, total));
        }
        widths
    }

    fn summed(&self, range: Range<usize>) -> Option<usize> {
        let first = self
            .prefixes
            .binary_search_by_key(&range.start, |(offset, _)| *offset)
            .ok()?;
        let last = self
            .prefixes
            .binary_search_by_key(&range.end, |(offset, _)| *offset)
            .ok()?;
        Some(self.prefixes[last].1 - self.prefixes[first].1)
    }

    fn width(&self, range: Range<usize>) -> usize {
        self.summed(range.clone())
            .unwrap_or_else(|| self.exact(range))
    }

    fn slice(&self, range: Range<usize>) -> Line<'static> {
        let first_span = self.span_ends.partition_point(|end| *end <= range.start);
        let offset = if first_span == 0 {
            0
        } else {
            self.span_ends[first_span - 1]
        };
        slice_from(self.line, range, first_span, offset)
    }

    fn exact(&self, range: Range<usize>) -> usize {
        #[cfg(test)]
        MEASURED_BYTES.with(|bytes| bytes.set(bytes.get() + range.len()));
        let first_span = self.span_ends.partition_point(|end| *end <= range.start);
        let mut offset = if first_span == 0 {
            0
        } else {
            self.span_ends[first_span - 1]
        };
        let mut width = 0;
        for span in &self.line.spans[first_span..] {
            let end = offset + span.content.len();
            let first = range.start.max(offset);
            let last = range.end.min(end);
            if first < last {
                width += span.content[first - offset..last - offset]
                    .graphemes(true)
                    .map(grapheme_width)
                    .sum::<usize>();
            }
            offset = end;
            if offset >= range.end {
                break;
            }
        }
        width
    }
}

fn record_break(
    widths: &RecordWidths<'_>,
    content: &str,
    token: Range<usize>,
    width: usize,
    protected_breaks: &[bool],
    protected_separators: &[bool],
) -> usize {
    let mut separators = [None; 4];
    let mut fallback = None;
    let mut first = token.end;
    for (offset, grapheme) in content[token.clone()].grapheme_indices(true) {
        let start = token.start + offset;
        let end = start + grapheme.len();
        #[cfg(test)]
        UNIT_VISITS.with(|visits| visits.set(visits.get() + 1));
        if protected_breaks[end] {
            continue;
        }
        first = end;
        if widths.width(token.start..end) > width {
            break;
        }
        fallback = Some(end);
        if protected_separators[start] {
            continue;
        }
        let tier = match grapheme {
            "/" | "\\" => 0,
            "?" | "&" | "#" => 1,
            "_" | "-" => 2,
            "." | ":" | "=" => 3,
            _ => continue,
        };
        separators[tier] = Some(end);
    }
    separators
        .into_iter()
        .flatten()
        .next()
        .or(fallback)
        .unwrap_or(first)
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

#[cfg(test)]
thread_local! {
    static SPAN_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SLICED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static MEASURED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static UNIT_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn slice_from(
    line: &Line<'_>,
    range: Range<usize>,
    first_span: usize,
    mut offset: usize,
) -> Line<'static> {
    let mut spans = Vec::new();
    for span in &line.spans[first_span..] {
        #[cfg(test)]
        SPAN_VISITS.with(|visits| visits.set(visits.get() + 1));
        let end = offset + span.content.len();
        let first = range.start.max(offset);
        let last = range.end.min(end);
        if first < last {
            #[cfg(test)]
            SLICED_BYTES.with(|bytes| bytes.set(bytes.get() + last - first));
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

    fn assert_records(
        line: &Line<'_>,
        width: usize,
        protected: &[Range<usize>],
        rows: &[&str],
        breaks: &[LineBreak],
    ) -> MarkdownLayout {
        let layout = super::wrap_records(line, width, protected);
        assert_eq!(
            layout
                .text
                .lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>(),
            rows,
            "source: {:?}, width: {width}",
            line.to_string()
        );
        assert_eq!(layout.breaks, breaks);
        layout
    }

    fn copied(layout: &MarkdownLayout) -> String {
        let mut text = String::new();
        for (index, line) in layout.text.lines.iter().enumerate() {
            if index > 0 {
                match layout.breaks[index] {
                    LineBreak::Original | LineBreak::Hard => text.push('\n'),
                    LineBreak::Word => text.push(' '),
                    LineBreak::Token => {}
                }
            }
            text.push_str(&line.to_string());
        }
        text
    }

    #[test]
    fn records_measure_the_graphemes_the_renderer_draws() {
        let layout = assert_records(
            &Line::from("abⵏ⵿ⴾ"),
            1,
            &[],
            &["a", "b", "ⵏ⵿", "ⴾ"],
            &[
                LineBreak::Hard,
                LineBreak::Token,
                LineBreak::Token,
                LineBreak::Token,
            ],
        );
        assert_eq!(copied(&layout), "abⵏ⵿ⴾ");
    }

    #[test]
    fn records_keep_non_additive_token_measurement_bounded() {
        let token = "ⵏ⵿ⴾa".repeat(2_000);
        let protected = std::iter::once(0..token.len()).collect::<Vec<_>>();
        MEASURED_BYTES.with(|bytes| bytes.set(0));
        let layout = wrap_records(&Line::from(token.as_str()), 2, &protected);
        let measured = MEASURED_BYTES.with(std::cell::Cell::get);
        assert_eq!(copied(&layout), token);
        assert!(layout.text.lines.iter().all(|row| row.width() <= 2));
        assert!(measured <= token.len() * 16, "{measured} measured bytes");
    }

    #[test]
    fn grids_do_not_rescan_preceding_styled_spans_for_each_row() {
        let spans = (0..3000)
            .map(|index| {
                Span::styled(
                    "x ",
                    if index % 2 == 0 {
                        Style::new().bold()
                    } else {
                        Style::new().italic()
                    },
                )
            })
            .collect::<Vec<_>>();
        let line = Line::from(spans);
        SPAN_VISITS.with(|visits| visits.set(0));
        let layout = wrap_words(&line, 1).expect("one-column words fit");
        let visits = SPAN_VISITS.with(std::cell::Cell::get);
        assert_eq!(layout.text.lines.len(), line.spans.len());
        assert_eq!(copied(&layout), line.to_string().trim_end());
        for (row, span) in layout.text.lines.iter().zip(&line.spans) {
            assert_eq!(row.spans, [Span::styled("x", span.style)]);
        }
        assert!(
            visits <= line.spans.len() * 2,
            "{visits} source-span visits"
        );
    }

    #[test]
    fn records_do_not_rescan_preceding_styled_spans_for_each_row() {
        let line = Line::from(
            (0..3_000)
                .map(|index| {
                    Span::styled(
                        "a",
                        if index % 2 == 0 {
                            Style::new().bold()
                        } else {
                            Style::new().italic()
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );
        SPAN_VISITS.with(|visits| visits.set(0));
        let layout = wrap_records(&line, 1, &[]);
        let visits = SPAN_VISITS.with(std::cell::Cell::get);
        assert_eq!(copied(&layout), "a".repeat(3_000));
        for (row, original) in layout.text.lines.iter().zip(&line.spans) {
            assert_eq!(row.spans[0].style, original.style);
        }
        assert!(
            visits <= line.spans.len() * 4,
            "{visits} source-span visits"
        );
    }

    #[test]
    fn records_copy_large_separator_free_tokens_with_linear_work() {
        let token = "a".repeat(50_000);
        for width in [1, 80] {
            SLICED_BYTES.with(|bytes| bytes.set(0));
            MEASURED_BYTES.with(|bytes| bytes.set(0));
            let layout = wrap_records(&Line::from(token.as_str()), width, &[]);
            let sliced_bytes = SLICED_BYTES.with(std::cell::Cell::get);
            let measured_bytes = MEASURED_BYTES.with(std::cell::Cell::get);
            assert!(
                measured_bytes <= token.len() * 8,
                "{measured_bytes} measured bytes at width {width}"
            );
            assert_eq!(copied(&layout), token);
            assert_eq!(layout.text.lines.len(), token.len().div_ceil(width));
            for (index, row) in layout.text.lines.iter().enumerate() {
                let start = index * width;
                assert_eq!(
                    row.to_string(),
                    token[start..(start + width).min(token.len())]
                );
                assert_eq!(
                    layout.breaks[index],
                    if index == 0 {
                        LineBreak::Hard
                    } else {
                        LineBreak::Token
                    }
                );
            }
            assert!(layout.prefixes.is_empty());
            assert!(
                sliced_bytes <= token.len() * 4,
                "{sliced_bytes} copied bytes at width {width}"
            );
        }
    }

    #[test]
    fn records_scan_large_escape_tokens_with_linear_work() {
        for token in [
            "%2F".repeat(2_000),
            format!("\"{}\"", r"\u0000".repeat(1_000)),
        ] {
            for width in [1, 80] {
                SLICED_BYTES.with(|bytes| bytes.set(0));
                MEASURED_BYTES.with(|bytes| bytes.set(0));
                UNIT_VISITS.with(|visits| visits.set(0));
                let layout = wrap_records(&Line::from(token.as_str()), width, &[]);
                let sliced_bytes = SLICED_BYTES.with(std::cell::Cell::get);
                let measured_bytes = MEASURED_BYTES.with(std::cell::Cell::get);
                let unit_visits = UNIT_VISITS.with(std::cell::Cell::get);
                assert_eq!(copied(&layout), token);
                assert!(layout.text.lines.iter().all(|row| row.width() <= width));
                assert!(
                    measured_bytes <= token.len() * 8,
                    "{measured_bytes} measured bytes at width {width}"
                );
                assert!(
                    unit_visits <= token.len() * 16,
                    "{unit_visits} unit visits at width {width}"
                );
                assert!(
                    sliced_bytes <= token.len() * 4,
                    "{sliced_bytes} copied bytes at width {width}"
                );
            }
        }
    }

    #[test]
    fn records_use_every_separator_tier_and_rightmost_fitting_break() {
        for (token, width, rows) in [
            ("aa/bb/cccc", 6, vec!["aa/bb/", "cccc"]),
            (r"aa\bb\cccc", 6, vec![r"aa\bb\", "cccc"]),
            ("aa?bb&cccc", 6, vec!["aa?bb&", "cccc"]),
            ("aa#bbbb", 4, vec!["aa#", "bbbb"]),
            ("aa_bb_cccc", 6, vec!["aa_bb_", "cccc"]),
            ("aa-bb-cccc", 6, vec!["aa-bb-", "cccc"]),
            ("aa.bb.cccc", 6, vec!["aa.bb.", "cccc"]),
            ("aa:bb:cccc", 6, vec!["aa:bb:", "cccc"]),
            ("aa=bb=cccc", 6, vec!["aa=bb=", "cccc"]),
        ] {
            let mut breaks = vec![LineBreak::Token; rows.len()];
            breaks[0] = LineBreak::Hard;
            let layout = assert_records(&Line::from(token), width, &[], &rows, &breaks);
            assert_eq!(copied(&layout), token);
            assert!(layout.text.lines.iter().all(|line| line.width() <= width));
        }
    }

    #[test]
    fn records_reapply_separator_priority_instead_of_filling_lines() {
        let token = "a/b?c_d.eeeee";
        let layout = assert_records(
            &Line::from(token),
            8,
            &[],
            &["a/", "b?", "c_", "d.eeeee"],
            &[
                LineBreak::Hard,
                LineBreak::Token,
                LineBreak::Token,
                LineBreak::Token,
            ],
        );
        assert_eq!(copied(&layout), token);
        assert_records(
            &Line::from("a/b/c/longtail"),
            8,
            &[],
            &["a/b/c/", "longtail"],
            &[LineBreak::Hard, LineBreak::Token],
        );
    }

    #[test]
    fn records_move_tokens_that_fit_a_fresh_line_intact_after_labels() {
        for token in ["path/to/file", "snake_case", "https://x.y", "ordinary"] {
            let source = format!("- Key: {token}");
            let width = Line::from(token).width();
            let layout = assert_records(
                &Line::from(source.clone()),
                width,
                &[],
                &["- Key:", token],
                &[LineBreak::Hard, LineBreak::Word],
            );
            assert_eq!(copied(&layout), source);
        }
    }

    #[test]
    fn overwide_protected_sequences_use_lossless_grapheme_fallback() {
        for token in [
            "https://example.com/a",
            "(https://example.com/a)",
            "(https://%65xample.com/a)",
            "%2F",
            r#""a\u0000b\\c\"d""#,
        ] {
            for width in 1..=16 {
                let layout = wrap_records(&Line::from(token), width, &[]);
                assert!(
                    layout.text.lines.iter().all(|line| line.width() <= width),
                    "{token:?} at {width}: {:?}",
                    layout.text
                );
                assert_eq!(copied(&layout), token, "{token:?} at {width}");
            }
        }
    }

    #[test]
    fn records_protect_schemes_and_percent_escapes() {
        for (token, width, rows) in [
            (
                "https://example.com/a",
                12,
                vec!["https://exam", "ple.com/a"],
            ),
            (
                "https://example.com/a",
                8,
                vec!["https://", "example.", "com/a"],
            ),
            ("ab%2Fcd/efgh", 5, vec!["ab%2F", "cd/", "efgh"]),
            ("%2F", 1, vec!["%", "2", "F"]),
        ] {
            let mut breaks = vec![LineBreak::Token; rows.len()];
            breaks[0] = LineBreak::Hard;
            let layout = assert_records(&Line::from(token), width, &[], &rows, &breaks);
            assert_eq!(copied(&layout), token);
        }
    }

    #[test]
    fn records_distinguish_quoted_escapes_from_raw_windows_separators() {
        let token = r#""ab\ncd\"ef\\gh\u0000ij""#;
        let layout = assert_records(
            &Line::from(token),
            5,
            &[],
            &[r#""ab\n"#, r#"cd\"e"#, r"f\\gh", r"\u000", "0ij\""],
            &[
                LineBreak::Hard,
                LineBreak::Token,
                LineBreak::Token,
                LineBreak::Token,
                LineBreak::Token,
            ],
        );
        assert_eq!(copied(&layout), token);
        let path = r"C:\foo\bar\file";
        let layout = assert_records(
            &Line::from(path),
            8,
            &[],
            &[r"C:\foo\", r"bar\file"],
            &[LineBreak::Hard, LineBreak::Token],
        );
        assert_eq!(copied(&layout), path);
    }

    #[test]
    fn records_emergency_split_hashes_and_whole_graphemes_losslessly() {
        for (token, width, rows) in [
            ("0123456789abcdef", 5, vec!["01234", "56789", "abcde", "f"]),
            (
                "a👩‍💻b🇯🇵cafe\u{301}",
                3,
                vec!["a👩‍💻", "b🇯🇵", "caf", "e\u{301}"],
            ),
        ] {
            let protected = std::iter::once(0..token.len()).collect::<Vec<_>>();
            let mut breaks = vec![LineBreak::Token; rows.len()];
            breaks[0] = LineBreak::Hard;
            let layout = assert_records(&Line::from(token), width, &protected, &rows, &breaks);
            assert_eq!(copied(&layout), token);
            assert!(layout.text.lines.iter().all(|line| line.width() <= width));
        }
    }

    #[test]
    fn records_keep_code_whole_but_use_unicode_prose_boundaries() {
        let line = Line::from("a 東京");
        assert_records(
            &line,
            4,
            &[],
            &["a 東", "京"],
            &[LineBreak::Hard, LineBreak::Token],
        );
        assert_records(
            &line,
            4,
            std::slice::from_ref(&(2..line.to_string().len())),
            &["a", "東京"],
            &[LineBreak::Hard, LineBreak::Word],
        );
        let line = Line::from("  Key: 東京京都");
        let content = line.to_string();
        let layout = assert_records(
            &line,
            6,
            std::slice::from_ref(&(content.find('東').unwrap()..content.len())),
            &["  Key:", "東京京", "都"],
            &[LineBreak::Hard, LineBreak::Word, LineBreak::Token],
        );
        assert_eq!(copied(&layout), content);
    }

    #[test]
    fn records_preserve_styles_across_token_splits_and_initial_indentation() {
        let bold = Style::new().bold();
        let code = Style::new().fg(Color::Yellow);
        let style = Style::new().bg(Color::Blue).italic();
        let line = Line::from(vec![
            Span::raw("- "),
            Span::styled("Key:", bold),
            Span::raw(" ab/"),
            Span::styled("cd/efgh", code),
        ])
        .style(style)
        .right_aligned();
        let layout = assert_records(
            &line,
            7,
            &[],
            &["- Key:", "ab/cd/", "efgh"],
            &[LineBreak::Hard, LineBreak::Word, LineBreak::Token],
        );
        assert_eq!(
            layout.text.lines[1].spans,
            vec![Span::raw("ab/"), Span::styled("cd/", code)]
        );
        assert_eq!(layout.text.lines[2].spans, vec![Span::styled("efgh", code)]);
        for row in &layout.text.lines {
            assert_eq!(row.style, style);
            assert_eq!(row.alignment, Some(Alignment::Right));
            assert!(
                row.spans
                    .iter()
                    .all(|span| matches!(span.content, std::borrow::Cow::Owned(_)))
            );
        }
        assert_eq!(copied(&layout), line.to_string());
        assert_records(
            &Line::from("  Key: one two"),
            10,
            &[],
            &["  Key: one", "two"],
            &[LineBreak::Hard, LineBreak::Word],
        );
    }

    #[test]
    fn records_are_safe_at_zero_and_indivisible_tiny_widths() {
        for text in ["", "- Key: text", "  Key: 👩‍💻"] {
            let layout = super::wrap_records(&Line::from(text), 0, &[]);
            assert!(layout.text.lines.is_empty());
            assert!(layout.breaks.is_empty());
        }
        assert_records(&Line::from(""), 1, &[], &[""], &[LineBreak::Hard]);
        let layout = assert_records(
            &Line::from("- K: 👩‍💻z"),
            1,
            &[],
            &["-", "K", ":", "👩‍💻", "z"],
            &[
                LineBreak::Hard,
                LineBreak::Word,
                LineBreak::Token,
                LineBreak::Word,
                LineBreak::Token,
            ],
        );
        assert_eq!(copied(&layout), "- K: 👩‍💻z");
    }
}
