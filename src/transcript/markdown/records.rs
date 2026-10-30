use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use yaml_rust2::Yaml;

use super::{Cell, LineBreak, MarkdownLayout, Table, wrapping};
use crate::theme::Theme;

pub(super) fn render(table: &Table, width: u16, theme: &Theme) -> MarkdownLayout {
    let mut layout = MarkdownLayout::hard(Text::default());
    if width == 0 {
        return layout;
    }
    let labels: Vec<_> = (0..table.alignments.len())
        .map(|index| {
            let label = table
                .header
                .get(index)
                .filter(|cell| !cell_text(cell).trim().is_empty())
                .cloned()
                .unwrap_or_else(|| Cell::plain(Line::raw(format!("Column {}", index + 1))));
            scalar(&label, theme.syntax_variable)
        })
        .collect();
    if table.rows.is_empty() {
        for label in &labels {
            append_wrapped(&mut layout, label, width);
        }
        append_wrapped(&mut layout, &Cell::plain(Line::raw("(no rows)")), width);
        return layout;
    }
    let punctuation = Style::new().fg(theme.syntax_punctuation);
    for (row_index, row) in table.rows.iter().enumerate() {
        if row_index > 0 {
            layout.text.lines.push(Line::default());
            layout.breaks.push(LineBreak::Hard);
        }
        for (column, label) in labels.iter().enumerate() {
            let empty = Cell::plain(Line::default());
            let cell = row.get(column).unwrap_or(&empty);
            let color = if is_display_number(&cell_text(cell)) {
                theme.syntax_number
            } else {
                theme.syntax_string
            };
            let value = scalar(cell, color);
            let mut field = Cell::plain(Line::from(Span::styled(
                if column == 0 { "- " } else { "  " },
                punctuation,
            )));
            append_cell(&mut field, label);
            field.line.spans.push(Span::styled(": ", punctuation));
            append_cell(&mut field, &value);
            append_wrapped(&mut layout, &field, width);
        }
    }
    layout
}

