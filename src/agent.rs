use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::arena::{
    Arena, ArenaError, ArenaProgress, Execution, ExecutionOutcome, InterruptedExecution,
    InterruptedRead, Observation, PythonError, ReadOutcome,
};
use crate::model::{
    AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelFailure, ModelHistory,
    ModelHistoryError, ModelProgress, ModelStep, ProviderReplay, READ_CAPABILITY_DESCRIPTION,
};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AgentProgress {
    ModelStepStarted,
    ExecutionStarted,
    ToolStarted {
        path: String,
    },
    ToolFinished {
        path: String,
        outcome: ReadOutcome,
    },
    ExecutionCompleted {
        source: String,
        execution: Execution,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRunOutcome {
    FinalMessageStaged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalMessage {
    text: String,
    attempts: Vec<AttemptAccounting>,
}

impl FinalMessage {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attempts(&self) -> &[AttemptAccounting] {
        &self.attempts
    }
}

struct StagedFinalMessage {
    final_message: FinalMessage,
    replay: ProviderReplay,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent run was cancelled")]
    Cancelled { attempts: Vec<AttemptAccounting> },
    #[error("{failure}")]
    Model {
        #[source]
        failure: ModelFailure,
        attempts: Vec<AttemptAccounting>,
    },
    #[error("{error}")]
    ModelHistory {
        #[source]
        error: ModelHistoryError,
        attempts: Vec<AttemptAccounting>,
    },
    #[error("{error}")]
    Arena {
        #[source]
        error: ArenaError,
        attempts: Vec<AttemptAccounting>,
    },
    #[error("agent has no staged final message")]
    NoStagedFinalMessage,
    #[error("agent has a staged final message awaiting terminal settlement")]
    StagedFinalMessageUnresolved,
}

impl AgentError {
    pub(crate) fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled { .. } => true,
            Self::Model { failure, .. } => failure.is_cancelled(),
            Self::Arena {
                error: ArenaError::Cancelled { .. },
                ..
            } => true,
            Self::ModelHistory { .. }
            | Self::Arena { .. }
            | Self::NoStagedFinalMessage
            | Self::StagedFinalMessageUnresolved => false,
        }
    }

    pub fn attempts(&self) -> &[AttemptAccounting] {
        match self {
            Self::Cancelled { attempts }
            | Self::Model { attempts, .. }
            | Self::ModelHistory { attempts, .. }
            | Self::Arena { attempts, .. } => attempts,
            Self::NoStagedFinalMessage | Self::StagedFinalMessageUnresolved => &[],
        }
    }
}

struct PendingCode {
    source: String,
    call_id: ModelCallId,
    replay: ProviderReplay,
}

impl PendingCode {
    fn append(
        self,
        history: &mut ModelHistory,
        result: CodeResult,
    ) -> Result<(), ModelHistoryError> {
        history.append_completed_code(self.source, self.call_id, self.replay, result)
    }
}

pub struct Agent<M: Model> {
    arena: Arena,
    model: M,
    model_context: ModelHistory,
    staged_final_message: Option<StagedFinalMessage>,
}

impl<M: Model> Agent<M> {
    pub fn new(arena: Arena, model: M) -> Self {
        Self {
            arena,
            model,
            model_context: ModelHistory::new(READ_CAPABILITY_DESCRIPTION),
            staged_final_message: None,
        }
    }

    pub async fn run(
        &mut self,
        request: impl Into<String>,
        cancellation: &CancellationToken,
    ) -> Result<AgentRunOutcome, AgentError> {
        let mut progress = |_| {};
        self.run_with_progress(request, cancellation, &mut progress)
            .await
    }

    pub fn admit_staged_final_message(&mut self) -> Result<FinalMessage, AgentError> {
        let StagedFinalMessage {
            final_message,
            replay,
        } = self
            .staged_final_message
            .take()
            .ok_or(AgentError::NoStagedFinalMessage)?;
        self.model_context
            .append_final_message(final_message.text.clone(), replay);
        Ok(final_message)
    }

    pub fn discard_staged_final_message(&mut self) -> Option<FinalMessage> {
        self.staged_final_message
            .take()
            .map(|staged| staged.final_message)
    }

    pub(crate) async fn run_with_progress(
        &mut self,
        request: impl Into<String>,
        cancellation: &CancellationToken,
        progress: &mut (impl FnMut(AgentProgress) + Send),
    ) -> Result<AgentRunOutcome, AgentError> {
        if self.staged_final_message.is_some() {
            return Err(AgentError::StagedFinalMessageUnresolved);
        }
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled {
                attempts: Vec::new(),
            });
        }
        self.model_context.append_user_request(request);
        let mut attempts = Vec::new();

        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled { attempts });
            }
            progress(AgentProgress::ModelStepStarted);
            let mut model_progress = |_model_progress: ModelProgress| {};
            let model_step = match self
                .model
                .step(&self.model_context, cancellation, &mut model_progress)
                .await
            {
                Ok(step) => step,
                Err(failure) => {
                    attempts.extend(failure.attempts().iter().cloned());
                    return Err(AgentError::Model { failure, attempts });
                }
            };
            let ModelStep {
                action,
                replay,
                attempts: step_attempts,
            } = model_step;
            attempts.extend(step_attempts);
            match action {
                ModelAction::Code { source, call_id } => {
                    if self.model_context.contains_call_id(&call_id) {
                        return Err(AgentError::ModelHistory {
                            error: ModelHistoryError::DuplicateCallId(call_id),
                            attempts,
                        });
                    }
                    let pending = PendingCode {
                        source,
                        call_id,
                        replay,
                    };
                    if cancellation.is_cancelled() {
                        if let Err(error) = pending.append(
                            &mut self.model_context,
                            CodeResult::cancelled(interruption_feedback(None, true)),
                        ) {
                            return Err(AgentError::ModelHistory { error, attempts });
                        }
                        return Err(AgentError::Cancelled { attempts });
                    }
                    progress(AgentProgress::ExecutionStarted);
                    let execution = {
                        let mut arena_progress = |arena_progress| match arena_progress {
                            ArenaProgress::ReadStarted { path } => {
                                progress(AgentProgress::ToolStarted { path });
                            }
                            ArenaProgress::ReadFinished { path, outcome } => {
                                progress(AgentProgress::ToolFinished { path, outcome });
                            }
                        };
                        self.arena
                            .execute_with_progress(
                                &pending.source,
                                cancellation,
                                &mut arena_progress,
                            )
                            .await
                    };
                    match execution {
                        Ok(execution) => {
                            let source = pending.source.clone();
                            if let Err(error) = pending.append(
                                &mut self.model_context,
                                CodeResult::execution(execution_feedback(&execution)),
                            ) {
                                return Err(AgentError::ModelHistory { error, attempts });
                            }
                            progress(AgentProgress::ExecutionCompleted { source, execution });
                        }
                        Err(error) => {
                            let result = if matches!(&error, ArenaError::Cancelled { .. }) {
                                CodeResult::cancelled(interruption_feedback(
                                    interrupted_execution(&error),
                                    true,
                                ))
                            } else {
                                CodeResult::failure(failure_feedback(&error))
                            };
                            if let Err(history_error) =
                                pending.append(&mut self.model_context, result)
                            {
                                return Err(AgentError::ModelHistory {
                                    error: history_error,
                                    attempts,
                                });
                            }
                            return Err(AgentError::Arena { error, attempts });
                        }
                    }
                }
                ModelAction::Message { text } => {
                    self.staged_final_message = Some(StagedFinalMessage {
                        final_message: FinalMessage { text, attempts },
                        replay,
                    });
                    return Ok(AgentRunOutcome::FinalMessageStaged);
                }
            }
        }
    }
}

