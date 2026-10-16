use std::future::Future;
use std::time::{Duration, Instant};

use eventsource_stream::{EventStreamError, Eventsource};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ModelSelection, models::Provider, settings::CrossProviderReasoning},
    model::{
        AttemptAccounting, ForeignPart, Model, ModelAction, ModelCallId, ModelContextEntry,
        ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep, ModelUsage,
        OpaqueReplayItem, ProviderFailure, ProviderReplay, SummaryRequest, SummaryResult,
        canonical_compaction_summary,
    },
    presentation::{
        ContentBudget, PresentationBatcher, PresentationKind, PythonPreviewDecoder,
        SSE_FRAME_LIMIT, SseFrameLimit,
    },
    provider::{bounded_body, redact, retry_delay},
};

const GOOGLE_PROVIDER: &str = Provider::Google.id();
const GOOGLE_API_BASE: &str = "https://generativelanguage.googleapis.com";
const EXECUTE_PYTHON: &str = "execute_python";
// Base64 of "skip_thought_signature_validator"; required for stateless foreign tool calls.
const BYPASS_THOUGHT_SIGNATURE: &str = "c2tpcF90aG91Z2h0X3NpZ25hdHVyZV92YWxpZGF0b3I=";
const HTTP_RETRIES: u32 = 2;
const MAX_STREAM_ATTEMPTS: u32 = 3;
const STREAM_RESTART_DELAY: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub(crate) struct GoogleModel {
    prompt: String,
    api_key: String,
    endpoint: String,
    cross_provider_reasoning: CrossProviderReasoning,
    client: reqwest::Client,
    frame_limit: usize,
    #[cfg(test)]
    wait_entered: std::sync::Arc<tokio::sync::Notify>,
}

impl GoogleModel {
    pub(crate) fn new(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self {
        Self::with_endpoint(prompt, api_key, GOOGLE_API_BASE, cross_provider_reasoning)
    }

    pub(crate) fn with_prompt(&self, prompt: String) -> Self {
        Self {
            prompt,
            ..self.clone()
        }
    }

    pub(crate) fn with_endpoint(
        prompt: impl Into<String>,
        api_key: impl Into<String>,
        api_base: &str,
        cross_provider_reasoning: CrossProviderReasoning,
    ) -> Self {
        Self {
            prompt: prompt.into(),
            api_key: api_key.into(),
            endpoint: format!("{api_base}/v1beta/interactions?alt=sse"),
            cross_provider_reasoning,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("Google HTTP client"),
            frame_limit: SSE_FRAME_LIMIT,
            #[cfg(test)]
            wait_entered: Default::default(),
        }
    }

    fn request(
        &self,
        selection: ModelSelection,
        history: &ModelHistory,
    ) -> Result<Value, ModelError> {
        let model = selection.model();
        if model.provider() != Provider::Google {
            return Err(ModelError::InvalidSelection("unsupported Google model"));
        }
        let mut input = Vec::new();

        for entry in history.entries() {
            if let Some(parts) = entry.foreign_parts(GOOGLE_PROVIDER, self.cross_provider_reasoning)
            {
                if matches!(entry, ModelContextEntry::CompletedCode(_)) {
                    input.push(json!({
                        "type": "thought",
                        "signature": BYPASS_THOUGHT_SIGNATURE
                    }));
                }
                for part in parts {
                    push_foreign(&mut input, part);
                }
                continue;
            }
            match entry {
                ModelContextEntry::UserRequest { text } => {
                    input.push(user_input_step(text));
                }
                ModelContextEntry::CompactionSummary { summary } => {
                    input.push(user_input_step(&canonical_compaction_summary(summary)));
                }
                ModelContextEntry::SkillActivation(activation) => {
                    input.push(user_input_step(&activation.model_text()));
                }
                ModelContextEntry::CompletedCode(completed) => {
                    let steps = replay_steps(completed.replay())?;
                    match replay_action(&steps)? {
                        ModelAction::Code {
                            source,
                            call_id,
                            commentary,
                        } if source == completed.source()
                            && call_id == *completed.call_id()
                            && commentary.as_deref() == completed.commentary() => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "completed code does not match its Google replay",
                            ));
                        }
                    }
                    input.extend(step_values(&steps));
                    input.push(function_result_step(
                        completed.call_id().as_str(),
                        completed.result().feedback(),
                    ));
                }
                ModelContextEntry::FinalMessage {
                    text,
                    reasoning_summary: _,
                    replay,
                } => {
                    let steps = replay_steps(replay)?;
                    match replay_action(&steps)? {
                        ModelAction::Message { text: replay_text } if replay_text == *text => {}
                        _ => {
                            return Err(ModelError::InvalidHistory(
                                "final message does not match its Google replay",
                            ));
                        }
                    }
                    input.extend(step_values(&steps));
                }
            }
        }

        Ok(json!({
            "model": model.wire_id(),
            "service_tier": model.service_tier,
            "store": false,
            "stream": true,
            "system_instruction": self.prompt,
            "input": input,
            "tools": [{
                "type": "function",
                "name": EXECUTE_PYTHON,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"]
                }
            }],
            "generation_config": {
                "max_output_tokens": model.max_output_tokens,
                "thinking_level": selection.thinking(),
                "thinking_summaries": "auto",
                "tool_choice": "validated"
            }
        }))
    }

    fn summary_request(&self, request: &SummaryRequest) -> Result<Value, ModelError> {
        let model = request.selection.model();
        if model.provider() != Provider::Google {
            return Err(ModelError::InvalidSelection("unsupported Google model"));
        }
        Ok(json!({
            "model": model.wire_id(),
            "service_tier": model.service_tier,
            "store": false,
            "stream": true,
            "system_instruction": request.instructions,
            "input": [user_input_step(&request.input)],
            "generation_config": {
                "max_output_tokens": request.output_limit,
                "thinking_level": request.selection.thinking(),
                "thinking_summaries": "auto"
            }
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
        let (mut step, attempts) = self
            .with_restarts(&request, selection, cancellation, progress, |message| {
                self.completed_step(history, message)
            })
            .await?;
        step.attempts = attempts;
        Ok(step)
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
        let mut progress = |event: ModelProgress| {
            if matches!(event, ModelProgress::AttemptStarted { .. }) {
                attempt_started();
            }
        };
        let (text, attempts) = self
            .with_restarts(
                &request,
                selection,
                cancellation,
                &mut progress,
                |message| self.completed_summary(message),
            )
            .await?;
        Ok(SummaryResult { text, attempts })
    }

    async fn with_restarts<T>(
        &self,
        request: &Value,
        selection: ModelSelection,
        cancellation: &CancellationToken,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
        finish: impl Fn(StreamedInteraction) -> Result<T, ModelError>,
    ) -> Result<(T, Vec<AttemptAccounting>), ModelFailure> {
        let mut attempts = Vec::new();
        for attempt in 1..=MAX_STREAM_ATTEMPTS {
            if cancellation.is_cancelled() {
                return Err(ModelFailure::new(ModelError::Cancelled, attempts));
            }
            progress(ModelProgress::AttemptStarted { attempt });
            let mut presentation = PresentationBatcher::default();
            let streamed = self
                .stream(
                    request,
                    selection.model().wire_id(),
                    cancellation,
                    &mut presentation,
                    progress,
                )
                .await;
            presentation.flush_all(progress);
            let (result, accounting) = match streamed {
                Ok(message) => {
                    let usage = self.usage(&message.usage, selection);
                    let accounting = if message.retried_requests {
                        AttemptAccounting::KnownUsageWithUntrackedRetries(usage)
                    } else {
                        AttemptAccounting::KnownUsage(usage)
                    };
                    (
                        finish(message).map_err(AttemptFailure::Terminal),
                        accounting,
                    )
                }
                Err(failure) => (Err(failure), AttemptAccounting::UsageUnavailable),
            };
            progress(ModelProgress::AttemptSettled {
                attempt,
                accounting: accounting.clone(),
            });
            attempts.push(accounting);

            match result {
                Ok(value) => return Ok((value, attempts)),
                Err(AttemptFailure::Restartable(_)) if attempt < MAX_STREAM_ATTEMPTS => {
                    if let Err(error) = self
                        .cancellable_wait(STREAM_RESTART_DELAY, cancellation)
                        .await
                    {
                        return Err(ModelFailure::new(error, attempts));
                    }
                }
                Err(AttemptFailure::Restartable(error) | AttemptFailure::Terminal(error)) => {
                    return Err(ModelFailure::new(error, attempts));
                }
            }
        }
        unreachable!("stream attempts are bounded")
    }

    async fn cancellable_wait(
        &self,
        delay: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), ModelError> {
        #[cfg(test)]
        self.wait_entered.notify_one();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ModelError::Cancelled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }

    async fn send(
        &self,
        request: &Value,
        cancellation: &CancellationToken,
    ) -> Result<(reqwest::Response, bool), AttemptFailure> {
        let mut retry = 0;
        loop {
            let send = self
                .client
                .post(&self.endpoint)
                .header("x-goog-api-key", &self.api_key)
                .json(request)
                .send();
            let response = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ModelError::Cancelled.into()),
                response = send => response,
            }
            .map_err(|_| {
                AttemptFailure::Restartable(self.provider_error(
                    None,
                    None,
                    "provider transport failed",
                ))
            })?;
            let status = response.status();
            if status.is_success() {
                return Ok((response, retry > 0));
            }
            if retry == HTTP_RETRIES || !matches!(status.as_u16(), 408 | 429 | 500..=599) {
                return Err(self
                    .http_failure(status.as_u16(), response, cancellation)
                    .await
                    .into());
            }
            let delay = retry_delay(response.headers(), retry);
            drop(response);
            self.cancellable_wait(delay, cancellation).await?;
            retry += 1;
        }
    }

    async fn stream(
        &self,
        request: &Value,
        model: &str,
        cancellation: &CancellationToken,
        presentation: &mut PresentationBatcher,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<StreamedInteraction, AttemptFailure> {
        let (response, retried_requests) = self.send(request, cancellation).await?;
        let mut frames = SseFrameLimit::new(self.frame_limit);
        let chunks = response.bytes_stream().map(move |chunk| match chunk {
            Ok(chunk) => Ok(frames.complete_lines(&chunk)?),
            Err(_) => Err(AttemptFailure::Restartable(self.provider_error(
                None,
                None,
                "provider stream failed",
            ))),
        });
        let mut events = std::pin::pin!(chunks.eventsource());
        let mut message = StreamedInteraction {
            retried_requests,
            ..StreamedInteraction::default()
        };
        loop {
            let event = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ModelError::Cancelled.into()),
                event = events.next() => event,
            };
            let event = match event {
                None if message.completed => return Ok(message),
                None => {
                    return Err(AttemptFailure::Restartable(self.provider_error(
                        None,
                        None,
                        "provider stream ended without a terminal response",
                    )));
                }
                Some(Err(EventStreamError::Transport(failure))) => return Err(failure),
                Some(Err(_)) => {
                    return Err(ModelError::InvalidResponse(
                        "stream was not valid server-sent events",
                    )
                    .into());
                }
                Some(Ok(event)) => event,
            };
            if event.event == "done" || event.data.trim() == "[DONE]" {
                if message.completed {
                    return Ok(message);
                }
                return Err(AttemptFailure::Restartable(self.provider_error(
                    None,
                    None,
                    "provider stream ended without a terminal response",
                )));
            }
            let event = serde_json::from_str(&event.data)
                .map_err(|_| ModelError::InvalidResponse("stream contained a malformed event"))?;
            let now = Instant::now();
            match event {
                WireEvent::Error { error } => {
                    let restartable = error.code.as_deref().is_some_and(|code| {
                        matches!(
                            code,
                            "overloaded" | "unavailable" | "internal" | "resource_exhausted"
                        )
                    });
                    let error = self.provider_error(None, error.code, error.message);
                    return Err(if restartable {
                        AttemptFailure::Restartable(error)
                    } else {
                        AttemptFailure::Terminal(error)
                    });
                }
                WireEvent::Unexpected => {}
                WireEvent::InteractionCreated { .. } if message.started => {
                    return Err(
                        ModelError::InvalidResponse("stream repeated interaction.created").into(),
                    );
                }
                WireEvent::InteractionCreated { interaction: start } => {
                    validate_echoed_model(start.model.as_deref(), model)?;
                    message.started = true;
                    message
                        .usage
                        .update_interaction(start.service_tier, start.usage);
                }
                _ if !message.started => {
                    return Err(ModelError::InvalidResponse(
                        "stream content arrived before interaction.created",
                    )
                    .into());
                }
                _ if message.completed => {
                    return Err(ModelError::InvalidResponse(
                        "stream continued after interaction.completed",
                    )
                    .into());
                }
                WireEvent::InteractionStatusUpdate {
                    model: echoed_model,
                    usage,
                } => {
                    validate_echoed_model(echoed_model.as_deref(), model)?;
                    if let Some(usage) = usage {
                        message.usage.update(usage);
                    }
                }
                WireEvent::StepStart { index, step } => {
                    message.start_step(index, step, now, presentation, progress)?;
                }
                WireEvent::StepDelta {
                    index,
                    delta,
                    metadata,
                } => {
                    message.apply_delta(index, delta, now, presentation, progress)?;
                    if let Some(usage) = metadata.and_then(|metadata| metadata.total_usage) {
                        message.usage.update(usage);
                    }
                }
                WireEvent::StepStop { index, usage } => {
                    message.stop_step(index)?;
                    if let Some(usage) = usage {
                        message.usage.update(usage);
                    }
                }
                WireEvent::InteractionCompleted { .. } if message.open.is_some() => {
                    return Err(ModelError::InvalidResponse("stream stopped inside a step").into());
                }
                WireEvent::InteractionCompleted { interaction } => {
                    validate_echoed_model(interaction.model.as_deref(), model)?;
                    message.status = Some(interaction.status);
                    message
                        .usage
                        .update_interaction(interaction.service_tier, interaction.usage);
                    message.completed = true;
                }
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
                let authentication = status == 400
                    && error
                        .details
                        .iter()
                        .any(|detail| detail.reason.as_deref() == Some("API_KEY_INVALID"));
                let code = error
                    .status
                    .or_else(|| error.code.as_ref()?.as_str().map(str::to_owned));
                self.provider_error_with_auth(Some(status), code, error.message, authentication)
            }
            None => self.provider_error(Some(status), None, "provider request failed"),
        }
    }

    fn completed_step(
        &self,
        history: &ModelHistory,
        message: StreamedInteraction,
    ) -> Result<ModelStep, ModelError> {
        let status = match message.status.as_deref() {
            Some(status @ ("requires_action" | "completed")) => status,
            status => return Err(self.status_failure(status)),
        };
        let action = steps_action(&message.steps)?;
        match (&action, status) {
            (ModelAction::Code { call_id, .. }, "requires_action") => {
                if history.contains_call_id(call_id) {
                    return Err(ModelError::InvalidResponse(
                        "tool call identifier duplicates model history",
                    ));
                }
            }
            (ModelAction::Message { .. }, "completed") => {}
            _ => {
                return Err(ModelError::InvalidResponse(
                    "interaction status did not match the response content",
                ));
            }
        }
        let reasoning_summary = message
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Thought { summary, .. } => {
                    Some(summary.iter().map(|part| part.text.as_str()))
                }
                _ => None,
            })
            .flatten()
            .collect();
        let replay = ProviderReplay::new(
            GOOGLE_PROVIDER,
            message
                .steps
                .iter()
                .map(|step| OpaqueReplayItem::new(serde_json::to_vec(step).expect("step JSON")))
                .collect(),
        );
        Ok(ModelStep {
            action,
            reasoning_summary,
            replay,
            attempts: Vec::new(),
        })
    }

    fn completed_summary(&self, message: StreamedInteraction) -> Result<String, ModelError> {
        let tool_call = message.status.as_deref() == Some("requires_action")
            || message
                .steps
                .iter()
                .any(|step| matches!(step, Step::FunctionCall { .. }));
        if tool_call {
            return Err(ModelError::InvalidResponse(
                "summary response contained a tool call",
            ));
        }
        if message.status.as_deref() != Some("completed") {
            return Err(self.status_failure(message.status.as_deref()));
        }
        let text: String = message
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::ModelOutput { content } => {
                    Some(content.iter().map(|part| part.text.as_str()))
                }
                _ => None,
            })
            .flatten()
            .collect();
        if text.trim().is_empty() {
            return Err(ModelError::InvalidResponse(
                "summary text was empty or whitespace",
            ));
        }
        Ok(text)
    }

    fn status_failure(&self, status: Option<&str>) -> ModelError {
        match status {
            Some(status @ ("incomplete" | "failed" | "cancelled")) => {
                self.provider_error(None, Some(status.to_owned()), format!("response {status}"))
            }
            Some(_) | None => {
                ModelError::InvalidResponse("interaction completed without a terminal status")
            }
        }
    }

    fn usage(&self, usage: &UsageTotals, selection: ModelSelection) -> ModelUsage {
        let thought = usage.thought.unwrap_or(0);
        let output = usage.output.and_then(|output| output.checked_add(thought));
        ModelUsage {
            model: selection.model().id.to_owned(),
            service_tier: Some(
                usage
                    .service_tier
                    .as_deref()
                    .map(|tier| redact(tier, &self.api_key))
                    .unwrap_or_else(|| selection.model().service_tier.to_owned()),
            ),
            input_tokens: usage.input,
            cached_input_tokens: Some(usage.cached.unwrap_or(0)),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: output,
            reasoning_output_tokens: usage.thought.or(output.map(|_| 0)),
            total_tokens: usage.total,
        }
    }

    fn provider_error(
        &self,
        status: Option<u16>,
        code: Option<String>,
        message: impl Into<String>,
    ) -> ModelError {
        self.provider_error_with_auth(status, code, message, false)
    }

    fn provider_error_with_auth(
        &self,
        status: Option<u16>,
        code: Option<String>,
        message: impl Into<String>,
        authentication: bool,
    ) -> ModelError {
        // No prompt-too-long signature is verified, so no failure signals context overflow.
        ModelError::Provider(Box::new(
            ProviderFailure::new(
                status,
                code.map(|code| redact(&code, &self.api_key)),
                redact(&message.into(), &self.api_key),
            )
            .with_authentication(authentication),
        ))
    }
}

