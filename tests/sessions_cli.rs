use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rusqlite::{Connection, params};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use tempfile::tempdir;
use unicode_segmentation::UnicodeSegmentation;

const PREFERENCES: &str =
    r#"{"selected_model":"openai/gpt-5.6-sol","thinking":{"openai/gpt-5.6-sol":"medium"}}"#;

fn path_codec(path: &Path) -> String {
    #[cfg(unix)]
    {
        path.as_os_str()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned()
    }
}

fn database_path(home: &Path, id: &str) -> PathBuf {
    home.join(".kiki/sessions").join(format!("{id}.sqlite3"))
}

fn create_session_fixture(
    home: &Path,
    id: &str,
    stored_id: &str,
    cwd: &Path,
    updated_at_ms: i64,
    name: Option<&str>,
    request: Option<&str>,
) -> PathBuf {
    let database = database_path(home, id);
    fs::create_dir_all(database.parent().expect("sessions directory"))
        .expect("create sessions directory");
    let connection = Connection::open(&database).expect("create session database");
    connection
        .execute_batch(include_str!("../src/session/schema.sql"))
        .expect("install production schema");
    connection
        .execute(
            "INSERT INTO checkpoints (runtime_identity, arena_state, authorisations_json) VALUES (?1, ?2, ?3)",
            params!["fixture", Vec::<u8>::new(), "[]"],
        )
        .expect("insert checkpoint");
    connection
        .execute(
            "INSERT INTO entries (kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES ('session_start', 1, '{}', ?1, 1)",
            params![updated_at_ms],
        )
        .expect("insert root entry");
    let active_entry_id = if let Some(request) = request {
        connection
            .execute("INSERT INTO runs DEFAULT VALUES", [])
            .expect("insert run");
        let payload = serde_json::json!({ "UserRequest": { "request": request } }).to_string();
        connection
            .execute(
                "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES (1, 1, 'user_request', 1, ?1, ?2, 2)",
                params![payload, updated_at_ms],
            )
            .expect("insert request");
        connection
            .execute(
                "INSERT INTO entries (parent_id, run_id, kind, checkpoint_id, payload_json, created_at_ms, revision) VALUES (2, 1, 'failed', 1, ?1, ?2, 3)",
                params![
                    r#"{"Terminal":{"terminal":{"Failed":{"message":"fixture","evidence":null,"attempts":[]}},"code":null}}"#,
                    updated_at_ms
                ],
            )
            .expect("insert terminal entry");
        connection
            .execute("UPDATE runs SET terminal_entry_id = 3 WHERE id = 1", [])
            .expect("close run");
        3
    } else {
        1
    };
    let revision = if request.is_some() { 3 } else { 1 };
    connection
        .execute(
            "INSERT INTO session (singleton, id, created_at_ms, updated_at_ms, cwd, name, active_entry_id, preferences_json, revision) VALUES (1, ?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                stored_id,
                updated_at_ms,
                path_codec(cwd),
                name,
                active_entry_id,
                PREFERENCES,
                revision,
            ],
        )
        .expect("insert session metadata");
    database
}

