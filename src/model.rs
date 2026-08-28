use std::future::Future;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

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
    },
    Message {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelStep {
    pub action: ModelAction,
    pub replay: ProviderReplay,
    pub attempts: Vec<AttemptAccounting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelUsage {
    pub model: String,
    pub service_tier: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptAccounting {
    KnownUsage(ModelUsage),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeResult {
    Execution { feedback: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedCode {
    source: String,
    call_id: ModelCallId,
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

    pub fn replay(&self) -> &ProviderReplay {
        &self.replay
    }

    pub fn result(&self) -> &CodeResult {
        &self.result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelContextEntry {
    ReadCapability {
        description: String,
    },
    UserRequest {
        text: String,
    },
    CompletedCode(CompletedCode),
    FinalMessage {
        text: String,
        replay: ProviderReplay,
    },
}

impl ModelContextEntry {
    pub fn completed_code(&self) -> Option<&CompletedCode> {
        match self {
            Self::CompletedCode(completed) => Some(completed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelHistory {
    entries: Vec<ModelContextEntry>,
}

impl ModelHistory {
    pub fn new(read_capability: impl Into<String>) -> Self {
        Self {
            entries: vec![ModelContextEntry::ReadCapability {
                description: read_capability.into(),
            }],
        }
    }

    pub fn entries(&self) -> &[ModelContextEntry] {
        &self.entries
    }

    pub fn append_user_request(&mut self, text: impl Into<String>) {
        self.entries
            .push(ModelContextEntry::UserRequest { text: text.into() });
    }

    pub fn append_completed_code(
        &mut self,
        source: String,
        call_id: ModelCallId,
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
                replay,
                result,
            }));
        Ok(())
    }

    pub fn append_final_message(&mut self, text: String, replay: ProviderReplay) {
        self.entries
            .push(ModelContextEntry::FinalMessage { text, replay });
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

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelError {
    #[error("model step was cancelled")]
    Cancelled,
    #[error("scripted model exhausted before returning a final message")]
    Exhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelProgress {}

pub trait Model: Send + 'static {
    fn step<'a>(
        &'a mut self,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelError>> + Send + 'a;
}

pub const READ_CAPABILITY_DESCRIPTION: &str = "read(path) returns the accepted file as complete exact lines with endings retained. Use standard Python print(\"\".join(lines[start:end]), end=\"\") to inspect a selection.";

pub struct ScriptedModel {
    actions: Vec<ScriptedAction>,
    cursor: usize,
    observed_user_requests: usize,
    delay: Duration,
    #[cfg(test)]
    contexts: Vec<Vec<ModelContextEntry>>,
}

impl ScriptedModel {
    pub fn new(actions: Vec<ScriptedAction>) -> Self {
        Self {
            actions,
            cursor: 0,
            observed_user_requests: 0,
            delay: Duration::ZERO,
            #[cfg(test)]
            contexts: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    #[cfg(test)]
    pub(crate) fn contexts(&self) -> &[Vec<ModelContextEntry>] {
        &self.contexts
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> usize {
        self.cursor
    }
}

impl Model for ScriptedModel {
    fn step<'a>(
        &'a mut self,
        history: &'a ModelHistory,
        cancellation: &'a CancellationToken,
        _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
    ) -> impl Future<Output = Result<ModelStep, ModelError>> + Send + 'a {
        async move {
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled);
            }

            let user_requests = history.user_request_count();
            if user_requests > self.observed_user_requests {
                self.cursor = 0;
                self.observed_user_requests = user_requests;
            }
            #[cfg(test)]
            self.contexts.push(history.entries().to_vec());

            let scripted_action = self
                .actions
                .get(self.cursor)
                .cloned()
                .ok_or(ModelError::Exhausted)?;
            if !self.delay.is_zero() {
                tokio::select! {
                    _ = cancellation.cancelled() => return Err(ModelError::Cancelled),
                    _ = tokio::time::sleep(self.delay) => {}
                }
            }
            if cancellation.is_cancelled() {
                return Err(ModelError::Cancelled);
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
                },
                ScriptedAction::Message { text } => ModelAction::Message { text },
            };
            Ok(ModelStep {
                action,
                replay,
                attempts: Vec::new(),
            })
        }
    }
}

impl Default for ScriptedModel {
    fn default() -> Self {
        Self::new(vec![
            ScriptedAction::Code {
                source: "try:\n    request_count += 1\nexcept NameError:\n    request_count = 1\nlines = await read(\"Cargo.toml\")".to_owned(),
            },
            ScriptedAction::Code {
                source: "print(\"\".join(lines[0:20]), end=\"\")\nrequest_count".to_owned(),
            },
            ScriptedAction::Message {
                text: "I inspected Cargo.toml.".to_owned(),
            },
        ])
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        CodeResult, Model, ModelAction, ModelCallId, ModelContextEntry, ModelError, ModelHistory,
        ModelHistoryError, OpaqueReplayItem, ProviderReplay, ScriptedAction, ScriptedModel,
    };

    fn history_with_request(request: &str) -> ModelHistory {
        let mut history = ModelHistory::new("read files");
        history.append_user_request(request);
        history
    }

    fn replay(value: &str) -> ProviderReplay {
        ProviderReplay::new(
            "test-provider",
            vec![OpaqueReplayItem::new(value.as_bytes().to_vec())],
        )
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
        let mut step = Box::pin(model.step(&history, &cancellation, &mut progress));

        assert!(timeout(Duration::from_millis(5), &mut step).await.is_err());
        let step = step.await.unwrap();

        assert_eq!(
            step.action,
            ModelAction::Code {
                source: "first".to_owned(),
                call_id: ModelCallId::new("scripted-1-1"),
            }
        );
        assert_eq!(step.replay.provider(), "scripted");
        assert_eq!(step.replay.items()[0].as_bytes(), b"scripted-1-1");
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
                .step(&history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Code { .. }
        ));
        assert!(matches!(
            model
                .step(&history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Message { .. }
        ));
        assert_eq!(
            model.step(&history, &cancellation, &mut progress).await,
            Err(ModelError::Exhausted)
        );

        history.append_user_request("second request");

        assert_eq!(
            model
                .step(&history, &cancellation, &mut progress)
                .await
                .unwrap()
                .action,
            ModelAction::Code {
                source: "first".to_owned(),
                call_id: ModelCallId::new("scripted-2-1"),
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
                replay("other-call"),
                CodeResult::Execution {
                    feedback: "2".to_owned(),
                },
            )
            .unwrap_err();

        assert_eq!(error, ModelHistoryError::DuplicateCallId(call_id));
        assert_eq!(history.entries().len(), 3);
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
            model.step(&history, &cancellation, &mut progress).await,
            Err(ModelError::Cancelled)
        );
        cancel.await.unwrap();
        assert_eq!(model.position(), 0);
    }
}