const CANCELLATION_WARNING: &str = "Execution was cancelled and its Python arena changes were discarded. External side effects completed before cancellation may remain even though pending actions were cancelled.";

fn interrupted_execution(error: &ArenaError) -> Option<&InterruptedExecution> {
    match error {
        ArenaError::Cancelled { execution } | ArenaError::TimedOut { execution } => Some(execution),
        ArenaError::Infrastructure { execution, .. }
        | ArenaError::UnsupportedSuspension { execution, .. } => execution.as_deref(),
        ArenaError::RecoveryFailed { original, .. } => interrupted_execution(original),
        ArenaError::WorkerPathNotAbsolute
        | ArenaError::WorkerVersionTimedOut
        | ArenaError::WorkerVersionExecution { .. }
        | ArenaError::WorkerVersionFailed { .. }
        | ArenaError::WorkerVersionInvalidOutput
        | ArenaError::WorkerVersionMismatch { .. }
        | ArenaError::IncompatibleCheckpoint { .. }
        | ArenaError::SuspendedCheckpoint => None,
    }
}

fn contains_cancellation(error: &ArenaError) -> bool {
    match error {
        ArenaError::Cancelled { .. } => true,
        ArenaError::RecoveryFailed { original, .. } => contains_cancellation(original),
        ArenaError::WorkerPathNotAbsolute
        | ArenaError::WorkerVersionTimedOut
        | ArenaError::WorkerVersionExecution { .. }
        | ArenaError::WorkerVersionFailed { .. }
        | ArenaError::WorkerVersionInvalidOutput
        | ArenaError::WorkerVersionMismatch { .. }
        | ArenaError::TimedOut { .. }
        | ArenaError::Infrastructure { .. }
        | ArenaError::IncompatibleCheckpoint { .. }
        | ArenaError::SuspendedCheckpoint
        | ArenaError::UnsupportedSuspension { .. } => false,
    }
}

fn interruption_feedback(execution: Option<&InterruptedExecution>, cancelled: bool) -> String {
    let mut frames = Vec::new();
    let began = execution.is_some_and(|execution| execution.began);
    let state = if cancelled { "cancelled" } else { "failed" };
    frames.push(format!(
        "<kiki_execution state=\"{state}\" began=\"{began}\">"
    ));
    if !began {
        frames.push("Execution did not begin.".to_owned());
    }
    if let Some(execution) = execution {
        append_output(&mut frames, "stdout", &execution.stdout);
        append_output(&mut frames, "stderr", &execution.stderr);
        frames.push(render_interrupted_read(&execution.read));
    } else {
        frames.push("<kiki_read state=\"none\" />".to_owned());
    }
    if cancelled {
        frames.push(CANCELLATION_WARNING.to_owned());
    }
    frames.push("</kiki_execution>".to_owned());
    frames.join("\n")
}

