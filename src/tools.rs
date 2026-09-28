mod command;

#[cfg(test)]
pub(crate) use command::CommandTestGate;
pub(crate) use command::ToolOutputChunk;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::fmt;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::{Condvar, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File, Metadata};
use futures::{Stream, StreamExt, future::BoxFuture, stream, stream::FuturesUnordered};
use globset::{GlobBuilder, GlobMatcher};
use grep_matcher::LineTerminator;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkError, SinkMatch};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
#[cfg(unix)]
use rustix::fs::{AtFlags, FileType, Mode, OFlags, openat, statat};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use rustix::fs::{RenameFlags, fchmod, fstat, linkat, mkdirat, renameat, renameat_with, unlinkat};
#[cfg(unix)]
use rustix::io::Errno;
use thiserror::Error;
use tokio::sync::mpsc;
#[cfg(test)]
use tokio::sync::{Notify, Semaphore};

use crate::config::constants::{
    COLLECTION_MAX_ITEMS, COLLECTION_MAX_PAYLOAD_BYTES, FILE_MAX_BYTES, TOOL_CONCURRENCY_LIMIT,
};
use crate::model::{ActiveSkills, SkillActivation, SkillActivationError};
use crate::skills::{
    SkillCatalog, SkillLookupError, model_visible_path, valid_name as valid_skill_name,
};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolValue {
    None,
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<Self>),
    Dictionary(BTreeMap<String, Self>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolName {
    Read,
    Write,
    Edit,
    ListDir,
    Glob,
    Grep,
    Run,
    LoadSkill,
    StartSubagent,
    GetSubagent,
    RefreshSubagent,
    WaitSubagent,
    CancelSubagent,
}

impl ToolName {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::ListDir => "list_dir",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::Run => "run",
            Self::LoadSkill => "load_skill",
            Self::StartSubagent => "start_subagent",
            Self::GetSubagent => "get_subagent",
            Self::RefreshSubagent => "refresh_subagent",
            Self::WaitSubagent => "wait_subagent",
            Self::CancelSubagent => "cancel_subagent",
        }
    }

    pub(crate) fn private_name(self) -> &'static str {
        match self {
            Self::Read => "_kiki_read",
            Self::Write => "_kiki_write",
            Self::Edit => "_kiki_edit",
            Self::ListDir => "_kiki_list_dir",
            Self::Glob => "_kiki_glob",
            Self::Grep => "_kiki_grep",
            Self::Run => "_kiki_run",
            Self::LoadSkill => "_kiki_load_skill",
            Self::StartSubagent => "_kiki_start_subagent",
            Self::GetSubagent => "_kiki_get_subagent",
            Self::RefreshSubagent => "_kiki_refresh_subagent",
            Self::WaitSubagent => "_kiki_wait_subagent",
            Self::CancelSubagent => "_kiki_cancel_subagent",
        }
    }

    pub(crate) fn from_private_name(name: &str) -> Option<Self> {
        ACTIVE_TOOLS
            .iter()
            .find(|definition| definition.name.private_name() == name)
            .map(|definition| definition.name)
    }

    pub(crate) fn label(self) -> &'static str {
        self.name()
    }

    fn is_allowed_read_only(self) -> bool {
        matches!(
            self,
            Self::Read
                | Self::ListDir
                | Self::Glob
                | Self::Grep
                | Self::LoadSkill
                | Self::StartSubagent
                | Self::GetSubagent
                | Self::RefreshSubagent
                | Self::WaitSubagent
                | Self::CancelSubagent
        )
    }

    pub(crate) fn count_label(self, count: usize) -> &'static str {
        match (self, count) {
            (Self::Read, 1) => "byte",
            (Self::Read, _) => "bytes",
            (Self::ListDir | Self::Glob, 1) => "entry",
            (Self::ListDir | Self::Glob, _) => "entries",
            (Self::Grep, 1) => "match",
            (Self::Grep, _) => "matches",
            (Self::LoadSkill, 1) => "skill",
            (Self::LoadSkill, _) => "skills",
            (Self::Write | Self::Edit, _) => {
                unreachable!("mutations have status summaries rather than counts")
            }
            (Self::Run, _) => {
                unreachable!("commands have exit or signal summaries rather than counts")
            }
            (
                Self::StartSubagent
                | Self::GetSubagent
                | Self::RefreshSubagent
                | Self::WaitSubagent
                | Self::CancelSubagent,
                _,
            ) => {
                unreachable!("subagent calls have identifier summaries rather than counts")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCall {
    pub(crate) call_id: u32,
    pub(crate) tool: ToolName,
    pub(crate) arguments: ToolValue,
}

impl ToolCall {
    pub(crate) fn mutation_path(&self) -> Option<PathBuf> {
        match self.tool {
            ToolName::Write => write_mutation_path(&self.arguments),
            ToolName::Edit => edit_mutation_path(&self.arguments),
            ToolName::Read
            | ToolName::ListDir
            | ToolName::Glob
            | ToolName::Grep
            | ToolName::Run
            | ToolName::LoadSkill
            | ToolName::StartSubagent
            | ToolName::GetSubagent
            | ToolName::RefreshSubagent
            | ToolName::WaitSubagent
            | ToolName::CancelSubagent => None,
        }
    }
}

#[derive(Clone, PartialEq)]
pub(crate) struct ToolResult {
    pub(crate) call_id: u32,
    pub(crate) result: Result<ToolValue, ToolError>,
    effect: Option<ToolEffect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MutationStatus {
    Created,
    Replaced,
    Applied,
}

impl ToolResult {
    #[cfg(test)]
    pub(crate) fn without_effect(result: ToolValue) -> Self {
        Self::from_result(0, Ok(result))
    }

    pub(crate) fn from_result(call_id: u32, result: Result<ToolValue, ToolError>) -> Self {
        Self {
            call_id,
            result,
            effect: None,
        }
    }

    pub(crate) fn apply_batch_transition(
        &self,
        authorisations: &mut WholeFileAuthorisations,
        mutation_paths: &BTreeSet<PathBuf>,
    ) {
        if self.result.is_ok()
            && let Some(effect) = &self.effect
            && !effect.is_suppressed_read(mutation_paths)
        {
            effect.apply(authorisations);
        }
    }

    fn apply_mutation_transition(&self, authorisations: &mut WholeFileAuthorisations) {
        if self.result.is_ok()
            && let Some(effect) = &self.effect
            && effect.is_mutation()
        {
            effect.apply(authorisations);
        }
    }

    pub(crate) fn mutation_status(&self) -> Option<MutationStatus> {
        self.result
            .is_ok()
            .then(|| self.effect.as_ref().and_then(ToolEffect::mutation_status))
            .flatten()
    }

    pub(crate) fn record_observed_file_operation(
        &self,
        read_paths: &mut BTreeSet<PathBuf>,
        modified_paths: &mut BTreeSet<PathBuf>,
    ) {
        if !self.result.is_ok() {
            return;
        }
        match self.effect.as_ref() {
            Some(ToolEffect::Read { path, .. }) => {
                read_paths.insert(path.clone());
            }
            Some(ToolEffect::Write { path, .. } | ToolEffect::Edit { path, .. }) => {
                modified_paths.insert(path.clone());
            }
            None => {}
        }
    }
}

impl fmt::Debug for ToolResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolResult")
            .field("call_id", &self.call_id)
            .field("result", &self.result)
            .field("effect", &self.effect)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolErrorKind {
    Type,
    FileNotFound,
    IsADirectory,
    NotADirectory,
    Permission,
    Value,
    Runtime,
    Os,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub(crate) struct ToolError {
    pub(crate) kind: ToolErrorKind,
    message: String,
}

impl ToolError {
    fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub(crate) fn type_error(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Type, message)
    }

    pub(crate) fn permission(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Permission, message)
    }

    pub(crate) fn value(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Value, message)
    }

    pub(crate) fn runtime(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Runtime, message)
    }

    fn from_io(error: std::io::Error) -> Self {
        let kind = match error.kind() {
            std::io::ErrorKind::NotFound => ToolErrorKind::FileNotFound,
            std::io::ErrorKind::NotADirectory => ToolErrorKind::NotADirectory,
            std::io::ErrorKind::PermissionDenied => ToolErrorKind::Permission,
            _ => ToolErrorKind::Os,
        };
        let message = match kind {
            ToolErrorKind::FileNotFound => "workspace path was not found".to_owned(),
            ToolErrorKind::NotADirectory => "path is not a directory".to_owned(),
            ToolErrorKind::Permission => "workspace access was denied".to_owned(),
            ToolErrorKind::Os => "filesystem operation failed".to_owned(),
            ToolErrorKind::Type
            | ToolErrorKind::IsADirectory
            | ToolErrorKind::Value
            | ToolErrorKind::Runtime => {
                unreachable!("I/O errors map only to filesystem exception categories")
            }
        };
        Self::new(kind, message)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolParameter {
    name: &'static str,
    value_type: ToolValueType,
    default: Option<ToolDefault>,
}

#[derive(Clone, Copy)]
enum ToolDefault {
    String(&'static str),
    None,
}

impl ToolDefault {
    fn source(self) -> String {
        match self {
            Self::String(value) => format!("{value:?}"),
            Self::None => "None".to_owned(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ToolValueType {
    String,
    Integer,
    OptionalInteger,
    OptionalString,
}

impl ToolValueType {
    fn annotation(self) -> &'static str {
        match self {
            Self::String => "str",
            Self::Integer => "int",
            Self::OptionalInteger => "int | None",
            Self::OptionalString => "str | None",
        }
    }
}

#[derive(Clone, Copy)]
struct ToolRecordField {
    name: &'static str,
    value_type: ToolValueType,
}

#[derive(Clone, Copy)]
pub(crate) struct ToolRecord {
    name: &'static str,
    fields: &'static [ToolRecordField],
}

#[derive(Clone, Copy)]
pub(crate) enum ToolResultShape {
    None,
    String,
    Record(&'static ToolRecord),
    RecordList(&'static ToolRecord),
}

impl ToolResultShape {
    fn annotation(self) -> String {
        match self {
            Self::None => "None".to_owned(),
            Self::String => "str".to_owned(),
            Self::Record(record) => record.name.to_owned(),
            Self::RecordList(record) => format!("list[{}]", record.name),
        }
    }

    fn record(self) -> Option<&'static ToolRecord> {
        match self {
            Self::None | Self::String => None,
            Self::Record(record) | Self::RecordList(record) => Some(record),
        }
    }

    fn bootstrap_body(self, private_name: &str, arguments: &str) -> String {
        match self {
            Self::None => format!("await {private_name}({arguments})"),
            Self::String => format!("return await {private_name}({arguments})"),
            Self::Record(record) => {
                format!(
                    "return {}(**await {private_name}({arguments}))",
                    record.name
                )
            }
            Self::RecordList(record) => {
                format!(
                    "return [{}(**row) for row in await {private_name}({arguments})]",
                    record.name
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ToolDefinition {
    name: ToolName,
    parameters: &'static [ToolParameter],
    result: ToolResultShape,
    description: &'static str,
}

const READ_PARAMETERS: &[ToolParameter] = &[ToolParameter {
    name: "path",
    value_type: ToolValueType::String,
    default: None,
}];

const WRITE_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "path",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "content",
        value_type: ToolValueType::String,
        default: None,
    },
];

const EDIT_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "path",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "old",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "new",
        value_type: ToolValueType::String,
        default: None,
    },
];

const DIRECTORY_ENTRY_FIELDS: &[ToolRecordField] = &[
    ToolRecordField {
        name: "path",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "kind",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "size_bytes",
        value_type: ToolValueType::OptionalInteger,
    },
];

const DIRECTORY_ENTRY: ToolRecord = ToolRecord {
    name: "DirectoryEntry",
    fields: DIRECTORY_ENTRY_FIELDS,
};

const GREP_MATCH_FIELDS: &[ToolRecordField] = &[
    ToolRecordField {
        name: "path",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "line",
        value_type: ToolValueType::Integer,
    },
    ToolRecordField {
        name: "text",
        value_type: ToolValueType::String,
    },
];

const GREP_MATCH: ToolRecord = ToolRecord {
    name: "GrepMatch",
    fields: GREP_MATCH_FIELDS,
};

const COMMAND_RESULT_FIELDS: &[ToolRecordField] = &[
    ToolRecordField {
        name: "exit_code",
        value_type: ToolValueType::OptionalInteger,
    },
    ToolRecordField {
        name: "stdout",
        value_type: ToolValueType::String,
    },
    ToolRecordField {
        name: "stderr",
        value_type: ToolValueType::String,
    },
];

const COMMAND_RESULT: ToolRecord = ToolRecord {
    name: "CommandResult",
    fields: COMMAND_RESULT_FIELDS,
};

const LIST_DIR_PARAMETERS: &[ToolParameter] = &[ToolParameter {
    name: "path",
    value_type: ToolValueType::String,
    default: Some(ToolDefault::String(".")),
}];

const GLOB_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "pattern",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "path",
        value_type: ToolValueType::String,
        default: Some(ToolDefault::String(".")),
    },
];

const GREP_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "pattern",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "path",
        value_type: ToolValueType::String,
        default: Some(ToolDefault::String(".")),
    },
    ToolParameter {
        name: "file_glob",
        value_type: ToolValueType::OptionalString,
        default: Some(ToolDefault::None),
    },
];

const RUN_PARAMETERS: &[ToolParameter] = &[
    ToolParameter {
        name: "command",
        value_type: ToolValueType::String,
        default: None,
    },
    ToolParameter {
        name: "cwd",
        value_type: ToolValueType::String,
        default: Some(ToolDefault::String(".")),
    },
];

const LOAD_SKILL_PARAMETERS: &[ToolParameter] = &[ToolParameter {
    name: "name",
    value_type: ToolValueType::String,
    default: None,
}];

pub(crate) const ACTIVE_TOOLS: &[ToolDefinition] = &[
    ToolDefinition {
        name: ToolName::Read,
        parameters: READ_PARAMETERS,
        result: ToolResultShape::String,
        description: "Read one complete UTF-8 workspace file or skill:// resource.",
    },
    ToolDefinition {
        name: ToolName::Write,
        parameters: WRITE_PARAMETERS,
        result: ToolResultShape::None,
        description: "Create or replace one complete UTF-8 workspace file.",
    },
    ToolDefinition {
        name: ToolName::Edit,
        parameters: EDIT_PARAMETERS,
        result: ToolResultShape::None,
        description: "Replace one exact occurrence in a UTF-8 workspace file.",
    },
    ToolDefinition {
        name: ToolName::ListDir,
        parameters: LIST_DIR_PARAMETERS,
        result: ToolResultShape::RecordList(&DIRECTORY_ENTRY),
        description: "List immediate workspace or skill:// directory entries.",
    },
    ToolDefinition {
        name: ToolName::Glob,
        parameters: GLOB_PARAMETERS,
        result: ToolResultShape::RecordList(&DIRECTORY_ENTRY),
        description: "Match workspace filesystem entries recursively.",
    },
    ToolDefinition {
        name: ToolName::Grep,
        parameters: GREP_PARAMETERS,
        result: ToolResultShape::RecordList(&GREP_MATCH),
        description: "Search workspace file contents with a regular expression.",
    },
    ToolDefinition {
        name: ToolName::Run,
        parameters: RUN_PARAMETERS,
        result: ToolResultShape::Record(&COMMAND_RESULT),
        description: "Run one project-local shell command.",
    },
    ToolDefinition {
        name: ToolName::LoadSkill,
        parameters: LOAD_SKILL_PARAMETERS,
        result: ToolResultShape::String,
        description: "Activate one model-invocable skill from the process catalog.",
    },
];

pub(crate) fn bootstrap_source() -> String {
    tool_source(false)
}

pub(crate) fn type_check_stubs() -> String {
    format!(
        "{}\n\nclass FileNotFoundError(OSError): ...\nclass IsADirectoryError(OSError): ...\nclass NotADirectoryError(OSError): ...\nclass PermissionError(OSError): ...\n\nresult_values: dict[str, object] = {{}}",
        type_reference_source(&ACTIVE_TOOLS.iter().collect::<Vec<_>>())
    )
}

pub(crate) fn prompt_reference(catalog: &SkillCatalog) -> String {
    prompt_reference_with_visibility(catalog.rendered_catalog().is_some(), false)
}

fn prompt_reference_with_visibility(load_skill_visible: bool, read_only: bool) -> String {
    prompt_reference_for(
        &ACTIVE_TOOLS
            .iter()
            .filter(|definition| {
                (load_skill_visible || definition.name != ToolName::LoadSkill)
                    && (!read_only || definition.name.is_allowed_read_only())
            })
            .collect::<Vec<_>>(),
        read_only,
    )
}

fn prompt_reference_for(definitions: &[&ToolDefinition], read_only: bool) -> String {
    let source = type_reference_source(definitions);
    let availability_guidance = if read_only {
        " This Arena is read-only; write, edit, and run are unavailable."
    } else {
        " Run project-local, one-shot commands."
    };
    format!(
        "## Kiki tool reference\n\nDo not redefine these host functions. Every host call must be awaited. Use `asyncio.gather()` only for independent calls expected to succeed. Keep dependent calls sequential. Avoid unbounded fan-out. A gather failure can follow completed host calls and does not assign sibling results. Keep large files and intermediate data in Python variables.{availability_guidance} Keep model-visible output focused. The Arena has five minutes of chargeable time. Time pauses only while waiting exclusively for child completion or cleanup; ordinary and control calls stay charged.\n\n```python\na, b = await asyncio.gather(read(\"a.rs\"), read(\"b.rs\"))\n```\n\n{source}\n"
    )
}

fn type_reference_source(definitions: &[&ToolDefinition]) -> String {
    let records = record_definitions(definitions)
        .into_iter()
        .map(ToolRecord::bootstrap_definition)
        .collect::<Vec<_>>();
    let definitions = definitions
        .iter()
        .map(|definition| definition.prompt_skeleton())
        .collect::<Vec<_>>();
    [
        Some("from dataclasses import dataclass".to_owned()),
        (!records.is_empty()).then(|| records.join("\n\n")),
        (!definitions.is_empty()).then(|| definitions.join("\n\n")),
        Some(invocation_source(true)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn tool_source(reference: bool) -> String {
    let definitions = ACTIVE_TOOLS.iter().collect::<Vec<_>>();
    let records = record_definitions(&definitions)
        .into_iter()
        .map(ToolRecord::bootstrap_definition)
        .collect::<Vec<_>>();
    let wrappers = ACTIVE_TOOLS
        .iter()
        .map(|definition| definition.bootstrap_wrapper())
        .collect::<Vec<_>>();
    [
        Some("from dataclasses import dataclass".to_owned()),
        (!records.is_empty()).then(|| records.join("\n\n")),
        (!wrappers.is_empty()).then(|| wrappers.join("\n\n")),
        Some(invocation_source(reference)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("\n\n")
}

const SUBAGENT_STATUS_FIELDS: &[(&str, &str)] = &[
    ("id", "str"),
    ("state", "str"),
    ("model", "str"),
    ("thinking", "str"),
    ("step_count", "int"),
    ("elapsed_ms", "int"),
    ("observed_at_ms", "int"),
    ("estimated_cost_usd", "float | None"),
    ("cost_status", "str"),
    ("error", "str | None"),
];

const SUBAGENT_TASK_FIELDS: &[(&str, &str)] = &[("id", "str"), ("status", "SubagentStatus")];

fn interface_fields_source(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(name, annotation)| format!("    {name}: {annotation}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn invocation_source(reference: bool) -> String {
    let refresh = if reference {
        "        ..."
    } else {
        "        self.status = SubagentStatus(**await _kiki_refresh_subagent(self.id))\n        return self.status"
    };
    let wait = if reference {
        "        ..."
    } else {
        "        outcome = await _kiki_wait_subagent(self.id)\n        self.status = SubagentStatus(**outcome['status'])\n        if outcome['outcome'] == 'completed':\n            return outcome['result']\n        if outcome['outcome'] == 'timed_out':\n            raise TimeoutError(outcome['error'])\n        raise RuntimeError(outcome['error'])"
    };
    let cancel = if reference {
        "        ..."
    } else {
        "        self.status = SubagentStatus(**await _kiki_cancel_subagent(self.id))\n        return self.status"
    };
    let constructor = (!reference).then_some(
        "\n\n    def __init__(self, value: dict):\n        self.id = value['id']\n        self.status = SubagentStatus(**value['status'])",
    );
    let start = if reference {
        "    ..."
    } else {
        "    return SubagentTask(await _kiki_start_subagent(prompt, model, read_only))"
    };
    let run = if reference {
        "    ..."
    } else {
        "    return await (await start_subagent(prompt, model, read_only)).wait()"
    };
    let get = if reference {
        "    ..."
    } else {
        "    return SubagentTask(await _kiki_get_subagent(id))"
    };
    format!(
        "@dataclass\nclass SubagentStatus:\n{}\n\nclass SubagentTask:\n{}{}\n\n    async def refresh(self) -> SubagentStatus:\n{refresh}\n\n    async def wait(self) -> dict:\n{wait}\n\n    async def cancel(self) -> SubagentStatus:\n{cancel}\n\nasync def start_subagent(prompt: str, model: str | None = None, read_only: bool = False) -> SubagentTask:\n    \"\"\"Start a subagent and return its task handle.\"\"\"\n{start}\n\nasync def run_subagent(prompt: str, model: str | None = None, read_only: bool = False) -> dict:\n    \"\"\"Start a subagent and wait for its result.\"\"\"\n{run}\n\nasync def get_subagent(id: str) -> SubagentTask:\n    \"\"\"Return an authorised subagent task handle by ID.\"\"\"\n{get}",
        interface_fields_source(SUBAGENT_STATUS_FIELDS),
        interface_fields_source(SUBAGENT_TASK_FIELDS),
        constructor.unwrap_or_default(),
    )
}

fn record_definitions<'a>(definitions: &'a [&'a ToolDefinition]) -> Vec<&'a ToolRecord> {
    let mut records: Vec<&ToolRecord> = Vec::new();
    for definition in definitions {
        let Some(record) = definition.result.record() else {
            continue;
        };
        if !records.iter().any(|defined| defined.name == record.name) {
            records.push(record);
        }
    }
    records
}

impl ToolRecord {
    fn bootstrap_definition(&self) -> String {
        format!("@dataclass\nclass {}:\n{}", self.name, self.fields_source())
    }

    fn fields_source(&self) -> String {
        self.fields
            .iter()
            .map(|field| format!("    {}: {}", field.name, field.value_type.annotation()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl ToolDefinition {
    fn signature(self) -> String {
        let parameters = self
            .parameters
            .iter()
            .map(|parameter| {
                let default = parameter
                    .default
                    .map(|default| format!(" = {}", default.source()))
                    .unwrap_or_default();
                format!(
                    "{}: {}{default}",
                    parameter.name,
                    parameter.value_type.annotation()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "async def {}({parameters}) -> {}:",
            self.name.name(),
            self.result.annotation()
        )
    }

    fn arguments(self) -> String {
        self.parameters
            .iter()
            .map(|parameter| parameter.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn bootstrap_wrapper(self) -> String {
        format!(
            "{}\n    \"\"\"{}\"\"\"\n    {}",
            self.signature(),
            self.description,
            self.result
                .bootstrap_body(self.name.private_name(), &self.arguments()),
        )
    }

    fn prompt_skeleton(self) -> String {
        format!(
            "{}\n    \"\"\"{}\"\"\"\n    ...",
            self.signature(),
            self.description,
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContentFingerprint([u8; 32]);

impl ContentFingerprint {
    pub(crate) fn session_bytes(self) -> [u8; 32] {
        self.0
    }

    pub(crate) fn from_session_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for ContentFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ContentFingerprint(..)")
    }
}

fn content_fingerprint(contents: &[u8]) -> ContentFingerprint {
    ContentFingerprint(*blake3::hash(contents).as_bytes())
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct WholeFileAuthorisations {
    fingerprints: BTreeMap<PathBuf, ContentFingerprint>,
}

impl fmt::Debug for WholeFileAuthorisations {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WholeFileAuthorisations(..)")
    }
}

#[derive(Clone, PartialEq, Eq)]
enum ToolEffect {
    Read {
        path: PathBuf,
        fingerprint: ContentFingerprint,
    },
    Write {
        path: PathBuf,
        outcome: WholeFileWriteOutcome,
        fingerprint: ContentFingerprint,
    },
    Edit {
        path: PathBuf,
        pre_fingerprint: ContentFingerprint,
        post_fingerprint: ContentFingerprint,
    },
}

impl ToolEffect {
    fn apply(&self, authorisations: &mut WholeFileAuthorisations) {
        match self {
            Self::Read { path, fingerprint }
            | Self::Write {
                path, fingerprint, ..
            } => {
                authorisations.authorise(path.clone(), *fingerprint);
            }
            Self::Edit {
                path,
                pre_fingerprint,
                post_fingerprint,
            } => {
                if authorisations.authorisation(path) == Some(*pre_fingerprint) {
                    authorisations.authorise(path.clone(), *post_fingerprint);
                } else {
                    authorisations.remove(path);
                }
            }
        }
    }

    fn is_mutation(&self) -> bool {
        matches!(self, Self::Write { .. } | Self::Edit { .. })
    }

    fn is_suppressed_read(&self, mutation_paths: &BTreeSet<PathBuf>) -> bool {
        matches!(self, Self::Read { path, .. } if mutation_paths.contains(path))
    }

    fn mutation_status(&self) -> Option<MutationStatus> {
        match self {
            Self::Write {
                outcome: WholeFileWriteOutcome::Created,
                ..
            } => Some(MutationStatus::Created),
            Self::Write {
                outcome: WholeFileWriteOutcome::Replaced,
                ..
            } => Some(MutationStatus::Replaced),
            Self::Edit { .. } => Some(MutationStatus::Applied),
            Self::Read { .. } => None,
        }
    }
}

impl fmt::Debug for ToolEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolEffect(..)")
    }
}

impl WholeFileAuthorisations {
    pub(crate) fn authorisation(&self, path: &Path) -> Option<ContentFingerprint> {
        self.fingerprints.get(path).copied()
    }

    pub(crate) fn authorise(&mut self, path: PathBuf, fingerprint: ContentFingerprint) {
        self.fingerprints.insert(path, fingerprint);
    }

    pub(crate) fn remove(&mut self, path: &Path) {
        self.fingerprints.remove(path);
    }

    pub(crate) fn session_entries(&self) -> impl Iterator<Item = (&PathBuf, &ContentFingerprint)> {
        self.fingerprints.iter()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WholeFileWriteOutcome {
    Created,
    Replaced,
}

struct WholeFileWrite {
    outcome: WholeFileWriteOutcome,
    fingerprint: ContentFingerprint,
}

struct WholeFileEdit {
    pre_fingerprint: ContentFingerprint,
    post_fingerprint: ContentFingerprint,
}

#[derive(Clone, Copy)]
struct ToolLimits {
    max_file_bytes: usize,
    max_collection_items: usize,
    max_collection_payload_bytes: usize,
}

struct ToolOutput {
    result: ToolValue,
    effect: Option<ToolEffect>,
}

impl ToolOutput {
    fn without_effect(result: ToolValue) -> Self {
        Self {
            result,
            effect: None,
        }
    }
}

struct ScheduledCall {
    result: ToolResult,
    mutation_path: Option<PathBuf>,
}

struct BatchDispatch<'a> {
    host: &'a ToolHost,
    queued: VecDeque<ToolCall>,
    active: FuturesUnordered<BoxFuture<'a, ScheduledCall>>,
    active_mutation_paths: BTreeSet<PathBuf>,
    authorisations: WholeFileAuthorisations,
    output: Option<mpsc::Sender<ToolOutputChunk>>,
    active_skills: &'a [SkillActivation],
}

pub(crate) struct SkillLoadLedger {
    active: ActiveSkills,
    captured: Vec<SkillActivation>,
}

impl SkillLoadLedger {
    pub(crate) fn new(active: &[SkillActivation]) -> Result<Self, SkillActivationError> {
        let mut skills = ActiveSkills::default();
        for activation in active {
            skills.activate(activation.clone())?;
        }
        Ok(Self {
            active: skills,
            captured: Vec::new(),
        })
    }

    #[cfg(test)]
    pub(crate) fn captured(&self) -> &[SkillActivation] {
        &self.captured
    }

    pub(crate) fn into_captured(self) -> Vec<SkillActivation> {
        self.captured
    }

    pub(crate) fn active(&self) -> &[SkillActivation] {
        self.active.as_slice()
    }

    fn activate(&mut self, activation: SkillActivation) -> Result<(), SkillActivationError> {
        if self.active.activate(activation.clone())? {
            self.captured
                .retain(|existing| existing.name() != activation.name());
            self.captured.push(activation);
        }
        Ok(())
    }
}

impl<'a> BatchDispatch<'a> {
    fn new(
        host: &'a ToolHost,
        calls: Vec<ToolCall>,
        authorisations: WholeFileAuthorisations,
        output: Option<mpsc::Sender<ToolOutputChunk>>,
        active_skills: &'a [SkillActivation],
    ) -> Self {
        Self {
            host,
            queued: calls.into(),
            active: FuturesUnordered::new(),
            active_mutation_paths: BTreeSet::new(),
            authorisations,
            output,
            active_skills,
        }
    }

    async fn next(mut self) -> Option<(ToolResult, Self)> {
        self.admit_calls();
        let ScheduledCall {
            result,
            mutation_path,
        } = self.active.next().await?;
        if let Some(path) = mutation_path {
            self.active_mutation_paths.remove(&path);
            result.apply_mutation_transition(&mut self.authorisations);
        }
        Some((result, self))
    }

    fn admit_calls(&mut self) {
        while self.active.len() < TOOL_CONCURRENCY_LIMIT {
            let Some(index) = self.queued.iter().position(|call| {
                call.mutation_path()
                    .is_none_or(|path| !self.active_mutation_paths.contains(&path))
            }) else {
                return;
            };
            let call = self
                .queued
                .remove(index)
                .expect("queued batch call index must remain valid");
            let mutation_path = call.mutation_path();
            let authorisation = mutation_path
                .as_ref()
                .and_then(|path| self.authorisations.authorisation(path));
            if let Some(path) = &mutation_path {
                assert!(
                    self.active_mutation_paths.insert(path.clone()),
                    "active same-path mutations must be serialised"
                );
            }
            let host = self.host;
            let output = self.output.clone();
            let active_skills = self.active_skills;
            self.active.push(Box::pin(async move {
                ScheduledCall {
                    result: host
                        .call_with_authorisation_and_output(
                            call,
                            authorisation,
                            output,
                            active_skills,
                        )
                        .await,
                    mutation_path,
                }
            }));
        }
    }
}

#[derive(Clone)]
pub(crate) struct ToolHost {
    workspace: Arc<Dir>,
    skill_catalog: Arc<SkillCatalog>,
    limits: ToolLimits,
    read_only: bool,
    #[cfg(test)]
    write_test_hook: Option<WriteTestHook>,
    #[cfg(test)]
    test_gate: Option<ReadTestGate>,
    #[cfg(test)]
    blocking_read_test_gate: Option<BlockingTestGate>,
    #[cfg(test)]
    list_dir_test_gate: Option<BlockingTestGate>,
    #[cfg(test)]
    batch_test_gate: Option<BatchTestGate>,
    #[cfg(test)]
    command_test_gate: Option<Arc<CommandTestGate>>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct ReadTestGate {
    read_started: Arc<Semaphore>,
    read_allowed: Arc<Semaphore>,
    read_settled: Arc<Semaphore>,
    driver_allowed: Arc<Semaphore>,
}

#[cfg(test)]
impl ReadTestGate {
    pub(crate) fn new() -> Self {
        Self {
            read_started: Arc::new(Semaphore::new(0)),
            read_allowed: Arc::new(Semaphore::new(0)),
            read_settled: Arc::new(Semaphore::new(0)),
            driver_allowed: Arc::new(Semaphore::new(0)),
        }
    }

    pub(crate) async fn wait_until_read_started(&self) {
        take_permit(&self.read_started).await;
    }

    pub(crate) fn resume_read(&self) {
        self.read_allowed.add_permits(1);
    }

    pub(crate) async fn wait_until_read_settled(&self) {
        take_permit(&self.read_settled).await;
    }

    pub(crate) fn resume_driver(&self) {
        self.driver_allowed.add_permits(1);
    }

    async fn wait_before_read(&self) {
        self.read_started.add_permits(1);
        take_permit(&self.read_allowed).await;
    }

    async fn wait_after_read_settled(&self) {
        self.read_settled.add_permits(1);
        take_permit(&self.driver_allowed).await;
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct BlockingTestGate {
    state: Arc<(Mutex<BlockingTestGateState>, Condvar)>,
}

#[cfg(test)]
struct BlockingTestGateState {
    started: bool,
    allowed: bool,
    finished: bool,
}

#[cfg(test)]
impl BlockingTestGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(BlockingTestGateState {
                    started: false,
                    allowed: false,
                    finished: false,
                }),
                Condvar::new(),
            )),
        }
    }

    pub(crate) async fn wait_until_started(&self) {
        self.wait_until(|state| state.started).await;
    }

    pub(crate) fn resume(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test gate mutex is not poisoned");
        state.allowed = true;
        changed.notify_all();
    }

    pub(crate) async fn wait_until_finished(&self) {
        self.wait_until(|state| state.finished).await;
    }

    fn wait_inside(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test gate mutex is not poisoned");
        state.started = true;
        changed.notify_all();
        while !state.allowed {
            state = changed
                .wait(state)
                .expect("test gate mutex is not poisoned");
        }
    }

    fn note_finished(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test gate mutex is not poisoned");
        state.finished = true;
        changed.notify_all();
    }

    async fn wait_until(
        &self,
        condition: impl Fn(&BlockingTestGateState) -> bool + Send + 'static,
    ) {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            let (lock, changed) = &*state;
            let mut state = lock.lock().expect("test gate mutex is not poisoned");
            while !condition(&state) {
                state = changed
                    .wait(state)
                    .expect("test gate mutex is not poisoned");
            }
        })
        .await
        .expect("test gate waiter completes");
    }
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct BatchTestGate {
    state: Arc<Mutex<BatchTestGateState>>,
    started: Arc<Semaphore>,
    driver_settled: Arc<Semaphore>,
    changed: Arc<Notify>,
}

#[cfg(test)]
struct BatchTestGateState {
    active_call_ids: BTreeSet<u32>,
    started_call_ids: Vec<u32>,
    released_call_ids: BTreeSet<u32>,
    pending_call_ids: Vec<u32>,
    driver_settled_call_ids: Vec<u32>,
    maximum_active: usize,
    resume_futures_calls: usize,
}

#[cfg(test)]
impl BatchTestGate {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(BatchTestGateState {
                active_call_ids: BTreeSet::new(),
                started_call_ids: Vec::new(),
                released_call_ids: BTreeSet::new(),
                pending_call_ids: Vec::new(),
                driver_settled_call_ids: Vec::new(),
                maximum_active: 0,
                resume_futures_calls: 0,
            })),
            started: Arc::new(Semaphore::new(0)),
            driver_settled: Arc::new(Semaphore::new(0)),
            changed: Arc::new(Notify::new()),
        }
    }

    pub(crate) async fn wait_until_calls_started(&self, count: usize) {
        for _ in 0..count {
            take_permit(&self.started).await;
        }
    }

    pub(crate) async fn wait_until_driver_settled(&self) {
        take_permit(&self.driver_settled).await;
    }

    pub(crate) fn driver_settled_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .driver_settled_call_ids
            .clone()
    }

    pub(crate) fn pending_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .pending_call_ids
            .clone()
    }

    pub(crate) fn started_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .started_call_ids
            .clone()
    }

    pub(crate) fn active_call_ids(&self) -> Vec<u32> {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .active_call_ids
            .iter()
            .copied()
            .collect()
    }

    pub(crate) fn maximum_active(&self) -> usize {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .maximum_active
    }

    pub(crate) fn resume_futures_calls(&self) -> usize {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .resume_futures_calls
    }

    pub(crate) fn release(&self, call_id: u32) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .released_call_ids
            .insert(call_id);
        self.changed.notify_waiters();
    }

    fn note_pending_call_ids(&self, call_ids: &[u32]) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .pending_call_ids = call_ids.to_vec();
    }

    fn note_driver_settled(&self, call_id: u32) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .driver_settled_call_ids
            .push(call_id);
        self.driver_settled.add_permits(1);
    }

    fn note_resume_futures(&self) {
        self.state
            .lock()
            .expect("test gate mutex is not poisoned")
            .resume_futures_calls += 1;
    }

    fn start_call(&self, call_id: u32) -> BatchTestCall {
        let mut state = self.state.lock().expect("test gate mutex is not poisoned");
        assert!(
            state.active_call_ids.insert(call_id),
            "test gate call IDs must be unique"
        );
        state.started_call_ids.push(call_id);
        state.maximum_active = state.maximum_active.max(state.active_call_ids.len());
        self.started.add_permits(1);
        BatchTestCall {
            gate: self.clone(),
            call_id,
        }
    }
}

#[cfg(test)]
struct BatchTestCall {
    gate: BatchTestGate,
    call_id: u32,
}

#[cfg(test)]
impl BatchTestCall {
    async fn wait_until_released(&self) {
        loop {
            let notified = self.gate.changed.notified();
            if self
                .gate
                .state
                .lock()
                .expect("test gate mutex is not poisoned")
                .released_call_ids
                .contains(&self.call_id)
            {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
impl Drop for BatchTestCall {
    fn drop(&mut self) {
        self.gate
            .state
            .lock()
            .expect("test gate mutex is not poisoned")
            .active_call_ids
            .remove(&self.call_id);
        self.gate.changed.notify_waiters();
    }
}

#[cfg(test)]
async fn take_permit(semaphore: &Arc<Semaphore>) {
    semaphore
        .clone()
        .acquire_owned()
        .await
        .expect("test gate semaphore remains open")
        .forget();
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum WritePause {
    InitialRead,
    BeforePublish,
    BeforeRecheck,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct WriteTestHook {
    state: Arc<(Mutex<WriteTestState>, Condvar)>,
}

#[cfg(test)]
struct WriteTestState {
    temporary_suffixes: VecDeque<[u8; 16]>,
    fail_entropy: bool,
    fail_write: bool,
    fail_publish: bool,
    force_hard_link_fallback: bool,
    pause: Option<WritePause>,
    paused: bool,
    resumed: bool,
}

#[cfg(test)]
impl WriteTestHook {
    fn new() -> Self {
        Self {
            state: Arc::new((
                Mutex::new(WriteTestState {
                    temporary_suffixes: VecDeque::new(),
                    fail_entropy: false,
                    fail_write: false,
                    fail_publish: false,
                    force_hard_link_fallback: false,
                    pause: None,
                    paused: false,
                    resumed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    fn with_temp_suffixes(suffixes: impl IntoIterator<Item = [u8; 16]>) -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .temporary_suffixes = suffixes.into_iter().collect();
        hook
    }

    fn fail_entropy() -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .fail_entropy = true;
        hook
    }

    fn fail_write() -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .fail_write = true;
        hook
    }

    fn fail_publish() -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .fail_publish = true;
        hook
    }

    fn force_hard_link_fallback() -> Self {
        let hook = Self::new();
        hook.set_hard_link_fallback();
        hook
    }

    fn set_fail_write(&self) {
        self.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .fail_write = true;
    }

    fn set_hard_link_fallback(&self) {
        self.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .force_hard_link_fallback = true;
    }

    fn pause_before_initial_read() -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .pause = Some(WritePause::InitialRead);
        hook
    }

    fn pause_before_publish() -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .pause = Some(WritePause::BeforePublish);
        hook
    }

    pub(crate) fn pause_before_recheck() -> Self {
        let hook = Self::new();
        hook.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .pause = Some(WritePause::BeforeRecheck);
        hook
    }

    pub(crate) async fn wait_until_paused(&self) {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            let (lock, changed) = &*state;
            let mut state = lock.lock().expect("test hook mutex is not poisoned");
            while !state.paused {
                state = changed
                    .wait(state)
                    .expect("test hook mutex is not poisoned");
            }
        })
        .await
        .expect("test hook waiter completes");
    }

    pub(crate) fn resume(&self) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test hook mutex is not poisoned");
        state.resumed = true;
        changed.notify_all();
    }

    fn temporary_name(suffix: [u8; 16]) -> PathBuf {
        temporary_name(suffix)
    }

    fn next_temporary_suffix(&self) -> Result<Option<[u8; 16]>, ToolError> {
        let mut state = self
            .state
            .0
            .lock()
            .expect("test hook mutex is not poisoned");
        if state.fail_entropy {
            return Err(ToolError::new(
                ToolErrorKind::Os,
                "temporary file creation failed",
            ));
        }
        Ok(state.temporary_suffixes.pop_front())
    }

    fn fail_writing(&self) -> bool {
        self.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .fail_write
    }

    fn fail_publishing(&self) -> bool {
        self.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .fail_publish
    }

    fn use_hard_link_fallback(&self) -> bool {
        self.state
            .0
            .lock()
            .expect("test hook mutex is not poisoned")
            .force_hard_link_fallback
    }

    fn pause(&self, pause: WritePause) {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().expect("test hook mutex is not poisoned");
        if state.pause != Some(pause) {
            return;
        }
        state.paused = true;
        changed.notify_all();
        while !state.resumed {
            state = changed
                .wait(state)
                .expect("test hook mutex is not poisoned");
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum ToolHostError {
    #[error("workspace operation failed")]
    Io(#[from] std::io::Error),
    #[error("blocking workspace operation failed")]
    Blocking(#[from] tokio::task::JoinError),
}

impl ToolHost {
    pub(crate) async fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolHostError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ToolLimits {
                max_file_bytes: FILE_MAX_BYTES,
                max_collection_items: COLLECTION_MAX_ITEMS,
                max_collection_payload_bytes: COLLECTION_MAX_PAYLOAD_BYTES,
            },
        )
        .await
    }

    #[cfg(test)]
    async fn with_limits(
        workspace: impl AsRef<Path>,
        max_bytes: usize,
    ) -> Result<Self, ToolHostError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ToolLimits {
                max_file_bytes: max_bytes,
                max_collection_items: COLLECTION_MAX_ITEMS,
                max_collection_payload_bytes: COLLECTION_MAX_PAYLOAD_BYTES,
            },
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn with_collection_limits(
        workspace: impl AsRef<Path>,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> Result<Self, ToolHostError> {
        Self::open(
            workspace.as_ref().to_path_buf(),
            ToolLimits {
                max_file_bytes: FILE_MAX_BYTES,
                max_collection_items: max_items,
                max_collection_payload_bytes: max_payload_bytes,
            },
        )
        .await
    }

    async fn open(workspace: PathBuf, limits: ToolLimits) -> Result<Self, ToolHostError> {
        let workspace = tokio::task::spawn_blocking(move || {
            Dir::open_ambient_dir(workspace, ambient_authority()).map(Arc::new)
        })
        .await??;

        Ok(Self {
            workspace,
            skill_catalog: Arc::default(),
            limits,
            read_only: false,
            #[cfg(test)]
            write_test_hook: None,
            #[cfg(test)]
            test_gate: None,
            #[cfg(test)]
            blocking_read_test_gate: None,
            #[cfg(test)]
            list_dir_test_gate: None,
            #[cfg(test)]
            batch_test_gate: None,
            #[cfg(test)]
            command_test_gate: None,
        })
    }

    pub(crate) fn for_child(&self, requested_read_only: bool) -> Self {
        let mut child = self.clone();
        child.read_only |= requested_read_only;
        child
    }

    pub(crate) fn with_skill_catalog(mut self, catalog: Arc<SkillCatalog>) -> Self {
        self.skill_catalog = catalog;
        self
    }

    pub(crate) fn load_skill(
        &self,
        arguments: ToolValue,
        ledger: &mut SkillLoadLedger,
    ) -> Result<ToolValue, ToolError> {
        let ToolValue::List(arguments) = arguments else {
            return Err(ToolError::type_error("load_skill expects one string name"));
        };
        let [ToolValue::String(name)] = arguments.as_slice() else {
            return Err(ToolError::type_error("load_skill expects one string name"));
        };
        let skill = self
            .skill_catalog
            .lookup_model_invocable(name)
            .map_err(|error| map_skill_lookup_error(name, error))?;
        ledger
            .activate(SkillActivation::from_skill(skill))
            .map_err(|error| ToolError::value(error.to_string()))?;
        let root = model_visible_path(skill.canonical_root()).map_or_else(
            || "root path unavailable".to_owned(),
            |path| {
                format!(
                    "root={}",
                    serde_json::to_string(path).expect("path is UTF-8")
                )
            },
        );
        Ok(ToolValue::String(format!(
            "Loaded skill {name}; resource skill://{name}/; {root}"
        )))
    }

    pub(crate) fn prompt_reference(&self) -> String {
        prompt_reference_with_visibility(
            self.skill_catalog.rendered_catalog().is_some(),
            self.read_only,
        )
    }

    pub(crate) fn skill_catalog(&self) -> &SkillCatalog {
        &self.skill_catalog
    }

    #[cfg(test)]
    pub(crate) fn with_write_test_hook(mut self, write_test_hook: WriteTestHook) -> Self {
        self.write_test_hook = Some(write_test_hook);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_gate(mut self, test_gate: ReadTestGate) -> Self {
        self.test_gate = Some(test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_blocking_read_test_gate(
        mut self,
        blocking_read_test_gate: BlockingTestGate,
    ) -> Self {
        self.blocking_read_test_gate = Some(blocking_read_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_list_dir_test_gate(mut self, list_dir_test_gate: BlockingTestGate) -> Self {
        self.list_dir_test_gate = Some(list_dir_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_batch_test_gate(mut self, batch_test_gate: BatchTestGate) -> Self {
        self.batch_test_gate = Some(batch_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_command_test_gate(
        mut self,
        command_test_gate: Arc<CommandTestGate>,
    ) -> Self {
        self.command_test_gate = Some(command_test_gate);
        self
    }

    #[cfg(test)]
    pub(crate) fn dispatch_batch(
        &self,
        calls: Vec<ToolCall>,
        authorisations: WholeFileAuthorisations,
    ) -> impl Stream<Item = ToolResult> + '_ {
        stream::unfold(
            BatchDispatch::new(self, calls, authorisations, None, &[]),
            |dispatch| async move { dispatch.next().await },
        )
    }

    pub(crate) fn dispatch_batch_with_output<'a>(
        &'a self,
        calls: Vec<ToolCall>,
        authorisations: WholeFileAuthorisations,
        output: mpsc::Sender<ToolOutputChunk>,
        active_skills: &'a [SkillActivation],
    ) -> impl Stream<Item = ToolResult> + 'a {
        stream::unfold(
            BatchDispatch::new(self, calls, authorisations, Some(output), active_skills),
            |dispatch| async move { dispatch.next().await },
        )
    }

    #[cfg(test)]
    async fn call(&self, call: ToolCall) -> ToolResult {
        self.call_with_authorisation(call, None).await
    }

    #[cfg(test)]
    async fn call_with_authorisation(
        &self,
        call: ToolCall,
        authorisation: Option<ContentFingerprint>,
    ) -> ToolResult {
        self.call_with_authorisation_and_output(call, authorisation, None, &[])
            .await
    }

    async fn call_with_authorisation_and_output(
        &self,
        call: ToolCall,
        authorisation: Option<ContentFingerprint>,
        output: Option<mpsc::Sender<ToolOutputChunk>>,
        active_skills: &[SkillActivation],
    ) -> ToolResult {
        let ToolCall {
            call_id,
            tool,
            arguments,
        } = call;
        if self.read_only && !tool.is_allowed_read_only() {
            return ToolResult {
                call_id,
                result: Err(ToolError::permission(
                    "tool is unavailable in a read-only arena",
                )),
                effect: None,
            };
        }
        #[cfg(test)]
        let batch_call = self
            .batch_test_gate
            .as_ref()
            .map(|gate| gate.start_call(call_id));
        #[cfg(test)]
        if let Some(batch_call) = &batch_call {
            batch_call.wait_until_released().await;
        }
        let result = match tool {
            ToolName::Read => self.read(arguments, active_skills).await,
            ToolName::Write => self.write(arguments, authorisation).await,
            ToolName::Edit => self.edit(arguments).await,
            ToolName::ListDir => self.list_dir(arguments, active_skills).await,
            ToolName::Glob => self.glob(arguments).await,
            ToolName::Grep => self.grep(arguments).await,
            ToolName::Run => self.run(call_id, arguments, output).await,
            ToolName::LoadSkill => Err(ToolError::runtime(
                "load_skill requires an active model execution",
            )),
            ToolName::StartSubagent
            | ToolName::GetSubagent
            | ToolName::RefreshSubagent
            | ToolName::WaitSubagent
            | ToolName::CancelSubagent => Err(ToolError::runtime(
                "subagent calls require an arena invocation context",
            )),
        };
        match result {
            Ok(ToolOutput { result, effect }) => ToolResult {
                call_id,
                result: Ok(result),
                effect,
            },
            Err(error) => ToolResult {
                call_id,
                result: Err(error),
                effect: None,
            },
        }
    }

    #[cfg(test)]
    async fn write_file(
        &self,
        path: &str,
        content: String,
        authorisation: Option<ContentFingerprint>,
    ) -> Result<WholeFileWrite, ToolError> {
        self.write_file_at(normalise_path(path)?, content, authorisation)
            .await
    }

    async fn write_file_at(
        &self,
        path: PathBuf,
        content: String,
        authorisation: Option<ContentFingerprint>,
    ) -> Result<WholeFileWrite, ToolError> {
        if content.len() > self.limits.max_file_bytes {
            return Err(ToolError::value(format!(
                "file exceeds the {}-byte limit",
                self.limits.max_file_bytes
            )));
        }
        let workspace = Arc::clone(&self.workspace);
        let max_file_bytes = self.limits.max_file_bytes;
        #[cfg(test)]
        let write_test_hook = self.write_test_hook.clone();
        tokio::task::spawn_blocking(move || {
            write_whole_file(
                &workspace,
                &path,
                &content,
                authorisation,
                max_file_bytes,
                #[cfg(test)]
                write_test_hook.as_ref(),
            )
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
    }

    #[cfg(test)]
    async fn edit_file(
        &self,
        path: &str,
        old: String,
        new: String,
    ) -> Result<WholeFileEdit, ToolError> {
        self.edit_file_at(normalise_path(path)?, old, new).await
    }

    async fn edit_file_at(
        &self,
        path: PathBuf,
        old: String,
        new: String,
    ) -> Result<WholeFileEdit, ToolError> {
        if old.is_empty() {
            return Err(ToolError::value("edit target cannot be empty"));
        }
        if old == new {
            return Err(ToolError::value(
                "edit replacement must differ from its target",
            ));
        }
        let workspace = Arc::clone(&self.workspace);
        let max_file_bytes = self.limits.max_file_bytes;
        #[cfg(test)]
        let write_test_hook = self.write_test_hook.clone();
        tokio::task::spawn_blocking(move || {
            edit_whole_file(
                &workspace,
                &path,
                &old,
                &new,
                max_file_bytes,
                #[cfg(test)]
                write_test_hook.as_ref(),
            )
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
    }

    async fn write(
        &self,
        arguments: ToolValue,
        authorisation: Option<ContentFingerprint>,
    ) -> Result<ToolOutput, ToolError> {
        let (path, content) = write_arguments(arguments)?;
        let effect_path = path.clone();
        let WholeFileWrite {
            outcome,
            fingerprint,
        } = self.write_file_at(path, content, authorisation).await?;
        Ok(ToolOutput {
            result: ToolValue::None,
            effect: Some(ToolEffect::Write {
                path: effect_path,
                outcome,
                fingerprint,
            }),
        })
    }

    async fn edit(&self, arguments: ToolValue) -> Result<ToolOutput, ToolError> {
        let (path, old, new) = edit_arguments(arguments)?;
        let effect_path = path.clone();
        let WholeFileEdit {
            pre_fingerprint,
            post_fingerprint,
        } = self.edit_file_at(path, old, new).await?;
        Ok(ToolOutput {
            result: ToolValue::None,
            effect: Some(ToolEffect::Edit {
                path: effect_path,
                pre_fingerprint,
                post_fingerprint,
            }),
        })
    }

    async fn read(
        &self,
        arguments: ToolValue,
        active_skills: &[SkillActivation],
    ) -> Result<ToolOutput, ToolError> {
        let target = read_path(arguments)?;
        #[cfg(test)]
        if let Some(test_gate) = &self.test_gate {
            test_gate.wait_before_read().await;
        }

        let ResolvedResource {
            directory,
            path,
            skill_name,
        } = self.resolve_resource_path(target, active_skills)?;
        let limits = self.limits;
        let effect_path = path.clone();
        #[cfg(test)]
        let blocking_read_test_gate = self.blocking_read_test_gate.clone();
        let read = tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(test_gate) = &blocking_read_test_gate {
                test_gate.wait_inside();
            }
            let read = read_file(&directory, &path, limits.max_file_bytes);
            #[cfg(test)]
            if let Some(test_gate) = &blocking_read_test_gate {
                test_gate.note_finished();
            }
            read
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))??;
        if skill_name.is_some() {
            Ok(ToolOutput::without_effect(ToolValue::String(read.contents)))
        } else {
            Ok(ToolOutput {
                result: ToolValue::String(read.contents),
                effect: Some(ToolEffect::Read {
                    path: effect_path,
                    fingerprint: read.fingerprint,
                }),
            })
        }
    }

    async fn list_dir(
        &self,
        arguments: ToolValue,
        active_skills: &[SkillActivation],
    ) -> Result<ToolOutput, ToolError> {
        let ResolvedResource {
            directory,
            path,
            skill_name,
        } = self.resolve_resource_path(list_dir_path(arguments)?, active_skills)?;
        let prefix = skill_name.map_or_else(String::new, |name| format!("skill://{name}"));
        let limits = self.limits;
        #[cfg(test)]
        let test_gate = self.list_dir_test_gate.clone();
        tokio::task::spawn_blocking(move || {
            #[cfg(test)]
            if let Some(test_gate) = test_gate {
                test_gate.wait_inside();
            }
            list_directory(&directory, &path, limits, &prefix)
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
        .map(ToolValue::List)
        .map(ToolOutput::without_effect)
    }

    fn resolve_resource_path(
        &self,
        target: ResourcePath,
        active_skills: &[SkillActivation],
    ) -> Result<ResolvedResource, ToolError> {
        match target {
            ResourcePath::Workspace(path) => Ok(ResolvedResource {
                directory: Arc::clone(&self.workspace),
                path,
                skill_name: None,
            }),
            ResourcePath::Skill { name, path } => {
                let skill = self
                    .skill_catalog
                    .lookup_model_invocable(&name)
                    .or_else(|error| {
                        self.skill_catalog
                            .lookup(&name)
                            .ok()
                            .filter(|skill| {
                                active_skills.iter().any(|active| {
                                    active.name() == name
                                        && active.canonical_root() == skill.canonical_root()
                                })
                            })
                            .ok_or(error)
                    })
                    .map_err(|error| map_skill_lookup_error(&name, error))?;
                Ok(ResolvedResource {
                    directory: skill.directory(),
                    path,
                    skill_name: Some(name),
                })
            }
        }
    }

    async fn glob(&self, arguments: ToolValue) -> Result<ToolOutput, ToolError> {
        let (pattern, path) = glob_arguments(arguments)?;
        let matcher = compile_glob(&pattern)?;
        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || glob_directory(&workspace, &path, matcher, limits))
            .await
            .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
            .map(ToolValue::List)
            .map(ToolOutput::without_effect)
    }

    async fn grep(&self, arguments: ToolValue) -> Result<ToolOutput, ToolError> {
        let (pattern, path, file_glob) = grep_arguments(arguments)?;
        let matcher = compile_regex(&pattern)?;
        let file_glob = match file_glob {
            Some(file_glob) => compile_glob(&file_glob)?,
            None => compile_glob("*")?,
        };
        let workspace = Arc::clone(&self.workspace);
        let limits = self.limits;
        tokio::task::spawn_blocking(move || {
            grep_workspace(&workspace, &path, matcher, file_glob, limits)
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))?
        .map(ToolValue::List)
        .map(ToolOutput::without_effect)
    }

    async fn run(
        &self,
        call_id: u32,
        arguments: ToolValue,
        output: Option<mpsc::Sender<ToolOutputChunk>>,
    ) -> Result<ToolOutput, ToolError> {
        let (command, cwd) = run_arguments(arguments)?;
        let workspace = Arc::clone(&self.workspace);
        let cwd = tokio::task::spawn_blocking(move || {
            open_directory_root(&workspace, &cwd)
                .map(|directory| OwnedFd::from(directory.into_std_file()))
        })
        .await
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "blocking filesystem operation failed"))??;
        #[cfg(test)]
        let result = command::execute_command_with_test_gate(
            &command,
            cwd,
            self.command_test_gate.clone(),
            call_id,
            output,
        )
        .await
        .map_err(map_command_error)?;
        #[cfg(not(test))]
        let result = command::execute_command_streaming(&command, cwd, call_id, output)
            .await
            .map_err(map_command_error)?;
        Ok(ToolOutput::without_effect(command_result_value(result)))
    }

    #[cfg(test)]
    pub(crate) fn note_batch_driver_pending_call_ids(&self, call_ids: &[u32]) {
        if let Some(batch_test_gate) = &self.batch_test_gate {
            batch_test_gate.note_pending_call_ids(call_ids);
        }
    }

    #[cfg(test)]
    pub(crate) fn note_batch_driver_settled(&self, call_id: u32) {
        if let Some(batch_test_gate) = &self.batch_test_gate {
            batch_test_gate.note_driver_settled(call_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn note_batch_driver_resume_futures(&self) {
        if let Some(batch_test_gate) = &self.batch_test_gate {
            batch_test_gate.note_resume_futures();
        }
    }

    #[cfg(test)]
    pub(crate) async fn wait_after_read_settled(&self) {
        if let Some(test_gate) = &self.test_gate {
            test_gate.wait_after_read_settled().await;
        }
    }
}

fn write_mutation_path(arguments: &ToolValue) -> Option<PathBuf> {
    let ToolValue::List(arguments) = arguments else {
        return None;
    };
    let [ToolValue::String(path), ToolValue::String(_)] = arguments.as_slice() else {
        return None;
    };
    workspace_mutation_path(path).ok()
}

fn edit_mutation_path(arguments: &ToolValue) -> Option<PathBuf> {
    let ToolValue::List(arguments) = arguments else {
        return None;
    };
    let [
        ToolValue::String(path),
        ToolValue::String(_),
        ToolValue::String(_),
    ] = arguments.as_slice()
    else {
        return None;
    };
    workspace_mutation_path(path).ok()
}

fn write_arguments(arguments: ToolValue) -> Result<(PathBuf, String), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "write() expects two positional string arguments",
        ));
    };
    match <[ToolValue; 2]>::try_from(arguments) {
        Ok([ToolValue::String(path), ToolValue::String(content)]) => {
            Ok((workspace_mutation_path(&path)?, content))
        }
        _ => Err(ToolError::type_error(
            "write() expects two positional string arguments",
        )),
    }
}

fn edit_arguments(arguments: ToolValue) -> Result<(PathBuf, String, String), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "edit() expects three positional string arguments",
        ));
    };
    match <[ToolValue; 3]>::try_from(arguments) {
        Ok(
            [
                ToolValue::String(path),
                ToolValue::String(old),
                ToolValue::String(new),
            ],
        ) => Ok((workspace_mutation_path(&path)?, old, new)),
        _ => Err(ToolError::type_error(
            "edit() expects three positional string arguments",
        )),
    }
}

fn run_arguments(arguments: ToolValue) -> Result<(String, PathBuf), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "run() expects one or two positional string arguments: string command, optional string cwd",
        ));
    };
    match arguments.as_slice() {
        [ToolValue::String(command)] => Ok((command.clone(), PathBuf::new())),
        [ToolValue::String(command), ToolValue::String(cwd)] => {
            Ok((command.clone(), normalise_path(cwd)?))
        }
        _ => Err(ToolError::type_error(
            "run() expects one or two positional string arguments: string command, optional string cwd",
        )),
    }
}

fn command_result_value(result: command::CommandResult) -> ToolValue {
    ToolValue::Dictionary(BTreeMap::from([
        (
            "exit_code".to_owned(),
            result
                .exit_code
                .map_or(ToolValue::None, |code| ToolValue::Integer(i64::from(code))),
        ),
        ("stdout".to_owned(), ToolValue::String(result.stdout)),
        ("stderr".to_owned(), ToolValue::String(result.stderr)),
    ]))
}

fn map_command_error(error: command::CommandError) -> ToolError {
    match error {
        command::CommandError::Overflow(message) => ToolError::runtime(message),
        command::CommandError::Launch | command::CommandError::Internal => {
            ToolError::new(ToolErrorKind::Os, "command execution failed")
        }
    }
}

fn map_skill_lookup_error(name: &str, error: SkillLookupError) -> ToolError {
    ToolError::value(match error {
        SkillLookupError::Unknown(_) => format!("unknown or unavailable skill {name:?}"),
        SkillLookupError::Ambiguous { paths, .. } => {
            format!("skill {name:?} is ambiguous among {} sources", paths.len())
        }
        SkillLookupError::Invalid { reason, .. } => {
            format!("skill {name:?} is invalid: {reason:?}")
        }
        SkillLookupError::Omitted { reason, .. } => {
            format!("skill {name:?} was omitted: {reason:?}")
        }
        SkillLookupError::DirectOnly(_) => {
            format!("skill {name:?} can only be activated directly by the user")
        }
    })
}

enum ResourcePath {
    Workspace(PathBuf),
    Skill { name: String, path: PathBuf },
}

struct ResolvedResource {
    directory: Arc<Dir>,
    path: PathBuf,
    skill_name: Option<String>,
}

fn parse_resource_path(path: &str) -> Result<ResourcePath, ToolError> {
    let Some(locator) = path.strip_prefix("skill://") else {
        return normalise_path(path).map(ResourcePath::Workspace);
    };
    let (name, relative) = locator
        .split_once('/')
        .ok_or_else(|| ToolError::value("skill locator must include a trailing slash"))?;
    if !valid_skill_name(name) {
        return Err(ToolError::value("skill locator has an invalid name"));
    }
    let path = normalise_path(relative)?;
    Ok(ResourcePath::Skill {
        name: name.to_owned(),
        path,
    })
}

fn workspace_mutation_path(path: &str) -> Result<PathBuf, ToolError> {
    if path.starts_with("skill://") {
        return Err(ToolError::permission("skill resources are read-only"));
    }
    normalise_path(path)
}

fn read_path(arguments: ToolValue) -> Result<ResourcePath, ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "read() expects one positional string argument",
        ));
    };
    let [ToolValue::String(path)] = arguments.as_slice() else {
        return Err(ToolError::type_error(
            "read() expects one positional string argument",
        ));
    };
    parse_resource_path(path)
}

fn list_dir_path(arguments: ToolValue) -> Result<ResourcePath, ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "list_dir() expects zero or one positional string argument",
        ));
    };
    match arguments.as_slice() {
        [] => Ok(ResourcePath::Workspace(PathBuf::new())),
        [ToolValue::String(path)] => parse_resource_path(path),
        _ => Err(ToolError::type_error(
            "list_dir() expects zero or one positional string argument",
        )),
    }
}

fn glob_arguments(arguments: ToolValue) -> Result<(String, PathBuf), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "glob() expects one or two positional string arguments",
        ));
    };
    match arguments.as_slice() {
        [ToolValue::String(pattern)] => Ok((pattern.clone(), PathBuf::new())),
        [ToolValue::String(pattern), ToolValue::String(path)] => {
            Ok((pattern.clone(), normalise_path(path)?))
        }
        _ => Err(ToolError::type_error(
            "glob() expects one or two positional string arguments",
        )),
    }
}

fn grep_arguments(arguments: ToolValue) -> Result<(String, PathBuf, Option<String>), ToolError> {
    let ToolValue::List(arguments) = arguments else {
        return Err(ToolError::type_error(
            "grep() expects one to three positional arguments: string pattern, string path, optional string file_glob",
        ));
    };
    match arguments.as_slice() {
        [ToolValue::String(pattern)] => Ok((pattern.clone(), PathBuf::new(), None)),
        [ToolValue::String(pattern), ToolValue::String(path)] => {
            Ok((pattern.clone(), normalise_path(path)?, None))
        }
        [
            ToolValue::String(pattern),
            ToolValue::String(path),
            ToolValue::None,
        ] => Ok((pattern.clone(), normalise_path(path)?, None)),
        [
            ToolValue::String(pattern),
            ToolValue::String(path),
            ToolValue::String(file_glob),
        ] => Ok((
            pattern.clone(),
            normalise_path(path)?,
            Some(file_glob.clone()),
        )),
        _ => Err(ToolError::type_error(
            "grep() expects one to three positional arguments: string pattern, string path, optional string file_glob",
        )),
    }
}

fn normalise_path(path: &str) -> Result<PathBuf, ToolError> {
    let path = Path::new(path);
    let mut normalised = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(component) => normalised.push(component),
            Component::ParentDir => {
                return Err(ToolError::permission(
                    "workspace paths cannot contain parent components",
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(ToolError::permission("workspace paths must be relative"));
            }
        }
    }
    Ok(normalised)
}

struct ReadFile {
    contents: String,
    fingerprint: ContentFingerprint,
}

fn read_file(workspace: &Dir, path: &Path, max_bytes: usize) -> Result<ReadFile, ToolError> {
    let (parent, file_name) = open_parent(workspace, path)?;
    let mut file = open_content_no_follow(&parent, &file_name)?;
    let metadata = file.metadata().map_err(ToolError::from_io)?;
    if metadata.is_dir() {
        return Err(ToolError::new(
            ToolErrorKind::IsADirectory,
            "path is a directory",
        ));
    }
    if !metadata.is_file() {
        return Err(ToolError::value("path is not a regular file"));
    }

    let contents = read_limited(&mut file, max_bytes)?;
    let fingerprint = content_fingerprint(&contents);
    let contents =
        String::from_utf8(contents).map_err(|_| ToolError::value("file is not valid UTF-8"))?;
    Ok(ReadFile {
        contents,
        fingerprint,
    })
}

fn list_directory(
    workspace: &Dir,
    path: &Path,
    limits: ToolLimits,
    display_prefix: &str,
) -> Result<Vec<ToolValue>, ToolError> {
    let directory = open_directory_root(workspace, path)?;
    let relative_base = normalised_path_string(path);
    let base_path = match (display_prefix.is_empty(), relative_base.is_empty()) {
        (true, _) => relative_base,
        (false, true) => display_prefix.to_owned(),
        (false, false) => format!("{display_prefix}/{relative_base}"),
    };
    let mut entries = Vec::new();

    for entry in directory.entries().map_err(ToolError::from_io)? {
        let entry = entry.map_err(ToolError::from_io)?;
        let file_name = entry.file_name();
        let name = file_name
            .to_str()
            .ok_or_else(|| ToolError::value("directory entry name is not valid UTF-8"))?;
        let metadata = directory
            .symlink_metadata(&file_name)
            .map_err(ToolError::from_io)?;
        let (kind, size_bytes) = entry_kind(&metadata)?;
        let path = if base_path.is_empty() {
            name.to_owned()
        } else {
            format!("{base_path}/{name}")
        };
        entries.push((path, kind, size_bytes));
        if entries.len() > limits.max_collection_items {
            return Err(ToolError::runtime(format!(
                "directory listing exceeds the {}-entry limit",
                limits.max_collection_items
            )));
        }
    }

    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut payload_bytes = 0;
    entries
        .into_iter()
        .map(|(path, kind, size_bytes)| {
            payload_bytes += path.len() + kind.len();
            if payload_bytes > limits.max_collection_payload_bytes {
                return Err(ToolError::runtime(format!(
                    "directory listing exceeds the {}-byte payload limit",
                    limits.max_collection_payload_bytes
                )));
            }
            Ok(directory_entry_value(path, kind, size_bytes))
        })
        .collect()
}

fn compile_glob(pattern: &str) -> Result<(GlobMatcher, bool), ToolError> {
    let mut builder = GlobBuilder::new(pattern);
    let glob = builder
        .literal_separator(true)
        .case_insensitive(false)
        .build()
        .map_err(|_| ToolError::value("glob pattern is invalid"))?;
    Ok((glob.compile_matcher(), !pattern.contains('/')))
}

fn compile_regex(pattern: &str) -> Result<RegexMatcher, ToolError> {
    let mut builder = RegexMatcherBuilder::new();
    builder.multi_line(true).crlf(true).ban_byte(Some(b'\0'));
    builder
        .build(pattern)
        .map_err(|_| ToolError::value("regular expression is invalid"))
}

#[derive(Clone)]
struct IgnoreMatchers {
    rgignore: Gitignore,
    ignore: Gitignore,
    gitignore: Gitignore,
}

struct SearchDirectory {
    directory: Dir,
    path: PathBuf,
    ignores: Vec<IgnoreMatchers>,
}

fn root_ignore_context(
    workspace: &Dir,
    root: &Path,
    root_directory: &Dir,
) -> Result<(PathBuf, Vec<IgnoreMatchers>), ToolError> {
    let mut ignore_directory = workspace.try_clone().map_err(ToolError::from_io)?;
    let mut components = root.components().peekable();
    let mut root_path = PathBuf::new();
    let mut ignores = vec![directory_ignore_matchers(&ignore_directory, &root_path)?];
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            unreachable!("search roots are normalised before traversal");
        };
        root_path.push(component);
        ignore_directory = if components.peek().is_some() {
            open_directory_no_follow(&ignore_directory, Path::new(component))?
        } else {
            root_directory.try_clone().map_err(ToolError::from_io)?
        };
        ignores.push(directory_ignore_matchers(&ignore_directory, &root_path)?);
    }
    Ok((root_path, ignores))
}

