use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::{
    ModelSelection,
    constants::{PROVIDER_CODE_LIMIT, PROVIDER_MESSAGE_LIMIT},
    settings::CrossProviderReasoning,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptedAction {
    Code { source: String },
    Message { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAction {
    Code {
        source: String,
        call_id: ModelCallId,
        commentary: Option<String>,
    },
    Message {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStep {
    pub action: ModelAction,
    pub reasoning_summary: String,
    pub replay: ProviderReplay,
    pub attempts: Vec<AttemptAccounting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRequest {
    pub instructions: String,
    pub input: String,
    pub selection: ModelSelection,
    pub output_limit: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryResult {
    pub text: String,
    pub attempts: Vec<AttemptAccounting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    pub model: String,
    pub service_tier: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub cache_write_1h_input_tokens: u64,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptAccounting {
    KnownUsage(ModelUsage),
    KnownUsageWithUntrackedRetries(ModelUsage),
    UsageUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelCallId(String);

impl ModelCallId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelCallId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueReplayItem(Vec<u8>);

impl OpaqueReplayItem {
    pub fn new(value: Vec<u8>) -> Self {
        Self(value)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReplay {
    provider: String,
    items: Vec<OpaqueReplayItem>,
}

impl ProviderReplay {
    pub fn new(provider: impl Into<String>, items: Vec<OpaqueReplayItem>) -> Self {
        Self {
            provider: provider.into(),
            items,
        }
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn items(&self) -> &[OpaqueReplayItem] {
        &self.items
    }
}

const CODE_RESULT_FEEDBACK_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeResult {
    Execution { feedback: String },
    Cancelled { feedback: String },
    Failure { feedback: String },
}

impl CodeResult {
    pub(crate) fn execution(feedback: String) -> Self {
        Self::Execution { feedback }.bounded()
    }

    pub(crate) fn cancelled(feedback: String) -> Self {
        Self::Cancelled { feedback }.bounded()
    }

    pub(crate) fn failure(feedback: String) -> Self {
        Self::Failure { feedback }.bounded()
    }

    pub(crate) fn feedback(&self) -> &str {
        match self {
            Self::Execution { feedback }
            | Self::Cancelled { feedback }
            | Self::Failure { feedback } => feedback,
        }
    }

    fn bounded(mut self) -> Self {
        let feedback = match &mut self {
            Self::Execution { feedback }
            | Self::Cancelled { feedback }
            | Self::Failure { feedback } => feedback,
        };
        *feedback = bounded_utf8(std::mem::take(feedback), CODE_RESULT_FEEDBACK_LIMIT);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedCode {
    source: String,
    call_id: ModelCallId,
    commentary: Option<String>,
    reasoning_summary: String,
    replay: ProviderReplay,
    result: CodeResult,
}

impl CompletedCode {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn call_id(&self) -> &ModelCallId {
        &self.call_id
    }

    pub fn commentary(&self) -> Option<&str> {
        self.commentary.as_deref()
    }

    pub fn reasoning_summary(&self) -> &str {
        &self.reasoning_summary
    }

    pub fn replay(&self) -> &ProviderReplay {
        &self.replay
    }

    pub fn result(&self) -> &CodeResult {
        &self.result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelContextEntry {
    UserRequest {
        text: String,
    },
    CompactionSummary {
        summary: String,
    },
    CompletedCode(CompletedCode),
    FinalMessage {
        text: String,
        reasoning_summary: String,
        replay: ProviderReplay,
    },
}

impl ModelContextEntry {
    pub(crate) fn user_request(&self) -> Option<&str> {
        match self {
            Self::UserRequest { text } => Some(text),
            _ => None,
        }
    }

    pub fn completed_code(&self) -> Option<&CompletedCode> {
        match self {
            Self::CompletedCode(completed) => Some(completed),
            _ => None,
        }
    }

    pub fn compaction_summary(&self) -> Option<&str> {
        match self {
            Self::CompactionSummary { summary } => Some(summary),
            _ => None,
        }
    }

    pub(crate) fn final_message(&self) -> Option<(&str, &str, &ProviderReplay)> {
        match self {
            Self::FinalMessage {
                text,
                reasoning_summary,
                replay,
            } => Some((text, reasoning_summary, replay)),
            _ => None,
        }
    }

    pub(crate) fn foreign_parts(
        &self,
        provider: &str,
        reasoning: CrossProviderReasoning,
    ) -> Option<Vec<ForeignPart<'_>>> {
        let (replay, reasoning_summary, commentary, action) = match self {
            Self::CompletedCode(completed) => (
                completed.replay(),
                completed.reasoning_summary(),
                completed.commentary(),
                ForeignPart::ToolCall {
                    call_id: completed.call_id().as_str(),
                    source: completed.source(),
                    output: completed.result().feedback(),
                },
            ),
            Self::FinalMessage {
                text,
                reasoning_summary,
                replay,
            } => (
                replay,
                reasoning_summary.as_str(),
                None,
                ForeignPart::Text(text),
            ),
            Self::UserRequest { .. } | Self::CompactionSummary { .. } => return None,
        };
        if replay.provider() == provider {
            return None;
        }
        let reasoning_summary =
            (reasoning == CrossProviderReasoning::Inline).then_some(reasoning_summary);
        Some(
            [reasoning_summary, commentary]
                .into_iter()
                .flatten()
                .filter(|text| !text.is_empty())
                .map(ForeignPart::Text)
                .chain([action])
                .collect(),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForeignPart<'a> {
    Text(&'a str),
    ToolCall {
        call_id: &'a str,
        source: &'a str,
        output: &'a str,
    },
}

pub(crate) fn canonical_compaction_summary(summary: &str) -> String {
    format!(
        "The conversation history before this point was compacted into the following summary.\n\nThe Python Arena and its existing variables, functions, classes, and data persisted unchanged across compaction. Treat Arena details marked unverified as historical evidence and inspect them directly before relying on them.\n\n<summary>\n{summary}\n</summary>"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelHistory {
    entries: Vec<ModelContextEntry>,
}

impl Default for ModelHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelHistory {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn entries(&self) -> &[ModelContextEntry] {
        &self.entries
    }

    pub fn append_user_request(&mut self, text: impl Into<String>) {
        self.entries
            .push(ModelContextEntry::UserRequest { text: text.into() });
    }

    pub fn append_compaction_summary(&mut self, summary: impl Into<String>) {
        self.entries.push(ModelContextEntry::CompactionSummary {
            summary: summary.into(),
        });
    }

    pub fn append_completed_code(
        &mut self,
        source: String,
        call_id: ModelCallId,
        commentary: Option<String>,
        reasoning_summary: String,
        replay: ProviderReplay,
        result: CodeResult,
    ) -> Result<(), ModelHistoryError> {
        if self.contains_call_id(&call_id) {
            return Err(ModelHistoryError::DuplicateCallId(call_id));
        }
        self.entries
            .push(ModelContextEntry::CompletedCode(CompletedCode {
                source,
                call_id,
                commentary,
                reasoning_summary,
                replay,
                result: result.bounded(),
            }));
        Ok(())
    }

    pub fn append_final_message(
        &mut self,
        text: String,
        reasoning_summary: String,
        replay: ProviderReplay,
    ) {
        self.entries.push(ModelContextEntry::FinalMessage {
            text,
            reasoning_summary,
            replay,
        });
    }

    pub(crate) fn contains_call_id(&self, call_id: &ModelCallId) -> bool {
        self.entries.iter().any(|entry| {
            entry
                .completed_code()
                .is_some_and(|completed| completed.call_id() == call_id)
        })
    }

    fn user_request_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| matches!(entry, ModelContextEntry::UserRequest { .. }))
            .count()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelHistoryError {
    #[error("model call identifier `{0}` already exists in history")]
    DuplicateCallId(ModelCallId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderFailure {
    status: Option<u16>,
    code: Option<String>,
    message: String,
}

impl ProviderFailure {
    pub(crate) fn new(status: Option<u16>, code: Option<String>, message: String) -> Self {
        Self {
            status,
            code: code.map(|value| bounded_utf8(value, PROVIDER_CODE_LIMIT)),
            message: bounded_utf8(message, PROVIDER_MESSAGE_LIMIT),
        }
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_authentication(&self) -> bool {
        self.status == Some(401)
    }
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_authentication() {
            formatter.write_str("authentication failure")?;
        } else {
            formatter.write_str("provider failure")?;
        }
        if let Some(status) = self.status {
            write!(formatter, " (HTTP {status})")?;
        }
        if let Some(code) = &self.code {
            write!(formatter, " [{code}]")?;
        }
        write!(formatter, ": {}", self.message)
    }
}

fn bounded_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("model step was cancelled")]
    Cancelled,
    #[error("model does not support summarisation")]
    UnsupportedSummarisation,
    #[error("scripted model exhausted before returning a final message")]
    Exhausted,
    #[error("model provider failed: {0}")]
    Provider(Box<ProviderFailure>),
    #[error("model selection is invalid: {0}")]
    InvalidSelection(&'static str),
    #[error("model request history is invalid: {0}")]
    InvalidHistory(&'static str),
    #[error("model response violated the action protocol: {0}")]
    InvalidResponse(&'static str),
    #[error("model response application content exceeded 16 MiB")]
    ContentTooLarge,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{error}")]
pub struct ModelFailure {
    #[source]
    error: ModelError,
    attempts: Vec<AttemptAccounting>,
}

impl ModelFailure {
    pub fn new(error: ModelError, attempts: Vec<AttemptAccounting>) -> Self {
        Self { error, attempts }
    }

    pub fn error(&self) -> &ModelError {
        &self.error
    }

    pub fn attempts(&self) -> &[AttemptAccounting] {
        &self.attempts
    }

    pub fn provider_failure(&self) -> Option<&ProviderFailure> {
        match &self.error {
            ModelError::Provider(failure) => Some(failure),
            _ => None,
        }
    }

    pub fn is_authentication(&self) -> bool {
        self.provider_failure()
            .is_some_and(ProviderFailure::is_authentication)
    }

    pub fn is_cancelled(&self) -> bool {
        self.error == ModelError::Cancelled
    }
}

impl From<ModelError> for ModelFailure {
    fn from(error: ModelError) -> Self {
        Self::new(error, Vec::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelProgress {
    AttemptStarted {
        attempt: u32,
    },
    ReasoningSummaryChunk {
        text: String,
    },
    AssistantTextChunk {
        text: String,
    },
    DecodedPythonChunk {
        code: String,
    },
    AttemptSettled {
        attempt: u32,
        accounting: AttemptAccounting,
    },
}

pub trait Model: Send + 'static {
    fn validate_history(
        &self,
        _history: &ModelHistory,
        _selection: ModelSelection,
    ) -> Result<(), ModelError> {
        Ok(())
    }

    #[allow(clippy::manual_async_fn)]
    fn summarise<'a>(
        &'a mut self,
        _request: SummaryRequest,
        _cancellation: &'a CancellationToken,
        _attempt_started: &'a mut (dyn FnMut() + Send),
    ) -> impl Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a {
        async { Err(ModelError::UnsupportedSummarisation.into()) }
    }

    fn step<'a>(
        &'a mut self,
        selection: ModelSelection,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a;
}

pub struct ScriptedModel {
    actions: Vec<ScriptedAction>,
    subsequent_first_action: Option<ScriptedAction>,
    cursor: usize,
    observed_user_requests: usize,
    summary_responses: Vec<String>,
    summary_cursor: usize,
    delay: Duration,
    #[cfg(test)]
    contexts: Vec<Vec<ModelContextEntry>>,
    #[cfg(test)]
    summary_requests: Vec<SummaryRequest>,
}

impl ScriptedModel {
    pub fn new(actions: Vec<ScriptedAction>) -> Self {
        Self {
            actions,
            subsequent_first_action: None,
            cursor: 0,
            observed_user_requests: 0,
            summary_responses: Vec::new(),
            summary_cursor: 0,
            delay: Duration::ZERO,
            #[cfg(test)]
            contexts: Vec::new(),
            #[cfg(test)]
            summary_requests: Vec::new(),
        }
    }

    pub(crate) fn with_subsequent_first_action(mut self, action: ScriptedAction) -> Self {
        self.subsequent_first_action = Some(action);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_summary_responses(mut self, responses: Vec<String>) -> Self {
        self.summary_responses = responses;
        self
    }

    #[cfg(test)]
    pub(crate) fn contexts(&self) -> &[Vec<ModelContextEntry>] {
        &self.contexts
    }

    #[cfg(test)]
    pub(crate) fn summary_requests(&self) -> &[SummaryRequest] {
        &self.summary_requests
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> usize {
        self.cursor
    }
}

impl Model for ScriptedModel {
    #[allow(clippy::manual_async_fn)]
    fn summarise<'a>(
        &'a mut self,
        _request: SummaryRequest,
        cancellation: &'a CancellationToken,
        attempt_started: &'a mut (dyn FnMut() + Send),
    ) -> impl Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a {
        async move {
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled.into());
            }
            let text = self
                .summary_responses
                .get(self.summary_cursor)
                .cloned()
                .ok_or(ModelError::Exhausted)?;
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled.into());
            }
            attempt_started();
            #[cfg(test)]
            self.summary_requests.push(_request);
            self.summary_cursor += 1;
            Ok(SummaryResult {
                text,
                attempts: Vec::new(),
            })
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn step<'a>(
        &'a mut self,
        _selection: ModelSelection,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a {
        async move {
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled.into());
            }

            let user_requests = history.user_request_count();
            if user_requests > self.observed_user_requests {
                self.cursor = 0;
                self.observed_user_requests = user_requests;
            }
            #[cfg(test)]
            self.contexts.push(history.entries().to_vec());

            let later_action = if user_requests > 1 && self.cursor == 0 {
                self.subsequent_first_action.as_ref()
            } else {
                None
            };
            let scripted_action = later_action
                .or_else(|| self.actions.get(self.cursor))
                .cloned()
                .ok_or(ModelError::Exhausted)?;
            if !self.delay.is_zero() {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(ModelError::Cancelled.into()),
                    _ = tokio::time::sleep(self.delay) => {}
                }
            }
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled.into());
            }

            let action_number = self.cursor + 1;
            self.cursor += 1;
            let replay = ProviderReplay::new(
                "scripted",
                vec![OpaqueReplayItem::new(
                    format!("scripted-{user_requests}-{action_number}").into_bytes(),
                )],
            );
            let action = match scripted_action {
                ScriptedAction::Code { source } => ModelAction::Code {
                    source,
                    call_id: ModelCallId::new(format!("scripted-{user_requests}-{action_number}")),
                    commentary: None,
                },
                ScriptedAction::Message { text } => ModelAction::Message { text },
            };
            Ok(ModelStep {
                action,
                reasoning_summary: String::new(),
                replay,
                attempts: Vec::new(),
            })
        }
    }
}

impl Default for ScriptedModel {
    fn default() -> Self {
        let actions = vec![
            ScriptedAction::Code {
                source: "try:\n    request_count += 1  # ty: ignore[unresolved-reference]\nexcept NameError:\n    request_count = 1\ncontents = await read(\"Cargo.toml\")".to_owned(),
            },
            ScriptedAction::Code {
                source: "print(contents[0:20], end=\"\")\nrequest_count".to_owned(),
            },
            ScriptedAction::Message {
                text: "I inspected Cargo.toml.".to_owned(),
            },
        ];
        Self::new(actions).with_subsequent_first_action(ScriptedAction::Code {
            source: "request_count += 1\ncontents = await read(\"Cargo.toml\")".to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        CodeResult, CompletedCode, ForeignPart, Model, ModelAction, ModelCallId, ModelContextEntry,
        ModelError, ModelFailure, ModelHistory, ModelHistoryError, OpaqueReplayItem,
        ProviderReplay, ScriptedAction, ScriptedModel, SummaryRequest,
        canonical_compaction_summary,
    };
    use crate::config::settings::CrossProviderReasoning;

    fn history_with_request(request: &str) -> ModelHistory {
        let mut history = ModelHistory::new();
        history.append_user_request(request);
        history
    }

    fn replay(value: &str) -> ProviderReplay {
        ProviderReplay::new(
            "test-provider",
            vec![OpaqueReplayItem::new(value.as_bytes().to_vec())],
        )
    }

    fn selection() -> crate::config::ModelSelection {
        crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap()
    }

    fn summary_request(input: &str) -> SummaryRequest {
        SummaryRequest {
            instructions: "summarise".to_owned(),
            input: input.to_owned(),
            selection: selection(),
            output_limit: 123,
        }
    }

    struct StepOnlyModel;

    impl Model for StepOnlyModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: crate::config::ModelSelection,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(super::ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<super::ModelStep, ModelFailure>> + Send + 'a
        {
            async { Err(ModelError::Exhausted.into()) }
        }
    }

    #[tokio::test]
    async fn models_without_summary_support_fail_without_attempts() {
        let mut model = StepOnlyModel;
        let cancellation = CancellationToken::new();

        let mut attempts = 0;
        assert_eq!(
            model
                .summarise(summary_request("conversation"), &cancellation, &mut || {
                    attempts += 1
                },)
                .await,
            Err(ModelFailure::from(ModelError::UnsupportedSummarisation))
        );
        assert_eq!(attempts, 0);
    }

    #[tokio::test]
    async fn scripted_model_records_summary_requests_independently_from_normal_context() {
        let mut model = ScriptedModel::new(vec![ScriptedAction::Message {
            text: "normal result".to_owned(),
        }])
        .with_summary_responses(vec![
            "first summary".to_owned(),
            "second summary".to_owned(),
        ]);
        let history = history_with_request("inspect");
        let cancellation = CancellationToken::new();
        let first = summary_request("first input");
        let second = summary_request("second input");
        let mut progress = |_| {};
        let mut attempts = 0;

        let first_result = model
            .summarise(first.clone(), &cancellation, &mut || attempts += 1)
            .await
            .unwrap();
        assert_eq!(first_result.text, "first summary");
        assert!(first_result.attempts.is_empty());
        assert_eq!(
            model
                .step(selection(), &history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Message {
                text: "normal result".to_owned(),
            }
        );
        assert_eq!(
            model
                .summarise(second.clone(), &cancellation, &mut || attempts += 1)
                .await
                .unwrap()
                .text,
            "second summary"
        );
        assert_eq!(model.summary_requests(), &[first, second]);
        assert_eq!(attempts, 2);
        assert_eq!(model.contexts(), &[history.entries().to_vec()]);
        assert_eq!(model.position(), 1);
    }

    #[test]
    fn compaction_summary_keeps_raw_text_and_its_canonical_wrapper() {
        let mut history = history_with_request("inspect");
        history.append_compaction_summary("saved summary");

        let summary = history.entries().last().unwrap();
        assert_eq!(summary.compaction_summary(), Some("saved summary"));
        assert_eq!(
            canonical_compaction_summary(summary.compaction_summary().unwrap()),
            "The conversation history before this point was compacted into the following summary.\n\nThe Python Arena and its existing variables, functions, classes, and data persisted unchanged across compaction. Treat Arena details marked unverified as historical evidence and inspect them directly before relying on them.\n\n<summary>\nsaved summary\n</summary>"
        );
        assert_eq!(history.user_request_count(), 1);
        let call_id = ModelCallId::new("call");
        history
            .append_completed_code(
                "code".to_owned(),
                call_id.clone(),
                None,
                String::new(),
                replay("opaque"),
                CodeResult::Execution {
                    feedback: "result".to_owned(),
                },
            )
            .unwrap();
        assert!(history.contains_call_id(&call_id));
    }

    #[tokio::test]
    async fn scripted_model_delivers_a_complete_action_after_an_async_delay() {
        let mut model = ScriptedModel::new(vec![ScriptedAction::Code {
            source: "first".to_owned(),
        }])
        .with_delay(Duration::from_millis(50));
        let history = history_with_request("inspect");
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        let mut step = Box::pin(model.step(selection(), &history, &cancellation, &mut progress));

        assert!(timeout(Duration::from_millis(5), &mut step).await.is_err());
        let step = step.await.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Code {
                source: "first".to_owned(),
                call_id: ModelCallId::new("scripted-1-1"),
                commentary: None,
            }
        );
        assert_eq!(step.replay.provider(), "scripted");
        assert_eq!(step.replay.items()[0].as_bytes(), b"scripted-1-1");
        assert_eq!(step.reasoning_summary, "");
        assert!(step.attempts.is_empty());
    }

    #[tokio::test]
    async fn scripted_model_rewinds_only_for_a_new_user_request_and_exhausts_deterministically() {
        let mut model = ScriptedModel::new(vec![
            ScriptedAction::Code {
                source: "first".to_owned(),
            },
            ScriptedAction::Message {
                text: "done".to_owned(),
            },
        ]);
        let mut history = history_with_request("first request");
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};

        assert!(matches!(
            model
                .step(selection(), &history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Code { .. }
        ));
        assert!(matches!(
            model
                .step(selection(), &history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Message { .. }
        ));
        assert_eq!(
            model
                .step(selection(), &history, &cancellation, &mut progress)
                .await,
            Err(ModelFailure::from(ModelError::Exhausted))
        );

        history.append_user_request("second request");

        assert_eq!(
            model
                .step(selection(), &history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Code {
                source: "first".to_owned(),
                call_id: ModelCallId::new("scripted-2-1"),
                commentary: None,
            }
        );
    }

    #[test]
    fn completed_code_history_pairs_one_call_with_one_result_and_rejects_duplicate_ids() {
        let mut history = history_with_request("inspect");
        let call_id = ModelCallId::new("call-1");
        history
            .append_completed_code(
                "print(1)".to_owned(),
                call_id.clone(),
                Some("I’ll inspect first.".to_owned()),
                "checked source".to_owned(),
                replay("opaque-call"),
                CodeResult::Execution {
                    feedback: "1".to_owned(),
                },
            )
            .unwrap();

        let completed = history
            .entries()
            .last()
            .and_then(ModelContextEntry::completed_code)
            .expect("completed code entry");
        assert_eq!(completed.source(), "print(1)");
        assert_eq!(completed.call_id(), &call_id);
        assert_eq!(completed.commentary(), Some("I’ll inspect first."));
        assert_eq!(completed.reasoning_summary(), "checked source");
        assert_eq!(completed.result().feedback(), "1");
        assert_eq!(completed.replay(), &replay("opaque-call"));
        assert_eq!(
            completed.result(),
            &CodeResult::Execution {
                feedback: "1".to_owned(),
            }
        );

        let error = history
            .append_completed_code(
                "print(2)".to_owned(),
                call_id.clone(),
                None,
                String::new(),
                replay("other-call"),
                CodeResult::Execution {
                    feedback: "2".to_owned(),
                },
            )
            .unwrap_err();

        assert_eq!(error, ModelHistoryError::DuplicateCallId(call_id));
        assert_eq!(history.entries().len(), 2);
    }

    #[test]
    fn final_message_history_stores_reasoning_summary_beside_opaque_replay() {
        let mut history = history_with_request("inspect");
        history.append_final_message(
            "answer".to_owned(),
            "checked result".to_owned(),
            replay("opaque-message"),
        );

        let entry = history.entries().last().unwrap();
        assert_eq!(
            entry,
            &ModelContextEntry::FinalMessage {
                text: "answer".to_owned(),
                reasoning_summary: "checked result".to_owned(),
                replay: replay("opaque-message"),
            }
        );
        assert_eq!(
            entry.final_message(),
            Some(("answer", "checked result", &replay("opaque-message")))
        );
    }

    fn foreign_code(commentary: Option<&str>, reasoning_summary: &str) -> ModelContextEntry {
        let mut history = ModelHistory::new();
        history
            .append_completed_code(
                "print(1)".to_owned(),
                ModelCallId::new("toolu_1"),
                commentary.map(str::to_owned),
                reasoning_summary.to_owned(),
                replay("signed-thinking"),
                CodeResult::execution("1\n".to_owned()),
            )
            .unwrap();
        history.entries()[0].clone()
    }

    fn foreign_message(reasoning_summary: &str) -> ModelContextEntry {
        let mut history = ModelHistory::new();
        history.append_final_message(
            "Done.".to_owned(),
            reasoning_summary.to_owned(),
            replay("signed-thinking"),
        );
        history.entries()[0].clone()
    }

    const TOOL_CALL: ForeignPart<'static> = ForeignPart::ToolCall {
        call_id: "toolu_1",
        source: "print(1)",
        output: "1\n",
    };

    #[test]
    fn foreign_code_is_reasoning_then_commentary_then_the_tool_call_with_its_result() {
        let entry = foreign_code(Some("I'll run it."), "Checking the file");

        assert_eq!(
            entry.foreign_parts("openai", CrossProviderReasoning::Inline),
            Some(vec![
                ForeignPart::Text("Checking the file"),
                ForeignPart::Text("I'll run it."),
                TOOL_CALL,
            ])
        );
    }

    #[test]
    fn foreign_parts_omit_empty_text_and_dropped_reasoning() {
        for (entry, reasoning, expected) in [
            (
                foreign_code(Some("I'll run it."), ""),
                CrossProviderReasoning::Inline,
                vec![ForeignPart::Text("I'll run it."), TOOL_CALL],
            ),
            (
                foreign_code(Some("I'll run it."), "Checking the file"),
                CrossProviderReasoning::Drop,
                vec![ForeignPart::Text("I'll run it."), TOOL_CALL],
            ),
            (
                foreign_code(Some(""), "Checking the file"),
                CrossProviderReasoning::Inline,
                vec![ForeignPart::Text("Checking the file"), TOOL_CALL],
            ),
            (
                foreign_code(None, ""),
                CrossProviderReasoning::Inline,
                vec![TOOL_CALL],
            ),
        ] {
            assert_eq!(
                entry.foreign_parts("openai", reasoning),
                Some(expected),
                "{entry:?} {reasoning:?}"
            );
        }
    }

    #[test]
    fn foreign_final_messages_give_their_text_after_any_kept_reasoning() {
        for (entry, reasoning, expected) in [
            (
                foreign_message("Summarising"),
                CrossProviderReasoning::Inline,
                vec![ForeignPart::Text("Summarising"), ForeignPart::Text("Done.")],
            ),
            (
                foreign_message("Summarising"),
                CrossProviderReasoning::Drop,
                vec![ForeignPart::Text("Done.")],
            ),
            (
                foreign_message(""),
                CrossProviderReasoning::Inline,
                vec![ForeignPart::Text("Done.")],
            ),
        ] {
            assert_eq!(
                entry.foreign_parts("anthropic", reasoning),
                Some(expected),
                "{entry:?} {reasoning:?}"
            );
        }
    }

    #[test]
    fn own_provider_and_user_entries_have_no_foreign_parts() {
        for entry in [
            foreign_code(Some("I'll run it."), "Checking the file"),
            foreign_message("Summarising"),
            ModelContextEntry::UserRequest {
                text: "inspect".to_owned(),
            },
            ModelContextEntry::CompactionSummary {
                summary: "saved summary".to_owned(),
            },
        ] {
            assert_eq!(
                entry.foreign_parts("test-provider", CrossProviderReasoning::Inline),
                None,
                "{entry:?}"
            );
        }
    }

    #[test]
    fn completed_code_history_admits_each_terminal_result_only_as_a_compound_entry() {
        let mut history = history_with_request("inspect");
        for (index, result) in [
            CodeResult::execution("completed".to_owned()),
            CodeResult::cancelled("cancelled".to_owned()),
            CodeResult::failure("failed".to_owned()),
        ]
        .into_iter()
        .enumerate()
        {
            history
                .append_completed_code(
                    format!("code-{index}"),
                    ModelCallId::new(format!("call-{index}")),
                    None,
                    String::new(),
                    replay(&format!("opaque-{index}")),
                    result,
                )
                .unwrap();
        }

        let results = history
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .map(CompletedCode::result)
            .collect::<Vec<_>>();

        assert!(matches!(results[0], CodeResult::Execution { .. }));
        assert!(matches!(results[1], CodeResult::Cancelled { .. }));
        assert!(matches!(results[2], CodeResult::Failure { .. }));
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn code_results_bound_feedback_at_a_utf8_boundary() {
        let feedback = "é".repeat(super::CODE_RESULT_FEEDBACK_LIMIT / 2 + 1);
        for result in [
            CodeResult::execution(feedback.clone()),
            CodeResult::cancelled(feedback.clone()),
            CodeResult::failure(feedback.clone()),
        ] {
            let feedback = match result {
                CodeResult::Execution { feedback }
                | CodeResult::Cancelled { feedback }
                | CodeResult::Failure { feedback } => feedback,
            };
            assert_eq!(feedback, "é".repeat(super::CODE_RESULT_FEEDBACK_LIMIT / 2));
            assert!(feedback.is_char_boundary(feedback.len()));
        }
    }

    #[test]
    fn model_history_bounds_direct_code_result_feedback() {
        let mut history = history_with_request("inspect");
        history
            .append_completed_code(
                "code".to_owned(),
                ModelCallId::new("call"),
                None,
                String::new(),
                replay("opaque"),
                CodeResult::Failure {
                    feedback: "é".repeat(super::CODE_RESULT_FEEDBACK_LIMIT / 2 + 1),
                },
            )
            .unwrap();

        let Some(CodeResult::Failure { feedback }) = history
            .entries()
            .last()
            .and_then(ModelContextEntry::completed_code)
            .map(CompletedCode::result)
        else {
            panic!("expected bounded failure result");
        };
        assert_eq!(
            feedback.as_str(),
            "é".repeat(super::CODE_RESULT_FEEDBACK_LIMIT / 2)
        );
    }

    #[tokio::test]
    async fn scripted_model_cancellation_does_not_consume_an_action() {
        let mut model = ScriptedModel::new(vec![ScriptedAction::Message {
            text: "done".to_owned(),
        }])
        .with_delay(Duration::from_millis(50));
        let history = history_with_request("inspect");
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            canceller.cancel();
        });
        let mut progress = |_| {};

        assert_eq!(
            model
                .step(selection(), &history, &cancellation, &mut progress)
                .await,
            Err(ModelFailure::from(ModelError::Cancelled))
        );
        cancel.await.unwrap();
        assert_eq!(model.position(), 0);
    }
}
