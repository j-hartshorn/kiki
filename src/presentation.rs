use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::model::ModelProgress;

pub(crate) const PROGRESS_PAYLOAD_LIMIT: usize = 4 * 1024;
pub(crate) const PROGRESS_RATE_LIMIT: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PresentationKind {
    ReasoningSummary,
    AssistantText,
    DecodedPython,
}

impl PresentationKind {
    fn progress(self, text: String) -> ModelProgress {
        match self {
            Self::ReasoningSummary => ModelProgress::ReasoningSummaryChunk { text },
            Self::AssistantText => ModelProgress::AssistantTextChunk { text },
            Self::DecodedPython => ModelProgress::DecodedPythonChunk { code: text },
        }
    }
}

struct PendingPresentation {
    kind: PresentationKind,
    text: String,
    emitted: usize,
}

#[derive(Default)]
pub(crate) struct PresentationBatcher {
    pending: VecDeque<PendingPresentation>,
    pending_bytes: usize,
    last_emission: Option<Instant>,
}

impl PresentationBatcher {
    pub(crate) fn push(
        &mut self,
        kind: PresentationKind,
        text: String,
        now: Instant,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) {
        if text.is_empty() {
            return;
        }
        self.pending_bytes += text.len();
        if let Some(pending) = self
            .pending
            .back_mut()
            .filter(|pending| pending.kind == kind)
        {
            pending.text.push_str(&text);
        } else {
            self.pending.push_back(PendingPresentation {
                kind,
                text,
                emitted: 0,
            });
        }

        if self.rate_window_open(now) {
            self.flush_all_at(now, progress);
        } else {
            self.flush_full_chunks(now, progress);
        }
    }

    pub(crate) fn flush_due(
        &mut self,
        now: Instant,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) {
        if !self.pending.is_empty() && self.rate_window_open(now) {
            self.flush_all_at(now, progress);
        }
    }

    pub(crate) fn flush_all(&mut self, progress: &mut (dyn FnMut(ModelProgress) + Send)) {
        self.flush_all_at(Instant::now(), progress);
    }

    fn flush_all_at(&mut self, now: Instant, progress: &mut (dyn FnMut(ModelProgress) + Send)) {
        while !self.pending.is_empty() {
            self.emit_front(now, progress);
        }
    }

    fn flush_full_chunks(
        &mut self,
        now: Instant,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) {
        while self.pending_bytes >= PROGRESS_PAYLOAD_LIMIT {
            self.emit_front(now, progress);
        }
    }

    fn emit_front(&mut self, now: Instant, progress: &mut (dyn FnMut(ModelProgress) + Send)) {
        let pending = self.pending.front_mut().expect("pending presentation");
        let kind = pending.kind;
        let unemitted = &pending.text[pending.emitted..];
        let chunk = unemitted[..utf8_prefix_end(unemitted, PROGRESS_PAYLOAD_LIMIT)].to_owned();
        pending.emitted += chunk.len();
        if pending.emitted == pending.text.len() {
            self.pending.pop_front();
        }
        self.pending_bytes -= chunk.len();
        self.last_emission = Some(now);
        progress(kind.progress(chunk));
    }

    fn rate_window_open(&self, now: Instant) -> bool {
        self.last_emission
            .is_none_or(|last| now.saturating_duration_since(last) >= PROGRESS_RATE_LIMIT)
    }
}

