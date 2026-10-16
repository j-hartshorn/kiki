//! Stored provider API keys for a user installation: `~/.kiki/auth.toml`.
//!
//! ```toml
//! [openai]
//! api_key = "sk-..."
//!
//! [anthropic]
//! api_key = "sk-ant-..."
//! ```
//!
//! Environment variables (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`) take
//! precedence over stored keys. The file holds secrets, so it lives apart from
//! `config.toml` (which is safe to share) and must be accessible by its owner
//! only: Kiki writes it with mode `0600` and refuses to read a file that other
//! users can access. Diagnostics never echo file content, which may be a key.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, value};

use super::create_private_dir_all;
use super::models::Provider;

pub(crate) const AUTH_FILE_LABEL: &str = "~/.kiki/auth.toml";
const API_KEY_FIELD: &str = "api_key";

pub(crate) fn auth_path(home: &Path) -> PathBuf {
    home.join(".kiki").join("auth.toml")
}

/// The table name (and `kiki login` argument) for a provider.
pub(crate) fn provider_id(provider: Provider) -> &'static str {
    match provider {
        Provider::OpenAi => "openai",
        Provider::Anthropic => "anthropic",
        Provider::Google => "google",
    }
}

pub(crate) fn parse_provider(id: &str) -> Option<Provider> {
    match id {
        "openai" => Some(Provider::OpenAi),
        "anthropic" => Some(Provider::Anthropic),
        _ => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StoredKeys {
    openai: Option<String>,
    anthropic: Option<String>,
    google: Option<String>,
}

impl StoredKeys {
    pub(crate) fn get(&self, provider: Provider) -> Option<&str> {
        match provider {
            Provider::OpenAi => self.openai.as_deref(),
            Provider::Anthropic => self.anthropic.as_deref(),
            Provider::Google => self.google.as_deref(),
        }
    }

    fn slot_mut(&mut self, provider: Provider) -> &mut Option<String> {
        match provider {
            Provider::OpenAi => &mut self.openai,
            Provider::Anthropic => &mut self.anthropic,
            Provider::Google => &mut self.google,
        }
    }
}

#[derive(Debug)]
pub(crate) enum AuthError {
    Unreadable(io::Error),
    InvalidUtf8,
    Malformed,
    Invalid(&'static str),
    InsecurePermissions {
        mode: u32,
    },
    InsecureDirectory {
        mode: u32,
    },
    NotOwned {
        what: &'static str,
    },
    WrongType {
        what: &'static str,
        expected: &'static str,
    },
    Unwritable(io::Error),
}

impl AuthError {
    pub(crate) fn remediation(&self) -> String {
        match self {
            Self::InsecurePermissions { .. } => format!(
                "Run `chmod 600 {AUTH_FILE_LABEL}` (and rotate the keys if the file was shared), then restart Kiki."
            ),
            Self::InsecureDirectory { .. } => "Run `chmod go-w ~/.kiki` (or `chmod 700 ~/.kiki`), check that nobody else changed ~/.kiki/auth.toml, then try again.".to_owned(),
            Self::NotOwned { .. } => format!(
                "Make sure ~/.kiki and {AUTH_FILE_LABEL} belong to your user (or delete them and run `kiki login <provider>`), then try again."
            ),
            Self::WrongType { what, .. } => format!(
                "Move or remove {what} so that Kiki can recreate it, then try again."
            ),
            Self::Unreadable(_) => format!(
                "Check that you can access ~/.kiki and {AUTH_FILE_LABEL}, then try again."
            ),
            Self::Unwritable(_) => {
                "Check that ~/.kiki is a writable directory, then try again.".to_owned()
            }
            _ => format!(
                "Fix {AUTH_FILE_LABEL}, or delete it and run `kiki login <provider>`, then restart Kiki."
            ),
        }
    }
}

impl fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(error) => {
                write!(formatter, "could not read {AUTH_FILE_LABEL}: {error}")
            }
            Self::InvalidUtf8 => write!(formatter, "{AUTH_FILE_LABEL} is not valid UTF-8"),
            Self::Malformed => write!(formatter, "{AUTH_FILE_LABEL} is not valid TOML"),
            Self::Invalid(detail) => write!(formatter, "{AUTH_FILE_LABEL} is invalid: {detail}"),
            Self::InsecurePermissions { mode } => write!(
                formatter,
                "{AUTH_FILE_LABEL} is accessible by other users (mode {mode:03o}), so Kiki will not use it"
            ),
            Self::InsecureDirectory { mode } => write!(
                formatter,
                "~/.kiki is writable by other users (mode {mode:03o}), so Kiki will not use {AUTH_FILE_LABEL}"
            ),
            Self::WrongType { what, expected } => write!(
                formatter,
                "{what} is not a {expected}, so Kiki cannot use {AUTH_FILE_LABEL}"
            ),
            Self::NotOwned { what } => write!(
                formatter,
                "{what} belongs to another user, so Kiki will not use {AUTH_FILE_LABEL}"
            ),
            Self::Unwritable(error) => {
                write!(formatter, "could not write {AUTH_FILE_LABEL}: {error}")
            }
        }
    }
}

