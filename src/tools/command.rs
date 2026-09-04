use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;

use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
#[cfg(test)]
use tokio::sync::{Mutex, watch};

use crate::config::constants::COMMAND_STREAM_MAX_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CommandResult {
    pub(super) exit_code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(super) enum CommandError {
    #[error("command could not be launched")]
    Launch,
    #[error("command execution failed")]
    Internal,
    #[error("{0}")]
    Overflow(String),
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct CommandTestGate {
    started_tx: watch::Sender<Option<libc::pid_t>>,
    started_rx: Mutex<watch::Receiver<Option<libc::pid_t>>>,
    kill_sent_tx: watch::Sender<Option<libc::pid_t>>,
    kill_sent_rx: watch::Receiver<Option<libc::pid_t>>,
    cleaned_up_tx: watch::Sender<Option<libc::pid_t>>,
    cleaned_up_rx: watch::Receiver<Option<libc::pid_t>>,
    supervisor_exited_tx: watch::Sender<bool>,
    supervisor_exited_rx: watch::Receiver<bool>,
    error_published_tx: watch::Sender<bool>,
    error_published_rx: watch::Receiver<bool>,
    error_hold_tx: watch::Sender<bool>,
    error_hold_rx: watch::Receiver<bool>,
    read_hold_tx: watch::Sender<bool>,
    read_hold_rx: watch::Receiver<bool>,
}

#[cfg(test)]
impl CommandTestGate {
    pub(crate) fn new() -> Arc<Self> {
        let (started_tx, started_rx) = watch::channel(None);
        let (kill_sent_tx, kill_sent_rx) = watch::channel(None);
        let (cleaned_up_tx, cleaned_up_rx) = watch::channel(None);
        let (supervisor_exited_tx, supervisor_exited_rx) = watch::channel(false);
        let (error_published_tx, error_published_rx) = watch::channel(false);
        let (error_hold_tx, error_hold_rx) = watch::channel(false);
        let (read_hold_tx, read_hold_rx) = watch::channel(false);
        Arc::new(Self {
            started_tx,
            started_rx: Mutex::new(started_rx),
            kill_sent_tx,
            kill_sent_rx,
            cleaned_up_tx,
            cleaned_up_rx,
            supervisor_exited_tx,
            supervisor_exited_rx,
            error_published_tx,
            error_published_rx,
            error_hold_tx,
            error_hold_rx,
            read_hold_tx,
            read_hold_rx,
        })
    }

    pub(crate) async fn wait_until_started(&self) -> libc::pid_t {
        let mut rx = self.started_rx.lock().await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                rx.changed()
                    .await
                    .expect("test gate started sender dropped");
                if let Some(pgid) = *rx.borrow() {
                    return pgid;
                }
            }
        })
        .await
        .expect("timed out waiting for command start")
    }

    pub(crate) async fn wait_until_kill_sent(&self, pgid: libc::pid_t) {
        let mut rx = self.kill_sent_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while *rx.borrow() != Some(pgid) {
                rx.changed().await.expect("test gate kill sender dropped");
            }
        })
        .await
        .expect("timed out waiting for group kill");
    }

    pub(crate) async fn wait_until_cleaned_up(&self, pgid: libc::pid_t) {
        let mut rx = self.cleaned_up_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while *rx.borrow() != Some(pgid) {
                rx.changed()
                    .await
                    .expect("test gate cleanup sender dropped");
            }
        })
        .await
        .expect("timed out waiting for command cleanup");
    }

    pub(crate) async fn wait_until_supervisor_exited(&self) {
        let mut rx = self.supervisor_exited_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*rx.borrow() {
                rx.changed()
                    .await
                    .expect("test gate supervisor sender dropped");
            }
        })
        .await
        .expect("timed out waiting for command supervisor exit");
    }

    pub(super) async fn wait_until_error_published(&self) {
        let mut rx = self.error_published_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !*rx.borrow() {
                rx.changed()
                    .await
                    .expect("test gate error publication sender dropped");
            }
        })
        .await
        .expect("timed out waiting for command error publication");
    }

    pub(crate) fn error_published(&self) -> bool {
        *self.error_published_rx.borrow()
    }

    pub(crate) fn hold_error_publication(&self) {
        let _ = self.error_hold_tx.send(true);
    }

    pub(crate) fn release_error_publication(&self) {
        let _ = self.error_hold_tx.send(false);
    }

    pub(super) fn hold_read(&self) {
        let _ = self.read_hold_tx.send(true);
    }

    pub(super) fn release_read(&self) {
        let _ = self.read_hold_tx.send(false);
    }

    fn notify_started(&self, pgid: libc::pid_t) {
        let _ = self.started_tx.send(Some(pgid));
    }

    fn notify_kill_sent(&self, pgid: libc::pid_t) {
        let _ = self.kill_sent_tx.send(Some(pgid));
    }

    fn notify_cleaned_up(&self, pgid: libc::pid_t) {
        let _ = self.cleaned_up_tx.send(Some(pgid));
    }

    fn notify_supervisor_exited(&self) {
        let _ = self.supervisor_exited_tx.send(true);
    }

    fn notify_error_published(&self) {
        let _ = self.error_published_tx.send(true);
    }

    fn reads_held(&self) -> bool {
        *self.read_hold_rx.borrow()
    }

    async fn wait_if_read_held(&self) {
        let mut rx = self.read_hold_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while *rx.borrow() {
                rx.changed().await.expect("test gate read sender dropped");
            }
        })
        .await
        .expect("timed out waiting to release command reads");
    }

    async fn wait_if_error_held(&self) {
        let mut rx = self.error_hold_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            while *rx.borrow() {
                rx.changed().await.expect("test gate error sender dropped");
            }
        })
        .await
        .expect("timed out waiting to release command error");
    }
}