fn utf8_prefix_end(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[derive(Default)]
pub(crate) struct PythonPreviewDecoder {
    state: PythonJsonState,
}

#[derive(Default)]
enum PythonJsonState {
    #[default]
    LeadingWhitespace,
    BeforeKey,
    Key(usize),
    AfterKey,
    BeforeCode,
    Code,
    Escape,
    Unicode {
        value: u16,
        digits: u8,
    },
    HighSurrogateBackslash {
        high: u16,
    },
    HighSurrogateU {
        high: u16,
    },
    LowSurrogate {
        high: u16,
        value: u16,
        digits: u8,
    },
    AfterCode,
    TrailingWhitespace,
    Invalid,
}

impl PythonPreviewDecoder {
    pub(crate) fn push(&mut self, fragment: &str) -> String {
        let mut decoded = String::new();
        for character in fragment.chars() {
            let state = std::mem::replace(&mut self.state, PythonJsonState::Invalid);
            self.state = match state {
                PythonJsonState::LeadingWhitespace if json_whitespace(character) => {
                    PythonJsonState::LeadingWhitespace
                }
                PythonJsonState::LeadingWhitespace if character == '{' => {
                    PythonJsonState::BeforeKey
                }
                PythonJsonState::BeforeKey if json_whitespace(character) => {
                    PythonJsonState::BeforeKey
                }
                PythonJsonState::BeforeKey if character == '"' => PythonJsonState::Key(0),
                PythonJsonState::Key(index) => {
                    let expected = ['c', 'o', 'd', 'e', '"'];
                    if expected.get(index) == Some(&character) {
                        if index + 1 == expected.len() {
                            PythonJsonState::AfterKey
                        } else {
                            PythonJsonState::Key(index + 1)
                        }
                    } else {
                        PythonJsonState::Invalid
                    }
                }
                PythonJsonState::AfterKey if json_whitespace(character) => {
                    PythonJsonState::AfterKey
                }
                PythonJsonState::AfterKey if character == ':' => PythonJsonState::BeforeCode,
                PythonJsonState::BeforeCode if json_whitespace(character) => {
                    PythonJsonState::BeforeCode
                }
                PythonJsonState::BeforeCode if character == '"' => PythonJsonState::Code,
                PythonJsonState::Code if character == '"' => PythonJsonState::AfterCode,
                PythonJsonState::Code if character == '\\' => PythonJsonState::Escape,
                PythonJsonState::Code if character <= '\u{1f}' => PythonJsonState::Invalid,
                PythonJsonState::Code => {
                    decoded.push(character);
                    PythonJsonState::Code
                }
                PythonJsonState::Escape => match character {
                    '"' | '\\' | '/' => {
                        decoded.push(character);
                        PythonJsonState::Code
                    }
                    'b' => {
                        decoded.push('\u{0008}');
                        PythonJsonState::Code
                    }
                    'f' => {
                        decoded.push('\u{000c}');
                        PythonJsonState::Code
                    }
                    'n' => {
                        decoded.push('\n');
                        PythonJsonState::Code
                    }
                    'r' => {
                        decoded.push('\r');
                        PythonJsonState::Code
                    }
                    't' => {
                        decoded.push('\t');
                        PythonJsonState::Code
                    }
                    'u' => PythonJsonState::Unicode {
                        value: 0,
                        digits: 0,
                    },
                    _ => PythonJsonState::Invalid,
                },
                PythonJsonState::Unicode { value, digits } => match hexadecimal(character) {
                    Some(digit) if digits < 3 => PythonJsonState::Unicode {
                        value: value * 16 + digit,
                        digits: digits + 1,
                    },
                    Some(digit) => {
                        let value = value * 16 + digit;
                        if (0xd800..=0xdbff).contains(&value) {
                            PythonJsonState::HighSurrogateBackslash { high: value }
                        } else if (0xdc00..=0xdfff).contains(&value) {
                            PythonJsonState::Invalid
                        } else if let Some(character) = char::from_u32(u32::from(value)) {
                            decoded.push(character);
                            PythonJsonState::Code
                        } else {
                            PythonJsonState::Invalid
                        }
                    }
                    None => PythonJsonState::Invalid,
                },
                PythonJsonState::HighSurrogateBackslash { high } if character == '\\' => {
                    PythonJsonState::HighSurrogateU { high }
                }
                PythonJsonState::HighSurrogateU { high } if character == 'u' => {
                    PythonJsonState::LowSurrogate {
                        high,
                        value: 0,
                        digits: 0,
                    }
                }
                PythonJsonState::LowSurrogate {
                    high,
                    value,
                    digits,
                } => match hexadecimal(character) {
                    Some(digit) if digits < 3 => PythonJsonState::LowSurrogate {
                        high,
                        value: value * 16 + digit,
                        digits: digits + 1,
                    },
                    Some(digit) => {
                        let low = value * 16 + digit;
                        if (0xdc00..=0xdfff).contains(&low) {
                            let scalar = 0x10000
                                + ((u32::from(high) - 0xd800) << 10)
                                + (u32::from(low) - 0xdc00);
                            if let Some(character) = char::from_u32(scalar) {
                                decoded.push(character);
                                PythonJsonState::Code
                            } else {
                                PythonJsonState::Invalid
                            }
                        } else {
                            PythonJsonState::Invalid
                        }
                    }
                    None => PythonJsonState::Invalid,
                },
                PythonJsonState::AfterCode if json_whitespace(character) => {
                    PythonJsonState::AfterCode
                }
                PythonJsonState::AfterCode if character == '}' => {
                    PythonJsonState::TrailingWhitespace
                }
                PythonJsonState::TrailingWhitespace if json_whitespace(character) => {
                    PythonJsonState::TrailingWhitespace
                }
                PythonJsonState::Invalid => PythonJsonState::Invalid,
                _ => PythonJsonState::Invalid,
            };
        }
        decoded
    }
}

fn json_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\n' | '\r' | '\t')
}

