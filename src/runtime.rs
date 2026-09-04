use tokio::sync::mpsc;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentError, AgentProgress, AgentRunOutcome, TerminalEvidence};
use crate::arena::{Execution, RunPreview, ToolCall, ToolCallSummary, ToolCallToken, ToolName};
use crate::model::{AttemptAccounting, Model, ModelProgress};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RunId(pub(crate) u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ModelStepId(pub(crate) u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuntimeCommand {
    Submit { request: String },
    Cancel,
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    RunSettled {
        run_id: RunId,
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

pub(crate) struct RuntimeController<M: Model> {
    agent: Option<Agent<M>>,
    disabled_reason: Option<RuntimeDisabledReason>,
    records: Vec<StoredRecord>,
    next_run_id: u64,
    next_model_step_id: u64,
    active: Option<ActiveRun<M>>,
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
        | AgentProgress::ExecutionStarted
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
        | RuntimeEvent::RunSettled { .. }
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
    pub(crate) fn new(agent: Agent<M>) -> Self {
        Self {
            agent: Some(agent),
            disabled_reason: None,
            records: Vec::new(),
            next_run_id: 1,
            next_model_step_id: 1,
            active: None,
        }
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

        loop {
            if self.active.is_some() {
                let input = {
                    let active = self
                        .active
                        .as_mut()
                        .expect("active run checked before receiving worker input");
                    tokio::select! {
                        biased;
                        command = commands.recv(), if commands_open => ControllerInput::Command(command),
                        message = active.worker.recv() => ControllerInput::Worker(message),
                        worker_progress = active.progress.recv(), if active.progress_open => ControllerInput::Progress(worker_progress),
                    }
                };
                match input {
                    ControllerInput::Command(Some(command)) => {
                        self.handle_command(
                            command,
                            &events,
                            &mut progress,
                            &mut shutdown_requested,
                        );
                    }
                    ControllerInput::Command(None) => {
                        commands_open = false;
                        shutdown_requested = true;
                        self.request_active_cancellation();
                    }
                    ControllerInput::Worker(Some(message)) => {
                        self.handle_worker_message(message, &events, &mut progress);
                    }
                    ControllerInput::Worker(None) => {
                        shutdown_requested = true;
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
                }
            } else {
                if shutdown_requested || !commands_open {
                    return self.records;
                }
                match commands.recv().await {
                    Some(command) => self.handle_command(
                        command,
                        &events,
                        &mut progress,
                        &mut shutdown_requested,
                    ),
                    None => return self.records,
                }
            }

            if shutdown_requested && self.active.is_none() {
                return self.records;
            }
        }
    }

    fn handle_command(
        &mut self,
        command: RuntimeCommand,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
        shutdown_requested: &mut bool,
    ) {
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
                    self.start_run(request, events, progress);
                }
            }
            RuntimeCommand::Cancel => self.request_active_cancellation(),
            RuntimeCommand::Shutdown => {
                *shutdown_requested = true;
                self.request_active_cancellation();
            }
        }
    }

    fn start_run(
        &mut self,
        request: String,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
        progress: &mut ProgressPublisher<RuntimeEvent>,
    ) {
        let run_id = self.next_run_id();
        self.publish_progress(progress, RuntimeEvent::RunStarted { run_id }, false);
        self.append_record(
            events,
            run_id,
            Record::UserRequest {
                request: request.clone(),
            },
            false,
        );

        let agent = self
            .agent
            .take()
            .expect("an idle runtime controller owns its agent");
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
            active.cancellation_requested = true;
            active.cancellation.cancel();
        }
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
                self.emit(events, RuntimeEvent::RunSettled { run_id });
                self.active = None;
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
        self.append_record(events, run_id, record, progress.take_pending_gap());
        self.emit(events, RuntimeEvent::RunSettled { run_id });
        self.active = None;
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
            AgentProgress::ExecutionStarted => RuntimeEvent::ExecutionStarted { run_id, step_id },
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
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use tokio::sync::{Notify, mpsc};
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::{
        ActiveRun, ModelStepId, PROGRESS_CHANNEL_CAPACITY, ProgressEnvelope, ProgressPublisher,
        Record, RunId, RuntimeCommand, RuntimeController, RuntimeDisabledReason, RuntimeEvent,
        RuntimeProgressReceiver, StoredRecord, WorkerMessage, runtime_event_payload_bytes,
    };
    use crate::agent::{
        Agent, ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence,
    };
    use crate::arena::{
        Arena, ArenaError, Execution, ExecutionOutcome, Observation, RecoveryTestGate, ToolCall,
        ToolCallSummary, ToolName,
    };
    use crate::model::{
        AttemptAccounting, Model, ModelAction as CompletedModelAction, ModelCallId, ModelError,
        ModelFailure, ModelHistory, ModelProgress, ModelStep, ProviderFailure, ProviderReplay,
        ScriptedAction as ModelAction, ScriptedModel,
    };
    use crate::tools::{ReadTestGate, ToolHost};
    use tokio_util::sync::CancellationToken;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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
        RuntimeController::new(Agent::new(arena, model))
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
                tool_call_label(*call)
            ),
            RuntimeEvent::RunPreviewUpdated { .. } => "run-preview-updated".to_owned(),
            RuntimeEvent::StdoutDelta { .. } => "stdout-delta".to_owned(),
            RuntimeEvent::StdoutTruncated { .. } => "stdout-truncated".to_owned(),
            RuntimeEvent::RecordAppended { record, .. } => record_label(record),
            RuntimeEvent::RunSettled { run_id } => format!("settled:{}", run_id.0),
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
        }
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
        let controller = RuntimeController::new(Agent::new(
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
        let controller = RuntimeController::new(Agent::new(
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
        let controller = RuntimeController::new(Agent::new(
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
        let controller = RuntimeController::new(Agent::new(
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
        let controller = RuntimeController::new(Agent::new(
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
        let controller = RuntimeController::new(Agent::new(
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