#[cfg(test)]
struct SupervisorExitGuard {
    gate: Option<Arc<CommandTestGate>>,
}

#[cfg(test)]
impl Drop for SupervisorExitGuard {
    fn drop(&mut self) {
        if let Some(gate) = &self.gate {
            gate.notify_supervisor_exited();
        }
    }
}

struct GroupKillGuard {
    pgid: libc::pid_t,
    killed: bool,
    #[cfg(test)]
    gate: Option<Arc<CommandTestGate>>,
}

impl GroupKillGuard {
    fn new(pgid: libc::pid_t, #[cfg(test)] gate: Option<Arc<CommandTestGate>>) -> Self {
        Self {
            pgid,
            killed: false,
            #[cfg(test)]
            gate,
        }
    }

    fn kill(&mut self) {
        if !self.killed && self.pgid > 0 {
            self.killed = true;
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
            #[cfg(test)]
            if let Some(gate) = &self.gate {
                gate.notify_kill_sent(self.pgid);
            }
        }
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

async fn kill_and_reap(mut guard: GroupKillGuard, mut child: tokio::process::Child) {
    guard.kill();
    let _ = child.wait().await;
}

fn child_has_exited(pid: libc::pid_t) -> Result<bool, ()> {
    let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
    loop {
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } == 0
        {
            return Ok(unsafe { info.assume_init_ref().si_pid() == pid });
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Err(());
        }
    }
}

async fn observe_child_termination(pid: libc::pid_t, mut sigchld: Signal) -> Result<(), ()> {
    while !child_has_exited(pid)? {
        sigchld.recv().await;
    }
    Ok(())
}

fn cancellation_requested(cancel_rx: &mut oneshot::Receiver<()>) -> bool {
    !matches!(cancel_rx.try_recv(), Err(TryRecvError::Empty))
}

pub(super) async fn execute_command(
    command: &str,
    cwd: OwnedFd,
) -> Result<CommandResult, CommandError> {
    #[cfg(test)]
    {
        execute_command_with_test_gate(command, cwd, None).await
    }
    #[cfg(not(test))]
    {
        execute_command_bounded(command, cwd, COMMAND_STREAM_MAX_BYTES).await
    }
}

#[cfg(test)]
pub(super) async fn execute_command_with_test_gate(
    command: &str,
    cwd: OwnedFd,
    gate: Option<Arc<CommandTestGate>>,
) -> Result<CommandResult, CommandError> {
    execute_command_bounded(command, cwd, COMMAND_STREAM_MAX_BYTES, gate).await
}

#[cfg(test)]
pub(super) async fn execute_command_bounded(
    command: &str,
    cwd: OwnedFd,
    limit: usize,
    gate: Option<Arc<CommandTestGate>>,
) -> Result<CommandResult, CommandError> {
    execute_command_supervisor(command, cwd, limit, gate).await
}

#[cfg(not(test))]
async fn execute_command_bounded(
    command: &str,
    cwd: OwnedFd,
    limit: usize,
) -> Result<CommandResult, CommandError> {
    execute_command_supervisor(command, cwd, limit).await
}

async fn execute_command_supervisor(
    command: &str,
    cwd: OwnedFd,
    limit: usize,
    #[cfg(test)] gate: Option<Arc<CommandTestGate>>,
) -> Result<CommandResult, CommandError> {
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(command);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let raw_cwd = cwd.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            if libc::fchdir(raw_cwd) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let sigchld = signal(SignalKind::child()).map_err(|_| CommandError::Internal)?;
    let mut child = cmd.spawn().map_err(|_| CommandError::Launch)?;
    drop(cwd);

    let pid = match child.id() {
        Some(pid) => pid as libc::pid_t,
        None => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(CommandError::Internal);
        }
    };

