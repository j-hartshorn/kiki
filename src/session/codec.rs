use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use thiserror::Error;

use super::{
    COMPACTION_DIAGNOSTIC_LIMIT, CompactionOutcome, CompactionReason, PersistedCode,
    PersistedTerminal, ReadableCode, ReadableTerminal, SessionEntryId,
};
use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
use crate::arena::{
    Checkpoint, Execution, ExecutionOutcome, Observation, PythonError, ToolCall, ToolCallSummary,
    ToolName,
};
use crate::config::{ModelSelection, SessionPreferences, model_by_id};
use crate::model::{
    AttemptAccounting, CodeResult, ModelCallId, ModelUsage, OpaqueReplayItem, ProviderReplay,
};
use crate::subagents::{ChildParameters, TaskOutcome, TaskPersistedDetails, TaskRecord, TaskState};
use crate::tools::{ContentFingerprint, ToolValue, WholeFileAuthorisations};

#[derive(Debug, Error)]
pub(super) enum CodecError {
    #[error("stored session JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("stored session path is not hexadecimal")]
    InvalidPath,
    #[error("stored execution path is not normalised workspace-relative")]
    InvalidExecutionPath,
    #[error("stored content fingerprint is not a 32-byte hexadecimal value")]
    InvalidFingerprint,
    #[error("stored session model selection is invalid")]
    InvalidSelection,
    #[error("stored session execution is missing")]
    MissingExecution,
    #[error("stored session payload has unexpected kind")]
    InvalidPayload,
    #[error("stored unfinished subagent task has terminal details")]
    TerminalTaskDetails,
    #[error("stored session replay is missing")]
    MissingReplay,
    #[error("stored compaction diagnostic exceeds 4096 bytes")]
    OversizedCompactionDiagnostic,
    #[cfg(not(unix))]
    #[error("session paths are supported only on Unix")]
    UnsupportedPath,
}

#[derive(Serialize, Deserialize)]
struct StoredAuthorisation {
    path: String,
    fingerprint: String,
}

pub(super) struct StoredCheckpoint {
    pub(super) runtime_identity: String,
    pub(super) arena_state: Vec<u8>,
    pub(super) authorisations_json: String,
}