fn glob_directory(
    workspace: &Dir,
    root: &Path,
    matcher: (GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let (matcher, basename_only) = matcher;
    let root_directory = open_directory_root(workspace, root)?;
    let (root_path, ignores) = root_ignore_context(workspace, root, &root_directory)?;

    let mut directories = vec![SearchDirectory {
        directory: root_directory,
        path: root_path,
        ignores,
    }];
    let mut entries = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in directory.directory.entries().map_err(ToolError::from_io)? {
            let entry = entry.map_err(ToolError::from_io)?;
            let name = entry.file_name();
            let path = directory.path.join(&name);
            let metadata = directory
                .directory
                .symlink_metadata(&name)
                .map_err(ToolError::from_io)?;
            if is_hidden_name(&name) || is_ignored(&directory.ignores, &path, metadata.is_dir()) {
                continue;
            }
            let name = name
                .to_str()
                .ok_or_else(|| ToolError::value("directory entry name is not valid UTF-8"))?;
            let relative_path = path
                .strip_prefix(root)
                .expect("glob entries remain beneath their requested root");
            if matches_glob(&matcher, basename_only, name, relative_path) {
                if entries.len() == limits.max_collection_items {
                    return Err(ToolError::runtime(format!(
                        "glob results exceed the {}-entry limit",
                        limits.max_collection_items
                    )));
                }
                entries.push((normalised_path_string(&path), entry_kind(&metadata)?));
            }
            if metadata.is_dir() {
                let child = open_directory_no_follow(&directory.directory, Path::new(name))?;
                let mut child_ignores = directory.ignores.clone();
                child_ignores.push(directory_ignore_matchers(&child, &path)?);
                directories.push(SearchDirectory {
                    directory: child,
                    path,
                    ignores: child_ignores,
                });
            }
        }
    }

    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut payload_bytes = 0;
    entries
        .into_iter()
        .map(|(path, (kind, size_bytes))| {
            payload_bytes += path.len() + kind.len();
            if payload_bytes > limits.max_collection_payload_bytes {
                return Err(ToolError::runtime(format!(
                    "glob results exceed the {}-byte payload limit",
                    limits.max_collection_payload_bytes
                )));
            }
            Ok(directory_entry_value(path, kind, size_bytes))
        })
        .collect()
}

