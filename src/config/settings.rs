use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value, value};

use super::{
    DEFAULT_MODEL, ModelConfiguration, StartupProblemKind,
    constants::{
        DEFAULT_SUBAGENT_ALLOW_NESTED, DEFAULT_SUBAGENT_MAX_CONCURRENT, DEFAULT_SUBAGENT_MAX_DEPTH,
        DEFAULT_SUBAGENT_MAX_STARTS_PER_RUN, DEFAULT_SUBAGENT_TIMEOUT, SUBAGENT_TIMER_HEADROOM,
    },
    model_by_id,
};

const MAX_DOCUMENT_PROBLEMS: usize = 3;
const USER_SETTINGS_LABEL: &str = "user settings file `~/.kiki/config.toml`";
const PROJECT_SETTINGS_LABEL: &str = "project settings file `.kiki/config.toml`";
const MODEL_REMEDIATION: &str = "Set it to a supported qualified model id (openai/gpt-6-astra, openai/gpt-5.6-sol, openai/gpt-5.6-terra, openai/gpt-5.6-luna) or remove it, then restart Kiki.";
const ENVIRONMENT_MODEL_REMEDIATION: &str = "Set KIKI_MODEL to a supported qualified model id (openai/gpt-6-astra, openai/gpt-5.6-sol, openai/gpt-5.6-terra, openai/gpt-5.6-luna) or leave it unset, then restart Kiki.";

pub(crate) struct Input {
    pub(crate) home: Option<OsString>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) model: Option<OsString>,
    pub(crate) thinking: Option<OsString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsOrigin {
    BuiltIn,
    User,
    Project,
    Environment,
}

impl fmt::Display for SettingsOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BuiltIn => "built-in defaults",
            Self::User => "user settings",
            Self::Project => "project settings",
            Self::Environment => "environment variables",
        })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsContext {
    user_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
    model_origin: SettingsOrigin,
    thinking: BTreeMap<&'static str, PersistedThinking>,
    environment_thinking_model: Option<&'static str>,
}

impl fmt::Debug for SettingsContext {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SettingsContext")
            .field(
                "user_path",
                &self.user_path.as_ref().map(|_| "[user settings path]"),
            )
            .field(
                "project_path",
                &self
                    .project_path
                    .as_ref()
                    .map(|_| "[project settings path]"),
            )
            .field("model_origin", &self.model_origin)
            .field("thinking", &self.thinking)
            .field(
                "environment_thinking_model",
                &self.environment_thinking_model,
            )
            .finish()
    }
}