impl Model for GoogleModel {
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
    Restartable(ModelError),
    Terminal(ModelError),
}

impl From<ModelError> for AttemptFailure {
    fn from(error: ModelError) -> Self {
        Self::Terminal(error)
    }
}

fn validate_echoed_model(echoed: Option<&str>, expected: &str) -> Result<(), ModelError> {
    if echoed.is_some_and(|echoed| echoed != expected) {
        return Err(ModelError::InvalidResponse(
            "interaction did not echo the selected model",
        ));
    }
    Ok(())
}

fn user_input_step(text: &str) -> Value {
    json!({
        "type": "user_input",
        "content": [{"type": "text", "text": text}]
    })
}

fn model_output_step(text: &str) -> Value {
    json!({
        "type": "model_output",
        "content": [{"type": "text", "text": text}]
    })
}

fn function_call_step(call_id: &str, source: &str) -> Value {
    json!({
        "type": "function_call",
        "id": call_id,
        "name": EXECUTE_PYTHON,
        "arguments": {"code": source}
    })
}

fn function_result_step(call_id: &str, result: &str) -> Value {
    json!({
        "type": "function_result",
        "call_id": call_id,
        "name": EXECUTE_PYTHON,
        "result": result
    })
}

fn push_foreign(input: &mut Vec<Value>, part: ForeignPart<'_>) {
    match part {
        ForeignPart::Text(text) => input.push(model_output_step(text)),
        ForeignPart::ToolCall {
            call_id,
            source,
            output,
        } => {
            input.push(function_call_step(call_id, source));
            input.push(function_result_step(call_id, output));
        }
    }
}

fn step_values(steps: &[Step]) -> Vec<Value> {
    steps
        .iter()
        .map(|step| serde_json::to_value(step).expect("step JSON"))
        .collect()
}

fn replay_steps(replay: &ProviderReplay) -> Result<Vec<Step>, ModelError> {
    replay
        .items()
        .iter()
        .map(|item| {
            serde_json::from_slice(item.as_bytes())
                .map_err(|_| ModelError::InvalidHistory("opaque Google replay is malformed"))
        })
        .collect()
}

fn replay_action(steps: &[Step]) -> Result<ModelAction, ModelError> {
    steps_action(steps).map_err(|error| match error {
        ModelError::InvalidResponse(message) => ModelError::InvalidHistory(message),
        error => error,
    })
}

