use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::prompt::{self, PromptError, PromptSource, ResolvedPrompt};
use models::Provider;

pub(crate) mod auth;
pub(crate) mod constants;
pub mod models;
pub(crate) mod settings;
pub(crate) mod skill_trust;

pub use models::{
    DEFAULT_MODEL, ModelConfiguration, ModelPricing, ModelSelection, PricingIdentity, Rates,
    model_by_id,
};
pub(crate) use settings::{CompactionSettings, SessionPreferences};

const MONTY_BIN_ENV: &str = "MONTY_BIN";
const KIKI_MODEL_ENV: &str = "KIKI_MODEL";
const KIKI_THINKING_ENV: &str = "KIKI_THINKING";
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
    UserSettingsUnreadable,
    UserSettingsInvalidUtf8,
    UserSettingsMalformed,
    UserSettingsInvalid,
    ProjectSettingsUnreadable,
    ProjectSettingsInvalidUtf8,
    ProjectSettingsMalformed,
    ProjectSettingsInvalid,
    ModelUnknown,
    ModelInvalidUnicode,
    ThinkingInvalidUnicode,
    ThinkingEmpty,
    ThinkingUnknown,
    ThinkingUnsupported,
    PromptOverrideUnreadable,
    PromptOverrideInvalidUtf8,
    SkillsCatalogPromptUnreadable,
    SkillsCatalogPromptInvalidUtf8,
    ApiKeyMissing,
    ApiKeyInvalidUnicode,
    AuthFileUnreadable,
    AuthFileInvalid,
    AuthFileInsecure,
}

impl StartupProblemKind {
    fn order(self) -> u8 {
        match self {
            Self::WorkerPathMissing | Self::WorkerPathNotAbsolute => 0,
            Self::WorkerConstructionFailed => 1,
            Self::WorkspaceRootUnavailable | Self::WorkspaceRootNotAbsolute => 2,
            Self::WorkspaceConstructionFailed => 3,
            Self::UserSettingsUnreadable
            | Self::UserSettingsInvalidUtf8
            | Self::UserSettingsMalformed
            | Self::UserSettingsInvalid => 4,
            Self::ProjectSettingsUnreadable
            | Self::ProjectSettingsInvalidUtf8
            | Self::ProjectSettingsMalformed
            | Self::ProjectSettingsInvalid => 5,
            Self::ModelUnknown | Self::ModelInvalidUnicode => 6,
            Self::ThinkingInvalidUnicode
            | Self::ThinkingEmpty
            | Self::ThinkingUnknown
            | Self::ThinkingUnsupported => 7,
            Self::PromptOverrideUnreadable
            | Self::PromptOverrideInvalidUtf8
            | Self::SkillsCatalogPromptUnreadable
            | Self::SkillsCatalogPromptInvalidUtf8 => 8,
            Self::ApiKeyMissing
            | Self::ApiKeyInvalidUnicode
            | Self::AuthFileUnreadable
            | Self::AuthFileInvalid
            | Self::AuthFileInsecure => 9,
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
        secrets: &[&str],
    ) -> Self {
        Self {
            kind,
            diagnostic: BoundedText::new(
                redact(diagnostic.into(), secrets),
                MAX_STARTUP_DIAGNOSTIC_BYTES,
            ),
            remediation: BoundedText::new(
                redact(remediation.into(), secrets),
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

    fn new(path: &Path, secrets: &[&str]) -> Self {
        Self(BoundedText::new(
            redact(path.to_string_lossy().into_owned(), secrets),
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
    fn from_source(source: &PromptSource, secrets: &[&str]) -> Self {
        match source {
            PromptSource::Bundled { name } => Self::Bundled { name },
            PromptSource::Override { path } => Self::Override {
                path: RedactedPath::new(path, secrets),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSnapshot {
    worker_path: Option<RedactedPath>,
    workspace_root: Option<RedactedPath>,
    model: &'static ModelConfiguration,
    thinking: &'static str,
    subagents: settings::SubagentSettings,
    settings: settings::SettingsContext,
    prompt_source: PromptProvenance,
    openai_api_key_presence: ApiKeyPresence,
    anthropic_api_key_presence: ApiKeyPresence,
    problems: Vec<StartupProblem>,
    /// Non-blocking issues, such as an unusable auth file while the selected
    /// provider's key comes from the environment.
    warnings: Vec<StartupProblem>,
}

impl StartupSnapshot {
    pub fn worker_path(&self) -> Option<&RedactedPath> {
        self.worker_path.as_ref()
    }

    pub fn workspace_root(&self) -> Option<&RedactedPath> {
        self.workspace_root.as_ref()
    }

    pub fn model(&self) -> &'static ModelConfiguration {
        self.model
    }

    pub fn thinking(&self) -> &'static str {
        self.thinking
    }

    pub(crate) fn subagents(&self) -> settings::SubagentSettings {
        self.subagents
    }

    pub(crate) fn settings(&self) -> &settings::SettingsContext {
        &self.settings
    }

    pub fn prompt_source(&self) -> &PromptProvenance {
        &self.prompt_source
    }

    pub fn api_key_presence(&self, provider: Provider) -> ApiKeyPresence {
        match provider {
            Provider::OpenAi => self.openai_api_key_presence,
            Provider::Anthropic => self.anthropic_api_key_presence,
        }
    }

    pub fn problems(&self) -> &[StartupProblem] {
        &self.problems
    }

    pub fn warnings(&self) -> &[StartupProblem] {
        &self.warnings
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
            prompt_base: self.construction.prompt_base.as_deref()?,
            skills_catalog_guidance: self.construction.skills_catalog_guidance.as_deref()?,
            api_keys: &self.construction.api_keys,
            model: self.construction.model,
            thinking: self.snapshot.thinking,
        })
    }

    pub fn with_worker_construction_failure(self, failure: impl fmt::Display) -> Self {
        self.with_construction_failure(
            StartupProblemKind::WorkerConstructionFailed,
            format!("Monty worker construction failed: {failure}"),
            "Check that the Monty worker (this kiki executable, or MONTY_BIN if set) can start, then restart Kiki.",
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
        let secrets = self.construction.api_keys.secrets();
        self.snapshot
            .insert_problem(StartupProblem::new(kind, diagnostic, remediation, &secrets));
        self
    }
}

struct StartupConstruction {
    worker_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    prompt_base: Option<String>,
    skills_catalog_guidance: Option<String>,
    api_keys: ApiKeys,
    model: &'static ModelConfiguration,
}

pub(crate) struct ReadyStartup<'a> {
    worker_path: &'a Path,
    workspace_root: &'a Path,
    prompt_base: &'a str,
    skills_catalog_guidance: &'a str,
    api_keys: &'a ApiKeys,
    model: &'static ModelConfiguration,
    thinking: &'static str,
}

impl<'a> ReadyStartup<'a> {
    pub(crate) fn worker_path(&self) -> &'a Path {
        self.worker_path
    }

    pub(crate) fn workspace_root(&self) -> &'a Path {
        self.workspace_root
    }

    pub(crate) fn prompt_base(&self) -> &'a str {
        self.prompt_base
    }

    pub(crate) fn skills_catalog_guidance(&self) -> &'a str {
        self.skills_catalog_guidance
    }

    pub(crate) fn api_key(&self, provider: Provider) -> Option<&'a ApiKey> {
        self.api_keys.get(provider)
    }

    pub(crate) fn model(&self) -> &'static ModelConfiguration {
        self.model
    }

    pub(crate) fn thinking(&self) -> &'static str {
        self.thinking
    }
}

