use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
#[cfg(test)]
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use monty_pool::{
    Checkout, Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync,
};
use monty_types::{ExcType, MontyException, MontyObject, PrintStream, ResourceLimits};
use thiserror::Error;
use tokio::process::Command;
#[cfg(test)]
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

use crate::{
    config::constants::{ARENA_EXECUTION_TIMEOUT, ARENA_MAX_MEMORY_BYTES},
    tools::{
        ToolCall as HostToolCall, ToolError, ToolErrorKind, ToolHost, ToolOutputChunk, ToolResult,
        ToolValue, WholeFileAuthorisations, bootstrap_source,
    },
};

pub(crate) use crate::tools::ToolName;

const RUNTIME_IDENTITY: &str = "monty-runtime 0.0.21";
const WORKER_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const ERROR_BYTES: usize = 32 * 1024;
const INTERRUPTED_EVIDENCE_TEXT_LIMIT: usize = 4 * 1024;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    identity: RuntimeIdentity,
    state: Vec<u8>,
    authorisations: WholeFileAuthorisations,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub(crate) stdout: Observation,
    pub(crate) stderr: Observation,
    pub(crate) outcome: ExecutionOutcome,
    pub(crate) tool_calls: Vec<ToolCall>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCallSummary {
    Pending,
    Failed,
    Exit(i64),
    Signal,
    Count(usize),
    Created,
    Replaced,
    Applied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        *call
    }

    fn call(&self, token: ToolCallToken) -> ToolCall {
        *self
            .calls
            .get(token.0)
            .expect("tool call token must identify an admitted row")
    }

    #[cfg(test)]
    fn calls(&self) -> &[ToolCall] {
        &self.calls
    }

    fn into_calls(self) -> Vec<ToolCall> {
        self.calls
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedExecution {
    pub(crate) began: bool,
    pub(crate) stdout: Observation,
    pub(crate) stderr: Observation,
    pub(crate) tool_calls: Vec<ToolCall>,
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
        InterruptedExecution {
            began,
            stdout: streams.stdout.finish(),
            stderr: streams.stderr.finish(),
            tool_calls: tools.into_calls(),
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

pub struct Arena {
    pool: Pool,
    checkout: Option<Checkout>,
    tool_host: ToolHost,
    checkpoint: Checkpoint,
    runtime: RuntimeIdentity,
    repl_config: ReplConfig,
    execution_timeout: Duration,
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
        )
        .await
    }

    async fn open(
        worker_path: std::path::PathBuf,
        tool_host: ToolHost,
        execution_timeout: Duration,
        max_memory: Option<usize>,
    ) -> Result<Self, ArenaError> {
        let runtime = validate_worker(&worker_path).await?;
        let mut config = PoolConfig::subprocess(worker_path);
        config.min_processes = 1;
        config.max_processes = 1;
        let pool = Pool::new(config).await.map_err(pool_failure)?;
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
            pool,
            checkout: Some(checkout),
            tool_host,
            checkpoint,
            runtime,
            repl_config,
            execution_timeout,
            #[cfg(test)]
            fail_next_recovery: false,
            #[cfg(test)]
            recovery_test_gate: None,
        })
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
        self.restore_current_checkpoint_if_needed().await?;
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
        let attempt = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AttemptFailure::Cancelled),
            _ = sleep(self.execution_timeout) => Err(AttemptFailure::TimedOut),
            result = execute_checkout(
                &mut checkout,
                &self.tool_host,
                code,
                max_memory,
                &mut authorisations,
                progress,
                &mut interruption,
            ) => result.map_err(AttemptFailure::Driver),
        };

        match attempt {
            Ok((outcome, state)) => {
                self.checkout = Some(checkout);
                self.checkpoint = Checkpoint {
                    identity: self.runtime.clone(),
                    state,
                    authorisations,
                };
                Ok(Execution {
                    stdout: interruption.streams.stdout.finish(),
                    stderr: interruption.streams.stderr.finish(),
                    outcome,
                    tool_calls: interruption.tools.into_calls(),
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
        let mut on_print = on_print_sync(|_, _| {});
        let (event, _) = checkout
            .restore(checkpoint.state.clone(), vec![], &mut on_print)
            .await
            .map_err(pool_failure)?;
        if event.is_some() {
            return Err(ArenaError::SuspendedCheckpoint);
        }
        Ok(checkout)
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

async fn execute_checkout(
    checkout: &mut Checkout,
    tool_host: &ToolHost,
    code: &str,
    max_memory: Option<usize>,
    authorisations: &mut WholeFileAuthorisations,
    progress: &mut (impl FnMut(ArenaProgress) + Send),
    interruption: &mut InterruptedExecutionState,
) -> Result<(ExecutionOutcome, Vec<u8>), ArenaError> {
    let result = drive(
        checkout,
        tool_host,
        code,
        authorisations,
        progress,
        interruption,
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
        Err(DriverError::Fatal(error)) => return Err(error),
    };
    let state = checkout.dump().await.map_err(|error| {
        let context = error_text.as_deref().unwrap_or("completed execution");
        ArenaError::Infrastructure {
            message: format!("post-execution checkpoint failed after {context}: {error}"),
            execution: None,
        }
    })?;
    if unsafe_memory {
        return Err(ArenaError::Infrastructure {
            message: format!(
                "unsafe resource memory failure: {}",
                error_text.expect("memory errors have diagnostics")
            ),
            execution: None,
        });
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
}

struct PendingToolCall {
    call_id: u32,
    tool: ToolName,
    arguments: ToolValue,
    token: ToolCallToken,
    run_preview: Option<RunPreviewState>,
}

async fn drive(
    checkout: &mut Checkout,
    tool_host: &ToolHost,
    code: &str,
    authorisations: &mut WholeFileAuthorisations,
    progress: &mut (impl FnMut(ArenaProgress) + Send),
    interruption: &mut InterruptedExecutionState,
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
    'drive: {
        let mut event = match with_print!(on_print => checkout
            .feed(code, vec![], vec![], false, &mut on_print)
            .await)
        {
            Ok(event) => event,
            Err(PoolError::Runtime(error)) => break 'drive Err(DriverError::Runtime(error)),
            Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
        };

        let mut pending_calls = Vec::new();
        loop {
            event = match event {
                TurnEvent::Complete(value) => break 'drive Ok(value),
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    kwargs,
                    call_id,
                    method_call,
                } => {
                    let Some(tool) = ToolName::from_private_name(&function_name) else {
                        event = match with_print!(on_print => checkout
                            .resume(ResumeValue::NotFound, &mut on_print)
                            .await)
                        {
                            Ok(event) => event,
                            Err(PoolError::Runtime(error)) => {
                                break 'drive Err(DriverError::Runtime(error));
                            }
                            Err(error) => {
                                break 'drive Err(DriverError::Fatal(pool_failure(error)));
                            }
                        };
                        continue;
                    };
                    if method_call || !kwargs.is_empty() {
                        match with_print!(on_print => checkout
                            .resume(
                                ResumeValue::Error(tool_error(ToolError::type_error(
                                    "host functions accept positional arguments only",
                                ))),
                                &mut on_print,
                            )
                            .await)
                        {
                            Ok(event) => event,
                            Err(PoolError::Runtime(error)) => {
                                break 'drive Err(DriverError::Runtime(error));
                            }
                            Err(error) => {
                                break 'drive Err(DriverError::Fatal(pool_failure(error)));
                            }
                        }
                    } else {
                        let arguments = match args
                            .into_iter()
                            .map(tool_value_from_monty)
                            .collect::<Result<Vec<_>, _>>()
                        {
                            Ok(arguments) => ToolValue::List(arguments),
                            Err(error) => {
                                event = match with_print!(on_print => checkout
                                    .resume(ResumeValue::Error(tool_error(error)), &mut on_print)
                                    .await)
                                {
                                    Ok(event) => event,
                                    Err(PoolError::Runtime(error)) => {
                                        break 'drive Err(DriverError::Runtime(error));
                                    }
                                    Err(error) => {
                                        break 'drive Err(DriverError::Fatal(pool_failure(error)));
                                    }
                                };
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
                            arguments,
                            token,
                            run_preview,
                        });
                        match with_print!(on_print => checkout
                            .resume(ResumeValue::Future, &mut on_print)
                            .await)
                        {
                            Ok(event) => event,
                            Err(PoolError::Runtime(error)) => {
                                break 'drive Err(DriverError::Runtime(error));
                            }
                            Err(error) => {
                                break 'drive Err(DriverError::Fatal(pool_failure(error)));
                            }
                        }
                    }
                }
                TurnEvent::NameLookup { name } => {
                    let value =
                        ToolName::from_private_name(&name).map(|tool| MontyObject::Function {
                            name: tool.private_name().to_owned(),
                            docstring: None,
                        });
                    match with_print!(on_print => checkout
                        .resume_name_lookup(value, &mut on_print)
                        .await)
                    {
                        Ok(event) => event,
                        Err(PoolError::Runtime(error)) => {
                            break 'drive Err(DriverError::Runtime(error));
                        }
                        Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
                    }
                }
                TurnEvent::OsCall { .. } => match with_print!(on_print => checkout
                    .resume(ResumeValue::NotHandled, &mut on_print)
                    .await)
                {
                    Ok(event) => event,
                    Err(PoolError::Runtime(error)) => {
                        break 'drive Err(DriverError::Runtime(error));
                    }
                    Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
                },
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
                    let mut run_previews = BTreeMap::new();
                    let calls = pending_call_ids
                        .iter()
                        .map(|call_id| {
                            let call = pending
                                .remove(call_id)
                                .expect("validated call ID must be admitted");
                            assert!(
                                tokens
                                    .insert(call.call_id, (call.tool, call.token))
                                    .is_none(),
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
                            HostToolCall {
                                call_id: call.call_id,
                                tool: call.tool,
                                arguments: call.arguments,
                            }
                        })
                        .collect::<Vec<_>>();
                    let mutation_paths = calls
                        .iter()
                        .filter_map(HostToolCall::mutation_path)
                        .collect::<BTreeSet<_>>();
                    let mut results = BTreeMap::new();
                    let (output_sender, mut output_receiver) =
                        mpsc::channel(RUN_OUTPUT_CHANNEL_CAPACITY);
                    let mut calls = Box::pin(tool_host.dispatch_batch_with_output(
                        calls,
                        authorisations.clone(),
                        output_sender,
                    ));
                    while !tokens.is_empty() {
                        let result = tokio::select! {
                            result = calls.next() => result.expect("every admitted host call must settle"),
                            output = output_receiver.recv(), if !run_previews.is_empty() => {
                                if let Some(output) = output {
                                    update_run_preview(output, &mut run_previews, progress);
                                }
                                continue;
                            }
                        };
                        while let Ok(output) = output_receiver.try_recv() {
                            update_run_preview(output, &mut run_previews, progress);
                        }
                        let call_id = result.call_id;
                        let (tool, token) = tokens
                            .remove(&call_id)
                            .expect("a host result must retain an admitted call ID");
                        let summary = tool_summary(tool, &result);
                        assert!(
                            results.insert(call_id, result).is_none(),
                            "a host tool call must settle once"
                        );
                        let call = tools.settle(token, summary);
                        run_previews.remove(&call_id);
                        #[cfg(test)]
                        tool_host.wait_after_read_settled().await;
                        progress(ArenaProgress::ToolUpdated { token, call });
                        #[cfg(test)]
                        tool_host.note_batch_driver_settled(call_id);
                    }
                    assert!(
                        tokens.is_empty(),
                        "every admitted host call must settle exactly once"
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
                    match with_print!(on_print => checkout.resume_futures(results, &mut on_print).await)
                    {
                        Ok(event) => event,
                        Err(PoolError::Runtime(error)) => {
                            break 'drive Err(DriverError::Runtime(error));
                        }
                        Err(error) => {
                            break 'drive Err(DriverError::Fatal(pool_failure(error)));
                        }
                    }
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
    }
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
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use monty_types::PrintStream;
    use tempfile::TempDir;
    use tokio::sync::oneshot;
    use tokio_util::sync::CancellationToken;

    use super::{
        Arena, ArenaError, Checkout, DriverError, Execution, ExecutionOutcome,
        INTERRUPTED_EVIDENCE_TEXT_LIMIT, InterruptedExecutionState, OBSERVATION_LIMITS,
        Observation, ObservationAccumulator, ObservationLimits, PythonError, RUNTIME_IDENTITY,
        RuntimeIdentity, StreamAccumulators, ToolCall, ToolCallSummary, ToolName, ToolTraceState,
        drive, is_unsafe_memory_error, is_unsafe_soft_memory_error, parse_soft_memory_limit,
        tool_summary, validate_worker, validate_worker_with_timeout,
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
            .expect("MONTY_BIN must point to the monty-runtime 0.0.21 worker")
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
                super::ArenaProgress::ToolUpdated { token, call } => Some((*token, *call)),
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
    async fn production_arena_sets_a_whole_execution_deadline_and_memory_limit() {
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
            *call
                == ToolCall {
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
        assert!(!format!("{execution:?}").contains("created-on-cancel"));
        assert!(!format!("{execution:?}").contains("still-pending"));
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
        let result = drive(
            checkout,
            &arena.tool_host,
            "x = ' ' * (1 << 60)",
            &mut authorisations,
            &mut progress,
            &mut interruption,
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
