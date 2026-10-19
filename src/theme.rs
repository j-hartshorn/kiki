use ratatui::style::Color;

pub(crate) struct Theme {
    pub(crate) text: Color,
    pub(crate) muted: Color,
    pub(crate) dim: Color,
    pub(crate) user_message_bg: Color,
    pub(crate) tool_pending_bg: Color,
    pub(crate) tool_success_bg: Color,
    pub(crate) tool_error_bg: Color,
    pub(crate) thinking_text: Color,
    pub(crate) card_bg: Color,
    pub(crate) error: Color,
    pub(crate) warning: Color,
    pub(crate) accent: Color,
    pub(crate) success: Color,
    pub(crate) border: Color,
    pub(crate) border_accent: Color,
    pub(crate) selected_bg: Color,
    pub(crate) thinking_off: Color,
    pub(crate) thinking_minimal: Color,
    pub(crate) thinking_low: Color,
    pub(crate) thinking_medium: Color,
    pub(crate) thinking_high: Color,
    pub(crate) thinking_xhigh: Color,
    pub(crate) thinking_max: Color,
    pub(crate) syntax_comment: Color,
    pub(crate) syntax_keyword: Color,
    pub(crate) syntax_function: Color,
    pub(crate) syntax_string: Color,
    pub(crate) syntax_number: Color,
    pub(crate) md_heading: Color,
    pub(crate) md_link: Color,
    pub(crate) md_quote: Color,
    pub(crate) md_code: Color,
    pub(crate) border_muted: Color,
    pub(crate) syntax_variable: Color,
    pub(crate) syntax_type: Color,
    pub(crate) syntax_operator: Color,
    pub(crate) syntax_punctuation: Color,
}

pub(crate) static DARK: Theme = Theme {
    text: Color::Rgb(0xde, 0xe0, 0xe1),
    muted: Color::Rgb(0x9d, 0xa5, 0xa9),
    dim: Color::Rgb(0x7e, 0x88, 0x8e),
    user_message_bg: Color::Rgb(0x21, 0x3b, 0x49),
    tool_pending_bg: Color::Rgb(0x34, 0x38, 0x3a),
    tool_success_bg: Color::Rgb(0x25, 0x41, 0x31),
    tool_error_bg: Color::Rgb(0x5b, 0x28, 0x2a),
    thinking_text: Color::Rgb(0x96, 0xa0, 0xa4),
    card_bg: Color::Rgb(0x28, 0x2c, 0x34),
    error: Color::Rgb(0xea, 0x7f, 0x81),
    warning: Color::Rgb(0xcd, 0x9a, 0x22),
    accent: Color::Rgb(0xa7, 0x98, 0xd7),
    success: Color::Rgb(0x68, 0xb7, 0x8d),
    border: Color::Rgb(0x5f, 0xa8, 0xcc),
    border_accent: Color::Rgb(0xa0, 0x8e, 0xd5),
    selected_bg: Color::Rgb(0x21, 0x3b, 0x49),
    thinking_off: Color::Rgb(0x6c, 0x76, 0x7b),
    thinking_minimal: Color::Rgb(0x68, 0x80, 0x8d),
    thinking_low: Color::Rgb(0x54, 0x89, 0xa4),
    thinking_medium: Color::Rgb(0x61, 0x85, 0xcc),
    thinking_high: Color::Rgb(0x97, 0x76, 0xe5),
    thinking_xhigh: Color::Rgb(0xde, 0x54, 0xc1),
    thinking_max: Color::Rgb(0xfe, 0x54, 0x62),
    syntax_comment: Color::Rgb(0x9d, 0xa5, 0xa9),
    syntax_keyword: Color::Rgb(0x69, 0xad, 0xd0),
    syntax_function: Color::Rgb(0xcd, 0x9a, 0x22),
    syntax_string: Color::Rgb(0xde, 0x8d, 0x5a),
    syntax_number: Color::Rgb(0x68, 0xb7, 0x8d),
    md_heading: Color::Rgb(0xcd, 0x9a, 0x22),
    md_link: Color::Rgb(0x69, 0xad, 0xd0),
    md_quote: Color::Rgb(0x9d, 0xa5, 0xa9),
    md_code: Color::Rgb(0xa7, 0x98, 0xd7),
    border_muted: Color::Rgb(0x76, 0x81, 0x86),
    syntax_variable: Color::Rgb(0x5d, 0xb3, 0xba),
    syntax_type: Color::Rgb(0xa7, 0x98, 0xd7),
    syntax_operator: Color::Rgb(0x9d, 0xa5, 0xa9),
    syntax_punctuation: Color::Rgb(0x9d, 0xa5, 0xa9),
};

// Keep these distinct from DARK and from each other to catch token mix-ups.
#[cfg(test)]
pub(crate) static DISTINCT_TEST_THEME: Theme = Theme {
    selected_bg: Color::Rgb(1, 2, 3),
    text: Color::Rgb(4, 5, 6),
    border_accent: Color::Rgb(7, 8, 9),
    ..DARK
};

#[cfg(test)]
mod tests {
    use super::DARK;
    use ratatui::style::Color;

    fn hex(value: &str) -> Color {
        let value = u32::from_str_radix(value.trim_start_matches('#'), 16).unwrap();
        Color::Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    #[test]
    fn dark_tokens_match_pi_resolved_hex() {
        for (actual, expected) in [
            (DARK.text, "#dee0e1"),
            (DARK.muted, "#9da5a9"),
            (DARK.dim, "#7e888e"),
            (DARK.user_message_bg, "#213b49"),
            (DARK.tool_pending_bg, "#34383a"),
            (DARK.tool_success_bg, "#254131"),
            (DARK.tool_error_bg, "#5b282a"),
            (DARK.thinking_text, "#96a0a4"),
            (DARK.card_bg, "#282c34"),
            (DARK.error, "#ea7f81"),
            (DARK.warning, "#cd9a22"),
            (DARK.accent, "#a798d7"),
            (DARK.success, "#68b78d"),
            (DARK.border, "#5fa8cc"),
            (DARK.border_accent, "#a08ed5"),
            (DARK.selected_bg, "#213b49"),
            (DARK.thinking_off, "#6c767b"),
            (DARK.thinking_minimal, "#68808d"),
            (DARK.thinking_low, "#5489a4"),
            (DARK.thinking_medium, "#6185cc"),
            (DARK.thinking_high, "#9776e5"),
            (DARK.thinking_xhigh, "#de54c1"),
            (DARK.thinking_max, "#fe5462"),
            (DARK.syntax_comment, "#9da5a9"),
            (DARK.syntax_keyword, "#69add0"),
            (DARK.syntax_function, "#cd9a22"),
            (DARK.syntax_string, "#de8d5a"),
            (DARK.syntax_number, "#68b78d"),
            (DARK.md_heading, "#cd9a22"),
            (DARK.md_link, "#69add0"),
            (DARK.md_quote, "#9da5a9"),
            (DARK.md_code, "#a798d7"),
            (DARK.border_muted, "#768186"),
            (DARK.syntax_variable, "#5db3ba"),
            (DARK.syntax_type, "#a798d7"),
            (DARK.syntax_operator, "#9da5a9"),
            (DARK.syntax_punctuation, "#9da5a9"),
        ] {
            assert_eq!(actual, hex(expected), "{expected}");
        }
    }
}