pub(crate) struct ApiKey(String);

impl ApiKey {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

struct ApiKeys {
    openai: Option<ApiKey>,
    anthropic: Option<ApiKey>,
}

impl ApiKeys {
    fn get(&self, provider: Provider) -> Option<&ApiKey> {
        match provider {
            Provider::OpenAi => self.openai.as_ref(),
            Provider::Anthropic => self.anthropic.as_ref(),
        }
    }

    fn presence(&self, provider: Provider) -> ApiKeyPresence {
        match self.get(provider) {
            Some(_) => ApiKeyPresence::Present,
            None => ApiKeyPresence::Missing,
        }
    }

    fn secrets(&self) -> Vec<&str> {
        [&self.openai, &self.anthropic]
            .into_iter()
            .flatten()
            .map(ApiKey::expose)
            .collect()
    }
}

struct StartupInput {
    worker_path: Option<OsString>,
    model: Option<OsString>,
    thinking: Option<OsString>,
    home: Option<OsString>,
    workspace_root: io::Result<PathBuf>,
    prompt: PromptResolution,
    skills_catalog_guidance: Result<String, PromptError>,
    openai_api_key: Option<OsString>,
    anthropic_api_key: Option<OsString>,
    /// Keys from `~/.kiki/auth.toml`, used when the environment has none.
    stored_api_keys: Result<auth::StoredKeys, auth::AuthError>,
}

enum PromptResolution {
    Resolved {
        base: String,
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
                base: resolved.base().to_owned(),
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

/// Creates `path` and any missing parents owner-only (`0700` on Unix). Used for
/// `~/.kiki` and its subdirectories, which hold sessions, settings and keys.
/// Existing directories keep their mode.
pub(crate) fn create_private_dir_all(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// The embedded Monty worker is this executable (`kiki subprocess`, see
/// `crate::worker`); `MONTY_BIN` overrides it.
fn embedded_worker_path() -> Option<OsString> {
    env::current_exe().ok().map(PathBuf::into_os_string)
}

pub fn load() -> StartupConfiguration {
    load_with_session_preferences(None)
}

pub(crate) fn load_with_session_preferences(
    session_preferences: Option<SessionPreferences>,
) -> StartupConfiguration {
    let home = env::var_os("HOME");
    let stored_api_keys = match &home {
        Some(home) => auth::load(Path::new(home)),
        None => Ok(auth::StoredKeys::default()),
    };
    let input = StartupInput {
        worker_path: env::var_os(MONTY_BIN_ENV).or_else(embedded_worker_path),
        model: env::var_os(KIKI_MODEL_ENV),
        thinking: env::var_os(KIKI_THINKING_ENV),
        home,
        workspace_root: env::current_dir(),
        prompt: PromptResolution::from(prompt::resolve()),
        skills_catalog_guidance: prompt::resolve_skills_catalog_guidance(),
        openai_api_key: env::var_os(Provider::OpenAi.api_key_variable()),
        anthropic_api_key: env::var_os(Provider::Anthropic.api_key_variable()),
        stored_api_keys,
    };
    resolve_startup_with_session_preferences(input, session_preferences)
}

#[cfg(test)]
fn resolve_startup(input: StartupInput) -> StartupConfiguration {
    resolve_startup_with_session_preferences(input, None)
}

fn resolve_startup_with_session_preferences(
    input: StartupInput,
    session_preferences: Option<SessionPreferences>,
) -> StartupConfiguration {
    let StartupInput {
        worker_path,
        model,
        thinking,
        home,
        workspace_root,
        prompt,
        skills_catalog_guidance,
        openai_api_key,
        anthropic_api_key,
        stored_api_keys,
    } = input;
    let resuming = session_preferences.is_some();
    let stored = stored_api_keys.as_ref().ok();
    let openai_api_key = resolve_api_key(
        openai_api_key,
        stored.and_then(|keys| keys.get(Provider::OpenAi)),
    );
    let anthropic_api_key = resolve_api_key(
        anthropic_api_key,
        stored.and_then(|keys| keys.get(Provider::Anthropic)),
    );
    let settings = settings::resolve(settings::Input {
        home,
        workspace_root: workspace_root.as_ref().ok().cloned(),
        session_preferences,
        model,
        thinking,
    });
    let model = settings.model();
    let selected_api_key = match model.provider() {
        Provider::OpenAi => &openai_api_key,
        Provider::Anthropic => &anthropic_api_key,
    };
    let auth_file_issue = stored_api_keys.as_ref().err().map(auth_file_problem);
    let api_key_problem = selected_api_key
        .as_ref()
        .err()
        .map(|&kind| api_key_problem(kind, model.provider(), resuming));
    // An unusable auth file may be why the selected key is missing, so report it
    // next to the missing-key problem (which still names the environment
    // variable and resume alternatives). Otherwise it does not block startup,
    // but its keys are unavailable, so it is a warning.
    let key_missing = api_key_problem
        .as_ref()
        .is_some_and(|problem| problem.kind == StartupProblemKind::ApiKeyMissing);
    let (auth_file_problem, warnings) = if key_missing {
        (auth_file_issue, Vec::new())
    } else {
        (None, auth_file_issue.into_iter().collect())
    };
    let api_keys = ApiKeys {
        openai: openai_api_key.ok(),
        anthropic: anthropic_api_key.ok(),
    };
    let secrets = api_keys.secrets();
    let thinking = settings.thinking();
    let subagents = settings.subagents();
    let settings_context = settings.context().clone();
    let settings_problems = settings
        .problems()
        .iter()
        .map(|problem| startup_settings_problem(problem, &secrets));
    let (worker_path, worker_problem) = resolve_worker_path(worker_path, &secrets);
    let (workspace_root, workspace_problem) = resolve_workspace_root(workspace_root, &secrets);
    let (prompt_source, prompt_base, prompt_problem) = resolve_prompt(prompt, &secrets);
    let (skills_catalog_guidance, skills_catalog_problem) =
        resolve_skills_catalog_guidance(skills_catalog_guidance, &secrets);

    let worker_path_display = worker_path
        .as_deref()
        .map(|path| RedactedPath::new(path, &secrets));
    let workspace_root_display = workspace_root
        .as_deref()
        .map(|path| RedactedPath::new(path, &secrets));
    let prompt_source_display = PromptProvenance::from_source(&prompt_source, &secrets);

    let mut problems = Vec::new();
    problems.extend(worker_problem);
    problems.extend(workspace_problem);
    problems.extend(settings_problems);
    problems.extend(prompt_problem);
    problems.extend(skills_catalog_problem);
    problems.extend(auth_file_problem);
    problems.extend(api_key_problem);
    problems.sort_by_key(|problem| problem.kind.order());

    StartupConfiguration {
        snapshot: StartupSnapshot {
            worker_path: worker_path_display,
            workspace_root: workspace_root_display,
            model,
            thinking,
            subagents,
            settings: settings_context,
            prompt_source: prompt_source_display,
            openai_api_key_presence: api_keys.presence(Provider::OpenAi),
            anthropic_api_key_presence: api_keys.presence(Provider::Anthropic),
            problems,
            warnings,
        },
        construction: StartupConstruction {
            worker_path,
            workspace_root,
            prompt_base,
            skills_catalog_guidance,
            api_keys,
            model,
        },
    }
}

fn startup_settings_problem(problem: &settings::Problem, secrets: &[&str]) -> StartupProblem {
    StartupProblem::new(
        problem.kind(),
        problem.diagnostic(),
        problem.remediation(),
        secrets,
    )
}

/// The environment variable wins; a stored key fills in only when it is unset or empty.
fn resolve_api_key(
    environment: Option<OsString>,
    stored: Option<&str>,
) -> Result<ApiKey, StartupProblemKind> {
    match resolve_environment_api_key(environment) {
        Err(StartupProblemKind::ApiKeyMissing) => stored
            .map(|api_key| ApiKey(api_key.to_owned()))
            .ok_or(StartupProblemKind::ApiKeyMissing),
        resolved => resolved,
    }
}

fn auth_file_problem(error: &auth::AuthError) -> StartupProblem {
    let kind = match error {
        auth::AuthError::InsecurePermissions { .. }
        | auth::AuthError::InsecureDirectory { .. }
        | auth::AuthError::NotOwned { .. } => StartupProblemKind::AuthFileInsecure,
        // `load` never writes, so `Unwritable` cannot occur here.
        auth::AuthError::Unreadable(_)
        | auth::AuthError::InvalidUtf8
        | auth::AuthError::WrongType { .. }
        | auth::AuthError::Unwritable(_) => StartupProblemKind::AuthFileUnreadable,
        auth::AuthError::Malformed | auth::AuthError::Invalid(_) => {
            StartupProblemKind::AuthFileInvalid
        }
    };
    StartupProblem::new(kind, format!("{error}."), error.remediation(), &[])
}

/// Whether `provider`'s environment variable is used instead of a stored key.
/// Shares the startup rule: an unset, empty or whitespace-only variable falls
/// back to the stored key; any other value (even invalid) takes precedence.
pub(crate) fn environment_api_key_overrides_stored(provider: Provider) -> bool {
    environment_value_overrides_stored(env::var_os(provider.api_key_variable()))
}

fn environment_value_overrides_stored(value: Option<OsString>) -> bool {
    !matches!(
        resolve_environment_api_key(value),
        Err(StartupProblemKind::ApiKeyMissing)
    )
}

fn resolve_environment_api_key(api_key: Option<OsString>) -> Result<ApiKey, StartupProblemKind> {
    let api_key = api_key
        .ok_or(StartupProblemKind::ApiKeyMissing)?
        .into_string()
        .map_err(|_| StartupProblemKind::ApiKeyInvalidUnicode)?;
    if api_key.trim().is_empty() {
        return Err(StartupProblemKind::ApiKeyMissing);
    }
    Ok(ApiKey(api_key))
}

fn api_key_problem(kind: StartupProblemKind, provider: Provider, resuming: bool) -> StartupProblem {
    let variable = provider.api_key_variable();
    let (diagnostic, key, login) = if kind == StartupProblemKind::ApiKeyInvalidUnicode {
        ("is not valid Unicode", "a non-empty Unicode", String::new())
    } else {
        (
            "is missing or empty",
            "a non-empty",
            format!(" or run `kiki login {}`", auth::provider_id(provider)),
        )
    };
    let saved_model = if resuming {
        ", or set KIKI_MODEL to override the saved model"
    } else {
        ""
    };
    StartupProblem::new(
        kind,
        format!("{variable} {diagnostic}."),
        format!(
            "Set {variable} to {key} {} API key{login}{saved_model}, then restart Kiki.",
            provider.display_name()
        ),
        &[],
    )
}

fn resolve_worker_path(
    worker_path: Option<OsString>,
    secrets: &[&str],
) -> (Option<PathBuf>, Option<StartupProblem>) {
    let Some(worker_path) = worker_path else {
        return (
            None,
            Some(StartupProblem::new(
                StartupProblemKind::WorkerPathMissing,
                "Kiki could not locate its own executable to run the embedded Monty worker.",
                "Set MONTY_BIN to the absolute path of a kiki or monty-runtime 0.0.22 executable, then restart Kiki.",
                secrets,
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
            "Set MONTY_BIN to the absolute path of a kiki or monty-runtime 0.0.22 executable, or unset it, then restart Kiki.",
            secrets,
        )
    });
    (Some(worker_path), problem)
}

fn resolve_workspace_root(
    workspace_root: io::Result<PathBuf>,
    secrets: &[&str],
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
                    secrets,
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
                secrets,
            )),
        ),
    }
}

fn resolve_prompt(
    prompt: PromptResolution,
    secrets: &[&str],
) -> (PromptSource, Option<String>, Option<StartupProblem>) {
    match prompt {
        PromptResolution::Resolved { base, source } => (source, Some(base), None),
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
                secrets,
            );
            (source, None, Some(problem))
        }
    }
}

