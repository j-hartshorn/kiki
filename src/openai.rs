use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use async_openai::Client;
use async_openai::config::{OPENAI_API_BASE, OpenAIConfig};
use async_openai::error::OpenAIError;
use async_openai::middleware::ReqwestService;
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::model::{
    AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelContextEntry, ModelError,
    ModelHistory, ModelProgress, ModelStep, ModelUsage, OpaqueReplayItem, ProviderReplay,
};

const OPENAI_PROVIDER: &str = "openai";
const FIXED_MODEL: &str = "gpt-5.6-sol";
const APPLICATION_CONTENT_LIMIT: usize = 16 * 1024 * 1024;

type WireStream = Pin<Box<dyn Stream<Item = Result<WireEvent, OpenAIError>> + Send>>;

fn openai_config(api_key: impl Into<String>, api_base: impl Into<String>) -> OpenAIConfig {
    OpenAIConfig::new()
        .with_api_base(api_base)
        .with_api_key(api_key)
        .with_org_id("")
        .with_project_id("")
}

pub(crate) struct OpenAiModel {
    prompt: String,
    client: Client<OpenAIConfig>,
    model: &'static str,
}

impl OpenAiModel {
    #[allow(dead_code)]
    pub(crate) fn new(prompt: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            client: Client::with_config(openai_config(api_key, OPENAI_API_BASE))
                .with_http_service(ReqwestService::default()),
            model: FIXED_MODEL,
        }
    }

    #[cfg(test)]
    fn with_endpoint(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        endpoint: &str,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            client: Client::with_config(openai_config(api_key, endpoint))
                .with_http_service(ReqwestService::default()),
            model: FIXED_MODEL,
        }
    }

    fn request(&self, history: &ModelHistory) -> Result<Value, ModelError> {
        let mut instructions = self.prompt.clone();
        let mut input = Vec::new();

        for entry in history.entries() {
            match entry {
                ModelContextEntry::ReadCapability { description } => {
                    instructions.push_str("\n\n");
                    instructions.push_str(description);
                }
                ModelContextEntry::UserRequest { text } => {
                    input.push(json!({"role": "user", "content": text}));
                }
                ModelContextEntry::CompletedCode(completed) => {
                    let replay = replay_values(completed.replay())?;
                    match validate_replay_output(&replay)? {
                        ValidatedAction::Code { source, call_id }
                            if source == completed.source() && call_id == *completed.call_id() => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "completed code does not match its OpenAI replay",
                            ));
                        }
                    }
                    input.extend(replay);
                    let output = match completed.result() {
                        CodeResult::Execution { feedback } => feedback,
                    };
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": completed.call_id().as_str(),
                        "output": output
                    }));
                }
                ModelContextEntry::FinalMessage { text, replay } => {
                    let replay = replay_values(replay)?;
                    match validate_replay_output(&replay)? {
                        ValidatedAction::Message { text: replay_text } if replay_text == *text => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "final message does not match its OpenAI replay",
                            ));
                        }
                    }
                    input.extend(replay);
                }
            }
        }

        Ok(json!({
            "model": self.model,
            "instructions": instructions,
            "input": input,
            "stream": true,
            "store": false,
            "service_tier": "default",
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {
                "effort": "medium",
                "summary": "auto",
                "context": "all_turns"
            },
            "include": ["reasoning.encrypted_content"],
            "tools": [{
                "type": "function",
                "name": "execute_python",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"],
                    "additionalProperties": false
                }
            }]
        }))
    }

    async fn one_attempt(
        &self,
        history: &ModelHistory,
        cancellation: &CancellationToken,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<ModelStep, ModelError> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled);
        }
        let request = self.request(history)?;
        progress(ModelProgress::AttemptStarted { attempt: 1 });
        let responses = self.client.responses();
        let open_stream = responses.create_stream_byot::<_, WireEvent>(request);
        let mut stream: WireStream = tokio::select! {
            _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
            stream = open_stream => stream.map_err(provider_error)?,
        };
        let mut content = ApplicationContentGuard::default();
        let mut presentation = PresentationBatcher::default();
        let mut python_decoders = HashMap::<(usize, String), PythonPreviewDecoder>::new();

        loop {
            let event = tokio::select! {
                _ = cancellation.cancelled() => {
                    presentation.flush_all(progress);
                    return Err(ModelError::Cancelled);
                }
                event = stream.next() => event,
            };
            let Some(event) = event else {
                presentation.flush_all(progress);
                return Err(ModelError::InvalidResponse(
                    "stream ended without a terminal response",
                ));
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    presentation.flush_all(progress);
                    return Err(provider_error(error));
                }
            };
            let now = Instant::now();
            let event_result = (|| -> Result<Option<WireResponse>, ModelError> {
                match event {
                    WireEvent::Completed { response, .. } => Ok(Some(response)),
                    WireEvent::Failed { response, .. } => Err(ModelError::Provider(
                        response_failure("response failed", &response),
                    )),
                    WireEvent::Incomplete { response, .. } => Err(ModelError::Provider(
                        response_failure("response incomplete", &response),
                    )),
                    WireEvent::Error { code, message, .. } => {
                        let code = code.map(|code| format!(" ({code})")).unwrap_or_default();
                        Err(ModelError::Provider(format!(
                            "stream error{code}: {message}"
                        )))
                    }
                    WireEvent::OutputTextDelta {
                        output_index,
                        content_index,
                        delta,
                        ..
                    } => {
                        content.append(
                            ContentField::AssistantText(output_index, content_index),
                            delta.len(),
                        )?;
                        presentation.push(PresentationKind::AssistantText, delta, now, progress);
                        Ok(None)
                    }
                    WireEvent::OutputTextDone {
                        output_index,
                        content_index,
                        text,
                        ..
                    } => {
                        content.observe(
                            ContentField::AssistantText(output_index, content_index),
                            text.len(),
                        )?;
                        Ok(None)
                    }
                    WireEvent::ReasoningSummaryDelta {
                        output_index,
                        summary_index,
                        delta,
                        ..
                    } => {
                        content.append(
                            ContentField::ReasoningSummary(output_index, summary_index),
                            delta.len(),
                        )?;
                        presentation.push(PresentationKind::ReasoningSummary, delta, now, progress);
                        Ok(None)
                    }
                    WireEvent::ReasoningSummaryDone {
                        output_index,
                        summary_index,
                        text,
                        ..
                    } => {
                        content.observe(
                            ContentField::ReasoningSummary(output_index, summary_index),
                            text.len(),
                        )?;
                        Ok(None)
                    }
                    WireEvent::FunctionArgumentsDelta {
                        output_index,
                        item_id,
                        delta,
                        ..
                    } => {
                        content
                            .append(ContentField::FunctionArguments(output_index), delta.len())?;
                        if let Some(decoder) = python_decoders.get_mut(&(output_index, item_id)) {
                            let code = decoder.push(&delta);
                            presentation.push(PresentationKind::DecodedPython, code, now, progress);
                        }
                        Ok(None)
                    }
                    WireEvent::FunctionArgumentsDone {
                        output_index,
                        item_id,
                        arguments,
                        ..
                    } => {
                        content.observe(
                            ContentField::FunctionArguments(output_index),
                            arguments.len(),
                        )?;
                        python_decoders.remove(&(output_index, item_id));
                        Ok(None)
                    }
                    WireEvent::OutputItemAdded {
                        output_index, item, ..
                    } => {
                        content.account_output_item(output_index, &item)?;
                        if let Some(item_id) = execute_python_item_id(&item) {
                            python_decoders
                                .entry((output_index, item_id.to_owned()))
                                .or_default();
                        }
                        Ok(None)
                    }
                    WireEvent::OutputItemDone {
                        output_index, item, ..
                    } => {
                        let item_id = item.get("id").and_then(Value::as_str).map(str::to_owned);
                        content.account_output_item(output_index, &item)?;
                        if let Some(item_id) = item_id {
                            python_decoders.remove(&(output_index, item_id));
                        }
                        validate_done_output_item(&item)?;
                        Ok(None)
                    }
                    WireEvent::ContentPartAdded {
                        output_index,
                        content_index,
                        part,
                        ..
                    }
                    | WireEvent::ContentPartDone {
                        output_index,
                        content_index,
                        part,
                        ..
                    } => {
                        content.account_content_part(output_index, content_index, &part)?;
                        Ok(None)
                    }
                    WireEvent::ReasoningSummaryPartAdded {
                        output_index,
                        summary_index,
                        part,
                        ..
                    }
                    | WireEvent::ReasoningSummaryPartDone {
                        output_index,
                        summary_index,
                        part,
                        ..
                    } => {
                        content.account_reasoning_part(output_index, summary_index, &part)?;
                        Ok(None)
                    }
                    WireEvent::RefusalDelta | WireEvent::RefusalDone => {
                        Err(ModelError::InvalidResponse("model refused the request"))
                    }
                    WireEvent::Created
                    | WireEvent::Queued
                    | WireEvent::InProgress
                    | WireEvent::OutputTextAnnotationAdded => Ok(None),
                    WireEvent::Unexpected => Err(ModelError::InvalidResponse(
                        "stream contained an unexpected event",
                    )),
                }
            })();

            match event_result {
                Ok(Some(response)) => {
                    presentation.flush_all(progress);
                    let step = self.completed_step(history, response, &mut content)?;
                    progress(ModelProgress::AttemptSettled {
                        attempt: 1,
                        accounting: step.attempts[0].clone(),
                    });
                    return Ok(step);
                }
                Ok(None) => presentation.flush_due(now, progress),
                Err(error) => {
                    presentation.flush_all(progress);
                    return Err(error);
                }
            }
        }
    }

    fn completed_step(
        &self,
        history: &ModelHistory,
        response: WireResponse,
        content: &mut ApplicationContentGuard,
    ) -> Result<ModelStep, ModelError> {
        if response.status != WireStatus::Completed {
            return Err(ModelError::InvalidResponse(
                "response.completed did not contain completed status",
            ));
        }
        if response.error.is_some() || response.incomplete_details.is_some() {
            return Err(ModelError::InvalidResponse(
                "completed response contained terminal failure details",
            ));
        }
        content.account_output(&response.output)?;
        let action = match validate_output(&response.output)? {
            ValidatedAction::Code { source, call_id } => {
                if history.contains_call_id(&call_id) {
                    return Err(ModelError::InvalidResponse(
                        "function call identifier duplicates model history",
                    ));
                }
                ModelAction::Code { source, call_id }
            }
            ValidatedAction::Message { text } => ModelAction::Message { text },
        };
        let replay = ProviderReplay::new(
            OPENAI_PROVIDER,
            response
                .output
                .iter()
                .map(|item| OpaqueReplayItem::new(serde_json::to_vec(item).expect("JSON value")))
                .collect(),
        );
        let attempts = vec![match response.usage {
            Some(usage) => AttemptAccounting::KnownUsage(ModelUsage {
                model: self.model.to_owned(),
                service_tier: response.service_tier,
                input_tokens: usage.input_tokens,
                cached_input_tokens: usage
                    .input_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens),
                cache_write_input_tokens: usage
                    .input_tokens_details
                    .and_then(|details| details.cache_write_tokens),
                output_tokens: usage.output_tokens,
                reasoning_output_tokens: usage
                    .output_tokens_details
                    .and_then(|details| details.reasoning_tokens),
                total_tokens: usage.total_tokens,
            }),
            None => AttemptAccounting::UsageUnavailable,
        }];

        Ok(ModelStep {
            action,
            replay,
            attempts,
        })
    }
}