#[cfg(unix)]
fn detach_controlling_terminal(command: &mut Command) {
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

#[cfg(not(unix))]
fn detach_controlling_terminal(_command: &mut Command) {}

fn invoke(home: &Path, workspace: &Path, arguments: &[&str], trace: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kiki"));
    command
        .args(arguments)
        .current_dir(workspace)
        .env_clear()
        .env("HOME", home);
    if let Some(trace) = trace {
        command.env("KIKI_TRACE_FILE", trace);
    }
    detach_controlling_terminal(&mut command);
    command.output().expect("run kiki")
}

fn output_text(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn invoke_ready(
    home: &Path,
    workspace: &Path,
    arguments: &[&str],
    selection: Option<(&str, &str)>,
) -> Output {
    let monty = std::env::var_os("MONTY_BIN")
        .expect("MONTY_BIN must be exported for configured CLI integration tests");
    let mut command = Command::new(env!("CARGO_BIN_EXE_kiki"));
    command
        .args(arguments)
        .current_dir(workspace)
        .env_clear()
        .env("HOME", home)
        .env("MONTY_BIN", monty)
        .env("OPENAI_API_KEY", "test-api-key");
    if let Some((model, thinking)) = selection {
        command
            .env("KIKI_MODEL", model)
            .env("KIKI_THINKING", thinking);
    }
    detach_controlling_terminal(&mut command);
    command.output().expect("run configured kiki")
}

fn session_id(stdout: &str) -> String {
    let prefix = "Session: ";
    let start = stdout.find(prefix).expect("session ID output") + prefix.len();
    stdout[start..].chars().take(32).collect::<String>()
}

fn assert_headless_terminal_failure(stderr: &str) {
    assert!(stderr.contains("terminal operation failed"), "{stderr}");
}

#[test]
fn sessions_lists_without_runtime_configuration() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let trace = home.path().join("trace.jsonl");

    let output = Command::new(env!("CARGO_BIN_EXE_kiki"))
        .arg("sessions")
        .current_dir(workspace.path())
        .env_clear()
        .env("HOME", home.path())
        .env("KIKI_MODEL", "not-a-model")
        .env(
            "KIKI_SYSTEM_PROMPT",
            workspace.path().join("missing-system-prompt.md"),
        )
        .env("KIKI_TRACE_FILE", &trace)
        .output()
        .expect("run kiki");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    assert!(!trace.exists());
    assert!(!home.path().join(".kiki").exists());
}

#[cfg(unix)]
#[test]
fn sessions_lists_matching_healthy_sessions_without_mutation() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let elsewhere = tempdir().expect("create other workspace");
    let workspace_link = home.path().join("workspace-link");
    symlink(workspace.path(), &workspace_link).expect("create workspace symlink");
    let named = "00000000000000000000000000000003";
    let requested = "00000000000000000000000000000002";
    let empty = "00000000000000000000000000000001";
    let elsewhere_id = "00000000000000000000000000000004";
    create_session_fixture(
        home.path(),
        named,
        named,
        &workspace_link,
        3_000,
        Some(" Named\n\t session \u{1b}"),
        Some("ignored request"),
    );
    create_session_fixture(
        home.path(),
        requested,
        requested,
        workspace.path(),
        2_000,
        None,
        Some(&format!("request\t{}\u{1b}", "界".repeat(100))),
    );
    create_session_fixture(
        home.path(),
        empty,
        empty,
        workspace.path(),
        1_000,
        None,
        None,
    );
    create_session_fixture(
        home.path(),
        elsewhere_id,
        elsewhere_id,
        elsewhere.path(),
        4_000,
        Some("other workspace"),
        None,
    );
    let corrupt = "00000000000000000000000000000005";
    let corrupt_path = database_path(home.path(), corrupt);
    fs::write(&corrupt_path, b"not sqlite").expect("create corrupt session");
    let unreadable = "00000000000000000000000000000006";
    let unreadable_path = create_session_fixture(
        home.path(),
        unreadable,
        unreadable,
        workspace.path(),
        6_000,
        None,
        None,
    );
    let unreadable_before = fs::read(&unreadable_path).expect("snapshot unreadable session");
    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o000))
        .expect("make session unreadable");
    let locked = "00000000000000000000000000000007";
    create_session_fixture(
        home.path(),
        locked,
        locked,
        workspace.path(),
        500,
        Some("locked session"),
        None,
    );
    let lock_path = home
        .path()
        .join(".kiki/sessions")
        .join(format!("{locked}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("create lock file");
    rustix::fs::fcntl_lock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("lock session");
    let ignored_link = home
        .path()
        .join(".kiki/sessions/00000000000000000000000000000008.sqlite3");
    symlink(database_path(home.path(), empty), ignored_link).expect("create database symlink");
    fs::write(
        home.path().join(".kiki/sessions/not-a-session.sqlite3"),
        b"ignored",
    )
    .expect("create unrelated file");
    let trace = home.path().join("trace.jsonl");
    let candidate_bytes_before = [named, requested, empty, elsewhere_id, locked].map(|id| {
        (
            id,
            fs::read(database_path(home.path(), id)).expect("snapshot healthy session"),
        )
    });
    let corrupt_before = fs::read(&corrupt_path).expect("read corrupt session");

    let output = invoke(home.path(), workspace.path(), &["sessions"], Some(&trace));

    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o600))
        .expect("restore session permissions");
    let (stdout, stderr) = output_text(&output);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 4, "{stdout}");
    assert!(lines[0].starts_with(&format!("{named} 1970-01-01T00:00:03Z Named session \\x1B")));
    assert!(lines[1].starts_with(&format!("{requested} 1970-01-01T00:00:02Z ")));
    assert!(lines[1].ends_with('…'));
    let requested_description = lines[1].splitn(3, ' ').nth(2).expect("listing description");
    assert!(requested_description.graphemes(true).count() <= 80);
    assert!(!requested_description.contains('\u{1b}'));
    assert!(lines[2].starts_with(&format!("{empty} 1970-01-01T00:00:01Z (empty session)")));
    assert!(lines[3].starts_with(&format!("{locked} 1970-01-01T00:00:00.5Z locked session")));
    assert!(!stdout.contains(elsewhere_id));
    assert!(!stdout.contains("00000000000000000000000000000008"));
    assert!(stderr.contains(corrupt));
    assert!(stderr.contains(unreadable));
    assert!(!trace.exists());
    for (id, before) in candidate_bytes_before {
        assert_eq!(
            fs::read(database_path(home.path(), id)).expect("reread healthy session"),
            before,
            "listing modified session {id}"
        );
    }
    assert_eq!(
        fs::read(corrupt_path).expect("reread corrupt session"),
        corrupt_before
    );
    assert_eq!(
        fs::read(unreadable_path).expect("reread unreadable session"),
        unreadable_before
    );
}

