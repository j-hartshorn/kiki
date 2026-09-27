use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex as SyncMutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use serde::Serialize;
#[cfg(test)]
use tokio::sync::Semaphore;
use tokio::sync::{Mutex, MutexGuard, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentProgress};
use crate::arena::{ArenaChildSource, Checkpoint, Execution};
use crate::config::{
    ModelSelection, SessionPreferences,
    constants::{MAX_PROMPT_BYTES, SUBAGENT_TIMER_HEADROOM},
    models::{ModelConfiguration, Provider, model_by_id},
    settings::{SettingsContext, SubagentSettings},
};
use crate::model::{AttemptAccounting, Model, ModelProgress};
use crate::pricing::{RunCostEstimate, estimate_run_cost};
use crate::prompt::{child_prompt, child_prompt_with_catalog};
use crate::session::{
    SessionError, SessionWriter, TaskAdmission, TaskSnapshot, TaskStepCommit, TaskTerminal,
};
use crate::telemetry::{ChildStopCause, CoordinatorOperation, Settlement, Trace, TraceEvent};
use crate::tools::{ToolError, ToolValue};

const FAILURE_MESSAGE_LIMIT: usize = 4 * 1024;
const MAINTENANCE_LOCK_TRACE_THRESHOLD: Duration = Duration::from_millis(5);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct TaskId(String);

enum TaskIdParts {
    Path(Vec<u64>),
    Legacy(u64),
}

impl TaskId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Self::parts(value).map(|_| Self(value.to_owned()))
    }

    fn parts(value: &str) -> Option<TaskIdParts> {
        let path = value
            .split('.')
            .map(|segment| {
                if segment.is_empty()
                    || segment.len() > 1 && segment.starts_with('0')
                    || !segment
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase())
                {
                    None
                } else {
                    u64::from_str_radix(segment, 36).ok()
                }
            })
            .collect::<Option<Vec<_>>>();
        if let Some(path) = path {
            return Some(TaskIdParts::Path(path));
        }
        if value.len() >= 2
            && !value.contains('.')
            && (value.len() == 2 || !value.starts_with('0'))
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            u64::from_str_radix(value, 16).ok().map(TaskIdParts::Legacy)
        } else {
            None
        }
    }
}

impl PartialOrd for TaskId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (Self::parts(&self.0), Self::parts(&other.0)) {
            (Some(TaskIdParts::Path(left)), Some(TaskIdParts::Path(right))) => {
                left.cmp(&right).then_with(|| self.0.cmp(&other.0))
            }
            (Some(TaskIdParts::Path(_)), Some(TaskIdParts::Legacy(_))) => std::cmp::Ordering::Less,
            (Some(TaskIdParts::Legacy(_)), Some(TaskIdParts::Path(_))) => {
                std::cmp::Ordering::Greater
            }
            (Some(TaskIdParts::Legacy(left)), Some(TaskIdParts::Legacy(right))) => {
                left.cmp(&right).then_with(|| self.0.cmp(&other.0))
            }
            _ => self.0.cmp(&other.0),
        }
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
    OperationSettled { id: TaskId },
}

#[derive(Default)]
struct CoordinatorPublication {
    taken: bool,
    sender: Option<mpsc::UnboundedSender<CoordinatorEvent>>,
}

fn publish_publication_event(
    publication: &Arc<SyncMutex<CoordinatorPublication>>,
    event: CoordinatorEvent,
) {
    if let Some(sender) = publication
        .lock()
        .expect("coordinator publication lock")
        .sender
        .as_ref()
    {
        let _ = sender.send(event);
    }
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
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    pub(crate) fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "starting" => Some(Self::Starting),
            "running" => Some(Self::Running),
            "stopping" => Some(Self::Stopping),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "timed_out" => Some(Self::TimedOut),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
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
    origin_step_id: Option<u64>,
}

impl Caller {
    fn with_child_owner(&self, owner: TaskId) -> Self {
        Self {
            session: self.session,
            owner: CallerOwner::Child(owner),
            origin: self.origin,
            origin_step_id: self.origin_step_id,
        }
    }
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
    pub(crate) fn model_step(&self) -> u64 {
        self.model_step
    }

    pub(crate) fn provider_attempt(&self) -> u32 {
        self.provider_attempt
    }

    pub(crate) fn accounting(&self) -> Option<&AttemptAccounting> {
        self.accounting.as_ref()
    }