#[derive(Debug)]
struct GrepMatchValue {
    path: String,
    line: i64,
    text: String,
}

enum GrepFileFailure {
    Binary,
    InvalidUtf8,
    NotRegular,
    Tool(ToolError),
}

enum GrepSearchError {
    Io(std::io::Error),
    InvalidUtf8,
    Other,
}

impl SinkError for GrepSearchError {
    fn error_message<T: std::fmt::Display>(_message: T) -> Self {
        Self::Other
    }

    fn error_io(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

struct GrepSink {
    path: String,
    matches: Vec<GrepMatchValue>,
    payload_bytes: usize,
    max_items: usize,
    max_payload_bytes: usize,
    overflowed: bool,
    binary: bool,
}

impl GrepSink {
    fn new(path: String, max_items: usize, max_payload_bytes: usize) -> Self {
        Self {
            path,
            matches: Vec::new(),
            payload_bytes: 0,
            max_items,
            max_payload_bytes,
            overflowed: false,
            binary: false,
        }
    }
}

impl Sink for GrepSink {
    type Error = GrepSearchError;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.matches.len() == self.max_items {
            self.overflowed = true;
            return Ok(true);
        }
        let text = matched
            .bytes()
            .strip_suffix(b"\n")
            .and_then(|text| text.strip_suffix(b"\r").or(Some(text)))
            .unwrap_or(matched.bytes());
        let text = std::str::from_utf8(text).map_err(|_| GrepSearchError::InvalidUtf8)?;
        let Some(payload_bytes) = self
            .payload_bytes
            .checked_add(self.path.len())
            .and_then(|bytes| bytes.checked_add(text.len()))
            .filter(|bytes| *bytes <= self.max_payload_bytes)
        else {
            self.overflowed = true;
            return Ok(true);
        };
        let line = matched.line_number().ok_or(GrepSearchError::Other)?;
        let line = i64::try_from(line).map_err(|_| GrepSearchError::Other)?;
        self.matches.push(GrepMatchValue {
            path: self.path.clone(),
            line,
            text: text.to_owned(),
        });
        self.payload_bytes = payload_bytes;
        Ok(true)
    }

    fn binary_data(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, Self::Error> {
        self.binary = true;
        Ok(false)
    }
}

struct Utf8ValidatingReader<R> {
    reader: R,
    pending: Vec<u8>,
    invalid_utf8: bool,
}

impl<R> Utf8ValidatingReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            pending: Vec::new(),
            invalid_utf8: false,
        }
    }

    fn invalid_utf8_error() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "file is not valid UTF-8")
    }
}

impl<R: Read> Read for Utf8ValidatingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let count = self.reader.read(buffer)?;
        if count == 0 {
            if self.pending.is_empty() {
                return Ok(0);
            }
            self.invalid_utf8 = true;
            return Err(Self::invalid_utf8_error());
        }

        self.pending.extend_from_slice(&buffer[..count]);
        match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.clear(),
            Err(error) if error.error_len().is_some() => {
                self.invalid_utf8 = true;
                return Err(Self::invalid_utf8_error());
            }
            Err(error) => {
                self.pending.drain(..error.valid_up_to());
            }
        }
        Ok(count)
    }
}

fn grep_workspace(
    workspace: &Dir,
    root: &Path,
    matcher: RegexMatcher,
    file_glob: (GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    if root.as_os_str().is_empty() {
        return grep_directory(workspace, root, &matcher, &file_glob, limits);
    }

    let (parent, file_name) = open_parent(workspace, root)?;
    let name = file_name
        .to_str()
        .expect("normalised paths originate from UTF-8 tool arguments");
    let file_matches = matches_glob(&file_glob.0, file_glob.1, name, Path::new(name));
    if !file_matches && is_regular_no_follow(&parent, &file_name)? {
        return Ok(Vec::new());
    }

    let file = open_content_no_follow(&parent, &file_name)?;
    let metadata = file.metadata().map_err(ToolError::from_io)?;
    if metadata.is_dir() {
        return grep_open_directory(
            workspace,
            root,
            Dir::from_std_file(file.into_std()),
            &matcher,
            &file_glob,
            limits,
        );
    }
    if !metadata.is_file() {
        return Err(ToolError::value("path is not a regular file"));
    }

    if !file_matches {
        return Ok(Vec::new());
    }
    grep_open_file(file, normalised_path_string(root), &matcher, limits, 0, 0)
        .map_err(grep_explicit_error)
        .map(grep_values)
}

fn grep_directory(
    workspace: &Dir,
    root: &Path,
    matcher: &RegexMatcher,
    file_glob: &(GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let root_directory = open_directory_root(workspace, root)?;
    grep_open_directory(workspace, root, root_directory, matcher, file_glob, limits)
}

fn grep_open_directory(
    workspace: &Dir,
    root: &Path,
    root_directory: Dir,
    matcher: &RegexMatcher,
    file_glob: &(GlobMatcher, bool),
    limits: ToolLimits,
) -> Result<Vec<ToolValue>, ToolError> {
    let (root_path, ignores) = root_ignore_context(workspace, root, &root_directory)?;

    let mut directories = vec![SearchDirectory {
        directory: root_directory,
        path: root_path,
        ignores,
    }];
    let mut matches = Vec::new();
    let mut payload_bytes = 0;
    while let Some(directory) = directories.pop() {
        for entry in directory.directory.entries().map_err(ToolError::from_io)? {
            let entry = entry.map_err(ToolError::from_io)?;
            let name = entry.file_name();
            let path = directory.path.join(&name);
            let metadata = directory
                .directory
                .symlink_metadata(&name)
                .map_err(ToolError::from_io)?;
            if is_hidden_name(&name) || is_ignored(&directory.ignores, &path, metadata.is_dir()) {
                continue;
            }
            let name = name
                .to_str()
                .ok_or_else(|| ToolError::value("directory entry name is not valid UTF-8"))?;
            if metadata.is_file() {
                let relative_path = path
                    .strip_prefix(root)
                    .expect("grep entries remain beneath their requested root");
                if matches_glob(&file_glob.0, file_glob.1, name, relative_path) {
                    let file_matches = match grep_regular_file(
                        &directory.directory,
                        Path::new(name),
                        normalised_path_string(&path),
                        matcher,
                        limits,
                        matches.len(),
                        payload_bytes,
                    ) {
                        Ok(matches) => matches,
                        Err(
                            GrepFileFailure::Binary
                            | GrepFileFailure::InvalidUtf8
                            | GrepFileFailure::NotRegular,
                        ) => continue,
                        Err(GrepFileFailure::Tool(error)) => return Err(error),
                    };
                    payload_bytes += file_matches
                        .iter()
                        .map(|matched| matched.path.len() + matched.text.len())
                        .sum::<usize>();
                    matches.extend(file_matches);
                }
            } else if metadata.is_dir() {
                let child = open_directory_no_follow(&directory.directory, Path::new(name))?;
                let mut child_ignores = directory.ignores.clone();
                child_ignores.push(directory_ignore_matchers(&child, &path)?);
                directories.push(SearchDirectory {
                    directory: child,
                    path,
                    ignores: child_ignores,
                });
            }
        }
    }

    matches.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    Ok(grep_values(matches))
}

fn grep_regular_file(
    parent: &Dir,
    name: &Path,
    path: String,
    matcher: &RegexMatcher,
    limits: ToolLimits,
    existing_items: usize,
    existing_payload_bytes: usize,
) -> Result<Vec<GrepMatchValue>, GrepFileFailure> {
    let file = open_content_no_follow(parent, name).map_err(GrepFileFailure::Tool)?;
    grep_open_file(
        file,
        path,
        matcher,
        limits,
        existing_items,
        existing_payload_bytes,
    )
}

fn grep_open_file(
    mut file: File,
    path: String,
    matcher: &RegexMatcher,
    limits: ToolLimits,
    existing_items: usize,
    existing_payload_bytes: usize,
) -> Result<Vec<GrepMatchValue>, GrepFileFailure> {
    if !file
        .metadata()
        .map_err(ToolError::from_io)
        .map_err(GrepFileFailure::Tool)?
        .is_file()
    {
        return Err(GrepFileFailure::NotRegular);
    }

    let mut searcher_builder = SearcherBuilder::new();
    searcher_builder
        .line_terminator(LineTerminator::crlf())
        .binary_detection(BinaryDetection::quit(b'\0'))
        .bom_sniffing(false);
    let mut searcher = searcher_builder.build();
    let mut reader = Utf8ValidatingReader::new(&mut file);
    let mut sink = GrepSink::new(
        path,
        limits.max_collection_items.saturating_sub(existing_items),
        limits
            .max_collection_payload_bytes
            .saturating_sub(existing_payload_bytes),
    );
    let result = searcher.search_reader(matcher, &mut reader, &mut sink);
    if sink.binary {
        return Err(GrepFileFailure::Binary);
    }
    match result {
        Ok(()) if sink.overflowed => Err(GrepFileFailure::Tool(ToolError::runtime(format!(
            "grep results exceed the {}-match or {}-byte payload limit",
            limits.max_collection_items, limits.max_collection_payload_bytes
        )))),
        Ok(()) => Ok(sink.matches),
        Err(GrepSearchError::InvalidUtf8) => Err(GrepFileFailure::InvalidUtf8),
        Err(GrepSearchError::Io(_)) if reader.invalid_utf8 => Err(GrepFileFailure::InvalidUtf8),
        Err(GrepSearchError::Io(error)) => Err(GrepFileFailure::Tool(ToolError::from_io(error))),
        Err(GrepSearchError::Other) => Err(GrepFileFailure::Tool(ToolError::new(
            ToolErrorKind::Os,
            "regular expression search failed",
        ))),
    }
}

fn grep_explicit_error(error: GrepFileFailure) -> ToolError {
    match error {
        GrepFileFailure::Binary => ToolError::value("file contains binary data"),
        GrepFileFailure::InvalidUtf8 => ToolError::value("file is not valid UTF-8"),
        GrepFileFailure::NotRegular => ToolError::value("path is not a regular file"),
        GrepFileFailure::Tool(error) => error,
    }
}

fn grep_values(matches: Vec<GrepMatchValue>) -> Vec<ToolValue> {
    matches
        .into_iter()
        .map(|matched| {
            ToolValue::Dictionary(BTreeMap::from([
                ("path".to_owned(), ToolValue::String(matched.path)),
                ("line".to_owned(), ToolValue::Integer(matched.line)),
                ("text".to_owned(), ToolValue::String(matched.text)),
            ]))
        })
        .collect()
}

fn matches_glob(
    matcher: &GlobMatcher,
    basename_only: bool,
    name: &str,
    relative_path: &Path,
) -> bool {
    if basename_only {
        matcher.is_match(name)
    } else {
        matcher.is_match(normalised_path_string(relative_path))
    }
}

fn is_hidden_name(name: &OsStr) -> bool {
    name.as_encoded_bytes().first() == Some(&b'.')
}

fn is_ignored(matchers: &[IgnoreMatchers], path: &Path, is_dir: bool) -> bool {
    if let Some(ignored) = ignored_by_family(
        matchers.iter().rev().map(|matchers| &matchers.rgignore),
        path,
        is_dir,
    ) {
        return ignored;
    }
    if let Some(ignored) = ignored_by_family(
        matchers.iter().rev().map(|matchers| &matchers.ignore),
        path,
        is_dir,
    ) {
        return ignored;
    }
    ignored_by_family(
        matchers.iter().rev().map(|matchers| &matchers.gitignore),
        path,
        is_dir,
    )
    .unwrap_or(false)
}

fn ignored_by_family<'a>(
    matchers: impl Iterator<Item = &'a Gitignore>,
    path: &Path,
    is_dir: bool,
) -> Option<bool> {
    for matcher in matchers {
        let matched = matcher.matched(path, is_dir);
        if matched.is_ignore() {
            return Some(true);
        }
        if matched.is_whitelist() {
            return Some(false);
        }
    }
    None
}

fn directory_ignore_matchers(directory: &Dir, path: &Path) -> Result<IgnoreMatchers, ToolError> {
    Ok(IgnoreMatchers {
        rgignore: directory_ignore_matcher(directory, path, ".rgignore")?,
        ignore: directory_ignore_matcher(directory, path, ".ignore")?,
        gitignore: directory_ignore_matcher(directory, path, ".gitignore")?,
    })
}

fn directory_ignore_matcher(
    directory: &Dir,
    path: &Path,
    name: &str,
) -> Result<Gitignore, ToolError> {
    let mut builder = GitignoreBuilder::new(path);
    if let Some(contents) = read_ignore_file(directory, Path::new(name))? {
        let contents = String::from_utf8(contents)
            .map_err(|_| ToolError::value("project ignore rules are not valid UTF-8"))?;
        let source = path.join(name);
        for (index, line) in contents.lines().enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            builder
                .add_line(Some(source.clone()), line)
                .map_err(|_| ToolError::value("project ignore rules are invalid"))?;
        }
    }
    builder
        .build()
        .map_err(|_| ToolError::value("project ignore rules are invalid"))
}

