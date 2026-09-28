use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::config::models::Provider;
use crate::config::settings::CrossProviderReasoning;
use crate::config::{CompactionSettings, ModelSelection};
use crate::model::{
    ActiveSkills, AttemptAccounting, ForeignPart, ModelContextEntry, ModelUsage, SkillActivation,
    SummaryRequest, canonical_compaction_summary,
};
use crate::prompt::{
    compaction_initial_prompt, compaction_split_run_prompt, compaction_system_prompt,
    compaction_update_prompt,
};
use crate::session::{CompactionReason, SessionEntryId};

const OUTPUT_RATIO_DENOMINATOR: u64 = 100;
const HISTORY_OUTPUT_RATIO: u64 = 80;
const SPLIT_OUTPUT_RATIO: u64 = 50;
const FEEDBACK_SCALAR_LIMIT: usize = 2_000;
const SPLIT_SEPARATOR: &str = "\n\n---\n\n**Turn Context (split turn):**\n\n";
const FOREIGN_TOOL_NAME: &str = "execute_python";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedAncestryFact {
    pub(crate) entry_id: SessionEntryId,
    pub(crate) kind: SelectedAncestryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectedAncestryKind {
    Context {
        entry: ModelContextEntry,
        normal_attempts: Vec<AttemptAccounting>,
        read_paths: Vec<PathBuf>,
        modified_paths: Vec<PathBuf>,
    },
    ContextNeutral {
        read_paths: Vec<PathBuf>,
        modified_paths: Vec<PathBuf>,
    },
    SkillActivation {
        activation: SkillActivation,
    },
    SuppressedSkillActivation {
        name: String,
    },
    Applied {
        summary: String,
        first_retained: SessionEntryId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionPreparation {
    context_tokens: u64,
    first_retained: SessionEntryId,
    retained_entries: Vec<ModelContextEntry>,
    protected_skills: Vec<SkillActivation>,
    requests: Vec<SummaryRequest>,
    assembly: SummaryAssembly,
    files: FileSets,
}

impl CompactionPreparation {
    pub(crate) fn context_tokens(&self) -> u64 {
        self.context_tokens
    }

    pub(crate) fn first_retained(&self) -> SessionEntryId {
        self.first_retained
    }

    pub(crate) fn requests(&self) -> &[SummaryRequest] {
        &self.requests
    }

    pub(crate) fn retained_entries(&self) -> &[ModelContextEntry] {
        &self.retained_entries
    }

    pub(crate) fn protected_skills(&self) -> &[SkillActivation] {
        &self.protected_skills
    }

    pub(crate) fn finish(&self, outputs: &[String]) -> Result<String, CompactionError> {
        if outputs.len() != self.requests.len() {
            return Err(CompactionError::UnexpectedOutputCount {
                expected: self.requests.len(),
                actual: outputs.len(),
            });
        }
        let outputs = outputs
            .iter()
            .map(|output| strip_reserved_blocks(output))
            .collect::<Result<Vec<_>, _>>()?;
        let history = self
            .assembly
            .history_output
            .map(|index| outputs[index].clone())
            .or_else(|| self.assembly.history.clone())
            .expect("preparation always has history output or a history value");
        let summary = match self.assembly.split_output {
            Some(index) => format!("{history}{SPLIT_SEPARATOR}{}", outputs[index]),
            None => history,
        };
        Ok(append_files(summary, &self.files))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SummaryAssembly {
    history: Option<String>,
    history_output: Option<usize>,
    split_output: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSets {
    read: Vec<PathBuf>,
    modified: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum CompactionError {
    #[error("invalid selected ancestry: {0}")]
    InvalidAncestry(&'static str),
    #[error("reserved file tag is malformed: {0}")]
    MalformedReservedTag(&'static str),
    #[error("expected {expected} summary outputs, received {actual}")]
    UnexpectedOutputCount { expected: usize, actual: usize },
    #[error("file path is not valid UTF-8")]
    NonUtf8Path,
}

pub(crate) fn prepare(
    facts: &[SelectedAncestryFact],
    policy: CompactionSettings,
    selection: ModelSelection,
    reasoning: CrossProviderReasoning,
    focus: &str,
    reason: CompactionReason,
) -> Result<Option<CompactionPreparation>, CompactionError> {
    if reason != CompactionReason::Manual && !policy.enabled() {
        return Ok(None);
    }
    let destination = Destination::new(selection, reasoning);
    let latest = latest_applied(facts)?;
    let active_skills = active_skills(facts);
    let protected_skills = active_skills.iter().cloned().collect();
    let projected = projected_context_indices(facts, latest)?;
    let context_tokens = measure_context(
        facts,
        &projected,
        latest,
        destination,
        selection.model().id,
        &active_skills,
    );
    if reason == CompactionReason::AutomaticThreshold
        && !exceeds_threshold(
            context_tokens,
            selection.model().context_window_tokens,
            policy.reserve_tokens(),
        )
    {
        return Ok(None);
    }
    let start = latest.map_or(0, |applied| applied.boundary_index);
    let Some(first_retained_index) = first_retained_index(
        facts,
        start,
        policy.keep_recent_tokens(),
        destination,
        &active_skills,
    ) else {
        return Ok(None);
    };
    if !has_discarded_context(facts, start, first_retained_index) {
        return Ok(None);
    }

    let (history_blocks, split_blocks) =
        split_blocks(facts, start, first_retained_index, destination);
    let previous = latest
        .map(|applied| strip_reserved_blocks(applied.summary))
        .transpose()?;
    let focus = focus.trim();
    let mut requests = Vec::new();
    let mut history = None;
    let mut history_output = None;

    if history_blocks.is_empty() && split_blocks.is_some() {
        match previous {
            Some(previous) if focus.is_empty() => history = Some(previous),
            Some(previous) => {
                history_output = Some(requests.len());
                requests.push(summary_request(
                    history_input("", Some(&previous), compaction_update_prompt(), focus),
                    selection,
                    output_budget(
                        policy.reserve_tokens(),
                        HISTORY_OUTPUT_RATIO,
                        selection.model().max_output_tokens,
                    ),
                ));
            }
            None => history = Some("No prior history.".to_owned()),
        }
    } else {
        history_output = Some(requests.len());
        requests.push(summary_request(
            history_input(
                &history_blocks,
                previous.as_deref(),
                if previous.is_some() {
                    compaction_update_prompt()
                } else {
                    compaction_initial_prompt()
                },
                focus,
            ),
            selection,
            output_budget(
                policy.reserve_tokens(),
                HISTORY_OUTPUT_RATIO,
                selection.model().max_output_tokens,
            ),
        ));
    }

    let split_output = split_blocks.map(|blocks| {
        let index = requests.len();
        requests.push(summary_request(
            split_input(&blocks, focus),
            selection,
            output_budget(
                policy.reserve_tokens(),
                SPLIT_OUTPUT_RATIO,
                selection.model().max_output_tokens,
            ),
        ));
        index
    });

    let retained_entries = facts[first_retained_index..]
        .iter()
        .filter(|fact| ordinary_context(fact))
        .filter_map(context_entry)
        .cloned()
        .collect();

    Ok(Some(CompactionPreparation {
        context_tokens,
        first_retained: facts[first_retained_index].entry_id,
        retained_entries,
        protected_skills,
        requests,
        assembly: SummaryAssembly {
            history,
            history_output,
            split_output,
        },
        files: cumulative_files(facts, first_retained_index)?,
    }))
}

#[derive(Clone, Copy)]
struct Destination {
    provider: &'static str,
    reasoning: CrossProviderReasoning,
}

impl Destination {
    fn new(selection: ModelSelection, reasoning: CrossProviderReasoning) -> Self {
        // Must match the replay provider names written by each adapter.
        let provider = match selection.model().provider() {
            Provider::OpenAi => "openai",
            Provider::Anthropic => "anthropic",
        };
        Self {
            provider,
            reasoning,
        }
    }

    fn foreign_parts<'a>(self, entry: &'a ModelContextEntry) -> Option<Vec<ForeignPart<'a>>> {
        entry.foreign_parts(self.provider, self.reasoning)
    }
}

#[derive(Clone, Copy)]
struct LatestApplied<'a> {
    index: usize,
    boundary_index: usize,
    summary: &'a str,
}

fn latest_applied(
    facts: &[SelectedAncestryFact],
) -> Result<Option<LatestApplied<'_>>, CompactionError> {
    let Some((index, summary, first_retained)) =
        facts
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, fact)| match &fact.kind {
                SelectedAncestryKind::Applied {
                    summary,
                    first_retained,
                } => Some((index, summary.as_str(), *first_retained)),
                _ => None,
            })
    else {
        return Ok(None);
    };
    let Some(boundary_index) = facts[..index]
        .iter()
        .position(|fact| fact.entry_id == first_retained)
    else {
        return Err(CompactionError::InvalidAncestry(
            "applied first-retained entry is absent or not earlier",
        ));
    };
    if !ordinary_context(&facts[boundary_index]) {
        return Err(CompactionError::InvalidAncestry(
            "applied first-retained entry is not context-bearing",
        ));
    }
    Ok(Some(LatestApplied {
        index,
        boundary_index,
        summary,
    }))
}

fn projected_context_indices(
    facts: &[SelectedAncestryFact],
    latest: Option<LatestApplied<'_>>,
) -> Result<Vec<usize>, CompactionError> {
    let range = latest.map_or(0..facts.len(), |applied| {
        applied.boundary_index..facts.len()
    });
    let mut projected = Vec::new();
    for index in range {
        let Some(entry) = context_entry(&facts[index]) else {
            continue;
        };
        if latest.is_some() && !ordinary_entry(entry) {
            continue;
        }
        projected.push(index);
    }
    if latest.is_some_and(|applied| !projected.contains(&applied.boundary_index)) {
        return Err(CompactionError::InvalidAncestry(
            "applied first-retained entry does not project",
        ));
    }
    Ok(projected)
}

fn measure_context(
    facts: &[SelectedAncestryFact],
    projected: &[usize],
    latest: Option<LatestApplied<'_>>,
    destination: Destination,
    model_id: &str,
    current_skills: &ActiveSkills,
) -> u64 {
    let projected_entry =
        |index: usize| context_entry(&facts[index]).expect("projected entries are context entries");
    if let Some((position, baseline)) =
        usage_baseline(facts, projected, latest, destination, model_id)
    {
        let mut bytes = completed_feedback_bytes(projected_entry(projected[position]));
        for &later in &projected[position + 1..] {
            bytes = bytes.saturating_add(context_bytes(projected_entry(later), destination));
        }
        let previous_bytes = active_text_bytes(&active_skills(&facts[..=projected[position]]));
        let current_bytes = active_text_bytes(current_skills);
        let adjusted = if current_bytes >= previous_bytes {
            baseline.saturating_add(estimate_tokens(current_bytes - previous_bytes))
        } else {
            baseline.saturating_sub(estimate_tokens(previous_bytes - current_bytes))
        };
        return adjusted.saturating_add(estimate_tokens(bytes));
    }

    let mut bytes = latest
        .map(|applied| byte_len(&canonical_compaction_summary(applied.summary)))
        .unwrap_or(0);
    for &index in projected {
        bytes = bytes.saturating_add(context_bytes(projected_entry(index), destination));
    }
    estimate_tokens(bytes.saturating_add(active_text_bytes(current_skills)))
}

fn exceeds_threshold(context_tokens: u64, context_window: u64, reserve_tokens: u64) -> bool {
    context_tokens > context_window.saturating_sub(reserve_tokens)
}

fn usage_baseline(
    facts: &[SelectedAncestryFact],
    projected: &[usize],
    latest: Option<LatestApplied<'_>>,
    destination: Destination,
    model_id: &str,
) -> Option<(usize, u64)> {
    let any_foreign = projected.iter().any(|&index| {
        context_entry(&facts[index])
            .and_then(|entry| destination.foreign_parts(entry))
            .is_some()
    });
    if any_foreign {
        return None;
    }
    let baseline_start = latest.map_or(0, |applied| applied.index.saturating_add(1));
    for (position, &index) in projected.iter().enumerate().rev() {
        if index < baseline_start {
            continue;
        }
        let Some(entry) = context_entry(&facts[index]) else {
            continue;
        };
        if entry.completed_code().is_none() && entry.final_message().is_none() {
            continue;
        }
        let Some(attempts) = normal_attempts(&facts[index]) else {
            continue;
        };
        for usage in attempts.iter().rev().filter_map(known_usage) {
            if usage.model != model_id {
                return None;
            }
            if let Some(tokens) = usage_tokens(usage) {
                return Some((position, tokens));
            }
        }
    }
    None
}

fn active_skills(facts: &[SelectedAncestryFact]) -> ActiveSkills {
    let mut active = ActiveSkills::default();
    for fact in facts {
        match &fact.kind {
            SelectedAncestryKind::SkillActivation { activation } => {
                active.replay(activation.clone());
            }
            SelectedAncestryKind::SuppressedSkillActivation { name } => {
                active.deactivate(name);
            }
            _ => {}
        }
    }
    active
}

fn active_text_bytes(active: &ActiveSkills) -> u64 {
    u64::try_from(active.text_bytes()).unwrap_or(u64::MAX)
}

fn known_usage(attempt: &AttemptAccounting) -> Option<&ModelUsage> {
    match attempt {
        AttemptAccounting::KnownUsage(usage)
        | AttemptAccounting::KnownUsageWithUntrackedRetries(usage) => Some(usage),
        AttemptAccounting::UsageUnavailable => None,
    }
}

fn usage_tokens(usage: &ModelUsage) -> Option<u64> {
    usage.total_tokens.filter(|total| *total != 0).or_else(|| {
        usage
            .input_tokens
            .zip(usage.output_tokens)
            .and_then(|(input, output)| input.checked_add(output))
            .filter(|total| *total != 0)
    })
}

fn first_retained_index(
    facts: &[SelectedAncestryFact],
    start: usize,
    keep_recent_tokens: u64,
    destination: Destination,
    active_skills: &ActiveSkills,
) -> Option<usize> {
    let mut bytes = active_text_bytes(active_skills);
    for index in (start..facts.len()).rev() {
        if !ordinary_context(&facts[index]) {
            continue;
        }
        bytes = bytes.saturating_add(context_bytes(
            context_entry(&facts[index]).expect("ordinary entries are context entries"),
            destination,
        ));
        if estimate_tokens(bytes) >= keep_recent_tokens {
            return Some(index);
        }
    }
    None
}

fn has_discarded_context(facts: &[SelectedAncestryFact], start: usize, retained: usize) -> bool {
    facts[start..retained].iter().any(ordinary_context)
}

fn split_blocks(
    facts: &[SelectedAncestryFact],
    start: usize,
    retained: usize,
    destination: Destination,
) -> (String, Option<String>) {
    if user_request(&facts[retained]).is_some() {
        return (serialise_range(facts, start, retained, destination), None);
    }
    let split_start = (start..retained)
        .rev()
        .find(|index| user_request(&facts[*index]).is_some());
    match split_start {
        Some(split_start) => (
            serialise_range(facts, start, split_start, destination),
            Some(serialise_range(facts, split_start, retained, destination)),
        ),
        None => (serialise_range(facts, start, retained, destination), None),
    }
}

fn cumulative_files(
    facts: &[SelectedAncestryFact],
    retained: usize,
) -> Result<FileSets, CompactionError> {
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    for fact in &facts[..retained] {
        let (reads, modifications) = file_paths(fact);
        read.extend(reads.iter().cloned());
        modified.extend(modifications.iter().cloned());
    }
    read.retain(|path| !modified.contains(path));
    for path in read.iter().chain(modified.iter()) {
        if path.to_str().is_none() {
            return Err(CompactionError::NonUtf8Path);
        }
    }
    Ok(FileSets {
        read: read.into_iter().collect(),
        modified: modified.into_iter().collect(),
    })
}

fn summary_request(input: String, selection: ModelSelection, output_limit: u64) -> SummaryRequest {
    SummaryRequest {
        instructions: compaction_system_prompt().to_owned(),
        input,
        selection,
        output_limit,
    }
}

fn history_input(blocks: &str, previous: Option<&str>, prompt: &str, focus: &str) -> String {
    let mut input = conversation_input(blocks);
    if let Some(previous) = previous {
        input.push_str("<previous-summary>\n");
        input.push_str(previous);
        input.push_str("\n</previous-summary>\n\n");
    }
    input.push_str(prompt);
    append_focus(&mut input, focus);
    input
}

fn split_input(blocks: &str, focus: &str) -> String {
    let mut input = conversation_input(blocks);
    input.push_str(compaction_split_run_prompt());
    append_focus(&mut input, focus);
    input
}

fn conversation_input(blocks: &str) -> String {
    format!("<conversation>\n{blocks}\n</conversation>\n\n")
}

fn append_focus(input: &mut String, focus: &str) {
    if !focus.is_empty() {
        input.push_str("\n\nAdditional focus: ");
        input.push_str(focus);
    }
}

fn output_budget(reserve: u64, ratio: u64, cap: u64) -> u64 {
    (reserve.saturating_mul(ratio) / OUTPUT_RATIO_DENOMINATOR)
        .max(1)
        .min(cap)
}

fn context_entry(fact: &SelectedAncestryFact) -> Option<&ModelContextEntry> {
    match &fact.kind {
        SelectedAncestryKind::Context { entry, .. } => Some(entry),
        _ => None,
    }
}

fn normal_attempts(fact: &SelectedAncestryFact) -> Option<&[AttemptAccounting]> {
    match &fact.kind {
        SelectedAncestryKind::Context {
            normal_attempts, ..
        } => Some(normal_attempts),
        _ => None,
    }
}

fn file_paths(fact: &SelectedAncestryFact) -> (&[PathBuf], &[PathBuf]) {
    match &fact.kind {
        SelectedAncestryKind::Context {
            read_paths,
            modified_paths,
            ..
        }
        | SelectedAncestryKind::ContextNeutral {
            read_paths,
            modified_paths,
        } => (read_paths, modified_paths),
        SelectedAncestryKind::Applied { .. }
        | SelectedAncestryKind::SkillActivation { .. }
        | SelectedAncestryKind::SuppressedSkillActivation { .. } => (&[], &[]),
    }
}

fn ordinary_context(fact: &SelectedAncestryFact) -> bool {
    context_entry(fact).is_some_and(ordinary_entry)
}

fn ordinary_entry(entry: &ModelContextEntry) -> bool {
    entry.user_request().is_some()
        || entry.completed_code().is_some()
        || entry.final_message().is_some()
}

fn user_request(fact: &SelectedAncestryFact) -> Option<&str> {
    context_entry(fact).and_then(ModelContextEntry::user_request)
}

fn context_bytes(entry: &ModelContextEntry, destination: Destination) -> u64 {
    if let Some(parts) = destination.foreign_parts(entry) {
        return parts.into_iter().fold(0, |total, part| {
            total.saturating_add(foreign_part_bytes(part))
        });
    }
    if let Some(text) = entry.user_request() {
        return byte_len(text);
    }
    if let Some(summary) = entry.compaction_summary() {
        return byte_len(&canonical_compaction_summary(summary));
    }
    if let Some(completed) = entry.completed_code() {
        return replay_bytes(completed.replay())
            .saturating_add(byte_len(completed.result().feedback()));
    }
    let Some((_, _, replay)) = entry.final_message() else {
        return 0;
    };
    replay_bytes(replay)
}

fn foreign_part_bytes(part: ForeignPart<'_>) -> u64 {
    match part {
        ForeignPart::Text(text) => byte_len(text),
        ForeignPart::ToolCall {
            call_id,
            source,
            output,
        } => [call_id, FOREIGN_TOOL_NAME, source, output]
            .into_iter()
            .fold(0, |total, value| total.saturating_add(byte_len(value))),
    }
}

fn completed_feedback_bytes(entry: &ModelContextEntry) -> u64 {
    entry
        .completed_code()
        .map_or(0, |completed| byte_len(completed.result().feedback()))
}

fn replay_bytes(replay: &crate::model::ProviderReplay) -> u64 {
    replay.items().iter().fold(0, |total, item| {
        total.saturating_add(byte_len_bytes(item.as_bytes()))
    })
}

fn byte_len(text: &str) -> u64 {
    byte_len_bytes(text.as_bytes())
}

fn byte_len_bytes(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

fn estimate_tokens(bytes: u64) -> u64 {
    bytes / 4 + u64::from(!bytes.is_multiple_of(4))
}

fn serialise_range(
    facts: &[SelectedAncestryFact],
    start: usize,
    end: usize,
    destination: Destination,
) -> String {
    facts[start..end]
        .iter()
        .filter_map(|fact| {
            context_entry(fact).and_then(|entry| serialise_entry(entry, destination))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn serialise_entry(entry: &ModelContextEntry, destination: Destination) -> Option<String> {
    if let Some(text) = entry.user_request() {
        return Some(format!("[User]: {text}"));
    }
    let reasoning_dropped = destination.reasoning == CrossProviderReasoning::Drop
        && destination.foreign_parts(entry).is_some();
    if let Some(completed) = entry.completed_code() {
        let mut blocks = Vec::new();
        if !reasoning_dropped && !completed.reasoning_summary().is_empty() {
            blocks.push(format!(
                "[Assistant reasoning]: {}",
                completed.reasoning_summary()
            ));
        }
        let action = match completed
            .commentary()
            .filter(|commentary| !commentary.is_empty())
        {
            Some(commentary) => format!(
                "[Assistant action]: {commentary}\n\nPython:\n{}",
                completed.source()
            ),
            None => format!("[Assistant action]: Python:\n{}", completed.source()),
        };
        blocks.push(action);
        blocks.push(format!(
            "[Execution result]: {}",
            truncate_feedback(completed.result().feedback())
        ));
        return Some(blocks.join("\n\n"));
    }
    if let Some((text, reasoning_summary, _)) = entry.final_message() {
        let mut blocks = Vec::new();
        if !reasoning_dropped && !reasoning_summary.is_empty() {
            blocks.push(format!("[Assistant reasoning]: {reasoning_summary}"));
        }
        blocks.push(format!("[Assistant action]: {text}"));
        return Some(blocks.join("\n\n"));
    }
    None
}

fn truncate_feedback(feedback: &str) -> String {
    let Some(index) = feedback
        .char_indices()
        .nth(FEEDBACK_SCALAR_LIMIT)
        .map(|(index, _)| index)
    else {
        return feedback.to_owned();
    };
    let omitted = feedback[index..].chars().count();
    format!(
        "{}\n\n[... {omitted} more characters truncated]",
        &feedback[..index]
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReservedTag {
    Read,
    Modified,
}

impl ReservedTag {
    fn open(self) -> &'static str {
        match self {
            Self::Read => "<read-files>",
            Self::Modified => "<modified-files>",
        }
    }

    fn close(self) -> &'static str {
        match self {
            Self::Read => "</read-files>",
            Self::Modified => "</modified-files>",
        }
    }
}

fn strip_reserved_blocks(text: &str) -> Result<String, CompactionError> {
    let mut stripped = String::with_capacity(text.len());
    let mut active = None;
    for line in text.split_inclusive('\n') {
        let line_without_lf = line.strip_suffix('\n').unwrap_or(line);
        let opening = reserved_tag(line_without_lf);
        let closing = reserved_close(line_without_lf);
        match active {
            None => match (opening, closing) {
                (Some(tag), None) => active = Some(tag),
                (None, Some(_)) => {
                    return Err(CompactionError::MalformedReservedTag(
                        "unmatched closing tag",
                    ));
                }
                (None, None) if has_reserved_token(line) => {
                    return Err(CompactionError::MalformedReservedTag("inline tag"));
                }
                (None, None) => stripped.push_str(line),
                _ => return Err(CompactionError::MalformedReservedTag("inline tag")),
            },
            Some(tag) => match (opening, closing) {
                (Some(_), _) => return Err(CompactionError::MalformedReservedTag("nested tag")),
                (None, Some(close)) if close == tag => active = None,
                (None, Some(_)) => {
                    return Err(CompactionError::MalformedReservedTag(
                        "mismatched closing tag",
                    ));
                }
                (None, None) if has_reserved_token(line) => {
                    return Err(CompactionError::MalformedReservedTag("inline tag"));
                }
                (None, None) => {}
            },
        }
    }
    if active.is_some() {
        return Err(CompactionError::MalformedReservedTag(
            "unmatched opening tag",
        ));
    }
    Ok(stripped)
}

fn reserved_tag(line: &str) -> Option<ReservedTag> {
    [ReservedTag::Read, ReservedTag::Modified]
        .into_iter()
        .find(|tag| line == tag.open())
}

fn reserved_close(line: &str) -> Option<ReservedTag> {
    [ReservedTag::Read, ReservedTag::Modified]
        .into_iter()
        .find(|tag| line == tag.close())
}

fn has_reserved_token(text: &str) -> bool {
    [ReservedTag::Read, ReservedTag::Modified]
        .into_iter()
        .any(|tag| text.contains(tag.open()) || text.contains(tag.close()))
}

fn append_files(mut summary: String, files: &FileSets) -> String {
    let mut sections = Vec::new();
    if !files.read.is_empty() {
        sections.push(file_section(ReservedTag::Read, &files.read));
    }
    if !files.modified.is_empty() {
        sections.push(file_section(ReservedTag::Modified, &files.modified));
    }
    if !sections.is_empty() && !summary.is_empty() {
        summary.push_str("\n\n");
    }
    summary.push_str(&sections.join("\n\n"));
    summary
}

fn file_section(tag: ReservedTag, paths: &[PathBuf]) -> String {
    let paths = paths
        .iter()
        .map(|path| {
            let encoded = serde_json::to_string(path.to_str().expect("paths were validated"))
                .expect("serialising a string cannot fail");
            encoded.replace('<', "\\u003c").replace('>', "\\u003e")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{}\n{paths}\n{}", tag.open(), tag.close())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        CompactionError, Destination, SPLIT_SEPARATOR, SelectedAncestryFact, SelectedAncestryKind,
        estimate_tokens, exceeds_threshold, output_budget, prepare, serialise_entry,
        strip_reserved_blocks, truncate_feedback,
    };
    use crate::config::models::{OPUS_5_5, TERRA};
    use crate::config::settings::CrossProviderReasoning;
    use crate::config::{DEFAULT_MODEL, ModelSelection};
    use crate::model::{
        AttemptAccounting, CodeResult, ModelCallId, ModelContextEntry, ModelHistory, ModelUsage,
        OpaqueReplayItem, ProviderReplay, SkillActivation,
    };
    use crate::session::{CompactionReason, SessionEntryId};
    use crate::skills::SkillOrigin;

    const INLINE: CrossProviderReasoning = CrossProviderReasoning::Inline;
    const MANUAL: CompactionReason = CompactionReason::Manual;
    const THRESHOLD: CompactionReason = CompactionReason::AutomaticThreshold;
    const OVERFLOW: CompactionReason = CompactionReason::ContextOverflow;
    const NATIVE: &str = "openai";

    fn selection() -> ModelSelection {
        ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking).unwrap()
    }

    fn native() -> Destination {
        Destination::new(selection(), INLINE)
    }

    fn anthropic_selection() -> ModelSelection {
        ModelSelection::new(&OPUS_5_5, OPUS_5_5.default_thinking).unwrap()
    }

    fn policy(keep_recent_tokens: u64) -> crate::config::CompactionSettings {
        crate::config::CompactionSettings::new(100, keep_recent_tokens)
    }

    fn context(
        id: u64,
        entry: ModelContextEntry,
        normal_attempts: Vec<AttemptAccounting>,
    ) -> SelectedAncestryFact {
        SelectedAncestryFact {
            entry_id: SessionEntryId(id),
            kind: SelectedAncestryKind::Context {
                entry,
                normal_attempts,
                read_paths: Vec::new(),
                modified_paths: Vec::new(),
            },
        }
    }

    fn user(id: u64, text: &str) -> SelectedAncestryFact {
        context(
            id,
            ModelContextEntry::UserRequest {
                text: text.to_owned(),
            },
            Vec::new(),
        )
    }

    fn completed_entry(
        provider: &str,
        call_id: &str,
        source: &str,
        commentary: Option<&str>,
        reasoning: &str,
        replay: &[u8],
        feedback: &str,
    ) -> ModelContextEntry {
        let mut history = ModelHistory::new();
        history
            .append_completed_code(
                source.to_owned(),
                ModelCallId::new(call_id),
                commentary.map(str::to_owned),
                reasoning.to_owned(),
                ProviderReplay::new(provider, vec![OpaqueReplayItem::new(replay.to_vec())]),
                CodeResult::Execution {
                    feedback: feedback.to_owned(),
                },
            )
            .unwrap();
        history.entries()[0].clone()
    }

    fn final_entry(
        provider: &str,
        text: &str,
        reasoning: &str,
        replay: &[u8],
    ) -> ModelContextEntry {
        ModelContextEntry::FinalMessage {
            text: text.to_owned(),
            reasoning_summary: reasoning.to_owned(),
            replay: ProviderReplay::new(provider, vec![OpaqueReplayItem::new(replay.to_vec())]),
        }
    }

    fn code(
        id: u64,
        source: &str,
        commentary: Option<&str>,
        reasoning: &str,
        replay: &[u8],
        feedback: &str,
        normal_attempts: Vec<AttemptAccounting>,
    ) -> SelectedAncestryFact {
        context(
            id,
            completed_entry(
                NATIVE,
                &format!("call-{id}"),
                source,
                commentary,
                reasoning,
                replay,
                feedback,
            ),
            normal_attempts,
        )
    }

    fn final_message(
        id: u64,
        text: &str,
        reasoning: &str,
        replay: &[u8],
        normal_attempts: Vec<AttemptAccounting>,
    ) -> SelectedAncestryFact {
        context(
            id,
            final_entry(NATIVE, text, reasoning, replay),
            normal_attempts,
        )
    }

    fn applied(id: u64, summary: &str, first_retained: u64) -> SelectedAncestryFact {
        SelectedAncestryFact {
            entry_id: SessionEntryId(id),
            kind: SelectedAncestryKind::Applied {
                summary: summary.to_owned(),
                first_retained: SessionEntryId(first_retained),
            },
        }
    }

    fn neutral(id: u64) -> SelectedAncestryFact {
        SelectedAncestryFact {
            entry_id: SessionEntryId(id),
            kind: SelectedAncestryKind::ContextNeutral {
                read_paths: Vec::new(),
                modified_paths: Vec::new(),
            },
        }
    }

    fn usage(total: Option<u64>, input: Option<u64>, output: Option<u64>) -> AttemptAccounting {
        model_usage(DEFAULT_MODEL.id, total, input, output)
    }

    fn model_usage(
        model: &str,
        total: Option<u64>,
        input: Option<u64>,
        output: Option<u64>,
    ) -> AttemptAccounting {
        AttemptAccounting::KnownUsage(ModelUsage {
            model: model.to_owned(),
            service_tier: None,
            input_tokens: input,
            cached_input_tokens: Some(999),
            cache_write_input_tokens: Some(999),
            cache_write_1h_input_tokens: 0,
            output_tokens: output,
            reasoning_output_tokens: Some(999),
            total_tokens: total,
        })
    }

    fn prepared(facts: &[SelectedAncestryFact]) -> super::CompactionPreparation {
        prepare(facts, policy(1), selection(), INLINE, "", MANUAL)
            .unwrap()
            .expect("a removable prefix")
    }

    fn measured(facts: &[SelectedAncestryFact]) -> super::CompactionPreparation {
        prepare(facts, policy(0), selection(), INLINE, "", MANUAL)
            .unwrap()
            .expect("a removable prefix")
    }

    #[test]
    fn foreign_history_uses_semantic_bytes_and_drops_reasoning_for_summary() {
        let opaque = vec![b'x'; 100_000];
        let padding = "p".repeat(40);
        for (destination, foreign, call_id, final_reasoning) in [
            (selection(), "anthropic", "toolu_1", "Summarising"),
            (
                anthropic_selection(),
                "openai",
                "call_1",
                "Reading the output",
            ),
        ] {
            for reasoning in [CrossProviderReasoning::Inline, CrossProviderReasoning::Drop] {
                let case = format!("{foreign} history under {reasoning:?}");
                let facts = [
                    user(1, "inspect"),
                    context(
                        2,
                        completed_entry(
                            foreign,
                            call_id,
                            "print(1)",
                            Some("I'll run it."),
                            "Checking the file",
                            &opaque,
                            "1\n",
                        ),
                        Vec::new(),
                    ),
                    context(
                        3,
                        final_entry(foreign, "It printed 1.", final_reasoning, &opaque),
                        Vec::new(),
                    ),
                    user(4, &padding),
                    user(5, "tail"),
                ];
                let inline = reasoning == CrossProviderReasoning::Inline;
                let kept = |text: &str| if inline { text.len() } else { 0 };
                let semantic_bytes = "inspect".len()
                    + kept("Checking the file")
                    + "I'll run it.".len()
                    + call_id.len()
                    + "execute_python".len()
                    + "print(1)".len()
                    + "1\n".len()
                    + kept(final_reasoning)
                    + "It printed 1.".len()
                    + padding.len()
                    + "tail".len();

                let summarised = prepare(&facts, policy(1), destination, reasoning, "", MANUAL)
                    .unwrap()
                    .expect("a removable prefix");
                assert_eq!(
                    summarised.context_tokens(),
                    semantic_bytes.div_ceil(4) as u64,
                    "{case}"
                );
                assert_eq!(summarised.first_retained(), SessionEntryId(5), "{case}");
                let input = &summarised.requests()[0].input;
                assert!(
                    input.contains(
                        "[Assistant action]: I'll run it.\n\nPython:\nprint(1)\n\n[Execution result]: 1\n"
                    ),
                    "{case}"
                );
                assert!(
                    input.contains("[Assistant action]: It printed 1."),
                    "{case}"
                );
                for sentinel in ["Checking the file", final_reasoning] {
                    assert_eq!(
                        input.contains(&format!("[Assistant reasoning]: {sentinel}")),
                        inline,
                        "{case}"
                    );
                    assert_eq!(input.contains(sentinel), inline, "{case}");
                }

                let tail = prepare(&facts, policy(20), destination, reasoning, "", MANUAL)
                    .unwrap()
                    .expect("a removable prefix");
                assert_eq!(tail.first_retained(), SessionEntryId(2), "{case}");
                assert_eq!(tail.retained_entries()[0], facts_entry(&facts[1]), "{case}");
            }
        }
    }

    fn facts_entry(fact: &SelectedAncestryFact) -> ModelContextEntry {
        match &fact.kind {
            SelectedAncestryKind::Context { entry, .. } => entry.clone(),
            _ => panic!("fact is not context-bearing"),
        }
    }

    #[test]
    fn native_history_counts_opaque_replay_and_keeps_reasoning_under_drop() {
        for (destination, native) in [
            (selection(), "openai"),
            (anthropic_selection(), "anthropic"),
        ] {
            let facts = [
                user(1, "inspect"),
                context(
                    2,
                    completed_entry(
                        native,
                        "call_1",
                        "print(1)",
                        Some("I'll run it."),
                        "Checking the file",
                        b"opaque code replay",
                        "1\n",
                    ),
                    Vec::new(),
                ),
                context(
                    3,
                    final_entry(native, "It printed 1.", "Summarising", b"opaque final"),
                    Vec::new(),
                ),
                user(4, "tail"),
            ];
            let bytes = "inspect".len()
                + "opaque code replay".len()
                + "1\n".len()
                + "opaque final".len()
                + "tail".len();

            let preparation = prepare(
                &facts,
                policy(1),
                destination,
                CrossProviderReasoning::Drop,
                "",
                MANUAL,
            )
            .unwrap()
            .expect("a removable prefix");

            assert_eq!(
                preparation.context_tokens(),
                bytes.div_ceil(4) as u64,
                "{native}"
            );
            let input = &preparation.requests()[0].input;
            assert!(
                input.contains("[Assistant reasoning]: Checking the file"),
                "{native}"
            );
            assert!(
                input.contains("[Assistant reasoning]: Summarising"),
                "{native}"
            );
        }
    }

    #[test]
    fn usage_baselines_require_the_selected_model_and_native_projection() {
        let own = |total| usage(Some(total), None, None);
        let opaque = vec![b'x'; 100_000];

        let newer_same_provider_model = [
            user(1, "abcd"),
            final_message(2, "old", "", b"abcd", vec![own(900)]),
            final_message(
                3,
                "new",
                "",
                b"abcd",
                vec![model_usage(TERRA.id, Some(700), None, None)],
            ),
            user(4, "abcd"),
        ];
        assert_eq!(measured(&newer_same_provider_model).context_tokens(), 4);

        let newer_foreign_model = [
            user(1, "abcd"),
            final_message(2, "old", "", b"abcd", vec![own(900)]),
            context(
                3,
                final_entry("anthropic", "abcd", "", &opaque),
                vec![model_usage(OPUS_5_5.id, Some(700), None, None)],
            ),
            user(4, "abcd"),
        ];
        assert_eq!(measured(&newer_foreign_model).context_tokens(), 4);

        for (reasoning, expected) in [(INLINE, 5), (CrossProviderReasoning::Drop, 4)] {
            let own_after_foreign = [
                user(1, "abcd"),
                context(
                    2,
                    final_entry("anthropic", "abcd", "wxyz", &opaque),
                    Vec::new(),
                ),
                final_message(3, "new", "", b"abcd", vec![own(900)]),
                user(4, "abcd"),
            ];
            let preparation = prepare(
                &own_after_foreign,
                policy(0),
                selection(),
                reasoning,
                "",
                MANUAL,
            )
            .unwrap()
            .expect("a removable prefix");
            assert_eq!(preparation.context_tokens(), expected, "{reasoning:?}");
        }

        let own_after_applied = [
            user(1, "boundary"),
            final_message(
                2,
                "old",
                "",
                b"",
                vec![model_usage(TERRA.id, Some(999), None, None)],
            ),
            applied(3, "prior", 1),
            final_message(4, "new", "", b"", vec![own(10)]),
            user(5, "abcd"),
        ];
        assert_eq!(measured(&own_after_applied).context_tokens(), 11);

        let other_after_applied = [
            user(1, "abcd"),
            final_message(2, "old", "", b"abcd", vec![own(999)]),
            applied(3, "s", 1),
            final_message(
                4,
                "new",
                "",
                b"abcd",
                vec![model_usage(TERRA.id, Some(10), None, None)],
            ),
            user(5, "abcd"),
        ];
        let summary = crate::model::canonical_compaction_summary("s").len();
        assert_eq!(
            measured(&other_after_applied).context_tokens(),
            (summary + 16).div_ceil(4) as u64
        );
    }

    fn measured_at(tokens: u64) -> [SelectedAncestryFact; 3] {
        [
            user(1, "old"),
            final_message(2, "answer", "", b"", vec![usage(Some(tokens), None, None)]),
            user(3, ""),
        ]
    }

    fn prepares(
        facts: &[SelectedAncestryFact],
        policy: crate::config::CompactionSettings,
        reason: CompactionReason,
    ) -> bool {
        prepare(facts, policy, selection(), INLINE, "", reason)
            .unwrap()
            .is_some()
    }

    #[test]
    fn threshold_is_strict_against_the_model_context_window() {
        let model = selection().model();
        let window = model.context_window_tokens;
        assert!(model.pricing.long_context_threshold < window - 100);
        let cases = [
            (model.pricing.long_context_threshold + 1, false),
            (window - 101, false),
            (window - 100, false),
            (window - 99, true),
            (u64::MAX, true),
        ];
        for (tokens, expected) in cases {
            let facts = measured_at(tokens);
            assert_eq!(measured(&facts).context_tokens(), tokens, "{tokens} tokens");
            assert_eq!(
                prepares(&facts, policy(0), THRESHOLD),
                expected,
                "{tokens} tokens"
            );
            assert!(prepares(&facts, policy(0), MANUAL), "{tokens} tokens");
        }
    }

    #[test]
    fn activated_skill_text_can_trigger_automatic_compaction() {
        let active = SkillActivation::new(
            "review".to_owned(),
            PathBuf::from("/home/review"),
            "x".repeat(1_000),
            SkillOrigin::User,
            None,
        )
        .unwrap();
        let without = [user(1, "old"), user(3, "new")];
        let facts = [
            user(1, "old"),
            SelectedAncestryFact {
                entry_id: SessionEntryId(2),
                kind: SelectedAncestryKind::SkillActivation {
                    activation: active.clone(),
                },
            },
            user(3, "new"),
        ];
        let window = selection().model().context_window_tokens;
        let policy = crate::config::CompactionSettings::new(window - 100, 1);
        assert!(!prepares(&without, policy, THRESHOLD));
        let preparation = prepare(&facts, policy, selection(), INLINE, "", THRESHOLD)
            .unwrap()
            .expect("activated skill exceeds the automatic threshold");
        assert!(preparation.context_tokens() > 100);
        assert_eq!(preparation.protected_skills(), &[active]);
    }

    #[test]
    fn threshold_saturates_when_reserve_reaches_the_window() {
        let window = selection().model().context_window_tokens;
        let empty = [user(1, ""), user(2, "")];
        let one_token = [user(1, "a"), user(2, "")];
        for reserve in [window, window + 1, u64::MAX] {
            let policy = crate::config::CompactionSettings::new(reserve, 0);
            assert!(!prepares(&empty, policy, THRESHOLD), "{reserve}");
            assert!(prepares(&one_token, policy, THRESHOLD), "{reserve}");
            assert!(prepares(&empty, policy, MANUAL), "{reserve}");
        }
        assert!(!prepares(&empty, policy(0), THRESHOLD));

        let cases = [
            (0, 0, 0, false),
            (1, 0, 0, true),
            (0, 0, u64::MAX, false),
            (u64::MAX, u64::MAX, 0, false),
            (u64::MAX, u64::MAX, 1, true),
            (0, u64::MAX, u64::MAX, false),
            (1, u64::MAX, u64::MAX, true),
        ];
        for (context, window, reserve, expected) in cases {
            assert_eq!(
                exceeds_threshold(context, window, reserve),
                expected,
                "{context} > {window} - {reserve}"
            );
        }
    }

    #[test]
    fn disabled_policy_skips_automatic_reasons_but_not_manual() {
        let above = measured_at(u64::MAX);
        let below = measured_at(1);
        let disabled = policy(0).disabled();
        assert!(!prepares(&above, disabled, THRESHOLD));
        assert!(!prepares(&above, disabled, OVERFLOW));
        assert!(prepares(&above, disabled, MANUAL));
        assert!(prepares(&below, disabled, MANUAL));

        assert!(prepares(&below, policy(0), OVERFLOW));
        assert!(!prepares(&below, policy(0), THRESHOLD));

        let no_progress = [
            user(1, "boundary"),
            applied(2, "prior", 1),
            final_message(3, "", "", b"", vec![usage(Some(u64::MAX), None, None)]),
        ];
        for reason in [MANUAL, THRESHOLD, OVERFLOW] {
            assert!(!prepares(&no_progress, policy(1), reason), "{reason:?}");
        }
    }

    #[test]
    fn below_threshold_returns_before_summary_assembly() {
        let facts = [
            user(1, "old"),
            applied(2, "inline <read-files>", 1),
            user(3, "tail"),
        ];
        assert!(matches!(
            prepare(&facts, policy(1), selection(), INLINE, "", MANUAL),
            Err(CompactionError::MalformedReservedTag(_))
        ));
        assert_eq!(
            prepare(&facts, policy(1), selection(), INLINE, "", THRESHOLD),
            Ok(None)
        );
        assert!(matches!(
            prepare(
                &facts,
                crate::config::CompactionSettings::new(u64::MAX, 1),
                selection(),
                INLINE,
                "",
                THRESHOLD,
            ),
            Err(CompactionError::MalformedReservedTag(_))
        ));
    }

    #[test]
    fn repeated_compaction_retains_exact_tail_from_previous_boundary() {
        let facts = [
            user(1, "first retained by the previous compaction"),
            applied(2, "previous summary", 1),
            user(3, "newly discardable"),
            user(4, "tail"),
        ];

        let preparation = prepare(&facts, policy(1), selection(), INLINE, "", MANUAL)
            .unwrap()
            .expect("new context after the previous boundary is removable");

        assert_eq!(preparation.first_retained(), SessionEntryId(4));
    }

    #[test]
    fn usage_baselines_choose_newest_eligible_normal_attempt() {
        let cases = [
            (
                "total takes precedence over input and output",
                usage(Some(9), Some(30), Some(40)),
                9,
            ),
            (
                "input and output are the fallback",
                usage(None, Some(3), Some(4)),
                7,
            ),
            (
                "zero total falls back to input and output",
                usage(Some(0), Some(3), Some(4)),
                7,
            ),
        ];
        for (name, attempt, expected) in cases {
            let facts = [
                user(1, "old"),
                code(2, "ignored", None, "", b"", "", vec![attempt]),
                user(3, ""),
            ];
            assert_eq!(measured(&facts).context_tokens(), expected, "{name}");
        }

        let facts = [
            user(1, "old"),
            code(
                2,
                "old",
                None,
                "",
                b"",
                "",
                vec![usage(Some(70), None, None)],
            ),
            final_message(3, "new", "", b"", vec![usage(Some(11), None, None)]),
            user(4, ""),
        ];
        assert_eq!(measured(&facts).context_tokens(), 11);

        let facts = [
            user(1, "old"),
            final_message(
                2,
                "answer",
                "",
                b"",
                vec![usage(Some(u64::MAX), None, None)],
            ),
            user(3, "later"),
        ];
        assert_eq!(measured(&facts).context_tokens(), u64::MAX);
    }

    #[test]
    fn suppressed_latest_project_omits_old_user_and_keeps_independent_user() {
        let replaced_user = SkillActivation::new(
            "review".to_owned(),
            PathBuf::from("/home/review"),
            "x".repeat(60 * 1024),
            SkillOrigin::User,
            None,
        )
        .unwrap();
        let independent = SkillActivation::new(
            "notes".to_owned(),
            PathBuf::from("/home/notes"),
            "independent".to_owned(),
            SkillOrigin::User,
            None,
        )
        .unwrap();
        let facts = [
            user(1, "old"),
            SelectedAncestryFact {
                entry_id: SessionEntryId(2),
                kind: SelectedAncestryKind::SkillActivation {
                    activation: replaced_user,
                },
            },
            SelectedAncestryFact {
                entry_id: SessionEntryId(3),
                kind: SelectedAncestryKind::SkillActivation {
                    activation: independent.clone(),
                },
            },
            SelectedAncestryFact {
                entry_id: SessionEntryId(4),
                kind: SelectedAncestryKind::SuppressedSkillActivation {
                    name: "review".to_owned(),
                },
            },
            code(
                5,
                "pass",
                None,
                "",
                b"",
                "",
                vec![usage(Some(100), None, None)],
            ),
            user(6, "tail"),
        ];
        let preparation = measured(&facts);
        assert_eq!(preparation.context_tokens(), 101);
        assert_eq!(preparation.protected_skills(), &[independent]);
    }

    #[test]
    fn newest_post_filter_baseline_wins() {
        let active = SkillActivation::new(
            "review".to_owned(),
            PathBuf::from("/home/review"),
            "x".repeat(1_000),
            SkillOrigin::User,
            None,
        )
        .unwrap();
        let facts = [
            user(1, "old"),
            SelectedAncestryFact {
                entry_id: SessionEntryId(2),
                kind: SelectedAncestryKind::SkillActivation { activation: active },
            },
            SelectedAncestryFact {
                entry_id: SessionEntryId(3),
                kind: SelectedAncestryKind::SuppressedSkillActivation {
                    name: "review".to_owned(),
                },
            },
            code(
                4,
                "old",
                None,
                "",
                b"",
                "",
                vec![usage(Some(100), None, None)],
            ),
            code(
                5,
                "new",
                None,
                "",
                b"",
                "",
                vec![usage(Some(500), None, None)],
            ),
            user(6, "tail"),
        ];
        assert_eq!(measured(&facts).context_tokens(), 501);
    }

    #[test]
    fn project_skill_active_before_baseline_is_not_charged_again() {
        let active = SkillActivation::new(
            "review".to_owned(),
            PathBuf::from("/project/.agents/skills/review"),
            "x".repeat(1_000),
            SkillOrigin::Project,
            Some(PathBuf::from("/project")),
        )
        .unwrap();
        let facts = [
            user(1, "old"),
            SelectedAncestryFact {
                entry_id: SessionEntryId(2),
                kind: SelectedAncestryKind::SkillActivation { activation: active },
            },
            code(
                3,
                "pass",
                None,
                "",
                b"",
                "",
                vec![usage(Some(100), None, None)],
            ),
            user(4, "tail"),
        ];
        assert_eq!(measured(&facts).context_tokens(), 101);
    }

    #[test]
    fn usage_baselines_exclude_retained_ancestors_and_terminal_only_facts() {
        let facts = [
            user(1, "boundary"),
            code(
                2,
                "old",
                None,
                "",
                b"",
                "",
                vec![usage(Some(999), None, None)],
            ),
            applied(3, "prior", 1),
            code(
                4,
                "new",
                None,
                "",
                b"",
                "",
                vec![usage(Some(10), None, None)],
            ),
            neutral(5),
            user(6, ""),
        ];
        assert_eq!(measured(&facts).context_tokens(), 10);

        let facts = [user(1, "abcd"), neutral(2), user(3, "")];
        assert_eq!(measured(&facts).context_tokens(), 1);

        let facts = [
            user(1, "abcd"),
            context(
                2,
                ModelContextEntry::UserRequest {
                    text: String::new(),
                },
                vec![usage(Some(999), None, None)],
            ),
            user(3, ""),
        ];
        assert_eq!(measured(&facts).context_tokens(), 1);
    }

    #[test]
    fn usage_baselines_accept_untracked_retries_and_skip_unusable_attempts() {
        let retried = AttemptAccounting::KnownUsageWithUntrackedRetries(ModelUsage {
            model: DEFAULT_MODEL.id.to_owned(),
            service_tier: None,
            input_tokens: Some(5),
            cached_input_tokens: Some(50),
            cache_write_input_tokens: Some(50),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(6),
            reasoning_output_tokens: Some(60),
            total_tokens: None,
        });
        let facts = [
            user(1, "old"),
            final_message(2, "text", "", b"", vec![retried]),
            user(3, ""),
        ];
        assert_eq!(measured(&facts).context_tokens(), 11);

        let unusable = [
            AttemptAccounting::UsageUnavailable,
            usage(Some(0), None, None),
            usage(None, Some(u64::MAX), Some(1)),
        ];
        for attempt in unusable {
            let facts = [
                user(1, "abc"),
                final_message(2, "decoded", "ignored", b"wxyz", vec![attempt]),
                user(3, ""),
            ];
            assert_eq!(measured(&facts).context_tokens(), 2);
        }
    }

    #[test]
    fn baseline_adds_only_same_completed_feedback_and_later_projection() {
        let facts = [
            user(1, "old"),
            code(
                2,
                "very long decoded source",
                Some("commentary"),
                "reason",
                b"replay",
                "1234",
                vec![usage(Some(10), None, None)],
            ),
            final_message(3, "ignored decoded text", "ignored", b"abcd", Vec::new()),
            user(4, "tail"),
        ];
        assert_eq!(measured(&facts).context_tokens(), 13);
    }

    #[test]
    fn estimator_uses_each_context_variant_once_with_aggregate_rounding() {
        let summary = ModelContextEntry::CompactionSummary {
            summary: "s".to_owned(),
        };
        let expected =
            ("é".len() + crate::model::canonical_compaction_summary("s").len() + 4 + 1 + 4 + 1)
                .div_ceil(4) as u64;
        let facts = [
            user(1, "é"),
            context(2, summary, Vec::new()),
            code(
                3,
                "decoded",
                Some("commentary"),
                "reason",
                "éé".as_bytes(),
                "x",
                Vec::new(),
            ),
            final_message(4, "decoded", "reason", "éé".as_bytes(), Vec::new()),
            user(5, "x"),
        ];
        assert_eq!(measured(&facts).context_tokens(), expected);
        assert_eq!(estimate_tokens(3 + 1), 1);
        assert_eq!(estimate_tokens(u64::MAX), u64::MAX / 4 + 1);
    }

    #[test]
    fn tail_selection_keeps_whole_entries_and_requires_a_new_cut() {
        let initial = [user(1, "aaaa"), user(2, "bbbb"), user(3, "cccc")];
        let cases = [(1, Some(3)), (2, Some(2)), (3, None)];
        for (keep, expected) in cases {
            let result = prepare(&initial, policy(keep), selection(), INLINE, "", MANUAL).unwrap();
            assert_eq!(
                result.map(|preparation| preparation.first_retained().0),
                expected
            );
        }

        let oversized = [user(1, "aaaa"), user(2, "abcdefgh")];
        assert_eq!(prepared(&oversized).first_retained(), SessionEntryId(2));

        let ignored = [
            user(1, "aaaa"),
            neutral(2),
            applied(3, "prior", 1),
            neutral(4),
            user(5, "bbbb"),
            user(6, "cccc"),
        ];
        assert_eq!(prepared(&ignored).first_retained(), SessionEntryId(6));

        let no_progress = [user(1, "boundary"), applied(2, "prior", 1), neutral(3)];
        assert!(
            prepare(
                &no_progress,
                policy(1),
                selection(),
                INLINE,
                "focus",
                MANUAL
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn invalid_applied_boundaries_are_rejected() {
        let missing = [user(1, "old"), applied(2, "prior", 99), user(3, "tail")];
        assert!(matches!(
            prepare(&missing, policy(1), selection(), INLINE, "", MANUAL),
            Err(CompactionError::InvalidAncestry(_))
        ));

        let neutral_boundary = [
            neutral(1),
            applied(2, "prior", 1),
            user(3, "discard"),
            user(4, "tail"),
        ];
        assert!(matches!(
            prepare(
                &neutral_boundary,
                policy(1),
                selection(),
                INLINE,
                "",
                MANUAL
            ),
            Err(CompactionError::InvalidAncestry(_))
        ));
    }

    #[test]
    fn split_requests_partition_initial_and_repeated_history() {
        let initial = [
            user(1, "request"),
            code(2, "discard", None, "", b"", "", Vec::new()),
            code(3, "tail", None, "", b"", "tail", Vec::new()),
        ];
        let preparation = prepared(&initial);
        assert_eq!(preparation.requests().len(), 1);
        assert_eq!(
            preparation.requests()[0].instructions,
            crate::prompt::compaction_system_prompt()
        );
        assert_eq!(preparation.requests()[0].output_limit, 50);
        assert!(preparation.requests()[0].input.contains("[User]: request"));
        assert!(
            preparation.requests()[0]
                .input
                .contains("## Original Request")
        );
        assert!(!preparation.requests()[0].input.contains("Python:\ntail"));
        assert_eq!(
            preparation.finish(&["turn".to_owned()]).unwrap(),
            format!("No prior history.{SPLIT_SEPARATOR}turn")
        );

        let with_history = [
            user(1, "older request"),
            final_message(2, "older answer", "", b"old", Vec::new()),
            user(3, "current request"),
            code(4, "prefix", None, "", b"", "", Vec::new()),
            code(5, "tail", None, "", b"", "tail", Vec::new()),
        ];
        let preparation = prepared(&with_history);
        assert_eq!(preparation.requests().len(), 2);
        assert!(
            preparation.requests()[0]
                .input
                .contains("[User]: older request")
        );
        assert!(!preparation.requests()[0].input.contains("current request"));
        assert!(
            preparation.requests()[1]
                .input
                .contains("[User]: current request")
        );
        assert!(!preparation.requests()[1].input.contains("older request"));
        assert!(!preparation.requests()[1].input.contains("Python:\ntail"));
        assert_eq!(
            preparation
                .finish(&["history".to_owned(), "prefix".to_owned()])
                .unwrap(),
            format!("history{SPLIT_SEPARATOR}prefix")
        );

        let request_boundary = [
            user(1, "discarded"),
            user(2, "retained"),
            code(3, "tail", None, "", b"", "", Vec::new()),
        ];
        let preparation = prepared(&request_boundary);
        assert_eq!(preparation.requests().len(), 1);
        assert_eq!(preparation.requests()[0].output_limit, 80);
        assert!(
            preparation.requests()[0]
                .input
                .contains("[User]: discarded")
        );
        assert!(
            !preparation.requests()[0]
                .input
                .contains("## Original Request")
        );

        let repeated_same_run = [
            user(1, "older request"),
            code(2, "boundary", None, "", b"", "", Vec::new()),
            applied(3, "prior", 2),
            code(4, "discard", None, "", b"", "", Vec::new()),
            code(5, "tail", None, "", b"", "tail", Vec::new()),
        ];
        let preparation = prepared(&repeated_same_run);
        assert_eq!(preparation.requests().len(), 1);
        assert!(
            !preparation.requests()[0]
                .input
                .contains("## Original Request")
        );
        assert!(
            preparation.requests()[0]
                .input
                .contains("[Assistant action]: Python:\nboundary")
        );
        assert!(
            preparation.requests()[0]
                .input
                .contains("[Assistant action]: Python:\ndiscard")
        );
        assert!(!preparation.requests()[0].input.contains("Python:\ntail"));
    }

    #[test]
    fn prefix_only_split_reuses_or_updates_previous_summary_by_focus() {
        let facts = [
            user(1, "request"),
            applied(2, "previous", 1),
            code(3, "tail", None, "", b"", "tail", Vec::new()),
        ];
        let no_focus = prepare(&facts, policy(1), selection(), INLINE, "  ", MANUAL)
            .unwrap()
            .unwrap();
        assert_eq!(no_focus.requests().len(), 1);
        assert!(no_focus.requests()[0].input.contains("## Original Request"));
        assert_eq!(
            no_focus.finish(&["turn".to_owned()]).unwrap(),
            format!("previous{SPLIT_SEPARATOR}turn")
        );

        let focused = prepare(
            &facts,
            policy(1),
            selection(),
            INLINE,
            "  keep variables  ",
            MANUAL,
        )
        .unwrap()
        .unwrap();
        assert_eq!(focused.requests().len(), 2);
        assert_eq!(
            focused.requests()[0].input,
            format!(
                "<conversation>\n\n</conversation>\n\n<previous-summary>\nprevious\n</previous-summary>\n\n{}\n\nAdditional focus: keep variables",
                crate::prompt::compaction_update_prompt()
            )
        );
        assert!(
            focused.requests()[1]
                .input
                .ends_with("Additional focus: keep variables")
        );
        assert_eq!(
            focused
                .finish(&["updated".to_owned(), "turn".to_owned()])
                .unwrap(),
            format!("updated{SPLIT_SEPARATOR}turn")
        );
    }

    #[test]
    fn serialisation_is_exact_and_feedback_is_unicode_safe() {
        let code = code(
            1,
            "print()",
            Some("commentary"),
            "reason",
            b"",
            "ok",
            Vec::new(),
        );
        let final_message = final_message(2, "", "", b"", Vec::new());
        assert_eq!(
            serialise_entry(
                match &code.kind {
                    SelectedAncestryKind::Context { entry, .. } => entry,
                    _ => unreachable!(),
                },
                native()
            ),
            Some(
                "[Assistant reasoning]: reason\n\n[Assistant action]: commentary\n\nPython:\nprint()\n\n[Execution result]: ok"
                    .to_owned()
            )
        );
        assert_eq!(
            serialise_entry(
                match &final_message.kind {
                    SelectedAncestryKind::Context { entry, .. } => entry,
                    _ => unreachable!(),
                },
                native()
            ),
            Some("[Assistant action]: ".to_owned())
        );
        assert_eq!(
            serialise_entry(
                &ModelContextEntry::UserRequest {
                    text: String::new(),
                },
                native()
            ),
            Some("[User]: ".to_owned())
        );
        for result in [
            CodeResult::Execution {
                feedback: String::new(),
            },
            CodeResult::Cancelled {
                feedback: String::new(),
            },
            CodeResult::Failure {
                feedback: String::new(),
            },
        ] {
            let mut history = ModelHistory::new();
            history
                .append_completed_code(
                    String::new(),
                    ModelCallId::new("empty"),
                    Some(String::new()),
                    String::new(),
                    ProviderReplay::new(NATIVE, Vec::new()),
                    result,
                )
                .unwrap();
            assert_eq!(
                serialise_entry(&history.entries()[0], native()),
                Some("[Assistant action]: Python:\n\n\n[Execution result]: ".to_owned())
            );
        }
        assert_eq!(
            truncate_feedback(&"é".repeat(2_001)),
            format!("{}\n\n[... 1 more characters truncated]", "é".repeat(2_000))
        );
    }

    #[test]
    fn reserved_tags_are_stripped_only_as_complete_line_blocks() {
        assert_eq!(
            strip_reserved_blocks("before\n<read-files>\n\"old\"\n</read-files>\nafter"),
            Ok("before\nafter".to_owned())
        );
        assert_eq!(
            strip_reserved_blocks("before\n<read-files>\n\"old\"\n</read-files>"),
            Ok("before\n".to_owned())
        );
        for malformed in [
            "<read-files>\r\n</read-files>\r\n",
            "text <read-files>",
            "<read-files>\n<modified-files>\n</modified-files>\n</read-files>\n",
            "</read-files>\n",
            "<read-files>\n</modified-files>\n",
            "<read-files>\n",
        ] {
            assert!(strip_reserved_blocks(malformed).is_err(), "{malformed:?}");
        }

        let facts = [
            user(1, "old"),
            applied(2, "kept\n<read-files>\n\"old\"\n</read-files>\n", 1),
            user(3, "tail"),
        ];
        let preparation = prepared(&facts);
        assert!(
            preparation.requests()[0]
                .input
                .contains("<previous-summary>\nkept\n\n</previous-summary>")
        );
        let summary = preparation
            .finish(&["new\n<modified-files>\n\"stale\"\n</modified-files>\n".to_owned()])
            .unwrap();
        assert_eq!(summary, "new\n");
        assert!(matches!(
            prepared(&[user(1, "old"), user(2, "tail")])
                .finish(&["inline <read-files>".to_owned()]),
            Err(CompactionError::MalformedReservedTag(_))
        ));
    }

    #[test]
    fn file_sections_are_cumulative_canonical_and_delayed_until_the_tail_is_discarded() {
        let mut first = [user(1, "old"), user(2, "tail")];
        set_paths(&mut first[0], &["a/b", "read-only"], &["a/b", "modified"]);
        set_paths(&mut first[1], &["later"], &[]);
        let first_summary = prepared(&first).finish(&["summary".to_owned()]).unwrap();
        assert_eq!(
            first_summary,
            "summary\n\n<read-files>\n\"read-only\"\n</read-files>\n\n<modified-files>\n\"a/b\"\n\"modified\"\n</modified-files>"
        );

        let mut repeated = [
            user(1, "old"),
            user(2, "previous tail"),
            applied(3, &first_summary, 2),
            user(4, "newly discarded"),
            user(5, "new tail"),
        ];
        set_paths(
            &mut repeated[0],
            &["a/b", "read-only"],
            &["a/b", "modified"],
        );
        set_paths(&mut repeated[1], &["later"], &[]);
        let summary = prepared(&repeated)
            .finish(&["fresh\n<read-files>\n\"stale\"\n</read-files>\n<read-files>\n\"duplicate\"\n</read-files>\n<modified-files>\n\"conflict\"\n</modified-files>\n".to_owned()])
            .unwrap();
        assert_eq!(
            summary,
            "fresh\n\n\n<read-files>\n\"later\"\n\"read-only\"\n</read-files>\n\n<modified-files>\n\"a/b\"\n\"modified\"\n</modified-files>"
        );
    }

    #[test]
    fn finish_requires_exact_outputs_and_omits_empty_file_sections() {
        let preparation = prepared(&[user(1, "old"), user(2, "tail")]);
        assert_eq!(
            preparation.finish(&[]),
            Err(CompactionError::UnexpectedOutputCount {
                expected: 1,
                actual: 0,
            })
        );
        assert_eq!(preparation.finish(&[String::new()]).unwrap(), "");
    }

    #[test]
    fn file_sections_escape_json_and_include_context_neutral_evidence() {
        let mut facts = [user(1, "old"), neutral(2), user(3, "tail")];
        set_paths(&mut facts[0], &["a/b", "a/b"], &[]);
        set_paths(
            &mut facts[1],
            &["line\nbreak", "<read-files>", "quote\"slash\\"],
            &["a/b"],
        );
        let summary = prepared(&facts).finish(&["summary".to_owned()]).unwrap();
        assert_eq!(
            summary,
            "summary\n\n<read-files>\n\"\\u003cread-files\\u003e\"\n\"line\\nbreak\"\n\"quote\\\"slash\\\\\"\n</read-files>\n\n<modified-files>\n\"a/b\"\n</modified-files>"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_file_paths_fail_cleanly() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut facts = [user(1, "old"), user(2, "tail")];
        set_paths(
            &mut facts[0],
            &[PathBuf::from(OsString::from_vec(vec![0xff]))],
            &[],
        );
        assert_eq!(
            prepare(&facts, policy(1), selection(), INLINE, "", MANUAL),
            Err(CompactionError::NonUtf8Path)
        );
    }

    #[test]
    fn output_budgets_saturate_floor_clamp_and_cap() {
        let cases = [
            (1, 80, 99, 1),
            (9, 50, 99, 4),
            (u64::MAX, 80, u64::MAX, u64::MAX / 100),
            (100, 80, 12, 12),
            (1, 80, 0, 0),
        ];
        for (reserve, ratio, cap, expected) in cases {
            assert_eq!(output_budget(reserve, ratio, cap), expected);
        }
    }

    fn set_paths<T: AsRef<std::path::Path>>(
        fact: &mut SelectedAncestryFact,
        reads: &[T],
        modifications: &[T],
    ) {
        let (read_paths, modified_paths) = match &mut fact.kind {
            SelectedAncestryKind::Context {
                read_paths,
                modified_paths,
                ..
            }
            | SelectedAncestryKind::ContextNeutral {
                read_paths,
                modified_paths,
            } => (read_paths, modified_paths),
            SelectedAncestryKind::Applied { .. }
            | SelectedAncestryKind::SkillActivation { .. }
            | SelectedAncestryKind::SuppressedSkillActivation { .. } => {
                panic!("fact has no file paths")
            }
        };
        *read_paths = reads
            .iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect();
        *modified_paths = modifications
            .iter()
            .map(|path| path.as_ref().to_path_buf())
            .collect();
    }
}