fn render_interrupted_read(read: &InterruptedRead) -> String {
    match read {
        InterruptedRead::NoRead => "<kiki_read state=\"none\" />".to_owned(),
        InterruptedRead::Pending { path } => {
            format!("<kiki_read state=\"pending\" path=\"{path}\" />")
        }
        InterruptedRead::Completed { path, outcome } => match outcome {
            ReadOutcome::Success { line_count } => format!(
                "<kiki_read state=\"completed\" path=\"{path}\" outcome=\"success\" lines=\"{line_count}\" />"
            ),
            ReadOutcome::Failure { message } => format!(
                "<kiki_read state=\"completed\" path=\"{path}\" outcome=\"failure\">\n{message}\n</kiki_read>"
            ),
        },
    }
}

fn failure_feedback(error: &ArenaError) -> String {
    let mut feedback = format!("<kiki_failure>\n{error}\n</kiki_failure>");
    if let Some(execution) = interrupted_execution(error) {
        feedback.push('\n');
        feedback.push_str(&interruption_feedback(
            Some(execution),
            contains_cancellation(error),
        ));
    }
    feedback
}

fn execution_feedback(execution: &Execution) -> String {
    let mut frames = Vec::new();
    append_output(&mut frames, "stdout", &execution.stdout);
    append_output(&mut frames, "stderr", &execution.stderr);
    match &execution.outcome {
        ExecutionOutcome::Complete {
            result: Some(result),
        } => {
            let truncated = result.tail.is_some();
            frames.push(render_result(result));
            if truncated {
                frames.push(truncation_warning("result"));
            }
        }
        ExecutionOutcome::Complete { result: None } => {}
        ExecutionOutcome::PythonError(error) => {
            frames.push(render_error(error));
            if error.omitted_bytes > 0 {
                frames.push(error_truncation_warning());
            }
        }
    }
    frames.join("\n")
}

fn append_output(frames: &mut Vec<String>, stream: &str, observation: &Observation) {
    if observation.total_bytes == 0 {
        return;
    }
    let truncated = observation.tail.is_some();
    frames.push(render_output(stream, observation));
    if truncated {
        frames.push(truncation_warning("output"));
    }
}

fn render_output(stream: &str, observation: &Observation) -> String {
    render_observation("kiki_output", &format!(" stream=\"{stream}\""), observation)
}

fn render_result(observation: &Observation) -> String {
    render_observation("kiki_result", "", observation)
}

fn render_observation(tag: &str, extra_attributes: &str, observation: &Observation) -> String {
    let truncated = observation.tail.is_some();
    let mut rendered = format!(
        "<{tag}{extra_attributes} truncated=\"{truncated}\" total_lines=\"{}\" total_bytes=\"{}\">\n<kiki_output_head lines=\"{}\" bytes=\"{}\" ends_mid_line=\"{}\">\n{}",
        observation.total_lines,
        observation.total_bytes,
        observation.head_lines,
        observation.head.len(),
        observation.head_ends_mid_line,
        observation.head,
    );
    rendered.push_str("</kiki_output_head>\n");
    if let Some(tail) = &observation.tail {
        rendered.push_str(&format!(
            "<kiki_output_omitted lines=\"{}\" bytes=\"{}\" partial_lines=\"{}\" />\n<kiki_output_tail lines=\"{}\" bytes=\"{}\" starts_mid_line=\"{}\">\n{}",
            observation.omitted_lines,
            observation.omitted_bytes,
            observation.partial_lines,
            observation.tail_lines,
            tail.len(),
            observation.tail_starts_mid_line,
            tail,
        ));
        rendered.push_str("</kiki_output_tail>\n");
    }
    rendered.push_str(&format!("</{tag}>"));
    rendered
}

fn truncation_warning(kind: &str) -> String {
    format!(
        "<kiki_warning code=\"{kind}_truncated\" arena_state=\"committed\">\nExecution completed, but its model-visible {kind} was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.\n</kiki_warning>"
    )
}

fn render_error(error: &PythonError) -> String {
    format!(
        "<kiki_error omitted_bytes=\"{}\">\n{}</kiki_error>",
        error.omitted_bytes, error.text
    )
}

