use std::fmt;
use std::path::PathBuf;

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::arena::{
    Arena, ArenaError, ArenaProgress, Checkpoint, Execution, ExecutionOutcome,
    InterruptedExecution, Observation, PythonError, RunPreview, ToolCall, ToolCallToken,
};
use crate::config::{
    CompactionSettings, ModelSelection, constants::ACCEPTED_MODEL_CONTENT_LIMIT,
    settings::CrossProviderReasoning,
};
use crate::model::{
    AttemptAccounting, CodeResult, Model, ModelAction, ModelCallId, ModelError, ModelFailure,
    ModelHistory, ModelHistoryError, ModelProgress, ModelStep, ProviderFailure, ProviderReplay,
    SkillActivation,
};
use crate::session::{
    CommittedCompaction, CompactionOutcome, CompactionReason, CompactionSettlement, PersistedCode,
    PersistedTerminal, SessionEntryId, SessionError, SessionWriter,
};
use crate::subagents::AgentInvocationContext;
const FAILED_STEP_EVIDENCE_LIMIT: usize = 4 * 1024;
const FAILED_RUN_MESSAGE_LIMIT: usize = 4 * 1024;
const OVERFLOW_MESSAGE_COMPONENT_LIMIT: usize = 2_000;
const NO_REMOVABLE_PREFIX: &str = "compaction could not remove a valid prefix";
const SUMMARY_NOT_STARTED: &str = "summary finished without starting a provider attempt";

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverflowRecovery {
    pub(crate) failure: ProviderFailure,
    pub(crate) generation: Option<GenerationEvidence>,
    pub(crate) attempts: Vec<AttemptAccounting>,
}