#[test]
fn invalid_arguments_fail_before_home_lookup_or_tracing() {
    let workspace = tempdir().expect("create workspace");
    let malformed = "ABCDEF0123456789abcdef0123456789";
    for arguments in [
        vec!["sessions", "extra"],
        vec!["--resume"],
        vec!["--resume", "short"],
        vec!["--resume", malformed],
        vec!["--here"],
        vec!["--here", "--resume", "00000000000000000000000000000001"],
        vec!["--resume", "00000000000000000000000000000001", "unexpected"],
        vec!["sessions", "--here"],
        vec!["--continue", "--here"],
    ] {
        let trace = workspace.path().join(format!("{}.jsonl", arguments.len()));
        let output = Command::new(env!("CARGO_BIN_EXE_kiki"))
            .args(&arguments)
            .current_dir(workspace.path())
            .env_clear()
            .env("KIKI_TRACE_FILE", &trace)
            .output()
            .expect("run invalid invocation");
        let (stdout, stderr) = output_text(&output);
        assert!(
            !output.status.success(),
            "{arguments:?}: {stdout}\n{stderr}"
        );
        assert!(stderr.contains("Usage: kiki"), "{arguments:?}: {stderr}");
        assert!(!stderr.contains("HOME is unset"), "{arguments:?}: {stderr}");
        assert!(!trace.exists(), "{arguments:?}");
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_argument_fails_with_usage_before_home_lookup() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let workspace = tempdir().expect("create workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_kiki"))
        .arg(OsString::from_vec(vec![b'\xff']))
        .current_dir(workspace.path())
        .env_clear()
        .output()
        .expect("run invalid invocation");

    let (_, stderr) = output_text(&output);
    assert!(!output.status.success());
    assert!(stderr.contains("Usage: kiki"));
    assert!(!stderr.contains("HOME is unset"));
}

