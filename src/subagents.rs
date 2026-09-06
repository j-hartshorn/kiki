use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as SyncMutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, AgentProgress, AgentRunOutcome};
use crate::arena::{ArenaChildSource, Checkpoint};
use crate::config::{
    ModelSelection,
    constants::MAX_PROMPT_BYTES,
    models::{ModelConfiguration, model_by_id},
    settings::{SettingsContext, SubagentSettings},
};
use crate::model::{AttemptAccounting, Model};
use crate::prompt::child_prompt;
use crate::tools::{ToolError, ToolValue};

#[cfg_attr(not(test), allow(dead_code))]
const TASK_ID_WIDTH: usize = 6;
#[cfg_attr(not(test), allow(dead_code))]
const TASK_ID_SPACE: u64 = 36_u64.pow(TASK_ID_WIDTH as u32);
#[cfg_attr(not(test), allow(dead_code))]
const FAILURE_MESSAGE_LIMIT: usize = 4 * 1024;
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct TaskId(String);

#[cfg_attr(not(test), allow(dead_code))]
impl TaskId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn parse(value: &str) -> Option<Self> {
        (value.len() == TASK_ID_WIDTH
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
        .then(|| Self(value.to_owned()))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum TaskState {
    Starting,
    Running,
    Stopping,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

#[cfg_attr(not(test), allow(dead_code))]
impl TaskState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChildParameters {
    prompt: String,
    selection: ModelSelection,
    read_only: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ChildParameters {
    pub(crate) fn new(prompt: String, selection: ModelSelection, read_only: bool) -> Self {
        Self {
            prompt,
            selection,
            read_only,
        }
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChildResult {
    message: String,
    values: BTreeMap<String, ToolValue>,
}

#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChildFailure(String);

#[cfg_attr(not(test), allow(dead_code))]
impl ChildFailure {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(bounded_utf8(message.into(), FAILURE_MESSAGE_LIMIT))
    }

    pub(crate) fn message(&self) -> &str {
        &self.0
    }
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct Caller {
    session: u64,
    owner: CallerOwner,
    origin: OriginId,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
enum CallerOwner {
    Root,
    Child(TaskId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(not(test), allow(dead_code))]
struct OriginId(u64);

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChildRun<P: Send + 'static> {
    id: TaskId,
    caller: Caller,
    parent: Option<TaskId>,
    depth: usize,
    parameters: ChildParameters,
    payload: P,
    reporter: TaskReporter,
    coordinator: SubagentCoordinator<P>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<P: Send + 'static> ChildRun<P> {
    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    pub(crate) fn caller(&self) -> &Caller {
        &self.caller
    }

    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn parameters(&self) -> &ChildParameters {
        &self.parameters
    }

    pub(crate) fn payload(&self) -> &P {
        &self.payload
    }

    pub(crate) fn model_step_started(&self) {
        self.reporter.model_step_started();
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

#[cfg_attr(not(test), allow(dead_code))]
struct TaskMetrics {
    step_count: AtomicU64,
    attempts: SyncMutex<Vec<AttemptAccounting>>,
    notifications: watch::Sender<u64>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl TaskMetrics {
    fn notify(&self) {
        self.notifications
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct TaskReporter {
    metrics: Arc<TaskMetrics>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl TaskReporter {
    pub(crate) fn model_step_started(&self) {
        self.metrics.step_count.fetch_add(1, Ordering::Relaxed);
        self.metrics.notify();
    }

    pub(crate) fn set_attempts(&self, attempts: Vec<AttemptAccounting>) {
        *self.metrics.attempts.lock().expect("task metrics lock") = attempts;
        self.metrics.notify();
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) type RunnerFuture =
    Pin<Box<dyn Future<Output = Result<ChildResult, ChildFailure>> + Send + 'static>>;

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait ChildRunner<P: Send + 'static>: Send + Sync + 'static {
    fn run(&self, child: ChildRun<P>, cancellation: CancellationToken) -> RunnerFuture;
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct ChildPayload {
    source: ArenaChildSource,
    checkpoint: Checkpoint,
    accepted_thinking: Option<AcceptedThinking>,
}

impl ChildPayload {
    #[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct AgentInvocationContext {
    caller: Caller,
    coordinator: SessionSubagents,
    accepted_thinking: Option<AcceptedThinking>,
}

#[cfg_attr(not(test), allow(dead_code))]
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
#[allow(dead_code)]
pub(crate) struct TrustedExecutionContext {
    caller: Caller,
    selection: Option<ModelSelection>,
    checkpoint: Checkpoint,
    source: ArenaChildSource,
    coordinator: SessionSubagents,
    accepted_thinking: Option<AcceptedThinking>,
}

#[allow(dead_code)]
impl TrustedExecutionContext {
    pub(crate) fn caller(&self) -> &Caller {
        &self.caller
    }

    pub(crate) fn selection(&self) -> Option<ModelSelection> {
        self.selection
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
                .map_err(admission_error)?;
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
            match context.coordinator().wait(context.caller(), &id).await {
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
                .cancel(context.caller(), &id)
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
    TaskId::parse(id)
        .ok_or_else(|| ToolError::value("subagent task ID must be six base-36 characters"))
}

fn admission_error(error: AdmissionError) -> ToolError {
    match error {
        AdmissionError::InvalidCaller => ToolError::runtime(error.to_string()),
        AdmissionError::ShuttingDown
        | AdmissionError::CallerStopped
        | AdmissionError::NestedDisabled
        | AdmissionError::DepthLimit
        | AdmissionError::ConcurrentLimit
        | AdmissionError::StartBudget
        | AdmissionError::IdentifierSpaceExhausted => ToolError::runtime(error.to_string()),
    }
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
        CostStatus::Unavailable => "unavailable",
    }
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait ChildModelFactory: Send + Sync + 'static {
    type Model: Model;

    fn create(&self, prompt: String, selection: ModelSelection) -> Self::Model;
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct CompleteChildRunner<F> {
    prompt_base: String,
    models: F,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<F> CompleteChildRunner<F> {
    pub(crate) fn new(prompt_base: String, models: F) -> Self {
        Self {
            prompt_base,
            models,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<F: ChildModelFactory + Clone> ChildRunner<ChildPayload> for CompleteChildRunner<F> {
    fn run(&self, child: ChildRun<ChildPayload>, cancellation: CancellationToken) -> RunnerFuture {
        let prompt_base = self.prompt_base.clone();
        let models = self.models.clone();
        let (id, caller, _parent, _depth, parameters, payload, reporter, coordinator) =
            child.into_parts();
        let (request, selection, read_only) = parameters.into_parts();
        let accepted_thinking = payload.accepted_thinking.clone();
        let child_prompt = child_prompt(&prompt_base, payload.source.prompt_reference(read_only));
        let model = models.create(child_prompt, selection);
        Box::pin(async move {
            let arena = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(ChildFailure::new(format!(
                    "subagent {id} was cancelled before arena construction"
                ))),
                result = payload.source.child(&payload.checkpoint, read_only) => {
                    result.map_err(|error| ChildFailure::new(error.to_string()))?
                }
            };
            let mut agent = Agent::new(arena, model);
            let mut context = AgentInvocationContext::new(caller, coordinator);
            if let Some(accepted_thinking) = accepted_thinking {
                context = context.with_accepted_thinking(accepted_thinking);
            }
            agent.set_invocation_context(context);
            let mut progress = |event| {
                if matches!(event, AgentProgress::ModelStepStarted) {
                    reporter.model_step_started();
                }
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
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum CostStatus {
    Unavailable,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct TaskObservation {
    id: TaskId,
    state: TaskState,
    selection: ModelSelection,
    step_count: u64,
    elapsed: Duration,
    observed_at_ms: i64,
    estimated_cost_usd: Option<f64>,
    cost_status: CostStatus,
    error: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl TaskObservation {
    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    pub(crate) fn state(&self) -> TaskState {
        self.state
    }

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
        self.estimated_cost_usd
    }

    pub(crate) fn cost_status(&self) -> CostStatus {
        self.cost_status
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum TaskOutcome {
    Completed(ChildResult),
    Failed(ChildFailure),
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct TaskRecord {
    id: TaskId,
    parent: Option<TaskId>,
    origin: OriginId,
    depth: usize,
    parameters: ChildParameters,
    observation: TaskObservation,
    attempts: Vec<AttemptAccounting>,
    outcome: Option<TaskOutcome>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl TaskRecord {
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> &TaskId {
        &self.id
    }

    #[allow(dead_code)]
    pub(crate) fn parent(&self) -> Option<&TaskId> {
        self.parent.as_ref()
    }

    pub(crate) fn origin(&self) -> u64 {
        self.origin.0
    }

    #[allow(dead_code)]
    pub(crate) fn depth(&self) -> usize {
        self.depth
    }

    pub(crate) fn parameters(&self) -> &ChildParameters {
        &self.parameters
    }

    #[allow(dead_code)]
    pub(crate) fn observation(&self) -> &TaskObservation {
        &self.observation
    }

    pub(crate) fn attempts(&self) -> &[AttemptAccounting] {
        &self.attempts
    }

    pub(crate) fn outcome(&self) -> Option<&TaskOutcome> {
        self.outcome.as_ref()
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SubagentPolicy {
    allow_nested: bool,
    max_concurrent: usize,
    max_starts_per_run: usize,
    max_depth: usize,
    timeout: Duration,
}

#[cfg_attr(not(test), allow(dead_code))]
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
#[cfg_attr(not(test), allow(dead_code))]
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
    #[error("subagent task identifier space is exhausted")]
    IdentifierSpaceExhausted,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum AccessError {
    #[error("subagent caller is not trusted by this session")]
    InvalidCaller,
    #[error("unknown subagent `{0}`")]
    UnknownTask(TaskId),
    #[error("subagent caller is not authorised for `{0}`")]
    Denied(TaskId),
}

#[derive(Debug, thiserror::Error, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    fn unix_time_ms(&self) -> i64;
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SystemClock;

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct SubagentCoordinator<P: Send + 'static> {
    inner: Arc<Mutex<Inner>>,
    policy: SubagentPolicy,
    runner: Arc<dyn ChildRunner<P>>,
    clock: Arc<dyn Clock>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<P: Send + 'static> Clone for SubagentCoordinator<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            policy: self.policy,
            runner: self.runner.clone(),
            clock: self.clock.clone(),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
struct Inner {
    session: u64,
    next_origin: u64,
    next_task: u64,
    active_slots: usize,
    shutting_down: bool,
    blocked_origins: BTreeSet<OriginId>,
    origin_starts: BTreeMap<OriginId, usize>,
    notifications: watch::Sender<u64>,
    tasks: BTreeMap<TaskId, Task>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl Inner {
    fn notify(&self) {
        self.notifications
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

#[cfg_attr(not(test), allow(dead_code))]
struct Task {
    id: TaskId,
    parent: Option<TaskId>,
    origin: OriginId,
    depth: usize,
    parameters: ChildParameters,
    admitted_at: Instant,
    deadline: Option<Instant>,
    settled_at: Option<Instant>,
    metrics: Arc<TaskMetrics>,
    state: TaskState,
    stop: Option<StopReason>,
    outcome: Option<TaskOutcome>,
    cancellation: CancellationToken,
    updates: watch::Sender<TaskState>,
}

#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
enum StopReason {
    Cancelled,
    TimedOut,
    Shutdown,
    AncestorStopped,
    RunnerFailed(ChildFailure),
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
impl Task {
    fn observation(&self, now: Instant, observed_at_ms: i64) -> TaskObservation {
        let finished_at = self.settled_at.unwrap_or(now);
        TaskObservation {
            id: self.id.clone(),
            state: self.state,
            selection: self.parameters.selection(),
            step_count: self.metrics.step_count.load(Ordering::Relaxed),
            elapsed: finished_at.saturating_duration_since(self.admitted_at),
            observed_at_ms,
            estimated_cost_usd: None,
            cost_status: CostStatus::Unavailable,
            error: match (&self.state, &self.stop, &self.outcome) {
                (TaskState::Stopping, Some(reason), _) => Some(reason.message()),
                (_, _, Some(TaskOutcome::Failed(failure))) => Some(failure.message().to_owned()),
                _ => None,
            },
        }
    }

    fn publish(&mut self) {
        self.updates.send_replace(self.state);
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<P: Send + 'static> SubagentCoordinator<P> {
    pub(crate) fn new(policy: SubagentPolicy, runner: Arc<dyn ChildRunner<P>>) -> Self {
        Self::with_clock(policy, runner, Arc::new(SystemClock))
    }

    pub(crate) fn with_clock(
        policy: SubagentPolicy,
        runner: Arc<dyn ChildRunner<P>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                session: NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
                next_origin: 1,
                next_task: 0,
                active_slots: 0,
                shutting_down: false,
                blocked_origins: BTreeSet::new(),
                origin_starts: BTreeMap::new(),
                notifications: watch::channel(0).0,
                tasks: BTreeMap::new(),
            })),
            policy,
            runner,
            clock,
        }
    }

    pub(crate) async fn root_caller(&self) -> Caller {
        let mut inner = self.inner.lock().await;
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
        let (child, observation) = {
            let mut inner = self.inner.lock().await;
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
                    if task.state.is_terminal()
                        || task.stop.is_some()
                        || task.deadline.is_some_and(|deadline| deadline <= now)
                    {
                        return Err(AdmissionError::CallerStopped);
                    }
                    if !self.policy.allow_nested {
                        return Err(AdmissionError::NestedDisabled);
                    }
                    (
                        Some(parent.clone()),
                        task.depth.saturating_add(1),
                        task.parameters.read_only(),
                        task.deadline,
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
            let id = task_id(inner.next_task).ok_or(AdmissionError::IdentifierSpaceExhausted)?;
            let own_deadline = now.checked_add(self.policy.timeout);
            let deadline = earliest_deadline(own_deadline, inherited_deadline);
            parameters.read_only |= inherited_read_only;
            let (updates, _) = watch::channel(TaskState::Starting);
            let cancellation = CancellationToken::new();
            let metrics = Arc::new(TaskMetrics {
                step_count: AtomicU64::new(0),
                attempts: SyncMutex::new(Vec::new()),
                notifications: inner.notifications.clone(),
            });
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
                updates,
            };
            let observation = task.observation(now, observed_at_ms);
            inner.next_task += 1;
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
                },
                observation,
            )
        };

        let coordinator = self.clone();
        tokio::spawn(async move {
            coordinator.run_child(child).await;
        });
        Ok(observation)
    }

    pub(crate) async fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.lock().await.notifications.subscribe()
    }

    pub(crate) async fn get(
        &self,
        caller: &Caller,
        id: &TaskId,
    ) -> Result<TaskObservation, AccessError> {
        let inner = self.inner.lock().await;
        ensure_access(&inner, caller, id)?;
        let task = inner
            .tasks
            .get(id)
            .expect("authorised task must be retained");
        Ok(task.observation(self.now(), self.unix_time_ms()))
    }

    pub(crate) async fn wait(
        &self,
        caller: &Caller,
        id: &TaskId,
    ) -> Result<(ChildResult, TaskObservation), WaitError> {
        let updates = {
            let inner = self.inner.lock().await;
            ensure_access(&inner, caller, id)?;
            inner
                .tasks
                .get(id)
                .expect("authorised task must be retained")
                .updates
                .subscribe()
        };
        self.wait_for_outcome(id, updates).await
    }

    pub(crate) async fn cancel(
        &self,
        caller: &Caller,
        id: &TaskId,
    ) -> Result<TaskObservation, AccessError> {
        {
            let inner = self.inner.lock().await;
            ensure_access(&inner, caller, id)?;
        }
        let stopping = self.request_stop_subtree(id, StopReason::Cancelled).await;
        self.wait_for_all(stopping).await;
        self.get(caller, id).await
    }

    pub(crate) async fn request_background_cancellation(&self, active: Option<&Caller>) {
        self.request_stop_all(StopReason::Cancelled, active).await;
    }

    pub(crate) async fn join_active(&self) {
        let active = self
            .inner
            .lock()
            .await
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal())
            .map(|task| task.id.clone())
            .collect();
        self.wait_for_all(active).await;
    }

    pub(crate) async fn cancel_all(&self) {
        self.request_background_cancellation(None).await;
        self.join_active().await;
    }

    pub(crate) async fn service(&self) {
        let now = self.now();
        let mut expired = {
            let inner = self.inner.lock().await;
            inner
                .tasks
                .values()
                .filter(|task| {
                    !task.state.is_terminal()
                        && task.stop.is_none()
                        && task.deadline.is_some_and(|deadline| deadline <= now)
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
        self.inner
            .lock()
            .await
            .tasks
            .values()
            .filter(|task| !task.state.is_terminal() && task.stop.is_none())
            .filter_map(|task| task.deadline)
            .min()
    }

    pub(crate) async fn shutdown(&self) {
        {
            let mut inner = self.inner.lock().await;
            inner.shutting_down = true;
            inner.notify();
        }
        self.request_stop_all(StopReason::Shutdown, None).await;
        self.join_active().await;
    }

    pub(crate) async fn records(&self) -> Vec<TaskRecord> {
        let inner = self.inner.lock().await;
        let now = self.now();
        let observed_at_ms = self.unix_time_ms();
        inner
            .tasks
            .values()
            .map(|task| TaskRecord {
                id: task.id.clone(),
                parent: task.parent.clone(),
                origin: task.origin,
                depth: task.depth,
                parameters: task.parameters.clone(),
                observation: task.observation(now, observed_at_ms),
                attempts: task
                    .metrics
                    .attempts
                    .lock()
                    .expect("task metrics lock")
                    .clone(),
                outcome: task.outcome.clone(),
            })
            .collect()
    }

    pub(crate) async fn active_count(&self) -> usize {
        self.inner.lock().await.active_slots
    }

    async fn run_child(&self, child: ChildRun<P>) {
        let id = child.id().clone();
        let cancellation = {
            let mut inner = self.inner.lock().await;
            let (changed, cancellation) = {
                let task = inner
                    .tasks
                    .get_mut(&id)
                    .expect("admitted child must remain retained");
                let changed = task.state == TaskState::Starting;
                if changed {
                    task.state = TaskState::Running;
                    task.publish();
                }
                (changed, task.cancellation.clone())
            };
            if changed {
                inner.notify();
            }
            cancellation
        };
        let result = if cancellation.is_cancelled() {
            Err(ChildFailure::new("child was cancelled before startup"))
        } else {
            let runner = self.runner.clone();
            AssertUnwindSafe(async move { runner.run(child, cancellation).await })
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(ChildFailure::new("child runner panicked")))
        };
        self.finish_child(&id, result).await;
    }

    async fn finish_child(&self, id: &TaskId, result: Result<ChildResult, ChildFailure>) {
        let now = self.now();
        let outcome = {
            let mut inner = self.inner.lock().await;
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
                        settle_task(&mut inner, id, TaskOutcome::Completed(result), now);
                        return;
                    }
                    Err(failure) => {
                        task.stop = Some(StopReason::RunnerFailed(failure.clone()));
                        task.state = TaskState::Stopping;
                        task.publish();
                        inner.notify();
                        TaskOutcome::Failed(failure)
                    }
                }
            }
        };
        let descendants = self
            .request_stop_descendants(id, StopReason::AncestorStopped)
            .await;
        self.wait_for_all(descendants).await;
        let mut inner = self.inner.lock().await;
        settle_task(&mut inner, id, outcome, self.now());
    }

    async fn request_stop_subtree(&self, id: &TaskId, reason: StopReason) -> Vec<TaskId> {
        let mut inner = self.inner.lock().await;
        let ids = inner
            .tasks
            .keys()
            .filter(|candidate| is_in_subtree(&inner.tasks, candidate, id))
            .cloned()
            .collect::<Vec<_>>();
        stop_tasks(&mut inner, &ids, reason)
    }

    async fn request_stop_descendants(&self, id: &TaskId, reason: StopReason) -> Vec<TaskId> {
        let mut inner = self.inner.lock().await;
        let ids = inner
            .tasks
            .keys()
            .filter(|candidate| *candidate != id && is_in_subtree(&inner.tasks, candidate, id))
            .cloned()
            .collect::<Vec<_>>();
        stop_tasks(&mut inner, &ids, reason)
    }

    async fn request_timeout(&self, id: &TaskId) -> Vec<TaskId> {
        let mut inner = self.inner.lock().await;
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
        let mut stopping = stop_tasks(&mut inner, std::slice::from_ref(id), StopReason::TimedOut);
        stopping.extend(stop_tasks(
            &mut inner,
            &descendants,
            StopReason::AncestorStopped,
        ));
        stopping
    }

    async fn request_stop_all(&self, reason: StopReason, active: Option<&Caller>) {
        let mut inner = self.inner.lock().await;
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
        stop_tasks(&mut inner, &ids, reason);
        inner.notify();
    }

    async fn wait_for_all(&self, ids: Vec<TaskId>) {
        for id in ids {
            self.wait_for_terminal(&id).await;
        }
    }

    async fn wait_for_terminal(&self, id: &TaskId) -> TaskOutcome {
        let updates = {
            let inner = self.inner.lock().await;
            inner
                .tasks
                .get(id)
                .expect("retained task must exist")
                .updates
                .subscribe()
        };
        let mut updates = updates;
        loop {
            if updates.borrow().is_terminal() {
                return self
                    .inner
                    .lock()
                    .await
                    .tasks
                    .get(id)
                    .expect("retained terminal task must exist")
                    .outcome
                    .clone()
                    .expect("terminal task must have an outcome");
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
        mut updates: watch::Receiver<TaskState>,
    ) -> Result<(ChildResult, TaskObservation), WaitError> {
        loop {
            if updates.borrow().is_terminal() {
                let inner = self.inner.lock().await;
                let task = inner
                    .tasks
                    .get(id)
                    .expect("retained terminal task must exist");
                let outcome = task
                    .outcome
                    .clone()
                    .expect("terminal task must have an outcome");
                let observation = task.observation(self.now(), self.unix_time_ms());
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

#[cfg_attr(not(test), allow(dead_code))]
fn earliest_deadline(own: Option<Instant>, inherited: Option<Instant>) -> Option<Instant> {
    match (own, inherited) {
        (Some(own), Some(inherited)) => Some(own.min(inherited)),
        (Some(deadline), None) | (None, Some(deadline)) => Some(deadline),
        (None, None) => None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
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

#[cfg_attr(not(test), allow(dead_code))]
fn is_in_subtree(tasks: &BTreeMap<TaskId, Task>, candidate: &TaskId, ancestor: &TaskId) -> bool {
    candidate == ancestor || is_proper_descendant(tasks, candidate, ancestor)
}

#[cfg_attr(not(test), allow(dead_code))]
fn stop_tasks(inner: &mut Inner, ids: &[TaskId], reason: StopReason) -> Vec<TaskId> {
    let mut stopping = Vec::new();
    for id in ids {
        let Some(task) = inner.tasks.get_mut(id) else {
            continue;
        };
        if task.state.is_terminal() {
            continue;
        }
        if task.stop.is_none() {
            task.stop = Some(reason.clone());
            task.state = TaskState::Stopping;
            task.cancellation.cancel();
            task.publish();
            inner.notify();
        }
        stopping.push(id.clone());
    }
    stopping
}

#[cfg_attr(not(test), allow(dead_code))]
fn settle_task(inner: &mut Inner, id: &TaskId, outcome: TaskOutcome, now: Instant) {
    let settled = {
        let task = inner
            .tasks
            .get_mut(id)
            .expect("admitted child must remain retained");
        if task.state.is_terminal() {
            false
        } else {
            task.state = match outcome {
                TaskOutcome::Completed(_) => TaskState::Completed,
                TaskOutcome::Failed(_) => TaskState::Failed,
                TaskOutcome::Cancelled => TaskState::Cancelled,
                TaskOutcome::TimedOut => TaskState::TimedOut,
            };
            task.outcome = Some(outcome);
            task.settled_at = Some(now);
            task.publish();
            inner.notify();
            true
        }
    };
    if settled {
        inner.active_slots = inner
            .active_slots
            .checked_sub(1)
            .expect("every settled task owns exactly one active slot");
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn task_id(mut sequence: u64) -> Option<TaskId> {
    if sequence >= TASK_ID_SPACE {
        return None;
    }
    let mut bytes = [b'0'; TASK_ID_WIDTH];
    for byte in bytes.iter_mut().rev() {
        let digit = (sequence % 36) as u8;
        *byte = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        sequence /= 36;
    }
    Some(TaskId(
        String::from_utf8(bytes.to_vec()).expect("base-36 task identifiers are UTF-8"),
    ))
}

#[cfg_attr(not(test), allow(dead_code))]
fn bounded_utf8(mut value: String, limit: usize) -> String {
    value.truncate(utf8_prefix_len(&value, limit));
    value
}

#[cfg_attr(not(test), allow(dead_code))]
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
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio::sync::{mpsc, oneshot};
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;

    use super::{
        AccessError, AdmissionError, ChildFailure, ChildParameters, ChildResult, ChildRun,
        ChildRunner, Clock, CostStatus, FAILURE_MESSAGE_LIMIT, RunnerFuture, SubagentCoordinator,
        SubagentPolicy, TaskId, TaskOutcome, TaskState, WaitError,
    };
    use crate::arena::{Arena, ChildConstructionTestGate, CollectionTestGate, ExecutionOutcome};
    use crate::config::{DEFAULT_MODEL, ModelSelection};
    use crate::model::{
        Model, ModelContextEntry, ModelFailure, ModelHistory, ModelProgress, ModelStep,
        ScriptedAction, ScriptedModel,
    };
    use crate::tools::{ToolHost, ToolValue};

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the monty-runtime 0.0.21 worker")
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
            Self(Arc::new(Mutex::new((Instant::now(), observed_at_ms))))
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
            assert_eq!(id.as_str().len(), 6);
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

    #[tokio::test]
    async fn task_identifiers_are_six_base36_characters_and_never_reused() {
        let (coordinator, mut started) = coordinator(policy());
        let caller = coordinator.root_caller().await;
        let (first_id, first) = start(&coordinator, &mut started, &caller).await;
        first.finish.send(Ok(result("first"))).unwrap();
        coordinator.wait(&caller, &first_id).await.unwrap();

        let (second_id, second) = start(&coordinator, &mut started, &caller).await;
        second.finish.send(Ok(result("second"))).unwrap();
        coordinator.wait(&caller, &second_id).await.unwrap();

        assert!(
            first_id
                .as_str()
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        );
        assert_eq!(first_id.as_str().len(), 6);
        assert_ne!(first_id, second_id);
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
        let guessed = TaskId("zzzzzz".to_owned());
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
        let id = TaskId("000000".to_owned());

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
    async fn huge_timeouts_admit_and_do_not_have_a_representable_deadline() {
        let (coordinator, mut started) = coordinator(SubagentPolicy {
            timeout: Duration::from_secs(i64::MAX as u64),
            ..policy()
        });
        let root = coordinator.root_caller().await;
        let (id, running) = start(&coordinator, &mut started, &root).await;

        assert_eq!(coordinator.next_deadline().await, None);
        coordinator.cancel(&root, &id).await.unwrap();
        assert_eq!(coordinator.active_count().await, 0);
        drop(running);
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
        assert_eq!(coordinator.records().await[0].attempts(), &[]);
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

    #[test]
    fn failures_are_utf8_bounded() {
        let failure = ChildFailure::new("é".repeat(FAILURE_MESSAGE_LIMIT));
        assert_eq!(failure.message().len(), FAILURE_MESSAGE_LIMIT);
        assert!(failure.message().is_char_boundary(failure.message().len()));
    }
}
