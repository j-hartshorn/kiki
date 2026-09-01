use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File};
#[cfg(unix)]
use cap_std::fs::{OpenOptions, OpenOptionsExt};
use thiserror::Error;
#[cfg(test)]
use tokio::sync::Semaphore;

use crate::config::constants::FILE_MAX_BYTES;

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
}

impl ToolName {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
        }
    }

    pub(crate) fn private_name(self) -> &'static str {
        match self {
            Self::Read => "_kiki_read",
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
    Permission,
    Value,
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

    fn from_io(error: std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::NotFound => ToolErrorKind::FileNotFound,
            std::io::ErrorKind::PermissionDenied => ToolErrorKind::Permission,
            _ => ToolErrorKind::Os,
        };
        let message = match kind {
            ToolErrorKind::FileNotFound => "workspace path was not found".to_owned(),
            ToolErrorKind::Permission => "workspace access was denied".to_owned(),
            ToolErrorKind::Os => "filesystem operation failed".to_owned(),
            ToolErrorKind::Type | ToolErrorKind::IsADirectory | ToolErrorKind::Value => {
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
}

#[derive(Clone, Copy)]
pub(crate) enum ToolValueType {
    String,
}

impl ToolValueType {
    fn annotation(self) -> &'static str {
        match self {
            Self::String => "str",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ToolResultShape {
    String,
}

impl ToolResultShape {
    fn annotation(self) -> &'static str {
        match self {
            Self::String => "str",
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
}];

pub(crate) const ACTIVE_TOOLS: &[ToolDefinition] = &[ToolDefinition {
    name: ToolName::Read,
    parameters: READ_PARAMETERS,
    result: ToolResultShape::String,
    description: "Read one complete UTF-8 workspace file.",
}];

pub(crate) fn bootstrap_source() -> String {
    ACTIVE_TOOLS
        .iter()
        .map(|definition| definition.bootstrap_wrapper())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) fn prompt_reference() -> String {
    let definitions = ACTIVE_TOOLS
        .iter()
        .map(|definition| definition.prompt_skeleton())
        .collect::<Vec<_>>()
        .join("\n\n");
    format!(
        "## Kiki tool reference\n\nDo not redefine these host functions. Await every host call.\n\n{definitions}\n"
    )
}

impl ToolDefinition {
    fn signature(self) -> String {
        let parameters = self
            .parameters
            .iter()
            .map(|parameter| format!("{}: {}", parameter.name, parameter.value_type.annotation()))
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
            "{}\n    \"\"\"{}\"\"\"\n    return await {}({})",
            self.signature(),
            self.description,
            self.name.private_name(),
            self.arguments(),
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
struct ReadLimits {
    max_bytes: usize,
}

pub(crate) struct ToolHost {
    workspace: Arc<Dir>,
    limits: ReadLimits,
    #[cfg(test)]
    test_gate: Option<ReadTestGate>,
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
            ReadLimits {
                max_bytes: FILE_MAX_BYTES,
            },
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits(
        workspace: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<Self, ToolHostError> {
        Self::open(workspace.as_ref().to_path_buf(), ReadLimits { max_bytes }).await
    }

    async fn open(workspace: PathBuf, limits: ReadLimits) -> Result<Self, ToolHostError> {
        let workspace = tokio::task::spawn_blocking(move || {
            Dir::open_ambient_dir(workspace, ambient_authority()).map(Arc::new)
        })
        .await??;

        Ok(Self {
            workspace,
            limits,
            #[cfg(test)]
            test_gate: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_gate(mut self, test_gate: ReadTestGate) -> Self {
        self.test_gate = Some(test_gate);
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
        tokio::task::spawn_blocking(move || read_file(&workspace, &path, limits))
            .await
            .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
            .map(ToolValue::String)
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

fn read_file(workspace: &Dir, path: &Path, limits: ReadLimits) -> Result<String, ToolError> {
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
    if metadata.len() > limits.max_bytes as u64 {
        return Err(ToolError::value(format!(
            "file exceeds the {}-byte limit",
            limits.max_bytes
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

    String::from_utf8(read_limited(&mut file, limits.max_bytes)?)
        .map_err(|_| ToolError::value("file is not valid UTF-8"))
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
    use std::fs;
    use std::path::Path;

    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{
        ToolCall, ToolErrorKind, ToolHost, ToolName, ToolValue, bootstrap_source, prompt_reference,
    };

    async fn tool_host(workspace: &TempDir) -> ToolHost {
        ToolHost::new(workspace.path()).await.unwrap()
    }

    async fn tool_host_with_limits(workspace: &TempDir, max_bytes: usize) -> ToolHost {
        ToolHost::with_limits(workspace.path(), max_bytes)
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

    #[test]
    fn generated_sources_share_one_read_definition() {
        assert_eq!(
            bootstrap_source(),
            concat!(
                "async def read(path: str) -> str:\n",
                "    \"\"\"Read one complete UTF-8 workspace file.\"\"\"\n",
                "    return await _kiki_read(path)"
            )
        );
        let reference = prompt_reference();
        assert_eq!(
            reference
                .matches("async def read(path: str) -> str:")
                .count(),
            1
        );
        assert!(reference.contains("Do not redefine these host functions."));
        assert!(reference.contains("    ..."));
    }
}