#[test]
fn resume_rejects_unknown_or_mismatched_sessions_without_replacement() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let unknown = "00000000000000000000000000000001";
    let unknown_output = invoke(home.path(), workspace.path(), &["--resume", unknown], None);
    let (_, unknown_error) = output_text(&unknown_output);
    assert!(!unknown_output.status.success());
    assert!(
        unknown_error.contains("session database is missing"),
        "{unknown_error}"
    );
    assert!(!home.path().join(".kiki/sessions").exists());

    let requested = "00000000000000000000000000000002";
    let stored = "00000000000000000000000000000003";
    let database = create_session_fixture(
        home.path(),
        requested,
        stored,
        workspace.path(),
        1,
        None,
        None,
    );
    let before = fs::read(&database).expect("read fixture database");
    let trace = home.path().join("trace.jsonl");
    let mismatch_output = invoke(
        home.path(),
        workspace.path(),
        &["--resume", requested],
        Some(&trace),
    );
    let (stdout, stderr) = output_text(&mismatch_output);
    assert!(!mismatch_output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("session database ID does not match its filename"));
    assert!(!stdout.contains("Session:"));
    assert!(!trace.exists());
    assert_eq!(fs::read(database).expect("reread fixture database"), before);
    assert!(
        !home
            .path()
            .join(format!(".kiki/sessions/{requested}.lock"))
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn resume_rejects_owned_or_different_workspace_before_runtime() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let elsewhere = tempdir().expect("create elsewhere");
    let owned = "00000000000000000000000000000009";
    let database =
        create_session_fixture(home.path(), owned, owned, workspace.path(), 1, None, None);
    let lock_path = home
        .path()
        .join(".kiki/sessions")
        .join(format!("{owned}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .expect("create owner lock");
    rustix::fs::fcntl_lock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("lock owner file");
    let before = fs::read(&database).expect("read owned fixture");
    let owned_output = invoke(home.path(), workspace.path(), &["--resume", owned], None);
    let (stdout, stderr) = output_text(&owned_output);
    assert!(!owned_output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("session is already owned by another process"),
        "{stderr}"
    );
    assert!(!stdout.contains("Session:"));
    assert_eq!(fs::read(database).expect("reread owned fixture"), before);

    let mismatched = "00000000000000000000000000000010";
    let database = create_session_fixture(
        home.path(),
        mismatched,
        mismatched,
        elsewhere.path(),
        1,
        None,
        None,
    );
    let before = fs::read(&database).expect("read mismatch fixture");
    let trace = home.path().join("mismatch-trace.jsonl");
    let mismatch_output = invoke(
        home.path(),
        workspace.path(),
        &["--resume", mismatched],
        Some(&trace),
    );
    let (stdout, stderr) = output_text(&mismatch_output);
    assert!(!mismatch_output.status.success(), "{stdout}\n{stderr}");
    assert!(stderr.contains("saved session belongs to another workspace"));
    assert!(!stdout.contains("Session:"));
    assert!(!trace.exists());
    assert_eq!(fs::read(database).expect("reread mismatch fixture"), before);
    assert!(
        !home
            .path()
            .join(format!(".kiki/sessions/{mismatched}.lock"))
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn resume_accepts_equivalent_workspace_and_prints_id_after_terminal_restore() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let workspace_link = home.path().join("workspace-link");
    symlink(workspace.path(), &workspace_link).expect("create workspace symlink");
    let id = "00000000000000000000000000000011";
    create_session_fixture(home.path(), id, id, workspace.path(), 1, None, None);

    let output = invoke(home.path(), &workspace_link, &["--resume", id], None);

    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    let restored = stdout
        .find("\u{1b}[?1049l")
        .expect("terminal restore output");
    let session = stdout
        .find(&format!("Session: {id}"))
        .expect("session ID output");
    assert!(restored < session, "{stdout:?}");
    assert!(stderr.contains("terminal operation failed"), "{stderr}");
}

#[test]
fn here_keeps_incompatible_session_metadata_unchanged() {
    let home = tempdir().expect("create home");
    let saved_workspace = tempdir().expect("create saved workspace");
    let current_workspace = tempdir().expect("create current workspace");
    let id = "00000000000000000000000000000013";
    let database =
        create_session_fixture(home.path(), id, id, saved_workspace.path(), 1, None, None);
    let connection = Connection::open(&database).expect("open incompatible fixture");
    let before: (String, String, i64, i64) = connection
        .query_row(
            "SELECT cwd, preferences_json, updated_at_ms, revision FROM session",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read metadata before restore");
    drop(connection);

    let output = invoke_ready(
        home.path(),
        current_workspace.path(),
        &["--resume", id, "--here"],
        None,
    );

    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert_headless_terminal_failure(&stderr);
    let restored = stdout
        .find("\u{1b}[?1049l")
        .expect("terminal restore output");
    let session = stdout
        .find(&format!("Session: {id}"))
        .expect("session ID output");
    assert!(restored < session, "{stdout:?}");
    assert!(!stderr.contains("session relocated"), "{stderr}");

    let connection = Connection::open(database).expect("reopen incompatible fixture");
    let after: (String, String, i64, i64) = connection
        .query_row(
            "SELECT cwd, preferences_json, updated_at_ms, revision FROM session",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read metadata after restore");
    assert_eq!(after, before);
}

#[test]
fn successful_process_resume_and_here_relocate_metadata_atomically() {
    let home = tempdir().expect("create home");
    let saved_workspace = tempdir().expect("create saved workspace");
    let relocated_workspace = tempdir().expect("create relocated workspace");
    let fresh = invoke_ready(
        home.path(),
        saved_workspace.path(),
        &[],
        Some(("openai/gpt-5.6-sol", "low")),
    );
    let (fresh_stdout, fresh_stderr) = output_text(&fresh);
    assert!(!fresh.status.success(), "{fresh_stdout}\n{fresh_stderr}");
    assert_headless_terminal_failure(&fresh_stderr);
    let id = session_id(&fresh_stdout);
    assert!(valid_id(&id));
    let database = database_path(home.path(), &id);
    let connection = Connection::open(&database).expect("open created session");
    let before: (String, Vec<u8>, i64) = connection
        .query_row(
            "SELECT session.cwd, checkpoints.arena_state, session.revision FROM session JOIN entries ON entries.id = session.active_entry_id JOIN checkpoints ON checkpoints.id = entries.checkpoint_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read initial session metadata");
    let entries_before: i64 = connection
        .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .expect("count initial entries");
    drop(connection);

    let resumed = invoke_ready(
        home.path(),
        saved_workspace.path(),
        &["--resume", &id],
        None,
    );
    let (resumed_stdout, resumed_stderr) = output_text(&resumed);
    assert!(
        !resumed.status.success(),
        "{resumed_stdout}\n{resumed_stderr}"
    );
    assert_headless_terminal_failure(&resumed_stderr);
    assert_eq!(session_id(&resumed_stdout), id);
    assert!(!resumed_stderr.contains("session relocated"));

    let relocated = invoke_ready(
        home.path(),
        relocated_workspace.path(),
        &["--resume", &id, "--here"],
        Some(("openai/gpt-5.6-terra", "high")),
    );
    let (relocated_stdout, relocated_stderr) = output_text(&relocated);
    assert!(
        !relocated.status.success(),
        "{relocated_stdout}\n{relocated_stderr}"
    );
    assert_headless_terminal_failure(&relocated_stderr);
    assert_eq!(session_id(&relocated_stdout), id);
    assert!(relocated_stderr.contains("session relocated"));
    let connection = Connection::open(&database).expect("reopen relocated session");
    let after: (String, String, Vec<u8>, i64) = connection
        .query_row(
            "SELECT session.cwd, session.preferences_json, checkpoints.arena_state, session.revision FROM session JOIN entries ON entries.id = session.active_entry_id JOIN checkpoints ON checkpoints.id = entries.checkpoint_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read relocated metadata");
    let entries_after: i64 = connection
        .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .expect("count relocated entries");
    assert_eq!(
        after.0,
        path_codec(
            &fs::canonicalize(relocated_workspace.path()).expect("resolve relocated workspace")
        )
    );
    assert_eq!(after.2, before.1);
    assert_eq!(entries_after, entries_before);
    assert_eq!(after.3, before.2 + 2);
    let preferences: serde_json::Value =
        serde_json::from_str(&after.1).expect("decode relocated preferences");
    assert_eq!(
        preferences["selected_model"],
        serde_json::Value::String("openai/gpt-5.6-terra".to_owned())
    );
    assert_eq!(
        preferences["thinking"]["openai/gpt-5.6-terra"],
        serde_json::Value::String("high".to_owned())
    );
    drop(connection);

    let resumed_without_environment = invoke_ready(
        home.path(),
        relocated_workspace.path(),
        &["--resume", &id],
        None,
    );
    let (stdout, stderr) = output_text(&resumed_without_environment);
    assert!(
        !resumed_without_environment.status.success(),
        "{stdout}\n{stderr}"
    );
    assert_headless_terminal_failure(&stderr);
    assert_eq!(session_id(&stdout), id);
    let connection = Connection::open(database).expect("open resumed session");
    let preferences_after_resume: String = connection
        .query_row("SELECT preferences_json FROM session", [], |row| row.get(0))
        .expect("read resumed preferences");
    assert_eq!(preferences_after_resume, after.1);
}

fn valid_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte <= b'f'))
}

#[test]
fn here_skips_directory_guard_but_does_not_relocate_after_configuration_failure() {
    let home = tempdir().expect("create home");
    let saved_workspace = tempdir().expect("create saved workspace");
    let current_workspace = tempdir().expect("create current workspace");
    let id = "00000000000000000000000000000012";
    let database =
        create_session_fixture(home.path(), id, id, saved_workspace.path(), 1, None, None);
    let before = fs::read(&database).expect("read fixture database");

    let output = invoke(
        home.path(),
        current_workspace.path(),
        &["--resume", id, "--here"],
        None,
    );

    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains(&format!("Session: {id}")));
    assert!(!stderr.contains("session relocated"));
    assert_eq!(fs::read(database).expect("reread fixture database"), before);
}

fn first_listed_id(home: &Path, workspace: &Path) -> String {
    let output = invoke(home, workspace, &["sessions"], None);
    let (stdout, stderr) = output_text(&output);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    stdout
        .lines()
        .next()
        .expect("listed session")
        .chars()
        .take(32)
        .collect()
}

#[cfg(unix)]
#[test]
fn continue_resumes_newest_session_for_current_workspace() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let elsewhere = tempdir().expect("create other workspace");
    let workspace_link = home.path().join("workspace-link");
    symlink(workspace.path(), &workspace_link).expect("create workspace symlink");
    let elsewhere_id = "00000000000000000000000000000021";
    let older = "00000000000000000000000000000022";
    let latest = "00000000000000000000000000000023";
    let tied = "00000000000000000000000000000024";
    create_session_fixture(
        home.path(),
        elsewhere_id,
        elsewhere_id,
        elsewhere.path(),
        4_000,
        None,
        None,
    );
    create_session_fixture(
        home.path(),
        older,
        older,
        workspace.path(),
        2_000,
        None,
        None,
    );
    create_session_fixture(home.path(), tied, tied, workspace.path(), 3_000, None, None);
    create_session_fixture(
        home.path(),
        latest,
        latest,
        &workspace_link,
        3_000,
        None,
        None,
    );
    assert_eq!(first_listed_id(home.path(), workspace.path()), latest);

    let output = invoke(home.path(), workspace.path(), &["--continue"], None);

    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!("Session: {latest}")),
        "{stdout}\n{stderr}"
    );
    assert!(!stdout.contains(elsewhere_id));
    assert!(!stdout.contains(older));
    assert!(!stdout.contains(tied));
}

