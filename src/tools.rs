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

#[derive(Clone, Copy)]
struct ReadLimits {
    max_bytes: usize,
}

pub struct ReadTool {
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
pub enum ReadError {
    #[error("workspace paths must be relative")]
    AbsolutePath,
    #[error("workspace paths cannot contain parent components")]
    ParentPath,
    #[error("path is not a regular file")]
    NonRegularFile,
    #[error("file exceeds the {limit}-byte limit")]
    ByteLimitExceeded { limit: usize },
    #[error("file is not valid UTF-8")]
    InvalidUtf8,
    #[error("filesystem operation failed")]
    Io(#[source] std::io::Error),
    #[error("blocking file operation failed")]
    Blocking(#[from] tokio::task::JoinError),
}

impl ReadTool {
    pub async fn new(workspace: impl AsRef<Path>) -> Result<Self, ReadError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ReadLimits {
                max_bytes: FILE_MAX_BYTES,
            },
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits(workspace: impl AsRef<Path>, max_bytes: usize) -> Result<Self, ReadError> {
        Self::open(workspace.as_ref().to_path_buf(), ReadLimits { max_bytes }).await
    }

    async fn open(workspace: PathBuf, limits: ReadLimits) -> Result<Self, ReadError> {
        let workspace = tokio::task::spawn_blocking(move || {
            Dir::open_ambient_dir(workspace, ambient_authority())
                .map(Arc::new)
                .map_err(ReadError::Io)
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

    pub async fn read(&self, path: impl AsRef<Path>) -> Result<Vec<String>, ReadError> {
        let path = path.as_ref().to_path_buf();
        validate_path(&path)?;
        #[cfg(test)]
        if let Some(test_gate) = &self.test_gate {
            test_gate.wait_before_read().await;
        }

        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || read_file(&workspace, &path, limits)).await?
    }

    #[cfg(test)]
    pub(crate) async fn wait_after_read_settled(&self) {
        if let Some(test_gate) = &self.test_gate {
            test_gate.wait_after_read_settled().await;
        }
    }
}

fn validate_path(path: &Path) -> Result<(), ReadError> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
    {
        return Err(ReadError::AbsolutePath);
    }

    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(ReadError::ParentPath);
    }

    Ok(())
}

fn read_file(workspace: &Dir, path: &Path, limits: ReadLimits) -> Result<Vec<String>, ReadError> {
    #[cfg(unix)]
    let mut file = {
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NONBLOCK);
        workspace.open_with(path, &options).map_err(ReadError::Io)?
    };
    #[cfg(not(unix))]
    let mut file = workspace.open(path).map_err(ReadError::Io)?;
    let metadata = file.metadata().map_err(ReadError::Io)?;
    if !metadata.is_file() {
        return Err(ReadError::NonRegularFile);
    }
    if metadata.len() > limits.max_bytes as u64 {
        return Err(ReadError::ByteLimitExceeded {
            limit: limits.max_bytes,
        });
    }

    let contents = String::from_utf8(read_limited(&mut file, limits.max_bytes)?)
        .map_err(|_| ReadError::InvalidUtf8)?;
    Ok(contents.split_inclusive('\n').map(str::to_owned).collect())
}

fn read_limited(file: &mut File, max_bytes: usize) -> Result<Vec<u8>, ReadError> {
    let mut contents = Vec::new();
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(ReadError::Io)?;
    if contents.len() > max_bytes {
        return Err(ReadError::ByteLimitExceeded { limit: max_bytes });
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

    use super::{ReadError, ReadTestGate, ReadTool};

    async fn read_tool(workspace: &TempDir) -> ReadTool {
        ReadTool::new(workspace.path()).await.unwrap()
    }

    async fn read_tool_with_limits(workspace: &TempDir, max_bytes: usize) -> ReadTool {
        ReadTool::with_limits(workspace.path(), max_bytes)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn test_gate_pauses_a_read_until_released() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("paused"), "contents").unwrap();
        let gate = ReadTestGate::new();
        let tool = read_tool(&workspace).await.with_test_gate(gate.clone());
        let mut read = tokio::spawn(async move { tool.read("paused").await });

        gate.wait_until_read_started().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut read)
                .await
                .is_err()
        );
        gate.resume_read();

