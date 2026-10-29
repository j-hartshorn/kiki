use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod picker;
mod slash;
mod subagents;
mod tree;

use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::SetTitle;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use rattles::TickedRattler;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use unicode_segmentation::UnicodeSegmentation;

use crate::clipboard::{ClipboardPort, LocalClipboard};
use crate::config::models::{CATALOGUE, Provider};
use crate::config::settings::{SaveError, SaveOutcome, SettingsContext};
use crate::config::{DEFAULT_MODEL, ModelConfiguration, ModelSelection, StartupSnapshot};
use crate::model::SkillActivation;
use crate::prompt_editor::{PromptEdit, PromptEditor, PromptLayout, PromptRow};
use crate::runtime::{
    CorrectionOutcome, NavigationOutcome, ProgressEnvelope, RunId, RuntimeCommand, RuntimeEvent,
    RuntimeInitialState, RuntimeProgressReceiver, TreeRequestOutcome, control_command,
};
use crate::selection::{DocumentPosition, Selection};
use crate::skills::{SkillCatalog, SkillDiagnostic, SkillLookupError};
use crate::telemetry::{Trace, TraceEvent};
use crate::theme::{self, Appearance, Theme, ThemeSetting};
#[cfg(test)]
pub(crate) use crate::transcript::format_startup_configuration;
use crate::transcript::{
    FooterContext, ModelChangeCard, Transcript, TranscriptLayout, model_change_card,
    normalise_terminal_text, page_style,
};
use picker::{Picker, PickerAction, PickerItem};
use tree::{TreeNavigator, TreeNavigatorAction};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MOUSE_WHEEL_SCROLL_ROWS: usize = 1;
const SETTLED_ESCAPE_WINDOW: Duration = Duration::from_millis(500);
const TREE_BUSY_NOTICE: &str = "Session tree is unavailable while work is active.";
const TREE_READ_ONLY_NOTICE: &str = "Session tree is unavailable while the session is read-only.";
const SESSION_RESTORE_FAILED_NOTICE: &str = "Session could not be restored.";
const THROBBER_ROWS: u16 = 2;
const THROBBER_LEFT_PADDING: u16 = 1;

rattles::rattle!(
    GenerationThrobber,
    generation_throbber,
    4,
    50,
    [
        "⠁  ⢀", "⠈  ⡀", " ⠁⢀ ", " ⠈⡀ ", " ⢀⠁ ", " ⡀⠈ ", "⢀  ⠁", "⡀  ⠈", "⠄  ⠐", "⠂  ⠠"
    ]
);

rattles::rattle!(
    CompactionThrobber,
    compaction_throbber,
    9,
    50,
    [
        "·       ·",
        "●·     ·●",
        "·●·   ·●·",
        " ·●· ·●· ",
        "  ·●·●·  ",
        "   ·●·   ",
        "    ·    ",
        "         "
    ]
);

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
    Theme(ThemeSetting),
    OpenThemePicker,
}

#[derive(Clone, Copy)]
struct ThinkingPreview {
    selection: ModelSelection,
    deadline: Instant,
    sent_revision: Option<u64>,
}

struct App {
    theme: &'static Theme,
    theme_setting: ThemeSetting,
    appearance: Appearance,
    mode: AppMode,
    prompt: PromptEditor,
    prompt_width: u16,
    prompt_view: Option<(Rect, PromptLayout)>,
    prompt_drag: Option<usize>,
    prompt_selection: Option<Range<usize>>,
    transcript: Transcript,
    subagent_monitor: subagents::SubagentMonitor,
    foreground_cancellation_requested: bool,
    compaction_cancellation_requested: bool,
    compact_acknowledgement_pending: bool,
    settled_escape_at: Option<Instant>,
    submission_at: Option<Instant>,
    correction_pending: bool,
    correction_run_id: Option<RunId>,
    ignored_progress_run_id: Option<RunId>,
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
    generation_throbber: TickedRattler<GenerationThrobber>,
    compaction_throbber: TickedRattler<CompactionThrobber>,
    throbber_next_tick: Option<Instant>,
    settings: Option<SettingsContext>,
    workspace_root: Option<String>,
    home: Option<PathBuf>,
    session_name: Option<String>,
    settings_selection: ModelSelection,
    session_thinking: BTreeMap<&'static str, &'static str>,
    providers: Vec<Provider>,
    picker: Option<Picker<PickerValue>>,
    return_to_settings: bool,
    tree_navigator: Option<TreeNavigator>,
    manual_navigation_pending: bool,
    next_tree_revision: u64,
    completion: slash::Completion,
    skill_catalog: Arc<SkillCatalog>,
    pending_skill_draft: Option<String>,
    slash_overlay: Option<Rect>,
    preview: Option<ThinkingPreview>,
    next_settings_revision: u64,
    pending_settings: BTreeMap<u64, ModelSelection>,
    next_name_revision: u64,
    pending_name: Option<(u64, String)>,
}

