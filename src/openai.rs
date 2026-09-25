use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::sync::Arc;

use async_openai::Client;
use async_openai::config::{OPENAI_API_BASE, OpenAIConfig};
use async_openai::error::OpenAIError;
use async_openai::middleware::{ReqwestService, retry::OpenAIRetryLayer};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tower::ServiceBuilder;

use crate::{
    config::{
        ModelConfiguration, ModelSelection, constants::ACCEPTED_MODEL_CONTENT_LIMIT,
        models::Provider, settings::CrossProviderReasoning,
    },
    model::{
        AttemptAccounting, CodeResult, ForeignPart, Model, ModelAction, ModelCallId,
        ModelContextEntry, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ModelUsage, OpaqueReplayItem, ProviderFailure, ProviderReplay, SummaryRequest,
        SummaryResult, canonical_compaction_summary,
    },
    presentation::{PresentationBatcher, PresentationKind, PythonPreviewDecoder},
};

const OPENAI_PROVIDER: &str = "openai";
const MAX_STREAM_ATTEMPTS: u32 = 3;
const STREAM_RESTART_DELAY: Duration = Duration::from_millis(100);

type WireStream = Pin<Box<dyn Stream<Item = Result<WireEvent, OpenAIError>> + Send>>;

fn openai_config(api_key: impl Into<String>, api_base: impl Into<String>) -> OpenAIConfig {
    OpenAIConfig::new()
        .with_api_base(api_base)
        .with_api_key(api_key)
        .with_org_id("")
        .with_project_id("")
}

#[derive(Clone)]
pub(crate) struct OpenAiModel {
    prompt: String,
    client: Client<OpenAIConfig>,
    api_key: String,
    cross_provider_reasoning: CrossProviderReasoning,
    #[cfg(test)]
    summary_retry_wait_started: Option<Arc<Notify>>,
}

impl OpenAiModel {
    pub(crate) fn new(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self {
        Self::build(
            prompt.into(),
            api_key.into(),
            OPENAI_API_BASE,
            cross_provider_reasoning,
        )
    }

    pub(crate) fn with_prompt(&self, prompt: String) -> Self {
        Self {
            prompt,
            ..self.clone()
        }
    }

    fn build(
        prompt: String,
        api_key: String,
        endpoint: &str,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self {
        let service = ServiceBuilder::new()
            .layer(OpenAIRetryLayer::new(2))
            .service(ReqwestService::default());
        Self {
            prompt,
            client: Client::with_config(openai_config(api_key.clone(), endpoint))
                .with_http_service(service),
            api_key,
            cross_provider_reasoning,
            #[cfg(test)]
            summary_retry_wait_started: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_endpoint(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        endpoint: &str,
    ) -> Self {
        Self::build(
            prompt.into(),
            api_key.into(),
            endpoint,
            CrossProviderReasoning::Inline,
        )
    }

    fn request(
        &self,
        selection: ModelSelection,
        history: &ModelHistory,
    ) -> Result<Value, ModelError> {
        let model = selection.model();
        let wire_model = openai_wire_model(model)?;
        let mut input = Vec::new();

        for entry in history.entries() {
            if let Some(parts) = entry.foreign_parts(OPENAI_PROVIDER, self.cross_provider_reasoning)
            {
                input.extend(parts.into_iter().flat_map(foreign_input));
                continue;
            }
            match entry {
                ModelContextEntry::UserRequest { text } => {
                    input.push(json!({"role": "user", "content": text}));
                }
                ModelContextEntry::CompactionSummary { summary } => {
                    input.push(json!({
                        "role": "user",
                        "content": canonical_compaction_summary(summary)
                    }));
                }
                ModelContextEntry::CompletedCode(completed) => {
                    let replay = replay_values(completed.replay())?;
                    match validate_replay_output(&replay)? {
                        ValidatedAction::Code {
                            source,
                            call_id,
                            commentary,
                        } if source == completed.source()
                            && call_id == *completed.call_id()
                            && commentary.as_deref() == completed.commentary() => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "completed code does not match its OpenAI replay",
                            ));
                        }
                    }
                    input.extend(replay);
                    let output = match completed.result() {
                        CodeResult::Execution { feedback }
                        | CodeResult::Cancelled { feedback }
                        | CodeResult::Failure { feedback } => feedback,
                    };
                    input.push(json!({
                        "type": "function_call_output",
                        "call_id": completed.call_id().as_str(),
                        "output": output
                    }));
                }
                ModelContextEntry::FinalMessage {
                    text,
                    reasoning_summary: _,
                    replay,
                } => {
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
            "model": wire_model,
            "instructions": self.prompt,
            "input": input,
            "stream": true,
            "store": false,
            "service_tier": model.service_tier,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {
                "effort": selection.thinking(),
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

    async fn step_with_restarts(
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
        let mut attempts = Vec::new();

        for attempt in 1..=MAX_STREAM_ATTEMPTS {
            if cancellation.is_cancelled() {
                return Err(ModelFailure::new(ModelError::Cancelled, attempts));
            }
            progress(ModelProgress::AttemptStarted { attempt });

            match self
                .one_attempt(request.clone(), selection, history, cancellation, progress)
                .await
            {
                Ok(mut step) => {
                    let accounting = step
                        .attempts
                        .pop()
                        .expect("completed OpenAI stream has accounting");
                    progress(ModelProgress::AttemptSettled {
                        attempt,
                        accounting: accounting.clone(),
                    });
                    attempts.push(accounting);
                    step.attempts = attempts;
                    return Ok(step);
                }
                Err(AttemptFailure::Cancelled) => {
                    let accounting = AttemptAccounting::UsageUnavailable;
                    progress(ModelProgress::AttemptSettled {
                        attempt,
                        accounting: accounting.clone(),
                    });
                    attempts.push(accounting);
                    return Err(ModelFailure::new(ModelError::Cancelled, attempts));
                }
                Err(AttemptFailure::Restartable(failure)) => {
                    let accounting = AttemptAccounting::UsageUnavailable;
                    progress(ModelProgress::AttemptSettled {
                        attempt,
                        accounting: accounting.clone(),
                    });
                    attempts.push(accounting);
                    if attempt == MAX_STREAM_ATTEMPTS {
                        return Err(ModelFailure::new(
                            ModelError::Provider(Box::new(failure)),
                            attempts,
                        ));
                    }
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            return Err(ModelFailure::new(ModelError::Cancelled, attempts));
                        }
                        _ = tokio::time::sleep(STREAM_RESTART_DELAY) => {}
                    }
                }
                Err(AttemptFailure::Terminal { error, accounting }) => {
                    let accounting = *accounting;
                    progress(ModelProgress::AttemptSettled {
                        attempt,
                        accounting: accounting.clone(),
                    });
                    attempts.push(accounting);
                    return Err(ModelFailure::new(error, attempts));
                }
            }
        }

        unreachable!("stream attempts are bounded")
    }

    async fn one_attempt(
        &self,
        request: Value,
        selection: ModelSelection,
        history: &ModelHistory,
        cancellation: &CancellationToken,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<ModelStep, AttemptFailure> {
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        let responses = self.client.responses();
        let open_stream = responses.create_stream_byot::<_, WireEvent>(request);
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
            stream = open_stream => stream,
        };
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                if matches!(&error, OpenAIError::Reqwest(error) if error.is_request() && !error.is_connect())
                {
                    return Err(self.stream_failure(error));
                }
                return Err(AttemptFailure::Terminal {
                    error: ModelError::Provider(Box::new(self.provider_failure(error))),
                    accounting: Box::new(AttemptAccounting::UsageUnavailable),
                });
            }
        };
        self.consume_stream(stream, selection, history, cancellation, progress)
            .await
    }

    async fn consume_stream(
        &self,
        mut stream: WireStream,
        selection: ModelSelection,
        history: &ModelHistory,
        cancellation: &CancellationToken,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<ModelStep, AttemptFailure> {
        let mut content = ApplicationContentGuard::default();
        let mut presentation = PresentationBatcher::default();
        let mut python_decoders = HashMap::<(usize, String), PythonPreviewDecoder>::new();

        loop {
            let event = tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    presentation.flush_all(progress);
                    return Err(AttemptFailure::Cancelled);
                }
                event = stream.next() => event,
            };
            if cancellation.is_cancelled() {
                presentation.flush_all(progress);
                return Err(AttemptFailure::Cancelled);
            }
            let Some(event) = event else {
                presentation.flush_all(progress);
                return Err(AttemptFailure::Restartable(self.diagnostic(
                    None,
                    None,
                    "provider stream ended without a terminal response",
                )));
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    presentation.flush_all(progress);
                    return Err(self.stream_failure(error));
                }
            };
            let now = Instant::now();
            let terminal_accounting = match &event {
                WireEvent::Completed { response, .. }
                | WireEvent::Failed { response, .. }
                | WireEvent::Incomplete { response, .. } => {
                    Some(self.response_accounting(response, selection))
                }
                _ => None,
            };
            let event_result = (|| -> Result<Option<WireResponse>, ModelError> {
                match event {
                    WireEvent::Completed { response, .. } => Ok(Some(response)),
                    WireEvent::Failed { response, .. } => Err(ModelError::Provider(Box::new(
                        self.response_failure("response failed", &response),
                    ))),
                    WireEvent::Incomplete { response, .. } => Err(ModelError::Provider(Box::new(
                        self.response_failure("response incomplete", &response),
                    ))),
                    WireEvent::Error { code, message, .. } => Err(ModelError::Provider(Box::new(
                        self.diagnostic(None, code, message),
                    ))),
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
                    WireEvent::RefusalDelta | WireEvent::RefusalDone => Err(ModelError::Provider(
                        Box::new(self.diagnostic(None, None, "model refused the request")),
                    )),
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
                    let accounting = terminal_accounting.expect("terminal response accounting");
                    return self
                        .completed_step(history, response, &mut content)
                        .map(|mut step| {
                            step.attempts.push(accounting.clone());
                            step
                        })
                        .map_err(|error| AttemptFailure::Terminal {
                            error,
                            accounting: Box::new(accounting),
                        });
                }
                Ok(None) => presentation.flush_due(now, progress),
                Err(error) => {
                    presentation.flush_all(progress);
                    return Err(AttemptFailure::Terminal {
                        error,
                        accounting: Box::new(
                            terminal_accounting.unwrap_or(AttemptAccounting::UsageUnavailable),
                        ),
                    });
                }
            }
        }
    }

    fn stream_failure(&self, error: OpenAIError) -> AttemptFailure {
        let restartable = matches!(error, OpenAIError::Reqwest(_) | OpenAIError::StreamError(_));
        let failure = self.provider_failure(error);
        if restartable {
            AttemptFailure::Restartable(failure)
        } else {
            AttemptFailure::Terminal {
                error: ModelError::Provider(Box::new(failure)),
                accounting: Box::new(AttemptAccounting::UsageUnavailable),
            }
        }
    }

    fn provider_failure(&self, error: OpenAIError) -> ProviderFailure {
        match error {
            OpenAIError::ApiError(error) => self.diagnostic(
                Some(error.status_code.as_u16()),
                error.api_error.code,
                error.api_error.message,
            ),
            OpenAIError::Reqwest(_) => self.diagnostic(None, None, "provider transport failed"),
            OpenAIError::StreamError(_) => self.diagnostic(None, None, "provider stream failed"),
            OpenAIError::JSONDeserialize(_, _) => {
                self.diagnostic(None, None, "provider response was invalid")
            }
            OpenAIError::InvalidArgument(_)
            | OpenAIError::FileSaveError(_)
            | OpenAIError::FileReadError(_)
            | OpenAIError::Boxed(_) => {
                self.diagnostic(None, None, "provider request could not be processed")
            }
        }
    }

    fn response_failure(&self, prefix: &str, response: &WireResponse) -> ProviderFailure {
        if let Some(error) = &response.error {
            self.diagnostic(
                None,
                Some(error.code.clone()),
                format!("{prefix}: {}", error.message),
            )
        } else if let Some(details) = &response.incomplete_details {
            self.diagnostic(
                None,
                Some(details.reason.clone()),
                format!("{prefix}: {}", details.reason),
            )
        } else {
            self.diagnostic(None, None, prefix)
        }
    }

    fn diagnostic(
        &self,
        status: Option<u16>,
        code: Option<String>,
        message: impl Into<String>,
    ) -> ProviderFailure {
        ProviderFailure::new(
            status,
            code.map(|value| redact(&value, &self.api_key)),
            redact(&message.into(), &self.api_key),
        )
    }

    fn response_accounting(
        &self,
        response: &WireResponse,
        selection: ModelSelection,
    ) -> AttemptAccounting {
        let Some(usage) = &response.usage else {
            return AttemptAccounting::UsageUnavailable;
        };
        AttemptAccounting::KnownUsageWithUntrackedRetries(ModelUsage {
            model: selection.model().id.to_owned(),
            service_tier: response
                .service_tier
                .as_deref()
                .map(|tier| redact(tier, &self.api_key)),
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.cached_tokens),
            cache_write_input_tokens: usage
                .input_tokens_details
                .as_ref()
                .and_then(|details| details.cache_write_tokens),
            cache_write_1h_input_tokens: 0,
            output_tokens: usage.output_tokens,
            reasoning_output_tokens: usage
                .output_tokens_details
                .as_ref()
                .and_then(|details| details.reasoning_tokens),
            total_tokens: usage.total_tokens,
        })
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
        let validated_action = validate_output(&response.output)?;
        content.account_output(&response.output)?;
        let reasoning_summary = readable_reasoning_summary(&response.output);
        let action = match validated_action {
            ValidatedAction::Code {
                source,
                call_id,
                commentary,
            } => {
                if history.contains_call_id(&call_id) {
                    return Err(ModelError::InvalidResponse(
                        "function call identifier duplicates model history",
                    ));
                }
                ModelAction::Code {
                    source,
                    call_id,
                    commentary,
                }
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
        Ok(ModelStep {
            action,
            reasoning_summary,
            replay,
            attempts: Vec::new(),
        })
    }

    fn summary_request(&self, request: &SummaryRequest) -> Result<Value, ModelError> {
        let model = request.selection.model();
        let wire_model = openai_wire_model(model)?;

        Ok(json!({
            "model": wire_model,
            "instructions": request.instructions,
            "input": request.input,
            "max_output_tokens": request.output_limit,
            "stream": true,
            "store": false,
            "service_tier": model.service_tier,
            "reasoning": {
                "effort": request.selection.thinking(),
                "summary": "auto",
                "context": "all_turns"
            },
            "include": ["reasoning.encrypted_content"]
        }))
    }

    async fn summary_with_restarts(
        &self,
        request: SummaryRequest,
        cancellation: &CancellationToken,
        attempt_started: &mut (dyn FnMut() + Send),
    ) -> Result<SummaryResult, ModelFailure> {
        if cancellation.is_cancelled() {
            return Err(ModelError::Cancelled.into());
        }
        let selection = request.selection;
        let request = self.summary_request(&request)?;
        let mut attempts = Vec::new();

        for attempt in 1..=MAX_STREAM_ATTEMPTS {
            if cancellation.is_cancelled() {
                return Err(ModelFailure::new(ModelError::Cancelled, attempts));
            }
            attempt_started();

            match self
                .one_summary_attempt(request.clone(), selection, cancellation)
                .await
            {
                Ok(mut result) => {
                    let accounting = result
                        .attempts
                        .pop()
                        .expect("completed OpenAI stream has accounting");
                    attempts.push(accounting);
                    result.attempts = attempts;
                    return Ok(result);
                }
                Err(AttemptFailure::Cancelled) => {
                    attempts.push(AttemptAccounting::UsageUnavailable);
                    return Err(ModelFailure::new(ModelError::Cancelled, attempts));
                }
                Err(AttemptFailure::Restartable(failure)) => {
                    attempts.push(AttemptAccounting::UsageUnavailable);
                    if attempt == MAX_STREAM_ATTEMPTS {
                        return Err(ModelFailure::new(
                            ModelError::Provider(Box::new(failure)),
                            attempts,
                        ));
                    }
                    #[cfg(test)]
                    if let Some(signal) = &self.summary_retry_wait_started {
                        signal.notify_one();
                    }
                    tokio::select! {
                        biased;
                        _ = cancellation.cancelled() => {
                            return Err(ModelFailure::new(ModelError::Cancelled, attempts));
                        }
                        _ = tokio::time::sleep(STREAM_RESTART_DELAY) => {}
                    }
                }
                Err(AttemptFailure::Terminal { error, accounting }) => {
                    attempts.push(*accounting);
                    return Err(ModelFailure::new(error, attempts));
                }
            }
        }

        unreachable!("stream attempts are bounded")
    }

    async fn one_summary_attempt(
        &self,
        request: Value,
        selection: ModelSelection,
        cancellation: &CancellationToken,
    ) -> Result<SummaryResult, AttemptFailure> {
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        let responses = self.client.responses();
        let open_stream = responses.create_stream_byot::<_, WireEvent>(request);
        let stream = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
            stream = open_stream => stream,
        };
        if cancellation.is_cancelled() {
            return Err(AttemptFailure::Cancelled);
        }
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                if matches!(&error, OpenAIError::Reqwest(error) if error.is_request() && !error.is_connect())
                {
                    return Err(self.stream_failure(error));
                }
                return Err(AttemptFailure::Terminal {
                    error: ModelError::Provider(Box::new(self.provider_failure(error))),
                    accounting: Box::new(AttemptAccounting::UsageUnavailable),
                });
            }
        };
        self.consume_summary_stream(stream, selection, cancellation)
            .await
    }

    async fn consume_summary_stream(
        &self,
        mut stream: WireStream,
        selection: ModelSelection,
        cancellation: &CancellationToken,
    ) -> Result<SummaryResult, AttemptFailure> {
        let mut content = ApplicationContentGuard::default();

        loop {
            let event = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AttemptFailure::Cancelled),
                event = stream.next() => event,
            };
            if cancellation.is_cancelled() {
                return Err(AttemptFailure::Cancelled);
            }
            let Some(event) = event else {
                return Err(AttemptFailure::Restartable(self.diagnostic(
                    None,
                    None,
                    "provider stream ended without a terminal response",
                )));
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => return Err(self.stream_failure(error)),
            };
            let terminal_accounting = match &event {
                WireEvent::Completed { response, .. }
                | WireEvent::Failed { response, .. }
                | WireEvent::Incomplete { response, .. } => {
                    Some(self.response_accounting(response, selection))
                }
                _ => None,
            };
            let event_result = (|| -> Result<Option<WireResponse>, ModelError> {
                match event {
                    WireEvent::Completed { response, .. } => Ok(Some(response)),
                    WireEvent::Failed { response, .. } => Err(ModelError::Provider(Box::new(
                        self.response_failure("response failed", &response),
                    ))),
                    WireEvent::Incomplete { response, .. } => Err(ModelError::Provider(Box::new(
                        self.response_failure("response incomplete", &response),
                    ))),
                    WireEvent::Error { code, message, .. } => Err(ModelError::Provider(Box::new(
                        self.diagnostic(None, code, message),
                    ))),
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
                    WireEvent::FunctionArgumentsDelta { .. }
                    | WireEvent::FunctionArgumentsDone { .. } => Err(ModelError::InvalidResponse(
                        "summary stream contained a function call",
                    )),
                    WireEvent::OutputItemAdded {
                        output_index, item, ..
                    } => {
                        content.account_output_item(output_index, &item)?;
                        reject_summary_stream_function_call(&item)?;
                        Ok(None)
                    }
                    WireEvent::OutputItemDone {
                        output_index, item, ..
                    } => {
                        content.account_output_item(output_index, &item)?;
                        reject_summary_stream_function_call(&item)?;
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
                    WireEvent::RefusalDelta | WireEvent::RefusalDone => Err(ModelError::Provider(
                        Box::new(self.diagnostic(None, None, "model refused the request")),
                    )),
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
                    let accounting = terminal_accounting.expect("terminal response accounting");
                    return self
                        .completed_summary(response, &mut content)
                        .map(|text| SummaryResult {
                            text,
                            attempts: vec![accounting.clone()],
                        })
                        .map_err(|error| AttemptFailure::Terminal {
                            error,
                            accounting: Box::new(accounting),
                        });
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(AttemptFailure::Terminal {
                        error,
                        accounting: Box::new(
                            terminal_accounting.unwrap_or(AttemptAccounting::UsageUnavailable),
                        ),
                    });
                }
            }
        }
    }

    fn completed_summary(
        &self,
        response: WireResponse,
        content: &mut ApplicationContentGuard,
    ) -> Result<String, ModelError> {
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
        let text = validate_summary_output(&response.output)?;
        content.account_output(&response.output)?;
        Ok(text)
    }
}