impl Error for AuthError {}

/// Reads the stored keys. A missing file is an empty store.
pub(crate) fn load(home: &Path) -> Result<StoredKeys, AuthError> {
    match read(&auth_path(home), Permissions::Enforce)? {
        Some(content) => parse(&content).map(|(keys, _)| keys),
        None => Ok(StoredKeys::default()),
    }
}

/// Stores (or replaces) one provider's key, preserving the others.
pub(crate) fn store(home: &Path, provider: Provider, api_key: &str) -> Result<(), AuthError> {
    if api_key.trim().is_empty() {
        return Err(AuthError::Invalid("an API key must not be empty"));
    }
    update(home, |document| {
        let mut table = Table::new();
        table.insert(API_KEY_FIELD, value(api_key.trim()));
        document.insert(provider_id(provider), Item::Table(table));
        true
    })
    .map(|_| ())
}

/// Removes one provider's key. Returns whether a key was stored; the file is
/// left untouched when it was not.
pub(crate) fn remove(home: &Path, provider: Provider) -> Result<bool, AuthError> {
    // Only a missing file means "no key"; an unreadable one is an error, not a no-op.
    match fs::metadata(auth_path(home)) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(AuthError::Unreadable(error)),
    }
    update(home, |document| {
        document.remove(provider_id(provider)).is_some()
    })
}

/// Removes group and other write access from an existing `~/.kiki` owned by
/// the current user, so stored keys can be used. Returns the previous mode if
/// it changed. Older Kiki versions created `~/.kiki` with the default umask,
/// which is group-writable on some systems.
pub(crate) fn secure_directory(home: &Path) -> Result<Option<u32>, AuthError> {
    let directory = home.join(".kiki");
    let metadata = match fs::metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AuthError::Unreadable(error)),
    };
    if !metadata.is_dir() {
        return Err(AuthError::WrongType {
            what: "~/.kiki",
            expected: "directory",
        });
    }
    ensure_owned(&metadata, "~/.kiki")?;
    tighten_directory(&directory, &metadata)
}

#[cfg(unix)]
fn tighten_directory(directory: &Path, metadata: &fs::Metadata) -> Result<Option<u32>, AuthError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 == 0 {
        return Ok(None);
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(mode & !0o022))
        .map_err(AuthError::Unwritable)?;
    Ok(Some(mode))
}

