use std::collections::BTreeMap;

use ratatui::style::{Color, Style};
use ratatui::text::Text;
use tui_markdown::{ImageFallback, Options, StyleSheet, from_str_with_options};

use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
use crate::arena::{Execution, ExecutionOutcome, InterruptedRead, Observation, ReadOutcome};
use crate::config::{ApiKeyPresence, PromptProvenance, StartupSnapshot, SubmissionAvailability};
use crate::model::{AttemptAccounting, ModelProgress};
use crate::pricing::{CostEstimate, RunCostEstimate, estimate_run_cost, estimate_usage};
use crate::runtime::{
    ModelStepId, Record, RunId, RuntimeDisabledReason, RuntimeEvent, StoredRecord, TerminalDetails,
};

#[derive(Clone, Copy)]
struct MarkdownStyleSheet;

impl StyleSheet for MarkdownStyleSheet {
    fn heading(&self, _level: u8) -> Style {
        Style::new().bold()
    }

    fn link(&self) -> Style {
        Style::new().fg(Color::Cyan).underlined()
    }
}

#[allow(dead_code)]
pub(crate) fn markdown(source: &str) -> Text<'_> {
    let options = Options::new(MarkdownStyleSheet).image_fallback(ImageFallback::AltText);
    from_str_with_options(source, &options)
}

const STORE_FALSE_PRIVACY_BOUNDARY: &str = "store: false disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.";

