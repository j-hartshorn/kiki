use std::io;
use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::clipboard::{ClipboardPort, LocalClipboard};
use crate::config::StartupSnapshot;
use crate::prompt_editor::{PromptEdit, PromptEditor, PromptLayout};
use crate::runtime::{ProgressEnvelope, RuntimeCommand, RuntimeEvent, RuntimeProgressReceiver};
use crate::selection::{DocumentPosition, Selection};
#[cfg(test)]
pub(crate) use crate::transcript::format_startup_configuration;
use crate::transcript::{Transcript, TranscriptLayout, normalise_terminal_text, page_style};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MOUSE_WHEEL_SCROLL_ROWS: usize = 3;

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

#[derive(Clone, Copy)]
struct HistoryViewport {
    area: Rect,
}

impl HistoryViewport {
    fn position(self, column: u16, row: u16, document_top: usize) -> Option<DocumentPosition> {
        (column >= self.area.x
            && column < self.area.right()
            && row >= self.area.y
            && row < self.area.bottom())
        .then(|| {
            DocumentPosition::new(
                document_top.saturating_add(usize::from(row.saturating_sub(self.area.y))),
                column,
            )
        })
    }

    fn edge(self, row: u16) -> Option<Edge> {
        if row == self.area.top() {
            Some(Edge::Older)
        } else if row == self.area.bottom().saturating_sub(1) {
            Some(Edge::Newer)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
enum Edge {
    Older,
    Newer,
}

#[derive(Clone, Copy)]
struct Drag {
    anchor: DocumentPosition,
    column: u16,
    row: u16,
    edge: Option<Edge>,
    next_tick: Instant,
}

#[derive(Clone, Copy)]
enum Notice {
    Copied,
    CopyFailed,
}

struct Flash {
    next_tick: Instant,
    transitions: u8,
    highlighted: bool,
}

struct App {
    mode: AppMode,
    enhanced_keyboard: bool,
    prompt: PromptEditor,
    prompt_width: u16,
    transcript: Transcript,
    scroll_from_bottom: usize,
    history_height: usize,
    history_rows: usize,
    history_viewport: Option<HistoryViewport>,
    selection: Option<Selection>,
    drag: Option<Drag>,
    pending_copy: bool,
    pending_copy_text: Option<String>,
    notice: Option<Notice>,
    flash: Option<Flash>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            mode: AppMode::Configured,
            enhanced_keyboard: false,
            prompt: PromptEditor::default(),
            prompt_width: 80,
            transcript: Transcript::default(),
            scroll_from_bottom: 0,
            history_height: 0,
            history_rows: 0,
            history_viewport: None,
            selection: None,
            drag: None,
            pending_copy: false,
            pending_copy_text: None,
            notice: None,
            flash: None,
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

    fn handle_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Update {
        let notice_cleared = self.clear_notice();
        let mut update = match mouse.kind {
            MouseEventKind::ScrollUp => self.update_scroll(
                self.scroll_from_bottom
                    .saturating_add(MOUSE_WHEEL_SCROLL_ROWS),
            ),
            MouseEventKind::ScrollDown => self.update_scroll(
                self.scroll_from_bottom
                    .saturating_sub(MOUSE_WHEEL_SCROLL_ROWS),
            ),
            MouseEventKind::Down(MouseButton::Left) => self.begin_selection(mouse, now),
            MouseEventKind::Drag(MouseButton::Left) => Update {
                command: None,
                visible_changed: self.update_drag(mouse, now),
                quit: false,
            },
            MouseEventKind::Up(MouseButton::Left) => self.finish_selection(mouse, now),
            _ => Update::default(),
        };
        update.visible_changed |= notice_cleared;
        update
    }

    fn handle_key(&mut self, key: KeyEvent) -> Update {
        let notice_cleared = self.clear_notice();
        let mut update = self.handle_key_inner(key);
        update.visible_changed |= notice_cleared;
        update
    }

    fn handle_paste(&mut self, text: &str) -> Update {
        let notice_cleared = self.clear_notice();
        let visible_changed = if self.is_submission_disabled() {
            false
        } else {
            let text = normalise_terminal_text(&normalise_paste_line_endings(text));
            self.prompt.insert_paste(&text)
        };
        Update {
            command: None,
            visible_changed: visible_changed || notice_cleared,
            quit: false,
        }
    }

    fn handle_key_inner(&mut self, key: KeyEvent) -> Update {
        if key.is_release() {
            return Update::default();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c' | 'C') => {
                    if !self.is_submission_disabled() && !self.prompt.is_empty() {
                        self.prompt.take();
                        return Update {
                            command: None,
                            visible_changed: true,
                            quit: false,
                        };
                    }
                    return Update {
                        command: (!self.is_configuration_only())
                            .then_some(RuntimeCommand::Shutdown),
                        visible_changed: false,
                        quit: true,
                    };
                }
                KeyCode::Char('t' | 'T') => {
                    let visible_changed = self.transcript.toggle_reasoning();
                    if visible_changed {
                        self.invalidate_selection();
                    }
                    return Update {
                        command: None,
                        visible_changed,
                        quit: false,
                    };
                }
                KeyCode::Char('o' | 'O') => {
                    let visible_changed = self.transcript.toggle_arena_expansion();
                    if visible_changed {
                        self.invalidate_selection();
                    }
                    return Update {
                        command: None,
                        visible_changed,
                        quit: false,
                    };
                }
                _ => {}
            }
        }

        if let Some(update) = self.handle_history_key(key) {
            return update;
        }
        if self.is_submission_disabled() {
            return Update::default();
        }
        if key.code == KeyCode::Esc && !self.transcript.is_idle() {
            return Update {
                command: Some(RuntimeCommand::Cancel),
                visible_changed: false,
                quit: false,
            };
        }
        if let Some(edit) = self.prompt_edit(key) {
            return Update {
                command: None,
                visible_changed: self.prompt.apply(edit, self.prompt_width),
                quit: false,
            };
        }
        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && self.transcript.is_idle()
            && !self.prompt.is_empty()
        {
            self.transcript.begin_submission();
            return Update {
                command: Some(RuntimeCommand::Submit {
                    request: self.prompt.take(),
                }),
                visible_changed: true,
                quit: false,
            };
        }
        Update::default()
    }

    fn begin_selection(&mut self, mouse: MouseEvent, now: Instant) -> Update {
        let visible_changed = self.invalidate_selection();
        let Some(position) = self.history_position(mouse.column, mouse.row) else {
            return Update {
                command: None,
                visible_changed,
                quit: false,
            };
        };
        self.drag = Some(Drag {
            anchor: position,
            column: mouse.column,
            row: mouse.row,
            edge: None,
            next_tick: now + POLL_INTERVAL,
        });
        Update {
            command: None,
            visible_changed,
            quit: false,
        }
    }

    fn update_drag(&mut self, mouse: MouseEvent, now: Instant) -> bool {
        let Some(viewport) = self.history_viewport else {
            return false;
        };
        let Some(focus) = self.history_position(mouse.column, mouse.row) else {
            if let Some(drag) = &mut self.drag {
                drag.edge = None;
            }
            return false;
        };
        let Some(drag) = &mut self.drag else {
            return false;
        };
        drag.column = mouse.column;
        drag.row = mouse.row;
        drag.edge = viewport.edge(mouse.row);
        drag.next_tick = now + POLL_INTERVAL;
        let selection = Selection::from_inclusive_cells(drag.anchor, focus);
        let visible_changed = self.selection != selection;
        self.selection = selection;
        visible_changed
    }

    fn finish_selection(&mut self, mouse: MouseEvent, now: Instant) -> Update {
        let visible_changed = self.update_drag(mouse, now);
        if self.drag.take().is_some() && self.selection.is_some() {
            self.pending_copy = true;
            return Update {
                command: None,
                visible_changed: true,
                quit: false,
            };
        }
        Update {
            command: None,
            visible_changed,
            quit: false,
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        self.tick_edge_scroll(now) | self.tick_flash(now)
    }

    fn tick_edge_scroll(&mut self, now: Instant) -> bool {
        let Some(drag) = self.drag else {
            return false;
        };
        let Some(edge) = drag.edge else {
            return false;
        };
        if now < drag.next_tick {
            return false;
        }
        let scroll = match edge {
            Edge::Older => self.scroll_from_bottom.saturating_add(1),
            Edge::Newer => self.scroll_from_bottom.saturating_sub(1),
        };
        if !self.update_scroll(scroll).visible_changed {
            if let Some(drag) = &mut self.drag {
                drag.edge = None;
            }
            return false;
        }
        let focus = self
            .history_position(drag.column, drag.row)
            .expect("drag position remains within history viewport");
        self.selection = Selection::from_inclusive_cells(drag.anchor, focus);
        if let Some(drag) = &mut self.drag {
            drag.next_tick = now + POLL_INTERVAL;
        }
        true
    }

    fn tick_flash(&mut self, now: Instant) -> bool {
        let Some(flash) = &mut self.flash else {
            return false;
        };
        if now < flash.next_tick {
            return false;
        }
        flash.highlighted = !flash.highlighted;
        flash.transitions = flash.transitions.saturating_add(1);
        if flash.transitions == 4 {
            self.flash = None;
        } else {
            flash.next_tick = now + Duration::from_millis(50);
        }
        true
    }

    fn history_document_top(&self) -> usize {
        self.history_rows
            .saturating_sub(self.history_height)
            .saturating_sub(self.scroll_from_bottom)
    }

    fn history_position(&self, column: u16, row: u16) -> Option<DocumentPosition> {
        self.history_viewport?
            .position(column, row, self.history_document_top())
    }

    fn selection_for_render(&self) -> Option<Selection> {
        self.flash
            .as_ref()
            .is_none_or(|flash| flash.highlighted)
            .then_some(self.selection)
            .flatten()
    }

    fn finish_pending_copy(&mut self, clipboard: &mut impl ClipboardPort, now: Instant) -> bool {
        let Some(text) = self.pending_copy_text.take() else {
            return false;
        };
        if clipboard.write_text(&text).is_ok() {
            self.notice = Some(Notice::Copied);
            self.flash = Some(Flash {
                next_tick: now + Duration::from_millis(50),
                transitions: 0,
                highlighted: true,
            });
        } else {
            self.notice = Some(Notice::CopyFailed);
            self.flash = None;
        }
        true
    }

    fn notice(&self) -> Option<&'static str> {
        match self.notice {
            Some(Notice::Copied) => Some("Copied"),
            Some(Notice::CopyFailed) => Some("Copy failed"),
            None => None,
        }
    }

    fn clear_notice(&mut self) -> bool {
        self.notice.take().is_some()
    }

    fn invalidate_selection(&mut self) -> bool {
        let visible_changed =
            self.selection.is_some() || self.notice.is_some() || self.flash.is_some();
        self.selection = None;
        self.drag = None;
        self.pending_copy = false;
        self.pending_copy_text = None;
        self.notice = None;
        self.flash = None;
        visible_changed
    }

    fn history_unavailable(&mut self) {
        self.history_height = 0;
        self.history_viewport = None;
        self.invalidate_selection();
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> Option<Update> {
        let editable = !self.is_submission_disabled();
        let scroll = match key.code {
            KeyCode::PageUp => Some(self.scroll_from_bottom.saturating_add(10)),
            KeyCode::PageDown => Some(self.scroll_from_bottom.saturating_sub(10)),
            KeyCode::Home if !editable || key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(usize::MAX)
            }
            KeyCode::End if !editable || key.modifiers.contains(KeyModifiers::CONTROL) => Some(0),
            KeyCode::Up if !editable => Some(self.scroll_from_bottom.saturating_add(1)),
            KeyCode::Down if !editable => Some(self.scroll_from_bottom.saturating_sub(1)),
            _ => None,
        };
        scroll.map(|scroll| self.update_scroll(scroll))
    }

    fn prompt_edit(&self, key: KeyEvent) -> Option<PromptEdit> {
        let command = key
            .modifiers
            .intersects(KeyModifiers::SUPER | KeyModifiers::META);
        match key.code {
            KeyCode::Left if command => Some(PromptEdit::LineStart),
            KeyCode::Right if command => Some(PromptEdit::LineEnd),
            KeyCode::Up if command => Some(PromptEdit::PromptStart),
            KeyCode::Down if command => Some(PromptEdit::PromptEnd),
            KeyCode::Left
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                Some(PromptEdit::WordLeft)
            }
            KeyCode::Right
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) =>
            {
                Some(PromptEdit::WordRight)
            }
            KeyCode::Left if key.modifiers.is_empty() => Some(PromptEdit::Left),
            KeyCode::Right if key.modifiers.is_empty() => Some(PromptEdit::Right),
            KeyCode::Up if key.modifiers.is_empty() => Some(PromptEdit::Up),
            KeyCode::Down if key.modifiers.is_empty() => Some(PromptEdit::Down),
            KeyCode::Home if key.modifiers.is_empty() => Some(PromptEdit::LineStart),
            KeyCode::End if key.modifiers.is_empty() => Some(PromptEdit::LineEnd),
            KeyCode::Backspace if command => Some(PromptEdit::DeleteToLineStart),
            KeyCode::Backspace if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(PromptEdit::DeletePreviousWord)
            }
            KeyCode::Backspace => Some(PromptEdit::DeletePreviousGrapheme),
            KeyCode::Delete if command => Some(PromptEdit::DeleteToLineEnd),
            KeyCode::Delete => Some(PromptEdit::DeleteNextGrapheme),
            KeyCode::Char('a' | 'A') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::LineStart)
            }
            KeyCode::Char('b' | 'B') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::Left)
            }
            KeyCode::Char('d' | 'D') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::DeleteNextGrapheme)
            }
            KeyCode::Char('e' | 'E') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::LineEnd)
            }
            KeyCode::Char('f' | 'F') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::Right)
            }
            KeyCode::Char('k' | 'K') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::DeleteToLineEnd)
            }
            KeyCode::Char('w' | 'W') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::DeletePreviousWord)
            }
            KeyCode::Char('u' | 'U') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(PromptEdit::DeleteToLineStart)
            }
            KeyCode::Char('b' | 'B') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(PromptEdit::WordLeft)
            }
            KeyCode::Char('d' | 'D') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(PromptEdit::DeleteNextWord)
            }
            KeyCode::Char('f' | 'F') if key.modifiers.contains(KeyModifiers::ALT) => {
                Some(PromptEdit::WordRight)
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META,
                ) =>
            {
                Some(PromptEdit::Insert(character))
            }
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT) && self.enhanced_keyboard =>
            {
                Some(PromptEdit::InsertNewline)
            }
            _ => None,
        }
    }

    fn handle_event(&mut self, event: RuntimeEvent) -> bool {
        self.handle_event_with_gap(event, false)
    }

    fn handle_progress(&mut self, envelope: ProgressEnvelope<RuntimeEvent>) -> bool {
        self.handle_event_with_gap(envelope.item, envelope.gap_before)
    }

    fn handle_event_with_gap(&mut self, event: RuntimeEvent, gap_before: bool) -> bool {
        let visible_changed = self.transcript.apply(event, gap_before);
        if visible_changed {
            self.invalidate_selection();
        }
        visible_changed
    }

    fn update_scroll(&mut self, scroll_from_bottom: usize) -> Update {
        let hidden_lines = self.history_rows.saturating_sub(self.history_height);
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

fn normalise_paste_line_endings(text: &str) -> String {
    let mut normalised = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            normalised.push('\n');
        } else {
            normalised.push(character);
        }
    }
    normalised
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
    let mut clipboard = LocalClipboard::new();
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| {
            with_bracketed_paste(enable_bracketed_paste, disable_bracketed_paste, || {
                with_mouse_capture(enable_mouse_capture, disable_mouse_capture, || {
                    with_keyboard_enhancement(
                        enable_keyboard_enhancement,
                        disable_keyboard_enhancement,
                        |enhanced_keyboard| {
                            app.enhanced_keyboard = enhanced_keyboard;
                            run_loop(
                                terminal,
                                &mut app,
                                &mut input,
                                &commands,
                                &mut events,
                                &mut progress,
                                &mut clipboard,
                            )
                            .map(|_| ())
                        },
                    )
                })
            })
        },
    )
}

