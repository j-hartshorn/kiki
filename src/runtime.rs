use std::collections::BTreeMap;

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentError, AgentProgress, AgentRunOutcome, TerminalEvidence};
use crate::arena::{Execution, RunPreview, ToolCall, ToolCallSummary, ToolCallToken, ToolName};
#[cfg(test)]
use crate::config::{DEFAULT_MODEL, ModelConfiguration};
use crate::config::{ModelSelection, settings::SettingsContext};
use crate::model::{AttemptAccounting, Model, ModelProgress};
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
    ApplySettings {
        selection: ModelSelection,
        revision: u64,
    },
    Cancel,
    CancelBackground,
    Shutdown,
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
    },
    CompletedExecution {
        step_id: ModelStepId,
        source: String,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeDisabledReason {
    OpenAiAuthentication,
    ArenaCheckpointRecoveryFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredRecord {
    pub(crate) run_id: RunId,
    pub(crate) record: Record,
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
    RunSettled {
        run_id: RunId,
    },
    SettingsAccepted {
        revision: u64,
    },
    SubmissionRejectedBusy {
        active_run_id: RunId,
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
const FAILED_RUN_MESSAGE_LIMIT: usize = 4 * 1024;

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
    next_run_id: u64,
    next_model_step_id: u64,
    settings: watch::Sender<ModelSelection>,
    accepted_thinking: AcceptedThinking,
    subagents: SessionSubagents,
    subagent_events: Option<mpsc::UnboundedReceiver<CoordinatorEvent>>,
    subagent_record_revisions: BTreeMap<TaskId, u64>,
    subagent_status_revisions: BTreeMap<TaskId, u64>,
    active_caller: Option<Caller>,
    active: Option<ActiveRun<M>>,
    trace: Trace,
}

struct ActiveRun<M: Model> {
    run_id: RunId,
    cancellation: CancellationToken,
    worker: mpsc::UnboundedReceiver<WorkerMessage<M>>,
    progress: mpsc::Receiver<ProgressEnvelope<WorkerProgress>>,
    progress_open: bool,
    cancellation_requested: bool,
    step_ids: Vec<ModelStepId>,
}

#[allow(clippy::large_enum_variant)]
enum WorkerMessage<M: Model> {
    ToolSettled {
        step_ordinal: u64,
        token: ToolCallToken,
        call: ToolCall,
    },
    ExecutionCompleted {
        step_ordinal: u64,
        source: String,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    },
    Finished {
        agent: Agent<M>,
        result: Result<AgentRunOutcome, AgentError>,
        final_step_ordinal: u64,
        progress_gap: bool,
    },
}

struct WorkerProgress {
    step_ordinal: u64,
    progress: AgentProgress,
}

#[allow(clippy::large_enum_variant)]
enum ControllerInput<M: Model> {
    Command(Option<RuntimeCommand>),
    Worker(Option<WorkerMessage<M>>),
    Progress(Option<ProgressEnvelope<WorkerProgress>>),
    SubagentsChanged,
    CoordinatorEvent(Option<CoordinatorEvent>),
    SubagentDeadline,
}

pub(crate) fn control_command(command: &RuntimeCommand) -> Option<ControlCommand> {
    match command {
        RuntimeCommand::Cancel => Some(ControlCommand::Cancel),
        RuntimeCommand::CancelBackground => Some(ControlCommand::CancelBackground),
        RuntimeCommand::Shutdown => Some(ControlCommand::Shutdown),
        RuntimeCommand::Submit { .. } | RuntimeCommand::ApplySettings { .. } => None,
    }
}

fn root_settlement(record: &Record) -> Settlement {
    match record {
        Record::FinalMessage { .. } => Settlement::Succeeded,
        Record::CancelledRun { .. } => Settlement::Cancelled,
        Record::FailedRun { .. } => Settlement::Failed,
        Record::UserRequest { .. } | Record::CompletedExecution { .. } => {
            unreachable!("only terminal root records settle runs")
        }
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
        AgentProgress::ModelStepStarted
        | AgentProgress::ExecutionStarted { .. }
        | AgentProgress::ExecutionCompleted { .. } => 0,
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
        RuntimeEvent::RunStarted { .. }
        | RuntimeEvent::ModelStepStarted { .. }
        | RuntimeEvent::ExecutionStarted { .. }
        | RuntimeEvent::RecordAppended { .. }
        | RuntimeEvent::SubagentSpawned { .. }
        | RuntimeEvent::SubagentStatus { .. }
        | RuntimeEvent::SubagentRecord { .. }
        | RuntimeEvent::SubagentFinished { .. }
        | RuntimeEvent::RunSettled { .. }
        | RuntimeEvent::SettingsAccepted { .. }
        | RuntimeEvent::SubmissionRejectedBusy { .. }
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

impl<M: Model> RuntimeController<M> {
    pub(crate) fn new(
        agent: Agent<M>,
        settings: watch::Sender<ModelSelection>,
        persisted_settings: SettingsContext,
        subagents: SessionSubagents,
    ) -> Self {
        let accepted_thinking = AcceptedThinking::new(*settings.borrow(), persisted_settings);
        Self {
            agent: Some(agent),
            disabled_reason: None,
            records: Vec::new(),
            next_run_id: 1,
            next_model_step_id: 1,
            settings,
            accepted_thinking,
            subagent_events: subagents.take_event_receiver(),
            subagents,
            subagent_record_revisions: BTreeMap::new(),
            subagent_status_revisions: BTreeMap::new(),
            active_caller: None,
            active: None,
            trace: Trace::disabled(),
        }
    }

    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
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
    fn for_test(agent: Agent<M>) -> Self {
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("default model selection must be valid");
        let (settings, _) = watch::channel(selection);
        let snapshot = crate::config::ready_snapshot_for_test();
        Self::new(
            agent,
            settings,
            snapshot.settings().clone(),
            SubagentCoordinator::new(
                SubagentPolicy::from(snapshot.subagents()),
                std::sync::Arc::new(UnavailableChildRunner),
            ),
        )
    }

    #[cfg(test)]
    fn with_settings(mut self, settings: watch::Sender<ModelSelection>) -> Self {
        self.accepted_thinking = AcceptedThinking::new(
            *settings.borrow(),
            self.accepted_thinking.persisted_settings(),
        );
        self.settings = settings;
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
                    self.handle_worker_message(message, &events, &mut progress);
                }
                ControllerInput::Worker(None) => {
                    shutdown_requested = true;
                    self.request_shutdown().await;
                    self.handle_worker_closed(&events, &mut progress);
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
                if let Some(active) = &self.active {
                    self.emit(
                        events,
                        RuntimeEvent::SubmissionRejectedBusy {
                            active_run_id: active.run_id,
                        },
                    );
                } else if let Some(reason) = &self.disabled_reason {
                    self.emit(
                        events,
                        RuntimeEvent::SubmissionRejectedDisabled {
                            reason: reason.clone(),
                        },
                    );
                } else {
                    self.start_run(request, events, progress).await;
                }
            }
            RuntimeCommand::ApplySettings {
                selection,
                revision,
            } => {
                self.accepted_thinking.accept(selection);
                self.settings.send_replace(selection);
                self.emit(events, RuntimeEvent::SettingsAccepted { revision });
            }
            RuntimeCommand::Cancel => self.request_active_cancellation(),
            RuntimeCommand::CancelBackground => self.request_background_cancellation().await,
            RuntimeCommand::Shutdown => {
                *shutdown_requested = true;
                self.request_shutdown().await;
            }
        }
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
                if record.revision() < revision {
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

    async fn start_run(
        &mut self,
        request: String,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        let caller = self.subagents.root_caller().await;
        let run_id = self.next_run_id();
        self.trace
            .emit(|| TraceEvent::RootRunStarted { run_id: run_id.0 });
        self.publish_progress(progress, RuntimeEvent::RunStarted { run_id }, false);
        self.append_record(
            events,
            run_id,
            Record::UserRequest {
                request: request.clone(),
            },
            false,
        );

        let mut agent = self
            .agent
            .take()
            .expect("an idle runtime controller owns its agent");
        agent.set_invocation_context(
            AgentInvocationContext::new(caller.clone(), self.subagents.clone())
                .with_accepted_thinking(self.accepted_thinking.clone()),
        );
        let cancellation = CancellationToken::new();
        let cancellation_for_worker = cancellation.clone();
        let (sender, worker) = mpsc::unbounded_channel();
        let (progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let worker_task = tokio::spawn(async move {
            let mut agent = agent;
            let (result, final_step_ordinal, progress_gap) = {
                let mut progress_sender = ProgressPublisher::new(progress_sender);
                let completion_sender = sender.clone();
                let mut step_ordinal: u64 = 0;
                let mut progress = |agent_progress| match agent_progress {
                    AgentProgress::ModelStepStarted => {
                        step_ordinal = step_ordinal
                            .checked_add(1)
                            .expect("model step ordinal overflowed");
                        progress_sender.publish(
                            WorkerProgress {
                                step_ordinal,
                                progress: AgentProgress::ModelStepStarted,
                            },
                            false,
                            0,
                        );
                    }
                    AgentProgress::ToolUpdated { token, call }
                        if call.tool == ToolName::Run
                            && !matches!(call.summary, ToolCallSummary::Pending) =>
                    {
                        let _ = completion_sender.send(WorkerMessage::ToolSettled {
                            step_ordinal,
                            token,
                            call,
                        });
                    }
                    AgentProgress::ExecutionCompleted {
                        source,
                        reasoning_summary,
                        execution,
                        attempts,
                    } => {
                        let _ = completion_sender.send(WorkerMessage::ExecutionCompleted {
                            step_ordinal,
                            source,
                            reasoning_summary,
                            execution,
                            attempts,
                        });
                    }
                    agent_progress => {
                        let payload_bytes = agent_progress_payload_bytes(&agent_progress);
                        progress_sender.publish(
                            WorkerProgress {
                                step_ordinal,
                                progress: agent_progress,
                            },
                            false,
                            payload_bytes,
                        );
                    }
                };
                let result = agent
                    .run_with_progress(request, &cancellation_for_worker, &mut progress)
                    .await;
                let progress_gap = progress_sender.take_pending_gap();
                (result, step_ordinal, progress_gap)
            };
            let _ = sender.send(WorkerMessage::Finished {
                agent,
                result,
                final_step_ordinal,
                progress_gap,
            });
        });
        drop(worker_task);
        self.active_caller = Some(caller);
        self.active = Some(ActiveRun {
            run_id,
            cancellation,
            worker,
            progress: worker_progress,
            progress_open: true,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
    }

    fn request_active_cancellation(&mut self) {
        if let Some(active) = &mut self.active {
            self.trace
                .emit(|| TraceEvent::ForegroundCancellationRequested);
            active.cancellation_requested = true;
            active.cancellation.cancel();
        }
    }

    async fn request_background_cancellation(&mut self) {
        self.trace
            .emit(|| TraceEvent::BackgroundCancellationRequested);
        self.subagents
            .request_background_cancellation(self.active_caller.as_ref())
            .await;
        self.trace
            .emit(|| TraceEvent::BackgroundCancellationReturned);
    }

    async fn request_shutdown(&mut self) {
        self.request_active_cancellation();
        self.request_background_cancellation().await;
    }

    fn handle_worker_message(
        &mut self,
        message: WorkerMessage<M>,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        match message {
            WorkerMessage::ToolSettled {
                step_ordinal,
                token,
                call,
            } => {
                let Some(step_id) = self.step_id_for(step_ordinal) else {
                    return;
                };
                self.emit(
                    events,
                    RuntimeEvent::ToolUpdated {
                        run_id: self.active_run_id(),
                        step_id,
                        token,
                        call,
                    },
                );
            }
            WorkerMessage::ExecutionCompleted {
                step_ordinal,
                source,
                reasoning_summary,
                execution,
                attempts,
            } => {
                let Some(step_id) = self.step_id_for(step_ordinal) else {
                    return;
                };
                self.append_record(
                    events,
                    self.active_run_id(),
                    Record::CompletedExecution {
                        step_id,
                        source,
                        reasoning_summary,
                        execution,
                        attempts,
                    },
                    false,
                );
            }
            WorkerMessage::Finished {
                mut agent,
                result,
                final_step_ordinal,
                progress_gap,
            } => {
                self.drain_worker_progress(progress);
                let run_id = self.active_run_id();
                let cancellation_requested = self
                    .active
                    .as_ref()
                    .expect("worker finish requires an active run")
                    .cancellation_requested;
                let disabled_reason = match &result {
                    Err(error) if error.is_recovery_failed() => {
                        Some(RuntimeDisabledReason::ArenaCheckpointRecoveryFailed)
                    }
                    Err(error) if !cancellation_requested && error.is_authentication() => {
                        Some(RuntimeDisabledReason::OpenAiAuthentication)
                    }
                    _ => None,
                };
                let record = match result {
                    Err(error) if error.is_recovery_failed() => Record::FailedRun {
                        message: error.to_string(),
                        terminal: TerminalDetails::new(
                            error.terminal_evidence(),
                            error.attempts().to_vec(),
                        ),
                    },
                    Ok(AgentRunOutcome::FinalMessageStaged) if cancellation_requested => {
                        let final_message = agent
                            .discard_staged_final_message()
                            .expect("cancelled staged final message must be discarded");
                        Record::CancelledRun {
                            terminal: TerminalDetails::new(
                                Some(final_message.generation_evidence()),
                                final_message.attempts().to_vec(),
                            ),
                        }
                    }
                    Err(error) if cancellation_requested || error.is_cancelled() => {
                        Record::CancelledRun {
                            terminal: TerminalDetails::new(
                                error.terminal_evidence(),
                                error.attempts().to_vec(),
                            ),
                        }
                    }
                    Ok(AgentRunOutcome::FinalMessageStaged) => {
                        let final_message = agent
                            .admit_staged_final_message()
                            .expect("successful agent outcome must stage a final message");
                        Record::FinalMessage {
                            step_id: self
                                .step_id_for(final_step_ordinal)
                                .expect("a final message requires a model step"),
                            text: final_message.text().to_owned(),
                            reasoning_summary: final_message.reasoning_summary().to_owned(),
                            terminal: TerminalDetails::new(None, final_message.attempts().to_vec()),
                        }
                    }
                    Err(error) => Record::FailedRun {
                        message: error.to_string(),
                        terminal: TerminalDetails::new(
                            error.terminal_evidence(),
                            error.attempts().to_vec(),
                        ),
                    },
                };
                let settlement = root_settlement(&record);
                if let Some(reason) = disabled_reason {
                    self.disabled_reason = Some(reason.clone());
                    self.append_record(
                        events,
                        run_id,
                        record,
                        progress_gap || progress.take_pending_gap(),
                    );
                    self.emit(events, RuntimeEvent::RuntimeDisabled { reason });
                } else {
                    self.agent = Some(agent);
                    self.append_record(
                        events,
                        run_id,
                        record,
                        progress_gap || progress.take_pending_gap(),
                    );
                }
                self.trace.emit(|| TraceEvent::RootRunSettled {
                    run_id: run_id.0,
                    outcome: settlement,
                });
                self.emit(events, RuntimeEvent::RunSettled { run_id });
                self.active = None;
                self.active_caller = None;
            }
        }
    }

    fn handle_worker_closed(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        let run_id = self.active_run_id();
        let cancellation_requested = self
            .active
            .as_ref()
            .expect("worker closure requires an active run")
            .cancellation_requested;
        let record = if cancellation_requested {
            Record::CancelledRun {
                terminal: TerminalDetails::new(None, Vec::new()),
            }
        } else {
            Record::FailedRun {
                message: "agent worker exited without returning its agent".to_owned(),
                terminal: TerminalDetails::new(None, Vec::new()),
            }
        };
        let settlement = root_settlement(&record);
        self.append_record(events, run_id, record, progress.take_pending_gap());
        self.trace.emit(|| TraceEvent::RootRunSettled {
            run_id: run_id.0,
            outcome: settlement,
        });
        self.emit(events, RuntimeEvent::RunSettled { run_id });
        self.active = None;
        self.active_caller = None;
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
            step_ordinal,
            progress: agent_progress,
        } = progress_envelope.item;
        let Some(step_id) = self.step_id_for(step_ordinal) else {
            return;
        };
        let run_id = self.active_run_id();
        let event = match agent_progress {
            AgentProgress::ModelStepStarted => RuntimeEvent::ModelStepStarted { run_id, step_id },
            AgentProgress::ModelProgress(progress) => RuntimeEvent::ModelProgress {
                run_id,
                step_id,
                progress,
            },
            AgentProgress::ExecutionStarted { .. } => {
                RuntimeEvent::ExecutionStarted { run_id, step_id }
            }
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

    fn step_id_for(&mut self, step_ordinal: u64) -> Option<ModelStepId> {
        let required_steps = usize::try_from(step_ordinal).ok()?;
        if required_steps == 0 {
            return None;
        }
        let current_steps = self
            .active
            .as_ref()
            .expect("worker message requires an active run")
            .step_ids
            .len();
        for _ in current_steps..required_steps {
            let step_id = self.next_model_step_id();
            self.active
                .as_mut()
                .expect("worker message requires an active run")
                .step_ids
                .push(step_id);
        }
        self.active
            .as_ref()
            .and_then(|active| active.step_ids.get(required_steps - 1))
            .copied()
    }

    fn active_run_id(&self) -> RunId {
        self.active
            .as_ref()
            .expect("worker message requires an active run")
            .run_id
    }

    fn next_run_id(&mut self) -> RunId {
        let run_id = RunId(self.next_run_id);
        self.next_run_id = self
            .next_run_id
            .checked_add(1)
            .expect("run identifier counter overflowed");
        run_id
    }

    fn next_model_step_id(&mut self) -> ModelStepId {
        let step_id = ModelStepId(self.next_model_step_id);
        self.next_model_step_id = self
            .next_model_step_id
            .checked_add(1)
            .expect("model step identifier counter overflowed");
        step_id
    }

    fn append_record(
        &mut self,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        run_id: RunId,
        mut record: Record,
        progress_gap: bool,
    ) {
        let completed_attempt_count = self
            .records
            .iter()
            .filter(|stored| stored.run_id == run_id)
            .filter_map(|stored| match &stored.record {
                Record::CompletedExecution { attempts, .. } => Some(attempts.len()),
                _ => None,
            })
            .sum();
        match &mut record {
            Record::FinalMessage { terminal, .. }
            | Record::CancelledRun { terminal }
            | Record::FailedRun { terminal, .. } => {
                terminal.attempts = terminal
                    .attempts
                    .get(completed_attempt_count..)
                    .unwrap_or_default()
                    .to_vec();
            }
            Record::UserRequest { .. } | Record::CompletedExecution { .. } => {}
        }
        if let Record::FailedRun { message, .. } = &mut record {
            *message = bounded_utf8(std::mem::take(message), FAILED_RUN_MESSAGE_LIMIT);
        }
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::sync::{Notify, mpsc, watch};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::{
        AcceptedThinking, ActiveRun, CoordinatorEvent, ModelStepId, PROGRESS_CHANNEL_CAPACITY,
        ProgressEnvelope, ProgressPublisher, Record, RunId, RuntimeCommand, RuntimeController,
        RuntimeDisabledReason, RuntimeEvent, RuntimeProgressReceiver, StoredRecord, WorkerMessage,
        runtime_event_payload_bytes,
    };
    use crate::agent::{
        Agent, ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence,
    };
    use crate::arena::{
        Arena, ArenaError, Execution, ExecutionOutcome, Observation, RecoveryTestGate, ToolCall,
        ToolCallSummary, ToolName,
    };
    use crate::config::{
        DEFAULT_MODEL, ModelSelection,
        models::{LUNA, SOL, TERRA},
    };
    use crate::model::{
        AttemptAccounting, CodeResult, Model, ModelAction as CompletedModelAction, ModelCallId,
        ModelContextEntry, ModelError, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ModelUsage, ProviderFailure, ProviderReplay, ScriptedAction as ModelAction, ScriptedModel,
    };
    use crate::subagents::{
        ChildFailure, ChildModelFactory, ChildParameters, ChildPayload, ChildRunner,
        CompleteChildRunner, RunnerFuture, SessionSubagents, StartSubagentTestGate,
        SubagentCoordinator, SubagentPolicy, TaskOutcome, TaskState,
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
        let (settings, _) = watch::channel(selection);
        RuntimeController::new(
            Agent::new(arena, model),
            settings,
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

        fn create(&self, _prompt: String, _selection: ModelSelection) -> Self::Model {
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

    impl Model for GatedChildModel {
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
                        selection: None,
                        reasoning_summary: String::new(),
                        replay: ProviderReplay::new("test", Vec::new()),
                        attempts: self.attempts.clone(),
                    }),
                }
            }
        }
    }

    struct SelectedScriptedModel {
        model: ScriptedModel,
        selection: ModelSelection,
        message_waiting: Option<Arc<Notify>>,
    }

    impl Model for SelectedScriptedModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let mut step = self.model.step(history, cancellation, progress).await?;
                if matches!(&step.action, CompletedModelAction::Message { .. })
                    && let Some(message_waiting) = &self.message_waiting
                {
                    message_waiting.notify_one();
                    cancellation.cancelled().await;
                    return Err(ModelError::Cancelled.into());
                }
                step.selection = Some(self.selection);
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
                    selection: None,
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
    }

    impl ChildModelFactory for CleanupGatedChildFactory {
        type Model = CleanupGatedChildModel;

        fn create(&self, _prompt: String, _selection: ModelSelection) -> Self::Model {
            CleanupGatedChildModel {
                started: self.started.clone(),
                cleanup_started: self.cleanup_started.clone(),
                release: self.release.clone(),
            }
        }
    }

    struct CleanupGatedChildModel {
        started: Arc<Notify>,
        cleanup_started: Arc<Notify>,
        release: Arc<Notify>,
    }

    impl Model for CleanupGatedChildModel {
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
                self.started.notify_one();
                cancellation.cancelled().await;
                self.cleanup_started.notify_one();
                self.release.notified().await;
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
        Scripted {
            model: ScriptedModel,
            selection: ModelSelection,
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
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                match self {
                    Self::Scripted {
                        model,
                        selection,
                        attempt,
                    } => {
                        let mut step = model.step(history, cancellation, progress).await?;
                        step.selection = Some(*selection);
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

        fn create(&self, _prompt: String, selection: ModelSelection) -> Self::Model {
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
                    selection,
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
                        selection,
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
        selection: watch::Receiver<ModelSelection>,
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        selections: Arc<Mutex<Vec<ModelSelection>>>,
        request: usize,
        step: usize,
    }

    impl Model for WorkflowRootModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
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
                let selection = *self.selection.borrow_and_update();
                self.selections.lock().unwrap().push(selection);
                let action = match (request, self.step) {
                    (1, 0) => CompletedModelAction::Code {
                        source: "handoff = 'before'".to_owned(),
                        call_id: ModelCallId::new("workflow-1-1"),
                    },
                    (1, 1) => CompletedModelAction::Code {
                        source: "same_execution = 'after'\nrecoverable_id = (await start_subagent('recoverable work', 'openai/gpt-5.6-terra')).id\nfailed_id = (await start_subagent('failing work', 'openai/gpt-5.6-luna')).id\nparent_id = (await start_subagent('cancellable parent')).id".to_owned(),
                        call_id: ModelCallId::new("workflow-1-2"),
                    },
                    (1, 2) => CompletedModelAction::Message {
                        text: "first foreground final".to_owned(),
                    },
                    (2, 0) => CompletedModelAction::Code {
                        source: "recoverable = await get_subagent(recoverable_id)\nawait recoverable.refresh()\nrecoverable_result = await recoverable.wait()\nassert recoverable.status.state == 'completed'\nassert recoverable_result['values']['handoff'] == handoff\nassert set(recoverable_result['values']) == {'handoff', 'same_execution'}\nfailed = await get_subagent(failed_id)\ntry:\n    await failed.wait()\nexcept RuntimeError:\n    pass\nassert failed.status.state == 'failed'\nparent = await get_subagent(parent_id)\nparent_result = await parent.wait()\ndescendant = await get_subagent(parent_result['values']['descendant_id'])\nawait descendant.refresh()\nassert descendant.status.state in ('starting', 'running')\nassert (await parent.cancel()).state == 'completed'\nassert (await descendant.refresh()).state == 'cancelled'\nrecoverable_result['values']['same_execution']".to_owned(),
                        call_id: ModelCallId::new("workflow-2-1"),
                    },
                    (2, 1) => CompletedModelAction::Message {
                        text: "second foreground final".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    selection: Some(selection),
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

    async fn next_event(events: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
        timeout(TEST_TIMEOUT, events.recv())
            .await
            .expect("runtime should emit an event before the test timeout")
            .expect("runtime event channel should remain open")
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
            RuntimeEvent::RunSettled { run_id } => format!("settled:{}", run_id.0),
            RuntimeEvent::SettingsAccepted { revision } => format!("settings:{revision}"),
            RuntimeEvent::SubmissionRejectedBusy { active_run_id } => {
                format!("busy:{}", active_run_id.0)
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
            SelectedScriptedModel {
                model: ScriptedModel::new(vec![
                    ModelAction::Code {
                        source: source.to_owned(),
                    },
                    ModelAction::Message {
                        text: "foreground complete".to_owned(),
                    },
                ]),
                selection: ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                    .unwrap(),
                message_waiting: Some(message_waiting.clone()),
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
                        record: Record::UserRequest { request },
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
                    record: Record::UserRequest { request },
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

        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
                        record: Record::UserRequest { request },
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
            Record::UserRequest { request } => format!("record:{}:user:{request}", record.run_id.0),
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
        selection: watch::Receiver<ModelSelection>,
        first_snapshotted: Arc<Notify>,
        allow_first_step: Arc<Notify>,
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        step: u8,
    }

    impl Model for GatedSelectionModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let selection = *self.selection.borrow_and_update();
                self.observed.lock().unwrap().push(selection);
                let action = match self.step {
                    0 => {
                        self.first_snapshotted.notify_one();
                        self.allow_first_step.notified().await;
                        CompletedModelAction::Code {
                            source: "value = 1".to_owned(),
                            call_id: ModelCallId::new("first"),
                        }
                    }
                    1 => CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    selection: None,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    #[tokio::test]
    async fn active_settings_update_applies_to_the_next_model_step_only() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let (settings, selection_receiver) = watch::channel(selection_a);
        let first_snapshotted = Arc::new(Notify::new());
        let allow_first_step = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            GatedSelectionModel {
                selection: selection_receiver,
                first_snapshotted: first_snapshotted.clone(),
                allow_first_step: allow_first_step.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_settings(settings);
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
        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(*observed.lock().unwrap(), [selection_a, selection_b]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::SettingsAccepted { .. }))
        );
        assert_eq!(records.len(), 3);
    }

    struct SelectionMessageModel {
        selection: watch::Receiver<ModelSelection>,
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        contexts: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
    }

    impl Model for SelectionMessageModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.observed
                    .lock()
                    .unwrap()
                    .push(*self.selection.borrow_and_update());
                self.contexts
                    .lock()
                    .unwrap()
                    .push(history.entries().to_vec());
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    selection: None,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct SnapshotAfterGateModel {
        selection: watch::Receiver<ModelSelection>,
        before_snapshot: Arc<Notify>,
        allow_snapshot: Arc<Notify>,
        observed: Arc<Mutex<Vec<ModelSelection>>>,
    }

    impl Model for SnapshotAfterGateModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.before_snapshot.notify_one();
                self.allow_snapshot.notified().await;
                self.observed
                    .lock()
                    .unwrap()
                    .push(*self.selection.borrow_and_update());
                Ok(ModelStep {
                    action: CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    selection: None,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct CancelThenMessageSelectionModel {
        selection: watch::Receiver<ModelSelection>,
        first_snapshotted: Arc<Notify>,
        observed: Arc<Mutex<Vec<ModelSelection>>>,
        step: u8,
    }

    impl Model for CancelThenMessageSelectionModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            _progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.observed
                    .lock()
                    .unwrap()
                    .push(*self.selection.borrow_and_update());
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
                    selection: None,
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
        let (settings, selection_receiver) = watch::channel(selection_a);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            SelectionMessageModel {
                selection: selection_receiver,
                observed: observed.clone(),
                contexts: contexts.clone(),
            },
        )
        .await
        .with_settings(settings);
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
                    record: Record::UserRequest { request },
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
        let (settings, selection_receiver) = watch::channel(selection_a);
        let first_snapshotted = Arc::new(Notify::new());
        let allow_first_step = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            GatedSelectionModel {
                selection: selection_receiver,
                first_snapshotted: first_snapshotted.clone(),
                allow_first_step: allow_first_step.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_settings(settings);
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
        shutdown_controller(commands, task).await;

        let observed = observed.lock().unwrap();
        assert_eq!(observed.as_slice(), [selection_a, selection_c]);
        assert!(std::ptr::eq(observed[1].model(), &LUNA));
        assert_eq!(observed[1].thinking(), "max");
    }

    #[tokio::test]
    async fn an_accepted_command_before_the_step_snapshot_applies_to_that_step() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let (settings, selection_receiver) = watch::channel(selection_a);
        let before_snapshot = Arc::new(Notify::new());
        let allow_snapshot = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            SnapshotAfterGateModel {
                selection: selection_receiver,
                before_snapshot: before_snapshot.clone(),
                allow_snapshot: allow_snapshot.clone(),
                observed: observed.clone(),
            },
        )
        .await
        .with_settings(settings);
        let (commands, mut events, _progress, task) = start_controller(controller);

        commands
            .send(RuntimeCommand::Submit {
                request: "inspect".to_owned(),
            })
            .await
            .unwrap();
        before_snapshot.notified().await;
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
        allow_snapshot.notify_one();
        events_until_settled(&mut events).await;
        shutdown_controller(commands, task).await;

        assert_eq!(*observed.lock().unwrap(), [selection_b]);
    }

    #[tokio::test]
    async fn active_settings_survive_cancellation_and_drive_a_later_run() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let (settings, selection_receiver) = watch::channel(selection_a);
        let first_snapshotted = Arc::new(Notify::new());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_model(
            &workspace,
            CancelThenMessageSelectionModel {
                selection: selection_receiver,
                first_snapshotted: first_snapshotted.clone(),
                observed: observed.clone(),
                step: 0,
            },
        )
        .await
        .with_settings(settings);
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
                    selection: None,
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

    impl Model for WaitingCancellationModel {
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

    struct AuthenticationFailureModel {
        calls: Arc<AtomicUsize>,
    }

    impl Model for AuthenticationFailureModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
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
                    selection: None,
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
                    selection: None,
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
                    }
                } else {
                    CompletedModelAction::Message {
                        text: "uncertain arena was reused".to_owned(),
                    }
                };
                Ok(ModelStep {
                    action,
                    selection: None,
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
                    },
                    1 => CompletedModelAction::Code {
                        source: "second = first + 1".to_owned(),
                        call_id: ModelCallId::new("second-accounting-code"),
                    },
                    2 => CompletedModelAction::Message {
                        text: "done".to_owned(),
                    },
                    _ => return Err(ModelError::Exhausted.into()),
                };
                self.step += 1;
                Ok(ModelStep {
                    action,
                    selection: None,
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
                    selection: None,
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
                    },
                    selection: None,
                    reasoning_summary: self.reasoning_summary.clone(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                })
            }
        }
    }

    struct SummaryMessageModel {
        text: String,
        reasoning_summary: String,
    }

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
                        text: self.text.clone(),
                    },
                    selection: None,
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
                    selection: None,
                    reasoning_summary,
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                })
            }
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

    fn failed_execution_evidence(failure: ExecutionFailure) -> ExecutionEvidence {
        ExecutionEvidence {
            source: "failed_code()".to_owned(),
            reasoning_summary: String::new(),
            began: true,
            stdout: empty_observation(),
            stderr: empty_observation(),
            tool_calls: Vec::new(),
            cancellation_warning: None,
            failure: Some(failure),
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

        assert!(events.iter().any(|event| matches!(
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
        )));
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
                },
                CompletedModelAction::Code {
                    source: "first + 1".to_owned(),
                    call_id: ModelCallId::new("second"),
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
            run_id: RunId(1),
            cancellation: CancellationToken::new(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut publisher = ProgressPublisher::new(progress_sender);

        controller.handle_worker_message(
            WorkerMessage::Finished {
                agent,
                result: Err(crate::agent::AgentError::Cancelled {
                    attempts: Vec::new(),
                    evidence: None,
                }),
                final_step_ordinal: 0,
                progress_gap: true,
            },
            &event_sender,
            &mut publisher,
        );

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
                "record:1:execution:1",
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
                "record:1:execution:1",
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
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    record: Record::CompletedExecution { .. },
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            next_event(&mut events).await,
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: RunId(1)
            }
        );
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        let reliable_events = events_until_settled(&mut events).await;

        assert_eq!(
            progress_events.iter().map(event_label).collect::<Vec<_>>(),
            ["run:1", "model:1:1", "execution:1:1",]
        );
        assert_eq!(
            reliable_events.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:user:cancel", "record:1:cancelled", "settled:1",]
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
                "record:1:execution:1",
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
            ["record:1:user:stop", "record:1:cancelled", "settled:1",]
        );
        assert_eq!(records, committed_records(&reliable_events));
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        let [
            StoredRecord {
                record: Record::UserRequest { .. },
                ..
            },
            StoredRecord {
                record:
                    Record::CompletedExecution {
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:user:accounting",
                "record:1:execution:1",
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
    async fn terminal_failure_retains_the_non_cancellation_arena_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let agent = controller
            .agent
            .take()
            .expect("idle controller should own its agent");
        let (worker_sender, worker) = mpsc::unbounded_channel();
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        controller.active = Some(ActiveRun {
            run_id: RunId(1),
            cancellation: CancellationToken::new(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
        worker_sender
            .send(WorkerMessage::Finished {
                agent,
                result: Err(crate::agent::AgentError::Arena {
                    error: ArenaError::Infrastructure {
                        message: "arena failed".to_owned(),
                        execution: None,
                    },
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                    evidence: Box::new(failed_execution_evidence(ExecutionFailure::Arena {
                        message: "arena failed".to_owned(),
                    })),
                }),
                final_step_ordinal: 0,
                progress_gap: false,
            })
            .unwrap();
        let (commands, command_receiver) = mpsc::channel(1);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let task = tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        let events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, task).await;

        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "record:1:failed:arena infrastructure failed: arena failed",
                "settled:1"
            ]
        );
        let Record::FailedRun { terminal, .. } = &records[0].record else {
            panic!("expected failed terminal record");
        };
        assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
        let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
            panic!("expected failed execution evidence");
        };
        assert_eq!(evidence.source, "failed_code()");
        assert!(matches!(
            &evidence.failure,
            Some(ExecutionFailure::Arena { .. })
        ));
    }

    #[tokio::test]
    async fn terminal_failures_scope_residual_attempts_and_cancellation_precedence() {
        #[derive(Clone, Copy)]
        enum FailureKind {
            Provider,
            Arena,
            Recovery,
        }

        for (kind, cancellation_queued) in [
            (FailureKind::Provider, true),
            (FailureKind::Arena, true),
            (FailureKind::Recovery, true),
            (FailureKind::Provider, false),
            (FailureKind::Arena, false),
        ] {
            let workspace = tempfile::tempdir().unwrap();
            let mut controller = controller(&workspace, Vec::new()).await;
            let agent = controller
                .agent
                .take()
                .expect("idle controller should own its agent");
            let attempts = vec![
                AttemptAccounting::UsageUnavailable,
                AttemptAccounting::UsageUnavailable,
            ];
            let (event_sender, mut events) = mpsc::unbounded_channel();
            controller.append_record(
                &event_sender,
                RunId(1),
                Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    source: "committed()".to_owned(),
                    reasoning_summary: String::new(),
                    execution: Execution {
                        stdout: empty_observation(),
                        stderr: empty_observation(),
                        outcome: ExecutionOutcome::Complete { result: None },
                        tool_calls: Vec::new(),
                    },
                    attempts: vec![AttemptAccounting::UsageUnavailable],
                },
                false,
            );
            let result = match kind {
                FailureKind::Provider => Err(crate::agent::AgentError::Model {
                    failure: ModelFailure::new(
                        ModelError::Provider(Box::new(ProviderFailure::new(
                            Some(500),
                            Some("provider_error".to_owned()),
                            "provider failed".to_owned(),
                        ))),
                        attempts.clone(),
                    ),
                    attempts: attempts.clone(),
                    evidence: Some(Box::new(TerminalEvidence::Generation(GenerationEvidence {
                        reasoning_summary: String::new(),
                        reasoning: "thinking".to_owned(),
                        assistant_text: "partial".to_owned(),
                        decoded_python: String::new(),
                    }))),
                }),
                FailureKind::Arena => Err(crate::agent::AgentError::Arena {
                    error: ArenaError::Infrastructure {
                        message: "arena failed".to_owned(),
                        execution: None,
                    },
                    attempts: attempts.clone(),
                    evidence: Box::new(failed_execution_evidence(ExecutionFailure::Arena {
                        message: "arena failed".to_owned(),
                    })),
                }),
                FailureKind::Recovery => Err(crate::agent::AgentError::Arena {
                    error: ArenaError::RecoveryFailed {
                        original: Box::new(ArenaError::Infrastructure {
                            message: "arena failed".to_owned(),
                            execution: None,
                        }),
                        recovery: Box::new(ArenaError::Infrastructure {
                            message: "recovery failed".to_owned(),
                            execution: None,
                        }),
                    },
                    attempts: attempts.clone(),
                    evidence: Box::new(failed_execution_evidence(
                        ExecutionFailure::RecoveryFailed {
                            original: "arena failed".to_owned(),
                            recovery: "recovery failed".to_owned(),
                        },
                    )),
                }),
            };
            let (worker_sender, worker) = mpsc::unbounded_channel();
            let (_worker_progress_sender, worker_progress) =
                mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            controller.active = Some(ActiveRun {
                run_id: RunId(1),
                cancellation: CancellationToken::new(),
                worker,
                progress: worker_progress,
                progress_open: false,
                cancellation_requested: false,
                step_ids: Vec::new(),
            });
            worker_sender
                .send(WorkerMessage::Finished {
                    agent,
                    result,
                    final_step_ordinal: 0,
                    progress_gap: false,
                })
                .unwrap();
            let (commands, command_receiver) = mpsc::channel(1);
            if cancellation_queued {
                commands.send(RuntimeCommand::Cancel).await.unwrap();
            }
            let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
            let task =
                tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

            let events = events_until_settled(&mut events).await;
            let records = shutdown_controller(commands, task).await;
            assert_eq!(records, committed_records(&events));
            let terminal = records.last().expect("terminal record");
            assert!(matches!(
                &records[0].record,
                Record::CompletedExecution { attempts, .. }
                    if attempts == &[AttemptAccounting::UsageUnavailable]
            ));
            assert!(matches!(
                events.first(),
                Some(RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(1),
                        record: Record::CompletedExecution { .. },
                    },
                    ..
                })
            ));
            match (kind, cancellation_queued) {
                (FailureKind::Provider | FailureKind::Arena, true) => {
                    assert!(matches!(
                        events.as_slice(),
                        [
                            _,
                            RuntimeEvent::RecordAppended {
                                record: StoredRecord {
                                    run_id: RunId(1),
                                    record: Record::CancelledRun { .. },
                                },
                                ..
                            },
                            RuntimeEvent::RunSettled { run_id: RunId(1) },
                        ]
                    ));
                    let Record::CancelledRun { terminal } = &terminal.record else {
                        panic!("accepted cancellation should win");
                    };
                    assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
                    assert!(terminal.evidence.is_some());
                }
                (FailureKind::Provider | FailureKind::Arena, false) => {
                    assert!(matches!(
                        events.as_slice(),
                        [
                            _,
                            RuntimeEvent::RecordAppended {
                                record: StoredRecord {
                                    run_id: RunId(1),
                                    record: Record::FailedRun { .. },
                                },
                                ..
                            },
                            RuntimeEvent::RunSettled { run_id: RunId(1) },
                        ]
                    ));
                    let Record::FailedRun { terminal, .. } = &terminal.record else {
                        panic!("ordinary failures should remain failed runs");
                    };
                    assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
                    assert!(terminal.evidence.is_some());
                }
                (FailureKind::Recovery, true) => {
                    assert!(matches!(
                        events.as_slice(),
                        [
                            _,
                            RuntimeEvent::RecordAppended {
                                record: StoredRecord {
                                    run_id: RunId(1),
                                    record: Record::FailedRun { .. },
                                },
                                ..
                            },
                            RuntimeEvent::RuntimeDisabled {
                                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
                            },
                            RuntimeEvent::RunSettled { run_id: RunId(1) },
                        ]
                    ));
                    let Record::FailedRun { terminal, .. } = &terminal.record else {
                        panic!("recovery failure should override cancellation");
                    };
                    assert_eq!(terminal.attempts, [AttemptAccounting::UsageUnavailable]);
                    let Some(TerminalEvidence::Execution(evidence)) = &terminal.evidence else {
                        panic!("expected recovery execution evidence");
                    };
                    assert!(matches!(
                        &evidence.failure,
                        Some(ExecutionFailure::RecoveryFailed { .. })
                    ));
                }
                (FailureKind::Recovery, false) => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn terminal_shutdown_wins_over_an_already_queued_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(
            &workspace,
            vec![ModelAction::Message {
                text: "must not be admitted".to_owned(),
            }],
        )
        .await;
        let mut agent = controller
            .agent
            .take()
            .expect("idle controller should own its agent");
        let model_cancellation = CancellationToken::new();
        agent.run("first", &model_cancellation).await.unwrap();

        let (worker_sender, worker) = mpsc::unbounded_channel();
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let cancellation = CancellationToken::new();
        controller.active = Some(ActiveRun {
            run_id: RunId(1),
            cancellation: cancellation.clone(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
        worker_sender
            .send(WorkerMessage::Finished {
                agent,
                result: Ok(crate::agent::AgentRunOutcome::FinalMessageStaged),
                final_step_ordinal: 1,
                progress_gap: false,
            })
            .unwrap();
        let (commands, command_receiver) = mpsc::channel(1);
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let task = tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        let events = events_until_settled(&mut events).await;
        let records = timeout(TEST_TIMEOUT, task)
            .await
            .expect("shutdown should settle the active run")
            .expect("controller task should not panic");

        assert!(cancellation.is_cancelled());
        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:cancelled", "settled:1"]
        );
        assert!(matches!(
            records.as_slice(),
            [StoredRecord {
                record: Record::CancelledRun { .. },
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn terminal_cancellation_wins_over_an_already_queued_final_message() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller_with_model(
            &workspace,
            SummaryMessageModel {
                text: "must not be admitted".to_owned(),
                reasoning_summary: "final summary".to_owned(),
            },
        )
        .await;
        let mut agent = controller
            .agent
            .take()
            .expect("idle controller should own its agent");
        let model_cancellation = CancellationToken::new();
        assert_eq!(
            agent.run("first", &model_cancellation).await.unwrap(),
            crate::agent::AgentRunOutcome::FinalMessageStaged
        );

        let (worker_sender, worker) = mpsc::unbounded_channel();
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let cancellation = CancellationToken::new();
        controller.active = Some(ActiveRun {
            run_id: RunId(1),
            cancellation: cancellation.clone(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
        worker_sender
            .send(WorkerMessage::Finished {
                agent,
                result: Ok(crate::agent::AgentRunOutcome::FinalMessageStaged),
                final_step_ordinal: 1,
                progress_gap: false,
            })
            .unwrap();

        let (commands, command_receiver) = mpsc::channel(1);
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let task = tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        let events = events_until_settled(&mut events).await;
        assert!(cancellation.is_cancelled());
        assert_eq!(
            events.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:cancelled", "settled:1"]
        );

        let records = shutdown_controller(commands, task).await;
        let [
            StoredRecord {
                record: Record::CancelledRun { terminal },
                ..
            },
        ] = records.as_slice()
        else {
            panic!("the queued final should be replaced by one cancelled terminal record");
        };
        let Some(TerminalEvidence::Generation(evidence)) = &terminal.evidence else {
            panic!("the lost final should be retained as generation evidence");
        };
        assert_eq!(evidence.assistant_text, "must not be admitted");
        assert_eq!(evidence.reasoning_summary, "final summary");
        assert!(evidence.reasoning.is_empty());
        assert!(evidence.decoded_python.is_empty());
        assert!(terminal.attempts.is_empty());
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        recovery_gate.wait_until_recovery_started().await;
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
    async fn disabled_recovery_failure_overrides_accepted_cancellation_and_settles_once() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let agent = controller
            .agent
            .take()
            .expect("idle controller should own its agent");
        let (worker_sender, worker) = mpsc::unbounded_channel();
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        controller.active = Some(ActiveRun {
            run_id: RunId(1),
            cancellation: CancellationToken::new(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
        worker_sender
            .send(WorkerMessage::Finished {
                agent,
                result: Err(crate::agent::AgentError::Arena {
                    error: ArenaError::RecoveryFailed {
                        original: Box::new(ArenaError::Cancelled {
                            execution: Box::new(crate::arena::InterruptedExecution {
                                began: true,
                                stdout: empty_observation(),
                                stderr: empty_observation(),
                                tool_calls: Vec::new(),
                            }),
                        }),
                        recovery: Box::new(ArenaError::Infrastructure {
                            message: "recovery failed".to_owned(),
                            execution: None,
                        }),
                    },
                    attempts: Vec::new(),
                    evidence: Box::new(failed_execution_evidence(
                        ExecutionFailure::RecoveryFailed {
                            original: "arena execution was cancelled".to_owned(),
                            recovery: "arena infrastructure failed: recovery failed".to_owned(),
                        },
                    )),
                }),
                final_step_ordinal: 0,
                progress_gap: false,
            })
            .unwrap();
        let (commands, command_receiver) = mpsc::channel(2);
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, mut progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let task = tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        let terminal_events = events_until_settled(&mut events).await;
        assert!(matches!(
            terminal_events.as_slice(),
            [
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
        assert_eq!(
            terminal_events
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
        let records = shutdown_controller(commands, task).await;
        assert_eq!(records.len(), 1);
    }

    #[tokio::test]
    async fn disabled_recovery_failure_bounds_its_display_message_at_a_utf8_boundary() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let agent = controller
            .agent
            .take()
            .expect("idle controller should own its agent");
        let (worker_sender, worker) = mpsc::unbounded_channel();
        let (_worker_progress_sender, worker_progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let diagnostic = "é".repeat(4 * 1024);
        controller.active = Some(ActiveRun {
            run_id: RunId(1),
            cancellation: CancellationToken::new(),
            worker,
            progress: worker_progress,
            progress_open: false,
            cancellation_requested: false,
            step_ids: Vec::new(),
        });
        worker_sender
            .send(WorkerMessage::Finished {
                agent,
                result: Err(crate::agent::AgentError::Arena {
                    error: ArenaError::RecoveryFailed {
                        original: Box::new(ArenaError::Infrastructure {
                            message: diagnostic.clone(),
                            execution: None,
                        }),
                        recovery: Box::new(ArenaError::Infrastructure {
                            message: diagnostic.clone(),
                            execution: None,
                        }),
                    },
                    attempts: Vec::new(),
                    evidence: Box::new(failed_execution_evidence(
                        ExecutionFailure::RecoveryFailed {
                            original: diagnostic.clone(),
                            recovery: diagnostic,
                        },
                    )),
                }),
                final_step_ordinal: 0,
                progress_gap: false,
            })
            .unwrap();
        let (commands, command_receiver) = mpsc::channel(1);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _progress) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let task = tokio::spawn(controller.run(command_receiver, event_sender, progress_sender));

        let events = events_until_settled(&mut events).await;
        let Record::FailedRun { message, terminal } = &committed_records(&events)[0].record else {
            panic!("expected failed terminal record");
        };

        assert!(message.len() <= 4 * 1024);
        assert!(message.is_char_boundary(message.len()));
        assert!(matches!(
            terminal.evidence,
            Some(TerminalEvidence::Execution(_))
        ));
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
        commands.send(RuntimeCommand::Cancel).await.unwrap();
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
        let accepted = AcceptedThinking::new(invoking, persisted_settings(&home, workspace.path()));

        assert_eq!(accepted.resolve(invoking, None), invoking);
        assert_eq!(
            accepted.resolve(ModelSelection::new(&LUNA, "none").unwrap(), Some(&SOL)),
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
    fn grandchild_without_an_override_inherits_its_invoking_child_selection() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let root = ModelSelection::new(&SOL, "low").unwrap();
        let child = ModelSelection::new(&LUNA, "max").unwrap();
        let accepted = AcceptedThinking::new(root, persisted_settings(&home, workspace.path()));

        assert_eq!(accepted.resolve(child, None), child);
    }

    #[tokio::test]
    async fn accepted_thinking_updates_before_its_settings_acknowledgement() {
        let workspace = tempfile::tempdir().unwrap();
        let selection_a = ModelSelection::new(&SOL, "low").unwrap();
        let selection_b = ModelSelection::new(&TERRA, "high").unwrap();
        let (settings, _) = watch::channel(selection_a);
        let mut controller = controller(&workspace, Vec::new())
            .await
            .with_settings(settings.clone());
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
        assert_eq!(*settings.borrow(), selection_b);
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
            "child1",
            3,
            selection,
            None,
            1,
            vec![AttemptAccounting::UsageUnavailable],
        );
        let finished = crate::subagents::TaskRecord::for_test(
            "child1",
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
    async fn finished_children_emit_one_new_authoritative_record_before_each_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let mut controller = controller(&workspace, Vec::new()).await;
        let (event_sender, mut events) = mpsc::unbounded_channel();
        let (progress_sender, _statuses) = mpsc::channel(PROGRESS_CHANNEL_CAPACITY);
        let mut progress = ProgressPublisher::new(progress_sender);
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap();
        let spawned =
            crate::subagents::TaskRecord::for_test("child1", 3, selection, None, 1, Vec::new());
        let finished = crate::subagents::TaskRecord::for_test(
            "child1",
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
        let (settings, selection) = watch::channel(initial_selection);
        let contexts = Arc::new(Mutex::new(Vec::new()));
        let root_selections = Arc::new(Mutex::new(Vec::new()));
        let children = WorkflowChildFactory::new();
        let coordinator = complete_child_coordinator(children.clone());
        let controller = controller_with_subagents(
            &workspace,
            WorkflowRootModel {
                selection,
                contexts: contexts.clone(),
                selections: root_selections.clone(),
                request: 0,
                step: 0,
            },
            coordinator.clone(),
        )
        .await
        .with_settings(settings);
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
            ["000000", "000001", "000002", "000003"]
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
        assert_eq!(child_records[3].parent().unwrap().to_string(), "000002");
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
                    ToolValue::String("000003".to_owned()),
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
            ["000000", "000001", "000002", "000003"]
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
            ("000000", TaskState::Completed),
            ("000001", TaskState::Failed),
            ("000002", TaskState::Completed),
            ("000003", TaskState::Cancelled),
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
                    summary: ToolCallSummary::Subagent("000000".to_owned()),
                },
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent("000001".to_owned()),
                },
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent("000002".to_owned()),
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
                    summary: ToolCallSummary::Subagent("000000".to_owned()),
                },
                ToolCall {
                    tool: ToolName::RefreshSubagent,
                    summary: ToolCallSummary::Subagent("000000".to_owned()),
                },
                ToolCall {
                    tool: ToolName::WaitSubagent,
                    summary: ToolCallSummary::Subagent("000000".to_owned()),
                },
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("000001".to_owned()),
                },
                ToolCall {
                    tool: ToolName::WaitSubagent,
                    summary: ToolCallSummary::Subagent("000001".to_owned()),
                },
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("000002".to_owned()),
                },
                ToolCall {
                    tool: ToolName::WaitSubagent,
                    summary: ToolCallSummary::Subagent("000002".to_owned()),
                },
                ToolCall {
                    tool: ToolName::GetSubagent,
                    summary: ToolCallSummary::Subagent("000003".to_owned()),
                },
                ToolCall {
                    tool: ToolName::RefreshSubagent,
                    summary: ToolCallSummary::Subagent("000003".to_owned()),
                },
                ToolCall {
                    tool: ToolName::CancelSubagent,
                    summary: ToolCallSummary::Subagent("000002".to_owned()),
                },
                ToolCall {
                    tool: ToolName::RefreshSubagent,
                    summary: ToolCallSummary::Subagent("000003".to_owned()),
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
        assert_eq!(events.len(), 5);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::ToolUpdated { .. }))
        );
    }
}