fn read_ignore_file(directory: &Dir, name: &Path) -> Result<Option<Vec<u8>>, ToolError> {
    let mut file = match open_content_no_follow(directory, name) {
        Ok(file) => file,
        Err(error) if error.kind == ToolErrorKind::FileNotFound => return Ok(None),
        Err(error) if error.kind == ToolErrorKind::Value => return Ok(None),
        Err(error) if error.kind == ToolErrorKind::Permission && is_symlink(directory, name)? => {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !file.metadata().map_err(ToolError::from_io)?.is_file() {
        return Ok(None);
    }

    Ok(Some(read_limited(&mut file, FILE_MAX_BYTES)?))
}

fn entry_kind(metadata: &Metadata) -> Result<(&'static str, Option<i64>), ToolError> {
    if metadata.file_type().is_symlink() {
        Ok(("symlink", None))
    } else if metadata.is_file() {
        Ok((
            "file",
            Some(
                i64::try_from(metadata.len())
                    .map_err(|_| ToolError::new(ToolErrorKind::Os, "file size is too large"))?,
            ),
        ))
    } else if metadata.is_dir() {
        Ok(("directory", None))
    } else {
        Ok(("other", None))
    }
}

fn directory_entry_value(path: String, kind: &str, size_bytes: Option<i64>) -> ToolValue {
    ToolValue::Dictionary(BTreeMap::from([
        ("path".to_owned(), ToolValue::String(path)),
        ("kind".to_owned(), ToolValue::String(kind.to_owned())),
        (
            "size_bytes".to_owned(),
            size_bytes.map_or(ToolValue::None, ToolValue::Integer),
        ),
    ]))
}

fn normalised_path_string(path: &Path) -> String {
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .expect("normalised paths originate from UTF-8 tool arguments")
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn open_directory_root(workspace: &Dir, path: &Path) -> Result<Dir, ToolError> {
    let (parent, file_name) = open_parent(workspace, path)?;
    if path.as_os_str().is_empty() {
        return Ok(parent);
    }
    open_directory_no_follow(&parent, &file_name)
}

fn open_parent(workspace: &Dir, path: &Path) -> Result<(Dir, PathBuf), ToolError> {
    let mut components = path.components().peekable();
    let mut directory = workspace.try_clone().map_err(ToolError::from_io)?;
    if components.peek().is_none() {
        return Ok((directory, PathBuf::from(".")));
    }
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            unreachable!("paths are normalised before component traversal");
        };
        let component = PathBuf::from(component);
        if components.peek().is_none() {
            return Ok((directory, component));
        }
        directory = open_directory_no_follow(&directory, &component)?;
    }
    unreachable!("normalised paths contain at least one component")
}

#[cfg(unix)]
fn open_content_no_follow(parent: &Dir, name: &Path) -> Result<File, ToolError> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(|descriptor| File::from_std(std::fs::File::from(descriptor)))
    .map_err(|error| content_open_error(parent, name, error))
}

#[cfg(not(unix))]
fn open_content_no_follow(parent: &Dir, name: &Path) -> Result<File, ToolError> {
    parent.open(name).map_err(ToolError::from_io)
}

#[cfg(unix)]
fn open_directory_no_follow(parent: &Dir, name: &Path) -> Result<Dir, ToolError> {
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(|descriptor| Dir::from_std_file(std::fs::File::from(descriptor)))
    .map_err(|error| directory_open_error(parent, name, error))
}

#[cfg(unix)]
fn directory_open_error(parent: &Dir, name: &Path, error: Errno) -> ToolError {
    if matches!(error, Errno::LOOP | Errno::MLINK) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    let io_error = std::io::Error::from(error);
    if io_error.kind() != std::io::ErrorKind::PermissionDenied
        && matches!(
            statat(parent, name, AtFlags::SYMLINK_NOFOLLOW),
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_symlink()
        )
    {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    ToolError::from_io(io_error)
}

#[cfg(unix)]
fn content_open_error(parent: &Dir, name: &Path, error: Errno) -> ToolError {
    if matches!(error, Errno::LOOP | Errno::MLINK) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    let io_error = std::io::Error::from(error);
    if matches!(
        io_error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::NotADirectory
            | std::io::ErrorKind::PermissionDenied
    ) {
        return ToolError::from_io(io_error);
    }
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let file_type = FileType::from_raw_mode(stat.st_mode);
            if file_type.is_symlink() {
                ToolError::permission("workspace paths cannot traverse symbolic links")
            } else if file_type.is_dir() {
                ToolError::new(ToolErrorKind::IsADirectory, "path is a directory")
            } else if !file_type.is_file() {
                ToolError::value("path is not a regular file")
            } else {
                ToolError::from_io(io_error)
            }
        }
        Err(_) => ToolError::from_io(io_error),
    }
}

#[cfg(unix)]
fn is_regular_no_follow(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileType::from_raw_mode(stat.st_mode).is_file())
        .map_err(no_follow_io)
}

#[cfg(unix)]
fn is_symlink(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| FileType::from_raw_mode(stat.st_mode).is_symlink())
        .map_err(no_follow_io)
}

#[cfg(unix)]
fn no_follow_io(error: Errno) -> ToolError {
    if matches!(error, Errno::LOOP | Errno::MLINK) {
        return ToolError::permission("workspace paths cannot traverse symbolic links");
    }
    ToolError::from_io(error.into())
}

#[cfg(not(unix))]
fn open_directory_no_follow(parent: &Dir, name: &Path) -> Result<Dir, ToolError> {
    let metadata = parent.symlink_metadata(name).map_err(ToolError::from_io)?;
    if metadata.file_type().is_symlink() {
        return Err(ToolError::permission(
            "workspace paths cannot traverse symbolic links",
        ));
    }
    parent.open_dir(name).map_err(ToolError::from_io)
}

#[cfg(not(unix))]
fn is_regular_no_follow(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    parent
        .symlink_metadata(name)
        .map(|metadata| metadata.is_file())
        .map_err(ToolError::from_io)
}

#[cfg(not(unix))]
fn is_symlink(parent: &Dir, name: &Path) -> Result<bool, ToolError> {
    parent
        .symlink_metadata(name)
        .map(|metadata| metadata.file_type().is_symlink())
        .map_err(ToolError::from_io)
}