pub(crate) fn run_configuration(startup: StartupSnapshot) -> Result<(), TuiError> {
    let mut app = App::configuration(startup);
    let mut input = CrosstermEvents;
    let mut clipboard = LocalClipboard::new();
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| {
            with_mouse_capture(enable_mouse_capture, disable_mouse_capture, || {
                run_configuration_loop(terminal, &mut app, &mut input, &mut clipboard).map(|_| ())
            })
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

fn enable_bracketed_paste() -> io::Result<()> {
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(stdout, EnableBracketedPaste)
}

fn disable_bracketed_paste() {
    let mut stdout = io::stdout();
    let _ = ratatui::crossterm::execute!(stdout, DisableBracketedPaste);
}

fn with_bracketed_paste<T>(
    enable: impl FnOnce() -> io::Result<()>,
    disable: impl FnOnce(),
    run: impl FnOnce() -> Result<T, TuiError>,
) -> Result<T, TuiError> {
    if let Err(error) = enable() {
        disable();
        return Err(TuiError::Terminal(error.to_string()));
    }
    let _guard = RestoreGuard::new(disable);
    run()
}

fn enable_mouse_capture() -> io::Result<()> {
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(stdout, EnableMouseCapture)
}

fn disable_mouse_capture() {
    let mut stdout = io::stdout();
    let _ = ratatui::crossterm::execute!(stdout, DisableMouseCapture);
}

fn with_mouse_capture<T>(
    enable: impl FnOnce() -> io::Result<()>,
    disable: impl FnOnce(),
    run: impl FnOnce() -> Result<T, TuiError>,
) -> Result<T, TuiError> {
    if let Err(error) = enable() {
        disable();
        return Err(TuiError::Terminal(error.to_string()));
    }
    let _guard = RestoreGuard::new(disable);
    run()
}

fn enable_keyboard_enhancement() -> io::Result<bool> {
    if !ratatui::crossterm::terminal::supports_keyboard_enhancement()? {
        return Ok(false);
    }
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    Ok(true)
}

fn disable_keyboard_enhancement() {
    let mut stdout = io::stdout();
    let _ = ratatui::crossterm::execute!(stdout, PopKeyboardEnhancementFlags);
}

fn with_keyboard_enhancement<T>(
    enable: impl FnOnce() -> io::Result<bool>,
    disable: impl FnOnce(),
    run: impl FnOnce(bool) -> T,
) -> T {
    let enabled = enable().unwrap_or(false);
    let _guard = enabled.then(|| RestoreGuard::new(disable));
    run(enabled)
}

fn draw_frame<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    clipboard: &mut impl ClipboardPort,
) -> Result<bool, TuiError> {
    terminal
        .draw(|frame| render(frame, app))
        .map_err(|error| TuiError::Terminal(error.to_string()))?;
    Ok(app.finish_pending_copy(clipboard, Instant::now()))
}

fn run_configuration_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    input: &mut impl EventSource,
    clipboard: &mut impl ClipboardPort,
) -> Result<usize, TuiError> {
    let mut dirty = true;
    let mut redraws = 0;

    loop {
        if dirty {
            dirty = draw_frame(terminal, app, clipboard)?;
            redraws += 1;
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
                Event::Mouse(mouse) => {
                    dirty |= app.handle_mouse(mouse, Instant::now()).visible_changed
                }
                Event::Resize(_, _) => {
                    app.invalidate_selection();
                    dirty = true;
                }
                Event::Paste(text) => dirty |= app.handle_paste(&text).visible_changed,
                _ => {}
            }
        }

        dirty |= app.tick(Instant::now());
    }
}

fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    input: &mut impl EventSource,
    commands: &mpsc::Sender<RuntimeCommand>,
    events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    progress: &mut RuntimeProgressReceiver,
    clipboard: &mut impl ClipboardPort,
) -> Result<usize, TuiError> {
    let mut dirty = true;
    let mut redraws = 0;
    let mut progress_open = true;

    loop {
        if dirty {
            dirty = draw_frame(terminal, app, clipboard)?;
            redraws += 1;
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
                Event::Mouse(mouse) => {
                    dirty |= app.handle_mouse(mouse, Instant::now()).visible_changed
                }
                Event::Resize(_, _) => {
                    app.invalidate_selection();
                    dirty = true;
                }
                Event::Paste(text) => dirty |= app.handle_paste(&text).visible_changed,
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

        dirty |= app.tick(Instant::now());
    }
}

const PROMPT_GUTTER_WIDTH: u16 = 2;

fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.buffer_mut().set_style(area, page_style());
    if area.width == 0 || area.height == 0 {
        app.history_unavailable();
        return;
    }

    let configuration_only = app.is_configuration_only();
    let notice = app.notice();
    let header = if configuration_only {
        notice.map_or_else(
            || {
                "Configuration only: read-only; submission unavailable | Ctrl-C quit | Up/Down scroll"
                    .to_owned()
            },
            |notice| {
                format!(
                    "{notice} | Configuration only: read-only; submission unavailable | Ctrl-C quit | Up/Down scroll"
                )
            },
        )
    } else if app.transcript.is_disabled() {
        let notice = notice.map_or(String::new(), |notice| format!(" | {notice}"));
        format!(
            "Status: {}{notice} | Input disabled | Up/Down scroll | Ctrl-T reasoning | Ctrl-O expand | Ctrl-C quit",
            app.transcript.status()
        )
    } else {
        let enhanced_keyboard_hint = if app.enhanced_keyboard {
            " | Shift-Enter newline"
        } else {
            ""
        };
        let notice = notice.map_or(String::new(), |notice| format!(" | {notice}"));
        format!(
            "Status: {}{notice} | Enter submit{enhanced_keyboard_hint} | Arrows edit | PgUp/PgDn scroll | Esc cancel | Ctrl-T reasoning | Ctrl-O expand | Ctrl-C clear/quit",
            app.transcript.status()
        )
    };
    frame.render_widget(
        Paragraph::new(normalise_terminal_text(&header)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if configuration_only {
        if area.height == 1 {
            app.history_unavailable();
            return;
        }
        let _ = render_history(
            frame,
            app,
            Rect::new(
                area.x,
                area.y.saturating_add(1),
                area.width,
                area.height.saturating_sub(1),
            ),
        );
        return;
    }

    if area.height == 1 {
        app.history_unavailable();
        return;
    }

    let bottom_rule = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        1,
    );
    if area.height == 2 {
        render_rule(frame, bottom_rule);
        app.history_unavailable();
        return;
    }
    if area.height == 3 {
        render_rule(
            frame,
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        );
        render_rule(frame, bottom_rule);
        app.history_unavailable();
        return;
    }

    app.prompt_width = area.width.saturating_sub(PROMPT_GUTTER_WIDTH);
    let prompt = app
        .prompt
        .layout(app.prompt_width, prompt_content_capacity(area.height));
    let prompt_box_height = u16::try_from(prompt.rows().len().saturating_add(2))
        .unwrap_or(area.height.saturating_sub(1));
    let prompt_top = area
        .y
        .saturating_add(area.height.saturating_sub(prompt_box_height));
    let content = Rect::new(
        area.x,
        prompt_top.saturating_add(1),
        area.width,
        prompt_box_height.saturating_sub(2),
    );
    let history = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height
            .saturating_sub(1)
            .saturating_sub(prompt_box_height),
    );
    let layout = render_history(frame, app, history);
    render_rule(frame, Rect::new(area.x, prompt_top, area.width, 1));
    render_prompt(frame, &prompt, content, !app.is_submission_disabled());
    frame.render_widget(
        Paragraph::new(layout.footer_rule(bottom_rule.width)),
        bottom_rule,
    );
}

fn render_history(frame: &mut Frame, app: &mut App, viewport: Rect) -> TranscriptLayout {
    let layout = app.transcript.layout(viewport.width);
    if viewport.is_empty() {
        app.history_unavailable();
        return layout;
    }
    if app
        .history_viewport
        .is_some_and(|history| history.area != viewport)
    {
        app.invalidate_selection();
    }
    app.history_height = usize::from(viewport.height);
    app.history_rows = layout.total_height();
    let last_page_start = app.history_rows.saturating_sub(app.history_height);
    app.scroll_from_bottom = app.scroll_from_bottom.min(last_page_start);
    let top = app.history_document_top();
    app.history_viewport = Some(HistoryViewport { area: viewport });
    if let Some(selection) = app.selection_for_render() {
        layout.render_with_selection(viewport, top, Some(selection), frame.buffer_mut());
    } else {
        layout.render(viewport, top, frame.buffer_mut());
    }
    if app.pending_copy {
        app.pending_copy = false;
        let text = app
            .selection
            .map(|selection| layout.extract_selection(selection))
            .unwrap_or_default();
        if text.is_empty() {
            app.invalidate_selection();
        } else {
            app.pending_copy_text = Some(text);
        }
    }
    layout
}

fn prompt_content_capacity(height: u16) -> usize {
    let prompt_box_capacity = usize::from(height)
        .saturating_div(3)
        .max(3)
        .min(usize::from(height.saturating_sub(1)));
    prompt_box_capacity.saturating_sub(2).max(1)
}

fn render_prompt(frame: &mut Frame, prompt: &PromptLayout, content: Rect, cursor_visible: bool) {
    if content.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new("> "),
        Rect::new(
            content.x,
            content.y,
            content.width.min(PROMPT_GUTTER_WIDTH),
            1,
        ),
    );
    let text_width = content.width.saturating_sub(PROMPT_GUTTER_WIDTH);
    if text_width == 0 {
        return;
    }
    let text: Text<'_> = prompt
        .rows()
        .iter()
        .map(String::as_str)
        .map(Line::from)
        .collect();
    frame.render_widget(
        Paragraph::new(text),
        Rect::new(
            content.x.saturating_add(PROMPT_GUTTER_WIDTH),
            content.y,
            text_width,
            content.height,
        ),
    );
    if cursor_visible
        && let Some((row, column)) = prompt.cursor()
        && let (Ok(row), Ok(column)) = (u16::try_from(row), u16::try_from(column))
        && row < content.height
        && column < text_width
    {
        frame.set_cursor_position((
            content
                .x
                .saturating_add(PROMPT_GUTTER_WIDTH)
                .saturating_add(column),
            content.y.saturating_add(row),
        ));
    }
}

