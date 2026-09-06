use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::mpsc::SyncSender;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use thiserror::Error;

use crate::arena::ChildArenaConstructionPhase;
use crate::config::ModelSelection;
use crate::subagents::TaskId;

const SCHEMA_VERSION: u32 = 1;
static NEXT_PROCESS_INSTANCE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct TraceOwner {
    trace: Trace,
    worker: Option<JoinHandle<Result<(), TraceError>>>,
}

#[derive(Clone)]
pub(crate) struct Trace {
    sender: Option<Sender<WriterMessage>>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlCommand {
    Cancel,
    CancelBackground,
    Shutdown,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Settlement {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CoordinatorOperation {
    RootCaller,
    Start,
    Subscribe,
    Get,
    WaitInitial,
    Cancel,
    JoinActive,
    Service,
    NextDeadline,
    Shutdown,
    Record,
    RunChild,
    FinishChild,
    FinishChildSettlement,
    RequestStopSubtree,
    RequestStopDescendants,
    RequestTimeout,
    RequestStopAll,
    WaitForTerminalSubscribe,
    WaitForTerminalCheck,
    WaitForOutcome,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildStopCause {
    Cancelled,
    TimedOut,
    Shutdown,
    AncestorStopped,
    RunnerFailed,
}

#[derive(Debug, Error)]
pub(crate) enum TraceError {
    #[error("could not open diagnostic trace file")]
    Open(#[source] io::Error),
    #[error("could not start diagnostic telemetry writer")]
    Start(#[source] io::Error),
    #[error("diagnostic telemetry writer could not encode an event")]
    Encode(#[source] serde_json::Error),
    #[error("diagnostic telemetry writer could not write an event")]
    Write(#[source] io::Error),
    #[error("diagnostic telemetry writer stopped unexpectedly")]
    WriterStopped,
    #[error("diagnostic telemetry writer panicked")]
    WriterPanicked,
}

impl TraceOwner {
    pub(crate) fn from_path(path: Option<OsString>) -> Result<Self, TraceError> {
        match path {
            Some(path) => Self::open(PathBuf::from(path)),
            None => Ok(Self::disabled()),
        }
    }

    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, TraceError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(TraceError::Open)?;
        Self::with_writer(file)
    }

    fn disabled() -> Self {
        Self {
            trace: Trace::disabled(),
            worker: None,
        }
    }

    fn with_writer(writer: impl Write + Send + 'static) -> Result<Self, TraceError> {
        let (sender, receiver) = mpsc::channel();
        let process = ProcessInstance {
            pid: std::process::id(),
            started_unix_ms: unix_ms(),
            counter: NEXT_PROCESS_INSTANCE.fetch_add(1, Ordering::Relaxed),
        };
        let worker = thread::Builder::new()
            .name("kiki-telemetry".to_owned())
            .spawn(move || writer_loop(writer, receiver, process, Instant::now()))
            .map_err(TraceError::Start)?;
        let trace = Trace {
            sender: Some(sender),
        };
        trace.emit(|| TraceEvent::ProcessStarted);
        Ok(Self {
            trace,
            worker: Some(worker),
        })
    }

    #[cfg(test)]
    fn with_writer_for_test(writer: impl Write + Send + 'static) -> Self {
        Self::with_writer(writer).expect("test telemetry writer starts")
    }

    pub(crate) fn handle(&self) -> Trace {
        self.trace.clone()
    }

    #[cfg(test)]
    pub(crate) fn sync(&self) -> Result<(), TraceError> {
        let Some(sender) = &self.trace.sender else {
            return Ok(());
        };
        let (acknowledged, received) = mpsc::sync_channel(0);
        sender
            .send(WriterMessage::Sync(acknowledged))
            .map_err(|_| TraceError::WriterStopped)?;
        received.recv().map_err(|_| TraceError::WriterStopped)
    }

    pub(crate) fn shutdown(mut self) -> Result<(), TraceError> {
        self.finish()
    }

    fn finish(&mut self) -> Result<(), TraceError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        self.trace.emit(|| TraceEvent::ProcessShutdown);
        let sent = self
            .trace
            .sender
            .as_ref()
            .expect("enabled telemetry owns a sender")
            .send(WriterMessage::Shutdown)
            .map_err(|_| TraceError::WriterStopped);
        let joined = worker.join().map_err(|_| TraceError::WriterPanicked)?;
        joined?;
        sent
    }
}

impl Drop for TraceOwner {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

impl Trace {
    pub(crate) fn disabled() -> Self {
        Self { sender: None }
    }

    pub(crate) fn emit(&self, event: impl FnOnce() -> TraceEvent) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(WriterMessage::Event(event()));
        }
    }
}

#[derive(Serialize)]
struct TraceEnvelope {
    schema_version: u32,
    process: ProcessInstance,
    sequence: u64,
    unix_ms: i64,
    elapsed_ms: u64,
    #[serde(flatten)]
    event: TraceEvent,
}

#[derive(Serialize, Clone)]
struct ProcessInstance {
    pid: u32,
    started_unix_ms: i64,
    counter: u64,
}

enum WriterMessage {
    Event(TraceEvent),
    #[cfg(test)]
    Sync(SyncSender<()>),
    Shutdown,
}

fn writer_loop<W: Write>(
    mut writer: W,
    receiver: Receiver<WriterMessage>,
    process: ProcessInstance,
    started: Instant,
) -> Result<(), TraceError> {
    let mut sequence = 1_u64;
    while let Ok(message) = receiver.recv() {
        match message {
            WriterMessage::Event(event) => {
                let envelope = TraceEnvelope {
                    schema_version: SCHEMA_VERSION,
                    process: process.clone(),
                    sequence,
                    unix_ms: unix_ms(),
                    elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    event,
                };
                write_event(&mut writer, &envelope)?;
                sequence = sequence.wrapping_add(1);
            }
            #[cfg(test)]
            WriterMessage::Sync(acknowledged) => {
                let _ = acknowledged.send(());
            }
            WriterMessage::Shutdown => return Ok(()),
        }
    }
    Ok(())
}

fn write_event<W: Write, S: Serialize>(writer: &mut W, event: &S) -> Result<(), TraceError> {
    let mut line = serde_json::to_vec(event).map_err(TraceError::Encode)?;
    line.push(b'\n');
    writer.write_all(&line).map_err(TraceError::Write)?;
    writer.flush().map_err(TraceError::Write)
}

fn unix_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
        Err(error) => i64::try_from(error.duration().as_millis())
            .ok()
            .and_then(|milliseconds| milliseconds.checked_neg())
            .unwrap_or(i64::MIN),
    }
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum TraceEvent {
    ProcessStarted,
    ProcessShutdown,
    TuiCommandDispatch {
        command: ControlCommand,
    },
    TuiCommandQueued {
        command: ControlCommand,
    },
    RuntimeCommandReceived {
        command: ControlCommand,
    },
    ForegroundCancellationRequested,
    BackgroundCancellationRequested,
    BackgroundCancellationReturned,
    CoordinatorLockWaiting {
        operation: CoordinatorOperation,
    },
    CoordinatorLockAcquired {
        operation: CoordinatorOperation,
    },
    CoordinatorLockReleased {
        operation: CoordinatorOperation,
    },
    RootRunStarted {
        run_id: u64,
    },
    RootRunSettled {
        run_id: u64,
        outcome: Settlement,
    },
    ChildAdmitted {
        child_id: TaskId,
        parent_id: Option<TaskId>,
        model: &'static str,
        thinking: &'static str,
    },
    ChildRunnerStarted {
        child_id: TaskId,
    },
    ChildRunnerSetupStarted {
        child_id: TaskId,
    },
    ChildRunnerSetupFinished {
        child_id: TaskId,
    },
    ChildRunnerReturned {
        child_id: TaskId,
        outcome: Settlement,
    },
    ChildSettled {
        child_id: TaskId,
        outcome: Settlement,
    },
    ChildStopRequested {
        child_id: TaskId,
        cause: ChildStopCause,
    },
    ChildArenaConstructionStarted {
        child_id: TaskId,
    },
    ChildArenaPoolOpened {
        child_id: TaskId,
    },
    ChildArenaCheckedOut {
        child_id: TaskId,
    },
    ChildArenaCheckpointRestored {
        child_id: TaskId,
    },
    ChildArenaResultValuesReset {
        child_id: TaskId,
    },
    ChildArenaDumped {
        child_id: TaskId,
    },
    ChildArenaSettled {
        child_id: TaskId,
        outcome: Settlement,
    },
    ChildModelStepStarted {
        child_id: TaskId,
        model_step: u64,
    },
    ChildExecutionStarted {
        child_id: TaskId,
        model_step: u64,
    },
    ChildExecutionSettled {
        child_id: TaskId,
        model_step: u64,
        outcome: Settlement,
    },
    ChildProviderAttemptStarted {
        child_id: TaskId,
        model_step: u64,
        attempt: u32,
    },
    ChildProviderAttemptSettled {
        child_id: TaskId,
        model_step: u64,
        attempt: u32,
    },
}

impl TraceEvent {
    pub(crate) fn child_admitted(
        child_id: &TaskId,
        parent_id: Option<&TaskId>,
        selection: ModelSelection,
    ) -> Self {
        Self::ChildAdmitted {
            child_id: child_id.clone(),
            parent_id: parent_id.cloned(),
            model: selection.model().id,
            thinking: selection.thinking(),
        }
    }

    pub(crate) fn child_arena_phase(child_id: TaskId, phase: ChildArenaConstructionPhase) -> Self {
        match phase {
            ChildArenaConstructionPhase::PoolOpened => Self::ChildArenaPoolOpened { child_id },
            ChildArenaConstructionPhase::CheckedOut => Self::ChildArenaCheckedOut { child_id },
            ChildArenaConstructionPhase::CheckpointRestored => {
                Self::ChildArenaCheckpointRestored { child_id }
            }
            ChildArenaConstructionPhase::ResultValuesReset => {
                Self::ChildArenaResultValuesReset { child_id }
            }
            ChildArenaConstructionPhase::Dumped => Self::ChildArenaDumped { child_id },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Write};

    use serde::Serialize;
    use serde_json::Value;

    use super::{
        ProcessInstance, SCHEMA_VERSION, Settlement, Trace, TraceEnvelope, TraceError, TraceEvent,
        TraceOwner, write_event,
    };

    fn read_events(path: &std::path::Path) -> Vec<Value> {
        fs::read_to_string(path)
            .expect("trace file")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL event"))
            .collect()
    }

    #[test]
    fn disabled_trace_creates_no_file() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("trace.jsonl");
        let owner = TraceOwner::from_path(None).expect("disabled trace");

        owner
            .handle()
            .emit(|| TraceEvent::RootRunStarted { run_id: 1 });
        owner.shutdown().expect("disabled trace shutdown");

        assert!(!path.exists());
    }

    #[test]
    fn enabled_trace_appends_valid_versioned_jsonl_in_sequence() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("trace.jsonl");
        let owner = TraceOwner::open(&path).expect("open trace");
        let trace = owner.handle();

        trace.emit(|| TraceEvent::RootRunStarted { run_id: 7 });
        trace.emit(|| TraceEvent::RootRunSettled {
            run_id: 7,
            outcome: Settlement::Succeeded,
        });
        owner.shutdown().expect("trace shutdown");

        let events = read_events(&path);
        assert_eq!(
            events
                .iter()
                .map(|event| event["event"].as_str().expect("event name"))
                .collect::<Vec<_>>(),
            [
                "process_started",
                "root_run_started",
                "root_run_settled",
                "process_shutdown",
            ]
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event["sequence"].as_u64().expect("sequence"))
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(events.iter().all(|event| {
            event["schema_version"] == 1
                && event["process"]["pid"].is_u64()
                && event["process"]["started_unix_ms"].is_i64()
                && event["process"]["counter"].is_u64()
                && event["unix_ms"].is_i64()
                && event["elapsed_ms"].is_u64()
        }));
        assert_eq!(events[1]["run_id"], 7);
        assert_eq!(events[2]["outcome"], "succeeded");
    }

    #[test]
    fn concurrent_emits_receive_one_contiguous_writer_sequence() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("trace.jsonl");
        let owner = TraceOwner::open(&path).expect("open trace");
        let mut workers = Vec::new();

        for run_id in 1..=32 {
            let trace = owner.handle();
            workers.push(std::thread::spawn(move || {
                trace.emit(|| TraceEvent::RootRunStarted { run_id });
            }));
        }
        for worker in workers {
            worker.join().expect("emitter thread");
        }
        owner.shutdown().expect("trace shutdown");

        let events = read_events(&path);
        assert_eq!(events.len(), 34);
        assert_eq!(
            events
                .iter()
                .map(|event| event["sequence"].as_u64().expect("sequence"))
                .collect::<Vec<_>>(),
            (1..=34).collect::<Vec<_>>()
        );
        let mut run_ids = events
            .iter()
            .filter(|event| event["event"] == "root_run_started")
            .map(|event| event["run_id"].as_u64().expect("run id"))
            .collect::<Vec<_>>();
        run_ids.sort_unstable();
        assert_eq!(run_ids, (1..=32).collect::<Vec<_>>());
    }

    #[test]
    fn writer_writes_each_serialized_envelope_as_one_jsonl_line() {
        let mut writer = RecordingWriter::default();
        let envelope = TraceEnvelope {
            schema_version: SCHEMA_VERSION,
            process: ProcessInstance {
                pid: 42,
                started_unix_ms: 1,
                counter: 2,
            },
            sequence: 3,
            unix_ms: 4,
            elapsed_ms: 5,
            event: TraceEvent::RootRunStarted { run_id: 6 },
        };

        write_event(&mut writer, &envelope).expect("write trace event");

        assert_eq!(writer.writes.len(), 1);
        assert_eq!(writer.flushes, 1);
        assert_eq!(writer.writes[0].last(), Some(&b'\n'));
        assert_eq!(
            serde_json::from_slice::<Value>(&writer.writes[0]).expect("valid JSONL event")["run_id"],
            6
        );
    }

    #[test]
    fn sync_flushes_events_before_shutdown() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let path = temporary.path().join("trace.jsonl");
        let owner = TraceOwner::open(&path).expect("open trace");

        owner
            .handle()
            .emit(|| TraceEvent::RootRunStarted { run_id: 3 });
        owner.sync().expect("trace sync");

        assert_eq!(
            read_events(&path)
                .iter()
                .map(|event| event["event"].as_str().expect("event name"))
                .collect::<Vec<_>>(),
            ["process_started", "root_run_started"]
        );
        owner.shutdown().expect("trace shutdown");
    }

    #[test]
    fn trace_file_open_failure_is_reported() {
        let temporary = tempfile::tempdir().expect("temporary directory");

        assert!(TraceOwner::open(temporary.path()).is_err());
    }

    #[test]
    fn runtime_write_failure_is_reported_only_when_the_owner_finishes() {
        let owner = TraceOwner::with_writer_for_test(FailOnSecondFlush { flushes: 0 });
        let trace = owner.handle();

        trace.emit(|| TraceEvent::RootRunStarted { run_id: 1 });
        trace.emit(|| TraceEvent::RootRunSettled {
            run_id: 1,
            outcome: Settlement::Failed,
        });

        assert!(owner.shutdown().is_err());
    }

    #[test]
    fn empty_trace_path_is_rejected() {
        assert!(TraceOwner::from_path(Some(OsString::new())).is_err());
    }

    #[test]
    fn disabled_trace_does_not_evaluate_event_factory() {
        let trace = Trace::disabled();
        let mut emitted = false;

        trace.emit(|| {
            emitted = true;
            TraceEvent::RootRunStarted { run_id: 1 }
        });

        assert!(!emitted);
    }

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
        flushes: usize,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    struct FailingSerialization;

    impl Serialize for FailingSerialization {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("controlled encoding failure"))
        }
    }

    #[test]
    fn writer_does_not_write_when_encoding_fails() {
        let mut writer = RecordingWriter::default();

        let error = write_event(&mut writer, &FailingSerialization)
            .expect_err("controlled serialisation failure");

        assert!(matches!(error, TraceError::Encode(_)));
        assert!(writer.writes.is_empty());
        assert_eq!(writer.flushes, 0);
    }

    struct FailOnSecondFlush {
        flushes: usize,
    }

    impl Write for FailOnSecondFlush {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if self.flushes == 2 {
                Err(io::Error::other("controlled trace failure"))
            } else {
                Ok(())
            }
        }
    }
}