impl Model for OpenAiModel {
    fn step<'a>(
        &'a mut self,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelError>> + Send + 'a {
        self.one_attempt(history, cancellation, progress)
    }
}

fn provider_error(error: OpenAIError) -> ModelError {
    ModelError::Provider(error.to_string())
}

fn response_failure(prefix: &str, response: &WireResponse) -> String {
    if let Some(error) = &response.error {
        format!("{prefix}: {}: {}", error.code, error.message)
    } else if let Some(details) = &response.incomplete_details {
        format!("{prefix}: {}", details.reason)
    } else {
        prefix.to_owned()
    }
}

fn replay_values(replay: &ProviderReplay) -> Result<Vec<Value>, ModelError> {
    if replay.provider() != OPENAI_PROVIDER {
        return Err(ModelError::InvalidHistory(
            "opaque replay belongs to another provider",
        ));
    }
    replay
        .items()
        .iter()
        .map(|item| {
            serde_json::from_slice(item.as_bytes())
                .map_err(|_| ModelError::InvalidHistory("opaque OpenAI replay is malformed"))
        })
        .collect()
}

fn validate_replay_output(output: &[Value]) -> Result<ValidatedAction, ModelError> {
    validate_output(output).map_err(|error| match error {
        ModelError::InvalidResponse(message) => ModelError::InvalidHistory(message),
        error => error,
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutePythonArguments {
    code: String,
}

#[derive(Debug, PartialEq, Eq)]
enum ValidatedAction {
    Code {
        source: String,
        call_id: ModelCallId,
    },
    Message {
        text: String,
    },
}

fn validate_output(output: &[Value]) -> Result<ValidatedAction, ModelError> {
    let mut code = None;
    let mut message = None;
    let mut item_ids = HashSet::new();

    for item in output {
        let object = item
            .as_object()
            .ok_or(ModelError::InvalidResponse("output item was not an object"))?;
        if !item_ids.insert(required_nonempty_string(object, "id")?) {
            return Err(ModelError::InvalidResponse(
                "response contained duplicate output item identifiers",
            ));
        }
        match required_string(object, "type")? {
            "reasoning" => validate_reasoning(object)?,
            "function_call" => {
                if code.is_some() {
                    return Err(ModelError::InvalidResponse(
                        "response contained multiple function calls",
                    ));
                }
                code = Some(validate_function_call(object)?);
            }
            "message" => {
                if message.is_some() {
                    return Err(ModelError::InvalidResponse(
                        "response contained multiple assistant messages",
                    ));
                }
                message = Some(validate_message(object)?);
            }
            _ => {
                return Err(ModelError::InvalidResponse(
                    "response contained an unexpected output item",
                ));
            }
        }
    }

    match (code, message) {
        (Some(action), None) => Ok(action),
        (None, Some(action)) => Ok(action),
        (Some(_), Some(_)) => Err(ModelError::InvalidResponse(
            "response mixed a function call with assistant text",
        )),
        (None, None) => Err(ModelError::InvalidResponse(
            "response did not contain an action",
        )),
    }
}

fn validate_reasoning(object: &serde_json::Map<String, Value>) -> Result<(), ModelError> {
    match object.get("status") {
        None => {}
        Some(Value::String(status)) if status == "completed" => {}
        _ => {
            return Err(ModelError::InvalidResponse(
                "completed response contained incomplete reasoning",
            ));
        }
    }
    required_nonempty_string(object, "id")?;
    match object.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::Array(content)) if content.is_empty() => {}
        _ => {
            return Err(ModelError::InvalidResponse(
                "reasoning contained unexpected content",
            ));
        }
    }
    let summary =
        object
            .get("summary")
            .and_then(Value::as_array)
            .ok_or(ModelError::InvalidResponse(
                "reasoning summary was missing or malformed",
            ))?;
    for part in summary {
        let part = part.as_object().ok_or(ModelError::InvalidResponse(
            "reasoning summary part was malformed",
        ))?;
        if required_string(part, "type")? != "summary_text" {
            return Err(ModelError::InvalidResponse(
                "reasoning contained unexpected summary content",
            ));
        }
        required_string(part, "text")?;
    }
    if !matches!(object.get("encrypted_content"), None | Some(Value::Null))
        && object
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_none()
    {
        return Err(ModelError::InvalidResponse(
            "encrypted reasoning content was malformed",
        ));
    }
    Ok(())
}

fn validate_done_output_item(item: &Value) -> Result<(), ModelError> {
    let object = item.as_object().ok_or(ModelError::InvalidResponse(
        "completed stream output item was not an object",
    ))?;
    match required_string(object, "type")? {
        "reasoning" => validate_reasoning(object),
        "function_call" => validate_function_call(object).map(|_| ()),
        "message" => validate_message(object).map(|_| ()),
        _ => Err(ModelError::InvalidResponse(
            "stream contained an unexpected output item",
        )),
    }
}

fn validate_function_call(
    object: &serde_json::Map<String, Value>,
) -> Result<ValidatedAction, ModelError> {
    require_completed(object)?;
    required_nonempty_string(object, "id")?;
    let call_id = required_nonempty_string(object, "call_id")?;
    if required_string(object, "name")? != "execute_python" {
        return Err(ModelError::InvalidResponse(
            "response called an unknown function",
        ));
    }
    let arguments = required_string(object, "arguments")?;
    let arguments: ExecutePythonArguments = serde_json::from_str(arguments).map_err(|_| {
        ModelError::InvalidResponse("execute_python arguments were not a closed code object")
    })?;
    Ok(ValidatedAction::Code {
        source: arguments.code,
        call_id: ModelCallId::new(call_id),
    })
}

fn validate_message(
    object: &serde_json::Map<String, Value>,
) -> Result<ValidatedAction, ModelError> {
    require_completed(object)?;
    required_nonempty_string(object, "id")?;
    if required_string(object, "role")? != "assistant" {
        return Err(ModelError::InvalidResponse(
            "response message did not have assistant role",
        ));
    }
    let content =
        object
            .get("content")
            .and_then(Value::as_array)
            .ok_or(ModelError::InvalidResponse(
                "assistant message content was missing or malformed",
            ))?;
    let mut text = String::new();
    for part in content {
        let part = part.as_object().ok_or(ModelError::InvalidResponse(
            "assistant message content part was malformed",
        ))?;
        if required_string(part, "type")? != "output_text" {
            return Err(ModelError::InvalidResponse(
                "assistant message contained unexpected content",
            ));
        }
        text.push_str(required_string(part, "text")?);
    }
    if text.is_empty() {
        return Err(ModelError::InvalidResponse("assistant message was empty"));
    }
    Ok(ValidatedAction::Message { text })
}

fn require_completed(object: &serde_json::Map<String, Value>) -> Result<(), ModelError> {
    if required_string(object, "status")? != "completed" {
        return Err(ModelError::InvalidResponse(
            "completed response contained an incomplete output item",
        ));
    }
    Ok(())
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ModelError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(ModelError::InvalidResponse(match field {
            "type" => "output type was missing or malformed",
            "status" => "output status was missing or malformed",
            "id" => "output identifier was missing or malformed",
            "call_id" => "function call identifier was missing or malformed",
            "name" => "function name was missing or malformed",
            "arguments" => "function arguments were missing or malformed",
            "role" => "message role was missing or malformed",
            "text" => "text content was missing or malformed",
            _ => "required output field was missing or malformed",
        }))
}

fn required_nonempty_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ModelError> {
    let value = required_string(object, field)?;
    if value.is_empty() {
        return Err(ModelError::InvalidResponse("output identifier was empty"));
    }
    Ok(value)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ContentField {
    AssistantText(usize, usize),
    ReasoningSummary(usize, usize),
    FunctionArguments(usize),
    EncryptedReplay(usize),
}

#[derive(Default)]
struct ApplicationContentGuard {
    bytes: usize,
    fields: HashMap<ContentField, usize>,
}

impl ApplicationContentGuard {
    fn append(&mut self, field: ContentField, bytes: usize) -> Result<(), ModelError> {
        let current = self.fields.get(&field).copied().unwrap_or_default();
        let observed = current
            .checked_add(bytes)
            .ok_or(ModelError::ContentTooLarge)?;
        self.set_observed(field, current, observed)
    }

    fn observe(&mut self, field: ContentField, observed: usize) -> Result<(), ModelError> {
        let current = self.fields.get(&field).copied().unwrap_or_default();
        if observed <= current {
            return Ok(());
        }
        self.set_observed(field, current, observed)
    }

    fn set_observed(
        &mut self,
        field: ContentField,
        current: usize,
        observed: usize,
    ) -> Result<(), ModelError> {
        let additional = observed - current;
        let bytes = self
            .bytes
            .checked_add(additional)
            .ok_or(ModelError::ContentTooLarge)?;
        if bytes > APPLICATION_CONTENT_LIMIT {
            return Err(ModelError::ContentTooLarge);
        }
        self.bytes = bytes;
        self.fields.insert(field, observed);
        Ok(())
    }

    fn account_output(&mut self, output: &[Value]) -> Result<(), ModelError> {
        for (output_index, item) in output.iter().enumerate() {
            self.account_output_item(output_index, item)?;
        }
        Ok(())
    }

    fn account_output_item(&mut self, output_index: usize, item: &Value) -> Result<(), ModelError> {
        let object = item.as_object().ok_or(ModelError::InvalidResponse(
            "stream output item was not an object",
        ))?;
        let item_type = required_string(object, "type")?;
        required_nonempty_string(object, "id")?;
        match item_type {
            "reasoning" => {
                if let Some(summary) = object.get("summary").and_then(Value::as_array) {
                    for (index, part) in summary.iter().enumerate() {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            self.observe(
                                ContentField::ReasoningSummary(output_index, index),
                                text.len(),
                            )?;
                        }
                    }
                }
                if let Some(encrypted) = object.get("encrypted_content").and_then(Value::as_str) {
                    self.observe(ContentField::EncryptedReplay(output_index), encrypted.len())?;
                }
            }
            "function_call" => {
                if let Some(arguments) = object.get("arguments").and_then(Value::as_str) {
                    self.observe(
                        ContentField::FunctionArguments(output_index),
                        arguments.len(),
                    )?;
                }
            }
            "message" => {
                if let Some(parts) = object.get("content").and_then(Value::as_array) {
                    for (index, part) in parts.iter().enumerate() {
                        self.account_content_part(output_index, index, part)?;
                    }
                }
            }
            _ => {
                return Err(ModelError::InvalidResponse(
                    "stream contained an unexpected output item",
                ));
            }
        }
        Ok(())
    }

    fn account_content_part(
        &mut self,
        output_index: usize,
        content_index: usize,
        part: &Value,
    ) -> Result<(), ModelError> {
        let object = part.as_object().ok_or(ModelError::InvalidResponse(
            "stream content part was not an object",
        ))?;
        match required_string(object, "type")? {
            "output_text" => {
                if let Some(text) = object.get("text").and_then(Value::as_str) {
                    self.observe(
                        ContentField::AssistantText(output_index, content_index),
                        text.len(),
                    )?;
                }
                Ok(())
            }
            "refusal" => Err(ModelError::InvalidResponse("model refused the request")),
            _ => Err(ModelError::InvalidResponse(
                "stream contained unexpected message content",
            )),
        }
    }

    fn account_reasoning_part(
        &mut self,
        output_index: usize,
        summary_index: usize,
        part: &Value,
    ) -> Result<(), ModelError> {
        let object = part.as_object().ok_or(ModelError::InvalidResponse(
            "stream reasoning summary part was not an object",
        ))?;
        if required_string(object, "type")? != "summary_text" {
            return Err(ModelError::InvalidResponse(
                "stream contained unexpected reasoning content",
            ));
        }
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            self.observe(
                ContentField::ReasoningSummary(output_index, summary_index),
                text.len(),
            )?;
        }
        Ok(())
    }
}

