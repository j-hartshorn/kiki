use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod picker;
mod slash;
mod subagents;

use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEvent,
    MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::SetTitle;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{Frame, Terminal};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

use crate::clipboard::{ClipboardPort, LocalClipboard};
use crate::config::models::CATALOGUE;
use crate::config::settings::{SaveError, SaveOutcome, SettingsContext};
use crate::config::{DEFAULT_MODEL, ModelConfiguration, ModelSelection, StartupSnapshot};
use crate::prompt_editor::{PromptEdit, PromptEditor, PromptLayout, PromptRow};
use crate::runtime::{
    ProgressEnvelope, RuntimeCommand, RuntimeEvent, RuntimeProgressReceiver, control_command,
};
use crate::selection::{DocumentPosition, Selection};
use crate::telemetry::{Trace, TraceEvent};
#[cfg(test)]
pub(crate) use crate::transcript::format_startup_configuration;
use crate::transcript::{
    FooterContext, Transcript, TranscriptLayout, normalise_terminal_text, page_style,
};
use picker::{Picker, PickerAction, PickerItem};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryPosition {
    FollowingBottom,
    Anchored(usize),
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

enum Notice {
    Copied,
    CopyFailed,
    Status(String),
}

struct Flash {
    next_tick: Instant,
    transitions: u8,
    highlighted: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerValue {
    Model(&'static ModelConfiguration),
    Thinking(&'static str),
}

#[derive(Clone, Copy)]
struct ThinkingPreview {
    selection: ModelSelection,
    deadline: Instant,
    sent_revision: Option<u64>,
}

struct App {
    mode: AppMode,
    prompt: PromptEditor,
    prompt_width: u16,
    transcript: Transcript,
    subagent_monitor: subagents::SubagentMonitor,
    foreground_cancellation_requested: bool,
    history_position: HistoryPosition,
    history_height: usize,
    history_rows: usize,
    history_viewport: Option<HistoryViewport>,
    selection: Option<Selection>,
    drag: Option<Drag>,
    pending_copy: bool,
    pending_copy_text: Option<String>,
    notice: Option<Notice>,
    flash: Option<Flash>,
    settings: Option<SettingsContext>,
    workspace_root: Option<String>,
    home: Option<PathBuf>,
    session_name: Option<String>,
    settings_selection: ModelSelection,
    session_thinking: BTreeMap<&'static str, &'static str>,
    picker: Option<Picker<PickerValue>>,
    completion: slash::Completion,
    slash_overlay: Option<Rect>,
    preview: Option<ThinkingPreview>,
    next_settings_revision: u64,
    pending_settings_revision: Option<u64>,
}

impl Default for App {
    fn default() -> Self {
        let settings_selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("default model thinking must be supported");
        Self {
            mode: AppMode::Configured,
            prompt: PromptEditor::default(),
            prompt_width: 80,
            transcript: Transcript::default(),
            subagent_monitor: subagents::SubagentMonitor::default(),
            foreground_cancellation_requested: false,
            history_position: HistoryPosition::FollowingBottom,
            history_height: 0,
            history_rows: 0,
            history_viewport: None,
            selection: None,
            drag: None,
            pending_copy: false,
            pending_copy_text: None,
            notice: None,
            flash: None,
            settings: None,
            workspace_root: None,
            home: None,
            session_name: None,
            settings_selection,
            session_thinking: BTreeMap::from([(DEFAULT_MODEL.id, DEFAULT_MODEL.default_thinking)]),
            picker: None,
            completion: slash::Completion::default(),
            slash_overlay: None,
            preview: None,
            next_settings_revision: 0,
            pending_settings_revision: None,
        }
    }
}

#[derive(Default)]
struct Update {
    commands: Vec<RuntimeCommand>,
    visible_changed: bool,
    quit: bool,
}

impl Update {
    fn with_commands(commands: impl IntoIterator<Item = RuntimeCommand>) -> Self {
        let commands = commands.into_iter().collect::<Vec<_>>();
        Self {
            commands,
            ..Self::default()
        }
    }
}

impl App {
    fn configured(startup: StartupSnapshot) -> Self {
        let settings_selection = ModelSelection::new(startup.model(), startup.thinking())
            .expect("startup thinking must be supported by its model");
        let settings = startup.settings().clone();
        let workspace_root = startup
            .workspace_root()
            .map(|path| normalise_single_line(path.as_str()));
        let home = settings
            .user_path()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        Self {
            transcript: Transcript::configured(startup),
            settings: Some(settings),
            workspace_root,
            home,
            session_name: None,
            settings_selection,
            session_thinking: BTreeMap::from([(
                settings_selection.model().id,
                settings_selection.thinking(),
            )]),
            ..Self::default()
        }
    }

    fn configuration(startup: StartupSnapshot) -> Self {
        let mut app = Self::configured(startup);
        app.mode = AppMode::ConfigurationOnly;
        app
    }

    fn is_configuration_only(&self) -> bool {
        self.mode == AppMode::ConfigurationOnly
    }

    fn is_submission_disabled(&self) -> bool {
        self.is_configuration_only() || self.transcript.is_disabled()
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Update {
        if let Some(update) = self.handle_slash_overlay_mouse(mouse) {
            return update;
        }
        let notice_cleared = self.clear_notice();
        let mut update = match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_older(MOUSE_WHEEL_SCROLL_ROWS),
            MouseEventKind::ScrollDown => self.scroll_newer(MOUSE_WHEEL_SCROLL_ROWS),
            MouseEventKind::Down(MouseButton::Left) => self.begin_selection(mouse, now),
            MouseEventKind::Drag(MouseButton::Left) => Update {
                commands: Vec::new(),
                visible_changed: self.update_drag(mouse, now),
                quit: false,
            },
            MouseEventKind::Up(MouseButton::Left) => self.finish_selection(mouse, now),
            _ => Update::default(),
        };
        update.visible_changed |= notice_cleared;
        update
    }

    fn handle_slash_overlay_mouse(&mut self, mouse: MouseEvent) -> Option<Update> {
        if !matches!(
            mouse.kind,
            MouseEventKind::Down(MouseButton::Left)
                | MouseEventKind::Drag(MouseButton::Left)
                | MouseEventKind::Up(MouseButton::Left)
        ) || !self
            .slash_overlay
            .is_some_and(|overlay| overlay.contains(Position::new(mouse.column, mouse.row)))
        {
            return None;
        }
        let Some(_) = self.drag.take() else {
            return Some(Update::default());
        };
        let visible_changed = self.selection.take().is_some();
        self.pending_copy = false;
        self.pending_copy_text = None;
        Some(Update {
            commands: Vec::new(),
            visible_changed,
            quit: false,
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> Update {
        self.handle_key_at(key, Instant::now())
    }

    fn handle_key_at(&mut self, key: KeyEvent, now: Instant) -> Update {
        if key.is_release() {
            return Update::default();
        }
        let notice_cleared = self.clear_notice();
        let mut update = self.handle_key_inner(key, now);
        update.visible_changed |= notice_cleared;
        update
    }

    fn handle_paste(&mut self, text: &str) -> Update {
        let notice_cleared = self.clear_notice();
        let prompt_changed = if self.is_submission_disabled() || self.picker.is_some() {
            false
        } else {
            let text = normalise_terminal_text(&normalise_paste_line_endings(text));
            self.prompt.insert_paste(&text)
        };
        Update {
            commands: Vec::new(),
            visible_changed: prompt_changed || self.refresh_completion() || notice_cleared,
            quit: false,
        }
    }

    fn handle_key_inner(&mut self, key: KeyEvent, now: Instant) -> Update {
        if key.is_release() {
            return Update::default();
        }
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c' | 'C') => {
                    if !self.is_submission_disabled() && !self.prompt.is_empty() {
                        self.prompt.take();
                        return Update {
                            commands: Vec::new(),
                            visible_changed: true,
                            quit: false,
                        };
                    }
                    let mut update = Update::with_commands(
                        (!self.is_configuration_only()).then_some(RuntimeCommand::Shutdown),
                    );
                    update.quit = true;
                    return update;
                }
                KeyCode::Char('t' | 'T') => {
                    let visible_changed = self.transcript.toggle_reasoning();
                    if visible_changed {
                        self.invalidate_selection();
                    }
                    return Update {
                        commands: Vec::new(),
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
                        commands: Vec::new(),
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
        if key.code == KeyCode::Esc {
            if self.is_configuration_only() {
                return Update::default();
            }
            if !self.transcript.is_disabled()
                && !self.transcript.is_idle()
                && !self.foreground_cancellation_requested
            {
                self.foreground_cancellation_requested = true;
                return Update::with_commands([RuntimeCommand::Cancel]);
            }
            return Update::with_commands([RuntimeCommand::CancelBackground]);
        }
        if self.is_submission_disabled() {
            return Update::default();
        }
        if is_shift_tab(key) {
            return self.cycle_thinking_preview(now);
        }
        if let Some(update) = self.handle_completion_key(key) {
            return update;
        }
        if key.code == KeyCode::Enter && key.modifiers.is_empty() && !self.prompt.is_empty() {
            let draft = self.prompt.take();
            if let Some(command) = slash::parse(&draft) {
                return self.handle_slash_command(command);
            }
            if self.transcript.is_idle() {
                let mut commands = Vec::new();
                self.flush_preview(&mut commands);
                self.transcript.begin_submission();
                commands.push(RuntimeCommand::Submit { request: draft });
                let mut update = Update::with_commands(commands);
                update.visible_changed = true;
                return update;
            }
            self.prompt.insert_paste(&draft);
            return Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
        if let Some(edit) = self.prompt_edit(key) {
            let prompt_changed = self.prompt.apply(edit, self.prompt_width);
            return Update {
                commands: Vec::new(),
                visible_changed: prompt_changed || self.refresh_completion(),
                quit: false,
            };
        }
        Update::default()
    }

    fn handle_completion_key(&mut self, key: KeyEvent) -> Option<Update> {
        self.refresh_completion();
        if !key.modifiers.is_empty() || self.completion.selected_token().is_none() {
            return None;
        }
        let visible_changed = match key.code {
            KeyCode::Up => self.completion.move_selection(-1),
            KeyCode::Down => self.completion.move_selection(1),
            KeyCode::Tab | KeyCode::Enter => {
                let token = self.completion.selected_token()?;
                self.prompt.take();
                self.prompt.insert_paste(token);
                self.refresh_completion();
                true
            }
            _ => return None,
        };
        Some(Update {
            commands: Vec::new(),
            visible_changed,
            quit: false,
        })
    }

    fn refresh_completion(&mut self) -> bool {
        self.completion
            .sync(self.prompt.text(), self.prompt.caret_at_end())
    }

    fn handle_slash_command(&mut self, command: slash::Parsed<'_>) -> Update {
        match command.action {
            slash::Action::Model => {
                let argument = command.argument;
                if let Some(model) = CATALOGUE
                    .iter()
                    .copied()
                    .find(|model| model.id.eq_ignore_ascii_case(argument))
                {
                    self.select_model(model)
                } else {
                    self.open_model_picker(argument);
                    Update {
                        commands: Vec::new(),
                        visible_changed: true,
                        quit: false,
                    }
                }
            }
            slash::Action::Name => self.name_session(command.argument),
            slash::Action::Thinking => {
                let argument = command.argument;
                let model = self.settings_selection.model();
                if argument.is_empty() {
                    self.open_thinking_picker();
                    return Update {
                        commands: Vec::new(),
                        visible_changed: true,
                        quit: false,
                    };
                }
                if let Some(thinking) = model
                    .thinking_levels
                    .iter()
                    .copied()
                    .find(|thinking| thinking.eq_ignore_ascii_case(argument))
                {
                    self.select_thinking(thinking)
                } else {
                    self.set_notice(format!(
                        "Unsupported thinking for {}; valid: {}.",
                        model.id,
                        model.thinking_levels.join(", ")
                    ));
                    Update {
                        commands: Vec::new(),
                        visible_changed: true,
                        quit: false,
                    }
                }
            }
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent) -> Update {
        let action = self
            .picker
            .as_mut()
            .expect("picker was checked before handling its key")
            .handle_key(key);
        match action {
            PickerAction::None => Update::default(),
            PickerAction::Changed => Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            },
            PickerAction::Close => {
                self.picker = None;
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            PickerAction::NoMatch => {
                let message = self
                    .picker
                    .as_ref()
                    .expect("picker was checked before reporting no match")
                    .empty_message();
                self.set_notice(message.to_owned());
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            PickerAction::Apply(value) => {
                self.picker = None;
                self.apply_picker_value(value)
            }
            PickerAction::Save(value) => {
                self.picker = None;
                let mut update = self.apply_picker_value(value);
                match value {
                    PickerValue::Model(model) => {
                        self.save_user_default(|settings| settings.save_model(model))
                    }
                    PickerValue::Thinking(thinking) => {
                        let model = self.settings_selection.model();
                        self.save_user_default(|settings| settings.save_thinking(model, thinking));
                    }
                }
                update.visible_changed = true;
                update
            }
        }
    }

    fn apply_picker_value(&mut self, value: PickerValue) -> Update {
        match value {
            PickerValue::Model(model) => self.select_model(model),
            PickerValue::Thinking(thinking) => self.select_thinking(thinking),
        }
    }

    fn open_model_picker(&mut self, filter: &str) {
        let items = CATALOGUE.iter().copied().map(|model| PickerItem {
            group: "OpenAI",
            label: model.id,
            value: PickerValue::Model(model),
        });
        let mut picker = Picker::new(
            "Models",
            "No matching models",
            items,
            PickerValue::Model(self.settings_selection.model()),
        );
        picker.set_filter(filter);
        self.picker = Some(picker);
    }

    fn open_thinking_picker(&mut self) {
        let model = self.settings_selection.model();
        let items = model
            .thinking_levels
            .iter()
            .copied()
            .map(|thinking| PickerItem {
                group: "Thinking",
                label: thinking,
                value: PickerValue::Thinking(thinking),
            });
        self.picker = Some(Picker::new(
            "Thinking",
            "No matching thinking levels",
            items,
            PickerValue::Thinking(self.settings_selection.thinking()),
        ));
    }

    fn select_model(&mut self, model: &'static ModelConfiguration) -> Update {
        let thinking = self
            .session_thinking
            .get(model.id)
            .copied()
            .or_else(|| {
                self.settings
                    .as_ref()
                    .and_then(|settings| settings.thinking_for(model))
            })
            .unwrap_or(model.default_thinking);
        let selection = ModelSelection::new(model, thinking)
            .expect("stored thinking must be supported by its model");
        self.apply_selection(selection)
    }

    fn select_thinking(&mut self, thinking: &'static str) -> Update {
        let model = self.settings_selection.model();
        let selection = ModelSelection::new(model, thinking)
            .expect("picker thinking must be supported by its model");
        self.apply_selection(selection)
    }

    fn apply_selection(&mut self, selection: ModelSelection) -> Update {
        self.settings_selection = selection;
        self.preview = None;
        let mut update = Update::with_commands([self.send_settings(selection)]);
        update.visible_changed = true;
        update
    }

    fn cycle_thinking_preview(&mut self, now: Instant) -> Update {
        let model = self.settings_selection.model();
        let current = self.settings_selection.thinking();
        let index = model
            .thinking_levels
            .iter()
            .position(|thinking| *thinking == current)
            .expect("selection thinking must be supported by its model");
        let next = model.thinking_levels[(index + 1) % model.thinking_levels.len()];
        let selection = ModelSelection::new(model, next)
            .expect("catalogue thinking must be supported by its model");
        self.settings_selection = selection;
        self.preview = Some(ThinkingPreview {
            selection,
            deadline: now + Duration::from_secs(2),
            sent_revision: None,
        });
        Update {
            commands: Vec::new(),
            visible_changed: true,
            quit: false,
        }
    }

    fn flush_preview(&mut self, commands: &mut Vec<RuntimeCommand>) {
        let Some(preview) = self.preview.take() else {
            return;
        };
        if preview.sent_revision.is_none() {
            commands.push(self.send_settings(preview.selection));
        }
    }

    fn send_settings(&mut self, selection: ModelSelection) -> RuntimeCommand {
        self.next_settings_revision = self
            .next_settings_revision
            .checked_add(1)
            .expect("settings revision overflowed");
        let revision = self.next_settings_revision;
        self.session_thinking
            .insert(selection.model().id, selection.thinking());
        self.pending_settings_revision = Some(revision);
        RuntimeCommand::ApplySettings {
            selection,
            revision,
        }
    }

    fn save_user_default(
        &mut self,
        save: impl FnOnce(&SettingsContext) -> Result<SaveOutcome, SaveError>,
    ) {
        let result = self
            .settings
            .as_ref()
            .ok_or_else(|| "user settings are unavailable".to_owned())
            .and_then(|settings| save(settings).map_err(|error| error.to_string()));
        self.set_notice(match result {
            Ok(outcome) => outcome.to_string(),
            Err(error) => format!("Could not save user default: {error}."),
        });
    }

    fn set_notice(&mut self, notice: String) {
        self.notice = Some(Notice::Status(notice));
    }

    fn name_session(&mut self, argument: &str) -> Update {
        let name = normalise_single_line(argument);
        if name.is_empty() {
            self.set_notice(self.session_name.as_ref().map_or_else(
                || "Usage: /name <name>".to_owned(),
                |name| format!("Session name: {name}"),
            ));
            return Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
        if Line::raw(&name).width() > 80 {
            self.set_notice("Session name must be at most 80 display cells.".to_owned());
            return Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
        self.session_name = Some(name);
        Update {
            commands: Vec::new(),
            visible_changed: true,
            quit: false,
        }
    }

    fn title(&self) -> String {
        match &self.session_name {
            Some(name) => format!("✷ - {name}"),
            None => format!(
                "✷ - {}",
                self.cwd_basename().unwrap_or_else(|| "kiki".to_owned())
            ),
        }
    }

    fn cwd_basename(&self) -> Option<String> {
        self.workspace_root.as_deref().and_then(|workspace_root| {
            Path::new(workspace_root)
                .file_name()
                .map(|basename| normalise_single_line(&basename.to_string_lossy()))
                .filter(|basename| !basename.is_empty())
        })
    }

    fn footer_context(&self) -> Option<FooterContext> {
        self.settings.as_ref().map(|_| FooterContext {
            workspace_root: self.workspace_root.clone(),
            home: self.home.clone(),
            name: self.session_name.clone(),
            selection: self.settings_selection,
        })
    }

    fn begin_selection(&mut self, mouse: MouseEvent, now: Instant) -> Update {
        let visible_changed = self.invalidate_selection();
        let Some(position) = self.history_document_position(mouse.column, mouse.row) else {
            return Update {
                commands: Vec::new(),
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
            commands: Vec::new(),
            visible_changed,
            quit: false,
        }
    }

    fn update_drag(&mut self, mouse: MouseEvent, now: Instant) -> bool {
        let Some(viewport) = self.history_viewport else {
            return false;
        };
        let Some(focus) = self.history_document_position(mouse.column, mouse.row) else {
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
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
        Update {
            commands: Vec::new(),
            visible_changed,
            quit: false,
        }
    }

    fn tick(&mut self, now: Instant) -> Update {
        let mut update = self.tick_thinking_preview(now);
        update.visible_changed |=
            self.tick_edge_scroll(now) | self.tick_flash(now) | self.subagent_monitor.tick(now);
        update
    }

    fn tick_thinking_preview(&mut self, now: Instant) -> Update {
        let Some(preview) = self.preview else {
            return Update::default();
        };
        if now < preview.deadline || preview.sent_revision.is_some() {
            return Update::default();
        }
        let command = self.send_settings(preview.selection);
        self.preview
            .as_mut()
            .expect("preview was checked before sending settings")
            .sent_revision = Some(self.next_settings_revision);
        Update::with_commands([command])
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
        let update = match edge {
            Edge::Older => self.scroll_older(1),
            Edge::Newer => self.scroll_newer(1),
        };
        if !update.visible_changed {
            if let Some(drag) = &mut self.drag {
                drag.edge = None;
            }
            return false;
        }
        let focus = self
            .history_document_position(drag.column, drag.row)
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

    fn newest_history_top(&self) -> usize {
        self.history_rows.saturating_sub(self.history_height)
    }

    fn history_document_top(&self) -> usize {
        match self.history_position {
            HistoryPosition::FollowingBottom => self.newest_history_top(),
            HistoryPosition::Anchored(top) => top,
        }
    }

    fn history_document_position(&self, column: u16, row: u16) -> Option<DocumentPosition> {
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

    fn notice(&self) -> Option<&str> {
        match &self.notice {
            Some(Notice::Copied) => Some("Copied"),
            Some(Notice::CopyFailed) => Some("Copy failed"),
            Some(Notice::Status(notice)) => Some(notice),
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
        self.history_viewport = None;
        self.invalidate_selection();
    }

    fn handle_history_key(&mut self, key: KeyEvent) -> Option<Update> {
        let editable = !self.is_submission_disabled();
        match key.code {
            KeyCode::PageUp => Some(self.scroll_older(10)),
            KeyCode::PageDown => Some(self.scroll_newer(10)),
            KeyCode::Home if !editable || key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(self.scroll_oldest())
            }
            KeyCode::End if !editable || key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(self.follow_history())
            }
            KeyCode::Up if !editable => Some(self.scroll_older(1)),
            KeyCode::Down if !editable => Some(self.scroll_newer(1)),
            _ => None,
        }
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
            KeyCode::Char('j' | 'J') if key.modifiers == KeyModifiers::CONTROL => {
                Some(PromptEdit::InsertNewline)
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
            KeyCode::Enter if key.modifiers == KeyModifiers::SHIFT => {
                Some(PromptEdit::InsertNewline)
            }
            _ => None,
        }
    }

    fn handle_event(&mut self, event: RuntimeEvent) -> bool {
        self.handle_event_at(event, Instant::now())
    }

    fn handle_event_at(&mut self, event: RuntimeEvent, now: Instant) -> bool {
        self.handle_event_with_gap_at(event, false, now)
    }

    fn handle_progress(&mut self, envelope: ProgressEnvelope<RuntimeEvent>) -> bool {
        self.handle_event_with_gap_at(envelope.item, envelope.gap_before, Instant::now())
    }

    fn handle_event_with_gap_at(
        &mut self,
        event: RuntimeEvent,
        gap_before: bool,
        now: Instant,
    ) -> bool {
        if matches!(&event, RuntimeEvent::RunSettled { .. }) {
            self.foreground_cancellation_requested = false;
        }
        if let RuntimeEvent::SettingsAccepted { revision } = &event {
            self.reconcile_settings(*revision);
        }
        let monitor_changed = self.subagent_monitor.apply(&event, now);
        let transcript_changed = self.transcript.apply(event, gap_before);
        if transcript_changed {
            self.invalidate_selection();
        }
        monitor_changed || transcript_changed
    }

    fn reconcile_settings(&mut self, revision: u64) {
        if self.pending_settings_revision != Some(revision) {
            return;
        }
        if self
            .preview
            .is_some_and(|preview| preview.sent_revision != Some(revision))
        {
            return;
        }
        self.pending_settings_revision = None;
        if self
            .preview
            .is_some_and(|preview| preview.sent_revision == Some(revision))
        {
            self.preview = None;
        }
    }

    fn scroll_older(&mut self, rows: usize) -> Update {
        if self.history_viewport.is_some() {
            self.set_history_position(
                self.history_position_at(self.history_document_top().saturating_sub(rows)),
            )
        } else {
            Update::default()
        }
    }

    fn scroll_newer(&mut self, rows: usize) -> Update {
        if self.history_viewport.is_some() {
            self.set_history_position(
                self.history_position_at(self.history_document_top().saturating_add(rows)),
            )
        } else {
            Update::default()
        }
    }

    fn scroll_oldest(&mut self) -> Update {
        if self.history_rows > self.history_height {
            self.set_history_position(HistoryPosition::Anchored(0))
        } else {
            Update::default()
        }
    }

    fn follow_history(&mut self) -> Update {
        self.set_history_position(HistoryPosition::FollowingBottom)
    }

    fn history_position_at(&self, top: usize) -> HistoryPosition {
        if top >= self.newest_history_top() {
            HistoryPosition::FollowingBottom
        } else {
            HistoryPosition::Anchored(top)
        }
    }

    fn set_history_position(&mut self, position: HistoryPosition) -> Update {
        if self.history_position == position {
            Update::default()
        } else {
            self.history_position = position;
            Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            }
        }
    }

    fn normalise_history_position(&mut self) {
        if let HistoryPosition::Anchored(top) = self.history_position {
            self.history_position = self.history_position_at(top);
        }
    }
}

fn is_shift_tab(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::BackTab => key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT,
        KeyCode::Tab => key.modifiers == KeyModifiers::SHIFT,
        _ => false,
    }
}

fn normalise_single_line(text: &str) -> String {
    normalise_terminal_text(&text.split_whitespace().collect::<Vec<_>>().join(" "))
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

trait TitlePublisher {
    fn publish(&mut self, title: &str) -> io::Result<()>;
}

struct TerminalTitlePublisher;

impl TitlePublisher for TerminalTitlePublisher {
    fn publish(&mut self, title: &str) -> io::Result<()> {
        let mut stdout = io::stdout();
        ratatui::crossterm::execute!(stdout, SetTitle(title))
    }
}

#[cfg(test)]
struct NoopTitlePublisher;

#[cfg(test)]
impl TitlePublisher for NoopTitlePublisher {
    fn publish(&mut self, _title: &str) -> io::Result<()> {
        Ok(())
    }
}

fn publish_title(app: &mut App, publisher: &mut impl TitlePublisher) -> bool {
    if publisher.publish(&app.title()).is_err() {
        app.set_notice("Could not change terminal title.".to_owned());
        true
    } else {
        false
    }
}

pub(crate) fn run(
    startup: StartupSnapshot,
    commands: mpsc::Sender<RuntimeCommand>,
    mut events: mpsc::UnboundedReceiver<RuntimeEvent>,
    mut progress: RuntimeProgressReceiver,
    trace: Trace,
) -> Result<(), TuiError> {
    let mut app = App::configured(startup);
    let mut input = CrosstermEvents;
    let mut clipboard = LocalClipboard::new();
    let mut titles = TerminalTitlePublisher;
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| {
            with_bracketed_paste(enable_bracketed_paste, disable_bracketed_paste, || {
                with_mouse_capture(enable_mouse_capture, disable_mouse_capture, || {
                    with_keyboard_enhancement(
                        enable_keyboard_enhancement,
                        disable_keyboard_enhancement,
                        || {
                            run_loop_with_titles(
                                terminal,
                                &mut app,
                                &mut input,
                                &commands,
                                &mut events,
                                &mut progress,
                                &mut clipboard,
                                &mut titles,
                                &trace,
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
    let mut titles = TerminalTitlePublisher;
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| {
            with_mouse_capture(enable_mouse_capture, disable_mouse_capture, || {
                publish_title(&mut app, &mut titles);
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

fn enable_keyboard_enhancement() -> io::Result<()> {
    let mut stdout = io::stdout();
    ratatui::crossterm::execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
}

fn disable_keyboard_enhancement() {
    let mut stdout = io::stdout();
    let _ = ratatui::crossterm::execute!(stdout, PopKeyboardEnhancementFlags);
}

fn with_keyboard_enhancement<T>(
    enable: impl FnOnce() -> io::Result<()>,
    disable: impl FnOnce(),
    run: impl FnOnce() -> T,
) -> T {
    let pushed = enable().is_ok();
    let _guard = pushed.then(|| RestoreGuard::new(disable));
    run()
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

        dirty |= app.tick(Instant::now()).visible_changed;
    }
}

fn dispatch_commands(
    commands: &mpsc::Sender<RuntimeCommand>,
    update: &Update,
    trace: &Trace,
) -> Result<(), TuiError> {
    for command in &update.commands {
        let control = control_command(command);
        if let Some(command) = control {
            trace.emit(|| TraceEvent::TuiCommandDispatch { command });
        }
        commands
            .blocking_send(command.clone())
            .map_err(|_| TuiError::RuntimeClosed)?;
        if let Some(command) = control {
            trace.emit(|| TraceEvent::TuiCommandQueued { command });
        }
    }
    Ok(())
}

#[cfg(test)]
fn run_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    input: &mut impl EventSource,
    commands: &mpsc::Sender<RuntimeCommand>,
    events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    progress: &mut RuntimeProgressReceiver,
    clipboard: &mut impl ClipboardPort,
) -> Result<usize, TuiError> {
    run_loop_with_titles(
        terminal,
        app,
        input,
        commands,
        events,
        progress,
        clipboard,
        &mut NoopTitlePublisher,
        &Trace::disabled(),
    )
}

#[allow(clippy::too_many_arguments)]
fn run_loop_with_titles<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    input: &mut impl EventSource,
    commands: &mpsc::Sender<RuntimeCommand>,
    events: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    progress: &mut RuntimeProgressReceiver,
    clipboard: &mut impl ClipboardPort,
    titles: &mut impl TitlePublisher,
    trace: &Trace,
) -> Result<usize, TuiError> {
    let mut dirty = true;
    publish_title(app, titles);
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
                    let previous_title = app.title();
                    let update = app.handle_key_at(key, Instant::now());
                    dirty |= update.visible_changed;
                    if previous_title != app.title() {
                        dirty |= publish_title(app, titles);
                    }
                    dispatch_commands(commands, &update, trace)?;
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

        let update = app.tick(Instant::now());
        dirty |= update.visible_changed;
        dispatch_commands(commands, &update, trace)?;
    }
}

const PROMPT_GUTTER_WIDTH: u16 = 2;

fn render(frame: &mut Frame, app: &mut App) {
    render_at(frame, app, Instant::now());
}

fn render_at(frame: &mut Frame, app: &mut App, now: Instant) {
    let area = frame.area();
    if let Some(stale_overlay) = app.slash_overlay.take() {
        frame.render_widget(Clear, stale_overlay.intersection(area));
    }
    frame.buffer_mut().set_style(area, page_style());
    if area.width == 0 || area.height == 0 {
        let _ = app.subagent_monitor.layout(area.width, 0, now);
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
            "Status: {}{notice} | Input disabled | Up/Down scroll | Ctrl-T thinking | Ctrl-O expand | Ctrl-C quit",
            app.transcript.status()
        )
    } else {
        let notice = notice.map_or(String::new(), |notice| format!(" | {notice}"));
        format!(
            "Status: {}{notice} | Enter submit | Shift-Enter/Ctrl-J newline | Arrows edit | PgUp/PgDn scroll | Esc cancel | Ctrl-T thinking | Ctrl-O expand | Ctrl-C clear/quit",
            app.transcript.status()
        )
    };
    frame.render_widget(
        Paragraph::new(normalise_terminal_text(&header)),
        Rect::new(area.x, area.y, area.width, 1),
    );

    if configuration_only {
        let _ = app.subagent_monitor.layout(area.width, 0, now);
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
        let _ = app.subagent_monitor.layout(area.width, 0, now);
        app.history_unavailable();
        return;
    }

    let bottom_rule = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        1,
    );
    if area.height <= 3 {
        let _ = app.subagent_monitor.layout(area.width, 0, now);
        let rule_style = thinking_rule_style(app.settings_selection);
        if area.height == 3 {
            render_rule(
                frame,
                Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
                rule_style,
            );
        }
        let layout = match app.footer_context() {
            Some(context) => app
                .transcript
                .layout_with_footer(bottom_rule.width, context),
            None => app.transcript.layout(bottom_rule.width),
        };
        frame.render_widget(
            Paragraph::new(layout.footer_rule(bottom_rule.width)).style(rule_style),
            bottom_rule,
        );
        app.history_unavailable();
        return;
    }

    app.prompt_width = area.width.saturating_sub(PROMPT_GUTTER_WIDTH);
    app.refresh_completion();
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
    let history_height = area
        .height
        .saturating_sub(1)
        .saturating_sub(prompt_box_height);
    let monitor = app
        .subagent_monitor
        .layout(area.width, history_height.saturating_sub(1), now);
    let monitor_top = prompt_top.saturating_sub(monitor.height());
    let history = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        monitor_top.saturating_sub(area.y.saturating_add(1)),
    );
    let completion_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        prompt_top.saturating_sub(area.y.saturating_add(1)),
    );
    let layout = render_history(frame, app, history);
    monitor.render(
        frame,
        Rect::new(area.x, monitor_top, area.width, monitor.height()),
    );
    render_slash_overlay(frame, app, completion_area);
    let rule_style = thinking_rule_style(app.settings_selection);
    render_rule(
        frame,
        Rect::new(area.x, prompt_top, area.width, 1),
        rule_style,
    );
    render_prompt(
        frame,
        &prompt,
        content,
        !app.is_submission_disabled() && app.picker.is_none(),
        slash::recognised_token_range(app.prompt.text()),
    );
    frame.render_widget(
        Paragraph::new(layout.footer_rule(bottom_rule.width)).style(rule_style),
        bottom_rule,
    );
    if let Some(picker) = &mut app.picker {
        picker.render(frame, area);
    }
}

fn render_history(frame: &mut Frame, app: &mut App, viewport: Rect) -> TranscriptLayout {
    let layout = match app.footer_context() {
        Some(context) => app.transcript.layout_with_footer(viewport.width, context),
        None => app.transcript.layout(viewport.width),
    };
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
    app.history_viewport = Some(HistoryViewport { area: viewport });
    app.normalise_history_position();
    let top = app.history_document_top();
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

fn render_slash_overlay(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.is_submission_disabled() || app.picker.is_some() {
        return;
    }
    let rows = app.completion.visible_rows(usize::from(area.height));
    let Some(height) = u16::try_from(rows.len()).ok().filter(|height| *height > 0) else {
        return;
    };
    let natural_width = rows
        .iter()
        .map(|row| Line::raw(format!("{} {}", row.token, row.purpose)).width())
        .max()
        .unwrap_or(0);
    let width = area
        .width
        .min(u16::try_from(natural_width).unwrap_or(u16::MAX));
    if width == 0 {
        return;
    }
    let x = area
        .x
        .saturating_add(PROMPT_GUTTER_WIDTH)
        .min(area.right().saturating_sub(width));
    let overlay = Rect::new(x, area.bottom().saturating_sub(height), width, height);
    frame.render_widget(Clear, overlay);
    frame.buffer_mut().set_style(overlay, page_style());
    let lines = rows.into_iter().map(|row| {
        let text = format!("{} {}", row.token, row.purpose);
        let padding = usize::from(width).saturating_sub(Line::raw(&text).width());
        let style = if row.selected {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        Line::styled(format!("{text}{}", " ".repeat(padding)), style)
    });
    frame.render_widget(Paragraph::new(Text::from_iter(lines)), overlay);
    app.slash_overlay = Some(overlay);
}

fn prompt_content_capacity(height: u16) -> usize {
    let prompt_box_capacity = usize::from(height)
        .saturating_div(3)
        .max(3)
        .min(usize::from(height.saturating_sub(1)));
    prompt_box_capacity.saturating_sub(2).max(1)
}

fn render_prompt(
    frame: &mut Frame,
    prompt: &PromptLayout,
    content: Rect,
    cursor_visible: bool,
    recognised_token: Option<Range<usize>>,
) {
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
        .map(|row| prompt_line(row, recognised_token.as_ref()))
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

fn prompt_line<'a>(row: &'a PromptRow, recognised_token: Option<&Range<usize>>) -> Line<'a> {
    let source = row.source();
    let Some(token) =
        recognised_token.filter(|token| token.start < source.end && source.start < token.end)
    else {
        return Line::raw(row.text());
    };
    let start = token.start.max(source.start) - source.start;
    let end = token.end.min(source.end) - source.start;
    let text = row.text();
    Line::from(vec![
        Span::raw(&text[..start]),
        Span::styled(
            &text[start..end],
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw(&text[end..]),
    ])
}

const THINKING_RULE_COLOURS: [Color; 6] = [
    Color::Rgb(102, 102, 102),
    Color::Rgb(63, 94, 158),
    Color::Rgb(91, 77, 169),
    Color::Rgb(139, 63, 192),
    Color::Rgb(190, 45, 209),
    Color::Rgb(255, 0, 255),
];

fn thinking_rule_style(selection: ModelSelection) -> Style {
    Style::new().fg(thinking_rule_colour(
        selection.model().thinking_levels,
        selection.thinking(),
    ))
}

fn thinking_rule_colour(levels: &[&str], selected: &str) -> Color {
    let index = levels
        .iter()
        .position(|level| *level == selected)
        .expect("selection thinking must be supported by its model");
    let denominator = levels.len().saturating_sub(1);
    if denominator == 0 {
        return THINKING_RULE_COLOURS[0];
    }
    let position = index * (THINKING_RULE_COLOURS.len() - 1);
    let lower = position / denominator;
    let remainder = position % denominator;
    let upper = (lower + usize::from(remainder > 0)).min(THINKING_RULE_COLOURS.len() - 1);
    interpolate_colour(
        THINKING_RULE_COLOURS[lower],
        THINKING_RULE_COLOURS[upper],
        remainder,
        denominator,
    )
}

fn interpolate_colour(start: Color, end: Color, remainder: usize, denominator: usize) -> Color {
    let (Color::Rgb(red, green, blue), Color::Rgb(end_red, end_green, end_blue)) = (start, end)
    else {
        unreachable!("thinking palette contains only RGB colours")
    };
    let blend = |start, end| {
        u8::try_from(
            (usize::from(start) * (denominator - remainder)
                + usize::from(end) * remainder
                + denominator / 2)
                / denominator,
        )
        .expect("interpolated colour is an RGB channel")
    };
    Color::Rgb(
        blend(red, end_red),
        blend(green, end_green),
        blend(blue, end_blue),
    )
}

fn render_rule(frame: &mut Frame, area: Rect, style: Style) {
    frame.render_widget(
        Paragraph::new(Line::raw("─".repeat(usize::from(area.width)))).style(style),
        area,
    );
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::fs;
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

    use super::{App, HistoryPosition, TitlePublisher};
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, Observation, PythonError, ToolCall, ToolCallSummary,
        ToolCallToken, ToolName,
    };
    use crate::clipboard::{ClipboardError, ClipboardPort};
    use crate::config::models::{ASTRA, SOL, TERRA};
    use crate::model::{AttemptAccounting, ModelProgress, ModelUsage};
    use crate::prompt_editor::{MAX_PROMPT_BYTES, PromptEdit, PromptEditor};
    use crate::runtime::{
        ModelStepId, Record, RunId, RuntimeCommand, RuntimeDisabledReason, RuntimeEvent,
        StoredRecord, TerminalDetails,
    };
    use crate::subagents::TaskState;
    use crate::telemetry::{Trace, TraceOwner};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

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
        let update = app.handle_key(key(KeyCode::Enter));
        let Some(RuntimeCommand::Submit { request }) = update.commands.into_iter().next() else {
            panic!("prompt was not submitted")
        };
        request
    }

    fn enter_at(app: &mut App, prompt: &str, now: Instant) -> super::Update {
        set_prompt(app, prompt);
        app.handle_key_at(key(KeyCode::Enter), now)
    }

    fn applied_settings(update: &super::Update) -> (crate::config::ModelSelection, u64) {
        let [
            RuntimeCommand::ApplySettings {
                selection,
                revision,
            },
        ] = update.commands.as_slice()
        else {
            panic!("expected one settings command, got {:?}", update.commands);
        };
        (*selection, *revision)
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
        rendered_at(app, width, height, Instant::now())
    }

    fn rendered_at(app: &mut App, width: u16, height: u16, now: Instant) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render_at(frame, app, now))
            .unwrap();
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

    fn history_top_row(screen: &str, viewport: super::HistoryViewport) -> &str {
        screen
            .lines()
            .nth(usize::from(viewport.area.y))
            .expect("history top row")
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

    #[test]
    fn responsiveness_p95_uses_the_48th_sorted_sample() {
        let mut samples: Vec<_> = (1..=50).rev().map(Duration::from_nanos).collect();

        assert_eq!(responsiveness_p95(&mut samples), Duration::from_nanos(48));
    }

    #[test]
    fn responsiveness_p95_of_four_samples_is_the_maximum() {
        let mut samples = [3, 1, 4, 2].map(Duration::from_nanos).to_vec();

        assert_eq!(responsiveness_p95(&mut samples), Duration::from_nanos(4));
    }

    #[test]
    fn responsiveness_tolerance_is_inclusive() {
        for tolerance in [Duration::from_nanos(16_700_000), Duration::from_millis(50)] {
            for (observed, expected) in [
                (tolerance, true),
                (tolerance + Duration::from_nanos(1), false),
            ] {
                assert_eq!(observed <= tolerance, expected);
            }
        }
    }

    fn responsiveness_p95(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        samples[(samples.len() * 95).div_ceil(100) - 1]
    }

    fn require_release_benchmark() {
        #[cfg(debug_assertions)]
        panic!("responsiveness benchmark requires a release build");
    }

    struct ResponsivenessFrame {
        key: KeyEvent,
        delta: RuntimeEvent,
    }

    fn responsiveness_frame(code: KeyCode, sequence: u16) -> ResponsivenessFrame {
        ResponsivenessFrame {
            key: key(code),
            delta: responsiveness_delta(sequence),
        }
    }

    fn responsiveness_delta(sequence: u16) -> RuntimeEvent {
        let text = format!("live tick {sequence:04}: 1234567890\n");
        assert_eq!(text.len(), 27);
        RuntimeEvent::ModelProgress {
            run_id: RunId(129),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk { text },
        }
    }

    fn responsiveness_assistant_message() -> String {
        const LINE: &str = "- stable **markdown** [link](https://example.test) `inline`\n";
        let mut message = String::new();
        while message.len() + LINE.len() <= 8_192 {
            message.push_str(LINE);
        }
        message.push_str(&"x".repeat(8_192 - message.len()));
        assert!(message.is_ascii());
        assert_eq!(message.len(), 8_192);
        message
    }

    fn responsiveness_settled_events() -> Vec<RuntimeEvent> {
        let assistant_message = responsiveness_assistant_message();
        let mut events = Vec::with_capacity(128 * 5);
        for run_id in 1..=128 {
            let source = (0..40)
                .map(|line| format!("value_{run_id:03}_{line:02} = {line:02}\n"))
                .collect::<String>();
            assert_eq!(source.lines().count(), 40);
            events.extend([
                RuntimeEvent::RunStarted {
                    run_id: RunId(run_id),
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(run_id),
                        record: Record::UserRequest {
                            request: format!("benchmark request {run_id:03}"),
                        },
                    },
                    progress_gap: false,
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(run_id),
                        record: Record::CompletedExecution {
                            step_id: ModelStepId(1),
                            commentary: None,
                            source,
                            reasoning_summary: format!("settled reasoning {run_id:03}"),
                            execution: Execution {
                                stdout: observation("authoritative stdout\n"),
                                stderr: observation(""),
                                outcome: ExecutionOutcome::Complete {
                                    result: Some(observation("42")),
                                },
                                tool_calls: Vec::new(),
                            },
                            attempts: Vec::new(),
                        },
                    },
                    progress_gap: false,
                },
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(run_id),
                        record: Record::FinalMessage {
                            step_id: ModelStepId(2),
                            text: assistant_message.clone(),
                            reasoning_summary: String::new(),
                            terminal: TerminalDetails::empty(),
                        },
                    },
                    progress_gap: false,
                },
                RuntimeEvent::RunSettled {
                    run_id: RunId(run_id),
                },
            ]);
        }
        events
    }

    #[test]
    #[ignore = "run with --release --ignored to measure TUI responsiveness"]
    fn responsiveness_benchmark() {
        require_release_benchmark();

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let mut clipboard = FakeClipboard::available();
        let settled_events = responsiveness_settled_events();
        let live_setup = [
            RuntimeEvent::RunStarted { run_id: RunId(129) },
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(129),
                step_id: ModelStepId(1),
            },
            RuntimeEvent::ModelProgress {
                run_id: RunId(129),
                step_id: ModelStepId(1),
                progress: ModelProgress::AssistantTextChunk {
                    text: "# live assistant\n\n".to_owned(),
                },
            },
        ];
        let warmups = [
            responsiveness_frame(KeyCode::Char('w'), 1),
            responsiveness_frame(KeyCode::PageUp, 2),
            responsiveness_frame(KeyCode::Char('w'), 3),
            responsiveness_frame(KeyCode::PageDown, 4),
            responsiveness_frame(KeyCode::Char('w'), 5),
            responsiveness_frame(KeyCode::PageUp, 6),
            responsiveness_frame(KeyCode::Char('w'), 7),
            responsiveness_frame(KeyCode::PageDown, 8),
        ];
        let key_frames = (0_u16..50)
            .map(|index| {
                responsiveness_frame(
                    KeyCode::Char(char::from(b'a' + u8::try_from(index % 26).unwrap())),
                    9 + index,
                )
            })
            .collect::<Vec<_>>();
        let scroll_frames = (0_u16..50)
            .map(|index| {
                responsiveness_frame(
                    if index % 2 == 0 {
                        KeyCode::PageUp
                    } else {
                        KeyCode::PageDown
                    },
                    59 + index,
                )
            })
            .collect::<Vec<_>>();
        let clear_draft = modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let raw_paste = "\t12345\r\n".repeat(6_553) + "abcdef";
        let normalised_paste =
            super::normalise_terminal_text(&super::normalise_paste_line_endings(&raw_paste));
        let paste_delta = responsiveness_delta(109);
        let key_scroll_tolerance = Duration::from_nanos(16_700_000);
        let paste_tolerance = Duration::from_millis(50);

        let assistant_messages = settled_events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::RecordAppended {
                    record:
                        StoredRecord {
                            record: Record::FinalMessage { text, .. },
                            ..
                        },
                    ..
                } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistant_messages.len(), 128);
        for message in &assistant_messages {
            assert!(message.is_ascii());
            assert_eq!(message.len(), 8_192);
        }
        assert_eq!(
            assistant_messages
                .iter()
                .map(|message| message.len())
                .sum::<usize>(),
            1_048_576
        );
        assert_eq!(normalised_paste.len(), 65_536);

        for event in settled_events {
            app.handle_event(event);
        }
        for event in live_setup {
            app.handle_event(event);
        }
        super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap();

        for ResponsivenessFrame { key, delta } in warmups {
            app.handle_key(key);
            app.handle_event(delta);
            super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap();
        }
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        let mut key_samples = Vec::with_capacity(50);
        for ResponsivenessFrame { key, delta } in key_frames {
            let started = Instant::now();
            app.handle_key(key);
            app.handle_event(delta);
            super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap();
            key_samples.push(started.elapsed());
        }

        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        let mut scroll_samples = Vec::with_capacity(50);
        for ResponsivenessFrame { key, delta } in scroll_frames {
            let started = Instant::now();
            app.handle_key(key);
            app.handle_event(delta);
            super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap();
            scroll_samples.push(started.elapsed());
        }

        assert!(app.handle_key(clear_draft).visible_changed);
        assert!(app.prompt.is_empty());
        let paste_started = Instant::now();
        app.handle_paste(&raw_paste);
        app.handle_event(paste_delta);
        super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap();
        let paste_sample = paste_started.elapsed();

        let key_p95 = responsiveness_p95(&mut key_samples);
        let scroll_p95 = responsiveness_p95(&mut scroll_samples);
        println!(
            "responsiveness benchmark: target 8.3ms; tolerance 16.7ms; paste tolerance 50ms; key p95 {key_p95:?}; scroll p95 {scroll_p95:?}; paste {paste_sample:?}"
        );
        assert_eq!(app.prompt.text().len(), 65_536);
        assert!(key_p95 <= key_scroll_tolerance);
        assert!(scroll_p95 <= key_scroll_tolerance);
        assert!(paste_sample <= paste_tolerance);
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

    #[test]
    fn history_render_reuses_settled_rasters_across_prompt_scroll_and_live_updates() {
        let mut app = app_with_history(App::default());
        rendered_buffer(&mut app, 40, 8);
        let initial = app.transcript.construction_counts();

        type_prompt(&mut app, "x");
        rendered_buffer(&mut app, 40, 8);
        assert_eq!(app.transcript.construction_counts(), initial);

        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        rendered_buffer(&mut app, 40, 8);
        assert_eq!(app.transcript.construction_counts(), initial);

        assert!(app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(9) }));
        assert!(app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(9),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "live update".to_owned(),
            },
        }));
        rendered_buffer(&mut app, 40, 8);
        assert_eq!(app.transcript.construction_counts(), initial);
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

    struct RecordingTitles {
        titles: Vec<String>,
        fail: bool,
    }

    impl RecordingTitles {
        fn available() -> Self {
            Self {
                titles: Vec::new(),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                titles: Vec::new(),
                fail: true,
            }
        }
    }

    impl TitlePublisher for RecordingTitles {
        fn publish(&mut self, title: &str) -> io::Result<()> {
            self.titles.push(title.to_owned());
            if self.fail {
                Err(io::Error::other("title unavailable"))
            } else {
                Ok(())
            }
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
    fn title_publisher_runs_at_startup_and_on_name_changes_only() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("worktrees/interactive/settings/kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        let mut input = FakeInput::new([
            Some(Event::Paste("/name Refactor auth".to_owned())),
            Some(Event::Key(key(KeyCode::Enter))),
            Some(quit_event()),
        ]);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("terminal");
        let (commands, _command_receiver) = mpsc::channel(8);
        let (_event_sender, mut events) = mpsc::unbounded_channel();
        let (_progress_sender, mut progress) = mpsc::channel(8);
        let mut clipboard = FakeClipboard::available();
        let mut titles = RecordingTitles::available();

        super::run_loop_with_titles(
            &mut terminal,
            &mut app,
            &mut input,
            &commands,
            &mut events,
            &mut progress,
            &mut clipboard,
            &mut titles,
            &Trace::disabled(),
        )
        .expect("loop");

        assert_eq!(titles.titles, ["✷ - kiki", "✷ - Refactor auth"]);
    }

    #[test]
    fn title_failure_is_nonfatal_and_terminal_cleanup_still_runs() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        let mut input = FakeInput::new([Some(quit_event())]);
        let (commands, _command_receiver) = mpsc::channel(8);
        let (_event_sender, mut events) = mpsc::unbounded_channel();
        let (_progress_sender, mut progress) = mpsc::channel(8);
        let mut clipboard = FakeClipboard::available();
        let mut titles = RecordingTitles::failing();
        let restored = Cell::new(false);

        let result = super::with_terminal(
            || {
                Terminal::new(TestBackend::new(80, 8))
                    .map_err(|error| super::TuiError::Terminal(error.to_string()))
            },
            || restored.set(true),
            |terminal| {
                super::run_loop_with_titles(
                    terminal,
                    &mut app,
                    &mut input,
                    &commands,
                    &mut events,
                    &mut progress,
                    &mut clipboard,
                    &mut titles,
                    &Trace::disabled(),
                )
                .map(|_| ())
            },
        );

        assert!(result.is_ok());
        assert!(restored.get());
        assert_eq!(titles.titles, ["✷ - kiki"]);
    }

    #[test]
    fn session_name_uses_exact_title_and_front_truncated_footer_identity() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("worktrees/interactive/settings/kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));

        assert_eq!(app.title(), "✷ - kiki");

        assert!(app.handle_paste("/name  Refactor\t auth").visible_changed);
        let update = app.handle_key_at(key(KeyCode::Enter), Instant::now());

        assert!(update.commands.is_empty());
        assert!(app.prompt.is_empty());
        assert_eq!(app.title(), "✷ - Refactor auth");
        assert!(rendered(&mut app, 80, 4).contains("~/…/kiki (Refactor auth)"));
    }

    #[test]
    fn session_name_is_ephemeral_and_title_failure_is_visible() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        let update = app.name_session("  Refactor\t auth  ");
        assert!(update.commands.is_empty());
        assert_eq!(app.title(), "✷ - Refactor auth");

        let mut titles = RecordingTitles::failing();
        assert!(super::publish_title(&mut app, &mut titles));
        assert_eq!(app.title(), "✷ - Refactor auth");
        assert_eq!(app.notice(), Some("Could not change terminal title."));
        assert_eq!(titles.titles, ["✷ - Refactor auth"]);
        assert!(rendered(&mut app, 80, 4).contains("~/kiki (Refactor auth)"));

        assert!(app.transcript.begin_submission());
        assert!(app.handle_paste("/name Active name").visible_changed);
        let update = app.handle_key_at(key(KeyCode::Enter), Instant::now());
        assert!(update.commands.is_empty());
        assert!(app.prompt.is_empty());
        assert_eq!(app.title(), "✷ - Active name");
    }

    #[test]
    fn session_name_validates_empty_and_display_width() {
        let mut app = App::default();
        assert!(app.handle_paste("/name").visible_changed);
        let update = app.handle_key_at(key(KeyCode::Enter), Instant::now());
        assert!(update.commands.is_empty());
        assert_eq!(app.notice(), Some("Usage: /name <name>"));

        let update = app.name_session(" \t ");
        assert!(update.commands.is_empty());
        assert_eq!(app.notice(), Some("Usage: /name <name>"));

        let accepted = "a".repeat(80);
        assert!(app.name_session(&accepted).visible_changed);
        assert_eq!(app.title(), format!("✷ - {accepted}"));
        let rejected = "b".repeat(81);
        assert!(app.name_session(&rejected).visible_changed);
        assert_eq!(app.title(), format!("✷ - {accepted}"));
        assert_eq!(
            app.notice(),
            Some("Session name must be at most 80 display cells.")
        );

        let wide = "界".repeat(40);
        assert!(app.name_session(&wide).visible_changed);
        assert_eq!(app.title(), format!("✷ - {wide}"));
        assert!(app.name_session(&"界".repeat(41)).visible_changed);
        assert_eq!(app.title(), format!("✷ - {wide}"));
    }

    #[test]
    fn titles_sanitise_cwd_and_fall_back_when_unavailable() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("kiki\nsettings");
        let app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        assert_eq!(app.title(), "✷ - kiki settings");
        assert_eq!(App::default().title(), "✷ - kiki");
        let configured = App::configuration(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        assert_eq!(configured.title(), "✷ - kiki settings");
    }

    #[test]
    fn prompt_rules_follow_active_and_preview_thinking_colours() {
        let levels = ["low", "medium", "high", "xhigh", "max"];
        assert_eq!(
            super::thinking_rule_colour(&levels, "low"),
            super::THINKING_RULE_COLOURS[0],
        );
        assert_eq!(
            super::thinking_rule_colour(&levels, "max"),
            super::THINKING_RULE_COLOURS[5],
        );
        assert_eq!(
            super::thinking_rule_colour(&["low", "medium", "max"], "medium"),
            super::interpolate_colour(
                super::THINKING_RULE_COLOURS[2],
                super::THINKING_RULE_COLOURS[3],
                1,
                2,
            )
        );

        let mut app = App::default();
        let now = Instant::now();
        assert!(app.cycle_thinking_preview(now).visible_changed);
        let buffer = rendered_buffer(&mut app, 80, 8);
        assert_eq!(
            buffer[(0, 7)].fg,
            super::thinking_rule_colour(SOL.thinking_levels, "high")
        );
    }

    #[test]
    fn slash_completion_completes_then_second_enter_executes() {
        for (draft, completion_key, completed) in [
            ("/mo", KeyCode::Enter, "/model"),
            ("/thi", KeyCode::Tab, "/thinking"),
        ] {
            let mut app = app_with_prompt(draft);

            let complete = app.handle_key(key(completion_key));

            assert!(complete.commands.is_empty());
            assert_eq!(app.prompt.text(), completed);
            assert!(app.picker.is_none());

            let execute = app.handle_key(key(KeyCode::Enter));

            assert!(execute.commands.is_empty());
            assert!(app.prompt.is_empty());
            assert!(app.picker.is_some());
        }
    }

    #[test]
    fn slash_completion_lists_catalogue_in_order_and_clamps_keyboard_selection() {
        let mut app = app_with_prompt("/");
        let initial = rendered_buffer(&mut app, 40, 8);
        let overlay = app.slash_overlay.expect("slash overlay");
        let (model_column, model_row) = cell_start(&initial, "/model");
        let (name_column, name_row) = cell_start(&initial, "/name");
        let (thinking_column, thinking_row) = cell_start(&initial, "/thinking");

        assert_eq!(model_column, overlay.x);
        assert!(model_row < name_row && name_row < thinking_row);
        assert!(
            initial[(model_column, model_row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        let name = rendered_buffer(&mut app, 40, 8);
        assert!(
            name[(name_column, name_row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        assert!(!app.handle_key(key(KeyCode::Down)).visible_changed);
        assert!(
            !app.handle_key(modified_key(KeyCode::Up, KeyModifiers::CONTROL))
                .visible_changed
        );
        let thinking = rendered_buffer(&mut app, 40, 8);
        assert!(
            thinking[(thinking_column, thinking_row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );
        assert_eq!(app.prompt.text(), "/");
    }

    #[test]
    fn slash_completion_keeps_picker_shift_tab_active_run_and_release_precedence() {
        let now = Instant::now();
        let mut shift_tab = App::configured(crate::config::ready_snapshot_for_test());
        set_prompt(&mut shift_tab, "/");
        assert!(
            shift_tab
                .handle_key_at(shifted_key(KeyCode::Tab), now)
                .visible_changed
        );
        assert_eq!(shift_tab.settings_selection.thinking(), "high");
        assert_eq!(shift_tab.prompt.text(), "/");

        let mut picker = App::default();
        picker.open_model_picker("");
        set_prompt(&mut picker, "/mo");
        assert!(!picker.handle_key(key(KeyCode::Tab)).visible_changed);
        assert_eq!(picker.prompt.text(), "/mo");
        assert!(picker.picker.is_some());
        let picker_screen = rendered(&mut picker, 40, 8);
        assert!(picker_screen.contains("Models"));
        assert!(!picker_screen.contains("Choose model"));

        let mut active = app_with_prompt("/mo");
        active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        assert!(active.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(active.prompt.text(), "/model");
        assert!(active.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert!(active.picker.is_some());

        let mut cancel = app_with_prompt("/mo");
        cancel.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        assert_eq!(
            cancel.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::Cancel]
        );
        assert_eq!(cancel.prompt.text(), "/mo");

        let mut release = app_with_prompt("/mo");
        release.set_notice("keep status".to_owned());
        for code in [KeyCode::Enter, KeyCode::Tab] {
            let update = release.handle_key(KeyEvent::new_with_kind(
                code,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ));
            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(release.prompt.text(), "/mo");
            assert_eq!(release.notice(), Some("keep status"));
        }
    }

    #[test]
    fn slash_completion_hides_for_interior_arguments_unknown_and_multiline_drafts() {
        let mut interior = app_with_prompt("/mo");
        assert!(interior.handle_key(key(KeyCode::Left)).visible_changed);
        rendered_buffer(&mut interior, 40, 8);
        assert!(interior.slash_overlay.is_none());

        for draft in ["/model terra", "/future", "/model\nterra"] {
            let mut app = app_with_prompt(draft);
            let screen = rendered(&mut app, 40, 8);
            assert!(app.slash_overlay.is_none(), "{draft}");
            assert!(!screen.contains("Choose model"), "{draft}");
        }
    }

    #[test]
    fn slash_exact_token_style_crosses_soft_wraps_and_leaves_ordinary_drafts_plain() {
        let mut wrapped = app_with_prompt("/model arg");
        let buffer = rendered_buffer(&mut wrapped, 5, 18);
        let (token_column, token_row) = cell_start(&buffer, "/mo");
        let (argument_column, argument_row) = cell_start(&buffer, " ar");

        for column in token_column..token_column + 3 {
            assert_eq!(buffer[(column, token_row)].fg, Color::Cyan);
            assert!(
                buffer[(column, token_row)]
                    .modifier
                    .contains(ratatui::style::Modifier::BOLD)
            );
            assert_eq!(buffer[(column, token_row + 1)].fg, Color::Cyan);
            assert!(
                buffer[(column, token_row + 1)]
                    .modifier
                    .contains(ratatui::style::Modifier::BOLD)
            );
        }
        assert_ne!(buffer[(argument_column + 1, argument_row)].fg, Color::Cyan);
        assert!(
            !buffer[(argument_column + 1, argument_row)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );

        assert!(wrapped.handle_key(key(KeyCode::Left)).visible_changed);
        let interior = rendered_buffer(&mut wrapped, 5, 18);
        assert_eq!(interior[(token_column, token_row)].fg, Color::Cyan);
        assert!(
            interior[(token_column, token_row)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );

        for (draft, token) in [("/future", "/future"), ("/model\narg", "/model")] {
            let mut app = app_with_prompt(draft);
            let buffer = rendered_buffer(&mut app, 40, 12);
            let (column, row) = cell_start(&buffer, token);
            assert_ne!(buffer[(column, row)].fg, Color::Cyan, "{draft}");
            assert!(
                !buffer[(column, row)]
                    .modifier
                    .contains(ratatui::style::Modifier::BOLD)
            );
        }
    }

    #[test]
    fn slash_overlay_preserves_history_geometry_and_clips_to_tiny_history() {
        let mut plain = app_with_history(App::default());
        let mut completed = app_with_history(app_with_prompt("/"));
        rendered_buffer(&mut plain, 40, 8);
        rendered_buffer(&mut completed, 40, 8);
        let overlay = completed.slash_overlay.expect("slash overlay");
        let viewport = completed.history_viewport.expect("history viewport");

        assert_eq!(completed.history_height, plain.history_height);
        assert_eq!(
            viewport.area,
            plain.history_viewport.expect("history viewport").area
        );
        assert_eq!(overlay.bottom(), viewport.area.bottom());
        assert_eq!(overlay.x, 2);

        let mut tiny = app_with_prompt("/");
        rendered_buffer(&mut tiny, 5, 5);
        assert_eq!(tiny.history_height, 1);
        assert_eq!(tiny.slash_overlay.expect("tiny overlay").height, 1);
        assert!(tiny.handle_key(key(KeyCode::Down)).visible_changed);
        assert!(tiny.handle_key(key(KeyCode::Down)).visible_changed);
        let buffer = rendered_buffer(&mut tiny, 5, 5);
        let (column, row) = cell_start(&buffer, "/thin");
        assert!(
            buffer[(column, row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        );

        rendered_buffer(&mut tiny, 5, 4);
        assert!(tiny.slash_overlay.is_none());
    }

    #[test]
    fn slash_overlay_stays_above_prompt_over_visible_subagent_monitor() {
        let now = Instant::now();
        for (height, last_suggestion) in [(12, "/thinking"), (6, "/name")] {
            let mut plain = app_with_history(App::default());
            plain
                .subagent_monitor
                .insert_for_test("child1", TaskState::Completed, now);
            plain
                .subagent_monitor
                .insert_for_test("child2", TaskState::Completed, now);
            let mut completed = app_with_history(app_with_prompt("/"));
            completed
                .subagent_monitor
                .insert_for_test("child1", TaskState::Completed, now);
            completed
                .subagent_monitor
                .insert_for_test("child2", TaskState::Completed, now);

            rendered_at(&mut plain, 80, height, now);
            let screen = rendered_at(&mut completed, 80, height, now);
            let rows = screen.lines().collect::<Vec<_>>();
            let prompt_row = u16::try_from(
                rows.iter()
                    .position(|row| row.starts_with("> "))
                    .expect("prompt"),
            )
            .unwrap();
            let prompt_top = prompt_row.saturating_sub(1);
            let overlay = completed.slash_overlay.expect("slash overlay");
            let last_suggestion_row = rows
                .iter()
                .position(|row| row.contains(last_suggestion))
                .and_then(|row| u16::try_from(row).ok());

            assert_eq!(completed.history_height, plain.history_height);
            assert_eq!(
                completed.history_viewport.map(|viewport| viewport.area),
                plain.history_viewport.map(|viewport| viewport.area),
            );
            if height == 12 {
                assert!(screen.contains("✓ subagent:child1"), "{screen}");
            }

            completed.set_notice("keep status".to_owned());
            let mut shielded = true;
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                let update = completed.handle_mouse(
                    mouse_event(kind, overlay.x, prompt_top.saturating_sub(1)),
                    now,
                );
                shielded &= !update.visible_changed
                    && completed.notice() == Some("keep status")
                    && completed.selection.is_none()
                    && completed.drag.is_none()
                    && !completed.pending_copy;
            }

            assert_eq!(
                (overlay.bottom(), last_suggestion_row, shielded),
                (prompt_top, Some(prompt_top.saturating_sub(1)), true),
                "height {height}",
            );
        }
    }

    #[test]
    fn slash_overlay_shields_primary_mouse_events_but_not_history_outside_it() {
        let mut app = app_with_history(app_with_prompt("/"));
        rendered_buffer(&mut app, 40, 12);
        let overlay = app.slash_overlay.expect("slash overlay");
        let viewport = app.history_viewport.expect("history viewport");
        let now = Instant::now();

        app.set_notice("keep status".to_owned());
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let update = app.handle_mouse(mouse_event(kind, overlay.x, overlay.y), now);
            assert!(!update.visible_changed);
            assert_eq!(app.notice(), Some("keep status"));
            assert!(app.selection.is_none());
            assert!(app.drag.is_none());
            assert!(!app.pending_copy);
        }

        let outside_column = viewport.area.x + 1;
        let outside_row = viewport.area.y;
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                outside_column,
                outside_row,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                outside_column + 1,
                outside_row,
            ),
            now,
        );
        assert!(app.selection.is_some());
        assert!(app.drag.as_ref().is_some_and(|drag| drag.edge.is_some()));
        app.set_notice("keep drag status".to_owned());

        let cancelled = app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                overlay.x,
                overlay.y,
            ),
            now,
        );
        assert!(cancelled.visible_changed);
        assert_eq!(app.notice(), Some("keep drag status"));
        assert!(app.selection.is_none());
        assert!(app.drag.is_none());
        assert!(!app.pending_copy);
        assert!(!app.tick(now + super::POLL_INTERVAL).visible_changed);

        app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                outside_column + 1,
                outside_row,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                outside_column + 1,
                outside_row,
            ),
            now,
        );
        assert!(app.selection.is_none());
        assert!(app.drag.is_none());
        assert!(!app.pending_copy);
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut clipboard = FakeClipboard::available();
        assert!(!super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap());
        assert!(clipboard.writes.is_empty());

        app.handle_mouse(
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                outside_column,
                outside_row,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Drag(MouseButton::Left),
                outside_column + 1,
                outside_row,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                outside_column + 1,
                outside_row,
            ),
            now,
        );
        let selection = app.selection;
        assert!(selection.is_some());
        assert!(app.drag.is_none());
        assert!(app.pending_copy);
        app.set_notice("keep completed status".to_owned());

        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            let update = app.handle_mouse(mouse_event(kind, overlay.x, overlay.y), now);
            assert!(!update.visible_changed);
            assert_eq!(app.notice(), Some("keep completed status"));
            assert_eq!(app.selection, selection);
            assert!(app.drag.is_none());
            assert!(app.pending_copy);
        }
    }

    #[test]
    fn model_argument_opens_a_filtered_picker_without_submitting() {
        let mut app = App::default();
        type_prompt(&mut app, "/model terra");

        let update = app.handle_key(key(KeyCode::Enter));

        assert!(update.commands.is_empty());
        assert!(app.prompt.is_empty());
        let screen = rendered(&mut app, 80, 15);
        assert!(screen.contains("Models"));
        assert!(screen.contains("openai/gpt-5.6-terra"));
        assert!(!screen.contains("openai/gpt-5.6-sol"));
    }

    #[test]
    fn astra_is_first_and_uses_its_model_owned_thinking_levels() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        assert!(std::ptr::eq(app.settings_selection.model(), &SOL));

        let open = enter_at(&mut app, "/model", now);
        assert!(open.commands.is_empty());
        let screen = rendered(&mut app, 80, 15);
        assert!(screen.find(ASTRA.id).unwrap() < screen.find(SOL.id).unwrap());

        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let update = enter_at(&mut app, "/model OPENAI/GPT-6-ASTRA", now);
        let (selection, _) = applied_settings(&update);
        assert!(std::ptr::eq(selection.model(), &ASTRA));
        assert_eq!(selection.thinking(), "low");

        let open = enter_at(&mut app, "/thinking", now);
        assert!(open.commands.is_empty());
        let screen = rendered(&mut app, 80, 15);
        for level in ASTRA.thinking_levels {
            assert!(screen.contains(level));
        }
        assert!(!screen.contains("none"));
    }

    #[test]
    fn direct_settings_commands_apply_exact_values_and_consume_the_draft() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let update = enter_at(&mut app, "/model OPENAI/GPT-5.6-TERRA", now);
        assert!(update.visible_changed);
        let (selection, revision) = applied_settings(&update);
        assert!(std::ptr::eq(selection.model(), &TERRA));
        assert_eq!(selection.thinking(), "medium");
        assert_eq!(revision, 1);
        assert!(app.prompt.is_empty());

        let update = enter_at(&mut app, "/thinking HIGH", now);
        assert!(update.visible_changed);
        let (selection, revision) = applied_settings(&update);
        assert!(std::ptr::eq(selection.model(), &TERRA));
        assert_eq!(selection.thinking(), "high");
        assert_eq!(revision, 2);
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn thinking_picker_contains_only_the_active_models_ordered_levels() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let _ = enter_at(&mut app, "/model openai/gpt-5.6-terra", now);

        let open = enter_at(&mut app, "/thinking", now);

        assert!(open.commands.is_empty());
        let screen = rendered(&mut app, 80, 15);
        assert!(screen.contains("Thinking"));
        for level in TERRA.thinking_levels {
            assert!(screen.contains(level));
        }
    }

    #[test]
    fn unsupported_thinking_is_consumed_without_a_runtime_command() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let update = enter_at(&mut app, "/thinking impossible", now);

        assert!(update.commands.is_empty());
        assert!(app.prompt.is_empty());
        let notice = app.notice().expect("unsupported level status");
        assert!(notice.contains(SOL.id));
        assert!(notice.contains("low, medium, high, xhigh, max"));
    }

    #[test]
    fn unrecognised_or_multiline_slash_input_remains_an_ordinary_prompt() {
        let now = Instant::now();
        let mut app = App::default();

        let update = enter_at(&mut app, "/future task", now);
        assert_eq!(
            update.commands,
            [RuntimeCommand::Submit {
                request: "/future task".to_owned()
            }]
        );

        let mut multiline = App::default();
        let update = enter_at(&mut multiline, "/model\nterra", now);
        assert_eq!(
            update.commands,
            [RuntimeCommand::Submit {
                request: "/model\nterra".to_owned()
            }]
        );
    }

    #[test]
    fn settings_commands_are_available_during_active_runs_and_picker_escape_wins() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });

        let (selection, _) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        assert!(std::ptr::eq(selection.model(), &TERRA));
        let (selection, _) = applied_settings(&enter_at(&mut app, "/thinking high", now));
        assert_eq!(selection.thinking(), "high");

        let open = enter_at(&mut app, "/model", now);
        assert!(open.commands.is_empty());
        assert!(app.picker.is_some());
        let escape = app.handle_key_at(key(KeyCode::Esc), now);
        assert!(escape.commands.is_empty());
        assert!(app.picker.is_none());
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            [RuntimeCommand::Cancel]
        );
    }

    #[test]
    fn model_switch_restores_accepted_thinking_and_cancels_previews() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let (_, revision) = applied_settings(&enter_at(&mut app, "/thinking low", now));
        app.handle_event(RuntimeEvent::SettingsAccepted { revision });

        let terra = enter_at(&mut app, "/model openai/gpt-5.6-terra", now);
        let (selection, revision) = applied_settings(&terra);
        assert_eq!(selection.thinking(), TERRA.default_thinking);
        app.handle_event(RuntimeEvent::SettingsAccepted { revision });

        let preview = app.handle_key_at(key(KeyCode::BackTab), now);
        assert!(preview.commands.is_empty());
        assert!(app.preview.is_some());
        assert_eq!(app.session_thinking.get(TERRA.id), Some(&"medium"));
        let sol = enter_at(&mut app, "/model openai/gpt-5.6-sol", now);
        let (selection, _) = applied_settings(&sol);
        assert!(std::ptr::eq(selection.model(), &SOL));
        assert_eq!(selection.thinking(), "low");
        assert!(app.preview.is_none());
    }

    #[test]
    fn model_switch_restores_sent_thinking_before_acknowledgements() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let (selection, _) = applied_settings(&enter_at(&mut app, "/thinking high", now));
        assert!(std::ptr::eq(selection.model(), &SOL));
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"high"));

        let (selection, _) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        assert!(std::ptr::eq(selection.model(), &TERRA));

        let (selection, _) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-sol", now));
        assert!(std::ptr::eq(selection.model(), &SOL));
        assert_eq!(selection.thinking(), "high");
    }

    #[test]
    fn uncommitted_thinking_preview_does_not_reach_runtime_or_session_memory() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let backtab = app.handle_key_at(key(KeyCode::BackTab), now);
        assert!(backtab.commands.is_empty());
        assert_eq!(app.settings_selection.thinking(), "high");
        let tab = app.handle_key_at(shifted_key(KeyCode::Tab), now + Duration::from_secs(1));
        assert!(tab.commands.is_empty());
        assert_eq!(app.settings_selection.thinking(), "xhigh");
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"medium"));

        assert!(
            app.tick(now + Duration::from_millis(2_999))
                .commands
                .is_empty()
        );
        let (selection, revision) = applied_settings(&app.tick(now + Duration::from_secs(3)));
        assert_eq!(selection.thinking(), "xhigh");
        assert_eq!(revision, 1);
        assert!(app.tick(now + Duration::from_secs(4)).commands.is_empty());
    }

    #[test]
    fn shift_tab_wraps_through_the_active_models_levels() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        for expected in ["high", "xhigh", "max", "low", "medium"] {
            let update = app.handle_key_at(key(KeyCode::BackTab), now);
            assert!(update.commands.is_empty());
            assert_eq!(app.settings_selection.thinking(), expected);
        }
    }

    #[test]
    fn telemetry_brackets_tui_cancellation_command_queueing() {
        let directory = tempdir().expect("trace directory");
        let trace_file = directory.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let (sender, mut receiver) = mpsc::channel(3);
        let update = super::Update::with_commands([
            RuntimeCommand::Cancel,
            RuntimeCommand::CancelBackground,
            RuntimeCommand::Shutdown,
        ]);

        super::dispatch_commands(&sender, &update, &trace_owner.handle())
            .expect("dispatch cancellation commands");
        assert!(matches!(receiver.try_recv(), Ok(RuntimeCommand::Cancel)));
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::CancelBackground)
        ));
        assert!(matches!(receiver.try_recv(), Ok(RuntimeCommand::Shutdown)));
        trace_owner.shutdown().expect("trace shutdown");

        let events = fs::read_to_string(trace_file)
            .expect("trace file")
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("trace event"))
            .filter_map(|event| {
                let name = event["event"].as_str()?;
                match name {
                    "tui_command_dispatch" | "tui_command_queued" => {
                        Some((name.to_owned(), event["command"].as_str()?.to_owned()))
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            [
                ("tui_command_dispatch".to_owned(), "cancel".to_owned()),
                ("tui_command_queued".to_owned(), "cancel".to_owned()),
                (
                    "tui_command_dispatch".to_owned(),
                    "cancel_background".to_owned(),
                ),
                (
                    "tui_command_queued".to_owned(),
                    "cancel_background".to_owned(),
                ),
                ("tui_command_dispatch".to_owned(), "shutdown".to_owned()),
                ("tui_command_queued".to_owned(), "shutdown".to_owned()),
            ]
        );
    }

    #[test]
    fn timer_updates_are_dispatched_without_a_key_event() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_key_at(key(KeyCode::BackTab), now);
        let update = app.tick(now + Duration::from_secs(2));
        let (sender, mut receiver) = mpsc::channel(1);

        super::dispatch_commands(&sender, &update, &Trace::disabled())
            .expect("dispatch timer update");

        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::ApplySettings { selection, revision: 1 }) if selection.thinking() == "high"
        ));
    }

    #[test]
    fn thinking_preview_waits_two_seconds_and_explicit_selection_replaces_it() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_key_at(key(KeyCode::BackTab), now);
        assert_eq!(app.settings_selection.thinking(), "high");
        assert!(
            app.tick(now + Duration::from_millis(1_999))
                .commands
                .is_empty()
        );
        let (selection, revision) = applied_settings(&app.tick(now + Duration::from_secs(2)));
        assert_eq!(selection.thinking(), "high");
        assert_eq!(revision, 1);
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"high"));

        app.handle_key_at(key(KeyCode::BackTab), now + Duration::from_secs(3));
        let (selection, revision) = applied_settings(&enter_at(
            &mut app,
            "/thinking low",
            now + Duration::from_secs(3),
        ));
        assert_eq!(selection.thinking(), "low");
        assert_eq!(revision, 2);
        assert!(app.preview.is_none());
    }

    #[test]
    fn stale_acknowledgements_do_not_overwrite_locally_retained_session_thinking() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let (_, stale) = applied_settings(&enter_at(&mut app, "/thinking high", now));
        let (_, latest) = applied_settings(&enter_at(&mut app, "/thinking low", now));

        assert_eq!(app.session_thinking.get(SOL.id), Some(&"low"));
        assert!(!app.handle_event(RuntimeEvent::SettingsAccepted { revision: stale }));
        assert_eq!(app.settings_selection.thinking(), "low");
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"low"));
        assert!(!app.handle_event(RuntimeEvent::SettingsAccepted { revision: latest }));
        assert!(app.pending_settings_revision.is_none());
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"low"));
    }

    #[test]
    fn acknowledgement_retains_pending_settings_for_a_newer_unsent_preview() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_key_at(key(KeyCode::BackTab), now);
        let (_, revision) = applied_settings(&app.tick(now + Duration::from_secs(2)));
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"high"));
        app.handle_key_at(key(KeyCode::BackTab), now + Duration::from_secs(2));

        assert!(!app.handle_event(RuntimeEvent::SettingsAccepted { revision }));

        assert_eq!(app.settings_selection.thinking(), "xhigh");
        assert!(app.preview.is_some());
        assert_eq!(app.pending_settings_revision, Some(revision));
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"high"));

        let (_, preview_revision) = applied_settings(&app.tick(now + Duration::from_secs(4)));
        assert!(!app.handle_event(RuntimeEvent::SettingsAccepted {
            revision: preview_revision,
        }));

        assert!(app.pending_settings_revision.is_none());
        assert!(app.preview.is_none());
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"xhigh"));
    }

    #[test]
    fn shift_tab_accepts_only_terminal_shift_encodings() {
        let now = Instant::now();

        for key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            let mut app = App::configured(crate::config::ready_snapshot_for_test());
            let update = app.handle_key_at(key, now);
            assert!(update.commands.is_empty());
            assert_eq!(app.settings_selection.thinking(), "high");
        }

        for key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SUPER),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::CONTROL | KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::ALT | KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SUPER | KeyModifiers::SHIFT),
        ] {
            let mut app = App::configured(crate::config::ready_snapshot_for_test());
            let update = app.handle_key_at(key, now);
            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(app.settings_selection.thinking(), "medium");
        }
    }

    #[test]
    fn shift_tab_release_events_are_noops() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.set_notice("keep status".to_owned());
        for key in [
            KeyEvent::new_with_kind(KeyCode::BackTab, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new_with_kind(KeyCode::Tab, KeyModifiers::SHIFT, KeyEventKind::Release),
        ] {
            let update = app.handle_key_at(key, now);
            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(app.settings_selection.thinking(), "medium");
            assert_eq!(app.notice(), Some("keep status"));
        }
    }

    #[test]
    fn prompt_submission_flushes_a_pending_preview_before_submit_and_transport_preserves_order() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_key_at(key(KeyCode::BackTab), now);

        let update = enter_at(&mut app, "use the visible setting", now);
        let [
            RuntimeCommand::ApplySettings {
                selection,
                revision,
            },
            RuntimeCommand::Submit { request },
        ] = update.commands.as_slice()
        else {
            panic!("expected settings then submit, got {:?}", update.commands);
        };
        assert_eq!(selection.thinking(), "high");
        assert_eq!(*revision, 1);
        assert_eq!(request, "use the visible setting");
        assert!(app.preview.is_none());
        assert_eq!(app.session_thinking.get(SOL.id), Some(&"high"));

        let (sender, mut receiver) = mpsc::channel(2);
        super::dispatch_commands(&sender, &update, &Trace::disabled()).expect("dispatch commands");
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::ApplySettings { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::Submit { request }) if request == "use the visible setting"
        ));
    }

    #[test]
    fn picker_escape_hides_the_prompt_cursor_and_tiny_picker_rendering_is_safe() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let _ = enter_at(&mut app, "/model", now);
        assert!(!cursor(&mut app, 80, 15).0);
        for (width, height) in [(1, 1), (1, 2), (2, 3), (3, 3)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
        }
    }

    #[test]
    fn ctrl_s_applies_and_persists_user_defaults_with_masking_status() {
        let home = tempdir().expect("home");
        let workspace = tempdir().expect("workspace");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ));
        let now = Instant::now();
        let _ = enter_at(&mut app, "/model", now);
        app.handle_key_at(key(KeyCode::Down), now);
        let (selection, _) = applied_settings(&app.handle_key_at(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            now,
        ));
        assert!(std::ptr::eq(selection.model(), &TERRA));
        assert!(
            app.notice()
                .is_some_and(|notice| notice.contains("User default saved"))
        );
        assert!(
            fs::read_to_string(home.path().join(".kiki/config.toml"))
                .expect("model setting")
                .contains("model = \"openai/gpt-5.6-terra\"")
        );

        let _ = enter_at(&mut app, "/thinking", now);
        app.handle_key_at(key(KeyCode::Down), now);
        let (_, _) = applied_settings(&app.handle_key_at(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            now,
        ));
        assert!(
            fs::read_to_string(home.path().join(".kiki/config.toml"))
                .expect("thinking setting")
                .contains("\"openai/gpt-5.6-terra\" = \"high\"")
        );

        fs::create_dir_all(workspace.path().join(".kiki")).expect("project settings directory");
        fs::write(
            workspace.path().join(".kiki/config.toml"),
            "model = \"openai/gpt-5.6-terra\"\n",
        )
        .expect("project settings");
        let mut masked = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ));
        let _ = enter_at(&mut masked, "/model", now);
        masked.handle_key_at(key(KeyCode::Home), now);
        let _ = masked.handle_key_at(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            now,
        );
        assert!(
            masked
                .notice()
                .is_some_and(|notice| notice.contains("overridden by project settings"))
        );
    }

    #[test]
    fn picker_enter_applies_its_filtered_selection_and_escape_discards_it() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let open = enter_at(&mut app, "/model terra", now);
        assert!(open.commands.is_empty());

        let update = app.handle_key_at(key(KeyCode::Enter), now);
        assert!(update.visible_changed);
        let (selection, _) = applied_settings(&update);
        assert!(std::ptr::eq(selection.model(), &TERRA));
        assert!(app.picker.is_none());
        assert!(app.prompt.is_empty());

        let _ = enter_at(&mut app, "/thinking", now);
        let selection = app.settings_selection;
        let close = app.handle_key_at(key(KeyCode::Esc), now);
        assert!(close.commands.is_empty());
        assert!(app.picker.is_none());
        assert!(app.prompt.is_empty());
        assert_eq!(app.settings_selection, selection);
    }

    #[test]
    fn no_match_picker_has_no_status_until_apply_is_requested() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let _ = enter_at(&mut app, "/model unknown", now);

        assert!(app.notice().is_none());
        assert!(app.picker.is_some());
        let update = app.handle_key_at(key(KeyCode::Enter), now);
        assert!(update.commands.is_empty());
        assert_eq!(app.notice(), Some("No matching models"));
    }

    #[test]
    fn persisted_target_thinking_is_restored_before_the_catalogue_default() {
        let home = tempdir().expect("home");
        let workspace = tempdir().expect("workspace");
        fs::create_dir_all(home.path().join(".kiki")).expect("settings directory");
        fs::write(
            home.path().join(".kiki/config.toml"),
            "[thinking]\n\"openai/gpt-5.6-terra\" = \"max\"\n",
        )
        .expect("settings");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ));

        let (selection, _) = applied_settings(&enter_at(
            &mut app,
            "/model openai/gpt-5.6-terra",
            Instant::now(),
        ));

        assert_eq!(selection.thinking(), "max");
    }

    #[test]
    fn settings_state_survives_runtime_settlement_without_prompt_history_entries() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let _ = enter_at(&mut app, "/model openai/gpt-5.6-terra", now);
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) });

        assert!(std::ptr::eq(app.settings_selection.model(), &TERRA));
        assert!(!rendered(&mut app, 80, 15).contains("/model openai/gpt-5.6-terra"));

        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            [RuntimeCommand::Cancel]
        );
        assert!(std::ptr::eq(app.settings_selection.model(), &TERRA));
    }

    #[test]
    fn active_unrecognised_slash_draft_stays_editable_without_submission() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        set_prompt(&mut app, "/future later");

        let update = app.handle_key_at(key(KeyCode::Enter), now);

        assert!(update.commands.is_empty());
        assert!(update.visible_changed);
        assert_eq!(app.prompt.text(), "/future later");
    }

    #[test]
    fn idle_enter_submits_the_prompt_once() {
        let mut app = App::default();
        app.handle_key(key(KeyCode::Char('h')));
        app.handle_key(key(KeyCode::Char('i')));

        let update = app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            update.commands,
            [RuntimeCommand::Submit {
                request: "hi".to_owned()
            }]
        );
        assert!(update.visible_changed);
        assert_eq!(app.prompt.text(), "");
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
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

            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(app.prompt.text(), "keep");
        }

        let mut app = app_with_prompt("send");
        let update = app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            update.commands,
            [RuntimeCommand::Submit {
                request: "send".to_owned(),
            }]
        );
        assert!(update.visible_changed);
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn shift_enter_edits_a_multiline_prompt_without_submitting() {
        let mut app = App::default();
        app.handle_key(key(KeyCode::Char('f')));
        assert!(
            app.handle_key(shifted_key(KeyCode::Enter))
                .commands
                .is_empty()
        );
        app.handle_key(key(KeyCode::Char('g')));
        assert_eq!(app.prompt.text(), "f\ng");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Submit {
                request: "f\ng".to_owned(),
            }]
        );

        for modifiers in [
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
            KeyModifiers::SHIFT | KeyModifiers::ALT,
            KeyModifiers::SHIFT | KeyModifiers::SUPER,
            KeyModifiers::SHIFT | KeyModifiers::META,
        ] {
            let mut app = app_with_prompt("keep");

            let update = app.handle_key(modified_key(KeyCode::Enter, modifiers));

            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(app.prompt.text(), "keep");
        }

        let mut active = App::default();
        active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        assert!(
            active
                .handle_key(shifted_key(KeyCode::Enter))
                .visible_changed
        );
        assert_eq!(active.prompt.text(), "\n");
        assert!(active.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(active.prompt.text(), "\nx");
        assert!(active.handle_key(key(KeyCode::Enter)).commands.is_empty());

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
            modified_key(KeyCode::Char('j'), KeyModifiers::CONTROL),
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
            modified_key(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            assert!(!configuration.handle_key(event).visible_changed);
        }
        assert_eq!(configuration.prompt.text(), "locked");

        let mut bounded = App::default();
        set_prompt(&mut bounded, &"a".repeat(MAX_PROMPT_BYTES - 1));
        assert!(
            bounded
                .handle_key(shifted_key(KeyCode::Enter))
                .visible_changed
        );
        assert_eq!(bounded.prompt.text().len(), MAX_PROMPT_BYTES);
        let limit_reached = bounded.handle_key(shifted_key(KeyCode::Enter));
        assert!(limit_reached.commands.is_empty());
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

        assert!(update.commands.is_empty());
        assert!(!update.quit);
        assert!(update.visible_changed);
        assert_eq!(app.prompt.text(), "α\nβ\n    \\x1B\nδ\\x9Dγ");

        let mut bounded = app_with_prompt(&"a".repeat(MAX_PROMPT_BYTES - 3));
        let update = bounded.handle_paste("\t");
        assert!(update.commands.is_empty());
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
        assert!(idle_update.commands.is_empty());
        assert!(!idle_update.quit);
        assert_eq!(idle.prompt.text(), "idle draft");

        let mut active = App::default();
        assert!(active.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) }));
        let active_update = active.handle_paste("active draft");
        assert!(active_update.visible_changed);
        assert!(active_update.commands.is_empty());
        assert!(!active_update.quit);
        assert_eq!(active.prompt.text(), "active draft");
    }

    #[test]
    fn empty_or_uninsertable_paste_is_a_visible_noop_without_notice() {
        let mut empty = app_with_prompt("keep");
        let empty_update = empty.handle_paste("");
        assert!(empty_update.commands.is_empty());
        assert!(!empty_update.visible_changed);
        assert_eq!(empty.prompt.text(), "keep");

        let mut full = app_with_prompt(&"a".repeat(MAX_PROMPT_BYTES));
        let full_update = full.handle_paste("x");
        assert!(full_update.commands.is_empty());
        assert!(!full_update.visible_changed);
        assert_eq!(full.prompt.text().len(), MAX_PROMPT_BYTES);
    }

    #[test]
    fn paste_is_read_only_in_configuration_and_disabled_views() {
        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, "locked");
        let configuration_update = configuration.handle_paste("discarded");
        assert!(configuration_update.commands.is_empty());
        assert!(!configuration_update.visible_changed);
        assert_eq!(configuration.prompt.text(), "locked");

        let mut disabled = App::default();
        set_prompt(&mut disabled, "locked");
        disabled.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::OpenAiAuthentication,
        });
        let disabled_update = disabled.handle_paste("discarded");
        assert!(disabled_update.commands.is_empty());
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

        let mut notice = App {
            notice: Some(super::Notice::Copied),
            ..App::default()
        };
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
            let mut app = App::default();
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

            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(app.prompt.text(), "kept");
        }

        let mut app = app_with_prompt("kept");
        let update = app.handle_key(modified_key(KeyCode::Char('å'), KeyModifiers::ALT));

        assert!(update.commands.is_empty());
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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));
        assert!(
            app.handle_key(modified_key(KeyCode::Home, KeyModifiers::CONTROL))
                .visible_changed
        );
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
        assert!(
            app.handle_key(modified_key(KeyCode::End, KeyModifiers::CONTROL))
                .visible_changed
        );
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
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
        assert_eq!(
            app.history_position,
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(1))
        );
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));
        assert!(app.handle_key(key(KeyCode::PageDown)).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
        assert!(app.handle_key(key(KeyCode::End)).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
    }

    #[test]
    fn control_j_edits_a_multiline_prompt_without_submitting() {
        for code in ['j', 'J'] {
            let mut app = App::default();
            type_prompt(&mut app, "f");

            let update = app.handle_key(modified_key(KeyCode::Char(code), KeyModifiers::CONTROL));

            assert!(update.commands.is_empty());
            assert!(update.visible_changed);
            type_prompt(&mut app, "g");
            assert_eq!(submitted_prompt(&mut app), "f\ng");
        }

        for modifiers in [
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
            KeyModifiers::CONTROL | KeyModifiers::SUPER,
            KeyModifiers::CONTROL | KeyModifiers::META,
        ] {
            let mut app = app_with_prompt("keep");

            let update = app.handle_key(modified_key(KeyCode::Char('j'), modifiers));

            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert_eq!(app.prompt.text(), "keep");
        }
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
            app.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Submit {
                request: "x".to_owned()
            }]
        );
    }

    #[test]
    fn shift_enter_and_control_j_are_advertised_in_configured_header() {
        let mut app = App::default();

        let header = rendered(&mut app, 200, 4);

        assert!(header.starts_with("Status: idle | Enter submit"));
        assert!(header.contains("Shift-Enter/Ctrl-J newline"));
        assert!(header.contains("Arrows edit | PgUp/PgDn scroll"));
        assert!(header.contains("Ctrl-C clear/quit"));
        assert!(!header.contains("Up/Down scroll"));
    }

    #[test]
    fn header_uses_thinking_terminology() {
        let mut app = App::default();

        let header = rendered(&mut app, 200, 4);

        assert!(header.contains("Ctrl-T thinking"));
        assert!(!header.contains("reasoning"));
        assert!(!include_str!("../README.md").contains("reasoning"));
    }

    #[test]
    fn configuration_controls_are_read_only_and_ctrl_c_needs_no_command() {
        let mut app = App::configuration(crate::config::ready_snapshot_for_test());
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
            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert!(!update.quit);
            assert_eq!(app.prompt.text(), prompt);
        }

        let quit = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(quit.commands.is_empty());
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
        let mut app = App::configuration(crate::config::ready_snapshot_for_test());
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
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));
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
        let mut app = App::configuration(crate::config::ready_snapshot_for_test());
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
        let result = {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_keyboard_enhancement(
                || {
                    calls.borrow_mut().push("push");
                    Ok(())
                },
                || calls.borrow_mut().push("pop"),
                || Err::<(), _>(super::TuiError::Terminal("loop failed".to_owned())),
            )
        };
        assert!(result.is_err());
        assert_eq!(calls.into_inner(), ["push", "pop", "restore"]);

        let calls = RefCell::new(Vec::new());
        {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_keyboard_enhancement(
                || Err(io::Error::other("push failed")),
                || calls.borrow_mut().push("pop"),
                || calls.borrow_mut().push("run"),
            );
        }
        assert_eq!(calls.into_inner(), ["run", "restore"]);

        let calls = RefCell::new(Vec::new());
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _terminal_restore = super::RestoreGuard::new(|| calls.borrow_mut().push("restore"));
            super::with_keyboard_enhancement(
                || {
                    calls.borrow_mut().push("push");
                    Ok(())
                },
                || calls.borrow_mut().push("pop"),
                || panic!("loop panic"),
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
                                    Ok(())
                                },
                                || calls.borrow_mut().push("keyboard disable"),
                                || {
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
                                    Ok(())
                                },
                                || calls.borrow_mut().push("keyboard disable"),
                                || {
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
                                    Ok(())
                                },
                                || calls.borrow_mut().push("keyboard disable"),
                                || -> Result<(), super::TuiError> {
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

        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::Cancel]
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
            assert!(clear.commands.is_empty());
            assert!(clear.visible_changed);
            assert!(!clear.quit);
            assert!(app.prompt.is_empty());

            let shutdown = app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(shutdown.commands, [RuntimeCommand::Shutdown]);
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
                .is_some_and(|header| header.contains("Shift-Enter/Ctrl-J newline"))
        );

        set_prompt(&mut app, "saved prompt");
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(app.prompt.text(), "saved prompt");
        let shutdown = app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(shutdown.commands, [RuntimeCommand::Shutdown]);
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
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));

        app.handle_key(history_home());
        let oldest = rendered(&mut app, 40, 7);
        assert!(oldest.contains("request 1"));
        assert!(!oldest.contains("request 8"));

        app.handle_key(history_end());
        assert!(rendered(&mut app, 40, 7).contains("request 8"));
    }

    #[test]
    fn transcript_anchored_append_preserves_numeric_top_and_text() {
        let mut app = app_with_history(App::default());
        rendered(&mut app, 40, 7);
        assert!(app.handle_key(history_home()).visible_changed);
        let oldest = rendered(&mut app, 40, 7);
        let viewport = app.history_viewport.expect("history viewport");
        let request_row = app.history_document_top().saturating_add(
            oldest
                .lines()
                .position(|row| row.contains("request 1"))
                .expect("oldest request")
                .saturating_sub(usize::from(viewport.area.y)),
        );
        assert!(app.scroll_newer(request_row).visible_changed);
        let anchored = rendered(&mut app, 40, 7);
        let top = app.history_document_top();
        let top_row = history_top_row(&anchored, viewport);
        assert_eq!(top_row.trim(), "request 1");

        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(9),
                record: Record::UserRequest {
                    request: "request 9".to_owned(),
                },
            },
            progress_gap: false,
        });
        let after_append = rendered(&mut app, 40, 7);

        assert_eq!(app.history_document_top(), top);
        assert_eq!(history_top_row(&after_append, viewport), top_row);
        assert!(!after_append.contains("request 9"));
    }

    #[test]
    fn transcript_following_append_stays_following_bottom_and_shows_newest_row() {
        let mut app = app_with_history(App::default());
        rendered(&mut app, 40, 7);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(9),
                record: Record::UserRequest {
                    request: "request 9".to_owned(),
                },
            },
            progress_gap: false,
        });
        let after_append = rendered(&mut app, 40, 7);

        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert_eq!(app.history_document_top(), app.newest_history_top());
        assert!(after_append.contains("request 9"));
    }

    #[test]
    fn transcript_streamed_markdown_reflow_preserves_numeric_anchor() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: format!(
                    "[mutable][id]\n{}",
                    (0..10)
                        .map(|row| format!("row {row}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            },
        });
        let full = rendered(&mut app, 20, 20);
        let viewport = app.history_viewport.expect("history viewport");
        let mutable_row = app.history_document_top().saturating_add(
            full.lines()
                .position(|row| row.contains("[mutable"))
                .expect("unresolved link")
                .saturating_sub(usize::from(viewport.area.y)),
        );

        rendered(&mut app, 20, 6);
        assert!(
            mutable_row < app.newest_history_top(),
            "mutable row {mutable_row}, newest top {}",
            app.newest_history_top()
        );
        assert!(
            app.scroll_older(app.newest_history_top().saturating_sub(mutable_row))
                .visible_changed
        );
        let before_reflow = rendered(&mut app, 20, 6);
        let viewport = app.history_viewport.expect("history viewport");
        assert_eq!(app.history_position, HistoryPosition::Anchored(mutable_row));
        let top_row = history_top_row(&before_reflow, viewport);
        assert!(top_row.contains("[mutable"), "{top_row}");

        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            progress: ModelProgress::AssistantTextChunk {
                text: "\n\n[id]: https://example.test".to_owned(),
            },
        });
        let after_reflow = rendered(&mut app, 20, 6);

        assert_eq!(app.history_position, HistoryPosition::Anchored(mutable_row));
        assert_eq!(app.history_document_top(), mutable_row);
        assert_ne!(history_top_row(&after_reflow, viewport), top_row);
        assert!(history_top_row(&after_reflow, viewport).contains("mutable"));
    }

    #[test]
    fn transcript_reflow_shrink_resumes_following_after_anchor_clamps() {
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
        assert!(app.scroll_older(1).visible_changed);
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));

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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::UserRequest {
                    request: "followed tail".to_owned(),
                },
            },
            progress_gap: false,
        });
        let after_append = rendered(&mut app, 20, 6);

        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(after_append.contains("followed tail"));
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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));
        assert!(app.handle_key(history_home()).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
        assert!(app.handle_key(history_end()).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
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
        let position = app.history_position;
        let measured_rows = app.history_rows;
        let measured_height = app.history_height;
        assert_eq!(position, HistoryPosition::Anchored(0));

        for (width, height) in [(0, 0), (1, 2)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
            assert_eq!(app.history_rows, measured_rows);
            assert_eq!(app.history_height, measured_height);
            assert_eq!(app.history_position, position);
        }

        assert!(!app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert_eq!(app.history_position, position);
        assert!(app.handle_key(history_end()).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(history_home()).visible_changed);
        assert_eq!(app.history_position, position);

        rendered(&mut app, 12, 5);
        assert_eq!(app.history_position, position);
        rendered(&mut app, 120, 5);
        assert_eq!(app.history_position, position);
    }

    #[test]
    fn transcript_scrolling_preserves_oldest_anchor_after_authoritative_replacement() {
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
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));

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

        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
        rendered(&mut app, 120, 6);
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
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
        assert!(!app.tick(now + Duration::from_millis(49)).visible_changed);
        assert!(app.tick(now + Duration::from_millis(50)).visible_changed);
        assert_eq!(
            app.history_position,
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(1))
        );
        rendered_buffer(&mut app, 40, 7);
        assert!(app.tick(now + Duration::from_millis(100)).visible_changed);
        assert_eq!(
            app.history_position,
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(2))
        );
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
        assert!(!app.tick(now + Duration::from_millis(150)).visible_changed);
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
            assert!(
                app.tick(now + Duration::from_millis(elapsed))
                    .visible_changed
            );
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
        assert!(update.commands.is_empty());
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
        assert!(newer.handle_key(history_home()).visible_changed);
        assert_eq!(newer.history_position, HistoryPosition::Anchored(0));
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
        assert!(newer.tick(now + super::POLL_INTERVAL).visible_changed);
        assert_eq!(newer.history_position, HistoryPosition::Anchored(1));

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
            let _ = older.tick(now + super::POLL_INTERVAL * u32::try_from(cadence).unwrap());
        }
        assert_eq!(older.history_position, HistoryPosition::Anchored(0));
        assert!(older.drag.is_some_and(|drag| drag.edge.is_none()));
        assert!(
            !older
                .tick(now + super::POLL_INTERVAL * u32::try_from(older.history_rows + 1).unwrap())
                .visible_changed
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

        assert_eq!(
            app.history_position,
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(3))
        );
        assert_eq!(redraws, 2);
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_towards_newest_history() {
        let mut app = app_with_history(App::default());
        rendered(&mut app, 40, 7);
        assert!(app.scroll_older(3).visible_changed);
        let mut input =
            FakeInput::new([Some(mouse(MouseEventKind::ScrollDown)), Some(quit_event())]);

        run_transcript_loop(&mut app, &mut input);

        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
    }

    #[test]
    fn configuration_mouse_wheel_scrolls_transcript_history() {
        let mut app =
            app_with_history(App::configuration(crate::config::ready_snapshot_for_test()));
        let mut input = FakeInput::new([
            Some(mouse(MouseEventKind::Down(MouseButton::Left))),
            Some(mouse(MouseEventKind::ScrollUp)),
            Some(quit_event()),
        ]);

        let redraws = run_configuration_loop(&mut app, &mut input);

        assert_eq!(
            app.history_position,
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(3))
        );
        assert_eq!(redraws, 2);
    }

    #[test]
    fn configuration_mouse_wheel_scrolls_towards_newest_history() {
        let mut app =
            app_with_history(App::configuration(crate::config::ready_snapshot_for_test()));
        rendered(&mut app, 40, 7);
        assert!(app.scroll_older(3).visible_changed);
        let mut input =
            FakeInput::new([Some(mouse(MouseEventKind::ScrollDown)), Some(quit_event())]);

        run_configuration_loop(&mut app, &mut input);

        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
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
            model: "openai/gpt-5.6-sol".to_owned(),
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
            model: "openai/gpt-5.6-sol".to_owned(),
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
                    commentary: None,
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
                    commentary: None,
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
                            commentary: None,
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
    fn subagent_monitor_renders_two_rows_above_prompt() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::SubagentSpawned {
            record: crate::subagents::TaskRecord::for_test(
                "c1",
                1,
                crate::config::ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                None,
                1,
                Vec::new(),
            )
            .with_observation_for_test(Duration::from_secs(287), 0),
        });

        let screen = rendered(&mut app, 80, 12);
        let rows = screen.lines().collect::<Vec<_>>();
        let entry = rows
            .iter()
            .position(|row| row.contains("✓ subagent:c1"))
            .expect("subagent monitor entry");
        let prompt = rows
            .iter()
            .position(|row| row.starts_with("> "))
            .expect("prompt");

        assert_eq!(
            rows[entry].trim_end(),
            "  ✓ subagent:c1  openai/gpt-5.6-terra  0 steps  04:47"
        );
        assert!(rows[entry + 1].starts_with("    └ `test child`"));
        assert!(entry < prompt);
    }

    #[test]
    fn subagent_monitor_renders_parent_context_above_the_prompt() {
        let now = Instant::now();
        let mut app = App::default();
        for record in [
            crate::subagents::TaskRecord::for_test(
                "00",
                1,
                crate::config::ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                None,
                0,
                Vec::new(),
            ),
            crate::subagents::TaskRecord::for_test(
                "01",
                1,
                crate::config::ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                Some("00"),
                1,
                Vec::new(),
            ),
        ] {
            app.handle_event_at(RuntimeEvent::SubagentSpawned { record }, now);
        }

        let screen = rendered_at(&mut app, 80, 14, now);
        let rows = screen.lines().collect::<Vec<_>>();
        let root = rows
            .iter()
            .position(|row| row.contains("✓ subagent:00"))
            .expect("root monitor entry");
        let child = rows
            .iter()
            .position(|row| row.contains("└─✓ subagent:01"))
            .expect("nested monitor entry");
        let prompt = rows
            .iter()
            .position(|row| row.starts_with("> "))
            .expect("prompt");

        assert!(rows[root + 1].starts_with("    └ `test child`"));
        assert!(rows[child + 1].starts_with("      └ `test child`"));
        assert!(root < child && child < prompt, "{screen}");
    }

    #[test]
    fn subagent_monitor_stays_outside_history_selection_and_shrinks_with_the_prompt() {
        let now = Instant::now();
        let mut app = App::default();
        app.subagent_monitor
            .insert_for_test("child1", TaskState::Completed, now);

        let initial = rendered_at(&mut app, 80, 12, now);
        let initial_history_height = app.history_height;
        let buffer = rendered_buffer(&mut app, 80, 12);
        let (column, row) = cell_start(&buffer, "✓ subagent:child1");
        assert!(
            app.history_viewport
                .is_some_and(|viewport| !viewport.area.contains(Position::new(column, row)))
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), column, row),
            now,
        );
        assert!(app.selection.is_none());

        set_prompt(&mut app, "one\ntwo");
        let grown = rendered_at(&mut app, 80, 12, now);
        assert_eq!(app.history_height + 1, initial_history_height);
        assert!(initial.contains("✓ subagent:child1"));
        assert!(grown.contains("✓ subagent:child1"));

        let tiny = rendered_at(&mut app, 80, 3, now);
        assert!(!tiny.contains("✓ subagent:child1"));

        let mut configured = App::configured(crate::config::ready_snapshot_for_test());
        let mut monitored = App::configured(crate::config::ready_snapshot_for_test());
        monitored
            .subagent_monitor
            .insert_for_test("child1", TaskState::Completed, now);
        for height in [2, 3] {
            assert_eq!(
                rendered_at(&mut monitored, 80, height, now),
                rendered_at(&mut configured, 80, height, now)
            );
        }
    }

    #[test]
    fn subagent_monitor_does_not_scroll_with_history() {
        let now = Instant::now();
        let mut app = app_with_history(App::default());
        app.subagent_monitor
            .insert_for_test("child1", TaskState::Running, now);

        rendered_at(&mut app, 80, 9, now);
        assert!(app.handle_key_at(history_home(), now).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
        assert!(rendered_at(&mut app, 80, 9, now).contains("• subagent:child1"));
    }

    #[test]
    fn subagent_monitor_survives_non_request_actions_and_clears_on_accepted_request() {
        let now = Instant::now();
        let mut app = App::default();
        let record = crate::subagents::TaskRecord::for_test(
            "c1",
            1,
            crate::config::ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        )
        .with_observation_for_test(Duration::from_secs(47), 0);
        app.handle_event_at(
            RuntimeEvent::SubagentSpawned {
                record: record.clone(),
            },
            now,
        );

        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1"));
        assert!(
            app.handle_key_at(modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL), now)
                .commands
                .is_empty()
        );
        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1"));
        set_prompt(&mut app, "/model");
        app.handle_key_at(key(KeyCode::Enter), now);
        app.handle_key_at(key(KeyCode::Esc), now);
        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1"));
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        set_prompt(&mut app, "busy draft");
        assert!(
            app.handle_key_at(key(KeyCode::Enter), now)
                .commands
                .is_empty()
        );
        app.handle_event_at(
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: RunId(1),
            },
            now,
        );
        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1"));

        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "accepted".to_owned(),
                    },
                },
                progress_gap: false,
            },
            now,
        );

        assert!(!rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1"));
        app.handle_event_at(RuntimeEvent::SubagentFinished { record }, now);
        assert!(!rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1"));
    }

    #[test]
    fn ctrl_o_rebuilds_finished_subagent_history_without_expanding_the_sticky_monitor() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(
            RuntimeEvent::SubagentFinished {
                record: crate::subagents::TaskRecord::for_test(
                    "c1",
                    1,
                    crate::config::ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                    None,
                    1,
                    Vec::new(),
                ),
            },
            now,
        );

        let before = rendered_at(&mut app, 80, 12, now);
        let before_counts = app.transcript.construction_counts();
        assert!(before.contains("Completed · subagent:c1"), "{before}");
        assert!(before.contains("✓ subagent:c1"), "{before}");

        let update =
            app.handle_key_at(modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL), now);
        assert!(update.commands.is_empty());
        assert!(update.visible_changed);

        let expanded = rendered_at(&mut app, 80, 12, now);
        assert!(expanded.contains("✓ subagent:c1"), "{expanded}");
        assert_ne!(app.transcript.construction_counts(), before_counts);
    }

    #[test]
    fn subagent_monitor_ticks_only_for_visible_seconds_without_rebuilding_history() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event(completed_record(1, 1, "settled()", vec![]));
        app.subagent_monitor
            .insert_for_test("child1", TaskState::Running, now);

        rendered_at(&mut app, 80, 12, now);
        let before = app.transcript.construction_counts();
        assert!(!app.tick(now + Duration::from_millis(999)).visible_changed);
        assert!(app.tick(now + Duration::from_secs(1)).visible_changed);
        rendered_at(&mut app, 80, 12, now + Duration::from_secs(1));
        assert_eq!(app.transcript.construction_counts(), before);
        rendered_at(&mut app, 80, 3, now + Duration::from_secs(2));
        assert!(!app.tick(now + Duration::from_secs(3)).visible_changed);
    }

    #[test]
    fn escape_cancels_foreground_once_then_background_except_in_configuration() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);

        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::Cancel]
        );
        app.handle_event_at(RuntimeEvent::RunSettled { run_id: RunId(1) }, now);
        assert_eq!(
            enter_at(&mut app, "second prompt", now).commands,
            vec![RuntimeCommand::Submit {
                request: "second prompt".to_owned(),
            }]
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::Cancel]
        );
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "second prompt".to_owned(),
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::CancelBackground]
        );

        let mut idle = App::default();
        assert_eq!(
            idle.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::CancelBackground]
        );

        let mut disabled = App::default();
        disabled.handle_event_at(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::OpenAiAuthentication,
            },
            now,
        );
        assert_eq!(
            disabled.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::CancelBackground]
        );
        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        assert!(
            configuration
                .handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );

        let mut picker = App::default();
        set_prompt(&mut picker, "/model");
        picker.handle_key_at(key(KeyCode::Enter), now);
        assert!(
            picker
                .handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
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
        assert!(with_rows[11].ends_with("↑1 ↓1 R0 W0 CH0.0% $0.00002400 0.0%/272k ─"));
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
    fn configured_two_row_prompt_renders_footer_model_and_thinking() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let screen = rendered(&mut app, 33, 2);
        let rows = screen.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], "─  …t-5.6-sol • thinking medium ─");

        let buffer = rendered_buffer(&mut app, 33, 2);
        assert_eq!(
            buffer[(0, 1)].fg,
            super::thinking_rule_colour(SOL.thinking_levels, SOL.default_thinking)
        );

        for width in 0..=4 {
            let backend = TestBackend::new(width, 2);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
        }
    }

    #[test]
    fn configured_three_row_prompt_keeps_top_rule_and_footer_model_and_thinking() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let screen = rendered(&mut app, 33, 3);
        let rows = screen.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1], "─".repeat(33));
        assert_eq!(rows[2], "─  …t-5.6-sol • thinking medium ─");

        let buffer = rendered_buffer(&mut app, 33, 3);
        assert_eq!(
            buffer[(0, 2)].fg,
            super::thinking_rule_colour(SOL.thinking_levels, SOL.default_thinking)
        );

        for width in 0..=4 {
            let backend = TestBackend::new(width, 3);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
        }
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
                .is_some_and(|header| header.contains("Shift-Enter/Ctrl-J newline"))
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
        assert!(!configuration_screen.contains("Shift-Enter/Ctrl-J newline"));
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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        set_prompt(&mut app, &"a".repeat(19));
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 7);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        set_prompt(&mut app, "");
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 8);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(history_home()).visible_changed);
        let position = app.history_position;
        assert_eq!(position, HistoryPosition::Anchored(0));

        set_prompt(&mut app, &"a".repeat(19));
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 7);
        assert_eq!(app.history_position, position);

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
        assert_eq!(app.history_position, position);

        set_prompt(&mut app, "");
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 8);
        assert_eq!(app.history_position, position);
        let backend = TestBackend::new(0, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render(frame, &mut app))
            .unwrap();
        assert_eq!(app.history_position, position);
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
        assert!(
            two.lines()
                .nth(1)
                .is_some_and(|row| row.ends_with("↑100 ↓20 R10 W5 CH10.0% $0.0007690 0.0%/272k ─"))
        );

        let three = rendered(&mut app, 80, 3);
        let rows = three.lines().collect::<Vec<_>>();
        assert!(rows[0].starts_with("Status:"));
        assert_eq!(rows[1], "─".repeat(80));
        assert!(rows[2].ends_with("↑100 ↓20 R10 W5 CH10.0% $0.0007690 0.0%/272k ─"));

        let four = rendered(&mut app, 80, 4);
        let rows = four.lines().collect::<Vec<_>>();
        assert!(rows[2].starts_with("> ask"));
        assert!(rows[3].ends_with("↑100 ↓20 R10 W5 CH10.0% $0.0007690 0.0%/272k ─"));
        assert_eq!(app.history_height, 0);

        let larger = rendered(&mut app, 80, 6);
        assert!(
            larger
                .lines()
                .nth(4)
                .is_some_and(|row| row.starts_with("> ask"))
        );
        assert!(
            larger
                .lines()
                .nth(5)
                .is_some_and(|row| row.ends_with("↑100 ↓20 R10 W5 CH10.0% $0.0007690 0.0%/272k ─"))
        );
        assert_eq!(app.history_height, 2);
        assert!(app.history_rows > 0);
    }

    #[test]
    fn configured_session_delegates_complete_footer_context() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("worktrees/interactive/settings/kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        assert!(app.name_session("Refactor auth").visible_changed);
        app.handle_event(completed_record(
            1,
            1,
            "metrics()",
            vec![detailed_usage(
                Some(217_600),
                Some(0),
                Some(163_200),
                Some(0),
                Some(0),
                Some(217_600),
            )],
        ));

        let footer = rendered(&mut app, 200, 20);
        assert!(
            footer.contains("~/worktrees/interactive/settings/kiki (Refactor auth)"),
            "{footer}"
        );
        assert!(
            footer.contains("↑218k ↓0 R163k W0 CH75.0% $0.2829 80.0%/272k"),
            "{footer}"
        );
        assert!(footer.contains("gpt-5.6-sol • thinking medium"), "{footer}");
    }

    #[test]
    fn footer_is_absent_without_metrics_for_unconfigured_and_configuration_only_sessions() {
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
                    && full[(*column + 2, 3)].symbol() == "."
                    && full[(*column + 3, 3)].symbol() == "0"
                    && full[(*column + 4, 3)].symbol() == "%"
            })
            .expect("context percentage");
        assert_eq!(full[(column, 3)].fg, Color::Yellow);
        let full = rendered(&mut app, 80, 4);
        assert!(
            full.lines()
                .nth(3)
                .is_some_and(|row| row.ends_with("CH75.0% $0.2829 80.0%/272k ─"))
        );

        let one = rendered(&mut app, 1, 4);
        assert_eq!(one.lines().nth(3), Some("…"));
        let narrow = rendered(&mut app, 29, 4);
        assert_eq!(narrow.lines().nth(3), Some("─ ────── $0.2829 80.0%/272k ─"));
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
            rendered(&mut app, 80, 5)
                .lines()
                .last()
                .is_some_and(|row| row.ends_with("↑100 ↓20 R10 W5 CH10.0% $0.0007690 0.0%/272k ─"))
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
            rendered(&mut app, 80, 8)
                .lines()
                .last()
                .is_some_and(|row| row.ends_with("↑107 ↓30 R10 W5 CH0.0% $0.0007690 0.0%/272k ─"))
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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        assert!(app.handle_key(history_home()).visible_changed);
        let position = app.history_position;
        assert_eq!(position, HistoryPosition::Anchored(0));
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
        assert_eq!(app.history_position, position);
    }

    #[test]
    fn footer_keeps_scroll_when_dimensions_are_non_renderable() {
        let mut app = App::default();
        for run_id in 1..=4 {
            app.handle_event(completed_record(run_id, 1, "code()", vec![usage(1, 1)]));
        }
        rendered(&mut app, 20, 5);
        app.handle_key(history_home());
        let position = app.history_position;
        assert_eq!(position, HistoryPosition::Anchored(0));

        rendered(&mut app, 20, 0);
        assert_eq!(app.history_position, position);
        let backend = TestBackend::new(0, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| super::render(frame, &mut app))
            .unwrap();
        assert_eq!(app.history_position, position);
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
                                model: "openai/gpt-5.6-sol".to_owned(),
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
        assert!(history.contains("↑100 ↓100 R0 W0 $0.002400"));
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
        assert!(rendered(&mut active, 120, 40).contains("↑100 ↓100 R0 W0 $0.002400 0.0%/272k"));
    }

    #[test]
    fn transcript_following_bottom_and_manual_scroll_survive_live_replacement() {
        let mut app = App::default();
        for step in 1..=4 {
            app.handle_event(completed_record(1, step, &format!("line_{step}"), vec![]));
        }
        rendered(&mut app, 50, 5);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(2),
            step_id: ModelStepId(5),
            progress: ModelProgress::AssistantTextChunk {
                text: "live".to_owned(),
            },
        });
        rendered(&mut app, 50, 5);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        let position = app.history_position;
        app.handle_event(completed_record(2, 5, "settled", vec![]));
        rendered(&mut app, 50, 5);
        assert_eq!(app.history_position, position);
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
                    commentary: None,
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
            assert!(update.commands.is_empty());
            assert!(update.visible_changed);
            assert_eq!(app.prompt.text(), "kept prompt");
        }
        assert!(rendered(&mut app, 60, 80).contains("visible reasoning"));

        let hidden = app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(hidden.commands.is_empty());
        assert!(hidden.visible_changed);
        let screen = rendered(&mut app, 60, 80);
        assert!(screen.contains("thinking hidden · Ctrl-T to show"));
        assert!(!screen.contains("visible reasoning"));

        for code in ['o', 'O'] {
            let update = app.handle_key(KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL));
            assert!(update.commands.is_empty());
            assert!(update.visible_changed);
            assert_eq!(app.prompt.text(), "kept prompt");
        }
        let collapsed = rendered(&mut app, 60, 40);
        assert!(!collapsed.contains("code line 01"));
        assert!(collapsed.contains("code line 33"));
        assert!(collapsed.contains("[4 earlier tool calls omitted]"));

        let expanded = app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(expanded.commands.is_empty());
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
        assert!(rendered(&mut disabled, 60, 8).contains("thinking hidden · Ctrl-T to show"));

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        let update =
            configuration.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(update.commands.is_empty());
        assert!(!update.visible_changed);
        let reasoning =
            configuration.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(reasoning.commands.is_empty());
        assert!(!reasoning.visible_changed);
        configuration.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(2),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    commentary: None,
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
        assert!(screen.contains("thinking hidden · Ctrl-T to show"));
    }

    #[test]
    fn controls_redraw_only_when_visible_and_preserve_oldest_anchor_after_height_reductions() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::CompletedExecution {
                    step_id: ModelStepId(1),
                    commentary: None,
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
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .visible_changed
        );
        rendered(&mut app, 20, 6);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.handle_key(history_home()).visible_changed);
        let position = app.history_position;
        assert_eq!(position, HistoryPosition::Anchored(0));
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .visible_changed
        );
        rendered(&mut app, 20, 6);
        assert_eq!(app.history_position, position);

        rendered(&mut app, 20, 6);
        app.handle_key(history_home());
        let position = app.history_position;
        assert!(
            app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
                .visible_changed
        );
        rendered(&mut app, 20, 6);
        assert_eq!(app.history_position, position);

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
        let position = app.history_position;
        for (width, height) in [(0, 0), (0, 1), (1, 1), (1, 2), (2, 2), (3, 3)] {
            app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| super::render(frame, &mut app))
                .unwrap();
            if width == 0 {
                assert_eq!(app.history_position, position);
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
