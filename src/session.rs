mod codec;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use rustix::fs::{FlockOperation, fcntl_lock};
use rustix::io::Errno;
use thiserror::Error;
use tokio::task;

use crate::agent::TerminalEvidence;
use crate::arena::{Checkpoint, Execution};
use crate::config::{ModelSelection, SessionPreferences};
use crate::model::{AttemptAccounting, ModelHistory};
use crate::subagents::{RestoredTaskRecord, TaskId, TaskRecord, TaskState};

const DATABASE_SUFFIX: &str = ".sqlite3";
const LOCK_SUFFIX: &str = ".lock";
const SQLITE_HEADER_BYTES: usize = 100;
const SQLITE_SIGNATURE: &[u8] = b"SQLite format 3\0";
const FORMAT_VERSION: u32 = 1;
const INTERRUPTION_MESSAGE: &str = "Interrupted by previous process exit";

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error("session ID must be exactly 32 lowercase hexadecimal characters")]
    InvalidId,
    #[error("could not generate a session ID")]
    Random,
    #[error("session is already owned by another process")]
    Owned,
    #[error("session database is missing")]
    Missing,
    #[error("session database is not a regular file")]
    NotAFile,
    #[error("session database header is truncated")]
    TruncatedHeader,
    #[error("session database has an invalid SQLite signature")]
    InvalidSignature,
    #[error("session database uses WAL or an unsupported journal mode")]
    UnsupportedJournalMode,
    #[error("session database format version {found} is unsupported")]
    UnsupportedVersion { found: u32 },
    #[error("session database ID does not match its filename")]
    StoredIdMismatch,
    #[error("session database connection is not configured for {setting}")]
    Configuration { setting: &'static str },
    #[error("session data could not be decoded: {0}")]
    Codec(String),
    #[error("session storage I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("session database operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("session writer is unavailable")]
    WriterUnavailable,
    #[error("session still has active write handles")]
    ActiveWriteHandles,
    #[error("session database identifier overflowed")]
    IdOverflow,
    #[error("session lifecycle transition was no longer valid")]
    InvalidTransition,
    #[error("session name must not be empty")]
    EmptyName,
    #[error("subagent task provenance does not identify a stored model step")]
    InvalidTaskProvenance,
}

impl SessionError {
    pub(crate) fn requires_writable_recovery(&self) -> bool {
        matches!(
            self,
            Self::Sqlite(error) if error.sqlite_extended_error_code() == Some(776)
        )
    }
}

struct SessionId(String);

impl SessionId {
    fn generate() -> Result<Self, SessionError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| SessionError::Random)?;
        Ok(Self(codec::encode_hex(&bytes)))
    }

    fn parse(value: &str) -> Result<Self, SessionError> {
        (value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte <= b'f')))
        .then(|| Self(value.to_owned()))
        .ok_or(SessionError::InvalidId)
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

struct SessionPaths {
    database: PathBuf,
    lock: PathBuf,
}

