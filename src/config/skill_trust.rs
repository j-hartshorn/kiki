use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use rustix::fs::{FlockOperation, flock};
use tempfile::NamedTempFile;
use toml_edit::{Array, DocumentMut, value};

const SKILL_TRUST_FILE: &str = ".kiki/trusted-skills.toml";
const SKILL_TRUST_LOCK_FILE: &str = ".kiki/trusted-skills.lock";

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
    if !is_canonical_scope(scope) {
        return false;
    }
    let Some(scope) = scope.to_str() else {
        return false;
    };
    read_project_skill_trust_store(&home.join(SKILL_TRUST_FILE))
        .is_ok_and(|scopes| scopes.contains(scope))
}

pub(crate) fn save_project_skill_scope_trust(
    home: &Path,
    scope: &Path,
) -> Result<(), SkillTrustSaveError> {
    if !is_canonical_scope(scope) {
        return Err(SkillTrustSaveError::NonCanonicalScope);
    }
    // A non-UTF-8 scope can't be stored, so its trust lasts for this launch only.
    let Some(scope) = scope.to_str() else {
        return Ok(());
    };
    let path = home.join(SKILL_TRUST_FILE);
    let parent = path.parent().ok_or(SkillTrustSaveError::InvalidStore)?;
    fs::create_dir_all(parent).map_err(SkillTrustSaveError::Io)?;
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
    flock(&lock_file, FlockOperation::LockExclusive)
        .map_err(|error| SkillTrustSaveError::Io(error.into()))?;
    let mut scopes = read_project_skill_trust_store(&path)?;
    scopes.insert(scope.to_owned());
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

fn is_canonical_scope(scope: &Path) -> bool {
    scope.is_absolute() && fs::canonicalize(scope).ok().as_deref() == Some(scope)
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
        if !Path::new(text).is_absolute() {
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
        assert!(stored.contains(first_scope.to_str().unwrap()));
        assert!(stored.contains(second_scope.to_str().unwrap()));
        assert!(!stored.contains("false"));
    }

    #[test]
    fn project_skill_trust_reads_hand_written_plain_paths() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let scope = project.path().canonicalize().unwrap();
        let store = home.path().join(super::SKILL_TRUST_FILE);
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::write(
            &store,
            format!("trusted_skill_scopes = [\"{}\"]\n", scope.display()),
        )
        .unwrap();
        assert!(super::is_project_skill_scope_trusted(home.path(), &scope));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn project_skill_trust_does_not_store_non_utf8_scopes() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let scope = project.path().join(OsStr::from_bytes(b"project-\xff"));
        fs::create_dir(&scope).unwrap();
        let scope = scope.canonicalize().unwrap();
        super::save_project_skill_scope_trust(home.path(), &scope).unwrap();
        assert!(!home.path().join(super::SKILL_TRUST_FILE).exists());
        assert!(!super::is_project_skill_scope_trusted(home.path(), &scope));
    }

    #[cfg(unix)]
    #[test]
    fn project_skill_trust_save_waits_for_concurrent_writer_lock() {
        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::Command;
        use std::thread;
        use std::time::{Duration, Instant};

        use rustix::fs::{FlockOperation, flock};

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
        flock(&lock, FlockOperation::LockExclusive).unwrap();

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

    #[test]
    fn project_skill_trust_save_waits_for_lock_held_by_another_open_in_process() {
        use std::thread;
        use std::time::Duration;

        use rustix::fs::{FlockOperation, flock};

        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let scope = project.path().canonicalize().unwrap();
        fs::create_dir_all(home.path().join(".kiki")).unwrap();
        let lock = fs::File::create(home.path().join(super::SKILL_TRUST_LOCK_FILE)).unwrap();
        flock(&lock, FlockOperation::LockExclusive).unwrap();

        let save = thread::spawn({
            let home = home.path().to_path_buf();
            let scope = scope.clone();
            move || super::save_project_skill_scope_trust(&home, &scope)
        });
        thread::sleep(Duration::from_millis(100));
        let completed_while_locked = save.is_finished();
        drop(lock);
        save.join().unwrap().unwrap();
        assert!(!completed_while_locked, "save bypassed the writer lock");
        assert!(super::is_project_skill_scope_trusted(home.path(), &scope));
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

        for content in [
            "trusted_skill_scopes = [true]",
            "trusted_skill_scopes = [\"relative/project\"]",
        ] {
            let other_home = tempdir().unwrap();
            let store = other_home.path().join(super::SKILL_TRUST_FILE);
            fs::create_dir_all(store.parent().unwrap()).unwrap();
            fs::write(&store, content).unwrap();
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
}