const PROGRESS_PAYLOAD_LIMIT: usize = 4 * 1024;
const PROGRESS_RATE_LIMIT: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PresentationKind {
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
}

#[derive(Default)]
struct PresentationBatcher {
    pending: VecDeque<PendingPresentation>,
    pending_bytes: usize,
    last_emission: Option<Instant>,
}

impl PresentationBatcher {
    fn push(
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
            self.pending.push_back(PendingPresentation { kind, text });
        }

        if self.rate_window_open(now) {
            self.flush_all_at(now, progress);
        } else {
            self.flush_full_chunks(now, progress);
        }
    }

    fn flush_due(&mut self, now: Instant, progress: &mut (dyn FnMut(ModelProgress) + Send)) {
        if !self.pending.is_empty() && self.rate_window_open(now) {
            self.flush_all_at(now, progress);
        }
    }

    fn flush_all(&mut self, progress: &mut (dyn FnMut(ModelProgress) + Send)) {
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
        let mut pending = self.pending.pop_front().expect("pending presentation");
        let split = utf8_prefix_end(&pending.text, PROGRESS_PAYLOAD_LIMIT);
        let remainder = pending.text.split_off(split);
        self.pending_bytes -= pending.text.len();
        if !remainder.is_empty() {
            self.pending.push_front(PendingPresentation {
                kind: pending.kind,
                text: remainder,
            });
        }
        self.last_emission = Some(now);
        progress(pending.kind.progress(pending.text));
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

fn execute_python_item_id(item: &Value) -> Option<&str> {
    if item.get("type").and_then(Value::as_str) != Some("function_call")
        || item.get("name").and_then(Value::as_str) != Some("execute_python")
    {
        return None;
    }
    item.get("id")
        .and_then(Value::as_str)
        .filter(|item_id| !item_id.is_empty())
}

#[derive(Default)]
struct PythonPreviewDecoder {
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
    fn push(&mut self, fragment: &str) -> String {
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

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum WireEvent {
    #[serde(rename = "response.created")]
    Created,
    #[serde(rename = "response.queued")]
    Queued,
    #[serde(rename = "response.in_progress")]
    InProgress,
    #[serde(rename = "response.completed")]
    Completed {
        sequence_number: u64,
        response: WireResponse,
    },
    #[serde(rename = "response.failed")]
    Failed {
        sequence_number: u64,
        response: WireResponse,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        sequence_number: u64,
        response: WireResponse,
    },
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { output_index: usize, item: Value },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { output_index: usize, item: Value },
    #[serde(rename = "response.content_part.added")]
    ContentPartAdded {
        output_index: usize,
        content_index: usize,
        part: Value,
    },
    #[serde(rename = "response.content_part.done")]
    ContentPartDone {
        output_index: usize,
        content_index: usize,
        part: Value,
    },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta {
        output_index: usize,
        content_index: usize,
        delta: String,
    },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone {
        output_index: usize,
        content_index: usize,
        text: String,
    },
    #[serde(rename = "response.output_text.annotation.added")]
    OutputTextAnnotationAdded,
    #[serde(rename = "response.refusal.delta")]
    RefusalDelta,
    #[serde(rename = "response.refusal.done")]
    RefusalDone,
    #[serde(rename = "response.reasoning_summary_part.added")]
    ReasoningSummaryPartAdded {
        output_index: usize,
        summary_index: usize,
        part: Value,
    },
    #[serde(rename = "response.reasoning_summary_part.done")]
    ReasoningSummaryPartDone {
        output_index: usize,
        summary_index: usize,
        part: Value,
    },
    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryDelta {
        sequence_number: u64,
        output_index: usize,
        summary_index: usize,
        delta: String,
    },
    #[serde(rename = "response.reasoning_summary_text.done")]
    ReasoningSummaryDone {
        output_index: usize,
        summary_index: usize,
        text: String,
    },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionArgumentsDelta {
        sequence_number: u64,
        output_index: usize,
        item_id: String,
        delta: String,
    },
    #[serde(rename = "response.function_call_arguments.done")]
    FunctionArgumentsDone {
        output_index: usize,
        item_id: String,
        arguments: String,
    },
    #[serde(rename = "error")]
    Error {
        sequence_number: u64,
        code: Option<String>,
        message: String,
    },
    #[serde(other)]
    Unexpected,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Deserialize)]
struct WireResponse {
    id: String,
    status: WireStatus,
    #[serde(default)]
    output: Vec<Value>,
    service_tier: Option<String>,
    usage: Option<WireUsage>,
    error: Option<WireResponseError>,
    incomplete_details: Option<WireIncompleteDetails>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum WireStatus {
    Queued,
    InProgress,
    Completed,
    Failed,
    Incomplete,
    Cancelled,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    input_tokens_details: Option<WireInputTokenDetails>,
    output_tokens: Option<u64>,
    output_tokens_details: Option<WireOutputTokenDetails>,
    total_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireInputTokenDetails {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireOutputTokenDetails {
    reasoning_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct WireResponseError {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct WireIncompleteDetails {
    reason: String,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_openai::Client;
    use async_openai::config::OpenAIConfig;
    use async_openai::error::OpenAIError;
    use async_openai::middleware::ReqwestService;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    use super::{WireEvent, WireStatus, WireStream, openai_config};

    const COMPLETED_STREAM: &str = concat!(
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"sequence_number\":1,\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"Checking\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":2,\"item_id\":\"fc_1\",\"output_index\":1,\"delta\":\"{\\\"code\\\":\\\"print(1)\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"service_tier\":\"default\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Checking\"}],\"encrypted_content\":\"encrypted-reasoning\",\"status\":\"completed\"},{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"execute_python\",\"arguments\":\"{\\\"code\\\":\\\"print(1)\\\"}\",\"status\":\"completed\"}],\"usage\":{\"input_tokens\":23,\"input_tokens_details\":{\"cached_tokens\":5,\"cache_write_tokens\":7},\"output_tokens\":11,\"output_tokens_details\":{\"reasoning_tokens\":3},\"total_tokens\":34}}}\n\n",
        "data: [DONE]\n\n",
    );
    const FAILED_STREAM: &str = concat!(
        "data: {\"type\":\"response.failed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_failed\",\"status\":\"failed\",\"output\":[],\"error\":{\"code\":\"server_error\",\"message\":\"failed\"}}}\n\n",
        "data: [DONE]\n\n",
    );
    const INCOMPLETE_STREAM: &str = concat!(
        "data: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":{\"id\":\"resp_incomplete\",\"status\":\"incomplete\",\"output\":[],\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        "data: [DONE]\n\n",
    );
    const ERROR_STREAM: &str = concat!(
        "data: {\"type\":\"error\",\"sequence_number\":1,\"code\":\"stream_error\",\"message\":\"stream failed\",\"param\":null}\n\n",
        "data: [DONE]\n\n",
    );
    const ONE_DELTA: &str = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"sequence_number\":1,\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"partial\"}\n\n";

    #[derive(Clone)]
    enum FixtureReply {
        Sse(&'static str),
        OwnedSse(String),
        Status { code: u16, body: &'static str },
        AbruptSse(&'static str),
        IdleSse(&'static str),
    }

    #[derive(Clone)]
    struct CapturedRequest {
        head: String,
        body: Value,
    }

    struct Fixture {
        endpoint: String,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        disconnected: Arc<AtomicBool>,
        task: JoinHandle<()>,
    }

    impl Fixture {
        async fn start(reply: FixtureReply) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let disconnected = Arc::new(AtomicBool::new(false));
            let task = {
                let requests = requests.clone();
                let disconnected = disconnected.clone();
                tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let request = read_request(&mut stream).await;
                        requests.lock().unwrap().push(request);
                        write_reply(&mut stream, &reply, &disconnected).await;
                    }
                })
            };
            Self {
                endpoint,
                requests,
                disconnected,
                task,
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn request(&self) -> CapturedRequest {
            self.requests.lock().unwrap()[0].clone()
        }

        async fn wait_for_disconnect(&self) {
            timeout(Duration::from_secs(1), async {
                while !self.disconnected.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("dropping the SDK stream should close the fixture connection");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "fixture connection closed before request headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "fixture connection closed before request body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
        CapturedRequest { head, body }
    }

    async fn write_reply(stream: &mut TcpStream, reply: &FixtureReply, disconnected: &AtomicBool) {
        match reply {
            FixtureReply::Sse(body) => {
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            FixtureReply::OwnedSse(body) => {
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            FixtureReply::Status { code, body } => {
                let response = format!(
                    "HTTP/1.1 {code} Fixture Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            FixtureReply::AbruptSse(body) => {
                write_chunked_headers(stream).await;
                write_chunk(stream, body).await;
            }
            FixtureReply::IdleSse(body) => {
                write_chunked_headers(stream).await;
                write_chunk(stream, body).await;
                let mut byte = [0];
                if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                    disconnected.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    async fn write_chunked_headers(stream: &mut TcpStream) {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    }

    async fn write_chunk(stream: &mut TcpStream, body: &str) {
        stream
            .write_all(format!("{:X}\r\n{}\r\n", body.len(), body).as_bytes())
            .await
            .unwrap();
    }

    fn sdk_client(endpoint: &str) -> Client<OpenAIConfig> {
        Client::with_config(openai_config("fixture-key", endpoint))
            .with_http_service(ReqwestService::default())
    }

    fn execute_python_request() -> Value {
        json!({
            "model": "gpt-5.6-sol",
            "input": "compatibility fixture",
            "stream": true,
            "store": false,
            "service_tier": "default",
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {
                "effort": "medium",
                "summary": "auto",
                "context": "all_turns"
            },
            "include": ["reasoning.encrypted_content"],
            "tools": [{
                "type": "function",
                "name": "execute_python",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"],
                    "additionalProperties": false
                }
            }]
        })
    }

    async fn open_stream_with_request(
        fixture: &Fixture,
        request: Value,
    ) -> Result<WireStream, OpenAIError> {
        sdk_client(&fixture.endpoint)
            .responses()
            .create_stream_byot(request)
            .await
    }

    async fn open_stream(fixture: &Fixture) -> Result<WireStream, OpenAIError> {
        open_stream_with_request(fixture, execute_python_request()).await
    }

    async fn next_event(stream: &mut WireStream) -> Result<WireEvent, OpenAIError> {
        timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("fixture should produce a stream event")
            .expect("fixture stream should remain open")
    }

    #[tokio::test]
    async fn sdk_compatibility_sends_exact_request_and_decodes_completed_data() {
        let fixture = Fixture::start(FixtureReply::Sse(COMPLETED_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::ReasoningSummaryDelta {
                sequence_number,
                delta,
                ..
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(delta, "Checking");
            }
            event => panic!("unexpected first event: {event:?}"),
        }
        match next_event(&mut stream).await.unwrap() {
            WireEvent::FunctionArgumentsDelta {
                sequence_number,
                delta,
                ..
            } => {
                assert_eq!(sequence_number, 2);
                assert_eq!(delta, r#"{"code":"print(1)"}"#);
            }
            event => panic!("unexpected second event: {event:?}"),
        }
        let response = match next_event(&mut stream).await.unwrap() {
            WireEvent::Completed {
                sequence_number,
                response,
            } => {
                assert_eq!(sequence_number, 3);
                response
            }
            event => panic!("unexpected terminal event: {event:?}"),
        };

        assert_eq!(response.id, "resp_1");
        assert_eq!(response.status, WireStatus::Completed);
        assert_eq!(response.service_tier.as_deref(), Some("default"));
        assert_eq!(
            response.output[0]["encrypted_content"],
            json!("encrypted-reasoning")
        );
        assert_eq!(response.output[1]["name"], json!("execute_python"));
        assert_eq!(
            response.output[1]["arguments"],
            json!(r#"{"code":"print(1)"}"#)
        );
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, Some(23));
        let input_details = usage.input_tokens_details.unwrap();
        assert_eq!(input_details.cached_tokens, Some(5));
        assert_eq!(input_details.cache_write_tokens, Some(7));
        assert_eq!(usage.output_tokens, Some(11));
        assert_eq!(
            usage.output_tokens_details.unwrap().reasoning_tokens,
            Some(3)
        );
        assert_eq!(usage.total_tokens, Some(34));
        assert!(stream.next().await.is_none());

        let captured = fixture.request();
        assert!(captured.head.starts_with("POST /responses HTTP/1.1\r\n"));
        assert_eq!(
            captured.body,
            json!({
                "model": "gpt-5.6-sol",
                "input": "compatibility fixture",
                "stream": true,
                "store": false,
                "service_tier": "default",
                "tool_choice": "auto",
                "parallel_tool_calls": false,
                "reasoning": {
                    "effort": "medium",
                    "summary": "auto",
                    "context": "all_turns"
                },
                "include": ["reasoning.encrypted_content"],
                "tools": [{
                    "type": "function",
                    "name": "execute_python",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "code": {"type": "string"}
                        },
                        "required": ["code"],
                        "additionalProperties": false
                    }
                }]
            })
        );
    }

    #[tokio::test]
    async fn sdk_compatibility_replays_completed_output_items_before_function_output() {
        let fixture = Fixture::start(FixtureReply::Sse(COMPLETED_STREAM)).await;
        let mut first_stream = open_stream(&fixture).await.unwrap();
        let first_response = loop {
            if let WireEvent::Completed { response, .. } =
                next_event(&mut first_stream).await.unwrap()
            {
                break response;
            }
        };
        assert_eq!(first_response.output.len(), 2);
        assert!(first_stream.next().await.is_none());

        let call_id = first_response.output[1]["call_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut second_input = first_response.output.clone();
        second_input.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": "1\n"
        }));
        let mut second_request = execute_python_request();
        second_request["input"] = Value::Array(second_input);

        let mut second_stream = open_stream_with_request(&fixture, second_request)
            .await
            .unwrap();
        while let Some(event) = second_stream.next().await {
            event.unwrap();
        }

        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.body["store"] == json!(false))
        );
        assert_eq!(
            requests[1].body["input"],
            json!([
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "Checking"}],
                    "encrypted_content": "encrypted-reasoning",
                    "status": "completed"
                },
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "execute_python",
                    "arguments": r#"{"code":"print(1)"}"#,
                    "status": "completed"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "1\n"
                }
            ])
        );
    }

    #[tokio::test]
    async fn sdk_compatibility_decodes_failed_event_and_ends_stream() {
        let fixture = Fixture::start(FixtureReply::Sse(FAILED_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::Failed {
                sequence_number,
                response,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(response.id, "resp_failed");
                assert_eq!(response.status, WireStatus::Failed);
                let error = response.error.unwrap();
                assert_eq!(error.code, "server_error");
                assert_eq!(error.message, "failed");
            }
            event => panic!("unexpected failed event: {event:?}"),
        }
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_decodes_incomplete_event_and_ends_stream() {
        let fixture = Fixture::start(FixtureReply::Sse(INCOMPLETE_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::Incomplete {
                sequence_number,
                response,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(response.id, "resp_incomplete");
                assert_eq!(response.status, WireStatus::Incomplete);
                assert_eq!(
                    response.incomplete_details.unwrap().reason,
                    "max_output_tokens"
                );
            }
            event => panic!("unexpected incomplete event: {event:?}"),
        }
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_decodes_error_event_and_ends_stream() {
        let fixture = Fixture::start(FixtureReply::Sse(ERROR_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::Error {
                sequence_number,
                code,
                message,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(code.as_deref(), Some("stream_error"));
                assert_eq!(message, "stream failed");
            }
            event => panic!("unexpected error event: {event:?}"),
        }
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_makes_one_request_for_a_transient_response() {
        let fixture = Fixture::start(FixtureReply::Status {
            code: 429,
            body: r#"{"error":{"message":"retry later","type":"rate_limit_error","param":null,"code":"rate_limit"}}"#,
        })
        .await;

        assert!(open_stream(&fixture).await.is_err());
        sleep(Duration::from_millis(100)).await;
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_reports_an_abrupt_stream_close() {
        let fixture = Fixture::start(FixtureReply::AbruptSse(ONE_DELTA)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        assert!(matches!(
            next_event(&mut stream).await.unwrap(),
            WireEvent::ReasoningSummaryDelta { delta, .. } if delta == "partial"
        ));
        assert!(next_event(&mut stream).await.is_err());
    }

    #[tokio::test]
    async fn sdk_compatibility_dropped_stream_closes_the_fixture_connection() {
        let fixture = Fixture::start(FixtureReply::IdleSse(ONE_DELTA)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        assert!(matches!(
            next_event(&mut stream).await.unwrap(),
            WireEvent::ReasoningSummaryDelta { delta, .. } if delta == "partial"
        ));
        drop(stream);

        fixture.wait_for_disconnect().await;
        assert_eq!(fixture.request_count(), 1);
    }

    mod streaming {
        use std::sync::{Arc, Mutex};
        use std::time::Instant;

        use tokio::time::timeout;
        use tokio_util::sync::CancellationToken;

        use super::super::{
            APPLICATION_CONTENT_LIMIT, OpenAiModel, PROGRESS_PAYLOAD_LIMIT, PROGRESS_RATE_LIMIT,
            PresentationBatcher, PresentationKind,
        };
        use super::*;
        use crate::model::{
            AttemptAccounting, Model, ModelAction, ModelCallId, ModelError, ModelHistory,
            ModelProgress, ModelUsage,
        };

        const STREAMED_PREFIX: &str = concat!(
            "data: {\"type\":\"response.output_item.added\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"execute_python\",\"arguments\":\"\",\"status\":\"in_progress\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":2,\"item_id\":\"fc_1\",\"output_index\":0,\"delta\":\"{\\\"code\\\":\\\"print('preview')\"}\n\n",
        );

        fn usage() -> Value {
            json!({
                "input_tokens": 23,
                "input_tokens_details": {
                    "cached_tokens": 5,
                    "cache_write_tokens": 7
                },
                "output_tokens": 11,
                "output_tokens_details": {"reasoning_tokens": 3},
                "total_tokens": 34
            })
        }

        fn expected_accounting() -> AttemptAccounting {
            AttemptAccounting::KnownUsage(ModelUsage {
                model: "gpt-5.6-sol".to_owned(),
                service_tier: Some("default".to_owned()),
                input_tokens: Some(23),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(7),
                output_tokens: Some(11),
                reasoning_output_tokens: Some(3),
                total_tokens: Some(34),
            })
        }

        fn reasoning(id: &str, text: &str) -> Value {
            json!({
                "type": "reasoning",
                "id": id,
                "summary": [{"type": "summary_text", "text": text}],
                "encrypted_content": "encrypted",
                "status": "completed"
            })
        }

        fn function_call(arguments: &str) -> Value {
            json!({
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "execute_python",
                "arguments": arguments,
                "status": "completed"
            })
        }

        fn message(text: &str) -> Value {
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }],
                "status": "completed"
            })
        }

        fn function_added(sequence_number: u64, output_index: usize, item_id: &str) -> Value {
            json!({
                "type": "response.output_item.added",
                "sequence_number": sequence_number,
                "output_index": output_index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": "call_1",
                    "name": "execute_python",
                    "arguments": "",
                    "status": "in_progress"
                }
            })
        }

        fn completed_event(sequence_number: u64, output: Vec<Value>) -> Value {
            json!({
                "type": "response.completed",
                "sequence_number": sequence_number,
                "response": {
                    "id": "resp_streaming",
                    "status": "completed",
                    "service_tier": "default",
                    "output": output,
                    "usage": usage()
                }
            })
        }

        fn sse_event(event: Value) -> String {
            format!("data: {event}\n\n")
        }

        fn event_stream(events: impl IntoIterator<Item = Value>) -> String {
            let mut stream = String::new();
            for event in events {
                stream.push_str(&sse_event(event));
            }
            stream.push_str("data: [DONE]\n\n");
            stream
        }

        fn history() -> ModelHistory {
            let mut history = ModelHistory::new("read files exactly");
            history.append_user_request("inspect");
            history
        }

        async fn step(
            fixture: &Fixture,
            history: &ModelHistory,
        ) -> (
            Result<crate::model::ModelStep, ModelError>,
            Vec<ModelProgress>,
        ) {
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut events = Vec::new();
            let result = {
                let mut progress = |event| events.push(event);
                model.step(history, &cancellation, &mut progress).await
            };
            (result, events)
        }

        #[tokio::test]
        async fn emits_ordered_typed_chunks_before_settled_usage_and_result() {
            let arguments = r#"{"code":"print(1)"}"#;
            let stream = event_stream([
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 1,
                    "item_id": "rs_1",
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": "first"
                }),
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": 2,
                    "item_id": "msg_candidate",
                    "output_index": 2,
                    "content_index": 0,
                    "delta": "assistant"
                }),
                function_added(3, 1, "fc_1"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 4,
                    "item_id": "fc_1",
                    "output_index": 1,
                    "delta": r#"{"code":"pri"#
                }),
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 5,
                    "item_id": "rs_1",
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": "second"
                }),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 6,
                    "item_id": "fc_1",
                    "output_index": 1,
                    "delta": r#"nt(1)"}"#
                }),
                completed_event(
                    7,
                    vec![reasoning("rs_1", "firstsecond"), function_call(arguments)],
                ),
            ]);
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream)).await;

            let (result, progress) = step(&fixture, &history()).await;

            assert_eq!(
                result.unwrap().action,
                ModelAction::Code {
                    source: "print(1)".to_owned(),
                    call_id: ModelCallId::new("call_1"),
                }
            );
            assert_eq!(
                progress,
                vec![
                    ModelProgress::AttemptStarted { attempt: 1 },
                    ModelProgress::ReasoningSummaryChunk {
                        text: "first".to_owned(),
                    },
                    ModelProgress::AssistantTextChunk {
                        text: "assistant".to_owned(),
                    },
                    ModelProgress::DecodedPythonChunk {
                        code: "pri".to_owned(),
                    },
                    ModelProgress::ReasoningSummaryChunk {
                        text: "second".to_owned(),
                    },
                    ModelProgress::DecodedPythonChunk {
                        code: "nt(1)".to_owned(),
                    },
                    ModelProgress::AttemptSettled {
                        attempt: 1,
                        accounting: expected_accounting(),
                    },
                ]
            );
        }

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

        #[tokio::test]
        async fn ignores_python_argument_deltas_before_the_output_item_is_added() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 1,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"{"code":"before"}"#
                }),
                function_added(2, 0, "fc_1"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 3,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"{"code":"after"}"#
                }),
                completed_event(4, vec![function_call(r#"{"code":"final"}"#)]),
            ])))
            .await;

            let (result, progress) = step(&fixture, &history()).await;

            assert_eq!(
                result.unwrap().action,
                ModelAction::Code {
                    source: "final".to_owned(),
                    call_id: ModelCallId::new("call_1"),
                }
            );
            assert_eq!(
                progress
                    .iter()
                    .filter_map(|event| match event {
                        ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                "after"
            );
        }

        #[tokio::test]
        async fn ignores_python_argument_deltas_with_a_mismatched_item_id() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                function_added(1, 0, "fc_1"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 2,
                    "item_id": "fc_other",
                    "output_index": 0,
                    "delta": r#"{"code":"wrong"}"#
                }),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 3,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"{"code":"right"}"#
                }),
                completed_event(4, vec![function_call(r#"{"code":"final"}"#)]),
            ])))
            .await;

            let (result, progress) = step(&fixture, &history()).await;

            assert!(result.is_ok());
            assert_eq!(
                progress
                    .iter()
                    .filter_map(|event| match event {
                        ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                "right"
            );
        }

        #[tokio::test]
        async fn unrelated_output_item_added_at_same_index_preserves_active_python_decoder() {
            let unrelated_items = [
                ("different Python call", function_added(3, 0, "fc_other")),
                (
                    "non-Python output item",
                    json!({
                        "type": "response.output_item.added",
                        "sequence_number": 3,
                        "output_index": 0,
                        "item": {
                            "type": "message",
                            "id": "msg_other",
                            "role": "assistant",
                            "content": [],
                            "status": "in_progress"
                        }
                    }),
                ),
            ];

            for (case, unrelated_item) in unrelated_items {
                let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                    function_added(1, 0, "fc_1"),
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": 2,
                        "item_id": "fc_1",
                        "output_index": 0,
                        "delta": r#"{"code":"before "#
                    }),
                    unrelated_item,
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": 4,
                        "item_id": "fc_1",
                        "output_index": 0,
                        "delta": r#"after"}"#
                    }),
                    completed_event(5, vec![function_call(r#"{"code":"before after"}"#)]),
                ])))
                .await;

                let (result, progress) = step(&fixture, &history()).await;

                assert!(result.is_ok(), "{case}");
                assert_eq!(
                    progress
                        .iter()
                        .filter_map(|event| match event {
                            ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "before after",
                    "{case}"
                );
            }
        }

        #[tokio::test]
        async fn duplicate_output_item_added_preserves_accumulated_python_decoder() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                function_added(1, 0, "fc_1"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 2,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"{"code":"before "#
                }),
                function_added(3, 0, "fc_1"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 4,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"after"}"#
                }),
                completed_event(5, vec![function_call(r#"{"code":"before after"}"#)]),
            ])))
            .await;

            let (result, progress) = step(&fixture, &history()).await;

            assert!(result.is_ok());
            assert_eq!(
                progress
                    .iter()
                    .filter_map(|event| match event {
                        ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                "before after"
            );
        }

        #[tokio::test]
        async fn mismatched_done_events_do_not_terminate_the_active_python_decoder() {
            let mut mismatched_item = function_call(r#"{"code":"wrong"}"#);
            mismatched_item["id"] = json!("fc_other");
            let done_cases = [
                (
                    "function arguments done",
                    json!({
                        "type": "response.function_call_arguments.done",
                        "sequence_number": 2,
                        "item_id": "fc_other",
                        "output_index": 0,
                        "arguments": r#"{"code":"wrong"}"#
                    }),
                ),
                (
                    "output item done",
                    json!({
                        "type": "response.output_item.done",
                        "sequence_number": 2,
                        "output_index": 0,
                        "item": mismatched_item
                    }),
                ),
            ];

            for (case, done) in done_cases {
                let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                    function_added(1, 0, "fc_1"),
                    done,
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": 3,
                        "item_id": "fc_1",
                        "output_index": 0,
                        "delta": r#"{"code":"right"}"#
                    }),
                    completed_event(4, vec![function_call(r#"{"code":"final"}"#)]),
                ])))
                .await;

                let (result, progress) = step(&fixture, &history()).await;

                assert!(result.is_ok(), "{case}");
                assert_eq!(
                    progress
                        .iter()
                        .filter_map(|event| match event {
                            ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    "right",
                    "{case}"
                );
            }
        }

        #[tokio::test]
        async fn ignores_python_argument_deltas_after_arguments_or_output_item_done() {
            let done_cases = [
                (
                    "function arguments done",
                    vec![
                        function_added(1, 0, "fc_1"),
                        json!({
                            "type": "response.function_call_arguments.done",
                            "sequence_number": 2,
                            "item_id": "fc_1",
                            "output_index": 0,
                            "arguments": r#"{"code":"final"}"#
                        }),
                    ],
                ),
                (
                    "output item done after added",
                    vec![
                        function_added(1, 0, "fc_1"),
                        json!({
                            "type": "response.output_item.done",
                            "sequence_number": 2,
                            "output_index": 0,
                            "item": function_call(r#"{"code":"final"}"#)
                        }),
                    ],
                ),
                (
                    "output item done without added",
                    vec![json!({
                        "type": "response.output_item.done",
                        "sequence_number": 1,
                        "output_index": 0,
                        "item": function_call(r#"{"code":"final"}"#)
                    })],
                ),
            ];

            for (case, mut events) in done_cases {
                events.push(json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 3,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"{"code":"after"}"#
                }));
                events.push(completed_event(
                    4,
                    vec![function_call(r#"{"code":"final"}"#)],
                ));
                let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream(events))).await;

                let (result, progress) = step(&fixture, &history()).await;

                assert!(result.is_ok(), "{case}");
                assert!(
                    progress
                        .iter()
                        .all(|event| !matches!(event, ModelProgress::DecodedPythonChunk { .. })),
                    "{case}"
                );
            }
        }

        #[tokio::test]
        async fn splits_chunks_at_four_kib_on_utf8_boundaries() {
            let assistant = "€".repeat(3_000);
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 1,
                    "item_id": "rs_1",
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": "open"
                }),
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": 2,
                    "item_id": "msg_1",
                    "output_index": 1,
                    "content_index": 0,
                    "delta": assistant
                }),
                completed_event(3, vec![message("done")]),
            ])))
            .await;

            let (result, progress) = step(&fixture, &history()).await;
            assert!(result.is_ok());
            let chunks: Vec<&str> = progress
                .iter()
                .filter_map(|event| match event {
                    ModelProgress::AssistantTextChunk { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();

            assert!(chunks.len() >= 3);
            assert!(chunks.iter().all(|chunk| chunk.len() <= 4 * 1024));
            assert_eq!(chunks.concat(), assistant);
        }

        #[tokio::test]
        async fn flushes_a_short_pending_chunk_before_terminal_settlement() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 1,
                    "item_id": "rs_1",
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": "first"
                }),
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": 2,
                    "item_id": "msg_1",
                    "output_index": 1,
                    "content_index": 0,
                    "delta": "short"
                }),
                completed_event(3, vec![message("done")]),
            ])))
            .await;

            let (result, progress) = step(&fixture, &history()).await;

            assert!(result.is_ok());
            assert_eq!(
                &progress[progress.len() - 2..],
                &[
                    ModelProgress::AssistantTextChunk {
                        text: "short".to_owned(),
                    },
                    ModelProgress::AttemptSettled {
                        attempt: 1,
                        accounting: expected_accounting(),
                    },
                ]
            );
        }

        #[tokio::test]
        async fn decodes_split_json_escapes_unicode_surrogates_and_utf8_for_preview() {
            let fragments = [
                " \n { \"co",
                "de\" \t: \"quote: \\",
                "\" slash: \\",
                "/ backslash: \\",
                "\\ newline:\\",
                "n lambda:\\u",
                "03",
                "BB emoji:\\uD8",
                "3D\\",
                "uDE",
                "00 utf8:é",
                "\" } \t",
            ];
            let arguments = fragments.concat();
            let mut events = vec![function_added(1, 0, "fc_1")];
            events.extend(fragments.iter().enumerate().map(|(index, fragment)| {
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": index + 2,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": fragment
                })
            }));
            events.push(completed_event(
                fragments.len() as u64 + 2,
                vec![function_call(&arguments)],
            ));
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream(events))).await;

            let (result, progress) = step(&fixture, &history()).await;
            let expected = "quote: \" slash: / backslash: \\ newline:\n lambda:λ emoji:😀 utf8:é";

            assert_eq!(
                result.unwrap().action,
                ModelAction::Code {
                    source: expected.to_owned(),
                    call_id: ModelCallId::new("call_1"),
                }
            );
            assert_eq!(
                progress
                    .iter()
                    .filter_map(|event| match event {
                        ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
                expected
            );
        }

        #[tokio::test]
        async fn malformed_or_incomplete_argument_prefixes_cannot_replace_the_final_action() {
            for (prefix, expected_preview) in [
                (r#"{"other":"ignored"}"#, ""),
                (r#"{"code":"bad\u12X"#, "bad"),
                (r#"{"code":"half\uD83D"#, "half"),
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                    function_added(1, 0, "fc_1"),
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": 2,
                        "item_id": "fc_1",
                        "output_index": 0,
                        "delta": prefix
                    }),
                    completed_event(3, vec![function_call(r#"{"code":"final"}"#)]),
                ])))
                .await;

                let (result, progress) = step(&fixture, &history()).await;

                assert_eq!(
                    result.unwrap().action,
                    ModelAction::Code {
                        source: "final".to_owned(),
                        call_id: ModelCallId::new("call_1"),
                    }
                );
                assert_eq!(
                    progress
                        .iter()
                        .filter_map(|event| match event {
                            ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    expected_preview
                );
            }

            let original_history = history();
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                function_added(1, 0, "fc_1"),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 2,
                    "item_id": "fc_1",
                    "output_index": 0,
                    "delta": r#"{"code":"preview"}"#
                }),
                completed_event(3, vec![function_call(r#"{"code":"unterminated}"#)]),
            ])))
            .await;

            let (result, progress) = step(&fixture, &original_history).await;

            assert!(matches!(result, Err(ModelError::InvalidResponse(_))));
            assert!(progress.iter().any(|event| {
                matches!(
                    event,
                    ModelProgress::DecodedPythonChunk { code } if code == "preview"
                )
            }));
            assert_eq!(original_history, history());
        }

        #[tokio::test]
        async fn flushes_pending_progress_when_the_content_guard_is_exhausted() {
            let oversized = "x".repeat(APPLICATION_CONTENT_LIMIT);
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 1,
                    "item_id": "rs_1",
                    "output_index": 0,
                    "summary_index": 0,
                    "delta": "first"
                }),
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": 2,
                    "item_id": "msg_1",
                    "output_index": 1,
                    "content_index": 0,
                    "delta": "pending"
                }),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 3,
                    "item_id": "fc_1",
                    "output_index": 2,
                    "delta": oversized
                }),
            ])))
            .await;

            let (result, progress) = step(&fixture, &history()).await;

            assert_eq!(result, Err(ModelError::ContentTooLarge));
            assert_eq!(
                progress.last(),
                Some(&ModelProgress::AssistantTextChunk {
                    text: "pending".to_owned(),
                })
            );
            assert!(
                !progress
                    .iter()
                    .any(|event| matches!(event, ModelProgress::AttemptSettled { .. }))
            );
        }

        #[tokio::test]
        async fn streamed_python_prefix_neither_returns_an_action_nor_enters_history() {
            let fixture = Fixture::start(FixtureReply::IdleSse(STREAMED_PREFIX)).await;
            let original_history = history();
            let step_history = original_history.clone();
            let progress = Arc::new(Mutex::new(Vec::new()));
            let collected = progress.clone();
            let endpoint = fixture.endpoint.clone();
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            let task = tokio::spawn(async move {
                let mut model =
                    OpenAiModel::with_endpoint("system prompt", "fixture-key", &endpoint);
                let mut sink = |event| collected.lock().unwrap().push(event);
                model.step(&step_history, &cancellation, &mut sink).await
            });

            timeout(Duration::from_secs(1), async {
                while !progress.lock().unwrap().iter().any(|event| {
                    matches!(
                        event,
                        ModelProgress::DecodedPythonChunk { code } if code == "print('preview')"
                    )
                }) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(!task.is_finished());
            assert_eq!(original_history, history());

            cancel.cancel();
            assert_eq!(task.await.unwrap(), Err(ModelError::Cancelled));
            fixture.wait_for_disconnect().await;
        }
    }

    mod action_protocol {
        use tokio_util::sync::CancellationToken;

        use super::super::{APPLICATION_CONTENT_LIMIT, ApplicationContentGuard, OpenAiModel};
        use super::*;
        use crate::model::{
            AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelContextEntry,
            ModelError, ModelHistory, ModelUsage, OpaqueReplayItem, ProviderReplay,
        };

        const COMPLETED_ITEM_ONLY_STREAM: &str = "data: {\"type\":\"response.output_item.done\",\"sequence_number\":1,\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_candidate\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"not terminal\",\"annotations\":[]}],\"status\":\"completed\"}}\n\n";

        fn usage() -> Value {
            json!({
                "input_tokens": 23,
                "input_tokens_details": {
                    "cached_tokens": 5,
                    "cache_write_tokens": 7
                },
                "output_tokens": 11,
                "output_tokens_details": {"reasoning_tokens": 3},
                "total_tokens": 34
            })
        }

        fn reasoning(id: &str, text: &str, encrypted_content: &str) -> Value {
            json!({
                "type": "reasoning",
                "id": id,
                "summary": [{"type": "summary_text", "text": text}],
                "encrypted_content": encrypted_content,
                "status": "completed"
            })
        }

        fn function_call(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
            json!({
                "type": "function_call",
                "id": id,
                "call_id": call_id,
                "name": name,
                "arguments": arguments,
                "status": "completed"
            })
        }

        fn message(id: &str, text: &str) -> Value {
            json!({
                "type": "message",
                "id": id,
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": []
                }],
                "status": "completed"
            })
        }

        fn completed_event(
            sequence_number: u64,
            status: &str,
            output: Vec<Value>,
            usage: Option<Value>,
        ) -> Value {
            let mut response = json!({
                "id": "resp_action",
                "status": status,
                "service_tier": "default",
                "output": output
            });
            if let Some(usage) = usage {
                response["usage"] = usage;
            }
            json!({
                "type": "response.completed",
                "sequence_number": sequence_number,
                "response": response
            })
        }

        fn terminal_stream(status: &str, output: Vec<Value>, usage: Option<Value>) -> String {
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                completed_event(1, status, output, usage)
            )
        }

        fn event_stream(events: impl IntoIterator<Item = Value>) -> String {
            let mut stream = String::new();
            for event in events {
                stream.push_str("data: ");
                stream.push_str(&event.to_string());
                stream.push_str("\n\n");
            }
            stream.push_str("data: [DONE]\n\n");
            stream
        }

        fn replay(items: &[Value]) -> ProviderReplay {
            ProviderReplay::new(
                "openai",
                items
                    .iter()
                    .map(|item| OpaqueReplayItem::new(serde_json::to_vec(item).unwrap()))
                    .collect(),
            )
        }

        fn history(request: &str) -> ModelHistory {
            let mut history = ModelHistory::new("read files exactly");
            history.append_user_request(request);
            history
        }

        fn expected_request(instructions: &str, input: Value) -> Value {
            json!({
                "model": "gpt-5.6-sol",
                "instructions": instructions,
                "input": input,
                "stream": true,
                "store": false,
                "service_tier": "default",
                "tool_choice": "auto",
                "parallel_tool_calls": false,
                "reasoning": {
                    "effort": "medium",
                    "summary": "auto",
                    "context": "all_turns"
                },
                "include": ["reasoning.encrypted_content"],
                "tools": [{
                    "type": "function",
                    "name": "execute_python",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "code": {"type": "string"}
                        },
                        "required": ["code"],
                        "additionalProperties": false
                    }
                }]
            })
        }

        async fn step(
            fixture: &Fixture,
            history: &ModelHistory,
        ) -> Result<crate::model::ModelStep, ModelError> {
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = |_| {};
            model.step(history, &cancellation, &mut progress).await
        }

        async fn assert_rejected(output: Vec<Value>) {
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                output,
                Some(usage()),
            )))
            .await;

            let error = step(&fixture, &history("inspect")).await.unwrap_err();
            assert!(
                matches!(error, ModelError::InvalidResponse(_)),
                "unexpected rejection: {error:?}"
            );
            assert_eq!(fixture.request_count(), 1);
        }

        #[tokio::test]
        async fn builds_the_exact_first_request_and_returns_one_completed_code_action() {
            let output = vec![
                reasoning("rs_1", "Checking", "encrypted-reasoning"),
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#),
            ];
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                output.clone(),
                Some(usage()),
            )))
            .await;
            let history = history("inspect this project");

            let completed = step(&fixture, &history).await.unwrap();

            assert_eq!(
                completed.action,
                ModelAction::Code {
                    source: "print(1)".to_owned(),
                    call_id: ModelCallId::new("call_1")
                }
            );
            assert_eq!(completed.replay.provider(), "openai");
            assert_eq!(completed.replay.items().len(), 2);
            for (opaque, expected) in completed.replay.items().iter().zip(&output) {
                assert_eq!(
                    serde_json::from_slice::<Value>(opaque.as_bytes()).unwrap(),
                    *expected
                );
            }
            assert_eq!(
                completed.attempts,
                vec![AttemptAccounting::KnownUsage(ModelUsage {
                    model: "gpt-5.6-sol".to_owned(),
                    service_tier: Some("default".to_owned()),
                    input_tokens: Some(23),
                    cached_input_tokens: Some(5),
                    cache_write_input_tokens: Some(7),
                    output_tokens: Some(11),
                    reasoning_output_tokens: Some(3),
                    total_tokens: Some(34),
                })]
            );

            let captured = fixture.request();
            assert!(
                captured
                    .head
                    .contains("authorization: Bearer fixture-key\r\n")
            );
            let routing_headers = captured.head.to_ascii_lowercase();
            assert!(!routing_headers.contains("openai-organization:"));
            assert!(!routing_headers.contains("openai-project:"));
            assert_eq!(
                captured.body,
                expected_request(
                    "system prompt\n\nread files exactly",
                    json!([{"role": "user", "content": "inspect this project"}])
                )
            );
        }

        #[tokio::test]
        async fn accepts_statusless_reasoning_in_completed_sol_output() {
            let mut statusless_reasoning = reasoning("rs_1", "Checking", "encrypted-reasoning");
            statusless_reasoning
                .as_object_mut()
                .unwrap()
                .remove("status");
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                vec![
                    statusless_reasoning,
                    function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#),
                ],
                Some(usage()),
            )))
            .await;

            assert_eq!(
                step(&fixture, &history("inspect")).await.unwrap().action,
                ModelAction::Code {
                    source: "print(1)".to_owned(),
                    call_id: ModelCallId::new("call_1"),
                }
            );
        }

        #[tokio::test]
        async fn replays_exact_subsequent_history_and_returns_one_completed_message() {
            let code_output = vec![
                reasoning("rs_1", "First", "encrypted-first"),
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"1 + 1"}"#),
            ];
            let message_output = vec![
                reasoning("rs_2", "Second", "encrypted-second"),
                message("msg_1", "first answer"),
            ];
            let mut history = history("first request");
            history
                .append_completed_code(
                    "1 + 1".to_owned(),
                    ModelCallId::new("call_1"),
                    replay(&code_output),
                    CodeResult::Execution {
                        feedback: "2".to_owned(),
                    },
                )
                .unwrap();
            history.append_final_message("first answer".to_owned(), replay(&message_output));
            history.append_user_request("second request");
            let returned_output = vec![
                reasoning("rs_3", "Third", "encrypted-third"),
                message("msg_2", "second answer"),
            ];
            let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([
                json!({
                    "type": "response.created",
                    "sequence_number": 1,
                    "response": {"id": "resp_action", "status": "in_progress"}
                }),
                json!({
                    "type": "response.queued",
                    "sequence_number": 2,
                    "response": {"id": "resp_action", "status": "queued"}
                }),
                json!({
                    "type": "response.in_progress",
                    "sequence_number": 3,
                    "response": {"id": "resp_action", "status": "in_progress"}
                }),
                completed_event(4, "completed", returned_output, None),
            ])))
            .await;

            let completed = step(&fixture, &history).await.unwrap();

            assert_eq!(
                completed.action,
                ModelAction::Message {
                    text: "second answer".to_owned()
                }
            );
            assert_eq!(
                completed.attempts,
                vec![AttemptAccounting::UsageUnavailable]
            );
            assert_eq!(
                fixture.request().body,
                expected_request(
                    "system prompt\n\nread files exactly",
                    json!([
                        {"role": "user", "content": "first request"},
                        code_output[0].clone(),
                        code_output[1].clone(),
                        {
                            "type": "function_call_output",
                            "call_id": "call_1",
                            "output": "2"
                        },
                        message_output[0].clone(),
                        message_output[1].clone(),
                        {"role": "user", "content": "second request"}
                    ])
                )
            );
        }

        #[tokio::test]
        async fn rejects_mixed_multiple_unknown_malformed_refused_and_unexpected_output() {
            let valid_call =
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#);
            let cases = vec![
                vec![message("msg_1", "answer"), valid_call.clone()],
                vec![
                    valid_call.clone(),
                    function_call("fc_2", "call_2", "execute_python", r#"{"code":"print(2)"}"#),
                ],
                vec![message("msg_1", "one"), message("msg_2", "two")],
                vec![function_call(
                    "fc_1",
                    "call_1",
                    "other_function",
                    r#"{"code":"print(1)"}"#,
                )],
                vec![function_call(
                    "fc_1",
                    "call_1",
                    "execute_python",
                    r#"{"code":"print(1)","extra":true}"#,
                )],
                vec![function_call(
                    "fc_1",
                    "call_1",
                    "execute_python",
                    r#"{"missing":"code"}"#,
                )],
                vec![function_call(
                    "fc_1",
                    "call_1",
                    "execute_python",
                    r#"{"code":1}"#,
                )],
                vec![function_call(
                    "fc_1",
                    "call_1",
                    "execute_python",
                    r#"{"code":"unterminated}"#,
                )],
                vec![json!({
                    "type": "function_call",
                    "id": "fc_missing_arguments",
                    "call_id": "call_missing_arguments",
                    "name": "execute_python",
                    "status": "completed"
                })],
                vec![json!({
                    "type": "message",
                    "id": "msg_refusal",
                    "role": "assistant",
                    "content": [{"type": "refusal", "refusal": "no"}],
                    "status": "completed"
                })],
                vec![json!({
                    "type": "web_search_call",
                    "id": "search_1",
                    "status": "completed"
                })],
                vec![json!({
                    "type": "message",
                    "id": "msg_input",
                    "role": "assistant",
                    "content": [{"type": "input_text", "text": "wrong"}],
                    "status": "completed"
                })],
                vec![
                    json!({
                        "type": "reasoning",
                        "id": "rs_raw",
                        "summary": [],
                        "content": [{"type": "reasoning_text", "text": "hidden"}],
                        "encrypted_content": "encrypted",
                        "status": "completed"
                    }),
                    message("msg_1", "answer"),
                ],
                vec![],
                vec![message("msg_empty", "")],
            ];

            for output in cases {
                assert_rejected(output).await;
            }
        }

        #[tokio::test]
        async fn rejects_duplicate_nonempty_terminal_output_item_ids() {
            assert_rejected(vec![
                reasoning("duplicate", "Checking", "encrypted"),
                message("duplicate", "answer"),
            ])
            .await;
        }

        #[tokio::test]
        async fn rejects_invalid_reasoning_status_and_noncompleted_action_statuses() {
            let mut incomplete_reasoning = reasoning("rs_1", "Wait", "encrypted");
            incomplete_reasoning["status"] = json!("incomplete");
            let mut malformed_reasoning = reasoning("rs_1", "Wait", "encrypted");
            malformed_reasoning["status"] = Value::Null;
            let mut incomplete_call =
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#);
            incomplete_call["status"] = json!("incomplete");
            let mut statusless_call =
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#);
            statusless_call.as_object_mut().unwrap().remove("status");
            let mut incomplete_message = message("msg_1", "answer");
            incomplete_message["status"] = json!("incomplete");
            let mut statusless_message = message("msg_1", "answer");
            statusless_message.as_object_mut().unwrap().remove("status");

            for output in [
                vec![incomplete_reasoning, message("msg_1", "answer")],
                vec![malformed_reasoning, message("msg_1", "answer")],
                vec![incomplete_call],
                vec![statusless_call],
                vec![incomplete_message],
                vec![statusless_message],
            ] {
                assert_rejected(output).await;
            }
        }

        #[tokio::test]
        async fn rejects_failed_incomplete_error_eof_and_noncompleted_terminal_statuses() {
            for (stream, expected) in [
                (
                    FAILED_STREAM.to_owned(),
                    ModelError::Provider("response failed: server_error: failed".to_owned()),
                ),
                (
                    INCOMPLETE_STREAM.to_owned(),
                    ModelError::Provider("response incomplete: max_output_tokens".to_owned()),
                ),
                (
                    ERROR_STREAM.to_owned(),
                    ModelError::Provider("stream error (stream_error): stream failed".to_owned()),
                ),
                (
                    "data: [DONE]\n\n".to_owned(),
                    ModelError::InvalidResponse("stream ended without a terminal response"),
                ),
                (
                    terminal_stream(
                        "incomplete",
                        vec![message("msg_1", "answer")],
                        Some(usage()),
                    ),
                    ModelError::InvalidResponse(
                        "response.completed did not contain completed status",
                    ),
                ),
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream)).await;

                assert_eq!(step(&fixture, &history("inspect")).await, Err(expected));
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn rejects_a_call_identifier_already_present_in_history() {
            let prior_output = vec![function_call(
                "fc_prior",
                "call_duplicate",
                "execute_python",
                r#"{"code":"1"}"#,
            )];
            let mut history = history("first request");
            history
                .append_completed_code(
                    "1".to_owned(),
                    ModelCallId::new("call_duplicate"),
                    replay(&prior_output),
                    CodeResult::Execution {
                        feedback: "1".to_owned(),
                    },
                )
                .unwrap();
            history.append_user_request("continue");
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                vec![function_call(
                    "fc_new",
                    "call_duplicate",
                    "execute_python",
                    r#"{"code":"2"}"#,
                )],
                Some(usage()),
            )))
            .await;

            assert_eq!(
                step(&fixture, &history).await,
                Err(ModelError::InvalidResponse(
                    "function call identifier duplicates model history"
                ))
            );
            assert_eq!(fixture.request_count(), 1);
        }

        #[tokio::test]
        async fn completed_output_item_without_response_completed_remains_pending() {
            let fixture = Fixture::start(FixtureReply::IdleSse(COMPLETED_ITEM_ONLY_STREAM)).await;
            let original_history = history("inspect");
            let history = original_history.clone();
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = |_| {};
            let mut pending = Box::pin(model.step(&history, &cancellation, &mut progress));

            assert!(
                timeout(Duration::from_millis(20), &mut pending)
                    .await
                    .is_err()
            );
            assert_eq!(history, original_history);

            cancellation.cancel();
            assert_eq!(pending.await, Err(ModelError::Cancelled));
            fixture.wait_for_disconnect().await;
            assert_eq!(fixture.request_count(), 1);
            assert!(history.entries().iter().all(|entry| matches!(
                entry,
                ModelContextEntry::ReadCapability { .. } | ModelContextEntry::UserRequest { .. }
            )));
        }

        #[tokio::test]
        async fn application_content_guard_counts_all_guarded_fields_as_events_arrive() {
            let quarter = APPLICATION_CONTENT_LIMIT / 4;
            let fragment = "x".repeat(quarter);
            let stream = event_stream([
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": 1,
                    "item_id": "msg_1",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": fragment
                }),
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 2,
                    "item_id": "rs_1",
                    "output_index": 1,
                    "summary_index": 0,
                    "delta": fragment
                }),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 3,
                    "item_id": "fc_1",
                    "output_index": 2,
                    "delta": fragment
                }),
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": 4,
                    "output_index": 1,
                    "item": {
                        "type": "reasoning",
                        "id": "rs_encrypted",
                        "summary": [],
                        "encrypted_content": format!("{fragment}x"),
                        "status": "completed"
                    }
                }),
                json!({
                    "type": "response.completed",
                    "sequence_number": 5,
                    "response": {
                        "id": "must_not_become_an_action",
                        "status": "completed",
                        "service_tier": "default",
                        "output": [message("msg_terminal", "must not return")],
                        "usage": usage()
                    }
                }),
            ]);
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream)).await;

            assert_eq!(
                step(&fixture, &history("inspect")).await,
                Err(ModelError::ContentTooLarge)
            );
            assert_eq!(fixture.request_count(), 1);
        }

        #[test]
        fn application_content_guard_combines_terminal_reasoning_by_output_index() {
            for ids in [["rs_1", "rs_2"], ["duplicate", "duplicate"]] {
                let mut first = reasoning(ids[0], "", "");
                first["encrypted_content"] =
                    Value::String("x".repeat(APPLICATION_CONTENT_LIMIT / 2 + 1));
                let mut second = reasoning(ids[1], "", "");
                second["encrypted_content"] =
                    Value::String("x".repeat(APPLICATION_CONTENT_LIMIT / 2 + 1));
                let mut content = ApplicationContentGuard::default();

                assert_eq!(
                    content.account_output(&[first, second]),
                    Err(ModelError::ContentTooLarge)
                );
            }
        }

        #[tokio::test]
        async fn rejects_invalid_stored_replays_without_making_a_request() {
            let mut provider_mismatch = history("first request");
            provider_mismatch.append_final_message(
                "answer".to_owned(),
                ProviderReplay::new(
                    "other-provider",
                    vec![OpaqueReplayItem::new(b"{}".to_vec())],
                ),
            );
            provider_mismatch.append_user_request("continue");

            let mut malformed_json = history("first request");
            malformed_json.append_final_message(
                "answer".to_owned(),
                ProviderReplay::new("openai", vec![OpaqueReplayItem::new(b"not json".to_vec())]),
            );
            malformed_json.append_user_request("continue");

            let mut semantically_malformed = history("first request");
            let mut non_assistant_message = message("msg_1", "answer");
            non_assistant_message["role"] = json!("user");
            semantically_malformed
                .append_final_message("answer".to_owned(), replay(&[non_assistant_message]));
            semantically_malformed.append_user_request("continue");

            let code_replay = replay(&[function_call(
                "fc_1",
                "call_1",
                "execute_python",
                r#"{"code":"1 + 1"}"#,
            )]);
            let mut source_mismatch = history("first request");
            source_mismatch
                .append_completed_code(
                    "2 + 2".to_owned(),
                    ModelCallId::new("call_1"),
                    code_replay.clone(),
                    CodeResult::Execution {
                        feedback: "2".to_owned(),
                    },
                )
                .unwrap();
            source_mismatch.append_user_request("continue");

            let mut call_id_mismatch = history("first request");
            call_id_mismatch
                .append_completed_code(
                    "1 + 1".to_owned(),
                    ModelCallId::new("call_2"),
                    code_replay,
                    CodeResult::Execution {
                        feedback: "2".to_owned(),
                    },
                )
                .unwrap();
            call_id_mismatch.append_user_request("continue");

            let mut text_mismatch = history("first request");
            text_mismatch.append_final_message(
                "different answer".to_owned(),
                replay(&[message("msg_1", "answer")]),
            );
            text_mismatch.append_user_request("continue");

            for (case, history, expected) in [
                (
                    "provider mismatch",
                    provider_mismatch,
                    ModelError::InvalidHistory("opaque replay belongs to another provider"),
                ),
                (
                    "malformed JSON",
                    malformed_json,
                    ModelError::InvalidHistory("opaque OpenAI replay is malformed"),
                ),
                (
                    "semantically malformed replay",
                    semantically_malformed,
                    ModelError::InvalidHistory("response message did not have assistant role"),
                ),
                (
                    "completed code source mismatch",
                    source_mismatch,
                    ModelError::InvalidHistory("completed code does not match its OpenAI replay"),
                ),
                (
                    "completed code call ID mismatch",
                    call_id_mismatch,
                    ModelError::InvalidHistory("completed code does not match its OpenAI replay"),
                ),
                (
                    "final message text mismatch",
                    text_mismatch,
                    ModelError::InvalidHistory("final message does not match its OpenAI replay"),
                ),
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                    "completed",
                    vec![message("msg_1", "answer")],
                    Some(usage()),
                )))
                .await;

                let result = step(&fixture, &history).await;

                assert_eq!(fixture.request_count(), 0, "{case}");
                assert_eq!(result, Err(expected), "{case}");
            }
        }
    }
}
