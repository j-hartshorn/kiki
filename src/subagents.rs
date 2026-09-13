use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as SyncMutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use serde::Serialize;
#[cfg(test)]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, MutexGuard, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentProgress, AgentRunOutcome};
use crate::arena::{ArenaChildSource, Checkpoint, Execution};
use crate::config::{
    ModelSelection,
    constants::MAX_PROMPT_BYTES,
    models::{ModelConfiguration, model_by_id},
    settings::{SettingsContext, SubagentSettings},
};
use crate::model::{AttemptAccounting, Model, ModelProgress};
use crate::pricing::{RunCostEstimate, estimate_run_cost};
use crate::prompt::child_prompt;
use crate::telemetry::{ChildStopCause, CoordinatorOperation, Settlement, Trace, TraceEvent};
use crate::tools::{ToolError, ToolValue};

const FAILURE_MESSAGE_LIMIT: usize = 4 * 1024;
const MAINTENANCE_LOCK_TRACE_THRESHOLD: Duration = Duration::from_millis(5);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

impl TaskId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn parse(value: &str) -> Option<Self> {
        (value.len() >= 2
            && (value.len() == 2 || !value.starts_with('0'))
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            && u64::from_str_radix(value, 16).is_ok())
        .then(|| Self(value.to_owned()))
    }
}

impl PartialOrd for TaskId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CoordinatorEvent {
    Spawned { record: TaskRecord },
    Changed { id: TaskId, revision: u64 },
    Finished { record: TaskRecord },
}

