mod codec;

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use thiserror::Error;
use tokio::task;

use crate::arena::Checkpoint;
use crate::config::ModelSelection;

const DATABASE_SUFFIX: &str = ".sqlite3";
const LOCK_SUFFIX: &str = ".lock";
const SQLITE_HEADER_BYTES: usize = 100;
const SQLITE_SIGNATURE: &[u8] = b"SQLite format 3\0";
const FORMAT_VERSION: u32 = 1;

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

#[derive(Clone)]
pub(crate) struct SessionWriter {
    state: Arc<SessionState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionWriteKind {
    Request,
    Step,
    Settlement,
    Closure,
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
    _writer_lock: File,
    #[cfg(test)]
    write_gate: Option<SessionWriteGate>,
}

pub(crate) struct Session {
    id: SessionId,
    state: Arc<SessionState>,
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
        let connection = open_writable_database(&paths.database)?;
        let stored_id: String =
            connection.query_row("SELECT id FROM session", [], |row| row.get(0))?;
        if stored_id != id.as_str() {
            return Err(SessionError::StoredIdMismatch);
        }
        Ok(Self {
            id,
            state: Arc::new(SessionState {
                connection: Mutex::new(connection),
                _writer_lock: writer_lock,
                #[cfg(test)]
                write_gate: None,
            }),
        })
    }

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

    pub(crate) fn initial_checkpoint(&self) -> Result<Checkpoint, SessionError> {
        let connection = self.connection()?;
        load_initial_checkpoint(&connection)
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

    async fn write<T: Send + 'static>(
        &self,
        kind: SessionWriteKind,
        operation: impl FnOnce(&mut Connection) -> Result<T, SessionError> + Send + 'static,
    ) -> Result<T, SessionError> {
        let state = Arc::clone(&self.state);
        task::spawn_blocking(move || {
            #[cfg(not(test))]
            let _ = kind;
            let mut connection = state
                .connection
                .lock()
                .map_err(|_| SessionError::WriterUnavailable)?;
            #[cfg(test)]
            if let Some(gate) = &state.write_gate {
                gate.wait(kind);
            }
            operation(&mut connection)
        })
        .await
        .map_err(|_| SessionError::WriterUnavailable)?
    }
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
    let preferences_json = initial_preferences(selection)?;
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
            _writer_lock: writer_lock,
            #[cfg(test)]
            write_gate,
        }),
    })
}

fn initial_preferences(selection: ModelSelection) -> Result<String, SessionError> {
    let mut thinking = serde_json::Map::new();
    thinking.insert(
        selection.model().id.to_owned(),
        serde_json::Value::String(selection.thinking().to_owned()),
    );
    serde_json::to_string(&serde_json::json!({
        "selected_model": selection.model().id,
        "thinking": thinking,
    }))
    .map_err(|error| SessionError::Codec(error.to_string()))
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
    options.mode(0o600);
    let file = options.open(path)?;
    set_owner_only(path, 0o600)?;
    flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(io::Error::from)?;
    Ok(file)
}

fn open_lock(path: &Path) -> Result<File, SessionError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    set_owner_only(path, 0o600)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(error) if error == Errno::AGAIN || error == Errno::WOULDBLOCK => {
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
    use std::collections::HashSet;
    use std::env;
    use std::fs;
    #[cfg(unix)]
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Command;

    use rusqlite::{Connection, params};
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    #[cfg(unix)]
    use std::os::unix::net::{UnixListener, UnixStream};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{PersistedCode, PersistedTerminal, Session, SessionError, SessionId};
    use crate::arena::{Arena, Checkpoint, ExecutionOutcome};
    use crate::config::{DEFAULT_MODEL, ModelSelection};
    use crate::tools::ToolHost;

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

            assert!(Session::open(home.path(), id).is_err());
            assert_eq!(fs::read(&database).unwrap(), contents);
            assert_eq!(fs::read(&journal).unwrap(), b"journal bytes");
        }
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