impl SettingsContext {
    pub(crate) fn user_path(&self) -> Option<&Path> {
        self.user_path.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn project_path(&self) -> Option<&Path> {
        self.project_path.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn model_origin(&self) -> SettingsOrigin {
        self.model_origin
    }

    pub(crate) fn thinking_for(&self, model: &'static ModelConfiguration) -> Option<&'static str> {
        self.thinking.get(model.id).map(|selection| selection.value)
    }

    pub(crate) fn save_model(
        &self,
        model: &'static ModelConfiguration,
    ) -> Result<SaveOutcome, SaveError> {
        update_user_document(self.user_path(), |document| {
            replace_string(&mut document["model"], model.id);
            Ok(())
        })?;
        Ok(self.model_save_outcome())
    }

    pub(crate) fn save_thinking(
        &self,
        model: &'static ModelConfiguration,
        thinking: &'static str,
    ) -> Result<SaveOutcome, SaveError> {
        if !model.supports_thinking(thinking) {
            return Err(SaveError::InvalidSelection);
        }
        update_user_document(self.user_path(), |document| {
            if document.as_table().get("thinking").is_none() {
                document
                    .as_table_mut()
                    .insert("thinking", Item::Table(Table::new()));
            }
            let thinking_table = document["thinking"]
                .as_table_mut()
                .ok_or(SaveError::InvalidExistingUserSettings)?;
            if let Some(existing) = thinking_table.get_mut(model.id) {
                replace_string(existing, thinking);
            } else {
                thinking_table.insert(model.id, value(thinking));
            }
            Ok(())
        })?;
        if !matches!(
            self.thinking_origin_for(model),
            SettingsOrigin::BuiltIn | SettingsOrigin::User
        ) {
            Ok(SaveOutcome::SavedButMasked {
                by: self.thinking_origin_for(model),
            })
        } else {
            Ok(SaveOutcome::Saved)
        }
    }

    fn model_save_outcome(&self) -> SaveOutcome {
        match self.model_origin {
            SettingsOrigin::BuiltIn | SettingsOrigin::User => SaveOutcome::Saved,
            by => SaveOutcome::SavedButMasked { by },
        }
    }

    fn thinking_origin_for(&self, model: &'static ModelConfiguration) -> SettingsOrigin {
        if self.environment_thinking_model == Some(model.id) {
            SettingsOrigin::Environment
        } else {
            self.thinking
                .get(model.id)
                .map_or(SettingsOrigin::BuiltIn, |selection| selection.origin)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PersistedThinking {
    value: &'static str,
    origin: SettingsOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SaveOutcome {
    Saved,
    SavedButMasked { by: SettingsOrigin },
}

#[derive(Debug)]
pub(crate) enum SaveError {
    UserSettingsUnavailable,
    InvalidExistingUserSettings,
    InvalidSelection,
    Io(io::Error),
}

impl fmt::Display for SaveOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Saved => formatter.write_str("User default saved."),
            Self::SavedButMasked { by } => {
                write!(
                    formatter,
                    "User default saved; currently overridden by {by}."
                )
            }
        }
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UserSettingsUnavailable => formatter
                .write_str("user settings are unavailable because HOME is unset or non-Unicode"),
            Self::InvalidExistingUserSettings => {
                formatter.write_str("existing user settings are not valid TOML")
            }
            Self::InvalidSelection => {
                formatter.write_str("selection is not supported by this model")
            }
            Self::Io(error) => write!(formatter, "could not save user settings ({})", error.kind()),
        }
    }
}

impl std::error::Error for SaveError {}

pub(crate) struct Problem {
    kind: StartupProblemKind,
    diagnostic: String,
    remediation: &'static str,
}

impl Problem {
    pub(crate) fn kind(&self) -> StartupProblemKind {
        self.kind
    }

    pub(crate) fn diagnostic(&self) -> &str {
        &self.diagnostic
    }

    pub(crate) fn remediation(&self) -> &str {
        self.remediation
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SubagentSettings {
    allow_nested: bool,
    max_concurrent: usize,
    max_starts_per_run: usize,
    max_depth: usize,
    timeout: Duration,
}

impl Default for SubagentSettings {
    fn default() -> Self {
        Self {
            allow_nested: DEFAULT_SUBAGENT_ALLOW_NESTED,
            max_concurrent: DEFAULT_SUBAGENT_MAX_CONCURRENT,
            max_starts_per_run: DEFAULT_SUBAGENT_MAX_STARTS_PER_RUN,
            max_depth: DEFAULT_SUBAGENT_MAX_DEPTH,
            timeout: DEFAULT_SUBAGENT_TIMEOUT,
        }
    }
}

impl SubagentSettings {
    pub(crate) fn allow_nested(self) -> bool {
        self.allow_nested
    }

    pub(crate) fn max_concurrent(self) -> usize {
        self.max_concurrent
    }

    pub(crate) fn max_starts_per_run(self) -> usize {
        self.max_starts_per_run
    }

    pub(crate) fn max_depth(self) -> usize {
        self.max_depth
    }

    pub(crate) fn timeout(self) -> Duration {
        self.timeout
    }
}

pub(crate) struct Resolution {
    model: &'static ModelConfiguration,
    thinking: &'static str,
    subagents: SubagentSettings,
    context: SettingsContext,
    problems: Vec<Problem>,
}

impl Resolution {
    pub(crate) fn model(&self) -> &'static ModelConfiguration {
        self.model
    }

    pub(crate) fn thinking(&self) -> &'static str {
        self.thinking
    }

    pub(crate) fn subagents(&self) -> SubagentSettings {
        self.subagents
    }

    pub(crate) fn context(&self) -> &SettingsContext {
        &self.context
    }

    pub(crate) fn problems(&self) -> &[Problem] {
        &self.problems
    }
}

pub(crate) fn resolve(input: Input) -> Resolution {
    let user_path = input.home.and_then(user_settings_path);
    let project_path = input
        .workspace_root
        .map(|workspace_root| workspace_root.join(".kiki/config.toml"));
    let (user, mut problems) = load_layer(user_path.as_deref(), FileLayer::User);
    let (project, project_problems) = load_layer(project_path.as_deref(), FileLayer::Project);
    problems.extend(project_problems);

    let mut model = DEFAULT_MODEL;
    let mut model_origin = SettingsOrigin::BuiltIn;
    let mut thinking = BTreeMap::new();
    let mut subagents = SubagentSettings::default();
    apply_layer(
        user.as_ref(),
        SettingsOrigin::User,
        &mut model,
        &mut model_origin,
        &mut thinking,
        &mut subagents,
    );
    apply_layer(
        project.as_ref(),
        SettingsOrigin::Project,
        &mut model,
        &mut model_origin,
        &mut thinking,
        &mut subagents,
    );

    let model_is_valid = match input.model {
        None => true,
        Some(model_input) => match model_input.into_string() {
            Err(_) => {
                problems.push(environment_problem(
                    StartupProblemKind::ModelInvalidUnicode,
                    "KIKI_MODEL is not valid Unicode.",
                    ENVIRONMENT_MODEL_REMEDIATION,
                ));
                false
            }
            Ok(model_input) => match model_by_id(&model_input) {
                Some(selected) => {
                    model = selected;
                    model_origin = SettingsOrigin::Environment;
                    true
                }
                None => {
                    problems.push(environment_problem(
                        StartupProblemKind::ModelUnknown,
                        "KIKI_MODEL is not a supported qualified model id.",
                        ENVIRONMENT_MODEL_REMEDIATION,
                    ));
                    false
                }
            },
        },
    };

    let mut selected_thinking = thinking
        .get(model.id)
        .map_or(model.default_thinking, |selection| selection.value);
    let mut environment_thinking_model = None;
    if model_is_valid && let Some(thinking_input) = input.thinking {
        match thinking_input.into_string() {
            Err(_) => problems.push(thinking_problem(
                StartupProblemKind::ThinkingInvalidUnicode,
                model,
                "KIKI_THINKING is not valid Unicode",
            )),
            Ok(thinking_input) if thinking_input.is_empty() => problems.push(thinking_problem(
                StartupProblemKind::ThinkingEmpty,
                model,
                "KIKI_THINKING is empty",
            )),
            Ok(thinking_input) => match environment_thinking_level(model, &thinking_input) {
                Ok(level) => {
                    selected_thinking = level;
                    environment_thinking_model = Some(model.id);
                }
                Err(StartupProblemKind::ThinkingUnsupported) => problems.push(thinking_problem(
                    StartupProblemKind::ThinkingUnsupported,
                    model,
                    "KIKI_THINKING is not supported",
                )),
                Err(StartupProblemKind::ThinkingUnknown) => problems.push(thinking_problem(
                    StartupProblemKind::ThinkingUnknown,
                    model,
                    "KIKI_THINKING is not a known thinking level",
                )),
                Err(_) => unreachable!("thinking validation only returns thinking problems"),
            },
        }
    }

    Resolution {
        model,
        thinking: selected_thinking,
        subagents,
        context: SettingsContext {
            user_path,
            project_path,
            model_origin,
            thinking,
            environment_thinking_model,
        },
        problems,
    }
}

fn user_settings_path(home: OsString) -> Option<PathBuf> {
    let home = home.into_string().ok()?;
    (!home.is_empty()).then(|| PathBuf::from(home).join(".kiki/config.toml"))
}

fn apply_layer(
    settings: Option<&FileSettings>,
    origin: SettingsOrigin,
    model: &mut &'static ModelConfiguration,
    model_origin: &mut SettingsOrigin,
    thinking: &mut BTreeMap<&'static str, PersistedThinking>,
    subagents: &mut SubagentSettings,
) {
    let Some(settings) = settings else {
        return;
    };
    if let Some(selected) = settings.model {
        *model = selected;
        *model_origin = origin;
    }
    thinking.extend(settings.thinking.iter().map(|(model, thinking)| {
        (
            *model,
            PersistedThinking {
                value: thinking,
                origin,
            },
        )
    }));
    if let Some(allow_nested) = settings.subagents.allow_nested {
        subagents.allow_nested = allow_nested;
    }
    if let Some(max_concurrent) = settings.subagents.max_concurrent {
        subagents.max_concurrent = max_concurrent;
    }
    if let Some(max_starts_per_run) = settings.subagents.max_starts_per_run {
        subagents.max_starts_per_run = max_starts_per_run;
    }
    if let Some(max_depth) = settings.subagents.max_depth {
        subagents.max_depth = max_depth;
    }
    if let Some(timeout) = settings.subagents.timeout {
        subagents.timeout = timeout;
    }
}

fn load_layer(path: Option<&Path>, layer: FileLayer) -> (Option<FileSettings>, Vec<Problem>) {
    let Some(path) = path else {
        return (None, Vec::new());
    };
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (None, Vec::new()),
        Err(_) => {
            return (
                None,
                vec![file_problem(
                    layer,
                    FileProblem::Unreadable,
                    "could not be read",
                )],
            );
        }
    };
    let Ok(content) = String::from_utf8(content) else {
        return (
            None,
            vec![file_problem(
                layer,
                FileProblem::InvalidUtf8,
                "is not valid UTF-8",
            )],
        );
    };
    let Ok(document) = content.parse::<DocumentMut>() else {
        return (
            None,
            vec![file_problem(
                layer,
                FileProblem::Malformed,
                "contains malformed TOML",
            )],
        );
    };
    validate_document(&document, layer)
}

fn validate_document(
    document: &DocumentMut,
    layer: FileLayer,
) -> (Option<FileSettings>, Vec<Problem>) {
    let mut settings = FileSettings::default();
    let mut problems = DocumentProblems::new(layer);

    for (key, item) in document.iter() {
        match key {
            "model" => match item.as_str().and_then(model_by_id) {
                Some(model) => settings.model = Some(model),
                None if item.as_str().is_some() => problems.push(
                    "field `model` must be a supported qualified model id",
                    MODEL_REMEDIATION,
                ),
                None => problems.push(
                    "field `model` must be a string containing a supported qualified model id",
                    MODEL_REMEDIATION,
                ),
            },
            "thinking" => validate_thinking_table(item, &mut settings.thinking, &mut problems),
            "subagents" => validate_subagents_table(item, &mut settings.subagents, &mut problems),
            _ => problems.push(
                "contains an unknown field; only `model`, `[thinking]`, and `[subagents]` are allowed",
                "Remove the unknown field, then restart Kiki.",
            ),
        }
    }

    if problems.problems.is_empty() {
        (Some(settings), Vec::new())
    } else {
        (None, problems.finish())
    }
}

fn validate_thinking_table(
    item: &Item,
    thinking: &mut BTreeMap<&'static str, &'static str>,
    problems: &mut DocumentProblems,
) {
    let Some(table) = item.as_table() else {
        problems.push(
            "field `thinking` must be a TOML table keyed by qualified model id",
            "Use [thinking] followed by quoted qualified model ids and supported string values, then restart Kiki.",
        );
        return;
    };

    for (key, item) in table.iter() {
        let Some(model) = model_by_id(key) else {
            problems.push(
                "field `thinking` contains an unknown model key; use openai/gpt-6-astra, openai/gpt-5.6-sol, openai/gpt-5.6-terra, or openai/gpt-5.6-luna",
                "Use only supported qualified model ids as [thinking] keys, then restart Kiki.",
            );
            continue;
        };
        let Some(level) = item.as_str() else {
            problems.push(
                format!(
                    "field `thinking` for model `{}` must be a string; valid levels are {}",
                    model.id,
                    model.thinking_levels.join(", ")
                ),
                "Set each [thinking] value to a supported thinking level for its model, then restart Kiki.",
            );
            continue;
        };
        let Some(level) = model
            .thinking_levels
            .iter()
            .copied()
            .find(|candidate| *candidate == level)
        else {
            problems.push(
                format!(
                    "field `thinking` for model `{}` must be one of {}",
                    model.id,
                    model.thinking_levels.join(", ")
                ),
                "Set it to a thinking level supported by that model, then restart Kiki.",
            );
            continue;
        };
        thinking.insert(model.id, level);
    }
}

fn validate_subagents_table(
    item: &Item,
    subagents: &mut FileSubagentSettings,
    problems: &mut DocumentProblems,
) {
    let Some(table) = item.as_table() else {
        problems.push(
            "field `subagents` must be a TOML table",
            "Use [subagents] with supported settings, then restart Kiki.",
        );
        return;
    };

    for (key, item) in table.iter() {
        match key {
            "allow_nested" => match item.as_bool() {
                Some(value) => subagents.allow_nested = Some(value),
                None => problems.push(
                    "field `subagents.allow_nested` must be a boolean",
                    "Set it to true or false, then restart Kiki.",
                ),
            },
            "max_concurrent" => {
                subagents.max_concurrent = positive_subagent_counter(item, key, problems);
            }
            "max_starts_per_run" => {
                subagents.max_starts_per_run = positive_subagent_counter(item, key, problems);
            }
            "max_depth" => {
                subagents.max_depth = positive_subagent_counter(item, key, problems);
            }
            "timeout_seconds" => {
                subagents.timeout = positive_subagent_timeout(item, problems);
            }
            _ => problems.push(
                "table `subagents` contains an unknown key; only `allow_nested`, `max_concurrent`, `max_starts_per_run`, `max_depth`, and `timeout_seconds` are allowed",
                "Remove the unknown key from table `subagents`, then restart Kiki.",
            ),
        }
    }
}

fn positive_subagent_counter(
    item: &Item,
    field: &str,
    problems: &mut DocumentProblems,
) -> Option<usize> {
    let value = item
        .as_integer()
        .and_then(|value| usize::try_from(value).ok());
    match value.filter(|value| *value > 0) {
        Some(value) => Some(value),
        None => {
            problems.push(
                format!(
                    "field `subagents.{field}` must be a positive integer that fits this platform"
                ),
                "Set it to a positive integer that fits this platform, then restart Kiki.",
            );
            None
        }
    }
}

fn positive_subagent_timeout(item: &Item, problems: &mut DocumentProblems) -> Option<Duration> {
    positive_subagent_timeout_at(item, problems, Instant::now())
}

fn positive_subagent_timeout_at(
    item: &Item,
    problems: &mut DocumentProblems,
    now: Instant,
) -> Option<Duration> {
    let seconds = item
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0);
    let Some(seconds) = seconds else {
        problems.push(
            "field `subagents.timeout_seconds` must be a positive integer",
            "Set it to a positive integer, then restart Kiki.",
        );
        return None;
    };
    let timeout = Duration::from_secs(seconds);
    if now.checked_add(timeout + SUBAGENT_TIMER_HEADROOM).is_some() {
        return Some(timeout);
    }
    problems.push(
        "field `subagents.timeout_seconds` must be a positive integer representable as a timer-safe deadline",
        "Set it to a smaller positive integer, then restart Kiki.",
    );
    None
}

fn environment_thinking_level(
    model: &ModelConfiguration,
    level: &str,
) -> Result<&'static str, StartupProblemKind> {
    model
        .thinking_levels
        .iter()
        .copied()
        .find(|candidate| *candidate == level)
        .ok_or_else(|| {
            if known_thinking_level(level) {
                StartupProblemKind::ThinkingUnsupported
            } else {
                StartupProblemKind::ThinkingUnknown
            }
        })
}

