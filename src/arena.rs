use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, stream::FuturesUnordered};
use monty_pool::{
    Checkout, Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync,
};
use monty_types::{ExcType, MontyException, MontyObject, PrintStream, ResourceLimits};
use thiserror::Error;
use tokio::process::Command;
#[cfg(test)]
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{
        ModelSelection,
        constants::{
            ARENA_EXECUTION_TIMEOUT, ARENA_MAX_MEMORY_BYTES, COLLECTION_MAX_ITEMS,
            COLLECTION_MAX_PAYLOAD_BYTES,
        },
    },
    subagents::{
        AgentInvocationContext, InvocationOperation, TrustedExecutionContext, WaitPhaseSignal,
        invoke as invoke_subagent,
    },
    tools::{
        ToolCall as HostToolCall, ToolError, ToolErrorKind, ToolHost, ToolOutputChunk, ToolResult,
        ToolValue, WholeFileAuthorisations, bootstrap_source,
    },
};

pub(crate) use crate::tools::ToolName;

const RUNTIME_IDENTITY: &str = "monty-runtime 0.0.22";
const WORKER_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_BYTES: usize = 32 * 1024;
const INTERRUPTED_EVIDENCE_TEXT_LIMIT: usize = 4 * 1024;
const RESULT_VALUES_ERROR_BYTES: usize = 4 * 1024;
const RESULT_VALUES_BINDING: &str = "result_values";
const STDOUT_DELTA_BYTES: usize = 4 * 1024;
const STDOUT_RATE_WINDOW: Duration = Duration::from_millis(50);
const RUN_COMMAND_PREVIEW_BYTES: usize = 160;
const RUN_STDOUT_PREVIEW_BYTES: usize = 3 * 1024;
const RUN_STDOUT_PREVIEW_LINES: usize = 8;
const RUN_OUTPUT_CHANNEL_CAPACITY: usize = 256;
const OBSERVATION_LIMITS: ObservationLimits = ObservationLimits {
    max_total_bytes: 256 * 1024,
    max_total_lines: 1_000,
    max_head_bytes: 192 * 1024,
    max_head_lines: 800,
    max_tail_bytes: 64 * 1024,
    max_tail_lines: 200,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity(String);

impl RuntimeIdentity {
    pub(crate) fn session_value(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_session_value(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    identity: RuntimeIdentity,
    state: Vec<u8>,
    authorisations: WholeFileAuthorisations,
}

impl Checkpoint {
    pub(crate) fn session_parts(&self) -> (&RuntimeIdentity, &[u8], &WholeFileAuthorisations) {
        (&self.identity, &self.state, &self.authorisations)
    }

    pub(crate) fn from_session_parts(
        identity: RuntimeIdentity,
        state: Vec<u8>,
        authorisations: WholeFileAuthorisations,
    ) -> Self {
        Self {
            identity,
            state,
            authorisations,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub(crate) head: String,
    pub(crate) tail: Option<String>,
    pub(crate) head_lines: usize,
    pub(crate) tail_lines: usize,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
    pub(crate) omitted_lines: usize,
    pub(crate) omitted_bytes: usize,
    pub(crate) head_ends_mid_line: bool,
    pub(crate) tail_starts_mid_line: bool,
    pub(crate) partial_lines: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonError {
    pub(crate) text: String,
    pub(crate) omitted_bytes: usize,
}

impl PythonError {
    fn from_text(mut text: String) -> Self {
        let retained = utf8_prefix_len(&text, ERROR_BYTES);
        let omitted_bytes = text.len() - retained;
        text.truncate(retained);
        Self {
            text,
            omitted_bytes,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Execution {
    pub(crate) stdout: Observation,
    pub(crate) stderr: Observation,
    pub(crate) outcome: ExecutionOutcome,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) modified_paths: Vec<PathBuf>,
}

impl fmt::Debug for Execution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Execution")
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("outcome", &self.outcome)
            .field("tool_calls", &self.tool_calls)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Complete { result: Option<Observation> },
    PythonError(PythonError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArenaProgress {
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunPreview {
    pub(crate) command: String,
    pub(crate) stdout: String,
}

struct RunPreviewState {
    command: String,
    stdout: Vec<u8>,
}

impl RunPreviewState {
    fn new(command: &str) -> Self {
        let first_line = command
            .split(['\r', '\n'])
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let truncated =
            first_line.len() < command.len() || first_line.len() > RUN_COMMAND_PREVIEW_BYTES;
        let mut command = first_line.to_owned();
        if command.len() > RUN_COMMAND_PREVIEW_BYTES {
            command.truncate(utf8_prefix_len(&command, RUN_COMMAND_PREVIEW_BYTES));
        }
        if truncated {
            command.push('…');
        }
        Self {
            command,
            stdout: Vec::new(),
        }
    }

    fn push_stdout(&mut self, stdout: &[u8]) {
        self.stdout.extend_from_slice(stdout);
        let logical_lines = self.stdout.iter().filter(|byte| **byte == b'\n').count()
            + usize::from(!self.stdout.ends_with(b"\n"));
        let excess_lines = logical_lines.saturating_sub(RUN_STDOUT_PREVIEW_LINES);
        if excess_lines > 0 {
            let mut seen = 0;
            if let Some(end) = self.stdout.iter().position(|byte| {
                if *byte == b'\n' {
                    seen += 1;
                }
                seen == excess_lines
            }) {
                self.stdout.drain(..=end);
            }
        }
        if self.stdout.len() > RUN_STDOUT_PREVIEW_BYTES {
            let start = self.stdout.len() - RUN_STDOUT_PREVIEW_BYTES;
            self.stdout.drain(..start);
        }
    }

    fn snapshot(&self) -> RunPreview {
        let mut stdout = String::from_utf8_lossy(&self.stdout).into_owned();
        if stdout.len() > RUN_STDOUT_PREVIEW_BYTES {
            let mut start = stdout.len() - RUN_STDOUT_PREVIEW_BYTES;
            while !stdout.is_char_boundary(start) {
                start += 1;
            }
            stdout.drain(..start);
        }
        RunPreview {
            command: self.command.clone(),
            stdout,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolCallSummary {
    Pending,
    Failed,
    Exit(i64),
    Signal,
    Count(usize),
    Created,
    Replaced,
    Applied,
    Subagent(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCall {
    pub(crate) tool: ToolName,
    pub(crate) summary: ToolCallSummary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ToolCallToken(usize);

#[cfg(test)]
impl ToolCallToken {
    pub(crate) fn for_test(value: usize) -> Self {
        Self(value)
    }
}

#[derive(Debug, Default)]
struct ToolTraceState {
    calls: Vec<ToolCall>,
    read_paths: BTreeSet<PathBuf>,
    modified_paths: BTreeSet<PathBuf>,
}

impl ToolTraceState {
    fn admit(&mut self, tool: ToolName) -> ToolCallToken {
        let token = ToolCallToken(self.calls.len());
        self.calls.push(ToolCall {
            tool,
            summary: ToolCallSummary::Pending,
        });
        token
    }

    fn settle(&mut self, token: ToolCallToken, summary: ToolCallSummary) -> ToolCall {
        assert!(
            !matches!(summary, ToolCallSummary::Pending),
            "a tool call must settle to a terminal summary"
        );
        let call = self
            .calls
            .get_mut(token.0)
            .expect("tool call token must identify an admitted row");
        assert!(
            matches!(call.summary, ToolCallSummary::Pending),
            "a tool call must settle once"
        );
        call.summary = summary;
        call.clone()
    }

    fn call(&self, token: ToolCallToken) -> ToolCall {
        self.calls
            .get(token.0)
            .expect("tool call token must identify an admitted row")
            .clone()
    }

    fn record_observed_file_operation(&mut self, result: &ToolResult) {
        result.record_observed_file_operation(&mut self.read_paths, &mut self.modified_paths);
    }

    #[cfg(test)]
    fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    fn into_parts(self) -> (Vec<ToolCall>, Vec<PathBuf>, Vec<PathBuf>) {
        (
            self.calls,
            self.read_paths.into_iter().collect(),
            self.modified_paths.into_iter().collect(),
        )
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct InterruptedExecution {
    pub(crate) began: bool,
    pub(crate) stdout: Observation,
    pub(crate) stderr: Observation,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) read_paths: Vec<PathBuf>,
    pub(crate) modified_paths: Vec<PathBuf>,
}

impl fmt::Debug for InterruptedExecution {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InterruptedExecution")
            .field("began", &self.began)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .field("tool_calls", &self.tool_calls)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ArenaError {
    #[error("Monty worker path must be absolute")]
    WorkerPathNotAbsolute,
    #[error("Monty worker version check timed out")]
    WorkerVersionTimedOut,
    #[error("could not execute Monty worker: {message}")]
    WorkerVersionExecution { message: String },
    #[error("Monty worker exited unsuccessfully: {status}")]
    WorkerVersionFailed { status: String },
    #[error("Monty worker version output was not UTF-8")]
    WorkerVersionInvalidOutput,
    #[error("Monty worker version mismatch: expected {expected}, got {actual}")]
    WorkerVersionMismatch { expected: String, actual: String },
    #[error("arena execution was cancelled")]
    Cancelled {
        execution: Box<InterruptedExecution>,
    },
    #[error("arena execution timed out")]
    TimedOut {
        execution: Box<InterruptedExecution>,
    },
    #[error("arena infrastructure failed: {message}")]
    Infrastructure {
        message: String,
        execution: Option<Box<InterruptedExecution>>,
    },
    #[error("arena recovery failed after {original}: {recovery}")]
    RecoveryFailed {
        original: Box<ArenaError>,
        recovery: Box<ArenaError>,
    },
    #[error("checkpoint runtime identity is incompatible: expected {expected}, got {actual}")]
    IncompatibleCheckpoint { expected: String, actual: String },
    #[error("Monty checkpoint was suspended")]
    SuspendedCheckpoint,
    #[error("unsupported Monty suspension: {message}")]
    UnsupportedSuspension {
        message: String,
        execution: Option<Box<InterruptedExecution>>,
    },
}

impl ArenaError {
    fn unsupported_suspension(message: impl Into<String>) -> Self {
        Self::UnsupportedSuspension {
            message: bounded_interruption_text(message.into()),
            execution: None,
        }
    }

    fn with_interrupted_execution(self, execution: InterruptedExecution) -> Self {
        match self {
            Self::Cancelled { .. } => Self::Cancelled {
                execution: Box::new(execution),
            },
            Self::TimedOut { .. } => Self::TimedOut {
                execution: Box::new(execution),
            },
            Self::Infrastructure { message, .. } => Self::Infrastructure {
                message,
                execution: Some(Box::new(execution)),
            },
            Self::UnsupportedSuspension { message, .. } => Self::UnsupportedSuspension {
                message,
                execution: Some(Box::new(execution)),
            },
            error => error,
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ResultValuesError {
    #[error("result_values collection was cancelled")]
    Cancelled,
    #[error("result_values export failed: {message}")]
    Export { message: String },
}

impl ResultValuesError {
    fn export(message: impl Into<String>) -> Self {
        let mut message = message.into();
        message.truncate(utf8_prefix_len(&message, RESULT_VALUES_ERROR_BYTES));
        Self::Export { message }
    }
}

struct InterruptedExecutionState {
    streams: StreamAccumulators,
    tools: ToolTraceState,
    began: bool,
}

impl InterruptedExecutionState {
    fn new() -> Self {
        Self {
            streams: StreamAccumulators::new(OBSERVATION_LIMITS),
            tools: ToolTraceState::default(),
            began: false,
        }
    }

    fn finish(
        self,
        now: Instant,
        progress: &mut impl FnMut(ArenaProgress),
    ) -> InterruptedExecution {
        let InterruptedExecutionState {
            mut streams,
            tools,
            began,
        } = self;
        streams.flush_stdout(now, progress);
        let (tool_calls, read_paths, modified_paths) = tools.into_parts();
        InterruptedExecution {
            began,
            stdout: streams.stdout.finish(),
            stderr: streams.stderr.finish(),
            tool_calls,
            read_paths,
            modified_paths,
        }
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct RecoveryTestGate {
    recovery_started: Arc<Semaphore>,
    recovery_allowed: Arc<Semaphore>,
}

#[cfg(test)]
impl RecoveryTestGate {
    pub(crate) fn new() -> Self {
        Self {
            recovery_started: Arc::new(Semaphore::new(0)),
            recovery_allowed: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_recovery_started(&self) {
        take_test_permit(&self.recovery_started).await;
    }

    pub(crate) fn resume_recovery(&self) {
        self.recovery_allowed.add_permits(1);
    }

    async fn wait_before_failure(&self) {
        self.recovery_started.add_permits(1);
        take_test_permit(&self.recovery_allowed).await;
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ChildConstructionTestGate {
    construction_started: Arc<Semaphore>,
    construction_allowed: Arc<Semaphore>,
}

#[cfg(test)]
impl ChildConstructionTestGate {
    pub(crate) fn new() -> Self {
        Self {
            construction_started: Arc::new(Semaphore::new(0)),
            construction_allowed: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_construction_started(&self) {
        take_test_permit(&self.construction_started).await;
    }

    pub(crate) fn resume_construction(&self) {
        self.construction_allowed.add_permits(1);
    }

    async fn wait_before_construction(&self) {
        self.construction_started.add_permits(1);
        take_test_permit(&self.construction_allowed).await;
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct CollectionTestGate {
    collection_started: Arc<Semaphore>,
    collection_allowed: Arc<Semaphore>,
}

#[cfg(test)]
impl CollectionTestGate {
    pub(crate) fn new() -> Self {
        Self {
            collection_started: Arc::new(Semaphore::new(0)),
            collection_allowed: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_collection_started(&self) {
        take_test_permit(&self.collection_started).await;
    }

    pub(crate) fn resume_collection(&self) {
        self.collection_allowed.add_permits(1);
    }

    async fn wait_before_collection(&self) {
        self.collection_started.add_permits(1);
        take_test_permit(&self.collection_allowed).await;
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

enum AttemptFailure {
    Cancelled,
    TimedOut,
    Driver(ArenaError),
}

#[derive(Clone, Copy)]
enum BudgetInterruption {
    Cancelled,
    TimedOut,
}

trait ArenaClock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Future<Output = ()> + Send>>;

    #[cfg(test)]
    fn test_budget_paused(&self) {}
}

struct SystemArenaClock;

impl ArenaClock for SystemArenaClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
            deadline,
        )))
    }
}

struct ChargeableBudget {
    clock: Arc<dyn ArenaClock>,
    remaining: Duration,
    charged_since: Option<Instant>,
}

impl ChargeableBudget {
    fn new(clock: Arc<dyn ArenaClock>, duration: Duration) -> Self {
        let charged_since = clock.now();
        Self {
            clock,
            remaining: duration,
            charged_since: Some(charged_since),
        }
    }

    fn pause(&mut self) {
        if self.charged_since.is_some() {
            self.charge();
            self.charged_since = None;
            #[cfg(test)]
            self.clock.test_budget_paused();
        }
    }

    fn resume(&mut self) {
        if self.charged_since.is_none() {
            self.charged_since = Some(self.clock.now());
        }
    }

    fn is_paused(&self) -> bool {
        self.charged_since.is_none()
    }

    fn charge(&mut self) {
        let Some(charged_since) = self.charged_since else {
            return;
        };
        let now = self.clock.now();
        self.remaining = self
            .remaining
            .saturating_sub(now.saturating_duration_since(charged_since));
        self.charged_since = Some(now);
    }

    fn check(&mut self, cancellation: &CancellationToken) -> Result<(), BudgetInterruption> {
        self.charge();
        if cancellation.is_cancelled() {
            Err(BudgetInterruption::Cancelled)
        } else if self.remaining.is_zero() {
            Err(BudgetInterruption::TimedOut)
        } else {
            Ok(())
        }
    }

    async fn wait<T>(
        &mut self,
        cancellation: &CancellationToken,
        future: impl Future<Output = T>,
    ) -> Result<T, BudgetInterruption> {
        if self.is_paused() {
            return tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(BudgetInterruption::Cancelled),
                result = future => Ok(result),
            };
        }

        self.charge();
        let deadline = self.clock.now().checked_add(self.remaining);
        let result = match deadline {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(BudgetInterruption::Cancelled),
                    _ = self.clock.sleep_until(deadline) => Err(BudgetInterruption::TimedOut),
                    result = future => Ok(result),
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(BudgetInterruption::Cancelled),
                    result = future => Ok(result),
                }
            }
        };
        self.charge();
        result
    }
}

impl From<BudgetInterruption> for AttemptFailure {
    fn from(interruption: BudgetInterruption) -> Self {
        match interruption {
            BudgetInterruption::Cancelled => Self::Cancelled,
            BudgetInterruption::TimedOut => Self::TimedOut,
        }
    }
}

impl AttemptFailure {
    fn with_interrupted_execution(self, execution: InterruptedExecution) -> ArenaError {
        match self {
            Self::Cancelled => ArenaError::Cancelled {
                execution: Box::new(execution),
            },
            Self::TimedOut => ArenaError::TimedOut {
                execution: Box::new(execution),
            },
            Self::Driver(error) => error.with_interrupted_execution(execution),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ChildArenaConstructionPhase {
    PoolOpened,
    CheckedOut,
    CheckpointRestored,
    ResultValuesReset,
    Dumped,
}

#[derive(Clone)]
pub(crate) struct ArenaChildSource {
    worker_path: std::path::PathBuf,
    tool_host: ToolHost,
    runtime: RuntimeIdentity,
    repl_config: ReplConfig,
    execution_timeout: Duration,
    clock: Arc<dyn ArenaClock>,
    #[cfg(test)]
    collection_test_gate: Option<CollectionTestGate>,
    #[cfg(test)]
    child_construction_test_gate: Option<ChildConstructionTestGate>,
}

impl ArenaChildSource {
    pub(crate) async fn child(
        &self,
        checkpoint: &Checkpoint,
        requested_read_only: bool,
        progress: &mut impl FnMut(ChildArenaConstructionPhase),
    ) -> Result<Arena, ArenaError> {
        #[cfg(test)]
        if let Some(child_construction_test_gate) = &self.child_construction_test_gate {
            child_construction_test_gate
                .wait_before_construction()
                .await;
        }
        if checkpoint.identity != self.runtime {
            return Err(ArenaError::IncompatibleCheckpoint {
                expected: self.runtime.0.clone(),
                actual: checkpoint.identity.0.clone(),
            });
        }
        let pool = Arena::open_pool(self.worker_path.clone()).await?;
        progress(ChildArenaConstructionPhase::PoolOpened);
        let mut checkout = pool
            .checkout(&self.repl_config)
            .await
            .map_err(pool_failure)?;
        progress(ChildArenaConstructionPhase::CheckedOut);
        Arena::restore_checkout(&mut checkout, checkpoint).await?;
        progress(ChildArenaConstructionPhase::CheckpointRestored);
        reset_result_values(&mut checkout).await?;
        progress(ChildArenaConstructionPhase::ResultValuesReset);
        let state = checkout.dump().await.map_err(pool_failure)?;
        progress(ChildArenaConstructionPhase::Dumped);

        Ok(Arena {
            worker_path: self.worker_path.clone(),
            pool,
            checkout: Some(checkout),
            tool_host: self.tool_host.for_child(requested_read_only),
            checkpoint: Checkpoint {
                identity: self.runtime.clone(),
                state,
                authorisations: checkpoint.authorisations.clone(),
            },
            runtime: self.runtime.clone(),
            repl_config: self.repl_config.clone(),
            execution_timeout: self.execution_timeout,
            clock: self.clock.clone(),
            #[cfg(test)]
            collection_test_gate: self.collection_test_gate.clone(),
            #[cfg(test)]
            child_construction_test_gate: self.child_construction_test_gate.clone(),
            #[cfg(test)]
            fail_next_recovery: false,
            #[cfg(test)]
            recovery_test_gate: None,
        })
    }

    pub(crate) fn prompt_reference(&self, read_only: bool) -> String {
        self.tool_host.for_child(read_only).prompt_reference()
    }
}

pub struct Arena {
    worker_path: std::path::PathBuf,
    pool: Pool,
    checkout: Option<Checkout>,
    tool_host: ToolHost,
    checkpoint: Checkpoint,
    runtime: RuntimeIdentity,
    repl_config: ReplConfig,
    execution_timeout: Duration,
    clock: Arc<dyn ArenaClock>,
    #[cfg(test)]
    collection_test_gate: Option<CollectionTestGate>,
    #[cfg(test)]
    child_construction_test_gate: Option<ChildConstructionTestGate>,
    #[cfg(test)]
    fail_next_recovery: bool,
    #[cfg(test)]
    recovery_test_gate: Option<RecoveryTestGate>,
}

impl Arena {
    pub(crate) async fn new(
        worker_path: impl AsRef<Path>,
        tool_host: ToolHost,
    ) -> Result<Self, ArenaError> {
        Self::open(
            worker_path.as_ref().to_path_buf(),
            tool_host,
            ARENA_EXECUTION_TIMEOUT,
            Some(ARENA_MAX_MEMORY_BYTES),
            Arc::new(SystemArenaClock),
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn with_limits(
        worker_path: impl AsRef<Path>,
        tool_host: ToolHost,
        execution_timeout: Duration,
        max_memory: Option<usize>,
    ) -> Result<Self, ArenaError> {
        Self::open(
            worker_path.as_ref().to_path_buf(),
            tool_host,
            execution_timeout,
            max_memory,
            Arc::new(SystemArenaClock),
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits_and_clock(
        worker_path: impl AsRef<Path>,
        tool_host: ToolHost,
        execution_timeout: Duration,
        max_memory: Option<usize>,
        clock: Arc<dyn ArenaClock>,
    ) -> Result<Self, ArenaError> {
        Self::open(
            worker_path.as_ref().to_path_buf(),
            tool_host,
            execution_timeout,
            max_memory,
            clock,
        )
        .await
    }

    async fn open(
        worker_path: std::path::PathBuf,
        tool_host: ToolHost,
        execution_timeout: Duration,
        max_memory: Option<usize>,
        clock: Arc<dyn ArenaClock>,
    ) -> Result<Self, ArenaError> {
        let runtime = validate_worker(&worker_path).await?;
        let pool = Self::open_pool(worker_path.clone()).await?;
        let repl_config = ReplConfig {
            limits: Some(ResourceLimits {
                max_memory,
                ..ResourceLimits::default()
            }),
            ..ReplConfig::default()
        };
        let mut checkout = pool.checkout(&repl_config).await.map_err(pool_failure)?;
        bootstrap(&mut checkout).await?;
        let state = checkout.dump().await.map_err(pool_failure)?;
        let checkpoint = Checkpoint {
            identity: runtime.clone(),
            state,
            authorisations: WholeFileAuthorisations::default(),
        };

        Ok(Self {
            worker_path,
            pool,
            checkout: Some(checkout),
            tool_host,
            checkpoint,
            runtime,
            repl_config,
            execution_timeout,
            clock,
            #[cfg(test)]
            collection_test_gate: None,
            #[cfg(test)]
            child_construction_test_gate: None,
            #[cfg(test)]
            fail_next_recovery: false,
            #[cfg(test)]
            recovery_test_gate: None,
        })
    }

    async fn open_pool(worker_path: std::path::PathBuf) -> Result<Pool, ArenaError> {
        let mut config = PoolConfig::subprocess(worker_path);
        config.min_processes = 1;
        config.max_processes = 1;
        Pool::new(config).await.map_err(pool_failure)
    }

    pub(crate) fn child_source(&self) -> ArenaChildSource {
        ArenaChildSource {
            worker_path: self.worker_path.clone(),
            tool_host: self.tool_host.clone(),
            runtime: self.runtime.clone(),
            repl_config: self.repl_config.clone(),
            execution_timeout: self.execution_timeout,
            clock: self.clock.clone(),
            #[cfg(test)]
            collection_test_gate: self.collection_test_gate.clone(),
            #[cfg(test)]
            child_construction_test_gate: self.child_construction_test_gate.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) async fn child(
        &self,
        checkpoint: &Checkpoint,
        requested_read_only: bool,
    ) -> Result<Self, ArenaError> {
        let mut progress = |_| {};
        self.child_source()
            .child(checkpoint, requested_read_only, &mut progress)
            .await
    }

    pub async fn execute(
        &mut self,
        code: &str,
        cancellation: &CancellationToken,
    ) -> Result<Execution, ArenaError> {
        let mut progress = |_| {};
        self.execute_with_progress(code, cancellation, &mut progress)
            .await
    }

    pub(crate) async fn execute_with_progress(
        &mut self,
        code: &str,
        cancellation: &CancellationToken,
        progress: &mut (impl FnMut(ArenaProgress) + Send),
    ) -> Result<Execution, ArenaError> {
        self.execute_with_progress_and_context(code, cancellation, progress, None, None)
            .await
    }

    pub(crate) async fn execute_with_progress_and_context(
        &mut self,
        code: &str,
        cancellation: &CancellationToken,
        progress: &mut (impl FnMut(ArenaProgress) + Send),
        invocation: Option<&AgentInvocationContext>,
        selection: Option<ModelSelection>,
    ) -> Result<Execution, ArenaError> {
        let mut budget = ChargeableBudget::new(self.clock.clone(), self.execution_timeout);
        match budget
            .wait(cancellation, self.restore_current_checkpoint_if_needed())
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(interruption) => {
                let error = AttemptFailure::from(interruption).with_interrupted_execution(
                    InterruptedExecutionState::new().finish(Instant::now(), progress),
                );
                self.checkout.take();
                return Err(self.recover(error).await);
            }
        }
        let context = invocation.map(|invocation| {
            invocation.execution(
                self.child_source(),
                selection,
                self.checkpoint(),
                cancellation.clone(),
            )
        });
        let mut checkout = self
            .checkout
            .take()
            .expect("restoring a missing checkout should install one");
        let max_memory = self
            .repl_config
            .limits
            .as_ref()
            .and_then(|limits| limits.max_memory);
        let mut interruption = InterruptedExecutionState::new();
        let mut authorisations = self.checkpoint.authorisations.clone();
        let attempt = execute_checkout(
            &mut checkout,
            &self.tool_host,
            code,
            context.as_ref(),
            max_memory,
            &mut authorisations,
            progress,
            &mut interruption,
            &mut budget,
            cancellation,
        )
        .await;

        match attempt {
            Ok((outcome, state)) => {
                self.checkout = Some(checkout);
                self.checkpoint = Checkpoint {
                    identity: self.runtime.clone(),
                    state,
                    authorisations,
                };
                let (tool_calls, read_paths, modified_paths) = interruption.tools.into_parts();
                Ok(Execution {
                    stdout: interruption.streams.stdout.finish(),
                    stderr: interruption.streams.stderr.finish(),
                    outcome,
                    tool_calls,
                    read_paths,
                    modified_paths,
                })
            }
            Err(failure) => {
                let error = failure
                    .with_interrupted_execution(interruption.finish(Instant::now(), progress));
                drop(checkout);
                Err(self.recover(error).await)
            }
        }
    }

    pub fn checkpoint(&self) -> Checkpoint {
        self.checkpoint.clone()
    }

    #[cfg(test)]
    pub(crate) async fn collect_result_values(
        &mut self,
    ) -> Result<BTreeMap<String, ToolValue>, ResultValuesError> {
        let cancellation = CancellationToken::new();
        self.collect_result_values_with_cancellation(&cancellation)
            .await
    }

    pub(crate) async fn collect_result_values_with_cancellation(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<BTreeMap<String, ToolValue>, ResultValuesError> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(ResultValuesError::Cancelled),
            result = self.restore_current_checkpoint_if_needed() => {
                result.map_err(result_values_arena_failure)?;
            }
        }
        let mut checkout = self
            .checkout
            .take()
            .expect("restoring a missing checkout should install one");
        let authorisations = self.checkpoint.authorisations.clone();
        #[cfg(test)]
        let collection_test_gate = self.collection_test_gate.clone();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ResultValuesError::Cancelled),
            result = async {
                #[cfg(test)]
                if let Some(collection_test_gate) = collection_test_gate {
                    collection_test_gate.wait_before_collection().await;
                }
                let value = timeout(
                    self.execution_timeout,
                    collect_result_values_checkout(&mut checkout),
                )
                .await
                .map_err(|_| ResultValuesError::export("collection timed out"))??;
                let values = result_values_from_monty(value)?;
                let state = checkout.dump().await.map_err(result_values_pool_failure)?;
                Ok((values, state))
            } => result,
        };

        match result {
            Ok((values, state)) => {
                self.checkout = Some(checkout);
                self.checkpoint = Checkpoint {
                    identity: self.runtime.clone(),
                    state,
                    authorisations,
                };
                Ok(values)
            }
            Err(error) => {
                drop(checkout);
                self.restore_current_checkpoint_if_needed()
                    .await
                    .map_err(result_values_arena_failure)?;
                Err(error)
            }
        }
    }

    pub async fn restore(&mut self, checkpoint: Checkpoint) -> Result<(), ArenaError> {
        self.validate_checkpoint(&checkpoint)?;
        self.checkout.take();
        let checkout = self.checkout_from_checkpoint(&checkpoint).await?;
        self.checkout = Some(checkout);
        self.checkpoint = checkpoint;
        Ok(())
    }

    fn validate_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), ArenaError> {
        if checkpoint.identity != self.runtime {
            return Err(ArenaError::IncompatibleCheckpoint {
                expected: self.runtime.0.clone(),
                actual: checkpoint.identity.0.clone(),
            });
        }
        Ok(())
    }

    async fn restore_current_checkpoint_if_needed(&mut self) -> Result<(), ArenaError> {
        if self.checkout.is_none() {
            self.checkout = Some(self.checkout_from_checkpoint(&self.checkpoint).await?);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn with_child_construction_test_gate(
        mut self,
        child_construction_test_gate: ChildConstructionTestGate,
    ) -> Self {
        self.child_construction_test_gate = Some(child_construction_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_collection_test_gate(
        mut self,
        collection_test_gate: CollectionTestGate,
    ) -> Self {
        self.collection_test_gate = Some(collection_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn fail_next_recovery_for_test(&mut self) {
        self.fail_next_recovery = true;
    }

    #[cfg(test)]
    pub(crate) fn with_recovery_test_gate(mut self, recovery_test_gate: RecoveryTestGate) -> Self {
        self.recovery_test_gate = Some(recovery_test_gate);
        self
    }

    async fn recover(&mut self, original: ArenaError) -> ArenaError {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_recovery) {
            if let Some(recovery_test_gate) = &self.recovery_test_gate {
                recovery_test_gate.wait_before_failure().await;
            }
            return ArenaError::RecoveryFailed {
                original: Box::new(original),
                recovery: Box::new(ArenaError::Infrastructure {
                    message: "test recovery failure".to_owned(),
                    execution: None,
                }),
            };
        }
        match self.checkout_from_checkpoint(&self.checkpoint).await {
            Ok(checkout) => {
                self.checkout = Some(checkout);
                original
            }
            Err(recovery) => ArenaError::RecoveryFailed {
                original: Box::new(original),
                recovery: Box::new(recovery),
            },
        }
    }

    async fn checkout_from_checkpoint(
        &self,
        checkpoint: &Checkpoint,
    ) -> Result<Checkout, ArenaError> {
        self.validate_checkpoint(checkpoint)?;
        let mut checkout = self
            .pool
            .checkout(&self.repl_config)
            .await
            .map_err(pool_failure)?;
        Self::restore_checkout(&mut checkout, checkpoint).await?;
        Ok(checkout)
    }

    async fn restore_checkout(
        checkout: &mut Checkout,
        checkpoint: &Checkpoint,
    ) -> Result<(), ArenaError> {
        let mut on_print = on_print_sync(|_, _| {});
        let (event, _) = checkout
            .restore(checkpoint.state.clone(), vec![], &mut on_print)
            .await
            .map_err(pool_failure)?;
        if event.is_some() {
            return Err(ArenaError::SuspendedCheckpoint);
        }
        Ok(())
    }
}

async fn reset_result_values(checkout: &mut Checkout) -> Result<(), ArenaError> {
    let mut on_print = on_print_sync(|_, _| {});
    match checkout
        .feed("result_values = {}", vec![], vec![], false, &mut on_print)
        .await
        .map_err(pool_failure)?
    {
        TurnEvent::Complete(MontyObject::None) => Ok(()),
        event => Err(ArenaError::unsupported_suspension(format!(
            "result_values initialisation did not complete: {event:?}"
        ))),
    }
}

async fn collect_result_values_checkout(
    checkout: &mut Checkout,
) -> Result<MontyObject, ResultValuesError> {
    let mut on_print = on_print_sync(|_, _| {});
    match checkout
        .feed(RESULT_VALUES_BINDING, vec![], vec![], false, &mut on_print)
        .await
        .map_err(result_values_pool_failure)?
    {
        TurnEvent::Complete(value) => Ok(value),
        TurnEvent::NameLookup { .. } => {
            Err(ResultValuesError::export("result_values is not defined"))
        }
        TurnEvent::FunctionCall { .. }
        | TurnEvent::OsCall { .. }
        | TurnEvent::ResolveFutures { .. } => Err(ResultValuesError::export(
            "collection attempted a host suspension",
        )),
    }
}

fn result_values_pool_failure(error: PoolError) -> ResultValuesError {
    ResultValuesError::export(format!("collection transport failed: {error}"))
}

fn result_values_arena_failure(error: ArenaError) -> ResultValuesError {
    ResultValuesError::export(format!(
        "could not restore result_values checkpoint: {error}"
    ))
}

struct ResultValueBounds {
    items: usize,
    payload_bytes: usize,
}

impl ResultValueBounds {
    fn new() -> Self {
        Self {
            items: 0,
            payload_bytes: 0,
        }
    }

    fn add_item(&mut self) -> Result<(), ResultValuesError> {
        self.items = self
            .items
            .checked_add(1)
            .filter(|items| *items <= COLLECTION_MAX_ITEMS)
            .ok_or_else(|| {
                ResultValuesError::export(format!(
                    "export exceeds the {COLLECTION_MAX_ITEMS}-item limit"
                ))
            })?;
        Ok(())
    }

    fn add_payload(&mut self, bytes: usize) -> Result<(), ResultValuesError> {
        self.payload_bytes = self
            .payload_bytes
            .checked_add(bytes)
            .filter(|payload_bytes| *payload_bytes <= COLLECTION_MAX_PAYLOAD_BYTES)
            .ok_or_else(|| {
                ResultValuesError::export(format!(
                    "export exceeds the {COLLECTION_MAX_PAYLOAD_BYTES}-byte payload limit"
                ))
            })?;
        Ok(())
    }
}

fn result_values_from_monty(
    value: MontyObject,
) -> Result<BTreeMap<String, ToolValue>, ResultValuesError> {
    let MontyObject::Dict(values) = value else {
        return Err(ResultValuesError::export(
            "result_values must be a dictionary",
        ));
    };
    let mut bounds = ResultValueBounds::new();
    let mut converted = BTreeMap::new();
    for (key, value) in values {
        bounds.add_item()?;
        let MontyObject::String(key) = key else {
            return Err(ResultValuesError::export(
                "result_values dictionaries require string keys",
            ));
        };
        bounds.add_payload(key.len())?;
        let value = result_value_from_monty(value, &mut bounds)?;
        if converted.insert(key, value).is_some() {
            return Err(ResultValuesError::export(
                "result_values dictionaries cannot contain duplicate keys",
            ));
        }
    }
    Ok(converted)
}

fn result_value_from_monty(
    value: MontyObject,
    bounds: &mut ResultValueBounds,
) -> Result<ToolValue, ResultValuesError> {
    match value {
        MontyObject::None => Ok(ToolValue::None),
        MontyObject::Bool(value) => Ok(ToolValue::Bool(value)),
        MontyObject::Int(value) => Ok(ToolValue::Integer(value)),
        MontyObject::Float(value) => Ok(ToolValue::Float(value)),
        MontyObject::String(value) => {
            bounds.add_payload(value.len())?;
            Ok(ToolValue::String(value))
        }
        MontyObject::List(values) => {
            let mut converted = Vec::new();
            for value in values {
                bounds.add_item()?;
                converted.push(result_value_from_monty(value, bounds)?);
            }
            Ok(ToolValue::List(converted))
        }
        MontyObject::Dict(values) => {
            let mut converted = BTreeMap::new();
            for (key, value) in values {
                bounds.add_item()?;
                let MontyObject::String(key) = key else {
                    return Err(ResultValuesError::export(
                        "result_values dictionaries require string keys",
                    ));
                };
                bounds.add_payload(key.len())?;
                let value = result_value_from_monty(value, bounds)?;
                if converted.insert(key, value).is_some() {
                    return Err(ResultValuesError::export(
                        "result_values dictionaries cannot contain duplicate keys",
                    ));
                }
            }
            Ok(ToolValue::Dictionary(converted))
        }
        value => Err(ResultValuesError::export(format!(
            "result_values contains unsupported {}",
            value.type_name()
        ))),
    }
}

async fn validate_worker(path: &Path) -> Result<RuntimeIdentity, ArenaError> {
    validate_worker_with_timeout(path, WORKER_VERSION_TIMEOUT).await
}

async fn validate_worker_with_timeout(
    path: &Path,
    version_timeout: Duration,
) -> Result<RuntimeIdentity, ArenaError> {
    if !path.is_absolute() {
        return Err(ArenaError::WorkerPathNotAbsolute);
    }
    let mut command = Command::new(path);
    command.arg("--version").kill_on_drop(true);
    let output = timeout(version_timeout, command.output())
        .await
        .map_err(|_| ArenaError::WorkerVersionTimedOut)?
        .map_err(|error| ArenaError::WorkerVersionExecution {
            message: error.to_string(),
        })?;
    if !output.status.success() {
        return Err(ArenaError::WorkerVersionFailed {
            status: output.status.to_string(),
        });
    }
    let actual = String::from_utf8(output.stdout)
        .map_err(|_| ArenaError::WorkerVersionInvalidOutput)?
        .trim()
        .to_owned();
    if actual != RUNTIME_IDENTITY {
        return Err(ArenaError::WorkerVersionMismatch {
            expected: RUNTIME_IDENTITY.to_owned(),
            actual,
        });
    }
    Ok(RuntimeIdentity(RUNTIME_IDENTITY.to_owned()))
}

fn pool_failure(error: PoolError) -> ArenaError {
    ArenaError::Infrastructure {
        message: error.to_string(),
        execution: None,
    }
}

async fn bootstrap(checkout: &mut Checkout) -> Result<(), ArenaError> {
    let source = bootstrap_source();
    let mut on_print = on_print_sync(|_, _| {});
    match checkout
        .feed(&source, vec![], vec![], false, &mut on_print)
        .await
        .map_err(pool_failure)?
    {
        TurnEvent::Complete(MontyObject::None) => Ok(()),
        event => Err(ArenaError::unsupported_suspension(format!(
            "tool bootstrap did not complete: {event:?}"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_checkout(
    checkout: &mut Checkout,
    tool_host: &ToolHost,
    code: &str,
    context: Option<&TrustedExecutionContext>,
    max_memory: Option<usize>,
    authorisations: &mut WholeFileAuthorisations,
    progress: &mut (impl FnMut(ArenaProgress) + Send),
    interruption: &mut InterruptedExecutionState,
    budget: &mut ChargeableBudget,
    cancellation: &CancellationToken,
) -> Result<(ExecutionOutcome, Vec<u8>), AttemptFailure> {
    let result = drive(
        checkout,
        tool_host,
        code,
        context,
        authorisations,
        progress,
        interruption,
        budget,
        cancellation,
    )
    .await;
    interruption.streams.flush_stdout(Instant::now(), progress);
    let (outcome, unsafe_memory, error_text) = match result {
        Ok(value) => (
            ExecutionOutcome::Complete {
                result: (!matches!(value, MontyObject::None))
                    .then(|| observation_from_text(value.py_repr())),
            },
            false,
            None,
        ),
        Err(DriverError::Runtime(error)) => {
            let unsafe_memory = is_unsafe_memory_error(&error, max_memory);
            let error_text = error.to_string();
            (
                ExecutionOutcome::PythonError(PythonError::from_text(error_text.clone())),
                unsafe_memory,
                Some(error_text),
            )
        }
        Err(DriverError::Fatal(error)) => return Err(AttemptFailure::Driver(error)),
        Err(DriverError::Cancelled) => return Err(AttemptFailure::Cancelled),
        Err(DriverError::TimedOut) => return Err(AttemptFailure::TimedOut),
    };
    let state = budget
        .wait(cancellation, checkout.dump())
        .await
        .map_err(AttemptFailure::from)?
        .map_err(|error| {
            let context = error_text.as_deref().unwrap_or("completed execution");
            AttemptFailure::Driver(ArenaError::Infrastructure {
                message: format!("post-execution checkpoint failed after {context}: {error}"),
                execution: None,
            })
        })?;
    budget.check(cancellation).map_err(AttemptFailure::from)?;
    if unsafe_memory {
        return Err(AttemptFailure::Driver(ArenaError::Infrastructure {
            message: format!(
                "unsafe resource memory failure: {}",
                error_text.expect("memory errors have diagnostics")
            ),
            execution: None,
        }));
    }
    Ok((outcome, state))
}

fn is_unsafe_memory_error(error: &MontyException, max_memory: Option<usize>) -> bool {
    if error.exc_type() != ExcType::MemoryError {
        return false;
    }
    match error.message() {
        Some("the worker exceeded its memory limit and was terminated") => {
            error.traceback().is_empty()
        }
        Some(message) => is_unsafe_soft_memory_error(message, max_memory),
        None => false,
    }
}

fn is_unsafe_soft_memory_error(message: &str, max_memory: Option<usize>) -> bool {
    let Some((used, limit)) = parse_soft_memory_limit(message) else {
        return false;
    };
    used > limit && max_memory == Some(limit)
}

fn parse_soft_memory_limit(message: &str) -> Option<(usize, usize)> {
    let message = message.strip_prefix("memory limit exceeded: ")?;
    let (used, limit) = message.split_once(" bytes > ")?;
    let limit = limit.strip_suffix(" bytes")?;
    Some((parse_canonical_usize(used)?, parse_canonical_usize(limit)?))
}

fn parse_canonical_usize(value: &str) -> Option<usize> {
    let parsed = value.parse::<usize>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

enum DriverError {
    Runtime(MontyException),
    Fatal(ArenaError),
    Cancelled,
    TimedOut,
}

struct PendingToolCall {
    call_id: u32,
    tool: ToolName,
    invocation: Option<InvocationOperation>,
    arguments: ToolValue,
    token: ToolCallToken,
    run_preview: Option<RunPreviewState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingCallMode {
    Ordinary,
    Control,
    Waiting,
}

fn update_budget_pause(budget: &mut ChargeableBudget, call_modes: &BTreeMap<u32, PendingCallMode>) {
    if !call_modes.is_empty()
        && call_modes
            .values()
            .all(|mode| *mode == PendingCallMode::Waiting)
    {
        budget.pause();
    } else {
        budget.resume();
    }
}

enum BatchEvent {
    Result(ToolResult),
    WaitPhase(u32),
    ChildDeadline,
    FilesystemComplete,
    Output(Option<ToolOutputChunk>),
}

#[allow(clippy::too_many_arguments)]
async fn drive(
    checkout: &mut Checkout,
    tool_host: &ToolHost,
    code: &str,
    context: Option<&TrustedExecutionContext>,
    authorisations: &mut WholeFileAuthorisations,
    progress: &mut (impl FnMut(ArenaProgress) + Send),
    interruption: &mut InterruptedExecutionState,
    budget: &mut ChargeableBudget,
    cancellation: &CancellationToken,
) -> Result<MontyObject, DriverError> {
    let InterruptedExecutionState {
        streams,
        tools,
        began,
    } = interruption;
    *began = true;
    macro_rules! with_print {
        ($on_print:ident => $operation:expr) => {{
            let mut $on_print = on_print_sync(|stream, text| {
                streams.push_with_progress(stream, text, Instant::now(), progress);
            });
            $operation
        }};
    }
    macro_rules! turn {
        ($label:lifetime, $on_print:ident => $operation:expr) => {{
            match with_print!($on_print => budget.wait(cancellation, $operation).await) {
                Ok(Ok(event)) => event,
                Ok(Err(PoolError::Runtime(error))) => {
                    break $label Err(DriverError::Runtime(error));
                }
                Ok(Err(error)) => break $label Err(DriverError::Fatal(pool_failure(error))),
                Err(BudgetInterruption::Cancelled) => break $label Err(DriverError::Cancelled),
                Err(BudgetInterruption::TimedOut) => break $label Err(DriverError::TimedOut),
            }
        }};
    }
    'drive: {
        let mut event =
            turn!('drive, on_print => checkout.feed(code, vec![], vec![], false, &mut on_print));

        let mut pending_calls = Vec::new();
        loop {
            event = match event {
                TurnEvent::Complete(value) => break 'drive Ok(value),
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    kwargs,
                    call_id,
                    object_id,
                } => {
                    let invocation = InvocationOperation::from_private_name(&function_name);
                    let Some(tool) = ToolName::from_private_name(&function_name)
                        .or_else(|| invocation.map(InvocationOperation::tool))
                    else {
                        event = turn!('drive, on_print => checkout.resume(ResumeValue::NotFound, &mut on_print));
                        continue;
                    };
                    if object_id.is_some() || !kwargs.is_empty() {
                        turn!('drive, on_print => checkout.resume(
                            ResumeValue::Error(tool_error(ToolError::type_error(
                                "host functions accept positional arguments only",
                            ))),
                            &mut on_print,
                        ))
                    } else {
                        let arguments = match args
                            .into_iter()
                            .map(tool_value_from_monty)
                            .collect::<Result<Vec<_>, _>>()
                        {
                            Ok(arguments) => ToolValue::List(arguments),
                            Err(error) => {
                                event = turn!('drive, on_print => checkout
                                    .resume(ResumeValue::Error(tool_error(error)), &mut on_print));
                                continue;
                            }
                        };
                        let token = tools.admit(tool);
                        progress(ArenaProgress::ToolUpdated {
                            token,
                            call: tools.call(token),
                        });
                        let run_preview =
                            run_command_argument(tool, &arguments).map(RunPreviewState::new);
                        if let Some(run_preview) = &run_preview {
                            progress(ArenaProgress::RunPreviewUpdated {
                                token,
                                preview: run_preview.snapshot(),
                            });
                        }
                        pending_calls.push(PendingToolCall {
                            call_id,
                            tool,
                            invocation,
                            arguments,
                            token,
                            run_preview,
                        });
                        turn!('drive, on_print => checkout.resume(ResumeValue::Future, &mut on_print))
                    }
                }
                TurnEvent::NameLookup { name, object_id } => {
                    let value = if object_id.is_none() {
                        ToolName::from_private_name(&name)
                            .map(|tool| tool.private_name())
                            .or_else(|| {
                                InvocationOperation::from_private_name(&name).map(|_| name.as_str())
                            })
                            .map(|name| MontyObject::Function {
                                name: name.to_owned(),
                                docstring: None,
                            })
                    } else {
                        None
                    };
                    turn!('drive, on_print => checkout.resume_name_lookup(value, &mut on_print))
                }
                TurnEvent::OsCall { .. } => {
                    turn!('drive, on_print => checkout.resume(ResumeValue::NotHandled, &mut on_print))
                }
                TurnEvent::ResolveFutures { pending_call_ids } => {
                    let pending = std::mem::take(&mut pending_calls);
                    if !matches_pending_batch(&pending, &pending_call_ids) {
                        break 'drive Err(DriverError::Fatal(ArenaError::unsupported_suspension(
                            format!(
                                "pending futures did not match admitted calls: {pending_call_ids:?}"
                            ),
                        )));
                    }
                    #[cfg(test)]
                    tool_host.note_batch_driver_pending_call_ids(&pending_call_ids);
                    let mut pending = pending
                        .into_iter()
                        .map(|call| (call.call_id, call))
                        .collect::<BTreeMap<_, _>>();
                    let mut tokens = BTreeMap::new();
                    let mut call_modes = BTreeMap::new();
                    let mut run_previews = BTreeMap::new();
                    let mut filesystem_calls = Vec::new();
                    let (wait_phase_sender, mut wait_phase_receiver) = mpsc::unbounded_channel();
                    let mut invocation_calls: FuturesUnordered<
                        Pin<Box<dyn Future<Output = ToolResult> + Send + '_>>,
                    > = FuturesUnordered::new();
                    for call_id in &pending_call_ids {
                        let call = pending
                            .remove(call_id)
                            .expect("validated call ID must be admitted");
                        assert!(
                            tokens
                                .insert(call.call_id, (call.tool, call.token))
                                .is_none(),
                            "a pending host call must have a unique call ID"
                        );
                        let mode = if call.invocation.is_some() {
                            PendingCallMode::Control
                        } else {
                            PendingCallMode::Ordinary
                        };
                        assert!(
                            call_modes.insert(call.call_id, mode).is_none(),
                            "a pending host call must have a unique call ID"
                        );
                        if let Some(run_preview) = call.run_preview {
                            assert!(
                                run_previews
                                    .insert(call.call_id, (call.token, run_preview))
                                    .is_none(),
                                "a pending run call must have a unique call ID"
                            );
                        }
                        if let Some(operation) = call.invocation {
                            let call_id = call.call_id;
                            let arguments = call.arguments;
                            let wait_phase_sender = wait_phase_sender.clone();
                            invocation_calls.push(Box::pin(async move {
                                ToolResult::from_result(
                                    call_id,
                                    invoke_subagent(
                                        context,
                                        operation,
                                        arguments,
                                        Some(WaitPhaseSignal::new(
                                            call_id,
                                            wait_phase_sender.clone(),
                                        )),
                                    )
                                    .await,
                                )
                            }));
                        } else {
                            filesystem_calls.push(HostToolCall {
                                call_id: call.call_id,
                                tool: call.tool,
                                arguments: call.arguments,
                            });
                        }
                    }
                    let mutation_paths = filesystem_calls
                        .iter()
                        .filter_map(HostToolCall::mutation_path)
                        .collect::<BTreeSet<_>>();
                    let mut results = BTreeMap::new();
                    let (output_sender, mut output_receiver) =
                        mpsc::channel(RUN_OUTPUT_CHANNEL_CAPACITY);
                    let mut filesystem = Box::pin(tool_host.dispatch_batch_with_output(
                        filesystem_calls,
                        authorisations.clone(),
                        output_sender,
                    ));
                    let mut filesystem_complete = false;
                    while !tokens.is_empty() {
                        update_budget_pause(budget, &call_modes);
                        let clock = budget.clock.clone();
                        let child_deadline = async {
                            let Some(context) = context else {
                                return std::future::pending::<BatchEvent>().await;
                            };
                            let Some(deadline) = context.coordinator().next_deadline().await else {
                                return std::future::pending::<BatchEvent>().await;
                            };
                            clock.sleep_until(deadline).await;
                            BatchEvent::ChildDeadline
                        };
                        let next_event = async {
                            tokio::select! {
                                event = child_deadline => event,
                                call_id = wait_phase_receiver.recv() => {
                                    BatchEvent::WaitPhase(call_id.expect("wait-phase sender remains open"))
                                },
                                result = filesystem.next(), if !filesystem_complete => match result {
                                    Some(result) => BatchEvent::Result(result),
                                    None => BatchEvent::FilesystemComplete,
                                },
                                result = invocation_calls.next(), if !invocation_calls.is_empty() => {
                                    BatchEvent::Result(result.expect("every admitted subagent call must settle"))
                                },
                                output = output_receiver.recv(), if !run_previews.is_empty() => BatchEvent::Output(output),
                            }
                        };
                        let event = if budget.is_paused() {
                            tokio::select! {
                                biased;
                                _ = cancellation.cancelled() => break 'drive Err(DriverError::Cancelled),
                                event = next_event => event,
                            }
                        } else {
                            match budget.wait(cancellation, next_event).await {
                                Ok(event) => event,
                                Err(BudgetInterruption::Cancelled) => {
                                    break 'drive Err(DriverError::Cancelled);
                                }
                                Err(BudgetInterruption::TimedOut) => {
                                    break 'drive Err(DriverError::TimedOut);
                                }
                            }
                        };
                        let result = match event {
                            BatchEvent::WaitPhase(call_id) => {
                                if let Some(mode) = call_modes.get_mut(&call_id) {
                                    assert_eq!(
                                        *mode,
                                        PendingCallMode::Control,
                                        "only control calls can enter the child wait phase"
                                    );
                                    *mode = PendingCallMode::Waiting;
                                }
                                continue;
                            }
                            BatchEvent::ChildDeadline => {
                                let context =
                                    context.expect("child deadlines require an invocation context");
                                budget.resume();
                                match budget
                                    .wait(cancellation, context.coordinator().service())
                                    .await
                                {
                                    Ok(()) => {}
                                    Err(BudgetInterruption::Cancelled) => {
                                        break 'drive Err(DriverError::Cancelled);
                                    }
                                    Err(BudgetInterruption::TimedOut) => {
                                        break 'drive Err(DriverError::TimedOut);
                                    }
                                }
                                continue;
                            }
                            BatchEvent::FilesystemComplete => {
                                filesystem_complete = true;
                                continue;
                            }
                            BatchEvent::Output(output) => {
                                if let Some(output) = output {
                                    update_run_preview(output, &mut run_previews, progress);
                                }
                                continue;
                            }
                            BatchEvent::Result(result) => {
                                budget.resume();
                                result
                            }
                        };
                        while let Ok(output) = output_receiver.try_recv() {
                            update_run_preview(output, &mut run_previews, progress);
                        }
                        let call_id = result.call_id;
                        let (tool, token) = tokens
                            .remove(&call_id)
                            .expect("a host result must retain an admitted call ID");
                        call_modes
                            .remove(&call_id)
                            .expect("a host result must retain an admitted call mode");
                        let summary = tool_summary(tool, &result);
                        let call = tools.settle(token, summary);
                        tools.record_observed_file_operation(&result);
                        assert!(
                            results.insert(call_id, result).is_none(),
                            "a host tool call must settle once"
                        );
                        run_previews.remove(&call_id);
                        #[cfg(test)]
                        match budget
                            .wait(cancellation, tool_host.wait_after_read_settled())
                            .await
                        {
                            Ok(()) => {}
                            Err(BudgetInterruption::Cancelled) => {
                                break 'drive Err(DriverError::Cancelled);
                            }
                            Err(BudgetInterruption::TimedOut) => {
                                break 'drive Err(DriverError::TimedOut);
                            }
                        }
                        progress(ArenaProgress::ToolUpdated { token, call });
                        #[cfg(test)]
                        tool_host.note_batch_driver_settled(call_id);
                    }
                    assert!(
                        tokens.is_empty(),
                        "every admitted host call must settle exactly once"
                    );
                    assert!(
                        call_modes.is_empty(),
                        "every admitted host call mode must settle exactly once"
                    );
                    for call_id in &pending_call_ids {
                        results
                            .get(call_id)
                            .expect("validated call ID must settle")
                            .apply_batch_transition(authorisations, &mutation_paths);
                    }
                    let results = pending_call_ids
                        .into_iter()
                        .map(|call_id| {
                            let result = results
                                .remove(&call_id)
                                .expect("validated call ID must settle");
                            (call_id, tool_resume_value(result))
                        })
                        .collect();
                    #[cfg(test)]
                    tool_host.note_batch_driver_resume_futures();
                    turn!('drive, on_print => checkout.resume_futures(results, &mut on_print))
                }
            };
        }
    }
}

fn run_command_argument(tool: ToolName, arguments: &ToolValue) -> Option<&str> {
    if tool != ToolName::Run {
        return None;
    }
    let ToolValue::List(arguments) = arguments else {
        return None;
    };
    match arguments.as_slice() {
        [ToolValue::String(command)] | [ToolValue::String(command), ToolValue::String(_)] => {
            Some(command)
        }
        _ => None,
    }
}

fn update_run_preview(
    output: ToolOutputChunk,
    run_previews: &mut BTreeMap<u32, (ToolCallToken, RunPreviewState)>,
    progress: &mut (impl FnMut(ArenaProgress) + Send),
) {
    let Some((token, preview)) = run_previews.get_mut(&output.call_id) else {
        return;
    };
    preview.push_stdout(&output.stdout);
    progress(ArenaProgress::RunPreviewUpdated {
        token: *token,
        preview: preview.snapshot(),
    });
}

fn matches_pending_batch(calls: &[PendingToolCall], pending_call_ids: &[u32]) -> bool {
    let admitted = calls
        .iter()
        .map(|call| call.call_id)
        .collect::<BTreeSet<_>>();
    let requested = pending_call_ids.iter().copied().collect::<BTreeSet<_>>();
    admitted.len() == calls.len()
        && requested.len() == pending_call_ids.len()
        && admitted == requested
}

fn tool_value_from_monty(value: MontyObject) -> Result<ToolValue, ToolError> {
    match value {
        MontyObject::None => Ok(ToolValue::None),
        MontyObject::Bool(value) => Ok(ToolValue::Bool(value)),
        MontyObject::Int(value) => Ok(ToolValue::Integer(value)),
        MontyObject::Float(value) => Ok(ToolValue::Float(value)),
        MontyObject::String(value) => Ok(ToolValue::String(value)),
        MontyObject::List(values) => values
            .into_iter()
            .map(tool_value_from_monty)
            .collect::<Result<Vec<_>, _>>()
            .map(ToolValue::List),
        MontyObject::Dict(values) => values
            .into_iter()
            .map(|(key, value)| {
                let MontyObject::String(key) = key else {
                    return Err(ToolError::type_error(
                        "host dictionaries require string keys",
                    ));
                };
                Ok((key, tool_value_from_monty(value)?))
            })
            .collect::<Result<_, _>>()
            .map(ToolValue::Dictionary),
        value => Err(ToolError::type_error(format!(
            "host functions do not accept {} values",
            value.type_name()
        ))),
    }
}

fn tool_value_to_monty(value: ToolValue) -> MontyObject {
    match value {
        ToolValue::None => MontyObject::None,
        ToolValue::Bool(value) => MontyObject::Bool(value),
        ToolValue::Integer(value) => MontyObject::Int(value),
        ToolValue::Float(value) => MontyObject::Float(value),
        ToolValue::String(value) => MontyObject::String(value),
        ToolValue::List(values) => {
            MontyObject::List(values.into_iter().map(tool_value_to_monty).collect())
        }
        ToolValue::Dictionary(values) => MontyObject::dict(
            values
                .into_iter()
                .map(|(key, value)| (MontyObject::String(key), tool_value_to_monty(value)))
                .collect::<Vec<_>>(),
        ),
    }
}

fn tool_error(error: ToolError) -> MontyException {
    let exc_type = match error.kind {
        ToolErrorKind::Type => ExcType::TypeError,
        ToolErrorKind::FileNotFound => ExcType::FileNotFoundError,
        ToolErrorKind::IsADirectory => ExcType::IsADirectoryError,
        ToolErrorKind::NotADirectory => ExcType::NotADirectoryError,
        ToolErrorKind::Permission => ExcType::PermissionError,
        ToolErrorKind::Value => ExcType::ValueError,
        ToolErrorKind::Runtime => ExcType::RuntimeError,
        ToolErrorKind::Os => ExcType::OSError,
    };
    MontyException::new(exc_type, Some(error.to_string()))
}

fn tool_resume_value(result: ToolResult) -> ResumeValue {
    match result.result {
        Ok(value) => ResumeValue::Return(tool_value_to_monty(value)),
        Err(error) => ResumeValue::Error(tool_error(error)),
    }
}

fn tool_summary(tool: ToolName, result: &ToolResult) -> ToolCallSummary {
    if matches!(
        tool,
        ToolName::StartSubagent
            | ToolName::GetSubagent
            | ToolName::RefreshSubagent
            | ToolName::WaitSubagent
            | ToolName::CancelSubagent
    ) {
        return subagent_tool_summary(result);
    }
    if let Some(status) = result.mutation_status() {
        return match status {
            crate::tools::MutationStatus::Created => ToolCallSummary::Created,
            crate::tools::MutationStatus::Replaced => ToolCallSummary::Replaced,
            crate::tools::MutationStatus::Applied => ToolCallSummary::Applied,
        };
    }
    match tool {
        ToolName::Run => match &result.result {
            Ok(ToolValue::Dictionary(fields)) => match fields.get("exit_code") {
                Some(ToolValue::Integer(exit_code)) => ToolCallSummary::Exit(*exit_code),
                Some(ToolValue::None) => ToolCallSummary::Signal,
                _ => ToolCallSummary::Failed,
            },
            Ok(_) | Err(_) => ToolCallSummary::Failed,
        },
        ToolName::Write | ToolName::Edit => ToolCallSummary::Failed,
        ToolName::Read | ToolName::ListDir | ToolName::Glob | ToolName::Grep => {
            match &result.result {
                Ok(ToolValue::String(contents)) => ToolCallSummary::Count(contents.len()),
                Ok(ToolValue::List(entries)) => ToolCallSummary::Count(entries.len()),
                Ok(_) => ToolCallSummary::Count(0),
                Err(_) => ToolCallSummary::Failed,
            }
        }
        ToolName::StartSubagent
        | ToolName::GetSubagent
        | ToolName::RefreshSubagent
        | ToolName::WaitSubagent
        | ToolName::CancelSubagent => unreachable!("subagent calls return above"),
    }
}

fn subagent_tool_summary(result: &ToolResult) -> ToolCallSummary {
    let Ok(ToolValue::Dictionary(value)) = &result.result else {
        return ToolCallSummary::Failed;
    };
    let id = value
        .get("id")
        .or_else(|| {
            value.get("status").and_then(|status| match status {
                ToolValue::Dictionary(status) => status.get("id"),
                _ => None,
            })
        })
        .and_then(|id| match id {
            ToolValue::String(id) => Some(id),
            _ => None,
        });
    id.map_or(ToolCallSummary::Failed, |id| {
        ToolCallSummary::Subagent(id.clone())
    })
}

#[derive(Clone, Copy)]
struct ObservationLimits {
    max_total_bytes: usize,
    max_total_lines: usize,
    max_head_bytes: usize,
    max_head_lines: usize,
    max_tail_bytes: usize,
    max_tail_lines: usize,
}

struct ObservationAccumulator {
    limits: ObservationLimits,
    total_bytes: usize,
    lines: LineCounter,
    whole: Option<String>,
    truncated: Option<TruncatedObservation>,
    live_stdout: Option<LiveStdout>,
}

struct LiveStdout {
    total_bytes: usize,
    lines: LineCounter,
    pending: String,
    last_emitted: Option<Instant>,
    truncated: bool,
}

struct TruncatedObservation {
    head: String,
    head_ends_mid_line: bool,
    tail: Tail,
}

struct Tail {
    scalars: VecDeque<Scalar>,
    bytes: usize,
    terminated_lines: usize,
    starts_mid_line: bool,
    limits: ObservationLimits,
    #[cfg(test)]
    scalar_operations: usize,
}

struct Scalar {
    character: char,
    terminates_line: bool,
    ends_line: bool,
}

struct StreamAccumulators {
    stdout: ObservationAccumulator,
    stderr: ObservationAccumulator,
}

impl StreamAccumulators {
    fn new(limits: ObservationLimits) -> Self {
        Self {
            stdout: ObservationAccumulator::with_live_stdout(limits),
            stderr: ObservationAccumulator::new(limits),
        }
    }

    fn push_with_progress(
        &mut self,
        stream: PrintStream,
        text: &str,
        now: Instant,
        progress: &mut impl FnMut(ArenaProgress),
    ) {
        match stream {
            PrintStream::Stdout => self.stdout.push_live_stdout(text, now, progress),
            PrintStream::Stderr => self.stderr.push(text),
        }
    }

    fn flush_stdout(&mut self, now: Instant, progress: &mut impl FnMut(ArenaProgress)) {
        if let Some(live_stdout) = &mut self.stdout.live_stdout {
            live_stdout.flush(now, progress);
        }
    }
}

impl LiveStdout {
    fn new() -> Self {
        Self {
            total_bytes: 0,
            lines: LineCounter::default(),
            pending: String::new(),
            last_emitted: None,
            truncated: false,
        }
    }

    fn push(
        &mut self,
        text: &str,
        limits: ObservationLimits,
        now: Instant,
        progress: &mut impl FnMut(ArenaProgress),
    ) {
        if self.truncated {
            return;
        }
        let accepted = self.accepted_prefix_len(text, limits);
        if accepted > 0 {
            let text = &text[..accepted];
            self.total_bytes += text.len();
            self.lines.push(text);
            self.queue(text, now, progress);
        }
        if accepted < text.len() {
            self.flush(now, progress);
            self.truncated = true;
            progress(ArenaProgress::StdoutTruncated);
        }
    }

    fn accepted_prefix_len(&self, text: &str, limits: ObservationLimits) -> usize {
        let mut total_bytes = self.total_bytes;
        let mut lines = self.lines.clone();
        let mut accepted = 0;
        for (index, character) in text.char_indices() {
            let end = index + character.len_utf8();
            if character.len_utf8() > limits.max_total_bytes.saturating_sub(total_bytes) {
                break;
            }
            lines.push(&text[index..end]);
            if lines.count() > limits.max_total_lines {
                break;
            }
            total_bytes += character.len_utf8();
            accepted = end;
        }
        accepted
    }

    fn queue(&mut self, mut text: &str, now: Instant, progress: &mut impl FnMut(ArenaProgress)) {
        while !text.is_empty() {
            let available = STDOUT_DELTA_BYTES - self.pending.len();
            let length = utf8_prefix_len(text, available);
            if length == 0 {
                self.publish_next(now, progress);
                continue;
            }
            self.pending.push_str(&text[..length]);
            text = &text[length..];
            self.publish_due(now, progress);
        }
    }

    fn publish_due(&mut self, now: Instant, progress: &mut impl FnMut(ArenaProgress)) {
        let rate_window_open = self.last_emitted.is_none_or(|last_emitted| {
            now.checked_duration_since(last_emitted)
                .is_some_and(|elapsed| elapsed >= STDOUT_RATE_WINDOW)
        });
        if rate_window_open && !self.pending.is_empty() {
            self.publish_next(now, progress);
        }
        while self.pending.len() >= STDOUT_DELTA_BYTES {
            self.publish_next(now, progress);
        }
    }

    fn flush(&mut self, now: Instant, progress: &mut impl FnMut(ArenaProgress)) {
        while !self.pending.is_empty() {
            self.publish_next(now, progress);
        }
    }

    fn publish_next(&mut self, now: Instant, progress: &mut impl FnMut(ArenaProgress)) {
        let length = utf8_prefix_len(&self.pending, STDOUT_DELTA_BYTES);
        assert!(length > 0, "pending stdout must contain valid UTF-8");
        let remainder = self.pending.split_off(length);
        let text = std::mem::replace(&mut self.pending, remainder);
        self.last_emitted = Some(now);
        progress(ArenaProgress::StdoutDelta { text });
    }
}

impl ObservationAccumulator {
    fn new(limits: ObservationLimits) -> Self {
        Self {
            limits,
            total_bytes: 0,
            lines: LineCounter::default(),
            whole: Some(String::new()),
            truncated: None,
            live_stdout: None,
        }
    }

    fn with_live_stdout(limits: ObservationLimits) -> Self {
        Self {
            live_stdout: Some(LiveStdout::new()),
            ..Self::new(limits)
        }
    }

    fn push_live_stdout(
        &mut self,
        text: &str,
        now: Instant,
        progress: &mut impl FnMut(ArenaProgress),
    ) {
        self.push(text);
        if let Some(live_stdout) = &mut self.live_stdout {
            live_stdout.push(text, self.limits, now, progress);
        }
    }

    fn push(&mut self, text: &str) {
        self.total_bytes += text.len();
        self.lines.push(text);
        if let Some(truncated) = &mut self.truncated {
            truncated.tail.push(text);
            return;
        }
        let exceeds_limit = self.total_bytes > self.limits.max_total_bytes
            || self.lines.count() > self.limits.max_total_lines;
        if !exceeds_limit {
            self.whole
                .as_mut()
                .expect("untruncated observation has text")
                .push_str(text);
            return;
        }

        let whole = self.whole.take().expect("untruncated observation has text");
        let source = head_source(&whole, text, self.limits.max_head_bytes);
        let mut head = retained_head(&source, self.limits);
        if head.ends_with('\r') && source[head.len()..].starts_with('\n') {
            head.pop();
        }
        let head_ends_mid_line = !head.is_empty() && !ends_at_line_boundary(&head);
        let mut tail = Tail::new(self.limits, head_ends_mid_line);
        if head.len() < whole.len() {
            tail.push(&whole[head.len()..]);
            tail.push(text);
        } else {
            tail.push(&text[head.len() - whole.len()..]);
        }
        self.truncated = Some(TruncatedObservation {
            head,
            head_ends_mid_line,
            tail,
        });
    }

    fn finish(self) -> Observation {
        let total_lines = self.lines.count();
        if let Some(truncated) = self.truncated {
            let TruncatedObservation {
                head,
                head_ends_mid_line,
                tail,
            } = truncated;
            let head_lines = line_count(&head);
            let tail_lines = tail.line_count();
            let tail_starts_mid_line = !tail.is_empty() && tail.starts_mid_line;
            let tail = tail.finish();
            let omitted_lines = total_lines.saturating_sub(head_lines.saturating_add(tail_lines));
            let omitted_bytes = self
                .total_bytes
                .saturating_sub(head.len().saturating_add(tail.len()));
            return Observation {
                head,
                tail: Some(tail),
                head_lines,
                tail_lines,
                total_lines,
                total_bytes: self.total_bytes,
                omitted_lines,
                omitted_bytes,
                head_ends_mid_line,
                tail_starts_mid_line,
                partial_lines: head_ends_mid_line || tail_starts_mid_line,
            };
        }
        Observation {
            head: self.whole.expect("untruncated observation has text"),
            tail: None,
            head_lines: total_lines,
            tail_lines: 0,
            total_lines,
            total_bytes: self.total_bytes,
            omitted_lines: 0,
            omitted_bytes: 0,
            head_ends_mid_line: false,
            tail_starts_mid_line: false,
            partial_lines: false,
        }
    }
}

impl Tail {
    fn new(limits: ObservationLimits, starts_mid_line: bool) -> Self {
        Self {
            scalars: VecDeque::new(),
            bytes: 0,
            terminated_lines: 0,
            starts_mid_line,
            limits,
            #[cfg(test)]
            scalar_operations: 0,
        }
    }

    fn push(&mut self, text: &str) {
        for character in text.chars() {
            let ends_line = is_line_separator(character);
            let terminates_line = ends_line
                && !(character == '\n'
                    && matches!(
                        self.scalars.back(),
                        Some(Scalar {
                            character: '\r',
                            ..
                        })
                    ));
            self.bytes += character.len_utf8();
            self.terminated_lines += usize::from(terminates_line);
            self.scalars.push_back(Scalar {
                character,
                terminates_line,
                ends_line,
            });
            #[cfg(test)]
            {
                self.scalar_operations += 1;
            }
            self.trim_bytes();
            self.trim_lines();
        }
    }

    fn trim_bytes(&mut self) {
        while self.bytes > self.limits.max_tail_bytes {
            let scalar = self.discard_front().expect("tail has excess bytes");
            if scalar.character == '\r'
                && matches!(
                    self.scalars.front(),
                    Some(Scalar {
                        character: '\n',
                        ..
                    })
                )
            {
                self.discard_front().expect("CRLF tail has line feed");
            }
        }
    }

    fn trim_lines(&mut self) {
        while self.line_count() > self.limits.max_tail_lines {
            loop {
                let scalar = self.discard_front().expect("tail has excess lines");
                if scalar.terminates_line {
                    if scalar.character == '\r'
                        && matches!(
                            self.scalars.front(),
                            Some(Scalar {
                                character: '\n',
                                ..
                            })
                        )
                    {
                        self.discard_front().expect("CRLF tail has line feed");
                    }
                    break;
                }
            }
        }
    }

    fn discard_front(&mut self) -> Option<Scalar> {
        let scalar = self.scalars.pop_front()?;
        self.bytes -= scalar.character.len_utf8();
        self.terminated_lines -= usize::from(scalar.terminates_line);
        self.starts_mid_line = !is_line_boundary_after(
            scalar.character,
            self.scalars.front().map(|next| next.character),
        );
        #[cfg(test)]
        {
            self.scalar_operations += 1;
        }
        Some(scalar)
    }

    fn line_count(&self) -> usize {
        self.terminated_lines
            + usize::from(self.scalars.back().is_some_and(|scalar| !scalar.ends_line))
    }

    fn is_empty(&self) -> bool {
        self.scalars.is_empty()
    }

    fn finish(self) -> String {
        let mut text = String::with_capacity(self.bytes);
        for scalar in self.scalars {
            text.push(scalar.character);
        }
        text
    }
}

#[derive(Clone, Default)]
struct LineCounter {
    terminated: usize,
    current_nonempty: bool,
    previous_was_carriage_return: bool,
}

impl LineCounter {
    fn push(&mut self, text: &str) {
        for character in text.chars() {
            if character == '\n' && self.previous_was_carriage_return {
                self.previous_was_carriage_return = false;
                continue;
            }
            if is_line_separator(character) {
                self.terminated += 1;
                self.current_nonempty = false;
                self.previous_was_carriage_return = character == '\r';
            } else {
                self.current_nonempty = true;
                self.previous_was_carriage_return = false;
            }
        }
    }

    fn count(&self) -> usize {
        self.terminated + usize::from(self.current_nonempty)
    }
}

fn observation_from_text(text: String) -> Observation {
    let mut accumulator = ObservationAccumulator::new(OBSERVATION_LIMITS);
    accumulator.push(&text);
    accumulator.finish()
}

fn head_source(whole: &str, text: &str, max_head_bytes: usize) -> String {
    let mut source = String::new();
    for character in whole.chars().chain(text.chars()) {
        if source.len() >= max_head_bytes.saturating_add(4) {
            break;
        }
        source.push(character);
    }
    source
}

fn retained_head(text: &str, limits: ObservationLimits) -> String {
    let mut retained = 0;
    for (lines, line) in splitlines_keepends(text).into_iter().enumerate() {
        if lines == limits.max_head_lines || retained == limits.max_head_bytes {
            break;
        }
        let available = limits.max_head_bytes - retained;
        let take = utf8_prefix_len(line, available);
        retained += take;
        if take < line.len() {
            break;
        }
    }
    text[..retained].to_owned()
}

fn utf8_prefix_len(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn bounded_interruption_text(mut text: String) -> String {
    text.truncate(utf8_prefix_len(&text, INTERRUPTED_EVIDENCE_TEXT_LIMIT));
    text
}

fn splitlines_keepends(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        let end = if character == '\r' {
            if matches!(characters.peek(), Some((_, '\n'))) {
                let (next, newline) = characters.next().expect("peeked character exists");
                next + newline.len_utf8()
            } else {
                index + character.len_utf8()
            }
        } else if is_line_separator(character) {
            index + character.len_utf8()
        } else {
            continue;
        };
        lines.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn line_count(text: &str) -> usize {
    splitlines_keepends(text).len()
}

fn is_line_separator(character: char) -> bool {
    matches!(
        character,
        '\n' | '\r'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn ends_at_line_boundary(text: &str) -> bool {
    is_line_boundary(text, text.len())
}

fn is_line_boundary(text: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    is_line_boundary_after(
        text[..index].chars().next_back().expect("nonempty prefix"),
        text[index..].chars().next(),
    )
}

fn is_line_boundary_after(previous: char, next: Option<char>) -> bool {
    is_line_separator(previous) && !(previous == '\r' && next == Some('\n'))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, VecDeque};
    use std::env;
    use std::fs;
    use std::future::Future;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use monty_types::{MontyObject, MontyType, PrintStream};
    use tempfile::TempDir;
    use tokio::sync::{Semaphore, mpsc, oneshot, watch};
    use tokio_util::sync::CancellationToken;

    use super::{
        Arena, ArenaError, ChargeableBudget, Checkout, DriverError, Execution, ExecutionOutcome,
        INTERRUPTED_EVIDENCE_TEXT_LIMIT, InterruptedExecutionState, OBSERVATION_LIMITS,
        Observation, ObservationAccumulator, ObservationLimits, PendingCallMode, PythonError,
        RUNTIME_IDENTITY, RuntimeIdentity, StreamAccumulators, ToolCall, ToolCallSummary, ToolName,
        ToolTraceState, drive, is_unsafe_memory_error, is_unsafe_soft_memory_error,
        parse_soft_memory_limit, result_values_from_monty, tool_summary, update_budget_pause,
        validate_worker, validate_worker_with_timeout,
    };
    use crate::model::{
        Model, ModelFailure, ModelHistory, ModelProgress, ModelStep, ScriptedAction, ScriptedModel,
    };
    use crate::subagents::{
        AcceptedThinking, AgentInvocationContext, Caller, ChildFailure, ChildModelFactory,
        ChildParameters, ChildPayload, ChildResult, ChildRun, ChildRunner, Clock,
        CompleteChildRunner, RunnerFuture, SessionSubagents, SubagentCoordinator, SubagentPolicy,
        TaskOutcome, TaskState,
    };
    use crate::tools::{
        BatchTestGate, BlockingTestGate, CommandTestGate, ReadTestGate, ToolHost, ToolResult,
        ToolValue, WholeFileAuthorisations, WriteTestHook,
    };

    const TEST_LIMITS: ObservationLimits = ObservationLimits {
        max_total_bytes: 100,
        max_total_lines: 3,
        max_head_bytes: 100,
        max_head_lines: 2,
        max_tail_bytes: 100,
        max_tail_lines: 1,
    };

    fn worker_path() -> PathBuf {
        env::var_os("MONTY_BIN")
            .map(PathBuf::from)
            .expect("MONTY_BIN must point to the monty-runtime 0.0.22 worker")
    }

    async fn arena(workspace: &TempDir) -> Arena {
        Arena::new(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena")
    }

    async fn arena_with_collection_limits(
        workspace: &TempDir,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> Arena {
        Arena::new(
            worker_path(),
            ToolHost::with_collection_limits(workspace.path(), max_items, max_payload_bytes)
                .await
                .expect("open tool host"),
        )
        .await
        .expect("create arena")
    }

    async fn arena_with_read_gate(workspace: &TempDir) -> (Arena, ReadTestGate) {
        let gate = ReadTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_test_gate(gate.clone());
        let arena = Arena::new(worker_path(), tool_host)
            .await
            .expect("create arena");
        (arena, gate)
    }

    async fn arena_with_read_gate_and_clock(
        workspace: &TempDir,
        clock: Arc<InvocationClock>,
    ) -> (Arena, ReadTestGate) {
        let gate = ReadTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_test_gate(gate.clone());
        let arena = Arena::with_limits_and_clock(
            worker_path(),
            tool_host,
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock,
        )
        .await
        .expect("create arena");
        (arena, gate)
    }

    async fn arena_with_batch_gate(workspace: &TempDir) -> (Arena, BatchTestGate) {
        let gate = BatchTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_batch_test_gate(gate.clone());
        let arena = Arena::new(worker_path(), tool_host)
            .await
            .expect("create arena");
        (arena, gate)
    }

    async fn arena_with_command_gate(
        workspace: &TempDir,
        execution_timeout: Duration,
    ) -> (Arena, Arc<CommandTestGate>) {
        let gate = CommandTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_command_test_gate(gate.clone());
        let arena = Arena::with_limits(
            worker_path(),
            tool_host,
            execution_timeout,
            Some(64 * 1024 * 1024),
        )
        .await
        .expect("create arena");
        (arena, gate)
    }

    async fn arena_with_blocking_read_gate(workspace: &TempDir) -> (Arena, BlockingTestGate) {
        let gate = BlockingTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_blocking_read_test_gate(gate.clone());
        let arena = Arena::new(worker_path(), tool_host)
            .await
            .expect("create arena");
        (arena, gate)
    }

    async fn arena_with_limits(
        workspace: &TempDir,
        execution_timeout: Duration,
        max_memory: Option<usize>,
    ) -> Arena {
        Arena::with_limits(
            worker_path(),
            ToolHost::new(workspace.path())
                .await
                .expect("open tool host"),
            execution_timeout,
            max_memory,
        )
        .await
        .expect("create arena")
    }

    async fn execute_without_cancellation(
        arena: &mut Arena,
        code: &str,
    ) -> Result<Execution, ArenaError> {
        let cancellation = CancellationToken::new();
        arena.execute(code, &cancellation).await
    }

    fn complete(execution: &Execution) -> Option<&Observation> {
        let ExecutionOutcome::Complete { result } = &execution.outcome else {
            panic!("expected completed execution, got {:?}", execution.outcome);
        };
        result.as_ref()
    }

    fn python_error(execution: &Execution) -> &PythonError {
        let ExecutionOutcome::PythonError(error) = &execution.outcome else {
            panic!("expected Python error, got {:?}", execution.outcome);
        };
        error
    }

    fn gathered_reads(count: usize) -> String {
        let calls = (0..count)
            .map(|index| format!("read('file-{index}')"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("import asyncio\nawait asyncio.gather({calls})")
    }

    fn has_authorisation(arena: &Arena, path: &str) -> bool {
        arena
            .checkpoint
            .authorisations
            .authorisation(Path::new(path))
            .is_some()
    }

    struct InvocationRunner {
        started: mpsc::UnboundedSender<oneshot::Sender<Result<ChildResult, ChildFailure>>>,
    }

    struct CleanupInvocationRunner {
        started: mpsc::UnboundedSender<oneshot::Sender<()>>,
    }

    struct CallerCapturingInvocationRunner {
        callers: mpsc::UnboundedSender<Caller>,
    }

    #[derive(Clone)]
    struct InvocationClock {
        time: Arc<Mutex<(Instant, i64)>>,
        changed: watch::Sender<u64>,
        paused: Arc<Semaphore>,
        coordinator_advance: Arc<Mutex<Option<Duration>>>,
    }

    impl InvocationClock {
        fn new() -> Self {
            Self {
                time: Arc::new(Mutex::new((Instant::now(), 0))),
                changed: watch::channel(0).0,
                paused: Arc::new(Semaphore::new(0)),
                coordinator_advance: Arc::new(Mutex::new(None)),
            }
        }

        async fn wait_until_budget_paused(&self) {
            self.paused
                .clone()
                .acquire_owned()
                .await
                .expect("test clock remains open")
                .forget();
        }

        fn advance(&self, duration: Duration) {
            let mut value = self.time.lock().unwrap();
            value.0 += duration;
            value.1 += i64::try_from(duration.as_millis()).unwrap();
            self.changed
                .send_modify(|revision| *revision = revision.wrapping_add(1));
        }

        fn advance_next_coordinator_now(&self, duration: Duration) {
            *self.coordinator_advance.lock().unwrap() = Some(duration);
        }
    }

    impl Clock for InvocationClock {
        fn now(&self) -> Instant {
            if let Some(duration) = self.coordinator_advance.lock().unwrap().take() {
                self.advance(duration);
            }
            self.time.lock().unwrap().0
        }

        fn unix_time_ms(&self) -> i64 {
            self.time.lock().unwrap().1
        }
    }

    impl super::ArenaClock for InvocationClock {
        fn now(&self) -> Instant {
            self.time.lock().unwrap().0
        }

        fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            let clock = self.clone();
            Box::pin(async move {
                let mut changed = clock.changed.subscribe();
                loop {
                    if <InvocationClock as super::ArenaClock>::now(&clock) >= deadline {
                        return;
                    }
                    changed
                        .changed()
                        .await
                        .expect("test clock sender remains open");
                }
            })
        }

        fn test_budget_paused(&self) {
            self.paused.add_permits(1);
        }
    }

    struct SelectionRunner {
        selections: mpsc::UnboundedSender<crate::config::ModelSelection>,
    }

    impl ChildRunner<ChildPayload> for SelectionRunner {
        fn run(
            &self,
            child: ChildRun<ChildPayload>,
            _cancellation: CancellationToken,
        ) -> RunnerFuture {
            self.selections
                .send(child.parameters().selection())
                .expect("test receives child selection");
            Box::pin(async { Ok(ChildResult::new("done".to_owned(), BTreeMap::new())) })
        }
    }

    struct SelectedScriptedModel {
        model: ScriptedModel,
        selection: crate::config::ModelSelection,
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
                step.selection = Some(self.selection);
                Ok(step)
            }
        }
    }

    #[derive(Clone)]
    struct ScriptedInvocationFactory {
        scripts: Arc<Mutex<VecDeque<Vec<ScriptedAction>>>>,
        selection: crate::config::ModelSelection,
    }

    impl ScriptedInvocationFactory {
        fn new(scripts: impl IntoIterator<Item = Vec<ScriptedAction>>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
                selection: crate::config::ModelSelection::new(
                    crate::config::DEFAULT_MODEL,
                    crate::config::DEFAULT_MODEL.default_thinking,
                )
                .unwrap(),
            }
        }
    }

    impl ChildModelFactory for ScriptedInvocationFactory {
        type Model = SelectedScriptedModel;

        fn create(&self, _prompt: String, selection: crate::config::ModelSelection) -> Self::Model {
            assert_eq!(selection, self.selection);
            SelectedScriptedModel {
                model: ScriptedModel::new(
                    self.scripts
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("test configures one child script"),
                ),
                selection,
            }
        }
    }

    impl ChildRunner<ChildPayload> for InvocationRunner {
        fn run(
            &self,
            _child: ChildRun<ChildPayload>,
            cancellation: CancellationToken,
        ) -> RunnerFuture {
            let (finish, result) = oneshot::channel();
            self.started
                .send(finish)
                .expect("test receives started child");
            Box::pin(async move {
                tokio::select! {
                    result = result => result.expect("test settles started child"),
                    _ = cancellation.cancelled() => Err(ChildFailure::new("child cancelled")),
                }
            })
        }
    }

    impl ChildRunner<ChildPayload> for CleanupInvocationRunner {
        fn run(
            &self,
            _child: ChildRun<ChildPayload>,
            cancellation: CancellationToken,
        ) -> RunnerFuture {
            let (release, cleanup) = oneshot::channel();
            self.started
                .send(release)
                .expect("test receives started child");
            Box::pin(async move {
                cancellation.cancelled().await;
                cleanup.await.expect("test releases child cleanup");
                Err(ChildFailure::new("child cancelled"))
            })
        }
    }

    impl ChildRunner<ChildPayload> for CallerCapturingInvocationRunner {
        fn run(
            &self,
            child: ChildRun<ChildPayload>,
            cancellation: CancellationToken,
        ) -> RunnerFuture {
            self.callers
                .send(child.caller().clone())
                .expect("test receives child caller");
            Box::pin(async move {
                cancellation.cancelled().await;
                Err(ChildFailure::new("child cancelled"))
            })
        }
    }

    async fn invocation_context() -> (
        AgentInvocationContext,
        mpsc::UnboundedReceiver<oneshot::Sender<Result<ChildResult, ChildFailure>>>,
    ) {
        let (started, receiver) = mpsc::unbounded_channel();
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let coordinator: SessionSubagents = SubagentCoordinator::new(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(InvocationRunner { started }),
        );
        let caller = coordinator.root_caller().await;
        (
            AgentInvocationContext::new(caller, coordinator).with_accepted_thinking(
                AcceptedThinking::new(selection, snapshot.settings().clone(), None),
            ),
            receiver,
        )
    }

    async fn invocation_context_with_clock(
        clock: Arc<InvocationClock>,
    ) -> (
        AgentInvocationContext,
        SessionSubagents,
        mpsc::UnboundedReceiver<oneshot::Sender<Result<ChildResult, ChildFailure>>>,
    ) {
        let (started, receiver) = mpsc::unbounded_channel();
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let coordinator: SessionSubagents = SubagentCoordinator::with_clock(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(InvocationRunner { started }),
            clock,
        );
        let context =
            AgentInvocationContext::new(coordinator.root_caller().await, coordinator.clone())
                .with_accepted_thinking(AcceptedThinking::new(
                    selection,
                    snapshot.settings().clone(),
                    None,
                ));
        (context, coordinator, receiver)
    }

    async fn complete_invocation_context(
        scripts: impl IntoIterator<Item = Vec<ScriptedAction>>,
    ) -> AgentInvocationContext {
        complete_invocation_context_with_coordinator(scripts)
            .await
            .0
    }

    async fn complete_invocation_context_with_coordinator(
        scripts: impl IntoIterator<Item = Vec<ScriptedAction>>,
    ) -> (AgentInvocationContext, SessionSubagents) {
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let coordinator: SessionSubagents = SubagentCoordinator::new(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(CompleteChildRunner::new(
                "canonical base".to_owned(),
                ScriptedInvocationFactory::new(scripts),
            )),
        );
        let context =
            AgentInvocationContext::new(coordinator.root_caller().await, coordinator.clone())
                .with_accepted_thinking(AcceptedThinking::new(
                    selection,
                    snapshot.settings().clone(),
                    None,
                ));
        (context, coordinator)
    }

    async fn execute_with_invocation(
        arena: &mut Arena,
        context: &AgentInvocationContext,
        code: &str,
    ) -> Execution {
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        arena
            .execute_with_progress_and_context(
                code,
                &CancellationToken::new(),
                &mut |_| {},
                Some(context),
                Some(selection),
            )
            .await
            .unwrap()
    }

    fn assert_interrupted_batch_trace(tool_calls: &[ToolCall]) {
        assert_eq!(tool_calls.len(), 18);
        assert_eq!(
            tool_calls
                .iter()
                .filter(|call| matches!(call.summary, ToolCallSummary::Count(_)))
                .count(),
            1
        );
        assert_eq!(
            tool_calls
                .iter()
                .filter(|call| matches!(call.summary, ToolCallSummary::Pending))
                .count(),
            17
        );
    }

    fn observe(text: &str, limits: ObservationLimits) -> Observation {
        let mut accumulator = ObservationAccumulator::new(limits);
        accumulator.push(text);
        accumulator.finish()
    }

    fn assert_failed_without_count_label_panic(tool: ToolName, result: &ToolResult) {
        let summary = std::panic::catch_unwind(|| {
            let summary = tool_summary(tool, result);
            if let ToolCallSummary::Count(count) = summary {
                tool.count_label(count);
            }
            summary
        })
        .expect("unexpected successful shape must not reach count_label");
        assert_eq!(summary, ToolCallSummary::Failed);
    }

    #[test]
    fn tool_summary_rejects_malformed_run_results_without_count_labels() {
        let result = ToolResult::without_effect(ToolValue::None);

        for value in [
            ToolValue::None,
            ToolValue::String("unexpected".to_owned()),
            ToolValue::List(vec![]),
            ToolValue::Dictionary(Default::default()),
            ToolValue::Dictionary(
                [("exit_code".to_owned(), ToolValue::String("0".to_owned()))].into(),
            ),
        ] {
            let mut result = result.clone();
            result.result = Ok(value);
            assert_failed_without_count_label_panic(ToolName::Run, &result);
        }
    }

    #[test]
    fn tool_summary_rejects_mutation_successes_without_statuses() {
        let result = ToolResult::without_effect(ToolValue::None);

        for tool in [ToolName::Write, ToolName::Edit] {
            let mut result = result.clone();
            result.result = Ok(ToolValue::None);
            assert_failed_without_count_label_panic(tool, &result);
        }
    }

    #[test]
    fn result_value_converter_rejects_unrepresentable_transport_values_without_partial_exports() {
        let values = [
            (
                "function",
                MontyObject::Function {
                    name: "external".to_owned(),
                    docstring: None,
                },
            ),
            (
                "class",
                MontyObject::Type(MontyType::Instance(Box::new(monty_types::MontyClassType {
                    name: "Unsupported".to_owned(),
                    id: monty_types::MontyUuid::from_u128(1),
                    host_defined: true,
                    is_dataclass: false,
                    attrs: Vec::new().into(),
                }))),
            ),
            ("bytes", MontyObject::Bytes(b"bytes".to_vec())),
            ("big integer", MontyObject::BigInt(Default::default())),
            ("repr", MontyObject::Repr("unsupported".to_owned())),
            ("cycle", MontyObject::Cycle(1, "[...]".to_owned())),
        ];

        for (kind, value) in values {
            let error = result_values_from_monty(MontyObject::dict(vec![
                (MontyObject::String("valid".to_owned()), MontyObject::Int(1)),
                (MontyObject::String("invalid".to_owned()), value),
            ]))
            .unwrap_err();
            assert!(
                error.to_string().contains("unsupported"),
                "{kind} should not produce a partial export: {error}"
            );
        }

        let error = super::ResultValuesError::export("é".repeat(super::RESULT_VALUES_ERROR_BYTES));
        let text = error.to_string();
        assert!(
            text.len() <= "result_values export failed: ".len() + super::RESULT_VALUES_ERROR_BYTES
        );
        assert!(text.is_char_boundary(text.len()));
    }

    #[test]
    fn tool_trace_preserves_admission_order_with_safe_summaries() {
        let fixture_path = "secrets/fixture-path";
        let fixture_error = "fixture error must not be retained";
        let mut trace = ToolTraceState::default();
        let first = trace.admit(ToolName::Read);
        let second = trace.admit(ToolName::Read);

        assert_eq!(
            trace.calls(),
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                },
            ]
        );

        trace.settle(second, ToolCallSummary::Failed);
        trace.settle(first, ToolCallSummary::Count(2));

        assert_eq!(
            trace.calls(),
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(2),
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Failed,
                },
            ]
        );
        let retained = format!("{trace:?}");
        assert!(!retained.contains(fixture_path));
        assert!(!retained.contains(fixture_error));
    }

    #[tokio::test]
    async fn run_call_trace_records_its_numeric_exit() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "result = await run('exit 7')\nresult.exit_code == 7 and result.stdout == '' and result.stderr == ''",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Exit(7),
            }]
        );
    }

    #[tokio::test]
    async fn run_returns_command_results_for_zero_nonzero_signal_and_stdin_eof() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "zero = await run(\"printf stdout; printf stderr >&2\")\nnonzero = await run(\"printf nonzero; printf failure >&2; exit 9\")\nsignalled = await run('kill -9 $$')\nstdin = await run('cat')\ntype(zero).__name__ == 'CommandResult' and zero.exit_code == 0 and zero.stdout == 'stdout' and zero.stderr == 'stderr' and nonzero.exit_code == 9 and nonzero.stdout == 'nonzero' and nonzero.stderr == 'failure' and signalled.exit_code is None and stdin.exit_code == 0 and stdin.stdout == '' and stdin.stderr == ''",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(9),
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Signal,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_maps_malformed_calls_and_cwd_failures_to_builtin_exceptions() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("real/sub")).unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        std::os::unix::fs::symlink("real", workspace.path().join("link")).unwrap();
        let mut arena = arena(&workspace).await;

        for (call, expected) in [
            ("await run()", "TypeError"),
            ("await run(1)", "TypeError"),
            ("await run('printf ignored', None)", "TypeError"),
            ("await run('printf ignored', '.', '.')", "TypeError"),
            ("await run('printf ignored', '/tmp')", "PermissionError"),
            ("await run('printf ignored', 'real/..')", "PermissionError"),
            (
                "await run('printf ignored', 'missing')",
                "FileNotFoundError",
            ),
            ("await run('printf ignored', 'file')", "NotADirectoryError"),
            ("await run('printf ignored', 'link')", "PermissionError"),
            ("await run('printf ignored', 'link/sub')", "PermissionError"),
        ] {
            let execution = execute_without_cancellation(
                &mut arena,
                &format!(
                    "result = 'allowed'\ntry:\n    {call}\nexcept Exception as error:\n    result = type(error).__name__\nresult"
                ),
            )
            .await
            .unwrap();
            assert_eq!(complete(&execution).unwrap().head, format!("'{expected}'"));
        }
    }

    #[tokio::test]
    async fn command_output_overflow_keeps_pre_and_post_catch_python_state_and_side_effects() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "before = 41\nresult = 'unassigned'\ntry:\n    result = await run(\"printf side-effect > effect; dd if=/dev/zero bs=8388609 count=1 2>/dev/null\")\nexcept RuntimeError as error:\n    error_name = type(error).__name__\n    after = before + 1\nerror_name == 'RuntimeError' and result == 'unassigned' and before == 41 and after == 42",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Failed,
            }]
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("effect")).unwrap(),
            "side-effect"
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "before + after")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "83"
        );
    }

    #[tokio::test]
    async fn gathered_commands_admit_concurrently_and_associate_reversed_settlement_by_call_id() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "import asyncio\nresults = await asyncio.gather(run(\"printf first\"), run(\"printf second\"))\nresults[0].stdout == 'first' and results[1].stdout == 'second'",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = &mut execution => panic!("execution finished before both commands started: {result:?}"),
        }
        let call_ids = gate.started_call_ids();
        assert_eq!(call_ids.len(), 2);
        assert_eq!(gate.active_call_ids().len(), 2);
        gate.release(call_ids[1]);
        tokio::select! {
            _ = gate.wait_until_driver_settled() => {}
            result = &mut execution => panic!("execution finished before the reversed command settled: {result:?}"),
        }
        assert_eq!(gate.driver_settled_call_ids(), [call_ids[1]]);
        gate.release(call_ids[0]);
        tokio::select! {
            _ = gate.wait_until_driver_settled() => {}
            result = &mut execution => panic!("execution finished before the first command settled: {result:?}"),
        }

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
            ]
        );
        assert_eq!(gate.resume_futures_calls(), 1);
    }

    #[tokio::test]
    async fn gathered_commands_settle_a_successful_sibling_after_an_ordinary_command_error() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "import asyncio\nawait asyncio.gather(run('printf ignored', 'missing'), run(\"printf sibling > sibling\"))",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = &mut execution => panic!("execution finished before both commands started: {result:?}"),
        }
        for call_id in gate.started_call_ids() {
            gate.release(call_id);
        }

        let execution = execution.await.unwrap();
        assert!(python_error(&execution).text.contains("FileNotFoundError"));
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Failed,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
            ]
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("sibling")).unwrap(),
            "sibling"
        );
        assert_eq!(gate.resume_futures_calls(), 1);
    }

    #[tokio::test]
    async fn run_evidence_stays_ordered_path_free_and_updates_pending_rows_in_place() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut progress = Vec::new();
        let execution = {
            let mut capture = |event| progress.push(event);
            let mut execution = Box::pin(arena.execute_with_progress(
                "import asyncio\nawait asyncio.gather(run(\"COMMAND_SENTINEL; printf OUTPUT_SENTINEL; printf ERROR_SENTINEL >&2\"), run('printf SECOND_OUTPUT_SENTINEL; kill -9 $$'), run('printf ignored', 'CWD_SENTINEL'))",
                &cancellation,
                &mut capture,
            ));

            tokio::select! {
                _ = gate.wait_until_calls_started(3) => {}
                result = &mut execution => panic!("execution finished before all commands started: {result:?}"),
            }
            for call_id in gate.started_call_ids() {
                gate.release(call_id);
            }
            execution.await.unwrap()
        };

        assert!(python_error(&execution).text.contains("FileNotFoundError"));
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Signal,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Failed,
                },
            ]
        );
        let updates = progress
            .iter()
            .filter_map(|event| match event {
                super::ArenaProgress::ToolUpdated { token, call } => Some((*token, call.clone())),
                super::ArenaProgress::RunPreviewUpdated { .. }
                | super::ArenaProgress::StdoutDelta { .. }
                | super::ArenaProgress::StdoutTruncated => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 6);
        assert!(updates[..3].iter().all(
            |(_, call)| call.tool == ToolName::Run && call.summary == ToolCallSummary::Pending
        ));
        for (token, _) in &updates[..3] {
            assert_eq!(
                updates[3..]
                    .iter()
                    .filter(|(settled, _)| settled == token)
                    .count(),
                1
            );
        }
        assert!(
            updates[3..]
                .iter()
                .any(|(_, call)| call.summary == ToolCallSummary::Exit(0))
        );
        assert!(
            updates[3..]
                .iter()
                .any(|(_, call)| call.summary == ToolCallSummary::Signal)
        );
        assert!(
            updates[3..]
                .iter()
                .any(|(_, call)| call.summary == ToolCallSummary::Failed)
        );
        let successful_token = updates[0].0;
        assert!(progress.iter().any(|event| matches!(
            event,
            super::ArenaProgress::RunPreviewUpdated {
                token,
                preview: super::RunPreview { command, stdout },
            } if *token == successful_token
                && command.contains("COMMAND_SENTINEL")
                && stdout.contains("OUTPUT_SENTINEL")
                && !stdout.contains("ERROR_SENTINEL")
        )));
        let signalled_token = updates[1].0;
        assert!(progress.iter().any(|event| matches!(
            event,
            super::ArenaProgress::RunPreviewUpdated {
                token,
                preview: super::RunPreview { command, stdout },
            } if *token == signalled_token
                && command.contains("SECOND_OUTPUT_SENTINEL")
                && stdout.contains("SECOND_OUTPUT_SENTINEL")
        )));
        let retained = format!("{:?}{updates:?}", execution.tool_calls);
        for sentinel in [
            "COMMAND_SENTINEL",
            "CWD_SENTINEL",
            "OUTPUT_SENTINEL",
            "SECOND_OUTPUT_SENTINEL",
            "ERROR_SENTINEL",
        ] {
            assert!(!retained.contains(sentinel), "retained {sentinel}");
        }
    }

    #[tokio::test]
    async fn validates_the_configured_worker_version() {
        let identity = validate_worker(&worker_path())
            .await
            .expect("validate worker");

        assert_eq!(identity, RuntimeIdentity(RUNTIME_IDENTITY.to_owned()));
    }

    #[cfg(unix)]
    async fn process_exited(pid: libc::pid_t) -> bool {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok()
    }

    #[cfg(unix)]
    async fn worker_pid(pid_file: &std::path::Path) -> libc::pid_t {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(pid) = fs::read_to_string(pid_file) {
                    return pid.parse().unwrap();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("hanging worker should record its process ID")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kills_a_worker_that_times_out_during_version_validation() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("monty");
        let pid_file = directory.path().join("pid");
        fs::write(
            &worker,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"${0%/*}/pid\"\nexec sleep 60\n",
        )
        .unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let worker_for_validation = worker.clone();
        let validation = tokio::spawn(async move {
            validate_worker_with_timeout(&worker_for_validation, Duration::from_secs(1)).await
        });
        let pid = worker_pid(&pid_file).await;
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "hanging worker should start"
        );
        let error = validation.await.unwrap().unwrap_err();
        let exited = process_exited(pid).await;
        if !exited {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            assert!(process_exited(pid).await, "clean up timed-out worker");
        }

        assert!(matches!(error, ArenaError::WorkerVersionTimedOut));
        assert!(exited, "timed-out worker {pid} is still running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_worker_with_a_mismatched_version() {
        let directory = tempfile::tempdir().unwrap();
        let worker = directory.path().join("monty");
        fs::write(&worker, "#!/bin/sh\nprintf 'monty-runtime 9.9.9\\n'\n").unwrap();
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o755)).unwrap();

        let error = validate_worker(&worker).await.unwrap_err();

        assert!(matches!(error, ArenaError::WorkerVersionMismatch { .. }));
    }

    #[tokio::test]
    async fn rejects_a_non_absolute_worker_path() {
        let error = validate_worker(std::path::Path::new("monty"))
            .await
            .unwrap_err();

        assert!(matches!(error, ArenaError::WorkerPathNotAbsolute));
    }

    #[tokio::test]
    async fn production_arena_sets_a_chargeable_execution_budget_and_memory_limit() {
        let workspace = tempfile::tempdir().unwrap();
        let arena = arena(&workspace).await;
        let limits = arena
            .repl_config
            .limits
            .as_ref()
            .expect("arena should configure Monty resource limits");

        assert_eq!(
            arena.execution_timeout,
            crate::config::constants::ARENA_EXECUTION_TIMEOUT
        );
        assert_eq!(limits.max_duration, None);
        assert_eq!(
            limits.max_memory,
            Some(crate::config::constants::ARENA_MAX_MEMORY_BYTES)
        );
    }

    #[tokio::test]
    async fn child_clones_an_idle_checkpoint_with_empty_exports_and_independent_state() {
        let workspace = tempfile::tempdir().unwrap();
        let mut parent = arena(&workspace).await;
        execute_without_cancellation(
            &mut parent,
            "values = [1]\ndef helper():\n    return values[0]\nalias = helper\nresult_values = {'parent': 'kept'}",
        )
        .await
        .unwrap();
        let checkpoint = parent.checkpoint();

        let mut child = parent.child(&checkpoint, false).await.unwrap();

        assert_eq!(
            child.collect_result_values().await.unwrap(),
            BTreeMap::new()
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(
                    &mut child,
                    "values[0] = 2\nresult_values = {'child': alias()}\nalias()",
                )
                .await
                .unwrap(),
            )
            .unwrap()
            .head,
            "2"
        );
        assert_eq!(
            child.collect_result_values().await.unwrap(),
            BTreeMap::from([("child".to_owned(), ToolValue::Integer(2))])
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut parent, "alias()")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "1"
        );
        assert_eq!(
            parent.collect_result_values().await.unwrap(),
            BTreeMap::from([("parent".to_owned(), ToolValue::String("kept".to_owned()))])
        );
    }

    #[tokio::test]
    async fn cancelled_child_export_restores_its_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        let collection_gate = super::CollectionTestGate::new();
        let parent = arena(&workspace)
            .await
            .with_collection_test_gate(collection_gate.clone());
        let mut child = parent.child(&parent.checkpoint(), false).await.unwrap();
        let cancellation = CancellationToken::new();
        let mut collection = Box::pin(child.collect_result_values_with_cancellation(&cancellation));

        tokio::select! {
            _ = collection_gate.wait_until_collection_started() => {}
            result = &mut collection => panic!("collection finished before cancellation: {result:?}"),
        }
        cancellation.cancel();
        collection_gate.resume_collection();

        assert!(matches!(
            collection.await,
            Err(super::ResultValuesError::Cancelled)
        ));
        assert_eq!(
            child.collect_result_values().await.unwrap(),
            BTreeMap::new()
        );
    }

    #[tokio::test]
    async fn children_exclude_later_parent_state_and_keep_freshness_separate() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let mut parent = arena(&workspace).await;
        execute_without_cancellation(&mut parent, "await read('file')")
            .await
            .unwrap();
        let checkpoint = parent.checkpoint();
        execute_without_cancellation(&mut parent, "after_checkpoint = 'parent'")
            .await
            .unwrap();

        let mut child = parent.child(&checkpoint, false).await.unwrap();
        assert_eq!(
            child.tool_host.prompt_reference(),
            crate::tools::prompt_reference()
        );
        assert!(
            python_error(
                &execute_without_cancellation(&mut child, "after_checkpoint")
                    .await
                    .unwrap()
            )
            .text
            .contains("NameError")
        );
        fs::write(workspace.path().join("file"), "external").unwrap();
        assert!(
            python_error(
                &execute_without_cancellation(&mut child, "await write('file', 'child')")
                    .await
                    .unwrap()
            )
            .text
            .contains("RuntimeError")
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "external"
        );
        execute_without_cancellation(&mut child, "await read('file')")
            .await
            .unwrap();
        execute_without_cancellation(&mut child, "await write('file', 'child')")
            .await
            .unwrap();

        assert!(has_authorisation(&child, "file"));
        assert!(has_authorisation(&parent, "file"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "child"
        );
        assert!(
            python_error(
                &execute_without_cancellation(&mut parent, "await write('file', 'parent')")
                    .await
                    .unwrap()
            )
            .text
            .contains("changed")
        );
    }

    #[tokio::test]
    async fn python_starts_refreshes_and_waits_for_a_subagent() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();

        let execution = arena
            .execute_with_progress_and_context(
                "task = await start_subagent('child work')\ncontinued = 'yes'\ntask.status.state",
                &cancellation,
                &mut |_| {},
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'starting'");
        assert_eq!(execution.stdout.total_bytes, 0);
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::StartSubagent,
                summary: ToolCallSummary::Subagent("0".to_owned()),
            }]
        );
        let finish = started.recv().await.expect("child starts");

        let execution = arena
            .execute_with_progress_and_context(
                "(continued, (await task.refresh()).state)",
                &cancellation,
                &mut |_| {},
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "('yes', 'running')");
        assert_eq!(execution.stdout.total_bytes, 0);
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::RefreshSubagent,
                summary: ToolCallSummary::Subagent("0".to_owned()),
            }]
        );

        finish
            .send(Ok(ChildResult::new(
                "done".to_owned(),
                BTreeMap::from([("answer".to_owned(), ToolValue::Integer(42))]),
            )))
            .unwrap();
        let execution = arena
            .execute_with_progress_and_context(
                "result = await task.wait()\n(continued, result['message'], result['values']['answer'], task.status.state)",
                &cancellation,
                &mut |_| {},
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        assert_eq!(
            complete(&execution).unwrap().head,
            "('yes', 'done', 42, 'completed')"
        );
        assert_eq!(execution.stdout.total_bytes, 0);
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::WaitSubagent,
                summary: ToolCallSummary::Subagent("0".to_owned()),
            }]
        );
    }

    #[tokio::test]
    async fn python_reconnects_and_repeats_waits_and_runs_subagents() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "task = await start_subagent('first child')\nrecovered = await get_subagent(task.id)\nrecovered.id == task.id",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "True");
        let finish = started.recv().await.expect("first child starts");
        finish
            .send(Ok(ChildResult::new(
                "first done".to_owned(),
                BTreeMap::from([("answer".to_owned(), ToolValue::Integer(42))]),
            )))
            .unwrap();

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "import asyncio\nfirst, second = await asyncio.gather(task.wait(), recovered.wait())\nfirst['values']['answer'] = 0\nsecond['values']['answer']",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "42");

        let mut run = Box::pin(execute_with_invocation(
            &mut arena,
            &context,
            "result = await run_subagent('second child')\nresult['message']",
        ));
        let finish = tokio::select! {
            finish = started.recv() => finish.expect("second child starts"),
            execution = &mut run => panic!("run_subagent completed before its child: {execution:?}"),
        };
        finish
            .send(Ok(ChildResult::new(
                "second done".to_owned(),
                BTreeMap::new(),
            )))
            .unwrap();
        let execution = run.await;
        assert_eq!(complete(&execution).unwrap().head, "'second done'");
    }

    #[tokio::test]
    async fn completed_children_survive_parent_result_delivery_failures() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, coordinator) = complete_invocation_context_with_coordinator([vec![
            ScriptedAction::Code {
                source: "result_values['answer'] = 42".to_owned(),
            },
            ScriptedAction::Message {
                text: "child completed".to_owned(),
            },
        ]])
        .await;

        let failed = execute_with_invocation(
            &mut arena,
            &context,
            "task = await start_subagent('complete before delivery failure')\nsaved_id = task.id\npublished = await task.wait()\nraise Exception('parent delivery failure')",
        )
        .await;
        assert!(
            python_error(&failed)
                .text
                .contains("parent delivery failure")
        );
        assert!(matches!(
            coordinator.records().await.as_slice(),
            [record] if matches!(
                record.outcome(),
                Some(TaskOutcome::Completed(result))
                    if result.message() == "child completed"
                        && result.values().get("answer") == Some(&ToolValue::Integer(42))
            )
        ));

        let repeated = execute_with_invocation(
            &mut arena,
            &context,
            "task = await get_subagent(saved_id)\nfirst = await task.wait()\nfirst['values']['answer'] = 0\nsecond = await task.wait()\n(task.status.state, second['message'], second['values']['answer'])",
        )
        .await;
        assert_eq!(
            complete(&repeated).unwrap().head,
            "('completed', 'child completed', 42)"
        );
        assert!(matches!(
            coordinator.records().await.as_slice(),
            [record] if matches!(record.outcome(), Some(TaskOutcome::Completed(_)))
        ));
    }

    #[tokio::test]
    async fn subagent_prompt_limits_accept_exact_ascii_and_unicode_bytes() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;
        let limit = crate::config::constants::MAX_PROMPT_BYTES;

        for (prompt, expected_id) in [
            ("a".repeat(limit), "0"),
            ("🦀".repeat(limit / '🦀'.len_utf8()), "1"),
        ] {
            let execution = execute_with_invocation(
                &mut arena,
                &context,
                &format!("task = await start_subagent('{prompt}')\ntask.id"),
            )
            .await;
            assert_eq!(
                complete(&execution).unwrap().head,
                format!("'{expected_id}'")
            );
            let _finish = started.recv().await.expect("exact-byte child starts");

            let cancelled =
                execute_with_invocation(&mut arena, &context, "(await task.cancel()).state").await;
            assert_eq!(complete(&cancelled).unwrap().head, "'cancelled'");
        }

        for prompt in [
            "a".repeat(limit + 1),
            "🦀".repeat(limit / '🦀'.len_utf8() + 1),
        ] {
            let execution = execute_with_invocation(
                &mut arena,
                &context,
                &format!("caught = None\ntry:\n    await start_subagent('{prompt}')\nexcept ValueError:\n    caught = 'ValueError'\ncaught"),
            )
            .await;
            assert_eq!(complete(&execution).unwrap().head, "'ValueError'");
            assert!(started.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn assignment_only_wait_does_not_emit_child_results() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;

        execute_with_invocation(&mut arena, &context, "task = await start_subagent('child')").await;
        let finish = started.recv().await.expect("child starts");
        finish
            .send(Ok(ChildResult::new(
                "hidden child message".to_owned(),
                BTreeMap::from([("hidden".to_owned(), ToolValue::Integer(42))]),
            )))
            .unwrap();

        let execution =
            execute_with_invocation(&mut arena, &context, "result = await task.wait()").await;
        assert_eq!(complete(&execution), None);
        assert_eq!(execution.stdout.total_bytes, 0);
    }

    #[tokio::test]
    async fn multiline_subagent_concurrency_limit_is_a_python_exception() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, _started) = invocation_context().await;
        let calls = (0..9)
            .map(|index| format!("start_subagent('child {index}')"))
            .collect::<Vec<_>>()
            .join(", ");

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            &format!("import asyncio\ntasks = await asyncio.gather(\n    {calls}\n)"),
        )
        .await;
        assert!(
            python_error(&execution)
                .text
                .contains("subagent concurrency limit is exhausted")
        );
    }

    #[tokio::test]
    async fn python_subagent_validation_and_terminal_errors_use_builtin_exceptions() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;
        let limit = crate::config::constants::MAX_PROMPT_BYTES;
        let validation = format!(
            "errors = []\ntry:\n    await _kiki_start_subagent('missing arguments')\nexcept TypeError:\n    errors.append('start_arity')\ntry:\n    await _kiki_start_subagent(prompt='keyword', model=None, read_only=False)\nexcept TypeError:\n    errors.append('keywords')\ntry:\n    await _kiki_get_subagent()\nexcept TypeError:\n    errors.append('get_arity')\ntry:\n    await _kiki_refresh_subagent('00', 'extra')\nexcept TypeError:\n    errors.append('refresh_arity')\ntry:\n    await _kiki_wait_subagent(0)\nexcept TypeError:\n    errors.append('wait_type')\ntry:\n    await _kiki_cancel_subagent('00', 'extra')\nexcept TypeError:\n    errors.append('cancel_arity')\ntry:\n    await start_subagent('', None, False)\nexcept ValueError:\n    errors.append('empty')\ntry:\n    await start_subagent('🦀' * {}, None, False)\nexcept ValueError:\n    errors.append('unicode')\ntry:\n    await start_subagent('valid', 'unknown/model', False)\nexcept ValueError:\n    errors.append('model')\ntry:\n    await start_subagent('valid', None, 1)\nexcept TypeError:\n    errors.append('boolean')\ntry:\n    await get_subagent('invalid')\nexcept ValueError:\n    errors.append('id')\nerrors",
            limit / '🦀'.len_utf8() + 1,
        );
        let execution = execute_with_invocation(&mut arena, &context, &validation).await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "['start_arity', 'keywords', 'get_arity', 'refresh_arity', 'wait_type', 'cancel_arity', 'empty', 'unicode', 'model', 'boolean', 'id']"
        );
        assert!(started.try_recv().is_err());

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "task = await start_subagent('failure')\ntask.id",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "'0'");
        let finish = started.recv().await.expect("child starts");
        finish.send(Err(ChildFailure::new("failed work"))).unwrap();
        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "caught = None\ntry:\n    await task.wait()\nexcept RuntimeError as error:\n    caught = (task.status.state, task.status.error, task.id in str(error))\ncaught",
        )
        .await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "('failed', 'failed work', True)"
        );

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "cancelled = await start_subagent('cancel')\nstatus = await cancelled.cancel()\ncaught = None\ntry:\n    await cancelled.wait()\nexcept RuntimeError:\n    caught = cancelled.status.state\n(status.state, caught)",
        )
        .await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "('cancelled', 'cancelled')"
        );

        let execution = execute_without_cancellation(
            &mut arena,
            "caught = None\ntry:\n    await start_subagent('missing context')\nexcept RuntimeError as error:\n    caught = str(error)\ncaught",
        )
        .await
        .unwrap();
        assert!(
            complete(&execution)
                .unwrap()
                .head
                .contains("context is unavailable")
        );
    }

    #[tokio::test]
    async fn gathered_subagent_starts_preserve_filesystem_effects_and_result_order() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let mut arena = arena(&workspace).await;
        let (context, _started) = invocation_context().await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "import asyncio\ncontents, task = await asyncio.gather(read('file'), start_subagent('child'))\nawait write('file', contents + '!')\nawait task.cancel()\ntask.id",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "'0'");
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "before!"
        );
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(6),
                },
                ToolCall {
                    tool: ToolName::StartSubagent,
                    summary: ToolCallSummary::Subagent("0".to_owned()),
                },
                ToolCall {
                    tool: ToolName::Write,
                    summary: ToolCallSummary::Replaced,
                },
                ToolCall {
                    tool: ToolName::CancelSubagent,
                    summary: ToolCallSummary::Subagent("0".to_owned()),
                },
            ]
        );
    }

    #[tokio::test]
    async fn gathered_waits_settle_successful_siblings_after_a_child_failure() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;
        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "first = await start_subagent('first')\nsecond = await start_subagent('second')\n(second.id)",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "'1'");
        let first = started.recv().await.expect("first child starts");
        let second = started.recv().await.expect("second child starts");
        first.send(Err(ChildFailure::new("failed first"))).unwrap();
        second
            .send(Ok(ChildResult::new(
                "second done".to_owned(),
                BTreeMap::new(),
            )))
            .unwrap();

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "import asyncio\nawait asyncio.gather(first.wait(), second.wait())",
        )
        .await;
        assert!(python_error(&execution).text.contains("RuntimeError"));
        assert_eq!(
            complete(
                &execute_with_invocation(&mut arena, &context, "(await second.wait())['message']")
                    .await,
            )
            .unwrap()
            .head,
            "'second done'"
        );
    }

    #[tokio::test]
    async fn python_wait_maps_timed_out_children_and_updates_the_cached_status() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let clock = Arc::new(InvocationClock::new());
        let (context, coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "task = await start_subagent('timeout')\ntask.id",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "'0'");
        let _finish = started.recv().await.expect("child starts");
        clock.advance(Duration::from_secs(901));
        coordinator.service().await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "caught = None\ntry:\n    await task.wait()\nexcept TimeoutError:\n    caught = task.status.state\ncaught",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "'timed_out'");
    }

    #[tokio::test]
    async fn sole_subagent_wait_pauses_the_chargeable_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let (context, _coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        arena
            .execute_with_progress_and_context(
                "task = await start_subagent('wait')",
                &cancellation,
                &mut progress,
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        let finish = started.recv().await.expect("child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "(await task.wait())['message']",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));

        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before its child wait: {result:?}"),
        }
        clock.advance(Duration::from_secs(6 * 60));
        finish
            .send(Ok(ChildResult::new("done".to_owned(), BTreeMap::new())))
            .unwrap();

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'done'");
    }

    #[tokio::test]
    async fn concurrent_subagent_waits_pause_the_chargeable_budget_and_preserve_result_order() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let (context, _coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        arena
            .execute_with_progress_and_context(
                "first = await start_subagent('first')\nsecond = await start_subagent('second')",
                &cancellation,
                &mut progress,
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        let first = started.recv().await.expect("first child starts");
        let second = started.recv().await.expect("second child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "import asyncio\nfirst_result, second_result = await asyncio.gather(first.wait(), second.wait())\n(first_result['message'], second_result['message'])",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));

        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before both child waits entered their wait phase: {result:?}"),
        }
        clock.advance(Duration::from_secs(6 * 60));
        second
            .send(Ok(ChildResult::new(
                "second done".to_owned(),
                BTreeMap::new(),
            )))
            .unwrap();
        first
            .send(Ok(ChildResult::new(
                "first done".to_owned(),
                BTreeMap::new(),
            )))
            .unwrap();

        let execution = execution.await.unwrap();
        assert_eq!(
            complete(&execution).unwrap().head,
            "('first done', 'second done')"
        );
    }

    #[tokio::test]
    async fn run_subagent_wait_and_repeated_waits_keep_the_same_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let (context, _coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "result = await run_subagent('wait')\ntask = await get_subagent(result['id'])\nagain = await task.wait()\n(result['message'], again['message'])",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        let finish = tokio::select! {
            finish = started.recv() => finish.expect("child starts"),
            result = &mut execution => panic!("execution finished before its child started: {result:?}"),
        };
        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before its child wait: {result:?}"),
        }
        clock.advance(Duration::from_secs(6 * 60));
        finish
            .send(Ok(ChildResult::new("done".to_owned(), BTreeMap::new())))
            .unwrap();

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "('done', 'done')");
        assert_eq!(
            execution
                .tool_calls
                .iter()
                .map(|call| call.tool)
                .collect::<Vec<_>>(),
            [
                ToolName::StartSubagent,
                ToolName::WaitSubagent,
                ToolName::GetSubagent,
                ToolName::WaitSubagent,
            ]
        );
    }

    #[tokio::test]
    async fn sole_wait_preserves_remaining_budget_for_later_host_work() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("before"), "before").unwrap();
        fs::write(workspace.path().join("after"), "after").unwrap();
        let clock = Arc::new(InvocationClock::new());
        let (mut arena, gate) = arena_with_read_gate_and_clock(&workspace, clock.clone()).await;
        let (context, _coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        let mut start = Box::pin(arena.execute_with_progress_and_context(
            "task = await start_subagent('wait')",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut start => panic!("start finished before its control call settled: {result:?}"),
        }
        gate.resume_driver();
        start.await.unwrap();
        let finish = started.recv().await.expect("child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "import asyncio\nawait asyncio.gather(task.wait(), read('before'))\nawait read('after')",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before the first read: {result:?}"),
        }
        clock.advance(Duration::from_secs(2));
        gate.resume_read();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut execution => panic!("execution finished before the first read settled: {result:?}"),
        }
        gate.resume_driver();
        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before its child wait: {result:?}"),
        }
        clock.advance(Duration::from_secs(10 * 60));
        finish
            .send(Ok(ChildResult::new("done".to_owned(), BTreeMap::new())))
            .unwrap();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut execution => panic!("execution finished before its child wait settled: {result:?}"),
        }
        gate.resume_driver();
        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before the second read: {result:?}"),
        }
        clock.advance(Duration::from_secs(3));

        assert!(matches!(execution.await, Err(ArenaError::TimedOut { .. })));
    }

    #[tokio::test]
    async fn mixed_gather_keeps_child_wait_chargeable_until_ordinary_call_settles() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("blocked"), "blocked").unwrap();
        let clock = Arc::new(InvocationClock::new());
        let (mut arena, gate) = arena_with_read_gate_and_clock(&workspace, clock.clone()).await;
        let (context, coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        let mut start = Box::pin(arena.execute_with_progress_and_context(
            "task = await start_subagent('wait')",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut start => panic!("start finished before its control call settled: {result:?}"),
        }
        gate.resume_driver();
        start.await.unwrap();
        let _finish = started.recv().await.expect("child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "import asyncio\nawait asyncio.gather(task.wait(), read('blocked'))",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before the ordinary call started: {result:?}"),
        }
        clock.advance(Duration::from_secs(5));

        assert!(matches!(execution.await, Err(ArenaError::TimedOut { .. })));
        coordinator.cancel_all().await;
    }

    #[tokio::test]
    async fn queued_ordinary_calls_remain_chargeable() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 0..17 {
            fs::write(
                workspace.path().join(format!("file-{index}")),
                index.to_string(),
            )
            .unwrap();
        }
        let clock = Arc::new(InvocationClock::new());
        let gate = BatchTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .unwrap()
            .with_batch_test_gate(gate.clone());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            tool_host,
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let cancellation = CancellationToken::new();
        let code = gathered_reads(17);
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut execution => panic!("execution finished before its active calls started: {result:?}"),
        }
        clock.advance(Duration::from_secs(5));

        assert!(matches!(execution.await, Err(ArenaError::TimedOut { .. })));
        assert!(gate.active_call_ids().is_empty());
    }

    #[tokio::test]
    async fn failed_ordinary_gather_sibling_does_not_pause_budget() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("blocked"), "blocked").unwrap();
        let clock = Arc::new(InvocationClock::new());
        let (mut arena, gate) = arena_with_read_gate_and_clock(&workspace, clock.clone()).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "import asyncio\nawait asyncio.gather(run('true', 'missing'), read('blocked'))",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before the ordinary call started: {result:?}"),
        }
        clock.advance(Duration::from_secs(5));

        assert!(matches!(execution.await, Err(ArenaError::TimedOut { .. })));
    }

    #[tokio::test]
    async fn parent_cancellation_while_paused_recovers_without_stopping_the_child() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        execute_without_cancellation(&mut arena, "value = 7")
            .await
            .unwrap();
        let (context, coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        arena
            .execute_with_progress_and_context(
                "task = await start_subagent('wait')",
                &cancellation,
                &mut progress,
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        let _finish = started.recv().await.expect("child starts");
        let child = coordinator.records().await.pop().unwrap().id().clone();
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "await task.wait()",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before its child wait: {result:?}"),
        }
        cancellation.cancel();

        assert!(matches!(execution.await, Err(ArenaError::Cancelled { .. })));
        assert_eq!(coordinator.active_count().await, 1);
        let observer = coordinator.root_caller().await;
        assert_eq!(
            coordinator.get(&observer, &child).await.unwrap().state(),
            TaskState::Running
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "7"
        );
        coordinator.cancel_all().await;
    }

    #[tokio::test]
    async fn chargeable_budget_ties_prioritise_cancellation_then_timeout() {
        let clock = Arc::new(InvocationClock::new());
        let cancellation = CancellationToken::new();
        let mut budget = ChargeableBudget::new(clock.clone(), Duration::ZERO);
        assert!(matches!(
            budget.wait(&cancellation, std::future::ready(())).await,
            Err(super::BudgetInterruption::TimedOut)
        ));

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut budget = ChargeableBudget::new(clock, Duration::ZERO);
        assert!(matches!(
            budget.wait(&cancellation, std::future::ready(())).await,
            Err(super::BudgetInterruption::Cancelled)
        ));
    }

    #[test]
    fn budget_pauses_for_all_and_only_waiting_call_modes() {
        for (modes, expected_paused) in [
            (vec![], false),
            (vec![PendingCallMode::Waiting], true),
            (
                vec![PendingCallMode::Waiting, PendingCallMode::Waiting],
                true,
            ),
            (
                vec![
                    PendingCallMode::Waiting,
                    PendingCallMode::Waiting,
                    PendingCallMode::Waiting,
                ],
                true,
            ),
            (vec![PendingCallMode::Control], false),
            (vec![PendingCallMode::Ordinary], false),
            (
                vec![PendingCallMode::Waiting, PendingCallMode::Control],
                false,
            ),
            (
                vec![PendingCallMode::Control, PendingCallMode::Waiting],
                false,
            ),
            (
                vec![PendingCallMode::Waiting, PendingCallMode::Ordinary],
                false,
            ),
            (
                vec![PendingCallMode::Ordinary, PendingCallMode::Waiting],
                false,
            ),
            (
                vec![
                    PendingCallMode::Waiting,
                    PendingCallMode::Waiting,
                    PendingCallMode::Control,
                ],
                false,
            ),
            (
                vec![
                    PendingCallMode::Waiting,
                    PendingCallMode::Waiting,
                    PendingCallMode::Ordinary,
                ],
                false,
            ),
        ] {
            let mut budget =
                ChargeableBudget::new(Arc::new(InvocationClock::new()), Duration::from_secs(1));
            let call_modes = modes
                .into_iter()
                .zip(0_u32..)
                .map(|(mode, call_id)| (call_id, mode))
                .collect();

            update_budget_pause(&mut budget, &call_modes);

            assert_eq!(budget.is_paused(), expected_paused, "{call_modes:?}");
        }
    }

    #[tokio::test]
    async fn child_cancellation_cleanup_pauses_the_chargeable_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let (started, mut children) = mpsc::unbounded_channel();
        let coordinator = SubagentCoordinator::with_clock(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(CleanupInvocationRunner { started }),
            clock.clone(),
        );
        let context =
            AgentInvocationContext::new(coordinator.root_caller().await, coordinator.clone())
                .with_accepted_thinking(AcceptedThinking::new(
                    selection,
                    snapshot.settings().clone(),
                    None,
                ));
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        arena
            .execute_with_progress_and_context(
                "task = await start_subagent('cancel')",
                &cancellation,
                &mut progress,
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        let release = children.recv().await.expect("child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "(await task.cancel()).state",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before child cleanup: {result:?}"),
        }
        clock.advance(Duration::from_secs(6 * 60));
        release.send(()).unwrap();

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'cancelled'");
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn child_deadline_expires_while_the_parent_budget_is_paused() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let (context, coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        arena
            .execute_with_progress_and_context(
                "task = await start_subagent('wait')",
                &cancellation,
                &mut progress,
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        let _finish = started.recv().await.expect("child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "caught = None\ntry:\n    await task.wait()\nexcept TimeoutError:\n    caught = 'timed out'\ncaught",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before its child wait: {result:?}"),
        }
        clock.advance(Duration::from_secs(901));

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'timed out'");
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn child_deadline_service_work_consumes_the_parent_budget() {
        let workspace = tempfile::tempdir().unwrap();
        let clock = Arc::new(InvocationClock::new());
        let mut arena = Arena::with_limits_and_clock(
            worker_path(),
            ToolHost::new(workspace.path()).await.unwrap(),
            Duration::from_secs(5),
            Some(64 * 1024 * 1024),
            clock.clone(),
        )
        .await
        .unwrap();
        let (context, coordinator, mut started) =
            invocation_context_with_clock(clock.clone()).await;
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let mut progress = |_| {};
        arena
            .execute_with_progress_and_context(
                "task = await start_subagent('wait')",
                &cancellation,
                &mut progress,
                Some(&context),
                Some(selection),
            )
            .await
            .unwrap();
        let _finish = started.recv().await.expect("child starts");
        let mut execution = Box::pin(arena.execute_with_progress_and_context(
            "caught = None\ntry:\n    await task.wait()\nexcept TimeoutError:\n    caught = 'timed out'\ncaught",
            &cancellation,
            &mut progress,
            Some(&context),
            Some(selection),
        ));
        let wait_phase = clock.wait_until_budget_paused();
        tokio::pin!(wait_phase);
        tokio::select! {
            _ = &mut wait_phase => {}
            result = &mut execution => panic!("execution finished before its child wait: {result:?}"),
        }
        clock.advance_next_coordinator_now(Duration::from_secs(5));
        clock.advance(Duration::from_secs(900));

        assert!(matches!(execution.await, Err(ArenaError::TimedOut { .. })));
        coordinator.cancel_all().await;
    }

    #[tokio::test]
    async fn python_subagent_selection_uses_the_invoking_step_or_accepted_target_thinking() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let snapshot = crate::config::ready_snapshot_for_test();
        let invoking = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let accepted = AcceptedThinking::new(invoking, snapshot.settings().clone(), None);
        let terra =
            crate::config::ModelSelection::new(&crate::config::models::TERRA, "high").unwrap();
        accepted.accept(terra);
        let (selections, mut received) = mpsc::unbounded_channel();
        let coordinator: SessionSubagents = SubagentCoordinator::new(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(SelectionRunner { selections }),
        );
        let context = AgentInvocationContext::new(coordinator.root_caller().await, coordinator)
            .with_accepted_thinking(accepted);

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "inherited = await start_subagent('inherit')\noverridden = await start_subagent('override', 'openai/gpt-5.6-terra')\n(inherited.status.model, inherited.status.thinking, overridden.status.model, overridden.status.thinking)",
        )
        .await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "('openai/gpt-5.6-sol', 'medium', 'openai/gpt-5.6-terra', 'high')"
        );
        assert_eq!(received.recv().await, Some(invoking));
        assert_eq!(received.recv().await, Some(terra));
    }

    #[tokio::test]
    async fn same_execution_starts_inherit_the_pre_execution_checkpoint_only() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        execute_without_cancellation(&mut arena, "inherited = 'before'")
            .await
            .unwrap();
        let context = complete_invocation_context([
            vec![
                ScriptedAction::Code {
                    source: "result_values['inherited'] = inherited\ntry:\n    result_values['same_execution'] = same_execution\nexcept NameError:\n    result_values['same_execution'] = 'missing'".to_owned(),
                },
                ScriptedAction::Message {
                    text: "first".to_owned(),
                },
            ],
            vec![
                ScriptedAction::Code {
                    source: "result_values['inherited'] = inherited\ntry:\n    result_values['same_execution'] = same_execution\nexcept NameError:\n    result_values['same_execution'] = 'missing'".to_owned(),
                },
                ScriptedAction::Message {
                    text: "second".to_owned(),
                },
            ],
        ])
        .await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "same_execution = 'later'\nfirst = await start_subagent('first')\nsecond = await start_subagent('second')\n(first.id, second.id)",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "('0', '1')");

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "first_result = await first.wait()\nsecond_result = await second.wait()\nroot_exports = None\ntry:\n    result_values\nexcept NameError:\n    root_exports = 'missing'\n(first_result['values']['inherited'], first_result['values']['same_execution'], second_result['values']['inherited'], second_result['values']['same_execution'], root_exports)",
        )
        .await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "('before', 'missing', 'before', 'missing', 'missing')"
        );
    }

    #[tokio::test]
    async fn mutated_task_ids_are_rejected_without_affecting_reconnectable_children() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let (context, mut started) = invocation_context().await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "task = await start_subagent('child')\nsaved_id = task.id\nerrors = []\ntask.id = '0g'\ntry:\n    await task.refresh()\nexcept ValueError:\n    errors.append('malformed_refresh')\ntry:\n    await task.wait()\nexcept ValueError:\n    errors.append('malformed_wait')\ntry:\n    await task.cancel()\nexcept ValueError:\n    errors.append('malformed_cancel')\ntask.id = 'dead'\ntry:\n    await task.refresh()\nexcept ValueError:\n    errors.append('unknown_refresh')\ntry:\n    await task.wait()\nexcept ValueError:\n    errors.append('unknown_wait')\ntry:\n    await task.cancel()\nexcept ValueError:\n    errors.append('unknown_cancel')\n(errors, saved_id)",
        )
        .await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "(['malformed_refresh', 'malformed_wait', 'malformed_cancel', 'unknown_refresh', 'unknown_wait', 'unknown_cancel'], '0')"
        );
        let checkpoint = arena.checkpoint();
        arena.restore(checkpoint).await.unwrap();
        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "task = await get_subagent(saved_id)\n(task.id, task.status.state)",
        )
        .await;
        assert!(matches!(
            complete(&execution).unwrap().head.as_str(),
            "('0', 'starting')" | "('0', 'running')"
        ));
        let finish = started.recv().await.expect("child starts");
        finish
            .send(Ok(ChildResult::new("done".to_owned(), BTreeMap::new())))
            .unwrap();
        let execution =
            execute_with_invocation(&mut arena, &context, "(await task.wait())['id']").await;
        assert_eq!(complete(&execution).unwrap().head, "'0'");
    }

    #[tokio::test]
    async fn child_callers_map_self_ancestor_and_sibling_denials_to_permission_errors() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .unwrap();
        let (callers, mut captured) = mpsc::unbounded_channel();
        let coordinator: SessionSubagents = SubagentCoordinator::new(
            SubagentPolicy::from(snapshot.subagents()),
            Arc::new(CallerCapturingInvocationRunner { callers }),
        );
        let root = coordinator.root_caller().await;
        let payload = || ChildPayload::new(arena.child_source(), arena.checkpoint());
        let parameters = || ChildParameters::new("child".to_owned(), selection, false);

        let parent = coordinator
            .start(&root, parameters(), payload())
            .await
            .unwrap();
        let parent_caller = captured.recv().await.expect("parent caller");
        let child = coordinator
            .start(&parent_caller, parameters(), payload())
            .await
            .unwrap();
        let child_caller = captured.recv().await.expect("child caller");
        let sibling = coordinator
            .start(&parent_caller, parameters(), payload())
            .await
            .unwrap();
        let _sibling_caller = captured.recv().await.expect("sibling caller");
        let context =
            AgentInvocationContext::new(child_caller, coordinator.clone()).with_accepted_thinking(
                AcceptedThinking::new(selection, snapshot.settings().clone(), None),
            );

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            &format!(
                "errors = []\nalias = _kiki_get_subagent\ntry:\n    await alias('{}')\nexcept PermissionError:\n    errors.append('self_get')\ntry:\n    await _kiki_refresh_subagent('{}')\nexcept PermissionError:\n    errors.append('ancestor_refresh')\ntry:\n    await _kiki_wait_subagent('{}')\nexcept PermissionError:\n    errors.append('sibling_wait')\ntry:\n    await _kiki_cancel_subagent('{}')\nexcept PermissionError:\n    errors.append('self_cancel')\nerrors",
                child.id(),
                parent.id(),
                sibling.id(),
                child.id(),
            ),
        )
        .await;
        assert_eq!(
            complete(&execution).unwrap().head,
            "['self_get', 'ancestor_refresh', 'sibling_wait', 'self_cancel']"
        );
        for id in [parent.id(), child.id(), sibling.id()] {
            assert_eq!(
                coordinator.get(&root, id).await.unwrap().state(),
                TaskState::Running
            );
        }
        coordinator.cancel_all().await;
        assert_eq!(coordinator.active_count().await, 0);
    }

    #[tokio::test]
    async fn nested_subagents_use_the_same_trusted_selection_context() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let context = complete_invocation_context([
            vec![
                ScriptedAction::Code {
                    source: "grandchild = await start_subagent('nested')\nresult_values['nested_id'] = grandchild.id".to_owned(),
                },
                ScriptedAction::Message {
                    text: "child complete".to_owned(),
                },
            ],
            vec![ScriptedAction::Message {
                text: "grandchild complete".to_owned(),
            }],
        ])
        .await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "child = await start_subagent('parent child')\nchild_result = await child.wait()\ngrandchild = await get_subagent(child_result['values']['nested_id'])\n(await grandchild.wait())['message']",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "'grandchild complete'");
    }

    #[tokio::test]
    async fn child_invocation_authority_and_read_only_permissions_are_host_enforced() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let mut arena = arena(&workspace).await;
        let context = complete_invocation_context([
            vec![
                ScriptedAction::Code {
                    source: "try:\n    await get_subagent('0')\nexcept PermissionError:\n    result_values['self_denied'] = True".to_owned(),
                },
                ScriptedAction::Message {
                    text: "authority checked".to_owned(),
                },
            ],
            vec![
                ScriptedAction::Code {
                    source: "try:\n    await write('file', 'after')\nexcept PermissionError:\n    result_values['write_denied'] = True".to_owned(),
                },
                ScriptedAction::Message {
                    text: "permissions checked".to_owned(),
                },
            ],
        ])
        .await;

        let execution = execute_with_invocation(
            &mut arena,
            &context,
            "parent = await start_subagent('authority')\nreadonly = await start_subagent('permissions', None, True)\nparent_result = await parent.wait()\nreadonly_result = await readonly.wait()\n(parent_result['values']['self_denied'], readonly_result['values']['write_denied'])",
        )
        .await;
        assert_eq!(complete(&execution).unwrap().head, "(True, True)");
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "before"
        );
    }

    #[tokio::test]
    async fn children_can_share_a_checkpoint_and_reject_foreign_identities() {
        let workspace = tempfile::tempdir().unwrap();
        let mut parent = arena(&workspace).await;
        execute_without_cancellation(&mut parent, "value = 1")
            .await
            .unwrap();
        let checkpoint = parent.checkpoint();

        let mut first = parent.child(&checkpoint, false).await.unwrap();
        let mut second = parent.child(&checkpoint, false).await.unwrap();
        execute_without_cancellation(&mut first, "value = 2")
            .await
            .unwrap();
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut second, "value")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "1"
        );

        let mut foreign = checkpoint;
        foreign.identity = RuntimeIdentity("monty-runtime 9.9.9".to_owned());
        let Err(error) = parent.child(&foreign, false).await else {
            panic!("foreign checkpoint unexpectedly created a child");
        };
        assert!(matches!(error, ArenaError::IncompatibleCheckpoint { .. }));
    }

    #[tokio::test]
    async fn read_only_children_deny_mutating_wrappers_and_descendants() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let mut parent = arena(&workspace).await;
        execute_without_cancellation(&mut parent, "await read('file')")
            .await
            .unwrap();
        let checkpoint = parent.checkpoint();
        let mut child = parent.child(&checkpoint, true).await.unwrap();
        let authorisations = child.checkpoint().authorisations.clone();

        for code in [
            "await write('created', 'child')",
            "await _kiki_write('created', 'child')",
            "write_alias = write\nawait write_alias('created', 'child')",
            "await edit('file', 'before', 'child')",
            "await _kiki_edit('file', 'before', 'child')",
            "edit_alias = edit\nawait edit_alias('file', 'before', 'child')",
            "await run('touch command-effect')",
            "await _kiki_run('touch command-effect')",
            "run_alias = run\nawait run_alias('touch command-effect')",
        ] {
            let execution = execute_without_cancellation(&mut child, code)
                .await
                .unwrap();
            assert!(python_error(&execution).text.contains("PermissionError"));
            assert_eq!(execution.tool_calls.len(), 1);
            assert_eq!(execution.tool_calls[0].summary, ToolCallSummary::Failed);
            assert_eq!(child.checkpoint().authorisations, authorisations);
        }
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut child, "await read('file')")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "'before'"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "before"
        );
        assert!(!workspace.path().join("created").exists());
        assert!(!workspace.path().join("command-effect").exists());

        let mut grandchild = child.child(&child.checkpoint(), false).await.unwrap();
        assert!(
            python_error(
                &execute_without_cancellation(&mut grandchild, "await write('created', 'child')")
                    .await
                    .unwrap()
            )
            .text
            .contains("PermissionError")
        );
    }

    #[tokio::test]
    async fn result_values_exports_neutral_values_and_recovers_after_rejection() {
        let workspace = tempfile::tempdir().unwrap();
        let mut root = arena(&workspace).await;
        let root_checkpoint = root.checkpoint();
        let missing = root.collect_result_values().await.unwrap_err();
        assert!(missing.to_string().contains("result_values is not defined"));
        assert_eq!(root.checkpoint(), root_checkpoint);
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut root, "1 + 1")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "2"
        );

        let mut parent = arena(&workspace).await;
        execute_without_cancellation(&mut parent, "def source():\n    return 42")
            .await
            .unwrap();
        let checkpoint = parent.checkpoint();
        let mut child = parent.child(&checkpoint, false).await.unwrap();
        execute_without_cancellation(
            &mut child,
            "result_values = {'none': None, 'bool': True, 'integer': -1, 'float': 1.5, 'string': 'value', 'list': [1, {'nested': 'value'}]}",
        )
        .await
        .unwrap();
        let expected = BTreeMap::from([
            ("bool".to_owned(), ToolValue::Bool(true)),
            ("float".to_owned(), ToolValue::Float(1.5)),
            ("integer".to_owned(), ToolValue::Integer(-1)),
            (
                "list".to_owned(),
                ToolValue::List(vec![
                    ToolValue::Integer(1),
                    ToolValue::Dictionary(BTreeMap::from([(
                        "nested".to_owned(),
                        ToolValue::String("value".to_owned()),
                    )])),
                ]),
            ),
            ("none".to_owned(), ToolValue::None),
            ("string".to_owned(), ToolValue::String("value".to_owned())),
        ]);
        let child_checkpoint = child.checkpoint();
        assert_eq!(child.collect_result_values().await.unwrap(), expected);
        assert_eq!(
            child.checkpoint().authorisations,
            child_checkpoint.authorisations
        );
        assert_eq!(child.collect_result_values().await.unwrap(), expected);
        assert_eq!(
            child.checkpoint().authorisations,
            child_checkpoint.authorisations
        );

        let cases = [
            ("non-dictionary", "result_values = 1"),
            ("non-string key", "result_values = {1: 'value'}"),
            (
                "function",
                "def unsupported():\n    return None\nresult_values = {'value': unsupported}",
            ),
            (
                "class",
                "class Unsupported:\n    pass\nresult_values = {'value': Unsupported}",
            ),
            ("bytes", "result_values = {'value': b'bytes'}"),
            ("big integer", "result_values = {'value': 1 << 100}"),
            ("repr", "result_values = {'value': iter([])}"),
            (
                "cycle",
                "cycle = []\ncycle.append(cycle)\nresult_values = {'value': cycle}",
            ),
            (
                "transport depth",
                "result_values = {}\nvalue = result_values\nfor _ in range(1_100):\n    next_value = {}\n    value['next'] = next_value\n    value = next_value",
            ),
            ("too many values", "result_values = {'value': [0] * 10_001}"),
            (
                "too much payload",
                "result_values = {'value': 'x' * (4 * 1024 * 1024 + 1)}",
            ),
        ];
        for (case, code) in cases {
            let setup = execute_without_cancellation(&mut child, code)
                .await
                .unwrap();
            assert!(
                matches!(setup.outcome, ExecutionOutcome::Complete { .. }),
                "{case} setup did not complete: {setup:?}"
            );
            let checkpoint = child.checkpoint();
            let error = child.collect_result_values().await.unwrap_err();
            assert!(matches!(error, super::ResultValuesError::Export { .. }));
            assert!(
                error.to_string().len()
                    <= "result_values export failed: ".len() + super::RESULT_VALUES_ERROR_BYTES
            );
            assert_eq!(child.checkpoint(), checkpoint);
            assert_eq!(
                complete(
                    &execute_without_cancellation(&mut child, "1 + 1")
                        .await
                        .unwrap(),
                )
                .unwrap()
                .head,
                "2",
                "child did not recover after {case} export"
            );
        }

        let hostile_setup = execute_without_cancellation(
            &mut child,
            "repr_mutations = []\nclass Hostile:\n    def __repr__(self):\n        repr_mutations.append('changed')\n        _kiki_write('collection-side-effect', 'bad')\n        return 'Hostile()'\nresult_values = {'value': Hostile()}",
        )
        .await
        .unwrap();
        assert!(matches!(
            hostile_setup.outcome,
            ExecutionOutcome::Complete { .. }
        ));
        let checkpoint = child.checkpoint();
        let error = child.collect_result_values().await.unwrap_err();
        assert!(matches!(error, super::ResultValuesError::Export { .. }));
        assert_eq!(child.checkpoint(), checkpoint);
        assert!(!workspace.path().join("collection-side-effect").exists());
        assert_eq!(child.checkpoint().authorisations, checkpoint.authorisations);
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut child, "repr_mutations")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "[]"
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut child, "source()")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "42"
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut parent, "source()")
                    .await
                    .unwrap(),
            )
            .unwrap()
            .head,
            "42"
        );
    }

    #[tokio::test]
    async fn preserves_values_across_several_executions() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value = 40")
                    .await
                    .unwrap()
            ),
            None
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value += 1")
                    .await
                    .unwrap()
            ),
            None
        );
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value * 2")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "82"
        );
    }

    #[tokio::test]
    async fn read_returns_one_exact_string_that_survives_checkpoint_restore() {
        let workspace = tempfile::tempdir().unwrap();
        let contents = "first\r\nsecond\nlast";
        fs::write(workspace.path().join("lines.txt"), contents).unwrap();
        let mut arena = arena(&workspace).await;
        let checkpoint = arena.checkpoint();

        let execution = execute_without_cancellation(
            &mut arena,
            "contents = await read('lines.txt')\ncontents == 'first\\r\\nsecond\\nlast'",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");

        arena.restore(checkpoint).await.unwrap();
        let execution = execute_without_cancellation(
            &mut arena,
            "(await read(path='./lines.txt')) == 'first\\r\\nsecond\\nlast'",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
    }

    #[tokio::test]
    async fn read_authorisations_commit_and_restore_with_monty_checkpoints() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena(&workspace).await;
        let initial = arena.checkpoint();

        execute_without_cancellation(&mut arena, "value = 1\nawait read('file')")
            .await
            .unwrap();
        let successful = arena.checkpoint();
        let successful_authorisations = successful.authorisations.clone();
        assert!(has_authorisation(&arena, "file"));

        execute_without_cancellation(&mut arena, "value = 99")
            .await
            .unwrap();
        arena.restore(initial).await.unwrap();
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        assert!(
            python_error(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .text
            .contains("NameError")
        );

        arena.restore(successful).await.unwrap();
        assert_eq!(arena.checkpoint.authorisations, successful_authorisations);
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "1"
        );
    }

    #[tokio::test]
    async fn list_dir_returns_dataclass_entries_and_survives_checkpoint_restore() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "four").unwrap();
        let mut arena = arena(&workspace).await;
        let checkpoint = arena.checkpoint();

        let execution = execute_without_cancellation(
            &mut arena,
            "entries = await list_dir()\nentry = entries[0]\nentry.path == 'file' and entry.kind == 'file' and entry.size_bytes == 4",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");

        arena.restore(checkpoint).await.unwrap();
        let execution = execute_without_cancellation(
            &mut arena,
            "entry = (await list_dir('./'))[0]\nentry.path == 'file'",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
    }

    #[tokio::test]
    async fn private_list_dir_accepts_zero_or_one_positional_string_arguments() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "entries = await _kiki_list_dir()\nentries[0]['path']",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'file'");

        for code in [
            "await _kiki_list_dir(1)",
            "await _kiki_list_dir('.', '.')",
            "await _kiki_list_dir(path='.')",
            "await _kiki_list_dir({'path': '.'})",
        ] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();
            assert!(python_error(&execution).text.contains("TypeError"));
        }
    }

    #[tokio::test]
    async fn list_dir_maps_not_a_directory_and_collection_overflow_to_builtin_errors() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "result = 'allowed'\ntry:\n    await list_dir('file')\nexcept Exception as error:\n    result = type(error).__name__\nresult",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'NotADirectoryError'");

        let mut arena = arena_with_collection_limits(&workspace, 0, usize::MAX).await;
        let execution = execute_without_cancellation(
            &mut arena,
            "result = 'allowed'\ntry:\n    await list_dir()\nexcept Exception as error:\n    result = type(error).__name__\nresult",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'RuntimeError'");
    }

    #[tokio::test]
    async fn list_dir_call_trace_counts_returned_entries() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a"), "").unwrap();
        fs::write(workspace.path().join("b"), "").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(&mut arena, "len(await list_dir())")
            .await
            .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "2");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::ListDir,
                summary: ToolCallSummary::Count(2),
            }]
        );
    }

    #[tokio::test]
    async fn glob_returns_directory_entries_for_default_and_explicit_roots_after_restore() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir_all(workspace.path().join("src/nested")).unwrap();
        fs::write(workspace.path().join("src/direct.rs"), "four").unwrap();
        fs::write(workspace.path().join("src/nested/mod.rs"), "five").unwrap();
        let mut arena = arena(&workspace).await;
        let checkpoint = arena.checkpoint();

        let execution = execute_without_cancellation(
            &mut arena,
            "entries = await glob('*.rs')\n[entry.path for entry in entries] == ['src/direct.rs', 'src/nested/mod.rs']",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");

        arena.restore(checkpoint).await.unwrap();
        let execution = execute_without_cancellation(
            &mut arena,
            "entry = (await glob('nested/*.rs', 'src'))[0]\nentry.path == 'src/nested/mod.rs' and entry.kind == 'file' and entry.size_bytes == 4",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
    }

    #[tokio::test]
    async fn private_glob_accepts_one_or_two_positional_strings_and_maps_errors() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("src/file.rs"), "").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "entries = await _kiki_glob('*.rs')\nlen(entries)",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "1");

        let execution = execute_without_cancellation(
            &mut arena,
            "entries = await _kiki_glob('*.rs', 'src')\nentries[0]['path']",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'src/file.rs'");

        for code in [
            "await _kiki_glob()",
            "await _kiki_glob(1)",
            "await _kiki_glob('*.rs', '.', '.')",
            "await _kiki_glob(pattern='*.rs')",
            "await _kiki_glob({'pattern': '*.rs'})",
        ] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();
            assert!(python_error(&execution).text.contains("TypeError"));
        }

        let execution = execute_without_cancellation(
            &mut arena,
            "result = 'allowed'\ntry:\n    await glob('[')\nexcept Exception as error:\n    result = type(error).__name__\nresult",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'ValueError'");
    }

    #[tokio::test]
    async fn glob_call_trace_counts_returned_entries() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("a.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/b.rs"), "").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(&mut arena, "len(await glob('*.rs'))")
            .await
            .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "2");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::from_private_name("_kiki_glob")
                    .expect("glob must be an active host tool"),
                summary: ToolCallSummary::Count(2),
            }]
        );
    }

    #[tokio::test]
    async fn grep_returns_dataclass_matches_and_survives_checkpoint_restore() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("lines.txt"),
            "skip\nmatch first\r\nmatch last",
        )
        .unwrap();
        let mut arena = arena(&workspace).await;
        let checkpoint = arena.checkpoint();

        let execution = execute_without_cancellation(
            &mut arena,
            "matches = await grep('^match')\nfirst = matches[0]\nfirst.path == 'lines.txt' and first.line == 2 and first.text == 'match first' and len(matches) == 2",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");

        arena.restore(checkpoint).await.unwrap();
        let execution = execute_without_cancellation(
            &mut arena,
            "match = (await grep('last', './lines.txt'))[0]\nmatch.path == 'lines.txt' and match.line == 3 and match.text == 'match last'",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
    }

    #[tokio::test]
    async fn private_grep_accepts_one_to_three_positional_arguments_and_maps_errors() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("src")).unwrap();
        fs::write(workspace.path().join("src/file.rs"), "match\n").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "matches = await _kiki_grep('match')\nlen(matches)",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "1");

        let execution = execute_without_cancellation(
            &mut arena,
            "matches = await _kiki_grep('match', 'src')\nmatches[0]['path']",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'src/file.rs'");

        let execution = execute_without_cancellation(
            &mut arena,
            "matches = await _kiki_grep('match', 'src', '*.rs')\nmatches[0]['path']",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "'src/file.rs'");

        let execution =
            execute_without_cancellation(&mut arena, "len(await _kiki_grep('match', 'src', None))")
                .await
                .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "1");

        for code in [
            "await _kiki_grep()",
            "await _kiki_grep(1)",
            "await _kiki_grep('match', '.', 1)",
            "await _kiki_grep('match', '.', None, 'extra')",
            "await _kiki_grep(pattern='match')",
            "await _kiki_grep({'pattern': 'match'})",
        ] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();
            assert!(python_error(&execution).text.contains("TypeError"));
        }

        for code in ["await grep('[')", "await grep('match', '.', '[')"] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();
            assert!(python_error(&execution).text.contains("ValueError"));
        }
    }

    #[tokio::test]
    async fn grep_call_trace_counts_returned_matches() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("b.rs"), "match\nmatch\n").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(&mut arena, "len(await grep('match'))")
            .await
            .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "3");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::from_private_name("_kiki_grep")
                    .expect("grep must be an active host tool"),
                summary: ToolCallSummary::Count(3),
            }]
        );
    }

    #[tokio::test]
    async fn exposes_exactly_seven_host_functions() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "match\n").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "contents = await read('file')\nlisted = await list_dir()\nmatched = await glob('*')\nfound = await grep('match')\nwrite_result = await write('new', contents)\nedit_result = await edit('file', 'match', 'changed')\ncommand_result = await run(\"printf 'command output'\")\ncontents == 'match\\n' and listed[0].path == 'file' and matched[0].path == 'file' and found[0].path == 'file' and write_result is None and edit_result is None and command_result.exit_code == 0 and command_result.stdout == 'command output' and command_result.stderr == '' and (await read('new')) == contents and (await read('file')) == 'changed\\n'",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
    }

    #[tokio::test]
    async fn private_mutations_accept_only_exact_positional_strings() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "write_result = await _kiki_write('file', 'first')\nedit_result = await _kiki_edit('file', 'first', 'second')\nwrite_result is None and edit_result is None and (await _kiki_read('file')) == 'second'",
        )
        .await
        .unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");

        for code in [
            "await _kiki_write('file')",
            "await _kiki_write('file', 1)",
            "await _kiki_write(path='file', content='contents')",
            "await _kiki_edit('file', 'old')",
            "await _kiki_edit('file', 'old', 1)",
            "await _kiki_edit(path='file', old='old', new='new')",
        ] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();
            assert!(python_error(&execution).text.contains("TypeError"));
        }
    }

    #[tokio::test]
    async fn sequential_mutations_return_none_update_freshness_and_record_safe_statuses() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "first").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "contents = await read('file')\nwrite_result = await write('file', 'second')\nedit_result = await edit('file', 'second', 'third')\nwrite_result is None and edit_result is None and (await read('file')) == 'third'",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(5),
                },
                ToolCall {
                    tool: ToolName::Write,
                    summary: ToolCallSummary::Replaced,
                },
                ToolCall {
                    tool: ToolName::Edit,
                    summary: ToolCallSummary::Applied,
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(5),
                },
            ]
        );
        assert!(has_authorisation(&arena, "file"));
    }

    #[tokio::test]
    async fn writes_create_then_replace_with_safe_statuses() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "await write('new', 'first')\nawait write('./new', 'second')\nawait read('new')",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "'second'");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Write,
                    summary: ToolCallSummary::Created,
                },
                ToolCall {
                    tool: ToolName::Write,
                    summary: ToolCallSummary::Replaced,
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(6),
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_same_batch_read_cannot_authorise_a_replacement_write() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "first").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "import asyncio\ntry:\n    await asyncio.gather(read('file'), write('file', 'second'))\nexcept RuntimeError:\n    pass\ntry:\n    await write('file', 'second')\nexcept RuntimeError:\n    denied = True\ndenied",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "first"
        );
        assert!(!has_authorisation(&arena, "file"));
    }

    #[tokio::test]
    async fn ordinary_python_errors_commit_mutation_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "await write('file', 'first')\nraise ValueError('expected')",
        )
        .await
        .unwrap();
        assert!(python_error(&execution).text.contains("ValueError"));
        assert!(has_authorisation(&arena, "file"));

        execute_without_cancellation(&mut arena, "await write('file', 'second')")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn restoring_checkpoint_restores_mutation_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let initial = arena.checkpoint();

        execute_without_cancellation(&mut arena, "await write('file', 'first')")
            .await
            .unwrap();
        let mutation_checkpoint = arena.checkpoint();
        assert!(has_authorisation(&arena, "file"));

        arena.restore(initial).await.unwrap();
        assert!(!has_authorisation(&arena, "file"));
        arena.restore(mutation_checkpoint).await.unwrap();

        execute_without_cancellation(&mut arena, "await write('file', 'second')")
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "second"
        );
    }

    #[tokio::test]
    async fn malformed_read_is_a_model_visible_python_error() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        for code in [
            "await read(1)",
            "await read()",
            "await _kiki_read(path='lines.txt')",
        ] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();

            assert!(python_error(&execution).text.contains("TypeError"));
        }
    }

    #[tokio::test]
    async fn read_maps_invalid_and_missing_paths_to_the_specified_exceptions() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        let mut arena = arena(&workspace).await;

        for (path, expected) in [
            ("/outside", "PermissionError"),
            ("missing", "FileNotFoundError"),
            ("directory", "IsADirectoryError"),
            ("nested/../file", "PermissionError"),
        ] {
            let execution = execute_without_cancellation(
                &mut arena,
                &format!(
                    "result = 'allowed'\ntry:\n    await read({path:?})\nexcept Exception as error:\n    result = type(error).__name__\nresult"
                ),
            )
            .await
            .unwrap();

            assert_eq!(complete(&execution).unwrap().head, format!("'{expected}'"));
        }
    }

    #[tokio::test]
    async fn private_host_calls_reject_malformed_and_unsupported_neutral_values_as_type_errors() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        for argument in ["{'path': 'file'}", "b'file'", "('file',)"] {
            let execution = execute_without_cancellation(
                &mut arena,
                &format!(
                    "result = 'allowed'\ntry:\n    await _kiki_read({argument})\nexcept Exception as error:\n    result = type(error).__name__\nresult"
                ),
            )
            .await
            .unwrap();

            assert_eq!(complete(&execution).unwrap().head, "'TypeError'");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_exposes_symlink_and_other_path_errors_as_builtin_exceptions() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("inside"), "inside").unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("directory/file"), "file").unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("directory", workspace.path().join("directory-link")).unwrap();
        let fifo = workspace.path().join("fifo");
        let fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let mut arena = arena(&workspace).await;

        for (path, expected) in [
            ("inside-link", "PermissionError"),
            ("outside-link", "PermissionError"),
            ("directory-link/file", "PermissionError"),
            ("fifo", "ValueError"),
        ] {
            let execution = execute_without_cancellation(
                &mut arena,
                &format!(
                    "result = 'allowed'\ntry:\n    await read({path:?})\nexcept Exception as error:\n    result = type(error).__name__\nresult"
                ),
            )
            .await
            .unwrap();

            assert_eq!(complete(&execution).unwrap().head, format!("'{expected}'"));
        }
    }

    #[tokio::test]
    async fn unknown_host_call_is_a_model_visible_python_error() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(&mut arena, "await missing_host_call()")
            .await
            .unwrap();

        assert!(python_error(&execution).text.contains("NameError"));
    }

    #[tokio::test]
    async fn direct_ambient_filesystem_access_is_denied() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "from pathlib import Path\nPath('/etc/passwd').read_text()",
        )
        .await
        .unwrap();

        assert!(python_error(&execution).text.contains("PermissionError"));
    }

    #[tokio::test]
    async fn captures_stdout_and_the_final_expression_across_a_host_suspension() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("lines.txt"), "one\ntwo\n").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "print('before')\ncontents = await read('lines.txt')\nprint('after')\nlen(contents)",
        )
        .await
        .unwrap();

        assert_eq!(execution.stdout.head, "before\nafter\n");
        assert_eq!(complete(&execution).unwrap().head, "8");
    }

    fn push_stdout(
        streams: &mut StreamAccumulators,
        text: &str,
        now: Instant,
        progress: &mut Vec<super::ArenaProgress>,
    ) {
        let mut capture = |event| progress.push(event);
        streams.push_with_progress(PrintStream::Stdout, text, now, &mut capture);
    }

    fn flush_stdout(
        streams: &mut StreamAccumulators,
        now: Instant,
        progress: &mut Vec<super::ArenaProgress>,
    ) {
        let mut capture = |event| progress.push(event);
        streams.flush_stdout(now, &mut capture);
    }

    fn stdout_text(progress: &[super::ArenaProgress]) -> String {
        progress
            .iter()
            .filter_map(|event| match event {
                super::ArenaProgress::StdoutDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn stdout_progress_emits_immediately_then_coalesces_until_the_next_due_producer_event() {
        let now = Instant::now();
        let mut streams = StreamAccumulators::new(TEST_LIMITS);
        let mut progress = Vec::new();

        push_stdout(&mut streams, "first", now, &mut progress);
        push_stdout(
            &mut streams,
            " second",
            now + Duration::from_millis(49),
            &mut progress,
        );
        push_stdout(
            &mut streams,
            " third",
            now + Duration::from_millis(50),
            &mut progress,
        );

        assert!(matches!(
            progress.as_slice(),
            [
                super::ArenaProgress::StdoutDelta { text: first },
                super::ArenaProgress::StdoutDelta { text: rest },
            ] if first == "first" && rest == " second third"
        ));
    }

    #[test]
    fn run_preview_keeps_the_command_prefix_and_last_eight_stdout_lines() {
        let mut preview = super::RunPreviewState::new(&format!("{}🦀tail", "x".repeat(200)));
        preview.push_stdout(
            &(1..=10)
                .map(|line| format!("line {line}\n"))
                .collect::<String>()
                .into_bytes(),
        );

        let preview = preview.snapshot();
        assert!(preview.command.ends_with('…'));
        assert!(preview.command.len() <= super::RUN_COMMAND_PREVIEW_BYTES + '…'.len_utf8());
        assert_eq!(
            preview.stdout,
            (3..=10)
                .map(|line| format!("line {line}\n"))
                .collect::<String>()
        );

        let mut invalid = super::RunPreviewState::new("invalid output");
        invalid.push_stdout(&vec![0xff; super::RUN_STDOUT_PREVIEW_BYTES]);
        let invalid = invalid.snapshot();
        assert!(invalid.stdout.len() <= super::RUN_STDOUT_PREVIEW_BYTES);
        assert!(
            invalid.command.len() + invalid.stdout.len()
                <= super::RUN_COMMAND_PREVIEW_BYTES + super::RUN_STDOUT_PREVIEW_BYTES
        );
    }

    #[test]
    fn run_preview_keeps_invalid_continuation_bytes_at_the_raw_tail_boundary() {
        let mut preview = super::RunPreviewState::new("invalid output");
        preview.push_stdout(&vec![0x80; super::RUN_STDOUT_PREVIEW_BYTES + 1]);

        let preview = preview.snapshot();
        assert_eq!(
            preview.stdout,
            "\u{fffd}".repeat(super::RUN_STDOUT_PREVIEW_BYTES / '\u{fffd}'.len_utf8())
        );
    }

    #[test]
    fn run_preview_skips_leading_blank_command_lines() {
        let preview = super::RunPreviewState::new("\r\n\nprintf first\nprintf second").snapshot();

        assert_eq!(preview.command, "printf first…");
    }

    #[test]
    fn stdout_progress_flushes_utf8_safe_chunks_when_pending_output_reaches_four_kib() {
        let now = Instant::now();
        let mut streams = StreamAccumulators::new(OBSERVATION_LIMITS);
        let mut progress = Vec::new();

        push_stdout(&mut streams, "start", now, &mut progress);
        push_stdout(
            &mut streams,
            &format!("{}é", "x".repeat(4 * 1024 - 1)),
            now + Duration::from_millis(1),
            &mut progress,
        );
        assert!(
            streams
                .stdout
                .live_stdout
                .as_ref()
                .expect("stdout has live state")
                .pending
                .len()
                <= 4 * 1024
        );
        flush_stdout(&mut streams, now + Duration::from_millis(2), &mut progress);

        assert_eq!(
            stdout_text(&progress),
            format!("start{}é", "x".repeat(4 * 1024 - 1))
        );
        assert!(progress.iter().all(|event| match event {
            super::ArenaProgress::StdoutDelta { text } => text.len() <= 4 * 1024,
            super::ArenaProgress::StdoutTruncated
            | super::ArenaProgress::ToolUpdated { .. }
            | super::ArenaProgress::RunPreviewUpdated { .. } => true,
        }));
        assert!(matches!(
            progress.get(1),
            Some(super::ArenaProgress::StdoutDelta { text }) if text.len() == 4 * 1024 - 1
        ));
    }

    #[test]
    fn stdout_progress_flushes_short_pending_output_when_execution_or_interruption_finishes() {
        let now = Instant::now();
        let mut streams = StreamAccumulators::new(TEST_LIMITS);
        let mut progress = Vec::new();

        push_stdout(&mut streams, "first", now, &mut progress);
        push_stdout(
            &mut streams,
            " pending",
            now + Duration::from_millis(1),
            &mut progress,
        );
        flush_stdout(&mut streams, now + Duration::from_millis(2), &mut progress);

        assert_eq!(stdout_text(&progress), "first pending");
        assert!(matches!(
            progress.as_slice(),
            [
                super::ArenaProgress::StdoutDelta { text: first },
                super::ArenaProgress::StdoutDelta { text: pending },
            ] if first == "first" && pending == " pending"
        ));

        let mut interruption = InterruptedExecutionState::new();
        push_stdout(
            &mut interruption.streams,
            "again",
            now + Duration::from_millis(60),
            &mut progress,
        );
        push_stdout(
            &mut interruption.streams,
            " later",
            now + Duration::from_millis(61),
            &mut progress,
        );
        let mut capture = |event| progress.push(event);
        let interrupted = interruption.finish(now + Duration::from_millis(62), &mut capture);

        assert_eq!(interrupted.stdout.head, "again later");
        assert_eq!(stdout_text(&progress), "first pendingagain later");
    }

    #[test]
    fn stdout_progress_preserves_fragmented_unicode_and_crlf_without_changing_observation() {
        let now = Instant::now();
        let fragments = ["head\r", "\n🦀", "\u{2028}tail"];
        let expected = {
            let mut accumulator = ObservationAccumulator::new(TEST_LIMITS);
            for fragment in fragments {
                accumulator.push(fragment);
            }
            accumulator.finish()
        };
        let mut streams = StreamAccumulators::new(TEST_LIMITS);
        let mut progress = Vec::new();

        for (index, fragment) in fragments.into_iter().enumerate() {
            push_stdout(
                &mut streams,
                fragment,
                now + Duration::from_millis(index as u64),
                &mut progress,
            );
        }
        flush_stdout(&mut streams, now + Duration::from_millis(4), &mut progress);

        assert_eq!(stdout_text(&progress), "head\r\n🦀\u{2028}tail");
        assert_eq!(streams.stdout.finish(), expected);
    }

    #[test]
    fn stdout_progress_marks_the_first_suffix_beyond_byte_or_line_limits_once() {
        let now = Instant::now();
        let byte_limits = ObservationLimits {
            max_total_bytes: 5,
            max_total_lines: 10,
            max_head_bytes: 5,
            max_head_lines: 10,
            max_tail_bytes: 5,
            max_tail_lines: 10,
        };
        let mut bytes = StreamAccumulators::new(byte_limits);
        let mut byte_progress = Vec::new();
        push_stdout(&mut bytes, "abcde", now, &mut byte_progress);
        push_stdout(
            &mut bytes,
            "f",
            now + Duration::from_millis(1),
            &mut byte_progress,
        );
        push_stdout(
            &mut bytes,
            "later",
            now + Duration::from_millis(2),
            &mut byte_progress,
        );

        assert_eq!(stdout_text(&byte_progress), "abcde");
        assert_eq!(
            byte_progress
                .iter()
                .filter(|event| matches!(event, super::ArenaProgress::StdoutTruncated))
                .count(),
            1
        );
        assert_eq!(bytes.stdout.finish(), observe("abcdeflater", byte_limits));

        let line_limits = ObservationLimits {
            max_total_bytes: 100,
            max_total_lines: 1,
            max_head_bytes: 100,
            max_head_lines: 1,
            max_tail_bytes: 100,
            max_tail_lines: 1,
        };
        let mut lines = StreamAccumulators::new(line_limits);
        let mut line_progress = Vec::new();
        push_stdout(&mut lines, "one\r", now, &mut line_progress);
        push_stdout(
            &mut lines,
            "\n",
            now + Duration::from_millis(1),
            &mut line_progress,
        );
        push_stdout(
            &mut lines,
            "two",
            now + Duration::from_millis(2),
            &mut line_progress,
        );

        assert_eq!(stdout_text(&line_progress), "one\r\n");
        assert_eq!(
            line_progress
                .iter()
                .filter(|event| matches!(event, super::ArenaProgress::StdoutTruncated))
                .count(),
            1
        );

        let mut actual_bytes = StreamAccumulators::new(OBSERVATION_LIMITS);
        let mut actual_byte_progress = Vec::new();
        let at_limit = "x".repeat(OBSERVATION_LIMITS.max_total_bytes);
        push_stdout(&mut actual_bytes, &at_limit, now, &mut actual_byte_progress);
        push_stdout(
            &mut actual_bytes,
            "beyond",
            now + Duration::from_millis(1),
            &mut actual_byte_progress,
        );
        assert_eq!(stdout_text(&actual_byte_progress), at_limit);
        assert_eq!(
            actual_byte_progress
                .iter()
                .filter(|event| matches!(event, super::ArenaProgress::StdoutTruncated))
                .count(),
            1
        );

        let mut actual_lines = StreamAccumulators::new(OBSERVATION_LIMITS);
        let mut actual_line_progress = Vec::new();
        let at_limit = "line\n".repeat(OBSERVATION_LIMITS.max_total_lines);
        push_stdout(&mut actual_lines, &at_limit, now, &mut actual_line_progress);
        push_stdout(
            &mut actual_lines,
            "beyond",
            now + Duration::from_millis(1),
            &mut actual_line_progress,
        );
        assert_eq!(stdout_text(&actual_line_progress), at_limit);
        assert_eq!(
            actual_line_progress
                .iter()
                .filter(|event| matches!(event, super::ArenaProgress::StdoutTruncated))
                .count(),
            1
        );
    }

    #[test]
    fn routes_stderr_to_its_own_accumulator() {
        let mut streams = StreamAccumulators::new(TEST_LIMITS);
        let mut progress = |_| {};

        streams.push_with_progress(
            PrintStream::Stderr,
            "warning\n",
            Instant::now(),
            &mut progress,
        );
        streams.push_with_progress(
            PrintStream::Stdout,
            "output\n",
            Instant::now(),
            &mut progress,
        );

        assert_eq!(streams.stdout.finish().head, "output\n");
        assert_eq!(streams.stderr.finish().head, "warning\n");
    }

    #[test]
    fn truncates_by_line_count_without_losing_boundary_metadata() {
        let observation = observe("a\nb\nc\nd\n", TEST_LIMITS);

        assert_eq!(observation.head, "a\nb\n");
        assert_eq!(observation.tail.as_deref(), Some("d\n"));
        assert_eq!(observation.head_lines, 2);
        assert_eq!(observation.tail_lines, 1);
        assert_eq!(observation.total_lines, 4);
        assert_eq!(observation.total_bytes, 8);
        assert_eq!(observation.omitted_lines, 1);
        assert_eq!(observation.omitted_bytes, 2);
        assert!(!observation.partial_lines);
    }

    #[test]
    fn truncates_by_bytes_without_losing_boundary_metadata() {
        let limits = ObservationLimits {
            max_total_bytes: 5,
            max_total_lines: 10,
            max_head_bytes: 3,
            max_head_lines: 10,
            max_tail_bytes: 2,
            max_tail_lines: 10,
        };

        let observation = observe("abcdef", limits);

        assert_eq!(observation.head, "abc");
        assert_eq!(observation.tail.as_deref(), Some("ef"));
        assert_eq!(observation.head_lines, 1);
        assert_eq!(observation.tail_lines, 1);
        assert_eq!(observation.total_lines, 1);
        assert_eq!(observation.omitted_lines, 0);
        assert_eq!(observation.omitted_bytes, 1);
        assert!(observation.head_ends_mid_line);
        assert!(observation.tail_starts_mid_line);
        assert!(observation.partial_lines);
    }

    #[test]
    fn truncates_by_bytes_and_line_count_together() {
        let limits = ObservationLimits {
            max_total_bytes: 8,
            max_total_lines: 3,
            max_head_bytes: 4,
            max_head_lines: 2,
            max_tail_bytes: 3,
            max_tail_lines: 1,
        };

        let observation = observe("aa\nbb\ncc\ndd\n", limits);

        assert_eq!(observation.head, "aa\nb");
        assert_eq!(observation.tail.as_deref(), Some("dd\n"));
        assert_eq!(observation.omitted_bytes, 5);
        assert_eq!(observation.omitted_lines, 1);
        assert!(observation.partial_lines);
    }

    #[test]
    fn retains_valid_unicode_prefix_and_suffix_for_an_oversized_line() {
        let limits = ObservationLimits {
            max_total_bytes: 10,
            max_total_lines: 10,
            max_head_bytes: 5,
            max_head_lines: 10,
            max_tail_bytes: 5,
            max_tail_lines: 10,
        };

        let observation = observe("éééééé", limits);

        assert_eq!(observation.head, "éé");
        assert_eq!(observation.tail.as_deref(), Some("éé"));
        assert_eq!(observation.head_lines, 1);
        assert_eq!(observation.tail_lines, 1);
        assert_eq!(observation.omitted_bytes, 4);
        assert_eq!(observation.omitted_lines, 0);
        assert!(observation.partial_lines);
    }

    #[test]
    fn tail_retention_keeps_crlf_together_across_fragments() {
        let limits = ObservationLimits {
            max_total_bytes: 1,
            max_total_lines: 10,
            max_head_bytes: 0,
            max_head_lines: 10,
            max_tail_bytes: 2,
            max_tail_lines: 10,
        };
        let whole = observe("a\r\n", limits);
        let mut fragments = ObservationAccumulator::new(limits);
        for character in "a\r\n".chars() {
            let mut buffer = [0; 4];
            fragments.push(character.encode_utf8(&mut buffer));
        }

        assert_eq!(fragments.finish(), whole);
        assert_eq!(whole.tail.as_deref(), Some("\r\n"));
        assert!(whole.tail_starts_mid_line);
    }

    #[test]
    fn is_independent_of_print_fragment_boundaries_and_matches_python_line_separators() {
        let text =
            "a\r\nb\rc\u{000b}d\u{000c}e\u{001c}f\u{001d}g\u{001e}h\u{0085}i\u{2028}j\u{2029}tail";
        let limits = ObservationLimits {
            max_total_bytes: 1_000,
            max_total_lines: 100,
            max_head_bytes: 1_000,
            max_head_lines: 100,
            max_tail_bytes: 1_000,
            max_tail_lines: 100,
        };
        let whole = observe(text, limits);
        let mut fragments = ObservationAccumulator::new(limits);
        for character in text.chars() {
            let mut buffer = [0; 4];
            fragments.push(character.encode_utf8(&mut buffer));
        }

        assert_eq!(fragments.finish(), whole);
        assert_eq!(whole.head, text);
        assert_eq!(whole.total_lines, 11);

        let repeated = text.repeat(3);
        let whole = observe(&repeated, TEST_LIMITS);
        let mut fragments = ObservationAccumulator::new(TEST_LIMITS);
        for character in repeated.chars() {
            let mut buffer = [0; 4];
            fragments.push(character.encode_utf8(&mut buffer));
        }

        assert_eq!(fragments.finish(), whole);
    }

    #[test]
    fn retains_a_large_single_line_from_one_character_fragments_in_linear_operations() {
        const SCALARS: usize = 10_000;
        let limits = ObservationLimits {
            max_total_bytes: 1,
            max_total_lines: usize::MAX,
            max_head_bytes: 0,
            max_head_lines: usize::MAX,
            max_tail_bytes: SCALARS,
            max_tail_lines: 1,
        };
        let mut accumulator = ObservationAccumulator::new(limits);

        for _ in 0..SCALARS {
            accumulator.push("x");
        }

        let tail = &accumulator
            .truncated
            .as_ref()
            .expect("large line should truncate the observation")
            .tail;
        assert!(
            tail.scalar_operations <= SCALARS * 3,
            "processed {} scalar operations for {SCALARS} input scalars",
            tail.scalar_operations
        );
        let observation = accumulator.finish();
        assert_eq!(
            observation.tail.as_deref(),
            Some("x".repeat(SCALARS).as_str())
        );
    }

    #[test]
    fn caps_python_error_text_at_a_utf8_boundary() {
        let error = PythonError::from_text("é".repeat(20_000));

        assert!(error.text.len() <= super::ERROR_BYTES);
        assert!(std::str::from_utf8(error.text.as_bytes()).is_ok());
        assert_eq!(error.omitted_bytes, 40_000 - error.text.len());
    }

    #[test]
    fn bounds_interruption_diagnostics_and_keeps_tool_evidence() {
        let unbounded = "é".repeat(INTERRUPTED_EVIDENCE_TEXT_LIMIT / 2 + 1);
        let expected = "é".repeat(INTERRUPTED_EVIDENCE_TEXT_LIMIT / 2);
        let mut interruption = InterruptedExecutionState::new();
        interruption.began = true;
        interruption.streams.stdout.push("before\n");
        let token = interruption.tools.admit(ToolName::Read);
        interruption.tools.settle(token, ToolCallSummary::Failed);

        let mut progress = |_| {};
        let error = ArenaError::unsupported_suspension(unbounded)
            .with_interrupted_execution(interruption.finish(Instant::now(), &mut progress));

        let ArenaError::UnsupportedSuspension {
            message,
            execution: Some(execution),
        } = error
        else {
            panic!("expected unsupported suspension with interruption evidence");
        };
        assert_eq!(message, expected);
        assert!(message.is_char_boundary(message.len()));
        assert!(execution.began);
        assert_eq!(execution.stdout.head, "before\n");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Failed,
            }]
        );
    }

    #[tokio::test]
    async fn ordinary_python_errors_commit_preceding_mutation() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("read-before-error"), "contents").unwrap();
        let mut arena = arena(&workspace).await;
        #[cfg(unix)]
        let worker_pid = arena_worker_pid(&arena);

        let error = execute_without_cancellation(
            &mut arena,
            "value = 41\nawait read('read-before-error')\nraise ValueError('expected')",
        )
        .await
        .unwrap();
        assert!(python_error(&error).text.contains("ValueError"));
        assert!(has_authorisation(&arena, "read-before-error"));

        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value + 1")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "42"
        );
        #[cfg(unix)]
        assert_eq!(arena_worker_pid(&arena), worker_pid);
    }

    #[test]
    fn recognises_only_the_configured_canonical_soft_memory_diagnostic() {
        const LIMIT: usize = 16;
        let canonical = "memory limit exceeded: 17 bytes > 16 bytes";

        assert_eq!(parse_soft_memory_limit(canonical), Some((17, LIMIT)));
        assert!(is_unsafe_soft_memory_error(canonical, Some(LIMIT)));
        assert!(!is_unsafe_soft_memory_error(canonical, Some(32)));
        for diagnostic in [
            "memory limit exceeded: 017 bytes > 16 bytes",
            "memory limit exceeded: +17 bytes > 16 bytes",
            "memory limit exceeded: 17 bytes > 016 bytes",
            "memory limit exceeded: 17 bytes > +16 bytes",
        ] {
            assert_eq!(parse_soft_memory_limit(diagnostic), None);
            assert!(!is_unsafe_soft_memory_error(diagnostic, Some(LIMIT)));
        }
        assert!(!is_unsafe_soft_memory_error(
            "memory limit exceeded: sixteen bytes > 16 bytes",
            Some(LIMIT)
        ));
        assert!(!is_unsafe_soft_memory_error(
            "memory limit exceeded: 17 bytes > sixteen bytes",
            Some(LIMIT)
        ));
        assert!(!is_unsafe_soft_memory_error(
            "memory limit exceeded: 18446744073709551616 bytes > 16 bytes",
            Some(LIMIT)
        ));
        assert!(!is_unsafe_soft_memory_error(
            "memory limit exceeded: 16 bytes > 16 bytes",
            Some(LIMIT)
        ));
        assert!(!is_unsafe_soft_memory_error(
            "memory limit exceeded: 15 bytes > 16 bytes",
            Some(LIMIT)
        ));
        assert!(!is_unsafe_soft_memory_error(
            "memory limit exceeded: 17 bytes > 16 bytes more",
            Some(LIMIT)
        ));
    }

    #[tokio::test]
    async fn user_raised_fatal_message_memory_error_commits_mutation_and_keeps_the_worker() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena(&workspace).await;
        #[cfg(unix)]
        let worker_pid = arena_worker_pid(&arena);

        let execution = execute_without_cancellation(
            &mut arena,
            "await read('file')\nvalue = 99\nraise MemoryError('the worker exceeded its memory limit and was terminated')",
        )
        .await
        .unwrap();

        assert!(python_error(&execution).text.contains("Traceback"));
        assert!(python_error(&execution).text.contains("MemoryError"));
        assert!(has_authorisation(&arena, "file"));
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "99"
        );
        #[cfg(unix)]
        assert_eq!(arena_worker_pid(&arena), worker_pid);
    }

    #[test]
    fn rejects_a_near_match_empty_traceback_fatal_memory_error() {
        let error = monty_types::MontyException::new(
            monty_types::ExcType::MemoryError,
            Some("the worker exceeded its memory limit and was terminated.".to_owned()),
        );

        assert!(error.traceback().is_empty());
        assert!(!is_unsafe_memory_error(&error, None));
    }

    async fn set_trusted_value(arena: &mut Arena) {
        execute_without_cancellation(arena, "value = 7")
            .await
            .expect("set trusted value");
    }

    async fn assert_trusted_value(arena: &mut Arena) {
        assert_eq!(
            complete(
                &execute_without_cancellation(arena, "value")
                    .await
                    .expect("execute after recovery")
            )
            .expect("value result")
            .head,
            "7"
        );
    }

    #[cfg(unix)]
    fn arena_worker_pid(arena: &Arena) -> u32 {
        arena
            .checkout
            .as_ref()
            .and_then(Checkout::pid)
            .expect("local arena worker should have a process ID")
    }

    #[tokio::test]
    async fn pre_cancelled_execution_has_no_driver_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = arena
            .execute("print('not run')", &cancellation)
            .await
            .unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert!(!execution.began);
        assert_eq!(execution.stdout.total_bytes, 0);
        assert_eq!(execution.stderr.total_bytes, 0);
        assert!(execution.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn stdout_progress_arrives_before_a_gated_read_and_survives_cancellation() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut progress = Vec::new();
        let error = {
            let mut capture = |event| progress.push(event);
            let mut execution = Box::pin(arena.execute_with_progress(
                "print('before')\nawait read('pending')",
                &cancellation,
                &mut capture,
            ));

            tokio::select! {
                _ = gate.wait_until_read_started() => {}
                result = &mut execution => panic!("execution finished before its read started: {result:?}"),
            }
            cancellation.cancel();
            execution.await.unwrap_err()
        };

        assert!(matches!(
            progress.as_slice(),
            [
                super::ArenaProgress::StdoutDelta { text },
                super::ArenaProgress::ToolUpdated { call: ToolCall { tool: ToolName::Read, summary: ToolCallSummary::Pending }, .. },
            ] if text == "before\n"
        ));
        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(execution.stdout.head, "before\n");
    }

    #[tokio::test]
    async fn asyncio_gather_resumes_every_read_in_argument_order() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("first"), "first").unwrap();
        fs::write(workspace.path().join("second"), "second").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "import asyncio\nvalues = await asyncio.gather(read('first'), read('second'))\nvalues == ['first', 'second']",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(5),
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(6),
                },
            ]
        );
    }

    #[tokio::test]
    async fn asyncio_gather_runs_all_read_only_tools_concurrently() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "match\n").unwrap();
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "import asyncio\nresults = await asyncio.gather(read('file'), list_dir(), glob('*'), grep('match'))\nresults[0] == 'match\\n' and len(results[1]) == len(results[2]) == len(results[3]) == 1",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_calls_started(4) => {}
            result = &mut execution => panic!("execution finished before all inspection calls started: {result:?}"),
        }
        assert_eq!(gate.active_call_ids().len(), 4);
        for call_id in gate.started_call_ids() {
            gate.release(call_id);
        }

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(6),
                },
                ToolCall {
                    tool: ToolName::ListDir,
                    summary: ToolCallSummary::Count(1),
                },
                ToolCall {
                    tool: ToolName::Glob,
                    summary: ToolCallSummary::Count(1),
                },
                ToolCall {
                    tool: ToolName::Grep,
                    summary: ToolCallSummary::Count(1),
                },
            ]
        );
    }

    #[tokio::test]
    async fn asyncio_gather_settles_siblings_when_one_tool_fails() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("present"), "contents").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "import asyncio\nawait asyncio.gather(read('missing'), read('present'))",
        )
        .await
        .unwrap();

        assert!(python_error(&execution).text.contains("FileNotFoundError"));
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Failed,
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(8),
                },
            ]
        );
        assert!(!has_authorisation(&arena, "missing"));
        assert!(has_authorisation(&arena, "present"));
    }

    #[tokio::test]
    async fn asyncio_gather_correlates_reversed_host_settlement_to_trace_rows() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("first"), "first").unwrap();
        fs::write(workspace.path().join("second"), "second").unwrap();
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut progress = Vec::new();
        let execution = {
            let mut capture = |event| progress.push(event);
            let mut execution = Box::pin(arena.execute_with_progress(
                "import asyncio\nvalues = await asyncio.gather(read('first'), read('second'))\nvalues == ['first', 'second']",
                &cancellation,
                &mut capture,
            ));

            tokio::select! {
                _ = gate.wait_until_calls_started(2) => {}
                result = &mut execution => panic!("execution finished before both calls started: {result:?}"),
            }
            let call_ids = gate.started_call_ids();
            assert_eq!(call_ids.len(), 2);
            gate.release(call_ids[1]);
            tokio::select! {
                _ = gate.wait_until_driver_settled() => {}
                result = &mut execution => panic!("execution finished before the reversed call settled: {result:?}"),
            }
            assert_eq!(gate.driver_settled_call_ids(), [call_ids[1]]);
            gate.release(call_ids[0]);
            tokio::select! {
                _ = gate.wait_until_driver_settled() => {}
                result = &mut execution => panic!("execution finished before the first call settled: {result:?}"),
            }
            execution.await.unwrap()
        };

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(5),
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(6),
                },
            ]
        );
        assert_eq!(gate.resume_futures_calls(), 1);
        let updates = progress
            .into_iter()
            .filter_map(|event| match event {
                super::ArenaProgress::ToolUpdated { token, call } => Some((token, call)),
                super::ArenaProgress::RunPreviewUpdated { .. }
                | super::ArenaProgress::StdoutDelta { .. }
                | super::ArenaProgress::StdoutTruncated => None,
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            updates.as_slice(),
            [
                (first_token, ToolCall { summary: ToolCallSummary::Pending, .. }),
                (second_token, ToolCall { summary: ToolCallSummary::Pending, .. }),
                (settled_first, ToolCall { summary: ToolCallSummary::Count(_), .. }),
                (settled_second, ToolCall { summary: ToolCallSummary::Count(_), .. }),
            ] if first_token != second_token
                && [settled_first, settled_second].contains(&first_token)
                && [settled_first, settled_second].contains(&second_token)
        ));
        assert!(
            updates[2..]
                .iter()
                .any(|(_, call)| { matches!(call.summary, ToolCallSummary::Count(5)) })
        );
        assert!(
            updates[2..]
                .iter()
                .any(|(_, call)| { matches!(call.summary, ToolCallSummary::Count(6)) })
        );
    }

    #[tokio::test]
    async fn reversed_same_path_reads_apply_effects_in_pending_call_order() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("file");
        fs::write(&path, "first").unwrap();
        let mut expected_arena = arena(&workspace).await;
        execute_without_cancellation(&mut expected_arena, "await read('file')")
            .await
            .unwrap();
        let first_authorisations = expected_arena.checkpoint.authorisations.clone();
        drop(expected_arena);

        let (mut actual_arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(actual_arena.execute(
            "import asyncio\nawait asyncio.gather(read('file'), read('file'))",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = &mut execution => panic!("execution finished before both reads started: {result:?}"),
        }
        let call_ids = gate.started_call_ids();
        gate.release(call_ids[1]);
        tokio::select! {
            _ = gate.wait_until_driver_settled() => {}
            result = &mut execution => panic!("execution finished before the second read settled: {result:?}"),
        }
        fs::write(&path, "second").unwrap();
        let mut second_expected_arena = arena(&workspace).await;
        execute_without_cancellation(&mut second_expected_arena, "await read('file')")
            .await
            .unwrap();
        let second_authorisations = second_expected_arena.checkpoint.authorisations.clone();
        drop(second_expected_arena);
        let pending_call_ids = gate.pending_call_ids();
        assert_eq!(pending_call_ids.len(), 2);
        let expected_authorisations = match pending_call_ids.last() {
            Some(call_id) if *call_id == call_ids[1] => first_authorisations,
            Some(call_id) if *call_id == call_ids[0] => second_authorisations,
            _ => panic!("pending calls must be the gated reads"),
        };
        gate.release(call_ids[0]);

        execution.await.unwrap();
        assert_eq!(
            actual_arena.checkpoint.authorisations,
            expected_authorisations
        );
    }

    #[tokio::test]
    async fn asyncio_gather_limits_active_host_calls_and_queues_the_rest() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 0..17 {
            fs::write(
                workspace.path().join(format!("file-{index}")),
                index.to_string(),
            )
            .unwrap();
        }
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let calls = (0..17)
            .map(|index| format!("read('file-{index}')"))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!(
            "import asyncio\nvalues = await asyncio.gather({calls})\nvalues == [str(index) for index in range(17)]"
        );
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut execution => panic!("execution finished before 16 calls started: {result:?}"),
        }
        let started = gate.started_call_ids();
        assert_eq!(started.len(), 16);
        assert_eq!(gate.active_call_ids().len(), 16);
        assert_eq!(gate.maximum_active(), 16);
        for call_id in started {
            gate.release(call_id);
        }
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut execution => panic!("execution finished before the queued call started: {result:?}"),
        }
        let queued_call = *gate
            .started_call_ids()
            .last()
            .expect("queued call must have started");
        gate.release(queued_call);

        let execution = execution.await.unwrap();
        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(execution.tool_calls.len(), 17);
        assert_eq!(gate.maximum_active(), 16);
        assert_eq!(gate.resume_futures_calls(), 1);
    }

    #[tokio::test]
    async fn a_failed_gathered_tool_does_not_stop_queued_siblings() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 1..17 {
            fs::write(
                workspace.path().join(format!("file-{index}")),
                index.to_string(),
            )
            .unwrap();
        }
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let calls = std::iter::once("read('missing')".to_owned())
            .chain((1..17).map(|index| format!("read('file-{index}')")))
            .collect::<Vec<_>>()
            .join(", ");
        let code = format!("import asyncio\nawait asyncio.gather({calls})");
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut execution => panic!("execution finished before 16 calls started: {result:?}"),
        }
        for call_id in gate.started_call_ids() {
            gate.release(call_id);
        }
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut execution => panic!("execution finished before the queued sibling started: {result:?}"),
        }
        let queued_call = *gate
            .started_call_ids()
            .last()
            .expect("queued sibling must have started");
        gate.release(queued_call);

        let execution = execution.await.unwrap();
        assert!(python_error(&execution).text.contains("FileNotFoundError"));
        assert_eq!(execution.tool_calls.len(), 17);
        assert!(matches!(
            execution.tool_calls.first(),
            Some(ToolCall {
                summary: ToolCallSummary::Failed,
                ..
            })
        ));
        assert!(
            execution.tool_calls[1..]
                .iter()
                .all(|call| matches!(call.summary, ToolCallSummary::Count(_)))
        );
        assert_eq!(gate.resume_futures_calls(), 1);
    }

    #[tokio::test]
    async fn nested_gathers_dispatch_later_batches_after_the_first_settles() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a"), "a").unwrap();
        fs::write(workspace.path().join("b"), "b").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "import asyncio\nasync def inspect(path):\n    first = await read(path)\n    rest = await asyncio.gather(read(path), read(path))\n    return first + rest[0] + rest[1]\nresults = await asyncio.gather(inspect('a'), inspect('b'))\nresults == ['aaa', 'bbb']",
        )
        .await
        .unwrap();

        assert_eq!(complete(&execution).unwrap().head, "True");
        assert_eq!(execution.tool_calls.len(), 6);
        assert!(execution.tool_calls.iter().all(|call| {
            call == &ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(1),
            }
        }));
    }

    #[cfg(unix)]
    async fn assert_group_absent(pgid: libc::pid_t) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if unsafe { libc::kill(-pgid, 0) } == -1 {
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ESRCH)
                    );
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("timed out waiting for command group cleanup");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_cleans_a_running_command_process_group() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_command_gate(&workspace, Duration::from_secs(5)).await;
        let cancellation = CancellationToken::new();
        let mut execution =
            Box::pin(arena.execute("await run('tail -f /dev/null')", &cancellation));

        let pgid = tokio::select! {
            pgid = gate.wait_until_started() => pgid,
            result = &mut execution => panic!("execution finished before its command started: {result:?}"),
        };
        assert_eq!(unsafe { libc::kill(-pgid, 0) }, 0);
        cancellation.cancel();

        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::Cancelled { .. }
        ));
        gate.wait_until_kill_sent(pgid).await;
        gate.wait_until_cleaned_up(pgid).await;
        assert_group_absent(pgid).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deadline_cleans_a_command_and_restores_the_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("trusted"), "contents").unwrap();
        let (mut arena, gate) = arena_with_command_gate(&workspace, Duration::from_secs(1)).await;
        set_trusted_value(&mut arena).await;
        execute_without_cancellation(&mut arena, "await read('trusted')")
            .await
            .unwrap();
        let trusted_checkpoint = arena.checkpoint();
        let cancellation = CancellationToken::new();
        let mut execution =
            Box::pin(arena.execute("value = 99\nawait run('tail -f /dev/null')", &cancellation));

        let pgid = tokio::select! {
            pgid = gate.wait_until_started() => pgid,
            result = &mut execution => panic!("execution finished before its command started: {result:?}"),
        };
        assert_eq!(unsafe { libc::kill(-pgid, 0) }, 0);
        let error = execution.await.unwrap_err();

        let ArenaError::TimedOut { execution } = error else {
            panic!("expected interrupted timeout");
        };
        gate.wait_until_kill_sent(pgid).await;
        gate.wait_until_cleaned_up(pgid).await;
        assert_group_absent(pgid).await;
        assert_eq!(arena.checkpoint(), trusted_checkpoint);
        assert!(has_authorisation(&arena, "trusted"));
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Pending,
            }]
        );
        assert_trusted_value(&mut arena).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interruption_preserves_completed_command_effects_and_freezes_safe_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("CWD_SENTINEL")).unwrap();
        let (mut arena, gate) = arena_with_command_gate(&workspace, Duration::from_secs(5)).await;
        set_trusted_value(&mut arena).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "await run('printf effect > SIDE_EFFECT_SENTINEL')\nvalue = 99\nawait run('tail -f /dev/null # COMMAND_SENTINEL', 'CWD_SENTINEL')",
            &cancellation,
        ));

        let first_pgid = tokio::select! {
            pgid = gate.wait_until_started() => pgid,
            result = &mut execution => panic!("execution finished before its first command started: {result:?}"),
        };
        gate.wait_until_kill_sent(first_pgid).await;
        gate.wait_until_cleaned_up(first_pgid).await;
        let second_pgid = tokio::select! {
            pgid = gate.wait_until_started() => pgid,
            result = &mut execution => panic!("execution finished before its second command started: {result:?}"),
        };
        assert_eq!(unsafe { libc::kill(-second_pgid, 0) }, 0);
        cancellation.cancel();

        let error = execution.await.unwrap_err();
        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        gate.wait_until_kill_sent(second_pgid).await;
        gate.wait_until_cleaned_up(second_pgid).await;
        assert_group_absent(second_pgid).await;
        assert_eq!(
            execution.tool_calls,
            [
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Pending,
                },
            ]
        );
        let retained = format!("{execution:?}");
        for sentinel in ["COMMAND_SENTINEL", "CWD_SENTINEL", "SIDE_EFFECT_SENTINEL"] {
            assert!(!retained.contains(sentinel), "retained {sentinel}");
        }
        assert_eq!(
            fs::read_to_string(workspace.path().join("SIDE_EFFECT_SENTINEL")).unwrap(),
            "effect"
        );
        assert_trusted_value(&mut arena).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_wins_before_a_held_overflow_error_is_published() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_command_gate(&workspace, Duration::from_secs(5)).await;
        gate.hold_error_publication();
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "await run('dd if=/dev/zero bs=8388609 count=1 2>/dev/null')",
            &cancellation,
        ));

        let pgid = tokio::select! {
            pgid = gate.wait_until_started() => pgid,
            result = &mut execution => panic!("execution finished before its command started: {result:?}"),
        };
        gate.wait_until_kill_sent(pgid).await;
        gate.wait_until_cleaned_up(pgid).await;
        cancellation.cancel();
        let error = execution.await.unwrap_err();
        gate.release_error_publication();
        gate.wait_until_supervisor_exited().await;

        let ArenaError::Cancelled { execution } = error else {
            panic!("cancellation must win over command overflow");
        };
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Pending,
            }]
        );
        assert!(!gate.error_published());
    }

    #[tokio::test]
    async fn cancellation_drops_active_batch_calls_and_restores_the_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 0..18 {
            fs::write(
                workspace.path().join(format!("file-{index}")),
                index.to_string(),
            )
            .unwrap();
        }
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        set_trusted_value(&mut arena).await;
        let cancellation = CancellationToken::new();
        let code = format!("value = 99\n{}", gathered_reads(18));
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut execution => panic!("execution finished before 16 calls started: {result:?}"),
        }
        let released_call = *gate
            .started_call_ids()
            .first()
            .expect("the first batch call must have started");
        gate.release(released_call);
        tokio::select! {
            _ = gate.wait_until_driver_settled() => {}
            result = &mut execution => panic!("execution finished before the released call settled: {result:?}"),
        }
        assert_eq!(gate.driver_settled_call_ids(), [released_call]);
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut execution => panic!("execution finished before the queued call started: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(gate.started_call_ids().len(), 17);
        assert!(gate.active_call_ids().is_empty());
        assert_eq!(gate.maximum_active(), 16);
        assert_interrupted_batch_trace(&execution.tool_calls);
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        assert_trusted_value(&mut arena).await;
    }

    #[tokio::test]
    async fn cancellation_discards_settled_mutation_authorisation_and_retains_safe_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute(
            "import asyncio\nawait asyncio.gather(write('created-on-cancel', 'first'), read('still-pending'))",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its pending read started: {result:?}"),
        }
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut execution => panic!("execution finished before its mutation settled: {result:?}"),
        }
        cancellation.cancel();

        let error = execution.await.unwrap_err();
        gate.resume_driver();
        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(
            execution
                .tool_calls
                .iter()
                .filter(|call| matches!(call.summary, ToolCallSummary::Created))
                .count(),
            1
        );
        assert_eq!(
            execution
                .tool_calls
                .iter()
                .filter(|call| matches!(call.summary, ToolCallSummary::Pending))
                .count(),
            1
        );
        assert!(execution.read_paths.is_empty());
        assert_eq!(
            execution.modified_paths,
            [PathBuf::from("created-on-cancel")]
        );
        assert!(!format!("{execution:?}").contains("created-on-cancel"));
        assert!(!format!("{:?}", execution.tool_calls).contains("created-on-cancel"));
        assert!(!format!("{:?}", execution.tool_calls).contains("still-pending"));
        assert!(!has_authorisation(&arena, "created-on-cancel"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("created-on-cancel")).unwrap(),
            "first"
        );

        let later =
            execute_without_cancellation(&mut arena, "await write('created-on-cancel', 'later')")
                .await
                .unwrap();
        assert!(python_error(&later).text.contains("RuntimeError"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("created-on-cancel")).unwrap(),
            "first"
        );
    }

    #[tokio::test]
    async fn timeout_drops_active_batch_calls_and_restores_the_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        for index in 0..18 {
            fs::write(
                workspace.path().join(format!("file-{index}")),
                index.to_string(),
            )
            .unwrap();
        }
        let gate = BatchTestGate::new();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_batch_test_gate(gate.clone());
        let mut arena = Arena::with_limits(
            worker_path(),
            tool_host,
            Duration::from_secs(2),
            Some(64 * 1024 * 1024),
        )
        .await
        .expect("create arena");
        set_trusted_value(&mut arena).await;
        let cancellation = CancellationToken::new();
        let code = format!("value = 99\n{}", gathered_reads(18));
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut execution => panic!("execution finished before 16 calls started: {result:?}"),
        }
        let released_call = *gate
            .started_call_ids()
            .first()
            .expect("the first batch call must have started");
        gate.release(released_call);
        tokio::select! {
            _ = gate.wait_until_driver_settled() => {}
            result = &mut execution => panic!("execution finished before the released call settled: {result:?}"),
        }
        assert_eq!(gate.driver_settled_call_ids(), [released_call]);
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut execution => panic!("execution finished before the queued call started: {result:?}"),
        }
        let error = execution.await.unwrap_err();

        let ArenaError::TimedOut { execution } = error else {
            panic!("expected interrupted timeout");
        };
        assert_eq!(gate.started_call_ids().len(), 17);
        assert!(gate.active_call_ids().is_empty());
        assert_eq!(gate.maximum_active(), 16);
        assert_interrupted_batch_trace(&execution.tool_calls);
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        assert_trusted_value(&mut arena).await;
    }

    #[tokio::test]
    async fn cancellation_discards_settled_read_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena(&workspace).await;
        let cancellation = CancellationToken::new();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let mut ready_sender = Some(ready_sender);
        let mut progress = move |event| {
            if matches!(event, super::ArenaProgress::StdoutDelta { ref text } if text == "ready\n")
            {
                ready_sender
                    .take()
                    .expect("ready output is emitted once")
                    .send(())
                    .expect("ready receiver remains open");
            }
        };
        let mut execution = Box::pin(arena.execute_with_progress(
            "await read('file')\nprint('ready')\nwhile True:\n    pass",
            &cancellation,
            &mut progress,
        ));

        tokio::select! {
            _ = ready_receiver => {}
            result = &mut execution => panic!("execution finished before its read effect settled: {result:?}"),
        }
        cancellation.cancel();
        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::Cancelled { .. }
        ));
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
    }

    #[tokio::test]
    async fn timeout_discards_settled_read_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena =
            arena_with_limits(&workspace, Duration::from_secs(1), Some(64 * 1024 * 1024)).await;
        let cancellation = CancellationToken::new();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let mut ready_sender = Some(ready_sender);
        let mut progress = move |event| {
            if matches!(event, super::ArenaProgress::StdoutDelta { ref text } if text == "ready\n")
            {
                ready_sender
                    .take()
                    .expect("ready output is emitted once")
                    .send(())
                    .expect("ready receiver remains open");
            }
        };
        let mut execution = Box::pin(arena.execute_with_progress(
            "await read('file')\nprint('ready')\nwhile True:\n    pass",
            &cancellation,
            &mut progress,
        ));

        tokio::select! {
            _ = ready_receiver => {}
            result = &mut execution => panic!("execution finished before its read effect settled: {result:?}"),
        }
        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::TimedOut { .. }
        ));
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
    }

    #[tokio::test]
    async fn execution_retains_normalised_file_operations_without_exposing_paths_in_tool_calls() {
        let workspace = tempfile::tempdir().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("nested/read-only"), "read").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "await write('./nested//mutated', 'before')\nawait read('nested/./read-only')\nawait read('./nested//mutated')\nawait edit('nested/mutated', 'before', 'after')\nawait write('nested/./second', 'second')",
        )
        .await
        .unwrap();

        assert_eq!(
            execution.read_paths,
            vec![
                PathBuf::from("nested/mutated"),
                PathBuf::from("nested/read-only"),
            ]
        );
        assert_eq!(
            execution.modified_paths,
            vec![
                PathBuf::from("nested/mutated"),
                PathBuf::from("nested/second"),
            ]
        );
        assert!(!format!("{execution:?}").contains("nested/mutated"));
        assert!(!format!("{:?}", execution.tool_calls).contains("nested/mutated"));
    }

    #[tokio::test]
    async fn file_operations_survive_python_errors_and_settled_cancellation() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("settled"), "contents").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "value = 'committed'\nawait read('./settled')\nawait write('written', 'contents')\nraise RuntimeError('expected')",
        )
        .await
        .unwrap();
        assert!(matches!(
            execution.outcome,
            ExecutionOutcome::PythonError(_)
        ));
        assert_eq!(execution.read_paths, [PathBuf::from("settled")]);
        assert_eq!(execution.modified_paths, [PathBuf::from("written")]);
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .expect("committed Python state")
            .head,
            "'committed'"
        );

        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        execute_without_cancellation(&mut arena, "value = 'before'")
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let mut execution =
            Box::pin(arena.execute("value = 'discarded'\nawait read('settled')", &cancellation));
        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        gate.resume_read();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut execution => panic!("execution finished before its read settled: {result:?}"),
        }
        cancellation.cancel();
        let ArenaError::Cancelled { execution } = execution.await.unwrap_err() else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(execution.read_paths, [PathBuf::from("settled")]);
        assert!(execution.modified_paths.is_empty());
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .expect("restored Python state")
            .head,
            "'before'"
        );
    }

    #[tokio::test]
    async fn read_call_trace_keeps_ordered_successes_and_failures() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("lines"), "one\ntwo\n").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "contents = await read('lines')\ntry:\n    await read('/outside')\nexcept PermissionError:\n    pass\nlen(contents)",
        )
        .await
        .unwrap();

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
        assert_eq!(execution.read_paths, [PathBuf::from("lines")]);
        assert!(execution.modified_paths.is_empty());
    }

    #[tokio::test]
    async fn read_call_trace_keeps_pending_calls_when_cancellation_interrupts_a_read() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute("await read('pending')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            }]
        );
        assert!(execution.read_paths.is_empty());
        assert!(execution.modified_paths.is_empty());
    }

    #[tokio::test]
    async fn late_blocking_read_completion_cannot_authorise_after_recovery() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("late"), "contents").unwrap();
        let (mut arena, gate) = arena_with_blocking_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute("await read('late')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_started() => {}
            result = &mut execution => panic!("execution finished before its blocking read started: {result:?}"),
        }
        cancellation.cancel();
        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::Cancelled { .. }
        ));
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );

        gate.resume();
        gate.wait_until_finished().await;
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
    }

    #[tokio::test]
    async fn late_mutation_publication_after_cancellation_cannot_authorise_a_replacement() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let hook = WriteTestHook::pause_before_recheck();
        let tool_host = ToolHost::new(workspace.path())
            .await
            .expect("open tool host")
            .with_write_test_hook(hook.clone());
        let mut arena = Arena::new(worker_path(), tool_host)
            .await
            .expect("create arena");
        execute_without_cancellation(&mut arena, "await read('file')")
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute("await write('file', 'after')", &cancellation));

        tokio::select! {
            _ = hook.wait_until_paused() => {}
            result = &mut execution => panic!("execution finished before its mutation paused: {result:?}"),
        }
        cancellation.cancel();
        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::Cancelled { .. }
        ));
        assert!(has_authorisation(&arena, "file"));

        hook.resume();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if fs::read_to_string(workspace.path().join("file")).unwrap() == "after" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("late mutation should publish");

        let later = execute_without_cancellation(&mut arena, "await write('file', 'later')")
            .await
            .unwrap();
        assert!(python_error(&later).text.contains("RuntimeError"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "after"
        );
    }

    #[tokio::test]
    async fn read_call_trace_keeps_settled_outcomes_when_cancellation_precedes_progress() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("settled"), "line\n").unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute("await read('settled')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        gate.resume_read();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut execution => panic!("execution finished before its read settled: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(5),
            }]
        );
        assert_eq!(execution.read_paths, [PathBuf::from("settled")]);
        assert!(execution.modified_paths.is_empty());
    }

    #[tokio::test]
    async fn read_call_trace_keeps_every_call_beyond_the_display_preview_budget() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("lines"), "line\n").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "for _ in range(20):\n    await read('lines')",
        )
        .await
        .unwrap();

        assert_eq!(execution.tool_calls.len(), 20);
        assert!(execution.tool_calls.iter().all(|call| {
            *call
                == ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(5),
                }
        }));
    }

    #[tokio::test]
    async fn cancellation_retains_short_output_and_a_pending_tool_call() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution =
            Box::pin(arena.execute("print('pending')\nawait read('pending')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert!(execution.began);
        assert_eq!(execution.stdout.head, "pending\n");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            }]
        );
    }

    #[tokio::test]
    async fn cancellation_does_not_retain_the_pending_read_path() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let path = "é".repeat(2_049);
        let code = format!("await read({path:?})");
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            }]
        );
        assert!(!format!("{execution:?}").contains(&path));
    }

    #[tokio::test]
    async fn cancellation_retains_the_last_settled_read_outcome() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("settled"), "line\n").unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut execution =
            Box::pin(arena.execute("print('before')\nawait read('settled')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        gate.resume_read();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut execution => panic!("execution finished before its read settled: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(execution.stdout.head, "before\n");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(5),
            }]
        );
    }

    #[tokio::test]
    async fn cancellation_observation_keeps_existing_head_and_tail_bounds() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let code = format!(
            "print('x' * {})\nawait read('pending')",
            OBSERVATION_LIMITS.max_total_bytes
        );
        let mut execution = Box::pin(arena.execute(&code, &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::Cancelled { execution } = error else {
            panic!("expected interrupted cancellation");
        };
        assert_eq!(
            execution.stdout.head.len(),
            OBSERVATION_LIMITS.max_head_bytes
        );
        assert_eq!(
            execution.stdout.tail.as_deref().map(str::len),
            Some(OBSERVATION_LIMITS.max_tail_bytes)
        );
        assert_eq!(
            execution.stdout.total_bytes,
            OBSERVATION_LIMITS.max_total_bytes + 1
        );
        assert_eq!(execution.stdout.total_lines, 1);
        assert_eq!(execution.stdout.omitted_bytes, 1);
        assert!(execution.stdout.partial_lines);
    }

    #[tokio::test]
    async fn successful_execution_keeps_its_existing_output() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("complete"), "line\n").unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut run = Box::pin(arena.execute(
            "print('before')\nawait read('complete')\nprint('after')",
            &cancellation,
        ));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut run => panic!("execution finished before its read started: {result:?}"),
        }
        gate.resume_read();
        tokio::select! {
            _ = gate.wait_until_read_settled() => {}
            result = &mut run => panic!("execution finished before its read settled: {result:?}"),
        }
        gate.resume_driver();
        let execution = run.await.unwrap();

        assert_eq!(execution.stdout.head, "before\nafter\n");
        assert_eq!(execution.stdout.tail, None);
        assert_eq!(execution.stdout.total_bytes, 13);
    }

    #[tokio::test]
    async fn cancellation_recovers_the_trusted_checkpoint_before_returning() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        set_trusted_value(&mut arena).await;
        #[cfg(unix)]
        let previous_pid = arena_worker_pid(&arena);
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        let cancel = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceller.cancel();
        });

        let error = arena
            .execute("value = 99\nwhile True:\n    pass", &cancellation)
            .await
            .unwrap_err();
        cancel.await.unwrap();

        assert!(matches!(error, ArenaError::Cancelled { .. }));
        assert_trusted_value(&mut arena).await;
        #[cfg(unix)]
        assert_ne!(arena_worker_pid(&arena), previous_pid);
    }

    #[tokio::test]
    async fn execution_deadline_recovers_the_trusted_checkpoint_before_returning() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena_with_limits(
            &workspace,
            Duration::from_millis(100),
            Some(64 * 1024 * 1024),
        )
        .await;
        set_trusted_value(&mut arena).await;
        #[cfg(unix)]
        let previous_pid = arena_worker_pid(&arena);

        let error = execute_without_cancellation(
            &mut arena,
            "print('before')\nvalue = 99\nwhile True:\n    pass",
        )
        .await
        .unwrap_err();

        let ArenaError::TimedOut { execution } = error else {
            panic!("expected interrupted timeout");
        };
        assert_eq!(execution.stdout.head, "before\n");
        assert_trusted_value(&mut arena).await;
        #[cfg(unix)]
        assert_ne!(arena_worker_pid(&arena), previous_pid);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_loss_discards_settled_read_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena(&workspace).await;
        set_trusted_value(&mut arena).await;
        let previous_pid = arena_worker_pid(&arena);
        let cancellation = CancellationToken::new();
        let (ready_sender, ready_receiver) = oneshot::channel();
        let mut ready_sender = Some(ready_sender);
        let mut progress = move |event| {
            if matches!(event, super::ArenaProgress::StdoutDelta { ref text } if text == "ready\n")
            {
                ready_sender
                    .take()
                    .expect("ready output is emitted once")
                    .send(())
                    .expect("ready receiver remains open");
            }
        };
        let mut execution = Box::pin(arena.execute_with_progress(
            "await read('file')\nprint('ready')\nwhile True:\n    pass",
            &cancellation,
            &mut progress,
        ));

        tokio::select! {
            _ = ready_receiver => {}
            result = &mut execution => panic!("execution finished before its read effect settled: {result:?}"),
        }
        assert_eq!(
            unsafe { libc::kill(previous_pid as libc::pid_t, libc::SIGKILL) },
            0
        );
        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::Infrastructure { .. }
        ));
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        assert_trusted_value(&mut arena).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn worker_crashes_recover_the_trusted_checkpoint_before_returning() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        set_trusted_value(&mut arena).await;
        let previous_pid = arena_worker_pid(&arena);
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(
                unsafe { libc::kill(previous_pid as libc::pid_t, libc::SIGKILL) },
                0
            );
        });

        let result = execute_without_cancellation(
            &mut arena,
            "print('before')\nvalue = 99\nwhile True:\n    pass",
        )
        .await;
        killer.join().unwrap();
        let error = result.unwrap_err();

        let ArenaError::Infrastructure {
            execution: Some(execution),
            ..
        } = error
        else {
            panic!("expected interrupted infrastructure failure");
        };
        assert_eq!(execution.stdout.head, "before\n");
        assert_trusted_value(&mut arena).await;
        assert_ne!(arena_worker_pid(&arena), previous_pid);
    }

    #[tokio::test]
    async fn soft_memory_limit_recovers_the_trusted_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena =
            arena_with_limits(&workspace, Duration::from_secs(10), Some(8 * 1024 * 1024)).await;
        set_trusted_value(&mut arena).await;
        #[cfg(unix)]
        let previous_pid = arena_worker_pid(&arena);

        let error = execute_without_cancellation(
            &mut arena,
            "await read('file')\nvalue = 99\n[str(i) for i in range(131_072)]",
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ArenaError::Infrastructure { .. }));
        assert!(error.to_string().contains("MemoryError"));
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        #[cfg(unix)]
        {
            assert_ne!(arena_worker_pid(&arena), previous_pid);
            assert!(
                process_exited(previous_pid as libc::pid_t).await,
                "soft-memory worker {previous_pid} is still running"
            );
        }
        assert_trusted_value(&mut arena).await;
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value + 1")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "8"
        );
    }

    #[tokio::test]
    async fn fatal_worker_memory_error_has_an_empty_traceback() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena_with_limits(&workspace, Duration::from_secs(1), None).await;
        let checkout = arena
            .checkout
            .as_mut()
            .expect("new arena should have a checkout");

        let mut progress = |_| {};
        let mut interruption = InterruptedExecutionState::new();
        let mut authorisations = WholeFileAuthorisations::default();
        let cancellation = CancellationToken::new();
        let mut budget = ChargeableBudget::new(arena.clock.clone(), Duration::from_secs(1));
        let result = drive(
            checkout,
            &arena.tool_host,
            "x = ' ' * (1 << 60)",
            None,
            &mut authorisations,
            &mut progress,
            &mut interruption,
            &mut budget,
            &cancellation,
        )
        .await;
        let Err(DriverError::Runtime(error)) = result else {
            panic!("expected a runtime worker allocator failure");
        };

        assert_eq!(error.exc_type(), monty_types::ExcType::MemoryError);
        assert_eq!(
            error.message(),
            Some("the worker exceeded its memory limit and was terminated")
        );
        assert!(error.traceback().is_empty());
        assert!(is_unsafe_memory_error(&error, None));
    }

    #[tokio::test]
    async fn fatal_allocator_failure_recovers_the_trusted_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let mut arena = arena_with_limits(&workspace, Duration::from_secs(1), None).await;
        set_trusted_value(&mut arena).await;
        #[cfg(unix)]
        let previous_pid = arena_worker_pid(&arena);

        let error = execute_without_cancellation(
            &mut arena,
            "await read('file')\nvalue = 99\nx = ' ' * (1 << 60)",
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ArenaError::Infrastructure { .. }));
        assert!(error.to_string().contains("MemoryError"));
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        assert_trusted_value(&mut arena).await;
        #[cfg(unix)]
        assert_ne!(arena_worker_pid(&arena), previous_pid);
    }

    #[tokio::test]
    async fn recovery_failure_retains_original_interruption_evidence() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut arena, gate) = arena_with_read_gate(&workspace).await;
        set_trusted_value(&mut arena).await;
        arena.fail_next_recovery_for_test();
        let cancellation = CancellationToken::new();
        let mut execution =
            Box::pin(arena.execute("print('before')\nawait read('pending')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_read_started() => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        cancellation.cancel();
        let error = execution.await.unwrap_err();

        let ArenaError::RecoveryFailed { original, recovery } = error else {
            panic!("expected recovery failure");
        };
        let ArenaError::Cancelled { execution } = *original else {
            panic!("expected original interrupted cancellation");
        };
        assert_eq!(execution.stdout.head, "before\n");
        assert_eq!(
            execution.tool_calls,
            [ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            }]
        );
        assert!(matches!(
            *recovery,
            ArenaError::Infrastructure {
                message,
                execution: None,
            } if message == "test recovery failure"
        ));
        assert!(arena.checkout.is_none());
        assert_trusted_value(&mut arena).await;
    }

    #[tokio::test]
    async fn failed_recovery_retains_prior_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("trusted"), "contents").unwrap();
        let (mut arena, gate) = arena_with_batch_gate(&workspace).await;
        let cancellation = CancellationToken::new();
        let mut setup = Box::pin(arena.execute("await read('trusted')", &cancellation));

        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut setup => panic!("setup finished before its read started: {result:?}"),
        }
        let trusted_call = *gate
            .started_call_ids()
            .first()
            .expect("trusted read must start");
        gate.release(trusted_call);
        setup.await.unwrap();
        let trusted_authorisations = arena.checkpoint.authorisations.clone();

        arena.fail_next_recovery_for_test();
        let cancellation = CancellationToken::new();
        let mut execution = Box::pin(arena.execute("await read('pending')", &cancellation));
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut execution => panic!("execution finished before its read started: {result:?}"),
        }
        cancellation.cancel();
        assert!(matches!(
            execution.await.unwrap_err(),
            ArenaError::RecoveryFailed { .. }
        ));
        assert_eq!(arena.checkpoint.authorisations, trusted_authorisations);
    }

    #[tokio::test]
    async fn failed_recovery_preserves_original_error_and_a_missing_checkout_self_recovers() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        set_trusted_value(&mut arena).await;
        let valid_checkpoint = arena.checkpoint();
        arena.checkpoint.state = vec![0];
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = arena
            .execute("value = 99", &cancellation)
            .await
            .unwrap_err();

        let ArenaError::RecoveryFailed { original, recovery } = error else {
            panic!("expected recovery failure, got {error:?}");
        };
        assert!(matches!(*original, ArenaError::Cancelled { .. }));
        assert!(matches!(*recovery, ArenaError::Infrastructure { .. }));
        assert!(arena.checkout.is_none());

        arena.checkpoint = valid_checkpoint;
        assert_trusted_value(&mut arena).await;
    }

    #[tokio::test]
    async fn restores_initial_successful_and_python_error_checkpoints() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("successful"), "success").unwrap();
        fs::write(workspace.path().join("python-error"), "error").unwrap();
        let mut arena = arena(&workspace).await;
        let initial = arena.checkpoint();

        execute_without_cancellation(&mut arena, "value = 1\nawait read('successful')")
            .await
            .unwrap();
        let successful_mutation = arena.checkpoint();
        let successful_authorisations = successful_mutation.authorisations.clone();
        assert!(has_authorisation(&arena, "successful"));
        execute_without_cancellation(&mut arena, "value = 99")
            .await
            .unwrap();
        arena.restore(initial).await.unwrap();
        assert_eq!(
            arena.checkpoint.authorisations,
            WholeFileAuthorisations::default()
        );
        assert!(
            python_error(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .text
            .contains("NameError")
        );

        arena.restore(successful_mutation).await.unwrap();
        assert_eq!(arena.checkpoint.authorisations, successful_authorisations);
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "1"
        );

        let error = execute_without_cancellation(
            &mut arena,
            "value = 2\nawait read('python-error')\nraise ValueError('expected')",
        )
        .await
        .unwrap();
        assert!(python_error(&error).text.contains("ValueError"));
        let python_error_mutation = arena.checkpoint();
        let python_error_authorisations = python_error_mutation.authorisations.clone();
        assert!(has_authorisation(&arena, "python-error"));
        execute_without_cancellation(&mut arena, "value = 99")
            .await
            .unwrap();
        arena.restore(python_error_mutation).await.unwrap();
        assert_eq!(arena.checkpoint.authorisations, python_error_authorisations);
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "value")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "2"
        );
    }

    #[tokio::test]
    async fn failed_explicit_restore_retains_canonical_authorisations() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("trusted"), "contents").unwrap();
        let mut arena = arena(&workspace).await;
        execute_without_cancellation(&mut arena, "await read('trusted')")
            .await
            .unwrap();
        let authorisations = arena.checkpoint.authorisations.clone();
        let mut invalid = arena.checkpoint();
        invalid.state = vec![0];
        invalid.authorisations = WholeFileAuthorisations::default();

        assert!(matches!(
            arena.restore(invalid).await.unwrap_err(),
            ArenaError::Infrastructure { .. }
        ));
        assert_eq!(arena.checkpoint.authorisations, authorisations);
        assert!(has_authorisation(&arena, "trusted"));
    }

    #[tokio::test]
    async fn rejects_an_incompatible_checkpoint_before_restore() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let mut checkpoint = arena.checkpoint();
        checkpoint.identity = RuntimeIdentity("monty-runtime 9.9.9".to_owned());

        let error = arena.restore(checkpoint).await.unwrap_err();

        assert!(matches!(error, ArenaError::IncompatibleCheckpoint { .. }));
        assert_eq!(
            complete(
                &execute_without_cancellation(&mut arena, "1 + 1")
                    .await
                    .unwrap()
            )
            .unwrap()
            .head,
            "2"
        );
    }
}