#[cfg(unix)]
#[test]
fn continue_skips_unreadable_and_invalid_timestamp_sessions_with_warnings() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let healthy = "00000000000000000000000000000041";
    let invalid_timestamp = "00000000000000000000000000000042";
    let unreadable = "00000000000000000000000000000043";
    create_session_fixture(
        home.path(),
        healthy,
        healthy,
        workspace.path(),
        1_000,
        None,
        None,
    );
    create_session_fixture(
        home.path(),
        invalid_timestamp,
        invalid_timestamp,
        workspace.path(),
        i64::MAX,
        None,
        None,
    );
    let unreadable_path = create_session_fixture(
        home.path(),
        unreadable,
        unreadable,
        workspace.path(),
        6_000,
        None,
        None,
    );
    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o000))
        .expect("make session unreadable");
    let listed = first_listed_id(home.path(), workspace.path());

    let output = invoke(home.path(), workspace.path(), &["--continue"], None);

    fs::set_permissions(&unreadable_path, fs::Permissions::from_mode(0o600))
        .expect("restore session permissions");
    assert_eq!(listed, healthy);
    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!("Session: {healthy}")),
        "{stdout}\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("Warning: could not read session {unreadable}.")),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!(
            "Warning: could not read session {invalid_timestamp}."
        )),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn continue_fails_when_selected_session_is_owned_without_fallback() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let older = "00000000000000000000000000000051";
    let owned = "00000000000000000000000000000052";
    create_session_fixture(
        home.path(),
        older,
        older,
        workspace.path(),
        1_000,
        None,
        None,
    );
    create_session_fixture(
        home.path(),
        owned,
        owned,
        workspace.path(),
        2_000,
        None,
        None,
    );
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(home.path().join(format!(".kiki/sessions/{owned}.lock")))
        .expect("create owner lock");
    rustix::fs::fcntl_lock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .expect("lock owner file");
    let output = invoke(home.path(), workspace.path(), &["--continue"], None);

    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("session is already owned by another process"),
        "{stderr}"
    );
    assert!(!stdout.contains("Session:"), "{stdout}");
    assert!(
        !home
            .path()
            .join(format!(".kiki/sessions/{older}.lock"))
            .exists()
    );
}

