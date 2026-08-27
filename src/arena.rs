use std::collections::VecDeque;
use std::path::Path;
use std::time::Duration;

use monty_pool::{
    Checkout, Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync,
};
use monty_types::{ExcType, MontyException, MontyObject, PrintStream, ResourceLimits};
use thiserror::Error;
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

use crate::tools::{ReadError, ReadTool};

const RUNTIME_IDENTITY: &str = "monty-runtime 0.0.21";
const WORKER_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MEMORY: usize = 64 * 1024 * 1024;
const ERROR_BYTES: usize = 32 * 1024;
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub(crate) head: String,
    pub(crate) tail: Option<String>,
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionOutcome {
    Complete { result: Option<Observation> },
    PythonError(PythonError),
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
    Cancelled,
    #[error("arena execution timed out")]
    TimedOut,
    #[error("arena infrastructure failed: {message}")]
    Infrastructure { message: String },
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
    UnsupportedSuspension { message: String },
}

pub struct Arena {
    pool: Pool,
    checkout: Option<Checkout>,
    read_tool: ReadTool,
    checkpoint: Checkpoint,
    runtime: RuntimeIdentity,
    repl_config: ReplConfig,
    max_memory: Option<usize>,
    execution_timeout: Duration,
}

impl Arena {
    pub async fn new(
        worker_path: impl AsRef<Path>,
        read_tool: ReadTool,
    ) -> Result<Self, ArenaError> {
        Self::open(
            worker_path.as_ref().to_path_buf(),
            read_tool,
            EXECUTION_TIMEOUT,
            Some(MAX_MEMORY),
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits(
        worker_path: impl AsRef<Path>,
        read_tool: ReadTool,
        execution_timeout: Duration,
        max_memory: Option<usize>,
    ) -> Result<Self, ArenaError> {
        Self::open(
            worker_path.as_ref().to_path_buf(),
            read_tool,
            execution_timeout,
            max_memory,
        )
        .await
    }

    async fn open(
        worker_path: std::path::PathBuf,
        read_tool: ReadTool,
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
        let state = checkout.dump().await.map_err(pool_failure)?;
        let checkpoint = Checkpoint {
            identity: runtime.clone(),
            state,
        };

        Ok(Self {
            pool,
            checkout: Some(checkout),
            read_tool,
            checkpoint,
            runtime,
            repl_config,
            max_memory,
            execution_timeout,
        })
    }

    pub async fn execute(
        &mut self,
        code: &str,
        cancellation: &CancellationToken,
    ) -> Result<Execution, ArenaError> {
        self.restore_current_checkpoint_if_needed().await?;
        let mut checkout = self
            .checkout
            .take()
            .expect("restoring a missing checkout should install one");
        let checkpoint = self.checkpoint.clone();
        let attempt = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(ArenaError::Cancelled),
            _ = sleep(self.execution_timeout) => Err(ArenaError::TimedOut),
            result = execute_checkout(&mut checkout, &self.read_tool, code, self.max_memory) => result,
        };

        match attempt {
            Ok((execution, state)) => {
                self.checkout = Some(checkout);
                self.checkpoint = Checkpoint {
                    identity: self.runtime.clone(),
                    state,
                };
                Ok(execution)
            }
            Err(original) => {
                drop(checkout);
                self.recover(checkpoint, original).await
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
            let checkpoint = self.checkpoint.clone();
            self.checkout = Some(self.checkout_from_checkpoint(&checkpoint).await?);
        }
        Ok(())
    }

    async fn recover(
        &mut self,
        checkpoint: Checkpoint,
        original: ArenaError,
    ) -> Result<Execution, ArenaError> {
        match self.checkout_from_checkpoint(&checkpoint).await {
            Ok(checkout) => {
                self.checkout = Some(checkout);
                Err(original)
            }
            Err(recovery) => Err(ArenaError::RecoveryFailed {
                original: Box::new(original),
                recovery: Box::new(recovery),
            }),
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
    }
}

async fn execute_checkout(
    checkout: &mut Checkout,
    read_tool: &ReadTool,
    code: &str,
    max_memory: Option<usize>,
) -> Result<(Execution, Vec<u8>), ArenaError> {
    let (result, streams) = drive(checkout, read_tool, code).await;
    let stdout = streams.stdout.finish();
    let stderr = streams.stderr.finish();
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
        }
    })?;
    if unsafe_memory {
        return Err(ArenaError::Infrastructure {
            message: format!(
                "unsafe resource memory failure: {}",
                error_text.expect("memory errors have diagnostics")
            ),
        });
    }
    Ok((
        Execution {
            stdout,
            stderr,
            outcome,
        },
        state,
    ))
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

async fn drive(
    checkout: &mut Checkout,
    read_tool: &ReadTool,
    code: &str,
) -> (Result<MontyObject, DriverError>, StreamAccumulators) {
    let mut streams = StreamAccumulators::new(OBSERVATION_LIMITS);
    let result = 'drive: {
        let mut on_print = on_print_sync(|stream, text| streams.push(stream, text));
        let mut event = match checkout
            .feed(code, vec![], vec![], false, &mut on_print)
            .await
        {
            Ok(event) => event,
            Err(PoolError::Runtime(error)) => break 'drive Err(DriverError::Runtime(error)),
            Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
        };

        loop {
            event = match event {
                TurnEvent::Complete(value) => break 'drive Ok(value),
                TurnEvent::FunctionCall {
                    function_name,
                    args,
                    kwargs,
                    call_id,
                    method_call,
                } if function_name == "read" => {
                    if !kwargs.is_empty()
                        || method_call
                        || !matches!(args.as_slice(), [MontyObject::String(_)])
                    {
                        match checkout
                            .resume(
                                ResumeValue::Error(MontyException::new(
                                    ExcType::TypeError,
                                    Some(
                                        "read() expects one positional string argument".to_owned(),
                                    ),
                                )),
                                &mut on_print,
                            )
                            .await
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
                        let [MontyObject::String(path)] = args.as_slice() else {
                            unreachable!("read argument was validated above");
                        };
                        let pending =
                            match checkout.resume(ResumeValue::Future, &mut on_print).await {
                                Ok(TurnEvent::ResolveFutures { pending_call_ids })
                                    if pending_call_ids.as_slice() == [call_id] =>
                                {
                                    pending_call_ids
                                }
                                Ok(event) => {
                                    break 'drive Err(DriverError::Fatal(
                                        ArenaError::UnsupportedSuspension {
                                            message: format!(
                                                "expected one pending read future, got {event:?}"
                                            ),
                                        },
                                    ));
                                }
                                Err(PoolError::Runtime(error)) => {
                                    break 'drive Err(DriverError::Runtime(error));
                                }
                                Err(error) => {
                                    break 'drive Err(DriverError::Fatal(pool_failure(error)));
                                }
                            };
                        let value = match read_tool.read(path).await {
                            Ok(lines) => ResumeValue::Return(MontyObject::List(
                                lines.into_iter().map(MontyObject::String).collect(),
                            )),
                            Err(error) => ResumeValue::Error(read_error(error)),
                        };
                        match checkout
                            .resume_futures(vec![(pending[0], value)], &mut on_print)
                            .await
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
                TurnEvent::FunctionCall { .. } => {
                    match checkout.resume(ResumeValue::NotFound, &mut on_print).await {
                        Ok(event) => event,
                        Err(PoolError::Runtime(error)) => {
                            break 'drive Err(DriverError::Runtime(error));
                        }
                        Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
                    }
                }
                TurnEvent::NameLookup { name } => {
                    let value = (name == "read").then(|| MontyObject::Function {
                        name: "read".to_owned(),
                        docstring: None,
                    });
                    match checkout.resume_name_lookup(value, &mut on_print).await {
                        Ok(event) => event,
                        Err(PoolError::Runtime(error)) => {
                            break 'drive Err(DriverError::Runtime(error));
                        }
                        Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
                    }
                }
                TurnEvent::OsCall { .. } => match checkout
                    .resume(ResumeValue::NotHandled, &mut on_print)
                    .await
                {
                    Ok(event) => event,
                    Err(PoolError::Runtime(error)) => {
                        break 'drive Err(DriverError::Runtime(error));
                    }
                    Err(error) => break 'drive Err(DriverError::Fatal(pool_failure(error))),
                },
                TurnEvent::ResolveFutures { pending_call_ids } => {
                    break 'drive Err(DriverError::Fatal(ArenaError::UnsupportedSuspension {
                        message: format!("unexpected pending futures: {pending_call_ids:?}"),
                    }));
                }
            };
        }
    };
    (result, streams)
}