fn cell_text(cell: &Cell) -> String {
    cell.line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn append_wrapped(layout: &mut MarkdownLayout, cell: &Cell, width: u16) {
    let wrapped = wrapping::wrap_records(&cell.line, usize::from(width), &cell.protected);
    layout.text.lines.extend(wrapped.text.lines);
    layout.breaks.extend(wrapped.breaks);
}

fn append_cell(target: &mut Cell, cell: &Cell) {
    let offset = target.line.to_string().len();
    target.protected.extend(
        cell.protected
            .iter()
            .map(|range| range.start + offset..range.end + offset),
    );
    target.line.spans.extend(
        cell.line
            .spans
            .iter()
            .map(|span| Span::styled(span.content.clone(), cell.line.style.patch(span.style))),
    );
}

fn is_display_number(text: &str) -> bool {
    text.bytes()
        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
        && text.parse::<f64>().is_ok()
}

fn needs_quotes(text: &str) -> bool {
    if is_display_number(text) {
        return false;
    }
    text.is_empty()
        || text.trim() != text
        || text.chars().any(char::is_control)
        || text.starts_with([
            '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@',
            '`', '+',
        ])
        || text
            .strip_prefix('-')
            .is_some_and(|tail| tail.is_empty() || tail.starts_with(char::is_whitespace))
        || text.contains('"')
        || text.contains(": ")
        || text.contains(" #")
        || text.ends_with(':')
        || matches!(text, "---" | "...")
        || ["true", "false", "null", "~", "yes", "no", "on", "off"]
            .iter()
            .any(|value| text.eq_ignore_ascii_case(value))
        || !matches!(Yaml::from_str(text), Yaml::String(_))
}

fn escaped(text: &str) -> String {
    let encoded = serde_json::to_string(text).expect("string serialization");
    encoded[1..encoded.len() - 1].to_owned()
}

fn scalar(cell: &Cell, color: Color) -> Cell {
    let text = cell_text(cell);
    let quoted = needs_quotes(&text);
    let mut spans = Vec::new();
    if quoted {
        spans.push(Span::styled("\"", Style::new().fg(color)));
    }
    spans.extend(cell.line.spans.iter().map(|span| {
        let mut style = cell.line.style.patch(span.style);
        if style.fg.is_none() {
            style = style.fg(color);
        }
        let content = if quoted {
            escaped(&span.content)
        } else {
            span.content.to_string()
        };
        Span::styled(content, style)
    }));
    if quoted {
        spans.push(Span::styled("\"", Style::new().fg(color)));
    }
    let protected = if quoted {
        cell.protected
            .iter()
            .map(|range| {
                1 + escaped(&text[..range.start]).len()..1 + escaped(&text[..range.end]).len()
            })
            .collect()
    } else {
        cell.protected.clone()
    };
    Cell {
        line: Line {
            spans,
            style: cell.line.style,
            alignment: None,
        },
        protected,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Cell, LineBreak};
    use super::*;
    use crate::theme::DARK;

    use pulldown_cmark::Alignment;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    fn render(table: &Table, width: u16) -> MarkdownLayout {
        super::render(table, width, &DARK)
    }

    fn table(header: &[&str], rows: &[&[&str]]) -> Table {
        Table {
            alignments: vec![Alignment::None; header.len()],
            header: header
                .iter()
                .map(|value| Cell::plain(Line::from(Span::raw((*value).to_owned()))))
                .collect(),
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|value| Cell::plain(Line::from(Span::raw((*value).to_owned()))))
                        .collect()
                })
                .collect(),
        }
    }

    #[test]
    fn records_repeat_ordered_fields_with_one_gap_between_rows() {
        let table = table(
            &["Name", "State"],
            &[&["Kiki", "ready"], &["Worker", "running"]],
        );
        let layout = render(&table, 80);
        assert_eq!(
            layout.text.to_string(),
            "- Name: Kiki\n  State: ready\n\n- Name: Worker\n  State: running"
        );
        assert_eq!(layout.breaks, vec![LineBreak::Hard; 5]);
    }

    #[test]
    fn records_preserve_duplicate_empty_and_ambiguous_fields() {
        let table = table(&["", "Same", "Same"], &[&["", "true", "a: b"]]);
        let layout = render(&table, 80);
        assert_eq!(
            layout.text.to_string(),
            "- Column 1: \"\"\n  Same: \"true\"\n  Same: \"a: b\""
        );
        assert!(
            layout.text.lines[0]
                .spans
                .iter()
                .any(|span| span.content == "Column 1"
                    && span.style.fg == Some(DARK.syntax_variable)
                    && !span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn scalar_quoting_is_yaml_friendly_without_coercing_values() {
        for (value, scalar) in [
            ("true", "\"true\""),
            ("null", "\"null\""),
            ("123", "123"),
            ("01", "01"),
            (".nan", "\".nan\""),
            ("2026-10-29", "2026-10-29"),
            ("x # comment", "\"x # comment\""),
            ("a: b", "\"a: b\""),
            ("{yaml}", "\"{yaml}\""),
            ("[array]", "\"[array]\""),
            ("\"quoted\"", "\"\\\"quoted\\\"\""),
            ("Kiki", "Kiki"),
            ("https://example.com/a", "https://example.com/a"),
        ] {
            let layout = render(&table(&["Value"], &[&[value]]), 120);
            assert_eq!(
                layout.text.to_string(),
                format!("- Value: {scalar}"),
                "{value}"
            );
        }
        let layout = render(&table(&["a: b"], &[&["value"]]), 80);
        assert_eq!(layout.text.to_string(), "- \"a: b\": value");
    }

    #[test]
    fn decimal_display_numbers_and_safe_strings_are_plain_without_changing_spelling() {
        for value in [
            "42",
            "-5",
            "+5",
            "3.5",
            ".5",
            "5.",
            "1e3",
            "-1.2e-3",
            "007",
            "1e999",
            "2026-10-29",
            "10:30",
            "two words",
            "-x",
            "--flag",
            "NaN",
            "Infinity",
            "inf",
            "1_000",
            "1,000",
            "50%",
            "$5",
        ] {
            let layout = render(&table(&["Value"], &[&[value]]), 120);
            assert_eq!(
                layout.text.to_string(),
                format!("- Value: {value}"),
                "{value}"
            );
        }
        for value in [
            "", "tRuE", "FaLsE", "Null", "~", "YES", "off", "0x1F", "0o17", ".inf", "-.inf",
            ".nan", "---", "...", "-", "- item", "+x", "+1 day", "a: b", "x # note", "edge ",
            "a\nb", "a\"b",
        ] {
            let layout = render(&table(&["Value"], &[&[value]]), 120);
            assert_eq!(
                layout.text.to_string(),
                format!("- Value: {}", serde_json::to_string(value).unwrap()),
                "{value:?}"
            );
        }
    }

    #[test]
    fn label_quotes_cannot_unprotect_value_escape_pairs() {
        let layout = render(&table(&["Size\""], &[&["C:\\foo\\bar a: b"]]), 9);
        for line in &layout.text.lines {
            let text = line.to_string();
            let slashes = text.chars().rev().take_while(|ch| *ch == '\\').count();
            assert_eq!(slashes % 2, 0, "escape split in {text:?}");
        }
    }

    #[test]
    fn common_record_scalars_round_trip_as_yaml_strings() {
        for value in [
            "",
            "true",
            "null",
            "2026-10-29",
            "a: b",
            "x # note",
            "Kiki",
            "C:\\folder\\file",
        ] {
            let layout = render(&table(&["Value"], &[&[value]]), 120);
            let docs = yaml_rust2::YamlLoader::load_from_str(&layout.text.to_string()).unwrap();
            assert_eq!(
                docs[0][0]["Value"],
                Yaml::String(value.to_owned()),
                "{value}"
            );
        }
    }

    #[test]
    fn quoting_keeps_inline_styles_and_code_identity() {
        let mut table = table(&["Value"], &[&["a: b"]]);
        table.rows[0][0] = Cell {
            line: Line::from(Span::styled(
                "a: b",
                Style::new().fg(Color::Yellow).italic(),
            )),
            protected: std::iter::once(0..4).collect(),
        };
        let layout = render(&table, 80);
        assert_eq!(layout.text.to_string(), "- Value: \"a: b\"");
        let styled = layout.text.lines[0]
            .spans
            .iter()
            .find(|span| span.content == "a: b")
            .unwrap();
        assert_eq!(styled.style.fg, Some(Color::Yellow));
        assert!(styled.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn header_only_and_short_rows_do_not_invent_or_drop_fields() {
        let layout = render(&table(&["Name", "State"], &[]), 80);
        assert_eq!(layout.text.to_string(), "Name\nState\n(no rows)");
        let layout = render(&table(&["Name", "State"], &[&["Kiki"]]), 80);
        assert_eq!(layout.text.to_string(), "- Name: Kiki\n  State: \"\"");
    }

    #[test]
    fn records_wrap_words_and_preserve_soft_break_provenance() {
        let layout = render(&table(&["H"], &[&["one two six ten"]]), 7);
        assert_eq!(layout.text.to_string(), "- H:\none two\nsix ten");
        assert_eq!(
            layout.breaks,
            [LineBreak::Hard, LineBreak::Word, LineBreak::Word]
        );
        assert!(layout.text.lines.iter().all(|line| line.width() <= 7));
        let layout = render(&table(&["H"], &[&["value"]]), 0);
        assert!(layout.text.lines.is_empty());
        assert!(layout.breaks.is_empty());
    }
}
