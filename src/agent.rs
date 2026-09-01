use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::arena::{
    Arena, ArenaError, ArenaProgress, Execution, ExecutionOutcome, InterruptedExecution,
    Observation, PythonError, ToolCall,
};
use crate::config::constants::ACCEPTED_MODEL_CONTENT_LIMIT;
use crate::model::{
    AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelFailure, ModelHistory,
    ModelHistoryError, ModelProgress, ModelStep, ProviderReplay, READ_CAPABILITY_DESCRIPTION,
};
const FAILED_STEP_EVIDENCE_LIMIT: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvidence {
    Generation(GenerationEvidence),
    Execution(Box<ExecutionEvidence>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationEvidence {
    pub(crate) reasoning_summary: String,
    pub(crate) reasoning: String,
    pub(crate) assistant_text: String,
    pub(crate) decoded_python: String,
}

impl GenerationEvidence {
    fn is_empty(&self) -> bool {
        self.reasoning_summary.is_empty()
            && self.reasoning.is_empty()
            && self.assistant_text.is_empty()
            && self.decoded_python.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionEvidence {
    pub(crate) source: String,
    pub(crate) reasoning_summary: String,
    pub(crate) began: bool,
    pub(crate) stdout: Observation,
    pub(crate) stderr: Observation,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) cancellation_warning: Option<String>,
    pub(crate) failure: Option<ExecutionFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionFailure {
    Arena { message: String },
    RecoveryFailed { original: String, recovery: String },
}

impl ExecutionFailure {
    fn message(&self) -> String {
        match self {
            Self::Arena { message } => message.clone(),
            Self::RecoveryFailed { original, recovery } => {
                format!("arena recovery failed after {original}: {recovery}")
            }
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AgentProgress {
    ModelStepStarted,
    ModelProgress(ModelProgress),
    ExecutionStarted,
    ToolUpdated {
        call: ToolCall,
    },
    StdoutDelta {
        text: String,
    },
    StdoutTruncated,
    ExecutionCompleted {
        source: String,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
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
    generation: GenerationEvidence,
}

impl FinalMessage {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn attempts(&self) -> &[AttemptAccounting] {
        &self.attempts
    }

    pub(crate) fn reasoning_summary(&self) -> &str {
        &self.generation.reasoning_summary
    }

    pub(crate) fn generation_evidence(&self) -> TerminalEvidence {
        TerminalEvidence::Generation(self.generation.clone())
    }
}

struct StagedFinalMessage {
    final_message: FinalMessage,
    replay: ProviderReplay,
}

#[derive(Default)]
struct GenerationEvidenceBuilder {
    reasoning: String,
    assistant_text: String,
    decoded_python: String,
}

impl GenerationEvidenceBuilder {
    fn capture(&mut self, progress: &ModelProgress) {
        match progress {
            ModelProgress::AttemptStarted { .. } => *self = Self::default(),
            ModelProgress::ReasoningSummaryChunk { text } => self.append_reasoning(text),
            ModelProgress::AssistantTextChunk { text } => self.append_assistant_text(text),
            ModelProgress::DecodedPythonChunk { code } => self.append_decoded_python(code),
            ModelProgress::AttemptSettled { .. } => {}
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn evidence(self) -> Option<GenerationEvidence> {
        let evidence = GenerationEvidence {
            reasoning_summary: String::new(),
            reasoning: self.reasoning,
            assistant_text: self.assistant_text,
            decoded_python: self.decoded_python,
        };
        (!evidence.is_empty()).then_some(evidence)
    }

    fn with_final_message(mut self, text: String, reasoning_summary: String) -> GenerationEvidence {
        self.assistant_text = bounded_utf8(text, ACCEPTED_MODEL_CONTENT_LIMIT);
        let remaining = ACCEPTED_MODEL_CONTENT_LIMIT - self.assistant_text.len();
        self.reasoning = bounded_utf8(self.reasoning, remaining);
        let remaining = remaining - self.reasoning.len();
        self.decoded_python = bounded_utf8(self.decoded_python, remaining);
        GenerationEvidence {
            reasoning_summary,
            reasoning: self.reasoning,
            assistant_text: self.assistant_text,
            decoded_python: self.decoded_python,
        }
    }

    fn append_reasoning(&mut self, text: &str) {
        let remaining = self.remaining();
        Self::append(&mut self.reasoning, text, remaining);
    }

    fn append_assistant_text(&mut self, text: &str) {
        let remaining = self.remaining();
        Self::append(&mut self.assistant_text, text, remaining);
    }

    fn append_decoded_python(&mut self, code: &str) {
        let remaining = self.remaining();
        Self::append(&mut self.decoded_python, code, remaining);
    }

    fn remaining(&self) -> usize {
        ACCEPTED_MODEL_CONTENT_LIMIT.saturating_sub(
            self.reasoning.len() + self.assistant_text.len() + self.decoded_python.len(),
        )
    }

    fn append(target: &mut String, value: &str, limit: usize) {
        let end = utf8_prefix_len(value, limit);
        target.push_str(&value[..end]);
    }
}

impl ExecutionEvidence {
    fn cancelled(source: String, reasoning_summary: String, error: Option<&ArenaError>) -> Self {
        Self::from_error(source, reasoning_summary, error, true)
    }

    fn failed(source: String, reasoning_summary: String, error: &ArenaError) -> Self {
        Self::from_error(
            source,
            reasoning_summary,
            Some(error),
            contains_cancellation(error),
        )
    }

    fn from_error(
        source: String,
        reasoning_summary: String,
        error: Option<&ArenaError>,
        cancellation_warning: bool,
    ) -> Self {
        let interrupted = error
            .and_then(interrupted_execution)
            .cloned()
            .unwrap_or_else(empty_interrupted_execution);
        Self {
            source,
            reasoning_summary,
            began: interrupted.began,
            stdout: interrupted.stdout,
            stderr: interrupted.stderr,
            tool_calls: interrupted.tool_calls,
            cancellation_warning: cancellation_warning.then(|| CANCELLATION_WARNING.to_owned()),
            failure: error.map(failure_evidence),
        }
    }

    fn feedback(&self) -> String {
        let mut frames = Vec::new();
        if let Some(failure) = &self.failure {
            frames.push(format!(
                "<kiki_failure>\n{}\n</kiki_failure>",
                failure.message()
            ));
        }
        let state = if self.cancellation_warning.is_some() {
            "cancelled"
        } else {
            "failed"
        };
        frames.push(interruption_feedback(self, state));
        frames.join("\n")
    }
}

fn empty_interrupted_execution() -> InterruptedExecution {
    InterruptedExecution {
        began: false,
        stdout: empty_observation(),
        stderr: empty_observation(),
        tool_calls: Vec::new(),
    }
}

fn empty_observation() -> Observation {
    Observation {
        head: String::new(),
        tail: None,
        head_lines: 0,
        tail_lines: 0,
        total_lines: 0,
        total_bytes: 0,
        omitted_lines: 0,
        omitted_bytes: 0,
        head_ends_mid_line: false,
        tail_starts_mid_line: false,
        partial_lines: false,
    }
}

fn failure_evidence(error: &ArenaError) -> ExecutionFailure {
    match error {
        ArenaError::RecoveryFailed { original, recovery } => ExecutionFailure::RecoveryFailed {
            original: bounded_utf8(original.to_string(), FAILED_STEP_EVIDENCE_LIMIT / 2),
            recovery: bounded_utf8(recovery.to_string(), FAILED_STEP_EVIDENCE_LIMIT / 2),
        },
        _ => ExecutionFailure::Arena {
            message: bounded_utf8(error.to_string(), FAILED_STEP_EVIDENCE_LIMIT),
        },
    }
}

fn bounded_utf8(mut value: String, limit: usize) -> String {
    value.truncate(utf8_prefix_len(&value, limit));
    value
}

fn utf8_prefix_len(value: &str, limit: usize) -> usize {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent run was cancelled")]
    Cancelled {
        attempts: Vec<AttemptAccounting>,
        evidence: Option<Box<TerminalEvidence>>,
    },
    #[error("{failure}")]
    Model {
        #[source]
        failure: ModelFailure,
        attempts: Vec<AttemptAccounting>,
        evidence: Option<Box<TerminalEvidence>>,
    },
    #[error("{error}")]
    ModelHistory {
        #[source]
        error: ModelHistoryError,
        attempts: Vec<AttemptAccounting>,
        evidence: Option<Box<TerminalEvidence>>,
    },
    #[error("{error}")]
    Arena {
        #[source]
        error: ArenaError,
        attempts: Vec<AttemptAccounting>,
        evidence: Box<ExecutionEvidence>,
    },
    #[error("agent has no staged final message")]
    NoStagedFinalMessage,
    #[error("agent has a staged final message awaiting terminal settlement")]
    StagedFinalMessageUnresolved,
}

impl AgentError {
    pub(crate) fn is_recovery_failed(&self) -> bool {
        matches!(
            self,
            Self::Arena {
                error: ArenaError::RecoveryFailed { .. },
                ..
            }
        )
    }

    pub(crate) fn is_authentication(&self) -> bool {
        matches!(self, Self::Model { failure, .. } if failure.is_authentication())
    }

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
            Self::Cancelled { attempts, .. }
            | Self::Model { attempts, .. }
            | Self::ModelHistory { attempts, .. }
            | Self::Arena { attempts, .. } => attempts,
            Self::NoStagedFinalMessage | Self::StagedFinalMessageUnresolved => &[],
        }
    }

    pub(crate) fn terminal_evidence(&self) -> Option<TerminalEvidence> {
        match self {
            Self::Cancelled { evidence, .. }
            | Self::Model { evidence, .. }
            | Self::ModelHistory { evidence, .. } => evidence.as_deref().cloned(),
            Self::Arena { evidence, .. } => Some(TerminalEvidence::Execution(evidence.clone())),
            Self::NoStagedFinalMessage | Self::StagedFinalMessageUnresolved => None,
        }
    }
}

struct PendingCode {
    source: String,
    call_id: ModelCallId,
    reasoning_summary: String,
    replay: ProviderReplay,
}

impl PendingCode {
    fn append(
        self,
        history: &mut ModelHistory,
        result: CodeResult,
    ) -> Result<(), ModelHistoryError> {
        history.append_completed_code(
            self.source,
            self.call_id,
            self.reasoning_summary,
            self.replay,
            result,
        )
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
        self.model_context.append_final_message(
            final_message.text.clone(),
            final_message.reasoning_summary().to_owned(),
            replay,
        );
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
                evidence: None,
            });
        }
        self.model_context.append_user_request(request);
        let mut attempts = Vec::new();
        let mut generation = GenerationEvidenceBuilder::default();

        loop {
            if cancellation.is_cancelled() {
                return Err(AgentError::Cancelled {
                    attempts,
                    evidence: generation
                        .evidence()
                        .map(TerminalEvidence::Generation)
                        .map(Box::new),
                });
            }
            progress(AgentProgress::ModelStepStarted);
            let mut model_progress = |model_progress: ModelProgress| {
                generation.capture(&model_progress);
                progress(AgentProgress::ModelProgress(model_progress));
            };
            let model_step = match self
                .model
                .step(&self.model_context, cancellation, &mut model_progress)
                .await
            {
                Ok(step) => step,
                Err(failure) => {
                    attempts.extend(failure.attempts().iter().cloned());
                    return Err(AgentError::Model {
                        failure,
                        attempts,
                        evidence: generation
                            .evidence()
                            .map(TerminalEvidence::Generation)
                            .map(Box::new),
                    });
                }
            };
            let ModelStep {
                action,
                reasoning_summary,
                replay,
                attempts: step_attempts,
            } = model_step;
            attempts.extend(step_attempts.iter().cloned());
            match action {
                ModelAction::Code { source, call_id } => {
                    if self.model_context.contains_call_id(&call_id) {
                        return Err(AgentError::ModelHistory {
                            error: ModelHistoryError::DuplicateCallId(call_id),
                            attempts,
                            evidence: generation
                                .evidence()
                                .map(TerminalEvidence::Generation)
                                .map(Box::new),
                        });
                    }
                    generation.clear();
                    let pending = PendingCode {
                        source,
                        call_id,
                        reasoning_summary,
                        replay,
                    };
                    if cancellation.is_cancelled() {
                        let evidence = Box::new(ExecutionEvidence::cancelled(
                            pending.source.clone(),
                            pending.reasoning_summary.clone(),
                            None,
                        ));
                        if let Err(error) = pending.append(
                            &mut self.model_context,
                            CodeResult::cancelled(evidence.feedback()),
                        ) {
                            return Err(AgentError::ModelHistory {
                                error,
                                attempts,
                                evidence: Some(Box::new(TerminalEvidence::Execution(evidence))),
                            });
                        }
                        return Err(AgentError::Cancelled {
                            attempts,
                            evidence: Some(Box::new(TerminalEvidence::Execution(evidence))),
                        });
                    }
                    progress(AgentProgress::ExecutionStarted);
                    let execution = {
                        let mut arena_progress = |arena_progress| match arena_progress {
                            ArenaProgress::ToolUpdated { call } => {
                                progress(AgentProgress::ToolUpdated { call });
                            }
                            ArenaProgress::StdoutDelta { text } => {
                                progress(AgentProgress::StdoutDelta { text });
                            }
                            ArenaProgress::StdoutTruncated => {
                                progress(AgentProgress::StdoutTruncated);
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
                            let reasoning_summary = pending.reasoning_summary.clone();
                            if let Err(error) = pending.append(
                                &mut self.model_context,
                                CodeResult::execution(execution_feedback(&execution)),
                            ) {
                                return Err(AgentError::ModelHistory {
                                    error,
                                    attempts,
                                    evidence: None,
                                });
                            }
                            progress(AgentProgress::ExecutionCompleted {
                                source,
                                reasoning_summary,
                                execution,
                                attempts: step_attempts,
                            });
                        }
                        Err(error) => {
                            let cancelled = matches!(&error, ArenaError::Cancelled { .. });
                            let evidence = Box::new(if cancelled {
                                ExecutionEvidence::cancelled(
                                    pending.source.clone(),
                                    pending.reasoning_summary.clone(),
                                    Some(&error),
                                )
                            } else {
                                ExecutionEvidence::failed(
                                    pending.source.clone(),
                                    pending.reasoning_summary.clone(),
                                    &error,
                                )
                            });
                            let result = if cancelled {
                                CodeResult::cancelled(evidence.feedback())
                            } else {
                                CodeResult::failure(evidence.feedback())
                            };
                            if let Err(history_error) =
                                pending.append(&mut self.model_context, result)
                            {
                                return Err(AgentError::ModelHistory {
                                    error: history_error,
                                    attempts,
                                    evidence: Some(Box::new(TerminalEvidence::Execution(evidence))),
                                });
                            }
                            return Err(AgentError::Arena {
                                error,
                                attempts,
                                evidence,
                            });
                        }
                    }
                }
                ModelAction::Message { text } => {
                    self.staged_final_message = Some(StagedFinalMessage {
                        final_message: FinalMessage {
                            generation: generation
                                .with_final_message(text.clone(), reasoning_summary),
                            text,
                            attempts,
                        },
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

fn interruption_feedback(evidence: &ExecutionEvidence, state: &str) -> String {
    let mut frames = vec![format!(
        "<kiki_execution state=\"{state}\" began=\"{}\">",
        evidence.began
    )];
    if !evidence.began {
        frames.push("Execution did not begin.".to_owned());
    }
    append_output(&mut frames, "stdout", &evidence.stdout);
    append_output(&mut frames, "stderr", &evidence.stderr);
    if let Some(warning) = &evidence.cancellation_warning {
        frames.push(warning.clone());
    }
    frames.push("</kiki_execution>".to_owned());
    frames.join("\n")
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
    use std::sync::Arc;
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::sync::Notify;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        Agent, AgentError, AgentProgress, AgentRunOutcome, ExecutionFailure,
        GenerationEvidenceBuilder, TerminalEvidence,
    };
    use crate::arena::{
        Arena, ArenaError, Execution, ExecutionOutcome, Observation, PythonError, ToolCall,
        ToolCallSummary, ToolName,
    };
    use crate::model::{
        AttemptAccounting, CodeResult, Model, ModelAction as CompletedModelAction, ModelCallId,
        ModelContextEntry, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ProviderFailure, ProviderReplay, READ_CAPABILITY_DESCRIPTION,
        ScriptedAction as ModelAction, ScriptedModel,
    };
    use crate::tools::{ReadTestGate, ReadTool};

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
                AgentProgress::ExecutionCompleted {
                    source, execution, ..
                } => Some((source.as_str(), execution)),
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
        let mut agent = agent_with_model(&workspace, SummaryMessageModel).await;
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
        assert_eq!(discarded.reasoning_summary(), "message summary");
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

    struct CompletedSummaryModel {
        step: u8,
    }

    impl Model for CompletedSummaryModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let (action, reasoning_summary) = match self.step {
                    0 => (
                        CompletedModelAction::Code {
                            source: "1 + 1".to_owned(),
                            call_id: ModelCallId::new("summary-code"),
                        },
                        "code summary".to_owned(),
                    ),
                    1 => (
                        CompletedModelAction::Message {
                            text: "done".to_owned(),
                        },
                        "message summary".to_owned(),
                    ),
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary,
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn agent_admits_completed_summaries_without_streamed_progress() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent_with_model(&workspace, CompletedSummaryModel { step: 0 }).await;
        let cancellation = CancellationToken::new();
        let mut progress = Vec::new();

        {
            let mut capture = |event| progress.push(event);
            assert_eq!(
                agent
                    .run_with_progress("inspect", &cancellation, &mut capture)
                    .await
                    .unwrap(),
                AgentRunOutcome::FinalMessageStaged
            );
        }

        assert!(
            !progress
                .iter()
                .any(|event| matches!(event, AgentProgress::ModelProgress(_)))
        );
        let execution_reasoning_summary = progress
            .iter()
            .find_map(|event| match event {
                AgentProgress::ExecutionCompleted {
                    reasoning_summary, ..
                } => Some(reasoning_summary.as_str()),
                _ => None,
            })
            .expect("completed execution should carry its summary");
        assert_eq!(execution_reasoning_summary, "code summary");
        let completed = agent
            .model_context
            .entries()
            .iter()
            .find_map(ModelContextEntry::completed_code)
            .expect("completed code should be admitted with its result");
        assert_eq!(completed.call_id().as_str(), "summary-code");
        assert_eq!(completed.reasoning_summary(), "code summary");
        assert!(matches!(completed.result(), CodeResult::Execution { .. }));
        assert!(
            !agent
                .model_context
                .entries()
                .iter()
                .any(|entry| matches!(entry, ModelContextEntry::FinalMessage { .. }))
        );

        let final_message = agent.admit_staged_final_message().unwrap();
        assert_eq!(final_message.text(), "done");
        assert_eq!(final_message.reasoning_summary(), "message summary");
        assert!(matches!(
            agent.model_context.entries().last(),
            Some(ModelContextEntry::FinalMessage {
                text,
                reasoning_summary,
                ..
            }) if text == "done" && reasoning_summary == "message summary"
        ));
    }

    struct SummaryMessageModel;

    impl Model for SummaryMessageModel {
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
                    reasoning_summary: "message summary".to_owned(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct SummaryCodeModel {
        source: String,
        reasoning_summary: String,
    }

    impl Model for SummaryCodeModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                Ok(ModelStep {
                    action: CompletedModelAction::Code {
                        source: self.source.clone(),
                        call_id: ModelCallId::new("summary-code"),
                    },
                    reasoning_summary: self.reasoning_summary.clone(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
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
                    reasoning_summary: String::new(),
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
                progress(ModelProgress::ReasoningSummaryChunk {
                    text: "partial reasoning".to_owned(),
                });
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

    #[test]
    fn agent_error_delegates_only_model_authentication_failures() {
        for (status, expected) in [(401, true), (403, false), (500, false)] {
            let error = AgentError::Model {
                failure: ModelFailure::new(
                    ModelError::Provider(Box::new(ProviderFailure::new(
                        Some(status),
                        None,
                        "provider failed".to_owned(),
                    ))),
                    Vec::new(),
                ),
                attempts: Vec::new(),
                evidence: None,
            };

            assert_eq!(error.is_authentication(), expected);
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
        let AgentError::Model {
            evidence: Some(evidence),
            ..
        } = error
        else {
            panic!("provider failure should retain its partial generation evidence");
        };
        let TerminalEvidence::Generation(evidence) = *evidence else {
            panic!("provider failure should retain generation evidence");
        };
        assert_eq!(evidence.reasoning, "partial reasoning");
        assert_eq!(evidence.assistant_text, "partial answer");
        assert!(evidence.reasoning_summary.is_empty());
        assert!(!failed.model_context.entries().iter().any(|entry| matches!(
            entry,
            ModelContextEntry::CompletedCode(_) | ModelContextEntry::FinalMessage { .. }
        )));
    }

    #[tokio::test]
    async fn agent_forwards_live_stdout_without_adding_it_to_model_progress() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "print('live stdout')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();
        let mut progress = Vec::new();

        {
            let mut capture = |event| progress.push(event);
            assert_eq!(
                agent
                    .run_with_progress("inspect", &cancellation, &mut capture)
                    .await
                    .unwrap(),
                AgentRunOutcome::FinalMessageStaged
            );
        }

        let stdout = progress
            .iter()
            .filter_map(|event| match event {
                AgentProgress::StdoutDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(stdout, "live stdout\n");
        assert!(!progress.iter().any(|event| matches!(
            event,
            AgentProgress::ModelProgress(_) | AgentProgress::StdoutTruncated
        )));
        let completed = agent
            .model_context
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .next()
            .expect("completed code should remain the only stdout context source");
        let CodeResult::Execution { feedback } = completed.result() else {
            panic!("expected execution feedback");
        };
        assert!(feedback.contains("live stdout\n"));
    }

    #[tokio::test]
    async fn agent_forwards_model_progress_to_its_progress_sink() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent_with_model(&workspace, PartialFailureModel).await;
        let cancellation = CancellationToken::new();
        let mut progress = Vec::new();

        let error = {
            let mut capture = |event| progress.push(event);
            agent
                .run_with_progress("inspect", &cancellation, &mut capture)
                .await
                .unwrap_err()
        };

        assert!(matches!(error, AgentError::Model { .. }));
        assert_eq!(
            progress.len(),
            3,
            "model progress should reach the agent sink"
        );
    }

    struct RetriedGenerationCancellationModel {
        ready: Arc<Notify>,
    }

    impl Model for RetriedGenerationCancellationModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                progress(ModelProgress::AttemptStarted { attempt: 1 });
                progress(ModelProgress::ReasoningSummaryChunk {
                    text: "discarded reasoning".to_owned(),
                });
                progress(ModelProgress::AssistantTextChunk {
                    text: "discarded text".to_owned(),
                });
                progress(ModelProgress::AttemptStarted { attempt: 2 });
                progress(ModelProgress::ReasoningSummaryChunk {
                    text: "retained reasoning".to_owned(),
                });
                progress(ModelProgress::AssistantTextChunk {
                    text: "retained text".to_owned(),
                });
                progress(ModelProgress::DecodedPythonChunk {
                    code: "print('retained')".to_owned(),
                });
                self.ready.notify_one();
                cancellation.cancelled().await;
                Err(ModelError::Cancelled.into())
            }
        }
    }

    #[tokio::test]
    async fn cancellation_retains_only_the_active_attempt_generation() {
        let workspace = tempfile::tempdir().unwrap();
        let ready = Arc::new(Notify::new());
        let mut agent = agent_with_model(
            &workspace,
            RetriedGenerationCancellationModel {
                ready: ready.clone(),
            },
        )
        .await;
        let cancellation = CancellationToken::new();
        let cancel_when_ready = async {
            ready.notified().await;
            cancellation.cancel();
        };

        let ((), error) = tokio::join!(cancel_when_ready, agent.run("inspect", &cancellation));
        let AgentError::Model {
            evidence: Some(evidence),
            ..
        } = error.unwrap_err()
        else {
            panic!("expected retained generation evidence");
        };
        let TerminalEvidence::Generation(evidence) = *evidence else {
            panic!("expected generation evidence");
        };

        assert_eq!(evidence.reasoning, "retained reasoning");
        assert_eq!(evidence.assistant_text, "retained text");
        assert_eq!(evidence.decoded_python, "print('retained')");
    }

    #[test]
    fn recovery_failure_evidence_bounds_original_and_recovery_diagnostics_separately() {
        let error = ArenaError::RecoveryFailed {
            original: Box::new(ArenaError::Infrastructure {
                message: "é".repeat(super::FAILED_STEP_EVIDENCE_LIMIT),
                execution: None,
            }),
            recovery: Box::new(ArenaError::Infrastructure {
                message: "é".repeat(super::FAILED_STEP_EVIDENCE_LIMIT),
                execution: None,
            }),
        };

        let ExecutionFailure::RecoveryFailed { original, recovery } =
            super::failure_evidence(&error)
        else {
            panic!("recovery error must retain typed diagnostics");
        };

        assert!(original.len() <= super::FAILED_STEP_EVIDENCE_LIMIT / 2);
        assert!(original.is_char_boundary(original.len()));
        assert!(recovery.len() <= super::FAILED_STEP_EVIDENCE_LIMIT / 2);
        assert!(recovery.is_char_boundary(recovery.len()));
    }

    #[test]
    fn execution_evidence_copies_original_tool_calls_after_recovery_failure() {
        let mut interrupted = super::empty_interrupted_execution();
        interrupted.tool_calls = vec![
            ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(3),
            },
            ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Failed,
            },
        ];
        let error = ArenaError::RecoveryFailed {
            original: Box::new(ArenaError::Cancelled {
                execution: Box::new(interrupted),
            }),
            recovery: Box::new(ArenaError::Infrastructure {
                message: "recovery failed".to_owned(),
                execution: None,
            }),
        };

        let evidence = super::ExecutionEvidence::failed("source".to_owned(), String::new(), &error);

        assert_eq!(
            evidence.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(3),
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Failed,
                },
            ]
        );
        assert!(!evidence.feedback().contains("tool calls"));
    }

    #[test]
    fn generation_evidence_is_utf8_safe_at_the_accepted_content_limit() {
        let mut generation = GenerationEvidenceBuilder::default();
        generation.capture(&ModelProgress::AssistantTextChunk {
            text: "é".repeat(crate::config::constants::ACCEPTED_MODEL_CONTENT_LIMIT / 2 + 1),
        });
        let evidence = generation.evidence().expect("generation evidence");

        assert_eq!(
            evidence.assistant_text,
            "é".repeat(crate::config::constants::ACCEPTED_MODEL_CONTENT_LIMIT / 2)
        );
        assert!(
            evidence
                .assistant_text
                .is_char_boundary(evidence.assistant_text.len())
        );
    }

    #[tokio::test]
    async fn agent_discards_a_lost_final_before_the_next_request() {
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
            agent.run("discarded", &cancellation).await.unwrap(),
            AgentRunOutcome::FinalMessageStaged
        );
        assert_eq!(
            agent
                .discard_staged_final_message()
                .expect("staged final")
                .text(),
            "done"
        );
        assert_eq!(
            agent.run("later", &cancellation).await.unwrap(),
            AgentRunOutcome::FinalMessageStaged
        );
        assert_eq!(agent.admit_staged_final_message().unwrap().text(), "done");
        assert_eq!(
            agent
                .model_context
                .entries()
                .iter()
                .filter(|entry| matches!(entry, ModelContextEntry::FinalMessage { .. }))
                .count(),
            1
        );
        assert!(agent.model_context.entries().iter().any(|entry| matches!(
            entry,
            ModelContextEntry::UserRequest { text } if text == "discarded"
        )));
    }

    #[tokio::test]
    async fn agent_uses_interrupted_execution_evidence_for_its_cancelled_code_result() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let mut agent = agent_with_model(
            &workspace,
            SummaryCodeModel {
                source: "lines = await read('Cargo.toml')".to_owned(),
                reasoning_summary: "cancelled code summary".to_owned(),
            },
        )
        .await;
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let error = {
            let mut progress = |event| {
                if matches!(event, AgentProgress::ToolUpdated { .. }) {
                    canceller.cancel();
                }
            };
            agent
                .run_with_progress("inspect", &cancellation, &mut progress)
                .await
                .unwrap_err()
        };

        let AgentError::Arena {
            error: crate::arena::ArenaError::Cancelled { execution },
            evidence,
            ..
        } = error
        else {
            panic!("expected cancelled execution");
        };
        assert_eq!(evidence.source, "lines = await read('Cargo.toml')");
        assert_eq!(evidence.reasoning_summary, "cancelled code summary");
        assert_eq!(evidence.began, execution.began);
        assert_eq!(evidence.stdout, execution.stdout);
        assert_eq!(evidence.stderr, execution.stderr);
        assert_eq!(evidence.tool_calls, execution.tool_calls);
        assert_eq!(
            evidence.cancellation_warning.as_deref(),
            Some(super::CANCELLATION_WARNING)
        );
        let Some(ModelContextEntry::CompletedCode(completed)) =
            agent.model_context.entries().last()
        else {
            panic!("cancelled code should have one compound history result");
        };
        let CodeResult::Cancelled { feedback } = completed.result() else {
            panic!("cancelled execution should have a cancelled code result");
        };
        assert_eq!(completed.reasoning_summary(), "cancelled code summary");
        assert_eq!(feedback, &evidence.feedback());
        assert!(!feedback.contains("cancelled code summary"));
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
                    reasoning_summary: "boundary summary".to_owned(),
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
                let AgentError::Cancelled {
                    evidence: Some(evidence),
                    ..
                } = result.unwrap_err()
                else {
                    panic!("cancelled code should retain execution evidence");
                };
                let TerminalEvidence::Execution(evidence) = *evidence else {
                    panic!("cancelled code should retain execution evidence");
                };
                assert_eq!(evidence.source, "boundary_action_ran = True");
                assert_eq!(evidence.reasoning_summary, "boundary summary");
                assert!(!evidence.began);
                assert!(evidence.tool_calls.is_empty());
                assert_eq!(
                    evidence.cancellation_warning.as_deref(),
                    Some(super::CANCELLATION_WARNING)
                );
                let Some(ModelContextEntry::CompletedCode(completed)) =
                    agent.model_context.entries().last()
                else {
                    panic!("cancelled code should be admitted with a result");
                };
                let CodeResult::Cancelled { feedback } = completed.result() else {
                    panic!("cancelled code should have a cancelled result");
                };
                assert_eq!(completed.reasoning_summary(), "boundary summary");
                assert_eq!(feedback, &evidence.feedback());
                assert!(!feedback.contains("boundary summary"));
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
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let gate = ReadTestGate::new();
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool")
                .with_test_gate(gate.clone()),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "print('before')\nlines = await read('Cargo.toml')".to_owned(),
                },
                ModelAction::Code {
                    source: "later_action_ran = True".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ]),
        );
        let cancellation = CancellationToken::new();
        let cancel_when_read_starts = async {
            gate.wait_until_read_started().await;
            cancellation.cancel();
        };

        let ((), error) = tokio::join!(cancel_when_read_starts, agent.run("cancel", &cancellation));
        let error = error.unwrap_err();

        let AgentError::Arena {
            error: crate::arena::ArenaError::Cancelled { execution },
            attempts,
            evidence,
        } = &error
        else {
            panic!("expected cancelled Arena evidence");
        };
        assert!(attempts.is_empty());
        assert_eq!(execution.stdout.head, "before\n");
        assert_eq!(
            evidence.source,
            "print('before')\nlines = await read('Cargo.toml')"
        );
        assert_eq!(evidence.stdout, execution.stdout);
        assert_eq!(evidence.stderr, execution.stderr);
        assert_eq!(evidence.tool_calls, execution.tool_calls);
        assert_eq!(
            evidence.cancellation_warning.as_deref(),
            Some(super::CANCELLATION_WARNING)
        );
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
        assert_eq!(feedback, &evidence.feedback());
        assert!(!feedback.contains("tool calls"));
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
        let mut agent = agent_with_model(
            &workspace,
            SummaryCodeModel {
                source: "x = ' ' * (1 << 60)".to_owned(),
                reasoning_summary: "failed code summary".to_owned(),
            },
        )
        .await;
        let cancellation = CancellationToken::new();

        let error = agent.run("inspect", &cancellation).await.unwrap_err();

        let AgentError::Arena {
            error: crate::arena::ArenaError::Infrastructure { .. },
            evidence,
            ..
        } = error
        else {
            panic!("expected arena infrastructure failure");
        };
        assert_eq!(evidence.source, "x = ' ' * (1 << 60)");
        assert_eq!(evidence.reasoning_summary, "failed code summary");
        assert!(evidence.began);
        assert!(matches!(
            evidence.failure,
            Some(ExecutionFailure::Arena { .. })
        ));
        let completed = agent
            .model_context
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        let CodeResult::Failure { feedback } = completed[0].result() else {
            panic!("arena failure should produce one failed code result");
        };
        assert_eq!(completed[0].reasoning_summary(), "failed code summary");
        assert_eq!(feedback, &evidence.feedback());
        assert!(!feedback.contains("failed code summary"));
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
        let mut agent = agent_with_model(&workspace, CompletedSummaryModel { step: 0 }).await;
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let mut progress = Vec::new();
        let error = {
            let mut capture = |event| {
                if matches!(event, AgentProgress::ExecutionCompleted { .. }) {
                    canceller.cancel();
                }
                progress.push(event);
            };
            agent
                .run_with_progress("inspect", &cancellation, &mut capture)
                .await
                .unwrap_err()
        };

        assert!(matches!(
            error,
            AgentError::Cancelled { evidence: None, .. }
        ));
        assert!(matches!(
            progress.as_slice(),
            [
                AgentProgress::ModelStepStarted,
                AgentProgress::ExecutionStarted,
                AgentProgress::ExecutionCompleted {
                    reasoning_summary,
                    ..
                },
            ] if reasoning_summary == "code summary"
        ));
        let completed = agent
            .model_context
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].reasoning_summary(), "code summary");
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
            tool_calls: Vec::new(),
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
                ModelContextEntry::FinalMessage {
                    text,
                    reasoning_summary: _,
                    replay,
                } if text == "I inspected Cargo.toml."
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
            tool_calls: Vec::new(),
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