    pub(crate) fn for_persisted(
        model_step: u64,
        provider_attempt: u32,
        accounting: Option<AttemptAccounting>,
    ) -> Self {
        Self {
            model_step,
            provider_attempt,
            accounting,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        model_step: u64,
        provider_attempt: u32,
        accounting: Option<AttemptAccounting>,
    ) -> Self {
        Self::for_persisted(model_step, provider_attempt, accounting)
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

    pub(crate) fn for_persisted(
        model_step: u64,
        source: String,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    ) -> Self {
        Self {
            model_step,
            source,
            reasoning_summary,
            execution,
            attempts,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        model_step: u64,
        source: String,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    ) -> Self {
        Self::for_persisted(model_step, source, reasoning_summary, execution, attempts)
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
    terminal_claimed: bool,
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
            if state.terminal_claimed {
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

    fn claim_terminal(&self) -> Option<TaskMetricSnapshot> {
        let mut state = self.state.lock().expect("task metrics lock");
        if state.terminal_claimed {
            return None;
        }
        state.terminal_claimed = true;
        let revision = state
            .revision
            .checked_add(1)
            .expect("subagent task revision overflowed");
        Some(TaskMetricSnapshot {
            revision,
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
        })
    }

    fn publish_terminal(&self, revision: u64) {
        {
            let mut state = self.state.lock().expect("task metrics lock");
            assert!(state.terminal_claimed, "terminal revision must be claimed");
            assert!(!state.settled, "terminal revision must be published once");
            assert_eq!(
                revision,
                state
                    .revision
                    .checked_add(1)
                    .expect("subagent task revision overflowed"),
                "terminal revision must follow the live revision"
            );
            state.revision = revision;
            state.settled = true;
        }
        self.notify(revision);
    }

    fn publish_changed(&self, revision: u64) {
        self.notify(revision);
        publish_publication_event(
            &self.publication,
            CoordinatorEvent::Changed {
                id: self.id.clone(),
                revision,
            },
        );
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
    providers: Vec<Provider>,
}

impl AcceptedThinking {
    pub(crate) fn new(
        selection: ModelSelection,
        persisted: SettingsContext,
        restored: Option<SessionPreferences>,
    ) -> Self {
        let values = restored
            .map(|preferences| preferences.thinking().clone())
            .unwrap_or_else(|| BTreeMap::from([(selection.model().id, selection.thinking())]));
        Self(Arc::new(RwLock::new(AcceptedThinkingState {
            persisted,
            values,
            providers: vec![Provider::OpenAi, Provider::Anthropic],
        })))
    }

    pub(crate) fn with_providers(self, providers: Vec<Provider>) -> Self {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .providers = providers;
        self
    }

    pub(crate) fn accept(&self, selection: ModelSelection) {
        self.0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values
            .insert(selection.model().id, selection.thinking());
    }

    pub(crate) fn proposal(&self, selection: ModelSelection) -> SessionPreferences {
        let state = self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut values = state.values.clone();
        values.insert(selection.model().id, selection.thinking());
        SessionPreferences::new(selection, values)
            .expect("accepted thinking values must be supported by their models")
    }

    fn allows_model(&self, model: &'static ModelConfiguration) -> bool {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .persisted
            .favourites()
            .is_none_or(|models| models.contains(&model))
    }

    fn has_provider(&self, provider: Provider) -> bool {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .providers
            .contains(&provider)
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
        let state = self
            .0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let target = target.or_else(|| state.persisted.favourites().map(|models| models[0]));
        let Some(target) = target else {
            return invoking;
        };
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

    pub(crate) fn set_origin_step_id(&mut self, origin_step_id: u64) {
        self.caller.origin_step_id = Some(origin_step_id);
    }

    pub(crate) async fn persist_child_step(&self) -> bool {
        self.coordinator.persist_child_step(&self.caller).await
    }

    pub(crate) fn execution(
        &self,
        source: ArenaChildSource,
        selection: ModelSelection,
        checkpoint: Checkpoint,
        cancellation: CancellationToken,
    ) -> TrustedExecutionContext {
        TrustedExecutionContext {
            caller: self.caller.clone(),
            selection,
            checkpoint,
            source,
            coordinator: self.coordinator.clone(),
            accepted_thinking: self.accepted_thinking.clone(),
            cancellation,
        }
    }
}

#[derive(Clone)]
pub(crate) struct TrustedExecutionContext {
    caller: Caller,
    selection: ModelSelection,
    checkpoint: Checkpoint,
    source: ArenaChildSource,
    coordinator: SessionSubagents,
    accepted_thinking: Option<AcceptedThinking>,
    cancellation: CancellationToken,
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
        if let Some(target) = target
            && !accepted_thinking.allows_model(target)
        {
            return Err(ToolError::value(format!(
                "subagent model `{}` is not a favourite",
                target.id
            )));
        }
        let selection = accepted_thinking.resolve(self.selection, target);
        let provider = selection.model().provider();
        if !accepted_thinking.has_provider(provider) {
            return Err(ToolError::value(format!(
                "subagent model `{}` needs {}, which is not set",
                selection.model().id,
                provider.api_key_variable()
            )));
        }
        Ok(selection)
    }

    pub(crate) fn coordinator(&self) -> &SessionSubagents {
        &self.coordinator
    }

    fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
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
                .start_with_cancellation(
                    context.caller(),
                    ChildParameters::new(prompt, selection, read_only),
                    context.payload(),
                    context.cancellation(),
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
                Err(WaitError::StorageUnavailable) => {
                    Err(ToolError::runtime("session storage is unavailable"))
                }
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
            let observation = match context
                .coordinator()
                .cancel_with_phase(context.caller(), &id, wait_phase.as_ref())
                .await
            {
                Ok(observation) => observation,
                Err(WaitError::Access(error)) => return Err(access_error(error)),
                Err(WaitError::StorageUnavailable) => {
                    return Err(ToolError::runtime("session storage is unavailable"));
                }
                Err(error) => {
                    unreachable!("cancellation only waits for durable terminal results: {error}")
                }
            };
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
        ToolError::value("subagent task ID must be a dot-separated lowercase base-36 path")
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
            ToolValue::String(observation.state().as_str().to_owned()),
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

fn cost_status_name(status: CostStatus) -> &'static str {
    match status {
        CostStatus::Complete => "complete",
        CostStatus::LowerBound => "lower_bound",
        CostStatus::Unavailable => "unavailable",
    }
}

pub(crate) trait ChildModelFactory: Send + Sync + 'static {
    type Model: Model;

    fn create(&self, prompt: String) -> Self::Model;
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
        let tool_reference = payload.source.prompt_reference(read_only);
        let child_prompt = match payload.source.skill_catalog() {
            Some(catalog) => child_prompt_with_catalog(&prompt_base, catalog, tool_reference),
            None => child_prompt(&prompt_base, tool_reference),
        };
        let model = models.create(child_prompt);
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
                    commentary: _,
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
                .run_with_progress(selection, request, &cancellation, &mut progress)
                .await
            {
                Ok(final_message) => {
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
    storage_id: u64,
    revision: u64,
    parent: Option<TaskId>,
    parent_storage_id: Option<u64>,
    admission_history_step_id: Option<u64>,
    terminal_history_step_id: Option<u64>,
    origin: OriginId,
    origin_step_id: Option<u64>,
    depth: usize,
    admitted_at_ms: i64,
    terminal_at_ms: Option<i64>,
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

    pub(crate) fn storage_id(&self) -> u64 {
        self.storage_id
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn parent(&self) -> Option<&TaskId> {
        self.parent.as_ref()
    }

    pub(crate) fn parent_storage_id(&self) -> Option<u64> {
        self.parent_storage_id
    }

    pub(crate) fn admission_history_step_id(&self) -> Option<u64> {
        self.admission_history_step_id
    }

    pub(crate) fn terminal_history_step_id(&self) -> Option<u64> {
        self.terminal_history_step_id
    }

    #[cfg(test)]
    pub(crate) fn origin(&self) -> u64 {
        self.origin.0
    }

    pub(crate) fn origin_step_id(&self) -> Option<u64> {
        self.origin_step_id
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn admitted_at_ms(&self) -> i64 {
        self.admitted_at_ms
    }

    pub(crate) fn terminal_at_ms(&self) -> Option<i64> {
        self.terminal_at_ms
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
            storage_id: test_storage_id(&id),
            parent_storage_id: parent.as_ref().map(test_storage_id),
            id: id.clone(),
            revision,
            parent,
            admission_history_step_id: None,
            terminal_history_step_id: None,
            origin: OriginId(0),
            origin_step_id: None,
            depth,
            admitted_at_ms: 0,
            terminal_at_ms: None,
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
    pub(crate) fn with_storage_ids_for_test(
        mut self,
        storage_id: u64,
        parent_storage_id: Option<u64>,
    ) -> Self {
        self.storage_id = storage_id;
        self.parent_storage_id = parent_storage_id;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_history_steps_for_test(
        mut self,
        admission: Option<u64>,
        terminal: Option<u64>,
    ) -> Self {
        self.admission_history_step_id = admission;
        self.terminal_history_step_id = terminal;
        self
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

    #[cfg(test)]
    pub(crate) fn with_parameters_for_test(
        mut self,
        prompt: String,
        selection: ModelSelection,
        read_only: bool,
    ) -> Self {
        self.parameters = ChildParameters::new(prompt, selection, read_only);
        self.observation.selection = selection;
        self
    }

    pub(crate) fn from_persisted(restored: RestoredTaskRecord) -> Self {
        let RestoredTaskRecord {
            id,
            storage_id,
            revision,
            parent,
            parent_storage_id,
            admission_history_step_id,
            terminal_history_step_id,
            origin_step_id,
            depth,
            parameters,
            attempts,
            details,
        } = restored;
        let cost = estimate_run_cost(&attempts);
        let selection = parameters.selection();
        let error = match &details.outcome {
            Some(TaskOutcome::Failed(error)) => Some(error.message().to_owned()),
            Some(TaskOutcome::Completed(_))
            | Some(TaskOutcome::Cancelled)
            | Some(TaskOutcome::TimedOut)
            | None => None,
        };
        Self {
            id: id.clone(),
            storage_id,
            revision,
            parent,
            parent_storage_id,
            admission_history_step_id,
            terminal_history_step_id,
            origin: OriginId(origin_step_id),
            origin_step_id: Some(origin_step_id),
            depth,
            admitted_at_ms: details.admitted_at_ms,
            terminal_at_ms: details.terminal_at_ms,
            parameters,
            observation: TaskObservation {
                id,
                state: details.state,
                selection,
                step_count: details.step_count,
                elapsed: details.elapsed,
                observed_at_ms: details.observed_at_ms,
                cost,
                error,
            },
            attempt_evidence: details.attempt_evidence,
            attempts,
            executions: details.executions,
            outcome: details.outcome,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_persisted_details_for_test(mut self, details: TaskPersistedDetails) -> Self {
        self.admitted_at_ms = details.admitted_at_ms;
        self.terminal_at_ms = details.terminal_at_ms;
        self.observation.state = details.state;
        self.observation.step_count = details.step_count;
        self.observation.elapsed = details.elapsed;
        self.observation.observed_at_ms = details.observed_at_ms;
        self.observation.error = match &details.outcome {
            Some(TaskOutcome::Failed(error)) => Some(error.message().to_owned()),
            Some(TaskOutcome::Completed(_))
            | Some(TaskOutcome::Cancelled)
            | Some(TaskOutcome::TimedOut)
            | None => None,
        };
        self.attempt_evidence = details.attempt_evidence;
        self.executions = details.executions;
        self.outcome = details.outcome;
        self
    }
}

pub(crate) struct RestoredTaskRecord {
    pub(crate) id: TaskId,
    pub(crate) storage_id: u64,
    pub(crate) revision: u64,
    pub(crate) parent: Option<TaskId>,
    pub(crate) parent_storage_id: Option<u64>,
    pub(crate) admission_history_step_id: Option<u64>,
    pub(crate) terminal_history_step_id: Option<u64>,
    pub(crate) origin_step_id: u64,
    pub(crate) depth: usize,
    pub(crate) parameters: ChildParameters,
    pub(crate) attempts: Vec<AttemptAccounting>,
    pub(crate) details: TaskPersistedDetails,
}

pub(crate) struct TaskPersistedDetails {
    pub(crate) admitted_at_ms: i64,
    pub(crate) terminal_at_ms: Option<i64>,
    pub(crate) state: TaskState,
    pub(crate) step_count: u64,
    pub(crate) elapsed: Duration,
    pub(crate) observed_at_ms: i64,
    pub(crate) attempt_evidence: Vec<TaskAttempt>,
    pub(crate) executions: Vec<TaskExecution>,
    pub(crate) outcome: Option<TaskOutcome>,
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
    #[error("subagent timeout cannot be represented as a timer-safe deadline")]
    DeadlineUnrepresentable,
    #[error("subagent task identifier space is exhausted")]
    IdentifierSpaceExhausted,
    #[error("subagent admission was cancelled before persistence")]
    Cancelled,
    #[error("subagent task has no originating model step")]
    MissingProvenance,
    #[error("subagent task provenance is not a stored model step")]
    InvalidProvenance,
    #[error("session storage is unavailable")]
    StorageUnavailable,
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
    #[error("session storage is unavailable")]
    StorageUnavailable,
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
            Self::Access(_) | Self::StorageUnavailable => None,
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
#[derive(Clone)]
pub(crate) struct TerminalSettlementTestGate {
    terminal_settled: Arc<Semaphore>,
    allow_operation_drop: Arc<Semaphore>,
    operation_dropped: Arc<Semaphore>,
}

#[cfg(test)]
impl TerminalSettlementTestGate {
    pub(crate) fn new() -> Self {
        Self {
            terminal_settled: Arc::new(Semaphore::new(0)),
            allow_operation_drop: Arc::new(Semaphore::new(0)),
            operation_dropped: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_terminal_settled(&self) {
        take_test_permit(&self.terminal_settled).await;
    }

    pub(crate) fn allow_operation_drop(&self) {
        self.allow_operation_drop.add_permits(1);
    }

    pub(crate) async fn wait_until_operation_dropped(&self) {
        take_test_permit(&self.operation_dropped).await;
    }

    async fn wait_after_terminal_settlement(&self) {
        self.terminal_settled.add_permits(1);
        take_test_permit(&self.allow_operation_drop).await;
    }

    fn operation_dropped(&self) {
        self.operation_dropped.add_permits(1);
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

struct TrackedOperations {
    active: AtomicUsize,
    idle: tokio::sync::Notify,
}

impl TrackedOperations {
    fn begin(self: &Arc<Self>) -> OperationGuard {
        self.active.fetch_add(1, Ordering::Relaxed);
        OperationGuard {
            operations: Arc::clone(self),
        }
    }

    fn count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct OperationGuard {
    operations: Arc<TrackedOperations>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        if self.operations.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.operations.idle.notify_waiters();
        }
    }
}

pub(crate) struct SubagentCoordinator<P: Send + 'static> {
    inner: Arc<Mutex<Inner>>,
    publication: Arc<SyncMutex<CoordinatorPublication>>,
    policy: SubagentPolicy,
    runner: Arc<dyn ChildRunner<P>>,
    writer: Option<SessionWriter>,
    storage_failure: CancellationToken,
    operations: Arc<TrackedOperations>,
    clock: Arc<dyn Clock>,
    trace: Trace,
    #[cfg(test)]
    start_test_gate: Option<StartSubagentTestGate>,
    #[cfg(test)]
    run_child_test_gate: Option<RunChildTestGate>,
    #[cfg(test)]
    terminal_settlement_test_gate: Option<TerminalSettlementTestGate>,
}

impl<P: Send + 'static> Clone for SubagentCoordinator<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            publication: self.publication.clone(),
            policy: self.policy,
            runner: self.runner.clone(),
            writer: self.writer.clone(),
            storage_failure: self.storage_failure.clone(),
            operations: self.operations.clone(),
            clock: self.clock.clone(),
            trace: self.trace.clone(),
            #[cfg(test)]
            start_test_gate: self.start_test_gate.clone(),
            #[cfg(test)]
            run_child_test_gate: self.run_child_test_gate.clone(),
            #[cfg(test)]
            terminal_settlement_test_gate: self.terminal_settlement_test_gate.clone(),
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
    next_sibling: BTreeMap<Option<TaskId>, Option<u64>>,
    active_slots: usize,
    shutting_down: bool,
    storage_failed: bool,
    blocked_origins: BTreeSet<OriginId>,
    active_root_origins: BTreeSet<OriginId>,
    selected_origins: Option<BTreeSet<u64>>,
    origin_starts: BTreeMap<OriginId, usize>,
    notifications: watch::Sender<u64>,
    reserved_task_ids: BTreeSet<TaskId>,
    pending: BTreeMap<TaskId, Task>,
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
    storage_id: u64,
    parent: Option<TaskId>,
    parent_storage_id: Option<u64>,
    admission_history_step_id: Option<u64>,
    terminal_history_step_id: Option<u64>,
    origin: OriginId,
    origin_step_id: Option<u64>,
    depth: usize,
    parameters: ChildParameters,
    admitted_at: Instant,
    admitted_at_ms: i64,
    deadline: Instant,
    settled_at: Option<Instant>,
    terminal_at_ms: Option<i64>,
    metrics: Arc<TaskMetrics>,
    state: TaskState,
    stop: Option<StopReason>,
    outcome: Option<TaskOutcome>,
    terminal_claim: Option<TerminalClaim>,
    restored_record: Option<TaskRecord>,
    holds_slot: bool,
    cancellation: CancellationToken,
}

#[derive(Clone)]
struct TerminalClaim {
    outcome: TaskOutcome,
    metrics: TaskMetricSnapshot,
}

struct TerminalSettlement {
    settled_at: Instant,
    terminal_at_ms: i64,
    record: TaskRecord,
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
    fn restored(record: TaskRecord, notifications: watch::Sender<u64>) -> Self {
        let now = Instant::now();
        let admitted_at = now.checked_sub(record.observation.elapsed()).unwrap_or(now);
        Self {
            id: record.id.clone(),
            storage_id: record.storage_id,
            parent: record.parent.clone(),
            parent_storage_id: record.parent_storage_id,
            admission_history_step_id: record.admission_history_step_id,
            terminal_history_step_id: record.terminal_history_step_id,
            origin: record.origin,
            origin_step_id: record.origin_step_id,
            depth: record.depth,
            parameters: record.parameters.clone(),
            admitted_at,
            admitted_at_ms: record.admitted_at_ms,
            deadline: now,
            settled_at: Some(now),
            terminal_at_ms: record.terminal_at_ms,
            metrics: Arc::new(TaskMetrics::new(
                record.id.clone(),
                notifications,
                Arc::new(SyncMutex::new(CoordinatorPublication::default())),
            )),
            state: record.observation.state(),
            stop: None,
            outcome: record.outcome.clone(),
            terminal_claim: None,
            restored_record: Some(record),
            holds_slot: false,
            cancellation: CancellationToken::new(),
        }
    }

    fn observation(&self, now: Instant, observed_at_ms: i64) -> TaskObservation {
        self.restored_record
            .as_ref()
            .map(|record| record.observation.clone())
            .unwrap_or_else(|| {
                self.observation_with_metrics(now, observed_at_ms, self.metrics.snapshot())
            })
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
        self.restored_record.clone().unwrap_or_else(|| {
            self.record_from_snapshot(now, observed_at_ms, self.metrics.snapshot())
        })
    }

    fn record_from_snapshot(
        &self,
        now: Instant,
        observed_at_ms: i64,
        metrics: TaskMetricSnapshot,
    ) -> TaskRecord {
        TaskRecord {
            id: self.id.clone(),
            storage_id: self.storage_id,
            revision: metrics.revision,
            parent: self.parent.clone(),
            parent_storage_id: self.parent_storage_id,
            admission_history_step_id: self.admission_history_step_id,
            terminal_history_step_id: self.terminal_history_step_id,
            origin: self.origin,
            origin_step_id: self.origin_step_id,
            depth: self.depth,
            admitted_at_ms: self.admitted_at_ms,
            terminal_at_ms: self.terminal_at_ms,
            parameters: self.parameters.clone(),
            observation: self.observation_with_metrics(now, observed_at_ms, metrics.clone()),
            attempt_evidence: metrics.attempt_evidence,
            attempts: metrics.attempts,
            executions: metrics.executions,
            outcome: self.outcome.clone(),
        }
    }

    fn claim_terminal(&mut self, outcome: TaskOutcome) -> Option<TerminalClaim> {
        let claim = TerminalClaim {
            outcome,
            metrics: self.metrics.claim_terminal()?,
        };
        self.terminal_claim = Some(claim.clone());
        Some(claim)
    }

    fn terminal_settlement(
        &self,
        claim: &TerminalClaim,
        settled_at: Instant,
        terminal_at_ms: i64,
        terminal_history_step_id: Option<u64>,
    ) -> TerminalSettlement {
        let mut record =
            self.record_from_snapshot(settled_at, terminal_at_ms, claim.metrics.clone());
        record.revision = claim.metrics.revision;
        record.terminal_at_ms = Some(terminal_at_ms);
        record.terminal_history_step_id = terminal_history_step_id;
        record.observation.state = terminal_state(&claim.outcome);
        record.observation.elapsed = settled_at.saturating_duration_since(self.admitted_at);
        record.observation.error = match &claim.outcome {
            TaskOutcome::Failed(failure) => Some(failure.message().to_owned()),
            TaskOutcome::Completed(_) | TaskOutcome::Cancelled | TaskOutcome::TimedOut => None,
        };
        record.outcome = Some(claim.outcome.clone());
        TerminalSettlement {
            settled_at,
            terminal_at_ms,
            record,
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
                next_sibling: BTreeMap::new(),
                active_slots: 0,
                shutting_down: false,
                storage_failed: false,
                blocked_origins: BTreeSet::new(),
                active_root_origins: BTreeSet::new(),
                selected_origins: None,
                origin_starts: BTreeMap::new(),
                notifications: watch::channel(0).0,
                reserved_task_ids: BTreeSet::new(),
                pending: BTreeMap::new(),
                tasks: BTreeMap::new(),
            })),
            publication: Arc::new(SyncMutex::new(CoordinatorPublication::default())),
            policy,
            runner,
            writer: None,
            storage_failure: CancellationToken::new(),
            operations: Arc::new(TrackedOperations {
                active: AtomicUsize::new(0),
                idle: tokio::sync::Notify::new(),
            }),
            clock: Arc::new(SystemClock),
            trace: Trace::disabled(),
            #[cfg(test)]
            start_test_gate: None,
            #[cfg(test)]
            run_child_test_gate: None,
            #[cfg(test)]
            terminal_settlement_test_gate: None,
        }
    }

    pub(crate) fn with_session(mut self, writer: SessionWriter) -> Self {
        self.writer = Some(writer);
        self
    }

    pub(crate) async fn restore_terminal_tasks(
        &self,
        tasks: impl IntoIterator<Item = TaskRecord>,
        next_task: Option<u64>,
        reserved_task_ids: impl IntoIterator<Item = TaskId>,
    ) {
        let mut inner = self.inner.lock().await;
        inner.next_task = next_task;
        inner.reserved_task_ids.extend(reserved_task_ids);
        let notifications = inner.notifications.clone();
        for record in tasks {
            if let Some(origin_step_id) = record.origin_step_id {
                inner.next_origin = inner.next_origin.max(
                    origin_step_id
                        .checked_add(1)
                        .expect("stored origin step identifier must leave a root origin"),
                );
            }
            let id = record.id.clone();
            inner.reserved_task_ids.insert(id.clone());
            inner
                .tasks
                .insert(id, Task::restored(record, notifications.clone()));
        }
    }

    pub(crate) fn operation_count(&self) -> usize {
        self.operations.count()
    }

    pub(crate) async fn replace_selected_origins(
        &self,
        origins: impl IntoIterator<Item = u64>,
    ) -> bool {
        if self.operation_count() != 0 {
            return false;
        }
        let mut inner = self.lock_inner(CoordinatorOperation::Record).await;
        if self.operation_count() != 0 {
            return false;
        }
        inner.selected_origins = Some(origins.into_iter().collect());
        true
    }

    pub(crate) async fn settle_root_run(
        &self,
        caller: &Caller,
        origins: impl IntoIterator<Item = u64>,
    ) {
        let mut inner = self.lock_inner(CoordinatorOperation::RootCaller).await;
        assert!(
            caller.session == inner.session,
            "root caller belongs to this session"
        );
        assert!(
            matches!(caller.owner, CallerOwner::Root),
            "caller is a root"
        );
        assert!(
            inner.active_root_origins.remove(&caller.origin),
            "root caller remains active until settlement"
        );
        inner
            .selected_origins
            .get_or_insert_default()
            .extend(origins);
    }

    pub(crate) fn storage_failure_token(&self) -> CancellationToken {
        self.storage_failure.clone()
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

    #[cfg(test)]
    pub(crate) fn with_terminal_settlement_test_gate(
        mut self,
        terminal_settlement_test_gate: TerminalSettlementTestGate,
    ) -> Self {
        self.terminal_settlement_test_gate = Some(terminal_settlement_test_gate);
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
        publish_publication_event(&self.publication, event);
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
        inner.active_root_origins.insert(origin);
        Caller {
            session: inner.session,
            owner: CallerOwner::Root,
            origin,
            origin_step_id: None,
        }
    }

    #[cfg(test)]
    pub(crate) async fn start(
        &self,
        caller: &Caller,
        parameters: ChildParameters,
        payload: P,
    ) -> Result<TaskObservation, AdmissionError> {
        self.start_with_cancellation(caller, parameters, payload, CancellationToken::new())
            .await
    }

    pub(crate) async fn start_with_cancellation(
        &self,
        caller: &Caller,
        mut parameters: ChildParameters,
        payload: P,
        invocation_cancellation: CancellationToken,
    ) -> Result<TaskObservation, AdmissionError> {
        let operation = self.operations.begin();
        #[cfg(test)]
        if let Some(start_test_gate) = &self.start_test_gate {
            start_test_gate.note_start_attempt();
        }
        let (spawned, writer) = {
            let mut inner = self.lock_inner(CoordinatorOperation::Start).await;
            #[cfg(test)]
            if let Some(start_test_gate) = &self.start_test_gate {
                start_test_gate.wait_before_first_start_returns().await;
            }
            let now = self.now();
            let observed_at_ms = self.unix_time_ms();
            if inner.storage_failed {
                return Err(AdmissionError::StorageUnavailable);
            }
            if inner.shutting_down {
                return Err(AdmissionError::ShuttingDown);
            }
            if caller.session != inner.session {
                return Err(AdmissionError::InvalidCaller);
            }
            if self.writer.is_some() && caller.origin_step_id.is_none() {
                return Err(AdmissionError::MissingProvenance);
            }
            if inner.blocked_origins.contains(&caller.origin) {
                return Err(AdmissionError::CallerStopped);
            }

            let (parent, parent_storage_id, depth, inherited_read_only, inherited_deadline) =
                match &caller.owner {
                    CallerOwner::Root => (None, None, 1, false, None),
                    CallerOwner::Child(parent) => {
                        let Some(task) = inner.tasks.get(parent) else {
                            return Err(AdmissionError::InvalidCaller);
                        };
                        if task.origin != caller.origin
                            || task.origin_step_id != caller.origin_step_id
                        {
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
                            Some(task.storage_id),
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
                    .filter(|deadline| deadline.checked_add(SUBAGENT_TIMER_HEADROOM).is_some())
                    .ok_or(AdmissionError::DeadlineUnrepresentable)?;
            let sequence = inner
                .next_task
                .ok_or(AdmissionError::IdentifierSpaceExhausted)?;
            if self.writer.is_some() && sequence > i64::MAX as u64 {
                return Err(AdmissionError::IdentifierSpaceExhausted);
            }
            let id = allocate_task_id(&mut inner, parent.as_ref())?;
            let admission_history_step_id = self
                .writer
                .as_ref()
                .and_then(SessionWriter::active_execution_step);
            parameters.read_only |= inherited_read_only;
            let cancellation = self.storage_failure.child_token();
            let metrics = Arc::new(TaskMetrics::new(
                id.clone(),
                inner.notifications.clone(),
                self.publication.clone(),
            ));
            let task = Task {
                id: id.clone(),
                storage_id: sequence,
                parent,
                parent_storage_id,
                admission_history_step_id,
                terminal_history_step_id: None,
                origin: caller.origin,
                origin_step_id: caller.origin_step_id,
                depth,
                parameters,
                admitted_at: now,
                admitted_at_ms: observed_at_ms,
                deadline,
                settled_at: None,
                terminal_at_ms: None,
                metrics,
                state: TaskState::Starting,
                stop: None,
                outcome: None,
                terminal_claim: None,
                restored_record: None,
                holds_slot: true,
                cancellation,
            };
            let spawned = task.record(now, observed_at_ms);
            inner.next_task = sequence.checked_add(1);
            inner.active_slots += 1;
            *inner.origin_starts.entry(caller.origin).or_default() += 1;
            inner.pending.insert(id.clone(), task);
            inner.notify();
            if self.writer.is_none() {
                if invocation_cancellation.is_cancelled() {
                    let task = inner
                        .pending
                        .remove(&id)
                        .expect("in-memory admission remains reserved");
                    release_pending_task(&mut inner, task);
                    inner.notify();
                    return Err(AdmissionError::Cancelled);
                }
                let task = inner
                    .pending
                    .remove(&id)
                    .expect("in-memory admission remains reserved");
                let child = ChildRun {
                    id: id.clone(),
                    caller: caller.with_child_owner(id.clone()),
                    parent: task.parent.clone(),
                    depth: task.depth,
                    parameters: task.parameters.clone(),
                    payload,
                    reporter: TaskReporter {
                        metrics: task.metrics.clone(),
                    },
                    coordinator: self.clone(),
                    trace: self.trace.clone(),
                };
                let observation = spawned.observation().clone();
                inner.tasks.insert(id.clone(), task);
                inner.notify();
                drop(inner);
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
                    let terminal = coordinator.run_child(child).await;
                    coordinator.settle_spawned_task(operation, terminal).await;
                });
                return Ok(observation);
            }
            (
                spawned,
                self.writer
                    .clone()
                    .expect("persistent admission requires a session writer"),
            )
        };

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let coordinator = self.clone();
        let caller = caller.clone();
        tokio::spawn(async move {
            let terminal = coordinator
                .complete_admission(
                    writer,
                    spawned,
                    payload,
                    caller,
                    invocation_cancellation,
                    sender,
                )
                .await;
            coordinator.settle_spawned_task(operation, terminal).await;
        });
        receiver
            .await
            .unwrap_or(Err(AdmissionError::StorageUnavailable))
    }

    async fn settle_spawned_task(self, operation: OperationGuard, terminal: Option<TaskRecord>) {
        let settled = terminal.as_ref().map(|record| record.id().clone());
        if let Some(record) = terminal {
            self.publish_event(CoordinatorEvent::Finished { record });
        }
        if let Some(id) = &settled {
            self.publish_event(CoordinatorEvent::OperationSettled { id: id.clone() });
        }
        #[cfg(test)]
        let terminal_settlement_test_gate = self.terminal_settlement_test_gate.clone();
        #[cfg(test)]
        if settled.is_some()
            && let Some(gate) = &terminal_settlement_test_gate
        {
            gate.wait_after_terminal_settlement().await;
        }
        drop(self);
        drop(operation);
        #[cfg(test)]
        if let Some(gate) = terminal_settlement_test_gate {
            gate.operation_dropped();
        }
    }

    async fn complete_admission(
        &self,
        writer: SessionWriter,
        spawned: TaskRecord,
        payload: P,
        caller: Caller,
        invocation_cancellation: CancellationToken,
        sender: tokio::sync::oneshot::Sender<Result<TaskObservation, AdmissionError>>,
    ) -> Option<TaskRecord> {
        let id = spawned.id().clone();
        let admission_cancellation = {
            let inner = self.lock_inner(CoordinatorOperation::Start).await;
            let Some(task) = inner.pending.get(&id) else {
                let error = if self.storage_failure.is_cancelled() {
                    AdmissionError::StorageUnavailable
                } else {
                    AdmissionError::Cancelled
                };
                let _ = sender.send(Err(error));
                return None;
            };
            task.cancellation.clone()
        };
        let Some(origin_step_id) = spawned.origin_step_id() else {
            self.discard_pending(&id).await;
            let _ = sender.send(Err(AdmissionError::MissingProvenance));
            return None;
        };
        let admission = match TaskAdmission::from_record(&spawned, origin_step_id) {
            Ok(admission) => admission,
            Err(SessionError::InvalidTaskProvenance) => {
                self.discard_pending(&id).await;
                let _ = sender.send(Err(AdmissionError::InvalidProvenance));
                return None;
            }
            Err(SessionError::IdOverflow) => {
                self.discard_pending(&id).await;
                let _ = sender.send(Err(AdmissionError::IdentifierSpaceExhausted));
                return None;
            }
            Err(_) => {
                self.discard_pending(&id).await;
                self.latch_storage_failure().await;
                let _ = sender.send(Err(AdmissionError::StorageUnavailable));
                return None;
            }
        };
        let failure = self.storage_failure.clone();
        let admission_invocation_cancellation = invocation_cancellation.clone();
        let committed = writer
            .admit_subagent_task(admission, move || {
                admission_invocation_cancellation.is_cancelled()
                    || admission_cancellation.is_cancelled()
                    || failure.is_cancelled()
            })
            .await;
        match committed {
            Ok(true) => {}
            Ok(false) => {
                self.discard_pending(&id).await;
                let error = if self.storage_failure.is_cancelled() {
                    AdmissionError::StorageUnavailable
                } else {
                    AdmissionError::Cancelled
                };
                let _ = sender.send(Err(error));
                return None;
            }
            Err(SessionError::InvalidTaskProvenance) => {
                self.discard_pending(&id).await;
                let _ = sender.send(Err(AdmissionError::InvalidProvenance));
                return None;
            }
            Err(SessionError::IdOverflow) => {
                self.discard_pending(&id).await;
                let _ = sender.send(Err(AdmissionError::IdentifierSpaceExhausted));
                return None;
            }
            Err(_) => {
                self.discard_pending(&id).await;
                self.latch_storage_failure().await;
                let _ = sender.send(Err(AdmissionError::StorageUnavailable));
                return None;
            }
        }

        let child = {
            let mut inner = self.lock_inner(CoordinatorOperation::Start).await;
            let Some(task) = inner.pending.remove(&id) else {
                let error = if self.storage_failure.is_cancelled() {
                    AdmissionError::StorageUnavailable
                } else {
                    AdmissionError::Cancelled
                };
                let _ = sender.send(Err(error));
                return None;
            };
            let child = ChildRun {
                id: id.clone(),
                caller: caller.with_child_owner(id.clone()),
                parent: task.parent.clone(),
                depth: task.depth,
                parameters: task.parameters.clone(),
                payload,
                reporter: TaskReporter {
                    metrics: task.metrics.clone(),
                },
                coordinator: self.clone(),
                trace: self.trace.clone(),
            };
            inner.tasks.insert(id.clone(), task);
            inner.notify();
            child
        };
        self.trace.emit(|| {
            TraceEvent::child_admitted(
                &child.id,
                child.parent.as_ref(),
                child.parameters.selection(),
            )
        });
        self.publish_event(CoordinatorEvent::Spawned {
            record: spawned.clone(),
        });
        let _ = sender.send(Ok(spawned.observation().clone()));
        self.run_child(child).await
    }

    async fn discard_pending(&self, id: &TaskId) {
        let mut inner = self.lock_inner(CoordinatorOperation::Start).await;
        if let Some(task) = inner.pending.remove(id) {
            release_pending_task(&mut inner, task);
            inner.notify();
        }
    }

    pub(crate) async fn latch_storage_failure(&self) {
        self.storage_failure.cancel();
        let mut inner = self.lock_inner(CoordinatorOperation::Shutdown).await;
        if inner.storage_failed {
            return;
        }
        inner.storage_failed = true;
        inner.shutting_down = true;
        let mut origins = inner
            .tasks
            .values()
            .chain(inner.pending.values())
            .map(|task| task.origin)
            .collect::<BTreeSet<_>>();
        inner.blocked_origins.append(&mut origins);
        let pending = std::mem::take(&mut inner.pending);
        for (_, task) in pending {
            task.cancellation.cancel();
            release_pending_task(&mut inner, task);
        }
        for task in inner.tasks.values_mut() {
            if !task.state.is_terminal() {
                task.stop.get_or_insert(StopReason::Shutdown);
                task.cancellation.cancel();
                task.holds_slot = false;
            }
        }
        inner.active_slots = 0;
        inner.notify();
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
    ) -> Result<TaskObservation, WaitError> {
        self.cancel_with_phase(caller, id, None).await
    }

    async fn cancel_with_phase(
        &self,
        caller: &Caller,
        id: &TaskId,
        wait_phase: Option<&WaitPhaseSignal>,
    ) -> Result<TaskObservation, WaitError> {
        {
            let inner = self.lock_inner(CoordinatorOperation::Cancel).await;
            ensure_access(&inner, caller, id).map_err(WaitError::Access)?;
        }
        let stopping = self.request_stop_subtree(id, StopReason::Cancelled).await;
        if !stopping.is_empty() {
            if let Some(wait_phase) = wait_phase {
                wait_phase.notify();
            }
            self.wait_for_all(stopping).await?;
        }
        self.get(caller, id).await.map_err(WaitError::Access)
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
        let _ = self.wait_for_all(active).await;
    }

    #[cfg(test)]
    pub(crate) async fn cancel_all(&self) {
        self.request_background_cancellation(None).await;
        self.join_active().await;
    }

    pub(crate) async fn service(&self) {
        let now = self.now();
        let mut expired = {
            let mut inner = self.lock_inner(CoordinatorOperation::Service).await;
            for task in inner.pending.values_mut() {
                if task.stop.is_none() && task.deadline <= now {
                    task.stop = Some(StopReason::TimedOut);
                    task.cancellation.cancel();
                }
            }
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
        let inner = self.lock_inner(CoordinatorOperation::NextDeadline).await;
        inner
            .tasks
            .values()
            .chain(inner.pending.values())
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
        self.operations.wait().await;
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

    async fn persist_child_step(&self, caller: &Caller) -> bool {
        let (writer, record, task_cancellation) = {
            let inner = self.lock_inner(CoordinatorOperation::Record).await;
            let CallerOwner::Child(id) = &caller.owner else {
                return true;
            };
            if caller.session != inner.session || inner.storage_failed {
                return false;
            }
            let Some(writer) = self.writer.clone() else {
                return true;
            };
            let Some(task) = inner.tasks.get(id) else {
                return false;
            };
            (
                writer,
                task.record(self.now(), self.unix_time_ms()),
                task.cancellation.clone(),
            )
        };
        let snapshot = match TaskSnapshot::from_record(&record) {
            Ok(snapshot) => snapshot,
            Err(_) => {
                self.latch_storage_failure().await;
                return false;
            }
        };
        let failure = self.storage_failure.clone();
        match writer
            .task_step(snapshot, move || {
                task_cancellation.is_cancelled() || failure.is_cancelled()
            })
            .await
        {
            Ok(TaskStepCommit::Committed) => true,
            Ok(TaskStepCommit::Superseded | TaskStepCommit::Rejected) => false,
            Err(_) => {
                self.latch_storage_failure().await;
                false
            }
        }
    }

    #[cfg(test)]
    pub(crate) async fn active_count(&self) -> usize {
        self.inner.lock().await.active_slots
    }

    async fn run_child(&self, child: ChildRun<P>) -> Option<TaskRecord> {
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
            if task.cancellation.is_cancelled() && task.stop.is_none() {
                task.stop = Some(StopReason::Cancelled);
            }
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
        self.finish_child(&id, result).await
    }

    async fn finish_child(
        &self,
        id: &TaskId,
        result: Result<ChildResult, ChildFailure>,
    ) -> Option<TaskRecord> {
        let (claim, stop_descendants) = {
            let mut inner = self.lock_inner(CoordinatorOperation::FinishChild).await;
            let task = inner
                .tasks
                .get_mut(id)
                .expect("admitted child must remain retained");
            if task.state.is_terminal() || task.terminal_claim.is_some() {
                return None;
            }
            let (outcome, stop_descendants) = if let Some(stop) = task.stop.clone() {
                (stop.outcome(), true)
            } else {
                match result {
                    Ok(result) => (TaskOutcome::Completed(result), false),
                    Err(failure) => {
                        task.stop = Some(StopReason::RunnerFailed(failure.clone()));
                        task.transition(TaskState::Stopping);
                        (TaskOutcome::Failed(failure), true)
                    }
                }
            };
            (task.claim_terminal(outcome), stop_descendants)
        };
        let claim = claim?;
        if stop_descendants {
            let descendants = self
                .request_stop_descendants(id, StopReason::AncestorStopped)
                .await;
            let _ = self.wait_for_all(descendants).await;
        }
        let settlement = self.prepare_terminal_settlement(id, &claim).await?;
        self.persist_terminal(id, settlement).await
    }

    async fn prepare_terminal_settlement(
        &self,
        id: &TaskId,
        claim: &TerminalClaim,
    ) -> Option<TerminalSettlement> {
        let inner = self
            .lock_inner(CoordinatorOperation::FinishChildSettlement)
            .await;
        if inner.storage_failed {
            return None;
        }
        let task = inner.tasks.get(id)?;
        let stored_claim = task.terminal_claim.as_ref()?;
        if stored_claim.metrics.revision != claim.metrics.revision {
            return None;
        }
        Some(
            task.terminal_settlement(
                stored_claim,
                self.now(),
                self.unix_time_ms(),
                self.writer
                    .as_ref()
                    .and_then(SessionWriter::active_execution_step),
            ),
        )
    }

    async fn persist_terminal(
        &self,
        id: &TaskId,
        terminal_settlement: TerminalSettlement,
    ) -> Option<TaskRecord> {
        let writer = self.writer.clone();
        let persisted = match writer {
            Some(writer) => match TaskTerminal::from_record(&terminal_settlement.record) {
                Ok(terminal) => writer.terminal_subagent_task(terminal).await,
                Err(error) => Err(error),
            },
            None => Ok(()),
        };
        if persisted.is_err() {
            self.latch_storage_failure().await;
            return None;
        }
        let record = {
            let mut inner = self
                .lock_inner(CoordinatorOperation::FinishChildSettlement)
                .await;
            if inner.storage_failed {
                return None;
            }
            let task = inner.tasks.get_mut(id)?;
            if task
                .terminal_claim
                .as_ref()
                .is_none_or(|claim| claim.metrics.revision != terminal_settlement.record.revision())
            {
                return None;
            }
            let stored_claim = task
                .terminal_claim
                .take()
                .expect("matching terminal claim remains present");
            task.state = terminal_state(&stored_claim.outcome);
            task.outcome = Some(stored_claim.outcome);
            task.settled_at = Some(terminal_settlement.settled_at);
            task.terminal_at_ms = Some(terminal_settlement.terminal_at_ms);
            task.terminal_history_step_id = terminal_settlement.record.terminal_history_step_id();
            let metrics = task.metrics.clone();
            let revision = terminal_settlement.record.revision();
            let record = terminal_settlement.record;
            let releases_slot = task.holds_slot;
            task.holds_slot = false;
            if releases_slot {
                inner.active_slots = inner
                    .active_slots
                    .checked_sub(1)
                    .expect("every active task owns exactly one slot");
            }
            metrics.publish_terminal(revision);
            inner.notify();
            self.trace.emit(|| TraceEvent::ChildSettled {
                child_id: id.clone(),
                outcome: settlement(record.outcome().expect("terminal record has outcome")),
            });
            record
        };
        Some(record)
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
        stop_pending_tasks(&mut inner, id, &reason);
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
        stop_pending_tasks(&mut inner, id, &reason);
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
            .chain(inner.pending.values())
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.origin)
            .collect::<BTreeSet<_>>();
        if let Some(caller) = active.filter(|caller| caller.session == inner.session) {
            origins.insert(caller.origin);
        }
        inner.blocked_origins.extend(origins);
        for task in inner.pending.values_mut() {
            if task.stop.is_none() {
                task.stop = Some(reason.clone());
                task.cancellation.cancel();
            }
        }
        let ids = inner
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();
        stop_tasks(&mut inner, &ids, reason, &self.trace);
        inner.notify();
    }

    async fn wait_for_all(&self, ids: Vec<TaskId>) -> Result<(), WaitError> {
        for id in ids {
            self.wait_for_terminal(&id).await?;
        }
        Ok(())
    }

    async fn wait_for_terminal(&self, id: &TaskId) -> Result<TaskOutcome, WaitError> {
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
                return Ok(outcome);
            }
            tokio::select! {
                _ = self.storage_failure.cancelled() => return Err(WaitError::StorageUnavailable),
                result = updates.changed() => result.expect("retained task update channel remains open"),
            };
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
            tokio::select! {
                _ = self.storage_failure.cancelled() => return Err(WaitError::StorageUnavailable),
                result = updates.changed() => result.expect("retained task update channel remains open"),
            };
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
    let visible = inner.selected_origins.as_ref().is_none_or(|origins| {
        target
            .origin_step_id
            .is_some_and(|origin| origins.contains(&origin))
            || (inner.active_root_origins.contains(&caller.origin)
                && target.origin == caller.origin)
    });
    if !visible {
        return Err(AccessError::Denied(id.clone()));
    }
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

fn stop_pending_tasks(inner: &mut Inner, ancestor: &TaskId, reason: &StopReason) {
    for task in inner.pending.values_mut() {
        if task
            .parent
            .as_ref()
            .is_some_and(|parent| is_in_subtree(&inner.tasks, parent, ancestor))
            && task.stop.is_none()
        {
            task.stop = Some(reason.clone());
            task.cancellation.cancel();
        }
    }
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
        if task.terminal_claim.is_some() {
            stopping.push(id.clone());
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

fn terminal_state(outcome: &TaskOutcome) -> TaskState {
    match outcome {
        TaskOutcome::Completed(_) => TaskState::Completed,
        TaskOutcome::Failed(_) => TaskState::Failed,
        TaskOutcome::Cancelled => TaskState::Cancelled,
        TaskOutcome::TimedOut => TaskState::TimedOut,
    }
}

fn settlement(outcome: &TaskOutcome) -> Settlement {
    match outcome {
        TaskOutcome::Completed(_) => Settlement::Succeeded,
        TaskOutcome::Failed(_) => Settlement::Failed,
        TaskOutcome::Cancelled | TaskOutcome::TimedOut => Settlement::Cancelled,
    }
}

fn release_pending_task(inner: &mut Inner, mut task: Task) {
    release_task_slot(inner, &mut task);
    let starts = inner
        .origin_starts
        .get_mut(&task.origin)
        .expect("pending task reserves an origin start");
    *starts = starts.checked_sub(1).expect("pending start is reserved");
    if *starts == 0 {
        inner.origin_starts.remove(&task.origin);
    }
}

fn release_task_slot(inner: &mut Inner, task: &mut Task) {
    if task.holds_slot {
        inner.active_slots = inner
            .active_slots
            .checked_sub(1)
            .expect("every active task owns exactly one slot");
        task.holds_slot = false;
    }
}

#[cfg(test)]
fn test_storage_id(id: &TaskId) -> u64 {
    match TaskId::parts(id.as_str()).expect("test task ID is valid") {
        TaskIdParts::Path(segments) => segments.into_iter().fold(0_u64, |encoded, segment| {
            encoded
                .checked_mul(37)
                .and_then(|encoded| encoded.checked_add(segment + 1))
                .expect("test ID fits storage")
        }),
        TaskIdParts::Legacy(value) => value,
    }
}

fn allocate_task_id(inner: &mut Inner, parent: Option<&TaskId>) -> Result<TaskId, AdmissionError> {
    let key = parent.cloned();
    let mut sequence = inner
        .next_sibling
        .get(&key)
        .copied()
        .unwrap_or(Some(0))
        .ok_or(AdmissionError::IdentifierSpaceExhausted)?;
    loop {
        let id = task_id(parent, sequence);
        let next = sequence.checked_add(1);
        inner.next_sibling.insert(key.clone(), next);
        if !inner.reserved_task_ids.contains(&id) {
            return Ok(id);
        }
        sequence = next.ok_or(AdmissionError::IdentifierSpaceExhausted)?;
    }
}

fn task_id(parent: Option<&TaskId>, sequence: u64) -> TaskId {
    let segment = base36(sequence);
    TaskId(match parent {
        Some(parent) => format!("{parent}.{segment}"),
        None => segment,
    })
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut encoded = Vec::new();
    loop {
        encoded.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    encoded.reverse();
    String::from_utf8(encoded).expect("base-36 digits are UTF-8")
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
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    use futures::FutureExt;
    use tokio::sync::{Notify, mpsc, oneshot, watch};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        AccessError, AdmissionError, ChildFailure, ChildParameters, ChildResult, ChildRun,
        ChildRunner, Clock, CoordinatorEvent, CostStatus, FAILURE_MESSAGE_LIMIT,
        RestoredTaskRecord, RunnerFuture, SubagentCoordinator, SubagentPolicy, TaskId, TaskOutcome,
        TaskPersistedDetails, TaskRecord, TaskState, WaitError, WaitPhaseSignal, task_id,
        task_id_argument,
    };
    use crate::arena::{Arena, ChildConstructionTestGate, CollectionTestGate, ExecutionOutcome};
    use crate::config::{
        DEFAULT_MODEL, ModelSelection,
        constants::{SUBAGENT_TIMER_HEADROOM, latest_representable_instant},
        models::Provider,
    };
    use crate::model::{
        AttemptAccounting, Model, ModelAction, ModelCallId, ModelContextEntry, ModelFailure,
        ModelHistory, ModelProgress, ModelStep, ProviderReplay, ScriptedAction, ScriptedModel,
    };
    use crate::session::{Session, SessionWriteGate, SessionWriteKind};
    use crate::skills::{ProjectScope, SkillCatalog};
    use crate::telemetry::TraceOwner;
    use crate::tools::{ReadTestGate, ToolErrorKind, ToolHost, ToolValue};

    #[tokio::test]
    async fn favourites_set_subagent_default_and_reject_unlisted_overrides() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".kiki")).unwrap();
        std::fs::write(home.path().join(".kiki/config.toml"), "favourites = [\"openai/gpt-6-luna\", \"openai/gpt-6-sol\"]\n[thinking]\n\"openai/gpt-6-sol\" = \"high\"\n").unwrap();
        let settings = crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        );
        let invoking = ModelSelection::new(&crate::config::models::TERRA, "low").unwrap();
        let accepted = super::AcceptedThinking::new(invoking, settings.settings().clone(), None);
        assert_eq!(
            accepted.resolve(invoking, None).model().id,
            "openai/gpt-6-luna"
        );
        assert_eq!(
            accepted
                .resolve(invoking, Some(&crate::config::models::GPT6_SOL))
                .thinking(),
            "high"
        );
        assert!(!accepted.allows_model(&crate::config::models::TERRA));
        assert!(!accepted.allows_model(&crate::config::models::SOL));
        assert!(accepted.allows_model(&crate::config::models::GPT6_SOL));

        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let coordinator: super::SessionSubagents = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "base".to_owned(),
                ScriptedChildFactory::new([]),
            )),
        );
        let caller = coordinator.root_caller().await;
        let context = super::AgentInvocationContext::new(caller, coordinator)
            .with_accepted_thinking(accepted)
            .execution(
                arena.child_source(),
                invoking,
                arena.checkpoint(),
                CancellationToken::new(),
            );
        assert_eq!(
            context.resolve_selection(None).unwrap().model().id,
            "openai/gpt-6-luna"
        );
        assert_eq!(
            context
                .resolve_selection(Some("openai/gpt-6-sol"))
                .unwrap()
                .thinking(),
            "high"
        );
        assert!(
            context
                .resolve_selection(Some(crate::config::models::TERRA.id))
                .is_err()
        );
        assert!(context.resolve_selection(Some("openai/unknown")).is_err());
    }

    #[tokio::test]
    async fn keyless_subagent_models_are_rejected_naming_the_missing_key_variable() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".kiki")).unwrap();
        std::fs::write(
            home.path().join(".kiki/config.toml"),
            "favourites = [\"anthropic/claude-opus-5-5\", \"openai/gpt-6-sol\"]\n",
        )
        .unwrap();
        let with_favourites = crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        );
        let without_favourites = crate::config::ready_snapshot_for_test();
        let arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
        )
        .await
        .unwrap();
        let invoking = ModelSelection::new(&crate::config::models::TERRA, "low").unwrap();
        let explicit = openai_only_context(&arena, invoking, &without_favourites).await;
        let favourite_default = openai_only_context(&arena, invoking, &with_favourites).await;

        for error in [
            explicit
                .resolve_selection(Some(crate::config::models::OPUS_5_5.id))
                .unwrap_err(),
            favourite_default.resolve_selection(None).unwrap_err(),
        ] {
            assert_eq!(error.kind, ToolErrorKind::Value);
            assert!(error.to_string().contains("ANTHROPIC_API_KEY"), "{error}");
        }
        assert_eq!(
            explicit
                .resolve_selection(Some(crate::config::models::SOL.id))
                .unwrap(),
            ModelSelection::new(
                &crate::config::models::SOL,
                crate::config::models::SOL.default_thinking
            )
            .unwrap()
        );
        assert_eq!(explicit.resolve_selection(None).unwrap(), invoking);
        assert_eq!(
            favourite_default
                .resolve_selection(Some(crate::config::models::GPT6_SOL.id))
                .unwrap()
                .model()
                .id,
            crate::config::models::GPT6_SOL.id
        );
    }

    async fn openai_only_context(
        arena: &Arena,
        invoking: ModelSelection,
        settings: &crate::config::StartupSnapshot,
    ) -> super::TrustedExecutionContext {
        let coordinator: super::SessionSubagents = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "base".to_owned(),
                ScriptedChildFactory::new([]),
            )),
        );
        let caller = coordinator.root_caller().await;
        super::AgentInvocationContext::new(caller, coordinator)
            .with_accepted_thinking(
                super::AcceptedThinking::new(invoking, settings.settings().clone(), None)
                    .with_providers(vec![Provider::OpenAi]),
            )
            .execution(
                arena.child_source(),
                invoking,
                arena.checkpoint(),
                CancellationToken::new(),
            )
    }

    #[test]
    fn absent_favourites_keep_existing_subagent_selection() {
        let settings = crate::config::ready_snapshot_for_test();
        let invoking = ModelSelection::new(&crate::config::models::TERRA, "low").unwrap();
        let accepted = super::AcceptedThinking::new(invoking, settings.settings().clone(), None);
        assert_eq!(accepted.resolve(invoking, None), invoking);
        assert_eq!(
            accepted
                .resolve(invoking, Some(&crate::config::models::SOL))
                .model()
                .id,
            crate::config::models::SOL.id
        );
    }

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
        selections: Arc<Mutex<Vec<ModelSelection>>>,
        histories: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
    }

    impl Model for RecordingScriptedModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            selection: ModelSelection,
            history: &'a ModelHistory,
            cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            self.selections.lock().unwrap().push(selection);
            self.histories
                .lock()
                .unwrap()
                .push(history.entries().to_vec());
            self.model.step(selection, history, cancellation, progress)
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

        fn create(&self, prompt: String) -> Self::Model {
            self.prompts.lock().unwrap().push(prompt);
            RecordingScriptedModel {
                model: ScriptedModel::new(
                    self.scripts
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("test configures one child script"),
                ),
                selections: self.selections.clone(),
                histories: self.histories.clone(),
            }
        }
    }

    #[derive(Clone)]
    struct TaskStepGatedChildFactory {
        calls: Arc<AtomicUsize>,
        histories: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        second_call_started: Arc<Notify>,
        allow_second_call: Arc<Notify>,
        step_attempts: Vec<Vec<AttemptAccounting>>,
    }

    struct TaskStepGatedChildModel {
        calls: Arc<AtomicUsize>,
        histories: Arc<Mutex<Vec<Vec<ModelContextEntry>>>>,
        second_call_started: Arc<Notify>,
        allow_second_call: Arc<Notify>,
        step_attempts: Vec<Vec<AttemptAccounting>>,
    }

    impl TaskStepGatedChildFactory {
        fn new() -> Self {
            Self::with_attempts(vec![
                vec![AttemptAccounting::UsageUnavailable],
                vec![AttemptAccounting::UsageUnavailable],
                vec![AttemptAccounting::UsageUnavailable],
            ])
        }

        fn with_attempts(step_attempts: Vec<Vec<AttemptAccounting>>) -> Self {
            Self {
                calls: Arc::new(AtomicUsize::new(0)),
                histories: Arc::new(Mutex::new(Vec::new())),
                second_call_started: Arc::new(Notify::new()),
                allow_second_call: Arc::new(Notify::new()),
                step_attempts,
            }
        }
    }

    impl super::ChildModelFactory for TaskStepGatedChildFactory {
        type Model = TaskStepGatedChildModel;

        fn create(&self, _prompt: String) -> Self::Model {
            TaskStepGatedChildModel {
                calls: self.calls.clone(),
                histories: self.histories.clone(),
                second_call_started: self.second_call_started.clone(),
                allow_second_call: self.allow_second_call.clone(),
                step_attempts: self.step_attempts.clone(),
            }
        }
    }

    impl Model for TaskStepGatedChildModel {
        #[allow(clippy::manual_async_fn)]
        fn step<'a>(
            &'a mut self,
            _selection: ModelSelection,
            history: &'a ModelHistory,
            _cancellation: &'a CancellationToken,
            progress: &'a mut (dyn FnMut(ModelProgress) + Send),
        ) -> impl std::future::Future<Output = Result<ModelStep, ModelFailure>> + Send + 'a
        {
            async move {
                self.histories
                    .lock()
                    .unwrap()
                    .push(history.entries().to_vec());
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                let attempts = self.step_attempts.get(call).cloned().unwrap_or_default();
                for (index, accounting) in attempts.iter().cloned().enumerate() {
                    let attempt = u32::try_from(index + 1).expect("attempt ordinal");
                    progress(ModelProgress::AttemptStarted { attempt });
                    progress(ModelProgress::AttemptSettled {
                        attempt,
                        accounting,
                    });
                }
                let action = match call {
                    0 => ModelAction::Code {
                        source: "first = 1".to_owned(),
                        call_id: ModelCallId::new("task-step-first"),
                        commentary: None,
                    },
                    1 => {
                        self.second_call_started.notify_one();
                        self.allow_second_call.notified().await;
                        ModelAction::Code {
                            source: "second = first + 1".to_owned(),
                            call_id: ModelCallId::new("task-step-second"),
                            commentary: None,
                        }
                    }
                    2 => ModelAction::Message {
                        text: "child complete".to_owned(),
                    },
                    _ => return Err(crate::model::ModelError::Exhausted.into()),
                };
                Ok(ModelStep {
                    action,
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts,
                })
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

    fn instant_with_tokio_unsafe_one_second_deadline() -> Instant {
        let latest = latest_representable_instant();
        let now = latest
            .checked_sub(Duration::from_secs(1))
            .expect("one-second timeout start must be representable");
        let deadline = now
            .checked_add(Duration::from_secs(1))
            .expect("raw one-second deadline must be representable");
        assert!(deadline.checked_add(SUBAGENT_TIMER_HEADROOM).is_none());
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

    fn restored_task(
        id: &str,
        parent: Option<&str>,
        origin_step_id: u64,
        message: &str,
    ) -> TaskRecord {
        TaskRecord::from_persisted(RestoredTaskRecord {
            id: TaskId::parse(id).expect("valid task ID"),
            storage_id: 0,
            revision: 2,
            parent: parent.map(|id| TaskId::parse(id).expect("valid parent task ID")),
            parent_storage_id: None,
            admission_history_step_id: None,
            terminal_history_step_id: None,
            origin_step_id,
            depth: 1,
            parameters: parameters(),
            attempts: Vec::new(),
            details: TaskPersistedDetails {
                admitted_at_ms: 100,
                terminal_at_ms: Some(200),
                state: TaskState::Completed,
                step_count: 1,
                elapsed: Duration::from_secs(1),
                observed_at_ms: 200,
                attempt_evidence: Vec::new(),
                executions: Vec::new(),
                outcome: Some(TaskOutcome::Completed(result(message))),
            },
        })
    }

    #[test]
    fn restored_terminal_observations_match_live_settlement() {
        let details = |state, outcome| TaskPersistedDetails {
            admitted_at_ms: 100,
            terminal_at_ms: Some(200),
            state,
            step_count: 3,
            elapsed: Duration::new(4, 5),
            observed_at_ms: 200,
            attempt_evidence: Vec::new(),
            executions: Vec::new(),
            outcome: Some(outcome),
        };
        let record = |details| {
            TaskRecord::from_persisted(RestoredTaskRecord {
                id: TaskId::parse("00").expect("valid task ID"),
                storage_id: 0,
                revision: 4,
                parent: None,
                parent_storage_id: None,
                admission_history_step_id: None,
                terminal_history_step_id: None,
                origin_step_id: 1,
                depth: 1,
                parameters: parameters(),
                attempts: Vec::new(),
                details,
            })
        };

        let completed = record(details(
            TaskState::Completed,
            TaskOutcome::Completed(result("saved")),
        ));
        assert_eq!(completed.observation().error(), None);
        assert_eq!(completed.observation().elapsed(), Duration::new(4, 5));
        assert_eq!(completed.observation().observed_at_ms(), 200);

        for (state, outcome) in [
            (TaskState::Cancelled, TaskOutcome::Cancelled),
            (TaskState::TimedOut, TaskOutcome::TimedOut),
        ] {
            assert_eq!(record(details(state, outcome)).observation().error(), None);
        }

        let failed = record(details(
            TaskState::Failed,
            TaskOutcome::Failed(ChildFailure::new("interrupted diagnostic")),
        ));
        assert_eq!(failed.observation().error(), Some("interrupted diagnostic"));
        assert_eq!(failed.observation().elapsed(), Duration::new(4, 5));
        assert_eq!(failed.observation().observed_at_ms(), 200);
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
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(0),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(1),
        })
    }

    async fn persisted_origin(
        gate: Option<SessionWriteGate>,
    ) -> (
        tempfile::TempDir,
        tempfile::TempDir,
        Session,
        crate::session::SessionWriter,
        u64,
        u64,
    ) {
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
        let session = match gate {
            Some(gate) => Session::create_with_write_gate(
                home.path(),
                workspace.path(),
                selection,
                arena.checkpoint(),
                gate,
            ),
            None => Session::create(home.path(), workspace.path(), selection, arena.checkpoint()),
        }
        .expect("create session");
        let writer = session.writer();
        let run_id = writer
            .commit_request("parent".to_owned(), selection)
            .await
            .expect("start parent run")
            .run_id;
        let origin_step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start parent model step");
        (home, workspace, session, writer, run_id, origin_step_id)
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

    #[tokio::test]
    async fn restore_without_tasks_allocates_the_first_child_as_zero() {
        let (coordinator, mut started) = coordinator(policy());
        coordinator
            .restore_terminal_tasks(Vec::new(), Some(0), Vec::new())
            .await;
        let caller = coordinator.root_caller().await;

        let (id, child) = start(&coordinator, &mut started, &caller).await;

        assert_eq!(id.as_str(), "0");
        drop(child);
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn restore_does_not_reuse_a_hidden_public_task_id() {
        let (coordinator, mut started) = coordinator(policy());
        let first = TaskRecord::for_test("0", 0, parameters().selection(), None, 1, Vec::new());
        coordinator
            .restore_terminal_tasks(
                [first],
                Some(2),
                [TaskId::parse("1").expect("reserved task ID")],
            )
            .await;
        let caller = coordinator.root_caller().await;

        let (id, child) = start(&coordinator, &mut started, &caller).await;

        assert_eq!(id.as_str(), "2");
        drop(child);
        coordinator.shutdown().await;
    }

    #[tokio::test]
    async fn restored_terminal_tasks_wait_without_runner_or_events() {
        let (coordinator, mut started) = coordinator(policy());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator events");
        let completed =
            TaskRecord::for_test("00", 7, parameters().selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: Some(200),
                    state: TaskState::Completed,
                    step_count: 3,
                    elapsed: Duration::new(4, 5),
                    observed_at_ms: 200,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: Some(TaskOutcome::Completed(ChildResult::new(
                        "stored result".to_owned(),
                        BTreeMap::from([(
                            "nested".to_owned(),
                            ToolValue::Dictionary(BTreeMap::from([(
                                "answer".to_owned(),
                                ToolValue::Integer(7),
                            )])),
                        )]),
                    ))),
                });
        let failed = TaskRecord::for_test("01", 8, parameters().selection(), None, 1, Vec::new())
            .with_persisted_details_for_test(TaskPersistedDetails {
                admitted_at_ms: 300,
                terminal_at_ms: Some(400),
                state: TaskState::Failed,
                step_count: 5,
                elapsed: Duration::new(6, 7),
                observed_at_ms: 400,
                attempt_evidence: Vec::new(),
                executions: Vec::new(),
                outcome: Some(TaskOutcome::Failed(ChildFailure::new("stored diagnostic"))),
            });
        let completed_id = completed.id().clone();
        let failed_id = failed.id().clone();
        coordinator
            .restore_terminal_tasks([completed, failed], Some(2), Vec::new())
            .await;
        let caller = coordinator.root_caller().await;

        let (result, observation) = coordinator
            .wait(&caller, &completed_id)
            .await
            .expect("return stored completed result");
        assert_eq!(result.message(), "stored result");
        assert_eq!(
            result.values(),
            &BTreeMap::from([(
                "nested".to_owned(),
                ToolValue::Dictionary(BTreeMap::from([(
                    "answer".to_owned(),
                    ToolValue::Integer(7),
                )])),
            )])
        );
        assert_eq!(observation.elapsed(), Duration::new(4, 5));
        assert_eq!(observation.observed_at_ms(), 200);

        let error = coordinator
            .wait(&caller, &failed_id)
            .await
            .expect_err("reraises stored failure");
        let failed_observation = error
            .observation()
            .expect("wait returns stored observation");
        assert_eq!(failed_observation.elapsed(), Duration::new(6, 7));
        assert_eq!(failed_observation.observed_at_ms(), 400);
        assert_failed(error, &failed_id, "stored diagnostic");
        let observation = coordinator
            .get(&caller, &failed_id)
            .await
            .expect("get stored failure observation");
        assert_eq!(observation.elapsed(), Duration::new(6, 7));
        assert_eq!(observation.observed_at_ms(), 400);
        assert_eq!(coordinator.active_count().await, 0);
        assert!(coordinator.next_deadline().await.is_none());
        assert!(
            started.try_recv().is_err(),
            "restoration must not start a runner"
        );
        assert!(
            events.try_recv().is_err(),
            "restoration must not publish lifecycle events"
        );
    }

    #[tokio::test]
    async fn root_access_is_limited_to_selected_origin_steps() {
        let (coordinator, mut started) = coordinator(policy());
        let selected = restored_task("00", None, 11, "selected");
        let sibling = restored_task("01", None, 12, "sibling");
        let selected_id = selected.id().clone();
        let sibling_id = sibling.id().clone();
        coordinator
            .restore_terminal_tasks([selected, sibling], Some(2), std::iter::empty())
            .await;
        assert!(coordinator.replace_selected_origins([11]).await);
        let root = coordinator.root_caller().await;

        assert_eq!(
            coordinator.get(&root, &selected_id).await.unwrap().state(),
            TaskState::Completed
        );
        assert_eq!(
            coordinator.get(&root, &sibling_id).await,
            Err(AccessError::Denied(sibling_id.clone()))
        );
        assert_eq!(
            coordinator.wait(&root, &sibling_id).await,
            Err(WaitError::Access(AccessError::Denied(sibling_id.clone())))
        );
        assert_eq!(
            coordinator.cancel(&root, &sibling_id).await,
            Err(WaitError::Access(AccessError::Denied(sibling_id.clone())))
        );

        assert!(coordinator.replace_selected_origins([12]).await);
        assert_eq!(
            coordinator
                .wait(&root, &sibling_id)
                .await
                .unwrap()
                .0
                .message(),
            "sibling"
        );
        assert_eq!(
            coordinator.get(&root, &selected_id).await,
            Err(AccessError::Denied(selected_id))
        );
        assert_eq!(
            coordinator
                .records()
                .await
                .iter()
                .map(|record| record.id().as_str())
                .collect::<Vec<_>>(),
            ["00", "01"]
        );
        assert_eq!(coordinator.inner.lock().await.next_task, Some(2));
        assert!(
            started.try_recv().is_err(),
            "restoration must not start a runner"
        );
    }

    #[tokio::test]
    async fn root_access_uses_active_origin_until_durable_settlement() {
        let (coordinator, mut started) = coordinator(policy());
        assert!(coordinator.replace_selected_origins([]).await);
        let mut root = coordinator.root_caller().await;
        root.origin_step_id = Some(11);
        let (id, running) = start(&coordinator, &mut started, &root).await;

        root.origin_step_id = Some(12);
        assert_eq!(
            coordinator.get(&root, &id).await.unwrap().state(),
            TaskState::Running
        );
        coordinator.settle_root_run(&root, [11, 12]).await;
        let later_root = coordinator.root_caller().await;
        assert_eq!(
            coordinator.get(&later_root, &id).await.unwrap().state(),
            TaskState::Running
        );

        running.finish.send(Ok(result("done"))).unwrap();
        assert_eq!(
            coordinator
                .wait(&later_root, &id)
                .await
                .unwrap()
                .0
                .message(),
            "done"
        );
    }

    #[tokio::test]
    async fn restored_origins_cannot_collide_with_live_root_origins() {
        let (coordinator, _) = coordinator(policy());
        let hidden = restored_task("00", None, 1, "hidden");
        let id = hidden.id().clone();
        coordinator
            .restore_terminal_tasks([hidden], Some(1), std::iter::empty())
            .await;
        assert!(coordinator.replace_selected_origins([]).await);
        let root = coordinator.root_caller().await;

        assert!(root.origin.0 > 1);
        assert_eq!(
            coordinator.get(&root, &id).await,
            Err(AccessError::Denied(id.clone()))
        );
        assert!(coordinator.replace_selected_origins([1]).await);
        assert_eq!(
            coordinator.wait(&root, &id).await.unwrap().0.message(),
            "hidden"
        );
    }

    #[tokio::test]
    async fn hidden_terminal_parents_cannot_expose_descendants() {
        let (coordinator, mut started) = coordinator(policy());
        assert!(coordinator.replace_selected_origins([]).await);
        let mut root = coordinator.root_caller().await;
        root.origin_step_id = Some(11);
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let parent_caller = parent.child.caller().clone();
        let (child_id, child) = start(&coordinator, &mut started, &parent_caller).await;

        assert_eq!(
            coordinator
                .get(&parent_caller, &child_id)
                .await
                .unwrap()
                .state(),
            TaskState::Running
        );
        parent.finish.send(Ok(result("parent"))).unwrap();
        child.finish.send(Ok(result("child"))).unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();
        coordinator.wait(&root, &child_id).await.unwrap();
        coordinator.settle_root_run(&root, []).await;

        assert_eq!(
            coordinator.get(&parent_caller, &child_id).await,
            Err(AccessError::Denied(child_id))
        );
    }

    #[tokio::test]
    async fn operation_count_fences_visibility_replacement_after_events() {
        let gate = super::TerminalSettlementTestGate::new();
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_terminal_settlement_test_gate(gate.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator events");
        let root = coordinator.root_caller().await;
        let task = coordinator.start(&root, parameters(), ()).await.unwrap();
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Spawned { record }) if record.id() == task.id()
        ));
        let running = started.recv().await.expect("child starts");
        running.finish.send(Ok(result("done"))).unwrap();

        gate.wait_until_terminal_settled().await;
        assert_eq!(coordinator.operation_count(), 1);
        let drained = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert!(drained.iter().any(|event| matches!(
            event,
            CoordinatorEvent::Changed { id, .. } if id == task.id()
        )));
        assert!(drained.iter().any(|event| matches!(
            event,
            CoordinatorEvent::Finished { record } if record.id() == task.id()
        )));
        assert!(drained.iter().any(|event| matches!(
            event,
            CoordinatorEvent::OperationSettled { id } if id == task.id()
        )));
        assert!(!coordinator.replace_selected_origins([]).await);

        gate.allow_operation_drop();
        gate.wait_until_operation_dropped().await;
        assert_eq!(coordinator.operation_count(), 0);
        assert!(events.try_recv().is_err());
        assert!(coordinator.replace_selected_origins([]).await);
    }

    #[test]
    fn terminal_metric_claim_freezes_without_publishing_a_revision() {
        let (notifications, _) = watch::channel(0);
        let metrics = super::TaskMetrics::new(
            TaskId::parse("00").expect("valid task ID"),
            notifications,
            Arc::new(std::sync::Mutex::new(
                super::CoordinatorPublication::default(),
            )),
        );

        let claim = metrics.claim_terminal().expect("claim terminal revision");

        assert_eq!(claim.revision, 2);
        assert_eq!(metrics.snapshot().revision, 1);
        assert!(metrics.update(|_| {}).is_none());
        metrics.publish_terminal(claim.revision);
        assert_eq!(metrics.snapshot().revision, 2);
    }

    #[tokio::test]
    async fn task_admission_commit_precedes_runner_start() {
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
        let admission_gate = SessionWriteGate::new(SessionWriteKind::TaskAdmission);
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            admission_gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        let run_id = writer
            .commit_request("parent".to_owned(), selection)
            .await
            .expect("start parent run")
            .run_id;
        let origin_step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start parent model step");
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);

        let start = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            async move { coordinator.start(&caller, parameters(), ()).await }
        });
        admission_gate.wait_until_started().await;
        assert_eq!(coordinator.operation_count(), 1);
        assert!(
            started.try_recv().is_err(),
            "runner started before admission committed"
        );
        assert!(
            events.try_recv().is_err(),
            "spawn published before admission committed"
        );
        admission_gate.release();
        let admitted = start.await.expect("start task").expect("admit child");
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Spawned { .. })
        ));
        let running = started.recv().await.expect("runner starts");
        let task_id = i64::from_str_radix(admitted.id().as_str(), 16).expect("task SQL ID");
        let rows: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT COUNT(*) FROM subagent_tasks WHERE id = ?1",
                [task_id],
                |row| row.get(0),
            )
            .expect("read admitted task");

        assert_eq!(
            rows, 1,
            "runner must not start before task admission commits"
        );
        let Started { child, finish } = running;
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);
        coordinator.shutdown().await;
        drop(events);
        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn child_step_commit_precedes_next_model_call() {
        let task_step_gate = SessionWriteGate::new(SessionWriteKind::TaskStep);
        let (_home, workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(task_step_gate.clone())).await;
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open parent tool host"),
        )
        .await
        .expect("create parent arena");
        let factory = TaskStepGatedChildFactory::new();
        let trace_file = workspace.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory.clone(),
            )),
        )
        .with_session(writer.clone())
        .with_trace(trace_owner.handle());
        let _events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        tokio::select! {
            () = task_step_gate.wait_until_started() => {}
            () = factory.second_call_started.notified() => {
                panic!("next model call began before the child step committed");
            }
        }
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(factory.histories.lock().unwrap().len(), 1);
        assert_eq!(
            coordinator
                .record(child.id())
                .await
                .expect("child record")
                .executions()
                .len(),
            1
        );

        task_step_gate.release();
        factory.second_call_started.notified().await;
        factory.allow_second_call.notify_one();
        let (result, observation) = coordinator
            .wait(&caller, child.id())
            .await
            .expect("child completes");
        assert_eq!(result.message(), "child complete");
        assert_eq!(observation.state(), TaskState::Completed);
        assert_eq!(factory.calls.load(Ordering::SeqCst), 3);
        assert_eq!(factory.histories.lock().unwrap().len(), 3);

        let (state, task_revision, details_json, entries, checkpoints): (String, i64, String, i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, task_revision, details_json, (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints) FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read persisted child");
        let details: serde_json::Value =
            serde_json::from_str(&details_json).expect("decode task details");
        assert_eq!(state, "completed");
        assert!(task_revision > 1);
        assert_eq!(
            details["executions"]
                .as_array()
                .expect("stored executions")
                .iter()
                .map(|execution| execution["source"].as_str().expect("execution source"))
                .collect::<Vec<_>>(),
            ["first = 1", "second = first + 1"]
        );
        assert_eq!((entries, checkpoints), (2, 1));

        coordinator.shutdown().await;
        trace_owner.shutdown().expect("trace shutdown");
        let terminal_settlement_lock_acquisitions = fs::read_to_string(&trace_file)
            .expect("trace file")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .filter(|event| {
                event["event"] == "coordinator_lock_acquired"
                    && event["operation"] == "finish_child_settlement"
            })
            .count();
        assert_eq!(terminal_settlement_lock_acquisitions, 2);

        drop(child);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn identical_attempts_remain_distinct_in_child_step_and_terminal_snapshots() {
        let task_step_gate = SessionWriteGate::new(SessionWriteKind::TaskStep);
        let (_home, workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(task_step_gate.clone())).await;
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open parent tool host"),
        )
        .await
        .expect("create parent arena");
        let accounting = known_usage();
        let factory = TaskStepGatedChildFactory::with_attempts(vec![
            vec![accounting.clone(), accounting.clone()],
            Vec::new(),
            Vec::new(),
        ]);
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory.clone(),
            )),
        )
        .with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        task_step_gate.wait_until_started().await;
        task_step_gate.release();
        factory.second_call_started.notified().await;
        let step_details: serde_json::Value = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT details_json FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| row.get::<_, String>(0),
            )
            .expect("read child-step snapshot")
            .parse()
            .expect("decode child-step details");
        assert_eq!(step_details["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(
            step_details["attempt_evidence"].as_array().unwrap().len(),
            2
        );
        assert_eq!(
            step_details["attempt_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .map(|attempt| (
                    attempt["model_step"].as_u64().unwrap(),
                    attempt["provider_attempt"].as_u64().unwrap(),
                ))
                .collect::<Vec<_>>(),
            [(1, 1), (1, 2)]
        );
        assert_eq!(step_details["executions"].as_array().unwrap().len(), 1);

        factory.allow_second_call.notify_one();
        coordinator
            .wait(&caller, child.id())
            .await
            .expect("child completes");
        let terminal_details: serde_json::Value = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT details_json FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| row.get::<_, String>(0),
            )
            .expect("read terminal task")
            .parse()
            .expect("decode terminal details");
        assert_eq!(terminal_details["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(
            terminal_details["attempt_evidence"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let record = coordinator
            .record(child.id())
            .await
            .expect("terminal record");
        assert_eq!(record.attempts(), [accounting.clone(), accounting]);
        assert_eq!(
            record.observation().cost(),
            crate::pricing::estimate_run_cost(record.attempts())
        );

        coordinator.shutdown().await;
        drop(child);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancellation_during_task_step_commit_stops_later_child_work() {
        let task_step_gate = SessionWriteGate::new(SessionWriteKind::TaskStep);
        let (_home, workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(task_step_gate.clone())).await;
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open parent tool host"),
        )
        .await
        .expect("create parent arena");
        let factory = TaskStepGatedChildFactory::new();
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory.clone(),
            )),
        )
        .with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        task_step_gate.wait_until_started().await;
        let (phase_sender, mut phase_receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(1, phase_sender);
        let cancellation = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = child.id().clone();
            async move {
                coordinator
                    .cancel_with_phase(&caller, &id, Some(&signal))
                    .await
            }
        });
        assert_eq!(phase_receiver.recv().await, Some(1));
        let stopping = coordinator
            .get(&caller, child.id())
            .await
            .expect("read stopping task");
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("cancellation requested"));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);

        task_step_gate.release();
        assert_eq!(
            cancellation
                .await
                .expect("join cancellation")
                .expect("durably cancel child")
                .state(),
            TaskState::Cancelled
        );
        assert_cancelled(
            coordinator
                .wait(&caller, child.id())
                .await
                .expect_err("cancelled child cannot return a result"),
            child.id(),
        );
        let (state, terminal_revision, details_json): (String, Option<i64>, String) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision, details_json FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read cancelled task");
        let details: serde_json::Value =
            serde_json::from_str(&details_json).expect("decode task details");
        assert_eq!(state, "cancelled");
        assert!(terminal_revision.is_some());
        assert_eq!(details["executions"].as_array().unwrap().len(), 1);
        assert_eq!(details["executions"][0]["source"], "first = 1");

        coordinator.shutdown().await;
        drop(child);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn deadline_during_task_step_commit_stops_later_child_work() {
        let task_step_gate = SessionWriteGate::new(SessionWriteKind::TaskStep);
        let (_home, workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(task_step_gate.clone())).await;
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open parent tool host"),
        )
        .await
        .expect("create parent arena");
        let clock = Arc::new(ManualClock::new(1_000));
        let factory = TaskStepGatedChildFactory::new();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(1),
                ..policy()
            },
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory.clone(),
            )),
            clock.clone(),
        )
        .with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        task_step_gate.wait_until_started().await;
        clock.advance(Duration::from_secs(1));
        coordinator.service().await;
        let stopping = coordinator
            .get(&caller, child.id())
            .await
            .expect("read stopping task");
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("deadline expired"));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);

        task_step_gate.release();
        assert_timed_out(
            coordinator
                .wait(&caller, child.id())
                .await
                .expect_err("timed-out child cannot return a result"),
            child.id(),
        );
        let (state, terminal_revision, executions): (String, Option<i64>, String) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision, details_json FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read timed-out task");
        assert_eq!(state, "timed_out");
        assert!(terminal_revision.is_some());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&executions)
                .expect("decode task details")["executions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        coordinator.shutdown().await;
        drop(child);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn task_step_storage_failure_stops_children_without_a_terminal_result() {
        let (_home, workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(None).await;
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch(
                "CREATE TRIGGER reject_task_step BEFORE UPDATE ON subagent_tasks WHEN NEW.terminal_revision IS NULL BEGIN SELECT RAISE(ABORT, 'reject task step'); END;",
            )
            .expect("reject task steps");
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open parent tool host"),
        )
        .await
        .expect("create parent arena");
        let factory = TaskStepGatedChildFactory::new();
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory.clone(),
            )),
        )
        .with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        coordinator.storage_failure_token().cancelled().await;
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.active_count().await, 0);
        assert_eq!(
            coordinator
                .start(
                    &caller,
                    parameters(),
                    super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
                )
                .await,
            Err(AdmissionError::StorageUnavailable)
        );
        assert_eq!(
            coordinator.wait(&caller, child.id()).await,
            Err(WaitError::StorageUnavailable)
        );
        let (state, terminal_revision, task_revision, details_json):
            (String, Option<i64>, i64, String) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision, task_revision, details_json FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read last committed task");
        assert_eq!(state, "starting");
        assert_eq!(terminal_revision, None);
        assert_eq!(task_revision, 1);
        assert!(
            serde_json::from_str::<serde_json::Value>(&details_json)
                .expect("decode admitted task details")["executions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, CoordinatorEvent::Finished { .. }),
                "storage failure must not publish an uncommitted terminal result"
            );
        }
        timeout(Duration::from_secs(1), coordinator.shutdown())
            .await
            .expect("shutdown completes after task-step failure");
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, CoordinatorEvent::Finished { .. }),
                "storage failure must not publish an uncommitted terminal result"
            );
        }

        drop(events);
        drop(child);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn shutdown_waits_for_task_step_commit_without_event_acknowledgement() {
        let task_step_gate = SessionWriteGate::new(SessionWriteKind::TaskStep);
        let (_home, workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(task_step_gate.clone())).await;
        let parent = Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open parent tool host"),
        )
        .await
        .expect("create parent arena");
        let factory = TaskStepGatedChildFactory::new();
        let coordinator = SubagentCoordinator::new(
            policy(),
            Arc::new(super::CompleteChildRunner::new(
                "child system prompt".to_owned(),
                factory.clone(),
            )),
        )
        .with_session(writer.clone());
        let events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let child = coordinator
            .start(
                &caller,
                parameters(),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .expect("admit child");

        task_step_gate.wait_until_started().await;
        let mut updates = coordinator.subscribe().await;
        let shutdown = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.shutdown().await }
        });
        updates
            .changed()
            .await
            .expect("shutdown requests task cancellation");
        let stopping = coordinator
            .get(&caller, child.id())
            .await
            .expect("read stopping task");
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("session shutdown requested"));
        assert_eq!(factory.calls.load(Ordering::SeqCst), 1);

        task_step_gate.release();
        timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown completes after task-step save releases")
            .expect("join shutdown");
        let state: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(child.id().as_str(), 16).expect("task SQL ID")],
                |row| row.get(0),
            )
            .expect("read shutdown terminal");
        assert_eq!(state, "cancelled");

        drop(events);
        drop(child);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn task_terminal_commit_precedes_waiter_event_and_foreground_settlement() {
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
        let terminal_gate = SessionWriteGate::new(SessionWriteKind::TaskTerminal);
        let session = Session::create_with_write_gate(
            home.path(),
            workspace.path(),
            selection,
            arena.checkpoint(),
            terminal_gate.clone(),
        )
        .expect("create session");
        let writer = session.writer();
        let run_id = writer
            .commit_request("parent".to_owned(), selection)
            .await
            .expect("start parent run")
            .run_id;
        let origin_step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start parent model step");
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit child");
        let running = started.recv().await.expect("runner starts");
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Spawned { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Changed { .. })
        ));
        let Started { child, finish } = running;
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);
        terminal_gate.wait_until_started().await;
        assert_eq!(coordinator.operation_count(), 1);

        let (phase_sender, mut phase_receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(7, phase_sender);
        let (wait_sender, mut wait_receiver) = oneshot::channel();
        let waiter = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = admitted.id().clone();
            async move {
                wait_sender
                    .send(
                        coordinator
                            .wait_with_phase(&caller, &id, Some(&signal))
                            .await,
                    )
                    .expect("return waiter result");
            }
        });
        assert_eq!(phase_receiver.recv().await, Some(7));
        assert!(wait_receiver.try_recv().is_err());
        assert!(events.try_recv().is_err());

        let (step_settled, entry_count, checkpoint_count): (Option<i64>, i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT model_steps.settled_entry_id, (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints) FROM model_steps WHERE id = ?1",
                [i64::try_from(origin_step_id).expect("step ID fits SQL")],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read unsettled parent step");
        assert_eq!(step_settled, None);
        assert_eq!(entry_count, 2);
        assert_eq!(checkpoint_count, 1);

        terminal_gate.release();
        let (result, observation) = wait_receiver
            .await
            .expect("waiter completes")
            .expect("read committed result");
        waiter.await.expect("join waiter");
        assert_eq!(result.message(), "done");
        assert_eq!(observation.state(), TaskState::Completed);
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Finished { .. })
        ));
        let (admitted_revision, terminal_revision): (i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT admitted_revision, terminal_revision FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(admitted.id().as_str(), 16).expect("task SQL ID")],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get::<_, Option<i64>>(1)?.expect("terminal revision"),
                    ))
                },
            )
            .expect("read completed task");

        writer
            .close_run(
                run_id,
                Some(origin_step_id),
                move || crate::session::PersistedTerminal::Final {
                    step_id: origin_step_id,
                    text: "parent done".to_owned(),
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                },
                None,
            )
            .await
            .expect("settle foreground run");
        let foreground_revision: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT entries.revision FROM model_steps JOIN entries ON entries.id = model_steps.settled_entry_id WHERE model_steps.id = ?1",
                [i64::try_from(origin_step_id).expect("step ID fits SQL")],
                |row| row.get(0),
            )
            .expect("read foreground settlement revision");
        assert!(admitted_revision < terminal_revision);
        assert!(terminal_revision < foreground_revision);

        coordinator.shutdown().await;
        assert_eq!(coordinator.operation_count(), 0);
        drop(events);
        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn result_claim_wins_over_concurrent_cancellation_after_terminal_commit() {
        let terminal_gate = SessionWriteGate::new(SessionWriteKind::TaskTerminal);
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(terminal_gate.clone())).await;
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit child");
        let Started { child, finish } = started.recv().await.expect("child starts");
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Spawned { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Changed { .. })
        ));
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);
        terminal_gate.wait_until_started().await;

        let (phase_sender, mut phase_receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(8, phase_sender);
        let (cancel_sender, mut cancel_receiver) = oneshot::channel();
        let cancel = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = admitted.id().clone();
            async move {
                cancel_sender
                    .send(
                        coordinator
                            .cancel_with_phase(&caller, &id, Some(&signal))
                            .await,
                    )
                    .expect("return cancellation result");
            }
        });
        assert_eq!(phase_receiver.recv().await, Some(8));
        assert!(cancel_receiver.try_recv().is_err());
        terminal_gate.release();

        let observation = cancel_receiver
            .await
            .expect("cancellation completes")
            .expect("claimed result remains durable");
        cancel.await.expect("join cancellation");
        assert_eq!(observation.state(), TaskState::Completed);
        let (waited, waited_observation) = coordinator
            .wait(&caller, admitted.id())
            .await
            .expect("completed result remains available");
        assert_eq!(waited.message(), "done");
        assert_eq!(waited_observation.state(), TaskState::Completed);
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Finished { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::OperationSettled { ref id }) if id == admitted.id()
        ));
        assert!(events.try_recv().is_err());
        let state: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(admitted.id().as_str(), 16).expect("task SQL ID")],
                |row| row.get(0),
            )
            .expect("read terminal state");
        assert_eq!(state, "completed");

        coordinator.shutdown().await;
        drop(events);
        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancellation_claim_wins_before_runner_result_and_publishes_once() {
        let terminal_gate = SessionWriteGate::new(SessionWriteKind::TaskTerminal);
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(terminal_gate.clone())).await;
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit child");
        let Started { child, finish } = started.recv().await.expect("child starts");
        drop(child);
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Spawned { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Changed { .. })
        ));

        let (phase_sender, mut phase_receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(9, phase_sender);
        let (cancel_sender, mut cancel_receiver) = oneshot::channel();
        let cancel = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = admitted.id().clone();
            async move {
                cancel_sender
                    .send(
                        coordinator
                            .cancel_with_phase(&caller, &id, Some(&signal))
                            .await,
                    )
                    .expect("return cancellation result");
            }
        });
        assert_eq!(phase_receiver.recv().await, Some(9));
        drop(finish);
        terminal_gate.wait_until_started().await;
        assert!(cancel_receiver.try_recv().is_err());
        terminal_gate.release();

        let observation = cancel_receiver
            .await
            .expect("cancellation completes")
            .expect("cancelled result is durable");
        cancel.await.expect("join cancellation");
        assert_eq!(observation.state(), TaskState::Cancelled);
        assert_cancelled(
            coordinator.wait(&caller, admitted.id()).await.unwrap_err(),
            admitted.id(),
        );
        let mut finished = false;
        let mut operation_settled = false;
        while !finished || !operation_settled {
            match events.recv().await {
                Some(CoordinatorEvent::Changed { .. }) => {}
                Some(CoordinatorEvent::Finished { .. }) => finished = true,
                Some(CoordinatorEvent::OperationSettled { ref id }) if id == admitted.id() => {
                    operation_settled = true;
                }
                Some(CoordinatorEvent::OperationSettled { .. }) => {}
                Some(CoordinatorEvent::Spawned { .. }) => panic!("spawn is emitted exactly once"),
                None => panic!("coordinator event sender remains live"),
            }
        }
        assert!(events.try_recv().is_err());
        let state: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(admitted.id().as_str(), 16).expect("task SQL ID")],
                |row| row.get(0),
            )
            .expect("read terminal state");
        assert_eq!(state, "cancelled");

        coordinator.shutdown().await;
        drop(events);
        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn storage_failure_clears_pending_admission_as_storage_unavailable() {
        let admission_gate = SessionWriteGate::new(SessionWriteKind::TaskAdmission);
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(admission_gate.clone())).await;
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let start = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            async move { coordinator.start(&caller, parameters(), ()).await }
        });
        admission_gate.wait_until_started().await;
        coordinator.latch_storage_failure().await;
        admission_gate.release();

        assert_eq!(
            start.await.expect("join failed admission"),
            Err(AdmissionError::StorageUnavailable)
        );
        assert!(events.try_recv().is_err());
        assert!(started.try_recv().is_err());
        assert_eq!(coordinator.active_count().await, 0);
        assert!(
            !coordinator
                .inner
                .lock()
                .await
                .origin_starts
                .contains_key(&caller.origin)
        );
        let rows: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row("SELECT COUNT(*) FROM subagent_tasks", [], |row| row.get(0))
            .expect("count task rows");
        assert_eq!(rows, 0);
        coordinator.shutdown().await;

        drop(events);
        drop(started);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn cancellation_before_admission_linearisation_leaves_no_task_and_releases_budget() {
        let admission_gate = SessionWriteGate::new(SessionWriteKind::TaskAdmission);
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(admission_gate.clone())).await;
        let mut limited_policy = policy();
        limited_policy.max_starts_per_run = 1;
        let (coordinator, mut started) = coordinator(limited_policy);
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let cancellation = CancellationToken::new();
        let start = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let cancellation = cancellation.clone();
            async move {
                coordinator
                    .start_with_cancellation(&caller, parameters(), (), cancellation)
                    .await
            }
        });
        admission_gate.wait_until_started().await;
        cancellation.cancel();
        admission_gate.release();
        assert_eq!(
            start.await.expect("join cancelled start"),
            Err(AdmissionError::Cancelled)
        );
        assert!(events.try_recv().is_err());
        assert!(started.try_recv().is_err());
        assert_eq!(coordinator.active_count().await, 0);
        let rows: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row("SELECT COUNT(*) FROM subagent_tasks", [], |row| row.get(0))
            .expect("count task rows");
        assert_eq!(rows, 0);

        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit after cancelled reservation");
        let Started { child, finish } = started.recv().await.expect("child starts");
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);
        coordinator.shutdown().await;
        drop(events);
        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn persisted_tasks_capture_foreground_execution_history_anchors() {
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(None).await;
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        writer.mark_execution_started(101);

        let (id, Started { child, finish }) = start(&coordinator, &mut started, &caller).await;

        assert_eq!(
            coordinator
                .record(&id)
                .await
                .unwrap()
                .admission_history_step_id(),
            Some(101)
        );
        writer.mark_execution_started(202);
        finish.send(Ok(result("done"))).unwrap();
        coordinator.wait(&caller, &id).await.unwrap();
        let terminal = coordinator.record(&id).await.unwrap();
        assert_eq!(terminal.admission_history_step_id(), Some(101));
        assert_eq!(terminal.terminal_history_step_id(), Some(202));

        coordinator.shutdown().await;
        drop(child);
        drop(started);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn start_budget_remains_scoped_to_root_across_originating_steps() {
        let (_home, _workspace, session, writer, run_id, first_step_id) =
            persisted_origin(None).await;
        let second_step_id = {
            let connection = session
                .connection_for_test()
                .expect("lock session connection");
            connection
                .execute(
                    "INSERT INTO model_steps(run_id) VALUES (?1)",
                    [i64::try_from(run_id).expect("run ID fits SQL")],
                )
                .expect("insert second model step");
            u64::try_from(connection.last_insert_rowid()).expect("model step ID is non-negative")
        };
        let mut limited_policy = policy();
        limited_policy.max_starts_per_run = 2;
        let (coordinator, mut started) = coordinator(limited_policy);
        let coordinator = coordinator.with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(first_step_id);
        let first = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit first task");
        let Started {
            child: first_child,
            finish: first_finish,
        } = started.recv().await.expect("first child starts");
        drop(first_child);
        caller.origin_step_id = Some(second_step_id);
        let second = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit second task");
        let Started {
            child: second_child,
            finish: second_finish,
        } = started.recv().await.expect("second child starts");
        drop(second_child);
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::StartBudget)
        );

        first_finish
            .send(Ok(result("first done")))
            .expect("finish first child");
        second_finish
            .send(Ok(result("second done")))
            .expect("finish second child");
        coordinator.shutdown().await;

        drop(started);
        drop(caller);
        drop(first);
        drop(second);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn provenance_admission_errors_do_not_latch_storage_failure() {
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(None).await;
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::MissingProvenance)
        );
        caller.origin_step_id = Some(origin_step_id + 1);
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::InvalidProvenance)
        );
        assert!(!coordinator.storage_failure_token().is_cancelled());
        assert_eq!(coordinator.active_count().await, 0);
        assert!(started.try_recv().is_err());

        caller.origin_step_id = Some(origin_step_id);
        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("valid provenance remains admissible");
        let Started { child, finish } = started.recv().await.expect("child starts");
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);
        coordinator.shutdown().await;

        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn descendant_admission_keeps_closed_root_step_provenance_and_budget() {
        let (_home, _workspace, session, writer, run_id, origin_step_id) =
            persisted_origin(None).await;
        let mut limited_policy = policy();
        limited_policy.max_starts_per_run = 2;
        let (coordinator, mut started) = coordinator(limited_policy);
        let coordinator = coordinator.with_session(writer.clone());
        let mut root = coordinator.root_caller().await;
        root.origin_step_id = Some(origin_step_id);
        let parent = coordinator
            .start(&root, parameters(), ())
            .await
            .expect("admit parent child");
        let Started {
            child: parent_child,
            finish: parent_finish,
        } = started.recv().await.expect("parent starts");
        let descendant_caller = parent_child.caller().clone();
        drop(parent_child);

        writer
            .close_run(
                run_id,
                Some(origin_step_id),
                move || crate::session::PersistedTerminal::Final {
                    step_id: origin_step_id,
                    text: "root done".to_owned(),
                    reasoning_summary: String::new(),
                    replay: ProviderReplay::new("test", Vec::new()),
                    attempts: Vec::new(),
                },
                None,
            )
            .await
            .expect("close originating foreground run");
        let descendant = coordinator
            .start(&descendant_caller, parameters(), ())
            .await
            .expect("admit descendant after root closes");
        let Started {
            child: descendant_child,
            finish: descendant_finish,
        } = started.recv().await.expect("descendant starts");
        drop(descendant_child);
        assert_eq!(
            coordinator
                .start(&descendant_caller, parameters(), ())
                .await,
            Err(AdmissionError::StartBudget)
        );
        let descendant_storage_id = coordinator
            .record(descendant.id())
            .await
            .expect("descendant record")
            .storage_id();
        let parent_storage_id = coordinator
            .record(parent.id())
            .await
            .expect("parent record")
            .storage_id();
        let (parent_id, origin_id): (Option<i64>, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT parent_task_id, origin_step_id FROM subagent_tasks WHERE id = ?1",
                [i64::try_from(descendant_storage_id).expect("task SQL ID")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read descendant provenance");
        assert_eq!(
            parent_id,
            Some(i64::try_from(parent_storage_id).expect("task SQL ID"))
        );
        assert_eq!(
            origin_id,
            i64::try_from(origin_step_id).expect("step ID fits SQL")
        );

        descendant_finish
            .send(Ok(result("descendant done")))
            .expect("finish descendant");
        parent_finish
            .send(Ok(result("parent done")))
            .expect("finish parent");
        coordinator.shutdown().await;

        drop(started);
        drop(descendant_caller);
        drop(root);
        drop(parent);
        drop(descendant);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn shutdown_waits_for_pending_admission_without_draining_events() {
        let admission_gate = SessionWriteGate::new(SessionWriteKind::TaskAdmission);
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(Some(admission_gate.clone())).await;
        let (coordinator, _started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let start = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            async move { coordinator.start(&caller, parameters(), ()).await }
        });
        admission_gate.wait_until_started().await;
        let mut updates = coordinator.subscribe().await;
        let shutdown = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.shutdown().await }
        });
        updates
            .changed()
            .await
            .expect("shutdown updates coordinator");
        admission_gate.release();
        assert_eq!(
            start.await.expect("join pending start"),
            Err(AdmissionError::Cancelled)
        );
        timeout(Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown finishes after admission release")
            .expect("join shutdown");

        drop(events);
        drop(caller);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn persistent_task_ids_outside_sqlite_range_reject_without_latching_storage() {
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(None).await;
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        seed_next_task(&coordinator, i64::MAX as u64 + 1).await;

        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::IdentifierSpaceExhausted)
        );
        assert!(!coordinator.storage_failure_token().is_cancelled());
        assert_eq!(coordinator.active_count().await, 0);
        let rows: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row("SELECT COUNT(*) FROM subagent_tasks", [], |row| row.get(0))
            .expect("count task rows");
        assert_eq!(rows, 0);

        seed_next_task(&coordinator, i64::MAX as u64).await;
        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit largest SQLite task ID");
        let Started { child, finish } = started.recv().await.expect("child starts");
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);
        coordinator.shutdown().await;

        drop(started);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn terminal_storage_failure_exposes_no_uncommitted_terminal_result() {
        let (_home, _workspace, session, writer, _run_id, origin_step_id) =
            persisted_origin(None).await;
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch(
                "CREATE TRIGGER reject_task_terminal BEFORE UPDATE ON subagent_tasks WHEN NEW.terminal_revision IS NOT NULL BEGIN SELECT RAISE(ABORT, 'reject terminal'); END;",
            )
            .expect("reject task terminals");
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer.clone());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);
        let admitted = coordinator
            .start(&caller, parameters(), ())
            .await
            .expect("admit child");
        let Started { child, finish } = started.recv().await.expect("child starts");
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Spawned { .. })
        ));
        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::Changed { .. })
        ));
        let (phase_sender, mut phase_receiver) = mpsc::unbounded_channel();
        let signal = WaitPhaseSignal::new(10, phase_sender);
        let (wait_sender, wait_receiver) = oneshot::channel();
        let waiter = tokio::spawn({
            let coordinator = coordinator.clone();
            let caller = caller.clone();
            let id = admitted.id().clone();
            async move {
                wait_sender
                    .send(
                        coordinator
                            .wait_with_phase(&caller, &id, Some(&signal))
                            .await,
                    )
                    .expect("return waiter result");
            }
        });
        assert_eq!(phase_receiver.recv().await, Some(10));
        finish.send(Ok(result("done"))).expect("finish child");
        drop(child);

        assert!(matches!(
            wait_receiver.await.expect("waiter completes"),
            Err(WaitError::StorageUnavailable)
        ));
        waiter.await.expect("join waiter");
        assert!(coordinator.storage_failure_token().is_cancelled());
        assert_eq!(coordinator.active_count().await, 0);
        let record = coordinator
            .record(admitted.id())
            .await
            .expect("retain task after storage failure");
        assert!(!record.observation().state().is_terminal());
        assert_eq!(record.outcome(), None);
        assert!(events.try_recv().is_err());
        let (state, terminal_revision): (String, Option<i64>) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision FROM subagent_tasks WHERE id = ?1",
                [i64::from_str_radix(admitted.id().as_str(), 16).expect("task SQL ID")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read unfinished task row");
        assert_eq!(state, "starting");
        assert_eq!(terminal_revision, None);
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::StorageUnavailable)
        );
        timeout(Duration::from_secs(1), coordinator.shutdown())
            .await
            .expect("shutdown finishes after terminal write failure");

        drop(events);
        drop(started);
        drop(record);
        drop(caller);
        drop(admitted);
        drop(coordinator);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn admission_storage_failure_cancels_work_and_releases_capacity() {
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
        let writer = session.writer();
        let run_id = writer
            .commit_request("parent".to_owned(), selection)
            .await
            .expect("start parent run")
            .run_id;
        let origin_step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start parent model step");
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch(
                "CREATE TRIGGER reject_task_admission BEFORE INSERT ON subagent_tasks BEGIN SELECT RAISE(ABORT, 'reject task'); END;",
            )
            .expect("reject task admissions");
        let (coordinator, mut started) = coordinator(policy());
        let coordinator = coordinator.with_session(writer);
        let mut caller = coordinator.root_caller().await;
        caller.origin_step_id = Some(origin_step_id);

        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::StorageUnavailable)
        );
        assert!(coordinator.storage_failure_token().is_cancelled());
        assert!(started.try_recv().is_err());
        assert_eq!(coordinator.active_count().await, 0);
        assert_eq!(
            coordinator.start(&caller, parameters(), ()).await,
            Err(AdmissionError::StorageUnavailable)
        );
        timeout(Duration::from_secs(1), coordinator.shutdown())
            .await
            .expect("shutdown should not wait for failed storage");
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

    struct FailureCleanupStarted {
        child: ChildRun<()>,
        finish: oneshot::Sender<Result<ChildResult, ChildFailure>>,
    }

    struct FailureCleanupRunner {
        started: mpsc::UnboundedSender<FailureCleanupStarted>,
        descendant_started: Arc<Notify>,
        cleanup_started: Arc<Notify>,
        cleanup_released: Arc<Notify>,
    }

    impl ChildRunner<()> for FailureCleanupRunner {
        fn run(
            &self,
            child: ChildRun<()>,
            cancellation: tokio_util::sync::CancellationToken,
        ) -> RunnerFuture {
            if child.depth() == 1 {
                let (finish, result) = oneshot::channel();
                self.started
                    .send(FailureCleanupStarted { child, finish })
                    .expect("test receives parent start");
                return Box::pin(async move { result.await.expect("test settles parent runner") });
            }
            self.descendant_started.notify_one();
            let cleanup_started = self.cleanup_started.clone();
            let cleanup_released = self.cleanup_released.clone();
            Box::pin(async move {
                cancellation.cancelled().await;
                cleanup_started.notify_one();
                cleanup_released.notified().await;
                Err(ChildFailure::new("descendant cancelled"))
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
            assert_eq!(id.as_str(), "0");
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
    fn task_identifiers_use_dotted_base36_paths() {
        for (sequence, expected) in [
            (0, "0"),
            (1, "1"),
            (9, "9"),
            (10, "a"),
            (35, "z"),
            (36, "10"),
        ] {
            assert_eq!(task_id(None, sequence).as_str(), expected);
        }
        let parent = TaskId::parse("g").expect("valid parent ID");
        assert_eq!(task_id(Some(&parent), 0).as_str(), "g.0");
        assert_eq!(task_id(Some(&parent), 35).as_str(), "g.z");
        assert_eq!(task_id(Some(&parent), 36).as_str(), "g.10");
    }

    #[tokio::test]
    async fn child_identifiers_extend_their_parent_path() {
        let (coordinator, mut started) = coordinator(policy());
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        let parent_caller = parent.child.caller().clone();
        let (first_id, first) = start(&coordinator, &mut started, &parent_caller).await;
        let (second_id, second) = start(&coordinator, &mut started, &parent_caller).await;

        assert_eq!(parent_id.as_str(), "0");
        assert_eq!(first_id.as_str(), "0.0");
        assert_eq!(second_id.as_str(), "0.1");

        first.finish.send(Ok(result("first"))).unwrap();
        second.finish.send(Ok(result("second"))).unwrap();
        parent.finish.send(Ok(result("parent"))).unwrap();
        coordinator.wait(&root, &first_id).await.unwrap();
        coordinator.wait(&root, &second_id).await.unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();
    }

    #[test]
    fn task_identifier_parsing_is_strict_and_keeps_legacy_ids_reachable() {
        for canonical in ["0", "g", "10", "g.0", "g.10", "g.a.0", "dead"] {
            assert_eq!(TaskId::parse(canonical).unwrap().as_str(), canonical);
        }
        for legacy in ["00", "01", "0f"] {
            assert_eq!(TaskId::parse(legacy).unwrap().as_str(), legacy);
        }
        for malformed in ["", ".", ".g", "g.", "g..0", "G", "g.01", "00.0", "g-0"] {
            assert!(TaskId::parse(malformed).is_none(), "{malformed}");
        }
        let canonical = TaskId::parse("a").unwrap();
        let legacy = TaskId::parse("0a").unwrap();
        assert_ne!(canonical.cmp(&legacy), std::cmp::Ordering::Equal);
    }

    #[test]
    fn task_identifiers_sort_segments_numerically() {
        let mut identifiers = ["1000000000000000", "g.10", "g.2", "10", "z", "0", "g.0"]
            .map(|id| TaskId::parse(id).expect("valid task ID"));
        identifiers.sort();
        assert_eq!(
            identifiers.each_ref().map(ToString::to_string),
            ["0", "g.0", "g.2", "g.10", "z", "10", "1000000000000000",]
        );
        for left in &identifiers {
            for middle in &identifiers {
                for right in &identifiers {
                    if left <= middle && middle <= right {
                        assert!(left <= right);
                    }
                }
            }
        }
    }

    #[test]
    fn malformed_task_identifier_errors_describe_dotted_base36_paths() {
        assert_eq!(
            task_id_argument(
                ToolValue::List(vec![ToolValue::String("g-0".to_owned())]),
                "get_subagent",
            )
            .unwrap_err()
            .to_string(),
            "subagent task ID must be a dot-separated lowercase base-36 path"
        );
    }

    #[tokio::test]
    async fn canonical_unknown_task_identifiers_remain_distinct_from_malformed_ids() {
        assert!(TaskId::parse("g-0").is_none());
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

        assert_eq!(first_id.as_str(), "0");
        assert_eq!(second_id.as_str(), "1");
    }

    #[tokio::test]
    async fn final_task_identifier_is_allocated_once_before_exhaustion() {
        let (coordinator, mut started) = coordinator(policy());
        seed_next_task(&coordinator, u64::MAX).await;
        let caller = coordinator.root_caller().await;

        let (id, final_task) = start(&coordinator, &mut started, &caller).await;
        assert_eq!(id.as_str(), "0");
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
            ["0"]
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
    async fn failed_parent_terminal_timing_includes_descendant_teardown() {
        let clock = Arc::new(ManualClock::new(1_000));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let descendant_started = Arc::new(Notify::new());
        let cleanup_started = Arc::new(Notify::new());
        let cleanup_released = Arc::new(Notify::new());
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                max_concurrent: 2,
                ..policy()
            },
            Arc::new(FailureCleanupRunner {
                started: started_sender,
                descendant_started: descendant_started.clone(),
                cleanup_started: cleanup_started.clone(),
                cleanup_released: cleanup_released.clone(),
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let parent = coordinator.start(&root, parameters(), ()).await.unwrap();
        let parent_running = started.recv().await.expect("parent starts");
        let child = coordinator
            .start(parent_running.child.caller(), parameters(), ())
            .await
            .expect("admit descendant");
        descendant_started.notified().await;

        clock.advance(Duration::from_secs(1));
        parent_running
            .finish
            .send(Err(ChildFailure::new("parent failed")))
            .unwrap();
        cleanup_started.notified().await;
        assert_eq!(coordinator.operation_count(), 2);
        let stopping = coordinator.get(&root, parent.id()).await.unwrap();
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("parent failed"));

        clock.advance(Duration::from_secs(5));
        cleanup_released.notify_one();
        assert_failed(
            coordinator.wait(&root, parent.id()).await.unwrap_err(),
            parent.id(),
            "parent failed",
        );
        assert_cancelled(
            coordinator.wait(&root, child.id()).await.unwrap_err(),
            child.id(),
        );
        let record = coordinator.record(parent.id()).await.unwrap();
        assert_eq!(record.observation().elapsed(), Duration::from_secs(6));
        assert_eq!(record.terminal_at_ms(), Some(7_000));
        coordinator.shutdown().await;
        assert_eq!(coordinator.operation_count(), 0);
    }

    #[tokio::test]
    async fn finished_failed_record_matches_coordinator_records() {
        let (coordinator, mut started) = coordinator(policy());
        let mut events = coordinator
            .take_event_receiver()
            .expect("take coordinator event receiver");
        let root = coordinator.root_caller().await;
        let admitted = coordinator.start(&root, parameters(), ()).await.unwrap();
        let running = started.recv().await.expect("child starts");
        running
            .finish
            .send(Err(ChildFailure::new("child failed")))
            .unwrap();
        assert_failed(
            coordinator.wait(&root, admitted.id()).await.unwrap_err(),
            admitted.id(),
            "child failed",
        );

        let finished = loop {
            match events.recv().await.expect("coordinator event") {
                CoordinatorEvent::Finished { record } => break record,
                CoordinatorEvent::Spawned { .. }
                | CoordinatorEvent::Changed { .. }
                | CoordinatorEvent::OperationSettled { .. } => {}
            }
        };
        let record = coordinator.record(admitted.id()).await.unwrap();
        let observation = coordinator.get(&root, admitted.id()).await.unwrap();
        assert_eq!(finished.observation().state(), TaskState::Failed);
        assert_eq!(finished.observation().error(), Some("child failed"));
        assert_eq!(finished.observation().state(), record.observation().state());
        assert_eq!(finished.observation().error(), record.observation().error());
        assert_eq!(finished.observation().state(), observation.state());
        assert_eq!(finished.observation().error(), observation.error());
        assert_eq!(finished.outcome(), record.outcome());
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
        assert_eq!(coordinator.operation_count(), 1);
        coordinator.request_background_cancellation(None).await;
        let stopping = coordinator.get(&caller, admitted.id()).await.unwrap();
        assert_eq!(stopping.state(), TaskState::Stopping);
        assert_eq!(stopping.error(), Some("cancellation requested"));

        gate.allow_lock();
        gate.wait_until_after_lock().await;
        assert_eq!(coordinator.operation_count(), 1);
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
        let started_at = latest_representable_instant()
            .checked_sub(Duration::from_secs(2) + SUBAGENT_TIMER_HEADROOM)
            .expect("parent start must be representable");
        let inherited_deadline = started_at
            .checked_add(Duration::from_secs(2))
            .expect("parent deadline must be representable");
        assert!(
            inherited_deadline
                .checked_add(SUBAGENT_TIMER_HEADROOM)
                .is_some()
        );
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
        assert!(clock.now().checked_add(Duration::from_secs(2)).is_none());

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
    async fn timer_safe_inherited_deadline_clamps_tokio_unsafe_nested_timeout() {
        let child_started_at = instant_with_tokio_unsafe_one_second_deadline();
        let parent_started_at = child_started_at
            .checked_sub(SUBAGENT_TIMER_HEADROOM)
            .expect("earlier parent start must be representable");
        let inherited_deadline = parent_started_at
            .checked_add(Duration::from_secs(1))
            .expect("parent deadline must be representable");
        assert!(
            inherited_deadline
                .checked_add(SUBAGENT_TIMER_HEADROOM)
                .is_some()
        );
        let clock = Arc::new(ManualClock::starting_at(parent_started_at, 0));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(1),
                ..policy()
            },
            Arc::new(GatedRunner {
                started: started_sender,
            }),
            clock.clone(),
        );
        let root = coordinator.root_caller().await;
        let (parent_id, parent) = start(&coordinator, &mut started, &root).await;
        clock.advance(SUBAGENT_TIMER_HEADROOM);
        let own_deadline = clock
            .now()
            .checked_add(Duration::from_secs(1))
            .expect("raw child deadline must be representable");
        assert!(own_deadline.checked_add(SUBAGENT_TIMER_HEADROOM).is_none());

        let (child_id, child) = start(&coordinator, &mut started, parent.child.caller()).await;

        parent.finish.send(Ok(result("parent"))).unwrap();
        coordinator.wait(&root, &parent_id).await.unwrap();
        assert_eq!(coordinator.next_deadline().await, Some(inherited_deadline));
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(inherited_deadline));
        tokio::pin!(sleep);
        assert!(matches!(
            futures::poll!(sleep.as_mut()),
            std::task::Poll::Pending
        ));
        child.finish.send(Ok(result("child"))).unwrap();
        coordinator.wait(&root, &child_id).await.unwrap();
    }

    #[tokio::test]
    async fn tokio_unsafe_root_deadline_rejects_without_consuming_admission_state() {
        let clock = Arc::new(ManualClock::starting_at(
            instant_with_tokio_unsafe_one_second_deadline(),
            0,
        ));
        let (started_sender, mut started) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy {
                timeout: Duration::from_secs(1),
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
            .expect_err("Tokio-unsafe timeout must reject admission");

        assert_eq!(
            error.to_string(),
            "subagent timeout cannot be represented as a timer-safe deadline"
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
    async fn complete_runner_derives_catalog_from_child_source_in_read_only_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".agents/skills/review");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\nname: review\ndescription: Review workflow\n---\nPRIVATE_SKILL_BODY",
        )
        .unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(
            &scope,
            home.path(),
            false,
            crate::prompt::skills_catalog_guidance(),
        ));
        let host = ToolHost::new(workspace.path())
            .await
            .unwrap()
            .with_skill_catalog(catalog.clone());
        let parent = Arena::new(worker_path(), host).await.unwrap();
        let factory = ScriptedChildFactory::new([vec![ScriptedAction::Message {
            text: "done".to_owned(),
        }]]);
        let runner = super::CompleteChildRunner::new("override base".to_owned(), factory.clone());
        let coordinator = SubagentCoordinator::new(policy(), Arc::new(runner));
        let caller = coordinator.root_caller().await;
        let admitted = coordinator
            .start(
                &caller,
                ChildParameters::new(
                    "review".to_owned(),
                    ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap(),
                    true,
                ),
                super::ChildPayload::new(parent.child_source(), parent.checkpoint()),
            )
            .await
            .unwrap();
        coordinator.wait(&caller, admitted.id()).await.unwrap();
        let prompt = factory.prompts.lock().unwrap().pop().unwrap();
        assert!(prompt.starts_with("override base\n\n# Subagent work"));
        assert_eq!(prompt.matches("Review workflow").count(), 1);
        assert!(prompt.contains("cannot authorize effects beyond the user's task"));
        assert_eq!(
            prompt
                .matches("async def load_skill(name: str) -> str:")
                .count(),
            1
        );
        assert_eq!(prompt.matches("## Kiki tool reference").count(), 1);
        assert!(!prompt.contains("PRIVATE_SKILL_BODY"));
        for signature in ["async def write(", "async def edit(", "async def run("] {
            assert!(!prompt.contains(signature));
        }
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
            .execute_model_code_with_progress_and_context(
                "inherited = 41\nresult_values = {'parent': 'kept'}",
                &CancellationToken::new(),
                &mut |_| {},
                None,
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
        assert_eq!(*factory.selections.lock().unwrap(), [selection, selection]);
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
                source: "result_values = 1  # ty: ignore[invalid-assignment]".to_owned(),
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

        fn create(&self, _prompt: String) -> Self::Model {
            TracedChildModel {
                model_step_started: self.model_step_started.clone(),
            }
        }
    }

    impl Model for TracedChildModel {
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
        assert_eq!(child_events[1]["child_id"], "0");
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
                ("0", "child_execution_started", 1, None),
                ("0", "child_execution_settled", 1, Some("succeeded")),
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
                ("0", "child_execution_started", 1, None),
                ("0", "child_execution_settled", 1, Some("cancelled")),
                ("1", "child_execution_started", 1, None),
                ("1", "child_execution_settled", 1, Some("failed")),
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
                ("0", "child_execution_started", 1, None),
                ("0", "child_execution_settled", 1, Some("cancelled")),
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
                CoordinatorEvent::OperationSettled { .. } => {}
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

        assert!(matches!(
            events.recv().await,
            Some(CoordinatorEvent::OperationSettled { ref id }) if id == admitted.id()
        ));
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
                CoordinatorEvent::Changed { .. } | CoordinatorEvent::OperationSettled { .. } => {}
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
