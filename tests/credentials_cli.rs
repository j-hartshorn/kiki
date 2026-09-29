//! End-to-end credential flows for an installed `kiki`: stored keys, the
//! embedded worker, and the hidden login prompt.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::tempdir;

/// `kiki` with a clean environment: no API keys and no `MONTY_BIN`, so it must
/// use stored keys and its embedded worker. It has no controlling terminal, so
/// a ready launch creates its session and then fails to open the TUI.
fn kiki(home: &Path, workspace: &Path, arguments: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kiki"));
    command
        .args(arguments)
        .current_dir(workspace)
        .env_clear()
        .env("HOME", home);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    command
}

fn text(output: &Output) -> String {
    format!(
        "status: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn login_with_piped_key(home: &Path, workspace: &Path, provider: &str, key: &str) -> Output {
    let mut child = kiki(home, workspace, &["login", provider])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run kiki login");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(format!("{key}\n").as_bytes())
        .expect("write key");
    child.wait_with_output().expect("wait for kiki login")
}

fn launch(home: &Path, workspace: &Path) -> Output {
    kiki(home, workspace, &[])
        .stdin(Stdio::null())
        .output()
        .expect("launch kiki")
}

fn listed_sessions(home: &Path, workspace: &Path) -> usize {
    let output = kiki(home, workspace, &["sessions"])
        .output()
        .expect("list sessions");
    assert!(output.status.success(), "{}", text(&output));
    String::from_utf8_lossy(&output.stdout).lines().count()
}

#[test]
fn stored_key_and_embedded_worker_launch_until_logout() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");

    let login = login_with_piped_key(home.path(), workspace.path(), "openai", "sk-e2e-stored");
    assert!(login.status.success(), "{}", text(&login));

    // No OPENAI_API_KEY and no MONTY_BIN: the stored key and embedded worker
    // are enough to configure Kiki, open the arena and create a session.
    let ready = launch(home.path(), workspace.path());
    let ready_stdout = String::from_utf8_lossy(&ready.stdout);
    assert!(ready_stdout.contains("Session: "), "{}", text(&ready));
    assert!(
        String::from_utf8_lossy(&ready.stderr).contains("terminal operation failed"),
        "{}",
        text(&ready)
    );
    assert!(!ready_stdout.contains("sk-e2e-stored"));
    assert_eq!(listed_sessions(home.path(), workspace.path()), 1);

    let logout = kiki(home.path(), workspace.path(), &["logout", "openai"])
        .output()
        .expect("run kiki logout");
    assert!(logout.status.success(), "{}", text(&logout));

    // Without a key, startup stops at configuration: no session is created.
    let blocked = launch(home.path(), workspace.path());
    assert!(!blocked.status.success(), "{}", text(&blocked));
    assert!(
        !String::from_utf8_lossy(&blocked.stdout).contains("Session: "),
        "{}",
        text(&blocked)
    );
    assert_eq!(listed_sessions(home.path(), workspace.path()), 1);
}

#[cfg(unix)]
fn with_umask(command: &mut Command, mask: libc::mode_t) -> &mut Command {
    use std::os::unix::process::CommandExt;
    // SAFETY: `umask` is async-signal-safe.
    unsafe {
        command.pre_exec(move || {
            libc::umask(mask);
            Ok(())
        });
    }
    command
}

#[cfg(unix)]
fn directory_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[cfg(unix)]
#[test]
fn login_after_a_first_launch_under_a_group_writable_umask() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");

    // The first launch creates ~/.kiki (for its session) with umask 002.
    let first = with_umask(&mut kiki(home.path(), workspace.path(), &[]), 0o002)
        .env("OPENAI_API_KEY", "sk-environment")
        .stdin(Stdio::null())
        .output()
        .expect("launch kiki");
    assert!(
        String::from_utf8_lossy(&first.stdout).contains("Session: "),
        "{}",
        text(&first)
    );
    let mode = directory_mode(&home.path().join(".kiki"));
    assert_eq!(mode & 0o077, 0, "~/.kiki mode {mode:o}");

    let login = login_with_piped_key(home.path(), workspace.path(), "openai", "sk-after-launch");
    assert!(login.status.success(), "{}", text(&login));
    assert!(!String::from_utf8_lossy(&login.stderr).contains("removed group"));
}