fn error_truncation_warning() -> String {
    "<kiki_warning code=\"error_truncated\" arena_state=\"committed\">\nExecution completed with a Python error, but its model-visible error was truncated. The underlying arena values were not changed.\n</kiki_warning>".to_owned()
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{Agent, AgentError, AgentProgress, AgentRunOutcome};
    use crate::arena::{Arena, Execution, ExecutionOutcome, Observation, PythonError};
    use crate::model::{
        AttemptAccounting, CodeResult, Model, ModelAction as CompletedModelAction, ModelCallId,
        ModelContextEntry, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ProviderFailure, ProviderReplay, READ_CAPABILITY_DESCRIPTION,
        ScriptedAction as ModelAction, ScriptedModel,
    };
    use crate::tools::ReadTool;

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the monty-runtime 0.0.21 worker")
    }

    async fn agent(workspace: &TempDir, actions: Vec<ModelAction>) -> Agent<ScriptedModel> {
        agent_with_model(workspace, ScriptedModel::new(actions)).await
    }

    async fn agent_with_model<M: Model>(workspace: &TempDir, model: M) -> Agent<M> {
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        Agent::new(arena, model)
    }

    async fn run_capturing_progress(
        agent: &mut Agent<ScriptedModel>,
        request: &str,
        cancellation: &CancellationToken,
    ) -> Result<(String, Vec<AgentProgress>), AgentError> {
        let mut captured = Vec::new();
        let outcome = {
            let mut progress = |event| captured.push(event);
            agent
                .run_with_progress(request, cancellation, &mut progress)
                .await?
        };
        assert_eq!(outcome, super::AgentRunOutcome::FinalMessageStaged);
        let message = agent.admit_staged_final_message()?.text().to_owned();
        Ok((message, captured))
    }

    fn completed_executions(progress: &[AgentProgress]) -> Vec<(&str, &Execution)> {
        progress
            .iter()
            .filter_map(|event| match event {
                AgentProgress::ExecutionCompleted { source, execution } => {
                    Some((source.as_str(), execution))
                }
                _ => None,
            })
            .collect()
    }

    struct ObservationMetadata {
        head_lines: usize,
        tail_lines: usize,
        total_lines: usize,
        total_bytes: usize,
        omitted_lines: usize,
        omitted_bytes: usize,
        head_ends_mid_line: bool,
        tail_starts_mid_line: bool,
    }

    fn observation(head: &str, tail: Option<&str>, metadata: ObservationMetadata) -> Observation {
        Observation {
            head: head.to_owned(),
            tail: tail.map(str::to_owned),
            head_lines: metadata.head_lines,
            tail_lines: metadata.tail_lines,
            total_lines: metadata.total_lines,
            total_bytes: metadata.total_bytes,
            omitted_lines: metadata.omitted_lines,
            omitted_bytes: metadata.omitted_bytes,
            head_ends_mid_line: metadata.head_ends_mid_line,
            tail_starts_mid_line: metadata.tail_starts_mid_line,
            partial_lines: metadata.head_ends_mid_line || metadata.tail_starts_mid_line,
        }
    }

    #[tokio::test]
    async fn agent_feeds_completed_code_back_before_a_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "print('hello')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
                ModelAction::Code {
                    source: "message_did_not_terminate = True".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        assert_eq!(
            agent.run("inspect", &cancellation).await.unwrap(),
            super::AgentRunOutcome::FinalMessageStaged
        );
        assert_eq!(agent.admit_staged_final_message().unwrap().text(), "done");
        assert_eq!(agent.model.contexts().len(), 2);
        assert_eq!(
            agent.model.contexts()[0],
            [
                ModelContextEntry::ReadCapability {
                    description: READ_CAPABILITY_DESCRIPTION.to_owned(),
                },
                ModelContextEntry::UserRequest {
                    text: "inspect".to_owned(),
                },
            ]
        );
        let completed = agent.model.contexts()[1]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .expect("second model step should receive completed code");
        assert_eq!(completed.source(), "print('hello')");
        assert_eq!(completed.call_id().as_str(), "scripted-1-1");
        assert_eq!(completed.replay().provider(), "scripted");
        assert_eq!(completed.replay().items().len(), 1);
        assert_eq!(
            completed.result(),
            &CodeResult::Execution {
                feedback: "<kiki_output stream=\"stdout\" truncated=\"false\" total_lines=\"1\" total_bytes=\"6\">\n<kiki_output_head lines=\"1\" bytes=\"6\" ends_mid_line=\"false\">\nhello\n</kiki_output_head>\n</kiki_output>".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn agent_keeps_a_completed_final_message_staged_until_runtime_admission() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![ModelAction::Message {
                text: "done".to_owned(),
            }],
        )
        .await;
        let cancellation = CancellationToken::new();

        assert_eq!(
            agent.run("inspect", &cancellation).await.unwrap(),
            AgentRunOutcome::FinalMessageStaged
        );
        cancellation.cancel();

        assert!(
            !agent
                .model_context
                .entries()
                .iter()
                .any(|entry| matches!(entry, ModelContextEntry::FinalMessage { .. }))
        );
        let discarded = agent
            .discard_staged_final_message()
            .expect("completed final message should remain staged");
        assert_eq!(discarded.text(), "done");
        assert!(discarded.attempts().is_empty());
        assert!(agent.discard_staged_final_message().is_none());
        assert!(matches!(
            agent.admit_staged_final_message(),
            Err(AgentError::NoStagedFinalMessage)
        ));
    }

    #[tokio::test]
    async fn agent_admits_a_staged_final_message_exactly_once() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![ModelAction::Message {
                text: "done".to_owned(),
            }],
        )
        .await;
        let cancellation = CancellationToken::new();

        assert_eq!(
            agent.run("inspect", &cancellation).await.unwrap(),
            AgentRunOutcome::FinalMessageStaged
        );
        let admitted = agent.admit_staged_final_message().unwrap();

        assert_eq!(admitted.text(), "done");
        assert!(
            agent
                .model_context
                .entries()
                .iter()
                .filter(|entry| matches!(entry, ModelContextEntry::FinalMessage { .. }))
                .count()
                == 1
        );
        assert!(matches!(
            agent.admit_staged_final_message(),
            Err(AgentError::NoStagedFinalMessage)
        ));
    }

    struct MessageWithAccountingModel;

    impl Model for MessageWithAccountingModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async {
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                })
            }
        }
    }

    struct PartialFailureModel;

    impl Model for PartialFailureModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                progress(ModelProgress::AssistantTextChunk {
                    text: "partial answer".to_owned(),
                });
                Err(ModelFailure::new(
                    ModelError::Provider(Box::new(ProviderFailure::new(
                        Some(500),
                        Some("failed".to_owned()),
                        "provider failed".to_owned(),
                    ))),
                    vec![AttemptAccounting::UsageUnavailable],
                ))
            }
        }
    }

    #[tokio::test]
    async fn agent_preserves_final_and_provider_failure_attempt_accounting() {
        let workspace = tempfile::tempdir().unwrap();
        let cancellation = CancellationToken::new();
        let mut successful = agent_with_model(&workspace, MessageWithAccountingModel).await;

        assert_eq!(
            successful.run("inspect", &cancellation).await.unwrap(),
            AgentRunOutcome::FinalMessageStaged
        );
        assert_eq!(
            successful.admit_staged_final_message().unwrap().attempts(),
            &[AttemptAccounting::UsageUnavailable]
        );

        let mut failed = agent_with_model(&workspace, PartialFailureModel).await;
        let error = failed.run("inspect", &cancellation).await.unwrap_err();

        assert!(matches!(
            &error,
            AgentError::Model { failure, .. }
                if matches!(failure.error(), ModelError::Provider(_))
                    && failure.attempts() == [AttemptAccounting::UsageUnavailable]
        ));
        assert_eq!(error.attempts(), &[AttemptAccounting::UsageUnavailable]);
        assert!(!failed.model_context.entries().iter().any(|entry| matches!(
            entry,
            ModelContextEntry::CompletedCode(_) | ModelContextEntry::FinalMessage { .. }
        )));
    }

    #[tokio::test]
    async fn agent_awaits_an_asynchronously_delayed_complete_model_action() {
        let workspace = tempfile::tempdir().unwrap();
        let model = ScriptedModel::new(vec![
            ModelAction::Code {
                source: "delayed_action = 1".to_owned(),
            },
            ModelAction::Message {
                text: "done".to_owned(),
            },
        ])
        .with_delay(Duration::from_millis(50));
        let mut agent = agent_with_model(&workspace, model).await;
        let cancellation = CancellationToken::new();
        let mut run = Box::pin(agent.run("inspect", &cancellation));

        assert!(timeout(Duration::from_millis(5), &mut run).await.is_err());
        assert_eq!(
            run.await.unwrap(),
            super::AgentRunOutcome::FinalMessageStaged
        );
        assert_eq!(agent.admit_staged_final_message().unwrap().text(), "done");
        assert!(matches!(
            agent.model_context.entries(),
            [
                ModelContextEntry::ReadCapability { .. },
                ModelContextEntry::UserRequest { .. },
                ModelContextEntry::CompletedCode(_),
                ModelContextEntry::FinalMessage { .. },
            ]
        ));
    }

    struct BoundaryCancellationModel {
        action: CompletedModelAction,
    }

    impl Model for BoundaryCancellationModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                cancellation.cancel();
                Ok(ModelStep {
                    action: self.action.clone(),
                    replay: ProviderReplay::new("boundary", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn agent_stages_a_completed_message_after_model_boundary_cancellation() {
        for action in [
            CompletedModelAction::Code {
                source: "boundary_action_ran = True".to_owned(),
                call_id: ModelCallId::new("boundary-code"),
            },
            CompletedModelAction::Message {
                text: "must not be admitted".to_owned(),
            },
        ] {
            let code_action = matches!(action, CompletedModelAction::Code { .. });
            let workspace = tempfile::tempdir().unwrap();
            let mut agent =
                agent_with_model(&workspace, BoundaryCancellationModel { action }).await;
            let cancellation = CancellationToken::new();
            let mut progress = Vec::new();

            let result = {
                let mut capture = |event| progress.push(event);
                agent
                    .run_with_progress("inspect", &cancellation, &mut capture)
                    .await
            };

            assert!(!progress.iter().any(|event| matches!(
                event,
                AgentProgress::ExecutionStarted | AgentProgress::ExecutionCompleted { .. }
            )));
            if code_action {
                let error = result.unwrap_err();
                assert!(matches!(error, AgentError::Cancelled { .. }));
                let Some(ModelContextEntry::CompletedCode(completed)) =
                    agent.model_context.entries().last()
                else {
                    panic!("cancelled code should be admitted with a result");
                };
                let CodeResult::Cancelled { feedback } = completed.result() else {
                    panic!("cancelled code should have a cancelled result");
                };
                assert!(feedback.contains("Execution did not begin."));
                assert!(feedback.contains(
                    "Execution was cancelled and its Python arena changes were discarded. External side effects completed before cancellation may remain even though pending actions were cancelled."
                ));
            } else {
                assert_eq!(result.unwrap(), AgentRunOutcome::FinalMessageStaged);
                assert!(!agent.model_context.entries().iter().any(|entry| matches!(
                    entry,
                    ModelContextEntry::CompletedCode(_) | ModelContextEntry::FinalMessage { .. }
                )));
                assert_eq!(
                    agent
                        .discard_staged_final_message()
                        .expect("completed final message should remain staged")
                        .text(),
                    "must not be admitted"
                );
            }
            let probe = agent
                .arena
                .execute("boundary_action_ran", &CancellationToken::new())
                .await
                .unwrap();
            assert!(matches!(probe.outcome, ExecutionOutcome::PythonError(_)));
        }
    }

    #[tokio::test]
    async fn agent_skips_a_pre_cancelled_run_without_changing_persistent_state() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "first_action = 1\nfirst_action".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let first_cancellation = CancellationToken::new();

        agent.run("first", &first_cancellation).await.unwrap();
        agent.admit_staged_final_message().unwrap();
        let persistent_context = agent.model_context.clone();
        let observed_contexts = agent.model.contexts().to_vec();
        let action_position = agent.model.position();
        let cancelled = CancellationToken::new();
        cancelled.cancel();

        let error = agent.run("cancelled", &cancelled).await.unwrap_err();

        assert!(matches!(error, AgentError::Cancelled { .. }));
        assert_eq!(agent.model_context, persistent_context);
        assert_eq!(agent.model.contexts(), observed_contexts.as_slice());
        assert_eq!(agent.model.position(), action_position);

        let (message, progress) =
            run_capturing_progress(&mut agent, "fresh", &CancellationToken::new())
                .await
                .unwrap();
        let executions = completed_executions(&progress);

        assert_eq!(message, "done");
        assert_eq!(executions.len(), 1);
        assert_eq!(executions[0].0, "first_action = 1\nfirst_action");
        assert_eq!(result_head(executions[0].1), "1");
    }

    #[tokio::test]
    async fn agent_returns_exhaustion_without_retrying_an_empty_script() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(&workspace, vec![]).await;
        let cancellation = CancellationToken::new();

        let error = agent.run("inspect", &cancellation).await.unwrap_err();

        assert!(matches!(
            error,
            AgentError::Model { failure, .. } if failure.error() == &ModelError::Exhausted
        ));
        assert_eq!(agent.model.contexts().len(), 1);
        assert_eq!(agent.model.position(), 0);
        assert_eq!(
            agent.model_context.entries(),
            [
                ModelContextEntry::ReadCapability {
                    description: READ_CAPABILITY_DESCRIPTION.to_owned(),
                },
                ModelContextEntry::UserRequest {
                    text: "inspect".to_owned(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn agent_cancellation_recovers_the_arena_without_consuming_later_actions() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "print('before')\nwhile True:\n    pass".to_owned(),
                },
                ModelAction::Code {
                    source: "later_action_ran = True".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let error = agent.run("cancel", &cancellation).await.unwrap_err();
        cancel.await.unwrap();

        let AgentError::Arena {
            error: crate::arena::ArenaError::Cancelled { execution },
            attempts,
        } = &error
        else {
            panic!("expected cancelled Arena evidence");
        };
        assert!(attempts.is_empty());
        assert_eq!(execution.stdout.head, "before\n");
        assert_eq!(
            agent
                .model_context
                .entries()
                .iter()
                .filter_map(ModelContextEntry::completed_code)
                .count(),
            1
        );
        let Some(ModelContextEntry::CompletedCode(completed)) =
            agent.model_context.entries().last()
        else {
            panic!("cancelled code should be admitted with a result");
        };
        let CodeResult::Cancelled { feedback } = completed.result() else {
            panic!("cancelled code should have a cancelled result");
        };
        assert!(feedback.contains("before\n"));
        assert!(feedback.contains(
            "Execution was cancelled and its Python arena changes were discarded. External side effects completed before cancellation may remain even though pending actions were cancelled."
        ));
        let recovered = agent
            .arena
            .execute("1 + 1", &CancellationToken::new())
            .await
            .expect("arena should recover before Agent returns cancellation");
        assert_eq!(result_head(&recovered), "2");
    }

    #[tokio::test]
    async fn agent_records_one_failure_result_after_an_arena_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![ModelAction::Code {
                source: "x = ' ' * (1 << 60)".to_owned(),
            }],
        )
        .await;
        let cancellation = CancellationToken::new();

        let error = agent.run("inspect", &cancellation).await.unwrap_err();

        assert!(matches!(
            error,
            AgentError::Arena {
                error: crate::arena::ArenaError::Infrastructure { .. },
                ..
            }
        ));
        let completed = agent
            .model_context
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        assert!(matches!(completed[0].result(), CodeResult::Failure { .. }));
    }

    #[tokio::test]
    async fn agent_returns_python_errors_as_feedback_and_executes_the_following_action() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "value = 1\nraise ValueError('expected')".to_owned(),
                },
                ModelAction::Code {
                    source: "value + 1".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        let (_, progress) = run_capturing_progress(&mut agent, "continue", &cancellation)
            .await
            .unwrap();
        let executions = completed_executions(&progress);

        assert_eq!(executions.len(), 2);
        assert!(matches!(
            executions[0].1.outcome,
            ExecutionOutcome::PythonError(_)
        ));
        assert_eq!(result_head(executions[1].1), "2");
        let completed = agent.model.contexts()[1]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .expect("following action should receive the Python error");
        let CodeResult::Execution { feedback } = completed.result() else {
            panic!("expected execution feedback");
        };
        assert!(feedback.contains("<kiki_error omitted_bytes=\"0\">"));
        assert!(feedback.contains("ValueError"));
    }

    #[tokio::test]
    async fn agent_admits_one_execution_result_for_success_and_python_error() {
        for source in ["1 + 1", "raise ValueError('expected')"] {
            let workspace = tempfile::tempdir().unwrap();
            let mut agent = agent(
                &workspace,
                vec![
                    ModelAction::Code {
                        source: source.to_owned(),
                    },
                    ModelAction::Message {
                        text: "done".to_owned(),
                    },
                ],
            )
            .await;
            let cancellation = CancellationToken::new();

            assert_eq!(
                agent.run("inspect", &cancellation).await.unwrap(),
                AgentRunOutcome::FinalMessageStaged
            );
            let completed = agent
                .model_context
                .entries()
                .iter()
                .filter_map(ModelContextEntry::completed_code)
                .collect::<Vec<_>>();

            assert_eq!(completed.len(), 1);
            assert!(matches!(
                completed[0].result(),
                CodeResult::Execution { .. }
            ));
            agent.admit_staged_final_message().unwrap();
        }
    }

    #[tokio::test]
    async fn agent_commits_completed_code_before_a_later_cancellation_check() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "value = 1".to_owned(),
                },
                ModelAction::Message {
                    text: "unreachable".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let error = {
            let mut progress = move |event| {
                if matches!(event, AgentProgress::ExecutionCompleted { .. }) {
                    canceller.cancel();
                }
            };
            agent
                .run_with_progress("inspect", &cancellation, &mut progress)
                .await
                .unwrap_err()
        };

        assert!(matches!(error, AgentError::Cancelled { .. }));
        let completed = agent
            .model_context
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        assert!(matches!(
            completed[0].result(),
            CodeResult::Execution { .. }
        ));
        assert!(
            !agent
                .model_context
                .entries()
                .iter()
                .any(|entry| matches!(entry, ModelContextEntry::FinalMessage { .. }))
        );
    }

    #[tokio::test]
    async fn earlier_completed_code_survives_a_later_staged_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "value = 1".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        assert_eq!(
            agent.run("inspect", &cancellation).await.unwrap(),
            AgentRunOutcome::FinalMessageStaged
        );
        cancellation.cancel();
        let discarded = agent
            .discard_staged_final_message()
            .expect("final message should remain staged");

        assert_eq!(discarded.text(), "done");
        assert!(matches!(
            agent.model_context.entries().last(),
            Some(ModelContextEntry::CompletedCode(completed))
                if matches!(completed.result(), CodeResult::Execution { .. })
        ));
        assert!(
            !agent
                .model_context
                .entries()
                .iter()
                .any(|entry| matches!(entry, ModelContextEntry::FinalMessage { .. }))
        );
    }

    #[test]
    fn agent_frames_stdout_stderr_and_result_in_order_without_escaping_raw_content() {
        let execution = Execution {
            stdout: observation(
                "<raw>&",
                None,
                ObservationMetadata {
                    head_lines: 1,
                    tail_lines: 0,
                    total_lines: 1,
                    total_bytes: 6,
                    omitted_lines: 0,
                    omitted_bytes: 0,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            stderr: observation(
                "a\nb\n",
                Some("d\n"),
                ObservationMetadata {
                    head_lines: 2,
                    tail_lines: 1,
                    total_lines: 4,
                    total_bytes: 8,
                    omitted_lines: 1,
                    omitted_bytes: 2,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            outcome: ExecutionOutcome::Complete {
                result: Some(observation(
                    "res<",
                    Some(">ult"),
                    ObservationMetadata {
                        head_lines: 1,
                        tail_lines: 1,
                        total_lines: 1,
                        total_bytes: 10,
                        omitted_lines: 0,
                        omitted_bytes: 2,
                        head_ends_mid_line: true,
                        tail_starts_mid_line: true,
                    },
                )),
            },
        };

        assert_eq!(
            super::execution_feedback(&execution),
            concat!(
                "<kiki_output stream=\"stdout\" truncated=\"false\" total_lines=\"1\" total_bytes=\"6\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"6\" ends_mid_line=\"false\">\n",
                "<raw>&</kiki_output_head>\n",
                "</kiki_output>\n",
                "<kiki_output stream=\"stderr\" truncated=\"true\" total_lines=\"4\" total_bytes=\"8\">\n",
                "<kiki_output_head lines=\"2\" bytes=\"4\" ends_mid_line=\"false\">\n",
                "a\nb\n</kiki_output_head>\n",
                "<kiki_output_omitted lines=\"1\" bytes=\"2\" partial_lines=\"false\" />\n",
                "<kiki_output_tail lines=\"1\" bytes=\"2\" starts_mid_line=\"false\">\n",
                "d\n</kiki_output_tail>\n",
                "</kiki_output>\n",
                "<kiki_warning code=\"output_truncated\" arena_state=\"committed\">\n",
                "Execution completed, but its model-visible output was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.\n",
                "</kiki_warning>\n",
                "<kiki_result truncated=\"true\" total_lines=\"1\" total_bytes=\"10\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"4\" ends_mid_line=\"true\">\n",
                "res<</kiki_output_head>\n",
                "<kiki_output_omitted lines=\"0\" bytes=\"2\" partial_lines=\"true\" />\n",
                "<kiki_output_tail lines=\"1\" bytes=\"4\" starts_mid_line=\"true\">\n",
                ">ult</kiki_output_tail>\n",
                "</kiki_result>\n",
                "<kiki_warning code=\"result_truncated\" arena_state=\"committed\">\n",
                "Execution completed, but its model-visible result was truncated. The underlying arena values were not changed. Print a smaller selection to inspect the omitted data.\n",
                "</kiki_warning>"
            )
        );
    }

    fn result_head(execution: &Execution) -> &str {
        let ExecutionOutcome::Complete {
            result: Some(result),
        } = &execution.outcome
        else {
            panic!("expected expression result, got {:?}", execution.outcome);
        };
        &result.head
    }

    #[tokio::test]
    async fn agent_default_script_reads_retained_lines_and_returns_its_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let cargo_toml = "[package]\nname = \"example\"\n";
        fs::write(workspace.path().join("Cargo.toml"), cargo_toml).unwrap();
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(arena, ScriptedModel::default());
        let cancellation = CancellationToken::new();

        let (message, progress) = run_capturing_progress(&mut agent, "inspect", &cancellation)
            .await
            .unwrap();
        let executions = completed_executions(&progress);

        assert_eq!(message, "I inspected Cargo.toml.");
        assert_eq!(executions.len(), 2);
        assert_eq!(
            executions[0].0,
            "try:\n    request_count += 1\nexcept NameError:\n    request_count = 1\nlines = await read(\"Cargo.toml\")"
        );
        assert_eq!(
            executions[1].0,
            "print(\"\".join(lines[0:20]), end=\"\")\nrequest_count"
        );
        assert_eq!(executions[1].1.stdout.head, cargo_toml);
        assert_eq!(result_head(executions[1].1), "1");
        assert_eq!(
            agent.model.contexts()[0][0],
            ModelContextEntry::ReadCapability {
                description: "read(path) returns the accepted file as complete exact lines with endings retained. Use standard Python print(\"\".join(lines[start:end]), end=\"\") to inspect a selection.".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn agent_replays_the_default_script_with_persistent_arena_state_and_context() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(arena, ScriptedModel::default());
        let cancellation = CancellationToken::new();

        let (_, first_progress) = run_capturing_progress(&mut agent, "first", &cancellation)
            .await
            .unwrap();
        let (_, second_progress) = run_capturing_progress(&mut agent, "second", &cancellation)
            .await
            .unwrap();
        let first_executions = completed_executions(&first_progress);
        let second_executions = completed_executions(&second_progress);

        assert_eq!(result_head(first_executions[1].1), "1");
        assert_eq!(result_head(second_executions[1].1), "2");
        let second_request_context = &agent.model.contexts()[3];
        assert!(second_request_context.iter().any(|entry| {
            matches!(
                entry,
                ModelContextEntry::FinalMessage { text, replay }
                    if text == "I inspected Cargo.toml."
                        && replay.provider() == "scripted"
                        && replay.items().len() == 1
            )
        }));
        assert_eq!(
            second_request_context.last(),
            Some(&ModelContextEntry::UserRequest {
                text: "second".to_owned(),
            })
        );
        let completed = agent.model.contexts()[5]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .expect("second final message should receive execution feedback");
        assert_eq!(completed.call_id().as_str(), "scripted-2-2");
        let CodeResult::Execution { feedback } = completed.result() else {
            panic!("expected execution feedback");
        };
        assert!(feedback.contains("<kiki_result truncated=\"false\" total_lines=\"1\" total_bytes=\"1\">\n<kiki_output_head lines=\"1\" bytes=\"1\" ends_mid_line=\"false\">\n2</kiki_output_head>\n</kiki_result>"));
    }

    #[test]
    fn agent_frames_python_errors_after_output_with_a_distinct_truncation_warning() {
        let execution = Execution {
            stdout: observation(
                "output\n",
                None,
                ObservationMetadata {
                    head_lines: 1,
                    tail_lines: 0,
                    total_lines: 1,
                    total_bytes: 7,
                    omitted_lines: 0,
                    omitted_bytes: 0,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            stderr: observation(
                "",
                None,
                ObservationMetadata {
                    head_lines: 0,
                    tail_lines: 0,
                    total_lines: 0,
                    total_bytes: 0,
                    omitted_lines: 0,
                    omitted_bytes: 0,
                    head_ends_mid_line: false,
                    tail_starts_mid_line: false,
                },
            ),
            outcome: ExecutionOutcome::PythonError(PythonError {
                text: "<error>".to_owned(),
                omitted_bytes: 3,
            }),
        };

        assert_eq!(
            super::execution_feedback(&execution),
            concat!(
                "<kiki_output stream=\"stdout\" truncated=\"false\" total_lines=\"1\" total_bytes=\"7\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"7\" ends_mid_line=\"false\">\n",
                "output\n</kiki_output_head>\n",
                "</kiki_output>\n",
                "<kiki_error omitted_bytes=\"3\">\n",
                "<error></kiki_error>\n",
                "<kiki_warning code=\"error_truncated\" arena_state=\"committed\">\n",
                "Execution completed with a Python error, but its model-visible error was truncated. The underlying arena values were not changed.\n",
                "</kiki_warning>"
            )
        );
    }
}
