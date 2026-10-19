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

pub(crate) static LIGHT: Theme = Theme {
    text: Color::Rgb(0x3b, 0x3f, 0x41),
    muted: Color::Rgb(0x67, 0x71, 0x76),
    dim: Color::Rgb(0x87, 0x90, 0x95),
    user_message_bg: Color::Rgb(0xdf, 0xe7, 0xec),
    tool_pending_bg: Color::Rgb(0xe4, 0xe5, 0xe6),
    tool_success_bg: Color::Rgb(0xde, 0xe9, 0xe1),
    tool_error_bg: Color::Rgb(0xee, 0xe2, 0xe1),
    thinking_text: Color::Rgb(0x7c, 0x86, 0x8c),
    card_bg: Color::Rgb(0xf7, 0xf6, 0xf6),
    error: Color::Rgb(0xc8, 0x25, 0x3d),
    warning: Color::Rgb(0x8f, 0x68, 0x02),
    accent: Color::Rgb(0x74, 0x59, 0xb4),
    success: Color::Rgb(0x33, 0x7e, 0x58),
    border: Color::Rgb(0x3d, 0x8e, 0xb3),
    border_accent: Color::Rgb(0x8a, 0x72, 0xcb),
    selected_bg: Color::Rgb(0xdf, 0xe7, 0xec),
    thinking_off: Color::Rgb(0xc2, 0xc8, 0xca),
    thinking_minimal: Color::Rgb(0xb5, 0xc4, 0xcb),
    thinking_low: Color::Rgb(0x9f, 0xc2, 0xd5),
    thinking_medium: Color::Rgb(0xa2, 0xb7, 0xe0),
    thinking_high: Color::Rgb(0xb5, 0xa5, 0xe8),
    thinking_xhigh: Color::Rgb(0xe5, 0x85, 0xcd),
    thinking_max: Color::Rgb(0xfe, 0x74, 0x79),
    syntax_comment: Color::Rgb(0x67, 0x71, 0x76),
    syntax_keyword: Color::Rgb(0x2f, 0x78, 0x99),
    syntax_function: Color::Rgb(0x8f, 0x68, 0x02),
    syntax_string: Color::Rgb(0xa4, 0x54, 0x17),
    syntax_number: Color::Rgb(0x33, 0x7e, 0x58),
    md_heading: Color::Rgb(0x8f, 0x68, 0x02),
    md_link: Color::Rgb(0x2f, 0x78, 0x99),
    md_quote: Color::Rgb(0x67, 0x71, 0x76),
    md_code: Color::Rgb(0x74, 0x59, 0xb4),
    border_muted: Color::Rgb(0x9a, 0xa2, 0xa7),
    syntax_variable: Color::Rgb(0x28, 0x7a, 0x81),
    syntax_type: Color::Rgb(0x74, 0x59, 0xb4),
    syntax_operator: Color::Rgb(0x67, 0x71, 0x76),
    syntax_punctuation: Color::Rgb(0x67, 0x71, 0x76),
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ThemeSetting {
    Dark,
    Light,
    #[default]
    Automatic,
}

impl ThemeSetting {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "automatic" => Some(Self::Automatic),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Automatic => "automatic",
        }
    }

    pub(crate) fn resolve(self, appearance: Appearance) -> &'static Theme {
        match (self, appearance) {
            (Self::Dark, _) | (Self::Automatic, Appearance::Dark) => &DARK,
            (Self::Light, _) | (Self::Automatic, Appearance::Light) => &LIGHT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Appearance {
    Dark,
    Light,
}

pub(crate) fn appearance_from_colorfgbg(value: &str) -> Option<Appearance> {
    let background = value.rsplit(';').next()?.trim();
    if background.is_empty()
        || background.len() > 2
        || !background.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    match background.parse::<u8>().ok()? {
        0..=6 | 8 => Some(Appearance::Dark),
        7 | 9..=15 => Some(Appearance::Light),
        _ => None,
    }
}

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
    fn light_tokens_match_pi_resolved_hex() {
        for (actual, expected) in [
            (super::LIGHT.text, "#3b3f41"),
            (super::LIGHT.muted, "#677176"),
            (super::LIGHT.dim, "#879095"),
            (super::LIGHT.user_message_bg, "#dfe7ec"),
            (super::LIGHT.tool_pending_bg, "#e4e5e6"),
            (super::LIGHT.tool_success_bg, "#dee9e1"),
            (super::LIGHT.tool_error_bg, "#eee2e1"),
            (super::LIGHT.thinking_text, "#7c868c"),
            (super::LIGHT.card_bg, "#f7f6f6"),
            (super::LIGHT.error, "#c8253d"),
            (super::LIGHT.warning, "#8f6802"),
            (super::LIGHT.accent, "#7459b4"),
            (super::LIGHT.success, "#337e58"),
            (super::LIGHT.border, "#3d8eb3"),
            (super::LIGHT.border_accent, "#8a72cb"),
            (super::LIGHT.selected_bg, "#dfe7ec"),
            (super::LIGHT.thinking_off, "#c2c8ca"),
            (super::LIGHT.thinking_minimal, "#b5c4cb"),
            (super::LIGHT.thinking_low, "#9fc2d5"),
            (super::LIGHT.thinking_medium, "#a2b7e0"),
            (super::LIGHT.thinking_high, "#b5a5e8"),
            (super::LIGHT.thinking_xhigh, "#e585cd"),
            (super::LIGHT.thinking_max, "#fe7479"),
            (super::LIGHT.syntax_comment, "#677176"),
            (super::LIGHT.syntax_keyword, "#2f7899"),
            (super::LIGHT.syntax_function, "#8f6802"),
            (super::LIGHT.syntax_string, "#a45417"),
            (super::LIGHT.syntax_number, "#337e58"),
            (super::LIGHT.md_heading, "#8f6802"),
            (super::LIGHT.md_link, "#2f7899"),
            (super::LIGHT.md_quote, "#677176"),
            (super::LIGHT.md_code, "#7459b4"),
            (super::LIGHT.border_muted, "#9aa2a7"),
            (super::LIGHT.syntax_variable, "#287a81"),
            (super::LIGHT.syntax_type, "#7459b4"),
            (super::LIGHT.syntax_operator, "#677176"),
            (super::LIGHT.syntax_punctuation, "#677176"),
        ] {
            assert_eq!(actual, hex(expected), "{expected}");
        }
    }

    #[test]
    fn theme_setting_round_trips_exact_lowercase_values() {
        for setting in [
            super::ThemeSetting::Dark,
            super::ThemeSetting::Light,
            super::ThemeSetting::Automatic,
        ] {
            assert_eq!(super::ThemeSetting::parse(setting.as_str()), Some(setting));
        }
        for invalid in ["Dark", "", "blue"] {
            assert_eq!(super::ThemeSetting::parse(invalid), None);
        }
    }

    #[test]
    fn theme_setting_resolves_fixed_and_automatic_appearances() {
        use super::{Appearance, LIGHT, ThemeSetting};
        for (setting, appearance, expected) in [
            (ThemeSetting::Dark, Appearance::Dark, &DARK),
            (ThemeSetting::Dark, Appearance::Light, &DARK),
            (ThemeSetting::Light, Appearance::Dark, &LIGHT),
            (ThemeSetting::Light, Appearance::Light, &LIGHT),
            (ThemeSetting::Automatic, Appearance::Dark, &DARK),
            (ThemeSetting::Automatic, Appearance::Light, &LIGHT),
        ] {
            assert!(
                std::ptr::eq(setting.resolve(appearance), expected),
                "{setting:?} {appearance:?}"
            );
        }
    }

    #[test]
    fn colorfgbg_uses_the_last_ansi_background_index() {
        use super::Appearance::{Dark, Light};
        for (value, expected) in [
            ("15;0", Some(Dark)),
            ("0;15", Some(Light)),
            ("0;7", Some(Light)),
            ("0;8", Some(Dark)),
            ("default;default", None),
            ("0;16", None),
            ("", None),
            ("0;default;15", Some(Light)),
            ("0;blue", None),
            ("0; 15 ", Some(Light)),
            ("0;+15", None),
            ("0;000", None),
            ("0;-1", None),
            ("0;", None),
        ] {
            assert_eq!(super::appearance_from_colorfgbg(value), expected, "{value}");
        }
        for index in 0..=15 {
            let expected = if index <= 6 || index == 8 {
                Dark
            } else {
                Light
            };
            assert_eq!(
                super::appearance_from_colorfgbg(&format!("0;{index}")),
                Some(expected)
            );
        }
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
