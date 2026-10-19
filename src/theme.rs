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

pub(crate) trait TerminalQuery {
    fn write_query(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn read_byte(&mut self, deadline: std::time::Instant) -> std::io::Result<Option<u8>>;
}

pub(crate) struct StdioTerminal;

impl StdioTerminal {
    pub(crate) fn new() -> std::io::Result<Self> {
        ratatui::crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for StdioTerminal {
    fn drop(&mut self) {
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

impl TerminalQuery for StdioTerminal {
    fn write_query(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;

        let mut stdout = std::io::stdout().lock();
        stdout.write_all(bytes)?;
        stdout.flush()
    }

    fn read_byte(&mut self, deadline: std::time::Instant) -> std::io::Result<Option<u8>> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let timeout = Timespec::try_from(remaining).map_err(std::io::Error::other)?;
        let stdin = rustix::stdio::stdin();
        let mut fds = [PollFd::new(&stdin, PollFlags::IN)];
        if poll(&mut fds, Some(&timeout))? == 0 || std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        if !fds[0].revents().contains(PollFlags::IN) {
            return Err(std::io::Error::other("terminal input unavailable"));
        }
        let mut byte = [0];
        match rustix::io::read(stdin, &mut byte)? {
            0 => Ok(None),
            _ => Ok(Some(byte[0])),
        }
    }
}

pub(crate) fn detect_appearance(
    terminal: &mut impl TerminalQuery,
    colorfgbg: Option<&str>,
) -> Appearance {
    use std::time::{Duration, Instant};

    enum ReplyState {
        Ignore,
        Escape,
        Osc,
        OscEnd,
        Csi,
        Da1,
    }

    let fallback = colorfgbg
        .and_then(appearance_from_colorfgbg)
        .unwrap_or(Appearance::Dark);
    let deadline = Instant::now() + Duration::from_millis(500);
    if terminal.write_query(b"\x1b]11;?\x07\x1b[c").is_err() {
        return fallback;
    }
    let mut state = ReplyState::Ignore;
    let mut osc = Vec::new();
    let mut background = None;
    while Instant::now() < deadline {
        let byte = match terminal.read_byte(deadline) {
            Ok(Some(byte)) => byte,
            Ok(None) | Err(_) => return fallback,
        };
        if byte == b'\x1b' {
            state = match state {
                ReplyState::Osc => ReplyState::OscEnd,
                _ => ReplyState::Escape,
            };
            continue;
        }
        state = match state {
            ReplyState::OscEnd if byte == b'\\' => {
                background = parse_osc11(&osc).or(background);
                ReplyState::Ignore
            }
            ReplyState::Escape | ReplyState::OscEnd => match byte {
                b']' => {
                    osc.clear();
                    ReplyState::Osc
                }
                b'[' => ReplyState::Csi,
                _ => ReplyState::Ignore,
            },
            ReplyState::Osc if byte == b'\x07' => {
                background = parse_osc11(&osc).or(background);
                ReplyState::Ignore
            }
            ReplyState::Osc if osc.len() < b"11;rgb:ffff/ffff/ffff".len() => {
                osc.push(byte);
                ReplyState::Osc
            }
            ReplyState::Csi if byte == b'?' => ReplyState::Da1,
            ReplyState::Da1 if byte == b'c' => return background.unwrap_or(fallback),
            ReplyState::Da1 if byte.is_ascii_digit() || byte == b';' => ReplyState::Da1,
            _ => ReplyState::Ignore,
        };
    }
    fallback
}

fn parse_osc11(bytes: &[u8]) -> Option<Appearance> {
    let value = std::str::from_utf8(bytes).ok()?.strip_prefix("11;rgb:")?;
    let mut channels = value.split('/');
    let red = parse_osc_channel(channels.next()?)?;
    let green = parse_osc_channel(channels.next()?)?;
    let blue = parse_osc_channel(channels.next()?)?;
    if channels.next().is_some() {
        return None;
    }
    let [red, green, blue] = [red, green, blue].map(|channel| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    });
    let l =
        (0.4122214694707629 * red + 0.5363325372617349 * green + 0.0514459932675022 * blue).cbrt();
    let m =
        (0.2119034958178251 * red + 0.6806995506452344 * green + 0.1073969535369405 * blue).cbrt();
    let s =
        (0.0883024591900564 * red + 0.2817188391361215 * green + 0.6299787016738222 * blue).cbrt();
    let lightness = 0.210454268309314 * l + 0.793617774702305 * m - 0.0040720430116193 * s;
    Some(if lightness >= 0.5 {
        Appearance::Light
    } else {
        Appearance::Dark
    })
}

fn parse_osc_channel(value: &str) -> Option<f64> {
    if !(1..=4).contains(&value.len()) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let max = (1_u32 << (4 * value.len())) - 1;
    Some(f64::from(u16::from_str_radix(value, 16).ok()?) / f64::from(max))
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

    use std::collections::VecDeque;
    use std::io;
    use std::time::{Duration, Instant};

    use super::{Appearance, TerminalQuery, detect_appearance};

    enum ScriptedRead {
        Bytes(VecDeque<u8>),
        Timeout,
        Error,
    }

    struct ScriptedTerminal {
        script: VecDeque<ScriptedRead>,
        writes: Vec<Vec<u8>>,
        deadlines: Vec<Instant>,
        write_error: bool,
    }

    impl ScriptedTerminal {
        fn new(chunks: &[&[u8]]) -> Self {
            Self {
                script: chunks
                    .iter()
                    .map(|chunk| ScriptedRead::Bytes(chunk.iter().copied().collect()))
                    .collect(),
                writes: Vec::new(),
                deadlines: Vec::new(),
                write_error: false,
            }
        }

        fn unread_bytes(&self) -> Vec<u8> {
            self.script
                .iter()
                .filter_map(|step| match step {
                    ScriptedRead::Bytes(bytes) => Some(bytes.iter().copied()),
                    _ => None,
                })
                .flatten()
                .collect()
        }
    }

    impl TerminalQuery for ScriptedTerminal {
        fn write_query(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes.push(bytes.to_vec());
            if self.write_error {
                Err(io::Error::other("scripted write error"))
            } else {
                Ok(())
            }
        }

        fn read_byte(&mut self, deadline: Instant) -> io::Result<Option<u8>> {
            self.deadlines.push(deadline);
            loop {
                match self.script.pop_front() {
                    Some(ScriptedRead::Bytes(mut bytes)) => {
                        if let Some(byte) = bytes.pop_front() {
                            self.script.push_front(ScriptedRead::Bytes(bytes));
                            return Ok(Some(byte));
                        }
                    }
                    Some(ScriptedRead::Error) => {
                        return Err(io::Error::other("scripted read error"));
                    }
                    Some(ScriptedRead::Timeout) | None => return Ok(None),
                }
            }
        }
    }

    #[test]
    fn osc11_classifies_black_and_white_with_each_channel_width_and_terminator() {
        for (channel, expected) in [
            ("0000", Appearance::Dark),
            ("00", Appearance::Dark),
            ("0", Appearance::Dark),
            ("ffff", Appearance::Light),
            ("ff", Appearance::Light),
            ("f", Appearance::Light),
            ("fff", Appearance::Light),
        ] {
            for terminator in ["\x07", "\x1b\\"] {
                let reply =
                    format!("\x1b]11;rgb:{channel}/{channel}/{channel}{terminator}\x1b[?1;2c");
                let mut terminal = ScriptedTerminal::new(&[reply.as_bytes()]);
                let fallback = if expected == Appearance::Dark {
                    "0;15"
                } else {
                    "15;0"
                };
                assert_eq!(
                    detect_appearance(&mut terminal, Some(fallback)),
                    expected,
                    "{reply:?}"
                );
                assert!(terminal.unread_bytes().is_empty());
            }
        }
    }

    #[test]
    fn osc11_uses_oklab_lightness_either_side_of_half() {
        let threshold = (1.055 * 0.125_f64.powf(1.0 / 2.4) - 0.055) * 65535.0;
        let below = threshold.floor() as u16;
        let above = below + 1;
        for (grey, expected) in [(below, Appearance::Dark), (above, Appearance::Light)] {
            let reply = format!("\x1b]11;rgb:{grey:04x}/{grey:04x}/{grey:04x}\x07\x1b[?1c");
            let mut terminal = ScriptedTerminal::new(&[reply.as_bytes()]);
            let fallback = if expected == Appearance::Dark {
                "0;15"
            } else {
                "15;0"
            };
            assert_eq!(
                detect_appearance(&mut terminal, Some(fallback)),
                expected,
                "{grey:04x}"
            );
        }
    }

    #[test]
    fn osc11_scales_each_channel_by_its_own_width() {
        for (channels, expected) in [
            ("f/ff/fff", Appearance::Light),
            ("3/33/333", Appearance::Dark),
            ("0000/f/ff", Appearance::Light),
        ] {
            let reply = format!("\x1b]11;rgb:{channels}\x07\x1b[?1c");
            let mut terminal = ScriptedTerminal::new(&[reply.as_bytes()]);
            assert_eq!(
                detect_appearance(&mut terminal, None),
                expected,
                "{channels}"
            );
        }
    }

    #[test]
    fn osc11_handles_three_fragmented_reply_batches_with_one_500ms_deadline() {
        let mut terminal =
            ScriptedTerminal::new(&[b"\x1b]11;rg", b"b:ff/ff/ff\x07\x1b[?", b"1;2c"]);
        let before = Instant::now();
        assert_eq!(detect_appearance(&mut terminal, None), Appearance::Light);
        let after = Instant::now();
        assert!(terminal.unread_bytes().is_empty());
        let deadline = terminal.deadlines[0];
        assert!(deadline >= before + Duration::from_millis(500));
        assert!(deadline <= after + Duration::from_millis(500));
        assert!(terminal.deadlines.iter().all(|&value| value == deadline));
    }

    #[test]
    fn osc11_da1_only_uses_colorfgbg_then_dark() {
        for (colorfgbg, expected) in [(Some("0;15"), Appearance::Light), (None, Appearance::Dark)] {
            let mut terminal = ScriptedTerminal::new(&[b"\x1b[?1;2c"]);
            assert_eq!(detect_appearance(&mut terminal, colorfgbg), expected);
            assert!(terminal.unread_bytes().is_empty());
        }
    }

    #[test]
    fn osc11_silence_times_out_immediately_in_the_fake() {
        for (colorfgbg, expected) in [
            (Some("0;15"), Appearance::Light),
            (None, Appearance::Dark),
            (Some("invalid"), Appearance::Dark),
        ] {
            let mut terminal = ScriptedTerminal::new(&[]);
            assert_eq!(detect_appearance(&mut terminal, colorfgbg), expected);
            assert_eq!(terminal.deadlines.len(), 1);
        }
    }

    #[test]
    fn osc11_ignores_garbage_before_da1() {
        let mut terminal = ScriptedTerminal::new(&[b"garbage\xff\x1b[31m\x1b[?1;2c"]);
        assert_eq!(
            detect_appearance(&mut terminal, Some("0;15")),
            Appearance::Light
        );
        assert!(terminal.unread_bytes().is_empty());
    }

    #[test]
    fn osc11_ignores_other_sequences_before_a_valid_reply() {
        let mut terminal = ScriptedTerminal::new(&[
            b"garbage\x1b]10;rgb:0/0/0\x07\x1b[31m\x1b]11;rgb:f/f/f\x1b\\\x1b[?1c",
        ]);
        assert_eq!(detect_appearance(&mut terminal, None), Appearance::Light);
    }

    #[test]
    fn osc11_does_not_read_bytes_queued_after_da1() {
        let mut terminal = ScriptedTerminal::new(&[
            b"\x1b]11;rgb:f/f/f\x07\x1b[?1;2cnext input\x1b]11;rgb:0/0/0\x07",
        ]);
        assert_eq!(detect_appearance(&mut terminal, None), Appearance::Light);
        assert_eq!(terminal.unread_bytes(), b"next input\x1b]11;rgb:0/0/0\x07");
    }

    #[test]
    fn osc11_write_error_falls_back_without_reading() {
        let reply = b"\x1b]11;rgb:0/0/0\x07\x1b[?1c";
        let mut terminal = ScriptedTerminal::new(&[reply]);
        terminal.write_error = true;
        assert_eq!(
            detect_appearance(&mut terminal, Some("0;15")),
            Appearance::Light
        );
        assert_eq!(terminal.writes, vec![b"\x1b]11;?\x07\x1b[c".to_vec()]);
        assert!(terminal.deadlines.is_empty());
        assert_eq!(terminal.unread_bytes(), reply);
    }

    #[test]
    fn osc11_writes_the_osc11_and_da1_queries_together() {
        let mut terminal = ScriptedTerminal::new(&[]);
        detect_appearance(&mut terminal, None);
        assert_eq!(terminal.writes, vec![b"\x1b]11;?\x07\x1b[c".to_vec()]);
    }

    #[test]
    fn osc11_read_error_falls_back_and_stops_reading() {
        let reply = b"\x1b]11;rgb:0/0/0\x07\x1b[?1c";
        let mut terminal = ScriptedTerminal::new(&[reply]);
        terminal.script.push_front(ScriptedRead::Error);
        assert_eq!(
            detect_appearance(&mut terminal, Some("0;15")),
            Appearance::Light
        );
        assert_eq!(terminal.deadlines.len(), 1);
        assert_eq!(terminal.unread_bytes(), reply);
    }

    #[test]
    fn osc11_timeout_stops_before_a_late_reply_even_after_a_valid_background() {
        let late_reply = b"\x1b[?1c";
        let mut terminal = ScriptedTerminal::new(&[b"\x1b]11;rgb:0/0/0\x07"]);
        terminal.script.push_back(ScriptedRead::Timeout);
        terminal
            .script
            .push_back(ScriptedRead::Bytes(late_reply.iter().copied().collect()));
        assert_eq!(
            detect_appearance(&mut terminal, Some("0;15")),
            Appearance::Light
        );
        assert_eq!(terminal.deadlines.len(), b"\x1b]11;rgb:0/0/0\x07".len() + 1);
        assert_eq!(terminal.unread_bytes(), late_reply);
    }

    #[test]
    fn osc11_parser_boundaries() {
        let mut terminal = ScriptedTerminal::new(&[b"\x1b]11;rgb:ffff/ffff/ffff\x07\x1b[?62;22cx"]);
        assert_eq!(detect_appearance(&mut terminal, None), Appearance::Light);
        assert_eq!(terminal.unread_bytes(), b"x");

        let mut terminal =
            ScriptedTerminal::new(&[b"\x1b[12;40R\x1b[1c\x1b]11;rgb:0/0/0\x07\x1b[?1;2cy"]);
        assert_eq!(
            detect_appearance(&mut terminal, Some("0;15")),
            Appearance::Dark
        );
        assert_eq!(terminal.unread_bytes(), b"y");

        let mut terminal = ScriptedTerminal::new(&[b"\x1b]11;rgb:ffff\x1b[?1cz"]);
        assert_eq!(detect_appearance(&mut terminal, None), Appearance::Dark);
        assert_eq!(terminal.unread_bytes(), b"z");

        let mut terminal = ScriptedTerminal::new(&[
            b"\x1b]11;rgb:fffff/fffff/fffff/fffff\x07\x1b]11;rgb:0/0/0\x07\x1b[?1c",
        ]);
        assert_eq!(
            detect_appearance(&mut terminal, Some("0;15")),
            Appearance::Dark
        );
    }

    #[test]
    fn osc11_malformed_colours_are_rejected_directly() {
        for value in [
            "rgb:/f/f",
            "rgb:fffff/f/f",
            "rgb:gg/f/f",
            "rgb:-1/f/f",
            "rgb:f/f",
            "rgb:f/f/f/f",
            "rgba:f/f/f/f",
            "#ffffff",
        ] {
            assert_eq!(
                super::parse_osc11(format!("11;{value}").as_bytes()),
                None,
                "{value}"
            );
        }
    }

    #[test]
    fn osc11_unparseable_background_falls_back() {
        for value in [
            "rgb:/f/f",
            "rgb:fffff/f/f",
            "rgb:gg/f/f",
            "rgb:-1/f/f",
            "rgb:f/f",
            "rgb:f/f/f/f",
            "rgb:0x0/0/0",
            "#ffffff",
        ] {
            let reply = format!("\x1b]11;{value}\x07\x1b[?1c");
            let mut terminal = ScriptedTerminal::new(&[reply.as_bytes()]);
            assert_eq!(
                detect_appearance(&mut terminal, Some("0;15")),
                Appearance::Light,
                "{value}"
            );
            assert!(terminal.unread_bytes().is_empty());
        }
    }

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
