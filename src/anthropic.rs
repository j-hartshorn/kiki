use std::future::Future;
use std::time::Instant;

use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ModelSelection, constants::ACCEPTED_MODEL_CONTENT_LIMIT, models::Provider},
    model::{
        AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelContextEntry,
        ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep, ModelUsage,
        OpaqueReplayItem, ProviderFailure, ProviderReplay, canonical_compaction_summary,
    },
    presentation::{PresentationBatcher, PresentationKind, PythonPreviewDecoder},
};

const ANTHROPIC_PROVIDER: &str = "anthropic";
const ANTHROPIC_API_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const EXECUTE_PYTHON: &str = "execute_python";
const ERROR_BODY_LIMIT: usize = 64 * 1024;
// Room for 16 MiB of content escaped as `\u00XX` (6 bytes per byte) in one frame.
const SSE_FRAME_LIMIT: usize = 128 * 1024 * 1024;

pub(crate) struct AnthropicModel {
    prompt: String,
    api_key: String,
    endpoint: String,
    client: reqwest::Client,
    frame_limit: usize,
}

impl AnthropicModel {
    pub(crate) fn new(prompt: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_endpoint(prompt, api_key, ANTHROPIC_API_BASE)
    }

    fn with_endpoint(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        api_base: &str,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            api_key: api_key.into(),
            endpoint: format!("{api_base}/v1/messages"),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("Anthropic HTTP client"),
            frame_limit: SSE_FRAME_LIMIT,
        }
    }

    fn request(
        &self,
        selection: ModelSelection,
        history: &ModelHistory,
    ) -> Result<Value, ModelError> {
        let model = selection.model();
        if model.provider() != Provider::Anthropic {
            return Err(ModelError::InvalidSelection("unsupported Anthropic model"));
        }
        let mut messages = Vec::new();

        for entry in history.entries() {
            match entry {
                ModelContextEntry::UserRequest { text } => {
                    push_content(&mut messages, "user", [text_content(text)]);
                }
                ModelContextEntry::CompactionSummary { summary } => {
                    let summary = canonical_compaction_summary(summary);
                    push_content(&mut messages, "user", [text_content(&summary)]);
                }
                ModelContextEntry::CompletedCode(completed) => {
                    let blocks = replay_blocks(completed.replay())?;
                    match replay_action(&blocks)? {
                        ModelAction::Code {
                            source,
                            call_id,
                            commentary,
                        } if source == completed.source()
                            && call_id == *completed.call_id()
                            && commentary.as_deref() == completed.commentary() => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "completed code does not match its Anthropic replay",
                            ));
                        }
                    }
                    push_content(&mut messages, "assistant", block_values(&blocks));
                    let output = match completed.result() {
                        CodeResult::Execution { feedback }
                        | CodeResult::Cancelled { feedback }
                        | CodeResult::Failure { feedback } => feedback,
                    };
                    push_content(
                        &mut messages,
                        "user",
                        [json!({
                            "type": "tool_result",
                            "tool_use_id": completed.call_id().as_str(),
                            "content": output
                        })],
                    );
                }
                ModelContextEntry::FinalMessage {
                    text,
                    reasoning_summary: _,
                    replay,
                } => {
                    let blocks = replay_blocks(replay)?;
                    match replay_action(&blocks)? {
                        ModelAction::Message { text: replay_text } if replay_text == *text => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "final message does not match its Anthropic replay",
                            ));
                        }
                    }
                    push_content(&mut messages, "assistant", block_values(&blocks));
                }
            }
        }

        Ok(json!({
            "model": model.wire_id(),
            "max_tokens": model.max_output_tokens,
            "stream": true,
            "system": [text_content(&self.prompt)],
            "messages": messages,
            "tools": [{
                "name": EXECUTE_PYTHON,
                "strict": true,
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"],
                    "additionalProperties": false
                }
            }],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": selection.thinking()}
        }))
    }

    async fn step_once(
        &self,
        selection: ModelSelection,
        history: &ModelHistory,
        cancellation: &CancellationToken,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<ModelStep, ModelFailure> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        let request = self.request(selection, history)?;
        progress(ModelProgress::AttemptStarted { attempt: 1 });

        let mut presentation = PresentationBatcher::default();
        let streamed = self
            .stream(
                &request,
                selection.model().wire_id(),
                cancellation,
                &mut presentation,
                progress,
            )
            .await;
        presentation.flush_all(progress);
        let (result, accounting) = match streamed {
            Ok(message) => {
                let accounting =
                    AttemptAccounting::KnownUsage(self.usage(&message.usage, selection));
                (self.completed_step(history, message), accounting)
            }
            Err(error) => (Err(error), AttemptAccounting::UsageUnavailable),
        };
        progress(ModelProgress::AttemptSettled {
            attempt: 1,
            accounting: accounting.clone(),
        });

        match result {
            Ok(mut step) => {
                step.attempts = vec![accounting];
                Ok(step)
            }
            Err(error) => Err(ModelFailure::new(error, vec![accounting])),
        }
    }

    async fn stream(
        &self,
        request: &Value,
        model: &str,
        cancellation: &CancellationToken,
        presentation: &mut PresentationBatcher,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<StreamedMessage, ModelError> {
        let send = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(request)
            .send();
        let response = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
            response = send => response,
        }
        .map_err(|_| self.provider_error(None, None, "provider transport failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self
                .http_failure(status.as_u16(), response, cancellation)
                .await);
        }

        let mut frames = SseFrameLimit::new(self.frame_limit);
        let chunks = response.bytes_stream().map(move |chunk| match chunk {
            Ok(chunk) => frames.complete_lines(&chunk),
            Err(_) => Err(self.provider_error(None, None, "provider stream failed")),
        });
        let mut events = std::pin::pin!(chunks.eventsource());
        let mut message = StreamedMessage::default();
        loop {
            let event = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
                event = events.next() => event,
            };
            let event = match event {
                None => {
                    return Err(self.provider_error(
                        None,
                        None,
                        "provider stream ended without a terminal response",
                    ));
                }
                Some(Err(EventStreamError::Transport(error))) => return Err(error),
                Some(Err(_)) => {
                    return Err(ModelError::InvalidResponse(
                        "stream was not valid server-sent events",
                    ));
                }
                Some(Ok(event)) => event,
            };
            let event = serde_json::from_str(&event.data)
                .map_err(|_| ModelError::InvalidResponse("stream contained a malformed event"))?;
            let now = Instant::now();
            match event {
                WireEvent::Error { error } => {
                    return Err(self.provider_error(None, Some(error.kind), error.message));
                }
                WireEvent::Ping | WireEvent::Unexpected => {}
                WireEvent::MessageStart { .. } if message.started => {
                    return Err(ModelError::InvalidResponse("stream repeated message start"));
                }
                WireEvent::MessageStart { message: start } => {
                    if start.role != "assistant" {
                        return Err(ModelError::InvalidResponse(
                            "message start was not an assistant message",
                        ));
                    }
                    if start.model != model {
                        return Err(ModelError::InvalidResponse(
                            "message start did not echo the selected model",
                        ));
                    }
                    message.started = true;
                    if let Some(usage) = start.usage {
                        message.usage.update(usage);
                    }
                }
                _ if !message.started => {
                    return Err(ModelError::InvalidResponse(
                        "stream content arrived before message start",
                    ));
                }
                WireEvent::MessageStop if message.open.is_some() => {
                    return Err(ModelError::InvalidResponse(
                        "stream stopped inside a content block",
                    ));
                }
                WireEvent::MessageStop => return Ok(message),
                WireEvent::MessageDelta { delta, usage } => {
                    message.stop_reason = delta.stop_reason.or(message.stop_reason.take());
                    if let Some(usage) = usage {
                        message.usage.update(usage);
                    }
                }
                WireEvent::ContentBlockStart {
                    index,
                    content_block,
                } => message.start_block(index, content_block, now, presentation, progress)?,
                WireEvent::ContentBlockDelta { index, delta } => {
                    message.apply_delta(index, delta, now, presentation, progress)?;
                }
                WireEvent::ContentBlockStop { index } => message.stop_block(index)?,
            }
            presentation.flush_due(now, progress);
        }
    }

    async fn http_failure(
        &self,
        status: u16,
        mut response: reqwest::Response,
        cancellation: &CancellationToken,
    ) -> ModelError {
        let body = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return ModelError::Cancelled,
            body = bounded_body(&mut response) => body,
        };
        match body.and_then(|body| serde_json::from_slice::<WireErrorBody>(&body).ok()) {
            Some(WireErrorBody { error }) => {
                self.provider_error(Some(status), Some(error.kind), error.message)
            }
            None => self.provider_error(Some(status), None, "provider request failed"),
        }
    }

    fn completed_step(
        &self,
        history: &ModelHistory,
        message: StreamedMessage,
    ) -> Result<ModelStep, ModelError> {
        let stop_reason = match message.stop_reason.as_deref() {
            Some(reason @ ("tool_use" | "end_turn")) => reason,
            Some("refusal") => {
                return Err(self.provider_error(
                    None,
                    Some("refusal".to_owned()),
                    "model refused the request",
                ));
            }
            Some(reason) => {
                return Err(self.provider_error(
                    None,
                    Some(reason.to_owned()),
                    format!("response incomplete: {reason}"),
                ));
            }
            None => {
                return Err(ModelError::InvalidResponse(
                    "response did not include a stop reason",
                ));
            }
        };
        let action = blocks_action(&message.blocks)?;
        match (&action, stop_reason) {
            (ModelAction::Code { call_id, .. }, "tool_use") => {
                if history.contains_call_id(call_id) {
                    return Err(ModelError::InvalidResponse(
                        "tool call identifier duplicates model history",
                    ));
                }
            }
            (ModelAction::Message { .. }, "end_turn") => {}
            _ => {
                return Err(ModelError::InvalidResponse(
                    "stop reason did not match the response content",
                ));
            }
        }
        let reasoning_summary = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                _ => None,
            })
            .collect();
        let replay = ProviderReplay::new(
            ANTHROPIC_PROVIDER,
            message
                .blocks
                .iter()
                .map(|block| {
                    OpaqueReplayItem::new(serde_json::to_vec(block).expect("content block JSON"))
                })
                .collect(),
        );
        Ok(ModelStep {
            action,
            reasoning_summary,
            replay,
            attempts: Vec::new(),
        })
    }

    fn usage(&self, usage: &UsageTotals, selection: ModelSelection) -> ModelUsage {
        let input = [usage.input, usage.cache_read, usage.cache_creation]
            .into_iter()
            .try_fold(0_u64, |total, tokens| total.checked_add(tokens?));
        ModelUsage {
            model: selection.model().id.to_owned(),
            service_tier: usage
                .service_tier
                .as_deref()
                .map(|tier| redact(tier, &self.api_key)),
            input_tokens: input,
            cached_input_tokens: usage.cache_read,
            cache_write_input_tokens: usage.cache_creation,
            cache_write_1h_input_tokens: usage.cache_creation_1h.unwrap_or(0),
            output_tokens: usage.output,
            reasoning_output_tokens: usage.thinking,
            total_tokens: input
                .zip(usage.output)
                .and_then(|(input, output)| input.checked_add(output)),
        }
    }

    fn provider_error(
        &self,
        status: Option<u16>,
        code: Option<String>,
        message: impl Into<String>,
    ) -> ModelError {
        ModelError::Provider(Box::new(ProviderFailure::new(
            status,
            code.map(|code| redact(&code, &self.api_key)),
            redact(&message.into(), &self.api_key),
        )))
    }
}