impl GenerationEvidence {
    fn is_empty(&self) -> bool {
        self.reasoning_summary.is_empty()
            && self.reasoning.is_empty()
            && self.assistant_text.is_empty()
            && self.decoded_python.is_empty()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExecutionEvidence {
    pub(crate) source: String,
    pub(crate) commentary: Option<String>,
    pub(crate) reasoning_summary: String,
    pub(crate) began: bool,
    pub(crate) stdout: Observation,
    pub(crate) stderr: Observation,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) modified_paths: Vec<PathBuf>,
    pub(crate) cancellation_warning: Option<String>,
    pub(crate) failure: Option<ExecutionFailure>,
}

impl fmt::Debug for ExecutionEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionEvidence")
            .field("source", &self.source)
            .field("commentary", &self.commentary)
            .field("reasoning_summary", &self.reasoning_summary)
            .field("began", &self.began)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("tool_calls", &self.tool_calls)
            .field("cancellation_warning", &self.cancellation_warning)
            .field("failure", &self.failure)
            .finish()
    }
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
    ExecutionStarted {
        commentary: Option<String>,
    },
    ToolUpdated {
        token: ToolCallToken,
        call: ToolCall,
    },
    RunPreviewUpdated {
        token: ToolCallToken,
        preview: RunPreview,
    },
    StdoutDelta {
        text: String,
    },
    StdoutTruncated,
    ExecutionCompleted {
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    },
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum PersistedAgentEvent {
    RequestCommitted {
        run_id: u64,
        request_entry: SessionEntryId,
        request: String,
        activation: Option<SkillActivation>,
    },
    CompactionStarted {
        run_id: u64,
        initial_overflow: Option<(u64, Vec<AttemptAccounting>)>,
    },
    CompactionCommitted {
        compaction: CommittedCompaction,
    },
    ModelStepStarted {
        run_id: u64,
        step_id: u64,
        epoch: u32,
    },
    GenerationRestarted {
        run_id: u64,
        step_id: u64,
        epoch: u32,
    },
    ModelProgress {
        run_id: u64,
        step_id: u64,
        epoch: u32,
        progress: ModelProgress,
    },
    ExecutionStarted {
        run_id: u64,
        step_id: u64,
        commentary: Option<String>,
    },
    ToolUpdated {
        run_id: u64,
        step_id: u64,
        token: ToolCallToken,
        call: ToolCall,
    },
    RunPreviewUpdated {
        run_id: u64,
        step_id: u64,
        token: ToolCallToken,
        preview: RunPreview,
    },
    StdoutDelta {
        run_id: u64,
        step_id: u64,
        text: String,
    },
    StdoutTruncated {
        run_id: u64,
        step_id: u64,
    },
    ExecutionCommitted {
        run_id: u64,
        step_id: u64,
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
        activations: Vec<SkillActivation>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PersistedRunOutcome {
    pub(crate) run_id: u64,
    pub(crate) terminal: PersistedTerminal,
    pub(crate) authentication_failure: bool,
    pub(crate) recovery_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactionResult {
    NoOp,
    CancelledBeforeStart,
    FailedBeforeStart(String),
    Settled(CommittedCompaction),
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
    fn cancelled(
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        error: Option<&ArenaError>,
    ) -> Self {
        Self::from_error(source, commentary, reasoning_summary, error, true)
    }

    fn failed(
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        error: &ArenaError,
    ) -> Self {
        Self::from_error(
            source,
            commentary,
            reasoning_summary,
            Some(error),
            contains_cancellation(error),
        )
    }

    fn from_error(
        source: String,
        commentary: Option<String>,
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
            commentary,
            reasoning_summary,
            began: interrupted.began,
            stdout: interrupted.stdout,
            stderr: interrupted.stderr,
            tool_calls: interrupted.tool_calls,
            read_paths: interrupted.read_paths,
            modified_paths: interrupted.modified_paths,
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
        read_paths: Vec::new(),
        modified_paths: Vec::new(),
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
}

impl AgentError {
    #[cfg(test)]
    pub(crate) fn is_authentication(&self) -> bool {
        matches!(self, Self::Model { failure, .. } if failure.is_authentication())
    }

    pub fn attempts(&self) -> &[AttemptAccounting] {
        match self {
            Self::Cancelled { attempts, .. }
            | Self::Model { attempts, .. }
            | Self::ModelHistory { attempts, .. }
            | Self::Arena { attempts, .. } => attempts,
        }
    }
}

#[derive(Clone)]
struct PendingCode {
    source: String,
    commentary: Option<String>,
    selection: ModelSelection,
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
            self.commentary,
            self.reasoning_summary,
            self.replay,
            result,
        )
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ArenaRestoreTestGate {
    attempts: std::sync::Arc<std::sync::Mutex<Vec<Checkpoint>>>,
    failed_attempt: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    paused_attempt: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    paused: std::sync::Arc<tokio::sync::Semaphore>,
    resume: std::sync::Arc<tokio::sync::Semaphore>,
}

#[cfg(test)]
impl ArenaRestoreTestGate {
    pub(crate) fn new() -> Self {
        Self {
            attempts: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            failed_attempt: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            paused_attempt: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            paused: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
            resume: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }

    pub(crate) fn fail_attempt(&self, attempt: usize) {
        self.failed_attempt
            .store(attempt, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) fn pause_attempt(&self, attempt: usize) {
        self.paused_attempt
            .store(attempt, std::sync::atomic::Ordering::SeqCst);
    }

    pub(crate) async fn wait_until_paused(&self) {
        self.paused
            .clone()
            .acquire_owned()
            .await
            .expect("restore gate semaphore remains open")
            .forget();
    }

    pub(crate) fn resume(&self) {
        self.resume.add_permits(1);
    }

    pub(crate) fn attempts(&self) -> Vec<Checkpoint> {
        self.attempts.lock().expect("restore gate lock").clone()
    }

    async fn record(&self, checkpoint: Checkpoint) -> bool {
        let attempt = {
            let mut attempts = self.attempts.lock().expect("restore gate lock");
            attempts.push(checkpoint);
            attempts.len()
        };
        if self
            .paused_attempt
            .load(std::sync::atomic::Ordering::SeqCst)
            == attempt
        {
            self.paused.add_permits(1);
            self.resume
                .clone()
                .acquire_owned()
                .await
                .expect("restore gate semaphore remains open")
                .forget();
        }
        self.failed_attempt
            .load(std::sync::atomic::Ordering::SeqCst)
            == attempt
    }
}

pub struct Agent<M: Model> {
    arena: Arena,
    model: M,
    invocation_context: Option<AgentInvocationContext>,
    #[cfg(test)]
    restore_test_gate: Option<ArenaRestoreTestGate>,
}

impl<M: Model> Agent<M> {
    pub fn new(arena: Arena, model: M) -> Self {
        Self {
            arena,
            model,
            invocation_context: None,
            #[cfg(test)]
            restore_test_gate: None,
        }
    }

    pub(crate) fn checkpoint(&self) -> Checkpoint {
        self.arena.checkpoint()
    }

    #[cfg(test)]
    pub(crate) fn with_restore_test_gate(mut self, gate: ArenaRestoreTestGate) -> Self {
        self.restore_test_gate = Some(gate);
        self
    }

    pub(crate) fn validate_restored_history(
        &self,
        history: &ModelHistory,
        selection: ModelSelection,
    ) -> Result<(), ModelError> {
        self.model.validate_history(history, selection)
    }

    pub(crate) async fn restore_arena(&mut self, checkpoint: Checkpoint) -> Result<(), ArenaError> {
        #[cfg(test)]
        if let Some(gate) = &self.restore_test_gate
            && gate.record(checkpoint.clone()).await
        {
            return Err(ArenaError::Infrastructure {
                message: "test arena restore failure".to_owned(),
                execution: None,
            });
        }
        self.arena.restore(checkpoint).await
    }

    pub(crate) fn set_invocation_context(&mut self, context: AgentInvocationContext) {
        self.invocation_context = Some(context);
    }

    pub(crate) async fn collect_child_result_values(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<
        std::collections::BTreeMap<String, crate::tools::ToolValue>,
        crate::arena::ResultValuesError,
    > {
        self.arena
            .collect_result_values_with_cancellation(cancellation)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn compact(
        &mut self,
        reason: CompactionReason,
        policy: CompactionSettings,
        selection: ModelSelection,
        reasoning: CrossProviderReasoning,
        focus: String,
        cancellation: &CancellationToken,
        writer: &SessionWriter,
        started: &mut (impl FnMut() + Send),
    ) -> Result<CompactionResult, SessionError> {
        let Some(preparation) = writer
            .prepare_compaction(policy, selection, reasoning, focus, reason)
            .await?
        else {
            return Ok(CompactionResult::NoOp);
        };

        let mut attempts = Vec::new();
        let mut outputs = Vec::new();
        let mut failure = None;
        let mut provider_started = false;
        for request in preparation.requests().iter().cloned() {
            if cancellation.is_cancelled() {
                break;
            }
            let mut attempt_started = || {
                if !provider_started {
                    provider_started = true;
                    started();
                }
            };
            match self
                .model
                .summarise(request, cancellation, &mut attempt_started)
                .await
            {
                Ok(result) => {
                    attempts.extend(result.attempts);
                    outputs.push(result.text);
                }
                Err(error) => {
                    attempts.extend(error.attempts().iter().cloned());
                    failure = Some(error.to_string());
                    break;
                }
            }
        }

        if !provider_started {
            return Ok(if cancellation.is_cancelled() {
                CompactionResult::CancelledBeforeStart
            } else {
                CompactionResult::FailedBeforeStart(
                    failure.unwrap_or_else(|| SUMMARY_NOT_STARTED.to_owned()),
                )
            });
        }

        let settlement = if cancellation.is_cancelled() {
            CompactionSettlement::Cancelled { attempts }
        } else if let Some(diagnostic) = failure {
            CompactionSettlement::Failed {
                diagnostic,
                attempts,
            }
        } else {
            match preparation.finish(&outputs) {
                Ok(summary) => CompactionSettlement::Applied { summary, attempts },
                Err(error) => CompactionSettlement::Failed {
                    diagnostic: error.to_string(),
                    attempts,
                },
            }
        };
        let committed = writer.settle_compaction(preparation, settlement).await?;
        Ok(CompactionResult::Settled(committed))
    }

    #[cfg(test)]
    pub(crate) async fn run(
        &mut self,
        selection: ModelSelection,
        request: impl Into<String>,
        cancellation: &CancellationToken,
    ) -> Result<FinalMessage, AgentError> {
        let mut progress = |_| {};
        self.run_with_progress(selection, request, cancellation, &mut progress)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_persisted(
        &mut self,
        selection: ModelSelection,
        policy: CompactionSettings,
        reasoning: CrossProviderReasoning,
        request: String,
        activation: Option<SkillActivation>,
        cancellation: &CancellationToken,
        writer: &SessionWriter,
        event: &mut (impl FnMut(PersistedAgentEvent) + Send),
    ) -> Result<PersistedRunOutcome, SessionError> {
        let admission = writer
            .commit_request(request.clone(), selection, activation.clone())
            .await?;
        let run_id = admission.run_id;
        let activation_written = admission.effective_entry != admission.request_entry;
        event(PersistedAgentEvent::RequestCommitted {
            run_id,
            request_entry: admission.request_entry,
            request,
            activation: activation.filter(|_| activation_written),
        });

        let mut attempts = Vec::new();
        let mut committed_attempts = 0;
        let mut generation = GenerationEvidenceBuilder::default();
        let mut pending_step_id = None;
        let mut overflow_recovery = None;

        loop {
            if !cancellation.is_cancelled() {
                let result = self
                    .compact(
                        CompactionReason::AutomaticThreshold,
                        policy,
                        selection,
                        reasoning,
                        String::new(),
                        cancellation,
                        writer,
                        &mut || {
                            event(PersistedAgentEvent::CompactionStarted {
                                run_id,
                                initial_overflow: None,
                            })
                        },
                    )
                    .await?;
                if let CompactionResult::Settled(compaction) = result {
                    event(PersistedAgentEvent::CompactionCommitted { compaction });
                }
            }
            let step_id = if cancellation.is_cancelled() {
                None
            } else {
                let allocation_cancellation = cancellation.clone();
                writer
                    .start_model_step_unless(run_id, move || allocation_cancellation.is_cancelled())
                    .await?
            };
            let Some(step_id) = step_id else {
                let terminal = PersistedTerminal::Cancelled {
                    evidence: generation.evidence().map(TerminalEvidence::Generation),
                    attempts: attempts[committed_attempts..].to_vec(),
                    overflow_recovery: None,
                };
                let terminal = writer
                    .close_run(
                        run_id,
                        pending_step_id,
                        move || seal_terminal(terminal, overflow_recovery),
                        None,
                    )
                    .await?;
                return Ok(PersistedRunOutcome {
                    run_id,
                    terminal,
                    authentication_failure: false,
                    recovery_failure: false,
                });
            };
            pending_step_id = Some(step_id);
            if let Some(context) = &mut self.invocation_context {
                context.set_origin_step_id(step_id);
            }
            event(PersistedAgentEvent::ModelStepStarted {
                run_id,
                step_id,
                epoch: 0,
            });

            let mut epoch = 0;
            let (history, model_step) = loop {
                if cancellation.is_cancelled() {
                    let terminal = PersistedTerminal::Cancelled {
                        evidence: generation.evidence().map(TerminalEvidence::Generation),
                        attempts: attempts[committed_attempts..].to_vec(),
                        overflow_recovery: None,
                    };
                    let terminal = writer
                        .close_run(
                            run_id,
                            pending_step_id,
                            move || seal_terminal(terminal, overflow_recovery),
                            None,
                        )
                        .await?;
                    return Ok(PersistedRunOutcome {
                        run_id,
                        terminal,
                        authentication_failure: false,
                        recovery_failure: false,
                    });
                }
                let mut model_progress = |progress: ModelProgress| {
                    generation.capture(&progress);
                    event(PersistedAgentEvent::ModelProgress {
                        run_id,
                        step_id,
                        epoch,
                        progress,
                    });
                };
                let history = writer.model_history()?;
                let failure = match self
                    .model
                    .step(selection, &history, cancellation, &mut model_progress)
                    .await
                {
                    Ok(step) => break (history, step),
                    Err(failure) => failure,
                };
                if overflow_recovery.is_none()
                    && policy.enabled()
                    && let Some(provider_failure) = failure
                        .provider_failure()
                        .filter(|failure| failure.is_context_overflow())
                {
                    let original = provider_failure.to_string();
                    let initial_attempts = failure.attempts().to_vec();
                    overflow_recovery = Some(OverflowRecovery {
                        failure: provider_failure.clone(),
                        generation: std::mem::take(&mut generation).evidence(),
                        attempts: initial_attempts.clone(),
                    });
                    let result = self
                        .compact(
                            CompactionReason::ContextOverflow,
                            policy,
                            selection,
                            reasoning,
                            String::new(),
                            cancellation,
                            writer,
                            &mut || {
                                event(PersistedAgentEvent::CompactionStarted {
                                    run_id,
                                    initial_overflow: Some((step_id, initial_attempts.clone())),
                                })
                            },
                        )
                        .await?;
                    let recovery_failure = match result {
                        CompactionResult::NoOp => Some(NO_REMOVABLE_PREFIX.to_owned()),
                        CompactionResult::CancelledBeforeStart => None,
                        CompactionResult::FailedBeforeStart(diagnostic) => Some(diagnostic),
                        CompactionResult::Settled(compaction) => {
                            let diagnostic = match &compaction.outcome {
                                CompactionOutcome::Failed { diagnostic, .. } => {
                                    Some(diagnostic.clone())
                                }
                                CompactionOutcome::Applied { .. }
                                | CompactionOutcome::Cancelled { .. } => None,
                            };
                            let retry =
                                matches!(compaction.outcome, CompactionOutcome::Applied { .. })
                                    && !cancellation.is_cancelled();
                            if retry {
                                epoch = 1;
                                event(PersistedAgentEvent::GenerationRestarted {
                                    run_id,
                                    step_id,
                                    epoch,
                                });
                            }
                            event(PersistedAgentEvent::CompactionCommitted { compaction });
                            if retry {
                                event(PersistedAgentEvent::ModelStepStarted {
                                    run_id,
                                    step_id,
                                    epoch,
                                });
                                continue;
                            }
                            diagnostic
                        }
                    };
                    let message = recovery_failure
                        .map(|recovery| overflow_recovery_message(original, recovery));
                    let attempts = attempts[committed_attempts..].to_vec();
                    let cancellation = cancellation.clone();
                    let terminal = writer
                        .close_run(
                            run_id,
                            pending_step_id,
                            move || {
                                seal_terminal(
                                    match message {
                                        Some(message) if !cancellation.is_cancelled() => {
                                            PersistedTerminal::Failed {
                                                message,
                                                evidence: None,
                                                attempts,
                                                overflow_recovery: None,
                                            }
                                        }
                                        _ => PersistedTerminal::Cancelled {
                                            evidence: None,
                                            attempts,
                                            overflow_recovery: None,
                                        },
                                    },
                                    overflow_recovery,
                                )
                            },
                            None,
                        )
                        .await?;
                    return Ok(PersistedRunOutcome {
                        run_id,
                        terminal,
                        authentication_failure: false,
                        recovery_failure: false,
                    });
                }
                attempts.extend(failure.attempts().iter().cloned());
                let failure_is_cancelled = failure.is_cancelled();
                let authentication_failure = failure.is_authentication();
                let message = bounded_utf8(failure.to_string(), FAILED_RUN_MESSAGE_LIMIT);
                let evidence = generation.evidence().map(TerminalEvidence::Generation);
                let attempts = attempts[committed_attempts..].to_vec();
                let cancellation = cancellation.clone();
                let terminal = writer
                    .close_run(
                        run_id,
                        pending_step_id,
                        move || {
                            seal_terminal(
                                if cancellation.is_cancelled() || failure_is_cancelled {
                                    PersistedTerminal::Cancelled {
                                        evidence,
                                        attempts,
                                        overflow_recovery: None,
                                    }
                                } else {
                                    PersistedTerminal::Failed {
                                        message,
                                        evidence,
                                        attempts,
                                        overflow_recovery: None,
                                    }
                                },
                                overflow_recovery,
                            )
                        },
                        None,
                    )
                    .await?;
                return Ok(PersistedRunOutcome {
                    run_id,
                    authentication_failure: matches!(&terminal, PersistedTerminal::Failed { .. })
                        && authentication_failure,
                    terminal,
                    recovery_failure: false,
                });
            };
            let ModelStep {
                action,
                reasoning_summary,
                replay,
                attempts: step_attempts,
            } = model_step;
            attempts.extend(step_attempts.iter().cloned());
            match action {
                ModelAction::Code {
                    source,
                    call_id,
                    commentary,
                } => {
                    if history.contains_call_id(&call_id) {
                        let message = bounded_utf8(
                            ModelHistoryError::DuplicateCallId(call_id).to_string(),
                            FAILED_RUN_MESSAGE_LIMIT,
                        );
                        let evidence = generation.evidence().map(TerminalEvidence::Generation);
                        let attempts = attempts[committed_attempts..].to_vec();
                        let cancellation = cancellation.clone();
                        let terminal = writer
                            .close_run(
                                run_id,
                                pending_step_id,
                                move || {
                                    seal_terminal(
                                        if cancellation.is_cancelled() {
                                            PersistedTerminal::Cancelled {
                                                evidence,
                                                attempts,
                                                overflow_recovery: None,
                                            }
                                        } else {
                                            PersistedTerminal::Failed {
                                                message,
                                                evidence,
                                                attempts,
                                                overflow_recovery: None,
                                            }
                                        },
                                        overflow_recovery,
                                    )
                                },
                                None,
                            )
                            .await?;
                        return Ok(PersistedRunOutcome {
                            run_id,
                            terminal,
                            authentication_failure: false,
                            recovery_failure: false,
                        });
                    }
                    generation.clear();
                    let pending = PendingCode {
                        source,
                        commentary,
                        selection,
                        call_id,
                        reasoning_summary,
                        replay,
                    };
                    if cancellation.is_cancelled() {
                        let evidence = ExecutionEvidence::cancelled(
                            pending.source.clone(),
                            pending.commentary.clone(),
                            pending.reasoning_summary.clone(),
                            None,
                        );
                        let result = CodeResult::cancelled(evidence.feedback());
                        let code = persisted_code(&pending, result, None, step_attempts.clone());
                        let terminal = PersistedTerminal::Cancelled {
                            evidence: Some(TerminalEvidence::Execution(Box::new(evidence))),
                            attempts: attempts[committed_attempts..].to_vec(),
                            overflow_recovery: None,
                        };
                        let terminal = writer
                            .close_run(
                                run_id,
                                pending_step_id,
                                move || seal_terminal(terminal, overflow_recovery),
                                Some(code),
                            )
                            .await?;
                        return Ok(PersistedRunOutcome {
                            run_id,
                            terminal,
                            authentication_failure: false,
                            recovery_failure: false,
                        });
                    }
                    event(PersistedAgentEvent::ExecutionStarted {
                        run_id,
                        step_id,
                        commentary: pending.commentary.clone(),
                    });
                    let active_skills = history.active_skills().cloned().collect::<Vec<_>>();
                    let execution = {
                        let mut arena_progress = |arena_progress| match arena_progress {
                            ArenaProgress::ToolUpdated { token, call } => {
                                event(PersistedAgentEvent::ToolUpdated {
                                    run_id,
                                    step_id,
                                    token,
                                    call,
                                });
                            }
                            ArenaProgress::RunPreviewUpdated { token, preview } => {
                                event(PersistedAgentEvent::RunPreviewUpdated {
                                    run_id,
                                    step_id,
                                    token,
                                    preview,
                                });
                            }
                            ArenaProgress::StdoutDelta { text } => {
                                event(PersistedAgentEvent::StdoutDelta {
                                    run_id,
                                    step_id,
                                    text,
                                });
                            }
                            ArenaProgress::StdoutTruncated => {
                                event(PersistedAgentEvent::StdoutTruncated { run_id, step_id });
                            }
                        };
                        self.arena
                            .execute_model_code(
                                &pending.source,
                                cancellation,
                                &mut arena_progress,
                                self.invocation_context
                                    .as_ref()
                                    .map(|context| (context, pending.selection)),
                                &active_skills,
                            )
                            .await
                    };
                    match execution {
                        Ok((execution, skill_activations)) => {
                            let result = CodeResult::execution(execution_feedback(&execution));
                            let code = persisted_code(
                                &pending,
                                result,
                                Some(execution.clone()),
                                step_attempts.clone(),
                            );
                            let checkpoint = self.arena.checkpoint();
                            let activations = writer
                                .settle_execution(
                                    run_id,
                                    step_id,
                                    code,
                                    checkpoint,
                                    skill_activations,
                                )
                                .await?;
                            pending_step_id = None;
                            committed_attempts += step_attempts.len();
                            event(PersistedAgentEvent::ExecutionCommitted {
                                run_id,
                                step_id,
                                source: pending.source,
                                commentary: pending.commentary,
                                reasoning_summary: pending.reasoning_summary,
                                execution,
                                attempts: step_attempts,
                                activations,
                            });
                        }
                        Err(error) => {
                            let arena_cancelled = matches!(&error, ArenaError::Cancelled { .. });
                            let evidence = if arena_cancelled {
                                ExecutionEvidence::cancelled(
                                    pending.source.clone(),
                                    pending.commentary.clone(),
                                    pending.reasoning_summary.clone(),
                                    Some(&error),
                                )
                            } else {
                                ExecutionEvidence::failed(
                                    pending.source.clone(),
                                    pending.commentary.clone(),
                                    pending.reasoning_summary.clone(),
                                    &error,
                                )
                            };
                            let result = if arena_cancelled {
                                CodeResult::cancelled(evidence.feedback())
                            } else {
                                CodeResult::failure(evidence.feedback())
                            };
                            let code =
                                persisted_code(&pending, result, None, step_attempts.clone());
                            let is_recovery_failure =
                                matches!(&error, ArenaError::RecoveryFailed { .. });
                            let message = bounded_utf8(error.to_string(), FAILED_RUN_MESSAGE_LIMIT);
                            let evidence = Some(TerminalEvidence::Execution(Box::new(evidence)));
                            let attempts = attempts[committed_attempts..].to_vec();
                            let cancellation = cancellation.clone();
                            let terminal = writer
                                .close_run(
                                    run_id,
                                    pending_step_id,
                                    move || {
                                        seal_terminal(
                                            if !is_recovery_failure
                                                && (cancellation.is_cancelled() || arena_cancelled)
                                            {
                                                PersistedTerminal::Cancelled {
                                                    evidence,
                                                    attempts,
                                                    overflow_recovery: None,
                                                }
                                            } else {
                                                PersistedTerminal::Failed {
                                                    message,
                                                    evidence,
                                                    attempts,
                                                    overflow_recovery: None,
                                                }
                                            },
                                            overflow_recovery,
                                        )
                                    },
                                    Some(code),
                                )
                                .await?;
                            return Ok(PersistedRunOutcome {
                                run_id,
                                recovery_failure: matches!(
                                    &terminal,
                                    PersistedTerminal::Failed { .. }
                                ) && is_recovery_failure,
                                terminal,
                                authentication_failure: false,
                            });
                        }
                    }
                }
                ModelAction::Message { text } => {
                    let generation =
                        generation.with_final_message(text.clone(), reasoning_summary.clone());
                    let attempts = attempts[committed_attempts..].to_vec();
                    let cancellation = cancellation.clone();
                    let terminal = writer
                        .close_run(
                            run_id,
                            pending_step_id,
                            move || {
                                seal_terminal(
                                    if cancellation.is_cancelled() {
                                        PersistedTerminal::Cancelled {
                                            evidence: Some(TerminalEvidence::Generation(
                                                generation,
                                            )),
                                            attempts,
                                            overflow_recovery: None,
                                        }
                                    } else {
                                        PersistedTerminal::Final {
                                            step_id,
                                            text,
                                            reasoning_summary,
                                            replay,
                                            attempts,
                                            overflow_recovery: None,
                                        }
                                    },
                                    overflow_recovery,
                                )
                            },
                            None,
                        )
                        .await?;
                    return Ok(PersistedRunOutcome {
                        run_id,
                        terminal,
                        authentication_failure: false,
                        recovery_failure: false,
                    });
                }
            }
        }
    }

    pub(crate) async fn run_with_progress(
        &mut self,
        selection: ModelSelection,
        request: impl Into<String>,
        cancellation: &CancellationToken,
        progress: &mut (impl FnMut(AgentProgress) + Send),
    ) -> Result<FinalMessage, AgentError> {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled {
                attempts: Vec::new(),
                evidence: None,
            });
        }
        let mut history = ModelHistory::new();
        history.append_user_request(request);
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
                .step(selection, &history, cancellation, &mut model_progress)
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
                ModelAction::Code {
                    source,
                    call_id,
                    commentary,
                } => {
                    if history.contains_call_id(&call_id) {
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
                        commentary,
                        selection,
                        call_id,
                        reasoning_summary,
                        replay,
                    };
                    if cancellation.is_cancelled() {
                        let evidence = Box::new(ExecutionEvidence::cancelled(
                            pending.source.clone(),
                            pending.commentary.clone(),
                            pending.reasoning_summary.clone(),
                            None,
                        ));
                        return Err(AgentError::Cancelled {
                            attempts,
                            evidence: Some(Box::new(TerminalEvidence::Execution(evidence))),
                        });
                    }
                    progress(AgentProgress::ExecutionStarted {
                        commentary: pending.commentary.clone(),
                    });
                    let active_skills = history.active_skills().cloned().collect::<Vec<_>>();
                    let execution = {
                        let mut arena_progress = |arena_progress| match arena_progress {
                            ArenaProgress::ToolUpdated { token, call } => {
                                progress(AgentProgress::ToolUpdated { token, call });
                            }
                            ArenaProgress::RunPreviewUpdated { token, preview } => {
                                progress(AgentProgress::RunPreviewUpdated { token, preview });
                            }
                            ArenaProgress::StdoutDelta { text } => {
                                progress(AgentProgress::StdoutDelta { text });
                            }
                            ArenaProgress::StdoutTruncated => {
                                progress(AgentProgress::StdoutTruncated);
                            }
                        };
                        self.arena
                            .execute_model_code(
                                &pending.source,
                                cancellation,
                                &mut arena_progress,
                                self.invocation_context
                                    .as_ref()
                                    .map(|context| (context, pending.selection)),
                                &active_skills,
                            )
                            .await
                    };
                    match execution {
                        Ok((execution, skill_activations)) => {
                            let source = pending.source.clone();
                            let commentary = pending.commentary.clone();
                            let reasoning_summary = pending.reasoning_summary.clone();
                            if let Err(error) = pending.append(
                                &mut history,
                                CodeResult::execution(execution_feedback(&execution)),
                            ) {
                                return Err(AgentError::ModelHistory {
                                    error,
                                    attempts,
                                    evidence: None,
                                });
                            }
                            for activation in skill_activations {
                                history
                                    .activate_skill(activation)
                                    .expect("load ledger validated active skill text cap");
                            }
                            progress(AgentProgress::ExecutionCompleted {
                                source,
                                commentary,
                                reasoning_summary,
                                execution,
                                attempts: step_attempts,
                            });
                            if let Some(context) = &self.invocation_context
                                && !context.persist_child_step().await
                            {
                                return Err(AgentError::Cancelled {
                                    attempts,
                                    evidence: None,
                                });
                            }
                        }
                        Err(error) => {
                            let cancelled = matches!(&error, ArenaError::Cancelled { .. });
                            let evidence = Box::new(if cancelled {
                                ExecutionEvidence::cancelled(
                                    pending.source.clone(),
                                    pending.commentary.clone(),
                                    pending.reasoning_summary.clone(),
                                    Some(&error),
                                )
                            } else {
                                ExecutionEvidence::failed(
                                    pending.source.clone(),
                                    pending.commentary.clone(),
                                    pending.reasoning_summary.clone(),
                                    &error,
                                )
                            });
                            return Err(AgentError::Arena {
                                error,
                                attempts,
                                evidence,
                            });
                        }
                    }
                }
                ModelAction::Message { text } => {
                    return Ok(FinalMessage { text, attempts });
                }
            }
        }
    }
}

fn seal_terminal(
    mut terminal: PersistedTerminal,
    recovery: Option<OverflowRecovery>,
) -> PersistedTerminal {
    let (PersistedTerminal::Final {
        overflow_recovery, ..
    }
    | PersistedTerminal::Cancelled {
        overflow_recovery, ..
    }
    | PersistedTerminal::Failed {
        overflow_recovery, ..
    }) = &mut terminal;
    *overflow_recovery = recovery;
    terminal
}

fn overflow_recovery_message(original: String, recovery: String) -> String {
    format!(
        "context overflow: {}\ncompaction recovery failed: {}",
        bounded_utf8(original, OVERFLOW_MESSAGE_COMPONENT_LIMIT),
        bounded_utf8(recovery, OVERFLOW_MESSAGE_COMPONENT_LIMIT),
    )
}

fn persisted_code(
    pending: &PendingCode,
    result: CodeResult,
    execution: Option<Execution>,
    attempts: Vec<AttemptAccounting>,
) -> PersistedCode {
    PersistedCode {
        source: pending.source.clone(),
        call_id: pending.call_id.clone(),
        commentary: pending.commentary.clone(),
        reasoning_summary: pending.reasoning_summary.clone(),
        replay: pending.replay.clone(),
        result,
        execution,
        attempts,
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
        ExecutionOutcome::TypeCheckError(error) => {
            frames.push(format!(
                "<kiki_type_check_error omitted_bytes=\"{}\" arena_state=\"unchanged\">\nType check failed before execution:\n{}</kiki_type_check_error>",
                error.omitted_bytes, error.text
            ));
            if error.omitted_bytes > 0 {
                frames.push("<kiki_warning code=\"type_check_error_truncated\" arena_state=\"unchanged\">\nType-check diagnostics were truncated. No Python code was executed.\n</kiki_warning>".to_owned());
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
    use std::collections::{BTreeMap, VecDeque};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::sync::Notify;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        Agent, AgentError, AgentProgress, CompactionResult, ExecutionFailure, GenerationEvidence,
        GenerationEvidenceBuilder, OverflowRecovery, TerminalEvidence,
    };
    use crate::arena::{
        Arena, ArenaError, Execution, ExecutionOutcome, Observation, PythonError, ToolCall,
        ToolCallSummary, ToolName,
    };
    use crate::config::settings::CrossProviderReasoning;
    use crate::config::{CompactionSettings, DEFAULT_MODEL, ModelSelection};
    use crate::model::{
        AttemptAccounting, CodeResult, Model, ModelAction as CompletedModelAction, ModelCallId,
        ModelContextEntry, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ProviderFailure, ProviderReplay, ScriptedAction as ModelAction, ScriptedModel,
        SummaryRequest, SummaryResult,
    };
    use crate::session::{
        CompactionOutcome, CompactionReason, PersistedTerminal, ReadableTerminal, RestoredRecord,
        RestoredRecordKind, Session, SessionEntryId, SessionWriteGate, SessionWriteKind,
        SessionWriter,
    };
    use crate::skills::{ProjectScope, SkillCatalog, write_skill};
    use crate::subagents::{
        AcceptedThinking, AgentInvocationContext, ChildPayload, ChildResult, ChildRun, ChildRunner,
        RunnerFuture, SessionSubagents, SubagentCoordinator, SubagentPolicy,
    };
    use crate::tools::{ReadTestGate, ToolHost, ToolValue};

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the monty-runtime 0.0.22 worker")
    }

    fn default_selection() -> ModelSelection {
        ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap()
    }

    async fn agent(workspace: &TempDir, actions: Vec<ModelAction>) -> Agent<ScriptedModel> {
        agent_with_model(workspace, ScriptedModel::new(actions)).await
    }

    async fn agent_with_model<M: Model>(workspace: &TempDir, model: M) -> Agent<M> {
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        Agent::new(arena, model)
    }

    #[tokio::test]
    async fn read_only_child_loads_skill_into_next_model_context_without_printing_body() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "name: review\ndescription: Review\n",
            "CHILD_PRIVATE_BODY",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let tool_host = ToolHost::new(workspace.path())
            .await
            .unwrap()
            .with_skill_catalog(catalog)
            .for_child(true);
        let arena = Arena::new(worker_path(), tool_host).await.unwrap();
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await load_skill('review')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ]),
        );
        let answer = agent
            .run(default_selection(), "review", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(answer.text(), "done");
        let contexts = agent.model.contexts();
        assert_eq!(contexts.len(), 2);
        assert_eq!(
            contexts[1]
                .iter()
                .filter(|entry| matches!(entry, ModelContextEntry::SkillActivation(_)))
                .count(),
            1
        );
        assert!(format!("{:?}", contexts[1]).contains("CHILD_PRIVATE_BODY"));
        assert!(!format!("{:?}", contexts[0]).contains("CHILD_PRIVATE_BODY"));
    }

    #[tokio::test]
    async fn persisted_root_load_skill_reaches_next_model_step_and_restore() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "name: review\ndescription: Review\n",
            "ROOT_PRIVATE_BODY",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let tool_host = ToolHost::new(workspace.path())
            .await
            .unwrap()
            .with_skill_catalog(catalog);
        let arena = Arena::new(worker_path(), tool_host).await.unwrap();
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await load_skill('review')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ]),
        );
        let selection = default_selection();
        let session =
            Session::create(home.path(), workspace.path(), selection, agent.checkpoint()).unwrap();
        let writer = session.writer();
        let outcome = agent
            .run_persisted(
                selection,
                CompactionSettings::new(16_384, 20_000),
                CrossProviderReasoning::Inline,
                "review".to_owned(),
                None,
                &CancellationToken::new(),
                &writer,
                &mut |_| {},
            )
            .await
            .unwrap();
        assert!(matches!(outcome.terminal, PersistedTerminal::Final { .. }));
        let contexts = agent.model.contexts();
        assert_eq!(contexts.len(), 2);
        assert!(!format!("{:?}", contexts[0]).contains("ROOT_PRIVATE_BODY"));
        assert_eq!(
            contexts[1]
                .iter()
                .filter(|entry| matches!(entry, ModelContextEntry::SkillActivation(_)))
                .count(),
            1
        );
        assert!(format!("{:?}", contexts[1]).contains("ROOT_PRIVATE_BODY"));
        let restored = session.restore().unwrap();
        let history = restored.executable.unwrap().history;
        assert_eq!(history.active_skills().count(), 1);
        assert!(
            history
                .active_skills()
                .next()
                .unwrap()
                .source_text()
                .contains("ROOT_PRIVATE_BODY")
        );
        drop(writer);
        session.close().unwrap();
    }

    #[tokio::test]
    async fn persisted_root_loads_two_skills_from_one_cell_into_next_step_and_ancestry() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("alpha", "ALPHA_PRIVATE_BODY"),
            ("beta", "BETA_PRIVATE_BODY"),
        ] {
            write_skill(
                &home.path().join(".agents/skills"),
                name,
                &format!("name: {name}\ndescription: {name}\n"),
                body,
            );
        }
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = ToolHost::new(workspace.path())
            .await
            .unwrap()
            .with_skill_catalog(catalog);
        let arena = Arena::new(worker_path(), host).await.unwrap();
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await load_skill('alpha')\nawait load_skill('beta')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ]),
        );
        let selection = default_selection();
        let session =
            Session::create(home.path(), workspace.path(), selection, agent.checkpoint()).unwrap();
        let writer = session.writer();
        agent
            .run_persisted(
                selection,
                CompactionSettings::new(16_384, 20_000),
                CrossProviderReasoning::Inline,
                "use both".to_owned(),
                None,
                &CancellationToken::new(),
                &writer,
                &mut |_| {},
            )
            .await
            .unwrap();

        let contexts = agent.model.contexts();
        assert_eq!(contexts.len(), 2);
        assert_eq!(
            contexts[1]
                .iter()
                .filter(|entry| matches!(entry, ModelContextEntry::SkillActivation(_)))
                .count(),
            2
        );
        for body in ["ALPHA_PRIVATE_BODY", "BETA_PRIVATE_BODY"] {
            assert!(!format!("{:?}", contexts[0]).contains(body));
            assert!(format!("{:?}", contexts[1]).contains(body));
        }

        let restored = session.restore().unwrap();
        let active = restored
            .executable
            .unwrap()
            .history
            .active_skills()
            .map(|skill| (skill.name().to_owned(), skill.source_text().to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(active.len(), 2);
        assert!(
            active
                .iter()
                .any(|(name, body)| name == "alpha" && body.contains("ALPHA_PRIVATE_BODY"))
        );
        assert!(
            active
                .iter()
                .any(|(name, body)| name == "beta" && body.contains("BETA_PRIVATE_BODY"))
        );
        drop(writer);
        session.close().unwrap();
    }

    async fn run_capturing_progress(
        agent: &mut Agent<ScriptedModel>,
        request: &str,
        cancellation: &CancellationToken,
    ) -> Result<(String, Vec<AgentProgress>), AgentError> {
        let mut captured = Vec::new();
        let message = {
            let mut progress = |event| captured.push(event);
            agent
                .run_with_progress(default_selection(), request, cancellation, &mut progress)
                .await?
                .text()
                .to_owned()
        };
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

    struct HistoryAwareModel {
        contexts: Vec<Vec<ModelContextEntry>>,
    }

    impl Model for HistoryAwareModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
            history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let entries = history.entries().to_vec();
                self.contexts.push(entries.clone());
                let action = match entries.as_slice() {
                    [ModelContextEntry::UserRequest { .. }] => CompletedModelAction::Code {
                        source: if self.contexts.len() == 1 {
                            "value: int = 1"
                        } else {
                            "value += 1"
                        }
                        .to_owned(),
                        call_id: ModelCallId::new("history-aware-code"),
                        commentary: None,
                    },
                    [
                        ModelContextEntry::UserRequest { .. },
                        ModelContextEntry::CompletedCode(_),
                    ] => CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    _ => {
                        return Err(
                            ModelError::InvalidHistory("unexpected ephemeral history").into()
                        );
                    }
                };
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("history-aware", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    async fn seed_compactable_history(writer: &SessionWriter) {
        for label in ["discarded", "retained"] {
            let admission = writer
                .commit_request(format!("{label} request"), default_selection(), None)
                .await
                .expect("commit compaction fixture request");
            let step_id = writer
                .start_model_step(admission.run_id)
                .await
                .expect("start compaction fixture step");
            let replay = ProviderReplay::new(
                "openai",
                vec![crate::model::OpaqueReplayItem::new(vec![b'x'; 100_000])],
            );
            writer
                .close_run(
                    admission.run_id,
                    Some(step_id),
                    move || PersistedTerminal::Final {
                        step_id,
                        text: format!("{label} reply"),
                        reasoning_summary: String::new(),
                        replay,
                        attempts: Vec::new(),
                        overflow_recovery: None,
                    },
                    None,
                )
                .await
                .expect("close compaction fixture run");
        }
    }

    struct CancelBeforeSummaryAttemptModel;

    impl Model for CancelBeforeSummaryAttemptModel {
        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            _request: crate::model::SummaryRequest,
            cancellation: &'a CancellationToken,
            _attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<crate::model::SummaryResult, ModelFailure>>
        + Send
        + 'a {
            async move {
                cancellation.cancel();
                Err(ModelError::Cancelled.into())
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async { Err(ModelError::Exhausted.into()) }
        }
    }

    struct CancelBetweenSummaryRequestsModel {
        calls: Arc<AtomicUsize>,
        attempts: Arc<AtomicUsize>,
    }

    impl Model for CancelBetweenSummaryRequestsModel {
        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            _request: SummaryRequest,
            cancellation: &'a CancellationToken,
            attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a
        {
            async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call != 0 {
                    return Err(ModelError::Cancelled.into());
                }
                attempt_started();
                self.attempts.fetch_add(1, Ordering::SeqCst);
                cancellation.cancel();
                Ok(SummaryResult {
                    text: "first summary".to_owned(),
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                })
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async { Err(ModelError::Exhausted.into()) }
        }
    }

    #[tokio::test]
    async fn manual_compaction_cancellation_between_prepared_requests_settles_one_attempt() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let calls = Arc::new(AtomicUsize::new(0));
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut agent = agent_with_model(
            &workspace,
            CancelBetweenSummaryRequestsModel {
                calls: calls.clone(),
                attempts: attempts.clone(),
            },
        )
        .await;
        let session = Session::create(home.path(), workspace.path(), selection, agent.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let policy = CompactionSettings::new(16_384, 20_000);
        assert_eq!(
            writer
                .prepare_compaction(
                    policy,
                    selection,
                    CrossProviderReasoning::Inline,
                    String::new(),
                    crate::session::CompactionReason::Manual,
                )
                .await
                .expect("prepare compaction")
                .expect("compaction should remove a prefix")
                .requests()
                .len(),
            2
        );
        let cancellation = CancellationToken::new();
        let mut started = 0;

        let result = agent
            .compact(
                CompactionReason::Manual,
                policy,
                selection,
                CrossProviderReasoning::Inline,
                String::new(),
                &cancellation,
                &writer,
                &mut || started += 1,
            )
            .await
            .expect("persist cancellation outcome");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(started, 1);
        assert!(matches!(
            result,
            CompactionResult::Settled(committed)
                if matches!(
                    committed.outcome,
                    CompactionOutcome::Cancelled { ref attempts, .. }
                        if attempts == &[AttemptAccounting::UsageUnavailable]
                )
        ));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn manual_compaction_uses_the_prepared_summary_calls_and_commits_once() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let mut agent = agent_with_model(
            &workspace,
            ScriptedModel::new(vec![ModelAction::Message {
                text: "next normal response".to_owned(),
            }])
            .with_summary_responses(vec![
                "first summary".to_owned(),
                "second summary".to_owned(),
            ]),
        )
        .await;
        let session = Session::create(home.path(), workspace.path(), selection, agent.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let policy = CompactionSettings::new(16_384, 20_000);
        let expected_requests = writer
            .prepare_compaction(
                policy,
                selection,
                CrossProviderReasoning::Inline,
                "preserve decisions".to_owned(),
                crate::session::CompactionReason::Manual,
            )
            .await
            .expect("prepare compaction")
            .expect("compaction should remove a prefix")
            .requests()
            .to_vec();
        let cancellation = CancellationToken::new();
        let mut started = 0;

        let result = agent
            .compact(
                CompactionReason::Manual,
                policy,
                selection,
                CrossProviderReasoning::Inline,
                "preserve decisions".to_owned(),
                &cancellation,
                &writer,
                &mut || started += 1,
            )
            .await
            .expect("settle compaction");

        assert_eq!(agent.model.summary_requests(), expected_requests);
        assert_eq!(started, 1);
        assert!(matches!(
            result,
            CompactionResult::Settled(committed)
                if matches!(committed.outcome, CompactionOutcome::Applied { .. })
        ));
        let next_cancellation = CancellationToken::new();
        let mut events = Vec::new();
        agent
            .run_persisted(
                selection,
                CompactionSettings::default(),
                CrossProviderReasoning::Inline,
                "next request".to_owned(),
                None,
                &next_cancellation,
                &writer,
                &mut |event| events.push(event),
            )
            .await
            .expect("run after compaction");
        assert!(
            agent
                .model
                .contexts()
                .last()
                .expect("capture next model context")
                .iter()
                .any(|entry| matches!(entry, ModelContextEntry::CompactionSummary { .. }))
        );

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn manual_compaction_cancelled_before_an_attempt_writes_nothing() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let mut agent = agent_with_model(&workspace, CancelBeforeSummaryAttemptModel).await;
        let session = Session::create(home.path(), workspace.path(), selection, agent.checkpoint())
            .expect("create session");
        let writer = session.writer();
        for request in [
            "first ".to_owned() + &"x".repeat(100_000),
            "second ".to_owned() + &"y".repeat(100_000),
        ] {
            let admission = writer
                .commit_request(request, selection, None)
                .await
                .expect("commit request");
            writer
                .close_run(
                    admission.run_id,
                    None,
                    || PersistedTerminal::Cancelled {
                        evidence: None,
                        attempts: Vec::new(),
                        overflow_recovery: None,
                    },
                    None,
                )
                .await
                .expect("close seeded run");
        }
        let revision_before: i64 = session
            .connection_for_test()
            .expect("open session")
            .query_row(
                "SELECT revision FROM session WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read revision");
        let cancellation = CancellationToken::new();
        let mut started = 0;

        let result = agent
            .compact(
                CompactionReason::Manual,
                CompactionSettings::new(16_384, 20_000),
                selection,
                CrossProviderReasoning::Inline,
                String::new(),
                &cancellation,
                &writer,
                &mut || started += 1,
            )
            .await
            .expect("cancelled compaction is not a storage failure");

        assert_eq!(result, CompactionResult::CancelledBeforeStart);
        assert_eq!(started, 0);
        let connection = session.connection_for_test().expect("open session");
        let revision_after: i64 = connection
            .query_row(
                "SELECT revision FROM session WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read revision");
        let compactions: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = 'compaction'",
                [],
                |row| row.get(0),
            )
            .expect("count compactions");
        assert_eq!(revision_after, revision_before);
        assert_eq!(compactions, 0);

        drop(connection);
        drop(writer);
        session.close().expect("close session");
    }

    fn threshold_policy(threshold_tokens: u64) -> CompactionSettings {
        CompactionSettings::new(
            DEFAULT_MODEL.context_window_tokens - threshold_tokens,
            20_000,
        )
    }

    #[tokio::test]
    async fn threshold_compacts_after_committed_request_before_first_step() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = default_selection();
        let mut agent = agent_with_model(
            &workspace,
            ScriptedModel::new(vec![
                ModelAction::Message {
                    text: "small done".to_owned(),
                },
                ModelAction::Message {
                    text: "large done".to_owned(),
                },
            ])
            .with_summary_responses(vec!["threshold summary".to_owned()]),
        )
        .await;
        let session = Session::create(home.path(), workspace.path(), selection, agent.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let policy = threshold_policy(60_000);

        let small = agent
            .run_persisted(
                selection,
                policy,
                CrossProviderReasoning::Inline,
                "small request".to_owned(),
                None,
                &CancellationToken::new(),
                &writer,
                &mut |_| {},
            )
            .await
            .expect("run small request");
        assert!(matches!(small.terminal, PersistedTerminal::Final { .. }));
        assert!(agent.model.summary_requests().is_empty());
        assert!(matches!(
            agent.model.contexts()[0].first(),
            Some(ModelContextEntry::UserRequest { text }) if text == "discarded request"
        ));

        let large_request = "large ".to_owned() + &"x".repeat(100_000);
        let mut events = Vec::new();
        let large = agent
            .run_persisted(
                selection,
                policy,
                CrossProviderReasoning::Inline,
                large_request.clone(),
                None,
                &CancellationToken::new(),
                &writer,
                &mut |event| events.push(event),
            )
            .await
            .expect("run large request");

        assert_eq!(agent.model.summary_requests().len(), 1);
        assert_eq!(
            agent
                .model
                .contexts()
                .last()
                .expect("capture the first large-request step"),
            &[
                ModelContextEntry::CompactionSummary {
                    summary: "threshold summary".to_owned(),
                },
                ModelContextEntry::UserRequest {
                    text: large_request.clone(),
                },
            ]
        );
        assert!(matches!(
            &large.terminal,
            PersistedTerminal::Final { text, .. } if text == "large done"
        ));
        let request_entries = events
            .iter()
            .filter_map(|event| match event {
                super::PersistedAgentEvent::RequestCommitted { request_entry, .. } => {
                    Some(*request_entry)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let [request_entry] = request_entries.as_slice() else {
            panic!("the large request is admitted once");
        };
        let restore = session.restore().expect("restore compacted run");
        let run_records = restore
            .records
            .iter()
            .filter(|record| record.run_id == large.run_id)
            .map(|record| &record.kind)
            .collect::<Vec<_>>();
        assert!(
            matches!(
                run_records.as_slice(),
                [
                    RestoredRecordKind::UserRequest { request, .. },
                    RestoredRecordKind::Compaction {
                        outcome: CompactionOutcome::Applied {
                            first_retained,
                            reason: CompactionReason::AutomaticThreshold,
                            ..
                        },
                    },
                    RestoredRecordKind::FinalMessage { text, .. },
                ] if request == &large_request
                    && first_retained == request_entry
                    && text == "large done"
            ),
            "{run_records:?}"
        );
        let (steps, requests): (i64, i64) = session
            .connection_for_test()
            .expect("open session")
            .query_row(
                "SELECT (SELECT COUNT(*) FROM model_steps WHERE run_id = ?1), (SELECT COUNT(*) FROM entries WHERE run_id = ?1 AND kind = 'user_request')",
                [i64::try_from(large.run_id).expect("run ID fits SQLite")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count run steps and requests");
        assert_eq!((steps, requests), (1, 1));

        drop(writer);
        session.close().expect("close session");
    }

    fn persisted_event_labels(events: &[super::PersistedAgentEvent]) -> Vec<&'static str> {
        events
            .iter()
            .filter_map(|event| match event {
                super::PersistedAgentEvent::RequestCommitted { .. } => Some("request"),
                super::PersistedAgentEvent::CompactionStarted { .. } => Some("compaction-started"),
                super::PersistedAgentEvent::CompactionCommitted { compaction } => {
                    Some(match compaction.outcome {
                        CompactionOutcome::Applied { .. } => "compaction:applied",
                        CompactionOutcome::Failed { .. } => "compaction:failed",
                        CompactionOutcome::Cancelled { .. } => "compaction:cancelled",
                    })
                }
                super::PersistedAgentEvent::ModelStepStarted { epoch: 0, .. } => Some("step"),
                super::PersistedAgentEvent::ModelStepStarted { epoch: 1, .. } => Some("retry-step"),
                super::PersistedAgentEvent::GenerationRestarted { epoch: 1, .. } => {
                    Some("generation-restarted")
                }
                super::PersistedAgentEvent::ExecutionCommitted { .. } => Some("execution"),
                _ => None,
            })
            .collect()
    }

    fn run_entry_kinds(session: &Session, run_id: u64) -> Vec<String> {
        let connection = session.connection_for_test().expect("open session");
        let mut statement = connection
            .prepare("SELECT kind FROM entries WHERE run_id = ?1 ORDER BY id")
            .expect("prepare entry kinds");
        statement
            .query_map(
                [i64::try_from(run_id).expect("run ID fits SQLite")],
                |row| row.get(0),
            )
            .expect("query entry kinds")
            .collect::<Result<Vec<_>, _>>()
            .expect("read entry kinds")
    }

    fn session_counts(session: &Session) -> (i64, i64) {
        session
            .connection_for_test()
            .expect("open session")
            .query_row(
                "SELECT (SELECT COUNT(*) FROM runs), (SELECT COUNT(*) FROM model_steps)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count runs and steps")
    }

    fn large_request() -> String {
        "large ".to_owned() + &"x".repeat(100_000)
    }

    async fn run_threshold_request<M: Model>(
        agent: &mut Agent<M>,
        writer: &SessionWriter,
        policy: CompactionSettings,
        request: String,
        cancellation: &CancellationToken,
    ) -> (super::PersistedRunOutcome, Vec<&'static str>) {
        let mut events = Vec::new();
        let outcome = agent
            .run_persisted(
                default_selection(),
                policy,
                CrossProviderReasoning::Inline,
                request,
                None,
                cancellation,
                writer,
                &mut |event| events.push(event),
            )
            .await
            .expect("run persisted request");
        (outcome, persisted_event_labels(&events))
    }

    #[tokio::test]
    async fn threshold_compacts_committed_feedback_before_the_next_step() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let source = format!("payload = '{}'", "y".repeat(40_000));
        let mut agent = agent_with_model(
            &workspace,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: source.clone(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ])
            .with_summary_responses(vec!["split summary".to_owned()]),
        )
        .await;
        let session = Session::create(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
        )
        .expect("create session");
        let writer = session.writer();
        let policy = CompactionSettings::new(DEFAULT_MODEL.context_window_tokens - 5_000, 1_000);

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            policy,
            "inspect".to_owned(),
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(outcome.terminal, PersistedTerminal::Final { .. }));
        assert_eq!(
            labels,
            [
                "request",
                "step",
                "execution",
                "compaction-started",
                "compaction:applied",
                "step",
            ]
        );
        let contexts = agent.model.contexts();
        assert_eq!(
            contexts[0],
            [ModelContextEntry::UserRequest {
                text: "inspect".to_owned(),
            }]
        );
        let [
            ModelContextEntry::CompactionSummary { summary },
            ModelContextEntry::CompletedCode(code),
        ] = contexts[1].as_slice()
        else {
            panic!("next step receives summary and the whole retained code entry");
        };
        assert!(summary.starts_with("No prior history."), "{summary}");
        assert!(summary.ends_with("split summary"), "{summary}");
        assert_eq!(code.source(), source);
        assert_eq!(
            code.result(),
            &CodeResult::Execution {
                feedback: String::new(),
            }
        );
        assert!(
            agent
                .model
                .summary_requests()
                .iter()
                .all(|request| request.selection == default_selection())
        );
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "execution", "compaction", "final_reply"]
        );
        assert_eq!(session_counts(&session), (1, 2));

        drop(writer);
        session.close().expect("close session");
    }

    struct NativeReplayModel {
        inner: ScriptedModel,
    }

    impl Model for NativeReplayModel {
        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            request: SummaryRequest,
            cancellation: &'a CancellationToken,
            attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a
        {
            self.inner.summarise(request, cancellation, attempt_started)
        }

        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let mut step = self
                    .inner
                    .step(selection, history, cancellation, progress)
                    .await?;
                step.replay = ProviderReplay::new("openai", step.replay.items().to_vec());
                Ok(step)
            }
        }
    }

    #[tokio::test]
    async fn later_threshold_checks_ignore_usage_from_before_an_applied_compaction() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent_with_model(
            &workspace,
            NativeReplayModel {
                inner: ScriptedModel::new(vec![
                    ModelAction::Code {
                        source: "value = 1".to_owned(),
                    },
                    ModelAction::Message {
                        text: "done".to_owned(),
                    },
                ])
                .with_summary_responses(vec![
                    "first summary".to_owned(),
                    "unexpected summary".to_owned(),
                ]),
            },
        )
        .await;
        let session = Session::create(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
        )
        .expect("create session");
        let writer = session.writer();
        append_seed_request(&writer, "oldest request").await;
        let admission = writer
            .commit_request("earlier request".to_owned(), default_selection(), None)
            .await
            .expect("commit earlier request");
        let step_id = writer
            .start_model_step(admission.run_id)
            .await
            .expect("start earlier step");
        writer
            .close_run(
                admission.run_id,
                Some(step_id),
                move || PersistedTerminal::Final {
                    step_id,
                    text: "earlier reply".to_owned(),
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("openai", Vec::new()),
                    attempts: vec![AttemptAccounting::KnownUsage(crate::model::ModelUsage {
                        model: DEFAULT_MODEL.id.to_owned(),
                        service_tier: None,
                        input_tokens: Some(450_000),
                        cached_input_tokens: Some(0),
                        cache_write_input_tokens: Some(0),
                        cache_write_1h_input_tokens: 0,
                        output_tokens: Some(50_000),
                        reasoning_output_tokens: Some(0),
                        total_tokens: Some(500_000),
                    })],
                    overflow_recovery: None,
                },
                None,
            )
            .await
            .expect("close earlier run");
        let policy = CompactionSettings::new(DEFAULT_MODEL.context_window_tokens - 100_000, 3);

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            policy,
            "inspect".to_owned(),
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(outcome.terminal, PersistedTerminal::Final { .. }));
        assert_eq!(
            labels,
            [
                "request",
                "compaction-started",
                "compaction:applied",
                "step",
                "execution",
                "step",
            ]
        );
        assert_eq!(agent.model.inner.summary_requests().len(), 1);
        assert!(agent.model.inner.contexts().iter().all(|context| matches!(
            context.as_slice(),
            [
                ModelContextEntry::CompactionSummary { summary },
                ModelContextEntry::UserRequest { text },
                ModelContextEntry::FinalMessage { .. },
                ..
            ] if summary == "first summary" && text == "earlier request"
        )));

        drop(writer);
        session.close().expect("close session");
    }

    struct FailingSummaryModel {
        steps: ScriptedModel,
    }

    impl Model for FailingSummaryModel {
        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            _request: SummaryRequest,
            _cancellation: &'a CancellationToken,
            attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a
        {
            async move {
                attempt_started();
                Err(ModelFailure::new(
                    ModelError::Provider(Box::new(ProviderFailure::new(
                        Some(500),
                        None,
                        "summary provider failed".to_owned(),
                    ))),
                    vec![AttemptAccounting::UsageUnavailable],
                ))
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            self.steps.step(selection, history, cancellation, progress)
        }
    }

    #[tokio::test]
    async fn failed_threshold_summary_persists_failed_and_continues_unchanged() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent_with_model(
            &workspace,
            FailingSummaryModel {
                steps: ScriptedModel::new(vec![ModelAction::Message {
                    text: "done".to_owned(),
                }]),
            },
        )
        .await;
        let session = Session::create(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
        )
        .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let request = large_request();

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            threshold_policy(60_000),
            request.clone(),
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(outcome.terminal, PersistedTerminal::Final { .. }));
        assert_eq!(
            labels,
            ["request", "compaction-started", "compaction:failed", "step"]
        );
        let context = &agent.model.steps.contexts()[0];
        assert!(matches!(
            context.first(),
            Some(ModelContextEntry::UserRequest { text }) if text == "discarded request"
        ));
        assert!(matches!(
            context.last(),
            Some(ModelContextEntry::UserRequest { text }) if text == &request
        ));
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "compaction", "final_reply"]
        );
        assert!(matches!(
            session.restore().expect("restore").records.as_slice(),
            [.., RestoredRecord {
                kind: RestoredRecordKind::Compaction {
                    outcome: CompactionOutcome::Failed {
                        diagnostic,
                        reason: CompactionReason::AutomaticThreshold,
                        attempts,
                    },
                },
                ..
            }, _] if diagnostic.contains("summary provider failed")
                && attempts == &[AttemptAccounting::UsageUnavailable]
        ));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn skipped_threshold_checks_emit_nothing_and_run_the_step() {
        for case in ["disabled", "no progress", "failed before start"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let summaries = if case == "failed before start" {
                Vec::new()
            } else {
                vec!["unexpected summary".to_owned()]
            };
            let mut agent = agent_with_model(
                &workspace,
                ScriptedModel::new(vec![ModelAction::Message {
                    text: "done".to_owned(),
                }])
                .with_summary_responses(summaries),
            )
            .await;
            let session = Session::create(
                home.path(),
                workspace.path(),
                default_selection(),
                agent.checkpoint(),
            )
            .expect("create session");
            let writer = session.writer();
            let policy = match case {
                "disabled" => threshold_policy(60_000).disabled(),
                "no progress" => threshold_policy(1_000),
                _ => threshold_policy(60_000),
            };
            if case != "no progress" {
                seed_compactable_history(&writer).await;
            }

            let (outcome, labels) = run_threshold_request(
                &mut agent,
                &writer,
                policy,
                large_request(),
                &CancellationToken::new(),
            )
            .await;

            assert!(
                matches!(outcome.terminal, PersistedTerminal::Final { .. }),
                "{case}"
            );
            assert_eq!(labels, ["request", "step"], "{case}");
            assert!(agent.model.summary_requests().is_empty(), "{case}");
            assert_eq!(
                run_entry_kinds(&session, outcome.run_id),
                ["user_request", "final_reply"],
                "{case}"
            );

            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn final_reply_settles_without_a_post_final_threshold_check() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent_with_model(
            &workspace,
            ScriptedModel::new(vec![ModelAction::Message {
                text: "z".repeat(100_000),
            }])
            .with_summary_responses(vec!["unexpected summary".to_owned()]),
        )
        .await;
        let session = Session::create(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
        )
        .expect("create session");
        let writer = session.writer();
        append_seed_request(&writer, "earlier request").await;

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            CompactionSettings::new(DEFAULT_MODEL.context_window_tokens - 1_000, 1),
            "inspect".to_owned(),
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(outcome.terminal, PersistedTerminal::Final { .. }));
        assert_eq!(labels, ["request", "step"]);
        assert!(agent.model.summary_requests().is_empty());
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "final_reply"]
        );

        drop(writer);
        session.close().expect("close session");
    }

    async fn append_seed_request(writer: &SessionWriter, request: &str) {
        let admission = writer
            .commit_request(request.to_owned(), default_selection(), None)
            .await
            .expect("commit seed request");
        writer
            .close_run(
                admission.run_id,
                None,
                || PersistedTerminal::Cancelled {
                    evidence: None,
                    attempts: Vec::new(),
                    overflow_recovery: None,
                },
                None,
            )
            .await
            .expect("close seed request");
    }

    #[tokio::test]
    async fn threshold_cancellation_before_an_attempt_closes_the_run_without_an_entry() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent_with_model(&workspace, CancelBeforeSummaryAttemptModel).await;
        let session = Session::create(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
        )
        .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let (runs_before, steps_before) = session_counts(&session);

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            threshold_policy(60_000),
            large_request(),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            outcome.terminal,
            PersistedTerminal::Cancelled {
                evidence: None,
                attempts: Vec::new(),
                overflow_recovery: None,
            }
        );
        assert_eq!(labels, ["request"]);
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "cancelled"]
        );
        assert_eq!(session_counts(&session), (runs_before + 1, steps_before));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn threshold_cancellation_after_an_attempt_persists_cancelled_and_closes_the_run() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent_with_model(
            &workspace,
            CancelBetweenSummaryRequestsModel {
                calls: Arc::new(AtomicUsize::new(0)),
                attempts: Arc::new(AtomicUsize::new(0)),
            },
        )
        .await;
        let session = Session::create(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
        )
        .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let (runs_before, steps_before) = session_counts(&session);

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            threshold_policy(60_000),
            large_request(),
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            outcome.terminal,
            PersistedTerminal::Cancelled { evidence: None, .. }
        ));
        assert_eq!(
            labels,
            ["request", "compaction-started", "compaction:cancelled"]
        );
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "compaction", "cancelled"]
        );
        assert_eq!(session_counts(&session), (runs_before + 1, steps_before));
        assert!(matches!(
            session.restore().expect("restore").records.as_slice(),
            [.., RestoredRecord {
                kind: RestoredRecordKind::Compaction {
                    outcome: CompactionOutcome::Cancelled {
                        reason: CompactionReason::AutomaticThreshold,
                        attempts,
                    },
                },
                run_id,
                ..
            }, _] if *run_id == outcome.run_id
                && attempts == &[AttemptAccounting::UsageUnavailable]
        ));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancellation_after_applied_is_chosen_keeps_it_and_starts_no_step() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Compaction);
        let mut agent = agent_with_model(
            &workspace,
            ScriptedModel::new(vec![ModelAction::Message {
                text: "must not run".to_owned(),
            }])
            .with_summary_responses(vec!["applied summary".to_owned()]),
        )
        .await;
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let (runs_before, steps_before) = session_counts(&session);
        let cancellation = CancellationToken::new();

        let ((outcome, labels), ()) = tokio::join!(
            run_threshold_request(
                &mut agent,
                &writer,
                threshold_policy(60_000),
                large_request(),
                &cancellation,
            ),
            async {
                gate.wait_until_started().await;
                cancellation.cancel();
                gate.release();
            }
        );

        assert!(matches!(
            outcome.terminal,
            PersistedTerminal::Cancelled { evidence: None, .. }
        ));
        assert_eq!(
            labels,
            ["request", "compaction-started", "compaction:applied"]
        );
        assert!(agent.model.contexts().is_empty());
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "compaction", "cancelled"]
        );
        assert_eq!(session_counts(&session), (runs_before + 1, steps_before));
        assert!(matches!(
            writer.model_history().expect("read compacted context").entries(),
            [ModelContextEntry::CompactionSummary { summary }, ModelContextEntry::UserRequest { .. }]
                if summary == "applied summary"
        ));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancellation_during_queued_step_allocation_after_applied_starts_no_step() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Step);
        let mut agent = agent_with_model(
            &workspace,
            ScriptedModel::new(vec![ModelAction::Message {
                text: "must not run".to_owned(),
            }])
            .with_summary_responses(vec!["applied summary".to_owned()]),
        )
        .await;
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            default_selection(),
            agent.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        append_seed_request(&writer, &large_request()).await;
        append_seed_request(&writer, &large_request()).await;
        let (runs_before, steps_before) = session_counts(&session);
        let cancellation = CancellationToken::new();

        let ((outcome, labels), ()) = tokio::join!(
            run_threshold_request(
                &mut agent,
                &writer,
                threshold_policy(60_000),
                large_request(),
                &cancellation,
            ),
            async {
                gate.wait_until_started().await;
                cancellation.cancel();
                gate.release();
            }
        );

        assert!(matches!(
            outcome.terminal,
            PersistedTerminal::Cancelled { evidence: None, .. }
        ));
        assert_eq!(
            labels,
            ["request", "compaction-started", "compaction:applied"]
        );
        assert!(agent.model.contexts().is_empty());
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "compaction", "cancelled"]
        );
        assert_eq!(session_counts(&session), (runs_before + 1, steps_before));
        assert!(matches!(
            writer.model_history().expect("read compacted context").entries(),
            [ModelContextEntry::CompactionSummary { summary }, ModelContextEntry::UserRequest { .. }]
                if summary == "applied summary"
        ));

        drop(writer);
        session.close().expect("close session");
    }

    type ScriptedStep = Box<
        dyn FnOnce(
                &mut (dyn FnMut(ModelProgress) + Send),
                &CancellationToken,
            ) -> Result<ModelStep, ModelFailure>
            + Send,
    >;

    enum ScriptedSummary {
        Text(String),
        FailedAfterStart(ModelFailure),
        FailedBeforeStart(ModelFailure),
        CancelledAfterStart,
        CancelledBeforeStart,
    }

    #[derive(Default)]
    struct RecoveryModel {
        steps: VecDeque<ScriptedStep>,
        summaries: VecDeque<ScriptedSummary>,
        contexts: Vec<Vec<ModelContextEntry>>,
        selections: Vec<ModelSelection>,
        summary_requests: Vec<SummaryRequest>,
    }

    impl RecoveryModel {
        fn new(steps: Vec<ScriptedStep>, summaries: Vec<ScriptedSummary>) -> Self {
            Self {
                steps: steps.into(),
                summaries: summaries.into(),
                ..Self::default()
            }
        }
    }

    impl Model for RecoveryModel {
        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            request: SummaryRequest,
            cancellation: &'a CancellationToken,
            attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a
        {
            async move {
                self.summary_requests.push(request);
                match self.summaries.pop_front().expect("scripted summary") {
                    ScriptedSummary::Text(text) => {
                        attempt_started();
                        Ok(SummaryResult {
                            text,
                            attempts: vec![call_usage(100)],
                        })
                    }
                    ScriptedSummary::FailedAfterStart(failure) => {
                        attempt_started();
                        Err(failure)
                    }
                    ScriptedSummary::FailedBeforeStart(failure) => Err(failure),
                    ScriptedSummary::CancelledAfterStart => {
                        attempt_started();
                        cancellation.cancel();
                        Err(ModelFailure::new(
                            ModelError::Cancelled,
                            vec![call_usage(100)],
                        ))
                    }
                    ScriptedSummary::CancelledBeforeStart => {
                        cancellation.cancel();
                        Err(ModelError::Cancelled.into())
                    }
                }
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.selections.push(selection);
                self.contexts.push(history.entries().to_vec());
                let step = self.steps.pop_front().expect("scripted step");
                step(progress, cancellation)
            }
        }
    }

    fn call_usage(input_tokens: u64) -> AttemptAccounting {
        AttemptAccounting::KnownUsage(crate::model::ModelUsage {
            model: DEFAULT_MODEL.id.to_owned(),
            service_tier: None,
            input_tokens: Some(input_tokens),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(1),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(input_tokens + 1),
        })
    }

    fn overflow_failure(message: &str) -> ProviderFailure {
        ProviderFailure::new(
            Some(400),
            Some("context_length_exceeded".to_owned()),
            message.to_owned(),
        )
        .with_context_overflow(true)
    }

    fn provider_step_failure(
        failure: ProviderFailure,
        chunk: Option<&'static str>,
        attempts: Vec<AttemptAccounting>,
    ) -> ScriptedStep {
        Box::new(move |progress, _| {
            progress(ModelProgress::AttemptStarted { attempt: 1 });
            if let Some(text) = chunk {
                progress(ModelProgress::AssistantTextChunk {
                    text: text.to_owned(),
                });
            }
            Err(ModelFailure::new(
                ModelError::Provider(Box::new(failure)),
                attempts,
            ))
        })
    }

    fn overflow_step(
        chunk: Option<&'static str>,
        attempts: Vec<AttemptAccounting>,
    ) -> ScriptedStep {
        provider_step_failure(overflow_failure("maximum context length"), chunk, attempts)
    }

    fn action_step(action: CompletedModelAction, attempts: Vec<AttemptAccounting>) -> ScriptedStep {
        Box::new(move |_, _| {
            Ok(ModelStep {
                action,
                reasoning_summary: String::new(),
                replay: ProviderReplay::new("openai", Vec::new()),
                attempts,
            })
        })
    }

    fn code_action(source: &str, call_id: &str) -> CompletedModelAction {
        CompletedModelAction::Code {
            source: source.to_owned(),
            call_id: ModelCallId::new(call_id),
            commentary: None,
        }
    }

    fn message_action(text: &str) -> CompletedModelAction {
        CompletedModelAction::Message {
            text: text.to_owned(),
        }
    }

    fn initial_overflow(
        generation: Option<&str>,
        attempts: Vec<AttemptAccounting>,
    ) -> OverflowRecovery {
        OverflowRecovery {
            failure: overflow_failure("maximum context length"),
            generation: generation.map(|text| GenerationEvidence {
                reasoning_summary: String::new(),
                reasoning: String::new(),
                assistant_text: text.to_owned(),
                decoded_python: String::new(),
            }),
            attempts,
        }
    }

    fn recovery_session(
        home: &TempDir,
        workspace: &TempDir,
        agent: &Agent<RecoveryModel>,
        gate: Option<SessionWriteGate>,
    ) -> (Session, SessionWriter) {
        let session = match gate {
            Some(gate) => Session::create_with_write_gate(
                home.path(),
                workspace.path(),
                default_selection(),
                agent.checkpoint(),
                gate,
            ),
            None => Session::create(
                home.path(),
                workspace.path(),
                default_selection(),
                agent.checkpoint(),
            ),
        }
        .expect("create session");
        let writer = session.writer();
        (session, writer)
    }

    fn billed_attempts(session: &Session, run_id: u64) -> Vec<AttemptAccounting> {
        let mut billed = Vec::new();
        for record in session.restore().expect("restore").records {
            if record.run_id != run_id {
                continue;
            }
            match record.kind {
                RestoredRecordKind::CompletedExecution { attempts, .. } => billed.extend(attempts),
                RestoredRecordKind::Compaction { outcome } => {
                    billed.extend(outcome.attempts().iter().cloned())
                }
                RestoredRecordKind::FinalMessage { terminal, .. }
                | RestoredRecordKind::CancelledRun { terminal }
                | RestoredRecordKind::FailedRun { terminal, .. } => {
                    let (ReadableTerminal::Final {
                        attempts,
                        overflow_recovery,
                        ..
                    }
                    | ReadableTerminal::Cancelled {
                        attempts,
                        overflow_recovery,
                        ..
                    }
                    | ReadableTerminal::Failed {
                        attempts,
                        overflow_recovery,
                        ..
                    }) = terminal;
                    billed.extend(attempts);
                    billed.extend(
                        overflow_recovery
                            .into_iter()
                            .flat_map(|recovery| recovery.attempts),
                    );
                }
                RestoredRecordKind::UserRequest { .. }
                | RestoredRecordKind::SkillActivation { .. } => {}
            }
        }
        billed
    }

    fn terminal_parts(
        terminal: &PersistedTerminal,
    ) -> (
        &'static str,
        &[AttemptAccounting],
        Option<&OverflowRecovery>,
    ) {
        match terminal {
            PersistedTerminal::Final {
                attempts,
                overflow_recovery,
                ..
            } => ("final", attempts, overflow_recovery.as_ref()),
            PersistedTerminal::Cancelled {
                attempts,
                overflow_recovery,
                ..
            } => ("cancelled", attempts, overflow_recovery.as_ref()),
            PersistedTerminal::Failed {
                attempts,
                overflow_recovery,
                ..
            } => ("failed", attempts, overflow_recovery.as_ref()),
        }
    }

    fn recovery_policy() -> CompactionSettings {
        CompactionSettings::new(16_384, 20_000)
    }

    fn run_step_settlements(session: &Session, run_id: u64) -> Vec<Option<i64>> {
        let connection = session.connection_for_test().expect("open session");
        let mut statement = connection
            .prepare("SELECT settled_entry_id FROM model_steps WHERE run_id = ?1 ORDER BY id")
            .expect("prepare step settlements");
        statement
            .query_map(
                [i64::try_from(run_id).expect("run ID fits SQLite")],
                |row| row.get(0),
            )
            .expect("query step settlements")
            .collect::<Result<Vec<_>, _>>()
            .expect("read step settlements")
    }

    fn entry_parent(session: &Session, entry_id: i64) -> (String, i64) {
        session
            .connection_for_test()
            .expect("open session")
            .query_row(
                "SELECT parent.kind, parent.id FROM entries AS child JOIN entries AS parent ON parent.id = child.parent_id WHERE child.id = ?1",
                [entry_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read entry parent")
    }

    #[tokio::test]
    async fn context_overflow_compacts_and_retries_same_pending_step_once() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent_with_model(
            &workspace,
            RecoveryModel::new(
                vec![
                    overflow_step(Some("partial"), vec![call_usage(1)]),
                    action_step(code_action("value = 1", "retry-code"), vec![call_usage(2)]),
                    action_step(message_action("done"), vec![call_usage(3)]),
                ],
                vec![ScriptedSummary::Text("overflow summary".to_owned())],
            ),
        )
        .await;
        let (session, writer) = recovery_session(&home, &workspace, &agent, None);
        seed_compactable_history(&writer).await;
        let (runs_before, steps_before) = session_counts(&session);
        let request = large_request();

        let (outcome, labels) = run_threshold_request(
            &mut agent,
            &writer,
            recovery_policy(),
            request.clone(),
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            outcome.terminal,
            PersistedTerminal::Final {
                step_id: steps_before as u64 + 2,
                text: "done".to_owned(),
                reasoning_summary: String::new(),
                replay: ProviderReplay::new("openai", Vec::new()),
                attempts: vec![call_usage(3)],
                overflow_recovery: Some(initial_overflow(Some("partial"), vec![call_usage(1)])),
            }
        );
        assert_eq!(
            labels,
            [
                "request",
                "step",
                "compaction-started",
                "generation-restarted",
                "compaction:applied",
                "retry-step",
                "execution",
                "step",
            ]
        );
        let model = &agent.model;
        assert_eq!(model.selections, [default_selection(); 3]);
        assert_eq!(model.summary_requests.len(), 1);
        assert_eq!(model.summary_requests[0].selection, default_selection());
        assert!(matches!(
            model.contexts[0].first(),
            Some(ModelContextEntry::UserRequest { text }) if text == "discarded request"
        ));
        assert_eq!(
            model.contexts[1],
            [
                ModelContextEntry::CompactionSummary {
                    summary: "overflow summary".to_owned(),
                },
                ModelContextEntry::UserRequest {
                    text: request.clone(),
                },
            ]
        );
        assert_eq!(model.contexts[2][..2], model.contexts[1][..]);
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "compaction", "execution", "final_reply"]
        );
        assert_eq!(
            session_counts(&session),
            (runs_before + 1, steps_before + 2)
        );
        let settlements = run_step_settlements(&session, outcome.run_id);
        let execution_entry = settlements[0].expect("retried step settles");
        let (parent_kind, compaction_entry) = entry_parent(&session, execution_entry);
        assert_eq!(parent_kind, "compaction");
        assert_eq!(entry_parent(&session, compaction_entry).0, "user_request");
        let restore = session.restore().expect("restore recovered run");
        let run_records = restore
            .records
            .iter()
            .filter(|record| record.run_id == outcome.run_id)
            .map(|record| &record.kind)
            .collect::<Vec<_>>();
        assert!(
            matches!(
                run_records.as_slice(),
                [
                    RestoredRecordKind::UserRequest { .. },
                    RestoredRecordKind::Compaction {
                        outcome: CompactionOutcome::Applied {
                            reason: CompactionReason::ContextOverflow,
                            ..
                        },
                    },
                    RestoredRecordKind::CompletedExecution { attempts, .. },
                    RestoredRecordKind::FinalMessage { .. },
                ] if attempts == &[call_usage(2)]
            ),
            "{run_records:?}"
        );
        assert_eq!(
            billed_attempts(&session, outcome.run_id),
            [call_usage(100), call_usage(2), call_usage(3), call_usage(1)]
        );

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn unrecovered_overflow_closes_once_with_independently_bounded_components() {
        let original = "x".repeat(3_000);
        let summary_failure = |status, message: String| {
            ModelFailure::new(
                ModelError::Provider(Box::new(
                    ProviderFailure::new(Some(status), None, message).with_context_overflow(true),
                )),
                vec![call_usage(100)],
            )
        };
        for case in ["failed", "no-op", "failed-before-start"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let summary = match case {
                "failed" => vec![ScriptedSummary::FailedAfterStart(summary_failure(
                    500,
                    "€".repeat(2_000),
                ))],
                "failed-before-start" => vec![ScriptedSummary::FailedBeforeStart(summary_failure(
                    503,
                    "y".repeat(3_000),
                ))],
                _ => Vec::new(),
            };
            let mut agent = agent_with_model(
                &workspace,
                RecoveryModel::new(
                    vec![provider_step_failure(
                        overflow_failure(&original),
                        Some("partial"),
                        vec![call_usage(1)],
                    )],
                    summary,
                ),
            )
            .await;
            let (session, writer) = recovery_session(&home, &workspace, &agent, None);
            if case != "no-op" {
                seed_compactable_history(&writer).await;
            }

            let (outcome, labels) = run_threshold_request(
                &mut agent,
                &writer,
                recovery_policy(),
                large_request(),
                &CancellationToken::new(),
            )
            .await;

            let PersistedTerminal::Failed {
                message,
                evidence: None,
                attempts,
                overflow_recovery: Some(recovery),
            } = &outcome.terminal
            else {
                panic!("{case}: {:?}", outcome.terminal);
            };
            assert!(attempts.is_empty(), "{case}");
            assert_eq!(
                recovery,
                &OverflowRecovery {
                    failure: overflow_failure(&original),
                    ..initial_overflow(Some("partial"), vec![call_usage(1)])
                },
                "{case}"
            );
            let (context, recovery_component) = message
                .split_once("\ncompaction recovery failed: ")
                .expect("two-part recovery message");
            assert_eq!(
                context,
                format!(
                    "context overflow: {}",
                    &overflow_failure(&original).to_string()[..2_000]
                ),
                "{case}"
            );
            let compactions = session
                .restore()
                .expect("restore")
                .records
                .into_iter()
                .filter_map(|record| match record.kind {
                    RestoredRecordKind::Compaction { outcome } => Some(outcome),
                    _ => None,
                })
                .collect::<Vec<_>>();
            match case {
                "failed" => {
                    let [
                        CompactionOutcome::Failed {
                            diagnostic,
                            reason: CompactionReason::ContextOverflow,
                            ..
                        },
                    ] = compactions.as_slice()
                    else {
                        panic!("one failed overflow compaction: {compactions:?}");
                    };
                    let prefix = "model provider failed: provider failure (HTTP 500): ";
                    assert_eq!(diagnostic.len(), 4_096);
                    assert!(diagnostic.starts_with(prefix));
                    assert_eq!(recovery_component, &diagnostic[..1_999]);
                    assert_eq!(message.len(), 4_046);
                    assert_eq!(
                        labels,
                        ["request", "step", "compaction-started", "compaction:failed"]
                    );
                }
                "no-op" => {
                    assert!(compactions.is_empty());
                    assert_eq!(
                        recovery_component,
                        "compaction could not remove a valid prefix"
                    );
                    assert_eq!(labels, ["request", "step"]);
                }
                _ => {
                    assert!(compactions.is_empty());
                    assert_eq!(
                        recovery_component,
                        &format!(
                            "model provider failed: provider failure (HTTP 503): {}",
                            "y".repeat(3_000)
                        )[..2_000]
                    );
                    assert_eq!(message.len(), 4_047);
                    assert_eq!(labels, ["request", "step"]);
                }
            }
            assert_eq!(agent.model.contexts.len(), 1, "{case}");
            assert_eq!(
                agent.model.summary_requests.len(),
                usize::from(case != "no-op"),
                "{case}"
            );
            assert_eq!(
                run_step_settlements(&session, outcome.run_id).len(),
                1,
                "{case}"
            );
            assert_eq!(
                run_entry_kinds(&session, outcome.run_id)
                    .last()
                    .map(String::as_str),
                Some("failed"),
                "{case}"
            );

            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn retry_failure_keeps_initial_generation_separate_and_never_recovers_again() {
        for case in ["different failure", "second overflow"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let retry: ScriptedStep = if case == "second overflow" {
                overflow_step(Some("second"), vec![call_usage(2)])
            } else {
                Box::new(|_, _| {
                    Err(ModelFailure::new(
                        ModelError::Provider(Box::new(ProviderFailure::new(
                            Some(500),
                            None,
                            "retry failed".to_owned(),
                        ))),
                        vec![call_usage(2)],
                    ))
                })
            };
            let mut agent = agent_with_model(
                &workspace,
                RecoveryModel::new(
                    vec![overflow_step(Some("partial"), vec![call_usage(1)]), retry],
                    vec![
                        ScriptedSummary::Text("overflow summary".to_owned()),
                        ScriptedSummary::Text("unexpected summary".to_owned()),
                    ],
                ),
            )
            .await;
            let (session, writer) = recovery_session(&home, &workspace, &agent, None);
            seed_compactable_history(&writer).await;

            let (outcome, _) = run_threshold_request(
                &mut agent,
                &writer,
                recovery_policy(),
                large_request(),
                &CancellationToken::new(),
            )
            .await;

            let expected_evidence = (case == "second overflow").then(|| {
                TerminalEvidence::Generation(GenerationEvidence {
                    reasoning_summary: String::new(),
                    reasoning: String::new(),
                    assistant_text: "second".to_owned(),
                    decoded_python: String::new(),
                })
            });
            let expected_message = if case == "second overflow" {
                "model provider failed: provider failure (HTTP 400) [context_length_exceeded]: maximum context length"
            } else {
                "model provider failed: provider failure (HTTP 500): retry failed"
            };
            assert_eq!(
                outcome.terminal,
                PersistedTerminal::Failed {
                    message: expected_message.to_owned(),
                    evidence: expected_evidence,
                    attempts: vec![call_usage(2)],
                    overflow_recovery: Some(initial_overflow(Some("partial"), vec![call_usage(1)])),
                },
                "{case}"
            );
            assert_eq!(agent.model.summary_requests.len(), 1, "{case}");
            assert_eq!(agent.model.contexts.len(), 2, "{case}");
            assert_eq!(
                run_entry_kinds(&session, outcome.run_id),
                ["user_request", "compaction", "failed"],
                "{case}"
            );
            assert_eq!(
                billed_attempts(&session, outcome.run_id),
                [call_usage(100), call_usage(2), call_usage(1)],
                "{case}"
            );

            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn disabled_or_unrecognised_overflow_fails_normally_without_summary() {
        for case in ["disabled", "unrecognised"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let failure = if case == "disabled" {
                overflow_failure("maximum context length")
            } else {
                ProviderFailure::new(Some(400), None, "maximum context length".to_owned())
            };
            let expected_message =
                ModelFailure::new(ModelError::Provider(Box::new(failure.clone())), Vec::new())
                    .to_string();
            let mut agent = agent_with_model(
                &workspace,
                RecoveryModel::new(
                    vec![provider_step_failure(
                        failure,
                        Some("partial"),
                        vec![call_usage(1)],
                    )],
                    vec![ScriptedSummary::Text("unexpected summary".to_owned())],
                ),
            )
            .await;
            let (session, writer) = recovery_session(&home, &workspace, &agent, None);
            seed_compactable_history(&writer).await;
            let policy = if case == "disabled" {
                recovery_policy().disabled()
            } else {
                recovery_policy()
            };

            let (outcome, labels) = run_threshold_request(
                &mut agent,
                &writer,
                policy,
                large_request(),
                &CancellationToken::new(),
            )
            .await;

            assert_eq!(
                outcome.terminal,
                PersistedTerminal::Failed {
                    message: expected_message,
                    evidence: initial_overflow(Some("partial"), Vec::new())
                        .generation
                        .map(TerminalEvidence::Generation),
                    attempts: vec![call_usage(1)],
                    overflow_recovery: None,
                },
                "{case}"
            );
            assert_eq!(labels, ["request", "step"], "{case}");
            assert!(agent.model.summary_requests.is_empty(), "{case}");

            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn overflow_recovery_cancellation_keeps_initial_evidence() {
        for case in ["before attempt", "after attempt", "during retry"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let (summary, retry): (ScriptedSummary, Option<ScriptedStep>) = match case {
                "before attempt" => (ScriptedSummary::CancelledBeforeStart, None),
                "after attempt" => (ScriptedSummary::CancelledAfterStart, None),
                _ => (
                    ScriptedSummary::Text("overflow summary".to_owned()),
                    Some(Box::new(|progress, cancellation| {
                        progress(ModelProgress::AttemptStarted { attempt: 1 });
                        progress(ModelProgress::AssistantTextChunk {
                            text: "retry".to_owned(),
                        });
                        cancellation.cancel();
                        Err(ModelFailure::new(
                            ModelError::Cancelled,
                            vec![call_usage(2)],
                        ))
                    })),
                ),
            };
            let mut agent = agent_with_model(
                &workspace,
                RecoveryModel::new(
                    [overflow_step(Some("partial"), vec![call_usage(1)])]
                        .into_iter()
                        .chain(retry)
                        .collect(),
                    vec![summary],
                ),
            )
            .await;
            let (session, writer) = recovery_session(&home, &workspace, &agent, None);
            seed_compactable_history(&writer).await;

            let (outcome, labels) = run_threshold_request(
                &mut agent,
                &writer,
                recovery_policy(),
                large_request(),
                &CancellationToken::new(),
            )
            .await;

            let (retry_evidence, retry_attempts, expected_labels, expected_kinds) = match case {
                "before attempt" => (
                    None,
                    Vec::new(),
                    vec!["request", "step"],
                    vec!["user_request", "cancelled"],
                ),
                "after attempt" => (
                    None,
                    Vec::new(),
                    vec![
                        "request",
                        "step",
                        "compaction-started",
                        "compaction:cancelled",
                    ],
                    vec!["user_request", "compaction", "cancelled"],
                ),
                _ => (
                    Some(TerminalEvidence::Generation(GenerationEvidence {
                        reasoning_summary: String::new(),
                        reasoning: String::new(),
                        assistant_text: "retry".to_owned(),
                        decoded_python: String::new(),
                    })),
                    vec![call_usage(2)],
                    vec![
                        "request",
                        "step",
                        "compaction-started",
                        "generation-restarted",
                        "compaction:applied",
                        "retry-step",
                    ],
                    vec!["user_request", "compaction", "cancelled"],
                ),
            };
            assert_eq!(
                outcome.terminal,
                PersistedTerminal::Cancelled {
                    evidence: retry_evidence,
                    attempts: retry_attempts,
                    overflow_recovery: Some(initial_overflow(Some("partial"), vec![call_usage(1)])),
                },
                "{case}"
            );
            assert_eq!(labels, expected_labels, "{case}");
            assert_eq!(
                run_entry_kinds(&session, outcome.run_id),
                expected_kinds,
                "{case}"
            );
            assert_eq!(
                run_step_settlements(&session, outcome.run_id).len(),
                1,
                "{case}"
            );
            assert!(
                session
                    .restore()
                    .expect("restore")
                    .records
                    .iter()
                    .all(|record| !matches!(
                        &record.kind,
                        RestoredRecordKind::Compaction { outcome }
                            if outcome.reason() != CompactionReason::ContextOverflow
                    )),
                "{case}"
            );

            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn cancellation_after_overflow_compaction_is_applied_suppresses_the_retry() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Compaction);
        let mut agent = agent_with_model(
            &workspace,
            RecoveryModel::new(
                vec![
                    overflow_step(None, vec![call_usage(1)]),
                    action_step(message_action("must not run"), Vec::new()),
                ],
                vec![ScriptedSummary::Text("overflow summary".to_owned())],
            ),
        )
        .await;
        let (session, writer) = recovery_session(&home, &workspace, &agent, Some(gate.clone()));
        seed_compactable_history(&writer).await;
        let cancellation = CancellationToken::new();

        let ((outcome, labels), ()) = tokio::join!(
            run_threshold_request(
                &mut agent,
                &writer,
                recovery_policy(),
                large_request(),
                &cancellation,
            ),
            async {
                gate.wait_until_started().await;
                cancellation.cancel();
                gate.release();
            }
        );

        assert_eq!(
            outcome.terminal,
            PersistedTerminal::Cancelled {
                evidence: None,
                attempts: Vec::new(),
                overflow_recovery: Some(initial_overflow(None, vec![call_usage(1)])),
            }
        );
        assert_eq!(
            labels,
            [
                "request",
                "step",
                "compaction-started",
                "compaction:applied"
            ]
        );
        assert_eq!(agent.model.contexts.len(), 1);
        assert!(matches!(
            writer.model_history().expect("read compacted context").entries(),
            [ModelContextEntry::CompactionSummary { summary }, ModelContextEntry::UserRequest { .. }]
                if summary == "overflow summary"
        ));
        assert_eq!(
            run_entry_kinds(&session, outcome.run_id),
            ["user_request", "compaction", "cancelled"]
        );
        assert_eq!(run_step_settlements(&session, outcome.run_id).len(), 1);

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancellation_from_retry_reset_events_starts_no_retry_call() {
        for trigger in ["generation-restarted", "compaction:applied", "retry-step"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let mut agent = agent_with_model(
                &workspace,
                RecoveryModel::new(
                    vec![
                        overflow_step(Some("partial"), vec![call_usage(1)]),
                        action_step(message_action("must not run"), vec![call_usage(2)]),
                    ],
                    vec![ScriptedSummary::Text("overflow summary".to_owned())],
                ),
            )
            .await;
            let (session, writer) = recovery_session(&home, &workspace, &agent, None);
            seed_compactable_history(&writer).await;
            let cancellation = CancellationToken::new();
            let mut events = Vec::new();

            let outcome = agent
                .run_persisted(
                    default_selection(),
                    recovery_policy(),
                    CrossProviderReasoning::Inline,
                    large_request(),
                    None,
                    &cancellation,
                    &writer,
                    &mut |event| {
                        events.push(event);
                        if persisted_event_labels(&events[events.len() - 1..]) == [trigger] {
                            cancellation.cancel();
                        }
                    },
                )
                .await
                .expect("run persisted request");

            assert_eq!(
                outcome.terminal,
                PersistedTerminal::Cancelled {
                    evidence: None,
                    attempts: Vec::new(),
                    overflow_recovery: Some(initial_overflow(Some("partial"), vec![call_usage(1)])),
                },
                "{trigger}"
            );
            assert_eq!(agent.model.contexts.len(), 1, "{trigger}");
            assert_eq!(
                persisted_event_labels(&events),
                [
                    "request",
                    "step",
                    "compaction-started",
                    "generation-restarted",
                    "compaction:applied",
                    "retry-step",
                ],
                "{trigger}"
            );
            assert!(
                matches!(
                    writer.model_history().expect("read compacted context").entries(),
                    [ModelContextEntry::CompactionSummary { summary }, ModelContextEntry::UserRequest { .. }]
                        if summary == "overflow summary"
                ),
                "{trigger}"
            );
            assert_eq!(
                run_entry_kinds(&session, outcome.run_id),
                ["user_request", "compaction", "cancelled"],
                "{trigger}"
            );
            assert_eq!(
                run_step_settlements(&session, outcome.run_id).len(),
                1,
                "{trigger}"
            );

            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn every_terminal_route_after_recovered_code_carries_the_initial_overflow_once() {
        for case in [
            "later final",
            "later provider failure",
            "later overflow",
            "duplicate call",
            "execution failure",
            "execution cancelled",
            "cancelled before execution",
            "cancelled at step start",
            "cancelled between steps",
        ] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
            let later: Option<ScriptedStep> = match case {
                "later final" => Some(action_step(message_action("done"), vec![call_usage(3)])),
                "later provider failure" => Some(provider_step_failure(
                    ProviderFailure::new(Some(500), None, "later failure".to_owned()),
                    None,
                    vec![call_usage(3)],
                )),
                "later overflow" => Some(overflow_step(None, vec![call_usage(3)])),
                "duplicate call" => Some(action_step(
                    code_action("value = 2", "retry-code"),
                    vec![call_usage(3)],
                )),
                "execution failure" => Some(action_step(
                    code_action("x = ' ' * (1 << 60)", "later-code"),
                    vec![call_usage(3)],
                )),
                "execution cancelled" => Some(action_step(
                    code_action("contents = await read('Cargo.toml')", "later-code"),
                    vec![call_usage(3)],
                )),
                "cancelled before execution" => Some(Box::new(|_, cancellation| {
                    cancellation.cancel();
                    Ok(ModelStep {
                        action: code_action("value = 2", "later-code"),
                        reasoning_summary: String::new(),
                        replay: ProviderReplay::new("openai", Vec::new()),
                        attempts: vec![call_usage(3)],
                    })
                })),
                _ => None,
            };
            let mut agent = agent_with_model(
                &workspace,
                RecoveryModel::new(
                    [
                        overflow_step(Some("partial"), vec![call_usage(1)]),
                        action_step(code_action("value = 1", "retry-code"), vec![call_usage(2)]),
                    ]
                    .into_iter()
                    .chain(later)
                    .collect(),
                    vec![
                        ScriptedSummary::Text("overflow summary".to_owned()),
                        ScriptedSummary::Text("unexpected summary".to_owned()),
                    ],
                ),
            )
            .await;
            let gate = (case == "cancelled between steps")
                .then(|| SessionWriteGate::new(SessionWriteKind::Settlement));
            let (session, writer) = recovery_session(&home, &workspace, &agent, gate.clone());
            seed_compactable_history(&writer).await;
            let cancellation = CancellationToken::new();
            let canceller = cancellation.clone();
            let mut steps_started = 0;
            let mut event = |event| match event {
                super::PersistedAgentEvent::ToolUpdated { .. } if case == "execution cancelled" => {
                    canceller.cancel()
                }
                super::PersistedAgentEvent::ModelStepStarted { .. } => {
                    steps_started += 1;
                    if case == "cancelled at step start" && steps_started == 3 {
                        canceller.cancel();
                    }
                }
                _ => {}
            };
            let run = agent.run_persisted(
                default_selection(),
                recovery_policy(),
                CrossProviderReasoning::Inline,
                large_request(),
                None,
                &cancellation,
                &writer,
                &mut event,
            );
            let (outcome, ()) = tokio::join!(run, async {
                if let Some(gate) = &gate {
                    gate.wait_until_started().await;
                    cancellation.cancel();
                    gate.release();
                }
            });
            let outcome = outcome.expect("run recovered request");

            let (kind, attempts, recovery) = terminal_parts(&outcome.terminal);
            let expected_kind = match case {
                "later final" => "final",
                "later provider failure"
                | "later overflow"
                | "duplicate call"
                | "execution failure" => "failed",
                _ => "cancelled",
            };
            assert_eq!(kind, expected_kind, "{case}: {:?}", outcome.terminal);
            assert_eq!(
                recovery,
                Some(&initial_overflow(Some("partial"), vec![call_usage(1)])),
                "{case}"
            );
            let later_attempts = if later_step_called(case) {
                vec![call_usage(3)]
            } else {
                Vec::new()
            };
            assert_eq!(attempts, later_attempts.as_slice(), "{case}");
            assert_eq!(agent.model.summary_requests.len(), 1, "{case}");
            let mut expected_billing = vec![call_usage(100), call_usage(2)];
            expected_billing.extend(later_attempts);
            expected_billing.push(call_usage(1));
            assert_eq!(
                billed_attempts(&session, outcome.run_id),
                expected_billing,
                "{case}"
            );

            drop(writer);
            session.close().expect("close session");
        }
    }

    fn later_step_called(case: &str) -> bool {
        !matches!(case, "cancelled at step start" | "cancelled between steps")
    }

    #[tokio::test]
    async fn ephemeral_run_keeps_multi_step_context_without_agent_state() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent_with_model(
            &workspace,
            HistoryAwareModel {
                contexts: Vec::new(),
            },
        )
        .await;
        let cancellation = CancellationToken::new();

        assert_eq!(
            agent
                .run(default_selection(), "first", &cancellation)
                .await
                .unwrap()
                .text(),
            "done"
        );
        assert_eq!(
            agent
                .run(default_selection(), "second", &cancellation)
                .await
                .unwrap()
                .text(),
            "done"
        );
        assert_eq!(agent.model.contexts.len(), 4);
        assert!(matches!(
            agent.model.contexts[0].as_slice(),
            [ModelContextEntry::UserRequest { text }] if text == "first"
        ));
        assert!(matches!(
            agent.model.contexts[1].as_slice(),
            [ModelContextEntry::UserRequest { text }, ModelContextEntry::CompletedCode(_)] if text == "first"
        ));
        assert!(matches!(
            agent.model.contexts[2].as_slice(),
            [ModelContextEntry::UserRequest { text }] if text == "second"
        ));
        assert!(matches!(
            agent.model.contexts[3].as_slice(),
            [ModelContextEntry::UserRequest { text }, ModelContextEntry::CompletedCode(_)] if text == "second"
        ));
        let execution = agent
            .arena
            .execute("value", &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result_head(&execution), "2");
    }

    struct ImmediateChildResultRunner;

    impl ChildRunner<ChildPayload> for ImmediateChildResultRunner {
        fn run(
            &self,
            _child: ChildRun<ChildPayload>,
            _cancellation: CancellationToken,
        ) -> RunnerFuture {
            Box::pin(async {
                Ok(ChildResult::new(
                    "hidden child message".to_owned(),
                    BTreeMap::from([("hidden".to_owned(), ToolValue::Integer(42))]),
                ))
            })
        }
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
    async fn assignment_only_wait_keeps_child_results_out_of_model_feedback() {
        let workspace = tempfile::tempdir().unwrap();
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let coordinator: SessionSubagents = SubagentCoordinator::new(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(ImmediateChildResultRunner),
        );
        let context = AgentInvocationContext::new(coordinator.root_caller().await, coordinator)
            .with_accepted_thinking(AcceptedThinking::new(
                selection,
                snapshot.settings().clone(),
                None,
            ));
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "task = await start_subagent('child')".to_owned(),
                },
                ModelAction::Code {
                    source: "result = await task.wait()".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        agent.set_invocation_context(context);

        assert_eq!(
            agent
                .run(selection, "inspect", &CancellationToken::new())
                .await
                .unwrap()
                .text(),
            "done"
        );
        let feedback = agent.model.contexts()[2]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .expect("final model step receives the wait execution")
            .result();
        assert_eq!(
            feedback,
            &CodeResult::Execution {
                feedback: String::new()
            }
        );
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
            agent
                .run(default_selection(), "inspect", &cancellation)
                .await
                .unwrap()
                .text(),
            "done"
        );
        assert_eq!(agent.model.contexts().len(), 2);
        assert_eq!(
            agent.model.contexts()[0],
            [ModelContextEntry::UserRequest {
                text: "inspect".to_owned(),
            }]
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
    async fn agent_returns_type_errors_to_model_before_retrying_code() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "wrong: int = 'text'\nawait write('rejected.txt', 'bad')".to_owned(),
                },
                ModelAction::Code {
                    source: "await write('accepted.txt', 'good')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;

        assert_eq!(
            agent
                .run(default_selection(), "inspect", &CancellationToken::new())
                .await
                .unwrap()
                .text(),
            "done"
        );
        let rejected = agent.model.contexts()[1]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .unwrap();
        assert!(matches!(rejected.result(), CodeResult::Execution { .. }));
        assert!(rejected.result().feedback().contains("invalid-assignment"));
        assert!(rejected.result().feedback().contains("before execution"));
        let accepted = agent.model.contexts()[2]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .unwrap();
        assert_eq!(accepted.result().feedback(), "");
        assert!(!workspace.path().join("rejected.txt").exists());
        assert_eq!(
            fs::read_to_string(workspace.path().join("accepted.txt")).unwrap(),
            "good"
        );
    }

    #[tokio::test]
    async fn persisted_type_error_is_visible_on_the_next_model_step() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let selection = default_selection();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "wrong: int = 'text'\nawait write('bad.txt', 'bad')".to_owned(),
                },
                ModelAction::Code {
                    source: "await write('good.txt', 'good')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let session =
            Session::create(home.path(), workspace.path(), selection, agent.checkpoint()).unwrap();
        let writer = session.writer();
        let mut events = Vec::new();
        let outcome = agent
            .run_persisted(
                selection,
                CompactionSettings::default(),
                CrossProviderReasoning::Inline,
                "inspect".to_owned(),
                None,
                &CancellationToken::new(),
                &writer,
                &mut |event| events.push(event),
            )
            .await
            .unwrap();

        assert!(matches!(outcome.terminal, PersistedTerminal::Final { .. }));
        assert!(!workspace.path().join("bad.txt").exists());
        assert_eq!(
            fs::read_to_string(workspace.path().join("good.txt")).unwrap(),
            "good"
        );
        let next_context = agent.model.contexts()[1]
            .last()
            .and_then(ModelContextEntry::completed_code)
            .expect("retry should receive rejected code");
        assert!(
            next_context
                .result()
                .feedback()
                .contains("invalid-assignment")
        );
        let history = writer.model_history().unwrap();
        let completed = history
            .entries()
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 2);
        assert!(
            completed[0]
                .result()
                .feedback()
                .contains("invalid-assignment")
        );
        assert_eq!(completed[1].result().feedback(), "");
        assert!(events.iter().any(|event| matches!(
            event,
            super::PersistedAgentEvent::ExecutionCommitted { execution, .. }
                if matches!(execution.outcome, ExecutionOutcome::TypeCheckError(_))
        )));
        drop(writer);
        session.close().unwrap();
    }

    #[tokio::test]
    async fn child_result_exports_can_be_collected_after_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "result_values = {'answer': 42}".to_owned(),
                },
                ModelAction::Message {
                    text: "child complete".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        assert_eq!(
            agent
                .run(default_selection(), "inspect", &cancellation)
                .await
                .unwrap()
                .text(),
            "child complete"
        );
        assert_eq!(
            agent
                .collect_child_result_values(&cancellation)
                .await
                .unwrap(),
            std::collections::BTreeMap::from([("answer".to_owned(), ToolValue::Integer(42))])
        );
    }

    struct CompletedSummaryModel {
        step: u8,
    }

    impl Model for CompletedSummaryModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
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
                            commentary: Some("I’ll calculate this first.".to_owned()),
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

        let final_message = {
            let mut capture = |event| progress.push(event);
            agent
                .run_with_progress(default_selection(), "inspect", &cancellation, &mut capture)
                .await
                .unwrap()
        };

        assert!(
            !progress
                .iter()
                .any(|event| matches!(event, AgentProgress::ModelProgress(_)))
        );
        let (execution_commentary, execution_reasoning_summary) = progress
            .iter()
            .find_map(|event| match event {
                AgentProgress::ExecutionCompleted {
                    commentary,
                    reasoning_summary,
                    ..
                } => Some((commentary.as_deref(), reasoning_summary.as_str())),
                _ => None,
            })
            .expect("completed execution should carry its model output");
        assert_eq!(execution_commentary, Some("I’ll calculate this first."));
        assert_eq!(execution_reasoning_summary, "code summary");
        assert_eq!(final_message.text(), "done");
    }

    struct SummaryCodeModel {
        source: String,
        reasoning_summary: String,
    }

    impl Model for SummaryCodeModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
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
                        commentary: None,
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
            _selection: ModelSelection,
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
            _selection: ModelSelection,
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
            successful
                .run(default_selection(), "inspect", &cancellation)
                .await
                .unwrap()
                .attempts(),
            &[AttemptAccounting::UsageUnavailable]
        );

        let mut failed = agent_with_model(&workspace, PartialFailureModel).await;
        let error = failed
            .run(default_selection(), "inspect", &cancellation)
            .await
            .unwrap_err();

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
    }

    #[tokio::test]
    async fn agent_forwards_opaque_tool_row_tokens() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut agent = agent(
            &workspace,
            vec![
                ModelAction::Code {
                    source: "await read('file')".to_owned(),
                },
                ModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
        )
        .await;
        let cancellation = CancellationToken::new();

        let (_, progress) = run_capturing_progress(&mut agent, "inspect", &cancellation)
            .await
            .unwrap();
        let updates = progress
            .into_iter()
            .filter_map(|event| match event {
                AgentProgress::ToolUpdated { token, call } => Some((token, call)),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(matches!(
            updates.as_slice(),
            [
                (pending, ToolCall { summary: ToolCallSummary::Pending, .. }),
                (settled, ToolCall { summary: ToolCallSummary::Count(8), .. }),
            ] if pending == settled
        ));
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
                    .run_with_progress(default_selection(), "inspect", &cancellation, &mut capture)
                    .await
                    .unwrap()
                    .text(),
                "done"
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
        let completed = agent.model.contexts()[1]
            .iter()
            .filter_map(ModelContextEntry::completed_code)
            .next()
            .expect("second model step should receive the stdout feedback");
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
                .run_with_progress(default_selection(), "inspect", &cancellation, &mut capture)
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
            _selection: ModelSelection,
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

        let ((), error) = tokio::join!(
            cancel_when_ready,
            agent.run(default_selection(), "inspect", &cancellation)
        );
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

        let evidence =
            super::ExecutionEvidence::failed("source".to_owned(), None, String::new(), &error);

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
    async fn agent_returns_interrupted_execution_evidence_when_code_is_cancelled() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let mut agent = agent_with_model(
            &workspace,
            SummaryCodeModel {
                source: "contents = await read('Cargo.toml')".to_owned(),
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
                .run_with_progress(default_selection(), "inspect", &cancellation, &mut progress)
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
        assert_eq!(evidence.source, "contents = await read('Cargo.toml')");
        assert_eq!(evidence.reasoning_summary, "cancelled code summary");
        assert_eq!(evidence.began, execution.began);
        assert_eq!(evidence.stdout, execution.stdout);
        assert_eq!(evidence.stderr, execution.stderr);
        assert_eq!(evidence.tool_calls, execution.tool_calls);
        assert_eq!(
            evidence.cancellation_warning.as_deref(),
            Some(super::CANCELLATION_WARNING)
        );
        let feedback = evidence.feedback();
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
        let mut run = Box::pin(agent.run(default_selection(), "inspect", &cancellation));

        assert!(timeout(Duration::from_millis(5), &mut run).await.is_err());
        assert_eq!(run.await.unwrap().text(), "done");
        assert!(matches!(
            agent.model.contexts()[1].as_slice(),
            [
                ModelContextEntry::UserRequest { .. },
                ModelContextEntry::CompletedCode(_)
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
            _selection: ModelSelection,
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
    async fn agent_returns_a_completed_message_after_model_boundary_cancellation() {
        for action in [
            CompletedModelAction::Code {
                source: "boundary_action_ran = True".to_owned(),
                call_id: ModelCallId::new("boundary-code"),
                commentary: None,
            },
            CompletedModelAction::Message {
                text: "returned final".to_owned(),
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
                    .run_with_progress(default_selection(), "inspect", &cancellation, &mut capture)
                    .await
            };

            assert!(!progress.iter().any(|event| matches!(
                event,
                AgentProgress::ExecutionStarted { .. } | AgentProgress::ExecutionCompleted { .. }
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
                let feedback = evidence.feedback();
                assert!(!feedback.contains("boundary summary"));
                assert!(feedback.contains("Execution did not begin."));
                assert!(feedback.contains(
                    "Execution was cancelled and its Python arena changes were discarded. External side effects completed before cancellation may remain even though pending actions were cancelled."
                ));
            } else {
                assert_eq!(result.unwrap().text(), "returned final");
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
    async fn agent_skips_a_pre_cancelled_run_without_provider_work() {
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
        let observed_contexts = agent.model.contexts().to_vec();
        let action_position = agent.model.position();
        let cancelled = CancellationToken::new();
        cancelled.cancel();

        let error = agent
            .run(default_selection(), "cancelled", &cancelled)
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::Cancelled { .. }));
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

        let error = agent
            .run(default_selection(), "inspect", &cancellation)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AgentError::Model { failure, .. } if failure.error() == &ModelError::Exhausted
        ));
        assert_eq!(agent.model.contexts().len(), 1);
        assert_eq!(agent.model.position(), 0);
        assert_eq!(
            agent.model.contexts()[0],
            [ModelContextEntry::UserRequest {
                text: "inspect".to_owned(),
            }]
        );
    }

    #[tokio::test]
    async fn agent_cancellation_recovers_the_arena_without_consuming_later_actions() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let gate = ReadTestGate::new();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(gate.clone()),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "print('before')\ncontents = await read('Cargo.toml')".to_owned(),
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

        let ((), error) = tokio::join!(
            cancel_when_read_starts,
            agent.run(default_selection(), "cancel", &cancellation)
        );
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
            "print('before')\ncontents = await read('Cargo.toml')"
        );
        assert_eq!(evidence.stdout, execution.stdout);
        assert_eq!(evidence.stderr, execution.stderr);
        assert_eq!(evidence.tool_calls, execution.tool_calls);
        assert_eq!(
            evidence.cancellation_warning.as_deref(),
            Some(super::CANCELLATION_WARNING)
        );
        let feedback = evidence.feedback();
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
    async fn agent_cancellation_evidence_keeps_settled_file_paths() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("set")).unwrap();
        fs::write(workspace.path().join("set/tled"), "contents").unwrap();
        let gate = ReadTestGate::new();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(gate.clone()),
        )
        .await
        .expect("create arena");
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![ModelAction::Code {
                source: "value = 'discarded'\nawait read('./set//tled')".to_owned(),
            }]),
        );
        agent
            .arena
            .execute("value = 'before'", &CancellationToken::new())
            .await
            .expect("set checkpoint state");
        let cancellation = CancellationToken::new();
        let mut run = Box::pin(agent.run(default_selection(), "cancel", &cancellation));
        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut run => panic!("agent returned before its read started: {result:?}"),
        }
        gate.resume_read();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut run => panic!("agent returned before its read settled: {result:?}"),
        }
        cancellation.cancel();
        let AgentError::Arena { evidence, .. } = run.await.unwrap_err() else {
            panic!("expected cancelled Arena evidence");
        };
        assert_eq!(evidence.read_paths, [PathBuf::from("set/tled")]);
        assert!(evidence.modified_paths.is_empty());
        assert!(!format!("{evidence:?}").contains("set/tled"));
        let restored = agent
            .arena
            .execute("value", &CancellationToken::new())
            .await
            .expect("arena restores checkpoint state");
        assert_eq!(result_head(&restored), "'before'");
    }

    #[tokio::test]
    async fn agent_returns_arena_failure_evidence() {
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

        let error = agent
            .run(default_selection(), "inspect", &cancellation)
            .await
            .unwrap_err();

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
        let feedback = evidence.feedback();
        assert!(!feedback.contains("failed code summary"));
    }

    #[tokio::test]
    async fn persisted_cancelled_timeout_restores_failure_code_result() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let arena = Arena::with_limits(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::ZERO,
            Some(64 * 1024 * 1024),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .unwrap();
        let mut agent = Agent::new(
            arena,
            ScriptedModel::new(vec![ModelAction::Code {
                source: "value = 1".to_owned(),
            }]),
        );
        let cancellation = CancellationToken::new();
        let writer = session.writer();
        let outcome = {
            let mut event = |_| {};
            let run = agent.run_persisted(
                selection,
                CompactionSettings::default(),
                CrossProviderReasoning::Inline,
                "time out then cancel".to_owned(),
                None,
                &cancellation,
                &writer,
                &mut event,
            );
            tokio::pin!(run);
            let cancel_at_closure = async {
                gate.wait_until_started().await;
                cancellation.cancel();
                gate.release();
            };
            let (outcome, ()) = tokio::join!(run, cancel_at_closure);
            outcome.unwrap()
        };
        assert!(matches!(
            outcome.terminal,
            crate::session::PersistedTerminal::Cancelled { .. }
        ));
        assert!(matches!(
            writer
                .model_history()
                .expect("read persisted context")
                .entries()
                .last()
                .and_then(ModelContextEntry::completed_code)
                .map(|code| code.result()),
            Some(CodeResult::Failure { .. })
        ));

        let restore = session.restore().unwrap();
        assert!(restore.executable.is_some());
        assert!(matches!(
            restore.records.as_slice(),
            [
                crate::session::RestoredRecord {
                    kind: crate::session::RestoredRecordKind::UserRequest { .. },
                    ..
                },
                crate::session::RestoredRecord {
                    kind: crate::session::RestoredRecordKind::CancelledRun { .. },
                    ..
                }
            ]
        ));
        assert!(matches!(
            restore
                .executable
                .as_ref()
                .and_then(|state| state.history.entries().last())
                .and_then(ModelContextEntry::completed_code)
                .map(|code| code.result()),
            Some(CodeResult::Failure { .. })
        ));

        drop(writer);
        session.close().unwrap();
    }

    #[tokio::test]
    async fn persisted_request_admission_carries_its_committed_entry() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut agent = agent(
            &workspace,
            vec![ModelAction::Message {
                text: "done".to_owned(),
            }],
        )
        .await;
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create(home.path(), workspace.path(), selection, agent.checkpoint())
            .expect("create session");
        let mut events = Vec::new();
        let outcome = {
            let mut event = |event| events.push(event);
            agent
                .run_persisted(
                    selection,
                    CompactionSettings::default(),
                    CrossProviderReasoning::Inline,
                    "request entry".to_owned(),
                    None,
                    &CancellationToken::new(),
                    &session.writer(),
                    &mut event,
                )
                .await
                .expect("run persisted agent")
        };
        let request_entry = events
            .into_iter()
            .find_map(|event| match event {
                super::PersistedAgentEvent::RequestCommitted { request_entry, .. } => {
                    Some(request_entry)
                }
                _ => None,
            })
            .expect("request admission event");
        let committed_entry: i64 = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT id FROM entries WHERE run_id = ?1 AND kind = 'user_request'",
                [i64::try_from(outcome.run_id).expect("run ID fits SQLite")],
                |row| row.get(0),
            )
            .expect("read committed request entry");
        assert_eq!(request_entry, SessionEntryId(committed_entry as u64));
        session.close().expect("close session");
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
                    source: "value + 1  # ty: ignore[unresolved-reference]".to_owned(),
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
                agent
                    .run(default_selection(), "inspect", &cancellation)
                    .await
                    .unwrap()
                    .text(),
                "done"
            );
            let completed = agent.model.contexts()[1]
                .iter()
                .filter_map(ModelContextEntry::completed_code)
                .collect::<Vec<_>>();

            assert_eq!(completed.len(), 1);
            assert!(matches!(
                completed[0].result(),
                CodeResult::Execution { .. }
            ));
        }
    }

    #[tokio::test]
    async fn agent_reports_completed_code_before_a_later_cancellation_check() {
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
                .run_with_progress(default_selection(), "inspect", &cancellation, &mut capture)
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
                AgentProgress::ExecutionStarted {
                    commentary: Some(commentary),
                },
                AgentProgress::ExecutionCompleted {
                    reasoning_summary,
                    ..
                },
            ] if commentary == "I’ll calculate this first."
                && reasoning_summary == "code summary"
        ));
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
            read_paths: vec![PathBuf::from("private/effect-sentinel")],
            modified_paths: vec![PathBuf::from("private/mutation-sentinel")],
        };

        let feedback = super::execution_feedback(&execution);
        assert!(!feedback.contains("private/effect-sentinel"));
        assert!(!feedback.contains("private/mutation-sentinel"));
        assert_eq!(
            feedback,
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
    async fn agent_default_script_reads_retained_contents_and_returns_its_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let cargo_toml = "[package]\nname = \"example\"\n";
        fs::write(workspace.path().join("Cargo.toml"), cargo_toml).unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
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
            "try:\n    request_count += 1  # ty: ignore[unresolved-reference]\nexcept NameError:\n    request_count = 1\ncontents = await read(\"Cargo.toml\")",
        );
        assert_eq!(
            executions[1].0,
            "print(contents[0:20], end=\"\")\nrequest_count"
        );
        assert_eq!(executions[1].1.stdout.head, &cargo_toml[..20]);
        assert_eq!(result_head(executions[1].1), "1");
        assert_eq!(
            agent.model.contexts()[0][0],
            ModelContextEntry::UserRequest {
                text: "inspect".to_owned(),
            }
        );
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
            read_paths: Vec::new(),
            modified_paths: Vec::new(),
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