#[test]
fn continue_without_matching_session_fails_without_session_or_runtime() {
    let home = tempdir().expect("create home");
    let workspace = tempdir().expect("create workspace");
    let elsewhere = tempdir().expect("create other workspace");
    let trace = home.path().join("trace.jsonl");

    let empty_output = invoke(home.path(), workspace.path(), &["--continue"], Some(&trace));

    let (stdout, stderr) = output_text(&empty_output);
    assert!(!empty_output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("no saved session for the current workspace"),
        "{stderr}"
    );
    assert!(!stderr.contains("terminal operation failed"), "{stderr}");
    assert!(!stdout.contains("Session:"), "{stdout}");
    assert!(!trace.exists());
    assert!(!home.path().join(".kiki").exists());

    let elsewhere_id = "00000000000000000000000000000061";
    create_session_fixture(
        home.path(),
        elsewhere_id,
        elsewhere_id,
        elsewhere.path(),
        1_000,
        None,
        None,
    );
    let sessions = home.path().join(".kiki/sessions");
    let entries = |directory: &Path| {
        let mut names = fs::read_dir(directory)
            .expect("read sessions directory")
            .map(|entry| entry.expect("read session entry").file_name())
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let entries_before = entries(&sessions);

    let output = invoke(home.path(), workspace.path(), &["--continue"], Some(&trace));

    let (stdout, stderr) = output_text(&output);
    assert!(!output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("no saved session for the current workspace"),
        "{stderr}"
    );
    assert!(!stderr.contains("terminal operation failed"), "{stderr}");
    assert!(!stdout.contains("Session:"), "{stdout}");
    assert!(!trace.exists());
    assert_eq!(entries(&sessions), entries_before);
}