    #[cfg(test)]
    if let Some(gate) = &gate {
        gate.notify_started(pid);
    }

    let guard = GroupKillGuard::new(
        pid,
        #[cfg(test)]
        gate.clone(),
    );

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            kill_and_reap(guard, child).await;
            return Err(CommandError::Internal);
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            kill_and_reap(guard, child).await;
            return Err(CommandError::Internal);
        }
    };

    let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
    let (result_tx, result_rx) = oneshot::channel::<Result<CommandResult, CommandError>>();

    #[cfg(test)]
    let gate_task = gate.clone();

    tokio::spawn(async move {
        run_supervisor_loop(Supervisor {
            guard,
            child,
            sigchld,
            stdout,
            stderr,
            limit,
            cancel_rx,
            result_tx,
            #[cfg(test)]
            gate: gate_task,
        })
        .await;
    });

    struct CancelOnDrop {
        cancel_tx: Option<oneshot::Sender<()>>,
    }
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            let _ = self.cancel_tx.take();
        }
    }

    let mut cancel_guard = CancelOnDrop {
        cancel_tx: Some(cancel_tx),
    };

    let result = result_rx.await.map_err(|_| CommandError::Internal)?;
    cancel_guard.cancel_tx = None;
    result
}

struct Supervisor {
    guard: GroupKillGuard,
    child: tokio::process::Child,
    sigchld: Signal,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    limit: usize,
    cancel_rx: oneshot::Receiver<()>,
    result_tx: oneshot::Sender<Result<CommandResult, CommandError>>,
    #[cfg(test)]
    gate: Option<Arc<CommandTestGate>>,
}