impl SessionPaths {
    fn for_id(home: &Path, id: &SessionId) -> Self {
        let directory = home.join(".kiki/sessions");
        Self {
            database: directory.join(format!("{}{DATABASE_SUFFIX}", id.as_str())),
            lock: directory.join(format!("{}{LOCK_SUFFIX}", id.as_str())),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PersistedCode {
    pub(crate) source: String,
    pub(crate) call_id: crate::model::ModelCallId,
    pub(crate) commentary: Option<String>,
    pub(crate) reasoning_summary: String,
    pub(crate) selection: Option<ModelSelection>,
    pub(crate) replay: crate::model::ProviderReplay,
    pub(crate) result: crate::model::CodeResult,
    pub(crate) execution: Option<crate::arena::Execution>,
    pub(crate) attempts: Vec<crate::model::AttemptAccounting>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PersistedTerminal {
    Final {
        step_id: u64,
        text: String,
        reasoning_summary: String,
        replay: crate::model::ProviderReplay,
        attempts: Vec<crate::model::AttemptAccounting>,
    },
    Cancelled {
        evidence: Option<crate::agent::TerminalEvidence>,
        attempts: Vec<crate::model::AttemptAccounting>,
    },
    Failed {
        message: String,
        evidence: Option<crate::agent::TerminalEvidence>,
        attempts: Vec<crate::model::AttemptAccounting>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadableCode {
    source: String,
    commentary: Option<String>,
    reasoning_summary: String,
    result: crate::model::CodeResult,
    attempts: Vec<AttemptAccounting>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadableTerminal {
    Final {
        text: String,
        reasoning_summary: String,
        attempts: Vec<AttemptAccounting>,
    },
    Cancelled {
        evidence: Option<TerminalEvidence>,
        attempts: Vec<AttemptAccounting>,
    },
    Failed {
        message: String,
        evidence: Option<TerminalEvidence>,
        attempts: Vec<AttemptAccounting>,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoredRecordKind {
    UserRequest {
        request: String,
    },
    CompletedExecution {
        step_id: u64,
        source: String,
        commentary: Option<String>,
        reasoning_summary: String,
        execution: Execution,
        attempts: Vec<AttemptAccounting>,
    },
    FinalMessage {
        step_id: u64,
        text: String,
        reasoning_summary: String,
        terminal: ReadableTerminal,
    },
    CancelledRun {
        terminal: ReadableTerminal,
    },
    FailedRun {
        message: String,
        terminal: ReadableTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestoredRecord {
    pub(crate) revision: u64,
    pub(crate) run_id: u64,
    pub(crate) kind: RestoredRecordKind,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RestoredTask {
    pub(crate) record: TaskRecord,
    pub(crate) admission_anchor_revision: u64,
    pub(crate) terminal_anchor_revision: u64,
    pub(crate) admitted_revision: u64,
    pub(crate) terminal_revision: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct RestoredExecutableState {
    pub(crate) checkpoint: Checkpoint,
    pub(crate) history: ModelHistory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionMetadata {
    name: Option<String>,
    preferences: SessionPreferences,
}

impl SessionMetadata {
    pub(crate) fn new(name: Option<String>, preferences: SessionPreferences) -> Self {
        Self { name, preferences }
    }

    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn preferences(&self) -> &SessionPreferences {
        &self.preferences
    }

    pub(crate) fn update_preferences(&mut self, preferences: SessionPreferences) {
        self.preferences = preferences;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SessionRestore {
    pub(crate) metadata: Option<SessionMetadata>,
    pub(crate) records: Vec<RestoredRecord>,
    pub(crate) tasks: Vec<RestoredTask>,
    pub(crate) reserved_task_ids: Vec<TaskId>,
    pub(crate) footer_adjustment: Vec<AttemptAccounting>,
    pub(crate) next_task: Option<u64>,
    pub(crate) executable: Option<RestoredExecutableState>,
}

#[derive(Clone)]
pub(crate) struct SessionWriter {
    state: Arc<SessionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionWriteKind {
    Preferences,
    Relocation,
    Name,
    Request,
    Step,
    Settlement,
    Closure,
    TaskAdmission,
    TaskStep,
    TaskTerminal,
}

pub(crate) struct TaskAdmission {
    task_id: i64,
    parent_task_id: Option<i64>,
    origin_step_id: i64,
    task_revision: i64,
    details_json: String,
}

impl TaskAdmission {
    pub(crate) fn from_record(
        record: &TaskRecord,
        origin_step_id: u64,
    ) -> Result<Self, SessionError> {
        Ok(Self {
            task_id: task_sql_id(record.storage_id())?,
            parent_task_id: record.parent_storage_id().map(task_sql_id).transpose()?,
            origin_step_id: i64::try_from(origin_step_id)
                .map_err(|_| SessionError::InvalidTaskProvenance)?,
            task_revision: i64::try_from(record.revision())
                .map_err(|_| SessionError::IdOverflow)?,
            details_json: codec::encode_task_details(record)
                .map_err(|error| SessionError::Codec(error.to_string()))?,
        })
    }
}

pub(crate) struct TaskSnapshot {
    task_id: i64,
    state: &'static str,
    task_revision: i64,
    details_json: String,
}

impl TaskSnapshot {
    pub(crate) fn from_record(record: &TaskRecord) -> Result<Self, SessionError> {
        let state = record.observation().state();
        if state.is_terminal() || record.terminal_at_ms().is_some() || record.outcome().is_some() {
            return Err(SessionError::InvalidTransition);
        }
        Ok(Self {
            task_id: task_sql_id(record.storage_id())?,
            state: state.as_str(),
            task_revision: i64::try_from(record.revision())
                .map_err(|_| SessionError::IdOverflow)?,
            details_json: codec::encode_task_details(record)
                .map_err(|error| SessionError::Codec(error.to_string()))?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStepCommit {
    Committed,
    Superseded,
    Rejected,
}

pub(crate) struct TaskTerminal {
    task_id: i64,
    state: &'static str,
    task_revision: i64,
    details_json: String,
}

impl TaskTerminal {
    pub(crate) fn from_record(record: &TaskRecord) -> Result<Self, SessionError> {
        let state = record.observation().state();
        if !state.is_terminal() {
            return Err(SessionError::InvalidTransition);
        }
        Ok(Self {
            task_id: task_sql_id(record.storage_id())?,
            state: state.as_str(),
            task_revision: i64::try_from(record.revision())
                .map_err(|_| SessionError::IdOverflow)?,
            details_json: codec::encode_task_details(record)
                .map_err(|error| SessionError::Codec(error.to_string()))?,
        })
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct SessionWriteGate {
    state: Arc<(Mutex<SessionWriteGateState>, Condvar)>,
    started: Arc<tokio::sync::Notify>,
}

#[cfg(test)]
struct SessionWriteGateState {
    kind: SessionWriteKind,
    started: bool,
    released: bool,
}

#[cfg(test)]
impl SessionWriteGate {
    pub(crate) fn new(kind: SessionWriteKind) -> Self {
        Self {
            state: Arc::new((
                Mutex::new(SessionWriteGateState {
                    kind,
                    started: false,
                    released: false,
                }),
                Condvar::new(),
            )),
            started: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) async fn wait_until_started(&self) {
        loop {
            let notified = self.started.notified();
            if self.state.0.lock().expect("write gate lock").started {
                return;
            }
            notified.await;
        }
    }

    pub(crate) fn release(&self) {
        let (state, wake) = &*self.state;
        state.lock().expect("write gate lock").released = true;
        wake.notify_all();
    }

    pub(crate) fn has_started(&self) -> bool {
        self.state.0.lock().expect("write gate lock").started
    }

    fn wait(&self, kind: SessionWriteKind) {
        let (state, wake) = &*self.state;
        let mut state = state.lock().expect("write gate lock");
        if state.kind != kind {
            return;
        }
        state.started = true;
        self.started.notify_one();
        while !state.released {
            state = wake.wait(state).expect("write gate lock");
        }
    }
}

struct SessionState {
    connection: Mutex<Connection>,
    active_execution_step: Mutex<Option<u64>>,
    _writer_lock: File,
    #[cfg(test)]
    write_gate: Option<SessionWriteGate>,
}

pub(crate) struct Session {
    id: SessionId,
    state: Arc<SessionState>,
}

pub(crate) struct SessionListRow {
    id: String,
    updated_at_ms: i64,
    cwd: PathBuf,
    name: Option<String>,
    first_request: Option<String>,
}

impl SessionListRow {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn updated_at_ms(&self) -> i64 {
        self.updated_at_ms
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) fn first_request(&self) -> Option<&str> {
        self.first_request.as_deref()
    }
}

pub(crate) struct SessionListWarning {
    id: String,
}

impl SessionListWarning {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Default)]
pub(crate) struct SessionListing {
    rows: Vec<SessionListRow>,
    warnings: Vec<SessionListWarning>,
}

impl SessionListing {
    pub(crate) fn into_parts(self) -> (Vec<SessionListRow>, Vec<SessionListWarning>) {
        (self.rows, self.warnings)
    }
}

impl Session {
    pub(crate) fn create(
        home: &Path,
        cwd: &Path,
        selection: ModelSelection,
        checkpoint: Checkpoint,
    ) -> Result<Self, SessionError> {
        Self::create_with_id_source(home, cwd, selection, checkpoint, SessionId::generate, None)
    }

    #[cfg(test)]
    pub(crate) fn create_with_write_gate(
        home: &Path,
        cwd: &Path,
        selection: ModelSelection,
        checkpoint: Checkpoint,
        gate: SessionWriteGate,
    ) -> Result<Self, SessionError> {
        Self::create_with_id_source(
            home,
            cwd,
            selection,
            checkpoint,
            SessionId::generate,
            Some(gate),
        )
    }

    #[cfg(test)]
    fn create_with_ids_for_test(
        home: &Path,
        cwd: &Path,
        selection: ModelSelection,
        checkpoint: Checkpoint,
        ids: impl IntoIterator<Item = String>,
    ) -> Result<Self, SessionError> {
        let mut ids = ids.into_iter();
        Self::create_with_id_source(
            home,
            cwd,
            selection,
            checkpoint,
            || {
                ids.next()
                    .ok_or(SessionError::Random)
                    .and_then(|id| SessionId::parse(&id))
            },
            None,
        )
    }

    fn create_with_id_source(
        home: &Path,
        cwd: &Path,
        selection: ModelSelection,
        checkpoint: Checkpoint,
        mut next_id: impl FnMut() -> Result<SessionId, SessionError>,
        #[cfg(test)] write_gate: Option<SessionWriteGate>,
        #[cfg(not(test))] write_gate: Option<()>,
    ) -> Result<Self, SessionError> {
        sessions_directory(home)?;
        loop {
            let id = next_id()?;
            let paths = SessionPaths::for_id(home, &id);
            let writer_lock = match create_lock(&paths.lock) {
                Ok(lock) => lock,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            match create_database(&paths.database) {
                Ok(()) => {
                    return initialise(
                        id,
                        paths,
                        writer_lock,
                        cwd,
                        selection,
                        checkpoint,
                        write_gate,
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    drop(writer_lock);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    pub(crate) fn list(home: &Path) -> Result<SessionListing, SessionError> {
        let directory = home.join(".kiki/sessions");
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SessionListing::default());
            }
            Err(error) => return Err(SessionError::Io(error)),
        };
        let mut listing = SessionListing::default();
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(id) = name
                .strip_suffix(DATABASE_SUFFIX)
                .and_then(|value| SessionId::parse(value).ok())
            else {
                continue;
            };
            let is_file = match entry.file_type() {
                Ok(file_type) => file_type.is_file(),
                Err(_) => {
                    listing.warnings.push(SessionListWarning {
                        id: id.as_str().to_owned(),
                    });
                    continue;
                }
            };
            if !is_file {
                continue;
            }
            match read_list_row(&entry.path(), &id) {
                Ok(row) => listing.rows.push(row),
                Err(_) => listing.warnings.push(SessionListWarning {
                    id: id.as_str().to_owned(),
                }),
            }
        }
        Ok(listing)
    }

    pub(crate) fn read_cwd(home: &Path, value: &str) -> Result<PathBuf, SessionError> {
        let id = SessionId::parse(value)?;
        let paths = SessionPaths::for_id(home, &id);
        existing_database(&paths.database)?;
        preflight_existing_database(&paths.database)?;
        let connection = open_read_only_database(&paths.database)?;
        read_stored_cwd(&connection, &id)
    }

    pub(crate) fn open(home: &Path, value: &str) -> Result<Self, SessionError> {
        let id = SessionId::parse(value)?;
        let paths = SessionPaths::for_id(home, &id);
        let metadata = fs::metadata(&paths.database).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                SessionError::Missing
            } else {
                SessionError::Io(error)
            }
        })?;
        if !metadata.is_file() {
            return Err(SessionError::NotAFile);
        }
        let writer_lock = open_lock(&paths.lock)?;
        preflight_existing_database(&paths.database)?;
        set_owner_only(
            paths
                .database
                .parent()
                .expect("session database has a parent"),
            0o700,
        )?;
        set_owner_only(&paths.database, 0o600)?;
        let mut connection = open_writable_database(&paths.database)?;
        read_stored_cwd(&connection, &id)?;
        recover_interrupted_operations(&mut connection)?;
        Ok(Self {
            id,
            state: Arc::new(SessionState {
                connection: Mutex::new(connection),
                active_execution_step: Mutex::new(None),
                _writer_lock: writer_lock,
                #[cfg(test)]
                write_gate: None,
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn read_initial_checkpoint(
        home: &Path,
        value: &str,
    ) -> Result<Checkpoint, SessionError> {
        let id = SessionId::parse(value)?;
        let paths = SessionPaths::for_id(home, &id);
        preflight_existing_database(&paths.database)?;
        let connection =
            Connection::open_with_flags(&paths.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        load_initial_checkpoint(&connection)
    }

    pub(crate) fn id(&self) -> &str {
        self.id.as_str()
    }

    #[cfg(test)]
    pub(crate) fn initial_checkpoint(&self) -> Result<Checkpoint, SessionError> {
        let connection = self.connection()?;
        load_initial_checkpoint(&connection)
    }

    pub(crate) fn cwd(&self) -> Result<PathBuf, SessionError> {
        let connection = self.connection()?;
        read_stored_cwd(&connection, &self.id)
    }

    pub(crate) fn restore(&self) -> Result<SessionRestore, SessionError> {
        let mut connection = self.connection()?;
        load_session_restore(&mut connection)
    }

    pub(crate) fn metadata(&self) -> Result<SessionMetadata, SessionError> {
        let connection = self.connection()?;
        load_session_metadata(&connection)
    }

    pub(crate) fn writer(&self) -> SessionWriter {
        SessionWriter {
            state: Arc::clone(&self.state),
        }
    }

    #[cfg(test)]
    pub(crate) fn connection_for_test(&self) -> Result<MutexGuard<'_, Connection>, SessionError> {
        self.connection()
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, SessionError> {
        self.state
            .connection
            .lock()
            .map_err(|_| SessionError::WriterUnavailable)
    }

    pub(crate) fn close(self) -> Result<(), SessionError> {
        let Session { state, .. } = self;
        let SessionState {
            connection,
            _writer_lock,
            ..
        } = Arc::try_unwrap(state).map_err(|_| SessionError::ActiveWriteHandles)?;
        let connection = connection
            .into_inner()
            .map_err(|_| SessionError::WriterUnavailable)?;
        match connection.close() {
            Ok(()) => Ok(()),
            Err((connection, error)) => {
                drop(connection);
                Err(SessionError::Sqlite(error))
            }
        }
    }
}

impl SessionWriter {
    pub(crate) fn mark_execution_started(&self, step_id: u64) {
        *self
            .state
            .active_execution_step
            .lock()
            .expect("active execution lock") = Some(step_id);
    }

    pub(crate) fn clear_execution(&self, step_id: u64) {
        let mut active = self
            .state
            .active_execution_step
            .lock()
            .expect("active execution lock");
        if *active == Some(step_id) {
            *active = None;
        }
    }

    pub(crate) fn clear_active_execution(&self) {
        *self
            .state
            .active_execution_step
            .lock()
            .expect("active execution lock") = None;
    }

    pub(crate) fn active_execution_step(&self) -> Option<u64> {
        *self
            .state
            .active_execution_step
            .lock()
            .expect("active execution lock")
    }

    pub(crate) async fn commit_preferences(
        &self,
        preferences: SessionPreferences,
    ) -> Result<(), SessionError> {
        self.commit_startup_preferences(preferences, None).await
    }

    pub(crate) async fn commit_startup_preferences(
        &self,
        preferences: SessionPreferences,
        relocated_cwd: Option<PathBuf>,
    ) -> Result<(), SessionError> {
        let preferences_json = codec::encode_session_preferences(&preferences)
            .map_err(|error| SessionError::Codec(error.to_string()))?;
        let relocated_cwd = relocated_cwd
            .as_deref()
            .map(codec::encode_path)
            .transpose()
            .map_err(|error| SessionError::Codec(error.to_string()))?;
        let kind = if relocated_cwd.is_some() {
            SessionWriteKind::Relocation
        } else {
            SessionWriteKind::Preferences
        };
        self.write(kind, move |connection| {
            commit_preferences(connection, &preferences_json, relocated_cwd.as_deref())
        })
        .await
    }

    pub(crate) async fn commit_name(&self, name: String) -> Result<(), SessionError> {
        if name.is_empty() {
            return Err(SessionError::EmptyName);
        }
        self.write(SessionWriteKind::Name, move |connection| {
            commit_name(connection, &name)
        })
        .await
    }

    pub(crate) async fn commit_request(&self, request: String) -> Result<u64, SessionError> {
        self.write(SessionWriteKind::Request, move |connection| {
            commit_request(connection, &request)
        })
        .await
    }

    pub(crate) async fn start_model_step(&self, run_id: u64) -> Result<u64, SessionError> {
        self.write(SessionWriteKind::Step, move |connection| {
            start_model_step(connection, run_id)
        })
        .await
    }

    pub(crate) async fn settle_execution(
        &self,
        run_id: u64,
        step_id: u64,
        code: PersistedCode,
        checkpoint: Checkpoint,
    ) -> Result<(), SessionError> {
        self.write(SessionWriteKind::Settlement, move |connection| {
            settle_execution(connection, run_id, step_id, &code, &checkpoint)
        })
        .await
    }

    pub(crate) async fn close_run(
        &self,
        run_id: u64,
        pending_step_id: Option<u64>,
        terminal: impl FnOnce() -> PersistedTerminal + Send + 'static,
        code: Option<PersistedCode>,
    ) -> Result<PersistedTerminal, SessionError> {
        self.write(SessionWriteKind::Closure, move |connection| {
            let terminal = terminal();
            close_run(
                connection,
                run_id,
                pending_step_id,
                &terminal,
                code.as_ref(),
            )?;
            Ok(terminal)
        })
        .await
    }

    pub(crate) async fn admit_subagent_task(
        &self,
        admission: TaskAdmission,
        reject: impl FnOnce() -> bool + Send + 'static,
    ) -> Result<bool, SessionError> {
        self.write(SessionWriteKind::TaskAdmission, move |connection| {
            if reject() {
                Ok(false)
            } else {
                admit_subagent_task(connection, &admission).map(|()| true)
            }
        })
        .await
    }

    pub(crate) async fn task_step(
        &self,
        step: TaskSnapshot,
        reject: impl FnOnce() -> bool + Send + 'static,
    ) -> Result<TaskStepCommit, SessionError> {
        self.write(SessionWriteKind::TaskStep, move |connection| {
            if reject() {
                Ok(TaskStepCommit::Rejected)
            } else {
                task_step(connection, &step)
            }
        })
        .await
    }

    pub(crate) async fn terminal_subagent_task(
        &self,
        terminal: TaskTerminal,
    ) -> Result<(), SessionError> {
        self.write(SessionWriteKind::TaskTerminal, move |connection| {
            terminal_subagent_task(connection, &terminal)
        })
        .await
    }

    async fn write<T: Send + 'static>(
        &self,
        kind: SessionWriteKind,
        operation: impl FnOnce(&mut Connection) -> Result<T, SessionError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let state = Arc::clone(&self.state);
        task::spawn_blocking(move || {
            #[cfg(not(test))]
            let _ = kind;
            #[cfg(test)]
            if let Some(gate) = &state.write_gate {
                gate.wait(kind);
            }
            let mut connection = state
                .connection
                .lock()
                .map_err(|_| SessionError::WriterUnavailable)?;
            operation(&mut connection)
        })
        .await
        .map_err(|_| SessionError::WriterUnavailable)?
    }
}

fn commit_preferences(
    connection: &mut Connection,
    preferences_json: &str,
    cwd: Option<&str>,
) -> Result<(), SessionError> {
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let revision: i64 = transaction.query_row(
        "SELECT revision FROM session WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    let updated = match cwd {
        Some(cwd) => transaction.execute(
            "UPDATE session SET preferences_json = ?1, cwd = ?2, updated_at_ms = ?3, revision = ?4 WHERE singleton = 1",
            params![preferences_json, cwd, now, next_revision],
        ),
        None => transaction.execute(
            "UPDATE session SET preferences_json = ?1, updated_at_ms = ?2, revision = ?3 WHERE singleton = 1",
            params![preferences_json, now, next_revision],
        ),
    }?;
    if updated != 1 {
        return Err(SessionError::InvalidTransition);
    }
    transaction.commit()?;
    Ok(())
}

fn commit_name(connection: &mut Connection, name: &str) -> Result<(), SessionError> {
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let revision: i64 = transaction.query_row(
        "SELECT revision FROM session WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    if transaction.execute(
        "UPDATE session SET name = ?1, updated_at_ms = ?2, revision = ?3 WHERE singleton = 1",
        params![name, now, next_revision],
    )? != 1
    {
        return Err(SessionError::InvalidTransition);
    }
    transaction.commit()?;
    Ok(())
}

fn commit_request(connection: &mut Connection, request: &str) -> Result<u64, SessionError> {
    let payload =
        codec::encode_request(request).map_err(|error| SessionError::Codec(error.to_string()))?;
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (parent_id, revision): (i64, i64) = transaction.query_row(
        "SELECT active_entry_id, revision FROM session WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let checkpoint_id: i64 = transaction.query_row(
        "SELECT checkpoint_id FROM entries WHERE id = ?1",
        params![parent_id],
        |row| row.get(0),
    )?;
    transaction.execute("INSERT INTO runs DEFAULT VALUES", [])?;
    let run_id = transaction.last_insert_rowid();
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    transaction.execute(
        "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, provider_replay, created_at_ms, revision) VALUES (?1, ?2, 'user_request', ?3, ?4, NULL, ?5, ?6)",
        params![parent_id, run_id, checkpoint_id, payload, now, next_revision],
    )?;
    let entry_id = transaction.last_insert_rowid();
    transaction.execute(
        "UPDATE session SET active_entry_id = ?1, updated_at_ms = ?2, revision = ?3 WHERE singleton = 1",
        params![entry_id, now, next_revision],
    )?;
    transaction.commit()?;
    u64::try_from(run_id).map_err(|_| SessionError::IdOverflow)
}

fn start_model_step(connection: &mut Connection, run_id: u64) -> Result<u64, SessionError> {
    let run_id = i64::try_from(run_id).map_err(|_| SessionError::IdOverflow)?;
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let revision: i64 = transaction.query_row(
        "SELECT revision FROM session WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO model_steps (run_id) VALUES (?1)",
        params![run_id],
    )?;
    let step_id = transaction.last_insert_rowid();
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    transaction.execute(
        "UPDATE session SET updated_at_ms = ?1, revision = ?2 WHERE singleton = 1",
        params![now, next_revision],
    )?;
    transaction.commit()?;
    u64::try_from(step_id).map_err(|_| SessionError::IdOverflow)
}

fn settle_execution(
    connection: &mut Connection,
    run_id: u64,
    step_id: u64,
    code: &PersistedCode,
    checkpoint: &Checkpoint,
) -> Result<(), SessionError> {
    let run_id = i64::try_from(run_id).map_err(|_| SessionError::IdOverflow)?;
    let step_id = i64::try_from(step_id).map_err(|_| SessionError::IdOverflow)?;
    let stored_code =
        codec::encode_execution(code).map_err(|error| SessionError::Codec(error.to_string()))?;
    let stored_checkpoint = codec::encode_checkpoint(checkpoint)
        .map_err(|error| SessionError::Codec(error.to_string()))?;
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (parent_id, revision): (i64, i64) = transaction.query_row(
        "SELECT active_entry_id, revision FROM session WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    transaction.execute(
        "INSERT INTO checkpoints (runtime_identity, arena_state, authorisations_json) VALUES (?1, ?2, ?3)",
        params![
            stored_checkpoint.runtime_identity,
            stored_checkpoint.arena_state,
            stored_checkpoint.authorisations_json,
        ],
    )?;
    let checkpoint_id = transaction.last_insert_rowid();
    transaction.execute(
        "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, provider_replay, created_at_ms, revision) VALUES (?1, ?2, 'execution', ?3, ?4, ?5, ?6, ?7)",
        params![
            parent_id,
            run_id,
            checkpoint_id,
            stored_code.payload_json,
            stored_code.provider_replay,
            now,
            next_revision,
        ],
    )?;
    let entry_id = transaction.last_insert_rowid();
    if transaction.execute(
        "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2 AND run_id = ?3 AND settled_entry_id IS NULL",
        params![entry_id, step_id, run_id],
    )? != 1 {
        return Err(SessionError::InvalidTransition);
    }
    transaction.execute(
        "UPDATE session SET active_entry_id = ?1, updated_at_ms = ?2, revision = ?3 WHERE singleton = 1",
        params![entry_id, now, next_revision],
    )?;
    transaction.commit()?;
    Ok(())
}

fn close_run(
    connection: &mut Connection,
    run_id: u64,
    pending_step_id: Option<u64>,
    terminal: &PersistedTerminal,
    code: Option<&PersistedCode>,
) -> Result<(), SessionError> {
    let run_id = i64::try_from(run_id).map_err(|_| SessionError::IdOverflow)?;
    let pending_step_id = pending_step_id
        .map(i64::try_from)
        .transpose()
        .map_err(|_| SessionError::IdOverflow)?;
    let stored_terminal = codec::encode_terminal(terminal, code)
        .map_err(|error| SessionError::Codec(error.to_string()))?;
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (parent_id, checkpoint_id, revision): (i64, i64, i64) = transaction.query_row(
        "SELECT session.active_entry_id, entries.checkpoint_id, session.revision FROM session JOIN entries ON entries.id = session.active_entry_id WHERE session.singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    transaction.execute(
        "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, provider_replay, created_at_ms, revision) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            parent_id,
            run_id,
            stored_terminal.kind,
            checkpoint_id,
            stored_terminal.payload_json,
            stored_terminal.provider_replay,
            now,
            next_revision,
        ],
    )?;
    let entry_id = transaction.last_insert_rowid();
    if let Some(step_id) = pending_step_id
        && transaction.execute(
            "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2 AND run_id = ?3 AND settled_entry_id IS NULL",
            params![entry_id, step_id, run_id],
        )? != 1
    {
        return Err(SessionError::InvalidTransition);
    }
    if transaction.execute(
        "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2 AND terminal_entry_id IS NULL",
        params![entry_id, run_id],
    )? != 1
    {
        return Err(SessionError::InvalidTransition);
    }
    transaction.execute(
        "UPDATE session SET active_entry_id = ?1, updated_at_ms = ?2, revision = ?3 WHERE singleton = 1",
        params![entry_id, now, next_revision],
    )?;
    transaction.commit()?;
    Ok(())
}

fn admit_subagent_task(
    connection: &mut Connection,
    admission: &TaskAdmission,
) -> Result<(), SessionError> {
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let revision: i64 = transaction.query_row(
        "SELECT revision FROM session WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let origin_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM model_steps WHERE id = ?1)",
        params![admission.origin_step_id],
        |row| row.get(0),
    )?;
    if !origin_exists {
        return Err(SessionError::InvalidTaskProvenance);
    }
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    transaction.execute(
        "INSERT INTO subagent_tasks (id, parent_task_id, origin_step_id, state, admitted_revision, terminal_revision, task_revision, details_json) VALUES (?1, ?2, ?3, 'starting', ?4, NULL, ?5, ?6)",
        params![
            admission.task_id,
            admission.parent_task_id,
            admission.origin_step_id,
            next_revision,
            admission.task_revision,
            admission.details_json,
        ],
    )?;
    transaction.execute(
        "UPDATE session SET updated_at_ms = ?1, revision = ?2 WHERE singleton = 1",
        params![now, next_revision],
    )?;
    transaction.commit()?;
    Ok(())
}

fn task_step(
    connection: &mut Connection,
    step: &TaskSnapshot,
) -> Result<TaskStepCommit, SessionError> {
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let task = transaction
        .query_row(
            "SELECT terminal_revision, task_revision FROM subagent_tasks WHERE id = ?1",
            params![step.task_id],
            |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let Some((terminal_revision, task_revision)) = task else {
        return Err(SessionError::InvalidTransition);
    };
    if terminal_revision.is_some() || task_revision >= step.task_revision {
        return Ok(TaskStepCommit::Superseded);
    }
    if transaction.execute(
        "UPDATE subagent_tasks SET state = ?1, task_revision = ?2, details_json = ?3 WHERE id = ?4 AND terminal_revision IS NULL AND task_revision < ?2",
        params![step.state, step.task_revision, step.details_json, step.task_id],
    )? != 1
    {
        return Ok(TaskStepCommit::Superseded);
    }
    let revision: i64 = transaction.query_row(
        "SELECT revision FROM session WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    transaction.execute(
        "UPDATE session SET updated_at_ms = ?1, revision = ?2 WHERE singleton = 1",
        params![now, next_revision],
    )?;
    transaction.commit()?;
    Ok(TaskStepCommit::Committed)
}

fn terminal_subagent_task(
    connection: &mut Connection,
    terminal: &TaskTerminal,
) -> Result<(), SessionError> {
    let now = now_ms()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let revision: i64 = transaction.query_row(
        "SELECT revision FROM session WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let next_revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
    if transaction.execute(
        "UPDATE subagent_tasks SET state = ?1, terminal_revision = ?2, task_revision = ?3, details_json = ?4 WHERE id = ?5 AND terminal_revision IS NULL",
        params![
            terminal.state,
            next_revision,
            terminal.task_revision,
            terminal.details_json,
            terminal.task_id,
        ],
    )? != 1
    {
        return Err(SessionError::InvalidTransition);
    }
    transaction.execute(
        "UPDATE session SET updated_at_ms = ?1, revision = ?2 WHERE singleton = 1",
        params![now, next_revision],
    )?;
    transaction.commit()?;
    Ok(())
}

fn recover_interrupted_operations(connection: &mut Connection) -> Result<(), SessionError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let open_runs = {
        let mut statement =
            transaction.prepare("SELECT id FROM runs WHERE terminal_entry_id IS NULL")?;
        statement
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let run_id = match open_runs.as_slice() {
        [] => None,
        [run_id] => Some(*run_id),
        _ => return Err(SessionError::InvalidTransition),
    };
    let unfinished_tasks = {
        let mut statement = transaction.prepare(
            "SELECT id, task_revision, details_json FROM subagent_tasks WHERE terminal_revision IS NULL ORDER BY admitted_revision, id",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    if run_id.is_none() && unfinished_tasks.is_empty() {
        return Ok(());
    }
    let detection = now_ms()?;
    let (mut active_entry_id, checkpoint_id, mut revision): (i64, i64, i64) = transaction
        .query_row(
            "SELECT session.active_entry_id, entries.checkpoint_id, session.revision FROM session JOIN entries ON entries.id = session.active_entry_id WHERE session.singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    if let Some(run_id) = run_id {
        let terminal = PersistedTerminal::Failed {
            message: INTERRUPTION_MESSAGE.to_owned(),
            evidence: None,
            attempts: Vec::new(),
        };
        let stored_terminal = codec::encode_terminal(&terminal, None)
            .map_err(|error| SessionError::Codec(error.to_string()))?;
        revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
        transaction.execute(
            "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, provider_replay, created_at_ms, revision) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                active_entry_id,
                run_id,
                stored_terminal.kind,
                checkpoint_id,
                stored_terminal.payload_json,
                stored_terminal.provider_replay,
                detection,
                revision,
            ],
        )?;
        let entry_id = transaction.last_insert_rowid();
        let pending_steps: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM model_steps WHERE run_id = ?1 AND settled_entry_id IS NULL",
            params![run_id],
            |row| row.get(0),
        )?;
        if transaction.execute(
            "UPDATE model_steps SET settled_entry_id = ?1 WHERE run_id = ?2 AND settled_entry_id IS NULL",
            params![entry_id, run_id],
        )? != usize::try_from(pending_steps).map_err(|_| SessionError::InvalidTransition)?
        {
            return Err(SessionError::InvalidTransition);
        }
        if transaction.execute(
            "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2 AND terminal_entry_id IS NULL",
            params![entry_id, run_id],
        )? != 1
        {
            return Err(SessionError::InvalidTransition);
        }
        active_entry_id = entry_id;
    }
    for (task_id, task_revision, details_json) in unfinished_tasks {
        let details_json = codec::fail_unfinished_task_details(&details_json, detection)
            .map_err(|error| SessionError::Codec(error.to_string()))?;
        let next_task_revision = task_revision
            .checked_add(1)
            .ok_or(SessionError::IdOverflow)?;
        revision = revision.checked_add(1).ok_or(SessionError::IdOverflow)?;
        if transaction.execute(
            "UPDATE subagent_tasks SET state = 'failed', terminal_revision = ?1, task_revision = ?2, details_json = ?3 WHERE id = ?4 AND terminal_revision IS NULL AND task_revision = ?5 AND state IN ('starting', 'running', 'stopping')",
            params![revision, next_task_revision, details_json, task_id, task_revision],
        )? != 1
        {
            return Err(SessionError::InvalidTransition);
        }
    }
    if transaction.execute(
        "UPDATE session SET active_entry_id = ?1, updated_at_ms = ?2, revision = ?3 WHERE singleton = 1",
        params![active_entry_id, detection, revision],
    )? != 1
    {
        return Err(SessionError::InvalidTransition);
    }
    transaction.commit()?;
    Ok(())
}

fn task_sql_id(value: u64) -> Result<i64, SessionError> {
    i64::try_from(value).map_err(|_| SessionError::IdOverflow)
}

fn initialise(
    id: SessionId,
    paths: SessionPaths,
    writer_lock: File,
    cwd: &Path,
    selection: ModelSelection,
    checkpoint: Checkpoint,
    #[cfg(test)] write_gate: Option<SessionWriteGate>,
    #[cfg(not(test))] _write_gate: Option<()>,
) -> Result<Session, SessionError> {
    let mut connection = open_writable_database(&paths.database)?;
    let now = now_ms()?;
    let stored_checkpoint = codec::encode_checkpoint(&checkpoint)
        .map_err(|error| SessionError::Codec(error.to_string()))?;
    let preferences_json =
        codec::encode_session_preferences(&SessionPreferences::initial(selection))
            .map_err(|error| SessionError::Codec(error.to_string()))?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(include_str!("session/schema.sql"))?;
    transaction.execute(
        "INSERT INTO checkpoints (runtime_identity, arena_state, authorisations_json) VALUES (?1, ?2, ?3)",
        params![
            stored_checkpoint.runtime_identity,
            stored_checkpoint.arena_state,
            stored_checkpoint.authorisations_json,
        ],
    )?;
    let checkpoint_id = transaction.last_insert_rowid();
    transaction.execute(
        "INSERT INTO entries (kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES ('session_start', ?1, '{}', ?2, 1)",
        params![checkpoint_id, now],
    )?;
    let root_entry_id = transaction.last_insert_rowid();
    let cwd = codec::encode_path(cwd).map_err(|error| SessionError::Codec(error.to_string()))?;
    transaction.execute(
        "INSERT INTO session (singleton, id, created_at_ms, updated_at_ms, cwd, name, active_entry_id, preferences_json, revision) VALUES (1, ?1, ?2, ?2, ?3, NULL, ?4, ?5, 1)",
        params![id.as_str(), now, cwd, root_entry_id, preferences_json],
    )?;
    transaction.commit()?;
    Ok(Session {
        id,
        state: Arc::new(SessionState {
            connection: Mutex::new(connection),
            active_execution_step: Mutex::new(None),
            _writer_lock: writer_lock,
            #[cfg(test)]
            write_gate,
        }),
    })
}

fn sessions_directory(home: &Path) -> Result<PathBuf, SessionError> {
    let directory = home.join(".kiki/sessions");
    fs::create_dir_all(&directory)?;
    set_owner_only(&directory, 0o700)?;
    Ok(directory)
}

fn create_database(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let _file = options.open(path)?;
    set_owner_only(path, 0o600)
}

fn create_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_CLOEXEC);
    let file = options.open(path)?;
    set_owner_only(path, 0o600)?;
    fcntl_lock(&file, FlockOperation::NonBlockingLockExclusive).map_err(io::Error::from)?;
    Ok(file)
}

fn open_lock(path: &Path) -> Result<File, SessionError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600).custom_flags(libc::O_CLOEXEC);
    let file = options.open(path)?;
    set_owner_only(path, 0o600)?;
    match fcntl_lock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(error)
            if error == Errno::ACCESS || error == Errno::AGAIN || error == Errno::WOULDBLOCK =>
        {
            Err(SessionError::Owned)
        }
        Err(error) => Err(SessionError::Io(error.into())),
    }
}

fn set_owner_only(path: &Path, mode: u32) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn preflight_existing_database(path: &Path) -> Result<(), SessionError> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; SQLITE_HEADER_BYTES];
    file.read_exact(&mut header).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            SessionError::TruncatedHeader
        } else {
            SessionError::Io(error)
        }
    })?;
    if header[..SQLITE_SIGNATURE.len()] != SQLITE_SIGNATURE[..] {
        return Err(SessionError::InvalidSignature);
    }
    if header[18] != 1 || header[19] != 1 {
        return Err(SessionError::UnsupportedJournalMode);
    }
    let found = u32::from_be_bytes(header[60..64].try_into().expect("fixed header range"));
    if found != FORMAT_VERSION {
        return Err(SessionError::UnsupportedVersion { found });
    }
    Ok(())
}

fn existing_database(path: &Path) -> Result<(), SessionError> {
    let metadata = fs::metadata(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            SessionError::Missing
        } else {
            SessionError::Io(error)
        }
    })?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(SessionError::NotAFile)
    }
}

fn open_read_only_database(path: &Path) -> Result<Connection, SessionError> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(SessionError::from)
}

fn read_stored_cwd(connection: &Connection, id: &SessionId) -> Result<PathBuf, SessionError> {
    let (stored_id, cwd): (String, String) = connection.query_row(
        "SELECT id, cwd FROM session WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if stored_id != id.as_str() {
        return Err(SessionError::StoredIdMismatch);
    }
    codec::decode_path(&cwd).map_err(|error| SessionError::Codec(error.to_string()))
}

fn read_list_row(path: &Path, id: &SessionId) -> Result<SessionListRow, SessionError> {
    preflight_existing_database(path)?;
    let connection = open_read_only_database(path)?;
    let (stored_id, updated_at_ms, cwd, name): (String, i64, String, Option<String>) = connection
        .query_row(
        "SELECT id, updated_at_ms, cwd, name FROM session WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if stored_id != id.as_str() {
        return Err(SessionError::StoredIdMismatch);
    }
    let cwd = codec::decode_path(&cwd).map_err(|error| SessionError::Codec(error.to_string()))?;
    let first_request = connection
        .query_row(
            "SELECT payload_json FROM entries WHERE kind = 'user_request' ORDER BY revision, id LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|payload| codec::decode_request(&payload))
        .transpose()
        .map_err(|error| SessionError::Codec(error.to_string()))?;
    Ok(SessionListRow {
        id: id.as_str().to_owned(),
        updated_at_ms,
        cwd,
        name,
        first_request,
    })
}

fn open_writable_database(path: &Path) -> Result<Connection, SessionError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))?;
    if journal_mode != "delete" {
        return Err(SessionError::Configuration {
            setting: "DELETE rollback journalling",
        });
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    if synchronous != 2 {
        return Err(SessionError::Configuration {
            setting: "FULL synchronous writes",
        });
    }
    connection.pragma_update(None, "foreign_keys", "ON")?;
    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(SessionError::Configuration {
            setting: "foreign-key enforcement",
        });
    }
    Ok(connection)
}

#[cfg(test)]
fn load_initial_checkpoint(connection: &Connection) -> Result<Checkpoint, SessionError> {
    let (runtime_identity, arena_state, authorisations_json) = connection.query_row(
        "SELECT checkpoints.runtime_identity, checkpoints.arena_state, checkpoints.authorisations_json FROM entries JOIN checkpoints ON checkpoints.id = entries.checkpoint_id WHERE entries.kind = 'session_start'",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;
    codec::decode_checkpoint(runtime_identity, arena_state, &authorisations_json)
        .map_err(|error| SessionError::Codec(error.to_string()))
}

#[derive(Clone)]
struct RestoreEntryRow {
    id: i64,
    parent_id: Option<i64>,
    run_id: Option<i64>,
    checkpoint_id: i64,
    kind: String,
    payload_json: String,
    provider_replay: Option<Vec<u8>>,
    revision: i64,
}

struct RestoreRunRow {
    id: i64,
    terminal_entry_id: Option<i64>,
}

struct RestoreStepRow {
    id: i64,
    run_id: i64,
    settled_entry_id: Option<i64>,
}

struct RestoreTaskRow {
    id: i64,
    parent_task_id: Option<i64>,
    origin_step_id: i64,
    state: String,
    admitted_revision: i64,
    terminal_revision: Option<i64>,
    task_revision: i64,
    details_json: String,
}

struct RestoreSnapshot {
    active_entry_id: i64,
    session_revision: i64,
    metadata: Option<SessionMetadata>,
    entries: Vec<RestoreEntryRow>,
    runs: Vec<RestoreRunRow>,
    steps: Vec<RestoreStepRow>,
    tasks: Vec<RestoreTaskRow>,
    checkpoint: Option<(String, Vec<u8>, String)>,
}

fn load_session_metadata(connection: &Connection) -> Result<SessionMetadata, SessionError> {
    let (name, preferences_json): (Option<String>, String) = connection.query_row(
        "SELECT name, preferences_json FROM session WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    session_metadata(name, preferences_json)
}

fn session_metadata(
    name: Option<String>,
    preferences_json: String,
) -> Result<SessionMetadata, SessionError> {
    let preferences = codec::decode_session_preferences(&preferences_json)
        .map_err(|error| SessionError::Codec(error.to_string()))?;
    Ok(SessionMetadata::new(name, preferences))
}

fn load_session_restore(connection: &mut Connection) -> Result<SessionRestore, SessionError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
    let (active_entry_id, name, preferences_json, session_revision): (
        i64,
        Option<String>,
        String,
        i64,
    ) = transaction.query_row(
        "SELECT active_entry_id, name, preferences_json, revision FROM session WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let metadata = session_metadata(name, preferences_json).ok();
    let entries = {
        let mut statement = transaction.prepare(
            "SELECT id, parent_id, run_id, checkpoint_id, kind, payload_json, provider_replay, revision FROM entries ORDER BY id",
        )?;
        statement
            .query_map([], |row| {
                Ok(RestoreEntryRow {
                    id: row.get(0)?,
                    parent_id: row.get(1)?,
                    run_id: row.get(2)?,
                    checkpoint_id: row.get(3)?,
                    kind: row.get(4)?,
                    payload_json: row.get(5)?,
                    provider_replay: row.get(6)?,
                    revision: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let runs = {
        let mut statement =
            transaction.prepare("SELECT id, terminal_entry_id FROM runs ORDER BY id")?;
        statement
            .query_map([], |row| {
                Ok(RestoreRunRow {
                    id: row.get(0)?,
                    terminal_entry_id: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let steps = {
        let mut statement = transaction
            .prepare("SELECT id, run_id, settled_entry_id FROM model_steps ORDER BY id")?;
        statement
            .query_map([], |row| {
                Ok(RestoreStepRow {
                    id: row.get(0)?,
                    run_id: row.get(1)?,
                    settled_entry_id: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let tasks = {
        let mut statement = transaction.prepare(
            "SELECT id, parent_task_id, origin_step_id, state, admitted_revision, terminal_revision, task_revision, details_json FROM subagent_tasks ORDER BY id",
        )?;
        statement
            .query_map([], |row| {
                Ok(RestoreTaskRow {
                    id: row.get(0)?,
                    parent_task_id: row.get(1)?,
                    origin_step_id: row.get(2)?,
                    state: row.get(3)?,
                    admitted_revision: row.get(4)?,
                    terminal_revision: row.get(5)?,
                    task_revision: row.get(6)?,
                    details_json: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let checkpoint = transaction
        .query_row(
            "SELECT checkpoints.runtime_identity, checkpoints.arena_state, checkpoints.authorisations_json FROM entries JOIN checkpoints ON checkpoints.id = entries.checkpoint_id WHERE entries.id = ?1",
            params![active_entry_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    transaction.commit()?;
    Ok(restore_projection(RestoreSnapshot {
        active_entry_id,
        session_revision,
        metadata,
        entries,
        runs,
        steps,
        tasks,
        checkpoint,
    }))
}

fn restore_projection(snapshot: RestoreSnapshot) -> SessionRestore {
    let metadata = snapshot.metadata.clone();
    let next_task = next_task_after(
        snapshot
            .tasks
            .iter()
            .filter_map(|task| u64::try_from(task.id).ok())
            .max(),
    );
    let entries = snapshot
        .entries
        .iter()
        .map(|entry| (entry.id, entry.clone()))
        .collect::<BTreeMap<_, _>>();
    let runs = snapshot
        .runs
        .iter()
        .map(|run| (run.id, run.terminal_entry_id))
        .collect::<BTreeMap<_, _>>();
    let task_provenance = snapshot
        .tasks
        .iter()
        .map(|task| {
            (
                task.id,
                (
                    task.parent_task_id,
                    task.origin_step_id,
                    task.admitted_revision,
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let ancestry = selected_ancestry(&entries, snapshot.active_entry_id);
    if ancestry.entries.is_empty() {
        return SessionRestore {
            metadata,
            records: Vec::new(),
            tasks: Vec::new(),
            reserved_task_ids: Vec::new(),
            footer_adjustment: Vec::new(),
            next_task,
            executable: None,
        };
    }
    let mut executable = metadata.is_some() && ancestry.intact && valid_timeline(&snapshot);
    let mut verified = Vec::new();
    let mut records = Vec::new();
    for entry in ancestry.entries {
        if !entry_reuses_parent_checkpoint(&entry, verified.last()) {
            executable = false;
            break;
        }
        match readable_record(&entry, &snapshot.entries, &snapshot.steps, &runs) {
            Ok(Some(record)) => {
                verified.push(entry);
                records.push(record);
            }
            Ok(None) => verified.push(entry),
            Err(()) => {
                executable = false;
                break;
            }
        }
    }
    let selected_ids = verified
        .iter()
        .map(|entry| entry.id)
        .collect::<BTreeSet<_>>();

    let mut footer_adjustment = Vec::new();
    for entry in entries.values() {
        let attempts = match entry.kind.as_str() {
            "execution" => {
                codec::decode_readable_execution(&entry.payload_json).map(|(code, _)| code.attempts)
            }
            "final_reply" | "cancelled" | "failed" => {
                codec::decode_readable_terminal(&entry.payload_json)
                    .map(|(terminal, _)| terminal_attempts(&terminal).to_vec())
            }
            "session_start" | "user_request" => Ok(Vec::new()),
            _ => Err(codec::CodecError::InvalidPayload),
        };
        match attempts {
            Ok(attempts) if !selected_ids.contains(&entry.id) => footer_adjustment.extend(attempts),
            Ok(_) => {}
            Err(_) => executable = false,
        }
    }

    let mut tasks = Vec::new();
    let mut restored_task_ids = BTreeMap::new();
    let mut visible_task_ids = BTreeSet::new();
    for task in snapshot.tasks {
        let Ok(task_id) = u64::try_from(task.id) else {
            executable = false;
            continue;
        };
        let decoded = match codec::decode_task_record_details(&task.details_json) {
            Ok(decoded) => decoded,
            Err(_) => {
                executable = false;
                continue;
            }
        };
        let Some(state) = TaskState::from_storage_name(&task.state) else {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        };
        let (
            Ok(admitted_revision),
            Some(terminal_revision),
            Ok(record_revision),
            Ok(origin_step_id),
        ) = (
            u64::try_from(task.admitted_revision),
            task.terminal_revision
                .and_then(|revision| u64::try_from(revision).ok()),
            u64::try_from(task.task_revision),
            u64::try_from(task.origin_step_id),
        )
        else {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        };
        let origin_steps = snapshot
            .steps
            .iter()
            .filter(|step| step.id == task.origin_step_id)
            .collect::<Vec<_>>();
        let [origin_step] = origin_steps.as_slice() else {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        };
        let parent = match task.parent_task_id {
            Some(parent_id) if parent_id < task.id => restored_task_ids.get(&parent_id).cloned(),
            Some(_) => None,
            None => None,
        };
        let parent_is_causal = task.parent_task_id.is_none_or(|parent_id| {
            task_provenance.get(&parent_id).is_some_and(
                |(_, parent_origin_step_id, parent_admitted_revision)| {
                    *parent_origin_step_id == task.origin_step_id
                        && *parent_admitted_revision < task.admitted_revision
                },
            )
        });
        if task.parent_task_id.is_some() && (parent.is_none() || !parent_is_causal) {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        }
        let mut details = decoded.details;
        details.state = state;
        if !state.is_terminal()
            || details.terminal_at_ms.is_none()
            || details.outcome.is_none()
            || !task_outcome_matches_state(details.outcome.as_ref(), state)
        {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        }
        let stored_id = decoded
            .id
            .as_deref()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{task_id:02x}"));
        let Some(id) = TaskId::parse(&stored_id).filter(|id| {
            !restored_task_ids.values().any(|existing| existing == id)
                && task_id_matches_parent(id, parent.as_ref(), decoded.id.is_none())
        }) else {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        };
        let record = TaskRecord::from_persisted(RestoredTaskRecord {
            id: id.clone(),
            storage_id: task_id,
            revision: record_revision,
            parent: parent.clone(),
            parent_storage_id: task
                .parent_task_id
                .and_then(|parent| u64::try_from(parent).ok()),
            admission_history_step_id: decoded.admission_history_step_id,
            terminal_history_step_id: decoded.terminal_history_step_id,
            origin_step_id,
            depth: decoded.depth,
            parameters: decoded.parameters,
            attempts: decoded.attempts.clone(),
            details,
        });
        restored_task_ids.insert(task.id, id.clone());
        let Some(origin_entry) = origin_step
            .settled_entry_id
            .and_then(|entry_id| entries.get(&entry_id))
        else {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        };
        let Some(origin_revision) = u64::try_from(origin_entry.revision).ok() else {
            executable = false;
            footer_adjustment.extend(decoded.attempts);
            continue;
        };
        let origin_is_selected = selected_ids.contains(&origin_entry.id)
            && origin_entry.run_id == Some(origin_step.run_id);
        let parent_is_visible = parent
            .as_ref()
            .is_none_or(|parent| visible_task_ids.contains(parent));
        if origin_is_selected && parent_is_visible {
            let admission_anchor_revision = selected_step_revision(
                record.admission_history_step_id(),
                &snapshot.steps,
                &entries,
                &selected_ids,
            )
            .unwrap_or(origin_revision);
            let terminal_anchor_revision = selected_step_revision(
                record.terminal_history_step_id(),
                &snapshot.steps,
                &entries,
                &selected_ids,
            )
            .unwrap_or(origin_revision);
            visible_task_ids.insert(id);
            tasks.push(RestoredTask {
                record,
                admission_anchor_revision,
                terminal_anchor_revision,
                admitted_revision,
                terminal_revision,
            });
        } else {
            if origin_is_selected && parent.is_some() {
                executable = false;
            }
            footer_adjustment.extend(decoded.attempts);
        }
    }

    let reserved_task_ids = restored_task_ids.into_values().collect();
    let history = executable
        .then(|| restored_history(&verified, &snapshot.steps, &runs))
        .flatten();
    let checkpoint = snapshot
        .checkpoint
        .and_then(|(identity, state, authorisations)| {
            codec::decode_checkpoint(identity, state, &authorisations).ok()
        });
    let executable = history
        .zip(checkpoint)
        .map(|(history, checkpoint)| RestoredExecutableState {
            checkpoint,
            history,
        });
    SessionRestore {
        metadata,
        records,
        tasks,
        reserved_task_ids,
        footer_adjustment,
        next_task,
        executable,
    }
}

fn next_task_after(maximum: Option<u64>) -> Option<u64> {
    maximum.map_or(Some(0), |maximum| maximum.checked_add(1))
}

fn selected_step_revision(
    step_id: Option<u64>,
    steps: &[RestoreStepRow],
    entries: &BTreeMap<i64, RestoreEntryRow>,
    selected_ids: &BTreeSet<i64>,
) -> Option<u64> {
    let step_id = step_id.and_then(|step_id| i64::try_from(step_id).ok())?;
    let mut matching = steps.iter().filter(|step| step.id == step_id);
    let step = matching.next()?;
    if matching.next().is_some() {
        return None;
    }
    let entry = entries.get(&step.settled_entry_id?)?;
    if selected_ids.contains(&entry.id) && entry.run_id == Some(step.run_id) {
        u64::try_from(entry.revision).ok()
    } else {
        None
    }
}

fn task_id_matches_parent(id: &TaskId, parent: Option<&TaskId>, legacy: bool) -> bool {
    if legacy {
        return true;
    }
    match parent {
        None => !id.as_str().contains('.'),
        Some(parent) => id
            .as_str()
            .rsplit_once('.')
            .is_some_and(|(prefix, _)| prefix == parent.as_str()),
    }
}

struct SelectedAncestry {
    entries: Vec<RestoreEntryRow>,
    intact: bool,
}

fn selected_ancestry(
    entries: &BTreeMap<i64, RestoreEntryRow>,
    active_entry_id: i64,
) -> SelectedAncestry {
    let mut selected = Vec::new();
    let mut current = active_entry_id;
    let mut seen = BTreeSet::new();
    while seen.insert(current) {
        let Some(entry) = entries.get(&current).cloned() else {
            return SelectedAncestry {
                entries: Vec::new(),
                intact: false,
            };
        };
        let parent = entry.parent_id;
        selected.push(entry);
        let Some(parent) = parent else {
            break;
        };
        current = parent;
    }
    if seen.contains(&current)
        && selected
            .last()
            .is_some_and(|entry| entry.parent_id.is_some())
    {
        return SelectedAncestry {
            entries: Vec::new(),
            intact: false,
        };
    }
    let Some(root) = selected.last() else {
        return SelectedAncestry {
            entries: Vec::new(),
            intact: false,
        };
    };
    if root.kind != "session_start" || root.run_id.is_some() || root.parent_id.is_some() {
        return SelectedAncestry {
            entries: Vec::new(),
            intact: false,
        };
    }
    selected.reverse();
    let first_invalid = selected.windows(2).position(|pair| {
        let [parent, child] = pair else {
            unreachable!("fixed windows have two entries")
        };
        parent.id >= child.id || parent.revision >= child.revision
    });
    let intact = first_invalid.is_none();
    if let Some(index) = first_invalid {
        selected.truncate(index + 1);
    }
    SelectedAncestry {
        entries: selected,
        intact,
    }
}

fn entry_reuses_parent_checkpoint(
    entry: &RestoreEntryRow,
    parent: Option<&RestoreEntryRow>,
) -> bool {
    match parent {
        None => entry.kind == "session_start",
        Some(parent) => entry.kind == "execution" || entry.checkpoint_id == parent.checkpoint_id,
    }
}

fn valid_timeline(snapshot: &RestoreSnapshot) -> bool {
    if snapshot.session_revision <= 0 || !valid_snapshot_ids(snapshot) {
        return false;
    }
    let mut revisions = BTreeSet::new();
    let mut unique_revision = |revision: i64| {
        revision > 0 && revision <= snapshot.session_revision && revisions.insert(revision)
    };
    snapshot
        .entries
        .iter()
        .all(|entry| unique_revision(entry.revision))
        && snapshot.tasks.iter().all(|task| {
            let Some(terminal_revision) = task.terminal_revision else {
                return false;
            };
            unique_revision(task.admitted_revision)
                && terminal_revision > task.admitted_revision
                && unique_revision(terminal_revision)
        })
        && valid_entry_ancestry(snapshot)
        && valid_run_membership(snapshot)
}

fn valid_snapshot_ids(snapshot: &RestoreSnapshot) -> bool {
    snapshot.active_entry_id > 0
        && snapshot
            .entries
            .iter()
            .all(|entry| entry.id > 0 && entry.checkpoint_id > 0)
        && snapshot.runs.iter().all(|run| run.id > 0)
        && snapshot
            .steps
            .iter()
            .all(|step| step.id > 0 && step.run_id > 0)
        && snapshot.tasks.iter().all(|task| {
            task.id >= 0
                && task.origin_step_id > 0
                && task.parent_task_id.is_none_or(|parent_id| parent_id >= 0)
        })
}

fn valid_entry_ancestry(snapshot: &RestoreSnapshot) -> bool {
    let entries = snapshot
        .entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<BTreeMap<_, _>>();
    snapshot.entries.iter().all(|entry| match entry.parent_id {
        None => entry.kind == "session_start" && entry.run_id.is_none(),
        Some(parent_id) => {
            entry.kind != "session_start"
                && entry.run_id.is_some()
                && entries.get(&parent_id).is_some_and(|parent| {
                    parent.id < entry.id
                        && parent.revision < entry.revision
                        && (entry.kind == "execution"
                            || entry.checkpoint_id == parent.checkpoint_id)
                })
        }
    })
}

fn valid_run_membership(snapshot: &RestoreSnapshot) -> bool {
    let run_ids = snapshot
        .runs
        .iter()
        .map(|run| run.id)
        .collect::<BTreeSet<_>>();
    let entries = snapshot
        .entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<BTreeMap<_, _>>();
    snapshot.entries.iter().all(|entry| {
        entry.kind == "session_start"
            || entry.run_id.is_some_and(|run_id| run_ids.contains(&run_id))
    }) && snapshot.runs.iter().all(|run| {
        let terminal_is_valid = run.terminal_entry_id.is_none_or(|entry_id| {
            entries.get(&entry_id).is_some_and(|entry| {
                entry.run_id == Some(run.id)
                    && matches!(entry.kind.as_str(), "final_reply" | "cancelled" | "failed")
            })
        });
        terminal_is_valid
            && snapshot
                .entries
                .iter()
                .filter(|entry| entry.run_id == Some(run.id) && entry.kind == "user_request")
                .count()
                == 1
    }) && snapshot.steps.iter().all(|step| {
        run_ids.contains(&step.run_id)
            && step.settled_entry_id.is_some_and(|entry_id| {
                entries
                    .get(&entry_id)
                    .is_some_and(|entry| entry.run_id == Some(step.run_id))
            })
    })
}

fn readable_record(
    entry: &RestoreEntryRow,
    entries: &[RestoreEntryRow],
    steps: &[RestoreStepRow],
    runs: &BTreeMap<i64, Option<i64>>,
) -> Result<Option<RestoredRecord>, ()> {
    if entry.kind == "session_start" {
        return (entry.run_id.is_none()
            && entry.parent_id.is_none()
            && entry.provider_replay.is_none()
            && entry.payload_json == "{}")
            .then_some(None)
            .ok_or(());
    }
    let run_id = entry
        .run_id
        .and_then(|id| u64::try_from(id).ok())
        .ok_or(())?;
    let revision = u64::try_from(entry.revision).map_err(|_| ())?;
    if entry.parent_id.is_none()
        || !runs.contains_key(&entry.run_id.unwrap_or_default())
        || !run_has_one_request(entries, entry.run_id.unwrap_or_default())
    {
        return Err(());
    }
    let kind = match entry.kind.as_str() {
        "user_request" => {
            if entry.provider_replay.is_some() {
                return Err(());
            }
            RestoredRecordKind::UserRequest {
                request: codec::decode_request(&entry.payload_json).map_err(|_| ())?,
            }
        }
        "execution" => {
            let step_id = settled_step_id(entry, steps)?;
            let (code, execution) =
                codec::decode_readable_execution(&entry.payload_json).map_err(|_| ())?;
            if !matches!(code.result, crate::model::CodeResult::Execution { .. }) {
                return Err(());
            }
            RestoredRecordKind::CompletedExecution {
                step_id,
                source: code.source,
                commentary: code.commentary,
                reasoning_summary: code.reasoning_summary,
                execution,
                attempts: code.attempts,
            }
        }
        "final_reply" => {
            let step_id = settled_step_id(entry, steps)?;
            let (terminal, code) =
                codec::decode_readable_terminal(&entry.payload_json).map_err(|_| ())?;
            let ReadableTerminal::Final {
                text,
                reasoning_summary,
                ..
            } = &terminal
            else {
                return Err(());
            };
            if code.is_some()
                || runs.get(&entry.run_id.unwrap_or_default()) != Some(&Some(entry.id))
            {
                return Err(());
            }
            RestoredRecordKind::FinalMessage {
                step_id,
                text: text.clone(),
                reasoning_summary: reasoning_summary.clone(),
                terminal,
            }
        }
        "cancelled" => {
            let (terminal, code) =
                codec::decode_readable_terminal(&entry.payload_json).map_err(|_| ())?;
            if !matches!(terminal, ReadableTerminal::Cancelled { .. })
                || runs.get(&entry.run_id.unwrap_or_default()) != Some(&Some(entry.id))
                || code.as_ref().is_some_and(|code| {
                    !matches!(
                        code.result,
                        crate::model::CodeResult::Cancelled { .. }
                            | crate::model::CodeResult::Failure { .. }
                    )
                })
            {
                return Err(());
            }
            if code.is_some() {
                settled_step_id(entry, steps)?;
            }
            RestoredRecordKind::CancelledRun { terminal }
        }
        "failed" => {
            let (terminal, code) =
                codec::decode_readable_terminal(&entry.payload_json).map_err(|_| ())?;
            let ReadableTerminal::Failed { message, .. } = &terminal else {
                return Err(());
            };
            if runs.get(&entry.run_id.unwrap_or_default()) != Some(&Some(entry.id))
                || code.as_ref().is_some_and(|code| {
                    !matches!(code.result, crate::model::CodeResult::Failure { .. })
                })
            {
                return Err(());
            }
            if code.is_some() {
                settled_step_id(entry, steps)?;
            }
            RestoredRecordKind::FailedRun {
                message: message.clone(),
                terminal,
            }
        }
        _ => return Err(()),
    };
    Ok(Some(RestoredRecord {
        revision,
        run_id,
        kind,
    }))
}

fn run_has_one_request(entries: &[RestoreEntryRow], run_id: i64) -> bool {
    entries
        .iter()
        .filter(|entry| entry.run_id == Some(run_id) && entry.kind == "user_request")
        .count()
        == 1
}

fn settled_step_id(entry: &RestoreEntryRow, steps: &[RestoreStepRow]) -> Result<u64, ()> {
    let matches = steps
        .iter()
        .filter(|step| {
            step.run_id == entry.run_id.unwrap_or_default()
                && step.settled_entry_id == Some(entry.id)
        })
        .collect::<Vec<_>>();
    let [step] = matches.as_slice() else {
        return Err(());
    };
    u64::try_from(step.id).map_err(|_| ())
}

fn restored_history(
    selected: &[RestoreEntryRow],
    steps: &[RestoreStepRow],
    runs: &BTreeMap<i64, Option<i64>>,
) -> Option<ModelHistory> {
    let mut history = ModelHistory::new();
    for entry in selected {
        match entry.kind.as_str() {
            "session_start" => {
                if readable_record(entry, selected, steps, runs).is_err() {
                    return None;
                }
            }
            "user_request" => {
                let request = codec::decode_request(&entry.payload_json).ok()?;
                if entry.provider_replay.is_some() {
                    return None;
                }
                history.append_user_request(request);
            }
            "execution" => {
                settled_step_id(entry, steps).ok()?;
                let code = codec::decode_execution_entry(
                    &entry.payload_json,
                    entry.provider_replay.as_deref(),
                )
                .ok()?;
                if code.execution.is_none()
                    || !matches!(code.result, crate::model::CodeResult::Execution { .. })
                    || append_history_code(&mut history, code).is_err()
                {
                    return None;
                }
            }
            "final_reply" | "cancelled" | "failed" => {
                let (terminal, code) = codec::decode_terminal_entry(
                    &entry.payload_json,
                    entry.provider_replay.as_deref(),
                )
                .ok()?;
                match (entry.kind.as_str(), terminal, code) {
                    (
                        "final_reply",
                        PersistedTerminal::Final {
                            text,
                            reasoning_summary,
                            replay,
                            ..
                        },
                        None,
                    ) => {
                        settled_step_id(entry, steps).ok()?;
                        history.append_final_message(text, reasoning_summary, replay);
                    }
                    ("cancelled", PersistedTerminal::Cancelled { .. }, Some(code))
                        if code.execution.is_none()
                            && matches!(
                                code.result,
                                crate::model::CodeResult::Cancelled { .. }
                                    | crate::model::CodeResult::Failure { .. }
                            ) =>
                    {
                        settled_step_id(entry, steps).ok()?;
                        append_history_code(&mut history, code).ok()?;
                    }
                    ("failed", PersistedTerminal::Failed { .. }, Some(code))
                        if code.execution.is_none()
                            && matches!(code.result, crate::model::CodeResult::Failure { .. }) =>
                    {
                        settled_step_id(entry, steps).ok()?;
                        append_history_code(&mut history, code).ok()?;
                    }
                    ("cancelled", PersistedTerminal::Cancelled { .. }, None)
                    | ("failed", PersistedTerminal::Failed { .. }, None) => {}
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(history)
}

fn append_history_code(history: &mut ModelHistory, code: PersistedCode) -> Result<(), ()> {
    history
        .append_completed_code(
            code.source,
            code.call_id,
            code.commentary,
            code.reasoning_summary,
            code.replay,
            code.result,
        )
        .map_err(|_| ())
}

fn terminal_attempts(terminal: &ReadableTerminal) -> &[AttemptAccounting] {
    match terminal {
        ReadableTerminal::Final { attempts, .. }
        | ReadableTerminal::Cancelled { attempts, .. }
        | ReadableTerminal::Failed { attempts, .. } => attempts,
    }
}

fn task_outcome_matches_state(
    outcome: Option<&crate::subagents::TaskOutcome>,
    state: TaskState,
) -> bool {
    matches!(
        (outcome, state),
        (
            Some(crate::subagents::TaskOutcome::Completed(_)),
            TaskState::Completed
        ) | (
            Some(crate::subagents::TaskOutcome::Failed(_)),
            TaskState::Failed
        ) | (
            Some(crate::subagents::TaskOutcome::Cancelled),
            TaskState::Cancelled
        ) | (
            Some(crate::subagents::TaskOutcome::TimedOut),
            TaskState::TimedOut
        )
    )
}

fn now_ms() -> Result<i64, SessionError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| SessionError::Io(io::Error::other(error)))?;
    i64::try_from(duration.as_millis()).map_err(|_| {
        SessionError::Io(io::Error::other(
            "current time does not fit in milliseconds",
        ))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;
    use std::time::Duration;

    use rusqlite::{Connection, params};
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    #[cfg(unix)]
    use std::os::unix::net::{UnixListener, UnixStream};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{
        PersistedCode, PersistedTerminal, Session, SessionError, SessionId, TaskAdmission,
        TaskSnapshot, TaskStepCommit, TaskTerminal,
    };
    use crate::arena::{Arena, Checkpoint, ExecutionOutcome, RuntimeIdentity};
    use crate::config::{DEFAULT_MODEL, ModelSelection, SessionPreferences, models::TERRA};
    use crate::subagents::{ChildResult, TaskOutcome, TaskPersistedDetails, TaskRecord, TaskState};
    use crate::tools::{ToolHost, ToolValue, WholeFileAuthorisations};

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

    fn selection() -> ModelSelection {
        ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select default model")
    }

    async fn checkpoint(workspace: &TempDir) -> Checkpoint {
        arena(workspace).await.checkpoint()
    }

    async fn create_session(home: &TempDir, workspace: &TempDir) -> Session {
        Session::create(
            home.path(),
            workspace.path(),
            selection(),
            checkpoint(workspace).await,
        )
        .expect("create session")
    }

    fn database_path(home: &Path, id: &str) -> PathBuf {
        home.join(".kiki/sessions").join(format!("{id}.sqlite3"))
    }

    fn lock_path(home: &Path, id: &str) -> PathBuf {
        home.join(".kiki/sessions").join(format!("{id}.lock"))
    }

    fn empty_observation() -> crate::arena::Observation {
        crate::arena::Observation {
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

    fn insert_entry(
        connection: &Connection,
        parent_id: i64,
        run_id: i64,
        kind: &str,
        revision: i64,
    ) -> i64 {
        connection
            .execute(
                "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES (?1, ?2, ?3, 1, '{}', 1, ?4)",
                params![parent_id, run_id, kind, revision],
            )
            .expect("insert entry");
        connection.last_insert_rowid()
    }

    fn insert_task(
        connection: &Connection,
        parent_task_id: Option<i64>,
        origin_step_id: i64,
    ) -> i64 {
        connection
            .execute(
                "INSERT INTO subagent_tasks (parent_task_id, origin_step_id, state, admitted_revision, terminal_revision, task_revision, details_json) VALUES (?1, ?2, 'starting', 2, NULL, 1, '{}')",
                params![parent_task_id, origin_step_id],
            )
            .expect("insert subagent task");
        connection.last_insert_rowid()
    }

    fn valid_checkpoint_tuple() -> Option<(String, Vec<u8>, String)> {
        let checkpoint = Checkpoint::from_session_parts(
            RuntimeIdentity::from_session_value("test-runtime".to_owned()),
            Vec::new(),
            WholeFileAuthorisations::default(),
        );
        let checkpoint = super::codec::encode_checkpoint(&checkpoint).expect("encode checkpoint");
        Some((
            checkpoint.runtime_identity,
            checkpoint.arena_state,
            checkpoint.authorisations_json,
        ))
    }

    fn restore_entry(
        id: i64,
        parent_id: Option<i64>,
        run_id: Option<i64>,
        payload_json: String,
        revision: i64,
    ) -> super::RestoreEntryRow {
        super::RestoreEntryRow {
            id,
            parent_id,
            run_id,
            checkpoint_id: 1,
            kind: if parent_id.is_none() {
                "session_start".to_owned()
            } else {
                "user_request".to_owned()
            },
            payload_json,
            provider_replay: None,
            revision,
        }
    }

    #[test]
    fn task_allocation_starts_at_zero_and_exhausts_only_at_u64_max() {
        assert_eq!(super::next_task_after(None), Some(0));
        assert_eq!(super::next_task_after(Some(u64::MAX - 1)), Some(u64::MAX));
        assert_eq!(super::next_task_after(Some(u64::MAX)), None);
    }

    #[test]
    fn restore_truncates_selected_history_at_the_first_unreadable_entry() {
        let request = |text| super::codec::encode_request(text).expect("encode request");
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 4,
            session_revision: 4,
            metadata: None,
            entries: vec![
                restore_entry(1, None, None, "{}".to_owned(), 1),
                restore_entry(2, Some(1), Some(1), request("first"), 2),
                restore_entry(3, Some(2), Some(2), "{}".to_owned(), 3),
                restore_entry(4, Some(3), Some(3), request("must be hidden"), 4),
            ],
            runs: vec![
                super::RestoreRunRow {
                    id: 1,
                    terminal_entry_id: None,
                },
                super::RestoreRunRow {
                    id: 2,
                    terminal_entry_id: None,
                },
                super::RestoreRunRow {
                    id: 3,
                    terminal_entry_id: None,
                },
            ],
            steps: Vec::new(),
            tasks: Vec::new(),
            checkpoint: valid_checkpoint_tuple(),
        });

        assert!(
            restore.executable.is_none(),
            "the selected entry is unreadable"
        );
        assert!(matches!(
            restore.records.as_slice(),
            [super::RestoredRecord {
                kind: super::RestoredRecordKind::UserRequest { request },
                ..
            }] if request == "first"
        ));
    }

    #[test]
    fn restore_rejects_selected_entry_with_a_different_parent_checkpoint() {
        let request = super::codec::encode_request("invalid checkpoint").expect("encode request");
        let mut child = restore_entry(2, Some(1), Some(1), request, 2);
        child.checkpoint_id = 2;
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 2,
            session_revision: 2,
            metadata: None,
            entries: vec![restore_entry(1, None, None, "{}".to_owned(), 1), child],
            runs: vec![super::RestoreRunRow {
                id: 1,
                terminal_entry_id: None,
            }],
            steps: Vec::new(),
            tasks: Vec::new(),
            checkpoint: valid_checkpoint_tuple(),
        });

        assert!(
            restore.executable.is_none(),
            "the selected checkpoint changes without execution"
        );
        assert!(restore.records.is_empty());
    }

    #[test]
    fn restore_rejects_selected_parent_with_a_non_older_revision() {
        let request = |text| super::codec::encode_request(text).expect("encode request");
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 3,
            session_revision: 3,
            metadata: None,
            entries: vec![
                restore_entry(1, None, None, "{}".to_owned(), 1),
                restore_entry(2, Some(1), Some(1), request("invalid"), 1),
                restore_entry(3, Some(2), Some(2), request("must be hidden"), 3),
            ],
            runs: vec![
                super::RestoreRunRow {
                    id: 1,
                    terminal_entry_id: None,
                },
                super::RestoreRunRow {
                    id: 2,
                    terminal_entry_id: None,
                },
            ],
            steps: Vec::new(),
            tasks: Vec::new(),
            checkpoint: valid_checkpoint_tuple(),
        });

        assert!(
            restore.executable.is_none(),
            "the selected parent revision is not older"
        );
        assert!(restore.records.is_empty());
    }

    #[test]
    fn restore_disables_duplicate_global_timeline_revisions_without_hiding_valid_history() {
        let request = |text| super::codec::encode_request(text).expect("encode request");
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 2,
            session_revision: 2,
            metadata: None,
            entries: vec![
                restore_entry(1, None, None, "{}".to_owned(), 1),
                restore_entry(2, Some(1), Some(1), request("selected"), 2),
                restore_entry(3, Some(1), Some(2), request("sibling"), 2),
            ],
            runs: vec![
                super::RestoreRunRow {
                    id: 1,
                    terminal_entry_id: None,
                },
                super::RestoreRunRow {
                    id: 2,
                    terminal_entry_id: None,
                },
            ],
            steps: Vec::new(),
            tasks: Vec::new(),
            checkpoint: valid_checkpoint_tuple(),
        });

        assert!(
            restore.executable.is_none(),
            "duplicate revisions disable execution"
        );
        assert!(matches!(
            restore.records.as_slice(),
            [super::RestoredRecord {
                kind: super::RestoredRecordKind::UserRequest { request },
                ..
            }] if request == "selected"
        ));
    }

    #[test]
    fn restore_disables_multiple_requests_in_a_run() {
        let request = |text| super::codec::encode_request(text).expect("encode request");
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 2,
            session_revision: 3,
            metadata: None,
            entries: vec![
                restore_entry(1, None, None, "{}".to_owned(), 1),
                restore_entry(2, Some(1), Some(1), request("selected"), 2),
                restore_entry(3, Some(1), Some(1), request("duplicate"), 3),
            ],
            runs: vec![super::RestoreRunRow {
                id: 1,
                terminal_entry_id: None,
            }],
            steps: Vec::new(),
            tasks: Vec::new(),
            checkpoint: valid_checkpoint_tuple(),
        });

        assert!(
            restore.executable.is_none(),
            "multiple requests in one run disable execution"
        );
    }

    #[test]
    fn missing_active_entry_preserves_task_identifier_exhaustion_boundary() {
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 999,
            session_revision: 1,
            metadata: None,
            entries: Vec::new(),
            runs: Vec::new(),
            steps: Vec::new(),
            tasks: vec![super::RestoreTaskRow {
                id: i64::MAX,
                parent_task_id: None,
                origin_step_id: 1,
                state: "completed".to_owned(),
                admitted_revision: 1,
                terminal_revision: Some(2),
                task_revision: 1,
                details_json: "{}".to_owned(),
            }],
            checkpoint: None,
        });

        assert_eq!(restore.next_task, Some(i64::MAX as u64 + 1));
        assert!(
            restore.records.is_empty(),
            "the active entry is intentionally missing"
        );
        assert!(
            restore.executable.is_none(),
            "a missing active entry has no checkpoint"
        );
    }

    #[test]
    fn restore_keeps_late_visible_task_after_selected_terminal() {
        let request = super::codec::encode_request("selected request").expect("encode request");
        let terminal = super::codec::encode_terminal(
            &PersistedTerminal::Final {
                step_id: 1,
                text: "selected final".to_owned(),
                reasoning_summary: String::new(),
                replay: crate::model::ProviderReplay::new("test", Vec::new()),
                attempts: Vec::new(),
            },
            None,
        )
        .expect("encode terminal");
        let mut final_entry = restore_entry(3, Some(2), Some(1), terminal.payload_json, 3);
        final_entry.kind = terminal.kind.to_owned();
        final_entry.provider_replay = terminal.provider_replay;
        let task = TaskRecord::for_test("00", 2, selection(), None, 1, Vec::new())
            .with_persisted_details_for_test(TaskPersistedDetails {
                admitted_at_ms: 4,
                terminal_at_ms: Some(5),
                state: TaskState::Completed,
                step_count: 1,
                elapsed: Duration::from_secs(2),
                observed_at_ms: 5,
                attempt_evidence: Vec::new(),
                executions: Vec::new(),
                outcome: Some(TaskOutcome::Completed(ChildResult::new(
                    "late selected result".to_owned(),
                    BTreeMap::from([("value".to_owned(), ToolValue::Integer(7))]),
                ))),
            });
        let mut task_details: serde_json::Value = serde_json::from_str(
            &super::codec::encode_task_details(&task).expect("encode late task"),
        )
        .expect("decode task details JSON");
        let task_details = task_details.as_object_mut().expect("task details object");
        task_details.remove("id");
        task_details.remove("admission_history_step_id");
        task_details.remove("terminal_history_step_id");
        let restore = super::restore_projection(super::RestoreSnapshot {
            active_entry_id: 3,
            session_revision: 5,
            metadata: Some(super::SessionMetadata::new(
                None,
                crate::config::SessionPreferences::initial(selection()),
            )),
            entries: vec![
                restore_entry(1, None, None, "{}".to_owned(), 1),
                restore_entry(2, Some(1), Some(1), request, 2),
                final_entry,
            ],
            runs: vec![super::RestoreRunRow {
                id: 1,
                terminal_entry_id: Some(3),
            }],
            steps: vec![super::RestoreStepRow {
                id: 1,
                run_id: 1,
                settled_entry_id: Some(3),
            }],
            tasks: vec![super::RestoreTaskRow {
                id: 0,
                parent_task_id: None,
                origin_step_id: 1,
                state: "completed".to_owned(),
                admitted_revision: 4,
                terminal_revision: Some(5),
                task_revision: 2,
                details_json: serde_json::to_string(&task_details).expect("encode legacy details"),
            }],
            checkpoint: valid_checkpoint_tuple(),
        });

        assert!(restore.executable.is_some());
        assert!(matches!(
            restore.tasks.as_slice(),
            [task] if task.record.id().as_str() == "00"
                && matches!(task.record.outcome(), Some(TaskOutcome::Completed(result))
                    if result.message() == "late selected result"
                        && result.values().get("value") == Some(&ToolValue::Integer(7)))
        ));
    }

    #[tokio::test]
    async fn reopen_restores_initial_checkpoint() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        fs::write(workspace.path().join("file"), "contents").expect("write workspace file");
        let mut original = arena(&workspace).await;
        let cancellation = CancellationToken::new();
        original
            .execute("value = 41\nawait read('file')", &cancellation)
            .await
            .expect("build initial checkpoint");
        let initial = original.checkpoint();
        let selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("select default model");

        let session = Session::create(home.path(), workspace.path(), selection, initial.clone())
            .expect("create session");
        let id = session.id().to_owned();
        session.close().expect("close created session");

        let session = Session::open(home.path(), &id).expect("reopen session");
        let restored = session
            .initial_checkpoint()
            .expect("load initial checkpoint");
        assert_eq!(restored, initial);

        let mut fresh = arena(&workspace).await;
        fresh.restore(restored).await.expect("restore checkpoint");
        let execution = fresh
            .execute("value", &CancellationToken::new())
            .await
            .expect("run restored arena");
        let ExecutionOutcome::Complete {
            result: Some(result),
        } = execution.outcome
        else {
            panic!("restored state did not produce a result");
        };
        assert_eq!(result.head, "41");
        fresh
            .execute(
                "await write('file', 'replacement')",
                &CancellationToken::new(),
            )
            .await
            .expect("use restored authorisation");
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "replacement"
        );
        session.close().expect("close reopened session");
    }

    #[tokio::test]
    async fn restore_starts_child_ids_at_zero_without_stored_tasks() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;

        assert_eq!(
            session.restore().expect("restore session").next_task,
            Some(0)
        );

        session.close().expect("close session");
    }

    #[test]
    fn generated_ids_are_exact_and_distinct() {
        let ids = (0..32)
            .map(|_| SessionId::generate().expect("generate session ID").0)
            .collect::<HashSet<_>>();

        assert_eq!(ids.len(), 32);
        assert!(ids.iter().all(|id| {
            id.len() == 32
                && id.bytes().all(|byte| {
                    byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte <= b'f')
                })
        }));
    }

    #[test]
    fn malformed_ids_are_rejected_before_session_lookup() {
        let home = tempfile::tempdir().expect("create home");

        for id in [
            "",
            "a",
            "A0123456789012345678901234567890",
            "../012345678901234567890123456789",
            "0123456789012345678901234567890g",
        ] {
            assert!(matches!(
                Session::open(home.path(), id),
                Err(SessionError::InvalidId)
            ));
        }

        assert!(!home.path().join(".kiki").exists());
    }

    #[tokio::test]
    async fn session_id_collision_keeps_the_existing_database() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let checkpoint = checkpoint(&workspace).await;
        let source = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned();
        let collided = "11111111111111111111111111111111".to_owned();
        let replacement = "22222222222222222222222222222222".to_owned();
        let first = Session::create_with_ids_for_test(
            home.path(),
            workspace.path(),
            selection(),
            checkpoint.clone(),
            [source.clone()],
        )
        .expect("create source session");
        first.close().expect("close source session");
        let database = database_path(home.path(), &collided);
        fs::copy(database_path(home.path(), &source), &database)
            .expect("create colliding database");
        let original = fs::read(&database).expect("read colliding database");

        let replacement_session = Session::create_with_ids_for_test(
            home.path(),
            workspace.path(),
            selection(),
            checkpoint,
            [collided.clone(), replacement.clone()],
        )
        .expect("retry after collision");

        assert_eq!(replacement_session.id(), replacement);
        assert_eq!(
            fs::read(database).expect("reread colliding database"),
            original
        );
        assert!(lock_path(home.path(), &collided).exists());
        replacement_session
            .close()
            .expect("close replacement session");
    }

    #[tokio::test]
    async fn stored_id_must_match_the_requested_session_id() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        session.close().expect("close session");
        let requested_id = "ffffffffffffffffffffffffffffffff";
        fs::copy(
            database_path(home.path(), &id),
            database_path(home.path(), requested_id),
        )
        .expect("copy database under another session ID");

        assert!(matches!(
            Session::open(home.path(), requested_id),
            Err(SessionError::StoredIdMismatch)
        ));
    }

    #[tokio::test]
    async fn initial_session_metadata_is_complete_and_secret_free() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let (id, created_at_ms, updated_at_ms, cwd, name, active_entry_id, revision, preferences): (
            String,
            i64,
            i64,
            String,
            Option<String>,
            i64,
            i64,
            String,
        ) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT id, created_at_ms, updated_at_ms, cwd, name, active_entry_id, revision, preferences_json FROM session",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .expect("read session metadata");
        let preference_value: serde_json::Value =
            serde_json::from_str(&preferences).expect("decode preferences");
        let active_kind: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT kind FROM entries WHERE id = ?1",
                params![active_entry_id],
                |row| row.get(0),
            )
            .expect("read active entry");

        assert_eq!(id, session.id());
        assert_eq!(created_at_ms, updated_at_ms);
        assert!(created_at_ms > 0);
        assert_eq!(super::codec::decode_path(&cwd).unwrap(), workspace.path());
        assert_eq!(name, None);
        assert_eq!(active_kind, "session_start");
        assert_eq!(revision, 1);
        assert_eq!(
            preference_value,
            serde_json::json!({
                "selected_model": DEFAULT_MODEL.id,
                "thinking": { DEFAULT_MODEL.id: DEFAULT_MODEL.default_thinking },
            })
        );
        assert!(!preferences.contains("sk-configured-credential"));
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn metadata_commits_preserve_user_and_project_configuration() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let user_settings = home.path().join(".kiki/config.toml");
        let project_settings = workspace.path().join(".kiki/config.toml");
        fs::create_dir_all(user_settings.parent().expect("user settings parent"))
            .expect("create user settings directory");
        fs::create_dir_all(project_settings.parent().expect("project settings parent"))
            .expect("create project settings directory");
        fs::write(
            &user_settings,
            "model = \"openai/gpt-5.6-sol\"\n[thinking]\n\"openai/gpt-5.6-sol\" = \"low\"\n",
        )
        .expect("write user settings");
        fs::write(
            &project_settings,
            "model = \"openai/gpt-5.6-terra\"\n[thinking]\n\"openai/gpt-5.6-terra\" = \"high\"\n",
        )
        .expect("write project settings");
        let user_before = fs::read(&user_settings).expect("read user settings");
        let project_before = fs::read(&project_settings).expect("read project settings");
        let session = create_session(&home, &workspace).await;
        let preferences = SessionPreferences::new(
            ModelSelection::new(&TERRA, "high").expect("select Terra"),
            BTreeMap::from([(DEFAULT_MODEL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid preferences");

        session
            .writer()
            .commit_name("Saved session".to_owned())
            .await
            .expect("commit name");
        session
            .writer()
            .commit_preferences(preferences.clone())
            .await
            .expect("commit preferences");
        let metadata = session.metadata().expect("load metadata");
        let restore = session.restore().expect("restore session");

        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row("SELECT revision FROM session", [], |row| row
                    .get::<_, i64>(0))
                .expect("read session revision"),
            3
        );
        assert_eq!(metadata.name(), Some("Saved session"));
        assert_eq!(metadata.preferences(), &preferences);
        assert_eq!(restore.metadata.as_ref(), Some(&metadata));
        assert_eq!(
            fs::read(&user_settings).expect("read user settings"),
            user_before
        );
        assert_eq!(
            fs::read(&project_settings).expect("read project settings"),
            project_before
        );
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn startup_preferences_and_relocation_share_one_transaction() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let relocated = tempfile::tempdir().expect("create relocated workspace");
        let session = create_session(&home, &workspace).await;
        let preferences = SessionPreferences::new(
            ModelSelection::new(&TERRA, "high").expect("select Terra"),
            BTreeMap::from([(DEFAULT_MODEL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid preferences");
        let before: (String, String, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, cwd, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata before relocation");

        session
            .writer()
            .commit_startup_preferences(preferences.clone(), Some(relocated.path().to_path_buf()))
            .await
            .expect("commit relocation");
        let after: (String, String, i64) = session
            .connection_for_test()
            .expect("lock session")
            .query_row(
                "SELECT preferences_json, cwd, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read metadata after relocation");

        assert_ne!(after.0, before.0);
        assert_eq!(
            super::codec::decode_path(&after.1).unwrap(),
            relocated.path()
        );
        assert_eq!(after.2, before.2 + 1);
        assert_eq!(
            super::codec::decode_session_preferences(&after.0).unwrap(),
            preferences
        );
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn read_cwd_ignores_corrupt_preferences_and_writer_ownership() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        session
            .connection_for_test()
            .expect("lock session")
            .execute(
                "UPDATE session SET preferences_json = '{}', revision = revision + 1",
                [],
            )
            .expect("corrupt preferences");

        assert_eq!(
            Session::read_cwd(home.path(), &id).unwrap(),
            workspace.path()
        );
        assert!(matches!(session.metadata(), Err(SessionError::Codec(_))));
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn corrupt_preferences_disable_restore_but_keep_readable_history() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        session
            .writer()
            .commit_request("readable request".to_owned())
            .await
            .expect("commit request");
        session
            .connection_for_test()
            .expect("lock session")
            .execute(
                "UPDATE session SET preferences_json = '{\"selected_model\":\"openai/gpt-5.6-sol\",\"thinking\":{\"openai/gpt-5.6-sol\":\"medium\"},\"unexpected\":true}', revision = revision + 1",
                [],
            )
            .expect("corrupt preferences");

        assert!(matches!(session.metadata(), Err(SessionError::Codec(_))));
        let restore = session.restore().expect("read restore");
        assert!(restore.metadata.is_none());
        assert!(restore.executable.is_none());
        assert!(matches!(
            restore.records.as_slice(),
            [super::RestoredRecord {
                kind: super::RestoredRecordKind::UserRequest { request },
                ..
            }] if request == "readable request"
        ));
        session.close().expect("close session");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn session_cwd_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let cwd = PathBuf::from(OsString::from_vec(b"/workspace/\xff".to_vec()));
        let session = Session::create(home.path(), &cwd, selection(), checkpoint(&workspace).await)
            .expect("create session");
        let stored_cwd: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row("SELECT cwd FROM session", [], |row| row.get(0))
            .expect("read stored cwd");

        assert_eq!(super::codec::decode_path(&stored_cwd).unwrap(), cwd);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn schema_enforces_the_session_tree_and_lifecycle_guards() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let connection = session
            .connection_for_test()
            .expect("lock session connection");
        let tables = {
            let mut statement = connection
                .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
                .expect("prepare table query");
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query tables")
                .collect::<Result<Vec<_>, _>>()
                .expect("read table names")
        };

        assert_eq!(
            tables,
            [
                "checkpoints",
                "entries",
                "model_steps",
                "runs",
                "session",
                "subagent_tasks",
            ]
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "foreign_keys", |row| row.get(0))
                .expect("read foreign key setting"),
            1
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .expect("read storage version"),
            1
        );
        assert!(
            connection
                .execute("UPDATE session SET cwd = 'changed'", [])
                .is_err()
        );
        assert!(
            connection
                .execute("UPDATE session SET preferences_json = '{}'", [])
                .is_err()
        );
        assert!(
            connection
                .execute("UPDATE session SET active_entry_id = 1", [])
                .is_err()
        );
        assert!(
            connection
                .execute("UPDATE session SET revision = 1", [])
                .is_err()
        );
        connection
            .execute("UPDATE session SET revision = 2", [])
            .expect("advance session revision");
        assert!(
            connection
                .execute("UPDATE session SET revision = 1", [])
                .is_err()
        );
        assert!(
            connection
                .execute(
                    "UPDATE session SET id = 'ffffffffffffffffffffffffffffffff', revision = 3",
                    [],
                )
                .is_err()
        );
        assert!(
            connection
                .execute("UPDATE session SET active_entry_id = 999", [])
                .is_err()
        );
        assert!(connection
            .execute(
                "INSERT INTO session (singleton, id, created_at_ms, updated_at_ms, cwd, name, active_entry_id, preferences_json, revision) SELECT 1, 'duplicate', created_at_ms, updated_at_ms, cwd, name, active_entry_id, preferences_json, revision FROM session",
                [],
            )
            .is_err());
        assert!(connection
            .execute(
                "INSERT INTO entries (kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES ('session_start', 1, '{}', 1, 2)",
                [],
            )
            .is_err());
        assert!(
            connection
                .execute(
                    "UPDATE entries SET payload_json = '{\"changed\":true}' WHERE id = 1",
                    []
                )
                .is_err()
        );
        assert!(
            connection
                .execute(
                    "UPDATE checkpoints SET arena_state = x'00' WHERE id = 1",
                    []
                )
                .is_err()
        );

        connection
            .execute("INSERT INTO runs DEFAULT VALUES", [])
            .expect("create first run");
        let first_run = connection.last_insert_rowid();
        assert!(
            connection
                .execute("INSERT INTO runs DEFAULT VALUES", [])
                .is_err()
        );
        assert!(connection
            .execute(
                "INSERT INTO entries (id, parent_id, run_id, kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES (99, 99, ?1, 'user_request', 1, '{}', 1, 2)",
                params![first_run],
            )
            .is_err());
        let first_request = insert_entry(&connection, 1, first_run, "user_request", 2);
        assert!(
            connection
                .execute(
                    "UPDATE session SET active_entry_id = ?1",
                    params![first_request],
                )
                .is_err()
        );
        assert!(connection
            .execute(
                "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES (?1, ?2, 'user_request', 1, '{}', 1, 2)",
                params![first_request, first_run],
            )
            .is_err());
        assert!(connection
            .execute(
                "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES (?1, ?2, 'user_request', 1, '{}', 1, 1)",
                params![first_request, first_run],
            )
            .is_err());
        connection
            .execute(
                "INSERT INTO model_steps (run_id) VALUES (?1)",
                params![first_run],
            )
            .expect("create first model step");
        let first_step = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO model_steps (run_id) VALUES (?1)",
                params![first_run],
            )
            .expect("create another first-run model step");
        let another_first_step = connection.last_insert_rowid();
        let first_terminal = insert_entry(&connection, first_request, first_run, "final_reply", 3);
        assert!(
            connection
                .execute(
                    "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2",
                    params![first_terminal, first_run],
                )
                .is_err()
        );
        connection
            .execute(
                "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2",
                params![first_terminal, first_step],
            )
            .expect("settle first model step");
        assert!(
            connection
                .execute(
                    "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2",
                    params![first_terminal, first_step],
                )
                .is_err()
        );
        connection
            .execute(
                "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2",
                params![first_terminal, another_first_step],
            )
            .expect("settle another first-run model step");
        connection
            .execute(
                "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2",
                params![first_terminal, first_run],
            )
            .expect("settle first run");
        assert!(
            connection
                .execute(
                    "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2",
                    params![first_terminal, first_run],
                )
                .is_err()
        );
        assert!(
            connection
                .execute(
                    "INSERT INTO model_steps (run_id) VALUES (?1)",
                    params![first_run],
                )
                .is_err()
        );

        connection
            .execute("INSERT INTO runs DEFAULT VALUES", [])
            .expect("create second run");
        let second_run = connection.last_insert_rowid();
        let second_request = insert_entry(&connection, 1, second_run, "user_request", 4);
        connection
            .execute(
                "INSERT INTO model_steps (run_id) VALUES (?1)",
                params![second_run],
            )
            .expect("create second model step");
        let second_step = connection.last_insert_rowid();
        let second_terminal =
            insert_entry(&connection, second_request, second_run, "final_reply", 5);
        assert!(
            connection
                .execute(
                    "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2",
                    params![first_terminal, second_step],
                )
                .is_err()
        );
        connection
            .execute(
                "UPDATE model_steps SET settled_entry_id = ?1 WHERE id = ?2",
                params![second_terminal, second_step],
            )
            .expect("settle second model step");
        assert!(
            connection
                .execute(
                    "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2",
                    params![first_terminal, second_run],
                )
                .is_err()
        );
        connection
            .execute(
                "UPDATE runs SET terminal_entry_id = ?1 WHERE id = ?2",
                params![second_terminal, second_run],
            )
            .expect("settle second run");

        assert!(connection
            .execute(
                "INSERT INTO subagent_tasks (id, parent_task_id, origin_step_id, state, admitted_revision, terminal_revision, task_revision, details_json) VALUES (99, 99, ?1, 'starting', 2, NULL, 1, '{}')",
                params![first_step],
            )
            .is_err());
        let first_task = insert_task(&connection, None, first_step);
        assert!(connection
            .execute(
                "INSERT INTO subagent_tasks (parent_task_id, origin_step_id, state, admitted_revision, terminal_revision, task_revision, details_json) VALUES (?1, ?2, 'starting', 2, NULL, 1, '{}')",
                params![first_task, another_first_step],
            )
            .is_err());
        connection
            .execute(
                "UPDATE subagent_tasks SET state = 'running', task_revision = 2 WHERE id = ?1",
                params![first_task],
            )
            .expect("advance first task revision");
        assert!(
            connection
                .execute(
                    "UPDATE subagent_tasks SET task_revision = 2 WHERE id = ?1",
                    params![first_task],
                )
                .is_err()
        );
        assert!(
            connection
                .execute(
                    "UPDATE subagent_tasks SET task_revision = 1 WHERE id = ?1",
                    params![first_task],
                )
                .is_err()
        );
        connection
            .execute(
                "UPDATE subagent_tasks SET state = 'completed', terminal_revision = 3, task_revision = 3 WHERE id = ?1",
                params![first_task],
            )
            .expect("settle first task");
        assert!(
            connection
                .execute(
                    "UPDATE subagent_tasks SET terminal_revision = 4 WHERE id = ?1",
                    params![first_task],
                )
                .is_err()
        );
        assert!(connection
            .execute(
                "UPDATE subagent_tasks SET state = 'completed', terminal_revision = NULL, task_revision = 4 WHERE id = ?1",
                params![first_task],
            )
            .is_err());
        assert!(
            connection
                .execute(
                    "UPDATE subagent_tasks SET state = 'failed', task_revision = 4 WHERE id = ?1",
                    params![first_task],
                )
                .is_err()
        );
        assert!(connection
            .execute(
                "UPDATE subagent_tasks SET details_json = '{\"changed\":true}', task_revision = 4 WHERE id = ?1",
                params![first_task],
            )
            .is_err());
        assert!(
            connection
                .execute(
                    "UPDATE subagent_tasks SET task_revision = 4 WHERE id = ?1",
                    params![first_task],
                )
                .is_err()
        );
        assert!(connection
            .execute(
                "INSERT INTO subagent_tasks (parent_task_id, origin_step_id, state, admitted_revision, terminal_revision, task_revision, details_json) VALUES (?1, ?2, 'starting', 2, NULL, 1, '{}')",
                params![first_task, second_step],
            )
            .is_err());
        drop(connection);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn writable_connections_use_delete_full_and_foreign_keys() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;

        let journal_mode: String = session
            .connection_for_test()
            .expect("lock session connection")
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("read journal mode");
        let synchronous: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("read synchronous mode");
        let foreign_keys: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .expect("read foreign key setting");

        assert_eq!(journal_mode, "delete");
        assert_eq!(synchronous, 2);
        assert_eq!(foreign_keys, 1);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn read_only_checkpoint_access_does_not_take_the_writer_lock() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let restored = Session::read_initial_checkpoint(home.path(), session.id())
            .expect("read checkpoint while writer is idle");

        assert_eq!(restored, session.initial_checkpoint().unwrap());
        session.close().expect("close session");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn session_files_are_owner_only_including_an_open_journal() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let database = database_path(home.path(), session.id());
        let lock = lock_path(home.path(), session.id());
        let journal = database.with_file_name(format!("{}.sqlite3-journal", session.id()));

        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO runs DEFAULT VALUES;")
            .expect("begin write transaction");

        for (path, expected_mode) in [
            (home.path().join(".kiki/sessions"), 0o700),
            (database, 0o600),
            (lock, 0o600),
            (journal, 0o600),
        ] {
            assert!(path.exists(), "{} does not exist", path.display());
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                expected_mode
            );
        }

        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch("ROLLBACK")
            .expect("rollback write transaction");
        session.close().expect("close session");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn created_session_files_remain_reopenable_after_restrictive_umask() {
        if let (Some(home), Some(workspace)) = (
            env::var_os("KIKI_SESSION_TEST_UMASK_HOME"),
            env::var_os("KIKI_SESSION_TEST_UMASK_WORKSPACE"),
        ) {
            let home = PathBuf::from(home);
            let workspace = PathBuf::from(workspace);
            let kiki = home.join(".kiki");
            fs::create_dir(&kiki).expect("create Kiki directory");
            fs::set_permissions(&kiki, fs::Permissions::from_mode(0o700))
                .expect("restrict Kiki directory");
            unsafe {
                libc::umask(0o777);
            }
            let checkpoint = Arena::new(
                worker_path(),
                ToolHost::new(&workspace).await.expect("open tool host"),
            )
            .await
            .expect("create arena")
            .checkpoint();
            let session = Session::create(&home, &workspace, selection(), checkpoint)
                .expect("create session under restrictive umask");
            session.close().expect("close session");
            return;
        }

        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let status = Command::new(env::current_exe().expect("locate test executable"))
            .arg("session::tests::created_session_files_remain_reopenable_after_restrictive_umask")
            .arg("--exact")
            .env("KIKI_SESSION_TEST_UMASK_HOME", home.path())
            .env("KIKI_SESSION_TEST_UMASK_WORKSPACE", workspace.path())
            .status()
            .expect("run restrictive-umask child");
        assert!(status.success());

        let database = fs::read_dir(home.path().join(".kiki/sessions"))
            .expect("read sessions directory")
            .map(|entry| entry.expect("read session entry").path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "sqlite3")
            })
            .expect("find session database");
        let id = database
            .file_stem()
            .and_then(|value| value.to_str())
            .expect("session database has a UTF-8 ID");
        let lock = lock_path(home.path(), id);
        for path in [&database, &lock] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        Session::open(home.path(), id)
            .expect("reopen session created under restrictive umask")
            .close()
            .expect("close reopened session");
    }

    #[test]
    fn invalid_existing_headers_leave_database_and_journal_unchanged() {
        let home = tempfile::tempdir().expect("create home");
        let directory = home.path().join(".kiki/sessions");
        fs::create_dir_all(&directory).expect("create sessions directory");
        let mut valid_header = [0_u8; 100];
        valid_header[..16].copy_from_slice(b"SQLite format 3\0");
        valid_header[18] = 1;
        valid_header[19] = 1;

        let cases = [
            ("33333333333333333333333333333333", vec![0; 99]),
            ("44444444444444444444444444444444", vec![0; 100]),
            ("55555555555555555555555555555555", {
                let mut header = valid_header;
                header[60..64].copy_from_slice(&2_u32.to_be_bytes());
                header.to_vec()
            }),
            ("66666666666666666666666666666666", {
                let mut header = valid_header;
                header[18] = 2;
                header.to_vec()
            }),
        ];

        for (id, contents) in cases {
            let database = database_path(home.path(), id);
            let journal = database.with_file_name(format!("{id}.sqlite3-journal"));
            fs::write(&database, &contents).expect("write invalid database");
            fs::write(&journal, b"journal bytes").expect("write journal");

            assert!(Session::read_initial_checkpoint(home.path(), id).is_err());
            assert_eq!(fs::read(&database).unwrap(), contents);
            assert_eq!(fs::read(&journal).unwrap(), b"journal bytes");
            assert!(Session::open(home.path(), id).is_err());
            assert_eq!(fs::read(&database).unwrap(), contents);
            assert_eq!(fs::read(&journal).unwrap(), b"journal bytes");
        }
    }

    #[cfg(unix)]
    fn persisted_execution(source: &str, execution: crate::arena::Execution) -> PersistedCode {
        PersistedCode {
            source: source.to_owned(),
            call_id: crate::model::ModelCallId::new("committed-step"),
            commentary: None,
            reasoning_summary: "commit execution checkpoint".to_owned(),
            selection: None,
            replay: crate::model::ProviderReplay::new("test", Vec::new()),
            result: crate::model::CodeResult::Execution {
                feedback: String::new(),
            },
            execution: Some(execution),
            attempts: Vec::new(),
        }
    }

    #[cfg(unix)]
    async fn restored_arena(session: &Session, workspace: &Path) -> Arena {
        let mut arena = Arena::new(
            worker_path(),
            ToolHost::new(workspace).await.expect("open tool host"),
        )
        .await
        .expect("create arena");
        arena
            .restore(
                session
                    .initial_checkpoint()
                    .expect("load initial checkpoint"),
            )
            .await
            .expect("restore initial checkpoint");
        arena
    }

    #[cfg(unix)]
    fn signal_and_block(stream: &mut UnixStream, signal: u8) {
        stream.write_all(&[signal]).expect("signal parent");
        let mut release = [0];
        stream.read_exact(&mut release).expect("wait for parent");
    }

    #[test]
    #[cfg(unix)]
    fn recovery_process() {
        let Some(endpoint) = env::var_os("KIKI_SESSION_TEST_SOCKET").map(PathBuf::from) else {
            return;
        };
        let home = env::var_os("KIKI_SESSION_TEST_HOME")
            .map(PathBuf::from)
            .expect("recovery process requires a home path");
        let id = env::var("KIKI_SESSION_TEST_ID").expect("recovery process requires a session ID");
        let milestone = env::var("KIKI_SESSION_RECOVERY_MILESTONE")
            .expect("recovery process requires a milestone");
        let workspace = env::var_os("KIKI_SESSION_TEST_WORKSPACE")
            .map(PathBuf::from)
            .expect("recovery process requires a workspace path");
        let mut stream = UnixStream::connect(endpoint).expect("connect to test parent");
        tokio::runtime::Runtime::new()
            .expect("create test runtime")
            .block_on(async {
                let session = Session::open(&home, &id).expect("open session in recovery process");
                let writer = session.writer();
                let signal =
                    match milestone.as_str() {
                        "generation" => {
                            let run_id = writer
                                .commit_request("generation interruption".to_owned())
                                .await
                                .expect("commit request");
                            writer
                                .start_model_step(run_id)
                                .await
                                .expect("start generation step");
                            b'G'
                        }
                        "execution" => {
                            let run_id = writer
                                .commit_request("execution interruption".to_owned())
                                .await
                                .expect("commit request");
                            writer
                                .start_model_step(run_id)
                                .await
                                .expect("start execution step");
                            let mut arena = restored_arena(&session, &workspace).await;
                            let cancellation = CancellationToken::new();
                            let execution = arena
                                .execute(
                                    "uncommitted_value = 1\nresult_values = {'uncommitted': 1}",
                                    &cancellation,
                                )
                                .await
                                .expect("execute uncommitted Python");
                            assert!(matches!(
                                execution.outcome,
                                ExecutionOutcome::Complete { result: None }
                            ));
                            b'E'
                        }
                        "committed-step" => {
                            let run_id = writer
                                .commit_request("committed step interruption".to_owned())
                                .await
                                .expect("commit request");
                            let settled_step = writer
                                .start_model_step(run_id)
                                .await
                                .expect("start settled step");
                            let mut arena = restored_arena(&session, &workspace).await;
                            let cancellation = CancellationToken::new();
                            let source = "committed_value = 1\nresult_values = {'committed': 1}";
                            let execution = arena
                                .execute(source, &cancellation)
                                .await
                                .expect("execute committed Python");
                            assert!(matches!(
                                &execution.outcome,
                                ExecutionOutcome::Complete { result: None }
                            ));
                            writer
                                .settle_execution(
                                    run_id,
                                    settled_step,
                                    persisted_execution(source, execution),
                                    arena.checkpoint(),
                                )
                                .await
                                .expect("commit settled step");
                            writer
                                .start_model_step(run_id)
                                .await
                                .expect("start pending step");
                            let execution = arena
                                .execute(
                                    "uncommitted_value = 2\nresult_values = {'uncommitted': 2}",
                                    &cancellation,
                                )
                                .await
                                .expect("execute uncommitted Python");
                            assert!(matches!(
                                execution.outcome,
                                ExecutionOutcome::Complete { result: None }
                            ));
                            b'C'
                        }
                        "background-only" => {
                            let run_id = writer
                                .commit_request("closed foreground".to_owned())
                                .await
                                .expect("commit request");
                            let origin_step_id = writer
                                .start_model_step(run_id)
                                .await
                                .expect("start origin step");
                            writer
                                .close_run(
                                    run_id,
                                    Some(origin_step_id),
                                    || PersistedTerminal::Failed {
                                        message: "closed before interruption".to_owned(),
                                        evidence: None,
                                        attempts: Vec::new(),
                                    },
                                    None,
                                )
                                .await
                                .expect("close foreground");
                            let task = crate::subagents::TaskRecord::for_test(
                                "00",
                                1,
                                selection(),
                                None,
                                1,
                                Vec::new(),
                            )
                            .with_persisted_details_for_test(TaskPersistedDetails {
                                admitted_at_ms: 100,
                                terminal_at_ms: None,
                                state: crate::subagents::TaskState::Running,
                                step_count: 1,
                                elapsed: Duration::new(2, 3),
                                observed_at_ms: 200,
                                attempt_evidence: Vec::new(),
                                executions: Vec::new(),
                                outcome: None,
                            });
                            assert!(
                                writer
                                    .admit_subagent_task(
                                        TaskAdmission::from_record(&task, origin_step_id)
                                            .expect("encode background task"),
                                        || false,
                                    )
                                    .await
                                    .expect("admit background task"),
                                "commit background task"
                            );
                            b'B'
                        }
                        _ => panic!("unknown recovery milestone"),
                    };
                signal_and_block(&mut stream, signal);
            });
    }

    #[test]
    #[cfg(unix)]
    fn hot_rollback_process() {
        let Some(endpoint) = env::var_os("KIKI_SESSION_TEST_SOCKET").map(PathBuf::from) else {
            return;
        };
        let home = env::var_os("KIKI_SESSION_TEST_HOME")
            .map(PathBuf::from)
            .expect("rollback process requires a home path");
        let id = env::var("KIKI_SESSION_TEST_ID").expect("rollback process requires a session ID");
        let mut stream = UnixStream::connect(endpoint).expect("connect to test parent");
        let session = Session::open(&home, &id).expect("open session in rollback process");
        let connection = session
            .connection_for_test()
            .expect("lock rollback process connection");
        connection
            .execute_batch("PRAGMA cache_size = 10; PRAGMA cache_spill = ON; BEGIN IMMEDIATE;")
            .expect("begin spilling rollback transaction");
        let revision: i64 = connection
            .query_row("SELECT revision FROM session", [], |row| row.get(0))
            .expect("read session revision");
        connection
            .execute(
                "UPDATE session SET updated_at_ms = ?1, revision = ?2 WHERE singleton = 1",
                params![super::now_ms().expect("read time"), revision + 1],
            )
            .expect("dirty session page");
        for page in 0_u8..32 {
            connection
                .execute(
                    "INSERT INTO checkpoints (runtime_identity, arena_state, authorisations_json) VALUES (?1, ?2, '[]')",
                    params![
                        format!("rollback page {page}"),
                        vec![page; 12 * 1024],
                    ],
                )
                .expect("dirty checkpoint page");
        }
        let database = database_path(&home, &id);
        let journal = database.with_file_name(format!("{id}.sqlite3-journal"));
        let journal_bytes = fs::read(&journal).expect("read hot journal");
        assert!(
            journal_bytes.starts_with(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]),
            "rollback journal header is missing"
        );
        assert!(journal_bytes.len() > 512, "rollback journal did not spill");
        assert!(
            journal_bytes.len() < 1_000_000,
            "rollback fixture is unbounded"
        );
        signal_and_block(&mut stream, b'J');
    }

    #[cfg(unix)]
    fn recovery_process_result(
        home: &Path,
        id: &str,
        workspace: &Path,
        socket: &Path,
        milestone: &str,
    ) -> (std::process::Child, UnixStream, u8) {
        let listener = UnixListener::bind(socket).expect("bind parent socket");
        let child = Command::new(env::current_exe().expect("locate test executable"))
            .arg("session::tests::recovery_process")
            .arg("--exact")
            .arg("--nocapture")
            .env("KIKI_SESSION_TEST_SOCKET", socket)
            .env("KIKI_SESSION_TEST_HOME", home)
            .env("KIKI_SESSION_TEST_ID", id)
            .env("KIKI_SESSION_TEST_WORKSPACE", workspace)
            .env("KIKI_SESSION_RECOVERY_MILESTONE", milestone)
            .spawn()
            .expect("start recovery process");
        let (mut stream, _) = listener.accept().expect("accept recovery process");
        let mut signal = [0];
        stream
            .read_exact(&mut signal)
            .expect("read recovery milestone");
        (child, stream, signal[0])
    }

    #[test]
    #[cfg(unix)]
    fn writer_process() {
        let Some(endpoint) = env::var_os("KIKI_SESSION_TEST_SOCKET").map(PathBuf::from) else {
            return;
        };
        let home = env::var_os("KIKI_SESSION_TEST_HOME")
            .map(PathBuf::from)
            .expect("writer process requires a home path");
        let id = env::var("KIKI_SESSION_TEST_ID").expect("writer process requires a session ID");
        let mut stream = UnixStream::connect(endpoint).expect("connect to test parent");

        let reader = Session::read_initial_checkpoint(&home, &id);
        stream
            .write_all(&[if reader.is_ok() { b'R' } else { b'r' }])
            .expect("report reader result");
        let writer = Session::open(&home, &id);
        stream
            .write_all(&[if matches!(writer, Err(SessionError::Owned)) {
                b'C'
            } else {
                b'O'
            }])
            .expect("report writer result");
        if env::var_os("KIKI_SESSION_TEST_BLOCK").is_some() {
            let mut release = [0];
            stream.read_exact(&mut release).expect("wait for parent");
        }
        if let Ok(writer) = writer {
            writer.close().expect("close child writer");
        }
    }

    #[cfg(unix)]
    fn writer_process_result(home: &Path, id: &str, socket: &Path) -> [u8; 2] {
        let listener = UnixListener::bind(socket).expect("bind parent socket");
        let mut child = Command::new(env::current_exe().expect("locate test executable"))
            .arg("session::tests::writer_process")
            .arg("--exact")
            .arg("--nocapture")
            .env("KIKI_SESSION_TEST_SOCKET", socket)
            .env("KIKI_SESSION_TEST_HOME", home)
            .env("KIKI_SESSION_TEST_ID", id)
            .spawn()
            .expect("start writer process");
        let (mut stream, _) = listener.accept().expect("accept child connection");
        let mut result = [0; 2];
        stream.read_exact(&mut result).expect("read child result");
        assert!(child.wait().expect("wait for child").success());
        result
    }

    #[tokio::test]
    async fn recovery_closes_pending_run_and_step() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let writer = session.writer();
        let run_id = writer
            .commit_request("interrupted request".to_owned())
            .await
            .expect("commit request");
        let step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start pending step");
        let run_id = i64::try_from(run_id).expect("run ID fits SQLite");
        let step_id = i64::try_from(step_id).expect("step ID fits SQLite");
        let (previous_active, previous_checkpoint, previous_revision): (i64, i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT session.active_entry_id, entries.checkpoint_id, session.revision FROM session JOIN entries ON entries.id = session.active_entry_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read preceding active entry");
        drop(writer);
        session.close().expect("close interrupted session");

        let recovered = Session::open(home.path(), &id).expect("open recovered session");
        let connection = recovered
            .connection_for_test()
            .expect("lock recovered session connection");
        let (entries, checkpoints, pending_steps, closed_runs, revision): (i64, i64, i64, i64, i64) =
            connection
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), revision FROM session",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .expect("read recovered lifecycle");
        assert_eq!(
            (entries, checkpoints, pending_steps, closed_runs, revision),
            (3, 1, 0, 1, previous_revision + 1)
        );
        let (
            entry_id,
            parent_id,
            entry_run_id,
            checkpoint_id,
            payload_json,
            replay,
            created_at_ms,
            entry_revision,
        ): (i64, i64, i64, i64, String, Option<Vec<u8>>, i64, i64) = connection
            .query_row(
                "SELECT entries.id, entries.parent_id, entries.run_id, entries.checkpoint_id, entries.payload_json, entries.provider_replay, entries.created_at_ms, entries.revision FROM runs JOIN entries ON entries.id = runs.terminal_entry_id WHERE runs.id = ?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
            )
            .expect("read interruption entry");
        assert_eq!(parent_id, previous_active);
        assert_eq!(entry_run_id, run_id);
        assert_eq!(checkpoint_id, previous_checkpoint);
        assert_eq!(entry_revision, previous_revision + 1);
        assert_eq!(
            connection
                .query_row("SELECT updated_at_ms FROM session", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read recovery timestamp"),
            created_at_ms
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload_json).expect("decode interruption"),
            serde_json::json!({
                "Terminal": {
                    "terminal": {
                        "Failed": {
                            "message": "Interrupted by previous process exit",
                            "evidence": null,
                            "attempts": [],
                        }
                    },
                    "code": null,
                }
            })
        );
        assert_eq!(replay, None);
        assert_eq!(
            connection
                .query_row(
                    "SELECT settled_entry_id FROM model_steps WHERE id = ?1 AND run_id = ?2",
                    params![step_id, run_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .expect("read recovered step"),
            Some(entry_id)
        );
        assert_eq!(
            connection
                .query_row("SELECT active_entry_id FROM session", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read active entry"),
            entry_id
        );
        drop(connection);
        recovered.close().expect("close recovered session");
    }

    #[tokio::test]
    async fn recovery_closes_pending_background_tasks_in_admission_order() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let writer = session.writer();
        let run_id = writer
            .commit_request("closed foreground".to_owned())
            .await
            .expect("commit request");
        let origin_step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start origin step");
        writer
            .close_run(
                run_id,
                Some(origin_step_id),
                || PersistedTerminal::Failed {
                    message: "already closed".to_owned(),
                    evidence: None,
                    attempts: Vec::new(),
                },
                None,
            )
            .await
            .expect("close foreground");
        let first = crate::subagents::TaskRecord::for_test(
            "00",
            7,
            selection(),
            None,
            1,
            vec![crate::model::AttemptAccounting::UsageUnavailable],
        )
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 100,
            terminal_at_ms: None,
            state: crate::subagents::TaskState::Running,
            step_count: 3,
            elapsed: Duration::new(4, 5),
            observed_at_ms: 200,
            attempt_evidence: vec![crate::subagents::TaskAttempt::for_test(
                2,
                1,
                Some(crate::model::AttemptAccounting::UsageUnavailable),
            )],
            executions: vec![crate::subagents::TaskExecution::for_test(
                2,
                "value = 1".to_owned(),
                "reasoning".to_owned(),
                crate::arena::Execution {
                    stdout: empty_observation(),
                    stderr: empty_observation(),
                    outcome: crate::arena::ExecutionOutcome::Complete { result: None },
                    tool_calls: Vec::new(),
                },
                vec![crate::model::AttemptAccounting::UsageUnavailable],
            )],
            outcome: None,
        });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&first, origin_step_id).expect("encode first task"),
                    || false,
                )
                .await
                .expect("admit first task"),
            "commit first task"
        );
        let second = crate::subagents::TaskRecord::for_test(
            "01",
            9,
            selection(),
            Some("00"),
            2,
            vec![
                crate::model::AttemptAccounting::UsageUnavailable,
                crate::model::AttemptAccounting::UsageUnavailable,
            ],
        )
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 101,
            terminal_at_ms: None,
            state: crate::subagents::TaskState::Stopping,
            step_count: 4,
            elapsed: Duration::new(5, 6),
            observed_at_ms: 201,
            attempt_evidence: Vec::new(),
            executions: Vec::new(),
            outcome: None,
        });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&second, origin_step_id)
                        .expect("encode second task"),
                    || false,
                )
                .await
                .expect("admit second task"),
            "commit second task"
        );
        let completed_admission =
            crate::subagents::TaskRecord::for_test("02", 1, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 102,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Starting,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    observed_at_ms: 102,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&completed_admission, origin_step_id)
                        .expect("encode completed task admission"),
                    || false,
                )
                .await
                .expect("admit completed task"),
            "commit completed task"
        );
        let completed =
            crate::subagents::TaskRecord::for_test("02", 2, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 102,
                    terminal_at_ms: Some(202),
                    state: crate::subagents::TaskState::Completed,
                    step_count: 1,
                    elapsed: Duration::new(6, 7),
                    observed_at_ms: 202,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: Some(crate::subagents::TaskOutcome::Completed(
                        crate::subagents::ChildResult::new("done".to_owned(), BTreeMap::new()),
                    )),
                });
        writer
            .terminal_subagent_task(TaskTerminal::from_record(&completed).expect("encode result"))
            .await
            .expect("commit completed task");
        let (previous_active, entries, checkpoints, previous_revision): (i64, i64, i64, i64) =
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row(
                    "SELECT active_entry_id, (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), revision FROM session",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("read preceding session state");
        let original_first: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT details_json FROM subagent_tasks WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .expect("read first task details");
        let original_second: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT details_json FROM subagent_tasks WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("read second task details");
        let completed_before: (String, Option<i64>, i64, String) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision, task_revision, details_json FROM subagent_tasks WHERE id = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read completed task");
        drop(writer);
        session.close().expect("close interrupted session");

        let recovered = Session::open(home.path(), &id).expect("open recovered session");
        let connection = recovered
            .connection_for_test()
            .expect("lock recovered session connection");
        let (active_entry_id, recovered_entries, recovered_checkpoints, updated_at_ms, revision):
            (i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT active_entry_id, (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read recovered session state");
        assert_eq!(active_entry_id, previous_active);
        assert_eq!(
            (recovered_entries, recovered_checkpoints),
            (entries, checkpoints)
        );
        assert_eq!(revision, previous_revision + 2);
        let tasks = {
            let mut statement = connection
                .prepare(
                    "SELECT id, state, terminal_revision, task_revision, details_json FROM subagent_tasks ORDER BY id",
                )
                .expect("prepare task query");
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .expect("query tasks")
                .collect::<Result<Vec<_>, _>>()
                .expect("read tasks")
        };
        let provenance = connection
            .prepare(
                "SELECT id, parent_task_id, origin_step_id, admitted_revision FROM subagent_tasks ORDER BY id",
            )
            .expect("prepare provenance query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("query task provenance")
            .collect::<Result<Vec<_>, _>>()
            .expect("read task provenance");
        assert_eq!(
            provenance,
            [
                (0, None, i64::try_from(origin_step_id).unwrap(), 5),
                (1, Some(0), i64::try_from(origin_step_id).unwrap(), 6),
                (2, None, i64::try_from(origin_step_id).unwrap(), 7),
            ]
        );
        assert_eq!(tasks[0].0, 0);
        assert_eq!(
            (&tasks[0].1, tasks[0].2, tasks[0].3),
            (&"failed".to_owned(), Some(previous_revision + 1), 8)
        );
        assert_eq!(tasks[1].0, 1);
        assert_eq!(
            (&tasks[1].1, tasks[1].2, tasks[1].3),
            (&"failed".to_owned(), Some(previous_revision + 2), 10)
        );
        assert_eq!(
            tasks[2],
            (
                2,
                completed_before.0.clone(),
                completed_before.1,
                completed_before.2,
                completed_before.3.clone()
            )
        );
        let mut original_first: serde_json::Value =
            serde_json::from_str(&original_first).expect("decode original first task");
        let mut recovered_first: serde_json::Value =
            serde_json::from_str(&tasks[0].4).expect("decode recovered first task");
        assert_eq!(
            recovered_first["terminal_at_ms"],
            serde_json::json!(updated_at_ms)
        );
        assert_eq!(
            recovered_first["outcome"],
            serde_json::json!({ "Failed": { "message": "Interrupted by previous process exit" } })
        );
        let mut original_second: serde_json::Value =
            serde_json::from_str(&original_second).expect("decode original second task");
        let mut recovered_second: serde_json::Value =
            serde_json::from_str(&tasks[1].4).expect("decode recovered second task");
        assert_eq!(
            recovered_second["terminal_at_ms"],
            serde_json::json!(updated_at_ms)
        );
        for details in [
            &mut original_first,
            &mut recovered_first,
            &mut original_second,
            &mut recovered_second,
        ] {
            let object = details.as_object_mut().expect("task details object");
            object.remove("terminal_at_ms");
            object.remove("outcome");
        }
        assert_eq!(recovered_first, original_first);
        assert_eq!(recovered_second, original_second);
        drop(connection);
        recovered.close().expect("close recovered session");
    }

    #[tokio::test]
    async fn recovery_orders_foreground_before_tasks_and_is_idempotent() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let writer = session.writer();
        let run_id = writer
            .commit_request("combined interruption".to_owned())
            .await
            .expect("commit request");
        let step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start pending step");
        let step_sql_id = i64::try_from(step_id).expect("step ID fits SQLite");
        let completed_admission =
            crate::subagents::TaskRecord::for_test("00", 1, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Starting,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    observed_at_ms: 100,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&completed_admission, step_id)
                        .expect("encode completed task admission"),
                    || false,
                )
                .await
                .expect("admit completed task"),
            "commit completed task"
        );
        let completed =
            crate::subagents::TaskRecord::for_test("00", 2, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: Some(200),
                    state: crate::subagents::TaskState::Completed,
                    step_count: 1,
                    elapsed: Duration::new(1, 2),
                    observed_at_ms: 200,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: Some(crate::subagents::TaskOutcome::Completed(
                        crate::subagents::ChildResult::new("done".to_owned(), BTreeMap::new()),
                    )),
                });
        writer
            .terminal_subagent_task(TaskTerminal::from_record(&completed).expect("encode result"))
            .await
            .expect("commit completed task");
        let pending = crate::subagents::TaskRecord::for_test(
            "01",
            4,
            selection(),
            None,
            1,
            vec![crate::model::AttemptAccounting::UsageUnavailable],
        )
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 101,
            terminal_at_ms: None,
            state: crate::subagents::TaskState::Running,
            step_count: 2,
            elapsed: Duration::new(3, 4),
            observed_at_ms: 201,
            attempt_evidence: Vec::new(),
            executions: Vec::new(),
            outcome: None,
        });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&pending, step_id).expect("encode pending task"),
                    || false,
                )
                .await
                .expect("admit pending task"),
            "commit pending task"
        );
        let previous_revision: i64 = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row("SELECT revision FROM session", [], |row| row.get(0))
            .expect("read preceding revision");
        let completed_details: String = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT details_json FROM subagent_tasks WHERE id = 0",
                [],
                |row| row.get(0),
            )
            .expect("read completed task details");
        drop(writer);
        session.close().expect("close interrupted session");

        let recovered = Session::open(home.path(), &id).expect("open recovered session");
        let connection = recovered
            .connection_for_test()
            .expect("lock recovered session connection");
        let foreground: (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT entries.id, entries.revision, entries.checkpoint_id, entries.created_at_ms FROM runs JOIN entries ON entries.id = runs.terminal_entry_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read recovered foreground terminal");
        let pending_task: (String, Option<i64>, i64, String, i64) = connection
            .query_row(
                "SELECT state, terminal_revision, task_revision, details_json, origin_step_id FROM subagent_tasks WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read recovered child");
        assert_eq!(foreground.1, previous_revision + 1);
        assert_eq!(pending_task.1, Some(previous_revision + 2));
        assert_eq!(pending_task.0, "failed");
        assert_eq!(pending_task.2, 5);
        assert_eq!(pending_task.4, step_sql_id);
        assert_eq!(
            connection
                .query_row(
                    "SELECT settled_entry_id FROM model_steps WHERE id = ?1",
                    params![step_sql_id],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .expect("read settled origin step"),
            Some(foreground.0)
        );
        assert_eq!(
            connection
                .query_row("SELECT active_entry_id FROM session", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("read active entry"),
            foreground.0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT details_json FROM subagent_tasks WHERE id = 0",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("read completed task details"),
            completed_details
        );
        let first_snapshot = {
            let session: (i64, i64, i64) = connection
                .query_row(
                    "SELECT active_entry_id, updated_at_ms, revision FROM session",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read session snapshot");
            let steps = connection
                .prepare("SELECT id, settled_entry_id FROM model_steps ORDER BY id")
                .expect("prepare step snapshot")
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
                })
                .expect("query step snapshot")
                .collect::<Result<Vec<_>, _>>()
                .expect("read step snapshot");
            let tasks = connection
                .prepare(
                    "SELECT id, state, terminal_revision, task_revision, details_json FROM subagent_tasks ORDER BY id",
                )
                .expect("prepare task snapshot")
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })
                .expect("query task snapshot")
                .collect::<Result<Vec<_>, _>>()
                .expect("read task snapshot");
            let counts: (i64, i64) = connection
                .query_row(
                    "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints)",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read entry snapshot");
            (session, foreground, steps, tasks, counts)
        };
        drop(connection);
        recovered.close().expect("close recovered session");

        let reopened = Session::open(home.path(), &id).expect("open settled session");
        let connection = reopened
            .connection_for_test()
            .expect("lock settled session connection");
        let second_session: (i64, i64, i64) = connection
            .query_row(
                "SELECT active_entry_id, updated_at_ms, revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read settled session snapshot");
        let second_foreground: (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT entries.id, entries.revision, entries.checkpoint_id, entries.created_at_ms FROM runs JOIN entries ON entries.id = runs.terminal_entry_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read settled foreground snapshot");
        let second_steps = connection
            .prepare("SELECT id, settled_entry_id FROM model_steps ORDER BY id")
            .expect("prepare settled step snapshot")
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
            })
            .expect("query settled step snapshot")
            .collect::<Result<Vec<_>, _>>()
            .expect("read settled step snapshot");
        let second_tasks = connection
            .prepare(
                "SELECT id, state, terminal_revision, task_revision, details_json FROM subagent_tasks ORDER BY id",
            )
            .expect("prepare settled task snapshot")
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .expect("query settled task snapshot")
            .collect::<Result<Vec<_>, _>>()
            .expect("read settled task snapshot");
        let second_counts: (i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read settled entry snapshot");
        assert_eq!(
            (
                second_session,
                second_foreground,
                second_steps,
                second_tasks,
                second_counts
            ),
            first_snapshot
        );
        drop(connection);
        reopened.close().expect("close settled session");
    }

    #[tokio::test]
    async fn recovery_rejects_terminal_task_details_atomically() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let writer = session.writer();
        let run_id = writer
            .commit_request("interrupted request".to_owned())
            .await
            .expect("commit request");
        let step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start pending step");
        let admission =
            crate::subagents::TaskRecord::for_test("00", 1, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Starting,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    observed_at_ms: 100,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&admission, step_id).expect("encode task admission"),
                    || false,
                )
                .await
                .expect("admit task"),
            "commit task"
        );
        let invalid =
            crate::subagents::TaskRecord::for_test("00", 2, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: Some(200),
                    state: crate::subagents::TaskState::Failed,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    observed_at_ms: 100,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: Some(crate::subagents::TaskOutcome::Failed(
                        crate::subagents::ChildFailure::new("already terminal"),
                    )),
                });
        let invalid_details = TaskAdmission::from_record(&invalid, step_id)
            .expect("encode invalid task details")
            .details_json;
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute(
                "UPDATE subagent_tasks SET task_revision = 2, details_json = ?1 WHERE id = 0",
                params![invalid_details],
            )
            .expect("store invalid task details");
        let before: (i64, i64, i64, i64, String) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), session.revision, (SELECT details_json FROM subagent_tasks WHERE id = 0) FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read preceding state");
        drop(writer);
        session.close().expect("close interrupted session");

        match Session::open(home.path(), &id) {
            Err(SessionError::Codec(_)) => {}
            Err(error) => panic!("unexpected recovery error: {error}"),
            Ok(session) => {
                session
                    .close()
                    .expect("close unexpectedly recovered session");
                panic!("recovery accepted terminal task details");
            }
        }
        let connection = Connection::open(database_path(home.path(), &id)).expect("open database");
        let after: (i64, i64, i64, i64, String) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), session.revision, (SELECT details_json FROM subagent_tasks WHERE id = 0) FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read rolled back state");
        assert_eq!(after, before);
    }

    #[tokio::test]
    async fn foreground_writes_settle_entries_and_runs_atomically() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let writer = session.writer();
        let run_id = writer
            .commit_request("persist this request".to_owned())
            .await
            .expect("commit request");
        let code_step = writer
            .start_model_step(run_id)
            .await
            .expect("allocate code step");
        let code = PersistedCode {
            source: "value = 1".to_owned(),
            call_id: crate::model::ModelCallId::new("call-1"),
            commentary: Some("setting a value".to_owned()),
            reasoning_summary: "set the value".to_owned(),
            selection: Some(selection()),
            replay: crate::model::ProviderReplay::new(
                "test",
                vec![crate::model::OpaqueReplayItem::new(vec![0, 255])],
            ),
            result: crate::model::CodeResult::Execution {
                feedback: "value set".to_owned(),
            },
            execution: Some(crate::arena::Execution {
                stdout: empty_observation(),
                stderr: empty_observation(),
                outcome: crate::arena::ExecutionOutcome::Complete { result: None },
                tool_calls: Vec::new(),
            }),
            attempts: vec![crate::model::AttemptAccounting::UsageUnavailable],
        };
        writer
            .settle_execution(
                run_id,
                code_step,
                code,
                session.initial_checkpoint().unwrap(),
            )
            .await
            .expect("settle code step");
        let final_step = writer
            .start_model_step(run_id)
            .await
            .expect("allocate final step");
        writer
            .close_run(
                run_id,
                Some(final_step),
                move || PersistedTerminal::Final {
                    step_id: final_step,
                    text: "done".to_owned(),
                    reasoning_summary: "answer".to_owned(),
                    replay: crate::model::ProviderReplay::new(
                        "test",
                        vec![crate::model::OpaqueReplayItem::new(vec![1, 2, 3])],
                    ),
                    attempts: vec![crate::model::AttemptAccounting::UsageUnavailable],
                },
                None,
            )
            .await
            .expect("close run");
        let connection = session
            .connection_for_test()
            .expect("lock session connection");
        let (entries, checkpoints, pending_steps, closed_runs, revision):
            (i64, i64, i64, i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read persisted lifecycle");
        assert_eq!(
            (entries, checkpoints, pending_steps, closed_runs, revision),
            (4, 2, 0, 1, 6)
        );
        drop(connection);
        let restored = session.restore().expect("restore session projection");
        assert_eq!(restored.records.len(), 3);
        assert_eq!(
            restored
                .executable
                .expect("valid persisted state")
                .history
                .entries()
                .len(),
            3
        );
        let connection = session
            .connection_for_test()
            .expect("lock session connection");
        connection
            .execute_batch("DROP TRIGGER entries_are_immutable_before_update")
            .expect("remove entry immutability trigger");
        connection
            .execute(
                "UPDATE entries SET provider_replay = X'00' WHERE kind = 'execution'",
                [],
            )
            .expect("corrupt replay");
        drop(connection);
        let restored = session.restore().expect("restore readable projection");
        assert_eq!(restored.records.len(), 3);
        assert!(restored.executable.is_none());
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn step_allocation_rollback_does_not_advance_revision() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let writer = session.writer();
        let run_id = writer
            .commit_request("rollback allocation".to_owned())
            .await
            .expect("commit request");
        writer
            .start_model_step(run_id)
            .await
            .expect("allocate first step");
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row("SELECT revision FROM session", [], |row| row
                    .get::<_, i64>(0))
                .expect("read session revision"),
            3
        );
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch(
                "CREATE TRIGGER reject_step BEFORE INSERT ON model_steps BEGIN SELECT RAISE(ABORT, 'reject step'); END;",
            )
            .expect("install rollback trigger");

        assert!(writer.start_model_step(run_id).await.is_err());
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row("SELECT revision FROM session", [], |row| row
                    .get::<_, i64>(0))
                .expect("read session revision"),
            3
        );
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn settled_execution_rollback_keeps_the_preceding_state() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let writer = session.writer();
        let run_id = writer
            .commit_request("rollback".to_owned())
            .await
            .expect("commit request");
        let step_id = writer
            .start_model_step(run_id)
            .await
            .expect("allocate step");
        session
            .connection_for_test()
            .expect("lock session connection")
            .execute_batch(
                "CREATE TRIGGER reject_execution BEFORE INSERT ON entries WHEN NEW.kind = 'execution' BEGIN SELECT RAISE(ABORT, 'reject execution'); END;",
            )
            .expect("install rollback trigger");
        let code = PersistedCode {
            source: "value = 1".to_owned(),
            call_id: crate::model::ModelCallId::new("call-rollback"),
            commentary: None,
            reasoning_summary: String::new(),
            selection: None,
            replay: crate::model::ProviderReplay::new("test", Vec::new()),
            result: crate::model::CodeResult::Execution {
                feedback: String::new(),
            },
            execution: Some(crate::arena::Execution {
                stdout: empty_observation(),
                stderr: empty_observation(),
                outcome: crate::arena::ExecutionOutcome::Complete { result: None },
                tool_calls: Vec::new(),
            }),
            attempts: Vec::new(),
        };
        assert!(
            writer
                .settle_execution(run_id, step_id, code, session.initial_checkpoint().unwrap())
                .await
                .is_err()
        );
        let connection = session
            .connection_for_test()
            .expect("lock session connection");
        let (entries, checkpoints, revision, pending_steps): (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), revision, (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL) FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read rolled back state");
        assert_eq!(
            (entries, checkpoints, revision, pending_steps),
            (2, 1, 3, 1)
        );
        drop(connection);
        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    async fn task_step_commit_advances_only_newer_nonterminal_snapshots() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let writer = session.writer();
        let run_id = writer
            .commit_request("parent".to_owned())
            .await
            .expect("start parent run");
        let origin_step_id = writer
            .start_model_step(run_id)
            .await
            .expect("start parent model step");
        let admitted =
            crate::subagents::TaskRecord::for_test("00", 1, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Starting,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    observed_at_ms: 100,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert!(
            writer
                .admit_subagent_task(
                    TaskAdmission::from_record(&admitted, origin_step_id)
                        .expect("encode admission"),
                    || false,
                )
                .await
                .expect("admit task"),
            "commit task admission"
        );
        let newer =
            crate::subagents::TaskRecord::for_test("00", 2, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Running,
                    step_count: 1,
                    elapsed: Duration::from_millis(25),
                    observed_at_ms: 125,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert_eq!(
            writer
                .task_step(
                    TaskSnapshot::from_record(&newer).expect("encode newer snapshot"),
                    || false,
                )
                .await
                .expect("commit newer snapshot"),
            TaskStepCommit::Committed
        );
        let persisted_after_newer: (String, Option<i64>, i64, String, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision, task_revision, details_json, (SELECT revision FROM session) FROM subagent_tasks WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read newer task snapshot");
        assert_eq!(persisted_after_newer.0, "running");
        assert_eq!(persisted_after_newer.1, None);
        assert_eq!(persisted_after_newer.2, 2);
        assert_eq!(persisted_after_newer.4, 5);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&persisted_after_newer.3)
                .expect("decode newer task details")["observation"]["step_count"],
            1
        );

        let rejected =
            crate::subagents::TaskRecord::for_test("00", 3, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Running,
                    step_count: 2,
                    elapsed: Duration::from_millis(50),
                    observed_at_ms: 150,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert_eq!(
            writer
                .task_step(
                    TaskSnapshot::from_record(&rejected).expect("encode rejected snapshot"),
                    || true,
                )
                .await
                .expect("reject cancelled snapshot"),
            TaskStepCommit::Rejected
        );
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row(
                    "SELECT state, terminal_revision, task_revision, details_json, (SELECT revision FROM session) FROM subagent_tasks WHERE id = 0",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .expect("read unchanged rejected task"),
            persisted_after_newer
        );

        for snapshot in [&newer, &admitted] {
            assert_eq!(
                writer
                    .task_step(
                        TaskSnapshot::from_record(snapshot).expect("encode stale snapshot"),
                        || false,
                    )
                    .await
                    .expect("reject stale snapshot"),
                TaskStepCommit::Superseded
            );
        }
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row(
                    "SELECT state, terminal_revision, task_revision, details_json, (SELECT revision FROM session) FROM subagent_tasks WHERE id = 0",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .expect("read unchanged stale task"),
            persisted_after_newer
        );

        let terminal =
            crate::subagents::TaskRecord::for_test("00", 3, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: Some(300),
                    state: crate::subagents::TaskState::Completed,
                    step_count: 1,
                    elapsed: Duration::from_millis(200),
                    observed_at_ms: 300,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: Some(crate::subagents::TaskOutcome::Completed(
                        crate::subagents::ChildResult::new("done".to_owned(), BTreeMap::new()),
                    )),
                });
        writer
            .terminal_subagent_task(TaskTerminal::from_record(&terminal).expect("encode terminal"))
            .await
            .expect("commit terminal task");
        let terminal_state: (String, Option<i64>, i64, String, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT state, terminal_revision, task_revision, details_json, (SELECT revision FROM session) FROM subagent_tasks WHERE id = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("read terminal task");
        assert_eq!(terminal_state.0, "completed");
        assert_eq!(terminal_state.1, Some(6));
        assert_eq!(terminal_state.2, 3);
        assert_eq!(terminal_state.4, 6);

        let late =
            crate::subagents::TaskRecord::for_test("00", 99, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 100,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Running,
                    step_count: 99,
                    elapsed: Duration::from_secs(99),
                    observed_at_ms: 99_000,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert_eq!(
            writer
                .task_step(
                    TaskSnapshot::from_record(&late).expect("encode late snapshot"),
                    || false,
                )
                .await
                .expect("reject late snapshot"),
            TaskStepCommit::Superseded
        );
        assert_eq!(
            session
                .connection_for_test()
                .expect("lock session connection")
                .query_row(
                    "SELECT state, terminal_revision, task_revision, details_json, (SELECT revision FROM session) FROM subagent_tasks WHERE id = 0",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .expect("read unchanged terminal task"),
            terminal_state
        );

        let unknown =
            crate::subagents::TaskRecord::for_test("01", 2, selection(), None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 0,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Running,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    observed_at_ms: 0,
                    attempt_evidence: Vec::new(),
                    executions: Vec::new(),
                    outcome: None,
                });
        assert!(matches!(
            writer
                .task_step(
                    TaskSnapshot::from_record(&unknown).expect("encode unknown snapshot"),
                    || false,
                )
                .await,
            Err(SessionError::InvalidTransition)
        ));
        let (entries, checkpoints): (i64, i64) = session
            .connection_for_test()
            .expect("lock session connection")
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("count immutable session rows");
        assert_eq!((entries, checkpoints), (2, 1));

        drop(writer);
        session.close().expect("close session");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn child_process_does_not_inherit_writer_locks() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let socket = home.path().join("inherited-lock.sock");
        let listener = UnixListener::bind(&socket).expect("bind parent socket");
        let mut child = Command::new(env::current_exe().expect("locate test executable"))
            .arg("session::tests::writer_process")
            .arg("--exact")
            .arg("--nocapture")
            .env("KIKI_SESSION_TEST_SOCKET", &socket)
            .env("KIKI_SESSION_TEST_HOME", home.path())
            .env("KIKI_SESSION_TEST_ID", &id)
            .env("KIKI_SESSION_TEST_BLOCK", "1")
            .spawn()
            .expect("start writer process");
        let (mut stream, _) = listener.accept().expect("accept writer process");
        let mut result = [0; 2];
        stream.read_exact(&mut result).expect("read writer result");
        session.close().expect("close parent session");
        let reopened = Session::open(home.path(), &id);
        stream.write_all(b"X").expect("release writer process");
        assert!(child.wait().expect("wait for writer process").success());
        assert_eq!(result, [b'R', b'C']);
        reopened
            .expect("writer lock must not leak through exec")
            .close()
            .expect("close reopened session");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn subprocess_death_recovers_only_committed_operations() {
        for (milestone, signal) in [
            ("generation", b'G'),
            ("execution", b'E'),
            ("committed-step", b'C'),
            ("background-only", b'B'),
        ] {
            let home = tempfile::tempdir().expect("create home");
            let workspace = tempfile::tempdir().expect("create workspace");
            let session = create_session(&home, &workspace).await;
            let id = session.id().to_owned();
            let initial_checkpoint = session
                .initial_checkpoint()
                .expect("load initial checkpoint");
            let initial_checkpoint_id: i64 = session
                .connection_for_test()
                .expect("lock session connection")
                .query_row(
                    "SELECT checkpoint_id FROM entries WHERE kind = 'session_start'",
                    [],
                    |row| row.get(0),
                )
                .expect("read initial checkpoint ID");
            let lock = lock_path(home.path(), &id);
            let lock_inode = fs::metadata(&lock).expect("read lock metadata").ino();
            session.close().expect("close parent session");
            let socket = home.path().join(format!("{milestone}.sock"));
            let (mut child, _stream, ready) =
                recovery_process_result(home.path(), &id, workspace.path(), &socket, milestone);
            let killed = child.kill();
            let status = child.wait().expect("wait for killed recovery process");
            assert_eq!(ready, signal);
            killed.expect("kill blocked recovery process");
            assert!(!status.success());
            assert_eq!(
                fs::metadata(&lock)
                    .expect("read retained lock metadata")
                    .ino(),
                lock_inode
            );

            let recovered = Session::open(home.path(), &id).expect("recover interrupted session");
            let committed_checkpoint = {
                let connection = recovered
                    .connection_for_test()
                    .expect("lock recovered session connection");
                match milestone {
                    "generation" | "execution" => {
                        let (entries, checkpoints, pending_steps, closed_runs, active_entry_id):
                        (i64, i64, i64, i64, i64) = connection
                        .query_row(
                            "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), (SELECT COUNT(*) FROM model_steps WHERE settled_entry_id IS NULL), (SELECT COUNT(*) FROM runs WHERE terminal_entry_id IS NOT NULL), active_entry_id FROM session",
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                        )
                        .expect("read recovered generation state");
                        let (terminal_entry_id, terminal_checkpoint_id): (i64, i64) = connection
                        .query_row(
                            "SELECT terminal_entry_id, entries.checkpoint_id FROM runs JOIN entries ON entries.id = runs.terminal_entry_id",
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .expect("read recovered terminal");
                        assert_eq!(
                            (entries, checkpoints, pending_steps, closed_runs),
                            (3, 1, 0, 1)
                        );
                        assert_eq!(active_entry_id, terminal_entry_id);
                        assert_eq!(terminal_checkpoint_id, initial_checkpoint_id);
                        None
                    }
                    "committed-step" => {
                        let (entries, checkpoints, active_entry_id): (i64, i64, i64) = connection
                        .query_row(
                            "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), active_entry_id FROM session",
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .expect("read recovered committed-step state");
                        let (terminal_entry_id, parent_id, terminal_checkpoint_id): (i64, i64, i64) =
                        connection
                            .query_row(
                                "SELECT terminal_entry_id, entries.parent_id, entries.checkpoint_id FROM runs JOIN entries ON entries.id = runs.terminal_entry_id",
                                [],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                            )
                            .expect("read recovered terminal");
                        let execution: (i64, i64) = connection
                            .query_row(
                                "SELECT id, checkpoint_id FROM entries WHERE kind = 'execution'",
                                [],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .expect("read committed execution");
                        let settled_entries = connection
                            .prepare("SELECT settled_entry_id FROM model_steps ORDER BY id")
                            .expect("prepare settled-step query")
                            .query_map([], |row| row.get::<_, Option<i64>>(0))
                            .expect("query settled steps")
                            .collect::<Result<Vec<_>, _>>()
                            .expect("read settled steps");
                        assert_eq!((entries, checkpoints), (4, 2));
                        assert_eq!(active_entry_id, terminal_entry_id);
                        assert_eq!(parent_id, execution.0);
                        assert_eq!(terminal_checkpoint_id, execution.1);
                        assert_eq!(
                            settled_entries,
                            [Some(execution.0), Some(terminal_entry_id)]
                        );
                        let (runtime_identity, arena_state, authorisations_json): (
                        String,
                        Vec<u8>,
                        String,
                    ) = connection
                        .query_row(
                            "SELECT runtime_identity, arena_state, authorisations_json FROM checkpoints WHERE id = ?1",
                            params![execution.1],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .expect("read committed checkpoint");
                        let committed_checkpoint = super::codec::decode_checkpoint(
                            runtime_identity,
                            arena_state,
                            &authorisations_json,
                        )
                        .expect("decode committed checkpoint");
                        assert_ne!(
                            committed_checkpoint.session_parts().1,
                            initial_checkpoint.session_parts().1
                        );
                        Some(committed_checkpoint)
                    }
                    "background-only" => {
                        let (entries, checkpoints, active_entry_id, terminal_entry_id):
                        (i64, i64, i64, i64) = connection
                        .query_row(
                            "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), session.active_entry_id, runs.terminal_entry_id FROM session JOIN runs",
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )
                        .expect("read recovered background state");
                        let (state, terminal_revision, details_json): (
                            String,
                            Option<i64>,
                            String,
                        ) = connection
                            .query_row(
                                "SELECT state, terminal_revision, details_json FROM subagent_tasks",
                                [],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                            )
                            .expect("read recovered background task");
                        assert_eq!((entries, checkpoints), (3, 1));
                        assert_eq!(active_entry_id, terminal_entry_id);
                        assert_eq!(state, "failed");
                        assert!(terminal_revision.is_some());
                        assert_eq!(
                            serde_json::from_str::<serde_json::Value>(&details_json)
                                .expect("decode recovered task details")["outcome"],
                            serde_json::json!({ "Failed": { "message": "Interrupted by previous process exit" } })
                        );
                        None
                    }
                    _ => unreachable!(),
                }
            };
            if let Some(committed_checkpoint) = committed_checkpoint {
                let mut restored_arena = arena(&workspace).await;
                restored_arena
                    .restore(committed_checkpoint)
                    .await
                    .expect("restore committed checkpoint");
                assert_eq!(
                    restored_arena
                        .collect_result_values()
                        .await
                        .expect("collect committed result values"),
                    BTreeMap::from([("committed".to_owned(), ToolValue::Integer(1))])
                );
            }
            recovered.close().expect("close recovered session");
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn hot_delete_rollback_journal_requires_writable_recovery() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let initial = session
            .initial_checkpoint()
            .expect("load initial checkpoint");
        let lock = lock_path(home.path(), &id);
        let lock_inode = fs::metadata(&lock).expect("read lock metadata").ino();
        let original_database_len = usize::try_from(
            fs::metadata(database_path(home.path(), &id))
                .expect("read initial database metadata")
                .len(),
        )
        .expect("initial database size fits usize");
        session.close().expect("close parent session");
        let socket = home.path().join("hot-journal.sock");
        let listener = UnixListener::bind(&socket).expect("bind parent socket");
        let mut child = Command::new(env::current_exe().expect("locate test executable"))
            .arg("session::tests::hot_rollback_process")
            .arg("--exact")
            .arg("--nocapture")
            .env("KIKI_SESSION_TEST_SOCKET", &socket)
            .env("KIKI_SESSION_TEST_HOME", home.path())
            .env("KIKI_SESSION_TEST_ID", &id)
            .spawn()
            .expect("start rollback process");
        let (mut stream, _) = listener.accept().expect("accept rollback process");
        let mut ready = [0];
        stream
            .read_exact(&mut ready)
            .expect("read rollback milestone");
        let killed = child.kill();
        let status = child.wait().expect("wait for killed rollback process");
        assert_eq!(ready, [b'J']);
        killed.expect("kill blocked rollback process");
        assert!(!status.success());
        let database = database_path(home.path(), &id);
        let journal = database.with_file_name(format!("{id}.sqlite3-journal"));
        let database_before = fs::read(&database).expect("snapshot hot database");
        let journal_before = fs::read(&journal).expect("snapshot hot journal");
        assert!(database_before.len() > original_database_len);
        assert!(journal_before.starts_with(&[0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7]));
        assert!(journal_before.len() > 512);
        assert!(journal_before.len() < 1_000_000);
        let error = Session::read_cwd(home.path(), &id)
            .expect_err("read-only access must reject a hot rollback journal");
        assert!(error.requires_writable_recovery());
        let SessionError::Sqlite(error) = error else {
            panic!("read-only hot-journal error must come from SQLite");
        };
        assert_eq!(error.sqlite_extended_error_code(), Some(776));
        let error = Session::read_initial_checkpoint(home.path(), &id)
            .expect_err("read-only access must reject a hot rollback journal");
        let SessionError::Sqlite(error) = error else {
            panic!("read-only hot-journal error must come from SQLite");
        };
        assert_eq!(error.sqlite_extended_error_code(), Some(776));
        let healthy = create_session(&home, &workspace).await;
        let healthy_id = healthy.id().to_owned();
        healthy.close().expect("close healthy session");
        assert_eq!(fs::read(&database).unwrap(), database_before);
        assert_eq!(fs::read(&journal).unwrap(), journal_before);
        let (rows, warnings) = Session::list(home.path())
            .expect("listing should retain healthy candidates despite a hot journal")
            .into_parts();
        assert_eq!(
            rows.iter()
                .map(super::SessionListRow::id)
                .collect::<Vec<_>>(),
            [healthy_id.as_str()]
        );
        assert_eq!(
            warnings
                .iter()
                .map(super::SessionListWarning::id)
                .collect::<Vec<_>>(),
            [id.as_str()]
        );
        assert_eq!(fs::read(&database).unwrap(), database_before);
        assert_eq!(fs::read(&journal).unwrap(), journal_before);
        assert_eq!(
            fs::metadata(&lock)
                .expect("read retained lock metadata")
                .ino(),
            lock_inode
        );

        let recovered = Session::open(home.path(), &id).expect("recover hot rollback journal");
        let connection = recovered
            .connection_for_test()
            .expect("lock recovered session connection");
        let state: (i64, i64, i64, i64) = connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM entries), (SELECT COUNT(*) FROM checkpoints), (SELECT COUNT(*) FROM runs), revision FROM session",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read recovered database state");
        assert_eq!(state, (1, 1, 0, 1));
        drop(connection);
        assert_eq!(
            recovered
                .initial_checkpoint()
                .expect("read recovered checkpoint"),
            initial
        );
        assert_eq!(
            recovered.cwd().expect("read recovered workspace"),
            workspace.path()
        );
        assert!(!journal.exists() || fs::metadata(&journal).unwrap().len() == 0);
        assert_eq!(
            fs::metadata(&lock)
                .expect("read retained lock metadata")
                .ino(),
            lock_inode
        );
        recovered.close().expect("close recovered session");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn writer_ownership_is_process_exclusive_and_keeps_a_stable_lock_file() {
        let home = tempfile::tempdir().expect("create home");
        let workspace = tempfile::tempdir().expect("create workspace");
        let session = create_session(&home, &workspace).await;
        let id = session.id().to_owned();
        let lock = lock_path(home.path(), &id);
        let inode = fs::metadata(&lock).expect("read lock metadata").ino();
        let first_socket = home.path().join("first-writer.sock");

        assert_eq!(
            writer_process_result(home.path(), &id, &first_socket),
            [b'R', b'C']
        );
        assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
        session.close().expect("close owner session");
        assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
        let second_socket = home.path().join("second-writer.sock");
        assert_eq!(
            writer_process_result(home.path(), &id, &second_socket),
            [b'R', b'O']
        );
    }
}