#[derive(Debug, Default, PartialEq, Eq)]
enum RunState {
    #[default]
    Idle,
    Submitting,
    Active(RunId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayedRecord {
    record: StoredRecord,
    progress_gap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LivePhase {
    Generating,
    Executing,
}

impl LivePhase {
    fn label(self) -> &'static str {
        match self {
            Self::Generating => "generating",
            Self::Executing => "executing",
        }
    }
}

fn disabled_status(reason: &RuntimeDisabledReason) -> &'static str {
    match reason {
        RuntimeDisabledReason::OpenAiAuthentication => "OpenAI authentication failed; restart Kiki",
        RuntimeDisabledReason::ArenaCheckpointRecoveryFailed => {
            "Arena recovery failed; restart Kiki"
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveStep {
    run_id: RunId,
    step_id: ModelStepId,
    phase: LivePhase,
    attempt: Option<u32>,
    settled_attempts: Vec<AttemptAccounting>,
    reasoning: String,
    assistant_text: String,
    decoded_python: String,
    stdout: String,
    stdout_truncated: bool,
    progress_gap: bool,
    activity: String,
}

impl LiveStep {
    fn new(run_id: RunId, step_id: ModelStepId, progress_gap: bool) -> Self {
        Self {
            run_id,
            step_id,
            phase: LivePhase::Generating,
            attempt: None,
            settled_attempts: Vec::new(),
            reasoning: String::new(),
            assistant_text: String::new(),
            decoded_python: String::new(),
            stdout: String::new(),
            stdout_truncated: false,
            progress_gap,
            activity: "waiting for model".to_owned(),
        }
    }
}

pub(crate) struct Transcript {
    startup: Option<StartupSnapshot>,
    run_state: RunState,
    disabled_reason: Option<RuntimeDisabledReason>,
    latest_run_id: Option<RunId>,
    settled_run_ids: Vec<RunId>,
    completed_steps: Vec<(RunId, ModelStepId)>,
    pending_run_gap: bool,
    status: String,
    records: Vec<DisplayedRecord>,
    live_step: Option<LiveStep>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            startup: None,
            run_state: RunState::Idle,
            disabled_reason: None,
            latest_run_id: None,
            settled_run_ids: Vec::new(),
            completed_steps: Vec::new(),
            pending_run_gap: false,
            status: "idle".to_owned(),
            records: Vec::new(),
            live_step: None,
        }
    }
}

impl Transcript {
    pub(crate) fn configured(startup: StartupSnapshot) -> Self {
        Self {
            startup: Some(startup),
            ..Self::default()
        }
    }

    pub(crate) fn begin_submission(&mut self) -> bool {
        if self.disabled_reason.is_some() || self.run_state != RunState::Idle {
            return false;
        }
        self.run_state = RunState::Submitting;
        self.set_status("starting run".to_owned())
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.run_state == RunState::Idle
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled_reason.is_some()
    }

    pub(crate) fn status(&self) -> &str {
        &self.status
    }

    pub(crate) fn apply(&mut self, event: RuntimeEvent, gap_before: bool) -> bool {
        match event {
            RuntimeEvent::RunStarted { run_id } => self.start_run(run_id, gap_before),
            RuntimeEvent::ModelStepStarted { run_id, step_id } => self.update_live_step(
                run_id,
                step_id,
                gap_before,
                Some(format!("model step {}", step_id.0)),
                |_| {},
            ),
            RuntimeEvent::ModelProgress {
                run_id,
                step_id,
                progress,
            } => {
                let status = match &progress {
                    ModelProgress::AttemptStarted { attempt } => {
                        Some(format!("model step {} attempt {attempt}", step_id.0))
                    }
                    _ => Some(format!("model step {}", step_id.0)),
                };
                self.update_live_step(run_id, step_id, gap_before, status, |live| match progress {
                    ModelProgress::AttemptStarted { attempt } => {
                        live.phase = LivePhase::Generating;
                        live.attempt = Some(attempt);
                        live.reasoning.clear();
                        live.assistant_text.clear();
                        live.decoded_python.clear();
                        live.activity = format!("model attempt {attempt}");
                    }
                    ModelProgress::ReasoningSummaryChunk { text } => live.reasoning.push_str(&text),
                    ModelProgress::AssistantTextChunk { text } => {
                        live.assistant_text.push_str(&text)
                    }
                    ModelProgress::DecodedPythonChunk { code } => {
                        live.decoded_python.push_str(&code)
                    }
                    ModelProgress::AttemptSettled { accounting, .. } => {
                        live.settled_attempts.push(accounting)
                    }
                })
            }
            RuntimeEvent::ExecutionStarted { run_id, step_id } => self.update_live_step(
                run_id,
                step_id,
                gap_before,
                Some(format!("running code for step {}", step_id.0)),
                |live| {
                    live.phase = LivePhase::Executing;
                    live.activity = "running code".to_owned();
                },
            ),
            RuntimeEvent::ToolStarted {
                run_id,
                step_id,
                path,
            } => self.update_live_step(
                run_id,
                step_id,
                gap_before,
                Some(format!("reading {path}")),
                |live| {
                    live.phase = LivePhase::Executing;
                    live.activity = format!("reading {path}");
                },
            ),
            RuntimeEvent::ToolFinished {
                run_id,
                step_id,
                path,
                outcome,
            } => {
                let activity = match outcome {
                    ReadOutcome::Success { line_count } => {
                        format!("read {path} ({line_count} lines)")
                    }
                    ReadOutcome::Failure { message } => format!("read {path} failed: {message}"),
                };
                self.update_live_step(
                    run_id,
                    step_id,
                    gap_before,
                    Some(activity.clone()),
                    |live| {
                        live.phase = LivePhase::Executing;
                        live.activity = activity;
                    },
                )
            }
            RuntimeEvent::StdoutDelta {
                run_id,
                step_id,
                text,
            } => self.update_live_step(run_id, step_id, gap_before, None, |live| {
                live.phase = LivePhase::Executing;
                live.stdout.push_str(&text);
            }),
            RuntimeEvent::StdoutTruncated { run_id, step_id } => {
                self.update_live_step(run_id, step_id, gap_before, None, |live| {
                    live.phase = LivePhase::Executing;
                    live.stdout_truncated = true;
                })
            }
            RuntimeEvent::RecordAppended {
                record,
                progress_gap,
            } => self.append_record(record, progress_gap),
            RuntimeEvent::RunSettled { run_id } => self.settle_run(run_id),
            RuntimeEvent::SubmissionRejectedBusy { active_run_id }
                if self.disabled_reason.is_none()
                    && (self.run_state == RunState::Submitting
                        || self.run_state == RunState::Active(active_run_id)) =>
            {
                self.latest_run_id = Some(
                    self.latest_run_id
                        .map_or(active_run_id, |latest| latest.max(active_run_id)),
                );
                self.run_state = RunState::Active(active_run_id);
                self.set_status(format!("run {} is busy", active_run_id.0))
            }
            RuntimeEvent::RuntimeDisabled { reason } => self.disable(reason, false),
            RuntimeEvent::SubmissionRejectedDisabled { reason } => self.disable(reason, true),
            RuntimeEvent::SubmissionRejectedBusy { .. } => false,
        }
    }

    pub(crate) fn history(&self) -> String {
        let mut entries = self
            .startup
            .as_ref()
            .map(format_startup_configuration)
            .into_iter()
            .collect::<Vec<_>>();
        let mut attempts_by_run = BTreeMap::<RunId, Vec<&AttemptAccounting>>::new();
        for displayed in &self.records {
            let prior_attempts = attempts_by_run.entry(displayed.record.run_id).or_default();
            entries.push(format_record(displayed, prior_attempts));
            if let Record::CompletedExecution { attempts, .. } = &displayed.record.record {
                prior_attempts.extend(attempts);
            }
        }
        if let Some(live) = &self.live_step {
            let prior_attempts = attempts_by_run
                .get(&live.run_id)
                .map_or(&[][..], Vec::as_slice);
            entries.push(format_live_step(live, prior_attempts));
        }
        entries.join("\n")
    }

    fn disable(&mut self, reason: RuntimeDisabledReason, clear_submitting: bool) -> bool {
        let reason_changed = self.disabled_reason.as_ref() != Some(&reason);
        self.disabled_reason = Some(reason.clone());
        let submitting_cleared = clear_submitting && self.run_state == RunState::Submitting;
        if submitting_cleared {
            self.run_state = RunState::Idle;
        }
        reason_changed | submitting_cleared | self.set_status(disabled_status(&reason).to_owned())
    }

    fn start_run(&mut self, run_id: RunId, gap_before: bool) -> bool {
        if self.disabled_reason.is_some()
            || self.is_terminal_run(run_id)
            || matches!(self.run_state, RunState::Active(_))
            || self
                .latest_run_id
                .is_some_and(|latest_run_id| run_id < latest_run_id)
            || (self.latest_run_id == Some(run_id) && self.run_state != RunState::Submitting)
        {
            return false;
        }
        self.latest_run_id = Some(run_id);
        self.pending_run_gap = gap_before;
        self.live_step = None;
        self.run_state = RunState::Active(run_id);
        self.set_status(format!("run {} started", run_id.0))
    }

    fn update_live_step(
        &mut self,
        run_id: RunId,
        step_id: ModelStepId,
        gap_before: bool,
        status: Option<String>,
        update: impl FnOnce(&mut LiveStep),
    ) -> bool {
        if !self.accepts_live_step(run_id, step_id) {
            return false;
        }

        let mut changed = self.activate_progress_run(run_id);
        if self
            .live_step
            .as_ref()
            .is_none_or(|live| live.run_id != run_id || live.step_id != step_id)
        {
            let progress_gap = std::mem::take(&mut self.pending_run_gap) || gap_before;
            self.live_step = Some(LiveStep::new(run_id, step_id, progress_gap));
            changed = true;
        } else if gap_before
            && !self
                .live_step
                .as_ref()
                .expect("matching live step checked")
                .progress_gap
        {
            self.live_step
                .as_mut()
                .expect("matching live step checked")
                .progress_gap = true;
            changed = true;
        }

        let live = self.live_step.as_mut().expect("live step was created");
        let before = live.clone();
        update(live);
        changed |= *live != before;
        if let Some(status) = status {
            changed |= self.set_status(status);
        }
        changed
    }

    fn accepts_live_step(&self, run_id: RunId, step_id: ModelStepId) -> bool {
        if self.disabled_reason.is_some()
            || self.is_terminal_run(run_id)
            || self
                .latest_run_id
                .is_some_and(|latest_run_id| run_id < latest_run_id)
            || self.step_is_complete(run_id, step_id)
        {
            return false;
        }
        let accepts_run = match self.run_state {
            RunState::Active(active_run_id) => active_run_id == run_id,
            RunState::Submitting => self
                .latest_run_id
                .is_none_or(|latest_run_id| run_id >= latest_run_id),
            RunState::Idle => self
                .latest_run_id
                .is_none_or(|latest_run_id| run_id > latest_run_id),
        };
        if !accepts_run {
            return false;
        }
        self.live_step
            .as_ref()
            .is_none_or(|live| live.run_id != run_id || step_id >= live.step_id)
    }

    fn activate_progress_run(&mut self, run_id: RunId) -> bool {
        if self.disabled_reason.is_some() {
            return false;
        }
        match self.run_state {
            RunState::Active(_) => false,
            RunState::Submitting | RunState::Idle => {
                self.latest_run_id = Some(run_id);
                self.pending_run_gap = false;
                self.run_state = RunState::Active(run_id);
                self.live_step = None;
                self.set_status(format!("run {} started", run_id.0))
            }
        }
    }

    fn append_record(&mut self, record: StoredRecord, progress_gap: bool) -> bool {
        self.latest_run_id = Some(
            self.latest_run_id
                .map_or(record.run_id, |latest| latest.max(record.run_id)),
        );
        match &record.record {
            Record::CompletedExecution { step_id, .. } => {
                self.completed_steps.push((record.run_id, *step_id));
                if self
                    .live_step
                    .as_ref()
                    .is_some_and(|live| live.run_id == record.run_id && live.step_id == *step_id)
                {
                    self.live_step = None;
                }
            }
            Record::FinalMessage { .. }
            | Record::CancelledRun { .. }
            | Record::FailedRun { .. } => {
                self.mark_terminal_run(record.run_id);
            }
            Record::UserRequest { .. } => {}
        }
        self.records.push(DisplayedRecord {
            record,
            progress_gap,
        });
        true
    }

    fn settle_run(&mut self, run_id: RunId) -> bool {
        if self
            .latest_run_id
            .is_some_and(|latest_run_id| run_id < latest_run_id)
            || self.is_terminal_run(run_id) && self.run_state == RunState::Idle
        {
            return false;
        }
        self.latest_run_id = Some(run_id);
        self.mark_terminal_run(run_id);
        let state_changed = self.run_state != RunState::Idle;
        self.run_state = RunState::Idle;
        state_changed | (self.disabled_reason.is_none() && self.set_status("idle".to_owned()))
    }

    fn mark_terminal_run(&mut self, run_id: RunId) {
        if !self.is_terminal_run(run_id) {
            self.settled_run_ids.push(run_id);
        }
        if self
            .live_step
            .as_ref()
            .is_some_and(|live| live.run_id == run_id)
        {
            self.live_step = None;
        }
        if self.latest_run_id == Some(run_id) {
            self.pending_run_gap = false;
        }
    }

    fn is_terminal_run(&self, run_id: RunId) -> bool {
        self.settled_run_ids.contains(&run_id)
    }

    fn step_is_complete(&self, run_id: RunId, step_id: ModelStepId) -> bool {
        self.completed_steps
            .iter()
            .any(|(completed_run_id, completed_step_id)| {
                *completed_run_id == run_id && *completed_step_id >= step_id
            })
    }

    fn set_status(&mut self, status: String) -> bool {
        if self.status == status {
            false
        } else {
            self.status = status;
            true
        }
    }
}

pub(crate) fn format_startup_configuration(snapshot: &StartupSnapshot) -> String {
    let worker_path = snapshot
        .worker_path()
        .map_or("unavailable", |path| path.as_str());
    let workspace_root = snapshot
        .workspace_root()
        .map_or("unavailable", |path| path.as_str());
    let prompt_source = match snapshot.prompt_source() {
        PromptProvenance::Bundled { name } => format!("bundled {name}"),
        PromptProvenance::Override { path } => format!("override {}", path.as_str()),
    };
    let api_key_presence = match snapshot.api_key_presence() {
        ApiKeyPresence::Missing => "missing",
        ApiKeyPresence::Present => "present",
    };
    let submission = match snapshot.submission_availability() {
        SubmissionAvailability::Available => "available",
        SubmissionAvailability::Unavailable => "unavailable",
    };
    let mut lines = vec![
        "Startup configuration".to_owned(),
        format!("Worker: {worker_path}"),
        format!("Workspace: {workspace_root}"),
        format!("Model: {}", snapshot.model()),
        format!("Prompt: {prompt_source}"),
        format!("OpenAI API key: {api_key_presence}"),
        format!("Submission: {submission}"),
    ];
    if snapshot.problems().is_empty() {
        lines.push("Problems: none".to_owned());
    } else {
        for problem in snapshot.problems() {
            lines.push(format!("Problem: {}", problem.diagnostic()));
            lines.push(format!("Remediation: {}", problem.remediation()));
        }
    }
    lines.push(STORE_FALSE_PRIVACY_BOUNDARY.to_owned());
    normalised_lines(lines)
}

fn format_record(displayed: &DisplayedRecord, prior_attempts: &[&AttemptAccounting]) -> String {
    let stored = &displayed.record;
    let run_id = stored.run_id.0;
    match &stored.record {
        Record::UserRequest { request } => {
            let mut lines = vec![format!("[run {run_id}] you: {request}")];
            append_display_gap(&mut lines, displayed.progress_gap);
            normalised_lines(lines)
        }
        Record::CompletedExecution {
            step_id,
            source,
            execution,
            attempts,
            ..
        } => format_execution(
            run_id,
            step_id.0,
            source,
            execution,
            attempts,
            prior_attempts,
            displayed.progress_gap,
        ),
        Record::FinalMessage {
            step_id,
            text,
            terminal,
            ..
        } => format_terminal(
            format!("[run {run_id} step {}] kiki: {text}", step_id.0),
            terminal,
            prior_attempts,
            displayed.progress_gap,
        ),
        Record::CancelledRun { terminal } => format_terminal(
            format!("[run {run_id}] cancelled"),
            terminal,
            prior_attempts,
            displayed.progress_gap,
        ),
        Record::FailedRun { message, terminal } => format_terminal(
            format!("[run {run_id}] failed: {message}"),
            terminal,
            prior_attempts,
            displayed.progress_gap,
        ),
    }
}

fn format_live_step(live: &LiveStep, prior_attempts: &[&AttemptAccounting]) -> String {
    let mut lines = vec![format!(
        "[run {} step {}] live: {}",
        live.run_id.0,
        live.step_id.0,
        live.phase.label()
    )];
    append_display_gap(&mut lines, live.progress_gap);
    if let Some(attempt) = live.attempt {
        lines.push(format!("attempt: {attempt}"));
    }
    if !live.reasoning.is_empty() {
        lines.push(format!("reasoning: {}", live.reasoning));
    }
    if !live.assistant_text.is_empty() {
        lines.push(format!("assistant: {}", live.assistant_text));
    }
    if !live.decoded_python.is_empty() {
        lines.push(format!("code:\n{}", live.decoded_python));
    }
    if !live.stdout.is_empty() {
        lines.push(format!("stdout: {}", live.stdout));
    }
    if live.stdout_truncated {
        lines.push("stdout preview truncated".to_owned());
    }
    if !live.activity.is_empty() {
        lines.push(format!("activity: {}", live.activity));
    }
    append_attempts(&mut lines, &live.settled_attempts);
    lines.push(format_cost(
        "Step estimated cost",
        &live.settled_attempts.iter().collect::<Vec<_>>(),
    ));
    let run_attempts = prior_attempts
        .iter()
        .copied()
        .chain(live.settled_attempts.iter())
        .collect::<Vec<_>>();
    lines.push(format_cost("Run estimated cost", &run_attempts));
    normalised_lines(lines)
}

fn format_execution(
    run_id: u64,
    step_id: u64,
    source: &str,
    execution: &Execution,
    attempts: &[AttemptAccounting],
    prior_attempts: &[&AttemptAccounting],
    progress_gap: bool,
) -> String {
    let mut lines = vec![format!("[run {run_id} step {step_id}] code:\n{source}")];
    append_display_gap(&mut lines, progress_gap);
    append_execution_output(&mut lines, execution);
    append_attempts(&mut lines, attempts);
    lines.push(format_cost(
        "Step estimated cost",
        &attempts.iter().collect::<Vec<_>>(),
    ));
    let run_attempts = prior_attempts
        .iter()
        .copied()
        .chain(attempts.iter())
        .collect::<Vec<_>>();
    lines.push(format_cost("Run estimated cost", &run_attempts));
    normalised_lines(lines)
}

fn append_execution_output(lines: &mut Vec<String>, execution: &Execution) {
    append_observation(lines, "stdout", &execution.stdout);
    append_observation(lines, "stderr", &execution.stderr);
    match &execution.outcome {
        ExecutionOutcome::Complete {
            result: Some(result),
        } => lines.push(format!("result: {}", format_observation(result))),
        ExecutionOutcome::Complete { result: None } => {}
        ExecutionOutcome::PythonError(error) => lines.push(format!("error: {}", error.text)),
    }
}

fn format_terminal(
    header: String,
    terminal: &TerminalDetails,
    prior_attempts: &[&AttemptAccounting],
    progress_gap: bool,
) -> String {
    let mut lines = vec![header];
    append_display_gap(&mut lines, progress_gap);
    append_attempts(&mut lines, &terminal.attempts);
    lines.push(format_cost(
        "Step estimated cost",
        &terminal.attempts.iter().collect::<Vec<_>>(),
    ));
    let run_attempts = prior_attempts
        .iter()
        .copied()
        .chain(terminal.attempts.iter())
        .collect::<Vec<_>>();
    lines.push(format_cost("Run estimated cost", &run_attempts));
    if let Some(evidence) = &terminal.evidence {
        append_terminal_evidence(&mut lines, evidence);
    }
    normalised_lines(lines)
}

fn append_terminal_evidence(lines: &mut Vec<String>, evidence: &TerminalEvidence) {
    match evidence {
        TerminalEvidence::Generation(GenerationEvidence {
            reasoning,
            assistant_text,
            decoded_python,
            ..
        }) => {
            if !reasoning.is_empty() {
                lines.push(format!("reasoning: {reasoning}"));
            }
            if !assistant_text.is_empty() {
                lines.push(format!("assistant: {assistant_text}"));
            }
            if !decoded_python.is_empty() {
                lines.push(format!("code:\n{decoded_python}"));
            }
        }
        TerminalEvidence::Execution(evidence) => append_execution_evidence(lines, evidence),
    }
}

fn append_execution_evidence(lines: &mut Vec<String>, evidence: &ExecutionEvidence) {
    lines.push(format!("source: {}", evidence.source));
    lines.push(if evidence.began {
        "execution: started".to_owned()
    } else {
        "execution: not started".to_owned()
    });
    append_observation(lines, "stdout", &evidence.stdout);
    append_observation(lines, "stderr", &evidence.stderr);
    lines.push(format_interrupted_read(&evidence.read));
    if let Some(failure) = &evidence.failure {
        lines.push(format!("failure: {}", format_execution_failure(failure)));
    }
    if let Some(warning) = &evidence.cancellation_warning {
        lines.push(format!("warning: {warning}"));
    }
}

fn format_interrupted_read(read: &InterruptedRead) -> String {
    match read {
        InterruptedRead::NoRead => "read: none".to_owned(),
        InterruptedRead::Pending { path } => format!("read: pending {path}"),
        InterruptedRead::Completed { path, outcome } => match outcome {
            ReadOutcome::Success { line_count } => {
                format!("read: completed {path} ({line_count} lines)")
            }
            ReadOutcome::Failure { message } => format!("read: completed {path} failed: {message}"),
        },
    }
}

fn format_execution_failure(failure: &ExecutionFailure) -> String {
    match failure {
        ExecutionFailure::Arena { message } => message.clone(),
        ExecutionFailure::RecoveryFailed { original, recovery } => {
            format!("arena recovery failed after {original}: {recovery}")
        }
    }
}

fn append_observation(lines: &mut Vec<String>, label: &str, observation: &Observation) {
    if observation.total_bytes > 0 {
        lines.push(format!("{label}: {}", format_observation(observation)));
    }
}

fn append_display_gap(lines: &mut Vec<String>, progress_gap: bool) {
    if progress_gap {
        lines.push("display gap: some live progress was dropped".to_owned());
    }
}

fn append_attempts(lines: &mut Vec<String>, attempts: &[AttemptAccounting]) {
    for (index, attempt) in attempts.iter().enumerate() {
        match attempt {
            AttemptAccounting::KnownUsage(usage)
            | AttemptAccounting::KnownUsageWithUntrackedRetries(usage) => {
                let priceability = if estimate_usage(usage).is_ok() {
                    "priceable"
                } else {
                    "unpriceable"
                };
                lines.push(format!(
                    "attempt {}: model={} tier={} input={} cached-input={} cache-write-input={} output={} reasoning-output={} total={} ({priceability})",
                    index + 1,
                    usage.model,
                    usage.service_tier.as_deref().unwrap_or("unavailable"),
                    format_usage_value(usage.input_tokens),
                    format_usage_value(usage.cached_input_tokens),
                    format_usage_value(usage.cache_write_input_tokens),
                    format_usage_value(usage.output_tokens),
                    format_usage_value(usage.reasoning_output_tokens),
                    format_usage_value(usage.total_tokens),
                ));
            }
            AttemptAccounting::UsageUnavailable => {
                lines.push(format!(
                    "attempt {}: model=unavailable tier=unavailable input=unavailable cached-input=unavailable cache-write-input=unavailable output=unavailable reasoning-output=unavailable total=unavailable (unpriceable)",
                    index + 1
                ));
            }
        }
    }
}

fn format_usage_value(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

fn format_cost(label: &str, attempts: &[&AttemptAccounting]) -> String {
    match estimate_run_cost(attempts.iter().copied()) {
        RunCostEstimate::Complete(estimate) => format_estimated_cost(label, "Complete", estimate),
        RunCostEstimate::LowerBound(estimate) => {
            format_estimated_cost(label, "LowerBound", estimate)
        }
        RunCostEstimate::Unavailable => format!("{label}: Unavailable (no priceable usage)"),
    }
}

fn format_estimated_cost(label: &str, completeness: &str, estimate: CostEstimate) -> String {
    let pricing = estimate.pricing();
    format!(
        "{label}: {completeness} {} USD ({}/{} pricing {})",
        format_usd(estimate.amount().as_nanos()),
        pricing.model,
        pricing.service_tier,
        pricing.version,
    )
}

fn format_usd(nanodollars: u128) -> String {
    const NANODOLLARS_PER_DOLLAR: u128 = 1_000_000_000;
    let dollars = nanodollars / NANODOLLARS_PER_DOLLAR;
    let fractional = nanodollars % NANODOLLARS_PER_DOLLAR;
    if fractional == 0 {
        return format!("${dollars}");
    }
    let fractional = format!("{fractional:09}");
    format!("${dollars}.{}", fractional.trim_end_matches('0'))
}

fn format_observation(observation: &Observation) -> String {
    let mut text = normalise_terminal_text(&observation.head);
    if let Some(tail) = &observation.tail {
        text.push_str(&format!(
            "\n[{} lines, {} bytes omitted]\n",
            observation.omitted_lines, observation.omitted_bytes
        ));
        text.push_str(&normalise_terminal_text(tail));
    }
    text
}

fn normalised_lines(lines: Vec<String>) -> String {
    lines
        .iter()
        .map(|line| normalise_terminal_text(line))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn normalise_terminal_text(text: &str) -> String {
    let mut normalised = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' if characters.peek() == Some(&'\n') => {
                characters.next();
                normalised.push('\n');
            }
            '\n' => normalised.push('\n'),
            '\t' => normalised.push_str("    "),
            '\r' | '\0'..='\x1f' | '\x7f' | '\u{0080}'..='\u{009f}' => {
                normalised.push_str(&format!("\\x{:02X}", character as u32));
            }
            _ => normalised.push(character),
        }
    }
    normalised
}

#[cfg(test)]
mod tests {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    use super::{Transcript, markdown, normalise_terminal_text};
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, InterruptedRead, Observation, PythonError, ReadOutcome,
    };
    use crate::model::{AttemptAccounting, ModelProgress, ModelUsage};
    use crate::runtime::{
        ModelStepId, Record, RunId, RuntimeDisabledReason, RuntimeEvent, StoredRecord,
        TerminalDetails,
    };