fn resolve_skills_catalog_guidance(
    resolution: Result<String, PromptError>,
    secrets: &[&str],
) -> (Option<String>, Option<StartupProblem>) {
    match resolution {
        Ok(guidance) => (Some(guidance), None),
        Err(PromptError::UnreadableOverride { path, source }) => (
            None,
            Some(StartupProblem::new(
                StartupProblemKind::SkillsCatalogPromptUnreadable,
                format!(
                    "KIKI_SKILLS_CATALOG_PROMPT override `{}` could not be read: {source}",
                    path.display()
                ),
                "Fix or unset KIKI_SKILLS_CATALOG_PROMPT, then restart Kiki.",
                secrets,
            )),
        ),
        Err(PromptError::InvalidUtf8Override { path, source }) => (
            None,
            Some(StartupProblem::new(
                StartupProblemKind::SkillsCatalogPromptInvalidUtf8,
                format!(
                    "KIKI_SKILLS_CATALOG_PROMPT override `{}` is not valid UTF-8: {source}",
                    path.display()
                ),
                "Fix or unset KIKI_SKILLS_CATALOG_PROMPT, then restart Kiki.",
                secrets,
            )),
        ),
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
    ready_snapshot_with_settings_for_test(None, Path::new("/workspace/project"))
}

#[cfg(test)]
pub(crate) fn ready_snapshot_with_settings_for_test(
    home: Option<&Path>,
    workspace_root: &Path,
) -> StartupSnapshot {
    resolve_startup(StartupInput {
        worker_path: Some(OsString::from("/opt/kiki/bin/monty")),
        model: None,
        thinking: None,
        home: home.map(|path| path.as_os_str().to_os_string()),
        workspace_root: Ok(workspace_root.to_path_buf()),
        prompt: PromptResolution::Resolved {
            base: "system instructions".to_owned(),
            source: PromptSource::Bundled { name: "system" },
        },
        skills_catalog_guidance: Ok(prompt::skills_catalog_guidance().to_owned()),
        openai_api_key: Some(OsString::from("test-api-key")),
        anthropic_api_key: None,
        stored_api_keys: Ok(crate::config::auth::StoredKeys::default()),
    })
    .snapshot
}

fn redact(text: String, secrets: &[&str]) -> String {
    let mut matches = Vec::new();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        let mut search_from = 0;
        while let Some(at) = text[search_from..].find(secret) {
            let start = search_from + at;
            matches.push(start..start + secret.len());
            search_from = start + text[start..].chars().next().unwrap().len_utf8();
        }
    }
    matches.sort_unstable_by_key(|range| range.start);

    let mut redacted = String::with_capacity(text.len());
    let mut consumed = 0;
    for range in matches {
        if range.end <= consumed {
            continue;
        }
        if range.start >= consumed {
            redacted.push_str(&text[consumed..range.start]);
            redacted.push_str(REDACTED);
        }
        consumed = range.end;
    }
    redacted.push_str(&text[consumed..]);
    redacted
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
    use crate::config::{
        DEFAULT_MODEL, ModelSelection, SessionPreferences,
        models::{ASTRA, LUNA, OPUS_5_5, Provider, SOL, TERRA},
    };
    use crate::prompt::{PromptError, PromptSource, ResolvedPrompt};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use tempfile::tempdir;

    use super::{
        ApiKeyPresence, MAX_STARTUP_DIAGNOSTIC_BYTES, MAX_STARTUP_PATH_BYTES,
        MAX_STARTUP_REMEDIATION_BYTES, PromptProvenance, PromptResolution, StartupInput,
        StartupProblemKind, SubmissionAvailability, resolve_startup,
        resolve_startup_with_session_preferences,
    };

    const WORKER_PATH: &str = "/opt/kiki/bin/monty";
    const WORKSPACE_ROOT: &str = "/workspace/project";

    fn bundled_prompt() -> PromptResolution {
        PromptResolution::Resolved {
            base: "system instructions".to_owned(),
            source: PromptSource::Bundled { name: "system" },
        }
    }

    fn ready_input(api_key: impl Into<OsString>) -> StartupInput {
        StartupInput {
            worker_path: Some(OsString::from(WORKER_PATH)),
            model: None,
            thinking: None,
            home: None,
            workspace_root: Ok(PathBuf::from(WORKSPACE_ROOT)),
            prompt: bundled_prompt(),
            skills_catalog_guidance: Ok(crate::prompt::skills_catalog_guidance().to_owned()),
            openai_api_key: Some(api_key.into()),
            anthropic_api_key: None,
            stored_api_keys: Ok(crate::config::auth::StoredKeys::default()),
        }
    }

    fn anthropic_only_input(api_key: &str) -> StartupInput {
        let mut input = ready_input("unused");
        input.openai_api_key = None;
        input.anthropic_api_key = Some(OsString::from(api_key));
        input
    }

    fn ready_input_with_settings(
        home: &std::path::Path,
        workspace_root: &std::path::Path,
    ) -> StartupInput {
        let mut input = ready_input("key");
        input.home = Some(home.as_os_str().to_os_string());
        input.workspace_root = Ok(workspace_root.to_path_buf());
        input
    }

    fn write_settings(root: &std::path::Path, content: &str) {
        let path = root.join(".kiki/config.toml");
        fs::create_dir_all(path.parent().expect("settings parent")).expect("settings directory");
        fs::write(path, content).expect("settings file");
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
        assert!(std::ptr::eq(snapshot.model(), DEFAULT_MODEL));
        assert_eq!(
            snapshot.prompt_source(),
            &PromptProvenance::Bundled { name: "system" }
        );
        assert_eq!(
            snapshot.api_key_presence(Provider::OpenAi),
            ApiKeyPresence::Present
        );
        assert!(snapshot.problems().is_empty());
        assert_eq!(
            snapshot.submission_availability(),
            SubmissionAvailability::Available
        );
        assert_eq!(
            configuration
                .construction
                .api_keys
                .get(Provider::OpenAi)
                .map(super::ApiKey::expose),
            Some("sk-private-value")
        );
        assert_eq!(
            configuration.construction.prompt_base.as_deref(),
            Some("system instructions")
        );
    }

    #[test]
    fn only_an_anthropic_key_with_an_anthropic_model_is_ready() {
        let mut input = anthropic_only_input("sk-ant-private");
        input.model = Some(OsString::from(OPUS_5_5.id));

        let configuration = resolve_startup(input);
        let snapshot = configuration.snapshot();
        let rendered = crate::tui::format_startup_configuration(snapshot);

        assert!(snapshot.problems().is_empty(), "{:?}", snapshot.problems());
        assert!(std::ptr::eq(snapshot.model(), &OPUS_5_5));
        assert_eq!(
            snapshot.api_key_presence(Provider::OpenAi),
            ApiKeyPresence::Missing
        );
        assert_eq!(
            snapshot.api_key_presence(Provider::Anthropic),
            ApiKeyPresence::Present
        );
        assert!(rendered.contains("OpenAI API key: missing"), "{rendered}");
        assert!(
            rendered.contains("Anthropic API key: present"),
            "{rendered}"
        );
        let ready = configuration
            .ready()
            .expect("Anthropic-only startup is ready");
        assert!(std::ptr::eq(ready.model(), &OPUS_5_5));
        assert!(ready.api_key(Provider::OpenAi).is_none());
        assert_eq!(
            ready
                .api_key(Provider::Anthropic)
                .map(super::ApiKey::expose),
            Some("sk-ant-private")
        );
    }

    #[test]
    fn only_an_openai_key_with_the_default_model_is_ready() {
        let configuration = resolve_startup(ready_input("sk-openai-private"));
        let snapshot = configuration.snapshot();

        assert!(snapshot.problems().is_empty());
        assert!(std::ptr::eq(snapshot.model(), DEFAULT_MODEL));
        assert_eq!(
            snapshot.api_key_presence(Provider::OpenAi),
            ApiKeyPresence::Present
        );
        assert_eq!(
            snapshot.api_key_presence(Provider::Anthropic),
            ApiKeyPresence::Missing
        );
        let ready = configuration.ready().expect("OpenAI-only startup is ready");
        assert_eq!(
            ready.api_key(Provider::OpenAi).map(super::ApiKey::expose),
            Some("sk-openai-private")
        );
        assert!(ready.api_key(Provider::Anthropic).is_none());
    }

    #[test]
    fn a_missing_selected_provider_key_is_a_problem_naming_its_variable() {
        let mut anthropic_selected = ready_input("sk-openai-private");
        anthropic_selected.model = Some(OsString::from(OPUS_5_5.id));
        let openai_selected = anthropic_only_input("sk-ant-private");

        for (input, variable, provider) in [
            (anthropic_selected, "ANTHROPIC_API_KEY", "Anthropic"),
            (openai_selected, "OPENAI_API_KEY", "OpenAI"),
        ] {
            let configuration = resolve_startup(input);
            let problems = configuration.snapshot().problems();

            assert_eq!(
                problems
                    .iter()
                    .map(|problem| problem.kind())
                    .collect::<Vec<_>>(),
                [StartupProblemKind::ApiKeyMissing]
            );
            assert_eq!(
                problems[0].diagnostic(),
                format!("{variable} is missing or empty.")
            );
            assert_eq!(
                problems[0].remediation(),
                format!(
                    "Set {variable} to a non-empty {provider} API key or run `kiki login {}`, then restart Kiki.",
                    provider.to_lowercase()
                )
            );
            assert!(configuration.ready().is_none());
        }
    }

    #[test]
    fn stored_key_fills_in_only_when_the_environment_has_none() {
        let home = tempfile::tempdir().unwrap();
        crate::config::auth::store(home.path(), Provider::OpenAi, "sk-stored").unwrap();
        let stored = crate::config::auth::load(home.path()).unwrap();

        let mut input = ready_input("");
        input.openai_api_key = None;
        input.stored_api_keys = Ok(stored.clone());
        let configuration = resolve_startup(input);
        assert!(configuration.snapshot().problems().is_empty());
        assert_eq!(
            configuration
                .ready()
                .unwrap()
                .api_key(Provider::OpenAi)
                .map(super::ApiKey::expose),
            Some("sk-stored")
        );

        let mut input = ready_input("sk-environment");
        input.stored_api_keys = Ok(stored);
        let configuration = resolve_startup(input);
        assert_eq!(
            configuration
                .ready()
                .unwrap()
                .api_key(Provider::OpenAi)
                .map(super::ApiKey::expose),
            Some("sk-environment")
        );
    }

    #[test]
    fn environment_overrides_stored_keys_by_the_startup_rule() {
        use super::environment_value_overrides_stored as overrides;

        assert!(!overrides(None));
        assert!(!overrides(Some(OsString::from(""))));
        assert!(!overrides(Some(OsString::from("  \t"))));
        assert!(overrides(Some(OsString::from("sk-environment"))));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert!(overrides(Some(OsString::from_vec(vec![0xff]))));
        }
    }

    #[test]
    fn unusable_auth_file_explains_a_missing_key_but_not_a_present_one() {
        let mut input = ready_input("");
        input.openai_api_key = None;
        input.stored_api_keys =
            Err(crate::config::auth::AuthError::InsecurePermissions { mode: 0o644 });
        let configuration = resolve_startup(input);
        let problems = configuration.snapshot().problems();
        assert_eq!(
            problems
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [
                StartupProblemKind::AuthFileInsecure,
                StartupProblemKind::ApiKeyMissing
            ]
        );
        assert!(
            problems[0]
                .remediation()
                .contains("chmod 600 ~/.kiki/auth.toml")
        );
        assert!(problems[1].remediation().contains("Set OPENAI_API_KEY"));
        assert!(problems[1].remediation().contains("kiki login openai"));

        let mut input = ready_input("sk-environment");
        input.stored_api_keys = Err(crate::config::auth::AuthError::Malformed);
        let configuration = resolve_startup(input);
        assert!(configuration.snapshot().problems().is_empty());
        assert!(configuration.ready().is_some());
        assert_eq!(
            configuration
                .snapshot()
                .warnings()
                .iter()
                .map(|warning| warning.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::AuthFileInvalid]
        );
    }

    #[test]
    fn resuming_a_saved_anthropic_model_without_its_key_mentions_kiki_model() {
        let saved = SessionPreferences::initial(
            ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap(),
        );

        let configuration =
            resolve_startup_with_session_preferences(ready_input("sk-openai-private"), Some(saved));
        let problems = configuration.snapshot().problems();

        assert!(std::ptr::eq(configuration.snapshot().model(), &OPUS_5_5));
        assert_eq!(
            problems
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::ApiKeyMissing]
        );
        assert_eq!(
            problems[0].diagnostic(),
            "ANTHROPIC_API_KEY is missing or empty."
        );
        assert_eq!(
            problems[0].remediation(),
            "Set ANTHROPIC_API_KEY to a non-empty Anthropic API key or run `kiki login anthropic`, or set KIKI_MODEL to override the saved model, then restart Kiki."
        );
        assert!(configuration.ready().is_none());
    }

    #[test]
    fn every_present_key_is_redacted_from_diagnostics_paths_and_provenance() {
        let openai_key = "sk-openai-BOTH-SECRET";
        let anthropic_key = "sk-ant-BOTH-SECRET";
        let mut input = ready_input(openai_key);
        input.anthropic_api_key = Some(OsString::from(anthropic_key));
        input.worker_path = Some(OsString::from(format!("/opt/{anthropic_key}/monty")));
        input.prompt = PromptResolution::Resolved {
            base: "system instructions".to_owned(),
            source: PromptSource::Override {
                path: PathBuf::from(format!("/prompts/{openai_key}/{anthropic_key}.md")),
            },
        };
        input.workspace_root = Err(io::Error::other(format!(
            "resolver repeated {openai_key} and {anthropic_key}"
        )));

        let configuration = resolve_startup(input)
            .with_worker_construction_failure(format!("worker repeated {anthropic_key}"));
        let snapshot = configuration.snapshot();
        let mut inspectable = format!(
            "{snapshot:?}\n{}",
            crate::tui::format_startup_configuration(snapshot)
        );
        for problem in snapshot.problems() {
            inspectable.push_str(&format!("\n{problem:?}\n{problem}"));
        }

        assert_eq!(
            snapshot.worker_path().unwrap().as_str(),
            "/opt/[redacted]/monty"
        );
        let PromptProvenance::Override { path } = snapshot.prompt_source() else {
            panic!("override provenance should be retained")
        };
        assert_eq!(path.as_str(), "/prompts/[redacted]/[redacted].md");
        assert_eq!(
            snapshot
                .problems()
                .iter()
                .map(|problem| problem.diagnostic())
                .collect::<Vec<_>>(),
            [
                "Monty worker construction failed: worker repeated [redacted]",
                "Could not determine the current workspace root: resolver repeated [redacted] and [redacted]",
            ]
        );
        assert!(!inspectable.contains(openai_key), "{inspectable}");
        assert!(!inspectable.contains(anthropic_key), "{inspectable}");
        assert!(!inspectable.contains("BOTH-SECRET"), "{inspectable}");
    }

    #[test]
    fn overlapping_keys_are_fully_redacted_from_original_text() {
        let openai_key = "sk-shared";
        let anthropic_key = "sk-shared-private";
        let mut input = ready_input(openai_key);
        input.anthropic_api_key = Some(OsString::from(anthropic_key));
        input.worker_path = Some(OsString::from(format!("/opt/{anthropic_key}/monty")));
        input.workspace_root = Err(io::Error::other(format!(
            "{openai_key}, {anthropic_key}, and sk-shared-private-extra"
        )));

        let snapshot = resolve_startup(input).snapshot;
        assert_eq!(
            snapshot.worker_path().unwrap().as_str(),
            "/opt/[redacted]/monty"
        );
        assert_eq!(
            snapshot.problems()[0].diagnostic(),
            "Could not determine the current workspace root: [redacted], [redacted], and [redacted]-extra"
        );
        assert_eq!(
            super::redact("abcdefg".into(), &["abcde", "cdefg"]),
            "[redacted]"
        );
        assert_eq!(super::redact("abcabcabc".into(), &["abcabc"]), "[redacted]");
    }

    #[test]
    fn restored_environment_effective_preferences_override_files_without_environment() {
        let home = tempdir().expect("home");
        let workspace = tempdir().expect("workspace");
        write_settings(
            home.path(),
            "model = \"openai/gpt-5.6-sol\"\n\n[thinking]\n\"openai/gpt-5.6-sol\" = \"max\"\n\"openai/gpt-5.6-terra\" = \"low\"\n",
        );
        write_settings(
            workspace.path(),
            "model = \"openai/gpt-5.6-luna\"\n\n[thinking]\n\"openai/gpt-5.6-sol\" = \"xhigh\"\n\"openai/gpt-5.6-terra\" = \"medium\"\n",
        );
        let environment_effective = ModelSelection::new(&TERRA, "high")
            .expect("select previous environment-effective Terra");
        let preferences = SessionPreferences::new(
            environment_effective,
            BTreeMap::from([(SOL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid session preferences");

        let restored = resolve_startup_with_session_preferences(
            ready_input_with_settings(home.path(), workspace.path()),
            Some(preferences.clone()),
        );
        assert_eq!(
            ModelSelection::new(restored.snapshot().model(), restored.snapshot().thinking()),
            Some(environment_effective)
        );
        assert_eq!(
            restored.snapshot().settings().thinking_for(&SOL),
            Some("low")
        );

        let mut environment = ready_input_with_settings(home.path(), workspace.path());
        environment.model = Some(OsString::from(SOL.id));
        environment.thinking = Some(OsString::from("xhigh"));
        let environment = resolve_startup_with_session_preferences(environment, Some(preferences));
        assert!(std::ptr::eq(environment.snapshot().model(), &SOL));
        assert_eq!(environment.snapshot().thinking(), "xhigh");
        assert_eq!(
            environment.snapshot().submission_availability(),
            SubmissionAvailability::Available
        );
    }

    #[test]
    fn unset_model_uses_sol_and_keeps_submission_available() {
        let configuration = resolve_startup(ready_input("key"));
        let snapshot = configuration.snapshot();

        assert_eq!(snapshot.model().id, "openai/gpt-5.6-sol");
        assert!(std::ptr::eq(snapshot.model(), DEFAULT_MODEL));
        assert_eq!(
            snapshot.submission_availability(),
            SubmissionAvailability::Available
        );
        assert!(std::ptr::eq(
            configuration
                .ready()
                .expect("configuration should be ready")
                .model(),
            DEFAULT_MODEL
        ));
    }

    #[test]
    fn configured_qualified_model_ids_keep_submission_available() {
        for model in [&ASTRA, &SOL, &TERRA, &LUNA] {
            let mut input = ready_input("key");
            input.model = Some(OsString::from(model.id));

            let configuration = resolve_startup(input);
            let snapshot = configuration.snapshot();

            assert_eq!(snapshot.model().id, model.id);
            assert!(std::ptr::eq(snapshot.model(), model));
            assert_eq!(
                snapshot.submission_availability(),
                SubmissionAvailability::Available
            );
            assert!(std::ptr::eq(
                configuration
                    .ready()
                    .expect("configuration should be ready")
                    .model(),
                model
            ));
        }
    }

    #[test]
    fn new_models_are_selectable_from_the_environment() {
        for (id, thinking) in [
            ("openai/gpt-6.1-sol", "medium"),
            ("anthropic/claude-sonnet-5-5", "high"),
        ] {
            let mut input = ready_input("openai-key");
            input.anthropic_api_key = Some(OsString::from("anthropic-key"));
            input.model = Some(OsString::from(id));
            let configuration = resolve_startup(input);
            let ready = configuration.ready().expect("new model is selectable");
            assert_eq!(ready.model().id, id);
            assert_eq!(ready.thinking(), thinking);
            assert!(configuration.snapshot().problems().is_empty());
        }
    }

    #[test]
    fn hierarchical_settings_flow_into_snapshot_and_ready_startup() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "model = \"openai/gpt-5.6-sol\"\n\n[thinking]\n\"openai/gpt-5.6-sol\" = \"low\"\n\n[subagents]\nallow_nested = false\nmax_concurrent = 3\nmax_starts_per_run = 12\ntimeout_seconds = 60\n",
        );
        write_settings(
            project.path(),
            "model = \"openai/gpt-5.6-terra\"\n\n[thinking]\n\"openai/gpt-5.6-terra\" = \"high\"\n\n[subagents]\nmax_depth = 2\n",
        );
        let mut input = ready_input_with_settings(home.path(), project.path());
        input.model = Some(OsString::from(LUNA.id));
        input.thinking = Some(OsString::from("max"));

        let configuration = resolve_startup(input);
        let snapshot = configuration.snapshot();

        let user_settings_path = home.path().join(".kiki/config.toml");
        let project_settings_path = project.path().join(".kiki/config.toml");
        assert_eq!(snapshot.model().id, LUNA.id);
        assert_eq!(snapshot.thinking(), "max");
        assert_eq!(
            snapshot.settings().user_path(),
            Some(user_settings_path.as_path())
        );
        assert_eq!(
            snapshot.settings().project_path(),
            Some(project_settings_path.as_path())
        );
        assert_eq!(snapshot.settings().thinking_for(&SOL), Some("low"));
        assert_eq!(snapshot.settings().thinking_for(&TERRA), Some("high"));
        let subagents = snapshot.subagents();
        assert!(!subagents.allow_nested());
        assert_eq!(subagents.max_concurrent(), 3);
        assert_eq!(subagents.max_starts_per_run(), 12);
        assert_eq!(subagents.max_depth(), 2);
        assert_eq!(subagents.timeout().as_secs(), 60);
        let ready = configuration.ready().expect("ready configuration");
        assert_eq!(ready.thinking(), "max");
    }

    #[test]
    fn invalid_file_settings_disable_submission_with_source_specific_diagnostics() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(home.path(), "model = [\n");
        write_settings(project.path(), "[subagents]\nmax_depth = 0\n");

        let configuration = resolve_startup(ready_input_with_settings(home.path(), project.path()));
        let snapshot = configuration.snapshot();

        assert_eq!(
            snapshot
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [
                StartupProblemKind::UserSettingsMalformed,
                StartupProblemKind::ProjectSettingsInvalid,
            ]
        );
        assert!(
            snapshot.problems()[1]
                .diagnostic()
                .contains("subagents.max_depth")
        );
        assert_eq!(
            snapshot.submission_availability(),
            SubmissionAvailability::Unavailable
        );
        assert!(configuration.ready().is_none());
    }

    #[test]
    fn settings_diagnostics_and_snapshot_debug_redact_untrusted_content_and_paths() {
        let temporary = tempdir().expect("temporary directory");
        let secret = "sk-settings-SECRET";
        let home = temporary.path().join(secret);
        fs::create_dir_all(&home).expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(&home, &format!("\"{secret}\" = \"{secret}\"\n"));

        let configuration = resolve_startup(ready_input_with_settings(&home, project.path()));
        let snapshot = configuration.snapshot();
        let rendered = crate::tui::format_startup_configuration(snapshot);

        assert_eq!(
            snapshot
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::UserSettingsInvalid]
        );
        assert!(rendered.contains(
            "Problem: user settings file `~/.kiki/config.toml` contains an unknown field"
        ));
        assert!(rendered.contains("Remediation: Remove the unknown field"));
        assert!(!rendered.contains(secret), "{rendered}");
        assert!(!format!("{snapshot:?}").contains(secret));
    }

    #[test]
    fn unqualified_and_unknown_models_disable_submission_with_a_diagnostic() {
        for model in ["gpt-4o", "gpt-5.6-sol", "gpt-5.6-unknown"] {
            let mut input = ready_input("key");
            input.model = Some(OsString::from(model));

            let configuration = resolve_startup(input);
            let snapshot = configuration.snapshot();

            assert_eq!(
                snapshot
                    .problems()
                    .iter()
                    .map(|problem| problem.kind())
                    .collect::<Vec<_>>(),
                [StartupProblemKind::ModelUnknown]
            );
            assert_eq!(
                snapshot.problems()[0].diagnostic(),
                "KIKI_MODEL is not a supported qualified model id."
            );
            assert_eq!(
                snapshot.problems()[0].remediation(),
                "Set KIKI_MODEL to a supported qualified model id (openai/gpt-6-astra, openai/gpt-6.1-sol, openai/gpt-6-sol, openai/gpt-6-luna, openai/gpt-5.6-sol, openai/gpt-5.6-terra, openai/gpt-5.6-luna, anthropic/claude-opus-5-5, anthropic/claude-sonnet-5-5, anthropic/claude-fable-5-1) or leave it unset, then restart Kiki."
            );
            assert!(std::ptr::eq(snapshot.model(), DEFAULT_MODEL));
            assert_eq!(
                snapshot.submission_availability(),
                SubmissionAvailability::Unavailable
            );
            assert!(configuration.ready().is_none());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_model_disables_submission_and_keeps_the_default() {
        use std::os::unix::ffi::OsStringExt;

        let mut input = ready_input("key");
        input.model = Some(OsString::from_vec(vec![b'g', b'p', b't', b'-', 0xff]));

        let configuration = resolve_startup(input);
        let snapshot = configuration.snapshot();

        assert_eq!(
            snapshot
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::ModelInvalidUnicode]
        );
        assert_eq!(
            snapshot.problems()[0].diagnostic(),
            "KIKI_MODEL is not valid Unicode."
        );
        assert_eq!(
            snapshot.problems()[0].remediation(),
            "Set KIKI_MODEL to a supported qualified model id (openai/gpt-6-astra, openai/gpt-6.1-sol, openai/gpt-6-sol, openai/gpt-6-luna, openai/gpt-5.6-sol, openai/gpt-5.6-terra, openai/gpt-5.6-luna, anthropic/claude-opus-5-5, anthropic/claude-sonnet-5-5, anthropic/claude-fable-5-1) or leave it unset, then restart Kiki."
        );
        assert!(std::ptr::eq(snapshot.model(), DEFAULT_MODEL));
        assert_eq!(
            snapshot.submission_availability(),
            SubmissionAvailability::Unavailable
        );
        assert!(configuration.ready().is_none());
    }

    #[test]
    fn model_diagnostic_does_not_echo_an_api_key_used_as_the_model_id() {
        let api_key = "sk-live-MODEL-SECRET";
        let mut input = ready_input(api_key);
        input.model = Some(OsString::from(api_key));

        let configuration = resolve_startup(input);
        let diagnostic = configuration.snapshot().problems()[0].diagnostic();

        assert_eq!(
            diagnostic,
            "KIKI_MODEL is not a supported qualified model id."
        );
        assert!(!diagnostic.contains(api_key), "{diagnostic}");
    }

    #[test]
    fn hostile_api_key_is_redacted_from_every_snapshot_path_and_provenance() {
        let api_key = "sk-live-PATH-SECRET";
        let worker_path = PathBuf::from(format!("/opt/{api_key}/bin/monty"));
        let workspace_root = PathBuf::from(format!("/workspace/{api_key}/project"));
        let prompt_path = PathBuf::from(format!("/prompts/{api_key}/agent.md"));
        let input = StartupInput {
            worker_path: Some(worker_path.clone().into_os_string()),
            model: None,
            thinking: None,
            home: None,
            workspace_root: Ok(workspace_root.clone()),
            prompt: PromptResolution::Resolved {
                base: "system instructions".to_owned(),
                source: PromptSource::Override {
                    path: prompt_path.clone(),
                },
            },
            skills_catalog_guidance: Ok(crate::prompt::skills_catalog_guidance().to_owned()),
            openai_api_key: Some(OsString::from(api_key)),
            anthropic_api_key: None,
            stored_api_keys: Ok(crate::config::auth::StoredKeys::default()),
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
        assert_eq!(ready.prompt_base(), "system instructions");
        assert_eq!(
            ready.api_key(Provider::OpenAi).map(super::ApiKey::expose),
            Some(api_key)
        );
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
        assert_eq!(configuration.construction.prompt_base, None);
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
        assert_eq!(configuration.construction.prompt_base, None);
    }

    #[test]
    fn catalog_guidance_override_is_retained_or_reported_as_a_typed_startup_problem() {
        let mut valid = ready_input("key");
        valid.skills_catalog_guidance = Ok("CUSTOM_GUIDANCE  \n".to_owned());
        let configuration = resolve_startup(valid);
        assert_eq!(
            configuration.ready().unwrap().skills_catalog_guidance(),
            "CUSTOM_GUIDANCE  \n"
        );

        let path = PathBuf::from("/private/catalog-guidance.md");
        let mut unreadable = ready_input("key");
        unreadable.skills_catalog_guidance = Err(PromptError::UnreadableOverride {
            path: path.clone(),
            source: io::Error::new(io::ErrorKind::NotFound, "missing override"),
        });
        let unreadable = resolve_startup(unreadable);
        assert_eq!(
            unreadable
                .snapshot()
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::SkillsCatalogPromptUnreadable]
        );
        assert!(
            unreadable.snapshot().problems()[0]
                .diagnostic()
                .contains("KIKI_SKILLS_CATALOG_PROMPT")
        );
        assert!(unreadable.ready().is_none());

        let mut invalid = ready_input("key");
        invalid.skills_catalog_guidance = Err(PromptError::InvalidUtf8Override {
            path,
            source: String::from_utf8(vec![0xff]).unwrap_err().utf8_error(),
        });
        let invalid = resolve_startup(invalid);
        assert_eq!(
            invalid
                .snapshot()
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [StartupProblemKind::SkillsCatalogPromptInvalidUtf8]
        );
        assert!(invalid.ready().is_none());
    }

    #[test]
    fn absent_empty_and_whitespace_only_api_keys_are_missing() {
        for api_key in [None, Some(""), Some(" \t\n"), Some("\u{2003}")] {
            let mut input = ready_input("placeholder");
            input.openai_api_key = api_key.map(OsString::from);

            let configuration = resolve_startup(input);

            assert_eq!(
                configuration.snapshot().api_key_presence(Provider::OpenAi),
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
            assert!(
                configuration
                    .construction
                    .api_keys
                    .get(Provider::OpenAi)
                    .is_none()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_api_key_is_invalid_without_retaining_its_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = || Some(OsString::from_vec(vec![b's', b'k', b'-', 0xff]));
        let mut openai = ready_input("placeholder");
        openai.openai_api_key = invalid();
        let mut anthropic = ready_input("placeholder");
        anthropic.model = Some(OsString::from(OPUS_5_5.id));
        anthropic.anthropic_api_key = invalid();

        for (input, provider) in [(openai, Provider::OpenAi), (anthropic, Provider::Anthropic)] {
            let configuration = resolve_startup(input);
            let problems = configuration.snapshot().problems();

            assert_eq!(
                configuration.snapshot().api_key_presence(provider),
                ApiKeyPresence::Missing
            );
            assert_eq!(
                problems
                    .iter()
                    .map(|problem| problem.kind())
                    .collect::<Vec<_>>(),
                [StartupProblemKind::ApiKeyInvalidUnicode]
            );
            assert_eq!(
                problems[0].diagnostic(),
                format!("{} is not valid Unicode.", provider.api_key_variable())
            );
            assert!(configuration.construction.api_keys.get(provider).is_none());
        }
    }

    #[test]
    fn problems_have_deterministic_field_and_construction_order() {
        let input = StartupInput {
            worker_path: Some(OsString::from("relative-worker")),
            model: Some(OsString::from("gpt-4o")),
            thinking: None,
            home: None,
            workspace_root: Err(io::Error::other("workspace unavailable")),
            prompt: PromptResolution::Failed {
                source: PromptSource::Override {
                    path: PathBuf::from("override.md"),
                },
                failure: super::PromptFailure::InvalidUtf8 {
                    detail: "invalid UTF-8".to_owned(),
                },
            },
            skills_catalog_guidance: Ok(crate::prompt::skills_catalog_guidance().to_owned()),
            openai_api_key: None,
            anthropic_api_key: None,
            stored_api_keys: Ok(crate::config::auth::StoredKeys::default()),
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
                StartupProblemKind::ModelUnknown,
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
            base: "system instructions".to_owned(),
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
                .api_keys
                .get(Provider::OpenAi)
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
            model: None,
            thinking: None,
            home: None,
            workspace_root: Ok(PathBuf::from(format!("/workspace/{api_key}/project\r"))),
            prompt: PromptResolution::Resolved {
                base: "system instructions".to_owned(),
                source: PromptSource::Override {
                    path: PathBuf::from(format!("/prompts/{api_key}/agent\t\x1b.md")),
                },
            },
            skills_catalog_guidance: Ok(crate::prompt::skills_catalog_guidance().to_owned()),
            openai_api_key: Some(OsString::from(api_key)),
            anthropic_api_key: None,
            stored_api_keys: Ok(crate::config::auth::StoredKeys::default()),
        })
        .with_worker_construction_failure(format!("worker failure \x1b {api_key}"))
        .with_workspace_construction_failure(format!("workspace failure \r {api_key}"));

        let rendered = crate::tui::format_startup_configuration(configuration.snapshot());

        assert!(rendered.contains("Worker: /worker/[redacted]/monty    \\x1B"));
        assert!(rendered.contains("Workspace: /workspace/[redacted]/project\\x0D"));
        assert!(rendered.contains("Model: openai/gpt-5.6-sol (context: 1.05m, max output: 128k)"));
        assert!(rendered.contains("Prompt: override /prompts/[redacted]/agent    \\x1B.md"));
        assert!(rendered.contains("OpenAI API key: present"));
        assert!(rendered.contains("Anthropic API key: missing"));
        assert!(rendered.contains("Submission: unavailable"));
        assert!(rendered.contains(
            "Problem: Monty worker construction failed: worker failure \\x1B [redacted]"
        ));
        assert!(rendered.contains(
            "Problem: Workspace construction failed: workspace failure \\x0D [redacted]"
        ));
        assert!(rendered.contains("Remediation: Check that the Monty worker (this kiki executable, or MONTY_BIN if set) can start, then restart Kiki."));
        assert!(rendered.contains("Remediation: Check the workspace directory permissions and filesystem access, then restart Kiki."));
        assert!(!rendered.contains(api_key), "{rendered}");
        assert!(!rendered.contains('\x1b'), "{rendered}");

        let bundled = resolve_startup(ready_input("key"));
        let bundled_rendered = crate::tui::format_startup_configuration(bundled.snapshot());
        assert!(bundled_rendered.contains("Prompt: bundled system"));
        assert!(bundled_rendered.contains("Submission: available"));
    }
}