fn known_thinking_level(level: &str) -> bool {
    super::models::CATALOGUE
        .iter()
        .any(|model| model.supports_thinking(level))
}

#[derive(Default)]
struct FileSettings {
    model: Option<&'static ModelConfiguration>,
    thinking: BTreeMap<&'static str, &'static str>,
    subagents: FileSubagentSettings,
}

#[derive(Default)]
struct FileSubagentSettings {
    allow_nested: Option<bool>,
    max_concurrent: Option<usize>,
    max_starts_per_run: Option<usize>,
    max_depth: Option<usize>,
    timeout: Option<Duration>,
}

#[derive(Clone, Copy)]
enum FileLayer {
    User,
    Project,
}

impl FileLayer {
    fn label(self) -> &'static str {
        match self {
            Self::User => USER_SETTINGS_LABEL,
            Self::Project => PROJECT_SETTINGS_LABEL,
        }
    }
}

#[derive(Clone, Copy)]
enum FileProblem {
    Unreadable,
    InvalidUtf8,
    Malformed,
    Invalid,
}

fn file_problem(layer: FileLayer, problem: FileProblem, detail: &str) -> Problem {
    let kind = match (layer, problem) {
        (FileLayer::User, FileProblem::Unreadable) => StartupProblemKind::UserSettingsUnreadable,
        (FileLayer::User, FileProblem::InvalidUtf8) => StartupProblemKind::UserSettingsInvalidUtf8,
        (FileLayer::User, FileProblem::Malformed) => StartupProblemKind::UserSettingsMalformed,
        (FileLayer::User, FileProblem::Invalid) => StartupProblemKind::UserSettingsInvalid,
        (FileLayer::Project, FileProblem::Unreadable) => {
            StartupProblemKind::ProjectSettingsUnreadable
        }
        (FileLayer::Project, FileProblem::InvalidUtf8) => {
            StartupProblemKind::ProjectSettingsInvalidUtf8
        }
        (FileLayer::Project, FileProblem::Malformed) => {
            StartupProblemKind::ProjectSettingsMalformed
        }
        (FileLayer::Project, FileProblem::Invalid) => StartupProblemKind::ProjectSettingsInvalid,
    };
    Problem {
        kind,
        diagnostic: format!("{} {detail}.", layer.label()),
        remediation: "Fix or remove the settings file, then restart Kiki.",
    }
}

