use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentError, AgentProgress};
use crate::arena::{Execution, ReadOutcome};

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

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Record {
    UserRequest {
        request: String,
    },
    CompletedExecution {
        step_id: ModelStepId,
        source: String,
        execution: Execution,
    },
    FinalMessage {
        step_id: ModelStepId,
        text: String,
    },
    CancelledRun,
    FailedRun {
        message: String,
    },
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
    ExecutionStarted {
        run_id: RunId,
        step_id: ModelStepId,
    },
    ToolStarted {
        run_id: RunId,
        step_id: ModelStepId,
        path: String,
    },
    ToolFinished {
        run_id: RunId,
        step_id: ModelStepId,
        path: String,
        outcome: ReadOutcome,
    },
    RecordAppended {
        record: StoredRecord,
    },
    RunSettled {
        run_id: RunId,
    },
    SubmissionRejectedBusy {
        active_run_id: RunId,
    },
}

pub(crate) struct RuntimeController {
    agent: Option<Agent>,
    records: Vec<StoredRecord>,
    next_run_id: u64,
    next_model_step_id: u64,
    active: Option<ActiveRun>,
}

struct ActiveRun {
    run_id: RunId,
    cancellation: CancellationToken,
    worker: mpsc::UnboundedReceiver<WorkerMessage>,
    current_step_id: Option<ModelStepId>,
}

#[allow(clippy::large_enum_variant)]
enum WorkerMessage {
    Progress(AgentProgress),
    Finished {
        agent: Agent,
        result: Result<String, AgentError>,
    },
}