impl Default for App {
    fn default() -> Self {
        let settings_selection = ModelSelection::new(DEFAULT_MODEL, DEFAULT_MODEL.default_thinking)
            .expect("default model thinking must be supported");
        Self {
            theme: &theme::DARK,
            theme_setting: ThemeSetting::Automatic,
            appearance: Appearance::Dark,
            mode: AppMode::Configured,
            prompt: PromptEditor::default(),
            prompt_width: 80,
            prompt_view: None,
            prompt_drag: None,
            prompt_selection: None,
            transcript: Transcript::default(),
            subagent_monitor: subagents::SubagentMonitor::default(),
            foreground_cancellation_requested: false,
            compaction_cancellation_requested: false,
            compact_acknowledgement_pending: false,
            settled_escape_at: None,
            submission_at: None,
            correction_pending: false,
            correction_run_id: None,
            ignored_progress_run_id: None,
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
            generation_throbber: generation_throbber().into_ticked(),
            compaction_throbber: compaction_throbber().into_ticked(),
            throbber_next_tick: None,
            settings: None,
            workspace_root: None,
            home: None,
            session_name: None,
            settings_selection,
            session_thinking: BTreeMap::from([(DEFAULT_MODEL.id, DEFAULT_MODEL.default_thinking)]),
            providers: vec![Provider::OpenAi, Provider::Anthropic, Provider::Google],
            picker: None,
            return_to_settings: false,
            tree_navigator: None,
            manual_navigation_pending: false,
            next_tree_revision: 0,
            completion: slash::Completion::default(),
            skill_catalog: Arc::default(),
            pending_skill_draft: None,
            slash_overlay: None,
            preview: None,
            next_settings_revision: 0,
            pending_settings: BTreeMap::new(),
            next_name_revision: 0,
            pending_name: None,
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

fn runtime_event_run_id(event: &RuntimeEvent) -> Option<RunId> {
    match event {
        RuntimeEvent::RunStarted { run_id }
        | RuntimeEvent::ModelStepStarted { run_id, .. }
        | RuntimeEvent::GenerationRestarted { run_id, .. }
        | RuntimeEvent::ModelProgress { run_id, .. }
        | RuntimeEvent::ExecutionStarted { run_id, .. }
        | RuntimeEvent::ToolUpdated { run_id, .. }
        | RuntimeEvent::RunPreviewUpdated { run_id, .. }
        | RuntimeEvent::StdoutDelta { run_id, .. }
        | RuntimeEvent::StdoutTruncated { run_id, .. }
        | RuntimeEvent::RunSettled { run_id }
        | RuntimeEvent::AutomaticCompactionStarted { run_id, .. }
        | RuntimeEvent::CorrectionCompleted { run_id, .. } => Some(*run_id),
        RuntimeEvent::RecordAppended { record, .. } => Some(record.run_id),
        RuntimeEvent::SettingsAccepted { .. }
        | RuntimeEvent::NameAccepted { .. }
        | RuntimeEvent::NavigationCompleted { .. }
        | RuntimeEvent::TreeCatalogue { .. }
        | RuntimeEvent::SubmissionRejectedBusy { .. }
        | RuntimeEvent::SkillActivationRejected { .. }
        | RuntimeEvent::CompactionAcknowledged { .. }
        | RuntimeEvent::CompactionLifecycle { .. }
        | RuntimeEvent::RuntimeDisabled { .. }
        | RuntimeEvent::SubmissionRejectedDisabled { .. }
        | RuntimeEvent::SubagentSpawned { .. }
        | RuntimeEvent::SubagentStatus { .. }
        | RuntimeEvent::SubagentRecord { .. }
        | RuntimeEvent::SubagentFinished { .. }
        | RuntimeEvent::SubagentOperationSettled { .. } => None,
    }
}

impl App {
    fn with_appearance(mut self, appearance: Appearance) -> Self {
        self.appearance = appearance;
        self.theme = self.theme_setting.resolve(self.appearance);
        self
    }

    fn with_skill_catalog(mut self, catalog: Arc<SkillCatalog>) -> Self {
        self.transcript
            .append_metadata(skill_diagnostic_report(&catalog));
        self.completion = slash::Completion::with_skills(
            catalog
                .entries()
                .map(|skill| (skill.name(), skill.description())),
        );
        self.skill_catalog = catalog;
        self.refresh_completion();
        self
    }

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
        let theme_setting = settings.theme();
        Self {
            theme: theme_setting.resolve(Appearance::Dark),
            theme_setting,
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

    fn restored(startup: StartupSnapshot, initial: &RuntimeInitialState) -> Self {
        let mut app = Self::configured(startup.clone());
        if let Some(metadata) = &initial.metadata {
            app.session_name = metadata.name().map(str::to_owned);
            app.session_thinking = metadata.preferences().thinking().clone();
            if metadata.type_check_disabled() {
                app.set_notice("Type checking is off for this session.".to_owned());
            }
        }
        app.transcript = Transcript::restored(startup, initial);
        app.subagent_monitor = subagents::SubagentMonitor::restored(&initial.timeline);
        app
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
        if self.is_tree_loading() {
            return Update::default();
        }
        if let Some(update) = self.handle_slash_overlay_mouse(mouse) {
            return update;
        }
        let notice_cleared = self.clear_notice();
        if let Some(mut update) = self.handle_prompt_mouse(mouse) {
            update.visible_changed |= notice_cleared;
            return update;
        }
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

    fn prompt_position(&self, column: u16, row: u16, clamp: bool) -> Option<usize> {
        let (area, layout) = self.prompt_view.as_ref()?;
        if area.is_empty() || layout.rows().is_empty() {
            return None;
        }
        if !clamp && !area.contains(Position::new(column, row)) {
            return None;
        }
        let row = row.clamp(area.y, area.bottom().saturating_sub(1));
        let index = usize::from(row - area.y).min(layout.rows().len() - 1);
        let visual = &layout.rows()[index];
        if self.prompt.text().get(visual.source()) != Some(visual.text()) {
            return None;
        }
        let target = usize::from(column.saturating_sub(area.x));
        let mut cells = 0;
        for (offset, grapheme) in visual.text().grapheme_indices(true) {
            cells += Line::raw(grapheme).width();
            if target < cells {
                return Some(visual.source().start + offset);
            }
        }
        Some(visual.source().end)
    }

    fn handle_prompt_mouse(&mut self, mouse: MouseEvent) -> Option<Update> {
        if self.is_submission_disabled()
            || self.compact_acknowledgement_pending
            || self.picker.is_some()
            || self.tree_navigator.is_some()
        {
            return None;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let caret = self.prompt_position(mouse.column, mouse.row, false)?;
                self.invalidate_selection();
                self.prompt_drag = Some(caret);
                self.prompt.move_to(caret);
                self.refresh_completion();
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                let anchor = self.prompt_drag?;
                let focus = self.prompt_position(mouse.column, mouse.row, true)?;
                self.prompt_selection =
                    (anchor != focus).then_some(anchor.min(focus)..anchor.max(focus));
                if matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)) {
                    self.prompt_drag = None;
                    if let Some(range) = &self.prompt_selection {
                        self.pending_copy_text = Some(self.prompt.text()[range.clone()].to_owned());
                    }
                    self.prompt.move_to(focus);
                    self.refresh_completion();
                }
            }
            _ => return None,
        }
        Some(Update {
            visible_changed: true,
            ..Update::default()
        })
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
        if self.prompt_drag.take().is_some() {
            let visible_changed = self.prompt_selection.take().is_some();
            return Some(Update {
                visible_changed,
                ..Update::default()
            });
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
        let notice_cleared = if !self.is_tree_loading() {
            self.clear_notice()
        } else {
            false
        };
        let prompt_selection_cleared = self.prompt_selection.take().is_some();
        self.prompt_drag = None;
        let mut update = self.handle_key_inner(key, now);
        update.visible_changed |= prompt_selection_cleared;
        update.visible_changed |= notice_cleared;
        update
    }

    fn handle_paste(&mut self, text: &str) -> Update {
        let selection_cleared = self.prompt_selection.take().is_some();
        self.prompt_drag = None;
        let notice_cleared = if !self.is_tree_loading() {
            self.clear_notice()
        } else {
            false
        };
        let prompt_changed = if self.is_submission_disabled()
            || self.compact_acknowledgement_pending
            || self.picker.is_some()
            || self.tree_navigator.is_some()
        {
            false
        } else {
            let text = normalise_terminal_text(&normalise_paste_line_endings(text));
            self.prompt.insert_paste(&text)
        };
        Update {
            commands: Vec::new(),
            visible_changed: prompt_changed
                || self.refresh_completion()
                || notice_cleared
                || selection_cleared,
            quit: false,
        }
    }

    fn handle_key_inner(&mut self, key: KeyEvent, now: Instant) -> Update {
        if key.is_release() {
            return Update::default();
        }
        if self.tree_navigator.is_some() {
            return self.handle_tree_navigator_key(key);
        }
        if self.picker.is_some() {
            return self.handle_picker_key(key);
        }
        if self.compact_acknowledgement_pending && matches!(key.code, KeyCode::Char('c' | 'C')) {
            return Update::default();
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
                KeyCode::Char('p' | 'P') if !self.is_submission_disabled() => {
                    self.open_model_picker("");
                    return Update {
                        commands: Vec::new(),
                        visible_changed: true,
                        quit: false,
                    };
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
                KeyCode::Char('r' | 'R') if !self.is_configuration_only() => {
                    return self.open_session_tree("");
                }
                _ => {}
            }
        }

        if let Some(update) = self.handle_history_key(key) {
            return update;
        }
        if key.code == KeyCode::Esc {
            if self.is_completely_settled() {
                return self.handle_settled_escape(now);
            }
            if self.is_configuration_only() {
                return Update::default();
            }
            if self.compact_acknowledgement_pending && !self.compaction_cancellation_requested {
                self.foreground_cancellation_requested = true;
                self.compaction_cancellation_requested = true;
                return Update::with_commands([RuntimeCommand::Cancel { correction: false }]);
            }
            if !self.transcript.is_disabled()
                && !self.transcript.is_idle()
                && self.transcript.has_foreground_compaction()
                && !self.compaction_cancellation_requested
            {
                self.foreground_cancellation_requested = true;
                self.compaction_cancellation_requested = true;
                return Update::with_commands([RuntimeCommand::Cancel { correction: false }]);
            }
            if !self.transcript.is_disabled()
                && !self.transcript.is_idle()
                && !self.foreground_cancellation_requested
            {
                self.foreground_cancellation_requested = true;
                let correction = self.submission_at.is_some_and(|submitted_at| {
                    now.saturating_duration_since(submitted_at) <= Duration::from_secs(2)
                });
                self.correction_pending = correction;
                return Update::with_commands([RuntimeCommand::Cancel { correction }]);
            }
            return Update::with_commands([RuntimeCommand::CancelBackground]);
        }
        if self.is_submission_disabled() {
            if !self.is_configuration_only()
                && key.code == KeyCode::Enter
                && key.modifiers.is_empty()
                && let Some(argument) = slash::parse(self.prompt.text()).and_then(|command| {
                    matches!(command.action, slash::Action::Tree)
                        .then(|| command.argument.to_owned())
                })
            {
                self.prompt.take();
                return self.open_session_tree(&argument);
            }
            return Update::default();
        }
        if is_shift_tab(key) {
            return self.cycle_thinking_preview(now);
        }
        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && let Some(focus) = slash::parse(self.prompt.text()).and_then(|command| {
                (command.action == slash::Action::Compact).then(|| command.argument.to_owned())
            })
        {
            return self.dispatch_compact(&focus);
        }
        if (self.manual_navigation_pending || self.correction_pending)
            && key.code == KeyCode::Enter
            && key.modifiers.is_empty()
        {
            self.set_notice("Session restoration is still in progress.".to_owned());
            return Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
        if let Some(update) = self.handle_completion_key(key) {
            return update;
        }
        if key.code == KeyCode::Enter && key.modifiers.is_empty() && !self.prompt.is_empty() {
            if let Some(command) = slash::parse(self.prompt.text())
                && command.action == slash::Action::Skill
            {
                let name = command.skill_name.unwrap_or_default().to_owned();
                return self.dispatch_skill(&name, now);
            }
            let draft = self.prompt.take();
            if let Some(command) = slash::parse(&draft) {
                return self.handle_slash_command(command);
            }
            if self.transcript.is_idle() {
                let mut commands = Vec::new();
                self.flush_preview(&mut commands);
                self.transcript.begin_submission();
                self.submission_at = Some(now);
                commands.push(RuntimeCommand::Submit {
                    request: draft,
                    activation: None,
                });
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
        if !self.compact_acknowledgement_pending
            && let Some(edit) = self.prompt_edit(key)
        {
            let prompt_changed = self.prompt.apply(edit, self.prompt_width);
            return Update {
                commands: Vec::new(),
                visible_changed: prompt_changed || self.refresh_completion(),
                quit: false,
            };
        }
        Update::default()
    }

    fn is_completely_settled(&self) -> bool {
        !self.is_submission_disabled()
            && self.prompt.text().trim().is_empty()
            && self.transcript.is_idle()
            && !self.foreground_cancellation_requested
            && !self.manual_navigation_pending
            && !self.correction_pending
            && !self.subagent_monitor.has_active_entries()
    }

    fn handle_settled_escape(&mut self, now: Instant) -> Update {
        let double_escape = self
            .settled_escape_at
            .is_some_and(|first| now.saturating_duration_since(first) < SETTLED_ESCAPE_WINDOW);
        if double_escape {
            self.settled_escape_at = None;
            self.open_session_tree("")
        } else {
            self.settled_escape_at = Some(now);
            Update::default()
        }
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
                let token = self.completion.selected_token()?.to_owned();
                self.prompt.take();
                self.prompt.insert_paste(&token);
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
        if self.is_exact_catalog_skill_command() {
            self.completion.sync("", false)
        } else {
            self.completion
                .sync(self.prompt.text(), self.prompt.caret_at_end())
        }
    }

    fn is_exact_catalog_skill_command(&self) -> bool {
        let Some(command) = slash::parse(self.prompt.text()) else {
            return false;
        };
        if command.action != slash::Action::Skill {
            return false;
        }
        let Some(name) = command.skill_name else {
            return false;
        };
        !matches!(
            self.skill_catalog.lookup(name),
            Err(SkillLookupError::Unknown(_))
        )
    }

    fn dispatch_compact(&mut self, focus: &str) -> Update {
        if self.compact_acknowledgement_pending {
            return Update::default();
        }
        if self.manual_navigation_pending {
            self.set_notice("Session restoration is still in progress.".to_owned());
            return Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
        self.compact_acknowledgement_pending = true;
        let mut update = Update::with_commands([RuntimeCommand::Compact {
            focus: focus.to_owned(),
        }]);
        update.visible_changed = true;
        update
    }

    fn dispatch_skill(&mut self, name: &str, now: Instant) -> Update {
        if !self.transcript.is_idle() {
            self.set_notice("A run is still active.".to_owned());
            return Update {
                visible_changed: true,
                ..Update::default()
            };
        }
        if name.is_empty() {
            self.set_notice("Enter a skill name after /skill:.".to_owned());
            return Update {
                visible_changed: true,
                ..Update::default()
            };
        }
        let activation = match self.skill_catalog.lookup(name) {
            Ok(skill) => SkillActivation::from_skill(skill),
            Err(error) => {
                self.set_notice(normalise_single_line(&error.to_string()));
                return Update {
                    visible_changed: true,
                    ..Update::default()
                };
            }
        };
        let request = self.prompt.take();
        self.refresh_completion();
        let mut commands = Vec::new();
        self.flush_preview(&mut commands);
        self.transcript.begin_submission();
        self.submission_at = Some(now);
        self.pending_skill_draft = Some(request.clone());
        commands.push(RuntimeCommand::Submit {
            request,
            activation: Some(activation),
        });
        let mut update = Update::with_commands(commands);
        update.visible_changed = true;
        update
    }

    fn handle_slash_command(&mut self, command: slash::Parsed<'_>) -> Update {
        match command.action {
            slash::Action::Compact => {
                unreachable!("compact dispatch is handled before editor consumption")
            }
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
            slash::Action::Settings => {
                self.open_settings_picker();
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            slash::Action::Skill => {
                unreachable!("direct skill dispatch precedes editor consumption")
            }
            slash::Action::Theme => {
                self.open_theme_picker(false);
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            slash::Action::Tree => self.open_session_tree(command.argument),
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

    fn is_tree_loading(&self) -> bool {
        self.tree_navigator
            .as_ref()
            .is_some_and(TreeNavigator::is_loading)
    }

    fn handle_tree_navigator_key(&mut self, key: KeyEvent) -> Update {
        let action = self
            .tree_navigator
            .as_mut()
            .expect("tree navigator was checked before handling its key")
            .handle_key(key);
        match action {
            TreeNavigatorAction::None => Update::default(),
            TreeNavigatorAction::Changed => Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            },
            TreeNavigatorAction::Close => {
                self.tree_navigator = None;
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            TreeNavigatorAction::Copy(text) => {
                self.pending_copy_text = Some(text);
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            TreeNavigatorAction::Navigate(entry) => {
                self.tree_navigator = None;
                self.manual_navigation_pending = true;
                let mut update = Update::with_commands([RuntimeCommand::Navigate { entry }]);
                update.visible_changed = true;
                update
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
            PickerAction::Changed => {
                if let Some(PickerValue::Theme(setting)) =
                    self.picker.as_ref().and_then(Picker::focused_value)
                {
                    self.theme = setting.resolve(self.appearance);
                }
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
            PickerAction::Close => {
                self.theme = self.theme_setting.resolve(self.appearance);
                if std::mem::take(&mut self.return_to_settings) {
                    self.open_settings_picker();
                } else {
                    self.picker = None;
                }
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
                    PickerValue::Theme(_) | PickerValue::OpenThemePicker => {}
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
            PickerValue::Theme(setting) => self.select_theme(setting),
            PickerValue::OpenThemePicker => {
                self.open_theme_picker(true);
                Update {
                    commands: Vec::new(),
                    visible_changed: true,
                    quit: false,
                }
            }
        }
    }

    fn open_session_tree(&mut self, initial_filter: &str) -> Update {
        self.next_tree_revision = self
            .next_tree_revision
            .checked_add(1)
            .expect("tree revision overflowed");
        let revision = self.next_tree_revision;
        self.tree_navigator = Some(TreeNavigator::new(self.theme, revision, initial_filter));
        Update {
            commands: vec![RuntimeCommand::RequestTree { revision }],
            visible_changed: true,
            quit: false,
        }
    }

    fn open_model_picker(&mut self, filter: &str) {
        let favourites = self
            .settings
            .as_ref()
            .and_then(SettingsContext::favourites)
            .unwrap_or(&[]);
        let items = favourites
            .iter()
            .map(|model| ("Favourites", *model))
            .chain(
                CATALOGUE
                    .iter()
                    .map(|model| (model.provider().display_name(), *model)),
            )
            .filter(|(_, model)| self.providers.contains(&model.provider()))
            .map(|(group, model)| PickerItem {
                group,
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

    fn open_settings_picker(&mut self) {
        let label = match self.theme_setting {
            ThemeSetting::Dark => "Theme  dark",
            ThemeSetting::Light => "Theme  light",
            ThemeSetting::Automatic => "Theme  automatic",
        };
        self.picker = Some(Picker::new(
            "Settings",
            "No matching settings",
            [PickerItem {
                group: "Settings",
                label,
                value: PickerValue::OpenThemePicker,
            }],
            PickerValue::OpenThemePicker,
        ));
    }

    fn open_theme_picker(&mut self, return_to_settings: bool) {
        self.return_to_settings = return_to_settings;
        let automatic_label = match self.appearance {
            Appearance::Dark => "automatic (currently dark)",
            Appearance::Light => "automatic (currently light)",
        };
        let items = [
            PickerItem {
                group: "Theme",
                label: automatic_label,
                value: PickerValue::Theme(ThemeSetting::Automatic),
            },
            PickerItem {
                group: "Theme",
                label: "dark",
                value: PickerValue::Theme(ThemeSetting::Dark),
            },
            PickerItem {
                group: "Theme",
                label: "light",
                value: PickerValue::Theme(ThemeSetting::Light),
            },
        ];
        self.picker = Some(Picker::new(
            "Theme",
            "No matching themes",
            items,
            PickerValue::Theme(self.theme_setting),
        ));
    }

    fn select_theme(&mut self, setting: ThemeSetting) -> Update {
        self.theme_setting = setting;
        self.theme = setting.resolve(self.appearance);
        self.save_user_default(|settings| settings.save_theme(setting));
        if std::mem::take(&mut self.return_to_settings) {
            self.open_settings_picker();
        }
        Update {
            commands: Vec::new(),
            visible_changed: true,
            quit: false,
        }
    }

    fn select_model(&mut self, model: &'static ModelConfiguration) -> Update {
        let provider = model.provider();
        if !self.providers.contains(&provider) {
            self.set_notice(format!(
                "Cannot select {}; {} is not set.",
                model.id,
                provider.api_key_variable()
            ));
            return Update {
                commands: Vec::new(),
                visible_changed: true,
                quit: false,
            };
        }
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

    fn settings_pending(&self) -> bool {
        self.preview.is_some()
            || self
                .pending_settings
                .values()
                .any(|selection| *selection == self.settings_selection)
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
        self.pending_settings
            .retain(|_, pending| pending.model() != selection.model());
        self.pending_settings.insert(revision, selection);
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
        self.next_name_revision = self
            .next_name_revision
            .checked_add(1)
            .expect("name revision overflowed");
        let revision = self.next_name_revision;
        self.pending_name = Some((revision, name.clone()));
        Update {
            commands: vec![RuntimeCommand::SetSessionName { name, revision }],
            visible_changed: true,
            quit: false,
        }
    }

    fn title(&self) -> String {
        let folder = self.cwd_basename().unwrap_or_else(|| "kiki".to_owned());
        match &self.session_name {
            Some(name) => format!("✷ {name} - {folder}"),
            None => format!("✷ {folder}"),
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
        update.visible_changed |= self.tick_edge_scroll(now)
            | self.tick_flash(now)
            | self.tick_throbber(now)
            | self.subagent_monitor.tick(now);
        update
    }

    fn tick_throbber(&mut self, now: Instant) -> bool {
        let Some(next_tick) = self.throbber_next_tick else {
            return false;
        };
        if now < next_tick {
            return false;
        }
        let interval = if self.transcript.is_generating() {
            self.generation_throbber.tick();
            self.generation_throbber.interval()
        } else if self.transcript.is_compacting() {
            self.compaction_throbber.tick();
            self.compaction_throbber.interval()
        } else {
            self.throbber_next_tick = None;
            return false;
        };
        let scheduled = next_tick + interval;
        self.throbber_next_tick = Some(if scheduled > now {
            scheduled
        } else {
            now + interval
        });
        true
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
        self.invalidate_selection_with_status(false)
    }

    fn invalidate_layout_selection(&mut self) -> bool {
        self.invalidate_selection_with_status(true)
    }

    fn invalidate_selection_with_status(&mut self, keep_status: bool) -> bool {
        let preserve_status = keep_status && matches!(&self.notice, Some(Notice::Status(_)));
        let visible_changed = self.prompt_selection.is_some()
            || self.selection.is_some()
            || (!preserve_status && self.notice.is_some())
            || self.flash.is_some();
        self.selection = None;
        self.drag = None;
        self.prompt_drag = None;
        self.prompt_selection = None;
        self.pending_copy = false;
        if !preserve_status {
            self.notice = None;
        }
        self.flash = None;
        visible_changed
    }

    fn history_unavailable(&mut self) {
        self.history_viewport = None;
        self.invalidate_layout_selection();
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
        if self
            .ignored_progress_run_id
            .is_some_and(|run_id| runtime_event_run_id(&envelope.item) == Some(run_id))
        {
            return false;
        }
        self.handle_event_with_gap_at(envelope.item, envelope.gap_before, Instant::now())
    }

    fn handle_event_with_gap_at(
        &mut self,
        event: RuntimeEvent,
        gap_before: bool,
        now: Instant,
    ) -> bool {
        if let RuntimeEvent::RecordAppended {
            record:
                crate::runtime::StoredRecord {
                    record: crate::runtime::Record::UserRequest { request, .. },
                    ..
                },
            ..
        } = &event
            && self.pending_skill_draft.as_ref() == Some(request)
        {
            self.pending_skill_draft = None;
        }
        let skill_draft_changed = if matches!(
            &event,
            RuntimeEvent::SkillActivationRejected { .. }
                | RuntimeEvent::SubmissionRejectedBusy { .. }
        ) {
            self.restore_pending_skill_draft()
        } else {
            if matches!(
                &event,
                RuntimeEvent::SubmissionRejectedDisabled { .. }
                    | RuntimeEvent::RuntimeDisabled { .. }
                    | RuntimeEvent::RunSettled { .. }
            ) {
                self.pending_skill_draft = None;
            }
            false
        };
        let compact_draft_changed = if self.compact_acknowledgement_pending
            && let RuntimeEvent::CompactionAcknowledged { acknowledgement } = &event
        {
            self.compact_acknowledgement_pending = false;
            if matches!(
                acknowledgement,
                crate::runtime::CompactionAcknowledgement::Accepted
            ) {
                self.prompt.take();
                self.refresh_completion();
            }
            true
        } else {
            false
        };
        if let RuntimeEvent::RecordAppended { record, .. } = &event
            && self.correction_pending
            && matches!(&record.record, crate::runtime::Record::CancelledRun { .. })
        {
            self.correction_run_id = Some(record.run_id);
        }
        if let RuntimeEvent::RunSettled { run_id } = &event {
            self.foreground_cancellation_requested = false;
            self.compaction_cancellation_requested = false;
            self.submission_at = None;
            if self.correction_pending
                && self
                    .correction_run_id
                    .is_none_or(|correction_run_id| correction_run_id == *run_id)
            {
                self.correction_pending = false;
                self.correction_run_id = None;
            }
        }
        if matches!(
            &event,
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(_),
            } | RuntimeEvent::SubmissionRejectedDisabled { .. }
                | RuntimeEvent::RuntimeDisabled { .. }
                | RuntimeEvent::CompactionAcknowledged {
                    acknowledgement: crate::runtime::CompactionAcknowledgement::Disabled { .. }
                }
        ) {
            self.compaction_cancellation_requested = false;
            self.submission_at = None;
            self.correction_pending = false;
            self.correction_run_id = None;
        }
        if matches!(
            &event,
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: None,
            }
        ) {
            self.submission_at = None;
        }
        if let RuntimeEvent::SkillActivationRejected { reason } = &event {
            self.submission_at = None;
            self.set_notice(format!(
                "Skill activation failed: {}",
                normalise_single_line(reason)
            ));
        }
        if matches!(
            &event,
            RuntimeEvent::CompactionLifecycle {
                lifecycle: crate::runtime::CompactionLifecycle::NoOp
            }
        ) {
            self.set_notice("Nothing to compact.".to_owned());
        }
        if matches!(
            &event,
            RuntimeEvent::CompactionLifecycle {
                lifecycle: crate::runtime::CompactionLifecycle::NoOp
                    | crate::runtime::CompactionLifecycle::Settled(_),
            }
        ) {
            self.compaction_cancellation_requested = false;
            if !self.transcript.has_active_run() {
                self.foreground_cancellation_requested = false;
                self.submission_at = None;
            }
        }
        if let RuntimeEvent::CorrectionCompleted { run_id, outcome } = &event
            && self.correction_pending
            && self.correction_run_id == Some(*run_id)
        {
            return self.handle_correction_completed(*run_id, outcome);
        }
        if let RuntimeEvent::TreeCatalogue { revision, outcome } = &event {
            return self.handle_tree_catalogue(*revision, outcome);
        }
        if let RuntimeEvent::NavigationCompleted { outcome } = &event
            && self.manual_navigation_pending
        {
            return self.handle_navigation_completed(outcome);
        }
        let metadata_changed = match &event {
            RuntimeEvent::SettingsAccepted { revision } => self.reconcile_settings(*revision),
            RuntimeEvent::NameAccepted { revision, name } => self.reconcile_name(*revision, name),
            _ => false,
        };
        let was_generating = self.transcript.is_generating();
        let was_compacting = self.transcript.is_compacting();
        let monitor_changed = self.subagent_monitor.apply(&event, now);
        let transcript_changed = self.transcript.apply(event, gap_before);
        let is_generating = self.transcript.is_generating();
        let is_compacting = self.transcript.is_compacting();
        if !was_generating && is_generating {
            self.generation_throbber.reset();
            self.throbber_next_tick = Some(now + self.generation_throbber.interval());
        } else if !was_compacting && is_compacting {
            self.compaction_throbber.reset();
            self.throbber_next_tick = Some(now + self.compaction_throbber.interval());
        } else if (was_generating || was_compacting) && !is_generating && !is_compacting {
            self.throbber_next_tick = None;
        }
        if transcript_changed {
            self.invalidate_layout_selection();
        }
        skill_draft_changed
            || compact_draft_changed
            || metadata_changed
            || monitor_changed
            || transcript_changed
    }

    fn restore_pending_skill_draft(&mut self) -> bool {
        let Some(draft) = self.pending_skill_draft.take() else {
            return false;
        };
        if self.prompt.is_empty() {
            let changed = self.prompt.insert_paste(&draft);
            if changed {
                self.refresh_completion();
            }
            changed
        } else {
            false
        }
    }

    fn handle_tree_catalogue(&mut self, revision: u64, outcome: &TreeRequestOutcome) -> bool {
        let Some(navigator) = &mut self.tree_navigator else {
            return false;
        };
        match outcome {
            TreeRequestOutcome::Catalogue(catalogue) => {
                navigator.install_catalogue(revision, catalogue)
            }
            TreeRequestOutcome::RejectedBusy => {
                if !navigator.reject_catalogue(revision) {
                    return false;
                }
                self.tree_navigator = None;
                self.set_notice(TREE_BUSY_NOTICE.to_owned());
                true
            }
            TreeRequestOutcome::RejectedReadOnly => {
                if !navigator.reject_catalogue(revision) {
                    return false;
                }
                self.tree_navigator = None;
                self.set_notice(TREE_READ_ONLY_NOTICE.to_owned());
                true
            }
            TreeRequestOutcome::Failed => {
                if !navigator.reject_catalogue(revision) {
                    return false;
                }
                self.tree_navigator = None;
                self.set_notice(SESSION_RESTORE_FAILED_NOTICE.to_owned());
                true
            }
        }
    }

    fn handle_correction_completed(&mut self, run_id: RunId, outcome: &CorrectionOutcome) -> bool {
        let notice_cleared = self.clear_notice();
        let changed = match outcome {
            CorrectionOutcome::Replaced {
                state,
                request_prefill,
            } => {
                self.replace_restored_state(state, request_prefill);
                self.ignored_progress_run_id = Some(run_id);
                true
            }
            CorrectionOutcome::SkippedBackground => {
                self.set_notice(
                    "Session was not restored because background work is active.".to_owned(),
                );
                true
            }
            CorrectionOutcome::Failed => {
                self.set_notice(SESSION_RESTORE_FAILED_NOTICE.to_owned());
                true
            }
        };
        changed | notice_cleared
    }

    fn handle_navigation_completed(&mut self, outcome: &NavigationOutcome) -> bool {
        let notice_cleared = self.clear_notice();
        self.manual_navigation_pending = false;
        match outcome {
            NavigationOutcome::Replaced {
                state,
                request_prefill,
            } => {
                self.replace_restored_state(state, request_prefill);
                true
            }
            NavigationOutcome::Unchanged { request_prefill } => {
                notice_cleared | self.apply_request_prefill(request_prefill)
            }
            NavigationOutcome::RejectedBusy => {
                self.set_notice(TREE_BUSY_NOTICE.to_owned());
                true
            }
            NavigationOutcome::RejectedReadOnly => {
                self.set_notice(TREE_READ_ONLY_NOTICE.to_owned());
                true
            }
            NavigationOutcome::Failed => {
                self.set_notice(SESSION_RESTORE_FAILED_NOTICE.to_owned());
                true
            }
        }
    }

    fn replace_restored_state(
        &mut self,
        state: &RuntimeInitialState,
        request_prefill: &Option<String>,
    ) {
        let mut transcript = self.transcript.restored_replacement(state);
        transcript.append_restoration_card();
        transcript.append_metadata(skill_diagnostic_report(&self.skill_catalog));
        self.transcript = transcript;
        self.subagent_monitor = subagents::SubagentMonitor::restored(&state.timeline);
        self.history_position = HistoryPosition::FollowingBottom;
        self.history_height = 0;
        self.history_rows = 0;
        self.history_viewport = None;
        self.selection = None;
        self.drag = None;
        self.pending_copy = false;
        self.pending_copy_text = None;
        self.flash = None;
        self.apply_request_prefill(request_prefill);
    }

    fn apply_request_prefill(&mut self, request_prefill: &Option<String>) -> bool {
        if self.prompt.is_empty()
            && let Some(request) = request_prefill
        {
            let changed = self.prompt.insert_paste(request);
            return changed | self.refresh_completion();
        }
        false
    }

    fn reconcile_settings(&mut self, revision: u64) -> bool {
        let Some(selection) = self.pending_settings.remove(&revision) else {
            return false;
        };
        self.session_thinking
            .insert(selection.model().id, selection.thinking());
        if self
            .preview
            .is_some_and(|preview| preview.sent_revision == Some(revision))
        {
            self.preview = None;
        }
        true
    }

    fn reconcile_name(&mut self, revision: u64, name: &str) -> bool {
        let Some((pending_revision, pending_name)) = &self.pending_name else {
            return false;
        };
        if *pending_revision != revision || pending_name != name {
            return false;
        }
        self.session_name = Some(name.to_owned());
        self.pending_name = None;
        true
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

pub(crate) struct TuiStartup {
    snapshot: StartupSnapshot,
    appearance: Appearance,
    skills_catalog: Arc<SkillCatalog>,
}

impl TuiStartup {
    pub(crate) fn new(
        snapshot: StartupSnapshot,
        skills_catalog: Arc<SkillCatalog>,
        appearance: Appearance,
    ) -> Self {
        Self {
            snapshot,
            appearance,
            skills_catalog,
        }
    }
}

pub(crate) fn run(
    startup: TuiStartup,
    providers: Vec<Provider>,
    commands: mpsc::Sender<RuntimeCommand>,
    events: mpsc::UnboundedReceiver<RuntimeEvent>,
    progress: RuntimeProgressReceiver,
    trace: Trace,
) -> Result<(), TuiError> {
    run_restored(
        startup,
        RuntimeInitialState::empty(),
        providers,
        commands,
        events,
        progress,
        trace,
    )
}

pub(crate) fn run_restored(
    startup: TuiStartup,
    initial: RuntimeInitialState,
    providers: Vec<Provider>,
    commands: mpsc::Sender<RuntimeCommand>,
    mut events: mpsc::UnboundedReceiver<RuntimeEvent>,
    mut progress: RuntimeProgressReceiver,
    trace: Trace,
) -> Result<(), TuiError> {
    let mut app = App::restored(startup.snapshot, &initial)
        .with_appearance(startup.appearance)
        .with_skill_catalog(startup.skills_catalog);
    app.providers = providers;
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

pub(crate) fn run_configuration(
    startup: StartupSnapshot,
    appearance: Appearance,
) -> Result<(), TuiError> {
    let mut app = App::configuration(startup).with_appearance(appearance);
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

pub(crate) fn run_project_skill_trust(
    scope: &Path,
    setting: ThemeSetting,
    appearance: Appearance,
    mut save: impl FnMut() -> Result<(), String>,
) -> Result<bool, TuiError> {
    let mut input = CrosstermEvents;
    let mut trusted = false;
    with_terminal(
        || ratatui::try_init().map_err(|error| TuiError::Terminal(error.to_string())),
        ratatui::restore,
        |terminal| {
            trusted = run_project_skill_trust_loop(
                terminal,
                &mut input,
                scope,
                setting.resolve(appearance),
                &mut save,
            )?;
            Ok(())
        },
    )?;
    Ok(trusted)
}

fn run_project_skill_trust_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    input: &mut impl EventSource,
    scope: &Path,
    theme: &Theme,
    mut save: impl FnMut() -> Result<(), String>,
) -> Result<bool, TuiError> {
    let mut trust_selected = false;
    let mut save_error = None;
    let mut path_scroll = 0usize;
    let mut visible_path_rows = None;
    let mut dirty = true;
    loop {
        if dirty {
            terminal
                .draw(|frame| {
                    visible_path_rows = render_project_skill_trust(
                        theme,
                        frame,
                        scope,
                        trust_selected,
                        save_error.as_deref(),
                        &mut path_scroll,
                    );
                })
                .map_err(|error| TuiError::Terminal(error.to_string()))?;
            dirty = false;
        }
        if !input.poll(POLL_INTERVAL).map_err(TuiError::Input)? {
            continue;
        }
        match input.read().map_err(TuiError::Input)? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL {
                    return Ok(false);
                }
                if key.modifiers != KeyModifiers::NONE {
                    continue;
                }
                match key.code {
                    KeyCode::Enter if trust_selected && visible_path_rows.is_some() => match save()
                    {
                        Ok(()) => return Ok(true),
                        Err(error) => {
                            save_error = Some(error);
                            trust_selected = false;
                            dirty = true;
                        }
                    },
                    KeyCode::Enter | KeyCode::Esc => return Ok(false),
                    KeyCode::Left | KeyCode::Char('s') => {
                        trust_selected = false;
                        dirty = true;
                    }
                    KeyCode::Right | KeyCode::Char('t') => {
                        trust_selected = true;
                        dirty = true;
                    }
                    KeyCode::Tab => {
                        trust_selected = !trust_selected;
                        dirty = true;
                    }
                    KeyCode::Up => {
                        path_scroll = path_scroll.saturating_sub(1);
                        dirty = true;
                    }
                    KeyCode::Down => {
                        path_scroll = path_scroll.saturating_add(1);
                        dirty = true;
                    }
                    KeyCode::PageUp => {
                        path_scroll = path_scroll.saturating_sub(visible_path_rows.unwrap_or(1));
                        dirty = true;
                    }
                    KeyCode::PageDown => {
                        path_scroll = path_scroll.saturating_add(visible_path_rows.unwrap_or(1));
                        dirty = true;
                    }
                    _ => {}
                }
            }
            Event::Resize(_, _) => dirty = true,
            _ => {}
        }
    }
}

fn render_project_skill_trust(
    theme: &Theme,
    frame: &mut Frame,
    scope: &Path,
    trust_selected: bool,
    save_error: Option<&str>,
    path_scroll: &mut usize,
) -> Option<usize> {
    let area = frame.area();
    if area.is_empty() {
        return None;
    }
    frame.buffer_mut().set_style(area, page_style(theme));
    let width = area.width.min(100);
    let height = area.height.min(12);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title("Project skills");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.is_empty() {
        return None;
    }
    let error_rows = if save_error.is_some() { 2 } else { 0 };
    let minimum_height = if save_error.is_some() { 9 } else { 8 };
    if inner.width < 46 || inner.height < minimum_height {
        frame.render_widget(
            Paragraph::new("Resize terminal to inspect the full project scope."),
            inner,
        );
        return None;
    }
    let path = escaped_project_skill_scope(scope);
    let path_lines = path
        .as_bytes()
        .chunks(inner.width as usize)
        .map(|bytes| std::str::from_utf8(bytes).expect("escaped scope is ASCII"))
        .collect::<Vec<_>>();
    let visible_rows = (inner.height as usize - 6 - error_rows as usize).min(3);
    let max_scroll = path_lines.len().saturating_sub(visible_rows);
    *path_scroll = (*path_scroll).min(max_scroll);
    let visible_end = (*path_scroll + visible_rows).min(path_lines.len());
    let choices = if trust_selected {
        "Skip    [Trust]"
    } else {
        "[Skip]    Trust"
    };
    let mut lines = vec![
        Line::raw("This project has skills with assistant instructions."),
        Line::raw("Project scope (escaped bytes):"),
    ];
    for line in &path_lines[*path_scroll..visible_end] {
        lines.push(Line::raw(*line));
    }
    lines.extend([
        Line::raw(format!(
            "Path lines {}-{}/{} | Up/Down/Page",
            *path_scroll + 1,
            visible_end,
            path_lines.len()
        )),
        Line::raw("Trust this project's skills?"),
        Line::raw(choices),
        Line::raw("Left/Right choice | Enter confirm | Esc skip"),
    ]);
    let content_height = (6 + visible_rows) as u16;
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(inner.x, inner.y, inner.width, content_height),
    );
    if let Some(error) = save_error {
        let error_area = Rect::new(inner.x, inner.y + content_height, inner.width, error_rows);
        frame.render_widget(
            Paragraph::new(format!(
                "Trust was not saved: {}",
                normalise_terminal_text(error)
            ))
            .wrap(Wrap { trim: false }),
            error_area,
        );
    }
    Some(visible_rows)
}

fn escaped_project_skill_scope(scope: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    escaped_terminal_bytes(scope.as_os_str().as_bytes(), usize::MAX)
}

const SKILL_DIAGNOSTIC_LINE_BYTES: usize = 512;

fn skill_diagnostic_report(catalog: &SkillCatalog) -> Vec<String> {
    let diagnostics = catalog.diagnostics();
    if diagnostics.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "Skill discovery diagnostics ({}):",
        diagnostics.len()
    )];
    for diagnostic in diagnostics {
        lines.push(format_skill_diagnostic(diagnostic));
    }
    lines
}

fn format_skill_diagnostic(diagnostic: &SkillDiagnostic) -> String {
    let path = escaped_diagnostic_path(&diagnostic.path);
    let reason = escaped_terminal_bytes(diagnostic.reason.as_bytes(), 176);
    let line = if let Some(name) = &diagnostic.name {
        let name = escaped_terminal_bytes(name.as_bytes(), 80);
        format!("- {path} [skill {name}]: {reason}")
    } else {
        format!("- {path}: {reason}")
    };
    debug_assert!(line.len() <= SKILL_DIAGNOSTIC_LINE_BYTES);
    line
}

fn escaped_diagnostic_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    const PATH_BYTES: usize = 224;
    let bytes = path.as_os_str().as_bytes();
    let full = escaped_terminal_bytes(bytes, usize::MAX);
    if full.len() <= PATH_BYTES {
        return full;
    }
    let hash = bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    let marker = format!("...#{hash:016X}:");
    let mut suffix = String::new();
    for byte in bytes.iter().rev() {
        let encoded = escaped_terminal_bytes(std::slice::from_ref(byte), usize::MAX);
        if marker.len() + suffix.len() + encoded.len() > PATH_BYTES {
            break;
        }
        suffix.insert_str(0, &encoded);
    }
    format!("{marker}{suffix}")
}

fn escaped_terminal_bytes(bytes: &[u8], limit: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut escaped = String::new();
    if limit < 3 {
        return escaped;
    }
    for byte in bytes {
        let encoded = match byte {
            b'\\' => "\\\\".to_owned(),
            0x20..=0x7e => (*byte as char).to_string(),
            _ => format!(
                "\\x{}{}",
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 0x0f) as usize] as char
            ),
        };
        if escaped.len() + encoded.len() > limit - 3 {
            escaped.push_str("...");
            break;
        }
        escaped.push_str(&encoded);
    }
    escaped
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
                    app.invalidate_layout_selection();
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
                    app.invalidate_layout_selection();
                    dirty = true;
                }
                Event::Paste(text) => dirty |= app.handle_paste(&text).visible_changed,
                _ => {}
            }
        }

        loop {
            match events.try_recv() {
                Ok(event) => {
                    let previous_title = app.title();
                    dirty |= app.handle_event(event);
                    if previous_title != app.title() {
                        dirty |= publish_title(app, titles);
                    }
                }
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
    app.prompt_view = None;
    if let Some(stale_overlay) = app.slash_overlay.take() {
        frame.render_widget(Clear, stale_overlay.intersection(area));
    }
    frame.buffer_mut().set_style(area, page_style(app.theme));
    if area.width == 0 || area.height == 0 {
        let _ = app.subagent_monitor.layout(app.theme, area.width, 0, now);
        app.history_unavailable();
        return;
    }

    let configuration_only = app.is_configuration_only();
    let tree_open = app.tree_navigator.is_some();
    if !tree_open {
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
    }

    if configuration_only {
        let _ = app.subagent_monitor.layout(app.theme, area.width, 0, now);
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

    let bottom_rule = Rect::new(
        area.x,
        area.y.saturating_add(area.height.saturating_sub(1)),
        area.width,
        1,
    );
    if tree_open {
        let notice = app.notice().map(str::to_owned);
        let content_height = app
            .tree_navigator
            .as_ref()
            .map_or(0, TreeNavigator::content_height)
            .min(area.height / 2);
        let dock_height = content_height
            .saturating_add(3)
            .min(area.height.saturating_sub(1));
        let dock_top = bottom_rule.y.saturating_sub(dock_height);
        let monitor = app.subagent_monitor.layout(
            app.theme,
            area.width,
            dock_top.saturating_sub(area.y).saturating_sub(1),
            now,
        );
        let monitor_top = dock_top.saturating_sub(monitor.height());
        let layout = render_history(
            frame,
            app,
            Rect::new(
                area.x,
                area.y,
                area.width,
                monitor_top.saturating_sub(area.y),
            ),
        );
        monitor.render(
            frame,
            Rect::new(area.x, monitor_top, area.width, monitor.height()),
        );
        let rule_style = thinking_rule_style(app.theme, app.settings_selection);
        frame.render_widget(
            Paragraph::new(layout.footer_rule(app.theme, bottom_rule.width)).style(rule_style),
            bottom_rule,
        );
        if let Some(navigator) = &mut app.tree_navigator {
            navigator.render(
                frame,
                Rect::new(area.x, dock_top, area.width, dock_height),
                notice.as_deref(),
            );
        }
        return;
    }

    if area.height == 1 {
        let _ = app.subagent_monitor.layout(app.theme, area.width, 0, now);
        app.history_unavailable();
        return;
    }

    if area.height <= 3 {
        let _ = app.subagent_monitor.layout(app.theme, area.width, 0, now);
        let rule_style = thinking_rule_style(app.theme, app.settings_selection);
        if area.height == 3 {
            render_rule(
                frame,
                Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
                rule_style,
            );
        }
        let layout = match app.footer_context() {
            Some(context) => {
                app.transcript
                    .layout_with_footer(app.theme, bottom_rule.width, context)
            }
            None => app.transcript.layout(app.theme, bottom_rule.width),
        };
        frame.render_widget(
            Paragraph::new(layout.footer_rule(app.theme, bottom_rule.width)).style(rule_style),
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
    let throbber_active = app.transcript.is_generating() || app.transcript.is_compacting();
    let throbber_height = if !throbber_active || history_height < 2 {
        0
    } else if history_height == THROBBER_ROWS {
        1
    } else {
        THROBBER_ROWS
    };
    let pending_card = app
        .transcript
        .pending_model_change(app.settings_selection)
        .map(|selection| {
            model_change_card(app.theme, selection, app.settings_pending(), area.width)
        })
        .filter(|card| card.height() < history_height.saturating_sub(throbber_height));
    let card_height = pending_card.as_ref().map_or(0, ModelChangeCard::height);
    let monitor = app.subagent_monitor.layout(
        app.theme,
        area.width,
        history_height
            .saturating_sub(throbber_height)
            .saturating_sub(card_height)
            .saturating_sub(1),
        now,
    );
    let throbber_top = prompt_top.saturating_sub(throbber_height);
    let card_top = throbber_top.saturating_sub(card_height);
    let monitor_top = card_top.saturating_sub(monitor.height());
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
        throbber_top.saturating_sub(area.y.saturating_add(1)),
    );
    let layout = render_history(frame, app, history);
    monitor.render(
        frame,
        Rect::new(area.x, monitor_top, area.width, monitor.height()),
    );
    if let Some(card) = &pending_card {
        card.render(
            Rect::new(area.x, card_top, area.width, card_height),
            frame.buffer_mut(),
        );
    }
    if throbber_height > 0 {
        let throbber_frame = if app.transcript.is_generating() {
            app.generation_throbber.current_frame()
        } else {
            app.compaction_throbber.current_frame()
        };
        frame.render_widget(
            Paragraph::new(Line::styled(
                throbber_frame,
                Style::new().add_modifier(Modifier::BOLD),
            )),
            Rect::new(
                area.x.saturating_add(THROBBER_LEFT_PADDING),
                prompt_top.saturating_sub(1),
                area.width.saturating_sub(THROBBER_LEFT_PADDING),
                1,
            ),
        );
    }
    render_slash_overlay(frame, app, completion_area);
    let rule_style = thinking_rule_style(app.theme, app.settings_selection);
    render_rule(
        frame,
        Rect::new(area.x, prompt_top, area.width, 1),
        rule_style,
    );
    render_prompt(
        app.theme,
        frame,
        &prompt,
        content,
        !app.is_submission_disabled()
            && !app.compact_acknowledgement_pending
            && app.picker.is_none()
            && app.tree_navigator.is_none(),
        slash::recognised_token_range(app.prompt.text()),
        app.prompt_selection
            .as_ref()
            .filter(|_| app.flash.as_ref().is_none_or(|flash| flash.highlighted)),
    );
    app.prompt_view = Some((
        Rect::new(
            content.x.saturating_add(PROMPT_GUTTER_WIDTH),
            content.y,
            content.width.saturating_sub(PROMPT_GUTTER_WIDTH),
            content.height,
        ),
        prompt,
    ));
    frame.render_widget(
        Paragraph::new(layout.footer_rule(app.theme, bottom_rule.width)).style(rule_style),
        bottom_rule,
    );
    if let Some(picker) = &mut app.picker {
        picker.render(app.theme, frame, area);
    }
}

fn render_history(frame: &mut Frame, app: &mut App, viewport: Rect) -> TranscriptLayout {
    let layout = match app.footer_context() {
        Some(context) => app
            .transcript
            .layout_with_footer(app.theme, viewport.width, context),
        None => app.transcript.layout(app.theme, viewport.width),
    };
    if viewport.is_empty() {
        app.history_unavailable();
        return layout;
    }
    if app
        .history_viewport
        .is_some_and(|history| history.area != viewport)
    {
        app.invalidate_layout_selection();
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
    if app.is_submission_disabled() || app.picker.is_some() || app.tree_navigator.is_some() {
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
    frame.buffer_mut().set_style(overlay, page_style(app.theme));
    let lines = rows.into_iter().map(|row| {
        let text = format!("{} {}", row.token, row.purpose);
        let padding = usize::from(width).saturating_sub(Line::raw(&text).width());
        let style = if row.selected {
            Style::new().fg(app.theme.text).bg(app.theme.selected_bg)
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
    theme: &Theme,
    frame: &mut Frame,
    prompt: &PromptLayout,
    content: Rect,
    cursor_visible: bool,
    recognised_token: Option<Range<usize>>,
    selection: Option<&Range<usize>>,
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
        .map(|row| prompt_line(theme, row, recognised_token.as_ref()))
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
    if let Some(selection) = selection {
        for (row_index, row) in prompt.rows().iter().enumerate() {
            let mut column = 0;
            for (offset, grapheme) in row.text().grapheme_indices(true) {
                let width = Line::raw(grapheme).width();
                let start = row.source().start + offset;
                if start < selection.end && start + grapheme.len() > selection.start {
                    for cell_column in column..column + width {
                        if cell_column < usize::from(text_width)
                            && row_index < usize::from(content.height)
                        {
                            let cell = &mut frame.buffer_mut()[(
                                content.x + PROMPT_GUTTER_WIDTH + cell_column as u16,
                                content.y + row_index as u16,
                            )];
                            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                        }
                    }
                }
                column += width;
            }
        }
    }
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

fn prompt_line<'a>(
    theme: &Theme,
    row: &'a PromptRow,
    recognised_token: Option<&Range<usize>>,
) -> Line<'a> {
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
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::raw(&text[end..]),
    ])
}

fn thinking_rule_style(theme: &Theme, selection: ModelSelection) -> Style {
    Style::new().fg(thinking_rule_colour(
        theme,
        selection.model().thinking_levels,
        selection.thinking(),
    ))
}

fn thinking_rule_colour(theme: &Theme, levels: &[&str], selected: &str) -> Color {
    let colours = [
        theme.thinking_off,
        theme.thinking_minimal,
        theme.thinking_low,
        theme.thinking_medium,
        theme.thinking_high,
        theme.thinking_xhigh,
        theme.thinking_max,
    ];
    let index = levels
        .iter()
        .position(|level| *level == selected)
        .expect("selection thinking must be supported by its model");
    let denominator = levels.len().saturating_sub(1);
    if denominator == 0 {
        return colours[0];
    }
    let position = index * (colours.len() - 1);
    let lower = position / denominator;
    let remainder = position % denominator;
    let upper = (lower + usize::from(remainder > 0)).min(colours.len() - 1);
    interpolate_colour(colours[lower], colours[upper], remainder, denominator)
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
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::io;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::Path;
    use std::sync::Arc;
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
    use ratatui::style::Modifier;

    use super::{
        App, DocumentPosition, HistoryPosition, Selection, TitlePublisher, compaction_throbber,
        generation_throbber,
    };
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, Observation, PythonError, ToolCall, ToolCallSummary,
        ToolCallToken, ToolName,
    };
    use crate::clipboard::{ClipboardError, ClipboardPort};
    use crate::config::models::{ASTRA, GPT6_1_SOL, GPT6_SOL, OPUS_5_5, Provider, SOL, TERRA};
    use crate::config::{ModelSelection, SessionPreferences};
    use crate::model::{AttemptAccounting, ModelProgress, ModelUsage};
    use crate::prompt_editor::{MAX_PROMPT_BYTES, PromptEdit, PromptEditor};
    use crate::runtime::{
        CompactionAcknowledgement, CompactionLifecycle, CompactionSettlement, CorrectionOutcome,
        ModelStepId, NavigationOutcome, ProgressEnvelope, Record, RestoredTimelineItem, RunId,
        RuntimeCommand, RuntimeDisabledReason, RuntimeEvent, RuntimeInitialState, StoredRecord,
        TerminalDetails, TreeRequestOutcome,
    };
    use crate::session::{
        SessionCatalogue, SessionCatalogueEntry, SessionCatalogueKind, SessionEntryId,
        SessionMetadata,
    };
    use crate::skills::{ProjectScope, SkillCatalog, SkillDiagnostic, write_skill};
    use crate::subagents::{TaskRecord, TaskState};
    use crate::telemetry::{Trace, TraceOwner};
    use crate::theme::{DARK, DISTINCT_TEST_THEME};
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn project_skill_trust_popup_defaults_to_skip_and_names_canonical_scope() {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut input = FakeInput::new([Some(Event::Key(key(KeyCode::Enter)))]);
        let saved = Cell::new(false);
        let trusted = super::run_project_skill_trust_loop(
            &mut terminal,
            &mut input,
            Path::new("/canonical/repository"),
            &DARK,
            || {
                saved.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(!trusted);
        assert!(!saved.get());
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("/canonical/repository"));
        assert!(screen.contains("[Skip]"));
        assert!(screen.contains("Project scope (escaped bytes):"));
        assert!(!screen.contains("Repository:"));
    }

    #[test]
    fn project_skill_trust_popup_ignores_release_and_modified_keys() {
        let unsafe_keys = [
            KeyEvent::new_with_kind(KeyCode::Right, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
        ];
        for unsafe_key in unsafe_keys {
            let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
            let mut input = FakeInput::new([
                Some(Event::Key(unsafe_key)),
                Some(Event::Key(key(KeyCode::Enter))),
            ]);
            let trusted = super::run_project_skill_trust_loop(
                &mut terminal,
                &mut input,
                Path::new("/canonical/project"),
                &DARK,
                || panic!("modified or release key must not select Trust"),
            )
            .unwrap();
            assert!(!trusted);
        }

        let unsafe_enter_keys = [
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        ];
        for unsafe_key in unsafe_enter_keys {
            let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
            let mut input = FakeInput::new([
                Some(Event::Key(key(KeyCode::Right))),
                Some(Event::Key(unsafe_key)),
                Some(Event::Key(key(KeyCode::Esc))),
            ]);
            let trusted = super::run_project_skill_trust_loop(
                &mut terminal,
                &mut input,
                Path::new("/canonical/project"),
                &DARK,
                || panic!("modified or release Enter must not confirm Trust"),
            )
            .unwrap();
            assert!(!trusted);
        }
    }

    #[test]
    fn project_skill_trust_popup_escapes_control_and_bidi_path_bytes() {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut input = FakeInput::new([Some(Event::Key(key(KeyCode::Enter)))]);
        let trusted = super::run_project_skill_trust_loop(
            &mut terminal,
            &mut input,
            Path::new("/project/line\n\u{202e}tail"),
            &DARK,
            || panic!("default Skip must not save"),
        )
        .unwrap();
        assert!(!trusted);
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("/project/line\\x0A\\xE2\\x80\\xAEtail"));
        assert!(!screen.contains('\u{202e}'));
    }

    #[test]
    fn project_skill_trust_popup_scrolls_to_long_path_suffix() {
        let path = format!("/project/{}/VISIBLE-SUFFIX", "a".repeat(240));
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut input = FakeInput::new([Some(Event::Key(key(KeyCode::Enter)))]);
        super::run_project_skill_trust_loop(
            &mut terminal,
            &mut input,
            Path::new(&path),
            &DARK,
            || panic!("default Skip must not save"),
        )
        .unwrap();
        let initial = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!initial.contains("VISIBLE-SUFFIX"));

        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let mut input = FakeInput::new([
            Some(Event::Key(key(KeyCode::PageDown))),
            Some(Event::Key(key(KeyCode::PageDown))),
            Some(Event::Key(key(KeyCode::Enter))),
        ]);
        super::run_project_skill_trust_loop(
            &mut terminal,
            &mut input,
            Path::new(&path),
            &DARK,
            || panic!("default Skip must not save"),
        )
        .unwrap();
        let scrolled = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(scrolled.contains("VISIBLE-SUFFIX"));
        assert!(scrolled.contains("Path lines"));
    }

    #[test]
    fn project_skill_trust_popup_only_enables_after_successful_save() {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let mut input = FakeInput::new([
            Some(Event::Key(key(KeyCode::Right))),
            Some(Event::Key(key(KeyCode::Enter))),
        ]);
        let saved = Cell::new(0);
        let trusted = super::run_project_skill_trust_loop(
            &mut terminal,
            &mut input,
            Path::new("/canonical/repository"),
            &DARK,
            || {
                saved.set(saved.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert!(trusted);
        assert_eq!(saved.get(), 1);

        let mut terminal = Terminal::new(TestBackend::new(48, 11)).unwrap();
        let mut input = FakeInput::new([
            Some(Event::Key(key(KeyCode::Right))),
            Some(Event::Key(key(KeyCode::Enter))),
            Some(Event::Key(key(KeyCode::Enter))),
        ]);
        let saved = Cell::new(0);
        let trusted = super::run_project_skill_trust_loop(
            &mut terminal,
            &mut input,
            Path::new("/canonical/repository"),
            &DARK,
            || {
                saved.set(saved.get() + 1);
                Err("could not save project skill trust (PermissionDenied)".to_owned())
            },
        )
        .unwrap();
        assert!(!trusted);
        assert_eq!(saved.get(), 1);
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("PermissionDenied"));
    }

    #[test]
    fn project_skill_trust_popup_cancellation_skips_without_saving() {
        for cancel in [
            key(KeyCode::Esc),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
            let mut input = FakeInput::new([
                Some(Event::Key(key(KeyCode::Right))),
                Some(Event::Key(cancel)),
            ]);
            let trusted = super::run_project_skill_trust_loop(
                &mut terminal,
                &mut input,
                Path::new("/canonical/repository"),
                &DARK,
                || panic!("cancel must not save trust"),
            )
            .unwrap();
            assert!(!trusted);
        }
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

    fn tree_execution(commentary: &str) -> SessionCatalogueKind {
        SessionCatalogueKind::Execution {
            source: "pass".to_owned(),
            commentary: Some(commentary.to_owned()),
            execution: Box::new(Execution {
                stdout: observation(""),
                stderr: observation(""),
                outcome: ExecutionOutcome::Complete { result: None },
                tool_calls: Vec::new(),
                read_paths: Vec::new(),
                modified_paths: Vec::new(),
                changed_bindings: Vec::new(),
            }),
        }
    }

    fn session_tree_catalogue() -> SessionCatalogue {
        SessionCatalogue {
            active_entry: SessionEntryId(3),
            entries: vec![
                SessionCatalogueEntry {
                    entry_id: SessionEntryId(1),
                    parent_id: None,
                    revision: 1,
                    kind: SessionCatalogueKind::Start,
                },
                SessionCatalogueEntry {
                    entry_id: SessionEntryId(2),
                    parent_id: Some(SessionEntryId(1)),
                    revision: 2,
                    kind: SessionCatalogueKind::Request {
                        text: "authorise access".to_owned(),
                    },
                },
                SessionCatalogueEntry {
                    entry_id: SessionEntryId(3),
                    parent_id: Some(SessionEntryId(2)),
                    revision: 3,
                    kind: tree_execution("inspect credentials"),
                },
            ],
        }
    }

    fn long_session_tree_catalogue(request_count: u64) -> SessionCatalogue {
        SessionCatalogue {
            active_entry: SessionEntryId(1),
            entries: std::iter::once(SessionCatalogueEntry {
                entry_id: SessionEntryId(1),
                parent_id: None,
                revision: 1,
                kind: SessionCatalogueKind::Start,
            })
            .chain((1..=request_count).map(|request| SessionCatalogueEntry {
                entry_id: SessionEntryId(request + 1),
                parent_id: Some(SessionEntryId(request)),
                revision: request + 1,
                kind: SessionCatalogueKind::Request {
                    text: format!("entry {request}"),
                },
            }))
            .collect(),
        }
    }

    fn tree_request(app: &mut App, initial_filter: &str) -> u64 {
        let update = app.open_session_tree(initial_filter);
        let [RuntimeCommand::RequestTree { revision }] = update.commands.as_slice() else {
            panic!("expected tree request, got {:?}", update.commands);
        };
        *revision
    }

    fn load_tree(app: &mut App, revision: u64) {
        assert!(app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(session_tree_catalogue()),
        }));
    }

    fn select_tree_request(app: &mut App) {
        let revision = tree_request(app, "");
        load_tree(app, revision);
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Navigate {
                entry: SessionEntryId(2)
            }]
        );
        assert!(app.manual_navigation_pending);
    }

    fn restored_state(request: &str) -> RuntimeInitialState {
        RuntimeInitialState {
            metadata: None,
            timeline: vec![RestoredTimelineItem::Record(StoredRecord {
                run_id: RunId(4),
                record: Record::UserRequest {
                    request: request.to_owned(),
                    selection: None,
                },
            })],
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        }
    }

    fn submitted_prompt(app: &mut App) -> String {
        let update = app.handle_key(key(KeyCode::Enter));
        let Some(RuntimeCommand::Submit {
            request,
            activation: None,
        }) = update.commands.into_iter().next()
        else {
            panic!("prompt was not submitted")
        };
        request
    }

    fn enter_at(app: &mut App, prompt: &str, now: Instant) -> super::Update {
        set_prompt(app, prompt);
        app.handle_key_at(key(KeyCode::Enter), now)
    }

    fn armed_settled_escape(now: Instant) -> App {
        let mut app = App::default();
        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        assert_eq!(app.settled_escape_at, Some(now));
        app
    }

    fn assert_nonqualifying_escape(app: &mut App, now: Instant, expected: &[RuntimeCommand]) {
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(1))
                .commands,
            expected
        );
        assert_eq!(app.settled_escape_at, Some(now));
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
            let rendered = buffer_row(buffer, row);
            if let Some(column) = rendered.find(text) {
                return (u16::try_from(column).expect("test terminal column"), row);
            }
        }
        panic!("rendered text: {text}")
    }

    fn buffer_row(buffer: &Buffer, row: u16) -> String {
        (buffer.area.left()..buffer.area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
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
            epoch: 0,
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
                            selection: None,
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
                                read_paths: Vec::new(),
                                modified_paths: Vec::new(),
                                changed_bindings: Vec::new(),
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
                epoch: 0,
            },
            RuntimeEvent::ModelProgress {
                run_id: RunId(129),
                step_id: ModelStepId(1),
                epoch: 0,
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
                        selection: None,
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
            epoch: 0,
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
                    selection: None,
                },
            },
            progress_gap: false,
        });
        select_history_text(app, text, width, height)
    }

    fn select_history_text(app: &mut App, text: &str, width: u16, height: u16) -> Instant {
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

    fn completed_run_records(run_id: u64, selection: ModelSelection) -> [StoredRecord; 2] {
        [
            StoredRecord {
                run_id: RunId(run_id),
                record: Record::UserRequest {
                    request: format!("request {run_id}"),
                    selection: Some(selection),
                },
            },
            StoredRecord {
                run_id: RunId(run_id),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: "answer".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails::empty(),
                },
            },
        ]
    }

    fn branch_state(selections: &[ModelSelection]) -> RuntimeInitialState {
        RuntimeInitialState {
            metadata: None,
            timeline: selections
                .iter()
                .zip(1..)
                .flat_map(|(selection, run_id)| completed_run_records(run_id, *selection))
                .map(RestoredTimelineItem::Record)
                .collect(),
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        }
    }

    fn history_rows(app: &mut App) -> Vec<String> {
        history_viewport_rows(app)
            .into_iter()
            .filter(|row| !row.is_empty())
            .collect()
    }

    fn history_viewport_rows(app: &mut App) -> Vec<String> {
        let buffer = rendered_buffer(app, 80, 40);
        let area = app.history_viewport.expect("history viewport").area;
        (area.top()..area.bottom())
            .map(|row| {
                (area.left()..area.right())
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim()
                    .to_owned()
            })
            .collect()
    }

    fn append_completed_run(app: &mut App, run_id: u64, selection: ModelSelection) {
        for record in completed_run_records(run_id, selection) {
            app.handle_event(RuntimeEvent::RecordAppended {
                record,
                progress_gap: false,
            });
        }
    }

    fn rows_and_prompt_rule(app: &mut App) -> (Vec<String>, usize) {
        let rows = rendered(app, 80, 40)
            .lines()
            .map(|row| row.trim().to_owned())
            .collect::<Vec<_>>();
        let rule = rows
            .iter()
            .rposition(|row| *row == "─".repeat(80))
            .expect("prompt rule");
        (rows, rule)
    }

    fn history_bottom(app: &App) -> usize {
        usize::from(
            app.history_viewport
                .expect("history viewport")
                .area
                .bottom(),
        )
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
        let update = app.name_session("Refactor auth");
        let [RuntimeCommand::SetSessionName { revision, .. }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        let mut input = FakeInput::new([Some(quit_event())]);
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).expect("terminal");
        let (commands, _command_receiver) = mpsc::channel(8);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        event_sender
            .send(RuntimeEvent::NameAccepted {
                revision: *revision,
                name: "Refactor auth".to_owned(),
            })
            .expect("send name acknowledgement");
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

        assert_eq!(titles.titles, ["✷ kiki", "✷ Refactor auth - kiki"]);
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
        assert_eq!(titles.titles, ["✷ kiki"]);
    }

    #[test]
    fn session_name_uses_exact_title_and_front_truncated_footer_identity() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("worktrees/interactive/settings/kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));

        assert_eq!(app.title(), "✷ kiki");

        assert!(app.handle_paste("/name  Refactor\t auth").visible_changed);
        let update = app.handle_key_at(key(KeyCode::Enter), Instant::now());

        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        assert_eq!(name, "Refactor auth");
        assert!(app.prompt.is_empty());
        assert_eq!(app.title(), "✷ kiki");
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        }));
        assert_eq!(app.title(), "✷ Refactor auth - kiki");
        assert!(rendered(&mut app, 80, 4).contains("~/…/kiki (Refactor auth)"));
    }

    #[test]
    fn session_name_waits_for_acknowledgement_and_title_failure_is_visible() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("kiki");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        let update = app.name_session("  Refactor\t auth  ");
        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        assert_eq!(name, "Refactor auth");
        assert_eq!(app.title(), "✷ kiki");
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        }));
        assert_eq!(app.title(), "✷ Refactor auth - kiki");

        let mut titles = RecordingTitles::failing();
        assert!(super::publish_title(&mut app, &mut titles));
        assert_eq!(app.title(), "✷ Refactor auth - kiki");
        assert_eq!(app.notice(), Some("Could not change terminal title."));
        assert_eq!(titles.titles, ["✷ Refactor auth - kiki"]);
        assert!(rendered(&mut app, 80, 4).contains("~/kiki (Refactor auth)"));

        assert!(app.transcript.begin_submission());
        assert!(app.handle_paste("/name Active name").visible_changed);
        let update = app.handle_key_at(key(KeyCode::Enter), Instant::now());
        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        assert!(app.prompt.is_empty());
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        }));
        assert_eq!(app.title(), "✷ Active name - kiki");
    }

    #[test]
    fn only_the_latest_matching_name_acknowledgement_changes_the_title() {
        let mut app = App::default();
        let first = app.name_session("First");
        let [
            RuntimeCommand::SetSessionName {
                revision: first_revision,
                name: first_name,
            },
        ] = first.commands.as_slice()
        else {
            panic!("expected first name command");
        };
        let second = app.name_session("Second");
        let [
            RuntimeCommand::SetSessionName {
                revision: second_revision,
                name: second_name,
            },
        ] = second.commands.as_slice()
        else {
            panic!("expected second name command");
        };

        assert!(!app.handle_event(RuntimeEvent::NameAccepted {
            revision: *first_revision,
            name: first_name.clone(),
        }));
        assert_eq!(app.title(), "✷ kiki");
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *second_revision,
            name: second_name.clone(),
        }));
        assert_eq!(app.title(), "✷ Second - kiki");
    }

    #[test]
    fn restored_session_thinking_wins_over_changed_defaults_when_switching_models() {
        let home = tempdir().expect("home");
        let workspace = tempdir().expect("workspace");
        fs::create_dir_all(home.path().join(".kiki")).expect("user settings directory");
        fs::write(
            home.path().join(".kiki/config.toml"),
            "[thinking]\n\"openai/gpt-5.6-terra\" = \"low\"\n\"openai/gpt-5.6-sol\" = \"max\"\n",
        )
        .expect("user settings");
        fs::create_dir_all(workspace.path().join(".kiki")).expect("project settings directory");
        fs::write(
            workspace.path().join(".kiki/config.toml"),
            "[thinking]\n\"openai/gpt-5.6-terra\" = \"medium\"\n\"openai/gpt-5.6-sol\" = \"xhigh\"\n",
        )
        .expect("project settings");
        let preferences = SessionPreferences::new(
            ModelSelection::new(&TERRA, "high").expect("select Terra"),
            BTreeMap::from([(SOL.id, "low"), (TERRA.id, "high")]),
        )
        .expect("valid preferences");
        let metadata =
            SessionMetadata::new(Some("Saved name".to_owned()), preferences.clone(), false);
        let initial = RuntimeInitialState {
            metadata: Some(metadata.clone()),
            timeline: Vec::new(),
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        };
        let mut app = App::restored(
            crate::config::ready_snapshot_with_settings_for_test(
                Some(home.path()),
                workspace.path(),
            ),
            &initial,
        );
        let now = Instant::now();

        assert_eq!(initial.metadata.as_ref(), Some(&metadata));
        assert_eq!(
            app.title(),
            format!(
                "✷ Saved name - {}",
                workspace.path().file_name().unwrap().to_string_lossy()
            )
        );
        assert_eq!(&app.session_thinking, preferences.thinking());
        let (terra, _) = applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        assert_eq!(terra.thinking(), "high");
        let (sol, _) = applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-sol", now));
        assert_eq!(sol.thinking(), "low");
        let (terra, _) = applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        assert_eq!(terra.thinking(), "high");
    }

    #[test]
    fn restored_type_check_opt_out_is_visible() {
        let snapshot = crate::config::ready_snapshot_for_test();
        let selection = ModelSelection::new(&SOL, "low").unwrap();
        let initial = RuntimeInitialState {
            metadata: Some(SessionMetadata::new(
                None,
                SessionPreferences::initial(selection),
                true,
            )),
            timeline: Vec::new(),
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        };
        let app = App::restored(snapshot, &initial);
        assert_eq!(app.notice(), Some("Type checking is off for this session."));
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
        let update = app.name_session(&accepted);
        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        assert_eq!(app.title(), "✷ kiki");
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        }));
        assert_eq!(app.title(), format!("✷ {accepted} - kiki"));
        let rejected = "b".repeat(81);
        assert!(app.name_session(&rejected).visible_changed);
        assert_eq!(app.title(), format!("✷ {accepted} - kiki"));
        assert_eq!(
            app.notice(),
            Some("Session name must be at most 80 display cells.")
        );

        let wide = "界".repeat(40);
        let update = app.name_session(&wide);
        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        }));
        assert_eq!(app.title(), format!("✷ {wide} - kiki"));
        assert!(app.name_session(&"界".repeat(41)).visible_changed);
        assert_eq!(app.title(), format!("✷ {wide} - kiki"));
    }

    #[test]
    fn titles_sanitise_cwd_and_fall_back_when_unavailable() {
        let home = tempdir().expect("home");
        let workspace = home.path().join("kiki\nsettings");
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        assert_eq!(app.title(), "✷ kiki settings");
        let update = app.name_session("Refactor auth");
        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        });
        assert_eq!(app.title(), "✷ Refactor auth - kiki settings");
        assert_eq!(App::default().title(), "✷ kiki");
        let configured = App::configuration(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            &workspace,
        ));
        assert_eq!(configured.title(), "✷ kiki settings");
    }

    #[test]
    fn thinking_rule_spreads_model_levels_across_theme_stops() {
        let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
        let stops = [
            DARK.thinking_off,
            DARK.thinking_minimal,
            DARK.thinking_low,
            DARK.thinking_medium,
            DARK.thinking_high,
            DARK.thinking_xhigh,
            DARK.thinking_max,
        ];
        for count in [1, 3, 5, 7] {
            let model_levels = &levels[..count];
            assert_eq!(
                super::thinking_rule_colour(&DARK, model_levels, model_levels[0]),
                stops[0],
                "{count} levels"
            );
            if count > 1 {
                assert_eq!(
                    super::thinking_rule_colour(&DARK, model_levels, model_levels[count - 1]),
                    stops[6],
                    "{count} levels"
                );
            }
        }
        for (level, stop) in levels.into_iter().zip(stops) {
            assert_eq!(
                super::thinking_rule_colour(&DARK, &levels, level),
                stop,
                "{level}"
            );
        }
        assert_eq!(
            super::thinking_rule_colour(&DARK, &levels[..3], "minimal"),
            stops[3]
        );
        assert_eq!(
            super::thinking_rule_colour(&DARK, &levels[..5], "minimal"),
            Color::Rgb(94, 133, 153)
        );
        assert_eq!(
            super::thinking_rule_colour(&DARK, &levels[..5], "medium"),
            Color::Rgb(187, 101, 211)
        );
    }

    #[test]
    fn prompt_rules_follow_active_and_preview_thinking_colours() {
        let mut app = App::default();
        let now = Instant::now();
        assert!(app.cycle_thinking_preview(now).visible_changed);
        let buffer = rendered_buffer(&mut app, 80, 8);
        assert_eq!(
            buffer[(0, 7)].fg,
            super::thinking_rule_colour(&DARK, super::DEFAULT_MODEL.thinking_levels, "high")
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
    fn unknown_direct_skill_submits_nothing_and_keeps_draft() {
        let mut app = app_with_prompt("/skill:missing inspect this");
        let update = app.handle_key(key(KeyCode::Enter));
        assert!(update.commands.is_empty());
        assert_eq!(app.prompt.text(), "/skill:missing inspect this");
        assert!(app.notice().unwrap_or("").contains("missing"));
        assert!(app.transcript.is_idle());

        set_prompt(&mut app, "/skill:");
        let update = app.handle_key(key(KeyCode::Enter));
        assert!(update.commands.is_empty());
        assert_eq!(app.prompt.text(), "/skill:");
        assert!(app.notice().unwrap().contains("skill name"));
    }

    #[test]
    fn catalog_diagnostics_appear_after_startup_and_restored_navigation() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let invalid = home.path().join(".agents/skills/broken");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("SKILL.md"), "---\ndescription: \n---\nBody\n").unwrap();
        let catalog = Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project.path()).unwrap(),
            home.path(),
            false,
            "guidance",
        ));
        assert_eq!(catalog.diagnostics().len(), 1);

        let mut app = App::default().with_skill_catalog(catalog);
        let first = rendered(&mut app, 120, 20);
        assert!(first.contains("Skill discovery diagnostics"), "{first}");
        assert!(first.contains("description"), "{first}");
        assert!(first.contains("must be a string"), "{first}");

        app.replace_restored_state(&RuntimeInitialState::empty(), &None);
        let restored = rendered(&mut app, 120, 20);
        assert!(
            restored.contains("Skill discovery diagnostics"),
            "{restored}"
        );
        assert!(restored.contains("description"), "{restored}");
        assert!(restored.contains("must be a string"), "{restored}");
    }

    #[test]
    fn skipped_roots_have_distinct_bounded_escaped_diagnostic_rows() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let repository = project.path().join("a".repeat(100)).join("b".repeat(100));
        fs::create_dir_all(repository.join(".git")).unwrap();
        let launch = repository.join("unsafe\n\u{202e}tail");
        for scope in [&repository, &launch] {
            let root = scope.join(".agents/skills");
            fs::create_dir_all(&root).unwrap();
            for index in 0..129 {
                fs::create_dir(root.join(format!("candidate-{index:03}"))).unwrap();
            }
        }
        let catalog = SkillCatalog::discover(
            &ProjectScope::discover(&launch).unwrap(),
            home.path(),
            true,
            "guidance",
        );
        let rows = super::skill_diagnostic_report(&catalog);
        assert_eq!(catalog.diagnostics().len(), 2);
        assert_eq!(rows.len(), 3, "one heading and one row per skipped root");
        assert_ne!(rows[1], rows[2], "long roots must stay distinguishable");
        for row in &rows[1..] {
            assert!(row.len() <= super::SKILL_DIAGNOSTIC_LINE_BYTES);
            assert!(row.contains("entire root skipped"), "{row}");
            assert!(
                row.contains("...#"),
                "long path needs hash and suffix: {row}"
            );
            assert_eq!(row.lines().count(), 1);
        }
        let hostile = rows
            .iter()
            .find(|row| row.contains("\\x0A"))
            .expect("hostile launch path must be visible as escaped bytes");
        assert!(hostile.contains("\\xE2\\x80\\xAE"), "{hostile}");
        assert!(!hostile.contains('\u{202e}'));
    }

    #[test]
    fn skill_diagnostic_row_escapes_hostile_name_and_reason() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let diagnostic = SkillDiagnostic {
            path: OsString::from_vec(b"/root/unsafe\n\xe2\x80\xae\xff".to_vec()).into(),
            name: Some("bad\x1bname".to_owned()),
            reason: format!("bad\n\u{202e}{}", "x".repeat(1000)),
        };
        let row = super::format_skill_diagnostic(&diagnostic);
        assert!(row.len() <= super::SKILL_DIAGNOSTIC_LINE_BYTES);
        assert_eq!(row.lines().count(), 1);
        assert!(row.contains("unsafe\\x0A\\xE2\\x80\\xAE\\xFF"), "{row}");
        assert!(row.contains("bad\\x1Bname"), "{row}");
        assert!(row.contains("bad\\x0A\\xE2\\x80\\xAE"), "{row}");
        assert!(row.ends_with("..."), "{row}");
    }

    fn app_with_user_skills(home: &Path, project: &Path) -> App {
        App::default().with_skill_catalog(Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project).unwrap(),
            home,
            false,
            "guidance",
        )))
    }

    #[test]
    fn skill_name_completion_expands_before_submitting_canonical_activation() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        for name in ["review", "review-more"] {
            write_skill(
                &home.path().join(".agents/skills"),
                name,
                "description: Review code\ndisable-model-invocation: true\n",
                "Review this.\n",
            );
        }
        let catalog = Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project.path()).unwrap(),
            home.path(),
            false,
            "guidance",
        ));
        for draft in ["/revi", "/review"] {
            for accept in [KeyCode::Tab, KeyCode::Enter] {
                let mut app = App::default().with_skill_catalog(catalog.clone());
                set_prompt(&mut app, draft);
                app.refresh_completion();
                assert_eq!(app.completion.selected_token(), Some("/skill:review"));
                assert!(app.handle_key(key(accept)).commands.is_empty());
                assert_eq!(app.prompt.text(), "/skill:review");
                assert_eq!(app.completion.selected_token(), None);
                let update = app.handle_key(key(KeyCode::Enter));
                let [
                    RuntimeCommand::Submit {
                        request,
                        activation: Some(activation),
                    },
                ] = update.commands.as_slice()
                else {
                    panic!("expected canonical skill submission for {draft:?} via {accept:?}");
                };
                assert_eq!(request, "/skill:review");
                assert_eq!(activation.name(), "review");
                assert!(app.prompt.is_empty());
            }
        }
    }

    #[test]
    fn exact_builtin_is_not_intercepted_by_skill_name_completion() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        for name in ["model", "model-review"] {
            write_skill(
                &home.path().join(".agents/skills"),
                name,
                "description: Model workflow\n",
                "Use this workflow.\n",
            );
        }
        let mut app = app_with_user_skills(home.path(), project.path());
        set_prompt(&mut app, "/model");
        app.refresh_completion();
        assert_eq!(app.completion.selected_token(), None);
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert!(app.prompt.is_empty());
        assert!(app.picker.is_some());
    }

    #[test]
    fn direct_only_skill_completes_and_submits_captured_activation() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let direct = write_skill(
            &home.path().join(".agents/skills"),
            "direct",
            "description: A direct workflow\ndisable-model-invocation: true\n",
            "Follow the direct workflow.\n",
        );
        let mut app = app_with_user_skills(home.path(), project.path());
        set_prompt(&mut app, "/skill:di");
        app.refresh_completion();
        assert_eq!(app.completion.selected_token(), Some("/skill:direct"));
        assert_eq!(
            app.completion.visible_rows(2)[0].purpose,
            "A direct workflow"
        );
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(app.prompt.text(), "/skill:direct");
        let update = app.handle_key(key(KeyCode::Enter));
        let [
            RuntimeCommand::Submit {
                request,
                activation: Some(activation),
            },
        ] = update.commands.as_slice()
        else {
            panic!("expected direct skill submission");
        };
        assert_eq!(request, "/skill:direct");
        assert_eq!(activation.name(), "direct");
        assert_eq!(
            activation.source_text(),
            fs::read_to_string(direct.join("SKILL.md")).unwrap()
        );
        assert!(app.prompt.is_empty());
    }

    #[test]
    fn skipped_project_and_invalid_skills_are_refused_without_submission() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let rejected = home.path().join(".agents/skills/rejected");
        fs::create_dir_all(&rejected).unwrap();
        fs::write(rejected.join("SKILL.md"), "---\ndescription: \n---\nBody\n").unwrap();
        write_skill(
            &project.path().join(".agents/skills"),
            "hidden",
            "description: Hidden\n",
            "Hidden body\n",
        );
        let mut app = app_with_user_skills(home.path(), project.path());
        for draft in ["/hid", "/rejected", "/skill:hid"] {
            set_prompt(&mut app, draft);
            app.refresh_completion();
            assert!(app.completion.visible_rows(5).is_empty(), "{draft}");
        }
        set_prompt(&mut app, "/skill:hidden");
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert!(app.notice().unwrap().contains("hidden"));
        set_prompt(&mut app, "/skill:rejected");
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert!(app.notice().unwrap().contains("invalid"));
    }

    #[test]
    fn direct_skill_submits_the_typed_command_as_the_request() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "description: Review\n",
            "Review this.\n",
        );
        let catalog = Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project.path()).unwrap(),
            home.path(),
            false,
            "guidance",
        ));
        for draft in [
            "/skill:review",
            "/skill:review fix it",
            "/skill:review inspect this\n  and this",
        ] {
            let mut app = App::default().with_skill_catalog(catalog.clone());
            set_prompt(&mut app, draft);
            let update = app.handle_key(key(KeyCode::Enter));
            let [
                RuntimeCommand::Submit {
                    request,
                    activation: Some(activation),
                },
            ] = update.commands.as_slice()
            else {
                panic!("expected direct skill submission for {draft:?}");
            };
            assert_eq!(request, draft);
            assert_eq!(activation.name(), "review");
        }
    }

    #[test]
    fn enter_invokes_exact_skill_even_when_a_longer_completion_exists() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        for name in ["review", "review-more"] {
            write_skill(
                &home.path().join(".agents/skills"),
                name,
                &format!("description: {name} instructions\ndisable-model-invocation: true\n"),
                &format!("Use {name}.\n"),
            );
        }
        let catalog = Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project.path()).unwrap(),
            home.path(),
            false,
            "guidance",
        ));
        let mut app = App::default().with_skill_catalog(catalog.clone());
        set_prompt(&mut app, "/skill:review");
        app.refresh_completion();
        assert_eq!(app.completion.selected_token(), None);
        let update = app.handle_key(key(KeyCode::Enter));
        let [
            RuntimeCommand::Submit {
                activation: Some(activation),
                ..
            },
        ] = update.commands.as_slice()
        else {
            panic!("exact skill should invoke on Enter");
        };
        assert_eq!(activation.name(), "review");
        assert!(app.prompt.is_empty());

        let mut partial = App::default().with_skill_catalog(catalog.clone());
        set_prompt(&mut partial, "/skill:rev");
        assert!(partial.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(partial.prompt.text(), "/skill:review");

        let mut tab = App::default().with_skill_catalog(catalog);
        set_prompt(&mut tab, "/skill:review");
        assert!(tab.handle_key(key(KeyCode::Tab)).commands.is_empty());
        assert_eq!(tab.prompt.text(), "/skill:review");
    }

    #[test]
    fn enter_reports_rejected_exact_skill_before_longer_completion() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        let invalid = home.path().join(".agents/skills/review");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join("SKILL.md"), "---\ndescription: \n---\nBody\n").unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "review-more",
            "description: Longer\n",
            "Body\n",
        );
        let catalog = Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project.path()).unwrap(),
            home.path(),
            false,
            "guidance",
        ));
        let mut app = App::default().with_skill_catalog(catalog);
        set_prompt(&mut app, "/skill:review");
        app.refresh_completion();
        assert_eq!(app.completion.selected_token(), None);
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(app.prompt.text(), "/skill:review");
        assert!(app.notice().unwrap().contains("invalid"));
    }

    #[test]
    fn direct_skill_busy_rejection_preserves_draft_and_cap_rejection_restores_idle() {
        let mut app = app_with_prompt("/skill:review inspect this");
        app.transcript.begin_submission();
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(app.prompt.text(), "/skill:review inspect this");
        app.handle_event(RuntimeEvent::SkillActivationRejected {
            reason: "active skill instruction text exceeds 256 KiB".to_owned(),
        });
        assert!(app.transcript.is_idle());
        assert!(!app.transcript.is_disabled());
        assert!(app.notice().unwrap().contains("256 KiB"));
    }

    #[test]
    fn direct_skill_admission_rejection_restores_unedited_draft_only() {
        let home = tempdir().unwrap();
        let project = tempdir().unwrap();
        write_skill(
            &home.path().join(".agents/skills"),
            "review",
            "description: Review\n",
            "Review this.\n",
        );
        let catalog = Arc::new(SkillCatalog::discover(
            &ProjectScope::discover(project.path()).unwrap(),
            home.path(),
            false,
            "guidance",
        ));
        let mut app = App::default().with_skill_catalog(catalog);
        let draft = "/skill:review inspect this\n  and this";
        set_prompt(&mut app, draft);
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)).commands.as_slice(),
            [RuntimeCommand::Submit {
                activation: Some(_),
                ..
            }]
        ));
        assert!(app.prompt.is_empty());
        app.handle_event(RuntimeEvent::SkillActivationRejected {
            reason: "active text exceeds 256 KiB".to_owned(),
        });
        assert_eq!(app.prompt.text(), draft);
        assert!(app.transcript.is_idle());

        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)).commands.as_slice(),
            [RuntimeCommand::Submit {
                activation: Some(_),
                ..
            }]
        ));
        set_prompt(&mut app, "new draft");
        app.handle_event(RuntimeEvent::SkillActivationRejected {
            reason: "active text exceeds 256 KiB".to_owned(),
        });
        assert_eq!(app.prompt.text(), "new draft");

        set_prompt(&mut app, draft);
        let update = app.handle_key(key(KeyCode::Enter));
        let [
            RuntimeCommand::Submit {
                request,
                activation: Some(_),
            },
        ] = update.commands.as_slice()
        else {
            panic!("expected direct skill submission");
        };
        assert!(app.pending_skill_draft.is_some());
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: request.clone(),
                    selection: None,
                },
            },
            progress_gap: false,
        });
        assert!(app.pending_skill_draft.is_none());
    }

    #[test]
    fn slash_completion_lists_catalogue_in_order_and_clamps_keyboard_selection() {
        fn assert_slash_selection(cell: &ratatui::buffer::Cell) {
            assert_eq!(cell.fg, DISTINCT_TEST_THEME.text);
            assert_eq!(cell.bg, DISTINCT_TEST_THEME.selected_bg);
            assert!(!cell.modifier.contains(ratatui::style::Modifier::REVERSED));
        }

        let mut app = app_with_prompt("/");
        app.theme = &DISTINCT_TEST_THEME;
        let initial = rendered_buffer(&mut app, 40, 12);
        let overlay = app.slash_overlay.expect("slash overlay");
        let (compact_column, compact_row) = cell_start(&initial, "/compact");
        let (model_column, model_row) = cell_start(&initial, "/model");
        let (name_column, name_row) = cell_start(&initial, "/name");
        let (settings_column, settings_row) = cell_start(&initial, "/settings");
        let (theme_column, theme_row) = cell_start(&initial, "/theme");
        let (thinking_column, thinking_row) = cell_start(&initial, "/thinking");
        let (tree_column, tree_row) = cell_start(&initial, "/tree");

        assert_eq!(compact_column, overlay.x);
        assert!(
            compact_row < model_row
                && model_row < name_row
                && name_row < settings_row
                && settings_row < theme_row
                && theme_row < thinking_row
                && thinking_row < tree_row
        );
        assert_slash_selection(&initial[(compact_column, compact_row)]);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        let model = rendered_buffer(&mut app, 40, 12);
        assert_slash_selection(&model[(model_column, model_row)]);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        let name = rendered_buffer(&mut app, 40, 12);
        assert_slash_selection(&name[(name_column, name_row)]);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        let settings = rendered_buffer(&mut app, 40, 12);
        assert_slash_selection(&settings[(settings_column, settings_row)]);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        let theme = rendered_buffer(&mut app, 40, 12);
        assert_slash_selection(&theme[(theme_column, theme_row)]);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        let thinking = rendered_buffer(&mut app, 40, 12);
        assert_slash_selection(&thinking[(thinking_column, thinking_row)]);
        assert!(app.handle_key(key(KeyCode::Down)).visible_changed);
        assert!(!app.handle_key(key(KeyCode::Down)).visible_changed);
        assert!(
            !app.handle_key(modified_key(KeyCode::Up, KeyModifiers::CONTROL))
                .visible_changed
        );
        let tree = rendered_buffer(&mut app, 40, 12);
        assert_slash_selection(&tree[(tree_column, tree_row)]);
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
            [RuntimeCommand::Cancel { correction: false }]
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
        let buffer = rendered_buffer(&mut wrapped, 5, 21);
        let (token_column, token_row) = cell_start(&buffer, "/mo");
        let (argument_column, argument_row) = cell_start(&buffer, "arg");

        for column in token_column..token_column + 3 {
            assert_eq!(buffer[(column, token_row)].fg, DARK.accent);
            assert!(
                buffer[(column, token_row)]
                    .modifier
                    .contains(ratatui::style::Modifier::BOLD)
            );
            assert_eq!(buffer[(column, token_row + 1)].fg, DARK.accent);
            assert!(
                buffer[(column, token_row + 1)]
                    .modifier
                    .contains(ratatui::style::Modifier::BOLD)
            );
        }
        assert_ne!(buffer[(argument_column + 1, argument_row)].fg, DARK.accent);
        assert!(
            !buffer[(argument_column + 1, argument_row)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );

        assert!(wrapped.handle_key(key(KeyCode::Left)).visible_changed);
        let interior = rendered_buffer(&mut wrapped, 5, 21);
        assert_eq!(interior[(token_column, token_row)].fg, DARK.accent);
        assert!(
            interior[(token_column, token_row)]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );

        for (draft, token) in [("/future", "/future"), ("/model\narg", "/model")] {
            let mut app = app_with_prompt(draft);
            let buffer = rendered_buffer(&mut app, 40, 12);
            let (column, row) = cell_start(&buffer, token);
            assert_ne!(buffer[(column, row)].fg, DARK.accent, "{draft}");
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
        assert!(tiny.handle_key(key(KeyCode::Down)).visible_changed);
        let buffer = rendered_buffer(&mut tiny, 5, 5);
        let (column, row) = cell_start(&buffer, "/sett");
        for selected_column in 0..5 {
            let cell = &buffer[(selected_column, row)];
            assert_eq!(cell.fg, DARK.text);
            assert_eq!(cell.bg, DARK.selected_bg);
            assert!(!cell.modifier.contains(ratatui::style::Modifier::REVERSED));
        }
        assert_eq!(column, 0);

        rendered_buffer(&mut tiny, 5, 4);
        assert!(tiny.slash_overlay.is_none());
    }

    #[test]
    fn slash_overlay_stays_above_prompt_over_visible_subagent_monitor() {
        let now = Instant::now();
        for (height, last_suggestion) in [(12, "/model"), (6, "/model")] {
            let mut plain = app_with_history(App::default());
            plain
                .subagent_monitor
                .insert_for_test("child1", TaskState::Completed, now);
            plain
                .subagent_monitor
                .insert_for_test("child2", TaskState::Completed, now);
            let mut completed = app_with_history(app_with_prompt("/m"));
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
    fn session_tree_picker_composes_rules_content_and_surrounding_views() {
        let now = Instant::now();
        let mut app = app_with_history(app_with_prompt("hidden\nmultiline draft"));
        app.subagent_monitor
            .insert_for_test("child", TaskState::Completed, now);
        let revision = tree_request(&mut app, "");
        assert!(app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(long_session_tree_catalogue(20)),
        }));

        let screen = rendered_at(&mut app, 100, 20, now);
        assert!(!screen.contains("hidden"), "{screen}");
        assert!(!screen.contains("Status:"), "{screen}");
        let tree = rendered_buffer(&mut app, 100, 20);
        for row in [6, 8] {
            assert!(
                (tree.area.left()..tree.area.right())
                    .all(|column| tree[(column, row)].symbol() == "─"),
                "tree rule at row {row}"
            );
        }
        assert!(
            (tree.area.left()..tree.area.right()).all(|column| tree[(column, 19)].symbol() == "─"),
            "footer rule"
        );
        assert!(
            (tree.area.left()..tree.area.right())
                .map(|column| tree[(column, 7)].symbol())
                .collect::<String>()
                .starts_with("Session tree")
        );
        assert!(
            (tree.area.left()..tree.area.right())
                .map(|column| tree[(column, 9)].symbol())
                .collect::<String>()
                .contains("Session start")
        );
        assert!(
            (tree.area.left()..tree.area.right())
                .map(|column| tree[(column, 18)].symbol())
                .collect::<String>()
                .contains("User: entry 9")
        );
        let (_, transcript_row) = cell_start(&tree, "request 8");
        let (_, monitor_row) = cell_start(&tree, "✓ subagent:child");
        assert!(transcript_row < 6, "transcript row {transcript_row}");
        assert!(monitor_row < 6, "monitor row {monitor_row}");
        assert!(!screen.contains("Filter:"), "{screen}");
        assert!(!screen.contains('┌'), "{screen}");

        assert!(app.handle_key(key(KeyCode::PageDown)).visible_changed);
        let paged = rendered_at(&mut app, 100, 20, now);
        assert!(paged.contains("User: entry 10"), "{paged}");

        assert!(app.handle_key(key(KeyCode::Esc)).visible_changed);
        assert_eq!(app.prompt.text(), "hidden\nmultiline draft");
        let restored = rendered_at(&mut app, 100, 20, now);
        assert!(restored.contains("hidden"), "{restored}");
        assert!(restored.contains("multiline draft"), "{restored}");
    }

    #[test]
    fn only_session_tree_picker_clips_constrained_docks_while_ordinary_pickers_remain_boxed() {
        let mut tree = app_with_prompt("hidden\nmultiline draft");
        let revision = tree_request(&mut tree, "");
        assert!(tree.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(long_session_tree_catalogue(20)),
        }));

        let screen = rendered(&mut tree, 100, 10);
        assert!(!screen.contains("hidden"), "{screen}");
        assert!(!screen.contains("multiline draft"), "{screen}");
        assert!(!screen.contains("User: entry 5"), "{screen}");
        let tree_buffer = rendered_buffer(&mut tree, 100, 10);
        assert_eq!(buffer_row(&tree_buffer, 1), "─".repeat(100));
        assert_eq!(
            buffer_row(&tree_buffer, 2).trim_end(),
            "Session tree · 1/21 · Enter restore · Esc clear/close · ↑/↓ move · ←/→ branch · Ctrl-X copy"
        );
        assert_eq!(buffer_row(&tree_buffer, 3), "─".repeat(100));
        assert_eq!(buffer_row(&tree_buffer, 4).trim_end(), "› ● Session start");
        assert_eq!(buffer_row(&tree_buffer, 8).trim_end(), "     User: entry 4");
        assert_eq!(buffer_row(&tree_buffer, 9), "─".repeat(100));

        assert!(tree.handle_key(key(KeyCode::Esc)).visible_changed);
        assert_eq!(tree.prompt.text(), "hidden\nmultiline draft");
        let restored = rendered(&mut tree, 100, 12);
        assert!(restored.contains("hidden"), "{restored}");
        assert!(restored.contains("multiline draft"), "{restored}");

        let mut tree = App::default();
        let revision = tree_request(&mut tree, "");
        load_tree(&mut tree, revision);
        let tree = rendered_buffer(&mut tree, 100, 12);
        let (_, tree_title_row) = cell_start(&tree, "Session tree");
        assert_eq!(tree[(0, tree_title_row.saturating_sub(1))].symbol(), "─");
        assert_ne!(tree[(0, tree_title_row.saturating_sub(1))].symbol(), "┌");

        let mut model = app_with_prompt("model draft");
        model.open_model_picker("");
        let model_screen = rendered(&mut model, 100, 12);
        assert!(model_screen.contains("model draft"), "{model_screen}");
        let model = rendered_buffer(&mut model, 100, 12);
        let (_, model_title_row) = cell_start(&model, "Models");
        assert_eq!(model[(14, model_title_row)].symbol(), "┌");
        assert_eq!(model[(85, model_title_row)].symbol(), "┐");

        let mut thinking = app_with_prompt("thinking draft");
        thinking.open_thinking_picker();
        let thinking_screen = rendered(&mut thinking, 100, 12);
        assert!(
            thinking_screen.contains("thinking draft"),
            "{thinking_screen}"
        );
        let thinking = rendered_buffer(&mut thinking, 100, 12);
        let (_, thinking_title_row) = cell_start(&thinking, "Thinking");
        assert_eq!(thinking[(14, thinking_title_row)].symbol(), "┌");
        assert_eq!(thinking[(85, thinking_title_row)].symbol(), "┐");
    }

    #[test]
    fn session_tree_navigator_refreshes_width_before_catalogue_load() {
        let mut app = App::default();
        let revision = tree_request(&mut app, "");
        let loading = rendered(&mut app, 100, 12);
        assert!(loading.contains("Loading…"), "{loading}");

        let text = format!("{} wide tail", "x".repeat(75));
        assert!(app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(SessionCatalogue {
                active_entry: SessionEntryId(2),
                entries: vec![
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(1),
                        parent_id: None,
                        revision: 1,
                        kind: SessionCatalogueKind::Start,
                    },
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(2),
                        parent_id: Some(SessionEntryId(1)),
                        revision: 2,
                        kind: SessionCatalogueKind::Request { text },
                    },
                ],
            }),
        }));

        let screen = rendered(&mut app, 100, 12);
        assert!(screen.contains("wide tail"), "{screen}");
    }

    #[test]
    fn tree_picker_navigates_without_losing_the_editor_draft() {
        let mut app = app_with_prompt("keep this draft");

        let update = app.handle_key(modified_key(KeyCode::Char('r'), KeyModifiers::CONTROL));

        assert_eq!(
            update.commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
        assert_eq!(app.prompt.text(), "keep this draft");
        assert!(rendered(&mut app, 72, 12).contains("Session tree"));
        assert!(rendered(&mut app, 72, 12).contains("Loading…"));
    }

    #[test]
    fn successful_copy_from_tree_keeps_tree_open() {
        let now = Instant::now();
        let mut app = App::default();
        let revision = tree_request(&mut app, "");
        load_tree(&mut app, revision);

        assert!(
            app.handle_key(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert!(app.tree_navigator.is_some());
        let mut clipboard = FakeClipboard::available();
        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(
            clipboard.writes,
            ["Commentary:\ninspect credentials\n\nPython:\npass"]
        );
        assert_eq!(app.notice(), Some("Copied"));
        assert!(app.flash.is_some());
        assert!(app.tree_navigator.is_some());
    }

    #[test]
    fn copying_from_tree_survives_unavailable_history_geometry() {
        let mut app = App::default();
        let revision = tree_request(&mut app, "");
        let mut catalogue = long_session_tree_catalogue(3);
        catalogue.active_entry = SessionEntryId(4);
        assert!(app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(catalogue),
        }));
        let mut terminal = Terminal::new(TestBackend::new(72, 8)).unwrap();
        let mut clipboard = FakeClipboard::available();

        assert!(!super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap());
        assert!(app.history_viewport.is_none());

        assert!(
            app.handle_key(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert!(super::draw_frame(&mut terminal, &mut app, &mut clipboard).unwrap());
        assert_eq!(clipboard.writes, ["entry 3"]);
        assert_eq!(app.notice(), Some("Copied"));
        assert!(app.tree_navigator.is_some());
    }

    #[test]
    fn failed_copy_from_tree_keeps_tree_open() {
        let now = Instant::now();
        let mut app = App::default();
        let revision = tree_request(&mut app, "");
        load_tree(&mut app, revision);

        assert!(
            app.handle_key(modified_key(KeyCode::Char('x'), KeyModifiers::CONTROL))
                .visible_changed
        );
        let mut clipboard = FakeClipboard {
            writes: Vec::new(),
            result: Err(ClipboardError),
        };
        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(app.notice(), Some("Copy failed"));
        assert!(app.flash.is_none());
        assert!(app.tree_navigator.is_some());
    }

    #[test]
    fn tree_picker_ctrl_u_reaches_the_navigator_before_prompt_controls() {
        let mut app = app_with_prompt("preserved draft");
        let revision = tree_request(&mut app, "");
        assert!(app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(SessionCatalogue {
                active_entry: SessionEntryId(4),
                entries: vec![
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(1),
                        parent_id: None,
                        revision: 1,
                        kind: SessionCatalogueKind::Start,
                    },
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(2),
                        parent_id: Some(SessionEntryId(1)),
                        revision: 2,
                        kind: SessionCatalogueKind::Request {
                            text: "conversation".to_owned(),
                        },
                    },
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(3),
                        parent_id: Some(SessionEntryId(2)),
                        revision: 3,
                        kind: tree_execution("omitted Python"),
                    },
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(4),
                        parent_id: Some(SessionEntryId(3)),
                        revision: 4,
                        kind: SessionCatalogueKind::Final {
                            text: "retained reply".to_owned(),
                        },
                    },
                ],
            }),
        }));
        assert!(rendered(&mut app, 72, 12).contains("Python: omitted Python"));

        assert!(
            app.handle_key(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert_eq!(app.prompt.text(), "preserved draft");
        let screen = rendered(&mut app, 72, 12);
        assert!(!screen.contains("Python: omitted Python"), "{screen}");
        assert!(screen.contains("Kiki: retained reply"), "{screen}");
    }

    #[test]
    fn tree_picker_left_and_right_reach_the_navigator_before_prompt_controls() {
        let mut app = app_with_prompt("preserved draft");
        let revision = tree_request(&mut app, "");
        assert!(app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(SessionCatalogue {
                active_entry: SessionEntryId(1),
                entries: vec![
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(1),
                        parent_id: None,
                        revision: 1,
                        kind: SessionCatalogueKind::Start,
                    },
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(2),
                        parent_id: Some(SessionEntryId(1)),
                        revision: 2,
                        kind: SessionCatalogueKind::Request {
                            text: "first branch".to_owned(),
                        },
                    },
                    SessionCatalogueEntry {
                        entry_id: SessionEntryId(3),
                        parent_id: Some(SessionEntryId(1)),
                        revision: 3,
                        kind: SessionCatalogueKind::Request {
                            text: "second branch".to_owned(),
                        },
                    },
                ],
            }),
        }));

        let left = app.handle_key(key(KeyCode::Left));
        assert!(left.visible_changed);
        assert!(left.commands.is_empty());
        assert_eq!(app.prompt.text(), "preserved draft");
        let folded = rendered(&mut app, 72, 12);
        assert!(folded.contains("⊞ ● Session start"), "{folded}");
        assert!(!folded.contains("first branch"), "{folded}");

        let right = app.handle_key(key(KeyCode::Right));
        assert!(right.visible_changed);
        assert!(right.commands.is_empty());
        assert_eq!(app.prompt.text(), "preserved draft");
        let expanded = rendered(&mut app, 72, 12);
        assert!(expanded.contains("first branch"), "{expanded}");
        assert!(expanded.contains("second branch"), "{expanded}");
    }

    #[test]
    fn tree_picker_loading_ignores_input_and_stale_catalogues() {
        let now = Instant::now();
        let mut bare = App::default();
        assert_eq!(
            enter_at(&mut bare, "/tree", now).commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
        assert!(rendered(&mut bare, 72, 12).contains("Session tree"));
        let loading = rendered_buffer(&mut bare, 72, 12);
        let (_, header_row) = cell_start(&loading, "Session tree");
        assert_eq!(loading[(0, header_row.saturating_sub(1))].symbol(), "─");

        let mut app = App::default();
        let update = enter_at(&mut app, "/tree auth", now);
        let [RuntimeCommand::RequestTree { revision }] = update.commands.as_slice() else {
            panic!("expected tree request, got {:?}", update.commands);
        };
        let screen = rendered(&mut app, 72, 12);
        assert!(screen.contains("Session tree"), "{screen}");
        assert!(screen.contains("Loading…"), "{screen}");
        assert!(!screen.contains("authorise access"), "{screen}");
        assert!(app.prompt.is_empty());

        for key in [
            key(KeyCode::Enter),
            key(KeyCode::Char('x')),
            modified_key(KeyCode::Char('s'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Char('r'), KeyModifiers::CONTROL),
        ] {
            let update = app.handle_key_at(key, now);
            assert!(update.commands.is_empty());
            assert!(!update.visible_changed);
            assert!(app.notice().is_none());
        }
        assert!(!app.handle_paste("ignored").visible_changed);
        assert!(app.prompt.is_empty());
        app.set_notice("keep loading notice".to_owned());
        assert!(
            !app.handle_mouse(mouse_event(MouseEventKind::ScrollUp, 0, 0), now)
                .visible_changed
        );
        assert_eq!(app.notice(), Some("keep loading notice"));
        assert!(app.clear_notice());

        assert!(app.handle_key_at(key(KeyCode::Esc), now).visible_changed);
        assert!(app.picker.is_none());
        assert!(app.tree_navigator.is_none());
        assert!(!app.handle_event(RuntimeEvent::TreeCatalogue {
            revision: *revision,
            outcome: TreeRequestOutcome::Catalogue(session_tree_catalogue()),
        }));
        assert!(app.picker.is_none());
        assert!(app.notice().is_none());

        let first = tree_request(&mut app, "first");
        let second = tree_request(&mut app, "auth");
        assert!(!app.handle_event(RuntimeEvent::TreeCatalogue {
            revision: first,
            outcome: TreeRequestOutcome::Catalogue(session_tree_catalogue()),
        }));
        assert!(rendered(&mut app, 72, 12).contains("Loading…"));
        load_tree(&mut app, second);
        let screen = rendered(&mut app, 72, 12);
        assert!(screen.contains("Search: auth"), "{screen}");
        assert!(screen.contains("authorise access"), "{screen}");
        assert!(!screen.contains("Loading…"), "{screen}");

        let mut no_matches = App::default();
        let revision = tree_request(&mut no_matches, "missing");
        assert!(no_matches.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(SessionCatalogue {
                active_entry: SessionEntryId(1),
                entries: vec![SessionCatalogueEntry {
                    entry_id: SessionEntryId(1),
                    parent_id: None,
                    revision: 1,
                    kind: SessionCatalogueKind::Start,
                }],
            }),
        }));
        assert!(rendered(&mut no_matches, 72, 12).contains("No matching session entries"));

        for (outcome, notice) in [
            (
                TreeRequestOutcome::RejectedBusy,
                "Session tree is unavailable while work is active.",
            ),
            (
                TreeRequestOutcome::RejectedReadOnly,
                "Session tree is unavailable while the session is read-only.",
            ),
            (TreeRequestOutcome::Failed, "Session could not be restored."),
        ] {
            let mut app = App::default();
            app.transcript.append_restoration_card();
            let revision = tree_request(&mut app, "");
            assert!(app.handle_event(RuntimeEvent::TreeCatalogue { revision, outcome }));
            assert!(app.picker.is_none());
            assert_eq!(app.notice(), Some(notice));
            assert!(rendered(&mut app, 72, 12).contains("Session restored."));
            assert!(!app.handle_event(RuntimeEvent::TreeCatalogue {
                revision,
                outcome: TreeRequestOutcome::Catalogue(session_tree_catalogue()),
            }));
            assert!(app.picker.is_none());
        }

        let mut loaded = App::default();
        let revision = tree_request(&mut loaded, "");
        load_tree(&mut loaded, revision);
        assert!(
            loaded
                .handle_key(modified_key(KeyCode::Char('s'), KeyModifiers::CONTROL))
                .commands
                .is_empty()
        );
        assert!(loaded.handle_key(key(KeyCode::Esc)).commands.is_empty());
        assert!(!loaded.manual_navigation_pending);
    }

    #[test]
    fn tree_loading_rejections_preserve_status_notices_across_prompt_geometry() {
        for (outcome, notice) in [
            (
                TreeRequestOutcome::RejectedBusy,
                "Session tree is unavailable while work is active.",
            ),
            (
                TreeRequestOutcome::RejectedReadOnly,
                "Session tree is unavailable while the session is read-only.",
            ),
            (TreeRequestOutcome::Failed, "Session could not be restored."),
        ] {
            let mut app = App::default();
            let revision = tree_request(&mut app, "");
            rendered(&mut app, 120, 12);

            assert!(app.handle_event(RuntimeEvent::TreeCatalogue { revision, outcome }));
            for _ in 0..2 {
                let screen = rendered(&mut app, 120, 12);
                assert!(screen.contains(notice), "{screen}");
                assert_eq!(app.notice(), Some(notice));
            }
        }
    }

    #[test]
    fn terminal_subagent_projections_stay_operational_until_settlement() {
        let now = Instant::now();
        let terminal = TaskRecord::for_test(
            "c1",
            2,
            ModelSelection::new(&TERRA, TERRA.default_thinking).expect("select test model"),
            None,
            1,
            Vec::new(),
        );
        for event in [
            RuntimeEvent::SubagentRecord {
                record: terminal.clone(),
            },
            RuntimeEvent::SubagentStatus {
                id: terminal.id().clone(),
                revision: terminal.revision(),
                observation: terminal.observation().clone(),
            },
        ] {
            let mut app = App::default();
            app.subagent_monitor
                .insert_for_test(terminal.id().as_str(), TaskState::Running, now);

            assert!(app.handle_event_at(event, now));
            assert!(app.subagent_monitor.has_active_entries());
            assert_eq!(
                app.handle_key_at(key(KeyCode::Esc), now).commands,
                [RuntimeCommand::CancelBackground]
            );
            assert_eq!(app.settled_escape_at, None);

            assert!(app.handle_event_at(
                RuntimeEvent::SubagentFinished {
                    record: terminal.clone(),
                },
                now,
            ));
            assert!(app.subagent_monitor.has_active_entries());

            assert!(app.handle_event_at(
                RuntimeEvent::SubagentOperationSettled {
                    id: terminal.id().clone(),
                },
                now,
            ));
            assert!(!app.subagent_monitor.has_active_entries());
            assert!(!app.handle_event_at(
                RuntimeEvent::SubagentStatus {
                    id: terminal.id().clone(),
                    revision: terminal.revision(),
                    observation: terminal.observation().clone(),
                },
                now,
            ));
            assert!(!app.subagent_monitor.has_active_entries());
            assert!(
                app.handle_key_at(key(KeyCode::Esc), now)
                    .commands
                    .is_empty()
            );
            assert_eq!(app.settled_escape_at, Some(now));
        }
    }

    #[test]
    fn tree_picker_filters_without_resetting_on_duplicate_catalogues() {
        let mut app = App::default();
        let revision = tree_request(&mut app, "");
        load_tree(&mut app, revision);

        for character in "credentials".chars() {
            assert!(
                app.handle_key(key(KeyCode::Char(character)))
                    .visible_changed
            );
        }
        let filtered = rendered(&mut app, 72, 12);
        let filtered_rows = filtered.lines().collect::<Vec<_>>();
        assert_eq!(filtered_rows[8].trim_end(), "  • Session start");
        assert_eq!(filtered_rows[9].trim_end(), "  └─ • User: authorise access");
        assert_eq!(
            filtered_rows[10].trim_end(),
            "›    ● Python: inspect credentials"
        );

        for _ in "credentials".chars() {
            assert!(app.handle_key(key(KeyCode::Backspace)).visible_changed);
        }
        let restored = rendered(&mut app, 72, 12);
        let restored_rows = restored.lines().collect::<Vec<_>>();
        assert_eq!(restored_rows[9].trim_end(), "  └─ • User: authorise access");
        assert_eq!(
            restored_rows[10].trim_end(),
            "›    ● Python: inspect credentials"
        );

        for character in "access".chars() {
            assert!(
                app.handle_key(key(KeyCode::Char(character)))
                    .visible_changed
            );
        }
        assert!(!app.handle_event(RuntimeEvent::TreeCatalogue {
            revision,
            outcome: TreeRequestOutcome::Catalogue(session_tree_catalogue()),
        }));
        let duplicate = rendered(&mut app, 72, 12);
        assert!(duplicate.contains("Search: access"), "{duplicate}");
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Navigate {
                entry: SessionEntryId(2)
            }]
        );
    }

    #[test]
    fn tree_navigation_to_current_point_redraws_closed_tree_before_quit() {
        let mut app = App::default();
        let revision = tree_request(&mut app, "");
        load_tree(&mut app, revision);
        let mut terminal = Terminal::new(TestBackend::new(72, 12)).unwrap();
        let mut input = FakeInput::new([Some(Event::Key(key(KeyCode::Enter))), Some(quit_event())]);
        let (commands, mut received) = mpsc::channel(8);
        let (event_sender, mut events) = mpsc::unbounded_channel();
        event_sender
            .send(RuntimeEvent::NavigationCompleted {
                outcome: NavigationOutcome::Unchanged {
                    request_prefill: None,
                },
            })
            .unwrap();
        let (_progress_sender, mut progress) = mpsc::channel(8);
        let mut clipboard = FakeClipboard::available();

        let redraws = super::run_loop(
            &mut terminal,
            &mut app,
            &mut input,
            &commands,
            &mut events,
            &mut progress,
            &mut clipboard,
        )
        .unwrap();

        assert_eq!(redraws, 2);
        assert_eq!(
            received.try_recv().unwrap(),
            RuntimeCommand::Navigate {
                entry: SessionEntryId(3)
            }
        );
        assert_eq!(received.try_recv().unwrap(), RuntimeCommand::Shutdown);
        assert!(app.tree_navigator.is_none());
        assert!(!app.manual_navigation_pending);
        let screen = terminal
            .backend()
            .buffer()
            .content
            .chunks(72)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Status: idle"), "{screen}");
        assert!(!screen.contains("Session tree"), "{screen}");
    }

    #[test]
    fn tree_navigation_preserves_pending_drafts_and_replaces_the_projection() {
        let mut app = app_with_history(App::default());
        rendered(&mut app, 80, 12);
        app.history_position = HistoryPosition::Anchored(0);
        app.selection = Some(Selection::new(
            DocumentPosition::new(0, 0),
            DocumentPosition::new(0, 1),
        ));
        select_tree_request(&mut app);

        type_prompt(&mut app, "draft");
        assert!(app.handle_key(shifted_key(KeyCode::Enter)).visible_changed);
        assert!(app.handle_paste("after selection").visible_changed);
        let blocked = app.handle_key(key(KeyCode::Enter));
        assert!(blocked.commands.is_empty());
        assert_eq!(
            app.notice(),
            Some("Session restoration is still in progress.")
        );
        assert_eq!(app.prompt.text(), "draft\nafter selection");

        assert!(app.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Replaced {
                state: restored_state("restored branch"),
                request_prefill: Some("prefill must not replace a draft".to_owned()),
            },
        }));
        assert!(!app.manual_navigation_pending);
        assert_eq!(app.notice(), None);
        assert_eq!(app.prompt.text(), "draft\nafter selection");
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(app.history_viewport.is_none());
        assert!(app.selection.is_none());
        let screen = rendered(&mut app, 80, 30);
        assert!(screen.contains("restored branch"), "{screen}");
        assert!(screen.contains("Session restored."), "{screen}");
        assert!(!screen.contains("request 8"), "{screen}");
    }

    #[test]
    fn tree_navigation_applies_request_prefill_only_after_each_outcome() {
        let mut replaced = App::default();
        select_tree_request(&mut replaced);
        assert!(replaced.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Replaced {
                state: restored_state("replacement"),
                request_prefill: Some("selected request".to_owned()),
            },
        }));
        assert_eq!(replaced.prompt.text(), "selected request");
        assert!(rendered(&mut replaced, 80, 12).contains("Session restored."));

        let mut non_request = App::default();
        select_tree_request(&mut non_request);
        assert!(non_request.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Replaced {
                state: restored_state("replacement"),
                request_prefill: None,
            },
        }));
        assert!(non_request.prompt.is_empty());

        let mut unchanged = app_with_history(App::default());
        select_tree_request(&mut unchanged);
        unchanged.set_notice("Session restoration is still in progress.".to_owned());
        assert!(unchanged.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Unchanged {
                request_prefill: Some("current request".to_owned()),
            },
        }));
        assert!(!unchanged.manual_navigation_pending);
        assert_eq!(unchanged.notice(), None);
        assert_eq!(unchanged.prompt.text(), "current request");
        let screen = rendered(&mut unchanged, 80, 12);
        assert!(screen.contains("request 8"), "{screen}");
        assert!(!screen.contains("Session restored."), "{screen}");

        for (outcome, notice) in [
            (
                NavigationOutcome::RejectedBusy,
                "Session tree is unavailable while work is active.",
            ),
            (
                NavigationOutcome::RejectedReadOnly,
                "Session tree is unavailable while the session is read-only.",
            ),
            (NavigationOutcome::Failed, "Session could not be restored."),
        ] {
            let mut app = app_with_history(App::default());
            select_tree_request(&mut app);
            assert!(app.handle_event(RuntimeEvent::NavigationCompleted { outcome }));
            assert!(!app.manual_navigation_pending);
            assert_eq!(app.notice(), Some(notice));
            let screen = rendered(&mut app, 80, 12);
            assert!(screen.contains("request 8"), "{screen}");
            assert!(!screen.contains("Session restored."), "{screen}");
        }
    }

    #[test]
    fn tree_navigation_rebuilds_the_subagent_monitor_from_the_restored_timeline() {
        let mut app = App::default();
        select_tree_request(&mut app);
        let task = TaskRecord::for_test(
            "00",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let mut state = restored_state("restored request");
        state
            .timeline
            .push(RestoredTimelineItem::SubagentSpawned(task.clone()));
        state
            .timeline
            .push(RestoredTimelineItem::SubagentFinished(task));

        assert!(app.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Replaced {
                state,
                request_prefill: None,
            },
        }));
        let screen = rendered(&mut app, 80, 20);
        assert!(screen.contains("✓ subagent:00"), "{screen}");
        assert!(screen.contains("Session restored."), "{screen}");
    }

    #[test]
    fn tree_navigation_rebuilds_model_change_cards_for_the_selected_branch() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let current = app.settings_selection;
        let terra_high = ModelSelection::new(&TERRA, "high").unwrap();

        select_tree_request(&mut app);
        assert!(app.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Replaced {
                state: branch_state(&[current, terra_high]),
                request_prefill: None,
            },
        }));
        assert_eq!(app.settings_selection, current);
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(
            rows[rule - 2],
            "Model selected: openai/gpt-6.1-sol · medium"
        );
        let rows = history_rows(&mut app);
        let terra_card = rows
            .iter()
            .position(|row| row == "Model selected: openai/gpt-5.6-terra · high")
            .expect("history card");
        assert!(rows[terra_card + 1].contains("request 2"), "{rows:?}");
        assert_eq!(rows.last().map(String::as_str), Some("Session restored."));

        select_tree_request(&mut app);
        assert!(app.handle_event(RuntimeEvent::NavigationCompleted {
            outcome: NavigationOutcome::Replaced {
                state: branch_state(&[terra_high, current]),
                request_prefill: None,
            },
        }));
        assert_eq!(app.settings_selection, current);
        let (_, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(history_bottom(&app), rule);
        let rows = history_rows(&mut app);
        let cards = rows
            .iter()
            .filter(|row| row.contains("Model selected"))
            .collect::<Vec<_>>();
        assert_eq!(cards, ["Model selected: openai/gpt-6.1-sol · medium"]);
        assert_eq!(rows.last().map(String::as_str), Some("Session restored."));
        let screen = rendered(&mut app, 80, 40);
        assert!(screen.contains("gpt-6.1-sol • thinking medium"), "{screen}");
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
    fn ctrl_p_opens_model_picker_during_an_active_run_without_consuming_the_draft() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        set_prompt(&mut app, "keep this draft");
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });

        let update =
            app.handle_key_at(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL), now);

        assert!(update.commands.is_empty());
        assert!(update.visible_changed);
        assert_eq!(app.prompt.text(), "keep this draft");
        assert!(app.picker.is_some());
        assert!(rendered(&mut app, 80, 15).contains("Models"));
    }

    fn app_with_favourites(favourites: &str) -> App {
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        fs::create_dir(home.path().join(".kiki")).unwrap();
        fs::write(
            home.path().join(".kiki/config.toml"),
            format!("favourites = {favourites}\n"),
        )
        .unwrap();
        App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ))
    }

    fn model_picker_rows(app: &mut App) -> Vec<String> {
        app.open_model_picker("");
        let screen = rendered(app, 80, 45);
        screen
            .lines()
            .skip_while(|line| !line.contains("Filter:"))
            .skip(1)
            .take_while(|line| !line.contains('└'))
            .map(|line| line.trim().trim_matches('│').trim().to_owned())
            .collect()
    }

    #[test]
    fn model_picker_lists_favourites_then_every_model_under_its_provider() {
        let mut app = app_with_favourites(
            "[\"anthropic/claude-fable-5-1\", \"openai/gpt-6-luna\", \"openai/gpt-6-sol\"]",
        );

        assert_eq!(
            model_picker_rows(&mut app),
            [
                "Favourites",
                "anthropic/claude-fable-5-1",
                "openai/gpt-6-luna",
                "openai/gpt-6-sol",
                "OpenAI",
                "openai/gpt-6-astra",
                "openai/gpt-6.1-sol",
                "openai/gpt-6-sol",
                "openai/gpt-6-luna",
                "openai/gpt-5.6-sol",
                "openai/gpt-5.6-terra",
                "openai/gpt-5.6-luna",
                "Anthropic",
                "anthropic/claude-opus-5-5",
                "anthropic/claude-sonnet-5-5",
                "anthropic/claude-fable-5-1",
                "Google",
                "google/gemini-4-argon",
                "google/gemini-3.8-flash",
            ]
        );
    }

    #[test]
    fn model_picker_leaves_out_keyless_models_and_their_empty_groups() {
        let mut app = app_with_favourites(
            "[\"anthropic/claude-fable-5-1\", \"openai/gpt-6-luna\", \"openai/gpt-6-sol\"]",
        );
        app.providers = vec![Provider::OpenAi];

        assert_eq!(
            model_picker_rows(&mut app),
            [
                "Favourites",
                "openai/gpt-6-luna",
                "openai/gpt-6-sol",
                "OpenAI",
                "openai/gpt-6-astra",
                "openai/gpt-6.1-sol",
                "openai/gpt-6-sol",
                "openai/gpt-6-luna",
                "openai/gpt-5.6-sol",
                "openai/gpt-5.6-terra",
                "openai/gpt-5.6-luna",
            ]
        );

        let mut app =
            app_with_favourites("[\"google/gemini-4-argon\", \"anthropic/claude-fable-5-1\"]");
        app.providers = vec![Provider::OpenAi, Provider::Anthropic];
        let rows = model_picker_rows(&mut app);
        assert_eq!(
            rows.iter().take(2).map(String::as_str).collect::<Vec<_>>(),
            ["Favourites", "anthropic/claude-fable-5-1"]
        );
        assert!(!rows.iter().any(|row| row == "Google"), "{rows:?}");
        assert!(
            !rows.iter().any(|row| row.starts_with("google/")),
            "{rows:?}"
        );

        let mut app = app_with_favourites("[\"anthropic/claude-fable-5-1\"]");
        app.providers = vec![Provider::OpenAi];
        let rows = model_picker_rows(&mut app);
        assert_eq!(rows.first().map(String::as_str), Some("OpenAI"));
        assert!(!rows.iter().any(|row| row == "Favourites"), "{rows:?}");
    }

    #[test]
    fn keyless_model_command_names_the_missing_key_and_keeps_the_selection() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.providers = vec![Provider::OpenAi];
        let before = app.settings_selection;

        for (model_id, variable) in [
            (OPUS_5_5.id, "ANTHROPIC_API_KEY"),
            ("google/gemini-4-argon", "GEMINI_API_KEY"),
        ] {
            let update = enter_at(&mut app, &format!("/model {model_id}"), now);

            assert!(update.commands.is_empty(), "{:?}", update.commands);
            assert!(update.visible_changed);
            assert!(app.prompt.is_empty());
            assert_eq!(app.settings_selection, before);
            let notice = app.notice().expect("keyless model notice");
            assert!(notice.contains(model_id), "{notice}");
            assert!(notice.contains(variable), "{notice}");
        }
    }

    #[test]
    fn astra_is_first_and_uses_its_model_owned_thinking_levels() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        assert!(std::ptr::eq(app.settings_selection.model(), &GPT6_1_SOL));

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
    fn new_model_commands_apply_their_default_thinking() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        for (id, thinking) in [
            ("openai/gpt-6.1-sol", "medium"),
            ("anthropic/claude-sonnet-5-5", "high"),
        ] {
            let update = enter_at(&mut app, &format!("/model {id}"), now);
            let (selection, _) = applied_settings(&update);
            assert_eq!(selection.model().id, id);
            assert_eq!(selection.thinking(), thinking);
            assert!(app.prompt.is_empty());
        }
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
        assert!(notice.contains(GPT6_1_SOL.id));
        assert!(notice.contains("low, medium, high, xhigh, max"));
    }

    #[test]
    fn unrecognised_or_multiline_slash_input_remains_an_ordinary_prompt() {
        let now = Instant::now();
        for draft in ["/future task", "/tree-more", "/model\nterra", "/tree\nauth"] {
            let mut app = App::default();
            assert_eq!(
                enter_at(&mut app, draft, now).commands,
                [RuntimeCommand::Submit {
                    request: draft.to_owned(),
                    activation: None,
                }],
                "{draft}"
            );
        }
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
            [RuntimeCommand::Cancel { correction: false }]
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
        let sol = enter_at(&mut app, "/model openai/gpt-6.1-sol", now);
        let (selection, _) = applied_settings(&sol);
        assert!(std::ptr::eq(selection.model(), &GPT6_1_SOL));
        assert_eq!(selection.thinking(), "low");
        assert!(app.preview.is_none());
    }

    #[test]
    fn model_switch_ignores_unacknowledged_thinking() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let (selection, _) = applied_settings(&enter_at(&mut app, "/thinking high", now));
        assert!(std::ptr::eq(selection.model(), &GPT6_1_SOL));
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));

        let (selection, _) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        assert!(std::ptr::eq(selection.model(), &TERRA));

        let (selection, _) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-6.1-sol", now));
        assert!(std::ptr::eq(selection.model(), &GPT6_1_SOL));
        assert_eq!(selection.thinking(), "medium");
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
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));

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

        let (selection, _) =
            applied_settings(&enter_at(&mut app, "/model google/gemini-4-argon", now));
        assert_eq!(selection.model().id, "google/gemini-4-argon");
        assert_eq!(selection.thinking(), "medium");
        for expected in ["high", "low", "medium", "high"] {
            let update = app.handle_key_at(key(KeyCode::BackTab), now);
            assert!(update.commands.is_empty());
            assert_eq!(app.settings_selection.thinking(), expected);
        }
        for level in ["low", "medium", "high"] {
            let (selection, _) =
                applied_settings(&enter_at(&mut app, &format!("/thinking {level}"), now));
            assert_eq!(selection.thinking(), level);
        }
        let unsupported = enter_at(&mut app, "/thinking xhigh", now);
        assert!(unsupported.commands.is_empty());
        let notice = app.notice().expect("unsupported google thinking notice");
        assert!(notice.contains("google/gemini-4-argon"), "{notice}");
        assert!(notice.contains("low, medium, high"), "{notice}");
        assert!(!notice.contains("xhigh"), "{notice}");
    }

    #[test]
    fn pending_model_change_card_sits_above_the_prompt_until_settings_apply() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let selection = app.settings_selection;
        append_completed_run(&mut app, 1, selection);

        let (_, revision) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(
            rows[rule - 2],
            "[pending] Model selected: openai/gpt-5.6-terra · medium"
        );
        assert!(rows[rule - 1].is_empty() && rows[rule - 3].is_empty());
        assert_eq!(history_bottom(&app), rule - 3);
        let buffer = rendered_buffer(&mut app, 80, 40);
        let (column, row) = cell_start(&buffer, "[pending]");
        assert_eq!(buffer[(column, row)].fg, app.theme.muted);

        assert!(app.handle_event(RuntimeEvent::SettingsAccepted { revision }));
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(
            rows[rule - 2],
            "Model selected: openai/gpt-5.6-terra · medium"
        );
        assert!(
            !history_rows(&mut app)
                .iter()
                .any(|row| row.contains("Model selected"))
        );
    }

    #[test]
    fn shift_tab_marks_the_pending_model_change_card_until_the_preview_applies() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let selection = app.settings_selection;
        append_completed_run(&mut app, 1, selection);
        select_history_text(&mut app, "request 1", 80, 40);
        assert!(app.selection.is_some());

        assert!(
            app.handle_key_at(key(KeyCode::BackTab), now)
                .visible_changed
        );
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(
            rows[rule - 2],
            "[pending] Model selected: openai/gpt-6.1-sol · high"
        );
        assert!(app.selection.is_none());

        let (_, revision) = applied_settings(&app.tick(now + Duration::from_secs(2)));
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(
            rows[rule - 2],
            "[pending] Model selected: openai/gpt-6.1-sol · high"
        );

        assert!(app.handle_event(RuntimeEvent::SettingsAccepted { revision }));
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(rows[rule - 2], "Model selected: openai/gpt-6.1-sol · high");
    }

    #[test]
    fn pending_model_change_card_sits_between_the_subagent_monitor_and_the_throbber() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let [request, _] = completed_run_records(1, app.settings_selection);
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: request,
                progress_gap: false,
            },
            now,
        );
        app.handle_event_at(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            now,
        );
        app.subagent_monitor
            .insert_for_test("child1", TaskState::Running, now);

        applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert!(rows[rule - 1].starts_with('⠁'), "{rows:?}");
        assert!(rows[rule - 2].is_empty());
        assert_eq!(
            rows[rule - 4],
            "[pending] Model selected: openai/gpt-5.6-terra · medium"
        );
        let monitor = rows
            .iter()
            .position(|row| row.contains("subagent:child1"))
            .expect("subagent monitor");
        assert!(
            history_bottom(&app) <= monitor && monitor < rule - 5,
            "{rows:?}"
        );
    }

    #[test]
    fn pending_model_change_card_stays_put_while_history_scrolls() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let selection = app.settings_selection;
        for run_id in 1..=20 {
            append_completed_run(&mut app, run_id, selection);
        }
        let (_, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(history_bottom(&app), rule);
        let hidden_height = app.history_viewport.expect("history viewport").area.height;

        applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        let (following, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(
            app.history_viewport.expect("history viewport").area.height,
            hidden_height - 3
        );
        assert_eq!(history_bottom(&app), rule - 3);
        assert_eq!(
            following[rule - 2],
            "[pending] Model selected: openai/gpt-5.6-terra · medium"
        );
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        assert!(app.handle_key_at(key(KeyCode::PageUp), now).visible_changed);
        let (scrolled, scrolled_rule) = rows_and_prompt_rule(&mut app);
        assert_ne!(app.history_position, HistoryPosition::FollowingBottom);
        assert_eq!(scrolled_rule, rule);
        assert_ne!(scrolled[..rule - 3], following[..rule - 3]);
        assert_eq!(scrolled[rule - 3..], following[rule - 3..]);
    }

    #[test]
    fn pending_model_change_card_clears_when_switched_back_and_settles_into_history() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let selection = app.settings_selection;
        append_completed_run(&mut app, 1, selection);

        applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        let (_, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(history_bottom(&app), rule - 3);

        applied_settings(&enter_at(&mut app, "/model openai/gpt-6.1-sol", now));
        let (rows, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(history_bottom(&app), rule);
        assert!(!rows.iter().any(|row| row.contains("Model selected")));

        let (terra, _) = applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        append_completed_run(&mut app, 2, terra);
        let (_, rule) = rows_and_prompt_rule(&mut app);
        assert_eq!(history_bottom(&app), rule);
        let rows = history_rows(&mut app);
        let cards = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.contains("Model selected"))
            .collect::<Vec<_>>();
        let second_request = rows
            .iter()
            .position(|row| row.contains("request 2"))
            .expect("second request");
        assert_eq!(
            cards,
            [(
                second_request - 1,
                &"Model selected: openai/gpt-5.6-terra · medium".to_owned()
            )]
        );
    }

    #[test]
    fn pending_model_change_card_leaves_at_least_one_history_row() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let selection = app.settings_selection;
        append_completed_run(&mut app, 1, selection);
        app.apply_selection(ModelSelection::new(&TERRA, "high").unwrap());

        assert!(!rendered(&mut app, 80, 7).contains("Model selected"));
        assert_eq!(app.history_height, 3);
        assert!(rendered(&mut app, 80, 8).contains("[pending] Model selected"));
        assert_eq!(app.history_height, 1);
    }

    #[test]
    fn telemetry_brackets_tui_cancellation_command_queueing() {
        let directory = tempdir().expect("trace directory");
        let trace_file = directory.path().join("trace.jsonl");
        let trace_owner = TraceOwner::open(&trace_file).expect("open trace");
        let (sender, mut receiver) = mpsc::channel(3);
        let update = super::Update::with_commands([
            RuntimeCommand::Cancel { correction: false },
            RuntimeCommand::CancelBackground,
            RuntimeCommand::Shutdown,
        ]);

        super::dispatch_commands(&sender, &update, &trace_owner.handle())
            .expect("dispatch cancellation commands");
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::Cancel { correction: false })
        ));
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
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));
        assert!(app.handle_event(RuntimeEvent::SettingsAccepted { revision }));
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"high"));

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
    fn same_model_stale_acknowledgements_do_not_publish_an_obsolete_choice() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let (_, first) = applied_settings(&enter_at(&mut app, "/thinking high", now));
        let (_, second) = applied_settings(&enter_at(&mut app, "/thinking low", now));

        assert_eq!(app.settings_selection.thinking(), "low");
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));
        assert!(!app.handle_event(RuntimeEvent::SettingsAccepted { revision: first }));
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));
        assert!(!app.handle_event(RuntimeEvent::SettingsAccepted { revision: first }));
        assert!(app.handle_event(RuntimeEvent::SettingsAccepted { revision: second }));
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"low"));
    }

    #[test]
    fn cross_model_acknowledgements_are_retained_when_switching_back() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        let (_, sol_revision) = applied_settings(&enter_at(&mut app, "/thinking high", now));
        let (terra, terra_revision) =
            applied_settings(&enter_at(&mut app, "/model openai/gpt-5.6-terra", now));
        assert_eq!(terra.thinking(), TERRA.default_thinking);

        assert!(app.handle_event(RuntimeEvent::SettingsAccepted {
            revision: sol_revision,
        }));
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"high"));
        assert!(app.handle_event(RuntimeEvent::SettingsAccepted {
            revision: terra_revision,
        }));
        assert_eq!(
            app.session_thinking.get(TERRA.id),
            Some(&TERRA.default_thinking)
        );

        let (sol, _) = applied_settings(&enter_at(&mut app, "/model openai/gpt-6.1-sol", now));
        assert_eq!(sol.thinking(), "high");
    }

    #[test]
    fn sent_acknowledgement_applies_while_newer_preview_remains_unsent() {
        let now = Instant::now();
        let mut app = App::configured(crate::config::ready_snapshot_for_test());
        app.handle_key_at(key(KeyCode::BackTab), now);
        let (_, revision) = applied_settings(&app.tick(now + Duration::from_secs(2)));
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));
        app.handle_key_at(key(KeyCode::BackTab), now + Duration::from_secs(2));

        assert!(app.handle_event(RuntimeEvent::SettingsAccepted { revision }));

        assert_eq!(app.settings_selection.thinking(), "xhigh");
        assert!(app.preview.is_some());
        assert!(app.pending_settings.is_empty());
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"high"));

        let (_, preview_revision) = applied_settings(&app.tick(now + Duration::from_secs(4)));
        assert!(app.handle_event(RuntimeEvent::SettingsAccepted {
            revision: preview_revision,
        }));

        assert!(app.pending_settings.is_empty());
        assert!(app.preview.is_none());
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"xhigh"));
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
            RuntimeCommand::Submit {
                request,
                activation: None,
            },
        ] = update.commands.as_slice()
        else {
            panic!("expected settings then submit, got {:?}", update.commands);
        };
        assert_eq!(selection.thinking(), "high");
        assert_eq!(*revision, 1);
        assert_eq!(request, "use the visible setting");
        assert!(app.preview.is_none());
        assert_eq!(app.session_thinking.get(GPT6_1_SOL.id), Some(&"medium"));

        let (sender, mut receiver) = mpsc::channel(2);
        super::dispatch_commands(&sender, &update, &Trace::disabled()).expect("dispatch commands");
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::ApplySettings { .. })
        ));
        assert!(matches!(
            receiver.try_recv(),
            Ok(RuntimeCommand::Submit {
                request,
                activation: None,
            }) if request == "use the visible setting"
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
        assert!(std::ptr::eq(selection.model(), &GPT6_SOL));
        assert!(
            app.notice()
                .is_some_and(|notice| notice.contains("User default saved"))
        );
        assert!(
            fs::read_to_string(home.path().join(".kiki/config.toml"))
                .expect("model setting")
                .contains("model = \"openai/gpt-6-sol\"")
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
                .contains("\"openai/gpt-6-sol\" = \"high\"")
        );

        fs::create_dir_all(workspace.path().join(".kiki")).expect("project settings directory");
        let project_settings = workspace.path().join(".kiki/config.toml");
        fs::write(&project_settings, "model = \"openai/gpt-5.6-terra\"\n")
            .expect("project settings");
        let project_before = fs::read(&project_settings).expect("read project settings");
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
        assert_eq!(
            fs::read(&project_settings).expect("read project settings"),
            project_before
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
            [RuntimeCommand::Cancel { correction: false }]
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
                request: "hi".to_owned(),
                activation: None,
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
                activation: None,
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
                activation: None,
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
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
        });
        set_prompt(&mut disabled, "locked");
        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
            shifted_key(KeyCode::Enter),
            modified_key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        ] {
            assert!(!disabled.handle_key(event).visible_changed);
        }
        assert_eq!(disabled.prompt.text(), "locked");
        assert!(disabled.picker.is_none());

        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, "locked");
        for event in [
            key(KeyCode::Char('x')),
            key(KeyCode::Backspace),
            key(KeyCode::Enter),
            shifted_key(KeyCode::Enter),
            modified_key(KeyCode::Char('j'), KeyModifiers::CONTROL),
            modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL),
        ] {
            assert!(!configuration.handle_key(event).visible_changed);
        }
        assert_eq!(configuration.prompt.text(), "locked");
        assert!(configuration.picker.is_none());

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
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
    fn prompt_mouse_selection_preserves_unicode_newlines_and_soft_wraps() {
        let mut app = app_with_prompt("ab界cd\nsecond line");
        let (_, position) = cursor(&mut app, 10, 18);
        let now = Instant::now();
        // Select backwards from the visible tail to the start of its preceding row.
        let start_row = position.y.saturating_sub(1);
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                position.x,
                position.y,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 2, start_row),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 2, start_row),
            now,
        );
        let mut clipboard = FakeClipboard::available();
        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(clipboard.writes, ["second line"]);
        app.handle_mouse(
            mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                position.x,
                position.y,
            ),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 2, start_row - 1),
            now,
        );
        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(clipboard.writes[1], "ab界cd\nsecond line");

        let mut unicode = app_with_prompt("a界e\u{301}z");
        cursor(&mut unicode, 20, 12);
        unicode.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 10),
            now,
        );
        unicode.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 10),
            now,
        );
        type_prompt(&mut unicode, "X");
        assert_eq!(unicode.prompt.text(), "aX界e\u{301}z");
    }

    #[test]
    fn prompt_mouse_click_is_ignored_when_input_is_disabled() {
        let mut app = app_with_prompt("hello");
        cursor(&mut app, 20, 12);
        app.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
        });
        let now = Instant::now();
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 10),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 2, 10),
            now,
        );
        assert!(app.prompt.caret_at_end());
        let mut clipboard = FakeClipboard::available();
        assert!(!app.finish_pending_copy(&mut clipboard, now));
    }

    #[test]
    fn prompt_mouse_click_does_not_copy_and_editing_clears_selection() {
        let mut app = app_with_prompt("hello");
        cursor(&mut app, 20, 12);
        let now = Instant::now();
        let mut clipboard = FakeClipboard::available();
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 3, 10),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 3, 10),
            now,
        );
        assert!(!app.finish_pending_copy(&mut clipboard, now));
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 10),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 5, 10),
            now,
        );
        app.finish_pending_copy(&mut clipboard, now);
        type_prompt(&mut app, "X");
        let buffer = rendered_buffer(&mut app, 20, 12);
        assert!(!buffer[(2, 10)].modifier.contains(Modifier::REVERSED));
        assert_eq!(app.prompt.text(), "helXlo");
    }

    #[test]
    fn prompt_mouse_drag_copies_exact_text_on_release() {
        let mut app = app_with_prompt("alpha beta");
        cursor(&mut app, 20, 12);
        let now = Instant::now();
        let mut clipboard = FakeClipboard::available();
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 2, 10),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Drag(MouseButton::Left), 7, 10),
            now,
        );
        let buffer = rendered_buffer(&mut app, 20, 12);
        assert!(buffer[(2, 10)].modifier.contains(Modifier::REVERSED));
        assert!(!app.finish_pending_copy(&mut clipboard, now));
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 7, 10),
            now,
        );
        assert!(app.finish_pending_copy(&mut clipboard, now));
        assert_eq!(clipboard.writes, ["alpha"]);
        assert_eq!(app.prompt.text(), "alpha beta");
        assert_eq!(app.notice(), Some("Copied"));
    }

    #[test]
    fn prompt_mouse_click_places_the_cursor_for_insertion() {
        let mut app = app_with_prompt("abcdef");
        cursor(&mut app, 20, 12);
        let now = Instant::now();
        app.handle_mouse(
            mouse_event(MouseEventKind::Down(MouseButton::Left), 4, 10),
            now,
        );
        app.handle_mouse(
            mouse_event(MouseEventKind::Up(MouseButton::Left), 4, 10),
            now,
        );
        type_prompt(&mut app, "X");
        assert_eq!(submitted_prompt(&mut app), "abXcdef");
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
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
                        selection: None,
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
                        selection: None,
                    },
                },
                progress_gap: false,
            });
        }
        rendered(&mut app, 20, 7);
        app.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
    fn theme_setting_controls_user_card_background() {
        use crate::theme::{Appearance, LIGHT};
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        fs::create_dir(home.path().join(".kiki")).unwrap();
        for (setting, appearance, expected) in [
            ("light", Appearance::Dark, LIGHT.user_message_bg),
            ("automatic", Appearance::Light, LIGHT.user_message_bg),
            ("automatic", Appearance::Dark, DARK.user_message_bg),
        ] {
            fs::write(
                home.path().join(".kiki/config.toml"),
                format!("theme = {setting:?}\n"),
            )
            .unwrap();
            let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
                Some(home.path()),
                workspace.path(),
            ))
            .with_appearance(appearance);
            app.handle_event(RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "themed user card".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            });
            let buffer = rendered_buffer(&mut app, 80, 20);
            assert_eq!(
                buffer[cell_start(&buffer, "themed user card")].bg,
                expected,
                "{setting} {appearance:?}"
            );
        }
    }

    #[test]
    fn theme_picker_lists_items_previews_live_and_restores_on_escape() {
        use crate::theme::{Appearance, LIGHT};

        let now = Instant::now();
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        fs::create_dir(home.path().join(".kiki")).unwrap();
        fs::write(home.path().join(".kiki/config.toml"), "theme = \"dark\"\n").unwrap();
        let mut light_appearance =
            App::configured(crate::config::ready_snapshot_with_settings_for_test(
                Some(home.path()),
                workspace.path(),
            ))
            .with_appearance(Appearance::Light);
        let _ = enter_at(&mut light_appearance, "/theme", now);
        let light_buffer = rendered_buffer(&mut light_appearance, 80, 20);
        let (auto_col, auto_row) = cell_start(&light_buffer, "automatic (currently light)");
        let (_, dark_row) = cell_start(&light_buffer, "│dark");
        let (_, light_row) = cell_start(&light_buffer, "│light");
        assert!(auto_row < dark_row && dark_row < light_row);
        assert_eq!(light_buffer[(auto_col, dark_row)].bg, DARK.selected_bg);
        assert_ne!(light_buffer[(auto_col, auto_row)].bg, DARK.selected_bg);

        let clean_home = tempdir().unwrap();
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(clean_home.path()),
            workspace.path(),
        ))
        .with_appearance(Appearance::Dark);
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "settled user card".to_owned(),
                    selection: None,
                },
            },
            progress_gap: false,
        });
        let initial = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            initial[cell_start(&initial, "settled user card")].bg,
            DARK.user_message_bg
        );

        let open = enter_at(&mut app, "/theme", now);
        assert!(open.commands.is_empty());
        assert!(open.visible_changed);
        let opened = rendered_buffer(&mut app, 80, 20);
        let (auto_dark_col, auto_dark_row) = cell_start(&opened, "automatic (currently dark)");
        assert_eq!(opened[(auto_dark_col, auto_dark_row)].bg, DARK.selected_bg);

        assert!(app.handle_key_at(key(KeyCode::Down), now).visible_changed);
        assert!(app.handle_key_at(key(KeyCode::Down), now).visible_changed);
        let preview_light = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            preview_light[cell_start(&preview_light, "settled user card")].bg,
            LIGHT.user_message_bg
        );

        assert!(app.handle_key_at(key(KeyCode::Up), now).visible_changed);
        let preview_dark = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            preview_dark[cell_start(&preview_dark, "settled user card")].bg,
            DARK.user_message_bg
        );

        assert!(
            app.handle_key_at(key(KeyCode::Char('g')), now)
                .visible_changed
        );
        let filtered_light = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            filtered_light[cell_start(&filtered_light, "settled user card")].bg,
            LIGHT.user_message_bg
        );

        assert!(app.handle_key_at(key(KeyCode::Esc), now).visible_changed);
        assert!(app.picker.is_none());
        let restored = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            restored[cell_start(&restored, "settled user card")].bg,
            DARK.user_message_bg
        );
        assert!(!clean_home.path().join(".kiki/config.toml").exists());
    }

    #[test]
    fn theme_picker_enter_and_ctrl_s_save_and_report_masking_or_save_errors() {
        use crate::theme::{Appearance, LIGHT};

        let now = Instant::now();
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ))
        .with_appearance(Appearance::Dark);
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "settled user card".to_owned(),
                    selection: None,
                },
            },
            progress_gap: false,
        });

        let _ = enter_at(&mut app, "/theme", now);
        app.handle_key_at(key(KeyCode::End), now);
        let apply = app.handle_key_at(key(KeyCode::Enter), now);
        assert!(apply.commands.is_empty());
        assert!(apply.visible_changed);
        assert!(app.picker.is_none());
        assert_eq!(app.notice(), Some("User default saved."));
        assert_eq!(
            fs::read_to_string(home.path().join(".kiki/config.toml")).unwrap(),
            "theme = \"light\"\n"
        );
        let after_enter = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            after_enter[cell_start(&after_enter, "settled user card")].bg,
            LIGHT.user_message_bg
        );

        let _ = enter_at(&mut app, "/theme", now);
        app.handle_key_at(key(KeyCode::Up), now);
        let save = app.handle_key_at(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            now,
        );
        assert!(save.commands.is_empty());
        assert!(save.visible_changed);
        assert!(app.picker.is_none());
        assert_eq!(app.notice(), Some("User default saved."));
        assert_eq!(
            fs::read_to_string(home.path().join(".kiki/config.toml")).unwrap(),
            "theme = \"dark\"\n"
        );
        let after_ctrl_s = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            after_ctrl_s[cell_start(&after_ctrl_s, "settled user card")].bg,
            DARK.user_message_bg
        );

        fs::create_dir_all(workspace.path().join(".kiki")).unwrap();
        fs::write(
            workspace.path().join(".kiki/config.toml"),
            "theme = \"dark\"\n",
        )
        .unwrap();
        let mut masked = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ))
        .with_appearance(Appearance::Dark);
        let _ = enter_at(&mut masked, "/theme", now);
        masked.handle_key_at(key(KeyCode::End), now);
        masked.handle_key_at(key(KeyCode::Enter), now);
        assert!(
            masked
                .notice()
                .is_some_and(|notice| notice.contains("overridden by project settings"))
        );
        assert!(std::ptr::eq(masked.theme, &LIGHT));
        assert_eq!(
            fs::read_to_string(home.path().join(".kiki/config.toml")).unwrap(),
            "theme = \"light\"\n"
        );

        fs::write(home.path().join(".kiki/config.toml"), "theme = [").unwrap();
        let _ = enter_at(&mut masked, "/theme", now);
        masked.handle_key_at(key(KeyCode::Home), now);
        masked.handle_key_at(key(KeyCode::Enter), now);
        assert!(
            masked
                .notice()
                .is_some_and(|notice| notice.starts_with("Could not save user default:"))
        );
        assert!(std::ptr::eq(masked.theme, &DARK));
    }

    #[test]
    fn settings_picker_opens_theme_picker_and_returns_on_enter_ctrl_s_or_escape() {
        use crate::theme::{Appearance, LIGHT};

        let now = Instant::now();
        let home = tempdir().unwrap();
        let workspace = tempdir().unwrap();
        let mut app = App::configured(crate::config::ready_snapshot_with_settings_for_test(
            Some(home.path()),
            workspace.path(),
        ))
        .with_appearance(Appearance::Dark);
        app.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "settled user card".to_owned(),
                    selection: None,
                },
            },
            progress_gap: false,
        });

        let open = enter_at(&mut app, "/settings", now);
        assert!(open.commands.is_empty());
        assert!(open.visible_changed);
        let settings_screen = rendered(&mut app, 80, 20);
        assert!(settings_screen.contains("Settings"), "{settings_screen}");
        assert!(
            settings_screen.contains("Theme  automatic"),
            "{settings_screen}"
        );

        app.handle_key_at(key(KeyCode::Enter), now);
        let theme_screen = rendered(&mut app, 80, 20);
        assert!(
            theme_screen.contains("automatic (currently dark)"),
            "{theme_screen}"
        );

        app.handle_key_at(key(KeyCode::End), now);
        let preview = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            preview[cell_start(&preview, "settled user card")].bg,
            LIGHT.user_message_bg
        );

        app.handle_key_at(key(KeyCode::Esc), now);
        let after_esc = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            after_esc[cell_start(&after_esc, "settled user card")].bg,
            DARK.user_message_bg
        );
        let returned_settings = rendered(&mut app, 80, 20);
        assert!(
            returned_settings.contains("Theme  automatic"),
            "{returned_settings}"
        );
        assert!(!home.path().join(".kiki/config.toml").exists());

        app.handle_key_at(key(KeyCode::Enter), now);
        app.handle_key_at(key(KeyCode::End), now);
        app.handle_key_at(key(KeyCode::Enter), now);
        let updated_settings = rendered(&mut app, 80, 20);
        assert!(
            updated_settings.contains("Theme  light"),
            "{updated_settings}"
        );
        let after_save = rendered_buffer(&mut app, 80, 20);
        assert_eq!(
            after_save[cell_start(&after_save, "settled user card")].bg,
            LIGHT.user_message_bg
        );
        assert_eq!(
            fs::read_to_string(home.path().join(".kiki/config.toml")).unwrap(),
            "theme = \"light\"\n"
        );

        app.handle_key_at(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            now,
        );
        app.handle_key_at(key(KeyCode::Up), now);
        app.handle_key_at(
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            now,
        );
        let ctrl_s_settings = rendered(&mut app, 80, 20);
        assert!(ctrl_s_settings.contains("Theme  dark"), "{ctrl_s_settings}");
        assert_eq!(
            fs::read_to_string(home.path().join(".kiki/config.toml")).unwrap(),
            "theme = \"dark\"\n"
        );

        app.handle_key_at(key(KeyCode::Esc), now);
        assert!(app.picker.is_none());
    }

    #[test]
    fn configuration_configured_mode_renders_a_ready_snapshot_and_remains_editable() {
        let mut app = App::configured(crate::config::ready_snapshot_for_test());

        let history = rendered(&mut app, 120, 17);
        assert!(history.contains("Startup configuration"));
        assert!(history.contains("Submission: available"));
        assert!(history.contains("OpenAI API key: present"));
        assert!(history.contains("Anthropic API key: missing"));
        assert!(history.contains("Google API key: missing"));
        assert!(!history.contains("test-api-key"));

        assert!(app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Submit {
                request: "x".to_owned(),
                activation: None,
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
        let _: fn(
            crate::config::StartupSnapshot,
            crate::theme::Appearance,
        ) -> Result<(), super::TuiError> = super::run_configuration;
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
            [RuntimeCommand::Cancel { correction: false }]
        );
    }

    #[test]
    fn compact_dispatch_waits_for_acknowledgement_to_consume_its_draft() {
        let draft = "/compact  keep the current focus  ";
        let mut app = app_with_prompt(draft);

        assert_eq!(
            app.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: "keep the current focus".to_owned(),
            }]
        );
        assert_eq!(app.prompt.text(), draft);
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert!(!app.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert!(!app.handle_paste(" changed").visible_changed);
        assert!(
            !app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert_eq!(app.prompt.text(), draft);

        assert!(app.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        }));
        assert!(app.prompt.is_empty());

        type_prompt(&mut app, "later draft");
        app.handle_event(RuntimeEvent::CompactionLifecycle {
            lifecycle: CompactionLifecycle::Started,
        });
        app.handle_event(RuntimeEvent::CompactionLifecycle {
            lifecycle: CompactionLifecycle::Settled(CompactionSettlement::Applied),
        });
        assert_eq!(app.prompt.text(), "later draft");
    }

    #[test]
    fn compact_noop_reports_a_notice_without_a_card() {
        let mut app = app_with_prompt("/compact");
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        });

        assert!(app.handle_event(RuntimeEvent::CompactionLifecycle {
            lifecycle: CompactionLifecycle::NoOp,
        }));
        assert_eq!(app.notice(), Some("Nothing to compact."));
        let screen = rendered(&mut app, 120, 24);
        assert!(screen.contains("Nothing to compact."), "{screen}");
        assert!(!screen.contains("Compaction ·"), "{screen}");
    }

    #[test]
    fn compact_busy_and_disabled_acknowledgements_retain_the_draft() {
        let busy_draft = "/compact  retain 🦀  ";
        let mut busy = app_with_prompt(busy_draft);
        assert_eq!(
            busy.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: "retain 🦀".to_owned(),
            }]
        );
        assert!(busy.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Busy,
        }));
        assert_eq!(busy.prompt.text(), busy_draft);
        assert!(busy.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(busy.prompt.text(), format!("{busy_draft}x"));

        let mut active = app_with_prompt("/compact first");
        active.handle_key(key(KeyCode::Enter));
        active.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        });
        let duplicate = "/compact second";
        set_prompt(&mut active, duplicate);
        assert_eq!(
            active.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: "second".to_owned(),
            }]
        );
        assert!(active.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Busy,
        }));
        assert_eq!(active.prompt.text(), duplicate);

        let disabled_draft = "/compact  preserve this  ";
        let mut disabled = app_with_prompt(disabled_draft);
        assert_eq!(
            disabled.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: "preserve this".to_owned(),
            }]
        );
        assert!(disabled.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Disabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
        }));
        assert_eq!(disabled.prompt.text(), disabled_draft);
        assert!(!disabled.handle_key(key(KeyCode::Char('x'))).visible_changed);
        assert_eq!(disabled.prompt.text(), disabled_draft);
    }

    #[test]
    fn compact_dispatches_during_runs_and_corrections_but_not_navigation() {
        let mut running = App::default();
        running.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        set_prompt(&mut running, "/compact  retain the tail  ");
        assert_eq!(
            running.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: "retain the tail".to_owned(),
            }]
        );
        assert_eq!(running.prompt.text(), "/compact  retain the tail  ");

        let mut correction = App {
            correction_pending: true,
            ..App::default()
        };
        set_prompt(&mut correction, "/compact use the restored leaf");
        assert_eq!(
            correction.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: "use the restored leaf".to_owned(),
            }]
        );

        let draft = "/compact keep this draft";
        let mut navigating = App {
            manual_navigation_pending: true,
            ..App::default()
        };
        set_prompt(&mut navigating, draft);
        assert!(
            navigating
                .handle_key(key(KeyCode::Enter))
                .commands
                .is_empty()
        );
        assert_eq!(navigating.prompt.text(), draft);
        assert_eq!(
            navigating.notice(),
            Some("Session restoration is still in progress.")
        );
    }

    #[test]
    fn compact_rejection_without_dispatch_preserves_the_draft() {
        let draft = "/compact  focus  ";
        let mut configuration = App::configuration(crate::config::ready_snapshot_for_test());
        set_prompt(&mut configuration, draft);
        assert!(
            configuration
                .handle_key(key(KeyCode::Enter))
                .commands
                .is_empty()
        );
        assert_eq!(configuration.prompt.text(), draft);

        let mut read_only = App::default();
        read_only.handle_event(RuntimeEvent::RuntimeDisabled {
            reason: RuntimeDisabledReason::SessionRestoreFailed,
        });
        set_prompt(&mut read_only, draft);
        assert!(
            read_only
                .handle_key(key(KeyCode::Enter))
                .commands
                .is_empty()
        );
        assert_eq!(read_only.prompt.text(), draft);
    }

    #[test]
    fn escape_cancels_pending_and_active_compaction_dispatched_from_the_prompt() {
        let mut unacknowledged = app_with_prompt("/compact");
        assert_eq!(
            unacknowledged.handle_key(key(KeyCode::Enter)).commands,
            [RuntimeCommand::Compact {
                focus: String::new(),
            }]
        );
        assert_eq!(
            unacknowledged.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::Cancel { correction: false }]
        );
        unacknowledged.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        });
        assert_eq!(
            unacknowledged.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::CancelBackground]
        );

        let mut pending = app_with_prompt("/compact");
        pending.handle_key(key(KeyCode::Enter));
        pending.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        });
        assert_eq!(
            pending.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::Cancel { correction: false }]
        );

        let mut active = app_with_prompt("/compact retain state");
        active.handle_key(key(KeyCode::Enter));
        active.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        });
        active.handle_event(RuntimeEvent::CompactionLifecycle {
            lifecycle: CompactionLifecycle::Started,
        });
        assert_eq!(
            active.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::Cancel { correction: false }]
        );
    }

    #[test]
    fn cancelling_retained_compaction_does_not_create_a_correction() {
        let submitted_at = Instant::now();
        let mut app = App::default();
        enter_at(&mut app, "request", submitted_at);
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, submitted_at);
        app.handle_event_at(
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: CompactionAcknowledgement::Accepted,
            },
            submitted_at,
        );

        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), submitted_at + Duration::from_millis(1))
                .commands,
            [RuntimeCommand::Cancel { correction: false }]
        );
        assert!(app.foreground_cancellation_requested);
        assert!(!app.correction_pending);

        app.handle_event_at(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Settled(CompactionSettlement::CancelledBeforeStart),
            },
            submitted_at + Duration::from_millis(1),
        );
        assert!(app.foreground_cancellation_requested);
        assert_eq!(app.submission_at, Some(submitted_at));
        assert!(!app.correction_pending);

        app.handle_event_at(
            RuntimeEvent::RunSettled { run_id: RunId(1) },
            submitted_at + Duration::from_millis(2),
        );
        assert!(!app.foreground_cancellation_requested);
        assert_eq!(app.submission_at, None);
        assert!(!app.correction_pending);
    }

    #[test]
    fn cancelling_retained_compaction_preserves_an_existing_correction() {
        let submitted_at = Instant::now();
        let mut app = App::default();
        enter_at(&mut app, "request", submitted_at);
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, submitted_at);
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), submitted_at + Duration::from_millis(1))
                .commands,
            [RuntimeCommand::Cancel { correction: true }]
        );
        assert!(app.correction_pending);
        app.handle_event_at(
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: CompactionAcknowledgement::Accepted,
            },
            submitted_at + Duration::from_millis(1),
        );

        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), submitted_at + Duration::from_millis(2))
                .commands,
            [RuntimeCommand::Cancel { correction: false }]
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), submitted_at + Duration::from_millis(3))
                .commands,
            [RuntimeCommand::CancelBackground]
        );
        app.handle_event_at(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Settled(CompactionSettlement::CancelledBeforeStart),
            },
            submitted_at + Duration::from_millis(2),
        );
        assert!(app.correction_pending);

        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails::empty(),
                    },
                },
                progress_gap: false,
            },
            submitted_at + Duration::from_millis(3),
        );
        assert!(app.handle_event_at(
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: CorrectionOutcome::SkippedBackground,
            },
            submitted_at + Duration::from_millis(4),
        ));
        assert!(app.correction_pending);
        app.handle_event_at(
            RuntimeEvent::RunSettled { run_id: RunId(1) },
            submitted_at + Duration::from_millis(5),
        );
        assert!(!app.correction_pending);
    }

    #[test]
    fn active_compaction_accepts_cancel_but_not_submit() {
        let mut app = App::default();
        assert!(app.handle_event(RuntimeEvent::CompactionAcknowledged {
            acknowledgement: CompactionAcknowledgement::Accepted,
        }));
        assert!(app.handle_event(RuntimeEvent::CompactionLifecycle {
            lifecycle: CompactionLifecycle::Started,
        }));
        app.handle_key(key(KeyCode::Char('x')));

        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::Cancel { correction: false }]
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Esc)).commands,
            [RuntimeCommand::CancelBackground]
        );

        assert!(app.handle_event(RuntimeEvent::CompactionLifecycle {
            lifecycle: CompactionLifecycle::Settled(CompactionSettlement::Cancelled),
        }));
        assert!(!app.handle_key(key(KeyCode::Enter)).commands.is_empty());
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
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
    fn restored_disabled_session_keeps_history_scrollable_and_expandable() {
        let mut records = (1..=8)
            .map(|run_id| StoredRecord {
                run_id: RunId(run_id),
                record: Record::UserRequest {
                    request: format!("saved request {run_id}"),
                    selection: None,
                },
            })
            .collect::<Vec<_>>();
        records.push(StoredRecord {
            run_id: RunId(9),
            record: Record::CompletedExecution {
                step_id: ModelStepId(9),
                commentary: Some("saved arena commentary".to_owned()),
                source: (1..=33)
                    .map(|line| format!("saved arena line {line:02}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                reasoning_summary: String::new(),
                execution: Execution {
                    stdout: observation(""),
                    stderr: observation(""),
                    outcome: ExecutionOutcome::Complete { result: None },
                    tool_calls: (1..=20)
                        .map(|count| ToolCall {
                            tool: ToolName::Read,
                            summary: ToolCallSummary::Count(count),
                            subject: None,
                            duration: None,
                        })
                        .collect(),
                    read_paths: Vec::new(),
                    modified_paths: Vec::new(),
                    changed_bindings: Vec::new(),
                },
                attempts: Vec::new(),
            },
        });
        let initial = RuntimeInitialState {
            metadata: None,
            timeline: records
                .into_iter()
                .map(RestoredTimelineItem::Record)
                .collect(),
            footer_adjustment: Vec::new(),
            disabled_reason: Some(RuntimeDisabledReason::SessionRestoreFailed),
        };
        let mut app = App::restored(crate::config::ready_snapshot_for_test(), &initial);

        let screen = rendered(&mut app, 60, 7);
        assert!(screen.contains("Session restore failed; session is read-only"));
        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        assert!(matches!(app.history_position, HistoryPosition::Anchored(_)));
        assert!(app.handle_key(key(KeyCode::Home)).visible_changed);
        assert_eq!(app.history_position, HistoryPosition::Anchored(0));
        assert!(
            app.handle_key(modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .visible_changed
        );
        app.handle_key(history_end());
        let expanded = rendered(&mut app, 60, 200);
        assert!(expanded.contains("saved arena line 01"));
        assert!(expanded.contains("read · 20 B"));
        set_prompt(&mut app, "draft");
        assert!(app.handle_key(key(KeyCode::Enter)).commands.is_empty());
    }

    #[test]
    fn restored_transcript_projects_timeline_and_footer_accounting() {
        let request = StoredRecord {
            run_id: RunId(1),
            record: Record::UserRequest {
                request: "saved request".to_owned(),
                selection: None,
            },
        };
        let final_message = StoredRecord {
            run_id: RunId(1),
            record: Record::FinalMessage {
                step_id: ModelStepId(1),
                text: "saved answer".to_owned(),
                reasoning_summary: String::new(),
                terminal: TerminalDetails::empty(),
            },
        };
        let task = crate::subagents::TaskRecord::for_test(
            "00",
            2,
            crate::config::ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let initial = RuntimeInitialState {
            metadata: None,
            timeline: vec![
                RestoredTimelineItem::Record(request),
                RestoredTimelineItem::SubagentSpawned(task.clone()),
                RestoredTimelineItem::SubagentFinished(task),
                RestoredTimelineItem::Record(final_message),
            ],
            footer_adjustment: vec![usage(3, 2)],
            disabled_reason: None,
        };
        let mut app = App::restored(crate::config::ready_snapshot_for_test(), &initial);

        let screen = rendered(&mut app, 120, 40);
        assert!(screen.find("saved request").unwrap() < screen.find("saved answer").unwrap());
        assert!(screen.contains("↑3 ↓2"), "{screen}");
        assert!(
            app.handle_key(modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL))
                .visible_changed
        );
        assert!(rendered(&mut app, 120, 40).contains("✓ subagent:00"));
    }

    #[test]
    fn restored_failed_run_shows_unmeasured_retries_in_the_tui() {
        let initial = RuntimeInitialState {
            metadata: None,
            timeline: vec![
                RestoredTimelineItem::Record(StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "inspect".to_owned(),
                        selection: None,
                    },
                }),
                RestoredTimelineItem::Record(StoredRecord {
                    run_id: RunId(1),
                    record: Record::FailedRun {
                        message:
                            "model provider failed: provider failure: provider transport failed"
                                .to_owned(),
                        terminal: terminal(None, vec![AttemptAccounting::UsageUnavailable; 3]),
                    },
                }),
            ],
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        };
        let mut app = App::restored(crate::config::ready_snapshot_for_test(), &initial);

        let screen = rendered(&mut app, 120, 24);
        assert!(
            screen.contains(
                "failed: model provider failed: provider failure: provider transport failed"
            ),
            "{screen}"
        );
        assert!(
            screen.lines().any(|row| row.trim() == "3 attempts"),
            "{screen}"
        );
        assert!(!screen.contains(['$', '↑', '↓']), "{screen}");
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
                    selection: None,
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
                        selection: None,
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
                    selection: None,
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
                    selection: None,
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
            epoch: 0,
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
            epoch: 0,
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
            epoch: 0,
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
                    selection: None,
                },
            },
            progress_gap: false,
        });
        let after_append = rendered(&mut app, 20, 6);

        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        assert!(after_append.contains("followed tail"));
    }

    #[test]
    fn transcript_scrolls_code_step_between_source_and_output() {
        let mut app = App::default();
        let source = (1..=33)
            .map(|line| format!("source line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.handle_event(completed_record(1, 1, &source, Vec::new()));

        let newest = rendered(&mut app, 40, 8);
        assert!(newest.contains("42"), "{newest}");
        assert!(!newest.contains("source line 1 "), "{newest}");
        assert!(app.handle_key(history_home()).visible_changed);
        let oldest = rendered(&mut app, 40, 8);
        assert!(oldest.contains("source line 1 "), "{oldest}");
        assert!(!oldest.contains("42"), "{oldest}");
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
                        selection: None,
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
                        selection: None,
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
            epoch: 0,
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
                    text: "done ".repeat(60),
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
                    selection: None,
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
                    selection: None,
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
                        selection: None,
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
        assert!(app.tick(now + Duration::from_millis(150)).visible_changed);
        assert_eq!(
            app.history_position,
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(3))
        );
        rendered_buffer(&mut app, 40, 7);

        app.handle_mouse(
            mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                column,
                viewport.area.top(),
            ),
            now + Duration::from_millis(150),
        );
        assert!(app.drag.is_none());
        assert!(!app.tick(now + Duration::from_millis(200)).visible_changed);
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
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
                    selection: None,
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
                    selection: None,
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
            epoch: 0,
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
        assert!(resized.notice().is_none());

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
                    selection: None,
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
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(1))
        );
        assert_eq!(redraws, 2);
    }

    #[test]
    fn mouse_wheel_scrolls_transcript_towards_newest_history() {
        let mut app = app_with_history(App::default());
        rendered(&mut app, 40, 7);
        assert!(app.scroll_older(1).visible_changed);
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
            HistoryPosition::Anchored(app.newest_history_top().saturating_sub(1))
        );
        assert_eq!(redraws, 2);
    }

    #[test]
    fn configuration_mouse_wheel_scrolls_towards_newest_history() {
        let mut app =
            app_with_history(App::configuration(crate::config::ready_snapshot_for_test()));
        rendered(&mut app, 40, 7);
        assert!(app.scroll_older(1).visible_changed);
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
            epoch: 0,
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
                    selection: None,
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
            cache_write_1h_input_tokens: 0,
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
            cache_write_1h_input_tokens: 0,
            output_tokens: output,
            reasoning_output_tokens: reasoning,
            total_tokens: total,
        })
    }

    fn terminal(
        evidence: Option<TerminalEvidence>,
        attempts: Vec<AttemptAccounting>,
    ) -> TerminalDetails {
        TerminalDetails {
            evidence,
            attempts,
            overflow_recovery: None,
        }
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
                        read_paths: Vec::new(),
                        modified_paths: Vec::new(),
                        changed_bindings: Vec::new(),
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
                        read_paths: Vec::new(),
                        modified_paths: Vec::new(),
                        changed_bindings: Vec::new(),
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
                                subject: None,
                                duration: None,
                            }],
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
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
            "\u{2717} read · failed",
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
    fn subagent_monitor_renders_one_row_above_prompt() {
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
            "  ✓ subagent:c1  openai/gpt-5.6-terra  0 steps  04:47  test child"
        );
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

        assert_eq!(
            rows[root].trim_end(),
            "  ✓ subagent:00  openai/gpt-5.6-terra  0 steps  00:00  test child"
        );
        assert_eq!(
            rows[child].trim_end(),
            "    └─✓ subagent:01  openai/gpt-5.6-terra  0 steps  00:00  test child"
        );
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
        assert!(rendered_at(&mut app, 80, 9, now).contains("… subagent:child1"));
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

        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1  "));
        assert!(
            app.handle_key_at(modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL), now)
                .commands
                .is_empty()
        );
        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1  "));
        set_prompt(&mut app, "/model");
        app.handle_key_at(key(KeyCode::Enter), now);
        app.handle_key_at(key(KeyCode::Esc), now);
        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1  "));
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        set_prompt(&mut app, "busy draft");
        assert!(
            app.handle_key_at(key(KeyCode::Enter), now)
                .commands
                .is_empty()
        );
        app.handle_event_at(
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(RunId(1)),
            },
            now,
        );
        assert!(rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1  "));

        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "accepted".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        );

        assert!(!rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1  "));
        app.handle_event_at(RuntimeEvent::SubagentFinished { record }, now);
        assert!(!rendered_at(&mut app, 80, 12, now).contains("✓ subagent:c1  "));
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
        let monitor_row = "  ✓ subagent:c1  openai/gpt-5.6-terra  0 steps  00:00  test child";
        assert!(
            before.contains(" ✓ subagent:c1 · openai/gpt-5.6-terra · 0 steps · 00:00"),
            "{before}"
        );
        assert!(before.contains(monitor_row), "{before}");

        let update =
            app.handle_key_at(modified_key(KeyCode::Char('o'), KeyModifiers::CONTROL), now);
        assert!(update.commands.is_empty());
        assert!(update.visible_changed);

        let expanded = rendered_at(&mut app, 80, 12, now);
        assert!(expanded.contains(monitor_row), "{expanded}");
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
    fn released_escape_does_not_change_keypress_anchored_correction_timing() {
        let submitted_at = Instant::now();
        let mut app = App::default();
        enter_at(&mut app, "delayed admission", submitted_at);

        let release =
            KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(
            app.handle_key_at(release, submitted_at + Duration::from_secs(1))
                .commands
                .is_empty()
        );
        assert!(!app.foreground_cancellation_requested);
        assert!(!app.correction_pending);
        assert_eq!(app.submission_at, Some(submitted_at));
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), submitted_at + Duration::from_secs(2))
                .commands,
            [RuntimeCommand::Cancel { correction: true }]
        );
    }

    #[test]
    fn settled_double_escape_opens_an_unfiltered_tree_within_499ms() {
        let now = Instant::now();
        let mut app = App::default();

        let first = app.handle_key_at(key(KeyCode::Esc), now);
        assert!(first.commands.is_empty());
        assert!(!first.visible_changed);

        let second = app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(499));
        assert_eq!(
            second.commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
        assert!(app.tree_navigator.is_some());
        assert_eq!(app.settled_escape_at, None);
    }

    #[test]
    fn settled_double_escape_at_the_same_instant_opens_like_pi() {
        let now = Instant::now();
        let mut app = App::default();

        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
    }

    #[test]
    fn settled_double_escape_at_500ms_rearms_before_opening() {
        let now = Instant::now();
        let mut app = App::default();

        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        assert_eq!(app.settled_escape_at, Some(now));
        assert!(
            app.tick(now + Duration::from_millis(500))
                .commands
                .is_empty()
        );
        assert_eq!(app.settled_escape_at, Some(now));
        assert!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(500))
                .commands
                .is_empty()
        );
        assert_eq!(
            app.settled_escape_at,
            Some(now + Duration::from_millis(500))
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(999))
                .commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
        assert_eq!(app.settled_escape_at, None);
    }

    #[test]
    fn settled_double_escape_accepts_whitespace_only_prompts() {
        let now = Instant::now();
        let mut app = app_with_prompt(" \n ");

        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(499))
                .commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
        assert_eq!(app.prompt.text(), " \n ");
    }

    #[test]
    fn settled_double_escape_survives_intervening_typing_and_clearing() {
        let now = Instant::now();
        let mut app = App::default();

        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        assert!(
            app.handle_key_at(key(KeyCode::Char('x')), now + Duration::from_millis(1))
                .visible_changed
        );
        assert!(
            app.handle_key_at(
                modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
                now + Duration::from_millis(2),
            )
            .visible_changed
        );
        assert!(app.prompt.is_empty());
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(499))
                .commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
    }

    #[test]
    fn nonqualifying_escape_paths_preserve_settled_double_escape_state() {
        let now = Instant::now();

        let mut background = armed_settled_escape(now);
        background
            .subagent_monitor
            .insert_for_test("child", TaskState::Running, now);
        assert_nonqualifying_escape(&mut background, now, &[RuntimeCommand::CancelBackground]);

        let mut foreground = armed_settled_escape(now);
        foreground.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        assert_nonqualifying_escape(
            &mut foreground,
            now,
            &[RuntimeCommand::Cancel { correction: false }],
        );

        let mut nonempty_prompt = armed_settled_escape(now);
        set_prompt(&mut nonempty_prompt, "draft");
        assert_nonqualifying_escape(
            &mut nonempty_prompt,
            now,
            &[RuntimeCommand::CancelBackground],
        );

        let mut configuration = armed_settled_escape(now);
        configuration.mode = super::AppMode::ConfigurationOnly;
        assert_nonqualifying_escape(&mut configuration, now, &[]);

        let mut read_only = armed_settled_escape(now);
        read_only.handle_event_at(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
            },
            now,
        );
        assert_nonqualifying_escape(&mut read_only, now, &[RuntimeCommand::CancelBackground]);

        for pending in ["foreground cancellation", "manual navigation", "correction"] {
            let mut app = armed_settled_escape(now);
            match pending {
                "foreground cancellation" => app.foreground_cancellation_requested = true,
                "manual navigation" => app.manual_navigation_pending = true,
                "correction" => app.correction_pending = true,
                _ => unreachable!("unknown pending state"),
            }
            assert_nonqualifying_escape(&mut app, now, &[RuntimeCommand::CancelBackground]);
        }
    }

    #[test]
    fn released_escape_does_not_reset_settled_double_escape_state() {
        let now = Instant::now();
        let mut app = App::default();

        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        let release =
            KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release);
        assert!(
            app.handle_key_at(release, now + Duration::from_millis(1))
                .commands
                .is_empty()
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(499))
                .commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
    }

    #[test]
    fn picker_escape_precedes_settled_double_escape() {
        let now = Instant::now();
        let mut app = App::default();

        assert!(
            app.handle_key_at(key(KeyCode::Esc), now)
                .commands
                .is_empty()
        );
        set_prompt(&mut app, "/model");
        assert!(
            app.handle_key_at(key(KeyCode::Enter), now + Duration::from_millis(1))
                .commands
                .is_empty()
        );
        assert!(app.picker.is_some());
        assert!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(2))
                .commands
                .is_empty()
        );
        assert!(app.picker.is_none());
        assert_eq!(app.settled_escape_at, Some(now));
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(499))
                .commands,
            [RuntimeCommand::RequestTree { revision: 1 }]
        );
    }

    #[test]
    fn correction_restoration_keeps_drafts_prefills_and_retains_its_card_until_request_commit() {
        let now = Instant::now();
        let replacement = CorrectionOutcome::Replaced {
            state: restored_state("restored parent"),
            request_prefill: Some("original request".to_owned()),
        };

        let mut drafted = App {
            correction_pending: true,
            correction_run_id: Some(RunId(1)),
            ..App::default()
        };
        set_prompt(&mut drafted, "draft wins");
        drafted.handle_event_at(
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: replacement.clone(),
            },
            now,
        );
        drafted.handle_event_at(RuntimeEvent::RunSettled { run_id: RunId(1) }, now);
        assert_eq!(drafted.prompt.text(), "draft wins");
        assert!(rendered_at(&mut drafted, 80, 20, now).contains("Session restored."));

        let mut prefilled = App {
            correction_pending: true,
            correction_run_id: Some(RunId(1)),
            ..App::default()
        };
        prefilled.handle_event_at(
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: replacement,
            },
            now,
        );
        prefilled.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(2) }, now);
        assert_eq!(prefilled.prompt.text(), "original request");
        assert!(rendered_at(&mut prefilled, 80, 20, now).contains("Session restored."));
        prefilled.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "corrected request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert!(!rendered_at(&mut prefilled, 80, 20, now).contains("Session restored."));
    }

    #[test]
    fn early_escape_carries_correction_intent_but_late_escape_only_cancels() {
        let submitted_at = Instant::now();

        for (elapsed, correction) in [
            (Duration::from_millis(1_999), true),
            (Duration::from_secs(2), true),
            (Duration::from_millis(2_001), false),
        ] {
            let mut app = App::default();
            assert_eq!(
                enter_at(&mut app, "correct this", submitted_at).commands,
                [RuntimeCommand::Submit {
                    request: "correct this".to_owned(),
                    activation: None,
                }]
            );
            assert_eq!(
                app.handle_key_at(key(KeyCode::Esc), submitted_at + elapsed)
                    .commands,
                [RuntimeCommand::Cancel { correction }],
                "Esc at {elapsed:?}"
            );
            assert_eq!(
                app.handle_key_at(key(KeyCode::Esc), submitted_at + elapsed)
                    .commands,
                [RuntimeCommand::CancelBackground],
                "repeated Esc at {elapsed:?}"
            );
        }
    }

    #[test]
    fn correction_replacement_ignores_progress_after_reliable_settlement() {
        let now = Instant::now();
        let mut app = App::default();
        assert_eq!(
            enter_at(&mut app, "correct this", now).commands,
            [RuntimeCommand::Submit {
                request: "correct this".to_owned(),
                activation: None,
            }]
        );
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "correct this".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now + Duration::from_millis(1))
                .commands,
            [RuntimeCommand::Cancel { correction: true }]
        );
        set_prompt(&mut app, "draft wins");
        assert!(
            app.handle_key_at(key(KeyCode::Enter), now)
                .commands
                .is_empty()
        );
        assert_eq!(
            app.notice(),
            Some("Session restoration is still in progress.")
        );
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CancelledRun {
                        terminal: terminal(None, vec![]),
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert!(app.handle_event_at(
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: CorrectionOutcome::Replaced {
                    state: restored_state("restored parent"),
                    request_prefill: Some("correct this".to_owned()),
                },
            },
            now,
        ));
        assert_eq!(app.prompt.text(), "draft wins");
        let restored = rendered_at(&mut app, 80, 20, now);
        assert!(restored.contains("restored parent"), "{restored}");
        assert!(restored.contains("Session restored."), "{restored}");
        assert!(!restored.contains("correct this"), "{restored}");
        app.handle_event_at(RuntimeEvent::RunSettled { run_id: RunId(1) }, now);
        assert!(!app.correction_pending);
        assert_eq!(app.ignored_progress_run_id, Some(RunId(1)));
        assert!(!app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale progress".to_owned(),
                },
            },
            gap_before: false,
        }));
        assert!(!rendered_at(&mut app, 80, 20, now).contains("stale progress"));
        app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::RunStarted { run_id: RunId(5) },
            gap_before: false,
        });
        app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id: RunId(5),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            gap_before: false,
        });
        assert!(app.handle_progress(ProgressEnvelope {
            item: RuntimeEvent::ModelProgress {
                run_id: RunId(5),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "fresh progress".to_owned(),
                },
            },
            gap_before: false,
        }));
        assert!(rendered_at(&mut app, 80, 20, now).contains("fresh progress"));

        let mut refill = App {
            correction_pending: true,
            correction_run_id: Some(RunId(1)),
            ..App::default()
        };
        assert!(refill.handle_event_at(
            RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome: CorrectionOutcome::Replaced {
                    state: restored_state("refilled parent"),
                    request_prefill: Some("correct this".to_owned()),
                },
            },
            now,
        ));
        assert_eq!(refill.prompt.text(), "correct this");
        assert!(rendered_at(&mut refill, 80, 20, now).contains("Session restored."));
        refill.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "correct this".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert!(!rendered_at(&mut refill, 80, 20, now).contains("Session restored."));
    }

    #[test]
    fn correction_failure_notices_survive_settlement_transcript_mutation() {
        for (outcome, notice) in [
            (CorrectionOutcome::Failed, "Session could not be restored."),
            (
                CorrectionOutcome::SkippedBackground,
                "Session was not restored because background work is active.",
            ),
        ] {
            let mut app = App {
                correction_pending: true,
                correction_run_id: Some(RunId(1)),
                ..App::default()
            };
            assert!(app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) }));
            assert!(app.handle_event(RuntimeEvent::CorrectionCompleted {
                run_id: RunId(1),
                outcome,
            }));
            assert_eq!(app.notice(), Some(notice));
            assert!(app.correction_pending);

            assert!(app.handle_event(RuntimeEvent::RunSettled { run_id: RunId(1) }));
            assert_eq!(app.notice(), Some(notice));
            assert!(!app.correction_pending);
        }
    }

    #[test]
    fn escape_preserves_foreground_and_background_cancellation_outside_configuration() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);

        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::Cancel { correction: false }]
        );
        app.handle_event_at(RuntimeEvent::RunSettled { run_id: RunId(1) }, now);
        assert_eq!(
            enter_at(&mut app, "second prompt", now).commands,
            vec![RuntimeCommand::Submit {
                request: "second prompt".to_owned(),
                activation: None,
            }]
        );
        assert_eq!(
            app.handle_key_at(key(KeyCode::Esc), now).commands,
            vec![RuntimeCommand::Cancel { correction: true }]
        );
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "second prompt".to_owned(),
                        selection: None,
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

        let mut disabled = App::default();
        disabled.handle_event_at(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
                    selection: None,
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
    fn generation_throbber_is_a_single_row_four_cell_orbit_with_opposite_dots() {
        let throbber = generation_throbber();

        assert_eq!((throbber.size().width, throbber.size().height), (4, 1));
        assert_eq!(
            (0..10)
                .map(|index| throbber.frame(index))
                .collect::<Vec<_>>(),
            [
                "⠁  ⢀", "⠈  ⡀", " ⠁⢀ ", " ⠈⡀ ", " ⢀⠁ ", " ⡀⠈ ", "⢀  ⠁", "⡀  ⠈", "⠄  ⠐", "⠂  ⠠",
            ]
        );
    }

    #[test]
    fn compaction_throbber_converges_and_disappears() {
        let throbber = compaction_throbber();

        assert_eq!((throbber.size().width, throbber.size().height), (9, 1));
        assert_eq!(
            (0..8)
                .map(|index| throbber.frame(index))
                .collect::<Vec<_>>(),
            [
                "·       ·",
                "●·     ·●",
                "·●·   ·●·",
                " ·●· ·●· ",
                "  ·●·●·  ",
                "   ·●·   ",
                "    ·    ",
                "         ",
            ]
        );
    }

    #[test]
    fn active_compaction_animates_the_convergence_throbber_above_the_prompt() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: CompactionAcknowledgement::Accepted,
            },
            now,
        );
        app.handle_event_at(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Started,
            },
            now,
        );

        let initial = rendered_at(&mut app, 20, 12, now);
        let rows = initial.lines().collect::<Vec<_>>();
        assert!(rows[8].starts_with(" ·       ·"), "{initial}");
        assert_eq!(rows[9], "─".repeat(20));
        assert_eq!(app.history_height, 6);

        assert!(!app.tick(now + Duration::from_millis(49)).visible_changed);
        assert!(app.tick(now + Duration::from_millis(50)).visible_changed);
        let advanced = rendered_at(&mut app, 20, 12, now + Duration::from_millis(50));
        assert!(
            advanced
                .lines()
                .nth(8)
                .is_some_and(|row| row.starts_with(" ●·     ·●")),
            "{advanced}"
        );

        app.handle_event_at(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Settled(CompactionSettlement::Applied),
            },
            now + Duration::from_millis(50),
        );
        let settled = rendered_at(&mut app, 20, 12, now + Duration::from_millis(50));
        assert!(!settled.contains('●'));
        assert!(!settled.contains('·'));
        assert_eq!(app.history_height, 8);
        assert!(!app.tick(now + Duration::from_millis(100)).visible_changed);
    }

    #[test]
    fn automatic_compaction_animates_convergence_over_delayed_generation_progress() {
        let now = Instant::now();
        let run_id = RunId(1);
        let mut app = App::default();
        set_prompt(&mut app, "draft");
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id,
                    record: Record::UserRequest {
                        request: "large request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        );
        app.handle_event_at(
            RuntimeEvent::AutomaticCompactionStarted {
                run_id,
                initial_overflow: None,
            },
            now,
        );
        for progress in [
            RuntimeEvent::RunStarted { run_id },
            RuntimeEvent::ModelStepStarted {
                run_id,
                step_id: ModelStepId(1),
                epoch: 0,
            },
            RuntimeEvent::ModelProgress {
                run_id,
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale text".to_owned(),
                },
            },
        ] {
            app.handle_progress(ProgressEnvelope {
                item: progress,
                gap_before: false,
            });
        }

        assert!(app.transcript.is_compacting());
        assert!(!app.transcript.is_generating());
        assert_eq!(app.prompt.text(), "draft");
        assert!(!app.compact_acknowledgement_pending);
        let initial = rendered_at(&mut app, 20, 12, now);
        assert!(!initial.contains("stale text"), "{initial}");
        assert!(
            initial
                .lines()
                .nth(8)
                .is_some_and(|row| row.starts_with(" ·       ·")),
            "{initial}"
        );
        assert!(app.tick(now + Duration::from_millis(50)).visible_changed);
        let advanced = rendered_at(&mut app, 20, 12, now + Duration::from_millis(50));
        assert!(
            advanced
                .lines()
                .nth(8)
                .is_some_and(|row| row.starts_with(" ●·     ·●")),
            "{advanced}"
        );

        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id,
                    record: Record::Compaction {
                        outcome: crate::session::CompactionOutcome::Cancelled {
                            reason: crate::session::CompactionReason::AutomaticThreshold,
                            attempts: Vec::new(),
                        },
                    },
                },
                progress_gap: false,
            },
            now + Duration::from_millis(50),
        );
        let settled = rendered_at(&mut app, 20, 12, now + Duration::from_millis(50));
        assert!(!settled.contains('●'), "{settled}");
        assert!(!app.tick(now + Duration::from_millis(100)).visible_changed);
        assert_eq!(app.prompt.text(), "draft");
    }

    #[test]
    fn overflow_retry_reset_keeps_buffered_initial_progress_off_screen() {
        let now = Instant::now();
        let run_id = RunId(1);
        let step_id = ModelStepId(1);
        let progress = |epoch, text: &str| ProgressEnvelope {
            item: RuntimeEvent::ModelProgress {
                run_id,
                step_id,
                epoch,
                progress: ModelProgress::AssistantTextChunk {
                    text: text.to_owned(),
                },
            },
            gap_before: true,
        };
        let started = |epoch| ProgressEnvelope {
            item: RuntimeEvent::ModelStepStarted {
                run_id,
                step_id,
                epoch,
            },
            gap_before: true,
        };
        let mut app = App::default();
        set_prompt(&mut app, "draft");
        app.handle_event_at(RuntimeEvent::RunStarted { run_id }, now);
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id,
                    record: Record::UserRequest {
                        request: "large request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert!(app.handle_progress(started(0)));
        assert!(app.handle_progress(progress(0, "initial partial")));
        let screen = rendered_at(&mut app, 40, 16, now);
        assert!(screen.contains("initial partial"), "{screen}");
        app.handle_event_at(
            RuntimeEvent::AutomaticCompactionStarted {
                run_id,
                initial_overflow: Some((step_id, Vec::new())),
            },
            now,
        );
        assert!(!app.handle_progress(progress(0, "buffered during summary")));
        assert!(app.transcript.is_compacting());
        assert!(!app.transcript.is_generating());

        app.handle_event_at(
            RuntimeEvent::GenerationRestarted {
                run_id,
                step_id,
                epoch: 1,
            },
            now,
        );
        app.handle_event_at(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id,
                    record: Record::Compaction {
                        outcome: crate::session::CompactionOutcome::Applied {
                            summary: "summary".to_owned(),
                            first_retained: crate::session::SessionEntryId(2),
                            context_tokens: 1_250,
                            reason: crate::session::CompactionReason::ContextOverflow,
                            attempts: Vec::new(),
                        },
                    },
                },
                progress_gap: false,
            },
            now,
        );
        assert!(!app.handle_progress(started(0)));
        assert!(!app.handle_progress(progress(0, "buffered after card")));

        let screen = rendered_at(&mut app, 40, 16, now);
        assert!(!screen.contains("partial"), "{screen}");
        assert!(!screen.contains("buffered"), "{screen}");
        assert_eq!(app.prompt.text(), "draft");
        assert!(!app.compact_acknowledgement_pending);

        assert!(app.handle_progress(started(1)));
        assert!(app.handle_progress(progress(1, "fresh retry")));
        let screen = rendered_at(&mut app, 40, 16, now);
        assert!(screen.contains("fresh retry"), "{screen}");
        assert!(!screen.contains("buffered"), "{screen}");
        assert!(app.transcript.is_generating());
    }

    #[test]
    fn generating_throbber_is_animated_and_anchored_above_the_prompt() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        app.handle_event_at(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            now,
        );

        let initial = rendered_at(&mut app, 20, 12, now);
        let rows = initial.lines().collect::<Vec<_>>();
        assert!(rows[7].trim().is_empty());
        assert!(rows[8].starts_with(" ⠁  ⢀"));
        let initial_buffer = rendered_buffer(&mut app, 20, 12);
        assert!(initial_buffer[(1, 8)].modifier.contains(Modifier::BOLD));
        assert_eq!(rows[9], "─".repeat(20));
        assert!(!rows[1..9].join("\n").contains("generating"));
        assert_eq!(app.history_height, 6);

        app.handle_event_at(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "streamed answer".to_owned(),
                },
            },
            now,
        );
        let streaming = rendered_at(&mut app, 20, 12, now);
        assert!(streaming.contains("streamed answer"));
        assert!(
            streaming
                .lines()
                .nth(8)
                .is_some_and(|row| row.starts_with(" ⠁  ⢀"))
        );

        set_prompt(&mut app, "/");
        let with_completion = rendered_at(&mut app, 20, 12, now);
        assert!(with_completion.contains("/model"));
        assert!(
            with_completion
                .lines()
                .nth(8)
                .is_some_and(|row| row.starts_with(" ⠁  ⢀"))
        );

        assert!(!app.tick(now + Duration::from_millis(49)).visible_changed);
        assert!(app.tick(now + Duration::from_millis(50)).visible_changed);
        let advanced = rendered_at(&mut app, 20, 12, now + Duration::from_millis(50));
        assert!(
            advanced
                .lines()
                .nth(8)
                .is_some_and(|row| row.starts_with(" ⠈  ⡀"))
        );

        app.handle_event_at(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: None,
            },
            now + Duration::from_millis(50),
        );
        let executing = rendered_at(&mut app, 20, 12, now + Duration::from_millis(50));
        assert!(!executing.contains('⠈'));
        assert_eq!(app.history_height, 8);
        assert!(!app.tick(now + Duration::from_millis(100)).visible_changed);
    }

    #[test]
    fn tiny_generation_keeps_the_only_history_row() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        app.handle_event_at(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            now,
        );

        let screen = rendered_at(&mut app, 20, 5, now);
        assert!(!screen.contains('⠁'));
        assert_eq!(app.history_height, 1);
        assert_eq!(
            app.history_viewport.map(|viewport| viewport.area.height),
            Some(1)
        );
    }

    #[test]
    fn disabling_during_generation_stops_the_throbber() {
        let now = Instant::now();
        let mut app = App::default();
        app.handle_event_at(RuntimeEvent::RunStarted { run_id: RunId(1) }, now);
        app.handle_event_at(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            now,
        );
        app.handle_event_at(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
            now,
        );

        let screen = rendered_at(&mut app, 20, 12, now);
        assert!(!screen.contains('⠁'));
        assert!(!app.transcript.is_generating());
        assert_eq!(app.history_height, 8);
        assert!(!app.tick(now + Duration::from_millis(80)).visible_changed);
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
        assert_eq!(rows[1], "─  …t-6.1-sol • thinking medium ─");

        let buffer = rendered_buffer(&mut app, 33, 2);
        assert_eq!(
            buffer[(0, 1)].fg,
            super::thinking_rule_colour(
                &DARK,
                GPT6_1_SOL.thinking_levels,
                GPT6_1_SOL.default_thinking
            )
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
        assert_eq!(rows[2], "─  …t-6.1-sol • thinking medium ─");

        let buffer = rendered_buffer(&mut app, 33, 3);
        assert_eq!(
            buffer[(0, 2)].fg,
            super::thinking_rule_colour(
                &DARK,
                GPT6_1_SOL.thinking_levels,
                GPT6_1_SOL.default_thinking
            )
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
                        selection: None,
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
            epoch: 0,
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: usage(1, 1),
            },
        });
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 5);
        assert_eq!(app.history_position, position);

        set_prompt(&mut app, "");
        rendered(&mut app, 20, 12);
        assert_eq!(app.history_height, 6);
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
        let update = app.name_session("Refactor auth");
        let [RuntimeCommand::SetSessionName { revision, name }] = update.commands.as_slice() else {
            panic!("expected name command");
        };
        assert!(app.handle_event(RuntimeEvent::NameAccepted {
            revision: *revision,
            name: name.clone(),
        }));
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
        assert!(footer.contains("gpt-6.1-sol • thinking medium"), "{footer}");
    }

    #[test]
    fn footer_is_absent_without_metrics_for_unconfigured_and_configuration_only_sessions() {
        let mut without_metrics = app_with_prompt("ask");
        without_metrics.handle_event(RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "request".to_owned(),
                    selection: None,
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
        assert_eq!(full[(column, 3)].fg, app.theme.warning);
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
                    selection: None,
                },
            },
            progress_gap: false,
        });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            epoch: 0,
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
                    selection: None,
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
                        selection: None,
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
            epoch: 0,
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
                                cache_write_1h_input_tokens: 0,
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
            epoch: 0,
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
        rendered(&mut app, 50, 6);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(2) });
        app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(2),
            step_id: ModelStepId(5),
            epoch: 0,
            progress: ModelProgress::AssistantTextChunk {
                text: "live".to_owned(),
            },
        });
        rendered(&mut app, 50, 6);
        assert_eq!(app.history_position, HistoryPosition::FollowingBottom);

        assert!(app.handle_key(key(KeyCode::PageUp)).visible_changed);
        let position = app.history_position;
        app.handle_event(completed_record(2, 5, "settled", vec![]));
        rendered(&mut app, 50, 6);
        assert_eq!(app.history_position, position);
    }

    #[test]
    fn transcript_live_changes_dirty_once_and_keeps_tiny_and_restore_behaviour() {
        let mut app = App::default();
        app.handle_event(RuntimeEvent::RunStarted { run_id: RunId(1) });
        let event = RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            epoch: 0,
            progress: ModelProgress::AssistantTextChunk {
                text: "once".to_owned(),
            },
        };
        assert!(app.handle_event(event.clone()));
        assert!(!app.handle_event(RuntimeEvent::ModelProgress {
            run_id: RunId(1),
            step_id: ModelStepId(1),
            epoch: 0,
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
                                subject: None,
                                duration: None,
                            })
                            .collect(),
                        read_paths: Vec::new(),
                        modified_paths: Vec::new(),
                        changed_bindings: Vec::new(),
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
        assert!(collapsed.contains("code line 10"));
        assert!(!collapsed.contains("code line 11"));
        assert!(collapsed.contains("… 23 more lines · Ctrl-O to expand"));
        assert!(collapsed.contains("… 12 earlier calls · Ctrl-O to expand"));
        assert!(!collapsed.contains("read · 12 B"));
        assert!(collapsed.contains("read · 13 B"));

        let expanded = app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(expanded.commands.is_empty());
        assert!(expanded.visible_changed);
        let expanded = rendered(&mut app, 60, 40);
        assert!(expanded.contains("code line 33"));
        assert!(!expanded.contains("Ctrl-O to expand"));
        assert!(expanded.contains("read · 1 B"));
        assert!(expanded.contains("read · 20 B"));
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
            reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
                        read_paths: Vec::new(),
                        modified_paths: Vec::new(),
                        changed_bindings: Vec::new(),
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
                        read_paths: Vec::new(),
                        modified_paths: Vec::new(),
                        changed_bindings: Vec::new(),
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
            epoch: 0,
        });
        assert!(rendered(&mut app, 120, 4).starts_with("Status: generating"));
        app.handle_event(RuntimeEvent::ToolUpdated {
            run_id: RunId(73),
            step_id: ModelStepId(91),
            token: ToolCallToken::for_test(1_234_567),
            call: ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
                subject: None,
                duration: None,
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
                subject: None,
                duration: None,
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