#[cfg(not(unix))]
fn tighten_directory(
    _directory: &Path,
    _metadata: &fs::Metadata,
) -> Result<Option<u32>, AuthError> {
    Ok(None)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Permissions {
    Enforce,
    /// Rewriting replaces the file with a private one, which repairs its mode.
    Repair,
}

/// Read-modify-write under `~/.kiki/auth.lock`, so concurrent `kiki login`s
/// cannot drop each other's keys. The directory is checked before anything in
/// it is opened. `change` returns whether it modified the document; the file is
/// rewritten only if so.
fn update(home: &Path, change: impl FnOnce(&mut DocumentMut) -> bool) -> Result<bool, AuthError> {
    let path = auth_path(home);
    let directory = path
        .parent()
        .ok_or(AuthError::Invalid("the auth file has no parent directory"))?;
    create_private_dir_all(directory).map_err(AuthError::Unwritable)?;
    ensure_safe_directory(&fs::metadata(directory).map_err(AuthError::Unreadable)?)?;
    let _lock = lock(directory).map_err(AuthError::Unwritable)?;
    // Refuse to clobber a file that does not parse: it may hold hand-edited keys.
    let mut document = match read(&path, Permissions::Repair)? {
        Some(content) => parse(&content)?.1,
        None => DocumentMut::new(),
    };
    let changed = change(&mut document);
    if changed {
        write_private(&path, &document.to_string()).map_err(AuthError::Unwritable)?;
    }
    Ok(changed)
}

/// Takes an exclusive lock on `auth.lock` in the (already checked) directory;
/// the auth file itself is replaced by rename, so it cannot carry the lock.
/// Symlinks are not followed. Released on drop.
fn lock(directory: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options.open(directory.join("auth.lock"))?;
    file.lock()?;
    Ok(file)
}

/// Reads the auth file after checking that `~/.kiki` is owned by the current
/// user and not writable by others. Ownership (and, when enforced, the owner-only
/// mode) is checked on the opened file, and the content is read from that same
/// handle, so the checked file is the one used.
fn read(path: &Path, permissions: Permissions) -> Result<Option<String>, AuthError> {
    let directory = path
        .parent()
        .ok_or(AuthError::Invalid("the auth file has no parent directory"))?;
    match fs::metadata(directory) {
        Ok(metadata) => ensure_safe_directory(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AuthError::Unreadable(error)),
    }
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AuthError::Unreadable(error)),
    };
    let metadata = file.metadata().map_err(AuthError::Unreadable)?;
    if !metadata.is_file() {
        return Err(AuthError::WrongType {
            what: AUTH_FILE_LABEL,
            expected: "regular file",
        });
    }
    ensure_owned(&metadata, "the auth file")?;
    if permissions == Permissions::Enforce {
        ensure_private(&metadata)?;
    }
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut file, &mut bytes).map_err(AuthError::Unreadable)?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| AuthError::InvalidUtf8)
}

#[cfg(unix)]
fn ensure_safe_directory(metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;

    if !metadata.is_dir() {
        return Err(AuthError::WrongType {
            what: "~/.kiki",
            expected: "directory",
        });
    }
    ensure_owned(metadata, "~/.kiki")?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o022 == 0 {
        Ok(())
    } else {
        Err(AuthError::InsecureDirectory { mode })
    }
}

#[cfg(not(unix))]
fn ensure_safe_directory(_metadata: &fs::Metadata) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn ensure_owned(metadata: &fs::Metadata, what: &'static str) -> Result<(), AuthError> {
    use std::os::unix::fs::MetadataExt;

    // SAFETY: `geteuid` has no preconditions and cannot fail.
    if metadata.uid() == unsafe { libc::geteuid() } {
        Ok(())
    } else {
        Err(AuthError::NotOwned { what })
    }
}

#[cfg(not(unix))]
fn ensure_owned(_metadata: &fs::Metadata, _what: &'static str) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(unix)]
fn ensure_private(metadata: &fs::Metadata) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(AuthError::InsecurePermissions { mode })
    }
}

#[cfg(not(unix))]
fn ensure_private(_metadata: &fs::Metadata) -> Result<(), AuthError> {
    Ok(())
}

fn parse(content: &str) -> Result<(StoredKeys, DocumentMut), AuthError> {
    let document = content
        .parse::<DocumentMut>()
        .map_err(|_| AuthError::Malformed)?;
    let mut keys = StoredKeys::default();
    for (name, item) in document.iter() {
        let provider = parse_provider(name).ok_or(AuthError::Invalid(
            "it contains an unknown table; only [openai] and [anthropic] are allowed",
        ))?;
        let table = item
            .as_table_like()
            .ok_or(AuthError::Invalid("each provider entry must be a table"))?;
        if table.iter().any(|(field, _)| field != API_KEY_FIELD) {
            return Err(AuthError::Invalid(
                "a provider table contains an unknown field; only `api_key` is allowed",
            ));
        }
        if let Some(item) = table.get(API_KEY_FIELD) {
            let api_key = item
                .as_str()
                .ok_or(AuthError::Invalid("`api_key` must be a string"))?;
            if api_key.trim().is_empty() {
                return Err(AuthError::Invalid("`api_key` must not be empty"));
            }
            *keys.slot_mut(provider) = Some(api_key.trim().to_owned());
        }
    }
    Ok((keys, document))
}