impl Model for OpenAiModel {
    fn validate_history(
        &self,
        history: &ModelHistory,
        selection: ModelSelection,
    ) -> Result<(), ModelError> {
        self.request(selection, history).map(|_| ())
    }

    fn summarise<'a>(
        &'a mut self,
        request: SummaryRequest,
        cancellation: &'a CancellationToken,
        attempt_started: &'a mut (dyn FnMut() + Send),
    ) -> impl Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a {
        self.summary_with_restarts(request, cancellation, attempt_started)
    }

    fn step<'a>(
        &'a mut self,
        selection: ModelSelection,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a {
        self.step_with_restarts(selection, history, cancellation, progress)
    }
}

enum AttemptFailure {
    Cancelled,
    Restartable(ProviderFailure),
    Terminal {
        error: ModelError,
        accounting: Box<AttemptAccounting>,
    },
}

fn openai_wire_model(model: &ModelConfiguration) -> Result<&'static str, ModelError> {
    (model.provider() == Provider::OpenAi)
        .then(|| model.wire_id())
        .ok_or(ModelError::InvalidSelection("unsupported OpenAI model"))
}

fn redact(value: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        value.to_owned()
    } else {
        value.replace(api_key, "[REDACTED]")
    }
}

fn foreign_input(part: ForeignPart<'_>) -> Vec<Value> {
    match part {
        ForeignPart::Text(text) => vec![json!({"role": "assistant", "content": text})],
        ForeignPart::ToolCall {
            call_id,
            source,
            output,
        } => vec![
            json!({
                "type": "function_call",
                "call_id": call_id,
                "name": "execute_python",
                "arguments": json!({"code": source}).to_string()
            }),
            json!({"type": "function_call_output", "call_id": call_id, "output": output}),
        ],
    }
}

fn replay_values(replay: &ProviderReplay) -> Result<Vec<Value>, ModelError> {
    replay
        .items()
        .iter()
        .map(|item| {
            serde_json::from_slice(item.as_bytes())
                .map_err(|_| ModelError::InvalidHistory("opaque OpenAI replay is malformed"))
        })
        .collect()
}

fn readable_reasoning_summary(output: &[Value]) -> String {
    let mut summary = String::new();
    for item in output {
        if item.get("type").and_then(Value::as_str) != Some("reasoning") {
            continue;
        }
        if let Some(parts) = item.get("summary").and_then(Value::as_array) {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    summary.push_str(text);
                }
            }
        }
    }
    summary
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
        commentary: Option<String>,
    },
    Message {
        text: String,
    },
}

struct ValidatedMessage {
    text: String,
    commentary: bool,
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
                let validated = validate_message(object)?;
                if code.is_some() && validated.commentary {
                    return Err(ModelError::InvalidResponse(
                        "commentary message followed a function call",
                    ));
                }
                message = Some(validated);
            }
            _ => {
                return Err(ModelError::InvalidResponse(
                    "response contained an unexpected output item",
                ));
            }
        }
    }

    match (code, message) {
        (
            Some(ValidatedAction::Code {
                source,
                call_id,
                commentary: None,
            }),
            Some(ValidatedMessage {
                text,
                commentary: true,
            }),
        ) => Ok(ValidatedAction::Code {
            source,
            call_id,
            commentary: Some(text),
        }),
        (Some(action), None) => Ok(action),
        (
            None,
            Some(ValidatedMessage {
                commentary: true, ..
            }),
        ) => Err(ModelError::InvalidResponse(
            "commentary message did not include a function call",
        )),
        (None, Some(ValidatedMessage { text, .. })) => Ok(ValidatedAction::Message { text }),
        (Some(_), Some(_)) => Err(ModelError::InvalidResponse(
            "response mixed a function call with final assistant text",
        )),
        (None, None) => Err(ModelError::InvalidResponse(
            "response did not contain an action",
        )),
    }
}

fn reject_summary_stream_function_call(item: &Value) -> Result<(), ModelError> {
    if item.get("type").and_then(Value::as_str) == Some("function_call") {
        return Err(ModelError::InvalidResponse(
            "summary stream contained a function call",
        ));
    }
    Ok(())
}

fn validate_summary_output(output: &[Value]) -> Result<String, ModelError> {
    let mut text = None;
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
            "message" => {
                if text.is_some() {
                    return Err(ModelError::InvalidResponse(
                        "response contained multiple assistant messages",
                    ));
                }
                text = Some(validate_summary_message(object)?);
            }
            "function_call" => {
                return Err(ModelError::InvalidResponse(
                    "summary response contained a function call",
                ));
            }
            _ => {
                return Err(ModelError::InvalidResponse(
                    "summary response contained an unexpected output item",
                ));
            }
        }
    }

    text.ok_or(ModelError::InvalidResponse(
        "summary response did not contain an assistant message",
    ))
}

fn validate_summary_message(object: &serde_json::Map<String, Value>) -> Result<String, ModelError> {
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
    let [part] = content.as_slice() else {
        return Err(ModelError::InvalidResponse(
            "summary message did not contain exactly one text part",
        ));
    };
    let part = part.as_object().ok_or(ModelError::InvalidResponse(
        "assistant message content part was malformed",
    ))?;
    if required_string(part, "type")? != "output_text" {
        return Err(ModelError::InvalidResponse(
            "summary message contained unexpected content",
        ));
    }
    let text = required_string(part, "text")?;
    if !text.chars().any(|scalar| !scalar.is_whitespace()) {
        return Err(ModelError::InvalidResponse(
            "summary message was empty or whitespace",
        ));
    }
    Ok(text.to_owned())
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
        commentary: None,
    })
}

