use std::io;
use std::time::Duration;

use ratatui::backend::Backend;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
use crate::arena::{Execution, ExecutionOutcome, InterruptedRead, Observation, ReadOutcome};
use crate::config::{ApiKeyPresence, PromptProvenance, StartupSnapshot, SubmissionAvailability};
use crate::model::{AttemptAccounting, ModelProgress};
use crate::pricing::{CostEstimate, RunCostEstimate, estimate_run_cost, estimate_usage};
use crate::runtime::{
    ModelStepId, ProgressEnvelope, Record, RunId, RuntimeCommand, RuntimeDisabledReason,
    RuntimeEvent, RuntimeProgressReceiver, StoredRecord, TerminalDetails,
};

const MAX_PROMPT_BYTES: usize = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STORE_FALSE_PRIVACY_BOUNDARY: &str = "store: false disables retained Response objects but does not prevent user requests or workspace content printed by Python from being sent to OpenAI.";

trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        ratatui::crossterm::event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        ratatui::crossterm::event::read()
    }
}

#[derive(Debug, Error)]
pub(crate) enum TuiError {
    #[error("terminal operation failed: {0}")]
    Terminal(String),
    #[error("terminal input failed")]
    Input(#[source] io::Error),
    #[error("runtime command channel closed")]
    RuntimeClosed,
}

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
enum AppMode {
    Configured,
    ConfigurationOnly,
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

struct App {
    startup: Option<StartupSnapshot>,
    mode: AppMode,
    prompt: String,
    run_state: RunState,
    disabled_reason: Option<RuntimeDisabledReason>,
    latest_run_id: Option<RunId>,
    settled_run_ids: Vec<RunId>,
    completed_steps: Vec<(RunId, ModelStepId)>,
    pending_run_gap: bool,
    status: String,
    records: Vec<DisplayedRecord>,
    live_step: Option<LiveStep>,
    scroll_from_bottom: usize,
    history_height: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            startup: None,
            mode: AppMode::Configured,
            prompt: String::new(),
            run_state: RunState::Idle,
            disabled_reason: None,
            latest_run_id: None,
            settled_run_ids: Vec::new(),
            completed_steps: Vec::new(),
            pending_run_gap: false,
            status: "idle".to_owned(),
            records: Vec::new(),
            live_step: None,
            scroll_from_bottom: 0,
            history_height: 0,
        }
    }
}

#[derive(Default)]
struct Update {
    command: Option<RuntimeCommand>,
    visible_changed: bool,
    quit: bool,
}

impl App {
    fn configured(startup: StartupSnapshot) -> Self {
        Self {
            startup: Some(startup),
            ..Self::default()
        }
    }

    fn configuration(startup: StartupSnapshot) -> Self {
        Self {
            startup: Some(startup),
            mode: AppMode::ConfigurationOnly,
            ..Self::default()
        }
    }

    fn is_configuration_only(&self) -> bool {
        self.mode == AppMode::ConfigurationOnly
    }

    fn is_submission_disabled(&self) -> bool {
        self.is_configuration_only() || self.disabled_reason.is_some()
    }