        assert_eq!(read.await.unwrap().unwrap(), ["contents"]);
    }

    #[tokio::test]
    async fn preserves_lf_crlf_and_missing_final_newline() {
        let workspace = TempDir::new().unwrap();
        let tool = read_tool(&workspace).await;
        let cases = [
            ("lf", "first\nsecond\n", vec!["first\n", "second\n"]),
            (
                "crlf",
                "first\r\nsecond\r\n",
                vec!["first\r\n", "second\r\n"],
            ),
            (
                "no-final-newline",
                "first\nsecond",
                vec!["first\n", "second"],
            ),
        ];

        for (name, contents, expected) in cases {
            fs::write(workspace.path().join(name), contents).unwrap();
            let lines = tool.read(name).await.unwrap();

            assert_eq!(lines, expected);
            assert_eq!(lines.concat(), contents);
        }
    }

    #[tokio::test]
    async fn reads_empty_file() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("empty"), "").unwrap();

        let lines = read_tool(&workspace).await.read("empty").await.unwrap();

        assert_eq!(lines, Vec::<String>::new());
    }

    #[tokio::test]
    async fn rejects_absolute_path() {
        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("file");
        fs::write(&path, "contents").unwrap();

        let error = read_tool(&workspace).await.read(path).await.unwrap_err();

        assert!(matches!(error, ReadError::AbsolutePath));
    }

    #[tokio::test]
    async fn rejects_parent_path() {
        let workspace = TempDir::new().unwrap();

        let error = read_tool(&workspace)
            .await
            .read(Path::new("../outside"))
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::ParentPath));
    }

    #[cfg(unix)]
    fn create_fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a NUL-terminated path owned for the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_fifo_without_waiting_for_writer() {
        let workspace = TempDir::new().unwrap();
        let fifo = workspace.path().join("fifo");
        create_fifo(&fifo);
        let tool = read_tool(&workspace).await;
        let read = tool.read("fifo");
        tokio::pin!(read);

        let result = tokio::time::timeout(Duration::from_millis(100), &mut read).await;
        if result.is_err() {
            let writer = std::thread::spawn(move || fs::OpenOptions::new().write(true).open(fifo));
            writer.join().unwrap().unwrap();
            let _ = read.await;
        }

        assert!(matches!(result, Ok(Err(ReadError::NonRegularFile))));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("outside");
        fs::write(&target, "outside").unwrap();
        std::os::unix::fs::symlink(&target, workspace.path().join("escape")).unwrap();

        let error = read_tool(&workspace)
            .await
            .read("escape")
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::Io(_)));
    }

    #[tokio::test]
    async fn rejects_missing_path() {
        let workspace = TempDir::new().unwrap();

        let error = read_tool(&workspace)
            .await
            .read("missing")
            .await
            .unwrap_err();

        assert!(
            matches!(error, ReadError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
        );
    }

    #[tokio::test]
    async fn rejects_non_regular_path() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();

        let error = read_tool(&workspace)
            .await
            .read("directory")
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::NonRegularFile));
    }

    #[tokio::test]
    async fn rejects_invalid_utf8() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("invalid"), [0xff]).unwrap();

        let error = read_tool(&workspace)
            .await
            .read("invalid")
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::InvalidUtf8));
    }

    #[tokio::test]
    async fn rejects_file_over_byte_limit() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("large"), "12345").unwrap();

        let error = read_tool_with_limits(&workspace, 4)
            .await
            .read("large")
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::ByteLimitExceeded { limit: 4 }));
    }

    #[tokio::test]
    async fn accepts_file_with_more_than_the_previous_line_limit() {
        let workspace = TempDir::new().unwrap();
        fs::write(
            workspace.path().join("many-lines"),
            "line\n".repeat(100_001),
        )
        .unwrap();

        let lines = read_tool(&workspace)
            .await
            .read("many-lines")
            .await
            .unwrap();

        assert_eq!(lines.len(), 100_001);
    }
}