impl RuntimeController {
    pub(crate) fn new(agent: Agent) -> Self {
        Self {
            agent: Some(agent),
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
    ) -> Vec<StoredRecord> {
        let mut commands_open = true;
        let mut shutdown_requested = false;

        loop {
            if self.active.is_some() {
                tokio::select! {
                    biased;
                    command = commands.recv(), if commands_open => match command {
                        Some(command) => self.handle_command(command, &events, &mut shutdown_requested),
                        None => {
                            commands_open = false;
                            shutdown_requested = true;
                            self.cancel_active();
                        }
                    },
                    message = self.active.as_mut().expect("active run checked before receiving worker message").worker.recv() => {
                        match message {
                            Some(message) => self.handle_worker_message(message, &events),
                            None => {
                                shutdown_requested = true;
                                self.handle_worker_closed(&events);
                            }
                        }
                    }
                }
            } else {
                if shutdown_requested || !commands_open {
                    return self.records;
                }
                match commands.recv().await {
                    Some(command) => self.handle_command(command, &events, &mut shutdown_requested),
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
                } else {
                    self.start_run(request, events);
                }
            }
            RuntimeCommand::Cancel => self.cancel_active(),
            RuntimeCommand::Shutdown => {
                *shutdown_requested = true;
                self.cancel_active();
            }
        }
    }

    fn start_run(&mut self, request: String, events: &mpsc::UnboundedSender<RuntimeEvent>) {
        let run_id = self.next_run_id();
        self.emit(events, RuntimeEvent::RunStarted { run_id });
        self.append_record(
            events,
            run_id,
            Record::UserRequest {
                request: request.clone(),
            },
        );

        let agent = self
            .agent
            .take()
            .expect("an idle runtime controller owns its agent");
        let cancellation = CancellationToken::new();
        let cancellation_for_worker = cancellation.clone();
        let (sender, worker) = mpsc::unbounded_channel();
        let worker_task = tokio::spawn(async move {
            let mut agent = agent;
            let mut progress = |progress| {
                let _ = sender.send(WorkerMessage::Progress(progress));
            };
            let result = agent
                .run_with_progress(request, &cancellation_for_worker, &mut progress)
                .await;
            let _ = sender.send(WorkerMessage::Finished { agent, result });
        });
        drop(worker_task);
        self.active = Some(ActiveRun {
            run_id,
            cancellation,
            worker,
            current_step_id: None,
        });
    }

    fn cancel_active(&self) {
        if let Some(active) = &self.active {
            active.cancellation.cancel();
        }
    }

    fn handle_worker_message(
        &mut self,
        message: WorkerMessage,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        match message {
            WorkerMessage::Progress(progress) => self.handle_progress(progress, events),
            WorkerMessage::Finished { agent, result } => {
                self.agent = Some(agent);
                let run_id = self.active_run_id();
                let record = match result {
                    Ok(final_message) => Record::FinalMessage {
                        step_id: self.current_step_id(),
                        text: final_message,
                    },
                    Err(AgentError::Cancelled) => Record::CancelledRun,
                    Err(error) => Record::FailedRun {
                        message: error.to_string(),
                    },
                };
                self.append_record(events, run_id, record);
                self.emit(events, RuntimeEvent::RunSettled { run_id });
                self.active = None;
            }
        }
    }

    fn handle_worker_closed(&mut self, events: &mpsc::UnboundedSender<RuntimeEvent>) {
        let run_id = self.active_run_id();
        self.append_record(
            events,
            run_id,
            Record::FailedRun {
                message: "agent worker exited without returning its agent".to_owned(),
            },
        );
        self.emit(events, RuntimeEvent::RunSettled { run_id });
        self.active = None;
    }

    fn handle_progress(
        &mut self,
        progress: AgentProgress,
        events: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        match progress {
            AgentProgress::ModelStepStarted => {
                let run_id = self.active_run_id();
                let step_id = self.next_model_step_id();
                self.active
                    .as_mut()
                    .expect("model progress requires an active run")
                    .current_step_id = Some(step_id);
                self.emit(events, RuntimeEvent::ModelStepStarted { run_id, step_id });
            }
            AgentProgress::ExecutionStarted => {
                let (run_id, step_id) = self.active_step();
                self.emit(events, RuntimeEvent::ExecutionStarted { run_id, step_id });
            }
            AgentProgress::ToolStarted { path } => {
                let (run_id, step_id) = self.active_step();
                self.emit(
                    events,
                    RuntimeEvent::ToolStarted {
                        run_id,
                        step_id,
                        path,
                    },
                );
            }
            AgentProgress::ToolFinished { path, outcome } => {
                let (run_id, step_id) = self.active_step();
                self.emit(
                    events,
                    RuntimeEvent::ToolFinished {
                        run_id,
                        step_id,
                        path,
                        outcome,
                    },
                );
            }
            AgentProgress::ExecutionCompleted { source, execution } => {
                let (run_id, step_id) = self.active_step();
                self.append_record(
                    events,
                    run_id,
                    Record::CompletedExecution {
                        step_id,
                        source,
                        execution,
                    },
                );
            }
        }
    }

    fn active_run_id(&self) -> RunId {
        self.active
            .as_ref()
            .expect("worker message requires an active run")
            .run_id
    }

    fn active_step(&self) -> (RunId, ModelStepId) {
        let active = self
            .active
            .as_ref()
            .expect("agent progress requires an active run");
        (
            active.run_id,
            active
                .current_step_id
                .expect("execution progress requires a model step"),
        )
    }

    fn current_step_id(&self) -> ModelStepId {
        self.active
            .as_ref()
            .and_then(|active| active.current_step_id)
            .expect("a completed agent run ends on a model step")
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
        record: Record,
    ) {
        let record = StoredRecord { run_id, record };
        self.records.push(record.clone());
        self.emit(events, RuntimeEvent::RecordAppended { record });
    }

    fn emit(&self, events: &mpsc::UnboundedSender<RuntimeEvent>, event: RuntimeEvent) {
        let _ = events.send(event);
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::{
        ModelStepId, Record, RunId, RuntimeCommand, RuntimeController, RuntimeEvent, StoredRecord,
    };
    use crate::agent::Agent;
    use crate::arena::{Arena, Execution, ExecutionOutcome, ReadOutcome};
    use crate::model::{ModelAction, ScriptedModel};
    use crate::tools::ReadTool;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the configured worker")
    }

    async fn controller(
        workspace: &tempfile::TempDir,
        actions: Vec<ModelAction>,
    ) -> RuntimeController {
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        RuntimeController::new(Agent::new(arena, ScriptedModel::new(actions)))
    }

    async fn default_controller(workspace: &tempfile::TempDir) -> RuntimeController {
        let arena = Arena::new(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena");
        RuntimeController::new(Agent::new(arena, ScriptedModel::default()))
    }

    fn start_controller(
        controller: RuntimeController,
    ) -> (
        mpsc::Sender<RuntimeCommand>,
        mpsc::UnboundedReceiver<RuntimeEvent>,
        JoinHandle<Vec<StoredRecord>>,
    ) {
        let (command_sender, command_receiver) = mpsc::channel(8);
        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let controller = tokio::spawn(controller.run(command_receiver, event_sender));
        (command_sender, event_receiver, controller)
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

    async fn events_until_execution_started(
        events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
        step_id: ModelStepId,
    ) -> Vec<RuntimeEvent> {
        let mut collected = Vec::new();
        loop {
            let event = next_event(events).await;
            let started = matches!(
                event,
                RuntimeEvent::ExecutionStarted {
                    step_id: actual_step_id,
                    ..
                } if actual_step_id == step_id
            );
            collected.push(event);
            if started {
                return collected;
            }
        }
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
            RuntimeEvent::ExecutionStarted {
                run_id, step_id, ..
            } => format!("execution:{}:{}", run_id.0, step_id.0),
            RuntimeEvent::ToolStarted {
                run_id,
                step_id,
                path,
            } => format!("tool-started:{}:{}:{path}", run_id.0, step_id.0),
            RuntimeEvent::ToolFinished {
                run_id,
                step_id,
                path,
                outcome,
            } => match outcome {
                ReadOutcome::Success { line_count } => {
                    format!(
                        "tool-finished:{}:{}:{path}:success:{line_count}",
                        run_id.0, step_id.0
                    )
                }
                ReadOutcome::Failure { message } => {
                    format!(
                        "tool-finished:{}:{}:{path}:failure:{message}",
                        run_id.0, step_id.0
                    )
                }
            },
            RuntimeEvent::RecordAppended { record } => record_label(record),
            RuntimeEvent::RunSettled { run_id } => format!("settled:{}", run_id.0),
            RuntimeEvent::SubmissionRejectedBusy { active_run_id } => {
                format!("busy:{}", active_run_id.0)
            }
        }
    }

    fn record_label(record: &StoredRecord) -> String {
        match &record.record {
            Record::UserRequest { request } => format!("record:{}:user:{request}", record.run_id.0),
            Record::CompletedExecution { step_id, .. } => {
                format!("record:{}:execution:{}", record.run_id.0, step_id.0)
            }
            Record::FinalMessage { step_id, text } => {
                format!("record:{}:message:{}:{text}", record.run_id.0, step_id.0)
            }
            Record::CancelledRun => format!("record:{}:cancelled", record.run_id.0),
            Record::FailedRun { message } => format!("record:{}:failed:{message}", record.run_id.0),
        }
    }

    fn committed_records(events: &[RuntimeEvent]) -> Vec<StoredRecord> {
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::RecordAppended { record } => Some(record.clone()),
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

    #[tokio::test]
    async fn default_script_emits_ordered_events_and_records() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("Cargo.toml"),
            "[package]\nname = \"example\"\n",
        )
        .unwrap();
        let (commands, mut events, controller) =
            start_controller(default_controller(&workspace).await);

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
                "run:1",
                "record:1:user:inspect",
                "model:1:1",
                "execution:1:1",
                "tool-started:1:1:Cargo.toml",
                "tool-finished:1:1:Cargo.toml:success:2",
                "record:1:execution:1",
                "model:1:2",
                "execution:1:2",
                "record:1:execution:2",
                "model:1:3",
                "record:1:message:3:I inspected Cargo.toml.",
                "settled:1",
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
        let (commands, mut events, controller) =
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
                "run:1",
                "record:1:user:inspect",
                "model:1:1",
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
                        }
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
                }
            }
        )));
        assert_eq!(records, committed_records(&events));
    }

    #[tokio::test]
    async fn successful_requests_keep_arena_state_and_monotonic_ids() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let (commands, mut events, controller) =
            start_controller(default_controller(&workspace).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let first_events = events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        let second_events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            first_events
                .iter()
                .filter_map(|event| match event {
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
            second_events
                .iter()
                .filter_map(|event| match event {
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
        let (commands, mut events, controller) = start_controller(
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
                "run:1",
                "record:1:user:continue",
                "model:1:1",
                "execution:1:1",
                "record:1:execution:1",
                "model:1:2",
                "execution:1:2",
                "record:1:execution:2",
                "model:1:3",
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
        let (commands, mut events, controller) = start_controller(
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
        let first_started = events_until_execution_started(&mut events, ModelStepId(2)).await;
        commands
            .send(RuntimeCommand::Submit {
                request: "rejected".to_owned(),
            })
            .await
            .unwrap();
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
        let second_events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            first_started.iter().map(event_label).collect::<Vec<_>>(),
            [
                "run:1",
                "record:1:user:first",
                "model:1:1",
                "execution:1:1",
                "record:1:execution:1",
                "model:1:2",
                "execution:1:2",
            ]
        );
        assert_eq!(
            first_settled.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:cancelled", "settled:1"]
        );
        assert_eq!(
            second_events
                .iter()
                .filter_map(|event| match event {
                    RuntimeEvent::RunStarted { run_id } => Some(*run_id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [RunId(2)]
        );
        assert_eq!(
            second_events
                .iter()
                .filter_map(|event| match event {
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
        let controller = tokio::spawn(controller.run(command_receiver, event_sender));

        commands
            .send(RuntimeCommand::Submit {
                request: "cancel".to_owned(),
            })
            .await
            .unwrap();
        wait_for_queued_events(&events, 4).await;
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
            [
                "run:1",
                "record:1:user:cancel",
                "model:1:1",
                "execution:1:1",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert_eq!(records, committed_records(&drained));
    }

    #[tokio::test]
    async fn cancellation_discards_the_active_execution_and_settles_once() {
        let workspace = tempfile::tempdir().unwrap();
        let (commands, mut events, controller) = start_controller(
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
        let mut all_events = events_until_execution_started(&mut events, ModelStepId(1)).await;
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        all_events.extend(events_until_settled(&mut events).await);

        assert_eq!(
            all_events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "run:1",
                "record:1:user:cancel",
                "model:1:1",
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
        let (commands, mut events, controller) =
            start_controller(controller(&workspace, actions).await);

        commands
            .send(RuntimeCommand::Submit {
                request: "first".to_owned(),
            })
            .await
            .unwrap();
        let first_started = events_until_execution_started(&mut events, ModelStepId(2)).await;
        commands.send(RuntimeCommand::Cancel).await.unwrap();
        let first_settled = events_until_settled(&mut events).await;
        commands
            .send(RuntimeCommand::Submit {
                request: "second".to_owned(),
            })
            .await
            .unwrap();
        let second_events = events_until_settled(&mut events).await;
        let records = shutdown_controller(commands, controller).await;

        assert_eq!(
            first_started.iter().map(event_label).collect::<Vec<_>>(),
            [
                "run:1",
                "record:1:user:first",
                "model:1:1",
                "execution:1:1",
                "record:1:execution:1",
                "model:1:2",
                "execution:1:2",
            ]
        );
        assert_eq!(
            first_settled.iter().map(event_label).collect::<Vec<_>>(),
            ["record:1:cancelled", "settled:1"]
        );
        assert_eq!(
            second_events
                .iter()
                .filter_map(|event| match event {
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
        let (commands, mut events, controller) = start_controller(
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
        let mut all_events = events_until_execution_started(&mut events, ModelStepId(1)).await;
        commands.send(RuntimeCommand::Shutdown).await.unwrap();
        let records = timeout(TEST_TIMEOUT, controller)
            .await
            .expect("shutdown should wait only for cancellation recovery")
            .expect("controller task should not panic");
        all_events.extend(events_until_settled(&mut events).await);

        assert_eq!(
            all_events.iter().map(event_label).collect::<Vec<_>>(),
            [
                "run:1",
                "record:1:user:stop",
                "model:1:1",
                "execution:1:1",
                "record:1:cancelled",
                "settled:1",
            ]
        );
        assert_eq!(records, committed_records(&all_events));
    }

    #[tokio::test]
    async fn returned_records_are_exactly_the_committed_record_events() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("Cargo.toml"), "[package]\n").unwrap();
        let (commands, mut events, controller) =
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
        assert_eq!(events.len(), 13);
        assert!(events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolStarted { .. } | RuntimeEvent::ToolFinished { .. }
        )));
    }
}
