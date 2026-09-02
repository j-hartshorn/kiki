use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::prompt::{self, PromptError, PromptSource, ResolvedPrompt};

pub(crate) mod constants;

use self::constants::FIXED_MODEL;

const MONTY_BIN_ENV: &str = "MONTY_BIN";
const OPENAI_API_KEY_ENV: &str = "OPENAI_API_KEY";
const MAX_STARTUP_DIAGNOSTIC_BYTES: usize = 1_024;
const MAX_STARTUP_REMEDIATION_BYTES: usize = 512;
const MAX_STARTUP_PATH_BYTES: usize = 1_024;
const REDACTED: &str = "[redacted]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeyPresence {
    Missing,
    Present,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionAvailability {
    Available,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupProblemKind {
    WorkerPathMissing,
    WorkerPathNotAbsolute,
    WorkerConstructionFailed,
    WorkspaceRootUnavailable,
    WorkspaceRootNotAbsolute,
    WorkspaceConstructionFailed,
    PromptOverrideUnreadable,
    PromptOverrideInvalidUtf8,
    ApiKeyMissing,
    ApiKeyInvalidUnicode,
}

impl StartupProblemKind {
    fn order(self) -> u8 {
        match self {
            Self::WorkerPathMissing | Self::WorkerPathNotAbsolute => 0,
            Self::WorkerConstructionFailed => 1,
            Self::WorkspaceRootUnavailable | Self::WorkspaceRootNotAbsolute => 2,
            Self::WorkspaceConstructionFailed => 3,
            Self::PromptOverrideUnreadable | Self::PromptOverrideInvalidUtf8 => 4,
            Self::ApiKeyMissing | Self::ApiKeyInvalidUnicode => 5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupProblem {
    kind: StartupProblemKind,
    diagnostic: BoundedText,
    remediation: BoundedText,
}

impl StartupProblem {
    pub fn kind(&self) -> StartupProblemKind {
        self.kind
    }

    pub fn diagnostic(&self) -> &str {
        &self.diagnostic.0
    }

    pub fn remediation(&self) -> &str {
        &self.remediation.0
    }

    fn new(
        kind: StartupProblemKind,
        diagnostic: impl Into<String>,
        remediation: impl Into<String>,
        api_key: Option<&str>,
    ) -> Self {
        Self {
            kind,
            diagnostic: BoundedText::new(
                redact(diagnostic.into(), api_key),
                MAX_STARTUP_DIAGNOSTIC_BYTES,
            ),
            remediation: BoundedText::new(
                redact(remediation.into(), api_key),
                MAX_STARTUP_REMEDIATION_BYTES,
            ),
        }
    }
}

impl fmt::Display for StartupProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} Remediation: {}",
            self.diagnostic.0, self.remediation.0
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundedText(String);

impl BoundedText {
    fn new(mut text: String, max_bytes: usize) -> Self {
        if text.len() > max_bytes {
            let retained = utf8_prefix_len(&text, max_bytes.saturating_sub(3));
            text.truncate(retained);
            text.push_str("...");
        }
        Self(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedPath(BoundedText);

impl RedactedPath {
    pub fn as_str(&self) -> &str {
        &self.0.0
    }

    fn new(path: &Path, api_key: Option<&str>) -> Self {
        Self(BoundedText::new(
            redact(path.to_string_lossy().into_owned(), api_key),
            MAX_STARTUP_PATH_BYTES,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptProvenance {
    Bundled { name: &'static str },
    Override { path: RedactedPath },
}

impl PromptProvenance {
    fn from_source(source: &PromptSource, api_key: Option<&str>) -> Self {
        match source {
            PromptSource::Bundled { name } => Self::Bundled { name },
            PromptSource::Override { path } => Self::Override {
                path: RedactedPath::new(path, api_key),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSnapshot {
    worker_path: Option<RedactedPath>,
    workspace_root: Option<RedactedPath>,
    model: &'static str,
    prompt_source: PromptProvenance,
    api_key_presence: ApiKeyPresence,
    problems: Vec<StartupProblem>,
}

impl StartupSnapshot {
    pub fn worker_path(&self) -> Option<&RedactedPath> {
        self.worker_path.as_ref()
    }

    pub fn workspace_root(&self) -> Option<&RedactedPath> {
        self.workspace_root.as_ref()
    }

    pub fn model(&self) -> &'static str {
        self.model
    }

    pub fn prompt_source(&self) -> &PromptProvenance {
        &self.prompt_source
    }

    pub fn api_key_presence(&self) -> ApiKeyPresence {
        self.api_key_presence
    }

    pub fn problems(&self) -> &[StartupProblem] {
        &self.problems
    }

    pub fn submission_availability(&self) -> SubmissionAvailability {
        if self.problems.is_empty() {
            SubmissionAvailability::Available
        } else {
            SubmissionAvailability::Unavailable
        }
    }

    fn insert_problem(&mut self, problem: StartupProblem) {
        self.problems
            .retain(|existing| existing.kind != problem.kind);
        self.problems.push(problem);
        self.problems.sort_by_key(|problem| problem.kind.order());
    }
}

pub struct StartupConfiguration {
    snapshot: StartupSnapshot,
    construction: StartupConstruction,
}

impl StartupConfiguration {
    pub fn snapshot(&self) -> &StartupSnapshot {
        &self.snapshot
    }

    pub(crate) fn ready(&self) -> Option<ReadyStartup<'_>> {
        if !self.snapshot.problems.is_empty() {
            return None;
        }

        Some(ReadyStartup {
            worker_path: self.construction.worker_path.as_deref()?,
            workspace_root: self.construction.workspace_root.as_deref()?,
            system_prompt: self.construction.system_prompt.as_deref()?,
            api_key: self.construction.api_key.as_ref()?,
        })
    }

    pub fn with_worker_construction_failure(self, failure: impl fmt::Display) -> Self {
        self.with_construction_failure(
            StartupProblemKind::WorkerConstructionFailed,
            format!("Monty worker construction failed: {failure}"),
            "Check that MONTY_BIN points to a working monty-runtime 0.0.21 executable, then restart Kiki.",
        )
    }

    pub fn with_workspace_construction_failure(self, failure: impl fmt::Display) -> Self {
        self.with_construction_failure(
            StartupProblemKind::WorkspaceConstructionFailed,
            format!("Workspace construction failed: {failure}"),
            "Check the workspace directory permissions and filesystem access, then restart Kiki.",
        )
    }

    fn with_construction_failure(
        mut self,
        kind: StartupProblemKind,
        diagnostic: String,
        remediation: &'static str,
    ) -> Self {
        let api_key = self.construction.api_key.as_ref().map(ApiKey::expose);
        self.snapshot
            .insert_problem(StartupProblem::new(kind, diagnostic, remediation, api_key));
        self
    }
}

struct StartupConstruction {
    worker_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    system_prompt: Option<String>,
    api_key: Option<ApiKey>,
}

pub(crate) struct ReadyStartup<'a> {
    worker_path: &'a Path,
    workspace_root: &'a Path,
    system_prompt: &'a str,
    api_key: &'a ApiKey,
}

impl<'a> ReadyStartup<'a> {
    pub(crate) fn worker_path(&self) -> &'a Path {
        self.worker_path
    }

    pub(crate) fn workspace_root(&self) -> &'a Path {
        self.workspace_root
    }

    pub(crate) fn system_prompt(&self) -> &'a str {
        self.system_prompt
    }

    pub(crate) fn api_key(&self) -> &'a ApiKey {
        self.api_key
    }
}

pub(crate) struct ApiKey(String);

impl ApiKey {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

struct StartupInput {
    worker_path: Option<OsString>,
    workspace_root: io::Result<PathBuf>,
    prompt: PromptResolution,
    api_key: Option<OsString>,
}

enum PromptResolution {
    Resolved {
        text: String,
        source: PromptSource,
    },
    Failed {
        source: PromptSource,
        failure: PromptFailure,
    },
}

enum PromptFailure {
    Unreadable { detail: String },
    InvalidUtf8 { detail: String },
}

impl From<Result<ResolvedPrompt, PromptError>> for PromptResolution {
    fn from(resolution: Result<ResolvedPrompt, PromptError>) -> Self {
        match resolution {
            Ok(resolved) => Self::Resolved {
                text: resolved.text().to_owned(),
                source: resolved.source().clone(),
            },
            Err(PromptError::UnreadableOverride { path, source }) => Self::Failed {
                source: PromptSource::Override { path },
                failure: PromptFailure::Unreadable {
                    detail: source.to_string(),
                },
            },
            Err(PromptError::InvalidUtf8Override { path, source }) => Self::Failed {
                source: PromptSource::Override { path },
                failure: PromptFailure::InvalidUtf8 {
                    detail: source.to_string(),
                },
            },
        }
    }
}

pub fn load() -> StartupConfiguration {
    let input = StartupInput {
        worker_path: env::var_os(MONTY_BIN_ENV),
        workspace_root: env::current_dir(),
        prompt: PromptResolution::from(prompt::resolve()),
        api_key: env::var_os(OPENAI_API_KEY_ENV),
    };
    resolve_startup(input)
}

fn resolve_startup(input: StartupInput) -> StartupConfiguration {
    let (api_key, api_key_presence, api_key_problem) = resolve_api_key(input.api_key);
    let exposed_api_key = api_key.as_ref().map(ApiKey::expose);
    let (worker_path, worker_problem) = resolve_worker_path(input.worker_path, exposed_api_key);
    let (workspace_root, workspace_problem) =
        resolve_workspace_root(input.workspace_root, exposed_api_key);
    let (prompt_source, system_prompt, prompt_problem) =
        resolve_prompt(input.prompt, exposed_api_key);

    let worker_path_display = worker_path
        .as_deref()
        .map(|path| RedactedPath::new(path, exposed_api_key));
    let workspace_root_display = workspace_root
        .as_deref()
        .map(|path| RedactedPath::new(path, exposed_api_key));
    let prompt_source_display = PromptProvenance::from_source(&prompt_source, exposed_api_key);

    let mut problems = Vec::new();
    problems.extend(worker_problem);
    problems.extend(workspace_problem);
    problems.extend(prompt_problem);
    problems.extend(api_key_problem);
    problems.sort_by_key(|problem| problem.kind.order());

    StartupConfiguration {
        snapshot: StartupSnapshot {
            worker_path: worker_path_display,
            workspace_root: workspace_root_display,
            model: FIXED_MODEL,
            prompt_source: prompt_source_display,
            api_key_presence,
            problems,
        },
        construction: StartupConstruction {
            worker_path,
            workspace_root,
            system_prompt,
            api_key,
        },
    }
}

fn resolve_api_key(
    api_key: Option<OsString>,
) -> (Option<ApiKey>, ApiKeyPresence, Option<StartupProblem>) {
    let Some(api_key) = api_key else {
        return (
            None,
            ApiKeyPresence::Missing,
            Some(StartupProblem::new(
                StartupProblemKind::ApiKeyMissing,
                "OPENAI_API_KEY is missing or empty.",
                "Set OPENAI_API_KEY to a non-empty OpenAI API key, then restart Kiki.",
                None,
            )),
        );
    };
    let Ok(api_key) = api_key.into_string() else {
        return (
            None,
            ApiKeyPresence::Missing,
            Some(StartupProblem::new(
                StartupProblemKind::ApiKeyInvalidUnicode,
                "OPENAI_API_KEY is not valid Unicode.",
                "Set OPENAI_API_KEY to a non-empty Unicode OpenAI API key, then restart Kiki.",
                None,
            )),
        );
    };
    if api_key.trim().is_empty() {
        return (
            None,
            ApiKeyPresence::Missing,
            Some(StartupProblem::new(
                StartupProblemKind::ApiKeyMissing,
                "OPENAI_API_KEY is missing or empty.",
                "Set OPENAI_API_KEY to a non-empty OpenAI API key, then restart Kiki.",
                None,
            )),
        );
    }

    (Some(ApiKey(api_key)), ApiKeyPresence::Present, None)
}

fn resolve_worker_path(
    worker_path: Option<OsString>,
    api_key: Option<&str>,
) -> (Option<PathBuf>, Option<StartupProblem>) {
    let Some(worker_path) = worker_path else {
        return (
            None,
            Some(StartupProblem::new(
                StartupProblemKind::WorkerPathMissing,
                "MONTY_BIN is not set.",
                "Set MONTY_BIN to the absolute path of monty-runtime 0.0.21, then restart Kiki.",
                api_key,
            )),
        );
    };
    let worker_path = PathBuf::from(worker_path);
    let problem = (!worker_path.is_absolute()).then(|| {
        StartupProblem::new(
            StartupProblemKind::WorkerPathNotAbsolute,
            format!(
                "MONTY_BIN must be an absolute path, but is `{}`.",
                worker_path.display()
            ),
            "Set MONTY_BIN to the absolute path of monty-runtime 0.0.21, then restart Kiki.",
            api_key,
        )
    });
    (Some(worker_path), problem)
}

fn resolve_workspace_root(
    workspace_root: io::Result<PathBuf>,
    api_key: Option<&str>,
) -> (Option<PathBuf>, Option<StartupProblem>) {
    match workspace_root {
        Ok(workspace_root) => {
            let problem = (!workspace_root.is_absolute()).then(|| {
                StartupProblem::new(
                    StartupProblemKind::WorkspaceRootNotAbsolute,
                    format!(
                        "The current workspace root must be absolute, but is `{}`.",
                        workspace_root.display()
                    ),
                    "Start Kiki from an absolute workspace directory, then restart Kiki.",
                    api_key,
                )
            });
            (Some(workspace_root), problem)
        }
        Err(error) => (
            None,
            Some(StartupProblem::new(
                StartupProblemKind::WorkspaceRootUnavailable,
                format!("Could not determine the current workspace root: {error}"),
                "Start Kiki from a readable workspace directory, then restart Kiki.",
                api_key,
            )),
        ),
    }
}

fn resolve_prompt(
    prompt: PromptResolution,
    api_key: Option<&str>,
) -> (PromptSource, Option<String>, Option<StartupProblem>) {
    match prompt {
        PromptResolution::Resolved { text, source } => (source, Some(text), None),
        PromptResolution::Failed { source, failure } => {
            let (kind, diagnostic) = match &failure {
                PromptFailure::Unreadable { detail } => (
                    StartupProblemKind::PromptOverrideUnreadable,
                    format!(
                        "KIKI_SYSTEM_PROMPT override `{}` could not be read: {detail}",
                        prompt_source_path(&source).display()
                    ),
                ),
                PromptFailure::InvalidUtf8 { detail } => (
                    StartupProblemKind::PromptOverrideInvalidUtf8,
                    format!(
                        "KIKI_SYSTEM_PROMPT override `{}` is not valid UTF-8: {detail}",
                        prompt_source_path(&source).display()
                    ),
                ),
            };
            let problem = StartupProblem::new(
                kind,
                diagnostic,
                "Fix or unset KIKI_SYSTEM_PROMPT, then restart Kiki.",
                api_key,
            );
            (source, None, Some(problem))
        }
    }
}

fn prompt_source_path(source: &PromptSource) -> &Path {
    let PromptSource::Override { path } = source else {
        unreachable!("prompt failures only come from overrides")
    };
    path
}

#[cfg(test)]
pub(crate) fn ready_snapshot_for_test() -> StartupSnapshot {
    resolve_startup(StartupInput {
        worker_path: Some(OsString::from("/opt/kiki/bin/monty")),
        workspace_root: Ok(PathBuf::from("/workspace/project")),
        prompt: PromptResolution::Resolved {
            text: "system instructions".to_owned(),
            source: PromptSource::Bundled {
                name: "read_only_agent",
            },
        },
        api_key: Some(OsString::from("test-api-key")),
    })
    .snapshot
}

fn redact(mut text: String, api_key: Option<&str>) -> String {
    if let Some(api_key) = api_key.filter(|api_key| !api_key.is_empty()) {
        text = text.replace(api_key, REDACTED);
    }
    text
}

fn utf8_prefix_len(text: &str, max_bytes: usize) -> usize {
    let mut end = text.len().min(max_bytes);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::io;
    use std::path::PathBuf;

    use crate::prompt::{PromptError, PromptSource, ResolvedPrompt};

    use super::{
        ApiKeyPresence, MAX_STARTUP_DIAGNOSTIC_BYTES, MAX_STARTUP_PATH_BYTES,
        MAX_STARTUP_REMEDIATION_BYTES, PromptProvenance, PromptResolution, StartupInput,
        StartupProblemKind, SubmissionAvailability, resolve_startup,
    };

    const WORKER_PATH: &str = "/opt/kiki/bin/monty";
    const WORKSPACE_ROOT: &str = "/workspace/project";

    fn bundled_prompt() -> PromptResolution {
        PromptResolution::Resolved {
            text: "system instructions".to_owned(),
            source: PromptSource::Bundled {
                name: "read_only_agent",
            },
        }
    }

    fn ready_input(api_key: impl Into<OsString>) -> StartupInput {
        StartupInput {
            worker_path: Some(OsString::from(WORKER_PATH)),
            workspace_root: Ok(PathBuf::from(WORKSPACE_ROOT)),
            prompt: bundled_prompt(),
            api_key: Some(api_key.into()),
        }
    }

    fn problem_kinds(input: StartupInput) -> Vec<StartupProblemKind> {
        resolve_startup(input)
            .snapshot()
            .problems()
            .iter()
            .map(|problem| problem.kind())
            .collect()
    }

    #[test]
    fn ready_configuration_is_a_complete_secret_free_snapshot() {
        let configuration = resolve_startup(ready_input("sk-private-value"));
        let snapshot = configuration.snapshot();

        assert_eq!(snapshot.worker_path().unwrap().as_str(), WORKER_PATH);
        assert_eq!(snapshot.workspace_root().unwrap().as_str(), WORKSPACE_ROOT);
        assert_eq!(snapshot.model(), crate::config::constants::FIXED_MODEL);
        assert_eq!(
            snapshot.prompt_source(),
            &PromptProvenance::Bundled {
                name: "read_only_agent"
            }
        );
        assert_eq!(snapshot.api_key_presence(), ApiKeyPresence::Present);
        assert!(snapshot.problems().is_empty());
        assert_eq!(
            snapshot.submission_availability(),
            SubmissionAvailability::Available
        );
        assert_eq!(
            configuration
                .construction
                .api_key
                .as_ref()
                .map(super::ApiKey::expose),
            Some("sk-private-value")
        );
        assert_eq!(
            configuration.construction.system_prompt.as_deref(),
            Some("system instructions")
        );
    }

    #[test]
    fn hostile_api_key_is_redacted_from_every_snapshot_path_and_provenance() {
        let api_key = "sk-live-PATH-SECRET";
        let worker_path = PathBuf::from(format!("/opt/{api_key}/bin/monty"));
        let workspace_root = PathBuf::from(format!("/workspace/{api_key}/project"));
        let prompt_path = PathBuf::from(format!("/prompts/{api_key}/agent.md"));
        let input = StartupInput {
            worker_path: Some(worker_path.clone().into_os_string()),
            workspace_root: Ok(workspace_root.clone()),
            prompt: PromptResolution::Resolved {
                text: "system instructions".to_owned(),
                source: PromptSource::Override {
                    path: prompt_path.clone(),
                },
            },
            api_key: Some(OsString::from(api_key)),
        };

        let configuration = resolve_startup(input);
        let snapshot = configuration.snapshot();
        let inspectable = format!(
            "{snapshot:?}\n{:?}\n{:?}\n{:?}",
            snapshot.worker_path(),
            snapshot.workspace_root(),
            snapshot.prompt_source()
        );

        assert!(snapshot.problems().is_empty());
        assert_eq!(
            snapshot.worker_path().unwrap().as_str(),
            "/opt/[redacted]/bin/monty"
        );
        assert_eq!(
            snapshot.workspace_root().unwrap().as_str(),
            "/workspace/[redacted]/project"
        );
        let PromptProvenance::Override { path } = snapshot.prompt_source() else {
            panic!("override provenance should be retained")
        };
        assert_eq!(path.as_str(), "/prompts/[redacted]/agent.md");
        assert!(!inspectable.contains(api_key), "{inspectable}");

        let ready = configuration
            .ready()
            .expect("configuration should be ready");
        assert_eq!(ready.worker_path(), worker_path);
        assert_eq!(ready.workspace_root(), workspace_root);
        assert_eq!(ready.system_prompt(), "system instructions");
        assert_eq!(ready.api_key().expose(), api_key);
    }

    #[test]
    fn raw_startup_values_are_unavailable_when_any_problem_exists() {
        let configuration = resolve_startup(ready_input("key"))
            .with_worker_construction_failure("worker open failed");

        assert!(configuration.ready().is_none());
    }

    #[test]
    fn missing_worker_path_is_typed_and_disables_submission() {
        let mut input = ready_input("key");
        input.worker_path = None;

        let configuration = resolve_startup(input);

        assert_eq!(
            problem_kinds(StartupInput {
                worker_path: None,
                ..ready_input("key")
            }),
            [StartupProblemKind::WorkerPathMissing]
        );
        assert_eq!(configuration.snapshot().worker_path(), None);
        assert_eq!(
            configuration.snapshot().submission_availability(),
            SubmissionAvailability::Unavailable
        );
    }

    #[test]
    fn relative_worker_path_is_rejected_without_copying_arena_validation() {
        let mut input = ready_input("key");
        input.worker_path = Some(OsString::from("bin/monty"));

        assert_eq!(
            problem_kinds(input),
            [StartupProblemKind::WorkerPathNotAbsolute]
        );
    }

    #[test]
    fn unavailable_and_relative_workspace_roots_are_typed() {
        let mut unavailable = ready_input("key");
        unavailable.workspace_root = Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "working directory is unavailable",
        ));
        assert_eq!(
            problem_kinds(unavailable),
            [StartupProblemKind::WorkspaceRootUnavailable]
        );

        let mut relative = ready_input("key");
        relative.workspace_root = Ok(PathBuf::from("relative/workspace"));
        assert_eq!(
            problem_kinds(relative),
            [StartupProblemKind::WorkspaceRootNotAbsolute]
        );
    }

    #[test]
    fn unreadable_prompt_override_is_a_problem_without_bundled_fallback() {
        let path = PathBuf::from("/private/missing-prompt.md");
        let error = PromptError::UnreadableOverride {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::NotFound, "missing override"),
        };
        let mut input = ready_input("key");
        input.prompt = PromptResolution::from(Result::<ResolvedPrompt, PromptError>::Err(error));

        let configuration = resolve_startup(input);

        let PromptProvenance::Override {
            path: displayed_path,
        } = configuration.snapshot().prompt_source()
        else {
            panic!("override provenance should be retained")
        };
        assert_eq!(displayed_path.as_str(), path.to_string_lossy().as_ref());
        assert_eq!(
            configuration
                .snapshot()
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::PromptOverrideUnreadable]
        );
        assert_eq!(configuration.construction.system_prompt, None);
    }

    #[test]
    fn invalid_utf8_prompt_override_is_a_problem_without_bundled_fallback() {
        let path = PathBuf::from("/private/invalid-prompt.md");
        let invalid_prompt = String::from_utf8(vec![0xff]).unwrap_err();
        let error = PromptError::InvalidUtf8Override {
            path: path.clone(),
            source: invalid_prompt.utf8_error(),
        };
        let mut input = ready_input("key");
        input.prompt = PromptResolution::from(Result::<ResolvedPrompt, PromptError>::Err(error));

        let configuration = resolve_startup(input);

        let PromptProvenance::Override {
            path: displayed_path,
        } = configuration.snapshot().prompt_source()
        else {
            panic!("override provenance should be retained")
        };
        assert_eq!(displayed_path.as_str(), path.to_string_lossy().as_ref());
        assert_eq!(
            configuration
                .snapshot()
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::PromptOverrideInvalidUtf8]
        );
        assert_eq!(configuration.construction.system_prompt, None);
    }

    #[test]
    fn absent_empty_and_whitespace_only_api_keys_are_missing() {
        for api_key in [None, Some(""), Some(" \t\n"), Some("\u{2003}")] {
            let mut input = ready_input("placeholder");
            input.api_key = api_key.map(OsString::from);

            let configuration = resolve_startup(input);

            assert_eq!(
                configuration.snapshot().api_key_presence(),
                ApiKeyPresence::Missing
            );
            assert_eq!(
                configuration
                    .snapshot()
                    .problems()
                    .iter()
                    .map(|problem| problem.kind())
                    .collect::<Vec<_>>(),
                [StartupProblemKind::ApiKeyMissing]
            );
            assert!(configuration.construction.api_key.is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_api_key_is_invalid_without_retaining_its_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let mut input = ready_input("placeholder");
        input.api_key = Some(OsString::from_vec(vec![b's', b'k', b'-', 0xff]));

        let configuration = resolve_startup(input);

        assert_eq!(
            configuration.snapshot().api_key_presence(),
            ApiKeyPresence::Missing
        );
        assert_eq!(
            configuration
                .snapshot()
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::ApiKeyInvalidUnicode]
        );
        assert!(configuration.construction.api_key.is_none());
    }

    #[test]
    fn problems_have_deterministic_field_and_construction_order() {
        let input = StartupInput {
            worker_path: Some(OsString::from("relative-worker")),
            workspace_root: Err(io::Error::other("workspace unavailable")),
            prompt: PromptResolution::Failed {
                source: PromptSource::Override {
                    path: PathBuf::from("override.md"),
                },
                failure: super::PromptFailure::InvalidUtf8 {
                    detail: "invalid UTF-8".to_owned(),
                },
            },
            api_key: None,
        };

        let configuration = resolve_startup(input)
            .with_workspace_construction_failure("workspace open failed")
            .with_worker_construction_failure("worker open failed");

        assert_eq!(
            configuration
                .snapshot()
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [
                StartupProblemKind::WorkerPathNotAbsolute,
                StartupProblemKind::WorkerConstructionFailed,
                StartupProblemKind::WorkspaceRootUnavailable,
                StartupProblemKind::WorkspaceConstructionFailed,
                StartupProblemKind::PromptOverrideInvalidUtf8,
                StartupProblemKind::ApiKeyMissing,
            ]
        );
    }

    #[test]
    fn dynamic_diagnostics_and_remediation_are_utf8_safely_bounded() {
        let long_text = "é".repeat(MAX_STARTUP_DIAGNOSTIC_BYTES * 2);
        let mut input = ready_input("key");
        input.workspace_root = Err(io::Error::other(long_text.clone()));

        let configuration = resolve_startup(input)
            .with_worker_construction_failure(&long_text)
            .with_workspace_construction_failure(&long_text);

        for problem in configuration.snapshot().problems() {
            assert!(problem.diagnostic().len() <= MAX_STARTUP_DIAGNOSTIC_BYTES);
            assert!(problem.remediation().len() <= MAX_STARTUP_REMEDIATION_BYTES);
            assert!(std::str::from_utf8(problem.diagnostic().as_bytes()).is_ok());
            assert!(std::str::from_utf8(problem.remediation().as_bytes()).is_ok());
        }
    }

    #[test]
    fn presentation_paths_are_utf8_safely_bounded() {
        let long_segment = "é".repeat(MAX_STARTUP_PATH_BYTES);
        let mut input = ready_input("key");
        input.worker_path = Some(PathBuf::from(format!("/{long_segment}/monty")).into_os_string());
        input.workspace_root = Ok(PathBuf::from(format!("/{long_segment}/workspace")));
        input.prompt = PromptResolution::Resolved {
            text: "system instructions".to_owned(),
            source: PromptSource::Override {
                path: PathBuf::from(format!("/{long_segment}/prompt.md")),
            },
        };

        let configuration = resolve_startup(input);
        let snapshot = configuration.snapshot();
        let PromptProvenance::Override { path: prompt_path } = snapshot.prompt_source() else {
            panic!("override provenance should be retained")
        };

        for path in [
            snapshot.worker_path().unwrap(),
            snapshot.workspace_root().unwrap(),
            prompt_path,
        ] {
            assert!(path.as_str().len() <= MAX_STARTUP_PATH_BYTES);
            assert!(std::str::from_utf8(path.as_str().as_bytes()).is_ok());
        }
    }

    #[test]
    fn adversarial_api_key_never_appears_in_snapshot_debug_or_problem_display() {
        let api_key = "sk-live-THIS_MUST_NEVER_APPEAR";
        let mut input = ready_input(api_key);
        input.workspace_root = Err(io::Error::other(format!(
            "hostile resolver repeated {api_key}"
        )));

        let configuration = resolve_startup(input)
            .with_worker_construction_failure(format!("hostile worker repeated {api_key}"));
        assert_eq!(
            configuration
                .construction
                .api_key
                .as_ref()
                .map(super::ApiKey::expose),
            Some(api_key)
        );

        let mut inspectable = format!("{:?}", configuration.snapshot());
        for problem in configuration.snapshot().problems() {
            inspectable.push_str(&format!("\n{problem:?}\n{problem}"));
        }

        assert!(!inspectable.contains(api_key), "{inspectable}");
        assert!(inspectable.contains("[redacted]"), "{inspectable}");
    }

    #[test]
    fn configuration_invalid_snapshot_reaches_the_tui_renderer_without_a_key() {
        let api_key = "sk-live-CONFIGURATION-SECRET";
        let configuration = resolve_startup(StartupInput {
            worker_path: Some(OsString::from(format!("/worker/{api_key}/monty\t\x1b"))),
            workspace_root: Ok(PathBuf::from(format!("/workspace/{api_key}/project\r"))),
            prompt: PromptResolution::Resolved {
                text: "system instructions".to_owned(),
                source: PromptSource::Override {
                    path: PathBuf::from(format!("/prompts/{api_key}/agent\t\x1b.md")),
                },
            },
            api_key: Some(OsString::from(api_key)),
        })
        .with_worker_construction_failure(format!("worker failure \x1b {api_key}"))
        .with_workspace_construction_failure(format!("workspace failure \r {api_key}"));

        let rendered = crate::tui::format_startup_configuration(configuration.snapshot());

        assert!(rendered.contains("Worker: /worker/[redacted]/monty    \\x1B"));
        assert!(rendered.contains("Workspace: /workspace/[redacted]/project\\x0D"));
        assert!(rendered.contains("Model: gpt-5.6-sol"));
        assert!(rendered.contains("Prompt: override /prompts/[redacted]/agent    \\x1B.md"));
        assert!(rendered.contains("OpenAI API key: present"));
        assert!(rendered.contains("Submission: unavailable"));
        assert!(rendered.contains(
            "Problem: Monty worker construction failed: worker failure \\x1B [redacted]"
        ));
        assert!(rendered.contains(
            "Problem: Workspace construction failed: workspace failure \\x0D [redacted]"
        ));
        assert!(rendered.contains("Remediation: Check that MONTY_BIN points to a working monty-runtime 0.0.21 executable, then restart Kiki."));
        assert!(rendered.contains("Remediation: Check the workspace directory permissions and filesystem access, then restart Kiki."));
        assert!(rendered.contains("store: false disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI."));
        assert!(!rendered.contains(api_key), "{rendered}");
        assert!(!rendered.contains('\x1b'), "{rendered}");

        let bundled = resolve_startup(ready_input("key"));
        let bundled_rendered = crate::tui::format_startup_configuration(bundled.snapshot());
        assert!(bundled_rendered.contains("Prompt: bundled read_only_agent"));
        assert!(bundled_rendered.contains("Submission: available"));
    }
}