    #[test]
    fn markdown_compatibility() {
        let heading = markdown("# Heading");
        assert_eq!(heading.to_string(), "# Heading");
        assert_eq!(heading.lines[0].style, Style::new().bold());

        let table = markdown("| Name | State |\n| --- | --- |\n| Kiki | ready |").to_string();
        assert!(table.contains("┌"));
        assert!(table.contains("Name"));
        assert!(table.contains("ready"));

        let tasks = markdown("- [ ] pending\n- [x] complete").to_string();
        assert!(tasks.contains("- [ ] pending"));
        assert!(tasks.contains("- [x] complete"));

        assert_eq!(
            markdown("[guide](https://example.com)").to_string(),
            "guide (https://example.com)"
        );
        assert_eq!(markdown("> quoted").to_string(), "> quoted");
        let strikethrough = markdown("~~obsolete~~");
        assert_eq!(strikethrough.to_string(), "obsolete");
        assert!(
            strikethrough.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );

        let fenced = markdown("```rust\nlet value = 1;\n```");
        assert_eq!(fenced.to_string(), "```rust\nlet value = 1;\n```");
        assert_eq!(fenced.lines[1].style, Style::new().white().on_black());

        let html = markdown("inline <em>HTML</em>\n\n<div>\nblock HTML\n</div>").to_string();
        for literal in ["<em>", "</em>", "<div>", "block HTML", "</div>"] {
            assert!(html.contains(literal));
        }

        let images = markdown("![diagram](diagram.png)\n\n![](empty.png)").to_string();
        assert!(images.contains("[img] diagram"));
        assert!(images.contains("[img] empty.png"));

        let styled_link = markdown("[guide](https://example.com)");
        let link = styled_link.lines[0]
            .spans
            .iter()
            .find(|span| span.content == "guide")
            .expect("link label span");
        assert_eq!(link.style, Style::new().fg(Color::Cyan).underlined());
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));

        assert_eq!(
            markdown("unfinished **emphasis").to_string(),
            "unfinished **emphasis"
        );
        assert!(
            markdown("```rust\nlet value = 1;")
                .to_string()
                .contains("let value = 1;")
        );
        assert!(!markdown("| header |\n| --- |").lines.is_empty());

        let source = "# Café 日本 🦀\n\n- [x] **résumé**\n\n[link](https://example.com/🦀)\n\n```text\nolé 世界\n```";
        let complete = markdown(source).to_string();
        let mut final_prefix = None;
        for end in 0..=source.len() {
            if source.is_char_boundary(end) {
                final_prefix = Some(markdown(&source[..end]).to_string());
            }
        }
        assert_eq!(final_prefix, Some(complete));

        let paragraph = Paragraph::new(markdown("café 日本 🦀 wraps across terminal rows"))
            .wrap(Wrap { trim: false });
        let width = 8;
        let measured_rows = paragraph.line_count(width);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, measured_rows as u16 + 1));
        (&paragraph).render(buffer.area, &mut buffer);
        let rendered_rows = buffer
            .content
            .chunks(width as usize)
            .filter(|row| row.iter().any(|cell| cell.symbol() != " "))
            .count();
        assert_eq!(rendered_rows, measured_rows);
    }

    fn observation(text: &str) -> Observation {
        Observation {
            head: text.to_owned(),
            tail: None,
            head_lines: usize::from(!text.is_empty()),
            tail_lines: 0,
            total_lines: usize::from(!text.is_empty()),
            total_bytes: text.len(),
            omitted_lines: 0,
            omitted_bytes: 0,
            head_ends_mid_line: false,
            tail_starts_mid_line: false,
            partial_lines: false,
        }
    }

    fn usage(input: u64, output: u64) -> AttemptAccounting {
        AttemptAccounting::KnownUsage(ModelUsage {
            model: "gpt-5.6-sol".to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(input),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            output_tokens: Some(output),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(input + output),
        })
    }

    fn completed_execution(
        run_id: u64,
        step_id: u64,
        source: &str,
        reasoning_summary: &str,
        attempts: Vec<AttemptAccounting>,
    ) -> RuntimeEvent {
        RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(run_id),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(step_id),
                    source: source.to_owned(),
                    reasoning_summary: reasoning_summary.to_owned(),
                    execution: Execution {
                        stdout: observation("authoritative stdout\\n"),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::Complete {
                            result: Some(observation("42")),
                        },
                    },
                    attempts,
                },
            },
            progress_gap: false,
        }
    }

    fn final_message(
        run_id: u64,
        step_id: u64,
        text: &str,
        attempts: Vec<AttemptAccounting>,
        progress_gap: bool,
    ) -> RuntimeEvent {
        RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(run_id),
                record: Record::FinalMessage {
                    step_id: ModelStepId(step_id),
                    text: text.to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails {
                        evidence: None,
                        attempts,
                    },
                },
            },
            progress_gap,
        }
    }

    #[test]
    fn projection_reliable_records_beat_lossy_progress() {
        let mut transcript = Transcript::default();
        assert!(transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false));
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::DecodedPythonChunk {
                    code: "preview()".to_owned(),
                },
            },
            false,
        ));
        assert!(transcript.history().contains("preview()"));

        assert!(transcript.apply(
            completed_execution(1, 1, "authoritative()", "settled reasoning", Vec::new()),
            false,
        ));

        let history = transcript.history();
        assert!(history.contains("authoritative()"));
        assert!(history.contains("authoritative stdout"));
        assert!(!history.contains("preview()"));
        assert!(!history.contains("settled reasoning"));
    }

    #[test]
    fn projection_stale_progress_is_a_visible_no_op() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(3),
            },
            false,
        );
        let history = transcript.history();
        let status = transcript.status().to_owned();

        assert!(!transcript.apply(
            RuntimeEvent::StdoutDelta {
                run_id: RunId(1),
                step_id: ModelStepId(2),
                text: "stale stdout".to_owned(),
            },
            false,
        ));
        assert!(!transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale step".to_owned(),
                },
            },
            true,
        ));
        assert_eq!(transcript.history(), history);
        assert_eq!(transcript.status(), status);
    }

    #[test]
    fn projection_retry_resets_only_live_generated_content() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AttemptStarted { attempt: 1 },
            },
            true,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::ReasoningSummaryChunk {
                    text: "discarded reasoning".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::DecodedPythonChunk {
                    code: "discarded_code()".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: usage(100, 0),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AttemptStarted { attempt: 2 },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AssistantTextChunk {
                    text: "retained answer".to_owned(),
                },
            },
            false,
        );

        let history = transcript.history();
        assert!(!history.contains("discarded reasoning"));
        assert!(!history.contains("discarded_code()"));
        assert!(history.contains("retained answer"));
        assert!(history.contains("attempt 1: model=gpt-5.6-sol"));
        assert!(history.contains("display gap"));
    }

    #[test]
    fn projection_keeps_completed_summary_after_replacing_live_content() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::ReasoningSummaryChunk {
                    text: "partial summary".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            completed_execution(1, 1, "done()", "completed summary", Vec::new()),
            false,
        );

        let summary =
            transcript
                .records
                .iter()
                .find_map(|displayed| match &displayed.record.record {
                    Record::CompletedExecution {
                        reasoning_summary, ..
                    } => Some(reasoning_summary.as_str()),
                    _ => None,
                });
        assert_eq!(summary, Some("completed summary"));
        assert!(!transcript.history().contains("completed summary"));
    }

    #[test]
    fn projection_keeps_old_reliable_runs_visible() {
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(1, 1, "older result", Vec::new(), false),
            false,
        );
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(2),
                progress: ModelProgress::AssistantTextChunk {
                    text: "new live result".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            final_message(1, 1, "late old result", Vec::new(), false),
            false,
        );

        let history = transcript.history();
        assert!(history.contains("older result"));
        assert!(history.contains("new live result"));
        assert!(history.contains("late old result"));
    }

    #[test]
    fn projection_settles_gaps_with_their_live_or_reliable_content() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, true);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            },
            false,
        );
        assert!(transcript.history().contains("display gap"));

        transcript.apply(completed_execution(1, 1, "done()", "", Vec::new()), false);
        assert!(!transcript.history().contains("display gap"));

        transcript.apply(final_message(1, 2, "settled", Vec::new(), true), false);
        transcript.apply(RuntimeEvent::RunSettled { run_id: RunId(1) }, false);
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(3),
            },
            false,
        );

        let history = transcript.history();
        assert!(history.contains("[run 1 step 2] kiki: settled\ndisplay gap"));
        assert!(
            !history
                .rsplit("[run 2 step 3] live")
                .next()
                .expect("live step")
                .contains("display gap")
        );
    }

    #[test]
    fn projection_folds_disjoint_attempts_forward_by_run() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "first()", "", vec![usage(100, 0)]),
            false,
        );
        transcript.apply(completed_execution(2, 2, "other()", "", Vec::new()), false);
        transcript.apply(
            final_message(1, 3, "done", vec![usage(0, 100)], false),
            false,
        );

        let history = transcript.history();
        assert_eq!(history.matches("attempt 1: model=gpt-5.6-sol").count(), 2);
        assert!(history.contains("Run estimated cost: Complete $0.0024 USD"));
        assert!(history.contains("[run 2 step 2] code:"));
    }

    #[test]
    fn projection_projects_disabled_status_and_rejects_late_progress() {
        let mut transcript = Transcript::default();
        transcript.begin_submission();
        assert!(transcript.apply(
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::OpenAiAuthentication,
            },
            false,
        ));
        assert!(transcript.is_disabled());
        assert!(transcript.is_idle());
        assert_eq!(
            transcript.status(),
            "OpenAI authentication failed; restart Kiki"
        );
        assert!(!transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false));
    }

    #[test]
    fn projection_formats_execution_and_interrupted_evidence() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        source: "print('hello')\n2".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("hello\n"),
                            stderr: observation("warning\n"),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: "boom".to_owned(),
                                omitted_bytes: 0,
                            }),
                        },
                        attempts: Vec::new(),
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Execution(Box::new(
                                ExecutionEvidence {
                                    source: "await read(path)".to_owned(),
                                    reasoning_summary: String::new(),
                                    began: true,
                                    stdout: observation("before\n"),
                                    stderr: observation("diagnostic\n"),
                                    read: InterruptedRead::Completed {
                                        path: "bad\tpath".to_owned(),
                                        outcome: ReadOutcome::Failure {
                                            message: "read failed".to_owned(),
                                        },
                                    },
                                    failure: Some(ExecutionFailure::Arena {
                                        message: "interrupted".to_owned(),
                                    }),
                                    cancellation_warning: Some("cancelled warning".to_owned()),
                                },
                            ))),
                            attempts: Vec::new(),
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(3),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Generation(GenerationEvidence {
                                reasoning_summary: String::new(),
                                reasoning: "interrupted reasoning".to_owned(),
                                assistant_text: "interrupted assistant".to_owned(),
                                decoded_python: "interrupted_code()".to_owned(),
                            })),
                            attempts: Vec::new(),
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let history = transcript.history();
        for text in [
            "stdout: hello",
            "stderr: warning",
            "error: boom",
            "source: await read(path)",
            "stderr: diagnostic",
            "read: completed bad    path failed: read failed",
            "failure: interrupted",
            "warning: cancelled warning",
            "reasoning: interrupted reasoning",
            "assistant: interrupted assistant",
            "code:\ninterrupted_code()",
        ] {
            assert!(history.contains(text), "{history}");
        }
    }

    #[test]
    fn projection_normalises_terminal_controls() {
        let c0 = (0_u8..=0x1f).map(char::from).collect::<String>();
        let c1 = (0x80_u8..=0x9f).map(char::from).collect::<String>();
        let input =
            format!("lf\ncrlf\r\ntab\tlone\r{c0}{c1}\x7f\x1b]title\x07\u{009d}wide\u{009c}é🦀");
        let expected_c0 = (0_u8..=0x1f)
            .map(|value| match value {
                b'\n' => "\n".to_owned(),
                b'\t' => "    ".to_owned(),
                _ => format!("\\x{value:02X}"),
            })
            .collect::<String>();
        let expected_c1 = (0x80_u8..=0x9f)
            .map(|value| format!("\\x{value:02X}"))
            .collect::<String>();
        let expected = format!(
            "lf\ncrlf\ntab    lone\\x0D{expected_c0}{expected_c1}\\x7F\\x1B]title\\x07\\x9Dwide\\x9Cé🦀"
        );

        assert_eq!(normalise_terminal_text(&input), expected);
    }

    #[test]
    fn projection_updates_status_through_live_and_settlement_events() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(3) }, false);
        assert_eq!(transcript.status(), "run 3 started");

        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(3),
                step_id: ModelStepId(8),
            },
            false,
        );
        assert_eq!(transcript.status(), "model step 8");
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(3),
                step_id: ModelStepId(8),
            },
            false,
        );
        assert_eq!(transcript.status(), "running code for step 8");
        transcript.apply(
            RuntimeEvent::ToolStarted {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                path: "Cargo.toml".to_owned(),
            },
            false,
        );
        assert_eq!(transcript.status(), "reading Cargo.toml");
        transcript.apply(
            RuntimeEvent::ToolFinished {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                path: "Cargo.toml".to_owned(),
                outcome: ReadOutcome::Success { line_count: 12 },
            },
            false,
        );
        assert_eq!(transcript.status(), "read Cargo.toml (12 lines)");
        transcript.apply(
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: RunId(3),
            },
            false,
        );
        assert_eq!(transcript.status(), "run 3 is busy");

        assert!(transcript.apply(RuntimeEvent::RunSettled { run_id: RunId(3) }, false));
        assert!(transcript.is_idle());
        assert_eq!(transcript.status(), "idle");
    }
}