fn render_rule(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::raw("─".repeat(usize::from(area.width)))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::time::{Duration, Instant};

    use ratatui::Terminal;
    use ratatui::backend::{Backend, TestBackend};
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    };
    use ratatui::layout::Position;
    use ratatui::style::Color;

    use super::App;
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, Observation, PythonError, ToolCall, ToolCallSummary,
        ToolCallToken, ToolName,
    };
    use crate::clipboard::{ClipboardError, ClipboardPort};
    use crate::model::{AttemptAccounting, ModelProgress, ModelUsage};
    use crate::prompt_editor::{MAX_PROMPT_BYTES, PromptEdit, PromptEditor};
    use crate::runtime::{
        ModelStepId, Record, RunId, RuntimeCommand, RuntimeDisabledReason, RuntimeEvent,
        StoredRecord, TerminalDetails,
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shifted_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn history_home() -> KeyEvent {
        modified_key(KeyCode::Home, KeyModifiers::CONTROL)
    }

    fn history_end() -> KeyEvent {
        modified_key(KeyCode::End, KeyModifiers::CONTROL)
    }

    fn type_prompt(app: &mut App, text: &str) {
        for character in text.chars() {
            assert_ne!(character, '\n');
            assert!(
                app.handle_key(key(KeyCode::Char(character)))
                    .visible_changed
            );
        }
    }

    fn set_prompt(app: &mut App, text: &str) {
        app.prompt = PromptEditor::default();
        for character in text.chars() {
            let edit = if character == '\n' {
                PromptEdit::InsertNewline
            } else {
                PromptEdit::Insert(character)
            };
            assert!(app.prompt.apply(edit, app.prompt_width));
        }
    }

    fn app_with_prompt(text: &str) -> App {
        let mut app = App::default();
        set_prompt(&mut app, text);
        app
    }

    fn submitted_prompt(app: &mut App) -> String {
        let Some(RuntimeCommand::Submit { request }) = app.handle_key(key(KeyCode::Enter)).command
        else {
            panic!("prompt was not submitted")
        };
        request
    }

    fn cursor(app: &mut App, width: u16, height: u16) -> (bool, Position) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::render(frame, app)).unwrap();
        let visible = terminal.backend().cursor_visible();
        let position = terminal.backend_mut().get_cursor_position().unwrap();
        (visible, position)
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

    fn rendered_buffer(app: &mut App, width: u16, height: u16) -> Buffer {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| super::render(frame, app)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn cell_start(buffer: &Buffer, text: &str) -> (u16, u16) {
        for row in buffer.area.top()..buffer.area.bottom() {
            let rendered = (buffer.area.left()..buffer.area.right())
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>();
            if let Some(column) = rendered.find(text) {
                return (u16::try_from(column).expect("test terminal column"), row);
            }
        }
        panic!("rendered text: {text}")
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

    struct FakeClipboard {
        writes: Vec<String>,
        result: Result<(), ClipboardError>,
    }

    impl FakeClipboard {
        fn available() -> Self {
            Self {
                writes: Vec::new(),
                result: Ok(()),
            }
        }
    }

    impl ClipboardPort for FakeClipboard {
        fn write_text(&mut self, text: &str) -> Result<(), ClipboardError> {
            self.writes.push(text.to_owned());
            self.result
        }
    }

    fn app_with_history(mut app: App) -> App {
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
        app
    }

    fn mouse(kind: MouseEventKind) -> Event {
        mouse_at(kind, 0, 0)
    }

    fn mouse_at(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(mouse_event(kind, column, row))
    }

    fn mouse_event(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn quit_event() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
    }

    fn run_transcript_loop(app: &mut App, input: &mut FakeInput) -> usize {
        let mut clipboard = FakeClipboard::available();
        run_transcript_loop_with_clipboard(app, input, &mut clipboard)
    }

    fn run_transcript_loop_with_clipboard(
        app: &mut App,
        input: &mut FakeInput,
        clipboard: &mut FakeClipboard,
    ) -> usize {
        let backend = TestBackend::new(40, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        let (command_sender, _command_receiver) = tokio::sync::mpsc::channel(8);
        let (_event_sender, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (_progress_sender, mut progress_receiver) = tokio::sync::mpsc::channel(256);

        super::run_loop(
            &mut terminal,
            app,
            input,
            &command_sender,
            &mut event_receiver,
            &mut progress_receiver,
            clipboard,
        )
        .unwrap()
    }

    fn run_configuration_loop(app: &mut App, input: &mut FakeInput) -> usize {
        let mut clipboard = FakeClipboard::available();
        run_configuration_loop_with_clipboard(app, input, &mut clipboard)
    }

    fn run_configuration_loop_with_clipboard(
        app: &mut App,
        input: &mut FakeInput,
        clipboard: &mut FakeClipboard,
    ) -> usize {
        run_configuration_loop_with_clipboard_at(app, input, clipboard, 40, 7)
    }

    fn run_configuration_loop_with_clipboard_at(
        app: &mut App,
        input: &mut FakeInput,
        clipboard: &mut FakeClipboard,
        width: u16,
        height: u16,
    ) -> usize {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();

        super::run_configuration_loop(&mut terminal, app, input, clipboard).unwrap()
    }

    fn app_with_pending_copy(text: &str) -> (App, Instant) {
        let mut app = App::default();
        let now = queue_copy(&mut app, text, 60, 12);
        (app, now)
    }

    fn queue_copy(app: &mut App, text: &str, width: u16, height: u16) -> Instant {
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: text.to_owned(),
                },
            },
            progress_gap: false,
        });
        let buffer = rendered_buffer(app, width, height);
        let (column, row) = cell_start(&buffer, text);
        let end = column + u16::try_from(text.len() - 1).unwrap();
        let now = Instant::now();
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), column, row),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Drag(MouseButton::Left), end, row),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), end, row),
            now,
        );
        rendered_buffer(app, width, height);
        now
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
        assert_eq!(app.prompt.text(), "");
        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
    }

    #[test]
    fn submission_requires_unmodified_enter() {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::META,
            KeyModifiers::ALT,
        ] {
            let mut app = app_with_prompt("keep");

            let update = app.handle_key(modified_key(KeyCode::Enter, modifiers));

            assert!(update.command.is_none());
            assert!(!update.visible_changed);
            assert_eq!(app.prompt.text(), "keep");
        }

        let mut app = app_with_prompt("send");
        let update = app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            update.command,
            Some(RuntimeCommand::Submit {
                request: "send".to_owned(),
            })
        );
        assert!(update.visible_changed);
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn shift_enter_edits_a_multiline_prompt_without_submitting() {
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.handle_key(shifted_key(KeyCode::Enter)).command, None);
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.prompt.text(), "f\ng");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).command,
            Some(RuntimeCommand::Submit {
                request: "f\ng".to_owned(),
            })
        );

        let mut active = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        assert!(
            active
                .handle_key(shifted_key(KeyCode::Enter))
                .visible_changed
        );
        assert_eq!(active.prompt.text(), "\n");
        assert!(active.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(active.prompt.text(), "\nx");
        assert!(active.handle_key(key(KeyCode::Enter)).command.is_none());

        let mut disabled = App::default();
        disabled.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });
        set_prompt(&mut disabled, "locked");
        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
            shifted_key(KeyCode::Enter),
        ] {
            assert!(!disabled.handle_key(event).visible_changed);
        }
        assert_eq!(disabled.prompt.text(), "locked");

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, "locked");
        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
            shifted_key(KeyCode::Enter),
        ] {
            assert!(!configuration.handle_key(event).visible_changed);
        }
        assert_eq!(configuration.prompt.text(), "locked");

        let mut bounded = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        set_prompt(&mut bounded, &"a".repeat(MAX_PROMPT_BYTES - 1));
        assert!(
            bounded
                .handle_key(shifted_key(KeyCode::Enter))
                .visible_changed
        );
        assert_eq!(bounded.prompt.text().len(), MAX_PROMPT_BYTES);
        let limit_reached = bounded.handle_key(shifted_key(KeyCode::Enter));
        assert!(limit_reached.command.is_none());
        assert!(!limit_reached.visible_changed);
        assert!(!bounded.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert!(bounded.handle_key(key(KeyCode::Backspace)).visible_changed);
        assert_eq!(bounded.prompt.text(), "a".repeat(MAX_PROMPT_BYTES - 1));

        let before_release = active.prompt.text().to_owned();
        let release =
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::SHIFT, KeyEventKind::Release);
        assert!(!active.handle_key(release).visible_changed);
        assert_eq!(active.prompt.text(), before_release);
    }

    #[test]
    fn prompt_enforces_its_byte_limit_and_backspaces_unicode() {
        let mut app = app_with_prompt(&"a".repeat(MAX_PROMPT_BYTES - '🦀'.len_utf8()));

        assert!(app.handle_key(key(KeyCode::Char('🦀'))).visible_changed);
        assert_eq!(app.prompt.text().len(), MAX_PROMPT_BYTES);
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(app.prompt.text().len(), MAX_PROMPT_BYTES);

        assert!(app.handle_key(key(KeyCode::Backspace)).visible_changed);
        assert!(!app.prompt.text().ends_with('🦀'));
        assert!(app.prompt.text().is_char_boundary(app.prompt.text().len()));
    }

    #[test]
    fn paste_normalises_before_splicing_and_accounting_for_capacity() {
        let mut app = app_with_prompt("α\nγ");
        assert!(app.handle_key(key(KeyCode::Left)).visible_changed);

        let update = app.handle_paste("β\r\n\t\x1b\rδ\u{009d}");

        assert!(update.command.is_none());
        assert!(!update.quit);
        assert!(update.visible_changed);
        assert_eq!(app.prompt.text(), "α\nβ\n    \\x1B\nδ\\x9Dγ");

        let mut bounded = app_with_prompt(&"a".repeat(MAX_PROMPT_BYTES - 3));
        let update = bounded.handle_paste("\t");
        assert!(update.command.is_none());
        assert!(update.visible_changed);
        assert_eq!(
            bounded.prompt.text(),
            format!("{}   ", "a".repeat(MAX_PROMPT_BYTES - 3))
        );
    }

    #[test]
    fn paste_edits_idle_and_active_drafts_without_commands() {
        let mut idle = App::default();
        let idle_update = idle.handle_paste("idle draft");
        assert!(idle_update.visible_changed);
        assert!(idle_update.command.is_none());
        assert!(!idle_update.quit);
        assert_eq!(idle.prompt.text(), "idle draft");

        let mut active = App::default();
        assert!(active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) }));
        let active_update = active.handle_paste("active draft");
        assert!(active_update.visible_changed);
        assert!(active_update.command.is_none());
        assert!(!active_update.quit);
        assert_eq!(active.prompt.text(), "active draft");
    }

    #[test]
    fn empty_or_uninsertable_paste_is_a_visible_noop_without_notice() {
        let mut empty = app_with_prompt("keep");
        let empty_update = empty.handle_paste("");
        assert!(empty_update.command.is_none());
        assert!(!empty_update.visible_changed);
        assert_eq!(empty.prompt.text(), "keep");

        let mut full = app_with_prompt(&"a".repeat(MAX_PROMPT_BYTES));
        let full_update = full.handle_paste("x");
        assert!(full_update.command.is_none());
        assert!(!full_update.visible_changed);
        assert_eq!(full.prompt.text().len(), MAX_PROMPT_BYTES);
    }

    #[test]
    fn paste_is_read_only_in_configuration_and_disabled_views() {
        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, "locked");
        let configuration_update = configuration.handle_paste("discarded");
        assert!(configuration_update.command.is_none());
        assert!(!configuration_update.visible_changed);
        assert_eq!(configuration.prompt.text(), "locked");

        let mut disabled = App::default();
        set_prompt(&mut disabled, "locked");
        disabled.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });
        let disabled_update = disabled.handle_paste("discarded");
        assert!(disabled_update.command.is_none());
        assert!(!disabled_update.visible_changed);
        assert_eq!(disabled.prompt.text(), "locked");
    }

    #[test]
    fn paste_redraws_once_only_when_it_changes_visible_state() {
        let mut inserted = App::default();
        let mut inserted_input = FakeInput::new([
            Some(Event::Paste("bulk draft".to_owned())),
            Some(quit_event()),
            Some(quit_event()),
        ]);
        assert_eq!(run_transcript_loop(&mut inserted, &mut inserted_input), 3);

        let mut noop = App::default();
        let mut noop_input =
            FakeInput::new([Some(Event::Paste(String::new())), Some(quit_event())]);
        assert_eq!(run_transcript_loop(&mut noop, &mut noop_input), 1);

        let mut notice = App::default();
        notice.notice = Some(super::Notice::Copied);
        let mut notice_input =
            FakeInput::new([Some(Event::Paste(String::new())), Some(quit_event())]);
        assert_eq!(run_transcript_loop(&mut notice, &mut notice_input), 2);
        assert!(notice.notice().is_none());
    }

    #[test]
    fn editable_prompt_exposes_a_hardware_cursor_at_the_insertion_point() {
        let mut app = App::default();
        type_prompt(&mut app, "abcdefg");
        assert_eq!(cursor(&mut app, 8, 8), (true, Position::new(3, 6)));

        assert!(app.handle_key(key(KeyCode::Left)).visible_changed);
        assert_eq!(cursor(&mut app, 8, 8), (true, Position::new(2, 6)));
        type_prompt(&mut app, "X");
        assert_eq!(cursor(&mut app, 8, 8), (true, Position::new(3, 6)));
        assert!(app.handle_key(key(KeyCode::Up)).visible_changed);
        assert_eq!(cursor(&mut app, 8, 8), (true, Position::new(3, 6)));

        let mut full_row = App::default();
        type_prompt(&mut full_row, "abcdef");
        assert_eq!(cursor(&mut full_row, 8, 12), (true, Position::new(2, 10)));

        let mut wide = App::default();
        type_prompt(&mut wide, "🦀");
        assert!(!cursor(&mut wide, 3, 8).0);
    }

    #[test]
    fn prompt_cursor_is_hidden_when_editing_is_unavailable_or_unrenderable() {
        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        assert!(!cursor(&mut configuration, 12, 8).0);

        let mut disabled = App::default();
        disabled.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });
        assert!(!cursor(&mut disabled, 12, 8).0);

        let mut tiny = App::default();
        assert!(!cursor(&mut tiny, 12, 3).0);
    }

    #[test]
    fn alt_word_navigation_moves_without_inserting_terminal_letters() {
        for event in [
            modified_key(KeyCode::Char('b'), KeyModifiers::ALT),
            modified_key(KeyCode::Left, KeyModifiers::ALT),
        ] {
            let mut app = app_with_prompt("one two");
            assert!(app.handle_key(event).visible_changed);
            type_prompt(&mut app, ">");
            assert_eq!(submitted_prompt(&mut app), "one >two");
        }

        for event in [
            modified_key(KeyCode::Char('f'), KeyModifiers::ALT),
            modified_key(KeyCode::Right, KeyModifiers::ALT),
        ] {
            let mut app = app_with_prompt("one two");
            assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
            for _ in 0..4 {
                assert!(app.handle_key(key(KeyCode::Right)).visible_changed);
            }
            assert!(app.handle_key(event).visible_changed);
            type_prompt(&mut app, "!");
            assert_eq!(submitted_prompt(&mut app), "one two!");
        }
    }

    #[test]
    fn control_navigation_shortcuts_move_by_character_and_word() {
        let mut app = app_with_prompt("one two");
        assert!(
            app.handle_key(modified_key(KeyCode::Left, KeyModifiers::CONTROL))
                .visible_changed
        );
        type_prompt(&mut app, ">");
        assert_eq!(submitted_prompt(&mut app), "one >two");

        let mut app = app_with_prompt("one two");
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
        assert!(
            app.handle_key(modified_key(KeyCode::Right, KeyModifiers::CONTROL))
                .visible_changed
        );
        type_prompt(&mut app, ">");
        assert_eq!(submitted_prompt(&mut app), "one> two");

        let mut app = app_with_prompt("abc");
        assert!(
            app.handle_key(modified_key(KeyCode::Char('b'), KeyModifiers::CONTROL,))
                .visible_changed
        );
        type_prompt(&mut app, ">");
        assert_eq!(submitted_prompt(&mut app), "ab>c");

        let mut app = app_with_prompt("abc");
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
        assert!(
            app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL,))
                .visible_changed
        );
        type_prompt(&mut app, ">");
        assert_eq!(submitted_prompt(&mut app), "a>bc");
    }

    #[test]
    fn command_up_and_down_move_to_prompt_boundaries() {
        for modifiers in [KeyModifiers::SUPER, KeyModifiers::META] {
            let mut app = app_with_prompt("one\ntwo");
            assert!(
                app.handle_key(modified_key(KeyCode::Up, modifiers))
                    .visible_changed
            );
            type_prompt(&mut app, ">");
            assert_eq!(submitted_prompt(&mut app), ">one\ntwo");

            let mut app = app_with_prompt("one\ntwo");
            assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
            assert!(
                app.handle_key(modified_key(KeyCode::Down, modifiers))
                    .visible_changed
            );
            type_prompt(&mut app, ">");
            assert_eq!(submitted_prompt(&mut app), "one\ntwo>");
        }
    }

    #[test]
    fn control_d_deletes_the_next_grapheme() {
        let mut app = app_with_prompt("🦀x");
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);

        assert!(
            app.handle_key(modified_key(KeyCode::Char('d'), KeyModifiers::CONTROL,))
                .visible_changed
        );
        assert_eq!(submitted_prompt(&mut app), "x");
    }

    #[test]
    fn alt_d_deletes_the_next_word() {
        for (prompt, offset, expected) in [("one two", 4, "one "), ("can't", 3, "can")] {
            let mut app = app_with_prompt(prompt);
            assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
            for _ in 0..offset {
                assert!(app.handle_key(key(KeyCode::Right)).visible_changed);
            }

            assert!(
                app.handle_key(modified_key(KeyCode::Char('d'), KeyModifiers::ALT,))
                    .visible_changed
            );
            assert_eq!(submitted_prompt(&mut app), expected);
        }
    }

    #[test]
    fn control_k_and_command_delete_remove_to_line_end() {
        for event in [
            modified_key(KeyCode::Char('k'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Delete, KeyModifiers::SUPER),
            modified_key(KeyCode::Delete, KeyModifiers::META),
        ] {
            let mut app = app_with_prompt("one\ntwo");
            assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
            assert!(app.handle_key(key(KeyCode::Right)).visible_changed);
            assert!(app.handle_key(event).visible_changed);
            assert_eq!(submitted_prompt(&mut app), "one\nt");
        }
    }

    #[test]
    fn editable_prompt_maps_word_line_and_portable_modifier_shortcuts() {
        for event in [
            modified_key(KeyCode::Backspace, KeyModifiers::ALT),
            modified_key(KeyCode::Char('w'), KeyModifiers::CONTROL),
        ] {
            let mut app = App::default();
            type_prompt(&mut app, "one two   ");
            assert!(app.handle_key(event).visible_changed);
            assert_eq!(submitted_prompt(&mut app), "one ");
        }

        for event in [
            modified_key(KeyCode::Backspace, KeyModifiers::SUPER),
            modified_key(KeyCode::Backspace, KeyModifiers::META),
            modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL),
        ] {
            let mut app = App {
                enhanced_keyboard: true,
                ..App::default()
            };
            set_prompt(&mut app, "one\ntwo");
            assert!(app.handle_key(event).visible_changed);
            assert_eq!(submitted_prompt(&mut app), "one\n");
        }

        for event in [
            modified_key(KeyCode::Left, KeyModifiers::SUPER),
            modified_key(KeyCode::Left, KeyModifiers::META),
            key(KeyCode::Home),
            modified_key(KeyCode::Char('a'), KeyModifiers::CONTROL),
        ] {
            let mut app = App::default();
            type_prompt(&mut app, "abc");
            assert!(app.handle_key(event).visible_changed);
            type_prompt(&mut app, ">");
            assert_eq!(submitted_prompt(&mut app), ">abc");
        }

        for event in [
            modified_key(KeyCode::Right, KeyModifiers::SUPER),
            modified_key(KeyCode::Right, KeyModifiers::META),
            key(KeyCode::End),
            modified_key(KeyCode::Char('e'), KeyModifiers::CONTROL),
        ] {
            let mut app = App::default();
            type_prompt(&mut app, "abc");
            assert!(app.handle_key(key(KeyCode::Left)).visible_changed);
            assert!(app.handle_key(event).visible_changed);
            type_prompt(&mut app, "!");
            assert_eq!(submitted_prompt(&mut app), "abc!");
        }

        let mut app = App::default();
        type_prompt(&mut app, "abc");
        assert!(app.handle_key(key(KeyCode::Left)).visible_changed);
        assert!(app.handle_key(key(KeyCode::Delete)).visible_changed);
        assert_eq!(submitted_prompt(&mut app), "ab");
    }

    #[test]
    fn prompt_rejects_control_and_command_characters_but_keeps_alt_characters() {
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::META,
        ] {
            let mut app = app_with_prompt("kept");

            let update = app.handle_key(modified_key(KeyCode::Char('x'), modifiers));

            assert!(update.command.is_none());
            assert!(!update.visible_changed);
            assert_eq!(app.prompt.text(), "kept");
        }

        let mut app = app_with_prompt("kept");
        let update = app.handle_key(modified_key(KeyCode::Char('å'), KeyModifiers::ALT));

        assert!(update.command.is_none());
        assert!(update.visible_changed);
        assert_eq!(app.prompt.text(), "keptå");
    }

    #[test]
    fn editable_navigation_keeps_arrows_for_the_prompt_and_history_keys_for_history() {
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
        type_prompt(&mut app, "abcdefg");
        rendered(&mut app, 8, 8);

        assert!(app.handle_key(key(KeyCode::Up)).visible_changed);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(app.scroll_from_bottom > 0);
        assert!(
            app.handle_key(modified_key(KeyCode::Home, KeyModifiers::CONTROL))
                .visible_changed
        );
        assert_eq!(
            app.scroll_from_bottom,
            app.history_rows.saturating_sub(app.history_height)
        );
        assert!(
            app.handle_key(modified_key(KeyCode::End, KeyModifiers::CONTROL))
                .visible_changed
        );
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn read_only_views_keep_their_existing_history_navigation() {
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
        rendered(&mut app, 20, 7);
        app.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });

        assert!(app.handle_key(key(KeyCode::Up)).visible_changed);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(app.handle_key(key(KeyCode::PageDown)).visible_changed);
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
        assert_eq!(
            app.scroll_from_bottom,
            app.history_rows.saturating_sub(app.history_height)
        );
        assert!(app.handle_key(key(KeyCode::End)).visible_changed);
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn shift_enter_requires_keyboard_enhancement_but_plain_enter_still_submits() {
        let mut plain = App::default();
        type_prompt(&mut plain, "f");
        assert!(
            !plain
                .handle_key(shifted_key(KeyCode::Enter))
                .visible_changed
        );
        assert_eq!(submitted_prompt(&mut plain), "f");

        let mut enhanced = App {
            enhanced_keyboard: true,
            ..App::default()
        };
        type_prompt(&mut enhanced, "f");
        assert!(
            enhanced
                .handle_key(shifted_key(KeyCode::Enter))
                .visible_changed
        );
        type_prompt(&mut enhanced, "g");
        assert_eq!(submitted_prompt(&mut enhanced), "f\ng");
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
    fn configured_header_without_keyboard_enhancement_omits_newline_hint() {
        let mut app = App::default();

        let header = rendered(&mut app, 200, 4);

        assert!(header.starts_with("Status: idle | Enter submit"));
        assert!(header.contains("Arrows edit | PgUp/PgDn scroll"));
        assert!(header.contains("Ctrl-C clear/quit"));
        assert!(!header.contains("Up/Down scroll"));
        assert!(!header.contains("Shift-Enter newline"));
    }

    #[test]
    fn configured_header_with_keyboard_enhancement_advertises_newline_hint() {
        let mut app = App {
            enhanced_keyboard: true,
            ..App::default()
        };

        let header = rendered(&mut app, 120, 4);

        assert!(header.starts_with("Status: idle | Enter submit"));
        assert!(header.contains("Arrows edit | PgUp/PgDn scroll"));
        assert!(header.contains("Shift-Enter newline"));
    }

    #[test]
    fn configuration_controls_are_read_only_and_ctrl_c_needs_no_command() {
        let mut app = App::configuration(crate::config::load().snapshot().clone());
        set_prompt(&mut app, "locked");
        let prompt = app.prompt.text().to_owned();
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
            assert_eq!(app.prompt.text(), prompt);
        }

        let quit = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(quit.command.is_none());
        assert!(!quit.visible_changed);
        assert!(quit.quit);
        assert_eq!(app.prompt.text(), prompt);
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

        let redraws = super::run_configuration_loop(
            &mut terminal,
            &mut app,
            &mut input,
            &mut FakeClipboard::available(),
        )
        .unwrap();

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
            |terminal| {
                super::run_configuration_loop(
                    terminal,
                    &mut app,
                    &mut input,
                    &mut FakeClipboard::available(),
                )
                .map(|_| ())
            },
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
    fn keyboard_enhancement_pops_before_terminal_restore_and_fails_soft() {
        let calls = RefCell::new(Vec::new());
        let enabled = Cell::new(false);
        let result = {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_keyboard_enhancement(
                || {
                    calls.borrow_mut().push("push");
                    Ok(true)
                },
                || calls.borrow_mut().push("pop"),
                |available| {
                    enabled.set(available);
                    Err::<(), _>(super::TuiError::Terminal("loop failed".to_owned()))
                },
            )
        };
        assert!(result.is_err());
        assert!(enabled.get());
        assert_eq!(calls.into_inner(), ["push", "pop", "restore"]);

        for enable in [Ok(false), Err(io::Error::other("push failed"))] {
            let calls = RefCell::new(Vec::new());
            let enabled = Cell::new(true);
            {
                let _terminal_restore =
                    super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
                super::with_keyboard_enhancement(
                    || enable,
                    || calls.borrow_mut().push("pop"),
                    |available| {
                        enabled.set(available);
                        calls.borrow_mut().push("run");
                    },
                );
            }
            assert!(!enabled.get());
            assert_eq!(calls.into_inner(), ["run", "restore"]);
        }

        let calls = RefCell::new(Vec::new());
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_keyboard_enhancement(
                || {
                    calls.borrow_mut().push("push");
                    Ok(true)
                },
                || calls.borrow_mut().push("pop"),
                |available| -> () {
                    assert!(available);
                    panic!("loop panic")
                },
            );
        }));
        assert!(panic.is_err());
        assert_eq!(calls.into_inner(), ["push", "pop", "restore"]);
    }

    #[test]
    fn mouse_capture_disables_on_success_error_and_panic() {
        let calls = RefCell::new(Vec::new());
        {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_mouse_capture(
                || {
                    calls.borrow_mut().push("enable");
                    Ok(())
                },
                || calls.borrow_mut().push("disable"),
                || {
                    calls.borrow_mut().push("run");
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(calls.into_inner(), ["enable", "run", "disable", "restore"]);

        let calls = RefCell::new(Vec::new());
        let result = {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_mouse_capture(
                || {
                    calls.borrow_mut().push("enable");
                    Ok(())
                },
                || calls.borrow_mut().push("disable"),
                || {
                    calls.borrow_mut().push("run");
                    Err::<(), _>(super::TuiError::Terminal("loop failed".to_owned()))
                },
            )
        };
        assert!(result.is_err());
        assert_eq!(calls.into_inner(), ["enable", "run", "disable", "restore"]);

        let calls = RefCell::new(Vec::new());
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            let _ = super::with_mouse_capture(
                || {
                    calls.borrow_mut().push("enable");
                    Ok(())
                },
                || calls.borrow_mut().push("disable"),
                || -> Result<(), super::TuiError> {
                    calls.borrow_mut().push("run");
                    panic!("loop panic")
                },
            );
        }));
        assert!(panic.is_err());
        assert_eq!(calls.into_inner(), ["enable", "run", "disable", "restore"]);
    }

    #[test]
    fn mouse_capture_enable_failure_still_restores_the_terminal() {
        let calls = RefCell::new(Vec::new());
        let result = {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_mouse_capture(
                || {
                    calls.borrow_mut().push("enable");
                    Err(io::Error::other("enable failed"))
                },
                || calls.borrow_mut().push("disable"),
                || {
                    calls.borrow_mut().push("run");
                    Ok(())
                },
            )
        };

        assert!(
            matches!(result, Err(super::TuiError::Terminal(message)) if message == "enable failed")
        );
        assert_eq!(calls.into_inner(), ["enable", "disable", "restore"]);
    }

    #[test]
    fn bracketed_paste_lifecycle_orders_configured_modes_and_skips_configuration() {
        let calls = RefCell::new(Vec::new());
        {
            let _terminal_restore =
                super::RestoreGuard::new(|| calls.borrow_mut().push("terminal restore"));
            super::with_bracketed_paste(
                || {
                    calls.borrow_mut().push("paste enable");
                    Ok(())
                },
                || calls.borrow_mut().push("paste disable"),
                || {
                    super::with_mouse_capture(
                        || {
                            calls.borrow_mut().push("mouse enable");
                            Ok(())
                        },
                        || calls.borrow_mut().push("mouse disable"),
                        || {
                            super::with_keyboard_enhancement(
                                || {
                                    calls.borrow_mut().push("keyboard enable");
                                    Ok(true)
                                },
                                || calls.borrow_mut().push("keyboard disable"),
                                |enabled| {
                                    assert!(enabled);
                                    calls.borrow_mut().push("run");
                                    Ok::<(), super::TuiError>(())
                                },
                            )
                        },
                    )
                },
            )
            .unwrap();
        }
        assert_eq!(
            calls.into_inner(),
            [
                "paste enable",
                "mouse enable",
                "keyboard enable",
                "run",
                "keyboard disable",
                "mouse disable",
                "paste disable",
                "terminal restore",
            ]
        );

        let calls = RefCell::new(Vec::new());
        let loop_error = {
            let _terminal_restore =
                super::RestoreGuard::new(|| calls.borrow_mut().push("terminal restore"));
            super::with_bracketed_paste(
                || {
                    calls.borrow_mut().push("paste enable");
                    Ok(())
                },
                || calls.borrow_mut().push("paste disable"),
                || {
                    super::with_mouse_capture(
                        || {
                            calls.borrow_mut().push("mouse enable");
                            Ok(())
                        },
                        || calls.borrow_mut().push("mouse disable"),
                        || {
                            super::with_keyboard_enhancement(
                                || {
                                    calls.borrow_mut().push("keyboard enable");
                                    Ok(true)
                                },
                                || calls.borrow_mut().push("keyboard disable"),
                                |_| {
                                    calls.borrow_mut().push("run");
                                    Err::<(), _>(super::TuiError::Terminal(
                                        "loop failed".to_owned(),
                                    ))
                                },
                            )
                        },
                    )
                },
            )
        };
        assert!(loop_error.is_err());
        assert_eq!(
            calls.into_inner(),
            [
                "paste enable",
                "mouse enable",
                "keyboard enable",
                "run",
                "keyboard disable",
                "mouse disable",
                "paste disable",
                "terminal restore",
            ]
        );

        let calls = RefCell::new(Vec::new());
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _terminal_restore =
                super::RestoreGuard::new(|| calls.borrow_mut().push("terminal restore"));
            let _ = super::with_bracketed_paste(
                || {
                    calls.borrow_mut().push("paste enable");
                    Ok(())
                },
                || calls.borrow_mut().push("paste disable"),
                || {
                    super::with_mouse_capture(
                        || {
                            calls.borrow_mut().push("mouse enable");
                            Ok(())
                        },
                        || calls.borrow_mut().push("mouse disable"),
                        || {
                            super::with_keyboard_enhancement(
                                || {
                                    calls.borrow_mut().push("keyboard enable");
                                    Ok(true)
                                },
                                || calls.borrow_mut().push("keyboard disable"),
                                |_| -> Result<(), super::TuiError> {
                                    calls.borrow_mut().push("run");
                                    panic!("loop panic")
                                },
                            )
                        },
                    )
                },
            );
        }));
        assert!(unwind.is_err());
        assert_eq!(
            calls.into_inner(),
            [
                "paste enable",
                "mouse enable",
                "keyboard enable",
                "run",
                "keyboard disable",
                "mouse disable",
                "paste disable",
                "terminal restore",
            ]
        );

        let calls = RefCell::new(Vec::new());
        let paste_error = {
            let _terminal_restore =
                super::RestoreGuard::new(|| calls.borrow_mut().push("terminal restore"));
            super::with_bracketed_paste(
                || {
                    calls.borrow_mut().push("paste enable");
                    Err(io::Error::other("paste failed"))
                },
                || calls.borrow_mut().push("paste disable"),
                || {
                    calls.borrow_mut().push("run");
                    Ok(())
                },
            )
        };
        assert!(
            matches!(paste_error, Err(super::TuiError::Terminal(message)) if message == "paste failed")
        );
        assert_eq!(
            calls.into_inner(),
            ["paste enable", "paste disable", "terminal restore"]
        );

        let calls = RefCell::new(Vec::new());
        let mouse_error = {
            let _terminal_restore =
                super::RestoreGuard::new(|| calls.borrow_mut().push("terminal restore"));
            super::with_bracketed_paste(
                || {
                    calls.borrow_mut().push("paste enable");
                    Ok(())
                },
                || calls.borrow_mut().push("paste disable"),
                || {
                    super::with_mouse_capture(
                        || {
                            calls.borrow_mut().push("mouse enable");
                            Err(io::Error::other("mouse failed"))
                        },
                        || calls.borrow_mut().push("mouse disable"),
                        || {
                            calls.borrow_mut().push("run");
                            Ok(())
                        },
                    )
                },
            )
        };
        assert!(
            matches!(mouse_error, Err(super::TuiError::Terminal(message)) if message == "mouse failed")
        );
        assert_eq!(
            calls.into_inner(),
            [
                "paste enable",
                "mouse enable",
                "mouse disable",
                "paste disable",
                "terminal restore",
            ]
        );

        let calls = RefCell::new(Vec::new());
        let initialisation_error = super::with_terminal::<TestBackend>(
            || {
                calls.borrow_mut().push("initialise");
                Err(super::TuiError::Terminal(
                    "initialisation failed".to_owned(),
                ))
            },
            || calls.borrow_mut().push("terminal restore"),
            |_| {
                calls.borrow_mut().push("paste enable");
                Ok(())
            },
        );
        assert!(initialisation_error.is_err());
        assert_eq!(calls.into_inner(), ["initialise", "terminal restore"]);

        let calls = RefCell::new(Vec::new());
        super::with_terminal(
            || {
                calls.borrow_mut().push("initialise");
                Terminal::new(TestBackend::new(10, 3))
                    .map_err(|error| super::TuiError::Terminal(error.to_string()))
            },
            || calls.borrow_mut().push("terminal restore"),
            |_| {
                super::with_mouse_capture(
                    || {
                        calls.borrow_mut().push("mouse enable");
                        Ok(())
                    },
                    || calls.borrow_mut().push("mouse disable"),
                    || {
                        calls.borrow_mut().push("run");
                        Ok(())
                    },
                )
            },
        )
        .unwrap();
        assert_eq!(
            calls.into_inner(),
            [
                "initialise",
                "mouse enable",
                "run",
                "mouse disable",
                "terminal restore",
            ]
        );
    }

    #[test]
    fn active_run_accepts_cancel_but_not_submit() {
        let mut app = App::default();
        assert!(app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(7) }));
        app.handle_key(key(KeyCode::Char('x')));

        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)).command,
            Some(RuntimeCommand::Cancel)
        );
    }

    #[test]
    fn ctrl_c_clears_a_prompt_then_shuts_down_when_it_is_empty() {
        for active in [false, true] {
            let mut app = app_with_prompt("draft");
            if active {
                assert!(app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(7) }));
            }

            let clear = app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(clear.command.is_none());
            assert!(clear.visible_changed);
            assert!(!clear.quit);
            assert!(app.prompt.is_empty());

            let shutdown = app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(shutdown.command, Some(RuntimeCommand::Shutdown));
            assert!(!shutdown.visible_changed);
            assert!(shutdown.quit);
        }
    }

    #[test]
    fn disabled_authentication_disables_tui_input() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        assert!(app.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        }));
        let disabled_screen = rendered(&mut app, 120, 15);
        assert!(disabled_screen.contains("Input disabled"));
        assert!(disabled_screen.contains("Up/Down scroll"));
        assert!(
            !disabled_screen
                .lines()
                .next()
                .is_some_and(|header| header.contains("Shift-Enter newline"))
        );

        set_prompt(&mut app, "saved prompt");
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert!(app.handle_key(key(KeyCode::Enter)).command.is_none());
        assert_eq!(app.prompt.text(), "saved prompt");
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

        let screen = rendered(&mut app, 60, 12);

        assert!(screen.starts_with("Status: idle"));
        let request = screen.find("inspect").unwrap();
        let final_message = screen.find("finished").unwrap();
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

        let newest = rendered(&mut app, 40, 7);
        assert!(newest.contains("request 8"));
        assert!(!newest.contains("request 1"));

        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(app.scroll_from_bottom > 0);

        app.handle_key(history_home());
        let oldest = rendered(&mut app, 40, 7);
        assert!(oldest.contains("request 1"));
        assert!(!oldest.contains("request 8"));

        app.handle_key(history_end());
        assert!(rendered(&mut app, 40, 7).contains("request 8"));
    }

    #[test]
    fn transcript_scrolls_nested_arena_frames_between_source_and_output() {
        let mut app = App::default();
        let source = (1..=33)
            .map(|line| format!("source line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.handle_event(completed_record(1, 1, &source, Vec::new()));

        let newest = rendered(&mut app, 40, 8);
        assert!(newest.contains("42"), "{newest}");
        assert!(app.handle_key(history_home()).visible_changed);
        let oldest = rendered(&mut app, 40, 8);
        assert!(oldest.contains("╭code"));
        assert!(oldest.contains("╭"));
        assert!(app.handle_key(history_end()).visible_changed);
        assert!(rendered(&mut app, 40, 8).contains("42"));
    }

    #[test]
    fn transcript_scrolling_uses_wrapped_rows() {
        let mut app = App::default();
        for (run_id, request) in [
            (1, "older first second third"),
            (2, "newer first second third"),
        ] {
            app.handle_event(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(run_id),
                    record: Record::UserRequest {
                        request: request.to_owned(),
                    },
                },
                progress_gap: false,
            });
        }

        rendered(&mut app, 12, 5);
        assert!(app.history_rows > 2);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(app.scroll_from_bottom > 0);
        assert!(app.handle_key(history_home()).visible_changed);
        assert_eq!(
            app.scroll_from_bottom,
            app.history_rows.saturating_sub(app.history_height)
        );
        assert!(app.handle_key(history_end()).visible_changed);
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn transcript_scrolling_survives_temporarily_non_renderable_history() {
        let mut app = App::default();
        for run_id in 1..=4 {
            app.handle_event(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(run_id),
                    record: Record::UserRequest {
                        request: "first second third fourth".to_owned(),
                    },
                },
                progress_gap: false,
            });
        }
        rendered(&mut app, 12, 5);
        assert!(app.handle_key(history_home()).visible_changed);
        let manual_scroll = app.scroll_from_bottom;
        let measured_rows = app.history_rows;
        assert!(manual_scroll > 0);

        for (width, height) in [(0, 0), (1, 2)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
            assert_eq!(app.history_rows, measured_rows);
            assert_eq!(app.scroll_from_bottom, manual_scroll);
        }

        rendered(&mut app, 12, 5);
        assert_eq!(app.scroll_from_bottom, manual_scroll);
        rendered(&mut app, 120, 5);
        assert_eq!(
            app.scroll_from_bottom,
            manual_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );
    }

    #[test]
    fn transcript_scrolling_clamps_after_authoritative_replacement() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "long live content ".repeat(40),
            },
        });
        rendered(&mut app, 20, 6);
        assert!(app.handle_key(history_home()).visible_changed);
        let manual_scroll = app.scroll_from_bottom;
        assert!(manual_scroll > 0);

        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: "done".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        rendered(&mut app, 20, 6);

        assert_eq!(
            app.scroll_from_bottom,
            app.history_rows.saturating_sub(app.history_height)
        );
        assert!(app.scroll_from_bottom < manual_scroll);
        let replacement_scroll = app.scroll_from_bottom;
        rendered(&mut app, 120, 6);
        assert_eq!(
            app.scroll_from_bottom,
            app.history_rows.saturating_sub(app.history_height)
        );
        assert!(app.scroll_from_bottom <= replacement_scroll);
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
        rendered(&mut app, 40, 7);

        assert!(!app.handle_key(key(KeyCode::PageUp)).visible_changed);
    }

    #[test]
    fn primary_drag_copies_unwrapped_logical_history_text() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: text.to_owned(),
                },
            },
            progress_gap: false,
        });
        let mut input = FakeInput::new([
            Some(mouse_at(MouseEventKind::Down(MouseButton::Left), 1, 1)),
            Some(mouse_at(MouseEventKind::Drag(MouseButton::Left), 38, 2)),
            Some(mouse_at(MouseEventKind::Up(MouseButton::Left), 38, 2)),
            None,
            Some(quit_event()),
        ]);
        let mut clipboard = FakeClipboard::available();

        assert_eq!(
            run_transcript_loop_with_clipboard(&mut app, &mut input, &mut clipboard),
            4
        );

        assert_eq!(clipboard.writes, [text]);
    }

    #[test]
    fn backwards_drag_across_cards_copies_display_order_and_highlights_only_content() {
        let first = "first selectable card";
        let second = "second selectable card";
        let mut app = App::default();
        for (run_id, request) in [(1, first), (2, second)] {
            app.handle_event(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(run_id),
                    record: Record::UserRequest {
                        request: request.to_owned(),
                    },
                },
                progress_gap: false,
            });
        }
        let initial = rendered_buffer(&mut app, 60, 12);
        let (first_column, first_row) = cell_start(&initial, first);
        let (second_column, second_row) = cell_start(&initial, second);
        let now = Instant::now();

        app.handle_mouse(
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                second_column + u16::try_from(second.len() - 1).unwrap(),
                second_row,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                first_column,
                first_row,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                first_column,
                first_row,
            ),
            now,
        );

        let selected = rendered_buffer(&mut app, 60, 12);
        assert!(
            selected[(first_column, first_row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(
            !selected[(first_column - 1, first_row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut clipboard = FakeClipboard::available();
        assert!(super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap());
        assert_eq!(clipboard.writes, [format!("{first}\n\n{second}")]);
    }

    #[test]
    fn edge_drag_scrolls_one_row_per_cadence_and_stops_on_release() {
        let mut app = app_with_history(App::default());
        let initial = rendered_buffer(&mut app, 40, 7);
        let (column, row) = cell_start(&initial, "request 8");
        let end_column = column + u16::try_from("request 8".len() - 1).unwrap();
        let viewport = app.history_viewport.expect("history viewport");
        let now = Instant::now();

        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), end_column, row),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                viewport.area.top(),
            ),
            now,
        );
        assert!(!app.tick(now + Duration::from_millis(49)));
        assert!(app.tick(now + Duration::from_millis(50)));
        assert_eq!(app.scroll_from_bottom, 1);
        rendered_buffer(&mut app, 40, 7);
        assert!(app.tick(now + Duration::from_millis(100)));
        assert_eq!(app.scroll_from_bottom, 2);
        rendered_buffer(&mut app, 40, 7);

        app.handle_mouse(
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                column,
                viewport.area.top(),
            ),
            now + Duration::from_millis(100),
        );
        assert!(app.drag.is_none());
        assert!(!app.tick(now + Duration::from_millis(150)));
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        let mut clipboard = FakeClipboard::available();
        super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap();
        assert_eq!(clipboard.writes.len(), 1);
        assert!(clipboard.writes[0].contains("request 7"));
        assert!(clipboard.writes[0].contains("request 8"));
    }

    #[test]
    fn successful_copy_flashes_four_times_and_retains_the_selection() {
        let (mut app, now) = app_with_pending_copy("flash selection");
        let mut clipboard = FakeClipboard::available();

        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(clipboard.writes, ["flash selection"]);
        assert_eq!(app.notice(), Some("Copied"));
        assert!(app.selection_for_render().is_some());
        for (elapsed, highlighted) in [(50, false), (100, true), (150, false), (200, true)] {
            assert!(app.tick(now + Duration::from_millis(elapsed)));
            assert_eq!(app.selection_for_render().is_some(), highlighted);
        }
        assert!(app.flash.is_none());
        assert!(app.selection.is_some());
        assert_eq!(app.notice(), Some("Copied"));
    }

    #[test]
    fn failed_copy_is_generic_nonfatal_and_keeps_an_ordinary_highlight() {
        let (mut app, now) = app_with_pending_copy("failed selection");
        let mut clipboard = FakeClipboard {
            writes: Vec::new(),
            result: Err(ClipboardError),
        };

        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(clipboard.writes, ["failed selection"]);
        assert_eq!(app.notice(), Some("Copy failed"));
        assert!(app.flash.is_none());
        assert!(app.selection_for_render().is_some());
        assert!(rendered(&mut app, 60, 12).starts_with("Status: idle |"));
        assert!(rendered(&mut app, 60, 12).contains("Copy failed"));
    }

    #[test]
    fn selection_copy_works_while_active_disabled_and_configuration_only() {
        let text = "mode copy";
        let mut active = App::default();
        active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        let active_now = queue_copy(&mut active, text, 60, 12);
        let mut active_clipboard = FakeClipboard::available();
        active.finish_pending_copy(&mut active_clipboard, active_now);
        assert_eq!(active_clipboard.writes, [text]);

        let mut disabled = App::default();
        disabled.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });
        let disabled_now = queue_copy(&mut disabled, text, 60, 12);
        let mut disabled_clipboard = FakeClipboard::available();
        disabled.finish_pending_copy(&mut disabled_clipboard, disabled_now);
        assert_eq!(disabled_clipboard.writes, [text]);

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        let configuration_now = queue_copy(&mut configuration, text, 120, 15);
        let mut configuration_clipboard = FakeClipboard::available();
        configuration.finish_pending_copy(&mut configuration_clipboard, configuration_now);
        assert_eq!(configuration_clipboard.writes, [text]);
    }

    #[test]
    fn decoration_only_selection_clears_without_a_clipboard_write() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "content".to_owned(),
                },
            },
            progress_gap: false,
        });
        rendered_buffer(&mut app, 60, 12);
        let viewport = app.history_viewport.expect("history viewport");
        let now = Instant::now();
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                viewport.area.x,
                viewport.area.y,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                viewport.area.x + 1,
                viewport.area.y,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                viewport.area.x + 1,
                viewport.area.y,
            ),
            now,
        );
        rendered_buffer(&mut app, 60, 12);
        let mut clipboard = FakeClipboard::available();

        assert!(!app.finish_pending_copy(&mut clipboard, now));
        assert!(clipboard.writes.is_empty());
        assert!(app.selection.is_none());
    }

    #[test]
    fn configuration_loop_copies_with_an_injected_clipboard() {
        let text = "configuration loop copy";
        let mut app = App::configuration(crate::config::ready_snapshot_for_test());
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: text.to_owned(),
                },
            },
            progress_gap: false,
        });
        let buffer = rendered_buffer(&mut app, 120, 15);
        let (column, row) = cell_start(&buffer, text);
        let end = column + u16::try_from(text.len() - 1).unwrap();
        let mut input = FakeInput::new([
            Some(mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
            )),
            Some(mouse_at(MouseEventKind::Drag(MouseButton::Left), end, row)),
            Some(mouse_at(MouseEventKind::Up(MouseButton::Left), end, row)),
            None,
            Some(quit_event()),
        ]);
        let mut clipboard = FakeClipboard::available();

        assert_eq!(
            run_configuration_loop_with_clipboard_at(&mut app, &mut input, &mut clipboard, 120, 15,),
            4
        );
        assert_eq!(clipboard.writes, [text]);
    }

    #[test]
    fn new_primary_down_replaces_the_retained_selection_and_feedback() {
        let (mut app, now) = app_with_pending_copy("first selection");
        let mut clipboard = FakeClipboard::available();
        app.finish_pending_copy(&mut clipboard, now);
        let buffer = rendered_buffer(&mut app, 60, 12);
        let (column, row) = cell_start(&buffer, "first selection");

        assert!(
            app.handle_mouse(
                mouse_event(MouseEventKind::Down(MouseButton::Left), column, row),
                now,
            )
            .visible_changed
        );
        assert!(app.selection.is_none());
        assert!(app.notice().is_none());
        assert!(app.flash.is_none());
        assert!(app.drag.is_some());
    }

    #[test]
    fn input_clears_the_notice_without_clearing_a_completed_selection() {
        let (mut app, now) = app_with_pending_copy("notice selection");
        let mut clipboard = FakeClipboard::available();
        app.finish_pending_copy(&mut clipboard, now);

        assert!(app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert!(app.notice().is_none());
        assert!(app.selection.is_some());
    }

    #[test]
    fn paste_clears_copy_feedback_without_affecting_selection() {
        let mut configured = App::default();
        set_prompt(&mut configured, "keep");
        let now = queue_copy(&mut configured, "copied selection", 40, 7);
        let mut copied_clipboard = FakeClipboard::available();
        assert!(configured.finish_pending_copy(&mut copied_clipboard, now));
        let copied_selection = configured.selection;

        let update = configured.handle_paste(" pasted");

        assert!(update.visible_changed);
        assert!(update.command.is_none());
        assert!(configured.notice().is_none());
        assert_eq!(configured.selection, copied_selection);
        assert_eq!(configured.prompt.text(), "keep pasted");

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, "keep");
        let now = queue_copy(&mut configuration, "failed selection", 40, 7);
        let mut failed_clipboard = FakeClipboard {
            writes: Vec::new(),
            result: Err(ClipboardError),
        };
        assert!(configuration.finish_pending_copy(&mut failed_clipboard, now));
        let failed_selection = configuration.selection;
        let mut configuration_input =
            FakeInput::new([Some(Event::Paste("ignored".to_owned())), Some(quit_event())]);

        assert_eq!(
            run_configuration_loop_with_clipboard(
                &mut configuration,
                &mut configuration_input,
                &mut failed_clipboard,
            ),
            2
        );
        assert!(configuration.notice().is_none());
        assert_eq!(configuration.selection, failed_selection);
        assert_eq!(configuration.prompt.text(), "keep");
    }

    #[test]
    fn completed_selection_survives_history_scrolling_but_not_stale_changes() {
        let mut app = app_with_history(App::default());
        let now = queue_copy(&mut app, "retained selection", 60, 7);
        let mut clipboard = FakeClipboard::available();
        app.finish_pending_copy(&mut clipboard, now);

        assert!(
            app.handle_mouse(mouse_event(MouseEventKind::ScrollUp, 0, 0), now)
                .visible_changed
        );
        assert!(app.handle_key(key(KeyCode::PageDown)).visible_changed);
        assert!(app.selection.is_some());
        assert_eq!(app.notice(), None);

        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(9),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "new history".to_owned(),
            },
        });
        assert!(app.selection.is_none());
        assert!(app.drag.is_none());
        assert!(app.flash.is_none());
    }

    #[test]
    fn selection_invalidates_when_history_geometry_or_layout_changes() {
        let (mut prompt_growth, _) = app_with_pending_copy("prompt geometry");
        set_prompt(&mut prompt_growth, &"x".repeat(120));
        rendered_buffer(&mut prompt_growth, 60, 12);
        assert!(prompt_growth.selection.is_none());

        let (mut resized, now) = app_with_pending_copy("resize geometry");
        let mut resized_clipboard = FakeClipboard::available();
        resized.finish_pending_copy(&mut resized_clipboard, now);
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut input = FakeInput::new([Some(Event::Resize(61, 12)), Some(quit_event())]);
        let (commands, _received) = tokio::sync::mpsc::channel(1);
        let (_events, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (_progress, mut progress_receiver) = tokio::sync::mpsc::channel(1);
        super::run_loop(
            &mut terminal,
            &mut resized,
            &mut input,
            &commands,
            &mut event_receiver,
            &mut progress_receiver,
            &mut resized_clipboard,
        )
        .unwrap();
        assert!(resized.selection.is_none());

        let (mut tiny, _) = app_with_pending_copy("tiny history");
        rendered_buffer(&mut tiny, 3, 3);
        assert!(tiny.selection.is_none());
        assert!(tiny.history_viewport.is_none());

        let mut layout = App::default();
        layout.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: "answer".to_owned(),
                    reasoning_summary: "reasoning".to_owned(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        queue_copy(&mut layout, "layout selection", 60, 12);
        assert!(layout.selection.is_some());
        assert!(
            layout
                .handle_key(modified_key(KeyCode::Char('t'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert!(layout.selection.is_none());
    }

    #[test]
    fn edge_drag_scrolls_newer_and_stops_at_history_bounds() {
        let mut newer = app_with_history(App::default());
        rendered_buffer(&mut newer, 40, 7);
        assert!(newer.update_scroll(usize::MAX).visible_changed);
        let visible = rendered_buffer(&mut newer, 40, 7);
        let (column, row) = cell_start(&visible, "request 1");
        let viewport = newer.history_viewport.expect("history viewport");
        let now = Instant::now();
        newer.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), column, row),
            now,
        );
        newer.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                viewport.area.bottom() - 1,
            ),
            now,
        );
        let before = newer.scroll_from_bottom;
        assert!(newer.tick(now + super::POLL_INTERVAL));
        assert_eq!(newer.scroll_from_bottom, before - 1);

        let mut older = app_with_history(App::default());
        let visible = rendered_buffer(&mut older, 40, 7);
        let (column, row) = cell_start(&visible, "request 8");
        let viewport = older.history_viewport.expect("history viewport");
        let now = Instant::now();
        older.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), column, row),
            now,
        );
        older.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                column,
                viewport.area.top(),
            ),
            now,
        );
        for cadence in 1..=older.history_rows {
            older.tick(now + super::POLL_INTERVAL * u32::try_from(cadence).unwrap());
        }
        assert_eq!(
            older.scroll_from_bottom,
            older.history_rows.saturating_sub(older.history_height)
        );
        assert!(older.drag.is_some_and(|drag| drag.edge.is_none()));
        assert!(
            !older
                .tick(now + super::POLL_INTERVAL * u32::try_from(older.history_rows + 1).unwrap())
        );
    }

    #[test]
    fn empty_non_primary_and_outside_clicks_do_not_copy_or_select() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "selectable text".to_owned(),
                },
            },
            progress_gap: false,
        });
        let buffer = rendered_buffer(&mut app, 40, 7);
        let viewport = app.history_viewport.expect("history viewport");
        let now = Instant::now();

        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Drag(MouseButton::Right),
            MouseEventKind::Up(MouseButton::Right),
        ] {
            app.handle_mouse(mouse_event(kind, 1, 2), now);
        }
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 1, 0),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 1, 0),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 1, viewport.area.y),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 1, viewport.area.y),
            now,
        );

        assert!(app.selection.is_none());
        assert!(app.drag.is_none());
        assert!(!app.pending_copy);
        let mut terminal = Terminal::new(TestBackend::new(40, 7)).unwrap();
        let mut clipboard = FakeClipboard::available();
        assert!(!super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap());
        assert!(clipboard.writes.is_empty());
        assert_eq!(buffer, rendered_buffer(&mut app, 40, 7));
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_history() {
        let mut app = app_with_history(App::default());
        let mut input = FakeInput::new([
            Some(mouse(MouseEventKind::Down(MouseButton::Left))),
            Some(mouse(MouseEventKind::ScrollUp)),
            Some(quit_event()),
        ]);

        let redraws = run_transcript_loop(&mut app, &mut input);

        assert_eq!(app.scroll_from_bottom, 3);
        assert_eq!(redraws, 2);
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_towards_newest_history() {
        let mut app = app_with_history(App::default());
        rendered(&mut app, 40, 7);
        assert!(app.update_scroll(3).visible_changed);
        let mut input =
            FakeInput::new([Some(mouse(MouseEventKind::ScrollDown)), Some(quit_event())]);

        run_transcript_loop(&mut app, &mut input);

        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn configuration_mouse_wheel_scrolls_transcript_history() {
        let mut app =
            app_with_history(App::configuration(crate::config::load().snapshot().clone()));
        let mut input = FakeInput::new([
            Some(mouse(MouseEventKind::Down(MouseButton::Left))),
            Some(mouse(MouseEventKind::ScrollUp)),
            Some(quit_event()),
        ]);

        let redraws = run_configuration_loop(&mut app, &mut input);

        assert_eq!(app.scroll_from_bottom, 3);
        assert_eq!(redraws, 2);
    }

    #[test]
    fn configuration_mouse_wheel_scrolls_towards_newest_history() {
        let mut app =
            app_with_history(App::configuration(crate::config::load().snapshot().clone()));
        rendered(&mut app, 40, 7);
        assert!(app.update_scroll(3).visible_changed);
        let mut input =
            FakeInput::new([Some(mouse(MouseEventKind::ScrollDown)), Some(quit_event())]);

        run_configuration_loop(&mut app, &mut input);

        assert_eq!(app.scroll_from_bottom, 0);
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
                &mut FakeClipboard::available(),
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
            &mut FakeClipboard::available(),
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
            &mut FakeClipboard::available(),
        );

        assert!(matches!(result, Err(super::TuiError::RuntimeClosed)));
    }

    #[test]
    fn transcript_live_markdown_uses_markdown_styles() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "# BoldHeading".to_owned(),
            },
        });

        let buffer = rendered_buffer(&mut app, 40, 8);
        assert!(buffer.content.iter().any(|cell| {
            cell.symbol() == "B"
                && cell
                    .style()
                    .add_modifier
                    .contains(ratatui::style::Modifier::BOLD)
        }));
    }

    #[test]
    fn transcript_rendering_is_stable_at_tiny_dimensions() {
        let mut app = app_with_prompt("🦀");
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
                    &mut FakeClipboard::available(),
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

    fn detailed_usage(
        input: Option<u64>,
        output: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
        reasoning: Option<u64>,
        total: Option<u64>,
    ) -> AttemptAccounting {
        AttemptAccounting::KnownUsage(ModelUsage {
            model: "gpt-5.6-sol".to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: input,
            cached_input_tokens: cache_read,
            cache_write_input_tokens: cache_write,
            output_tokens: output,
            reasoning_output_tokens: reasoning,
            total_tokens: total,
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
                        tool_calls: Vec::new(),
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
                        tool_calls: Vec::new(),
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
                            tool_calls: vec![ToolCall {
                                tool: ToolName::Read,
                                summary: ToolCallSummary::Failed,
                            }],
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
        for text in [
            "result",
            "42",
            "error",
            "boom",
            "await read(path)",
            "stderr",
            "diagnostic",
            "tool calls",
            "read · failed",
            "failure",
            "interrupted",
            "warning",
            "cancelled warning",
        ] {
            assert!(history.contains(text), "{history}");
        }
        for flattened_label in ["result: 42", "error: boom", "stderr: diagnostic"] {
            assert!(!history.contains(flattened_label), "{history}");
        }

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
        assert!(generation_history.contains("interrupted reasoning"));
        assert!(generation_history.contains("interrupted assistant"));
        assert!(generation_history.contains("code"));
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
    fn configured_prompt_box_is_bottom_anchored_and_footer_is_embedded() {
        let mut app = app_with_prompt("ask");
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "history".to_owned(),
                },
            },
            progress_gap: false,
        });

        let without_accounting = rendered(&mut app, 80, 12);
        let without_rows = without_accounting.lines().collect::<Vec<_>>();
        assert_eq!(without_rows[9], "─".repeat(80));
        assert!(without_rows[10].starts_with("> ask"));
        assert_eq!(without_rows[11], "─".repeat(80));
        assert_eq!(app.history_height, 8);

        app.handle_event(completed_record(1, 1, "accounting()", vec![usage(1, 1)]));
        let with_accounting = rendered(&mut app, 80, 12);
        let with_rows = with_accounting.lines().collect::<Vec<_>>();
        assert_eq!(with_rows[9], "─".repeat(80));
        assert!(with_rows[10].starts_with("> ask"));
        assert!(with_rows[11].starts_with('─'));
        assert!(with_rows[11].ends_with("↑1 ↓1 R0 W0 r0 Σ2 C0%/272k H0% $0.000024 ─"));
        assert_eq!(app.history_height, 8);
    }

    #[test]
    fn prompt_preserves_empty_and_trailing_logical_lines() {
        for (prompt, height, expected_rows, rule_row, prompt_row) in [
            ("\n", 12, 2, 8, "> "),
            ("abc\n", 12, 2, 8, "> abc"),
            ("abc\n\n\n", 21, 4, 15, "> abc"),
        ] {
            let mut app = app_with_prompt(prompt);
            assert_eq!(app.prompt.layout(6, usize::MAX).rows().len(), expected_rows);
            let screen = rendered(&mut app, 8, height);
            let rows = screen.lines().collect::<Vec<_>>();
            assert_eq!(rows[rule_row], "─".repeat(8));
            assert!(rows[rule_row + 1].starts_with(prompt_row));
        }
    }

    #[test]
    fn prompt_wraps_above_its_fixed_baseline_and_clips_to_the_newest_tail() {
        let mut app = app_with_prompt("old\nabcdefghij");

        let screen = rendered(&mut app, 8, 12);
        let rows = screen.lines().collect::<Vec<_>>();
        assert_eq!(rows[8], "─".repeat(8));
        assert!(rows[9].starts_with("> abcdef"));
        assert!(rows[10].starts_with("  ghij"));
        assert_eq!(rows[11], "─".repeat(8));
        assert_eq!(app.history_height, 7);
    }

    #[test]
    fn prompt_chrome_is_safe_at_small_dimensions_and_configuration_stays_unchanged() {
        let mut app = App::default();
        for height in 0..=6 {
            for width in 0..=2 {
                let backend = TestBackend::new(width, height);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal
                    .draw(|frame| super::render(frame, &mut app))
                    .unwrap();
            }
        }

        let normal_header = rendered(&mut app, 80, 4);
        assert!(
            normal_header
                .lines()
                .next()
                .is_some_and(|header| !header.contains("Shift-Enter newline"))
        );

        let three_rows = rendered(&mut app, 10, 3);
        let rows = three_rows.lines().collect::<Vec<_>>();
        assert!(rows[0].starts_with("Status:"));
        assert_eq!(rows[1], "─".repeat(10));
        assert_eq!(rows[2], "─".repeat(10));

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, "hidden");
        let configuration_screen = rendered(&mut configuration, 80, 15);
        assert!(configuration_screen.starts_with("Configuration only:"));
        assert!(configuration_screen.contains("Startup configuration"));
        assert!(!configuration_screen.contains("Shift-Enter newline"));
        assert!(!configuration_screen.contains("> hidden"));
        assert!(!configuration_screen.contains('─'));
        assert_eq!(configuration.history_height, 14);
    }

    #[test]
    fn prompt_growth_preserves_scroll_and_footer_does_not_change_history_height() {
        let mut app = App::default();
        for run_id in 1..=8 {
            app.handle_event(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(run_id),
                    record: Record::UserRequest {
                        request: format!("history line {run_id}"),
                    },
                },
                progress_gap: false,
            });
        }
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 8);
        assert_eq!(app.scroll_from_bottom, 0);

        set_prompt(&mut app, &"a".repeat(19));
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 7);
        assert_eq!(app.scroll_from_bottom, 0);

        set_prompt(&mut app, "");
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 8);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.handle_key(history_home()).visible_changed);
        let manual_scroll = app.scroll_from_bottom;

        set_prompt(&mut app, &"a".repeat(19));
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 7);
        assert_eq!(
            app.scroll_from_bottom,
            manual_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );
        let manual_scroll = app.scroll_from_bottom;

        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(9) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(9),
            step_id: ModelStepId(1),
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: usage(1, 1),
            },
        });
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 7);
        assert_eq!(
            app.scroll_from_bottom,
            manual_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );

        set_prompt(&mut app, "");
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 8);
        assert_eq!(
            app.scroll_from_bottom,
            manual_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );
        let cached_scroll = app.scroll_from_bottom;
        let backend = TestBackend::new(0, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render(frame, &mut app))
            .unwrap();
        assert_eq!(app.scroll_from_bottom, cached_scroll);
    }

    #[test]
    fn footer_places_prompt_above_accounting_at_prioritised_heights() {
        let mut app = app_with_prompt("ask");
        app.handle_event(completed_record(
            1,
            1,
            "accounting()",
            vec![detailed_usage(
                Some(100),
                Some(20),
                Some(10),
                Some(5),
                Some(5),
                Some(120),
            )],
        ));

        assert!(rendered(&mut app, 80, 0).is_empty());
        let one = rendered(&mut app, 80, 1);
        assert!(one.starts_with("Status:"));
        assert!(!one.contains("> ask"));
        let two = rendered(&mut app, 80, 2);
        assert!(two.lines().nth(1).is_some_and(|row| row == "─".repeat(80)));

        let three = rendered(&mut app, 80, 3);
        let rows = three.lines().collect::<Vec<_>>();
        assert!(rows[0].starts_with("Status:"));
        assert_eq!(rows[1], "─".repeat(80));
        assert_eq!(rows[2], "─".repeat(80));

        let four = rendered(&mut app, 80, 4);
        let rows = four.lines().collect::<Vec<_>>();
        assert!(rows[2].starts_with("> ask"));
        assert!(rows[3].ends_with("↑100 ↓20 R10 W5 r5 Σ120 C0%/272k H10% $0.000769 ─"));
        assert_eq!(app.history_height, 0);

        let larger = rendered(&mut app, 80, 6);
        assert!(
            larger
                .lines()
                .nth(4)
                .is_some_and(|row| row.starts_with("> ask"))
        );
        assert!(
            larger.lines().nth(5).is_some_and(
                |row| row.ends_with("↑100 ↓20 R10 W5 r5 Σ120 C0%/272k H10% $0.000769 ─")
            )
        );
        assert_eq!(app.history_height, 2);
        assert!(app.history_rows > 0);
    }

    #[test]
    fn footer_is_absent_without_metrics_or_in_configuration_only_mode() {
        let mut without_metrics = app_with_prompt("ask");
        without_metrics.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "request".to_owned(),
                },
            },
            progress_gap: false,
        });
        let screen = rendered(&mut without_metrics, 80, 4);
        assert!(
            screen
                .lines()
                .nth(2)
                .is_some_and(|row| row.starts_with("> ask"))
        );
        assert!(
            screen
                .lines()
                .nth(3)
                .is_some_and(|row| row == "─".repeat(80))
        );
        assert!(!screen.contains('↑'));

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        configuration.handle_event(completed_record(1, 1, "code()", vec![usage(1, 1)]));
        assert!(!rendered(&mut configuration, 80, 5).contains("↑1 ↓1 R0 W0 r0 Σ2 $"));
    }

    #[test]
    fn footer_renders_styled_metrics_and_truncates_to_one_display_row() {
        let mut app = App::default();
        app.handle_event(completed_record(
            1,
            1,
            "accounting()",
            vec![detailed_usage(
                Some(217_600),
                Some(0),
                Some(163_200),
                Some(0),
                Some(0),
                Some(217_600),
            )],
        ));

        let full = rendered_buffer(&mut app, 80, 4);
        let column = (0..full.area.width)
            .find(|column| {
                full[(*column, 3)].symbol() == "8"
                    && full[(*column + 1, 3)].symbol() == "0"
                    && full[(*column + 2, 3)].symbol() == "%"
            })
            .expect("context percentage");
        assert_eq!(full[(column, 3)].fg, Color::Yellow);
        let full = rendered(&mut app, 80, 4);
        assert!(
            full.lines()
                .nth(3)
                .is_some_and(|row| row.ends_with("C80%/272k H75% $0.28288 ─"))
        );

        let one = rendered(&mut app, 1, 4);
        assert_eq!(one.lines().nth(3), Some("…"));
        let narrow = rendered(&mut app, 29, 4);
        assert_eq!(narrow.lines().nth(3), Some("─ … C80%/272k H75% $0.28288 ─"));
    }

    #[test]
    fn footer_aggregates_disjoint_reliable_attempts_after_live_replacement() {
        let first = detailed_usage(Some(100), Some(20), Some(10), Some(5), Some(5), Some(120));
        let residual = detailed_usage(None, Some(10), None, Some(0), Some(0), Some(10));
        let mut wrong_tier =
            match detailed_usage(Some(7), Some(0), Some(0), Some(0), Some(0), Some(7)) {
                AttemptAccounting::KnownUsage(usage) => usage,
                _ => unreachable!("known usage helper"),
            };
        wrong_tier.service_tier = Some("priority".to_owned());

        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "first prompt".to_owned(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: first.clone(),
            },
        });
        assert!(
            rendered(&mut app, 80, 5).lines().last().is_some_and(
                |row| row.ends_with("↑100 ↓20 R10 W5 r5 Σ120 C0%/272k H10% $0.000769 ─")
            )
        );

        app.handle_event(completed_record(1, 1, "first()", vec![first]));
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(2),
                    text: "done".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: terminal(None, vec![residual]),
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::UserRequest {
                    request: "second prompt".to_owned(),
                },
            },
            progress_gap: false,
        });
        app.handle_event(completed_record(
            2,
            1,
            "other()",
            vec![AttemptAccounting::KnownUsage(wrong_tier)],
        ));

        assert!(
            rendered(&mut app, 80, 8).lines().last().is_some_and(
                |row| row.ends_with("↑107 ↓30 R10 W5 r5 Σ137 C0%/272k H0% $0.000769 ─")
            )
        );
    }

    #[test]
    fn footer_reserves_history_and_preserves_bottom_and_manual_scroll() {
        let mut app = App::default();
        for run_id in 1..=4 {
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
        rendered(&mut app, 20, 5);
        assert_eq!(app.history_height, 1);
        assert_eq!(app.scroll_from_bottom, 0);

        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(5) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(5),
            step_id: ModelStepId(1),
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: usage(1, 1),
            },
        });
        rendered(&mut app, 20, 5);
        assert_eq!(app.history_height, 1);
        assert_eq!(app.scroll_from_bottom, 0);

        assert!(app.handle_key(history_home()).visible_changed);
        let manual_scroll = app.scroll_from_bottom;
        assert!(manual_scroll > 0);
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(5),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: "settled".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        rendered(&mut app, 20, 5);
        assert_eq!(app.history_height, 1);
        assert_eq!(
            app.scroll_from_bottom,
            manual_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );
    }

    #[test]
    fn footer_keeps_scroll_when_dimensions_are_non_renderable() {
        let mut app = App::default();
        for run_id in 1..=4 {
            app.handle_event(completed_record(run_id, 1, "code()", vec![usage(1, 1)]));
        }
        rendered(&mut app, 20, 5);
        app.handle_key(history_home());
        let scroll = app.scroll_from_bottom;
        assert!(scroll > 0);

        rendered(&mut app, 20, 0);
        assert_eq!(app.scroll_from_bottom, scroll);
        let backend = TestBackend::new(0, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render(frame, &mut app))
            .unwrap();
        assert_eq!(app.scroll_from_bottom, scroll);
    }

    #[test]
    fn transcript_renders_compact_step_and_session_costs_without_double_counting() {
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
        assert!(history.contains("↑100 ↓0 R0 W0 r0 Σ100 · $0.0004"));
        assert!(history.contains("↑0 ↓100 R0 W0 r20 Σ100 · $0.002"));
        assert!(history.contains("↑100 ↓100 R0 W0 r20 Σ200 $0.0024"));
        assert!(!history.contains("Run estimated cost"));
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
            rendered(&mut active, 120, 40).contains("↑100 ↓100 R0 W0 r0 Σ200 C0%/272k $0.0024")
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
        rendered(&mut app, 50, 5);
        assert_eq!(app.scroll_from_bottom, 0);

        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        let manual_scroll = app.scroll_from_bottom;
        app.handle_event(completed_record(2, 5, "settled", vec![]));
        rendered(&mut app, 50, 5);
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
    fn controls_toggle_reasoning_and_arena_expansion_without_commands() {
        let mut app = app_with_prompt("kept prompt");
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    source: (1..=33)
                        .map(|line| format!("code line {line:02}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    reasoning_summary: "visible reasoning".to_owned(),
                    execution: Execution {
                        stdout: observation(""),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::Complete { result: None },
                        tool_calls: (1..=20)
                            .map(|count| ToolCall {
                                tool: ToolName::Read,
                                summary: ToolCallSummary::Count(count),
                            })
                            .collect(),
                    },
                    attempts: Vec::new(),
                },
            },
            progress_gap: false,
        });

        for code in ['t', 'T'] {
            let update = app.handle_key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL));
            assert!(update.command.is_none());
            assert!(update.visible_changed);
            assert_eq!(app.prompt.text(), "kept prompt");
        }
        assert!(rendered(&mut app, 60, 80).contains("visible reasoning"));

        let hidden = app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(hidden.command.is_none());
        assert!(hidden.visible_changed);
        let screen = rendered(&mut app, 60, 80);
        assert!(screen.contains("reasoning hidden · Ctrl-T to show"));
        assert!(!screen.contains("visible reasoning"));

        for code in ['o', 'O'] {
            let update = app.handle_key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL));
            assert!(update.command.is_none());
            assert!(update.visible_changed);
            assert_eq!(app.prompt.text(), "kept prompt");
        }
        let collapsed = rendered(&mut app, 60, 40);
        assert!(!collapsed.contains("code line 01"));
        assert!(collapsed.contains("code line 33"));
        assert!(collapsed.contains("[4 earlier tool calls omitted]"));

        let expanded = app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(expanded.command.is_none());
        assert!(expanded.visible_changed);
        let expanded = rendered(&mut app, 60, 40);
        assert!(expanded.contains("code line 33"));
        assert!(!expanded.contains("earlier tool calls omitted"));
        assert!(expanded.contains("read · 1 byte"));
        assert!(expanded.contains("read · 20 bytes"));
    }

    #[test]
    fn controls_work_when_submission_is_disabled_and_are_noops_without_blocks() {
        let mut disabled = App::default();
        disabled.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: String::new(),
                    reasoning_summary: "reasoning".to_owned(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        disabled.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });
        assert!(
            disabled
                .handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert!(rendered(&mut disabled, 60, 8).contains("reasoning hidden · Ctrl-T to show"));

        let mut configuration = App::configuration(crate::config::load().snapshot().clone());
        let update =
            configuration.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(update.command.is_none());
        assert!(!update.visible_changed);
        let reasoning =
            configuration.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(reasoning.command.is_none());
        assert!(!reasoning.visible_changed);
        configuration.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    source: (1..=33)
                        .map(|line| format!("code {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    reasoning_summary: "future reasoning".to_owned(),
                    execution: Execution {
                        stdout: observation(""),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::Complete { result: None },
                        tool_calls: Vec::new(),
                    },
                    attempts: Vec::new(),
                },
            },
            progress_gap: false,
        });
        let screen = rendered(&mut configuration, 80, 80);
        assert!(screen.contains("code 33"));
        assert!(screen.contains("reasoning hidden · Ctrl-T to show"));
    }

    #[test]
    fn controls_redraw_only_when_visible_and_clamp_scroll_after_height_reductions() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    source: (1..=33)
                        .map(|line| format!("code {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    reasoning_summary: (1..=33)
                        .map(|line| format!("reasoning {line}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    execution: Execution {
                        stdout: observation(""),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::Complete { result: None },
                        tool_calls: Vec::new(),
                    },
                    attempts: Vec::new(),
                },
            },
            progress_gap: false,
        });
        rendered(&mut app, 20, 6);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .visible_changed
        );
        rendered(&mut app, 20, 6);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(app.handle_key(history_home()).visible_changed);
        let expanded_scroll = app.scroll_from_bottom;
        assert!(expanded_scroll > 0);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .visible_changed
        );
        rendered(&mut app, 20, 6);
        assert_eq!(
            app.scroll_from_bottom,
            expanded_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );

        rendered(&mut app, 20, 6);
        app.handle_key(history_home());
        let reasoning_scroll = app.scroll_from_bottom;
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
                .visible_changed
        );
        rendered(&mut app, 20, 6);
        assert_eq!(
            app.scroll_from_bottom,
            reasoning_scroll.min(app.history_rows.saturating_sub(app.history_height))
        );

        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut redraw_app = App::default();
        redraw_app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: String::new(),
                    reasoning_summary: "reasoning".to_owned(),
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        });
        let mut input = FakeInput::new([
            Some(ratatui::crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL,
            ))),
            Some(ratatui::crossterm::event::Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
            ))),
        ]);
        let (commands, mut received) = tokio::sync::mpsc::channel(1);
        let (_events, mut event_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (_progress, mut progress_receiver) = tokio::sync::mpsc::channel(1);
        assert_eq!(
            super::run_loop(
                &mut terminal,
                &mut redraw_app,
                &mut input,
                &commands,
                &mut event_receiver,
                &mut progress_receiver,
                &mut FakeClipboard::available(),
            )
            .unwrap(),
            2
        );
        assert_eq!(received.try_recv().unwrap(), RuntimeCommand::Shutdown);
    }

    #[test]
    fn controls_are_safe_on_tiny_terminals_without_losing_zero_width_scroll() {
        let mut app = App::default();
        app.handle_event(completed_record(
            1,
            1,
            &(1..=33)
                .map(|line| format!("code {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            Vec::new(),
        ));
        rendered(&mut app, 20, 5);
        app.handle_key(history_home());
        let scroll = app.scroll_from_bottom;
        for (width, height) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 2), (3, 3)] {
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
            if width == 0 {
                assert_eq!(app.scroll_from_bottom, scroll);
            }
        }
    }

    #[test]
    fn status_header_uses_concise_activity_without_identifiers() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(73) });
        assert!(rendered(&mut app, 120, 4).starts_with("Status: starting run"));
        app.handle_event(RuntimeEvent::ModelStepStarted {
            run_id: RunId(73),
            step_id: ModelStepId(91),
        });
        assert!(rendered(&mut app, 120, 4).starts_with("Status: generating"));
        app.handle_event(RuntimeEvent::ToolUpdated {
            run_id: RunId(73),
            step_id: ModelStepId(91),
            token: ToolCallToken::for_test(1_234_567),
            call: ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            },
        });

        let screen = rendered(&mut app, 120, 4);
        let header = screen.lines().next().expect("status header");
        assert!(header.starts_with("Status: read · pending"));
        for hidden in ["run 73", "step 91", "attempt", "1234567"] {
            assert!(!header.contains(hidden), "{header}");
        }
    }

    #[test]
    fn input_rejects_controls_and_normalises_status_before_drawing() {
        let mut app = App::default();
        for character in ['\r', '\t', '\x1b', '\u{009d}'] {
            assert!(
                !app.handle_key(key(KeyCode::Char(character)))
                    .visible_changed
            );
        }
        assert!(app.prompt.is_empty());
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::ToolUpdated {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            token: ToolCallToken::for_test(0),
            call: ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
            },
        });

        let screen = rendered(&mut app, 120, 30);
        assert!(screen.contains("read · pending"));
        assert!(!screen.contains("path\\x9Dunsafe"));
        assert!(!screen.chars().any(|character| {
            matches!(character, '\u{1b}' | '\u{7f}') || character.is_control() && character != '\n'
        }));
    }
}
