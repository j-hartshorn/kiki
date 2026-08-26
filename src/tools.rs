use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File};
#[cfg(unix)]
use cap_std::fs::{OpenOptions, OpenOptionsExt};
use thiserror::Error;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINES: usize = 100_000;

#[derive(Clone, Copy)]
struct ReadLimits {
    max_bytes: usize,
    max_lines: usize,
}

pub struct ReadTool {
    workspace: Arc<Dir>,
    limits: ReadLimits,
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
    #[error("file exceeds the {limit}-line limit")]
    LineLimitExceeded { limit: usize },
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
                max_bytes: MAX_BYTES,
                max_lines: MAX_LINES,
            },
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits(
        workspace: impl AsRef<Path>,
        max_bytes: usize,
        max_lines: usize,
    ) -> Result<Self, ReadError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ReadLimits {
                max_bytes,
                max_lines,
            },
        )
        .await
    }

    async fn open(workspace: PathBuf, limits: ReadLimits) -> Result<Self, ReadError> {
        let workspace = tokio::task::spawn_blocking(move || {
            Dir::open_ambient_dir(workspace, ambient_authority())
                .map(Arc::new)
                .map_err(ReadError::Io)
        })
        .await??;

        Ok(Self { workspace, limits })
    }

    pub async fn read(&self, path: impl AsRef<Path>) -> Result<Vec<String>, ReadError> {
        let path = path.as_ref().to_path_buf();
        validate_path(&path)?;

        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || read_file(&workspace, &path, limits)).await?
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
    let line_count = contents.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!contents.is_empty() && !contents.ends_with('\n'));
    if line_count > limits.max_lines {
        return Err(ReadError::LineLimitExceeded {
            limit: limits.max_lines,
        });
    }

    Ok(contents.split_inclusive('\n').map(str::to_owned).collect())
}

fn read_limited(file: &mut File, max_bytes: usize) -> Result<Vec<u8>, ReadError> {
    let mut contents = Vec::new();
    let mut buffer = [0; 8192];

    loop {
        let remaining = max_bytes - contents.len();
        let read_len = remaining.saturating_add(1).min(buffer.len());
        let read = file.read(&mut buffer[..read_len]).map_err(ReadError::Io)?;
        if read == 0 {
            return Ok(contents);
        }
        if read > remaining {
            return Err(ReadError::ByteLimitExceeded { limit: max_bytes });
        }
        contents.extend_from_slice(&buffer[..read]);
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    #[cfg(unix)]
    use std::time::Duration;

    use tempfile::TempDir;

    use super::{ReadError, ReadTool};

    async fn read_tool(workspace: &TempDir) -> ReadTool {
        ReadTool::new(workspace.path()).await.unwrap()
    }

    async fn read_tool_with_limits(
        workspace: &TempDir,
        max_bytes: usize,
        max_lines: usize,
    ) -> ReadTool {
        ReadTool::with_limits(workspace.path(), max_bytes, max_lines)
            .await
            .unwrap()
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

        let error = read_tool_with_limits(&workspace, 4, 100)
            .await
            .read("large")
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::ByteLimitExceeded { limit: 4 }));
    }

    #[tokio::test]
    async fn rejects_file_over_line_limit() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("many-lines"), "one\ntwo\nthree\n").unwrap();

        let error = read_tool_with_limits(&workspace, 100, 2)
            .await
            .read("many-lines")
            .await
            .unwrap_err();

        assert!(matches!(error, ReadError::LineLimitExceeded { limit: 2 }));
    }
}