fn validate_message(
    object: &serde_json::Map<String, Value>,
) -> Result<ValidatedMessage, ModelError> {
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
    let commentary = match object.get("phase") {
        None | Some(Value::Null) => false,
        Some(Value::String(phase)) if phase == "commentary" => true,
        Some(Value::String(phase)) if phase == "final_answer" => false,
        _ => {
            return Err(ModelError::InvalidResponse(
                "assistant message phase was malformed",
            ));
        }
    };
    Ok(ValidatedMessage { text, commentary })
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
        if bytes > ACCEPTED_MODEL_CONTENT_LIMIT {
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
        #[cfg(test)]
        sequence_number: u64,
        response: WireResponse,
    },
    #[serde(rename = "response.failed")]
    Failed {
        #[cfg(test)]
        sequence_number: u64,
        response: WireResponse,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete {
        #[cfg(test)]
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
        #[cfg(test)]
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
        #[cfg(test)]
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
        #[cfg(test)]
        sequence_number: u64,
        code: Option<String>,
        message: String,
    },
    #[serde(other)]
    Unexpected,
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    #[cfg(test)]
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_openai::Client;
    use async_openai::config::{Config, OPENAI_API_BASE, OpenAIConfig};
    use async_openai::error::OpenAIError;
    use async_openai::middleware::ReqwestService;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use tokio::net::TcpListener;
    use tokio::sync::{Notify, mpsc};
    use tokio::task::JoinHandle;
    use tokio::time::{Instant, sleep, sleep_until, timeout};
    use tokio_util::sync::CancellationToken;

    use super::{WireEvent, WireStatus, WireStream, openai_config};
    use crate::{
        config::{
            ModelSelection,
            models::{ASTRA, CATALOGUE, Provider, SOL, TERRA},
            settings::CrossProviderReasoning,
        },
        model::{AttemptAccounting, Model, ModelError, ModelHistory},
        sse_fixture::{Fixture, FixtureReply},
    };

    fn history() -> ModelHistory {
        let mut history = ModelHistory::new();
        history.append_user_request("inspect");
        history
    }

    fn default_selection() -> ModelSelection {
        ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap()
    }

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
    const TWO_DELTAS: &str = concat!(
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"sequence_number\":1,\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"first\"}\n\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"sequence_number\":2,\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"second\"}\n\n",
    );

    #[test]
    fn summary_request_has_no_tools_and_uses_captured_selection() {
        let model = super::OpenAiModel::new(
            "system prompt",
            "test-api-key",
            CrossProviderReasoning::Inline,
        );
        let request = model
            .summary_request(&crate::model::SummaryRequest {
                instructions: "summary instructions".to_owned(),
                input: "summary input".to_owned(),
                selection: ModelSelection::new(&SOL, "high").unwrap(),
                output_limit: 321,
            })
            .unwrap();

        assert_eq!(request["model"], json!("gpt-5.6-sol"));
        assert_eq!(request["instructions"], json!("summary instructions"));
        assert_eq!(request["input"], json!("summary input"));
        assert_eq!(request["max_output_tokens"], json!(321));
        assert_eq!(request["reasoning"]["effort"], json!("high"));
        assert_eq!(request["service_tier"], json!("default"));
        assert_eq!(request["stream"], json!(true));
        assert_eq!(request["store"], json!(false));
        assert!(request.get("tools").is_none());
        assert!(request.get("tool_choice").is_none());
        assert!(request.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn constructs_qualified_sol_request_with_high_effort() {
        let model = super::OpenAiModel::new(
            "system prompt",
            "test-api-key",
            CrossProviderReasoning::Inline,
        );
        let request = model
            .request(ModelSelection::new(&SOL, "high").unwrap(), &history())
            .unwrap();

        assert_eq!(request["model"], json!("gpt-5.6-sol"));
        assert_eq!(request["reasoning"]["effort"], json!("high"));
    }

    #[test]
    fn constructs_and_serializes_each_configured_openai_model() {
        let model = super::OpenAiModel::new(
            "system prompt",
            "test-api-key",
            CrossProviderReasoning::Inline,
        );
        assert_eq!(model.prompt, "system prompt");
        assert_eq!(model.client.config().api_base(), OPENAI_API_BASE);
        assert_eq!(
            model.client.config().url("/responses"),
            format!("{OPENAI_API_BASE}/responses")
        );
        for configuration in CATALOGUE
            .into_iter()
            .filter(|model| model.provider() == Provider::OpenAi)
        {
            for &thinking in configuration.thinking_levels {
                let request = model
                    .request(
                        ModelSelection::new(configuration, thinking).unwrap(),
                        &history(),
                    )
                    .unwrap();

                assert_eq!(
                    request["model"],
                    json!(configuration.id.strip_prefix("openai/").unwrap())
                );
                assert_eq!(request["service_tier"], json!(configuration.service_tier));
                assert_eq!(request["reasoning"]["effort"], json!(thinking));
            }
        }
    }

    #[tokio::test]
    async fn rejects_anthropic_selections_before_networking() {
        let fixture = Fixture::start(FixtureReply::Sse(COMPLETED_STREAM)).await;
        let mut model =
            super::OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
        let cancellation = CancellationToken::new();

        for id in ["anthropic/claude-opus-5-5", "anthropic/claude-fable-5-1"] {
            let configuration =
                crate::config::model_by_id(id).expect("Anthropic model is in the catalogue");
            let selection =
                ModelSelection::new(configuration, configuration.default_thinking).unwrap();
            let mut progress_events = 0;
            let mut attempts_started = 0;

            let step = model
                .step(selection, &history(), &cancellation, &mut |_| {
                    progress_events += 1
                })
                .await
                .unwrap_err();
            let summary = model
                .summarise(
                    crate::model::SummaryRequest {
                        instructions: "summary instructions".to_owned(),
                        input: "summary input".to_owned(),
                        selection,
                        output_limit: 321,
                    },
                    &cancellation,
                    &mut || attempts_started += 1,
                )
                .await
                .unwrap_err();

            assert!(
                matches!(step.error(), ModelError::InvalidSelection(_)),
                "{id}"
            );
            assert!(step.attempts().is_empty(), "{id}");
            assert!(
                matches!(summary.error(), ModelError::InvalidSelection(_)),
                "{id}"
            );
            assert!(summary.attempts().is_empty(), "{id}");
            assert_eq!((progress_events, attempts_started), (0, 0), "{id}");
            assert!(matches!(
                model.validate_history(&history(), selection),
                Err(ModelError::InvalidSelection(_))
            ));
        }
        assert_eq!(fixture.request_count(), 0);
    }

    #[test]
    fn constructs_astra_request_with_verified_wire_contract() {
        let model = super::OpenAiModel::new(
            "system prompt",
            "test-api-key",
            CrossProviderReasoning::Inline,
        );
        let request = model
            .request(
                ModelSelection::new(&ASTRA, ASTRA.default_thinking).unwrap(),
                &history(),
            )
            .unwrap();

        assert_eq!(request["model"], json!("gpt-6-astra"));
        assert_eq!(request["service_tier"], json!("default"));
        assert_eq!(request["store"], json!(false));
        assert_eq!(request["stream"], json!(true));
        assert_eq!(request["reasoning"]["effort"], json!("low"));
        assert_eq!(request["reasoning"]["summary"], json!("auto"));
        assert_eq!(request["reasoning"]["context"], json!("all_turns"));
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(ModelSelection::new(&ASTRA, "none"), None);
    }

    #[test]
    fn rejects_unsupported_thinking_before_networking() {
        assert_eq!(ModelSelection::new(&SOL, "minimal"), None);
    }

    #[tokio::test]
    async fn retries_preserve_selected_thinking_and_qualified_usage_identity() {
        let fixture = Fixture::start_sequence(vec![
            FixtureReply::Sse(""),
            FixtureReply::Sse(COMPLETED_STREAM),
        ])
        .await;
        let mut model =
            super::OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};

        let step = model
            .step(
                ModelSelection::new(&SOL, "high").unwrap(),
                &history(),
                &cancellation,
                &mut progress,
            )
            .await
            .unwrap();

        assert_eq!(fixture.request_count(), 2);
        for request in fixture.requests.lock().unwrap().iter() {
            assert_eq!(request.body["model"], json!("gpt-5.6-sol"));
            assert_eq!(request.body["reasoning"]["effort"], json!("high"));
        }
        let [
            AttemptAccounting::UsageUnavailable,
            AttemptAccounting::KnownUsageWithUntrackedRetries(usage),
        ] = step.attempts.as_slice()
        else {
            panic!("expected one restarted attempt followed by known usage");
        };
        assert_eq!(usage.model, SOL.id);
        assert_eq!(
            crate::pricing::estimate_usage(usage)
                .unwrap()
                .pricing()
                .model,
            SOL.id
        );
    }

    #[tokio::test]
    async fn each_step_uses_the_selection_it_is_given() {
        let fixture = Fixture::start_sequence(vec![
            FixtureReply::Sse(COMPLETED_STREAM),
            FixtureReply::Sse(COMPLETED_STREAM),
        ])
        .await;
        let mut model =
            super::OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};

        let first = model
            .step(
                ModelSelection::new(&SOL, "low").unwrap(),
                &history(),
                &cancellation,
                &mut progress,
            )
            .await
            .unwrap();
        let second = model
            .step(
                ModelSelection::new(&TERRA, "high").unwrap(),
                &history(),
                &cancellation,
                &mut progress,
            )
            .await
            .unwrap();

        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].body["model"], json!("gpt-5.6-sol"));
        assert_eq!(requests[0].body["reasoning"]["effort"], json!("low"));
        assert_eq!(requests[1].body["model"], json!("gpt-5.6-terra"));
        assert_eq!(requests[1].body["reasoning"]["effort"], json!("high"));
        drop(requests);
        let AttemptAccounting::KnownUsageWithUntrackedRetries(first_usage) =
            first.attempts.first().unwrap()
        else {
            panic!("first step should report known usage")
        };
        let AttemptAccounting::KnownUsageWithUntrackedRetries(second_usage) =
            second.attempts.first().unwrap()
        else {
            panic!("second step should report known usage")
        };
        assert_eq!(first_usage.model, SOL.id);
        assert_eq!(second_usage.model, TERRA.id);
        assert_eq!(
            crate::pricing::estimate_usage(second_usage)
                .unwrap()
                .pricing(),
            TERRA.pricing.identity
        );
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

    mod retry_policy {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use tokio::time::timeout;
        use tokio_util::sync::CancellationToken;

        use super::super::OpenAiModel;
        use super::*;
        use crate::config::constants::ACCEPTED_MODEL_CONTENT_LIMIT;
        use crate::model::{
            AttemptAccounting, Model, ModelError, ModelFailure, ModelProgress, ProviderReplay,
        };
        use crate::pricing::{RunCostEstimate, estimate_run_cost};

        async fn step(
            fixture: &Fixture,
        ) -> (
            Result<crate::model::ModelStep, ModelFailure>,
            Vec<ModelProgress>,
        ) {
            step_with(fixture, default_selection()).await
        }

        async fn step_with(
            fixture: &Fixture,
            selection: ModelSelection,
        ) -> (
            Result<crate::model::ModelStep, ModelFailure>,
            Vec<ModelProgress>,
        ) {
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = Vec::new();
            let result = {
                let mut sink = |event| progress.push(event);
                model
                    .step(selection, &history(), &cancellation, &mut sink)
                    .await
            };
            (result, progress)
        }

        fn starts(progress: &[ModelProgress]) -> Vec<u32> {
            progress
                .iter()
                .filter_map(|event| match event {
                    ModelProgress::AttemptStarted { attempt } => Some(*attempt),
                    _ => None,
                })
                .collect()
        }

        fn settles(progress: &[ModelProgress]) -> Vec<u32> {
            progress
                .iter()
                .filter_map(|event| match event {
                    ModelProgress::AttemptSettled { attempt, .. } => Some(*attempt),
                    _ => None,
                })
                .collect()
        }

        fn complete_message(text: &str) -> String {
            format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({
                    "type": "response.completed",
                    "sequence_number": 1,
                    "response": {
                        "id": "resp_complete",
                        "status": "completed",
                        "service_tier": "default",
                        "output": [{
                            "type": "message",
                            "id": "msg_complete",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                            "status": "completed"
                        }],
                        "usage": {
                            "input_tokens": 1,
                            "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                            "output_tokens": 1,
                            "output_tokens_details": {"reasoning_tokens": 0},
                            "total_tokens": 2
                        }
                    }
                })
            )
        }

        fn error_body(message: &str, code: &str) -> String {
            json!({
                "error": {
                    "message": message,
                    "type": "fixture_error",
                    "param": null,
                    "code": code
                }
            })
            .to_string()
        }

        fn function_stream(arguments: &str, complete: bool) -> String {
            let mut stream = format!(
                "data: {}\n\ndata: {}\n\n",
                json!({
                    "type": "response.output_item.added",
                    "sequence_number": 1,
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "id": "fc_1",
                        "call_id": "call_1",
                        "name": "execute_python",
                        "arguments": "",
                        "status": "in_progress"
                    }
                }),
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 2,
                    "output_index": 0,
                    "item_id": "fc_1",
                    "delta": arguments
                })
            );
            if complete {
                stream.push_str(&format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({
                        "type": "response.completed",
                        "sequence_number": 3,
                        "response": {
                            "id": "resp_complete",
                            "status": "completed",
                            "service_tier": "default",
                            "output": [{
                                "type": "function_call",
                                "id": "fc_1",
                                "call_id": "call_1",
                                "name": "execute_python",
                                "arguments": arguments,
                                "status": "completed"
                            }],
                            "usage": {
                                "input_tokens": 1,
                                "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0},
                                "output_tokens": 1,
                                "output_tokens_details": {"reasoning_tokens": 0},
                                "total_tokens": 2
                            }
                        }
                    })
                ));
            }
            stream
        }

        #[tokio::test]
        async fn configured_sdk_retry_layer_handles_429_and_5xx_within_two_retries() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::Status {
                    code: 429,
                    body: r#"{"error":{"message":"retry later","type":"rate_limit_error","param":null,"code":"rate_limit"}}"#,
                },
                FixtureReply::Status {
                    code: 500,
                    body: r#"{"error":{"message":"unavailable","type":"server_error","param":null,"code":"server_error"}}"#,
                },
                FixtureReply::Sse(COMPLETED_STREAM),
            ])
            .await;

            let (result, progress) =
                step_with(&fixture, ModelSelection::new(&TERRA, "high").unwrap()).await;

            assert!(result.is_ok());
            assert_eq!(fixture.request_count(), 3);
            for request in fixture.requests.lock().unwrap().iter() {
                assert_eq!(request.body["model"], json!("gpt-5.6-terra"));
                assert_eq!(request.body["reasoning"]["effort"], json!("high"));
            }
            assert_eq!(starts(&progress), vec![1]);
            assert_eq!(settles(&progress), vec![1]);
        }

        #[tokio::test]
        async fn exhausted_sdk_retries_do_not_trigger_a_kiki_stream_restart() {
            let fixture = Fixture::start(FixtureReply::Status {
                code: 429,
                body: r#"{"error":{"message":"retry later","type":"rate_limit_error","param":null,"code":"rate_limit"}}"#,
            })
            .await;

            let (result, progress) = step(&fixture).await;
            let failure = result.unwrap_err();

            assert_eq!(fixture.request_count(), 3);
            assert_eq!(starts(&progress), vec![1]);
            assert_eq!(settles(&progress), vec![1]);
            assert_eq!(failure.provider_failure().unwrap().status(), Some(429));
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        }

        #[tokio::test]
        async fn opening_transport_disconnect_restarts_the_model_step() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::CloseBeforeHeaders,
                FixtureReply::Sse(COMPLETED_STREAM),
            ])
            .await;

            let (result, progress) = step(&fixture).await;
            let step = result.expect("opening transport disconnect should restart");

            assert_eq!(fixture.request_count(), 2);
            assert_eq!(starts(&progress), vec![1, 2]);
            assert_eq!(settles(&progress), vec![1, 2]);
            assert!(matches!(
                step.attempts.as_slice(),
                [
                    AttemptAccounting::UsageUnavailable,
                    AttemptAccounting::KnownUsageWithUntrackedRetries(_)
                ]
            ));
        }

        #[tokio::test]
        async fn opening_transport_disconnects_stop_after_three_attempts() {
            let fixture = Fixture::start(FixtureReply::CloseBeforeHeaders).await;

            let (result, progress) = step(&fixture).await;
            let failure = result.unwrap_err();

            assert_eq!(fixture.request_count(), 3);
            assert_eq!(starts(&progress), vec![1, 2, 3]);
            assert_eq!(settles(&progress), vec![1, 2, 3]);
            assert_eq!(
                failure.to_string(),
                "model provider failed: provider failure: provider transport failed"
            );
            assert_eq!(
                failure.attempts(),
                &[
                    AttemptAccounting::UsageUnavailable,
                    AttemptAccounting::UsageUnavailable,
                    AttemptAccounting::UsageUnavailable,
                ]
            );
        }

        #[tokio::test]
        async fn exhausted_sdk_connection_retries_do_not_restart_the_model_step() {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            drop(listener);
            let mut model = OpenAiModel::with_endpoint("system prompt", "fixture-key", &endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = Vec::new();
            let failure = model
                .step(
                    default_selection(),
                    &history(),
                    &cancellation,
                    &mut |event| progress.push(event),
                )
                .await
                .unwrap_err();

            assert_eq!(starts(&progress), vec![1]);
            assert_eq!(settles(&progress), vec![1]);
            assert_eq!(
                failure.to_string(),
                "model provider failed: provider failure: provider transport failed"
            );
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        }

        #[tokio::test]
        async fn restartable_stream_failures_reset_presentation_before_the_next_attempt() {
            for first_reply in [
                FixtureReply::AbruptSse(ONE_DELTA),
                FixtureReply::OwnedSse(ONE_DELTA.to_owned()),
            ] {
                let fixture =
                    Fixture::start_sequence(vec![first_reply, FixtureReply::Sse(COMPLETED_STREAM)])
                        .await;
                let (result, progress) = step(&fixture).await;
                let step = result.unwrap();

                assert_eq!(fixture.request_count(), 2);
                assert_eq!(starts(&progress), vec![1, 2]);
                assert_eq!(settles(&progress), vec![1, 2]);
                assert_eq!(
                    progress.iter().position(|event| matches!(
                        event,
                        ModelProgress::AttemptSettled { attempt: 1, .. }
                    )),
                    Some(2)
                );
                assert_eq!(
                    progress.iter().position(|event| matches!(
                        event,
                        ModelProgress::AttemptStarted { attempt: 2 }
                    )),
                    Some(3)
                );
                assert!(matches!(
                    step.attempts.as_slice(),
                    [
                        AttemptAccounting::UsageUnavailable,
                        AttemptAccounting::KnownUsageWithUntrackedRetries(_)
                    ]
                ));
                assert!(matches!(
                    estimate_run_cost(&step.attempts),
                    RunCostEstimate::LowerBound(_)
                ));
                assert_eq!(step.replay.items().len(), 2);
            }
        }

        #[tokio::test]
        async fn restarts_after_pre_event_eof() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::Sse(""),
                FixtureReply::OwnedSse(complete_message("fresh")),
            ])
            .await;
            let (result, progress) = step(&fixture).await;

            assert!(result.is_ok());
            assert_eq!(fixture.request_count(), 2);
            assert_eq!(starts(&progress), vec![1, 2]);
            assert_eq!(settles(&progress), vec![1, 2]);
        }

        #[tokio::test]
        async fn restart_uses_a_fresh_python_decoder() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::OwnedSse(function_stream(r#"{"code":"partial"#, false)),
                FixtureReply::OwnedSse(function_stream(r#"{"code":"fresh"}"#, true)),
            ])
            .await;
            let (result, progress) = step(&fixture).await;
            let step = result.unwrap();

            assert!(matches!(
                step.action,
                crate::model::ModelAction::Code { source, .. } if source == "fresh"
            ));
            assert_eq!(
                progress
                    .iter()
                    .filter_map(|event| match event {
                        ModelProgress::DecodedPythonChunk { code } => Some(code.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                ["partial", "fresh"]
            );
            assert!(
                step.replay
                    .items()
                    .iter()
                    .all(|item| !String::from_utf8_lossy(item.as_bytes()).contains("partial"))
            );
        }

        #[tokio::test]
        async fn restart_after_post_event_eof_has_a_fresh_content_budget() {
            let first = format!(
                "data: {}\n\n",
                json!({
                    "type": "response.output_text.done",
                    "sequence_number": 1,
                    "output_index": 0,
                    "content_index": 0,
                    "text": "x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT)
                })
            );
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::OwnedSse(first),
                FixtureReply::OwnedSse(complete_message("fresh")),
            ])
            .await;

            let (result, progress) = step(&fixture).await;
            let step = result.unwrap();

            assert_eq!(fixture.request_count(), 2);
            assert_eq!(starts(&progress), vec![1, 2]);
            assert_eq!(settles(&progress), vec![1, 2]);
            assert!(
                matches!(step.action, crate::model::ModelAction::Message { text } if text == "fresh")
            );
        }

        #[tokio::test]
        async fn failure_flushes_pending_presentation_before_settling_and_restarting() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::OwnedSse(TWO_DELTAS.to_owned()),
                FixtureReply::Sse(COMPLETED_STREAM),
            ])
            .await;
            let (_, progress) = step(&fixture).await;

            assert_eq!(
                &progress[..5],
                &[
                    ModelProgress::AttemptStarted { attempt: 1 },
                    ModelProgress::ReasoningSummaryChunk {
                        text: "first".to_owned(),
                    },
                    ModelProgress::ReasoningSummaryChunk {
                        text: "second".to_owned(),
                    },
                    ModelProgress::AttemptSettled {
                        attempt: 1,
                        accounting: AttemptAccounting::UsageUnavailable,
                    },
                    ModelProgress::AttemptStarted { attempt: 2 },
                ]
            );
        }

        #[tokio::test]
        async fn restart_attempts_stop_after_three_interrupted_streams() {
            let fixture = Fixture::start(FixtureReply::Sse("")).await;
            let (result, progress) = step(&fixture).await;
            let error = result.unwrap_err();

            assert_eq!(fixture.request_count(), 3);
            assert_eq!(starts(&progress), vec![1, 2, 3]);
            assert_eq!(settles(&progress), vec![1, 2, 3]);
            assert_eq!(
                error.attempts(),
                [
                    AttemptAccounting::UsageUnavailable,
                    AttemptAccounting::UsageUnavailable,
                    AttemptAccounting::UsageUnavailable,
                ]
            );
            let provider = error.provider_failure().unwrap();
            assert_eq!(provider.status(), None);
            assert_eq!(
                provider.message(),
                "provider stream ended without a terminal response"
            );
        }

        #[tokio::test]
        async fn cancellation_stops_sdk_backoff_without_starting_another_request() {
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code: 429,
                headers: vec![("retry-after".to_owned(), "1".to_owned())],
                body: error_body("retry later", "rate_limit"),
            })
            .await;
            let endpoint = fixture.endpoint.clone();
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            let task = tokio::spawn(async move {
                let mut model =
                    OpenAiModel::with_endpoint("system prompt", "fixture-key", &endpoint);
                let mut progress = |_| {};
                model
                    .step(
                        default_selection(),
                        &history(),
                        &cancellation,
                        &mut progress,
                    )
                    .await
            });

            fixture.wait_for_replies_written(1).await;
            assert_eq!(fixture.request_count(), 1);
            cancel.cancel();

            let error = timeout(Duration::from_millis(100), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.is_cancelled());
            assert_eq!(fixture.request_count(), 1);
        }

        #[tokio::test]
        async fn cancellation_stops_the_kiki_restart_delay_before_the_next_attempt() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::Sse(""),
                FixtureReply::Sse(COMPLETED_STREAM),
            ])
            .await;
            let endpoint = fixture.endpoint.clone();
            let progress = Arc::new(Mutex::new(Vec::new()));
            let captured = progress.clone();
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            let task = tokio::spawn(async move {
                let mut model =
                    OpenAiModel::with_endpoint("system prompt", "fixture-key", &endpoint);
                let mut sink = |event| captured.lock().unwrap().push(event);
                model
                    .step(default_selection(), &history(), &cancellation, &mut sink)
                    .await
            });

            timeout(Duration::from_secs(1), async {
                while !progress
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| matches!(event, ModelProgress::AttemptSettled { attempt: 1, .. }))
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            cancel.cancel();

            let error = timeout(Duration::from_millis(100), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(error.is_cancelled());
            assert_eq!(fixture.request_count(), 1);
            assert_eq!(starts(&progress.lock().unwrap()), vec![1]);
        }

        #[tokio::test]
        async fn semantic_and_local_failures_never_restart_the_model_step() {
            for status in [400, 401, 403] {
                let fixture = Fixture::start(FixtureReply::OwnedStatus {
                    code: status,
                    headers: Vec::new(),
                    body: error_body("terminal", "terminal"),
                })
                .await;
                let error = step(&fixture).await.0.unwrap_err();
                let provider = error.provider_failure().unwrap();

                assert_eq!(provider.status(), Some(status));
                assert_eq!(provider.message(), "terminal");
                assert_eq!(error.is_authentication(), status == 401);
                assert_eq!(fixture.request_count(), 1, "HTTP {status}");
            }

            for (stream, code, message) in [
                (
                    "data: {\"type\":\"response.refusal.delta\"}\n\ndata: [DONE]\n\n".to_owned(),
                    None,
                    "model refused the request",
                ),
                (
                    "data: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":{\"id\":\"resp\",\"status\":\"incomplete\",\"output\":[],\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\ndata: [DONE]\n\n".to_owned(),
                    Some("content_filter"),
                    "response incomplete: content_filter",
                ),
                (
                    "data: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":{\"id\":\"resp\",\"status\":\"incomplete\",\"output\":[],\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\ndata: [DONE]\n\n".to_owned(),
                    Some("max_output_tokens"),
                    "response incomplete: max_output_tokens",
                ),
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream)).await;
                let error = step(&fixture).await.0.unwrap_err();
                let provider = error.provider_failure().unwrap();

                assert_eq!(provider.code(), code);
                assert_eq!(provider.message(), message);
                assert_eq!(fixture.request_count(), 1);
            }

            let invalid = Fixture::start(FixtureReply::OwnedSse(complete_message(""))).await;
            let error = step(&invalid).await.0.unwrap_err();
            assert_eq!(
                error.error(),
                &ModelError::InvalidResponse("assistant message was empty")
            );
            assert_eq!(invalid.request_count(), 1);

            let mut invalid_history = ModelHistory::new();
            invalid_history.append_final_message(
                "answer".to_owned(),
                String::new(),
                ProviderReplay::new("openai", Vec::new()),
            );
            let fixture = Fixture::start(FixtureReply::Sse(COMPLETED_STREAM)).await;
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = |_| {};
            assert!(matches!(
                model
                    .step(default_selection(), &invalid_history, &cancellation, &mut progress)
                    .await,
                Err(failure) if matches!(failure.error(), ModelError::InvalidHistory(_))
            ));
            assert_eq!(fixture.request_count(), 0);
        }
    }

    mod safe_failures {
        use tokio_util::sync::CancellationToken;

        use super::super::OpenAiModel;
        use super::*;
        use crate::model::{Model, ModelFailure};

        fn error_body(message: &str, code: &str) -> String {
            json!({
                "error": {
                    "message": message,
                    "type": "fixture_error",
                    "param": "ignored",
                    "code": code
                }
            })
            .to_string()
        }

        async fn failure(fixture: &Fixture, key: &str) -> ModelFailure {
            let mut model = OpenAiModel::with_endpoint("system prompt", key, &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = |_| {};
            model
                .step(
                    default_selection(),
                    &history(),
                    &cancellation,
                    &mut progress,
                )
                .await
                .unwrap_err()
        }

        #[tokio::test]
        async fn provider_fields_are_capped_once_on_utf8_boundaries() {
            let message = format!("{}tail", "é".repeat(2_049));
            let code = format!("{}tail", "é".repeat(129));
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code: 400,
                headers: Vec::new(),
                body: error_body(&message, &code),
            })
            .await;

            let error = failure(&fixture, "fixture-key").await;
            let failure = error.provider_failure().unwrap();

            assert_eq!(
                failure.message(),
                "é".repeat(crate::config::constants::PROVIDER_MESSAGE_LIMIT / 2)
            );
            assert_eq!(
                failure.message().len(),
                crate::config::constants::PROVIDER_MESSAGE_LIMIT
            );
            assert_eq!(
                failure.code(),
                Some(
                    "é".repeat(crate::config::constants::PROVIDER_CODE_LIMIT / 2)
                        .as_str()
                )
            );
            assert_eq!(
                failure.code().unwrap().len(),
                crate::config::constants::PROVIDER_CODE_LIMIT
            );
        }

        #[tokio::test]
        async fn provider_failures_redact_secrets_without_retaining_response_metadata() {
            let key = "sk-secret-credential";
            let message = format!("before:{key}:{}:after:{key}", "x".repeat(5_000));
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code: 401,
                headers: vec![("x-request-id".to_owned(), "request-id-secret".to_owned())],
                body: error_body(&message, &format!("code-{key}")),
            })
            .await;

            let error = failure(&fixture, key).await;
            let failure = error.provider_failure().unwrap();
            let diagnostics = format!("{failure:?}\n{failure}\n{error:?}\n{error}");

            assert!(failure.is_authentication());
            assert!(error.is_authentication());
            assert_eq!(failure.status(), Some(401));
            assert!(!diagnostics.contains(key));
            assert!(!diagnostics.contains("request-id-secret"));
            assert!(failure.message().len() <= 4 * 1024);
            assert!(failure.code().unwrap().len() <= 256);
        }
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

        use tokio::time::timeout;
        use tokio_util::sync::CancellationToken;

        use super::super::OpenAiModel;
        use super::*;
        use crate::config::constants::ACCEPTED_MODEL_CONTENT_LIMIT;
        use crate::model::{
            AttemptAccounting, Model, ModelAction, ModelCallId, ModelError, ModelFailure,
            ModelHistory, ModelProgress, ModelUsage,
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
            AttemptAccounting::KnownUsageWithUntrackedRetries(ModelUsage {
                model: "openai/gpt-5.6-sol".to_owned(),
                service_tier: Some("default".to_owned()),
                input_tokens: Some(23),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(7),
                cache_write_1h_input_tokens: 0,
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

        async fn step(
            fixture: &Fixture,
            history: &ModelHistory,
        ) -> (
            Result<crate::model::ModelStep, ModelFailure>,
            Vec<ModelProgress>,
        ) {
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut events = Vec::new();
            let result = {
                let mut progress = |event| events.push(event);
                model
                    .step(default_selection(), history, &cancellation, &mut progress)
                    .await
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
                    commentary: None,
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

        #[tokio::test]
        async fn terminal_reasoning_summary_is_authoritative_over_streamed_progress() {
            for (streamed, expected_progress) in [
                (
                    vec![json!({
                        "type": "response.reasoning_summary_text.delta",
                        "sequence_number": 1,
                        "item_id": "rs_1",
                        "output_index": 0,
                        "summary_index": 0,
                        "delta": "streamed"
                    })],
                    "streamed",
                ),
                (Vec::new(), ""),
            ] {
                let mut events = streamed;
                events.push(completed_event(
                    2,
                    vec![
                        reasoning("rs_1", "terminal"),
                        function_call(r#"{"code":"print(1)"}"#),
                    ],
                ));
                let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream(events))).await;

                let (result, progress) = step(&fixture, &history()).await;

                assert_eq!(result.unwrap().reasoning_summary, "terminal");
                assert_eq!(
                    progress
                        .iter()
                        .filter_map(|event| match event {
                            ModelProgress::ReasoningSummaryChunk { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    expected_progress
                );
            }
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
                    commentary: None,
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
                    commentary: None,
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
                        commentary: None,
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

            assert!(matches!(
                result,
                Err(failure)
                    if failure.error()
                        == &ModelError::InvalidResponse(
                            "execute_python arguments were not a closed code object"
                        )
            ));
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
            let oversized = "x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT);
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

            assert!(matches!(
                result,
                Err(failure) if failure.error() == &ModelError::ContentTooLarge
            ));
            assert_eq!(
                &progress[progress.len() - 2..],
                &[
                    ModelProgress::AssistantTextChunk {
                        text: "pending".to_owned(),
                    },
                    ModelProgress::AttemptSettled {
                        attempt: 1,
                        accounting: AttemptAccounting::UsageUnavailable,
                    },
                ]
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
                model
                    .step(default_selection(), &step_history, &cancellation, &mut sink)
                    .await
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
            assert!(task.await.unwrap().unwrap_err().is_cancelled());
            fixture.wait_for_disconnect().await;
        }
    }

    mod summarisation {
        use super::super::OpenAiModel;
        use super::*;
        use crate::{
            config::constants::ACCEPTED_MODEL_CONTENT_LIMIT,
            model::{Model, ModelError, SummaryRequest},
        };

        fn request() -> SummaryRequest {
            SummaryRequest {
                instructions: "summary instructions".to_owned(),
                input: "summary input".to_owned(),
                selection: ModelSelection::new(&SOL, "high").unwrap(),
                output_limit: 321,
            }
        }

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
            AttemptAccounting::KnownUsageWithUntrackedRetries(crate::model::ModelUsage {
                model: SOL.id.to_owned(),
                service_tier: Some("default".to_owned()),
                input_tokens: Some(23),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(7),
                cache_write_1h_input_tokens: 0,
                output_tokens: Some(11),
                reasoning_output_tokens: Some(3),
                total_tokens: Some(34),
            })
        }

        fn message(text: &str) -> Value {
            json!({
                "type": "message",
                "id": "msg_summary",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
                "status": "completed"
            })
        }

        fn oversized_message() -> Value {
            let mut message = message("");
            message["content"][0]["text"] =
                Value::String("x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT + 1));
            message
        }

        fn completed_event(output: Vec<Value>, usage: Option<Value>) -> Value {
            let mut response = json!({
                "id": "resp_summary",
                "status": "completed",
                "service_tier": "default",
                "output": [],
            });
            response["output"] = Value::Array(output);
            if let Some(usage) = usage {
                response["usage"] = usage;
            }
            let mut event = json!({
                "type": "response.completed",
                "sequence_number": 2,
                "response": {},
            });
            event["response"] = response;
            event
        }

        fn stream(events: impl IntoIterator<Item = Value>) -> String {
            let mut stream = String::new();
            for event in events {
                stream.push_str("data: ");
                stream.push_str(&event.to_string());
                stream.push_str("\n\n");
            }
            stream.push_str("data: [DONE]\n\n");
            stream
        }

        async fn summarise(
            fixture: &Fixture,
            request: SummaryRequest,
            cancellation: &CancellationToken,
        ) -> Result<crate::model::SummaryResult, crate::model::ModelFailure> {
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let mut attempt_started = || {};
            model
                .summarise(request, cancellation, &mut attempt_started)
                .await
        }

        #[tokio::test]
        async fn summary_text_streaming_returns_only_terminal_text_and_usage() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream([
                json!({
                    "type": "response.reasoning_summary_text.delta",
                    "sequence_number": 1,
                    "output_index": 1,
                    "summary_index": 0,
                    "delta": "untrusted streamed reasoning"
                }),
                json!({
                    "type": "response.output_text.delta",
                    "sequence_number": 2,
                    "output_index": 0,
                    "content_index": 0,
                    "delta": "untrusted streamed text"
                }),
                completed_event(vec![message("terminal summary\n")], Some(usage())),
            ])))
            .await;
            let cancellation = CancellationToken::new();

            let result = summarise(&fixture, request(), &cancellation).await.unwrap();

            assert_eq!(result.text, "terminal summary\n");
            assert_eq!(result.attempts, [expected_accounting()]);
            assert_eq!(fixture.request_count(), 1);
            let request = fixture.request().body;
            assert_eq!(request["model"], json!("gpt-5.6-sol"));
            assert_eq!(request["reasoning"]["effort"], json!("high"));
            assert_eq!(request["instructions"], json!("summary instructions"));
            assert_eq!(request["input"], json!("summary input"));
            assert_eq!(request["max_output_tokens"], json!(321));
            assert_eq!(request["stream"], json!(true));
            assert_eq!(request["store"], json!(false));
            assert_eq!(request["service_tier"], json!("default"));
            assert!(request.get("tools").is_none());
            assert!(request.get("tool_choice").is_none());
            assert!(request.get("parallel_tool_calls").is_none());
        }

        #[tokio::test]
        async fn summary_rejects_oversized_streamed_and_terminal_content() {
            {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream([
                    json!({
                        "type": "response.output_text.delta",
                        "sequence_number": 1,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": "x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT)
                    }),
                    json!({
                        "type": "response.reasoning_summary_text.delta",
                        "sequence_number": 2,
                        "output_index": 1,
                        "summary_index": 0,
                        "delta": "x"
                    }),
                ])))
                .await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();

                assert_eq!(failure.error(), &ModelError::ContentTooLarge);
                assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
                assert_eq!(fixture.request_count(), 1);
            }

            {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream([completed_event(
                    vec![oversized_message()],
                    Some(usage()),
                )])))
                .await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();

                assert_eq!(failure.error(), &ModelError::ContentTooLarge);
                assert_eq!(failure.attempts(), &[expected_accounting()]);
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn summary_rejects_function_call_stream_evidence_before_terminal_message() {
            let stream_evidence = [
                json!({
                    "type": "response.function_call_arguments.delta",
                    "sequence_number": 1,
                    "output_index": 0,
                    "item_id": "fc_summary",
                    "delta": r#"{"code":"print(1)"}"#
                }),
                json!({
                    "type": "response.function_call_arguments.done",
                    "sequence_number": 1,
                    "output_index": 0,
                    "item_id": "fc_summary",
                    "arguments": r#"{"code":"print(1)"}"#
                }),
                json!({
                    "type": "response.output_item.added",
                    "sequence_number": 1,
                    "output_index": 0,
                    "item": function_call()
                }),
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": 1,
                    "output_index": 0,
                    "item": function_call()
                }),
            ];

            for evidence in stream_evidence {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream([
                    evidence,
                    completed_event(vec![message("terminal summary")], Some(usage())),
                ])))
                .await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();

                assert_eq!(
                    failure.error(),
                    &ModelError::InvalidResponse("summary stream contained a function call")
                );
                assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn summary_rejects_malformed_stream_items_and_content() {
            let malformed_events = [
                json!({
                    "type": "response.output_item.done",
                    "sequence_number": 1,
                    "output_index": 0,
                    "item": {
                        "type": "message",
                        "id": "msg_summary",
                        "role": "assistant",
                        "content": "not content",
                        "status": "completed"
                    }
                }),
                json!({
                    "type": "response.content_part.added",
                    "sequence_number": 1,
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "input_text", "text": "not output"}
                }),
            ];

            for event in malformed_events {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream([
                    event,
                    completed_event(vec![message("terminal summary")], Some(usage())),
                ])))
                .await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();

                assert!(matches!(failure.error(), ModelError::InvalidResponse(_)));
                assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
                assert_eq!(fixture.request_count(), 1);
            }
        }

        fn function_call() -> Value {
            json!({
                "type": "function_call",
                "id": "fc_summary",
                "call_id": "call_summary",
                "name": "execute_python",
                "arguments": r#"{"code":"print(1)"}"#,
                "status": "completed"
            })
        }

        fn reasoning() -> Value {
            json!({
                "type": "reasoning",
                "id": "rs_summary",
                "summary": [{"type": "summary_text", "text": "thinking"}],
                "encrypted_content": "encrypted",
                "status": "completed"
            })
        }

        #[tokio::test]
        async fn summary_retries_interrupted_streams_and_preserves_attempts() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::Sse(""),
                FixtureReply::OwnedSse(stream([completed_event(
                    vec![message("recovered summary")],
                    Some(usage()),
                )])),
            ])
            .await;
            let cancellation = CancellationToken::new();

            let result = summarise(&fixture, request(), &cancellation).await.unwrap();

            assert_eq!(result.text, "recovered summary");
            assert_eq!(fixture.request_count(), 2);
            assert_eq!(
                result.attempts,
                [AttemptAccounting::UsageUnavailable, expected_accounting()]
            );
        }

        #[tokio::test]
        async fn summary_restarts_after_opening_transport_disconnect() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::CloseBeforeHeaders,
                FixtureReply::OwnedSse(stream([completed_event(
                    vec![message("recovered summary")],
                    Some(usage()),
                )])),
            ])
            .await;
            let cancellation = CancellationToken::new();

            let result = summarise(&fixture, request(), &cancellation)
                .await
                .expect("opening summary request should restart");

            assert_eq!(fixture.request_count(), 2);
            assert_eq!(
                result.attempts,
                [AttemptAccounting::UsageUnavailable, expected_accounting()]
            );
        }

        #[tokio::test]
        async fn summary_opening_disconnects_stop_after_three_attempts() {
            let fixture = Fixture::start(FixtureReply::CloseBeforeHeaders).await;
            let cancellation = CancellationToken::new();
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let mut started = 0;

            let failure = model
                .summarise(request(), &cancellation, &mut || started += 1)
                .await
                .unwrap_err();

            assert_eq!(started, 3);
            assert_eq!(fixture.request_count(), 3);
            assert_eq!(
                failure.to_string(),
                "model provider failed: provider failure: provider transport failed"
            );
            assert_eq!(
                failure.attempts(),
                vec![AttemptAccounting::UsageUnavailable; 3]
            );
        }

        #[tokio::test]
        async fn summary_cancellation_after_opening_disconnect_prevents_retry() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::CloseBeforeHeaders,
                FixtureReply::OwnedSse(stream([completed_event(
                    vec![message("late summary")],
                    Some(usage()),
                )])),
            ])
            .await;
            let endpoint = fixture.endpoint.clone();
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            let started = Arc::new(AtomicUsize::new(0));
            let observed = started.clone();
            let retry_wait_started = Arc::new(Notify::new());
            let retry_wait_signal = retry_wait_started.clone();
            let task = tokio::spawn(async move {
                let mut model =
                    OpenAiModel::with_endpoint("system prompt", "fixture-key", &endpoint);
                model.summary_retry_wait_started = Some(retry_wait_signal);
                model
                    .summarise(request(), &cancellation, &mut || {
                        observed.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            });

            timeout(Duration::from_secs(1), retry_wait_started.notified())
                .await
                .expect("summary retry should enter its backoff");
            assert_eq!(started.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.request_count(), 1);
            cancel.cancel();

            let failure = timeout(Duration::from_millis(60), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(failure.is_cancelled());
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
            assert_eq!(fixture.request_count(), 1);
        }

        #[tokio::test]
        async fn summary_attempt_start_callback_matches_real_attempts() {
            let fixture = Fixture::start_sequence(vec![
                FixtureReply::Sse(""),
                FixtureReply::OwnedSse(stream([completed_event(
                    vec![message("recovered summary")],
                    Some(usage()),
                )])),
            ])
            .await;
            let cancellation = CancellationToken::new();
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let mut started = 0;

            let result = model
                .summarise(request(), &cancellation, &mut || started += 1)
                .await
                .expect("retry should complete");

            assert_eq!(started, 2);
            assert_eq!(fixture.request_count(), 2);
            assert_eq!(result.attempts.len(), 2);
        }

        #[tokio::test]
        async fn summary_accepts_reasoning_metadata_and_usage_absence() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream([completed_event(
                vec![reasoning(), message("summary")],
                None,
            )])))
            .await;
            let cancellation = CancellationToken::new();

            let result = summarise(&fixture, request(), &cancellation).await.unwrap();

            assert_eq!(result.text, "summary");
            assert_eq!(result.attempts, [AttemptAccounting::UsageUnavailable]);
        }

        #[tokio::test]
        async fn summary_cancellation_before_an_attempt_makes_no_request() {
            let fixture = Fixture::start(FixtureReply::Sse("")).await;
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let mut started = 0;

            let failure = model
                .summarise(request(), &cancellation, &mut || started += 1)
                .await
                .unwrap_err();

            assert_eq!(failure.error(), &ModelError::Cancelled);
            assert!(failure.attempts().is_empty());
            assert_eq!(started, 0);
            assert_eq!(fixture.request_count(), 0);
        }

        #[tokio::test]
        async fn summary_cancellation_during_an_attempt_settles_the_attempt() {
            let fixture = Fixture::start(FixtureReply::IdleSse(ONE_DELTA)).await;
            let endpoint = fixture.endpoint.clone();
            let cancellation = CancellationToken::new();
            let cancel = cancellation.clone();
            let task = tokio::spawn(async move {
                let mut model =
                    OpenAiModel::with_endpoint("system prompt", "fixture-key", &endpoint);
                let mut attempt_started = || {};
                model
                    .summarise(request(), &cancellation, &mut attempt_started)
                    .await
            });

            fixture.wait_for_requests(1).await;
            cancel.cancel();

            let failure = task.await.unwrap().unwrap_err();
            assert_eq!(failure.error(), &ModelError::Cancelled);
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
            fixture.wait_for_disconnect().await;
        }

        #[tokio::test]
        async fn summary_authentication_and_provider_failures_are_redacted() {
            for (status, body, expected) in [
                (
                    401,
                    r#"{"error":{"message":"fixture-key denied","type":"authentication_error","param":null,"code":"invalid_api_key"}}"#,
                    "[REDACTED] denied",
                ),
                (
                    400,
                    r#"{"error":{"message":"request rejected","type":"invalid_request_error","param":null,"code":"bad_request"}}"#,
                    "request rejected",
                ),
            ] {
                let fixture = Fixture::start(FixtureReply::Status { code: status, body }).await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();
                let provider = failure.provider_failure().unwrap();

                assert_eq!(provider.status(), Some(status));
                assert_eq!(provider.message(), expected);
                assert_eq!(failure.is_authentication(), status == 401);
                assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn summary_rejects_empty_or_unicode_whitespace_text() {
            for text in ["", "\u{2003}\u{202f}"] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream([completed_event(
                    vec![message(text)],
                    Some(usage()),
                )])))
                .await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();

                assert_eq!(
                    failure.error(),
                    &ModelError::InvalidResponse("summary message was empty or whitespace")
                );
                assert_eq!(failure.attempts(), &[expected_accounting()]);
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn summary_rejects_function_calls_and_malformed_messages() {
            let multiple_text_parts = json!({
                "type": "message",
                "id": "msg_multiple",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "first"},
                    {"type": "output_text", "text": "second"}
                ],
                "status": "completed"
            });
            let malformed_content = json!({
                "type": "message",
                "id": "msg_malformed",
                "role": "assistant",
                "content": [{"type": "input_text", "text": "wrong"}],
                "status": "completed"
            });
            let multiple_messages = vec![message("first"), message("second")];

            for output in [
                vec![function_call()],
                vec![multiple_text_parts],
                vec![malformed_content],
                multiple_messages,
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream([completed_event(
                    output,
                    Some(usage()),
                )])))
                .await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();

                assert!(matches!(failure.error(), ModelError::InvalidResponse(_)));
                assert_eq!(failure.attempts(), &[expected_accounting()]);
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn summary_rejects_refused_failed_incomplete_and_output_limited_responses() {
            let failed = json!({
                "type": "response.failed",
                "sequence_number": 1,
                "response": {
                    "id": "resp_failed",
                    "status": "failed",
                    "output": [],
                    "service_tier": "default",
                    "usage": usage(),
                    "error": {"code": "server_error", "message": "failed"}
                }
            });
            let incomplete = |reason: &str| {
                json!({
                    "type": "response.incomplete",
                    "sequence_number": 1,
                    "response": {
                        "id": "resp_incomplete",
                        "status": "incomplete",
                        "output": [],
                        "service_tier": "default",
                        "usage": usage(),
                        "incomplete_details": {"reason": reason}
                    }
                })
            };
            let cases = vec![
                (
                    stream([json!({"type": "response.refusal.delta"})]),
                    None,
                    AttemptAccounting::UsageUnavailable,
                ),
                (
                    stream([failed]),
                    Some("server_error"),
                    expected_accounting(),
                ),
                (
                    stream([incomplete("content_filter")]),
                    Some("content_filter"),
                    expected_accounting(),
                ),
                (
                    stream([incomplete("max_output_tokens")]),
                    Some("max_output_tokens"),
                    expected_accounting(),
                ),
            ];

            for (body, code, accounting) in cases {
                let fixture = Fixture::start(FixtureReply::OwnedSse(body)).await;
                let cancellation = CancellationToken::new();

                let failure = summarise(&fixture, request(), &cancellation)
                    .await
                    .unwrap_err();
                let provider = failure.provider_failure().unwrap();

                assert_eq!(provider.code(), code);
                assert_eq!(failure.attempts(), &[accounting]);
                assert_eq!(fixture.request_count(), 1);
            }
        }
    }

    mod action_protocol {
        use tokio_util::sync::CancellationToken;

        use super::super::{ApplicationContentGuard, OpenAiModel};
        use super::*;
        use crate::config::constants::ACCEPTED_MODEL_CONTENT_LIMIT;
        use crate::model::{
            AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelContextEntry,
            ModelError, ModelFailure, ModelHistory, ModelProgress, ModelUsage, OpaqueReplayItem,
            ProviderReplay,
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

        fn expected_accounting() -> AttemptAccounting {
            AttemptAccounting::KnownUsageWithUntrackedRetries(ModelUsage {
                model: "openai/gpt-5.6-sol".to_owned(),
                service_tier: Some("default".to_owned()),
                input_tokens: Some(23),
                cached_input_tokens: Some(5),
                cache_write_input_tokens: Some(7),
                cache_write_1h_input_tokens: 0,
                output_tokens: Some(11),
                reasoning_output_tokens: Some(3),
                total_tokens: Some(34),
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
            let mut history = ModelHistory::new();
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

        async fn step_with_progress(
            fixture: &Fixture,
            history: &ModelHistory,
        ) -> (
            Result<crate::model::ModelStep, ModelFailure>,
            Vec<ModelProgress>,
        ) {
            let mut model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut events = Vec::new();
            let result = {
                let mut progress = |event| events.push(event);
                model
                    .step(default_selection(), history, &cancellation, &mut progress)
                    .await
            };
            (result, events)
        }

        async fn step(
            fixture: &Fixture,
            history: &ModelHistory,
        ) -> Result<crate::model::ModelStep, ModelFailure> {
            step_with_progress(fixture, history).await.0
        }

        async fn assert_rejected(output: Vec<Value>) {
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                output,
                Some(usage()),
            )))
            .await;

            let (result, progress) = step_with_progress(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();
            assert!(
                matches!(failure.error(), ModelError::InvalidResponse(_)),
                "unexpected rejection: {failure:?}"
            );
            assert_eq!(failure.attempts(), &[expected_accounting()]);
            assert_eq!(
                progress.last(),
                Some(&ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: expected_accounting(),
                })
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
                    call_id: ModelCallId::new("call_1"),
                    commentary: None,
                }
            );
            assert_eq!(completed.reasoning_summary, "Checking");
            assert_eq!(completed.replay.provider(), "openai");
            assert_eq!(completed.replay.items().len(), 2);
            for (opaque, expected) in completed.replay.items().iter().zip(&output) {
                assert_eq!(opaque.as_bytes(), serde_json::to_vec(expected).unwrap());
            }
            assert_eq!(completed.attempts, vec![expected_accounting()]);

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
                    "system prompt",
                    json!([{"role": "user", "content": "inspect this project"}])
                )
            );
        }

        #[tokio::test]
        async fn accepts_commentary_with_a_completed_code_action() {
            let mut commentary = message("msg_1", "I’ll inspect the configuration first.");
            commentary["phase"] = json!("commentary");
            let output = vec![
                commentary,
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#),
            ];
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                output.clone(),
                Some(usage()),
            )))
            .await;

            let completed = step(&fixture, &history("inspect")).await.unwrap();

            assert_eq!(
                completed.action,
                ModelAction::Code {
                    source: "print(1)".to_owned(),
                    call_id: ModelCallId::new("call_1"),
                    commentary: Some("I’ll inspect the configuration first.".to_owned()),
                }
            );
            assert_eq!(completed.replay.items().len(), 2);
            for (opaque, expected) in completed.replay.items().iter().zip(output) {
                assert_eq!(opaque.as_bytes(), serde_json::to_vec(&expected).unwrap());
            }
        }

        #[tokio::test]
        async fn sends_the_generated_tool_reference_with_only_execute_python() {
            let output = vec![function_call(
                "fc_1",
                "call_1",
                "execute_python",
                r#"{"code":"print(1)"}"#,
            )];
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                output,
                Some(usage()),
            )))
            .await;
            let reference = crate::tools::prompt_reference();
            let prompt = format!("arbitrary base prompt\n\n{reference}");
            let mut model = OpenAiModel::with_endpoint(&prompt, "fixture-key", &fixture.endpoint);
            let cancellation = CancellationToken::new();
            let mut progress = |_| {};

            model
                .step(
                    default_selection(),
                    &history("inspect"),
                    &cancellation,
                    &mut progress,
                )
                .await
                .unwrap();

            let request = fixture.request().body;
            let instructions = request["instructions"]
                .as_str()
                .expect("instructions are a string");
            assert_eq!(instructions, prompt);
            for signature in [
                "async def read(path: str) -> str:",
                "async def write(path: str, content: str) -> None:",
                "async def edit(path: str, old: str, new: str) -> None:",
                "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
                "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
                "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:",
                "async def run(command: str, cwd: str = \".\") -> CommandResult:",
            ] {
                assert_eq!(instructions.matches(signature).count(), 1);
            }
            assert_eq!(instructions.matches("class CommandResult:").count(), 1);
            assert!(instructions.contains("asyncio.gather"));
            assert!(instructions.contains("Run project-local, one-shot commands."));
            assert_eq!(
                request["tools"],
                json!([{
                    "type": "function",
                    "name": "execute_python",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {"code": {"type": "string"}},
                        "required": ["code"],
                        "additionalProperties": false,
                    },
                }])
            );
        }

        #[tokio::test]
        async fn returns_reasoning_summary_parts_in_terminal_provider_order() {
            let output = vec![
                json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [
                        {"type": "summary_text", "text": "first "},
                        {"type": "summary_text", "text": "second "}
                    ],
                    "encrypted_content": "encrypted-first",
                    "status": "completed"
                }),
                json!({
                    "type": "reasoning",
                    "id": "rs_2",
                    "summary": [
                        {"type": "summary_text", "text": "third"}
                    ],
                    "encrypted_content": "encrypted-second",
                    "status": "completed"
                }),
                function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#),
            ];
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                output.clone(),
                Some(usage()),
            )))
            .await;

            let completed = step(&fixture, &history("inspect")).await.unwrap();

            assert_eq!(completed.reasoning_summary, "first second third");
            for (opaque, expected) in completed.replay.items().iter().zip(output) {
                assert_eq!(opaque.as_bytes(), serde_json::to_vec(&expected).unwrap());
            }
        }

        #[tokio::test]
        async fn returns_an_empty_summary_without_reasoning_or_with_an_empty_summary() {
            for output in [
                vec![function_call(
                    "fc_1",
                    "call_1",
                    "execute_python",
                    r#"{"code":"print(1)"}"#,
                )],
                vec![
                    json!({
                        "type": "reasoning",
                        "id": "rs_1",
                        "summary": [],
                        "encrypted_content": "encrypted",
                        "status": "completed"
                    }),
                    function_call("fc_1", "call_1", "execute_python", r#"{"code":"print(1)"}"#),
                ],
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                    "completed",
                    output,
                    Some(usage()),
                )))
                .await;

                assert_eq!(
                    step(&fixture, &history("inspect"))
                        .await
                        .unwrap()
                        .reasoning_summary,
                    ""
                );
            }
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
                    commentary: None,
                }
            );
        }

        #[tokio::test]
        async fn replays_exact_subsequent_history_and_returns_one_completed_message() {
            let mut commentary = message("msg_commentary", "I’ll calculate the value first.");
            commentary["phase"] = json!("commentary");
            let code_output = vec![
                reasoning("rs_1", "First", "encrypted-first"),
                commentary,
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
                    Some("I’ll calculate the value first.".to_owned()),
                    "First".to_owned(),
                    replay(&code_output),
                    CodeResult::Execution {
                        feedback: "2".to_owned(),
                    },
                )
                .unwrap();
            history.append_final_message(
                "first answer".to_owned(),
                "Second".to_owned(),
                replay(&message_output),
            );
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
            assert_eq!(completed.reasoning_summary, "Third");
            assert_eq!(
                completed.attempts,
                vec![AttemptAccounting::UsageUnavailable]
            );
            assert_eq!(
                fixture.request().body,
                expected_request(
                    "system prompt",
                    json!([
                        {"role": "user", "content": "first request"},
                        code_output[0].clone(),
                        code_output[1].clone(),
                        code_output[2].clone(),
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
            let mut lone_commentary = message("msg_commentary", "I’ll inspect first.");
            lone_commentary["phase"] = json!("commentary");
            let mut late_commentary = message("msg_late", "I’ll inspect first.");
            late_commentary["phase"] = json!("commentary");
            let mut final_answer = message("msg_final", "answer");
            final_answer["phase"] = json!("final_answer");
            let cases = vec![
                vec![message("msg_1", "answer"), valid_call.clone()],
                vec![lone_commentary],
                vec![valid_call.clone(), late_commentary],
                vec![final_answer, valid_call.clone()],
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
                        "id": "rs_missing_summary",
                        "encrypted_content": "encrypted",
                        "status": "completed"
                    }),
                    message("msg_1", "answer"),
                ],
                vec![
                    json!({
                        "type": "reasoning",
                        "id": "rs_malformed_summary",
                        "summary": [{"type": "summary_text"}],
                        "encrypted_content": "encrypted",
                        "status": "completed"
                    }),
                    message("msg_1", "answer"),
                ],
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
        async fn failed_and_incomplete_terminal_responses_retain_returned_usage() {
            for (event_type, status, details, code, message) in [
                (
                    "response.failed",
                    "failed",
                    json!({"error": {"code": "server_error", "message": "failed"}}),
                    "server_error",
                    "response failed: failed",
                ),
                (
                    "response.incomplete",
                    "incomplete",
                    json!({"incomplete_details": {"reason": "max_output_tokens"}}),
                    "max_output_tokens",
                    "response incomplete: max_output_tokens",
                ),
            ] {
                let mut response = json!({
                    "id": "resp_terminal_failure",
                    "status": status,
                    "service_tier": "default",
                    "output": [],
                    "usage": usage(),
                });
                response
                    .as_object_mut()
                    .unwrap()
                    .extend(details.as_object().unwrap().clone());
                let fixture = Fixture::start(FixtureReply::OwnedSse(event_stream([json!({
                    "type": event_type,
                    "sequence_number": 1,
                    "response": response,
                })])))
                .await;

                let (result, progress) = step_with_progress(&fixture, &history("inspect")).await;
                let failure = result.unwrap_err();
                let provider = failure.provider_failure().unwrap();

                assert_eq!(provider.code(), Some(code));
                assert_eq!(provider.message(), message);
                assert_eq!(failure.attempts(), &[expected_accounting()]);
                assert_eq!(
                    progress.last(),
                    Some(&ModelProgress::AttemptSettled {
                        attempt: 1,
                        accounting: expected_accounting(),
                    })
                );
                assert_eq!(fixture.request_count(), 1);
            }
        }

        #[tokio::test]
        async fn rejects_semantic_terminal_outcomes_without_restarting() {
            for (stream, code, message) in [
                (
                    FAILED_STREAM.to_owned(),
                    "server_error",
                    "response failed: failed",
                ),
                (
                    INCOMPLETE_STREAM.to_owned(),
                    "max_output_tokens",
                    "response incomplete: max_output_tokens",
                ),
                (ERROR_STREAM.to_owned(), "stream_error", "stream failed"),
            ] {
                let fixture = Fixture::start(FixtureReply::OwnedSse(stream)).await;
                let error = step(&fixture, &history("inspect")).await.unwrap_err();
                let provider = error.provider_failure().unwrap();

                assert_eq!(provider.status(), None);
                assert_eq!(provider.code(), Some(code));
                assert_eq!(provider.message(), message);
                assert_eq!(error.attempts(), &[AttemptAccounting::UsageUnavailable]);
                assert_eq!(fixture.request_count(), 1);
            }

            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "incomplete",
                vec![message("msg_1", "answer")],
                Some(usage()),
            )))
            .await;
            let error = step(&fixture, &history("inspect")).await.unwrap_err();

            assert_eq!(
                error.error(),
                &ModelError::InvalidResponse("response.completed did not contain completed status")
            );
            assert_eq!(error.attempts(), &[expected_accounting()]);
            assert_eq!(fixture.request_count(), 1);
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
                    None,
                    String::new(),
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

            let error = step(&fixture, &history).await.unwrap_err();
            assert_eq!(
                error.error(),
                &ModelError::InvalidResponse("function call identifier duplicates model history")
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
            let mut pending =
                Box::pin(model.step(default_selection(), &history, &cancellation, &mut progress));

            assert!(
                timeout(Duration::from_millis(20), &mut pending)
                    .await
                    .is_err()
            );
            assert_eq!(history, original_history);

            cancellation.cancel();
            assert!(pending.await.unwrap_err().is_cancelled());
            fixture.wait_for_disconnect().await;
            assert_eq!(fixture.request_count(), 1);
            assert!(
                history
                    .entries()
                    .iter()
                    .all(|entry| matches!(entry, ModelContextEntry::UserRequest { .. }))
            );
        }

        #[tokio::test]
        async fn application_content_guard_counts_all_guarded_fields_as_events_arrive() {
            let quarter = ACCEPTED_MODEL_CONTENT_LIMIT / 4;
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

            let error = step(&fixture, &history("inspect")).await.unwrap_err();
            assert_eq!(error.error(), &ModelError::ContentTooLarge);
            assert_eq!(error.attempts(), &[AttemptAccounting::UsageUnavailable]);
            assert_eq!(fixture.request_count(), 1);
        }

        #[test]
        fn application_content_guard_combines_terminal_reasoning_by_output_index() {
            for ids in [["rs_1", "rs_2"], ["duplicate", "duplicate"]] {
                let mut first = reasoning(ids[0], "", "");
                first["encrypted_content"] =
                    Value::String("x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT / 2 + 1));
                let mut second = reasoning(ids[1], "", "");
                second["encrypted_content"] =
                    Value::String("x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT / 2 + 1));
                let mut content = ApplicationContentGuard::default();

                assert_eq!(
                    content.account_output(&[first, second]),
                    Err(ModelError::ContentTooLarge)
                );
            }
        }

        #[tokio::test]
        async fn rejects_invalid_stored_replays_without_making_a_request() {
            let mut malformed_json = history("first request");
            malformed_json.append_final_message(
                "answer".to_owned(),
                String::new(),
                ProviderReplay::new("openai", vec![OpaqueReplayItem::new(b"not json".to_vec())]),
            );
            malformed_json.append_user_request("continue");

            let mut semantically_malformed = history("first request");
            let mut non_assistant_message = message("msg_1", "answer");
            non_assistant_message["role"] = json!("user");
            semantically_malformed.append_final_message(
                "answer".to_owned(),
                String::new(),
                replay(&[non_assistant_message]),
            );
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
                    None,
                    String::new(),
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
                    None,
                    String::new(),
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
                String::new(),
                replay(&[message("msg_1", "answer")]),
            );
            text_mismatch.append_user_request("continue");

            for (case, history, expected) in [
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

                let failure = step(&fixture, &history).await.unwrap_err();

                assert_eq!(fixture.request_count(), 0, "{case}");
                assert_eq!(failure.error(), &expected, "{case}");
            }
        }

        fn anthropic_replay(blocks: &[Value]) -> ProviderReplay {
            ProviderReplay::new(
                "anthropic",
                blocks
                    .iter()
                    .map(|block| OpaqueReplayItem::new(serde_json::to_vec(block).unwrap()))
                    .collect(),
            )
        }

        fn cross_provider_history(code_reasoning: &str) -> ModelHistory {
            let mut history = history("first request");
            history.append_final_message(
                "own answer".to_owned(),
                String::new(),
                replay(&[message("msg_own", "own answer")]),
            );
            history.append_user_request("inspect");
            history
                .append_completed_code(
                    "print(1)".to_owned(),
                    ModelCallId::new("toolu_1"),
                    Some("I'll run it.".to_owned()),
                    code_reasoning.to_owned(),
                    anthropic_replay(&[
                        json!({"type": "thinking", "thinking": code_reasoning, "signature": "anthropic-signature"}),
                        json!({"type": "text", "text": "I'll run it."}),
                        json!({"type": "tool_use", "id": "toolu_1", "name": "execute_python", "input": {"code": "print(1)"}}),
                    ]),
                    CodeResult::execution("1\n".to_owned()),
                )
                .unwrap();
            history.append_final_message(
                "It printed 1.".to_owned(),
                "Summarising".to_owned(),
                anthropic_replay(&[
                    json!({"type": "redacted_thinking", "data": "anthropic-redacted-data"}),
                    json!({"type": "text", "text": "It printed 1."}),
                ]),
            );
            history.append_user_request("continue");
            history
        }

        #[tokio::test]
        async fn anthropic_entries_replay_as_assistant_text_and_function_calls_with_their_results()
        {
            for (reasoning, code_reasoning, reasoning_texts) in [
                (
                    CrossProviderReasoning::Inline,
                    "Checking the file",
                    [Some("Checking the file"), Some("Summarising")],
                ),
                (
                    CrossProviderReasoning::Inline,
                    "",
                    [None, Some("Summarising")],
                ),
                (
                    CrossProviderReasoning::Drop,
                    "Checking the file",
                    [None, None],
                ),
            ] {
                let fixture = Fixture::start_sequence(vec![
                    FixtureReply::OwnedSse(terminal_stream(
                        "completed",
                        vec![message("msg_next", "next")],
                        Some(usage()),
                    ));
                    2
                ])
                .await;
                let mut parent = OpenAiModel::build(
                    "system prompt".to_owned(),
                    "fixture-key".to_owned(),
                    &fixture.endpoint,
                    reasoning,
                );
                let mut child = parent.with_prompt("child prompt".to_owned());
                let history = cross_provider_history(code_reasoning);
                let original = history.clone();
                let cancellation = CancellationToken::new();

                for model in [&mut parent, &mut child] {
                    model
                        .step(default_selection(), &history, &cancellation, &mut |_| {})
                        .await
                        .unwrap();
                }

                let assistant_text = |text: &str| json!({"role": "assistant", "content": text});
                let mut input = vec![
                    json!({"role": "user", "content": "first request"}),
                    message("msg_own", "own answer"),
                    json!({"role": "user", "content": "inspect"}),
                ];
                input.extend(reasoning_texts[0].map(assistant_text));
                input.extend([
                    assistant_text("I'll run it."),
                    json!({
                        "type": "function_call",
                        "call_id": "toolu_1",
                        "name": "execute_python",
                        "arguments": r#"{"code":"print(1)"}"#
                    }),
                    json!({"type": "function_call_output", "call_id": "toolu_1", "output": "1\n"}),
                ]);
                input.extend(reasoning_texts[1].map(assistant_text));
                input.extend([
                    assistant_text("It printed 1."),
                    json!({"role": "user", "content": "continue"}),
                ]);
                let requests = fixture.requests.lock().unwrap().clone();
                assert_eq!(
                    requests
                        .iter()
                        .map(|request| request.body.clone())
                        .collect::<Vec<_>>(),
                    [
                        expected_request("system prompt", json!(input)),
                        expected_request("child prompt", json!(input)),
                    ],
                    "{reasoning:?} {code_reasoning:?}"
                );
                for request in &requests {
                    let body = request.body.to_string();
                    for opaque in ["anthropic-signature", "anthropic-redacted-data", "thinking"] {
                        assert!(!body.contains(opaque), "{opaque} leaked into {body}");
                    }
                }
                assert_eq!(history, original);
            }
        }

        #[tokio::test]
        async fn validate_history_accepts_valid_replay_and_rejects_invalid_replay_without_http() {
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                vec![message("msg_1", "answer")],
                Some(usage()),
            )))
            .await;
            let model =
                OpenAiModel::with_endpoint("system prompt", "fixture-key", &fixture.endpoint);
            let valid = history("first request");
            let mut foreign = history("first request");
            foreign.append_final_message(
                "answer".to_owned(),
                String::new(),
                ProviderReplay::new("other-provider", Vec::new()),
            );
            let mut mismatched = history("first request");
            mismatched.append_final_message(
                "different answer".to_owned(),
                String::new(),
                replay(&[message("msg_1", "answer")]),
            );

            assert_eq!(model.validate_history(&valid, default_selection()), Ok(()));
            assert_eq!(
                model.validate_history(&foreign, default_selection()),
                Ok(())
            );
            assert_eq!(
                model.validate_history(&mismatched, default_selection()),
                Err(ModelError::InvalidHistory(
                    "final message does not match its OpenAI replay"
                ))
            );
            assert_eq!(fixture.request_count(), 0);
        }

        #[tokio::test]
        async fn replays_a_compaction_summary_once_before_retained_history() {
            let retained_output = vec![message("msg_retained", "retained reply")];
            let mut history = ModelHistory::new();
            history.append_compaction_summary("stored summary");
            history.append_user_request("retained request");
            history.append_final_message(
                "retained reply".to_owned(),
                String::new(),
                replay(&retained_output),
            );
            history.append_user_request("continue");
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                vec![message("msg_next", "next reply")],
                Some(usage()),
            )))
            .await;

            step(&fixture, &history).await.unwrap();

            let canonical_summary = "The conversation history before this point was compacted into the following summary.\n\nThe Python Arena and its existing variables, functions, classes, and data persisted unchanged across compaction. Treat Arena details marked unverified as historical evidence and inspect them directly before relying on them.\n\n<summary>\nstored summary\n</summary>";
            assert_eq!(
                fixture.request().body,
                expected_request(
                    "system prompt",
                    json!([
                        {"role": "user", "content": canonical_summary},
                        {"role": "user", "content": "retained request"},
                        retained_output[0].clone(),
                        {"role": "user", "content": "continue"}
                    ])
                )
            );
        }

        #[tokio::test]
        async fn compaction_summaries_do_not_bypass_retained_replay_validation() {
            let mut history = ModelHistory::new();
            history.append_compaction_summary("stored summary");
            history.append_final_message(
                "different reply".to_owned(),
                String::new(),
                replay(&[message("msg_retained", "retained reply")]),
            );
            let fixture = Fixture::start(FixtureReply::OwnedSse(terminal_stream(
                "completed",
                vec![message("msg_next", "next reply")],
                Some(usage()),
            )))
            .await;

            let failure = step(&fixture, &history).await.unwrap_err();

            assert_eq!(fixture.request_count(), 0);
            assert_eq!(
                failure.error(),
                &ModelError::InvalidHistory("final message does not match its OpenAI replay")
            );
        }
    }

    const LIVE_OUTER_TIMEOUT: Duration = Duration::from_secs(300);
    const LIVE_PROVIDER_TIMEOUT: Duration = Duration::from_secs(120);
    const LIVE_CANCELLATION_TIMEOUT: Duration = Duration::from_secs(30);
    const LIVE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
    const LIVE_SETUP_CONFIGURATION: &str =
        "live test requires OPENAI_API_KEY and an absolute MONTY_BIN for monty-runtime 0.0.22";
    const LIVE_SETUP_READ_TOOL: &str =
        "live test could not initialise the workspace read capability";
    const LIVE_SETUP_ARENA: &str = "live test could not initialise the Monty arena";
    const LIVE_COMMAND: &str = "live test could not send a runtime command";
    const LIVE_EVENT_CHANNEL_CLOSED: &str = "live runtime event channels closed before settlement";
    const LIVE_PROVIDER_DEADLINE: &str = "live provider attempt exceeded its two-minute deadline";
    const LIVE_OUTER_DEADLINE: &str = "live scenario exceeded its five-minute deadline";
    const LIVE_STEP_LIMIT: &str = "live first run exceeded eight model steps";
    const LIVE_FIRST_SETTLEMENT: &str = "live first run did not settle correctly";
    const LIVE_SECOND_START: &str = "live cancellation run did not start an OpenAI attempt";
    const LIVE_SECOND_SETTLEMENT: &str = "live cancellation run did not settle correctly";
    const LIVE_THIRD_SETTLEMENT: &str = "live follow-up run did not settle correctly";
    const LIVE_CLEANUP_SHUTDOWN: &str = "live test could not request runtime shutdown";
    const LIVE_CLEANUP_CONTROLLER: &str = "live runtime controller stopped unexpectedly";
    const LIVE_CLEANUP_TIMEOUT_MESSAGE: &str =
        "live runtime controller did not stop within ten seconds";
    const LIVE_RECORDS: &str = "live runtime records did not match the scenario";
    const FIRST_REQUEST: &str = "Work through this exact sequence before replying. Use at least two separate execute_python steps. First successfully execute `contents = await read(\"README.md\")` and retain the returned string in the persistent arena. In a later separate step, use `contents` and print only a focused, non-empty selection. Do not combine those Python steps. After both executions complete, provide a concise nonblank final answer.";
    const CANCELLATION_REQUEST: &str = "Begin this request with execute_python and do not send a final message before starting that work.";
    const FOLLOW_UP_REQUEST: &str = "Provide a concise nonblank final response confirming that you can continue after the cancelled request.";

    struct LiveController {
        commands: mpsc::Sender<crate::runtime::RuntimeCommand>,
        events: mpsc::UnboundedReceiver<crate::runtime::RuntimeEvent>,
        progress: mpsc::Receiver<crate::runtime::ProgressEnvelope<crate::runtime::RuntimeEvent>>,
        controller: JoinHandle<Vec<crate::runtime::StoredRecord>>,
        events_open: bool,
        progress_open: bool,
    }

    struct LiveRun {
        run_id: crate::runtime::RunId,
        steps: Vec<crate::runtime::ModelStepId>,
        completed_executions: usize,
        successful_read: bool,
        final_messages: usize,
        nonblank_final_messages: usize,
        cancelled_runs: usize,
        failed_runs: usize,
        streamed_content: bool,
        settled: usize,
        attempts_started: usize,
        attempts_settled: usize,
        attempt_open: bool,
    }

    impl LiveRun {
        fn new(run_id: crate::runtime::RunId) -> Self {
            Self {
                run_id,
                steps: Vec::new(),
                completed_executions: 0,
                successful_read: false,
                final_messages: 0,
                nonblank_final_messages: 0,
                cancelled_runs: 0,
                failed_runs: 0,
                streamed_content: false,
                settled: 0,
                attempts_started: 0,
                attempts_settled: 0,
                attempt_open: false,
            }
        }

        fn add_step(&mut self, step_id: crate::runtime::ModelStepId) {
            if !self.steps.contains(&step_id) {
                self.steps.push(step_id);
            }
        }
    }

    #[derive(Default)]
    struct LiveCollector {
        runs: Vec<LiveRun>,
        provider_attempt: Option<(crate::runtime::RunId, Instant)>,
        disabled_events: usize,
    }

    impl LiveCollector {
        fn run_mut(&mut self, run_id: crate::runtime::RunId) -> &mut LiveRun {
            if let Some(index) = self.runs.iter().position(|run| run.run_id == run_id) {
                return &mut self.runs[index];
            }
            let index = self.runs.len();
            self.runs.push(LiveRun::new(run_id));
            &mut self.runs[index]
        }

        fn run(&self, run_id: crate::runtime::RunId) -> Option<&LiveRun> {
            self.runs.iter().find(|run| run.run_id == run_id)
        }

        fn observe(&mut self, event: &crate::runtime::RuntimeEvent) {
            use crate::model::ModelProgress;
            use crate::runtime::{Record, RuntimeEvent};

            match event {
                RuntimeEvent::ModelProgress {
                    run_id, step_id, ..
                }
                | RuntimeEvent::ExecutionStarted {
                    run_id, step_id, ..
                }
                | RuntimeEvent::ToolUpdated {
                    run_id, step_id, ..
                }
                | RuntimeEvent::RunPreviewUpdated {
                    run_id, step_id, ..
                }
                | RuntimeEvent::StdoutDelta {
                    run_id, step_id, ..
                }
                | RuntimeEvent::StdoutTruncated { run_id, step_id } => {
                    self.run_mut(*run_id).add_step(*step_id);
                }
                _ => {}
            }

            match event {
                RuntimeEvent::RunStarted { run_id } => {
                    self.run_mut(*run_id);
                }
                RuntimeEvent::ModelStepStarted { run_id, step_id } => {
                    self.run_mut(*run_id).add_step(*step_id);
                }
                RuntimeEvent::ModelProgress {
                    run_id, progress, ..
                } => match progress {
                    ModelProgress::AttemptStarted { .. } => {
                        let run = self.run_mut(*run_id);
                        run.attempts_started += 1;
                        run.attempt_open = true;
                        self.provider_attempt =
                            Some((*run_id, Instant::now() + LIVE_PROVIDER_TIMEOUT));
                    }
                    ModelProgress::AttemptSettled { .. } => {
                        let run = self.run_mut(*run_id);
                        run.attempts_settled += 1;
                        run.attempt_open = false;
                        if self
                            .provider_attempt
                            .is_some_and(|(current_run_id, _)| current_run_id == *run_id)
                        {
                            self.provider_attempt = None;
                        }
                    }
                    ModelProgress::ReasoningSummaryChunk { text }
                    | ModelProgress::AssistantTextChunk { text } => {
                        self.run_mut(*run_id).streamed_content |= !text.is_empty();
                    }
                    ModelProgress::DecodedPythonChunk { code } => {
                        self.run_mut(*run_id).streamed_content |= !code.is_empty();
                    }
                },
                RuntimeEvent::ToolUpdated {
                    run_id,
                    call:
                        crate::arena::ToolCall {
                            tool: crate::arena::ToolName::Read,
                            summary: crate::arena::ToolCallSummary::Count(_),
                        },
                    ..
                } => {
                    self.run_mut(*run_id).successful_read = true;
                }
                RuntimeEvent::RecordAppended { record, .. } => {
                    let run = self.run_mut(record.run_id);
                    match &record.record {
                        Record::UserRequest { .. } => {}
                        Record::CompletedExecution { step_id, .. } => {
                            run.add_step(*step_id);
                            run.completed_executions += 1;
                        }
                        Record::FinalMessage { step_id, text, .. } => {
                            run.add_step(*step_id);
                            run.final_messages += 1;
                            run.nonblank_final_messages += usize::from(!text.trim().is_empty());
                        }
                        Record::CancelledRun { .. } => run.cancelled_runs += 1,
                        Record::FailedRun { .. } => run.failed_runs += 1,
                        Record::Compaction { .. } => {}
                    }
                }
                RuntimeEvent::RunSettled { run_id } => {
                    self.run_mut(*run_id).settled += 1;
                }
                RuntimeEvent::RuntimeDisabled { .. }
                | RuntimeEvent::SubmissionRejectedDisabled { .. } => {
                    self.disabled_events += 1;
                }
                RuntimeEvent::ExecutionStarted { .. }
                | RuntimeEvent::ToolUpdated { .. }
                | RuntimeEvent::RunPreviewUpdated { .. }
                | RuntimeEvent::StdoutDelta { .. }
                | RuntimeEvent::StdoutTruncated { .. }
                | RuntimeEvent::SubagentSpawned { .. }
                | RuntimeEvent::SubagentStatus { .. }
                | RuntimeEvent::SubagentRecord { .. }
                | RuntimeEvent::SubagentFinished { .. }
                | RuntimeEvent::SubagentOperationSettled { .. }
                | RuntimeEvent::SettingsAccepted { .. }
                | RuntimeEvent::NameAccepted { .. }
                | RuntimeEvent::NavigationCompleted { .. }
                | RuntimeEvent::CorrectionCompleted { .. }
                | RuntimeEvent::TreeCatalogue { .. }
                | RuntimeEvent::SubmissionRejectedBusy { .. }
                | RuntimeEvent::CompactionAcknowledged { .. }
                | RuntimeEvent::CompactionLifecycle { .. } => {}
            }
        }

        fn drain_ready(&mut self, controller: &mut LiveController) {
            while let Ok(event) = controller.events.try_recv() {
                self.observe(&event);
            }
            while let Ok(envelope) = controller.progress.try_recv() {
                self.observe(&envelope.item);
            }
        }

        fn deadline(&self, outer_deadline: Instant) -> (Instant, &'static str) {
            match self.provider_attempt {
                Some((_, deadline)) if deadline <= outer_deadline => {
                    (deadline, LIVE_PROVIDER_DEADLINE)
                }
                _ => (outer_deadline, LIVE_OUTER_DEADLINE),
            }
        }

        async fn next(
            &mut self,
            controller: &mut LiveController,
            outer_deadline: Instant,
        ) -> Result<Option<crate::runtime::RuntimeEvent>, &'static str> {
            loop {
                if !controller.events_open && !controller.progress_open {
                    return Err(LIVE_EVENT_CHANNEL_CLOSED);
                }
                let (deadline, timeout_message) = self.deadline(outer_deadline);
                tokio::select! {
                    event = controller.events.recv(), if controller.events_open => match event {
                        Some(event) => {
                            self.observe(&event);
                            return Ok(Some(event));
                        }
                        None => controller.events_open = false,
                    },
                    envelope = controller.progress.recv(), if controller.progress_open => match envelope {
                        Some(envelope) => {
                            let event = envelope.item;
                            self.observe(&event);
                            return Ok(None);
                        }
                        None => controller.progress_open = false,
                    },
                    _ = sleep_until(deadline) => return Err(timeout_message),
                }
            }
        }
    }

    #[test]
    fn live_collector_counts_step_from_progress_once() {
        let run_id = crate::runtime::RunId(1);
        let progress = crate::runtime::RuntimeEvent::ModelProgress {
            run_id,
            step_id: crate::runtime::ModelStepId(1),
            progress: crate::model::ModelProgress::AssistantTextChunk {
                text: "progress".to_owned(),
            },
        };
        let mut collector = LiveCollector::default();

        collector.observe(&progress);
        collector.observe(&progress);

        assert_eq!(
            collector.run(run_id).unwrap().steps,
            [crate::runtime::ModelStepId(1)]
        );
    }

    struct LiveScenario {
        first_run_id: crate::runtime::RunId,
        second_run_id: crate::runtime::RunId,
        third_run_id: crate::runtime::RunId,
    }

    async fn start_live_controller() -> Result<LiveController, &'static str> {
        let configuration = crate::config::load();
        let persisted_settings = configuration.snapshot().settings().clone();
        let (worker_path, workspace_root, model, prompt_base, selection, policy) = {
            let ready = configuration.ready().ok_or(LIVE_SETUP_CONFIGURATION)?;
            (
                ready.worker_path().to_path_buf(),
                ready.workspace_root().to_path_buf(),
                crate::production_model(
                    &ready,
                    persisted_settings.anthropic_cache_ttl(),
                    persisted_settings.cross_provider_reasoning(),
                ),
                ready.prompt_base().to_owned(),
                ModelSelection::new(ready.model(), ready.thinking())
                    .expect("ready startup selection must be valid"),
                configuration.snapshot().subagents(),
            )
        };
        let tool_host = crate::tools::ToolHost::new(workspace_root)
            .await
            .map_err(|_| LIVE_SETUP_READ_TOOL)?;
        let arena = crate::arena::Arena::new(worker_path, tool_host)
            .await
            .map_err(|_| LIVE_SETUP_ARENA)?;
        let subagents = crate::subagents::SubagentCoordinator::new(
            crate::subagents::SubagentPolicy::from(policy),
            Arc::new(crate::subagents::CompleteChildRunner::new(
                prompt_base,
                model.clone(),
            )),
        );
        let runtime = crate::runtime::RuntimeController::new(
            crate::agent::Agent::new(arena, model),
            selection,
            persisted_settings,
            subagents,
        );
        let (commands, command_receiver) = mpsc::channel(8);
        let (event_sender, events) = mpsc::unbounded_channel();
        let (progress_sender, progress) = mpsc::channel(256);
        let controller = tokio::spawn(runtime.run(command_receiver, event_sender, progress_sender));
        Ok(LiveController {
            commands,
            events,
            progress,
            controller,
            events_open: true,
            progress_open: true,
        })
    }

    fn send_command(
        commands: &mpsc::Sender<crate::runtime::RuntimeCommand>,
        command: crate::runtime::RuntimeCommand,
    ) -> Result<(), &'static str> {
        commands.try_send(command).map_err(|_| LIVE_COMMAND)
    }

    fn requested_run(
        input: &Option<crate::runtime::RuntimeEvent>,
        request: &str,
    ) -> Option<crate::runtime::RunId> {
        let Some(crate::runtime::RuntimeEvent::RecordAppended { record, .. }) = input else {
            return None;
        };
        let crate::runtime::Record::UserRequest {
            request: actual_request,
            ..
        } = &record.record
        else {
            return None;
        };
        (actual_request == request).then_some(record.run_id)
    }

    fn settled_run(
        input: &Option<crate::runtime::RuntimeEvent>,
        run_id: crate::runtime::RunId,
    ) -> bool {
        matches!(
            input,
            Some(crate::runtime::RuntimeEvent::RunSettled {
                run_id: actual_run_id
            }) if *actual_run_id == run_id
        )
    }

    async fn wait_for_request(
        controller: &mut LiveController,
        collector: &mut LiveCollector,
        request: &str,
        deadline: Instant,
    ) -> Result<crate::runtime::RunId, &'static str> {
        loop {
            let input = collector.next(controller, deadline).await?;
            if let Some(run_id) = requested_run(&input, request) {
                return Ok(run_id);
            }
        }
    }

    async fn wait_for_attempt_started(
        controller: &mut LiveController,
        collector: &mut LiveCollector,
        run_id: crate::runtime::RunId,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        loop {
            if collector
                .run(run_id)
                .is_some_and(|run| run.attempts_started > 0)
            {
                return Ok(());
            }
            collector.next(controller, deadline).await?;
        }
    }

    async fn wait_for_attempt_settlement(
        controller: &mut LiveController,
        collector: &mut LiveCollector,
        run_id: crate::runtime::RunId,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        collector.drain_ready(controller);
        loop {
            let Some(run) = collector.run(run_id) else {
                return Err(LIVE_EVENT_CHANNEL_CLOSED);
            };
            if !run.attempt_open {
                return Ok(());
            }
            collector.next(controller, deadline).await?;
        }
    }

    async fn wait_for_first_settlement(
        controller: &mut LiveController,
        collector: &mut LiveCollector,
        run_id: crate::runtime::RunId,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        loop {
            let input = collector.next(controller, deadline).await?;
            if collector.run(run_id).is_some_and(|run| run.steps.len() > 8) {
                send_command(
                    &controller.commands,
                    crate::runtime::RuntimeCommand::Cancel { correction: false },
                )?;
                return Err(LIVE_STEP_LIMIT);
            }
            if settled_run(&input, run_id) {
                return wait_for_attempt_settlement(controller, collector, run_id, deadline).await;
            }
        }
    }

    async fn wait_for_settlement(
        controller: &mut LiveController,
        collector: &mut LiveCollector,
        run_id: crate::runtime::RunId,
        deadline: Instant,
    ) -> Result<(), &'static str> {
        loop {
            let input = collector.next(controller, deadline).await?;
            if settled_run(&input, run_id) {
                return wait_for_attempt_settlement(controller, collector, run_id, deadline).await;
            }
        }
    }

    fn first_run_is_valid(collector: &LiveCollector, run_id: crate::runtime::RunId) -> bool {
        collector.run(run_id).is_some_and(|run| {
            run.completed_executions >= 2
                && run.successful_read
                && run.final_messages == 1
                && run.nonblank_final_messages == 1
                && run.cancelled_runs == 0
                && run.failed_runs == 0
                && run.steps.len() <= 8
                && run.streamed_content
                && run.attempts_started > 0
                && run.attempts_started == run.attempts_settled
                && run.settled == 1
        })
    }

    fn cancellation_run_is_valid(
        collector: &LiveCollector,
        run_id: crate::runtime::RunId,
        disabled_events: usize,
    ) -> bool {
        collector.run(run_id).is_some_and(|run| {
            run.cancelled_runs == 1
                && run.final_messages == 0
                && run.failed_runs == 0
                && run.attempts_started > 0
                && run.attempts_started == run.attempts_settled
                && run.settled == 1
                && collector.disabled_events == disabled_events
        })
    }

    fn follow_up_run_is_valid(collector: &LiveCollector, run_id: crate::runtime::RunId) -> bool {
        collector.run(run_id).is_some_and(|run| {
            run.final_messages == 1
                && run.nonblank_final_messages == 1
                && run.cancelled_runs == 0
                && run.failed_runs == 0
                && run.attempts_started > 0
                && run.attempts_started == run.attempts_settled
                && run.settled == 1
        })
    }

    async fn run_live_scenario(
        controller: &mut LiveController,
    ) -> Result<LiveScenario, &'static str> {
        let outer_deadline = Instant::now() + LIVE_OUTER_TIMEOUT;
        send_command(
            &controller.commands,
            crate::runtime::RuntimeCommand::Submit {
                request: FIRST_REQUEST.to_owned(),
            },
        )?;
        let mut collector = LiveCollector::default();
        let first_run_id = wait_for_request(
            controller,
            &mut collector,
            FIRST_REQUEST,
            outer_deadline.min(Instant::now() + LIVE_PROVIDER_TIMEOUT),
        )
        .await?;
        wait_for_first_settlement(controller, &mut collector, first_run_id, outer_deadline).await?;
        if !first_run_is_valid(&collector, first_run_id) {
            return Err(LIVE_FIRST_SETTLEMENT);
        }

        let disabled_events = collector.disabled_events;
        let cancellation_start = Instant::now();
        let cancellation_start_deadline = cancellation_start + LIVE_CANCELLATION_TIMEOUT;
        send_command(
            &controller.commands,
            crate::runtime::RuntimeCommand::Submit {
                request: CANCELLATION_REQUEST.to_owned(),
            },
        )?;
        let second_run_id = wait_for_request(
            controller,
            &mut collector,
            CANCELLATION_REQUEST,
            cancellation_start_deadline,
        )
        .await?;
        wait_for_attempt_started(
            controller,
            &mut collector,
            second_run_id,
            cancellation_start_deadline,
        )
        .await
        .map_err(|_| LIVE_SECOND_START)?;
        send_command(
            &controller.commands,
            crate::runtime::RuntimeCommand::Cancel { correction: false },
        )?;
        let cancellation_settlement_deadline = Instant::now() + LIVE_CANCELLATION_TIMEOUT;
        wait_for_settlement(
            controller,
            &mut collector,
            second_run_id,
            cancellation_settlement_deadline,
        )
        .await?;
        if !cancellation_run_is_valid(&collector, second_run_id, disabled_events) {
            return Err(LIVE_SECOND_SETTLEMENT);
        }

        let follow_up_deadline = Instant::now() + LIVE_PROVIDER_TIMEOUT;
        send_command(
            &controller.commands,
            crate::runtime::RuntimeCommand::Submit {
                request: FOLLOW_UP_REQUEST.to_owned(),
            },
        )?;
        let third_run_id = wait_for_request(
            controller,
            &mut collector,
            FOLLOW_UP_REQUEST,
            follow_up_deadline,
        )
        .await?;
        wait_for_settlement(controller, &mut collector, third_run_id, follow_up_deadline).await?;
        if !follow_up_run_is_valid(&collector, third_run_id) {
            return Err(LIVE_THIRD_SETTLEMENT);
        }

        Ok(LiveScenario {
            first_run_id,
            second_run_id,
            third_run_id,
        })
    }

    fn record_summary(
        records: &[crate::runtime::StoredRecord],
        run_id: crate::runtime::RunId,
    ) -> (
        usize,
        usize,
        usize,
        usize,
        usize,
        usize,
        Vec<crate::runtime::ModelStepId>,
    ) {
        use crate::runtime::Record;

        let mut requests = 0;
        let mut completed_executions = 0;
        let mut final_messages = 0;
        let mut nonblank_final_messages = 0;
        let mut cancelled_runs = 0;
        let mut failed_runs = 0;
        let mut steps = Vec::new();
        for stored in records.iter().filter(|stored| stored.run_id == run_id) {
            match &stored.record {
                Record::UserRequest { .. } => requests += 1,
                Record::CompletedExecution { step_id, .. } => {
                    completed_executions += 1;
                    if !steps.contains(step_id) {
                        steps.push(*step_id);
                    }
                }
                Record::FinalMessage { step_id, text, .. } => {
                    final_messages += 1;
                    nonblank_final_messages += usize::from(!text.trim().is_empty());
                    if !steps.contains(step_id) {
                        steps.push(*step_id);
                    }
                }
                Record::CancelledRun { .. } => cancelled_runs += 1,
                Record::FailedRun { .. } => failed_runs += 1,
                Record::Compaction { .. } => {}
            }
        }
        (
            requests,
            completed_executions,
            final_messages,
            nonblank_final_messages,
            cancelled_runs,
            failed_runs,
            steps,
        )
    }

    fn records_match_scenario(
        records: &[crate::runtime::StoredRecord],
        scenario: &LiveScenario,
    ) -> bool {
        let first = record_summary(records, scenario.first_run_id);
        let second = record_summary(records, scenario.second_run_id);
        let third = record_summary(records, scenario.third_run_id);
        first.0 == 1
            && first.1 >= 2
            && first.2 == 1
            && first.3 == 1
            && first.4 == 0
            && first.5 == 0
            && first.6.len() <= 8
            && second.0 == 1
            && second.2 == 0
            && second.4 == 1
            && second.5 == 0
            && third.0 == 1
            && third.2 == 1
            && third.3 == 1
            && third.4 == 0
            && third.5 == 0
    }

    async fn cleanup_live_controller(
        mut controller: LiveController,
    ) -> Result<Vec<crate::runtime::StoredRecord>, &'static str> {
        let deadline = Instant::now() + LIVE_CLEANUP_TIMEOUT;
        let shutdown = tokio::select! {
            result = controller.commands.send(crate::runtime::RuntimeCommand::Shutdown) => result.is_ok(),
            _ = sleep_until(deadline) => false,
        };
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        match timeout(remaining, &mut controller.controller).await {
            Ok(Ok(records)) if shutdown => Ok(records),
            Ok(Ok(_)) => Err(LIVE_CLEANUP_SHUTDOWN),
            Ok(Err(_)) => Err(LIVE_CLEANUP_CONTROLLER),
            Err(_) => {
                controller.controller.abort();
                let _ = controller.controller.await;
                Err(LIVE_CLEANUP_TIMEOUT_MESSAGE)
            }
        }
    }

    async fn run_live_sol_smoke_test() -> Result<(), &'static str> {
        let mut controller = start_live_controller().await?;
        let scenario = match timeout(LIVE_OUTER_TIMEOUT, run_live_scenario(&mut controller)).await {
            Ok(result) => result,
            Err(_) => Err(LIVE_OUTER_DEADLINE),
        };
        let records = cleanup_live_controller(controller).await?;
        let scenario = scenario?;
        if !records_match_scenario(&records, &scenario) {
            return Err(LIVE_RECORDS);
        }
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires explicit OPENAI_API_KEY and MONTY_BIN"]
    async fn live_sol() {
        if let Err(message) = run_live_sol_smoke_test().await {
            panic!("{message}");
        }
    }
}