/// Atomically replaces `path` with an owner-only (`0600`) file.
fn write_private(path: &Path, content: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("auth file has no parent directory"))?;
    create_private_dir_all(parent)?;
    // `NamedTempFile` creates its file with mode 0600 on Unix.
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{AuthError, StoredKeys, auth_path, load, remove, store};
    use crate::config::models::Provider;

    fn write_auth(home: &std::path::Path, content: &str) {
        let path = auth_path(home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn missing_file_is_an_empty_store() {
        let home = tempdir().unwrap();
        assert_eq!(load(home.path()).unwrap(), StoredKeys::default());
    }

    #[test]
    fn store_round_trips_and_preserves_other_providers() {
        let home = tempdir().unwrap();
        store(home.path(), Provider::OpenAi, "sk-open").unwrap();
        store(home.path(), Provider::Anthropic, "sk-ant").unwrap();
        store(home.path(), Provider::OpenAi, "sk-open-2").unwrap();
        let keys = load(home.path()).unwrap();
        assert_eq!(keys.get(Provider::OpenAi), Some("sk-open-2"));
        assert_eq!(keys.get(Provider::Anthropic), Some("sk-ant"));

        assert!(remove(home.path(), Provider::OpenAi).unwrap());
        assert!(!remove(home.path(), Provider::OpenAi).unwrap());
        let keys = load(home.path()).unwrap();
        assert_eq!(keys.get(Provider::OpenAi), None);
        assert_eq!(keys.get(Provider::Anthropic), Some("sk-ant"));
    }

    #[test]
    fn removing_an_absent_key_leaves_the_file_untouched() {
        let home = tempdir().unwrap();
        let content = "# my keys\n[openai]\napi_key =   \"sk-open\"\n";
        write_auth(home.path(), content);
        assert!(!remove(home.path(), Provider::Anthropic).unwrap());
        assert_eq!(fs::read_to_string(auth_path(home.path())).unwrap(), content);
    }

    #[test]
    fn concurrent_stores_keep_every_key() {
        let home = tempdir().unwrap();
        let providers = [Provider::OpenAi, Provider::Anthropic];
        for round in 0..20 {
            std::thread::scope(|scope| {
                for provider in providers {
                    let home = home.path();
                    scope.spawn(move || {
                        store(home, provider, &format!("sk-{round}")).unwrap();
                    });
                }
            });
            let keys = load(home.path()).unwrap();
            for provider in providers {
                assert_eq!(keys.get(provider), Some(format!("sk-{round}").as_str()));
            }
        }
    }

    #[test]
    fn remove_without_a_file_does_not_create_one() {
        let home = tempdir().unwrap();
        assert!(!remove(home.path(), Provider::OpenAi).unwrap());
        assert!(!auth_path(home.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn stored_file_and_new_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        store(home.path(), Provider::OpenAi, "sk-open").unwrap();
        let path = auth_path(home.path());
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn shared_file_is_refused_and_rewriting_repairs_it() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        write_auth(home.path(), "[openai]\napi_key = \"sk-open\"\n");
        let path = auth_path(home.path());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = load(home.path()).unwrap_err();
        assert!(matches!(
            error,
            AuthError::InsecurePermissions { mode: 0o644 }
        ));
        assert!(error.remediation().contains("chmod 600"));

        store(home.path(), Provider::Anthropic, "sk-ant").unwrap();
        let keys = load(home.path()).unwrap();
        assert_eq!(keys.get(Provider::OpenAi), Some("sk-open"));
        assert_eq!(keys.get(Provider::Anthropic), Some("sk-ant"));
    }

    #[test]
    fn stored_keys_are_trimmed() {
        let home = tempdir().unwrap();
        write_auth(home.path(), "[openai]\napi_key = \"  sk-padded \\n\"\n");
        assert_eq!(
            load(home.path()).unwrap().get(Provider::OpenAi),
            Some("sk-padded")
        );
    }

    #[cfg(unix)]
    #[test]
    fn writable_by_others_directory_is_refused_for_reads_and_writes() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        write_auth(home.path(), "[openai]\napi_key = \"sk-open\"\n");
        let directory = auth_path(home.path()).parent().unwrap().to_path_buf();
        for mode in [0o777, 0o720, 0o702] {
            fs::set_permissions(&directory, fs::Permissions::from_mode(mode)).unwrap();
            let error = load(home.path()).unwrap_err();
            assert!(
                matches!(error, AuthError::InsecureDirectory { mode: found } if found == mode),
                "{mode:o}: {error:?}"
            );
            assert!(error.remediation().contains("chmod go-w ~/.kiki"));
            assert!(matches!(
                store(home.path(), Provider::Anthropic, "sk-ant"),
                Err(AuthError::InsecureDirectory { .. })
            ));
            assert!(matches!(
                remove(home.path(), Provider::OpenAi),
                Err(AuthError::InsecureDirectory { .. })
            ));
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            load(home.path()).unwrap().get(Provider::OpenAi),
            Some("sk-open")
        );
        assert_eq!(load(home.path()).unwrap().get(Provider::Anthropic), None);
    }

    #[cfg(unix)]
    #[test]
    fn remove_reports_an_inaccessible_directory_instead_of_no_key() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        store(home.path(), Provider::OpenAi, "sk-open").unwrap();
        let directory = auth_path(home.path()).parent().unwrap().to_path_buf();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
        let result = remove(home.path(), Provider::OpenAi);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            matches!(result, Err(AuthError::Unreadable(_))),
            "{result:?}"
        );
        assert_eq!(
            load(home.path()).unwrap().get(Provider::OpenAi),
            Some("sk-open")
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_regular_auth_file_is_refused() {
        let home = tempdir().unwrap();
        fs::create_dir_all(auth_path(home.path())).unwrap();
        assert!(matches!(
            load(home.path()),
            Err(AuthError::WrongType {
                expected: "regular file",
                ..
            })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unsafe_directory_is_refused_before_anything_in_it_is_opened() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        let directory = home.path().join(".kiki");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o777)).unwrap();
        let planted_target = home.path().join("planted-target");
        std::os::unix::fs::symlink(&planted_target, directory.join("auth.lock")).unwrap();

        assert!(matches!(
            store(home.path(), Provider::OpenAi, "sk-open"),
            Err(AuthError::InsecureDirectory { mode: 0o777 })
        ));
        assert!(!planted_target.exists());

        // Even in a safe directory, a planted lock symlink is not followed.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(
            store(home.path(), Provider::OpenAi, "sk-open"),
            Err(AuthError::Unwritable(_))
        ));
        assert!(!planted_target.exists());
    }

    #[cfg(unix)]
    #[test]
    fn secure_directory_removes_group_and_other_write_access_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempdir().unwrap();
        assert_eq!(super::secure_directory(home.path()).unwrap(), None);
        let directory = home.path().join(".kiki");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o775)).unwrap();

        assert_eq!(super::secure_directory(home.path()).unwrap(), Some(0o775));
        let mode = fs::metadata(&directory).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(super::secure_directory(home.path()).unwrap(), None);
        store(home.path(), Provider::OpenAi, "sk-open").unwrap();
    }

    #[test]
    fn stored_keys_are_written_trimmed() {
        let home = tempdir().unwrap();
        store(home.path(), Provider::OpenAi, "  sk-open\n").unwrap();
        let content = fs::read_to_string(auth_path(home.path())).unwrap();
        assert!(content.contains("api_key = \"sk-open\""), "{content}");
    }

    #[test]
    fn accepts_inline_tables() {
        let home = tempdir().unwrap();
        write_auth(home.path(), "openai = { api_key = \"sk-inline\" }\n");
        assert_eq!(
            load(home.path()).unwrap().get(Provider::OpenAi),
            Some("sk-inline")
        );
    }

    #[test]
    fn invalid_files_are_rejected_without_echoing_content() {
        let home = tempdir().unwrap();
        for content in [
            "[openai]\napi_key = \"sk-secret\n",
            "[sk-secret]\napi_key = \"x\"\n",
            "[openai]\ntoken = \"sk-secret\"\n",
            "[openai]\napi_key = 7\n",
            "[openai]\napi_key = \"  \"\n",
            "openai = \"sk-secret\"\n",
        ] {
            write_auth(home.path(), content);
            let error = load(home.path()).unwrap_err();
            assert!(
                matches!(error, AuthError::Malformed | AuthError::Invalid(_)),
                "{content}: {error:?}"
            );
            assert!(!error.to_string().contains("sk-secret"), "{error}");
            // A file that does not parse is never overwritten.
            assert!(store(home.path(), Provider::OpenAi, "sk-new").is_err());
            assert_eq!(fs::read_to_string(auth_path(home.path())).unwrap(), content);
        }
    }

    #[test]
    fn empty_keys_are_not_stored() {
        let home = tempdir().unwrap();
        assert!(store(home.path(), Provider::OpenAi, " ").is_err());
        assert!(!auth_path(home.path()).exists());
    }
}