fn hexadecimal(character: char) -> Option<u16> {
    character
        .to_digit(16)
        .and_then(|value| u16::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        PROGRESS_PAYLOAD_LIMIT, PROGRESS_RATE_LIMIT, PresentationBatcher, PresentationKind,
        PythonPreviewDecoder,
    };
    use crate::{config::constants::ACCEPTED_MODEL_CONTENT_LIMIT, model::ModelProgress};

    #[test]
    fn presentation_batcher_emits_pending_content_at_the_next_due_producer_event() {
        let start = Instant::now();
        let mut batcher = PresentationBatcher::default();
        let mut progress = Vec::new();
        {
            let mut sink = |event| progress.push(event);
            batcher.push(
                PresentationKind::AssistantText,
                "first".to_owned(),
                start,
                &mut sink,
            );
            batcher.push(
                PresentationKind::AssistantText,
                "later".to_owned(),
                start + PROGRESS_RATE_LIMIT / 2,
                &mut sink,
            );
            batcher.flush_due(
                start + PROGRESS_RATE_LIMIT - Duration::from_nanos(1),
                &mut sink,
            );
        }
        assert_eq!(
            progress,
            vec![ModelProgress::AssistantTextChunk {
                text: "first".to_owned(),
            }]
        );

        {
            let mut sink = |event| progress.push(event);
            batcher.flush_due(start + PROGRESS_RATE_LIMIT, &mut sink);
        }
        assert_eq!(
            progress,
            vec![
                ModelProgress::AssistantTextChunk {
                    text: "first".to_owned(),
                },
                ModelProgress::AssistantTextChunk {
                    text: "later".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn presentation_batcher_emits_four_kib_before_an_explicit_flush() {
        let start = Instant::now();
        let full_chunk = "x".repeat(PROGRESS_PAYLOAD_LIMIT);
        let mut batcher = PresentationBatcher::default();
        let mut progress = Vec::new();
        let mut sink = |event| progress.push(event);

        batcher.push(
            PresentationKind::AssistantText,
            "first".to_owned(),
            start,
            &mut sink,
        );
        batcher.push(
            PresentationKind::AssistantText,
            full_chunk.clone(),
            start + PROGRESS_RATE_LIMIT / 2,
            &mut sink,
        );

        assert_eq!(
            progress,
            vec![
                ModelProgress::AssistantTextChunk {
                    text: "first".to_owned(),
                },
                ModelProgress::AssistantTextChunk { text: full_chunk },
            ]
        );
    }

    fn assistant_chunks(progress: &[ModelProgress]) -> Vec<&String> {
        progress
            .iter()
            .map(|event| match event {
                ModelProgress::AssistantTextChunk { text } => text,
                other => panic!("unexpected progress: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn presentation_batcher_copies_each_payload_out_of_a_large_push() {
        let large = "x".repeat(32 * PROGRESS_PAYLOAD_LIMIT);
        let mut batcher = PresentationBatcher::default();
        let mut progress = Vec::new();
        {
            let mut sink = |event| progress.push(event);
            batcher.push(
                PresentationKind::AssistantText,
                large.clone(),
                Instant::now(),
                &mut sink,
            );
        }
        let chunks = assistant_chunks(&progress);

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.as_str())
                .collect::<String>(),
            large
        );
        for chunk in chunks {
            assert!(
                chunk.capacity() <= PROGRESS_PAYLOAD_LIMIT,
                "{}",
                chunk.capacity()
            );
        }
    }

    #[test]
    fn presentation_batcher_streams_a_content_limit_delta_in_bounded_utf8_payloads() {
        let start = Instant::now();
        let delta = "é€😀".repeat(ACCEPTED_MODEL_CONTENT_LIMIT / "é€😀".len());
        let mut batcher = PresentationBatcher::default();
        let mut progress = Vec::new();
        {
            let mut sink = |event| progress.push(event);
            batcher.push(
                PresentationKind::AssistantText,
                "first".to_owned(),
                start,
                &mut sink,
            );
            batcher.push(
                PresentationKind::AssistantText,
                delta.clone(),
                start + PROGRESS_RATE_LIMIT / 2,
                &mut sink,
            );
            batcher.flush_all(&mut sink);
        }
        let chunks = assistant_chunks(&progress);

        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.as_str())
                .collect::<String>(),
            format!("first{delta}")
        );
        for chunk in &chunks[1..chunks.len() - 1] {
            assert!(chunk.len() > PROGRESS_PAYLOAD_LIMIT - 4, "{}", chunk.len());
        }
        for chunk in chunks {
            assert!(
                chunk.capacity() <= PROGRESS_PAYLOAD_LIMIT,
                "{}",
                chunk.capacity()
            );
        }
    }

    #[test]
    fn python_preview_decoder_decodes_split_escapes_surrogates_and_utf8() {
        let mut decoder = PythonPreviewDecoder::default();
        let fragments = [
            " \n { \"co",
            "de\" \t: \"quote: \\",
            "\" newline:\\",
            "n lambda:\\u03",
            "BB emoji:\\uD83D\\",
            "uDE00 utf8:é",
            "\" } \t",
        ];

        let decoded = fragments
            .iter()
            .map(|fragment| decoder.push(fragment))
            .collect::<String>();

        assert_eq!(decoded, "quote: \" newline:\n lambda:λ emoji:😀 utf8:é");
    }

    #[test]
    fn python_preview_decoder_stops_at_an_unexpected_key_or_malformed_escape() {
        for arguments in [r#"{"other":"ignored"}"#, r#"{"code":"ok\qignored"}"#] {
            let mut decoder = PythonPreviewDecoder::default();
            let decoded = decoder.push(arguments);
            assert!(!decoded.contains("ignored"), "{arguments}: {decoded}");
        }
    }
}
