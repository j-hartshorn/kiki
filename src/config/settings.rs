use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use crate::theme::ThemeSetting;

use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, Table, Value, value};

use super::{
    DEFAULT_MODEL, ModelConfiguration, ModelSelection, StartupProblemKind,
    constants::{
        DEFAULT_SUBAGENT_ALLOW_NESTED, DEFAULT_SUBAGENT_MAX_CONCURRENT, DEFAULT_SUBAGENT_MAX_DEPTH,
        DEFAULT_SUBAGENT_MAX_STARTS_PER_RUN, DEFAULT_SUBAGENT_TIMEOUT, SUBAGENT_TIMER_HEADROOM,
    },
    model_by_id,
};

const MAX_DOCUMENT_PROBLEMS: usize = 3;
const DEFAULT_COMPACTION_RESERVE_TOKENS: u64 = 16_384;
const DEFAULT_COMPACTION_KEEP_RECENT_TOKENS: u64 = 20_000;
const USER_SETTINGS_LABEL: &str = "user settings file `~/.kiki/config.toml`";
const PROJECT_SETTINGS_LABEL: &str = "project settings file `.kiki/config.toml`";
static MODEL_REMEDIATION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Set it to a supported qualified model id ({}) or remove it, then restart Kiki.",
        supported_model_ids()
    )
});
static ENVIRONMENT_MODEL_REMEDIATION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Set KIKI_MODEL to a supported qualified model id ({}) or leave it unset, then restart Kiki.",
        supported_model_ids()
    )
});
static THINKING_KEY_REMEDIATION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "Use only supported qualified model ids ({}) as [thinking] keys, then restart Kiki.",
        supported_model_ids()
    )
});

fn supported_model_ids() -> String {
    super::models::CATALOGUE.map(|model| model.id).join(", ")
}

pub(crate) struct Input {
    pub(crate) home: Option<OsString>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) session_preferences: Option<SessionPreferences>,
    pub(crate) model: Option<OsString>,
    pub(crate) thinking: Option<OsString>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionPreferences {
    selection: ModelSelection,
    thinking: BTreeMap<&'static str, &'static str>,
}

impl SessionPreferences {
    pub(crate) fn initial(selection: ModelSelection) -> Self {
        Self {
            selection,
            thinking: BTreeMap::from([(selection.model().id, selection.thinking())]),
        }
    }

    pub(crate) fn new(
        selection: ModelSelection,
        thinking: BTreeMap<&'static str, &'static str>,
    ) -> Option<Self> {
        (thinking.get(selection.model().id) == Some(&selection.thinking())
            && thinking.iter().all(|(model_id, thinking)| {
                model_by_id(model_id).is_some_and(|model| model.supports_thinking(thinking))
            }))
        .then_some(Self {
            selection,
            thinking,
        })
    }

    pub(crate) fn from_ids(
        selected_model: &str,
        thinking: impl IntoIterator<Item = (String, String)>,
    ) -> Option<Self> {
        let selected_model = model_by_id(selected_model)?;
        let mut accepted = BTreeMap::new();
        for (model_id, thinking) in thinking {
            let model = model_by_id(&model_id)?;
            let thinking = model
                .thinking_levels
                .iter()
                .copied()
                .find(|candidate| *candidate == thinking)?;
            accepted.insert(model.id, thinking);
        }
        let thinking = *accepted.get(selected_model.id)?;
        Self::new(ModelSelection::new(selected_model, thinking)?, accepted)
    }

    pub(crate) fn selection(&self) -> ModelSelection {
        self.selection
    }

    pub(crate) fn thinking(&self) -> &BTreeMap<&'static str, &'static str> {
        &self.thinking
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsOrigin {
    BuiltIn,
    User,
    Project,
    Session,
    Environment,
}

impl fmt::Display for SettingsOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BuiltIn => "built-in defaults",
            Self::User => "user settings",
            Self::Project => "project settings",
            Self::Session => "session preferences",
            Self::Environment => "environment variables",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CompactionSettings {
    enabled: bool,
    reserve_tokens: u64,
    keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self::new(
            DEFAULT_COMPACTION_RESERVE_TOKENS,
            DEFAULT_COMPACTION_KEEP_RECENT_TOKENS,
        )
    }
}

impl CompactionSettings {
    pub(crate) fn new(reserve_tokens: u64, keep_recent_tokens: u64) -> Self {
        Self {
            enabled: true,
            reserve_tokens,
            keep_recent_tokens,
        }
    }

    pub(crate) fn enabled(self) -> bool {
        self.enabled
    }

    #[cfg(test)]
    pub(crate) fn disabled(self) -> Self {
        Self {
            enabled: false,
            ..self
        }
    }

    pub(crate) fn reserve_tokens(self) -> u64 {
        self.reserve_tokens
    }

