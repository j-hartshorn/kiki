use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Mutex;

use rustix::fs::{FlockOperation, fcntl_lock};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, value};

const SKILL_TRUST_FILE: &str = ".kiki/trusted-skills.toml";
const SKILL_TRUST_LOCK_FILE: &str = ".kiki/trusted-skills.lock";
static SKILL_TRUST_WRITE_MUTEX: Mutex<()> = Mutex::new(());

#[derive(Debug)]
pub(crate) enum SkillTrustSaveError {
    InvalidStore,
    NonCanonicalScope,
    Io(io::Error),
}

impl fmt::Display for SkillTrustSaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidStore => {
                formatter.write_str("existing project skill trust store is invalid")
            }
            Self::NonCanonicalScope => {
                formatter.write_str("project skill trust scope is not a canonical path")
            }
            Self::Io(error) => write!(
                formatter,
                "could not save project skill trust ({})",
                error.kind()
            ),
        }
    }
}

impl std::error::Error for SkillTrustSaveError {}

pub(crate) fn is_project_skill_scope_trusted(home: &Path, scope: &Path) -> bool {
    let Some(scope_key) = canonical_scope_key(scope) else {
        return false;
    };
    read_project_skill_trust_store(&home.join(SKILL_TRUST_FILE))
        .is_ok_and(|scopes| scopes.contains(&scope_key))
}

pub(crate) fn save_project_skill_scope_trust(
    home: &Path,
    scope: &Path,
) -> Result<(), SkillTrustSaveError> {
    let scope_key = canonical_scope_key(scope).ok_or(SkillTrustSaveError::NonCanonicalScope)?;
    let path = home.join(SKILL_TRUST_FILE);
    let parent = path.parent().ok_or(SkillTrustSaveError::InvalidStore)?;
    fs::create_dir_all(parent).map_err(SkillTrustSaveError::Io)?;
    let _process_lock = SKILL_TRUST_WRITE_MUTEX
        .lock()
        .map_err(|_| SkillTrustSaveError::InvalidStore)?;
    let mut lock_options = OpenOptions::new();
    lock_options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let lock_file = lock_options
        .open(home.join(SKILL_TRUST_LOCK_FILE))
        .map_err(SkillTrustSaveError::Io)?;
    if !lock_file
        .metadata()
        .map_err(SkillTrustSaveError::Io)?
        .is_file()
    {
        return Err(SkillTrustSaveError::InvalidStore);
    }
    fcntl_lock(&lock_file, FlockOperation::LockExclusive)
        .map_err(|error| SkillTrustSaveError::Io(error.into()))?;
    let mut scopes = read_project_skill_trust_store(&path)?;
    scopes.insert(scope_key);
    let mut array = Array::new();
    for scope in scopes {
        array.push(scope);
    }
    let mut document = DocumentMut::new();
    document["trusted_skill_scopes"] = value(array);
    let mut temporary = NamedTempFile::new_in(parent).map_err(SkillTrustSaveError::Io)?;
    temporary
        .write_all(document.to_string().as_bytes())
        .map_err(SkillTrustSaveError::Io)?;
    temporary
        .as_file()
        .sync_all()
        .map_err(SkillTrustSaveError::Io)?;
    temporary
        .persist(&path)
        .map(|_| ())
        .map_err(|error| SkillTrustSaveError::Io(error.error))
}

fn canonical_scope_key(scope: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;

    if !scope.is_absolute() || fs::canonicalize(scope).ok().as_deref() != Some(scope) {
        return None;
    }
    Some(scope_bytes_key(scope.as_os_str().as_bytes()))
}

fn scope_bytes_key(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut key = String::with_capacity(5 + bytes.len() * 2);
    key.push_str("unix:");
    for byte in bytes {
        key.push(HEX[(byte >> 4) as usize] as char);
        key.push(HEX[(byte & 0x0f) as usize] as char);
    }
    key
}