#[derive(Default)]
struct CoordinatorPublication {
    taken: bool,
    sender: Option<mpsc::UnboundedSender<CoordinatorEvent>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskState {
    Starting,
    Running,
    Stopping,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl TaskState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildParameters {
    prompt: String,
    selection: ModelSelection,
    read_only: bool,
}

impl ChildParameters {
    pub(crate) fn new(prompt: String, selection: ModelSelection, read_only: bool) -> Self {
        Self {
            prompt,
            selection,
            read_only,
        }
    }

    pub(crate) fn prompt(&self) -> &str {
        &self.prompt
    }

    pub(crate) fn selection(&self) -> ModelSelection {
        self.selection
    }

    pub(crate) fn read_only(&self) -> bool {
        self.read_only
    }

    pub(crate) fn into_parts(self) -> (String, ModelSelection, bool) {
        (self.prompt, self.selection, self.read_only)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ChildResult {
    message: String,
    values: BTreeMap<String, ToolValue>,
}

impl ChildResult {
    pub(crate) fn new(message: String, values: BTreeMap<String, ToolValue>) -> Self {
        Self { message, values }
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    #[cfg(test)]
    pub(crate) fn values(&self) -> &BTreeMap<String, ToolValue> {
        &self.values
    }

    pub(crate) fn into_parts(self) -> (String, BTreeMap<String, ToolValue>) {
        (self.message, self.values)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChildFailure(String);

impl ChildFailure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(bounded_utf8(message.into(), FAILURE_MESSAGE_LIMIT))
    }

    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
pub(crate) struct Caller {
    session: u64,
    owner: CallerOwner,
    origin: OriginId,
}

#[derive(Clone)]
enum CallerOwner {
    Root,
    Child(TaskId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OriginId(u64);

pub(crate) struct ChildRun<P: Send + 'static> {
    id: TaskId,
    caller: Caller,
    parent: Option<TaskId>,
    depth: usize,
    parameters: ChildParameters,
    payload: P,
    reporter: TaskReporter,
    coordinator: SubagentCoordinator<P>,
    trace: Trace,
}

impl<P: Send + 'static> ChildRun<P> {
    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    #[cfg(test)]
    pub(crate) fn caller(&self) -> &Caller {
        &self.caller
    }

    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    #[cfg(test)]
    pub(crate) fn parameters(&self) -> &ChildParameters {
        &self.parameters
    }

    #[cfg(test)]
    pub(crate) fn payload(&self) -> &P {
        &self.payload
    }

    #[cfg(test)]
    pub(crate) fn model_step_started(&self) {
        self.reporter.model_step_started();
    }

    fn trace(&self) -> Trace {
        self.trace.clone()
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        TaskId,
        Caller,
        Option<TaskId>,
        usize,
        ChildParameters,
        P,
        TaskReporter,
        SubagentCoordinator<P>,
    ) {
        (
            self.id,
            self.caller,
            self.parent,
            self.depth,
            self.parameters,
            self.payload,
            self.reporter,
            self.coordinator,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskAttempt {
    model_step: u64,
    provider_attempt: u32,
    accounting: Option<AttemptAccounting>,
}

impl TaskAttempt {
    #[cfg(test)]
    pub(crate) fn model_step(&self) -> u64 {
        self.model_step
    }

    pub(crate) fn provider_attempt(&self) -> u32 {
        self.provider_attempt
    }

    #[cfg(test)]
    pub(crate) fn accounting(&self) -> Option<&AttemptAccounting> {
        self.accounting.as_ref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskExecution {
    model_step: u64,
    source: String,
    reasoning_summary: String,
    execution: Execution,
    attempts: Vec<AttemptAccounting>,
}

#[cfg(test)]
impl TaskExecution {
    pub(crate) fn model_step(&self) -> u64 {
        self.model_step
    }

    pub(crate) fn source(&self) -> &str {
        &self.source
    }

    pub(crate) fn reasoning_summary(&self) -> &str {
        &self.reasoning_summary
    }

    pub(crate) fn execution(&self) -> &Execution {
        &self.execution
    }

    pub(crate) fn attempts(&self) -> &[AttemptAccounting] {
        &self.attempts
    }
}

struct TaskMetrics {
    state: SyncMutex<TaskMetricState>,
    updates: watch::Sender<u64>,
    notifications: watch::Sender<u64>,
    publication: Arc<SyncMutex<CoordinatorPublication>>,
    id: TaskId,
}

#[derive(Default)]
struct TaskMetricState {
    revision: u64,
    step_count: u64,
    attempts_by_key: BTreeMap<(u64, u32), Option<AttemptAccounting>>,
    attempts: Vec<AttemptAccounting>,
    executions: Vec<TaskExecution>,
    settled: bool,
}

#[derive(Clone)]
struct TaskMetricSnapshot {
    revision: u64,
    step_count: u64,
    attempt_evidence: Vec<TaskAttempt>,
    attempts: Vec<AttemptAccounting>,
    executions: Vec<TaskExecution>,
}

impl TaskMetrics {
    fn new(
        id: TaskId,
        notifications: watch::Sender<u64>,
        publication: Arc<SyncMutex<CoordinatorPublication>>,
    ) -> Self {
        let (updates, _) = watch::channel(1);
        Self {
            state: SyncMutex::new(TaskMetricState {
                revision: 1,
                ..TaskMetricState::default()
            }),
            updates,
            notifications,
            publication,
            id,
        }
    }

    fn update(&self, update: impl FnOnce(&mut TaskMetricState)) -> Option<u64> {
        let revision = {
            let mut state = self.state.lock().expect("task metrics lock");
            if state.settled {
                return None;
            }
            update(&mut state);
            state.revision = state
                .revision
                .checked_add(1)
                .expect("subagent task revision overflowed");
            state.revision
        };
        self.publish_changed(revision);
        Some(revision)
    }

    fn revise(&self) -> Option<u64> {
        self.update(|_| {})
    }

    fn settle(&self) -> Option<u64> {
        let mut state = self.state.lock().expect("task metrics lock");
        if state.settled {
            return None;
        }
        state.settled = true;
        state.revision = state
            .revision
            .checked_add(1)
            .expect("subagent task revision overflowed");
        let revision = state.revision;
        drop(state);
        self.notify(revision);
        Some(revision)
    }

    fn publish_changed(&self, revision: u64) {
        self.notify(revision);
        if let Some(sender) = self
            .publication
            .lock()
            .expect("coordinator publication lock")
            .sender
            .as_ref()
        {
            let _ = sender.send(CoordinatorEvent::Changed {
                id: self.id.clone(),
                revision,
            });
        }
    }

    fn notify(&self, revision: u64) {
        self.updates.send_replace(revision);
        self.notifications
            .send_modify(|global_revision| *global_revision = global_revision.wrapping_add(1));
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.updates.subscribe()
    }

    fn snapshot(&self) -> TaskMetricSnapshot {
        let state = self.state.lock().expect("task metrics lock");
        TaskMetricSnapshot {
            revision: state.revision,
            step_count: state.step_count,
            attempt_evidence: state
                .attempts_by_key
                .iter()
                .map(
                    |(&(model_step, provider_attempt), accounting)| TaskAttempt {
                        model_step,
                        provider_attempt,
                        accounting: accounting.clone(),
                    },
                )
                .collect(),
            attempts: state.attempts.clone(),
            executions: state.executions.clone(),
        }
    }
}

fn rebuild_attempts(state: &mut TaskMetricState) {
    state.attempts = state
        .attempts_by_key
        .values()
        .filter_map(Clone::clone)
        .collect();
}

#[derive(Clone)]
pub(crate) struct TaskReporter {
    metrics: Arc<TaskMetrics>,
}

impl TaskReporter {
    pub(crate) fn model_step_started(&self) {
        self.metrics.update(|state| {
            state.step_count = state
                .step_count
                .checked_add(1)
                .expect("subagent model step count overflowed");
        });
    }

    pub(crate) fn attempt_started(&self, model_step: u64, provider_attempt: u32) {
        self.metrics.update(|state| {
            state
                .attempts_by_key
                .entry((model_step, provider_attempt))
                .or_insert(None);
        });
    }

    pub(crate) fn attempt_settled(
        &self,
        model_step: u64,
        provider_attempt: u32,
        accounting: AttemptAccounting,
    ) {
        self.metrics.update(|state| {
            state
                .attempts_by_key
                .insert((model_step, provider_attempt), Some(accounting));
            rebuild_attempts(state);
        });
    }

    pub(crate) fn execution_completed(
        &self,
        model_step: u64,
        source: String,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    ) {
        self.metrics.update(|state| {
            for (index, accounting) in attempts.iter().cloned().enumerate() {
                let provider_attempt =
                    u32::try_from(index + 1).expect("provider attempt ordinal overflowed");
                let evidence = state
                    .attempts_by_key
                    .entry((model_step, provider_attempt))
                    .or_insert(None);
                if evidence.is_none() {
                    *evidence = Some(accounting);
                }
            }
            rebuild_attempts(state);
            state.executions.push(TaskExecution {
                model_step,
                source,
                reasoning_summary,
                execution,
                attempts,
            });
        });
    }

    pub(crate) fn set_attempts(&self, attempts: Vec<AttemptAccounting>) {
        self.metrics.update(|state| state.attempts = attempts);
    }
}

pub(crate) type RunnerFuture =
    Pin<Box<dyn Future<Output = Result<ChildResult, ChildFailure>> + Send + 'static>>;

pub(crate) trait ChildRunner<P: Send + 'static>: Send + Sync + 'static {
    fn run(&self, child: ChildRun<P>, cancellation: CancellationToken) -> RunnerFuture;
}

#[derive(Clone)]
pub(crate) struct ChildPayload {
    source: ArenaChildSource,
    checkpoint: Checkpoint,
    accepted_thinking: Option<AcceptedThinking>,
}

impl ChildPayload {
    pub(crate) fn new(source: ArenaChildSource, checkpoint: Checkpoint) -> Self {
        Self {
            source,
            checkpoint,
            accepted_thinking: None,
        }
    }

    fn with_accepted_thinking(mut self, accepted_thinking: AcceptedThinking) -> Self {
        self.accepted_thinking = Some(accepted_thinking);
        self
    }
}

pub(crate) type SessionSubagents = SubagentCoordinator<ChildPayload>;

#[derive(Clone)]
pub(crate) struct AcceptedThinking(Arc<RwLock<AcceptedThinkingState>>);

struct AcceptedThinkingState {
    persisted: SettingsContext,
    values: BTreeMap<&'static str, &'static str>,
}

impl AcceptedThinking {
    pub(crate) fn new(selection: ModelSelection, persisted: SettingsContext) -> Self {
        Self(Arc::new(RwLock::new(AcceptedThinkingState {
            persisted,
            values: BTreeMap::from([(selection.model().id, selection.thinking())]),
        })))
    }

    pub(crate) fn accept(&self, selection: ModelSelection) {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values
            .insert(selection.model().id, selection.thinking());
    }

    #[cfg(test)]
    pub(crate) fn persisted_settings(&self) -> SettingsContext {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .persisted
            .clone()
    }

    pub(crate) fn resolve(
        &self,
        invoking: ModelSelection,
        target: Option<&'static ModelConfiguration>,
    ) -> ModelSelection {
        let Some(target) = target else {
            return invoking;
        };
        let state = self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let thinking = state
            .values
            .get(target.id)
            .copied()
            .or_else(|| state.persisted.thinking_for(target))
            .unwrap_or(target.default_thinking);
        ModelSelection::new(target, thinking)
            .expect("accepted or persisted thinking must be supported by its model")
    }
}

#[derive(Clone)]
pub(crate) struct AgentInvocationContext {
    caller: Caller,
    coordinator: SessionSubagents,
    accepted_thinking: Option<AcceptedThinking>,
}

impl AgentInvocationContext {
    pub(crate) fn new(caller: Caller, coordinator: SessionSubagents) -> Self {
        Self {
            caller,
            coordinator,
            accepted_thinking: None,
        }
    }

    pub(crate) fn with_accepted_thinking(mut self, accepted_thinking: AcceptedThinking) -> Self {
        self.accepted_thinking = Some(accepted_thinking);
        self
    }

    pub(crate) fn execution(
        &self,
        source: ArenaChildSource,
        selection: Option<ModelSelection>,
        checkpoint: Checkpoint,
    ) -> TrustedExecutionContext {
        TrustedExecutionContext {
            caller: self.caller.clone(),
            selection,
            checkpoint,
            source,
            coordinator: self.coordinator.clone(),
            accepted_thinking: self.accepted_thinking.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct TrustedExecutionContext {
    caller: Caller,
    selection: Option<ModelSelection>,
    checkpoint: Checkpoint,
    source: ArenaChildSource,
    coordinator: SessionSubagents,
    accepted_thinking: Option<AcceptedThinking>,
}

impl TrustedExecutionContext {
    pub(crate) fn caller(&self) -> &Caller {
        &self.caller
    }

    pub(crate) fn payload(&self) -> ChildPayload {
        let payload = ChildPayload::new(self.source.clone(), self.checkpoint.clone());
        match &self.accepted_thinking {
            Some(accepted_thinking) => payload.with_accepted_thinking(accepted_thinking.clone()),
            None => payload,
        }
    }

    fn resolve_selection(&self, model: Option<&str>) -> Result<ModelSelection, ToolError> {
        let invoking = self.selection.ok_or_else(|| {
            ToolError::runtime("subagent model selection is unavailable for this execution")
        })?;
        let target = match model {
            Some(model) => Some(
                model_by_id(model)
                    .ok_or_else(|| ToolError::value(format!("unknown subagent model `{model}`")))?,
            ),
            None => None,
        };
        let accepted_thinking = self.accepted_thinking.as_ref().ok_or_else(|| {
            ToolError::runtime("subagent invocation context is unavailable for this execution")
        })?;
        Ok(accepted_thinking.resolve(invoking, target))
    }

    pub(crate) fn coordinator(&self) -> &SessionSubagents {
        &self.coordinator
    }
}

#[derive(Clone, Copy)]
pub(crate) enum InvocationOperation {
    Start,
    Get,
    Refresh,
    Wait,
    Cancel,
}

#[derive(Clone)]
pub(crate) struct WaitPhaseSignal {
    call_id: u32,
    sender: mpsc::UnboundedSender<u32>,
}

impl WaitPhaseSignal {
    pub(crate) fn new(call_id: u32, sender: mpsc::UnboundedSender<u32>) -> Self {
        Self { call_id, sender }
    }

    fn notify(&self) {
        let _ = self.sender.send(self.call_id);
    }
}

impl InvocationOperation {
    pub(crate) fn from_private_name(name: &str) -> Option<Self> {
        match name {
            "_kiki_start_subagent" => Some(Self::Start),
            "_kiki_get_subagent" => Some(Self::Get),
            "_kiki_refresh_subagent" => Some(Self::Refresh),
            "_kiki_wait_subagent" => Some(Self::Wait),
            "_kiki_cancel_subagent" => Some(Self::Cancel),
            _ => None,
        }
    }

    pub(crate) fn tool(self) -> crate::tools::ToolName {
        match self {
            Self::Start => crate::tools::ToolName::StartSubagent,
            Self::Get => crate::tools::ToolName::GetSubagent,
            Self::Refresh => crate::tools::ToolName::RefreshSubagent,
            Self::Wait => crate::tools::ToolName::WaitSubagent,
            Self::Cancel => crate::tools::ToolName::CancelSubagent,
        }
    }
}

pub(crate) async fn invoke(
    context: Option<&TrustedExecutionContext>,
    operation: InvocationOperation,
    arguments: ToolValue,
    wait_phase: Option<WaitPhaseSignal>,
) -> Result<ToolValue, ToolError> {
    let context = context.ok_or_else(|| {
        ToolError::runtime("subagent invocation context is unavailable for this execution")
    })?;
    match operation {
        InvocationOperation::Start => {
            let (prompt, model, read_only) = start_arguments(arguments)?;
            let selection = context.resolve_selection(model.as_deref())?;
            let observation = context
                .coordinator()
                .start(
                    context.caller(),
                    ChildParameters::new(prompt, selection, read_only),
                    context.payload(),
                )
                .await
                .map_err(|error| ToolError::runtime(error.to_string()))?;
            Ok(task_handle_value(&observation))
        }
        InvocationOperation::Get => {
            let id = task_id_argument(arguments, "get_subagent")?;
            let observation = context
                .coordinator()
                .get(context.caller(), &id)
                .await
                .map_err(access_error)?;
            Ok(task_handle_value(&observation))
        }
        InvocationOperation::Refresh => {
            let id = task_id_argument(arguments, "refresh_subagent")?;
            let observation = context
                .coordinator()
                .get(context.caller(), &id)
                .await
                .map_err(access_error)?;
            Ok(observation_value(&observation))
        }
        InvocationOperation::Wait => {
            let id = task_id_argument(arguments, "wait_subagent")?;
            match context
                .coordinator()
                .wait_with_phase(context.caller(), &id, wait_phase.as_ref())
                .await
            {
                Ok((result, observation)) => Ok(wait_value(
                    "completed",
                    observation,
                    Some(result_value(&id, result)),
                    None,
                )),
                Err(WaitError::Access(error)) => Err(access_error(error)),
                Err(error) => {
                    let message = error.to_string();
                    let observation = error
                        .observation()
                        .expect("terminal wait errors carry observations")
                        .clone();
                    let outcome = match observation.state() {
                        TaskState::TimedOut => "timed_out",
                        TaskState::Failed | TaskState::Cancelled => "failed",
                        state => unreachable!("wait errors have terminal states, got {state:?}"),
                    };
                    Ok(wait_value(outcome, observation, None, Some(message)))
                }
            }
        }
        InvocationOperation::Cancel => {
            let id = task_id_argument(arguments, "cancel_subagent")?;
            let observation = context
                .coordinator()
                .cancel_with_phase(context.caller(), &id, wait_phase.as_ref())
                .await
                .map_err(access_error)?;
            Ok(observation_value(&observation))
        }
    }
}

fn start_arguments(arguments: ToolValue) -> Result<(String, Option<String>, bool), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "start_subagent requires prompt, model, and read_only positional arguments",
        ));
    };
    let [prompt, model, read_only] = arguments.as_slice() else {
        return Err(ToolError::type_error(
            "start_subagent requires prompt, model, and read_only positional arguments",
        ));
    };
    let ToolValue::String(prompt) = prompt else {
        return Err(ToolError::type_error("subagent prompt must be a string"));
    };
    if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
        return Err(ToolError::value(format!(
            "subagent prompt must be non-empty and at most {MAX_PROMPT_BYTES} UTF-8 bytes"
        )));
    }
    let model = match model {
        ToolValue::None => None,
        ToolValue::String(model) => Some(model.clone()),
        _ => {
            return Err(ToolError::type_error(
                "subagent model must be a string or None",
            ));
        }
    };
    let ToolValue::Bool(read_only) = read_only else {
        return Err(ToolError::type_error(
            "subagent read_only must be a boolean",
        ));
    };
    Ok((prompt.clone(), model, *read_only))
}

fn task_id_argument(arguments: ToolValue, operation: &str) -> Result<TaskId, ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(format!(
            "{operation} requires one task ID positional argument"
        )));
    };
    let [id] = arguments.as_slice() else {
        return Err(ToolError::type_error(format!(
            "{operation} requires one task ID positional argument"
        )));
    };
    let ToolValue::String(id) = id else {
        return Err(ToolError::type_error("subagent task ID must be a string"));
    };
    TaskId::parse(id).ok_or_else(|| {
        ToolError::value(
            "subagent task ID must be a canonical lowercase hexadecimal string with at least two characters",
        )
    })
}

fn access_error(error: AccessError) -> ToolError {
    match error {
        AccessError::InvalidCaller => ToolError::runtime(error.to_string()),
        AccessError::UnknownTask(_) => ToolError::value(error.to_string()),
        AccessError::Denied(_) => ToolError::permission(error.to_string()),
    }
}

fn task_handle_value(observation: &TaskObservation) -> ToolValue {
    ToolValue::Dictionary(BTreeMap::from([
        (
            "id".to_owned(),
            ToolValue::String(observation.id().as_str().to_owned()),
        ),
        ("status".to_owned(), observation_value(observation)),
    ]))
}

fn observation_value(observation: &TaskObservation) -> ToolValue {
    ToolValue::Dictionary(BTreeMap::from([
        (
            "id".to_owned(),
            ToolValue::String(observation.id().as_str().to_owned()),
        ),
        (
            "state".to_owned(),
            ToolValue::String(task_state_name(observation.state()).to_owned()),
        ),
        (
            "model".to_owned(),
            ToolValue::String(observation.model().to_owned()),
        ),
        (
            "thinking".to_owned(),
            ToolValue::String(observation.thinking().to_owned()),
        ),
        (
            "step_count".to_owned(),
            ToolValue::Integer(i64::try_from(observation.step_count()).unwrap_or(i64::MAX)),
        ),
        (
            "elapsed_ms".to_owned(),
            ToolValue::Integer(
                i64::try_from(observation.elapsed().as_millis()).unwrap_or(i64::MAX),
            ),
        ),
        (
            "observed_at_ms".to_owned(),
            ToolValue::Integer(observation.observed_at_ms()),
        ),
        (
            "estimated_cost_usd".to_owned(),
            observation
                .estimated_cost_usd()
                .map_or(ToolValue::None, ToolValue::Float),
        ),
        (
            "cost_status".to_owned(),
            ToolValue::String(cost_status_name(observation.cost_status()).to_owned()),
        ),
        (
            "error".to_owned(),
            observation
                .error()
                .map_or(ToolValue::None, |error| ToolValue::String(error.to_owned())),
        ),
    ]))
}

fn result_value(id: &TaskId, result: ChildResult) -> ToolValue {
    let (message, values) = result.into_parts();
    ToolValue::Dictionary(BTreeMap::from([
        ("id".to_owned(), ToolValue::String(id.as_str().to_owned())),
        ("message".to_owned(), ToolValue::String(message)),
        ("values".to_owned(), ToolValue::Dictionary(values)),
    ]))
}

fn wait_value(
    outcome: &str,
    observation: TaskObservation,
    result: Option<ToolValue>,
    error: Option<String>,
) -> ToolValue {
    ToolValue::Dictionary(BTreeMap::from([
        ("outcome".to_owned(), ToolValue::String(outcome.to_owned())),
        ("status".to_owned(), observation_value(&observation)),
        ("result".to_owned(), result.unwrap_or(ToolValue::None)),
        (
            "error".to_owned(),
            error.map_or(ToolValue::None, ToolValue::String),
        ),
    ]))
}

fn task_state_name(state: TaskState) -> &'static str {
    match state {
        TaskState::Starting => "starting",
        TaskState::Running => "running",
        TaskState::Stopping => "stopping",
        TaskState::Completed => "completed",
        TaskState::Failed => "failed",
        TaskState::Cancelled => "cancelled",
        TaskState::TimedOut => "timed_out",
    }
}

fn cost_status_name(status: CostStatus) -> &'static str {
    match status {
        CostStatus::Complete => "complete",
        CostStatus::LowerBound => "lower_bound",
        CostStatus::Unavailable => "unavailable",
    }
}

pub(crate) trait ChildModelFactory: Send + Sync + 'static {
    type Model: Model;

    fn create(&self, prompt: String, selection: ModelSelection) -> Self::Model;
}

struct ChildExecutionSpan<'a> {
    trace: &'a Trace,
    child_id: &'a TaskId,
    cancellation: &'a CancellationToken,
    model_step: Option<u64>,
}

impl<'a> ChildExecutionSpan<'a> {
    fn new(trace: &'a Trace, child_id: &'a TaskId, cancellation: &'a CancellationToken) -> Self {
        Self {
            trace,
            child_id,
            cancellation,
            model_step: None,
        }
    }

    fn started(&mut self, model_step: u64) {
        assert!(
            self.model_step.is_none(),
            "child execution started before the previous execution settled"
        );
        self.model_step = Some(model_step);
        self.trace.emit(|| TraceEvent::ChildExecutionStarted {
            child_id: self.child_id.clone(),
            model_step,
        });
    }

    fn completed(&mut self) {
        let model_step = self
            .model_step
            .take()
            .expect("child execution completed without starting");
        self.trace.emit(|| TraceEvent::ChildExecutionSettled {
            child_id: self.child_id.clone(),
            model_step,
            outcome: Settlement::Succeeded,
        });
    }
}

impl Drop for ChildExecutionSpan<'_> {
    fn drop(&mut self) {
        let Some(model_step) = self.model_step else {
            return;
        };
        let outcome = if self.cancellation.is_cancelled() {
            Settlement::Cancelled
        } else {
            Settlement::Failed
        };
        self.trace.emit(|| TraceEvent::ChildExecutionSettled {
            child_id: self.child_id.clone(),
            model_step,
            outcome,
        });
    }
}

pub(crate) struct CompleteChildRunner<F> {
    prompt_base: String,
    models: F,
}

impl<F> CompleteChildRunner<F> {
    pub(crate) fn new(prompt_base: String, models: F) -> Self {
        Self {
            prompt_base,
            models,
        }
    }
}

impl<F: ChildModelFactory + Clone> ChildRunner<ChildPayload> for CompleteChildRunner<F> {
    fn run(&self, child: ChildRun<ChildPayload>, cancellation: CancellationToken) -> RunnerFuture {
        let prompt_base = self.prompt_base.clone();
        let models = self.models.clone();
        let trace = child.trace();
        let (id, caller, _parent, _depth, parameters, payload, reporter, coordinator) =
            child.into_parts();
        let (request, selection, read_only) = parameters.into_parts();
        let accepted_thinking = payload.accepted_thinking.clone();
        let child_prompt = child_prompt(&prompt_base, payload.source.prompt_reference(read_only));
        let model = models.create(child_prompt, selection);
        Box::pin(async move {
            trace.emit(|| TraceEvent::ChildArenaConstructionStarted {
                child_id: id.clone(),
            });
            let arena_result = {
                let mut construction_progress = |phase| {
                    trace.emit(|| TraceEvent::child_arena_phase(id.clone(), phase));
                };
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        trace.emit(|| TraceEvent::ChildArenaSettled {
                            child_id: id.clone(),
                            outcome: Settlement::Cancelled,
                        });
                        return Err(ChildFailure::new(format!(
                            "subagent {id} was cancelled before arena construction"
                        )));
                    }
                    result = payload.source.child(&payload.checkpoint, read_only, &mut construction_progress) => result,
                }
            };
            let arena = match arena_result {
                Ok(arena) => {
                    trace.emit(|| TraceEvent::ChildArenaSettled {
                        child_id: id.clone(),
                        outcome: Settlement::Succeeded,
                    });
                    arena
                }
                Err(error) => {
                    trace.emit(|| TraceEvent::ChildArenaSettled {
                        child_id: id.clone(),
                        outcome: Settlement::Failed,
                    });
                    return Err(ChildFailure::new(error.to_string()));
                }
            };
            let mut agent = Agent::new(arena, model);
            let mut context = AgentInvocationContext::new(caller, coordinator);
            if let Some(accepted_thinking) = accepted_thinking {
                context = context.with_accepted_thinking(accepted_thinking);
            }
            agent.set_invocation_context(context);
            let mut model_step: u64 = 0;
            let mut execution_span = ChildExecutionSpan::new(&trace, &id, &cancellation);
            let mut progress = |event| match event {
                AgentProgress::ModelStepStarted => {
                    model_step = model_step
                        .checked_add(1)
                        .expect("subagent model step ordinal overflowed");
                    reporter.model_step_started();
                    trace.emit(|| TraceEvent::ChildModelStepStarted {
                        child_id: id.clone(),
                        model_step,
                    });
                }
                AgentProgress::ModelProgress(ModelProgress::AttemptStarted { attempt }) => {
                    reporter.attempt_started(model_step, attempt);
                    trace.emit(|| TraceEvent::ChildProviderAttemptStarted {
                        child_id: id.clone(),
                        model_step,
                        attempt,
                    });
                }
                AgentProgress::ModelProgress(ModelProgress::AttemptSettled {
                    attempt,
                    accounting,
                }) => {
                    reporter.attempt_settled(model_step, attempt, accounting);
                    trace.emit(|| TraceEvent::ChildProviderAttemptSettled {
                        child_id: id.clone(),
                        model_step,
                        attempt,
                    });
                }
                AgentProgress::ExecutionStarted { .. } => execution_span.started(model_step),
                AgentProgress::ExecutionCompleted {
                    source,
                    reasoning_summary,
                    execution,
                    attempts,
                } => {
                    execution_span.completed();
                    reporter.execution_completed(
                        model_step,
                        source,
                        reasoning_summary,
                        execution,
                        attempts,
                    );
                }
                AgentProgress::ToolUpdated { .. }
                | AgentProgress::RunPreviewUpdated { .. }
                | AgentProgress::StdoutDelta { .. }
                | AgentProgress::StdoutTruncated
                | AgentProgress::ModelProgress(_) => {}
            };
            match agent
                .run_with_progress(request, &cancellation, &mut progress)
                .await
            {
                Ok(AgentRunOutcome::FinalMessageStaged) => {
                    let final_message = agent
                        .discard_staged_final_message()
                        .expect("completed child must stage a final message");
                    reporter.set_attempts(final_message.attempts().to_vec());
                    if cancellation.is_cancelled() {
                        return Err(ChildFailure::new(format!(
                            "subagent {id} was cancelled before result admission"
                        )));
                    }
                    let message = final_message.text().to_owned();
                    let values = agent.collect_child_result_values(&cancellation).await;
                    if cancellation.is_cancelled() {
                        return Err(ChildFailure::new(format!(
                            "subagent {id} was cancelled before result admission"
                        )));
                    }
                    match values {
                        Ok(values) => Ok(ChildResult::new(message, values)),
                        Err(error) => Err(ChildFailure::new(format!(
                            "{error}; final message: {message}"
                        ))),
                    }
                }
                Err(error) => {
                    reporter.set_attempts(error.attempts().to_vec());
                    Err(ChildFailure::new(error.to_string()))
                }
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostStatus {
    Complete,
    LowerBound,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskObservation {
    id: TaskId,
    state: TaskState,
    selection: ModelSelection,
    step_count: u64,
    elapsed: Duration,
    observed_at_ms: i64,
    cost: RunCostEstimate,
    error: Option<String>,
}

impl TaskObservation {
    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    pub(crate) fn state(&self) -> TaskState {
        self.state
    }

    #[cfg(test)]
    pub(crate) fn selection(&self) -> ModelSelection {
        self.selection
    }

    pub(crate) fn model(&self) -> &'static str {
        self.selection.model().id
    }

    pub(crate) fn thinking(&self) -> &'static str {
        self.selection.thinking()
    }

    pub(crate) fn step_count(&self) -> u64 {
        self.step_count
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub(crate) fn observed_at_ms(&self) -> i64 {
        self.observed_at_ms
    }

    pub(crate) fn estimated_cost_usd(&self) -> Option<f64> {
        match self.cost {
            RunCostEstimate::Complete(amount) | RunCostEstimate::LowerBound(amount) => {
                Some(amount.as_nanos() as f64 / 1_000_000_000.0)
            }
            RunCostEstimate::Unavailable => None,
        }
    }

    pub(crate) fn cost(&self) -> RunCostEstimate {
        self.cost
    }

    pub(crate) fn cost_status(&self) -> CostStatus {
        match self.cost {
            RunCostEstimate::Complete(_) => CostStatus::Complete,
            RunCostEstimate::LowerBound(_) => CostStatus::LowerBound,
            RunCostEstimate::Unavailable => CostStatus::Unavailable,
        }
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TaskOutcome {
    Completed(ChildResult),
    Failed(ChildFailure),
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskRecord {
    id: TaskId,
    revision: u64,
    parent: Option<TaskId>,
    origin: OriginId,
    depth: usize,
    parameters: ChildParameters,
    observation: TaskObservation,
    attempt_evidence: Vec<TaskAttempt>,
    attempts: Vec<AttemptAccounting>,
    executions: Vec<TaskExecution>,
    outcome: Option<TaskOutcome>,
}

impl TaskRecord {
    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn parent(&self) -> Option<&TaskId> {
        self.parent.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn origin(&self) -> u64 {
        self.origin.0
    }

    #[cfg(test)]
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn parameters(&self) -> &ChildParameters {
        &self.parameters
    }

    pub(crate) fn observation(&self) -> &TaskObservation {
        &self.observation
    }

    pub(crate) fn attempts(&self) -> &[AttemptAccounting] {
        &self.attempts
    }

    pub(crate) fn attempt_evidence(&self) -> &[TaskAttempt] {
        &self.attempt_evidence
    }

    #[cfg(test)]
    pub(crate) fn executions(&self) -> &[TaskExecution] {
        &self.executions
    }

    pub(crate) fn outcome(&self) -> Option<&TaskOutcome> {
        self.outcome.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        id: &str,
        revision: u64,
        selection: ModelSelection,
        parent: Option<&str>,
        depth: usize,
        attempts: Vec<AttemptAccounting>,
    ) -> Self {
        let id = TaskId::parse(id).expect("test task ID must be valid");
        let parent = parent.map(|id| TaskId::parse(id).expect("test parent ID must be valid"));
        let cost = estimate_run_cost(&attempts);
        Self {
            id: id.clone(),
            revision,
            parent,
            origin: OriginId(0),
            depth,
            parameters: ChildParameters::new("test child".to_owned(), selection, false),
            observation: TaskObservation {
                id,
                state: TaskState::Completed,
                selection,
                step_count: 0,
                elapsed: Duration::ZERO,
                observed_at_ms: 0,
                cost,
                error: None,
            },
            attempt_evidence: Vec::new(),
            attempts,
            executions: Vec::new(),
            outcome: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_observation_for_test(
        mut self,
        elapsed: Duration,
        observed_at_ms: i64,
    ) -> Self {
        self.observation.elapsed = elapsed;
        self.observation.observed_at_ms = observed_at_ms;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SubagentPolicy {
    allow_nested: bool,
    max_concurrent: usize,
    max_starts_per_run: usize,
    max_depth: usize,
    timeout: Duration,
}

impl From<SubagentSettings> for SubagentPolicy {
    fn from(settings: SubagentSettings) -> Self {
        Self {
            allow_nested: settings.allow_nested(),
            max_concurrent: settings.max_concurrent(),
            max_starts_per_run: settings.max_starts_per_run(),
            max_depth: settings.max_depth(),
            timeout: settings.timeout(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum AdmissionError {
    #[error("subagent admission is unavailable during shutdown")]
    ShuttingDown,
    #[error("subagent caller is not trusted by this session")]
    InvalidCaller,
    #[error("subagent caller is no longer running")]
    CallerStopped,
    #[error("nested subagent admission is disabled")]
    NestedDisabled,
    #[error("subagent depth limit is exhausted")]
    DepthLimit,
    #[error("subagent concurrency limit is exhausted")]
    ConcurrentLimit,
    #[error("subagent start budget is exhausted for this request")]
    StartBudget,
    #[error("subagent timeout cannot be represented as a deadline")]
    DeadlineUnrepresentable,
    #[error("subagent task identifier space is exhausted")]
    IdentifierSpaceExhausted,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum AccessError {
    #[error("subagent caller is not trusted by this session")]
    InvalidCaller,
    #[error("unknown subagent `{0}`")]
    UnknownTask(TaskId),
    #[error("subagent caller is not authorised for `{0}`")]
    Denied(TaskId),
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub(crate) enum WaitError {
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error("subagent `{id}` failed: {message}")]
    Failed {
        id: TaskId,
        message: String,
        observation: Box<TaskObservation>,
    },
    #[error("subagent `{id}` was cancelled")]
    Cancelled {
        id: TaskId,
        observation: Box<TaskObservation>,
    },
    #[error("subagent `{id}` timed out")]
    TimedOut {
        id: TaskId,
        observation: Box<TaskObservation>,
    },
}

impl WaitError {
    pub(crate) fn observation(&self) -> Option<&TaskObservation> {
        match self {
            Self::Access(_) => None,
            Self::Failed { observation, .. }
            | Self::Cancelled { observation, .. }
            | Self::TimedOut { observation, .. } => Some(observation),
        }
    }
}

pub(crate) trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn unix_time_ms(&self) -> i64;
}

pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn unix_time_ms(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
            Err(error) => i64::try_from(error.duration().as_millis())
                .ok()
                .and_then(|milliseconds| milliseconds.checked_neg())
                .unwrap_or(i64::MIN),
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct StartSubagentTestGate {
    first_start_locked: Arc<Semaphore>,
    first_start_allowed: Arc<Semaphore>,
    start_attempts: Arc<Semaphore>,
    first_start_seen: Arc<AtomicBool>,
}

#[cfg(test)]
impl StartSubagentTestGate {
    pub(crate) fn new() -> Self {
        Self {
            first_start_locked: Arc::new(Semaphore::new(0)),
            first_start_allowed: Arc::new(Semaphore::new(0)),
            start_attempts: Arc::new(Semaphore::new(0)),
            first_start_seen: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) async fn wait_until_first_start_locked(&self) {
        take_test_permit(&self.first_start_locked).await;
    }

    pub(crate) async fn wait_until_start_attempts(&self, count: usize) {
        for _ in 0..count {
            take_test_permit(&self.start_attempts).await;
        }
    }

    pub(crate) fn allow_first_start(&self) {
        self.first_start_allowed.add_permits(1);
    }

    fn note_start_attempt(&self) {
        self.start_attempts.add_permits(1);
    }

    async fn wait_before_first_start_returns(&self) {
        if !self.first_start_seen.swap(true, Ordering::SeqCst) {
            self.first_start_locked.add_permits(1);
            take_test_permit(&self.first_start_allowed).await;
        }
    }
}

#[cfg(test)]
async fn take_test_permit(semaphore: &Arc<Semaphore>) {
    semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("test gate semaphore remains open")
        .forget();
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct RunChildTestGate {
    before_lock: Arc<Semaphore>,
    allow_lock: Arc<Semaphore>,
    after_lock: Arc<Semaphore>,
    allow_finish: Arc<Semaphore>,
}

#[cfg(test)]
impl RunChildTestGate {
    pub(crate) fn new() -> Self {
        Self {
            before_lock: Arc::new(Semaphore::new(0)),
            allow_lock: Arc::new(Semaphore::new(0)),
            after_lock: Arc::new(Semaphore::new(0)),
            allow_finish: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_before_lock(&self) {
        take_test_permit(&self.before_lock).await;
    }

    pub(crate) fn allow_lock(&self) {
        self.allow_lock.add_permits(1);
    }

    pub(crate) async fn wait_until_after_lock(&self) {
        take_test_permit(&self.after_lock).await;
    }

    pub(crate) fn allow_finish(&self) {
        self.allow_finish.add_permits(1);
    }

    async fn wait_before_lock(&self) {
        self.before_lock.add_permits(1);
        take_test_permit(&self.allow_lock).await;
    }

    async fn wait_after_lock(&self) {
        self.after_lock.add_permits(1);
        take_test_permit(&self.allow_finish).await;
    }
}

pub(crate) struct SubagentCoordinator<P: Send + 'static> {
    inner: Arc<Mutex<Inner>>,
    publication: Arc<SyncMutex<CoordinatorPublication>>,
    policy: SubagentPolicy,
    runner: Arc<dyn ChildRunner<P>>,
    clock: Arc<dyn Clock>,
    trace: Trace,
    #[cfg(test)]
    start_test_gate: Option<StartSubagentTestGate>,
    #[cfg(test)]
    run_child_test_gate: Option<RunChildTestGate>,
}

impl<P: Send + 'static> Clone for SubagentCoordinator<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            publication: self.publication.clone(),
            policy: self.policy,
            runner: self.runner.clone(),
            clock: self.clock.clone(),
            trace: self.trace.clone(),
            #[cfg(test)]
            start_test_gate: self.start_test_gate.clone(),
            #[cfg(test)]
            run_child_test_gate: self.run_child_test_gate.clone(),
        }
    }
}

struct CoordinatorLockGuard<'a> {
    inner: MutexGuard<'a, Inner>,
    trace: &'a Trace,
    operation: CoordinatorOperation,
}

impl Deref for CoordinatorLockGuard<'_> {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for CoordinatorLockGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for CoordinatorLockGuard<'_> {
    fn drop(&mut self) {
        if !self.operation.is_maintenance() {
            self.trace.emit(|| TraceEvent::CoordinatorLockReleased {
                operation: self.operation,
            });
        }
    }
}

struct Inner {
    session: u64,
    next_origin: u64,
    next_task: Option<u64>,
    active_slots: usize,
    shutting_down: bool,
    blocked_origins: BTreeSet<OriginId>,
    origin_starts: BTreeMap<OriginId, usize>,
    notifications: watch::Sender<u64>,
    tasks: BTreeMap<TaskId, Task>,
}

impl Inner {
    fn notify(&self) {
        self.notifications
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

struct Task {
    id: TaskId,
    parent: Option<TaskId>,
    origin: OriginId,
    depth: usize,
    parameters: ChildParameters,
    admitted_at: Instant,
    deadline: Instant,
    settled_at: Option<Instant>,
    metrics: Arc<TaskMetrics>,
    state: TaskState,
    stop: Option<StopReason>,
    outcome: Option<TaskOutcome>,
    cancellation: CancellationToken,
}

#[derive(Clone)]
enum StopReason {
    Cancelled,
    TimedOut,
    Shutdown,
    AncestorStopped,
    RunnerFailed(ChildFailure),
}

impl StopReason {
    fn outcome(&self) -> TaskOutcome {
        match self {
            Self::Cancelled | Self::Shutdown | Self::AncestorStopped => TaskOutcome::Cancelled,
            Self::TimedOut => TaskOutcome::TimedOut,
            Self::RunnerFailed(failure) => TaskOutcome::Failed(failure.clone()),
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Cancelled => "cancellation requested".to_owned(),
            Self::TimedOut => "deadline expired".to_owned(),
            Self::Shutdown => "session shutdown requested".to_owned(),
            Self::AncestorStopped => "ancestor stopped".to_owned(),
            Self::RunnerFailed(failure) => failure.message().to_owned(),
        }
    }
}

impl Task {
    fn observation(&self, now: Instant, observed_at_ms: i64) -> TaskObservation {
        self.observation_with_metrics(now, observed_at_ms, self.metrics.snapshot())
    }

    fn observation_with_metrics(
        &self,
        now: Instant,
        observed_at_ms: i64,
        metrics: TaskMetricSnapshot,
    ) -> TaskObservation {
        let finished_at = self.settled_at.unwrap_or(now);
        let cost = estimate_run_cost(&metrics.attempts);
        TaskObservation {
            id: self.id.clone(),
            state: self.state,
            selection: self.parameters.selection(),
            step_count: metrics.step_count,
            elapsed: finished_at.saturating_duration_since(self.admitted_at),
            observed_at_ms,
            cost,
            error: match (&self.state, &self.stop, &self.outcome) {
                (TaskState::Stopping, Some(reason), _) => Some(reason.message()),
                (_, _, Some(TaskOutcome::Failed(failure))) => Some(failure.message().to_owned()),
                _ => None,
            },
        }
    }

    fn record(&self, now: Instant, observed_at_ms: i64) -> TaskRecord {
        let metrics = self.metrics.snapshot();
        TaskRecord {
            id: self.id.clone(),
            revision: metrics.revision,
            parent: self.parent.clone(),
            origin: self.origin,
            depth: self.depth,
            parameters: self.parameters.clone(),
            observation: self.observation_with_metrics(now, observed_at_ms, metrics.clone()),
            attempt_evidence: metrics.attempt_evidence,
            attempts: metrics.attempts,
            executions: metrics.executions,
            outcome: self.outcome.clone(),
        }
    }

    fn transition(&mut self, state: TaskState) -> bool {
        if self.state == state {
            return false;
        }
        self.state = state;
        self.metrics.revise().is_some()
    }
}

impl<P: Send + 'static> SubagentCoordinator<P> {
    pub(crate) fn new(policy: SubagentPolicy, runner: Arc<dyn ChildRunner<P>>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
                next_origin: 1,
                next_task: Some(0),
                active_slots: 0,
                shutting_down: false,
                blocked_origins: BTreeSet::new(),
                origin_starts: BTreeMap::new(),
                notifications: watch::channel(0).0,
                tasks: BTreeMap::new(),
            })),
            publication: Arc::new(SyncMutex::new(CoordinatorPublication::default())),
            policy,
            runner,
            clock: Arc::new(SystemClock),
            trace: Trace::disabled(),
            #[cfg(test)]
            start_test_gate: None,
            #[cfg(test)]
            run_child_test_gate: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_start_test_gate(mut self, start_test_gate: StartSubagentTestGate) -> Self {
        self.start_test_gate = Some(start_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_run_child_test_gate(
        mut self,
        run_child_test_gate: RunChildTestGate,
    ) -> Self {
        self.run_child_test_gate = Some(run_child_test_gate);
        self
    }

    pub(crate) fn with_trace(mut self, trace: Trace) -> Self {
        self.trace = trace;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_clock(
        policy: SubagentPolicy,
        runner: Arc<dyn ChildRunner<P>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let mut coordinator = Self::new(policy, runner);
        coordinator.clock = clock;
        coordinator
    }

    pub(crate) fn take_event_receiver(&self) -> Option<mpsc::UnboundedReceiver<CoordinatorEvent>> {
        let mut publication = self
            .publication
            .lock()
            .expect("coordinator publication lock");
        if publication.taken {
            return None;
        }
        publication.taken = true;
        let (sender, receiver) = mpsc::unbounded_channel();
        publication.sender = Some(sender);
        Some(receiver)
    }

    fn publish_event(&self, event: CoordinatorEvent) {
        if let Some(sender) = self
            .publication
            .lock()
            .expect("coordinator publication lock")
            .sender
            .as_ref()
        {
            let _ = sender.send(event);
        }
    }

    async fn lock_inner(&self, operation: CoordinatorOperation) -> CoordinatorLockGuard<'_> {
        if operation.is_maintenance() {
            let wait_started = self.trace.is_enabled().then(Instant::now);
            let inner = self.inner.lock().await;
            if let Some(wait_started) = wait_started {
                let wait = wait_started.elapsed();
                if wait >= MAINTENANCE_LOCK_TRACE_THRESHOLD {
                    self.trace.emit(|| TraceEvent::CoordinatorLockContended {
                        operation,
                        wait_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                    });
                }
            }
            return CoordinatorLockGuard {
                inner,
                trace: &self.trace,
                operation,
            };
        }

        self.trace
            .emit(|| TraceEvent::CoordinatorLockWaiting { operation });
        let inner = self.inner.lock().await;
        self.trace
            .emit(|| TraceEvent::CoordinatorLockAcquired { operation });
        CoordinatorLockGuard {
            inner,
            trace: &self.trace,
            operation,
        }
    }

    pub(crate) async fn root_caller(&self) -> Caller {
        let mut inner = self.lock_inner(CoordinatorOperation::RootCaller).await;
        let origin = OriginId(inner.next_origin);
        inner.next_origin = inner
            .next_origin
            .checked_add(1)
            .expect("subagent origin identifier space is exhausted");
        Caller {
            session: inner.session,
            owner: CallerOwner::Root,
            origin,
        }
    }

    pub(crate) async fn start(
        &self,
        caller: &Caller,
        mut parameters: ChildParameters,
        payload: P,
    ) -> Result<TaskObservation, AdmissionError> {
        #[cfg(test)]
        if let Some(start_test_gate) = &self.start_test_gate {
            start_test_gate.note_start_attempt();
        }
        let (child, observation, spawned) = {
            let mut inner = self.lock_inner(CoordinatorOperation::Start).await;
            #[cfg(test)]
            if let Some(start_test_gate) = &self.start_test_gate {
                start_test_gate.wait_before_first_start_returns().await;
            }
            let now = self.now();
            let observed_at_ms = self.unix_time_ms();
            if inner.shutting_down {
                return Err(AdmissionError::ShuttingDown);
            }
            if caller.session != inner.session {
                return Err(AdmissionError::InvalidCaller);
            }
            if inner.blocked_origins.contains(&caller.origin) {
                return Err(AdmissionError::CallerStopped);
            }

            let (parent, depth, inherited_read_only, inherited_deadline) = match &caller.owner {
                CallerOwner::Root => (None, 1, false, None),
                CallerOwner::Child(parent) => {
                    let Some(task) = inner.tasks.get(parent) else {
                        return Err(AdmissionError::InvalidCaller);
                    };
                    if task.origin != caller.origin {
                        return Err(AdmissionError::InvalidCaller);
                    }
                    if task.state.is_terminal() || task.stop.is_some() || task.deadline <= now {
                        return Err(AdmissionError::CallerStopped);
                    }
                    if !self.policy.allow_nested {
                        return Err(AdmissionError::NestedDisabled);
                    }
                    (
                        Some(parent.clone()),
                        task.depth.saturating_add(1),
                        task.parameters.read_only(),
                        Some(task.deadline),
                    )
                }
            };
            if depth > self.policy.max_depth {
                return Err(AdmissionError::DepthLimit);
            }
            if inner.active_slots >= self.policy.max_concurrent {
                return Err(AdmissionError::ConcurrentLimit);
            }
            if inner
                .origin_starts
                .get(&caller.origin)
                .copied()
                .unwrap_or_default()
                >= self.policy.max_starts_per_run
            {
                return Err(AdmissionError::StartBudget);
            }
            let deadline =
                earliest_deadline(now.checked_add(self.policy.timeout), inherited_deadline)
                    .ok_or(AdmissionError::DeadlineUnrepresentable)?;
            let sequence = inner
                .next_task
                .ok_or(AdmissionError::IdentifierSpaceExhausted)?;
            let id = task_id(sequence);
            parameters.read_only |= inherited_read_only;
            let cancellation = CancellationToken::new();
            let metrics = Arc::new(TaskMetrics::new(
                id.clone(),
                inner.notifications.clone(),
                self.publication.clone(),
            ));
            let task = Task {
                id: id.clone(),
                parent: parent.clone(),
                origin: caller.origin,
                depth,
                parameters: parameters.clone(),
                admitted_at: now,
                deadline,
                settled_at: None,
                metrics: metrics.clone(),
                state: TaskState::Starting,
                stop: None,
                outcome: None,
                cancellation: cancellation.clone(),
            };
            let observation = task.observation(now, observed_at_ms);
            let spawned = task.record(now, observed_at_ms);
            inner.next_task = sequence.checked_add(1);
            inner.active_slots += 1;
            *inner.origin_starts.entry(caller.origin).or_default() += 1;
            inner.tasks.insert(id.clone(), task);
            inner
                .notifications
                .send_modify(|revision| *revision = revision.wrapping_add(1));
            (
                ChildRun {
                    id,
                    caller: Caller {
                        session: caller.session,
                        owner: CallerOwner::Child(observation.id.clone()),
                        origin: caller.origin,
                    },
                    parent,
                    depth,
                    parameters,
                    payload,
                    reporter: TaskReporter { metrics },
                    coordinator: self.clone(),
                    trace: self.trace.clone(),
                },
                observation,
                spawned,
            )
        };

        self.trace.emit(|| {
            TraceEvent::child_admitted(
                &child.id,
                child.parent.as_ref(),
                child.parameters.selection(),
            )
        });
        self.publish_event(CoordinatorEvent::Spawned { record: spawned });
        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator.run_child(child).await;
        });
        Ok(observation)
    }

    pub(crate) async fn subscribe(&self) -> watch::Receiver<u64> {
        self.lock_inner(CoordinatorOperation::Subscribe)
            .await
            .notifications
            .subscribe()
    }

    pub(crate) async fn get(
        &self,
        caller: &Caller,
        id: &TaskId,
    ) -> Result<TaskObservation, AccessError> {
        let inner = self.lock_inner(CoordinatorOperation::Get).await;
        ensure_access(&inner, caller, id)?;
        let task = inner
            .tasks
            .get(id)
            .expect("authorised task must be retained");
        Ok(task.observation(self.now(), self.unix_time_ms()))
    }

    #[cfg(test)]
    pub(crate) async fn wait(
        &self,
        caller: &Caller,
        id: &TaskId,
    ) -> Result<(ChildResult, TaskObservation), WaitError> {
        self.wait_with_phase(caller, id, None).await
    }

    async fn wait_with_phase(
        &self,
        caller: &Caller,
        id: &TaskId,
        wait_phase: Option<&WaitPhaseSignal>,
    ) -> Result<(ChildResult, TaskObservation), WaitError> {
        let updates = {
            let inner = self.lock_inner(CoordinatorOperation::WaitInitial).await;
            ensure_access(&inner, caller, id)?;
            inner
                .tasks
                .get(id)
                .expect("authorised task must be retained")
                .metrics
                .subscribe()
        };
        if let Some(wait_phase) = wait_phase {
            wait_phase.notify();
        }
        self.wait_for_outcome(id, updates).await
    }

    #[cfg(test)]
    pub(crate) async fn cancel(
        &self,
        caller: &Caller,
        id: &TaskId,
    ) -> Result<TaskObservation, AccessError> {
        self.cancel_with_phase(caller, id, None).await
    }

    async fn cancel_with_phase(
        &self,
        caller: &Caller,
        id: &TaskId,
        wait_phase: Option<&WaitPhaseSignal>,
    ) -> Result<TaskObservation, AccessError> {
        {
            let inner = self.lock_inner(CoordinatorOperation::Cancel).await;
            ensure_access(&inner, caller, id)?;
        }
        let stopping = self.request_stop_subtree(id, StopReason::Cancelled).await;
        if !stopping.is_empty() {
            if let Some(wait_phase) = wait_phase {
                wait_phase.notify();
            }
            self.wait_for_all(stopping).await;
        }
        self.get(caller, id).await
    }

    pub(crate) async fn request_background_cancellation(&self, active: Option<&Caller>) {
        self.request_stop_all(StopReason::Cancelled, active).await;
    }

    pub(crate) async fn join_active(&self) {
        let active = self
            .lock_inner(CoordinatorOperation::JoinActive)
            .await
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.id.clone())
            .collect();
        self.wait_for_all(active).await;
    }

    #[cfg(test)]
    pub(crate) async fn cancel_all(&self) {
        self.request_background_cancellation(None).await;
        self.join_active().await;
    }

    pub(crate) async fn service(&self) {
        let now = self.now();
        let mut expired = {
            let inner = self.lock_inner(CoordinatorOperation::Service).await;
            inner
                .tasks
                .values()
                .filter(|task| {
                    !task.state.is_terminal() && task.stop.is_none() && task.deadline <= now
                })
                .map(|task| (task.depth, task.id.clone()))
                .collect::<Vec<_>>()
        };
        expired.sort_unstable();
        for (_, id) in expired {
            self.request_timeout(&id).await;
        }
    }

    pub(crate) async fn next_deadline(&self) -> Option<Instant> {
        self.lock_inner(CoordinatorOperation::NextDeadline)
            .await
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal() && task.stop.is_none())
            .map(|task| task.deadline)
            .min()
    }

    pub(crate) async fn shutdown(&self) {
        {
            let mut inner = self.lock_inner(CoordinatorOperation::Shutdown).await;
            inner.shutting_down = true;
            inner.notify();
        }
        self.request_stop_all(StopReason::Shutdown, None).await;
        self.join_active().await;
    }

    #[cfg(test)]
    pub(crate) async fn records(&self) -> Vec<TaskRecord> {
        let inner = self.inner.lock().await;
        let now = self.now();
        let observed_at_ms = self.unix_time_ms();
        inner
            .tasks
            .values()
            .map(|task| task.record(now, observed_at_ms))
            .collect()
    }

    pub(crate) async fn record(&self, id: &TaskId) -> Option<TaskRecord> {
        let inner = self.lock_inner(CoordinatorOperation::Record).await;
        let now = self.now();
        let observed_at_ms = self.unix_time_ms();
        inner
            .tasks
            .get(id)
            .map(|task| task.record(now, observed_at_ms))
    }

    #[cfg(test)]
    pub(crate) async fn active_count(&self) -> usize {
        self.inner.lock().await.active_slots
    }

    async fn run_child(&self, child: ChildRun<P>) {
        let id = child.id().clone();
        self.trace.emit(|| TraceEvent::ChildRunnerStarted {
            child_id: id.clone(),
        });
        #[cfg(test)]
        if let Some(run_child_test_gate) = &self.run_child_test_gate {
            run_child_test_gate.wait_before_lock().await;
        }
        let cancellation = {
            let mut inner = self.lock_inner(CoordinatorOperation::RunChild).await;
            let task = inner
                .tasks
                .get_mut(&id)
                .expect("admitted child must remain retained");
            if task.state == TaskState::Starting && task.stop.is_none() {
                task.transition(TaskState::Running);
            }
            task.cancellation.clone()
        };
        #[cfg(test)]
        if let Some(run_child_test_gate) = &self.run_child_test_gate {
            run_child_test_gate.wait_after_lock().await;
        }
        let result = if cancellation.is_cancelled() {
            Err(ChildFailure::new("child was cancelled before startup"))
        } else {
            let runner = self.runner.clone();
            let runner_cancellation = cancellation.clone();
            self.trace.emit(|| TraceEvent::ChildRunnerSetupStarted {
                child_id: id.clone(),
            });
            let runner = catch_unwind(AssertUnwindSafe(|| runner.run(child, runner_cancellation)));
            self.trace.emit(|| TraceEvent::ChildRunnerSetupFinished {
                child_id: id.clone(),
            });
            match runner {
                Ok(runner) => AssertUnwindSafe(runner)
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(ChildFailure::new("child runner panicked"))),
                Err(_) => Err(ChildFailure::new("child runner panicked")),
            }
        };
        let outcome = match &result {
            Ok(_) => Settlement::Succeeded,
            Err(_) if cancellation.is_cancelled() => Settlement::Cancelled,
            Err(_) => Settlement::Failed,
        };
        self.trace.emit(|| TraceEvent::ChildRunnerReturned {
            child_id: id.clone(),
            outcome,
        });
        self.finish_child(&id, result).await;
    }

    async fn finish_child(&self, id: &TaskId, result: Result<ChildResult, ChildFailure>) {
        let now = self.now();
        let outcome = {
            let mut inner = self.lock_inner(CoordinatorOperation::FinishChild).await;
            let task = inner
                .tasks
                .get_mut(id)
                .expect("admitted child must remain retained");
            if task.state.is_terminal() {
                return;
            }
            if let Some(stop) = task.stop.clone() {
                stop.outcome()
            } else {
                match result {
                    Ok(result) => {
                        let record = settle_task(
                            &mut inner,
                            id,
                            TaskOutcome::Completed(result),
                            now,
                            self.unix_time_ms(),
                            &self.trace,
                        );
                        drop(inner);
                        if let Some(record) = record {
                            self.publish_event(CoordinatorEvent::Finished { record });
                        }
                        return;
                    }
                    Err(failure) => {
                        task.stop = Some(StopReason::RunnerFailed(failure.clone()));
                        task.transition(TaskState::Stopping);
                        TaskOutcome::Failed(failure)
                    }
                }
            }
        };
        let descendants = self
            .request_stop_descendants(id, StopReason::AncestorStopped)
            .await;
        self.wait_for_all(descendants).await;
        let record = {
            let mut inner = self
                .lock_inner(CoordinatorOperation::FinishChildSettlement)
                .await;
            settle_task(
                &mut inner,
                id,
                outcome,
                self.now(),
                self.unix_time_ms(),
                &self.trace,
            )
        };
        if let Some(record) = record {
            self.publish_event(CoordinatorEvent::Finished { record });
        }
    }

    async fn request_stop_subtree(&self, id: &TaskId, reason: StopReason) -> Vec<TaskId> {
        let mut inner = self
            .lock_inner(CoordinatorOperation::RequestStopSubtree)
            .await;
        let ids = inner
            .tasks
            .keys()
            .filter(|candidate| is_in_subtree(&inner.tasks, candidate, id))
            .cloned()
            .collect::<Vec<_>>();
        stop_tasks(&mut inner, &ids, reason, &self.trace)
    }

    async fn request_stop_descendants(&self, id: &TaskId, reason: StopReason) -> Vec<TaskId> {
        let mut inner = self
            .lock_inner(CoordinatorOperation::RequestStopDescendants)
            .await;
        let ids = inner
            .tasks
            .keys()
            .filter(|candidate| *candidate != id && is_in_subtree(&inner.tasks, candidate, id))
            .cloned()
            .collect::<Vec<_>>();
        stop_tasks(&mut inner, &ids, reason, &self.trace)
    }

    async fn request_timeout(&self, id: &TaskId) -> Vec<TaskId> {
        let mut inner = self.lock_inner(CoordinatorOperation::RequestTimeout).await;
        let Some(task) = inner.tasks.get(id) else {
            return Vec::new();
        };
        if task.state.is_terminal() || task.stop.is_some() {
            return Vec::new();
        }
        let descendants = inner
            .tasks
            .keys()
            .filter(|candidate| *candidate != id && is_in_subtree(&inner.tasks, candidate, id))
            .cloned()
            .collect::<Vec<_>>();
        let mut stopping = stop_tasks(
            &mut inner,
            std::slice::from_ref(id),
            StopReason::TimedOut,
            &self.trace,
        );
        stopping.extend(stop_tasks(
            &mut inner,
            &descendants,
            StopReason::AncestorStopped,
            &self.trace,
        ));
        stopping
    }

    async fn request_stop_all(&self, reason: StopReason, active: Option<&Caller>) {
        let mut inner = self.lock_inner(CoordinatorOperation::RequestStopAll).await;
        let mut origins = inner
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.origin)
            .collect::<BTreeSet<_>>();
        if let Some(caller) = active.filter(|caller| caller.session == inner.session) {
            origins.insert(caller.origin);
        }
        inner.blocked_origins.extend(origins);
        let ids = inner
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        stop_tasks(&mut inner, &ids, reason, &self.trace);
        inner.notify();
    }

    async fn wait_for_all(&self, ids: Vec<TaskId>) {
        for id in ids {
            self.wait_for_terminal(&id).await;
        }
    }

    async fn wait_for_terminal(&self, id: &TaskId) -> TaskOutcome {
        let updates = {
            let inner = self
                .lock_inner(CoordinatorOperation::WaitForTerminalSubscribe)
                .await;
            inner
                .tasks
                .get(id)
                .expect("retained task must exist")
                .metrics
                .subscribe()
        };
        let mut updates = updates;
        loop {
            if let Some(outcome) = self
                .lock_inner(CoordinatorOperation::WaitForTerminalCheck)
                .await
                .tasks
                .get(id)
                .and_then(|task| task.state.is_terminal().then(|| task.outcome.clone()))
                .flatten()
            {
                return outcome;
            }
            updates
                .changed()
                .await
                .expect("retained task update channel remains open");
        }
    }

    async fn wait_for_outcome(
        &self,
        id: &TaskId,
        mut updates: watch::Receiver<u64>,
    ) -> Result<(ChildResult, TaskObservation), WaitError> {
        loop {
            let terminal = {
                let inner = self.lock_inner(CoordinatorOperation::WaitForOutcome).await;
                let task = inner
                    .tasks
                    .get(id)
                    .expect("retained terminal task must exist");
                task.state.is_terminal().then(|| {
                    (
                        task.outcome
                            .clone()
                            .expect("terminal task must have an outcome"),
                        task.observation(self.now(), self.unix_time_ms()),
                    )
                })
            };
            if let Some((outcome, observation)) = terminal {
                return match outcome {
                    TaskOutcome::Completed(result) => Ok((result, observation)),
                    TaskOutcome::Failed(failure) => Err(WaitError::Failed {
                        id: id.clone(),
                        message: failure.message().to_owned(),
                        observation: Box::new(observation),
                    }),
                    TaskOutcome::Cancelled => Err(WaitError::Cancelled {
                        id: id.clone(),
                        observation: Box::new(observation),
                    }),
                    TaskOutcome::TimedOut => Err(WaitError::TimedOut {
                        id: id.clone(),
                        observation: Box::new(observation),
                    }),
                };
            }
            updates
                .changed()
                .await
                .expect("retained task update channel remains open");
        }
    }

    fn now(&self) -> Instant {
        self.clock.now()
    }

    fn unix_time_ms(&self) -> i64 {
        self.clock.unix_time_ms()
    }
}

fn earliest_deadline(own: Option<Instant>, inherited: Option<Instant>) -> Option<Instant> {
    match (own, inherited) {
        (Some(own), Some(inherited)) => Some(own.min(inherited)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

fn ensure_access(inner: &Inner, caller: &Caller, id: &TaskId) -> Result<(), AccessError> {
    if caller.session != inner.session {
        return Err(AccessError::InvalidCaller);
    }
    let Some(target) = inner.tasks.get(id) else {
        return Err(AccessError::UnknownTask(id.clone()));
    };
    match &caller.owner {
        CallerOwner::Root => Ok(()),
        CallerOwner::Child(owner) => {
            let Some(owner_task) = inner.tasks.get(owner) else {
                return Err(AccessError::InvalidCaller);
            };
            if owner_task.origin != caller.origin {
                return Err(AccessError::InvalidCaller);
            }
            if target.origin != caller.origin || !is_proper_descendant(&inner.tasks, id, owner) {
                return Err(AccessError::Denied(id.clone()));
            }
            Ok(())
        }
    }
}

fn is_proper_descendant(
    tasks: &BTreeMap<TaskId, Task>,
    candidate: &TaskId,
    ancestor: &TaskId,
) -> bool {
    let mut current = tasks.get(candidate).and_then(|task| task.parent.clone());
    while let Some(id) = current {
        if &id == ancestor {
            return true;
        }
        current = tasks.get(&id).and_then(|task| task.parent.clone());
    }
    false
}

fn is_in_subtree(tasks: &BTreeMap<TaskId, Task>, candidate: &TaskId, ancestor: &TaskId) -> bool {
    candidate == ancestor || is_proper_descendant(tasks, candidate, ancestor)
}

fn stop_tasks(inner: &mut Inner, ids: &[TaskId], reason: StopReason, trace: &Trace) -> Vec<TaskId> {
    let mut stopping = Vec::new();
    for id in ids {
        let Some(task) = inner.tasks.get_mut(id) else {
            continue;
        };
        if task.state.is_terminal() {
            continue;
        }
        if task.stop.is_none() {
            trace.emit(|| TraceEvent::ChildStopRequested {
                child_id: id.clone(),
                cause: stop_cause(&reason),
            });
            task.stop = Some(reason.clone());
            task.transition(TaskState::Stopping);
            task.cancellation.cancel();
        }
        stopping.push(id.clone());
    }
    stopping
}

fn stop_cause(reason: &StopReason) -> ChildStopCause {
    match reason {
        StopReason::Cancelled => ChildStopCause::Cancelled,
        StopReason::TimedOut => ChildStopCause::TimedOut,
        StopReason::Shutdown => ChildStopCause::Shutdown,
        StopReason::AncestorStopped => ChildStopCause::AncestorStopped,
        StopReason::RunnerFailed(_) => ChildStopCause::RunnerFailed,
    }
}

fn settlement(outcome: &TaskOutcome) -> Settlement {
    match outcome {
        TaskOutcome::Completed(_) => Settlement::Succeeded,
        TaskOutcome::Failed(_) => Settlement::Failed,
        TaskOutcome::Cancelled | TaskOutcome::TimedOut => Settlement::Cancelled,
    }
}

fn settle_task(
    inner: &mut Inner,
    id: &TaskId,
    outcome: TaskOutcome,
    now: Instant,
    observed_at_ms: i64,
    trace: &Trace,
) -> Option<TaskRecord> {
    let outcome_classification = settlement(&outcome);
    let record = {
        let task = inner
            .tasks
            .get_mut(id)
            .expect("admitted child must remain retained");
        task.metrics.settle()?;
        task.state = match outcome {
            TaskOutcome::Completed(_) => TaskState::Completed,
            TaskOutcome::Failed(_) => TaskState::Failed,
            TaskOutcome::Cancelled => TaskState::Cancelled,
            TaskOutcome::TimedOut => TaskState::TimedOut,
        };
        task.outcome = Some(outcome);
        task.settled_at = Some(now);
        task.record(now, observed_at_ms)
    };
    inner.active_slots = inner
        .active_slots
        .checked_sub(1)
        .expect("every settled task owns exactly one active slot");
    trace.emit(|| TraceEvent::ChildSettled {
        child_id: id.clone(),
        outcome: outcome_classification,
    });
    Some(record)
}

fn task_id(sequence: u64) -> TaskId {
    TaskId(format!("{sequence:02x}"))
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
    use std::collections::{BTreeMap, VecDeque};
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use futures::FutureExt;
    use tokio::sync::{Notify, mpsc, oneshot};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        AccessError, AdmissionError, ChildFailure, ChildParameters, ChildResult, ChildRun,
        ChildRunner, Clock, CoordinatorEvent, CostStatus, FAILURE_MESSAGE_LIMIT, RunnerFuture,
        SubagentCoordinator, SubagentPolicy, TaskId, TaskOutcome, TaskState, WaitError,
        WaitPhaseSignal, task_id, task_id_argument,
    };
    use crate::arena::{Arena, ChildConstructionTestGate, CollectionTestGate, ExecutionOutcome};
    use crate::config::{DEFAULT_MODEL, ModelSelection};
    use crate::model::{
        AttemptAccounting, Model, ModelAction, ModelContextEntry, ModelFailure, ModelHistory,
        ModelProgress, ModelStep, ProviderReplay, ScriptedAction, ScriptedModel,
    };
    use crate::telemetry::TraceOwner;
    use crate::tools::{ReadTestGate, ToolHost, ToolValue};

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the monty-runtime 0.0.22 worker")
    }

    #[derive(Clone)]
    struct ScriptedChildFactory {
        scripts: Arc<Mutex<VecDeque<Vec<ScriptedAction>>>>,
        prompts: Arc<Mutex<Vec<String>>>,
        selections: Arc<Mutex<Vec<ModelSelection>>>,
        histories: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
    }

    struct RecordingScriptedModel {
        model: ScriptedModel,
        histories: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
    }

    impl Model for RecordingScriptedModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            self.histories
                .lock()
                .unwrap()
                .push(history.entries().to_vec());
            self.model.step(history, cancellation, progress)
        }
    }

    impl ScriptedChildFactory {
        fn new(scripts: impl IntoIterator<Item = Vec<ScriptedAction>>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
                prompts: Arc::new(Mutex::new(Vec::new())),
                selections: Arc::new(Mutex::new(Vec::new())),
                histories: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl super::ChildModelFactory for ScriptedChildFactory {
        type Model = RecordingScriptedModel;

        fn create(&self, prompt: String, selection: ModelSelection) -> Self::Model {
            self.prompts.lock().unwrap().push(prompt);
            self.selections.lock().unwrap().push(selection);
            RecordingScriptedModel {
                model: ScriptedModel::new(
                    self.scripts
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("test configures one child script"),
                ),
                histories: self.histories.clone(),
            }
        }
    }

    struct Started {
        child: ChildRun<()>,
        finish: oneshot::Sender<Result<ChildResult, ChildFailure>>,
    }

    struct GatedRunner {
        started: mpsc::UnboundedSender<Started>,
    }

    impl ChildRunner<()> for GatedRunner {
        fn run(
            &self,
            child: ChildRun<()>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> RunnerFuture {
            let (finish, result) = oneshot::channel();
            self.started
                .send(Started { child, finish })
                .expect("test receives each started child");
            Box::pin(async move {
                tokio::select! {
                    result = result => result.expect("test settles every started child"),
                    _ = cancellation.cancelled() => Err(ChildFailure::new("runner cancelled")),
                }
            })
        }
    }

    #[derive(Clone)]
    struct ManualClock(Arc<Mutex<(Instant, i64)>>);

    impl ManualClock {
        fn new(observed_at_ms: i64) -> Self {
            Self::starting_at(Instant::now(), observed_at_ms)
        }

        fn starting_at(now: Instant, observed_at_ms: i64) -> Self {
            Self(Arc::new(Mutex::new((now, observed_at_ms))))
        }

        fn advance(&self, duration: Duration) {
            let mut time = self.0.lock().expect("test clock lock");
            time.0 += duration;
            time.1 += i64::try_from(duration.as_millis()).expect("test duration fits i64");
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.0.lock().expect("test clock lock").0
        }

        fn unix_time_ms(&self) -> i64 {
            self.0.lock().expect("test clock lock").1
        }
    }

    fn instant_with_second_headroom(headroom: u64) -> Instant {
        let base = Instant::now();
        let mut lower = 0;
        let mut upper = u64::MAX;
        while lower < upper {
            let distance = upper - lower;
            let midpoint = lower + distance / 2 + distance % 2;
            if base.checked_add(Duration::from_secs(midpoint)).is_some() {
                lower = midpoint;
            } else {
                upper = midpoint - 1;
            }
        }
        assert!(lower < u64::MAX);
        assert!(lower >= headroom);
        let now = base
            .checked_add(Duration::from_secs(lower - headroom))
            .expect("headroom instant must be representable");
        assert!(
            now.checked_add(Duration::from_secs(headroom)).is_some(),
            "requested headroom must be representable"
        );
        assert!(
            now.checked_add(Duration::from_secs(headroom + 1)).is_none(),
            "instant must have exactly the requested whole-second headroom"
        );
        now
    }

    fn policy() -> SubagentPolicy {
        SubagentPolicy {
            allow_nested: true,
            max_concurrent: 8,
            max_starts_per_run: 64,
            max_depth: 4,
            timeout: Duration::from_secs(900),
        }
    }

    fn coordinator(
        policy: SubagentPolicy,
    ) -> (SubagentCoordinator<()>, mpsc::UnboundedReceiver<Started>) {
        let (started_sender, started_receiver) = mpsc::unbounded_channel();
        (
            SubagentCoordinator::new(
                policy,
                Arc::new(GatedRunner {
                    started: started_sender,
                }),
            ),
            started_receiver,
        )
    }

    fn parameters() -> ChildParameters {
        parameters_with(
            ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                .expect("default selection is valid"),
            false,
        )
    }

    fn parameters_with(selection: ModelSelection, read_only: bool) -> ChildParameters {
        ChildParameters::new("inspect the lifecycle".to_owned(), selection, read_only)
    }

    fn result(message: &str) -> ChildResult {
        ChildResult::new(message.to_owned(), BTreeMap::new())
    }

    fn child_execution_events(trace: &str) -> Vec<serde_json::Value> {
        trace
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .filter(|event| {
                matches!(
                    event["event"].as_str(),
                    Some("child_execution_started" | "child_execution_settled")
                )
            })
            .collect()
    }

    fn child_execution_event_summaries(
        events: &[serde_json::Value],
    ) -> Vec<(&str, &str, u64, Option<&str>)> {
        events
            .iter()
            .map(|event| {
                (
                    event["child_id"].as_str().expect("child id"),
                    event["event"].as_str().expect("event name"),
                    event["model_step"].as_u64().expect("model step"),
                    event["outcome"].as_str(),
                )
            })
            .collect()
    }

    fn known_usage() -> crate::model::AttemptAccounting {
        crate::model::AttemptAccounting::KnownUsage(crate::model::ModelUsage {
            model: DEFAULT_MODEL.id.to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(1),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            output_tokens: Some(0),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(1),
        })
    }

    fn assert_failed(error: WaitError, id: &TaskId, message: &str) {
        let observation = error.observation().expect("terminal observation");
        assert_eq!(observation.id(), id);
        assert_eq!(observation.state(), TaskState::Failed);
        assert_eq!(observation.error(), Some(message));
        assert!(matches!(
            error,
            WaitError::Failed {
                id: actual_id,
                message: actual_message,
                ..
            } if actual_id == *id && actual_message == message
        ));
    }

    fn assert_cancelled(error: WaitError, id: &TaskId) {
        let observation = error.observation().expect("terminal observation");
        assert_eq!(observation.id(), id);
        assert_eq!(observation.state(), TaskState::Cancelled);
        assert!(matches!(
            error,
            WaitError::Cancelled { id: actual_id, .. } if actual_id == *id
        ));
    }

    fn assert_timed_out(error: WaitError, id: &TaskId) {
        let observation = error.observation().expect("terminal observation");
        assert_eq!(observation.id(), id);
        assert_eq!(observation.state(), TaskState::TimedOut);
        assert!(matches!(
            error,
            WaitError::TimedOut { id: actual_id, .. } if actual_id == *id
        ));
    }

    async fn start(
        coordinator: &SubagentCoordinator<()>,
        started: &mut mpsc::UnboundedReceiver<Started>,
        caller: &super::Caller,
    ) -> (TaskId, Started) {
        let admitted = coordinator.start(caller, parameters(), ()).await.unwrap();
        assert_eq!(admitted.state(), TaskState::Starting);
        let id = admitted.id().clone();
        let running = started.recv().await.expect("child starts");
        assert_eq!(running.child.id(), &id);
        (id, running)
    }

    async fn seed_next_task(coordinator: &SubagentCoordinator<()>, sequence: u64) {
        coordinator.inner.lock().await.next_task = Some(sequence);
    }

    #[derive(Clone, Copy)]
    enum PanicKind {
        Synchronous,
        Asynchronous,
    }

    struct PanicStarted {
        child: ChildRun<()>,
        trigger: oneshot::Sender<()>,
    }

    struct PanicRunner {
        kind: PanicKind,
        started: mpsc::UnboundedSender<PanicStarted>,
    }

    impl ChildRunner<()> for PanicRunner {
        fn run(
            &self,
            child: ChildRun<()>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> RunnerFuture {
            if child.depth() > 1 {
                return Box::pin(async move {
                    cancellation.cancelled().await;
                    Err(ChildFailure::new("runner cancelled"))
                });
            }
            let (trigger, gate) = oneshot::channel();
            self.started
                .send(PanicStarted { child, trigger })
                .expect("test receives each started child");
            if matches!(self.kind, PanicKind::Synchronous) {
                panic!("synchronous runner panic");
            }
            Box::pin(async move {
                gate.await.expect("test triggers asynchronous panic");
                panic!("asynchronous runner panic");
            })
        }
    }

    struct CleanupStarted {
        release: oneshot::Sender<()>,
    }

    struct CleanupRunner {
        started: mpsc::UnboundedSender<CleanupStarted>,
    }

    impl ChildRunner<()> for CleanupRunner {
        fn run(
            &self,
            _child: ChildRun<()>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> RunnerFuture {
            let (release, cleanup) = oneshot::channel();
            self.started
                .send(CleanupStarted { release })
                .expect("test receives each started child");
            Box::pin(async move {
                cancellation.cancelled().await;
                cleanup.await.expect("test releases runner cleanup");
                Err(ChildFailure::new("runner cancelled"))
            })
        }
    }

    struct PayloadRunner {
        payloads: mpsc::UnboundedSender<u8>,
    }

    impl ChildRunner<std::cell::Cell<u8>> for PayloadRunner {
        fn run(
            &self,
            child: ChildRun<std::cell::Cell<u8>>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> RunnerFuture {
            assert_eq!(child.payload().get(), 7);
            let (id, caller, parent, depth, parameters, payload, reporter, coordinator) =
                child.into_parts();
            assert_eq!(id.as_str().len(), 2);
            assert_eq!(parent, None);
            assert_eq!(depth, 1);
            let (prompt, selection, read_only) = parameters.into_parts();
            assert_eq!(prompt, "inspect the lifecycle");
            assert_eq!(
                selection,
                ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                    .expect("default selection is valid")
            );
            assert!(!read_only);
            drop((caller, reporter, coordinator));
            self.payloads
                .send(payload.get())
                .expect("test receives the moved payload");
            Box::pin(async move {
                cancellation.cancelled().await;
                Err(ChildFailure::new("runner cancelled"))
            })
        }
    }

    #[tokio::test]
    async fn traced_coordinator_locks_are_balanced_after_success_and_early_return() {
        let workspace = tempfile::tempdir().expect("workspace");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_starts_per_run: 1,
            ..policy()
        });
        let coordinator = coordinator.with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let admitted = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let running = started.recv().await.expect("child starts");
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::StartBudget)
        );
        running.finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, admitted.id()).await.unwrap();
        trace_owner.shutdown().expect("trace shutdown");

        let events = fs::read_to_string(&trace_file)
            .expect("trace file")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .collect::<Vec<_>>();
        let mut spans = BTreeMap::<String, [usize; 3]>::new();
        for event in events.iter().filter(|event| {
            matches!(
                event["event"].as_str(),
                Some(
                    "coordinator_lock_waiting"
                        | "coordinator_lock_acquired"
                        | "coordinator_lock_released"
                )
            )
        }) {
            assert_eq!(
                event.as_object().expect("trace event object").len(),
                7,
                "coordinator lock events contain only their fixed operation"
            );
            let phase = match event["event"].as_str() {
                Some("coordinator_lock_waiting") => 0,
                Some("coordinator_lock_acquired") => 1,
                Some("coordinator_lock_released") => 2,
                _ => unreachable!("filtered coordinator lock event"),
            };
            let operation = event["operation"].as_str().expect("fixed operation");
            spans.entry(operation.to_owned()).or_default()[phase] += 1;
        }

        assert_eq!(spans["start"], [2, 2, 2]);
        assert!(
            spans
                .iter()
                .all(|(_, counts)| counts[0] == counts[1] && counts[1] == counts[2])
        );
    }

    #[tokio::test]
    async fn traced_uncontended_deadline_maintenance_omits_lock_spans() {
        let workspace = tempfile::tempdir().expect("workspace");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;

        for _ in 0..3 {
            assert!(coordinator.next_deadline().await.is_some());
            coordinator.service().await;
        }

        running.finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, &id).await.unwrap();
        trace_owner.shutdown().expect("trace shutdown");

        let maintenance_spans = fs::read_to_string(&trace_file)
            .expect("trace file")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .filter(|event| {
                matches!(
                    event["operation"].as_str(),
                    Some("service" | "next_deadline")
                )
            })
            .collect::<Vec<_>>();
        assert!(maintenance_spans.is_empty());
    }

    #[tokio::test]
    async fn traced_contended_deadline_maintenance_reports_the_wait() {
        let workspace = tempfile::tempdir().expect("workspace");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let gate = super::StartSubagentTestGate::new();
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator
            .with_trace(trace_owner.handle())
            .with_start_test_gate(gate.clone());
        let caller = coordinator.root_caller().await;
        let start = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            async move { coordinator.start(&caller, parameters(), ()).await }
        });

        gate.wait_until_first_start_locked().await;
        let mut deadline = Box::pin({
            let coordinator = coordinator.clone();
            async move { coordinator.next_deadline().await }
        });
        assert!(
            timeout(Duration::from_millis(10), &mut deadline)
                .await
                .is_err(),
            "deadline maintenance should wait for the held coordinator lock"
        );
        gate.allow_first_start();
        let admitted = start.await.unwrap().unwrap();
        assert!(deadline.await.is_some());
        let running = started.recv().await.expect("child starts");
        running.finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, admitted.id()).await.unwrap();
        trace_owner.shutdown().expect("trace shutdown");

        let contended = fs::read_to_string(&trace_file)
            .expect("trace file")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .find(|event| {
                event["event"] == "coordinator_lock_contended"
                    && event["operation"] == "next_deadline"
            })
            .expect("contended deadline maintenance should be traced");
        assert!(
            contended["wait_ms"].as_u64().expect("wait milliseconds")
                >= u64::try_from(super::MAINTENANCE_LOCK_TRACE_THRESHOLD.as_millis()).unwrap()
        );
    }

    #[tokio::test]
    async fn admitted_tasks_outlive_the_initiating_caller() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;
        drop(caller);

        let observer = coordinator.root_caller().await;
        assert_eq!(
            coordinator.get(&observer, &id).await.unwrap().state(),
            TaskState::Running
        );
        running.finish.send(Ok(result("done"))).unwrap();

        assert_eq!(
            coordinator.wait(&observer, &id).await.unwrap().0.message(),
            "done"
        );
    }

    #[test]
    fn task_identifiers_use_canonical_hexadecimal_sequence() {
        for (sequence, expected) in [
            (0, "00"),
            (1, "01"),
            (15, "0f"),
            (16, "10"),
            (255, "ff"),
            (256, "100"),
        ] {
            assert_eq!(task_id(sequence).as_str(), expected);
        }
    }

    #[test]
    fn task_identifier_parsing_is_strict_and_keeps_canonical_unknowns() {
        for canonical in ["00", "01", "0f", "10", "ff", "100", "dead"] {
            assert_eq!(TaskId::parse(canonical).unwrap().as_str(), canonical);
        }
        for malformed in ["", "0", "0A", "0g", "000", "0100", "10000000000000000"] {
            assert!(TaskId::parse(malformed).is_none(), "{malformed}");
        }
    }

    #[test]
    fn task_identifiers_sort_numerically_across_widths() {
        let mut identifiers = ["100", "0f", "ff", "10", "00", "01"].map(|id| TaskId(id.to_owned()));
        identifiers.sort();
        assert_eq!(
            identifiers.map(|id| id.to_string()),
            ["00", "01", "0f", "10", "ff", "100"]
        );
    }

    #[test]
    fn malformed_task_identifier_errors_describe_canonical_hexadecimal_ids() {
        assert_eq!(
            task_id_argument(
                ToolValue::List(vec![ToolValue::String("0g".to_owned())]),
                "get_subagent",
            )
            .unwrap_err()
            .to_string(),
            "subagent task ID must be a canonical lowercase hexadecimal string with at least two characters"
        );
    }

    #[tokio::test]
    async fn canonical_unknown_task_identifiers_remain_distinct_from_malformed_ids() {
        assert!(TaskId::parse("0g").is_none());
        let unknown = TaskId::parse("dead").expect("canonical unknown task ID");
        let (coordinator, _) = coordinator(policy());
        let caller = coordinator.root_caller().await;

        assert_eq!(
            coordinator.get(&caller, &unknown).await,
            Err(AccessError::UnknownTask(unknown))
        );
    }

    #[tokio::test]
    async fn settled_task_identifiers_are_never_reused() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (first_id, first) = start(&coordinator, &mut started, &caller).await;
        first.finish.send(Ok(result("first"))).unwrap();
        coordinator.wait(&caller, &first_id).await.unwrap();

        let (second_id, second) = start(&coordinator, &mut started, &caller).await;
        second.finish.send(Ok(result("second"))).unwrap();
        coordinator.wait(&caller, &second_id).await.unwrap();

        assert_eq!(first_id.as_str(), "00");
        assert_eq!(second_id.as_str(), "01");
    }

    #[tokio::test]
    async fn final_task_identifier_is_allocated_once_before_exhaustion() {
        let (coordinator, mut started) = coordinator(policy());
        seed_next_task(&coordinator, u64::MAX).await;
        let caller = coordinator.root_caller().await;

        let (id, final_task) = start(&coordinator, &mut started, &caller).await;
        assert_eq!(id.as_str(), "ffffffffffffffff");
        final_task.finish.send(Ok(result("final"))).unwrap();
        coordinator.wait(&caller, &id).await.unwrap();

        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::IdentifierSpaceExhausted)
        );
        assert_eq!(
            coordinator
                .records()
                .await
                .iter()
                .map(|record| record.id().as_str())
                .collect::<Vec<_>>(),
            ["ffffffffffffffff"]
        );
        assert!(started.try_recv().is_err());
    }

    #[tokio::test]
    async fn concurrent_admission_is_atomic_and_releases_slots_after_settlement() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_concurrent: 1,
            ..policy()
        });
        let first_caller = coordinator.root_caller().await;
        let second_caller = coordinator.root_caller().await;
        let (first, second) = tokio::join!(
            coordinator.start(&first_caller, parameters(), ()),
            coordinator.start(&second_caller, parameters(), ())
        );
        let admitted = match (first, second) {
            (Ok(admitted), Err(AdmissionError::ConcurrentLimit))
            | (Err(AdmissionError::ConcurrentLimit), Ok(admitted)) => admitted,
            other => panic!("unexpected concurrent admission result: {other:?}"),
        };
        let running = started.recv().await.expect("one task starts");
        running.finish.send(Ok(result("done"))).unwrap();
        coordinator
            .wait(&first_caller, admitted.id())
            .await
            .unwrap();
        assert_eq!(coordinator.active_count().await, 0);

