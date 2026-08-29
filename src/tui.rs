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

use crate::arena::{Execution, ExecutionOutcome, Observation, ReadOutcome};
use crate::runtime::{
    ModelStepId, ProgressEnvelope, Record, RunId, RuntimeCommand, RuntimeEvent,
    RuntimeProgressReceiver, StoredRecord,
};

const MAX_PROMPT_BYTES: usize = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(50);

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

struct App {
    prompt: String,
    run_state: RunState,
    latest_run_id: Option<RunId>,
    settled_run_id: Option<RunId>,
    current_step_id: Option<ModelStepId>,
    completed_step_id: Option<ModelStepId>,
    progress_gap: bool,
    status: String,
    records: Vec<StoredRecord>,
    scroll_from_bottom: usize,
    history_height: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            run_state: RunState::Idle,
            latest_run_id: None,
            settled_run_id: None,
            current_step_id: None,
            completed_step_id: None,
            progress_gap: false,
            status: "idle".to_owned(),
            records: Vec::new(),
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
    fn handle_key(&mut self, key: KeyEvent) -> Update {
        if key.is_release() {
            return Update::default();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c' | 'C'))
        {
            return Update {
                command: Some(RuntimeCommand::Shutdown),
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
            KeyCode::Enter if self.run_state == RunState::Idle && !self.prompt.is_empty() => {
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
        match event {
            RuntimeEvent::RunStarted { run_id } => self.start_run(run_id),
            RuntimeEvent::ModelStepStarted { run_id, step_id } => {
                self.handle_step_progress(run_id, step_id, format!("model step {}", step_id.0))
            }
            RuntimeEvent::ModelProgress {
                run_id,
                step_id,
                progress,
            } => self.handle_step_progress(
                run_id,
                step_id,
                match progress {
                    crate::model::ModelProgress::AttemptStarted { attempt } => {
                        format!("model step {} attempt {attempt}", step_id.0)
                    }
                    _ => format!("model step {}", step_id.0),
                },
            ),
            RuntimeEvent::ExecutionStarted { run_id, step_id } => self.handle_step_progress(
                run_id,
                step_id,
                format!("running code for step {}", step_id.0),
            ),
            RuntimeEvent::ToolStarted {
                run_id,
                step_id,
                path,
            } => self.handle_step_progress(run_id, step_id, format!("reading {path}")),
            RuntimeEvent::ToolFinished {
                run_id,
                step_id,
                path,
                outcome,
            } => match outcome {
                ReadOutcome::Success { line_count } => self.handle_step_progress(
                    run_id,
                    step_id,
                    format!("read {path} ({line_count} lines)"),
                ),
                ReadOutcome::Failure { message } => self.handle_step_progress(
                    run_id,
                    step_id,
                    format!("read {path} failed: {message}"),
                ),
            },
            RuntimeEvent::RecordAppended {
                record,
                progress_gap,
            } => {
                self.progress_gap |= progress_gap;
                self.latest_run_id = Some(
                    self.latest_run_id
                        .map_or(record.run_id, |latest| latest.max(record.run_id)),
                );
                if let Record::CompletedExecution { step_id, .. } = &record.record
                    && self.latest_run_id == Some(record.run_id)
                {
                    self.completed_step_id = Some(
                        self.completed_step_id
                            .map_or(*step_id, |completed| completed.max(*step_id)),
                    );
                }
                if matches!(
                    &record.record,
                    Record::FinalMessage { .. }
                        | Record::CancelledRun { .. }
                        | Record::FailedRun { .. }
                ) {
                    self.settled_run_id = Some(
                        self.settled_run_id
                            .map_or(record.run_id, |settled| settled.max(record.run_id)),
                    );
                }
                self.records.push(record);
                true
            }
            RuntimeEvent::RunSettled { run_id }
                if self
                    .latest_run_id
                    .is_none_or(|latest_run_id| run_id >= latest_run_id) =>
            {
                self.latest_run_id = Some(run_id);
                self.settled_run_id = Some(
                    self.settled_run_id
                        .map_or(run_id, |settled| settled.max(run_id)),
                );
                self.run_state = RunState::Idle;
                self.current_step_id = None;
                self.status = "idle".to_owned();
                true
            }
            RuntimeEvent::SubmissionRejectedBusy { active_run_id }
                if self.run_state == RunState::Submitting
                    || self.run_state == RunState::Active(active_run_id) =>
            {
                self.latest_run_id = Some(
                    self.latest_run_id
                        .map_or(active_run_id, |latest| latest.max(active_run_id)),
                );
                self.run_state = RunState::Active(active_run_id);
                self.set_status(format!("run {} is busy", active_run_id.0))
            }
            _ => false,
        }
    }

    fn handle_progress(&mut self, envelope: ProgressEnvelope<RuntimeEvent>) -> bool {
        self.progress_gap |= envelope.gap_before;
        self.handle_event(envelope.item)
    }

    fn start_run(&mut self, run_id: RunId) -> bool {
        if self
            .settled_run_id
            .is_some_and(|settled_run_id| run_id <= settled_run_id)
            || self
                .latest_run_id
                .is_some_and(|latest_run_id| run_id < latest_run_id)
            || (self.latest_run_id == Some(run_id) && self.run_state != RunState::Submitting)
        {
            return false;
        }
        self.latest_run_id = Some(run_id);
        self.current_step_id = None;
        self.completed_step_id = None;
        self.run_state = RunState::Active(run_id);
        self.status = format!("run {} started", run_id.0);
        true
    }

    fn handle_step_progress(
        &mut self,
        run_id: RunId,
        step_id: ModelStepId,
        status: String,
    ) -> bool {
        if self
            .settled_run_id
            .is_some_and(|settled_run_id| run_id <= settled_run_id)
            || self
                .completed_step_id
                .is_some_and(|completed_step_id| step_id <= completed_step_id)
        {
            return false;
        }
        match self.run_state {
            RunState::Active(active_run_id) if active_run_id == run_id => {}
            RunState::Submitting
                if self
                    .latest_run_id
                    .is_none_or(|latest_run_id| run_id >= latest_run_id) =>
            {
                self.latest_run_id = Some(run_id);
                self.run_state = RunState::Active(run_id);
                self.current_step_id = None;
                self.completed_step_id = None;
            }
            RunState::Idle
                if self
                    .latest_run_id
                    .is_none_or(|latest_run_id| run_id > latest_run_id) =>
            {
                self.latest_run_id = Some(run_id);
                self.run_state = RunState::Active(run_id);
                self.current_step_id = None;
                self.completed_step_id = None;
            }
            _ => return false,
        }
        if self
            .current_step_id
            .is_some_and(|current_step_id| step_id < current_step_id)
        {
            return false;
        }
        self.current_step_id = Some(step_id);
        self.set_status(status)
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
    commands: mpsc::Sender<RuntimeCommand>,
    mut events: mpsc::UnboundedReceiver<RuntimeEvent>,
    mut progress: RuntimeProgressReceiver,
) -> Result<(), TuiError> {
    let mut app = App::default();
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

fn with_terminal<B: Backend>(
    initialize: impl FnOnce() -> Result<Terminal<B>, TuiError>,
    restore: impl FnOnce(),
    run: impl FnOnce(&mut Terminal<B>) -> Result<(), TuiError>,
) -> Result<(), TuiError> {
    let _guard = RestoreGuard::new(restore);
    let mut terminal = initialize()?;
    run(&mut terminal)
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
    app.history_height = if area.width == 0 {
        0
    } else {
        area.height.saturating_sub(2) as usize
    };
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(format!(
            "Status: {} | Enter submit | Esc cancel | Ctrl-C quit | Up/Down scroll",
            app.status
        )),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if area.height < 2 {
        return;
    }

    let prompt_y = area.y.saturating_add(area.height - 1);
    frame.render_widget(
        Paragraph::new(format!("> {}", app.prompt)),
        Rect::new(area.x, prompt_y, area.width, 1),
    );

    if area.height < 3 {
        return;
    }

    let history = format_history(app);
    let history_height = area.height - 2;
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
    app.records
        .iter()
        .map(format_record)
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_record(stored: &StoredRecord) -> String {
    let run_id = stored.run_id.0;
    match &stored.record {
        Record::UserRequest { request } => format!("[run {run_id}] you: {request}"),
        Record::CompletedExecution {
            step_id,
            source,
            execution,
            ..
        } => format_execution(run_id, step_id.0, source, execution),
        Record::FinalMessage { step_id, text, .. } => {
            format!("[run {run_id} step {}] kiki: {text}", step_id.0)
        }
        Record::CancelledRun { .. } => format!("[run {run_id}] cancelled"),
        Record::FailedRun { message, .. } => format!("[run {run_id}] failed: {message}"),
    }
}

fn format_execution(run_id: u64, step_id: u64, source: &str, execution: &Execution) -> String {
    let mut lines = vec![format!("[run {run_id} step {step_id}] code:\n{source}")];
    if execution.stdout.total_bytes > 0 {
        lines.push(format!("stdout: {}", format_observation(&execution.stdout)));
    }
    if execution.stderr.total_bytes > 0 {
        lines.push(format!("stderr: {}", format_observation(&execution.stderr)));
    }
    match &execution.outcome {
        ExecutionOutcome::Complete {
            result: Some(result),
        } => lines.push(format!("result: {}", format_observation(result))),
        ExecutionOutcome::Complete { result: None } => {}
        ExecutionOutcome::PythonError(error) => lines.push(format!("error: {}", error.text)),
    }
    lines.join("\n")
}

fn format_observation(observation: &Observation) -> String {
    let mut text = observation.head.clone();
    if let Some(tail) = &observation.tail {
        text.push_str(&format!(
            "\n[{} lines, {} bytes omitted]\n",
            observation.omitted_lines, observation.omitted_bytes
        ));
        text.push_str(tail);
    }
    text.trim_end_matches('\n').to_owned()
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
    use crate::arena::{Execution, ExecutionOutcome, Observation, ReadOutcome};
    use crate::runtime::{
        ModelStepId, ProgressEnvelope, Record, RunId, RuntimeCommand, RuntimeEvent, StoredRecord,
        TerminalDetails,
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
        assert_eq!(app.records, [stale_record]);
    }

    #[test]
    fn current_live_progress_updates_status_until_settlement() {
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
    fn committed_history_renders_chronologically_through_final_settlement() {
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
    fn completed_and_terminal_records_render_inspectable_content() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    source: "print('hello')\n2".to_owned(),
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

        let screen = rendered(&mut app, 60, 16);

        assert!(screen.contains("[run 1 step 1] code:"));
        assert!(screen.contains("print('hello')"));
        assert!(screen.contains("stdout: hello"));
        assert!(screen.contains("stderr: warning"));
        assert!(screen.contains("result: 2"));
        assert!(screen.contains("[run 1] cancelled"));
        assert!(screen.contains("[run 2] failed: model failed"));
    }

    #[test]
    fn keyboard_scrolling_moves_between_oldest_and_newest_history() {
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
    fn scrolling_without_hidden_history_does_not_request_a_redraw() {
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
    fn progress_channels_reject_stale_runs_and_steps_and_keep_terminal_authoritative() {
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
        assert!(app.progress_gap);
        assert!(app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(3),
            },
            gap_before: false,
        }));
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
        assert!(app.progress_gap);
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
    fn loop_redraws_only_for_visible_changes_and_drains_events_without_a_key() {
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
    fn rendering_is_stable_at_tiny_dimensions() {
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
    fn terminal_is_restored_after_initialisation_and_loop_errors() {
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
    fn terminal_is_restored_during_unwind() {
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
}