fn environment_problem(
    kind: StartupProblemKind,
    diagnostic: &'static str,
    remediation: &'static str,
) -> Problem {
    Problem {
        kind,
        diagnostic: diagnostic.to_owned(),
        remediation,
    }
}

fn thinking_problem(
    kind: StartupProblemKind,
    model: &'static ModelConfiguration,
    prefix: &str,
) -> Problem {
    Problem {
        kind,
        diagnostic: format!(
            "{prefix} for effective model `{}`; valid levels are {}.",
            model.id,
            model.thinking_levels.join(", ")
        ),
        remediation: "Set KIKI_THINKING to one of the valid levels or leave it unset, then restart Kiki.",
    }
}

struct DocumentProblems {
    layer: FileLayer,
    problems: Vec<Problem>,
    omitted: bool,
}

impl DocumentProblems {
    fn new(layer: FileLayer) -> Self {
        Self {
            layer,
            problems: Vec::new(),
            omitted: false,
        }
    }

    fn push(&mut self, detail: impl Into<String>, remediation: &'static str) {
        if self.problems.len() < MAX_DOCUMENT_PROBLEMS {
            self.problems.push(Problem {
                kind: match self.layer {
                    FileLayer::User => StartupProblemKind::UserSettingsInvalid,
                    FileLayer::Project => StartupProblemKind::ProjectSettingsInvalid,
                },
                diagnostic: format!("{} {}.", self.layer.label(), detail.into()),
                remediation,
            });
        } else {
            self.omitted = true;
        }
    }

    fn finish(mut self) -> Vec<Problem> {
        if self.omitted {
            let last = self
                .problems
                .last_mut()
                .expect("document errors exceed a non-zero limit");
            *last = file_problem(
                self.layer,
                FileProblem::Invalid,
                "has additional invalid settings omitted from this diagnostic",
            );
        }
        self.problems
    }
}

fn replace_string(item: &mut Item, text: &str) {
    let Some(existing) = item.as_value_mut() else {
        *item = value(text);
        return;
    };
    let decor = existing.decor().clone();
    let mut replacement = Value::from(text);
    *replacement.decor_mut() = decor;
    *existing = replacement;
}

fn update_user_document(
    user_path: Option<&Path>,
    update: impl FnOnce(&mut DocumentMut) -> Result<(), SaveError>,
) -> Result<(), SaveError> {
    let Some(user_path) = user_path else {
        return Err(SaveError::UserSettingsUnavailable);
    };
    let mut document = match fs::read_to_string(user_path) {
        Ok(content) => content
            .parse::<DocumentMut>()
            .map_err(|_| SaveError::InvalidExistingUserSettings)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => DocumentMut::new(),
        Err(error) => return Err(SaveError::Io(error)),
    };
    update(&mut document)?;
    write_document(user_path, &document.to_string())
}

