use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio_util::sync::CancellationToken;

use crate::agent::{
    Agent, AgentProgress, ManualCompactionResult, PersistedAgentEvent, PersistedRunOutcome,
    TerminalEvidence,
};
use crate::arena::{Execution, RunPreview, ToolCall, ToolCallSummary, ToolCallToken, ToolName};
use crate::config::{
    CompactionSettings, ModelSelection, models::Provider, settings::SettingsContext,
};
#[cfg(test)]
use crate::config::{DEFAULT_MODEL, ModelConfiguration};
use crate::model::{AttemptAccounting, Model, ModelProgress};
use crate::session::{
    CompactionOutcome, PersistedTerminal, ReadableTerminal, RestoredRecord, RestoredRecordKind,
    Session, SessionCatalogue, SessionEntryId, SessionError, SessionMetadata, SessionRestore,
    SessionWriter,
};
use crate::subagents::{
    AcceptedThinking, AgentInvocationContext, Caller, CoordinatorEvent, SessionSubagents, TaskId,
    TaskObservation, TaskRecord,
};
#[cfg(test)]
use crate::subagents::{
    ChildFailure, ChildPayload, ChildRunner, RunnerFuture, SubagentCoordinator, SubagentPolicy,
};
use crate::telemetry::{ControlCommand, Settlement, Trace, TraceEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RunId(pub(crate) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ModelStepId(pub(crate) u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeCommand {
    Submit {
        request: String,
    },
    #[allow(dead_code)]
    Compact {
        focus: String,
    },
    ApplySettings {
        selection: ModelSelection,
        revision: u64,
    },
    SetSessionName {
        name: String,
        revision: u64,
    },
    Navigate {
        entry: SessionEntryId,
    },
    RequestTree {
        revision: u64,
    },
    Cancel {
        correction: bool,
    },
    CancelBackground,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompactionAcknowledgement {
    Accepted,
    Busy,
    Disabled { reason: RuntimeDisabledReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionLifecycle {
    Started,
    NoOp,
    Settled(CompactionSettlement),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionSettlement {
    Applied,
    Failed,
    Cancelled,
    CancelledBeforeStart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalDetails {
    pub(crate) evidence: Option<TerminalEvidence>,
    pub(crate) attempts: Vec<AttemptAccounting>,
}

impl TerminalDetails {
    fn new(evidence: Option<TerminalEvidence>, attempts: Vec<AttemptAccounting>) -> Self {
        Self { evidence, attempts }
    }

    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::new(None, Vec::new())
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    UserRequest {
        request: String,
        selection: Option<ModelSelection>,
    },
    CompletedExecution {
        step_id: ModelStepId,
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    },
    FinalMessage {
        step_id: ModelStepId,
        text: String,
        reasoning_summary: String,
        terminal: TerminalDetails,
    },
    CancelledRun {
        terminal: TerminalDetails,
    },
    FailedRun {
        message: String,
        terminal: TerminalDetails,
    },
    Compaction {
        outcome: CompactionOutcome,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeDisabledReason {
    OpenAiAuthentication,
    AnthropicAuthentication,
    ArenaCheckpointRecoveryFailed,
    SessionStorageWriteFailed,
    SessionRestoreFailed,
}

impl RuntimeDisabledReason {
    fn authentication(provider: Provider) -> Self {
        match provider {
            Provider::OpenAi => Self::OpenAiAuthentication,
            Provider::Anthropic => Self::AnthropicAuthentication,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredRecord {
    pub(crate) run_id: RunId,
    pub(crate) record: Record,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RestoredTimelineItem {
    Record(StoredRecord),
    SubagentSpawned(TaskRecord),
    SubagentFinished(TaskRecord),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeInitialState {
    pub(crate) metadata: Option<SessionMetadata>,
    pub(crate) timeline: Vec<RestoredTimelineItem>,
    pub(crate) footer_adjustment: Vec<AttemptAccounting>,
    pub(crate) disabled_reason: Option<RuntimeDisabledReason>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TreeRequestOutcome {
    Catalogue(SessionCatalogue),
    RejectedBusy,
    RejectedReadOnly,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NavigationOutcome {
    Replaced {
        state: RuntimeInitialState,
        request_prefill: Option<String>,
    },
    Unchanged {
        request_prefill: Option<String>,
    },
    RejectedBusy,
    RejectedReadOnly,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CorrectionOutcome {
    Replaced {
        state: RuntimeInitialState,
        request_prefill: Option<String>,
    },
    SkippedBackground,
    Failed,
}

impl RuntimeInitialState {
    pub(crate) fn empty() -> Self {
        Self {
            metadata: None,
            timeline: Vec::new(),
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RuntimeEvent {
    RunStarted {
        run_id: RunId,
    },
    ModelStepStarted {
        run_id: RunId,
        step_id: ModelStepId,
    },
    ModelProgress {
        run_id: RunId,
        step_id: ModelStepId,
        progress: ModelProgress,
    },
    ExecutionStarted {
        run_id: RunId,
        step_id: ModelStepId,
        commentary: Option<String>,
    },
    ToolUpdated {
        run_id: RunId,
        step_id: ModelStepId,
        token: ToolCallToken,
        call: ToolCall,
    },
    RunPreviewUpdated {
        run_id: RunId,
        step_id: ModelStepId,
        token: ToolCallToken,
        preview: RunPreview,
    },
    StdoutDelta {
        run_id: RunId,
        step_id: ModelStepId,
        text: String,
    },
    StdoutTruncated {
        run_id: RunId,
        step_id: ModelStepId,
    },
    RecordAppended {
        record: StoredRecord,
        progress_gap: bool,
    },
    SubagentSpawned {
        record: TaskRecord,
    },
    SubagentStatus {
        id: TaskId,
        revision: u64,
        observation: TaskObservation,
    },
    SubagentRecord {
        record: TaskRecord,
    },
    SubagentFinished {
        record: TaskRecord,
    },
    SubagentOperationSettled {
        id: TaskId,
    },
    RunSettled {
        run_id: RunId,
    },
    SettingsAccepted {
        revision: u64,
    },
    NameAccepted {
        revision: u64,
        name: String,
    },
    NavigationCompleted {
        outcome: NavigationOutcome,
    },
    CorrectionCompleted {
        run_id: RunId,
        outcome: CorrectionOutcome,
    },
    TreeCatalogue {
        revision: u64,
        outcome: TreeRequestOutcome,
    },
    SubmissionRejectedBusy {
        active_run_id: Option<RunId>,
    },
    CompactionAcknowledged {
        acknowledgement: CompactionAcknowledgement,
    },
    CompactionLifecycle {
        lifecycle: CompactionLifecycle,
    },
    RuntimeDisabled {
        reason: RuntimeDisabledReason,
    },
    SubmissionRejectedDisabled {
        reason: RuntimeDisabledReason,
    },
}

pub(crate) const PROGRESS_CHANNEL_CAPACITY: usize = 256;
const PROGRESS_PAYLOAD_LIMIT: usize = 4 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgressEnvelope<T> {
    pub(crate) item: T,
    pub(crate) gap_before: bool,
}

struct ProgressPublisher<T> {
    sender: mpsc::Sender<ProgressEnvelope<T>>,
    gap_pending: bool,
}

impl<T> ProgressPublisher<T> {
    fn new(sender: mpsc::Sender<ProgressEnvelope<T>>) -> Self {
        Self {
            sender,
            gap_pending: false,
        }
    }

    fn publish(&mut self, item: T, upstream_gap: bool, payload_bytes: usize) -> bool {
        if payload_bytes > PROGRESS_PAYLOAD_LIMIT {
            self.gap_pending = true;
            return false;
        }
        let gap_before = upstream_gap || self.gap_pending;
        match self.sender.try_send(ProgressEnvelope { item, gap_before }) {
            Ok(()) => {
                self.gap_pending = false;
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => {
                self.gap_pending = true;
                false
            }
        }
    }

    fn take_pending_gap(&mut self) -> bool {
        std::mem::take(&mut self.gap_pending)
    }
}

pub(crate) type RuntimeProgressReceiver = mpsc::Receiver<ProgressEnvelope<RuntimeEvent>>;

#[cfg(test)]
struct UnavailableChildRunner;

#[cfg(test)]
impl ChildRunner<ChildPayload> for UnavailableChildRunner {
    fn run(
        &self,
        _child: crate::subagents::ChildRun<ChildPayload>,
        _cancellation: CancellationToken,
    ) -> RunnerFuture {
        Box::pin(async { Err(ChildFailure::new("test child runner was not configured")) })
    }
}

pub(crate) struct RuntimeController<M: Model> {
    agent: Option<Agent<M>>,
    disabled_reason: Option<RuntimeDisabledReason>,
    records: Vec<StoredRecord>,
    session: SessionWriter,
    selection: ModelSelection,
    accepted_thinking: AcceptedThinking,
    subagents: SessionSubagents,
    subagent_events: Option<mpsc::UnboundedReceiver<CoordinatorEvent>>,
    subagent_record_revisions: BTreeMap<TaskId, u64>,
    subagent_status_revisions: BTreeMap<TaskId, u64>,
    compaction_policy: CompactionSettings,
    pending_compaction: Option<PendingCompaction>,
    active: Option<ActiveRun<M>>,
    trace: Trace,
    #[cfg(test)]
    test_session: Option<crate::session::Session>,
    #[cfg(test)]
    test_home: Option<tempfile::TempDir>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ForegroundOperation {
    Run,
    Compaction,
}

struct PendingCompaction {
    focus: String,
    cancellation: CancellationToken,
}

struct ActiveRun<M: Model> {
    operation: ForegroundOperation,
    provider: Provider,
    run_id: Option<RunId>,
    request_entry: Option<SessionEntryId>,
    correction_requested: bool,
    persisted_model_steps: BTreeSet<u64>,
    pending_busy_submissions: usize,
    caller: Caller,
    cancellation: CancellationToken,
    worker: mpsc::UnboundedReceiver<WorkerMessage<M>>,
    progress: mpsc::Receiver<ProgressEnvelope<WorkerProgress>>,
    progress_open: bool,
    cancellation_requested: bool,
}

#[allow(clippy::large_enum_variant)]
enum WorkerMessage<M: Model> {
    RequestCommitted {
        run_id: u64,
        request_entry: SessionEntryId,
        request: String,
        selection: ModelSelection,
    },
    PersistedModelStepStarted {
        run_id: u64,
        step_id: u64,
    },
    ExecutionStarted {
        run_id: u64,
        step_id: u64,
        commentary: Option<String>,
    },
    PersistedToolSettled {
        run_id: u64,
        step_id: u64,
        token: ToolCallToken,
        call: ToolCall,
    },
    PersistedExecutionCompleted {
        run_id: u64,
        step_id: u64,
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    },
    PersistedFinished {
        agent: Agent<M>,
        result: Result<PersistedRunOutcome, SessionError>,
        progress_gap: bool,
    },
    CompactionStarted,
    CompactionFinished {
        agent: Agent<M>,
        result: Result<ManualCompactionResult, SessionError>,
    },
}

struct WorkerProgress {
    run_id: RunId,
    step_id: ModelStepId,
    progress: AgentProgress,
}

struct PersistedToolUpdate {
    run_id: u64,
    step_id: u64,
    token: ToolCallToken,
    call: ToolCall,
}

fn flush_persisted_tool_updates<M: Model>(
    sender: &mpsc::UnboundedSender<WorkerMessage<M>>,
    updates: &mut Vec<PersistedToolUpdate>,
    run_id: u64,
    step_id: Option<u64>,
) {
    let mut remaining = Vec::new();
    for update in std::mem::take(updates) {
        if update.run_id == run_id && (step_id.is_none() || step_id == Some(update.step_id)) {
            let _ = sender.send(WorkerMessage::PersistedToolSettled {
                run_id: update.run_id,
                step_id: update.step_id,
                token: update.token,
                call: update.call,
            });
        } else {
            remaining.push(update);
        }
    }
    *updates = remaining;
}

#[allow(clippy::large_enum_variant)]
enum ControllerInput<M: Model> {
    Command(Option<RuntimeCommand>),
    Worker(Option<WorkerMessage<M>>),
    Progress(Option<ProgressEnvelope<WorkerProgress>>),
    SubagentsChanged,
    CoordinatorEvent(Option<CoordinatorEvent>),
    SubagentDeadline,
    StorageFailure,
}

pub(crate) fn control_command(command: &RuntimeCommand) -> Option<ControlCommand> {
    match command {
        RuntimeCommand::Cancel { .. } => Some(ControlCommand::Cancel),
        RuntimeCommand::CancelBackground => Some(ControlCommand::CancelBackground),
        RuntimeCommand::Shutdown => Some(ControlCommand::Shutdown),
        RuntimeCommand::Submit { .. }
        | RuntimeCommand::Compact { .. }
        | RuntimeCommand::ApplySettings { .. }
        | RuntimeCommand::SetSessionName { .. }
        | RuntimeCommand::Navigate { .. }
        | RuntimeCommand::RequestTree { .. } => None,
    }
}

fn record_from_persisted_terminal(terminal: PersistedTerminal) -> Record {
    match terminal {
        PersistedTerminal::Final {
            step_id,
            text,
            reasoning_summary,
            attempts,
            ..
        } => Record::FinalMessage {
            step_id: ModelStepId(step_id),
            text,
            reasoning_summary,
            terminal: TerminalDetails::new(None, attempts),
        },
        PersistedTerminal::Cancelled { evidence, attempts } => Record::CancelledRun {
            terminal: TerminalDetails::new(evidence, attempts),
        },
        PersistedTerminal::Failed {
            message,
            evidence,
            attempts,
        } => Record::FailedRun {
            message,
            terminal: TerminalDetails::new(evidence, attempts),
        },
    }
}

fn root_settlement(record: &Record) -> Settlement {
    match record {
        Record::FinalMessage { .. } => Settlement::Succeeded,
        Record::CancelledRun { .. } => Settlement::Cancelled,
        Record::FailedRun { .. } => Settlement::Failed,
        Record::UserRequest { .. }
        | Record::CompletedExecution { .. }
        | Record::Compaction { .. } => unreachable!("only terminal root records settle runs"),
    }
}

fn agent_progress_payload_bytes(progress: &AgentProgress) -> usize {
    match progress {
        AgentProgress::ModelProgress(progress) => model_progress_payload_bytes(progress),
        AgentProgress::ToolUpdated { .. } => 0,
        AgentProgress::RunPreviewUpdated { preview, .. } => {
            preview.command.len() + preview.stdout.len()
        }
        AgentProgress::StdoutDelta { text } => text.len(),
        AgentProgress::StdoutTruncated => 0,
        AgentProgress::ExecutionStarted { commentary, .. } => {
            commentary.as_ref().map_or(0, String::len)
        }
        AgentProgress::ModelStepStarted | AgentProgress::ExecutionCompleted { .. } => 0,
    }
}

fn runtime_event_payload_bytes(event: &RuntimeEvent) -> usize {
    match event {
        RuntimeEvent::ModelProgress { progress, .. } => model_progress_payload_bytes(progress),
        RuntimeEvent::ToolUpdated { .. } => 0,
        RuntimeEvent::RunPreviewUpdated { preview, .. } => {
            preview.command.len() + preview.stdout.len()
        }
        RuntimeEvent::StdoutDelta { text, .. } => text.len(),
        RuntimeEvent::StdoutTruncated { .. } => 0,
        RuntimeEvent::ExecutionStarted { commentary, .. } => {
            commentary.as_ref().map_or(0, String::len)
        }
        RuntimeEvent::RunStarted { .. }
        | RuntimeEvent::ModelStepStarted { .. }
        | RuntimeEvent::RecordAppended { .. }
        | RuntimeEvent::SubagentSpawned { .. }
        | RuntimeEvent::SubagentStatus { .. }
        | RuntimeEvent::SubagentRecord { .. }
        | RuntimeEvent::SubagentFinished { .. }
        | RuntimeEvent::SubagentOperationSettled { .. }
        | RuntimeEvent::RunSettled { .. }
        | RuntimeEvent::SettingsAccepted { .. }
        | RuntimeEvent::NameAccepted { .. }
        | RuntimeEvent::NavigationCompleted { .. }
        | RuntimeEvent::CorrectionCompleted { .. }
        | RuntimeEvent::TreeCatalogue { .. }
        | RuntimeEvent::SubmissionRejectedBusy { .. }
        | RuntimeEvent::CompactionAcknowledged { .. }
        | RuntimeEvent::CompactionLifecycle { .. }
        | RuntimeEvent::RuntimeDisabled { .. }
        | RuntimeEvent::SubmissionRejectedDisabled { .. } => 0,
    }
}

fn model_progress_payload_bytes(progress: &ModelProgress) -> usize {
    match progress {
        ModelProgress::ReasoningSummaryChunk { text }
        | ModelProgress::AssistantTextChunk { text } => text.len(),
        ModelProgress::DecodedPythonChunk { code } => code.len(),
        ModelProgress::AttemptStarted { .. } => 0,
        ModelProgress::AttemptSettled { accounting, .. } => {
            attempt_accounting_payload_bytes(accounting)
        }
    }
}

fn attempt_accounting_payload_bytes(accounting: &crate::model::AttemptAccounting) -> usize {
    match accounting {
        crate::model::AttemptAccounting::KnownUsage(usage)
        | crate::model::AttemptAccounting::KnownUsageWithUntrackedRetries(usage) => {
            usage.model.len() + usage.service_tier.as_ref().map_or(0, String::len)
        }
        crate::model::AttemptAccounting::UsageUnavailable => 0,
    }
}

fn restored_initial_state(restore: &SessionRestore) -> RuntimeInitialState {
    let mut ordered = restore
        .records
        .iter()
        .cloned()
        .map(|record| {
            (
                record.revision,
                false,
                record.revision,
                RestoredTimelineItem::Record(restored_stored_record(record)),
            )
        })
        .chain(restore.tasks.iter().flat_map(|task| {
            let admitted_revision = task.admitted_revision.max(task.admission_anchor_revision);
            let terminal_revision = task.terminal_revision.max(task.terminal_anchor_revision);
            [
                (
                    admitted_revision,
                    admitted_revision != task.admitted_revision,
                    task.admitted_revision,
                    RestoredTimelineItem::SubagentSpawned(task.record.clone()),
                ),
                (
                    terminal_revision,
                    terminal_revision != task.terminal_revision,
                    task.terminal_revision,
                    RestoredTimelineItem::SubagentFinished(task.record.clone()),
                ),
            ]
        }))
        .collect::<Vec<_>>();
    ordered.sort_by_key(|(revision, shifted, original_revision, _)| {
        (*revision, *shifted, *original_revision)
    });
    RuntimeInitialState {
        metadata: restore.metadata.clone(),
        timeline: ordered.into_iter().map(|(_, _, _, item)| item).collect(),
        footer_adjustment: restore.footer_adjustment.clone(),
        disabled_reason: restore
            .executable
            .is_none()
            .then_some(RuntimeDisabledReason::SessionRestoreFailed),
    }
}

fn restored_stored_record(record: RestoredRecord) -> StoredRecord {
    let run_id = record.run_id;
    let terminal_details = |terminal: ReadableTerminal| match terminal {
        ReadableTerminal::Final { attempts, .. } => TerminalDetails::new(None, attempts),
        ReadableTerminal::Cancelled { evidence, attempts }
        | ReadableTerminal::Failed {
            evidence, attempts, ..
        } => TerminalDetails::new(evidence, attempts),
    };
    let record = match record.kind {
        RestoredRecordKind::UserRequest { request, selection } => {
            Record::UserRequest { request, selection }
        }
        RestoredRecordKind::CompletedExecution {
            step_id,
            source,
            commentary,
            reasoning_summary,
            execution,
            attempts,
        } => Record::CompletedExecution {
            step_id: ModelStepId(step_id),
            source,
            commentary,
            reasoning_summary,
            execution,
            attempts,
        },
        RestoredRecordKind::FinalMessage {
            step_id,
            text,
            reasoning_summary,
            terminal,
        } => Record::FinalMessage {
            step_id: ModelStepId(step_id),
            text,
            reasoning_summary,
            terminal: terminal_details(terminal),
        },
        RestoredRecordKind::CancelledRun { terminal } => Record::CancelledRun {
            terminal: terminal_details(terminal),
        },
        RestoredRecordKind::FailedRun { message, terminal } => Record::FailedRun {
            message,
            terminal: terminal_details(terminal),
        },
        RestoredRecordKind::Compaction { outcome } => Record::Compaction { outcome },
    };
    StoredRecord {
        run_id: RunId(run_id),
        record,
    }
}

impl<M: Model> RuntimeController<M> {
    pub(crate) fn with_session(
        agent: Agent<M>,
        selection: ModelSelection,
        persisted_settings: SettingsContext,
        subagents: SessionSubagents,
        session: SessionWriter,
    ) -> Self {
        let compaction = persisted_settings.compaction();
        let accepted_thinking = AcceptedThinking::new(selection, persisted_settings, None);
        let subagents = subagents.with_session(session.clone());
        Self {
            agent: Some(agent),
            disabled_reason: None,
            records: Vec::new(),
            session,
            selection,
            accepted_thinking,
            subagent_events: subagents.take_event_receiver(),
            subagents,
            subagent_record_revisions: BTreeMap::new(),
            subagent_status_revisions: BTreeMap::new(),
            compaction_policy: compaction,
            pending_compaction: None,
            active: None,
            trace: Trace::disabled(),
            #[cfg(test)]
            test_session: None,
            #[cfg(test)]
            test_home: None,
        }
    }

    pub(crate) async fn restore(
        session: &Session,
        mut arena: crate::arena::Arena,
        model: M,
        selection: ModelSelection,
        persisted_settings: SettingsContext,
        subagents: SessionSubagents,
        relocated_cwd: Option<PathBuf>,
    ) -> Result<(Self, RuntimeInitialState), SessionError> {
        let restore = session.restore()?;
        let mut initial = restored_initial_state(&restore);
        let records = restore
            .records
            .iter()
            .cloned()
            .map(restored_stored_record)
            .collect();
        let selected_origins = restore
            .tasks
            .iter()
            .filter_map(|task| task.record.origin_step_id())
            .collect::<Vec<_>>();
        let restored_tasks = restore.all_tasks.iter().map(|task| task.record.clone());
        let subagents = subagents.with_session(session.writer());
        subagents
            .restore_terminal_tasks(
                restored_tasks,
                restore.next_task,
                restore.reserved_task_ids.clone(),
            )
            .await;
        assert!(
            subagents.replace_selected_origins(selected_origins).await,
            "restoration has no tracked subagent operations"
        );
        let compaction = persisted_settings.compaction();
        let accepted_thinking = AcceptedThinking::new(
            selection,
            persisted_settings,
            restore
                .metadata
                .as_ref()
                .map(|metadata| metadata.preferences().clone()),
        );
        let agent = if let Some(state) = restore.executable {
            if model.validate_history(&state.history, selection).is_ok()
                && arena.restore(state.checkpoint.clone()).await.is_ok()
            {
                let preferences = accepted_thinking.proposal(selection);
                match session
                    .writer()
                    .commit_startup_preferences(preferences.clone(), relocated_cwd)
                    .await
                {
                    Ok(()) => match session
                        .writer()
                        .activate(
                            restore.effective_entry,
                            restore.observed_active_entry,
                            state.history,
                        )
                        .await
                    {
                        Ok(()) => {
                            accepted_thinking.accept(selection);
                            initial
                                .metadata
                                .as_mut()
                                .expect("executable session restore must have metadata")
                                .update_preferences(preferences);
                            Some(Agent::new(arena, model))
                        }
                        Err(_) => {
                            subagents.latch_storage_failure().await;
                            initial.disabled_reason =
                                Some(RuntimeDisabledReason::SessionStorageWriteFailed);
                            None
                        }
                    },
                    Err(_) => {
                        subagents.latch_storage_failure().await;
                        initial.disabled_reason =
                            Some(RuntimeDisabledReason::SessionStorageWriteFailed);
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        if agent.is_none() && initial.disabled_reason.is_none() {
            initial.disabled_reason = Some(RuntimeDisabledReason::SessionRestoreFailed);
        }
        let subagent_events = subagents.take_event_receiver();
        Ok((
            Self {
                agent,
                disabled_reason: initial.disabled_reason.clone(),
                records,
                session: session.writer(),
                selection,
                accepted_thinking,
                subagents,
                subagent_events,
                subagent_record_revisions: BTreeMap::new(),
                subagent_status_revisions: BTreeMap::new(),
                compaction_policy: compaction,
                pending_compaction: None,
                active: None,
                trace: Trace::disabled(),
                #[cfg(test)]
                test_session: None,
                #[cfg(test)]
                test_home: None,
            },
            initial,
        ))
    }

    #[cfg(test)]
    fn new_for_test(
        agent: Agent<M>,
        selection: ModelSelection,
        persisted_settings: SettingsContext,
        subagents: SessionSubagents,
    ) -> Self {
        let home = tempfile::tempdir().expect("create session home");
        let session = crate::session::Session::create(
            home.path(),
            std::path::Path::new("/"),
            selection,
            agent.checkpoint(),
        )
        .expect("create persisted runtime session");
        let mut controller = Self::with_session(
            agent,
            selection,
            persisted_settings,
            subagents,
            session.writer(),
        );
        controller.test_session = Some(session);
        controller.test_home = Some(home);
        controller
    }

    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }

    pub(crate) fn with_providers(mut self, providers: Vec<Provider>) -> Self {
        self.accepted_thinking = self.accepted_thinking.with_providers(providers);
        self
    }

    #[cfg(test)]
    pub(crate) fn resolve_child_selection(
        &self,
        invoking: ModelSelection,
        target: Option<&'static ModelConfiguration>,
    ) -> ModelSelection {
        self.accepted_thinking.resolve(invoking, target)
    }

    #[cfg(test)]
    pub(crate) fn new(
        agent: Agent<M>,
        selection: ModelSelection,
        persisted_settings: SettingsContext,
        subagents: SessionSubagents,
    ) -> Self {
        Self::new_for_test(agent, selection, persisted_settings, subagents)
    }

    #[cfg(test)]
    fn for_test(agent: Agent<M>) -> Self {
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("default model selection must be valid");
        let snapshot = crate::config::ready_snapshot_for_test();
        Self::new_for_test(
            agent,
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                std::sync::Arc::new(UnavailableChildRunner),
            ),
        )
    }

    #[cfg(test)]
    fn with_selection(mut self, selection: ModelSelection) -> Self {
        self.accepted_thinking =
            AcceptedThinking::new(selection, self.accepted_thinking.persisted_settings(), None);
        self.selection = selection;
        self
    }

    pub(crate) async fn run(
        mut self,
        mut commands: mpsc::Receiver<RuntimeCommand>,
        events: mpsc::UnboundedSender<RuntimeEvent>,
        progress_sender: mpsc::Sender<ProgressEnvelope<RuntimeEvent>>,
    ) -> Vec<StoredRecord> {
        let mut progress = ProgressPublisher::new(progress_sender);
        let mut commands_open = true;
        let mut shutdown_requested = false;
        let mut subagent_updates = self.subagents.subscribe().await;
        let storage_failure = self.subagents.storage_failure_token();

        loop {
            if shutdown_requested && self.active.is_none() {
                self.subagents.shutdown().await;
                self.drain_coordinator_events(&events, &mut progress).await;
                return self.records;
            }

            let deadline = self.subagents.next_deadline().await;
            let deadline_wait = async {
                match deadline {
                    Some(deadline) => {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await
                    }
                    None => futures::future::pending::<()>().await,
                }
            };
            let input = if let Some(active) = self.active.as_mut() {
                let subagent_events = &mut self.subagent_events;
                tokio::select! {
                    biased;
                    _ = storage_failure.cancelled(), if self.disabled_reason != Some(RuntimeDisabledReason::SessionStorageWriteFailed) => ControllerInput::StorageFailure,
                    command = commands.recv(), if commands_open => ControllerInput::Command(command),
                    message = active.worker.recv() => ControllerInput::Worker(message),
                    worker_progress = active.progress.recv(), if active.progress_open => ControllerInput::Progress(worker_progress),
                    event = Self::next_coordinator_event(subagent_events) => ControllerInput::CoordinatorEvent(event),
                    _ = subagent_updates.changed() => ControllerInput::SubagentsChanged,
                    _ = deadline_wait => ControllerInput::SubagentDeadline,
                }
            } else {
                let subagent_events = &mut self.subagent_events;
                tokio::select! {
                    biased;
                    _ = storage_failure.cancelled(), if self.disabled_reason != Some(RuntimeDisabledReason::SessionStorageWriteFailed) => ControllerInput::StorageFailure,
                    command = commands.recv(), if commands_open => ControllerInput::Command(command),
                    event = Self::next_coordinator_event(subagent_events) => ControllerInput::CoordinatorEvent(event),
                    _ = subagent_updates.changed() => ControllerInput::SubagentsChanged,
                    _ = deadline_wait => ControllerInput::SubagentDeadline,
                }
            };

            match input {
                ControllerInput::Command(Some(command)) => {
                    self.handle_command(command, &events, &mut progress, &mut shutdown_requested)
                        .await;
                }
                ControllerInput::Command(None) => {
                    commands_open = false;
                    shutdown_requested = true;
                    self.request_shutdown().await;
                }
                ControllerInput::Worker(Some(message)) => {
                    self.handle_worker_message(message, &events, &mut progress)
                        .await;
                }
                ControllerInput::Worker(None) => {
                    shutdown_requested = true;
                    self.request_shutdown().await;
                    self.handle_worker_closed(&events, &mut progress).await;
                }
                ControllerInput::Progress(Some(progress_envelope)) => {
                    self.handle_worker_progress(progress_envelope, &mut progress);
                }
                ControllerInput::Progress(None) => {
                    self.active
                        .as_mut()
                        .expect("active run checked before closing worker progress")
                        .progress_open = false;
                }
                ControllerInput::SubagentsChanged => {}
                ControllerInput::CoordinatorEvent(Some(event)) => {
                    self.handle_coordinator_event(event, &events, &mut progress)
                        .await;
                }
                ControllerInput::CoordinatorEvent(None) => self.subagent_events = None,
                ControllerInput::SubagentDeadline => self.subagents.service().await,
                ControllerInput::StorageFailure => self.disable_session_storage(&events).await,
            }
            self.subagents.service().await;
        }
    }

    async fn next_coordinator_event(
        receiver: &mut Option<mpsc::UnboundedReceiver<CoordinatorEvent>>,
    ) -> Option<CoordinatorEvent> {
        match receiver {
            Some(receiver) => receiver.recv().await,
            None => futures::future::pending().await,
        }
    }

    async fn handle_command(
        &mut self,
        command: RuntimeCommand,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
        shutdown_requested: &mut bool,
    ) {
        if let Some(command) = control_command(&command) {
            self.trace
                .emit(|| TraceEvent::RuntimeCommandReceived { command });
        }
        match command {
            RuntimeCommand::Submit { request } => {
                if self.subagents.storage_failure_token().is_cancelled() {
                    self.disable_session_storage(events).await;
                }
                if let Some(reason) = &self.disabled_reason {
                    self.emit(
                        events,
                        RuntimeEvent::SubmissionRejectedDisabled {
                            reason: reason.clone(),
                        },
                    );
                } else if self.pending_compaction.is_some() {
                    self.emit(
                        events,
                        RuntimeEvent::SubmissionRejectedBusy {
                            active_run_id: None,
                        },
                    );
                } else if let Some(active) = &mut self.active {
                    if active.operation == ForegroundOperation::Compaction {
                        self.emit(
                            events,
                            RuntimeEvent::SubmissionRejectedBusy {
                                active_run_id: None,
                            },
                        );
                    } else if let Some(run_id) = active.run_id {
                        self.emit(
                            events,
                            RuntimeEvent::SubmissionRejectedBusy {
                                active_run_id: Some(run_id),
                            },
                        );
                    } else {
                        active.pending_busy_submissions += 1;
                    }
                } else {
                    self.start_run(request, events, progress).await;
                }
            }
            RuntimeCommand::Compact { focus } => {
                if self.subagents.storage_failure_token().is_cancelled() {
                    self.disable_session_storage(events).await;
                }
                let acknowledgement = if let Some(reason) = &self.disabled_reason {
                    CompactionAcknowledgement::Disabled {
                        reason: reason.clone(),
                    }
                } else if self.pending_compaction.is_some()
                    || self
                        .active
                        .as_ref()
                        .is_some_and(|active| active.operation == ForegroundOperation::Compaction)
                {
                    CompactionAcknowledgement::Busy
                } else {
                    let cancellation = self.subagents.storage_failure_token().child_token();
                    self.pending_compaction = Some(PendingCompaction {
                        focus,
                        cancellation,
                    });
                    if self.active.is_some() {
                        self.request_active_cancellation(false);
                    }
                    CompactionAcknowledgement::Accepted
                };
                let accepted = acknowledgement == CompactionAcknowledgement::Accepted;
                self.emit(
                    events,
                    RuntimeEvent::CompactionAcknowledged { acknowledgement },
                );
                if accepted {
                    self.start_pending_compaction().await;
                }
            }
            RuntimeCommand::ApplySettings {
                selection,
                revision,
            } => {
                if self.disabled_reason.is_some() {
                    return;
                }
                let preferences = self.accepted_thinking.proposal(selection);
                if self.session.commit_preferences(preferences).await.is_err() {
                    self.subagents.latch_storage_failure().await;
                    self.disable_session_storage(events).await;
                    return;
                }
                self.accepted_thinking.accept(selection);
                self.selection = selection;
                self.emit(events, RuntimeEvent::SettingsAccepted { revision });
            }
            RuntimeCommand::SetSessionName { name, revision } => {
                if self.disabled_reason.is_some() {
                    return;
                }
                match self.session.commit_name(name.clone()).await {
                    Ok(()) => self.emit(events, RuntimeEvent::NameAccepted { revision, name }),
                    Err(SessionError::EmptyName) => {}
                    Err(_) => {
                        self.subagents.latch_storage_failure().await;
                        self.disable_session_storage(events).await;
                    }
                }
            }
            RuntimeCommand::Navigate { entry } => {
                let outcome = if self.disabled_reason.is_some() {
                    NavigationOutcome::RejectedReadOnly
                } else if self.pending_compaction.is_some()
                    || self.active.is_some()
                    || self.subagents.operation_count() != 0
                {
                    NavigationOutcome::RejectedBusy
                } else {
                    self.drain_coordinator_events(events, progress).await;
                    let (outcome, disabled) = self.restore_selected_entry(entry).await;
                    self.emit(events, RuntimeEvent::NavigationCompleted { outcome });
                    self.disable_after_restoration(events, disabled).await;
                    return;
                };
                self.emit(events, RuntimeEvent::NavigationCompleted { outcome });
            }
            RuntimeCommand::RequestTree { revision } => {
                let storage_failure = self.subagents.storage_failure_token().is_cancelled();
                let outcome = if storage_failure || self.disabled_reason.is_some() {
                    TreeRequestOutcome::RejectedReadOnly
                } else if self.pending_compaction.is_some()
                    || self.active.is_some()
                    || self.subagents.operation_count() != 0
                {
                    TreeRequestOutcome::RejectedBusy
                } else {
                    self.drain_coordinator_events(events, progress).await;
                    match self.session.catalogue().await {
                        Ok(catalogue) => TreeRequestOutcome::Catalogue(catalogue),
                        Err(_) => TreeRequestOutcome::Failed,
                    }
                };
                self.emit(events, RuntimeEvent::TreeCatalogue { revision, outcome });
            }
            RuntimeCommand::Cancel { correction } => {
                self.cancel_foreground(correction, events);
            }
            RuntimeCommand::CancelBackground => self.request_background_cancellation().await,
            RuntimeCommand::Shutdown => {
                *shutdown_requested = true;
                self.request_shutdown().await;
            }
        }
    }

    async fn restore_selected_entry(
        &mut self,
        entry: SessionEntryId,
    ) -> (NavigationOutcome, Option<RuntimeDisabledReason>) {
        debug_assert!(self.agent.is_some(), "navigation requires the idle agent");
        debug_assert_eq!(
            self.subagents.operation_count(),
            0,
            "navigation requires settled child operations"
        );

        let restore = match self.session.restore_at(entry).await {
            Ok(restore) => restore,
            Err(_) => return (NavigationOutcome::Failed, None),
        };
        if restore.effective_entry == restore.observed_active_entry {
            return (
                NavigationOutcome::Unchanged {
                    request_prefill: restore.request_prefill,
                },
                None,
            );
        }
        let Some(executable) = restore.executable.as_ref() else {
            return (NavigationOutcome::Failed, None);
        };
        let Some(agent) = self.agent.as_ref() else {
            return (NavigationOutcome::Failed, None);
        };
        if agent
            .validate_restored_history(&executable.history, self.selection)
            .is_err()
        {
            return (NavigationOutcome::Failed, None);
        }
        let initial = restored_initial_state(&restore);
        let executable = restore
            .executable
            .expect("navigation validated executable state");
        let previous_checkpoint = agent.checkpoint();
        let target_restore = self
            .agent
            .as_mut()
            .expect("navigation checked the idle agent")
            .restore_arena(executable.checkpoint)
            .await;
        if let Err(error) = target_restore {
            if !matches!(
                error,
                crate::arena::ArenaError::IncompatibleCheckpoint { .. }
            ) && self
                .agent
                .as_mut()
                .expect("navigation retains its agent after a failed restore")
                .restore_arena(previous_checkpoint)
                .await
                .is_err()
            {
                return (
                    NavigationOutcome::Failed,
                    Some(RuntimeDisabledReason::ArenaCheckpointRecoveryFailed),
                );
            }
            return (NavigationOutcome::Failed, None);
        }
        if self
            .session
            .activate(
                restore.effective_entry,
                restore.observed_active_entry,
                executable.history,
            )
            .await
            .is_err()
        {
            let _ = self
                .agent
                .as_mut()
                .expect("navigation retains its agent after persistence failure")
                .restore_arena(previous_checkpoint)
                .await;
            return (
                NavigationOutcome::Failed,
                Some(RuntimeDisabledReason::SessionStorageWriteFailed),
            );
        }

        let selected_origins = restore
            .tasks
            .iter()
            .filter_map(|task| task.record.origin_step_id())
            .collect::<Vec<_>>();
        assert!(
            self.subagents
                .replace_selected_origins(selected_origins)
                .await,
            "navigation has no tracked child operations"
        );
        let request_prefill = restore.request_prefill;
        self.records = restore
            .records
            .into_iter()
            .map(restored_stored_record)
            .collect();
        (
            NavigationOutcome::Replaced {
                state: initial,
                request_prefill,
            },
            None,
        )
    }

    async fn disable_after_restoration(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        disabled: Option<RuntimeDisabledReason>,
    ) {
        let Some(reason) = disabled else {
            return;
        };
        match reason {
            RuntimeDisabledReason::ArenaCheckpointRecoveryFailed => {
                self.disable_arena_checkpoint_recovery(events);
            }
            RuntimeDisabledReason::SessionStorageWriteFailed => {
                self.subagents.latch_storage_failure().await;
                self.disable_session_storage(events).await;
            }
            RuntimeDisabledReason::OpenAiAuthentication
            | RuntimeDisabledReason::AnthropicAuthentication
            | RuntimeDisabledReason::SessionRestoreFailed => {
                unreachable!("restoration only disables for Arena or storage recovery")
            }
        }
    }

    fn disable_arena_checkpoint_recovery(&mut self, events: &mpsc::UnboundedSender<RuntimeEvent>) {
        if let Some(pending) = self.pending_compaction.take() {
            pending.cancellation.cancel();
        }
        self.disabled_reason = Some(RuntimeDisabledReason::ArenaCheckpointRecoveryFailed);
        self.emit(
            events,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            },
        );
    }

    async fn handle_coordinator_event(
        &mut self,
        event: CoordinatorEvent,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        match event {
            CoordinatorEvent::Spawned { record } => {
                self.emit(
                    events,
                    RuntimeEvent::SubagentSpawned {
                        record: record.clone(),
                    },
                );
                self.accept_subagent_record(&record);
                self.publish_subagent_status(progress, &record);
            }
            CoordinatorEvent::Changed { id, revision } => {
                let Some(record) = self.subagents.record(&id).await else {
                    return;
                };
                if record.revision() < revision || record.observation().state().is_terminal() {
                    return;
                }
                self.publish_subagent_record(events, &record);
                self.publish_subagent_status(progress, &record);
            }
            CoordinatorEvent::Finished { record } => {
                self.publish_subagent_record(events, &record);
                self.publish_subagent_status(progress, &record);
                self.emit(events, RuntimeEvent::SubagentFinished { record });
            }
            CoordinatorEvent::OperationSettled { id } => {
                self.emit(events, RuntimeEvent::SubagentOperationSettled { id });
            }
        }
    }

    fn accept_subagent_record(&mut self, record: &TaskRecord) -> bool {
        let id = record.id().clone();
        let revision = record.revision();
        if self
            .subagent_record_revisions
            .get(&id)
            .is_some_and(|latest| *latest >= revision)
        {
            return false;
        }
        self.subagent_record_revisions.insert(id, revision);
        true
    }

    fn publish_subagent_record(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        record: &TaskRecord,
    ) {
        if self.accept_subagent_record(record) {
            self.emit(
                events,
                RuntimeEvent::SubagentRecord {
                    record: record.clone(),
                },
            );
        }
    }

    fn publish_subagent_status(
        &mut self,
        progress: &mut ProgressPublisher<RuntimeEvent>,
        record: &TaskRecord,
    ) {
        let id = record.id().clone();
        let revision = record.revision();
        if self
            .subagent_status_revisions
            .get(&id)
            .is_some_and(|latest| *latest >= revision)
        {
            return;
        }
        self.subagent_status_revisions.insert(id.clone(), revision);
        self.publish_progress(
            progress,
            RuntimeEvent::SubagentStatus {
                id,
                revision,
                observation: record.observation().clone(),
            },
            false,
        );
    }

    async fn drain_coordinator_events(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        loop {
            let event = match self.subagent_events.as_mut() {
                Some(receiver) => match receiver.try_recv() {
                    Ok(event) => Some(event),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        self.subagent_events = None;
                        None
                    }
                },
                None => None,
            };
            let Some(event) = event else {
                return;
            };
            self.handle_coordinator_event(event, events, progress).await;
        }
    }

    async fn start_pending_compaction(&mut self) {
        if self.active.is_some() {
            return;
        }
        let Some(PendingCompaction {
            focus,
            cancellation,
        }) = self.pending_compaction.take()
        else {
            return;
        };
        let caller = self.subagents.root_caller().await;
        let agent = self
            .agent
            .take()
            .expect("an idle runtime controller owns its agent");
        let writer = self.session.clone();
        let cancellation_for_worker = cancellation.clone();
        let policy = self.compaction_policy;
        let selection = self.selection;
        let (sender, worker) = mpsc::unbounded_channel();
        let (_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let worker_task = tokio::spawn(async move {
            let mut agent = agent;
            let completion_sender = sender.clone();
            let result = agent
                .compact_manual(
                    policy,
                    selection,
                    focus,
                    &cancellation_for_worker,
                    &writer,
                    &mut || {
                        let _ = completion_sender.send(WorkerMessage::CompactionStarted);
                    },
                )
                .await;
            drop(writer);
            let _ = sender.send(WorkerMessage::CompactionFinished { agent, result });
        });
        drop(worker_task);
        self.active = Some(ActiveRun {
            operation: ForegroundOperation::Compaction,
            provider: selection.model().provider(),
            run_id: None,
            request_entry: None,
            correction_requested: false,
            persisted_model_steps: BTreeSet::new(),
            pending_busy_submissions: 0,
            caller,
            cancellation,
            worker,
            progress: worker_progress,
            progress_open: true,
            cancellation_requested: false,
        });
    }

    async fn start_run(
        &mut self,
        request: String,
        _events: &mpsc::UnboundedSender<RuntimeEvent>,
        _progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        let caller = self.subagents.root_caller().await;
        let mut agent = self
            .agent
            .take()
            .expect("an idle runtime controller owns its agent");
        agent.set_invocation_context(
            AgentInvocationContext::new(caller.clone(), self.subagents.clone())
                .with_accepted_thinking(self.accepted_thinking.clone()),
        );
        let cancellation = self.subagents.storage_failure_token().child_token();
        let cancellation_for_worker = cancellation.clone();
        let writer = self.session.clone();
        let selection = self.selection;
        let (sender, worker) = mpsc::unbounded_channel();
        let (progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let worker_task = tokio::spawn(async move {
            let mut agent = agent;
            let mut progress_sender = ProgressPublisher::new(progress_sender);
            let completion_sender = sender.clone();
            let mut settled_tool_updates = Vec::new();
            let result = {
                let mut event = |event| match event {
                    PersistedAgentEvent::RequestCommitted {
                        run_id,
                        request_entry,
                        request,
                    } => {
                        let _ = completion_sender.send(WorkerMessage::RequestCommitted {
                            run_id,
                            request_entry,
                            request,
                            selection,
                        });
                    }
                    PersistedAgentEvent::ModelStepStarted { run_id, step_id } => {
                        let _ = completion_sender
                            .send(WorkerMessage::PersistedModelStepStarted { run_id, step_id });
                    }
                    PersistedAgentEvent::ToolUpdated {
                        run_id,
                        step_id,
                        token,
                        call,
                    } if call.tool == ToolName::Run
                        && !matches!(call.summary, ToolCallSummary::Pending) =>
                    {
                        settled_tool_updates.push(PersistedToolUpdate {
                            run_id,
                            step_id,
                            token,
                            call,
                        });
                    }
                    PersistedAgentEvent::ExecutionCommitted {
                        run_id,
                        step_id,
                        source,
                        commentary,
                        reasoning_summary,
                        execution,
                        attempts,
                    } => {
                        flush_persisted_tool_updates(
                            &completion_sender,
                            &mut settled_tool_updates,
                            run_id,
                            Some(step_id),
                        );
                        let _ =
                            completion_sender.send(WorkerMessage::PersistedExecutionCompleted {
                                run_id,
                                step_id,
                                source,
                                commentary,
                                reasoning_summary,
                                execution,
                                attempts,
                            });
                    }
                    PersistedAgentEvent::ModelProgress {
                        run_id,
                        step_id,
                        progress,
                    } => {
                        let progress = AgentProgress::ModelProgress(progress);
                        let bytes = agent_progress_payload_bytes(&progress);
                        progress_sender.publish(
                            WorkerProgress {
                                run_id: RunId(run_id),
                                step_id: ModelStepId(step_id),
                                progress,
                            },
                            false,
                            bytes,
                        );
                    }
                    PersistedAgentEvent::ExecutionStarted {
                        run_id,
                        step_id,
                        commentary,
                    } => {
                        writer.mark_execution_started(step_id);
                        let progress = AgentProgress::ExecutionStarted {
                            commentary: commentary.clone(),
                        };
                        let bytes = agent_progress_payload_bytes(&progress);
                        progress_sender.publish(
                            WorkerProgress {
                                run_id: RunId(run_id),
                                step_id: ModelStepId(step_id),
                                progress,
                            },
                            false,
                            bytes,
                        );
                        let _ = completion_sender.send(WorkerMessage::ExecutionStarted {
                            run_id,
                            step_id,
                            commentary,
                        });
                    }
                    PersistedAgentEvent::ToolUpdated {
                        run_id,
                        step_id,
                        token,
                        call,
                    } => {
                        progress_sender.publish(
                            WorkerProgress {
                                run_id: RunId(run_id),
                                step_id: ModelStepId(step_id),
                                progress: AgentProgress::ToolUpdated { token, call },
                            },
                            false,
                            0,
                        );
                    }
                    PersistedAgentEvent::RunPreviewUpdated {
                        run_id,
                        step_id,
                        token,
                        preview,
                    } => {
                        let bytes = preview.command.len() + preview.stdout.len();
                        progress_sender.publish(
                            WorkerProgress {
                                run_id: RunId(run_id),
                                step_id: ModelStepId(step_id),
                                progress: AgentProgress::RunPreviewUpdated { token, preview },
                            },
                            false,
                            bytes,
                        );
                    }
                    PersistedAgentEvent::StdoutDelta {
                        run_id,
                        step_id,
                        text,
                    } => {
                        let bytes = text.len();
                        progress_sender.publish(
                            WorkerProgress {
                                run_id: RunId(run_id),
                                step_id: ModelStepId(step_id),
                                progress: AgentProgress::StdoutDelta { text },
                            },
                            false,
                            bytes,
                        );
                    }
                    PersistedAgentEvent::StdoutTruncated { run_id, step_id } => {
                        progress_sender.publish(
                            WorkerProgress {
                                run_id: RunId(run_id),
                                step_id: ModelStepId(step_id),
                                progress: AgentProgress::StdoutTruncated,
                            },
                            false,
                            0,
                        );
                    }
                };
                agent
                    .run_persisted(
                        selection,
                        request,
                        &cancellation_for_worker,
                        &writer,
                        &mut event,
                    )
                    .await
            };
            if let Ok(outcome) = &result {
                flush_persisted_tool_updates(
                    &completion_sender,
                    &mut settled_tool_updates,
                    outcome.run_id,
                    None,
                );
            }
            let progress_gap = progress_sender.take_pending_gap();
            drop(writer);
            let _ = sender.send(WorkerMessage::PersistedFinished {
                agent,
                result,
                progress_gap,
            });
        });
        drop(worker_task);
        self.active = Some(ActiveRun {
            operation: ForegroundOperation::Run,
            provider: selection.model().provider(),
            run_id: None,
            request_entry: None,
            correction_requested: false,
            persisted_model_steps: BTreeSet::new(),
            pending_busy_submissions: 0,
            caller,
            cancellation,
            worker,
            progress: worker_progress,
            progress_open: true,
            cancellation_requested: false,
        });
    }

    fn cancel_foreground(
        &mut self,
        correction: bool,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if self.pending_compaction.is_some()
            && self
                .active
                .as_ref()
                .is_some_and(|active| active.operation == ForegroundOperation::Run)
        {
            let pending = self
                .pending_compaction
                .take()
                .expect("pending compaction was checked");
            pending.cancellation.cancel();
            self.emit(
                events,
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: CompactionLifecycle::Settled(
                        CompactionSettlement::CancelledBeforeStart,
                    ),
                },
            );
            return;
        }
        self.request_active_cancellation(correction);
    }

    fn request_active_cancellation(&mut self, correction: bool) {
        if let Some(active) = &mut self.active {
            self.trace
                .emit(|| TraceEvent::ForegroundCancellationRequested);
            let first_cancellation = !active.cancellation_requested;
            active.cancellation_requested = true;
            active.cancellation.cancel();
            if first_cancellation && active.operation == ForegroundOperation::Run {
                active.correction_requested = correction;
            }
        }
    }

    async fn request_background_cancellation(&mut self) {
        self.trace
            .emit(|| TraceEvent::BackgroundCancellationRequested);
        self.subagents
            .request_background_cancellation(self.active.as_ref().map(|active| &active.caller))
            .await;
        self.trace
            .emit(|| TraceEvent::BackgroundCancellationReturned);
    }

    async fn request_shutdown(&mut self) {
        if let Some(pending) = self.pending_compaction.take() {
            pending.cancellation.cancel();
        }
        self.request_active_cancellation(false);
        self.request_background_cancellation().await;
    }

    async fn handle_worker_message(
        &mut self,
        message: WorkerMessage<M>,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        match message {
            WorkerMessage::RequestCommitted {
                run_id,
                request_entry,
                request,
                selection,
            } => {
                let run_id = RunId(run_id);
                let pending_busy_submissions = std::mem::take(
                    &mut self
                        .active
                        .as_mut()
                        .expect("request commit requires an active run")
                        .pending_busy_submissions,
                );
                let active = self
                    .active
                    .as_mut()
                    .expect("request commit requires an active run");
                active.run_id = Some(run_id);
                active.request_entry = Some(request_entry);
                self.trace
                    .emit(|| TraceEvent::RootRunStarted { run_id: run_id.0 });
                self.publish_progress(progress, RuntimeEvent::RunStarted { run_id }, false);
                self.append_committed_record(
                    events,
                    run_id,
                    Record::UserRequest {
                        request,
                        selection: Some(selection),
                    },
                    false,
                );
                for _ in 0..pending_busy_submissions {
                    self.emit(
                        events,
                        RuntimeEvent::SubmissionRejectedBusy {
                            active_run_id: Some(run_id),
                        },
                    );
                }
            }
            WorkerMessage::PersistedModelStepStarted { run_id, step_id } => {
                let run_id = RunId(run_id);
                let step_id = ModelStepId(step_id);
                let active = self
                    .active
                    .as_mut()
                    .expect("model step commit requires an active run");
                if active.run_id != Some(run_id) {
                    return;
                }
                active.persisted_model_steps.insert(step_id.0);
                self.publish_progress(
                    progress,
                    RuntimeEvent::ModelStepStarted { run_id, step_id },
                    false,
                );
            }
            WorkerMessage::ExecutionStarted {
                run_id,
                step_id,
                commentary,
            } => {
                self.emit(
                    events,
                    RuntimeEvent::ExecutionStarted {
                        run_id: RunId(run_id),
                        step_id: ModelStepId(step_id),
                        commentary,
                    },
                );
            }
            WorkerMessage::PersistedToolSettled {
                run_id,
                step_id,
                token,
                call,
            } => {
                self.emit(
                    events,
                    RuntimeEvent::ToolUpdated {
                        run_id: RunId(run_id),
                        step_id: ModelStepId(step_id),
                        token,
                        call,
                    },
                );
            }
            WorkerMessage::PersistedExecutionCompleted {
                run_id,
                step_id,
                source,
                commentary,
                reasoning_summary,
                execution,
                attempts,
            } => {
                self.append_committed_record(
                    events,
                    RunId(run_id),
                    Record::CompletedExecution {
                        step_id: ModelStepId(step_id),
                        source,
                        commentary,
                        reasoning_summary,
                        execution,
                        attempts,
                    },
                    false,
                );
                self.session.clear_execution(step_id);
            }
            WorkerMessage::CompactionStarted => {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.operation == ForegroundOperation::Compaction)
                {
                    self.emit(
                        events,
                        RuntimeEvent::CompactionLifecycle {
                            lifecycle: CompactionLifecycle::Started,
                        },
                    );
                }
            }
            WorkerMessage::CompactionFinished { agent, result } => {
                self.drain_worker_progress(progress);
                self.agent = Some(agent);
                match result {
                    Ok(ManualCompactionResult::NoOp) => self.emit(
                        events,
                        RuntimeEvent::CompactionLifecycle {
                            lifecycle: CompactionLifecycle::NoOp,
                        },
                    ),
                    Ok(ManualCompactionResult::CancelledBeforeStart) => self.emit(
                        events,
                        RuntimeEvent::CompactionLifecycle {
                            lifecycle: CompactionLifecycle::Settled(
                                CompactionSettlement::CancelledBeforeStart,
                            ),
                        },
                    ),
                    Ok(ManualCompactionResult::FailedBeforeStart) => self.emit(
                        events,
                        RuntimeEvent::CompactionLifecycle {
                            lifecycle: CompactionLifecycle::Settled(CompactionSettlement::Failed),
                        },
                    ),
                    Ok(ManualCompactionResult::Settled(committed)) => {
                        let settlement = match &committed.outcome {
                            CompactionOutcome::Applied { .. } => CompactionSettlement::Applied,
                            CompactionOutcome::Failed { .. } => CompactionSettlement::Failed,
                            CompactionOutcome::Cancelled { .. } => CompactionSettlement::Cancelled,
                        };
                        self.append_committed_record(
                            events,
                            RunId(committed.run_id),
                            Record::Compaction {
                                outcome: committed.outcome,
                            },
                            false,
                        );
                        self.emit(
                            events,
                            RuntimeEvent::CompactionLifecycle {
                                lifecycle: CompactionLifecycle::Settled(settlement),
                            },
                        );
                    }
                    Err(_) => {
                        self.subagents.latch_storage_failure().await;
                        self.active = None;
                        self.disable_session_storage(events).await;
                        self.session.clear_active_execution();
                        return;
                    }
                }
                self.active = None;
                self.session.clear_active_execution();
            }
            WorkerMessage::PersistedFinished {
                agent,
                result,
                progress_gap,
            } => {
                self.drain_worker_progress(progress);
                match result {
                    Ok(outcome) => {
                        let run_id = RunId(outcome.run_id);
                        let record = record_from_persisted_terminal(outcome.terminal);
                        let settlement = root_settlement(&record);
                        let (
                            provider,
                            correction_requested,
                            request_entry,
                            caller,
                            persisted_model_steps,
                        ) = {
                            let active = self
                                .active
                                .as_ref()
                                .expect("finished run remains active until settlement");
                            (
                                active.provider,
                                active.correction_requested,
                                active.request_entry,
                                active.caller.clone(),
                                active.persisted_model_steps.clone(),
                            )
                        };
                        let disabled_reason = outcome
                            .recovery_failure
                            .then_some(RuntimeDisabledReason::ArenaCheckpointRecoveryFailed)
                            .or_else(|| {
                                outcome
                                    .authentication_failure
                                    .then(|| RuntimeDisabledReason::authentication(provider))
                            });
                        let correction = correction_requested
                            && matches!(&record, Record::CancelledRun { .. })
                            && self.disabled_reason.is_none()
                            && disabled_reason.is_none()
                            && request_entry.is_some();
                        if disabled_reason.is_none() {
                            self.agent = Some(agent);
                        }
                        self.append_committed_record(
                            events,
                            run_id,
                            record,
                            progress_gap || progress.take_pending_gap(),
                        );
                        if let Some(reason) = disabled_reason {
                            self.disabled_reason = Some(reason.clone());
                            if let Some(pending) = self.pending_compaction.take() {
                                pending.cancellation.cancel();
                            }
                            self.emit(events, RuntimeEvent::RuntimeDisabled { reason });
                        }
                        self.subagents
                            .settle_root_run(&caller, persisted_model_steps)
                            .await;
                        if correction {
                            let (outcome, disabled) = if self.subagents.operation_count() != 0 {
                                (CorrectionOutcome::SkippedBackground, None)
                            } else {
                                self.drain_coordinator_events(events, progress).await;
                                let (outcome, disabled) = self
                                    .restore_selected_entry(
                                        request_entry.expect("eligible correction has a request"),
                                    )
                                    .await;
                                (
                                    match outcome {
                                        NavigationOutcome::Replaced {
                                            state,
                                            request_prefill,
                                        } => CorrectionOutcome::Replaced {
                                            state,
                                            request_prefill,
                                        },
                                        NavigationOutcome::Unchanged { .. }
                                        | NavigationOutcome::RejectedBusy
                                        | NavigationOutcome::RejectedReadOnly
                                        | NavigationOutcome::Failed => CorrectionOutcome::Failed,
                                    },
                                    disabled,
                                )
                            };
                            self.emit(
                                events,
                                RuntimeEvent::CorrectionCompleted { run_id, outcome },
                            );
                            self.disable_after_restoration(events, disabled).await;
                        }
                        self.trace.emit(|| TraceEvent::RootRunSettled {
                            run_id: run_id.0,
                            outcome: settlement,
                        });
                        self.emit(events, RuntimeEvent::RunSettled { run_id });
                        self.active = None;
                        if self.disabled_reason.is_none() {
                            self.start_pending_compaction().await;
                        }
                    }
                    Err(_error) => {
                        self.agent = Some(agent);
                        self.subagents.latch_storage_failure().await;
                        self.disable_session_storage(events).await;
                        self.active = None;
                    }
                }
                self.session.clear_active_execution();
            }
        }
    }

    async fn disable_session_storage(&mut self, events: &mpsc::UnboundedSender<RuntimeEvent>) {
        if self.disabled_reason == Some(RuntimeDisabledReason::SessionStorageWriteFailed) {
            return;
        }
        self.disabled_reason = Some(RuntimeDisabledReason::SessionStorageWriteFailed);
        if let Some(pending) = self.pending_compaction.take() {
            pending.cancellation.cancel();
        }
        self.request_active_cancellation(false);
        self.request_background_cancellation().await;
        self.emit(
            events,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
        );
        self.reject_pending_submissions(events);
    }

    fn reject_pending_submissions(&mut self, events: &mpsc::UnboundedSender<RuntimeEvent>) {
        let pending_busy_submissions = self.active.as_mut().map_or(0, |active| {
            std::mem::take(&mut active.pending_busy_submissions)
        });
        for _ in 0..pending_busy_submissions {
            self.emit(
                events,
                RuntimeEvent::SubmissionRejectedDisabled {
                    reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                },
            );
        }
    }

    async fn handle_worker_closed(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        let _ = progress.take_pending_gap();
        self.subagents.latch_storage_failure().await;
        self.disable_session_storage(events).await;
        self.active = None;
        self.session.clear_active_execution();
    }

    fn drain_worker_progress(&mut self, progress: &mut ProgressPublisher<RuntimeEvent>) {
        loop {
            let received = self
                .active
                .as_mut()
                .expect("worker finish requires an active run")
                .progress
                .try_recv();
            match received {
                Ok(progress_envelope) => self.handle_worker_progress(progress_envelope, progress),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    fn handle_worker_progress(
        &mut self,
        progress_envelope: ProgressEnvelope<WorkerProgress>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        let WorkerProgress {
            run_id,
            step_id,
            progress: agent_progress,
        } = progress_envelope.item;
        let event = match agent_progress {
            AgentProgress::ModelStepStarted => return,
            AgentProgress::ModelProgress(progress) => RuntimeEvent::ModelProgress {
                run_id,
                step_id,
                progress,
            },
            AgentProgress::ExecutionStarted { commentary, .. } => RuntimeEvent::ExecutionStarted {
                run_id,
                step_id,
                commentary,
            },
            AgentProgress::ToolUpdated { token, call } => RuntimeEvent::ToolUpdated {
                run_id,
                step_id,
                token,
                call,
            },
            AgentProgress::RunPreviewUpdated { token, preview } => {
                RuntimeEvent::RunPreviewUpdated {
                    run_id,
                    step_id,
                    token,
                    preview,
                }
            }
            AgentProgress::StdoutDelta { text } => RuntimeEvent::StdoutDelta {
                run_id,
                step_id,
                text,
            },
            AgentProgress::StdoutTruncated => RuntimeEvent::StdoutTruncated { run_id, step_id },
            AgentProgress::ExecutionCompleted { .. } => return,
        };
        self.publish_progress(progress, event, progress_envelope.gap_before);
    }

    fn publish_progress(
        &self,
        progress: &mut ProgressPublisher<RuntimeEvent>,
        event: RuntimeEvent,
        upstream_gap: bool,
    ) {
        let payload_bytes = runtime_event_payload_bytes(&event);
        progress.publish(event, upstream_gap, payload_bytes);
    }

    fn append_committed_record(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        run_id: RunId,
        record: Record,
        progress_gap: bool,
    ) {
        let record = StoredRecord { run_id, record };
        self.records.push(record.clone());
        self.emit(
            events,
            RuntimeEvent::RecordAppended {
                record,
                progress_gap,
            },
        );
    }

    fn emit(&self, events: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) {
        let _ = events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::io::{Read, Write};
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use rusqlite::{Connection, OpenFlags, params};
    #[cfg(unix)]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[cfg(unix)]
    use tokio::net::UnixListener;
    use tokio::sync::{Notify, mpsc, oneshot};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::{
        AcceptedThinking, ActiveRun, CoordinatorEvent, CorrectionOutcome, ForegroundOperation,
        ModelStepId, PROGRESS_CHANNEL_CAPACITY, ProgressEnvelope, ProgressPublisher, Record,
        RestoredTimelineItem, RunId, RuntimeCommand, RuntimeController, RuntimeDisabledReason,
        RuntimeEvent, RuntimeProgressReceiver, StoredRecord, TerminalDetails, WorkerMessage,
        restored_initial_state, restored_stored_record, runtime_event_payload_bytes,
    };
    use crate::agent::{Agent, ArenaRestoreTestGate, ExecutionFailure, TerminalEvidence};
    use crate::anthropic::AnthropicModel;
    use crate::arena::{
        Arena, Checkpoint, Execution, ExecutionOutcome, RecoveryTestGate, RuntimeIdentity,
        ToolCall, ToolCallSummary, ToolName,
    };
    use crate::config::{
        DEFAULT_MODEL, ModelSelection, SessionPreferences,
        models::{LUNA, OPUS_5_5, Provider, SOL, TERRA},
    };
    use crate::model::{
        AttemptAccounting, CodeResult, Model, ModelAction as CompletedModelAction, ModelCallId,
        ModelContextEntry, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ModelUsage, OpaqueReplayItem, ProviderFailure, ProviderReplay,
        ScriptedAction as ModelAction, ScriptedModel, SummaryRequest, SummaryResult,
    };
    use crate::session::{
        CompactionOutcome, CompactionReason, PersistedCode, PersistedTerminal, RestoredRecord,
        RestoredRecordKind, RestoredTask, Session, SessionEntryId, SessionRestore,
        SessionWriteGate, SessionWriteKind, SessionWriter, TaskAdmission, TaskTerminal,
    };
    use crate::sse_fixture::{Fixture, FixtureReply};
    use crate::subagents::{
        ChildFailure, ChildModelFactory, ChildParameters, ChildPayload, ChildResult, ChildRunner,
        CompleteChildRunner, RunnerFuture, SessionSubagents, StartSubagentTestGate,
        SubagentCoordinator, SubagentPolicy, TaskAttempt, TaskExecution, TaskOutcome,
        TaskPersistedDetails, TaskRecord, TaskState, TerminalSettlementTestGate,
    };
    use crate::telemetry::TraceOwner;
    use crate::tools::{ReadTestGate, ToolHost, ToolValue};
    use crate::transcript::Transcript;
    use tokio_util::sync::CancellationToken;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    struct CancellationOnlyChildRunner;

    impl ChildRunner<ChildPayload> for CancellationOnlyChildRunner {
        fn run(
            &self,
            _child: crate::subagents::ChildRun<ChildPayload>,
            cancellation: CancellationToken,
        ) -> RunnerFuture {
            Box::pin(async move {
                cancellation.cancelled().await;
                Err(ChildFailure::new("child cancelled"))
            })
        }
    }

    struct CountingChildRunner(Arc<AtomicUsize>);

    impl ChildRunner<ChildPayload> for CountingChildRunner {
        fn run(
            &self,
            _child: crate::subagents::ChildRun<ChildPayload>,
            _cancellation: CancellationToken,
        ) -> RunnerFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(ChildResult::new(
                    "saved child".to_owned(),
                    BTreeMap::from([("answer".to_owned(), ToolValue::Integer(7))]),
                ))
            })
        }
    }

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the configured worker")
    }

    async fn controller_with_model<M: Model>(
        workspace: &tempfile::TempDir,
        model: M,
    ) -> RuntimeController<M> {
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        RuntimeController::for_test(Agent::new(arena, model))
    }

    async fn controller(
        workspace: &tempfile::TempDir,
        actions: Vec<ModelAction>,
    ) -> RuntimeController<ScriptedModel> {
        controller_with_model(workspace, ScriptedModel::new(actions)).await
    }

    async fn default_controller(workspace: &tempfile::TempDir) -> RuntimeController<ScriptedModel> {
        controller_with_model(workspace, ScriptedModel::default()).await
    }

    async fn seed_compactable_history(writer: &SessionWriter) {
        for label in ["discarded", "retained"] {
            let admission = writer
                .commit_request(
                    format!("{label} request"),
                    ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
                )
                .await
                .expect("commit compaction fixture request");
            let step_id = writer
                .start_model_step(admission.run_id)
                .await
                .expect("start compaction fixture step");
            writer
                .close_run(
                    admission.run_id,
                    Some(step_id),
                    move || PersistedTerminal::Final {
                        step_id,
                        text: format!("{label} reply"),
                        reasoning_summary: String::new(),
                        replay: ProviderReplay::new(
                            "fixture",
                            vec![OpaqueReplayItem::new(vec![b'x'; 100_000])],
                        ),
                        attempts: Vec::new(),
                    },
                    None,
                )
                .await
                .expect("close compaction fixture run");
        }
    }

    async fn controller_with_subagents<M: Model>(
        workspace: &tempfile::TempDir,
        model: M,
        subagents: SessionSubagents,
    ) -> RuntimeController<M> {
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        RuntimeController::new_for_test(
            Agent::new(arena, model),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            subagents,
        )
    }

    async fn child_payload(workspace: &tempfile::TempDir) -> ChildPayload {
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create child source arena");
        arena
            .execute(
                "result_values = {'parent': 'kept'}",
                &CancellationToken::new(),
            )
            .await
            .expect("initialise parent exports");
        ChildPayload::new(arena.child_source(), arena.checkpoint())
    }

    fn child_parameters() -> ChildParameters {
        ChildParameters::new(
            "background work".to_owned(),
            ModelSelection::new(
                crate::config::DEFAULT_MODEL,
                crate::config::DEFAULT_MODEL.default_thinking,
            )
            .unwrap(),
            false,
        )
    }

    fn known_usage(
        model: &'static crate::config::ModelConfiguration,
        input_tokens: u64,
        output_tokens: u64,
        cached_input_tokens: u64,
    ) -> AttemptAccounting {
        AttemptAccounting::KnownUsage(ModelUsage {
            model: model.id.to_owned(),
            service_tier: Some(model.service_tier.to_owned()),
            input_tokens: Some(input_tokens),
            cached_input_tokens: Some(cached_input_tokens),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(output_tokens),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(input_tokens + output_tokens),
        })
    }

    #[derive(Clone)]
    struct GatedChildFactory {
        started: Arc<Notify>,
        release: Arc<Notify>,
        attempts: Vec<AttemptAccounting>,
    }

    impl ChildModelFactory for GatedChildFactory {
        type Model = GatedChildModel;

        fn create(&self, _prompt: String) -> Self::Model {
            GatedChildModel {
                started: self.started.clone(),
                release: self.release.clone(),
                attempts: self.attempts.clone(),
            }
        }
    }

    struct GatedChildModel {
        started: Arc<Notify>,
        release: Arc<Notify>,
        attempts: Vec<AttemptAccounting>,
    }

    #[derive(Clone)]
    struct TaskStepFailureChildFactory {
        calls: Arc<AtomicUsize>,
    }

    struct TaskStepFailureChildModel {
        calls: Arc<AtomicUsize>,
    }

    impl ChildModelFactory for TaskStepFailureChildFactory {
        type Model = TaskStepFailureChildModel;

        fn create(&self, _prompt: String) -> Self::Model {
            TaskStepFailureChildModel {
                calls: self.calls.clone(),
            }
        }
    }

    impl Model for TaskStepFailureChildModel {
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
                let action = match self.calls.fetch_add(1, Ordering::SeqCst) {
                    0 => CompletedModelAction::Code {
                        source: "child_value = 1".to_owned(),
                        call_id: ModelCallId::new("child-task-step"),
                        commentary: None,
                    },
                    1 => CompletedModelAction::Message {
                        text: "child complete".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    impl Model for GatedChildModel {
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
                self.started.notify_one();
                tokio::select! {
                    _ = cancellation.cancelled() => Err(ModelFailure::new(
                        ModelError::Cancelled,
                        vec![AttemptAccounting::UsageUnavailable],
                    )),
                    _ = self.release.notified() => Ok(ModelStep {
                        action: CompletedModelAction::Message {
                            text: "child complete".to_owned(),
                        },
                        reasoning_summary: String::new(),
                        replay: ProviderReplay::new("test", Vec::new()),
                        attempts: self.attempts.clone(),
                    }),
                }
            }
        }
    }

    struct MessageWaitingModel {
        model: ScriptedModel,
        message_waiting: Arc<Notify>,
    }

    impl Model for MessageWaitingModel {
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
                let step = self
                    .model
                    .step(selection, history, cancellation, progress)
                    .await?;
                if matches!(&step.action, CompletedModelAction::Message { .. }) {
                    self.message_waiting.notify_one();
                    cancellation.cancelled().await;
                    return Err(ModelError::Cancelled.into());
                }
                Ok(step)
            }
        }
    }

    struct UsageMessageModel {
        attempts: Option<Vec<AttemptAccounting>>,
    }

    impl Model for UsageMessageModel {
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
                let attempts = self.attempts.take().ok_or(ModelError::Exhausted)?;
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "foreground complete".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts,
                })
            }
        }
    }

    #[derive(Clone)]
    struct CleanupGatedChildFactory {
        started: Arc<Notify>,
        cleanup_started: Arc<Notify>,
        release: Arc<Notify>,
        returned: Arc<Notify>,
    }

    impl ChildModelFactory for CleanupGatedChildFactory {
        type Model = CleanupGatedChildModel;

        fn create(&self, _prompt: String) -> Self::Model {
            CleanupGatedChildModel {
                started: self.started.clone(),
                cleanup_started: self.cleanup_started.clone(),
                release: self.release.clone(),
                returned: self.returned.clone(),
            }
        }
    }

    struct CleanupGatedChildModel {
        started: Arc<Notify>,
        cleanup_started: Arc<Notify>,
        release: Arc<Notify>,
        returned: Arc<Notify>,
    }

    impl Model for CleanupGatedChildModel {
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
                self.started.notify_one();
                cancellation.cancelled().await;
                self.cleanup_started.notify_one();
                self.release.notified().await;
                self.returned.notify_one();
                Err(ModelFailure::new(
                    ModelError::Cancelled,
                    vec![AttemptAccounting::UsageUnavailable],
                ))
            }
        }
    }

    fn complete_child_coordinator<F: ChildModelFactory + Clone>(factory: F) -> SessionSubagents {
        SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(CompleteChildRunner::new(
                "test child prompt".to_owned(),
                factory,
            )),
        )
    }

    #[derive(Clone)]
    struct WorkflowChildFactory {
        sol_models: Arc<AtomicUsize>,
        selections: Arc<Mutex<Vec<ModelSelection>>>,
        grandchild_started: Arc<Notify>,
    }

    impl WorkflowChildFactory {
        fn new() -> Self {
            Self {
                sol_models: Arc::new(AtomicUsize::new(0)),
                selections: Arc::new(Mutex::new(Vec::new())),
                grandchild_started: Arc::new(Notify::new()),
            }
        }
    }

    enum WorkflowChildModel {
        Unselected(WorkflowChildFactory),
        Scripted {
            model: ScriptedModel,
            attempt: AttemptAccounting,
        },
        Failed {
            attempt: AttemptAccounting,
        },
        Blocked {
            attempt: AttemptAccounting,
            started: Arc<Notify>,
        },
    }

    impl Model for WorkflowChildModel {
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
                if let Self::Unselected(factory) = self {
                    *self = factory.model_for(selection);
                }
                match self {
                    Self::Unselected(_) => unreachable!("workflow child resolved its model"),
                    Self::Scripted { model, attempt } => {
                        let mut step = model
                            .step(selection, history, cancellation, progress)
                            .await?;
                        step.attempts = vec![attempt.clone()];
                        Ok(step)
                    }
                    Self::Failed { attempt } => {
                        progress(ModelProgress::AttemptStarted { attempt: 1 });
                        Err(ModelFailure::new(
                            ModelError::InvalidResponse("controlled failure"),
                            vec![attempt.clone()],
                        ))
                    }
                    Self::Blocked { attempt, started } => {
                        progress(ModelProgress::AttemptStarted { attempt: 1 });
                        started.notify_one();
                        cancellation.cancelled().await;
                        Err(ModelFailure::new(
                            ModelError::Cancelled,
                            vec![attempt.clone()],
                        ))
                    }
                }
            }
        }
    }

    impl ChildModelFactory for WorkflowChildFactory {
        type Model = WorkflowChildModel;

        fn create(&self, _prompt: String) -> Self::Model {
            WorkflowChildModel::Unselected(self.clone())
        }
    }

    impl WorkflowChildFactory {
        fn model_for(&self, selection: ModelSelection) -> WorkflowChildModel {
            self.selections.lock().unwrap().push(selection);
            let attempt = known_usage(selection.model(), 10, 1, 0);
            match selection.model().id {
                id if id == TERRA.id => WorkflowChildModel::Scripted {
                    model: ScriptedModel::new(vec![
                        ModelAction::Code {
                            source: "result_values['handoff'] = handoff\ntry:\n    result_values['same_execution'] = same_execution\nexcept NameError:\n    result_values['same_execution'] = 'missing'".to_owned(),
                        },
                        ModelAction::Message {
                            text: "recoverable child completed".to_owned(),
                        },
                    ]),
                    attempt,
                },
                id if id == LUNA.id => WorkflowChildModel::Failed { attempt },
                id if id == SOL.id
                    && self.sol_models.fetch_add(1, Ordering::SeqCst) == 0 =>
                {
                    WorkflowChildModel::Scripted {
                        model: ScriptedModel::new(vec![
                            ModelAction::Code {
                                source: "descendant = await start_subagent('read-only descendant', None, True)\nresult_values['descendant_id'] = descendant.id".to_owned(),
                            },
                            ModelAction::Message {
                                text: "cancellable parent completed".to_owned(),
                            },
                        ]),
                        attempt,
                    }
                }
                id if id == SOL.id => WorkflowChildModel::Blocked {
                    attempt,
                    started: self.grandchild_started.clone(),
                },
                _ => panic!("unexpected child selection {selection:?}"),
            }
        }
    }

    struct WorkflowRootModel {
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        selections: Arc<Mutex<Vec<ModelSelection>>>,
        request: usize,
        step: usize,
    }

    impl Model for WorkflowRootModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.contexts
                    .lock()
                    .unwrap()
                    .push(history.entries().to_vec());
                let request = history
                    .entries()
                    .iter()
                    .filter(|entry| matches!(entry, ModelContextEntry::UserRequest { .. }))
                    .count();
                if request != self.request {
                    self.request = request;
                    self.step = 0;
                }
                self.selections.lock().unwrap().push(selection);
                let action = match (request, self.step) {
                    (1, 0) => CompletedModelAction::Code {
                        source: "handoff = 'before'".to_owned(),
                        call_id: ModelCallId::new("workflow-1-1"),
                        commentary: None,
                    },
                    (1, 1) => CompletedModelAction::Code {
                        source: "same_execution = 'after'\nrecoverable_id = (await start_subagent('recoverable work', 'openai/gpt-5.6-terra')).id\nfailed_id = (await start_subagent('failing work', 'openai/gpt-5.6-luna')).id\nparent_id = (await start_subagent('cancellable parent')).id".to_owned(),
                        call_id: ModelCallId::new("workflow-1-2"),
                        commentary: None,
                    },
                    (1, 2) => CompletedModelAction::Message {
                        text: "first foreground final".to_owned(),
                    },
                    (2, 0) => CompletedModelAction::Code {
                        source: "recoverable = await get_subagent(recoverable_id)\nawait recoverable.refresh()\nrecoverable_result = await recoverable.wait()\nassert recoverable.status.state == 'completed'\nassert recoverable_result['values']['handoff'] == handoff\nassert set(recoverable_result['values']) == {'handoff', 'same_execution'}\nfailed = await get_subagent(failed_id)\ntry:\n    await failed.wait()\nexcept RuntimeError:\n    pass\nassert failed.status.state == 'failed'\nparent = await get_subagent(parent_id)\nparent_result = await parent.wait()\ndescendant = await get_subagent(parent_result['values']['descendant_id'])\nawait descendant.refresh()\nassert descendant.status.state in ('starting', 'running')\nassert (await parent.cancel()).state == 'completed'\nassert (await descendant.refresh()).state == 'cancelled'\nrecoverable_result['values']['same_execution']".to_owned(),
                        call_id: ModelCallId::new("workflow-2-1"),
                        commentary: None,
                    },
                    (2, 1) => CompletedModelAction::Message {
                        text: "second foreground final".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![known_usage(selection.model(), 100, 10, 20)],
                })
            }
        }
    }

    fn start_controller<M: Model>(
        controller: RuntimeController<M>,
    ) -> (
        mpsc::Sender<RuntimeCommand>,
        mpsc::UnboundedReceiver<RuntimeEvent>,
        RuntimeProgressReceiver,
        JoinHandle<Vec<StoredRecord>>,
    ) {
        let (command_sender, command_receiver) = mpsc::channel(8);
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let (progress_sender, progress_receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let controller =
            tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));
        (
            command_sender,
            event_receiver,
            progress_receiver,
            controller,
        )
    }

    struct AdmissionGatedModel {
        calls: Arc<AtomicUsize>,
        provider_called: Arc<Notify>,
    }

    impl Model for AdmissionGatedModel {
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
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.provider_called.notify_one();
                cancellation.cancelled().await;
                Err(ModelError::Cancelled.into())
            }
        }
    }

    struct RetainedCompactionModel {
        run_started: Arc<Notify>,
        summary_requests: Arc<Mutex<Vec<SummaryRequest>>>,
    }

    impl Model for RetainedCompactionModel {
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
                self.run_started.notify_one();
                cancellation.cancelled().await;
                Err(ModelError::Cancelled.into())
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            request: SummaryRequest,
            cancellation: &'a CancellationToken,
            attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a
        {
            async move {
                if cancellation.is_cancelled() {
                    return Err(ModelError::Cancelled.into());
                }
                attempt_started();
                self.summary_requests
                    .lock()
                    .expect("record summary request")
                    .push(request);
                Ok(SummaryResult {
                    text: "summary".to_owned(),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct GatedSummaryModel {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl Model for GatedSummaryModel {
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

        #[allow(clippy::manual_async_fn)]
        fn summarise<'a>(
            &'a mut self,
            _request: SummaryRequest,
            cancellation: &'a CancellationToken,
            attempt_started: &'a mut (dyn FnMut() + Send),
        ) -> impl std::future::Future<Output = Result<SummaryResult, ModelFailure>> + Send + 'a
        {
            async move {
                if cancellation.is_cancelled() {
                    return Err(ModelError::Cancelled.into());
                }
                attempt_started();
                self.started.notify_one();
                tokio::select! {
                    _ = cancellation.cancelled() => Err(ModelFailure::new(
                        ModelError::Cancelled,
                        vec![AttemptAccounting::UsageUnavailable],
                    )),
                    _ = self.release.notified() => Ok(SummaryResult {
                        text: "summary".to_owned(),
                        attempts: vec![AttemptAccounting::UsageUnavailable],
                    }),
                }
            }
        }
    }

    struct SettlementGatedModel {
        calls: Arc<AtomicUsize>,
        provider_called: Arc<Notify>,
        source: String,
    }

    impl Model for SettlementGatedModel {
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
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                self.provider_called.notify_one();
                Ok(ModelStep {
                    action: if call == 0 {
                        CompletedModelAction::Code {
                            source: self.source.clone(),
                            call_id: ModelCallId::new("first"),
                            commentary: None,
                        }
                    } else {
                        CompletedModelAction::Message {
                            text: "done".to_owned(),
                        }
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct CountingMessageModel {
        calls: Arc<AtomicUsize>,
    }

    impl Model for CountingMessageModel {
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
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    reasoning_summary: "final summary".to_owned(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                })
            }
        }
    }

    struct ContextCapturingMessageModel {
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
    }

    impl Model for ContextCapturingMessageModel {
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
                self.contexts
                    .lock()
                    .expect("contexts lock")
                    .push(history.entries().to_vec());
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "next response".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn manual_compaction_persists_before_reporting_completion() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Compaction);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        for request in [
            "first ".to_owned() + &"x".repeat(100_000),
            "second ".to_owned() + &"y".repeat(100_000),
        ] {
            let admission = writer
                .commit_request(request, selection)
                .await
                .expect("commit request");
            writer
                .close_run(
                    admission.run_id,
                    None,
                    || PersistedTerminal::Cancelled {
                        evidence: None,
                        attempts: Vec::new(),
                    },
                    None,
                )
                .await
                .expect("close seeded run");
        }
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(Vec::new()).with_summary_responses(vec!["summary".to_owned()]),
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, mut progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Compact {
                focus: String::new(),
            })
            .await
            .expect("request compaction");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: super::CompactionAcknowledgement::Accepted,
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::CompactionLifecycle {
                lifecycle: super::CompactionLifecycle::Started,
            }
        );
        gate.wait_until_started().await;
        commands
            .send(RuntimeCommand::Submit {
                request: "busy request".to_owned(),
            })
            .await
            .expect("submit while compacting");
        commands
            .send(RuntimeCommand::Compact {
                focus: "duplicate".to_owned(),
            })
            .await
            .expect("duplicate compaction");
        commands
            .send(RuntimeCommand::RequestTree { revision: 7 })
            .await
            .expect("request tree while compacting");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: None,
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: super::CompactionAcknowledgement::Busy,
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::TreeCatalogue {
                revision: 7,
                outcome: super::TreeRequestOutcome::RejectedBusy,
            }
        );
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel after compaction linearisation");
        assert!(events.try_recv().is_err());
        assert!(progress.try_recv().is_err());
        gate.release();

        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::Compaction { .. },
                    ..
                },
                ..
            }
        ));
        let count: i64 = session
            .connection_for_test()
            .expect("open session")
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = 'compaction'",
                [],
                |row| row.get(0),
            )
            .expect("count compactions");
        assert_eq!(count, 1);
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::CompactionLifecycle {
                lifecycle: super::CompactionLifecycle::Settled(
                    super::CompactionSettlement::Applied,
                ),
            }
        );

        shutdown_controller(commands, controller).await;
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn empty_history_compaction_reports_noop_without_a_card() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let controller = default_controller(&workspace).await;
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Compact {
                focus: String::new(),
            })
            .await
            .expect("request compaction");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: super::CompactionAcknowledgement::Accepted,
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::CompactionLifecycle {
                lifecycle: super::CompactionLifecycle::NoOp,
            }
        );
        assert!(events.try_recv().is_err());

        shutdown_controller(commands, controller).await;
    }

    #[tokio::test]
    async fn unsupported_manual_compaction_settles_failed_without_an_entry() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(arena, ScriptedModel::new(Vec::new())),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Compact {
                focus: String::new(),
            })
            .await
            .expect("request unsupported compaction");
        let events = events_until_compaction_terminal(&mut events).await;

        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                },
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Settled(
                        super::CompactionSettlement::Failed,
                    ),
                },
            ]
        ));
        assert_eq!(
            session
                .connection_for_test()
                .expect("open session")
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE kind = 'compaction'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count compactions"),
            0
        );

        shutdown_controller(commands, controller).await;
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn compaction_lifecycle_remains_fifo_when_progress_is_saturated() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(Vec::new()).with_summary_responses(vec![
                    "history summary".to_owned(),
                    "split summary".to_owned(),
                ]),
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (command_sender, command_receiver) = mpsc::channel(8);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, mut progress) = mpsc::channel(1);
        progress_sender
            .try_send(ProgressEnvelope {
                item: RuntimeEvent::ModelProgress {
                    run_id: RunId(99),
                    step_id: ModelStepId(99),
                    progress: ModelProgress::AttemptStarted { attempt: 1 },
                },
                gap_before: false,
            })
            .expect("saturate progress channel");
        let controller =
            tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        command_sender
            .send(RuntimeCommand::Compact {
                focus: String::new(),
            })
            .await
            .expect("request compaction");
        let events = events_until_compaction_terminal(&mut events).await;

        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                },
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Started,
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::Compaction { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Settled(
                        super::CompactionSettlement::Applied,
                    ),
                },
            ]
        ));
        assert!(progress.try_recv().is_ok(), "progress remained saturated");

        shutdown_controller(command_sender, controller).await;
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn retained_manual_compaction_starts_after_run_settlement() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        for request in [
            "first ".to_owned() + &"x".repeat(100_000),
            "second ".to_owned() + &"y".repeat(100_000),
        ] {
            let admission = writer
                .commit_request(request, selection)
                .await
                .expect("commit request");
            writer
                .close_run(
                    admission.run_id,
                    None,
                    || PersistedTerminal::Cancelled {
                        evidence: None,
                        attempts: Vec::new(),
                    },
                    None,
                )
                .await
                .expect("close seeded run");
        }
        let run_started = Arc::new(Notify::new());
        let summary_requests = Arc::new(Mutex::new(Vec::new()));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                RetainedCompactionModel {
                    run_started: run_started.clone(),
                    summary_requests: summary_requests.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "current ".to_owned() + &"z".repeat(100_000),
            })
            .await
            .expect("start run");
        run_started.notified().await;
        commands
            .send(RuntimeCommand::Compact {
                focus: "preserve decisions".to_owned(),
            })
            .await
            .expect("retain compaction");

        let mut run_settled = None;
        let mut compaction_started = None;
        let mut card = None;
        let mut compaction_settled = None;
        for index in 0..16 {
            match next_event(&mut events).await {
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                } => {}
                RuntimeEvent::RunSettled { .. } => run_settled = Some(index),
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Started,
                } => compaction_started = Some(index),
                RuntimeEvent::RecordAppended {
                    record:
                        StoredRecord {
                            record: Record::Compaction { .. },
                            ..
                        },
                    ..
                } => card = Some(index),
                RuntimeEvent::CompactionLifecycle {
                    lifecycle:
                        super::CompactionLifecycle::Settled(super::CompactionSettlement::Applied),
                } => {
                    compaction_settled = Some(index);
                    break;
                }
                _ => {}
            }
        }

        assert!(
            run_settled.expect("run settled") < compaction_started.expect("compaction started")
        );
        assert!(compaction_started.expect("compaction started") < card.expect("compaction card"));
        assert!(card.expect("compaction card") < compaction_settled.expect("compaction settled"));
        {
            let summary_requests = summary_requests.lock().expect("read summary requests");
            assert!(!summary_requests.is_empty());
            assert!(
                summary_requests
                    .iter()
                    .all(|request| request.input.contains("preserve decisions"))
            );
        }

        shutdown_controller(commands, controller).await;
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancelling_retained_compaction_before_start_leaves_no_card_or_entry() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        for request in [
            "first ".to_owned() + &"x".repeat(100_000),
            "second ".to_owned() + &"y".repeat(100_000),
        ] {
            let admission = writer
                .commit_request(request, selection)
                .await
                .expect("commit request");
            writer
                .close_run(
                    admission.run_id,
                    None,
                    || PersistedTerminal::Cancelled {
                        evidence: None,
                        attempts: Vec::new(),
                    },
                    None,
                )
                .await
                .expect("close seeded run");
        }
        let run_started = Arc::new(Notify::new());
        let summary_requests = Arc::new(Mutex::new(Vec::new()));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                RetainedCompactionModel {
                    run_started: run_started.clone(),
                    summary_requests: summary_requests.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "current ".to_owned() + &"z".repeat(100_000),
            })
            .await
            .expect("start run");
        run_started.notified().await;
        commands
            .send(RuntimeCommand::Compact {
                focus: "preserve decisions".to_owned(),
            })
            .await
            .expect("retain compaction");
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel retained compaction");

        let mut acknowledged = 0;
        let mut cancelled_before_start = 0;
        let mut run_settled = false;
        while !run_settled {
            match next_event(&mut events).await {
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                } => acknowledged += 1,
                RuntimeEvent::CompactionLifecycle {
                    lifecycle:
                        super::CompactionLifecycle::Settled(
                            super::CompactionSettlement::CancelledBeforeStart,
                        ),
                } => cancelled_before_start += 1,
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Started,
                }
                | RuntimeEvent::RecordAppended {
                    record:
                        StoredRecord {
                            record: Record::Compaction { .. },
                            ..
                        },
                    ..
                } => panic!("cancelled retained compaction must not start or append"),
                RuntimeEvent::RunSettled { .. } => run_settled = true,
                _ => {}
            }
        }

        assert_eq!(acknowledged, 1);
        assert_eq!(cancelled_before_start, 1);
        assert!(
            summary_requests
                .lock()
                .expect("read summary requests")
                .is_empty()
        );
        let compactions: i64 = session
            .connection_for_test()
            .expect("open session")
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE kind = 'compaction'",
                [],
                |row| row.get(0),
            )
            .expect("count compactions");
        assert_eq!(compactions, 0);

        shutdown_controller(commands, controller).await;
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn retained_compaction_uses_terminal_leaf_when_final_or_failed_wins_cancellation() {
        for terminal_kind in ["final", "failed"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let arena = Arena::new(
                worker_path(),
                ToolHost::new(workspace.path())
                    .await
                    .expect("open tool host"),
            )
            .await
            .expect("create arena");
            let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                .expect("select model");
            let session =
                Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
                    .expect("create session");
            let writer = session.writer();
            seed_compactable_history(&writer).await;
            let admission = writer
                .commit_request("race winner".to_owned(), selection)
                .await
                .expect("commit race request");
            let terminal = if terminal_kind == "final" {
                let step_id = writer
                    .start_model_step(admission.run_id)
                    .await
                    .expect("start final step");
                writer
                    .close_run(
                        admission.run_id,
                        Some(step_id),
                        move || PersistedTerminal::Final {
                            step_id,
                            text: "final wins".to_owned(),
                            reasoning_summary: String::new(),
                            replay: ProviderReplay::new("fixture", Vec::new()),
                            attempts: Vec::new(),
                        },
                        None,
                    )
                    .await
                    .expect("close final race winner")
            } else {
                writer
                    .close_run(
                        admission.run_id,
                        None,
                        || PersistedTerminal::Failed {
                            message: "failed wins".to_owned(),
                            evidence: None,
                            attempts: Vec::new(),
                        },
                        None,
                    )
                    .await
                    .expect("close failed race winner")
            };
            let terminal_leaf = active_entry_id(&session);
            let summary_requests = Arc::new(Mutex::new(Vec::new()));
            let snapshot = crate::config::ready_snapshot_for_test();
            let mut controller = RuntimeController::with_session(
                Agent::new(
                    arena,
                    RetainedCompactionModel {
                        run_started: Arc::new(Notify::new()),
                        summary_requests: summary_requests.clone(),
                    },
                ),
                selection,
                snapshot.settings().clone(),
                SubagentCoordinator::new(
                    SubagentPolicy::from(snapshot.subagents()),
                    Arc::new(super::UnavailableChildRunner),
                ),
                session.writer(),
            );
            let agent = controller.agent.take().expect("take racing run agent");
            let (worker_sender, worker) = mpsc::unbounded_channel();
            let (_worker_progress_sender, worker_progress) =
                mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            controller.active = Some(ActiveRun {
                operation: ForegroundOperation::Run,
                provider: controller.selection.model().provider(),
                run_id: Some(RunId(admission.run_id)),
                request_entry: Some(admission.request_entry),
                correction_requested: false,
                persisted_model_steps: BTreeSet::new(),
                pending_busy_submissions: 0,
                caller: controller.subagents.root_caller().await,
                cancellation: CancellationToken::new(),
                worker,
                progress: worker_progress,
                progress_open: false,
                cancellation_requested: false,
            });
            controller.request_active_cancellation(true);
            let (commands, mut events, _progress, controller) = start_controller(controller);

            commands
                .send(RuntimeCommand::Compact {
                    focus: "retain race winner".to_owned(),
                })
                .await
                .expect("retain compaction");
            assert_eq!(
                next_event(&mut events).await,
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                }
            );
            worker_sender
                .send(WorkerMessage::PersistedFinished {
                    agent,
                    result: Ok(crate::agent::PersistedRunOutcome {
                        run_id: admission.run_id,
                        terminal,
                        authentication_failure: false,
                        recovery_failure: false,
                    }),
                    progress_gap: false,
                })
                .expect("report race-winning terminal");
            let events = events_until_compaction_terminal(&mut events).await;

            let terminal = events
                .iter()
                .position(|event| match (terminal_kind, event) {
                    (
                        "final",
                        RuntimeEvent::RecordAppended {
                            record:
                                StoredRecord {
                                    run_id,
                                    record: Record::FinalMessage { text, .. },
                                },
                            ..
                        },
                    ) => *run_id == RunId(admission.run_id) && text == "final wins",
                    (
                        "failed",
                        RuntimeEvent::RecordAppended {
                            record:
                                StoredRecord {
                                    run_id,
                                    record: Record::FailedRun { message, .. },
                                },
                            ..
                        },
                    ) => *run_id == RunId(admission.run_id) && message == "failed wins",
                    _ => false,
                })
                .expect("race-winning terminal record");
            let settled = events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEvent::RunSettled { run_id } if *run_id == RunId(admission.run_id)
                    )
                })
                .expect("run settled");
            let started = events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEvent::CompactionLifecycle {
                            lifecycle: super::CompactionLifecycle::Started,
                        }
                    )
                })
                .expect("compaction started");
            assert!(terminal < settled && settled < started);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, RuntimeEvent::CorrectionCompleted { .. }))
            );
            {
                let summary_requests = summary_requests.lock().expect("read summary requests");
                assert!(!summary_requests.is_empty());
                assert!(
                    summary_requests
                        .iter()
                        .all(|request| request.input.contains("retain race winner"))
                );
            }

            let compaction_parent: i64 = session
                .connection_for_test()
                .expect("open session")
                .query_row(
                    "SELECT parent_id FROM entries WHERE kind = 'compaction'",
                    [],
                    |row| row.get(0),
                )
                .expect("read compaction parent");
            assert_eq!(compaction_parent, terminal_leaf);

            shutdown_controller(commands, controller).await;
            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn retained_compaction_uses_the_leaf_left_by_correction() {
        for (corrupt_correction_target, expected_correction) in
            [(false, "replaced"), (true, "failed")]
        {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let arena = Arena::new(
                worker_path(),
                ToolHost::new(workspace.path())
                    .await
                    .expect("open tool host"),
            )
            .await
            .expect("create arena");
            let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                .expect("select model");
            let session =
                Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
                    .expect("create session");
            let writer = session.writer();
            seed_compactable_history(&writer).await;
            let selected_before_run = active_entry_id(&session);
            if corrupt_correction_target {
                replace_entry_checkpoint_identity(
                    &session,
                    SessionEntryId(selected_before_run as u64),
                    "invalid correction checkpoint",
                );
            }
            let run_started = Arc::new(Notify::new());
            let summary_requests = Arc::new(Mutex::new(Vec::new()));
            let snapshot = crate::config::ready_snapshot_for_test();
            let controller = RuntimeController::with_session(
                Agent::new(
                    arena,
                    RetainedCompactionModel {
                        run_started: run_started.clone(),
                        summary_requests: summary_requests.clone(),
                    },
                ),
                selection,
                snapshot.settings().clone(),
                SubagentCoordinator::new(
                    SubagentPolicy::from(snapshot.subagents()),
                    Arc::new(super::UnavailableChildRunner),
                ),
                session.writer(),
            );
            let (commands, mut events, _progress, controller) = start_controller(controller);

            commands
                .send(RuntimeCommand::Submit {
                    request: "cancel and compact".to_owned(),
                })
                .await
                .expect("start run");
            run_started.notified().await;
            commands
                .send(RuntimeCommand::Cancel { correction: true })
                .await
                .expect("latch correction");
            commands
                .send(RuntimeCommand::Compact {
                    focus: "keep correction leaf".to_owned(),
                })
                .await
                .expect("retain compaction");
            let events = events_until_compaction_terminal(&mut events).await;

            let correction = events
                .iter()
                .position(|event| {
                    matches!(
                        (expected_correction, event),
                        (
                            "replaced",
                            RuntimeEvent::CorrectionCompleted {
                                outcome: CorrectionOutcome::Replaced { .. },
                                ..
                            },
                        ) | (
                            "failed",
                            RuntimeEvent::CorrectionCompleted {
                                outcome: CorrectionOutcome::Failed,
                                ..
                            },
                        )
                    )
                })
                .expect("expected correction outcome");
            let settled_run = events
                .iter()
                .position(|event| matches!(event, RuntimeEvent::RunSettled { .. }))
                .expect("run settled");
            let started = events
                .iter()
                .position(|event| {
                    matches!(
                        event,
                        RuntimeEvent::CompactionLifecycle {
                            lifecycle: super::CompactionLifecycle::Started,
                        }
                    )
                })
                .expect("compaction started");
            assert!(correction < settled_run && settled_run < started);
            assert!(
                summary_requests
                    .lock()
                    .expect("read summary requests")
                    .iter()
                    .all(|request| request.input.contains("keep correction leaf"))
            );

            {
                let connection = session.connection_for_test().expect("open session");
                let compaction_parent: i64 = connection
                    .query_row(
                        "SELECT parent_id FROM entries WHERE kind = 'compaction'",
                        [],
                        |row| row.get(0),
                    )
                    .expect("read compaction parent");
                let cancelled_entry: i64 = connection
                    .query_row(
                        "SELECT id FROM entries WHERE kind = 'cancelled' ORDER BY id DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .expect("read cancelled terminal");
                assert_eq!(
                    compaction_parent,
                    if corrupt_correction_target {
                        cancelled_entry
                    } else {
                        selected_before_run
                    }
                );
            }

            shutdown_controller(commands, controller).await;
            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn retained_compaction_uses_cancelled_leaf_when_correction_skips_background() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: Vec::new(),
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let _child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let run_started = Arc::new(Notify::new());
        let summary_requests = Arc::new(Mutex::new(Vec::new()));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                RetainedCompactionModel {
                    run_started: run_started.clone(),
                    summary_requests: summary_requests.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            coordinator,
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel and compact".to_owned(),
            })
            .await
            .expect("start run");
        run_started.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("latch correction");
        commands
            .send(RuntimeCommand::Compact {
                focus: "keep cancelled leaf".to_owned(),
            })
            .await
            .expect("retain compaction");
        let events = events_until_compaction_terminal(&mut events).await;

        let correction = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::CorrectionCompleted {
                        outcome: CorrectionOutcome::SkippedBackground,
                        ..
                    }
                )
            })
            .expect("correction skipped for background work");
        let settled = events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { .. }))
            .expect("run settled");
        let started = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::CompactionLifecycle {
                        lifecycle: super::CompactionLifecycle::Started,
                    }
                )
            })
            .expect("compaction started");
        assert!(correction < settled && settled < started);
        {
            let summary_requests = summary_requests.lock().expect("read summary requests");
            assert!(!summary_requests.is_empty());
            assert!(
                summary_requests
                    .iter()
                    .all(|request| request.input.contains("keep cancelled leaf"))
            );
        }

        {
            let connection = session.connection_for_test().expect("open session");
            let compaction_parent: i64 = connection
                .query_row(
                    "SELECT parent_id FROM entries WHERE kind = 'compaction'",
                    [],
                    |row| row.get(0),
                )
                .expect("read compaction parent");
            let cancelled_leaf: i64 = connection
                .query_row(
                    "SELECT id FROM entries WHERE kind = 'cancelled' ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .expect("read cancelled terminal");
            assert_eq!(compaction_parent, cancelled_leaf);
        }

        shutdown_controller(commands, controller).await;
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn shutdown_clears_retained_compaction_before_it_starts() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let run_started = Arc::new(Notify::new());
        let summary_requests = Arc::new(Mutex::new(Vec::new()));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                RetainedCompactionModel {
                    run_started: run_started.clone(),
                    summary_requests: summary_requests.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "running request".to_owned(),
            })
            .await
            .expect("start run");
        run_started.notified().await;
        commands
            .send(RuntimeCommand::Compact {
                focus: "retain nothing after shutdown".to_owned(),
            })
            .await
            .expect("retain compaction");
        loop {
            if matches!(
                next_event(&mut events).await,
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                }
            ) {
                break;
            }
        }
        commands
            .send(RuntimeCommand::Shutdown)
            .await
            .expect("shutdown before compaction start");
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown completes")
            .expect("controller task should not panic");

        assert!(
            summary_requests
                .lock()
                .expect("read summary requests")
                .is_empty()
        );
        assert!(
            !records
                .iter()
                .any(|record| matches!(record.record, Record::Compaction { .. }))
        );

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn shutdown_during_compaction_waits_for_cancelled_settlement() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Compaction);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        seed_compactable_history(&writer).await;
        let summary_started = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                GatedSummaryModel {
                    started: summary_started.clone(),
                    release: Arc::new(Notify::new()),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, mut controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Compact {
                focus: String::new(),
            })
            .await
            .expect("start compaction");
        summary_started.notified().await;
        commands
            .send(RuntimeCommand::Shutdown)
            .await
            .expect("shutdown compaction");
        gate.wait_until_started().await;
        assert!(
            timeout(Duration::from_millis(50), &mut controller)
                .await
                .is_err(),
            "shutdown returned before durable cancellation"
        );
        gate.release();
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown completes after settlement")
            .expect("controller task should not panic");
        let events = events_until_compaction_terminal(&mut events).await;

        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Accepted,
                },
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Started,
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::Compaction {
                            outcome: CompactionOutcome::Cancelled { .. },
                        },
                        ..
                    },
                    ..
                },
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::Settled(
                        super::CompactionSettlement::Cancelled,
                    ),
                },
            ]
        ));
        assert!(matches!(
            records.as_slice(),
            [StoredRecord {
                record: Record::Compaction {
                    outcome: CompactionOutcome::Cancelled { .. },
                },
                ..
            }]
        ));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn failed_compaction_settlement_disables_without_a_successful_card() {
        for failure in ["storage", "expected leaf"] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let arena = Arena::new(
                worker_path(),
                ToolHost::new(workspace.path())
                    .await
                    .expect("open tool host"),
            )
            .await
            .expect("create arena");
            let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                .expect("select model");
            let session =
                Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
                    .expect("create session");
            let writer = session.writer();
            seed_compactable_history(&writer).await;
            let summary_started = Arc::new(Notify::new());
            let summary_release = Arc::new(Notify::new());
            if failure == "storage" {
                session
                    .connection_for_test()
                    .expect("open session")
                    .execute_batch(
                        "CREATE TRIGGER reject_compaction BEFORE INSERT ON entries WHEN NEW.kind = 'compaction' BEGIN SELECT RAISE(ABORT, 'reject compaction'); END;",
                    )
                    .expect("reject compaction writes");
            }
            let snapshot = crate::config::ready_snapshot_for_test();
            let controller = RuntimeController::with_session(
                Agent::new(
                    arena,
                    GatedSummaryModel {
                        started: summary_started.clone(),
                        release: summary_release.clone(),
                    },
                ),
                selection,
                snapshot.settings().clone(),
                SubagentCoordinator::new(
                    SubagentPolicy::from(snapshot.subagents()),
                    Arc::new(super::UnavailableChildRunner),
                ),
                session.writer(),
            );
            let (commands, mut events, _progress, controller) = start_controller(controller);

            commands
                .send(RuntimeCommand::Compact {
                    focus: String::new(),
                })
                .await
                .expect("start compaction");
            summary_started.notified().await;
            if failure == "expected leaf" {
                let admission = writer
                    .commit_request("concurrent leaf".to_owned(), selection)
                    .await
                    .expect("advance selected leaf");
                let step_id = writer
                    .start_model_step(admission.run_id)
                    .await
                    .expect("start concurrent step");
                writer
                    .close_run(
                        admission.run_id,
                        Some(step_id),
                        move || PersistedTerminal::Final {
                            step_id,
                            text: "concurrent reply".to_owned(),
                            reasoning_summary: String::new(),
                            replay: ProviderReplay::new("fixture", Vec::new()),
                            attempts: Vec::new(),
                        },
                        None,
                    )
                    .await
                    .expect("close concurrent run");
            }
            summary_release.notify_one();
            summary_release.notify_one();
            let disabled_events = events_until_disabled(&mut events).await;

            assert!(matches!(
                disabled_events.as_slice(),
                [
                    RuntimeEvent::CompactionAcknowledged {
                        acknowledgement: super::CompactionAcknowledgement::Accepted,
                    },
                    RuntimeEvent::CompactionLifecycle {
                        lifecycle: super::CompactionLifecycle::Started,
                    },
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                    },
                ]
            ));
            assert_eq!(
                session
                    .connection_for_test()
                    .expect("open session")
                    .query_row(
                        "SELECT COUNT(*) FROM entries WHERE kind = 'compaction'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("count compactions"),
                0
            );
            commands
                .send(RuntimeCommand::Compact {
                    focus: String::new(),
                })
                .await
                .expect("retry after disable");
            assert!(matches!(
                next_event(&mut events).await,
                RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: super::CompactionAcknowledgement::Disabled {
                        reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                    },
                }
            ));

            shutdown_controller(commands, controller).await;
            drop(writer);
            session.close().expect("close session");
        }
    }

    #[tokio::test]
    async fn persisted_agent_reads_context_from_session_model() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let writer = session.writer();
        let prior = writer
            .commit_request("saved request".to_owned(), selection)
            .await
            .expect("commit saved request");
        writer
            .close_run(
                prior.run_id,
                None,
                || PersistedTerminal::Cancelled {
                    evidence: None,
                    attempts: Vec::new(),
                },
                None,
            )
            .await
            .expect("close saved request");
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(
            arena,
            ContextCapturingMessageModel {
                contexts: contexts.clone(),
            },
        );
        let mut event = |_| {};

        agent
            .run_persisted(
                selection,
                "next request".to_owned(),
                &CancellationToken::new(),
                &writer,
                &mut event,
            )
            .await
            .expect("run persisted agent");

        assert_eq!(
            contexts.lock().expect("contexts lock")[0],
            [
                ModelContextEntry::UserRequest {
                    text: "saved request".to_owned(),
                },
                ModelContextEntry::UserRequest {
                    text: "next request".to_owned(),
                },
            ]
        );

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn request_commit_precedes_provider_call() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Request);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_called = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                AdmissionGatedModel {
                    calls: calls.clone(),
                    provider_called: provider_called.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "persist this request".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            session
                .writer()
                .model_history()
                .expect("read uncommitted context")
                .entries()
                .is_empty()
        );

        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        gate.release();
        let events = events_until_settled(&mut events).await;
        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { request, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::CancelledRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ] if request == "persist this request"
        ));
        let records = shutdown_controller(commands, controller).await;
        assert_eq!(records.len(), 2);
        assert_eq!(
            session
                .connection_for_test()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM entries WHERE kind = 'user_request'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn model_step_commit_precedes_provider_call() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Step);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_called = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                AdmissionGatedModel {
                    calls: calls.clone(),
                    provider_called: provider_called.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "allocate a step".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        gate.release();
        timeout(TEST_TIMEOUT, provider_called.notified())
            .await
            .expect("provider should begin after the step commit");
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        session.close().unwrap();
    }

    #[tokio::test]
    async fn settlement_commit_precedes_the_next_provider_call() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Settlement);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_called = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                SettlementGatedModel {
                    calls: calls.clone(),
                    provider_called: provider_called.clone(),
                    source: "value = 1".to_owned(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "settle before continuing".to_owned(),
            })
            .await
            .unwrap();
        timeout(TEST_TIMEOUT, provider_called.notified())
            .await
            .expect("first provider call");
        gate.wait_until_started().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            session
                .writer()
                .model_history()
                .expect("read uncommitted context")
                .entries(),
            [ModelContextEntry::UserRequest { .. }]
        ));

        gate.release();
        timeout(TEST_TIMEOUT, provider_called.notified())
            .await
            .expect("second provider call after settlement");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(matches!(
            session
                .writer()
                .model_history()
                .expect("read committed context")
                .entries()
                .get(1),
            Some(ModelContextEntry::CompletedCode(_))
        ));
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        session.close().unwrap();
    }

    #[tokio::test]
    async fn closure_commit_arbitrates_cancel_at_storage_linearisation() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                CountingMessageModel {
                    calls: calls.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "finish then cancel".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            session
                .writer()
                .model_history()
                .expect("read uncommitted context")
                .entries(),
            [ModelContextEntry::UserRequest { .. }]
        ));
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        loop {
            if next_event(&mut events).await == (RuntimeEvent::SettingsAccepted { revision: 1 }) {
                break;
            }
        }
        gate.release();
        let events = events_until_settled(&mut events).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::CancelledRun { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        let records = shutdown_controller(commands, controller).await;
        assert!(matches!(
            records.last(),
            Some(StoredRecord {
                record: Record::CancelledRun {
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Generation(_)),
                        ..
                    },
                },
                ..
            })
        ));
        let (cancelled, final_replies, payload): (i64, i64, String) = session
            .connection_for_test()
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries WHERE kind = 'cancelled'), (SELECT COUNT(*) FROM entries WHERE kind = 'final_reply'), (SELECT payload_json FROM entries WHERE kind = 'cancelled')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!((cancelled, final_replies), (1, 0));
        assert_eq!(
            payload["Terminal"]["terminal"]["Cancelled"]["evidence"]["Generation"]["assistant_text"],
            "done"
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn closure_commit_cancellation_wins_over_provider_failure() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(arena, ClosureCommitProviderFailureModel { steps: 0 }),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel provider failure".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        let mut first_events = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            if event == (RuntimeEvent::SettingsAccepted { revision: 1 }) {
                break;
            }
            first_events.push(event);
        }
        gate.release();
        first_events.extend(events_until_settled(&mut events).await);

        assert!(!first_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::OpenAiAuthentication,
            }
        )));
        assert!(!first_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::FailedRun { .. },
                    ..
                },
                ..
            }
        )));
        assert_eq!(
            first_events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::CancelledRun { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            first_events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(1) }))
                .count(),
            1
        );
        let records = committed_records(&first_events);
        let Some(StoredRecord {
            record: Record::CancelledRun { terminal },
            ..
        }) = records.last()
        else {
            panic!("cancellation should win over the provider failure");
        };
        let Some(TerminalEvidence::Generation(evidence)) = &terminal.evidence else {
            panic!("cancelled provider failure should retain generation evidence");
        };
        assert_eq!(evidence.reasoning, "thinking");
        assert_eq!(evidence.assistant_text, "partial answer");
        assert_eq!(evidence.decoded_python, "print('partial')");
        assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);

        let (cancelled, failed, terminal_entries, closed_runs, settled_steps, payload):
            (i64, i64, i64, i64, i64, String) = session
            .connection_for_test()
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries WHERE kind = 'cancelled'), (SELECT COUNT(*) FROM entries WHERE kind = 'failed'), (SELECT COUNT(*) FROM entries WHERE kind IN ('cancelled', 'failed', 'final_reply')), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NOT NULL), (SELECT payload_json FROM entries WHERE kind = 'cancelled')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            (
                cancelled,
                failed,
                terminal_entries,
                closed_runs,
                settled_steps
            ),
            (1, 0, 1, 1, 1)
        );
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            payload["Terminal"]["terminal"]["Cancelled"]["evidence"]["Generation"]["reasoning"],
            "thinking"
        );
        assert_eq!(
            payload["Terminal"]["terminal"]["Cancelled"]["evidence"]["Generation"]["assistant_text"],
            "partial answer"
        );
        assert_eq!(
            payload["Terminal"]["terminal"]["Cancelled"]["evidence"]["Generation"]["decoded_python"],
            "print('partial')"
        );
        assert_eq!(
            payload["Terminal"]["terminal"]["Cancelled"]["attempts"],
            serde_json::json!(["UsageUnavailable"])
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        let later_events = events_until_settled(&mut events).await;
        assert!(matches!(
            later_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FinalMessage { text, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(2) },
            ] if text == "later completed"
        ));
        assert_eq!(shutdown_controller(commands, controller).await.len(), 4);
        session.close().unwrap();
    }

    #[tokio::test]
    async fn persisted_failed_run_messages_are_bounded_at_utf8_boundaries() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(arena, OversizedProviderFailureModel),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "trigger oversized failure".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;
        let expected = format!(
            "model provider failed: provider failure (HTTP 500) [oversize]: {}",
            "é".repeat(2016)
        );

        let Record::FailedRun { message, .. } = &committed_records(&events)[1].record else {
            panic!("expected emitted failed run");
        };
        assert_eq!(message, &expected);
        assert!(message.len() <= 4 * 1024);
        assert!(message.is_char_boundary(message.len()));
        let Record::FailedRun { message, .. } = &records[1].record else {
            panic!("expected readable failed run");
        };
        assert_eq!(message, &expected);
        assert!(message.len() <= 4 * 1024);
        assert!(message.is_char_boundary(message.len()));

        let restore = session.restore().unwrap();
        let RestoredRecordKind::FailedRun { message, .. } = &restore.records[1].kind else {
            panic!("expected restored failed run");
        };
        assert_eq!(message, &expected);
        assert!(message.len() <= 4 * 1024);
        assert!(message.is_char_boundary(message.len()));
        session.close().unwrap();
    }

    #[tokio::test]
    async fn closure_commit_arbitrates_shutdown_at_storage_linearisation() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                CountingMessageModel {
                    calls: calls.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "finish then shutdown".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        loop {
            if next_event(&mut events).await == (RuntimeEvent::SettingsAccepted { revision: 1 }) {
                break;
            }
        }
        gate.release();
        let events = events_until_settled(&mut events).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::CancelledRun { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown should return after terminal persistence")
            .expect("controller task should not panic");
        assert!(matches!(
            records.last(),
            Some(StoredRecord {
                record: Record::CancelledRun {
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Generation(_)),
                        ..
                    },
                },
                ..
            })
        ));
        let (cancelled, final_replies, payload): (i64, i64, String) = session
            .connection_for_test()
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries WHERE kind = 'cancelled'), (SELECT COUNT(*) FROM entries WHERE kind = 'final_reply'), (SELECT payload_json FROM entries WHERE kind = 'cancelled')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!((cancelled, final_replies), (1, 0));
        assert_eq!(
            payload["Terminal"]["terminal"]["Cancelled"]["evidence"]["Generation"]["assistant_text"],
            "done"
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn request_storage_failure_disables_the_runtime_before_provider_work() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Request);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        session
            .connection_for_test()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_request BEFORE INSERT ON entries WHEN NEW.kind = 'user_request' BEGIN SELECT RAISE(ABORT, 'reject request'); END;",
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_called = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                AdmissionGatedModel {
                    calls: calls.clone(),
                    provider_called: provider_called.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cannot persist".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        commands
            .send(RuntimeCommand::Submit {
                request: "also retained".to_owned(),
            })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        gate.release();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        commands
            .send(RuntimeCommand::Submit {
                request: "also blocked".to_owned(),
            })
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed
            }
        ));
        assert!(shutdown_controller(commands, controller).await.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            session
                .connection_for_test()
                .unwrap()
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM runs), (SELECT COUNT(*) FROM model_steps), revision FROM session",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)),
                )
                .unwrap(),
            (1, 0, 0, 2)
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn latched_coordinator_storage_failure_beats_submit() {
        let workspace = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(super::UnavailableChildRunner),
        );
        let controller = controller_with_subagents(
            &workspace,
            CountingMessageModel {
                calls: calls.clone(),
            },
            coordinator.clone(),
        )
        .await;
        coordinator.latch_storage_failure().await;
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "must not start".to_owned(),
            })
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(shutdown_controller(commands, controller).await.is_empty());
    }

    #[tokio::test]
    async fn step_allocation_storage_failure_stops_before_provider_work() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        session
            .connection_for_test()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_step BEFORE INSERT ON model_steps BEGIN SELECT RAISE(ABORT, 'reject step'); END;",
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_called = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                AdmissionGatedModel {
                    calls: calls.clone(),
                    provider_called,
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cannot allocate".to_owned(),
            })
            .await
            .unwrap();
        let disabled_events = events_until_disabled(&mut events).await;
        assert!(matches!(
            disabled_events.last(),
            Some(RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(disabled_events.iter().all(|event| !matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. }
                        | Record::FailedRun { .. }
                        | Record::FinalMessage { .. },
                    ..
                },
                ..
            } | RuntimeEvent::RunSettled { .. }
        )));
        commands
            .send(RuntimeCommand::Submit {
                request: "also blocked".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let records = shutdown_controller(commands, controller).await;
        assert!(matches!(
            records.as_slice(),
            [StoredRecord {
                record: Record::UserRequest { .. },
                ..
            }]
        ));
        assert_eq!(
            session
                .connection_for_test()
                .unwrap()
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM runs), (SELECT COUNT(*) FROM model_steps), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), revision FROM session",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
                )
                .unwrap(),
            (2, 1, 0, 0, 2)
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn settlement_storage_failure_stops_later_model_work() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        session
            .connection_for_test()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_execution BEFORE INSERT ON entries WHEN NEW.kind = 'execution' BEGIN SELECT RAISE(ABORT, 'reject execution'); END;",
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                SettlementGatedModel {
                    calls: calls.clone(),
                    provider_called: Arc::new(Notify::new()),
                    source: "await run(\"printf 'settled'\")".to_owned(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cannot settle".to_owned(),
            })
            .await
            .unwrap();
        let disabled_events = events_until_disabled(&mut events).await;
        assert!(matches!(
            disabled_events.last(),
            Some(RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(disabled_events.iter().all(|event| !matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. }
                        | Record::FailedRun { .. }
                        | Record::FinalMessage { .. }
                        | Record::CompletedExecution { .. },
                    ..
                },
                ..
            } | RuntimeEvent::RunSettled { .. }
                | RuntimeEvent::ToolUpdated { .. }
        )));
        commands
            .send(RuntimeCommand::Submit {
                request: "also blocked".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let records = shutdown_controller(commands, controller).await;
        assert!(matches!(
            records.as_slice(),
            [StoredRecord {
                record: Record::UserRequest { .. },
                ..
            }]
        ));
        assert_eq!(
            session
                .connection_for_test()
                .unwrap()
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), revision FROM session",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?, row.get::<_, i64>(4)?)),
                )
                .unwrap(),
            (2, 1, 1, 0, 3)
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn closure_storage_failure_stops_later_model_work() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        session
            .connection_for_test()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_terminal BEFORE INSERT ON entries WHEN NEW.kind = 'final_reply' BEGIN SELECT RAISE(ABORT, 'reject terminal'); END;",
            )
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                CountingMessageModel {
                    calls: calls.clone(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cannot close".to_owned(),
            })
            .await
            .unwrap();
        let disabled_events = events_until_disabled(&mut events).await;
        assert!(matches!(
            disabled_events.last(),
            Some(RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(disabled_events.iter().all(|event| !matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. }
                        | Record::FailedRun { .. }
                        | Record::FinalMessage { .. },
                    ..
                },
                ..
            } | RuntimeEvent::RunSettled { .. }
        )));
        commands
            .send(RuntimeCommand::Submit {
                request: "also blocked".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let records = shutdown_controller(commands, controller).await;
        assert!(matches!(
            records.as_slice(),
            [StoredRecord {
                record: Record::UserRequest { .. },
                ..
            }]
        ));
        assert_eq!(
            session
                .connection_for_test()
                .unwrap()
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), revision FROM session",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, i64>(3)?)),
                )
                .unwrap(),
            (2, 1, 0, 3)
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn task_admission_storage_failure_disables_runtime_without_spawning_a_child() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        session
            .connection_for_test()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_task_admission BEFORE INSERT ON subagent_tasks BEGIN SELECT RAISE(ABORT, 'reject task'); END;",
            )
            .unwrap();
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(vec![ModelAction::Code {
                    source: "await start_subagent('background work')".to_owned(),
                }]),
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(CancellationOnlyChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "start a child".to_owned(),
            })
            .await
            .unwrap();

        let mut lifecycle = Vec::new();
        let mut disabled = false;
        let mut settled = false;
        while !disabled || !settled {
            let event = next_event(&mut events).await;
            disabled |= matches!(
                event,
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                }
            );
            settled |= matches!(event, RuntimeEvent::RunSettled { .. });
            lifecycle.push(event);
        }
        assert_eq!(
            lifecycle
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::SessionStorageWriteFailed
                    }
                ))
                .count(),
            1
        );
        assert!(lifecycle.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. },
                    ..
                },
                ..
            }
        )));
        assert!(lifecycle.iter().all(|event| !matches!(
            event,
            RuntimeEvent::SubagentSpawned { .. } | RuntimeEvent::SubagentFinished { .. }
        )));
        let task_rows: i64 = session
            .connection_for_test()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM subagent_tasks", [], |row| row.get(0))
            .unwrap();
        assert_eq!(task_rows, 0);

        commands
            .send(RuntimeCommand::Submit {
                request: "still disabled".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        shutdown_controller(commands, controller).await;
        session.close().unwrap();
    }

    #[tokio::test]
    async fn background_terminal_storage_failure_disables_after_root_settles() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        session
            .connection_for_test()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_task_terminal BEFORE UPDATE ON subagent_tasks WHEN NEW.terminal_revision IS NOT NULL BEGIN SELECT RAISE(ABORT, 'reject terminal'); END;",
            )
            .unwrap();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: Vec::new(),
        };
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(vec![
                    ModelAction::Code {
                        source: "await start_subagent('background work')".to_owned(),
                    },
                    ModelAction::Message {
                        text: "root complete".to_owned(),
                    },
                ]),
            ),
            selection,
            snapshot.settings().clone(),
            complete_child_coordinator(factory.clone()),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "start a child".to_owned(),
            })
            .await
            .unwrap();

        let mut root_events = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            let settled = matches!(event, RuntimeEvent::RunSettled { .. });
            root_events.push(event);
            if settled {
                break;
            }
        }
        assert!(
            root_events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SubagentSpawned { .. }))
        );
        assert!(root_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::FinalMessage { text, .. },
                    ..
                },
                ..
            } if text == "root complete"
        )));
        factory.started.notified().await;
        factory.release.notify_one();

        let disabled = events_until_disabled(&mut events).await;
        assert_eq!(
            disabled
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::SessionStorageWriteFailed
                    }
                ))
                .count(),
            1
        );
        assert!(
            disabled
                .iter()
                .all(|event| !matches!(event, RuntimeEvent::SubagentFinished { .. }))
        );
        let (rows, terminal_rows): (i64, i64) = session
            .connection_for_test()
            .unwrap()
            .query_row(
                "SELECT COUNT(*), COUNT(terminal_revision) FROM subagent_tasks",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((rows, terminal_rows), (1, 0));
        commands
            .send(RuntimeCommand::Submit {
                request: "still disabled".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        shutdown_controller(commands, controller).await;
        session.close().unwrap();
    }

    #[tokio::test]
    async fn busy_submissions_before_request_commit_use_the_committed_run_id() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let gate = SessionWriteGate::new(SessionWriteKind::Request);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                AdmissionGatedModel {
                    calls: calls.clone(),
                    provider_called: Arc::new(Notify::new()),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_started().await;
        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::Submit {
                request: "third".to_owned(),
            })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        gate.release();
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest { .. },
                },
                ..
            }
        ));
        for _ in 0..2 {
            assert_eq!(
                next_event(&mut events).await,
                RuntimeEvent::SubmissionRejectedBusy {
                    active_run_id: Some(RunId(1)),
                }
            );
        }
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        session.close().unwrap();
    }

    #[tokio::test]
    async fn worker_closure_rejects_retained_precommit_submissions_after_disabling() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let (worker_sender, worker) = mpsc::unbounded_channel();
        drop(worker_sender);
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        controller.active = Some(ActiveRun {
            operation: ForegroundOperation::Run,
            provider: controller.selection.model().provider(),
            run_id: None,
            request_entry: None,
            correction_requested: false,
            persisted_model_steps: BTreeSet::new(),
            pending_busy_submissions: 1,
            caller: controller.subagents.root_caller().await,
            cancellation: CancellationToken::new(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
        });
        let (commands, mut events, _progress, controller) = start_controller(controller);

        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert!(
            timeout(TEST_TIMEOUT, controller)
                .await
                .expect("worker closure should stop the controller")
                .expect("controller task should not panic")
                .is_empty()
        );
        drop(commands);
    }

    async fn next_event(events: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
        timeout(TEST_TIMEOUT, events.recv())
            .await
            .expect("runtime should emit an event before the test timeout")
            .expect("runtime event channel should remain open")
    }

    async fn events_until_disabled(
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> Vec<RuntimeEvent> {
        let mut collected = Vec::new();
        loop {
            let event = next_event(events).await;
            let disabled = matches!(event, RuntimeEvent::RuntimeDisabled { .. });
            collected.push(event);
            if disabled {
                return collected;
            }
        }
    }

    async fn wait_for_queued_events(events: &mpsc::UnboundedReceiver<RuntimeEvent>, count: usize) {
        timeout(TEST_TIMEOUT, async {
            while events.len() < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime should queue events before the test timeout");
    }

    async fn events_until_settled(
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> Vec<RuntimeEvent> {
        let mut collected = Vec::new();
        loop {
            let event = next_event(events).await;
            let settled = matches!(event, RuntimeEvent::RunSettled { .. });
            collected.push(event);
            if settled {
                return collected;
            }
        }
    }

    async fn events_until_compaction_terminal(
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> Vec<RuntimeEvent> {
        let mut collected = Vec::new();
        loop {
            let event = next_event(events).await;
            let terminal = matches!(
                event,
                RuntimeEvent::CompactionLifecycle {
                    lifecycle: super::CompactionLifecycle::NoOp
                        | super::CompactionLifecycle::Settled(_),
                }
            );
            collected.push(event);
            if terminal {
                return collected;
            }
        }
    }

    fn assert_restore_unchanged(before: &SessionRestore, after: &SessionRestore) {
        assert_eq!(after.effective_entry, before.effective_entry);
        assert_eq!(after.observed_active_entry, before.observed_active_entry);
        assert_eq!(after.request_prefill, before.request_prefill);
        assert_eq!(after.metadata, before.metadata);
        assert_eq!(after.records, before.records);
        assert_eq!(after.tasks, before.tasks);
        assert_eq!(after.all_tasks, before.all_tasks);
        assert_eq!(after.footer_adjustment, before.footer_adjustment);
        assert_eq!(after.next_task, before.next_task);
        assert_eq!(
            after
                .executable
                .as_ref()
                .map(|state| (&state.checkpoint, &state.history)),
            before
                .executable
                .as_ref()
                .map(|state| (&state.checkpoint, &state.history))
        );
    }

    fn session_database_snapshot(session: &Session) -> (i64, i64, i64, i64, i64, i64, i64) {
        session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT active_entry_id, revision, updated_at_ms, (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM runs), (SELECT COUNT(*) FROM model_steps), (SELECT COUNT(*) FROM subagent_tasks) FROM session WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("snapshot session database")
    }

    async fn progress_until_execution_started(
        progress: &mut RuntimeProgressReceiver,
        step_id: ModelStepId,
    ) -> Vec<RuntimeEvent> {
        let mut collected = Vec::new();
        loop {
            let envelope = timeout(TEST_TIMEOUT, progress.recv())
                .await
                .expect("runtime should emit progress before the test timeout")
                .expect("progress channel should remain open");
            let started = matches!(
                envelope.item,
                RuntimeEvent::ExecutionStarted {
                    step_id: actual_step_id,
                    ..
                } if actual_step_id == step_id
            );
            collected.push(envelope.item);
            if started {
                return collected;
            }
        }
    }

    fn drain_progress(
        progress: &mut RuntimeProgressReceiver,
    ) -> Vec<ProgressEnvelope<RuntimeEvent>> {
        let mut collected = Vec::new();
        while let Ok(envelope) = progress.try_recv() {
            collected.push(envelope);
        }
        collected
    }

    async fn shutdown_controller(
        commands: mpsc::Sender<RuntimeCommand>,
        controller: JoinHandle<Vec<StoredRecord>>,
    ) -> Vec<StoredRecord> {
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        timeout(TEST_TIMEOUT, controller)
            .await
            .expect("controller should stop before the test timeout")
            .expect("controller task should not panic")
    }

    fn event_label(event: &RuntimeEvent) -> String {
        match event {
            RuntimeEvent::RunStarted { run_id } => format!("run:{}", run_id.0),
            RuntimeEvent::ModelStepStarted { run_id, step_id } => {
                format!("model:{}:{}", run_id.0, step_id.0)
            }
            RuntimeEvent::ModelProgress { .. } => "model-progress".to_owned(),
            RuntimeEvent::ExecutionStarted {
                run_id, step_id, ..
            } => format!("execution:{}:{}", run_id.0, step_id.0),
            RuntimeEvent::ToolUpdated {
                run_id,
                step_id,
                call,
                ..
            } => format!(
                "tool-updated:{}:{}:{}",
                run_id.0,
                step_id.0,
                tool_call_label(call.clone())
            ),
            RuntimeEvent::RunPreviewUpdated { .. } => "run-preview-updated".to_owned(),
            RuntimeEvent::StdoutDelta { .. } => "stdout-delta".to_owned(),
            RuntimeEvent::StdoutTruncated { .. } => "stdout-truncated".to_owned(),
            RuntimeEvent::RecordAppended { record, .. } => record_label(record),
            RuntimeEvent::SubagentSpawned { record } => {
                format!("subagent-spawned:{}", record.id())
            }
            RuntimeEvent::SubagentStatus { id, revision, .. } => {
                format!("subagent-status:{id}:{revision}")
            }
            RuntimeEvent::SubagentRecord { record } => {
                format!("subagent-record:{}:{}", record.id(), record.revision())
            }
            RuntimeEvent::SubagentFinished { record } => {
                format!("subagent-finished:{}", record.id())
            }
            RuntimeEvent::SubagentOperationSettled { id } => {
                format!("subagent-operation-settled:{id}")
            }
            RuntimeEvent::RunSettled { run_id } => format!("settled:{}", run_id.0),
            RuntimeEvent::SettingsAccepted { revision } => format!("settings:{revision}"),
            RuntimeEvent::NameAccepted { revision, name } => format!("name:{revision}:{name}"),
            RuntimeEvent::NavigationCompleted { .. } => "navigation-completed".to_owned(),
            RuntimeEvent::CorrectionCompleted { .. } => "correction-completed".to_owned(),
            RuntimeEvent::TreeCatalogue { revision, .. } => format!("tree-catalogue:{revision}"),
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(active_run_id),
            } => format!("busy:{}", active_run_id.0),
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: None,
            } => "busy:compaction".to_owned(),
            RuntimeEvent::CompactionAcknowledged { acknowledgement } => {
                format!("compaction-ack:{acknowledgement:?}")
            }
            RuntimeEvent::CompactionLifecycle { lifecycle } => {
                format!("compaction:{lifecycle:?}")
            }
            RuntimeEvent::RuntimeDisabled { reason } => format!("disabled:{reason:?}"),
            RuntimeEvent::SubmissionRejectedDisabled { reason } => {
                format!("disabled-rejected:{reason:?}")
            }
        }
    }

    fn tool_call_label(call: ToolCall) -> String {
        match call.summary {
            ToolCallSummary::Pending => format!("{}:pending", call.tool.label()),
            ToolCallSummary::Failed => format!("{}:failed", call.tool.label()),
            ToolCallSummary::Exit(code) => format!("{}:exit {code}", call.tool.label()),
            ToolCallSummary::Signal => format!("{}:signal", call.tool.label()),
            ToolCallSummary::Count(count) => {
                format!(
                    "{}:{count} {}",
                    call.tool.label(),
                    call.tool.count_label(count)
                )
            }
            ToolCallSummary::Created => format!("{}:created", call.tool.label()),
            ToolCallSummary::Replaced => format!("{}:replaced", call.tool.label()),
            ToolCallSummary::Applied => format!("{}:applied", call.tool.label()),
            ToolCallSummary::Subagent(id) => format!("{}:{id}", call.tool.label()),
        }
    }

    #[tokio::test]
    async fn child_task_step_storage_failure_disables_runtime_without_event_acknowledgement() {
        let home = tempfile::tempdir().expect("create session home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select default model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch(
                "CREATE TRIGGER reject_task_step BEFORE UPDATE ON subagent_tasks WHEN NEW.terminal_revision IS NULL BEGIN SELECT RAISE(ABORT, 'reject task step'); END;",
            )
            .expect("reject child task-step writes");
        let child_factory = TaskStepFailureChildFactory {
            calls: Arc::new(AtomicUsize::new(0)),
        };
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(CompleteChildRunner::new(
                "child system prompt".to_owned(),
                child_factory.clone(),
            )),
        );
        let mut child_events = coordinator
            .take_event_receiver()
            .expect("reserve undrained child event receiver");
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(vec![
                    ModelAction::Code {
                        source: "task = await start_subagent('child work')\nawait task.wait()"
                            .to_owned(),
                    },
                    ModelAction::Message {
                        text: "must not run".to_owned(),
                    },
                ]),
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            coordinator.clone(),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "wait for child".to_owned(),
            })
            .await
            .expect("submit foreground request");

        let mut runtime_events = events_until_disabled(&mut events).await;
        assert_eq!(child_factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime_events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                    }
                ))
                .count(),
            1
        );
        commands
            .send(RuntimeCommand::Submit {
                request: "must be rejected".to_owned(),
            })
            .await
            .expect("submit after storage failure");
        let mut foreground_settled = false;
        let mut later_submission_rejected = false;
        while !foreground_settled || !later_submission_rejected {
            let event = next_event(&mut events).await;
            foreground_settled |= matches!(event, RuntimeEvent::RunSettled { run_id: RunId(1) });
            later_submission_rejected |= matches!(
                event,
                RuntimeEvent::SubmissionRejectedDisabled {
                    reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                }
            );
            runtime_events.push(event);
        }
        assert!(runtime_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. },
                    ..
                },
                ..
            }
        )));
        assert!(coordinator.storage_failure_token().is_cancelled());
        assert_eq!(coordinator.active_count().await, 0);

        shutdown_controller(commands, controller).await;
        while let Ok(event) = child_events.try_recv() {
            assert!(
                !matches!(event, CoordinatorEvent::Finished { .. }),
                "storage failure must not publish an uncommitted terminal result"
            );
        }
        drop(child_events);
        drop(coordinator);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn gathered_starts_do_not_starve_deadlines_or_background_cancellation() {
        let workspace = tempfile::tempdir().unwrap();
        let gate = StartSubagentTestGate::new();
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(CancellationOnlyChildRunner),
        )
        .with_start_test_gate(gate.clone());
        let source = "import asyncio\nhandles = await asyncio.gather(start_subagent('first'), start_subagent('second'), start_subagent('third'))\nlen(handles) == 3 and handles[0].id != handles[1].id and handles[0].id != handles[2].id and handles[1].id != handles[2].id";
        let message_waiting = Arc::new(Notify::new());
        let controller = controller_with_subagents(
            &workspace,
            MessageWaitingModel {
                model: ScriptedModel::new(vec![
                    ModelAction::Code {
                        source: source.to_owned(),
                    },
                    ModelAction::Message {
                        text: "foreground complete".to_owned(),
                    },
                ]),
                message_waiting: message_waiting.clone(),
            },
            coordinator,
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "start children".to_owned(),
            })
            .await
            .unwrap();

        timeout(TEST_TIMEOUT, async {
            gate.wait_until_first_start_locked().await;
            gate.wait_until_start_attempts(3).await;
        })
        .await
        .expect("all gathered starts should queue behind the held coordinator lock");
        gate.allow_first_start();

        let mut event_order = Vec::new();
        let execution = match timeout(TEST_TIMEOUT, async {
            loop {
                let event = events
                    .recv()
                    .await
                    .expect("runtime event channel remains open");
                event_order.push(event_label(&event));
                if let RuntimeEvent::RecordAppended {
                    record:
                        StoredRecord {
                            record:
                                Record::CompletedExecution {
                                    source: completed_source,
                                    execution,
                                    ..
                                },
                            ..
                        },
                    ..
                } = event
                    && completed_source == source
                {
                    return execution;
                }
            }
        })
        .await
        {
            Ok(execution) => execution,
            Err(_) => panic!("gathered starts stalled; observed event order: {event_order:?}"),
        };
        let ExecutionOutcome::Complete {
            result: Some(result),
        } = execution.outcome
        else {
            panic!("gathered starts should return their handles");
        };
        assert_eq!(result.head, "True");
        assert!(execution.tool_calls.iter().all(|call| {
            matches!(
                call,
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent(_),
                }
            )
        }));
        assert_eq!(execution.tool_calls.len(), 3);
        timeout(TEST_TIMEOUT, message_waiting.notified())
            .await
            .expect("foreground should remain active while background cancellation runs");

        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .unwrap();
        let mut finished = BTreeSet::new();
        timeout(TEST_TIMEOUT, async {
            while finished.len() < 3 {
                if let RuntimeEvent::SubagentFinished { record } = next_event(&mut events).await {
                    assert_eq!(record.observation().state(), TaskState::Cancelled);
                    finished.insert(record.id().clone());
                }
            }
        })
        .await
        .expect("background cancellation should settle every gathered child");

        shutdown_controller(commands, task).await;
    }

    #[tokio::test]
    async fn background_children_do_not_block_later_foreground_requests() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;

        let controller = controller_with_subagents(
            &workspace,
            ScriptedModel::new(vec![ModelAction::Message {
                text: "foreground complete".to_owned(),
            }]),
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let first_events = events_until_settled(&mut events).await;
        assert!(matches!(
            first_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { request, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FinalMessage { text, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ] if request == "first" && text == "foreground complete"
        ));
        assert_eq!(
            coordinator.get(&caller, child.id()).await.unwrap().state(),
            TaskState::Running
        );
        assert_eq!(
            coordinator
                .get(&caller, child.id())
                .await
                .unwrap()
                .step_count(),
            1
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest { request, .. },
                },
                ..
            } if request == "second"
        ));
        assert_eq!(
            coordinator.get(&caller, child.id()).await.unwrap().state(),
            TaskState::Running
        );
        let second_events = events_until_settled(&mut events).await;
        assert!(matches!(
            second_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(2),
                        record: Record::FinalMessage { text, .. },
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(2) },
            ] if text == "foreground complete"
        ));
        assert_eq!(
            coordinator.get(&caller, child.id()).await.unwrap().state(),
            TaskState::Running
        );

        factory.release.notify_one();
        let (result, observation) = coordinator.wait(&caller, child.id()).await.unwrap();
        assert_eq!(result.message(), "child complete");
        assert_eq!(result.values(), &BTreeMap::new());
        assert_eq!(observation.state(), TaskState::Completed);
        assert_eq!(observation.step_count(), 1);
        let records = coordinator.records().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].id(), child.id());
        assert_eq!(
            records[0].attempts(),
            &[AttemptAccounting::UsageUnavailable]
        );
        assert!(matches!(
            records[0].outcome(),
            Some(crate::subagents::TaskOutcome::Completed(result))
                if result.message() == "child complete" && result.values().is_empty()
        ));
        assert_eq!(coordinator.active_count().await, 0);
        shutdown_controller(commands, task).await;
    }

    #[tokio::test]
    async fn explicit_background_cancellation_leaves_foreground_running() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        let foreground_ready = Arc::new(Notify::new());
        let controller = controller_with_subagents(
            &workspace,
            WaitingCancellationModel {
                ready: foreground_ready.clone(),
                partial: false,
            },
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "foreground".to_owned(),
            })
            .await
            .unwrap();
        foreground_ready.notified().await;

        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .unwrap();
        let error = coordinator.wait(&caller, child.id()).await.unwrap_err();
        assert_eq!(
            error.observation().unwrap().step_count(),
            1,
            "the real child started one model step"
        );
        assert!(matches!(
            error,
            crate::subagents::WaitError::Cancelled { .. }
        ));
        assert_eq!(
            coordinator.records().await[0].attempts(),
            &[AttemptAccounting::UsageUnavailable]
        );
        assert!(
            timeout(Duration::from_millis(50), events_until_settled(&mut events))
                .await
                .is_err(),
            "background cancellation settled the foreground request"
        );

        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, task).await;
    }

    #[tokio::test]
    async fn traced_runtime_cancellation_commands_bracket_foreground_and_background_work() {
        let workspace = tempfile::tempdir().unwrap();
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let child_factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(CompleteChildRunner::new(
                "child system prompt".to_owned(),
                child_factory.clone(),
            )),
        )
        .with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(
                &caller,
                ChildParameters::new(
                    "child-request-SECRET".to_owned(),
                    ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
                    false,
                ),
                child_payload(&workspace).await,
            )
            .await
            .expect("admit background child");
        child_factory.started.notified().await;

        let foreground_ready = Arc::new(Notify::new());
        let controller = controller_with_subagents(
            &workspace,
            WaitingCancellationModel {
                ready: foreground_ready.clone(),
                partial: false,
            },
            coordinator.clone(),
        )
        .await
        .with_trace(trace_owner.handle());
        let (commands, mut events, _, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "foreground-request-SECRET".to_owned(),
            })
            .await
            .unwrap();
        foreground_ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .unwrap();
        assert!(matches!(
            coordinator.wait(&caller, child.id()).await,
            Err(crate::subagents::WaitError::Cancelled { .. })
        ));
        shutdown_controller(commands, task).await;
        trace_owner.shutdown().expect("trace shutdown");

        let trace = fs::read_to_string(&trace_file).expect("trace file");
        let events = trace
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .collect::<Vec<_>>();
        let commands = events
            .iter()
            .filter_map(|event| {
                let name = event["event"].as_str()?;
                matches!(
                    name,
                    "root_run_started"
                        | "root_run_settled"
                        | "runtime_command_received"
                        | "foreground_cancellation_requested"
                        | "background_cancellation_requested"
                        | "child_stop_requested"
                        | "background_cancellation_returned"
                )
                .then_some((name, event["command"].as_str()))
            })
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            [
                ("root_run_started", None),
                ("runtime_command_received", Some("cancel")),
                ("foreground_cancellation_requested", None),
                ("root_run_settled", None),
                ("runtime_command_received", Some("cancel_background")),
                ("background_cancellation_requested", None),
                ("child_stop_requested", None),
                ("background_cancellation_returned", None),
                ("runtime_command_received", Some("shutdown")),
                ("background_cancellation_requested", None),
                ("background_cancellation_returned", None),
            ]
        );
        assert_eq!(
            events[events
                .iter()
                .position(|event| event["event"] == "root_run_settled")
                .unwrap()]["outcome"],
            "cancelled"
        );
        assert_eq!(
            events[events
                .iter()
                .position(|event| event["event"] == "child_stop_requested")
                .unwrap()]["cause"],
            "cancelled"
        );
        let child_returned = events
            .iter()
            .position(|event| event["event"] == "child_runner_returned")
            .unwrap();
        let child_settled = events
            .iter()
            .position(|event| event["event"] == "child_settled")
            .unwrap();
        assert!(child_returned < child_settled);
        for sensitive in ["child-request-SECRET", "foreground-request-SECRET"] {
            assert!(!trace.contains(sensitive), "trace leaked {sensitive}");
        }
    }

    #[tokio::test]
    async fn shutdown_waits_for_blocked_child_cleanup() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = CleanupGatedChildFactory {
            started: Arc::new(Notify::new()),
            cleanup_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            returned: Arc::new(Notify::new()),
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        let controller =
            controller_with_subagents(&workspace, ScriptedModel::default(), coordinator.clone())
                .await;
        let (commands, mut events, _, mut task) = start_controller(controller);

        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        factory.cleanup_started.notified().await;
        assert!(
            timeout(Duration::from_millis(50), &mut task).await.is_err(),
            "shutdown returned before child cleanup"
        );
        factory.release.notify_one();
        let records = timeout(TEST_TIMEOUT, task)
            .await
            .expect("shutdown should finish after child cleanup")
            .expect("controller should not panic");
        assert!(records.is_empty());
        let error = coordinator.wait(&caller, child.id()).await.unwrap_err();
        assert_eq!(error.observation().unwrap().state(), TaskState::Cancelled);
        assert_eq!(error.observation().unwrap().step_count(), 1);
        assert!(matches!(
            error,
            crate::subagents::WaitError::Cancelled { .. }
        ));
        assert_eq!(
            coordinator.records().await[0].attempts(),
            &[AttemptAccounting::UsageUnavailable]
        );
        assert!(matches!(
            events.try_recv(),
            Ok(RuntimeEvent::SubagentRecord { .. })
        ));
        while !matches!(events.try_recv(), Ok(RuntimeEvent::SubagentFinished { .. })) {}
    }

    #[tokio::test]
    async fn disabled_runtime_cancels_background_children_via_command() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = CleanupGatedChildFactory {
            started: Arc::new(Notify::new()),
            cleanup_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            returned: Arc::new(Notify::new()),
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;

        let calls = Arc::new(AtomicUsize::new(0));
        let controller = controller_with_subagents(
            &workspace,
            AuthenticationFailureModel {
                calls: calls.clone(),
            },
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _, mut task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "disable runtime".to_owned(),
            })
            .await
            .unwrap();
        let disabled_events = events_until_settled(&mut events).await;
        assert!(matches!(
            disabled_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { request, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FailedRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::OpenAiAuthentication,
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ] if request == "disable runtime"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            coordinator.get(&caller, child.id()).await.unwrap().state(),
            TaskState::Running
        );
        assert_eq!(
            coordinator
                .get(&caller, child.id())
                .await
                .unwrap()
                .step_count(),
            1
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "rejected while disabled".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::OpenAiAuthentication,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .unwrap();
        factory.cleanup_started.notified().await;
        assert_eq!(
            coordinator.get(&caller, child.id()).await.unwrap().state(),
            TaskState::Stopping
        );
        factory.release.notify_one();
        let error = coordinator.wait(&caller, child.id()).await.unwrap_err();
        assert_eq!(error.observation().unwrap().state(), TaskState::Cancelled);
        assert_eq!(error.observation().unwrap().step_count(), 1);
        assert!(matches!(
            error,
            crate::subagents::WaitError::Cancelled { .. }
        ));
        let child_records = coordinator.records().await;
        assert_eq!(child_records.len(), 1);
        assert_eq!(
            child_records[0].attempts(),
            &[AttemptAccounting::UsageUnavailable]
        );
        assert!(
            timeout(Duration::from_millis(50), &mut task).await.is_err(),
            "disabled controller stopped before shutdown"
        );
        let records = shutdown_controller(commands, task).await;
        assert_eq!(records.len(), 2);
        let mut relayed_finish = false;
        while let Ok(event) = events.try_recv() {
            relayed_finish |= matches!(event, RuntimeEvent::SubagentFinished { record } if record.id() == child.id());
        }
        assert!(
            relayed_finish,
            "disabled runtime must relay child settlement"
        );
    }

    #[test]
    fn restored_compaction_is_a_record_without_terminal_settlement() {
        let outcome = CompactionOutcome::Cancelled {
            reason: CompactionReason::Manual,
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let restored = restored_stored_record(RestoredRecord {
            revision: 7,
            run_id: 3,
            kind: RestoredRecordKind::Compaction {
                outcome: outcome.clone(),
            },
        });
        assert_eq!(restored.run_id, RunId(3));
        assert_eq!(restored.record, Record::Compaction { outcome });
    }

    #[test]
    fn mutation_evidence_labels_are_path_free() {
        assert_eq!(
            tool_call_label(ToolCall {
                tool: ToolName::Write,
                summary: ToolCallSummary::Created,
            }),
            "write:created"
        );
        assert_eq!(
            tool_call_label(ToolCall {
                tool: ToolName::Write,
                summary: ToolCallSummary::Replaced,
            }),
            "write:replaced"
        );
        assert_eq!(
            tool_call_label(ToolCall {
                tool: ToolName::Edit,
                summary: ToolCallSummary::Applied,
            }),
            "edit:applied"
        );
    }

    #[test]
    fn command_evidence_labels_are_path_free() {
        for (summary, expected) in [
            (ToolCallSummary::Pending, "run:pending"),
            (ToolCallSummary::Exit(7), "run:exit 7"),
            (ToolCallSummary::Signal, "run:signal"),
            (ToolCallSummary::Failed, "run:failed"),
        ] {
            assert_eq!(
                tool_call_label(ToolCall {
                    tool: ToolName::Run,
                    summary,
                }),
                expected
            );
        }
    }

    fn record_label(record: &StoredRecord) -> String {
        match &record.record {
            Record::UserRequest { request, .. } => {
                format!("record:{}:user:{request}", record.run_id.0)
            }
            Record::CompletedExecution { step_id, .. } => {
                format!("record:{}:execution:{}", record.run_id.0, step_id.0)
            }
            Record::FinalMessage { step_id, text, .. } => {
                format!("record:{}:message:{}:{text}", record.run_id.0, step_id.0)
            }
            Record::CancelledRun { .. } => format!("record:{}:cancelled", record.run_id.0),
            Record::FailedRun { message, .. } => {
                format!("record:{}:failed:{message}", record.run_id.0)
            }
            Record::Compaction { .. } => format!("record:{}:compaction", record.run_id.0),
        }
    }

    fn committed_records(events: &[RuntimeEvent]) -> Vec<StoredRecord> {
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::RecordAppended { record, .. } => Some(record.clone()),
                _ => None,
            })
            .collect()
    }

    fn completed_execution(
        records: &[StoredRecord],
        run_id: RunId,
        step_id: ModelStepId,
    ) -> &Execution {
        records
            .iter()
            .find_map(|stored| match stored {
                StoredRecord {
                    run_id: actual_run_id,
                    record:
                        Record::CompletedExecution {
                            step_id: actual_step_id,
                            execution,
                            ..
                        },
                } if *actual_run_id == run_id && *actual_step_id == step_id => Some(execution),
                _ => None,
            })
            .expect("completed execution record")
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

    fn progress_event(text: String) -> RuntimeEvent {
        RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk { text },
        }
    }

    struct GatedSelectionModel {
        first_snapshotted: Arc<Notify>,
        allow_first_step: Arc<Notify>,
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        step: u8,
    }

    impl Model for GatedSelectionModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.observed.lock().unwrap().push(selection);
                let action = match self.step {
                    0 => {
                        self.first_snapshotted.notify_one();
                        self.allow_first_step.notified().await;
                        CompletedModelAction::Code {
                            source: "value = 1".to_owned(),
                            call_id: ModelCallId::new("first"),
                            commentary: None,
                        }
                    }
                    _ => CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn settings_accepted_during_a_run_apply_to_the_next_run() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let first_snapshotted = Arc::new(Notify::new());
        let allow_first_step = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            GatedSelectionModel {
                first_snapshotted: first_snapshotted.clone(),
                allow_first_step: allow_first_step.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_selection(selection_a);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        first_snapshotted.notified().await;
        commands
            .send(RuntimeCommand::ApplySettings {
                selection: selection_b,
                revision: 1,
            })
            .await
            .unwrap();

        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );

        allow_first_step.notify_one();
        let settled = events_until_settled(&mut events).await;
        assert_eq!(*observed.lock().unwrap(), [selection_a, selection_a]);
        assert!(
            !settled
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SettingsAccepted { .. }))
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "follow up".to_owned(),
            })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            *observed.lock().unwrap(),
            [selection_a, selection_a, selection_b]
        );
        assert_eq!(records.len(), 5);
    }

    #[tokio::test]
    async fn a_run_request_record_keeps_the_selection_captured_at_submit() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let allow_first_step = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            GatedSelectionModel {
                first_snapshotted: Arc::new(Notify::new()),
                allow_first_step: allow_first_step.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_selection(selection_a);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::ApplySettings {
                selection: selection_b,
                revision: 1,
            })
            .await
            .unwrap();

        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { selection, .. },
                    ..
                },
                ..
            } if selection == Some(selection_a)
        ));

        allow_first_step.notify_one();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, task).await;
        assert_eq!(*observed.lock().unwrap(), [selection_a, selection_a]);
    }

    struct MidRunChildModel {
        first_started: Arc<Notify>,
        allow_first_step: Arc<Notify>,
        step: u8,
    }

    impl Model for MidRunChildModel {
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
                let action = match self.step {
                    0 => {
                        self.first_started.notify_one();
                        self.allow_first_step.notified().await;
                        CompletedModelAction::Code {
                            source: "value = 1".to_owned(),
                            call_id: ModelCallId::new("before-child"),
                            commentary: None,
                        }
                    }
                    1 => CompletedModelAction::Code {
                        source: "await start_subagent('child work')".to_owned(),
                        call_id: ModelCallId::new("start-child"),
                        commentary: None,
                    },
                    _ => CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn a_default_child_admitted_after_a_mid_run_settings_change_uses_the_run_selection() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let first_started = Arc::new(Notify::new());
        let allow_first_step = Arc::new(Notify::new());
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(CountingChildRunner(Arc::new(AtomicUsize::new(0)))),
        );
        let controller = controller_with_subagents(
            &workspace,
            MidRunChildModel {
                first_started: first_started.clone(),
                allow_first_step: allow_first_step.clone(),
                step: 0,
            },
            coordinator.clone(),
        )
        .await
        .with_selection(selection_a);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        first_started.notified().await;
        commands
            .send(RuntimeCommand::ApplySettings {
                selection: selection_b,
                revision: 1,
            })
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended { .. }
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        allow_first_step.notify_one();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, task).await;

        let records = coordinator.records().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].parameters().selection(), selection_a);
    }

    struct SelectionMessageModel {
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
    }

    impl Model for SelectionMessageModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.observed.lock().unwrap().push(selection);
                self.contexts
                    .lock()
                    .unwrap()
                    .push(history.entries().to_vec());
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn settings_ack_follows_preference_commit() {
        let home = tempfile::tempdir().expect("create session home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection_a = ModelSelection::new(&SOL, "low").expect("select Sol");
        let selection_b = ModelSelection::new(&TERRA, "high").expect("select Terra");
        let gate = SessionWriteGate::new(SessionWriteKind::Preferences);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection_a,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let metadata_before: (String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read initial metadata");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                SelectionMessageModel {
                    observed: observed.clone(),
                    contexts,
                },
            ),
            selection_a,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let accepted_thinking = controller.accepted_thinking.clone();
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::ApplySettings {
                selection: selection_b,
                revision: 3,
            })
            .await
            .expect("send settings");
        gate.wait_until_started().await;
        assert_eq!(
            accepted_thinking
                .resolve(selection_a, Some(&TERRA))
                .thinking(),
            TERRA.default_thinking
        );
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT preferences_json, updated_at_ms, revision FROM session",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read blocked metadata"),
            metadata_before
        );
        assert!(events.try_recv().is_err());

        commands
            .send(RuntimeCommand::Submit {
                request: "use saved settings".to_owned(),
            })
            .await
            .expect("queue submission");
        assert!(observed.lock().expect("observed selections").is_empty());

        gate.release();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 3 }
        );
        let settled = events_until_settled(&mut events).await;
        assert_eq!(
            accepted_thinking
                .resolve(selection_a, Some(&TERRA))
                .thinking(),
            "high"
        );
        assert_eq!(
            *observed.lock().expect("observed selections"),
            [selection_b]
        );
        assert!(matches!(
            settled.as_slice(),
            [
                RuntimeEvent::RecordAppended { .. },
                RuntimeEvent::RecordAppended { .. },
                RuntimeEvent::RunSettled { .. },
            ]
        ));
        let metadata_after: (String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read committed metadata");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&metadata_after.0).expect("decode metadata"),
            serde_json::json!({
                "selected_model": TERRA.id,
                "thinking": {
                    SOL.id: "low",
                    TERRA.id: "high",
                },
            })
        );
        assert!(metadata_after.1 >= metadata_before.1);
        assert!(metadata_after.2 > metadata_before.2);

        shutdown_controller(commands, task).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn name_acknowledgement_follows_the_name_commit() {
        let home = tempfile::tempdir().expect("create session home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(&SOL, "low").expect("select Sol");
        let gate = SessionWriteGate::new(SessionWriteKind::Name);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(arena, ScriptedModel::default()),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::SetSessionName {
                name: "Saved name".to_owned(),
                revision: 4,
            })
            .await
            .expect("set session name");
        gate.wait_until_started().await;
        assert_eq!(session.metadata().expect("read metadata").name(), None);
        assert!(events.try_recv().is_err());

        gate.release();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::NameAccepted {
                revision: 4,
                name: "Saved name".to_owned(),
            }
        );
        assert_eq!(
            session.metadata().expect("read metadata").name(),
            Some("Saved name")
        );
        shutdown_controller(commands, task).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn failed_name_commit_cancels_persisted_background_work_without_acknowledgement() {
        let home = tempfile::tempdir().expect("create session home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(&SOL, "medium").expect("select Sol");
        let factory = CleanupGatedChildFactory {
            started: Arc::new(Notify::new()),
            cleanup_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            returned: Arc::new(Notify::new()),
        };
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let snapshot = crate::config::ready_snapshot_for_test();
        let coordinator = complete_child_coordinator(factory.clone());
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(vec![
                    ModelAction::Code {
                        source: "task = await start_subagent('background work')".to_owned(),
                    },
                    ModelAction::Message {
                        text: "foreground complete".to_owned(),
                    },
                ]),
            ),
            selection,
            snapshot.settings().clone(),
            coordinator.clone(),
            session.writer(),
        );
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "start background work".to_owned(),
            })
            .await
            .expect("submit root request");
        factory.started.notified().await;
        let root_events = events_until_settled(&mut events).await;
        assert!(root_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::FinalMessage { text, .. },
                    ..
                },
                ..
            } if text == "foreground complete"
        )));
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT COUNT(*), COUNT(terminal_revision) FROM subagent_tasks",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read admitted child"),
            (1, 0)
        );
        let metadata_before: (Option<String>, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT name, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata before rejected name");
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch(
                "CREATE TRIGGER reject_name BEFORE UPDATE OF name ON session BEGIN SELECT RAISE(ABORT, 'reject name'); END;",
            )
            .expect("reject name writes");

        commands
            .send(RuntimeCommand::SetSessionName {
                name: "Unsaved name".to_owned(),
                revision: 1,
            })
            .await
            .expect("set name");
        let metadata_events = events_until_disabled(&mut events).await;
        factory.cleanup_started.notified().await;
        assert!(coordinator.storage_failure_token().is_cancelled());
        assert_eq!(coordinator.active_count().await, 0);
        let metadata_after: (Option<String>, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT name, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata after rejected name");
        assert_eq!(metadata_after, metadata_before);
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT COUNT(*), COUNT(terminal_revision) FROM subagent_tasks",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("read unfinished child after storage failure"),
            (1, 0)
        );
        assert!(
            !metadata_events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::NameAccepted { .. }))
        );
        assert_eq!(
            metadata_events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::RuntimeDisabled { .. }))
                .count(),
            1
        );

        factory.release.notify_one();
        factory.returned.notified().await;
        shutdown_controller(commands, task).await;
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT COUNT(*), COUNT(terminal_revision) FROM subagent_tasks",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("preserve unfinished child for recovery"),
            (1, 0)
        );
        drop(coordinator);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn failed_preference_commit_cancels_active_work_and_withholds_acknowledgement() {
        let home = tempfile::tempdir().expect("create session home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection_a = ModelSelection::new(&SOL, "low").expect("select Sol");
        let selection_b = ModelSelection::new(&TERRA, "high").expect("select Terra");
        let provider_called = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let closure_gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection_a,
            arena.checkpoint(),
            closure_gate.clone(),
        )
        .expect("create session");
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                AdmissionGatedModel {
                    calls: calls.clone(),
                    provider_called: provider_called.clone(),
                },
            ),
            selection_a,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let accepted_thinking = controller.accepted_thinking.clone();
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "keep running".to_owned(),
            })
            .await
            .expect("submit request");
        provider_called.notified().await;
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                },
                ..
            }
        ));
        let metadata_before: (String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata before rejected preferences");
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch(
                "CREATE TRIGGER reject_preferences BEFORE UPDATE OF preferences_json ON session BEGIN SELECT RAISE(ABORT, 'reject preferences'); END;",
            )
            .expect("reject preference writes");

        commands
            .send(RuntimeCommand::ApplySettings {
                selection: selection_b,
                revision: 1,
            })
            .await
            .expect("apply settings");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(
            accepted_thinking
                .resolve(selection_a, Some(&TERRA))
                .thinking(),
            TERRA.default_thinking
        );
        closure_gate.wait_until_started().await;
        let metadata_after: (String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata after rejected preferences");
        assert_eq!(metadata_after, metadata_before);

        commands
            .send(RuntimeCommand::Submit {
                request: "must be rejected".to_owned(),
            })
            .await
            .expect("submit disabled request");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        closure_gate.release();
        let settled = events_until_settled(&mut events).await;
        assert!(settled.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. },
                    ..
                },
                ..
            }
        )));
        shutdown_controller(commands, task).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn restore_waits_for_and_reports_the_effective_startup_preferences() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Preferences);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let saved = ModelSelection::new(&SOL, "low").expect("select Sol");
        let effective = ModelSelection::new(&TERRA, "high").expect("select Terra");
        let session = Arc::new(
            Session::create_with_write_gate(
                home.path(),
                workspace.path(),
                saved,
                arena.checkpoint(),
                gate.clone(),
            )
            .expect("create session"),
        );
        session
            .writer()
            .commit_name("Saved session".to_owned())
            .await
            .expect("save session name");
        let snapshot = crate::config::ready_snapshot_for_test();
        let restore_session = Arc::clone(&session);
        let selections = Arc::new(Mutex::new(Vec::new()));
        let model = ValidatedSelectionsModel {
            selections: selections.clone(),
        };
        let (returned, mut returned_receiver) = oneshot::channel();
        let restore = tokio::spawn(async move {
            let restored = RuntimeController::restore(
                &restore_session,
                arena,
                model,
                effective,
                snapshot.settings().clone(),
                SubagentCoordinator::new(
                    SubagentPolicy::from(snapshot.subagents()),
                    Arc::new(super::UnavailableChildRunner),
                ),
                None,
            )
            .await;
            let _ = returned.send(());
            restored
        });

        gate.wait_until_started().await;
        assert!(returned_receiver.try_recv().is_err());
        gate.release();
        let (controller, initial) = timeout(TEST_TIMEOUT, restore)
            .await
            .expect("restore should finish after preference commit")
            .expect("restore task should not panic")
            .expect("restore runtime");

        assert_eq!(initial.disabled_reason, None);
        assert_eq!(*selections.lock().unwrap(), vec![effective]);
        let metadata = session.metadata().expect("read committed metadata");
        assert_eq!(initial.metadata.as_ref(), Some(&metadata));
        assert_eq!(metadata.name(), Some("Saved session"));
        assert_eq!(metadata.preferences().selection(), effective);
        assert_eq!(metadata.preferences().thinking().get(SOL.id), Some(&"low"));
        assert_eq!(
            metadata.preferences().thinking().get(TERRA.id),
            Some(&"high")
        );
        drop(controller);
        Arc::try_unwrap(session)
            .unwrap_or_else(|_| panic!("restore should release session handles"))
            .close()
            .expect("close session");
    }

    #[tokio::test]
    async fn failed_startup_preference_commit_disables_the_restored_runtime() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(&SOL, "low").expect("select Sol");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        session
            .writer()
            .commit_request("readable saved request".to_owned(), selection)
            .await
            .expect("commit readable request");
        let saved_metadata = session.metadata().expect("read saved metadata");
        let metadata_before: (String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata before rejected startup preferences");
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch(
                "CREATE TRIGGER reject_startup_preferences BEFORE UPDATE OF preferences_json ON session BEGIN SELECT RAISE(ABORT, 'reject startup preferences'); END;",
            )
            .expect("reject startup preference writes");
        let effective = ModelSelection::new(&TERRA, "high").expect("select Terra");
        let snapshot = crate::config::ready_snapshot_for_test();
        let calls = Arc::new(AtomicUsize::new(0));

        let (controller, initial) = RuntimeController::restore(
            &session,
            arena,
            CountingMessageModel {
                calls: Arc::clone(&calls),
            },
            effective,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            None,
        )
        .await
        .expect("return disabled restored runtime");

        assert_eq!(
            initial.disabled_reason,
            Some(RuntimeDisabledReason::SessionStorageWriteFailed)
        );
        assert_eq!(
            controller.records,
            [StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "readable saved request".to_owned(),
                    selection: Some(selection),
                },
            }]
        );
        assert_eq!(initial.metadata.as_ref(), Some(&saved_metadata));
        let metadata_after: (String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata after rejected startup preferences");
        assert_eq!(metadata_after, metadata_before);
        let (commands, mut events, _progress, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "must stay disabled".to_owned(),
            })
            .await
            .expect("submit disabled request");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        shutdown_controller(commands, task).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn failed_startup_relocation_rolls_back_preferences_and_cwd() {
        let home = tempfile::tempdir().expect("create home");
        let saved_workspace = tempfile::tempdir().expect("create saved workspace");
        let relocated_workspace = tempfile::tempdir().expect("create relocated workspace");
        let saved_arena = Arena::new(
            worker_path(),
            ToolHost::new(saved_workspace.path())
                .await
                .expect("open saved tool host"),
        )
        .await
        .expect("create saved arena");
        let saved_selection = ModelSelection::new(&SOL, "low").expect("select Sol");
        let session = Session::create(
            home.path(),
            saved_workspace.path(),
            saved_selection,
            saved_arena.checkpoint(),
        )
        .expect("create session");
        drop(saved_arena);
        session
            .writer()
            .commit_request("readable saved request".to_owned(), saved_selection)
            .await
            .expect("commit readable request");
        let metadata_before: (String, String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, cwd, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read metadata before failed relocation");
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch(
                "CREATE TRIGGER reject_relocation BEFORE UPDATE OF cwd ON session BEGIN SELECT RAISE(ABORT, 'reject relocation'); END;",
            )
            .expect("reject relocation writes");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(relocated_workspace.path())
                .await
                .expect("open relocated tool host"),
        )
        .await
        .expect("create relocated arena");
        let effective = ModelSelection::new(&TERRA, "high").expect("select Terra");
        let snapshot = crate::config::ready_snapshot_for_test();
        let calls = Arc::new(AtomicUsize::new(0));

        let (controller, initial) = RuntimeController::restore(
            &session,
            arena,
            CountingMessageModel {
                calls: Arc::clone(&calls),
            },
            effective,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            Some(relocated_workspace.path().to_path_buf()),
        )
        .await
        .expect("return disabled restored runtime");

        assert_eq!(
            initial.disabled_reason,
            Some(RuntimeDisabledReason::SessionStorageWriteFailed)
        );
        assert!(matches!(
            controller.records.as_slice(),
            [StoredRecord {
                record: Record::UserRequest { request, .. },
                ..
            }] if request == "readable saved request"
        ));
        assert!(initial.metadata.is_some());
        let metadata_after: (String, String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, cwd, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read metadata after failed relocation");
        assert_eq!(metadata_after, metadata_before);
        let (commands, mut events, _progress, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "must stay disabled".to_owned(),
            })
            .await
            .expect("submit disabled request");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        shutdown_controller(commands, task).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        session.close().expect("close session");
    }

    #[derive(Clone)]
    enum RestoredModelAction {
        Code {
            source: String,
            commentary: Option<String>,
            attempts: Vec<AttemptAccounting>,
            cancel_before_execution: bool,
        },
        Message {
            text: String,
            attempts: Vec<AttemptAccounting>,
        },
    }

    struct RestoredStateModel {
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        calls: Arc<AtomicUsize>,
        actions: Vec<RestoredModelAction>,
        call_prefix: &'static str,
        runner_starts: Option<Arc<AtomicUsize>>,
        expected_runner_starts: Vec<usize>,
        step: usize,
    }

    impl Model for RestoredStateModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.observed.lock().unwrap().push(selection);
                self.contexts
                    .lock()
                    .unwrap()
                    .push(history.entries().to_vec());
                if let Some(starts) = &self.runner_starts {
                    assert_eq!(
                        starts.load(Ordering::SeqCst),
                        self.expected_runner_starts[self.step],
                        "unexpected child runner start before restored model step {}",
                        self.step
                    );
                }
                let (action, attempts) = match self.actions.get(self.step).cloned() {
                    Some(RestoredModelAction::Code {
                        source,
                        commentary,
                        attempts,
                        cancel_before_execution,
                    }) => {
                        if cancel_before_execution {
                            cancellation.cancel();
                        }
                        (
                            CompletedModelAction::Code {
                                source,
                                call_id: ModelCallId::new(format!(
                                    "{}-{}",
                                    self.call_prefix, self.step
                                )),
                                commentary,
                            },
                            attempts,
                        )
                    }
                    Some(RestoredModelAction::Message { text, attempts }) => {
                        (CompletedModelAction::Message { text }, attempts)
                    }
                    None => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new(
                        "fixture",
                        vec![OpaqueReplayItem::new(
                            format!("{} replay {}", self.call_prefix, self.step).into_bytes(),
                        )],
                    ),
                    attempts,
                })
            }
        }
    }

    struct CancelThenMessageSelectionModel {
        first_snapshotted: Arc<Notify>,
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        step: u8,
    }

    impl Model for CancelThenMessageSelectionModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            _history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.observed.lock().unwrap().push(selection);
                if self.step == 0 {
                    self.step = 1;
                    self.first_snapshotted.notify_one();
                    cancellation.cancelled().await;
                    return Err(ModelError::Cancelled.into());
                }
                self.step += 1;
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "later done".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn settings_acknowledgements_are_ordered_and_idle_updates_keep_only_the_latest_pair() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let selection_c = ModelSelection::new(&LUNA, "max").unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            SelectionMessageModel {
                observed: observed.clone(),
                contexts: contexts.clone(),
            },
        )
        .await
        .with_selection(selection_a);
        let (commands, mut events, _progress, task) = start_controller(controller);

        for (selection, revision) in [(selection_b, 7), (selection_c, 8)] {
            commands
                .send(RuntimeCommand::ApplySettings {
                    selection,
                    revision,
                })
                .await
                .unwrap();
        }
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 7 }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 8 }
        );
        assert!(events.try_recv().is_err());

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let settled = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(*observed.lock().unwrap(), [selection_c]);
        assert_eq!(
            *contexts.lock().unwrap(),
            [vec![ModelContextEntry::UserRequest {
                text: "inspect".to_owned(),
            }]]
        );
        assert!(
            settled
                .iter()
                .all(|event| !matches!(event, RuntimeEvent::SettingsAccepted { .. }))
        );
        assert!(matches!(
            records.as_slice(),
            [
                StoredRecord {
                    record: Record::UserRequest { request, .. },
                    ..
                },
                StoredRecord {
                    record: Record::FinalMessage { text, .. },
                    ..
                },
            ] if request == "inspect" && text == "done"
        ));
    }

    #[tokio::test]
    async fn active_settings_updates_keep_only_the_latest_complete_pair() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let selection_c = ModelSelection::new(&LUNA, "max").unwrap();
        let first_snapshotted = Arc::new(Notify::new());
        let allow_first_step = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            GatedSelectionModel {
                first_snapshotted: first_snapshotted.clone(),
                allow_first_step: allow_first_step.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_selection(selection_a);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        first_snapshotted.notified().await;
        for (selection, revision) in [(selection_b, 1), (selection_c, 2)] {
            commands
                .send(RuntimeCommand::ApplySettings {
                    selection,
                    revision,
                })
                .await
                .unwrap();
        }

        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended { .. }
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 2 }
        );
        allow_first_step.notify_one();
        events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::Submit {
                request: "follow up".to_owned(),
            })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, task).await;

        let observed = observed.lock().unwrap();
        assert_eq!(observed.as_slice(), [selection_a, selection_a, selection_c]);
        assert!(std::ptr::eq(observed[2].model(), &LUNA));
        assert_eq!(observed[2].thinking(), "max");
    }

    #[tokio::test]
    async fn active_settings_survive_cancellation_and_drive_a_later_run() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let first_snapshotted = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            CancelThenMessageSelectionModel {
                first_snapshotted: first_snapshotted.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_selection(selection_a);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        first_snapshotted.notified().await;
        commands
            .send(RuntimeCommand::ApplySettings {
                selection: selection_b,
                revision: 1,
            })
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended { .. }
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let cancelled = events_until_settled(&mut events).await;
        assert!(matches!(
            cancelled.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::CancelledRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(*observed.lock().unwrap(), [selection_a, selection_b]);
        assert!(matches!(
            records.as_slice(),
            [
                StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest { .. },
                },
                StoredRecord {
                    run_id: RunId(1),
                    record: Record::CancelledRun { .. },
                },
                StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest { .. },
                },
                StoredRecord {
                    run_id: RunId(2),
                    record: Record::FinalMessage { text, .. },
                },
            ] if text == "later done"
        ));
    }

    struct ProgressModel {
        actions: Vec<CompletedModelAction>,
        chunks_per_step: usize,
        next_action: usize,
    }

    impl ProgressModel {
        fn new(actions: Vec<CompletedModelAction>, chunks_per_step: usize) -> Self {
            Self {
                actions,
                chunks_per_step,
                next_action: 0,
            }
        }
    }

    impl Model for ProgressModel {
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
                for _ in 0..self.chunks_per_step {
                    progress(ModelProgress::AssistantTextChunk {
                        text: "x".to_owned(),
                    });
                }
                let action = self
                    .actions
                    .get(self.next_action)
                    .cloned()
                    .ok_or(ModelError::Exhausted)?;
                self.next_action += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    async fn progress_controller(
        workspace: &tempfile::TempDir,
        model: ProgressModel,
    ) -> RuntimeController<ProgressModel> {
        controller_with_model(workspace, model).await
    }

    struct WaitingCancellationModel {
        ready: Arc<Notify>,
        partial: bool,
    }

    struct CorrectionChildModel {
        ready: Arc<Notify>,
        step: usize,
    }

    impl Model for WaitingCancellationModel {
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
                if self.partial {
                    progress(ModelProgress::ReasoningSummaryChunk {
                        text: "thinking".to_owned(),
                    });
                    progress(ModelProgress::AssistantTextChunk {
                        text: "partial answer".to_owned(),
                    });
                    progress(ModelProgress::DecodedPythonChunk {
                        code: "print('partial')".to_owned(),
                    });
                }
                self.ready.notify_one();
                cancellation.cancelled().await;
                Err(ModelFailure::new(
                    ModelError::Cancelled,
                    vec![AttemptAccounting::UsageUnavailable],
                ))
            }
        }
    }

    impl Model for CorrectionChildModel {
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
                let action = match self.step {
                    0 => CompletedModelAction::Code {
                        source: "cancelled_task = await start_subagent('cancelled child')"
                            .to_owned(),
                        call_id: ModelCallId::new("cancelled-child"),
                        commentary: None,
                    },
                    1 => {
                        self.ready.notify_one();
                        cancellation.cancelled().await;
                        return Err(ModelError::Cancelled.into());
                    }
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    fn provider_failure(status: u16) -> ModelFailure {
        ModelFailure::new(
            ModelError::Provider(Box::new(ProviderFailure::new(
                Some(status),
                Some("provider_error".to_owned()),
                "provider request failed".to_owned(),
            ))),
            Vec::new(),
        )
    }

    struct UnavailableAttemptsModel;

    impl Model for UnavailableAttemptsModel {
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
                Err(ModelFailure::new(
                    ModelError::Provider(Box::new(ProviderFailure::new(
                        None,
                        None,
                        "provider transport failed".to_owned(),
                    ))),
                    vec![AttemptAccounting::UsageUnavailable; 3],
                ))
            }
        }
    }

    struct AuthenticationFailureModel {
        calls: Arc<AtomicUsize>,
    }

    impl Model for AuthenticationFailureModel {
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
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(provider_failure(401))
            }
        }
    }

    struct StatusThenMessageModel {
        status: u16,
        steps: u8,
    }

    impl Model for StatusThenMessageModel {
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
                if self.steps == 0 {
                    self.steps += 1;
                    return Err(provider_failure(self.status));
                }
                self.steps += 1;
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "later completed".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct OversizedProviderFailureModel;

    impl Model for OversizedProviderFailureModel {
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
                Err(ModelFailure::new(
                    ModelError::Provider(Box::new(ProviderFailure::new(
                        Some(500),
                        Some("oversize".to_owned()),
                        "é".repeat(2_048),
                    ))),
                    Vec::new(),
                ))
            }
        }
    }

    struct ClosureCommitProviderFailureModel {
        steps: u8,
    }

    impl Model for ClosureCommitProviderFailureModel {
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
                if self.steps == 0 {
                    self.steps += 1;
                    progress(ModelProgress::AttemptStarted { attempt: 1 });
                    progress(ModelProgress::ReasoningSummaryChunk {
                        text: "thinking".to_owned(),
                    });
                    progress(ModelProgress::AssistantTextChunk {
                        text: "partial answer".to_owned(),
                    });
                    progress(ModelProgress::DecodedPythonChunk {
                        code: "print('partial')".to_owned(),
                    });
                    return Err(ModelFailure::new(
                        ModelError::Provider(Box::new(ProviderFailure::new(
                            Some(401),
                            Some("provider_error".to_owned()),
                            "provider request failed".to_owned(),
                        ))),
                        vec![AttemptAccounting::UsageUnavailable],
                    ));
                }
                self.steps += 1;
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "later completed".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct AuthenticationAfterCancellationModel {
        ready: Arc<Notify>,
        steps: u8,
    }

    impl Model for AuthenticationAfterCancellationModel {
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
                if self.steps == 0 {
                    self.steps += 1;
                    self.ready.notify_one();
                    cancellation.cancelled().await;
                    return Err(provider_failure(401));
                }
                self.steps += 1;
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "later completed".to_owned(),
                    },
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct RecoveryFailureModel {
        calls: Arc<AtomicUsize>,
        source: String,
    }

    impl Model for RecoveryFailureModel {
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
                let action = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    CompletedModelAction::Code {
                        source: self.source.clone(),
                        call_id: ModelCallId::new("recovery-failure"),
                        commentary: None,
                    }
                } else {
                    CompletedModelAction::Message {
                        text: "uncertain arena was reused".to_owned(),
                    }
                };
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct AccountingModel {
        step: u8,
    }

    impl Model for AccountingModel {
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
                let action = match self.step {
                    0 => CompletedModelAction::Code {
                        source: "first = 1".to_owned(),
                        call_id: ModelCallId::new("first-accounting-code"),
                        commentary: None,
                    },
                    1 => CompletedModelAction::Code {
                        source: "second = first + 1".to_owned(),
                        call_id: ModelCallId::new("second-accounting-code"),
                        commentary: None,
                    },
                    2 => CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                })
            }
        }
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

    struct SummaryCodeModel {
        source: String,
        reasoning_summary: String,
        cancel_before_execution: bool,
    }

    impl Model for SummaryCodeModel {
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
                if self.cancel_before_execution {
                    cancellation.cancel();
                }
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

    struct CodeThenCancellationModel {
        first_request_step: u8,
        later_request_step: u8,
        ready: Arc<Notify>,
    }

    impl Model for CodeThenCancellationModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let requests = history
                    .entries()
                    .iter()
                    .filter(|entry| {
                        matches!(entry, crate::model::ModelContextEntry::UserRequest { .. })
                    })
                    .count();
                let (action, reasoning_summary) = match requests {
                    1 if self.first_request_step == 0 => {
                        self.first_request_step = 1;
                        (
                            CompletedModelAction::Code {
                                source: "value = 1".to_owned(),
                                call_id: ModelCallId::new("first-code"),
                                commentary: None,
                            },
                            "committed code summary".to_owned(),
                        )
                    }
                    1 => {
                        self.ready.notify_one();
                        cancellation.cancelled().await;
                        return Err(ModelError::Cancelled.into());
                    }
                    _ if self.later_request_step == 0 => {
                        self.later_request_step = 1;
                        (
                            CompletedModelAction::Code {
                                source: "value + 1".to_owned(),
                                call_id: ModelCallId::new("later-code"),
                                commentary: None,
                            },
                            String::new(),
                        )
                    }
                    _ => (
                        CompletedModelAction::Message {
                            text: "later done".to_owned(),
                        },
                        String::new(),
                    ),
                };
                Ok(ModelStep {
                    action,
                    reasoning_summary,
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                })
            }
        }
    }

    #[test]
    fn bounded_progress_publisher_enforces_payload_capacity_fifo_and_gaps() {
        assert_eq!(PROGRESS_CHANNEL_CAPACITY, 256);
        let (sender, mut receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut publisher = ProgressPublisher::new(sender);

        assert!(publisher.publish(progress_event("x".repeat(4 * 1024)), false, 4 * 1024,));
        assert!(!publisher.publish(
            progress_event("x".repeat(4 * 1024 + 1)),
            false,
            4 * 1024 + 1,
        ));
        let accepted = receiver
            .try_recv()
            .expect("4 KiB progress should be accepted");
        assert!(!accepted.gap_before);
        assert!(publisher.publish(progress_event("ok".to_owned()), false, 2));
        assert!(
            receiver
                .try_recv()
                .expect("post-rejection progress should be accepted")
                .gap_before
        );

        for index in 0..PROGRESS_CHANNEL_CAPACITY {
            assert!(publisher.publish(progress_event(index.to_string()), false, 3));
        }
        assert!(!publisher.publish(progress_event("dropped".to_owned()), false, 7));
        let first = receiver.try_recv().expect("first FIFO item");
        assert_eq!(
            first.item,
            progress_event("0".to_owned()),
            "accepted progress must remain FIFO"
        );
        assert!(publisher.publish(progress_event("recovered".to_owned()), false, 9));
        for index in 1..PROGRESS_CHANNEL_CAPACITY {
            assert_eq!(
                receiver.try_recv().expect("queued FIFO item").item,
                progress_event(index.to_string())
            );
        }
        let recovered = receiver.try_recv().expect("recovered progress");
        assert_eq!(recovered.item, progress_event("recovered".to_owned()));
        assert!(recovered.gap_before);
    }

    #[test]
    fn bounded_progress_forwards_upstream_and_local_gaps() {
        let (worker_sender, mut worker_receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut worker = ProgressPublisher::new(worker_sender);
        for _ in 0..PROGRESS_CHANNEL_CAPACITY {
            assert!(worker.publish(progress_event("x".to_owned()), false, 1));
        }
        assert!(!worker.publish(progress_event("lost".to_owned()), false, 4));
        worker_receiver.try_recv().expect("free one worker slot");
        assert!(worker.publish(progress_event("upstream".to_owned()), false, 8));
        while worker_receiver.len() > 1 {
            worker_receiver.try_recv().expect("drain worker progress");
        }
        let upstream = worker_receiver.try_recv().expect("upstream gap envelope");
        assert!(upstream.gap_before);

        let (tui_sender, mut tui_receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut runtime = ProgressPublisher::new(tui_sender);
        let bytes = runtime_event_payload_bytes(&upstream.item);
        assert!(runtime.publish(upstream.item, upstream.gap_before, bytes));
        assert!(
            tui_receiver
                .try_recv()
                .expect("runtime should forward upstream envelope")
                .gap_before
        );

        for _ in 0..PROGRESS_CHANNEL_CAPACITY {
            assert!(runtime.publish(progress_event("x".to_owned()), false, 1));
        }
        assert!(!runtime.publish(progress_event("lost".to_owned()), false, 4));
        tui_receiver.try_recv().expect("free one TUI slot");
        assert!(runtime.publish(progress_event("local".to_owned()), false, 5));
        while tui_receiver.len() > 1 {
            tui_receiver.try_recv().expect("drain TUI progress");
        }
        assert!(
            tui_receiver
                .try_recv()
                .expect("runtime local gap envelope")
                .gap_before
        );
    }

    #[test]
    fn bounded_progress_marks_a_gap_after_dropping_stdout_progress() {
        let (limit_sender, mut limit_receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut limit_publisher = ProgressPublisher::new(limit_sender);
        let allowed = RuntimeEvent::StdoutDelta {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            text: "x".repeat(4 * 1024),
        };
        assert!(limit_publisher.publish(
            allowed.clone(),
            false,
            runtime_event_payload_bytes(&allowed),
        ));
        let rejected = RuntimeEvent::StdoutDelta {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            text: "x".repeat(4 * 1024 + 1),
        };
        assert!(!limit_publisher.publish(
            rejected.clone(),
            false,
            runtime_event_payload_bytes(&rejected),
        ));
        assert_eq!(
            limit_receiver
                .try_recv()
                .expect("4 KiB stdout should be accepted")
                .item,
            allowed
        );

        let (sender, mut receiver) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut publisher = ProgressPublisher::new(sender);
        let stdout = || RuntimeEvent::StdoutDelta {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            text: "x".to_owned(),
        };

        for _ in 0..PROGRESS_CHANNEL_CAPACITY {
            assert!(publisher.publish(stdout(), false, 1));
        }
        assert!(!publisher.publish(stdout(), false, 1));
        receiver.try_recv().expect("free one bounded progress slot");
        assert!(publisher.publish(
            RuntimeEvent::StdoutTruncated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            },
            false,
            0,
        ));
        while receiver.len() > 1 {
            receiver.try_recv().expect("drain prior stdout progress");
        }

        let retained = receiver.try_recv().expect("retained truncation marker");
        assert!(retained.gap_before);
        assert!(matches!(
            retained.item,
            RuntimeEvent::StdoutTruncated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            }
        ));
    }

    #[tokio::test]
    async fn progress_forwards_stdout_with_run_and_step_identity_before_terminal_settlement() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, mut progress, controller) = start_controller(
            controller(
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
            .await,
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, controller).await;

        assert!(progress.iter().any(|envelope| matches!(
            &envelope.item,
            RuntimeEvent::StdoutDelta {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                text,
            } if text == "live stdout\n"
        )));
        assert!(
            !progress
                .iter()
                .any(|envelope| matches!(envelope.item, RuntimeEvent::StdoutTruncated { .. }))
        );
        assert!(matches!(
            records.as_slice(),
            [
                StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                },
                StoredRecord {
                    record: Record::CompletedExecution { .. },
                    ..
                },
                StoredRecord {
                    record: Record::FinalMessage { text, .. },
                    ..
                },
            ] if text == "done"
        ));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::RunSettled { run_id: RunId(1) })
        ));
    }

    #[tokio::test]
    async fn progress_forwards_active_run_previews_with_run_and_step_identity() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, mut progress, controller) = start_controller(
            controller(
                &workspace,
                vec![
                    ModelAction::Code {
                        source: "await run(\"printf 'preview stdout'\")".to_owned(),
                    },
                    ModelAction::Message {
                        text: "done".to_owned(),
                    },
                ],
            )
            .await,
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let progress = drain_progress(&mut progress);
        shutdown_controller(commands, controller).await;

        let settled_run_tool = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::ToolUpdated {
                        run_id: RunId(1),
                        step_id: ModelStepId(1),
                        call: ToolCall {
                            tool: ToolName::Run,
                            summary: ToolCallSummary::Exit(0),
                        },
                        ..
                    }
                )
            })
            .expect("reliable Run settlement");
        let completed_execution = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            run_id: RunId(1),
                            record: Record::CompletedExecution {
                                step_id: ModelStepId(1),
                                ..
                            },
                        },
                        ..
                    }
                )
            })
            .expect("durable completed execution");
        assert!(settled_run_tool < completed_execution);
        assert!(progress.iter().any(|envelope| matches!(
            &envelope.item,
            RuntimeEvent::RunPreviewUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                preview,
                ..
            } if preview.command == "printf 'preview stdout'"
                && preview.stdout == "preview stdout"
        )));
    }

    #[tokio::test]
    async fn progress_stdout_does_not_delay_cancellation_settlement_with_an_undrained_receiver() {
        let workspace = tempfile::tempdir().unwrap();
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
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            ScriptedModel::new(vec![ModelAction::Code {
                source: "print('before cancellation')\nawait read('blocked')".to_owned(),
            }]),
        ));
        let (commands, mut events, progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_read_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert!(
            !progress.is_empty(),
            "the receiver stayed deliberately undrained"
        );
        assert!(matches!(
            records.last(),
            Some(StoredRecord {
                record: Record::CancelledRun { terminal },
                ..
            }) if matches!(
                terminal.evidence,
                Some(TerminalEvidence::Execution(ref evidence))
                    if evidence.stdout.head == "before cancellation\n"
            )
        ));
        assert!(matches!(
            events.last(),
            Some(RuntimeEvent::RunSettled { run_id: RunId(1) })
        ));
    }

    #[tokio::test]
    async fn bounded_progress_keeps_execution_records_and_settlement_reliable() {
        let workspace = tempfile::tempdir().unwrap();
        let model = ProgressModel::new(
            vec![
                CompletedModelAction::Code {
                    source: "first = 1".to_owned(),
                    call_id: ModelCallId::new("first"),
                    commentary: None,
                },
                CompletedModelAction::Code {
                    source: "first + 1".to_owned(),
                    call_id: ModelCallId::new("second"),
                    commentary: None,
                },
                CompletedModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
            200,
        );
        let controller = progress_controller(&workspace, model).await;
        let (commands, command_receiver) = mpsc::channel(8);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, mut progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let controller =
            tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        commands
            .send(RuntimeCommand::Submit {
                request: "flood".to_owned(),
            })
            .await
            .unwrap();
        let reliable = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(progress.len(), PROGRESS_CHANNEL_CAPACITY);
        assert_eq!(
            reliable
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::ExecutionStarted { .. }))
                .count(),
            2
        );
        assert!(reliable.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { .. },
                    ..
                },
                ..
            }
        )));
        assert!(reliable.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::FinalMessage { text, .. },
                    ..
                },
                progress_gap: true,
            } if text == "done"
        )));
        assert!(matches!(
            reliable.last(),
            Some(RuntimeEvent::RunSettled { .. })
        ));
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record.record, Record::CompletedExecution { .. }))
                .count(),
            2
        );
        assert_eq!(
            drain_progress(&mut progress).len(),
            PROGRESS_CHANNEL_CAPACITY
        );
    }

    #[tokio::test]
    async fn reliable_execution_records_keep_tool_calls_when_lossy_progress_drops() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("input"), "one\ntwo\n").unwrap();
        let model = ProgressModel::new(
            vec![
                CompletedModelAction::Code {
                    source: "import asyncio\nawait asyncio.gather(read('input'), read('missing'))"
                        .to_owned(),
                    call_id: ModelCallId::new("read-input"),
                    commentary: None,
                },
                CompletedModelAction::Message {
                    text: "done".to_owned(),
                },
            ],
            PROGRESS_CHANNEL_CAPACITY + 1,
        );
        let (commands, mut events, mut progress, controller) =
            start_controller(progress_controller(&workspace, model).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let reliable = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        let execution = completed_execution(&records, RunId(1), ModelStepId(1));
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(8),
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Failed,
                },
            ]
        );
        assert!(reliable.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { execution, .. },
                    ..
                },
                ..
            } if execution.tool_calls == [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(8),
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Failed,
                },
            ]
        )));
        assert!(
            !drain_progress(&mut progress)
                .iter()
                .any(|envelope| matches!(envelope.item, RuntimeEvent::ToolUpdated { .. }))
        );
    }

    #[tokio::test]
    async fn bounded_progress_marks_an_unresolved_worker_gap_on_the_terminal_record() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let agent = controller
            .agent
            .take()
            .expect("idle controller should own its agent");
        let (_worker_sender, worker) = mpsc::unbounded_channel();
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        controller.active = Some(ActiveRun {
            operation: ForegroundOperation::Run,
            provider: controller.selection.model().provider(),
            run_id: Some(RunId(1)),
            request_entry: None,
            correction_requested: false,
            persisted_model_steps: BTreeSet::new(),
            pending_busy_submissions: 0,
            caller: controller.subagents.root_caller().await,
            cancellation: CancellationToken::new(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
        });
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut publisher = ProgressPublisher::new(progress_sender);

        controller
            .handle_worker_message(
                WorkerMessage::PersistedFinished {
                    agent,
                    result: Ok(crate::agent::PersistedRunOutcome {
                        run_id: 1,
                        terminal: PersistedTerminal::Cancelled {
                            evidence: None,
                            attempts: Vec::new(),
                        },
                        authentication_failure: false,
                        recovery_failure: false,
                    }),
                    progress_gap: true,
                },
                &event_sender,
                &mut publisher,
            )
            .await;

        assert!(matches!(
            events.try_recv(),
            Ok(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. },
                    ..
                },
                progress_gap: true,
            })
        ));
    }

    #[tokio::test]
    async fn bounded_progress_cancellation_and_shutdown_do_not_deadlock() {
        let workspace = tempfile::tempdir().unwrap();
        let model = ProgressModel::new(
            vec![CompletedModelAction::Code {
                source: "while True:\n    pass".to_owned(),
                call_id: ModelCallId::new("loop"),
                commentary: None,
            }],
            300,
        );
        let controller = progress_controller(&workspace, model).await;
        let (commands, command_receiver) = mpsc::channel(8);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let controller =
            tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel".to_owned(),
            })
            .await
            .unwrap();
        timeout(TEST_TIMEOUT, async {
            while progress.len() < PROGRESS_CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("undrained progress should fill without blocking the worker");
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown should settle with a full progress channel")
            .expect("controller task should not panic");

        assert!(matches!(
            records.last().map(|record| &record.record),
            Some(Record::CancelledRun { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn default_script_emits_ordered_events_and_records() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname = \"example\"\n",
        )
        .unwrap();
        let (commands, mut events, mut progress, controller) =
            start_controller(default_controller(&workspace).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:inspect",
                "execution:1:1",
                "record:1:execution:1",
                "execution:1:2",
                "record:1:execution:2",
                "record:1:message:3:I inspected Cargo.toml.",
                "settled:1",
            ]
        );
        assert_eq!(
            progress
                .iter()
                .map(|envelope| event_label(&envelope.item))
                .collect::<Vec<_>>(),
            [
                "run:1",
                "model:1:1",
                "execution:1:1",
                "tool-updated:1:1:read:pending",
                "tool-updated:1:1:read:27 bytes",
                "model:1:2",
                "execution:1:2",
                "stdout-delta",
                "model:1:3",
            ]
        );
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            [
                "record:1:user:inspect",
                "record:1:execution:1",
                "record:1:execution:2",
                "record:1:message:3:I inspected Cargo.toml.",
            ]
        );
        assert_eq!(records, committed_records(&events));
    }

    #[tokio::test]
    async fn exhausted_model_commits_one_failed_run_and_settles() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, _progress, controller) =
            start_controller(controller(&workspace, vec![]).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:inspect",
                "record:1:failed:scripted model exhausted before returning a final message",
                "settled:1",
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::FailedRun { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { .. },
                    ..
                },
                ..
            }
        )));
        assert_eq!(records, committed_records(&events));
    }

    #[tokio::test]
    async fn failed_unmeasured_attempts_survive_session_reopen() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller_with_model(&workspace, UnavailableAttemptsModel).await;
        let home = controller.test_home.take().expect("persisted test home");
        let session_id = controller.test_session.as_ref().unwrap().id().to_owned();
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;
        let [
            _,
            StoredRecord {
                record: Record::FailedRun { message, terminal },
                ..
            },
        ] = records.as_slice()
        else {
            panic!("expected failed Run after the request");
        };
        assert_eq!(
            message,
            "model provider failed: provider failure: provider transport failed"
        );
        assert_eq!(
            terminal.attempts,
            vec![AttemptAccounting::UsageUnavailable; 3]
        );
        assert_eq!(records, committed_records(&events));

        let session = Session::open(home.path(), &session_id).expect("reopen failed session");
        let restored = session.restore().expect("restore failed Run");
        assert!(restored.executable.is_some());
        let initial = restored_initial_state(&restored);
        assert_eq!(initial.disabled_reason, None);
        assert_eq!(
            initial.timeline,
            records
                .into_iter()
                .map(RestoredTimelineItem::Record)
                .collect::<Vec<_>>()
        );
        session.close().expect("close failed session");
    }

    #[tokio::test]
    async fn successful_requests_keep_arena_state_and_monotonic_ids() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let (commands, mut events, mut progress, controller) =
            start_controller(default_controller(&workspace).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let _first_events = events_until_settled(&mut events).await;
        let first_progress = drain_progress(&mut progress);
        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        let _second_events = events_until_settled(&mut events).await;
        let second_progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            first_progress
                .iter()
                .filter_map(|envelope| match &envelope.item {
                    RuntimeEvent::ModelStepStarted { run_id, step_id } => Some((*run_id, *step_id)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                (RunId(1), ModelStepId(1)),
                (RunId(1), ModelStepId(2)),
                (RunId(1), ModelStepId(3)),
            ]
        );
        assert_eq!(
            second_progress
                .iter()
                .filter_map(|envelope| match &envelope.item {
                    RuntimeEvent::ModelStepStarted { run_id, step_id } => Some((*run_id, *step_id)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                (RunId(2), ModelStepId(4)),
                (RunId(2), ModelStepId(5)),
                (RunId(2), ModelStepId(6)),
            ]
        );
        assert_eq!(
            result_head(completed_execution(&records, RunId(2), ModelStepId(5))),
            "2"
        );
    }

    #[tokio::test]
    async fn python_error_execution_is_committed_before_the_next_action() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, _progress, controller) = start_controller(
            controller(
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
            .await,
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "continue".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:continue",
                "execution:1:1",
                "record:1:execution:1",
                "execution:1:2",
                "record:1:execution:2",
                "record:1:message:3:done",
                "settled:1",
            ]
        );
        assert!(matches!(
            completed_execution(&records, RunId(1), ModelStepId(1)).outcome,
            ExecutionOutcome::PythonError(_)
        ));
        assert_eq!(
            result_head(completed_execution(&records, RunId(1), ModelStepId(2))),
            "2"
        );
    }

    #[tokio::test]
    async fn busy_submission_does_not_allocate_run_or_model_step_ids() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, mut progress, controller) = start_controller(
            controller(
                &workspace,
                vec![
                    ModelAction::Code {
                        source: "try:\n    attempts += 1\nexcept NameError:\n    attempts = 1"
                            .to_owned(),
                    },
                    ModelAction::Code {
                        source: "if attempts == 1:\n    while True:\n        pass\nattempts"
                            .to_owned(),
                    },
                    ModelAction::Message {
                        text: "done".to_owned(),
                    },
                ],
            )
            .await,
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let first_started = progress_until_execution_started(&mut progress, ModelStepId(2)).await;
        commands
            .send(RuntimeCommand::Submit {
                request: "rejected".to_owned(),
            })
            .await
            .unwrap();
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                ..
            }
        ));
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { .. },
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(2),
                ..
            }
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(RunId(1))
            }
        );
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let first_settled = events_until_settled(&mut events).await;

        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        let _second_events = events_until_settled(&mut events).await;
        let second_progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            first_started.iter().map(event_label).collect::<Vec<_>>(),
            [
                "run:1",
                "model:1:1",
                "execution:1:1",
                "model:1:2",
                "execution:1:2",
            ]
        );
        assert_eq!(
            first_settled.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:cancelled", "settled:1"]
        );
        assert_eq!(
            second_progress
                .iter()
                .filter_map(|envelope| match &envelope.item {
                    RuntimeEvent::RunStarted { run_id } => Some(*run_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [RunId(2)]
        );
        assert_eq!(
            second_progress
                .iter()
                .filter_map(|envelope| match &envelope.item {
                    RuntimeEvent::ModelStepStarted { run_id, step_id } => Some((*run_id, *step_id)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                (RunId(2), ModelStepId(3)),
                (RunId(2), ModelStepId(4)),
                (RunId(2), ModelStepId(5)),
            ]
        );
        assert_eq!(
            result_head(completed_execution(&records, RunId(2), ModelStepId(4))),
            "2"
        );
    }

    #[tokio::test]
    async fn undrained_events_do_not_block_cancellation_settlement() {
        let workspace = tempfile::tempdir().unwrap();
        let controller = controller(
            &workspace,
            vec![ModelAction::Code {
                source: "while True:\n    pass".to_owned(),
            }],
        )
        .await;
        let (commands, command_receiver) = mpsc::channel(8);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let controller =
            tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel".to_owned(),
            })
            .await
            .unwrap();
        wait_for_queued_events(&events, 1).await;
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("controller should settle despite undrained events")
            .expect("controller task should not panic");

        let mut drained = Vec::new();
        while let Some(event) = events.recv().await {
            drained.push(event);
        }
        assert_eq!(
            drained.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:user:cancel", "record:1:cancelled", "settled:1",]
        );
        assert_eq!(records, committed_records(&drained));
    }

    #[tokio::test]
    async fn cancellation_discards_the_active_execution_and_settles_once() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, mut progress, controller) = start_controller(
            controller(
                &workspace,
                vec![
                    ModelAction::Code {
                        source: "while True:\n    pass".to_owned(),
                    },
                    ModelAction::Message {
                        text: "unreachable".to_owned(),
                    },
                ],
            )
            .await,
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel".to_owned(),
            })
            .await
            .unwrap();
        let progress_events = progress_until_execution_started(&mut progress, ModelStepId(1)).await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let reliable_events = events_until_settled(&mut events).await;

        assert_eq!(
            progress_events.iter().map(event_label).collect::<Vec<_>>(),
            ["run:1", "model:1:1", "execution:1:1",]
        );
        assert_eq!(
            reliable_events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:cancel",
                "execution:1:1",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert!(
            timeout(Duration::from_millis(100), events.recv())
                .await
                .is_err()
        );
        let records = shutdown_controller(commands, controller).await;
        assert_eq!(
            records.iter().map(record_label).collect::<Vec<_>>(),
            ["record:1:user:cancel", "record:1:cancelled"]
        );
    }

    #[tokio::test]
    async fn a_later_request_succeeds_after_cancellation_recovery() {
        let workspace = tempfile::tempdir().unwrap();
        let actions = vec![
            ModelAction::Code {
                source: "try:\n    attempts += 1\nexcept NameError:\n    attempts = 1".to_owned(),
            },
            ModelAction::Code {
                source: "if attempts == 1:\n    while True:\n        pass\nattempts".to_owned(),
            },
            ModelAction::Message {
                text: "done".to_owned(),
            },
        ];
        let (commands, mut events, mut progress, controller) =
            start_controller(controller(&workspace, actions).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let first_started = progress_until_execution_started(&mut progress, ModelStepId(2)).await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let first_settled = events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        let _second_events = events_until_settled(&mut events).await;
        let second_progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            first_started.iter().map(event_label).collect::<Vec<_>>(),
            [
                "run:1",
                "model:1:1",
                "execution:1:1",
                "model:1:2",
                "execution:1:2",
            ]
        );
        assert_eq!(
            first_settled.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:first",
                "execution:1:1",
                "record:1:execution:1",
                "execution:1:2",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert_eq!(
            second_progress
                .iter()
                .filter_map(|envelope| match &envelope.item {
                    RuntimeEvent::ModelStepStarted { run_id, step_id } => Some((*run_id, *step_id)),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [
                (RunId(2), ModelStepId(3)),
                (RunId(2), ModelStepId(4)),
                (RunId(2), ModelStepId(5)),
            ]
        );
        assert_eq!(
            result_head(completed_execution(&records, RunId(2), ModelStepId(4))),
            "2"
        );
    }

    #[tokio::test]
    async fn shutdown_waits_for_cancellation_recovery_and_settlement() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, mut progress, controller) = start_controller(
            controller(
                &workspace,
                vec![ModelAction::Code {
                    source: "while True:\n    pass".to_owned(),
                }],
            )
            .await,
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "stop".to_owned(),
            })
            .await
            .unwrap();
        let progress_events = progress_until_execution_started(&mut progress, ModelStepId(1)).await;
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown should wait only for cancellation recovery")
            .expect("controller task should not panic");
        let reliable_events = events_until_settled(&mut events).await;

        assert_eq!(
            progress_events.iter().map(event_label).collect::<Vec<_>>(),
            ["run:1", "model:1:1", "execution:1:1",]
        );
        assert_eq!(
            reliable_events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:stop",
                "execution:1:1",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert_eq!(records, committed_records(&reliable_events));
    }

    #[tokio::test]
    async fn shutdown_releases_foreground_session_writer_before_session_close() {
        let home = tempfile::tempdir().expect("create session home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select default model");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let ready = Arc::new(Notify::new());
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                WaitingCancellationModel {
                    ready: ready.clone(),
                    partial: false,
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, _events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "stop".to_owned(),
            })
            .await
            .expect("submit request");
        ready.notified().await;
        commands
            .send(RuntimeCommand::Shutdown)
            .await
            .expect("request shutdown");
        timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown completes")
            .expect("controller task should not panic");

        session
            .close()
            .expect("shutdown releases foreground session writer");
    }

    #[tokio::test]
    async fn correction_restores_the_cancelled_request_parent_before_settlement() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let root = SessionEntryId(active_entry_id(&session) as u64);
        let ready = Arc::new(Notify::new());
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                WaitingCancellationModel {
                    ready: ready.clone(),
                    partial: true,
                },
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "original request".to_owned(),
            })
            .await
            .expect("submit original request");
        ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("cancel for correction");
        gate.wait_until_started().await;
        assert!(matches!(
            events.try_recv(),
            Ok(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                },
                ..
            })
        ));
        assert!(events.try_recv().is_err());
        gate.release();
        let first = events_until_settled(&mut events).await;
        let correction = first
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::CorrectionCompleted {
                    run_id,
                    outcome:
                        CorrectionOutcome::Replaced {
                            state,
                            request_prefill,
                        },
                } => Some((*run_id, state, request_prefill)),
                _ => None,
            })
            .expect("cancelled request should restore its parent");
        assert_eq!(correction.0, RunId(1));
        assert!(correction.1.timeline.is_empty());
        assert_eq!(correction.2.as_deref(), Some("original request"));
        let terminal = first
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::CancelledRun { .. },
                            ..
                        },
                        ..
                    }
                )
            })
            .expect("cancelled terminal record");
        let restored = first
            .iter()
            .position(|event| matches!(event, RuntimeEvent::CorrectionCompleted { .. }))
            .expect("correction outcome");
        let settled = first
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(1) }))
            .expect("run settlement");
        assert!(terminal < restored && restored < settled);
        assert_eq!(active_entry_id(&session), root.0 as i64);

        commands
            .send(RuntimeCommand::Submit {
                request: "corrected request".to_owned(),
            })
            .await
            .expect("submit corrected request");
        ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel corrected request");
        events_until_settled(&mut events).await;
        {
            let connection = session.connection_for_test().expect("lock session");
            let parents = connection
                .prepare("SELECT parent_id FROM entries WHERE kind = 'user_request' ORDER BY id")
                .expect("prepare request parent query")
                .query_map([], |row| row.get::<_, i64>(0))
                .expect("query request parents")
                .collect::<Result<Vec<_>, _>>()
                .expect("read request parents");
            assert_eq!(parents, [root.0 as i64, root.0 as i64]);
            assert_eq!(
                connection
                    .query_row(
                        "SELECT COUNT(*) FROM entries WHERE kind = 'cancelled'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("count cancelled terminals"),
                2
            );
        }
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_restores_after_python_execution_cancellation() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
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
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                ScriptedModel::new(vec![ModelAction::Code {
                    source: "print('before correction')\nawait read('blocked')".to_owned(),
                }]),
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "correct after execution".to_owned(),
            })
            .await
            .expect("submit request");
        gate.wait_until_read_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("cancel execution for correction");
        let settled = events_until_settled(&mut events).await;
        assert!(settled.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { terminal },
                    ..
                },
                ..
            } if matches!(
                terminal.evidence,
                Some(TerminalEvidence::Execution(ref evidence))
                    if evidence.stdout.head == "before correction\n"
            )
        )));
        assert!(settled.iter().any(|event| matches!(
            event,
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: CorrectionOutcome::Replaced { state, .. },
            } if state.timeline.is_empty()
        )));
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_restores_after_cancellation_precedes_request_admission() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Request);
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                WaitingCancellationModel {
                    ready: Arc::new(Notify::new()),
                    partial: false,
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "admit before correcting".to_owned(),
            })
            .await
            .expect("submit request");
        gate.wait_until_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("cancel before request admission");
        gate.release();
        let settled = events_until_settled(&mut events).await;
        assert!(
            session
                .writer()
                .model_history()
                .expect("read corrected context")
                .entries()
                .is_empty(),
            "correction must republish the request parent context"
        );
        assert!(matches!(
            settled.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { request, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::CancelledRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::CorrectionCompleted {
                    outcome:
                        CorrectionOutcome::Replaced {
                            request_prefill: Some(prefill),
                            ..
                        },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ] if request == "admit before correcting" && prefill == "admit before correcting"
        ));
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_intent_does_not_restore_final_or_failed_terminals() {
        for (terminal, expected) in [
            (
                PersistedTerminal::Final {
                    step_id: 1,
                    text: "final wins".to_owned(),
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                },
                "final",
            ),
            (
                PersistedTerminal::Failed {
                    message: "failed wins".to_owned(),
                    evidence: None,
                    attempts: Vec::new(),
                },
                "failed",
            ),
        ] {
            let workspace = tempfile::tempdir().expect("create workspace");
            let gate = ArenaRestoreTestGate::new();
            let arena = Arena::new(
                worker_path(),
                ToolHost::new(workspace.path())
                    .await
                    .expect("open tool host"),
            )
            .await
            .expect("create arena");
            let selection =
                ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
            let mut controller = RuntimeController::new_for_test(
                Agent::new(arena, ScriptedModel::new(Vec::new()))
                    .with_restore_test_gate(gate.clone()),
                selection,
                crate::config::ready_snapshot_for_test().settings().clone(),
                SubagentCoordinator::new(
                    SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                    Arc::new(super::UnavailableChildRunner),
                ),
            );
            let agent = controller.agent.take().expect("controller owns idle agent");
            let (_worker_sender, worker) = mpsc::unbounded_channel();
            let (_progress_sender, progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            controller.active = Some(ActiveRun {
                operation: ForegroundOperation::Run,
                provider: controller.selection.model().provider(),
                run_id: Some(RunId(1)),
                request_entry: Some(SessionEntryId(1)),
                correction_requested: false,
                persisted_model_steps: BTreeSet::new(),
                pending_busy_submissions: 0,
                caller: controller.subagents.root_caller().await,
                cancellation: CancellationToken::new(),
                worker,
                progress,
                progress_open: false,
                cancellation_requested: false,
            });
            controller.request_active_cancellation(true);
            assert!(
                controller
                    .active
                    .as_ref()
                    .expect("foreground run remains active")
                    .correction_requested
            );
            let (events_sender, mut events) = mpsc::unbounded_channel();
            let (progress_sender, _) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            let mut progress = ProgressPublisher::new(progress_sender);

            controller
                .handle_worker_message(
                    WorkerMessage::PersistedFinished {
                        agent,
                        result: Ok(crate::agent::PersistedRunOutcome {
                            run_id: 1,
                            terminal,
                            authentication_failure: false,
                            recovery_failure: false,
                        }),
                        progress_gap: false,
                    },
                    &events_sender,
                    &mut progress,
                )
                .await;

            let emitted = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
            assert!(matches!(
                emitted.as_slice(),
                [
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord { record, .. },
                        ..
                    },
                    RuntimeEvent::RunSettled { run_id: RunId(1) },
                ] if matches!(
                    (expected, record),
                    ("final", Record::FinalMessage { .. }) | ("failed", Record::FailedRun { .. })
                )
            ));
            assert!(gate.attempts().is_empty());
            assert!(controller.active.is_none());
        }
    }

    async fn correction_restore_fixture(
        workspace: &tempfile::TempDir,
        gate: ArenaRestoreTestGate,
    ) -> (
        tempfile::TempDir,
        Session,
        SessionEntryId,
        Checkpoint,
        Arc<Notify>,
        RuntimeController<WaitingCancellationModel>,
    ) {
        let (home, session, live_arena, _root, old_entry, _target_entry) =
            navigation_fixture(workspace).await;
        let old_checkpoint = live_arena.checkpoint();
        let ready = Arc::new(Notify::new());
        let controller = navigation_controller(
            &session,
            live_arena,
            WaitingCancellationModel {
                ready: ready.clone(),
                partial: false,
            },
            gate,
        );
        (home, session, old_entry, old_checkpoint, ready, controller)
    }

    async fn submit_correction(commands: &mpsc::Sender<RuntimeCommand>, ready: &Arc<Notify>) {
        commands
            .send(RuntimeCommand::Submit {
                request: "correct this request".to_owned(),
            })
            .await
            .expect("submit correction request");
        ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("request correction");
    }

    #[tokio::test]
    async fn correction_incompatible_target_keeps_cancelled_selection_enabled() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = ArenaRestoreTestGate::new();
        let (_home, session, old_entry, _old_checkpoint, ready, controller) =
            correction_restore_fixture(&workspace, gate.clone()).await;
        replace_entry_checkpoint_identity(&session, old_entry, "incompatible correction runtime");
        let target_checkpoint = navigation_checkpoint(&session, old_entry).await;
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        let emitted = events_until_settled(&mut events).await;

        let completed = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::CorrectionCompleted {
                        run_id: RunId(3),
                        outcome: CorrectionOutcome::Failed,
                    }
                )
            })
            .expect("failed correction outcome");
        let settled = emitted
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(3) }))
            .expect("correction run settlement");
        assert!(completed < settled);
        assert!(
            !emitted
                .iter()
                .any(|event| matches!(event, RuntimeEvent::RuntimeDisabled { .. }))
        );
        assert_eq!(gate.attempts(), [target_checkpoint]);
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT kind FROM entries WHERE id = (SELECT active_entry_id FROM session)",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read active entry kind"),
            "cancelled"
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_recovers_from_target_load_failure_without_disabling() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = ArenaRestoreTestGate::new();
        let (_home, session, old_entry, old_checkpoint, ready, controller) =
            correction_restore_fixture(&workspace, gate.clone()).await;
        replace_entry_checkpoint_state(&session, old_entry, vec![0]);
        let target_checkpoint = navigation_checkpoint(&session, old_entry).await;
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        let emitted = events_until_settled(&mut events).await;

        let completed = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::CorrectionCompleted {
                        run_id: RunId(3),
                        outcome: CorrectionOutcome::Failed,
                    }
                )
            })
            .expect("failed correction outcome");
        let settled = emitted
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(3) }))
            .expect("correction run settlement");
        assert!(completed < settled);
        assert!(
            !emitted
                .iter()
                .any(|event| matches!(event, RuntimeEvent::RuntimeDisabled { .. }))
        );
        assert_eq!(gate.attempts(), [target_checkpoint, old_checkpoint]);
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT kind FROM entries WHERE id = (SELECT active_entry_id FROM session)",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read active entry kind"),
            "cancelled"
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_disables_when_target_and_compensation_restore_fail() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = ArenaRestoreTestGate::new();
        let (_home, session, old_entry, old_checkpoint, ready, controller) =
            correction_restore_fixture(&workspace, gate.clone()).await;
        replace_entry_checkpoint_state(&session, old_entry, vec![0]);
        let target_checkpoint = navigation_checkpoint(&session, old_entry).await;
        gate.fail_attempt(2);
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        let emitted = events_until_settled(&mut events).await;

        let completed = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::CorrectionCompleted {
                        run_id: RunId(3),
                        outcome: CorrectionOutcome::Failed,
                    }
                )
            })
            .expect("failed correction outcome");
        let disabled = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                    }
                )
            })
            .expect("Arena recovery disable");
        let settled = emitted
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(3) }))
            .expect("correction run settlement");
        assert!(completed < disabled && disabled < settled);
        assert_eq!(gate.attempts(), [target_checkpoint, old_checkpoint]);
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT kind FROM entries WHERE id = (SELECT active_entry_id FROM session)",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read active entry kind"),
            "cancelled"
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_selection_failure_compensates_arena_and_disables_storage() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = ArenaRestoreTestGate::new();
        let (_home, session, _old_entry, old_checkpoint, ready, controller) =
            correction_restore_fixture(&workspace, gate.clone()).await;
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch(
                "CREATE TRIGGER reject_correction_selection BEFORE UPDATE OF active_entry_id ON session WHEN (SELECT kind FROM entries WHERE id = OLD.active_entry_id) = 'cancelled' BEGIN SELECT RAISE(ABORT, 'reject correction selection'); END;",
            )
            .expect("reject correction selection");
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        let emitted = events_until_settled(&mut events).await;

        let completed = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::CorrectionCompleted {
                        run_id: RunId(3),
                        outcome: CorrectionOutcome::Failed,
                    }
                )
            })
            .expect("failed correction outcome");
        let disabled = emitted
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::SessionStorageWriteFailed,
                    }
                )
            })
            .expect("storage disable");
        let settled = emitted
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(3) }))
            .expect("correction run settlement");
        assert!(completed < disabled && disabled < settled);
        assert_eq!(gate.attempts(), [old_checkpoint.clone(), old_checkpoint]);
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT kind FROM entries WHERE id = (SELECT active_entry_id FROM session)",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read active entry kind"),
            "cancelled"
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_skips_active_background_work_without_restoring_later() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        let foreground_ready = Arc::new(Notify::new());
        let controller = controller_with_subagents(
            &workspace,
            WaitingCancellationModel {
                ready: foreground_ready.clone(),
                partial: false,
            },
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel but retain child".to_owned(),
            })
            .await
            .expect("submit foreground request");
        foreground_ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("request correction");
        let foreground = events_until_settled(&mut events).await;
        assert!(matches!(
            foreground.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::CancelledRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::CorrectionCompleted {
                    run_id: RunId(1),
                    outcome: CorrectionOutcome::SkippedBackground,
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));
        assert_eq!(
            coordinator
                .get(&caller, child.id())
                .await
                .expect("child remains accessible")
                .state(),
            TaskState::Running
        );

        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .expect("explicitly cancel background child");
        assert!(matches!(
            coordinator.wait(&caller, child.id()).await,
            Err(crate::subagents::WaitError::Cancelled { .. })
        ));
        let mut later = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            let finished = matches!(event, RuntimeEvent::SubagentFinished { .. });
            later.push(event);
            if finished {
                break;
            }
        }
        assert!(
            !later
                .iter()
                .any(|event| matches!(event, RuntimeEvent::CorrectionCompleted { .. }))
        );
        shutdown_controller(commands, controller).await;
    }

    #[tokio::test]
    async fn correction_skips_stopping_background_work_without_later_restoration() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let operation_gate = TerminalSettlementTestGate::new();
        let factory = CleanupGatedChildFactory {
            started: Arc::new(Notify::new()),
            cleanup_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            returned: Arc::new(Notify::new()),
        };
        let coordinator = complete_child_coordinator(factory.clone())
            .with_terminal_settlement_test_gate(operation_gate.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        coordinator.request_background_cancellation(None).await;
        factory.cleanup_started.notified().await;
        assert_eq!(
            coordinator
                .get(&caller, child.id())
                .await
                .expect("read stopping child")
                .state(),
            TaskState::Stopping
        );
        let ready = Arc::new(Notify::new());
        let controller = controller_with_subagents(
            &workspace,
            WaitingCancellationModel {
                ready: ready.clone(),
                partial: false,
            },
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        let foreground = events_until_settled(&mut events).await;
        assert_eq!(
            foreground
                .iter()
                .filter_map(|event| match event {
                    RuntimeEvent::CorrectionCompleted { outcome, .. } => Some(outcome.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [CorrectionOutcome::SkippedBackground]
        );

        factory.release.notify_one();
        operation_gate.wait_until_terminal_settled().await;
        operation_gate.allow_operation_drop();
        operation_gate.wait_until_operation_dropped().await;
        assert_eq!(coordinator.operation_count(), 0);
        let mut later = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            let finished = matches!(event, RuntimeEvent::SubagentFinished { .. });
            later.push(event);
            if finished {
                break;
            }
        }
        assert!(
            !later
                .iter()
                .any(|event| matches!(event, RuntimeEvent::CorrectionCompleted { .. }))
        );
        shutdown_controller(commands, controller).await;
    }

    #[tokio::test]
    async fn correction_skips_terminal_settlement_pending_background_work() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let operation_gate = TerminalSettlementTestGate::new();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: Vec::new(),
        };
        let coordinator = complete_child_coordinator(factory.clone())
            .with_terminal_settlement_test_gate(operation_gate.clone());
        let caller = coordinator.root_caller().await;
        coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        factory.release.notify_one();
        operation_gate.wait_until_terminal_settled().await;
        assert_eq!(coordinator.operation_count(), 1);
        let ready = Arc::new(Notify::new());
        let controller = controller_with_subagents(
            &workspace,
            WaitingCancellationModel {
                ready: ready.clone(),
                partial: false,
            },
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        let foreground = events_until_settled(&mut events).await;
        assert_eq!(
            foreground
                .iter()
                .filter_map(|event| match event {
                    RuntimeEvent::CorrectionCompleted { outcome, .. } => Some(outcome.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [CorrectionOutcome::SkippedBackground]
        );

        operation_gate.allow_operation_drop();
        operation_gate.wait_until_operation_dropped().await;
        assert_eq!(coordinator.operation_count(), 0);
        assert!(!foreground.iter().any(|event| matches!(
            event,
            RuntimeEvent::CorrectionCompleted {
                outcome: CorrectionOutcome::Replaced { .. },
                ..
            }
        )));
        shutdown_controller(commands, controller).await;
    }

    #[tokio::test]
    async fn correction_restores_after_repeated_escape_settles_background_work_first() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let closure_gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let operation_gate = TerminalSettlementTestGate::new();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: Vec::new(),
        };
        let coordinator = complete_child_coordinator(factory.clone())
            .with_terminal_settlement_test_gate(operation_gate.clone());
        let caller = coordinator.root_caller().await;
        coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit background child");
        factory.started.notified().await;
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            closure_gate.clone(),
        )
        .expect("create session");
        let ready = Arc::new(Notify::new());
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                WaitingCancellationModel {
                    ready: ready.clone(),
                    partial: false,
                },
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            coordinator.clone(),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        submit_correction(&commands, &ready).await;
        closure_gate.wait_until_started().await;
        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .expect("repeat Escape cancels background work");
        operation_gate.wait_until_terminal_settled().await;
        operation_gate.allow_operation_drop();
        operation_gate.wait_until_operation_dropped().await;
        assert_eq!(coordinator.operation_count(), 0);
        closure_gate.release();
        let foreground = events_until_settled(&mut events).await;
        assert!(foreground.iter().any(|event| matches!(
            event,
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: CorrectionOutcome::Replaced { .. },
            }
        )));
        assert!(!foreground.iter().any(|event| matches!(
            event,
            RuntimeEvent::CorrectionCompleted {
                outcome: CorrectionOutcome::SkippedBackground,
                ..
            }
        )));
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn correction_hides_cancelled_run_tasks_until_manual_return() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let operation_gate = TerminalSettlementTestGate::new();
        let child_attempt = known_usage(DEFAULT_MODEL, 7, 3, 0);
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![child_attempt.clone()],
        };
        let coordinator = complete_child_coordinator(factory.clone())
            .with_terminal_settlement_test_gate(operation_gate.clone());
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create(home.path(), workspace.path(), selection, arena.checkpoint())
            .expect("create session");
        let ready = Arc::new(Notify::new());
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                CorrectionChildModel {
                    ready: ready.clone(),
                    step: 0,
                },
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            coordinator.clone(),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel with a child".to_owned(),
            })
            .await
            .expect("submit request");
        timeout(TEST_TIMEOUT, factory.started.notified())
            .await
            .expect("child should start");
        factory.release.notify_one();
        timeout(TEST_TIMEOUT, operation_gate.wait_until_terminal_settled())
            .await
            .expect("child terminal should settle");
        operation_gate.allow_operation_drop();
        timeout(TEST_TIMEOUT, operation_gate.wait_until_operation_dropped())
            .await
            .expect("child operation should drop");
        timeout(TEST_TIMEOUT, ready.notified())
            .await
            .expect("foreground should wait for correction");
        commands
            .send(RuntimeCommand::Cancel { correction: true })
            .await
            .expect("request correction");
        let correction_events = events_until_settled(&mut events).await;
        let restored = correction_events
            .iter()
            .find_map(|event| match event {
                RuntimeEvent::CorrectionCompleted {
                    outcome: CorrectionOutcome::Replaced { state, .. },
                    ..
                } => Some(state),
                _ => None,
            })
            .expect("restore cancelled request parent");
        assert!(restored.footer_adjustment.contains(&child_attempt));
        let task_id = coordinator
            .records()
            .await
            .into_iter()
            .next()
            .expect("retain cancelled child")
            .id()
            .clone();
        let hidden_caller = coordinator.root_caller().await;
        assert!(matches!(
            coordinator.get(&hidden_caller, &task_id).await,
            Err(crate::subagents::AccessError::Denied(id)) if id == task_id
        ));
        let cancelled_leaf = SessionEntryId(
            u64::try_from(
                session
                    .connection_for_test()
                    .expect("lock session")
                    .query_row(
                        "SELECT terminal_entry_id FROM runs WHERE id = 1",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("read cancelled terminal"),
            )
            .expect("terminal entry fits u64"),
        );
        commands
            .send(RuntimeCommand::Navigate {
                entry: cancelled_leaf,
            })
            .await
            .expect("return to cancelled leaf");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced { .. }
        ));
        let returned_caller = coordinator.root_caller().await;
        assert_eq!(
            coordinator
                .get(&returned_caller, &task_id)
                .await
                .expect("cancelled child is visible again")
                .state(),
            TaskState::Completed
        );
        assert_eq!(coordinator.records().await.len(), 1);
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn terminal_cancellation_before_provider_output_has_no_generation_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let ready = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            WaitingCancellationModel {
                ready: ready.clone(),
                partial: false,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel before output".to_owned(),
            })
            .await
            .unwrap();
        ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:cancel before output",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        let Record::CancelledRun { terminal } = &records[1].record else {
            panic!("expected cancelled terminal record");
        };
        assert_eq!(terminal.evidence, None);
        assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
    }

    #[tokio::test]
    async fn terminal_cancellation_retains_partial_generation_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let ready = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            WaitingCancellationModel {
                ready: ready.clone(),
                partial: true,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel during generation".to_owned(),
            })
            .await
            .unwrap();
        ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let _events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        let Record::CancelledRun { terminal } = &records[1].record else {
            panic!("expected cancelled terminal record");
        };
        let Some(TerminalEvidence::Generation(evidence)) = &terminal.evidence else {
            panic!("expected generation evidence");
        };
        assert!(evidence.reasoning_summary.is_empty());
        assert_eq!(evidence.reasoning, "thinking");
        assert_eq!(evidence.assistant_text, "partial answer");
        assert_eq!(evidence.decoded_python, "print('partial')");
        assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
    }

    #[tokio::test]
    async fn terminal_cancellation_during_execution_retains_interrupted_evidence() {
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
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            SummaryCodeModel {
                source: "contents = await read('Cargo.toml')".to_owned(),
                reasoning_summary: "cancelled code summary".to_owned(),
                cancel_before_execution: false,
            },
        ));
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel execution".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_read_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let _events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        let Record::CancelledRun { terminal } = &records[1].record else {
            panic!("expected cancellation terminal record");
        };
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("expected interrupted execution evidence");
        };
        assert_eq!(evidence.source, "contents = await read('Cargo.toml')");
        assert_eq!(evidence.reasoning_summary, "cancelled code summary");
        assert!(evidence.began);
        assert_eq!(
            evidence.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            }]
        );
        assert!(evidence.cancellation_warning.is_some());
        assert_eq!(terminal.attempts, []);
    }

    #[tokio::test]
    async fn terminal_cancellation_after_completed_code_before_execution_retains_its_summary() {
        let workspace = tempfile::tempdir().unwrap();
        let controller = controller_with_model(
            &workspace,
            SummaryCodeModel {
                source: "1 + 1".to_owned(),
                reasoning_summary: "code summary".to_owned(),
                cancel_before_execution: true,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel before execution".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:cancel before execution",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        let Record::CancelledRun { terminal } = &records[1].record else {
            panic!("expected cancellation terminal record");
        };
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("expected interrupted execution evidence");
        };
        assert_eq!(evidence.reasoning_summary, "code summary");
        assert!(!evidence.began);
    }

    #[tokio::test]
    async fn terminal_arena_failure_retains_completed_code_summary() {
        let workspace = tempfile::tempdir().unwrap();
        let controller = controller_with_model(
            &workspace,
            SummaryCodeModel {
                source: "x = ' ' * (1 << 60)".to_owned(),
                reasoning_summary: "failed code summary".to_owned(),
                cancel_before_execution: false,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "fail execution".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::ExecutionStarted { .. },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FailedRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));
        let Record::FailedRun { terminal, .. } = &records[1].record else {
            panic!("expected failed terminal record");
        };
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("expected failed execution evidence");
        };
        assert_eq!(evidence.reasoning_summary, "failed code summary");
        assert!(matches!(
            evidence.failure,
            Some(ExecutionFailure::Arena { .. })
        ));
    }

    #[tokio::test]
    async fn command_closure_marks_and_cancels_the_active_run() {
        let workspace = tempfile::tempdir().unwrap();
        let ready = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            WaitingCancellationModel {
                ready: ready.clone(),
                partial: false,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "closed commands".to_owned(),
            })
            .await
            .unwrap();
        ready.notified().await;
        drop(commands);
        let events = events_until_settled(&mut events).await;
        let records = timeout(TEST_TIMEOUT, task)
            .await
            .expect("command closure should settle the active run")
            .expect("controller task should not panic");

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:closed commands",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert!(matches!(
            records.last(),
            Some(StoredRecord {
                record: Record::CancelledRun { .. },
                ..
            })
        ));
    }

    #[tokio::test]
    async fn completed_summaries_are_carried_by_reliable_records() {
        let workspace = tempfile::tempdir().unwrap();
        let controller = controller_with_model(&workspace, CompletedSummaryModel { step: 0 }).await;
        let (commands, mut events, mut progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:inspect",
                "execution:1:1",
                "record:1:execution:1",
                "record:1:message:2:done",
                "settled:1",
            ]
        );
        assert!(
            !progress
                .iter()
                .any(|envelope| matches!(envelope.item, RuntimeEvent::ModelProgress { .. }))
        );
        assert!(progress.iter().any(|envelope| matches!(
            &envelope.item,
            RuntimeEvent::ExecutionStarted {
                commentary: Some(commentary),
                ..
            } if commentary == "I’ll calculate this first."
        )));
        let [
            StoredRecord {
                record: Record::UserRequest { .. },
                ..
            },
            StoredRecord {
                record:
                    Record::CompletedExecution {
                        commentary: Some(commentary),
                        reasoning_summary: code_summary,
                        ..
                    },
                ..
            },
            StoredRecord {
                record:
                    Record::FinalMessage {
                        reasoning_summary: message_summary,
                        ..
                    },
                ..
            },
        ] = records.as_slice()
        else {
            panic!("expected user request, completed execution, and final message");
        };
        assert_eq!(commentary, "I’ll calculate this first.");
        assert_eq!(code_summary, "code summary");
        assert_eq!(message_summary, "message summary");
    }

    #[tokio::test]
    async fn terminal_records_scope_cumulative_attempts_to_the_residual_step() {
        let workspace = tempfile::tempdir().unwrap();
        let controller = controller_with_model(&workspace, AccountingModel { step: 0 }).await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "accounting".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:accounting",
                "execution:1:1",
                "record:1:execution:1",
                "execution:1:2",
                "record:1:execution:2",
                "record:1:message:3:done",
                "settled:1",
            ]
        );
        for record in &records[1..3] {
            let Record::CompletedExecution { attempts, .. } = &record.record else {
                panic!("expected completed execution record");
            };
            assert_eq!(attempts.as_slice(), [AttemptAccounting::UsageUnavailable]);
        }
        let Record::FinalMessage { terminal, .. } = &records[3].record else {
            panic!("expected final terminal record");
        };
        assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
        assert_eq!(
            records
                .iter()
                .filter(|record| {
                    matches!(
                        record.record,
                        Record::FinalMessage { .. }
                            | Record::CancelledRun { .. }
                            | Record::FailedRun { .. }
                    )
                })
                .count(),
            1
        );
        assert_eq!(records, committed_records(&events));
    }

    #[tokio::test]
    async fn terminal_cancellation_after_execution_commits_the_step_and_allows_a_later_run() {
        let workspace = tempfile::tempdir().unwrap();
        let ready = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            CodeThenCancellationModel {
                first_request_step: 0,
                later_request_step: 0,
                ready: ready.clone(),
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        ready.notified().await;
        let mut first_events = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            let completed = matches!(
                event,
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::CompletedExecution { .. },
                        ..
                    },
                    ..
                }
            );
            first_events.push(event);
            if completed {
                break;
            }
        }
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        first_events.extend(events_until_settled(&mut events).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        let later_events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            first_events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:first",
                "execution:1:1",
                "record:1:execution:1",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert!(matches!(
            later_events.last(),
            Some(RuntimeEvent::RunSettled { run_id: RunId(2) })
        ));
        let committed_execution = records
            .iter()
            .find_map(|record| match record {
                StoredRecord {
                    run_id: RunId(1),
                    record:
                        Record::CompletedExecution {
                            reasoning_summary, ..
                        },
                } => Some(reasoning_summary),
                _ => None,
            })
            .expect("first request should retain its completed summary");
        assert_eq!(committed_execution, "committed code summary");
        let Record::CompletedExecution { attempts, .. } = &records[1].record else {
            panic!("first request should retain its completed attempt");
        };
        assert_eq!(attempts.as_slice(), [AttemptAccounting::UsageUnavailable]);
        let Record::CancelledRun { terminal } = &records[2].record else {
            panic!("first request should end with cancellation");
        };
        assert!(terminal.evidence.is_none());
        assert!(terminal.attempts.is_empty());
        let later_execution = records
            .iter()
            .find_map(|record| match record {
                StoredRecord {
                    run_id: RunId(2),
                    record: Record::CompletedExecution { execution, .. },
                } => Some(execution),
                _ => None,
            })
            .expect("later request should execute against the committed arena state");
        assert_eq!(result_head(later_execution), "2");
        assert!(matches!(
            records.last(),
            Some(StoredRecord {
                run_id: RunId(2),
                record: Record::FinalMessage { text, .. },
            }) if text == "later done"
        ));
    }

    #[tokio::test]
    async fn disabled_recovery_failure_after_cancellation_drops_the_uncertain_agent() {
        let workspace = tempfile::tempdir().unwrap();
        let gate = ReadTestGate::new();
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(gate.clone()),
        )
        .await
        .expect("create arena");
        arena.fail_next_recovery_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            RecoveryFailureModel {
                calls: calls.clone(),
                source: "print('before')\nawait read('blocked')".to_owned(),
            },
        ));
        let (commands, mut events, mut progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        gate.wait_until_read_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let first_events = events_until_settled(&mut events).await;
        assert!(matches!(
            first_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::ExecutionStarted { .. },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FailedRun { terminal, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ] if matches!(
                terminal.evidence,
                Some(TerminalEvidence::Execution(ref evidence))
                    if evidence.source == "print('before')\nawait read('blocked')"
                        && evidence.began
                        && evidence.stdout.head == "before\n"
                        && evidence.tool_calls == [ToolCall {
                            tool: ToolName::Read,
                            summary: ToolCallSummary::Pending,
                        }]
                        && evidence.cancellation_warning.is_some()
                        && matches!(
                            evidence.failure,
                            Some(ExecutionFailure::RecoveryFailed { .. })
                        )
            )
        ));
        drain_progress(&mut progress);

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            }
        );
        assert!(progress.try_recv().is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let records = shutdown_controller(commands, task).await;
        assert_eq!(records.len(), 2);
    }

    #[tokio::test]
    async fn disabled_recovery_failure_after_timeout_drops_the_uncertain_agent() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = Arena::with_limits(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
            Duration::from_millis(10),
            Some(64 * 1024 * 1024),
        )
        .await
        .expect("create arena");
        arena.fail_next_recovery_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            RecoveryFailureModel {
                calls: calls.clone(),
                source: "while True:\n    pass".to_owned(),
            },
        ));
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let terminal_events = events_until_settled(&mut events).await;
        let Record::FailedRun { terminal, .. } = &committed_records(&terminal_events)[1].record
        else {
            panic!("timed-out recovery failure must produce a failed run");
        };
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("timed-out recovery failure must retain execution evidence");
        };
        assert!(matches!(
            evidence.failure,
            Some(ExecutionFailure::RecoveryFailed { .. })
        ));
        assert!(matches!(
            terminal_events.as_slice(),
            [
                RuntimeEvent::RecordAppended { .. },
                RuntimeEvent::ExecutionStarted { .. },
                RuntimeEvent::RecordAppended { .. },
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(shutdown_controller(commands, task).await.len(), 2);
    }

    #[tokio::test]
    async fn disabled_recovery_failure_after_infrastructure_error_drops_the_uncertain_agent() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena");
        arena.fail_next_recovery_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            RecoveryFailureModel {
                calls: calls.clone(),
                source: "x = ' ' * (1 << 60)".to_owned(),
            },
        ));
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let terminal_events = events_until_settled(&mut events).await;
        let Record::FailedRun { terminal, .. } = &committed_records(&terminal_events)[1].record
        else {
            panic!("infrastructure recovery failure must produce a failed run");
        };
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("infrastructure recovery failure must retain execution evidence");
        };
        assert!(matches!(
            evidence.failure,
            Some(ExecutionFailure::RecoveryFailed { .. })
        ));
        assert!(matches!(
            terminal_events.as_slice(),
            [
                RuntimeEvent::RecordAppended { .. },
                RuntimeEvent::ExecutionStarted { .. },
                RuntimeEvent::RecordAppended { .. },
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(shutdown_controller(commands, task).await.len(), 2);
    }

    #[tokio::test]
    async fn closure_commit_recovery_failure_overrides_accepted_cancellation() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let read_gate = ReadTestGate::new();
        let closure_gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(read_gate.clone()),
        )
        .await
        .expect("create arena");
        arena.fail_next_recovery_for_test();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            closure_gate.clone(),
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let snapshot = crate::config::ready_snapshot_for_test();
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                RecoveryFailureModel {
                    calls: calls.clone(),
                    source: "print('before')\nawait read('blocked')".to_owned(),
                },
            ),
            selection,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                Arc::new(super::UnavailableChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        read_gate.wait_until_read_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        closure_gate.wait_until_started().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .unwrap();
        let mut events_before_release = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            if event == (RuntimeEvent::SettingsAccepted { revision: 1 }) {
                break;
            }
            events_before_release.push(event);
        }
        closure_gate.release();
        events_before_release.extend(events_until_settled(&mut events).await);

        let failed = events_before_release
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::FailedRun { .. },
                            ..
                        },
                        ..
                    }
                )
            })
            .expect("failed terminal record");
        let disabled = events_before_release
            .iter()
            .position(|event| {
                matches!(
                    event,
                    RuntimeEvent::RuntimeDisabled {
                        reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                    }
                )
            })
            .expect("recovery failure should disable the runtime");
        let settled = events_before_release
            .iter()
            .position(|event| matches!(event, RuntimeEvent::RunSettled { run_id: RunId(1) }))
            .expect("failed run settlement");
        assert!(failed < disabled && disabled < settled);
        assert!(!events_before_release.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CancelledRun { .. },
                    ..
                },
                ..
            }
        )));
        assert_eq!(
            events_before_release
                .iter()
                .filter(|event| matches!(
                    event,
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::FinalMessage { .. }
                                | Record::CancelledRun { .. }
                                | Record::FailedRun { .. },
                            ..
                        },
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            events_before_release
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::RunSettled { .. }))
                .count(),
            1
        );

        let records = shutdown_controller(commands, controller).await;
        let Some(StoredRecord {
            record: Record::FailedRun { terminal, .. },
            ..
        }) = records.last()
        else {
            panic!("recovery failure should override cancellation");
        };
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("failed recovery should retain execution evidence");
        };
        assert!(matches!(
            evidence.failure,
            Some(ExecutionFailure::RecoveryFailed { .. })
        ));

        let (cancelled, failed, terminal_entries, closed_runs, settled_steps, payload):
            (i64, i64, i64, i64, i64, String) = session
            .connection_for_test()
            .unwrap()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries WHERE kind = 'cancelled'), (SELECT COUNT(*) FROM entries WHERE kind = 'failed'), (SELECT COUNT(*) FROM entries WHERE kind IN ('cancelled', 'failed', 'final_reply')), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NOT NULL), (SELECT payload_json FROM entries WHERE kind = 'failed')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            (
                cancelled,
                failed,
                terminal_entries,
                closed_runs,
                settled_steps
            ),
            (0, 1, 1, 1, 1)
        );
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(
            payload["Terminal"]["terminal"]["Failed"]["evidence"]["Execution"]["failure"]
                ["RecoveryFailed"]
                .is_object()
        );
        session.close().unwrap();
    }

    #[tokio::test]
    async fn disabled_recovery_failure_overrides_cancellation_arriving_during_recovery() {
        let workspace = tempfile::tempdir().unwrap();
        let read_gate = ReadTestGate::new();
        let recovery_gate = RecoveryTestGate::new();
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(read_gate.clone()),
        )
        .await
        .expect("create arena")
        .with_recovery_test_gate(recovery_gate.clone());
        arena.fail_next_recovery_for_test();
        let calls = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            RecoveryFailureModel {
                calls: calls.clone(),
                source: "print('before')\nawait read('blocked')".to_owned(),
            },
        ));
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        read_gate.wait_until_read_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        recovery_gate.wait_until_recovery_started().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        tokio::task::yield_now().await;
        recovery_gate.resume_recovery();

        let events = events_until_settled(&mut events).await;
        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::ExecutionStarted { .. },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FailedRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        shutdown_controller(commands, task).await;
    }

    #[tokio::test]
    async fn disabled_authentication_does_not_apply_to_permission_or_server_failures() {
        for status in [403, 500] {
            let workspace = tempfile::tempdir().unwrap();
            let controller =
                controller_with_model(&workspace, StatusThenMessageModel { status, steps: 0 })
                    .await;
            let (commands, mut events, _progress, task) = start_controller(controller);

            commands
                .send(RuntimeCommand::Submit {
                    request: "first".to_owned(),
                })
                .await
                .unwrap();
            let failed_events = events_until_settled(&mut events).await;
            assert!(matches!(
                failed_events.as_slice(),
                [
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::UserRequest { .. },
                            ..
                        },
                        ..
                    },
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::FailedRun { .. },
                            ..
                        },
                        ..
                    },
                    RuntimeEvent::RunSettled { run_id: RunId(1) },
                ]
            ));

            commands
                .send(RuntimeCommand::Submit {
                    request: "later".to_owned(),
                })
                .await
                .unwrap();
            let later_events = events_until_settled(&mut events).await;
            assert!(matches!(
                later_events.as_slice(),
                [
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::UserRequest { .. },
                            ..
                        },
                        ..
                    },
                    RuntimeEvent::RecordAppended {
                        record: StoredRecord {
                            record: Record::FinalMessage { text, .. },
                            ..
                        },
                        ..
                    },
                    RuntimeEvent::RunSettled { run_id: RunId(2) },
                ] if text == "later completed"
            ));
            assert_eq!(shutdown_controller(commands, task).await.len(), 4);
        }
    }

    #[tokio::test]
    async fn disabled_cancellation_wins_over_a_queued_authentication_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let ready = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            AuthenticationAfterCancellationModel {
                ready: ready.clone(),
                steps: 0,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        ready.notified().await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .unwrap();
        let cancelled_events = events_until_settled(&mut events).await;
        assert!(matches!(
            cancelled_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::CancelledRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ]
        ));

        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        let later_events = events_until_settled(&mut events).await;
        assert!(matches!(
            later_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FinalMessage { text, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(2) },
            ] if text == "later completed"
        ));
        assert_eq!(shutdown_controller(commands, task).await.len(), 4);
    }

    #[tokio::test]
    async fn disabled_authentication_rejects_later_submission_without_starting_a_run() {
        let workspace = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let controller = controller_with_model(
            &workspace,
            AuthenticationFailureModel {
                calls: calls.clone(),
            },
        )
        .await;
        let (commands, mut events, mut progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let first_events = events_until_settled(&mut events).await;
        assert!(matches!(
            first_events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FailedRun { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RuntimeDisabled {
                    reason: RuntimeDisabledReason::OpenAiAuthentication,
                },
                RuntimeEvent::RunSettled { .. },
            ]
        ));

        drain_progress(&mut progress);
        commands
            .send(RuntimeCommand::Submit {
                request: "later".to_owned(),
            })
            .await
            .unwrap();
        let rejection = next_event(&mut events).await;

        assert_eq!(
            rejection,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::OpenAiAuthentication,
            }
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(progress.try_recv().is_err());
        let records = shutdown_controller(commands, task).await;
        assert_eq!(records.len(), 2);

        let restarted = controller_with_model(
            &workspace,
            StatusThenMessageModel {
                status: 500,
                steps: 1,
            },
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(restarted);
        commands
            .send(RuntimeCommand::Submit {
                request: "after restart".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        assert!(matches!(
            events.as_slice(),
            [
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::UserRequest { .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        record: Record::FinalMessage { text, .. },
                        ..
                    },
                    ..
                },
                RuntimeEvent::RunSettled { run_id: RunId(1) },
            ] if text == "later completed"
        ));
        assert_eq!(shutdown_controller(commands, task).await.len(), 2);
    }

    struct GatedAuthenticationFailureModel {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl Model for GatedAuthenticationFailureModel {
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
                self.started.notify_one();
                self.release.notified().await;
                Err(provider_failure(401))
            }
        }
    }

    async fn authentication_failure_after_mid_run_selection(
        run: ModelSelection,
        accepted_mid_run: ModelSelection,
    ) -> Vec<RuntimeEvent> {
        let workspace = tempfile::tempdir().unwrap();
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            GatedAuthenticationFailureModel {
                started: started.clone(),
                release: release.clone(),
            },
        )
        .await
        .with_selection(run);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        started.notified().await;
        commands
            .send(RuntimeCommand::ApplySettings {
                selection: accepted_mid_run,
                revision: 1,
            })
            .await
            .unwrap();
        while next_event(&mut events).await != (RuntimeEvent::SettingsAccepted { revision: 1 }) {}
        release.notify_one();
        let settled = events_until_settled(&mut events).await;
        shutdown_controller(commands, task).await;
        settled
    }

    #[tokio::test]
    async fn anthropic_authentication_failure_after_an_accepted_openai_selection_disables_as_anthropic()
     {
        let settled = authentication_failure_after_mid_run_selection(
            ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap(),
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
        )
        .await;

        let disabled = settled
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::RuntimeDisabled { reason } => Some(reason.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            disabled,
            [RuntimeDisabledReason::AnthropicAuthentication],
            "{settled:?}"
        );
    }

    #[tokio::test]
    async fn openai_authentication_failure_after_an_accepted_anthropic_selection_disables_as_openai()
     {
        let settled = authentication_failure_after_mid_run_selection(
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap(),
        )
        .await;

        let disabled = settled
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::RuntimeDisabled { reason } => Some(reason.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            disabled,
            [RuntimeDisabledReason::OpenAiAuthentication],
            "{settled:?}"
        );
    }

    #[tokio::test]
    async fn fixture_anthropic_authentication_failure_keeps_the_key_and_request_metadata_private() {
        const KEY: &str = "sk-ant-runtime-hostile-credential";
        const METADATA: &str = "req_runtime_hostile_metadata";
        let fixture = Fixture::start(FixtureReply::OwnedStatus {
            code: 401,
            headers: vec![("request-id".to_owned(), METADATA.to_owned())],
            body: serde_json::json!({
                "type": "error",
                "error": {
                    "type": "authentication_error",
                    "message": format!("invalid x-api-key {KEY}")
                },
                "request_id": METADATA
            })
            .to_string(),
        })
        .await;
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller_with_model(
            &workspace,
            AnthropicModel::with_endpoint("system prompt", KEY, &fixture.endpoint),
        )
        .await
        .with_selection(ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap());
        let home = controller.test_home.take().expect("persisted test home");
        let session_id = controller.test_session.as_ref().unwrap().id().to_owned();
        let (commands, mut events, mut progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let settled = events_until_settled(&mut events).await;
        let progress = drain_progress(&mut progress);
        let records = shutdown_controller(commands, task).await;

        assert_eq!(fixture.request_count(), 1);
        assert!(
            settled.contains(&RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::AnthropicAuthentication,
            }),
            "{settled:?}"
        );
        let [
            _,
            StoredRecord {
                record: Record::FailedRun { message, .. },
                ..
            },
        ] = records.as_slice()
        else {
            panic!("expected a failed Run after the request: {records:?}");
        };
        assert_eq!(
            message,
            "model provider failed: authentication failure (HTTP 401) [authentication_error]: invalid x-api-key [REDACTED]"
        );

        let mut transcript = Transcript::default();
        for event in settled.iter().cloned() {
            transcript.apply(event, false);
        }
        let layout = transcript.layout(1024);
        let height = u16::try_from(layout.total_height()).expect("test transcript height");
        let mut buffer =
            ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 1024, height));
        layout.render(buffer.area, 0, &mut buffer);
        let rendered = buffer
            .content
            .chunks(1024)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            transcript.status(),
            "Anthropic authentication failed; restart Kiki"
        );
        assert!(
            rendered.contains("invalid x-api-key [REDACTED]"),
            "{rendered}"
        );

        let observable = format!("{settled:?}\n{progress:?}\n{records:?}\n{rendered}");
        assert!(!observable.contains(KEY), "{observable}");
        assert!(!observable.contains(METADATA), "{observable}");

        let session = Session::open(home.path(), &session_id).expect("reopen failed session");
        let restored = session.restore().expect("restore failed Run");
        assert_eq!(
            restored_initial_state(&restored).timeline,
            records
                .into_iter()
                .map(RestoredTimelineItem::Record)
                .collect::<Vec<_>>()
        );
        session.close().expect("close failed session");
        for entry in fs::read_dir(home.path().join(".kiki/sessions")).unwrap() {
            let path = entry.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            for secret in [KEY, METADATA] {
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes()),
                    "{} persisted {secret}",
                    path.display()
                );
            }
        }
    }

    fn persisted_settings(
        home: &tempfile::TempDir,
        workspace: &Path,
    ) -> crate::config::settings::SettingsContext {
        let path = home.path().join(".kiki/config.toml");
        fs::create_dir_all(path.parent().expect("settings parent")).unwrap();
        fs::write(path, "[thinking]\n\"openai/gpt-5.6-terra\" = \"low\"\n").unwrap();
        crate::config::ready_snapshot_with_settings_for_test(Some(home.path()), workspace)
            .settings()
            .clone()
    }

    #[test]
    fn child_selection_prefers_accepted_target_thinking_then_persisted_then_default() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let invoking = ModelSelection::new(&SOL, "high").unwrap();
        let accepted =
            AcceptedThinking::new(invoking, persisted_settings(&home, workspace.path()), None);

        assert_eq!(accepted.resolve(invoking, None), invoking);
        assert_eq!(
            accepted.resolve(ModelSelection::new(&LUNA, "low").unwrap(), Some(&SOL)),
            invoking
        );
        assert_eq!(
            accepted.resolve(invoking, Some(&TERRA)),
            ModelSelection::new(&TERRA, "low").unwrap()
        );
        assert_eq!(
            accepted.resolve(invoking, Some(&LUNA)),
            ModelSelection::new(&LUNA, LUNA.default_thinking).unwrap()
        );

        let accepted_target = ModelSelection::new(&TERRA, "max").unwrap();
        accepted.accept(accepted_target);

        assert_eq!(accepted.resolve(invoking, Some(&TERRA)), accepted_target);
    }

    #[test]
    fn restored_accepted_thinking_overrides_current_settings_context() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let invoking = ModelSelection::new(&SOL, "high").unwrap();
        let project_settings = workspace.path().join(".kiki/config.toml");
        fs::create_dir_all(project_settings.parent().expect("project settings parent")).unwrap();
        fs::write(
            &project_settings,
            "[thinking]\n\"openai/gpt-5.6-terra\" = \"medium\"\n",
        )
        .unwrap();
        let settings = persisted_settings(&home, workspace.path());
        assert_eq!(settings.thinking_for(&TERRA), Some("medium"));
        let restored = SessionPreferences::new(
            invoking,
            BTreeMap::from([(SOL.id, "high"), (TERRA.id, "max")]),
        )
        .expect("valid restored preferences");
        let accepted = AcceptedThinking::new(invoking, settings, Some(restored));

        assert_eq!(
            accepted.resolve(invoking, Some(&TERRA)),
            ModelSelection::new(&TERRA, "max").unwrap()
        );
    }

    #[test]
    fn grandchild_without_an_override_inherits_its_invoking_child_selection() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = ModelSelection::new(&SOL, "low").unwrap();
        let child = ModelSelection::new(&LUNA, "max").unwrap();
        let accepted =
            AcceptedThinking::new(root, persisted_settings(&home, workspace.path()), None);

        assert_eq!(accepted.resolve(child, None), child);
    }

    #[tokio::test]
    async fn relayed_provider_set_rejects_keyless_subagent_models() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, _progress, controller) = start_controller(
            controller(
                &workspace,
                vec![
                    ModelAction::Code {
                        source: "try:\n    await start_subagent('child', 'anthropic/claude-opus-5-5')\n    outcome = 'started'\nexcept ValueError as error:\n    outcome = str(error)\noutcome".to_owned(),
                    },
                    ModelAction::Message {
                        text: "done".to_owned(),
                    },
                ],
            )
            .await
            .with_providers(vec![Provider::OpenAi]),
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "start a child".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        let outcome = result_head(completed_execution(&records, RunId(1), ModelStepId(1)));
        assert!(outcome.contains("ANTHROPIC_API_KEY"), "{outcome}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SubagentSpawned { .. }))
        );
    }

    #[tokio::test]
    async fn accepted_thinking_updates_before_its_settings_acknowledgement() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let mut controller = controller(&workspace, Vec::new())
            .await
            .with_selection(selection_a);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut progress = ProgressPublisher::new(progress_sender);
        let mut shutdown_requested = false;

        controller
            .handle_command(
                RuntimeCommand::ApplySettings {
                    selection: selection_b,
                    revision: 1,
                },
                &event_sender,
                &mut progress,
                &mut shutdown_requested,
            )
            .await;

        assert_eq!(
            controller.resolve_child_selection(selection_a, Some(&TERRA)),
            selection_b
        );
        assert_eq!(controller.selection, selection_b);
        assert_eq!(
            events.recv().await,
            Some(RuntimeEvent::SettingsAccepted { revision: 1 })
        );
    }

    #[tokio::test]
    async fn idle_controller_relays_queued_child_lifecycle_in_order() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let controller =
            controller_with_subagents(&workspace, ScriptedModel::default(), coordinator.clone())
                .await;
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit child");
        factory.started.notified().await;
        factory.release.notify_one();
        coordinator.wait(&caller, child.id()).await.unwrap();

        let (commands, mut events, _progress, task) = start_controller(controller);
        let mut lifecycle = Vec::new();
        loop {
            let event = next_event(&mut events).await;
            let finished = matches!(event, RuntimeEvent::SubagentFinished { .. });
            lifecycle.push(event);
            if finished {
                break;
            }
        }

        assert!(matches!(
            lifecycle.first(),
            Some(RuntimeEvent::SubagentSpawned { record }) if record.id() == child.id()
        ));
        assert!(lifecycle[1..lifecycle.len() - 1]
            .iter()
            .any(|event| matches!(event, RuntimeEvent::SubagentRecord { record } if record.id() == child.id())));
        assert!(matches!(
            lifecycle.last(),
            Some(RuntimeEvent::SubagentFinished { record }) if record.id() == child.id()
        ));
        assert_eq!(coordinator.active_count().await, 0);

        shutdown_controller(commands, task).await;
    }

    #[tokio::test]
    async fn child_records_relay_after_foreground_settlement() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![known_usage(&TERRA, 200, 20, 0)],
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let root_attempt = known_usage(&LUNA, 100, 10, 50);
        let controller = controller_with_subagents(
            &workspace,
            UsageMessageModel {
                attempts: Some(vec![root_attempt]),
            },
            coordinator.clone(),
        )
        .await;
        let (commands, mut events, _progress, task) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "foreground".to_owned(),
            })
            .await
            .unwrap();
        let foreground = events_until_settled(&mut events).await;
        assert!(matches!(
            foreground.last(),
            Some(RuntimeEvent::RunSettled { run_id: RunId(1) })
        ));
        let mut transcript = Transcript::default();
        for event in foreground {
            transcript.apply(event, false);
        }
        let before = transcript.layout(128).footer_rule(128).to_string();
        assert!(before.contains("↑100 ↓10 R50 W0"), "{before}");
        assert!(before.contains("$0.00002300"), "{before}");
        assert!(before.contains("CH50.0%"), "{before}");
        assert!(before.contains("0.0%/272k"), "{before}");

        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(
                &caller,
                ChildParameters::new(
                    "background work".to_owned(),
                    ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                    false,
                ),
                child_payload(&workspace).await,
            )
            .await
            .expect("admit child after root settlement");
        factory.started.notified().await;
        factory.release.notify_one();
        coordinator.wait(&caller, child.id()).await.unwrap();

        let mut first_child_event = true;
        loop {
            let event = next_event(&mut events).await;
            if first_child_event {
                assert!(matches!(
                    event,
                    RuntimeEvent::SubagentSpawned { ref record } if record.id() == child.id()
                ));
                first_child_event = false;
            }
            let finished = matches!(
                event,
                RuntimeEvent::SubagentFinished { ref record } if record.id() == child.id()
            );
            transcript.apply(event, false);
            if finished {
                break;
            }
        }
        let after = transcript.layout(128).footer_rule(128).to_string();
        assert!(after.contains("↑300 ↓30 R50 W0"), "{after}");
        assert!(after.contains("$0.0006630"), "{after}");
        assert!(after.contains("CH50.0%"), "{after}");
        assert!(after.contains("0.0%/272k"), "{after}");
        assert_ne!(after, before);
        shutdown_controller(commands, task).await;
    }

    #[tokio::test]
    async fn full_progress_drops_only_child_status_and_ignores_stale_revisions() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, mut statuses) = mpsc::channel(1);
        let mut progress = ProgressPublisher::new(progress_sender);
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let spawned = crate::subagents::TaskRecord::for_test(
            "c1",
            3,
            selection,
            None,
            1,
            vec![AttemptAccounting::UsageUnavailable],
        );
        let finished = crate::subagents::TaskRecord::for_test(
            "c1",
            5,
            selection,
            None,
            1,
            vec![AttemptAccounting::UsageUnavailable],
        );

        controller
            .handle_coordinator_event(
                CoordinatorEvent::Spawned {
                    record: spawned.clone(),
                },
                &event_sender,
                &mut progress,
            )
            .await;
        controller
            .handle_coordinator_event(
                CoordinatorEvent::Finished {
                    record: finished.clone(),
                },
                &event_sender,
                &mut progress,
            )
            .await;
        controller.publish_subagent_status(&mut progress, &spawned);

        assert!(matches!(
            events.recv().await,
            Some(RuntimeEvent::SubagentSpawned { record }) if record == spawned
        ));
        assert!(matches!(
            events.recv().await,
            Some(RuntimeEvent::SubagentRecord { record }) if record == finished
        ));
        assert!(matches!(
            events.recv().await,
            Some(RuntimeEvent::SubagentFinished { record }) if record == finished
        ));
        assert!(matches!(
            statuses.recv().await,
            Some(ProgressEnvelope {
                item: RuntimeEvent::SubagentStatus { revision: 3, .. },
                ..
            })
        ));
        assert!(statuses.try_recv().is_err());
        assert_eq!(
            controller.subagent_status_revisions.get(finished.id()),
            Some(&5)
        );
    }

    #[tokio::test]
    async fn coalesced_child_changes_publish_the_latest_record_once() {
        let workspace = tempfile::tempdir().unwrap();
        let factory = GatedChildFactory {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            attempts: vec![AttemptAccounting::UsageUnavailable],
        };
        let coordinator = complete_child_coordinator(factory.clone());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(&caller, child_parameters(), child_payload(&workspace).await)
            .await
            .expect("admit child");
        factory.started.notified().await;
        let mut controller =
            controller_with_subagents(&workspace, ScriptedModel::default(), coordinator.clone())
                .await;
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _statuses) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut progress = ProgressPublisher::new(progress_sender);
        let record = coordinator.record(child.id()).await.expect("child record");
        let changed = CoordinatorEvent::Changed {
            id: child.id().clone(),
            revision: record.revision(),
        };

        controller
            .handle_coordinator_event(changed.clone(), &event_sender, &mut progress)
            .await;
        controller
            .handle_coordinator_event(changed, &event_sender, &mut progress)
            .await;

        assert!(matches!(
            events.recv().await,
            Some(RuntimeEvent::SubagentRecord { record: emitted })
                if emitted.id() == record.id() && emitted.revision() == record.revision()
        ));
        assert!(events.try_recv().is_err());
        assert_eq!(
            controller.subagent_record_revisions.get(child.id()),
            Some(&record.revision())
        );

        factory.release.notify_one();
        coordinator.wait(&caller, child.id()).await.unwrap();
    }

    #[tokio::test]
    async fn terminal_child_changes_emit_no_record_or_status() {
        let workspace = tempfile::tempdir().unwrap();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let terminal = TaskRecord::for_test("c1", 2, selection, None, 1, Vec::new());
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(super::UnavailableChildRunner),
        );
        coordinator
            .restore_terminal_tasks([terminal.clone()], Some(1), std::iter::empty())
            .await;
        let mut controller =
            controller_with_subagents(&workspace, ScriptedModel::default(), coordinator).await;
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, mut statuses) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut progress = ProgressPublisher::new(progress_sender);

        controller
            .handle_coordinator_event(
                CoordinatorEvent::Changed {
                    id: terminal.id().clone(),
                    revision: terminal.revision(),
                },
                &event_sender,
                &mut progress,
            )
            .await;

        assert!(events.try_recv().is_err());
        assert!(statuses.try_recv().is_err());
    }

    #[tokio::test]
    async fn finished_children_emit_one_new_authoritative_record_before_each_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _statuses) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut progress = ProgressPublisher::new(progress_sender);
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let spawned =
            crate::subagents::TaskRecord::for_test("c1", 3, selection, None, 1, Vec::new());
        let finished = crate::subagents::TaskRecord::for_test(
            "c1",
            5,
            selection,
            None,
            1,
            vec![AttemptAccounting::UsageUnavailable],
        );

        controller
            .handle_coordinator_event(
                CoordinatorEvent::Spawned {
                    record: spawned.clone(),
                },
                &event_sender,
                &mut progress,
            )
            .await;
        for _ in 0..2 {
            controller
                .handle_coordinator_event(
                    CoordinatorEvent::Finished {
                        record: finished.clone(),
                    },
                    &event_sender,
                    &mut progress,
                )
                .await;
        }

        let emitted = (0..4)
            .map(|_| events.try_recv().expect("runtime event"))
            .collect::<Vec<_>>();
        assert!(matches!(
            emitted.as_slice(),
            [
                RuntimeEvent::SubagentSpawned { record },
                RuntimeEvent::SubagentRecord { record: record_after_finish },
                RuntimeEvent::SubagentFinished { record: first_snapshot },
                RuntimeEvent::SubagentFinished { record: second_snapshot },
            ] if record == &spawned
                && record_after_finish == &finished
                && first_snapshot == &finished
                && second_snapshot == &finished
        ));
        assert!(events.try_recv().is_err());
        assert_eq!(
            controller.subagent_record_revisions.get(finished.id()),
            Some(&finished.revision())
        );
    }

    #[tokio::test]
    async fn complete_delegation_workflow_crosses_requests_without_context_leaks() {
        let workspace = tempfile::tempdir().unwrap();
        let initial_selection = ModelSelection::new(&SOL, "high").unwrap();
        let changed_selection = ModelSelection::new(&TERRA, "low").unwrap();
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let root_selections = Arc::new(Mutex::new(Vec::new()));
        let children = WorkflowChildFactory::new();
        let coordinator = complete_child_coordinator(children.clone());
        let controller = controller_with_subagents(
            &workspace,
            WorkflowRootModel {
                contexts: contexts.clone(),
                selections: root_selections.clone(),
                request: 0,
                step: 0,
            },
            coordinator.clone(),
        )
        .await
        .with_selection(initial_selection);
        let (commands, mut events, mut progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "delegate the first request".to_owned(),
            })
            .await
            .unwrap();
        let mut workflow_events = events_until_settled(&mut events).await;
        children.grandchild_started.notified().await;

        commands
            .send(RuntimeCommand::ApplySettings {
                selection: changed_selection,
                revision: 1,
            })
            .await
            .unwrap();
        loop {
            let event = next_event(&mut events).await;
            let accepted = event == (RuntimeEvent::SettingsAccepted { revision: 1 });
            workflow_events.push(event);
            if accepted {
                break;
            }
        }

        commands
            .send(RuntimeCommand::Submit {
                request: "clarify the delegated work".to_owned(),
            })
            .await
            .unwrap();
        workflow_events.extend(events_until_settled(&mut events).await);
        while workflow_events
            .iter()
            .filter(|event| matches!(event, RuntimeEvent::SubagentFinished { .. }))
            .count()
            < 4
        {
            workflow_events.push(next_event(&mut events).await);
        }
        let status_events = drain_progress(&mut progress)
            .into_iter()
            .map(|event| event.item)
            .collect::<Vec<_>>();

        let records = shutdown_controller(commands, task).await;
        assert_eq!(records, committed_records(&workflow_events));
        assert_eq!(coordinator.active_count().await, 0);

        let child_records = coordinator.records().await;
        assert_eq!(
            child_records
                .iter()
                .map(|record| record.id().to_string())
                .collect::<Vec<_>>(),
            ["0", "1", "2", "2.0"]
        );
        assert!(
            child_records[..3]
                .iter()
                .all(|record| record.admission_history_step_id().is_some())
        );
        assert_eq!(
            child_records
                .iter()
                .map(|record| record.parameters().selection())
                .collect::<Vec<_>>(),
            [
                ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                ModelSelection::new(&LUNA, LUNA.default_thinking).unwrap(),
                initial_selection,
                initial_selection,
            ]
        );
        assert!(child_records[3].parameters().read_only());
        assert_eq!(child_records[3].parent().unwrap().to_string(), "2");
        assert!(matches!(
            child_records[0].outcome(),
            Some(TaskOutcome::Completed(result))
                if result.message() == "recoverable child completed"
                    && result.values() == &BTreeMap::from([
                        ("handoff".to_owned(), ToolValue::String("before".to_owned())),
                        ("same_execution".to_owned(), ToolValue::String("missing".to_owned())),
                    ])
        ));
        assert!(matches!(
            child_records[1].outcome(),
            Some(TaskOutcome::Failed(failure)) if failure.message().contains("controlled failure")
        ));
        assert!(matches!(
            child_records[2].outcome(),
            Some(TaskOutcome::Completed(result))
                if result.values() == &BTreeMap::from([(
                    "descendant_id".to_owned(),
                    ToolValue::String("2.0".to_owned()),
                )])
        ));
        assert!(matches!(
            child_records[3].outcome(),
            Some(TaskOutcome::Cancelled)
        ));

        let mut factory_selections = children
            .selections
            .lock()
            .unwrap()
            .iter()
            .map(|selection| format!("{}:{}", selection.model().id, selection.thinking()))
            .collect::<Vec<_>>();
        factory_selections.sort();
        assert_eq!(
            factory_selections,
            [
                "openai/gpt-5.6-luna:medium",
                "openai/gpt-5.6-sol:high",
                "openai/gpt-5.6-sol:high",
                "openai/gpt-5.6-terra:medium",
            ]
        );

        let first_execution_started = workflow_events
            .iter()
            .position(|event| matches!(event, RuntimeEvent::ExecutionStarted { .. }))
            .expect("reliable execution start");
        let first_root_spawned = workflow_events
            .iter()
            .position(|event| {
                matches!(event, RuntimeEvent::SubagentSpawned { record } if record.id().as_str() == "0")
            })
            .expect("first root spawn");
        assert!(first_execution_started < first_root_spawned);

        let first_spawns = workflow_events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::SubagentSpawned { record } => Some(record),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(first_spawns.len(), 4);
        assert_eq!(
            first_spawns
                .iter()
                .map(|record| record.id().to_string())
                .collect::<Vec<_>>(),
            ["0", "1", "2", "2.0"]
        );
        for record in &child_records {
            assert_eq!(
                workflow_events
                    .iter()
                    .filter(|event| {
                        matches!(event, RuntimeEvent::SubagentFinished { record: finished }
                            if finished.id() == record.id())
                    })
                    .count(),
                1,
                "{} should have one terminal card event",
                record.id()
            );
            assert!(workflow_events.iter().any(|event| {
                matches!(event, RuntimeEvent::SubagentRecord { record: updated }
                if updated.id() == record.id()
                    && matches!(
                        updated.observation().state(),
                        TaskState::Completed
                            | TaskState::Failed
                            | TaskState::Cancelled
                            | TaskState::TimedOut
                    ))
            }));
        }
        for (id, state) in [
            ("0", TaskState::Completed),
            ("1", TaskState::Failed),
            ("2", TaskState::Completed),
            ("2.0", TaskState::Cancelled),
        ] {
            assert!(status_events.iter().any(|event| {
                matches!(event, RuntimeEvent::SubagentStatus { id: observed, observation, .. }
                    if observed.as_str() == id && observation.state() == state)
            }));
        }

        let first_execution = completed_execution(&records, RunId(1), ModelStepId(2));
        assert_eq!(
            first_execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent("0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent("1".to_owned()),
                },
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent("2".to_owned()),
                },
            ]
        );
        let clarification = completed_execution(&records, RunId(2), ModelStepId(4));
        assert_eq!(result_head(clarification), "'missing'");
        assert_eq!(
            clarification.tool_calls,
            [
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::RefreshSubagent,
                    summary: ToolCallSummary::Subagent("0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::WaitSubagent,
                    summary: ToolCallSummary::Subagent("0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("1".to_owned()),
                },
                ToolCall {
                    tool: ToolName::WaitSubagent,
                    summary: ToolCallSummary::Subagent("1".to_owned()),
                },
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("2".to_owned()),
                },
                ToolCall {
                    tool: ToolName::WaitSubagent,
                    summary: ToolCallSummary::Subagent("2".to_owned()),
                },
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("2.0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::RefreshSubagent,
                    summary: ToolCallSummary::Subagent("2.0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::CancelSubagent,
                    summary: ToolCallSummary::Subagent("2".to_owned()),
                },
                ToolCall {
                    tool: ToolName::RefreshSubagent,
                    summary: ToolCallSummary::Subagent("2.0".to_owned()),
                },
            ]
        );

        assert_eq!(
            root_selections.lock().unwrap().as_slice(),
            [
                initial_selection,
                initial_selection,
                initial_selection,
                changed_selection,
                changed_selection,
            ]
        );

        let contexts = contexts.lock().unwrap();
        assert_eq!(contexts.len(), 5);
        assert!(matches!(
            contexts[3].as_slice(),
            [
                ModelContextEntry::UserRequest { text: first },
                ModelContextEntry::CompletedCode(_),
                ModelContextEntry::CompletedCode(_),
                ModelContextEntry::FinalMessage { text: final_message, .. },
                ModelContextEntry::UserRequest { text: clarification },
            ] if first == "delegate the first request"
                && final_message == "first foreground final"
                && clarification == "clarify the delegated work"
        ));
        let [
            ModelContextEntry::UserRequest { text: first },
            ModelContextEntry::CompletedCode(_),
            ModelContextEntry::CompletedCode(_),
            ModelContextEntry::FinalMessage {
                text: final_message,
                ..
            },
            ModelContextEntry::UserRequest {
                text: clarification,
            },
            ModelContextEntry::CompletedCode(clarification_result),
        ] = contexts[4].as_slice()
        else {
            panic!("unexpected clarification context: {:#?}", contexts[4]);
        };
        assert_eq!(first, "delegate the first request");
        assert_eq!(final_message, "first foreground final");
        assert_eq!(clarification, "clarify the delegated work");
        let CodeResult::Execution { feedback } = clarification_result.result() else {
            panic!("expected clarification execution feedback");
        };
        assert_eq!(
            feedback,
            concat!(
                "<kiki_result truncated=\"false\" total_lines=\"1\" total_bytes=\"9\">\n",
                "<kiki_output_head lines=\"1\" bytes=\"9\" ends_mid_line=\"false\">\n",
                "'missing'</kiki_output_head>\n",
                "</kiki_result>"
            )
        );
        let context_debug = format!("{contexts:#?}");
        for child_text in [
            "recoverable child completed",
            "cancellable parent completed",
            "controlled failure",
        ] {
            assert!(
                !context_debug.contains(child_text),
                "root model context leaked `{child_text}`: {context_debug}"
            );
        }
        assert_eq!(
            context_debug.matches("missing").count(),
            1,
            "the deliberate child value must appear only in execution feedback: {context_debug}"
        );

        let mut transcript = Transcript::default();
        for event in workflow_events
            .iter()
            .cloned()
            .chain(status_events.iter().cloned())
        {
            transcript.apply(event, false);
        }
        let footer = transcript.layout(160).footer_rule(160).to_string();
        assert!(footer.contains("↑560 ↓56 R100 W0"), "{footer}");
        assert!(footer.contains("CH20.0%"), "{footer}");
        assert!(footer.contains("0.0%/272k"), "{footer}");
        assert!(footer.contains('$'), "{footer}");
    }

    #[test]
    fn restored_timeline_orders_records_and_terminal_task_lifecycle_by_revision() {
        let task = crate::subagents::TaskRecord::for_test(
            "00",
            2,
            ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let initial = restored_initial_state(&SessionRestore {
            effective_entry: SessionEntryId(1),
            observed_active_entry: SessionEntryId(1),
            request_prefill: None,
            metadata: None,
            records: vec![
                RestoredRecord {
                    revision: 2,
                    run_id: 1,
                    kind: RestoredRecordKind::UserRequest {
                        request: "saved request".to_owned(),
                        selection: None,
                    },
                },
                RestoredRecord {
                    revision: 5,
                    run_id: 1,
                    kind: RestoredRecordKind::FinalMessage {
                        step_id: 1,
                        text: "saved answer".to_owned(),
                        reasoning_summary: String::new(),
                        terminal: crate::session::ReadableTerminal::Final {
                            text: "saved answer".to_owned(),
                            reasoning_summary: String::new(),
                            attempts: Vec::new(),
                        },
                    },
                },
            ],
            tasks: vec![RestoredTask {
                record: task.clone(),
                admission_anchor_revision: 5,
                terminal_anchor_revision: 5,
                admitted_revision: 3,
                terminal_revision: 4,
            }],
            all_tasks: vec![RestoredTask {
                record: task,
                admission_anchor_revision: 5,
                terminal_anchor_revision: 5,
                admitted_revision: 3,
                terminal_revision: 4,
            }],
            reserved_task_ids: Vec::new(),
            footer_adjustment: Vec::new(),
            next_task: Some(1),
            executable: None,
        });

        assert!(matches!(
            initial.timeline.as_slice(),
            [
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                }),
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::FinalMessage { .. },
                    ..
                }),
                RestoredTimelineItem::SubagentSpawned(_),
                RestoredTimelineItem::SubagentFinished(_),
            ]
        ));
    }

    #[test]
    fn restored_timeline_keeps_later_task_completion_at_its_event_revision() {
        let task = crate::subagents::TaskRecord::for_test(
            "0",
            2,
            ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let initial = restored_initial_state(&SessionRestore {
            effective_entry: SessionEntryId(1),
            observed_active_entry: SessionEntryId(1),
            request_prefill: None,
            metadata: None,
            records: vec![
                RestoredRecord {
                    revision: 2,
                    run_id: 1,
                    kind: RestoredRecordKind::UserRequest {
                        request: "origin request".to_owned(),
                        selection: None,
                    },
                },
                RestoredRecord {
                    revision: 5,
                    run_id: 1,
                    kind: RestoredRecordKind::FinalMessage {
                        step_id: 1,
                        text: "origin answer".to_owned(),
                        reasoning_summary: String::new(),
                        terminal: crate::session::ReadableTerminal::Final {
                            text: "origin answer".to_owned(),
                            reasoning_summary: String::new(),
                            attempts: Vec::new(),
                        },
                    },
                },
                RestoredRecord {
                    revision: 6,
                    run_id: 2,
                    kind: RestoredRecordKind::UserRequest {
                        request: "later request".to_owned(),
                        selection: None,
                    },
                },
            ],
            tasks: vec![RestoredTask {
                record: task,
                admission_anchor_revision: 5,
                terminal_anchor_revision: 5,
                admitted_revision: 3,
                terminal_revision: 7,
            }],
            all_tasks: Vec::new(),
            reserved_task_ids: Vec::new(),
            footer_adjustment: Vec::new(),
            next_task: Some(1),
            executable: None,
        });

        assert!(matches!(
            initial.timeline.as_slice(),
            [
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::UserRequest { request: origin, .. },
                    ..
                }),
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::FinalMessage { .. },
                    ..
                }),
                RestoredTimelineItem::SubagentSpawned(_),
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::UserRequest { request: later, .. },
                    ..
                }),
                RestoredTimelineItem::SubagentFinished(_),
            ] if origin == "origin request" && later == "later request"
        ));
    }

    #[test]
    fn restored_timeline_queues_completion_after_its_later_foreground_anchor() {
        let task = crate::subagents::TaskRecord::for_test(
            "0",
            2,
            ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let final_message = |step_id, text: &str| RestoredRecordKind::FinalMessage {
            step_id,
            text: text.to_owned(),
            reasoning_summary: String::new(),
            terminal: crate::session::ReadableTerminal::Final {
                text: text.to_owned(),
                reasoning_summary: String::new(),
                attempts: Vec::new(),
            },
        };
        let initial = restored_initial_state(&SessionRestore {
            effective_entry: SessionEntryId(1),
            observed_active_entry: SessionEntryId(1),
            request_prefill: None,
            metadata: None,
            records: vec![
                RestoredRecord {
                    revision: 2,
                    run_id: 1,
                    kind: RestoredRecordKind::UserRequest {
                        request: "origin request".to_owned(),
                        selection: None,
                    },
                },
                RestoredRecord {
                    revision: 5,
                    run_id: 1,
                    kind: final_message(1, "origin answer"),
                },
                RestoredRecord {
                    revision: 6,
                    run_id: 2,
                    kind: RestoredRecordKind::UserRequest {
                        request: "later request".to_owned(),
                        selection: None,
                    },
                },
                RestoredRecord {
                    revision: 8,
                    run_id: 2,
                    kind: final_message(2, "later answer"),
                },
            ],
            tasks: vec![RestoredTask {
                record: task,
                admission_anchor_revision: 5,
                terminal_anchor_revision: 8,
                admitted_revision: 3,
                terminal_revision: 7,
            }],
            all_tasks: Vec::new(),
            reserved_task_ids: Vec::new(),
            footer_adjustment: Vec::new(),
            next_task: Some(1),
            executable: None,
        });

        assert!(matches!(
            initial.timeline.as_slice(),
            [
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                }),
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::FinalMessage { .. },
                    ..
                }),
                RestoredTimelineItem::SubagentSpawned(_),
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::UserRequest { .. },
                    ..
                }),
                RestoredTimelineItem::Record(StoredRecord {
                    record: Record::FinalMessage { .. },
                    ..
                }),
                RestoredTimelineItem::SubagentFinished(_),
            ]
        ));
    }

    #[tokio::test]
    async fn incompatible_active_checkpoint_keeps_records_readable_and_disables_runtime() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let relocated = tempfile::tempdir().expect("create relocated workspace");
        let relocation_gate = SessionWriteGate::new(SessionWriteKind::Relocation);
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            relocation_gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        let run_id = writer
            .commit_request("readable saved request".to_owned(), selection)
            .await
            .expect("commit readable request")
            .run_id;
        let step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start readable model step");
        let execution = arena
            .execute("checkpoint_value = 1", &CancellationToken::new())
            .await
            .expect("execute readable model step");
        let compatible = arena.checkpoint();
        let (_, state, authorisations) = compatible.session_parts();
        let incompatible = Checkpoint::from_session_parts(
            RuntimeIdentity::from_session_value("incompatible test runtime".to_owned()),
            state.to_vec(),
            authorisations.clone(),
        );
        writer
            .settle_execution(
                run_id,
                step_id,
                PersistedCode {
                    source: "checkpoint_value = 1".to_owned(),
                    call_id: ModelCallId::new("incompatible-checkpoint"),
                    commentary: None,
                    reasoning_summary: String::new(),
                    replay: fixture_replay("incompatible checkpoint replay"),
                    result: CodeResult::execution(String::new()),
                    execution: Some(execution),
                    attempts: Vec::new(),
                },
                incompatible,
            )
            .await
            .expect("persist incompatible checkpoint");
        let metadata_before: (String, String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, cwd, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("snapshot session metadata");
        drop(writer);
        let restored_arena = Arena::new(
            worker_path(),
            ToolHost::new(relocated.path())
                .await
                .expect("create relocated tool host"),
        )
        .await
        .expect("create relocated arena");
        let calls = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let (controller, initial) = RuntimeController::restore(
            &session,
            restored_arena,
            RestoredStateModel {
                observed: Arc::new(Mutex::new(Vec::new())),
                contexts: Arc::new(Mutex::new(Vec::new())),
                calls: calls.clone(),
                actions: Vec::new(),
                call_prefix: "incompatible",
                runner_starts: None,
                expected_runner_starts: Vec::new(),
                step: 0,
            },
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CountingChildRunner(starts.clone())),
            ),
            Some(relocated.path().to_path_buf()),
        )
        .await
        .expect("return disabled restored runtime");

        assert!(matches!(
            initial.timeline.as_slice(),
            [
                RestoredTimelineItem::Record(StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest { request, .. },
                }),
                RestoredTimelineItem::Record(StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution { source, .. },
                }),
            ] if request == "readable saved request" && source == "checkpoint_value = 1"
        ));
        assert_eq!(
            initial.disabled_reason,
            Some(RuntimeDisabledReason::SessionRestoreFailed)
        );
        assert!(!relocation_gate.has_started());
        let metadata_after: (String, String, i64, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, cwd, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read session metadata");
        assert_eq!(metadata_after, metadata_before);
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "must not run".to_owned(),
            })
            .await
            .expect("submit disabled request");
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionRestoreFailed
            }
        ));
        shutdown_controller(commands, controller).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        session.close().expect("close session");
    }

    fn fixture_replay(label: &str) -> ProviderReplay {
        ProviderReplay::new(
            "fixture",
            vec![OpaqueReplayItem::new(label.as_bytes().to_vec())],
        )
    }

    #[cfg(unix)]
    fn active_entry_id(session: &Session) -> i64 {
        session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT active_entry_id FROM session WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("read active entry")
    }

    #[cfg(unix)]
    fn set_active_entry_for_fixture(session: &Session, entry_id: i64) {
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute(
                "UPDATE session SET active_entry_id = ?1, revision = revision + 1 WHERE singleton = 1",
                params![entry_id],
            )
            .expect("move fixture active entry");
        session
            .sync_model_context_for_test()
            .expect("sync fixture model context");
    }

    #[cfg(unix)]
    fn completed_fixture_task(
        id: &str,
        parent: Option<&str>,
        attempts: Vec<AttemptAccounting>,
        result: ChildResult,
    ) -> TaskRecord {
        TaskRecord::for_test(
            id,
            2,
            ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
            parent,
            2,
            attempts,
        )
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 100,
            terminal_at_ms: Some(200),
            state: TaskState::Completed,
            step_count: 2,
            elapsed: Duration::from_secs(3),
            observed_at_ms: 200,
            attempt_evidence: Vec::new(),
            executions: Vec::new(),
            outcome: Some(TaskOutcome::Completed(result)),
        })
    }

    #[cfg(unix)]
    async fn persist_fixture_task(
        writer: &crate::session::SessionWriter,
        origin_step_id: u64,
        terminal: TaskRecord,
    ) {
        let admitted = TaskRecord::for_test(
            terminal.id().as_str(),
            1,
            terminal.parameters().selection(),
            terminal.parent().map(|parent| parent.as_str()),
            terminal.depth(),
            Vec::new(),
        )
        .with_storage_ids_for_test(terminal.storage_id(), terminal.parent_storage_id())
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: terminal.admitted_at_ms(),
            terminal_at_ms: None,
            state: TaskState::Starting,
            step_count: 0,
            elapsed: Duration::ZERO,
            observed_at_ms: terminal.admitted_at_ms(),
            attempt_evidence: Vec::new(),
            executions: Vec::new(),
            outcome: None,
        });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&admitted, origin_step_id)
                        .expect("encode task admission"),
                    || false,
                )
                .await
                .expect("persist task admission"),
            "accept fixture task admission"
        );
        writer
            .terminal_subagent_task(
                TaskTerminal::from_record(&terminal).expect("encode task terminal"),
            )
            .await
            .expect("persist task terminal");
    }

    #[cfg(unix)]
    struct BranchedResumeFixture {
        footer_adjustment: Vec<AttemptAccounting>,
        selected_leaf: SessionEntryId,
        sibling_leaf: SessionEntryId,
    }

    #[cfg(unix)]
    async fn append_branched_resume_fixture(
        session: &Session,
        workspace: &Path,
    ) -> BranchedResumeFixture {
        let writer = session.writer();
        let selected_origin_step = u64::try_from(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row(
                    "SELECT model_steps.id FROM model_steps JOIN entries ON entries.id = model_steps.settled_entry_id WHERE entries.kind = 'execution' ORDER BY model_steps.id LIMIT 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read selected origin step"),
        )
        .expect("selected origin step fits u64");
        let late_task = completed_fixture_task(
            "0.0",
            Some("0"),
            Vec::new(),
            ChildResult::new(
                "late selected result".to_owned(),
                BTreeMap::from([("visible".to_owned(), ToolValue::Integer(62))]),
            ),
        )
        .with_storage_ids_for_test(98, Some(0));
        let late_task_storage_id = late_task.storage_id();
        persist_fixture_task(&writer, selected_origin_step, late_task).await;
        let selected_leaf = active_entry_id(session);
        let (origin_revision, selected_terminal_revision, late_task_revision): (i64, i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT (SELECT entries.revision FROM model_steps JOIN entries ON entries.id = model_steps.settled_entry_id WHERE model_steps.id = ?1), (SELECT revision FROM entries WHERE id = ?2), (SELECT terminal_revision FROM subagent_tasks WHERE id = ?3)",
                params![
                    i64::try_from(selected_origin_step).expect("selected origin fits SQLite"),
                    selected_leaf,
                    i64::try_from(late_task_storage_id).expect("task ID fits SQLite"),
                ],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read selected task chronology");
        assert!(late_task_revision > origin_revision);
        assert!(late_task_revision > selected_terminal_revision);
        let selected_checkpoint = session
            .restore()
            .expect("restore selected checkpoint")
            .executable
            .expect("selected history is executable")
            .checkpoint;
        let root_entry: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT id FROM entries WHERE kind = 'session_start'",
                [],
                |row| row.get(0),
            )
            .expect("read session root");
        set_active_entry_for_fixture(session, root_entry);

        let sibling_run = writer
            .commit_request(
                "sibling request".to_owned(),
                ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
            )
            .await
            .expect("commit sibling request")
            .run_id;
        let sibling_step = writer
            .start_model_step(sibling_run)
            .await
            .expect("start sibling execution");
        let mut sibling_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace)
                .await
                .expect("create sibling tool host"),
        )
        .await
        .expect("create sibling arena");
        sibling_arena
            .restore(session.initial_checkpoint().expect("load root checkpoint"))
            .await
            .expect("restore root checkpoint");
        let source = "sibling_value = 99\nresult_values = {'sibling': sibling_value}";
        let execution = sibling_arena
            .execute(source, &CancellationToken::new())
            .await
            .expect("execute sibling branch");
        assert!(matches!(
            execution.outcome,
            ExecutionOutcome::Complete { result: None }
        ));
        let sibling_execution_attempt = known_usage(DEFAULT_MODEL, 33, 3, 30);
        let sibling_checkpoint = sibling_arena.checkpoint();
        assert_ne!(sibling_checkpoint, selected_checkpoint);
        writer
            .settle_execution(
                sibling_run,
                sibling_step,
                PersistedCode {
                    source: source.to_owned(),
                    call_id: ModelCallId::new("sibling-execution"),
                    commentary: Some("sibling commentary".to_owned()),
                    reasoning_summary: "sibling reasoning".to_owned(),
                    replay: fixture_replay("sibling execution replay"),
                    result: CodeResult::execution("sibling execution feedback".to_owned()),
                    execution: Some(execution.clone()),
                    attempts: vec![sibling_execution_attempt.clone()],
                },
                sibling_checkpoint,
            )
            .await
            .expect("settle sibling execution");
        let sibling_terminal_step = writer
            .start_model_step(sibling_run)
            .await
            .expect("start sibling terminal step");
        let sibling_terminal_attempt = known_usage(DEFAULT_MODEL, 44, 4, 40);
        writer
            .close_run(
                sibling_run,
                Some(sibling_terminal_step),
                {
                    let sibling_terminal_attempt = sibling_terminal_attempt.clone();
                    move || PersistedTerminal::Failed {
                        message: "sibling terminal failure".to_owned(),
                        evidence: None,
                        attempts: vec![sibling_terminal_attempt],
                    }
                },
                Some(PersistedCode {
                    source: "unreachable sibling failure".to_owned(),
                    call_id: ModelCallId::new("sibling-terminal"),
                    commentary: None,
                    reasoning_summary: "sibling terminal reasoning".to_owned(),
                    replay: fixture_replay("sibling terminal replay"),
                    result: CodeResult::failure("sibling terminal feedback".to_owned()),
                    execution: None,
                    attempts: Vec::new(),
                }),
            )
            .await
            .expect("close sibling run");
        let sibling_leaf = SessionEntryId(
            u64::try_from(active_entry_id(session)).expect("sibling entry ID fits u64"),
        );

        let hidden_attempt = known_usage(DEFAULT_MODEL, 55, 5, 50);
        let mut hidden = completed_fixture_task(
            "63",
            None,
            vec![hidden_attempt.clone(), hidden_attempt.clone()],
            ChildResult::new(
                "hidden high result".to_owned(),
                BTreeMap::from([("hidden".to_owned(), ToolValue::Integer(63))]),
            ),
        )
        .with_storage_ids_for_test(99, None);
        hidden = hidden.with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 300,
            terminal_at_ms: Some(400),
            state: TaskState::Completed,
            step_count: 2,
            elapsed: Duration::from_secs(4),
            observed_at_ms: 400,
            attempt_evidence: vec![
                TaskAttempt::for_test(1, 1, Some(hidden_attempt.clone())),
                TaskAttempt::for_test(1, 2, Some(hidden_attempt.clone())),
            ],
            executions: vec![TaskExecution::for_test(
                sibling_step,
                "hidden execution copy".to_owned(),
                String::new(),
                execution,
                vec![hidden_attempt.clone(), hidden_attempt.clone()],
            )],
            outcome: Some(TaskOutcome::Completed(ChildResult::new(
                "hidden high result".to_owned(),
                BTreeMap::from([("hidden".to_owned(), ToolValue::Integer(63))]),
            ))),
        });
        persist_fixture_task(&writer, sibling_step, hidden).await;
        let (origin_revision, hidden_task_revision): (i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT (SELECT entries.revision FROM model_steps JOIN entries ON entries.id = model_steps.settled_entry_id WHERE model_steps.id = ?1), (SELECT terminal_revision FROM subagent_tasks WHERE id = 99)",
                params![i64::try_from(sibling_step).expect("sibling step fits SQLite")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read hidden task chronology");
        assert!(hidden_task_revision > origin_revision);
        set_active_entry_for_fixture(session, selected_leaf);

        BranchedResumeFixture {
            footer_adjustment: vec![
                sibling_execution_attempt,
                sibling_terminal_attempt,
                hidden_attempt.clone(),
                hidden_attempt,
            ],
            selected_leaf: SessionEntryId(
                u64::try_from(selected_leaf).expect("selected entry ID fits u64"),
            ),
            sibling_leaf,
        }
    }

    #[cfg(unix)]
    #[derive(Debug)]
    struct StoredIdBounds {
        maximum: i64,
        count: i64,
    }

    #[cfg(unix)]
    #[derive(Debug)]
    struct ResumeIdBounds {
        runs: StoredIdBounds,
        entries: StoredIdBounds,
        checkpoints: StoredIdBounds,
        model_steps: StoredIdBounds,
        tasks: StoredIdBounds,
    }

    #[cfg(unix)]
    fn stored_id_bounds(home: &Path, id: &str) -> ResumeIdBounds {
        let connection = Connection::open_with_flags(
            home.join(".kiki/sessions").join(format!("{id}.sqlite3")),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only session database");
        let bounds = |table: &str| {
            connection
                .query_row(
                    &format!("SELECT COALESCE(MAX(id), -1), COUNT(*) FROM {table}"),
                    [],
                    |row| {
                        Ok(StoredIdBounds {
                            maximum: row.get(0)?,
                            count: row.get::<_, i64>(1)?,
                        })
                    },
                )
                .expect("read stored identifiers")
        };
        ResumeIdBounds {
            runs: bounds("runs"),
            entries: bounds("entries"),
            checkpoints: bounds("checkpoints"),
            model_steps: bounds("model_steps"),
            tasks: bounds("subagent_tasks"),
        }
    }

    #[cfg(unix)]
    fn assert_new_ids_follow_bounds(home: &Path, id: &str, before: &ResumeIdBounds) {
        let after = stored_id_bounds(home, id);
        let connection = Connection::open_with_flags(
            home.join(".kiki/sessions").join(format!("{id}.sqlite3")),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("reopen read-only session database");
        for (table, before, after, expected_new) in [
            ("runs", &before.runs, &after.runs, 1),
            ("entries", &before.entries, &after.entries, 3),
            ("checkpoints", &before.checkpoints, &after.checkpoints, 1),
            ("model_steps", &before.model_steps, &after.model_steps, 2),
            ("subagent_tasks", &before.tasks, &after.tasks, 1),
        ] {
            let mut statement = connection
                .prepare(&format!("SELECT id FROM {table} ORDER BY id"))
                .expect("query stored identifiers");
            let ids = statement
                .query_map([], |row| row.get::<_, i64>(0))
                .expect("iterate stored identifiers")
                .collect::<Result<Vec<_>, _>>()
                .expect("read stored identifiers");
            let new_ids = ids
                .iter()
                .copied()
                .filter(|id| *id > before.maximum)
                .collect::<Vec<_>>();
            assert_eq!(
                new_ids.len(),
                expected_new,
                "unexpected resumed {table} count"
            );
            assert_eq!(
                i64::try_from(ids.len()).expect("identifier count fits i64"),
                before.count + i64::try_from(expected_new).expect("new identifier count fits i64"),
                "{table} reused an identifier at or below its pre-resume maximum"
            );
            assert!(
                new_ids.iter().all(|id| *id > before.maximum),
                "{table} allocated an identifier at or below its pre-resume maximum"
            );
            assert!(
                after.maximum > before.maximum,
                "{table} did not allocate an identifier"
            );
        }
        let task_ids = connection
            .prepare("SELECT id FROM subagent_tasks WHERE id > ?1 ORDER BY id")
            .expect("query resumed task")
            .query_map(params![before.tasks.maximum], |row| row.get::<_, i64>(0))
            .expect("iterate resumed task")
            .collect::<Result<Vec<_>, _>>()
            .expect("read resumed task");
        assert_eq!(task_ids, [100]);
    }

    #[cfg(unix)]
    async fn resume_restores_hierarchical_task_ids_in_fresh_process() {
        let home = PathBuf::from(env::var("KIKI_RESUME_HOME").expect("resume home"));
        let workspace = PathBuf::from(env::var("KIKI_RESUME_WORKSPACE").expect("resume workspace"));
        let session_id = env::var("KIKI_RESUME_SESSION").expect("resume session ID");
        let socket_path = env::var("KIKI_RESUME_SOCKET").expect("resume socket");
        let mut socket = UnixStream::connect(socket_path).expect("connect to parent");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session = Session::open(&home, &session_id).expect("open session through normal path");
        let resumed_run_id = RunId(
            u64::try_from(
                session
                    .connection_for_test()
                    .expect("lock session")
                    .query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM runs", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .expect("read resumed run ID"),
            )
            .expect("resumed run ID fits u64"),
        );
        let mut expected_history = session
            .restore()
            .expect("restore session")
            .executable
            .expect("executable restore")
            .history;
        expected_history.append_user_request("continue");
        let expected_context = expected_history.entries().to_vec();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(&workspace).await.expect("create tool host"),
        )
        .await
        .expect("create arena");
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(AtomicUsize::new(0));
        let start_gate = StartSubagentTestGate::new();
        let (controller, initial) = RuntimeController::restore(
            &session,
            arena,
            RestoredStateModel {
                observed: Arc::new(Mutex::new(Vec::new())),
                contexts: contexts.clone(),
                calls: calls.clone(),
                actions: vec![
                    RestoredModelAction::Code {
                        source: "assert saved_value == 41\nsaved_result = await saved_task.wait()\nassert saved_result['message'] == 'saved child'\nassert saved_result['values']['answer'] == 7\nvisible = await get_subagent('0.0')\nvisible_result = await visible.wait()\nassert visible_result['message'] == 'late selected result'\nassert visible_result['values']['visible'] == 62\ntry:\n    sibling_value\n    assert False\nexcept NameError:\n    pass\ntry:\n    await get_subagent('63')\n    assert False\nexcept Exception as error:\n    assert 'subagent caller is not authorised for `63`' in str(error)\nnext_task = await start_subagent('new child')\nassert next_task.id == '1'\nawait next_task.wait()\nawait write('authorised', 'changed without reread')"
                            .to_owned(),
                        commentary: None,
                        attempts: Vec::new(),
                        cancel_before_execution: false,
                    },
                    RestoredModelAction::Message {
                        text: "restored response".to_owned(),
                        attempts: Vec::new(),
                    },
                ],
                call_prefix: "continued",
                runner_starts: Some(starts.clone()),
                expected_runner_starts: vec![0, 1],
                step: 0,
            },
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CountingChildRunner(starts.clone())),
            )
            .with_start_test_gate(start_gate.clone()),
            None,
        )
        .await
        .expect("restore runtime");
        assert!(initial.disabled_reason.is_none());
        assert!(initial.timeline.iter().all(|item| match item {
            RestoredTimelineItem::Record(_) => true,
            RestoredTimelineItem::SubagentSpawned(record)
            | RestoredTimelineItem::SubagentFinished(record) => record.id().as_str() != "63",
        }));
        let restored_tasks = controller.subagents.records().await;
        assert_eq!(
            restored_tasks
                .iter()
                .map(|task| task.id().as_str())
                .collect::<Vec<_>>(),
            ["0", "0.0", "63"]
        );
        assert_eq!(restored_tasks[0].parent(), None);
        assert_eq!(
            restored_tasks[1].parent().map(|parent| parent.as_str()),
            Some("0")
        );
        assert_eq!(
            restored_tasks[1].parent_storage_id(),
            Some(restored_tasks[0].storage_id())
        );
        assert!(matches!(
            restored_tasks[0].outcome(),
            Some(TaskOutcome::Completed(result)) if result.values().get("answer") == Some(&ToolValue::Integer(7))
        ));
        assert!(matches!(
            restored_tasks[1].outcome(),
            Some(TaskOutcome::Completed(result))
                if result.message() == "late selected result"
                    && result.values().get("visible") == Some(&ToolValue::Integer(62))
        ));
        assert!(matches!(
            restored_tasks[2].outcome(),
            Some(TaskOutcome::Completed(result))
                if result.message() == "hidden high result"
                    && result.values().get("hidden") == Some(&ToolValue::Integer(63))
        ));
        let (commands, mut events, mut progress, controller) = start_controller(controller);
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(matches!(
            progress.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        socket.write_all(b"R").expect("signal readiness");
        let mut start = [0];
        socket.read_exact(&mut start).expect("read parent start");
        assert_eq!(start, *b"S");
        commands
            .send(RuntimeCommand::Submit {
                request: "continue".to_owned(),
            })
            .await
            .expect("submit continued request");
        start_gate.wait_until_start_attempts(1).await;
        assert_eq!(
            starts.load(Ordering::SeqCst),
            0,
            "waiting on the restored task must not start a runner"
        );
        start_gate.allow_first_start();
        let resumed_events = events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert!(resumed_events.iter().all(|event| match event {
            RuntimeEvent::RunStarted { run_id } | RuntimeEvent::RunSettled { run_id } => {
                *run_id == resumed_run_id
            }
            RuntimeEvent::ModelStepStarted { run_id, .. }
            | RuntimeEvent::RecordAppended {
                record: StoredRecord { run_id, .. },
                ..
            } => *run_id == resumed_run_id,
            RuntimeEvent::SubagentSpawned { record }
            | RuntimeEvent::SubagentRecord { record }
            | RuntimeEvent::SubagentFinished { record } => record.id().as_str() == "1",
            _ => true,
        }));
        let contexts = contexts.lock().unwrap();
        assert_eq!(contexts[0], expected_context);
        assert_eq!(contexts.len(), 2);
        assert!(resumed_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { request, .. },
                    ..
                },
                ..
            } if request == "continue"
        )));
        session.close().expect("close restored session");
        socket.write_all(b"O").expect("signal success");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resume_restores_hierarchical_task_ids_and_handles() {
        if env::var_os("KIKI_RESUME_CHILD").is_some() {
            resume_restores_hierarchical_task_ids_in_fresh_process().await;
            return;
        }

        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("authorised"), "original").expect("write fixture");
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host"),
        )
        .await
        .expect("create arena");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let session =
            Session::create(home.path(), workspace.path(), selection, arena.checkpoint()).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::with_session(
            Agent::new(
                arena,
                RestoredStateModel {
                    observed: Arc::new(Mutex::new(Vec::new())),
                    contexts: Arc::new(Mutex::new(Vec::new())),
                    calls: Arc::new(AtomicUsize::new(0)),
                    actions: vec![
                        RestoredModelAction::Code {
                            source: "saved_value = 41\nsaved_task = await start_subagent('saved child', None, True)\nsaved_child_result = await saved_task.wait()\nawait read('authorised')".to_owned(),
                            commentary: Some("selected completed-code commentary".to_owned()),
                            attempts: vec![known_usage(DEFAULT_MODEL, 11, 1, 0)],
                            cancel_before_execution: false,
                        },
                        RestoredModelAction::Code {
                            source: "selected cancellation feedback source".to_owned(),
                            commentary: None,
                            attempts: vec![known_usage(DEFAULT_MODEL, 22, 2, 0)],
                            cancel_before_execution: true,
                        },
                    ],
                    call_prefix: "saved",
                    runner_starts: None,
                    expected_runner_starts: Vec::new(),
                    step: 0,
                },
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CountingChildRunner(starts.clone())),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "save this state".to_owned(),
            })
            .await
            .expect("submit saved request");
        let saved_events = events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1, "{saved_events:?}");

        let fixture = append_branched_resume_fixture(&session, workspace.path()).await;
        let expected_footer_adjustment = fixture.footer_adjustment.clone();
        let (sibling_payload, sibling_replay): (String, Vec<u8>) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT payload_json, provider_replay FROM entries WHERE kind = 'failed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read sibling terminal");
        assert!(sibling_payload.contains("sibling terminal feedback"));
        assert!(!sibling_replay.is_empty());
        let restore = session.restore().expect("restore branched session");
        let executable = restore
            .executable
            .as_ref()
            .expect("selected branch remains executable");
        let [
            ModelContextEntry::UserRequest { text },
            ModelContextEntry::CompletedCode(selected_code),
            ModelContextEntry::CompletedCode(cancelled_code),
        ] = executable.history.entries()
        else {
            panic!(
                "unexpected selected model context: {:#?}",
                executable.history.entries()
            );
        };
        assert_eq!(text, "save this state");
        assert!(
            !format!("{:#?}", executable.history.entries()).contains("sibling_value"),
            "selected context must not contain sibling Python"
        );
        assert_eq!(
            selected_code.commentary(),
            Some("selected completed-code commentary")
        );
        let CodeResult::Cancelled { feedback } = cancelled_code.result() else {
            panic!("selected terminal did not retain cancelled code feedback");
        };
        assert_eq!(
            feedback,
            "<kiki_execution state=\"cancelled\" began=\"false\">\nExecution did not begin.\nExecution was cancelled and its Python arena changes were discarded. External side effects completed before cancellation may remain even though pending actions were cancelled.\n</kiki_execution>"
        );
        assert!(matches!(
            restore.records.last().map(|record| &record.kind),
            Some(crate::session::RestoredRecordKind::CancelledRun {
                terminal: crate::session::ReadableTerminal::Cancelled {
                    evidence: Some(TerminalEvidence::Execution(evidence)),
                    ..
                }
            }) if evidence.source == "selected cancellation feedback source"
                && evidence.commentary.is_none()
        ));
        assert_eq!(restore.footer_adjustment, expected_footer_adjustment);
        assert_eq!(
            restore
                .tasks
                .iter()
                .map(|task| task.record.id().as_str())
                .collect::<Vec<_>>(),
            ["0", "0.0"]
        );
        assert!(matches!(
            restore.tasks[1].record.outcome(),
            Some(TaskOutcome::Completed(result))
                if result.message() == "late selected result"
                    && result.values().get("visible") == Some(&ToolValue::Integer(62))
        ));
        assert_eq!(restore.next_task, Some(100));
        let initial = restored_initial_state(&restore);
        let mut transcript =
            Transcript::restored(crate::config::ready_snapshot_for_test(), &initial);
        let footer = transcript.layout(160).footer_rule(160).to_string();
        assert!(footer.contains("↑220 ↓20"), "{footer}");
        assert!(footer.contains("CH0.0%"), "{footer}");
        assert!(footer.contains("0.0%/272k"), "{footer}");

        let mut sibling_context = session
            .writer()
            .restore_at(fixture.sibling_leaf)
            .await
            .expect("restore sibling branch")
            .executable
            .expect("sibling branch is executable")
            .history;
        sibling_context.append_user_request("continue sibling");
        let mut original_context = session
            .writer()
            .restore_at(fixture.selected_leaf)
            .await
            .expect("restore original branch")
            .executable
            .expect("original branch is executable")
            .history;
        original_context.append_user_request("probe original");
        let root = SessionEntryId(
            u64::try_from(
                session
                    .connection_for_test()
                    .expect("lock session")
                    .query_row(
                        "SELECT id FROM entries WHERE kind = 'session_start'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("read root entry"),
            )
            .expect("root entry ID fits u64"),
        );
        let navigation_contexts = Arc::new(Mutex::new(Vec::new()));
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create navigation tool host"),
        )
        .await
        .expect("create navigation arena");
        let (controller, initial) = RuntimeController::restore(
            &session,
            arena,
            NavigationBranchesModel {
                contexts: navigation_contexts.clone(),
                step: 0,
            },
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CountingChildRunner(Arc::new(AtomicUsize::new(0)))),
            ),
            None,
        )
        .await
        .expect("restore navigation runtime");
        assert_eq!(
            initial,
            restored_initial_state(&session.restore().expect("restore original"))
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Navigate { entry: root })
            .await
            .expect("rewind to root");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            } if state.timeline.is_empty()
        ));
        commands
            .send(RuntimeCommand::Navigate {
                entry: fixture.sibling_leaf,
            })
            .await
            .expect("select sibling branch");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                request_prefill: None,
                ..
            }
        ));
        commands
            .send(RuntimeCommand::Submit {
                request: "continue sibling".to_owned(),
            })
            .await
            .expect("continue sibling");
        events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::Navigate {
                entry: fixture.selected_leaf,
            })
            .await
            .expect("return original branch");
        let returned_state = match next_navigation_outcome(&mut events).await {
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            } => state,
            outcome => panic!("unexpected original-branch outcome: {outcome:?}"),
        };
        let expected_returned_state =
            restored_initial_state(&session.restore().expect("restore returned original branch"));
        assert_eq!(returned_state, expected_returned_state);
        assert_eq!(active_entry_id(&session), fixture.selected_leaf.0 as i64);
        let visible_task_ids = returned_state
            .timeline
            .iter()
            .filter_map(|item| match item {
                RestoredTimelineItem::SubagentSpawned(record)
                | RestoredTimelineItem::SubagentFinished(record) => {
                    Some(record.id().as_str().to_owned())
                }
                RestoredTimelineItem::Record(_) => None,
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            visible_task_ids,
            BTreeSet::from(["0".to_owned(), "0.0".to_owned()])
        );
        let returned_footer =
            Transcript::restored(crate::config::ready_snapshot_for_test(), &returned_state)
                .layout(160)
                .footer_rule(160)
                .to_string();
        assert!(returned_footer.contains("↑220 ↓20"), "{returned_footer}");
        assert!(returned_footer.contains("CH0.0%"), "{returned_footer}");
        assert!(returned_footer.contains("0.0%/272k"), "{returned_footer}");
        assert_eq!(
            fs::read_to_string(workspace.path().join("authorised"))
                .expect("read retained external effect"),
            "original"
        );
        commands
            .send(RuntimeCommand::Submit {
                request: "probe original".to_owned(),
            })
            .await
            .expect("probe original branch");
        let probe_events = events_until_settled(&mut events).await;
        assert!(probe_events.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { execution, .. },
                    ..
                },
                ..
            } if result_head(execution) == "41"
        )));
        assert_eq!(
            navigation_contexts
                .lock()
                .expect("navigation model contexts lock")[0],
            sibling_context.entries()
        );
        assert_eq!(
            navigation_contexts
                .lock()
                .expect("navigation model contexts lock")[2],
            original_context.entries()
        );
        shutdown_controller(commands, controller).await;
        let final_restore = session.restore().expect("restore final selected branch");
        let final_checkpoint = final_restore
            .executable
            .as_ref()
            .expect("final selected branch is executable")
            .checkpoint
            .clone();
        let final_history = final_restore
            .executable
            .as_ref()
            .expect("final selected branch is executable")
            .history
            .clone();
        let final_leaf = final_restore.effective_entry;

        let session_id = session.id().to_owned();
        let id_bounds = stored_id_bounds(home.path(), &session_id);
        session.close().expect("close original session");
        let reopened =
            Session::open(home.path(), &session_id).expect("reopen final selected session");
        let reopened_restore = reopened
            .restore()
            .expect("restore reopened selected branch");
        assert_eq!(reopened_restore.effective_entry, final_leaf);
        let reopened_executable = reopened_restore
            .executable
            .expect("reopened selected branch is executable");
        assert_eq!(reopened_executable.checkpoint, final_checkpoint);
        assert_eq!(reopened_executable.history, final_history);
        assert_eq!(
            reopened_restore.footer_adjustment,
            final_restore.footer_adjustment
        );
        assert_eq!(reopened_restore.records, final_restore.records);
        assert_eq!(reopened_restore.tasks, final_restore.tasks);
        reopened.close().expect("close reopened selected session");
        let socket_path = home.path().join("resume.sock");
        let listener = UnixListener::bind(&socket_path).expect("bind resume socket");
        let mut child = Command::new(env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("runtime::tests::resume_restores_hierarchical_task_ids_and_handles")
            .arg("--nocapture")
            .env("KIKI_RESUME_CHILD", "1")
            .env("KIKI_RESUME_HOME", home.path())
            .env("KIKI_RESUME_WORKSPACE", workspace.path())
            .env("KIKI_RESUME_SESSION", &session_id)
            .env("KIKI_RESUME_SOCKET", &socket_path)
            .spawn()
            .expect("spawn fresh process");
        let result = timeout(TEST_TIMEOUT, async {
            let (mut socket, _) = listener.accept().await?;
            let mut ready = [0];
            socket.read_exact(&mut ready).await?;
            if ready != *b"R" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "fresh process did not signal readiness",
                ));
            }
            socket.write_all(b"S").await?;
            let mut complete = [0];
            socket.read_exact(&mut complete).await?;
            Ok(complete)
        })
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "fresh process timed out"))
        .and_then(|result| result);
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait().expect("reap fresh process");
        assert!(status.success(), "fresh process failed: {status}");
        assert_eq!(result.expect("fresh process result"), *b"O");
        assert_new_ids_follow_bounds(home.path(), &session_id, &id_bounds);
        assert_eq!(
            fs::read_to_string(workspace.path().join("authorised")).expect("read restored write"),
            "changed without reread"
        );
    }

    async fn persist_navigation_branch(
        session: &Session,
        arena: &mut Arena,
        request: &str,
        source: &str,
        final_text: &str,
    ) -> (SessionEntryId, ModelHistory, Checkpoint) {
        let writer = session.writer();
        let run_id = writer
            .commit_request(
                request.to_owned(),
                ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
            )
            .await
            .expect("commit branch request")
            .run_id;
        let step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start branch execution");
        let execution = arena
            .execute(source, &CancellationToken::new())
            .await
            .expect("execute branch state");
        let checkpoint = arena.checkpoint();
        writer
            .settle_execution(
                run_id,
                step_id,
                PersistedCode {
                    source: source.to_owned(),
                    call_id: ModelCallId::new(format!("{request}-code")),
                    commentary: None,
                    reasoning_summary: String::new(),
                    replay: fixture_replay(&format!("{request} execution")),
                    result: CodeResult::execution(String::new()),
                    execution: Some(execution),
                    attempts: Vec::new(),
                },
                checkpoint.clone(),
            )
            .await
            .expect("settle branch execution");
        let terminal_step = writer
            .start_model_step(run_id)
            .await
            .expect("start branch terminal");
        let terminal_text = final_text.to_owned();
        let terminal_replay = fixture_replay(&format!("{request} final"));
        writer
            .close_run(
                run_id,
                Some(terminal_step),
                move || PersistedTerminal::Final {
                    step_id: terminal_step,
                    text: terminal_text,
                    reasoning_summary: String::new(),
                    replay: terminal_replay,
                    attempts: Vec::new(),
                },
                None,
            )
            .await
            .expect("close branch run");
        let terminal_entry = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT terminal_entry_id FROM runs WHERE id = ?1",
                params![i64::try_from(run_id).expect("run ID fits SQLite")],
                |row| row.get::<_, i64>(0),
            )
            .expect("read branch terminal");
        let mut history = ModelHistory::new();
        history.append_user_request(request);
        history
            .append_completed_code(
                source.to_owned(),
                ModelCallId::new(format!("{request}-code")),
                None,
                String::new(),
                fixture_replay(&format!("{request} execution")),
                CodeResult::execution(String::new()),
            )
            .expect("append branch code");
        history.append_final_message(
            final_text.to_owned(),
            String::new(),
            fixture_replay(&format!("{request} final")),
        );
        (
            SessionEntryId(u64::try_from(terminal_entry).expect("entry ID fits u64")),
            history,
            checkpoint,
        )
    }

    struct NavigationStateModel {
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        step: usize,
    }

    struct RejectTargetHistoryModel;

    impl Model for RejectTargetHistoryModel {
        fn validate_history(
            &self,
            history: &ModelHistory,
            _selection: ModelSelection,
        ) -> Result<(), ModelError> {
            if history.entries().iter().any(|entry| {
                matches!(entry, ModelContextEntry::UserRequest { text } if text == "target branch")
            }) {
                return Err(ModelError::InvalidResponse("rejected target history"));
            }
            Ok(())
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

    struct ValidatedSelectionsModel {
        selections: Arc<Mutex<Vec<ModelSelection>>>,
    }

    impl Model for ValidatedSelectionsModel {
        fn validate_history(
            &self,
            _history: &ModelHistory,
            selection: ModelSelection,
        ) -> Result<(), ModelError> {
            self.selections.lock().unwrap().push(selection);
            Ok(())
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

    struct NavigationBranchesModel {
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        step: usize,
    }

    impl Model for NavigationBranchesModel {
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
                self.contexts
                    .lock()
                    .expect("navigation model contexts lock")
                    .push(history.entries().to_vec());
                let action = match self.step {
                    0 => CompletedModelAction::Code {
                        source: "assert sibling_value == 99\nsibling_value".to_owned(),
                        call_id: ModelCallId::new("continue-sibling"),
                        commentary: None,
                    },
                    1 => CompletedModelAction::Message {
                        text: "sibling continuation".to_owned(),
                    },
                    2 => CompletedModelAction::Code {
                        source: "assert saved_value == 41\ntry:\n    sibling_value\n    assert False\nexcept NameError:\n    pass\nvisible = await get_subagent('0.0')\nassert visible.status.state == 'completed'\ntry:\n    await get_subagent('63')\n    assert False\nexcept Exception as error:\n    assert 'subagent caller is not authorised for `63`' in str(error)\nsaved_value".to_owned(),
                        call_id: ModelCallId::new("probe-original"),
                        commentary: None,
                    },
                    3 => CompletedModelAction::Message {
                        text: "original branch verified".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    impl Model for NavigationStateModel {
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
                self.contexts
                    .lock()
                    .unwrap()
                    .push(history.entries().to_vec());
                let action = match self.step {
                    0 => CompletedModelAction::Code {
                        source: "assert branch_value == 'target'\nbranch_value".to_owned(),
                        call_id: ModelCallId::new("navigation-continuation"),
                        commentary: None,
                    },
                    1 => CompletedModelAction::Message {
                        text: "continued target branch".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn navigation_restores_arena_and_context_before_publishing_history() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let initial_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host"),
        )
        .await
        .expect("create initial arena");
        let session = Session::create(
            home.path(),
            workspace.path(),
            selection,
            initial_arena.checkpoint(),
        )
        .expect("create session");
        let root = session.restore().expect("restore root").effective_entry;
        let mut fixture_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create fixture tool host"),
        )
        .await
        .expect("create fixture arena");
        fixture_arena
            .restore(session.initial_checkpoint().expect("load root checkpoint"))
            .await
            .expect("restore root checkpoint");
        let (old_entry, _, old_checkpoint) = persist_navigation_branch(
            &session,
            &mut fixture_arena,
            "old branch",
            "branch_value = 'old'",
            "old branch final",
        )
        .await;
        session
            .writer()
            .select_active_entry(root, old_entry)
            .await
            .expect("return fixture to root");
        fixture_arena
            .restore(session.initial_checkpoint().expect("load root checkpoint"))
            .await
            .expect("restore fixture root checkpoint");
        let (target_entry, target_history, _) = persist_navigation_branch(
            &session,
            &mut fixture_arena,
            "target branch",
            "branch_value = 'target'",
            "target branch final",
        )
        .await;
        session
            .writer()
            .select_active_entry(old_entry, target_entry)
            .await
            .expect("select old branch for navigation");
        let mut live_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create live tool host"),
        )
        .await
        .expect("create live arena");
        live_arena
            .restore(old_checkpoint)
            .await
            .expect("restore old branch live state");
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let starts = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::with_session(
            Agent::new(
                live_arena,
                NavigationStateModel {
                    contexts: contexts.clone(),
                    step: 0,
                },
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CountingChildRunner(starts.clone())),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("navigate to target branch");
        let navigation = next_event(&mut events).await;
        assert!(matches!(
            navigation,
            RuntimeEvent::NavigationCompleted {
                outcome: super::NavigationOutcome::Replaced {
                    ref state,
                    request_prefill: None,
                },
            } if state.timeline.len() == 3
        ));
        assert_eq!(
            session
                .restore()
                .expect("persist selected target")
                .effective_entry,
            target_entry
        );

        commands
            .send(RuntimeCommand::Submit {
                request: "continue".to_owned(),
            })
            .await
            .expect("submit after navigation");
        let continued = events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;

        let mut expected_context = target_history;
        expected_context.append_user_request("continue");
        assert_eq!(contexts.lock().unwrap()[0], expected_context.entries());
        assert!(continued.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { execution, .. },
                    ..
                },
                ..
            } if matches!(execution.outcome, ExecutionOutcome::Complete { .. })
        )));
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        session.close().expect("close session");
    }

    async fn next_navigation_outcome(
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> super::NavigationOutcome {
        loop {
            if let RuntimeEvent::NavigationCompleted { outcome } = next_event(events).await {
                return outcome;
            }
        }
    }

    async fn next_tree_outcome(
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> (u64, super::TreeRequestOutcome) {
        loop {
            if let RuntimeEvent::TreeCatalogue { revision, outcome } = next_event(events).await {
                return (revision, outcome);
            }
        }
    }

    async fn navigation_fixture(
        workspace: &tempfile::TempDir,
    ) -> (
        tempfile::TempDir,
        Session,
        Arena,
        SessionEntryId,
        SessionEntryId,
        SessionEntryId,
    ) {
        let home = tempfile::tempdir().expect("create home");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let initial_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create initial tool host"),
        )
        .await
        .expect("create initial arena");
        let session = Session::create(
            home.path(),
            workspace.path(),
            selection,
            initial_arena.checkpoint(),
        )
        .expect("create session");
        let root = session.restore().expect("restore root").effective_entry;
        let mut fixture_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create fixture tool host"),
        )
        .await
        .expect("create fixture arena");
        fixture_arena
            .restore(session.initial_checkpoint().expect("load root checkpoint"))
            .await
            .expect("restore fixture root checkpoint");
        let (old_entry, _, old_checkpoint) = persist_navigation_branch(
            &session,
            &mut fixture_arena,
            "old branch",
            "branch_value = 'old'",
            "old branch final",
        )
        .await;
        session
            .writer()
            .select_active_entry(root, old_entry)
            .await
            .expect("return fixture to root");
        fixture_arena
            .restore(session.initial_checkpoint().expect("load root checkpoint"))
            .await
            .expect("restore fixture root checkpoint");
        let (target_entry, _, _) = persist_navigation_branch(
            &session,
            &mut fixture_arena,
            "target branch",
            "branch_value = 'target'",
            "target branch final",
        )
        .await;
        session
            .writer()
            .select_active_entry(old_entry, target_entry)
            .await
            .expect("select old fixture branch");
        let mut live_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create live tool host"),
        )
        .await
        .expect("create live arena");
        live_arena
            .restore(old_checkpoint)
            .await
            .expect("restore old live state");
        (home, session, live_arena, root, old_entry, target_entry)
    }

    #[tokio::test]
    async fn navigation_restores_root_request_execution_and_terminal_points() {
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("effect"), "retained").expect("write external effect");
        let (_home, session, live_arena, root, _old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let target_execution = SessionEntryId(
            u64::try_from(
                session
                    .connection_for_test()
                    .expect("lock session")
                    .query_row(
                        "SELECT parent_id FROM entries WHERE id = ?1",
                        [i64::try_from(target_entry.0).expect("target entry fits SQLite")],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("read target execution"),
            )
            .expect("execution entry fits u64"),
        );
        let target_request = SessionEntryId(
            u64::try_from(
                session
                    .connection_for_test()
                    .expect("lock session")
                    .query_row(
                        "SELECT parent_id FROM entries WHERE id = ?1",
                        [i64::try_from(target_execution.0).expect("execution entry fits SQLite")],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("read target request"),
            )
            .expect("request entry fits u64"),
        );
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let controller = RuntimeController::with_session(
            Agent::new(live_arena, ScriptedModel::new(Vec::new())),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CountingChildRunner(starts.clone())),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate { entry: root })
            .await
            .expect("navigate to root");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            } if state.timeline.is_empty()
        ));
        commands
            .send(RuntimeCommand::Navigate {
                entry: target_execution,
            })
            .await
            .expect("navigate to execution");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            } if state.timeline.len() == 2
        ));
        commands
            .send(RuntimeCommand::Navigate {
                entry: target_request,
            })
            .await
            .expect("navigate to request");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: Some(request),
            } if state.timeline.is_empty() && request == "target branch"
        ));
        commands
            .send(RuntimeCommand::Navigate {
                entry: target_request,
            })
            .await
            .expect("navigate to current request");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Unchanged {
                request_prefill: Some(request),
            } if request == "target branch"
        ));
        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("navigate to terminal");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            } if state.timeline.len() == 3
        ));
        shutdown_controller(commands, controller).await;

        assert_eq!(active_entry_id(&session), target_entry.0 as i64);
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read_to_string(workspace.path().join("effect")).expect("read external effect"),
            "retained"
        );
        session.close().expect("close session");
    }

    fn replace_entry_checkpoint_state(session: &Session, entry: SessionEntryId, state: Vec<u8>) {
        let connection = session.connection_for_test().expect("lock session");
        connection
            .execute_batch("DROP TRIGGER checkpoints_are_immutable_before_update")
            .expect("allow corrupt checkpoint fixture");
        connection
            .execute(
                "UPDATE checkpoints SET arena_state = ?1 WHERE id = (SELECT checkpoint_id FROM entries WHERE id = ?2)",
                params![state, i64::try_from(entry.0).expect("entry ID fits SQLite")],
            )
            .expect("replace target checkpoint state");
    }

    fn replace_entry_checkpoint_identity(session: &Session, entry: SessionEntryId, identity: &str) {
        let connection = session.connection_for_test().expect("lock session");
        connection
            .execute_batch("DROP TRIGGER checkpoints_are_immutable_before_update")
            .expect("allow incompatible checkpoint fixture");
        connection
            .execute(
                "UPDATE checkpoints SET runtime_identity = ?1 WHERE id = (SELECT checkpoint_id FROM entries WHERE id = ?2)",
                params![identity, i64::try_from(entry.0).expect("entry ID fits SQLite")],
            )
            .expect("replace target checkpoint identity");
    }

    async fn navigation_checkpoint(session: &Session, entry: SessionEntryId) -> Checkpoint {
        session
            .writer()
            .restore_at(entry)
            .await
            .expect("restore navigation entry")
            .executable
            .expect("navigation entry is executable")
            .checkpoint
    }

    fn navigation_controller<M: Model>(
        session: &Session,
        arena: Arena,
        model: M,
        gate: ArenaRestoreTestGate,
    ) -> RuntimeController<M> {
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        RuntimeController::with_session(
            Agent::new(arena, model).with_restore_test_gate(gate),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CancellationOnlyChildRunner),
            ),
            session.writer(),
        )
    }

    #[tokio::test]
    async fn navigation_rejects_invalid_history_without_restoring_arena() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let gate = ArenaRestoreTestGate::new();
        let controller =
            navigation_controller(&session, live_arena, RejectTargetHistoryModel, gate.clone());
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Failed
        ));
        assert!(gate.attempts().is_empty());
        assert_eq!(active_entry_id(&session), old_entry.0 as i64);
        commands
            .send(RuntimeCommand::Navigate { entry: old_entry })
            .await
            .expect("navigate at unchanged entry");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Unchanged {
                request_prefill: None
            }
        ));
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_validates_target_history_with_the_current_selection() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, _old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let selections = Arc::new(Mutex::new(Vec::new()));
        let controller = navigation_controller(
            &session,
            live_arena,
            ValidatedSelectionsModel {
                selections: selections.clone(),
            },
            ArenaRestoreTestGate::new(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);
        let selection = ModelSelection::new(&TERRA, "high").expect("select Terra");

        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 1,
            })
            .await
            .expect("apply settings");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 1 }
        );
        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced { .. }
        ));
        assert_eq!(*selections.lock().unwrap(), vec![selection]);
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_does_not_compensate_an_incompatible_checkpoint() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        replace_entry_checkpoint_identity(
            &session,
            target_entry,
            "incompatible navigation runtime",
        );
        let target_checkpoint = navigation_checkpoint(&session, target_entry).await;
        let gate = ArenaRestoreTestGate::new();
        let controller = navigation_controller(
            &session,
            live_arena,
            ScriptedModel::new(Vec::new()),
            gate.clone(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Failed
        ));
        assert_eq!(gate.attempts(), vec![target_checkpoint]);
        assert_eq!(active_entry_id(&session), old_entry.0 as i64);
        commands
            .send(RuntimeCommand::Navigate { entry: old_entry })
            .await
            .expect("navigate at unchanged entry");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Unchanged {
                request_prefill: None
            }
        ));
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_recovers_after_a_compatible_checkpoint_load_failure() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        replace_entry_checkpoint_state(&session, target_entry, vec![0]);
        let target_checkpoint = navigation_checkpoint(&session, target_entry).await;
        let old_checkpoint = navigation_checkpoint(&session, old_entry).await;
        let gate = ArenaRestoreTestGate::new();
        let controller = navigation_controller(
            &session,
            live_arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "assert branch_value == 'old'\nbranch_value".to_owned(),
                },
                ModelAction::Message {
                    text: "old branch remains active".to_owned(),
                },
            ]),
            gate.clone(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Failed
        ));
        assert_eq!(gate.attempts(), vec![target_checkpoint, old_checkpoint]);
        assert_eq!(active_entry_id(&session), old_entry.0 as i64);
        commands
            .send(RuntimeCommand::Submit {
                request: "continue old branch".to_owned(),
            })
            .await
            .expect("submit after recovered navigation");
        let events_after_recovery = events_until_settled(&mut events).await;
        assert!(events_after_recovery.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { execution, .. },
                    ..
                },
                ..
            } if result_head(execution) == "'old'"
        )));
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_disables_when_checkpoint_compensation_fails() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        replace_entry_checkpoint_state(&session, target_entry, vec![0]);
        let gate = ArenaRestoreTestGate::new();
        gate.fail_attempt(2);
        let controller = navigation_controller(
            &session,
            live_arena,
            ScriptedModel::new(Vec::new()),
            gate.clone(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Failed
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            }
        );
        assert_eq!(
            gate.attempts(),
            vec![
                session
                    .writer()
                    .restore_at(target_entry)
                    .await
                    .expect("restore target")
                    .executable
                    .expect("target history is executable")
                    .checkpoint,
                session
                    .writer()
                    .restore_at(old_entry)
                    .await
                    .expect("restore old")
                    .executable
                    .expect("old history is executable")
                    .checkpoint,
            ]
        );
        assert_eq!(active_entry_id(&session), old_entry.0 as i64);
        commands
            .send(RuntimeCommand::Submit {
                request: "must remain disabled".to_owned(),
            })
            .await
            .expect("submit disabled request");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            }
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_compensates_after_an_active_leaf_conflict() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let target_checkpoint = navigation_checkpoint(&session, target_entry).await;
        let old_checkpoint = navigation_checkpoint(&session, old_entry).await;
        let gate = ArenaRestoreTestGate::new();
        gate.pause_attempt(1);
        let controller = navigation_controller(
            &session,
            live_arena,
            ScriptedModel::new(Vec::new()),
            gate.clone(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        gate.wait_until_paused().await;
        set_active_entry_for_fixture(&session, target_entry.0 as i64);
        gate.resume();

        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Failed
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(gate.attempts(), vec![target_checkpoint, old_checkpoint]);
        assert_eq!(active_entry_id(&session), target_entry.0 as i64);
        commands
            .send(RuntimeCommand::Submit {
                request: "must remain disabled".to_owned(),
            })
            .await
            .expect("submit disabled request");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_rejects_foreground_and_background_work() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let provider_called = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            AdmissionGatedModel {
                calls: Arc::new(AtomicUsize::new(0)),
                provider_called: provider_called.clone(),
            },
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "active request".to_owned(),
            })
            .await
            .expect("submit active request");
        provider_called.notified().await;
        commands
            .send(RuntimeCommand::Navigate {
                entry: SessionEntryId(1),
            })
            .await
            .expect("request navigation during foreground work");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::RejectedBusy
        ));
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel foreground work");
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;

        let subagents = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(CancellationOnlyChildRunner),
        );
        let controller = controller_with_subagents(
            &workspace,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await start_subagent('background')".to_owned(),
                },
                ModelAction::Message {
                    text: "foreground settled".to_owned(),
                },
            ]),
            subagents,
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "start background work".to_owned(),
            })
            .await
            .expect("submit child request");
        let settled = events_until_settled(&mut events).await;
        assert!(
            settled
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SubagentSpawned { .. })),
            "child was not admitted: {settled:?}"
        );
        commands
            .send(RuntimeCommand::Navigate {
                entry: SessionEntryId(1),
            })
            .await
            .expect("request navigation during child work");
        let outcome = next_navigation_outcome(&mut events).await;
        assert!(
            matches!(outcome, super::NavigationOutcome::RejectedBusy),
            "unexpected navigation outcome: {outcome:?}"
        );
        commands
            .send(RuntimeCommand::CancelBackground)
            .await
            .expect("cancel background work");
        shutdown_controller(commands, controller).await;
    }

    #[derive(Clone, Copy)]
    enum BusyCommand {
        Navigation,
        TreeRequest,
    }

    async fn assert_command_is_rejected_busy(
        command: BusyCommand,
        commands: &mpsc::Sender<RuntimeCommand>,
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) {
        match command {
            BusyCommand::Navigation => {
                commands
                    .send(RuntimeCommand::Navigate {
                        entry: SessionEntryId(1),
                    })
                    .await
                    .expect("request navigation while work is active");
                assert!(matches!(
                    next_navigation_outcome(events).await,
                    super::NavigationOutcome::RejectedBusy
                ));
            }
            BusyCommand::TreeRequest => {
                commands
                    .send(RuntimeCommand::RequestTree { revision: 1 })
                    .await
                    .expect("request session tree while work is active");
                assert!(matches!(
                    next_tree_outcome(events).await,
                    (1, super::TreeRequestOutcome::RejectedBusy)
                ));
            }
        }
    }

    async fn controller_with_write_gate<M: Model>(
        workspace: &tempfile::TempDir,
        gate: SessionWriteGate,
        model: M,
        subagents: SessionSubagents,
    ) -> (tempfile::TempDir, Session, RuntimeController<M>) {
        let home = tempfile::tempdir().expect("create session home");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate,
        )
        .expect("create session");
        let controller = RuntimeController::with_session(
            Agent::new(arena, model),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            subagents,
            session.writer(),
        );
        (home, session, controller)
    }

    async fn assert_each_busy_phase_rejects(command: BusyCommand) {
        let workspace = tempfile::tempdir().expect("create workspace");
        let policy = SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents());

        let request_gate = SessionWriteGate::new(SessionWriteKind::Request);
        let (_home, session, controller) = controller_with_write_gate(
            &workspace,
            request_gate.clone(),
            AdmissionGatedModel {
                calls: Arc::new(AtomicUsize::new(0)),
                provider_called: Arc::new(Notify::new()),
            },
            SubagentCoordinator::new(policy, Arc::new(CancellationOnlyChildRunner)),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold request admission".to_owned(),
            })
            .await
            .expect("submit request");
        request_gate.wait_until_started().await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        request_gate.release();
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel run");
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");

        let provider_called = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            AdmissionGatedModel {
                calls: Arc::new(AtomicUsize::new(0)),
                provider_called: provider_called.clone(),
            },
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold generation".to_owned(),
            })
            .await
            .expect("submit request");
        provider_called.notified().await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel run");
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;

        let read_gate = ReadTestGate::new();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host")
                .with_test_gate(read_gate.clone()),
        )
        .await
        .expect("create arena");
        let controller = RuntimeController::for_test(Agent::new(
            arena,
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await read('blocked')".to_owned(),
                },
                ModelAction::Message {
                    text: "unreachable".to_owned(),
                },
            ]),
        ));
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold execution".to_owned(),
            })
            .await
            .expect("submit request");
        read_gate.wait_until_read_started().await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel run");
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;

        let closure_gate = SessionWriteGate::new(SessionWriteKind::Closure);
        let (_home, session, controller) = controller_with_write_gate(
            &workspace,
            closure_gate.clone(),
            CountingMessageModel {
                calls: Arc::new(AtomicUsize::new(0)),
            },
            SubagentCoordinator::new(policy, Arc::new(CancellationOnlyChildRunner)),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold closure".to_owned(),
            })
            .await
            .expect("submit request");
        closure_gate.wait_until_started().await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        closure_gate.release();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");

        let admission_gate = SessionWriteGate::new(SessionWriteKind::TaskAdmission);
        let (_home, session, controller) = controller_with_write_gate(
            &workspace,
            admission_gate.clone(),
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await start_subagent('hold admission')".to_owned(),
                },
                ModelAction::Message {
                    text: "foreground settled".to_owned(),
                },
            ]),
            complete_child_coordinator(GatedChildFactory {
                started: Arc::new(Notify::new()),
                release: Arc::new(Notify::new()),
                attempts: Vec::new(),
            }),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold child admission".to_owned(),
            })
            .await
            .expect("submit request");
        admission_gate.wait_until_started().await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        admission_gate.release();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");

        let child_started = Arc::new(Notify::new());
        let child_release = Arc::new(Notify::new());
        let (_home, session, controller) = controller_with_write_gate(
            &workspace,
            SessionWriteGate::new(SessionWriteKind::Preferences),
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await start_subagent('hold child execution')".to_owned(),
                },
                ModelAction::Message {
                    text: "foreground settled".to_owned(),
                },
            ]),
            complete_child_coordinator(GatedChildFactory {
                started: child_started.clone(),
                release: child_release.clone(),
                attempts: Vec::new(),
            }),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold child execution".to_owned(),
            })
            .await
            .expect("submit request");
        child_started.notified().await;
        events_until_settled(&mut events).await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        child_release.notify_one();
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");

        let terminal_gate = SessionWriteGate::new(SessionWriteKind::TaskTerminal);
        let child_started = Arc::new(Notify::new());
        let child_release = Arc::new(Notify::new());
        let (_home, session, controller) = controller_with_write_gate(
            &workspace,
            terminal_gate.clone(),
            ScriptedModel::new(vec![
                ModelAction::Code {
                    source: "await start_subagent('hold child terminal')".to_owned(),
                },
                ModelAction::Message {
                    text: "foreground settled".to_owned(),
                },
            ]),
            complete_child_coordinator(GatedChildFactory {
                started: child_started.clone(),
                release: child_release.clone(),
                attempts: Vec::new(),
            }),
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold child terminal".to_owned(),
            })
            .await
            .expect("submit request");
        child_started.notified().await;
        events_until_settled(&mut events).await;
        child_release.notify_one();
        terminal_gate.wait_until_started().await;
        assert_command_is_rejected_busy(command, &commands, &mut events).await;
        terminal_gate.release();
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_rejects_each_busy_phase() {
        assert_each_busy_phase_rejects(BusyCommand::Navigation).await;
    }

    #[tokio::test]
    async fn tree_request_rejects_each_busy_phase() {
        assert_each_busy_phase_rejects(BusyCommand::TreeRequest).await;
    }

    #[tokio::test]
    async fn navigation_failure_preserves_selection_and_latches_storage() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, _root, old_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let target_checkpoint = navigation_checkpoint(&session, target_entry).await;
        let old_checkpoint = navigation_checkpoint(&session, old_entry).await;
        let gate = ArenaRestoreTestGate::new();
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch(
                "CREATE TRIGGER reject_navigation_selection BEFORE UPDATE OF active_entry_id ON session WHEN NEW.active_entry_id != OLD.active_entry_id BEGIN SELECT RAISE(ABORT, 'reject navigation selection'); END;",
            )
            .expect("reject navigation selection");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let controller = RuntimeController::with_session(
            Agent::new(live_arena, ScriptedModel::new(Vec::new()))
                .with_restore_test_gate(gate.clone()),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CancellationOnlyChildRunner),
            ),
            session.writer(),
        );
        let before_context = session
            .writer()
            .model_history()
            .expect("read context before rejected navigation");
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Failed
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        assert_eq!(active_entry_id(&session), old_entry.0 as i64);
        assert_eq!(
            session
                .writer()
                .model_history()
                .expect("read context after rejected navigation"),
            before_context
        );
        assert_eq!(gate.attempts(), vec![target_checkpoint, old_checkpoint]);
        commands
            .send(RuntimeCommand::Navigate {
                entry: target_entry,
            })
            .await
            .expect("request navigation after storage failure");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::RejectedReadOnly
        ));
        commands
            .send(RuntimeCommand::Submit {
                request: "after storage failure".to_owned(),
            })
            .await
            .expect("submit after storage failure");
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            }
        );
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn navigation_waits_for_selection_persistence_before_later_commands() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Selection);
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let mut live_arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            live_arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let root = SessionEntryId(active_entry_id(&session) as u64);
        let (_, _, _) = persist_navigation_branch(
            &session,
            &mut live_arena,
            "selected branch",
            "branch_value = 'selected'",
            "selected final",
        )
        .await;
        let before_context = session
            .writer()
            .model_history()
            .expect("read selected branch context");
        let controller = RuntimeController::with_session(
            Agent::new(
                live_arena,
                CountingMessageModel {
                    calls: Arc::new(AtomicUsize::new(0)),
                },
            ),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CancellationOnlyChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate { entry: root })
            .await
            .expect("request navigation");
        gate.wait_until_started().await;
        assert!(events.try_recv().is_err());
        assert_eq!(
            session
                .writer()
                .model_history()
                .expect("read context while selection is pending"),
            before_context
        );
        commands
            .send(RuntimeCommand::Submit {
                request: "queued after navigation".to_owned(),
            })
            .await
            .expect("queue request after navigation");
        assert!(events.try_recv().is_err());
        gate.release();
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            } if state.timeline.is_empty()
        ));
        assert!(
            !session
                .writer()
                .model_history()
                .expect("read published root context")
                .entries()
                .iter()
                .any(|entry| matches!(entry, ModelContextEntry::UserRequest { text } if text == "selected branch"))
        );
        let submitted = events_until_settled(&mut events).await;
        assert!(submitted.iter().any(|event| matches!(
            event,
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::UserRequest { request, .. },
                    ..
                },
                ..
            } if request == "queued after navigation"
        )));
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session")
                .query_row(
                    "SELECT parent_id FROM entries WHERE kind = 'user_request' ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read queued request parent"),
            root.0 as i64
        );
        shutdown_controller(commands, controller).await;

        let selected_after_submission = SessionEntryId(active_entry_id(&session) as u64);
        let id = session.id().to_owned();
        session.close().expect("close session");
        let reopened = Session::open(home.path(), &id).expect("reopen session");
        assert_eq!(
            reopened
                .restore()
                .expect("restore persisted navigation")
                .effective_entry,
            selected_after_submission
        );
        reopened.close().expect("close reopened session");
    }

    #[tokio::test]
    async fn navigation_short_circuits_a_current_request_before_arena_restore() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let gate = SessionWriteGate::new(SessionWriteKind::Selection);
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("create tool host"),
        )
        .await
        .expect("create arena");
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            gate.clone(),
        )
        .expect("create session");
        let root = SessionEntryId(active_entry_id(&session) as u64);
        let writer = session.writer();
        let run = writer
            .commit_request("prefilled request".to_owned(), selection)
            .await
            .expect("commit request")
            .run_id;
        let request = SessionEntryId(active_entry_id(&session) as u64);
        writer
            .close_run(
                run,
                None,
                || PersistedTerminal::Cancelled {
                    evidence: None,
                    attempts: Vec::new(),
                },
                None,
            )
            .await
            .expect("close request");
        set_active_entry_for_fixture(&session, root.0 as i64);
        drop(writer);
        let controller = RuntimeController::with_session(
            Agent::new(arena, ScriptedModel::new(Vec::new())),
            selection,
            crate::config::ready_snapshot_for_test().settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
                Arc::new(CancellationOnlyChildRunner),
            ),
            session.writer(),
        );
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::Navigate { entry: request })
            .await
            .expect("request current request");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Unchanged {
                request_prefill: Some(request),
            } if request == "prefilled request"
        ));
        assert!(!gate.has_started());
        assert_eq!(active_entry_id(&session), root.0 as i64);
        shutdown_controller(commands, controller).await;
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn tree_request_returns_catalogue_only_when_fully_settled() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let (_home, session, live_arena, root, active_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let before = session.restore().expect("snapshot selected session");
        let database_before = session_database_snapshot(&session);
        let controller = navigation_controller(
            &session,
            live_arena,
            ScriptedModel::new(Vec::new()),
            ArenaRestoreTestGate::new(),
        );
        let records_before = controller.records.clone();
        let (commands, mut events, _progress, controller) = start_controller(controller);

        commands
            .send(RuntimeCommand::RequestTree { revision: 72 })
            .await
            .expect("request session tree");
        let catalogue = match next_event(&mut events).await {
            RuntimeEvent::TreeCatalogue {
                revision: 72,
                outcome: super::TreeRequestOutcome::Catalogue(catalogue),
            } => catalogue,
            event => panic!("unexpected tree response: {event:?}"),
        };
        assert_eq!(catalogue.active_entry, active_entry);
        assert_eq!(catalogue.active_entry, before.effective_entry);
        assert_eq!(catalogue.active_entry, before.observed_active_entry);
        assert_eq!(catalogue.entries.len(), 7);
        assert!(matches!(
            catalogue.entries[0].kind,
            crate::session::SessionCatalogueKind::Start
        ));
        assert!(matches!(
            &catalogue.entries[1].kind,
            crate::session::SessionCatalogueKind::Request { text } if text == "old branch"
        ));
        assert!(matches!(
            catalogue.entries[2].kind,
            crate::session::SessionCatalogueKind::Execution {
                commentary: None,
                ref execution,
                ..
            } if execution.tool_calls.is_empty()
        ));
        assert!(matches!(
            &catalogue.entries[3].kind,
            crate::session::SessionCatalogueKind::Final { text } if text == "old branch final"
        ));
        assert!(matches!(
            &catalogue.entries[4].kind,
            crate::session::SessionCatalogueKind::Request { text } if text == "target branch"
        ));
        assert!(matches!(
            catalogue.entries[5].kind,
            crate::session::SessionCatalogueKind::Execution {
                commentary: None,
                ref execution,
                ..
            } if execution.tool_calls.is_empty()
        ));
        assert!(matches!(
            &catalogue.entries[6].kind,
            crate::session::SessionCatalogueKind::Final { text } if text == "target branch final"
        ));
        assert_eq!(catalogue.entries[0].entry_id, root);
        assert_eq!(catalogue.entries[3].entry_id, active_entry);
        assert_eq!(catalogue.entries[6].entry_id, target_entry);
        assert_eq!(catalogue.entries[1].parent_id, Some(root));
        assert_eq!(catalogue.entries[4].parent_id, Some(root));

        let after = session.restore().expect("snapshot selected session");
        assert_restore_unchanged(&before, &after);
        assert_eq!(session_database_snapshot(&session), database_before);
        assert_eq!(
            shutdown_controller(commands, controller).await,
            records_before
        );
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn idle_tree_and_navigation_commands_drain_finished_before_responding() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select default model");

        for command in [
            RuntimeCommand::RequestTree { revision: 1 },
            RuntimeCommand::Navigate {
                entry: SessionEntryId(1),
            },
        ] {
            let mut controller = controller(&workspace, Vec::new()).await;
            let (coordinator_events, receiver) = mpsc::unbounded_channel();
            controller.subagent_events = Some(receiver);
            let record = TaskRecord::for_test("00", 2, selection, None, 1, Vec::new());
            coordinator_events
                .send(CoordinatorEvent::Finished {
                    record: record.clone(),
                })
                .expect("queue finished child");
            let (event_sender, mut events) = mpsc::unbounded_channel();
            let (progress_sender, _) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            let mut progress = ProgressPublisher::new(progress_sender);
            let mut shutdown_requested = false;
            let tree_request = matches!(command, RuntimeCommand::RequestTree { .. });

            controller
                .handle_command(
                    command,
                    &event_sender,
                    &mut progress,
                    &mut shutdown_requested,
                )
                .await;

            assert!(matches!(
                events.try_recv(),
                Ok(RuntimeEvent::SubagentRecord { record: emitted }) if emitted == record
            ));
            assert!(matches!(
                events.try_recv(),
                Ok(RuntimeEvent::SubagentFinished { record: emitted }) if emitted == record
            ));
            if tree_request {
                assert!(matches!(
                    events.try_recv(),
                    Ok(RuntimeEvent::TreeCatalogue { revision: 1, .. })
                ));
            } else {
                assert!(matches!(
                    events.try_recv(),
                    Ok(RuntimeEvent::NavigationCompleted {
                        outcome: super::NavigationOutcome::Unchanged { .. }
                    })
                ));
            }
            assert!(events.try_recv().is_err());

            coordinator_events
                .send(CoordinatorEvent::OperationSettled {
                    id: record.id().clone(),
                })
                .expect("queue operation settlement");
            controller
                .drain_coordinator_events(&event_sender, &mut progress)
                .await;
            assert!(matches!(
                events.try_recv(),
                Ok(RuntimeEvent::SubagentOperationSettled { ref id }) if id == record.id()
            ));
            assert!(events.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn tree_request_busy_reply_is_not_deferred_until_settlement() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let provider_called = Arc::new(Notify::new());
        let controller = controller_with_model(
            &workspace,
            AdmissionGatedModel {
                calls: Arc::new(AtomicUsize::new(0)),
                provider_called: provider_called.clone(),
            },
        )
        .await;
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::Submit {
                request: "hold generation".to_owned(),
            })
            .await
            .expect("submit request");
        provider_called.notified().await;
        commands
            .send(RuntimeCommand::RequestTree { revision: 73 })
            .await
            .expect("request session tree while busy");
        assert!(matches!(
            next_tree_outcome(&mut events).await,
            (73, super::TreeRequestOutcome::RejectedBusy)
        ));
        commands
            .send(RuntimeCommand::Cancel { correction: false })
            .await
            .expect("cancel request");
        let settlement = events_until_settled(&mut events).await;
        assert!(
            !settlement
                .iter()
                .any(|event| matches!(event, RuntimeEvent::TreeCatalogue { .. }))
        );
        assert!(events.try_recv().is_err());
        shutdown_controller(commands, controller).await;
    }

    #[tokio::test]
    async fn tree_request_distinguishes_read_only_and_catalogue_failure() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let mut controller =
            controller_with_model(&workspace, ScriptedModel::new(Vec::new())).await;
        controller.disabled_reason = Some(RuntimeDisabledReason::SessionRestoreFailed);
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::RequestTree { revision: 74 })
            .await
            .expect("request session tree while read-only");
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::TreeCatalogue {
                revision: 74,
                outcome: super::TreeRequestOutcome::RejectedReadOnly,
            }
        ));
        shutdown_controller(commands, controller).await;

        let (_home, session, live_arena, _root, active_entry, target_entry) =
            navigation_fixture(&workspace).await;
        let target_request: i64 = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT parent_id FROM entries WHERE id = (SELECT parent_id FROM entries WHERE id = ?1)",
                params![i64::try_from(target_entry.0).expect("target entry fits SQLite")],
                |row| row.get(0),
            )
            .expect("read target request");
        session
            .connection_for_test()
            .expect("lock session")
            .execute_batch("DROP TRIGGER entries_are_immutable_before_update")
            .expect("remove entry immutability trigger");
        session
            .connection_for_test()
            .expect("lock session")
            .execute(
                "UPDATE entries SET payload_json = '{}' WHERE id = ?1",
                params![target_request],
            )
            .expect("corrupt non-selected request");
        let before = session.restore().expect("restore selected branch");
        assert!(before.executable.is_some());
        let controller = navigation_controller(
            &session,
            live_arena,
            ScriptedModel::new(Vec::new()),
            ArenaRestoreTestGate::new(),
        );
        let records_before = controller.records.clone();
        let (commands, mut events, _progress, controller) = start_controller(controller);
        commands
            .send(RuntimeCommand::RequestTree { revision: 75 })
            .await
            .expect("request corrupt session tree");
        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::TreeCatalogue {
                revision: 75,
                outcome: super::TreeRequestOutcome::Failed,
            }
        ));
        let after = session.restore().expect("restore selected branch");
        assert_restore_unchanged(&before, &after);
        assert!(after.executable.is_some());
        commands
            .send(RuntimeCommand::Navigate {
                entry: active_entry,
            })
            .await
            .expect("navigate to unchanged selected entry");
        assert!(matches!(
            next_navigation_outcome(&mut events).await,
            super::NavigationOutcome::Unchanged {
                request_prefill: None,
            }
        ));
        assert_eq!(
            shutdown_controller(commands, controller).await,
            records_before
        );
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn tree_request_observes_latched_storage_failure_before_loading() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy::from(crate::config::ready_snapshot_for_test().subagents()),
            Arc::new(super::UnavailableChildRunner),
        );
        let mut controller = controller_with_subagents(
            &workspace,
            ScriptedModel::new(Vec::new()),
            coordinator.clone(),
        )
        .await;
        coordinator.latch_storage_failure().await;
        let (events_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut progress = ProgressPublisher::new(progress_sender);
        let mut shutdown_requested = false;

        controller
            .handle_command(
                RuntimeCommand::RequestTree { revision: 76 },
                &events_sender,
                &mut progress,
                &mut shutdown_requested,
            )
            .await;

        assert!(matches!(
            next_event(&mut events).await,
            RuntimeEvent::TreeCatalogue {
                revision: 76,
                outcome: super::TreeRequestOutcome::RejectedReadOnly,
            }
        ));
        assert!(events.try_recv().is_err());
        assert_eq!(controller.disabled_reason, None);
        assert!(controller.active.is_none());
    }

    #[tokio::test]
    async fn tree_request_preserves_correlation_and_command_ordering() {
        let workspace = tempfile::tempdir().expect("create workspace");
        let controller = controller_with_model(&workspace, ScriptedModel::new(Vec::new())).await;
        let selection = ModelSelection::new(&TERRA, TERRA.default_thinking).expect("select Terra");
        let (commands, mut events, _progress, controller) = start_controller(controller);
        for revision in [76, 77] {
            commands
                .send(RuntimeCommand::RequestTree { revision })
                .await
                .expect("request session tree");
        }
        commands
            .send(RuntimeCommand::ApplySettings {
                selection,
                revision: 78,
            })
            .await
            .expect("apply settings");
        commands
            .send(RuntimeCommand::Shutdown)
            .await
            .expect("request shutdown");

        for revision in [76, 77] {
            assert!(matches!(
                next_event(&mut events).await,
                RuntimeEvent::TreeCatalogue {
                    revision: actual_revision,
                    outcome: super::TreeRequestOutcome::Catalogue(_),
                } if actual_revision == revision
            ));
        }
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SettingsAccepted { revision: 78 }
        );
        assert!(
            timeout(TEST_TIMEOUT, controller)
                .await
                .expect("controller should stop")
                .expect("controller task should not panic")
                .is_empty()
        );
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn returned_records_are_exactly_the_committed_record_events() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let (commands, mut events, _progress, controller) =
            start_controller(default_controller(&workspace).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(records, committed_records(&events));
        assert_eq!(records.len(), 4);
        assert_eq!(events.len(), 7);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, RuntimeEvent::ExecutionStarted { .. }))
                .count(),
            2
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolUpdated { .. }))
        );
    }
}