fn read_project_skill_trust_store(path: &Path) -> Result<BTreeSet<String>, SkillTrustSaveError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(SkillTrustSaveError::Io(error)),
    };
    if !file.metadata().map_err(SkillTrustSaveError::Io)?.is_file() {
        return Err(SkillTrustSaveError::InvalidStore);
    }
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(SkillTrustSaveError::Io)?;
    let document = content
        .parse::<DocumentMut>()
        .map_err(|_| SkillTrustSaveError::InvalidStore)?;
    if document
        .iter()
        .any(|(key, _)| key != "trusted_skill_scopes")
    {
        return Err(SkillTrustSaveError::InvalidStore);
    }
    let Some(item) = document.get("trusted_skill_scopes") else {
        return Ok(BTreeSet::new());
    };
    let array = item.as_array().ok_or(SkillTrustSaveError::InvalidStore)?;
    let mut scopes = BTreeSet::new();
    for entry in array.iter() {
        let text = entry.as_str().ok_or(SkillTrustSaveError::InvalidStore)?;
        let Some(hex) = text.strip_prefix("unix:") else {
            return Err(SkillTrustSaveError::InvalidStore);
        };
        if !hex.starts_with("2f")
            || hex.len() % 2 != 0
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(SkillTrustSaveError::InvalidStore);
        }
        scopes.insert(text.to_owned());
    }
    Ok(scopes)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use tempfile::tempdir;

    use crate::config::settings::{Input, resolve};

    #[test]
    fn project_skill_trust_persists_only_positive_canonical_scopes_in_user_store() {
        let home = tempdir().unwrap();
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let first_scope = first.path().canonicalize().unwrap();
        let second_scope = second.path().canonicalize().unwrap();
        assert!(!super::is_project_skill_scope_trusted(
            home.path(),
            &first_scope
        ));
        super::save_project_skill_scope_trust(home.path(), &first_scope).unwrap();
        assert!(super::is_project_skill_scope_trusted(
            home.path(),
            &first_scope
        ));
        assert!(!super::is_project_skill_scope_trusted(
            home.path(),
            &second_scope
        ));
        super::save_project_skill_scope_trust(home.path(), &second_scope).unwrap();
        assert!(super::is_project_skill_scope_trusted(
            home.path(),
            &first_scope
        ));
        assert!(super::is_project_skill_scope_trusted(
            home.path(),
            &second_scope
        ));
        let stored = fs::read_to_string(home.path().join(super::SKILL_TRUST_FILE)).unwrap();
        assert!(stored.contains(&super::canonical_scope_key(&first_scope).unwrap()));
        assert!(stored.contains(&super::canonical_scope_key(&second_scope).unwrap()));
        assert!(!stored.contains("false"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn project_skill_trust_round_trips_non_utf8_scope_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let name = std::ffi::OsString::from_vec(b"project-\xff".to_vec());
        let scope = project.path().join(name);
        fs::create_dir(&scope).unwrap();
        let scope = scope.canonicalize().unwrap();
        super::save_project_skill_scope_trust(home.path(), &scope).unwrap();
        assert!(super::is_project_skill_scope_trusted(home.path(), &scope));
        let stored = fs::read_to_string(home.path().join(super::SKILL_TRUST_FILE)).unwrap();
        assert!(!stored.contains('\u{fffd}'));
    }

    #[cfg(unix)]
    #[test]
    fn project_skill_trust_key_preserves_exact_non_utf8_unix_bytes() {
        assert_ne!(
            super::scope_bytes_key(b"/project/\xff"),
            super::scope_bytes_key(b"/project/\xfe")
        );
        assert_eq!(
            super::scope_bytes_key(b"/project/\xff"),
            "unix:2f70726f6a6563742fff"
        );
    }

    #[cfg(unix)]
    #[test]
    fn project_skill_trust_save_waits_for_concurrent_writer_lock() {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::Command;
        use std::thread;
        use std::time::{Duration, Instant};

        use rustix::fs::{FlockOperation, fcntl_lock};

        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let first = project.path().join("first");
        let second = project.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        super::save_project_skill_scope_trust(home.path(), &first).unwrap();

        let lock_path = home.path().join(".kiki/trusted-skills.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .unwrap();
        fcntl_lock(&lock, FlockOperation::LockExclusive).unwrap();

        let started = home.path().join("started");
        let done = home.path().join("done");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("config::skill_trust::tests::project_skill_trust_lock_helper")
            .env("KIKI_SKILL_TRUST_HELPER_HOME", home.path())
            .env("KIKI_SKILL_TRUST_HELPER_SCOPE", &second)
            .env("KIKI_SKILL_TRUST_HELPER_STARTED", &started)
            .env("KIKI_SKILL_TRUST_HELPER_DONE", &done)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !started.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(started.exists(), "child save did not start");
        thread::sleep(Duration::from_millis(100));
        let completed_while_locked = done.exists();
        drop(lock);
        let result = child.wait().unwrap();
        assert!(result.success());
        assert!(!completed_while_locked, "save bypassed the writer lock");
        assert!(super::is_project_skill_scope_trusted(home.path(), &first));
        assert!(super::is_project_skill_scope_trusted(home.path(), &second));
    }

    #[cfg(unix)]
    #[test]
    fn project_skill_trust_lock_helper() {
        let Some(home) = std::env::var_os("KIKI_SKILL_TRUST_HELPER_HOME") else {
            return;
        };
        let scope = std::env::var_os("KIKI_SKILL_TRUST_HELPER_SCOPE").unwrap();
        let started = std::env::var_os("KIKI_SKILL_TRUST_HELPER_STARTED").unwrap();
        let done = std::env::var_os("KIKI_SKILL_TRUST_HELPER_DONE").unwrap();
        fs::write(started, "ready").unwrap();
        super::save_project_skill_scope_trust(Path::new(&home), Path::new(&scope)).unwrap();
        fs::write(done, "saved").unwrap();
    }

    #[test]
    fn project_settings_cannot_approve_project_skill_trust() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let scope = project.path().canonicalize().unwrap();
        let project_settings = project.path().join(".kiki/config.toml");
        fs::create_dir_all(project_settings.parent().unwrap()).unwrap();
        fs::write(
            project_settings,
            format!("trusted_skill_scopes = [\"{}\"]\n", scope.display()),
        )
        .unwrap();
        let resolution = resolve(Input {
            home: Some(home.path().as_os_str().to_os_string()),
            workspace_root: Some(project.path().to_path_buf()),
            session_preferences: None,
            model: None,
            thinking: None,
        });
        assert!(
            resolution.problems().iter().any(|problem| problem.kind()
                == crate::config::StartupProblemKind::ProjectSettingsInvalid)
        );
        assert!(!super::is_project_skill_scope_trusted(home.path(), &scope));
    }

    #[test]
    fn failed_or_malformed_trust_save_fails_closed() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let scope = project.path().canonicalize().unwrap();
        fs::write(home.path().join(".kiki"), "file blocks settings directory").unwrap();
        assert!(matches!(
            super::save_project_skill_scope_trust(home.path(), &scope),
            Err(super::SkillTrustSaveError::Io(_))
        ));
        assert!(!super::is_project_skill_scope_trusted(home.path(), &scope));

        let other_home = tempdir().unwrap();
        let store = other_home.path().join(super::SKILL_TRUST_FILE);
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::write(&store, "trusted_skill_scopes = [true]").unwrap();
        assert!(!super::is_project_skill_scope_trusted(
            other_home.path(),
            &scope
        ));
        assert!(matches!(
            super::save_project_skill_scope_trust(other_home.path(), &scope),
            Err(super::SkillTrustSaveError::InvalidStore)
        ));
    }
}
