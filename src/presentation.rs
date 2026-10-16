use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::{
    config::constants::ACCEPTED_MODEL_CONTENT_LIMIT,
    model::{ModelError, ModelProgress},
};

pub(crate) const PROGRESS_PAYLOAD_LIMIT: usize = 4 * 1024;
pub(crate) const PROGRESS_RATE_LIMIT: Duration = Duration::from_millis(50);
// Room for 16 MiB of content escaped as `\u00XX` (6 bytes per byte) in one frame.
pub(crate) const SSE_FRAME_LIMIT: usize = 128 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct ContentBudget(usize);

impl ContentBudget {
    pub(crate) fn spend(&mut self, bytes: usize) -> Result<(), ModelError> {
        self.0 = self
            .0
            .checked_add(bytes)
            .filter(|&used| used <= ACCEPTED_MODEL_CONTENT_LIMIT)
            .ok_or(ModelError::ContentTooLarge)?;
        Ok(())
    }
}

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

pub(crate) struct SseFrameLimit {
    limit: usize,
    partial_line: Vec<u8>,
    frame_bytes: usize,
    after_cr: bool,
}

impl SseFrameLimit {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            partial_line: Vec::new(),
            frame_bytes: 0,
            after_cr: false,
        }
    }

    // eventsource-stream re-parses partial lines per chunk and stalls on a trailing CR: emit whole LF lines.
    pub(crate) fn complete_lines(&mut self, chunk: &[u8]) -> Result<Vec<u8>, ModelError> {
        let mut lines = Vec::new();
        for &byte in chunk {
            let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
            match byte {
                // Only a blank-line CR leaves the count at zero.
                b'\n' if after_cr && self.frame_bytes == 0 => continue,
                b'\n' if after_cr => {}
                b'\r' | b'\n' if self.partial_line.is_empty() => {
                    self.frame_bytes = 0;
                    lines.push(b'\n');
                    continue;
                }
                b'\r' | b'\n' => {
                    lines.append(&mut self.partial_line);
                    lines.push(b'\n');
                }
                _ => self.partial_line.push(byte),
            }
            self.frame_bytes += 1;
            if self.frame_bytes > self.limit {
                return Err(ModelError::ContentTooLarge);
            }
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use serde_json::json;

    use super::{
        PROGRESS_PAYLOAD_LIMIT, PROGRESS_RATE_LIMIT, PresentationBatcher, PresentationKind,
        PythonPreviewDecoder, SseFrameLimit,
    };
    use crate::{
        config::constants::ACCEPTED_MODEL_CONTENT_LIMIT,
        model::{ModelError, ModelProgress},
    };

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

    const SMALL_FRAME_LIMIT: usize = 1024;

    fn ignored_frame(padding: &str) -> String {
        let event = json!({"type": "future_event", "padding": padding});
        format!("event: future_event\ndata: {event}\n\n")
    }

    fn error_frame(message: &str) -> String {
        let event = json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": message}
        });
        format!("event: error\ndata: {event}\n\n")
    }

    fn padded(frame: fn(&str) -> String, frame_bytes: usize) -> String {
        // The closing blank line resets the count instead of adding to it.
        let framing = frame("").len() - 1;
        frame(&"x".repeat(frame_bytes - framing))
    }

    #[tokio::test]
    async fn sse_frame_limit_keeps_a_crlf_split_across_chunks_as_one_line_ending() {
        let mut frames = SseFrameLimit::new(SMALL_FRAME_LIMIT);
        let chunks: [&[u8]; 3] = [
            b"event: ping\r",
            b"\ndata: {\"type\":\"ping\"}\r",
            b"\n\r\n",
        ];

        let events = futures::stream::iter(chunks)
            .map(|chunk| frames.complete_lines(chunk))
            .eventsource()
            .map(|event| event.map(|event| (event.event, event.data)))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(
            events,
            [Ok(("ping".to_owned(), r#"{"type":"ping"}"#.to_owned()))]
        );
    }

    #[test]
    fn sse_frame_limit_accepts_ignored_and_error_frames_at_the_limit_but_not_one_byte_over() {
        for frame in [ignored_frame, error_frame] {
            let at_limit = padded(frame, SMALL_FRAME_LIMIT);
            let over_limit = padded(frame, SMALL_FRAME_LIMIT + 1);

            assert_eq!(
                SseFrameLimit::new(SMALL_FRAME_LIMIT).complete_lines(at_limit.as_bytes()),
                Ok(at_limit.into_bytes())
            );
            assert_eq!(
                SseFrameLimit::new(SMALL_FRAME_LIMIT).complete_lines(over_limit.as_bytes()),
                Err(ModelError::ContentTooLarge)
            );
        }
    }

    #[test]
    fn sse_frame_limit_charges_both_bytes_of_crlf_frame_lines_in_whole_and_byte_chunks() {
        let crlf_frame = |frame_bytes: usize| {
            let framing = "event: ping\r\ndata: \r\n".len();
            format!(
                "event: ping\r\ndata: {}\r\n\r\n",
                "x".repeat(frame_bytes - framing)
            )
        };
        let at_limit = crlf_frame(SMALL_FRAME_LIMIT).repeat(2);
        let over_limit = crlf_frame(SMALL_FRAME_LIMIT + 1);
        for chunk_size in [1, usize::MAX] {
            let lines = |body: &str| {
                let mut frames = SseFrameLimit::new(SMALL_FRAME_LIMIT);
                body.as_bytes()
                    .chunks(chunk_size)
                    .map(|chunk| frames.complete_lines(chunk))
                    .collect::<Result<Vec<_>, _>>()
                    .map(|lines| lines.concat())
            };

            assert_eq!(
                lines(&at_limit),
                Ok(at_limit.replace("\r\n", "\n").into_bytes()),
                "{chunk_size}-byte chunks"
            );
            assert_eq!(
                lines(&over_limit),
                Err(ModelError::ContentTooLarge),
                "{chunk_size}-byte chunks"
            );
        }
    }

    #[test]
    fn sse_frame_limit_emits_each_line_ending_as_one_lf_and_resets_at_blank_lines_across_chunks() {
        for line_end in ["\n", "\r", "\r\n"] {
            let frame = format!("xxx{line_end}xxx{line_end}");
            let body = format!("{frame}{line_end}{frame}{line_end}{frame}");
            for chunk_size in [1, 2, 3, body.len()] {
                let mut frames = SseFrameLimit::new(frame.len());

                let lines = body
                    .as_bytes()
                    .chunks(chunk_size)
                    .map(|chunk| frames.complete_lines(chunk))
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();

                assert!(
                    lines
                        .iter()
                        .all(|lines| lines.last().is_none_or(|&byte| byte == b'\n'))
                );
                assert_eq!(
                    lines.concat(),
                    body.replace(line_end, "\n").into_bytes(),
                    "{line_end:?} in {chunk_size}-byte chunks"
                );
                assert_eq!(
                    frames.complete_lines(b"x"),
                    Err(ModelError::ContentTooLarge)
                );
            }
        }
    }
}