async fn run_supervisor_loop(supervisor: Supervisor) {
    let Supervisor {
        mut guard,
        mut child,
        sigchld,
        mut stdout,
        mut stderr,
        limit,
        mut cancel_rx,
        result_tx,
        #[cfg(test)]
        gate,
    } = supervisor;
    #[cfg(test)]
    let _supervisor_exit_guard = SupervisorExitGuard { gate: gate.clone() };

    let mut cancelled = false;
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_overflow = false;
    let mut stderr_overflow = false;
    let mut stdout_bytes = 0usize;
    let mut stderr_bytes = 0usize;
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    let mut child_waited = false;
    let mut child_status = None;
    let mut pipe_error = false;
    let mut wait_error = false;
    let mut chunk_out = [0u8; 16 * 1024];
    let mut chunk_err = [0u8; 16 * 1024];
    let mut child_exit = Box::pin(observe_child_termination(guard.pgid, sigchld));

    while !stdout_done || !stderr_done || !child_waited {
        #[cfg(test)]
        let reads_held = gate.as_ref().is_some_and(|gate| gate.reads_held());
        #[cfg(not(test))]
        let reads_held = false;
        #[cfg(test)]
        let wait_for_read_release = async {
            gate.as_ref().unwrap().wait_if_read_held().await;
        };
        #[cfg(not(test))]
        let wait_for_read_release = std::future::pending::<()>();

        tokio::select! {
            _ = &mut cancel_rx, if !cancelled => {
                cancelled = true;
                guard.kill();
            }
            res = stdout.read(&mut chunk_out), if !stdout_done && !reads_held => {
                match res {
                    Ok(0) => stdout_done = true,
                    Ok(n) => {
                        stdout_bytes += n;
                        if stdout_bytes > limit {
                            stdout_overflow = true;
                            guard.kill();
                        } else {
                            stdout_buf.extend_from_slice(&chunk_out[..n]);
                        }
                    }
                    Err(_) => {
                        stdout_done = true;
                        pipe_error = true;
                        guard.kill();
                    }
                }
            }
            res = stderr.read(&mut chunk_err), if !stderr_done && !reads_held => {
                match res {
                    Ok(0) => stderr_done = true,
                    Ok(n) => {
                        stderr_bytes += n;
                        if stderr_bytes > limit {
                            stderr_overflow = true;
                            guard.kill();
                        } else {
                            stderr_buf.extend_from_slice(&chunk_err[..n]);
                        }
                    }
                    Err(_) => {
                        stderr_done = true;
                        pipe_error = true;
                        guard.kill();
                    }
                }
            }
            res = &mut child_exit, if !child_waited => {
                guard.kill();
                match res {
                    Ok(()) => match child.wait().await {
                        Ok(status) => child_status = Some(status),
                        Err(_) => wait_error = true,
                    },
                    Err(()) => {
                        wait_error = true;
                        let _ = child.wait().await;
                    }
                }
                child_waited = true;
            }
            _ = wait_for_read_release, if reads_held => {}
        }
    }

    #[cfg(test)]
    if let Some(gate) = &gate {
        gate.notify_cleaned_up(guard.pgid);
    }

    if cancelled || cancellation_requested(&mut cancel_rx) {
        return;
    }

    if pipe_error {
        let _ = result_tx.send(Err(CommandError::Internal));
        return;
    }

    if stdout_overflow || stderr_overflow {
        #[cfg(test)]
        if let Some(gate) = &gate {
            gate.wait_if_error_held().await;
        }

        if cancellation_requested(&mut cancel_rx) {
            return;
        }

        let status_desc = match child_status {
            Some(status) => match status.code() {
                Some(code) => format!("exit status {code}"),
                None => match status.signal() {
                    Some(sig) => format!("terminated by signal {sig}"),
                    None => "terminated by signal".to_string(),
                },
            },
            None => "status unavailable".to_string(),
        };

        let msg = if stdout_overflow && stderr_overflow {
            let omitted_out = stdout_bytes.saturating_sub(limit);
            let omitted_err = stderr_bytes.saturating_sub(limit);
            let total_omitted = omitted_out + omitted_err;
            format!(
                "command stdout and stderr exceeded the {limit}-byte limit by {total_omitted} bytes ({status_desc}); the command ran and may have produced side effects; rerun with filtering or redirection"
            )
        } else if stdout_overflow {
            let omitted = stdout_bytes.saturating_sub(limit);
            format!(
                "command stdout exceeded the {limit}-byte limit by {omitted} bytes ({status_desc}); the command ran and may have produced side effects; rerun with filtering or redirection"
            )
        } else {
            let omitted = stderr_bytes.saturating_sub(limit);
            format!(
                "command stderr exceeded the {limit}-byte limit by {omitted} bytes ({status_desc}); the command ran and may have produced side effects; rerun with filtering or redirection"
            )
        };

        #[cfg(test)]
        let published = result_tx.send(Err(CommandError::Overflow(msg))).is_ok();
        #[cfg(not(test))]
        let _ = result_tx.send(Err(CommandError::Overflow(msg)));
        #[cfg(test)]
        if published && let Some(gate) = &gate {
            gate.notify_error_published();
        }
        return;
    }

    if wait_error {
        let _ = result_tx.send(Err(CommandError::Internal));
        return;
    }

    let exit_code = child_status.and_then(|status| status.code());
    let stdout = String::from_utf8_lossy(&stdout_buf).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_buf).into_owned();

    let _ = result_tx.send(Ok(CommandResult {
        exit_code,
        stdout,
        stderr,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::time::Duration;
    use tempfile::tempdir;

    fn test_cwd() -> (tempfile::TempDir, OwnedFd) {
        let dir = tempdir().unwrap();
        let file = fs::File::open(dir.path()).unwrap();
        (dir, OwnedFd::from(file))
    }

    async fn assert_signal_esrch(target: libc::pid_t) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if unsafe { libc::kill(target, 0) } == -1 {
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
        .expect("timed out waiting for process reaping");
    }

    async fn assert_process_absent(pid: libc::pid_t) {
        assert_signal_esrch(pid).await;
    }

    async fn assert_group_absent(pgid: libc::pid_t) {
        assert_signal_esrch(-pgid).await;
    }

    #[tokio::test]
    async fn independent_stdout_and_stderr() {
        let (_dir, cwd) = test_cwd();
        let output = execute_command("printf 'hello stdout'; printf 'hello stderr' >&2", cwd)
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello stdout");
        assert_eq!(output.stderr, "hello stderr");
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn pipe_buffer_fill_concurrency() {
        let (_dir, cwd) = test_cwd();
        let output = execute_command(
            "dd if=/dev/zero bs=1024 count=128 2>/dev/null & (dd if=/dev/zero bs=1024 count=128 2>/dev/null) 1>&2; wait",
            cwd,
        )
        .await
        .unwrap();
        assert_eq!(output.stdout.len(), 128 * 1024);
        assert_eq!(output.stderr.len(), 128 * 1024);
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn invalid_utf8_replacement() {
        let (_dir, cwd) = test_cwd();
        let output = execute_command("printf '\\377\\376' && printf '\\375' >&2", cwd)
            .await
            .unwrap();
        assert_eq!(output.stdout, "\u{FFFD}\u{FFFD}");
        assert_eq!(output.stderr, "\u{FFFD}");
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn invalid_utf8_counted_by_raw_bytes() {
        let (_dir, cwd) = test_cwd();
        let output = execute_command_bounded("printf 'a\\377bc'", cwd, 5, None)
            .await
            .unwrap();
        assert_eq!(output.stdout, "a\u{FFFD}bc");
        assert_eq!(output.stdout.len(), 6);

        let (_dir2, cwd2) = test_cwd();
        let err = execute_command_bounded("printf 'a\\377bcde'", cwd2, 5, None)
            .await
            .unwrap_err();
        match err {
            CommandError::Overflow(msg) => {
                assert!(msg.contains("stdout exceeded the 5-byte limit by 1 bytes"));
            }
            other => panic!("expected overflow error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exactly_8_mib_accepted_on_stdout() {
        let (_dir, cwd) = test_cwd();
        let output = execute_command("dd if=/dev/zero bs=8388608 count=1 2>/dev/null", cwd)
            .await
            .unwrap();
        assert_eq!(output.stdout.len(), 8 * 1024 * 1024);
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn one_byte_over_rejected_on_stdout() {
        let (_dir, cwd) = test_cwd();
        let err = execute_command("dd if=/dev/zero bs=8388609 count=1 2>/dev/null", cwd)
            .await
            .unwrap_err();
        match err {
            CommandError::Overflow(msg) => {
                assert!(msg.contains("stdout exceeded the 8388608-byte limit by 1 bytes"));
                assert!(msg.contains("the command ran and may have produced side effects"));
                assert!(msg.contains("rerun with filtering or redirection"));
            }
            other => panic!("expected overflow error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exactly_8_mib_accepted_on_stderr() {
        let (_dir, cwd) = test_cwd();
        let output = execute_command("(dd if=/dev/zero bs=8388608 count=1 2>/dev/null) 1>&2", cwd)
            .await
            .unwrap();
        assert_eq!(output.stderr.len(), 8 * 1024 * 1024);
        assert_eq!(output.exit_code, Some(0));
    }

    #[tokio::test]
    async fn one_byte_over_rejected_on_stderr() {
        let (_dir, cwd) = test_cwd();
        let err = execute_command("(dd if=/dev/zero bs=8388609 count=1 2>/dev/null) 1>&2", cwd)
            .await
            .unwrap_err();
        match err {
            CommandError::Overflow(msg) => {
                assert!(msg.contains("stderr exceeded the 8388608-byte limit by 1 bytes"));
                assert!(msg.contains("the command ran and may have produced side effects"));
                assert!(msg.contains("rerun with filtering or redirection"));
            }
            other => panic!("expected overflow error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn omitted_byte_accounting_in_diagnostic() {
        let (_dir, cwd) = test_cwd();
        let err = execute_command_bounded(
            "printf '1234567890' && printf 'abcdefghij' >&2",
            cwd,
            5,
            None,
        )
        .await
        .unwrap_err();
        match err {
            CommandError::Overflow(msg) => {
                assert!(msg.contains("stdout and stderr exceeded the 5-byte limit by 10 bytes"));
                assert!(msg.contains("the command ran and may have produced side effects"));
                assert!(msg.contains("rerun with filtering or redirection"));
                assert!(!msg.contains("1234567890"));
                assert!(!msg.contains("abcdefghij"));
            }
            other => panic!("expected overflow error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn bounded_overflow_after_shell_exit() {
        let (_dir, cwd) = test_cwd();
        let gate = CommandTestGate::new();
        gate.hold_read();
        let gate_clone = gate.clone();

        let handle = tokio::spawn(async move {
            execute_command_bounded("printf '%01024d' 0", cwd, 10, Some(gate_clone)).await
        });

        let pgid = gate.wait_until_started().await;
        gate.wait_until_kill_sent(pgid).await;
        gate.release_read();

        let err = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for command cleanup")
            .unwrap()
            .unwrap_err();
        match err {
            CommandError::Overflow(msg) => {
                assert!(msg.contains("stdout exceeded the 10-byte limit by 1014 bytes"));
                assert!(msg.contains("the command ran and may have produced side effects"));
                assert!(msg.contains("rerun with filtering or redirection"));
            }
            other => panic!("expected overflow error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn overflow_triggered_group_cleanup() {
        let (dir, cwd) = test_cwd();
        let pid_file = dir.path().join("bg.pid");
        let cmd = format!(
            "tail -f /dev/null & echo $! > '{}' && dd if=/dev/zero bs=8388609 count=1 2>/dev/null",
            pid_file.display()
        );
        let err = tokio::time::timeout(Duration::from_secs(5), execute_command(&cmd, cwd))
            .await
            .expect("timed out waiting for overflow cleanup")
            .unwrap_err();
        assert!(matches!(err, CommandError::Overflow(_)));

        let pid_str = fs::read_to_string(&pid_file).unwrap();
        let bg_pid: libc::pid_t = pid_str.trim().parse().unwrap();
        assert_process_absent(bg_pid).await;
    }

    #[tokio::test]
    async fn pipe_drainage_after_cleanup_and_no_partial_success() {
        let (_dir, cwd) = test_cwd();
        let err =
            execute_command_bounded("dd if=/dev/zero bs=100 count=1 2>/dev/null", cwd, 10, None)
                .await
                .unwrap_err();
        match err {
            CommandError::Overflow(msg) => {
                assert!(msg.contains("stdout exceeded the 10-byte limit by 90 bytes"));
            }
            other => panic!("expected overflow error, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exit_statuses_zero_nonzero_signal() {
        let (_dir, cwd1) = test_cwd();
        let out0 = execute_command("exit 0", cwd1).await.unwrap();
        assert_eq!(out0.exit_code, Some(0));

        let (_dir, cwd2) = test_cwd();
        let out42 = execute_command("exit 42", cwd2).await.unwrap();
        assert_eq!(out42.exit_code, Some(42));

        let (_dir, cwd3) = test_cwd();
        let out_sig = execute_command("kill -9 $$", cwd3).await.unwrap();
        assert_eq!(out_sig.exit_code, None);
    }

    #[tokio::test]
    async fn inherited_environment() {
        let (_dir, cwd) = test_cwd();
        let out = execute_command("echo \"$PATH\"", cwd).await.unwrap();
        let path_env = std::env::var("PATH").unwrap_or_default();
        assert_eq!(out.stdout.trim(), path_env);
    }

    #[tokio::test]
    async fn immediate_stdin_eof() {
        let (_dir, cwd) = test_cwd();
        let out = execute_command("cat", cwd).await.unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.stdout, "");
    }

    #[tokio::test]
    async fn child_side_descriptor_cwd_after_pathname_renamed_and_symlinked() {
        let parent = tempdir().unwrap();
        let dir_a = parent.path().join("dir_a");
        fs::create_dir(&dir_a).unwrap();
        let dir_b = parent.path().join("dir_b");
        let dir_c = parent.path().join("dir_c");
        fs::create_dir(&dir_c).unwrap();

        let file_a = fs::File::open(&dir_a).unwrap();
        let cwd = OwnedFd::from(file_a);

        fs::rename(&dir_a, &dir_b).unwrap();
        symlink(&dir_c, &dir_a).unwrap();

        let out = execute_command("pwd -P", cwd).await.unwrap();
        let real_b = fs::canonicalize(&dir_b).unwrap();
        assert_eq!(out.stdout.trim(), real_b.to_str().unwrap());
    }

    #[tokio::test]
    async fn shell_exit_with_pipe_holding_descendant() {
        let (_dir, cwd) = test_cwd();
        let out = tokio::time::timeout(
            Duration::from_secs(5),
            execute_command("tail -f /dev/null &", cwd),
        )
        .await
        .expect("command waited for its descendant's natural expiry")
        .unwrap();
        assert_eq!(out.exit_code, Some(0));
    }

    #[tokio::test]
    async fn group_wide_cleanup() {
        let (dir, cwd) = test_cwd();
        let pid_file = dir.path().join("descendants.pid");
        let cmd = format!(
            "tail -f /dev/null & echo \"$$ $!\" > '{}'; exit 0",
            pid_file.display()
        );
        let out = tokio::time::timeout(Duration::from_secs(5), execute_command(&cmd, cwd))
            .await
            .expect("command waited for its descendant's natural expiry")
            .unwrap();
        assert_eq!(out.exit_code, Some(0));

        let pids: Vec<libc::pid_t> = fs::read_to_string(&pid_file)
            .unwrap()
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(pids.len(), 2);
        assert_group_absent(pids[0]).await;
        assert_process_absent(pids[1]).await;
    }

    #[tokio::test]
    async fn drop_triggered_cleanup() {
        let (_dir, cwd) = test_cwd();
        let gate = CommandTestGate::new();
        let gate_clone = gate.clone();

        let handle = tokio::spawn(async move {
            execute_command_bounded(
                "tail -f /dev/null",
                cwd,
                COMMAND_STREAM_MAX_BYTES,
                Some(gate_clone),
            )
            .await
        });

        let pgid = gate.wait_until_started().await;
        assert!(pgid > 0);
        assert_eq!(unsafe { libc::kill(-pgid, 0) }, 0);

        handle.abort();
        gate.wait_until_cleaned_up(pgid).await;
        assert_group_absent(pgid).await;
        assert_process_absent(pgid).await;
    }

    #[tokio::test]
    async fn lifecycle_waits_do_not_reuse_prior_command_notifications() {
        let (_dir, first_cwd) = test_cwd();
        let gate = CommandTestGate::new();
        let first_gate = gate.clone();
        let first = tokio::spawn(async move {
            execute_command_bounded(
                "tail -f /dev/null",
                first_cwd,
                COMMAND_STREAM_MAX_BYTES,
                Some(first_gate),
            )
            .await
        });

        let first_pgid = gate.wait_until_started().await;
        first.abort();
        gate.wait_until_kill_sent(first_pgid).await;
        gate.wait_until_cleaned_up(first_pgid).await;
        let cancellation = tokio::time::timeout(Duration::from_secs(5), first)
            .await
            .expect("timed out cancelling first command")
            .unwrap_err();
        assert!(cancellation.is_cancelled());

        let (_dir, second_cwd) = test_cwd();
        let second_gate = gate.clone();
        let second = tokio::spawn(async move {
            execute_command_bounded(
                "tail -f /dev/null",
                second_cwd,
                COMMAND_STREAM_MAX_BYTES,
                Some(second_gate),
            )
            .await
        });

        let second_pgid = gate.wait_until_started().await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                gate.wait_until_kill_sent(second_pgid)
            )
            .await
            .is_err(),
            "first command's kill satisfied the second wait"
        );
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                gate.wait_until_cleaned_up(second_pgid)
            )
            .await
            .is_err(),
            "first command's cleanup satisfied the second wait"
        );

        second.abort();
        gate.wait_until_kill_sent(second_pgid).await;
        gate.wait_until_cleaned_up(second_pgid).await;
        let cancellation = tokio::time::timeout(Duration::from_secs(5), second)
            .await
            .expect("timed out cancelling second command")
            .unwrap_err();
        assert!(cancellation.is_cancelled());
        assert_group_absent(second_pgid).await;
    }

    #[tokio::test]
    async fn immediate_guard_cleanup_if_supervisor_dropped() {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("tail -f /dev/null");
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let pgid = child.id() as libc::pid_t;

        let gate = CommandTestGate::new();
        let guard = GroupKillGuard::new(pgid, Some(gate.clone()));
        assert_eq!(unsafe { libc::kill(-pgid, 0) }, 0);

        let unpolled_future = async move {
            let _held = guard;
            std::future::pending::<()>().await;
        };
        drop(unpolled_future);

        gate.wait_until_kill_sent(pgid).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || child.wait()),
        )
        .await
        .expect("timed out reaping killed child")
        .unwrap()
        .unwrap();
        assert_group_absent(pgid).await;
        assert_process_absent(pgid).await;
    }

    #[tokio::test]
    async fn launch_failure() {
        let temp = tempdir().unwrap();
        let file_path = temp.path().join("not_a_dir.txt");
        fs::write(&file_path, "hello").unwrap();
        let file = fs::File::open(&file_path).unwrap();
        let bad_cwd = OwnedFd::from(file);
        let err = execute_command("echo hello", bad_cwd).await.unwrap_err();
        assert_eq!(err.to_string(), "command could not be launched");
    }

    #[tokio::test]
    async fn hold_and_release_error_publication() {
        let (_dir, cwd) = test_cwd();
        let gate = CommandTestGate::new();
        gate.hold_error_publication();
        let gate_clone = gate.clone();

        let handle = tokio::spawn(async move {
            execute_command_bounded(
                "dd if=/dev/zero bs=100 count=1 2>/dev/null",
                cwd,
                10,
                Some(gate_clone),
            )
            .await
        });

        let pgid = gate.wait_until_started().await;
        gate.wait_until_kill_sent(pgid).await;
        gate.wait_until_cleaned_up(pgid).await;

        gate.release_error_publication();
        let res = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("timed out waiting for overflow publication")
            .unwrap();
        assert!(matches!(res, Err(CommandError::Overflow(_))));
        gate.wait_until_error_published().await;
    }

    #[tokio::test]
    async fn cancellation_wins_over_held_overflow_publication() {
        let (_dir, cwd) = test_cwd();
        let gate = CommandTestGate::new();
        gate.hold_error_publication();
        let gate_clone = gate.clone();

        let handle = tokio::spawn(async move {
            execute_command_bounded(
                "dd if=/dev/zero bs=100 count=1 2>/dev/null",
                cwd,
                10,
                Some(gate_clone),
            )
            .await
        });

        let pgid = gate.wait_until_started().await;
        gate.wait_until_cleaned_up(pgid).await;

        handle.abort();
        let cancellation = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("timed out cancelling command")
            .unwrap_err();
        assert!(cancellation.is_cancelled());

        gate.release_error_publication();
        gate.wait_until_supervisor_exited().await;
        assert!(!gate.error_published());
    }
}