impl Model for AnthropicModel {
    fn validate_history(
        &self,
        history: &ModelHistory,
        selection: ModelSelection,
    ) -> Result<(), ModelError> {
        self.request(selection, history).map(|_| ())
    }

    fn step<'a>(
        &'a mut self,
        selection: ModelSelection,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a {
        self.step_once(selection, history, cancellation, progress)
    }
}

fn redact(value: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        value.to_owned()
    } else {
        value.replace(api_key, "[REDACTED]")
    }
}

async fn bounded_body(response: &mut reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > ERROR_BODY_LIMIT {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

fn text_content(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

fn push_content(messages: &mut Vec<Value>, role: &str, content: impl IntoIterator<Item = Value>) {
    match messages.last_mut() {
        Some(last) if last["role"] == role => last["content"]
            .as_array_mut()
            .expect("message content is an array")
            .extend(content),
        _ => messages.push(json!({
            "role": role,
            "content": content.into_iter().collect::<Vec<_>>()
        })),
    }
}

fn block_values(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .map(|block| serde_json::to_value(block).expect("content block JSON"))
        .collect()
}

fn replay_blocks(replay: &ProviderReplay) -> Result<Vec<ContentBlock>, ModelError> {
    if replay.provider() != ANTHROPIC_PROVIDER {
        return Err(ModelError::InvalidHistory(
            "opaque replay belongs to another provider",
        ));
    }
    replay
        .items()
        .iter()
        .map(|item| {
            serde_json::from_slice(item.as_bytes())
                .map_err(|_| ModelError::InvalidHistory("opaque Anthropic replay is malformed"))
        })
        .collect()
}

fn replay_action(blocks: &[ContentBlock]) -> Result<ModelAction, ModelError> {
    blocks_action(blocks).map_err(|error| match error {
        ModelError::InvalidResponse(message) => ModelError::InvalidHistory(message),
        error => error,
    })
}

fn blocks_action(blocks: &[ContentBlock]) -> Result<ModelAction, ModelError> {
    let mut text = String::new();
    let mut tool_use = None;
    for block in blocks {
        match block {
            ContentBlock::Text { text: part } => text.push_str(part),
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
            ContentBlock::ToolUse { id, name, input } => {
                if tool_use.is_some() {
                    return Err(ModelError::InvalidResponse(
                        "response contained multiple tool calls",
                    ));
                }
                if name != EXECUTE_PYTHON {
                    return Err(ModelError::InvalidResponse(
                        "response called an unknown tool",
                    ));
                }
                if id.is_empty() {
                    return Err(ModelError::InvalidResponse(
                        "tool call identifier was empty",
                    ));
                }
                tool_use = Some((id, input));
            }
        }
    }
    match tool_use {
        Some((id, input)) => Ok(ModelAction::Code {
            source: input.code.clone(),
            call_id: ModelCallId::new(id.as_str()),
            commentary: (!text.is_empty()).then_some(text),
        }),
        None if text.is_empty() => Err(ModelError::InvalidResponse("assistant message was empty")),
        None => Ok(ModelAction::Message { text }),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: ExecutePythonInput,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutePythonInput {
    code: String,
}

enum OpenBlock {
    Text(String),
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking(String),
    ToolUse {
        id: String,
        name: String,
        arguments: String,
        preview: Option<PythonPreviewDecoder>,
    },
}

impl OpenBlock {
    fn finish(self) -> Result<ContentBlock, ModelError> {
        Ok(match self {
            Self::Text(text) => ContentBlock::Text { text },
            Self::Thinking {
                thinking,
                signature,
            } => ContentBlock::Thinking {
                thinking,
                signature,
            },
            Self::RedactedThinking(data) => ContentBlock::RedactedThinking { data },
            Self::ToolUse {
                id,
                name,
                arguments,
                ..
            } => {
                if name != EXECUTE_PYTHON {
                    return Err(ModelError::InvalidResponse(
                        "response called an unknown tool",
                    ));
                }
                let input = serde_json::from_str(&arguments).map_err(|_| {
                    ModelError::InvalidResponse(
                        "execute_python arguments were not a closed code object",
                    )
                })?;
                ContentBlock::ToolUse { id, name, input }
            }
        })
    }
}

#[derive(Default)]
struct ContentBudget(usize);

impl ContentBudget {
    fn spend(&mut self, bytes: usize) -> Result<(), ModelError> {
        self.0 = self
            .0
            .checked_add(bytes)
            .filter(|&used| used <= ACCEPTED_MODEL_CONTENT_LIMIT)
            .ok_or(ModelError::ContentTooLarge)?;
        Ok(())
    }
}

struct SseFrameLimit {
    limit: usize,
    partial_line: Vec<u8>,
    frame_bytes: usize,
    after_cr: bool,
}

impl SseFrameLimit {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            partial_line: Vec::new(),
            frame_bytes: 0,
            after_cr: false,
        }
    }

    // eventsource-stream re-parses partial lines per chunk and stalls on a trailing CR: emit whole LF lines.
    fn complete_lines(&mut self, chunk: &[u8]) -> Result<Vec<u8>, ModelError> {
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

#[derive(Default)]
struct UsageTotals {
    input: Option<u64>,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    cache_creation_1h: Option<u64>,
    output: Option<u64>,
    thinking: Option<u64>,
    service_tier: Option<String>,
}

impl UsageTotals {
    fn update(&mut self, usage: WireUsage) {
        self.input = usage.input_tokens.or(self.input);
        self.cache_read = usage.cache_read_input_tokens.or(self.cache_read);
        self.cache_creation = usage.cache_creation_input_tokens.or(self.cache_creation);
        self.cache_creation_1h = usage
            .cache_creation
            .and_then(|creation| creation.ephemeral_1h_input_tokens)
            .or(self.cache_creation_1h);
        self.output = usage.output_tokens.or(self.output);
        self.thinking = usage
            .output_tokens_details
            .and_then(|details| details.thinking_tokens)
            .or(self.thinking);
        self.service_tier = usage.service_tier.or(self.service_tier.take());
    }
}

#[derive(Default)]
struct StreamedMessage {
    started: bool,
    usage: UsageTotals,
    blocks: Vec<ContentBlock>,
    open: Option<OpenBlock>,
    stop_reason: Option<String>,
    content: ContentBudget,
}

impl StreamedMessage {
    fn start_block(
        &mut self,
        index: usize,
        block: WireBlockStart,
        now: Instant,
        presentation: &mut PresentationBatcher,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<(), ModelError> {
        if self.open.is_some() || index != self.blocks.len() {
            return Err(ModelError::InvalidResponse(
                "stream content blocks were out of order",
            ));
        }
        self.open = Some(match block {
            WireBlockStart::Text { text } => {
                self.content.spend(text.len())?;
                presentation.push(PresentationKind::AssistantText, text.clone(), now, progress);
                OpenBlock::Text(text)
            }
            WireBlockStart::Thinking {
                thinking,
                signature,
            } => {
                self.content.spend(thinking.len())?;
                self.content.spend(signature.len())?;
                presentation.push(
                    PresentationKind::ReasoningSummary,
                    thinking.clone(),
                    now,
                    progress,
                );
                OpenBlock::Thinking {
                    thinking,
                    signature,
                }
            }
            WireBlockStart::RedactedThinking { data } => {
                self.content.spend(data.len())?;
                OpenBlock::RedactedThinking(data)
            }
            WireBlockStart::ToolUse { id, name } => {
                self.content.spend(id.len())?;
                self.content.spend(name.len())?;
                let preview = (name == EXECUTE_PYTHON).then(PythonPreviewDecoder::default);
                OpenBlock::ToolUse {
                    id,
                    name,
                    arguments: String::new(),
                    preview,
                }
            }
            WireBlockStart::Unexpected => {
                return Err(ModelError::InvalidResponse(
                    "stream contained an unexpected content block",
                ));
            }
        });
        Ok(())
    }

    fn apply_delta(
        &mut self,
        index: usize,
        delta: WireDelta,
        now: Instant,
        presentation: &mut PresentationBatcher,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<(), ModelError> {
        let open = self
            .open
            .as_mut()
            .filter(|_| index == self.blocks.len())
            .ok_or(ModelError::InvalidResponse(
                "stream delta did not match an open content block",
            ))?;
        match (open, delta) {
            (OpenBlock::Text(text), WireDelta::TextDelta { text: delta }) => {
                self.content.spend(delta.len())?;
                text.push_str(&delta);
                presentation.push(PresentationKind::AssistantText, delta, now, progress);
            }
            (
                OpenBlock::Thinking { thinking, .. },
                WireDelta::ThinkingDelta { thinking: delta },
            ) => {
                self.content.spend(delta.len())?;
                thinking.push_str(&delta);
                presentation.push(PresentationKind::ReasoningSummary, delta, now, progress);
            }
            (
                OpenBlock::Thinking { signature, .. },
                WireDelta::SignatureDelta { signature: delta },
            ) => {
                self.content.spend(delta.len())?;
                signature.push_str(&delta);
            }
            (
                OpenBlock::ToolUse {
                    arguments, preview, ..
                },
                WireDelta::InputJsonDelta { partial_json },
            ) => {
                self.content.spend(partial_json.len())?;
                if let Some(preview) = preview {
                    let code = preview.push(&partial_json);
                    presentation.push(PresentationKind::DecodedPython, code, now, progress);
                }
                arguments.push_str(&partial_json);
            }
            _ => {
                return Err(ModelError::InvalidResponse(
                    "stream delta did not match its content block",
                ));
            }
        }
        Ok(())
    }

    fn stop_block(&mut self, index: usize) -> Result<(), ModelError> {
        let open = self
            .open
            .take()
            .filter(|_| index == self.blocks.len())
            .ok_or(ModelError::InvalidResponse(
                "stream stop did not match an open content block",
            ))?;
        self.blocks.push(open.finish()?);
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    MessageStart {
        message: WireMessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: WireBlockStart,
    },
    ContentBlockDelta {
        index: usize,
        delta: WireDelta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: WireMessageDelta,
        usage: Option<WireUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: WireError,
    },
    #[serde(other)]
    Unexpected,
}

#[derive(Deserialize)]
struct WireMessageStart {
    role: String,
    model: String,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireMessageDelta {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlockStart {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
    },
    #[serde(other)]
    Unexpected,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unexpected,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<WireCacheCreation>,
    output_tokens: Option<u64>,
    output_tokens_details: Option<WireOutputTokensDetails>,
    service_tier: Option<String>,
}

#[derive(Deserialize)]
struct WireCacheCreation {
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireOutputTokensDetails {
    thinking_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireErrorBody {
    error: WireError,
}

#[derive(Deserialize)]
struct WireError {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::{AnthropicModel, SSE_FRAME_LIMIT, SseFrameLimit};
    use crate::{
        config::{
            ModelSelection,
            constants::{
                ACCEPTED_MODEL_CONTENT_LIMIT, PROVIDER_CODE_LIMIT, PROVIDER_MESSAGE_LIMIT,
            },
            models::{OPUS_5_5, SOL},
        },
        model::{
            AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelError,
            ModelFailure, ModelHistory, ModelProgress, ModelStep, ModelUsage, OpaqueReplayItem,
            ProviderReplay, canonical_compaction_summary,
        },
        sse_fixture::{Fixture, FixtureReply},
    };

    const KEY: &str = "fixture-key";
    const HOSTILE_KEY: &str = "sk-ant-hostile-credential";
    const METADATA: &str = "req_hostile_metadata";
    const TOOL_OVERHEAD: usize = "toolu_1".len() + "execute_python".len() + r#"{"code":""}"#.len();
    const IDLE_STREAM: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"role\":\"assistant\",\"model\":\"claude-opus-5-5\",\"usage\":{\"input_tokens\":1}}}\n\n";

    fn selection() -> ModelSelection {
        ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap()
    }

    fn history(request: &str) -> ModelHistory {
        let mut history = ModelHistory::new();
        history.append_user_request(request);
        history
    }

    fn start_usage() -> Value {
        json!({
            "input_tokens": 12,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 0,
                "ephemeral_1h_input_tokens": 0
            },
            "output_tokens": 1,
            "service_tier": "standard"
        })
    }

    fn final_usage() -> Value {
        json!({
            "output_tokens": 40,
            "output_tokens_details": {"thinking_tokens": 25}
        })
    }

    fn expected_accounting() -> AttemptAccounting {
        AttemptAccounting::KnownUsage(ModelUsage {
            model: "anthropic/claude-opus-5-5".to_owned(),
            service_tier: Some("standard".to_owned()),
            input_tokens: Some(12),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(40),
            reasoning_output_tokens: Some(25),
            total_tokens: Some(52),
        })
    }

    fn message_start(usage: Value) -> Value {
        json!({
            "type": "message_start",
            "message": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": "claude-opus-5-5",
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage
            }
        })
    }

    fn block_start(index: usize, block: Value) -> Value {
        json!({"type": "content_block_start", "index": index, "content_block": block})
    }

    fn delta(index: usize, delta: Value) -> Value {
        json!({"type": "content_block_delta", "index": index, "delta": delta})
    }

    fn block_stop(index: usize) -> Value {
        json!({"type": "content_block_stop", "index": index})
    }

    fn thinking(index: usize, parts: &[&str], signature: &str) -> Vec<Value> {
        let mut events = vec![block_start(
            index,
            json!({"type": "thinking", "thinking": ""}),
        )];
        events.extend(
            parts
                .iter()
                .map(|part| delta(index, json!({"type": "thinking_delta", "thinking": part}))),
        );
        events.push(delta(
            index,
            json!({"type": "signature_delta", "signature": signature}),
        ));
        events.push(block_stop(index));
        events
    }

    fn text(index: usize, text: &str) -> Vec<Value> {
        vec![
            block_start(index, json!({"type": "text", "text": ""})),
            delta(index, json!({"type": "text_delta", "text": text})),
            block_stop(index),
        ]
    }

    fn tool_use(index: usize, id: &str, fragments: &[&str]) -> Vec<Value> {
        let mut events = vec![block_start(
            index,
            json!({"type": "tool_use", "id": id, "name": "execute_python", "input": {}}),
        )];
        events.extend(fragments.iter().map(|fragment| {
            delta(
                index,
                json!({"type": "input_json_delta", "partial_json": fragment}),
            )
        }));
        events.push(block_stop(index));
        events
    }

    fn sse(events: impl IntoIterator<Item = Value>) -> String {
        events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn stream(
        start_usage: Value,
        blocks: Vec<Vec<Value>>,
        stop_reason: &str,
        final_usage: Value,
    ) -> String {
        let mut events = vec![message_start(start_usage), json!({"type": "ping"})];
        events.extend(blocks.into_iter().flatten());
        events.push(json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": final_usage
        }));
        events.push(json!({"type": "message_stop"}));
        sse(events)
    }

    fn tool_stream() -> String {
        stream(
            start_usage(),
            vec![
                thinking(0, &["Checking ", "the file"], "sig-1"),
                text(1, "I'll run it."),
                tool_use(2, "toolu_1", &[r#"{"code":"pri"#, r#"nt(1)"}"#]),
            ],
            "tool_use",
            final_usage(),
        )
    }

    fn message_stream() -> String {
        stream(
            start_usage(),
            vec![
                vec![
                    block_start(
                        0,
                        json!({"type": "redacted_thinking", "data": "opaque-data"}),
                    ),
                    block_stop(0),
                ],
                thinking(1, &["Summarising"], "sig-2"),
                text(2, "Done."),
            ],
            "end_turn",
            final_usage(),
        )
    }

    fn tool_blocks() -> Vec<Value> {
        vec![
            json!({"type": "thinking", "thinking": "Checking the file", "signature": "sig-1"}),
            json!({"type": "text", "text": "I'll run it."}),
            json!({
                "type": "tool_use",
                "id": "toolu_1",
                "name": "execute_python",
                "input": {"code": "print(1)"}
            }),
        ]
    }

    fn message_blocks() -> Vec<Value> {
        vec![
            json!({"type": "redacted_thinking", "data": "opaque-data"}),
            json!({"type": "thinking", "thinking": "Summarising", "signature": "sig-2"}),
            json!({"type": "text", "text": "Done."}),
        ]
    }

    fn replay(blocks: &[Value]) -> ProviderReplay {
        ProviderReplay::new(
            "anthropic",
            blocks
                .iter()
                .map(|block| OpaqueReplayItem::new(serde_json::to_vec(block).unwrap()))
                .collect(),
        )
    }

    fn replay_values(replay: &ProviderReplay) -> Vec<Value> {
        replay
            .items()
            .iter()
            .map(|item| serde_json::from_slice(item.as_bytes()).unwrap())
            .collect()
    }

    fn history_with_code(request: &str) -> ModelHistory {
        let mut history = history(request);
        history
            .append_completed_code(
                "print(1)".to_owned(),
                ModelCallId::new("toolu_1"),
                Some("I'll run it.".to_owned()),
                "Checking the file".to_owned(),
                replay(&tool_blocks()),
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        history
    }

    fn expected_request(messages: Value) -> Value {
        json!({
            "model": "claude-opus-5-5",
            "max_tokens": 128000,
            "stream": true,
            "system": [{"type": "text", "text": "system prompt"}],
            "messages": messages,
            "tools": [{
                "name": "execute_python",
                "strict": true,
                "input_schema": {
                    "type": "object",
                    "properties": {"code": {"type": "string"}},
                    "required": ["code"],
                    "additionalProperties": false
                }
            }],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": "medium"}
        })
    }

    fn headers(head: &str) -> BTreeMap<String, String> {
        head.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            .collect()
    }

    fn streamed(progress: &[ModelProgress]) -> (String, String, String) {
        let (mut reasoning, mut text, mut code) = (String::new(), String::new(), String::new());
        for event in progress {
            match event {
                ModelProgress::ReasoningSummaryChunk { text: chunk } => reasoning.push_str(chunk),
                ModelProgress::AssistantTextChunk { text: chunk } => text.push_str(chunk),
                ModelProgress::DecodedPythonChunk { code: chunk } => code.push_str(chunk),
                _ => {}
            }
        }
        (reasoning, text, code)
    }

    fn model(fixture: &Fixture, key: &str) -> AnthropicModel {
        AnthropicModel::with_endpoint("system prompt", key, &fixture.endpoint)
    }

    async fn step_with(
        fixture: &Fixture,
        key: &str,
        selection: ModelSelection,
        history: &ModelHistory,
    ) -> (Result<ModelStep, ModelFailure>, Vec<ModelProgress>) {
        let mut model = model(fixture, key);
        let cancellation = CancellationToken::new();
        let mut events = Vec::new();
        let result = {
            let mut progress = |event| events.push(event);
            model
                .step(selection, history, &cancellation, &mut progress)
                .await
        };
        (result, events)
    }

    async fn step(
        fixture: &Fixture,
        history: &ModelHistory,
    ) -> (Result<ModelStep, ModelFailure>, Vec<ModelProgress>) {
        step_with(fixture, KEY, selection(), history).await
    }

    fn assert_sanitised(failure: &ModelFailure) {
        let provider = failure.provider_failure().expect("provider failure");
        let diagnostics = format!("{provider:?}\n{provider}\n{failure:?}\n{failure}");
        assert!(!diagnostics.contains(HOSTILE_KEY), "{diagnostics}");
        assert!(!diagnostics.contains("sk-ant-"), "{diagnostics}");
        assert!(!diagnostics.contains(METADATA), "{diagnostics}");
        assert!(provider.message().len() <= PROVIDER_MESSAGE_LIMIT);
        assert!(
            provider
                .code()
                .is_none_or(|code| code.len() <= PROVIDER_CODE_LIMIT)
        );
    }

    #[test]
    fn production_model_posts_to_the_anthropic_messages_endpoint() {
        assert_eq!(
            AnthropicModel::new("system prompt", KEY).endpoint,
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[tokio::test]
    async fn sends_the_exact_fresh_request_headers_and_body() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;

        let (result, _) = step_with(
            &fixture,
            KEY,
            ModelSelection::new(&OPUS_5_5, "high").unwrap(),
            &history("inspect this project"),
        )
        .await;
        result.unwrap();

        let captured = fixture.request();
        assert!(captured.head.starts_with("POST /v1/messages HTTP/1.1\r\n"));
        let headers = headers(&captured.head);
        assert_eq!(
            headers.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "accept",
                "anthropic-version",
                "content-length",
                "content-type",
                "host",
                "x-api-key"
            ]
        );
        assert_eq!(headers["x-api-key"], KEY);
        assert_eq!(headers["anthropic-version"], "2023-06-01");
        assert_eq!(headers["content-type"], "application/json");
        let mut expected = expected_request(json!([{
            "role": "user",
            "content": [{"type": "text", "text": "inspect this project"}]
        }]));
        expected["output_config"]["effort"] = json!("high");
        assert_eq!(captured.body, expected);
    }

    #[tokio::test]
    async fn tool_use_stream_returns_code_with_commentary_reasoning_preview_and_replay() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(tool_stream())).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Code {
                source: "print(1)".to_owned(),
                call_id: ModelCallId::new("toolu_1"),
                commentary: Some("I'll run it.".to_owned()),
            }
        );
        assert_eq!(step.reasoning_summary, "Checking the file");
        assert_eq!(step.replay.provider(), "anthropic");
        assert_eq!(replay_values(&step.replay), tool_blocks());
        assert_eq!(step.attempts, vec![expected_accounting()]);
        assert_eq!(
            progress.first(),
            Some(&ModelProgress::AttemptStarted { attempt: 1 })
        );
        assert_eq!(
            progress.last(),
            Some(&ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: expected_accounting(),
            })
        );
        assert_eq!(
            streamed(&progress),
            (
                "Checking the file".to_owned(),
                "I'll run it.".to_owned(),
                "print(1)".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn end_turn_stream_returns_the_final_message_and_opaque_replay() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Message {
                text: "Done.".to_owned(),
            }
        );
        assert_eq!(step.reasoning_summary, "Summarising");
        assert_eq!(replay_values(&step.replay), message_blocks());
        assert_eq!(step.attempts, vec![expected_accounting()]);
        assert_eq!(
            streamed(&progress),
            ("Summarising".to_owned(), "Done.".to_owned(), String::new())
        );
    }

    #[tokio::test]
    async fn replay_round_trips_into_the_next_request_with_the_tool_result() {
        let fixture = Fixture::start_sequence(vec![
            FixtureReply::OwnedSse(tool_stream()),
            FixtureReply::OwnedSse(message_stream()),
        ])
        .await;
        let mut model = model(&fixture, KEY);
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        let mut history = history("inspect");

        let first = model
            .step(selection(), &history, &cancellation, &mut progress)
            .await
            .unwrap();
        let ModelAction::Code {
            source,
            call_id,
            commentary,
        } = first.action
        else {
            panic!("expected code");
        };
        history
            .append_completed_code(
                source,
                call_id,
                commentary,
                first.reasoning_summary,
                first.replay,
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        model.validate_history(&history, selection()).unwrap();
        let second = model
            .step(selection(), &history, &cancellation, &mut progress)
            .await
            .unwrap();
        let ModelAction::Message { text } = second.action else {
            panic!("expected message");
        };
        history.append_final_message(text, second.reasoning_summary, second.replay);
        history.append_user_request("thanks");

        let first_turn = json!([
            {"role": "user", "content": [{"type": "text", "text": "inspect"}]},
            {"role": "assistant", "content": tool_blocks()},
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "1\n"}]
            }
        ]);
        assert_eq!(
            fixture.requests.lock().unwrap()[1].body,
            expected_request(first_turn.clone())
        );
        let mut next_turn = first_turn.as_array().unwrap().clone();
        next_turn.extend([
            json!({"role": "assistant", "content": message_blocks()}),
            json!({"role": "user", "content": [{"type": "text", "text": "thanks"}]}),
        ]);
        assert_eq!(
            model.request(selection(), &history).unwrap(),
            expected_request(Value::Array(next_turn))
        );
    }

    #[test]
    fn adjacent_user_content_merges_into_one_user_message() {
        let model = AnthropicModel::new("system prompt", KEY);
        let mut history = ModelHistory::new();
        history.append_compaction_summary("saved summary");
        history.append_user_request("continue");
        history
            .append_completed_code(
                "print(1)".to_owned(),
                ModelCallId::new("toolu_1"),
                Some("I'll run it.".to_owned()),
                "Checking the file".to_owned(),
                replay(&tool_blocks()),
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        history.append_user_request("next");

        assert_eq!(
            model.request(selection(), &history).unwrap(),
            expected_request(json!([
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": canonical_compaction_summary("saved summary")},
                        {"type": "text", "text": "continue"}
                    ]
                },
                {"role": "assistant", "content": tool_blocks()},
                {
                    "role": "user",
                    "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": "1\n"},
                        {"type": "text", "text": "next"}
                    ]
                }
            ]))
        );
    }

    #[tokio::test]
    async fn usage_keeps_omitted_fields_and_prices_the_one_hour_cache_write_split() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(stream(
            json!({
                "input_tokens": 10,
                "cache_read_input_tokens": 20,
                "cache_creation_input_tokens": 30,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 10,
                    "ephemeral_1h_input_tokens": 20
                },
                "output_tokens": 1,
                "service_tier": "standard"
            }),
            vec![text(0, "Done.")],
            "end_turn",
            json!({
                "cache_read_input_tokens": 25,
                "output_tokens": 50,
                "output_tokens_details": {"thinking_tokens": 12}
            }),
        )))
        .await;

        let (result, _) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();
        let [AttemptAccounting::KnownUsage(usage)] = step.attempts.as_slice() else {
            panic!("expected known usage");
        };

        assert_eq!(
            usage,
            &ModelUsage {
                model: "anthropic/claude-opus-5-5".to_owned(),
                service_tier: Some("standard".to_owned()),
                input_tokens: Some(65),
                cached_input_tokens: Some(25),
                cache_write_input_tokens: Some(30),
                cache_write_1h_input_tokens: 20,
                output_tokens: Some(50),
                reasoning_output_tokens: Some(12),
                total_tokens: Some(115),
            }
        );
        let estimate = crate::pricing::estimate_usage(usage).unwrap();
        assert_eq!(
            estimate.amount().as_nanos(),
            10 * 4_000 + 25 * 200 + 10 * 5_000 + 20 * 8_000 + 50 * 20_000
        );
        assert_eq!(estimate.pricing(), OPUS_5_5.pricing.identity);
    }

    #[tokio::test]
    async fn max_tokens_and_refusal_fail_as_provider_failures_with_usage() {
        for (stop_reason, message) in [
            ("max_tokens", "response incomplete: max_tokens"),
            ("refusal", "model refused the request"),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream(
                start_usage(),
                vec![text(0, "partial")],
                stop_reason,
                final_usage(),
            )))
            .await;

            let (result, progress) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();
            let provider = failure.provider_failure().expect(stop_reason);

            assert_eq!(
                (provider.status(), provider.code(), provider.message()),
                (None, Some(stop_reason), message)
            );
            assert!(!failure.is_authentication());
            assert_eq!(failure.attempts(), &[expected_accounting()]);
            assert_eq!(
                progress.last(),
                Some(&ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: expected_accounting(),
                })
            );
        }
    }

    #[tokio::test]
    async fn end_turn_without_text_is_an_invalid_response() {
        for blocks in [vec![thinking(0, &["Nothing"], "sig")], vec![text(0, "")]] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream(
                start_usage(),
                blocks,
                "end_turn",
                final_usage(),
            )))
            .await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();

            assert_eq!(
                failure.error(),
                &ModelError::InvalidResponse("assistant message was empty")
            );
            assert_eq!(failure.attempts(), &[expected_accounting()]);
        }
    }

    #[tokio::test]
    async fn http_401_is_an_authentication_failure_without_usage() {
        let fixture = Fixture::start(FixtureReply::Status {
            code: 401,
            body: r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"},"request_id":"req_1"}"#,
        })
        .await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();

        assert!(failure.is_authentication());
        assert_eq!(
            (provider.status(), provider.code(), provider.message()),
            (Some(401), Some("authentication_error"), "invalid x-api-key")
        );
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        assert_eq!(
            progress,
            vec![
                ModelProgress::AttemptStarted { attempt: 1 },
                ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: AttemptAccounting::UsageUnavailable,
                },
            ]
        );
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn redirects_are_provider_failures_and_never_resend_the_request() {
        for code in [307, 308] {
            let target = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code,
                headers: vec![(
                    "location".to_owned(),
                    format!("{}/v1/messages", target.endpoint),
                )],
                body: String::new(),
            })
            .await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();
            let provider = failure.provider_failure().expect("provider failure");

            assert_eq!(
                (provider.status(), provider.code(), provider.message()),
                (Some(code), None, "provider request failed")
            );
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
            assert_eq!(fixture.request_count(), 1);
            assert_eq!(target.request_count(), 0);
        }
    }

    #[tokio::test]
    async fn hostile_http_errors_redact_the_key_before_bounding() {
        let prefix = "x".repeat(PROVIDER_MESSAGE_LIMIT - 6);
        let body = json!({
            "type": "error",
            "error": {
                "type": format!("{HOSTILE_KEY}{}", "é".repeat(PROVIDER_CODE_LIMIT)),
                "message": format!("{prefix}{HOSTILE_KEY}")
            },
            "request_id": METADATA
        })
        .to_string();
        let fixture = Fixture::start(FixtureReply::OwnedStatus {
            code: 400,
            headers: vec![("request-id".to_owned(), METADATA.to_owned())],
            body,
        })
        .await;

        let (result, _) = step_with(&fixture, HOSTILE_KEY, selection(), &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();

        assert_sanitised(&failure);
        assert_eq!(provider.status(), Some(400));
        assert_eq!(provider.message(), format!("{prefix}[REDAC"));
        assert_eq!(
            provider.code(),
            Some(format!("[REDACTED]{}", "é".repeat((PROVIDER_CODE_LIMIT - 10) / 2)).as_str())
        );
    }

    #[tokio::test]
    async fn hostile_http_errors_never_expose_request_metadata() {
        for (code, body, expected_code, expected_message) in [
            (
                400,
                json!({
                    "type": "error",
                    "error": {
                        "type": "invalid_request_error",
                        "message": format!("bad request for {HOSTILE_KEY}")
                    },
                    "request_id": METADATA
                })
                .to_string(),
                Some("invalid_request_error"),
                "bad request for [REDACTED]",
            ),
            (
                502,
                format!("<html>{HOSTILE_KEY} {METADATA}</html>"),
                None,
                "provider request failed",
            ),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code,
                headers: vec![
                    ("request-id".to_owned(), METADATA.to_owned()),
                    ("anthropic-organization-id".to_owned(), METADATA.to_owned()),
                ],
                body,
            })
            .await;

            let (result, _) =
                step_with(&fixture, HOSTILE_KEY, selection(), &history("inspect")).await;
            let failure = result.unwrap_err();
            let provider = failure.provider_failure().unwrap();

            assert_sanitised(&failure);
            assert_eq!(
                (provider.status(), provider.code(), provider.message()),
                (Some(code), expected_code, expected_message)
            );
        }
    }

    #[tokio::test]
    async fn hostile_sse_errors_are_redacted_and_bounded_without_metadata() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(sse([
            message_start(start_usage()),
            json!({
                "type": "error",
                "error": {
                    "type": format!("overloaded_error {HOSTILE_KEY}"),
                    "message": format!("{HOSTILE_KEY} {}", "é".repeat(PROVIDER_MESSAGE_LIMIT))
                },
                "request_id": METADATA
            }),
        ])))
        .await;

        let (result, _) = step_with(&fixture, HOSTILE_KEY, selection(), &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();

        assert_sanitised(&failure);
        assert_eq!(provider.status(), None);
        assert_eq!(provider.code(), Some("overloaded_error [REDACTED]"));
        assert_eq!(
            provider.message(),
            format!(
                "[REDACTED] {}",
                "é".repeat((PROVIDER_MESSAGE_LIMIT - 11) / 2)
            )
        );
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
    }

    #[tokio::test]
    async fn stream_requires_one_assistant_message_start_echoing_the_selected_model() {
        let start = message_start(start_usage());
        let mut user = start.clone();
        user["message"]["role"] = json!("user");
        let mut other_model = start.clone();
        other_model["message"]["model"] = json!("claude-fable-5-1");
        let mut no_role = start.clone();
        no_role["message"].as_object_mut().unwrap().remove("role");
        let mut no_model = start.clone();
        no_model["message"].as_object_mut().unwrap().remove("model");
        let message_delta = json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": final_usage()
        });
        let message_stop = json!({"type": "message_stop"});
        let with_content = |envelope: Vec<Value>| -> Vec<Value> {
            envelope
                .into_iter()
                .chain(text(0, "Done."))
                .chain([message_delta.clone(), message_stop.clone()])
                .collect()
        };

        for (events, expected) in [
            (
                with_content(vec![]),
                "stream content arrived before message start",
            ),
            (
                vec![message_delta.clone(), message_stop.clone()],
                "stream content arrived before message start",
            ),
            (
                vec![message_stop.clone()],
                "stream content arrived before message start",
            ),
            (
                with_content(vec![start.clone(), start.clone()]),
                "stream repeated message start",
            ),
            (
                with_content(vec![user]),
                "message start was not an assistant message",
            ),
            (
                with_content(vec![other_model]),
                "message start did not echo the selected model",
            ),
            (
                with_content(vec![no_role]),
                "stream contained a malformed event",
            ),
            (
                with_content(vec![no_model]),
                "stream contained a malformed event",
            ),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(sse(events))).await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();

            assert_eq!(failure.error(), &ModelError::InvalidResponse(expected));
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        }
    }

    #[tokio::test]
    async fn sse_error_before_message_start_is_a_provider_failure() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(sse([json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "Overloaded"}
        })])))
        .await;

        let (result, _) = step(&fixture, &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().expect("provider failure");

        assert_eq!(
            (provider.status(), provider.code(), provider.message()),
            (None, Some("overloaded_error"), "Overloaded")
        );
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
    }

    fn budget_stream(
        thinking: usize,
        signature: usize,
        redacted: usize,
        text: usize,
        code: usize,
    ) -> String {
        let x = |bytes: usize| "x".repeat(bytes);
        stream(
            start_usage(),
            vec![
                vec![
                    block_start(0, json!({"type": "thinking", "thinking": ""})),
                    delta(
                        0,
                        json!({"type": "thinking_delta", "thinking": x(thinking)}),
                    ),
                    delta(
                        0,
                        json!({"type": "signature_delta", "signature": x(signature)}),
                    ),
                    block_stop(0),
                ],
                vec![
                    block_start(1, json!({"type": "redacted_thinking", "data": x(redacted)})),
                    block_stop(1),
                ],
                vec![
                    block_start(2, json!({"type": "text", "text": "ab"})),
                    delta(2, json!({"type": "text_delta", "text": x(text - 2)})),
                    block_stop(2),
                ],
                tool_use(3, "toolu_1", &[r#"{"code":""#, &x(code), r#""}"#]),
            ],
            "tool_use",
            final_usage(),
        )
    }

    #[tokio::test]
    async fn content_budget_accepts_every_field_counted_once_at_the_limit() {
        let quarter = ACCEPTED_MODEL_CONTENT_LIMIT / 4;
        let fixture = Fixture::start(FixtureReply::OwnedSse(budget_stream(
            quarter,
            quarter / 2,
            quarter / 2,
            quarter,
            quarter - TOOL_OVERHEAD,
        )))
        .await;

        let (result, _) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert!(matches!(
            step.action,
            ModelAction::Code { source, .. } if source.len() == quarter - TOOL_OVERHEAD
        ));
    }

    #[tokio::test]
    async fn content_budget_rejects_combined_fields_over_the_limit_without_an_action() {
        let quarter = ACCEPTED_MODEL_CONTENT_LIMIT / 4;
        let fixture = Fixture::start(FixtureReply::OwnedSse(budget_stream(
            quarter,
            quarter / 2,
            quarter / 2,
            quarter,
            quarter - TOOL_OVERHEAD + 1,
        )))
        .await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let failure = result.unwrap_err();

        assert_eq!(failure.error(), &ModelError::ContentTooLarge);
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        assert_eq!(
            progress.last(),
            Some(&ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: AttemptAccounting::UsageUnavailable,
            })
        );
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn content_budget_counts_oversized_opaque_replay_data() {
        let oversized = "x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT + 1);
        for block in [
            json!({"type": "redacted_thinking", "data": oversized}),
            json!({"type": "thinking", "thinking": "", "signature": oversized}),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream(
                start_usage(),
                vec![vec![block_start(0, block), block_stop(0)], text(1, "Done.")],
                "end_turn",
                final_usage(),
            )))
            .await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();

            assert_eq!(failure.error(), &ModelError::ContentTooLarge);
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        }
    }

    const SMALL_FRAME_LIMIT: usize = 1024;

    fn ignored_frame(padding: &str) -> String {
        sse([json!({"type": "future_event", "padding": padding})])
    }

    fn error_frame(message: &str) -> String {
        sse([json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": message}
        })])
    }

    fn padded(frame: fn(&str) -> String, frame_bytes: usize) -> String {
        // The closing blank line resets the count instead of adding to it.
        let framing = frame("").len() - 1;
        frame(&"x".repeat(frame_bytes - framing))
    }

    async fn step_with_small_frame_limit(frame: &str) -> Result<ModelStep, ModelFailure> {
        let start = sse([message_start(start_usage())]);
        let rest = sse(text(0, "Done.").into_iter().chain([
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": final_usage()
            }),
            json!({"type": "message_stop"}),
        ]));
        let fixture = Fixture::start(FixtureReply::OwnedSse(format!("{start}{frame}{rest}"))).await;
        let mut model = model(&fixture, KEY);
        model.frame_limit = SMALL_FRAME_LIMIT;

        let result = model
            .step(
                selection(),
                &history("inspect"),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .await;

        assert_eq!(fixture.request_count(), 1);
        result
    }

    #[tokio::test]
    async fn stream_accepts_a_frame_at_the_model_frame_limit() {
        let result = step_with_small_frame_limit(&padded(ignored_frame, SMALL_FRAME_LIMIT)).await;

        assert_eq!(
            result.unwrap().action,
            ModelAction::Message {
                text: "Done.".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn stream_rejects_ignored_and_error_frames_over_the_model_frame_limit() {
        for frame in [ignored_frame, error_frame] {
            let result = step_with_small_frame_limit(&padded(frame, SMALL_FRAME_LIMIT + 1)).await;
            let failure = result.unwrap_err();

            assert_eq!(failure.error(), &ModelError::ContentTooLarge);
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        }
    }

    #[tokio::test]
    async fn cr_only_stream_returns_the_message_with_usage() {
        let body = stream(
            start_usage(),
            vec![text(0, "one\r\ntwo")],
            "end_turn",
            final_usage(),
        )
        .replace('\n', "\r");
        let fixture = Fixture::start(FixtureReply::OwnedSse(body)).await;

        let (result, _) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Message {
                text: "one\r\ntwo".to_owned()
            }
        );
        assert_eq!(step.attempts, vec![expected_accounting()]);
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

    #[test]
    fn production_frame_limit_fits_at_limit_content_escaped_six_fold_in_one_frame() {
        let frame = |data: &str| {
            sse([block_start(
                0,
                json!({"type": "redacted_thinking", "data": data}),
            )])
        };
        let framing = frame("").len();

        assert_eq!(frame("\u{1}").len(), framing + 6);
        assert!(6 * ACCEPTED_MODEL_CONTENT_LIMIT + framing <= SSE_FRAME_LIMIT);
        assert_eq!(
            AnthropicModel::new("system prompt", KEY).frame_limit,
            SSE_FRAME_LIMIT
        );
    }

    #[tokio::test]
    async fn rejects_a_tool_call_identifier_already_in_history() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(tool_stream())).await;

        let (result, _) = step(&fixture, &history_with_code("inspect")).await;

        assert_eq!(
            result.unwrap_err().error(),
            &ModelError::InvalidResponse("tool call identifier duplicates model history")
        );
    }

    #[tokio::test]
    async fn foreign_replay_and_selections_fail_before_networking() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;
        let mut model = model(&fixture, KEY);
        let cancellation = CancellationToken::new();
        let mut foreign = history("inspect");
        foreign.append_final_message(
            "Done.".to_owned(),
            String::new(),
            ProviderReplay::new("openai", vec![OpaqueReplayItem::new(b"{}".to_vec())]),
        );
        let mut malformed = history("inspect");
        malformed.append_final_message(
            "Done.".to_owned(),
            String::new(),
            ProviderReplay::new("anthropic", vec![OpaqueReplayItem::new(b"{".to_vec())]),
        );
        let mut mismatched = history("inspect");
        mismatched
            .append_completed_code(
                "print(2)".to_owned(),
                ModelCallId::new("toolu_1"),
                Some("I'll run it.".to_owned()),
                String::new(),
                replay(&tool_blocks()),
                CodeResult::execution("2\n".to_owned()),
            )
            .unwrap();
        let openai = ModelSelection::new(&SOL, SOL.default_thinking).unwrap();

        for (history, selection, expected) in [
            (
                &foreign,
                selection(),
                ModelError::InvalidHistory("opaque replay belongs to another provider"),
            ),
            (
                &malformed,
                selection(),
                ModelError::InvalidHistory("opaque Anthropic replay is malformed"),
            ),
            (
                &mismatched,
                selection(),
                ModelError::InvalidHistory("completed code does not match its Anthropic replay"),
            ),
            (
                &history("inspect"),
                openai,
                ModelError::InvalidSelection("unsupported Anthropic model"),
            ),
        ] {
            let mut progress_events = 0;
            assert_eq!(
                &model.validate_history(history, selection).unwrap_err(),
                &expected
            );
            let failure = model
                .step(selection, history, &cancellation, &mut |_| {
                    progress_events += 1
                })
                .await
                .unwrap_err();
            assert_eq!(failure.error(), &expected);
            assert!(failure.attempts().is_empty());
            assert_eq!(progress_events, 0);
        }
        assert_eq!(fixture.request_count(), 0);
    }

    #[tokio::test]
    async fn cancellation_during_the_stream_settles_without_usage_and_closes_the_connection() {
        let fixture = Fixture::start(FixtureReply::IdleSse(IDLE_STREAM)).await;
        let mut model = model(&fixture, KEY);
        let cancellation = CancellationToken::new();
        let history = history("inspect");
        let mut progress = Vec::new();

        let (result, ()) = {
            let mut sink = |event| progress.push(event);
            tokio::join!(
                model.step(selection(), &history, &cancellation, &mut sink),
                async {
                    fixture.wait_for_requests(1).await;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    cancellation.cancel();
                }
            )
        };
        let failure = result.unwrap_err();

        assert!(failure.is_cancelled());
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        assert_eq!(
            progress.last(),
            Some(&ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: AttemptAccounting::UsageUnavailable,
            })
        );
        fixture.wait_for_disconnect().await;
    }
}
