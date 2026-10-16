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
        ] {
            assert_eq!(actual, hex(expected), "{expected}");
        }
    }
}
