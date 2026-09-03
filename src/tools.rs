use std::collections::BTreeMap;
#[cfg(test)]
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Condvar, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, Metadata};
use futures::{Stream, StreamExt, stream};
use globset::{GlobBuilder, GlobMatcher};
use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkError, SinkMatch};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
#[cfg(unix)]
use rustix::io::Errno;
use thiserror::Error;
#[cfg(test)]
use tokio::sync::{Notify, Semaphore};

use crate::config::constants::{
    COLLECTION_MAX_ITEMS, COLLECTION_MAX_PAYLOAD_BYTES, FILE_MAX_BYTES, TOOL_CONCURRENCY_LIMIT,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolValue {
    None,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<Self>),
    Dictionary(BTreeMap<String, Self>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolName {
    Read,
    ListDir,
    Glob,
    Grep,
}

impl ToolName {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ListDir => "list_dir",
            Self::Glob => "glob",
            Self::Grep => "grep",
        }
    }

    pub(crate) fn private_name(self) -> &'static str {
        match self {
            Self::Read => "_kiki_read",
            Self::ListDir => "_kiki_list_dir",
            Self::Glob => "_kiki_glob",
            Self::Grep => "_kiki_grep",
        }
    }

    pub(crate) fn from_private_name(name: &str) -> Option<Self> {
        ACTIVE_TOOLS
            .iter()
            .find(|definition| definition.name.private_name() == name)
            .map(|definition| definition.name)
    }

    pub(crate) fn label(self) -> &'static str {
        self.name()
    }

    pub(crate) fn count_label(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::Read, 1) => "byte",
            (Self::Read, _) => "bytes",
            (Self::ListDir | Self::Glob, 1) => "entry",
            (Self::ListDir | Self::Glob, _) => "entries",
            (Self::Grep, 1) => "match",
            (Self::Grep, _) => "matches",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCall {
    pub(crate) call_id: u32,
    pub(crate) tool: ToolName,
    pub(crate) arguments: ToolValue,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolResult {
    pub(crate) call_id: u32,
    pub(crate) result: Result<ToolValue, ToolError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolErrorKind {
    Type,
    FileNotFound,
    IsADirectory,
    NotADirectory,
    Permission,
    Value,
    Runtime,
    Os,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub(crate) struct ToolError {
    pub(crate) kind: ToolErrorKind,
    message: String,
}

impl ToolError {
    fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn type_error(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Type, message)
    }

    fn permission(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Permission, message)
    }

    fn value(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Value, message)
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Runtime, message)
    }

    fn from_io(error: std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::NotFound => ToolErrorKind::FileNotFound,
            std::io::ErrorKind::NotADirectory => ToolErrorKind::NotADirectory,
            std::io::ErrorKind::PermissionDenied => ToolErrorKind::Permission,
            _ => ToolErrorKind::Os,
        };
        let message = match kind {
            ToolErrorKind::FileNotFound => "workspace path was not found".to_owned(),
            ToolErrorKind::NotADirectory => "path is not a directory".to_owned(),
            ToolErrorKind::Permission => "workspace access was denied".to_owned(),
            ToolErrorKind::Os => "filesystem operation failed".to_owned(),
            ToolErrorKind::Type
            | ToolErrorKind::IsADirectory
            | ToolErrorKind::Value
            | ToolErrorKind::Runtime => {
                unreachable!("I/O errors map only to filesystem exception categories")
            }
        };
        Self::new(kind, message)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolParameter {
    name: &'static str,
    value_type: ToolValueType,
    default: Option<ToolDefault>,
}

