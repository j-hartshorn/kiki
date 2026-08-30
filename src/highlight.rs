use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span, Text};
use tree_sitter_highlight::{Highlight, HighlightConfiguration, HighlightEvent, Highlighter};

const HIGHLIGHT_NAMES: [&str; 5] = ["comment", "keyword", "function", "string", "number"];

#[allow(dead_code)]
pub(crate) struct PythonHighlighter {
    state: Option<(Highlighter, Box<HighlightConfiguration>)>,
}

impl PythonHighlighter {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        let mut configuration = match HighlightConfiguration::new(
            tree_sitter_python::LANGUAGE.into(),
            "python",
            tree_sitter_python::HIGHLIGHTS_QUERY,
            "",
            "",
        ) {
            Ok(configuration) => configuration,
            Err(_) => return Self::fallback(),
        };
        configuration.configure(&HIGHLIGHT_NAMES);

        Self {
            state: Some((Highlighter::new(), Box::new(configuration))),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn highlight<'a>(&mut self, source: &'a str) -> Text<'a> {
        let Some((highlighter, configuration)) = &mut self.state else {
            return plain_text(source);
        };

        highlighted(highlighter, configuration, source).unwrap_or_else(|| plain_text(source))
    }

    fn fallback() -> Self {
        Self { state: None }
    }

    #[cfg(test)]
    pub(crate) fn fallback_for_test() -> Self {
        Self::fallback()
    }
}

fn highlighted<'a>(
    highlighter: &mut Highlighter,
    configuration: &HighlightConfiguration,
    source: &'a str,
) -> Option<Text<'a>> {
    let events = highlighter
        .highlight(configuration, source.as_bytes(), None, |_| None)
        .ok()?;
    let mut spans = vec![Vec::new()];
    let mut styles = Vec::new();
    let mut source_end = 0;

    for event in events {
        match event.ok()? {
            HighlightEvent::Source { start, end } => {
                if start != source_end
                    || start > end
                    || end > source.len()
                    || !source.is_char_boundary(start)
                    || !source.is_char_boundary(end)
                {
                    return None;
                }
                append_source(
                    &mut spans,
                    source,
                    start,
                    end,
                    styles.last().copied().unwrap_or_default(),
                )?;
                source_end = end;
            }
            HighlightEvent::HighlightStart(highlight) => {
                styles.push(style_for(highlight)?);
            }
            HighlightEvent::HighlightEnd => {
                styles.pop()?;
            }
        }
    }

    if source_end != source.len() || !styles.is_empty() {
        return None;
    }

    Some(Text::from(
        spans.into_iter().map(Line::from).collect::<Vec<_>>(),
    ))
}

fn style_for(highlight: Highlight) -> Option<Style> {
    let colour = match highlight.0 {
        0 => Color::DarkGray,
        1 => Color::Magenta,
        2 => Color::Cyan,
        3 => Color::Green,
        4 => Color::Yellow,
        _ => return None,
    };
    Some(Style::new().fg(colour))
}

fn append_source<'a>(
    spans: &mut Vec<Vec<Span<'a>>>,
    source: &'a str,
    start: usize,
    end: usize,
    style: Style,
) -> Option<()> {
    for part in source[start..end].split_inclusive('\n') {
        let content = part.strip_suffix('\n').unwrap_or(part);
        spans.last_mut()?.push(Span::styled(content, style));
        if part.ends_with('\n') {
            spans.push(Vec::new());
        }
    }
    Some(())
}

fn plain_text(source: &str) -> Text<'_> {
    Text::from(
        source
            .split('\n')
            .map(|line| Line::from(Span::raw(line)))
            .collect::<Vec<_>>(),
    )
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};
    use ratatui::text::Text;

    use super::PythonHighlighter;

    fn reconstruct(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn style_for_content(text: &Text<'_>, content: &str) -> Style {
        text.lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content == content)
            .expect("highlighted span")
            .style
    }

    #[test]
    fn highlights_complete_python_with_expected_styles() {
        let source = "# note\ndef greeting():\n    text = \"hello\"\n    return 42\n";
        let mut highlighter = PythonHighlighter::new();
        let text = highlighter.highlight(source);

        assert_eq!(reconstruct(&text), source);
        assert_eq!(
            style_for_content(&text, "# note"),
            Style::new().fg(Color::DarkGray)
        );
        assert_eq!(
            style_for_content(&text, "def"),
            Style::new().fg(Color::Magenta)
        );
        assert_eq!(
            style_for_content(&text, "greeting"),
            Style::new().fg(Color::Cyan)
        );
        assert_eq!(
            style_for_content(&text, "\"hello\""),
            Style::new().fg(Color::Green)
        );
        assert_eq!(
            style_for_content(&text, "42"),
            Style::new().fg(Color::Yellow)
        );
    }

    #[test]
    fn preserves_every_utf8_prefix_of_incomplete_python() {
        let source = "# Café 日本 🦀\ndef greet_é(名):\n    message = \"Hello 🦀";
        let mut highlighter = PythonHighlighter::new();

        for end in 0..=source.len() {
            if source.is_char_boundary(end) {
                let prefix = &source[..end];
                assert_eq!(
                    reconstruct(&highlighter.highlight(prefix)),
                    prefix,
                    "{prefix:?}"
                );
            }
        }
    }

    #[test]
    fn preserves_empty_interior_repeated_and_trailing_newlines() {
        let mut highlighter = PythonHighlighter::new();

        for source in ["", "\n", "before\n\nafter\n", "\n\n"] {
            let text = highlighter.highlight(source);
            assert_eq!(reconstruct(&text), source);
            assert_eq!(text.lines.len(), source.split('\n').count());
        }
    }

    #[test]
    fn forced_fallback_preserves_exact_text_with_default_styles() {
        let source = "# Café 日本 🦀\n\ndef incomplete(名):\n    value = \"hello";
        let mut highlighter = PythonHighlighter::fallback_for_test();
        let text = highlighter.highlight(source);

        assert_eq!(reconstruct(&text), source);
        for line in &text.lines {
            assert_eq!(line.style, Style::default());
            for span in &line.spans {
                assert_eq!(span.style, Style::default());
            }
        }
    }
}