fn read_error(error: ReadError) -> MontyException {
    let exc_type = match error {
        ReadError::Io(_) => ExcType::OSError,
        ReadError::Blocking(_) => ExcType::RuntimeError,
        ReadError::AbsolutePath
        | ReadError::ParentPath
        | ReadError::NonRegularFile
        | ReadError::ByteLimitExceeded { .. }
        | ReadError::LineLimitExceeded { .. }
        | ReadError::InvalidUtf8 => ExcType::ValueError,
    };
    MontyException::new(exc_type, Some(error.to_string()))
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
            stdout: ObservationAccumulator::new(limits),
            stderr: ObservationAccumulator::new(limits),
        }
    }

    fn push(&mut self, stream: PrintStream, text: &str) {
        match stream {
            PrintStream::Stdout => self.stdout.push(text),
            PrintStream::Stderr => self.stderr.push(text),
        }
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

#[derive(Default)]
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
    use std::path::PathBuf;
    use std::time::Duration;

    use monty_types::PrintStream;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{
        Arena, ArenaError, Checkout, DriverError, Execution, ExecutionOutcome, Observation,
        ObservationAccumulator, ObservationLimits, PythonError, RUNTIME_IDENTITY, RuntimeIdentity,
        StreamAccumulators, drive, is_unsafe_memory_error, is_unsafe_soft_memory_error,
        parse_soft_memory_limit, validate_worker, validate_worker_with_timeout,
    };
    use crate::tools::ReadTool;

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
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
        )
        .await
        .expect("create arena")
    }

    async fn arena_with_limits(
        workspace: &TempDir,
        execution_timeout: Duration,
        max_memory: Option<usize>,
    ) -> Arena {
        Arena::with_limits(
            worker_path(),
            ReadTool::new(workspace.path())
                .await
                .expect("open read tool"),
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

    fn observe(text: &str, limits: ObservationLimits) -> Observation {
        let mut accumulator = ObservationAccumulator::new(limits);
        accumulator.push(text);
        accumulator.finish()
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

        assert_eq!(arena.execution_timeout, Duration::from_secs(10));
        assert_eq!(limits.max_duration, None);
        assert_eq!(limits.max_memory, Some(64 * 1024 * 1024));
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
    async fn read_returns_exact_line_endings() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("lines.txt"), "first\r\nsecond\nlast").unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(
            &mut arena,
            "lines = await read('lines.txt')\n''.join(lines)",
        )
        .await
        .unwrap();

        assert_eq!(
            complete(&execution).unwrap().head,
            "'first\\r\\nsecond\\nlast'"
        );
    }

    #[tokio::test]
    async fn malformed_read_is_a_model_visible_python_error() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        for code in [
            "await read(1)",
            "await read()",
            "await read(path='lines.txt')",
        ] {
            let execution = execute_without_cancellation(&mut arena, code)
                .await
                .unwrap();

            assert!(python_error(&execution).text.contains("TypeError"));
        }
    }

    #[tokio::test]
    async fn read_failures_are_model_visible_python_errors() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;

        let execution = execute_without_cancellation(&mut arena, "await read('/outside')")
            .await
            .unwrap();

        assert!(python_error(&execution).text.contains("ValueError"));
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
            "print('before')\nlines = await read('lines.txt')\nprint('after')\nlen(lines)",
        )
        .await
        .unwrap();

        assert_eq!(execution.stdout.head, "before\nafter\n");
        assert_eq!(complete(&execution).unwrap().head, "2");
    }

    #[test]
    fn routes_stderr_to_its_own_accumulator() {
        let mut streams = StreamAccumulators::new(TEST_LIMITS);

        streams.push(PrintStream::Stderr, "warning\n");
        streams.push(PrintStream::Stdout, "output\n");

        assert_eq!(streams.stdout.finish().head, "output\n");
        assert_eq!(streams.stderr.finish().head, "warning\n");
    }

    #[test]
    fn truncates_by_line_count_without_losing_boundary_metadata() {
        let observation = observe("a\nb\nc\nd\n", TEST_LIMITS);

        assert_eq!(observation.head, "a\nb\n");
        assert_eq!(observation.tail.as_deref(), Some("d\n"));
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

    #[tokio::test]
    async fn ordinary_python_errors_commit_preceding_mutation() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        #[cfg(unix)]
        let worker_pid = arena_worker_pid(&arena);

        let error =
            execute_without_cancellation(&mut arena, "value = 41\nraise ValueError('expected')")
                .await
                .unwrap();
        assert!(python_error(&error).text.contains("ValueError"));

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
        let mut arena = arena(&workspace).await;
        #[cfg(unix)]
        let worker_pid = arena_worker_pid(&arena);

        let execution = execute_without_cancellation(
            &mut arena,
            "value = 99\nraise MemoryError('the worker exceeded its memory limit and was terminated')",
        )
        .await
        .unwrap();

        assert!(python_error(&execution).text.contains("Traceback"));
        assert!(python_error(&execution).text.contains("MemoryError"));
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

        assert!(matches!(error, ArenaError::Cancelled));
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

        let error = execute_without_cancellation(&mut arena, "value = 99\nwhile True:\n    pass")
            .await
            .unwrap_err();

        assert!(matches!(error, ArenaError::TimedOut));
        assert_trusted_value(&mut arena).await;
        #[cfg(unix)]
        assert_ne!(arena_worker_pid(&arena), previous_pid);
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

        let result =
            execute_without_cancellation(&mut arena, "value = 99\nwhile True:\n    pass").await;
        killer.join().unwrap();
        let error = result.unwrap_err();

        assert!(matches!(error, ArenaError::Infrastructure { .. }));
        assert_trusted_value(&mut arena).await;
        assert_ne!(arena_worker_pid(&arena), previous_pid);
    }

    #[tokio::test]
    async fn soft_memory_limit_recovers_the_trusted_checkpoint() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena =
            arena_with_limits(&workspace, Duration::from_secs(10), Some(8 * 1024 * 1024)).await;
        set_trusted_value(&mut arena).await;
        #[cfg(unix)]
        let previous_pid = arena_worker_pid(&arena);

        let error = execute_without_cancellation(
            &mut arena,
            "value = 99\n[str(i) for i in range(131_072)]",
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ArenaError::Infrastructure { .. }));
        assert!(error.to_string().contains("MemoryError"));
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

        let (result, _) = drive(checkout, &arena.read_tool, "x = ' ' * (1 << 60)").await;
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
        let mut arena = arena_with_limits(&workspace, Duration::from_secs(1), None).await;
        set_trusted_value(&mut arena).await;
        #[cfg(unix)]
        let previous_pid = arena_worker_pid(&arena);

        let error = execute_without_cancellation(&mut arena, "value = 99\nx = ' ' * (1 << 60)")
            .await
            .unwrap_err();

        assert!(matches!(error, ArenaError::Infrastructure { .. }));
        assert!(error.to_string().contains("MemoryError"));
        assert_trusted_value(&mut arena).await;
        #[cfg(unix)]
        assert_ne!(arena_worker_pid(&arena), previous_pid);
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
        assert!(matches!(*original, ArenaError::Cancelled));
        assert!(matches!(*recovery, ArenaError::Infrastructure { .. }));
        assert!(arena.checkout.is_none());

        arena.checkpoint = valid_checkpoint;
        assert_trusted_value(&mut arena).await;
    }

    #[tokio::test]
    async fn restores_initial_successful_and_python_error_checkpoints() {
        let workspace = tempfile::tempdir().unwrap();
        let mut arena = arena(&workspace).await;
        let initial = arena.checkpoint();

        execute_without_cancellation(&mut arena, "value = 1")
            .await
            .unwrap();
        let successful_mutation = arena.checkpoint();
        execute_without_cancellation(&mut arena, "value = 99")
            .await
            .unwrap();
        arena.restore(initial).await.unwrap();
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

        let error =
            execute_without_cancellation(&mut arena, "value = 2\nraise ValueError('expected')")
                .await
                .unwrap();
        assert!(python_error(&error).text.contains("ValueError"));
        let python_error_mutation = arena.checkpoint();
        execute_without_cancellation(&mut arena, "value = 99")
            .await
            .unwrap();
        arena.restore(python_error_mutation).await.unwrap();
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