#[cfg(unix)]
#[test]
fn login_repairs_a_group_writable_kiki_directory_from_older_versions() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let directory = home.path().join(".kiki");
    std::fs::create_dir(&directory).expect("create ~/.kiki");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o775))
        .expect("make ~/.kiki group-writable");

    let login = login_with_piped_key(home.path(), workspace.path(), "openai", "sk-repaired");
    assert!(login.status.success(), "{}", text(&login));
    assert!(
        String::from_utf8_lossy(&login.stderr)
            .contains("removed group and other write access from ~/.kiki (was 775)"),
        "{}",
        text(&login)
    );
    assert_eq!(directory_mode(&directory), 0o755);

    let ready = launch(home.path(), workspace.path());
    assert!(
        String::from_utf8_lossy(&ready.stdout).contains("Session: "),
        "{}",
        text(&ready)
    );
}

#[cfg(unix)]
mod pseudo_terminal {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    struct Pty {
        master: std::fs::File,
        slave: OwnedFd,
    }

    fn open_pty() -> Pty {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: both out-pointers are valid; null name, termios and winsize are allowed.
        let result = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(result, 0, "openpty: {}", std::io::Error::last_os_error());
        // SAFETY: `openpty` returned two new descriptors that we now own.
        unsafe {
            Pty {
                master: std::fs::File::from_raw_fd(master),
                slave: OwnedFd::from_raw_fd(slave),
            }
        }
    }

    /// Reads the shared terminal settings through the master side, which stays
    /// valid after the session leader exits (macOS revokes the slave then).
    fn local_flags(pty: &Pty) -> libc::tcflag_t {
        let mut settings = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: `settings` is valid for writes; checked below.
        let result = unsafe { libc::tcgetattr(pty.master.as_raw_fd(), settings.as_mut_ptr()) };
        assert_eq!(result, 0, "tcgetattr: {}", std::io::Error::last_os_error());
        // SAFETY: initialised by the successful `tcgetattr`.
        unsafe { settings.assume_init() }.c_lflag
    }

    /// Starts `kiki login <provider>` with the pseudo-terminal as its
    /// controlling terminal and stdin, then waits until it has disabled echo.
    fn start_login(pty: &Pty, home: &std::path::Path, provider: &str) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kiki"));
        command
            .args(["login", provider])
            .env_clear()
            .env("HOME", home)
            .stdin(Stdio::from(pty.slave.try_clone().expect("clone slave")))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let child = command.spawn().expect("run kiki login");
        let deadline = Instant::now() + Duration::from_secs(10);
        while local_flags(pty) & libc::ECHO != 0 {
            assert!(Instant::now() < deadline, "kiki login never disabled echo");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    fn assert_terminal_restored(pty: &Pty) {
        let flags = local_flags(pty);
        for (flag, name) in [
            (libc::ECHO, "ECHO"),
            (libc::ICANON, "ICANON"),
            (libc::ISIG, "ISIG"),
        ] {
            assert_ne!(flags & flag, 0, "{name} was not restored");
        }
    }

    fn echoed(pty: &mut Pty) -> String {
        // SAFETY: plain fcntl on a descriptor we own.
        unsafe {
            let flags = libc::fcntl(pty.master.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(
                pty.master.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            );
        }
        let mut buffer = [0_u8; 4096];
        let read = std::io::Read::read(&mut pty.master, &mut buffer).unwrap_or(0);
        String::from_utf8_lossy(&buffer[..read]).into_owned()
    }

    #[test]
    fn hidden_prompt_stores_the_edited_key_without_echo_and_restores_the_terminal() {
        let home = tempdir().expect("create home");
        let mut pty = open_pty();
        let child = start_login(&pty, home.path(), "openai");
        pty.master
            .write_all(b"sk-ptyX\x7f-key\r")
            .expect("type key");
        let output = child.wait_with_output().expect("wait for kiki login");
        assert!(output.status.success(), "{output:?}");

        let auth = std::fs::read_to_string(home.path().join(".kiki/auth.toml")).expect("auth");
        assert!(auth.contains("api_key = \"sk-pty-key\""), "{auth}");
        assert!(!echoed(&mut pty).contains("sk-pty"));
        assert_terminal_restored(&pty);
    }

    #[test]
    fn ctrl_c_cancels_without_a_signal_and_restores_the_terminal() {
        let home = tempdir().expect("create home");
        let pty = open_pty();
        let child = start_login(&pty, home.path(), "anthropic");
        (&pty.master)
            .write_all(b"sk-partial\x03")
            .expect("type Ctrl-C");
        let output = child.wait_with_output().expect("wait for kiki login");

        // Exit status 1, not death by SIGINT with the terminal left without echo.
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("Cancelled; nothing was saved."));
        assert!(!home.path().join(".kiki/auth.toml").exists());
        assert_terminal_restored(&pty);
    }
}