    fn handle_key(&mut self, key: KeyEvent) -> Update {
        if key.is_release() {
            return Update::default();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Update {
                command: (!self.is_configuration_only()).then_some(RuntimeCommand::Shutdown),
                visible_changed: false,
                quit: true,
            };
        }

        match key.code {
            KeyCode::Up | KeyCode::PageUp | KeyCode::Home => {
                let scroll = match key.code {
                    KeyCode::Up => self.scroll_from_bottom.saturating_add(1),
                    KeyCode::PageUp => self.scroll_from_bottom.saturating_add(10),
                    KeyCode::Home => usize::MAX,
                    _ => unreachable!(),
                };
                self.update_scroll(scroll)
            }
            KeyCode::Down | KeyCode::PageDown | KeyCode::End => {
                let scroll = match key.code {
                    KeyCode::Down => self.scroll_from_bottom.saturating_sub(1),
                    KeyCode::PageDown => self.scroll_from_bottom.saturating_sub(10),
                    KeyCode::End => 0,
                    _ => unreachable!(),
                };
                self.update_scroll(scroll)
            }
            _ if self.is_submission_disabled() => Update::default(),
            KeyCode::Esc if self.run_state != RunState::Idle => Update {
                command: Some(RuntimeCommand::Cancel),
                visible_changed: false,
                quit: false,
            },
            KeyCode::Backspace if !self.prompt.is_empty() => {
                self.prompt.pop();
                Update {
                    command: None,
                    visible_changed: true,
                    quit: false,
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.prompt.len() + character.len_utf8() <= MAX_PROMPT_BYTES =>
            {
                self.prompt.push(character);
                Update {
                    command: None,
                    visible_changed: true,
                    quit: false,
                }
            }
            KeyCode::Enter
                if self.disabled_reason.is_none()
                    && self.run_state == RunState::Idle
                    && !self.prompt.is_empty() =>
            {
                self.run_state = RunState::Submitting;
                self.status = "starting run".to_owned();
                Update {
                    command: Some(RuntimeCommand::Submit {
                        request: std::mem::take(&mut self.prompt),
                    }),
                    visible_changed: true,
                    quit: false,
                }
            }
            _ => Update::default(),
        }
    }

    fn handle_event(&mut self, event: RuntimeEvent) -> bool {
        self.handle_event_with_gap(event, false)
    }

    fn handle_progress(&mut self, envelope: ProgressEnvelope<RuntimeEvent>) -> bool {
        self.handle_event_with_gap(envelope.item, envelope.gap_before)
    }

    fn handle_event_with_gap(&mut self, event: RuntimeEvent, gap_before: bool) -> bool {
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

    fn update_scroll(&mut self, scroll_from_bottom: usize) -> Update {
        let hidden_lines = format_history(self)
            .lines()
            .count()
            .saturating_sub(self.history_height);
        let scroll_from_bottom = scroll_from_bottom.min(hidden_lines);
        if self.scroll_from_bottom == scroll_from_bottom {
            Update::default()
        } else {
            self.scroll_from_bottom = scroll_from_bottom;
            Update {
                command: None,
                visible_changed: true,
                quit: false,
            }
        }
    }
}

struct RestoreGuard<F: FnOnce()> {
    restore: Option<F>,
}

impl<F: FnOnce()> RestoreGuard<F> {
    fn new(restore: F) -> Self {
        Self {
            restore: Some(restore),
        }
    }
}

impl<F: FnOnce()> Drop for RestoreGuard<F> {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

pub(crate) fn run(
    startup: StartupSnapshot,
    commands: mpsc::Sender<RuntimeCommand>,
    mut events: mpsc::UnboundedReceiver<RuntimeEvent>,
    mut progress: RuntimeProgressReceiver,
) -> Result<(), TuiError> {
    let mut app = App::configured(startup);
    let mut input = CrosstermEvents;
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| {
            run_loop(
                terminal,
                &mut app,
                &mut input,
                &commands,
                &mut events,
                &mut progress,
            )
            .map(|_| ())
        },
    )
}

pub(crate) fn run_configuration(startup: StartupSnapshot) -> Result<(), TuiError> {
    let mut app = App::configuration(startup);
    let mut input = CrosstermEvents;
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| run_configuration_loop(terminal, &mut app, &mut input).map(|_| ()),
    )
}

fn with_terminal<B: Backend>(
    initialize: impl FnOnce() -> Result<Terminal<B>, TuiError>,
    restore: impl FnOnce(),
    run: impl FnOnce(&mut Terminal<B>) -> Result<(), TuiError>,
) -> Result<(), TuiError> {
    let _guard = RestoreGuard::new(restore);
    let mut terminal = initialize()?;
    run(&mut terminal)
}

fn run_configuration_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    input: &mut impl EventSource,
) -> Result<usize, TuiError> {
    let mut dirty = true;
    let mut redraws = 0;

    loop {
        if dirty {
            terminal
                .draw(|frame| render(frame, app))
                .map_err(|error| TuiError::Terminal(error.to_string()))?;
            redraws += 1;
            dirty = false;
        }

        if input.poll(POLL_INTERVAL).map_err(TuiError::Input)? {
            match input.read().map_err(TuiError::Input)? {
                Event::Key(key) => {
                    let update = app.handle_key(key);
                    dirty |= update.visible_changed;
                    if update.quit {
                        return Ok(redraws);
                    }
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }
    }
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    input: &mut impl EventSource,
    commands: &mpsc::Sender<RuntimeCommand>,
    events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    progress: &mut RuntimeProgressReceiver,
) -> Result<usize, TuiError> {
    let mut dirty = true;
    let mut redraws = 0;
    let mut progress_open = true;

    loop {
        if dirty {
            terminal
                .draw(|frame| render(frame, app))
                .map_err(|error| TuiError::Terminal(error.to_string()))?;
            redraws += 1;
            dirty = false;
        }

        let mut quit = false;
        if input.poll(POLL_INTERVAL).map_err(TuiError::Input)? {
            match input.read().map_err(TuiError::Input)? {
                Event::Key(key) => {
                    let update = app.handle_key(key);
                    dirty |= update.visible_changed;
                    if let Some(command) = update.command {
                        commands
                            .blocking_send(command)
                            .map_err(|_| TuiError::RuntimeClosed)?;
                    }
                    quit = update.quit;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        loop {
            match events.try_recv() {
                Ok(event) => dirty |= app.handle_event(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) if quit => return Ok(redraws),
                Err(TryRecvError::Disconnected) => return Err(TuiError::RuntimeClosed),
            }
        }

        if progress_open {
            loop {
                match progress.try_recv() {
                    Ok(envelope) => dirty |= app.handle_progress(envelope),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        progress_open = false;
                        break;
                    }
                }
            }
        }

        if quit {
            return Ok(redraws);
        }
    }
}

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    let configuration_only = app.is_configuration_only();
    let reserved_rows = if configuration_only { 1 } else { 2 };
    app.history_height = if area.width == 0 {
        0
    } else {
        area.height.saturating_sub(reserved_rows) as usize
    };
    if area.width == 0 || area.height == 0 {
        return;
    }

    let header = if configuration_only {
        "Configuration only: read-only; submission unavailable | Ctrl-C quit | Up/Down scroll"
            .to_owned()
    } else if app.disabled_reason.is_some() {
        format!(
            "Status: {} | Input disabled | Ctrl-C quit | Up/Down scroll",
            normalise_terminal_text(&app.status)
        )
    } else {
        format!(
            "Status: {} | Enter submit | Esc cancel | Ctrl-C quit | Up/Down scroll",
            normalise_terminal_text(&app.status)
        )
    };
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if !configuration_only && area.height >= 2 {
        let prompt_y = area.y.saturating_add(area.height - 1);
        frame.render_widget(
            Paragraph::new(format!("> {}", normalise_terminal_text(&app.prompt))),
            Rect::new(area.x, prompt_y, area.width, 1),
        );
    }

    let history_height = area.height.saturating_sub(reserved_rows);
    if history_height == 0 {
        return;
    }

    let history = format_history(app);
    let last_page_start = history
        .lines()
        .count()
        .saturating_sub(history_height as usize);
    let scroll = app.scroll_from_bottom.min(last_page_start);
    let top = last_page_start.saturating_sub(scroll);
    frame.render_widget(
        Paragraph::new(history).scroll((u16::try_from(top).unwrap_or(u16::MAX), 0)),
        Rect::new(area.x, area.y.saturating_add(1), area.width, history_height),
    );
}

fn format_history(app: &App) -> String {
    let mut entries = app
        .startup
        .as_ref()
        .map(format_startup_configuration)
        .into_iter()
        .collect::<Vec<_>>();
    for (index, displayed) in app.records.iter().enumerate() {
        let prior_attempts = app.records[..index]
            .iter()
            .filter(|prior| prior.record.run_id == displayed.record.run_id)
            .filter_map(|prior| match &prior.record.record {
                Record::CompletedExecution { attempts, .. } => Some(attempts.iter()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        entries.push(format_record(displayed, &prior_attempts));
    }
    if let Some(live) = &app.live_step {
        let prior_attempts = app
            .records
            .iter()
            .filter(|record| record.record.run_id == live.run_id)
            .filter_map(|record| match &record.record.record {
                Record::CompletedExecution { attempts, .. } => Some(attempts.iter()),
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>();
        entries.push(format_live_step(live, &prior_attempts));
    }
    entries.join("\n")
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
    let terminal_step_attempts = terminal
        .attempts
        .get(prior_attempts.len()..)
        .unwrap_or_default();
    append_attempts(&mut lines, terminal_step_attempts);
    lines.push(format_cost(
        "Step estimated cost",
        &terminal_step_attempts.iter().collect::<Vec<_>>(),
    ));
    lines.push(format_cost(
        "Run estimated cost",
        &terminal.attempts.iter().collect::<Vec<_>>(),
    ));
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

fn normalise_terminal_text(text: &str) -> String {
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
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::Duration;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::App;
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, InterruptedRead, Observation, PythonError, ReadOutcome,
    };
    use crate::model::{AttemptAccounting, ModelProgress, ModelUsage};
    use crate::runtime::{
        ModelStepId, ProgressEnvelope, Record, RunId, RuntimeCommand, RuntimeDisabledReason,
        RuntimeEvent, StoredRecord, TerminalDetails,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn rendered(app: &mut App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
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

    struct FakeInput {
        polls: VecDeque<Option<ratatui::crossterm::event::Event>>,
        ready: Option<ratatui::crossterm::event::Event>,
        timeouts: Vec<Duration>,
    }

    impl FakeInput {
        fn new(polls: impl IntoIterator<Item = Option<ratatui::crossterm::event::Event>>) -> Self {
            Self {
                polls: polls.into_iter().collect(),
                ready: None,
                timeouts: Vec::new(),
            }
        }
    }

    impl super::EventSource for FakeInput {
        fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
            self.timeouts.push(timeout);
            self.ready = self.polls.pop_front().flatten();
            Ok(self.ready.is_some())
        }

        fn read(&mut self) -> io::Result<ratatui::crossterm::event::Event> {
            self.ready
                .take()
                .ok_or_else(|| io::Error::other("no fake input ready"))
        }
    }

    struct ErrorInput;

    impl super::EventSource for ErrorInput {
        fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
            Err(io::Error::other("input failed"))
        }

        fn read(&mut self) -> io::Result<ratatui::crossterm::event::Event> {
            unreachable!()
        }
    }

    #[test]
    fn idle_enter_submits_the_prompt_once() {
        let mut app = App::default();
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));

        let update = app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            update.command,
            Some(RuntimeCommand::Submit {
                request: "hi".to_owned()
            })
        );
        assert!(update.visible_changed);
        assert_eq!(app.prompt, "");
        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
    }

    #[test]
    fn prompt_enforces_its_byte_limit_and_backspaces_unicode() {
        let mut app = App {
            prompt: "a".repeat(super::MAX_PROMPT_BYTES - '🦀'.len_utf8()),
            ..App::default()
        };

        assert!(app.handle_key(key(KeyCode::Char('🦀'))).visible_changed);
        assert_eq!(app.prompt.len(), super::MAX_PROMPT_BYTES);
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(app.prompt.len(), super::MAX_PROMPT_BYTES);

        assert!(app.handle_key(key(KeyCode::Backspace)).visible_changed);
        assert!(!app.prompt.ends_with('🦀'));
        assert!(app.prompt.is_char_boundary(app.prompt.len()));
    }

    #[test]
    fn configuration_configured_mode_renders_a_ready_snapshot_and_remains_editable() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let history = super::format_history(&app);
        assert!(history.contains("Startup configuration"));
        assert!(history.contains("Submission: available"));
        assert!(history.contains("OpenAI API key: present"));
        assert!(!history.contains("test-api-key"));

        assert!(app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).command,
            Some(RuntimeCommand::Submit {
                request: "x".to_owned()
            })
        );
    }

    #[test]
    fn configuration_controls_are_read_only_and_ctrl_c_needs_no_command() {
        let mut app = App::configuration(crate::config::load().snapshot().clone());
        app.prompt = "locked".to_owned();
        let prompt = app.prompt.clone();
        let screen = rendered(&mut app, 120, 15);
        assert!(screen.contains("Configuration only: read-only; submission unavailable"));
        assert!(!screen.contains("> locked"));

        for code in [
            KeyCode::Char('x'),
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Esc,
        ] {
            let update = app.handle_key(key(code));
            assert!(update.command.is_none());
            assert!(!update.visible_changed);
            assert!(!update.quit);
            assert_eq!(app.prompt, prompt);
            assert_eq!(app.run_state, super::RunState::Idle);
        }

        let quit = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(quit.command.is_none());
        assert!(!quit.visible_changed);
        assert!(quit.quit);
        assert_eq!(app.prompt, prompt);
    }

    #[test]
    fn configuration_entry_requires_only_a_startup_snapshot() {
        let _: fn(crate::config::StartupSnapshot) -> Result<(), super::TuiError> =
            super::run_configuration;
    }

    #[test]
    fn configuration_loop_redraws_scrolls_and_handles_tiny_terminals() {
        let mut app = App::configuration(crate::config::load().snapshot().clone());
        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut input = FakeInput::new([
            None,
            Some(ratatui::crossterm::event::Event::Key(key(KeyCode::Up))),
            Some(ratatui::crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
        ]);

        let redraws = super::run_configuration_loop(&mut terminal, &mut app, &mut input).unwrap();

        assert_eq!(redraws, 2);
        assert!(app.scroll_from_bottom > 0);
        assert!(
            input
                .timeouts
                .iter()
                .all(|timeout| *timeout <= Duration::from_millis(50))
        );

        for (width, height) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 2), (3, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
        }
    }

    #[test]
    fn configuration_loop_restores_the_terminal_after_initialisation_input_and_panic() {
        let restored_after_initialisation = Cell::new(false);
        let initialisation_result = super::with_terminal::<TestBackend>(
            || {
                Err(super::TuiError::Terminal(
                    "initialisation failed".to_owned(),
                ))
            },
            || restored_after_initialisation.set(true),
            |_| Ok(()),
        );
        assert!(initialisation_result.is_err());
        assert!(restored_after_initialisation.get());

        let restored_after_error = Cell::new(false);
        let mut app = App::configuration(crate::config::load().snapshot().clone());
        let mut input = ErrorInput;
        let input_result = super::with_terminal(
            || {
                Terminal::new(TestBackend::new(10, 3))
                    .map_err(|error| super::TuiError::Terminal(error.to_string()))
            },
            || restored_after_error.set(true),
            |terminal| super::run_configuration_loop(terminal, &mut app, &mut input).map(|_| ()),
        );
        assert!(matches!(input_result, Err(super::TuiError::Input(_))));
        assert!(restored_after_error.get());

        let restored_after_panic = Cell::new(false);
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = super::with_terminal(
                || {
                    Terminal::new(TestBackend::new(10, 3))
                        .map_err(|error| super::TuiError::Terminal(error.to_string()))
                },
                || restored_after_panic.set(true),
                |_| -> Result<(), super::TuiError> { panic!("configuration loop panic") },
            );
        }));
        assert!(result.is_err());
        assert!(restored_after_panic.get());
    }

    #[test]
    fn active_run_accepts_cancel_but_not_submit_and_ctrl_c_shuts_down() {
        let mut app = App::default();
        assert!(app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(7) }));
        app.handle_key(key(KeyCode::Char('x')));

        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)).command,
            Some(RuntimeCommand::Cancel)
        );

        let shutdown = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(shutdown.command, Some(RuntimeCommand::Shutdown));
        assert!(shutdown.quit);
    }

    #[test]
    fn obsolete_live_events_are_ignored_but_committed_records_are_kept() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::ExecutionStarted {
            run_id: RunId(2),
            step_id: ModelStepId(4),
        });
        let current_status = app.status.clone();

        assert!(!app.handle_event(RuntimeEvent::ToolStarted {
            run_id: RunId(1),
            step_id: ModelStepId(3),
            path: "stale".to_owned(),
        }));
        assert!(!app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) }));
        assert_eq!(app.status, current_status);
        assert_eq!(app.run_state, super::RunState::Active(RunId(2)));
        assert!(app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(2) }));
        assert!(!app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) }));
        assert_eq!(app.run_state, super::RunState::Idle);

        let stale_record = StoredRecord {
            run_id: RunId(1),
            record: Record::UserRequest {
                request: "committed earlier".to_owned(),
            },
        };
        assert!(app.handle_event(RuntimeEvent::RecordAppended {
            record: stale_record.clone(),
            progress_gap: false,
        }));
        assert_eq!(app.records[0].record, stale_record);
    }

    #[test]
    fn transcript_current_live_progress_updates_status_until_settlement() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(3) });
        assert_eq!(app.status, "run 3 started");

        app.handle_event(RuntimeEvent::ModelStepStarted {
            run_id: RunId(3),
            step_id: ModelStepId(8),
        });
        assert_eq!(app.status, "model step 8");
        app.handle_event(RuntimeEvent::ExecutionStarted {
            run_id: RunId(3),
            step_id: ModelStepId(8),
        });
        assert_eq!(app.status, "running code for step 8");
        app.handle_event(RuntimeEvent::ToolStarted {
            run_id: RunId(3),
            step_id: ModelStepId(8),
            path: "Cargo.toml".to_owned(),
        });
        assert_eq!(app.status, "reading Cargo.toml");
        app.handle_event(RuntimeEvent::ToolFinished {
            run_id: RunId(3),
            step_id: ModelStepId(8),
            path: "Cargo.toml".to_owned(),
            outcome: ReadOutcome::Success { line_count: 12 },
        });
        assert_eq!(app.status, "read Cargo.toml (12 lines)");
        app.handle_event(RuntimeEvent::SubmissionRejectedBusy {
            active_run_id: RunId(3),
        });
        assert_eq!(app.status, "run 3 is busy");

        assert!(app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(3) }));
        assert_eq!(app.status, "idle");
        app.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)).command,
            Some(RuntimeCommand::Submit { .. })
        ));
    }

    #[test]
    fn transcript_rejects_stale_stdout_progress() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::ModelStepStarted {
            run_id: RunId(2),
            step_id: ModelStepId(3),
        });
        let status = app.status.clone();

        assert!(!app.handle_event(RuntimeEvent::StdoutDelta {
            run_id: RunId(1),
            step_id: ModelStepId(2),
            text: "stale stdout".to_owned(),
        }));
        assert!(!app.handle_event(RuntimeEvent::StdoutTruncated {
            run_id: RunId(1),
            step_id: ModelStepId(2),
        }));
        assert_eq!(app.status, status);
        assert!(matches!(
            app.live_step,
            Some(ref live) if live.run_id == RunId(2) && live.step_id == ModelStepId(3)
        ));
    }

    #[test]
    fn disabled_authentication_stays_sticky_and_rejects_late_progress() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FailedRun {
                    message: "provider error".to_owned(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });

        assert!(app.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        }));
        assert_eq!(app.status, "OpenAI authentication failed; restart Kiki");
        assert!(rendered(&mut app, 120, 15).contains("Input disabled"));
        assert!(app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) }));
        assert_eq!(app.run_state, super::RunState::Idle);
        assert_eq!(app.status, "OpenAI authentication failed; restart Kiki");

        app.prompt = "saved prompt".to_owned();
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(app.prompt, "saved prompt");
        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
        assert_eq!(app.prompt, "saved prompt");
        let shutdown = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(shutdown.command, Some(RuntimeCommand::Shutdown));
        assert!(shutdown.quit);

        let records = app.records.clone();
        let status = app.status.clone();
        for event in [
            RuntimeEvent::RunStarted { run_id: RunId(2) },
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: RunId(1),
            },
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            },
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AssistantTextChunk {
                    text: "late model".to_owned(),
                },
            },
            RuntimeEvent::ToolStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                path: "late tool".to_owned(),
            },
            RuntimeEvent::StdoutDelta {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                text: "late stdout".to_owned(),
            },
        ] {
            assert!(!app.handle_event(event));
        }
        assert_eq!(app.status, status);
        assert!(app.live_step.is_none());
        assert!(!app.pending_run_gap);
        assert_eq!(app.records, records);
        assert!(super::format_history(&app).contains("Submission: available"));

        let mut rejected = App {
            prompt: "submit".to_owned(),
            ..App::default()
        };
        assert!(matches!(
            rejected.handle_key(key(KeyCode::Enter)).command,
            Some(RuntimeCommand::Submit { .. })
        ));
        assert_eq!(rejected.run_state, super::RunState::Submitting);
        assert!(
            rejected.handle_event(RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::ArenaCheckpointRecoveryFailed,
            })
        );
        assert_eq!(rejected.run_state, super::RunState::Idle);
        assert_eq!(rejected.status, "Arena recovery failed; restart Kiki");
    }

    #[test]
    fn transcript_committed_history_renders_chronologically_through_final_settlement() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "inspect".to_owned(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(2),
                    text: "finished".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) });

        let screen = rendered(&mut app, 60, 8);

        assert!(screen.starts_with("Status: idle"));
        let request = screen.find("[run 1] you: inspect").unwrap();
        let final_message = screen.find("[run 1 step 2] kiki: finished").unwrap();
        assert!(request < final_message);
        assert!(screen.contains("> "));
    }

    #[test]
    fn transcript_completed_and_terminal_records_render_inspectable_content() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    source: "print('hello')\n2".to_owned(),
                    reasoning_summary: String::new(),
                    execution: Execution {
                        stdout: observation("hello\n"),
                        stderr: observation("warning\n"),
                        outcome: ExecutionOutcome::Complete {
                            result: Some(observation("2")),
                        },
                    },
                    attempts: Vec::new(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CancelledRun {
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::FailedRun {
                    message: "model failed".to_owned(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });

        let screen = super::format_history(&app);

        assert!(screen.contains("[run 1 step 1] code:"));
        assert!(screen.contains("print('hello')"));
        assert!(screen.contains("stdout: hello"));
        assert!(screen.contains("stderr: warning"));
        assert!(screen.contains("result: 2"));
        assert!(screen.contains("[run 1] cancelled"));
        assert!(screen.contains("[run 2] failed: model failed"));
    }

    #[test]
    fn transcript_keyboard_scrolling_moves_between_oldest_and_newest_history() {
        let mut app = App::default();
        for run_id in 1..=8 {
            app.handle_event(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(run_id),
                    record: Record::UserRequest {
                        request: format!("request {run_id}"),
                    },
                },
                progress_gap: false,
            });
        }

        let newest = rendered(&mut app, 40, 5);
        assert!(newest.contains("request 8"));
        assert!(!newest.contains("request 1"));

        assert!(app.handle_key(key(KeyCode::Up)).visible_changed);
        let one_line_up = rendered(&mut app, 40, 5);
        assert!(one_line_up.contains("request 5"));
        assert!(!one_line_up.contains("request 8"));

        app.handle_key(key(KeyCode::Home));
        let oldest = rendered(&mut app, 40, 5);
        assert!(oldest.contains("request 1"));
        assert!(!oldest.contains("request 8"));

        app.handle_key(key(KeyCode::End));
        assert!(rendered(&mut app, 40, 5).contains("request 8"));
    }

    #[test]
    fn transcript_scrolling_without_hidden_history_does_not_request_a_redraw() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "only line".to_owned(),
                },
            },
            progress_gap: false,
        });
        rendered(&mut app, 40, 5);

        assert!(!app.handle_key(key(KeyCode::Up)).visible_changed);
    }

    #[test]
    fn transcript_progress_channels_reject_stale_runs_and_steps_and_keep_terminal_authoritative() {
        let mut app = App {
            run_state: super::RunState::Submitting,
            ..App::default()
        };

        assert!(app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::UserRequest {
                    request: "request".to_owned(),
                },
            },
            progress_gap: false,
        }));
        assert!(app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::RunStarted { run_id: RunId(2) },
            gap_before: true,
        }));
        assert!(app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(3),
            },
            gap_before: false,
        }));
        assert!(matches!(app.live_step, Some(ref live) if live.progress_gap));
        assert!(!app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(9),
            },
            gap_before: false,
        }));
        assert!(!app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ExecutionStarted {
                run_id: RunId(2),
                step_id: ModelStepId(2),
            },
            gap_before: false,
        }));

        let mut completion_first = App::default();
        completion_first.handle_event(RuntimeEvent::RunStarted { run_id: RunId(3) });
        completion_first.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(3),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(4),
                    source: "code".to_owned(),
                    reasoning_summary: String::new(),
                    execution: Execution {
                        stdout: observation(""),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::Complete { result: None },
                    },
                    attempts: Vec::new(),
                },
            },
            progress_gap: false,
        });
        assert!(!completion_first.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id: RunId(3),
                step_id: ModelStepId(4),
            },
            gap_before: false,
        }));

        assert!(app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::FinalMessage {
                    step_id: ModelStepId(3),
                    text: "done".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: true,
        }));
        assert!(!app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(4),
                progress: crate::model::ModelProgress::AssistantTextChunk {
                    text: "late".to_owned(),
                },
            },
            gap_before: false,
        }));
        assert!(super::format_history(&app).contains("display gap"));
    }

    #[test]
    fn progress_channels_close_without_closing_the_tui() {
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        let mut input = FakeInput::new([Some(ratatui::crossterm::event::Event::Key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ))]);
        let (command_sender, mut command_receiver) = tokio::sync::mpsc::channel(8);
        let (_event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (progress_sender, mut progress_receiver) = tokio::sync::mpsc::channel(256);
        drop(progress_sender);

        assert!(
            super::run_loop(
                &mut terminal,
                &mut app,
                &mut input,
                &command_sender,
                &mut event_receiver,
                &mut progress_receiver,
            )
            .is_ok()
        );
        assert_eq!(
            command_receiver.try_recv().unwrap(),
            RuntimeCommand::Shutdown
        );
    }

    #[test]
    fn transcript_loop_redraws_only_for_visible_changes_and_drains_events_without_a_key() {
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        let mut input = FakeInput::new([
            None,
            Some(ratatui::crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
        ]);
        let (command_sender, mut command_receiver) = tokio::sync::mpsc::channel(8);
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (_progress_sender, mut progress_receiver) = tokio::sync::mpsc::channel(256);
        event_sender
            .send(RuntimeEvent::RunStarted { run_id: RunId(1) })
            .unwrap();
        event_sender
            .send(RuntimeEvent::RunStarted { run_id: RunId(1) })
            .unwrap();

        let redraws = super::run_loop(
            &mut terminal,
            &mut app,
            &mut input,
            &command_sender,
            &mut event_receiver,
            &mut progress_receiver,
        )
        .unwrap();

        assert_eq!(redraws, 2);
        assert_eq!(
            command_receiver.try_recv().unwrap(),
            RuntimeCommand::Shutdown
        );
        assert!(
            input
                .timeouts
                .iter()
                .all(|timeout| *timeout <= Duration::from_millis(50))
        );
    }

    #[test]
    fn loop_fails_when_the_runtime_event_channel_closes() {
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::default();
        let mut input = FakeInput::new([None]);
        let (command_sender, _command_receiver) = tokio::sync::mpsc::channel(8);
        let (event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (_progress_sender, mut progress_receiver) = tokio::sync::mpsc::channel(256);
        drop(event_sender);

        let result = super::run_loop(
            &mut terminal,
            &mut app,
            &mut input,
            &command_sender,
            &mut event_receiver,
            &mut progress_receiver,
        );

        assert!(matches!(result, Err(super::TuiError::RuntimeClosed)));
    }

    #[test]
    fn transcript_rendering_is_stable_at_tiny_dimensions() {
        let mut app = App {
            prompt: "🦀".to_owned(),
            ..App::default()
        };
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "small".to_owned(),
                },
            },
            progress_gap: false,
        });

        for (width, height) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 2), (3, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
        }
    }

    #[test]
    fn transcript_terminal_is_restored_after_initialisation_and_loop_errors() {
        let restored_after_init = Cell::new(false);
        let init_result = super::with_terminal::<TestBackend>(
            || Err(super::TuiError::Terminal("init failed".to_owned())),
            || restored_after_init.set(true),
            |_| Ok(()),
        );
        assert!(init_result.is_err());
        assert!(restored_after_init.get());

        let restored_after_loop = Cell::new(false);
        let (command_sender, _command_receiver) = tokio::sync::mpsc::channel(8);
        let (_event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (_progress_sender, mut progress_receiver) = tokio::sync::mpsc::channel(256);
        let mut app = App::default();
        let mut input = ErrorInput;
        let loop_result = super::with_terminal(
            || {
                Terminal::new(TestBackend::new(10, 3))
                    .map_err(|error| super::TuiError::Terminal(error.to_string()))
            },
            || restored_after_loop.set(true),
            |terminal| {
                super::run_loop(
                    terminal,
                    &mut app,
                    &mut input,
                    &command_sender,
                    &mut event_receiver,
                    &mut progress_receiver,
                )
                .map(|_| ())
            },
        );
        assert!(loop_result.is_err());
        assert!(restored_after_loop.get());
    }

    #[test]
    fn transcript_terminal_is_restored_during_unwind() {
        let restored = Cell::new(false);

        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = super::with_terminal(
                || {
                    Terminal::new(TestBackend::new(10, 3))
                        .map_err(|error| super::TuiError::Terminal(error.to_string()))
                },
                || restored.set(true),
                |_| -> Result<(), super::TuiError> { panic!("loop panic") },
            );
        }));

        assert!(result.is_err());
        assert!(restored.get());
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

    fn terminal(
        evidence: Option<TerminalEvidence>,
        attempts: Vec<AttemptAccounting>,
    ) -> TerminalDetails {
        TerminalDetails { evidence, attempts }
    }

    fn completed_record(
        run_id: u64,
        step_id: u64,
        source: &str,
        attempts: Vec<AttemptAccounting>,
    ) -> RuntimeEvent {
        RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(run_id),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(step_id),
                    source: source.to_owned(),
                    reasoning_summary: String::new(),
                    execution: Execution {
                        stdout: observation("authoritative stdout\n"),
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

    #[test]
    fn transcript_projects_reasoning_code_and_stdout_then_replaces_the_preview() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::ModelStepStarted {
            run_id: RunId(1),
            step_id: ModelStepId(1),
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::ReasoningSummaryChunk {
                text: "reasoning".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::DecodedPythonChunk {
                code: "print('preview')".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::ExecutionStarted {
            run_id: RunId(1),
            step_id: ModelStepId(1),
        });
        app.handle_event(RuntimeEvent::StdoutDelta {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            text: "preview stdout\n".to_owned(),
        });
        assert!(app.handle_event(RuntimeEvent::StdoutTruncated {
            run_id: RunId(1),
            step_id: ModelStepId(1),
        }));
        assert!(!app.handle_event(RuntimeEvent::StdoutTruncated {
            run_id: RunId(1),
            step_id: ModelStepId(1),
        }));

        let preview = super::format_history(&app);
        let reasoning = preview.find("reasoning: reasoning").unwrap();
        let code = preview.find("print('preview')").unwrap();
        let stdout = preview.find("preview stdout").unwrap();
        assert!(reasoning < code && code < stdout);
        assert_eq!(preview.matches("stdout preview truncated").count(), 1);

        assert!(app.handle_event(completed_record(1, 1, "print('authoritative')", vec![])));
        let history = super::format_history(&app);
        assert!(history.contains("print('authoritative')"));
        assert_eq!(history.matches("authoritative stdout").count(), 1);
        assert!(!history.contains("print('preview')"));
        assert!(!history.contains("preview stdout"));
    }

    #[test]
    fn transcript_retry_reset_keeps_settled_attempt_accounting_and_gap() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AttemptStarted { attempt: 1 },
            },
            gap_before: true,
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::ReasoningSummaryChunk {
                text: "discarded reasoning".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::DecodedPythonChunk {
                code: "discarded_code()".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: usage(100, 0),
            },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AttemptStarted { attempt: 2 },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "retained answer".to_owned(),
            },
        });

        let history = super::format_history(&app);
        assert!(!history.contains("discarded reasoning"));
        assert!(!history.contains("discarded_code()"));
        assert!(history.contains("retained answer"));
        assert!(history.contains("attempt 1: model=gpt-5.6-sol"));
        assert!(history.contains("display gap"));
    }

    #[test]
    fn transcript_attaches_gaps_only_to_current_live_or_committed_content() {
        let mut app = App::default();
        app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::RunStarted { run_id: RunId(1) },
            gap_before: true,
        });
        app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            },
            gap_before: false,
        });
        assert!(super::format_history(&app).contains("display gap"));

        app.handle_event(completed_record(1, 1, "done", vec![]));
        assert!(!app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale".to_owned(),
                },
            },
            gap_before: true,
        }));
        assert!(!super::format_history(&app).contains("stale"));
        assert!(!super::format_history(&app).contains("display gap"));

        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(2),
                    text: "settled".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: terminal(None, vec![]),
                },
            },
            progress_gap: true,
        });
        let history = super::format_history(&app);
        assert!(history.contains("settled"));
        assert!(history.contains("display gap"));

        let mut next_run = App::default();
        next_run.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::RunStarted { run_id: RunId(1) },
            gap_before: true,
        });
        next_run.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CancelledRun {
                    terminal: terminal(None, vec![]),
                },
            },
            progress_gap: false,
        });
        next_run.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) });
        next_run.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::RunStarted { run_id: RunId(2) },
            gap_before: false,
        });
        next_run.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(2),
            },
            gap_before: false,
        });
        assert!(matches!(next_run.live_step, Some(ref live) if !live.progress_gap));
    }

    #[test]
    fn transcript_rejects_stale_steps_and_post_settlement_progress_without_dirtying() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::ModelStepStarted {
            run_id: RunId(2),
            step_id: ModelStepId(2),
        });
        assert!(!app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(2),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "old step".to_owned(),
            },
        }));
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::CancelledRun {
                    terminal: terminal(None, vec![]),
                },
            },
            progress_gap: false,
        });
        assert!(!super::format_history(&app).contains("[run 2 step 2] live"));
        assert!(!app.handle_event(RuntimeEvent::StdoutDelta {
            run_id: RunId(2),
            step_id: ModelStepId(3),
            text: "late stdout".to_owned(),
        }));
        assert!(!super::format_history(&app).contains("old step"));
        assert!(!super::format_history(&app).contains("late stdout"));
    }

    #[test]
    fn transcript_renders_completed_and_interrupted_evidence_variants() {
        let mut app = App::default();
        app.handle_event(completed_record(1, 1, "result = 42", vec![]));
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(2),
                    source: "raise RuntimeError()".to_owned(),
                    reasoning_summary: String::new(),
                    execution: Execution {
                        stdout: observation(""),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::PythonError(PythonError {
                            text: "boom".to_owned(),
                            omitted_bytes: 0,
                        }),
                    },
                    attempts: vec![],
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(3),
                record: Record::CancelledRun {
                    terminal: terminal(
                        Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
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
                        }))),
                        vec![],
                    ),
                },
            },
            progress_gap: false,
        });

        let history = super::format_history(&app);
        assert!(history.contains("result: 42"));
        assert!(history.contains("error: boom"));
        assert!(!history.contains("stderr: \n"));
        assert!(history.contains("source: await read(path)"));
        assert!(history.contains("stderr: diagnostic"));
        assert!(history.contains("read: completed bad    path failed: read failed"));
        assert!(history.contains("failure: interrupted"));
        assert!(history.ends_with("warning: cancelled warning"));

        let mut generation = App::default();
        generation.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(4),
                record: Record::CancelledRun {
                    terminal: terminal(
                        Some(TerminalEvidence::Generation(GenerationEvidence {
                            reasoning_summary: String::new(),
                            reasoning: "interrupted reasoning".to_owned(),
                            assistant_text: "interrupted assistant".to_owned(),
                            decoded_python: "interrupted_code()".to_owned(),
                        })),
                        vec![],
                    ),
                },
            },
            progress_gap: false,
        });
        let generation_history = super::format_history(&generation);
        assert!(generation_history.contains("reasoning: interrupted reasoning"));
        assert!(generation_history.contains("assistant: interrupted assistant"));
        assert!(generation_history.contains("code:\ninterrupted_code()"));
    }

    #[test]
    fn transcript_keeps_final_messages_after_prior_executions() {
        let mut app = App::default();
        app.handle_event(completed_record(1, 1, "first()", vec![]));
        app.handle_event(completed_record(1, 2, "second()", vec![]));
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(3),
                    text: "final answer".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: terminal(None, vec![]),
                },
            },
            progress_gap: false,
        });

        let history = super::format_history(&app);
        assert!(history.find("first()").unwrap() < history.find("second()").unwrap());
        assert!(history.find("second()").unwrap() < history.find("final answer").unwrap());
    }

    #[test]
    fn transcript_renders_per_step_and_cumulative_costs_without_double_counting() {
        let mut app = App::default();
        app.handle_event(completed_record(1, 1, "first()", vec![usage(100, 0)]));
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(2),
                    text: "done".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: terminal(
                        None,
                        vec![
                            usage(100, 0),
                            AttemptAccounting::KnownUsageWithUntrackedRetries(ModelUsage {
                                model: "gpt-5.6-sol".to_owned(),
                                service_tier: Some("default".to_owned()),
                                input_tokens: Some(0),
                                cached_input_tokens: Some(0),
                                cache_write_input_tokens: Some(0),
                                output_tokens: Some(100),
                                reasoning_output_tokens: Some(20),
                                total_tokens: Some(100),
                            }),
                        ],
                    ),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::FailedRun {
                    message: "unpriced".to_owned(),
                    terminal: terminal(None, vec![AttemptAccounting::UsageUnavailable]),
                },
            },
            progress_gap: false,
        });

        let history = super::format_history(&app);
        assert_eq!(history.matches("attempt 1: model=gpt-5.6-sol").count(), 2);
        assert!(history.contains("Step estimated cost: Complete $0.0004 USD"));
        assert!(history.contains("Step estimated cost: LowerBound $0.002 USD"));
        assert!(history.contains("Run estimated cost: LowerBound $0.0024 USD"));
        assert!(history.contains("Step estimated cost: Unavailable"));
        assert!(history.contains("Run estimated cost: Unavailable"));
        assert!(!history.contains("Unavailable $0"));

        let mut active = App::default();
        active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(3) });
        active.handle_event(completed_record(3, 3, "prior()", vec![usage(100, 0)]));
        active.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(3),
            step_id: ModelStepId(4),
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: usage(0, 100),
            },
        });
        assert!(
            super::format_history(&active).contains("Run estimated cost: Complete $0.0024 USD")
        );
    }

    #[test]
    fn transcript_following_bottom_and_manual_scroll_survive_live_replacement() {
        let mut app = App::default();
        for step in 1..=4 {
            app.handle_event(completed_record(1, step, &format!("line_{step}"), vec![]));
        }
        rendered(&mut app, 50, 5);
        assert_eq!(app.scroll_from_bottom, 0);
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(2),
            step_id: ModelStepId(5),
            progress: ModelProgress::AssistantTextChunk {
                text: "live".to_owned(),
            },
        });
        assert_eq!(app.scroll_from_bottom, 0);

        assert!(app.handle_key(key(KeyCode::Up)).visible_changed);
        let manual_scroll = app.scroll_from_bottom;
        app.handle_event(completed_record(2, 5, "settled", vec![]));
        assert_eq!(app.scroll_from_bottom, manual_scroll);
    }

    #[test]
    fn transcript_live_changes_dirty_once_and_keeps_tiny_and_restore_behaviour() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        let event = RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "once".to_owned(),
            },
        };
        assert!(app.handle_event(event.clone()));
        assert!(!app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: String::new(),
            },
        }));

        for (width, height) in [(0, 0), (1, 1), (2, 2), (3, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
        }
        let restored = Cell::new(false);
        let _ = super::with_terminal(
            || {
                Terminal::new(TestBackend::new(10, 3))
                    .map_err(|error| super::TuiError::Terminal(error.to_string()))
            },
            || restored.set(true),
            |_| Ok(()),
        );
        assert!(restored.get());
    }

    #[test]
    fn normalise_exhaustively_escapes_terminal_controls() {
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

        assert_eq!(super::normalise_terminal_text(&input), expected);
    }

    #[test]
    fn normalise_fragmented_crlf_and_hostile_projection_have_no_executable_controls() {
        let mut app = App {
            prompt: "prompt\r\n\t\x1b]title\x07".to_owned(),
            status: "status\u{009d}unsafe".to_owned(),
            ..App::default()
        };
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.status = "status\u{009d}unsafe".to_owned();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "user\trequest".to_owned(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "provider\r".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "\ntext\x1b]payload\x07".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::DecodedPythonChunk {
                code: "code\x7f".to_owned(),
            },
        });
        app.handle_event(RuntimeEvent::StdoutDelta {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            text: "stdout\u{0085}".to_owned(),
        });
        app.handle_event(RuntimeEvent::ToolStarted {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            path: "path\r".to_owned(),
        });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::FailedRun {
                    message: "error\x1b".to_owned(),
                    terminal: terminal(
                        Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                            source: "source\x1b".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation("terminal stdout\u{009f}"),
                            stderr: observation(""),
                            read: InterruptedRead::Pending {
                                path: "terminal path\t".to_owned(),
                            },
                            failure: Some(ExecutionFailure::Arena {
                                message: "failure\r".to_owned(),
                            }),
                            cancellation_warning: None,
                        }))),
                        vec![AttemptAccounting::KnownUsage(ModelUsage {
                            model: "model\x1b".to_owned(),
                            service_tier: Some("tier\u{0080}".to_owned()),
                            input_tokens: Some(0),
                            cached_input_tokens: Some(0),
                            cache_write_input_tokens: Some(0),
                            output_tokens: Some(0),
                            reasoning_output_tokens: Some(0),
                            total_tokens: Some(0),
                        })],
                    ),
                },
            },
            progress_gap: false,
        });

        let history = super::format_history(&app);
        assert!(history.contains("user    request"));
        assert!(matches!(
            &app.records[0].record.record,
            Record::UserRequest { request } if request == "user\trequest"
        ));
        assert_eq!(app.prompt, "prompt\r\n\t\x1b]title\x07");
        assert!(matches!(
            app.live_step,
            Some(ref live) if live.assistant_text == "provider\r\ntext\x1b]payload\x07"
        ));
        assert!(history.contains("provider\ntext\\x1B]payload\\x07"));
        assert!(history.contains("code\\x7F"));
        assert!(history.contains("stdout\\x85"));
        assert!(history.contains("path\\x0D"), "{history}");
        assert!(history.contains("source\\x1B"));
        assert!(history.contains("terminal stdout\\x9F"));
        assert!(history.contains("failure\\x0D"));
        assert!(history.contains("terminal path    "));
        assert!(history.contains("error\\x1B"));
        assert!(history.contains("model=model\\x1B"));
        assert!(history.contains("tier=tier\\x80"));

        app.status = "status\u{009d}unsafe".to_owned();
        let screen = rendered(&mut app, 120, 30);
        assert!(screen.contains("status\\x9Dunsafe"));
        assert!(screen.contains("prompt"));
        assert!(!screen.chars().any(|character| {
            matches!(character, '\u{1b}' | '\u{7f}') || character.is_control() && character != '\n'
        }));
    }
}