fn read_limited(file: &mut File, max_bytes: usize) -> Result<Vec<u8>, ToolError> {
    let mut contents = Vec::new();
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut contents)
        .map_err(ToolError::from_io)?;
    if contents.len() > max_bytes {
        return Err(ToolError::value(format!(
            "file exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(contents)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const TEMPORARY_NAME_ATTEMPTS: usize = 16;

#[cfg(any(target_os = "linux", target_os = "macos"))]
enum WriteTarget {
    Missing,
    Regular,
    Directory,
    Symlink,
    Other,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct CurrentRegularFile {
    contents: Vec<u8>,
    fingerprint: ContentFingerprint,
    mode: Mode,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct UnpublishedTemporary<'a> {
    parent: &'a Dir,
    name: PathBuf,
    published: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<'a> UnpublishedTemporary<'a> {
    fn new(parent: &'a Dir, name: PathBuf) -> Self {
        Self {
            parent,
            name,
            published: false,
        }
    }

    fn publish(&mut self) {
        self.published = true;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for UnpublishedTemporary<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = unlinkat(self.parent, &self.name, AtFlags::empty());
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_whole_file(
    workspace: &Dir,
    path: &Path,
    content: &str,
    authorisation: Option<ContentFingerprint>,
    max_file_bytes: usize,
    #[cfg(test)] test_hook: Option<&WriteTestHook>,
) -> Result<WholeFileWrite, ToolError> {
    let (parent, name) = open_parent_creating(workspace, path)?;
    match write_target(&parent, &name)? {
        WriteTarget::Missing => {
            let (temporary, file) = create_temporary_file(
                &parent,
                #[cfg(test)]
                test_hook,
            )?;
            let mut temporary_file = UnpublishedTemporary::new(&parent, temporary);
            write_temporary_file(
                file,
                content,
                None,
                #[cfg(test)]
                test_hook,
            )?;
            #[cfg(test)]
            if let Some(test_hook) = test_hook {
                test_hook.pause(WritePause::BeforePublish);
                if test_hook.fail_publishing() {
                    return Err(ToolError::new(
                        ToolErrorKind::Os,
                        "temporary file publication failed",
                    ));
                }
            }
            publish_new_file(
                &parent,
                &temporary_file.name,
                &name,
                #[cfg(test)]
                test_hook,
            )?;
            temporary_file.publish();
            Ok(WholeFileWrite {
                outcome: WholeFileWriteOutcome::Created,
                fingerprint: content_fingerprint(content.as_bytes()),
            })
        }
        WriteTarget::Regular => {
            let Some(authorisation) = authorisation else {
                return Err(ToolError::runtime(
                    "file contents are not authorised for replacement",
                ));
            };
            #[cfg(test)]
            if let Some(test_hook) = test_hook {
                test_hook.pause(WritePause::InitialRead);
            }
            let initial = current_regular_file(&parent, &name, max_file_bytes)
                .map_err(|_| ToolError::runtime("file changed during replacement"))?;
            if initial.fingerprint != authorisation {
                return Err(ToolError::runtime("file changed during replacement"));
            }
            replace_existing_file(
                &parent,
                &name,
                content,
                initial.fingerprint,
                initial.mode,
                max_file_bytes,
                #[cfg(test)]
                test_hook,
            )?;
            Ok(WholeFileWrite {
                outcome: WholeFileWriteOutcome::Replaced,
                fingerprint: content_fingerprint(content.as_bytes()),
            })
        }
        WriteTarget::Directory => Err(ToolError::new(
            ToolErrorKind::IsADirectory,
            "path is a directory",
        )),
        WriteTarget::Symlink => Err(ToolError::permission(
            "workspace paths cannot traverse symbolic links",
        )),
        WriteTarget::Other => Err(ToolError::value("path is not a regular file")),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn write_whole_file(
    _workspace: &Dir,
    _path: &Path,
    _content: &str,
    _authorisation: Option<ContentFingerprint>,
    _max_file_bytes: usize,
    #[cfg(test)] _test_hook: Option<&WriteTestHook>,
) -> Result<WholeFileWrite, ToolError> {
    Err(ToolError::new(
        ToolErrorKind::Os,
        "workspace mutation is unsupported on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn edit_whole_file(
    workspace: &Dir,
    path: &Path,
    old: &str,
    new: &str,
    max_file_bytes: usize,
    #[cfg(test)] test_hook: Option<&WriteTestHook>,
) -> Result<WholeFileEdit, ToolError> {
    let (parent, name) = open_parent(workspace, path)?;
    let CurrentRegularFile {
        contents,
        fingerprint: pre_fingerprint,
        mode,
    } = current_regular_file(&parent, &name, max_file_bytes)?;
    let contents =
        String::from_utf8(contents).map_err(|_| ToolError::value("file is not valid UTF-8"))?;
    let Some(start) = contents.find(old) else {
        return Err(ToolError::value("edit target does not occur exactly once"));
    };
    let next_search_start = start
        + old
            .chars()
            .next()
            .expect("edit target is non-empty")
            .len_utf8();
    if contents[next_search_start..].find(old).is_some() {
        return Err(ToolError::value("edit target does not occur exactly once"));
    }
    let replacement_length = contents
        .len()
        .checked_sub(old.len())
        .and_then(|length| length.checked_add(new.len()))
        .ok_or_else(|| ToolError::value("edited file exceeds the byte limit"))?;
    if replacement_length > max_file_bytes {
        return Err(ToolError::value(format!(
            "file exceeds the {max_file_bytes}-byte limit"
        )));
    }
    let mut replacement = String::with_capacity(replacement_length);
    replacement.push_str(&contents[..start]);
    replacement.push_str(new);
    replacement.push_str(&contents[start + old.len()..]);
    let post_fingerprint = content_fingerprint(replacement.as_bytes());
    replace_existing_file(
        &parent,
        &name,
        &replacement,
        pre_fingerprint,
        mode,
        max_file_bytes,
        #[cfg(test)]
        test_hook,
    )?;
    Ok(WholeFileEdit {
        pre_fingerprint,
        post_fingerprint,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn edit_whole_file(
    _workspace: &Dir,
    _path: &Path,
    _old: &str,
    _new: &str,
    _max_file_bytes: usize,
    #[cfg(test)] _test_hook: Option<&WriteTestHook>,
) -> Result<WholeFileEdit, ToolError> {
    Err(ToolError::new(
        ToolErrorKind::Os,
        "workspace mutation is unsupported on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_parent_creating(workspace: &Dir, path: &Path) -> Result<(Dir, PathBuf), ToolError> {
    let mut components = path.components().peekable();
    let mut directory = workspace.try_clone().map_err(ToolError::from_io)?;
    if components.peek().is_none() {
        return Ok((directory, PathBuf::from(".")));
    }
    while let Some(component) = components.next() {
        let Component::Normal(component) = component else {
            unreachable!("paths are normalised before component traversal");
        };
        let component = PathBuf::from(component);
        if components.peek().is_none() {
            return Ok((directory, component));
        }
        directory = match open_directory_no_follow(&directory, &component) {
            Ok(directory) => directory,
            Err(error) if error.kind == ToolErrorKind::FileNotFound => {
                match mkdirat(&directory, &component, Mode::from_raw_mode(0o777)) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(no_follow_io(error)),
                }
                open_directory_no_follow(&directory, &component)?
            }
            Err(error) => return Err(error),
        };
    }
    unreachable!("normalised paths contain at least one component")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_target(parent: &Dir, name: &Path) -> Result<WriteTarget, ToolError> {
    match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            let file_type = FileType::from_raw_mode(stat.st_mode);
            if file_type.is_file() {
                Ok(WriteTarget::Regular)
            } else if file_type.is_dir() {
                Ok(WriteTarget::Directory)
            } else if file_type.is_symlink() {
                Ok(WriteTarget::Symlink)
            } else {
                Ok(WriteTarget::Other)
            }
        }
        Err(Errno::NOENT) => Ok(WriteTarget::Missing),
        Err(error) => Err(no_follow_io(error)),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn current_regular_file(
    parent: &Dir,
    name: &Path,
    max_file_bytes: usize,
) -> Result<CurrentRegularFile, ToolError> {
    let mut file = open_content_no_follow(parent, name)?;
    let stat = fstat(&file).map_err(|error| ToolError::from_io(error.into()))?;
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if file_type.is_dir() {
        return Err(ToolError::new(
            ToolErrorKind::IsADirectory,
            "path is a directory",
        ));
    }
    if !file_type.is_file() {
        return Err(ToolError::value("path is not a regular file"));
    }
    let contents = read_limited(&mut file, max_file_bytes)?;
    Ok(CurrentRegularFile {
        fingerprint: content_fingerprint(&contents),
        contents,
        mode: Mode::from_raw_mode(stat.st_mode),
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn replace_existing_file(
    parent: &Dir,
    name: &Path,
    content: &str,
    initial_fingerprint: ContentFingerprint,
    replacement_mode: Mode,
    max_file_bytes: usize,
    #[cfg(test)] test_hook: Option<&WriteTestHook>,
) -> Result<(), ToolError> {
    let (temporary, file) = create_temporary_file(
        parent,
        #[cfg(test)]
        test_hook,
    )?;
    let mut temporary_file = UnpublishedTemporary::new(parent, temporary);
    write_temporary_file(
        file,
        content,
        Some(replacement_mode),
        #[cfg(test)]
        test_hook,
    )?;
    #[cfg(test)]
    if let Some(test_hook) = test_hook {
        test_hook.pause(WritePause::BeforeRecheck);
    }
    let latest = current_regular_file(parent, name, max_file_bytes)
        .map_err(|_| ToolError::runtime("file changed during replacement"))?;
    if latest.fingerprint != initial_fingerprint {
        return Err(ToolError::runtime("file changed during replacement"));
    }
    #[cfg(test)]
    if test_hook.is_some_and(WriteTestHook::fail_publishing) {
        return Err(ToolError::new(
            ToolErrorKind::Os,
            "temporary file publication failed",
        ));
    }
    renameat(parent, &temporary_file.name, parent, name).map_err(replacement_rename_error)?;
    temporary_file.publish();
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn create_temporary_file(
    parent: &Dir,
    #[cfg(test)] test_hook: Option<&WriteTestHook>,
) -> Result<(PathBuf, File), ToolError> {
    for _ in 0..TEMPORARY_NAME_ATTEMPTS {
        let suffix = temporary_suffix(
            #[cfg(test)]
            test_hook,
        )?;
        let name = temporary_name(suffix);
        match openat(
            parent,
            &name,
            OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        ) {
            Ok(descriptor) => return Ok((name, File::from_std(std::fs::File::from(descriptor)))),
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(ToolError::from_io(error.into())),
        }
    }
    Err(ToolError::runtime(
        "temporary file creation did not succeed",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn temporary_suffix(#[cfg(test)] test_hook: Option<&WriteTestHook>) -> Result<[u8; 16], ToolError> {
    #[cfg(test)]
    if let Some(test_hook) = test_hook
        && let Some(suffix) = test_hook.next_temporary_suffix()?
    {
        return Ok(suffix);
    }
    let mut suffix = [0; 16];
    getrandom::fill(&mut suffix)
        .map_err(|_| ToolError::new(ToolErrorKind::Os, "temporary file creation failed"))?;
    Ok(suffix)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn temporary_name(suffix: [u8; 16]) -> PathBuf {
    let suffix = suffix
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    PathBuf::from(format!(".kiki-write-{suffix}"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn write_temporary_file(
    mut file: File,
    content: &str,
    replacement_mode: Option<Mode>,
    #[cfg(test)] test_hook: Option<&WriteTestHook>,
) -> Result<(), ToolError> {
    #[cfg(test)]
    if test_hook.is_some_and(WriteTestHook::fail_writing) {
        return Err(ToolError::new(
            ToolErrorKind::Os,
            "temporary file write failed",
        ));
    }
    file.write_all(content.as_bytes())
        .map_err(ToolError::from_io)?;
    file.flush().map_err(ToolError::from_io)?;
    if let Some(mode) = replacement_mode {
        fchmod(&file, mode).map_err(|error| ToolError::from_io(error.into()))?;
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_new_file(
    parent: &Dir,
    temporary_name: &Path,
    name: &Path,
    #[cfg(test)] test_hook: Option<&WriteTestHook>,
) -> Result<(), ToolError> {
    #[cfg(test)]
    if test_hook.is_some_and(WriteTestHook::use_hard_link_fallback) {
        return publish_new_file_by_link(parent, temporary_name, name);
    }
    match renameat_with(parent, temporary_name, parent, name, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(error) if no_replace_is_unsupported(error) => {
            publish_new_file_by_link(parent, temporary_name, name)
        }
        Err(error) => Err(no_replace_rename_error(error)),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn replacement_rename_error(error: Errno) -> ToolError {
    if matches!(
        error,
        Errno::NOENT | Errno::ISDIR | Errno::NOTDIR | Errno::NOTEMPTY | Errno::EXIST
    ) {
        ToolError::runtime("file changed during replacement")
    } else {
        ToolError::from_io(error.into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn no_replace_rename_error(error: Errno) -> ToolError {
    if matches!(
        error,
        Errno::NOENT | Errno::ISDIR | Errno::NOTDIR | Errno::NOTEMPTY | Errno::EXIST
    ) {
        ToolError::runtime("file changed during publication")
    } else {
        ToolError::from_io(error.into())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn no_replace_is_unsupported(error: Errno) -> bool {
    matches!(error, Errno::NOSYS | Errno::INVAL | Errno::NOTSUP)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_new_file_by_link(
    parent: &Dir,
    temporary_name: &Path,
    name: &Path,
) -> Result<(), ToolError> {
    match linkat(parent, temporary_name, parent, name, AtFlags::empty()) {
        Ok(()) => match unlinkat(parent, temporary_name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(ToolError::from_io(error.into())),
        },
        Err(Errno::NOENT) => Err(ToolError::runtime("file changed during publication")),
        Err(Errno::EXIST) => Err(ToolError::runtime("file appeared before publication")),
        Err(error) => Err(ToolError::from_io(error.into())),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    #[cfg(unix)]
    use cap_std::{ambient_authority, fs::Dir};
    #[cfg(unix)]
    use std::ffi::CString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt;
    use std::time::Duration;

    use futures::StreamExt;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use rustix::io::Errno;
    use tempfile::TempDir;

    use crate::config::constants::FILE_MAX_BYTES;
    use crate::model::SkillActivation;
    use crate::skills::{ProjectScope, SkillCatalog, SkillOrigin, write_skill};

    use super::{
        ACTIVE_TOOLS, ContentFingerprint, SkillLoadLedger, ToolCall, ToolEffect, ToolError,
        ToolErrorKind, ToolHost, ToolName, ToolResult, ToolValue, Utf8ValidatingReader,
        WholeFileAuthorisations, WholeFileEdit, WholeFileWrite, WriteTestHook, bootstrap_source,
        content_fingerprint, invocation_source, normalise_path, open_content_no_follow,
        open_parent, prompt_reference,
    };

    async fn tool_host(workspace: &TempDir) -> ToolHost {
        ToolHost::new(workspace.path()).await.unwrap()
    }

    #[tokio::test]
    async fn skill_resources_read_and_list_from_catalogued_directory() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let root = write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "description: Review\n",
            "Pinned body\n",
        );
        fs::create_dir(root.join("references")).unwrap();
        fs::write(root.join("references/checklist.md"), "live resource").unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace).await.with_skill_catalog(catalog);

        assert_eq!(
            read(&host, "skill://review/references/checklist.md")
                .await
                .unwrap(),
            "live resource"
        );
        assert_eq!(
            list_dir_path(&host, "skill://review/references")
                .await
                .unwrap(),
            [directory_entry(
                "skill://review/references/checklist.md",
                "file",
                Some(13)
            )]
        );
        assert!(
            list_dir_path(&host, "skill://review/")
                .await
                .unwrap()
                .contains(&directory_entry(
                    "skill://review/references",
                    "directory",
                    None
                ))
        );
    }

    #[tokio::test]
    async fn skill_resource_reads_are_live_but_effect_free_and_mutations_are_denied() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let root = home.path().join(".agents/skills/review");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("SKILL.md");
        fs::write(&source, "---\ndescription: Review\n---\nPINNED_BODY\n").unwrap();
        let resource = root.join("reference.txt");
        fs::write(&resource, "first").unwrap();
        let workspace_alias = workspace.path().join("skill:/review");
        fs::create_dir_all(&workspace_alias).unwrap();
        fs::write(workspace_alias.join("reference.txt"), "workspace file").unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace)
            .await
            .with_skill_catalog(catalog.clone());
        let locator = "skill://review/reference.txt";

        let result = host
            .call(ToolCall {
                call_id: 8,
                tool: ToolName::Read,
                arguments: ToolValue::List(vec![ToolValue::String(locator.to_owned())]),
            })
            .await;
        assert_eq!(result.result, Ok(ToolValue::String("first".to_owned())));
        assert!(result.effect.is_none());
        let mut reads = BTreeSet::new();
        let mut modifications = BTreeSet::new();
        let mut authorisations = WholeFileAuthorisations::default();
        result.record_observed_file_operation(&mut reads, &mut modifications);
        result.apply_batch_transition(&mut authorisations, &BTreeSet::new());
        assert!(reads.is_empty());
        assert!(modifications.is_empty());
        assert_eq!(authorisations, WholeFileAuthorisations::default());

        for (tool, arguments) in [
            (ToolName::Write, vec![locator, "changed"]),
            (ToolName::Edit, vec![locator, "first", "changed"]),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 9,
                    tool,
                    arguments: ToolValue::List(
                        arguments
                            .into_iter()
                            .map(|item| ToolValue::String(item.to_owned()))
                            .collect(),
                    ),
                })
                .await;
            assert!(
                matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Permission)
            );
        }
        assert_eq!(
            fs::read_to_string(workspace_alias.join("reference.txt")).unwrap(),
            "workspace file"
        );
        assert_eq!(fs::read_to_string(&resource).unwrap(), "first");

        fs::write(&resource, "second").unwrap();
        fs::write(&source, "---\ndescription: Review\n---\nCHANGED_BODY\n").unwrap();
        assert_eq!(read(&host, locator).await.unwrap(), "second");
        assert!(
            catalog
                .lookup("review")
                .unwrap()
                .source_text()
                .contains("PINNED_BODY")
        );
        assert!(
            !catalog
                .lookup("review")
                .unwrap()
                .source_text()
                .contains("CHANGED_BODY")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn skill_resources_reject_escapes_and_stay_on_held_directory_after_replacement() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("secret"), "outside").unwrap();
        let root = write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "description: Review\n",
            "Pinned\n",
        );
        fs::create_dir(root.join("references")).unwrap();
        fs::write(root.join("references/old.txt"), "old directory").unwrap();
        fs::write(root.join("%2e%2e"), "literal escape").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("outside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("final-link")).unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace).await.with_skill_catalog(catalog);
        for path in [
            "skill://review/../secret",
            "skill://review//secret",
            "skill://review/outside-link/secret",
            "skill://review/final-link",
        ] {
            assert_eq!(
                read_error(&host, path).await,
                ToolErrorKind::Permission,
                "{path}"
            );
        }
        for path in ["skill://review/outside-link", "skill://review/final-link"] {
            assert_eq!(
                list_dir_error(&host, path).await,
                ToolErrorKind::Permission,
                "{path}"
            );
        }
        for path in ["skill:///secret", "skill://review", "skill://../secret"] {
            assert_eq!(
                read_error(&host, path).await,
                ToolErrorKind::Value,
                "{path}"
            );
        }
        assert_eq!(
            read(&host, "skill://review/%2e%2e").await.unwrap(),
            "literal escape"
        );

        let old = home.path().join(".agents/skills/review-old");
        fs::rename(&root, &old).unwrap();
        fs::create_dir_all(root.join("references")).unwrap();
        fs::write(root.join("references/old.txt"), "new directory").unwrap();
        assert_eq!(
            read(&host, "skill://review/references/old.txt")
                .await
                .unwrap(),
            "old directory"
        );
    }

    #[tokio::test]
    async fn skill_resource_listing_counts_locator_prefix_and_skipped_project_is_unavailable() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let user_root = write_skill(
            &home.path().join(".agents/skills"),
            "user",
            "description: User\n",
            "Body\n",
        );
        fs::create_dir(user_root.join("references")).unwrap();
        fs::write(user_root.join("references/x"), "").unwrap();
        write_skill(
            &workspace.path().join(".agents/skills"),
            "project",
            "description: Project\n",
            "Body\n",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let expected_path = "skill://user/references/x";
        let exact = expected_path.len() + "file".len();
        let at_limit = tool_host_with_collection_limits(&workspace, 1, exact)
            .await
            .with_skill_catalog(catalog.clone());
        assert_eq!(
            list_dir_path(&at_limit, "skill://user/references")
                .await
                .unwrap(),
            [directory_entry(expected_path, "file", Some(0))]
        );
        let over_limit = tool_host_with_collection_limits(&workspace, 1, exact - 1)
            .await
            .with_skill_catalog(catalog);
        assert_eq!(
            list_dir_error(&over_limit, "skill://user/references").await,
            ToolErrorKind::Runtime
        );
        assert_eq!(
            read_error(&at_limit, "skill://project/SKILL.md").await,
            ToolErrorKind::Value
        );
    }

    #[tokio::test]
    async fn skill_resources_require_a_listed_model_invocable_or_active_skill() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let skills = home.path().join(".agents/skills");
        write_skill(
            &skills,
            "direct",
            "description: Direct\ndisable-model-invocation: true\n",
            "body",
        );
        for index in 0..33 {
            write_skill(
                &skills,
                &format!("visible-{index:02}"),
                "description: Visible\n",
                "body",
            );
        }
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace)
            .await
            .with_skill_catalog(catalog.clone());

        for (tool, path) in [
            (ToolName::Read, "skill://visible-00/SKILL.md"),
            (ToolName::ListDir, "skill://visible-00/"),
        ] {
            assert!(
                skill_resource(&host, tool, path, &[]).await.is_ok(),
                "{path}"
            );
        }
        for (name, reason) in [
            ("direct", "directly by the user"),
            ("visible-32", "omitted"),
        ] {
            let refused = host
                .load_skill(
                    ToolValue::List(vec![ToolValue::String(name.to_owned())]),
                    &mut SkillLoadLedger::new(&[]).unwrap(),
                )
                .unwrap_err();
            assert!(refused.to_string().contains(reason), "{name}: {refused}");
            let active = [SkillActivation::from_skill(catalog.lookup(name).unwrap())];
            let elsewhere = [SkillActivation::new(
                name.to_owned(),
                PathBuf::from("/elsewhere").join(name),
                "---\ndescription: Other\n---\nbody".to_owned(),
                SkillOrigin::User,
                None,
            )
            .unwrap()];
            for (tool, path) in [
                (ToolName::Read, format!("skill://{name}/SKILL.md")),
                (ToolName::ListDir, format!("skill://{name}/")),
            ] {
                assert_eq!(
                    skill_resource(&host, tool, &path, &[]).await,
                    Err(refused.clone()),
                    "{path}"
                );
                assert!(
                    skill_resource(&host, tool, &path, &active).await.is_ok(),
                    "{path}"
                );
                assert_eq!(
                    skill_resource(&host, tool, &path, &elsewhere).await,
                    Err(refused.clone()),
                    "{path}"
                );
            }
        }
    }

    #[tokio::test]
    async fn load_skill_ledger_captures_body_outside_ack_and_enforces_combined_limit() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        for index in 0..5 {
            let name = format!("large-{index}");
            write_skill(
                &home.path().join(".agents/skills"),
                &name,
                &format!("name: {name}\ndescription: Large\n"),
                &"BODY_SENTINEL".repeat(4_700),
            );
        }
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace).await.with_skill_catalog(catalog);
        let mut ledger = SkillLoadLedger::new(&[]).unwrap();
        for index in 0..4 {
            let name = format!("large-{index}");
            let ack = host
                .load_skill(
                    ToolValue::List(vec![ToolValue::String(name.clone())]),
                    &mut ledger,
                )
                .unwrap();
            let ToolValue::String(ack) = ack else {
                panic!("load_skill must return a string acknowledgement")
            };
            assert!(ack.contains(&format!("skill://{name}/")));
            assert!(!ack.contains("BODY_SENTINEL"));
        }
        assert_eq!(ledger.captured().len(), 4);
        let error = host
            .load_skill(
                ToolValue::List(vec![ToolValue::String("large-4".to_owned())]),
                &mut ledger,
            )
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert_eq!(ledger.captured().len(), 4);
    }

    #[tokio::test]
    async fn load_skill_lookup_rejects_direct_only_invalid_ambiguous_and_skipped_project() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let user_root = home.path().join(".agents/skills");
        for (directory, frontmatter, body) in [
            (
                "direct",
                "name: direct\ndescription: Direct\ndisable-model-invocation: true\n",
                "body",
            ),
            ("invalid", "name: invalid\n", "body"),
            ("one", "name: shared\ndescription: One\n", "body"),
            ("two", "name: shared\ndescription: Two\n", "body"),
        ] {
            write_skill(&user_root, directory, frontmatter, body);
        }
        write_skill(
            &workspace.path().join(".agents/skills"),
            "project",
            "name: project\ndescription: Project\n",
            "body",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace).await.with_skill_catalog(catalog);
        let mut ledger = SkillLoadLedger::new(&[]).unwrap();
        for (name, reason) in [
            ("direct", "directly by the user"),
            ("invalid", "invalid"),
            ("shared", "ambiguous"),
            ("project", "unavailable"),
            ("missing", "unknown"),
        ] {
            let error = host
                .load_skill(
                    ToolValue::List(vec![ToolValue::String(name.to_owned())]),
                    &mut ledger,
                )
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::Value);
            assert!(error.to_string().contains(reason), "{name}: {error}");
        }
        assert!(ledger.captured().is_empty());
    }

    #[tokio::test]
    async fn load_skill_ack_omits_unsafe_root_path_and_is_allowed_in_read_only_host() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "\u{202e}hidden",
            "name: safe\ndescription: Safe\n",
            "body",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace)
            .await
            .with_skill_catalog(catalog)
            .for_child(true);
        let mut ledger = SkillLoadLedger::new(&[]).unwrap();
        let result = host
            .load_skill(
                ToolValue::List(vec![ToolValue::String("safe".to_owned())]),
                &mut ledger,
            )
            .unwrap();
        let ToolValue::String(ack) = result else {
            panic!("load_skill must return a string")
        };
        assert!(ack.contains("skill://safe/"));
        assert!(ack.contains("root path unavailable"));
        assert!(!ack.contains('\u{202e}'));
        assert_eq!(ledger.captured().len(), 1);
    }

    #[tokio::test]
    async fn load_skill_uses_process_catalog_snapshot_after_source_changes() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let skills = home.path().join(".agents/skills");
        write_skill(
            &skills,
            "review",
            "name: review\ndescription: Review\n",
            "FIRST_BODY",
        );
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let old_catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let old_host = tool_host(&workspace).await.with_skill_catalog(old_catalog);

        write_skill(
            &skills,
            "review",
            "name: review\ndescription: Review\n",
            "SECOND_BODY",
        );
        let new_catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let new_host = tool_host(&workspace).await.with_skill_catalog(new_catalog);
        let mut old_ledger = SkillLoadLedger::new(&[]).unwrap();
        old_host
            .load_skill(
                ToolValue::List(vec![ToolValue::String("review".to_owned())]),
                &mut old_ledger,
            )
            .unwrap();
        let mut new_ledger = SkillLoadLedger::new(old_ledger.captured()).unwrap();
        assert!(
            new_ledger
                .active
                .iter()
                .next()
                .unwrap()
                .source_text()
                .contains("FIRST_BODY")
        );
        assert!(new_ledger.captured().is_empty());
        new_host
            .load_skill(
                ToolValue::List(vec![ToolValue::String("review".to_owned())]),
                &mut new_ledger,
            )
            .unwrap();
        assert!(
            old_ledger.captured()[0]
                .source_text()
                .contains("FIRST_BODY")
        );
        assert!(
            new_ledger.captured()[0]
                .source_text()
                .contains("SECOND_BODY")
        );
        assert!(
            new_ledger
                .active
                .iter()
                .next()
                .unwrap()
                .source_text()
                .contains("SECOND_BODY")
        );
    }

    #[tokio::test]
    async fn load_skill_reports_catalog_validation_reason() {
        let home = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let root = home.path().join(".agents/skills/oversized");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            format!(
                "---\nname: oversized\ndescription: Large\n---\n{}",
                "x".repeat(crate::skills::MAX_SKILL_BYTES)
            ),
        )
        .unwrap();
        let scope = ProjectScope::discover(workspace.path()).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&scope, home.path(), false, ""));
        let host = tool_host(&workspace).await.with_skill_catalog(catalog);
        let mut ledger = SkillLoadLedger::new(&[]).unwrap();
        let error = host
            .load_skill(
                ToolValue::List(vec![ToolValue::String("oversized".to_owned())]),
                &mut ledger,
            )
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(error.to_string().contains("exceeds 65536 bytes"));
    }

    #[tokio::test]
    async fn host_without_skills_reports_unknown_skill_for_load_and_resources() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;
        let unknown = ToolError::value("unknown or unavailable skill \"review\"");
        let error = host
            .load_skill(
                ToolValue::List(vec![ToolValue::String("review".to_owned())]),
                &mut SkillLoadLedger::new(&[]).unwrap(),
            )
            .unwrap_err();
        assert_eq!(error, unknown);
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::Read,
                arguments: ToolValue::List(vec![ToolValue::String(
                    "skill://review/SKILL.md".to_owned(),
                )]),
            })
            .await;
        assert_eq!(result.result, Err(unknown));
    }

    async fn tool_host_with_limits(workspace: &TempDir, max_bytes: usize) -> ToolHost {
        ToolHost::with_limits(workspace.path(), max_bytes)
            .await
            .unwrap()
    }

    async fn tool_host_with_collection_limits(
        workspace: &TempDir,
        max_items: usize,
        max_payload_bytes: usize,
    ) -> ToolHost {
        ToolHost::with_collection_limits(workspace.path(), max_items, max_payload_bytes)
            .await
            .unwrap()
    }

    async fn read(host: &ToolHost, path: &str) -> Result<String, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::Read,
                arguments: ToolValue::List(vec![ToolValue::String(path.to_owned())]),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::String(contents)) => Ok(contents),
            Ok(value) => panic!("read returned {value:?} instead of a string"),
            Err(error) => Err(error.kind),
        }
    }

    async fn read_error(host: &ToolHost, path: &str) -> ToolErrorKind {
        read(host, path).await.unwrap_err()
    }

    async fn skill_resource(
        host: &ToolHost,
        tool: ToolName,
        path: &str,
        active_skills: &[SkillActivation],
    ) -> Result<ToolValue, ToolError> {
        host.call_with_authorisation_and_output(
            ToolCall {
                call_id: 7,
                tool,
                arguments: ToolValue::List(vec![ToolValue::String(path.to_owned())]),
            },
            None,
            None,
            active_skills,
        )
        .await
        .result
    }

    async fn list_dir(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::ListDir,
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::List(entries)) => Ok(entries),
            Ok(value) => panic!("list_dir returned {value:?} instead of a list"),
            Err(error) => Err(error.kind),
        }
    }

    async fn list_dir_path(host: &ToolHost, path: &str) -> Result<Vec<ToolValue>, ToolErrorKind> {
        list_dir(host, vec![ToolValue::String(path.to_owned())]).await
    }

    async fn list_dir_error(host: &ToolHost, path: &str) -> ToolErrorKind {
        list_dir_path(host, path).await.unwrap_err()
    }

    fn glob_tool() -> ToolName {
        ToolName::from_private_name("_kiki_glob").expect("glob must be an active host tool")
    }

    fn grep_tool() -> ToolName {
        ToolName::from_private_name("_kiki_grep").expect("grep must be an active host tool")
    }

    async fn glob(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: glob_tool(),
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::List(entries)) => Ok(entries),
            Ok(value) => panic!("glob returned {value:?} instead of a list"),
            Err(error) => Err(error.kind),
        }
    }

    async fn glob_path(
        host: &ToolHost,
        pattern: &str,
        path: &str,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        glob(
            host,
            vec![
                ToolValue::String(pattern.to_owned()),
                ToolValue::String(path.to_owned()),
            ],
        )
        .await
    }

    async fn glob_error(host: &ToolHost, pattern: &str, path: &str) -> ToolErrorKind {
        glob_path(host, pattern, path).await.unwrap_err()
    }

    async fn glob_failure(host: &ToolHost, pattern: &str, path: &str) -> ToolError {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: glob_tool(),
                arguments: ToolValue::List(vec![
                    ToolValue::String(pattern.to_owned()),
                    ToolValue::String(path.to_owned()),
                ]),
            })
            .await;
        result.result.unwrap_err()
    }

    async fn grep(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: grep_tool(),
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        match result.result {
            Ok(ToolValue::List(matches)) => Ok(matches),
            Ok(value) => panic!("grep returned {value:?} instead of a list"),
            Err(error) => Err(error.kind),
        }
    }

    async fn grep_path(
        host: &ToolHost,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
    ) -> Result<Vec<ToolValue>, ToolErrorKind> {
        let mut arguments = vec![
            ToolValue::String(pattern.to_owned()),
            ToolValue::String(path.to_owned()),
        ];
        if let Some(file_glob) = file_glob {
            arguments.push(ToolValue::String(file_glob.to_owned()));
        }
        grep(host, arguments).await
    }

    async fn grep_error(
        host: &ToolHost,
        pattern: &str,
        path: &str,
        file_glob: Option<&str>,
    ) -> ToolErrorKind {
        grep_path(host, pattern, path, file_glob).await.unwrap_err()
    }

    async fn run_call(host: &ToolHost, arguments: Vec<ToolValue>) -> Result<ToolValue, ToolError> {
        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::Run,
                arguments: ToolValue::List(arguments),
            })
            .await;
        assert_eq!(result.call_id, 7);
        result.result
    }

    async fn run_tool(
        host: &ToolHost,
        arguments: Vec<ToolValue>,
    ) -> Result<ToolValue, ToolErrorKind> {
        run_call(host, arguments).await.map_err(|error| error.kind)
    }

    async fn run_error(host: &ToolHost, arguments: Vec<ToolValue>) -> ToolErrorKind {
        run_tool(host, arguments).await.unwrap_err()
    }

    fn directory_entry(path: &str, kind: &str, size_bytes: Option<i64>) -> ToolValue {
        ToolValue::Dictionary(BTreeMap::from([
            ("path".to_owned(), ToolValue::String(path.to_owned())),
            ("kind".to_owned(), ToolValue::String(kind.to_owned())),
            (
                "size_bytes".to_owned(),
                size_bytes.map_or(ToolValue::None, ToolValue::Integer),
            ),
        ]))
    }

    fn grep_match(path: &str, line: i64, text: &str) -> ToolValue {
        ToolValue::Dictionary(BTreeMap::from([
            ("path".to_owned(), ToolValue::String(path.to_owned())),
            ("line".to_owned(), ToolValue::Integer(line)),
            ("text".to_owned(), ToolValue::String(text.to_owned())),
        ]))
    }

    #[cfg(unix)]
    #[test]
    fn direct_content_open_refuses_symlinks_and_classifies_opened_descriptors() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "workspace").unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("file", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        let workspace_dir = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

        let mut file = open_content_no_follow(&workspace_dir, Path::new("file")).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "workspace");
        assert!(file.metadata().unwrap().is_file());

        let directory = open_content_no_follow(&workspace_dir, Path::new("directory")).unwrap();
        assert!(directory.metadata().unwrap().is_dir());

        let fifo = open_content_no_follow(&workspace_dir, Path::new("fifo")).unwrap();
        assert!(!fifo.metadata().unwrap().is_file());

        for name in ["inside-link", "outside-link", "broken-link"] {
            assert_eq!(
                open_content_no_follow(&workspace_dir, Path::new(name))
                    .unwrap_err()
                    .kind,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            open_content_no_follow(&workspace_dir, Path::new("missing"))
                .unwrap_err()
                .kind,
            ToolErrorKind::FileNotFound
        );
        assert_eq!(
            open_content_no_follow(&workspace_dir, Path::new("socket"))
                .unwrap_err()
                .kind,
            ToolErrorKind::Value
        );
    }

    #[cfg(unix)]
    #[test]
    fn direct_parent_open_refuses_intermediate_symlinks() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::write(workspace.path().join("inside/file"), "workspace").unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        let workspace_dir = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

        for path in ["inside-link/file", "outside-link/outside"] {
            assert_eq!(
                open_parent(&workspace_dir, Path::new(path))
                    .unwrap_err()
                    .kind,
                ToolErrorKind::Permission
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn direct_open_stays_anchored_after_parent_rename_and_component_swaps() {
        use std::io::Read;
        use std::sync::atomic::{AtomicBool, Ordering};

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let parent_path = workspace.path().join("parent");
        fs::create_dir(&parent_path).unwrap();
        fs::write(parent_path.join("file"), "workspace").unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        let workspace_dir = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();

        let (parent, file_name) = open_parent(&workspace_dir, Path::new("parent/file")).unwrap();
        fs::rename(&parent_path, workspace.path().join("held")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &parent_path).unwrap();
        let mut file = open_content_no_follow(&parent, &file_name).unwrap();
        let mut contents = String::new();
        file.read_to_string(&mut contents).unwrap();
        assert_eq!(contents, "workspace");
        fs::remove_file(&parent_path).unwrap();
        fs::rename(workspace.path().join("held"), &parent_path).unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let swapping_workspace = workspace.path().to_path_buf();
        let swapping_outside = outside.path().to_path_buf();
        let swapping_done = Arc::clone(&done);
        let swaps = std::thread::spawn(move || {
            let parent = swapping_workspace.join("parent");
            let held = swapping_workspace.join("held");
            for _ in 0..1_000 {
                if fs::rename(&parent, &held).is_err() {
                    continue;
                }
                std::os::unix::fs::symlink(&swapping_outside, &parent).unwrap();
                std::thread::yield_now();
                fs::remove_file(&parent).unwrap();
                fs::rename(&held, &parent).unwrap();
            }
            swapping_done.store(true, Ordering::Release);
        });

        while !done.load(Ordering::Acquire) {
            let Ok((parent, file_name)) = open_parent(&workspace_dir, Path::new("parent/file"))
            else {
                continue;
            };
            let Ok(mut file) = open_content_no_follow(&parent, &file_name) else {
                continue;
            };
            let mut contents = String::new();
            file.read_to_string(&mut contents).unwrap();
            assert_eq!(contents, "workspace");
        }
        swaps.join().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_maps_direct_root_kinds_to_distinct_errors() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        let host = tool_host(&workspace).await;

        for path in ["missing"] {
            assert_eq!(
                list_dir_error(&host, path).await,
                ToolErrorKind::FileNotFound
            );
        }
        for path in ["file", "fifo", "socket"] {
            assert_eq!(
                list_dir_error(&host, path).await,
                ToolErrorKind::NotADirectory
            );
        }
        for path in ["inside-link", "outside-link", "broken-link"] {
            assert_eq!(list_dir_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_lists_sorted_immediate_entries_without_filtering() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("z"), "z").unwrap();
        fs::write(workspace.path().join("a-file"), "file").unwrap();
        fs::create_dir(workspace.path().join("b-directory")).unwrap();
        fs::write(workspace.path().join(".hidden"), "hidden").unwrap();
        fs::write(workspace.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(workspace.path().join("ignored.txt"), "ignored").unwrap();
        std::os::unix::fs::symlink("a-file", workspace.path().join("c-link")).unwrap();
        create_fifo(&workspace.path().join("d-fifo"));
        let host = tool_host(&workspace).await;

        let entries = tokio::time::timeout(Duration::from_millis(100), list_dir(&host, Vec::new()))
            .await
            .expect("listing a FIFO must not block")
            .unwrap();

        assert_eq!(
            entries,
            [
                directory_entry(".gitignore", "file", Some(12)),
                directory_entry(".hidden", "file", Some(6)),
                directory_entry("a-file", "file", Some(4)),
                directory_entry("b-directory", "directory", None),
                directory_entry("c-link", "symlink", None),
                directory_entry("d-fifo", "other", None),
                directory_entry("ignored.txt", "file", Some(7)),
                directory_entry("z", "file", Some(1)),
            ]
        );
    }

    #[tokio::test]
    async fn list_dir_normalises_workspace_relative_output_paths() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("nested/file"), "contents").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            list_dir_path(&host, "./nested//.").await.unwrap(),
            [directory_entry("nested/file", "file", Some(8))]
        );
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(list_dir_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn list_dir_refuses_inside_and_outside_intermediate_symlinks() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        let host = tool_host(&workspace).await;

        for path in ["inside-link/file", "outside-link/file"] {
            assert_eq!(list_dir_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn list_dir_rejects_any_non_utf8_entry_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("valid"), "contents").unwrap();
        fs::write(
            workspace.path().join(OsStr::from_bytes(b"\xff")),
            "contents",
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(list_dir(&host, Vec::new()).await, Err(ToolErrorKind::Value));
    }

    #[tokio::test]
    async fn list_dir_enforces_exact_item_and_payload_limits_without_results() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("aa"), "").unwrap();
        fs::write(workspace.path().join("bb"), "").unwrap();

        let at_item_limit = tool_host_with_collection_limits(&workspace, 2, 12).await;
        assert_eq!(
            list_dir(&at_item_limit, Vec::new()).await.unwrap(),
            [
                directory_entry("aa", "file", Some(0)),
                directory_entry("bb", "file", Some(0)),
            ]
        );

        let over_item_limit = tool_host_with_collection_limits(&workspace, 1, 12).await;
        assert_eq!(
            list_dir(&over_item_limit, Vec::new()).await,
            Err(ToolErrorKind::Runtime)
        );

        let at_payload_limit = tool_host_with_collection_limits(&workspace, 2, 12).await;
        assert_eq!(
            list_dir(&at_payload_limit, Vec::new()).await.unwrap().len(),
            2
        );
        let over_payload_limit = tool_host_with_collection_limits(&workspace, 2, 11).await;
        assert_eq!(
            list_dir(&over_payload_limit, Vec::new()).await,
            Err(ToolErrorKind::Runtime)
        );
    }

    #[tokio::test]
    async fn list_dir_rejects_malformed_arguments_as_type_errors() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        for arguments in [
            ToolValue::None,
            ToolValue::Bool(true),
            ToolValue::Integer(1),
            ToolValue::Float(1.0),
            ToolValue::String("path".to_owned()),
            ToolValue::List(vec![ToolValue::None]),
            ToolValue::List(vec![ToolValue::String("a".to_owned()), ToolValue::None]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: ToolName::ListDir,
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_list_dir_does_not_stop_a_read_future() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        let gate = super::BlockingTestGate::new();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_list_dir_test_gate(gate.clone()),
        );
        let list_host = Arc::clone(&host);
        let mut listing = tokio::spawn(async move { list_dir(&list_host, Vec::new()).await });

        gate.wait_until_started().await;
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), read(&host, "file"))
                .await
                .expect("read should make progress while listing is blocked")
                .unwrap(),
            "contents"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut listing)
                .await
                .is_err()
        );
        gate.resume();
        assert_eq!(listing.await.unwrap().unwrap().len(), 1);
    }

    fn batch_read(call_id: u32, path: impl Into<String>) -> ToolCall {
        ToolCall {
            call_id,
            tool: ToolName::Read,
            arguments: ToolValue::List(vec![ToolValue::String(path.into())]),
        }
    }

    fn batch_write(call_id: u32, path: impl Into<String>, content: impl Into<String>) -> ToolCall {
        ToolCall {
            call_id,
            tool: ToolName::Write,
            arguments: ToolValue::List(vec![
                ToolValue::String(path.into()),
                ToolValue::String(content.into()),
            ]),
        }
    }

    fn batch_edit(
        call_id: u32,
        path: impl Into<String>,
        old: impl Into<String>,
        new: impl Into<String>,
    ) -> ToolCall {
        ToolCall {
            call_id,
            tool: ToolName::Edit,
            arguments: ToolValue::List(vec![
                ToolValue::String(path.into()),
                ToolValue::String(old.into()),
                ToolValue::String(new.into()),
            ]),
        }
    }

    fn batch_run(call_id: u32, command: impl Into<String>, cwd: Option<&str>) -> ToolCall {
        let mut arguments = vec![ToolValue::String(command.into())];
        if let Some(cwd) = cwd {
            arguments.push(ToolValue::String(cwd.to_owned()));
        }
        ToolCall {
            call_id,
            tool: ToolName::Run,
            arguments: ToolValue::List(arguments),
        }
    }

    #[tokio::test]
    async fn dispatch_batch_serialises_lexical_aliases_and_advances_freshness() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "first").unwrap();
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(
            normalise_path("file").unwrap(),
            content_fingerprint(b"first"),
        );
        let mut calls = Box::pin(host.dispatch_batch(
            vec![
                batch_write(1, "./file", "second"),
                batch_edit(2, "file", "second", "third"),
            ],
            authorisations,
        ));

        let mut first = Box::pin(calls.next());
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut first => panic!("batch finished before the first mutation started: {result:?}"),
        }
        assert_eq!(gate.started_call_ids(), vec![1]);
        gate.release(1);
        assert_eq!(first.await.unwrap().result, Ok(ToolValue::None));
        let mut second = Box::pin(calls.next());
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut second => panic!("batch finished before the second mutation started: {result:?}"),
        }
        assert_eq!(gate.started_call_ids(), vec![1, 2]);
        gate.release(2);
        assert_eq!(second.await.unwrap().result, Ok(ToolValue::None));
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "third"
        );
    }

    #[tokio::test]
    async fn queued_same_path_mutations_leave_slots_for_independent_calls() {
        let workspace = TempDir::new().unwrap();
        for call_id in 3..=17 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                std::iter::once(batch_write(1, "shared", "first"))
                    .chain(std::iter::once(batch_write(2, "./shared", "second")))
                    .chain((3..=17).map(|call_id| batch_read(call_id, format!("file-{call_id}"))))
                    .collect(),
                WholeFileAuthorisations::default(),
            ),
        );

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = calls.next() => panic!("batch finished before its active calls started: {result:?}"),
        }
        let started = gate.started_call_ids().into_iter().collect::<BTreeSet<_>>();
        assert_eq!(started.len(), 16);
        assert!(started.contains(&1));
        assert!(!started.contains(&2));
        assert!((3..=17).all(|call_id| started.contains(&call_id)));
        assert_eq!(gate.active_call_ids().len(), 16);
        assert_eq!(gate.maximum_active(), 16);

        drop(calls);
        assert!(gate.active_call_ids().is_empty());
    }

    #[tokio::test]
    async fn failed_same_path_mutations_settle_without_losing_later_freshness() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "first").unwrap();
        let host = tool_host(&workspace).await;
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(
            normalise_path("file").unwrap(),
            content_fingerprint(b"first"),
        );

        let results = host
            .dispatch_batch(
                vec![
                    batch_write(1, "file", "second"),
                    batch_edit(2, "./file", "missing", "third"),
                    batch_write(3, "file", "fourth"),
                ],
                authorisations,
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(results.len(), 3);
        assert!(results[0].result.is_ok());
        assert!(matches!(
            results[1].result,
            Err(ref error) if error.kind == ToolErrorKind::Value
        ));
        assert!(results[2].result.is_ok());
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "fourth"
        );
    }

    #[tokio::test]
    async fn dispatch_batch_starts_different_path_mutations_together() {
        let workspace = TempDir::new().unwrap();
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(host.dispatch_batch(
            vec![
                batch_write(1, "first", "one"),
                batch_write(2, "second", "two"),
            ],
            WholeFileAuthorisations::default(),
        ));

        let mut results = Box::pin(calls.as_mut().collect::<Vec<_>>());
        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = &mut results => panic!("batch finished before both mutations started: {result:?}"),
        }
        assert_eq!(gate.active_call_ids().len(), 2);
        gate.release(1);
        gate.release(2);
        assert!(
            results
                .await
                .into_iter()
                .all(|result| result.result == Ok(ToolValue::None))
        );
    }

    #[tokio::test]
    async fn dispatch_batch_yields_completion_order_with_correlated_call_ids() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("first"), "first").unwrap();
        fs::write(workspace.path().join("second"), "second").unwrap();
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(host.dispatch_batch(
            vec![batch_read(1, "first"), batch_read(2, "second")],
            WholeFileAuthorisations::default(),
        ));

        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = calls.next() => panic!("batch finished before both calls started: {result:?}"),
        }
        gate.release(2);
        let second = calls.next().await.unwrap();
        assert_eq!(second.call_id, 2);
        assert_eq!(second.result, Ok(ToolValue::String("second".to_owned())));
        gate.release(1);
        let first = calls.next().await.unwrap();
        assert_eq!(first.call_id, 1);
        assert_eq!(first.result, Ok(ToolValue::String("first".to_owned())));
    }

    #[tokio::test]
    async fn dispatch_batch_limits_active_calls_and_admits_queued_calls() {
        let workspace = TempDir::new().unwrap();
        for call_id in 1..=17 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                (1..=17)
                    .map(|call_id| batch_read(call_id, format!("file-{call_id}")))
                    .collect(),
                WholeFileAuthorisations::default(),
            ),
        );

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = calls.next() => panic!("batch finished before 16 calls started: {result:?}"),
        }
        assert_eq!(gate.active_call_ids().len(), 16);
        assert_eq!(gate.maximum_active(), 16);

        let released_call = *gate
            .started_call_ids()
            .first()
            .expect("a batch call must have started");
        gate.release(released_call);
        assert_eq!(calls.next().await.unwrap().call_id, released_call);
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = calls.next() => panic!("batch finished before the queued call started: {result:?}"),
        }
        assert_eq!(gate.maximum_active(), 16);

        for call_id in gate.active_call_ids() {
            gate.release(call_id);
        }
        let mut call_ids = calls.map(|result| result.call_id).collect::<Vec<_>>().await;
        call_ids.push(released_call);
        call_ids.sort_unstable();
        assert_eq!(call_ids, (1..=17).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn dispatch_batch_settles_queued_siblings_after_an_ordinary_failure() {
        let workspace = TempDir::new().unwrap();
        for call_id in 2..=17 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                std::iter::once(batch_read(1, "missing"))
                    .chain((2..=17).map(|call_id| batch_read(call_id, format!("file-{call_id}"))))
                    .collect(),
                WholeFileAuthorisations::default(),
            ),
        );
        let mut results = Box::pin(calls.as_mut().collect::<Vec<_>>());

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = &mut results => panic!("batch finished before 16 calls started: {result:?}"),
        }
        for call_id in gate.started_call_ids() {
            gate.release(call_id);
        }
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut results => panic!("batch finished before the queued sibling started: {result:?}"),
        }
        let queued_call = *gate
            .started_call_ids()
            .last()
            .expect("queued sibling must have started");
        gate.release(queued_call);

        let results = results.await;
        assert_eq!(results.len(), 17);
        assert!(matches!(
            results.iter().find(|result| result.call_id == 1),
            Some(result) if result.result.is_err()
        ));
        assert!(
            results
                .iter()
                .filter(|result| result.call_id != 1)
                .all(|result| result.result.is_ok())
        );
    }

    #[tokio::test]
    async fn dropping_a_partly_consumed_batch_drops_active_calls_and_leaves_calls_queued() {
        let workspace = TempDir::new().unwrap();
        for call_id in 1..=18 {
            fs::write(
                workspace.path().join(format!("file-{call_id}")),
                call_id.to_string(),
            )
            .unwrap();
        }
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut calls = Box::pin(
            host.dispatch_batch(
                (1..=18)
                    .map(|call_id| batch_read(call_id, format!("file-{call_id}")))
                    .collect(),
                WholeFileAuthorisations::default(),
            ),
        );

        tokio::select! {
            _ = gate.wait_until_calls_started(16) => {}
            result = calls.next() => panic!("batch finished before 16 calls started: {result:?}"),
        }
        let released_call = *gate
            .started_call_ids()
            .first()
            .expect("a batch call must have started");
        gate.release(released_call);
        assert_eq!(calls.next().await.unwrap().call_id, released_call);
        let mut next_result = Box::pin(calls.next());
        tokio::select! {
            _ = gate.wait_until_calls_started(1) => {}
            result = &mut next_result => panic!("batch finished before another call started: {result:?}"),
        }
        drop(next_result);
        drop(calls);

        assert!(gate.active_call_ids().is_empty());
        assert_eq!(gate.started_call_ids(), (1..=17).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn test_gate_pauses_a_read_until_released() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("paused"), "contents").unwrap();
        let gate = super::ReadTestGate::new();
        let host = tool_host(&workspace).await.with_test_gate(gate.clone());
        let mut read = tokio::spawn(async move { read(&host, "paused").await });

        gate.wait_until_read_started().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut read)
                .await
                .is_err()
        );
        gate.resume_read();

        assert_eq!(read.await.unwrap().unwrap(), "contents");
    }

    #[tokio::test]
    async fn reads_one_exact_utf8_string_without_line_limits() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;
        let cases = [
            ("lf", "first\nsecond\n"),
            ("crlf", "first\r\nsecond\r\n"),
            ("no-final-newline", "first\nsecond"),
            ("empty", ""),
        ];

        for (name, contents) in cases {
            fs::write(workspace.path().join(name), contents).unwrap();
            assert_eq!(read(&host, name).await.unwrap(), contents);
        }

        let many_lines = "line\n".repeat(100_001);
        fs::write(workspace.path().join("many-lines"), &many_lines).unwrap();
        assert_eq!(read(&host, "many-lines").await.unwrap(), many_lines);
    }

    #[tokio::test]
    async fn normalises_lexical_aliases_and_denies_escapes() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("nested/file"), "contents").unwrap();
        let host = tool_host(&workspace).await;

        for path in ["nested/file", "./nested/file", "nested//./file"] {
            assert_eq!(read(&host, path).await.unwrap(), "contents");
        }
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(read_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_final_symlinks_inside_and_outside_the_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("inside"), "inside").unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        for path in ["inside-link", "outside-link"] {
            assert_eq!(read_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_intermediate_symlinks_inside_and_outside_the_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::write(workspace.path().join("inside/file"), "inside").unwrap();
        fs::write(outside.path().join("file"), "outside").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        let host = tool_host(&workspace).await;

        for path in ["inside-link/file", "outside-link/file"] {
            assert_eq!(read_error(&host, path).await, ToolErrorKind::Permission);
        }
    }

    #[tokio::test]
    async fn maps_read_path_kinds_to_distinct_errors() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("invalid"), [0xff]).unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;
        fs::write(workspace.path().join("large"), "12345").unwrap();

        assert_eq!(
            read_error(&host, "missing").await,
            ToolErrorKind::FileNotFound
        );
        assert_eq!(
            read_error(&host, "directory").await,
            ToolErrorKind::IsADirectory
        );
        assert_eq!(read_error(&host, "invalid").await, ToolErrorKind::Value);
        assert_eq!(read_error(&host, "large").await, ToolErrorKind::Value);
    }

    #[cfg(unix)]
    fn create_fifo(path: &Path) {
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_fifo_without_waiting_for_a_writer() {
        let workspace = TempDir::new().unwrap();
        let fifo = workspace.path().join("fifo");
        create_fifo(&fifo);
        let host = tool_host(&workspace).await;

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), read_error(&host, "fifo"))
                .await
                .unwrap(),
            ToolErrorKind::Value
        );
    }

    #[tokio::test]
    async fn enforces_the_exact_byte_boundary() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("at-limit"), "1234").unwrap();
        fs::write(workspace.path().join("over-limit"), "12345").unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;

        assert_eq!(read(&host, "at-limit").await.unwrap(), "1234");
        assert_eq!(read_error(&host, "over-limit").await, ToolErrorKind::Value);
    }

    #[tokio::test]
    async fn rejects_malformed_arguments_as_type_errors() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        for arguments in [
            ToolValue::None,
            ToolValue::Bool(true),
            ToolValue::Integer(1),
            ToolValue::Float(1.0),
            ToolValue::String("path".to_owned()),
            ToolValue::List(Vec::new()),
            ToolValue::List(vec![ToolValue::String("path".to_owned()), ToolValue::None]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: ToolName::Read,
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[tokio::test]
    async fn glob_rejects_invalid_patterns_and_malformed_arguments() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(glob_error(&host, "[", ".").await, ToolErrorKind::Value);
        for arguments in [
            ToolValue::None,
            ToolValue::String("*.rs".to_owned()),
            ToolValue::List(Vec::new()),
            ToolValue::List(vec![ToolValue::String("*.rs".to_owned()), ToolValue::None]),
            ToolValue::List(vec![
                ToolValue::String("*.rs".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::String("extra".to_owned()),
            ]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: glob_tool(),
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_maps_direct_root_kinds_and_intermediate_symlinks_to_errors() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "contents").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_error(&host, "*", "missing").await,
            ToolErrorKind::FileNotFound
        );
        for path in ["file", "fifo", "socket"] {
            assert_eq!(
                glob_error(&host, "*", path).await,
                ToolErrorKind::NotADirectory
            );
        }
        for path in [
            "inside-link",
            "outside-link",
            "broken-link",
            "inside-link/file",
            "outside-link/file",
        ] {
            assert_eq!(
                glob_error(&host, "*", path).await,
                ToolErrorKind::Permission
            );
        }
    }

    #[tokio::test]
    async fn glob_matches_root_relative_and_basename_patterns() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("src/nested/deeper")).unwrap();
        fs::write(workspace.path().join("src/direct.rs"), "").unwrap();
        fs::write(workspace.path().join("src/nested/mod.rs"), "").unwrap();
        fs::write(workspace.path().join("src/nested/deeper/lib.rs"), "").unwrap();
        fs::write(workspace.path().join("src/nested/mod.py"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "nested/*.rs", "src").await.unwrap(),
            [directory_entry("src/nested/mod.rs", "file", Some(0))]
        );
        assert_eq!(
            glob_path(&host, "nested/deeper/*.rs", "src").await.unwrap(),
            [directory_entry("src/nested/deeper/lib.rs", "file", Some(0))]
        );
        assert_eq!(
            glob_path(&host, "*.rs", "src").await.unwrap(),
            [
                directory_entry("src/direct.rs", "file", Some(0)),
                directory_entry("src/nested/deeper/lib.rs", "file", Some(0)),
                directory_entry("src/nested/mod.rs", "file", Some(0)),
            ]
        );
    }

    #[tokio::test]
    async fn glob_filters_hidden_descendants_but_traverses_an_explicit_hidden_root() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join(".hidden/nested")).unwrap();
        fs::create_dir(workspace.path().join("visible")).unwrap();
        fs::write(workspace.path().join(".hidden/kept.rs"), "").unwrap();
        fs::write(workspace.path().join(".hidden/nested/.hidden.rs"), "").unwrap();
        fs::write(workspace.path().join("visible/.hidden.rs"), "").unwrap();
        fs::write(workspace.path().join("visible/kept.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "*.rs", ".").await.unwrap(),
            [directory_entry("visible/kept.rs", "file", Some(0))]
        );
        assert_eq!(
            glob_path(&host, "*.rs", ".hidden").await.unwrap(),
            [directory_entry(".hidden/kept.rs", "file", Some(0))]
        );
    }

    #[tokio::test]
    async fn glob_uses_nearest_rgignore_rule_and_negation() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join("ignored.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/.rgignore"), "!kept.rs\n").unwrap();
        fs::write(workspace.path().join("nested/ignored.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/kept.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "*.rs", ".").await.unwrap(),
            [directory_entry("nested/kept.rs", "file", Some(0))]
        );
    }

    #[tokio::test]
    async fn glob_prefers_ancestor_rgignore_over_nested_gitignore_whitelist() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "candidate.rs\n").unwrap();
        fs::write(
            workspace.path().join("nested/.gitignore"),
            "!candidate.rs\n",
        )
        .unwrap();
        fs::write(workspace.path().join("nested/candidate.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn glob_prefers_root_ignore_over_root_and_nested_gitignore() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".ignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join(".gitignore"), "!root.rs\n").unwrap();
        fs::write(workspace.path().join("nested/.gitignore"), "!nested.rs\n").unwrap();
        fs::write(workspace.path().join("root.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/nested.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn glob_prefers_rgignore_over_ignore_and_gitignore() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join(".gitignore"), "candidate.rs\n").unwrap();
        fs::write(workspace.path().join(".ignore"), "!candidate.rs\n").unwrap();
        fs::write(workspace.path().join(".rgignore"), "candidate.rs\n").unwrap();
        fs::write(workspace.path().join("candidate.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn glob_rejects_oversized_or_invalid_ignore_files_without_paths() {
        let oversized = TempDir::new().unwrap();
        fs::write(
            oversized.path().join(".rgignore"),
            vec![b'x'; FILE_MAX_BYTES + 1],
        )
        .unwrap();
        fs::write(oversized.path().join("candidate.rs"), "").unwrap();
        let error = glob_failure(&tool_host(&oversized).await, "*.rs", ".").await;
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(
            !error
                .to_string()
                .contains(&oversized.path().display().to_string())
        );

        let invalid_utf8 = TempDir::new().unwrap();
        fs::write(invalid_utf8.path().join(".ignore"), [0xff]).unwrap();
        fs::write(invalid_utf8.path().join("candidate.rs"), "").unwrap();
        let error = glob_failure(&tool_host(&invalid_utf8).await, "*.rs", ".").await;
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(
            !error
                .to_string()
                .contains(&invalid_utf8.path().display().to_string())
        );

        let malformed = TempDir::new().unwrap();
        fs::write(malformed.path().join(".gitignore"), "[z-a]\n").unwrap();
        fs::write(malformed.path().join("candidate.rs"), "").unwrap();
        let error = glob_failure(&tool_host(&malformed).await, "*.rs", ".").await;
        assert_eq!(error.kind, ToolErrorKind::Value);
        assert!(
            !error
                .to_string()
                .contains(&malformed.path().display().to_string())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_treats_linked_and_non_regular_ignore_files_as_absent() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("candidate.rs"), "").unwrap();
        fs::write(workspace.path().join("nested/candidate.rs"), "").unwrap();
        fs::write(outside.path().join("rules"), "*.rs\n").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("rules"),
            workspace.path().join(".rgignore"),
        )
        .unwrap();
        fs::create_dir(workspace.path().join(".ignore")).unwrap();
        create_fifo(&workspace.path().join(".gitignore"));
        let _socket = UnixListener::bind(workspace.path().join("nested/.rgignore")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), glob_path(&host, "*.rs", "."))
                .await
                .expect("ignore-file FIFOs must not block")
                .unwrap(),
            [
                directory_entry("candidate.rs", "file", Some(0)),
                directory_entry("nested/candidate.rs", "file", Some(0)),
            ]
        );
    }

    #[tokio::test]
    async fn glob_traverses_an_explicit_parent_ignored_root() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("ignored-root")).unwrap();
        fs::write(workspace.path().join(".gitignore"), "ignored-root/\n").unwrap();
        fs::write(workspace.path().join("ignored-root/kept.rs"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert!(glob_path(&host, "*.rs", ".").await.unwrap().is_empty());
        assert_eq!(
            glob_path(&host, "*.rs", "ignored-root").await.unwrap(),
            [directory_entry("ignored-root/kept.rs", "file", Some(0))]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn glob_filters_hidden_and_ignored_non_utf8_names_before_rejecting_visible_ones() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let hidden = TempDir::new().unwrap();
        fs::write(hidden.path().join(OsStr::from_bytes(b".\xff")), "").unwrap();
        fs::write(hidden.path().join("kept.rs"), "").unwrap();
        assert_eq!(
            glob_path(&tool_host(&hidden).await, "*.rs", ".")
                .await
                .unwrap(),
            [directory_entry("kept.rs", "file", Some(0))]
        );

        let ignored = TempDir::new().unwrap();
        fs::write(ignored.path().join(".gitignore"), "ignored-*\n").unwrap();
        fs::write(ignored.path().join(OsStr::from_bytes(b"ignored-\xff")), "").unwrap();
        fs::write(ignored.path().join("kept.rs"), "").unwrap();
        assert_eq!(
            glob_path(&tool_host(&ignored).await, "*.rs", ".")
                .await
                .unwrap(),
            [directory_entry("kept.rs", "file", Some(0))]
        );

        let visible = TempDir::new().unwrap();
        fs::write(visible.path().join(OsStr::from_bytes(b"visible-\xff")), "").unwrap();
        assert_eq!(
            glob_error(&tool_host(&visible).await, "*.rs", ".").await,
            ToolErrorKind::Value
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn glob_reports_symlinks_and_other_entries_without_descending_or_blocking() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("inside")).unwrap();
        fs::write(workspace.path().join("inside/file.rs"), "").unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        fs::write(outside.path().join("outside/file.rs"), "").unwrap();
        std::os::unix::fs::symlink("inside", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        let host = tool_host(&workspace).await;

        let entries = tokio::time::timeout(Duration::from_millis(100), glob_path(&host, "*", "."))
            .await
            .expect("globbing a FIFO must not block")
            .unwrap();
        assert_eq!(
            entries,
            [
                directory_entry("fifo", "other", None),
                directory_entry("inside", "directory", None),
                directory_entry("inside-link", "symlink", None),
                directory_entry("inside/file.rs", "file", Some(0)),
                directory_entry("outside-link", "symlink", None),
                directory_entry("socket", "other", None),
            ]
        );
    }

    #[tokio::test]
    async fn glob_returns_workspace_relative_paths_in_stable_order() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("z"), "").unwrap();
        fs::write(workspace.path().join("a"), "").unwrap();
        fs::write(workspace.path().join("nested/b"), "").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            glob_path(&host, "*", "./").await.unwrap(),
            [
                directory_entry("a", "file", Some(0)),
                directory_entry("nested", "directory", None),
                directory_entry("nested/b", "file", Some(0)),
                directory_entry("z", "file", Some(0)),
            ]
        );
        assert_eq!(
            glob_path(&host, "*", "nested").await.unwrap(),
            [directory_entry("nested/b", "file", Some(0))]
        );
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(
                glob_error(&host, "*", path).await,
                ToolErrorKind::Permission
            );
        }
    }

    #[tokio::test]
    async fn glob_enforces_exact_item_and_payload_limits_without_partial_results() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("aa"), "").unwrap();
        fs::write(workspace.path().join("bb"), "").unwrap();

        let at_limit = tool_host_with_collection_limits(&workspace, 2, 12).await;
        assert_eq!(
            glob_path(&at_limit, "*", ".").await.unwrap(),
            [
                directory_entry("aa", "file", Some(0)),
                directory_entry("bb", "file", Some(0)),
            ]
        );

        let over_items = tool_host_with_collection_limits(&workspace, 1, 12).await;
        assert_eq!(
            glob_path(&over_items, "*", ".").await,
            Err(ToolErrorKind::Runtime)
        );
        let over_payload = tool_host_with_collection_limits(&workspace, 2, 11).await;
        assert_eq!(
            glob_path(&over_payload, "*", ".").await,
            Err(ToolErrorKind::Runtime)
        );
    }

    #[test]
    fn active_tools_are_in_the_canonical_order() {
        assert_eq!(
            ACTIVE_TOOLS
                .iter()
                .map(|definition| definition.name.name())
                .collect::<Vec<_>>(),
            [
                "read",
                "write",
                "edit",
                "list_dir",
                "glob",
                "grep",
                "run",
                "load_skill"
            ]
        );
        assert_eq!(ToolName::Grep.count_label(1), "match");
        assert_eq!(ToolName::Grep.count_label(2), "matches");
    }

    #[tokio::test]
    async fn grep_returns_one_record_per_matching_line_in_stable_path_and_line_order() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("src/nested")).unwrap();
        fs::write(workspace.path().join("src/z.rs"), "match z\nmatch again\n").unwrap();
        fs::write(
            workspace.path().join("src/nested/a.rs"),
            "skip\r\nmatch a\r\nmatch last",
        )
        .unwrap();
        fs::write(workspace.path().join("src/nested/a.py"), "match python\n").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "^match", "src", Some("*.rs"))
                .await
                .unwrap(),
            [
                grep_match("src/nested/a.rs", 2, "match a"),
                grep_match("src/nested/a.rs", 3, "match last"),
                grep_match("src/z.rs", 1, "match z"),
                grep_match("src/z.rs", 2, "match again"),
            ]
        );
        assert_eq!(
            grep_path(&host, "match", "src", Some("nested/*.rs"))
                .await
                .unwrap(),
            [
                grep_match("src/nested/a.rs", 2, "match a"),
                grep_match("src/nested/a.rs", 3, "match last"),
            ]
        );
        assert_eq!(
            grep_path(&host, "^match a$", "src", Some("nested/*.rs"))
                .await
                .unwrap(),
            [grep_match("src/nested/a.rs", 2, "match a")]
        );
    }

    #[tokio::test]
    async fn grep_rejects_invalid_patterns_and_malformed_arguments() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_error(&host, "[", ".", None).await,
            ToolErrorKind::Value
        );
        assert_eq!(
            grep_error(&host, "match", ".", Some("[")).await,
            ToolErrorKind::Value
        );
        for arguments in [
            ToolValue::None,
            ToolValue::String("match".to_owned()),
            ToolValue::List(Vec::new()),
            ToolValue::List(vec![ToolValue::String("match".to_owned()), ToolValue::None]),
            ToolValue::List(vec![
                ToolValue::String("match".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::Integer(1),
            ]),
            ToolValue::List(vec![
                ToolValue::String("match".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::None,
                ToolValue::String("extra".to_owned()),
            ]),
            ToolValue::Dictionary(Default::default()),
        ] {
            let result = host
                .call(ToolCall {
                    call_id: 1,
                    tool: grep_tool(),
                    arguments,
                })
                .await;
            assert!(matches!(result.result, Err(ref error) if error.kind == ToolErrorKind::Type));
        }
    }

    #[tokio::test]
    async fn grep_searches_files_larger_than_the_read_limit() {
        let workspace = TempDir::new().unwrap();
        fs::write(
            workspace.path().join("large"),
            format!("{}\nmatch\n", "x".repeat(FILE_MAX_BYTES)),
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "^match$", "large", None).await.unwrap(),
            [grep_match("large", 2, "match")]
        );
    }

    #[test]
    fn utf8_validating_reader_checks_chunk_boundaries_and_eof() {
        use std::io::Read;

        struct ChunkedReader {
            chunks: Vec<Vec<u8>>,
            next: usize,
        }

        impl Read for ChunkedReader {
            fn read(&mut self, buffer: &mut [u8]) -> Result<usize, std::io::Error> {
                let Some(chunk) = self.chunks.get(self.next) else {
                    return Ok(0);
                };
                self.next += 1;
                buffer[..chunk.len()].copy_from_slice(chunk);
                Ok(chunk.len())
            }
        }

        let mut valid = Utf8ValidatingReader::new(ChunkedReader {
            chunks: vec![b"match \xe2".to_vec(), b"\x82".to_vec(), b"\xac\n".to_vec()],
            next: 0,
        });
        let mut contents = Vec::new();
        valid.read_to_end(&mut contents).unwrap();
        assert_eq!(std::str::from_utf8(&contents).unwrap(), "match €\n");

        let mut invalid = Utf8ValidatingReader::new(ChunkedReader {
            chunks: vec![b"match \xe2".to_vec(), b"(".to_vec()],
            next: 0,
        });
        assert_eq!(
            invalid.read_to_end(&mut Vec::new()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut incomplete = Utf8ValidatingReader::new(ChunkedReader {
            chunks: vec![b"match \xe2".to_vec()],
            next: 0,
        });
        assert_eq!(
            incomplete.read_to_end(&mut Vec::new()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_classifies_roots_refuses_symlinks_and_never_opens_other_entries() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "match\n").unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("directory/file"), "match\n").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        fs::create_dir(outside.path().join("outside")).unwrap();
        std::os::unix::fs::symlink("file", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-link")).unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        std::os::unix::fs::symlink("directory", workspace.path().join("directory-link")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "match", "file", None).await.unwrap(),
            [grep_match("file", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "directory", None).await.unwrap(),
            [grep_match("directory/file", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "./directory//.", None)
                .await
                .unwrap(),
            [grep_match("directory/file", 1, "match")]
        );
        for path in ["/outside", "../outside", "directory/../file"] {
            assert_eq!(
                grep_error(&host, "match", path, None).await,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            grep_error(&host, "match", "missing", None).await,
            ToolErrorKind::FileNotFound
        );
        for path in [
            "inside-link",
            "outside-link",
            "broken-link",
            "directory-link",
            "directory-link/file",
            "outside-link/file",
        ] {
            assert_eq!(
                grep_error(&host, "match", path, None).await,
                ToolErrorKind::Permission
            );
        }
        for path in ["fifo", "socket"] {
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    grep_error(&host, "match", path, None)
                )
                .await
                .expect("grepping an other entry must not block"),
                ToolErrorKind::Value
            );
        }
    }

    #[tokio::test]
    async fn grep_applies_the_file_glob_before_explicit_file_content_validation() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("binary.txt"), b"match\n\0").unwrap();
        fs::write(workspace.path().join("invalid.txt"), b"match\n\xff").unwrap();
        fs::write(
            workspace.path().join("utf16.txt"),
            b"\xff\xfem\0a\0t\0c\0h\0\n\0",
        )
        .unwrap();
        fs::write(workspace.path().join("kept.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;

        for path in ["binary.txt", "invalid.txt", "utf16.txt"] {
            assert!(
                grep_path(&host, "match", path, Some("*.rs"))
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                grep_error(&host, "match", path, Some("*.txt")).await,
                ToolErrorKind::Value
            );
        }
        assert_eq!(
            grep_path(&host, "match", "kept.rs", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("kept.rs", 1, "match")]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_does_not_open_a_regular_file_excluded_by_file_glob() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("excluded.txt");
        fs::write(&path, "match\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        let host = tool_host(&workspace).await;

        assert!(
            grep_path(&host, "match", "excluded.txt", Some("*.rs"))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_skips_traversed_non_regular_binary_invalid_and_symlink_entries() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(workspace.path().join("kept.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("binary.rs"), b"match\n\0").unwrap();
        fs::write(workspace.path().join("invalid.rs"), b"not a match\n\xff").unwrap();
        fs::write(
            workspace.path().join("late-binary.rs"),
            format!("match\n{}\0", "x".repeat(64 * 1024)),
        )
        .unwrap();
        let mut late_invalid = format!("match\n{}", "x".repeat(64 * 1024)).into_bytes();
        late_invalid.push(0xff);
        fs::write(workspace.path().join("late-invalid.rs"), late_invalid).unwrap();
        create_fifo(&workspace.path().join("fifo.rs"));
        fs::write(outside.path().join("outside.rs"), "match\n").unwrap();
        fs::create_dir(workspace.path().join("linked-directory")).unwrap();
        fs::write(
            workspace.path().join("linked-directory/match.rs"),
            "match\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            "linked-directory",
            workspace.path().join("inside-directory-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("kept.rs", workspace.path().join("inside-link.rs")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside.rs"),
            workspace.path().join("outside-link.rs"),
        )
        .unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            tokio::time::timeout(
                Duration::from_millis(100),
                grep_path(&host, "match", ".", Some("*.rs")),
            )
            .await
            .expect("grepping a traversed FIFO must not block")
            .unwrap(),
            [
                grep_match("kept.rs", 1, "match"),
                grep_match("linked-directory/match.rs", 1, "match"),
            ]
        );
    }

    #[tokio::test]
    async fn grep_reuses_hidden_and_project_ignore_filtering_but_accepts_explicit_roots() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join(".hidden/nested")).unwrap();
        fs::create_dir(workspace.path().join("ignored-root")).unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".hidden/kept.rs"), "match\n").unwrap();
        fs::write(
            workspace.path().join(".hidden/nested/.hidden.rs"),
            "match\n",
        )
        .unwrap();
        fs::write(workspace.path().join("ignored-root/kept.rs"), "match\n").unwrap();
        fs::write(
            workspace.path().join(".gitignore"),
            "ignored-root/\n*.tmp\n",
        )
        .unwrap();
        fs::write(workspace.path().join("ignored.tmp"), "match\n").unwrap();
        fs::write(workspace.path().join("nested/.rgignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join("nested/ignored.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("nested/kept.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("visible.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            grep_path(&host, "match", ".", Some("*.rs")).await.unwrap(),
            [grep_match("visible.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", ".hidden", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match(".hidden/kept.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", ".hidden/kept.rs", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match(".hidden/kept.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "ignored-root", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("ignored-root/kept.rs", 1, "match")]
        );
        assert_eq!(
            grep_path(&host, "match", "ignored-root/kept.rs", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("ignored-root/kept.rs", 1, "match")]
        );
    }

    #[tokio::test]
    async fn grep_preserves_ignore_family_precedence_and_negation() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "candidate.rs\n").unwrap();
        fs::write(
            workspace.path().join("nested/.gitignore"),
            "!candidate.rs\n",
        )
        .unwrap();
        fs::write(workspace.path().join("nested/candidate.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;
        assert!(
            grep_path(&host, "match", ".", Some("*.rs"))
                .await
                .unwrap()
                .is_empty()
        );

        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join(".rgignore"), "*.rs\n").unwrap();
        fs::write(workspace.path().join("nested/.rgignore"), "!kept.rs\n").unwrap();
        fs::write(workspace.path().join("nested/ignored.rs"), "match\n").unwrap();
        fs::write(workspace.path().join("nested/kept.rs"), "match\n").unwrap();
        let host = tool_host(&workspace).await;
        assert_eq!(
            grep_path(&host, "match", ".", Some("*.rs")).await.unwrap(),
            [grep_match("nested/kept.rs", 1, "match")]
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn grep_filters_hidden_and_ignored_non_utf8_names_before_rejecting_visible_ones() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let hidden = TempDir::new().unwrap();
        fs::write(hidden.path().join(OsStr::from_bytes(b".\xff")), "match\n").unwrap();
        fs::write(hidden.path().join("kept.rs"), "match\n").unwrap();
        assert_eq!(
            grep_path(&tool_host(&hidden).await, "match", ".", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("kept.rs", 1, "match")]
        );

        let ignored = TempDir::new().unwrap();
        fs::write(ignored.path().join(".gitignore"), "ignored-*\n").unwrap();
        fs::write(
            ignored.path().join(OsStr::from_bytes(b"ignored-\xff")),
            "match\n",
        )
        .unwrap();
        fs::write(ignored.path().join("kept.rs"), "match\n").unwrap();
        assert_eq!(
            grep_path(&tool_host(&ignored).await, "match", ".", Some("*.rs"))
                .await
                .unwrap(),
            [grep_match("kept.rs", 1, "match")]
        );

        let visible = TempDir::new().unwrap();
        fs::write(
            visible.path().join(OsStr::from_bytes(b"visible-\xff")),
            "match\n",
        )
        .unwrap();
        assert_eq!(
            grep_error(&tool_host(&visible).await, "match", ".", Some("*.rs")).await,
            ToolErrorKind::Value
        );
    }

    #[tokio::test]
    async fn grep_enforces_exact_item_and_payload_limits_without_partial_results() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("aa"), "match\n").unwrap();
        fs::write(workspace.path().join("bb"), "match\n").unwrap();

        let at_limit = tool_host_with_collection_limits(&workspace, 2, 14).await;
        assert_eq!(
            grep_path(&at_limit, "match", ".", None).await.unwrap(),
            [grep_match("aa", 1, "match"), grep_match("bb", 1, "match")]
        );
        let over_items = tool_host_with_collection_limits(&workspace, 1, 14).await;
        assert_eq!(
            grep_error(&over_items, "match", ".", None).await,
            ToolErrorKind::Runtime
        );
        let over_payload = tool_host_with_collection_limits(&workspace, 2, 13).await;
        assert_eq!(
            grep_error(&over_payload, "match", ".", None).await,
            ToolErrorKind::Runtime
        );
    }

    async fn write(
        host: &ToolHost,
        path: &str,
        content: &str,
        authorisation: Option<ContentFingerprint>,
    ) -> Result<WholeFileWrite, ToolErrorKind> {
        host.write_file(path, content.to_owned(), authorisation)
            .await
            .map_err(|error| error.kind)
    }

    async fn write_error(
        host: &ToolHost,
        path: &str,
        content: &str,
        authorisation: Option<ContentFingerprint>,
    ) -> ToolErrorKind {
        match write(host, path, content, authorisation).await {
            Ok(_) => panic!("write succeeded unexpectedly"),
            Err(error) => error,
        }
    }

    async fn edit(
        host: &ToolHost,
        path: &str,
        old: &str,
        new: &str,
    ) -> Result<WholeFileEdit, ToolErrorKind> {
        host.edit_file(path, old.to_owned(), new.to_owned())
            .await
            .map_err(|error| error.kind)
    }

    async fn edit_error(host: &ToolHost, path: &str, old: &str, new: &str) -> ToolErrorKind {
        match edit(host, path, old, new).await {
            Ok(_) => panic!("edit succeeded unexpectedly"),
            Err(error) => error,
        }
    }

    fn fingerprint(contents: impl AsRef<[u8]>) -> ContentFingerprint {
        content_fingerprint(contents.as_ref())
    }

    fn temporary_entries(directory: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b".kiki-write-")
            })
            .collect()
    }

    #[test]
    fn content_fingerprints_are_canonical_blake3_digests() {
        let expected = ContentFingerprint(*blake3::hash(b"kiki").as_bytes());
        assert_eq!(std::mem::size_of::<ContentFingerprint>(), 32);
        assert!(fingerprint("kiki") == expected);
        assert!(fingerprint("kiki") != fingerprint("Kiki"));
    }

    #[test]
    fn whole_file_authorisations_are_keyed_by_normalised_paths() {
        let mut authorisations = WholeFileAuthorisations::default();
        let path = normalise_path("nested//./file").unwrap();
        let digest = fingerprint("contents");

        authorisations.authorise(path.clone(), digest);
        assert!(authorisations.authorisation(&path) == Some(digest));
        authorisations.remove(&path);
        assert!(authorisations.authorisation(&path).is_none());
    }

    #[tokio::test]
    async fn reads_return_model_values_and_normalised_fingerprint_effects() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(workspace.path().join("nested/file"), "contents").unwrap();
        let host = tool_host(&workspace).await;

        let result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::Read,
                arguments: ToolValue::List(vec![ToolValue::String("./nested//file".to_owned())]),
            })
            .await;

        assert_eq!(result.result, Ok(ToolValue::String("contents".to_owned())));
        let mut authorisations = WholeFileAuthorisations::default();
        result.apply_batch_transition(&mut authorisations, &BTreeSet::new());
        assert_eq!(
            authorisations.authorisation(Path::new("nested/file")),
            Some(fingerprint("contents"))
        );
        assert_eq!(authorisations.fingerprints.len(), 1);
    }

    #[tokio::test]
    async fn failed_reads_have_no_effect() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("invalid"), [0xff]).unwrap();
        fs::write(workspace.path().join("oversized"), "12345").unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;
        let mut authorisations = WholeFileAuthorisations::default();

        for path in ["missing", "invalid", "oversized"] {
            let result = host
                .call(ToolCall {
                    call_id: 7,
                    tool: ToolName::Read,
                    arguments: ToolValue::List(vec![ToolValue::String(path.to_owned())]),
                })
                .await;
            assert!(result.result.is_err());
            assert!(result.effect.is_none());
            result.apply_batch_transition(&mut authorisations, &BTreeSet::new());
        }

        assert_eq!(authorisations, WholeFileAuthorisations::default());
    }

    #[test]
    fn mutation_effects_update_or_invalidate_authorisations_in_pending_order() {
        let path = normalise_path("file").unwrap();
        let first = fingerprint("first");
        let second = fingerprint("second");
        let third = fingerprint("third");
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(path.clone(), first);
        let edit = ToolResult {
            call_id: 1,
            result: Ok(ToolValue::None),
            effect: Some(ToolEffect::Edit {
                path: path.clone(),
                pre_fingerprint: first,
                post_fingerprint: second,
            }),
        };
        edit.apply_batch_transition(&mut authorisations, &BTreeSet::from([path.clone()]));
        assert_eq!(authorisations.authorisation(&path), Some(second));
        assert_eq!(edit.mutation_status(), Some(super::MutationStatus::Applied));

        let write = ToolResult {
            call_id: 2,
            result: Ok(ToolValue::None),
            effect: Some(ToolEffect::Write {
                path: path.clone(),
                outcome: super::WholeFileWriteOutcome::Replaced,
                fingerprint: third,
            }),
        };
        write.apply_batch_transition(&mut authorisations, &BTreeSet::from([path.clone()]));
        assert_eq!(authorisations.authorisation(&path), Some(third));
        assert_eq!(
            write.mutation_status(),
            Some(super::MutationStatus::Replaced)
        );

        edit.apply_batch_transition(&mut authorisations, &BTreeSet::from([path.clone()]));
        assert_eq!(authorisations.authorisation(&path), None);
    }

    #[test]
    fn same_batch_read_suppression_is_independent_of_settlement_order() {
        let path = normalise_path("file").unwrap();
        let read = ToolResult {
            call_id: 1,
            result: Ok(ToolValue::String("observed".to_owned())),
            effect: Some(ToolEffect::Read {
                path: path.clone(),
                fingerprint: fingerprint("observed"),
            }),
        };
        let mutation = ToolResult {
            call_id: 2,
            result: Ok(ToolValue::None),
            effect: Some(ToolEffect::Write {
                path: path.clone(),
                outcome: super::WholeFileWriteOutcome::Replaced,
                fingerprint: fingerprint("written"),
            }),
        };
        let mutation_paths =
            BTreeSet::from([batch_write(2, "./file", "written").mutation_path().unwrap()]);

        for settled in [[&read, &mutation], [&mutation, &read]] {
            let mut authorisations = WholeFileAuthorisations::default();
            for result in settled {
                result.apply_batch_transition(&mut authorisations, &mutation_paths);
            }
            assert_eq!(
                authorisations.authorisation(&path),
                Some(fingerprint("written"))
            );
        }
    }

    #[test]
    fn observed_file_operations_keep_reads_and_mutations_independent() {
        let path = normalise_path("nested/./file").unwrap();
        let read = ToolResult {
            call_id: 1,
            result: Ok(ToolValue::String("contents".to_owned())),
            effect: Some(ToolEffect::Read {
                path: path.clone(),
                fingerprint: fingerprint("contents"),
            }),
        };
        let write = ToolResult {
            call_id: 2,
            result: Ok(ToolValue::None),
            effect: Some(ToolEffect::Write {
                path: path.clone(),
                outcome: super::WholeFileWriteOutcome::Replaced,
                fingerprint: fingerprint("new contents"),
            }),
        };
        let failed = ToolResult::from_result(
            3,
            Err(ToolError::runtime("failed before an effect was observed")),
        );
        let mut reads = BTreeSet::new();
        let mut modifications = BTreeSet::new();
        for result in [&read, &write, &failed] {
            result.record_observed_file_operation(&mut reads, &mut modifications);
        }

        assert_eq!(reads, BTreeSet::from([path.clone()]));
        assert_eq!(modifications, BTreeSet::from([path]));
    }

    #[tokio::test]
    async fn read_effects_apply_in_call_order() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("nested")).unwrap();
        let path = workspace.path().join("nested/file");
        fs::write(&path, "first").unwrap();
        let host = tool_host(&workspace).await;
        let first = host.call(ToolCall {
            call_id: 1,
            tool: ToolName::Read,
            arguments: ToolValue::List(vec![ToolValue::String("nested/file".to_owned())]),
        });
        let first = first.await;
        fs::write(&path, "second").unwrap();
        let second = host.call(ToolCall {
            call_id: 2,
            tool: ToolName::Read,
            arguments: ToolValue::List(vec![ToolValue::String("./nested//file".to_owned())]),
        });
        let second = second.await;

        let mut ordered = WholeFileAuthorisations::default();
        first.apply_batch_transition(&mut ordered, &BTreeSet::new());
        second.apply_batch_transition(&mut ordered, &BTreeSet::new());
        let mut expected = WholeFileAuthorisations::default();
        second.apply_batch_transition(&mut expected, &BTreeSet::new());
        let mut reversed = WholeFileAuthorisations::default();
        second.apply_batch_transition(&mut reversed, &BTreeSet::new());
        first.apply_batch_transition(&mut reversed, &BTreeSet::new());

        assert_eq!(ordered, expected);
        assert_ne!(ordered, reversed);
    }

    #[test]
    fn authorisation_effect_and_result_debug_are_opaque() {
        let path = PathBuf::from("distinctive/private-path");
        let fingerprint = ContentFingerprint([42; 32]);
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(path.clone(), fingerprint);
        let result = ToolResult {
            call_id: 7,
            result: Ok(ToolValue::String("visible".to_owned())),
            effect: Some(ToolEffect::Read { path, fingerprint }),
        };
        let marker = "[42, 42";

        for debug in [
            format!("{authorisations:?}"),
            format!("{:?}", result.effect.as_ref().unwrap()),
            format!("{result:?}"),
        ] {
            assert!(!debug.contains("distinctive/private-path"));
            assert!(!debug.contains(marker));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn unsupported_no_replace_errors_choose_hard_link_fallback() {
        assert!(
            [Errno::NOSYS, Errno::INVAL, Errno::NOTSUP]
                .into_iter()
                .all(super::no_replace_is_unsupported)
        );
        assert!(
            ![Errno::PERM, Errno::ACCESS, Errno::IO]
                .into_iter()
                .any(super::no_replace_is_unsupported)
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publication_rename_errors_map_freshness_conflicts_to_runtime() {
        for errno in [
            Errno::NOENT,
            Errno::ISDIR,
            Errno::NOTDIR,
            Errno::NOTEMPTY,
            Errno::EXIST,
        ] {
            let error = super::replacement_rename_error(errno);
            assert_eq!(error.kind, ToolErrorKind::Runtime);
            assert_eq!(error.to_string(), "file changed during replacement");
        }
        for errno in [
            Errno::NOENT,
            Errno::ISDIR,
            Errno::NOTDIR,
            Errno::NOTEMPTY,
            Errno::EXIST,
        ] {
            let error = super::no_replace_rename_error(errno);
            assert_eq!(error.kind, ToolErrorKind::Runtime);
            assert_eq!(error.to_string(), "file changed during publication");
        }
        for classify in [
            super::replacement_rename_error as fn(Errno) -> ToolError,
            super::no_replace_rename_error,
        ] {
            for errno in [Errno::PERM, Errno::ACCESS] {
                assert_eq!(classify(errno).kind, ToolErrorKind::Permission);
            }
            assert_eq!(classify(Errno::IO).kind, ToolErrorKind::Os);
        }
    }

    #[tokio::test]
    async fn write_creates_root_and_nested_files_with_normalised_paths() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        let root = write(&host, "./root", "root", None).await.unwrap();
        let nested = write(&host, "nested//./file", "nested", None)
            .await
            .unwrap();

        assert!(root.outcome == super::WholeFileWriteOutcome::Created);
        assert!(nested.outcome == super::WholeFileWriteOutcome::Created);
        assert!(root.fingerprint == fingerprint("root"));
        assert!(nested.fingerprint == fingerprint("nested"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("root")).unwrap(),
            "root"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("nested/file")).unwrap(),
            "nested"
        );
    }

    #[tokio::test]
    async fn write_validates_paths_and_content_before_creating_parents() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;

        for path in ["/outside", "../outside", "nested/../outside"] {
            assert_eq!(
                write_error(&host, path, "text", None).await,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            write_error(&host, "missing/too-large", "12345", None).await,
            ToolErrorKind::Value
        );
        assert!(!workspace.path().join("missing").exists());
        assert!(
            write(&host, "at-limit", "1234", None)
                .await
                .unwrap()
                .outcome
                == super::WholeFileWriteOutcome::Created
        );
    }

    #[tokio::test]
    async fn write_requires_current_authorisation_to_replace_regular_files() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            write_error(&host, "file", "after", None).await,
            ToolErrorKind::Runtime
        );
        assert_eq!(
            write_error(&host, "file", "after", Some(fingerprint("stale"))).await,
            ToolErrorKind::Runtime
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "before"
        );

        let outcome = write(&host, "file", "after", Some(fingerprint("before")))
            .await
            .unwrap();
        assert!(outcome.outcome == super::WholeFileWriteOutcome::Replaced);
        assert!(outcome.fingerprint == fingerprint("after"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "after"
        );
    }

    #[tokio::test]
    async fn write_rejects_same_length_external_changes_and_binary_files_without_authorisation() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        fs::write(workspace.path().join("binary"), [0xff]).unwrap();
        let host = tool_host(&workspace).await;

        fs::write(workspace.path().join("file"), "other!").unwrap();
        assert_eq!(
            write_error(&host, "file", "after", Some(fingerprint("before"))).await,
            ToolErrorKind::Runtime
        );
        assert_eq!(
            write_error(&host, "binary", "after", None).await,
            ToolErrorKind::Runtime
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "other!"
        );
        assert_eq!(fs::read(workspace.path().join("binary")).unwrap(), [0xff]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_classifies_terminal_and_intermediate_path_kinds_without_following_symlinks() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("regular"), "before").unwrap();
        fs::write(outside.path().join("outside"), "outside").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        std::os::unix::fs::symlink("regular", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        std::os::unix::fs::symlink("directory", workspace.path().join("inside-parent")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-parent"))
            .unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-parent")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            write_error(&host, "directory", "x", None).await,
            ToolErrorKind::IsADirectory
        );
        assert_eq!(
            write_error(&host, "fifo", "x", None).await,
            ToolErrorKind::Value
        );
        assert_eq!(
            write_error(&host, "socket", "x", None).await,
            ToolErrorKind::Value
        );
        for path in [
            "inside-link",
            "outside-link",
            "broken-link",
            "inside-parent/file",
            "outside-parent/file",
            "broken-parent/file",
        ] {
            assert_eq!(
                write_error(&host, path, "x", None).await,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            write_error(&host, "regular/child", "x", None).await,
            ToolErrorKind::NotADirectory
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("outside")).unwrap(),
            "outside"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_preserves_replacement_mode_and_uses_ordinary_new_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().unwrap();
        let replacement = workspace.path().join("replacement");
        let reference = workspace.path().join("reference");
        fs::write(&replacement, "before").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(&reference, "reference").unwrap();
        let host = tool_host(&workspace).await;

        write(&host, "replacement", "after", Some(fingerprint("before")))
            .await
            .unwrap();
        write(&host, "new", "new", None).await.unwrap();

        assert_eq!(
            fs::metadata(replacement).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::metadata(workspace.path().join("new"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            fs::metadata(reference).unwrap().permissions().mode() & 0o777
        );
    }

    #[tokio::test]
    async fn write_rechecks_before_replacement_and_never_clobbers_a_new_target() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("replace"), "before").unwrap();
        let recheck_hook = WriteTestHook::pause_before_recheck();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(recheck_hook.clone()),
        );
        let writing_host = Arc::clone(&host);
        let replace = tokio::spawn(async move {
            write(
                &writing_host,
                "replace",
                "after",
                Some(fingerprint("before")),
            )
            .await
        });
        recheck_hook.wait_until_paused().await;
        fs::write(workspace.path().join("replace"), "other!").unwrap();
        recheck_hook.resume();
        assert!(matches!(
            replace.await.unwrap(),
            Err(ToolErrorKind::Runtime)
        ));
        assert_eq!(
            fs::read_to_string(workspace.path().join("replace")).unwrap(),
            "other!"
        );
        assert!(temporary_entries(workspace.path()).is_empty());

        let publish_hook = WriteTestHook::pause_before_publish();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(publish_hook.clone()),
        );
        let writing_host = Arc::clone(&host);
        let create =
            tokio::spawn(async move { write(&writing_host, "new", "created", None).await });
        publish_hook.wait_until_paused().await;
        fs::write(workspace.path().join("new"), "appeared").unwrap();
        publish_hook.resume();
        assert!(matches!(create.await.unwrap(), Err(ToolErrorKind::Runtime)));
        assert_eq!(
            fs::read_to_string(workspace.path().join("new")).unwrap(),
            "appeared"
        );
        assert!(temporary_entries(workspace.path()).is_empty());

        let fallback_hook = WriteTestHook::pause_before_publish();
        fallback_hook.set_hard_link_fallback();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(fallback_hook.clone()),
        );
        let writing_host = Arc::clone(&host);
        let create =
            tokio::spawn(
                async move { write(&writing_host, "fallback-new", "created", None).await },
            );
        fallback_hook.wait_until_paused().await;
        fs::write(workspace.path().join("fallback-new"), "appeared").unwrap();
        fallback_hook.resume();
        assert!(matches!(create.await.unwrap(), Err(ToolErrorKind::Runtime)));
        assert_eq!(
            fs::read_to_string(workspace.path().join("fallback-new")).unwrap(),
            "appeared"
        );
        assert!(temporary_entries(workspace.path()).is_empty());
    }

    #[tokio::test]
    async fn write_publishes_only_complete_old_or_new_file_contents() {
        use std::sync::{Barrier, Mutex};

        let workspace = TempDir::new().unwrap();
        let old = "old".repeat(256 * 1024);
        let new = "new".repeat(256 * 1024);
        fs::write(workspace.path().join("file"), &old).unwrap();
        let hook = WriteTestHook::pause_before_recheck();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(hook.clone()),
        );
        let writing_host = Arc::clone(&host);
        let expected = fingerprint(&old);
        let new_for_write = new.clone();
        let write_task = tokio::spawn(async move {
            write(&writing_host, "file", &new_for_write, Some(expected)).await
        });
        hook.wait_until_paused().await;

        let observations = Arc::new(Mutex::new(Vec::new()));
        let reader_observations = Arc::clone(&observations);
        let old_observed = Arc::new(Barrier::new(2));
        let new_observed = Arc::new(Barrier::new(2));
        let reader_old_observed = Arc::clone(&old_observed);
        let reader_new_observed = Arc::clone(&new_observed);
        let path = workspace.path().join("file");
        let new_for_reader = new.clone();
        let reader = std::thread::spawn(move || {
            let observation = fs::read_to_string(&path).unwrap();
            reader_observations.lock().unwrap().push(observation);
            reader_old_observed.wait();

            loop {
                let observation = fs::read_to_string(&path).unwrap();
                let published = observation == new_for_reader;
                reader_observations.lock().unwrap().push(observation);
                if published {
                    reader_new_observed.wait();
                    return;
                }
            }
        });
        old_observed.wait();
        hook.resume();
        write_task.await.unwrap().unwrap();
        new_observed.wait();
        reader.join().unwrap();

        let observations = observations.lock().unwrap();
        assert!(observations.iter().any(|value| value == &old));
        assert!(observations.iter().any(|value| value == &new));
        assert!(
            observations
                .iter()
                .all(|value| value == &old || value == &new)
        );
    }

    #[tokio::test]
    async fn write_retries_temp_collisions_and_cleans_unpublished_temps_after_failures() {
        let workspace = TempDir::new().unwrap();
        let first = [1; 16];
        let second = [2; 16];
        fs::write(
            workspace.path().join(WriteTestHook::temporary_name(first)),
            "collision",
        )
        .unwrap();
        let collision_hook = WriteTestHook::with_temp_suffixes([first, second]);
        collision_hook.set_fail_write();
        let host = tool_host(&workspace)
            .await
            .with_write_test_hook(collision_hook);
        assert_eq!(
            write_error(&host, "file", "contents", None).await,
            ToolErrorKind::Os
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join(WriteTestHook::temporary_name(first)))
                .unwrap(),
            "collision"
        );
        assert_eq!(
            temporary_entries(workspace.path()),
            [workspace.path().join(WriteTestHook::temporary_name(first))]
        );

        for failure in [
            WriteTestHook::fail_entropy as fn() -> WriteTestHook,
            WriteTestHook::fail_write,
            WriteTestHook::fail_publish,
        ] {
            let workspace = TempDir::new().unwrap();
            fs::write(workspace.path().join("file"), "before").unwrap();
            let host = tool_host(&workspace).await.with_write_test_hook(failure());
            assert_eq!(
                write_error(&host, "file", "contents", Some(fingerprint("before"))).await,
                ToolErrorKind::Os
            );
            assert_eq!(
                fs::read_to_string(workspace.path().join("file")).unwrap(),
                "before"
            );
            assert!(temporary_entries(workspace.path()).is_empty());
        }
    }

    #[tokio::test]
    async fn write_keeps_created_parents_after_later_failure_and_uses_no_clobber_fallback() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace)
            .await
            .with_write_test_hook(WriteTestHook::fail_write());
        assert_eq!(
            write_error(&host, "created/then-fail", "contents", None).await,
            ToolErrorKind::Os
        );
        assert!(workspace.path().join("created").is_dir());
        assert!(!workspace.path().join("created/then-fail").exists());

        let fallback_hook = WriteTestHook::force_hard_link_fallback();
        let host = tool_host(&workspace)
            .await
            .with_write_test_hook(fallback_hook);
        assert!(
            write(&host, "fallback", "contents", None)
                .await
                .unwrap()
                .outcome
                == super::WholeFileWriteOutcome::Created
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("fallback")).unwrap(),
            "contents"
        );
        assert!(temporary_entries(workspace.path()).is_empty());
    }

    #[tokio::test]
    async fn edit_replaces_one_exact_match_and_returns_content_fingerprints() {
        let workspace = TempDir::new().unwrap();
        let before = "before needle after";
        fs::write(workspace.path().join("file"), before).unwrap();
        let host = tool_host(&workspace).await;

        let outcome = edit(&host, "./file", "needle", "replacement")
            .await
            .unwrap();

        assert!(outcome.pre_fingerprint == fingerprint(before));
        assert!(outcome.post_fingerprint == fingerprint("before replacement after"));
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "before replacement after"
        );
    }

    #[tokio::test]
    async fn edit_rejects_empty_identical_zero_multiple_and_overlapping_matches_without_mutation() {
        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("file");
        fs::write(&path, "aaa").unwrap();
        let host = tool_host(&workspace).await;

        for (old, new) in [
            ("", "x"),
            ("aaa", "aaa"),
            ("missing", "x"),
            ("a", "x"),
            ("aa", "x"),
        ] {
            assert_eq!(
                edit_error(&host, "file", old, new).await,
                ToolErrorKind::Value
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), "aaa");
        }
    }

    #[tokio::test]
    async fn edit_counts_ascii_and_multibyte_matches_at_character_boundaries() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("ascii"), "one needle two").unwrap();
        fs::write(workspace.path().join("multibyte"), "one é needle € two").unwrap();
        fs::write(workspace.path().join("multibyte-overlap"), "ééé").unwrap();
        fs::write(workspace.path().join("scalar"), "€").unwrap();
        let host = tool_host(&workspace).await;

        edit(&host, "ascii", "needle", "replacement").await.unwrap();
        edit(&host, "multibyte", "é needle €", "£").await.unwrap();
        assert_eq!(
            edit_error(&host, "multibyte-overlap", "éé", "£").await,
            ToolErrorKind::Value
        );
        assert_eq!(
            edit_error(&host, "scalar", "©", "replacement").await,
            ToolErrorKind::Value
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("ascii")).unwrap(),
            "one replacement two"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("multibyte")).unwrap(),
            "one £ two"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("multibyte-overlap")).unwrap(),
            "ééé"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("scalar")).unwrap(),
            "€"
        );
    }

    #[tokio::test]
    async fn edit_enforces_pre_and_post_edit_byte_limits_without_mutation() {
        let workspace = TempDir::new().unwrap();
        let at_limit = workspace.path().join("at-limit");
        let over_limit = workspace.path().join("over-limit");
        let multibyte_at_limit = workspace.path().join("multibyte-at-limit");
        let multibyte_growth = workspace.path().join("multibyte-growth");
        fs::write(&at_limit, "1234").unwrap();
        fs::write(&over_limit, "12345").unwrap();
        fs::write(&multibyte_at_limit, "éab").unwrap();
        fs::write(&multibyte_growth, "éab").unwrap();
        let host = tool_host_with_limits(&workspace, 4).await;

        edit(&host, "at-limit", "4", "x").await.unwrap();
        assert_eq!(fs::read_to_string(&at_limit).unwrap(), "123x");
        assert_eq!(
            edit_error(&host, "at-limit", "x", "56").await,
            ToolErrorKind::Value
        );
        assert_eq!(fs::read_to_string(&at_limit).unwrap(), "123x");
        edit(&host, "multibyte-at-limit", "ab", "é").await.unwrap();
        assert_eq!(fs::read_to_string(&multibyte_at_limit).unwrap(), "éé");
        assert_eq!(
            edit_error(&host, "multibyte-growth", "ab", "€").await,
            ToolErrorKind::Value
        );
        assert_eq!(fs::read_to_string(&multibyte_growth).unwrap(), "éab");
        assert_eq!(
            edit_error(&host, "over-limit", "5", "x").await,
            ToolErrorKind::Value
        );
        assert_eq!(fs::read_to_string(&over_limit).unwrap(), "12345");
    }

    #[tokio::test]
    async fn edit_normalises_paths_and_rejects_escapes_before_mutation() {
        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("nested/file");
        fs::create_dir(workspace.path().join("nested")).unwrap();
        fs::write(&path, "before").unwrap();
        let host = tool_host(&workspace).await;

        edit(&host, "./nested//file", "before", "after")
            .await
            .unwrap();
        for path in ["/outside", "../outside", "nested/../file"] {
            assert_eq!(
                edit_error(&host, path, "after", "changed").await,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            fs::read_to_string(workspace.path().join("nested/file")).unwrap(),
            "after"
        );
    }

    #[tokio::test]
    async fn edit_rejects_invalid_utf8_without_mutation() {
        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("file");
        fs::write(&path, [0xff]).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            edit_error(&host, "file", "x", "y").await,
            ToolErrorKind::Value
        );
        assert_eq!(fs::read(&path).unwrap(), [0xff]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_classifies_terminal_and_intermediate_path_kinds_without_following_symlinks() {
        use std::os::unix::net::UnixListener;

        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir(workspace.path().join("directory")).unwrap();
        fs::write(workspace.path().join("regular"), "old").unwrap();
        fs::write(outside.path().join("outside"), "old").unwrap();
        create_fifo(&workspace.path().join("fifo"));
        let _socket = UnixListener::bind(workspace.path().join("socket")).unwrap();
        std::os::unix::fs::symlink("regular", workspace.path().join("inside-link")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside"),
            workspace.path().join("outside-link"),
        )
        .unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-link")).unwrap();
        std::os::unix::fs::symlink("directory", workspace.path().join("inside-parent")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside-parent"))
            .unwrap();
        std::os::unix::fs::symlink("missing", workspace.path().join("broken-parent")).unwrap();
        let host = tool_host(&workspace).await;

        assert_eq!(
            edit_error(&host, "missing", "old", "new").await,
            ToolErrorKind::FileNotFound
        );
        assert_eq!(
            edit_error(&host, "directory", "old", "new").await,
            ToolErrorKind::IsADirectory
        );
        for path in ["fifo", "socket"] {
            assert_eq!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    edit_error(&host, path, "old", "new")
                )
                .await
                .expect("editing an other entry must not block"),
                ToolErrorKind::Value
            );
        }
        for path in [
            "inside-link",
            "outside-link",
            "broken-link",
            "inside-parent/file",
            "outside-parent/file",
            "broken-parent/file",
        ] {
            assert_eq!(
                edit_error(&host, path, "old", "new").await,
                ToolErrorKind::Permission
            );
        }
        assert_eq!(
            edit_error(&host, "regular/child", "old", "new").await,
            ToolErrorKind::NotADirectory
        );
        assert_eq!(
            fs::read_to_string(outside.path().join("outside")).unwrap(),
            "old"
        );
    }

    #[tokio::test]
    async fn edit_rechecks_content_disappearance_and_kind_changes_before_publication() {
        let workspace = TempDir::new().unwrap();

        let content_hook = WriteTestHook::pause_before_recheck();
        fs::write(workspace.path().join("content"), "before").unwrap();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(content_hook.clone()),
        );
        let editing_host = Arc::clone(&host);
        let edit_task =
            tokio::spawn(async move { edit(&editing_host, "content", "before", "after").await });
        content_hook.wait_until_paused().await;
        fs::write(workspace.path().join("content"), "external").unwrap();
        content_hook.resume();
        assert!(matches!(
            edit_task.await.unwrap(),
            Err(ToolErrorKind::Runtime)
        ));
        assert_eq!(
            fs::read_to_string(workspace.path().join("content")).unwrap(),
            "external"
        );
        assert!(temporary_entries(workspace.path()).is_empty());

        let missing_hook = WriteTestHook::pause_before_recheck();
        fs::write(workspace.path().join("missing"), "before").unwrap();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(missing_hook.clone()),
        );
        let editing_host = Arc::clone(&host);
        let edit_task =
            tokio::spawn(async move { edit(&editing_host, "missing", "before", "after").await });
        missing_hook.wait_until_paused().await;
        fs::remove_file(workspace.path().join("missing")).unwrap();
        missing_hook.resume();
        assert!(matches!(
            edit_task.await.unwrap(),
            Err(ToolErrorKind::Runtime)
        ));
        assert!(!workspace.path().join("missing").exists());
        assert!(temporary_entries(workspace.path()).is_empty());

        let kind_hook = WriteTestHook::pause_before_recheck();
        fs::write(workspace.path().join("kind"), "before").unwrap();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(kind_hook.clone()),
        );
        let editing_host = Arc::clone(&host);
        let edit_task =
            tokio::spawn(async move { edit(&editing_host, "kind", "before", "after").await });
        kind_hook.wait_until_paused().await;
        fs::remove_file(workspace.path().join("kind")).unwrap();
        fs::create_dir(workspace.path().join("kind")).unwrap();
        kind_hook.resume();
        assert!(matches!(
            edit_task.await.unwrap(),
            Err(ToolErrorKind::Runtime)
        ));
        assert!(workspace.path().join("kind").is_dir());
        assert!(temporary_entries(workspace.path()).is_empty());
    }

    #[tokio::test]
    async fn edit_publishes_only_complete_old_or_new_file_contents() {
        use std::sync::{Barrier, Mutex};

        let workspace = TempDir::new().unwrap();
        let old = "old".repeat(256 * 1024);
        let new = "new".repeat(256 * 1024);
        fs::write(workspace.path().join("file"), &old).unwrap();
        let hook = WriteTestHook::pause_before_recheck();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(hook.clone()),
        );
        let editing_host = Arc::clone(&host);
        let old_for_edit = old.clone();
        let new_for_edit = new.clone();
        let edit_task =
            tokio::spawn(
                async move { edit(&editing_host, "file", &old_for_edit, &new_for_edit).await },
            );
        hook.wait_until_paused().await;

        let observations = Arc::new(Mutex::new(Vec::new()));
        let reader_observations = Arc::clone(&observations);
        let old_observed = Arc::new(Barrier::new(2));
        let new_observed = Arc::new(Barrier::new(2));
        let reader_old_observed = Arc::clone(&old_observed);
        let reader_new_observed = Arc::clone(&new_observed);
        let path = workspace.path().join("file");
        let new_for_reader = new.clone();
        let reader = std::thread::spawn(move || {
            let observation = fs::read_to_string(&path).unwrap();
            reader_observations.lock().unwrap().push(observation);
            reader_old_observed.wait();

            loop {
                let observation = fs::read_to_string(&path).unwrap();
                let published = observation == new_for_reader;
                reader_observations.lock().unwrap().push(observation);
                if published {
                    reader_new_observed.wait();
                    return;
                }
            }
        });
        old_observed.wait();
        hook.resume();
        edit_task.await.unwrap().unwrap();
        new_observed.wait();
        reader.join().unwrap();

        let observations = observations.lock().unwrap();
        assert!(observations.iter().any(|value| value == &old));
        assert!(observations.iter().any(|value| value == &new));
        assert!(
            observations
                .iter()
                .all(|value| value == &old || value == &new)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn edit_preserves_exact_existing_permission_bits() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("file");
        fs::write(&path, "before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let host = tool_host(&workspace).await;

        edit(&host, "file", "before", "after").await.unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_uses_the_mode_from_its_opened_initial_descriptor() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = TempDir::new().unwrap();
        let path = workspace.path().join("file");
        fs::write(&path, "before").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let hook = WriteTestHook::pause_before_initial_read();
        let host = Arc::new(
            tool_host(&workspace)
                .await
                .with_write_test_hook(hook.clone()),
        );
        let writing_host = Arc::clone(&host);
        let write_task = tokio::spawn(async move {
            write(&writing_host, "file", "after", Some(fingerprint("before"))).await
        });
        hook.wait_until_paused().await;
        let replacement = workspace.path().join("replacement");
        fs::write(&replacement, "before").unwrap();
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o640)).unwrap();
        fs::rename(&replacement, &path).unwrap();
        hook.resume();

        write_task.await.unwrap().unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "after");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
    }

    #[test]
    fn generated_subagent_interface_separates_reference_from_runtime_transport() {
        let expected_reference = concat!(
            "@dataclass\n",
            "class SubagentStatus:\n",
            "    id: str\n",
            "    state: str\n",
            "    model: str\n",
            "    thinking: str\n",
            "    step_count: int\n",
            "    elapsed_ms: int\n",
            "    observed_at_ms: int\n",
            "    estimated_cost_usd: float | None\n",
            "    cost_status: str\n",
            "    error: str | None\n\n",
            "class SubagentTask:\n",
            "    id: str\n",
            "    status: SubagentStatus\n\n",
            "    async def refresh(self) -> SubagentStatus:\n",
            "        ...\n\n",
            "    async def wait(self) -> dict:\n",
            "        ...\n\n",
            "    async def cancel(self) -> SubagentStatus:\n",
            "        ...\n\n",
            "async def start_subagent(prompt: str, model: str | None = None, read_only: bool = False) -> SubagentTask:\n",
            "    \"\"\"Start a subagent and return its task handle.\"\"\"\n",
            "    ...\n\n",
            "async def run_subagent(prompt: str, model: str | None = None, read_only: bool = False) -> dict:\n",
            "    \"\"\"Start a subagent and wait for its result.\"\"\"\n",
            "    ...\n\n",
            "async def get_subagent(id: str) -> SubagentTask:\n",
            "    \"\"\"Return an authorised subagent task handle by ID.\"\"\"\n",
            "    ..."
        );
        let expected_runtime = expected_reference.replace(
            "    async def refresh(self) -> SubagentStatus:\n        ...",
            "    def __init__(self, value: dict):\n        self.id = value['id']\n        self.status = SubagentStatus(**value['status'])\n\n    async def refresh(self) -> SubagentStatus:\n        self.status = SubagentStatus(**await _kiki_refresh_subagent(self.id))\n        return self.status",
        )
        .replace(
            "    async def wait(self) -> dict:\n        ...",
            "    async def wait(self) -> dict:\n        outcome = await _kiki_wait_subagent(self.id)\n        self.status = SubagentStatus(**outcome['status'])\n        if outcome['outcome'] == 'completed':\n            return outcome['result']\n        if outcome['outcome'] == 'timed_out':\n            raise TimeoutError(outcome['error'])\n        raise RuntimeError(outcome['error'])",
        )
        .replace(
            "    async def cancel(self) -> SubagentStatus:\n        ...",
            "    async def cancel(self) -> SubagentStatus:\n        self.status = SubagentStatus(**await _kiki_cancel_subagent(self.id))\n        return self.status",
        )
        .replace(
            "    \"\"\"Start a subagent and return its task handle.\"\"\"\n    ...",
            "    \"\"\"Start a subagent and return its task handle.\"\"\"\n    return SubagentTask(await _kiki_start_subagent(prompt, model, read_only))",
        )
        .replace(
            "    \"\"\"Start a subagent and wait for its result.\"\"\"\n    ...",
            "    \"\"\"Start a subagent and wait for its result.\"\"\"\n    return await (await start_subagent(prompt, model, read_only)).wait()",
        )
        .replace(
            "    \"\"\"Return an authorised subagent task handle by ID.\"\"\"\n    ...",
            "    \"\"\"Return an authorised subagent task handle by ID.\"\"\"\n    return SubagentTask(await _kiki_get_subagent(id))",
        );

        assert_eq!(invocation_source(true), expected_reference);
        assert_eq!(invocation_source(false), expected_runtime);
        let reference = prompt_reference(&SkillCatalog::default());
        assert!(reference.contains(expected_reference));
        assert!(!reference.contains("def __init__(self, value: dict):"));
        assert!(bootstrap_source().contains(&expected_runtime));
    }

    #[test]
    fn generated_sources_share_active_tool_definitions() {
        assert_eq!(
            ACTIVE_TOOLS
                .iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>(),
            [
                ToolName::Read,
                ToolName::Write,
                ToolName::Edit,
                ToolName::ListDir,
                ToolName::Glob,
                ToolName::Grep,
                ToolName::Run,
                ToolName::LoadSkill,
            ]
        );
        assert_eq!(
            bootstrap_source(),
            format!(
                concat!(
                    "from dataclasses import dataclass\n\n",
                    "@dataclass\n",
                    "class DirectoryEntry:\n",
                    "    path: str\n",
                    "    kind: str\n",
                    "    size_bytes: int | None\n\n",
                    "@dataclass\n",
                    "class GrepMatch:\n",
                    "    path: str\n",
                    "    line: int\n",
                    "    text: str\n\n",
                    "@dataclass\n",
                    "class CommandResult:\n",
                    "    exit_code: int | None\n",
                    "    stdout: str\n",
                    "    stderr: str\n\n",
                    "async def read(path: str) -> str:\n",
                    "    \"\"\"Read one complete UTF-8 workspace file or skill:// resource.\"\"\"\n",
                    "    return await _kiki_read(path)\n\n",
                    "async def write(path: str, content: str) -> None:\n",
                    "    \"\"\"Create or replace one complete UTF-8 workspace file.\"\"\"\n",
                    "    await _kiki_write(path, content)\n\n",
                    "async def edit(path: str, old: str, new: str) -> None:\n",
                    "    \"\"\"Replace one exact occurrence in a UTF-8 workspace file.\"\"\"\n",
                    "    await _kiki_edit(path, old, new)\n\n",
                    "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:\n",
                    "    \"\"\"List immediate workspace or skill:// directory entries.\"\"\"\n",
                    "    return [DirectoryEntry(**row) for row in await _kiki_list_dir(path)]\n\n",
                    "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:\n",
                    "    \"\"\"Match workspace filesystem entries recursively.\"\"\"\n",
                    "    return [DirectoryEntry(**row) for row in await _kiki_glob(pattern, path)]\n\n",
                    "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:\n",
                    "    \"\"\"Search workspace file contents with a regular expression.\"\"\"\n",
                    "    return [GrepMatch(**row) for row in await _kiki_grep(pattern, path, file_glob)]\n\n",
                    "async def run(command: str, cwd: str = \".\") -> CommandResult:\n",
                    "    \"\"\"Run one project-local shell command.\"\"\"\n",
                    "    return CommandResult(**await _kiki_run(command, cwd))\n\n",
                    "async def load_skill(name: str) -> str:\n",
                    "    \"\"\"Activate one model-invocable skill from the process catalog.\"\"\"\n",
                    "    return await _kiki_load_skill(name)\n\n{}"
                ),
                invocation_source(false)
            )
        );
        let reference = prompt_reference(&SkillCatalog::default());
        for signature in [
            "async def read(path: str) -> str:",
            "async def write(path: str, content: str) -> None:",
            "async def edit(path: str, old: str, new: str) -> None:",
            "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
            "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
            "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:",
            "async def run(command: str, cwd: str = \".\") -> CommandResult:",
        ] {
            assert_eq!(reference.matches(signature).count(), 1);
        }
        assert_eq!(reference.matches("class DirectoryEntry:").count(), 1);
        assert_eq!(reference.matches("class GrepMatch:").count(), 1);
        assert_eq!(reference.matches("class CommandResult:").count(), 1);
        assert!(reference.contains("Do not redefine these host functions."));
        assert!(reference.contains("Run project-local, one-shot commands."));
        assert!(reference.contains("five minutes of chargeable time"));
        assert!(reference.contains(
            "Time pauses only while waiting exclusively for child completion or cleanup; ordinary and control calls stay charged."
        ));
        assert!(!reference.contains("five-minute deadline"));
        assert!(reference.contains("    ..."));
    }

    #[tokio::test]
    async fn read_only_child_tool_hosts_narrow_prompts_and_deny_effects() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file"), "before").unwrap();
        let root = tool_host(&workspace).await;
        let root_reference = prompt_reference(&SkillCatalog::default());
        assert_eq!(root.prompt_reference(), root_reference);
        assert_eq!(root.for_child(false).prompt_reference(), root_reference);

        let child = root.for_child(true);
        let reference = child.prompt_reference();
        for signature in [
            "async def read(path: str) -> str:",
            "async def list_dir(path: str = \".\") -> list[DirectoryEntry]:",
            "async def glob(pattern: str, path: str = \".\") -> list[DirectoryEntry]:",
            "async def grep(pattern: str, path: str = \".\", file_glob: str | None = None) -> list[GrepMatch]:",
        ] {
            assert_eq!(reference.matches(signature).count(), 1);
        }
        for omitted in [
            "async def write(",
            "async def edit(",
            "async def run(",
            "class CommandResult:",
            "Run project-local, one-shot commands.",
        ] {
            assert!(
                !reference.contains(omitted),
                "restricted reference retained {omitted}"
            );
        }

        let descendant = child.for_child(false);
        assert_eq!(descendant.prompt_reference(), reference);
        assert_eq!(read(&descendant, "file").await.unwrap(), "before");

        let path = normalise_path("file").unwrap();
        let digest = content_fingerprint(b"before");
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(path.clone(), digest);
        for call in [
            batch_write(1, "created", "write"),
            batch_edit(2, "file", "before", "after"),
            batch_run(3, "printf run > command-effect", None),
        ] {
            let before = authorisations.clone();
            let result = descendant.call_with_authorisation(call, Some(digest)).await;
            assert!(matches!(
                &result.result,
                Err(error) if error.kind == ToolErrorKind::Permission
            ));
            assert!(result.effect.is_none());
            result.apply_batch_transition(&mut authorisations, &BTreeSet::new());
            assert_eq!(authorisations, before);
        }
        assert_eq!(
            fs::read_to_string(workspace.path().join("file")).unwrap(),
            "before"
        );
        assert!(!workspace.path().join("created").exists());
        assert!(!workspace.path().join("command-effect").exists());
    }

    #[tokio::test]
    async fn run_rejects_missing_extra_and_non_string_arguments() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        let expected_message = "run() expects one or two positional string arguments: string command, optional string cwd";

        let non_list_result = host
            .call(ToolCall {
                call_id: 7,
                tool: ToolName::Run,
                arguments: ToolValue::None,
            })
            .await;
        let err = non_list_result.result.unwrap_err();
        assert_eq!(err.kind, ToolErrorKind::Type);
        assert_eq!(err.to_string(), expected_message);

        for args in [
            vec![],
            vec![
                ToolValue::String("echo".to_owned()),
                ToolValue::String(".".to_owned()),
                ToolValue::String("extra".to_owned()),
            ],
            vec![ToolValue::Integer(1)],
            vec![ToolValue::String("echo".to_owned()), ToolValue::None],
            vec![ToolValue::String("echo".to_owned()), ToolValue::Integer(1)],
        ] {
            let err = run_call(&host, args).await.unwrap_err();
            assert_eq!(err.kind, ToolErrorKind::Type);
            assert_eq!(err.to_string(), expected_message);
        }
    }

    #[tokio::test]
    async fn run_resolves_default_root_nested_and_lexically_aliased_cwd() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("nested/dir")).unwrap();
        fs::write(workspace.path().join("root.txt"), "root").unwrap();
        fs::write(workspace.path().join("nested/dir/nested.txt"), "nested").unwrap();
        let host = tool_host(&workspace).await;

        let res = run_tool(&host, vec![ToolValue::String("cat root.txt".to_owned())])
            .await
            .unwrap();
        let ToolValue::Dictionary(dict) = res else {
            panic!()
        };
        assert_eq!(dict.get("exit_code"), Some(&ToolValue::Integer(0)));
        assert_eq!(
            dict.get("stdout"),
            Some(&ToolValue::String("root".to_owned()))
        );

        for root_cwd in [".", ""] {
            let res = run_tool(
                &host,
                vec![
                    ToolValue::String("cat root.txt".to_owned()),
                    ToolValue::String(root_cwd.to_owned()),
                ],
            )
            .await
            .unwrap();
            let ToolValue::Dictionary(dict) = res else {
                panic!()
            };
            assert_eq!(dict.get("exit_code"), Some(&ToolValue::Integer(0)));
            assert_eq!(
                dict.get("stdout"),
                Some(&ToolValue::String("root".to_owned()))
            );
        }

        let res = run_tool(
            &host,
            vec![
                ToolValue::String("cat nested.txt".to_owned()),
                ToolValue::String("nested/dir".to_owned()),
            ],
        )
        .await
        .unwrap();
        let ToolValue::Dictionary(dict) = res else {
            panic!()
        };
        assert_eq!(dict.get("exit_code"), Some(&ToolValue::Integer(0)));
        assert_eq!(
            dict.get("stdout"),
            Some(&ToolValue::String("nested".to_owned()))
        );

        let res = run_tool(
            &host,
            vec![
                ToolValue::String("cat nested.txt".to_owned()),
                ToolValue::String("./nested//./dir//.".to_owned()),
            ],
        )
        .await
        .unwrap();
        let ToolValue::Dictionary(dict) = res else {
            panic!()
        };
        assert_eq!(dict.get("exit_code"), Some(&ToolValue::Integer(0)));
        assert_eq!(
            dict.get("stdout"),
            Some(&ToolValue::String("nested".to_owned()))
        );
    }

    #[tokio::test]
    async fn run_denies_absolute_and_parent_cwd() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        for bad_cwd in ["/tmp", "/", "..", "nested/..", "../nested"] {
            let err = run_error(
                &host,
                vec![
                    ToolValue::String("echo hi".to_owned()),
                    ToolValue::String(bad_cwd.to_owned()),
                ],
            )
            .await;
            assert_eq!(err, ToolErrorKind::Permission);
        }
    }

    #[tokio::test]
    async fn run_classifies_missing_non_directory_and_symlinked_cwd() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("real_dir/sub")).unwrap();
        fs::create_dir(outside.path().join("outside-sub")).unwrap();
        fs::write(workspace.path().join("file.txt"), "hello").unwrap();
        std::os::unix::fs::symlink("real_dir", workspace.path().join("link_dir")).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside_link")).unwrap();
        let host = tool_host(&workspace).await;

        let err = run_error(
            &host,
            vec![
                ToolValue::String("echo hi".to_owned()),
                ToolValue::String("missing_dir".to_owned()),
            ],
        )
        .await;
        assert_eq!(err, ToolErrorKind::FileNotFound);

        let err = run_error(
            &host,
            vec![
                ToolValue::String("echo hi".to_owned()),
                ToolValue::String("file.txt".to_owned()),
            ],
        )
        .await;
        assert_eq!(err, ToolErrorKind::NotADirectory);

        for symlink in ["link_dir", "outside_link"] {
            let err = run_error(
                &host,
                vec![
                    ToolValue::String("echo hi".to_owned()),
                    ToolValue::String(symlink.to_owned()),
                ],
            )
            .await;
            assert_eq!(err, ToolErrorKind::Permission);
        }

        for cwd in ["link_dir/sub", "outside_link/outside-sub"] {
            let err = run_error(
                &host,
                vec![
                    ToolValue::String("echo hi".to_owned()),
                    ToolValue::String(cwd.to_owned()),
                ],
            )
            .await;
            assert_eq!(err, ToolErrorKind::Permission);
        }
    }

    #[tokio::test]
    async fn run_returns_exact_result_dictionaries() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        let res0 = run_tool(&host, vec![ToolValue::String("exit 0".to_owned())])
            .await
            .unwrap();
        assert_eq!(
            res0,
            ToolValue::Dictionary(BTreeMap::from([
                ("exit_code".to_owned(), ToolValue::Integer(0)),
                ("stdout".to_owned(), ToolValue::String("".to_owned())),
                ("stderr".to_owned(), ToolValue::String("".to_owned())),
            ]))
        );

        let res42 = run_tool(
            &host,
            vec![ToolValue::String(
                "printf 'stdout text'; printf 'stderr text' >&2; exit 42".to_owned(),
            )],
        )
        .await
        .unwrap();
        assert_eq!(
            res42,
            ToolValue::Dictionary(BTreeMap::from([
                ("exit_code".to_owned(), ToolValue::Integer(42)),
                (
                    "stdout".to_owned(),
                    ToolValue::String("stdout text".to_owned())
                ),
                (
                    "stderr".to_owned(),
                    ToolValue::String("stderr text".to_owned())
                ),
            ]))
        );

        let res_sig = run_tool(&host, vec![ToolValue::String("kill -9 $$".to_owned())])
            .await
            .unwrap();
        assert_eq!(
            res_sig,
            ToolValue::Dictionary(BTreeMap::from([
                ("exit_code".to_owned(), ToolValue::None),
                ("stdout".to_owned(), ToolValue::String("".to_owned())),
                ("stderr".to_owned(), ToolValue::String("".to_owned())),
            ]))
        );
    }

    #[test]
    fn run_maps_command_errors_to_safe_tool_errors() {
        use super::command::CommandError;

        let overflow = super::map_command_error(CommandError::Overflow(
            "safe overflow diagnostic".to_owned(),
        ));
        assert_eq!(overflow.kind, ToolErrorKind::Runtime);
        assert_eq!(overflow.to_string(), "safe overflow diagnostic");

        for command_error in [CommandError::Launch, CommandError::Internal] {
            let error = super::map_command_error(command_error);
            assert_eq!(error.kind, ToolErrorKind::Os);
            assert_eq!(error.to_string(), "command execution failed");
        }
    }

    #[tokio::test]
    async fn run_maps_real_overflow_to_runtime_error() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;

        let overflow = run_call(
            &host,
            vec![ToolValue::String(
                "dd if=/dev/zero bs=8388609 count=1 2>/dev/null".to_owned(),
            )],
        )
        .await
        .unwrap_err();
        assert_eq!(overflow.kind, ToolErrorKind::Runtime);
        assert!(
            overflow
                .to_string()
                .contains("stdout exceeded the 8388608-byte limit by 1 bytes")
        );
    }

    #[tokio::test]
    async fn run_side_effects_leave_authorisations_unchanged() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("file.txt"), "hello").unwrap();
        let host = tool_host(&workspace).await;

        let path = normalise_path("file.txt").unwrap();
        let digest = content_fingerprint(b"hello");
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(path.clone(), digest);

        let result = host
            .call(batch_run(1, "printf changed > file.txt", None))
            .await;
        assert!(result.result.is_ok());
        assert!(result.effect.is_none());
        result.apply_batch_transition(&mut authorisations, &BTreeSet::new());
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
            "changed"
        );
        assert_eq!(authorisations.authorisation(&path), Some(digest));

        let stale_write = host
            .call_with_authorisation(
                batch_write(2, "file.txt", "replacement"),
                authorisations.authorisation(&path),
            )
            .await;
        assert!(matches!(
            stale_write.result,
            Err(ref error) if error.kind == ToolErrorKind::Runtime
        ));

        let result = host
            .call(batch_run(3, "echo fail", Some("missing_dir")))
            .await;
        assert!(result.result.is_err());
        assert!(result.effect.is_none());
        result.apply_batch_transition(&mut authorisations, &BTreeSet::new());
        assert_eq!(authorisations.authorisation(&path), Some(digest));
    }

    #[tokio::test]
    async fn dispatch_batch_admits_independent_commands_concurrently() {
        let workspace = TempDir::new().unwrap();
        let gate = super::BatchTestGate::new();
        let host = tool_host(&workspace)
            .await
            .with_batch_test_gate(gate.clone());
        let mut results = Box::pin(
            host.dispatch_batch(
                vec![
                    batch_run(1, "echo first", None),
                    batch_run(2, "echo second", None),
                ],
                WholeFileAuthorisations::default(),
            )
            .collect::<Vec<_>>(),
        );

        tokio::select! {
            _ = gate.wait_until_calls_started(2) => {}
            result = &mut results => panic!("batch finished before both commands started: {result:?}"),
        }
        let started = gate.started_call_ids().into_iter().collect::<BTreeSet<_>>();
        assert!(started.contains(&1));
        assert!(started.contains(&2));
        assert_eq!(gate.active_call_ids().len(), 2);

        gate.release(1);
        gate.release(2);

        let results = results.await;
        assert_eq!(results.len(), 2);
        assert!(results[0].result.is_ok());
        assert!(results[1].result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_batch_settles_successful_sibling_after_ordinary_command_error() {
        let workspace = TempDir::new().unwrap();
        let host = tool_host(&workspace).await;
        let results = host
            .dispatch_batch(
                vec![
                    batch_run(1, "echo fail", Some("missing_dir")),
                    batch_run(2, "printf sibling", None),
                ],
                WholeFileAuthorisations::default(),
            )
            .collect::<Vec<_>>()
            .await;

        assert_eq!(results.len(), 2);
        let first = results.iter().find(|r| r.call_id == 1).unwrap();
        let second = results.iter().find(|r| r.call_id == 2).unwrap();

        assert_eq!(
            first.result.as_ref().unwrap_err().kind,
            ToolErrorKind::FileNotFound
        );
        let ToolValue::Dictionary(dict) = second.result.as_ref().unwrap() else {
            panic!()
        };
        assert_eq!(dict.get("exit_code"), Some(&ToolValue::Integer(0)));
        assert_eq!(
            dict.get("stdout"),
            Some(&ToolValue::String("sibling".to_owned()))
        );
    }

    #[test]
    fn tool_name_run_properties() {
        assert_eq!(ToolName::Run.name(), "run");
        assert_eq!(ToolName::Run.private_name(), "_kiki_run");
        assert_eq!(ToolName::Run.label(), "run");
        assert_eq!(
            ToolName::from_private_name("_kiki_run"),
            Some(ToolName::Run)
        );

        let call = ToolCall {
            call_id: 1,
            tool: ToolName::Run,
            arguments: ToolValue::None,
        };
        assert_eq!(call.mutation_path(), None);

        let count_label_panic = std::panic::catch_unwind(|| ToolName::Run.count_label(1));
        assert!(count_label_panic.is_err());
    }
}