    pub(crate) fn keep_recent_tokens(self) -> u64 {
        self.keep_recent_tokens
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CacheTtl {
    #[default]
    FiveMinutes,
    OneHour,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CrossProviderReasoning {
    #[default]
    Inline,
    Drop,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SettingsContext {
    user_path: Option<PathBuf>,
    project_path: Option<PathBuf>,
    model_origin: SettingsOrigin,
    favourites: Option<Vec<&'static ModelConfiguration>>,
    thinking: BTreeMap<&'static str, PersistedThinking>,
    environment_thinking_model: Option<&'static str>,
    compaction: CompactionSettings,
    anthropic_cache_ttl: CacheTtl,
    cross_provider_reasoning: CrossProviderReasoning,
    theme: ThemeSetting,
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
            .field("favourites", &self.favourites)
            .field("thinking", &self.thinking)
            .field(
                "environment_thinking_model",
                &self.environment_thinking_model,
            )
            .field("compaction", &self.compaction)
            .field("anthropic_cache_ttl", &self.anthropic_cache_ttl)
            .field("cross_provider_reasoning", &self.cross_provider_reasoning)
            .field("theme", &self.theme)
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

    pub(crate) fn favourites(&self) -> Option<&[&'static ModelConfiguration]> {
        self.favourites.as_deref()
    }

    pub(crate) fn thinking_for(&self, model: &'static ModelConfiguration) -> Option<&'static str> {
        self.thinking.get(model.id).map(|selection| selection.value)
    }

    pub(crate) fn compaction(&self) -> CompactionSettings {
        self.compaction
    }

    pub(crate) fn anthropic_cache_ttl(&self) -> CacheTtl {
        self.anthropic_cache_ttl
    }

    pub(crate) fn theme(&self) -> ThemeSetting {
        self.theme
    }

    pub(crate) fn cross_provider_reasoning(&self) -> CrossProviderReasoning {
        self.cross_provider_reasoning
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
    let mut favourites = None;
    let mut subagents = SubagentSettings::default();
    let mut compaction = CompactionSettings::default();
    apply_layer(
        user.as_ref(),
        SettingsOrigin::User,
        &mut model,
        &mut model_origin,
        &mut thinking,
        &mut favourites,
        &mut subagents,
    );
    apply_compaction_layer(user.as_ref(), &mut compaction);
    apply_layer(
        project.as_ref(),
        SettingsOrigin::Project,
        &mut model,
        &mut model_origin,
        &mut thinking,
        &mut favourites,
        &mut subagents,
    );
    apply_compaction_layer(project.as_ref(), &mut compaction);
    let anthropic_cache_ttl = [&project, &user]
        .into_iter()
        .find_map(|layer| layer.as_ref()?.anthropic_cache_ttl)
        .unwrap_or_default();
    let cross_provider_reasoning = [&project, &user]
        .into_iter()
        .find_map(|layer| layer.as_ref()?.cross_provider_reasoning)
        .unwrap_or_default();
    let theme = [&project, &user]
        .into_iter()
        .find_map(|layer| layer.as_ref()?.theme)
        .unwrap_or_default();
    if let Some(session_preferences) = input.session_preferences {
        model = session_preferences.selection().model();
        model_origin = SettingsOrigin::Session;
        thinking.extend(
            session_preferences
                .thinking()
                .iter()
                .map(|(model, thinking)| {
                    (
                        *model,
                        PersistedThinking {
                            value: thinking,
                            origin: SettingsOrigin::Session,
                        },
                    )
                }),
        );
    }

    let model_is_valid = match input.model {
        None => true,
        Some(model_input) => match model_input.into_string() {
            Err(_) => {
                problems.push(environment_problem(
                    StartupProblemKind::ModelInvalidUnicode,
                    "KIKI_MODEL is not valid Unicode.",
                    &ENVIRONMENT_MODEL_REMEDIATION,
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
                        &ENVIRONMENT_MODEL_REMEDIATION,
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
            favourites,
            thinking,
            environment_thinking_model,
            compaction,
            anthropic_cache_ttl,
            cross_provider_reasoning,
            theme,
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
    favourites: &mut Option<Vec<&'static ModelConfiguration>>,
    subagents: &mut SubagentSettings,
) {
    let Some(settings) = settings else {
        return;
    };
    if let Some(selected) = settings.model {
        *model = selected;
        *model_origin = origin;
    }
    if let Some(selected) = &settings.favourites {
        *favourites = Some(selected.clone());
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

fn apply_compaction_layer(settings: Option<&FileSettings>, compaction: &mut CompactionSettings) {
    let Some(settings) = settings else {
        return;
    };
    if let Some(enabled) = settings.compaction.enabled {
        compaction.enabled = enabled;
    }
    if let Some(reserve_tokens) = settings.compaction.reserve_tokens {
        compaction.reserve_tokens = reserve_tokens;
    }
    if let Some(keep_recent_tokens) = settings.compaction.keep_recent_tokens {
        compaction.keep_recent_tokens = keep_recent_tokens;
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
                    &MODEL_REMEDIATION,
                ),
                None => problems.push(
                    "field `model` must be a string containing a supported qualified model id",
                    &MODEL_REMEDIATION,
                ),
            },
            "favourites" => validate_favourites(item, &mut settings.favourites, &mut problems),
            "thinking" => validate_thinking_table(item, &mut settings.thinking, &mut problems),
            "subagents" => validate_subagents_table(item, &mut settings.subagents, &mut problems),
            "compaction" => validate_compaction_table(item, &mut settings.compaction, &mut problems),
            "anthropic" => {
                validate_anthropic_table(item, &mut settings.anthropic_cache_ttl, &mut problems);
            }
            "theme" => match item.as_str().and_then(ThemeSetting::parse) {
                Some(theme) => settings.theme = Some(theme),
                None => problems.push(
                    format!(
                        "field `theme` must be \"{}\", \"{}\" or \"{}\"",
                        ThemeSetting::Dark.as_str(),
                        ThemeSetting::Light.as_str(),
                        ThemeSetting::Automatic.as_str(),
                    ),
                    "Set it to \"dark\", \"light\" or \"automatic\", then restart Kiki.",
                ),
            },
            "cross_provider_reasoning" => match item.as_str() {
                Some("inline") => {
                    settings.cross_provider_reasoning = Some(CrossProviderReasoning::Inline);
                }
                Some("drop") => settings.cross_provider_reasoning = Some(CrossProviderReasoning::Drop),
                _ => problems.push(
                    "field `cross_provider_reasoning` must be \"inline\" or \"drop\"",
                    "Set it to \"inline\" or \"drop\", then restart Kiki.",
                ),
            },
            _ => problems.push(
                "contains an unknown field; only `model`, `favourites`, `cross_provider_reasoning`, `theme`, `[thinking]`, `[subagents]`, `[compaction]`, and `[anthropic]` are allowed",
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

fn validate_favourites(
    item: &Item,
    favourites: &mut Option<Vec<&'static ModelConfiguration>>,
    problems: &mut DocumentProblems,
) {
    let Some(array) = item.as_array() else {
        problems.push(
            "field `favourites` must be a non-empty array of supported qualified model ids",
            "Set favourites to a non-empty array of unique supported model ids, then restart Kiki.",
        );
        return;
    };
    let mut models = Vec::new();
    for value in array.iter() {
        let Some(model) = value.as_str().and_then(model_by_id) else {
            problems.push(
                "field `favourites` contains an invalid model id",
                "Use only supported qualified model ids in favourites, then restart Kiki.",
            );
            return;
        };
        if models.contains(&model) {
            problems.push(
                "field `favourites` contains a duplicate model id",
                "List each favourite model only once, then restart Kiki.",
            );
            return;
        }
        models.push(model);
    }
    if models.is_empty() {
        problems.push(
            "field `favourites` must not be empty",
            "List at least one favourite model, then restart Kiki.",
        );
    } else {
        *favourites = Some(models);
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
                "field `thinking` contains an unknown model key",
                &THINKING_KEY_REMEDIATION,
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

fn validate_compaction_table(
    item: &Item,
    compaction: &mut FileCompactionSettings,
    problems: &mut DocumentProblems,
) {
    let Some(table) = item.as_table() else {
        problems.push(
            "field `compaction` must be a TOML table",
            "Use [compaction] with supported settings, then restart Kiki.",
        );
        return;
    };

    for (key, item) in table.iter() {
        match key {
            "enabled" => match item.as_bool() {
                Some(value) => compaction.enabled = Some(value),
                None => problems.push(
                    "field `compaction.enabled` must be a boolean",
                    "Set it to true or false, then restart Kiki.",
                ),
            },
            "reserve_tokens" => {
                compaction.reserve_tokens = positive_compaction_tokens(item, key, problems);
            }
            "keep_recent_tokens" => {
                compaction.keep_recent_tokens = positive_compaction_tokens(item, key, problems);
            }
            _ => problems.push(
                "table `compaction` contains an unknown key; only `enabled`, `reserve_tokens`, and `keep_recent_tokens` are allowed",
                "Remove the unknown key from table `compaction`, then restart Kiki.",
            ),
        }
    }
}

fn validate_anthropic_table(
    item: &Item,
    cache_ttl: &mut Option<CacheTtl>,
    problems: &mut DocumentProblems,
) {
    let Some(table) = item.as_table() else {
        problems.push(
            "field `anthropic` must be a TOML table",
            "Use [anthropic] with supported settings, then restart Kiki.",
        );
        return;
    };

    for (key, item) in table.iter() {
        match key {
            "cache_ttl" => match item.as_str() {
                Some("5m") => *cache_ttl = Some(CacheTtl::FiveMinutes),
                Some("1h") => *cache_ttl = Some(CacheTtl::OneHour),
                _ => problems.push(
                    "field `anthropic.cache_ttl` must be \"5m\" or \"1h\"",
                    "Set it to \"5m\" or \"1h\", then restart Kiki.",
                ),
            },
            _ => problems.push(
                "table `anthropic` contains an unknown key; only `cache_ttl` is allowed",
                "Remove the unknown key from table `anthropic`, then restart Kiki.",
            ),
        }
    }
}

fn positive_compaction_tokens(
    item: &Item,
    field: &str,
    problems: &mut DocumentProblems,
) -> Option<u64> {
    match item
        .as_integer()
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
    {
        Some(value) => Some(value),
        None => {
            problems.push(
                format!("field `compaction.{field}` must be a positive integer"),
                "Set it to a positive integer, then restart Kiki.",
            );
            None
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
    favourites: Option<Vec<&'static ModelConfiguration>>,
    thinking: BTreeMap<&'static str, &'static str>,
    subagents: FileSubagentSettings,
    compaction: FileCompactionSettings,
    anthropic_cache_ttl: Option<CacheTtl>,
    cross_provider_reasoning: Option<CrossProviderReasoning>,
    theme: Option<ThemeSetting>,
}

#[derive(Default)]
struct FileSubagentSettings {
    allow_nested: Option<bool>,
    max_concurrent: Option<usize>,
    max_starts_per_run: Option<usize>,
    max_depth: Option<usize>,
    timeout: Option<Duration>,
}

#[derive(Default)]
struct FileCompactionSettings {
    enabled: Option<bool>,
    reserve_tokens: Option<u64>,
    keep_recent_tokens: Option<u64>,
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
    super::create_private_dir_all(parent).map_err(SaveError::Io)?;
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
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use crate::config::{
        ModelSelection,
        constants::{SUBAGENT_TIMER_HEADROOM, latest_representable_instant},
        models::{ASTRA, CATALOGUE, LUNA, SOL, TERRA},
    };

    use super::{
        CacheTtl, CrossProviderReasoning, Input, SessionPreferences, SubagentSettings, resolve,
    };

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
    fn compaction_defaults_and_project_fields_override_user_fields() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        let defaults = resolve(input(Some(home.path()), Some(project.path())));
        let default_policy = defaults.context().compaction();
        assert_eq!(default_policy.reserve_tokens(), 16_384);
        assert_eq!(default_policy.keep_recent_tokens(), 20_000);
        assert!(defaults.problems().is_empty());

        write_settings(
            home.path(),
            "[compaction]\nreserve_tokens = 1024\nkeep_recent_tokens = 2048\n",
        );
        write_settings(project.path(), "[compaction]\nreserve_tokens = 4096\n");
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let policy = resolution.context().compaction();

        assert_eq!(policy.reserve_tokens(), 4096);
        assert_eq!(policy.keep_recent_tokens(), 2048);
        assert!(resolution.problems().is_empty());
    }

    #[test]
    fn compaction_enabled_defaults_true_and_project_overrides_user() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let phase_2 = "[compaction]\nreserve_tokens = 1024\nkeep_recent_tokens = 2048\n";
        let enabled = "[compaction]\nenabled = true\n";
        let disabled = "[compaction]\nenabled = false\n";

        for (user_content, project_content, expected) in [
            ("", "", true),
            (phase_2, "", true),
            ("", phase_2, true),
            (disabled, "", false),
            ("", disabled, false),
            (disabled, "[compaction]\nreserve_tokens = 4096\n", false),
            (disabled, enabled, true),
            (enabled, disabled, false),
        ] {
            write_settings(home.path(), user_content);
            write_settings(project.path(), project_content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert!(
                resolution.problems().is_empty(),
                "{user_content:?} / {project_content:?}"
            );
            assert_eq!(
                resolution.context().compaction().enabled(),
                expected,
                "{user_content:?} / {project_content:?}"
            );
        }

        write_settings(
            home.path(),
            "[compaction]\nenabled = false\nreserve_tokens = 1024\nkeep_recent_tokens = 2048\n",
        );
        write_settings(
            project.path(),
            "[compaction]\nenabled = true\nreserve_tokens = 4096\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        let policy = resolution.context().compaction();
        assert!(resolution.problems().is_empty());
        assert!(policy.enabled());
        assert_eq!(policy.reserve_tokens(), 4096);
        assert_eq!(policy.keep_recent_tokens(), 2048);
    }

    #[test]
    fn compaction_enabled_requires_a_boolean_and_valid_neighbours() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(project.path(), "[compaction]\nkeep_recent_tokens = 2048\n");

        for content in [
            "[compaction]\nenabled = \"false\"\n",
            "[compaction]\nenabled = 0\n",
            "[compaction]\nenabled = [false]\n",
            "[compaction]\nenabled = false\nreserve_tokens = 0\n",
            "[compaction]\nenabled = false\nother = true\n",
        ] {
            write_settings(home.path(), content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));
            let policy = resolution.context().compaction();

            assert!(policy.enabled(), "{content:?}");
            assert_eq!(policy.reserve_tokens(), 16_384, "{content:?}");
            assert_eq!(policy.keep_recent_tokens(), 2048, "{content:?}");
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{content:?}"
            );
        }

        write_settings(home.path(), "[compaction]\nenabled = \"false\"\n");
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        assert!(
            resolution.problems()[0]
                .diagnostic()
                .contains("field `compaction.enabled` must be a boolean")
        );
    }

    #[test]
    fn compaction_settings_require_a_table_of_positive_integers() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        for field in ["reserve_tokens", "keep_recent_tokens"] {
            for value in ["\"8\"", "false", "1.5", "[8]", "0", "-1"] {
                write_settings(home.path(), &format!("[compaction]\n{field} = {value}\n"));
                let resolution = resolve(input(Some(home.path()), Some(project.path())));
                let policy = resolution.context().compaction();

                assert_eq!(policy.reserve_tokens(), 16_384, "{field} = {value}");
                assert_eq!(policy.keep_recent_tokens(), 20_000, "{field} = {value}");
                assert_eq!(
                    resolution.problems()[0].kind(),
                    crate::config::StartupProblemKind::UserSettingsInvalid,
                    "{field} = {value}"
                );
                assert!(
                    resolution.problems()[0]
                        .diagnostic()
                        .contains(&format!("compaction.{field}")),
                    "{field} = {value}"
                );
            }
        }

        for content in [
            "compaction = true\n",
            "compaction = {}\n",
            "[[compaction]]\n",
        ] {
            write_settings(home.path(), content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(
                resolution.context().compaction(),
                super::CompactionSettings::default()
            );
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{content}"
            );
            assert!(
                resolution.problems()[0]
                    .diagnostic()
                    .contains("field `compaction`")
            );
        }
    }

    #[test]
    fn invalid_compaction_layers_are_discarded_and_overflow_is_malformed() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(project.path(), "[compaction]\nkeep_recent_tokens = 2048\n");
        for unknown in ["enable", "other"] {
            write_settings(
                home.path(),
                &format!(
                    "model = \"openai/gpt-5.6-terra\"\n\n[compaction]\nreserve_tokens = 1024\n{unknown} = true\n"
                ),
            );
            let resolution = resolve(input(Some(home.path()), Some(project.path())));
            let policy = resolution.context().compaction();

            assert_eq!(resolution.model().id, SOL.id, "{unknown}");
            assert_eq!(policy.reserve_tokens(), 16_384, "{unknown}");
            assert_eq!(policy.keep_recent_tokens(), 2048, "{unknown}");
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{unknown}"
            );
            assert!(
                resolution.problems()[0]
                    .diagnostic()
                    .contains("unknown key")
            );
        }

        for content in [
            "[compaction]\nreserve_tokens = 9223372036854775808\n",
            "[compaction\nreserve_tokens = 1024\n",
        ] {
            write_settings(home.path(), content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(resolution.context().compaction().reserve_tokens(), 16_384);
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsMalformed,
                "{content}"
            );
        }

        write_settings(
            home.path(),
            "[compaction]\nreserve_tokens = 9223372036854775807\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            resolution.context().compaction().reserve_tokens(),
            9_223_372_036_854_775_807
        );
        assert!(resolution.problems().is_empty());
    }

    #[test]
    fn anthropic_cache_ttl_defaults_to_five_minutes_and_project_overrides_user() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        let defaults = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            defaults.context().anthropic_cache_ttl(),
            CacheTtl::FiveMinutes
        );
        assert!(defaults.problems().is_empty());

        write_settings(home.path(), "[anthropic]\ncache_ttl = \"1h\"\n");
        for (project_content, expected) in [
            ("", CacheTtl::OneHour),
            ("[anthropic]\n", CacheTtl::OneHour),
            ("[anthropic]\ncache_ttl = \"5m\"\n", CacheTtl::FiveMinutes),
        ] {
            write_settings(project.path(), project_content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(
                resolution.context().anthropic_cache_ttl(),
                expected,
                "{project_content}"
            );
            assert!(resolution.problems().is_empty(), "{project_content}");
        }

        write_settings(home.path(), "");
        write_settings(project.path(), "[anthropic]\ncache_ttl = \"1h\"\n");
        let project_only = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            project_only.context().anthropic_cache_ttl(),
            CacheTtl::OneHour
        );
    }

    #[test]
    fn invalid_anthropic_settings_discard_their_whole_layer() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        for (content, diagnostic) in [
            (
                "[anthropic]\ncache_ttl = \"secret-ttl\"\n",
                "anthropic.cache_ttl",
            ),
            ("[anthropic]\ncache_ttl = \"1H\"\n", "anthropic.cache_ttl"),
            ("[anthropic]\ncache_ttl = 60\n", "anthropic.cache_ttl"),
            ("[anthropic]\ncache_ttl = [\"1h\"]\n", "anthropic.cache_ttl"),
            ("anthropic = \"1h\"\n", "field `anthropic`"),
            ("[[anthropic]]\ncache_ttl = \"1h\"\n", "field `anthropic`"),
            ("[anthropic]\nunknown = true\n", "table `anthropic`"),
            (
                "[anthropic]\ncache_ttl = \"1h\"\nttl = \"1h\"\n",
                "table `anthropic`",
            ),
        ] {
            write_settings(
                home.path(),
                &format!("model = \"openai/gpt-5.6-terra\"\n{content}"),
            );
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(resolution.model().id, SOL.id, "{content}");
            assert_eq!(
                resolution.context().anthropic_cache_ttl(),
                CacheTtl::FiveMinutes,
                "{content}"
            );
            let [problem] = resolution.problems() else {
                panic!("expected one problem for {content}");
            };
            assert_eq!(
                problem.kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{content}"
            );
            assert!(problem.diagnostic().contains(diagnostic), "{content}");
            assert!(!problem.diagnostic().contains("secret-ttl"));
        }

        write_settings(home.path(), "[anthropic]\ncache_ttl = \"1h\"\n");
        write_settings(
            project.path(),
            "model = \"openai/gpt-5.6-terra\"\n[anthropic]\ncache_ttl = \"5m\"\nunknown = true\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_eq!(resolution.model().id, SOL.id);
        assert_eq!(
            resolution.context().anthropic_cache_ttl(),
            CacheTtl::OneHour
        );
        assert_eq!(
            resolution
                .problems()
                .iter()
                .map(|problem| problem.kind())
                .collect::<Vec<_>>(),
            [crate::config::StartupProblemKind::ProjectSettingsInvalid]
        );
    }

    #[test]
    fn theme_defaults_to_automatic_and_project_overrides_user() {
        use crate::theme::ThemeSetting;
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let defaults = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(defaults.context().theme(), ThemeSetting::Automatic);
        assert!(defaults.problems().is_empty());

        write_settings(home.path(), "theme = \"light\"\n");
        let user = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(user.context().theme(), ThemeSetting::Light);
        assert!(user.problems().is_empty());

        write_settings(project.path(), "theme = \"dark\"\n");
        let overridden = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(overridden.context().theme(), ThemeSetting::Dark);
        assert!(overridden.problems().is_empty());

        write_settings(project.path(), "theme = \"automatic\"\n");
        let explicit_automatic = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            explicit_automatic.context().theme(),
            ThemeSetting::Automatic
        );
        assert!(explicit_automatic.problems().is_empty());
    }

    #[test]
    fn invalid_project_theme_reports_one_problem_and_preserves_user_theme() {
        use crate::theme::ThemeSetting;
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(home.path(), "theme = \"light\"\n");
        write_settings(
            project.path(),
            "model = \"openai/gpt-5.6-terra\"\ntheme = \"blue\"\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(resolution.context().theme(), ThemeSetting::Light);
        assert_eq!(resolution.model().id, SOL.id);
        let [problem] = resolution.problems() else {
            panic!("expected one project problem");
        };
        assert_eq!(
            problem.kind(),
            crate::config::StartupProblemKind::ProjectSettingsInvalid
        );
        for choice in ["dark", "light", "automatic"] {
            assert!(
                problem.diagnostic().contains(choice),
                "{}",
                problem.diagnostic()
            );
        }
    }

    #[test]
    fn theme_values_are_validated_with_supported_choices() {
        let home = tempdir().expect("home directory");
        for value in ["\"light\"", "\"dark\"", "\"automatic\""] {
            write_settings(home.path(), &format!("theme = {value}\n"));
            let resolution = resolve(input(Some(home.path()), None));
            assert!(resolution.problems().is_empty(), "{value}");
        }
        for value in ["\"blue\"", "1"] {
            write_settings(home.path(), &format!("theme = {value}\n"));
            let resolution = resolve(input(Some(home.path()), None));
            let [problem] = resolution.problems() else {
                panic!("expected one problem for {value}");
            };
            assert_eq!(
                problem.kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid
            );
            for choice in ["dark", "light", "automatic"] {
                assert!(
                    problem.diagnostic().contains(choice),
                    "{}",
                    problem.diagnostic()
                );
            }
        }
    }

    #[test]
    fn cross_provider_reasoning_defaults_to_inline_and_project_overrides_user() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        let defaults = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            defaults.context().cross_provider_reasoning(),
            CrossProviderReasoning::Inline
        );
        assert!(defaults.problems().is_empty());

        write_settings(home.path(), "cross_provider_reasoning = \"drop\"\n");
        for (project_content, expected) in [
            ("", CrossProviderReasoning::Drop),
            (
                "cross_provider_reasoning = \"inline\"\n",
                CrossProviderReasoning::Inline,
            ),
        ] {
            write_settings(project.path(), project_content);
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(
                resolution.context().cross_provider_reasoning(),
                expected,
                "{project_content}"
            );
            assert!(resolution.problems().is_empty(), "{project_content}");
        }

        write_settings(home.path(), "");
        write_settings(project.path(), "cross_provider_reasoning = \"drop\"\n");
        let project_only = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            project_only.context().cross_provider_reasoning(),
            CrossProviderReasoning::Drop
        );
    }

    #[test]
    fn invalid_cross_provider_reasoning_discards_its_whole_layer() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");

        for value in [
            "\"secret-mode\"",
            "\"Drop\"",
            "\"\"",
            "true",
            "[\"drop\"]",
            "{ mode = \"drop\" }",
        ] {
            write_settings(
                home.path(),
                &format!("model = \"openai/gpt-5.6-terra\"\ncross_provider_reasoning = {value}\n"),
            );
            let resolution = resolve(input(Some(home.path()), Some(project.path())));

            assert_eq!(resolution.model().id, SOL.id, "{value}");
            assert_eq!(
                resolution.context().cross_provider_reasoning(),
                CrossProviderReasoning::Inline,
                "{value}"
            );
            let [problem] = resolution.problems() else {
                panic!("expected one problem for {value}");
            };
            assert_eq!(
                problem.kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid,
                "{value}"
            );
            assert!(
                problem
                    .diagnostic()
                    .contains("field `cross_provider_reasoning`"),
                "{value}"
            );
            assert!(!problem.diagnostic().contains("secret-mode"));
        }

        write_settings(home.path(), "cross_provider_reasoning = \"drop\"\n");
        write_settings(
            project.path(),
            "model = \"openai/gpt-5.6-terra\"\ncross_provider_reasoning = \"inline\"\nunknown = true\n",
        );
        let resolution = resolve(input(Some(home.path()), Some(project.path())));

        assert_eq!(resolution.model().id, SOL.id);
        assert_eq!(
            resolution.context().cross_provider_reasoning(),
            CrossProviderReasoning::Drop
        );
        let [problem] = resolution.problems() else {
            panic!("expected one project problem");
        };
        assert_eq!(
            problem.kind(),
            crate::config::StartupProblemKind::ProjectSettingsInvalid
        );
        assert!(
            problem.diagnostic().contains("`cross_provider_reasoning`"),
            "{}",
            problem.diagnostic()
        );
    }

    #[test]
    fn new_models_are_selectable_in_settings_and_favourites() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        for id in ["openai/gpt-6.1-sol", "anthropic/claude-sonnet-5-5"] {
            write_settings(
                project.path(),
                &format!(
                    "model = \"{id}\"\nfavourites = [\"{id}\"]\n[thinking]\n\"{id}\" = \"max\"\n"
                ),
            );
            let resolution = resolve(input(Some(home.path()), Some(project.path())));
            assert!(resolution.problems().is_empty());
            assert_eq!(resolution.model().id, id);
            assert_eq!(resolution.thinking(), "max");
            assert_eq!(resolution.context().favourites().unwrap()[0].id, id);
        }
    }

    #[test]
    fn ordered_favourites_override_as_one_setting_without_changing_foreground_model() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        write_settings(
            home.path(),
            "favourites = [\"openai/gpt-6-luna\", \"openai/gpt-6-astra\"]\n",
        );
        let user = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            user.context()
                .favourites()
                .unwrap()
                .iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
            ["openai/gpt-6-luna", "openai/gpt-6-astra"]
        );
        assert_eq!(user.model().id, SOL.id);
        write_settings(
            project.path(),
            "favourites = [\"openai/gpt-6-astra\", \"openai/gpt-6-sol\"]\n",
        );
        let project = resolve(input(Some(home.path()), Some(project.path())));
        assert_eq!(
            project
                .context()
                .favourites()
                .unwrap()
                .iter()
                .map(|model| model.id)
                .collect::<Vec<_>>(),
            ["openai/gpt-6-astra", "openai/gpt-6-sol"]
        );
        assert!(project.problems().is_empty());
    }

    #[test]
    fn invalid_favourites_reject_whole_layer_without_leaking_values() {
        for favourites in [
            "[]",
            "[\"openai/gpt-6-sol\", \"openai/gpt-6-sol\"]",
            "[\"openai/unknown\"]",
            "[7]",
            "\"openai/gpt-6-sol\"",
        ] {
            let home = tempdir().unwrap();
            write_settings(
                home.path(),
                &format!("model = \"openai/gpt-5.6-terra\"\nfavourites = {favourites}\n"),
            );
            let resolution = resolve(input(Some(home.path()), None));
            assert_eq!(resolution.model().id, SOL.id);
            assert!(resolution.context().favourites().is_none());
            assert_eq!(
                resolution.problems()[0].kind(),
                crate::config::StartupProblemKind::UserSettingsInvalid
            );
            assert!(resolution.problems()[0].diagnostic().contains("favourites"));
        }
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
            session_preferences: None,
            model: None,
            thinking: None,
        });

        assert_eq!(resolution.model().id, TERRA.id);
        assert_eq!(resolution.context().thinking_for(&LUNA), Some("high"));
        assert!(resolution.problems().is_empty());
        assert_eq!(resolution.thinking(), TERRA.default_thinking);
        assert_eq!(resolution.context().thinking_for(&SOL), None);
    }

    #[test]
    fn session_preferences_override_files_but_environment_overrides_session() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        write_settings(
            home.path(),
            "model = \"openai/gpt-5.6-sol\"\n\n[thinking]\n\"openai/gpt-5.6-terra\" = \"low\"\n",
        );
        write_settings(
            project.path(),
            "model = \"openai/gpt-5.6-luna\"\n\n[thinking]\n\"openai/gpt-5.6-terra\" = \"medium\"\n",
        );
        let preferences = SessionPreferences::new(
            ModelSelection::new(&TERRA, "high").expect("select Terra"),
            BTreeMap::from([(SOL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid session preferences");

        let mut session = input(Some(home.path()), Some(project.path()));
        session.session_preferences = Some(preferences.clone());
        let session = resolve(session);
        assert!(std::ptr::eq(session.model(), &TERRA));
        assert_eq!(session.thinking(), "high");
        assert_eq!(
            session.context().model_origin(),
            super::SettingsOrigin::Session
        );
        assert_eq!(session.context().thinking_for(&TERRA), Some("high"));

        let mut environment = input(Some(home.path()), Some(project.path()));
        environment.session_preferences = Some(preferences);
        environment.model = Some(OsString::from(SOL.id));
        let environment = resolve(environment);
        assert!(std::ptr::eq(environment.model(), &SOL));
        assert_eq!(environment.thinking(), "low");
        assert_eq!(
            environment.context().model_origin(),
            super::SettingsOrigin::Environment
        );

        let mut environment_thinking = input(Some(home.path()), Some(project.path()));
        environment_thinking.session_preferences = Some(SessionPreferences::initial(
            ModelSelection::new(&TERRA, "high").expect("select Terra"),
        ));
        environment_thinking.thinking = Some(OsString::from("medium"));
        let environment_thinking = resolve(environment_thinking);
        assert!(std::ptr::eq(environment_thinking.model(), &TERRA));
        assert_eq!(environment_thinking.thinking(), "medium");
    }

    fn input(home: Option<&Path>, workspace_root: Option<&Path>) -> Input {
        Input {
            home: home.map(|path| path.as_os_str().to_os_string()),
            workspace_root: workspace_root.map(Path::to_path_buf),
            session_preferences: None,
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
    fn unknown_model_remediations_list_every_catalogue_model_id() {
        let home = tempdir().expect("home directory");
        let project = tempdir().expect("project directory");
        let untrusted = "openai/typo-SECRET";
        write_settings(home.path(), &format!("model = \"{untrusted}\"\n"));
        let file_model = resolve(input(Some(home.path()), Some(project.path())));
        let mut environment = input(None, Some(project.path()));
        environment.model = Some(OsString::from(untrusted));
        let environment_model = resolve(environment);
        write_settings(
            home.path(),
            &format!("[thinking]\n\"{untrusted}\" = \"high\"\n"),
        );
        let thinking_key = resolve(input(Some(home.path()), Some(project.path())));

        for resolution in [file_model, environment_model, thinking_key] {
            let problem = &resolution.problems()[0];
            let remediation = problem.remediation();
            for model in CATALOGUE {
                assert!(
                    remediation.contains(model.id),
                    "{remediation} should list {}",
                    model.id
                );
            }
            assert!(
                !problem.diagnostic().contains("SECRET"),
                "{}",
                problem.diagnostic()
            );
            assert!(!remediation.contains("SECRET"), "{remediation}");
        }
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
    fn saving_reports_project_environment_or_session_masking() {
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

        let preferences = SessionPreferences::new(
            ModelSelection::new(&TERRA, "high").expect("select Terra"),
            BTreeMap::from([(SOL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid session preferences");
        let mut session = input(Some(home.path()), Some(project.path()));
        session.session_preferences = Some(preferences);
        let session = resolve(session);
        assert!(matches!(
            session.context().save_model(&SOL),
            Ok(super::SaveOutcome::SavedButMasked {
                by: super::SettingsOrigin::Session
            })
        ));
        assert!(matches!(
            session.context().save_thinking(&TERRA, "low"),
            Ok(super::SaveOutcome::SavedButMasked {
                by: super::SettingsOrigin::Session
            })
        ));
        let user =
            fs::read_to_string(home.path().join(".kiki/config.toml")).expect("saved user settings");
        assert!(user.contains("model = \"openai/gpt-5.6-sol\""));
        assert!(user.contains("\"openai/gpt-5.6-terra\" = \"low\""));
    }
}

#[cfg(all(test, unix))]
mod private_kiki_directory_tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn saving_user_settings_creates_a_private_kiki_directory() {
        let home = tempfile::tempdir().unwrap();
        super::write_document(&home.path().join(".kiki/config.toml"), "").unwrap();
        let mode = std::fs::metadata(home.path().join(".kiki"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
    }
}
