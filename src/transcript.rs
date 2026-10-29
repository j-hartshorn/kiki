use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use unicode_segmentation::UnicodeSegmentation;

use ratatui::buffer::{Buffer, CellWidth};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use tui_markdown::{
    AlertKind, CodeTheme, ImageFallback, Options, StyleSheet, from_str_with_options,
};

use crate::agent::{
    ExecutionEvidence, ExecutionFailure, GenerationEvidence, OverflowRecovery, TerminalEvidence,
};
use crate::arena::{
    Execution, ExecutionOutcome, Observation, RunPreview, ToolCall, ToolCallSummary, ToolCallToken,
    ToolName,
};
use crate::config::{
    ApiKeyPresence, DEFAULT_MODEL, ModelSelection, PromptProvenance, StartupSnapshot,
    SubmissionAvailability, model_by_id, models::Provider,
};
use crate::highlight::PythonHighlighter;
use crate::model::{AttemptAccounting, ModelProgress, ModelUsage};
use crate::pricing::{RunCostEstimate, estimate_run_cost};
use crate::runtime::{
    ModelStepId, Record, RestoredTimelineItem, RunId, RuntimeDisabledReason, RuntimeEvent,
    RuntimeInitialState, StoredRecord, TerminalDetails,
};
use crate::selection::{
    CardId, Extraction, RowBreaks, SelectableRegion, Selection, Viewport, highlight,
};
use crate::session::{CompactionOutcome, CompactionReason};
use crate::skills::SkillOrigin;
use crate::subagents::{TaskId, TaskOutcome, TaskRecord, TaskState};
use crate::theme::{DARK, LIGHT, Theme};

#[cfg(test)]
mod markdown;

#[derive(Clone, Copy)]
struct MarkdownStyleSheet {
    theme: &'static Theme,
}

impl StyleSheet for MarkdownStyleSheet {
    fn heading(&self, _level: u8) -> Style {
        Style::new().fg(self.theme.md_heading).bold()
    }

    fn link(&self) -> Style {
        Style::new().fg(self.theme.md_link).underlined()
    }

    fn blockquote(&self) -> Style {
        Style::new().fg(self.theme.md_quote)
    }

    fn code(&self) -> Style {
        Style::new().fg(self.theme.md_code)
    }

    fn heading_meta(&self) -> Style {
        Style::new().fg(self.theme.dim)
    }

    fn html(&self) -> Style {
        Style::new().fg(self.theme.dim)
    }

    fn footnote_ref(&self) -> Style {
        Style::new().fg(self.theme.dim).italic()
    }

    fn footnote_def(&self) -> Style {
        Style::new().fg(self.theme.dim)
    }

    fn image_alt(&self) -> Style {
        Style::new().fg(self.theme.dim).italic()
    }

    fn metadata_block(&self) -> Style {
        Style::new().fg(self.theme.muted)
    }

    fn math_inline(&self) -> Style {
        Style::new().fg(self.theme.md_code).italic()
    }

    fn math_display(&self) -> Style {
        Style::new().fg(self.theme.md_code)
    }

    fn alert(&self, kind: AlertKind) -> Style {
        let color = match kind {
            AlertKind::Note => self.theme.md_link,
            AlertKind::Tip => self.theme.success,
            AlertKind::Important => self.theme.accent,
            AlertKind::Warning => self.theme.warning,
            AlertKind::Caution => self.theme.error,
        };
        Style::new().fg(color)
    }

    fn table_header(&self) -> Style {
        Style::new().fg(self.theme.text).bold()
    }

    fn table_border(&self) -> Style {
        Style::new().fg(self.theme.border_muted)
    }
}

fn textmate_theme(theme: &Theme) -> String {
    let mut source = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>settings</key><array>",
    );
    for (scope, color) in [
        ("", theme.md_code),
        ("comment", theme.syntax_comment),
        (
            "keyword, storage, storage.type.function, storage.type.class, source.rust storage.type",
            theme.syntax_keyword,
        ),
        (
            "entity.name.function, support.function",
            theme.syntax_function,
        ),
        ("variable", theme.syntax_variable),
        ("string", theme.syntax_string),
        ("constant.numeric, constant.language", theme.syntax_number),
        (
            "entity.name.type, support.type, storage.type",
            theme.syntax_type,
        ),
        ("keyword.operator", theme.syntax_operator),
        ("punctuation", theme.syntax_punctuation),
    ] {
        source.push_str("<dict>");
        if !scope.is_empty() {
            source.push_str(&format!("<key>scope</key><string>{scope}</string>"));
        }
        let Color::Rgb(red, green, blue) = color else {
            panic!("TextMate theme tokens must be RGB colours");
        };
        source.push_str(&format!(
            "<key>settings</key><dict><key>foreground</key><string>#{red:02x}{green:02x}{blue:02x}</string></dict></dict>"
        ));
    }
    source.push_str("</array></dict></plist>");
    source
}

type MarkdownOptions = Options<MarkdownStyleSheet>;

fn build_markdown_options(theme: &'static Theme) -> MarkdownOptions {
    let code_theme = CodeTheme::from_textmate(&textmate_theme(theme))
        .expect("generated TextMate theme must be valid");
    Options::new(MarkdownStyleSheet { theme })
        .image_fallback(ImageFallback::AltText)
        .code_theme(code_theme)
}

fn markdown_options(theme: &'static Theme) -> &'static MarkdownOptions {
    static DARK_OPTIONS: LazyLock<MarkdownOptions> =
        LazyLock::new(|| build_markdown_options(&DARK));
    static LIGHT_OPTIONS: LazyLock<MarkdownOptions> =
        LazyLock::new(|| build_markdown_options(&LIGHT));
    if std::ptr::eq(theme, &DARK) {
        &DARK_OPTIONS
    } else {
        assert!(
            std::ptr::eq(theme, &LIGHT),
            "markdown options exist only for the built-in themes"
        );
        &LIGHT_OPTIONS
    }
}

fn markdown<'a>(
    source: &'a str,
    theme: &'static Theme,
    #[cfg(test)] construction_counts: Option<&mut ConstructionCounts>,
) -> Text<'a> {
    #[cfg(test)]
    if let Some(construction_counts) = construction_counts {
        construction_counts.markdown += 1;
    }
    from_str_with_options(source, markdown_options(theme))
}

#[derive(Debug, Default, PartialEq, Eq)]
enum RunState {
    #[default]
    Idle,
    Submitting,
    Active(RunId),
}

#[derive(Debug, Default, PartialEq, Eq)]
enum CompactionState {
    #[default]
    Idle,
    Pending,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayedRecord {
    record: StoredRecord,
    run_previews: Vec<Option<RunPreview>>,
    progress_gap: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SubagentFinishedCard {
    id: String,
    state: TaskState,
    summary: String,
    prompt: String,
    message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactionCard {
    state: ArenaState,
    heading: String,
    collapsed: String,
    expanded: CompactionContent,
    accounting: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillActivationCard {
    heading: String,
    source_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompactionContent {
    Markdown(String),
    Plain(String),
}

impl CompactionContent {
    fn normalise(self) -> Self {
        match self {
            Self::Markdown(text) => Self::Markdown(normalise_terminal_text(&text)),
            Self::Plain(text) => Self::Plain(normalise_terminal_text(&text)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LivePhase {
    Generating,
    Executing,
}

fn disabled_status(reason: &RuntimeDisabledReason) -> &'static str {
    match reason {
        RuntimeDisabledReason::Authentication(Provider::OpenAi) => {
            "OpenAI authentication failed; restart Kiki"
        }
        RuntimeDisabledReason::Authentication(Provider::Anthropic) => {
            "Anthropic authentication failed; restart Kiki"
        }
        RuntimeDisabledReason::Authentication(Provider::Google) => {
            "Google authentication failed; restart Kiki"
        }
        RuntimeDisabledReason::ArenaCheckpointRecoveryFailed => {
            "Arena recovery failed; restart Kiki"
        }
        RuntimeDisabledReason::SessionStorageWriteFailed => "Session save failed; restart Kiki",
        RuntimeDisabledReason::SessionRestoreFailed => {
            "Session restore failed; session is read-only"
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
    tool_calls: Vec<ToolCall>,
    tool_call_tokens: Vec<ToolCallToken>,
    run_previews: Vec<Option<RunPreview>>,
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
            tool_calls: Vec::new(),
            tool_call_tokens: Vec::new(),
            run_previews: Vec::new(),
            progress_gap,
            activity: "generating".to_owned(),
        }
    }

    fn insert_tool_call(
        &mut self,
        token: ToolCallToken,
        call: ToolCall,
        preview: Option<RunPreview>,
    ) {
        let index = self
            .tool_call_tokens
            .partition_point(|existing| *existing <= token);
        self.tool_calls.insert(index, call);
        self.tool_call_tokens.insert(index, token);
        self.run_previews.insert(index, preview);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptBlock {
    Startup {
        lines: Vec<String>,
    },
    ModelChange {
        selection: ModelSelection,
    },
    User {
        request: String,
    },
    SubagentFinished(SubagentFinishedCard),
    Compaction(CompactionCard),
    SkillActivation(SkillActivationCard),
    Reasoning {
        markdown: String,
    },
    Arena(ArenaBlock),
    Assistant {
        markdown: String,
        accounting: Option<String>,
    },
    Status {
        text: String,
    },
    SessionRestored,
    Metadata {
        lines: Vec<String>,
    },
    Accounting {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArenaBlock {
    state: ArenaState,
    heading: Option<String>,
    status: Option<String>,
    source: Option<String>,
    tool_calls: Vec<ToolCall>,
    run_previews: Vec<Option<RunPreview>>,
    content: Vec<ArenaContent>,
    progress_gap: bool,
    accounting: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArenaState {
    InProgress,
    Complete,
    Error,
}

impl ArenaState {
    fn background(self, theme: &Theme) -> Color {
        match self {
            Self::InProgress => theme.tool_pending_bg,
            Self::Complete => theme.tool_success_bg,
            Self::Error => theme.tool_error_bg,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArenaContent {
    label: &'static str,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormalisedBlock {
    Startup {
        lines: Vec<String>,
    },
    ModelChange {
        text: String,
    },
    User {
        request: String,
    },
    SubagentFinished(SubagentFinishedCard),
    Compaction(CompactionCard),
    SkillActivation(SkillActivationCard),
    Reasoning {
        markdown: String,
    },
    Arena(NormalisedArenaBlock),
    Assistant {
        markdown: String,
        accounting: Option<String>,
    },
    Status {
        text: String,
    },
    SessionRestored,
    Metadata {
        lines: Vec<String>,
    },
    Accounting {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalisedArenaBlock {
    state: ArenaState,
    heading: Option<String>,
    status: Option<String>,
    source: Option<String>,
    tool_calls: Vec<ToolCall>,
    run_previews: Vec<Option<RunPreview>>,
    content: Vec<ArenaContent>,
    progress_gap: bool,
    accounting: Option<String>,
}

impl TranscriptBlock {
    fn normalise(self) -> NormalisedBlock {
        match self {
            Self::Startup { lines } => NormalisedBlock::Startup {
                lines: lines
                    .into_iter()
                    .map(|line| normalise_terminal_text(&line))
                    .collect(),
            },
            Self::ModelChange { selection } => NormalisedBlock::ModelChange {
                text: model_change_text(selection),
            },
            Self::User { request } => NormalisedBlock::User {
                request: normalise_terminal_text(&request),
            },
            Self::SubagentFinished(card) => {
                NormalisedBlock::SubagentFinished(SubagentFinishedCard {
                    id: normalise_terminal_text(&card.id),
                    state: card.state,
                    summary: normalise_terminal_text(&card.summary),
                    prompt: bounded_terminal_text(&card.prompt, 64 * 1024),
                    message: card
                        .message
                        .map(|message| bounded_terminal_text(&message, 4 * 1024)),
                })
            }
            Self::Compaction(card) => NormalisedBlock::Compaction(CompactionCard {
                state: card.state,
                heading: normalise_terminal_text(&card.heading),
                collapsed: normalise_terminal_text(&card.collapsed),
                expanded: card.expanded.normalise(),
                accounting: card
                    .accounting
                    .map(|accounting| normalise_terminal_text(&accounting)),
            }),
            Self::SkillActivation(card) => NormalisedBlock::SkillActivation(SkillActivationCard {
                heading: normalise_terminal_text(&card.heading),
                source_text: bounded_terminal_text(&card.source_text, 64 * 1024),
            }),
            Self::Reasoning { markdown } => NormalisedBlock::Reasoning {
                markdown: normalise_terminal_text(&markdown),
            },
            Self::Arena(arena) => NormalisedBlock::Arena(NormalisedArenaBlock {
                state: arena.state,
                heading: arena
                    .heading
                    .map(|heading| normalise_terminal_text(&heading)),
                status: arena.status.map(|status| normalise_terminal_text(&status)),
                source: arena.source.map(|source| normalise_terminal_text(&source)),
                tool_calls: arena
                    .tool_calls
                    .into_iter()
                    .map(|call| ToolCall {
                        subject: call
                            .subject
                            .map(|subject| normalise_terminal_text(&subject)),
                        ..call
                    })
                    .collect(),
                run_previews: arena
                    .run_previews
                    .into_iter()
                    .map(|preview| {
                        preview.map(|preview| RunPreview {
                            stdout: normalise_terminal_text(&preview.stdout),
                        })
                    })
                    .collect(),
                content: arena
                    .content
                    .into_iter()
                    .map(|content| ArenaContent {
                        label: content.label,
                        text: normalise_terminal_text(&content.text),
                    })
                    .collect(),
                progress_gap: arena.progress_gap,
                accounting: arena
                    .accounting
                    .map(|accounting| normalise_terminal_text(&accounting)),
            }),
            Self::Assistant {
                markdown,
                accounting,
            } => NormalisedBlock::Assistant {
                markdown: normalise_terminal_text(&markdown),
                accounting: accounting.map(|accounting| normalise_terminal_text(&accounting)),
            },
            Self::Status { text } => NormalisedBlock::Status {
                text: normalise_terminal_text(&text),
            },
            Self::SessionRestored => NormalisedBlock::SessionRestored,
            Self::Metadata { lines } => NormalisedBlock::Metadata {
                lines: lines
                    .into_iter()
                    .map(|line| normalise_terminal_text(&line))
                    .collect(),
            },
            Self::Accounting { text } => NormalisedBlock::Accounting {
                text: normalise_terminal_text(&text),
            },
        }
    }
}

pub(crate) fn page_style(theme: &Theme) -> Style {
    Style::reset().fg(theme.text).bg(Color::Reset)
}

pub(crate) fn muted_page_style(theme: &Theme) -> Style {
    page_style(theme).fg(theme.muted)
}

fn card_style(theme: &Theme, background: Color) -> Style {
    Style::reset().fg(theme.text).bg(background)
}

fn muted_card_style(theme: &Theme, background: Color) -> Style {
    Style::reset().fg(theme.muted).bg(background)
}

fn dim_card_style(theme: &Theme, background: Color) -> Style {
    Style::reset().fg(theme.dim).bg(background)
}

fn effective_padding(width: u16, wanted: u16) -> (u16, u16) {
    let total = wanted.saturating_mul(2).min(width.saturating_sub(1));
    let left = wanted.min(total.saturating_add(1) / 2);
    (left, total.saturating_sub(left))
}

fn logical_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    text.strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .collect()
}

const CODE_PREVIEW_LINES: usize = 10;
const CALL_PREVIEW_COUNT: usize = 8;
const OUTPUT_PREVIEW_LINES: usize = 8;

fn expand_hint(theme: &Theme, background: Color, count: usize, singular: &str) -> Line<'static> {
    let plural = if count == 1 { "" } else { "s" };
    Line::styled(
        format!("\u{2026} {count} {singular}{plural} \u{b7} Ctrl-O to expand"),
        dim_card_style(theme, background),
    )
}

fn code_text(
    theme: &Theme,
    source: &str,
    expanded: bool,
    live: bool,
    background: Color,
    highlighter: &mut PythonHighlighter,
) -> Text<'static> {
    let style = card_style(theme, background);
    let lines = logical_lines(source);
    let omitted = lines.len().saturating_sub(CODE_PREVIEW_LINES);
    if expanded || omitted == 0 {
        return own_styled_text(highlighter.highlight(source, theme), style);
    }
    if live {
        let mut text = own_styled_text(
            highlighter.highlight(&lines[omitted..].join("\n"), theme),
            style,
        );
        text.lines
            .insert(0, expand_hint(theme, background, omitted, "earlier line"));
        text
    } else {
        let mut text = own_styled_text(
            highlighter.highlight(&lines[..CODE_PREVIEW_LINES].join("\n"), theme),
            style,
        );
        text.lines
            .push(expand_hint(theme, background, omitted, "more line"));
        text
    }
}

fn tool_call_text(
    theme: &Theme,
    tool_calls: &[ToolCall],
    run_previews: &[Option<RunPreview>],
    expanded: bool,
    background: Color,
    width: u16,
) -> Text<'static> {
    let omitted = if expanded {
        0
    } else {
        tool_calls.len().saturating_sub(CALL_PREVIEW_COUNT)
    };
    let name_width = tool_calls
        .iter()
        .skip(omitted)
        .filter(|call| call.subject.is_some())
        .map(|call| call.tool.label().len())
        .max()
        .unwrap_or_default();
    let row_width = (!expanded).then_some(usize::from(width));
    let mut lines = Vec::new();
    if omitted > 0 {
        lines.push(expand_hint(theme, background, omitted, "earlier call"));
    }
    for (index, call) in tool_calls.iter().enumerate().skip(omitted) {
        let preview = run_previews.get(index).and_then(Option::as_ref);
        lines.push(tool_call_line(
            theme, call, name_width, row_width, background,
        ));
        if let Some(preview) = preview.filter(|_| call.summary == ToolCallSummary::Pending) {
            lines.extend(
                logical_lines(&preview.stdout).into_iter().map(|line| {
                    Line::styled(format!("  {line}"), dim_card_style(theme, background))
                }),
            );
        }
    }
    Text::from(lines)
}

fn tool_call_line(
    theme: &Theme,
    call: &ToolCall,
    name_width: usize,
    row_width: Option<usize>,
    background: Color,
) -> Line<'static> {
    let (glyph, colour) = match call.summary {
        ToolCallSummary::Pending => ("\u{2026}", theme.warning),
        ToolCallSummary::Failed | ToolCallSummary::Signal => ("\u{2717}", theme.error),
        ToolCallSummary::Exit(code) if code != 0 => ("\u{2717}", theme.error),
        _ => ("\u{2713}", theme.success),
    };
    let style = card_style(theme, background);
    let name = match call.subject {
        Some(_) => format!("{:name_width$}  ", call.tool.label()),
        None => call.tool.label().to_owned(),
    };
    let mut head = vec![
        Span::styled(glyph, style.fg(colour)),
        Span::styled(" ", style),
        Span::styled(name, style.fg(theme.accent)),
    ];
    let mut tail = Vec::new();
    if let Some(summary) = tool_call_summary(call) {
        tail.push(Span::styled(
            format!(" \u{b7} {summary}"),
            muted_card_style(theme, background),
        ));
    }
    if let Some(duration) = call.duration {
        tail.push(Span::styled(
            format!("  {}", format_call_duration(duration)),
            dim_card_style(theme, background),
        ));
    }
    if let Some(subject) = &call.subject {
        let subject = match row_width {
            Some(row_width) => {
                let used = Line::from(head.clone()).width() + Line::from(tail.clone()).width();
                truncate_end(subject, row_width.saturating_sub(used))
            }
            None => subject.clone(),
        };
        head.push(Span::styled(subject, style));
    }
    head.extend(tail);
    let line = Line::from(head);
    match row_width {
        Some(row_width) if line.width() > row_width => truncate_line(line, row_width),
        _ => line,
    }
}

fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    let mut remaining = width;
    let mut spans = Vec::new();
    for span in line.spans {
        if remaining == 0 {
            break;
        }
        let span_width = span.width();
        if span_width <= remaining {
            remaining -= span_width;
            spans.push(span);
        } else {
            spans.push(Span::styled(
                truncate_end(&span.content, remaining),
                span.style,
            ));
            break;
        }
    }
    Line::from(spans)
}

fn format_call_duration(duration: std::time::Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        let seconds = duration.as_secs();
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

fn format_size(bytes: usize) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1000.0;
    for unit in ["KB", "MB", "GB"] {
        if value < 9.95 {
            return format!("{value:.1} {unit}");
        }
        if value < 999.5 || unit == "GB" {
            return format!("{value:.0} {unit}");
        }
        value /= 1000.0;
    }
    unreachable!("the last unit always returns")
}

fn call_count_text(tool: ToolName, count: usize) -> String {
    match tool {
        ToolName::Read => format_size(count),
        _ => format!("{count} {}", tool.count_label(count)),
    }
}

fn tool_call_summary(call: &ToolCall) -> Option<String> {
    Some(match &call.summary {
        ToolCallSummary::Pending => return None,
        ToolCallSummary::Failed => "failed".to_owned(),
        ToolCallSummary::Exit(code) => format!("exit {code}"),
        ToolCallSummary::Signal => "signal".to_owned(),
        ToolCallSummary::Count(count) => call_count_text(call.tool, *count),
        ToolCallSummary::Created => "created".to_owned(),
        ToolCallSummary::Replaced => "replaced".to_owned(),
        ToolCallSummary::Applied => "applied".to_owned(),
        ToolCallSummary::Subagent(id) if call.subject.is_none() => format!("subagent:{id}"),
        ToolCallSummary::Subagent(_) => return None,
    })
}

pub(crate) fn tool_call_label(call: &ToolCall) -> String {
    if let ToolCallSummary::Subagent(id) = &call.summary {
        let label = match call.tool {
            ToolName::StartSubagent => "Spawned",
            ToolName::GetSubagent | ToolName::RefreshSubagent => "Refreshed",
            ToolName::WaitSubagent => "Waiting",
            ToolName::CancelSubagent => "Cancelled",
            _ => unreachable!("only subagent tools return subagent summaries"),
        };
        return format!("{label} · subagent:{id}");
    }
    let summary = match &call.summary {
        ToolCallSummary::Pending => "pending".to_owned(),
        ToolCallSummary::Failed => "failed".to_owned(),
        ToolCallSummary::Exit(code) => format!("exit {code}"),
        ToolCallSummary::Signal => "signal".to_owned(),
        ToolCallSummary::Count(count) => call_count_text(call.tool, *count),
        ToolCallSummary::Created => "created".to_owned(),
        ToolCallSummary::Replaced => "replaced".to_owned(),
        ToolCallSummary::Applied => "applied".to_owned(),
        ToolCallSummary::Subagent(_) => unreachable!("subagent summaries return above"),
    };
    format!("{} · {summary}", call.tool.label())
}

fn finished_subagent_block(record: &TaskRecord) -> TranscriptBlock {
    let observation = record.observation();
    let (state, message) = match record.outcome() {
        Some(TaskOutcome::Completed(result)) => (TaskState::Completed, Some(result.message())),
        Some(TaskOutcome::Failed(failure)) => (TaskState::Failed, Some(failure.message())),
        Some(TaskOutcome::Cancelled) => (TaskState::Cancelled, None),
        Some(TaskOutcome::TimedOut) => (TaskState::TimedOut, None),
        None => (
            observation.state(),
            (observation.state() == TaskState::Failed)
                .then(|| observation.error())
                .flatten(),
        ),
    };
    let mut summary = vec![
        record.parameters().selection().model().id.to_owned(),
        format_subagent_steps(observation.step_count(), subagent_retry_count(record)),
        format_subagent_elapsed(observation.elapsed()),
    ];
    summary.extend(format_subagent_cost(observation.cost()));
    if let Some(parent) = record.parent() {
        summary.push(format!("parent subagent:{parent}"));
    }
    TranscriptBlock::SubagentFinished(SubagentFinishedCard {
        id: record.id().to_string(),
        state,
        summary: summary.join(" \u{b7} "),
        prompt: record.parameters().prompt().to_owned(),
        message: message.map(str::to_owned),
    })
}

pub(crate) fn subagent_glyph(theme: &Theme, state: TaskState) -> Span<'static> {
    let (glyph, colour) = match state {
        TaskState::Starting | TaskState::Running | TaskState::Stopping => {
            ("\u{2026}", theme.warning)
        }
        TaskState::Completed => ("\u{2713}", theme.success),
        TaskState::Failed | TaskState::TimedOut => ("\u{2717}", theme.error),
        TaskState::Cancelled => ("\u{2298}", theme.muted),
    };
    Span::styled(glyph, Style::new().fg(colour))
}

pub(crate) fn subagent_state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Starting => " starting",
        TaskState::Running | TaskState::Completed => "",
        TaskState::Stopping => " stopping",
        TaskState::Failed => " failed",
        TaskState::Cancelled => " cancelled",
        TaskState::TimedOut => " timed out",
    }
}

pub(crate) fn subagent_retry_count(record: &TaskRecord) -> u64 {
    u64::try_from(
        record
            .attempt_evidence()
            .iter()
            .filter(|attempt| attempt.provider_attempt() > 1)
            .count(),
    )
    .unwrap_or(u64::MAX)
}

pub(crate) fn format_subagent_steps(count: u64, retries: u64) -> String {
    let steps = format!("{count} {}", if count == 1 { "step" } else { "steps" });
    if retries == 0 {
        steps
    } else {
        format!(
            "{steps}, {retries} {}",
            if retries == 1 { "retry" } else { "retries" }
        )
    }
}

pub(crate) fn format_subagent_elapsed(elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

pub(crate) fn format_subagent_cost(cost: RunCostEstimate) -> Option<String> {
    match cost {
        RunCostEstimate::Complete(cost) => Some(format_footer_usd(cost.as_nanos())),
        RunCostEstimate::LowerBound(cost) => {
            Some(format!("\u{2265}{}", format_footer_usd(cost.as_nanos())))
        }
        RunCostEstimate::Unavailable => None,
    }
}

fn bounded_terminal_text(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    normalise_terminal_text(&text[..end])
}

fn output_text(
    theme: &Theme,
    content: &[ArenaContent],
    expanded: bool,
    background: Color,
) -> Text<'static> {
    let line_count = content
        .iter()
        .map(|section| logical_lines(&section.text).len())
        .sum::<usize>();
    let omitted = if expanded {
        0
    } else {
        line_count.saturating_sub(OUTPUT_PREVIEW_LINES)
    };
    let mut skipped = omitted;
    let mut lines = Vec::new();
    if omitted > 0 {
        lines.push(expand_hint(theme, background, omitted, "earlier line"));
    }
    for section in content {
        let section_lines = logical_lines(&section.text);
        let skip = skipped.min(section_lines.len());
        skipped -= skip;
        if skip == section_lines.len() {
            continue;
        }
        if section.label != "stdout" {
            lines.push(Line::styled(
                section.label,
                dim_card_style(theme, background),
            ));
        }
        let colour = match section.label {
            "error" | "type check" | "failure" => theme.error,
            "stderr" | "warning" => theme.warning,
            _ => theme.text,
        };
        let style = card_style(theme, background).fg(colour);
        lines.extend(
            section_lines[skip..]
                .iter()
                .map(|line| Line::styled((*line).to_owned(), style)),
        );
    }
    Text::from(lines)
}

fn text_style(style: Style) -> Style {
    Style::new()
        .fg(style.fg.unwrap_or(Color::Reset))
        .bg(style.bg.unwrap_or(Color::Reset))
        .add_modifier(style.add_modifier)
}

fn own_styled_text(text: Text<'_>, style: Style) -> Text<'static> {
    let base_style = text_style(style);
    Text {
        alignment: text.alignment,
        style: base_style,
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                style: base_style.patch(line.style),
                alignment: line.alignment,
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| {
                        Span::styled(span.content.into_owned(), base_style.patch(span.style))
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn accounting_line(text: String, style: Style) -> Line<'static> {
    Line::styled(text, style).alignment(Alignment::Right)
}

fn assistant_text(
    theme: &'static Theme,
    markdown_source: &str,
    accounting: Option<String>,
    #[cfg(test)] construction_counts: Option<&mut ConstructionCounts>,
) -> Text<'static> {
    let mut text = own_styled_text(
        markdown(
            markdown_source,
            theme,
            #[cfg(test)]
            construction_counts,
        ),
        page_style(theme),
    );
    if let Some(accounting) = accounting {
        if !markdown_source.is_empty() {
            text.lines.push(Line::raw(""));
        }
        text.lines
            .push(accounting_line(accounting, muted_page_style(theme)));
    }
    text
}

fn literal_text(text: &str, style: Style) -> Text<'static> {
    Text::from(
        text.split('\n')
            .map(|line| Line::styled(line.to_owned(), style))
            .collect::<Vec<_>>(),
    )
}

const SUBAGENT_PREVIEW_ROWS: u16 = 3;

fn subagent_preview(text: &str, width: u16, expanded: bool) -> (Vec<Line<'static>>, bool) {
    let full = || {
        text.split('\n')
            .map(|line| Line::raw(line.to_owned()))
            .collect::<Vec<_>>()
    };
    if expanded || width == 0 {
        return (full(), false);
    }

    let paragraph = Paragraph::new(Text::raw(text.to_owned())).wrap(Wrap { trim: false });
    if paragraph.line_count(width) <= usize::from(SUBAGENT_PREVIEW_ROWS) {
        return (full(), false);
    }

    let area = Rect::new(0, 0, width, SUBAGENT_PREVIEW_ROWS);
    let mut buffer = Buffer::empty(area);
    paragraph.render(area, &mut buffer);
    let rows = (0..SUBAGENT_PREVIEW_ROWS)
        .map(|row| {
            let mut text = String::new();
            let mut column = 0;
            while column < width {
                let cell = &buffer[(column, row)];
                text.push_str(cell.symbol());
                column = column.saturating_add(cell.cell_width().max(1));
            }
            Line::raw(text.trim_end().to_owned())
        })
        .collect();
    (rows, true)
}

fn subagent_finished_text(
    theme: &Theme,
    card: &SubagentFinishedCard,
    width: u16,
    expanded: bool,
) -> Text<'static> {
    let background = subagent_background(theme, card.state);
    let (left_padding, right_padding) = effective_padding(width, 1);
    let content_width = width.saturating_sub(left_padding.saturating_add(right_padding));
    let dim = dim_card_style(theme, background);
    let mut lines = vec![Line::from(vec![
        subagent_glyph(theme, card.state),
        Span::styled(
            format!(" subagent:{}{}", card.id, subagent_state_label(card.state)),
            card_style(theme, background),
        ),
        Span::styled(
            format!(" \u{b7} {}", card.summary),
            muted_card_style(theme, background),
        ),
    ])];
    if expanded {
        lines.extend(literal_text(&card.prompt, dim).lines);
    } else {
        let prompt = card.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
        lines.push(Line::styled(
            truncate_end(&prompt, usize::from(content_width)),
            dim,
        ));
    }
    if let Some(message) = &card.message {
        let (preview, omitted) = subagent_preview(message, content_width, expanded);
        lines.extend(preview);
        if omitted {
            lines.push(Line::styled("\u{2026} Ctrl-O to expand", dim));
        }
    }
    Text::from(lines)
}

fn subagent_background(theme: &Theme, state: TaskState) -> Color {
    match state {
        TaskState::Completed => theme.tool_success_bg,
        TaskState::Failed | TaskState::TimedOut => theme.tool_error_bg,
        TaskState::Starting | TaskState::Running | TaskState::Stopping | TaskState::Cancelled => {
            theme.tool_pending_bg
        }
    }
}

#[cfg(test)]
fn subagent_finished_plain_text(card: &SubagentFinishedCard) -> String {
    let mut lines = vec![
        format!(
            "subagent:{}{} \u{b7} {}",
            card.id,
            subagent_state_label(card.state),
            card.summary
        ),
        card.prompt.clone(),
    ];
    if let Some(message) = &card.message {
        lines.push(message.clone());
    }
    lines.join("\n")
}

struct VerticalClip {
    top: usize,
    bottom: usize,
}

fn vertical_clip(
    content_top: usize,
    content_height: usize,
    scroll_top: usize,
    viewport_height: u16,
) -> Option<VerticalClip> {
    let top = content_top.max(scroll_top);
    let bottom = content_top
        .saturating_add(content_height)
        .min(scroll_top.saturating_add(usize::from(viewport_height)));
    (top < bottom).then_some(VerticalClip { top, bottom })
}

struct TextPlacement {
    width: u16,
    viewport: Rect,
    top: usize,
    text_top: usize,
    x: u16,
}

fn selectable_text_regions(
    text: &Text<'_>,
    placement: TextPlacement,
    card: CardId,
    buffer: &Buffer,
) -> Vec<SelectableRegion> {
    if placement.width == 0 {
        return Vec::new();
    }

    let mut regions = Vec::new();
    let mut line_top = placement.text_top;
    for line in &text.lines {
        let line_text = Text::from(line.clone());
        let line_height = Paragraph::new(line.clone())
            .wrap(Wrap { trim: false })
            .line_count(placement.width);
        if let Some(clip) = vertical_clip(
            line_top,
            line_height,
            placement.top,
            placement.viewport.height,
        ) {
            let breaks = RowBreaks::for_rows(
                &line_text,
                placement.width,
                clip.top.saturating_sub(line_top),
                clip.bottom.saturating_sub(clip.top),
            );
            let y = placement.viewport.y.saturating_add(
                u16::try_from(clip.top.saturating_sub(placement.top)).unwrap_or(u16::MAX),
            );
            let height = u16::try_from(clip.bottom.saturating_sub(clip.top)).unwrap_or(u16::MAX);
            let area = Rect::new(
                placement.viewport.x.saturating_add(placement.x),
                y,
                placement.width,
                height,
            );
            if matches!(line.alignment.or(text.alignment), Some(Alignment::Right)) {
                for row in area.top()..area.bottom() {
                    if let Some(area) = rendered_content_area(buffer, area, row) {
                        regions.push(SelectableRegion::new(area, line_top, breaks.clone(), card));
                    }
                }
            } else {
                regions.push(SelectableRegion::new(area, line_top, breaks, card));
            }
        }
        line_top = line_top.saturating_add(line_height);
    }
    regions
}

fn rendered_content_area(buffer: &Buffer, area: Rect, y: u16) -> Option<Rect> {
    let mut x = area.x;
    let mut left = None;
    let mut right = area.x;
    while x < area.right() {
        let cell = &buffer[(x, y)];
        let end = x.saturating_add(cell.cell_width().max(1)).min(area.right());
        if cell.symbol() != " " {
            left.get_or_insert(x);
            right = end;
        }
        x = end;
    }
    left.map(|left| Rect::new(left, y, right.saturating_sub(left), 1))
}

struct ArenaText {
    text: Text<'static>,
    style: Style,
    width: u16,
    height: usize,
}

impl ArenaText {
    fn new(text: Text<'static>, style: Style, width: u16) -> Self {
        let height = if width == 0 {
            0
        } else {
            Paragraph::new(text.clone())
                .style(style)
                .wrap(Wrap { trim: false })
                .line_count(width)
        };
        Self {
            text,
            style,
            width,
            height,
        }
    }

    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        text_top: usize,
        x: u16,
        card: CardId,
        buffer: &Buffer,
    ) -> Vec<SelectableRegion> {
        selectable_text_regions(
            &self.text,
            TextPlacement {
                width: self.width,
                viewport,
                top,
                text_top,
                x,
            },
            card,
            buffer,
        )
    }

    fn render(&self, viewport: Rect, top: usize, text_top: usize, x: u16, buffer: &mut Buffer) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        let Some(clip) = vertical_clip(text_top, self.height, top, viewport.height) else {
            return;
        };
        let y = viewport
            .y
            .saturating_add(u16::try_from(clip.top.saturating_sub(top)).unwrap_or(u16::MAX));
        let height = u16::try_from(clip.bottom.saturating_sub(clip.top)).unwrap_or(u16::MAX);
        Paragraph::new(self.text.clone())
            .style(self.style)
            .wrap(Wrap { trim: false })
            .scroll((
                u16::try_from(clip.top.saturating_sub(text_top)).unwrap_or(u16::MAX),
                0,
            ))
            .render(
                Rect::new(viewport.x.saturating_add(x), y, self.width, height),
                buffer,
            );
    }
}

enum ArenaItem {
    Text(ArenaText),
    Spacer,
}

impl ArenaItem {
    fn height(&self) -> usize {
        match self {
            Self::Text(text) => text.height,
            Self::Spacer => 1,
        }
    }

    fn render(&self, viewport: Rect, top: usize, item_top: usize, x: u16, buffer: &mut Buffer) {
        match self {
            Self::Text(text) => text.render(viewport, top, item_top, x, buffer),
            Self::Spacer => {}
        }
    }

    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        item_top: usize,
        x: u16,
        card: CardId,
        buffer: &Buffer,
    ) -> Vec<SelectableRegion> {
        match self {
            Self::Text(text) => text.selectable_regions(viewport, top, item_top, x, card, buffer),
            Self::Spacer => Vec::new(),
        }
    }
}

struct ArenaRenderBlock {
    style: Style,
    content_x: u16,
    items: Vec<ArenaItem>,
    height: usize,
}

impl ArenaRenderBlock {
    fn new(
        theme: &Theme,
        arena: NormalisedArenaBlock,
        expanded: bool,
        highlighter: &mut PythonHighlighter,
        width: u16,
        #[cfg(test)] construction_counts: Option<&mut ConstructionCounts>,
    ) -> Self {
        let background = arena.state.background(theme);
        let style = card_style(theme, background);
        let content_x = u16::from(width >= 3);
        let content_width = width.saturating_sub(content_x.saturating_mul(2));
        let mut groups = Vec::new();

        let header = [
            arena
                .heading
                .map(|text| (text, card_style(theme, background).fg(theme.error))),
            arena
                .status
                .map(|text| (text, dim_card_style(theme, background))),
            arena.progress_gap.then(|| {
                (
                    "display gap: some live progress was dropped".to_owned(),
                    dim_card_style(theme, background),
                )
            }),
        ]
        .into_iter()
        .flatten()
        .filter(|(text, _)| !text.is_empty())
        .flat_map(|(text, row_style)| literal_text(&text, row_style).lines)
        .collect::<Vec<_>>();
        groups.push(Text::from(header));

        if let Some(source) = arena.source.filter(|source| !source.is_empty()) {
            #[cfg(test)]
            if let Some(construction_counts) = construction_counts {
                construction_counts.highlighting += 1;
            }
            groups.push(code_text(
                theme,
                &source,
                expanded,
                arena.state == ArenaState::InProgress,
                background,
                highlighter,
            ));
        }
        groups.push(tool_call_text(
            theme,
            &arena.tool_calls,
            &arena.run_previews,
            expanded,
            background,
            content_width,
        ));
        groups.push(output_text(theme, &arena.content, expanded, background));
        if let Some(accounting) = arena.accounting.filter(|accounting| !accounting.is_empty()) {
            groups.push(Text::from(accounting_line(
                accounting,
                muted_card_style(theme, background),
            )));
        }

        let mut items = vec![ArenaItem::Spacer];
        for (index, group) in groups
            .into_iter()
            .filter(|group| !group.lines.is_empty())
            .enumerate()
        {
            if index > 0 {
                items.push(ArenaItem::Spacer);
            }
            items.push(ArenaItem::Text(ArenaText::new(group, style, content_width)));
        }
        items.push(ArenaItem::Spacer);

        let height = items
            .iter()
            .fold(0_usize, |height, item| height.saturating_add(item.height()));
        Self {
            style,
            content_x,
            items,
            height,
        }
    }

    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        block_top: usize,
        card: CardId,
        buffer: &Buffer,
    ) -> Vec<SelectableRegion> {
        let mut regions = Vec::new();
        let mut item_top = block_top;
        for item in &self.items {
            regions.extend(item.selectable_regions(
                viewport,
                top,
                item_top,
                self.content_x,
                card,
                buffer,
            ));
            item_top = item_top.saturating_add(item.height());
        }
        regions
    }

    fn render(&self, viewport: Rect, top: usize, block_top: usize, buffer: &mut Buffer) {
        let Some(clip) = vertical_clip(block_top, self.height, top, viewport.height) else {
            return;
        };
        let y = viewport
            .y
            .saturating_add(u16::try_from(clip.top.saturating_sub(top)).unwrap_or(u16::MAX));
        let height = u16::try_from(clip.bottom.saturating_sub(clip.top)).unwrap_or(u16::MAX);
        buffer.set_style(Rect::new(viewport.x, y, viewport.width, height), self.style);
        let mut item_top = block_top;
        for item in &self.items {
            item.render(viewport, top, item_top, self.content_x, buffer);
            item_top = item_top.saturating_add(item.height());
        }
    }
}

#[cfg(test)]
impl NormalisedBlock {
    fn paragraph(&self) -> Paragraph<'_> {
        let text = match self {
            Self::Reasoning { markdown: source } => markdown(source, &crate::theme::DARK, None),
            Self::Assistant {
                markdown: source,
                accounting,
            } => assistant_text(&crate::theme::DARK, source, accounting.clone(), None),
            _ => Text::raw(self.plain_text()),
        };
        Paragraph::new(text).wrap(Wrap { trim: false })
    }

    fn plain_text(&self) -> String {
        match self {
            Self::Startup { lines } | Self::Metadata { lines } => lines.join("\n"),
            Self::ModelChange { text } => text.clone(),
            Self::User { request } => request.clone(),
            Self::SubagentFinished(card) => subagent_finished_plain_text(card),
            Self::Compaction(card) => format!("{}\n{}", card.heading, card.collapsed),
            Self::SkillActivation(card) => card.heading.clone(),
            Self::Arena(arena) => arena.plain_text(),
            Self::Status { text } => text.clone(),
            Self::SessionRestored => "Session restored.".to_owned(),
            Self::Accounting { text } => format!("\n{text}"),
            Self::Reasoning { .. } | Self::Assistant { .. } => unreachable!("markdown block"),
        }
    }
}

#[cfg(test)]
impl NormalisedArenaBlock {
    fn plain_text(&self) -> String {
        let mut lines = Vec::new();
        if let Some(heading) = &self.heading {
            lines.push(heading.clone());
        }
        if let Some(status) = &self.status {
            lines.push(status.clone());
        }
        append_display_gap(&mut lines, self.progress_gap);
        if let Some(source) = &self.source {
            lines.push(format!("code:\n{source}"));
        }
        if !self.tool_calls.is_empty() {
            let tool_calls = tool_call_text(
                &crate::theme::DARK,
                &self.tool_calls,
                &self.run_previews,
                false,
                Color::Reset,
                u16::MAX,
            );
            lines.push(format!("tool calls:\n{tool_calls}"));
        }
        if self.source.is_some() && !self.content.is_empty() {
            lines.push(String::new());
        }
        lines.extend(
            self.content
                .iter()
                .map(|content| format!("{}: {}", content.label, content.text)),
        );
        if let Some(accounting) = &self.accounting {
            lines.push(String::new());
            lines.push(accounting.clone());
        }
        lines.join("\n")
    }
}

enum RenderBlock {
    Card(CardRenderBlock),
    Arena(ArenaRenderBlock),
}

impl RenderBlock {
    fn card(
        text: Text<'static>,
        style: Style,
        width: u16,
        horizontal_padding: u16,
        vertical_padding: u16,
    ) -> Self {
        let (left_padding, right_padding) = effective_padding(width, horizontal_padding);
        let content_width = width.saturating_sub(left_padding.saturating_add(right_padding));
        let content_height = if content_width > 0 {
            Paragraph::new(text.clone())
                .style(style)
                .wrap(Wrap { trim: false })
                .line_count(content_width)
        } else {
            0
        };
        Self::Card(CardRenderBlock {
            text,
            style,
            left_padding,
            right_padding,
            top_padding: vertical_padding,
            bottom_padding: vertical_padding,
            height: content_height.saturating_add(usize::from(vertical_padding).saturating_mul(2)),
        })
    }

    fn height(&self) -> usize {
        match self {
            Self::Card(card) => card.height,
            Self::Arena(arena) => arena.height,
        }
    }

    fn render(&self, viewport: Rect, top: usize, block_top: usize, buffer: &mut Buffer) {
        match self {
            Self::Card(card) => card.render(viewport, top, block_top, buffer),
            Self::Arena(arena) => arena.render(viewport, top, block_top, buffer),
        }
    }

    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        block_top: usize,
        card: CardId,
        buffer: &Buffer,
    ) -> Vec<SelectableRegion> {
        match self {
            Self::Card(block) => block.selectable_regions(viewport, top, block_top, card, buffer),
            Self::Arena(block) => block.selectable_regions(viewport, top, block_top, card, buffer),
        }
    }
}

struct CardRenderBlock {
    text: Text<'static>,
    style: Style,
    left_padding: u16,
    right_padding: u16,
    top_padding: u16,
    bottom_padding: u16,
    height: usize,
}

impl CardRenderBlock {
    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        block_top: usize,
        card: CardId,
        buffer: &Buffer,
    ) -> Vec<SelectableRegion> {
        let text_width = viewport
            .width
            .saturating_sub(self.left_padding.saturating_add(self.right_padding));
        selectable_text_regions(
            &self.text,
            TextPlacement {
                width: text_width,
                viewport,
                top,
                text_top: block_top.saturating_add(usize::from(self.top_padding)),
                x: self.left_padding,
            },
            card,
            buffer,
        )
    }

    fn render(&self, viewport: Rect, top: usize, block_top: usize, buffer: &mut Buffer) {
        let block_bottom = block_top.saturating_add(self.height);
        let Some(clip) = vertical_clip(block_top, self.height, top, viewport.height) else {
            return;
        };
        let y = viewport
            .y
            .saturating_add(u16::try_from(clip.top.saturating_sub(top)).unwrap_or(u16::MAX));
        let height = u16::try_from(clip.bottom.saturating_sub(clip.top)).unwrap_or(u16::MAX);
        let visible = Rect::new(viewport.x, y, viewport.width, height);
        buffer.set_style(visible, self.style);

        let text_top = block_top.saturating_add(usize::from(self.top_padding));
        let text_bottom = block_bottom.saturating_sub(usize::from(self.bottom_padding));
        let visible_text_top = clip.top.max(text_top);
        let visible_text_bottom = clip.bottom.min(text_bottom);
        if visible_text_top >= visible_text_bottom {
            return;
        }
        let text_y = viewport.y.saturating_add(
            u16::try_from(visible_text_top.saturating_sub(top)).unwrap_or(u16::MAX),
        );
        let text_height =
            u16::try_from(visible_text_bottom.saturating_sub(visible_text_top)).unwrap_or(u16::MAX);
        let text_width = viewport
            .width
            .saturating_sub(self.left_padding.saturating_add(self.right_padding));
        if text_width == 0 {
            return;
        }
        Paragraph::new(self.text.clone())
            .style(self.style)
            .wrap(Wrap { trim: false })
            .scroll((
                u16::try_from(visible_text_top.saturating_sub(text_top)).unwrap_or(u16::MAX),
                0,
            ))
            .render(
                Rect::new(
                    viewport.x.saturating_add(self.left_padding),
                    text_y,
                    text_width,
                    text_height,
                ),
                buffer,
            );
    }
}

#[derive(Clone)]
struct FooterField {
    spans: Vec<Span<'static>>,
}

impl FooterField {
    fn muted(theme: &Theme, text: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::styled(text.into(), muted_page_style(theme))],
        }
    }

    fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }
}

#[derive(Clone)]
struct FooterIdentity {
    cwd: Option<String>,
    name: Option<String>,
}

impl FooterIdentity {
    fn full(&self) -> String {
        match (&self.cwd, &self.name) {
            (Some(cwd), Some(name)) => format!("{cwd} ({name})"),
            (Some(cwd), None) => cwd.clone(),
            (None, Some(name)) => format!("({name})"),
            (None, None) => String::new(),
        }
    }

    fn preferred(&self) -> String {
        match (&self.cwd, &self.name) {
            (Some(cwd), Some(name)) => format!("{} ({name})", preferred_cwd(cwd)),
            (Some(cwd), None) => preferred_cwd(cwd),
            (None, Some(name)) => format!("({name})"),
            (None, None) => String::new(),
        }
    }

    fn truncated(&self, theme: &Theme, width: usize) -> Option<FooterField> {
        let full = self.full();
        if Line::raw(&full).width() <= width {
            return Some(FooterField::muted(theme, full));
        }
        let text = match (&self.cwd, &self.name) {
            (Some(cwd), Some(name)) => {
                let name = format!("({name})");
                let cwd_width = width.saturating_sub(Line::raw(&name).width().saturating_add(1));
                let cwd = truncate_cwd(cwd, cwd_width);
                if cwd.is_empty() {
                    truncate_end(&name, width)
                } else {
                    format!("{cwd} {name}")
                }
            }
            (Some(cwd), None) => truncate_cwd(cwd, width),
            (None, Some(name)) => truncate_end(&format!("({name})"), width),
            (None, None) => String::new(),
        };
        (!text.is_empty()).then(|| FooterField::muted(theme, text))
    }
}

#[derive(Clone)]
struct ModelFooter {
    id: String,
    thinking: String,
}

impl ModelFooter {
    fn full(&self) -> String {
        format!("{} • thinking {}", self.id, self.thinking)
    }

    fn compact(&self, theme: &Theme) -> FooterField {
        FooterField::muted(
            theme,
            format!(
                "{} • thinking {}",
                unqualified_model_id(&self.id),
                self.thinking
            ),
        )
    }

    fn truncated(&self, theme: &Theme, width: usize) -> Option<FooterField> {
        let model = unqualified_model_id(&self.id);
        let thinking = format!("thinking {}", self.thinking);
        let separator = " • ";
        let full = self.full();
        let text = if Line::raw(&full).width() <= width {
            full
        } else if width >= 1 + Line::raw(separator).width() + Line::raw(&thinking).width() {
            format!(
                "{}{}{}",
                truncate_front(
                    model,
                    width - Line::raw(separator).width() - Line::raw(&thinking).width()
                ),
                separator,
                thinking
            )
        } else {
            truncate_end(&thinking, width)
        };
        (!text.is_empty()).then(|| FooterField::muted(theme, text))
    }
}

#[derive(Clone)]
pub(crate) struct FooterContext {
    pub(crate) workspace_root: Option<String>,
    pub(crate) home: Option<PathBuf>,
    pub(crate) name: Option<String>,
    pub(crate) selection: ModelSelection,
}

struct Footer {
    identity: Option<FooterIdentity>,
    statistics: Vec<FooterField>,
    model: Option<ModelFooter>,
}

struct FooterLayout {
    identity: Option<FooterField>,
    right: Vec<FooterField>,
    statistics_len: usize,
}

impl FooterLayout {
    fn right_spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for (index, field) in self.right.iter().enumerate() {
            if index > 0 {
                let separator = if index == self.statistics_len && self.statistics_len > 0 {
                    "  "
                } else {
                    " "
                };
                spans.push(Span::styled(separator, muted_page_style(theme)));
            }
            spans.extend(field.spans.clone());
        }
        spans
    }

    #[cfg(test)]
    fn spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        let mut spans = self
            .identity
            .as_ref()
            .map(|field| field.spans.clone())
            .unwrap_or_default();
        if self.identity.is_some() && !self.right.is_empty() {
            spans.push(Span::styled(" ", muted_page_style(theme)));
        }
        spans.extend(self.right_spans(theme));
        spans
    }

    fn rule_spans(&self, theme: &Theme, width: usize) -> Vec<Span<'static>> {
        let right = self.right_spans(theme);
        let right_width = right.iter().map(Span::width).sum::<usize>();
        let identity_width = self.identity.as_ref().map_or(0, FooterField::width);
        let seams = footer_seam_width(self.identity.is_some(), !right.is_empty());
        let mut spans = self
            .identity
            .as_ref()
            .map(|field| field.spans.clone())
            .unwrap_or_default();
        if self.identity.is_some() {
            spans.push(Span::styled(" ", muted_page_style(theme)));
        }
        spans.push(Span::raw(
            "─".repeat(
                width.saturating_sub(
                    identity_width
                        .saturating_add(right_width)
                        .saturating_add(seams),
                ),
            ),
        ));
        if !right.is_empty() {
            spans.push(Span::styled(" ", muted_page_style(theme)));
        }
        spans.extend(right);
        spans
    }
}

impl Footer {
    fn new(
        identity: Option<FooterIdentity>,
        statistics: Vec<FooterField>,
        model: Option<ModelFooter>,
    ) -> Option<Self> {
        (identity.is_some() || !statistics.is_empty() || model.is_some()).then_some(Self {
            identity,
            statistics,
            model,
        })
    }

    #[cfg(test)]
    fn spans(&self, theme: &Theme) -> Vec<Span<'static>> {
        self.layout(theme, usize::MAX).spans(theme)
    }

    fn layout(&self, theme: &Theme, width: usize) -> FooterLayout {
        let mut statistics = self.statistics.clone();
        let mut model = self
            .model
            .as_ref()
            .and_then(|model| model.truncated(theme, usize::MAX));
        let full_identity = self.identity.as_ref().and_then(|identity| {
            let text = identity.full();
            (!text.is_empty()).then(|| FooterField::muted(theme, text))
        });
        let preferred_identity = self.identity.as_ref().and_then(|identity| {
            let text = identity.preferred();
            (!text.is_empty()).then(|| FooterField::muted(theme, text))
        });
        let identity = if footer_fits(width, full_identity.as_ref(), &statistics, model.as_ref()) {
            full_identity
        } else {
            if !footer_fits(
                width,
                preferred_identity.as_ref(),
                &statistics,
                model.as_ref(),
            ) {
                model = self.model.as_ref().map(|model| model.compact(theme));
            }
            while !statistics.is_empty()
                && !footer_fits(
                    width,
                    preferred_identity.as_ref(),
                    &statistics,
                    model.as_ref(),
                )
            {
                statistics.remove(0);
            }
            if footer_fits(
                width,
                preferred_identity.as_ref(),
                &statistics,
                model.as_ref(),
            ) {
                preferred_identity
            } else {
                let right_width = right_width(&statistics, model.as_ref());
                let right_seam = footer_seam_width(false, right_width > 0);
                if right_width.saturating_add(right_seam) > width {
                    model = self
                        .model
                        .as_ref()
                        .and_then(|model| model.truncated(theme, width.saturating_sub(right_seam)));
                    None
                } else {
                    self.identity.as_ref().and_then(|identity| {
                        identity.truncated(
                            theme,
                            width
                                .saturating_sub(right_width)
                                .saturating_sub(footer_seam_width(true, right_width > 0)),
                        )
                    })
                }
            }
        };
        let statistics_len = statistics.len();
        let mut right = statistics;
        if let Some(model) = model {
            right.push(model);
        }
        FooterLayout {
            identity,
            right,
            statistics_len,
        }
    }
}

fn footer_fits(
    width: usize,
    identity: Option<&FooterField>,
    statistics: &[FooterField],
    model: Option<&FooterField>,
) -> bool {
    let right_width = right_width(statistics, model);
    identity
        .map_or(0, FooterField::width)
        .saturating_add(right_width)
        .saturating_add(footer_seam_width(identity.is_some(), right_width > 0))
        <= width
}

fn footer_seam_width(has_identity: bool, has_right: bool) -> usize {
    usize::from(has_identity) + usize::from(has_right)
}

fn right_width(statistics: &[FooterField], model: Option<&FooterField>) -> usize {
    let statistics_width = statistics.iter().map(FooterField::width).sum::<usize>()
        + statistics.len().saturating_sub(1);
    match model {
        Some(model) if statistics_width > 0 => statistics_width + 2 + model.width(),
        Some(model) => model.width(),
        None => statistics_width,
    }
}

fn unqualified_model_id(id: &str) -> &str {
    id.split_once('/').map_or(id, |(_, model)| model)
}

pub(crate) fn truncate_front(text: &str, width: usize) -> String {
    if Line::raw(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut suffix = Vec::new();
    let mut used = 1_usize;
    for grapheme in text.graphemes(true).rev() {
        let grapheme_width = Line::raw(grapheme).width();
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        suffix.push(grapheme);
        used += grapheme_width;
    }
    suffix.reverse();
    format!("…{}", suffix.concat())
}

pub(crate) fn truncate_end(text: &str, width: usize) -> String {
    if Line::raw(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut prefix = String::new();
    let mut used = 1_usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = Line::raw(grapheme).width();
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        prefix.push_str(grapheme);
        used += grapheme_width;
    }
    format!("{prefix}…")
}

fn preferred_cwd(cwd: &str) -> String {
    let basename = cwd.rsplit('/').next().unwrap_or(cwd);
    let shortened = if cwd.starts_with("~/") {
        format!("~/…/{basename}")
    } else {
        format!("…/{basename}")
    };
    if Line::raw(&shortened).width() < Line::raw(cwd).width() {
        shortened
    } else {
        cwd.to_owned()
    }
}

fn truncate_cwd(cwd: &str, width: usize) -> String {
    if Line::raw(cwd).width() <= width {
        return cwd.to_owned();
    }
    let shortened = preferred_cwd(cwd);
    if Line::raw(&shortened).width() <= width {
        shortened
    } else {
        truncate_front(cwd, width)
    }
}

struct SettledBlock {
    source: NormalisedBlock,
    raster: Option<Arc<SettledRaster>>,
}

impl SettledBlock {
    fn new(source: NormalisedBlock) -> Self {
        Self {
            source,
            raster: None,
        }
    }

    fn invalidates_for_reasoning(&self) -> bool {
        matches!(self.source, NormalisedBlock::Reasoning { .. })
    }

    fn invalidates_for_expansion(&self) -> bool {
        matches!(
            self.source,
            NormalisedBlock::Arena(..)
                | NormalisedBlock::SubagentFinished(..)
                | NormalisedBlock::Compaction(..)
                | NormalisedBlock::SkillActivation(..)
        )
    }
}

struct SettledRaster {
    theme: &'static Theme,
    buffer: Buffer,
    regions: Vec<SelectableRegion>,
}

impl SettledRaster {
    fn height(&self) -> usize {
        usize::from(self.buffer.area.height)
    }

    fn render(&self, viewport: Rect, top: usize, block_top: usize, buffer: &mut Buffer) {
        assert_eq!(
            viewport.width, self.buffer.area.width,
            "cached raster width must match viewport width"
        );
        let Some(clip) = vertical_clip(block_top, self.height(), top, viewport.height) else {
            return;
        };
        for row in clip.top..clip.bottom {
            let source_y = u16::try_from(row.saturating_sub(block_top)).unwrap_or(u16::MAX);
            let target_y = viewport
                .y
                .saturating_add(u16::try_from(row.saturating_sub(top)).unwrap_or(u16::MAX));
            for x in 0..viewport.width {
                buffer[(viewport.x.saturating_add(x), target_y)] =
                    self.buffer[(x, source_y)].clone();
            }
        }
    }

    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        block_top: usize,
        card: CardId,
    ) -> Vec<SelectableRegion> {
        self.regions
            .iter()
            .filter_map(|region| region.rebase(viewport, top, block_top, card))
            .collect()
    }
}

enum LayoutBlock {
    Cached(Arc<SettledRaster>),
    Fresh(RenderBlock),
}

impl LayoutBlock {
    fn height(&self) -> usize {
        match self {
            Self::Cached(raster) => raster.height(),
            Self::Fresh(block) => block.height(),
        }
    }

    fn render(&self, viewport: Rect, top: usize, block_top: usize, buffer: &mut Buffer) {
        match self {
            Self::Cached(raster) => raster.render(viewport, top, block_top, buffer),
            Self::Fresh(block) => block.render(viewport, top, block_top, buffer),
        }
    }

    fn selectable_regions(
        &self,
        viewport: Rect,
        top: usize,
        block_top: usize,
        card: CardId,
        buffer: &Buffer,
    ) -> Vec<SelectableRegion> {
        match self {
            Self::Cached(raster) => raster.selectable_regions(viewport, top, block_top, card),
            Self::Fresh(block) => block.selectable_regions(viewport, top, block_top, card, buffer),
        }
    }
}

const BLOCK_GAP: usize = 1;

pub(crate) struct TranscriptLayout {
    page_style: Style,
    blocks: Vec<LayoutBlock>,
    width: u16,
    total_height: usize,
    footer: Option<Footer>,
}

impl TranscriptLayout {
    pub(crate) fn total_height(&self) -> usize {
        self.total_height
    }

    pub(crate) fn footer_rule(&self, theme: &Theme, width: u16) -> Line<'static> {
        let Some(footer) = &self.footer else {
            return Line::raw("─".repeat(usize::from(width)));
        };
        match width {
            0 => Line::raw(""),
            1 => Line::from(Span::styled("…", muted_page_style(theme))),
            2 => Line::from(vec![
                Span::raw("─"),
                Span::styled("…", muted_page_style(theme)),
            ]),
            3 => Line::from(vec![
                Span::raw("─"),
                Span::styled("…", muted_page_style(theme)),
                Span::raw("─"),
            ]),
            4 => Line::from(vec![
                Span::raw("─"),
                Span::styled("…", muted_page_style(theme)),
                Span::raw(" ─"),
            ]),
            _ => {
                let layout = footer.layout(theme, usize::from(width - 4));
                let mut spans = vec![Span::raw("─ ")];
                spans.extend(layout.rule_spans(theme, usize::from(width - 4)));
                spans.push(Span::raw(" ─"));
                Line::from(spans)
            }
        }
    }

    pub(crate) fn render(&self, viewport: Rect, top: usize, buffer: &mut Buffer) {
        self.render_with_selection(viewport, top, None, buffer);
    }

    pub(crate) fn render_with_selection(
        &self,
        viewport: Rect,
        top: usize,
        selection: Option<Selection>,
        buffer: &mut Buffer,
    ) {
        if viewport.width == 0 || viewport.height == 0 {
            return;
        }
        buffer.set_style(viewport, self.page_style);
        let selection =
            selection.and_then(|selection| selection.project(Viewport::new(viewport, top)));
        let mut regions = Vec::new();
        for (index, block_top, block) in self.placed_blocks() {
            if block_top >= top.saturating_add(usize::from(viewport.height)) {
                break;
            }
            block.render(viewport, top, block_top, buffer);
            if selection.is_some() {
                regions.extend(block.selectable_regions(
                    viewport,
                    top,
                    block_top,
                    CardId::new(index),
                    buffer,
                ));
            }
        }
        if let Some(selection) = selection {
            highlight(buffer, selection, &regions);
        }
    }

    pub(crate) fn extract_selection(&self, selection: Selection) -> String {
        const CHUNK_HEIGHT: usize = 64;

        if self.width == 0 {
            return String::new();
        }

        let mut extraction = Extraction::new();
        for (index, block_top, block) in self.placed_blocks() {
            let block_bottom = block_top.saturating_add(block.height());
            let mut chunk_top = block_top;
            while chunk_top < block_bottom {
                let chunk_bottom = chunk_top.saturating_add(CHUNK_HEIGHT).min(block_bottom);
                let viewport = Rect::new(
                    0,
                    0,
                    self.width,
                    u16::try_from(chunk_bottom.saturating_sub(chunk_top)).unwrap_or(u16::MAX),
                );
                let projected = selection.project(Viewport::new(viewport, chunk_top));
                if let Some(projected) = projected {
                    let mut buffer = Buffer::empty(viewport);
                    block.render(viewport, chunk_top, block_top, &mut buffer);
                    let regions = block.selectable_regions(
                        viewport,
                        chunk_top,
                        block_top,
                        CardId::new(index),
                        &buffer,
                    );
                    extraction.append(&buffer, projected, &regions);
                }
                chunk_top = chunk_bottom;
            }
        }
        extraction.finish()
    }

    fn placed_blocks(&self) -> impl Iterator<Item = (usize, usize, &LayoutBlock)> {
        self.blocks
            .iter()
            .enumerate()
            .scan(0_usize, |next_top, (index, block)| {
                let block_top = *next_top;
                *next_top = block_top
                    .saturating_add(block.height())
                    .saturating_add(BLOCK_GAP);
                Some((index, block_top, block))
            })
    }
}

fn stacked_height(heights: impl IntoIterator<Item = usize>) -> usize {
    heights
        .into_iter()
        .fold(None, |total: Option<usize>, height| {
            Some(total.map_or(height, |total| {
                total.saturating_add(BLOCK_GAP).saturating_add(height)
            }))
        })
        .unwrap_or_default()
}

#[cfg(test)]
struct DisplayProjection {
    blocks: Vec<TranscriptBlock>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ConstructionCounts {
    normalisations: usize,
    rasterisations: usize,
    markdown: usize,
    highlighting: usize,
    render_block_builds: usize,
    selection_region_builds: usize,
}

struct PendingSubagentBlock {
    run_id: Option<RunId>,
    step_id: ModelStepId,
    block: TranscriptBlock,
}

pub(crate) struct Transcript {
    startup: Option<StartupSnapshot>,
    run_state: RunState,
    compaction_state: CompactionState,
    automatic_compaction: Option<RunId>,
    initial_overflow: Option<(RunId, ModelStepId, Vec<AttemptAccounting>)>,
    disabled_reason: Option<RuntimeDisabledReason>,
    latest_run_id: Option<RunId>,
    settled_run_ids: Vec<RunId>,
    completed_steps: Vec<(RunId, ModelStepId)>,
    pending_run_gap: bool,
    status: String,
    records: Vec<DisplayedRecord>,
    subagent_records: BTreeMap<TaskId, TaskRecord>,
    finished_subagent_cards: BTreeSet<TaskId>,
    pending_subagent_blocks: Vec<PendingSubagentBlock>,
    show_pending_subagent_blocks: bool,
    footer_adjustment: Vec<AttemptAccounting>,
    settled_blocks: Vec<SettledBlock>,
    live_step: Option<LiveStep>,
    restoration_card: bool,
    show_reasoning: bool,
    expand_arenas: bool,
    highlighter: PythonHighlighter,
    #[cfg(test)]
    construction_counts: ConstructionCounts,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            startup: None,
            run_state: RunState::Idle,
            compaction_state: CompactionState::Idle,
            automatic_compaction: None,
            initial_overflow: None,
            disabled_reason: None,
            latest_run_id: None,
            settled_run_ids: Vec::new(),
            completed_steps: Vec::new(),
            pending_run_gap: false,
            status: "idle".to_owned(),
            records: Vec::new(),
            subagent_records: BTreeMap::new(),
            finished_subagent_cards: BTreeSet::new(),
            pending_subagent_blocks: Vec::new(),
            show_pending_subagent_blocks: false,
            footer_adjustment: Vec::new(),
            settled_blocks: Vec::new(),
            live_step: None,
            restoration_card: false,
            show_reasoning: true,
            expand_arenas: false,
            highlighter: PythonHighlighter::new(),
            #[cfg(test)]
            construction_counts: ConstructionCounts::default(),
        }
    }
}

impl Transcript {
    pub(crate) fn configured(startup: StartupSnapshot) -> Self {
        let mut transcript = Self {
            startup: Some(startup),
            ..Self::default()
        };
        let block = startup_block(transcript.startup.as_ref().expect("startup was set"));
        transcript.append_settled_blocks([block]);
        transcript
    }

    pub(crate) fn restored(startup: StartupSnapshot, initial: &RuntimeInitialState) -> Self {
        let mut transcript = Self::configured(startup);
        transcript.apply_initial_state(initial);
        transcript
    }

    pub(crate) fn restored_replacement(&self, initial: &RuntimeInitialState) -> Self {
        let mut replacement = self
            .startup
            .clone()
            .map_or_else(Self::default, Self::configured);
        replacement.apply_initial_state(initial);
        replacement.show_reasoning = self.show_reasoning;
        replacement.expand_arenas = self.expand_arenas;
        replacement
    }

    pub(crate) fn append_restoration_card(&mut self) {
        self.restoration_card = true;
        self.append_settled_blocks([TranscriptBlock::SessionRestored]);
    }

    pub(crate) fn append_metadata(&mut self, lines: Vec<String>) {
        if !lines.is_empty() {
            self.append_settled_blocks([TranscriptBlock::Metadata { lines }]);
        }
    }

    fn apply_initial_state(&mut self, initial: &RuntimeInitialState) {
        for item in &initial.timeline {
            match item {
                RestoredTimelineItem::Record(record) => {
                    self.apply(
                        RuntimeEvent::RecordAppended {
                            record: record.clone(),
                            progress_gap: false,
                        },
                        false,
                    );
                }
                RestoredTimelineItem::SubagentSpawned(record) => {
                    self.replace_subagent_record(record.clone());
                }
                RestoredTimelineItem::SubagentFinished(record) => {
                    self.append_subagent_card_with_anchor(record, false);
                    self.replace_subagent_record(record.clone());
                }
            }
        }
        self.footer_adjustment = initial.footer_adjustment.clone();
        if let Some(reason) = &initial.disabled_reason {
            self.apply(
                RuntimeEvent::RuntimeDisabled {
                    reason: reason.clone(),
                },
                false,
            );
        }
    }

    pub(crate) fn begin_submission(&mut self) -> bool {
        if self.disabled_reason.is_some()
            || self.run_state != RunState::Idle
            || self.compaction_state != CompactionState::Idle
        {
            return false;
        }
        self.run_state = RunState::Submitting;
        self.set_status("starting run".to_owned())
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.run_state == RunState::Idle && self.compaction_state == CompactionState::Idle
    }

    pub(crate) fn has_foreground_compaction(&self) -> bool {
        self.compaction_state != CompactionState::Idle
    }

    pub(crate) fn has_active_run(&self) -> bool {
        self.run_state != RunState::Idle
    }

    pub(crate) fn is_generating(&self) -> bool {
        self.disabled_reason.is_none()
            && self.automatic_compaction.is_none()
            && self
                .live_step
                .as_ref()
                .is_some_and(|live| live.phase == LivePhase::Generating)
    }

    pub(crate) fn is_compacting(&self) -> bool {
        self.disabled_reason.is_none()
            && (self.compaction_state == CompactionState::Active
                || self.automatic_compaction.is_some())
    }

    pub(crate) fn is_disabled(&self) -> bool {
        self.disabled_reason.is_some()
    }

    pub(crate) fn status(&self) -> &str {
        &self.status
    }

    fn is_executing_step(&self, run_id: RunId, step_id: ModelStepId) -> bool {
        self.live_step.as_ref().is_some_and(|live| {
            live.run_id == run_id && live.step_id == step_id && live.phase == LivePhase::Executing
        })
    }

    fn has_settled_run(&self, run_id: RunId, step_id: ModelStepId) -> bool {
        self.live_step.as_ref().is_some_and(|live| {
            live.run_id == run_id
                && live.step_id == step_id
                && live.tool_calls.iter().any(|call| {
                    call.tool == ToolName::Run && !matches!(call.summary, ToolCallSummary::Pending)
                })
        })
    }

    pub(crate) fn apply(&mut self, event: RuntimeEvent, gap_before: bool) -> bool {
        match event {
            RuntimeEvent::RunStarted { run_id } => self.start_run(run_id, gap_before),
            RuntimeEvent::ModelStepStarted {
                run_id,
                step_id,
                epoch,
            } => {
                if self.automatic_compaction == Some(run_id)
                    || self.is_stale_generation(run_id, step_id, epoch)
                    || self.has_settled_run(run_id, step_id)
                {
                    false
                } else {
                    let status =
                        (!self.is_executing_step(run_id, step_id)).then(|| "generating".to_owned());
                    self.update_live_step(run_id, step_id, gap_before, status, |_| {})
                }
            }
            RuntimeEvent::ModelProgress {
                run_id,
                step_id,
                epoch,
                progress,
            } => {
                if self.automatic_compaction == Some(run_id)
                    || self.is_stale_generation(run_id, step_id, epoch)
                    || self.has_settled_run(run_id, step_id)
                {
                    false
                } else {
                    let executing = self.is_executing_step(run_id, step_id);
                    let status = (!executing).then(|| "generating".to_owned());
                    self.update_live_step(
                        run_id,
                        step_id,
                        gap_before,
                        status,
                        |live| match progress {
                            ModelProgress::AttemptStarted { attempt } => {
                                if live.attempt.is_none_or(|current| attempt > current) {
                                    live.attempt = Some(attempt);
                                    live.reasoning.clear();
                                    live.decoded_python.clear();
                                    if !executing {
                                        live.phase = LivePhase::Generating;
                                        live.assistant_text.clear();
                                        live.activity = "generating".to_owned();
                                    }
                                }
                            }
                            ModelProgress::ReasoningSummaryChunk { text } => {
                                live.reasoning.push_str(&text)
                            }
                            ModelProgress::AssistantTextChunk { text } if !executing => {
                                live.assistant_text.push_str(&text)
                            }
                            ModelProgress::AssistantTextChunk { .. } => {}
                            ModelProgress::DecodedPythonChunk { code } => {
                                live.decoded_python.push_str(&code)
                            }
                            ModelProgress::AttemptSettled { accounting, .. } => {
                                live.settled_attempts.push(accounting)
                            }
                        },
                    )
                }
            }
            RuntimeEvent::ExecutionStarted {
                run_id,
                step_id,
                commentary,
            } => {
                self.associate_pending_subagent_blocks(run_id, step_id);
                if self.has_settled_run(run_id, step_id) {
                    false
                } else {
                    self.update_live_step(
                        run_id,
                        step_id,
                        gap_before,
                        Some("running code".to_owned()),
                        |live| {
                            live.phase = LivePhase::Executing;
                            live.assistant_text = commentary.unwrap_or_default();
                            live.activity = "running code".to_owned();
                        },
                    )
                }
            }
            RuntimeEvent::ToolUpdated {
                run_id,
                step_id,
                token,
                call,
            } => {
                let activity = tool_call_label(&call);
                let mut accepted = false;
                let mut changed =
                    self.update_live_step(run_id, step_id, gap_before, None, |live| {
                        let index = live
                            .tool_call_tokens
                            .iter()
                            .position(|existing| *existing == token);
                        if index.is_some_and(|index| {
                            !matches!(live.tool_calls[index].summary, ToolCallSummary::Pending)
                                && matches!(&call.summary, ToolCallSummary::Pending)
                        }) {
                            return;
                        }
                        accepted = true;
                        live.phase = LivePhase::Executing;
                        live.activity = activity.clone();
                        if let Some(index) = index {
                            live.tool_calls[index] = call;
                        } else {
                            live.insert_tool_call(token, call, None);
                        }
                    });
                if accepted {
                    changed |= self.set_status(activity);
                }
                changed
            }
            RuntimeEvent::RunPreviewUpdated {
                run_id,
                step_id,
                token,
                preview,
            } => {
                let activity = tool_call_label(&ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                });
                let mut accepted = false;
                let mut changed =
                    self.update_live_step(run_id, step_id, gap_before, None, |live| {
                        let index = live
                            .tool_call_tokens
                            .iter()
                            .position(|existing| *existing == token);
                        if index.is_some_and(|index| {
                            !matches!(live.tool_calls[index].summary, ToolCallSummary::Pending)
                        }) {
                            return;
                        }
                        accepted = true;
                        live.phase = LivePhase::Executing;
                        live.activity = activity.clone();
                        if let Some(index) = index {
                            live.run_previews[index] = Some(preview);
                        } else {
                            live.insert_tool_call(
                                token,
                                ToolCall {
                                    tool: ToolName::Run,
                                    summary: ToolCallSummary::Pending,
                                    subject: None,
                                    duration: None,
                                },
                                Some(preview),
                            );
                        }
                    });
                if accepted {
                    changed |= self.set_status(activity);
                }
                changed
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
            RuntimeEvent::SubagentSpawned { record } => self.replace_subagent_record(record),
            RuntimeEvent::SubagentStatus { .. } => false,
            RuntimeEvent::SubagentRecord { record } => self.replace_subagent_record(record),
            RuntimeEvent::SubagentFinished { record } => {
                self.append_subagent_card_with_anchor(&record, true)
                    | self.replace_subagent_record(record)
            }
            RuntimeEvent::SubagentOperationSettled { .. } => false,
            RuntimeEvent::RunSettled { run_id } => self.settle_run(run_id),
            RuntimeEvent::AutomaticCompactionStarted {
                run_id,
                initial_overflow,
            } => self.start_automatic_compaction(run_id, initial_overflow),
            RuntimeEvent::GenerationRestarted {
                run_id, step_id, ..
            } => self.restart_generation(run_id, step_id),
            RuntimeEvent::CompactionAcknowledged { acknowledgement } => match acknowledgement {
                crate::runtime::CompactionAcknowledgement::Accepted
                    if self.disabled_reason.is_none()
                        && self.compaction_state == CompactionState::Idle =>
                {
                    self.compaction_state = CompactionState::Pending;
                    true
                }
                crate::runtime::CompactionAcknowledgement::Disabled { reason } => {
                    self.disable(reason, true)
                }
                crate::runtime::CompactionAcknowledgement::Busy
                | crate::runtime::CompactionAcknowledgement::Accepted => false,
            },
            RuntimeEvent::CompactionLifecycle { lifecycle } => match lifecycle {
                crate::runtime::CompactionLifecycle::Started
                    if self.compaction_state == CompactionState::Pending =>
                {
                    self.compaction_state = CompactionState::Active;
                    self.set_status("compacting context".to_owned())
                }
                crate::runtime::CompactionLifecycle::NoOp
                | crate::runtime::CompactionLifecycle::Settled(_) => self.settle_compaction(),
                crate::runtime::CompactionLifecycle::Started => false,
            },
            RuntimeEvent::SettingsAccepted { .. }
            | RuntimeEvent::NameAccepted { .. }
            | RuntimeEvent::NavigationCompleted { .. }
            | RuntimeEvent::CorrectionCompleted { .. }
            | RuntimeEvent::TreeCatalogue { .. } => false,
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(active_run_id),
            } if self.disabled_reason.is_none()
                && (self.run_state == RunState::Submitting
                    || self.run_state == RunState::Active(active_run_id)) =>
            {
                self.latest_run_id = Some(
                    self.latest_run_id
                        .map_or(active_run_id, |latest| latest.max(active_run_id)),
                );
                self.run_state = RunState::Active(active_run_id);
                self.set_status("busy".to_owned())
            }
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: None,
            } if self.disabled_reason.is_none() && self.run_state == RunState::Submitting => {
                self.run_state = RunState::Idle;
                self.compaction_state != CompactionState::Idle || self.set_status("idle".to_owned())
            }
            RuntimeEvent::RuntimeDisabled { reason } => self.disable(reason, false),
            RuntimeEvent::SubmissionRejectedDisabled { reason } => self.disable(reason, true),
            RuntimeEvent::SkillActivationRejected { .. }
                if self.disabled_reason.is_none() && self.run_state == RunState::Submitting =>
            {
                self.run_state = RunState::Idle;
                self.compaction_state != CompactionState::Idle || self.set_status("idle".to_owned())
            }
            RuntimeEvent::SkillActivationRejected { .. } => false,
            RuntimeEvent::SubmissionRejectedBusy { .. } => false,
        }
    }

    fn replace_subagent_record(&mut self, record: TaskRecord) -> bool {
        let id = record.id().clone();
        let Some(current) = self.subagent_records.get(&id) else {
            let accounting_changed = !record.attempts().is_empty();
            self.subagent_records.insert(id, record);
            return accounting_changed;
        };
        if current.revision() >= record.revision() {
            return false;
        }
        let accounting_changed = current.attempts() != record.attempts();
        self.subagent_records.insert(id, record);
        accounting_changed
    }

    fn append_subagent_card_with_anchor(
        &mut self,
        record: &TaskRecord,
        defer_until_anchor: bool,
    ) -> bool {
        if !self.finished_subagent_cards.insert(record.id().clone()) {
            return false;
        }
        let block = finished_subagent_block(record);
        let pending_anchor = record
            .terminal_history_step_id()
            .filter(|_| defer_until_anchor)
            .map(ModelStepId)
            .filter(|step_id| {
                !self
                    .completed_steps
                    .iter()
                    .any(|(_, completed_step_id)| completed_step_id == step_id)
            })
            .map(|step_id| {
                let run_id = self.live_step.as_ref().and_then(|live| {
                    (live.phase == LivePhase::Executing && live.step_id == step_id)
                        .then_some(live.run_id)
                });
                (run_id, step_id)
            });
        if let Some((run_id, step_id)) = pending_anchor {
            self.pending_subagent_blocks.push(PendingSubagentBlock {
                run_id,
                step_id,
                block,
            });
            if self.disabled_reason.is_some() {
                self.show_pending_subagent_blocks = true;
            }
        } else {
            self.append_settled_blocks([block]);
        }
        true
    }

    fn associate_pending_subagent_blocks(&mut self, run_id: RunId, step_id: ModelStepId) {
        for pending in &mut self.pending_subagent_blocks {
            if pending.run_id.is_none() && pending.step_id == step_id {
                pending.run_id = Some(run_id);
            }
        }
    }

    fn flush_pending_subagent_blocks(
        &mut self,
        run_id: RunId,
        step_id: Option<ModelStepId>,
    ) -> bool {
        let mut blocks = Vec::new();
        let mut remaining = Vec::new();
        for pending in std::mem::take(&mut self.pending_subagent_blocks) {
            if pending
                .run_id
                .is_none_or(|pending_run_id| pending_run_id == run_id)
                && step_id.is_none_or(|step_id| pending.step_id == step_id)
            {
                blocks.push(pending.block);
            } else {
                remaining.push(pending);
            }
        }
        self.pending_subagent_blocks = remaining;
        if self.pending_subagent_blocks.is_empty() {
            self.show_pending_subagent_blocks = false;
        }
        if blocks.is_empty() {
            return false;
        }
        self.append_settled_blocks(blocks);
        true
    }

    pub(crate) fn toggle_reasoning(&mut self) -> bool {
        self.show_reasoning = !self.show_reasoning;
        let visible = self
            .settled_blocks
            .iter()
            .any(SettledBlock::invalidates_for_reasoning)
            || self
                .live_step
                .as_ref()
                .is_some_and(|live| !live.reasoning.is_empty());
        if visible {
            for block in &mut self.settled_blocks {
                if block.invalidates_for_reasoning() {
                    block.raster = None;
                }
            }
        }
        visible
    }

    pub(crate) fn toggle_arena_expansion(&mut self) -> bool {
        self.expand_arenas = !self.expand_arenas;
        let visible = self
            .settled_blocks
            .iter()
            .any(SettledBlock::invalidates_for_expansion)
            || self.live_step.as_ref().is_some_and(|live| {
                live.phase == LivePhase::Executing
                    || !live.decoded_python.is_empty()
                    || !live.stdout.is_empty()
                    || live.stdout_truncated
            })
            || self.show_pending_subagent_blocks && !self.pending_subagent_blocks.is_empty();
        if visible {
            for block in &mut self.settled_blocks {
                if block.invalidates_for_expansion() {
                    block.raster = None;
                }
            }
        }
        visible
    }

    pub(crate) fn layout(&mut self, theme: &'static Theme, width: u16) -> TranscriptLayout {
        self.layout_with_cache(theme, width, true, None)
    }

    pub(crate) fn layout_with_footer(
        &mut self,
        theme: &'static Theme,
        width: u16,
        context: FooterContext,
    ) -> TranscriptLayout {
        self.layout_with_cache(theme, width, true, Some(context))
    }

    #[cfg(test)]
    pub(crate) fn layout_fresh(&mut self, theme: &'static Theme, width: u16) -> TranscriptLayout {
        self.layout_with_cache(theme, width, false, None)
    }

    fn layout_with_cache(
        &mut self,
        theme: &'static Theme,
        width: u16,
        use_cache: bool,
        footer_context: Option<FooterContext>,
    ) -> TranscriptLayout {
        let show_reasoning = self.show_reasoning;
        let expand_arenas = self.expand_arenas;
        let footer = self.footer(theme, footer_context);
        let live_sources = self
            .live_step
            .as_ref()
            .map(live_blocks)
            .unwrap_or_default()
            .into_iter()
            .map(TranscriptBlock::normalise)
            .collect::<Vec<_>>();
        let pending_sources = if self.show_pending_subagent_blocks {
            self.pending_subagent_blocks
                .iter()
                .map(|pending| pending.block.clone().normalise())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let highlighter = &mut self.highlighter;
        #[cfg(test)]
        let mut construction_counts = use_cache.then_some(&mut self.construction_counts);
        let mut blocks = Vec::with_capacity(
            self.settled_blocks.len() + live_sources.len() + pending_sources.len(),
        );
        for settled in &mut self.settled_blocks {
            if use_cache
                && let Some(raster) = settled.raster.as_ref().filter(|raster| {
                    raster.buffer.area.width == width && std::ptr::eq(raster.theme, theme)
                })
            {
                blocks.push(LayoutBlock::Cached(Arc::clone(raster)));
                continue;
            }
            if use_cache {
                settled.raster = None;
            }
            let render_block = build_render_block(
                theme,
                &settled.source,
                show_reasoning,
                expand_arenas,
                width,
                highlighter,
                #[cfg(test)]
                construction_counts.as_deref_mut(),
            );
            let block = if use_cache {
                match rasterise(
                    theme,
                    render_block,
                    width,
                    #[cfg(test)]
                    construction_counts
                        .as_deref_mut()
                        .expect("cached layout records construction"),
                ) {
                    Ok(raster) => {
                        settled.raster = Some(Arc::clone(&raster));
                        LayoutBlock::Cached(raster)
                    }
                    Err(render_block) => LayoutBlock::Fresh(render_block),
                }
            } else {
                LayoutBlock::Fresh(render_block)
            };
            blocks.push(block);
        }
        for source in live_sources.iter().chain(&pending_sources) {
            blocks.push(LayoutBlock::Fresh(build_render_block(
                theme,
                source,
                show_reasoning,
                expand_arenas,
                width,
                highlighter,
                #[cfg(test)]
                None,
            )));
        }
        let total_height = stacked_height(blocks.iter().map(LayoutBlock::height));
        TranscriptLayout {
            page_style: page_style(theme),
            blocks,
            width,
            total_height,
            footer,
        }
    }

    #[cfg(test)]
    pub(crate) fn construction_counts(&self) -> ConstructionCounts {
        self.construction_counts
    }

    #[cfg(test)]
    fn blocks(&self) -> Vec<TranscriptBlock> {
        self.projection().blocks
    }

    #[cfg(test)]
    fn projection(&self) -> DisplayProjection {
        let mut blocks = self
            .startup
            .as_ref()
            .map(startup_block)
            .into_iter()
            .collect::<Vec<_>>();
        let mut previous_selection = None;
        for displayed in &self.records {
            blocks.extend(record_blocks(displayed, previous_selection));
            if let Record::UserRequest { selection, .. } = &displayed.record.record {
                previous_selection = *selection;
            }
        }
        if let Some(live) = &self.live_step {
            blocks.extend(live_blocks(live));
        }
        if self.restoration_card {
            blocks.push(TranscriptBlock::SessionRestored);
        }
        if self.show_pending_subagent_blocks {
            blocks.extend(
                self.pending_subagent_blocks
                    .iter()
                    .map(|pending| pending.block.clone()),
            );
        }
        DisplayProjection { blocks }
    }

    fn footer(&self, theme: &Theme, context: Option<FooterContext>) -> Option<Footer> {
        let mut foreground_attempts = Vec::new();
        let mut normal_foreground_attempts = Vec::new();
        for displayed in &self.records {
            let ordinary = record_attempts(&displayed.record.record);
            foreground_attempts.extend(ordinary);
            append_initial_overflow_attempts(&displayed.record.record, &mut foreground_attempts);
            if !matches!(displayed.record.record, Record::Compaction { .. }) {
                normal_foreground_attempts.extend(ordinary);
            }
        }
        if let Some(live) = &self.live_step {
            foreground_attempts.extend(&live.settled_attempts);
            normal_foreground_attempts.extend(&live.settled_attempts);
        }
        if let Some((_, _, attempts)) = &self.initial_overflow {
            foreground_attempts.extend(attempts);
        }
        let latest_foreground_attempt = normal_foreground_attempts.last().copied();
        let mut attempts = foreground_attempts;
        attempts.extend(
            self.subagent_records
                .values()
                .flat_map(|record| record.attempts()),
        );
        attempts.extend(&self.footer_adjustment);
        session_footer(theme, &attempts, latest_foreground_attempt, context)
    }

    fn append_settled_blocks(&mut self, blocks: impl IntoIterator<Item = TranscriptBlock>) {
        for block in blocks {
            #[cfg(test)]
            {
                self.construction_counts.normalisations += 1;
            }
            self.settled_blocks
                .push(SettledBlock::new(block.normalise()));
        }
    }

    fn disable(&mut self, reason: RuntimeDisabledReason, clear_submitting: bool) -> bool {
        let reason_changed = self.disabled_reason.as_ref() != Some(&reason);
        self.disabled_reason = Some(reason.clone());
        let submitting_cleared = clear_submitting && self.run_state == RunState::Submitting;
        if submitting_cleared {
            self.run_state = RunState::Idle;
        }
        let automatic_cleared = self.automatic_compaction.take().is_some();
        let compaction_cleared =
            automatic_cleared || self.compaction_state != CompactionState::Idle;
        self.compaction_state = CompactionState::Idle;
        let cards_released = self.release_pending_subagent_blocks();
        reason_changed
            | submitting_cleared
            | compaction_cleared
            | cards_released
            | self.set_status(disabled_status(&reason).to_owned())
    }

    fn release_pending_subagent_blocks(&mut self) -> bool {
        if self.pending_subagent_blocks.is_empty() || self.show_pending_subagent_blocks {
            return false;
        }
        self.show_pending_subagent_blocks = true;
        true
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
        self.initial_overflow = None;
        self.run_state = RunState::Active(run_id);
        self.set_status("starting run".to_owned())
    }

    fn start_automatic_compaction(
        &mut self,
        run_id: RunId,
        initial_overflow: Option<(ModelStepId, Vec<AttemptAccounting>)>,
    ) -> bool {
        if self.disabled_reason.is_some()
            || self.is_terminal_run(run_id)
            || self
                .latest_run_id
                .is_some_and(|latest_run_id| run_id < latest_run_id)
            || matches!(self.run_state, RunState::Active(active_run_id) if active_run_id != run_id)
        {
            return false;
        }
        self.latest_run_id = Some(run_id);
        self.run_state = RunState::Active(run_id);
        let mut changed = self.automatic_compaction.replace(run_id).is_none();
        if let Some((step_id, attempts)) = initial_overflow {
            if let Some(live) = self
                .live_step
                .as_mut()
                .filter(|live| live.run_id == run_id && live.step_id == step_id)
            {
                live.settled_attempts.clear();
            }
            self.initial_overflow = Some((run_id, step_id, attempts));
            changed = true;
        }
        changed | self.set_status("compacting context".to_owned())
    }

    fn is_stale_generation(&self, run_id: RunId, step_id: ModelStepId, epoch: u32) -> bool {
        epoch == 0
            && self
                .initial_overflow
                .as_ref()
                .is_some_and(|(overflow_run, overflow_step, _)| {
                    *overflow_run == run_id && *overflow_step == step_id
                })
    }

    fn restart_generation(&mut self, run_id: RunId, step_id: ModelStepId) -> bool {
        let Some(live) = self
            .live_step
            .as_mut()
            .filter(|live| live.run_id == run_id && live.step_id == step_id)
        else {
            return false;
        };
        let restarted = LiveStep::new(run_id, step_id, live.progress_gap);
        let changed = *live != restarted;
        *live = restarted;
        changed
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
                self.set_status("starting run".to_owned())
            }
        }
    }

    fn append_record(&mut self, record: StoredRecord, progress_gap: bool) -> bool {
        if matches!(record.record, Record::UserRequest { .. }) && self.restoration_card {
            self.restoration_card = false;
            self.settled_blocks
                .retain(|block| !matches!(block.source, NormalisedBlock::SessionRestored));
        }
        let run_id = record.run_id;
        let settled_step = match &record.record {
            Record::CompletedExecution { step_id, .. } => Some(*step_id),
            Record::UserRequest { .. }
            | Record::FinalMessage { .. }
            | Record::CancelledRun { .. }
            | Record::FailedRun { .. }
            | Record::Compaction { .. }
            | Record::SkillActivation { .. } => None,
        };
        let settles_run = matches!(
            &record.record,
            Record::FinalMessage { .. } | Record::CancelledRun { .. } | Record::FailedRun { .. }
        );
        let terminal_step = settles_run.then(|| {
            self.live_step.as_ref().and_then(|live| {
                (live.run_id == record.run_id && live.phase == LivePhase::Executing)
                    .then_some(live.step_id)
            })
        });
        let terminal_step = terminal_step.flatten();
        self.latest_run_id = Some(
            self.latest_run_id
                .map_or(record.run_id, |latest| latest.max(record.run_id)),
        );
        let run_previews = match (&record.record, &self.live_step) {
            (Record::CompletedExecution { step_id, .. }, Some(live))
                if live.run_id == record.run_id && live.step_id == *step_id =>
            {
                live.run_previews.clone()
            }
            _ => Vec::new(),
        };
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
                if let Some(step_id) = terminal_step {
                    self.completed_steps.push((record.run_id, step_id));
                }
                self.mark_terminal_run(record.run_id);
            }
            Record::Compaction { .. } if self.automatic_compaction == Some(record.run_id) => {
                self.automatic_compaction = None;
                self.set_status("starting run".to_owned());
            }
            Record::UserRequest { .. }
            | Record::Compaction { .. }
            | Record::SkillActivation { .. } => {}
        }
        let displayed = DisplayedRecord {
            record,
            run_previews,
            progress_gap,
        };
        let previous_selection = matches!(displayed.record.record, Record::UserRequest { .. })
            .then(|| self.last_request_selection())
            .flatten();
        let blocks = record_blocks(&displayed, previous_selection);
        self.records.push(displayed);
        self.append_settled_blocks(blocks);
        if let Some(step_id) = settled_step {
            self.flush_pending_subagent_blocks(run_id, Some(step_id));
        } else if settles_run {
            self.flush_pending_subagent_blocks(run_id, None);
        }
        true
    }

    pub(crate) fn pending_model_change(&self, current: ModelSelection) -> Option<ModelSelection> {
        self.last_request_selection()
            .is_some_and(|last| last != current)
            .then_some(current)
    }

    fn last_request_selection(&self) -> Option<ModelSelection> {
        self.records
            .iter()
            .rev()
            .find_map(|displayed| match &displayed.record.record {
                Record::UserRequest { selection, .. } => Some(*selection),
                _ => None,
            })
            .flatten()
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
        if let Some(step_id) = self.live_step.as_ref().and_then(|live| {
            (live.run_id == run_id && live.phase == LivePhase::Executing).then_some(live.step_id)
        }) {
            self.completed_steps.push((run_id, step_id));
        }
        self.mark_terminal_run(run_id);
        let state_changed = self.run_state != RunState::Idle;
        self.run_state = RunState::Idle;
        let cards_flushed = self.flush_pending_subagent_blocks(run_id, None);
        state_changed
            | cards_flushed
            | (self.disabled_reason.is_none()
                && self.compaction_state == CompactionState::Idle
                && self.set_status("idle".to_owned()))
    }

    fn settle_compaction(&mut self) -> bool {
        if self.compaction_state == CompactionState::Idle {
            return false;
        }
        self.compaction_state = CompactionState::Idle;
        let mut changed = true;
        changed |= self.disabled_reason.is_none()
            && self.run_state == RunState::Idle
            && self.set_status("idle".to_owned());
        changed
    }

    fn mark_terminal_run(&mut self, run_id: RunId) {
        if !self.is_terminal_run(run_id) {
            self.settled_run_ids.push(run_id);
        }
        if self.automatic_compaction == Some(run_id) {
            self.automatic_compaction = None;
        }
        if self
            .initial_overflow
            .as_ref()
            .is_some_and(|(overflow_run, ..)| *overflow_run == run_id)
        {
            self.initial_overflow = None;
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

pub(crate) struct ModelChangeCard(RenderBlock);

impl ModelChangeCard {
    pub(crate) fn height(&self) -> u16 {
        u16::try_from(self.0.height()).unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&self, area: Rect, buffer: &mut Buffer) {
        self.0.render(area, 0, 0, buffer);
    }
}

pub(crate) fn model_change_card(
    theme: &Theme,
    selection: ModelSelection,
    pending: bool,
    width: u16,
) -> ModelChangeCard {
    let prefix = if pending { "[pending] " } else { "" };
    ModelChangeCard(model_change_block(
        theme,
        &format!("{prefix}{}", model_change_text(selection)),
        width,
    ))
}

fn model_change_text(selection: ModelSelection) -> String {
    format!(
        "Model selected: {} · {}",
        selection.model().id,
        selection.thinking()
    )
}

fn model_change_block(theme: &Theme, text: &str, width: u16) -> RenderBlock {
    RenderBlock::card(
        own_styled_text(Text::raw(text), muted_page_style(theme)),
        muted_page_style(theme),
        width,
        1,
        1,
    )
}

fn build_render_block(
    theme: &'static Theme,
    source: &NormalisedBlock,
    show_reasoning: bool,
    expand_arenas: bool,
    width: u16,
    highlighter: &mut PythonHighlighter,
    #[cfg(test)] mut construction_counts: Option<&mut ConstructionCounts>,
) -> RenderBlock {
    #[cfg(test)]
    if let Some(construction_counts) = construction_counts.as_deref_mut() {
        construction_counts.render_block_builds += 1;
    }

    match source {
        NormalisedBlock::Startup { lines } | NormalisedBlock::Metadata { lines } => {
            RenderBlock::card(
                own_styled_text(Text::raw(lines.join("\n")), page_style(theme)),
                page_style(theme),
                width,
                0,
                0,
            )
        }
        NormalisedBlock::ModelChange { text } => model_change_block(theme, text, width),
        NormalisedBlock::User { request } => {
            let style = card_style(theme, theme.user_message_bg);
            RenderBlock::card(
                own_styled_text(Text::raw(request.as_str()), style),
                style,
                width,
                1,
                1,
            )
        }
        NormalisedBlock::SubagentFinished(card) => {
            let style = card_style(theme, subagent_background(theme, card.state));
            RenderBlock::card(
                subagent_finished_text(theme, card, width, expand_arenas),
                style,
                width,
                1,
                1,
            )
        }
        NormalisedBlock::Compaction(card) => {
            let background = card.state.background(theme);
            let style = card_style(theme, background);
            let mut text = if expand_arenas {
                match &card.expanded {
                    CompactionContent::Markdown(source) => markdown(
                        source,
                        theme,
                        #[cfg(test)]
                        construction_counts,
                    ),
                    CompactionContent::Plain(source) => literal_text(source, style),
                }
            } else {
                Text::raw(card.collapsed.clone())
            };
            text.lines.insert(0, Line::raw(card.heading.clone()));
            if expand_arenas && let Some(accounting) = &card.accounting {
                text.lines.push(accounting_line(
                    accounting.clone(),
                    muted_card_style(theme, background),
                ));
            }
            RenderBlock::card(own_styled_text(text, style), style, width, 1, 1)
        }
        NormalisedBlock::SkillActivation(card) => {
            let style = card_style(theme, theme.tool_success_bg);
            let mut lines = vec![Line::raw(if expand_arenas {
                card.heading.clone()
            } else {
                format!("{} \u{b7} Ctrl-O to expand", card.heading)
            })];
            if expand_arenas {
                lines.extend(
                    logical_lines(&card.source_text)
                        .into_iter()
                        .map(|line| Line::raw(line.to_owned())),
                );
            }
            let text = own_styled_text(Text::from(lines), style);
            RenderBlock::card(text, style, width, 1, 1)
        }
        NormalisedBlock::Reasoning { markdown: source } => {
            let style = card_style(theme, theme.card_bg)
                .fg(theme.thinking_text)
                .add_modifier(Modifier::ITALIC);
            let text = if show_reasoning {
                own_styled_text(
                    markdown(
                        source,
                        theme,
                        #[cfg(test)]
                        construction_counts,
                    ),
                    style,
                )
            } else {
                own_styled_text(
                    Text::raw("thinking hidden · Ctrl-T to show"),
                    dim_card_style(theme, theme.card_bg).add_modifier(Modifier::ITALIC),
                )
            };
            RenderBlock::card(text, style, width, 2, 1)
        }
        NormalisedBlock::Arena(arena) => RenderBlock::Arena(ArenaRenderBlock::new(
            theme,
            arena.clone(),
            expand_arenas,
            highlighter,
            width,
            #[cfg(test)]
            construction_counts,
        )),
        NormalisedBlock::Assistant {
            markdown: source,
            accounting,
        } => RenderBlock::card(
            assistant_text(
                theme,
                source,
                accounting.clone(),
                #[cfg(test)]
                construction_counts,
            ),
            page_style(theme),
            width,
            0,
            0,
        ),
        NormalisedBlock::Status { text } => RenderBlock::card(
            own_styled_text(Text::raw(text.as_str()), page_style(theme)),
            page_style(theme),
            width,
            0,
            0,
        ),
        NormalisedBlock::SessionRestored => {
            let style = card_style(theme, theme.tool_success_bg);
            RenderBlock::card(
                own_styled_text(Text::raw("Session restored."), style),
                style,
                width,
                1,
                1,
            )
        }
        NormalisedBlock::Accounting { text } => RenderBlock::card(
            Text {
                alignment: None,
                style: page_style(theme),
                lines: vec![accounting_line(text.clone(), muted_page_style(theme))],
            },
            page_style(theme),
            width,
            0,
            0,
        ),
    }
}

fn rasterise(
    theme: &'static Theme,
    block: RenderBlock,
    width: u16,
    #[cfg(test)] construction_counts: &mut ConstructionCounts,
) -> Result<Arc<SettledRaster>, RenderBlock> {
    let Ok(height) = u16::try_from(block.height()) else {
        return Err(block);
    };
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    block.render(area, 0, 0, &mut buffer);
    #[cfg(test)]
    {
        construction_counts.rasterisations += 1;
        construction_counts.selection_region_builds += 1;
    }
    let regions = block.selectable_regions(area, 0, 0, CardId::new(0), &buffer);
    Ok(Arc::new(SettledRaster {
        theme,
        buffer,
        regions,
    }))
}

fn startup_block(snapshot: &StartupSnapshot) -> TranscriptBlock {
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
    let api_key_line = |provider: Provider| {
        let presence = match snapshot.api_key_presence(provider) {
            ApiKeyPresence::Missing => "missing",
            ApiKeyPresence::Present => "present",
        };
        format!("{} API key: {presence}", provider.display_name())
    };
    let submission = match snapshot.submission_availability() {
        SubmissionAvailability::Available => "available",
        SubmissionAvailability::Unavailable => "unavailable",
    };
    let mut lines = vec![
        "Startup configuration".to_owned(),
        format!("Worker: {worker_path}"),
        format!("Workspace: {workspace_root}"),
        format!(
            "Model: {} (context: {}, max output: {})",
            snapshot.model().id,
            snapshot.model().context_window_display,
            snapshot.model().max_output_display
        ),
        format!("Thinking: {}", snapshot.thinking()),
        format!("Prompt: {prompt_source}"),
        api_key_line(Provider::OpenAi),
        api_key_line(Provider::Anthropic),
        api_key_line(Provider::Google),
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
    for warning in snapshot.warnings() {
        lines.push(format!("Warning: {}", warning.diagnostic()));
        lines.push(format!("Remediation: {}", warning.remediation()));
    }
    TranscriptBlock::Startup { lines }
}

#[cfg(test)]
pub(crate) fn format_startup_configuration(snapshot: &StartupSnapshot) -> String {
    let NormalisedBlock::Startup { lines } = startup_block(snapshot).normalise() else {
        unreachable!("startup block")
    };
    lines.join("\n")
}

fn record_attempts(record: &Record) -> &[AttemptAccounting] {
    match record {
        Record::CompletedExecution { attempts, .. } => attempts,
        Record::FinalMessage { terminal, .. }
        | Record::CancelledRun { terminal }
        | Record::FailedRun { terminal, .. } => &terminal.attempts,
        Record::Compaction { outcome } => outcome.attempts(),
        Record::UserRequest { .. } | Record::SkillActivation { .. } => &[],
    }
}

fn append_initial_overflow_attempts<'a>(
    record: &'a Record,
    attempts: &mut Vec<&'a AttemptAccounting>,
) {
    if let Record::FinalMessage { terminal, .. }
    | Record::CancelledRun { terminal }
    | Record::FailedRun { terminal, .. } = record
        && let Some(recovery) = &terminal.overflow_recovery
    {
        attempts.extend(&recovery.attempts);
    }
}

fn record_blocks(
    displayed: &DisplayedRecord,
    previous_selection: Option<ModelSelection>,
) -> Vec<TranscriptBlock> {
    match &displayed.record.record {
        Record::UserRequest { request, selection } => {
            let mut blocks = Vec::new();
            if let (Some(previous), Some(selection)) = (previous_selection, *selection)
                && previous != selection
            {
                blocks.push(TranscriptBlock::ModelChange { selection });
            }
            blocks.push(TranscriptBlock::User {
                request: request.clone(),
            });
            append_gap_metadata(&mut blocks, displayed.progress_gap);
            blocks
        }
        Record::CompletedExecution {
            source,
            commentary,
            reasoning_summary,
            execution,
            attempts,
            ..
        } => {
            let mut blocks = Vec::new();
            append_reasoning(&mut blocks, reasoning_summary, "");
            append_commentary(&mut blocks, commentary.as_deref());
            blocks.push(TranscriptBlock::Arena(ArenaBlock {
                state: arena_state(execution),
                heading: None,
                status: None,
                source: Some(source.clone()),
                tool_calls: execution.tool_calls.clone(),
                run_previews: displayed.run_previews.clone(),
                content: execution_content(execution),
                progress_gap: displayed.progress_gap,
                accounting: step_accounting(attempts),
            }));
            blocks
        }
        Record::FinalMessage {
            text,
            reasoning_summary,
            terminal,
            ..
        } => {
            let mut blocks = Vec::new();
            append_overflow_recovery(&mut blocks, terminal.overflow_recovery.as_ref());
            append_reasoning(&mut blocks, reasoning_summary, "");
            terminal_evidence_blocks(&mut blocks, terminal.evidence.as_ref());
            blocks.push(TranscriptBlock::Assistant {
                markdown: text.clone(),
                accounting: step_accounting(&terminal.attempts),
            });
            append_gap_metadata(&mut blocks, displayed.progress_gap);
            blocks
        }
        Record::CancelledRun { terminal } => {
            let mut blocks = Vec::new();
            append_overflow_recovery(&mut blocks, terminal.overflow_recovery.as_ref());
            blocks.extend(terminal_blocks(
                "cancelled".to_owned(),
                terminal,
                displayed.progress_gap,
            ));
            blocks
        }
        Record::FailedRun { message, terminal } => {
            let mut blocks = Vec::new();
            append_overflow_recovery(&mut blocks, terminal.overflow_recovery.as_ref());
            blocks.extend(terminal_blocks(
                format!("failed: {message}"),
                terminal,
                displayed.progress_gap,
            ));
            blocks
        }
        Record::Compaction { outcome } => {
            vec![TranscriptBlock::Compaction(compaction_card(outcome))]
        }
        Record::SkillActivation { activation } => {
            let origin = match activation.origin() {
                SkillOrigin::Project => "project",
                SkillOrigin::User => "user",
            };
            vec![TranscriptBlock::SkillActivation(SkillActivationCard {
                heading: format!("Skill · {} · {origin}", activation.name()),
                source_text: activation.source_text().to_owned(),
            })]
        }
    }
}

fn compaction_card(outcome: &CompactionOutcome) -> CompactionCard {
    let heading = format!("compaction · {}", compaction_reason_label(outcome.reason()));
    let accounting = step_accounting(outcome.attempts());
    match outcome {
        CompactionOutcome::Applied {
            summary,
            context_tokens,
            ..
        } => CompactionCard {
            state: ArenaState::Complete,
            heading,
            collapsed: format!(
                "Compacted {} tokens · Ctrl-O to expand",
                format_token_count(u128::from(*context_tokens))
            ),
            expanded: CompactionContent::Markdown(summary.clone()),
            accounting,
        },
        CompactionOutcome::Failed { diagnostic, .. } => CompactionCard {
            state: ArenaState::Error,
            heading,
            collapsed: "Failed · Ctrl-O to expand".to_owned(),
            expanded: CompactionContent::Plain(diagnostic.clone()),
            accounting,
        },
        CompactionOutcome::Cancelled { .. } => CompactionCard {
            state: ArenaState::Error,
            heading,
            collapsed: "Cancelled · Ctrl-O to expand".to_owned(),
            expanded: CompactionContent::Plain("Cancelled".to_owned()),
            accounting,
        },
    }
}

pub(crate) fn compaction_reason_label(reason: CompactionReason) -> &'static str {
    match reason {
        CompactionReason::Manual => "manual",
        CompactionReason::AutomaticThreshold => "automatic threshold",
        CompactionReason::ContextOverflow => "context overflow",
    }
}

fn live_blocks(live: &LiveStep) -> Vec<TranscriptBlock> {
    let has_arena_content = live.phase == LivePhase::Executing
        || !live.decoded_python.is_empty()
        || !live.stdout.is_empty()
        || live.stdout_truncated;
    let accounting = step_accounting(&live.settled_attempts);

    let mut blocks = Vec::new();
    append_reasoning(&mut blocks, "", &live.reasoning);
    if has_arena_content {
        if !live.assistant_text.is_empty() {
            blocks.push(TranscriptBlock::Assistant {
                markdown: live.assistant_text.clone(),
                accounting: None,
            });
        }
        let mut content = Vec::new();
        if !live.stdout.is_empty() {
            content.push(ArenaContent {
                label: "stdout",
                text: live.stdout.clone(),
            });
        }
        if live.stdout_truncated {
            content.push(ArenaContent {
                label: "stdout preview",
                text: "truncated".to_owned(),
            });
        }
        blocks.push(TranscriptBlock::Arena(ArenaBlock {
            state: ArenaState::InProgress,
            heading: None,
            status: None,
            source: (!live.decoded_python.is_empty()).then(|| live.decoded_python.clone()),
            tool_calls: live.tool_calls.clone(),
            run_previews: live.run_previews.clone(),
            content,
            progress_gap: live.progress_gap,
            accounting,
        }));
        return blocks;
    }

    append_gap_metadata(&mut blocks, live.progress_gap);
    if !live.assistant_text.is_empty() {
        blocks.push(TranscriptBlock::Assistant {
            markdown: live.assistant_text.clone(),
            accounting,
        });
    } else {
        append_accounting(&mut blocks, accounting);
    }
    blocks
}

fn terminal_blocks(
    diagnostic: String,
    terminal: &TerminalDetails,
    progress_gap: bool,
) -> Vec<TranscriptBlock> {
    let accounting = step_accounting(&terminal.attempts);
    match terminal.evidence.as_ref() {
        Some(TerminalEvidence::Generation(evidence)) if !evidence.decoded_python.is_empty() => {
            let mut blocks = Vec::new();
            append_reasoning(
                &mut blocks,
                &evidence.reasoning_summary,
                &evidence.reasoning,
            );
            let arena = TranscriptBlock::Arena(ArenaBlock {
                state: ArenaState::Error,
                heading: Some(diagnostic),
                status: None,
                source: Some(evidence.decoded_python.clone()),
                tool_calls: Vec::new(),
                run_previews: Vec::new(),
                content: Vec::new(),
                progress_gap,
                accounting: None,
            });
            if evidence.assistant_text.is_empty() {
                blocks.push(arena);
                append_accounting(&mut blocks, accounting);
            } else {
                blocks.push(TranscriptBlock::Assistant {
                    markdown: evidence.assistant_text.clone(),
                    accounting,
                });
                blocks.push(arena);
            }
            blocks
        }
        Some(TerminalEvidence::Generation(evidence)) => {
            let mut blocks = vec![TranscriptBlock::Status { text: diagnostic }];
            append_reasoning(
                &mut blocks,
                &evidence.reasoning_summary,
                &evidence.reasoning,
            );
            if evidence.assistant_text.is_empty() {
                append_gap_metadata(&mut blocks, progress_gap);
                append_accounting(&mut blocks, accounting);
            } else {
                blocks.push(TranscriptBlock::Assistant {
                    markdown: evidence.assistant_text.clone(),
                    accounting,
                });
                append_gap_metadata(&mut blocks, progress_gap);
            }
            blocks
        }
        Some(TerminalEvidence::Execution(evidence)) => {
            let mut blocks = Vec::new();
            append_reasoning(&mut blocks, &evidence.reasoning_summary, "");
            append_commentary(&mut blocks, evidence.commentary.as_deref());
            blocks.push(TranscriptBlock::Arena(execution_evidence_block(
                Some(diagnostic),
                evidence,
                progress_gap,
                accounting,
            )));
            blocks
        }
        None => {
            let mut blocks = vec![TranscriptBlock::Status { text: diagnostic }];
            append_gap_metadata(&mut blocks, progress_gap);
            append_accounting(&mut blocks, accounting);
            blocks
        }
    }
}

fn append_commentary(blocks: &mut Vec<TranscriptBlock>, commentary: Option<&str>) {
    if let Some(commentary) = commentary {
        blocks.push(TranscriptBlock::Assistant {
            markdown: commentary.to_owned(),
            accounting: None,
        });
    }
}

fn append_reasoning(blocks: &mut Vec<TranscriptBlock>, summary: &str, streamed: &str) {
    if !summary.is_empty() {
        blocks.push(TranscriptBlock::Reasoning {
            markdown: summary.to_owned(),
        });
    }
    if !streamed.is_empty() {
        blocks.push(TranscriptBlock::Reasoning {
            markdown: streamed.to_owned(),
        });
    }
}

fn append_gap_metadata(blocks: &mut Vec<TranscriptBlock>, progress_gap: bool) {
    if progress_gap {
        blocks.push(TranscriptBlock::Metadata {
            lines: vec!["display gap: some live progress was dropped".to_owned()],
        });
    }
}

fn append_accounting(blocks: &mut Vec<TranscriptBlock>, accounting: Option<String>) {
    if let Some(text) = accounting {
        blocks.push(TranscriptBlock::Accounting { text });
    }
}

fn terminal_evidence_blocks(
    blocks: &mut Vec<TranscriptBlock>,
    evidence: Option<&TerminalEvidence>,
) {
    match evidence {
        Some(TerminalEvidence::Generation(evidence)) => {
            append_generation_evidence(blocks, evidence)
        }
        Some(TerminalEvidence::Execution(evidence)) => {
            append_reasoning(blocks, &evidence.reasoning_summary, "");
            append_commentary(blocks, evidence.commentary.as_deref());
            blocks.push(TranscriptBlock::Arena(execution_evidence_block(
                None, evidence, false, None,
            )));
        }
        None => {}
    }
}

fn append_overflow_recovery(
    blocks: &mut Vec<TranscriptBlock>,
    recovery: Option<&OverflowRecovery>,
) {
    let Some(recovery) = recovery else {
        return;
    };
    blocks.push(TranscriptBlock::Status {
        text: format!("context overflow: {}", recovery.failure),
    });
    if let Some(generation) = &recovery.generation {
        append_generation_evidence(blocks, generation);
    }
    append_accounting(blocks, step_accounting(&recovery.attempts));
}

fn append_generation_evidence(blocks: &mut Vec<TranscriptBlock>, evidence: &GenerationEvidence) {
    append_reasoning(blocks, &evidence.reasoning_summary, &evidence.reasoning);
    if !evidence.assistant_text.is_empty() {
        blocks.push(TranscriptBlock::Assistant {
            markdown: evidence.assistant_text.clone(),
            accounting: None,
        });
    }
    if !evidence.decoded_python.is_empty() {
        blocks.push(TranscriptBlock::Arena(ArenaBlock {
            state: ArenaState::Error,
            heading: None,
            status: None,
            source: Some(evidence.decoded_python.clone()),
            tool_calls: Vec::new(),
            run_previews: Vec::new(),
            content: Vec::new(),
            progress_gap: false,
            accounting: None,
        }));
    }
}

fn arena_state(execution: &Execution) -> ArenaState {
    match execution.outcome {
        ExecutionOutcome::Complete { .. } => ArenaState::Complete,
        ExecutionOutcome::PythonError(..) | ExecutionOutcome::TypeCheckError(..) => {
            ArenaState::Error
        }
    }
}

fn execution_content(execution: &Execution) -> Vec<ArenaContent> {
    let mut content = Vec::new();
    append_observation(&mut content, "stdout", &execution.stdout);
    append_observation(&mut content, "stderr", &execution.stderr);
    match &execution.outcome {
        ExecutionOutcome::Complete {
            result: Some(result),
        } => append_observation(&mut content, "result", result),
        ExecutionOutcome::Complete { result: None } => {}
        ExecutionOutcome::PythonError(error) => content.push(ArenaContent {
            label: "error",
            text: error.text.clone(),
        }),
        ExecutionOutcome::TypeCheckError(error) => content.push(ArenaContent {
            label: "type check",
            text: error.text.clone(),
        }),
    }
    content
}

fn execution_evidence_block(
    heading: Option<String>,
    evidence: &ExecutionEvidence,
    progress_gap: bool,
    accounting: Option<String>,
) -> ArenaBlock {
    let mut content = Vec::new();
    append_observation(&mut content, "stdout", &evidence.stdout);
    append_observation(&mut content, "stderr", &evidence.stderr);
    if let Some(failure) = &evidence.failure {
        content.push(ArenaContent {
            label: "failure",
            text: format_execution_failure(failure),
        });
    }
    if let Some(warning) = &evidence.cancellation_warning {
        content.push(ArenaContent {
            label: "warning",
            text: warning.clone(),
        });
    }
    ArenaBlock {
        state: ArenaState::Error,
        heading,
        status: Some(format!(
            "execution: {}",
            if evidence.began {
                "started"
            } else {
                "not started"
            }
        )),
        source: Some(evidence.source.clone()),
        tool_calls: evidence.tool_calls.clone(),
        run_previews: Vec::new(),
        content,
        progress_gap,
        accounting,
    }
}

#[derive(Default)]
struct SessionAccounting {
    input: Option<u128>,
    output: Option<u128>,
    cache_read: Option<u128>,
    cache_write: Option<u128>,
    reasoning: Option<u128>,
    total: Option<u128>,
}

impl SessionAccounting {
    fn add(&mut self, attempt: &AttemptAccounting) {
        let (AttemptAccounting::KnownUsage(usage)
        | AttemptAccounting::KnownUsageWithUntrackedRetries(usage)) = attempt
        else {
            return;
        };
        add_total(&mut self.input, usage.input_tokens);
        add_total(&mut self.output, usage.output_tokens);
        add_total(&mut self.cache_read, usage.cached_input_tokens);
        add_total(&mut self.cache_write, usage.cache_write_input_tokens);
        add_total(&mut self.reasoning, usage.reasoning_output_tokens);
        add_total(&mut self.total, usage.total_tokens);
    }
}

fn latest_context_field(theme: &Theme, usage: &ModelUsage) -> Option<FooterField> {
    let total = usage.total_tokens?;
    let model = model_by_id(&usage.model).unwrap_or(DEFAULT_MODEL);
    let (capacity, denominator) = if total <= model.pricing.long_context_threshold {
        (
            model.pricing.long_context_threshold,
            model.pricing.long_context_threshold_display,
        )
    } else {
        (model.context_window_tokens, model.context_window_display)
    };
    let tenths = (u128::from(total) * 1_000 + u128::from(capacity) / 2) / u128::from(capacity);
    let style = if tenths >= 900 {
        Style::new().fg(theme.error)
    } else if tenths >= 800 {
        Style::new().fg(theme.warning)
    } else {
        muted_page_style(theme)
    };
    Some(FooterField {
        spans: vec![
            Span::styled(format!("{}.{}%", tenths / 10, tenths % 10), style),
            Span::styled(format!("/{denominator}"), muted_page_style(theme)),
        ],
    })
}

fn latest_cache_hit_field(theme: &Theme, usage: &ModelUsage) -> Option<FooterField> {
    match (usage.input_tokens, usage.cached_input_tokens) {
        (Some(input), Some(cached)) if input > 0 && cached <= input => {
            let tenths = (u128::from(cached) * 1_000 + u128::from(input) / 2) / u128::from(input);
            Some(FooterField::muted(
                theme,
                format!("CH{}.{}%", tenths / 10, tenths % 10),
            ))
        }
        _ => None,
    }
}

fn session_footer(
    theme: &Theme,
    attempts: &[&AttemptAccounting],
    latest_foreground_attempt: Option<&AttemptAccounting>,
    context: Option<FooterContext>,
) -> Option<Footer> {
    let mut accounting = SessionAccounting::default();
    for attempt in attempts {
        accounting.add(attempt);
    }
    let mut statistics = footer_accounting_fields(&accounting)
        .into_iter()
        .map(|text| FooterField::muted(theme, text))
        .collect::<Vec<_>>();
    if let Some(
        AttemptAccounting::KnownUsage(usage)
        | AttemptAccounting::KnownUsageWithUntrackedRetries(usage),
    ) = latest_foreground_attempt
        && let Some(cache_hit) = latest_cache_hit_field(theme, usage)
    {
        statistics.push(cache_hit);
    }
    if let Some(cost) = footer_estimated_cost(attempts.iter().copied()) {
        statistics.push(FooterField::muted(theme, cost));
    }
    if let Some(
        AttemptAccounting::KnownUsage(usage)
        | AttemptAccounting::KnownUsageWithUntrackedRetries(usage),
    ) = latest_foreground_attempt
        && let Some(context) = latest_context_field(theme, usage)
    {
        statistics.push(context);
    }
    let model = context.as_ref().map(|context| ModelFooter {
        id: context.selection.model().id.to_owned(),
        thinking: context.selection.thinking().to_owned(),
    });
    let identity = context.and_then(footer_identity);
    Footer::new(identity, statistics, model)
}

fn footer_identity(context: FooterContext) -> Option<FooterIdentity> {
    let cwd = context.workspace_root.map(|workspace_root| {
        context
            .home
            .as_deref()
            .and_then(|home| {
                Path::new(&workspace_root)
                    .strip_prefix(home)
                    .ok()
                    .map(|relative| {
                        if relative.as_os_str().is_empty() {
                            "~".to_owned()
                        } else {
                            format!("~/{}", relative.display())
                        }
                    })
            })
            .unwrap_or(workspace_root)
    });
    (cwd.is_some() || context.name.is_some()).then_some(FooterIdentity {
        cwd,
        name: context.name,
    })
}

fn footer_accounting_fields(accounting: &SessionAccounting) -> Vec<String> {
    [
        ("↑", accounting.input),
        ("↓", accounting.output),
        ("R", accounting.cache_read),
        ("W", accounting.cache_write),
    ]
    .into_iter()
    .filter_map(|(label, total)| total.map(|total| format!("{label}{}", format_token_count(total))))
    .collect()
}

pub(crate) fn format_token_count(value: u128) -> String {
    const K: u128 = 1_000;
    const M: u128 = 1_000_000;
    if value < K {
        return value.to_string();
    }
    if value >= M {
        return format_token_count_with_suffix(value, M, "m");
    }
    let formatted = format_token_count_with_suffix(value, K, "k");
    if formatted.starts_with("1000k") {
        format_token_count_with_suffix(value, M, "m")
    } else {
        formatted
    }
}

fn format_token_count_with_suffix(value: u128, base: u128, suffix: &str) -> String {
    let whole = value / base;
    let precision = if whole < 10 {
        2
    } else if whole < 100 {
        1
    } else {
        0
    };
    let factor = 10_u128.pow(precision);
    let scaled = whole
        .saturating_mul(factor)
        .saturating_add(((value % base).saturating_mul(factor) + base / 2) / base);
    let integer = scaled / factor;
    let fraction = scaled % factor;
    if precision == 0 || fraction == 0 {
        format!("{integer}{suffix}")
    } else {
        let fraction = format!("{fraction:0width$}", width = precision as usize)
            .trim_end_matches('0')
            .to_owned();
        format!("{integer}.{fraction}{suffix}")
    }
}

fn footer_estimated_cost<'a>(
    attempts: impl IntoIterator<Item = &'a AttemptAccounting>,
) -> Option<String> {
    match estimate_run_cost(attempts) {
        RunCostEstimate::Complete(estimate) | RunCostEstimate::LowerBound(estimate) => {
            Some(format_footer_usd(estimate.as_nanos()))
        }
        RunCostEstimate::Unavailable => None,
    }
}

pub(crate) fn format_footer_usd(nanodollars: u128) -> String {
    const NANODOLLARS_PER_DOLLAR: u128 = 1_000_000_000;
    let mut decimals = footer_cost_decimals(nanodollars);
    let mut rounded = round_nanodollars(nanodollars, decimals);
    let rounded_decimals = footer_cost_decimals(rounded);
    if rounded_decimals != decimals {
        decimals = rounded_decimals;
        rounded = round_nanodollars(nanodollars, decimals);
    }
    let dollars = rounded / NANODOLLARS_PER_DOLLAR;
    let divisor = 10_u128.pow(9 - decimals);
    let fractional = (rounded % NANODOLLARS_PER_DOLLAR) / divisor;
    format!("${dollars}.{fractional:0width$}", width = decimals as usize)
}

fn footer_cost_decimals(nanodollars: u128) -> u32 {
    const NANODOLLARS_PER_DOLLAR: u128 = 1_000_000_000;
    let dollars = nanodollars / NANODOLLARS_PER_DOLLAR;
    if dollars > 0 {
        return 2.max(4_u32.saturating_sub(decimal_digits(dollars)));
    }
    if nanodollars == 0 {
        return 2;
    }
    2.max((13_u32.saturating_sub(decimal_digits(nanodollars))).min(9))
}

fn decimal_digits(mut value: u128) -> u32 {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}

fn round_nanodollars(nanodollars: u128, decimals: u32) -> u128 {
    let unit = 10_u128.pow(9 - decimals);
    let quotient = nanodollars / unit;
    let remainder = nanodollars % unit;
    quotient
        .saturating_add(u128::from(remainder >= unit.div_ceil(2)))
        .saturating_mul(unit)
}

fn add_total(total: &mut Option<u128>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or_default().saturating_add(u128::from(value)));
    }
}

fn accounting_fields(accounting: &SessionAccounting) -> Vec<String> {
    [
        ("↑", accounting.input),
        ("↓", accounting.output),
        ("R", accounting.cache_read),
        ("W", accounting.cache_write),
        ("r", accounting.reasoning),
        ("Σ", accounting.total),
    ]
    .into_iter()
    .filter_map(|(label, total)| total.map(|total| format!("{label}{total}")))
    .collect()
}

fn step_accounting(attempts: &[AttemptAccounting]) -> Option<String> {
    let mut accounting = SessionAccounting::default();
    for attempt in attempts {
        accounting.add(attempt);
    }
    let fields = accounting_fields(&accounting);
    let cost = estimated_cost(attempts.iter());
    if fields.is_empty() && cost.is_none() {
        return (attempts.len() > 1).then(|| format!("{} attempts", attempts.len()));
    }
    let mut line = fields.join(" ");
    if let Some(cost) = cost {
        if !line.is_empty() {
            line.push_str(" · ");
        }
        line.push_str(&cost);
    }
    if attempts.len() > 1 {
        line = format!("{} attempts · {line}", attempts.len());
    }
    Some(line)
}

fn estimated_cost<'a>(attempts: impl IntoIterator<Item = &'a AttemptAccounting>) -> Option<String> {
    match estimate_run_cost(attempts) {
        RunCostEstimate::Complete(estimate) | RunCostEstimate::LowerBound(estimate) => {
            Some(format_usd(estimate.as_nanos()))
        }
        RunCostEstimate::Unavailable => None,
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

fn append_observation(
    content: &mut Vec<ArenaContent>,
    label: &'static str,
    observation: &Observation,
) {
    if observation.total_bytes > 0 {
        content.push(ArenaContent {
            label,
            text: observation_text(observation),
        });
    }
}

#[cfg(test)]
fn append_display_gap(lines: &mut Vec<String>, progress_gap: bool) {
    if progress_gap {
        lines.push("display gap: some live progress was dropped".to_owned());
    }
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

pub(crate) fn observation_text(observation: &Observation) -> String {
    let mut text = observation.head.clone();
    if let Some(tail) = &observation.tail {
        text.push_str(&format!(
            "\n[{} lines, {} bytes omitted]\n",
            observation.omitted_lines, observation.omitted_bytes
        ));
        text.push_str(tail);
    }
    text
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
    use crate::theme::{DARK, LIGHT};

    use std::path::PathBuf;
    use std::time::Duration;

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    use super::{
        ArenaBlock, CardRenderBlock, Footer, FooterContext, FooterField, FooterIdentity,
        LayoutBlock, LivePhase, ModelFooter, NormalisedBlock, RenderBlock, SubagentFinishedCard,
        Transcript, TranscriptBlock, TranscriptLayout, assistant_text, format_footer_usd,
        format_token_count, markdown, muted_page_style, normalise_terminal_text, own_styled_text,
        page_style, stacked_height, step_accounting, subagent_preview, tool_call_label,
    };
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, Observation, PythonError, RunPreview, ToolCall,
        ToolCallSummary, ToolCallToken, ToolName,
    };
    use crate::config::models::{LUNA, Provider, SOL, TERRA};
    use crate::config::{ModelSelection, ready_snapshot_for_test};
    use crate::highlight::PythonHighlighter;
    use crate::model::{AttemptAccounting, ModelProgress, ModelUsage, SkillActivation};
    use crate::pricing::{RunCostEstimate, estimate_run_cost};
    use crate::runtime::{
        CompactionAcknowledgement, CompactionLifecycle, CompactionSettlement, ModelStepId, Record,
        RestoredTimelineItem, RunId, RuntimeDisabledReason, RuntimeEvent, RuntimeInitialState,
        StoredRecord, TerminalDetails,
    };
    use crate::selection::{DocumentPosition, Selection};
    use crate::session::{CompactionOutcome, CompactionReason, SessionEntryId};
    use crate::skills::SkillOrigin;
    use crate::subagents::{TaskRecord, TaskState};

    #[test]
    fn page_uses_terminal_background() {
        assert_eq!(page_style(&DARK).bg, Some(Color::Reset));
    }

    #[test]
    fn metadata_acknowledgements_do_not_change_display_history() {
        let mut transcript = Transcript::default();

        assert!(!transcript.apply(RuntimeEvent::SettingsAccepted { revision: 1 }, false));
        assert!(!transcript.apply(
            RuntimeEvent::NameAccepted {
                revision: 2,
                name: "Saved name".to_owned(),
            },
            false,
        ));
        assert!(transcript.blocks().is_empty());
    }

    #[test]
    fn compaction_busy_releases_submitting_without_clearing_foreground_state() {
        let mut transcript = Transcript::default();

        assert!(transcript.begin_submission());
        assert!(transcript.apply(
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: CompactionAcknowledgement::Accepted,
            },
            false,
        ));
        assert!(transcript.apply(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Started,
            },
            false,
        ));

        assert!(transcript.apply(
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: None,
            },
            false,
        ));
        assert!(!transcript.is_idle());
        assert_eq!(transcript.status(), "compacting context");
        assert!(!transcript.begin_submission());
        assert!(transcript.apply(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Settled(CompactionSettlement::Cancelled),
            },
            false,
        ));
        assert!(transcript.is_idle());
    }

    #[test]
    fn compaction_lifecycle_owns_foreground_state_without_a_run() {
        let mut transcript = Transcript::default();

        assert!(transcript.apply(
            RuntimeEvent::CompactionAcknowledged {
                acknowledgement: CompactionAcknowledgement::Accepted,
            },
            false,
        ));
        assert!(!transcript.is_idle());
        assert_eq!(transcript.status(), "idle");

        assert!(transcript.apply(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Started,
            },
            false,
        ));
        assert!(!transcript.is_idle());
        assert_eq!(transcript.status(), "compacting context");

        assert!(transcript.apply(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Settled(CompactionSettlement::Applied),
            },
            false,
        ));
        assert!(transcript.is_idle());
        assert_eq!(transcript.status(), "idle");

        assert!(!transcript.apply(
            RuntimeEvent::CompactionLifecycle {
                lifecycle: CompactionLifecycle::Started,
            },
            false,
        ));
        assert!(transcript.is_idle());
    }

    fn threshold_compaction_started(run_id: RunId) -> RuntimeEvent {
        RuntimeEvent::AutomaticCompactionStarted {
            run_id,
            initial_overflow: None,
        }
    }

    fn automatic_compaction_card(run_id: RunId) -> RuntimeEvent {
        RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id,
                record: Record::Compaction {
                    outcome: CompactionOutcome::Applied {
                        summary: "summary".to_owned(),
                        first_retained: SessionEntryId(4),
                        context_tokens: 1_250,
                        reason: CompactionReason::AutomaticThreshold,
                        attempts: vec![usage(3, 2)],
                    },
                },
            },
            progress_gap: false,
        }
    }

    fn submitted_run(run_id: RunId) -> Transcript {
        let mut transcript = Transcript::default();
        assert!(transcript.begin_submission());
        transcript.apply(
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
            false,
        );
        transcript
    }

    #[test]
    fn automatic_compaction_owns_status_and_spinner_until_its_card() {
        let run_id = RunId(3);
        let mut transcript = submitted_run(run_id);

        assert!(transcript.apply(threshold_compaction_started(run_id), false));
        assert_eq!(transcript.status(), "compacting context");
        assert!(transcript.is_compacting());
        assert!(!transcript.is_generating());
        assert!(!transcript.has_foreground_compaction());
        assert!(!transcript.is_idle());
        assert!(!transcript.begin_submission());

        for step_id in [ModelStepId(2), ModelStepId(3)] {
            assert!(!transcript.apply(RuntimeEvent::RunStarted { run_id }, false));
            assert!(!transcript.apply(
                RuntimeEvent::ModelStepStarted {
                    run_id,
                    step_id,
                    epoch: 0,
                },
                false,
            ));
            assert!(!transcript.apply(
                RuntimeEvent::ModelProgress {
                    run_id,
                    step_id,
                    epoch: 0,
                    progress: ModelProgress::AssistantTextChunk {
                        text: "stale streamed text".to_owned(),
                    },
                },
                false,
            ));
        }
        assert_eq!(transcript.status(), "compacting context");
        assert!(transcript.is_compacting());
        assert!(!transcript.is_generating());
        assert!(!rendered_layout_text(&mut transcript).contains("stale streamed text"));

        assert!(transcript.apply(automatic_compaction_card(run_id), false));
        assert!(!transcript.is_compacting());
        assert!(!transcript.is_idle());
        assert_eq!(transcript.status(), "starting run");
        assert!(transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id,
                step_id: ModelStepId(4),
                epoch: 0,
            },
            false,
        ));
        assert!(transcript.is_generating());
        assert_eq!(transcript.status(), "generating");
    }

    #[test]
    fn automatic_compaction_ends_on_terminal_settlement_or_disable() {
        let run_id = RunId(3);
        let terminal = RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id,
                record: Record::CancelledRun {
                    terminal: TerminalDetails::empty(),
                },
            },
            progress_gap: false,
        };
        for ending in [
            terminal,
            RuntimeEvent::RunSettled { run_id },
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
        ] {
            let mut transcript = submitted_run(run_id);
            assert!(transcript.apply(threshold_compaction_started(run_id), false));

            assert!(transcript.apply(ending.clone(), false), "{ending:?}");
            assert!(!transcript.is_compacting(), "{ending:?}");
            assert!(!transcript.is_generating(), "{ending:?}");
            transcript.apply(RuntimeEvent::RunSettled { run_id }, false);
            assert_ne!(transcript.status(), "compacting context", "{ending:?}");
            assert!(
                !transcript.apply(threshold_compaction_started(run_id), false),
                "{ending:?}"
            );
            assert!(!transcript.is_compacting(), "{ending:?}");
        }
    }

    fn overflow_compaction_card(run_id: RunId) -> RuntimeEvent {
        RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id,
                record: Record::Compaction {
                    outcome: CompactionOutcome::Applied {
                        summary: "summary".to_owned(),
                        first_retained: SessionEntryId(4),
                        context_tokens: 1_250,
                        reason: CompactionReason::ContextOverflow,
                        attempts: Vec::new(),
                    },
                },
            },
            progress_gap: false,
        }
    }

    #[test]
    fn overflow_retry_reset_rejects_stale_generation_and_keeps_settled_usage() {
        let run_id = RunId(3);
        let step_id = ModelStepId(4);
        let started = |epoch| RuntimeEvent::ModelStepStarted {
            run_id,
            step_id,
            epoch,
        };
        let progress = |epoch, progress| RuntimeEvent::ModelProgress {
            run_id,
            step_id,
            epoch,
            progress,
        };
        let text = |text: &str| ModelProgress::AssistantTextChunk {
            text: text.to_owned(),
        };
        let mut transcript = submitted_run(run_id);
        for event in [
            started(0),
            progress(0, ModelProgress::AttemptStarted { attempt: 1 }),
            progress(0, text("initial partial")),
            progress(
                0,
                ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: usage(3, 2),
                },
            ),
        ] {
            assert!(transcript.apply(event, false));
        }
        assert!(rendered_layout_text(&mut transcript).contains("initial partial"));
        assert!(transcript.apply(
            RuntimeEvent::AutomaticCompactionStarted {
                run_id,
                initial_overflow: Some((step_id, vec![usage(3, 2)])),
            },
            false,
        ));

        assert!(transcript.apply(
            RuntimeEvent::GenerationRestarted {
                run_id,
                step_id,
                epoch: 1,
            },
            false,
        ));
        assert_eq!(transcript.status(), "compacting context");
        assert!(transcript.is_compacting());
        assert!(!transcript.is_generating());
        assert!(!rendered_layout_text(&mut transcript).contains("initial partial"));

        assert!(transcript.apply(overflow_compaction_card(run_id), false));
        assert_eq!(transcript.status(), "starting run");
        for event in [
            started(0),
            progress(0, ModelProgress::AttemptStarted { attempt: 2 }),
            progress(0, text("delayed stale")),
        ] {
            assert!(!transcript.apply(event, false));
        }
        assert_eq!(transcript.status(), "starting run");
        assert!(!rendered_layout_text(&mut transcript).contains("stale"));

        assert!(transcript.apply(started(1), false));
        assert!(transcript.is_generating());
        assert_eq!(transcript.status(), "generating");
        assert!(transcript.apply(
            progress(1, ModelProgress::AttemptStarted { attempt: 1 }),
            false
        ));
        assert!(transcript.apply(progress(1, text("fresh retry")), false));
        let rendered = rendered_layout_text(&mut transcript);
        assert!(rendered.contains("fresh retry"), "{rendered}");
        assert!(!rendered.contains("partial"), "{rendered}");
        let live = transcript.live_step.as_ref().expect("retry live step");
        assert_eq!(live.attempt, Some(1));
        assert!(live.settled_attempts.is_empty());
        assert!(
            footer_text(&transcript.layout(&DARK, 128))
                .expect("footer")
                .contains("↑3 ↓2")
        );

        assert!(transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id,
                step_id: ModelStepId(5),
                epoch: 0,
            },
            false,
        ));
    }

    #[test]
    fn initial_overflow_usage_bills_once_through_recovery_without_occupancy() {
        let run_id = RunId(1);
        let step_id = ModelStepId(1);
        let next_step_id = ModelStepId(2);
        let initial = detailed_usage(
            Some(100_000),
            Some(0),
            Some(75_000),
            Some(0),
            Some(0),
            Some(100_000),
        );
        let next = detailed_usage(
            Some(1_000),
            Some(0),
            Some(500),
            Some(0),
            Some(0),
            Some(1_000),
        );
        let started = |step_id, epoch| RuntimeEvent::ModelStepStarted {
            run_id,
            step_id,
            epoch,
        };
        let settled = |step_id, epoch, accounting| RuntimeEvent::ModelProgress {
            run_id,
            step_id,
            epoch,
            progress: ModelProgress::AttemptSettled {
                attempt: 1,
                accounting,
            },
        };
        let footer = |transcript: &mut Transcript| {
            footer_text(&transcript.layout(&DARK, 128)).unwrap_or_default()
        };
        let assert_initial_only = |transcript: &mut Transcript, context: &str| {
            let footer = footer(transcript);
            assert!(footer.contains("↑100k"), "{context}: {footer}");
            assert!(footer.contains('$'), "{context}: {footer}");
            assert!(!footer.contains("%/"), "{context}: {footer}");
            assert!(!footer.contains("CH"), "{context}: {footer}");
        };

        for initial_seen in [true, false] {
            let case = if initial_seen { "seen" } else { "blocked" };
            let mut transcript = submitted_run(run_id);
            assert!(transcript.apply(started(step_id, 0), false));
            assert!(transcript.apply(
                RuntimeEvent::ModelProgress {
                    run_id,
                    step_id,
                    epoch: 0,
                    progress: ModelProgress::AttemptStarted { attempt: 1 },
                },
                false,
            ));
            if initial_seen {
                assert!(transcript.apply(settled(step_id, 0, initial.clone()), false));
            }

            assert!(transcript.apply(
                RuntimeEvent::AutomaticCompactionStarted {
                    run_id,
                    initial_overflow: Some((step_id, vec![initial.clone()])),
                },
                false,
            ));
            assert_eq!(transcript.status(), "compacting context", "{case}");
            assert!(transcript.is_compacting(), "{case}");
            assert!(!transcript.is_generating(), "{case}");
            assert_initial_only(&mut transcript, &format!("{case} summary"));
            assert!(!transcript.apply(settled(step_id, 0, initial.clone()), false));

            transcript.apply(
                RuntimeEvent::GenerationRestarted {
                    run_id,
                    step_id,
                    epoch: 1,
                },
                false,
            );
            assert!(transcript.apply(overflow_compaction_card(run_id), false));
            assert!(!transcript.apply(started(step_id, 0), false));
            assert!(!transcript.apply(settled(step_id, 0, initial.clone()), false));
            assert_eq!(transcript.status(), "starting run", "{case}");
            assert_initial_only(&mut transcript, &format!("{case} card"));

            assert!(transcript.apply(started(step_id, 1), false));
            assert_eq!(transcript.status(), "generating", "{case}");
            assert!(transcript.apply(
                settled(step_id, 1, AttemptAccounting::UsageUnavailable),
                false
            ));
            assert_initial_only(&mut transcript, &format!("{case} retry"));

            assert!(transcript.apply(
                completed_execution(
                    1,
                    1,
                    "retry()",
                    "",
                    vec![AttemptAccounting::UsageUnavailable],
                ),
                false,
            ));
            assert_initial_only(&mut transcript, &format!("{case} code"));

            assert!(transcript.apply(started(next_step_id, 0), false));
            assert_eq!(transcript.status(), "generating", "{case}");
            assert_initial_only(&mut transcript, &format!("{case} next step"));
            assert!(transcript.apply(settled(next_step_id, 0, next.clone()), false));
            let streaming = footer(&mut transcript);
            assert!(streaming.contains("↑101k"), "{case}: {streaming}");
            assert!(streaming.contains("CH50.0%"), "{case}: {streaming}");
            assert!(streaming.contains("%/"), "{case}: {streaming}");

            assert!(transcript.apply(
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id,
                        record: Record::FinalMessage {
                            step_id: next_step_id,
                            text: "done".to_owned(),
                            reasoning_summary: String::new(),
                            terminal: TerminalDetails {
                                evidence: None,
                                attempts: vec![next.clone()],
                                overflow_recovery: Some(overflow_recovery(vec![initial.clone()])),
                            },
                        },
                    },
                    progress_gap: false,
                },
                false,
            ));
            assert_eq!(footer(&mut transcript), streaming, "{case} terminal");
            assert!(transcript.apply(RuntimeEvent::RunSettled { run_id }, false));
            assert!(transcript.begin_submission());
            transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
            assert!(transcript.apply(
                RuntimeEvent::ModelStepStarted {
                    run_id: RunId(2),
                    step_id: ModelStepId(3),
                    epoch: 0,
                },
                false,
            ));
            assert_eq!(footer(&mut transcript), streaming, "{case} next run");
        }
    }

    #[test]
    fn initial_overflow_usage_survives_disable_and_counts_once_with_a_later_terminal() {
        let run_id = RunId(1);
        let step_id = ModelStepId(1);
        let mut transcript = submitted_run(run_id);
        assert!(transcript.apply(
            RuntimeEvent::AutomaticCompactionStarted {
                run_id,
                initial_overflow: Some((step_id, vec![usage(3, 2)])),
            },
            false,
        ));
        assert!(
            footer_text(&transcript.layout(&DARK, 128))
                .expect("footer")
                .contains("↑3 ↓2")
        );

        assert!(transcript.apply(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
            false,
        ));
        assert!(
            footer_text(&transcript.layout(&DARK, 128))
                .expect("footer after storage failure")
                .contains("↑3 ↓2")
        );

        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id,
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: None,
                            attempts: Vec::new(),
                            overflow_recovery: Some(overflow_recovery(vec![usage(3, 2)])),
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        assert!(
            footer_text(&transcript.layout(&DARK, 128))
                .expect("footer")
                .contains("↑3 ↓2")
        );
    }

    #[test]
    fn compaction_cards_expand_durable_content_before_accounting() {
        let attempts = vec![usage(3, 2)];
        let accounting = step_accounting(&attempts).expect("known usage accounting");
        let cases = [
            (
                CompactionOutcome::Applied {
                    summary: "## Applied detail\n\n[guide](https://example.com)".to_owned(),
                    first_retained: SessionEntryId(4),
                    context_tokens: 1_250,
                    reason: CompactionReason::Manual,
                    attempts: attempts.clone(),
                },
                "Compacted 1.25k tokens · Ctrl-O to expand",
                "guide (https://example.com)",
                true,
                DARK.tool_success_bg,
            ),
            (
                CompactionOutcome::Failed {
                    diagnostic: "first line\n[plain](https://example.com)".to_owned(),
                    reason: CompactionReason::AutomaticThreshold,
                    attempts: attempts.clone(),
                },
                "Failed · Ctrl-O to expand",
                "[plain](https://example.com)",
                true,
                DARK.tool_error_bg,
            ),
            (
                CompactionOutcome::Cancelled {
                    reason: CompactionReason::ContextOverflow,
                    attempts: attempts.clone(),
                },
                "Cancelled · Ctrl-O to expand",
                "Cancelled",
                false,
                DARK.tool_error_bg,
            ),
        ];

        for (outcome, collapsed_line, expanded_content, hides_expanded_content, background) in cases
        {
            let mut transcript = Transcript::default();
            transcript.apply(
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(3),
                        record: Record::Compaction { outcome },
                    },
                    progress_gap: false,
                },
                false,
            );

            let collapsed = rendered_layout_text(&mut transcript);
            assert!(collapsed.contains("compaction ·"), "{collapsed}");
            assert!(collapsed.contains(collapsed_line), "{collapsed}");
            if hides_expanded_content {
                assert!(!collapsed.contains(expanded_content), "{collapsed}");
            }
            assert!(!collapsed.contains(&accounting), "{collapsed}");
            let buffer = rendered_buffer(&mut transcript, 80);
            assert_row_background(&buffer, row_containing(&buffer, collapsed_line), background);

            assert!(transcript.toggle_arena_expansion());
            let expanded = rendered_layout_text(&mut transcript);
            assert!(expanded.contains(expanded_content), "{expanded}");
            assert!(expanded.contains(&accounting), "{expanded}");
            let buffer = rendered_buffer(&mut transcript, 80);
            assert_row_background(
                &buffer,
                row_containing(&buffer, expanded_content),
                background,
            );
        }
    }

    #[test]
    fn skill_activation_card_follows_its_request_and_expands_to_the_skill_text() {
        let activation = SkillActivation::new(
            "review".to_owned(),
            PathBuf::from("/workspace/.agents/skills/review"),
            "---\nname: review\ndescription: Review\n---\nReview every change.".to_owned(),
            SkillOrigin::Project,
            Some(PathBuf::from("/workspace")),
        )
        .unwrap();
        let mut transcript = Transcript::default();
        append_records(
            &mut transcript,
            vec![
                StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "/skill:review".to_owned(),
                        selection: None,
                    },
                },
                StoredRecord {
                    run_id: RunId(1),
                    record: Record::SkillActivation { activation },
                },
            ],
        );

        let collapsed = rendered_layout_text(&mut transcript);
        let card = collapsed
            .find("Skill · review · project · Ctrl-O to expand")
            .unwrap_or_else(|| panic!("{collapsed}"));
        assert!(
            collapsed.find("/skill:review").unwrap() < card,
            "{collapsed}"
        );
        assert!(!collapsed.contains("Review every change."), "{collapsed}");

        assert!(transcript.toggle_arena_expansion());
        let expanded = rendered_layout_text(&mut transcript);
        assert!(expanded.contains("Skill · review · project"), "{expanded}");
        assert!(!expanded.contains("Ctrl-O to expand"), "{expanded}");
        assert!(expanded.contains("description: Review"), "{expanded}");
        assert!(expanded.contains("Review every change."), "{expanded}");
    }

    #[test]
    fn session_restored_replacement_keeps_toggles_and_removes_its_card_on_a_request() {
        let mut transcript = Transcript::default();
        transcript.toggle_reasoning();
        transcript.toggle_arena_expansion();
        let state = RuntimeInitialState {
            metadata: None,
            timeline: vec![RestoredTimelineItem::Record(StoredRecord {
                run_id: RunId(1),
                record: Record::UserRequest {
                    request: "restored request".to_owned(),
                    selection: None,
                },
            })],
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        };

        let mut replacement = transcript.restored_replacement(&state);
        replacement.append_restoration_card();
        assert!(!replacement.show_reasoning);
        assert!(replacement.expand_arenas);
        assert_eq!(
            replacement
                .blocks()
                .iter()
                .map(|block| match block {
                    TranscriptBlock::User { .. } => "user",
                    TranscriptBlock::SessionRestored => "restored",
                    _ => "other",
                })
                .collect::<Vec<_>>(),
            ["user", "restored"]
        );

        assert!(replacement.begin_submission());
        assert!(replacement.apply(
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(RunId(2)),
            },
            false,
        ));
        assert!(replacement.apply(
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
            },
            false,
        ));
        assert!(
            replacement
                .blocks()
                .iter()
                .any(|block| matches!(block, TranscriptBlock::SessionRestored))
        );
        assert!(replacement.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "new request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        ));
        assert!(
            !replacement
                .blocks()
                .iter()
                .any(|block| matches!(block, TranscriptBlock::SessionRestored))
        );
    }

    #[test]
    fn session_restored_card_follows_late_finished_tasks() {
        let task = TaskRecord::for_test(
            "0",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let state = RuntimeInitialState {
            metadata: None,
            timeline: vec![
                RestoredTimelineItem::Record(StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "restored request".to_owned(),
                        selection: None,
                    },
                }),
                RestoredTimelineItem::SubagentSpawned(task.clone()),
                RestoredTimelineItem::SubagentFinished(task),
            ],
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        };
        let mut transcript = Transcript::default().restored_replacement(&state);
        transcript.append_restoration_card();
        let text = rendered_layout_text(&mut transcript);

        assert!(
            text.find("subagent:0").expect("finished task")
                < text.find("Session restored.").expect("restoration card")
        );
    }

    fn run_records(selections: &[Option<ModelSelection>]) -> Vec<StoredRecord> {
        selections
            .iter()
            .zip(1..)
            .flat_map(|(selection, run_id)| {
                [
                    StoredRecord {
                        run_id: RunId(run_id),
                        record: Record::UserRequest {
                            request: format!("request {run_id}"),
                            selection: *selection,
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
            })
            .collect()
    }

    fn append_records(transcript: &mut Transcript, records: Vec<StoredRecord>) {
        for record in records {
            transcript.apply(
                RuntimeEvent::RecordAppended {
                    record,
                    progress_gap: false,
                },
                false,
            );
        }
    }

    fn is_model_change(block: &TranscriptBlock) -> bool {
        matches!(block, TranscriptBlock::ModelChange { .. })
    }

    #[test]
    fn model_change_card_sits_above_a_request_whose_selection_differs() {
        let sol_low = ModelSelection::new(&SOL, "low").unwrap();
        let terra_high = ModelSelection::new(&TERRA, "high").unwrap();
        let mut transcript = Transcript::default();
        append_records(
            &mut transcript,
            run_records(&[Some(sol_low), Some(terra_high)]),
        );

        let blocks = transcript.blocks();
        let second_request = blocks
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::User { .. }))
            .expect("second request");
        assert_eq!(
            blocks[second_request - 1],
            TranscriptBlock::ModelChange {
                selection: terra_high
            }
        );
        assert_eq!(
            blocks.iter().filter(|block| is_model_change(block)).count(),
            1
        );

        let buffer = rendered_buffer(&mut transcript, 60);
        let card_row = row_containing(&buffer, "Model selected:");
        assert_eq!(
            row_text(&buffer, card_row).trim(),
            "Model selected: openai/gpt-5.6-terra · high"
        );
        assert!(row_containing(&buffer, "answer") < card_row);
        assert!(card_row < row_containing(&buffer, "request 2"));
        for column in 0..buffer.area.width {
            let cell = &buffer[(column, card_row as u16)];
            assert_eq!(cell.fg, DARK.muted);
            assert_eq!(cell.bg, Color::Reset);
        }
    }

    #[test]
    fn model_change_card_needs_two_known_different_run_selections() {
        let sol_low = Some(ModelSelection::new(&SOL, "low").unwrap());
        let terra_high = Some(ModelSelection::new(&TERRA, "high").unwrap());
        for selections in [
            vec![terra_high],
            vec![sol_low, sol_low],
            vec![None, terra_high],
            vec![sol_low, None],
            vec![sol_low, None, terra_high],
        ] {
            let mut transcript = Transcript::default();
            append_records(&mut transcript, run_records(&selections));

            assert!(
                !transcript.blocks().iter().any(is_model_change),
                "{selections:?}"
            );
            assert!(
                !rendered_layout_text(&mut transcript).contains("Model selected"),
                "{selections:?}"
            );
        }
    }

    #[test]
    fn model_change_card_looks_past_compaction_to_the_previous_run() {
        let terra_high = ModelSelection::new(&TERRA, "high").unwrap();
        let mut records = run_records(&[
            Some(ModelSelection::new(&SOL, "low").unwrap()),
            Some(terra_high),
        ]);
        records.insert(
            2,
            StoredRecord {
                run_id: RunId(1),
                record: Record::Compaction {
                    outcome: CompactionOutcome::Applied {
                        summary: "summary".to_owned(),
                        first_retained: SessionEntryId(2),
                        context_tokens: 1,
                        reason: CompactionReason::Manual,
                        attempts: Vec::new(),
                    },
                },
            },
        );
        let mut transcript = Transcript::default();
        append_records(&mut transcript, records);

        let blocks = transcript.blocks();
        let second_request = blocks
            .iter()
            .rposition(|block| matches!(block, TranscriptBlock::User { .. }))
            .expect("second request");
        assert!(matches!(
            blocks[second_request - 2],
            TranscriptBlock::Compaction(..)
        ));
        assert_eq!(
            blocks[second_request - 1],
            TranscriptBlock::ModelChange {
                selection: terra_high
            }
        );
        let text = rendered_layout_text(&mut transcript);
        assert!(
            text.find("compaction ·").expect("compaction card")
                < text.find("Model selected").expect("model change card")
        );
    }

    #[test]
    fn restored_model_change_cards_match_live_appends() {
        let sol_low = ModelSelection::new(&SOL, "low").unwrap();
        let terra_high = ModelSelection::new(&TERRA, "high").unwrap();
        let records = run_records(&[
            Some(sol_low),
            Some(terra_high),
            None,
            Some(terra_high),
            Some(sol_low),
        ]);
        let mut live = Transcript::default();
        append_records(&mut live, records.clone());
        let state = RuntimeInitialState {
            metadata: None,
            timeline: records
                .into_iter()
                .map(RestoredTimelineItem::Record)
                .collect(),
            footer_adjustment: Vec::new(),
            disabled_reason: None,
        };

        let mut restored = Transcript::default().restored_replacement(&state);

        assert_eq!(
            live.blocks()
                .into_iter()
                .filter(is_model_change)
                .collect::<Vec<_>>(),
            [
                TranscriptBlock::ModelChange {
                    selection: terra_high
                },
                TranscriptBlock::ModelChange { selection: sol_low },
            ]
        );
        assert_eq!(restored.blocks(), live.blocks());
        let restored_text = rendered_layout_text(&mut restored);
        assert_eq!(restored_text.matches("Model selected").count(), 2);
        assert_eq!(restored_text, rendered_layout_text(&mut live));
    }

    #[test]
    fn copying_a_model_change_card_yields_its_text() {
        let mut transcript = Transcript::default();
        append_records(
            &mut transcript,
            run_records(&[
                Some(ModelSelection::new(&SOL, "low").unwrap()),
                Some(ModelSelection::new(&TERRA, "high").unwrap()),
            ]),
        );

        assert_eq!(
            copied_text(&mut transcript, 80),
            "request 1\n\nanswer\n\nModel selected: openai/gpt-5.6-terra · high\n\nrequest 2\n\nanswer"
        );
    }

    #[test]
    fn one_page_row_separates_consecutive_blocks() {
        let mut records = run_records(&[None, None]);
        let Record::FinalMessage {
            reasoning_summary, ..
        } = &mut records[1].record
        else {
            unreachable!("run_records pairs requests with final messages");
        };
        *reasoning_summary = "thought".to_owned();
        let mut transcript = Transcript::default();
        append_records(&mut transcript, records);

        let buffer = rendered_buffer(&mut transcript, 20);
        let rows = (0..usize::from(buffer.area.height))
            .map(|row| {
                (
                    row_text(&buffer, row).trim().to_owned(),
                    buffer[(0, row as u16)].bg,
                )
            })
            .collect::<Vec<_>>();

        let user = DARK.user_message_bg;
        let card = DARK.card_bg;
        let page = Color::Reset;
        let row = |text: &str, bg: Color| (text.to_owned(), bg);
        assert_eq!(
            rows,
            [
                row("", user),
                row("request 1", user),
                row("", user),
                row("", page),
                row("", card),
                row("thought", card),
                row("", card),
                row("", page),
                row("answer", page),
                row("", page),
                row("", user),
                row("request 2", user),
                row("", user),
                row("", page),
                row("answer", page),
            ]
        );
    }

    fn rendered_rows_with_selection(
        transcript: &mut Transcript,
        selection: ModelSelection,
    ) -> Vec<String> {
        let layout = transcript.layout_with_footer(
            &DARK,
            80,
            FooterContext {
                workspace_root: None,
                home: None,
                name: None,
                selection,
            },
        );
        let height = u16::try_from(layout.total_height()).expect("test transcript height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, height));
        layout.render(buffer.area, 0, &mut buffer);
        (0..usize::from(height))
            .map(|row| row_text(&buffer, row).trim().to_owned())
            .filter(|row| !row.is_empty())
            .collect()
    }

    #[test]
    fn layout_with_footer_leaves_the_pending_model_change_to_the_prompt_area() {
        let sol_low = ModelSelection::new(&SOL, "low").unwrap();
        let terra_high = ModelSelection::new(&TERRA, "high").unwrap();
        let mut records = run_records(&[Some(sol_low), Some(terra_high)]);
        let second_run = records.split_off(2);
        let mut transcript = Transcript::default();
        append_records(&mut transcript, records);

        assert_eq!(
            transcript.pending_model_change(terra_high),
            Some(terra_high)
        );
        let rows = rendered_rows_with_selection(&mut transcript, terra_high);
        assert!(!rows.iter().any(|row| row.contains("Model selected")));
        assert_eq!(rows.last().map(String::as_str), Some("answer"));

        append_records(&mut transcript, second_run);

        assert_eq!(transcript.pending_model_change(terra_high), None);
        assert_eq!(transcript.pending_model_change(sol_low), Some(sol_low));
        let rows = rendered_rows_with_selection(&mut transcript, sol_low);
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("Model selected"))
                .collect::<Vec<_>>(),
            ["Model selected: openai/gpt-5.6-terra · high"]
        );
        assert_eq!(rows.last().map(String::as_str), Some("answer"));
    }

    #[test]
    fn pending_model_change_needs_a_known_last_run_selection() {
        let sol_low = Some(ModelSelection::new(&SOL, "low").unwrap());
        let terra_high = ModelSelection::new(&TERRA, "high").unwrap();
        for selections in [vec![], vec![None], vec![sol_low, None]] {
            let mut transcript = Transcript::default();
            append_records(&mut transcript, run_records(&selections));

            assert_eq!(
                transcript.pending_model_change(terra_high),
                None,
                "{selections:?}"
            );
        }
    }

    #[test]
    fn disabling_after_storage_failure_releases_queued_subagent_cards_after_python() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "await start_subagent('review')".to_owned(),
                },
            },
            false,
        ));
        assert!(
            transcript.apply(
                RuntimeEvent::SubagentFinished {
                    record: TaskRecord::for_test(
                        "c1",
                        1,
                        ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                        None,
                        1,
                        Vec::new(),
                    )
                    .with_history_steps_for_test(Some(1), Some(1)),
                },
                false,
            )
        );

        assert!(transcript.apply(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
            false,
        ));

        assert!(transcript.live_step.is_some());
        let rendered = rendered_layout_text(&mut transcript);
        let python = rendered
            .find("await start_subagent('review')")
            .expect("Python card");
        let finished = rendered.find("subagent:c1").expect("finished entry");
        assert!(python < finished, "{rendered}");

        let mut late = Transcript::default();
        start_live_run(&mut late);
        late.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "late_python()".to_owned(),
                },
            },
            false,
        );
        late.apply(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
            false,
        );
        late.apply(
            RuntimeEvent::SubagentFinished {
                record: TaskRecord::for_test(
                    "c2",
                    1,
                    ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                    None,
                    1,
                    Vec::new(),
                )
                .with_history_steps_for_test(Some(1), Some(1)),
            },
            false,
        );
        let rendered = rendered_layout_text(&mut late);
        assert!(
            rendered.find("late_python()").unwrap() < rendered.find("subagent:c2").unwrap(),
            "{rendered}"
        );

        let mut no_live = Transcript::default();
        no_live.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        no_live.apply(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::SessionStorageWriteFailed,
            },
            false,
        );
        no_live.apply(
            RuntimeEvent::SubagentFinished {
                record: TaskRecord::for_test(
                    "c3",
                    1,
                    ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                    None,
                    1,
                    Vec::new(),
                )
                .with_history_steps_for_test(Some(1), Some(1)),
            },
            false,
        );
        assert!(no_live.toggle_arena_expansion());
    }

    #[test]
    fn delayed_generation_progress_keeps_execution_state_and_authoritative_content() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptStarted { attempt: 1 },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "discarded_python()".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: Some("Authoritative commentary.".to_owned()),
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptStarted { attempt: 2 },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "Authoritative commentary.".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "executed_python()".to_owned(),
                },
            },
            false,
        );

        let live = transcript.live_step.as_ref().unwrap();
        assert_eq!(live.phase, LivePhase::Executing);
        assert_eq!(live.assistant_text, "Authoritative commentary.");
        assert_eq!(live.decoded_python, "executed_python()");
    }

    #[test]
    fn terminal_settlement_does_not_strand_late_anchored_cards() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: None,
                            attempts: Vec::new(),
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let record = TaskRecord::for_test(
            "late",
            1,
            ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        )
        .with_history_steps_for_test(None, Some(1));

        transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: record.clone(),
            },
            false,
        );
        assert!(transcript.pending_subagent_blocks.is_empty());
        assert!(rendered_layout_text(&mut transcript).contains("✓ subagent:late · "));

        let mut restored = Transcript::default();
        restored.append_subagent_card_with_anchor(&record, false);
        assert!(restored.pending_subagent_blocks.is_empty());
    }

    #[test]
    fn finished_entry_anchor_queues_before_execution_progress_arrives() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: TaskRecord::for_test(
                    "early",
                    1,
                    ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                    None,
                    1,
                    Vec::new(),
                )
                .with_history_steps_for_test(Some(1), Some(1)),
            },
            false,
        );
        assert!(!rendered_layout_text(&mut transcript).contains("subagent:early"));

        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "spawn_python()".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            completed_execution(1, 1, "spawn_python()", "", Vec::new()),
            false,
        );

        let rendered = rendered_layout_text(&mut transcript);
        assert!(
            rendered.find("spawn_python()").unwrap()
                < rendered.find("✓ subagent:early · ").unwrap(),
            "{rendered}"
        );
    }

    #[test]
    fn finished_entries_use_the_captured_history_anchor_not_delivery_time() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "current_python()".to_owned(),
                },
            },
            false,
        );

        transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: TaskRecord::for_test(
                    "earlier",
                    1,
                    ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
                    None,
                    1,
                    Vec::new(),
                ),
            },
            false,
        );

        let rendered = rendered_layout_text(&mut transcript);
        assert!(
            rendered.find("✓ subagent:earlier · ").unwrap()
                < rendered.find("current_python()").unwrap(),
            "{rendered}"
        );
    }

    #[test]
    fn mutation_tool_labels_are_path_free() {
        assert_eq!(
            tool_call_label(&ToolCall {
                tool: ToolName::Write,
                summary: ToolCallSummary::Created,
                subject: None,
                duration: None,
            }),
            "write · created"
        );
        assert_eq!(
            tool_call_label(&ToolCall {
                tool: ToolName::Write,
                summary: ToolCallSummary::Replaced,
                subject: None,
                duration: None,
            }),
            "write · replaced"
        );
        assert_eq!(
            tool_call_label(&ToolCall {
                tool: ToolName::Edit,
                summary: ToolCallSummary::Applied,
                subject: None,
                duration: None,
            }),
            "edit · applied"
        );
    }

    #[test]
    fn finished_subagent_entries_follow_the_active_python_in_publication_order() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptStarted { attempt: 2 },
            },
            false,
        );
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "await start_subagent('review')".to_owned(),
                },
            },
            false,
        ));
        assert!(transcript.is_executing_step(RunId(1), ModelStepId(1)));
        let child = TaskRecord::for_test(
            "c1",
            1,
            ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        )
        .with_history_steps_for_test(Some(1), Some(1));

        assert!(transcript.apply(RuntimeEvent::SubagentFinished { record: child }, false));

        let live = rendered_layout_text(&mut transcript);
        assert!(live.contains("await start_subagent('review')"), "{live}");
        assert!(!live.contains("subagent:c1"), "{live}");

        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(2),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "later_python()".to_owned(),
                },
            },
            false,
        ));
        assert!(transcript.apply(
            completed_execution(1, 1, "await start_subagent('review')", "", Vec::new(),),
            false,
        ));
        let settled = rendered_layout_text(&mut transcript);
        let positions = [
            "await start_subagent('review')",
            "✓ subagent:c1 · openai/gpt-5.6-terra · ",
            "test child",
            "later_python()",
        ]
        .map(|text| settled.find(text).expect(text));
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{settled}"
        );
    }

    #[test]
    fn finished_subagent_result_uses_three_wrapped_rows_until_ctrl_o_expands_and_recollapses_it() {
        let message = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
        let mut transcript = Transcript::default();
        transcript.append_settled_blocks([finished_subagent_card(
            "child1",
            TaskState::Completed,
            "1 step · 00:01",
            "review",
            Some(message),
        )]);

        let collapsed = copied_text(&mut transcript, 20);
        assert_eq!(
            collapsed,
            "✓ subagent:child1 · 1 step · 00:01\nreview\nalpha beta gamma\ndelta epsilon zeta\neta theta iota\n… Ctrl-O to expand"
        );

        assert!(transcript.toggle_arena_expansion());
        let expanded = copied_text(&mut transcript, 20);
        assert_eq!(
            expanded,
            format!("✓ subagent:child1 · 1 step · 00:01\nreview\n{message}")
        );

        assert!(transcript.toggle_arena_expansion());
        assert_eq!(copied_text(&mut transcript, 20), collapsed);
    }

    #[test]
    fn collapsed_subagent_result_preserves_wide_graphemes_when_copied() {
        let prompt = "日本🦀e\u{301}日本🦀e\u{301}日本🦀e\u{301}";
        let (rows, omitted) = subagent_preview(prompt, 4, false);
        assert!(omitted);
        assert_eq!(
            rows.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["日本", "🦀e\u{301}", "日本"]
        );

        let mut transcript = Transcript::default();
        transcript.append_settled_blocks([finished_subagent_card(
            "child1",
            TaskState::Completed,
            "1 step",
            "p",
            Some(prompt),
        )]);

        assert_eq!(
            copied_text(&mut transcript, 6),
            "✓ subagent:child1 · 1 step\np\n日本\n🦀e\u{301}\n日本\n… Ctrl-O to expand"
        );
    }

    fn subagent_entry(state: TaskState, prompt: &str, message: Option<&str>) -> TranscriptBlock {
        TranscriptBlock::SubagentFinished(SubagentFinishedCard {
            id: "c1".to_owned(),
            state,
            summary: "openai/gpt-5.6-sol \u{b7} 3 steps \u{b7} 02:05 \u{b7} $0.01".to_owned(),
            prompt: prompt.to_owned(),
            message: message.map(str::to_owned),
        })
    }

    #[test]
    fn spawning_a_subagent_adds_no_history_entry() {
        let child = TaskRecord::for_test(
            "c1",
            2,
            ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let mut transcript = Transcript::default();

        transcript.apply(RuntimeEvent::SubagentSpawned { record: child }, false);

        assert!(transcript.blocks().is_empty());
    }

    #[test]
    fn finished_subagent_entry_is_tinted_and_marked_by_outcome() {
        let summary = "openai/gpt-5.6-sol \u{b7} 3 steps \u{b7} 02:05 \u{b7} $0.01";
        for (state, header, background, glyph) in [
            (
                TaskState::Completed,
                format!("\u{2713} subagent:c1 \u{b7} {summary}"),
                DARK.tool_success_bg,
                DARK.success,
            ),
            (
                TaskState::Failed,
                format!("\u{2717} subagent:c1 failed \u{b7} {summary}"),
                DARK.tool_error_bg,
                DARK.error,
            ),
            (
                TaskState::TimedOut,
                format!("\u{2717} subagent:c1 timed out \u{b7} {summary}"),
                DARK.tool_error_bg,
                DARK.error,
            ),
            (
                TaskState::Cancelled,
                format!("\u{2298} subagent:c1 cancelled \u{b7} {summary}"),
                DARK.tool_pending_bg,
                DARK.muted,
            ),
        ] {
            let mut transcript = Transcript::default();
            transcript.append_settled_blocks([subagent_entry(
                state,
                "review the diff",
                Some("all good"),
            )]);

            let buffer = rendered_buffer(&mut transcript, 80);

            assert_eq!(
                trimmed_rows(&buffer),
                ["", header.as_str(), "review the diff", "all good", ""]
            );
            for row in 0..buffer.area.height {
                assert_row_background(&buffer, usize::from(row), background);
            }
            assert_eq!(buffer[(1, 1)].fg, glyph, "{state:?}");
            assert_eq!(cell_at(&buffer, "subagent:c1").fg, DARK.text);
            assert_eq!(cell_at(&buffer, "openai/").fg, DARK.muted);
            assert_eq!(cell_at(&buffer, "review the diff").fg, DARK.dim);
            assert_eq!(cell_at(&buffer, "all good").fg, DARK.text);
        }
    }

    #[test]
    fn finished_subagent_entry_collapses_prompt_and_result_until_ctrl_o() {
        let prompt = "inspect every module and report back on anything odd";
        let message = "one two three four five six seven eight nine ten eleven twelve";
        let mut transcript = Transcript::default();
        transcript.append_settled_blocks([subagent_entry(
            TaskState::Completed,
            prompt,
            Some(message),
        )]);

        let buffer = rendered_buffer(&mut transcript, 22);
        assert_eq!(
            trimmed_rows(&buffer)[5..],
            [
                "inspect every modul\u{2026}",
                "one two three four",
                "five six seven eight",
                "nine ten eleven",
                "\u{2026} Ctrl-O to expand",
                "",
            ]
        );
        assert_eq!(cell_at(&buffer, "\u{2026} Ctrl-O").fg, DARK.dim);

        transcript.toggle_arena_expansion();
        let expanded = trimmed_rows(&rendered_buffer(&mut transcript, 22));
        assert_eq!(
            expanded[5..],
            [
                "inspect every module",
                "and report back on",
                "anything odd",
                "one two three four",
                "five six seven eight",
                "nine ten eleven",
                "twelve",
                "",
            ]
        );
    }

    #[test]
    fn finished_subagent_card_renders_message_without_exported_data() {
        let mut transcript = Transcript::default();
        transcript.append_settled_blocks([finished_subagent_card(
            "child1",
            TaskState::Completed,
            "3 steps · 02:05 · $0.00005",
            "review",
            Some("**literal result**\x1B[31m"),
        )]);

        let rendered = rendered_layout_text(&mut transcript);
        assert!(
            rendered.contains("✓ subagent:child1 · 3 steps · 02:05 · $0.00005"),
            "{rendered}"
        );
        assert!(
            rendered.contains("**literal result**\\x1B[31m"),
            "{rendered}"
        );
        for absent in ["secret_value", "42", "attempt"] {
            assert!(!rendered.contains(absent), "{rendered}");
        }
        assert!(
            !copied_text(&mut transcript, 20).contains("Ctrl-O to expand"),
            "short results remain fully visible"
        );
    }

    #[test]
    fn finished_subagent_events_project_record_measures() {
        let child = TaskRecord::for_test(
            "c1",
            2,
            ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
            None,
            1,
            vec![usage(1, 1)],
        );
        let mut transcript = Transcript::default();

        assert!(transcript.apply(
            RuntimeEvent::SubagentSpawned {
                record: child.clone(),
            },
            false,
        ));
        assert!(transcript.apply(RuntimeEvent::SubagentFinished { record: child }, false));

        let copied = copied_text(&mut transcript, 80);
        assert!(
            copied.contains("✓ subagent:c1 · openai/gpt-5.6-terra · 0 steps · 00:00 · $0.00002"),
            "{copied}"
        );
    }

    #[test]
    fn subagent_cards_normalise_unicode_and_controls_at_narrow_widths() {
        let prompt = "Café 日本 🦀 e\u{301}\t\r\nnext\u{7}";
        let diagnostic =
            "failed \u{1b}日本 🦀 e\u{301} alpha beta gamma delta epsilon zeta eta theta iota";
        let mut transcript = Transcript::default();
        transcript.append_settled_blocks([finished_subagent_card(
            "child1",
            TaskState::Failed,
            "1 step · 00:01",
            prompt,
            Some(diagnostic),
        )]);

        for width in 1..=20 {
            let buffer = rendered_buffer(&mut transcript, width);
            assert_eq!(buffer.area.width, width);
        }
        assert_eq!(
            copied_text(&mut transcript, 20),
            "✗ subagent:child1 failed · 1 step · 00:01\nCafé 日本 🦀 e\u{301} ne…\nfailed \\x1B日本 🦀\ne\u{301} alpha beta gamma\ndelta epsilon zeta\n… Ctrl-O to expand"
        );

        assert!(transcript.toggle_arena_expansion());
        let expanded = copied_text(&mut transcript, 20);
        assert!(expanded.contains("Café 日本 🦀 e\u{301}"), "{expanded}");
        assert!(expanded.contains("next\\x07"), "{expanded}");
        assert!(expanded.contains("iota"), "{expanded}");
        assert!(!expanded.contains('\u{1b}'), "{expanded}");
        assert!(!expanded.contains("Ctrl-O to expand"), "{expanded}");
    }

    #[test]
    fn subagent_card_previews_bound_text_without_splitting_utf8() {
        let prompt = format!("{}🦀", "a".repeat(64 * 1024 - 1));
        let message = format!("{}🦀", "b".repeat(4 * 1024 - 1));
        let NormalisedBlock::SubagentFinished(finished) = finished_subagent_card(
            "child1",
            TaskState::Completed,
            "1 step · 00:01",
            &prompt,
            Some(&message),
        )
        .normalise() else {
            panic!("finished card normalises to a finished card");
        };

        let expected_prompt = "a".repeat(64 * 1024 - 1);
        let expected_message = "b".repeat(4 * 1024 - 1);
        assert_eq!(finished.prompt, expected_prompt);
        assert_eq!(finished.message.as_deref(), Some(expected_message.as_str()));
    }

    #[test]
    fn subagent_cards_are_immutable_cached_and_not_duplicated() {
        let child = TaskRecord::for_test(
            "c1",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let mut transcript = Transcript::default();
        assert!(!transcript.apply(
            RuntimeEvent::SubagentSpawned {
                record: child.clone(),
            },
            false,
        ));
        transcript.layout(&DARK, 20);
        let spawned = transcript.construction_counts();
        transcript.layout(&DARK, 20);
        assert_eq!(transcript.construction_counts(), spawned);

        assert!(!transcript.apply(
            RuntimeEvent::SubagentStatus {
                id: child.id().clone(),
                revision: 2,
                observation: child.observation().clone(),
            },
            false,
        ));
        transcript.layout(&DARK, 20);
        assert_eq!(transcript.construction_counts(), spawned);

        assert!(!transcript.apply(
            RuntimeEvent::SubagentSpawned {
                record: child.clone(),
            },
            false,
        ));
        assert!(transcript.settled_blocks.is_empty());
        assert!(transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: child.clone(),
            },
            false
        ));
        transcript.layout(&DARK, 20);
        let finished = transcript.construction_counts();
        assert_eq!(finished.rasterisations, spawned.rasterisations + 1);
        assert_eq!(
            finished.render_block_builds,
            spawned.render_block_builds + 1
        );

        assert!(!transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: child.clone()
            },
            false
        ));
        assert!(!transcript.apply(
            RuntimeEvent::SubagentStatus {
                id: child.id().clone(),
                revision: 2,
                observation: child.observation().clone(),
            },
            false,
        ));
        transcript.layout(&DARK, 20);
        assert_eq!(transcript.construction_counts(), finished);
        assert_eq!(transcript.settled_blocks.len(), 1);

        assert!(transcript.toggle_arena_expansion());
        transcript.layout(&DARK, 20);
        let expanded = transcript.construction_counts();
        assert_eq!(expanded.rasterisations, finished.rasterisations + 1);
        assert_eq!(
            expanded.render_block_builds,
            finished.render_block_builds + 1
        );
    }

    #[test]
    fn subagent_cards_keep_root_and_descendant_publication_order() {
        let mut transcript = Transcript::default();

        assert!(transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "root before".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        ));
        assert!(transcript.apply(
            completed_execution(1, 1, "root_between()", "", Vec::new()),
            false,
        ));
        let child = TaskRecord::for_test(
            "child1",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            Some("parent"),
            2,
            Vec::new(),
        );
        let parent = TaskRecord::for_test(
            "parent",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        assert!(transcript.apply(RuntimeEvent::SubagentFinished { record: child }, false));
        assert!(transcript.apply(RuntimeEvent::SubagentFinished { record: parent }, false));

        let copied = copied_text(&mut transcript, 120);
        let positions = [
            "root before",
            "root_between()",
            "✓ subagent:child1 · ",
            "✓ subagent:parent · ",
        ]
        .map(|text| copied.find(text).expect(text));
        assert!(
            positions.windows(2).all(|pair| pair[0] < pair[1]),
            "{copied}"
        );
        assert!(
            copied.contains(
                "✓ subagent:child1 · openai/gpt-5.6-sol · 0 steps · 00:00 · parent subagent:parent"
            ),
            "{copied}"
        );
    }

    #[test]
    fn finished_subagent_cards_preserve_lower_bound_and_unavailable_costs() {
        let mut transcript = Transcript::default();
        transcript.append_settled_blocks([
            finished_subagent_card(
                "lower1",
                TaskState::Failed,
                "1 step · 00:07 · ≥$0.00002",
                "review",
                Some("bad\x1b diagnostic"),
            ),
            finished_subagent_card(
                "none01",
                TaskState::Cancelled,
                "0 steps · 00:09",
                "review",
                None,
            ),
            finished_subagent_card(
                "late01",
                TaskState::TimedOut,
                "1 step · 15:00",
                "review",
                None,
            ),
        ]);

        let copied = copied_text(&mut transcript, 80);
        assert!(
            copied.contains(
                "✗ subagent:lower1 failed · 1 step · 00:07 · ≥$0.00002\nreview\nbad\\x1B diagnostic"
            ),
            "{copied}"
        );
        assert!(
            copied.contains("⊘ subagent:none01 cancelled · 0 steps · 00:09\nreview"),
            "{copied}"
        );
        assert!(
            copied.contains("✗ subagent:late01 timed out · 1 step · 15:00\nreview"),
            "{copied}"
        );
        assert!(!copied.contains("Ctrl-O to expand"), "{copied}");
        assert!(!copied.contains("unavailable"), "{copied}");
        assert!(!copied.contains("cancellation requested"), "{copied}");
    }

    #[test]
    fn command_and_subagent_tool_labels_are_concise() {
        for (summary, expected) in [
            (ToolCallSummary::Pending, "run · pending"),
            (ToolCallSummary::Exit(7), "run · exit 7"),
            (ToolCallSummary::Signal, "run · signal"),
            (ToolCallSummary::Failed, "run · failed"),
        ] {
            assert_eq!(
                tool_call_label(&ToolCall {
                    subject: None,
                    duration: None,
                    tool: ToolName::Run,
                    summary,
                }),
                expected
            );
        }
        for (tool, expected) in [
            (ToolName::StartSubagent, "Spawned · subagent:child1"),
            (ToolName::GetSubagent, "Refreshed · subagent:child1"),
            (ToolName::RefreshSubagent, "Refreshed · subagent:child1"),
            (ToolName::WaitSubagent, "Waiting · subagent:child1"),
            (ToolName::CancelSubagent, "Cancelled · subagent:child1"),
        ] {
            assert_eq!(
                tool_call_label(&ToolCall {
                    subject: None,
                    duration: None,
                    tool,
                    summary: ToolCallSummary::Subagent("child1".to_owned()),
                }),
                expected
            );
        }
    }

    #[test]
    fn markdown_compatibility() {
        let heading = markdown("# Heading", &DARK, None);
        assert_eq!(heading.to_string(), "# Heading");
        assert_eq!(
            heading.lines[0].style,
            Style::new().fg(DARK.md_heading).bold()
        );

        let table = markdown(
            "| Name | State |\n| --- | --- |\n| Kiki | ready |",
            &DARK,
            None,
        )
        .to_string();
        assert!(table.contains("┌"));
        assert!(table.contains("Name"));
        assert!(table.contains("ready"));

        let tasks = markdown("- [ ] pending\n- [x] complete", &DARK, None).to_string();
        assert!(tasks.contains("- [ ] pending"));
        assert!(tasks.contains("- [x] complete"));

        assert_eq!(
            markdown("[guide](https://example.com)", &DARK, None).to_string(),
            "guide (https://example.com)"
        );
        assert_eq!(markdown("> quoted", &DARK, None).to_string(), "> quoted");
        let strikethrough = markdown("~~obsolete~~", &DARK, None);
        assert_eq!(strikethrough.to_string(), "obsolete");
        assert!(
            strikethrough.lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );

        let fenced = markdown("```rust\nlet value = 1;\n```", &DARK, None);
        assert_eq!(fenced.to_string(), "```rust\nlet value = 1;\n```");

        let html = markdown(
            "inline <em>HTML</em>\n\n<div>\nblock HTML\n</div>",
            &DARK,
            None,
        )
        .to_string();
        for literal in ["<em>", "</em>", "<div>", "block HTML", "</div>"] {
            assert!(html.contains(literal));
        }

        let images = markdown("![diagram](diagram.png)\n\n![](empty.png)", &DARK, None).to_string();
        assert!(images.contains("[img] diagram"));
        assert!(images.contains("[img] empty.png"));

        let styled_link = markdown("[guide](https://example.com)", &DARK, None);
        let link = styled_link.lines[0]
            .spans
            .iter()
            .find(|span| span.content == "guide")
            .expect("link label span");
        assert_eq!(link.style, Style::new().fg(DARK.md_link).underlined());
        assert!(link.style.add_modifier.contains(Modifier::UNDERLINED));

        assert_eq!(
            markdown("unfinished **emphasis", &DARK, None).to_string(),
            "unfinished **emphasis"
        );
        assert!(
            markdown("```rust\nlet value = 1;", &DARK, None)
                .to_string()
                .contains("let value = 1;")
        );
        assert!(
            !markdown("| header |\n| --- |", &DARK, None)
                .lines
                .is_empty()
        );

        let source = "# Café 日本 🦀\n\n- [x] **résumé**\n\n[link](https://example.com/🦀)\n\n```text\nolé 世界\n```";
        let complete = markdown(source, &DARK, None).to_string();
        let mut final_prefix = None;
        for end in 0..=source.len() {
            if source.is_char_boundary(end) {
                final_prefix = Some(markdown(&source[..end], &DARK, None).to_string());
            }
        }
        assert_eq!(final_prefix, Some(complete));

        let paragraph = Paragraph::new(markdown(
            "café 日本 🦀 wraps across terminal rows",
            &DARK,
            None,
        ))
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

    fn markdown_style_at(text: &ratatui::text::Text<'_>, needle: &str) -> Style {
        for line in &text.lines {
            let content = line.to_string();
            if let Some(offset) = content.find(needle) {
                let mut start = 0;
                for span in &line.spans {
                    let end = start + span.content.len();
                    if start <= offset && offset < end {
                        return text.style.patch(line.style).patch(span.style);
                    }
                    start = end;
                }
            }
        }
        panic!("missing {needle:?} in {text:?}");
    }

    #[test]
    fn markdown_element_styles_use_pi_tokens() {
        for level in 1..=6 {
            let source = format!("{} Heading", "#".repeat(level));
            let text = markdown(&source, &DARK, None);
            let style = markdown_style_at(&text, "Heading");
            assert_eq!(style.fg, Some(DARK.md_heading));
            assert!(style.add_modifier.contains(Modifier::BOLD));
        }
        for (source, needle, color) in [
            ("> quoted", "quoted", DARK.md_quote),
            ("> [!NOTE]\n> note body", "note body", DARK.md_link),
            ("> [!CAUTION]\n> caution body", "caution body", DARK.error),
        ] {
            assert_eq!(
                markdown_style_at(&markdown(source, &DARK, None), needle).fg,
                Some(color)
            );
        }
        let table = markdown(
            "| Name | State |\n| --- | --- |\n| Kiki | ready |",
            &DARK,
            None,
        );
        let header = markdown_style_at(&table, "Name");
        assert_eq!(header.fg, Some(DARK.text));
        assert!(header.add_modifier.contains(Modifier::BOLD));
        for line in &table.lines {
            for span in &line.spans {
                if span.content.chars().any(|ch| "┌┐└┘├┤┬┴┼─│".contains(ch)) {
                    assert_eq!(line.style.patch(span.style).fg, Some(DARK.border_muted));
                }
            }
        }
    }

    #[test]
    fn markdown_inline_code_has_no_background() {
        let text = markdown("an `inline` value", &DARK, None);
        let style = markdown_style_at(&text, "inline");
        assert_eq!(style.fg, Some(DARK.md_code));
        assert_eq!(style.bg, None);
    }

    #[test]
    fn markdown_rust_fence_uses_syntax_tokens_without_background() {
        let text = markdown("```rust\nlet value = 1;\n```", &DARK, None);
        assert_eq!(
            markdown_style_at(&text, "let").fg,
            Some(DARK.syntax_keyword)
        );
        assert_eq!(markdown_style_at(&text, "1").fg, Some(DARK.syntax_number));
        for line in &text.lines {
            assert_eq!(line.style.bg, None);
            for span in &line.spans {
                assert_eq!(span.style.bg, None);
            }
        }
    }

    #[test]
    fn markdown_fence_scope_resolution_uses_syntax_tokens() {
        // DARK shares comment/operator/punctuation and type/fallback colours.
        // Distinct colours ensure these assertions exercise scope resolution.
        static SCOPE_THEME: crate::theme::Theme = crate::theme::Theme {
            md_code: Color::Rgb(1, 2, 3),
            syntax_comment: Color::Rgb(4, 5, 6),
            syntax_keyword: Color::Rgb(7, 8, 9),
            syntax_function: Color::Rgb(10, 11, 12),
            syntax_variable: Color::Rgb(13, 14, 15),
            syntax_string: Color::Rgb(16, 17, 18),
            syntax_number: Color::Rgb(19, 20, 21),
            syntax_type: Color::Rgb(22, 23, 24),
            syntax_operator: Color::Rgb(25, 26, 27),
            syntax_punctuation: Color::Rgb(28, 29, 30),
            ..DARK
        };
        let source = concat!(
            "```rust\n",
            "fn add(left: i32, right: String) -> Option<u8> { let x = left + 1; // note\n",
            " \"text\" }\n",
            "```\n\n",
            "```rust\nstruct Point;\n```\n\n",
            "```python\ndef f(x: int):\n    return \"s\"  # c\n```",
        );
        for theme in [&DARK, &SCOPE_THEME] {
            let options = super::build_markdown_options(theme);
            let text = tui_markdown::from_str_with_options(source, &options);
            for (token, color) in [
                ("note", theme.syntax_comment),
                (" c", theme.syntax_comment),
                ("fn", theme.syntax_keyword),
                ("let", theme.syntax_keyword),
                ("struct", theme.syntax_keyword),
                ("return", theme.syntax_keyword),
                ("add", theme.syntax_function),
                ("f(", theme.syntax_function),
                ("left:", theme.syntax_variable),
                ("right:", theme.syntax_variable),
                ("x:", theme.syntax_variable),
                ("text", theme.syntax_string),
                ("s\"", theme.syntax_string),
                ("1", theme.syntax_number),
                ("int)", theme.syntax_type),
                ("def", theme.syntax_keyword),
                ("=", theme.syntax_operator),
                ("+", theme.syntax_operator),
                ("(", theme.syntax_punctuation),
                (":", theme.syntax_punctuation),
                ("->", theme.syntax_punctuation),
                ("<", theme.syntax_punctuation),
                (">", theme.syntax_punctuation),
                ("{", theme.syntax_punctuation),
                ("}", theme.syntax_punctuation),
                (";", theme.syntax_punctuation),
                ("//", theme.syntax_punctuation),
                ("#", theme.syntax_punctuation),
                ("\"", theme.syntax_punctuation),
                // Syntect gives primitives and `let` the same storage.type.rust scope.
                ("i32", theme.syntax_keyword),
                ("u8", theme.syntax_keyword),
                // These names (and the parameter comma) have no mapped type scope.
                ("String", theme.md_code),
                ("Option", theme.md_code),
                ("Point", theme.md_code),
                (",", theme.md_code),
                ("x =", theme.md_code),
                ("left +", theme.md_code),
            ] {
                assert_eq!(
                    markdown_style_at(&text, token).fg,
                    Some(color),
                    "wrong foreground for {token:?}"
                );
            }
        }
    }

    #[test]
    fn markdown_unrecognised_code_inherits_card_colors() {
        for source in [
            "```nosuchlang\nunknown code\n```",
            "```\nunknown code\n```",
            "    unknown code",
        ] {
            let text = markdown(source, &DARK, None);
            let style = markdown_style_at(&text, "unknown code");
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);

            let card = super::CompactionCard {
                heading: "Compaction".to_owned(),
                collapsed: String::new(),
                expanded: super::CompactionContent::Markdown(source.to_owned()),
                accounting: None,
                state: super::ArenaState::Complete,
            };
            let block = super::build_render_block(
                &DARK,
                &NormalisedBlock::Compaction(card),
                true,
                true,
                40,
                &mut PythonHighlighter::new(),
                None,
            );
            let RenderBlock::Card(card) = block else {
                panic!("compaction card");
            };
            let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 10));
            card.render(buffer.area, 0, 0, &mut buffer);
            let row = row_containing(&buffer, "unknown code") as u16;
            for column in 0..buffer.area.width {
                let cell = &buffer[(column, row)];
                assert_eq!(cell.fg, DARK.text);
                assert_eq!(cell.bg, DARK.tool_success_bg);
            }
        }
    }

    #[test]
    #[ignore]
    fn markdown_highlight_cost() {
        let mut source = String::from("```rust\n");
        for number in 0..400 {
            source.push_str(&format!("let value_{number} = {number};\n"));
        }
        source.push_str("```");
        std::hint::black_box(markdown(&source, &DARK, None));
        let runs = 5;
        let start = std::time::Instant::now();
        for _ in 0..runs {
            std::hint::black_box(markdown(&source, &DARK, None));
        }
        eprintln!(
            "markdown_highlight_cost: {:.3} ms/render",
            start.elapsed().as_secs_f64() * 1000.0 / f64::from(runs)
        );
    }

    #[test]
    fn markdown_generated_theme_parses_and_options_are_reused() {
        for theme in [&DARK, &crate::theme::LIGHT] {
            assert!(tui_markdown::CodeTheme::from_textmate(&super::textmate_theme(theme)).is_ok());
            assert!(std::ptr::eq(
                super::markdown_options(theme),
                super::markdown_options(theme)
            ));
            let text = markdown("| Name |\n| --- |", theme, None);
            assert_eq!(markdown_style_at(&text, "Name").fg, Some(theme.text));
        }
    }

    #[test]
    fn markdown_secondary_styles_keep_modifiers_without_ansi_colors() {
        use tui_markdown::{AlertKind, StyleSheet};

        let styles = super::MarkdownStyleSheet { theme: &DARK };
        for (actual, expected) in [
            (styles.heading_meta(), Style::new().fg(DARK.dim)),
            (styles.html(), Style::new().fg(DARK.dim)),
            (styles.footnote_ref(), Style::new().fg(DARK.dim).italic()),
            (styles.footnote_def(), Style::new().fg(DARK.dim)),
            (styles.image_alt(), Style::new().fg(DARK.dim).italic()),
            (styles.metadata_block(), Style::new().fg(DARK.muted)),
            (styles.math_inline(), Style::new().fg(DARK.md_code).italic()),
            (styles.math_display(), Style::new().fg(DARK.md_code)),
            (styles.alert(AlertKind::Tip), Style::new().fg(DARK.success)),
            (
                styles.alert(AlertKind::Important),
                Style::new().fg(DARK.accent),
            ),
            (
                styles.alert(AlertKind::Warning),
                Style::new().fg(DARK.warning),
            ),
        ] {
            assert_eq!(actual, expected);
        }
    }

    fn rendered_text(transcript: &Transcript) -> String {
        transcript
            .blocks()
            .into_iter()
            .map(TranscriptBlock::normalise)
            .map(|block| match &block {
                NormalisedBlock::Reasoning { markdown: source } => {
                    markdown(source, &DARK, None).to_string()
                }
                NormalisedBlock::Assistant {
                    markdown: source,
                    accounting,
                } => assistant_text(&DARK, source, accounting.clone(), None).to_string(),
                block => block.plain_text(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_layout_text(transcript: &mut Transcript) -> String {
        let layout = transcript.layout(&DARK, 1024);
        let height = u16::try_from(layout.total_height()).expect("test transcript height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1024, height));
        layout.render(buffer.area, 0, &mut buffer);
        buffer
            .content
            .chunks(1024)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_buffer(transcript: &mut Transcript, width: u16) -> Buffer {
        let layout = transcript.layout(&DARK, width);
        let height = u16::try_from(layout.total_height()).expect("test transcript height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        layout.render(buffer.area, 0, &mut buffer);
        buffer
    }

    fn copied_text(transcript: &mut Transcript, width: u16) -> String {
        let layout = transcript.layout(&DARK, width);
        layout.extract_selection(Selection::new(
            DocumentPosition::new(0, 0),
            DocumentPosition::new(layout.total_height(), 0),
        ))
    }

    fn footer_text(layout: &TranscriptLayout) -> Option<String> {
        layout.footer.as_ref().map(|footer| {
            footer
                .spans(&DARK)
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
    }

    fn render_footer_rule(footer: Footer, width: u16) -> Line<'static> {
        TranscriptLayout {
            page_style: page_style(&DARK),
            blocks: Vec::new(),
            width,
            total_height: 0,
            footer: Some(footer),
        }
        .footer_rule(&DARK, width)
    }

    fn layout_buffer(
        layout: &TranscriptLayout,
        viewport: Rect,
        top: usize,
        selection: Option<Selection>,
    ) -> Buffer {
        let mut buffer = Buffer::empty(viewport);
        if let Some(selection) = selection {
            layout.render_with_selection(viewport, top, Some(selection), &mut buffer);
        } else {
            layout.render(viewport, top, &mut buffer);
        }
        buffer
    }

    fn assert_cached_layout_matches_fresh(transcript: &mut Transcript, width: u16) {
        let cached = transcript.layout(&DARK, width);
        let fresh = transcript.layout_fresh(&DARK, width);
        assert_eq!(cached.total_height(), fresh.total_height());
        assert_eq!(footer_text(&cached), footer_text(&fresh));
        assert_eq!(
            cached.footer_rule(&DARK, width),
            fresh.footer_rule(&DARK, width)
        );

        let selection = Selection::new(
            DocumentPosition::new(0, 0),
            DocumentPosition::new(cached.total_height(), 0),
        );
        let height = u16::try_from(cached.total_height()).expect("test transcript height");
        for (top, viewport_height) in [(0, height), (1, height.min(6))] {
            let viewport = Rect::new(3, 2, width, viewport_height);
            assert_eq!(
                layout_buffer(&cached, viewport, top, None),
                layout_buffer(&fresh, viewport, top, None)
            );
            assert_eq!(
                layout_buffer(&cached, viewport, top, Some(selection)),
                layout_buffer(&fresh, viewport, top, Some(selection))
            );
        }
        assert_eq!(
            cached.extract_selection(selection),
            fresh.extract_selection(selection)
        );
    }

    fn card(block: &LayoutBlock) -> Option<&CardRenderBlock> {
        let LayoutBlock::Fresh(RenderBlock::Card(card)) = block else {
            return None;
        };
        Some(card)
    }

    fn card_with_text<'a>(blocks: &'a [LayoutBlock], text: &str) -> &'a CardRenderBlock {
        blocks
            .iter()
            .filter_map(card)
            .find(|card| card.text.to_string().contains(text))
            .expect("card block")
    }

    fn row_text(buffer: &Buffer, row: usize) -> String {
        buffer.content[row * buffer.area.width as usize..(row + 1) * buffer.area.width as usize]
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn row_containing(buffer: &Buffer, text: &str) -> usize {
        (0..buffer.area.height as usize)
            .find(|row| row_text(buffer, *row).contains(text))
            .expect("rendered row")
    }

    fn assert_row_background(buffer: &Buffer, row: usize, background: Color) {
        for cell in &buffer.content
            [row * buffer.area.width as usize..(row + 1) * buffer.area.width as usize]
        {
            assert_eq!(cell.bg, background);
        }
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
        detailed_usage_for(
            SOL.id,
            input,
            output,
            cache_read,
            cache_write,
            reasoning,
            total,
        )
    }

    fn detailed_usage_for(
        model: &str,
        input: Option<u64>,
        output: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
        reasoning: Option<u64>,
        total: Option<u64>,
    ) -> AttemptAccounting {
        AttemptAccounting::KnownUsage(ModelUsage {
            model: model.to_owned(),
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
                    commentary: None,
                    source: source.to_owned(),
                    reasoning_summary: reasoning_summary.to_owned(),
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

    fn completed_execution_with_tool_calls(
        run_id: u64,
        step_id: u64,
        source: &str,
        stdout: &str,
        tool_calls: Vec<ToolCall>,
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
                        stdout: observation(stdout),
                        stderr: observation(""),
                        outcome: ExecutionOutcome::Complete {
                            result: Some(observation("result")),
                        },
                        tool_calls,
                        read_paths: Vec::new(),
                        modified_paths: Vec::new(),
                        changed_bindings: Vec::new(),
                    },
                    attempts: Vec::new(),
                },
            },
            progress_gap: false,
        }
    }

    fn start_live_run(transcript: &mut Transcript) {
        assert!(transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false));
        assert!(transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        ));
    }

    fn apply_run_tool_update(
        transcript: &mut Transcript,
        token: ToolCallToken,
        summary: ToolCallSummary,
    ) -> bool {
        transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token,
                call: ToolCall {
                    subject: None,
                    duration: None,
                    tool: ToolName::Run,
                    summary,
                },
            },
            false,
        )
    }

    fn apply_run_preview(transcript: &mut Transcript, token: ToolCallToken, stdout: &str) -> bool {
        transcript.apply(
            RuntimeEvent::RunPreviewUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token,
                preview: RunPreview {
                    stdout: stdout.to_owned(),
                },
            },
            false,
        )
    }

    fn assert_live_run_rows(
        transcript: &Transcript,
        tool_calls: &[ToolCall],
        run_previews: &[Option<RunPreview>],
    ) {
        let live = transcript
            .live_step
            .as_ref()
            .expect("run updates create a live step");
        assert_eq!(live.tool_calls, tool_calls);
        assert_eq!(live.run_previews, run_previews);
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
                        overflow_recovery: None,
                    },
                },
            },
            progress_gap,
        }
    }

    fn finished_subagent_card(
        id: &str,
        state: TaskState,
        summary: &str,
        prompt: &str,
        message: Option<&str>,
    ) -> TranscriptBlock {
        TranscriptBlock::SubagentFinished(SubagentFinishedCard {
            id: id.to_owned(),
            state,
            summary: summary.to_owned(),
            prompt: prompt.to_owned(),
            message: message.map(str::to_owned),
        })
    }

    fn cached_transcript() -> Transcript {
        let source = (1..=34)
            .map(|line| format!("print('line {line:02}: 日本 🦀 e\u{301}')"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "cached request: Café 日本 🦀 e\u{301}".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(
                1,
                1,
                &source,
                "## Cached reasoning 日本 🦀 e\u{301}\n\n- [x] **Markdown**\n\n[guide](https://example.com/🦀)",
                Vec::new(),
            ),
            false,
        );
        transcript.apply(
            final_message(
                1,
                2,
                "# Cached answer\n\n| Word | State |\n| --- | --- |\n| Café 日本 🦀 e\u{301} | **ready** |",
                vec![usage(1, 1)],
                false,
            ),
            false,
        );
        transcript
    }

    #[test]
    fn settled_layout_reuses_all_construction_work_at_an_unchanged_width() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "def cached():\n    return 42", "# Cached", Vec::new()),
            false,
        );

        transcript.layout(&DARK, 40);
        let first = transcript.construction_counts();
        assert!(first.rasterisations > 0);
        assert!(first.markdown > 0);
        assert!(first.highlighting > 0);
        assert!(first.render_block_builds > 0);
        assert!(first.selection_region_builds > 0);

        transcript.layout(&DARK, 40);

        assert_eq!(transcript.construction_counts(), first);
    }

    #[test]
    fn cached_layout_matches_fresh_across_rendering_states() {
        let mut transcript = cached_transcript();
        for width in [1, 2, 3, 4, 80] {
            assert_cached_layout_matches_fresh(&mut transcript, width);
        }

        assert!(transcript.toggle_reasoning());
        assert_cached_layout_matches_fresh(&mut transcript, 80);
        assert!(transcript.toggle_arena_expansion());
        assert_cached_layout_matches_fresh(&mut transcript, 80);
        assert!(transcript.toggle_reasoning());
        assert_cached_layout_matches_fresh(&mut transcript, 80);

        assert!(transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false));
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::ReasoningSummaryChunk {
                    text: "## Live reasoning 日本 🦀 e\u{301}".to_owned(),
                },
            },
            false,
        ));
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "**Live Markdown** 日本 🦀 e\u{301}".to_owned(),
                },
            },
            false,
        ));
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: usage(2, 3),
                },
            },
            false,
        ));
        assert_cached_layout_matches_fresh(&mut transcript, 80);

        assert!(transcript.apply(
            final_message(
                2,
                1,
                "## Settled Markdown 日本 🦀 e\u{301}",
                vec![usage(2, 3)],
                false,
            ),
            false,
        ));
        assert_cached_layout_matches_fresh(&mut transcript, 80);
    }

    #[test]
    #[should_panic(expected = "cached raster width must match viewport width")]
    fn cached_raster_rejects_a_different_viewport_width() {
        let mut transcript = cached_transcript();
        let layout = transcript.layout(&DARK, 24);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 25, 1));

        layout.render(buffer.area, 0, &mut buffer);
    }

    #[test]
    fn settled_cache_appends_and_invalidates_only_affected_rasters() {
        let mut transcript = cached_transcript();
        transcript.layout(&DARK, 24);
        let initial = transcript.construction_counts();
        assert_eq!(initial.normalisations, 4);
        assert_eq!(initial.rasterisations, 4);
        assert_eq!(initial.markdown, 2);

        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::UserRequest {
                        request: "new request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.layout(&DARK, 24);
        let appended = transcript.construction_counts();
        assert_eq!(appended.normalisations, initial.normalisations + 1);
        assert_eq!(appended.rasterisations, initial.rasterisations + 1);
        assert_eq!(appended.markdown, initial.markdown);
        assert_eq!(appended.highlighting, initial.highlighting);
        assert_eq!(
            appended.render_block_builds,
            initial.render_block_builds + 1
        );
        assert_eq!(
            appended.selection_region_builds,
            initial.selection_region_builds + 1
        );

        assert!(transcript.toggle_reasoning());
        transcript.layout(&DARK, 24);
        let reasoning = transcript.construction_counts();
        assert_eq!(reasoning.rasterisations, appended.rasterisations + 1);
        assert_eq!(reasoning.markdown, appended.markdown);
        assert_eq!(reasoning.highlighting, appended.highlighting);

        assert!(transcript.toggle_arena_expansion());
        transcript.layout(&DARK, 24);
        let arena = transcript.construction_counts();
        assert_eq!(arena.rasterisations, reasoning.rasterisations + 1);
        assert_eq!(arena.markdown, reasoning.markdown);
        assert_eq!(arena.highlighting, reasoning.highlighting + 1);

        transcript.layout(&DARK, 48);
        let width = transcript.construction_counts();
        let block_count = transcript.settled_blocks.len();
        assert_eq!(width.rasterisations, arena.rasterisations + block_count);
        assert_eq!(width.markdown, arena.markdown + 1);
        assert_eq!(width.highlighting, arena.highlighting + 1);
        assert_eq!(
            width.render_block_builds,
            arena.render_block_builds + block_count
        );
        assert_eq!(
            width.selection_region_builds,
            arena.selection_region_builds + block_count
        );
        assert!(transcript.toggle_reasoning());
        transcript.layout(&DARK, 48);
        let shown_reasoning = transcript.construction_counts();
        assert_eq!(shown_reasoning.markdown, width.markdown + 1);
        assert!(transcript.settled_blocks.iter().all(|block| {
            block
                .raster
                .as_ref()
                .is_some_and(|raster| raster.buffer.area.width == 48)
        }));

        for width in [17, 48, 17, 48] {
            transcript.layout(&DARK, width);
            assert!(transcript.settled_blocks.iter().all(|block| {
                block
                    .raster
                    .as_ref()
                    .is_some_and(|raster| raster.buffer.area.width == width)
            }));
        }

        let before_light = transcript.construction_counts();
        let light_layout = transcript.layout(&LIGHT, 48);
        let light = transcript.construction_counts();
        assert_eq!(
            light.rasterisations,
            before_light.rasterisations + block_count
        );
        assert_eq!(light.markdown, before_light.markdown + 2);
        assert_eq!(light.highlighting, before_light.highlighting + 1);
        assert_eq!(
            light.render_block_builds,
            before_light.render_block_builds + block_count
        );
        assert_eq!(
            light.selection_region_builds,
            before_light.selection_region_builds + block_count
        );
        let height = u16::try_from(light_layout.total_height()).expect("test transcript height");
        let light_buffer = layout_buffer(&light_layout, Rect::new(0, 0, 48, height), 0, None);
        assert_row_background(
            &light_buffer,
            row_containing(&light_buffer, "cached request"),
            LIGHT.user_message_bg,
        );

        transcript.layout(&LIGHT, 48);
        assert_eq!(transcript.construction_counts(), light);
    }

    #[test]
    fn live_progress_and_settlement_preserve_settled_rasters_and_footer() {
        let mut transcript = cached_transcript();
        let initial_layout = transcript.layout(&DARK, 24);
        let initial_footer = footer_text(&initial_layout);
        let initial_counts = transcript.construction_counts();

        assert!(transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false));
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: usage(3, 2),
                },
            },
            false,
        ));
        let live_layout = transcript.layout(&DARK, 24);
        assert_eq!(transcript.construction_counts(), initial_counts);
        assert_ne!(footer_text(&live_layout), initial_footer);
        assert!(
            footer_text(&live_layout)
                .expect("live footer")
                .contains("↑4 ↓3 R0 W0")
        );

        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "live assistant".to_owned(),
                },
            },
            false,
        ));
        transcript.layout(&DARK, 24);
        assert_eq!(transcript.construction_counts(), initial_counts);

        assert!(transcript.apply(
            completed_execution(2, 1, "settled()", "", vec![usage(3, 2)]),
            false,
        ));
        assert!(transcript.live_step.is_none());
        let settled_layout = transcript.layout(&DARK, 24);
        let settled_counts = transcript.construction_counts();
        assert_eq!(
            settled_counts.rasterisations,
            initial_counts.rasterisations + 1
        );
        assert_eq!(settled_counts.highlighting, initial_counts.highlighting + 1);
        assert_eq!(footer_text(&settled_layout), footer_text(&live_layout));
        let area = Rect::new(
            0,
            0,
            24,
            u16::try_from(settled_layout.total_height()).expect("test transcript height"),
        );
        let text = layout_buffer(&settled_layout, area, 0, None)
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("settled()"));
        assert!(!text.contains("live assistant"));

        assert!(!transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale".to_owned(),
                },
            },
            false,
        ));
        transcript.layout(&DARK, 24);
        assert_eq!(transcript.construction_counts(), settled_counts);
    }

    #[test]
    fn over_u16_settled_blocks_keep_the_uncached_renderer() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "x".repeat(usize::from(u16::MAX) + 1),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let cached = transcript.layout(&DARK, 1);
        let first = transcript.construction_counts();
        let fresh = transcript.layout_fresh(&DARK, 1);
        assert_eq!(transcript.construction_counts(), first);
        assert!(cached.total_height() > usize::from(u16::MAX));
        assert_eq!(first.rasterisations, 0);
        assert_eq!(first.selection_region_builds, 0);
        assert!(transcript.settled_blocks[0].raster.is_none());
        let viewport = Rect::new(0, 0, 1, 1);
        assert_eq!(
            layout_buffer(&cached, viewport, 1, None),
            layout_buffer(&fresh, viewport, 1, None)
        );

        transcript.layout(&DARK, 1);
        let rebuilt = transcript.construction_counts();
        assert_eq!(rebuilt.rasterisations, 0);
        assert_eq!(rebuilt.selection_region_builds, 0);
        assert_eq!(rebuilt.render_block_builds, first.render_block_builds + 1);
    }

    #[test]
    fn fresh_layout_bypass_retains_settled_rasters() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "def cached():\n    return 42", "# Cached", Vec::new()),
            false,
        );

        transcript.layout(&DARK, 40);
        let first = transcript.construction_counts();
        transcript.layout_fresh(&DARK, 40);
        assert_eq!(transcript.construction_counts(), first);

        transcript.layout(&DARK, 40);

        assert_eq!(transcript.construction_counts(), first);
    }

    #[test]
    fn selection_extracts_wrapped_markdown_without_a_soft_newline() {
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(1, 1, "[Kiki guide](https://example.com)", Vec::new(), false),
            false,
        );

        let layout = transcript.layout(&DARK, 8);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "Kiki guide (https://example.com)"
        );
    }

    #[test]
    fn selection_extracts_arena_content_without_padding_rows() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "    answer()", "", Vec::new()),
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "    answer()\nauthoritative stdout\nresult\n42"
        );
    }

    #[test]
    fn selection_omits_right_aligned_accounting_terminal_fill() {
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(1, 1, "answer", vec![usage(1, 1)], false),
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "answer\n\n↑1 ↓1 R0 W0 r0 Σ2 · $0.000024"
        );
    }

    #[test]
    fn selection_extracts_standalone_live_accounting() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: usage(1, 1),
                },
            },
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "↑1 ↓1 R0 W0 r0 Σ2 · $0.000024"
        );
    }

    #[test]
    fn selection_extracts_a_backwards_range_across_vertical_chunks() {
        let source = (1..=66)
            .map(|line| format!("line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: source,
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(layout.total_height(), 0),
                DocumentPosition::new(0, 0),
            )),
            "line 01
line 02
line 03
line 04
line 05
line 06
line 07
line 08
line 09
line 10
line 11
line 12
line 13
line 14
line 15
line 16
line 17
line 18
line 19
line 20
line 21
line 22
line 23
line 24
line 25
line 26
line 27
line 28
line 29
line 30
line 31
line 32
line 33
line 34
line 35
line 36
line 37
line 38
line 39
line 40
line 41
line 42
line 43
line 44
line 45
line 46
line 47
line 48
line 49
line 50
line 51
line 52
line 53
line 54
line 55
line 56
line 57
line 58
line 59
line 60
line 61
line 62
line 63
line 64
line 65
line 66"
        );
    }

    #[test]
    fn selection_keeps_wide_and_combining_graphemes_whole() {
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(1, 1, "a你🦀e\u{301}b", Vec::new(), false),
            false,
        );

        let layout = transcript.layout(&DARK, 8);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 2),
                DocumentPosition::new(0, 4),
            )),
            "你🦀"
        );
        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 5),
                DocumentPosition::new(0, 6),
            )),
            "e\u{301}"
        );
    }

    #[test]
    fn selection_preserves_unselected_layout_buffers_and_heights() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "padded request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(1, 1, "source()", "reasoning", Vec::new()),
            false,
        );

        let layout = transcript.layout(&DARK, 24);
        let height = u16::try_from(layout.total_height()).expect("test transcript height");
        let area = Rect::new(0, 0, 24, height);
        let mut rendered = Buffer::empty(area);
        let mut unselected = Buffer::empty(area);
        layout.render(area, 0, &mut rendered);
        layout.render_with_selection(area, 0, None, &mut unselected);

        assert_eq!(layout.total_height(), usize::from(height));
        assert_eq!(unselected, rendered);
    }

    #[test]
    fn selection_highlights_clipped_card_content_without_its_padding() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "padded request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let layout = transcript.layout(&DARK, 24);
        let selection = Selection::new(
            DocumentPosition::new(0, 0),
            DocumentPosition::new(layout.total_height(), 0),
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 24, 1));

        layout.render_with_selection(buffer.area, 1, Some(selection), &mut buffer);

        assert_eq!(layout.extract_selection(selection), "padded request");
        assert!(buffer[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn selection_uses_visible_reasoning_and_collapsed_arena_content() {
        let source = (1..=33)
            .map(|line| format!("code {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, &source, "hidden reasoning", Vec::new()),
            false,
        );
        assert!(transcript.toggle_reasoning());

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "thinking hidden · Ctrl-T to show

code 01
code 02
code 03
code 04
code 05
code 06
code 07
code 08
code 09
code 10
… 23 more lines · Ctrl-O to expand
authoritative stdout
result
42"
        );
    }

    #[test]
    fn selection_copies_generated_markdown_presentation() {
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(
                1,
                1,
                "[guide](https://example.com)\n\n- first\n- second\n\n| Name | State |\n| --- | --- |\n| Kiki | ready |",
                Vec::new(),
                false,
            ),
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "guide (https://example.com)

- first
- second

┌──────┬───────┐
│ Name │ State │
├──────┼───────┤
│ Kiki │ ready │
└──────┴───────┘"
        );
    }

    #[test]
    fn selection_orders_cross_card_content_with_one_blank_line() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "first card".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(final_message(1, 1, "second card", Vec::new(), false), false);

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(layout.total_height(), 0),
                DocumentPosition::new(0, 0),
            )),
            "first card\n\nsecond card"
        );
    }

    #[test]
    fn selection_copies_narrow_arena_content_and_zero_width_empty() {
        let mut transcript = Transcript::default();
        transcript.apply(completed_execution(1, 1, "XYZ", "", Vec::new()), false);

        let narrow = transcript.layout(&DARK, 8);
        assert_eq!(
            narrow.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(narrow.total_height(), 0),
            )),
            "XYZ\nauthoritative stdout\nresult\n42"
        );

        let zero = transcript.layout(&DARK, 0);
        assert_eq!(
            zero.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(zero.total_height(), 0),
            )),
            ""
        );
    }

    #[test]
    fn selection_copies_visible_startup_text() {
        let mut transcript = Transcript::configured(ready_snapshot_for_test());
        let layout = transcript.layout(&DARK, 512);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "Startup configuration
Worker: /opt/kiki/bin/monty
Workspace: /workspace/project
Model: openai/gpt-6.1-sol (context: 1.05m, max output: 128k)
Thinking: medium
Prompt: bundled system
OpenAI API key: present
Anthropic API key: missing
Google API key: missing
Submission: available
Problems: none"
        );
    }

    #[test]
    fn selection_extracts_one_very_tall_logical_line_in_chunks() {
        let source = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmn";
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: source.to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let layout = transcript.layout(&DARK, 1);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(layout.total_height(), 0),
                DocumentPosition::new(0, 0),
            )),
            "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmn"
        );
    }

    #[test]
    fn selection_copies_visible_terminal_arena_sections_in_order() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Execution(Box::new(
                                ExecutionEvidence {
                                    commentary: Some("I’ll inspect the failure first.".to_owned()),
                                    source: "failure_code".to_owned(),
                                    reasoning_summary: String::new(),
                                    began: true,
                                    stdout: observation("output body"),
                                    stderr: observation("diagnostic body"),
                                    tool_calls: vec![ToolCall {
                                        tool: ToolName::Read,
                                        summary: ToolCallSummary::Failed,
                                        subject: None,
                                        duration: None,
                                    }],
                                    read_paths: Vec::new(),
                                    modified_paths: Vec::new(),
                                    failure: Some(ExecutionFailure::Arena {
                                        message: "failure body".to_owned(),
                                    }),
                                    cancellation_warning: Some("warning body".to_owned()),
                                },
                            ))),
                            attempts: Vec::new(),
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "I’ll inspect the failure first.

cancelled
execution: started
failure_code
\u{2717} read · failed
output body
stderr
diagnostic body
failure
failure body
warning
warning body"
        );
    }

    #[test]
    fn selection_copies_visible_arena_gap_and_code() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, true);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "gap_code".to_owned(),
                },
            },
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "display gap: some live progress was dropped\ngap_code"
        );
    }

    #[test]
    fn selection_copies_error_preview_and_arena_accounting() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "error_code".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(""),
                            stderr: observation("error_stderr"),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: "error_body".to_owned(),
                                omitted_bytes: 0,
                            }),
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            changed_bindings: Vec::new(),
                        },
                        attempts: vec![usage(1, 1)],
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::StdoutTruncated {
                run_id: RunId(2),
                step_id: ModelStepId(1),
            },
            false,
        );

        let layout = transcript.layout(&DARK, 80);

        assert_eq!(
            layout.extract_selection(Selection::new(
                DocumentPosition::new(0, 0),
                DocumentPosition::new(layout.total_height(), 0),
            )),
            "error_code
stderr
error_stderr
error
error_body
↑1 ↓1 R0 W0 r0 Σ2 · $0.000024

stdout preview
truncated"
        );
    }

    #[test]
    fn projection_reliable_records_beat_lossy_progress() {
        let mut transcript = Transcript::default();
        assert!(transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false));
        assert!(transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "preview()".to_owned(),
                },
            },
            false,
        ));
        assert!(rendered_text(&transcript).contains("preview()"));

        assert!(transcript.apply(
            completed_execution(1, 1, "authoritative()", "settled reasoning", Vec::new()),
            false,
        ));

        let history = rendered_text(&transcript);
        assert!(history.contains("authoritative()"));
        assert!(history.contains("authoritative stdout"));
        assert!(!history.contains("preview()"));
        assert!(history.contains("settled reasoning"));
    }

    #[test]
    fn projection_stale_progress_is_a_visible_no_op() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(3),
                epoch: 0,
            },
            false,
        );
        let history = rendered_text(&transcript);
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
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale step".to_owned(),
                },
            },
            true,
        ));
        assert_eq!(rendered_text(&transcript), history);
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
                epoch: 0,
                progress: ModelProgress::AttemptStarted { attempt: 1 },
            },
            true,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
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
                epoch: 0,
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
                epoch: 0,
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
                epoch: 0,
                progress: ModelProgress::AttemptStarted { attempt: 2 },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "retained answer".to_owned(),
                },
            },
            false,
        );

        let history = rendered_text(&transcript);
        assert!(!history.contains("discarded reasoning"));
        assert!(!history.contains("discarded_code()"));
        assert!(history.contains("retained answer"));
        assert!(history.contains("↑100 ↓0 R0 W0 r0 Σ100 · $0.0004"));
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
                epoch: 0,
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
        assert!(rendered_text(&transcript).contains("completed summary"));
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
                epoch: 0,
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

        let history = rendered_text(&transcript);
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
                epoch: 0,
            },
            false,
        );
        assert!(rendered_text(&transcript).contains("display gap"));

        transcript.apply(completed_execution(1, 1, "done()", "", Vec::new()), false);
        assert!(!rendered_text(&transcript).contains("display gap"));

        transcript.apply(final_message(1, 2, "settled", Vec::new(), true), false);
        transcript.apply(RuntimeEvent::RunSettled { run_id: RunId(1) }, false);
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(3),
                epoch: 0,
            },
            false,
        );

        let history = rendered_text(&transcript);
        assert!(history.contains("settled\ndisplay gap"));
        assert!(history.ends_with("display gap: some live progress was dropped"));
    }

    #[test]
    fn accounting_folds_disjoint_attempts_forward_by_run() {
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

        let history = rendered_text(&transcript);
        assert!(history.contains("↑100 ↓0 R0 W0 r0 Σ100 · $0.0004"));
        assert!(history.contains("↑0 ↓100 R0 W0 r0 Σ100 · $0.002"));
        assert!(!history.contains("Run estimated cost"));
        assert!(history.contains("code:\nother()"));
    }

    #[test]
    fn footer_compacts_counts_and_costs_with_required_precision() {
        for (value, expected) in [
            (999, "999"),
            (1_000, "1k"),
            (1_050, "1.05k"),
            (11_500, "11.5k"),
            (999_499, "999k"),
            (999_500, "1m"),
            (1_230_000, "1.23m"),
        ] {
            assert_eq!(format_token_count(value), expected);
        }
        for (nanodollars, expected) in [
            (794_000_000, "$0.7940"),
            (12_350_000, "$0.01235"),
            (12_350_000_000, "$12.35"),
            (123_450_000_000, "$123.45"),
            (1_234_500_000_000, "$1234.50"),
        ] {
            assert_eq!(format_footer_usd(nanodollars), expected);
        }
    }

    #[test]
    fn footer_rule_separates_present_fields_and_reserves_empty_rule_seams() {
        fn footer(identity: bool, right: bool) -> Footer {
            Footer::new(
                identity.then(|| FooterIdentity {
                    cwd: Some("identity".to_owned()),
                    name: None,
                }),
                right
                    .then(|| FooterField::muted(&DARK, "model"))
                    .into_iter()
                    .collect(),
                None,
            )
            .expect("footer")
        }

        for (identity, right, width, expected) in [
            (true, true, 20, "─ identity ─ model ─"),
            (true, false, 14, "─ identity ─ ─"),
            (false, true, 11, "─ ─ model ─"),
            (true, true, 19, "─ identity  model ─"),
            (true, false, 13, "─ identity  ─"),
            (false, true, 10, "─  model ─"),
        ] {
            let rule = render_footer_rule(footer(identity, right), width);
            assert_eq!(rule.to_string(), expected);
            assert_eq!(rule.width(), usize::from(width));
        }
    }

    #[test]
    fn footer_rule_accounts_for_seams_during_truncation_and_field_removal() {
        let truncated = render_footer_rule(
            Footer::new(
                Some(FooterIdentity {
                    cwd: Some("abc🦀".to_owned()),
                    name: None,
                }),
                Vec::new(),
                Some(ModelFooter {
                    id: "m".to_owned(),
                    thinking: "x".to_owned(),
                }),
            )
            .expect("footer"),
            23,
        );
        assert_eq!(truncated.to_string(), "─ …🦀  m • thinking x ─");
        assert_eq!(truncated.width(), 23);

        let prioritised = render_footer_rule(
            Footer::new(
                Some(FooterIdentity {
                    cwd: Some("identity".to_owned()),
                    name: None,
                }),
                vec![
                    FooterField::muted(&DARK, "first"),
                    FooterField::muted(&DARK, "last"),
                ],
                None,
            )
            .expect("footer"),
            19,
        );
        assert_eq!(prioritised.to_string(), "─ identity ─ last ─");
        assert!(!prioritised.to_string().contains("first"));
        assert_eq!(prioritised.width(), 19);
    }

    #[test]
    fn footer_adds_identity_and_preserves_model_thinking_under_pressure() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(
                1,
                1,
                "metrics()",
                "",
                vec![detailed_usage(
                    Some(217_600),
                    Some(0),
                    Some(163_200),
                    Some(0),
                    Some(0),
                    Some(217_600),
                )],
            ),
            false,
        );
        let context = FooterContext {
            workspace_root: Some("/home/kiki/worktrees/interactive/settings/kiki".to_owned()),
            home: Some("/home/kiki".into()),
            name: Some("Refactor auth".to_owned()),
            selection: ModelSelection::new(&SOL, "high").expect("supported thinking"),
        };

        let layout = transcript.layout_with_footer(&DARK, 128, context.clone());
        assert_eq!(
            footer_text(&layout).as_deref(),
            Some(
                "~/worktrees/interactive/settings/kiki (Refactor auth) ↑218k ↓0 R163k W0 CH75.0% $0.2829 80.0%/272k  openai/gpt-5.6-sol • thinking high"
            )
        );

        let rule = transcript
            .layout_with_footer(&DARK, 64, context.clone())
            .footer_rule(&DARK, 64);
        assert_eq!(rule.width(), 64);
        for metric in [
            "↑218k",
            "↓0",
            "R163k",
            "W0",
            "CH75.0%",
            "$0.2829",
            "80.0%/272k",
        ] {
            assert!(!rule.to_string().contains(metric), "{rule}");
        }
        assert!(
            rule.to_string().contains("~/…/kiki (Refactor auth)"),
            "{rule}"
        );
        assert!(
            rule.to_string().contains("gpt-5.6-sol • thinking high"),
            "{rule}"
        );

        let rule = transcript
            .layout_with_footer(&DARK, 52, context.clone())
            .footer_rule(&DARK, 52);
        assert!(!rule.to_string().contains("$0.2829"), "{rule}");
        assert!(rule.to_string().contains("Refactor auth"), "{rule}");
        assert!(
            rule.to_string().contains("gpt-5.6-sol • thinking high"),
            "{rule}"
        );

        let model_only = transcript
            .layout_with_footer(&DARK, 31, context.clone())
            .footer_rule(&DARK, 31);
        assert_eq!(model_only.to_string(), "─  …t-5.6-sol • thinking high ─");
        assert_eq!(model_only.width(), 31);
        for width in 0..=30 {
            let rule = transcript
                .layout_with_footer(&DARK, width, context.clone())
                .footer_rule(&DARK, width);
            assert_eq!(rule.width(), usize::from(width), "{rule}");
        }

        let model = Transcript::default()
            .layout_with_footer(&DARK, 80, context)
            .footer_rule(&DARK, 80);
        assert!(
            model.to_string().contains("~/…/kiki (Refactor auth)"),
            "{model}"
        );
        assert!(
            model
                .to_string()
                .contains("openai/gpt-5.6-sol • thinking high"),
            "{model}"
        );
    }

    #[test]
    fn footer_keeps_shortened_identity_before_dropping_metrics() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(
                1,
                1,
                "metrics()",
                "",
                vec![detailed_usage(
                    Some(217_600),
                    Some(0),
                    Some(163_200),
                    Some(0),
                    Some(0),
                    Some(217_600),
                )],
            ),
            false,
        );
        let context = FooterContext {
            workspace_root: Some("/home/kiki/worktrees/interactive/settings/kiki".to_owned()),
            home: Some("/home/kiki".into()),
            name: Some("Refactor auth".to_owned()),
            selection: ModelSelection::new(&SOL, "high").expect("supported thinking"),
        };

        let footer = transcript
            .layout_with_footer(&DARK, 79, context)
            .footer_rule(&DARK, 79)
            .to_string();

        assert_eq!(
            footer,
            "─ ~/…/kiki (Refactor auth) ── $0.2829 80.0%/272k  gpt-5.6-sol • thinking high ─"
        );
    }

    #[test]
    fn footer_clips_thinking_without_model_truncation_artefacts() {
        for (width, expected) in [(2, "t…"), (3, "th…")] {
            let field = ModelFooter {
                id: SOL.id.to_owned(),
                thinking: "medium".to_owned(),
            }
            .truncated(&DARK, width)
            .expect("model footer");

            assert_eq!(field.spans[0].content, expected);
        }
    }

    #[test]
    fn footer_name_only_fallback_has_no_leading_space() {
        let field = FooterIdentity {
            cwd: Some("/home/kiki/worktrees/interactive/settings/kiki".to_owned()),
            name: Some("Refactor auth".to_owned()),
        }
        .truncated(&DARK, 3)
        .expect("identity footer");

        assert_eq!(field.spans[0].content, "(R…");
    }

    #[test]
    fn footer_sums_mixed_model_costs_without_per_step_regression() {
        let mut terra = usage(100, 0);
        let AttemptAccounting::KnownUsage(model_usage) = &mut terra else {
            unreachable!("usage helper produces known usage")
        };
        model_usage.model = TERRA.id.to_owned();

        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "mixed()", "", vec![usage(100, 0), terra]),
            false,
        );

        let footer = footer_text(&transcript.layout(&DARK, 80)).expect("footer");
        assert!(footer.contains("$0.0006000"), "{footer}");
        let history = rendered_text(&transcript);
        assert!(history.contains("2 attempts · ↑200 ↓0 R0 W0 r0 Σ200 · $0.0006"));
    }

    #[test]
    fn footer_reports_short_context_occupancy_and_cache_hit_rate() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(
                1,
                1,
                "metrics()",
                "",
                vec![detailed_usage(
                    Some(217_600),
                    Some(0),
                    Some(163_200),
                    Some(0),
                    Some(0),
                    Some(217_600),
                )],
            ),
            false,
        );

        assert_eq!(
            footer_text(&transcript.layout(&DARK, 80)).as_deref(),
            Some("↑218k ↓0 R163k W0 CH75.0% $0.2829 80.0%/272k")
        );
    }

    #[test]
    fn footer_context_metric_switches_capacity_and_alert_styles() {
        for (total, context, percentage, denominator, style) in [
            (
                217_328,
                "79.9%/272k",
                "79.9%",
                "/272k",
                Style::reset().fg(DARK.muted).bg(Color::Reset),
            ),
            (
                217_600,
                "80.0%/272k",
                "80.0%",
                "/272k",
                Style::new().fg(DARK.warning),
            ),
            (
                244_800,
                "90.0%/272k",
                "90.0%",
                "/272k",
                Style::new().fg(DARK.error),
            ),
            (
                272_000,
                "100.0%/272k",
                "100.0%",
                "/272k",
                Style::new().fg(DARK.error),
            ),
            (
                272_001,
                "25.9%/1.05m",
                "25.9%",
                "/1.05m",
                muted_page_style(&DARK),
            ),
        ] {
            let mut transcript = Transcript::default();
            transcript.apply(
                completed_execution(
                    1,
                    1,
                    "metrics()",
                    "",
                    vec![detailed_usage(
                        Some(total),
                        Some(0),
                        None,
                        None,
                        None,
                        Some(total),
                    )],
                ),
                false,
            );

            let footer = transcript.layout(&DARK, 128).footer_rule(&DARK, 128);
            assert!(footer.to_string().contains(context), "{footer}");
            assert_eq!(
                footer
                    .spans
                    .iter()
                    .find(|span| span.content == percentage)
                    .expect("context percentage span")
                    .style,
                style
            );
            assert_eq!(
                footer
                    .spans
                    .iter()
                    .find(|span| span.content == denominator)
                    .expect("context denominator span")
                    .style,
                muted_page_style(&DARK)
            );
        }
    }

    #[test]
    fn footer_formats_cache_hit_rates_and_omits_invalid_values() {
        for (input, cached, expected) in [
            (4, 3, "CH75.0%"),
            (4, 4, "CH100.0%"),
            (1_000, 873, "CH87.3%"),
        ] {
            let mut transcript = Transcript::default();
            transcript.apply(
                completed_execution(
                    1,
                    1,
                    "metrics()",
                    "",
                    vec![detailed_usage(
                        Some(input),
                        Some(0),
                        Some(cached),
                        Some(0),
                        Some(0),
                        Some(input),
                    )],
                ),
                false,
            );

            let layout = transcript.layout(&DARK, 80);
            assert!(footer_text(&layout).expect("footer").contains(expected));
            assert_eq!(
                layout
                    .footer_rule(&DARK, 80)
                    .spans
                    .iter()
                    .find(|span| span.content == expected)
                    .expect("cache-hit span")
                    .style,
                muted_page_style(&DARK)
            );
        }

        for usage in [
            detailed_usage(Some(0), Some(0), Some(0), Some(0), Some(0), Some(0)),
            detailed_usage(Some(1), Some(0), None, Some(0), Some(0), Some(1)),
            detailed_usage(Some(1), Some(0), Some(2), Some(0), Some(0), Some(1)),
        ] {
            let mut transcript = Transcript::default();
            transcript.apply(
                completed_execution(1, 1, "metrics()", "", vec![usage]),
                false,
            );

            assert!(
                !footer_text(&transcript.layout(&DARK, 80))
                    .expect("footer")
                    .contains("CH")
            );
        }
    }

    #[test]
    fn footer_counts_compaction_attempts_without_replacing_normal_context_measurement() {
        let normal = detailed_usage(Some(100), Some(10), Some(75), Some(0), Some(0), Some(110));
        let summary = detailed_usage(Some(20), Some(5), Some(5), Some(0), Some(0), Some(25));
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "normal()", "", vec![normal]),
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::Compaction {
                        outcome: CompactionOutcome::Applied {
                            summary: "summary".to_owned(),
                            first_retained: SessionEntryId(2),
                            context_tokens: 1,
                            reason: CompactionReason::Manual,
                            attempts: vec![summary],
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let footer = footer_text(&transcript.layout(&DARK, 128)).expect("footer");
        assert!(footer.contains("↑120 ↓15 R80 W0"), "{footer}");
        assert!(footer.contains("CH75.0%"), "{footer}");
        assert!(!footer.contains("CH25.0%"), "{footer}");
    }

    fn overflow_recovery(attempts: Vec<AttemptAccounting>) -> crate::agent::OverflowRecovery {
        crate::agent::OverflowRecovery {
            failure: crate::model::ProviderFailure::new(
                Some(400),
                Some("context_length_exceeded".to_owned()),
                "maximum context length".to_owned(),
            )
            .with_context_overflow(true),
            generation: Some(GenerationEvidence {
                reasoning_summary: String::new(),
                reasoning: "initial reasoning".to_owned(),
                assistant_text: "initial partial".to_owned(),
                decoded_python: "initial_code()".to_owned(),
            }),
            attempts,
        }
    }

    fn recovered_final(
        attempts: Vec<AttemptAccounting>,
        overflow_recovery: Option<crate::agent::OverflowRecovery>,
    ) -> RuntimeEvent {
        RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: RunId(1),
                record: Record::FinalMessage {
                    step_id: ModelStepId(1),
                    text: "retry final".to_owned(),
                    reasoning_summary: String::new(),
                    terminal: TerminalDetails {
                        evidence: None,
                        attempts,
                        overflow_recovery,
                    },
                },
            },
            progress_gap: false,
        }
    }

    #[test]
    fn overflow_recovery_renders_initial_evidence_before_the_last_outcome() {
        let initial_blocks = |attempts: &[AttemptAccounting]| {
            let mut blocks = vec![
                TranscriptBlock::Status {
                    text: "context overflow: provider failure (HTTP 400) [context_length_exceeded]: maximum context length".to_owned(),
                },
                TranscriptBlock::Reasoning {
                    markdown: "initial reasoning".to_owned(),
                },
                TranscriptBlock::Assistant {
                    markdown: "initial partial".to_owned(),
                    accounting: None,
                },
                TranscriptBlock::Arena(ArenaBlock {
                    state: super::ArenaState::Error,
                    heading: None,
                    status: None,
                    source: Some("initial_code()".to_owned()),
                    tool_calls: Vec::new(),
                    run_previews: Vec::new(),
                    content: Vec::new(),
                    progress_gap: false,
                    accounting: None,
                }),
            ];
            if let Some(text) = step_accounting(attempts) {
                blocks.push(TranscriptBlock::Accounting { text });
            }
            blocks
        };
        let initial = vec![usage(1, 1)];
        let mut transcript = Transcript::default();
        transcript.apply(
            recovered_final(vec![usage(2, 2)], Some(overflow_recovery(initial.clone()))),
            false,
        );
        let mut expected = initial_blocks(&initial);
        expected.push(TranscriptBlock::Assistant {
            markdown: "retry final".to_owned(),
            accounting: step_accounting(&[usage(2, 2)]),
        });
        assert_eq!(transcript.blocks(), expected);

        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::FailedRun {
                        message: "retry failed".to_owned(),
                        terminal: TerminalDetails {
                            evidence: None,
                            attempts: Vec::new(),
                            overflow_recovery: Some(overflow_recovery(Vec::new())),
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let mut expected = initial_blocks(&[]);
        expected.push(TranscriptBlock::Status {
            text: "failed: retry failed".to_owned(),
        });
        assert_eq!(transcript.blocks(), expected);
    }

    #[test]
    fn footer_bills_initial_overflow_attempts_without_replacing_normal_occupancy() {
        let footer = |event| {
            let mut transcript = Transcript::default();
            transcript.apply(event, false);
            footer_text(&transcript.layout(&DARK, 128)).unwrap_or_default()
        };
        let known = detailed_usage(
            Some(100_000),
            Some(0),
            Some(75_000),
            Some(0),
            Some(0),
            Some(100_000),
        );

        let unavailable = footer(recovered_final(
            vec![AttemptAccounting::UsageUnavailable],
            None,
        ));
        let recovered = footer(recovered_final(
            vec![AttemptAccounting::UsageUnavailable],
            Some(overflow_recovery(vec![known.clone()])),
        ));
        assert!(!unavailable.contains('$'), "{unavailable}");
        assert!(recovered.contains("↑100k"), "{recovered}");
        assert!(recovered.contains('$'), "{recovered}");
        assert!(!recovered.contains("%/"), "{recovered}");
        assert!(!recovered.contains("CH"), "{recovered}");

        let ordinary = footer(recovered_final(vec![known.clone()], None));
        let doubled = footer(recovered_final(
            vec![known.clone()],
            Some(overflow_recovery(vec![known])),
        ));
        assert!(ordinary.contains("↑100k"), "{ordinary}");
        assert!(doubled.contains("↑200k"), "{doubled}");
        for footer in [ordinary, doubled] {
            assert!(footer.contains("CH75.0% "), "{footer}");
            assert!(footer.contains("36.8%/272k"), "{footer}");
        }
    }

    #[test]
    fn footer_uses_the_latest_settled_attempt_during_streaming() {
        let first = detailed_usage(
            Some(100_000),
            Some(0),
            Some(25_000),
            Some(0),
            Some(0),
            Some(100_000),
        );
        let latest = detailed_usage(
            Some(217_600),
            Some(0),
            Some(163_200),
            Some(0),
            Some(0),
            Some(217_600),
        );
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "metrics()", "", vec![first, latest]),
            false,
        );

        let settled_footer = footer_text(&transcript.layout(&DARK, 128)).expect("footer");
        assert!(settled_footer.contains("CH75.0%"));
        assert!(settled_footer.contains("80.0%/272k"));
        assert!(!settled_footer.contains("CH25.0%"));

        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            false,
        );
        assert_eq!(
            footer_text(&transcript.layout(&DARK, 128)).as_deref(),
            Some(settled_footer.as_str())
        );

        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: AttemptAccounting::UsageUnavailable,
                },
            },
            false,
        );
        let unavailable_layout = transcript.layout(&DARK, 128);
        let unavailable_footer = footer_text(&unavailable_layout).expect("footer");
        assert!(!unavailable_footer.contains("%/"));
        assert!(!unavailable_footer.contains("CH"));
    }

    #[test]
    fn footer_aggregates_child_records_without_changing_foreground_context() {
        let root = detailed_usage(Some(100), Some(0), Some(50), Some(0), Some(0), Some(100));
        let child = detailed_usage(Some(200), Some(10), Some(0), Some(0), Some(0), Some(210));
        let mut transcript = Transcript::default();
        transcript.apply(completed_execution(1, 1, "root()", "", vec![root]), false);
        transcript.apply(RuntimeEvent::RunSettled { run_id: RunId(1) }, false);

        let before = footer_text(&transcript.layout(&DARK, 128)).expect("root footer");
        assert!(before.contains("CH50.0%"), "{before}");
        assert!(before.contains("0.0%/272k"), "{before}");

        let child = TaskRecord::for_test(
            "c1",
            3,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            vec![child],
        );
        assert!(transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: child.clone(),
            },
            false,
        ));
        let footer = footer_text(&transcript.layout(&DARK, 128)).expect("aggregated footer");
        assert!(footer.contains("↑300 ↓10 R50 W0"), "{footer}");
        assert!(footer.contains("CH50.0%"), "{footer}");
        assert!(footer.contains("0.0%/272k"), "{footer}");

        assert!(!transcript.apply(
            RuntimeEvent::SubagentStatus {
                id: child.id().clone(),
                revision: 4,
                observation: child.observation().clone(),
            },
            false,
        ));
        assert!(!transcript.apply(
            RuntimeEvent::SubagentStatus {
                id: child.id().clone(),
                revision: 2,
                observation: child.observation().clone(),
            },
            false,
        ));
        assert!(!transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: child.clone(),
            },
            false,
        ));
        assert!(transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: child.clone(),
            },
            false,
        ));
        assert_eq!(
            footer_text(&transcript.layout(&DARK, 128)).as_deref(),
            Some(footer.as_str())
        );
    }

    #[test]
    fn footer_counts_mixed_model_descendants_once_and_keeps_root_context() {
        let root = detailed_usage_for(
            LUNA.id,
            Some(100),
            Some(10),
            Some(50),
            Some(0),
            Some(0),
            Some(110),
        );
        let child_attempt = detailed_usage_for(
            TERRA.id,
            Some(200),
            Some(20),
            Some(0),
            Some(0),
            Some(0),
            Some(220),
        );
        let nested_attempt = detailed_usage_for(
            SOL.id,
            Some(300),
            Some(30),
            Some(0),
            Some(0),
            Some(0),
            Some(330),
        );
        let child = TaskRecord::for_test(
            "c1",
            2,
            ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap(),
            None,
            1,
            vec![child_attempt.clone()],
        );
        let nested = TaskRecord::for_test(
            "d1",
            2,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            Some("c1"),
            2,
            vec![nested_attempt.clone()],
        );
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(1, 1, "root", vec![root.clone()], false),
            false,
        );
        let before = footer_text(&transcript.layout(&DARK, 128)).expect("root footer");

        assert_eq!(child.attempts(), std::slice::from_ref(&child_attempt));
        assert_eq!(nested.parent(), Some(child.id()));
        assert_eq!(nested.depth(), 2);
        assert_eq!(nested.attempts(), std::slice::from_ref(&nested_attempt));
        assert_eq!(
            child.observation().cost(),
            estimate_run_cost([&child_attempt])
        );
        assert_eq!(
            nested.observation().cost(),
            estimate_run_cost([&nested_attempt])
        );
        assert!(transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: child.clone(),
            },
            false,
        ));
        assert!(transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: nested.clone(),
            },
            false,
        ));

        let footer = footer_text(&transcript.layout(&DARK, 128)).expect("mixed-model footer");
        assert!(footer.contains("↑600 ↓60 R50 W0"), "{footer}");
        let RunCostEstimate::Complete(cost) =
            estimate_run_cost([&root, &child_attempt, &nested_attempt])
        else {
            panic!("complete mixed-model cost");
        };
        assert!(
            footer.contains(&format_footer_usd(cost.as_nanos())),
            "{footer}"
        );
        assert!(footer.contains("CH50.0%"), "{footer}");
        assert!(footer.contains("0.0%/272k"), "{footer}");
        assert!(before.contains("CH50.0%"), "{before}");
        assert!(before.contains("0.0%/272k"), "{before}");
    }

    #[test]
    fn child_records_replace_live_attempts_and_ignore_equal_or_stale_revisions() {
        let partial_attempt = detailed_usage_for(
            TERRA.id,
            Some(100),
            Some(10),
            Some(0),
            Some(0),
            Some(0),
            Some(110),
        );
        let additional_attempt = detailed_usage_for(
            TERRA.id,
            Some(200),
            Some(20),
            Some(0),
            Some(0),
            Some(0),
            Some(220),
        );
        let unexpected_attempt = detailed_usage_for(
            TERRA.id,
            Some(300),
            Some(30),
            Some(0),
            Some(0),
            Some(0),
            Some(330),
        );
        let selection = ModelSelection::new(&TERRA, TERRA.default_thinking).unwrap();
        let partial =
            TaskRecord::for_test("c1", 2, selection, None, 1, vec![partial_attempt.clone()]);
        let terminal = TaskRecord::for_test(
            "c1",
            4,
            selection,
            None,
            1,
            vec![partial_attempt.clone(), additional_attempt.clone()],
        );
        let equal_projection = terminal
            .clone()
            .with_observation_for_test(Duration::from_secs(10), 10_000);
        let equal_revision = TaskRecord::for_test(
            "c1",
            4,
            selection,
            None,
            1,
            vec![
                partial_attempt.clone(),
                additional_attempt.clone(),
                unexpected_attempt.clone(),
            ],
        );
        let stale = TaskRecord::for_test(
            "c1",
            3,
            selection,
            None,
            1,
            vec![
                partial_attempt.clone(),
                additional_attempt.clone(),
                unexpected_attempt,
            ],
        );
        let mut transcript = Transcript::default();

        assert!(transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: partial.clone(),
            },
            false,
        ));
        let partial_footer = footer_text(&transcript.layout(&DARK, 128)).expect("partial footer");
        assert!(
            partial_footer.contains("↑100 ↓10 R0 W0"),
            "{partial_footer}"
        );

        assert!(transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: terminal.clone(),
            },
            false,
        ));
        let footer = footer_text(&transcript.layout(&DARK, 128)).expect("terminal footer");
        assert!(footer.contains("↑300 ↓30 R0 W0"), "{footer}");
        let RunCostEstimate::Complete(cost) =
            estimate_run_cost([&partial_attempt, &additional_attempt])
        else {
            panic!("complete terminal cost");
        };
        assert!(
            footer.contains(&format_footer_usd(cost.as_nanos())),
            "{footer}"
        );

        assert!(!transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: terminal.clone(),
            },
            false,
        ));
        assert!(!transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: equal_projection,
            },
            false,
        ));
        let retained = transcript
            .subagent_records
            .get(terminal.id())
            .expect("retained terminal record")
            .observation();
        assert_eq!(retained.elapsed(), Duration::ZERO);
        assert_eq!(retained.observed_at_ms(), 0);
        assert!(!transcript.apply(
            RuntimeEvent::SubagentRecord {
                record: equal_revision.clone(),
            },
            false,
        ));
        assert!(!transcript.apply(RuntimeEvent::SubagentRecord { record: stale }, false,));
        assert!(!transcript.apply(
            RuntimeEvent::SubagentFinished {
                record: equal_revision,
            },
            false,
        ));
        assert_eq!(transcript.finished_subagent_cards.len(), 1);
        assert_eq!(
            footer_text(&transcript.layout(&DARK, 128)).as_deref(),
            Some(footer.as_str())
        );
    }

    #[test]
    fn footer_rule_fills_the_terminal_and_truncates_to_latest_metrics() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(
                1,
                1,
                "metrics()",
                "",
                vec![detailed_usage(
                    Some(217_600),
                    Some(0),
                    Some(163_200),
                    Some(0),
                    Some(0),
                    Some(217_600),
                )],
            ),
            false,
        );

        let full = transcript.layout(&DARK, 128).footer_rule(&DARK, 128);
        assert_eq!(full.width(), 128);
        assert!(full.to_string().starts_with('─'), "{full}");
        assert!(full.to_string().ends_with(" ─"), "{full}");
        assert_eq!(
            full.spans
                .iter()
                .find(|span| span.content == "80.0%")
                .expect("context percentage span")
                .style,
            Style::new().fg(DARK.warning)
        );

        let narrow = transcript.layout(&DARK, 29).footer_rule(&DARK, 29);
        assert_eq!(narrow.width(), 29);
        assert_eq!(narrow.to_string(), "─ ────── $0.2829 80.0%/272k ─");
        assert_eq!(
            narrow
                .spans
                .iter()
                .find(|span| span.content == "80.0%")
                .expect("context percentage span")
                .style,
            Style::new().fg(DARK.warning)
        );

        for width in 0..=12 {
            let rule = Transcript::default()
                .layout(&DARK, width)
                .footer_rule(&DARK, width);
            assert_eq!(rule.width(), usize::from(width));
            assert_eq!(rule.to_string(), "─".repeat(usize::from(width)));

            let populated_rule = transcript.layout(&DARK, width).footer_rule(&DARK, width);
            assert_eq!(populated_rule.width(), usize::from(width));
        }
    }

    #[test]
    fn failed_run_displays_retries_when_usage_is_unavailable() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::FailedRun {
                        message:
                            "model provider failed: provider failure: provider transport failed"
                                .to_owned(),
                        terminal: TerminalDetails {
                            evidence: None,
                            attempts: vec![AttemptAccounting::UsageUnavailable; 3],
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        assert_eq!(
            transcript.blocks(),
            [
                TranscriptBlock::Status {
                    text:
                        "failed: model provider failed: provider failure: provider transport failed"
                            .to_owned(),
                },
                TranscriptBlock::Accounting {
                    text: "3 attempts".to_owned(),
                },
            ]
        );
        assert_eq!(footer_text(&transcript.layout(&DARK, 120)), None);
        let rendered = rendered_layout_text(&mut transcript);
        assert!(rendered.contains("3 attempts"), "{rendered}");
    }

    #[test]
    fn accounting_compacts_attempt_fields_and_omits_unavailable_values() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(
                1,
                1,
                "accounting()",
                "",
                vec![
                    detailed_usage(Some(100), Some(20), Some(10), Some(5), Some(5), Some(120)),
                    detailed_usage(Some(0), None, Some(0), None, Some(0), Some(0)),
                    AttemptAccounting::UsageUnavailable,
                ],
            ),
            false,
        );

        let blocks = transcript.blocks();
        let arena = blocks
            .iter()
            .find_map(|block| match block {
                TranscriptBlock::Arena(arena) => Some(arena),
                _ => None,
            })
            .expect("accounting arena");
        assert_eq!(
            arena.accounting.as_deref(),
            Some("3 attempts · ↑100 ↓20 R10 W5 r5 Σ120 · $0.000769")
        );

        assert_eq!(
            footer_text(&transcript.layout(&DARK, 1)).as_deref(),
            Some("↑100 ↓20 R10 W5 $0.0007690")
        );

        let mut unavailable = Transcript::default();
        unavailable.apply(
            completed_execution(
                2,
                1,
                "unavailable()",
                "",
                vec![AttemptAccounting::UsageUnavailable],
            ),
            false,
        );
        let unavailable_blocks = unavailable.blocks();
        let [TranscriptBlock::Arena(unavailable_arena)] = unavailable_blocks.as_slice() else {
            panic!("unavailable accounting arena")
        };
        assert_eq!(unavailable_arena.accounting, None);
        assert_eq!(footer_text(&unavailable.layout(&DARK, 1)), None);

        let AttemptAccounting::KnownUsage(mut partial_usage) =
            detailed_usage(Some(7), None, None, None, None, None)
        else {
            unreachable!("known usage helper")
        };
        partial_usage.service_tier = Some("priority".to_owned());
        let mut unpriceable = Transcript::default();
        unpriceable.apply(
            completed_execution(
                3,
                1,
                "unpriceable()",
                "",
                vec![AttemptAccounting::KnownUsage(partial_usage)],
            ),
            false,
        );
        let unpriceable_blocks = unpriceable.blocks();
        let [TranscriptBlock::Arena(unpriceable_arena)] = unpriceable_blocks.as_slice() else {
            panic!("unpriceable accounting arena")
        };
        assert_eq!(unpriceable_arena.accounting.as_deref(), Some("↑7"));
        assert_eq!(
            footer_text(&unpriceable.layout(&DARK, 1)).as_deref(),
            Some("↑7")
        );

        let mut zeroes = Transcript::default();
        zeroes.apply(
            completed_execution(
                4,
                1,
                "zeroes()",
                "",
                vec![detailed_usage(
                    Some(0),
                    None,
                    Some(0),
                    None,
                    Some(0),
                    Some(0),
                )],
            ),
            false,
        );
        let zeroes_blocks = zeroes.blocks();
        let [TranscriptBlock::Arena(zeroes_arena)] = zeroes_blocks.as_slice() else {
            panic!("zero accounting arena")
        };
        assert_eq!(zeroes_arena.accounting.as_deref(), Some("↑0 R0 r0 Σ0"));
        assert_eq!(
            footer_text(&zeroes.layout(&DARK, 1)).as_deref(),
            Some("↑0 R0 0.0%/272k")
        );

        let mut complete = Transcript::default();
        complete.apply(
            completed_execution(
                2,
                1,
                "complete()",
                "",
                vec![detailed_usage(
                    Some(100),
                    Some(20),
                    Some(10),
                    Some(5),
                    Some(5),
                    Some(120),
                )],
            ),
            false,
        );
        assert!(rendered_text(&complete).contains("↑100 ↓20 R10 W5 r5 Σ120 · $0.000769"));

        let history = rendered_text(&transcript);
        for verbose in [
            "attempt 1:",
            "model=",
            "tier=",
            "pricing",
            "Complete",
            "LowerBound",
            "USD",
            "Run estimated cost",
            "unavailable",
        ] {
            assert!(!history.contains(verbose), "{history}");
        }
    }

    #[test]
    fn accounting_keeps_attempts_step_local_across_live_and_terminal_replacement() {
        let first = detailed_usage(Some(100), Some(0), Some(0), Some(0), Some(0), Some(100));
        let residual = detailed_usage(Some(0), Some(100), Some(0), Some(0), Some(20), Some(100));
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: first.clone(),
                },
            },
            false,
        );
        assert_eq!(
            rendered_text(&transcript)
                .matches("↑100 ↓0 R0 W0 r0 Σ100 · $0.0004")
                .count(),
            1
        );

        transcript.apply(completed_execution(1, 1, "first()", "", vec![first]), false);
        transcript.apply(final_message(1, 2, "done", vec![residual], false), false);
        transcript.apply(
            completed_execution(
                2,
                1,
                "other()",
                "",
                vec![AttemptAccounting::UsageUnavailable],
            ),
            false,
        );

        let history = rendered_text(&transcript);
        assert_eq!(
            history.matches("↑100 ↓0 R0 W0 r0 Σ100 · $0.0004").count(),
            1
        );
        assert_eq!(
            history.matches("↑0 ↓100 R0 W0 r20 Σ100 · $0.002").count(),
            1
        );
        assert!(!history.contains("#"));
        assert!(!history.contains("Run estimated cost"));
    }

    #[test]
    fn projection_projects_disabled_status_and_rejects_late_progress() {
        let mut transcript = Transcript::default();
        transcript.begin_submission();
        assert!(transcript.apply(
            RuntimeEvent::SubmissionRejectedDisabled {
                reason: RuntimeDisabledReason::Authentication(Provider::OpenAi),
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
    fn anthropic_authentication_failure_shows_the_anthropic_status() {
        let mut transcript = Transcript::default();

        assert!(transcript.apply(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::Authentication(Provider::Anthropic),
            },
            false,
        ));

        assert!(transcript.is_disabled());
        assert_eq!(
            transcript.status(),
            "Anthropic authentication failed; restart Kiki"
        );
    }

    #[test]
    fn google_authentication_failure_shows_the_google_status() {
        let mut transcript = Transcript::default();

        assert!(transcript.apply(
            RuntimeEvent::RuntimeDisabled {
                reason: RuntimeDisabledReason::Authentication(Provider::Google),
            },
            false,
        ));

        assert!(transcript.is_disabled());
        assert_eq!(
            transcript.status(),
            "Google authentication failed; restart Kiki"
        );
    }

    #[test]
    fn execution_paths_remain_private_in_collapsed_and_expanded_cards() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(""),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::Complete { result: None },
                            tool_calls: Vec::new(),
                            read_paths: vec![PathBuf::from("private/effect-sentinel")],
                            modified_paths: vec![PathBuf::from("private/mutation-sentinel")],
                            changed_bindings: Vec::new(),
                        },
                        attempts: Vec::new(),
                    },
                },
                progress_gap: false,
            },
            false,
        );

        for expanded in [false, true] {
            if expanded {
                assert!(transcript.toggle_arena_expansion());
            }
            let rendered = rendered_layout_text(&mut transcript);
            assert!(!rendered.contains("private/effect-sentinel"));
            assert!(!rendered.contains("private/mutation-sentinel"));
        }
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
                        commentary: None,
                        source: "print('hello')\n2".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("hello\n"),
                            stderr: observation("warning\n"),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: "boom".to_owned(),
                                omitted_bytes: 0,
                            }),
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
                                },
                            ))),
                            attempts: Vec::new(),
                            overflow_recovery: None,
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
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let history = rendered_text(&transcript);
        for text in [
            "stdout: hello",
            "stderr: warning",
            "error: boom",
            "code:\nawait read(path)",
            "stderr: diagnostic",
            "tool calls:\n\u{2717} read · failed",
            "failure: interrupted",
            "warning: cancelled warning",
            "interrupted reasoning",
            "interrupted assistant",
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
    fn rendering_settled_assistant_markdown_is_not_flattened() {
        let mut transcript = Transcript::default();
        transcript.apply(
            final_message(1, 1, "# Settled heading", Vec::new(), false),
            false,
        );

        let layout = transcript.layout_fresh(&DARK, 80);
        let assistant = card_with_text(&layout.blocks, "Settled heading");
        assert!(
            assistant.text.lines[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn rendering_uses_typed_blocks_in_chronological_order() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "inspect".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(1, 1, "first()", "# Before action", Vec::new()),
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::FinalMessage {
                        step_id: ModelStepId(2),
                        text: "# Final answer".to_owned(),
                        reasoning_summary: "## Before final".to_owned(),
                        terminal: TerminalDetails::empty(),
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let roles = transcript
            .blocks()
            .iter()
            .map(|block| match block {
                TranscriptBlock::Startup { .. } => "startup",
                TranscriptBlock::ModelChange { .. } => "model change",
                TranscriptBlock::User { .. } => "user",
                TranscriptBlock::SubagentFinished(..) => "subagent finished",
                TranscriptBlock::Compaction(..) => "compaction",
                TranscriptBlock::SkillActivation(..) => "skill activation",
                TranscriptBlock::Reasoning { .. } => "reasoning",
                TranscriptBlock::Arena(..) => "arena",
                TranscriptBlock::Assistant { .. } => "assistant",
                TranscriptBlock::Status { .. } => "status",
                TranscriptBlock::SessionRestored => "session restored",
                TranscriptBlock::Metadata { .. } => "metadata",
                TranscriptBlock::Accounting { .. } => "accounting",
            })
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            ["user", "reasoning", "arena", "reasoning", "assistant"]
        );
    }

    #[test]
    fn rendering_live_prefixes_converge_on_authoritative_markdown() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        for progress in [
            ModelProgress::ReasoningSummaryChunk {
                text: "# Live reasoning".to_owned(),
            },
            ModelProgress::DecodedPythonChunk {
                code: "print('live')".to_owned(),
            },
            ModelProgress::AssistantTextChunk {
                text: "# Live assistant".to_owned(),
            },
        ] {
            transcript.apply(
                RuntimeEvent::ModelProgress {
                    run_id: RunId(1),
                    step_id: ModelStepId(1),
                    epoch: 0,
                    progress,
                },
                false,
            );
        }

        let live = transcript.layout(&DARK, 80);
        let live_markdown = live
            .blocks
            .iter()
            .filter_map(card)
            .filter(|card| {
                let text = card.text.to_string();
                text.contains("Live reasoning") || text.contains("Live assistant")
            })
            .collect::<Vec<_>>();
        assert_eq!(live_markdown.len(), 2);
        assert!(live_markdown.iter().all(|block| {
            block.text.lines[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        }));

        transcript.apply(
            completed_execution(1, 1, "print('settled')", "# Settled reasoning", Vec::new()),
            false,
        );
        let settled_layout = transcript.layout_fresh(&DARK, 80);
        let settled_reasoning = card_with_text(&settled_layout.blocks, "Settled reasoning");
        assert!(
            settled_reasoning.text.lines[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let settled = rendered_text(&transcript);
        assert!(settled.contains("Settled reasoning"));
        assert!(settled.contains("print('settled')"));
        assert!(!settled.contains("Live reasoning"));
        assert!(!settled.contains("Live assistant"));
        assert!(!settled.contains("print('live')"));
    }

    #[test]
    fn rendering_live_arena_owns_live_details() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        for progress in [
            ModelProgress::AttemptStarted { attempt: 1 },
            ModelProgress::ReasoningSummaryChunk {
                text: "live reasoning".to_owned(),
            },
            ModelProgress::DecodedPythonChunk {
                code: "print('live')".to_owned(),
            },
            ModelProgress::AttemptSettled {
                attempt: 1,
                accounting: usage(1, 1),
            },
            ModelProgress::AssistantTextChunk {
                text: "live assistant".to_owned(),
            },
        ] {
            let gap_before = matches!(&progress, ModelProgress::AttemptStarted { .. });
            transcript.apply(
                RuntimeEvent::ModelProgress {
                    run_id: RunId(1),
                    step_id: ModelStepId(1),
                    epoch: 0,
                    progress,
                },
                gap_before,
            );
        }

        let blocks = transcript.blocks();
        let roles = blocks
            .iter()
            .map(|block| match block {
                TranscriptBlock::Startup { .. } => "startup",
                TranscriptBlock::ModelChange { .. } => "model change",
                TranscriptBlock::User { .. } => "user",
                TranscriptBlock::SubagentFinished(..) => "subagent finished",
                TranscriptBlock::Compaction(..) => "compaction",
                TranscriptBlock::SkillActivation(..) => "skill activation",
                TranscriptBlock::Reasoning { .. } => "reasoning",
                TranscriptBlock::Arena(..) => "arena",
                TranscriptBlock::Assistant { .. } => "assistant",
                TranscriptBlock::Status { .. } => "status",
                TranscriptBlock::SessionRestored => "session restored",
                TranscriptBlock::Metadata { .. } => "metadata",
                TranscriptBlock::Accounting { .. } => "accounting",
            })
            .collect::<Vec<_>>();
        assert_eq!(roles, ["reasoning", "assistant", "arena"]);

        let [
            TranscriptBlock::Reasoning { .. },
            TranscriptBlock::Assistant { .. },
            TranscriptBlock::Arena(arena),
        ] = blocks.as_slice()
        else {
            unreachable!("live arena roles checked")
        };
        assert_eq!(arena.heading, None);
        assert!(arena.progress_gap);
        assert_eq!(arena.status, None);
        assert_eq!(
            arena.accounting.as_deref(),
            Some("↑1 ↓1 R0 W0 r0 Σ2 · $0.000024")
        );

        let rendered = rendered_text(&transcript);
        assert_eq!(rendered.matches("print('live')").count(), 1);
        assert!(
            rendered.find("live reasoning").expect("reasoning")
                < rendered.find("print('live')").expect("arena")
        );
        assert!(
            rendered.find("live assistant").expect("assistant")
                < rendered.find("print('live')").expect("arena")
        );
    }

    #[test]
    fn rendering_preserves_every_partial_generation_combination() {
        for mask in 0..16 {
            let summary = if mask & 1 != 0 {
                "authoritative summary"
            } else {
                ""
            };
            let reasoning = if mask & 2 != 0 {
                "streamed reasoning"
            } else {
                ""
            };
            let assistant = if mask & 4 != 0 {
                "streamed assistant"
            } else {
                ""
            };
            let code = if mask & 8 != 0 { "streamed_code()" } else { "" };
            let mut transcript = Transcript::default();
            transcript.apply(
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(mask),
                        record: Record::CancelledRun {
                            terminal: TerminalDetails {
                                evidence: Some(TerminalEvidence::Generation(GenerationEvidence {
                                    reasoning_summary: summary.to_owned(),
                                    reasoning: reasoning.to_owned(),
                                    assistant_text: assistant.to_owned(),
                                    decoded_python: code.to_owned(),
                                })),
                                attempts: Vec::new(),
                                overflow_recovery: None,
                            },
                        },
                    },
                    progress_gap: false,
                },
                false,
            );

            let blocks = transcript.blocks();
            let reasoning_blocks = blocks
                .iter()
                .filter_map(|block| match block {
                    TranscriptBlock::Reasoning { markdown } => Some(markdown.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            let expected_reasoning = [summary, reasoning]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>();
            assert_eq!(reasoning_blocks, expected_reasoning, "mask {mask}");
            assert_eq!(
                blocks.iter().any(|block| matches!(block, TranscriptBlock::Assistant { markdown, .. } if markdown == assistant)),
                !assistant.is_empty(),
                "mask {mask}"
            );
            assert_eq!(
                blocks.iter().any(|block| matches!(block, TranscriptBlock::Arena(ArenaBlock { source: Some(source), .. }) if source == code)),
                !code.is_empty(),
                "mask {mask}"
            );
            if !assistant.is_empty() && !code.is_empty() {
                let assistant_index = blocks
                    .iter()
                    .position(|block| matches!(block, TranscriptBlock::Assistant { .. }))
                    .unwrap();
                let arena_index = blocks
                    .iter()
                    .position(|block| matches!(block, TranscriptBlock::Arena(..)))
                    .unwrap();
                assert!(assistant_index < arena_index, "mask {mask}");
            }
        }
    }

    #[test]
    fn rendering_settled_commentary_before_its_execution_arena() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: Some("I’ll inspect the value first.".to_owned()),
                        source: "print(value)".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("value\n"),
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
            },
            false,
        );

        let rendered = rendered_text(&transcript);
        assert!(rendered.find("I’ll inspect").unwrap() < rendered.find("print(value)").unwrap());
    }

    #[test]
    fn rendering_terminal_arenas_own_terminal_details() {
        let cases = [
            (
                1,
                "cancelled".to_owned(),
                true,
                Record::CancelledRun {
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                            commentary: None,
                            source: "cancelled_code()".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation(""),
                            stderr: observation(""),
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            failure: None,
                            cancellation_warning: None,
                        }))),
                        attempts: vec![usage(1, 1)],
                        overflow_recovery: None,
                    },
                },
            ),
            (
                2,
                "failed: model failed".to_owned(),
                false,
                Record::FailedRun {
                    message: "model failed".to_owned(),
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Generation(GenerationEvidence {
                            reasoning_summary: String::new(),
                            reasoning: String::new(),
                            assistant_text: String::new(),
                            decoded_python: "failed_code()".to_owned(),
                        })),
                        attempts: vec![usage(1, 1)],
                        overflow_recovery: None,
                    },
                },
            ),
        ];

        for (run_id, diagnostic, accounting_in_arena, record) in cases {
            let mut transcript = Transcript::default();
            transcript.apply(
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(run_id),
                        record,
                    },
                    progress_gap: true,
                },
                false,
            );

            let blocks = transcript.blocks();
            assert!(rendered_text(&transcript).contains(&diagnostic));
            assert!(
                !blocks
                    .iter()
                    .any(|block| matches!(block, TranscriptBlock::Status { .. }))
            );
            assert!(
                !blocks
                    .iter()
                    .any(|block| matches!(block, TranscriptBlock::Metadata { .. }))
            );
            let arenas = blocks
                .iter()
                .filter_map(|block| match block {
                    TranscriptBlock::Arena(arena) => Some(arena),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(arenas.len(), 1);
            assert_eq!(arenas[0].heading.as_deref(), Some(diagnostic.as_str()));
            assert!(arenas[0].progress_gap);
            assert_eq!(
                arenas[0].accounting.as_deref(),
                accounting_in_arena.then_some("↑1 ↓1 R0 W0 r0 Σ2 · $0.000024")
            );
            assert_eq!(
                blocks
                    .iter()
                    .filter(|block| matches!(block, TranscriptBlock::Accounting { .. }))
                    .count(),
                usize::from(!accounting_in_arena)
            );
        }
    }

    #[test]
    fn rendering_generated_terminal_accounting_belongs_to_assistant() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::FailedRun {
                        message: "provider failed".to_owned(),
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Generation(GenerationEvidence {
                                reasoning_summary: String::new(),
                                reasoning: String::new(),
                                assistant_text: "partial answer".to_owned(),
                                decoded_python: "partial_code()".to_owned(),
                            })),
                            attempts: vec![usage(1, 1)],
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let blocks = transcript.blocks();
        let arena = blocks
            .iter()
            .find_map(|block| match block {
                TranscriptBlock::Arena(arena) => Some(arena),
                _ => None,
            })
            .expect("generation arena");
        assert_eq!(arena.accounting, None);
        assert!(blocks.iter().any(|block| {
            matches!(
                block,
                TranscriptBlock::Assistant {
                    markdown,
                    accounting: Some(accounting),
                } if markdown == "partial answer" && accounting == "↑1 ↓1 R0 W0 r0 Σ2 · $0.000024"
            )
        }));
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, TranscriptBlock::Accounting { .. }))
        );
    }

    #[test]
    fn rendering_terminal_without_arena_keeps_standalone_details() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails::empty(),
                    },
                },
                progress_gap: true,
            },
            false,
        );

        let blocks = transcript.blocks();
        assert!(blocks.iter().any(|block| {
            matches!(block, TranscriptBlock::Status { text } if text == "cancelled")
        }));
        assert!(blocks.iter().any(|block| {
            matches!(block, TranscriptBlock::Metadata { lines } if lines.len() == 1 && lines[0] == "display gap: some live progress was dropped")
        }));
        assert!(!blocks.iter().any(|block| {
            matches!(block, TranscriptBlock::Metadata { lines } if lines.iter().any(|line| line.starts_with("cost $")))
        }));
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, TranscriptBlock::Arena(..)))
        );
    }

    #[test]
    fn execution_start_replaces_streamed_text_with_authoritative_commentary_before_the_arena() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "I’ll inspect".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: Some("I’ll inspect the value first.".to_owned()),
            },
            false,
        );

        let blocks = transcript.blocks();
        assert!(matches!(
            blocks.as_slice(),
            [
                TranscriptBlock::Assistant { markdown, .. },
                TranscriptBlock::Arena(ArenaBlock {
                    heading: None,
                    source: None,
                    content,
                    ..
                })
            ] if markdown == "I’ll inspect the value first."
                && content.is_empty()
        ));
    }

    #[test]
    fn rejected_code_renders_type_check_diagnostics() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "wrong: int = 'text'".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(""),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::TypeCheckError(PythonError {
                                text: "invalid-assignment".to_owned(),
                                omitted_bytes: 0,
                            }),
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
            false,
        );

        let arena = transcript
            .blocks()
            .into_iter()
            .find_map(|block| match block {
                TranscriptBlock::Arena(arena) => Some(arena),
                _ => None,
            })
            .expect("rejected execution card");
        assert!(matches!(arena.state, super::ArenaState::Error));
        assert_eq!(arena.content.len(), 1);
        assert_eq!(arena.content[0].label, "type check");
        assert_eq!(arena.content[0].text, "invalid-assignment");
        let rendered = rendered_text(&transcript);
        assert!(rendered.contains("type check"), "{rendered}");
        assert!(rendered.contains("invalid-assignment"), "{rendered}");
    }

    #[test]
    fn rendering_keeps_execution_content_literal_and_plain() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "**literal source**".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("# [literal output](https://example.com)"),
                            stderr: observation("~~literal stderr~~"),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("**literal result**")),
                            },
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            changed_bindings: Vec::new(),
                        },
                        attempts: vec![AttemptAccounting::UsageUnavailable],
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 100);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for text in [
            "**literal source**",
            "# [literal output](https://example.com)",
            "~~literal stderr~~",
            "**literal result**",
        ] {
            assert!(rendered.contains(text), "{rendered}");
        }
        assert!(buffer.content.iter().all(|cell| {
            !cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        }));
    }

    #[test]
    fn rendering_normalises_every_dynamic_source_before_projection() {
        let hostile = "\x1b]title\x07\r\t\u{009d}";
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(99) }, false);
        transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(99),
                step_id: ModelStepId(1),
                token: ToolCallToken::for_test(0),
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: hostile.to_owned(),
                        selection: None,
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
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: hostile.to_owned(),
                        reasoning_summary: hostile.to_owned(),
                        execution: Execution {
                            stdout: observation(hostile),
                            stderr: observation(hostile),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: hostile.to_owned(),
                                omitted_bytes: 0,
                            }),
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            changed_bindings: Vec::new(),
                        },
                        attempts: vec![AttemptAccounting::KnownUsage(ModelUsage {
                            model: hostile.to_owned(),
                            service_tier: Some(hostile.to_owned()),
                            input_tokens: Some(1),
                            cached_input_tokens: Some(0),
                            cache_write_input_tokens: Some(0),
                            cache_write_1h_input_tokens: 0,
                            output_tokens: Some(1),
                            reasoning_output_tokens: Some(0),
                            total_tokens: Some(2),
                        })],
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
                    record: Record::FinalMessage {
                        step_id: ModelStepId(1),
                        text: hostile.to_owned(),
                        reasoning_summary: hostile.to_owned(),
                        terminal: TerminalDetails::empty(),
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(4),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Generation(GenerationEvidence {
                                reasoning_summary: hostile.to_owned(),
                                reasoning: hostile.to_owned(),
                                assistant_text: hostile.to_owned(),
                                decoded_python: hostile.to_owned(),
                            })),
                            attempts: Vec::new(),
                            overflow_recovery: None,
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
                    run_id: RunId(5),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Execution(Box::new(
                                ExecutionEvidence {
                                    commentary: None,
                                    source: hostile.to_owned(),
                                    reasoning_summary: hostile.to_owned(),
                                    began: true,
                                    stdout: observation(hostile),
                                    stderr: observation(hostile),
                                    tool_calls: vec![ToolCall {
                                        tool: ToolName::Read,
                                        summary: ToolCallSummary::Failed,
                                        subject: None,
                                        duration: None,
                                    }],
                                    read_paths: Vec::new(),
                                    modified_paths: Vec::new(),
                                    failure: Some(ExecutionFailure::RecoveryFailed {
                                        original: hostile.to_owned(),
                                        recovery: hostile.to_owned(),
                                    }),
                                    cancellation_warning: Some(hostile.to_owned()),
                                },
                            ))),
                            attempts: Vec::new(),
                            overflow_recovery: None,
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
                    run_id: RunId(6),
                    record: Record::FailedRun {
                        message: hostile.to_owned(),
                        terminal: TerminalDetails::empty(),
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let rendered = rendered_text(&transcript);
        let marker = "\\x1B]title\\x07\\x0D    \\x9D";
        assert!(rendered.matches(marker).count() >= 18, "{rendered}");
        assert!(
            !rendered
                .chars()
                .any(|character| { character != '\n' && character.is_control() })
        );
        let Record::UserRequest { request, .. } = &transcript.records[0].record.record else {
            unreachable!("user record")
        };
        assert_eq!(request, hostile);
    }

    #[test]
    fn rendering_matches_cjk_and_emoji_measurement() {
        let block = TranscriptBlock::Assistant {
            markdown: "日本語 🦀 日本語 🦀".to_owned(),
            accounting: None,
        }
        .normalise();
        let paragraph = block.paragraph();
        let height = paragraph.line_count(8);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 8, u16::try_from(height).unwrap()));
        paragraph.render(buffer.area, &mut buffer);
        let rendered_rows = buffer
            .content
            .chunks(8)
            .filter(|row| row.iter().any(|cell| cell.symbol() != " "))
            .count();
        assert_eq!(rendered_rows, height);
    }

    #[test]
    fn rendering_clips_blocks_to_the_viewport() {
        let blocks = ["older", "visible", "newer"]
            .into_iter()
            .map(|text| {
                RenderBlock::card(
                    own_styled_text(ratatui::text::Text::raw(text), page_style(&DARK)),
                    page_style(&DARK),
                    20,
                    0,
                    0,
                )
            })
            .collect::<Vec<_>>();
        let layout = TranscriptLayout {
            page_style: page_style(&DARK),
            total_height: stacked_height(blocks.iter().map(RenderBlock::height)),
            width: 20,
            blocks: blocks.into_iter().map(LayoutBlock::Fresh).collect(),
            footer: None,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        layout.render(buffer.area, 2, &mut buffer);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("visible"));
        assert!(!rendered.contains("older"));
        assert!(!rendered.contains("newer"));
    }

    #[test]
    fn rendering_attaches_progress_gaps_to_live_and_reliable_blocks() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, true);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "live".to_owned(),
                },
            },
            false,
        );
        assert!(rendered_text(&transcript).contains("display gap"));

        transcript.apply(completed_execution(1, 1, "done()", "", Vec::new()), false);
        assert!(!rendered_text(&transcript).contains("display gap"));
        transcript.apply(final_message(1, 2, "settled", Vec::new(), true), false);
        assert!(rendered_text(&transcript).contains("display gap"));
    }

    #[test]
    fn code_step_renders_path_free_tool_calls_between_code_and_output() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "await read('live-path')".to_owned(),
                },
            },
            false,
        );
        let expected_calls = vec![
            ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(1),
                subject: None,
                duration: None,
            },
            ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Failed,
                subject: None,
                duration: None,
            },
            ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Pending,
                subject: None,
                duration: None,
            },
        ];
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "await read('authoritative-path')",
                "host error message",
                expected_calls.clone(),
            ),
            false,
        );

        let block = transcript
            .blocks()
            .into_iter()
            .find(|block| matches!(block, TranscriptBlock::Arena(..)))
            .expect("settled arena block");
        let TranscriptBlock::Arena(arena) = &block else {
            unreachable!("arena block checked");
        };
        assert_eq!(arena.tool_calls, expected_calls);
        let NormalisedBlock::Arena(arena) = block.normalise() else {
            unreachable!("arena block checked");
        };
        assert_eq!(arena.tool_calls, expected_calls);

        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 80)),
            [
                "",
                "await read('authoritative-path')",
                "",
                "\u{2713} read \u{b7} 1 B",
                "\u{2717} read \u{b7} failed",
                "\u{2026} read",
                "",
                "host error message",
                "result",
                "result",
                "",
            ]
        );
    }

    #[test]
    fn code_step_collapses_tool_calls_independently_and_ctrl_o_reveals_all_rows() {
        let mut transcript = Transcript::default();
        let source = (1..=33)
            .map(|line| format!("code {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stdout = (1..=17)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tool_calls = (1..=20)
            .map(|count| ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(count),
                subject: None,
                duration: None,
            })
            .collect();
        transcript.apply(
            completed_execution_with_tool_calls(1, 1, &source, &stdout, tool_calls),
            false,
        );

        let collapsed = rendered_buffer(&mut transcript, 80);
        let mut expected = vec![String::new()];
        expected.extend((1..=10).map(|line| format!("code {line}")));
        expected.push("\u{2026} 23 more lines \u{b7} Ctrl-O to expand".to_owned());
        expected.push(String::new());
        expected.push("\u{2026} 12 earlier calls \u{b7} Ctrl-O to expand".to_owned());
        expected.extend((13..=20).map(|count| format!("\u{2713} read \u{b7} {count} B")));
        expected.push(String::new());
        expected.push("\u{2026} 10 earlier lines \u{b7} Ctrl-O to expand".to_owned());
        expected.extend((11..=17).map(|line| format!("output {line}")));
        expected.extend(["result", "result", ""].map(str::to_owned));
        assert_eq!(trimmed_rows(&collapsed), expected);
        let hint = cell_at(&collapsed, "\u{2026} 12 earlier calls");
        assert_eq!(hint.fg, DARK.dim);
        assert_eq!(hint.bg, DARK.tool_success_bg);

        assert!(transcript.toggle_arena_expansion());
        let expanded = trimmed_rows(&rendered_buffer(&mut transcript, 80));
        assert!(!expanded.iter().any(|row| row.contains("Ctrl-O")));
        assert!(expanded.contains(&"code 33".to_owned()));
        assert!(expanded.contains(&"output 1".to_owned()));
        assert_eq!(
            expanded
                .iter()
                .filter(|row| row.starts_with("\u{2713} read"))
                .count(),
            20
        );
    }

    #[test]
    fn code_step_wraps_and_scrolls_tool_call_lines() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "pass",
                "",
                vec![ToolCall {
                    tool: ToolName::Grep,
                    summary: ToolCallSummary::Count(usize::MAX),
                    subject: None,
                    duration: None,
                }],
            ),
            false,
        );
        transcript.toggle_arena_expansion();

        let full = rendered_buffer(&mut transcript, 16);
        assert_eq!(
            trimmed_rows(&full),
            [
                "",
                "pass",
                "",
                "\u{2713} grep \u{b7}",
                "18446744073709",
                "551615 matches",
                "",
                "result",
                "result",
                "",
            ]
        );
        let call_row = row_containing(&full, "\u{2713} grep");
        let layout = transcript.layout(&DARK, 16);
        let mut clipped = Buffer::empty(Rect::new(0, 0, 16, 2));
        layout.render(clipped.area, call_row + 1, &mut clipped);
        assert_eq!(trimmed_rows(&clipped), ["18446744073709", "551615 matches"]);
        assert_eq!(clipped[(0, 0)].symbol(), " ");
        assert_ne!(clipped[(1, 0)].symbol(), " ");
        assert_row_background(&clipped, 0, DARK.tool_success_bg);
    }

    #[test]
    fn cards_use_pi_palette_and_full_width_fills() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "inspect".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(
                1,
                1,
                "def answer():\n    return 42",
                "reasoning",
                Vec::new(),
            ),
            false,
        );

        let layout = transcript.layout(&DARK, 40);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, layout.total_height() as u16));
        layout.render(buffer.area, 0, &mut buffer);

        let user = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "i" && cell.bg == DARK.user_message_bg)
            .expect("user request cell");
        assert_eq!(user.bg, DARK.user_message_bg);
        assert_eq!(buffer[(0, 0)].bg, DARK.user_message_bg);

        let code_top = row_containing(&buffer, "def answer");
        for row in code_top - 1..usize::from(buffer.area.height) {
            assert_row_background(&buffer, row, DARK.tool_success_bg);
        }
        assert_eq!(buffer[(1, code_top as u16)].symbol(), "d");
        assert_eq!(buffer[(1, code_top as u16)].fg, DARK.syntax_keyword);
    }

    #[test]
    fn reasoning_card_uses_italic_thinking_text_on_card_background() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "answer()", "reasoning", Vec::new()),
            false,
        );
        let buffer = rendered_buffer(&mut transcript, 40);
        let row = row_containing(&buffer, "reasoning");
        for column in 0..buffer.area.width {
            let cell = &buffer[(column, row as u16)];
            assert_eq!(cell.fg, DARK.thinking_text);
            assert_eq!(cell.bg, DARK.card_bg);
            assert!(cell.modifier.contains(Modifier::ITALIC));
        }
    }

    #[test]
    fn code_step_collapses_long_source_to_its_head() {
        let mut short = Transcript::default();
        let short_source = (1..=10)
            .map(|line| format!("short code line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        short.apply(
            completed_execution(1, 1, &format!("{short_source}\n"), "", Vec::new()),
            false,
        );
        assert!(!rendered_layout_text(&mut short).contains("Ctrl-O"));

        let mut transcript = Transcript::default();
        let source = (1..=11)
            .map(|line| format!("code line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(completed_execution(1, 1, &source, "", Vec::new()), false);

        let collapsed = trimmed_rows(&rendered_buffer(&mut transcript, 80));
        let mut expected = vec![String::new()];
        expected.extend((1..=10).map(|line| format!("code line {line:02}")));
        expected.push("\u{2026} 1 more line \u{b7} Ctrl-O to expand".to_owned());
        assert_eq!(collapsed[..12], expected);

        assert!(transcript.toggle_arena_expansion());
        let expanded = trimmed_rows(&rendered_buffer(&mut transcript, 80));
        let mut expected = vec![String::new()];
        expected.extend((1..=11).map(|line| format!("code line {line:02}")));
        expected.push(String::new());
        assert_eq!(expanded[..13], expected);
    }

    #[test]
    fn code_step_hints_after_an_unclosed_source_head() {
        let mut transcript = Transcript::default();
        let source = (1..=9)
            .map(|line| format!("line_{line} = {line}"))
            .chain(std::iter::once("unterminated = \"\"\"".to_owned()))
            .chain(
                ["hidden = 11", "still_hidden = 12"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(completed_execution(1, 1, &source, "", Vec::new()), false);

        let buffer = rendered_buffer(&mut transcript, 80);
        let rows = trimmed_rows(&buffer);
        assert_eq!(
            rows[10..12],
            [
                "unterminated = \"\"\"",
                "\u{2026} 2 more lines \u{b7} Ctrl-O to expand"
            ]
        );
        assert!(!rows.iter().any(|row| row.contains("hidden")));
        assert_eq!(cell_at(&buffer, "\u{2026} 2 more lines").fg, DARK.dim);
    }

    #[test]
    fn code_step_collapses_output_tail_across_sections() {
        let mut short = Transcript::default();
        short.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(
                                &(1..=8)
                                    .map(|line| format!("short out {line}"))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
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
            },
            false,
        );
        assert!(!rendered_layout_text(&mut short).contains("Ctrl-O"));

        let mut transcript = Transcript::default();
        let stdout = (1..=17)
            .map(|line| format!("out {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(&stdout),
                            stderr: observation("stderr-body"),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("done")),
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
            false,
        );

        let collapsed = trimmed_rows(&rendered_buffer(&mut transcript, 80));
        let mut expected = [
            "",
            "pass",
            "",
            "\u{2026} 11 earlier lines \u{b7} Ctrl-O to expand",
        ]
        .map(str::to_owned)
        .to_vec();
        expected.extend((12..=17).map(|line| format!("out {line:02}")));
        expected.extend(["stderr", "stderr-body", "result", "done", ""].map(str::to_owned));
        assert_eq!(collapsed, expected);

        assert!(transcript.toggle_arena_expansion());
        let expanded = trimmed_rows(&rendered_buffer(&mut transcript, 80));
        let mut expected = ["", "pass", ""].map(str::to_owned).to_vec();
        expected.extend((1..=17).map(|line| format!("out {line:02}")));
        expected.extend(["stderr", "stderr-body", "result", "done", ""].map(str::to_owned));
        assert_eq!(expanded, expected);
    }

    #[test]
    fn code_step_drops_fully_omitted_output_sections() {
        let stdout = (1..=4)
            .map(|line| format!("stdout {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stderr = (1..=20)
            .map(|line| format!("stderr {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(&stdout),
                            stderr: observation(&stderr),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("result-tail")),
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
            false,
        );

        let collapsed = trimmed_rows(&rendered_buffer(&mut transcript, 80));
        let mut expected = [
            "",
            "pass",
            "",
            "\u{2026} 17 earlier lines \u{b7} Ctrl-O to expand",
            "stderr",
        ]
        .map(str::to_owned)
        .to_vec();
        expected.extend((14..=20).map(|line| format!("stderr {line:02}")));
        expected.extend(["result", "result-tail", ""].map(str::to_owned));
        assert_eq!(collapsed, expected);
    }

    fn trimmed_rows(buffer: &Buffer) -> Vec<String> {
        (0..usize::from(buffer.area.height))
            .map(|row| row_text(buffer, row).trim().to_owned())
            .collect()
    }

    fn cell_at(buffer: &Buffer, text: &str) -> ratatui::buffer::Cell {
        let (column, row) = (0..buffer.area.height)
            .find_map(|row| {
                let line = row_text(buffer, usize::from(row));
                line.find(text).map(|byte| {
                    let column = line[..byte].chars().count();
                    (u16::try_from(column).unwrap(), row)
                })
            })
            .unwrap_or_else(|| panic!("{text} rendered"));
        buffer[(column, row)].clone()
    }

    #[test]
    fn code_step_is_one_frameless_tinted_block() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "files = tools.glob('*')\nprint(files)",
                "a.rs\n",
                vec![
                    ToolCall {
                        tool: ToolName::Glob,
                        summary: ToolCallSummary::Count(12),
                        subject: None,
                        duration: None,
                    },
                    ToolCall {
                        tool: ToolName::Run,
                        summary: ToolCallSummary::Exit(0),
                        subject: None,
                        duration: None,
                    },
                ],
            ),
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 40);

        assert_eq!(
            trimmed_rows(&buffer),
            [
                "",
                "files = tools.glob('*')",
                "print(files)",
                "",
                "\u{2713} glob \u{b7} 12 entries",
                "\u{2713} run \u{b7} exit 0",
                "",
                "a.rs",
                "result",
                "result",
                "",
            ]
        );
        for row in 0..buffer.area.height {
            assert_row_background(&buffer, usize::from(row), DARK.tool_success_bg);
        }
        assert_eq!(buffer[(1, 1)].symbol(), "f");
    }

    #[test]
    fn tool_call_lines_carry_coloured_status_glyphs() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "pass",
                "",
                vec![
                    ToolCall {
                        tool: ToolName::Read,
                        summary: ToolCallSummary::Count(3),
                        subject: None,
                        duration: None,
                    },
                    ToolCall {
                        tool: ToolName::Write,
                        summary: ToolCallSummary::Failed,
                        subject: None,
                        duration: None,
                    },
                    ToolCall {
                        tool: ToolName::Run,
                        summary: ToolCallSummary::Exit(2),
                        subject: None,
                        duration: None,
                    },
                    ToolCall {
                        tool: ToolName::Run,
                        summary: ToolCallSummary::Signal,
                        subject: None,
                        duration: None,
                    },
                ],
            ),
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 40);

        for (line, glyph) in [
            ("\u{2713} read \u{b7} 3 B", DARK.success),
            ("\u{2717} write \u{b7} failed", DARK.error),
            ("\u{2717} run \u{b7} exit 2", DARK.error),
            ("\u{2717} run \u{b7} signal", DARK.error),
        ] {
            assert_eq!(cell_at(&buffer, line).fg, glyph, "{line}");
        }
        let name = cell_at(&buffer, "read \u{b7}");
        assert_eq!(name.fg, DARK.accent);
        assert_eq!(cell_at(&buffer, "3 B").fg, DARK.muted);
    }

    fn call(
        tool: ToolName,
        summary: ToolCallSummary,
        subject: Option<&str>,
        millis: Option<u64>,
    ) -> ToolCall {
        ToolCall {
            tool,
            summary,
            subject: subject.map(str::to_owned),
            duration: millis.map(std::time::Duration::from_millis),
        }
    }

    #[test]
    fn call_lines_align_names_and_show_subject_result_and_duration() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "pass",
                "",
                vec![
                    call(
                        ToolName::Read,
                        ToolCallSummary::Count(84_123),
                        Some("src/transcript.rs"),
                        Some(4),
                    ),
                    call(
                        ToolName::Grep,
                        ToolCallSummary::Count(12),
                        Some("\"fn tool_call\" src/"),
                        Some(31),
                    ),
                    call(
                        ToolName::StartSubagent,
                        ToolCallSummary::Subagent("c1".to_owned()),
                        Some("subagent:c1"),
                        Some(1),
                    ),
                    call(
                        ToolName::Run,
                        ToolCallSummary::Exit(101),
                        Some("just test"),
                        Some(44_900),
                    ),
                ],
            ),
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 80);
        let rows = trimmed_rows(&buffer);

        for row in [
            "\u{2713} read            src/transcript.rs \u{b7} 84 KB  4ms",
            "\u{2713} grep            \"fn tool_call\" src/ \u{b7} 12 matches  31ms",
            "\u{2713} start_subagent  subagent:c1  1ms",
            "\u{2717} run             just test \u{b7} exit 101  44.9s",
        ] {
            assert!(rows.contains(&row.to_owned()), "{row}\n{rows:#?}");
        }
        assert_eq!(cell_at(&buffer, "read   ").fg, DARK.accent);
        assert_eq!(cell_at(&buffer, "src/transcript.rs").fg, DARK.text);
        assert_eq!(cell_at(&buffer, "84 KB").fg, DARK.muted);
        assert_eq!(cell_at(&buffer, "44.9s").fg, DARK.dim);
    }

    #[test]
    fn call_line_subject_truncates_to_keep_its_result_until_ctrl_o() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "pass",
                "",
                vec![call(
                    ToolName::Run,
                    ToolCallSummary::Exit(0),
                    Some("just test 2>&1 | grep -E 'test result'"),
                    Some(2_000),
                )],
            ),
            false,
        );

        let collapsed = trimmed_rows(&rendered_buffer(&mut transcript, 34));
        assert!(
            collapsed.contains(&"\u{2713} run  just test\u{2026} \u{b7} exit 0  2.0s".to_owned()),
            "{collapsed:#?}"
        );

        transcript.toggle_arena_expansion();
        let expanded = trimmed_rows(&rendered_buffer(&mut transcript, 34));
        assert!(
            expanded
                .join(" ")
                .contains("just test 2>&1 | grep -E 'test result'"),
            "{expanded:#?}"
        );
    }

    #[test]
    fn calls_without_subjects_render_as_before() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "pass",
                "",
                vec![
                    call(ToolName::Read, ToolCallSummary::Count(512), None, None),
                    call(
                        ToolName::StartSubagent,
                        ToolCallSummary::Subagent("c1".to_owned()),
                        None,
                        None,
                    ),
                ],
            ),
            false,
        );

        let rows = trimmed_rows(&rendered_buffer(&mut transcript, 40));

        assert!(
            rows.contains(&"\u{2713} read \u{b7} 512 B".to_owned()),
            "{rows:#?}"
        );
        assert!(
            rows.contains(&"\u{2713} start_subagent \u{b7} subagent:c1".to_owned()),
            "{rows:#?}"
        );
    }

    #[test]
    fn sizes_and_durations_read_at_a_glance() {
        for (bytes, text) in [
            (0, "0 B"),
            (999, "999 B"),
            (1_000, "1.0 KB"),
            (1_234, "1.2 KB"),
            (9_960, "10 KB"),
            (84_123, "84 KB"),
            (999_600, "1.0 MB"),
            (3_400_000, "3.4 MB"),
            (12_000_000_000_000, "12000 GB"),
        ] {
            assert_eq!(super::format_size(bytes), text, "{bytes}");
        }
        for (millis, text) in [
            (0, "0ms"),
            (999, "999ms"),
            (1_000, "1.0s"),
            (44_900, "44.9s"),
            (59_999, "60.0s"),
            (60_000, "1m00s"),
            (125_000, "2m05s"),
        ] {
            assert_eq!(
                super::format_call_duration(std::time::Duration::from_millis(millis)),
                text,
                "{millis}"
            );
        }
    }

    #[test]
    fn run_call_reads_the_same_while_pending_and_shows_its_tail() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let token = ToolCallToken::for_test(0);
        let update = |transcript: &mut Transcript, summary, millis| {
            transcript.apply(
                RuntimeEvent::ToolUpdated {
                    run_id: RunId(1),
                    step_id: ModelStepId(1),
                    token,
                    call: call(ToolName::Run, summary, Some("cargo test"), millis),
                },
                false,
            )
        };
        update(&mut transcript, ToolCallSummary::Pending, None);
        apply_run_preview(&mut transcript, token, "running 2 tests\nok\n");

        let buffer = rendered_buffer(&mut transcript, 40);
        let live = trimmed_rows(&buffer);
        assert!(
            live.contains(&"\u{2026} run  cargo test".to_owned()),
            "{live:?}"
        );
        assert!(live.contains(&"running 2 tests".to_owned()), "{live:?}");
        assert_eq!(cell_at(&buffer, "\u{2026} run").fg, DARK.warning);
        assert_eq!(cell_at(&buffer, "running 2 tests").fg, DARK.dim);
        assert_eq!(
            buffer[(
                3,
                u16::try_from(row_containing(&buffer, "running 2 tests")).unwrap()
            )]
                .symbol(),
            "r"
        );

        update(&mut transcript, ToolCallSummary::Exit(0), Some(1_500));

        let settled = trimmed_rows(&rendered_buffer(&mut transcript, 40));
        assert!(
            settled.contains(&"\u{2713} run  cargo test \u{b7} exit 0  1.5s".to_owned()),
            "{settled:?}"
        );
        assert!(!settled.iter().any(|row| row.contains("running 2 tests")));
    }

    #[test]
    fn live_code_step_leaves_activity_to_the_status_line() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        apply_run_tool_update(
            &mut transcript,
            ToolCallToken::for_test(0),
            ToolCallSummary::Pending,
        );

        let rows = trimmed_rows(&rendered_buffer(&mut transcript, 40));

        assert_eq!(rows, ["", "\u{2026} run", ""]);
    }

    #[test]
    fn output_other_than_stdout_is_labelled_and_coloured() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("out\n"),
                            stderr: observation("careful\n"),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: "Traceback".to_owned(),
                                omitted_bytes: 0,
                            }),
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
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 40);

        assert_eq!(
            trimmed_rows(&buffer),
            [
                "",
                "pass",
                "",
                "out",
                "stderr",
                "careful",
                "error",
                "Traceback",
                ""
            ]
        );
        assert_eq!(cell_at(&buffer, "out").fg, DARK.text);
        assert_eq!(cell_at(&buffer, "stderr").fg, DARK.dim);
        assert_eq!(cell_at(&buffer, "careful").fg, DARK.warning);
        assert_eq!(cell_at(&buffer, "error").fg, DARK.dim);
        assert_eq!(cell_at(&buffer, "Traceback").fg, DARK.error);
        assert_row_background(&buffer, 0, DARK.tool_error_bg);
    }

    #[test]
    fn collapsed_code_step_hints_ctrl_o_at_each_cut() {
        let source = (1..=12)
            .map(|line| format!("x{line} = {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stdout = (1..=10)
            .map(|line| format!("out {line}\n"))
            .collect::<String>();
        let calls = (0..10)
            .map(|_| ToolCall {
                tool: ToolName::Read,
                summary: ToolCallSummary::Count(1),
                subject: None,
                duration: None,
            })
            .collect();
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution_with_tool_calls(1, 1, &source, &stdout, calls),
            false,
        );

        let rows = trimmed_rows(&rendered_buffer(&mut transcript, 50));

        let read = "\u{2713} read \u{b7} 1 B";
        let mut expected = vec!["".to_owned()];
        expected.extend((1..=10).map(|line| format!("x{line} = {line}")));
        expected.push("\u{2026} 2 more lines \u{b7} Ctrl-O to expand".to_owned());
        expected.push(String::new());
        expected.push("\u{2026} 2 earlier calls \u{b7} Ctrl-O to expand".to_owned());
        expected.extend((0..8).map(|_| read.to_owned()));
        expected.push(String::new());
        expected.push("\u{2026} 3 earlier lines \u{b7} Ctrl-O to expand".to_owned());
        expected.extend((4..=10).map(|line| format!("out {line}")));
        expected.extend(["result", "result", ""].map(str::to_owned));
        assert_eq!(rows, expected);

        transcript.toggle_arena_expansion();
        let expanded = trimmed_rows(&rendered_buffer(&mut transcript, 50));
        assert!(!expanded.iter().any(|row| row.contains("Ctrl-O")));
        assert_eq!(expanded.iter().filter(|row| *row == read).count(), 10);
    }

    #[test]
    fn failed_code_step_leads_with_its_diagnostic_in_error_colour() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::FailedRun {
                        message: "boom".to_owned(),
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Execution(Box::new(
                                ExecutionEvidence {
                                    commentary: None,
                                    source: "pass".to_owned(),
                                    reasoning_summary: String::new(),
                                    began: true,
                                    stdout: observation(""),
                                    stderr: observation(""),
                                    tool_calls: Vec::new(),
                                    read_paths: Vec::new(),
                                    modified_paths: Vec::new(),
                                    failure: None,
                                    cancellation_warning: None,
                                },
                            ))),
                            attempts: Vec::new(),
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 40);
        let rows = trimmed_rows(&buffer);

        assert_eq!(
            rows[1..5],
            ["failed: boom", "execution: started", "", "pass"]
        );
        assert_eq!(cell_at(&buffer, "failed: boom").fg, DARK.error);
        assert_eq!(cell_at(&buffer, "execution: started").fg, DARK.dim);
    }

    #[test]
    fn code_step_omits_empty_groups() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
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
            },
            false,
        );

        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 40)),
            ["", "pass", ""]
        );
    }

    #[test]
    fn code_step_shows_the_pending_run_tail_and_ignores_stale_previews() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let token = ToolCallToken::for_test(0);
        apply_run_tool_update(&mut transcript, token, ToolCallSummary::Pending);
        let stdout = (3..=10)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        apply_run_preview(&mut transcript, token, &stdout);

        let mut expected = vec![String::new(), "\u{2026} run".to_owned()];
        expected.extend((3..=10).map(|line| format!("line {line}")));
        expected.push(String::new());
        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 80)),
            expected
        );

        apply_run_tool_update(&mut transcript, token, ToolCallSummary::Exit(0));
        transcript.apply(
            RuntimeEvent::RunPreviewUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token,
                preview: RunPreview {
                    stdout: "stale output".to_owned(),
                },
            },
            true,
        );
        apply_run_tool_update(&mut transcript, token, ToolCallSummary::Pending);

        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 80)),
            [
                "",
                "display gap: some live progress was dropped",
                "",
                "\u{2713} run \u{b7} exit 0",
                "",
            ]
        );
        assert_eq!(transcript.status(), "run · exit 0");
    }

    #[test]
    fn live_run_events_upsert_delayed_and_duplicate_updates() {
        let mut transcript = Transcript::default();
        assert!(transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false));
        let token = ToolCallToken::for_test(0);

        assert!(apply_run_preview(&mut transcript, token, "first output"));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Pending,
                subject: None,
                duration: None,
            }],
            &[Some(RunPreview {
                stdout: "first output".to_owned(),
            })],
        );
        assert!(rendered_text(&transcript).contains("first output"));

        assert!(apply_run_preview(&mut transcript, token, "latest output"));
        assert!(!apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Pending
        ));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Pending,
                subject: None,
                duration: None,
            }],
            &[Some(RunPreview {
                stdout: "latest output".to_owned(),
            })],
        );

        assert!(!apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Pending
        ));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Pending,
                subject: None,
                duration: None,
            }],
            &[Some(RunPreview {
                stdout: "latest output".to_owned(),
            })],
        );

        assert!(apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Exit(0)
        ));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Exit(0),
                subject: None,
                duration: None,
            }],
            &[Some(RunPreview {
                stdout: "latest output".to_owned(),
            })],
        );
        let status = transcript.status().to_owned();

        assert!(!apply_run_preview(&mut transcript, token, "stale output"));
        assert!(!apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Pending
        ));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Exit(0),
                subject: None,
                duration: None,
            }],
            &[Some(RunPreview {
                stdout: "latest output".to_owned(),
            })],
        );
        assert_eq!(transcript.status(), status);
    }

    #[test]
    fn live_tool_rows_upsert_by_token_in_admission_order() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let first = ToolCallToken::for_test(0);
        let second = ToolCallToken::for_test(1);

        assert!(transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token: second,
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(1),
                    subject: None,
                    duration: None,
                },
            },
            false,
        ));
        assert!(transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token: first,
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
            },
            false,
        ));

        assert_live_run_rows(
            &transcript,
            &[
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(1),
                    subject: None,
                    duration: None,
                },
            ],
            &[None, None],
        );
    }

    #[test]
    fn live_terminal_run_updates_create_and_upsert_rows() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let token = ToolCallToken::for_test(0);

        assert!(apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Exit(7)
        ));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Exit(7),
                subject: None,
                duration: None,
            }],
            &[None],
        );
        assert_eq!(transcript.status(), "run · exit 7");

        assert!(!apply_run_preview(&mut transcript, token, "stale output"));
        assert!(!apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Exit(7)
        ));
        assert_live_run_rows(
            &transcript,
            &[ToolCall {
                tool: ToolName::Run,
                summary: ToolCallSummary::Exit(7),
                subject: None,
                duration: None,
            }],
            &[None],
        );
    }

    #[test]
    fn reliable_run_settlement_rejects_delayed_step_lifecycle_progress() {
        let mut transcript = Transcript::default();
        let token = ToolCallToken::for_test(0);

        assert!(apply_run_tool_update(
            &mut transcript,
            token,
            ToolCallSummary::Exit(0)
        ));
        assert!(!transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false));
        assert!(!transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
            },
            false,
        ));
        assert!(!transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "stale()".to_owned(),
                },
            },
            false,
        ));
        assert!(!transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        ));
        assert_eq!(transcript.status(), "run · exit 0");
        let rendered = rendered_text(&transcript);
        assert!(rendered.contains("run · exit 0"));
        assert!(!rendered.contains("stale()"));
    }

    #[test]
    fn mixed_tool_rows_keep_admission_order_when_run_settles_first() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let read = ToolCallToken::for_test(0);
        let run = ToolCallToken::for_test(1);

        assert!(apply_run_tool_update(
            &mut transcript,
            run,
            ToolCallSummary::Exit(0)
        ));
        assert!(transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token: read,
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
            },
            false,
        ));
        assert_live_run_rows(
            &transcript,
            &[
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                    subject: None,
                    duration: None,
                },
            ],
            &[None, None],
        );
    }

    #[test]
    fn concurrent_run_rows_keep_admission_order_when_later_call_settles_first() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let first = ToolCallToken::for_test(0);
        let second = ToolCallToken::for_test(1);

        assert!(apply_run_tool_update(
            &mut transcript,
            second,
            ToolCallSummary::Exit(0)
        ));
        assert!(apply_run_preview(&mut transcript, first, "first output"));
        assert_live_run_rows(
            &transcript,
            &[
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                    subject: None,
                    duration: None,
                },
            ],
            &[
                Some(RunPreview {
                    stdout: "first output".to_owned(),
                }),
                None,
            ],
        );
    }

    #[test]
    fn concurrent_live_run_previews_settle_independently() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let first = ToolCallToken::for_test(0);
        let second = ToolCallToken::for_test(1);

        assert!(apply_run_preview(&mut transcript, first, "first output"));
        assert!(apply_run_preview(&mut transcript, second, "second output"));
        assert!(apply_run_tool_update(
            &mut transcript,
            first,
            ToolCallSummary::Exit(0)
        ));
        assert_live_run_rows(
            &transcript,
            &[
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                    subject: None,
                    duration: None,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
            ],
            &[
                Some(RunPreview {
                    stdout: "first output".to_owned(),
                }),
                Some(RunPreview {
                    stdout: "second output".to_owned(),
                }),
            ],
        );

        assert!(apply_run_preview(
            &mut transcript,
            second,
            "second latest output"
        ));
        assert!(apply_run_tool_update(
            &mut transcript,
            second,
            ToolCallSummary::Exit(1)
        ));
        assert_live_run_rows(
            &transcript,
            &[
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                    subject: None,
                    duration: None,
                },
                ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(1),
                    subject: None,
                    duration: None,
                },
            ],
            &[
                Some(RunPreview {
                    stdout: "first output".to_owned(),
                }),
                Some(RunPreview {
                    stdout: "second latest output".to_owned(),
                }),
            ],
        );
    }

    #[test]
    fn completed_execution_replaces_live_run_previews() {
        let mut transcript = Transcript::default();
        start_live_run(&mut transcript);
        let token = ToolCallToken::for_test(0);

        assert!(apply_run_preview(&mut transcript, token, "live output"));
        assert!(rendered_text(&transcript).contains("live output"));
        assert!(transcript.apply(
            completed_execution_with_tool_calls(
                1,
                1,
                "authoritative()",
                "authoritative output",
                vec![ToolCall {
                    tool: ToolName::Run,
                    summary: ToolCallSummary::Exit(0),
                    subject: None,
                    duration: None,
                }],
            ),
            false,
        ));

        let retained = rendered_text(&transcript);
        assert!(retained.contains("\u{2713} run · exit 0"));
        assert!(!retained.contains("live output"));
        assert!(transcript.live_step.is_none());
        assert!(transcript.apply(RuntimeEvent::RunSettled { run_id: RunId(1) }, false));
        assert!(transcript.is_idle());

        assert!(!apply_run_preview(&mut transcript, token, "stale output"));
        assert_eq!(rendered_text(&transcript), retained);
    }

    #[test]
    fn live_code_step_shows_settled_reads_and_stdout_preview() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "pass".to_owned(),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token: ToolCallToken::for_test(0),
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::StdoutDelta {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                text: "live output".to_owned(),
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::StdoutTruncated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                token: ToolCallToken::for_test(0),
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(12),
                    subject: None,
                    duration: None,
                },
            },
            false,
        );

        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 80)),
            [
                "",
                "pass",
                "",
                "\u{2713} read \u{b7} 12 B",
                "",
                "live output",
                "stdout preview",
                "truncated",
                "",
            ]
        );
    }

    #[test]
    fn live_tool_updates_target_their_opaque_trace_tokens() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        );
        for token in [ToolCallToken::for_test(0), ToolCallToken::for_test(1)] {
            transcript.apply(
                RuntimeEvent::ToolUpdated {
                    run_id: RunId(1),
                    step_id: ModelStepId(1),
                    token,
                    call: ToolCall {
                        tool: ToolName::Read,
                        summary: ToolCallSummary::Pending,
                        subject: None,
                        duration: None,
                    },
                },
                false,
            );
        }
        for (token, summary) in [
            (ToolCallToken::for_test(0), ToolCallSummary::Count(5)),
            (ToolCallToken::for_test(1), ToolCallSummary::Failed),
        ] {
            transcript.apply(
                RuntimeEvent::ToolUpdated {
                    run_id: RunId(1),
                    step_id: ModelStepId(1),
                    token,
                    call: ToolCall {
                        subject: None,
                        duration: None,
                        tool: ToolName::Read,
                        summary,
                    },
                },
                false,
            );
        }

        assert_eq!(
            transcript
                .live_step
                .as_ref()
                .expect("tool updates create a live step")
                .tool_calls,
            [
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(5),
                    subject: None,
                    duration: None,
                },
                ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Failed,
                    subject: None,
                    duration: None,
                },
            ]
        );
    }

    #[test]
    fn code_step_wraps_wide_source_and_clips_directly() {
        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "print('日本🦀日本')", "", Vec::new()),
            false,
        );

        let full = rendered_buffer(&mut transcript, 12);
        assert_eq!(
            trimmed_rows(&full)[..4],
            ["", "print('日", "本 🦀 日 本 ')", ""]
        );

        let layout = transcript.layout(&DARK, 12);
        let mut clipped = Buffer::empty(Rect::new(0, 0, 12, 2));
        layout.render(clipped.area, 2, &mut clipped);
        assert_eq!(trimmed_rows(&clipped), ["本 🦀 日 本 ')", ""]);
        assert_eq!(clipped[(0, 0)].symbol(), " ");
        assert_row_background(&clipped, 0, DARK.tool_success_bg);
    }

    #[test]
    fn cards_degrade_nested_frames_without_losing_renderable_text() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "XYZ".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("QRS"),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("UVW")),
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
            false,
        );

        for width in 0..=8 {
            let layout = transcript.layout(&DARK, width);
            let height = u16::try_from(layout.total_height()).expect("test transcript height");
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
            layout.render(buffer.area, 0, &mut buffer);
            if width > 0 {
                for character in ['X', 'Y', 'Z', 'Q', 'R', 'S', 'U', 'V', 'W'] {
                    assert!(
                        buffer
                            .content
                            .iter()
                            .any(|cell| cell.symbol() == character.to_string()),
                        "width {width} lost {character}"
                    );
                }
            }
        }
    }

    #[test]
    fn cards_map_live_completed_and_terminal_evidence_to_typed_states() {
        let mut live = Transcript::default();
        live.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        live.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "live_source".to_owned(),
                },
            },
            false,
        );
        let live_buffer = rendered_buffer(&mut live, 40);
        let live_row = row_containing(&live_buffer, "live_source");
        for row in [live_row - 1, live_row, live_row + 1] {
            assert_row_background(&live_buffer, row, DARK.tool_pending_bg);
        }

        let mut transcript = Transcript::default();
        transcript.apply(
            completed_execution(1, 1, "complete_source", "", Vec::new()),
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "python_error_source".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(""),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: "error".to_owned(),
                                omitted_bytes: 0,
                            }),
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
            false,
        );
        for (run_id, source, record) in [
            (
                3,
                "cancelled_source",
                Record::CancelledRun {
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                            commentary: None,
                            source: "cancelled_source".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation(""),
                            stderr: observation(""),
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            cancellation_warning: Some("cancelled".to_owned()),
                            failure: None,
                        }))),
                        attempts: Vec::new(),
                        overflow_recovery: None,
                    },
                },
            ),
            (
                4,
                "failed_source",
                Record::FailedRun {
                    message: "failed".to_owned(),
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                            commentary: None,
                            source: "failed_source".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation(""),
                            stderr: observation(""),
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            cancellation_warning: None,
                            failure: Some(ExecutionFailure::Arena {
                                message: "failed".to_owned(),
                            }),
                        }))),
                        attempts: Vec::new(),
                        overflow_recovery: None,
                    },
                },
            ),
        ] {
            transcript.apply(
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(run_id),
                        record,
                    },
                    progress_gap: false,
                },
                false,
            );
            assert!(rendered_layout_text(&mut transcript).contains(source));
        }
        let buffer = rendered_buffer(&mut transcript, 60);
        assert_row_background(
            &buffer,
            row_containing(&buffer, "complete_source"),
            DARK.tool_success_bg,
        );
        for source in ["python_error_source", "cancelled_source", "failed_source"] {
            assert_row_background(&buffer, row_containing(&buffer, source), DARK.tool_error_bg);
        }
        let error_label = trimmed_rows(&buffer)
            .iter()
            .position(|row| row == "error")
            .expect("error label");
        assert_row_background(&buffer, error_label, DARK.tool_error_bg);
        let mut decoded = Transcript::default();
        decoded.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(5),
                    record: Record::FailedRun {
                        message: "failed".to_owned(),
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Generation(GenerationEvidence {
                                reasoning_summary: String::new(),
                                reasoning: String::new(),
                                assistant_text: String::new(),
                                decoded_python: "terminal_decoded_source".to_owned(),
                            })),
                            attempts: Vec::new(),
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let decoded_buffer = rendered_buffer(&mut decoded, 60);
        let decoded_row = row_containing(&decoded_buffer, "terminal_decoded_source");
        for row in [decoded_row - 1, decoded_row, decoded_row + 1] {
            assert_row_background(&decoded_buffer, row, DARK.tool_error_bg);
        }
    }

    #[test]
    fn code_step_keeps_short_source_and_output_open_and_metadata_outside_budgets() {
        let mut transcript = Transcript::default();
        let source = (1..=10)
            .map(|line| format!("code {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stdout = (1..=8)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source,
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(&stdout),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::Complete { result: None },
                            tool_calls: Vec::new(),
                            read_paths: Vec::new(),
                            modified_paths: Vec::new(),
                            changed_bindings: Vec::new(),
                        },
                        attempts: vec![usage(1, 1)],
                    },
                },
                progress_gap: true,
            },
            false,
        );
        let mut expected = ["", "display gap: some live progress was dropped", ""]
            .map(str::to_owned)
            .to_vec();
        expected.extend((1..=10).map(|line| format!("code {line}")));
        expected.push(String::new());
        expected.extend((1..=8).map(|line| format!("output {line}")));
        expected.extend(["", "↑1 ↓1 R0 W0 r0 Σ2 · $0.000024", ""].map(str::to_owned));
        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 60)),
            expected
        );

        let mut started = Transcript::default();
        started.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        started.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                commentary: None,
            },
            false,
        );
        assert_eq!(trimmed_rows(&rendered_buffer(&mut started, 60)), ["", ""]);
    }

    #[test]
    fn code_step_preserves_output_order_and_labels() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("first\nsecond"),
                            stderr: observation("third"),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("fourth")),
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
            false,
        );
        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 80)),
            [
                "", "pass", "", "first", "second", "stderr", "third", "result", "fourth", ""
            ]
        );

        let mut interrupted = Transcript::default();
        interrupted.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Execution(Box::new(
                                ExecutionEvidence {
                                    commentary: None,
                                    source: "pass".to_owned(),
                                    reasoning_summary: String::new(),
                                    began: true,
                                    stdout: observation("output body"),
                                    stderr: observation("diagnostic body"),
                                    tool_calls: vec![ToolCall {
                                        tool: ToolName::Read,
                                        summary: ToolCallSummary::Failed,
                                        subject: None,
                                        duration: None,
                                    }],
                                    read_paths: Vec::new(),
                                    modified_paths: Vec::new(),
                                    failure: Some(ExecutionFailure::Arena {
                                        message: "failure".to_owned(),
                                    }),
                                    cancellation_warning: Some("warning".to_owned()),
                                },
                            ))),
                            attempts: Vec::new(),
                            overflow_recovery: None,
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut interrupted, 80)),
            [
                "",
                "cancelled",
                "execution: started",
                "",
                "pass",
                "",
                "\u{2717} read \u{b7} failed",
                "",
                "output body",
                "stderr",
                "diagnostic body",
                "failure",
                "failure",
                "warning",
                "warning",
                "",
            ]
        );
    }

    #[test]
    fn cards_keep_monty_retention_markers_when_expanded() {
        let mut transcript = Transcript::default();
        let mut stdout = observation(
            &(1..=17)
                .map(|line| format!("head {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        stdout.tail = Some(
            (1..=17)
                .map(|line| format!("tail {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        stdout.omitted_lines = 20;
        stdout.omitted_bytes = 200;
        stdout.total_bytes = stdout.head.len() + stdout.tail.as_ref().unwrap().len() + 200;
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        commentary: None,
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout,
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
            },
            false,
        );
        let mut collapsed = [
            "",
            "pass",
            "",
            "\u{2026} 27 earlier lines \u{b7} Ctrl-O to expand",
        ]
        .map(str::to_owned)
        .to_vec();
        collapsed.extend((10..=17).map(|line| format!("tail {line}")));
        collapsed.push(String::new());
        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 60)),
            collapsed
        );

        assert!(transcript.toggle_arena_expansion());
        let mut expanded = ["", "pass", ""].map(str::to_owned).to_vec();
        expanded.extend((1..=17).map(|line| format!("head {line}")));
        expanded.push("[20 lines, 200 bytes omitted]".to_owned());
        expanded.extend((1..=17).map(|line| format!("tail {line}")));
        expanded.push(String::new());
        assert_eq!(
            trimmed_rows(&rendered_buffer(&mut transcript, 60)),
            expanded
        );
    }

    #[test]
    fn cards_highlight_normalised_complete_and_incomplete_python_with_a_plain_fallback() {
        let mut complete = Transcript::default();
        complete.apply(
            completed_execution(1, 1, "def complete():\n\treturn 42", "", Vec::new()),
            false,
        );
        let complete_buffer = rendered_buffer(&mut complete, 60);
        assert!(rendered_layout_text(&mut complete).contains("    return 42"));
        let def = complete_buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "d" && cell.fg == DARK.syntax_keyword)
            .expect("highlighted def");
        assert_eq!(def.bg, DARK.tool_success_bg);

        let mut live = Transcript::default();
        live.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        live.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::DecodedPythonChunk {
                    code: "def incomplete(".to_owned(),
                },
            },
            false,
        );
        let live_buffer = rendered_buffer(&mut live, 60);
        assert!(live_buffer.content.iter().any(|cell| cell.symbol() == "d"
            && cell.fg == DARK.syntax_keyword
            && cell.bg == DARK.tool_pending_bg));

        let mut fallback = Transcript {
            highlighter: PythonHighlighter::fallback_for_test(),
            ..Transcript::default()
        };
        fallback.apply(
            completed_execution(3, 1, "def fallback(", "", Vec::new()),
            false,
        );
        let fallback_buffer = rendered_buffer(&mut fallback, 60);
        assert!(
            fallback_buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "d"
                    && cell.fg == DARK.text
                    && cell.bg == DARK.tool_success_bg)
        );
        assert!(rendered_layout_text(&mut fallback).contains("def fallback("));
    }

    #[test]
    fn cards_toggle_reasoning_and_all_arenas_without_changing_chronology() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let source = (1..=33)
            .map(|line| format!("code {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(
            completed_execution(1, 1, &source, "reasoning", Vec::new()),
            false,
        );
        transcript.apply(final_message(1, 2, "answer", Vec::new(), false), false);

        let automatic = rendered_layout_text(&mut transcript);
        assert!(
            automatic.contains("\u{2026} 23 more lines \u{b7} Ctrl-O to expand"),
            "{automatic}"
        );
        assert!(transcript.toggle_reasoning());
        let hidden = rendered_layout_text(&mut transcript);
        assert_eq!(
            hidden.matches("thinking hidden · Ctrl-T to show").count(),
            1
        );
        assert!(!hidden.contains("reasoning\n"));
        assert!(transcript.toggle_arena_expansion());
        let expanded = rendered_layout_text(&mut transcript);
        assert!(expanded.contains("code 33"));
        let request = expanded.find("request").unwrap();
        let placeholder = expanded.find("thinking hidden").unwrap();
        let arena = expanded.find("code 1 ").unwrap();
        let answer = expanded.find("answer").unwrap();
        assert!(request < placeholder && placeholder < arena && arena < answer);
    }

    #[test]
    fn cards_saturate_padding_without_losing_a_content_column() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "user".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(1, 1, "pass", "reasoning", Vec::new()),
            false,
        );

        for width in 1..=4 {
            let layout = transcript.layout_fresh(&DARK, width);
            assert!(
                layout
                    .blocks
                    .iter()
                    .filter_map(card)
                    .all(|card| card.left_padding.saturating_add(card.right_padding) < width)
            );
        }
    }

    #[test]
    fn projection_updates_status_through_live_and_settlement_events() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(3) }, false);
        assert_eq!(transcript.status(), "starting run");

        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                epoch: 0,
            },
            false,
        );
        assert_eq!(transcript.status(), "generating");
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                epoch: 0,
                progress: ModelProgress::AttemptStarted { attempt: 9 },
            },
            false,
        );
        assert_eq!(transcript.status(), "generating");
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                commentary: None,
            },
            false,
        );
        assert_eq!(transcript.status(), "running code");
        transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                token: ToolCallToken::for_test(0),
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Pending,
                    subject: None,
                    duration: None,
                },
            },
            false,
        );
        assert_eq!(transcript.status(), "read · pending");
        transcript.apply(
            RuntimeEvent::ToolUpdated {
                run_id: RunId(3),
                step_id: ModelStepId(8),
                token: ToolCallToken::for_test(0),
                call: ToolCall {
                    tool: ToolName::Read,
                    summary: ToolCallSummary::Count(12),
                    subject: None,
                    duration: None,
                },
            },
            false,
        );
        assert_eq!(transcript.status(), "read · 12 B");
        transcript.apply(
            RuntimeEvent::SubmissionRejectedBusy {
                active_run_id: Some(RunId(3)),
            },
            false,
        );
        assert_eq!(transcript.status(), "busy");

        assert!(transcript.apply(RuntimeEvent::RunSettled { run_id: RunId(3) }, false));
        assert!(transcript.is_idle());
        assert_eq!(transcript.status(), "idle");
    }

    #[test]
    fn feedback_removes_labels_and_keeps_accounting_with_its_step() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(97),
                    record: Record::UserRequest {
                        request: "inspect this".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(
                97,
                83,
                "inspect()",
                "reasoning",
                vec![
                    detailed_usage(Some(100), Some(20), Some(10), Some(5), Some(5), Some(120)),
                    detailed_usage(Some(0), None, Some(0), None, Some(0), Some(0)),
                    AttemptAccounting::UsageUnavailable,
                ],
            ),
            false,
        );
        transcript.apply(
            final_message(97, 84, "final answer", vec![usage(2, 1)], false),
            false,
        );

        let blocks = transcript.blocks();
        assert!(blocks.iter().any(|block| {
            matches!(
                block,
                TranscriptBlock::Arena(ArenaBlock {
                    heading: None,
                    accounting: Some(accounting),
                    ..
                }) if accounting == "3 attempts · ↑100 ↓20 R10 W5 r5 Σ120 · $0.000769"
            )
        }));
        assert!(blocks.iter().any(|block| {
            matches!(
                block,
                TranscriptBlock::Assistant {
                    markdown,
                    accounting: Some(accounting),
                } if markdown == "final answer" && accounting == "↑2 ↓1 R0 W0 r0 Σ3 · $0.000028"
            )
        }));

        let history = rendered_layout_text(&mut transcript);
        assert!(history.contains("inspect this"));
        assert!(!history.contains("you:"));
        assert!(!history.contains("[run 97 step 83]"));
        assert!(!history.contains("code:"));
        assert!(!history.contains("stdout:"));
        assert!(!history.contains("result:"));
        let buffer = rendered_buffer(&mut transcript, 96);
        let source_row = row_containing(&buffer, "inspect()");
        assert_eq!(
            trimmed_rows(&buffer)[source_row - 1..=source_row + 7],
            [
                "",
                "inspect()",
                "",
                "authoritative stdout",
                "result",
                "42",
                "",
                "3 attempts · ↑100 ↓20 R10 W5 r5 Σ120 · $0.000769",
                "",
            ]
        );
        assert!(row_containing(&buffer, "final answer") < row_containing(&buffer, "↑2 ↓1"));
        assert!(!history.contains("#1"));
        assert!(!history.contains("unavailable"));
    }

    #[test]
    fn feedback_live_final_text_owns_settled_accounting_once() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AttemptSettled {
                    attempt: 1,
                    accounting: usage(1, 1),
                },
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "live final".to_owned(),
                },
            },
            false,
        );

        assert!(transcript.blocks().iter().any(|block| {
            matches!(
                block,
                TranscriptBlock::Assistant {
                    markdown,
                    accounting: Some(accounting),
                } if markdown == "live final" && accounting == "↑1 ↓1 R0 W0 r0 Σ2 · $0.000024"
            )
        }));

        transcript.apply(
            final_message(1, 1, "settled final", vec![usage(1, 1)], false),
            false,
        );
        assert_eq!(
            rendered_layout_text(&mut transcript)
                .matches("↑1 ↓1 R0 W0 r0 Σ2 · $0.000024")
                .count(),
            1
        );
    }

    #[test]
    fn feedback_user_card_contains_only_the_request_when_progress_is_gapped() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: true,
            },
            false,
        );

        let blocks = transcript.blocks();
        assert!(matches!(blocks.as_slice(), [
            TranscriptBlock::User { request },
            TranscriptBlock::Metadata { .. },
        ] if request == "request"));
        let layout = transcript.layout_fresh(&DARK, 40);
        let user = card_with_text(&layout.blocks, "request");
        assert_eq!(user.height, 3);
    }

    #[test]
    fn feedback_cards_have_exact_padding_separation_and_accounting_alignment() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "user request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            false,
        );
        transcript.apply(
            completed_execution(1, 1, "pass", "reasoning", vec![usage(1, 1)]),
            false,
        );
        transcript.apply(
            final_message(1, 2, "assistant response", vec![usage(2, 1)], false),
            false,
        );

        let layout = transcript.layout_fresh(&DARK, 64);
        let reasoning = card_with_text(&layout.blocks, "reasoning");
        assert_eq!((reasoning.left_padding, reasoning.right_padding), (2, 2));
        assert_eq!((reasoning.top_padding, reasoning.bottom_padding), (1, 1));
        let assistant = card_with_text(&layout.blocks, "assistant response");
        assert_eq!((assistant.left_padding, assistant.right_padding), (0, 0));
        assert_eq!((assistant.top_padding, assistant.bottom_padding), (0, 0));

        let buffer = rendered_buffer(&mut transcript, 64);
        let user_row = row_containing(&buffer, "user request");
        assert_eq!(buffer[(1, user_row as u16)].symbol(), "u");
        assert_row_background(&buffer, user_row - 1, DARK.user_message_bg);
        assert_row_background(&buffer, user_row + 1, DARK.user_message_bg);

        let source_row = row_containing(&buffer, "pass");
        let stdout_row = row_containing(&buffer, "stdout");
        assert!(source_row < stdout_row);
        let result_row = row_containing(&buffer, "result");
        let execution_accounting = "↑1 ↓1 R0 W0 r0 Σ2 · $0.000024";
        let execution_accounting_row = row_containing(&buffer, execution_accounting);
        assert!(result_row < execution_accounting_row);
        assert!(
            buffer.content
                [(execution_accounting_row - 1) * 64 + 1..execution_accounting_row * 64 - 1]
                .iter()
                .all(|cell| cell.symbol() == " ")
        );
        let execution_accounting_cells =
            &buffer.content[execution_accounting_row * 64..(execution_accounting_row + 1) * 64];
        let execution_accounting_row_text = execution_accounting_cells
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(execution_accounting_row_text.starts_with(' '));
        assert!(execution_accounting_row_text.ends_with(&format!("{execution_accounting} ")));
        assert_row_background(&buffer, execution_accounting_row, DARK.tool_success_bg);

        let assistant_row = row_containing(&buffer, "assistant response");
        assert_eq!(buffer[(0, assistant_row as u16)].symbol(), "a");
        let final_accounting = "↑2 ↓1 R0 W0 r0 Σ3 · $0.000028";
        let final_accounting_row = row_containing(&buffer, final_accounting);
        assert_eq!(final_accounting_row, assistant_row + 2);
        let final_accounting_cells =
            &buffer.content[final_accounting_row * 64..(final_accounting_row + 1) * 64];
        let final_accounting_row_text = final_accounting_cells
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(
            final_accounting_row_text
                .trim_end()
                .ends_with(final_accounting)
        );
        assert!(final_accounting_row_text.starts_with("          "));
    }

    #[test]
    fn feedback_hides_identifiers_without_changing_stale_event_authority() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(73) }, false);
        transcript.apply(
            RuntimeEvent::ModelStepStarted {
                run_id: RunId(73),
                step_id: ModelStepId(91),
                epoch: 0,
            },
            false,
        );
        assert_eq!(transcript.status(), "generating");
        assert_eq!(rendered_text(&transcript), "");

        let history = rendered_text(&transcript);
        let status = transcript.status().to_owned();
        assert!(!transcript.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(72),
                step_id: ModelStepId(90),
                epoch: 0,
                progress: ModelProgress::AssistantTextChunk {
                    text: "stale".to_owned(),
                },
            },
            false,
        ));
        assert_eq!(rendered_text(&transcript), history);
        assert_eq!(transcript.status(), status);

        transcript.apply(completed_execution(73, 91, "done()", "", Vec::new()), false);
        for (run_id, record) in [
            (
                74,
                Record::CancelledRun {
                    terminal: TerminalDetails::empty(),
                },
            ),
            (
                75,
                Record::FailedRun {
                    message: "provider unavailable".to_owned(),
                    terminal: TerminalDetails::empty(),
                },
            ),
        ] {
            transcript.apply(
                RuntimeEvent::RecordAppended {
                    record: StoredRecord {
                        run_id: RunId(run_id),
                        record,
                    },
                    progress_gap: false,
                },
                false,
            );
        }

        let history = rendered_text(&transcript);
        assert!(history.contains("cancelled"));
        assert!(history.contains("failed: provider unavailable"));
        for hidden in [
            "[run",
            "run 73",
            "run 74",
            "run 75",
            "step 91",
            "model step",
        ] {
            assert!(!history.contains(hidden), "{history}");
        }
    }
}