fn steps_action(steps: &[Step]) -> Result<ModelAction, ModelError> {
    let mut text = String::new();
    let mut function_call = None;
    for step in steps {
        match step {
            Step::Thought { .. } => {}
            Step::ModelOutput { content } => {
                for part in content {
                    text.push_str(&part.text);
                }
            }
            Step::FunctionCall {
                id,
                name,
                arguments,
            } => {
                if function_call.is_some() {
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
                function_call = Some((id, arguments));
            }
        }
    }
    match function_call {
        Some((id, arguments)) => Ok(ModelAction::Code {
            source: arguments.code.clone(),
            call_id: ModelCallId::new(id.as_str()),
            commentary: (!text.is_empty()).then_some(text),
        }),
        None if text.is_empty() => Err(ModelError::InvalidResponse("assistant message was empty")),
        None => Ok(ModelAction::Message { text }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    Thought {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        summary: Vec<TextContent>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        signature: String,
    },
    ModelOutput {
        content: Vec<TextContent>,
    },
    FunctionCall {
        id: String,
        name: String,
        arguments: ExecutePythonInput,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextContent {
    #[serde(rename = "type")]
    kind: TextKind,
    text: String,
}

impl TextContent {
    fn text(text: String) -> Self {
        Self {
            kind: TextKind::Text,
            text,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TextKind {
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutePythonInput {
    code: String,
}

enum OpenStep {
    Thought {
        summary: String,
        signature: String,
    },
    ModelOutput(String),
    FunctionCall {
        id: String,
        name: String,
        arguments: String,
        preview: Option<PythonPreviewDecoder>,
    },
}

impl OpenStep {
    fn finish(self) -> Result<Step, ModelError> {
        Ok(match self {
            Self::Thought { summary, signature } => Step::Thought {
                summary: if summary.is_empty() {
                    Vec::new()
                } else {
                    vec![TextContent::text(summary)]
                },
                signature,
            },
            Self::ModelOutput(text) => Step::ModelOutput {
                content: vec![TextContent::text(text)],
            },
            Self::FunctionCall {
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
                let arguments = serde_json::from_str(&arguments).map_err(|_| {
                    ModelError::InvalidResponse(
                        "execute_python arguments were not a closed code object",
                    )
                })?;
                Step::FunctionCall {
                    id,
                    name,
                    arguments,
                }
            }
        })
    }
}

#[derive(Default)]
struct UsageTotals {
    input: Option<u64>,
    cached: Option<u64>,
    output: Option<u64>,
    thought: Option<u64>,
    total: Option<u64>,
    service_tier: Option<String>,
}

impl UsageTotals {
    fn update(&mut self, usage: WireUsage) {
        self.input = usage.total_input_tokens.or(self.input);
        self.cached = usage.total_cached_tokens.or(self.cached);
        self.output = usage.total_output_tokens.or(self.output);
        self.thought = usage.total_thought_tokens.or(self.thought);
        self.total = usage.total_tokens.or(self.total);
    }

    fn update_interaction(&mut self, service_tier: Option<String>, usage: Option<WireUsage>) {
        self.service_tier = service_tier.or(self.service_tier.take());
        if let Some(usage) = usage {
            self.update(usage);
        }
    }
}

#[derive(Default)]
struct StreamedInteraction {
    retried_requests: bool,
    started: bool,
    completed: bool,
    usage: UsageTotals,
    steps: Vec<Step>,
    last_step_index: Option<usize>,
    open: Option<(usize, OpenStep)>,
    status: Option<String>,
    content: ContentBudget,
}

impl StreamedInteraction {
    fn start_step(
        &mut self,
        index: usize,
        step: WireStepStart,
        now: Instant,
        presentation: &mut PresentationBatcher,
        progress: &mut (dyn FnMut(ModelProgress) + Send),
    ) -> Result<(), ModelError> {
        if self.open.is_some() || self.last_step_index.is_some_and(|last| index <= last) {
            return Err(ModelError::InvalidResponse(
                "stream steps were out of order",
            ));
        }
        let open = match step {
            WireStepStart::Thought { summary, signature } => {
                let mut combined = String::new();
                for part in summary {
                    self.content.spend(part.text.len())?;
                    presentation.push(
                        PresentationKind::ReasoningSummary,
                        part.text.clone(),
                        now,
                        progress,
                    );
                    combined.push_str(&part.text);
                }
                self.content.spend(signature.len())?;
                OpenStep::Thought {
                    summary: combined,
                    signature,
                }
            }
            WireStepStart::ModelOutput { content } => {
                let mut combined = String::new();
                for part in content {
                    self.content.spend(part.text.len())?;
                    presentation.push(
                        PresentationKind::AssistantText,
                        part.text.clone(),
                        now,
                        progress,
                    );
                    combined.push_str(&part.text);
                }
                OpenStep::ModelOutput(combined)
            }
            WireStepStart::FunctionCall {
                id,
                name,
                arguments,
            } => {
                self.content.spend(id.len())?;
                self.content.spend(name.len())?;
                let mut preview = (name == EXECUTE_PYTHON).then(PythonPreviewDecoder::default);
                let initial = match arguments {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::Object(map)) if map.is_empty() => String::new(),
                    Some(Value::Object(map)) => serde_json::to_string(&map).expect("object JSON"),
                    Some(Value::String(text)) => text,
                    Some(_) => {
                        return Err(ModelError::InvalidResponse(
                            "execute_python arguments were not a closed code object",
                        ));
                    }
                };
                if !initial.is_empty() {
                    self.content.spend(initial.len())?;
                    if let Some(preview) = &mut preview {
                        let code = preview.push(&initial);
                        presentation.push(PresentationKind::DecodedPython, code, now, progress);
                    }
                }
                OpenStep::FunctionCall {
                    id,
                    name,
                    arguments: initial,
                    preview,
                }
            }
            WireStepStart::Unexpected => {
                return Err(ModelError::InvalidResponse(
                    "stream contained an unexpected step",
                ));
            }
        };
        self.last_step_index = Some(index);
        self.open = Some((index, open));
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
        let (_, open) = self
            .open
            .as_mut()
            .filter(|(open_index, _)| *open_index == index)
            .ok_or(ModelError::InvalidResponse(
                "stream delta did not match an open step",
            ))?;
        match (open, delta) {
            (
                OpenStep::Thought { summary, .. },
                WireDelta::ThoughtSummary {
                    content: WireText { text: delta },
                },
            ) => {
                self.content.spend(delta.len())?;
                summary.push_str(&delta);
                presentation.push(PresentationKind::ReasoningSummary, delta, now, progress);
            }
            (
                OpenStep::Thought { signature, .. },
                WireDelta::ThoughtSignature { signature: delta },
            ) => {
                self.content.spend(delta.len())?;
                signature.push_str(&delta);
            }
            (OpenStep::ModelOutput(text), WireDelta::Text { text: delta }) => {
                self.content.spend(delta.len())?;
                text.push_str(&delta);
                presentation.push(PresentationKind::AssistantText, delta, now, progress);
            }
            (
                OpenStep::FunctionCall {
                    arguments, preview, ..
                },
                WireDelta::ArgumentsDelta { arguments: delta },
            ) => {
                self.content.spend(delta.len())?;
                if let Some(preview) = preview {
                    let code = preview.push(&delta);
                    presentation.push(PresentationKind::DecodedPython, code, now, progress);
                }
                arguments.push_str(&delta);
            }
            (_, WireDelta::Unexpected) => {}
            _ => {
                return Err(ModelError::InvalidResponse(
                    "stream delta did not match its step",
                ));
            }
        }
        Ok(())
    }

    fn stop_step(&mut self, index: usize) -> Result<(), ModelError> {
        let (_, open) = self
            .open
            .take()
            .filter(|(open_index, _)| *open_index == index)
            .ok_or(ModelError::InvalidResponse(
                "stream stop did not match an open step",
            ))?;
        self.steps.push(open.finish()?);
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "event_type")]
enum WireEvent {
    #[serde(rename = "interaction.created")]
    InteractionCreated { interaction: WireInteraction },
    #[serde(rename = "interaction.status_update")]
    InteractionStatusUpdate {
        model: Option<String>,
        usage: Option<WireUsage>,
    },
    #[serde(rename = "step.start")]
    StepStart { index: usize, step: WireStepStart },
    #[serde(rename = "step.delta")]
    StepDelta {
        index: usize,
        delta: WireDelta,
        metadata: Option<WireDeltaMetadata>,
    },
    #[serde(rename = "step.stop")]
    StepStop {
        index: usize,
        usage: Option<WireUsage>,
    },
    #[serde(rename = "interaction.completed")]
    InteractionCompleted { interaction: WireInteraction },
    #[serde(rename = "error")]
    Error { error: WireStreamError },
    #[serde(other)]
    Unexpected,
}

#[derive(Deserialize)]
struct WireInteraction {
    model: Option<String>,
    status: String,
    service_tier: Option<String>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct WireDeltaMetadata {
    total_usage: Option<WireUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireStepStart {
    Thought {
        #[serde(default)]
        summary: Vec<WireText>,
        #[serde(default)]
        signature: String,
    },
    ModelOutput {
        #[serde(default)]
        content: Vec<WireText>,
    },
    FunctionCall {
        id: String,
        name: String,
        arguments: Option<Value>,
    },
    #[serde(other)]
    Unexpected,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireDelta {
    ThoughtSummary {
        content: WireText,
    },
    ThoughtSignature {
        signature: String,
    },
    Text {
        text: String,
    },
    ArgumentsDelta {
        arguments: String,
    },
    #[serde(other)]
    Unexpected,
}

#[derive(Deserialize)]
struct WireText {
    text: String,
}

#[derive(Deserialize)]
struct WireUsage {
    total_input_tokens: Option<u64>,
    total_cached_tokens: Option<u64>,
    total_output_tokens: Option<u64>,
    total_thought_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireStreamError {
    code: Option<String>,
    message: String,
}

#[derive(Deserialize)]
struct WireErrorBody {
    error: WireHttpError,
}

#[derive(Deserialize)]
struct WireHttpError {
    status: Option<String>,
    code: Option<Value>,
    message: String,
    #[serde(default)]
    details: Vec<WireErrorDetail>,
}

#[derive(Deserialize)]
struct WireErrorDetail {
    reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio_util::sync::CancellationToken;

    use super::{BYPASS_THOUGHT_SIGNATURE, GoogleModel, SSE_FRAME_LIMIT};
    use crate::{
        config::{
            ModelSelection,
            constants::{
                ACCEPTED_MODEL_CONTENT_LIMIT, PROVIDER_CODE_LIMIT, PROVIDER_MESSAGE_LIMIT,
            },
            models::{GEMINI_3_8_FLASH, GEMINI_4_ARGON, OPUS_5_5, SOL},
            settings::CrossProviderReasoning,
        },
        model::{
            AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelError,
            ModelFailure, ModelHistory, ModelProgress, ModelStep, ModelUsage, OpaqueReplayItem,
            ProviderFailure, ProviderReplay, SkillActivation, SummaryRequest, SummaryResult,
            canonical_compaction_summary,
        },
        pricing::{PricingError, RunCostEstimate, estimate_run_cost, estimate_usage},
        skills::SkillOrigin,
        sse_fixture::{Fixture, FixtureReply},
    };

    const KEY: &str = "fixture-key";
    const HOSTILE_KEY: &str = "AIzaSy-hostile-credential";
    const METADATA: &str = "req_hostile_metadata";
    const TOOL_OVERHEAD: usize = "call_1".len() + "execute_python".len() + r#"{"code":""}"#.len();
    const IDLE_STREAM: &str = "event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_1\",\"model\":\"gemini-4-argon\",\"status\":\"in_progress\"}}\n\n";

    fn selection() -> ModelSelection {
        ModelSelection::new(&GEMINI_4_ARGON, GEMINI_4_ARGON.default_thinking).unwrap()
    }

    fn history(request: &str) -> ModelHistory {
        let mut history = ModelHistory::new();
        history.append_user_request(request);
        history
    }

    fn final_usage() -> Value {
        json!({
            "total_input_tokens": 12,
            "total_cached_tokens": 2,
            "total_output_tokens": 15,
            "total_thought_tokens": 25,
            "total_tokens": 52
        })
    }

    fn expected_accounting() -> AttemptAccounting {
        AttemptAccounting::KnownUsage(expected_usage())
    }

    fn retried_accounting() -> AttemptAccounting {
        AttemptAccounting::KnownUsageWithUntrackedRetries(expected_usage())
    }

    fn expected_usage() -> ModelUsage {
        ModelUsage {
            model: "google/gemini-4-argon".to_owned(),
            service_tier: Some("standard".to_owned()),
            input_tokens: Some(12),
            cached_input_tokens: Some(2),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(40),
            reasoning_output_tokens: Some(25),
            total_tokens: Some(52),
        }
    }

    fn interaction_created() -> Value {
        json!({
            "event_type": "interaction.created",
            "interaction": {
                "id": "int_1",
                "model": "gemini-4-argon",
                "status": "in_progress"
            }
        })
    }

    fn status_update(status: &str) -> Value {
        json!({
            "event_type": "interaction.status_update",
            "interaction_id": "int_1",
            "status": status
        })
    }

    fn step_start(index: usize, step: Value) -> Value {
        json!({"event_type": "step.start", "index": index, "step": step})
    }

    fn step_delta(index: usize, delta: Value) -> Value {
        json!({"event_type": "step.delta", "index": index, "delta": delta})
    }

    fn step_stop(index: usize) -> Value {
        json!({"event_type": "step.stop", "index": index})
    }

    fn thought(index: usize, parts: &[&str], signature: &str) -> Vec<Value> {
        let mut events = vec![step_start(index, json!({"type": "thought"}))];
        events.extend(parts.iter().map(|part| {
            step_delta(
                index,
                json!({
                    "type": "thought_summary",
                    "content": {"type": "text", "text": part}
                }),
            )
        }));
        if !signature.is_empty() {
            events.push(step_delta(
                index,
                json!({"type": "thought_signature", "signature": signature}),
            ));
        }
        events.push(step_stop(index));
        events
    }

    fn model_output(index: usize, text: &str) -> Vec<Value> {
        vec![
            step_start(index, json!({"type": "model_output"})),
            step_delta(index, json!({"type": "text", "text": text})),
            step_stop(index),
        ]
    }

    fn function_call(index: usize, id: &str, fragments: &[&str]) -> Vec<Value> {
        let mut events = vec![step_start(
            index,
            json!({"type": "function_call", "id": id, "name": "execute_python"}),
        )];
        events.extend(fragments.iter().map(|fragment| {
            step_delta(
                index,
                json!({"type": "arguments_delta", "arguments": fragment}),
            )
        }));
        events.push(step_stop(index));
        events
    }

    fn sse(events: impl IntoIterator<Item = Value>) -> String {
        events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["event_type"].as_str().unwrap()
                )
            })
            .collect()
    }

    fn stream_with_tier(
        steps: Vec<Vec<Value>>,
        status: &str,
        service_tier: Option<&str>,
        usage: Value,
    ) -> String {
        let mut events = vec![interaction_created(), status_update("in_progress")];
        events.extend(steps.into_iter().flatten());
        let mut interaction = json!({
            "id": "int_1",
            "model": "gemini-4-argon",
            "status": status,
            "usage": usage
        });
        if let Some(tier) = service_tier {
            interaction["service_tier"] = json!(tier);
        }
        events.push(json!({
            "event_type": "interaction.completed",
            "interaction": interaction
        }));
        format!("{}event: done\ndata: [DONE]\n\n", sse(events))
    }

    fn stream(steps: Vec<Vec<Value>>, status: &str, usage: Value) -> String {
        stream_with_tier(steps, status, None, usage)
    }

    fn tool_stream() -> String {
        stream(
            vec![
                thought(0, &["Checking ", "the file"], "sig-1"),
                model_output(1, "I'll run it."),
                function_call(2, "call_1", &[r#"{"code":"pri"#, r#"nt(1)"}"#]),
            ],
            "requires_action",
            final_usage(),
        )
    }

    fn message_stream() -> String {
        stream(
            vec![
                thought(0, &["Summarising"], "sig-2"),
                model_output(1, "Done."),
            ],
            "completed",
            final_usage(),
        )
    }

    fn tool_steps() -> Vec<Value> {
        vec![
            json!({
                "type": "thought",
                "summary": [{"type": "text", "text": "Checking the file"}],
                "signature": "sig-1"
            }),
            json!({
                "type": "model_output",
                "content": [{"type": "text", "text": "I'll run it."}]
            }),
            json!({
                "type": "function_call",
                "id": "call_1",
                "name": "execute_python",
                "arguments": {"code": "print(1)"}
            }),
        ]
    }

    fn message_steps() -> Vec<Value> {
        vec![
            json!({
                "type": "thought",
                "summary": [{"type": "text", "text": "Summarising"}],
                "signature": "sig-2"
            }),
            json!({
                "type": "model_output",
                "content": [{"type": "text", "text": "Done."}]
            }),
        ]
    }

    fn replay(steps: &[Value]) -> ProviderReplay {
        ProviderReplay::new(
            "google",
            steps
                .iter()
                .map(|step| OpaqueReplayItem::new(serde_json::to_vec(step).unwrap()))
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
                ModelCallId::new("call_1"),
                Some("I'll run it.".to_owned()),
                "Checking the file".to_owned(),
                replay(&tool_steps()),
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        history
    }

    fn user_step(text: &str) -> Value {
        json!({
            "type": "user_input",
            "content": [{"type": "text", "text": text}]
        })
    }

    fn expected_request(input: Value) -> Value {
        json!({
            "model": "gemini-4-argon",
            "service_tier": "standard",
            "store": false,
            "stream": true,
            "system_instruction": "system prompt",
            "input": input,
            "tools": [{
                "type": "function",
                "name": "execute_python",
                "parameters": {
                    "type": "object",
                    "properties": {"code": {"type": "string"}},
                    "required": ["code"]
                }
            }],
            "generation_config": {
                "max_output_tokens": 65536,
                "thinking_level": "medium",
                "thinking_summaries": "auto",
                "tool_choice": "validated"
            }
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

    fn model(fixture: &Fixture, key: &str) -> GoogleModel {
        GoogleModel::with_endpoint(
            "system prompt",
            key,
            &fixture.endpoint,
            CrossProviderReasoning::Inline,
        )
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
        assert!(!diagnostics.contains("AIzaSy-"), "{diagnostics}");
        assert!(!diagnostics.contains(METADATA), "{diagnostics}");
        assert!(provider.message().len() <= PROVIDER_MESSAGE_LIMIT);
        assert!(
            provider
                .code()
                .is_none_or(|code| code.len() <= PROVIDER_CODE_LIMIT)
        );
    }

    #[test]
    fn production_model_posts_to_the_google_interactions_sse_endpoint() {
        assert_eq!(
            GoogleModel::new("system prompt", KEY, CrossProviderReasoning::Inline).endpoint,
            "https://generativelanguage.googleapis.com/v1beta/interactions?alt=sse"
        );
    }

    #[test]
    fn constructs_google_requests_and_summaries_for_both_models_at_every_thinking_level() {
        let model = GoogleModel::new("system prompt", KEY, CrossProviderReasoning::Inline);
        for configuration in [&GEMINI_4_ARGON, &GEMINI_3_8_FLASH] {
            for &thinking in configuration.thinking_levels {
                let selection = ModelSelection::new(configuration, thinking).unwrap();
                let request = model.request(selection, &history("inspect")).unwrap();
                assert_eq!(request["model"], json!(configuration.wire_id()));
                assert_eq!(request["service_tier"], json!("standard"));
                assert_eq!(request["store"], json!(false));
                assert_eq!(request["stream"], json!(true));
                assert_eq!(request["system_instruction"], json!("system prompt"));
                assert_eq!(
                    request["generation_config"],
                    json!({
                        "max_output_tokens": 65536,
                        "thinking_level": thinking,
                        "thinking_summaries": "auto",
                        "tool_choice": "validated"
                    })
                );
                assert!(
                    request["tools"][0]["parameters"]
                        .get("additionalProperties")
                        .is_none()
                );

                let summary = model.summary_request(&summary_request(selection)).unwrap();
                assert_eq!(summary["model"], json!(configuration.wire_id()));
                assert_eq!(summary["service_tier"], json!("standard"));
                assert_eq!(summary["store"], json!(false));
                assert_eq!(summary["stream"], json!(true));
                assert_eq!(
                    summary["generation_config"],
                    json!({
                        "max_output_tokens": 4096,
                        "thinking_level": thinking,
                        "thinking_summaries": "auto"
                    })
                );
                assert!(summary.get("tools").is_none());
            }
        }
    }

    #[test]
    fn activated_skill_body_is_projected_once_in_next_google_request() {
        let model = GoogleModel::new("system", KEY, CrossProviderReasoning::Inline);
        let mut history = history("inspect");
        let activation = SkillActivation::new(
            "build".to_owned(),
            PathBuf::from("/home/skills/build"),
            "---\nname: build\ndescription: Build\n---\nRun the build.".to_owned(),
            SkillOrigin::User,
            None,
        )
        .unwrap();
        history.activate_skill(activation.clone()).unwrap();
        history.activate_skill(activation).unwrap();
        let request = model.request(selection(), &history).unwrap();
        let steps = request["input"].as_array().unwrap();
        assert_eq!(
            steps
                .iter()
                .filter(|step| step["type"] == "user_input")
                .flat_map(|step| step["content"].as_array().into_iter().flatten())
                .filter(|part| part["text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Run the build.")))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn sends_the_exact_fresh_request_headers_and_body() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;

        let (result, _) = step_with(
            &fixture,
            KEY,
            ModelSelection::new(&GEMINI_4_ARGON, "high").unwrap(),
            &history("inspect this project"),
        )
        .await;
        result.unwrap();

        let captured = fixture.request();
        assert!(
            captured
                .head
                .starts_with("POST /v1beta/interactions?alt=sse HTTP/1.1\r\n")
        );
        let headers = headers(&captured.head);
        assert_eq!(
            headers.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "accept",
                "content-length",
                "content-type",
                "host",
                "x-goog-api-key"
            ]
        );
        assert_eq!(headers["x-goog-api-key"], KEY);
        assert_eq!(headers["content-type"], "application/json");
        let mut expected = expected_request(json!([user_step("inspect this project")]));
        expected["generation_config"]["thinking_level"] = json!("high");
        assert_eq!(captured.body, expected);
    }

    #[tokio::test]
    async fn tool_call_stream_returns_code_with_commentary_reasoning_preview_and_replay() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(tool_stream())).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Code {
                source: "print(1)".to_owned(),
                call_id: ModelCallId::new("call_1"),
                commentary: Some("I'll run it.".to_owned()),
            }
        );
        assert_eq!(step.reasoning_summary, "Checking the file");
        assert_eq!(step.replay.provider(), "google");
        assert_eq!(replay_values(&step.replay), tool_steps());
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
    async fn completed_stream_returns_the_final_message_with_or_without_done_sentinel() {
        let without_done = sse(vec![interaction_created()]
            .into_iter()
            .chain(thought(0, &["Summarising"], "sig-2"))
            .chain(model_output(1, "Done."))
            .chain([json!({
                "event_type": "interaction.completed",
                "interaction": {
                    "id": "int_1",
                    "model": "gemini-4-argon",
                    "status": "completed",
                    "usage": final_usage()
                }
            })]));
        for body in [message_stream(), without_done] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(body)).await;

            let (result, progress) = step(&fixture, &history("inspect")).await;
            let step = result.unwrap();

            assert_eq!(
                step.action,
                ModelAction::Message {
                    text: "Done.".to_owned(),
                }
            );
            assert_eq!(step.reasoning_summary, "Summarising");
            assert_eq!(replay_values(&step.replay), message_steps());
            assert_eq!(step.attempts, vec![expected_accounting()]);
            assert_eq!(
                streamed(&progress),
                ("Summarising".to_owned(), "Done.".to_owned(), String::new())
            );
        }
    }

    #[tokio::test]
    async fn step_start_accepts_initial_summary_signature_content_and_arguments() {
        let initial_stream = stream(
            vec![
                vec![
                    step_start(
                        0,
                        json!({
                            "type": "thought",
                            "summary": [{"type": "text", "text": "Checking "}],
                            "signature": "sig-"
                        }),
                    ),
                    step_delta(
                        0,
                        json!({
                            "type": "thought_summary",
                            "content": {"type": "text", "text": "the file"}
                        }),
                    ),
                    step_delta(0, json!({"type": "thought_signature", "signature": "1"})),
                    step_stop(0),
                ],
                vec![
                    step_start(
                        1,
                        json!({
                            "type": "model_output",
                            "content": [{"type": "text", "text": "I'll "}]
                        }),
                    ),
                    step_delta(1, json!({"type": "text", "text": "run it."})),
                    step_stop(1),
                ],
                vec![
                    step_start(
                        2,
                        json!({
                            "type": "function_call",
                            "id": "call_1",
                            "name": "execute_python",
                            "arguments": {"code": "print(1)"}
                        }),
                    ),
                    step_stop(2),
                ],
            ],
            "requires_action",
            final_usage(),
        );
        let fixture = Fixture::start(FixtureReply::OwnedSse(initial_stream)).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Code {
                source: "print(1)".to_owned(),
                call_id: ModelCallId::new("call_1"),
                commentary: Some("I'll run it.".to_owned()),
            }
        );
        assert_eq!(step.reasoning_summary, "Checking the file");
        assert_eq!(replay_values(&step.replay), tool_steps());
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
    async fn stream_accepts_annotated_text_content_and_ignores_unknown_step_deltas() {
        let annotated_stream = stream(
            vec![
                vec![
                    step_start(
                        0,
                        json!({
                            "type": "thought",
                            "summary": [{
                                "type": "text",
                                "text": "Checking ",
                                "annotations": [{
                                    "type": "url_citation",
                                    "start_index": 0,
                                    "end_index": 8,
                                    "url": "https://example.com",
                                    "title": "Example"
                                }]
                            }],
                            "signature": "sig-2"
                        }),
                    ),
                    step_delta(
                        0,
                        json!({
                            "type": "thought_summary",
                            "content": {
                                "type": "text",
                                "text": "the file",
                                "annotations": [{
                                    "type": "file_citation",
                                    "file_name": "spec.md",
                                    "start_index": 0,
                                    "end_index": 8
                                }]
                            }
                        }),
                    ),
                    step_delta(
                        0,
                        json!({
                            "type": "text_annotation_delta",
                            "annotations": [{
                                "type": "url_citation",
                                "start_index": 0,
                                "end_index": 8,
                                "url": "https://example.com"
                            }]
                        }),
                    ),
                    step_stop(0),
                ],
                vec![
                    step_start(
                        1,
                        json!({
                            "type": "model_output",
                            "content": [{
                                "type": "text",
                                "text": "Done",
                                "annotations": [{
                                    "type": "url_citation",
                                    "start_index": 0,
                                    "end_index": 4,
                                    "url": "https://example.com",
                                    "title": "Example"
                                }]
                            }]
                        }),
                    ),
                    step_delta(
                        1,
                        json!({
                            "type": "text_annotation_delta",
                            "annotations": [{
                                "type": "url_citation",
                                "start_index": 0,
                                "end_index": 4,
                                "url": "https://example.com"
                            }]
                        }),
                    ),
                    step_delta(1, json!({"type": "text", "text": "."})),
                    step_stop(1),
                ],
            ],
            "completed",
            final_usage(),
        );
        let fixture = Fixture::start(FixtureReply::OwnedSse(annotated_stream)).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Message {
                text: "Done.".to_owned(),
            }
        );
        assert_eq!(step.reasoning_summary, "Checking the file");
        assert_eq!(
            replay_values(&step.replay),
            vec![
                json!({
                    "type": "thought",
                    "summary": [{"type": "text", "text": "Checking the file"}],
                    "signature": "sig-2"
                }),
                json!({
                    "type": "model_output",
                    "content": [{"type": "text", "text": "Done."}]
                }),
            ]
        );
        assert_eq!(
            streamed(&progress),
            (
                "Checking the file".to_owned(),
                "Done.".to_owned(),
                String::new()
            )
        );

        let mut annotated_replay = history("inspect");
        annotated_replay.append_final_message(
            "Done.".to_owned(),
            String::new(),
            replay(&[json!({
                "type": "model_output",
                "content": [{
                    "type": "text",
                    "text": "Done.",
                    "annotations": [{"type": "url_citation", "url": "https://example.com"}]
                }]
            })]),
        );
        assert_eq!(
            model(&fixture, KEY)
                .validate_history(&annotated_replay, selection())
                .unwrap_err(),
            ModelError::InvalidHistory("opaque Google replay is malformed")
        );
    }

    #[tokio::test]
    async fn non_zero_increasing_step_indices_assemble_in_order() {
        let offset_stream = stream(
            vec![
                thought(3, &["Checking ", "the file"], "sig-1"),
                model_output(4, "I'll run it."),
                function_call(5, "call_1", &[r#"{"code":"pri"#, r#"nt(1)"}"#]),
            ],
            "requires_action",
            final_usage(),
        );
        let fixture = Fixture::start(FixtureReply::OwnedSse(offset_stream)).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Code {
                source: "print(1)".to_owned(),
                call_id: ModelCallId::new("call_1"),
                commentary: Some("I'll run it.".to_owned()),
            }
        );
        assert_eq!(step.reasoning_summary, "Checking the file");
        assert_eq!(replay_values(&step.replay), tool_steps());
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
    async fn replay_round_trips_into_the_next_request_and_after_compaction() {
        let fixture = Fixture::start_sequence(vec![
            FixtureReply::OwnedSse(tool_stream()),
            FixtureReply::OwnedSse(message_stream()),
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
                source.clone(),
                call_id.clone(),
                commentary.clone(),
                first.reasoning_summary.clone(),
                first.replay.clone(),
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

        let mut compacted = ModelHistory::new();
        compacted.append_compaction_summary("saved summary");
        compacted
            .append_completed_code(
                source,
                call_id,
                commentary,
                first.reasoning_summary,
                first.replay,
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        compacted.append_user_request("next");
        model
            .step(selection(), &compacted, &cancellation, &mut progress)
            .await
            .unwrap();

        let requests = fixture.requests.lock().unwrap().clone();
        let mut expected_tool_turn = vec![user_step("inspect")];
        expected_tool_turn.extend(tool_steps());
        expected_tool_turn.push(json!({
            "type": "function_result",
            "call_id": "call_1",
            "name": "execute_python",
            "result": "1\n"
        }));
        assert_eq!(
            requests[1].body,
            expected_request(json!(expected_tool_turn))
        );

        let mut expected_next = expected_tool_turn;
        expected_next.extend(message_steps());
        expected_next.push(user_step("thanks"));
        assert_eq!(
            model.request(selection(), &history).unwrap(),
            expected_request(json!(expected_next))
        );

        let mut expected_compacted =
            vec![user_step(&canonical_compaction_summary("saved summary"))];
        expected_compacted.extend(tool_steps());
        expected_compacted.push(json!({
            "type": "function_result",
            "call_id": "call_1",
            "name": "execute_python",
            "result": "1\n"
        }));
        expected_compacted.push(user_step("next"));
        assert_eq!(
            requests[2].body,
            expected_request(json!(expected_compacted))
        );
    }

    fn foreign_replay(provider: &str, items: &[Value]) -> ProviderReplay {
        ProviderReplay::new(
            provider,
            items
                .iter()
                .map(|item| OpaqueReplayItem::new(serde_json::to_vec(item).unwrap()))
                .collect(),
        )
    }

    fn cross_provider_history(code_reasoning: &str) -> ModelHistory {
        let mut history = history("first request");
        history.append_final_message(
            "Done.".to_owned(),
            "Summarising".to_owned(),
            replay(&message_steps()),
        );
        history.append_user_request("inspect openai");
        history
            .append_completed_code(
                "print(1)".to_owned(),
                ModelCallId::new("call_openai_1"),
                Some("I'll run it.".to_owned()),
                code_reasoning.to_owned(),
                foreign_replay(
                    "openai",
                    &[
                        json!({
                            "type": "reasoning",
                            "id": "rs_1",
                            "summary": [{"type": "summary_text", "text": code_reasoning}],
                            "encrypted_content": "openai-encrypted-code",
                            "status": "completed"
                        }),
                        json!({
                            "type": "function_call",
                            "id": "fc_1",
                            "call_id": "call_openai_1",
                            "name": "execute_python",
                            "arguments": r#"{"code":"print(1)"}"#,
                            "status": "completed"
                        }),
                    ],
                ),
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        history.append_final_message(
            "It printed 1.".to_owned(),
            "OpenAI summary".to_owned(),
            foreign_replay(
                "openai",
                &[json!({
                    "type": "reasoning",
                    "id": "rs_2",
                    "encrypted_content": "openai-encrypted-final",
                    "status": "completed"
                })],
            ),
        );
        history.append_user_request("inspect anthropic");
        history
            .append_completed_code(
                "print(2)".to_owned(),
                ModelCallId::new("toolu_1"),
                None,
                code_reasoning.to_owned(),
                foreign_replay(
                    "anthropic",
                    &[
                        json!({
                            "type": "thinking",
                            "thinking": code_reasoning,
                            "signature": "anthropic-signature"
                        }),
                        json!({
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "execute_python",
                            "input": {"code": "print(2)"}
                        }),
                    ],
                ),
                CodeResult::execution("2\n".to_owned()),
            )
            .unwrap();
        history.append_final_message(
            "It printed 2.".to_owned(),
            "Anthropic summary".to_owned(),
            foreign_replay(
                "anthropic",
                &[json!({"type": "redacted_thinking", "data": "anthropic-redacted-data"})],
            ),
        );
        history.append_user_request("thanks");
        history
    }

    #[tokio::test]
    async fn foreign_openai_and_anthropic_entries_replay_with_bypass_thought_step() {
        for (reasoning, code_reasoning, reasoning_texts) in [
            (
                CrossProviderReasoning::Inline,
                "Checking the file",
                [
                    Some("Checking the file"),
                    Some("OpenAI summary"),
                    Some("Anthropic summary"),
                ],
            ),
            (
                CrossProviderReasoning::Inline,
                "",
                [None, Some("OpenAI summary"), Some("Anthropic summary")],
            ),
            (
                CrossProviderReasoning::Drop,
                "Checking the file",
                [None, None, None],
            ),
        ] {
            let fixture =
                Fixture::start_sequence(vec![FixtureReply::OwnedSse(message_stream()); 2]).await;
            let mut parent =
                GoogleModel::with_endpoint("system prompt", KEY, &fixture.endpoint, reasoning);
            let mut child = parent.with_prompt("child prompt".to_owned());
            let history = cross_provider_history(code_reasoning);
            let original = history.clone();
            let cancellation = CancellationToken::new();

            for model in [&mut parent, &mut child] {
                model
                    .step(selection(), &history, &cancellation, &mut |_| {})
                    .await
                    .unwrap();
            }

            let bypass_thought = json!({"type": "thought", "signature": BYPASS_THOUGHT_SIGNATURE});
            let model_step = |text: &str| {
                json!({
                    "type": "model_output",
                    "content": [{"type": "text", "text": text}]
                })
            };
            let mut input = vec![user_step("first request")];
            input.extend(message_steps());
            input.push(user_step("inspect openai"));
            input.push(bypass_thought.clone());
            input.extend(reasoning_texts[0].map(model_step));
            input.extend([
                model_step("I'll run it."),
                json!({
                    "type": "function_call",
                    "id": "call_openai_1",
                    "name": "execute_python",
                    "arguments": {"code": "print(1)"}
                }),
                json!({
                    "type": "function_result",
                    "call_id": "call_openai_1",
                    "name": "execute_python",
                    "result": "1\n"
                }),
            ]);
            input.extend(reasoning_texts[1].map(model_step));
            input.push(model_step("It printed 1."));
            input.push(user_step("inspect anthropic"));
            input.push(bypass_thought);
            input.extend(reasoning_texts[0].map(model_step));
            input.extend([
                json!({
                    "type": "function_call",
                    "id": "toolu_1",
                    "name": "execute_python",
                    "arguments": {"code": "print(2)"}
                }),
                json!({
                    "type": "function_result",
                    "call_id": "toolu_1",
                    "name": "execute_python",
                    "result": "2\n"
                }),
            ]);
            input.extend(reasoning_texts[2].map(model_step));
            input.push(model_step("It printed 2."));
            input.push(user_step("thanks"));

            let parent_expected = expected_request(json!(input));
            let mut child_expected = parent_expected.clone();
            child_expected["system_instruction"] = json!("child prompt");
            let requests = fixture.requests.lock().unwrap().clone();
            assert_eq!(
                requests
                    .iter()
                    .map(|request| request.body.clone())
                    .collect::<Vec<_>>(),
                [parent_expected, child_expected],
                "{reasoning:?} {code_reasoning:?}"
            );
            for request in &requests {
                let body = request.body.to_string();
                for opaque in [
                    "openai-encrypted",
                    "rs_1",
                    "fc_1",
                    "anthropic-signature",
                    "anthropic-redacted-data",
                ] {
                    assert!(!body.contains(opaque), "{opaque} leaked into {body}");
                }
            }
            assert_eq!(history, original);
        }
    }

    #[tokio::test]
    async fn invalid_replay_and_selections_fail_before_networking() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;
        let mut model = model(&fixture, KEY);
        let cancellation = CancellationToken::new();
        let mut malformed = history("inspect");
        malformed.append_final_message(
            "Done.".to_owned(),
            String::new(),
            ProviderReplay::new("google", vec![OpaqueReplayItem::new(b"{".to_vec())]),
        );
        let mut mismatched_code = history("inspect");
        mismatched_code
            .append_completed_code(
                "print(2)".to_owned(),
                ModelCallId::new("call_1"),
                Some("I'll run it.".to_owned()),
                String::new(),
                replay(&tool_steps()),
                CodeResult::execution("2\n".to_owned()),
            )
            .unwrap();
        let mut mismatched_message = history("inspect");
        mismatched_message.append_final_message(
            "Other.".to_owned(),
            String::new(),
            replay(&message_steps()),
        );
        let openai = ModelSelection::new(&SOL, SOL.default_thinking).unwrap();
        let anthropic = ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap();

        for (history, selection, expected) in [
            (
                &malformed,
                selection(),
                ModelError::InvalidHistory("opaque Google replay is malformed"),
            ),
            (
                &mismatched_code,
                selection(),
                ModelError::InvalidHistory("completed code does not match its Google replay"),
            ),
            (
                &mismatched_message,
                selection(),
                ModelError::InvalidHistory("final message does not match its Google replay"),
            ),
            (
                &history("inspect"),
                openai,
                ModelError::InvalidSelection("unsupported Google model"),
            ),
            (
                &history("inspect"),
                anthropic,
                ModelError::InvalidSelection("unsupported Google model"),
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
    async fn usage_maps_input_cached_output_thought_and_service_tier_for_pricing() {
        for (configuration, tier, expected_tier, expected_nanos) in [
            (
                &GEMINI_4_ARGON,
                None,
                "standard",
                Ok(80 * 2_000 + 20 * 100 + 45 * 10_000),
            ),
            (
                &GEMINI_4_ARGON,
                Some("standard"),
                "standard",
                Ok(80 * 2_000 + 20 * 100 + 45 * 10_000),
            ),
            (
                &GEMINI_3_8_FLASH,
                Some("standard"),
                "standard",
                Ok(80 * 750 + 20 * 75 + 45 * 3_750),
            ),
            (
                &GEMINI_4_ARGON,
                Some("priority"),
                "priority",
                Err(PricingError::ServiceTierMismatch),
            ),
        ] {
            let mut body = stream_with_tier(
                vec![model_output(0, "Done.")],
                "completed",
                tier,
                json!({
                    "total_input_tokens": 100,
                    "total_cached_tokens": 20,
                    "total_output_tokens": 30,
                    "total_thought_tokens": 15,
                    "total_tokens": 145
                }),
            );
            if configuration.id == GEMINI_3_8_FLASH.id {
                body = body.replace("gemini-4-argon", "gemini-3.8-flash");
            }
            let fixture = Fixture::start(FixtureReply::OwnedSse(body)).await;
            let selection = ModelSelection::new(configuration, "medium").unwrap();

            let (result, _) = step_with(&fixture, KEY, selection, &history("inspect")).await;
            let step = result.unwrap();
            let [AttemptAccounting::KnownUsage(usage)] = step.attempts.as_slice() else {
                panic!("expected known usage");
            };

            assert_eq!(
                usage,
                &ModelUsage {
                    model: configuration.id.to_owned(),
                    service_tier: Some(expected_tier.to_owned()),
                    input_tokens: Some(100),
                    cached_input_tokens: Some(20),
                    cache_write_input_tokens: Some(0),
                    cache_write_1h_input_tokens: 0,
                    output_tokens: Some(45),
                    reasoning_output_tokens: Some(15),
                    total_tokens: Some(145),
                }
            );
            assert_eq!(
                estimate_usage(usage).map(|estimate| estimate.amount().as_nanos()),
                expected_nanos
            );
        }

        let no_cached_or_thought = stream(
            vec![model_output(0, "Done.")],
            "completed",
            json!({
                "total_input_tokens": 50,
                "total_output_tokens": 10,
                "total_tokens": 60
            }),
        );
        let fixture = Fixture::start(FixtureReply::OwnedSse(no_cached_or_thought)).await;
        let step_result = step(&fixture, &history("inspect")).await.0.unwrap();
        let [AttemptAccounting::KnownUsage(usage)] = step_result.attempts.as_slice() else {
            panic!("expected known usage");
        };
        assert_eq!(usage.cached_input_tokens, Some(0));
        assert_eq!(usage.reasoning_output_tokens, Some(0));
        assert_eq!(usage.output_tokens, Some(10));
        assert_eq!(
            estimate_usage(usage).unwrap().amount().as_nanos(),
            50 * 2_000 + 10 * 10_000
        );

        let missing_fields = stream(
            vec![model_output(0, "Done.")],
            "completed",
            json!({"total_thought_tokens": 5}),
        );
        let fixture = Fixture::start(FixtureReply::OwnedSse(missing_fields)).await;
        let step_result = step(&fixture, &history("inspect")).await.0.unwrap();
        let [AttemptAccounting::KnownUsage(usage)] = step_result.attempts.as_slice() else {
            panic!("expected known usage");
        };
        assert_eq!(usage.input_tokens, None);
        assert_eq!(usage.output_tokens, None);
        assert_eq!(usage.reasoning_output_tokens, Some(5));
        assert_eq!(usage.total_tokens, None);
        assert!(matches!(
            estimate_usage(usage),
            Err(PricingError::MissingCategory(_))
        ));
    }

    #[tokio::test]
    async fn stream_validates_echoed_model_and_framing_order() {
        let created = interaction_created();
        let mut wrong_created = created.clone();
        wrong_created["interaction"]["model"] = json!("gemini-3.8-flash");
        let mut wrong_status = status_update("in_progress");
        wrong_status["model"] = json!("gemini-3.8-flash");
        let completed = json!({
            "event_type": "interaction.completed",
            "interaction": {
                "id": "int_1",
                "model": "gemini-4-argon",
                "status": "completed",
                "usage": final_usage()
            }
        });
        let with_content = |prefix: Vec<Value>| -> Vec<Value> {
            prefix
                .into_iter()
                .chain(model_output(0, "Done."))
                .chain([completed.clone()])
                .collect()
        };

        for (events, expected) in [
            (
                with_content(vec![]),
                "stream content arrived before interaction.created",
            ),
            (
                vec![completed.clone()],
                "stream content arrived before interaction.created",
            ),
            (
                with_content(vec![created.clone(), created.clone()]),
                "stream repeated interaction.created",
            ),
            (
                with_content(vec![wrong_created]),
                "interaction did not echo the selected model",
            ),
            (
                with_content(vec![created.clone(), wrong_status]),
                "interaction did not echo the selected model",
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
    async fn stream_rejects_invalid_step_framing_and_restarts_early_done() {
        let created = interaction_created();
        let completed = json!({
            "event_type": "interaction.completed",
            "interaction": {
                "id": "int_1",
                "model": "gemini-4-argon",
                "status": "completed",
                "usage": final_usage()
            }
        });

        for (events, expected) in [
            (
                vec![
                    created.clone(),
                    step_start(0, json!({"type": "model_output"})),
                    step_start(1, json!({"type": "model_output"})),
                ],
                "stream steps were out of order",
            ),
            (
                vec![
                    created.clone(),
                    step_start(3, json!({"type": "thought"})),
                    step_stop(3),
                    step_start(3, json!({"type": "model_output"})),
                ],
                "stream steps were out of order",
            ),
            (
                vec![
                    created.clone(),
                    step_start(3, json!({"type": "thought"})),
                    step_stop(3),
                    step_start(2, json!({"type": "model_output"})),
                ],
                "stream steps were out of order",
            ),
            (
                vec![
                    created.clone(),
                    step_delta(0, json!({"type": "text", "text": "Done."})),
                ],
                "stream delta did not match an open step",
            ),
            (
                vec![
                    created.clone(),
                    step_start(3, json!({"type": "model_output"})),
                    step_delta(4, json!({"type": "text", "text": "Done."})),
                ],
                "stream delta did not match an open step",
            ),
            (
                vec![
                    created.clone(),
                    step_start(0, json!({"type": "model_output"})),
                    step_delta(
                        0,
                        json!({"type": "arguments_delta", "arguments": r#"{"code":"1"}"#}),
                    ),
                ],
                "stream delta did not match its step",
            ),
            (
                vec![created.clone(), step_stop(0)],
                "stream stop did not match an open step",
            ),
            (
                vec![
                    created.clone(),
                    step_start(3, json!({"type": "model_output"})),
                    step_stop(4),
                ],
                "stream stop did not match an open step",
            ),
            (
                vec![
                    created.clone(),
                    step_start(0, json!({"type": "model_output"})),
                    completed.clone(),
                ],
                "stream stopped inside a step",
            ),
            (
                vec![
                    created.clone(),
                    step_start(0, json!({"type": "model_output"})),
                    step_delta(0, json!({"type": "text", "text": "Done."})),
                    step_stop(0),
                    completed.clone(),
                    status_update("completed"),
                ],
                "stream continued after interaction.completed",
            ),
            (
                vec![
                    created.clone(),
                    step_start(
                        0,
                        json!({
                            "type": "code_execution_call",
                            "id": "code_1",
                            "arguments": {"code": "print(1)", "language": "python"}
                        }),
                    ),
                ],
                "stream contained an unexpected step",
            ),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(sse(events))).await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();

            assert_eq!(failure.error(), &ModelError::InvalidResponse(expected));
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
            assert_eq!(fixture.request_count(), 1);
        }

        let early_done = format!(
            "{}event: done\ndata: [DONE]\n\n",
            sse([
                created,
                step_start(0, json!({"type": "model_output"})),
                step_delta(0, json!({"type": "text", "text": "stale"})),
            ])
        );
        let restart_fixture = Fixture::start_sequence(vec![
            FixtureReply::OwnedSse(early_done),
            FixtureReply::OwnedSse(message_stream()),
        ])
        .await;

        let (result, progress) = step(&restart_fixture, &history("inspect")).await;
        let step = result.unwrap();

        assert_same_requests(&restart_fixture, 2);
        assert_eq!(starts(&progress), [1, 2]);
        assert_eq!(settles(&progress), [1, 2]);
        assert_eq!(
            step.action,
            ModelAction::Message {
                text: "Done.".to_owned()
            }
        );
        assert_eq!(
            step.attempts,
            vec![AttemptAccounting::UsageUnavailable, expected_accounting()]
        );
    }

    #[tokio::test]
    async fn incomplete_failed_and_cancelled_statuses_fail_as_provider_failures_with_usage() {
        for (status, message) in [
            ("incomplete", "response incomplete"),
            ("failed", "response failed"),
            ("cancelled", "response cancelled"),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream(
                vec![model_output(0, "partial")],
                status,
                final_usage(),
            )))
            .await;

            let (result, progress) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();
            let provider = failure.provider_failure().expect(status);

            assert_eq!(
                (provider.status(), provider.code(), provider.message()),
                (None, Some(status), message)
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
    async fn invalid_step_combinations_and_duplicate_call_ids_are_invalid_responses() {
        for (steps, status, expected) in [
            (
                vec![thought(0, &["Nothing"], "sig")],
                "completed",
                "assistant message was empty",
            ),
            (
                vec![model_output(0, "")],
                "completed",
                "assistant message was empty",
            ),
            (
                vec![
                    function_call(0, "call_1", &[r#"{"code":"print(1)"}"#]),
                    function_call(1, "call_2", &[r#"{"code":"print(2)"}"#]),
                ],
                "requires_action",
                "response contained multiple tool calls",
            ),
            (
                vec![function_call(0, "call_1", &[r#"{"code":"print(1)"}"#])],
                "completed",
                "interaction status did not match the response content",
            ),
            (
                vec![model_output(0, "Done.")],
                "requires_action",
                "interaction status did not match the response content",
            ),
            (
                vec![function_call(0, "", &[r#"{"code":"print(1)"}"#])],
                "requires_action",
                "tool call identifier was empty",
            ),
        ] {
            let fixture =
                Fixture::start(FixtureReply::OwnedSse(stream(steps, status, final_usage()))).await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();

            assert_eq!(failure.error(), &ModelError::InvalidResponse(expected));
            assert_eq!(failure.attempts(), &[expected_accounting()]);
        }

        for (steps, expected) in [
            (
                vec![vec![
                    step_start(
                        0,
                        json!({"type": "function_call", "id": "call_1", "name": "other_tool"}),
                    ),
                    step_delta(
                        0,
                        json!({"type": "arguments_delta", "arguments": r#"{"code":"print(1)"}"#}),
                    ),
                    step_stop(0),
                ]],
                "response called an unknown tool",
            ),
            (
                vec![function_call(
                    0,
                    "call_1",
                    &[r#"{"code":"print(1)","extra":true}"#],
                )],
                "execute_python arguments were not a closed code object",
            ),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedSse(stream(
                steps,
                "requires_action",
                final_usage(),
            )))
            .await;

            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();

            assert_eq!(failure.error(), &ModelError::InvalidResponse(expected));
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        }

        let fixture = Fixture::start(FixtureReply::OwnedSse(tool_stream())).await;
        let (result, _) = step(&fixture, &history_with_code("inspect")).await;
        assert_eq!(
            result.unwrap_err().error(),
            &ModelError::InvalidResponse("tool call identifier duplicates model history")
        );
    }

    fn api_key_invalid_body(message: &str) -> String {
        json!({
            "error": {
                "code": 400,
                "message": message,
                "status": "INVALID_ARGUMENT",
                "details": [{
                    "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                    "reason": "API_KEY_INVALID",
                    "domain": "googleapis.com"
                }]
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn http_401_and_http_400_api_key_invalid_are_authentication_failures_on_step_and_summary()
    {
        for (code, body, expected_code, expected_message, expected_auth) in [
            (
                401,
                json!({
                    "error": {
                        "code": 401,
                        "message": "Request had invalid authentication credentials.",
                        "status": "UNAUTHENTICATED"
                    }
                })
                .to_string(),
                Some("UNAUTHENTICATED"),
                "Request had invalid authentication credentials.",
                true,
            ),
            (
                400,
                api_key_invalid_body("API key not valid. Please pass a valid API key."),
                Some("INVALID_ARGUMENT"),
                "API key not valid. Please pass a valid API key.",
                true,
            ),
            (
                400,
                json!({
                    "error": {
                        "code": 400,
                        "message": "Invalid thinking_level",
                        "status": "INVALID_ARGUMENT"
                    }
                })
                .to_string(),
                Some("INVALID_ARGUMENT"),
                "Invalid thinking_level",
                false,
            ),
        ] {
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code,
                headers: vec![("retry-after-ms".to_owned(), "0".to_owned())],
                body: body.clone(),
            })
            .await;

            let (result, progress) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();
            let provider = failure.provider_failure().unwrap();

            assert_eq!(failure.is_authentication(), expected_auth, "HTTP {code}");
            assert_eq!(
                (provider.status(), provider.code(), provider.message()),
                (Some(code), expected_code, expected_message)
            );
            assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
            assert_eq!(starts(&progress), [1]);
            assert_eq!(fixture.request_count(), 1);

            let summary_fixture = Fixture::start(FixtureReply::OwnedStatus {
                code,
                headers: vec![("retry-after-ms".to_owned(), "0".to_owned())],
                body,
            })
            .await;
            let (summary_result, attempts_started) = summarise(&summary_fixture).await;
            let summary_failure = summary_result.unwrap_err();
            assert_eq!(
                summary_failure.is_authentication(),
                expected_auth,
                "summary HTTP {code}"
            );
            assert_eq!(attempts_started, 1);
            assert_eq!(summary_fixture.request_count(), 1);
        }
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

    fn status_reply(code: u16, headers: &[(&str, &str)]) -> FixtureReply {
        FixtureReply::OwnedStatus {
            code,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body: json!({
                "error": {
                    "code": code,
                    "message": "try again",
                    "status": "UNAVAILABLE"
                }
            })
            .to_string(),
        }
    }

    fn assert_same_requests(fixture: &Fixture, count: usize) {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), count);
        let expected = expected_request(json!([user_step("inspect")]));
        assert!(requests.iter().all(|request| request.body == expected));
    }

    fn interrupted_stream() -> String {
        sse([interaction_created()]
            .into_iter()
            .chain(thought(0, &["stale thought"], "stale-sig"))
            .chain(model_output(1, "stale text"))
            .chain([
                step_start(
                    2,
                    json!({"type": "function_call", "id": "call_stale", "name": "execute_python"}),
                ),
                step_delta(
                    2,
                    json!({"type": "arguments_delta", "arguments": r#"{"code":"stale"#}),
                ),
            ]))
    }

    fn retryable_error_stream(code: &str) -> String {
        format!(
            "{}{}",
            interrupted_stream(),
            sse([json!({
                "event_type": "error",
                "error": {"code": code, "message": "Transient stream failure"}
            })])
        )
    }

    #[tokio::test]
    async fn retryable_statuses_are_retried_within_one_attempt_with_the_same_request() {
        for code in [408, 429, 500, 503] {
            let fixture = Fixture::start_sequence(vec![
                status_reply(code, &[("retry-after-ms", "0"), ("retry-after", "30")]),
                FixtureReply::OwnedSse(tool_stream()),
            ])
            .await;

            let (result, progress) = step(&fixture, &history("inspect")).await;
            let step = result.unwrap();

            assert_same_requests(&fixture, 2);
            assert_eq!(starts(&progress), [1], "HTTP {code}");
            assert_eq!(settles(&progress), [1], "HTTP {code}");
            assert_eq!(step.attempts, vec![retried_accounting()], "HTTP {code}");
            assert!(matches!(
                estimate_run_cost(&step.attempts),
                RunCostEstimate::LowerBound(_)
            ));
        }
    }

    #[tokio::test]
    async fn exhausted_http_retries_fail_the_attempt_without_a_stream_restart() {
        let fixture = Fixture::start(status_reply(503, &[("retry-after-ms", "0")])).await;

        let (result, progress) = step(&fixture, &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();

        assert_same_requests(&fixture, 3);
        assert_eq!(starts(&progress), [1]);
        assert_eq!(settles(&progress), [1]);
        assert_eq!(
            (provider.status(), provider.code(), provider.message()),
            (Some(503), Some("UNAVAILABLE"), "try again")
        );
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
    }

    #[tokio::test]
    async fn interrupted_streams_and_retryable_sse_errors_restart_and_discard_partial_output() {
        for first in [
            FixtureReply::CloseBeforeHeaders,
            FixtureReply::AbruptSse(IDLE_STREAM),
            FixtureReply::OwnedSse(interrupted_stream()),
            FixtureReply::OwnedSse(retryable_error_stream("overloaded")),
            FixtureReply::OwnedSse(retryable_error_stream("unavailable")),
            FixtureReply::OwnedSse(retryable_error_stream("internal")),
            FixtureReply::OwnedSse(retryable_error_stream("resource_exhausted")),
        ] {
            let fixture =
                Fixture::start_sequence(vec![first, FixtureReply::OwnedSse(tool_stream())]).await;

            let (result, progress) = step(&fixture, &history("inspect")).await;
            let step = result.unwrap();

            assert_same_requests(&fixture, 2);
            assert_eq!(starts(&progress), [1, 2]);
            assert_eq!(settles(&progress), [1, 2]);
            let restart = progress
                .iter()
                .position(|event| event == &ModelProgress::AttemptStarted { attempt: 2 })
                .unwrap();
            assert_eq!(
                streamed(&progress[restart..]),
                (
                    "Checking the file".to_owned(),
                    "I'll run it.".to_owned(),
                    "print(1)".to_owned()
                )
            );
            assert_eq!(
                step.attempts,
                vec![AttemptAccounting::UsageUnavailable, expected_accounting()]
            );
        }
    }

    #[tokio::test]
    async fn stream_restarts_stop_after_three_attempts_and_terminal_sse_errors_do_not_restart() {
        let fixture =
            Fixture::start(FixtureReply::OwnedSse(retryable_error_stream("overloaded"))).await;
        let (result, progress) = step(&fixture, &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();
        assert_same_requests(&fixture, 3);
        assert_eq!(starts(&progress), [1, 2, 3]);
        assert_eq!(settles(&progress), [1, 2, 3]);
        assert_eq!(
            (provider.status(), provider.code(), provider.message()),
            (None, Some("overloaded"), "Transient stream failure")
        );
        assert_eq!(
            failure.attempts(),
            vec![AttemptAccounting::UsageUnavailable; 3]
        );

        let terminal_fixture = Fixture::start(FixtureReply::OwnedSse(sse([json!({
            "event_type": "error",
            "error": {"code": "invalid_argument", "message": "Bad input"}
        })])))
        .await;
        let (result, _) = step(&terminal_fixture, &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();
        assert_eq!(
            (provider.status(), provider.code(), provider.message()),
            (None, Some("invalid_argument"), "Bad input")
        );
        assert_eq!(terminal_fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn redirects_are_provider_failures_and_never_resend_the_request() {
        for code in [307, 308] {
            let target = Fixture::start(FixtureReply::OwnedSse(message_stream())).await;
            let fixture = Fixture::start(FixtureReply::OwnedStatus {
                code,
                headers: vec![(
                    "location".to_owned(),
                    format!("{}/v1beta/interactions?alt=sse", target.endpoint),
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
    async fn no_google_failure_is_classified_as_context_overflow() {
        let error = |code: u16, status: &str, message: &str| FixtureReply::OwnedStatus {
            code,
            headers: vec![("retry-after-ms".to_owned(), "0".to_owned())],
            body: json!({"error": {"code": code, "status": status, "message": message}})
                .to_string(),
        };
        let prompt_too_long = "input token count (1200000) exceeds maximum context length";
        for (reply, expected) in [
            (
                error(400, "INVALID_ARGUMENT", prompt_too_long),
                (Some(400), Some("INVALID_ARGUMENT")),
            ),
            (
                error(
                    413,
                    "INVALID_ARGUMENT",
                    "Request payload size exceeds the limit.",
                ),
                (Some(413), Some("INVALID_ARGUMENT")),
            ),
            (
                error(429, "RESOURCE_EXHAUSTED", "Quota exceeded"),
                (Some(429), Some("RESOURCE_EXHAUSTED")),
            ),
            (
                error(503, "UNAVAILABLE", "Service unavailable"),
                (Some(503), Some("UNAVAILABLE")),
            ),
            (
                FixtureReply::OwnedSse(retryable_error_stream("resource_exhausted")),
                (None, Some("resource_exhausted")),
            ),
            (
                FixtureReply::OwnedSse(stream(
                    vec![model_output(0, "partial")],
                    "incomplete",
                    final_usage(),
                )),
                (None, Some("incomplete")),
            ),
        ] {
            let fixture = Fixture::start(reply).await;
            let (result, _) = step(&fixture, &history("inspect")).await;
            let failure = result.unwrap_err();
            let provider = failure.provider_failure().unwrap();
            assert_eq!((provider.status(), provider.code()), expected);
            assert!(!failure.is_context_overflow());
        }
    }

    #[tokio::test]
    async fn hostile_http_and_sse_errors_redact_the_key_before_bounding() {
        let prefix = "x".repeat(PROVIDER_MESSAGE_LIMIT - 6);
        let body = json!({
            "error": {
                "code": 400,
                "status": format!("{HOSTILE_KEY}{}", "é".repeat(PROVIDER_CODE_LIMIT)),
                "message": format!("{prefix}{HOSTILE_KEY}"),
                "details": [{"reason": "API_KEY_INVALID", "metadata": METADATA}]
            }
        })
        .to_string();
        let fixture = Fixture::start(FixtureReply::OwnedStatus {
            code: 400,
            headers: vec![("x-request-id".to_owned(), METADATA.to_owned())],
            body,
        })
        .await;

        let (result, _) = step_with(&fixture, HOSTILE_KEY, selection(), &history("inspect")).await;
        let failure = result.unwrap_err();
        let provider = failure.provider_failure().unwrap();

        assert_sanitised(&failure);
        assert!(failure.is_authentication());
        assert_eq!(provider.status(), Some(400));
        assert_eq!(provider.message(), format!("{prefix}[REDAC"));
        assert_eq!(
            provider.code(),
            Some(format!("[REDACTED]{}", "é".repeat((PROVIDER_CODE_LIMIT - 10) / 2)).as_str())
        );

        let sse_fixture = Fixture::start(FixtureReply::OwnedSse(sse([
            interaction_created(),
            json!({
                "event_type": "error",
                "error": {
                    "code": format!("invalid_argument {HOSTILE_KEY}"),
                    "message": format!("{HOSTILE_KEY} {}", "é".repeat(PROVIDER_MESSAGE_LIMIT))
                }
            }),
        ])))
        .await;
        let (sse_result, _) =
            step_with(&sse_fixture, HOSTILE_KEY, selection(), &history("inspect")).await;
        let sse_failure = sse_result.unwrap_err();
        assert_sanitised(&sse_failure);
    }

    fn budget_stream(thinking: usize, signature: usize, text: usize, code: usize) -> String {
        let x = |bytes: usize| "x".repeat(bytes);
        stream(
            vec![
                thought(0, &[&x(thinking)], &x(signature)),
                model_output(1, &x(text)),
                function_call(2, "call_1", &[r#"{"code":""#, &x(code), r#""}"#]),
            ],
            "requires_action",
            final_usage(),
        )
    }

    #[tokio::test]
    async fn content_budget_accepts_at_limit_rejects_over_limit_and_resets_on_restart() {
        let quarter = ACCEPTED_MODEL_CONTENT_LIMIT / 4;
        let at_limit = Fixture::start(FixtureReply::OwnedSse(budget_stream(
            quarter,
            quarter,
            quarter,
            quarter - TOOL_OVERHEAD,
        )))
        .await;
        let step_ok = step(&at_limit, &history("inspect")).await.0.unwrap();
        assert!(matches!(
            step_ok.action,
            ModelAction::Code { source, .. } if source.len() == quarter - TOOL_OVERHEAD
        ));

        let over_limit = Fixture::start(FixtureReply::OwnedSse(budget_stream(
            quarter,
            quarter,
            quarter,
            quarter - TOOL_OVERHEAD + 1,
        )))
        .await;
        let failure = step(&over_limit, &history("inspect")).await.0.unwrap_err();
        assert_eq!(failure.error(), &ModelError::ContentTooLarge);
        assert_eq!(failure.attempts(), &[AttemptAccounting::UsageUnavailable]);
        assert_eq!(over_limit.request_count(), 1);

        let dropped_at_limit = sse([
            interaction_created(),
            step_start(0, json!({"type": "model_output"})),
            step_delta(
                0,
                json!({"type": "text", "text": "x".repeat(ACCEPTED_MODEL_CONTENT_LIMIT)}),
            ),
        ]);
        let restart_fixture = Fixture::start_sequence(vec![
            FixtureReply::OwnedSse(dropped_at_limit),
            FixtureReply::OwnedSse(message_stream()),
        ])
        .await;
        let restarted = step(&restart_fixture, &history("inspect")).await.0.unwrap();
        assert_eq!(
            restarted.action,
            ModelAction::Message {
                text: "Done.".to_owned()
            }
        );
        assert_eq!(
            restarted.attempts,
            vec![AttemptAccounting::UsageUnavailable, expected_accounting()]
        );
    }

    const SMALL_FRAME_LIMIT: usize = 1024;

    fn ignored_frame(padding: &str) -> String {
        sse([json!({"event_type": "future_event", "padding": padding})])
    }

    fn error_frame(message: &str) -> String {
        sse([json!({
            "event_type": "error",
            "error": {"code": "overloaded", "message": message}
        })])
    }

    fn padded(frame: fn(&str) -> String, frame_bytes: usize) -> String {
        let framing = frame("").len() - 1;
        frame(&"x".repeat(frame_bytes - framing))
    }

    #[tokio::test]
    async fn stream_enforces_the_sse_frame_limit_and_normalises_cr_lines() {
        let start = sse([interaction_created()]);
        let rest = format!(
            "{}event: done\ndata: [DONE]\n\n",
            sse(model_output(0, "Done.").into_iter().chain([json!({
                "event_type": "interaction.completed",
                "interaction": {
                    "id": "int_1",
                    "model": "gemini-4-argon",
                    "status": "completed",
                    "usage": final_usage()
                }
            })]))
        );
        let at_limit = padded(ignored_frame, SMALL_FRAME_LIMIT);
        let fixture =
            Fixture::start(FixtureReply::OwnedSse(format!("{start}{at_limit}{rest}"))).await;
        let mut ok_model = model(&fixture, KEY);
        ok_model.frame_limit = SMALL_FRAME_LIMIT;
        assert_eq!(
            ok_model
                .step(
                    selection(),
                    &history("inspect"),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await
                .unwrap()
                .action,
            ModelAction::Message {
                text: "Done.".to_owned()
            }
        );

        for frame in [ignored_frame, error_frame] {
            let over = padded(frame, SMALL_FRAME_LIMIT + 1);
            let fixture =
                Fixture::start(FixtureReply::OwnedSse(format!("{start}{over}{rest}"))).await;
            let mut over_model = model(&fixture, KEY);
            over_model.frame_limit = SMALL_FRAME_LIMIT;
            let failure = over_model
                .step(
                    selection(),
                    &history("inspect"),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .await
                .unwrap_err();
            assert_eq!(failure.error(), &ModelError::ContentTooLarge);
            assert_eq!(fixture.request_count(), 1);
        }

        let cr_body = stream(
            vec![model_output(0, "one\r\ntwo")],
            "completed",
            final_usage(),
        )
        .replace('\n', "\r");
        let cr_fixture = Fixture::start(FixtureReply::OwnedSse(cr_body)).await;
        assert_eq!(
            step(&cr_fixture, &history("inspect"))
                .await
                .0
                .unwrap()
                .action,
            ModelAction::Message {
                text: "one\r\ntwo".to_owned()
            }
        );
        assert_eq!(
            GoogleModel::new("system prompt", KEY, CrossProviderReasoning::Inline).frame_limit,
            SSE_FRAME_LIMIT
        );
    }

    async fn cancel_once_waiting(
        fixture: &Fixture,
        wait: &str,
    ) -> (ModelFailure, Vec<ModelProgress>) {
        let mut model = model(fixture, KEY);
        let wait_entered = model.wait_entered.clone();
        let cancellation = CancellationToken::new();
        let history = history("inspect");
        let mut progress = Vec::new();

        let (result, ()) = {
            let mut sink = |event| progress.push(event);
            tokio::join!(
                tokio::time::timeout(
                    Duration::from_secs(5),
                    model.step(selection(), &history, &cancellation, &mut sink),
                ),
                async {
                    tokio::time::timeout(Duration::from_secs(1), wait_entered.notified())
                        .await
                        .unwrap_or_else(|_| panic!("model should enter the {wait}"));
                    cancellation.cancel();
                }
            )
        };
        let failure = result
            .expect("cancellation should end the wait")
            .unwrap_err();
        (failure, progress)
    }

    #[tokio::test]
    async fn cancellation_stops_active_stream_http_retry_wait_and_restart_delay() {
        let idle = Fixture::start(FixtureReply::IdleSse(IDLE_STREAM)).await;
        let mut idle_model = model(&idle, KEY);
        let cancellation = CancellationToken::new();
        let hist = history("inspect");
        let mut sink = |_| {};
        let (result, ()) = tokio::join!(
            idle_model.step(selection(), &hist, &cancellation, &mut sink),
            async {
                idle.wait_for_requests(1).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancellation.cancel();
            }
        );
        assert!(result.unwrap_err().is_cancelled());
        idle.wait_for_disconnect().await;

        let retry_fixture = Fixture::start_sequence(vec![
            status_reply(429, &[("retry-after", "30")]),
            FixtureReply::OwnedSse(tool_stream()),
        ])
        .await;
        let (retry_failure, _) = cancel_once_waiting(&retry_fixture, "HTTP retry wait").await;
        assert!(retry_failure.is_cancelled());
        assert_eq!(retry_fixture.request_count(), 1);

        let restart_fixture = Fixture::start_sequence(vec![
            FixtureReply::Sse(""),
            FixtureReply::OwnedSse(tool_stream()),
        ])
        .await;
        let (restart_failure, _) = cancel_once_waiting(&restart_fixture, "restart wait").await;
        assert!(restart_failure.is_cancelled());
        assert_eq!(restart_fixture.request_count(), 1);
    }

    fn summary_request(selection: ModelSelection) -> SummaryRequest {
        SummaryRequest {
            instructions: "summary instructions".to_owned(),
            input: "conversation".to_owned(),
            selection,
            output_limit: 4_096,
        }
    }

    fn expected_summary_request() -> Value {
        json!({
            "model": "gemini-4-argon",
            "service_tier": "standard",
            "store": false,
            "stream": true,
            "system_instruction": "summary instructions",
            "input": [user_step("conversation")],
            "generation_config": {
                "max_output_tokens": 4096,
                "thinking_level": "medium",
                "thinking_summaries": "auto"
            }
        })
    }

    fn summary_stream() -> String {
        stream(
            vec![
                thought(0, &["Reading ", "history"], "sig-3"),
                model_output(1, "Part one. "),
                model_output(2, "Part two."),
            ],
            "completed",
            final_usage(),
        )
    }

    async fn summarise_with(
        fixture: &Fixture,
        key: &str,
        request: SummaryRequest,
    ) -> (Result<SummaryResult, ModelFailure>, usize) {
        let mut attempts_started = 0;
        let result = model(fixture, key)
            .summarise(request, &CancellationToken::new(), &mut || {
                attempts_started += 1
            })
            .await;
        (result, attempts_started)
    }

    async fn summarise(fixture: &Fixture) -> (Result<SummaryResult, ModelFailure>, usize) {
        summarise_with(fixture, KEY, summary_request(selection())).await
    }

    #[tokio::test]
    async fn summary_sends_exact_request_concatenates_text_and_validates_failures() {
        let fixture = Fixture::start(FixtureReply::OwnedSse(summary_stream())).await;
        let (result, attempts_started) = summarise(&fixture).await;
        let summary = result.unwrap();

        let captured = fixture.request();
        assert!(
            captured
                .head
                .starts_with("POST /v1beta/interactions?alt=sse HTTP/1.1\r\n")
        );
        assert_eq!(headers(&captured.head)["x-goog-api-key"], KEY);
        assert_eq!(captured.body, expected_summary_request());
        assert_eq!(summary.text, "Part one. Part two.");
        assert_eq!(summary.attempts, vec![expected_accounting()]);
        assert_eq!(attempts_started, 1);

        let tool_call = || ModelError::InvalidResponse("summary response contained a tool call");
        let empty = || ModelError::InvalidResponse("summary text was empty or whitespace");
        let incomplete = || {
            ModelError::Provider(Box::new(ProviderFailure::new(
                None,
                Some("incomplete".to_owned()),
                "response incomplete".to_owned(),
            )))
        };
        for (steps, status, expected) in [
            (
                vec![
                    model_output(0, "Summary."),
                    function_call(1, "call_1", &[r#"{"code":"print(1)"}"#]),
                ],
                "completed",
                tool_call(),
            ),
            (
                vec![model_output(0, "Summary.")],
                "requires_action",
                tool_call(),
            ),
            (vec![model_output(0, "partial")], "incomplete", incomplete()),
            (vec![thought(0, &["Nothing"], "sig")], "completed", empty()),
            (vec![model_output(0, " \n\t ")], "completed", empty()),
        ] {
            let fixture =
                Fixture::start(FixtureReply::OwnedSse(stream(steps, status, final_usage()))).await;
            let (result, _) = summarise(&fixture).await;
            let failure = result.unwrap_err();
            assert_eq!(failure.error(), &expected);
            assert_eq!(failure.attempts(), &[expected_accounting()]);
        }

        let cancelled_after = CancellationToken::new();
        cancelled_after.cancel();
        let mut progress = Vec::new();
        let failure = model(&fixture, KEY)
            .with_restarts(
                &expected_summary_request(),
                selection(),
                &cancelled_after,
                &mut |event| progress.push(event),
                |_| Ok(()),
            )
            .await
            .unwrap_err();
        assert!(failure.is_cancelled());
        assert!(failure.attempts().is_empty());

        let foreign_selection = ModelSelection::new(&SOL, SOL.default_thinking).unwrap();
        let (foreign_result, foreign_attempts) =
            summarise_with(&fixture, KEY, summary_request(foreign_selection)).await;
        assert_eq!(
            foreign_result.unwrap_err().error(),
            &ModelError::InvalidSelection("unsupported Google model")
        );
        assert_eq!(foreign_attempts, 0);
    }

    #[tokio::test]
    #[ignore = "requires explicit GEMINI_API_KEY, KIKI_MODEL=google/gemini-4-argon and MONTY_BIN"]
    async fn live_argon() {
        if let Err(message) = crate::openai::tests::run_live_smoke_test(&GEMINI_4_ARGON).await {
            panic!("{message}");
        }
    }

    #[tokio::test]
    #[ignore = "requires explicit GEMINI_API_KEY"]
    async fn live_argon_continues_after_compaction_with_retained_signed_thinking() {
        let key = std::env::var("GEMINI_API_KEY").expect("GEMINI_API_KEY");
        let mut model = GoogleModel::new(
            "Use execute_python for every calculation.",
            key,
            CrossProviderReasoning::Inline,
        );
        let selection = ModelSelection::new(&GEMINI_4_ARGON, "high").unwrap();
        let cancellation = CancellationToken::new();

        let first = model
            .step(
                selection,
                &history("Think it through, then call execute_python to compute 17 * 23."),
                &cancellation,
                &mut |_| {},
            )
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
        assert!(
            replay_values(&first.replay)
                .iter()
                .any(|step| step["type"] == "thought"
                    && step["signature"]
                        .as_str()
                        .is_some_and(|signature| !signature.is_empty())),
            "no signed thinking to retain"
        );
        let summary = model
            .summarise(
                SummaryRequest {
                    instructions: "Summarise the conversation in one sentence.".to_owned(),
                    input: "User: compute 17 * 23 with execute_python.".to_owned(),
                    selection,
                    output_limit: 16_384,
                },
                &cancellation,
                &mut || {},
            )
            .await
            .unwrap();
        let mut compacted = ModelHistory::new();
        compacted.append_compaction_summary(summary.text);
        compacted
            .append_completed_code(
                source,
                call_id,
                commentary,
                first.reasoning_summary,
                first.replay,
                CodeResult::execution("391\n".to_owned()),
            )
            .unwrap();

        model
            .step(selection, &compacted, &cancellation, &mut |_| {})
            .await
            .unwrap();
    }
}
