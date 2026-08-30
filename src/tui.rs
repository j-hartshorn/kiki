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

use crate::config::StartupSnapshot;
use crate::runtime::{ProgressEnvelope, RuntimeCommand, RuntimeEvent, RuntimeProgressReceiver};
#[cfg(test)]
pub(crate) use crate::transcript::format_startup_configuration;
use crate::transcript::{Transcript, normalise_terminal_text};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppMode {
    Configured,
    ConfigurationOnly,
}

struct App {
    mode: AppMode,
    prompt: String,
    transcript: Transcript,
    scroll_from_bottom: usize,
    history_height: usize,
}

impl Default for App {
    fn default() -> Self {
        Self {
            mode: AppMode::Configured,
            prompt: String::new(),
            transcript: Transcript::default(),
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
            transcript: Transcript::configured(startup),
            ..Self::default()
        }
    }

    fn configuration(startup: StartupSnapshot) -> Self {
        Self {
            mode: AppMode::ConfigurationOnly,
            transcript: Transcript::configured(startup),
            ..Self::default()
        }
    }

    fn is_configuration_only(&self) -> bool {
        self.mode == AppMode::ConfigurationOnly
    }

    fn is_submission_disabled(&self) -> bool {
        self.is_configuration_only() || self.transcript.is_disabled()
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
            KeyCode::Esc if !self.transcript.is_idle() => Update {
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
            KeyCode::Enter if self.transcript.is_idle() && !self.prompt.is_empty() => {
                self.transcript.begin_submission();
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
        self.transcript.apply(event, gap_before)
    }

    fn update_scroll(&mut self, scroll_from_bottom: usize) -> Update {
        let hidden_lines = self
            .transcript
            .history()
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
    } else if app.transcript.is_disabled() {
        format!(
            "Status: {} | Input disabled | Ctrl-C quit | Up/Down scroll",
            normalise_terminal_text(app.transcript.status())
        )
    } else {
        format!(
            "Status: {} | Enter submit | Esc cancel | Ctrl-C quit | Up/Down scroll",
            normalise_terminal_text(app.transcript.status())
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

    let history = app.transcript.history();
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
        ModelStepId, Record, RunId, RuntimeCommand, RuntimeDisabledReason, RuntimeEvent,
        StoredRecord, TerminalDetails,
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

        let history = rendered(&mut app, 120, 15);
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
    fn disabled_authentication_disables_tui_input() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        assert!(app.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        }));
        assert!(rendered(&mut app, 120, 15).contains("Input disabled"));

        app.prompt = "saved prompt".to_owned();
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
        assert_eq!(app.prompt, "saved prompt");
        let shutdown = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(shutdown.command, Some(RuntimeCommand::Shutdown));
        assert!(shutdown.quit);
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

        let history = rendered(&mut app, 120, 40);
        assert!(history.contains("result: 42"));
        assert!(history.contains("error: boom"));
        assert!(!history.contains("stderr: \n"));
        assert!(history.contains("source: await read(path)"));
        assert!(history.contains("stderr: diagnostic"));
        assert!(history.contains("read: completed bad    path failed: read failed"));
        assert!(history.contains("failure: interrupted"));
        assert!(history.contains("warning: cancelled warning"));

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
        let generation_history = rendered(&mut generation, 120, 40);
        assert!(generation_history.contains("reasoning: interrupted reasoning"));
        assert!(generation_history.contains("assistant: interrupted assistant"));
        assert!(generation_history.contains("code:"));
        assert!(generation_history.contains("interrupted_code()"));
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

        let history = rendered(&mut app, 120, 40);
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
                        vec![AttemptAccounting::KnownUsageWithUntrackedRetries(
                            ModelUsage {
                                model: "gpt-5.6-sol".to_owned(),
                                service_tier: Some("default".to_owned()),
                                input_tokens: Some(0),
                                cached_input_tokens: Some(0),
                                cache_write_input_tokens: Some(0),
                                output_tokens: Some(100),
                                reasoning_output_tokens: Some(20),
                                total_tokens: Some(100),
                            },
                        )],
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

        let history = rendered(&mut app, 120, 40);
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
            rendered(&mut active, 120, 40).contains("Run estimated cost: Complete $0.0024 USD")
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
    fn normalise_hostile_prompt_and_status_before_drawing() {
        let mut app = App {
            prompt: "prompt\r\n\t\x1b]title\x07".to_owned(),
            ..App::default()
        };
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::ToolStarted {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            path: "path\u{009d}unsafe".to_owned(),
        });

        let screen = rendered(&mut app, 120, 30);
        assert!(screen.contains("reading path\\x9Dunsafe"));
        assert!(screen.contains("prompt"));
        assert!(!screen.chars().any(|character| {
            matches!(character, '\u{1b}' | '\u{7f}') || character.is_control() && character != '\n'
        }));
    }
}