pub(super) struct StoredEntry {
    pub(super) kind: &'static str,
    pub(super) payload_json: String,
    pub(super) provider_replay: Option<Vec<u8>>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum StoredPayload {
    UserRequest {
        request: String,
        #[serde(default)]
        selection: Option<StoredSelection>,
    },
    Execution {
        code: StoredCode,
        execution: StoredExecution,
    },
    Terminal {
        terminal: StoredTerminal,
        code: Option<StoredCode>,
    },
    Compaction {
        compaction: StoredCompactionOutcome,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum StoredCompactionOutcome {
    Applied {
        summary: String,
        first_retained: u64,
        context_tokens: u64,
        reason: StoredCompactionReason,
        attempts: Vec<StoredAttemptAccounting>,
    },
    Failed {
        diagnostic: String,
        reason: StoredCompactionReason,
        attempts: Vec<StoredAttemptAccounting>,
    },
    Cancelled {
        reason: StoredCompactionReason,
        attempts: Vec<StoredAttemptAccounting>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredCompactionReason {
    Manual,
    AutomaticThreshold,
    ContextOverflow,
}

pub(super) struct DecodedEntry(DecodedEntryPayload);

#[allow(clippy::large_enum_variant)]
enum DecodedEntryPayload {
    SessionStart,
    UserRequest {
        request: String,
        selection: Option<StoredSelection>,
    },
    Execution {
        code: StoredCode,
        execution: StoredExecution,
    },
    Terminal {
        terminal: StoredTerminal,
        code: Option<StoredCode>,
    },
    Compaction(StoredCompactionOutcome),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DecodedEntryKind {
    SessionStart,
    UserRequest,
    Execution,
    FinalReply,
    Cancelled,
    Failed,
    Compaction,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DecodedCodeResult {
    Execution,
    Cancelled,
    Failure,
}

pub(super) enum DecodedTerminalHistory {
    Final {
        text: String,
        reasoning_summary: String,
        replay: ProviderReplay,
    },
    Cancelled {
        code: Option<PersistedCode>,
    },
    Failed {
        code: Option<PersistedCode>,
    },
}

#[derive(Serialize, Deserialize)]
struct StoredCode {
    source: String,
    call_id: String,
    commentary: Option<String>,
    reasoning_summary: String,
    result: StoredCodeResult,
    attempts: Vec<StoredAttemptAccounting>,
}

#[derive(Serialize, Deserialize)]
struct StoredSelection {
    model_id: String,
    thinking: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredSessionPreferences {
    selected_model: String,
    thinking: BTreeMap<String, String>,
    #[serde(default)]
    type_check_disabled: bool,
}

#[derive(Serialize, Deserialize)]
enum StoredCodeResult {
    Execution { feedback: String },
    Cancelled { feedback: String },
    Failure { feedback: String },
}

#[derive(Serialize, Deserialize)]
struct StoredExecution {
    stdout: StoredObservation,
    stderr: StoredObservation,
    outcome: StoredExecutionOutcome,
    tool_calls: Vec<StoredToolCall>,
    read_paths: Vec<String>,
    modified_paths: Vec<String>,
}

#[derive(Serialize, Deserialize)]
enum StoredExecutionOutcome {
    Complete { result: Option<StoredObservation> },
    PythonError { text: String, omitted_bytes: usize },
    TypeCheckError { text: String, omitted_bytes: usize },
}

#[derive(Serialize, Deserialize)]
struct StoredObservation {
    head: String,
    tail: Option<String>,
    head_lines: usize,
    tail_lines: usize,
    total_lines: usize,
    total_bytes: usize,
    omitted_lines: usize,
    omitted_bytes: usize,
    head_ends_mid_line: bool,
    tail_starts_mid_line: bool,
    partial_lines: bool,
}

#[derive(Serialize, Deserialize)]
struct StoredToolCall {
    tool: StoredToolName,
    summary: StoredToolCallSummary,
}

#[derive(Serialize, Deserialize)]
enum StoredToolName {
    Read,
    Write,
    Edit,
    ListDir,
    Glob,
    Grep,
    Run,
    StartSubagent,
    GetSubagent,
    RefreshSubagent,
    WaitSubagent,
    CancelSubagent,
}

#[derive(Serialize, Deserialize)]
enum StoredToolCallSummary {
    Pending,
    Failed,
    Exit(i64),
    Signal,
    Count(usize),
    Created,
    Replaced,
    Applied,
    Subagent(String),
}

#[derive(Serialize, Deserialize)]
enum StoredTerminal {
    Final {
        text: String,
        reasoning_summary: String,
        attempts: Vec<StoredAttemptAccounting>,
    },
    Cancelled {
        evidence: Option<StoredTerminalEvidence>,
        attempts: Vec<StoredAttemptAccounting>,
    },
    Failed {
        message: String,
        evidence: Option<StoredTerminalEvidence>,
        attempts: Vec<StoredAttemptAccounting>,
    },
}

#[derive(Serialize, Deserialize)]
enum StoredTerminalEvidence {
    Generation(StoredGenerationEvidence),
    Execution(Box<StoredExecutionEvidence>),
}

#[derive(Serialize, Deserialize)]
struct StoredGenerationEvidence {
    reasoning_summary: String,
    reasoning: String,
    assistant_text: String,
    decoded_python: String,
}

#[derive(Serialize, Deserialize)]
struct StoredExecutionEvidence {
    source: String,
    commentary: Option<String>,
    reasoning_summary: String,
    began: bool,
    stdout: StoredObservation,
    stderr: StoredObservation,
    tool_calls: Vec<StoredToolCall>,
    read_paths: Vec<String>,
    modified_paths: Vec<String>,
    cancellation_warning: Option<String>,
    failure: Option<StoredExecutionFailure>,
}

#[derive(Serialize, Deserialize)]
enum StoredExecutionFailure {
    Arena { message: String },
    RecoveryFailed { original: String, recovery: String },
}

#[derive(Serialize, Deserialize)]
enum StoredAttemptAccounting {
    KnownUsage(StoredModelUsage),
    KnownUsageWithUntrackedRetries(StoredModelUsage),
    UsageUnavailable,
}

#[derive(Serialize, Deserialize)]
struct StoredModelUsage {
    model: String,
    service_tier: Option<String>,
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    #[serde(default)]
    cache_write_1h_input_tokens: u64,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct StoredReplay {
    provider: String,
    items: Vec<String>,
}

#[derive(Serialize, Deserialize)]
enum StoredToolValue {
    None,
    Bool(bool),
    Integer(i64),
    FloatBits(u64),
    String(String),
    List(Vec<StoredToolValue>),
    Dictionary(BTreeMap<String, StoredToolValue>),
}

#[derive(Serialize, Deserialize)]
struct StoredTaskDetails {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    admission_history_step_id: Option<u64>,
    #[serde(default)]
    terminal_history_step_id: Option<u64>,
    prompt: String,
    selection: StoredSelection,
    read_only: bool,
    depth: usize,
    admitted_at_ms: i64,
    terminal_at_ms: Option<i64>,
    observation: StoredTaskObservation,
    attempt_evidence: Vec<StoredTaskAttempt>,
    attempts: Vec<StoredAttemptAccounting>,
    executions: Vec<StoredTaskExecution>,
    outcome: Option<StoredTaskOutcome>,
}

#[derive(Serialize, Deserialize)]
struct StoredTaskObservation {
    step_count: u64,
    elapsed_seconds: u64,
    elapsed_subsec_nanos: u32,
    observed_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
struct StoredTaskAttempt {
    model_step: u64,
    provider_attempt: u32,
    accounting: Option<StoredAttemptAccounting>,
}

#[derive(Serialize, Deserialize)]
struct StoredTaskExecution {
    model_step: u64,
    source: String,
    reasoning_summary: String,
    execution: StoredExecution,
    attempts: Vec<StoredAttemptAccounting>,
}

#[derive(Serialize, Deserialize)]
enum StoredTaskOutcome {
    Completed {
        message: String,
        values: BTreeMap<String, StoredToolValue>,
    },
    Failed {
        message: String,
    },
    Cancelled,
    TimedOut,
}

pub(super) struct DecodedTaskDetails {
    pub(super) id: Option<String>,
    pub(super) admission_history_step_id: Option<u64>,
    pub(super) terminal_history_step_id: Option<u64>,
    pub(super) parameters: ChildParameters,
    pub(super) depth: usize,
    pub(super) details: TaskPersistedDetails,
    pub(super) attempts: Vec<AttemptAccounting>,
}

pub(super) fn encode_checkpoint(checkpoint: &Checkpoint) -> Result<StoredCheckpoint, CodecError> {
    let (identity, state, authorisations) = checkpoint.session_parts();
    let authorisations = authorisations
        .session_entries()
        .map(|(path, fingerprint)| {
            Ok(StoredAuthorisation {
                path: encode_path(path)?,
                fingerprint: encode_hex(&fingerprint.session_bytes()),
            })
        })
        .collect::<Result<Vec<_>, CodecError>>()?;
    Ok(StoredCheckpoint {
        runtime_identity: identity.session_value().to_owned(),
        arena_state: state.to_vec(),
        authorisations_json: serde_json::to_string(&authorisations)?,
    })
}

pub(super) fn decode_checkpoint(
    runtime_identity: String,
    arena_state: Vec<u8>,
    authorisations_json: &str,
) -> Result<Checkpoint, CodecError> {
    let records: Vec<StoredAuthorisation> = serde_json::from_str(authorisations_json)?;
    let mut authorisations = WholeFileAuthorisations::default();
    for record in records {
        let path = decode_path(&record.path)?;
        let fingerprint = decode_fingerprint(&record.fingerprint)?;
        authorisations.authorise(path, fingerprint);
    }
    Ok(Checkpoint::from_session_parts(
        crate::arena::RuntimeIdentity::from_session_value(runtime_identity),
        arena_state,
        authorisations,
    ))
}

pub(super) fn encode_request(
    request: &str,
    selection: ModelSelection,
) -> Result<String, CodecError> {
    serde_json::to_string(&StoredPayload::UserRequest {
        request: request.to_owned(),
        selection: Some(encode_selection(selection)),
    })
    .map_err(CodecError::from)
}

pub(super) fn decode_request(payload_json: &str) -> Result<String, CodecError> {
    let StoredPayload::UserRequest { request, .. } = serde_json::from_str(payload_json)? else {
        return Err(CodecError::InvalidPayload);
    };
    Ok(request)
}

pub(super) fn decode_entry(kind: &str, payload_json: &str) -> Result<DecodedEntry, CodecError> {
    if kind == "session_start" {
        return (payload_json == "{}")
            .then_some(DecodedEntry(DecodedEntryPayload::SessionStart))
            .ok_or(CodecError::InvalidPayload);
    }
    let payload: StoredPayload = serde_json::from_str(payload_json)?;
    match (kind, payload) {
        ("user_request", StoredPayload::UserRequest { request, selection }) => {
            Ok(DecodedEntry(DecodedEntryPayload::UserRequest {
                request,
                selection,
            }))
        }
        ("execution", StoredPayload::Execution { code, execution }) => {
            Ok(DecodedEntry(DecodedEntryPayload::Execution {
                code,
                execution,
            }))
        }
        ("final_reply", StoredPayload::Terminal { terminal, code })
            if matches!(terminal, StoredTerminal::Final { .. }) =>
        {
            Ok(DecodedEntry(DecodedEntryPayload::Terminal {
                terminal,
                code,
            }))
        }
        ("cancelled", StoredPayload::Terminal { terminal, code })
            if matches!(terminal, StoredTerminal::Cancelled { .. }) =>
        {
            Ok(DecodedEntry(DecodedEntryPayload::Terminal {
                terminal,
                code,
            }))
        }
        ("failed", StoredPayload::Terminal { terminal, code })
            if matches!(terminal, StoredTerminal::Failed { .. }) =>
        {
            Ok(DecodedEntry(DecodedEntryPayload::Terminal {
                terminal,
                code,
            }))
        }
        ("compaction", StoredPayload::Compaction { compaction }) => {
            validate_stored_compaction(&compaction)?;
            Ok(DecodedEntry(DecodedEntryPayload::Compaction(compaction)))
        }
        _ => Err(CodecError::InvalidPayload),
    }
}

pub(super) fn decoded_entry_kind(entry: &DecodedEntry) -> DecodedEntryKind {
    match &entry.0 {
        DecodedEntryPayload::SessionStart => DecodedEntryKind::SessionStart,
        DecodedEntryPayload::UserRequest { .. } => DecodedEntryKind::UserRequest,
        DecodedEntryPayload::Execution { .. } => DecodedEntryKind::Execution,
        DecodedEntryPayload::Terminal { terminal, .. } => match terminal {
            StoredTerminal::Final { .. } => DecodedEntryKind::FinalReply,
            StoredTerminal::Cancelled { .. } => DecodedEntryKind::Cancelled,
            StoredTerminal::Failed { .. } => DecodedEntryKind::Failed,
        },
        DecodedEntryPayload::Compaction(_) => DecodedEntryKind::Compaction,
    }
}

pub(super) fn decoded_request(entry: &DecodedEntry) -> Option<&str> {
    let DecodedEntryPayload::UserRequest { request, .. } = &entry.0 else {
        return None;
    };
    Some(request)
}

// Display-only: an undecodable selection must not make the session unreadable.
pub(super) fn decoded_request_selection(entry: &DecodedEntry) -> Option<ModelSelection> {
    let DecodedEntryPayload::UserRequest {
        selection: Some(selection),
        ..
    } = &entry.0
    else {
        return None;
    };
    decode_selection(selection).ok()
}

pub(super) fn decoded_final_text(entry: &DecodedEntry) -> Option<&str> {
    let DecodedEntryPayload::Terminal {
        terminal: StoredTerminal::Final { text, .. },
        code: None,
    } = &entry.0
    else {
        return None;
    };
    Some(text)
}

pub(super) fn decoded_terminal_code_result(
    entry: &DecodedEntry,
) -> Option<Option<DecodedCodeResult>> {
    let DecodedEntryPayload::Terminal { code, .. } = &entry.0 else {
        return None;
    };
    Some(
        code.as_ref()
            .map(|code| decoded_code_result_kind(&code.result)),
    )
}

pub(super) fn decoded_attempts(entry: &DecodedEntry) -> Vec<AttemptAccounting> {
    match &entry.0 {
        DecodedEntryPayload::SessionStart | DecodedEntryPayload::UserRequest { .. } => Vec::new(),
        DecodedEntryPayload::Execution { code, .. } => {
            code.attempts.iter().map(decode_attempt).collect()
        }
        DecodedEntryPayload::Terminal { terminal, .. } => match terminal {
            StoredTerminal::Final { attempts, .. }
            | StoredTerminal::Cancelled { attempts, .. }
            | StoredTerminal::Failed { attempts, .. } => {
                attempts.iter().map(decode_attempt).collect()
            }
        },
        DecodedEntryPayload::Compaction(outcome) => stored_compaction_attempts(outcome)
            .iter()
            .map(decode_attempt)
            .collect(),
    }
}

pub(super) fn decoded_compaction(entry: &DecodedEntry) -> Option<CompactionOutcome> {
    let DecodedEntryPayload::Compaction(outcome) = &entry.0 else {
        return None;
    };
    Some(decode_compaction_outcome(outcome))
}

fn validate_stored_compaction(outcome: &StoredCompactionOutcome) -> Result<(), CodecError> {
    match outcome {
        StoredCompactionOutcome::Applied { first_retained, .. } if *first_retained == 0 => {
            Err(CodecError::InvalidPayload)
        }
        StoredCompactionOutcome::Failed { diagnostic, .. }
            if diagnostic.len() > COMPACTION_DIAGNOSTIC_LIMIT =>
        {
            Err(CodecError::OversizedCompactionDiagnostic)
        }
        StoredCompactionOutcome::Applied { .. }
        | StoredCompactionOutcome::Failed { .. }
        | StoredCompactionOutcome::Cancelled { .. } => Ok(()),
    }
}

fn stored_compaction_attempts(outcome: &StoredCompactionOutcome) -> &[StoredAttemptAccounting] {
    match outcome {
        StoredCompactionOutcome::Applied { attempts, .. }
        | StoredCompactionOutcome::Failed { attempts, .. }
        | StoredCompactionOutcome::Cancelled { attempts, .. } => attempts,
    }
}

fn decode_compaction_outcome(outcome: &StoredCompactionOutcome) -> CompactionOutcome {
    match outcome {
        StoredCompactionOutcome::Applied {
            summary,
            first_retained,
            context_tokens,
            reason,
            attempts,
        } => CompactionOutcome::Applied {
            summary: summary.clone(),
            first_retained: SessionEntryId(*first_retained),
            context_tokens: *context_tokens,
            reason: decode_compaction_reason(reason),
            attempts: attempts.iter().map(decode_attempt).collect(),
        },
        StoredCompactionOutcome::Failed {
            diagnostic,
            reason,
            attempts,
        } => CompactionOutcome::Failed {
            diagnostic: diagnostic.clone(),
            reason: decode_compaction_reason(reason),
            attempts: attempts.iter().map(decode_attempt).collect(),
        },
        StoredCompactionOutcome::Cancelled { reason, attempts } => CompactionOutcome::Cancelled {
            reason: decode_compaction_reason(reason),
            attempts: attempts.iter().map(decode_attempt).collect(),
        },
    }
}

fn decode_compaction_reason(reason: &StoredCompactionReason) -> CompactionReason {
    match reason {
        StoredCompactionReason::Manual => CompactionReason::Manual,
        StoredCompactionReason::AutomaticThreshold => CompactionReason::AutomaticThreshold,
        StoredCompactionReason::ContextOverflow => CompactionReason::ContextOverflow,
    }
}

fn encode_compaction_reason(reason: CompactionReason) -> StoredCompactionReason {
    match reason {
        CompactionReason::Manual => StoredCompactionReason::Manual,
        CompactionReason::AutomaticThreshold => StoredCompactionReason::AutomaticThreshold,
        CompactionReason::ContextOverflow => StoredCompactionReason::ContextOverflow,
    }
}

pub(super) fn decoded_readable_execution(
    entry: &DecodedEntry,
) -> Result<Option<(ReadableCode, Execution)>, CodecError> {
    let DecodedEntryPayload::Execution { code, execution } = &entry.0 else {
        return Ok(None);
    };
    Ok(Some((
        decode_readable_code(code),
        decode_execution_value(execution)?,
    )))
}

pub(super) fn decoded_readable_terminal(
    entry: &DecodedEntry,
) -> Result<Option<ReadableTerminal>, CodecError> {
    let DecodedEntryPayload::Terminal { terminal, .. } = &entry.0 else {
        return Ok(None);
    };
    Ok(Some(decode_readable_terminal_value(terminal)?))
}

pub(super) fn decoded_execution_history(
    entry: &DecodedEntry,
    provider_replay: Option<&[u8]>,
) -> Result<PersistedCode, CodecError> {
    let DecodedEntryPayload::Execution { code, .. } = &entry.0 else {
        return Err(CodecError::InvalidPayload);
    };
    Ok(decode_code(code, decode_replay(provider_replay)?))
}

pub(super) fn decoded_terminal_history(
    entry: &DecodedEntry,
    provider_replay: Option<&[u8]>,
) -> Result<DecodedTerminalHistory, CodecError> {
    let DecodedEntryPayload::Terminal { terminal, code } = &entry.0 else {
        return Err(CodecError::InvalidPayload);
    };
    match terminal {
        StoredTerminal::Final {
            text,
            reasoning_summary,
            ..
        } if code.is_none() => Ok(DecodedTerminalHistory::Final {
            text: text.clone(),
            reasoning_summary: reasoning_summary.clone(),
            replay: decode_replay(provider_replay)?,
        }),
        StoredTerminal::Cancelled { .. } => Ok(DecodedTerminalHistory::Cancelled {
            code: decoded_terminal_code(code.as_ref(), provider_replay)?,
        }),
        StoredTerminal::Failed { .. } => Ok(DecodedTerminalHistory::Failed {
            code: decoded_terminal_code(code.as_ref(), provider_replay)?,
        }),
        StoredTerminal::Final { .. } => Err(CodecError::InvalidPayload),
    }
}

fn decoded_terminal_code(
    code: Option<&StoredCode>,
    provider_replay: Option<&[u8]>,
) -> Result<Option<PersistedCode>, CodecError> {
    let replay = provider_replay
        .map(|replay| decode_replay(Some(replay)))
        .transpose()?;
    code.map(|code| {
        Ok(decode_code(
            code,
            replay.clone().ok_or(CodecError::MissingReplay)?,
        ))
    })
    .transpose()
}

fn decoded_code_result_kind(result: &StoredCodeResult) -> DecodedCodeResult {
    match result {
        StoredCodeResult::Execution { .. } => DecodedCodeResult::Execution,
        StoredCodeResult::Cancelled { .. } => DecodedCodeResult::Cancelled,
        StoredCodeResult::Failure { .. } => DecodedCodeResult::Failure,
    }
}

pub(super) fn encode_task_details(record: &TaskRecord) -> Result<String, CodecError> {
    let parameters = record.parameters();
    let observation = record.observation();
    serde_json::to_string(&StoredTaskDetails {
        id: Some(record.id().to_string()),
        admission_history_step_id: record.admission_history_step_id(),
        terminal_history_step_id: record.terminal_history_step_id(),
        prompt: parameters.prompt().to_owned(),
        selection: encode_selection(parameters.selection()),
        read_only: parameters.read_only(),
        depth: record.depth(),
        admitted_at_ms: record.admitted_at_ms(),
        terminal_at_ms: record.terminal_at_ms(),
        observation: StoredTaskObservation {
            step_count: observation.step_count(),
            elapsed_seconds: observation.elapsed().as_secs(),
            elapsed_subsec_nanos: observation.elapsed().subsec_nanos(),
            observed_at_ms: observation.observed_at_ms(),
        },
        attempt_evidence: record
            .attempt_evidence()
            .iter()
            .map(|attempt| StoredTaskAttempt {
                model_step: attempt.model_step(),
                provider_attempt: attempt.provider_attempt(),
                accounting: attempt.accounting().map(encode_attempt),
            })
            .collect(),
        attempts: record.attempts().iter().map(encode_attempt).collect(),
        executions: record
            .executions()
            .iter()
            .map(|execution| {
                Ok(StoredTaskExecution {
                    model_step: execution.model_step(),
                    source: execution.source().to_owned(),
                    reasoning_summary: execution.reasoning_summary().to_owned(),
                    execution: encode_execution_value(execution.execution())?,
                    attempts: execution.attempts().iter().map(encode_attempt).collect(),
                })
            })
            .collect::<Result<Vec<_>, CodecError>>()?,
        outcome: record.outcome().map(|outcome| match outcome {
            TaskOutcome::Completed(result) => StoredTaskOutcome::Completed {
                message: result.message().to_owned(),
                values: result
                    .values()
                    .iter()
                    .map(|(key, value)| (key.clone(), encode_tool_value(value)))
                    .collect(),
            },
            TaskOutcome::Failed(failure) => StoredTaskOutcome::Failed {
                message: failure.message().to_owned(),
            },
            TaskOutcome::Cancelled => StoredTaskOutcome::Cancelled,
            TaskOutcome::TimedOut => StoredTaskOutcome::TimedOut,
        }),
    })
    .map_err(CodecError::from)
}

pub(super) fn fail_unfinished_task_details(
    value: &str,
    terminal_at_ms: i64,
) -> Result<String, CodecError> {
    let mut details: StoredTaskDetails = serde_json::from_str(value)?;
    if details.terminal_at_ms.is_some() || details.outcome.is_some() {
        return Err(CodecError::TerminalTaskDetails);
    }
    details.terminal_at_ms = Some(terminal_at_ms);
    details.outcome = Some(StoredTaskOutcome::Failed {
        message: super::INTERRUPTION_MESSAGE.to_owned(),
    });
    serde_json::to_string(&details).map_err(CodecError::from)
}

pub(super) fn decode_task_record_details(value: &str) -> Result<DecodedTaskDetails, CodecError> {
    let details: StoredTaskDetails = serde_json::from_str(value)?;
    let parameters = ChildParameters::new(
        details.prompt,
        decode_selection(&details.selection)?,
        details.read_only,
    );
    let outcome = details.outcome.map(decode_task_outcome);
    Ok(DecodedTaskDetails {
        id: details.id,
        admission_history_step_id: details.admission_history_step_id,
        terminal_history_step_id: details.terminal_history_step_id,
        parameters,
        depth: details.depth,
        attempts: details
            .attempts
            .into_iter()
            .map(|attempt| decode_attempt(&attempt))
            .collect(),
        details: TaskPersistedDetails {
            admitted_at_ms: details.admitted_at_ms,
            terminal_at_ms: details.terminal_at_ms,
            state: TaskState::Starting,
            step_count: details.observation.step_count,
            elapsed: std::time::Duration::new(
                details.observation.elapsed_seconds,
                details.observation.elapsed_subsec_nanos,
            ),
            observed_at_ms: details.observation.observed_at_ms,
            attempt_evidence: details
                .attempt_evidence
                .into_iter()
                .map(|attempt| {
                    crate::subagents::TaskAttempt::for_persisted(
                        attempt.model_step,
                        attempt.provider_attempt,
                        attempt.accounting.as_ref().map(decode_attempt),
                    )
                })
                .collect(),
            executions: details
                .executions
                .into_iter()
                .map(|execution| {
                    Ok(crate::subagents::TaskExecution::for_persisted(
                        execution.model_step,
                        execution.source,
                        execution.reasoning_summary,
                        decode_execution_value(&execution.execution)?,
                        execution
                            .attempts
                            .into_iter()
                            .map(|attempt| decode_attempt(&attempt))
                            .collect(),
                    ))
                })
                .collect::<Result<Vec<_>, CodecError>>()?,
            outcome,
        },
    })
}

#[cfg(test)]
fn decode_task_details(value: &str) -> Result<StoredTaskDetails, CodecError> {
    serde_json::from_str(value).map_err(CodecError::from)
}

pub(super) fn encode_compaction(outcome: &CompactionOutcome) -> Result<StoredEntry, CodecError> {
    let compaction = match outcome {
        CompactionOutcome::Applied {
            summary,
            first_retained,
            context_tokens,
            reason,
            attempts,
        } => StoredCompactionOutcome::Applied {
            summary: summary.clone(),
            first_retained: first_retained.0,
            context_tokens: *context_tokens,
            reason: encode_compaction_reason(*reason),
            attempts: attempts.iter().map(encode_attempt).collect(),
        },
        CompactionOutcome::Failed {
            diagnostic,
            reason,
            attempts,
        } => StoredCompactionOutcome::Failed {
            diagnostic: diagnostic.clone(),
            reason: encode_compaction_reason(*reason),
            attempts: attempts.iter().map(encode_attempt).collect(),
        },
        CompactionOutcome::Cancelled { reason, attempts } => StoredCompactionOutcome::Cancelled {
            reason: encode_compaction_reason(*reason),
            attempts: attempts.iter().map(encode_attempt).collect(),
        },
    };
    validate_stored_compaction(&compaction)?;
    Ok(StoredEntry {
        kind: "compaction",
        payload_json: serde_json::to_string(&StoredPayload::Compaction { compaction })?,
        provider_replay: None,
    })
}

pub(super) fn encode_execution(code: &PersistedCode) -> Result<StoredEntry, CodecError> {
    let execution = code
        .execution
        .as_ref()
        .ok_or(CodecError::MissingExecution)?;
    Ok(StoredEntry {
        kind: "execution",
        payload_json: serde_json::to_string(&StoredPayload::Execution {
            code: encode_code(code),
            execution: encode_execution_value(execution)?,
        })?,
        provider_replay: Some(encode_replay(&code.replay)?),
    })
}

pub(super) fn encode_terminal(
    terminal: &PersistedTerminal,
    code: Option<&PersistedCode>,
) -> Result<StoredEntry, CodecError> {
    let (kind, replay) = match terminal {
        PersistedTerminal::Final { replay, .. } => ("final_reply", Some(encode_replay(replay)?)),
        PersistedTerminal::Cancelled { .. } => (
            "cancelled",
            code.map(|code| encode_replay(&code.replay)).transpose()?,
        ),
        PersistedTerminal::Failed { .. } => (
            "failed",
            code.map(|code| encode_replay(&code.replay)).transpose()?,
        ),
    };
    Ok(StoredEntry {
        kind,
        payload_json: serde_json::to_string(&StoredPayload::Terminal {
            terminal: encode_terminal_value(terminal)?,
            code: code.map(|code| {
                let mut code = encode_code(code);
                code.attempts.clear();
                code
            }),
        })?,
        provider_replay: replay,
    })
}

fn decode_readable_terminal_value(
    terminal: &StoredTerminal,
) -> Result<ReadableTerminal, CodecError> {
    Ok(match terminal {
        StoredTerminal::Final {
            text,
            reasoning_summary,
            attempts,
        } => ReadableTerminal::Final {
            text: text.clone(),
            reasoning_summary: reasoning_summary.clone(),
            attempts: attempts.iter().map(decode_attempt).collect(),
        },
        StoredTerminal::Cancelled { evidence, attempts } => ReadableTerminal::Cancelled {
            evidence: evidence.as_ref().map(decode_evidence).transpose()?,
            attempts: attempts.iter().map(decode_attempt).collect(),
        },
        StoredTerminal::Failed {
            message,
            evidence,
            attempts,
        } => ReadableTerminal::Failed {
            message: message.clone(),
            evidence: evidence.as_ref().map(decode_evidence).transpose()?,
            attempts: attempts.iter().map(decode_attempt).collect(),
        },
    })
}

fn decode_readable_code(code: &StoredCode) -> ReadableCode {
    ReadableCode {
        source: code.source.clone(),
        commentary: code.commentary.clone(),
        reasoning_summary: code.reasoning_summary.clone(),
        result: decode_code_result(&code.result),
        attempts: code.attempts.iter().map(decode_attempt).collect(),
    }
}

fn encode_code(code: &PersistedCode) -> StoredCode {
    StoredCode {
        source: code.source.clone(),
        call_id: code.call_id.as_str().to_owned(),
        commentary: code.commentary.clone(),
        reasoning_summary: code.reasoning_summary.clone(),
        result: encode_code_result(&code.result),
        attempts: code.attempts.iter().map(encode_attempt).collect(),
    }
}

fn decode_code(code: &StoredCode, replay: ProviderReplay) -> PersistedCode {
    PersistedCode {
        source: code.source.clone(),
        call_id: ModelCallId::new(code.call_id.clone()),
        commentary: code.commentary.clone(),
        reasoning_summary: code.reasoning_summary.clone(),
        replay,
        result: decode_code_result(&code.result),
        execution: None,
        attempts: code.attempts.iter().map(decode_attempt).collect(),
    }
}

fn encode_selection(selection: ModelSelection) -> StoredSelection {
    StoredSelection {
        model_id: selection.model().id.to_owned(),
        thinking: selection.thinking().to_owned(),
    }
}

fn decode_selection(selection: &StoredSelection) -> Result<ModelSelection, CodecError> {
    model_by_id(&selection.model_id)
        .and_then(|model| {
            model
                .thinking_levels
                .iter()
                .copied()
                .find(|thinking| *thinking == selection.thinking)
                .and_then(|thinking| ModelSelection::new(model, thinking))
        })
        .ok_or(CodecError::InvalidSelection)
}

pub(super) fn encode_session_preferences(
    preferences: &SessionPreferences,
    type_check_disabled: bool,
) -> Result<String, CodecError> {
    serde_json::to_string(&StoredSessionPreferences {
        selected_model: preferences.selection().model().id.to_owned(),
        thinking: preferences
            .thinking()
            .iter()
            .map(|(model, thinking)| ((*model).to_owned(), (*thinking).to_owned()))
            .collect(),
        type_check_disabled,
    })
    .map_err(CodecError::from)
}

pub(super) fn decode_session_preferences(
    value: &str,
) -> Result<(SessionPreferences, bool), CodecError> {
    let stored: StoredSessionPreferences = serde_json::from_str(value)?;
    SessionPreferences::from_ids(stored.selected_model.as_str(), stored.thinking)
        .map(|preferences| (preferences, stored.type_check_disabled))
        .ok_or(CodecError::InvalidSelection)
}

fn encode_code_result(result: &CodeResult) -> StoredCodeResult {
    match result {
        CodeResult::Execution { feedback } => StoredCodeResult::Execution {
            feedback: feedback.clone(),
        },
        CodeResult::Cancelled { feedback } => StoredCodeResult::Cancelled {
            feedback: feedback.clone(),
        },
        CodeResult::Failure { feedback } => StoredCodeResult::Failure {
            feedback: feedback.clone(),
        },
    }
}

fn decode_code_result(result: &StoredCodeResult) -> CodeResult {
    match result {
        StoredCodeResult::Execution { feedback } => CodeResult::Execution {
            feedback: feedback.clone(),
        },
        StoredCodeResult::Cancelled { feedback } => CodeResult::Cancelled {
            feedback: feedback.clone(),
        },
        StoredCodeResult::Failure { feedback } => CodeResult::Failure {
            feedback: feedback.clone(),
        },
    }
}

fn encode_execution_value(execution: &Execution) -> Result<StoredExecution, CodecError> {
    Ok(StoredExecution {
        stdout: encode_observation(&execution.stdout),
        stderr: encode_observation(&execution.stderr),
        outcome: match &execution.outcome {
            ExecutionOutcome::Complete { result } => StoredExecutionOutcome::Complete {
                result: result.as_ref().map(encode_observation),
            },
            ExecutionOutcome::PythonError(error) => StoredExecutionOutcome::PythonError {
                text: error.text.clone(),
                omitted_bytes: error.omitted_bytes,
            },
            ExecutionOutcome::TypeCheckError(error) => StoredExecutionOutcome::TypeCheckError {
                text: error.text.clone(),
                omitted_bytes: error.omitted_bytes,
            },
        },
        tool_calls: execution.tool_calls.iter().map(encode_tool_call).collect(),
        read_paths: encode_effect_paths(&execution.read_paths)?,
        modified_paths: encode_effect_paths(&execution.modified_paths)?,
    })
}

fn decode_execution_value(execution: &StoredExecution) -> Result<Execution, CodecError> {
    Ok(Execution {
        stdout: decode_observation(&execution.stdout),
        stderr: decode_observation(&execution.stderr),
        outcome: match &execution.outcome {
            StoredExecutionOutcome::Complete { result } => ExecutionOutcome::Complete {
                result: result.as_ref().map(decode_observation),
            },
            StoredExecutionOutcome::PythonError {
                text,
                omitted_bytes,
            } => ExecutionOutcome::PythonError(PythonError {
                text: text.clone(),
                omitted_bytes: *omitted_bytes,
            }),
            StoredExecutionOutcome::TypeCheckError {
                text,
                omitted_bytes,
            } => ExecutionOutcome::TypeCheckError(PythonError {
                text: text.clone(),
                omitted_bytes: *omitted_bytes,
            }),
        },
        tool_calls: execution
            .tool_calls
            .iter()
            .map(decode_tool_call)
            .collect::<Result<Vec<_>, _>>()?,
        read_paths: decode_effect_paths(&execution.read_paths)?,
        modified_paths: decode_effect_paths(&execution.modified_paths)?,
    })
}

fn encode_effect_paths(paths: &[PathBuf]) -> Result<Vec<String>, CodecError> {
    paths
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| {
            if !normalised_workspace_relative(path) {
                return Err(CodecError::InvalidExecutionPath);
            }
            encode_path(path)
        })
        .collect()
}

fn decode_effect_paths(paths: &[String]) -> Result<Vec<PathBuf>, CodecError> {
    let paths = paths
        .iter()
        .map(|path| {
            let path = decode_path(path)?;
            normalised_workspace_relative(&path)
                .then_some(path)
                .ok_or(CodecError::InvalidExecutionPath)
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths
        .windows(2)
        .all(|pair| pair[0] < pair[1])
        .then_some(paths)
        .ok_or(CodecError::InvalidExecutionPath)
}

fn normalised_workspace_relative(path: &Path) -> bool {
    if path.as_os_str().is_empty() {
        return false;
    }
    let mut normalised = PathBuf::new();
    for component in path.components() {
        let Component::Normal(component) = component else {
            return false;
        };
        normalised.push(component);
    }
    normalised.as_os_str() == path.as_os_str()
}

fn encode_observation(observation: &Observation) -> StoredObservation {
    StoredObservation {
        head: observation.head.clone(),
        tail: observation.tail.clone(),
        head_lines: observation.head_lines,
        tail_lines: observation.tail_lines,
        total_lines: observation.total_lines,
        total_bytes: observation.total_bytes,
        omitted_lines: observation.omitted_lines,
        omitted_bytes: observation.omitted_bytes,
        head_ends_mid_line: observation.head_ends_mid_line,
        tail_starts_mid_line: observation.tail_starts_mid_line,
        partial_lines: observation.partial_lines,
    }
}

fn decode_observation(observation: &StoredObservation) -> Observation {
    Observation {
        head: observation.head.clone(),
        tail: observation.tail.clone(),
        head_lines: observation.head_lines,
        tail_lines: observation.tail_lines,
        total_lines: observation.total_lines,
        total_bytes: observation.total_bytes,
        omitted_lines: observation.omitted_lines,
        omitted_bytes: observation.omitted_bytes,
        head_ends_mid_line: observation.head_ends_mid_line,
        tail_starts_mid_line: observation.tail_starts_mid_line,
        partial_lines: observation.partial_lines,
    }
}

fn encode_tool_call(call: &ToolCall) -> StoredToolCall {
    StoredToolCall {
        tool: match call.tool {
            ToolName::Read => StoredToolName::Read,
            ToolName::Write => StoredToolName::Write,
            ToolName::Edit => StoredToolName::Edit,
            ToolName::ListDir => StoredToolName::ListDir,
            ToolName::Glob => StoredToolName::Glob,
            ToolName::Grep => StoredToolName::Grep,
            ToolName::Run => StoredToolName::Run,
            ToolName::StartSubagent => StoredToolName::StartSubagent,
            ToolName::GetSubagent => StoredToolName::GetSubagent,
            ToolName::RefreshSubagent => StoredToolName::RefreshSubagent,
            ToolName::WaitSubagent => StoredToolName::WaitSubagent,
            ToolName::CancelSubagent => StoredToolName::CancelSubagent,
        },
        summary: match &call.summary {
            ToolCallSummary::Pending => StoredToolCallSummary::Pending,
            ToolCallSummary::Failed => StoredToolCallSummary::Failed,
            ToolCallSummary::Exit(code) => StoredToolCallSummary::Exit(*code),
            ToolCallSummary::Signal => StoredToolCallSummary::Signal,
            ToolCallSummary::Count(count) => StoredToolCallSummary::Count(*count),
            ToolCallSummary::Created => StoredToolCallSummary::Created,
            ToolCallSummary::Replaced => StoredToolCallSummary::Replaced,
            ToolCallSummary::Applied => StoredToolCallSummary::Applied,
            ToolCallSummary::Subagent(id) => StoredToolCallSummary::Subagent(id.clone()),
        },
    }
}

fn decode_tool_call(call: &StoredToolCall) -> Result<ToolCall, CodecError> {
    if !valid_tool_call_pair(&call.tool, &call.summary) {
        return Err(CodecError::InvalidPayload);
    }
    Ok(ToolCall {
        tool: match call.tool {
            StoredToolName::Read => ToolName::Read,
            StoredToolName::Write => ToolName::Write,
            StoredToolName::Edit => ToolName::Edit,
            StoredToolName::ListDir => ToolName::ListDir,
            StoredToolName::Glob => ToolName::Glob,
            StoredToolName::Grep => ToolName::Grep,
            StoredToolName::Run => ToolName::Run,
            StoredToolName::StartSubagent => ToolName::StartSubagent,
            StoredToolName::GetSubagent => ToolName::GetSubagent,
            StoredToolName::RefreshSubagent => ToolName::RefreshSubagent,
            StoredToolName::WaitSubagent => ToolName::WaitSubagent,
            StoredToolName::CancelSubagent => ToolName::CancelSubagent,
        },
        summary: decode_tool_call_summary(&call.summary),
    })
}

fn valid_tool_call_pair(tool: &StoredToolName, summary: &StoredToolCallSummary) -> bool {
    matches!(
        (tool, summary),
        (
            _,
            StoredToolCallSummary::Pending | StoredToolCallSummary::Failed
        ) | (
            StoredToolName::Run,
            StoredToolCallSummary::Exit(_) | StoredToolCallSummary::Signal
        ) | (
            StoredToolName::Read
                | StoredToolName::ListDir
                | StoredToolName::Glob
                | StoredToolName::Grep,
            StoredToolCallSummary::Count(_)
        ) | (
            StoredToolName::Write,
            StoredToolCallSummary::Created | StoredToolCallSummary::Replaced
        ) | (StoredToolName::Edit, StoredToolCallSummary::Applied)
            | (
                StoredToolName::StartSubagent
                    | StoredToolName::GetSubagent
                    | StoredToolName::RefreshSubagent
                    | StoredToolName::WaitSubagent
                    | StoredToolName::CancelSubagent,
                StoredToolCallSummary::Subagent(_)
            )
    )
}

fn decode_tool_call_summary(summary: &StoredToolCallSummary) -> ToolCallSummary {
    match summary {
        StoredToolCallSummary::Pending => ToolCallSummary::Pending,
        StoredToolCallSummary::Failed => ToolCallSummary::Failed,
        StoredToolCallSummary::Exit(code) => ToolCallSummary::Exit(*code),
        StoredToolCallSummary::Signal => ToolCallSummary::Signal,
        StoredToolCallSummary::Count(count) => ToolCallSummary::Count(*count),
        StoredToolCallSummary::Created => ToolCallSummary::Created,
        StoredToolCallSummary::Replaced => ToolCallSummary::Replaced,
        StoredToolCallSummary::Applied => ToolCallSummary::Applied,
        StoredToolCallSummary::Subagent(id) => ToolCallSummary::Subagent(id.clone()),
    }
}

fn encode_terminal_value(terminal: &PersistedTerminal) -> Result<StoredTerminal, CodecError> {
    Ok(match terminal {
        PersistedTerminal::Final {
            text,
            reasoning_summary,
            attempts,
            ..
        } => StoredTerminal::Final {
            text: text.clone(),
            reasoning_summary: reasoning_summary.clone(),
            attempts: attempts.iter().map(encode_attempt).collect(),
        },
        PersistedTerminal::Cancelled { evidence, attempts } => StoredTerminal::Cancelled {
            evidence: evidence.as_ref().map(encode_evidence).transpose()?,
            attempts: attempts.iter().map(encode_attempt).collect(),
        },
        PersistedTerminal::Failed {
            message,
            evidence,
            attempts,
        } => StoredTerminal::Failed {
            message: message.clone(),
            evidence: evidence.as_ref().map(encode_evidence).transpose()?,
            attempts: attempts.iter().map(encode_attempt).collect(),
        },
    })
}

fn encode_evidence(evidence: &TerminalEvidence) -> Result<StoredTerminalEvidence, CodecError> {
    match evidence {
        TerminalEvidence::Generation(evidence) => Ok(StoredTerminalEvidence::Generation(
            StoredGenerationEvidence {
                reasoning_summary: evidence.reasoning_summary.clone(),
                reasoning: evidence.reasoning.clone(),
                assistant_text: evidence.assistant_text.clone(),
                decoded_python: evidence.decoded_python.clone(),
            },
        )),
        TerminalEvidence::Execution(evidence) => Ok(StoredTerminalEvidence::Execution(Box::new(
            StoredExecutionEvidence {
                source: evidence.source.clone(),
                commentary: evidence.commentary.clone(),
                reasoning_summary: evidence.reasoning_summary.clone(),
                began: evidence.began,
                stdout: encode_observation(&evidence.stdout),
                stderr: encode_observation(&evidence.stderr),
                tool_calls: evidence.tool_calls.iter().map(encode_tool_call).collect(),
                read_paths: encode_effect_paths(&evidence.read_paths)?,
                modified_paths: encode_effect_paths(&evidence.modified_paths)?,
                cancellation_warning: evidence.cancellation_warning.clone(),
                failure: evidence.failure.as_ref().map(|failure| match failure {
                    ExecutionFailure::Arena { message } => StoredExecutionFailure::Arena {
                        message: message.clone(),
                    },
                    ExecutionFailure::RecoveryFailed { original, recovery } => {
                        StoredExecutionFailure::RecoveryFailed {
                            original: original.clone(),
                            recovery: recovery.clone(),
                        }
                    }
                }),
            },
        ))),
    }
}

fn decode_evidence(evidence: &StoredTerminalEvidence) -> Result<TerminalEvidence, CodecError> {
    Ok(match evidence {
        StoredTerminalEvidence::Generation(evidence) => {
            TerminalEvidence::Generation(GenerationEvidence {
                reasoning_summary: evidence.reasoning_summary.clone(),
                reasoning: evidence.reasoning.clone(),
                assistant_text: evidence.assistant_text.clone(),
                decoded_python: evidence.decoded_python.clone(),
            })
        }
        StoredTerminalEvidence::Execution(evidence) => {
            TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                source: evidence.source.clone(),
                commentary: evidence.commentary.clone(),
                reasoning_summary: evidence.reasoning_summary.clone(),
                began: evidence.began,
                stdout: decode_observation(&evidence.stdout),
                stderr: decode_observation(&evidence.stderr),
                tool_calls: evidence
                    .tool_calls
                    .iter()
                    .map(decode_tool_call)
                    .collect::<Result<Vec<_>, _>>()?,
                read_paths: decode_effect_paths(&evidence.read_paths)?,
                modified_paths: decode_effect_paths(&evidence.modified_paths)?,
                cancellation_warning: evidence.cancellation_warning.clone(),
                failure: evidence.failure.as_ref().map(|failure| match failure {
                    StoredExecutionFailure::Arena { message } => ExecutionFailure::Arena {
                        message: message.clone(),
                    },
                    StoredExecutionFailure::RecoveryFailed { original, recovery } => {
                        ExecutionFailure::RecoveryFailed {
                            original: original.clone(),
                            recovery: recovery.clone(),
                        }
                    }
                }),
            }))
        }
    })
}

fn encode_attempt(accounting: &AttemptAccounting) -> StoredAttemptAccounting {
    match accounting {
        AttemptAccounting::KnownUsage(usage) => {
            StoredAttemptAccounting::KnownUsage(encode_usage(usage))
        }
        AttemptAccounting::KnownUsageWithUntrackedRetries(usage) => {
            StoredAttemptAccounting::KnownUsageWithUntrackedRetries(encode_usage(usage))
        }
        AttemptAccounting::UsageUnavailable => StoredAttemptAccounting::UsageUnavailable,
    }
}

fn decode_attempt(accounting: &StoredAttemptAccounting) -> AttemptAccounting {
    match accounting {
        StoredAttemptAccounting::KnownUsage(usage) => {
            AttemptAccounting::KnownUsage(decode_usage(usage))
        }
        StoredAttemptAccounting::KnownUsageWithUntrackedRetries(usage) => {
            AttemptAccounting::KnownUsageWithUntrackedRetries(decode_usage(usage))
        }
        StoredAttemptAccounting::UsageUnavailable => AttemptAccounting::UsageUnavailable,
    }
}

fn encode_usage(usage: &ModelUsage) -> StoredModelUsage {
    StoredModelUsage {
        model: usage.model.clone(),
        service_tier: usage.service_tier.clone(),
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        cache_write_1h_input_tokens: usage.cache_write_1h_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn decode_usage(usage: &StoredModelUsage) -> ModelUsage {
    ModelUsage {
        model: usage.model.clone(),
        service_tier: usage.service_tier.clone(),
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        cache_write_1h_input_tokens: usage.cache_write_1h_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.total_tokens,
    }
}

fn encode_replay(replay: &ProviderReplay) -> Result<Vec<u8>, CodecError> {
    Ok(serde_json::to_vec(&StoredReplay {
        provider: replay.provider().to_owned(),
        items: replay
            .items()
            .iter()
            .map(|item| encode_hex(item.as_bytes()))
            .collect(),
    })?)
}

fn decode_replay(replay: Option<&[u8]>) -> Result<ProviderReplay, CodecError> {
    let replay = replay.ok_or(CodecError::MissingReplay)?;
    let replay: StoredReplay = serde_json::from_slice(replay)?;
    Ok(ProviderReplay::new(
        replay.provider,
        replay
            .items
            .iter()
            .map(|item| decode_hex(item).map(OpaqueReplayItem::new))
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn encode_tool_value(value: &ToolValue) -> StoredToolValue {
    match value {
        ToolValue::None => StoredToolValue::None,
        ToolValue::Bool(value) => StoredToolValue::Bool(*value),
        ToolValue::Integer(value) => StoredToolValue::Integer(*value),
        ToolValue::Float(value) => StoredToolValue::FloatBits(value.to_bits()),
        ToolValue::String(value) => StoredToolValue::String(value.clone()),
        ToolValue::List(values) => {
            StoredToolValue::List(values.iter().map(encode_tool_value).collect())
        }
        ToolValue::Dictionary(values) => StoredToolValue::Dictionary(
            values
                .iter()
                .map(|(key, value)| (key.clone(), encode_tool_value(value)))
                .collect(),
        ),
    }
}

fn decode_task_outcome(outcome: StoredTaskOutcome) -> TaskOutcome {
    match outcome {
        StoredTaskOutcome::Completed { message, values } => {
            TaskOutcome::Completed(crate::subagents::ChildResult::new(
                message,
                values
                    .into_iter()
                    .map(|(key, value)| (key, decode_tool_value(value)))
                    .collect(),
            ))
        }
        StoredTaskOutcome::Failed { message } => {
            TaskOutcome::Failed(crate::subagents::ChildFailure::new(message))
        }
        StoredTaskOutcome::Cancelled => TaskOutcome::Cancelled,
        StoredTaskOutcome::TimedOut => TaskOutcome::TimedOut,
    }
}

fn decode_tool_value(value: StoredToolValue) -> ToolValue {
    match value {
        StoredToolValue::None => ToolValue::None,
        StoredToolValue::Bool(value) => ToolValue::Bool(value),
        StoredToolValue::Integer(value) => ToolValue::Integer(value),
        StoredToolValue::FloatBits(bits) => ToolValue::Float(f64::from_bits(bits)),
        StoredToolValue::String(value) => ToolValue::String(value),
        StoredToolValue::List(values) => {
            ToolValue::List(values.into_iter().map(decode_tool_value).collect())
        }
        StoredToolValue::Dictionary(values) => ToolValue::Dictionary(
            values
                .into_iter()
                .map(|(key, value)| (key, decode_tool_value(value)))
                .collect(),
        ),
    }
}

pub(super) fn encode_path(path: &Path) -> Result<String, CodecError> {
    #[cfg(unix)]
    {
        Ok(encode_hex(path.as_os_str().as_bytes()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(CodecError::UnsupportedPath)
    }
}

pub(super) fn decode_path(value: &str) -> Result<PathBuf, CodecError> {
    #[cfg(unix)]
    {
        Ok(PathBuf::from(std::ffi::OsString::from_vec(decode_hex(
            value,
        )?)))
    }
    #[cfg(not(unix))]
    {
        let _ = value;
        Err(CodecError::UnsupportedPath)
    }
}

fn decode_fingerprint(value: &str) -> Result<ContentFingerprint, CodecError> {
    let bytes = decode_hex(value).map_err(|_| CodecError::InvalidFingerprint)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| CodecError::InvalidFingerprint)?;
    Ok(ContentFingerprint::from_session_bytes(bytes))
}

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)] as char);
        encoded.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn decode_hex(value: &str) -> Result<Vec<u8>, CodecError> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(CodecError::InvalidPath);
    }
    pairs
        .iter()
        .map(|[high, low]| Ok((hex_value(*high)? << 4) | hex_value(*low)?))
        .collect()
}

fn hex_value(value: u8) -> Result<u8, CodecError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(CodecError::InvalidPath),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use super::{
        DecodedTerminalHistory, decode_checkpoint, decode_entry, decode_execution_value,
        decode_path, decode_request, decode_session_preferences, decode_task_details,
        decode_task_record_details, decode_tool_value, decoded_attempts, decoded_compaction,
        decoded_execution_history, decoded_readable_execution, decoded_readable_terminal,
        decoded_request, decoded_request_selection, decoded_terminal_history, encode_checkpoint,
        encode_compaction, encode_execution, encode_execution_value, encode_path, encode_request,
        encode_session_preferences, encode_task_details, encode_terminal, encode_tool_value,
        fail_unfinished_task_details,
    };
    use crate::arena::{Checkpoint, Execution, ExecutionOutcome, Observation, RuntimeIdentity};
    use crate::config::{
        ModelSelection, SessionPreferences,
        models::{SOL, TERRA},
    };
    use crate::model::{AttemptAccounting, ModelUsage};
    use crate::pricing::{RunCostEstimate, estimate_run_cost, estimate_usage};
    use crate::session::{CompactionOutcome, CompactionReason, SessionEntryId};
    use crate::subagents::TaskPersistedDetails;
    use crate::tools::{ContentFingerprint, ToolValue, WholeFileAuthorisations};

    #[test]
    fn compaction_codec_round_trips_attempts_and_rejects_invalid_payloads() {
        let usage = ModelUsage {
            model: "openai/gpt-5.6-sol".to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(1),
            cached_input_tokens: Some(2),
            cache_write_input_tokens: Some(3),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(4),
            reasoning_output_tokens: Some(5),
            total_tokens: Some(6),
        };
        let attempts = vec![
            AttemptAccounting::KnownUsage(usage.clone()),
            AttemptAccounting::KnownUsageWithUntrackedRetries(usage),
            AttemptAccounting::UsageUnavailable,
        ];
        let outcomes = [
            CompactionOutcome::Applied {
                summary: "summary".to_owned(),
                first_retained: SessionEntryId(42),
                context_tokens: 1_500,
                reason: CompactionReason::Manual,
                attempts: attempts.clone(),
            },
            CompactionOutcome::Failed {
                diagnostic: "diagnostic".to_owned(),
                reason: CompactionReason::AutomaticThreshold,
                attempts: attempts.clone(),
            },
            CompactionOutcome::Cancelled {
                reason: CompactionReason::ContextOverflow,
                attempts,
            },
        ];

        for outcome in outcomes {
            let stored = encode_compaction(&outcome).expect("encode compaction");
            let decoded =
                decode_entry(stored.kind, &stored.payload_json).expect("decode compaction outcome");
            assert_eq!(decoded_compaction(&decoded), Some(outcome));
            assert!(decode_entry("execution", &stored.payload_json).is_err());
        }

        for diagnostic in ["x".repeat(4_096), "é".repeat(2_048)] {
            let outcome = CompactionOutcome::Failed {
                diagnostic: diagnostic.clone(),
                reason: CompactionReason::Manual,
                attempts: Vec::new(),
            };
            let stored = encode_compaction(&outcome).expect("encode boundary diagnostic");
            let decoded = decode_entry(stored.kind, &stored.payload_json)
                .expect("decode boundary diagnostic");
            assert_eq!(decoded_compaction(&decoded), Some(outcome));
        }

        let applied = encode_compaction(&CompactionOutcome::Applied {
            summary: "summary".to_owned(),
            first_retained: SessionEntryId(42),
            context_tokens: 1,
            reason: CompactionReason::Manual,
            attempts: Vec::new(),
        })
        .expect("encode applied compaction");
        let mut invalid: serde_json::Value =
            serde_json::from_str(&applied.payload_json).expect("decode applied JSON");
        invalid["Compaction"]["compaction"]["first_retained"] = serde_json::json!(0);
        assert!(decode_entry("compaction", &invalid.to_string()).is_err());
        invalid["Compaction"]["compaction"]["first_retained"] = serde_json::json!(42);
        invalid["Compaction"]["compaction"]["reason"] = serde_json::json!("unknown");
        assert!(decode_entry("compaction", &invalid.to_string()).is_err());
        invalid["Compaction"]["compaction"]["reason"] = serde_json::json!("manual");
        invalid["Compaction"]["compaction"]["diagnostic"] = serde_json::json!("forbidden");
        assert!(decode_entry("compaction", &invalid.to_string()).is_err());

        let failed = encode_compaction(&CompactionOutcome::Failed {
            diagnostic: "diagnostic".to_owned(),
            reason: CompactionReason::Manual,
            attempts: Vec::new(),
        })
        .expect("encode failed compaction");
        for diagnostic in ["x".repeat(4_097), "é".repeat(2_049)] {
            let mut invalid: serde_json::Value =
                serde_json::from_str(&failed.payload_json).expect("decode failed JSON");
            invalid["Compaction"]["compaction"]["diagnostic"] = serde_json::json!(diagnostic);
            assert!(decode_entry("compaction", &invalid.to_string()).is_err());
        }
        assert!(
            encode_compaction(&CompactionOutcome::Failed {
                diagnostic: "x".repeat(4_097),
                reason: CompactionReason::Manual,
                attempts: Vec::new(),
            })
            .is_err()
        );
    }

    #[test]
    fn stored_usage_round_trips_1h_cache_writes_and_decodes_legacy_usage_as_zero() {
        let usage = ModelUsage {
            model: "openai/gpt-5.6-sol".to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(200_000),
            cached_input_tokens: Some(40_000),
            cache_write_input_tokens: Some(60_000),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(100_000),
            reasoning_output_tokens: Some(50_000),
            total_tokens: Some(300_000),
        };
        let outcome = |usage: &ModelUsage| CompactionOutcome::Cancelled {
            reason: CompactionReason::Manual,
            attempts: vec![AttemptAccounting::KnownUsage(usage.clone())],
        };

        let split = ModelUsage {
            cache_write_1h_input_tokens: 10_000,
            ..usage.clone()
        };
        let stored = encode_compaction(&outcome(&split)).expect("encode 1h usage");
        let decoded = decode_entry(stored.kind, &stored.payload_json).expect("decode 1h usage");
        assert_eq!(decoded_compaction(&decoded), Some(outcome(&split)));

        let stored = encode_compaction(&outcome(&usage)).expect("encode usage");
        let mut legacy: serde_json::Value =
            serde_json::from_str(&stored.payload_json).expect("decode usage JSON");
        let removed = legacy["Compaction"]["compaction"]["attempts"][0]["KnownUsage"]
            .as_object_mut()
            .expect("stored usage object")
            .remove("cache_write_1h_input_tokens");
        assert_eq!(removed, Some(serde_json::json!(0)));
        let decoded = decode_entry("compaction", &legacy.to_string()).expect("decode legacy usage");
        assert_eq!(decoded_compaction(&decoded), Some(outcome(&usage)));
        assert_eq!(
            estimate_run_cost(&decoded_attempts(&decoded)),
            RunCostEstimate::Complete(estimate_usage(&usage).unwrap().amount())
        );
        assert_eq!(
            estimate_usage(&usage).unwrap().amount().as_nanos(),
            2_716_000_000
        );
        assert_eq!(crate::session::FORMAT_VERSION, 2);
    }

    #[test]
    fn session_preferences_codec_round_trips_and_rejects_invalid_metadata() {
        let selection = ModelSelection::new(&TERRA, "high").expect("select Terra");
        let preferences = SessionPreferences::new(
            selection,
            BTreeMap::from([(SOL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid preferences");

        for disabled in [false, true] {
            let encoded =
                encode_session_preferences(&preferences, disabled).expect("encode preferences");
            assert_eq!(
                decode_session_preferences(&encoded).expect("decode preferences"),
                (preferences.clone(), disabled)
            );
        }
        let older = r#"{"selected_model":"openai/gpt-5.6-terra","thinking":{"openai/gpt-5.6-terra":"high"}}"#;
        assert!(!decode_session_preferences(older).unwrap().1);

        for invalid in [
            r#"{"selected_model":"openai/gpt-5.6-terra","thinking":{"openai/gpt-5.6-terra":"high"},"type_check_disabled":"off"}"#,
            r#"{"selected_model":"openai/gpt-5.6-terra","thinking":{"openai/gpt-5.6-terra":"high"},"extra":true}"#,
            r#"{"selected_model":"openai/gpt-5.6-terra","thinking":{"openai/gpt-5.6-terra":"unsupported"}}"#,
            r#"{"selected_model":"openai/gpt-5.6-terra","thinking":{}}"#,
        ] {
            assert!(decode_session_preferences(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn checkpoint_codec_preserves_non_utf8_authorisations() {
        let path = PathBuf::from(OsString::from_vec(b"nested/\xff-file".to_vec()));
        let fingerprint = ContentFingerprint::from_session_bytes([42; 32]);
        let mut authorisations = WholeFileAuthorisations::default();
        authorisations.authorise(path.clone(), fingerprint);
        let checkpoint = Checkpoint::from_session_parts(
            RuntimeIdentity::from_session_value("monty-runtime 0.0.22".to_owned()),
            vec![0, 1, 2, 255],
            authorisations,
        );

        let encoded = encode_checkpoint(&checkpoint).expect("encode checkpoint");
        let decoded = decode_checkpoint(
            encoded.runtime_identity,
            encoded.arena_state,
            &encoded.authorisations_json,
        )
        .expect("decode checkpoint");

        assert_eq!(decoded, checkpoint);
        assert_eq!(decode_path(&encode_path(&path).unwrap()).unwrap(), path);
    }

    #[test]
    fn execution_codec_preserves_observations_and_errors() {
        let observation = |head: &str| Observation {
            head: head.to_owned(),
            tail: Some("tail".to_owned()),
            head_lines: 1,
            tail_lines: 1,
            total_lines: 3,
            total_bytes: 12,
            omitted_lines: 1,
            omitted_bytes: 4,
            head_ends_mid_line: true,
            tail_starts_mid_line: true,
            partial_lines: true,
        };
        let mut execution = Execution {
            stdout: observation("stdout"),
            stderr: observation("stderr"),
            outcome: ExecutionOutcome::Complete { result: None },
            tool_calls: Vec::new(),
            read_paths: Vec::new(),
            modified_paths: Vec::new(),
        };

        for outcome in [
            ExecutionOutcome::PythonError(crate::arena::PythonError {
                text: "ValueError".to_owned(),
                omitted_bytes: 9,
            }),
            ExecutionOutcome::TypeCheckError(crate::arena::PythonError {
                text: "invalid-assignment".to_owned(),
                omitted_bytes: 9,
            }),
        ] {
            execution.outcome = outcome;
            assert_eq!(
                decode_execution_value(
                    &encode_execution_value(&execution).expect("encode execution")
                )
                .expect("decode execution"),
                execution
            );
        }
    }

    #[test]
    fn execution_file_path_codec_sorts_deduplicates_and_rejects_malformed_paths() {
        let mut execution = task_execution();
        execution.read_paths = vec![
            PathBuf::from("nested/z"),
            PathBuf::from("a-b"),
            PathBuf::from("a/b"),
            PathBuf::from("nested/a"),
            PathBuf::from("nested/z"),
        ];
        execution.modified_paths = vec![PathBuf::from("nested/b")];
        let stored = encode_execution_value(&execution).expect("encode execution");
        let decoded = decode_execution_value(&stored).expect("decode execution");
        assert_eq!(
            decoded.read_paths,
            [
                PathBuf::from("a/b"),
                PathBuf::from("a-b"),
                PathBuf::from("nested/a"),
                PathBuf::from("nested/z"),
            ]
        );
        assert_eq!(decoded.modified_paths, [PathBuf::from("nested/b")]);

        for paths in [
            serde_json::json!(["2e2f6e65737465642f61"]),
            serde_json::json!(["6e65737465642f61", "6e65737465642f61"]),
            serde_json::json!(["612d62", "612f62"]),
            serde_json::json!(["not-hex"]),
        ] {
            let mut value = serde_json::to_value(&stored).expect("serialise execution");
            value["read_paths"] = paths;
            let stored: super::StoredExecution =
                serde_json::from_value(value).expect("decode stored execution");
            assert!(decode_execution_value(&stored).is_err());
        }

        let mut value = serde_json::to_value(&stored).expect("serialise execution");
        value
            .as_object_mut()
            .expect("stored execution object")
            .remove("modified_paths");
        assert!(serde_json::from_value::<super::StoredExecution>(value).is_err());
    }

    #[test]
    fn codec_rejects_invalid_tool_pairs_in_all_stored_execution_evidence() {
        let mut execution = task_execution();
        execution.tool_calls.push(crate::arena::ToolCall {
            tool: crate::arena::ToolName::StartSubagent,
            summary: crate::arena::ToolCallSummary::Subagent("00".to_owned()),
        });
        let code = crate::session::PersistedCode {
            source: "value = 1".to_owned(),
            call_id: crate::model::ModelCallId::new("call-id"),
            commentary: None,
            reasoning_summary: String::new(),
            replay: crate::model::ProviderReplay::new("provider", Vec::new()),
            result: crate::model::CodeResult::Execution {
                feedback: String::new(),
            },
            execution: Some(execution.clone()),
            attempts: Vec::new(),
        };
        let stored = encode_execution(&code).expect("encode foreground execution");
        let mut foreground: serde_json::Value =
            serde_json::from_str(&stored.payload_json).expect("decode foreground execution");
        foreground["Execution"]["execution"]["tool_calls"][0]["tool"] =
            serde_json::Value::String("Read".to_owned());
        let decoded =
            decode_entry(stored.kind, &foreground.to_string()).expect("decode foreground envelope");
        assert!(matches!(
            decoded_readable_execution(&decoded),
            Err(super::CodecError::InvalidPayload)
        ));

        let terminal = crate::session::PersistedTerminal::Cancelled {
            evidence: Some(crate::agent::TerminalEvidence::Execution(Box::new(
                crate::agent::ExecutionEvidence {
                    source: "value = 1".to_owned(),
                    commentary: None,
                    reasoning_summary: String::new(),
                    began: true,
                    stdout: execution.stdout.clone(),
                    stderr: execution.stderr.clone(),
                    tool_calls: execution.tool_calls.clone(),
                    read_paths: execution.read_paths.clone(),
                    modified_paths: execution.modified_paths.clone(),
                    cancellation_warning: None,
                    failure: None,
                },
            ))),
            attempts: Vec::new(),
        };
        let stored = encode_terminal(&terminal, None).expect("encode terminal evidence");
        let mut terminal: serde_json::Value =
            serde_json::from_str(&stored.payload_json).expect("decode terminal evidence");
        terminal["Terminal"]["terminal"]["Cancelled"]["evidence"]["Execution"]["tool_calls"][0]["tool"] =
            serde_json::Value::String("Read".to_owned());
        let decoded =
            decode_entry(stored.kind, &terminal.to_string()).expect("decode terminal envelope");
        assert!(matches!(
            decoded_readable_terminal(&decoded),
            Err(super::CodecError::InvalidPayload)
        ));

        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .expect("default selection");
        let record =
            crate::subagents::TaskRecord::for_test("0a", 1, selection, None, 1, Vec::new())
                .with_persisted_details_for_test(TaskPersistedDetails {
                    admitted_at_ms: 1,
                    terminal_at_ms: None,
                    state: crate::subagents::TaskState::Running,
                    step_count: 1,
                    elapsed: std::time::Duration::ZERO,
                    observed_at_ms: 1,
                    attempt_evidence: Vec::new(),
                    executions: vec![crate::subagents::TaskExecution::for_test(
                        1,
                        "value = 1".to_owned(),
                        String::new(),
                        execution,
                        Vec::new(),
                    )],
                    outcome: None,
                });
        let mut child: serde_json::Value = serde_json::from_str(
            &encode_task_details(&record).expect("encode child task evidence"),
        )
        .expect("decode child task evidence");
        child["executions"][0]["execution"]["tool_calls"][0]["tool"] =
            serde_json::Value::String("Read".to_owned());
        assert!(matches!(
            decode_task_record_details(&child.to_string()),
            Err(super::CodecError::InvalidPayload)
        ));
    }

    #[test]
    fn foreground_codec_preserves_code_replay_and_terminal_evidence() {
        let execution = Execution {
            stdout: Observation {
                head: "stdout".to_owned(),
                tail: Some("tail".to_owned()),
                head_lines: 1,
                tail_lines: 1,
                total_lines: 3,
                total_bytes: 12,
                omitted_lines: 1,
                omitted_bytes: 4,
                head_ends_mid_line: true,
                tail_starts_mid_line: true,
                partial_lines: true,
            },
            stderr: Observation {
                head: "stderr".to_owned(),
                tail: None,
                head_lines: 1,
                tail_lines: 0,
                total_lines: 1,
                total_bytes: 6,
                omitted_lines: 0,
                omitted_bytes: 0,
                head_ends_mid_line: false,
                tail_starts_mid_line: false,
                partial_lines: false,
            },
            outcome: ExecutionOutcome::Complete { result: None },
            tool_calls: vec![crate::arena::ToolCall {
                tool: crate::arena::ToolName::Run,
                summary: crate::arena::ToolCallSummary::Exit(7),
            }],
            read_paths: vec![PathBuf::from("nested/read")],
            modified_paths: vec![PathBuf::from("nested/modified")],
        };
        let attempts = vec![
            crate::model::AttemptAccounting::KnownUsageWithUntrackedRetries(
                crate::model::ModelUsage {
                    model: "openai/gpt-5.6-sol".to_owned(),
                    service_tier: Some("tier".to_owned()),
                    input_tokens: Some(1),
                    cached_input_tokens: None,
                    cache_write_input_tokens: Some(3),
                    cache_write_1h_input_tokens: 0,
                    output_tokens: Some(4),
                    reasoning_output_tokens: None,
                    total_tokens: Some(8),
                },
            ),
        ];
        let code = crate::session::PersistedCode {
            source: "value = 1".to_owned(),
            call_id: crate::model::ModelCallId::new("call-id"),
            commentary: Some("commentary".to_owned()),
            reasoning_summary: "reasoning".to_owned(),
            replay: crate::model::ProviderReplay::new(
                "provider",
                vec![crate::model::OpaqueReplayItem::new(vec![0, 255, 1])],
            ),
            result: crate::model::CodeResult::Execution {
                feedback: "feedback".to_owned(),
            },
            execution: Some(execution.clone()),
            attempts: attempts.clone(),
        };
        let missing_execution = crate::session::PersistedCode {
            execution: None,
            ..code.clone()
        };
        assert_eq!(
            encode_execution(&missing_execution)
                .err()
                .expect("missing execution should be rejected")
                .to_string(),
            "stored session execution is missing"
        );
        let invalid_payload = encode_request(
            "unexpected payload",
            ModelSelection::new(&SOL, "low").unwrap(),
        )
        .unwrap();
        assert_eq!(
            decode_entry("execution", &invalid_payload)
                .err()
                .expect("execution payload must be rejected")
                .to_string(),
            "stored session payload has unexpected kind"
        );
        assert_eq!(
            decode_entry("cancelled", &invalid_payload)
                .err()
                .expect("terminal payload must be rejected")
                .to_string(),
            "stored session payload has unexpected kind"
        );

        let stored = encode_execution(&code).unwrap();
        let decoded = decode_entry(stored.kind, &stored.payload_json).unwrap();
        assert_eq!(
            decoded_execution_history(&decoded, stored.provider_replay.as_deref()).unwrap(),
            crate::session::PersistedCode {
                execution: None,
                ..code.clone()
            }
        );
        assert_eq!(
            decoded_readable_execution(&decoded).expect("decode readable execution"),
            Some((
                crate::session::ReadableCode {
                    source: code.source.clone(),
                    commentary: code.commentary.clone(),
                    reasoning_summary: code.reasoning_summary.clone(),
                    result: code.result.clone(),
                    attempts: code.attempts.clone(),
                },
                execution.clone(),
            ))
        );

        let terminal = crate::session::PersistedTerminal::Cancelled {
            evidence: Some(crate::agent::TerminalEvidence::Execution(Box::new(
                crate::agent::ExecutionEvidence {
                    source: "value = 1".to_owned(),
                    commentary: Some("commentary".to_owned()),
                    reasoning_summary: "reasoning".to_owned(),
                    began: true,
                    stdout: execution.stdout.clone(),
                    stderr: execution.stderr.clone(),
                    tool_calls: execution.tool_calls.clone(),
                    read_paths: execution.read_paths.clone(),
                    modified_paths: execution.modified_paths.clone(),
                    cancellation_warning: Some("cancelled".to_owned()),
                    failure: Some(crate::agent::ExecutionFailure::RecoveryFailed {
                        original: "original".to_owned(),
                        recovery: "recovery".to_owned(),
                    }),
                },
            ))),
            attempts,
        };
        let terminal_code = crate::session::PersistedCode {
            execution: None,
            ..code.clone()
        };
        let stored = encode_terminal(&terminal, Some(&terminal_code)).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&stored.payload_json).unwrap();
        assert_eq!(
            payload["Terminal"]["code"]["attempts"],
            serde_json::json!([])
        );
        let stored_terminal_code = crate::session::PersistedCode {
            attempts: Vec::new(),
            ..terminal_code.clone()
        };
        let decoded = decode_entry(stored.kind, &stored.payload_json).unwrap();
        assert!(matches!(
            decoded_terminal_history(&decoded, stored.provider_replay.as_deref()).unwrap(),
            DecodedTerminalHistory::Cancelled { code: Some(code) } if code == stored_terminal_code
        ));
        let crate::session::PersistedTerminal::Cancelled { evidence, attempts } = terminal else {
            unreachable!("test terminal is cancelled");
        };
        assert_eq!(
            decoded_readable_terminal(&decoded).expect("decode readable terminal"),
            Some(crate::session::ReadableTerminal::Cancelled { evidence, attempts })
        );

        let stored = encode_terminal(
            &crate::session::PersistedTerminal::Cancelled {
                evidence: None,
                attempts: Vec::new(),
            },
            None,
        )
        .unwrap();
        let decoded = decode_entry(stored.kind, &stored.payload_json).unwrap();
        assert!(matches!(
            decoded_terminal_history(&decoded, None).unwrap(),
            DecodedTerminalHistory::Cancelled { code: None }
        ));
        assert!(decoded_terminal_history(&decoded, Some(&[0])).is_err());
    }

    fn foreground_code() -> crate::session::PersistedCode {
        crate::session::PersistedCode {
            source: "value = 1".to_owned(),
            call_id: crate::model::ModelCallId::new("call-id"),
            commentary: None,
            reasoning_summary: String::new(),
            replay: crate::model::ProviderReplay::new("provider", Vec::new()),
            result: crate::model::CodeResult::Execution {
                feedback: String::new(),
            },
            execution: Some(task_execution()),
            attempts: Vec::new(),
        }
    }

    fn cancelled_terminal() -> crate::session::PersistedTerminal {
        crate::session::PersistedTerminal::Cancelled {
            evidence: None,
            attempts: Vec::new(),
        }
    }

    #[test]
    fn encoded_code_payloads_omit_selection() {
        let code = foreground_code();
        let execution: serde_json::Value =
            serde_json::from_str(&encode_execution(&code).unwrap().payload_json).unwrap();
        let terminal: serde_json::Value = serde_json::from_str(
            &encode_terminal(&cancelled_terminal(), Some(&code))
                .unwrap()
                .payload_json,
        )
        .unwrap();
        for stored_code in [
            &execution["Execution"]["code"],
            &terminal["Terminal"]["code"],
        ] {
            assert!(
                !stored_code
                    .as_object()
                    .expect("stored code object")
                    .contains_key("selection")
            );
        }
    }

    #[test]
    fn legacy_code_payloads_with_selection_still_decode() {
        let code = foreground_code();
        let expected = crate::session::PersistedCode {
            execution: None,
            ..code.clone()
        };
        let legacy_selection = serde_json::json!({
            "model_id": crate::config::DEFAULT_MODEL.id,
            "thinking": crate::config::DEFAULT_MODEL.default_thinking,
        });

        let stored = encode_execution(&code).unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(&stored.payload_json).unwrap();
        payload["Execution"]["code"]["selection"] = legacy_selection.clone();
        let decoded =
            decode_entry(stored.kind, &payload.to_string()).expect("decode legacy execution");
        assert_eq!(
            decoded_execution_history(&decoded, stored.provider_replay.as_deref()).unwrap(),
            expected
        );

        let stored = encode_terminal(&cancelled_terminal(), Some(&code)).unwrap();
        let mut payload: serde_json::Value = serde_json::from_str(&stored.payload_json).unwrap();
        payload["Terminal"]["code"]["selection"] = legacy_selection;
        let decoded =
            decode_entry(stored.kind, &payload.to_string()).expect("decode legacy terminal");
        assert!(matches!(
            decoded_terminal_history(&decoded, stored.provider_replay.as_deref()).unwrap(),
            DecodedTerminalHistory::Cancelled { code: Some(code) } if code == expected
        ));
    }

    #[test]
    fn user_request_payloads_decode_their_stored_selection() {
        let payload = serde_json::json!({
            "UserRequest": {
                "request": "hi",
                "selection": {
                    "model_id": crate::config::DEFAULT_MODEL.id,
                    "thinking": crate::config::DEFAULT_MODEL.default_thinking,
                },
            },
        });

        let decoded = decode_entry("user_request", &payload.to_string())
            .expect("decode request with selection");

        assert_eq!(decoded_request(&decoded), Some("hi"));
        assert_eq!(
            decoded_request_selection(&decoded),
            Some(
                ModelSelection::new(
                    crate::config::DEFAULT_MODEL,
                    crate::config::DEFAULT_MODEL.default_thinking
                )
                .unwrap()
            )
        );
    }

    #[test]
    fn user_request_payloads_without_a_known_selection_decode_with_none() {
        let legacy = serde_json::json!({ "UserRequest": { "request": "hi" } });
        let removed_model = serde_json::json!({
            "UserRequest": {
                "request": "hi",
                "selection": { "model_id": "openai/removed-model", "thinking": "high" },
            },
        });

        for payload in [legacy, removed_model] {
            let decoded =
                decode_entry("user_request", &payload.to_string()).expect("decode request");
            assert_eq!(decoded_request(&decoded), Some("hi"));
            assert_eq!(decoded_request_selection(&decoded), None);
        }
    }

    #[test]
    fn encoded_requests_round_trip_their_selection() {
        let selection = ModelSelection::new(&TERRA, "high").unwrap();

        let payload = encode_request("hi", selection).expect("encode request");
        let decoded = decode_entry("user_request", &payload).expect("decode request");

        assert_eq!(decoded_request_selection(&decoded), Some(selection));
        assert_eq!(decode_request(&payload).expect("decode request text"), "hi");
    }

    #[test]
    fn task_details_codec_preserves_parameters_and_timing() {
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .expect("default selection");
        let record =
            crate::subagents::TaskRecord::for_test("0a", 4, selection, Some("01"), 2, Vec::new())
                .with_observation_for_test(
                    std::time::Duration::new(1, 234_567_890),
                    1_789_578_356_296,
                );

        let details = decode_task_details(&encode_task_details(&record).expect("encode task"))
            .expect("decode task");

        assert_eq!(details.prompt, "test child");
        assert_eq!(details.selection.model_id, crate::config::DEFAULT_MODEL.id);
        assert_eq!(
            details.selection.thinking,
            crate::config::DEFAULT_MODEL.default_thinking
        );
        assert!(!details.read_only);
        assert_eq!(details.depth, 2);
        assert_eq!(details.admitted_at_ms, 0);
        assert_eq!(details.terminal_at_ms, None);
        assert_eq!(details.observation.elapsed_seconds, 1);
        assert_eq!(details.observation.elapsed_subsec_nanos, 234_567_890);
        assert_eq!(details.observation.observed_at_ms, 1_789_578_356_296);
        assert!(details.outcome.is_none());
    }

    fn task_usage() -> crate::model::AttemptAccounting {
        crate::model::AttemptAccounting::KnownUsage(crate::model::ModelUsage {
            model: crate::config::DEFAULT_MODEL.id.to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(1),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(0),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(1),
        })
    }

    fn task_execution() -> Execution {
        Execution {
            stdout: Observation {
                head: "stdout".to_owned(),
                tail: None,
                head_lines: 1,
                tail_lines: 0,
                total_lines: 1,
                total_bytes: 6,
                omitted_lines: 0,
                omitted_bytes: 0,
                head_ends_mid_line: false,
                tail_starts_mid_line: false,
                partial_lines: false,
            },
            stderr: Observation {
                head: String::new(),
                tail: None,
                head_lines: 0,
                tail_lines: 0,
                total_lines: 0,
                total_bytes: 0,
                omitted_lines: 0,
                omitted_bytes: 0,
                head_ends_mid_line: false,
                tail_starts_mid_line: false,
                partial_lines: false,
            },
            outcome: ExecutionOutcome::Complete { result: None },
            tool_calls: Vec::new(),
            read_paths: vec![PathBuf::from("nested/read")],
            modified_paths: vec![PathBuf::from("nested/modified")],
        }
    }

    #[test]
    fn task_details_codec_round_trips_completed_history_and_values() {
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .expect("default selection");
        let values = std::collections::BTreeMap::from([
            ("none".to_owned(), ToolValue::None),
            ("bool".to_owned(), ToolValue::Bool(true)),
            ("string".to_owned(), ToolValue::String("value".to_owned())),
            ("integer".to_owned(), ToolValue::Integer(-7)),
            ("nan".to_owned(), ToolValue::Float(f64::NAN)),
            (
                "positive_infinity".to_owned(),
                ToolValue::Float(f64::INFINITY),
            ),
            (
                "negative_infinity".to_owned(),
                ToolValue::Float(f64::NEG_INFINITY),
            ),
            ("negative_zero".to_owned(), ToolValue::Float(-0.0)),
            (
                "list".to_owned(),
                ToolValue::List(vec![ToolValue::None, ToolValue::Bool(false)]),
            ),
            (
                "dictionary".to_owned(),
                ToolValue::Dictionary(std::collections::BTreeMap::from([(
                    "nested".to_owned(),
                    ToolValue::String("value".to_owned()),
                )])),
            ),
        ]);
        let record = crate::subagents::TaskRecord::for_test(
            "0a",
            9,
            selection,
            Some("01"),
            2,
            vec![task_usage()],
        )
        .with_parameters_for_test("investigate".to_owned(), selection, true)
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 100,
            terminal_at_ms: Some(200),
            state: crate::subagents::TaskState::Completed,
            step_count: 3,
            elapsed: std::time::Duration::new(4, 5),
            observed_at_ms: 200,
            attempt_evidence: vec![
                crate::subagents::TaskAttempt::for_test(1, 1, Some(task_usage())),
                crate::subagents::TaskAttempt::for_test(1, 2, None),
            ],
            executions: vec![crate::subagents::TaskExecution::for_test(
                2,
                "value = 1".to_owned(),
                "reasoning".to_owned(),
                task_execution(),
                vec![crate::model::AttemptAccounting::UsageUnavailable],
            )],
            outcome: Some(crate::subagents::TaskOutcome::Completed(
                crate::subagents::ChildResult::new("done".to_owned(), values),
            )),
        });

        let details = decode_task_details(&encode_task_details(&record).expect("encode task"))
            .expect("decode task");

        assert_eq!(details.prompt, "investigate");
        assert_eq!(details.selection.model_id, crate::config::DEFAULT_MODEL.id);
        assert_eq!(details.depth, 2);
        assert!(details.read_only);
        assert_eq!(details.admitted_at_ms, 100);
        assert_eq!(details.terminal_at_ms, Some(200));
        assert_eq!(details.observation.step_count, 3);
        assert_eq!(details.observation.elapsed_seconds, 4);
        assert_eq!(details.observation.elapsed_subsec_nanos, 5);
        assert_eq!(details.observation.observed_at_ms, 200);
        assert_eq!(details.attempt_evidence.len(), 2);
        assert_eq!(details.attempt_evidence[0].model_step, 1);
        assert_eq!(details.attempt_evidence[0].provider_attempt, 1);
        assert!(matches!(
            details.attempt_evidence[0].accounting,
            Some(super::StoredAttemptAccounting::KnownUsage(_))
        ));
        assert_eq!(details.attempt_evidence[1].model_step, 1);
        assert_eq!(details.attempt_evidence[1].provider_attempt, 2);
        assert!(details.attempt_evidence[1].accounting.is_none());
        assert_eq!(details.executions.len(), 1);
        assert_eq!(details.executions[0].model_step, 2);
        assert_eq!(details.executions[0].source, "value = 1");
        assert_eq!(details.executions[0].reasoning_summary, "reasoning");
        assert_eq!(details.executions[0].execution.stdout.head, "stdout");
        assert_eq!(
            details.executions[0].execution.read_paths,
            ["6e65737465642f72656164"]
        );
        assert_eq!(
            details.executions[0].execution.modified_paths,
            ["6e65737465642f6d6f646966696564"]
        );
        let restored = decode_task_record_details(
            &encode_task_details(&record).expect("encode child task execution"),
        )
        .expect("decode child task execution");
        assert_eq!(
            restored.details.executions[0].execution().read_paths,
            [PathBuf::from("nested/read")]
        );
        assert_eq!(
            restored.details.executions[0].execution().modified_paths,
            [PathBuf::from("nested/modified")]
        );
        assert!(matches!(
            details.executions[0].execution.outcome,
            super::StoredExecutionOutcome::Complete { result: None }
        ));
        assert!(matches!(
            details.executions[0].attempts.as_slice(),
            [super::StoredAttemptAccounting::UsageUnavailable]
        ));
        let attempts = details
            .attempts
            .into_iter()
            .map(|attempt| super::decode_attempt(&attempt))
            .collect::<Vec<_>>();
        assert_eq!(
            crate::pricing::estimate_run_cost(&attempts),
            record.observation().cost()
        );
        let Some(super::StoredTaskOutcome::Completed { message, values }) = details.outcome else {
            panic!("completed outcome must round trip");
        };
        assert_eq!(message, "done");
        assert!(matches!(
            values.get("none"),
            Some(super::StoredToolValue::None)
        ));
        assert!(matches!(
            values.get("bool"),
            Some(super::StoredToolValue::Bool(true))
        ));
        assert!(matches!(
            values.get("string"),
            Some(super::StoredToolValue::String(value)) if value == "value"
        ));
        assert!(matches!(
            values.get("integer"),
            Some(super::StoredToolValue::Integer(-7))
        ));
        for (key, expected) in [
            ("nan", f64::NAN.to_bits()),
            ("positive_infinity", f64::INFINITY.to_bits()),
            ("negative_infinity", f64::NEG_INFINITY.to_bits()),
            ("negative_zero", (-0.0_f64).to_bits()),
        ] {
            assert_eq!(
                match values.get(key).expect("float result") {
                    super::StoredToolValue::FloatBits(bits) => *bits,
                    _ => panic!("{key} result must preserve float bits"),
                },
                expected
            );
        }
        assert!(matches!(
            values.get("list"),
            Some(super::StoredToolValue::List(values))
                if matches!(values.as_slice(), [
                    super::StoredToolValue::None,
                    super::StoredToolValue::Bool(false),
                ])
        ));
        let super::StoredToolValue::Dictionary(dictionary) =
            values.get("dictionary").expect("dictionary result")
        else {
            panic!("dictionary result must round trip");
        };
        assert!(matches!(
            dictionary.get("nested"),
            Some(super::StoredToolValue::String(value)) if value == "value"
        ));
    }

    #[test]
    fn interrupted_task_details_codec_preserves_unfinished_history() {
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .expect("default selection");
        let record = crate::subagents::TaskRecord::for_test(
            "0a",
            9,
            selection,
            Some("01"),
            2,
            vec![task_usage(), task_usage()],
        )
        .with_parameters_for_test("investigate".to_owned(), selection, true)
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 100,
            terminal_at_ms: None,
            state: crate::subagents::TaskState::Running,
            step_count: 3,
            elapsed: std::time::Duration::new(4, 567_890_123),
            observed_at_ms: 200,
            attempt_evidence: vec![
                crate::subagents::TaskAttempt::for_test(1, 1, Some(task_usage())),
                crate::subagents::TaskAttempt::for_test(1, 2, None),
            ],
            executions: vec![crate::subagents::TaskExecution::for_test(
                2,
                "value = 1".to_owned(),
                "reasoning".to_owned(),
                task_execution(),
                vec![task_usage(), task_usage()],
            )],
            outcome: None,
        });
        let original = encode_task_details(&record).expect("encode task details");
        let recovered =
            fail_unfinished_task_details(&original, 300).expect("fail unfinished task details");
        let mut original: serde_json::Value =
            serde_json::from_str(&original).expect("decode original task details");
        let mut recovered: serde_json::Value =
            serde_json::from_str(&recovered).expect("decode recovered task details");
        assert_eq!(recovered["terminal_at_ms"], serde_json::json!(300));
        assert_eq!(
            recovered["outcome"],
            serde_json::json!({ "Failed": { "message": "Interrupted by previous process exit" } })
        );
        for details in [&mut original, &mut recovered] {
            let object = details.as_object_mut().expect("task details object");
            object.remove("terminal_at_ms");
            object.remove("outcome");
        }
        assert_eq!(recovered, original);
        let object = recovered.as_object().expect("task details object");
        assert!(!object.contains_key("cost"));
        assert!(!object.contains_key("offline"));

        for (terminal_at_ms, outcome) in [
            (serde_json::json!(200), serde_json::Value::Null),
            (
                serde_json::Value::Null,
                serde_json::json!({ "Failed": { "message": "already terminal" } }),
            ),
        ] {
            let mut invalid = original.clone();
            let object = invalid.as_object_mut().expect("task details object");
            object.insert("terminal_at_ms".to_owned(), terminal_at_ms);
            object.insert("outcome".to_owned(), outcome);
            assert!(fail_unfinished_task_details(&invalid.to_string(), 300).is_err());
        }
    }

    #[test]
    fn task_details_codec_maps_failed_cancelled_and_timed_out_outcomes() {
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .expect("default selection");
        let cases = [
            (
                crate::subagents::TaskState::Failed,
                crate::subagents::TaskOutcome::Failed(crate::subagents::ChildFailure::new(
                    "failed",
                )),
                "Failed",
            ),
            (
                crate::subagents::TaskState::Cancelled,
                crate::subagents::TaskOutcome::Cancelled,
                "Cancelled",
            ),
            (
                crate::subagents::TaskState::TimedOut,
                crate::subagents::TaskOutcome::TimedOut,
                "TimedOut",
            ),
        ];
        for (state, outcome, expected) in cases {
            let record =
                crate::subagents::TaskRecord::for_test("0a", 2, selection, None, 1, Vec::new())
                    .with_persisted_details_for_test(TaskPersistedDetails {
                        admitted_at_ms: 10,
                        terminal_at_ms: Some(20),
                        state,
                        step_count: 0,
                        elapsed: std::time::Duration::ZERO,
                        observed_at_ms: 20,
                        attempt_evidence: Vec::new(),
                        executions: Vec::new(),
                        outcome: Some(outcome),
                    });
            let details = decode_task_details(&encode_task_details(&record).expect("encode task"))
                .expect("decode task");
            let actual = match details.outcome.expect("terminal outcome") {
                super::StoredTaskOutcome::Failed { message } => {
                    assert_eq!(message, "failed");
                    "Failed"
                }
                super::StoredTaskOutcome::Cancelled => "Cancelled",
                super::StoredTaskOutcome::TimedOut => "TimedOut",
                super::StoredTaskOutcome::Completed { .. } => "Completed",
            };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn task_details_codec_stores_public_id_but_excludes_internal_structure() {
        let selection = crate::config::ModelSelection::new(
            crate::config::DEFAULT_MODEL,
            crate::config::DEFAULT_MODEL.default_thinking,
        )
        .expect("default selection");
        let record = crate::subagents::TaskRecord::for_test(
            "0a",
            9,
            selection,
            Some("01"),
            2,
            vec![task_usage()],
        )
        .with_history_steps_for_test(Some(3), Some(4))
        .with_persisted_details_for_test(TaskPersistedDetails {
            admitted_at_ms: 10,
            terminal_at_ms: Some(20),
            state: crate::subagents::TaskState::Failed,
            step_count: 1,
            elapsed: std::time::Duration::ZERO,
            observed_at_ms: 20,
            attempt_evidence: Vec::new(),
            executions: Vec::new(),
            outcome: Some(crate::subagents::TaskOutcome::Failed(
                crate::subagents::ChildFailure::new("not stored here"),
            )),
        });
        let json: serde_json::Value =
            serde_json::from_str(&encode_task_details(&record).expect("encode task details"))
                .expect("decode JSON");
        let object = json.as_object().expect("task details object");
        assert_eq!(
            object.get("id").and_then(serde_json::Value::as_str),
            Some("0a")
        );
        assert_eq!(object["admission_history_step_id"], 3);
        assert_eq!(object["terminal_history_step_id"], 4);
        let decoded = decode_task_record_details(
            &serde_json::to_string(&json).expect("re-encode task details"),
        )
        .expect("decode task details");
        assert_eq!(decoded.admission_history_step_id, Some(3));
        assert_eq!(decoded.terminal_history_step_id, Some(4));
        for key in [
            "cost",
            "parent",
            "origin",
            "origin_step",
            "state",
            "revision",
            "checkpoint",
            "arena",
            "error",
        ] {
            assert!(!object.contains_key(key), "details must not store `{key}`");
        }
        assert!(
            !object
                .get("observation")
                .expect("observation")
                .as_object()
                .expect("observation object")
                .contains_key("error")
        );
    }

    #[test]
    fn tool_value_codec_preserves_nonfinite_values_and_signed_zero() {
        let values = ToolValue::List(vec![
            ToolValue::Float(f64::NAN),
            ToolValue::Float(f64::INFINITY),
            ToolValue::Float(f64::NEG_INFINITY),
            ToolValue::Float(-0.0),
            ToolValue::Dictionary(std::collections::BTreeMap::from([(
                "nested".to_owned(),
                ToolValue::Float(f64::from_bits(0x7ff8_0000_0000_0001)),
            )])),
        ]);
        let encoded = serde_json::to_string(&encode_tool_value(&values)).unwrap();
        let decoded = decode_tool_value(serde_json::from_str(&encoded).unwrap());
        let ToolValue::List(decoded) = decoded else {
            panic!("decoded tool value should be a list");
        };
        let bits = decoded
            .iter()
            .filter_map(|value| match value {
                ToolValue::Float(value) => Some(value.to_bits()),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            bits,
            [
                f64::NAN.to_bits(),
                f64::INFINITY.to_bits(),
                f64::NEG_INFINITY.to_bits(),
                (-0.0_f64).to_bits()
            ]
        );
        let ToolValue::Dictionary(nested) = &decoded[4] else {
            panic!("nested tool value should be a dictionary");
        };
        assert_eq!(
            match nested.get("nested").unwrap() {
                ToolValue::Float(value) => value.to_bits(),
                _ => panic!("nested tool value should be a float"),
            },
            0x7ff8_0000_0000_0001
        );
    }
}