#[derive(Clone, Copy)]
enum ToolDefault {
    String(&'static str),
    None,
}

impl ToolDefault {
    fn source(self) -> String {
        match self {
            Self::String(value) => format!("{value:?}"),
            Self::None => "None".to_owned(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ToolValueType {
    String,
    Integer,
    OptionalInteger,
    OptionalString,
}

impl ToolValueType {
    fn annotation(self) -> &'static str {
        match self {
            Self::String => "str",
            Self::Integer => "int",
            Self::OptionalInteger => "int | None",
            Self::OptionalString => "str | None",
        }
    }
}

#[derive(Clone, Copy)]
struct ToolRecordField {
    name: &'static str,
    value_type: ToolValueType,
}

#[derive(Clone, Copy)]
pub(crate) struct ToolRecord {
    name: &'static str,
    fields: &'static [ToolRecordField],
}

#[derive(Clone, Copy)]
pub(crate) enum ToolResultShape {
    String,
    RecordList(&'static ToolRecord),
}

impl ToolResultShape {
    fn annotation(self) -> String {
        match self {
            Self::String => "str".to_owned(),
            Self::RecordList(record) => format!("list[{}]", record.name),
        }
    }

    fn record(self) -> Option<&'static ToolRecord> {
        match self {
            Self::String => None,
            Self::RecordList(record) => Some(record),
        }
    }

    fn bootstrap_return(self, private_name: &str, arguments: &str) -> String {
        match self {
            Self::String => format!("await {private_name}({arguments})"),
            Self::RecordList(record) => {
                format!(
                    "[{}(**row) for row in await {private_name}({arguments})]",
                    record.name
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolDefinition {
    name: ToolName,
    parameters: &'static [ToolParameter],
    result: ToolResultShape,
    description: &'static str,
}

const READ_PARAMETERS: &[ToolParameter] = &[ToolParameter {
    name: "path",
    value_type: ToolValueType::String,
    default: None,
}];

const DIRECTORY_ENTRY_FIELDS: &[ToolRecordField] = &[
    ToolRecordField {
        name: "path",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "kind",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "size_bytes",
        value_type: ToolValueType::OptionalInteger,
    },
];

const DIRECTORY_ENTRY: ToolRecord = ToolRecord {
    name: "DirectoryEntry",
    fields: DIRECTORY_ENTRY_FIELDS,
};

const GREP_MATCH_FIELDS: &[ToolRecordField] = &[
    ToolRecordField {
        name: "path",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "line",
        value_type: ToolValueType::Integer,
    },
    ToolRecordField {
        name: "text",
        value_type: ToolValueType::String,
    },
];

const GREP_MATCH: ToolRecord = ToolRecord {
    name: "GrepMatch",
    fields: GREP_MATCH_FIELDS,
};

const LIST_DIR_PARAMETERS: &[ToolParameter] = &[ToolParameter {
    name: "path",
    value_type: ToolValueType::String,
    default: Some(ToolDefault::String(".")),
}];

const GLOB_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "pattern",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "path",
        value_type: ToolValueType::String,
        default: Some(ToolDefault::String(".")),
    },
];

const GREP_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "pattern",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "path",
        value_type: ToolValueType::String,
        default: Some(ToolDefault::String(".")),
    },
    ToolParameter {
        name: "file_glob",
        value_type: ToolValueType::OptionalString,
        default: Some(ToolDefault::None),
    },
];

pub(crate) const ACTIVE_TOOLS: &[ToolDefinition] = &[
    ToolDefinition {
        name: ToolName::Read,
        parameters: READ_PARAMETERS,
        result: ToolResultShape::String,
        description: "Read one complete UTF-8 workspace file.",
    },
    ToolDefinition {
        name: ToolName::ListDir,
        parameters: LIST_DIR_PARAMETERS,
        result: ToolResultShape::RecordList(&DIRECTORY_ENTRY),
        description: "List immediate workspace directory entries.",
    },
    ToolDefinition {
        name: ToolName::Glob,
        parameters: GLOB_PARAMETERS,
        result: ToolResultShape::RecordList(&DIRECTORY_ENTRY),
        description: "Match workspace filesystem entries recursively.",
    },
    ToolDefinition {
        name: ToolName::Grep,
        parameters: GREP_PARAMETERS,
        result: ToolResultShape::RecordList(&GREP_MATCH),
        description: "Search workspace file contents with a regular expression.",
    },
];

pub(crate) fn bootstrap_source() -> String {
    let records = record_definitions()
        .into_iter()
        .map(ToolRecord::bootstrap_definition)
        .collect::<Vec<_>>();
    let wrappers = ACTIVE_TOOLS
        .iter()
        .map(|definition| definition.bootstrap_wrapper())
        .collect::<Vec<_>>();
    [
        (!records.is_empty()).then(|| {
            format!(
                "from dataclasses import dataclass\n\n{}",
                records.join("\n\n")
            )
        }),
        (!wrappers.is_empty()).then(|| wrappers.join("\n\n")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

pub(crate) fn prompt_reference() -> String {
    let records = record_definitions()
        .into_iter()
        .map(ToolRecord::bootstrap_definition)
        .collect::<Vec<_>>();
    let definitions = ACTIVE_TOOLS
        .iter()
        .map(|definition| definition.prompt_skeleton())
        .collect::<Vec<_>>();
    let source = [
        (!records.is_empty()).then(|| {
            format!(
                "from dataclasses import dataclass\n\n{}",
                records.join("\n\n")
            )
        }),
        (!definitions.is_empty()).then(|| definitions.join("\n\n")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n");
    format!(
        "## Kiki tool reference\n\nDo not redefine these host functions. Every host call must be awaited. Use `asyncio.gather()` only for independent calls expected to succeed. Keep dependent calls sequential. Avoid unbounded fan-out. A gather failure can follow completed host calls and does not assign sibling results. Keep large files and intermediate data in Python variables. Keep model-visible output focused. The whole Arena execution has a five-minute deadline.\n\n```python\na, b = await asyncio.gather(read(\"a.rs\"), read(\"b.rs\"))\n```\n\n{source}\n"
    )
}

fn record_definitions() -> Vec<&'static ToolRecord> {
    let mut records: Vec<&'static ToolRecord> = Vec::new();
    for definition in ACTIVE_TOOLS {
        let Some(record) = definition.result.record() else {
            continue;
        };
        if !records.iter().any(|defined| defined.name == record.name) {
            records.push(record);
        }
    }
    records
}

impl ToolRecord {
    fn bootstrap_definition(&self) -> String {
        format!("@dataclass\nclass {}:\n{}", self.name, self.fields_source())
    }

    fn fields_source(&self) -> String {
        self.fields
            .iter()
            .map(|field| format!("    {}: {}", field.name, field.value_type.annotation()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl ToolDefinition {
    fn signature(self) -> String {
        let parameters = self
            .parameters
            .iter()
            .map(|parameter| {
                let default = parameter
                    .default
                    .map(|default| format!(" = {}", default.source()))
                    .unwrap_or_default();
                format!(
                    "{}: {}{default}",
                    parameter.name,
                    parameter.value_type.annotation()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "async def {}({parameters}) -> {}:",
            self.name.name(),
            self.result.annotation()
        )
    }

    fn arguments(self) -> String {
        self.parameters
            .iter()
            .map(|parameter| parameter.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn bootstrap_wrapper(self) -> String {
        format!(
            "{}\n    \"\"\"{}\"\"\"\n    return {}",
            self.signature(),
            self.description,
            self.result
                .bootstrap_return(self.name.private_name(), &self.arguments()),
        )
    }

    fn prompt_skeleton(self) -> String {
        format!(
            "{}\n    \"\"\"{}\"\"\"\n    ...",
            self.signature(),
            self.description,
        )
    }
}

#[derive(Clone, Copy)]
struct ToolLimits {
    max_file_bytes: usize,
    max_collection_items: usize,
    max_collection_payload_bytes: usize,
}

pub(crate) struct ToolHost {
    workspace: Arc<Dir>,
    limits: ToolLimits,
    #[cfg(test)]
    test_gate: Option<ReadTestGate>,
    #[cfg(test)]
    list_dir_test_gate: Option<ListDirTestGate>,
    #[cfg(test)]
    batch_test_gate: Option<BatchTestGate>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ReadTestGate {
    read_started: Arc<Semaphore>,
    read_allowed: Arc<Semaphore>,
    read_settled: Arc<Semaphore>,
    driver_allowed: Arc<Semaphore>,
}

#[cfg(test)]
impl ReadTestGate {
    pub(crate) fn new() -> Self {
        Self {
            read_started: Arc::new(Semaphore::new(0)),
            read_allowed: Arc::new(Semaphore::new(0)),
            read_settled: Arc::new(Semaphore::new(0)),
            driver_allowed: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_read_started(&self) {
        take_permit(&self.read_started).await;
    }

    pub(crate) fn resume_read(&self) {
        self.read_allowed.add_permits(1);
    }

    pub(crate) async fn wait_until_read_settled(&self) {
        take_permit(&self.read_settled).await;
    }

    pub(crate) fn resume_driver(&self) {
        self.driver_allowed.add_permits(1);
    }

    async fn wait_before_read(&self) {
        self.read_started.add_permits(1);
        take_permit(&self.read_allowed).await;
    }

    async fn wait_after_read_settled(&self) {
        self.read_settled.add_permits(1);
        take_permit(&self.driver_allowed).await;
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct BatchTestGate {
    state: Arc<Mutex<BatchTestGateState>>,
    started: Arc<Semaphore>,
    driver_settled: Arc<Semaphore>,
    changed: Arc<Notify>,
}

#[cfg(test)]
struct BatchTestGateState {
    active_call_ids: BTreeSet<u32>,
    started_call_ids: Vec<u32>,
    released_call_ids: BTreeSet<u32>,
    driver_settled_call_ids: Vec<u32>,
    maximum_active: usize,
    resume_futures_calls: usize,
}

#[cfg(test)]
impl BatchTestGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BatchTestGateState {
                active_call_ids: BTreeSet::new(),
                started_call_ids: Vec::new(),
                released_call_ids: BTreeSet::new(),
                driver_settled_call_ids: Vec::new(),
                maximum_active: 0,
                resume_futures_calls: 0,
            })),
            started: Arc::new(Semaphore::new(0)),
            driver_settled: Arc::new(Semaphore::new(0)),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(crate) async fn wait_until_calls_started(&self, count: usize) {
        for _ in 0..count {
            take_permit(&self.started).await;
        }
    }

    pub(crate) async fn wait_until_driver_settled(&self) {
        take_permit(&self.driver_settled).await;
    }

    pub(crate) fn driver_settled_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .driver_settled_call_ids
            .clone()
    }

    pub(crate) fn started_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .started_call_ids
            .clone()
    }

    pub(crate) fn active_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .active_call_ids
            .iter()
            .copied()
            .collect()
    }

    pub(crate) fn maximum_active(&self) -> usize {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .maximum_active
    }

    pub(crate) fn resume_futures_calls(&self) -> usize {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .resume_futures_calls
    }

    pub(crate) fn release(&self, call_id: u32) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .released_call_ids
            .insert(call_id);
        self.changed.notify_waiters();
    }

    fn note_driver_settled(&self, call_id: u32) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .driver_settled_call_ids
            .push(call_id);
        self.driver_settled.add_permits(1);
    }

    fn note_resume_futures(&self) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .resume_futures_calls += 1;
    }

    fn start_call(&self, call_id: u32) -> BatchTestCall {
        let mut state = self.state.lock().expect("test gate mutex is not poisoned");
        assert!(
            state.active_call_ids.insert(call_id),
            "test gate call IDs must be unique"
        );
        state.started_call_ids.push(call_id);
        state.maximum_active = state.maximum_active.max(state.active_call_ids.len());
        self.started.add_permits(1);
        BatchTestCall {
            gate: self.clone(),
            call_id,
        }
    }
}

#[cfg(test)]
struct BatchTestCall {
    gate: BatchTestGate,
    call_id: u32,
}

#[cfg(test)]
impl BatchTestCall {
    async fn wait_until_released(&self) {
        loop {
            let notified = self.gate.changed.notified();
            if self
                .gate
                .state
                .lock()
                .expect("test gate mutex is not poisoned")
                .released_call_ids
                .contains(&self.call_id)
            {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
impl Drop for BatchTestCall {
    fn drop(&mut self) {
        self.gate
            .state
            .lock()
            .expect("test gate mutex is not poisoned")
            .active_call_ids
            .remove(&self.call_id);
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
async fn take_permit(semaphore: &Arc<Semaphore>) {
    semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("test gate semaphore remains open")
        .forget();
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ListDirTestGate {
    state: Arc<(Mutex<ListDirTestGateState>, Condvar)>,
}

#[cfg(test)]
struct ListDirTestGateState {
    started: bool,
    allowed: bool,
}

#[cfg(test)]
impl ListDirTestGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(ListDirTestGateState {
                    started: false,
                    allowed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    pub(crate) async fn wait_until_list_dir_started(&self) {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            let (lock, changed) = &*state;
            let mut state = lock.lock().expect("test gate mutex is not poisoned");
            while !state.started {
                state = changed
                    .wait(state)
                    .expect("test gate mutex is not poisoned");
            }
        })
        .await
        .expect("test gate waiter completes");
    }

    pub(crate) fn resume_list_dir(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test gate mutex is not poisoned");
        state.allowed = true;
        changed.notify_all();
    }

    fn wait_before_list_dir(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test gate mutex is not poisoned");
        state.started = true;
        changed.notify_all();
        while !state.allowed {
            state = changed
                .wait(state)
                .expect("test gate mutex is not poisoned");
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ToolHostError {
    #[error("workspace operation failed")]
    Io(#[from] std::io::Error),
    #[error("blocking workspace operation failed")]
    Blocking(#[from] tokio::task::JoinError),
}

impl ToolHost {
    pub(crate) async fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolHostError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ToolLimits {
                max_file_bytes: FILE_MAX_BYTES,
                max_collection_items: COLLECTION_MAX_ITEMS,
                max_collection_payload_bytes: COLLECTION_MAX_PAYLOAD_BYTES,
            },
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits(
        workspace: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<Self, ToolHostError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ToolLimits {
                max_file_bytes: max_bytes,
                max_collection_items: COLLECTION_MAX_ITEMS,
                max_collection_payload_bytes: COLLECTION_MAX_PAYLOAD_BYTES,
            },
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn with_collection_limits(
        workspace: impl AsRef<Path>,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> Result<Self, ToolHostError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ToolLimits {
                max_file_bytes: FILE_MAX_BYTES,
                max_collection_items: max_items,
                max_collection_payload_bytes: max_payload_bytes,
            },
        )
        .await
    }

    async fn open(workspace: PathBuf, limits: ToolLimits) -> Result<Self, ToolHostError> {
        let workspace = tokio::task::spawn_blocking(move || {
            Dir::open_ambient_dir(workspace, ambient_authority()).map(Arc::new)
        })
        .await??;

        Ok(Self {
            workspace,
            limits,
            #[cfg(test)]
            test_gate: None,
            #[cfg(test)]
            list_dir_test_gate: None,
            #[cfg(test)]
            batch_test_gate: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_gate(mut self, test_gate: ReadTestGate) -> Self {
        self.test_gate = Some(test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_list_dir_test_gate(mut self, list_dir_test_gate: ListDirTestGate) -> Self {
        self.list_dir_test_gate = Some(list_dir_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_batch_test_gate(mut self, batch_test_gate: BatchTestGate) -> Self {
        self.batch_test_gate = Some(batch_test_gate);
        self
    }

    pub(crate) fn dispatch_batch(
        &self,
        calls: Vec<ToolCall>,
    ) -> impl Stream<Item = ToolResult> + '_ {
        stream::iter(calls)
            .map(|call| self.call(call))
            .buffer_unordered(TOOL_CONCURRENCY_LIMIT)
    }

    async fn call(&self, call: ToolCall) -> ToolResult {
        let ToolCall {
            call_id,
            tool,
            arguments,
        } = call;
        #[cfg(test)]
        let batch_call = self
            .batch_test_gate
            .as_ref()
            .map(|gate| gate.start_call(call_id));
        #[cfg(test)]
        if let Some(batch_call) = &batch_call {
            batch_call.wait_until_released().await;
        }
        let result = match tool {
            ToolName::Read => self.read(arguments).await,
            ToolName::ListDir => self.list_dir(arguments).await,
            ToolName::Glob => self.glob(arguments).await,
            ToolName::Grep => self.grep(arguments).await,
        };
        ToolResult { call_id, result }
    }

    async fn read(&self, arguments: ToolValue) -> Result<ToolValue, ToolError> {
        let path = read_path(arguments)?;
        #[cfg(test)]
        if let Some(test_gate) = &self.test_gate {
            test_gate.wait_before_read().await;
        }

        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || read_file(&workspace, &path, limits.max_file_bytes))
            .await
            .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
            .map(ToolValue::String)
    }

    async fn list_dir(&self, arguments: ToolValue) -> Result<ToolValue, ToolError> {
        let path = list_dir_path(arguments)?;
        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        #[cfg(test)]
        let test_gate = self.list_dir_test_gate.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(test_gate) = test_gate {
                test_gate.wait_before_list_dir();
            }
            list_directory(&workspace, &path, limits)
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
        .map(ToolValue::List)
    }

    async fn glob(&self, arguments: ToolValue) -> Result<ToolValue, ToolError> {
        let (pattern, path) = glob_arguments(arguments)?;
        let matcher = compile_glob(&pattern)?;
        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || glob_directory(&workspace, &path, matcher, limits))
            .await
            .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
            .map(ToolValue::List)
    }

    async fn grep(&self, arguments: ToolValue) -> Result<ToolValue, ToolError> {
        let (pattern, path, file_glob) = grep_arguments(arguments)?;
        let matcher = compile_regex(&pattern)?;
        let file_glob = match file_glob {
            Some(file_glob) => compile_glob(&file_glob)?,
            None => compile_glob("*")?,
        };
        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || {
            grep_workspace(&workspace, &path, matcher, file_glob, limits)
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
        .map(ToolValue::List)
    }

    #[cfg(test)]
    pub(crate) fn note_batch_driver_settled(&self, call_id: u32) {
        if let Some(batch_test_gate) = &self.batch_test_gate {
            batch_test_gate.note_driver_settled(call_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn note_batch_driver_resume_futures(&self) {
        if let Some(batch_test_gate) = &self.batch_test_gate {
            batch_test_gate.note_resume_futures();
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_after_read_settled(&self) {
        if let Some(test_gate) = &self.test_gate {
            test_gate.wait_after_read_settled().await;
        }
    }
}

fn read_path(arguments: ToolValue) -> Result<PathBuf, ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "read() expects one positional string argument",
        ));
    };
    let [ToolValue::String(path)] = arguments.as_slice() else {
        return Err(ToolError::type_error(
            "read() expects one positional string argument",
        ));
    };
    normalise_path(path)
}

fn list_dir_path(arguments: ToolValue) -> Result<PathBuf, ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "list_dir() expects zero or one positional string argument",
        ));
    };
    match arguments.as_slice() {
        [] => Ok(PathBuf::new()),
        [ToolValue::String(path)] => normalise_path(path),
        _ => Err(ToolError::type_error(
            "list_dir() expects zero or one positional string argument",
        )),
    }
}

fn glob_arguments(arguments: ToolValue) -> Result<(String, PathBuf), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "glob() expects one or two positional string arguments",
        ));
    };
    match arguments.as_slice() {
        [ToolValue::String(pattern)] => Ok((pattern.clone(), PathBuf::new())),
        [ToolValue::String(pattern), ToolValue::String(path)] => {
            Ok((pattern.clone(), normalise_path(path)?))
        }
        _ => Err(ToolError::type_error(
            "glob() expects one or two positional string arguments",
        )),
    }
}

fn grep_arguments(arguments: ToolValue) -> Result<(String, PathBuf, Option<String>), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "grep() expects one to three positional arguments: string pattern, string path, optional string file_glob",
        ));
    };
    match arguments.as_slice() {
        [ToolValue::String(pattern)] => Ok((pattern.clone(), PathBuf::new(), None)),
        [ToolValue::String(pattern), ToolValue::String(path)] => {
            Ok((pattern.clone(), normalise_path(path)?, None))
        }
        [
            ToolValue::String(pattern),
            ToolValue::String(path),
            ToolValue::None,
        ] => Ok((pattern.clone(), normalise_path(path)?, None)),
        [
            ToolValue::String(pattern),
            ToolValue::String(path),
            ToolValue::String(file_glob),
        ] => Ok((
            pattern.clone(),
            normalise_path(path)?,
            Some(file_glob.clone()),
        )),
        _ => Err(ToolError::type_error(
            "grep() expects one to three positional arguments: string pattern, string path, optional string file_glob",
        )),
    }
}

fn normalise_path(path: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(path);
    let mut normalised = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalised.push(component),
            Component::ParentDir => {
                return Err(ToolError::permission(
                    "workspace paths cannot contain parent components",
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(ToolError::permission("workspace paths must be relative"));
            }
        }
    }
    Ok(normalised)
}

fn read_file(workspace: &Dir, path: &Path, max_bytes: usize) -> Result<String, ToolError> {
    let (parent, file_name) = open_parent(workspace, path)?;
    let mut file = open_content_no_follow(&parent, &file_name)?;
    let metadata = file.metadata().map_err(ToolError::from_io)?;
    if metadata.is_dir() {
        return Err(ToolError::new(
            ToolErrorKind::IsADirectory,
            "path is a directory",
        ));
    }
    if !metadata.is_file() {
        return Err(ToolError::value("path is not a regular file"));
    }

    String::from_utf8(read_limited(&mut file, max_bytes)?)
        .map_err(|_| ToolError::value("file is not valid UTF-8"))
}

fn list_directory(
    workspace: &Dir,
    path: &Path,
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let directory = open_directory_root(workspace, path)?;
    let base_path = normalised_path_string(path);
    let mut entries = Vec::new();

    for entry in directory.entries().map_err(ToolError::from_io)? {
        let entry = entry.map_err(ToolError::from_io)?;
        let file_name = entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| ToolError::value("directory entry name is not valid UTF-8"))?;
        let metadata = directory
            .symlink_metadata(&file_name)
            .map_err(ToolError::from_io)?;
        let (kind, size_bytes) = if metadata.file_type().is_symlink() {
            ("symlink", None)
        } else if metadata.is_file() {
            (
                "file",
                Some(
                    i64::try_from(metadata.len())
                        .map_err(|_| ToolError::new(ToolErrorKind::Os, "file size is too large"))?,
                ),
            )
        } else if metadata.is_dir() {
            ("directory", None)
        } else {
            ("other", None)
        };
        let path = if base_path.is_empty() {
            name.to_owned()
        } else {
            format!("{base_path}/{name}")
        };
        entries.push((path, kind, size_bytes));
        if entries.len() > limits.max_collection_items {
            return Err(ToolError::runtime(format!(
                "directory listing exceeds the {}-entry limit",
                limits.max_collection_items
            )));
        }
    }

    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut payload_bytes = 0;
    entries
        .into_iter()
        .map(|(path, kind, size_bytes)| {
            payload_bytes += path.len() + kind.len();
            if payload_bytes > limits.max_collection_payload_bytes {
                return Err(ToolError::runtime(format!(
                    "directory listing exceeds the {}-byte payload limit",
                    limits.max_collection_payload_bytes
                )));
            }
            Ok(ToolValue::Dictionary(BTreeMap::from([
                ("path".to_owned(), ToolValue::String(path)),
                ("kind".to_owned(), ToolValue::String(kind.to_owned())),
                (
                    "size_bytes".to_owned(),
                    size_bytes.map_or(ToolValue::None, ToolValue::Integer),
                ),
            ])))
        })
        .collect()
}

fn compile_glob(pattern: &str) -> Result<(GlobMatcher, bool), ToolError> {
    let mut builder = GlobBuilder::new(pattern);
    let glob = builder
        .literal_separator(true)
        .case_insensitive(false)
        .build()
        .map_err(|_| ToolError::value("glob pattern is invalid"))?;
    Ok((glob.compile_matcher(), !pattern.contains('/')))
}

fn compile_regex(pattern: &str) -> Result<RegexMatcher, ToolError> {
    let mut builder = RegexMatcherBuilder::new();
    builder.multi_line(true).crlf(true).ban_byte(Some(b'\0'));
    builder
        .build(pattern)
        .map_err(|_| ToolError::value("regular expression is invalid"))
}

#[derive(Clone)]
struct IgnoreMatchers {
    rgignore: Gitignore,
    ignore: Gitignore,
    gitignore: Gitignore,
}

struct SearchDirectory {
    directory: Dir,
    path: PathBuf,
    ignores: Vec<IgnoreMatchers>,
}

fn root_ignore_context(
    workspace: &Dir,
    root: &Path,
    root_directory: &Dir,
) -> Result<(PathBuf, Vec<IgnoreMatchers>), ToolError> {
    let mut ignore_directory = workspace.try_clone().map_err(ToolError::from_io)?;
    let mut components = root.components().peekable();
    let mut root_path = PathBuf::new();
    let mut ignores = vec![directory_ignore_matchers(&ignore_directory, &root_path)?];
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            unreachable!("search roots are normalised before traversal");
        };
        root_path.push(component);
        ignore_directory = if components.peek().is_some() {
            open_directory_no_follow(&ignore_directory, Path::new(component))?
        } else {
            root_directory.try_clone().map_err(ToolError::from_io)?
        };
        ignores.push(directory_ignore_matchers(&ignore_directory, &root_path)?);
    }
    Ok((root_path, ignores))
}

fn glob_directory(
    workspace: &Dir,
    root: &Path,
    matcher: (GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let (matcher, basename_only) = matcher;
    let root_directory = open_directory_root(workspace, root)?;
    let (root_path, ignores) = root_ignore_context(workspace, root, &root_directory)?;

    let mut directories = vec![SearchDirectory {
        directory: root_directory,
        path: root_path,
        ignores,
    }];
    let mut entries = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in directory.directory.entries().map_err(ToolError::from_io)? {
            let entry = entry.map_err(ToolError::from_io)?;
            let name = entry.file_name();
            let path = directory.path.join(&name);
            let metadata = directory
                .directory
                .symlink_metadata(&name)
                .map_err(ToolError::from_io)?;
            if is_hidden_name(&name) || is_ignored(&directory.ignores, &path, metadata.is_dir()) {
                continue;
            }
            let name = name
                .to_str()
                .ok_or_else(|| ToolError::value("directory entry name is not valid UTF-8"))?;
            let relative_path = path
                .strip_prefix(root)
                .expect("glob entries remain beneath their requested root");
            if matches_glob(&matcher, basename_only, name, relative_path) {
                if entries.len() == limits.max_collection_items {
                    return Err(ToolError::runtime(format!(
                        "glob results exceed the {}-entry limit",
                        limits.max_collection_items
                    )));
                }
                entries.push((normalised_path_string(&path), entry_kind(&metadata)?));
            }
            if metadata.is_dir() {
                let child = open_directory_no_follow(&directory.directory, Path::new(name))?;
                let mut child_ignores = directory.ignores.clone();
                child_ignores.push(directory_ignore_matchers(&child, &path)?);
                directories.push(SearchDirectory {
                    directory: child,
                    path,
                    ignores: child_ignores,
                });
            }
        }
    }

    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut payload_bytes = 0;
    entries
        .into_iter()
        .map(|(path, (kind, size_bytes))| {
            payload_bytes += path.len() + kind.len();
            if payload_bytes > limits.max_collection_payload_bytes {
                return Err(ToolError::runtime(format!(
                    "glob results exceed the {}-byte payload limit",
                    limits.max_collection_payload_bytes
                )));
            }
            Ok(directory_entry_value(path, kind, size_bytes))
        })
        .collect()
}

#[derive(Debug)]
struct GrepMatchValue {
    path: String,
    line: i64,
    text: String,
}

enum GrepFileFailure {
    Binary,
    InvalidUtf8,
    NotRegular,
    Tool(ToolError),
}

enum GrepSearchError {
    Io(std::io::Error),
    InvalidUtf8,
    Other,
}

impl SinkError for GrepSearchError {
    fn error_message<T: std::fmt::Display>(_message: T) -> Self {
        Self::Other
    }

    fn error_io(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct GrepSink {
    path: String,
    matches: Vec<GrepMatchValue>,
    payload_bytes: usize,
    max_items: usize,
    max_payload_bytes: usize,
    overflowed: bool,
    binary: bool,
}

impl GrepSink {
    fn new(path: String, max_items: usize, max_payload_bytes: usize) -> Self {
        Self {
            path,
            matches: Vec::new(),
            payload_bytes: 0,
            max_items,
            max_payload_bytes,
            overflowed: false,
            binary: false,
        }
    }
}

impl Sink for GrepSink {
    type Error = GrepSearchError;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.matches.len() == self.max_items {
            self.overflowed = true;
            return Ok(true);
        }
        let text = matched
            .bytes()
            .strip_suffix(b"\n")
            .and_then(|text| text.strip_suffix(b"\r").or(Some(text)))
            .unwrap_or(matched.bytes());
        let text = std::str::from_utf8(text).map_err(|_| GrepSearchError::InvalidUtf8)?;
        let Some(payload_bytes) = self
            .payload_bytes
            .checked_add(self.path.len())
            .and_then(|bytes| bytes.checked_add(text.len()))
            .filter(|bytes| *bytes <= self.max_payload_bytes)
        else {
            self.overflowed = true;
            return Ok(true);
        };
        let line = matched.line_number().ok_or(GrepSearchError::Other)?;
        let line = i64::try_from(line).map_err(|_| GrepSearchError::Other)?;
        self.matches.push(GrepMatchValue {
            path: self.path.clone(),
            line,
            text: text.to_owned(),
        });
        self.payload_bytes = payload_bytes;
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

struct Utf8ValidatingReader<R> {
    reader: R,
    pending: Vec<u8>,
    invalid_utf8: bool,
}

impl<R> Utf8ValidatingReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            invalid_utf8: false,
        }
    }

    fn invalid_utf8_error() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "file is not valid UTF-8")
    }
}

impl<R: Read> Read for Utf8ValidatingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let count = self.reader.read(buffer)?;
        if count == 0 {
            if self.pending.is_empty() {
                return Ok(0);
            }
            self.invalid_utf8 = true;
            return Err(Self::invalid_utf8_error());
        }

        self.pending.extend_from_slice(&buffer[..count]);
        match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.clear(),
            Err(error) if error.error_len().is_some() => {
                self.invalid_utf8 = true;
                return Err(Self::invalid_utf8_error());
            }
            Err(error) => {
                self.pending.drain(..error.valid_up_to());
            }
        }
        Ok(count)
    }
}

fn grep_workspace(
    workspace: &Dir,
    root: &Path,
    matcher: RegexMatcher,
    file_glob: (GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    if root.as_os_str().is_empty() {
        return grep_directory(workspace, root, &matcher, &file_glob, limits);
    }

    let (parent, file_name) = open_parent(workspace, root)?;
    let name = file_name
        .to_str()
        .expect("normalised paths originate from UTF-8 tool arguments");
    let file_matches = matches_glob(&file_glob.0, file_glob.1, name, Path::new(name));
    if !file_matches && is_regular_no_follow(&parent, &file_name)? {
        return Ok(Vec::new());
    }

    let file = open_content_no_follow(&parent, &file_name)?;
    let metadata = file.metadata().map_err(ToolError::from_io)?;
    if metadata.is_dir() {
        return grep_open_directory(
            workspace,
            root,
            Dir::from_std_file(file.into_std()),
            &matcher,
            &file_glob,
            limits,
        );
    }
    if !metadata.is_file() {
        return Err(ToolError::value("path is not a regular file"));
    }

    if !file_matches {
        return Ok(Vec::new());
    }
    grep_open_file(file, normalised_path_string(root), &matcher, limits, 0, 0)
        .map_err(grep_explicit_error)
        .map(grep_values)
}

fn grep_directory(
    workspace: &Dir,
    root: &Path,
    matcher: &RegexMatcher,
    file_glob: &(GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let root_directory = open_directory_root(workspace, root)?;
    grep_open_directory(workspace, root, root_directory, matcher, file_glob, limits)
}

fn grep_open_directory(
    workspace: &Dir,
    root: &Path,
    root_directory: Dir,
    matcher: &RegexMatcher,
    file_glob: &(GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let (root_path, ignores) = root_ignore_context(workspace, root, &root_directory)?;

    let mut directories = vec![SearchDirectory {
        directory: root_directory,
        path: root_path,
        ignores,
    }];
    let mut matches = Vec::new();
    let mut payload_bytes = 0;
    while let Some(directory) = directories.pop() {
        for entry in directory.directory.entries().map_err(ToolError::from_io)? {
            let entry = entry.map_err(ToolError::from_io)?;
            let name = entry.file_name();
            let path = directory.path.join(&name);
            let metadata = directory
                .directory
                .symlink_metadata(&name)
                .map_err(ToolError::from_io)?;
            if is_hidden_name(&name) || is_ignored(&directory.ignores, &path, metadata.is_dir()) {
                continue;
            }
            let name = name
                .to_str()
                .ok_or_else(|| ToolError::value("directory entry name is not valid UTF-8"))?;
            if metadata.is_file() {
                let relative_path = path
                    .strip_prefix(root)
                    .expect("grep entries remain beneath their requested root");
                if matches_glob(&file_glob.0, file_glob.1, name, relative_path) {
                    let file_matches = match grep_regular_file(
                        &directory.directory,
                        Path::new(name),
                        normalised_path_string(&path),
                        matcher,
                        limits,
                        matches.len(),
                        payload_bytes,
                    ) {
                        Ok(matches) => matches,
                        Err(
                            GrepFileFailure::Binary
                            | GrepFileFailure::InvalidUtf8
                            | GrepFileFailure::NotRegular,
                        ) => continue,
                        Err(GrepFileFailure::Tool(error)) => return Err(error),
                    };
                    payload_bytes += file_matches
                        .iter()
                        .map(|matched| matched.path.len() + matched.text.len())
                        .sum::<usize>();
                    matches.extend(file_matches);
                }
            } else if metadata.is_dir() {
                let child = open_directory_no_follow(&directory.directory, Path::new(name))?;
                let mut child_ignores = directory.ignores.clone();
                child_ignores.push(directory_ignore_matchers(&child, &path)?);
                directories.push(SearchDirectory {
                    directory: child,
                    path,
                    ignores: child_ignores,
                });
            }
        }
    }

    matches.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    Ok(grep_values(matches))
}

fn grep_regular_file(
    parent: &Dir,
    name: &Path,
    path: String,
    matcher: &RegexMatcher,
    limits: ToolLimits,
    existing_items: usize,
    existing_payload_bytes: usize,
) -> Result<Vec<GrepMatchValue>, GrepFileFailure> {
    let file = open_content_no_follow(parent, name).map_err(GrepFileFailure::Tool)?;
    grep_open_file(
        file,
        path,
        matcher,
        limits,
        existing_items,
        existing_payload_bytes,
    )
}

fn grep_open_file(
    mut file: File,
    path: String,
    matcher: &RegexMatcher,
    limits: ToolLimits,
    existing_items: usize,
    existing_payload_bytes: usize,
) -> Result<Vec<GrepMatchValue>, GrepFileFailure> {
    if !file
        .metadata()
        .map_err(ToolError::from_io)
        .map_err(GrepFileFailure::Tool)?
        .is_file()
    {
        return Err(GrepFileFailure::NotRegular);
    }

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder
        .line_terminator(LineTerminator::crlf())
        .binary_detection(BinaryDetection::quit(b'\0'))
        .bom_sniffing(false);
    let mut searcher = searcher_builder.build();
    let mut reader = Utf8ValidatingReader::new(&mut file);
    let mut sink = GrepSink::new(
        path,
        limits.max_collection_items.saturating_sub(existing_items),
        limits
            .max_collection_payload_bytes
            .saturating_sub(existing_payload_bytes),
    );
    let result = searcher.search_reader(matcher, &mut reader, &mut sink);
    if sink.binary {
        return Err(GrepFileFailure::Binary);
    }
    match result {
        Ok(()) if sink.overflowed => Err(GrepFileFailure::Tool(ToolError::runtime(format!(
            "grep results exceed the {}-match or {}-byte payload limit",
            limits.max_collection_items, limits.max_collection_payload_bytes
        )))),
        Ok(()) => Ok(sink.matches),
        Err(GrepSearchError::InvalidUtf8) => Err(GrepFileFailure::InvalidUtf8),
        Err(GrepSearchError::Io(_)) if reader.invalid_utf8 => Err(GrepFileFailure::InvalidUtf8),
        Err(GrepSearchError::Io(error)) => Err(GrepFileFailure::Tool(ToolError::from_io(error))),
        Err(GrepSearchError::Other) => Err(GrepFileFailure::Tool(ToolError::new(
            ToolErrorKind::Os,
            "regular expression search failed",
        ))),
    }
}

fn grep_explicit_error(error: GrepFileFailure) -> ToolError {
    match error {
        GrepFileFailure::Binary => ToolError::value("file contains binary data"),
        GrepFileFailure::InvalidUtf8 => ToolError::value("file is not valid UTF-8"),
        GrepFileFailure::NotRegular => ToolError::value("path is not a regular file"),
        GrepFileFailure::Tool(error) => error,
    }
}

fn grep_values(matches: Vec<GrepMatchValue>) -> Vec<ToolValue> {
    matches
        .into_iter()
        .map(|matched| {
            ToolValue::Dictionary(BTreeMap::from([
                ("path".to_owned(), ToolValue::String(matched.path)),
                ("line".to_owned(), ToolValue::Integer(matched.line)),
                ("text".to_owned(), ToolValue::String(matched.text)),
            ]))
        })
        .collect()
}

fn matches_glob(
    matcher: &GlobMatcher,
    basename_only: bool,
    name: &str,
    relative_path: &Path,
) -> bool {
    if basename_only {
        matcher.is_match(name)
    } else {
        matcher.is_match(normalised_path_string(relative_path))
    }
}

fn is_hidden_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

fn is_ignored(matchers: &[IgnoreMatchers], path: &Path, is_dir: bool) -> bool {
    if let Some(ignored) = ignored_by_family(
        matchers.iter().rev().map(|matchers| &matchers.rgignore),
        path,
        is_dir,
    ) {
        return ignored;
    }
    if let Some(ignored) = ignored_by_family(
        matchers.iter().rev().map(|matchers| &matchers.ignore),
        path,
        is_dir,
    ) {
        return ignored;
    }
    ignored_by_family(
        matchers.iter().rev().map(|matchers| &matchers.gitignore),
        path,
        is_dir,
    )
    .unwrap_or(false)
}

fn ignored_by_family<'a>(
    matchers: impl Iterator<Item = &'a Gitignore>,
    path: &Path,
    is_dir: bool,
) -> Option<bool> {
    for matcher in matchers {
        let matched = matcher.matched(path, is_dir);
        if matched.is_ignore() {
            return Some(true);
        }
        if matched.is_whitelist() {
            return Some(false);
        }
    }
    None
}

fn directory_ignore_matchers(directory: &Dir, path: &Path) -> Result<IgnoreMatchers, ToolError> {
    Ok(IgnoreMatchers {
        rgignore: directory_ignore_matcher(directory, path, ".rgignore")?,
        ignore: directory_ignore_matcher(directory, path, ".ignore")?,
        gitignore: directory_ignore_matcher(directory, path, ".gitignore")?,
    })
}

fn directory_ignore_matcher(
    directory: &Dir,
    path: &Path,
    name: &str,
) -> Result<Gitignore, ToolError> {
    let mut builder = GitignoreBuilder::new(path);
    if let Some(contents) = read_ignore_file(directory, Path::new(name))? {
        let contents = String::from_utf8(contents)
            .map_err(|_| ToolError::value("project ignore rules are not valid UTF-8"))?;
        let source = path.join(name);
        for (index, line) in contents.lines().enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            builder
                .add_line(Some(source.clone()), line)
                .map_err(|_| ToolError::value("project ignore rules are invalid"))?;
        }
    }
    builder
        .build()
        .map_err(|_| ToolError::value("project ignore rules are invalid"))
}

fn read_ignore_file(directory: &Dir, name: &Path) -> Result<Option<Vec<u8>>, ToolError> {
    let mut file = match open_content_no_follow(directory, name) {
        Ok(file) => file,
        Err(error) if error.kind == ToolErrorKind::FileNotFound => return Ok(None),
        Err(error) if error.kind == ToolErrorKind::Value => return Ok(None),
        Err(error) if error.kind == ToolErrorKind::Permission && is_symlink(directory, name)? => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !file.metadata().map_err(ToolError::from_io)?.is_file() {
        return Ok(None);
    }

    Ok(Some(read_limited(&mut file, FILE_MAX_BYTES)?))
}

fn entry_kind(metadata: &Metadata) -> Result<(&'static str, Option<i64>), ToolError> {
    if metadata.file_type().is_symlink() {
        Ok(("symlink", None))
    } else if metadata.is_file() {
        Ok((
            "file",
            Some(
                i64::try_from(metadata.len())
                    .map_err(|_| ToolError::new(ToolErrorKind::Os, "file size is too large"))?,
            ),
        ))
    } else if metadata.is_dir() {
        Ok(("directory", None))
    } else {
        Ok(("other", None))
    }
}

fn directory_entry_value(path: String, kind: &str, size_bytes: Option<i64>) -> ToolValue {
    ToolValue::Dictionary(BTreeMap::from([
        ("path".to_owned(), ToolValue::String(path)),
        ("kind".to_owned(), ToolValue::String(kind.to_owned())),
        (
            "size_bytes".to_owned(),
            size_bytes.map_or(ToolValue::None, ToolValue::Integer),
        ),
    ]))
}

fn normalised_path_string(path: &Path) -> String {
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .expect("normalised paths originate from UTF-8 tool arguments")
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn open_directory_root(workspace: &Dir, path: &Path) -> Result<Dir, ToolError> {
    let (parent, file_name) = open_parent(workspace, path)?;
    if path.as_os_str().is_empty() {
        return Ok(parent);
    }
    open_directory_no_follow(&parent, &file_name)
}

fn open_parent(workspace: &Dir, path: &Path) -> Result<(Dir, PathBuf), ToolError> {
    let mut components = path.components().peekable();
    let mut directory = workspace.try_clone().map_err(ToolError::from_io)?;
    if components.peek().is_none() {
        return Ok((directory, PathBuf::from(".")));
    }
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            unreachable!("paths are normalised before component traversal");
        };
        let component = PathBuf::from(component);
        if components.peek().is_none() {
            return Ok((directory, component));
        }
        directory = open_directory_no_follow(&directory, &component)?;
    }
    unreachable!("normalised paths contain at least one component")
}

#[cfg(unix)]
fn open_content_no_follow(parent: &Dir, name: &Path) -> Result<File, ToolError> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(|descriptor| File::from_std(std::fs::File::from(descriptor)))
    .map_err(|error| content_open_error(parent, name, error))
}

#[cfg(not(unix))]
fn open_content_no_follow(parent: &Dir, name: &Path) -> Result<File, ToolError> {
    parent.open(name).map_err(ToolError::from_io)
}

#[cfg(unix)]
fn open_directory_no_follow(parent: &Dir, name: &Path) -> Result<Dir, ToolError> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(|descriptor| Dir::from_std_file(std::fs::File::from(descriptor)))
    .map_err(|error| directory_open_error(parent, name, error))
}

#[cfg(unix)]
fn directory_open_error(parent: &Dir, name: &Path, error: Errno) -> ToolError {
    if matches!(error, Errno::LOOP | Errno::MLINK) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    let io_error = std::io::Error::from(error);
    if io_error.kind() != std::io::ErrorKind::PermissionDenied
        && matches!(
            statat(parent, name, AtFlags::SYMLINK_NOFOLLOW),
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_symlink()
        )
    {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    ToolError::from_io(io_error)
}

#[cfg(unix)]
fn content_open_error(parent: &Dir, name: &Path, error: Errno) -> ToolError {
    if matches!(error, Errno::LOOP | Errno::MLINK) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    let io_error = std::io::Error::from(error);
    if matches!(
        io_error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::PermissionDenied
    ) {
        return ToolError::from_io(io_error);
    }
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let file_type = FileType::from_raw_mode(stat.st_mode);
            if file_type.is_symlink() {
                ToolError::permission("workspace paths cannot traverse symbolic links")
            } else if file_type.is_dir() {
                ToolError::new(ToolErrorKind::IsADirectory, "path is a directory")
            } else if !file_type.is_file() {
                ToolError::value("path is not a regular file")
            } else {
                ToolError::from_io(io_error)
            }
        }
        Err(_) => ToolError::from_io(io_error),
    }
}

#[cfg(unix)]
fn is_regular_no_follow(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileType::from_raw_mode(stat.st_mode).is_file())
        .map_err(no_follow_io)
}

#[cfg(unix)]
fn is_symlink(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileType::from_raw_mode(stat.st_mode).is_symlink())
        .map_err(no_follow_io)
}

#[cfg(unix)]
fn no_follow_io(error: Errno) -> ToolError {
    if matches!(error, Errno::LOOP | Errno::MLINK) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    ToolError::from_io(error.into())
}

#[cfg(not(unix))]
fn open_directory_no_follow(parent: &Dir, name: &Path) -> Result<Dir, ToolError> {
    let metadata = parent.symlink_metadata(name).map_err(ToolError::from_io)?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::permission(
            "workspace paths cannot traverse symbolic links",
        ));
    }
    parent.open_dir(name).map_err(ToolError::from_io)
}

#[cfg(not(unix))]
fn is_regular_no_follow(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    parent
        .symlink_metadata(name)
        .map(|metadata| metadata.is_file())
        .map_err(ToolError::from_io)
}

#[cfg(not(unix))]
fn is_symlink(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    parent
        .symlink_metadata(name)
        .map(|metadata| metadata.file_type().is_symlink())
        .map_err(ToolError::from_io)
}

fn read_limited(file: &mut File, max_bytes: usize) -> Result<Vec<u8>, ToolError> {
    let mut contents = Vec::new();
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(ToolError::from_io)?;
    if contents.len() > max_bytes {
        return Err(ToolError::value(format!(
            "file exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    #[cfg(unix)]
    use cap_std::{ambient_authority, fs::Dir};
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    use futures::StreamExt;
    use tempfile::TempDir;

    use crate::config::constants::FILE_MAX_BYTES;

    use super::{
        ACTIVE_TOOLS, ToolCall, ToolError, ToolErrorKind, ToolHost, ToolName, ToolValue,
        Utf8ValidatingReader, bootstrap_source, open_content_no_follow, open_parent,
        prompt_reference,
    };

    async fn tool_host(workspace: &TempDir) -> ToolHost {
        ToolHost::new(workspace.path()).await.unwrap()
    }

    async fn tool_host_with_limits(workspace: &TempDir, max_bytes: usize) -> ToolHost {
        ToolHost::with_limits(workspace.path(), max_bytes)
            .await
            .unwrap()
    }

    async fn tool_host_with_collection_limits(
        workspace: &TempDir,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> ToolHost {
        ToolHost::with_collection_limits(workspace.path(), max_items, max_payload_bytes)
            .await
            .unwrap()
    }

    async fn read(host: &ToolHost, path: &str) -> Result<String, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::Read,
                arguments: ToolValue::List(vec![ToolValue::String(path.to_owned())]),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::String(contents)) => Ok(contents),
            Ok(value) => panic!("read returned {value:?} instead of a string"),
            Err(error) => Err(error.kind),
        }
    }

    async fn read_error(host: &ToolHost, path: &str) -> ToolErrorKind {
        read(host, path).await.unwrap_err()
    }

    async fn list_dir(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::ListDir,
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::List(entries)) => Ok(entries),
            Ok(value) => panic!("list_dir returned {value:?} instead of a list"),
            Err(error) => Err(error.kind),
        }
    }

    async fn list_dir_path(host: &ToolHost, path: &str) -> Result<Vec<ToolValue>, ToolErrorKind> {
        list_dir(host, vec![ToolValue::String(path.to_owned())]).await
    }

    async fn list_dir_error(host: &ToolHost, path: &str) -> ToolErrorKind {
        list_dir_path(host, path).await.unwrap_err()
    }

    fn glob_tool() -> ToolName {
        ToolName::from_private_name("_kiki_glob").expect("glob must be an active host tool")
    }

    fn grep_tool() -> ToolName {
        ToolName::from_private_name("_kiki_grep").expect("grep must be an active host tool")
    }

    async fn glob(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: glob_tool(),
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::List(entries)) => Ok(entries),
            Ok(value) => panic!("glob returned {value:?} instead of a list"),
            Err(error) => Err(error.kind),
        }
    }

    async fn glob_path(
        host: &ToolHost,
        pattern: &str,
        path: &str,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        glob(
            host,
            vec![
                ToolValue::String(pattern.to_owned()),
                ToolValue::String(path.to_owned()),
            ],
        )
        .await
    }

    async fn glob_error(host: &ToolHost, pattern: &str, path: &str) -> ToolErrorKind {
        glob_path(host, pattern, path).await.unwrap_err()
    }

    async fn glob_failure(host: &ToolHost, pattern: &str, path: &str) -> ToolError {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: glob_tool(),
                arguments: ToolValue::List(vec![
                    ToolValue::String(pattern.to_owned()),
                    ToolValue::String(path.to_owned()),
                ]),
            })
            .await;
        result.result.unwrap_err()
    }