fn write_document(path: &Path, content: &str) -> Result<(), SaveError> {
    let parent = path.parent().ok_or(SaveError::UserSettingsUnavailable)?;
    fs::create_dir_all(parent).map_err(SaveError::Io)?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(SaveError::Io)?;
    temporary
        .write_all(content.as_bytes())
        .map_err(SaveError::Io)?;
    temporary.as_file().sync_all().map_err(SaveError::Io)?;
    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| SaveError::Io(error.error))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use crate::config::{
        constants::{SUBAGENT_TIMER_HEADROOM, latest_representable_instant},
        models::{ASTRA, LUNA, SOL, TERRA},
    };

    use super::{Input, SubagentSettings, resolve};

    #[test]
    fn astra_uses_low_by_default_and_openai_models_reject_none() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let mut selected = input(Some(home.path()), Some(project.path()));
        selected.model = Some(OsString::from(ASTRA.id));
        let resolution = resolve(selected);

        assert!(std::ptr::eq(resolution.model(), &ASTRA));
        assert_eq!(resolution.thinking(), "low");
        assert!(resolution.problems().is_empty());

        let mut unsupported = input(Some(home.path()), Some(project.path()));
        unsupported.model = Some(OsString::from(ASTRA.id));
        unsupported.thinking = Some(OsString::from("none"));
        let unsupported = resolve(unsupported);
        let problem = unsupported.problems().first().expect("thinking problem");
        assert!(
            problem
                .diagnostic()
                .contains("low, medium, high, xhigh, max")
        );
        assert!(!problem.diagnostic().contains("none,"));
    }

    #[test]
    fn project_subagent_limit_overrides_only_matching_user_limits() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "[subagents]\nallow_nested = false\nmax_concurrent = 3\nmax_starts_per_run = 12\nmax_depth = 2\ntimeout_seconds = 60\n",
        );
        write_settings(project.path(), "[subagents]\nmax_concurrent = 5\n");

        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let subagents = resolution.subagents();

        assert!(!subagents.allow_nested());
        assert_eq!(subagents.max_concurrent(), 5);
        assert_eq!(subagents.max_starts_per_run(), 12);
        assert_eq!(subagents.max_depth(), 2);
        assert_eq!(subagents.timeout().as_secs(), 60);
        assert!(resolution.problems().is_empty());
    }

    #[test]
    fn project_model_overrides_user_model_while_unrelated_thinking_is_inherited() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let user_settings = home.path().join(".kiki/config.toml");
        let project_settings = project.path().join(".kiki/config.toml");
        fs::create_dir_all(user_settings.parent().expect("user settings parent"))
            .expect("user settings directory");
        fs::create_dir_all(project_settings.parent().expect("project settings parent"))
            .expect("project settings directory");
        fs::write(
            user_settings,
            "model = \"openai/gpt-5.6-sol\"\n\n[thinking]\n\"openai/gpt-5.6-luna\" = \"high\"\n",
        )
        .expect("user settings");
        fs::write(project_settings, "model = \"openai/gpt-5.6-terra\"\n")
            .expect("project settings");

        let resolution = resolve(Input {
            home: Some(home.path().as_os_str().to_os_string()),
            workspace_root: Some(project.path().to_path_buf()),
            model: None,
            thinking: None,
        });

        assert_eq!(resolution.model().id, TERRA.id);
        assert_eq!(resolution.context().thinking_for(&LUNA), Some("high"));
        assert!(resolution.problems().is_empty());
        assert_eq!(resolution.thinking(), TERRA.default_thinking);
        assert_eq!(resolution.context().thinking_for(&SOL), None);
    }

    fn input(home: Option<&Path>, workspace_root: Option<&Path>) -> Input {
        Input {
            home: home.map(|path| path.as_os_str().to_os_string()),
            workspace_root: workspace_root.map(Path::to_path_buf),
            model: None,
            thinking: None,
        }
    }

    fn write_settings(root: &Path, content: &str) {
        let path = root.join(".kiki/config.toml");
        fs::create_dir_all(path.parent().expect("settings parent")).expect("settings directory");
        fs::write(path, content).expect("settings file");
    }

    fn assert_default_subagents(subagents: SubagentSettings) {
        assert!(subagents.allow_nested());
        assert_eq!(subagents.max_concurrent(), 8);
        assert_eq!(subagents.max_starts_per_run(), 64);
        assert_eq!(subagents.max_depth(), 4);
        assert_eq!(subagents.timeout().as_secs(), 900);
    }

    fn instant_with_tokio_unsafe_one_second_deadline() -> Instant {
        let latest = latest_representable_instant();
        let now = latest
            .checked_sub(Duration::from_secs(1))
            .expect("one-second timeout start must be representable");
        let deadline = now
            .checked_add(Duration::from_secs(1))
            .expect("raw one-second deadline must be representable");
        assert!(deadline.checked_add(SUBAGENT_TIMER_HEADROOM).is_none());
        now
    }

    #[test]
    fn subagent_defaults_apply_with_absent_files_or_tables() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        assert_default_subagents(resolution.subagents());
        assert!(resolution.problems().is_empty());

        write_settings(home.path(), "model = \"openai/gpt-5.6-terra\"\n");
        write_settings(project.path(), "[subagents]\n");
        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_default_subagents(resolution.subagents());
        assert!(resolution.problems().is_empty());
    }

    #[test]
    fn tokio_unsafe_toml_timeout_is_rejected() {
        let document = "timeout_seconds = 1\n"
            .parse::<toml_edit::DocumentMut>()
            .expect("timeout setting");
        let mut problems = super::DocumentProblems::new(super::FileLayer::User);

        let timeout = super::positive_subagent_timeout_at(
            &document["timeout_seconds"],
            &mut problems,
            instant_with_tokio_unsafe_one_second_deadline(),
        );

        assert_eq!(timeout, None);
        let problems = problems.finish();
        let [problem] = problems.as_slice() else {
            panic!("timer-unsafe timeout reports exactly one problem");
        };
        assert_eq!(
            problem.kind(),
            crate::config::StartupProblemKind::UserSettingsInvalid
        );
        assert_eq!(
            problem.diagnostic(),
            "user settings file `~/.kiki/config.toml` field `subagents.timeout_seconds` must be a positive integer representable as a timer-safe deadline."
        );
        assert_eq!(
            problem.remediation(),
            "Set it to a smaller positive integer, then restart Kiki."
        );
        assert!(problem.diagnostic().len() <= 256);
        assert!(problem.remediation().len() <= 256);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn counters_above_u32_max_are_accepted_when_usize_can_represent_them() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let value = u32::MAX as usize + 1;
        write_settings(
            home.path(),
            &format!(
                "[subagents]\nmax_concurrent = {value}\nmax_starts_per_run = {value}\nmax_depth = {value}\n"
            ),
        );

        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let subagents = resolution.subagents();

        assert_eq!(
            u64::try_from(subagents.max_concurrent()).expect("counter fits u64"),
            value as u64
        );
        assert_eq!(
            u64::try_from(subagents.max_starts_per_run()).expect("counter fits u64"),
            value as u64
        );
        assert_eq!(
            u64::try_from(subagents.max_depth()).expect("counter fits u64"),
            value as u64
        );
        assert!(resolution.problems().is_empty());
    }

    #[cfg(any(target_pointer_width = "16", target_pointer_width = "32"))]
    #[test]
    fn counter_value_that_overflows_usize_is_rejected() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let value = u64::try_from(usize::MAX).expect("usize fits u64") + 1;
        write_settings(
            home.path(),
            &format!("[subagents]\nmax_concurrent = {value}\n"),
        );

        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_default_subagents(resolution.subagents());
        assert_eq!(
            resolution.problems()[0].kind(),
            crate::config::StartupProblemKind::UserSettingsInvalid
        );
        assert!(
            resolution.problems()[0]
                .diagnostic()
                .contains("subagents.max_concurrent")
        );
    }

    #[test]
    fn invalid_subagent_layer_contributes_no_partial_values() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "model = \"openai/gpt-5.6-terra\"\n\n[subagents]\nallow_nested = false\nmax_concurrent = 3\nunknown = true\n",
        );
        write_settings(project.path(), "[subagents]\nmax_starts_per_run = 9\n");

        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let subagents = resolution.subagents();

        assert_eq!(resolution.model().id, SOL.id);
        assert!(subagents.allow_nested());
        assert_eq!(subagents.max_concurrent(), 8);
        assert_eq!(subagents.max_starts_per_run(), 9);
        assert_eq!(subagents.max_depth(), 4);
        assert_eq!(subagents.timeout().as_secs(), 900);
        assert_eq!(
            resolution
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [crate::config::StartupProblemKind::UserSettingsInvalid]
        );
    }

    #[test]
    fn invalid_subagent_values_and_keys_are_rejected() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        for field in [
            "max_concurrent",
            "max_starts_per_run",
            "max_depth",
            "timeout_seconds",
        ] {
            for value in ["\"8\"", "false", "0", "-1"] {
                write_settings(home.path(), &format!("[subagents]\n{field} = {value}\n"));
                let resolution = resolve(input(Some(home.path()), Some(project.path())));

                assert_default_subagents(resolution.subagents());
                assert_eq!(
                    resolution.problems()[0].kind(),
                    crate::config::StartupProblemKind::UserSettingsInvalid,
                    "{field} = {value}"
                );
                let problem = &resolution.problems()[0];
                assert!(
                    problem.diagnostic().contains(&format!("subagents.{field}")),
                    "{field} = {value}"
                );
                if field == "timeout_seconds" {
                    assert!(!problem.diagnostic().contains(&u32::MAX.to_string()));
                    assert!(!problem.remediation().contains(&u32::MAX.to_string()));
                }
            }
        }

        for (content, diagnostic) in [
            (
                "[subagents]\nallow_nested = \"true\"\n",
                "subagents.allow_nested",
            ),
            ("subagents = true\n", "field `subagents`"),
            ("[subagents]\nunknown = true\n", "table `subagents`"),
            ("unknown = true\n", "unknown field"),
        ] {
            write_settings(home.path(), content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_default_subagents(resolution.subagents());
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{content}"
            );
            assert!(resolution.problems()[0].diagnostic().contains(diagnostic));
            assert!(resolution.problems()[0].remediation().len() <= 256);
        }
    }

    #[test]
    fn out_of_range_toml_integers_are_malformed_settings() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "[subagents]\nmax_concurrent = 9223372036854775808\n",
        );

        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_default_subagents(resolution.subagents());
        assert_eq!(
            resolution.problems()[0].kind(),
            crate::config::StartupProblemKind::UserSettingsMalformed
        );
    }

    #[test]
    fn only_reads_the_exact_workspace_settings_path_and_accepts_absent_files() {
        let home = tempdir().expect("home directory");
        let parent = tempdir().expect("parent directory");
        let project = parent.path().join("project");
        fs::create_dir(&project).expect("project directory");
        write_settings(home.path(), "model = \"openai/gpt-5.6-luna\"\n");
        write_settings(parent.path(), "model = \"openai/gpt-5.6-terra\"\n");

        let resolution = resolve(input(Some(home.path()), Some(&project)));

        let expected_user_path = home.path().join(".kiki/config.toml");
        let expected_project_path = project.join(".kiki/config.toml");
        assert_eq!(resolution.model().id, LUNA.id);
        assert!(resolution.problems().is_empty());
        assert_eq!(
            resolution.context().user_path(),
            Some(expected_user_path.as_path())
        );
        assert_eq!(
            resolution.context().project_path(),
            Some(expected_project_path.as_path())
        );
    }

    #[test]
    fn project_thinking_entries_override_matching_user_entries_only() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "[thinking]\n\"openai/gpt-5.6-sol\" = \"low\"\n\"openai/gpt-5.6-luna\" = \"high\"\n",
        );
        write_settings(
            project.path(),
            "[thinking]\n\"openai/gpt-5.6-sol\" = \"max\"\n",
        );

        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_eq!(resolution.thinking(), "max");
        assert_eq!(resolution.context().thinking_for(&SOL), Some("max"));
        assert_eq!(resolution.context().thinking_for(&LUNA), Some("high"));
    }

    #[test]
    fn qualified_environment_selection_overrides_files_without_becoming_persistent() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "model = \"openai/gpt-5.6-sol\"\n\n[thinking]\n\"openai/gpt-5.6-terra\" = \"low\"\n",
        );
        let mut input = input(Some(home.path()), Some(project.path()));
        input.model = Some(OsString::from(TERRA.id));
        input.thinking = Some(OsString::from("high"));

        let resolution = resolve(input);

        assert_eq!(resolution.model().id, TERRA.id);
        assert_eq!(resolution.thinking(), "high");
        assert_eq!(
            resolution.context().model_origin(),
            super::SettingsOrigin::Environment
        );
        assert_eq!(resolution.context().thinking_for(&TERRA), Some("low"));
        assert!(resolution.problems().is_empty());
    }

    #[test]
    fn invalid_model_environment_skips_thinking_validation() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(project.path(), "model = \"openai/gpt-5.6-terra\"\n");
        let mut input = input(Some(home.path()), Some(project.path()));
        input.model = Some(OsString::from("not-a-model"));
        input.thinking = Some(OsString::from(""));

        let resolution = resolve(input);

        assert_eq!(resolution.model().id, TERRA.id);
        assert_eq!(
            resolution
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [crate::config::StartupProblemKind::ModelUnknown]
        );
    }

    #[test]
    fn thinking_environment_problems_name_the_variable_model_and_levels() {
        let project = tempdir().expect("project directory");
        for (value, kind) in [
            (
                OsString::from(""),
                crate::config::StartupProblemKind::ThinkingEmpty,
            ),
            (
                OsString::from("not-a-level"),
                crate::config::StartupProblemKind::ThinkingUnknown,
            ),
        ] {
            let mut input = input(None, Some(project.path()));
            input.thinking = Some(value);
            let resolution = resolve(input);
            let problem = resolution.problems().first().expect("thinking problem");

            assert_eq!(problem.kind(), kind);
            assert!(problem.diagnostic().contains("KIKI_THINKING"));
            assert!(problem.diagnostic().contains(SOL.id));
            assert!(
                problem
                    .diagnostic()
                    .contains("low, medium, high, xhigh, max")
            );
            assert!(problem.remediation().contains("KIKI_THINKING"));
        }
    }

    #[test]
    fn known_level_unsupported_by_a_model_is_reported_as_unsupported() {
        let limited = crate::config::ModelConfiguration {
            thinking_levels: &["none"],
            default_thinking: "none",
            ..SOL
        };

        assert_eq!(
            super::environment_thinking_level(&limited, "high"),
            Err(crate::config::StartupProblemKind::ThinkingUnsupported)
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_home_is_skipped_and_non_unicode_thinking_is_constructive() {
        use std::os::unix::ffi::OsStringExt;

        let project = tempdir().expect("project directory");
        write_settings(project.path(), "model = \"openai/gpt-5.6-terra\"\n");
        let mut input = input(None, Some(project.path()));
        input.home = Some(OsString::from_vec(vec![0xff]));
        input.thinking = Some(OsString::from_vec(vec![0xff]));

        let resolution = resolve(input);

        assert_eq!(resolution.context().user_path(), None);
        assert!(matches!(
            resolution.context().save_model(&SOL),
            Err(super::SaveError::UserSettingsUnavailable)
        ));
        assert_eq!(resolution.model().id, TERRA.id);
        assert_eq!(
            resolution.problems()[0].kind(),
            crate::config::StartupProblemKind::ThinkingInvalidUnicode
        );
        assert!(resolution.problems()[0].diagnostic().contains(TERRA.id));
    }

    #[test]
    fn invalid_documents_contribute_no_values_and_keep_layer_problems() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "model = \"openai/gpt-5.6-terra\"\nunknown = \"secret-user-value\"\n",
        );
        write_settings(
            project.path(),
            "model = 7\n[thinking]\n\"openai/gpt-5.6-sol\" = \"secret-project-value\"\n",
        );

        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let diagnostics = resolution
            .problems()
            .iter()
            .map(|problem| problem.diagnostic())
            .collect::<Vec<_>>();

        assert_eq!(resolution.model().id, SOL.id);
        assert_eq!(
            resolution
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [
                crate::config::StartupProblemKind::UserSettingsInvalid,
                crate::config::StartupProblemKind::ProjectSettingsInvalid,
                crate::config::StartupProblemKind::ProjectSettingsInvalid,
            ]
        );
        assert!(
            diagnostics
                .iter()
                .all(|text| !text.contains("secret-user-value"))
        );
        assert!(
            diagnostics
                .iter()
                .all(|text| !text.contains("secret-project-value"))
        );
    }

    #[test]
    fn unknown_models_wrong_types_and_unsupported_values_reject_the_whole_document() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        for content in [
            "model = \"openai/unknown\"\n",
            "model = false\n",
            "thinking = \"high\"\n",
            "[thinking]\n\"openai/unknown\" = \"high\"\n",
            "[thinking]\n\"openai/gpt-5.6-sol\" = 7\n",
            "[thinking]\n\"openai/gpt-5.6-sol\" = \"unsupported\"\n",
        ] {
            write_settings(home.path(), content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(resolution.model().id, SOL.id, "{content}");
            assert_eq!(resolution.context().thinking_for(&SOL), None, "{content}");
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{content}"
            );
        }
    }

    #[test]
    fn malformed_invalid_utf8_and_unreadable_documents_are_source_specific() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(home.path(), "model = [\n");
        let project_path = project.path().join(".kiki/config.toml");
        fs::create_dir_all(&project_path).expect("directory at settings path");

        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_eq!(
            resolution
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [
                crate::config::StartupProblemKind::UserSettingsMalformed,
                crate::config::StartupProblemKind::ProjectSettingsUnreadable,
            ]
        );
        assert!(
            resolution.problems()[0]
                .diagnostic()
                .contains("~/.kiki/config.toml")
        );
        assert!(
            resolution.problems()[1]
                .diagnostic()
                .contains(".kiki/config.toml")
        );

        fs::remove_dir_all(project_path).expect("settings directory");
        fs::create_dir_all(project.path().join(".kiki")).expect("project settings parent");
        fs::write(project.path().join(".kiki/config.toml"), [0xff]).expect("invalid UTF-8");
        let resolution = resolve(input(None, Some(project.path())));
        assert_eq!(
            resolution.problems()[0].kind(),
            crate::config::StartupProblemKind::ProjectSettingsInvalidUtf8
        );
    }

    #[test]
    fn document_diagnostics_are_bounded_and_never_echo_untrusted_content() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let secret = "sk-settings-SECRET";
        let document = "[subagents]\n".to_owned()
            + &(0..10)
                .map(|index| format!("\"{secret}-{index}\" = true\n"))
                .collect::<String>();
        write_settings(home.path(), &document);
        write_settings(project.path(), &document);

        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_eq!(resolution.problems().len(), 6);
        for problem in resolution.problems() {
            assert!(!problem.diagnostic().contains(secret));
            assert!(problem.diagnostic().len() <= 256);
            assert!(problem.remediation().len() <= 256);
        }
    }

    #[test]
    fn saving_one_model_or_thinking_key_preserves_comments_and_subagent_settings() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "# keep this comment\nmodel = \"openai/gpt-5.6-sol\" # keep this too\nfuture = \"preserved\"\n\n[thinking]\n\"openai/gpt-5.6-luna\" = \"low\" # untouched\n\"openai/gpt-5.6-sol\" = \"medium\" # updated\n\n[subagents] # keep this table\nallow_nested = false # untouched\nmax_concurrent = 3\nmax_starts_per_run = 12\nmax_depth = 2\ntimeout_seconds = 60\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let context = resolution.context();

        assert!(matches!(
            context.save_model(&TERRA),
            Ok(super::SaveOutcome::Saved)
        ));
        assert!(matches!(
            context.save_thinking(&SOL, "high"),
            Ok(super::SaveOutcome::Saved)
        ));

        let content = fs::read_to_string(context.user_path().expect("user path")).expect("saved");
        assert!(content.contains("# keep this comment"));
        assert!(content.contains("model = \"openai/gpt-5.6-terra\" # keep this too"));
        assert!(content.contains("future = \"preserved\""));
        assert!(content.contains("\"openai/gpt-5.6-luna\" = \"low\" # untouched"));
        assert!(content.contains("\"openai/gpt-5.6-sol\" = \"high\" # updated"));
        assert!(content.contains("[subagents] # keep this table"));
        assert!(content.contains("allow_nested = false # untouched"));
        assert!(content.contains("max_concurrent = 3"));
        assert!(content.contains("max_starts_per_run = 12"));
        assert!(content.contains("max_depth = 2"));
        assert!(content.contains("timeout_seconds = 60"));
    }

    #[test]
    fn saving_a_model_keeps_it_at_the_document_root() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "[thinking]\n\"openai/gpt-5.6-sol\" = \"low\"\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        resolution.context().save_model(&TERRA).expect("save model");

        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(resolution.model().id, TERRA.id);
        assert_eq!(resolution.context().thinking_for(&SOL), Some("low"));
        assert!(resolution.problems().is_empty());
    }

    #[test]
    fn saving_creates_the_user_directory_and_refuses_malformed_input() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let path = home.path().join(".kiki/config.toml");

        assert!(matches!(
            resolution.context().save_model(&TERRA),
            Ok(super::SaveOutcome::Saved)
        ));
        assert_eq!(
            fs::read_to_string(&path).expect("saved"),
            "model = \"openai/gpt-5.6-terra\"\n"
        );

        fs::write(&path, "model = [").expect("malformed settings");
        assert!(matches!(
            resolution.context().save_thinking(&SOL, "high"),
            Err(super::SaveError::InvalidExistingUserSettings)
        ));
        assert_eq!(fs::read_to_string(path).expect("unchanged"), "model = [");
    }

    #[test]
    fn saves_replace_complete_files_and_clean_up_temporary_files_after_failures() {
        let directory = tempdir().expect("settings directory");
        let path = directory.path().join("config.toml");
        fs::write(&path, "old = \"value\"\n").expect("old content");

        super::write_document(&path, "model = \"openai/gpt-5.6-sol\"\n").expect("replace");
        assert_eq!(
            fs::read_to_string(&path).expect("replacement"),
            "model = \"openai/gpt-5.6-sol\"\n"
        );

        let target = directory.path().join("target");
        fs::create_dir(&target).expect("target directory");
        assert!(matches!(
            super::write_document(&target, "replacement"),
            Err(super::SaveError::Io(_))
        ));
        assert_eq!(
            {
                let mut entries = fs::read_dir(directory.path())
                    .expect("directory")
                    .map(|entry| entry.expect("entry").file_name())
                    .collect::<Vec<_>>();
                entries.sort();
                entries
            },
            [OsString::from("config.toml"), OsString::from("target")]
        );
    }

    #[test]
    fn saving_reports_project_or_environment_masking() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            project.path(),
            "model = \"openai/gpt-5.6-terra\"\n\n[thinking]\n\"openai/gpt-5.6-sol\" = \"high\"\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        let project_save = resolution.context().save_model(&SOL).expect("project save");
        assert_eq!(
            project_save.to_string(),
            "User default saved; currently overridden by project settings."
        );
        assert!(matches!(
            project_save,
            super::SaveOutcome::SavedButMasked {
                by: super::SettingsOrigin::Project
            }
        ));
        assert!(matches!(
            resolution.context().save_thinking(&SOL, "low"),
            Ok(super::SaveOutcome::SavedButMasked {
                by: super::SettingsOrigin::Project
            })
        ));

        let mut environment = input(Some(home.path()), Some(project.path()));
        environment.model = Some(OsString::from(TERRA.id));
        environment.thinking = Some(OsString::from("max"));
        let resolution = resolve(environment);
        assert!(matches!(
            resolution.context().save_model(&SOL),
            Ok(super::SaveOutcome::SavedButMasked {
                by: super::SettingsOrigin::Environment
            })
        ));
        assert!(matches!(
            resolution.context().save_thinking(&TERRA, "low"),
            Ok(super::SaveOutcome::SavedButMasked {
                by: super::SettingsOrigin::Environment
            })
        ));
    }
}
