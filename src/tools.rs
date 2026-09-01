use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Condvar, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, Metadata};
#[cfg(unix)]
use cap_std::fs::{OpenOptions, OpenOptionsExt};
use globset::{GlobBuilder, GlobMatcher};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use thiserror::Error;
#[cfg(test)]
use tokio::sync::Semaphore;

use crate::config::constants::{
    COLLECTION_MAX_ITEMS, COLLECTION_MAX_PAYLOAD_BYTES, FILE_MAX_BYTES,
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
}

impl ToolName {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::ListDir => "list_dir",
            Self::Glob => "glob",
        }
    }

    pub(crate) fn private_name(self) -> &'static str {
        match self {
            Self::Read => "_kiki_read",
            Self::ListDir => "_kiki_list_dir",
            Self::Glob => "_kiki_glob",
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
}

impl ToolDefault {
    fn source(self) -> String {
        match self {
            Self::String(value) => format!("{value:?}"),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ToolValueType {
    String,
    OptionalInteger,
}

impl ToolValueType {
    fn annotation(self) -> &'static str {
        match self {
            Self::String => "str",
            Self::OptionalInteger => "int | None",
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
        .map(ToolRecord::prompt_skeleton)
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
        "## Kiki tool reference\n\nDo not redefine these host functions. Await every host call.\n\n{source}\n"
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

    fn prompt_skeleton(&self) -> String {
        self.bootstrap_definition()
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

    pub(crate) async fn call(&self, call: ToolCall) -> ToolResult {
        let ToolCall {
            call_id,
            tool,
            arguments,
        } = call;
        let result = match tool {
            ToolName::Read => self.read(arguments).await,
            ToolName::ListDir => self.list_dir(arguments).await,
            ToolName::Glob => self.glob(arguments).await,
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
    let metadata = parent
        .symlink_metadata(&file_name)
        .map_err(ToolError::from_io)?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::permission(
            "workspace paths cannot traverse symbolic links",
        ));
    }
    if metadata.is_dir() {
        return Err(ToolError::new(
            ToolErrorKind::IsADirectory,
            "path is a directory",
        ));
    }
    if !metadata.is_file() {
        return Err(ToolError::value("path is not a regular file"));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(ToolError::value(format!(
            "file exceeds the {max_bytes}-byte limit"
        )));
    }

    #[cfg(unix)]
    let mut file = {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        parent
            .open_with(&file_name, &options)
            .map_err(no_follow_io)?
    };
    #[cfg(not(unix))]
    let mut file = parent.open(&file_name).map_err(ToolError::from_io)?;
    let metadata = file.metadata().map_err(ToolError::from_io)?;
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

#[derive(Clone)]
struct IgnoreMatchers {
    rgignore: Gitignore,
    ignore: Gitignore,
    gitignore: Gitignore,
}

struct GlobDirectory {
    directory: Dir,
    path: PathBuf,
    ignores: Vec<IgnoreMatchers>,
}

fn glob_directory(
    workspace: &Dir,
    root: &Path,
    matcher: (GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let (matcher, basename_only) = matcher;
    let root_directory = open_directory_root(workspace, root)?;
    let mut ignore_directory = workspace.try_clone().map_err(ToolError::from_io)?;
    let mut root_path = PathBuf::new();
    let mut ignores = vec![directory_ignore_matchers(&ignore_directory, &root_path)?];
    for component in root.components() {
        let Component::Normal(component) = component else {
            unreachable!("glob roots are normalised before traversal");
        };
        ignore_directory = open_directory_no_follow(&ignore_directory, Path::new(component))?;
        root_path.push(component);
        ignores.push(directory_ignore_matchers(&ignore_directory, &root_path)?);
    }

    let mut directories = vec![GlobDirectory {
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
                directories.push(GlobDirectory {
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
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ToolError::from_io(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }

    #[cfg(unix)]
    let mut file = {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
        directory.open_with(name, &options).map_err(no_follow_io)?
    };
    #[cfg(not(unix))]
    let mut file = directory.open(name).map_err(ToolError::from_io)?;
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
    let metadata = parent
        .symlink_metadata(&file_name)
        .map_err(ToolError::from_io)?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::permission(
            "workspace paths cannot traverse symbolic links",
        ));
    }
    if !metadata.is_dir() {
        return Err(ToolError::new(
            ToolErrorKind::NotADirectory,
            "path is not a directory",
        ));
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
fn open_directory_no_follow(parent: &Dir, name: &Path) -> Result<Dir, ToolError> {
    let metadata = parent.symlink_metadata(name).map_err(ToolError::from_io)?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::permission(
            "workspace paths cannot traverse symbolic links",
        ));
    }

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    parent
        .open_with(name, &options)
        .map(|file| Dir::from_std_file(file.into_std()))
        .map_err(no_follow_io)
}

#[cfg(unix)]
fn no_follow_io(error: std::io::Error) -> ToolError {
    if matches!(error.raw_os_error(), Some(libc::ELOOP) | Some(libc::EMLINK)) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    ToolError::from_io(error)
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
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    use tempfile::TempDir;

    use crate::config::constants::FILE_MAX_BYTES;

    use super::{
        ToolCall, ToolError, ToolErrorKind, ToolHost, ToolName, ToolValue, bootstrap_source,
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
                "async def read(path: str) -> str:\n",
                "    \"\"\"Read one complete UTF-8 workspace file.\"\"\"\n",
                "    return await _kiki_read(path)\n\n",
                "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:\n",
                "    \"\"\"List immediate workspace directory entries.\"\"\"\n",
                "    return [DirectoryEntry(**row) for row in await _kiki_list_dir(path)]\n\n",
                "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:\n",
                "    \"\"\"Match workspace filesystem entries recursively.\"\"\"\n",
                "    return [DirectoryEntry(**row) for row in await _kiki_glob(pattern, path)]"
            )
        );
        let reference = prompt_reference();
        for signature in [
            "async def read(path: str) -> str:",
            "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
            "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
        ] {
            assert_eq!(reference.matches(signature).count(), 1);
        }
        assert_eq!(reference.matches("class DirectoryEntry:").count(), 1);
        assert!(reference.contains("Do not redefine these host functions."));
        assert!(reference.contains("    ..."));
        for unavailable in ["grep(", "write(", "edit(", "run("] {
            assert!(!reference.contains(unavailable));
        }
    }
}