    async fn grep(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: grep_tool(),
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::List(matches)) => Ok(matches),
            Ok(value) => panic!("grep returned {value:?} instead of a list"),
            Err(error) => Err(error.kind),
        }
    }

    async fn grep_path(
        host: &ToolHost,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let mut arguments = vec![
            ToolValue::String(pattern.to_owned()),
            ToolValue::String(path.to_owned()),
        ];
        if let Some(file_glob) = file_glob {
            arguments.push(ToolValue::String(file_glob.to_owned()));
        }
        grep(host, arguments).await
    }

    async fn grep_error(
        host: &ToolHost,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
    ) -> ToolErrorKind {
        grep_path(host, pattern, path, file_glob).await.unwrap_err()
    }

    fn directory_entry(path: &str, kind: &str, size_bytes: Option<i64>) -> ToolValue {
        ToolValue::Dictionary(BTreeMap::from([
            ("path".to_owned(), ToolValue::String(path.to_owned())),
            ("kind".to_owned(), ToolValue::String(kind.to_owned())),
            (
                "size_bytes".to_owned(),
                size_bytes.map_or(ToolValue::None, ToolValue::Integer),
            ),
        ]))
    }

    fn grep_match(path: &str, line: i64, text: &str) -> ToolValue {
        ToolValue::Dictionary(BTreeMap::from([
            ("path".to_owned(), ToolValue::String(path.to_owned())),
            ("line".to_owned(), ToolValue::Integer(line)),
            ("text".to_owned(), ToolValue::String(text.to_owned())),
        ]))
    }

    #[cfg(unix)]
    #[test]
    fn direct_content_open_refuses_symlinks_and_classifies_opened_descriptors() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "workspace").unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("file", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        let workspace_dir = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

        let mut file = open_content_no_follow(&workspace_dir, Path::new("file")).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "workspace");
        assert!(file.metadata().unwrap().is_file());

        let directory = open_content_no_follow(&workspace_dir, Path::new("directory")).unwrap();
        assert!(directory.metadata().unwrap().is_dir());

        let fifo = open_content_no_follow(&workspace_dir, Path::new("fifo")).unwrap();
        assert!(!fifo.metadata().unwrap().is_file());

        for name in ["inside-link", "outside-link", "broken-link"] {
            assert_eq!(
                open_content_no_follow(&workspace_dir, Path::new(name))
                    .unwrap_err()
                    .kind,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            open_content_no_follow(&workspace_dir, Path::new("missing"))
                .unwrap_err()
                .kind,
            ToolErrorKind::FileNotFound
        );
        assert_eq!(
            open_content_no_follow(&workspace_dir, Path::new("socket"))
                .unwrap_err()
                .kind,
            ToolErrorKind::Value
        );
    }

    #[cfg(unix)]
    #[test]
    fn direct_parent_open_refuses_intermediate_symlinks() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::write(workspace.path().join("inside/file"), "workspace").unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        let workspace_dir = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

        for path in ["inside-link/file", "outside-link/outside"] {
            assert_eq!(
                open_parent(&workspace_dir, Path::new(path))
                    .unwrap_err()
                    .kind,
                ToolErrorKind::Permission
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_open_stays_anchored_after_parent_rename_and_component_swaps() {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let parent_path = workspace.path().join("parent");
        fs::create_dir(&parent_path).unwrap();
        fs::write(parent_path.join("file"), "workspace").unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        let workspace_dir = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

        let (parent, file_name) = open_parent(&workspace_dir, Path::new("parent/file")).unwrap();
        fs::rename(&parent_path, workspace.path().join("held")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent_path).unwrap();
        let mut file = open_content_no_follow(&parent, &file_name).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "workspace");
        fs::remove_file(&parent_path).unwrap();
        fs::rename(workspace.path().join("held"), &parent_path).unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let swapping_workspace = workspace.path().to_path_buf();
        let swapping_outside = outside.path().to_path_buf();
        let swapping_done = Arc::clone(&done);
        let swaps = std::thread::spawn(move || {
            let parent = swapping_workspace.join("parent");
            let held = swapping_workspace.join("held");
            for _ in 0..1_000 {
                if fs::rename(&parent, &held).is_err() {
                    continue;
                }
                std::os::unix::fs::symlink(&swapping_outside, &parent).unwrap();
                std::thread::yield_now();
                fs::remove_file(&parent).unwrap();
                fs::rename(&held, &parent).unwrap();
            }
            swapping_done.store(true, Ordering::Release);
        });

        while !done.load(Ordering::Acquire) {
            let Ok((parent, file_name)) = open_parent(&workspace_dir, Path::new("parent/file"))
            else {
                continue;
            };
            let Ok(mut file) = open_content_no_follow(&parent, &file_name) else {
                continue;
            };
            let mut contents = String::new();
            file.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "workspace");
        }
        swaps.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_maps_direct_root_kinds_to_distinct_errors() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        let host = tool_host(&workspace).await;

        for path in ["missing"] {
            assert_eq!(
                list_dir_error(&host, path).await,
                ToolErrorKind::FileNotFound
            );
        }
        for path in ["file", "fifo", "socket"] {
            assert_eq!(
                list_dir_error(&host, path).await,
                ToolErrorKind::NotADirectory
            );
        }
        for path in ["inside-link", "outside-link", "broken-link"] {
            assert_eq!(list_dir_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_lists_sorted_immediate_entries_without_filtering() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("z"), "z").unwrap();
        fs::write(workspace.path().join("a-file"), "file").unwrap();
        fs::create_dir(workspace.path().join("b-directory")).unwrap();
        fs::write(workspace.path().join(".hidden"), "hidden").unwrap();
        fs::write(workspace.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(workspace.path().join("ignored.txt"), "ignored").unwrap();
        std::os::unix::fs::symlink("a-file", workspace.path().join("c-link")).unwrap();
        create_fifo(&workspace.path().join("d-fifo"));
        let host = tool_host(&workspace).await;

        let entries = tokio::time::timeout(Duration::from_millis(100), list_dir(&host, Vec::new()))
            .await
            .expect("listing a FIFO must not block")
            .unwrap();

        assert_eq!(
            entries,
            [
                directory_entry(".gitignore", "file", Some(12)),
                directory_entry(".hidden", "file", Some(6)),
                directory_entry("a-file", "file", Some(4)),
                directory_entry("b-directory", "directory", None),
                directory_entry("c-link", "symlink", None),
                directory_entry("d-fifo", "other", None),
                directory_entry("ignored.txt", "file", Some(7)),
                directory_entry("z", "file", Some(1)),
            ]
        );
    }

    #[tokio::test]
    async fn list_dir_normalises_workspace_relative_output_paths() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("nested/file"), "contents").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            list_dir_path(&host, "./nested//.").await.unwrap(),
            [directory_entry("nested/file", "file", Some(8))]
        );
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(list_dir_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_refuses_inside_and_outside_intermediate_symlinks() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        let host = tool_host(&workspace).await;

        for path in ["inside-link/file", "outside-link/file"] {
            assert_eq!(list_dir_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn list_dir_rejects_any_non_utf8_entry_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("valid"), "contents").unwrap();
        fs::write(
            workspace.path().join(OsStr::from_bytes(b"\xff")),
            "contents",
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(list_dir(&host, Vec::new()).await, Err(ToolErrorKind::Value));
    }

    #[tokio::test]
    async fn list_dir_enforces_exact_item_and_payload_limits_without_results() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("aa"), "").unwrap();
        fs::write(workspace.path().join("bb"), "").unwrap();

        let at_item_limit = tool_host_with_collection_limits(&workspace, 2, 12).await;
        assert_eq!(
            list_dir(&at_item_limit, Vec::new()).await.unwrap(),
            [
                directory_entry("aa", "file", Some(0)),
                directory_entry("bb", "file", Some(0)),
            ]
        );

        let over_item_limit = tool_host_with_collection_limits(&workspace, 1, 12).await;
        assert_eq!(
            list_dir(&over_item_limit, Vec::new()).await,
            Err(ToolErrorKind::Runtime)
        );

        let at_payload_limit = tool_host_with_collection_limits(&workspace, 2, 12).await;
        assert_eq!(
            list_dir(&at_payload_limit, Vec::new()).await.unwrap().len(),
            2
        );
        let over_payload_limit = tool_host_with_collection_limits(&workspace, 2, 11).await;
        assert_eq!(
            list_dir(&over_payload_limit, Vec::new()).await,
            Err(ToolErrorKind::Runtime)
        );
    }

    #[tokio::test]
    async fn list_dir_rejects_malformed_arguments_as_type_errors() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        for arguments in [
            ToolValue::None,
            ToolValue::Bool(true),
            ToolValue::Integer(1),
            ToolValue::Float(1.0),
            ToolValue::String("path".to_owned()),
            ToolValue::List(vec![ToolValue::None]),
            ToolValue::List(vec![ToolValue::String("a".to_owned()), ToolValue::None]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: ToolName::ListDir,
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_list_dir_does_not_stop_a_read_future() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let gate = super::ListDirTestGate::new();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_list_dir_test_gate(gate.clone()),
        );
        let list_host = Arc::clone(&host);
        let mut listing = tokio::spawn(async move { list_dir(&list_host, Vec::new()).await });

        gate.wait_until_list_dir_started().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), read(&host, "file"))
                .await
                .expect("read should make progress while listing is blocked")
                .unwrap(),
            "contents"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut listing)
                .await
                .is_err()
        );
        gate.resume_list_dir();
        assert_eq!(listing.await.unwrap().unwrap().len(), 1);
    }

    fn batch_read(call_id: u32, path: impl Into<String>) -> ToolCall {
        ToolCall {
            call_id,
            tool: ToolName::Read,
            arguments: ToolValue::List(vec![ToolValue::String(path.into())]),
        }
    }

    #[tokio::test]
    async fn dispatch_batch_yields_completion_order_with_correlated_call_ids() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("first"), "first").unwrap();
        fs::write(workspace.path().join("second"), "second").unwrap();
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls =
            Box::pin(host.dispatch_batch(vec![batch_read(1, "first"), batch_read(2, "second")]));

        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = calls.next() => panic!("batch finished before both calls started: {result:?}"),
        }
        gate.release(2);
        let second = calls.next().await.unwrap();
        assert_eq!(second.call_id, 2);
        assert_eq!(second.result, Ok(ToolValue::String("second".to_owned())));
        gate.release(1);
        let first = calls.next().await.unwrap();
        assert_eq!(first.call_id, 1);
        assert_eq!(first.result, Ok(ToolValue::String("first".to_owned())));
    }

    #[tokio::test]
    async fn dispatch_batch_limits_active_calls_and_admits_queued_calls() {
        let workspace = TempDir::new().unwrap();
        for call_id in 1..=17 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                (1..=17)
                    .map(|call_id| batch_read(call_id, format!("file-{call_id}")))
                    .collect(),
            ),
        );

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = calls.next() => panic!("batch finished before 16 calls started: {result:?}"),
        }
        assert_eq!(gate.active_call_ids().len(), 16);
        assert_eq!(gate.maximum_active(), 16);

        let released_call = *gate
            .started_call_ids()
            .first()
            .expect("a batch call must have started");
        gate.release(released_call);
        assert_eq!(calls.next().await.unwrap().call_id, released_call);
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = calls.next() => panic!("batch finished before the queued call started: {result:?}"),
        }
        assert_eq!(gate.maximum_active(), 16);

        for call_id in gate.active_call_ids() {
            gate.release(call_id);
        }
        let mut call_ids = calls.map(|result| result.call_id).collect::<Vec<_>>().await;
        call_ids.push(released_call);
        call_ids.sort_unstable();
        assert_eq!(call_ids, (1..=17).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn dispatch_batch_settles_queued_siblings_after_an_ordinary_failure() {
        let workspace = TempDir::new().unwrap();
        for call_id in 2..=17 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                std::iter::once(batch_read(1, "missing"))
                    .chain((2..=17).map(|call_id| batch_read(call_id, format!("file-{call_id}"))))
                    .collect(),
            ),
        );
        let mut results = Box::pin(calls.as_mut().collect::<Vec<_>>());

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut results => panic!("batch finished before 16 calls started: {result:?}"),
        }
        for call_id in gate.started_call_ids() {
            gate.release(call_id);
        }
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut results => panic!("batch finished before the queued sibling started: {result:?}"),
        }
        let queued_call = *gate
            .started_call_ids()
            .last()
            .expect("queued sibling must have started");
        gate.release(queued_call);

        let results = results.await;
        assert_eq!(results.len(), 17);
        assert!(matches!(
            results.iter().find(|result| result.call_id == 1),
            Some(result) if result.result.is_err()
        ));
        assert!(
            results
                .iter()
                .filter(|result| result.call_id != 1)
                .all(|result| result.result.is_ok())
        );
    }

    #[tokio::test]
    async fn dropping_a_partly_consumed_batch_drops_active_calls_and_leaves_calls_queued() {
        let workspace = TempDir::new().unwrap();
        for call_id in 1..=18 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                (1..=18)
                    .map(|call_id| batch_read(call_id, format!("file-{call_id}")))
                    .collect(),
            ),
        );

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = calls.next() => panic!("batch finished before 16 calls started: {result:?}"),
        }
        let released_call = *gate
            .started_call_ids()
            .first()
            .expect("a batch call must have started");
        gate.release(released_call);
        assert_eq!(calls.next().await.unwrap().call_id, released_call);
        let mut next_result = Box::pin(calls.next());
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut next_result => panic!("batch finished before another call started: {result:?}"),
        }
        drop(next_result);
        drop(calls);

        assert!(gate.active_call_ids().is_empty());
        assert_eq!(gate.started_call_ids(), (1..=17).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn test_gate_pauses_a_read_until_released() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("paused"), "contents").unwrap();
        let gate = super::ReadTestGate::new();
        let host = tool_host(&workspace).await.with_test_gate(gate.clone());
        let mut read = tokio::spawn(async move { read(&host, "paused").await });

        gate.wait_until_read_started().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut read)
                .await
                .is_err()
        );
        gate.resume_read();

        assert_eq!(read.await.unwrap().unwrap(), "contents");
    }

    #[tokio::test]
    async fn reads_one_exact_utf8_string_without_line_limits() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;
        let cases = [
            ("lf", "first\nsecond\n"),
            ("crlf", "first\r\nsecond\r\n"),
            ("no-final-newline", "first\nsecond"),
            ("empty", ""),
        ];

        for (name, contents) in cases {
            fs::write(workspace.path().join(name), contents).unwrap();
            assert_eq!(read(&host, name).await.unwrap(), contents);
        }

        let many_lines = "line\n".repeat(100_001);
        fs::write(workspace.path().join("many-lines"), &many_lines).unwrap();
        assert_eq!(read(&host, "many-lines").await.unwrap(), many_lines);
    }

    #[tokio::test]
    async fn normalises_lexical_aliases_and_denies_escapes() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("nested/file"), "contents").unwrap();
        let host = tool_host(&workspace).await;

        for path in ["nested/file", "./nested/file", "nested//./file"] {
            assert_eq!(read(&host, path).await.unwrap(), "contents");
        }
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(read_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_final_symlinks_inside_and_outside_the_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("inside"), "inside").unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        for path in ["inside-link", "outside-link"] {
            assert_eq!(read_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_intermediate_symlinks_inside_and_outside_the_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::write(workspace.path().join("inside/file"), "inside").unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        let host = tool_host(&workspace).await;

        for path in ["inside-link/file", "outside-link/file"] {
            assert_eq!(read_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[tokio::test]
    async fn maps_read_path_kinds_to_distinct_errors() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("invalid"), [0xff]).unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;
        fs::write(workspace.path().join("large"), "12345").unwrap();

        assert_eq!(
            read_error(&host, "missing").await,
            ToolErrorKind::FileNotFound
        );
        assert_eq!(
            read_error(&host, "directory").await,
            ToolErrorKind::IsADirectory
        );
        assert_eq!(read_error(&host, "invalid").await, ToolErrorKind::Value);
        assert_eq!(read_error(&host, "large").await, ToolErrorKind::Value);
    }

    #[cfg(unix)]
    fn create_fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_fifo_without_waiting_for_a_writer() {
        let workspace = TempDir::new().unwrap();
        let fifo = workspace.path().join("fifo");
        create_fifo(&fifo);
        let host = tool_host(&workspace).await;

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), read_error(&host, "fifo"))
                .await
                .unwrap(),
            ToolErrorKind::Value
        );
    }

    #[tokio::test]
    async fn enforces_the_exact_byte_boundary() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("at-limit"), "1234").unwrap();
        fs::write(workspace.path().join("over-limit"), "12345").unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;

        assert_eq!(read(&host, "at-limit").await.unwrap(), "1234");
        assert_eq!(read_error(&host, "over-limit").await, ToolErrorKind::Value);
    }

    #[tokio::test]
    async fn rejects_malformed_arguments_as_type_errors() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        for arguments in [
            ToolValue::None,
            ToolValue::Bool(true),
            ToolValue::Integer(1),
            ToolValue::Float(1.0),
            ToolValue::String("path".to_owned()),
            ToolValue::List(Vec::new()),
            ToolValue::List(vec![ToolValue::String("path".to_owned()), ToolValue::None]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: ToolName::Read,
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[tokio::test]
    async fn glob_rejects_invalid_patterns_and_malformed_arguments() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(glob_error(&host, "[", ".").await, ToolErrorKind::Value);
        for arguments in [
            ToolValue::None,
            ToolValue::String("*.rs".to_owned()),
            ToolValue::List(Vec::new()),
            ToolValue::List(vec![ToolValue::String("*.rs".to_owned()), ToolValue::None]),
            ToolValue::List(vec![
                ToolValue::String("*.rs".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::String("extra".to_owned()),
            ]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: glob_tool(),
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_maps_direct_root_kinds_and_intermediate_symlinks_to_errors() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_error(&host, "*", "missing").await,
            ToolErrorKind::FileNotFound
        );
        for path in ["file", "fifo", "socket"] {
            assert_eq!(
                glob_error(&host, "*", path).await,
                ToolErrorKind::NotADirectory
            );
        }
        for path in [
            "inside-link",
            "outside-link",
            "broken-link",
            "inside-link/file",
            "outside-link/file",
        ] {
            assert_eq!(
                glob_error(&host, "*", path).await,
                ToolErrorKind::Permission
            );
        }
    }

    #[tokio::test]
    async fn glob_matches_root_relative_and_basename_patterns() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("src/nested/deeper")).unwrap();
        fs::write(workspace.path().join("src/direct.rs"), "").unwrap();
        fs::write(workspace.path().join("src/nested/mod.rs"), "").unwrap();
        fs::write(workspace.path().join("src/nested/deeper/lib.rs"), "").unwrap();
        fs::write(workspace.path().join("src/nested/mod.py"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "nested/*.rs", "src").await.unwrap(),
            [directory_entry("src/nested/mod.rs", "file", Some(0))]
        );
        assert_eq!(
            glob_path(&host, "nested/deeper/*.rs", "src").await.unwrap(),
            [directory_entry("src/nested/deeper/lib.rs", "file", Some(0))]
        );
        assert_eq!(
            glob_path(&host, "*.rs", "src").await.unwrap(),
            [
                directory_entry("src/direct.rs", "file", Some(0)),
                directory_entry("src/nested/deeper/lib.rs", "file", Some(0)),
                directory_entry("src/nested/mod.rs", "file", Some(0)),
            ]
        );
    }

    #[tokio::test]
    async fn glob_filters_hidden_descendants_but_traverses_an_explicit_hidden_root() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join(".hidden/nested")).unwrap();
        fs::create_dir(workspace.path().join("visible")).unwrap();
        fs::write(workspace.path().join(".hidden/kept.rs"), "").unwrap();
        fs::write(workspace.path().join(".hidden/nested/.hidden.rs"), "").unwrap();
        fs::write(workspace.path().join("visible/.hidden.rs"), "").unwrap();
        fs::write(workspace.path().join("visible/kept.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "*.rs", ".").await.unwrap(),
            [directory_entry("visible/kept.rs", "file", Some(0))]
        );
        assert_eq!(
            glob_path(&host, "*.rs", ".hidden").await.unwrap(),
            [directory_entry(".hidden/kept.rs", "file", Some(0))]
        );
    }

    #[tokio::test]
    async fn glob_uses_nearest_rgignore_rule_and_negation() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join("ignored.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/.rgignore"), "!kept.rs\n").unwrap();
        fs::write(workspace.path().join("nested/ignored.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/kept.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "*.rs", ".").await.unwrap(),
            [directory_entry("nested/kept.rs", "file", Some(0))]
        );
    }

    #[tokio::test]
    async fn glob_prefers_ancestor_rgignore_over_nested_gitignore_whitelist() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "candidate.rs\n").unwrap();
        fs::write(
            workspace.path().join("nested/.gitignore"),
            "!candidate.rs\n",
        )
        .unwrap();
        fs::write(workspace.path().join("nested/candidate.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn glob_prefers_root_ignore_over_root_and_nested_gitignore() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".ignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join(".gitignore"), "!root.rs\n").unwrap();
        fs::write(workspace.path().join("nested/.gitignore"), "!nested.rs\n").unwrap();
        fs::write(workspace.path().join("root.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/nested.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn glob_prefers_rgignore_over_ignore_and_gitignore() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join(".gitignore"), "candidate.rs\n").unwrap();
        fs::write(workspace.path().join(".ignore"), "!candidate.rs\n").unwrap();
        fs::write(workspace.path().join(".rgignore"), "candidate.rs\n").unwrap();
        fs::write(workspace.path().join("candidate.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn glob_rejects_oversized_or_invalid_ignore_files_without_paths() {
        let oversized = TempDir::new().unwrap();
        fs::write(
            oversized.path().join(".rgignore"),
            vec![b'x'; FILE_MAX_BYTES + 1],
        )
        .unwrap();
        fs::write(oversized.path().join("candidate.rs"), "").unwrap();
        let error = glob_failure(&tool_host(&oversized).await, "*.rs", ".").await;
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(
            !error
                .to_string()
                .contains(&oversized.path().display().to_string())
        );

        let invalid_utf8 = TempDir::new().unwrap();
        fs::write(invalid_utf8.path().join(".ignore"), [0xff]).unwrap();
        fs::write(invalid_utf8.path().join("candidate.rs"), "").unwrap();
        let error = glob_failure(&tool_host(&invalid_utf8).await, "*.rs", ".").await;
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(
            !error
                .to_string()
                .contains(&invalid_utf8.path().display().to_string())
        );

        let malformed = TempDir::new().unwrap();
        fs::write(malformed.path().join(".gitignore"), "[z-a]\n").unwrap();
        fs::write(malformed.path().join("candidate.rs"), "").unwrap();
        let error = glob_failure(&tool_host(&malformed).await, "*.rs", ".").await;
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(
            !error
                .to_string()
                .contains(&malformed.path().display().to_string())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_treats_linked_and_non_regular_ignore_files_as_absent() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("candidate.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/candidate.rs"), "").unwrap();
        fs::write(outside.path().join("rules"), "*.rs\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("rules"),
            workspace.path().join(".rgignore"),
        )
        .unwrap();
        fs::create_dir(workspace.path().join(".ignore")).unwrap();
        create_fifo(&workspace.path().join(".gitignore"));
        let _socket = UnixListener::bind(workspace.path().join("nested/.rgignore")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), glob_path(&host, "*.rs", "."))
                .await
                .expect("ignore-file FIFOs must not block")
                .unwrap(),
            [
                directory_entry("candidate.rs", "file", Some(0)),
                directory_entry("nested/candidate.rs", "file", Some(0)),
            ]
        );
    }

    #[tokio::test]
    async fn glob_traverses_an_explicit_parent_ignored_root() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("ignored-root")).unwrap();
        fs::write(workspace.path().join(".gitignore"), "ignored-root/\n").unwrap();
        fs::write(workspace.path().join("ignored-root/kept.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
        assert_eq!(
            glob_path(&host, "*.rs", "ignored-root").await.unwrap(),
            [directory_entry("ignored-root/kept.rs", "file", Some(0))]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn glob_filters_hidden_and_ignored_non_utf8_names_before_rejecting_visible_ones() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let hidden = TempDir::new().unwrap();
        fs::write(hidden.path().join(OsStr::from_bytes(b".\xff")), "").unwrap();
        fs::write(hidden.path().join("kept.rs"), "").unwrap();
        assert_eq!(
            glob_path(&tool_host(&hidden).await, "*.rs", ".")
                .await
                .unwrap(),
            [directory_entry("kept.rs", "file", Some(0))]
        );

        let ignored = TempDir::new().unwrap();
        fs::write(ignored.path().join(".gitignore"), "ignored-*\n").unwrap();
        fs::write(ignored.path().join(OsStr::from_bytes(b"ignored-\xff")), "").unwrap();
        fs::write(ignored.path().join("kept.rs"), "").unwrap();
        assert_eq!(
            glob_path(&tool_host(&ignored).await, "*.rs", ".")
                .await
                .unwrap(),
            [directory_entry("kept.rs", "file", Some(0))]
        );

        let visible = TempDir::new().unwrap();
        fs::write(visible.path().join(OsStr::from_bytes(b"visible-\xff")), "").unwrap();
        assert_eq!(
            glob_error(&tool_host(&visible).await, "*.rs", ".").await,
            ToolErrorKind::Value
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_reports_symlinks_and_other_entries_without_descending_or_blocking() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::write(workspace.path().join("inside/file.rs"), "").unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        fs::write(outside.path().join("outside/file.rs"), "").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        let host = tool_host(&workspace).await;

        let entries = tokio::time::timeout(Duration::from_millis(100), glob_path(&host, "*", "."))
            .await
            .expect("globbing a FIFO must not block")
            .unwrap();
        assert_eq!(
            entries,
            [
                directory_entry("fifo", "other", None),
                directory_entry("inside", "directory", None),
                directory_entry("inside-link", "symlink", None),
                directory_entry("inside/file.rs", "file", Some(0)),
                directory_entry("outside-link", "symlink", None),
                directory_entry("socket", "other", None),
            ]
        );
    }

    #[tokio::test]
    async fn glob_returns_workspace_relative_paths_in_stable_order() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("z"), "").unwrap();
        fs::write(workspace.path().join("a"), "").unwrap();
        fs::write(workspace.path().join("nested/b"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "*", "./").await.unwrap(),
            [
                directory_entry("a", "file", Some(0)),
                directory_entry("nested", "directory", None),
                directory_entry("nested/b", "file", Some(0)),
                directory_entry("z", "file", Some(0)),
            ]
        );
        assert_eq!(
            glob_path(&host, "*", "nested").await.unwrap(),
            [directory_entry("nested/b", "file", Some(0))]
        );
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(
                glob_error(&host, "*", path).await,
                ToolErrorKind::Permission
            );
        }
    }

    #[tokio::test]
    async fn glob_enforces_exact_item_and_payload_limits_without_partial_results() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("aa"), "").unwrap();
        fs::write(workspace.path().join("bb"), "").unwrap();

        let at_limit = tool_host_with_collection_limits(&workspace, 2, 12).await;
        assert_eq!(
            glob_path(&at_limit, "*", ".").await.unwrap(),
            [
                directory_entry("aa", "file", Some(0)),
                directory_entry("bb", "file", Some(0)),
            ]
        );

        let over_items = tool_host_with_collection_limits(&workspace, 1, 12).await;
        assert_eq!(
            glob_path(&over_items, "*", ".").await,
            Err(ToolErrorKind::Runtime)
        );
        let over_payload = tool_host_with_collection_limits(&workspace, 2, 11).await;
        assert_eq!(
            glob_path(&over_payload, "*", ".").await,
            Err(ToolErrorKind::Runtime)
        );
    }

    #[test]
    fn grep_is_the_fourth_active_tool_with_match_evidence_labels() {
        assert_eq!(
            ACTIVE_TOOLS
                .iter()
                .map(|definition| definition.name.name())
                .collect::<Vec<_>>(),
            ["read", "list_dir", "glob", "grep"]
        );
        assert_eq!(ToolName::Grep.count_label(1), "match");
        assert_eq!(ToolName::Grep.count_label(2), "matches");
    }

    #[tokio::test]
    async fn grep_returns_one_record_per_matching_line_in_stable_path_and_line_order() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("src/nested")).unwrap();
        fs::write(workspace.path().join("src/z.rs"), "match z\nmatch again\n").unwrap();
        fs::write(
            workspace.path().join("src/nested/a.rs"),
            "skip\r\nmatch a\r\nmatch last",
        )
        .unwrap();
        fs::write(workspace.path().join("src/nested/a.py"), "match python\n").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "^match", "src", Some("*.rs"))
                .await
                .unwrap(),
            [
                grep_match("src/nested/a.rs", 2, "match a"),
                grep_match("src/nested/a.rs", 3, "match last"),
                grep_match("src/z.rs", 1, "match z"),
                grep_match("src/z.rs", 2, "match again"),
            ]
        );
        assert_eq!(
            grep_path(&host, "match", "src", Some("nested/*.rs"))
                .await
                .unwrap(),
            [
                grep_match("src/nested/a.rs", 2, "match a"),
                grep_match("src/nested/a.rs", 3, "match last"),
            ]
        );
        assert_eq!(
            grep_path(&host, "^match a$", "src", Some("nested/*.rs"))
                .await
                .unwrap(),
            [grep_match("src/nested/a.rs", 2, "match a")]
        );
    }

    #[tokio::test]
    async fn grep_rejects_invalid_patterns_and_malformed_arguments() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_error(&host, "[", ".", None).await,
            ToolErrorKind::Value
        );
        assert_eq!(
            grep_error(&host, "match", ".", Some("[")).await,
            ToolErrorKind::Value
        );
        for arguments in [
            ToolValue::None,
            ToolValue::String("match".to_owned()),
            ToolValue::List(Vec::new()),
            ToolValue::List(vec![ToolValue::String("match".to_owned()), ToolValue::None]),
            ToolValue::List(vec![
                ToolValue::String("match".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::Integer(1),
            ]),
            ToolValue::List(vec![
                ToolValue::String("match".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::None,
                ToolValue::String("extra".to_owned()),
            ]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: grep_tool(),
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[tokio::test]
    async fn grep_searches_files_larger_than_the_read_limit() {
        let workspace = TempDir::new().unwrap();
        fs::write(
            workspace.path().join("large"),
            format!("{}\nmatch\n", "x".repeat(FILE_MAX_BYTES)),
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "^match$", "large", None).await.unwrap(),
            [grep_match("large", 2, "match")]
        );
    }

    #[test]
    fn utf8_validating_reader_checks_chunk_boundaries_and_eof() {
        use std::io::Read;

        struct ChunkedReader {
            chunks: Vec<Vec<u8>>,
            next: usize,
        }

        impl Read for ChunkedReader {
            fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
                let Some(chunk) = self.chunks.get(self.next) else {
                    return Ok(0);
                };
                self.next += 1;
                buffer[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }

        let mut valid = Utf8ValidatingReader::new(ChunkedReader {
            chunks: vec![b"match \xe2".to_vec(), b"\x82".to_vec(), b"\xac\n".to_vec()],
            next: 0,
        });
        let mut contents = Vec::new();
        valid.read_to_end(&mut contents).unwrap();
        assert_eq!(std::str::from_utf8(&contents).unwrap(), "match €\n");

        let mut invalid = Utf8ValidatingReader::new(ChunkedReader {
            chunks: vec![b"match \xe2".to_vec(), b"(".to_vec()],
            next: 0,
        });
        assert_eq!(
            invalid.read_to_end(&mut Vec::new()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut incomplete = Utf8ValidatingReader::new(ChunkedReader {
            chunks: vec![b"match \xe2".to_vec()],
            next: 0,
        });
        assert_eq!(
            incomplete.read_to_end(&mut Vec::new()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_classifies_roots_refuses_symlinks_and_never_opens_other_entries() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "match\n").unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("directory/file"), "match\n").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("file", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        std::os::unix::fs::symlink("directory", workspace.path().join("directory-link")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "match", "file", None).await.unwrap(),
            [grep_match("file", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "directory", None).await.unwrap(),
            [grep_match("directory/file", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "./directory//.", None)
                .await
                .unwrap(),
            [grep_match("directory/file", 1, "match")]
        );
        for path in ["/outside", "../outside", "directory/../file"] {
            assert_eq!(
                grep_error(&host, "match", path, None).await,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            grep_error(&host, "match", "missing", None).await,
            ToolErrorKind::FileNotFound
        );
        for path in [
            "inside-link",
            "outside-link",
            "broken-link",
            "directory-link",
            "directory-link/file",
            "outside-link/file",
        ] {
            assert_eq!(
                grep_error(&host, "match", path, None).await,
                ToolErrorKind::Permission
            );
        }
        for path in ["fifo", "socket"] {
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    grep_error(&host, "match", path, None)
                )
                .await
                .expect("grepping an other entry must not block"),
                ToolErrorKind::Value
            );
        }
    }

    #[tokio::test]
    async fn grep_applies_the_file_glob_before_explicit_file_content_validation() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("binary.txt"), b"match\n\0").unwrap();
        fs::write(workspace.path().join("invalid.txt"), b"match\n\xff").unwrap();
        fs::write(
            workspace.path().join("utf16.txt"),
            b"\xff\xfem\0a\0t\0c\0h\0\n\0",
        )
        .unwrap();
        fs::write(workspace.path().join("kept.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;

        for path in ["binary.txt", "invalid.txt", "utf16.txt"] {
            assert!(
                grep_path(&host, "match", path, Some("*.rs"))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                grep_error(&host, "match", path, Some("*.txt")).await,
                ToolErrorKind::Value
            );
        }
        assert_eq!(
            grep_path(&host, "match", "kept.rs", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("kept.rs", 1, "match")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_does_not_open_a_regular_file_excluded_by_file_glob() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("excluded.txt");
        fs::write(&path, "match\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let host = tool_host(&workspace).await;

        assert!(
            grep_path(&host, "match", "excluded.txt", Some("*.rs"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_skips_traversed_non_regular_binary_invalid_and_symlink_entries() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("kept.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("binary.rs"), b"match\n\0").unwrap();
        fs::write(workspace.path().join("invalid.rs"), b"not a match\n\xff").unwrap();
        fs::write(
            workspace.path().join("late-binary.rs"),
            format!("match\n{}\0", "x".repeat(64 * 1024)),
        )
        .unwrap();
        let mut late_invalid = format!("match\n{}", "x".repeat(64 * 1024)).into_bytes();
        late_invalid.push(0xff);
        fs::write(workspace.path().join("late-invalid.rs"), late_invalid).unwrap();
        create_fifo(&workspace.path().join("fifo.rs"));
        fs::write(outside.path().join("outside.rs"), "match\n").unwrap();
        fs::create_dir(workspace.path().join("linked-directory")).unwrap();
        fs::write(
            workspace.path().join("linked-directory/match.rs"),
            "match\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "linked-directory",
            workspace.path().join("inside-directory-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("kept.rs", workspace.path().join("inside-link.rs")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside.rs"),
            workspace.path().join("outside-link.rs"),
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(100),
                grep_path(&host, "match", ".", Some("*.rs")),
            )
            .await
            .expect("grepping a traversed FIFO must not block")
            .unwrap(),
            [
                grep_match("kept.rs", 1, "match"),
                grep_match("linked-directory/match.rs", 1, "match"),
            ]
        );
    }

    #[tokio::test]
    async fn grep_reuses_hidden_and_project_ignore_filtering_but_accepts_explicit_roots() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join(".hidden/nested")).unwrap();
        fs::create_dir(workspace.path().join("ignored-root")).unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".hidden/kept.rs"), "match\n").unwrap();
        fs::write(
            workspace.path().join(".hidden/nested/.hidden.rs"),
            "match\n",
        )
        .unwrap();
        fs::write(workspace.path().join("ignored-root/kept.rs"), "match\n").unwrap();
        fs::write(
            workspace.path().join(".gitignore"),
            "ignored-root/\n*.tmp\n",
        )
        .unwrap();
        fs::write(workspace.path().join("ignored.tmp"), "match\n").unwrap();
        fs::write(workspace.path().join("nested/.rgignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join("nested/ignored.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("nested/kept.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("visible.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "match", ".", Some("*.rs")).await.unwrap(),
            [grep_match("visible.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", ".hidden", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match(".hidden/kept.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", ".hidden/kept.rs", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match(".hidden/kept.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "ignored-root", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("ignored-root/kept.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "ignored-root/kept.rs", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("ignored-root/kept.rs", 1, "match")]
        );
    }

    #[tokio::test]
    async fn grep_preserves_ignore_family_precedence_and_negation() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "candidate.rs\n").unwrap();
        fs::write(
            workspace.path().join("nested/.gitignore"),
            "!candidate.rs\n",
        )
        .unwrap();
        fs::write(workspace.path().join("nested/candidate.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;
        assert!(
            grep_path(&host, "match", ".", Some("*.rs"))
                .await
                .unwrap()
                .is_empty()
        );

        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join("nested/.rgignore"), "!kept.rs\n").unwrap();
        fs::write(workspace.path().join("nested/ignored.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("nested/kept.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;
        assert_eq!(
            grep_path(&host, "match", ".", Some("*.rs")).await.unwrap(),
            [grep_match("nested/kept.rs", 1, "match")]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn grep_filters_hidden_and_ignored_non_utf8_names_before_rejecting_visible_ones() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let hidden = TempDir::new().unwrap();
        fs::write(hidden.path().join(OsStr::from_bytes(b".\xff")), "match\n").unwrap();
        fs::write(hidden.path().join("kept.rs"), "match\n").unwrap();
        assert_eq!(
            grep_path(&tool_host(&hidden).await, "match", ".", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("kept.rs", 1, "match")]
        );

        let ignored = TempDir::new().unwrap();
        fs::write(ignored.path().join(".gitignore"), "ignored-*\n").unwrap();
        fs::write(
            ignored.path().join(OsStr::from_bytes(b"ignored-\xff")),
            "match\n",
        )
        .unwrap();
        fs::write(ignored.path().join("kept.rs"), "match\n").unwrap();
        assert_eq!(
            grep_path(&tool_host(&ignored).await, "match", ".", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("kept.rs", 1, "match")]
        );

        let visible = TempDir::new().unwrap();
        fs::write(
            visible.path().join(OsStr::from_bytes(b"visible-\xff")),
            "match\n",
        )
        .unwrap();
        assert_eq!(
            grep_error(&tool_host(&visible).await, "match", ".", Some("*.rs")).await,
            ToolErrorKind::Value
        );
    }

    #[tokio::test]
    async fn grep_enforces_exact_item_and_payload_limits_without_partial_results() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("aa"), "match\n").unwrap();
        fs::write(workspace.path().join("bb"), "match\n").unwrap();

        let at_limit = tool_host_with_collection_limits(&workspace, 2, 14).await;
        assert_eq!(
            grep_path(&at_limit, "match", ".", None).await.unwrap(),
            [grep_match("aa", 1, "match"), grep_match("bb", 1, "match")]
        );
        let over_items = tool_host_with_collection_limits(&workspace, 1, 14).await;
        assert_eq!(
            grep_error(&over_items, "match", ".", None).await,
            ToolErrorKind::Runtime
        );
        let over_payload = tool_host_with_collection_limits(&workspace, 2, 13).await;
        assert_eq!(
            grep_error(&over_payload, "match", ".", None).await,
            ToolErrorKind::Runtime
        );
    }

    #[test]
    fn generated_sources_share_active_tool_definitions() {
        assert_eq!(
            bootstrap_source(),
            concat!(
                "from dataclasses import dataclass\n\n",
                "@dataclass\n",
                "class DirectoryEntry:\n",
                "    path: str\n",
                "    kind: str\n",
                "    size_bytes: int | None\n\n",
                "@dataclass\n",
                "class GrepMatch:\n",
                "    path: str\n",
                "    line: int\n",
                "    text: str\n\n",
                "async def read(path: str) -> str:\n",
                "    \"\"\"Read one complete UTF-8 workspace file.\"\"\"\n",
                "    return await _kiki_read(path)\n\n",
                "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:\n",
                "    \"\"\"List immediate workspace directory entries.\"\"\"\n",
                "    return [DirectoryEntry(**row) for row in await _kiki_list_dir(path)]\n\n",
                "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:\n",
                "    \"\"\"Match workspace filesystem entries recursively.\"\"\"\n",
                "    return [DirectoryEntry(**row) for row in await _kiki_glob(pattern, path)]\n\n",
                "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:\n",
                "    \"\"\"Search workspace file contents with a regular expression.\"\"\"\n",
                "    return [GrepMatch(**row) for row in await _kiki_grep(pattern, path, file_glob)]"
            )
        );
        let reference = prompt_reference();
        for signature in [
            "async def read(path: str) -> str:",
            "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
            "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
            "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:",
        ] {
            assert_eq!(reference.matches(signature).count(), 1);
        }
        assert_eq!(reference.matches("class DirectoryEntry:").count(), 1);
        assert_eq!(reference.matches("class GrepMatch:").count(), 1);
        assert!(reference.contains("Do not redefine these host functions."));
        assert!(reference.contains("    ..."));
        for unavailable in ["write(", "edit(", "run("] {
            assert!(!reference.contains(unavailable));
        }
    }
}