        let (_, running) = start(&coordinator, &mut started, &second_caller).await;
        running.finish.send(Ok(result("next"))).unwrap();
        coordinator.cancel_all().await;
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn rejected_admission_creates_no_record_and_runner_failure_retains_one() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_starts_per_run: 1,
            ..policy()
        });
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::StartBudget)
        );
        assert_eq!(coordinator.records().await.len(), 1);
        running
            .finish
            .send(Err(ChildFailure::new("launch failed")))
            .unwrap();

        assert_failed(
            coordinator.wait(&caller, &id).await.unwrap_err(),
            &id,
            "launch failed",
        );
        assert_eq!(coordinator.active_count().await, 0);
        assert!(matches!(
            coordinator.records().await[0].outcome(),
            Some(TaskOutcome::Failed(_))
        ));
    }

    #[tokio::test]
    async fn completed_results_are_repeatable_and_independent_copies() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;
        running
            .finish
            .send(Ok(ChildResult::new(
                "done".to_owned(),
                BTreeMap::from([("value".to_owned(), ToolValue::Integer(1))]),
            )))
            .unwrap();

        let (mut first, _) = coordinator.wait(&caller, &id).await.unwrap();
        first.values.insert(
            "changed".to_owned(),
            ToolValue::String("locally".to_owned()),
        );
        let second = coordinator.wait(&caller, &id).await.unwrap();

        assert_eq!(second.0.values().get("value"), Some(&ToolValue::Integer(1)));
        assert!(!second.0.values().contains_key("changed"));
    }

    #[tokio::test]
    async fn child_callers_can_manage_only_proper_descendants() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let (left_id, left) = start(&coordinator, &mut started, &root).await;
        let (right_id, right) = start(&coordinator, &mut started, &root).await;
        let (child_id, child) = start(&coordinator, &mut started, left.child.caller()).await;

        assert_eq!(
            coordinator.get(left.child.caller(), &left_id).await,
            Err(AccessError::Denied(left_id.clone()))
        );
        assert_eq!(
            coordinator.get(left.child.caller(), &right_id).await,
            Err(AccessError::Denied(right_id.clone()))
        );
        assert_eq!(
            coordinator.get(child.child.caller(), &left_id).await,
            Err(AccessError::Denied(left_id.clone()))
        );
        let guessed = TaskId::parse("dead").expect("canonical unknown task ID");
        assert_eq!(
            coordinator.get(left.child.caller(), &guessed).await,
            Err(AccessError::UnknownTask(guessed))
        );
        assert_eq!(
            coordinator
                .get(left.child.caller(), &child_id)
                .await
                .unwrap()
                .state(),
            TaskState::Running
        );

        left.finish.send(Ok(result("left"))).unwrap();
        right.finish.send(Ok(result("right"))).unwrap();
        child.finish.send(Ok(result("child"))).unwrap();
        coordinator.cancel_all().await;
    }

    #[tokio::test]
    async fn callers_from_other_sessions_are_not_trusted() {
        let (session, _) = coordinator(policy());
        let (other, _) = coordinator(policy());
        let other_caller = other.root_caller().await;
        let id = TaskId::parse("00").expect("canonical task ID");

        assert_eq!(
            session.start(&other_caller, parameters(), ()).await,
            Err(AdmissionError::InvalidCaller)
        );
        assert_eq!(
            session.get(&other_caller, &id).await,
            Err(AccessError::InvalidCaller)
        );
    }

    #[tokio::test]
    async fn origins_depth_and_read_only_restrictions_are_frozen_at_admission() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_starts_per_run: 1,
            max_depth: 1,
            ..policy()
        });
        let first_origin = coordinator.root_caller().await;
        let second_origin = coordinator.root_caller().await;
        let (first_id, first) = start(&coordinator, &mut started, &first_origin).await;
        assert_eq!(
            coordinator.start(&first_origin, parameters(), ()).await,
            Err(AdmissionError::StartBudget)
        );
        assert_eq!(
            coordinator
                .start(first.child.caller(), parameters(), ())
                .await,
            Err(AdmissionError::DepthLimit)
        );
        first.finish.send(Ok(result("first"))).unwrap();
        coordinator.wait(&first_origin, &first_id).await.unwrap();

        let (_, second) = start(&coordinator, &mut started, &second_origin).await;
        assert_eq!(
            coordinator
                .start(second.child.caller(), parameters(), ())
                .await,
            Err(AdmissionError::DepthLimit)
        );
        second.finish.send(Ok(result("second"))).unwrap();
        coordinator.cancel_all().await;

        let records = coordinator.records().await;
        assert_ne!(records[0].origin(), records[1].origin());
        assert!(!records[0].parameters().read_only());
    }

    #[tokio::test]
    async fn descendants_consume_their_originating_request_budget() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_starts_per_run: 2,
            ..policy()
        });
        let first_origin = coordinator.root_caller().await;
        let second_origin = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &first_origin).await;
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;

        assert_eq!(
            coordinator
                .start(child.child.caller(), parameters(), ())
                .await,
            Err(AdmissionError::StartBudget)
        );
        parent.finish.send(Ok(result("parent"))).unwrap();
        child.finish.send(Ok(result("child"))).unwrap();
        coordinator.wait(&first_origin, &parent_id).await.unwrap();
        coordinator.wait(&first_origin, &child_id).await.unwrap();

        let (_, second) = start(&coordinator, &mut started, &second_origin).await;
        second.finish.send(Ok(result("second"))).unwrap();
        coordinator.cancel_all().await;
    }

    #[tokio::test]
    async fn child_parameters_keep_the_admitted_selection_and_only_narrow_permissions() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let selection = ModelSelection::new(&crate::config::models::TERRA, "high")
            .expect("terra selection is valid");
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;

        assert_eq!(
            parent.child.parameters().selection(),
            ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                .expect("default selection is valid")
        );
        assert!(!parent.child.parameters().read_only());
        assert!(!child.child.parameters().read_only());
        assert_eq!(
            coordinator
                .start(&root, parameters_with(selection, true), ())
                .await
                .unwrap()
                .selection(),
            selection
        );
        let restricted = started.recv().await.expect("restricted child starts");
        let (_, inherited) = start(&coordinator, &mut started, restricted.child.caller()).await;
        assert!(inherited.child.parameters().read_only());

        parent.finish.send(Ok(result("parent"))).unwrap();
        child.finish.send(Ok(result("child"))).unwrap();
        restricted.finish.send(Ok(result("restricted"))).unwrap();
        inherited.finish.send(Ok(result("inherited"))).unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();
        coordinator.wait(&root, &child_id).await.unwrap();
        coordinator.cancel_all().await;
    }

    #[tokio::test]
    async fn nested_admission_can_be_disabled_without_affecting_root_admission() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            allow_nested: false,
            ..policy()
        });
        let root = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &root).await;

        assert_eq!(
            coordinator
                .start(running.child.caller(), parameters(), ())
                .await,
            Err(AdmissionError::NestedDisabled)
        );
        running.finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&root, &id).await.unwrap();
    }

    #[tokio::test]
    async fn completed_ancestors_preserve_live_descendants_until_explicit_cancellation() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;
        parent.finish.send(Ok(result("parent"))).unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();

        assert_eq!(
            coordinator.get(&root, &child_id).await.unwrap().state(),
            TaskState::Running
        );
        assert_eq!(
            coordinator.cancel(&root, &parent_id).await.unwrap().state(),
            TaskState::Completed
        );
        assert_cancelled(
            coordinator.wait(&root, &child_id).await.unwrap_err(),
            &child_id,
        );
        drop(child);
    }

    #[tokio::test]
    async fn failure_cleans_up_descendants_before_releasing_the_parent_slot() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_concurrent: 2,
            ..policy()
        });
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;
        parent
            .finish
            .send(Err(ChildFailure::new("parent failed")))
            .unwrap();

        assert_failed(
            coordinator.wait(&root, &parent_id).await.unwrap_err(),
            &parent_id,
            "parent failed",
        );
        assert_cancelled(
            coordinator.wait(&root, &child_id).await.unwrap_err(),
            &child_id,
        );
        assert_eq!(coordinator.active_count().await, 0);
        drop(child);
    }

    #[tokio::test]
    async fn ancestor_timeout_cancels_unfinished_descendants() {
        let clock = Arc::new(ManualClock::new(1_000));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(10),
                ..policy()
            },
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;
        clock.advance(Duration::from_secs(10));
        coordinator.service().await;

        assert_timed_out(
            coordinator.wait(&root, &parent_id).await.unwrap_err(),
            &parent_id,
        );
        assert_cancelled(
            coordinator.wait(&root, &child_id).await.unwrap_err(),
            &child_id,
        );
        assert_eq!(coordinator.active_count().await, 0);
        drop((parent, child));
    }

    #[tokio::test]
    async fn descendant_deadlines_survive_normal_ancestor_completion() {
        let clock = Arc::new(ManualClock::new(1_000));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(10),
                ..policy()
            },
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;
        parent.finish.send(Ok(result("parent"))).unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();
        clock.advance(Duration::from_secs(11));
        coordinator.service().await;

        assert_timed_out(
            coordinator.wait(&root, &child_id).await.unwrap_err(),
            &child_id,
        );
        assert_eq!(coordinator.active_count().await, 0);
        drop(child);
    }

    #[tokio::test]
    async fn cancel_all_fences_racing_nested_admission() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let nested = coordinator.start(parent.child.caller(), parameters(), ());
        let cancellation = coordinator.cancel_all();
        let (nested, ()) = tokio::join!(nested, cancellation);

        if let Ok(observation) = nested {
            assert_eq!(
                coordinator
                    .get(&root, observation.id())
                    .await
                    .unwrap()
                    .state(),
                TaskState::Cancelled
            );
        }
        assert_cancelled(
            coordinator.wait(&root, &parent_id).await.unwrap_err(),
            &parent_id,
        );
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn background_cancellation_keeps_a_stopping_child_stopping_before_runner_start() {
        let gate = super::RunChildTestGate::new();
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_run_child_test_gate(gate.clone());
        let caller = coordinator.root_caller().await;
        let admitted = coordinator.start(&caller, parameters(), ()).await.unwrap();

        gate.wait_until_before_lock().await;
        coordinator.request_background_cancellation(None).await;
        let stopping = coordinator.get(&caller, admitted.id()).await.unwrap();
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("cancellation requested"));

        gate.allow_lock();
        gate.wait_until_after_lock().await;
        let after_runner_start = coordinator.get(&caller, admitted.id()).await.unwrap();
        assert_eq!(after_runner_start.state(), TaskState::Stopping);
        assert_eq!(after_runner_start.error(), Some("cancellation requested"));

        gate.allow_finish();
        assert_cancelled(
            coordinator.wait(&caller, admitted.id()).await.unwrap_err(),
            admitted.id(),
        );
        assert_eq!(coordinator.active_count().await, 0);
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn cancellation_before_result_admission_wins_and_completed_results_remain_immutable() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let (cancelled_id, cancelled) = start(&coordinator, &mut started, &root).await;
        coordinator.cancel(&root, &cancelled_id).await.unwrap();
        assert_cancelled(
            coordinator.wait(&root, &cancelled_id).await.unwrap_err(),
            &cancelled_id,
        );
        drop(cancelled);

        let (completed_id, completed) = start(&coordinator, &mut started, &root).await;
        completed.finish.send(Ok(result("complete"))).unwrap();
        coordinator.wait(&root, &completed_id).await.unwrap();
        assert_eq!(
            coordinator
                .cancel(&root, &completed_id)
                .await
                .unwrap()
                .state(),
            TaskState::Completed
        );
        assert_eq!(
            coordinator
                .wait(&root, &completed_id)
                .await
                .unwrap()
                .0
                .message(),
            "complete"
        );
    }

    #[tokio::test]
    async fn shutdown_drains_all_work_and_blocks_later_admission() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let (first_id, first) = start(&coordinator, &mut started, &root).await;
        let (second_id, second) = start(&coordinator, &mut started, &root).await;

        coordinator.shutdown().await;
        coordinator.shutdown().await;

        assert_cancelled(
            coordinator.wait(&root, &first_id).await.unwrap_err(),
            &first_id,
        );
        assert_cancelled(
            coordinator.wait(&root, &second_id).await.unwrap_err(),
            &second_id,
        );
        assert_eq!(coordinator.active_count().await, 0);
        assert_eq!(
            coordinator.start(&root, parameters(), ()).await,
            Err(AdmissionError::ShuttingDown)
        );
        drop((first, second));
    }

    #[tokio::test]
    async fn wait_phase_signals_after_subscribing_and_requesting_stop() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(7, sender);
        let mut wait = Box::pin(coordinator.wait_with_phase(&caller, &id, Some(&signal)));

        tokio::select! {
            phase = receiver.recv() => assert_eq!(phase, Some(7)),
            result = &mut wait => panic!("wait finished before its phase signal: {result:?}"),
        }
        running.finish.send(Ok(result("done"))).unwrap();
        assert_eq!(wait.await.unwrap().0.message(), "done");

        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(CleanupRunner {
                started: started_sender,
            }),
        );
        let caller = coordinator.root_caller().await;
        let task = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let cleanup = started.recv().await.expect("child starts");
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(8, sender);
        let mut cancel = Box::pin(coordinator.cancel_with_phase(&caller, task.id(), Some(&signal)));

        tokio::select! {
            phase = receiver.recv() => assert_eq!(phase, Some(8)),
            result = &mut cancel => panic!("cancellation finished before its phase signal: {result:?}"),
        }
        assert_eq!(
            coordinator.get(&caller, task.id()).await.unwrap().state(),
            TaskState::Stopping
        );
        cleanup.release.send(()).unwrap();
        assert_eq!(cancel.await.unwrap().state(), TaskState::Cancelled);
    }

    #[tokio::test]
    async fn terminal_cancellation_does_not_signal_a_wait_phase() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;
        running.finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, &id).await.unwrap();
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(7, sender);

        let observation = coordinator
            .cancel_with_phase(&caller, &id, Some(&signal))
            .await
            .unwrap();

        assert_eq!(observation.state(), TaskState::Completed);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn concurrent_waiters_each_receive_independent_results_and_observations() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &caller).await;
        let first = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = id.clone();
            async move { coordinator.wait(&caller, &id).await }
        });
        let second = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = id.clone();
            async move { coordinator.wait(&caller, &id).await }
        });
        let third = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = id.clone();
            async move { coordinator.wait(&caller, &id).await }
        });

        running.finish.send(Ok(result("done"))).unwrap();
        for waiter in [first, second, third] {
            let (result, observation) = waiter
                .await
                .expect("waiter task completes")
                .expect("child completes");
            assert_eq!(result.message(), "done");
            assert_eq!(observation.id(), &id);
            assert_eq!(observation.state(), TaskState::Completed);
        }
    }

    #[tokio::test]
    async fn panicking_runners_settle_and_clean_up_descendants() {
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let synchronous = SubagentCoordinator::new(
            policy(),
            Arc::new(PanicRunner {
                kind: PanicKind::Synchronous,
                started: started_sender,
            }),
        );
        let root = synchronous.root_caller().await;
        let observation = synchronous.start(&root, parameters(), ()).await.unwrap();
        let synchronous_start = started.recv().await.expect("runner started");
        drop(synchronous_start.trigger);
        assert_failed(
            tokio::time::timeout(
                Duration::from_secs(1),
                synchronous.wait(&root, observation.id()),
            )
            .await
            .expect("synchronous panic cannot hang wait")
            .unwrap_err(),
            observation.id(),
            "child runner panicked",
        );
        tokio::time::timeout(Duration::from_secs(1), synchronous.cancel_all())
            .await
            .expect("synchronous panic cannot hang cancel all");
        tokio::time::timeout(Duration::from_secs(1), synchronous.shutdown())
            .await
            .expect("synchronous panic cannot hang shutdown");
        assert_eq!(synchronous.active_count().await, 0);

        let (started_sender, mut started) = mpsc::unbounded_channel();
        let asynchronous = SubagentCoordinator::new(
            SubagentPolicy {
                max_concurrent: 2,
                ..policy()
            },
            Arc::new(PanicRunner {
                kind: PanicKind::Asynchronous,
                started: started_sender,
            }),
        );
        let root = asynchronous.root_caller().await;
        let parent = asynchronous.start(&root, parameters(), ()).await.unwrap();
        let parent_run = started.recv().await.expect("parent runner started");
        let child = asynchronous
            .start(parent_run.child.caller(), parameters(), ())
            .await
            .expect("child admits before parent panic");
        parent_run.trigger.send(()).unwrap();

        assert_failed(
            tokio::time::timeout(
                Duration::from_secs(1),
                asynchronous.wait(&root, parent.id()),
            )
            .await
            .expect("asynchronous panic cannot hang wait")
            .unwrap_err(),
            parent.id(),
            "child runner panicked",
        );
        assert_cancelled(
            tokio::time::timeout(Duration::from_secs(1), asynchronous.wait(&root, child.id()))
                .await
                .expect("descendant cleanup cannot hang")
                .unwrap_err(),
            child.id(),
        );
        tokio::time::timeout(Duration::from_secs(1), asynchronous.cancel_all())
            .await
            .expect("asynchronous panic cannot hang cancel all");
        tokio::time::timeout(Duration::from_secs(1), asynchronous.shutdown())
            .await
            .expect("asynchronous panic cannot hang shutdown");
        assert_eq!(asynchronous.active_count().await, 0);
    }

    #[tokio::test]
    async fn finite_inherited_deadline_clamps_unrepresentable_nested_timeout() {
        let started_at = instant_with_second_headroom(2);
        let inherited_deadline = started_at
            .checked_add(Duration::from_secs(2))
            .expect("parent deadline must be representable");
        let clock = Arc::new(ManualClock::starting_at(started_at, 0));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(2),
                ..policy()
            },
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        clock.advance(Duration::from_secs(1));

        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;

        parent.finish.send(Ok(result("parent"))).unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();
        assert_eq!(coordinator.next_deadline().await, Some(inherited_deadline));
        clock.advance(Duration::from_secs(1));
        coordinator.service().await;
        assert_timed_out(
            coordinator.wait(&root, &child_id).await.unwrap_err(),
            &child_id,
        );
        drop(child);
    }

    #[tokio::test]
    async fn unrepresentable_policy_timeout_rejects_without_consuming_admission_state() {
        let clock = Arc::new(ManualClock::starting_at(instant_with_second_headroom(1), 0));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(2),
                ..policy()
            },
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock,
        );
        let root = coordinator.root_caller().await;

        let error = coordinator
            .start(&root, parameters(), ())
            .await
            .expect_err("unrepresentable timeout must reject admission");

        assert_eq!(
            error.to_string(),
            "subagent timeout cannot be represented as a deadline"
        );
        assert!(coordinator.records().await.is_empty());
        assert_eq!(coordinator.active_count().await, 0);
        let inner = coordinator.inner.lock().await;
        assert_eq!(inner.next_task, Some(0));
        assert!(!inner.origin_starts.contains_key(&root.origin));
        drop(inner);
        assert!(matches!(
            started.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn child_owned_parts_support_non_sync_payloads() {
        let (payload_sender, mut payloads) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(PayloadRunner {
                payloads: payload_sender,
            }),
        );
        let root = coordinator.root_caller().await;
        let observation = coordinator
            .start(&root, parameters(), std::cell::Cell::new(7))
            .await
            .unwrap();

        assert_eq!(payloads.recv().await, Some(7));
        coordinator.cancel(&root, observation.id()).await.unwrap();
        assert_eq!(coordinator.active_count().await, 0);

        let (message, values) = ChildResult::new(
            "done".to_owned(),
            BTreeMap::from([("value".to_owned(), ToolValue::Integer(1))]),
        )
        .into_parts();
        assert_eq!(message, "done");
        assert_eq!(
            values,
            BTreeMap::from([("value".to_owned(), ToolValue::Integer(1))])
        );
    }

    #[tokio::test]
    async fn complete_runner_uses_a_child_checkpoint_and_validated_exports() {
        let workspace = tempfile::tempdir().unwrap();
        let mut parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create parent arena");
        parent
            .execute(
                "inherited = 41\nresult_values = {'parent': 'kept'}",
                &CancellationToken::new(),
            )
            .await
            .expect("initialise parent state");
        let payload = super::ChildPayload::new(parent.child_source(), parent.checkpoint());
        let factory = ScriptedChildFactory::new([vec![
            ScriptedAction::Code {
                source: "result_values['child'] = inherited + 1".to_owned(),
            },
            ScriptedAction::Message {
                text: "child complete".to_owned(),
            },
        ]]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "canonical base".to_owned(),
                factory.clone(),
            )),
        );
        let caller = coordinator.root_caller().await;
        let selection = ModelSelection::new(&crate::config::models::TERRA, "high").unwrap();
        let admitted = coordinator
            .start(
                &caller,
                ChildParameters::new("do the child work".to_owned(), selection, false),
                payload,
            )
            .await
            .expect("admit child");
        let (result, observation) = coordinator.wait(&caller, admitted.id()).await.unwrap();

        assert_eq!(result.message(), "child complete");
        assert_eq!(
            result.values(),
            &BTreeMap::from([("child".to_owned(), ToolValue::Integer(42))])
        );
        assert_eq!(observation.state(), TaskState::Completed);
        assert_eq!(observation.step_count(), 2);
        let records = coordinator.records().await;
        assert_eq!(records[0].attempts(), &[]);
        let execution = &records[0].executions()[0];
        assert_eq!(execution.model_step(), 1);
        assert_eq!(execution.source(), "result_values['child'] = inherited + 1");
        assert_eq!(execution.reasoning_summary(), "");
        assert!(matches!(
            &execution.execution().outcome,
            ExecutionOutcome::Complete { .. }
        ));
        assert!(execution.attempts().is_empty());
        assert_eq!(*factory.selections.lock().unwrap(), [selection]);
        {
            let histories = factory.histories.lock().unwrap();
            assert_eq!(histories.len(), 2);
            assert_eq!(
                histories[0],
                [ModelContextEntry::UserRequest {
                    text: "do the child work".to_owned(),
                }]
            );
            assert!(matches!(
                histories[1].as_slice(),
                [
                    ModelContextEntry::UserRequest { .. },
                    ModelContextEntry::CompletedCode(_)
                ]
            ));
        }
        let prompt = factory.prompts.lock().unwrap().pop().unwrap();
        assert!(prompt.starts_with("canonical base\n\n# Subagent work"));
        assert_eq!(prompt.matches("## Kiki tool reference").count(), 1);
        assert!(matches!(
            parent
                .execute("result_values", &CancellationToken::new())
                .await
                .unwrap()
                .outcome,
            ExecutionOutcome::Complete { result: Some(ref value) } if value.head == "{'parent': 'kept'}"
        ));
    }

    #[tokio::test]
    async fn complete_runner_fails_exports_without_losing_final_message_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create parent arena");
        let payload = super::ChildPayload::new(parent.child_source(), parent.checkpoint());
        let factory = ScriptedChildFactory::new([vec![
            ScriptedAction::Code {
                source: "result_values = 1".to_owned(),
            },
            ScriptedAction::Message {
                text: "generated but not admitted".to_owned(),
            },
        ]]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "canonical base".to_owned(),
                factory,
            )),
        );
        let caller = coordinator.root_caller().await;
        let admitted = coordinator
            .start(&caller, parameters(), payload)
            .await
            .expect("admit child");

        let error = coordinator.wait(&caller, admitted.id()).await.unwrap_err();
        assert_failed(
            error,
            admitted.id(),
            "result_values export failed: result_values must be a dictionary; final message: generated but not admitted",
        );
        let records = coordinator.records().await;
        assert_eq!(records[0].attempts(), &[]);
    }

    #[derive(Clone)]
    struct TracedChildFactory {
        model_step_started: Arc<Notify>,
    }

    struct TracedChildModel {
        model_step_started: Arc<Notify>,
    }

    impl super::ChildModelFactory for TracedChildFactory {
        type Model = TracedChildModel;

        fn create(&self, _prompt: String, _selection: ModelSelection) -> Self::Model {
            TracedChildModel {
                model_step_started: self.model_step_started.clone(),
            }
        }
    }

    impl Model for TracedChildModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                let accounting = AttemptAccounting::UsageUnavailable;
                progress(ModelProgress::AttemptStarted { attempt: 1 });
                self.model_step_started.notify_one();
                progress(ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: accounting.clone(),
                });
                Ok(ModelStep {
                    action: ModelAction::Message {
                        text: "final-message-SECRET".to_owned(),
                    },
                    selection: None,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: vec![accounting],
                })
            }
        }
    }

    #[tokio::test]
    async fn traced_child_construction_precedes_its_model_step_without_sensitive_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let construction_gate = ChildConstructionTestGate::new();
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create parent arena")
        .with_child_construction_test_gate(construction_gate.clone());
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let model_step_started = Arc::new(Notify::new());
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                TracedChildFactory {
                    model_step_started: model_step_started.clone(),
                },
            )),
        )
        .with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(
                &caller,
                ChildParameters::new(
                    "child-request-SECRET".to_owned(),
                    ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                        .expect("default selection"),
                    false,
                ),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        construction_gate.wait_until_construction_started().await;
        assert!(
            timeout(Duration::from_millis(50), model_step_started.notified())
                .await
                .is_err(),
            "child model step started before construction was released"
        );
        construction_gate.resume_construction();
        coordinator
            .wait(&caller, child.id())
            .await
            .expect("child completion");
        trace_owner.shutdown().expect("trace shutdown");

        let trace = fs::read_to_string(&trace_file).expect("trace file");
        let events = trace
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .collect::<Vec<_>>();
        let child_events = events
            .iter()
            .filter(|event| {
                !event["event"]
                    .as_str()
                    .expect("event name")
                    .starts_with("coordinator_lock_")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            child_events
                .iter()
                .map(|event| event["event"].as_str().expect("event name"))
                .collect::<Vec<_>>(),
            [
                "process_started",
                "child_admitted",
                "child_runner_started",
                "child_runner_setup_started",
                "child_runner_setup_finished",
                "child_arena_construction_started",
                "child_arena_pool_opened",
                "child_arena_checked_out",
                "child_arena_checkpoint_restored",
                "child_arena_result_values_reset",
                "child_arena_dumped",
                "child_arena_settled",
                "child_model_step_started",
                "child_provider_attempt_started",
                "child_provider_attempt_settled",
                "child_runner_returned",
                "child_settled",
                "process_shutdown",
            ]
        );
        assert_eq!(child_events[1]["child_id"], "00");
        assert_eq!(child_events[1]["parent_id"], serde_json::Value::Null);
        assert_eq!(child_events[1]["model"], DEFAULT_MODEL.id);
        assert_eq!(child_events[1]["thinking"], DEFAULT_MODEL.default_thinking);
        assert!(events.iter().all(|event| {
            event["schema_version"] == 1
                && event["process"].is_object()
                && event["sequence"].is_u64()
                && event["unix_ms"].is_i64()
                && event["elapsed_ms"].is_u64()
        }));
        for sensitive in ["child-request-SECRET", "final-message-SECRET"] {
            assert!(!trace.contains(sensitive), "trace leaked {sensitive}");
        }
        for event in &events {
            for prohibited in [
                "prompt", "request", "source", "stdout", "stderr", "result", "message", "payload",
                "error",
            ] {
                assert!(
                    event.get(prohibited).is_none(),
                    "trace contained prohibited {prohibited} field"
                );
            }
        }
    }

    #[tokio::test]
    async fn traced_child_execution_brackets_completed_execution_without_content() {
        let workspace = tempfile::tempdir().expect("workspace");
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create parent arena");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let factory = ScriptedChildFactory::new([vec![
            ScriptedAction::Code {
                source: "result_values = {'key-SECRET': 'value-SECRET'}".to_owned(),
            },
            ScriptedAction::Message {
                text: "final-message-SECRET".to_owned(),
            },
        ]]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory,
            )),
        )
        .with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(
                &caller,
                ChildParameters::new(
                    "child-request-SECRET".to_owned(),
                    ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
                        .expect("default selection"),
                    false,
                ),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");
        coordinator
            .wait(&caller, child.id())
            .await
            .expect("child completion");
        trace_owner.shutdown().expect("trace shutdown");

        let trace = fs::read_to_string(&trace_file).expect("trace file");
        let execution_events = child_execution_events(&trace);
        assert_eq!(
            child_execution_event_summaries(&execution_events),
            [
                ("00", "child_execution_started", 1, None),
                ("00", "child_execution_settled", 1, Some("succeeded")),
            ]
        );
        assert!(execution_events.iter().all(|event| {
            event.get("source").is_none()
                && event.get("stdout").is_none()
                && event.get("stderr").is_none()
                && event.get("result").is_none()
        }));
        for sensitive in [
            "child-request-SECRET",
            "key-SECRET",
            "value-SECRET",
            "final-message-SECRET",
        ] {
            assert!(!trace.contains(sensitive), "trace leaked {sensitive}");
        }
    }

    #[tokio::test]
    async fn traced_child_execution_settles_cancelled_and_failed_executions() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("cancelled-SECRET"), "contents").expect("write fixture");
        let read_gate = ReadTestGate::new();
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(read_gate.clone()),
        )
        .await
        .expect("create parent arena");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let factory = ScriptedChildFactory::new([
            vec![ScriptedAction::Code {
                source: "contents = await read('cancelled-SECRET')".to_owned(),
            }],
            vec![ScriptedAction::Code {
                source: "x = 'failure-SECRET' * (1 << 60)".to_owned(),
            }],
        ]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory,
            )),
        )
        .with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let cancelled = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit cancelled child");
        read_gate.wait_until_read_started().await;
        assert_eq!(
            coordinator
                .cancel(&caller, cancelled.id())
                .await
                .expect("cancel child")
                .state(),
            TaskState::Cancelled
        );

        let failed = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit failed child");
        let failure = coordinator
            .wait(&caller, failed.id())
            .await
            .expect_err("arena failure settles child");
        assert_eq!(
            failure.observation().expect("failed observation").state(),
            TaskState::Failed
        );
        trace_owner.shutdown().expect("trace shutdown");

        let trace = fs::read_to_string(&trace_file).expect("trace file");
        assert_eq!(
            child_execution_event_summaries(&child_execution_events(&trace)),
            [
                ("00", "child_execution_started", 1, None),
                ("00", "child_execution_settled", 1, Some("cancelled")),
                ("01", "child_execution_started", 1, None),
                ("01", "child_execution_settled", 1, Some("failed")),
            ]
        );
        for sensitive in ["cancelled-SECRET", "failure-SECRET"] {
            assert!(!trace.contains(sensitive), "trace leaked {sensitive}");
        }
    }

    #[tokio::test]
    async fn traced_child_execution_settles_timed_out_execution_as_cancelled() {
        let workspace = tempfile::tempdir().expect("workspace");
        fs::write(workspace.path().join("timed-out-SECRET"), "contents").expect("write fixture");
        let read_gate = ReadTestGate::new();
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host")
                .with_test_gate(read_gate.clone()),
        )
        .await
        .expect("create parent arena");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let clock = Arc::new(ManualClock::new(0));
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(1),
                ..policy()
            },
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                ScriptedChildFactory::new([vec![ScriptedAction::Code {
                    source: "contents = await read('timed-out-SECRET')".to_owned(),
                }]]),
            )),
            clock.clone(),
        )
        .with_trace(trace_owner.handle());
        let caller = coordinator.root_caller().await;
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");
        read_gate.wait_until_read_started().await;
        clock.advance(Duration::from_secs(1));
        coordinator.service().await;
        assert_timed_out(
            coordinator
                .wait(&caller, child.id())
                .await
                .expect_err("timed-out child fails to wait"),
            child.id(),
        );
        trace_owner.shutdown().expect("trace shutdown");

        let trace = fs::read_to_string(&trace_file).expect("trace file");
        assert_eq!(
            child_execution_event_summaries(&child_execution_events(&trace)),
            [
                ("00", "child_execution_started", 1, None),
                ("00", "child_execution_settled", 1, Some("cancelled")),
            ]
        );
        assert!(!trace.contains("timed-out-SECRET"));
    }

    #[tokio::test]
    async fn traced_child_execution_settles_runner_future_unwind() {
        let workspace = tempfile::tempdir().expect("workspace");
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let trace = trace_owner.handle();
        let child_id = TaskId::parse("00").expect("valid child id");
        let cancellation = CancellationToken::new();

        let unwind = std::panic::AssertUnwindSafe(async {
            let mut execution = super::ChildExecutionSpan::new(&trace, &child_id, &cancellation);
            execution.started(1);
            panic!("runner future unwound");
        })
        .catch_unwind()
        .await;
        assert!(unwind.is_err());
        trace_owner.shutdown().expect("trace shutdown");

        let trace = fs::read_to_string(&trace_file).expect("trace file");
        assert_eq!(
            child_execution_event_summaries(&child_execution_events(&trace)),
            [
                ("00", "child_execution_started", 1, None),
                ("00", "child_execution_settled", 1, Some("failed")),
            ]
        );
    }

    #[tokio::test]
    async fn cancelling_child_construction_does_not_wait_for_arena_startup() {
        let workspace = tempfile::tempdir().unwrap();
        let construction_gate = ChildConstructionTestGate::new();
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create parent arena")
        .with_child_construction_test_gate(construction_gate.clone());
        let payload = super::ChildPayload::new(parent.child_source(), parent.checkpoint());
        let factory = ScriptedChildFactory::new([vec![ScriptedAction::Message {
            text: "child complete".to_owned(),
        }]]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "canonical base".to_owned(),
                factory,
            )),
        );
        let caller = coordinator.root_caller().await;
        let admitted = coordinator
            .start(&caller, parameters(), payload)
            .await
            .expect("admit child");
        construction_gate.wait_until_construction_started().await;

        let mut cancellation = Box::pin(coordinator.cancel(&caller, admitted.id()));
        let cancellation_result = timeout(Duration::from_secs(1), &mut cancellation).await;
        construction_gate.resume_construction();
        let (settled, observation) = match cancellation_result {
            Ok(observation) => (true, observation.unwrap()),
            Err(_) => (false, cancellation.await.unwrap()),
        };

        assert!(settled, "cancellation waited for arena construction");
        assert_eq!(observation.state(), TaskState::Cancelled);
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn cancelling_child_collection_does_not_wait_for_execution_timeout() {
        let workspace = tempfile::tempdir().unwrap();
        let collection_gate = CollectionTestGate::new();
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create parent arena")
        .with_collection_test_gate(collection_gate.clone());
        let payload = super::ChildPayload::new(parent.child_source(), parent.checkpoint());
        let factory = ScriptedChildFactory::new([vec![ScriptedAction::Message {
            text: "child complete".to_owned(),
        }]]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "canonical base".to_owned(),
                factory,
            )),
        );
        let caller = coordinator.root_caller().await;
        let admitted = coordinator
            .start(&caller, parameters(), payload)
            .await
            .expect("admit child");
        collection_gate.wait_until_collection_started().await;

        let mut cancellation = Box::pin(coordinator.cancel(&caller, admitted.id()));
        let cancellation_result = timeout(Duration::from_secs(1), &mut cancellation).await;
        collection_gate.resume_collection();
        let (settled, observation) = match cancellation_result {
            Ok(observation) => (true, observation.unwrap()),
            Err(_) => (false, cancellation.await.unwrap()),
        };

        assert!(settled, "cancellation waited for result collection");
        assert_eq!(observation.state(), TaskState::Cancelled);
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn observations_report_fixed_selection_steps_time_and_unavailable_cost() {
        let clock = Arc::new(ManualClock::new(1_000));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            policy(),
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let selection = ModelSelection::new(&crate::config::models::TERRA, "high")
            .expect("terra selection is valid");
        let admitted = coordinator
            .start(&root, parameters_with(selection, false), ())
            .await
            .unwrap();
        assert_eq!(admitted.state(), TaskState::Starting);
        assert_eq!(admitted.model(), "openai/gpt-5.6-terra");
        assert_eq!(admitted.thinking(), "high");
        assert_eq!(admitted.step_count(), 0);
        assert_eq!(admitted.elapsed(), Duration::ZERO);
        assert_eq!(admitted.observed_at_ms(), 1_000);
        assert_eq!(admitted.estimated_cost_usd(), None);
        assert_eq!(admitted.cost_status(), CostStatus::Unavailable);
        assert_eq!(admitted.error(), None);

        let running = started.recv().await.expect("child starts");
        running.child.model_step_started();
        running.child.model_step_started();
        clock.advance(Duration::from_millis(2_500));
        let observation = coordinator.get(&root, admitted.id()).await.unwrap();
        assert_eq!(observation.state(), TaskState::Running);
        assert_eq!(observation.step_count(), 2);
        assert_eq!(observation.elapsed(), Duration::from_millis(2_500));
        assert_eq!(observation.observed_at_ms(), 3_500);
        assert_eq!(observation.estimated_cost_usd(), None);
        assert_eq!(observation.cost_status(), CostStatus::Unavailable);

        running.finish.send(Ok(result("done"))).unwrap();
        clock.advance(Duration::from_millis(500));
        let (_, settled) = coordinator.wait(&root, admitted.id()).await.unwrap();
        assert_eq!(settled.elapsed(), Duration::from_secs(3));
        assert_eq!(settled.observed_at_ms(), 4_000);
        clock.advance(Duration::from_secs(5));
        let later = coordinator.get(&root, admitted.id()).await.unwrap();
        assert_eq!(later.elapsed(), Duration::from_secs(3));
        assert_eq!(later.observed_at_ms(), 9_000);
    }

    #[tokio::test]
    async fn authoritative_attempts_update_child_cost_before_settlement() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let admitted = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let Started { child, finish } = started.recv().await.expect("child starts");
        let (_, _, _, _, _, _, reporter, _) = child.into_parts();
        let known = known_usage();
        reporter.set_attempts(vec![known.clone()]);

        let observation = coordinator.get(&caller, admitted.id()).await.unwrap();
        assert!(observation.estimated_cost_usd().is_some());
        assert!(matches!(
            observation.cost(),
            crate::pricing::RunCostEstimate::Complete(_)
        ));
        assert_eq!(observation.cost_status(), CostStatus::Complete);

        reporter.set_attempts(vec![
            known,
            crate::model::AttemptAccounting::UsageUnavailable,
        ]);
        let lower_bound = coordinator.get(&caller, admitted.id()).await.unwrap();
        assert!(lower_bound.estimated_cost_usd().is_some());
        assert_eq!(lower_bound.cost_status(), CostStatus::LowerBound);

        reporter.set_attempts(vec![crate::model::AttemptAccounting::UsageUnavailable]);
        let unavailable = coordinator.get(&caller, admitted.id()).await.unwrap();
        assert_eq!(unavailable.estimated_cost_usd(), None);
        assert_eq!(unavailable.cost_status(), CostStatus::Unavailable);

        finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, admitted.id()).await.unwrap();
    }

    #[tokio::test]
    async fn failure_and_cancellation_retain_known_usage() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let failed = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let Started { child, finish } = started.recv().await.expect("child starts");
        let (_, _, _, _, _, _, reporter, _) = child.into_parts();
        reporter.set_attempts(vec![known_usage()]);
        finish
            .send(Err(ChildFailure::new("provider failed")))
            .unwrap();
        assert_failed(
            coordinator.wait(&caller, failed.id()).await.unwrap_err(),
            failed.id(),
            "provider failed",
        );
        let failed_record = coordinator.record(failed.id()).await.unwrap();
        assert_eq!(failed_record.observation().state(), TaskState::Failed);
        assert_eq!(failed_record.attempts().len(), 1);
        assert_eq!(
            failed_record.observation().cost_status(),
            CostStatus::Complete
        );

        let cancelled = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let Started { child, .. } = started.recv().await.expect("child starts");
        let (_, _, _, _, _, _, reporter, _) = child.into_parts();
        reporter.set_attempts(vec![known_usage()]);
        coordinator.cancel(&caller, cancelled.id()).await.unwrap();
        let cancelled_record = coordinator.record(cancelled.id()).await.unwrap();
        assert_eq!(cancelled_record.observation().state(), TaskState::Cancelled);
        assert_eq!(cancelled_record.attempts().len(), 1);
        assert_eq!(
            cancelled_record.observation().cost_status(),
            CostStatus::Complete
        );
    }

    #[tokio::test]
    async fn publication_orders_spawn_record_and_finish_and_freezes_reporters() {
        let (coordinator, mut started) = coordinator(policy());
        let mut events = coordinator
            .take_event_receiver()
            .expect("first observer takes coordinator events");
        let caller = coordinator.root_caller().await;
        let admitted = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let Started { child, finish } = started.recv().await.expect("child starts");
        let (_, _, _, _, _, _, reporter, _) = child.into_parts();

        let CoordinatorEvent::Spawned { record } = events.recv().await.expect("spawn event") else {
            panic!("first event must be spawned");
        };
        assert_eq!(record.id(), admitted.id());
        assert_eq!(record.parameters().prompt(), "inspect the lifecycle");
        assert_eq!(record.revision(), 1);
        assert!(record.attempts().is_empty());

        reporter.model_step_started();
        reporter.attempt_started(1, 7);
        let accounting = known_usage();
        reporter.attempt_settled(1, 7, accounting.clone());
        reporter.attempt_started(1, 8);
        reporter.attempt_settled(1, 8, accounting.clone());
        reporter.set_attempts(vec![accounting.clone(), accounting]);
        finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, admitted.id()).await.unwrap();

        let mut revisions = Vec::new();
        let final_record = loop {
            match events.recv().await.expect("lifecycle event") {
                CoordinatorEvent::Changed { id, revision } => {
                    assert_eq!(id, admitted.id().clone());
                    revisions.push(revision);
                }
                CoordinatorEvent::Finished { record } => break record,
                CoordinatorEvent::Spawned { .. } => panic!("spawn is emitted exactly once"),
            }
        };
        assert!(revisions.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            revisions
                .iter()
                .all(|revision| *revision < final_record.revision())
        );
        assert_eq!(final_record.observation().step_count(), 1);
        assert_eq!(final_record.attempts().len(), 2);
        assert_eq!(coordinator.active_count().await, 0);
        assert_eq!(final_record.attempt_evidence().len(), 2);
        assert_eq!(final_record.attempt_evidence()[0].model_step(), 1);
        assert_eq!(final_record.attempt_evidence()[0].provider_attempt(), 7);
        assert!(final_record.attempt_evidence()[0].accounting().is_some());
        assert_eq!(final_record.attempt_evidence()[1].provider_attempt(), 8);

        reporter.set_attempts(vec![crate::model::AttemptAccounting::UsageUnavailable]);
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert_eq!(
            coordinator.record(admitted.id()).await.unwrap().attempts(),
            final_record.attempts()
        );
    }

    #[tokio::test]
    async fn rejected_admission_has_no_publication_and_nested_lifecycle_stays_ordered() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            max_starts_per_run: 2,
            ..policy()
        });
        let mut events = coordinator
            .take_event_receiver()
            .expect("first observer takes coordinator events");
        let root = coordinator.root_caller().await;
        let parent = coordinator.start(&root, parameters(), ()).await.unwrap();
        let parent_running = started.recv().await.expect("parent starts");
        let child = coordinator
            .start(parent_running.child.caller(), parameters(), ())
            .await
            .expect("child starts");
        let child_running = started.recv().await.expect("child starts");
        assert_eq!(
            coordinator.start(&root, parameters(), ()).await,
            Err(AdmissionError::StartBudget)
        );

        parent_running.finish.send(Ok(result("parent"))).unwrap();
        coordinator.wait(&root, parent.id()).await.unwrap();
        assert_eq!(
            coordinator.get(&root, child.id()).await.unwrap().state(),
            TaskState::Running
        );
        child_running.finish.send(Ok(result("child"))).unwrap();
        coordinator.wait(&root, child.id()).await.unwrap();

        let mut spawns = Vec::new();
        let mut finishes = Vec::new();
        while finishes.len() < 2 {
            match events.recv().await.expect("lifecycle event") {
                CoordinatorEvent::Spawned { record } => {
                    spawns.push((record.id().clone(), record.parent().cloned()));
                }
                CoordinatorEvent::Finished { record } => finishes.push(record.id().clone()),
                CoordinatorEvent::Changed { .. } => {}
            }
        }
        assert_eq!(
            spawns,
            vec![
                (parent.id().clone(), None),
                (child.id().clone(), Some(parent.id().clone()))
            ]
        );
        assert_eq!(finishes, vec![parent.id().clone(), child.id().clone()]);
    }

    #[tokio::test]
    async fn inactive_event_receivers_do_not_accumulate_publications() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let task = coordinator.start(&root, parameters(), ()).await.unwrap();
        let running = started.recv().await.expect("child starts");

        let mut events = coordinator
            .take_event_receiver()
            .expect("first observer takes coordinator events");
        assert!(matches!(
            events.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        running.finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&root, task.id()).await.unwrap();
    }

    #[tokio::test]
    async fn finite_ancestor_deadlines_clamp_descendants_and_win_ties() {
        let clock = Arc::new(ManualClock::new(1_000));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(10),
                ..policy()
            },
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        clock.advance(Duration::from_secs(5));
        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;
        let (sibling_id, sibling) = start(&coordinator, &mut started, &root).await;
        assert_eq!(
            coordinator.next_deadline().await,
            Some(clock.now() + Duration::from_secs(5))
        );

        clock.advance(Duration::from_secs(5));
        coordinator.service().await;
        assert_timed_out(
            coordinator.wait(&root, &parent_id).await.unwrap_err(),
            &parent_id,
        );
        assert_cancelled(
            coordinator.wait(&root, &child_id).await.unwrap_err(),
            &child_id,
        );
        assert_eq!(
            coordinator.get(&root, &sibling_id).await.unwrap().state(),
            TaskState::Running
        );
        sibling.finish.send(Ok(result("sibling"))).unwrap();
        coordinator.wait(&root, &sibling_id).await.unwrap();
        assert_eq!(coordinator.next_deadline().await, None);
        drop((parent, child));
    }

    #[tokio::test]
    async fn timeout_holds_its_slot_until_cancellation_cleanup_finishes() {
        let clock = Arc::new(ManualClock::new(1_000));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                max_concurrent: 1,
                timeout: Duration::from_secs(10),
                ..policy()
            },
            Arc::new(CleanupRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let timed_out = coordinator.start(&root, parameters(), ()).await.unwrap();
        let cleanup = started.recv().await.expect("child starts");
        clock.advance(Duration::from_secs(10));
        coordinator.service().await;
        coordinator.service().await;

        let stopping = coordinator.get(&root, timed_out.id()).await.unwrap();
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("deadline expired"));
        assert_eq!(coordinator.active_count().await, 1);
        assert_eq!(coordinator.next_deadline().await, None);
        assert_eq!(
            coordinator.start(&root, parameters(), ()).await,
            Err(AdmissionError::ConcurrentLimit)
        );
        cleanup.release.send(()).unwrap();
        assert_timed_out(
            coordinator.wait(&root, timed_out.id()).await.unwrap_err(),
            timed_out.id(),
        );
        assert_eq!(coordinator.active_count().await, 0);

        let next = coordinator.start(&root, parameters(), ()).await.unwrap();
        let cleanup = started.recv().await.expect("next child starts");
        let release = async move {
            tokio::task::yield_now().await;
            cleanup.release.send(()).unwrap();
        };
        let (_, ()) = tokio::join!(coordinator.cancel_all(), release);
        assert_eq!(coordinator.active_count().await, 0);
        assert_cancelled(
            coordinator.wait(&root, next.id()).await.unwrap_err(),
            next.id(),
        );
    }

    #[tokio::test]
    async fn saturated_admission_keeps_lookup_wait_and_cancel_operable() {
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::new(
            SubagentPolicy {
                max_concurrent: 1,
                ..policy()
            },
            Arc::new(CleanupRunner {
                started: started_sender,
            }),
        );
        let caller = coordinator.root_caller().await;
        let task = coordinator.start(&caller, parameters(), ()).await.unwrap();
        let cleanup = started.recv().await.expect("child starts");

        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::ConcurrentLimit)
        );
        assert_eq!(
            coordinator.get(&caller, task.id()).await.unwrap().state(),
            TaskState::Running
        );
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(9, sender);
        let mut wait = Box::pin(coordinator.wait_with_phase(&caller, task.id(), Some(&signal)));
        tokio::select! {
            phase = receiver.recv() => assert_eq!(phase, Some(9)),
            result = &mut wait => panic!("wait finished before its phase signal: {result:?}"),
        }
        let mut cancel = Box::pin(coordinator.cancel(&caller, task.id()));
        let stopping = async {
            loop {
                if coordinator.get(&caller, task.id()).await.unwrap().state() == TaskState::Stopping
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        };
        tokio::pin!(stopping);
        tokio::select! {
            _ = &mut stopping => {}
            result = &mut cancel => panic!("cancellation finished before cleanup: {result:?}"),
        }
        assert_eq!(coordinator.active_count().await, 1);
        cleanup.release.send(()).unwrap();

        assert_eq!(cancel.await.unwrap().state(), TaskState::Cancelled);
        assert_cancelled(wait.await.unwrap_err(), task.id());
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[test]
    fn failures_are_utf8_bounded() {
        let failure = ChildFailure::new("é".repeat(FAILURE_MESSAGE_LIMIT));
        assert_eq!(failure.message().len(), FAILURE_MESSAGE_LIMIT);
        assert!(failure.message().is_char_boundary(failure.message().len()));
    }
}
