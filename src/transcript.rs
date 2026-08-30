use std::collections::BTreeMap;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use tui_markdown::{ImageFallback, Options, StyleSheet, from_str_with_options};

use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
use crate::arena::{Execution, ExecutionOutcome, InterruptedRead, Observation, ReadOutcome};
use crate::config::{ApiKeyPresence, PromptProvenance, StartupSnapshot, SubmissionAvailability};
use crate::highlight::PythonHighlighter;
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum TranscriptBlock {
    Startup { lines: Vec<String> },
    User { request: String, progress_gap: bool },
    Reasoning { markdown: String },
    Arena(ArenaBlock),
    Assistant { markdown: String },
    Status { text: String },
    Metadata { lines: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArenaBlock {
    state: ArenaState,
    title: String,
    status: Option<String>,
    source: Option<String>,
    content: Vec<ArenaContent>,
    progress_gap: bool,
    metadata: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArenaState {
    InProgress,
    Complete,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArenaContent {
    label: &'static str,
    text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NormalisedBlock {
    Startup { lines: Vec<String> },
    User { request: String, progress_gap: bool },
    Reasoning { markdown: String },
    Arena(NormalisedArenaBlock),
    Assistant { markdown: String },
    Status { text: String },
    Metadata { lines: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalisedArenaBlock {
    state: ArenaState,
    title: String,
    status: Option<String>,
    source: Option<String>,
    content: Vec<ArenaContent>,
    progress_gap: bool,
    metadata: Vec<String>,
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
            Self::User {
                request,
                progress_gap,
            } => NormalisedBlock::User {
                request: normalise_terminal_text(&request),
                progress_gap,
            },
            Self::Reasoning { markdown } => NormalisedBlock::Reasoning {
                markdown: normalise_terminal_text(&markdown),
            },
            Self::Arena(arena) => NormalisedBlock::Arena(NormalisedArenaBlock {
                state: arena.state,
                title: normalise_terminal_text(&arena.title),
                status: arena.status.map(|status| normalise_terminal_text(&status)),
                source: arena.source.map(|source| normalise_terminal_text(&source)),
                content: arena
                    .content
                    .into_iter()
                    .map(|content| ArenaContent {
                        label: content.label,
                        text: normalise_terminal_text(&content.text),
                    })
                    .collect(),
                progress_gap: arena.progress_gap,
                metadata: arena
                    .metadata
                    .into_iter()
                    .map(|line| normalise_terminal_text(&line))
                    .collect(),
            }),
            Self::Assistant { markdown } => NormalisedBlock::Assistant {
                markdown: normalise_terminal_text(&markdown),
            },
            Self::Status { text } => NormalisedBlock::Status {
                text: normalise_terminal_text(&text),
            },
            Self::Metadata { lines } => NormalisedBlock::Metadata {
                lines: lines
                    .into_iter()
                    .map(|line| normalise_terminal_text(&line))
                    .collect(),
            },
        }
    }
}

const PAGE: Color = Color::Rgb(24, 24, 30);
const TEXT: Color = Color::Rgb(212, 212, 212);
const MUTED: Color = Color::Rgb(128, 128, 128);
const DIM: Color = Color::Rgb(102, 102, 102);
const USER: Color = Color::Rgb(52, 53, 65);
const IN_PROGRESS_ARENA: Color = Color::Rgb(40, 40, 50);
const COMPLETE_ARENA: Color = Color::Rgb(40, 50, 40);
const ERROR_ARENA: Color = Color::Rgb(60, 40, 40);
const REASONING: Color = Color::Rgb(30, 30, 36);

pub(crate) fn page_style() -> Style {
    Style::reset().fg(TEXT).bg(PAGE)
}

fn card_style(background: Color) -> Style {
    Style::reset().fg(TEXT).bg(background)
}

fn muted_card_style(background: Color) -> Style {
    Style::reset().fg(MUTED).bg(background)
}

fn dim_card_style(background: Color) -> Style {
    Style::reset().fg(DIM).bg(background)
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

fn code_preview(source: &str, expanded: bool) -> (String, Option<usize>) {
    let lines = logical_lines(source);
    if expanded || lines.len() <= 32 {
        return (source.to_owned(), None);
    }
    (lines[..32].join("\n"), Some(lines.len() - 32))
}

fn execution_lines(content: &[ArenaContent]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for section in content {
        for (index, line) in logical_lines(&section.text).into_iter().enumerate() {
            if index == 0 {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}: ", section.label), Style::new().fg(MUTED)),
                    Span::raw(line.to_owned()),
                ]));
            } else {
                lines.push(Line::raw(line.to_owned()));
            }
        }
    }
    lines
}

fn execution_preview(content: &[ArenaContent], expanded: bool) -> Vec<Line<'static>> {
    let lines = execution_lines(content);
    if expanded || lines.len() <= 16 {
        return lines;
    }
    let omitted = lines.len() - 16;
    let mut preview = vec![Line::styled(
        format!("[{omitted} execution lines omitted]"),
        Style::new().fg(DIM),
    )];
    preview.extend(lines.into_iter().skip(omitted));
    preview
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

fn arena_text(
    arena: NormalisedArenaBlock,
    expanded: bool,
    highlighter: &mut PythonHighlighter,
    style: Style,
) -> Text<'static> {
    let mut lines = vec![Line::styled(
        arena.title,
        muted_card_style(style.bg.unwrap_or(PAGE)),
    )];
    if let Some(status) = arena.status {
        lines.push(Line::styled(
            status,
            muted_card_style(style.bg.unwrap_or(PAGE)),
        ));
    }
    if arena.progress_gap {
        lines.push(Line::styled(
            "display gap: some live progress was dropped",
            dim_card_style(style.bg.unwrap_or(PAGE)),
        ));
    }
    if let Some(source) = arena.source {
        lines.push(Line::styled(
            "code:",
            muted_card_style(style.bg.unwrap_or(PAGE)),
        ));
        let (preview, omitted) = code_preview(&source, expanded);
        lines.extend(own_styled_text(highlighter.highlight(&preview), style).lines);
        if let Some(omitted) = omitted {
            lines.push(Line::styled(
                format!("[{omitted} code lines omitted]"),
                dim_card_style(style.bg.unwrap_or(PAGE)),
            ));
        }
    }
    lines.extend(execution_preview(&arena.content, expanded));
    lines.extend(
        arena
            .metadata
            .into_iter()
            .map(|line| Line::styled(line, dim_card_style(style.bg.unwrap_or(PAGE)))),
    );
    Text {
        alignment: None,
        style: text_style(style),
        lines,
    }
}

#[cfg(test)]
impl NormalisedBlock {
    fn paragraph(&self) -> Paragraph<'_> {
        let text = match self {
            Self::Reasoning { markdown: source } | Self::Assistant { markdown: source } => {
                markdown(source)
            }
            _ => Text::raw(self.plain_text()),
        };
        Paragraph::new(text).wrap(Wrap { trim: false })
    }

    fn plain_text(&self) -> String {
        match self {
            Self::Startup { lines } | Self::Metadata { lines } => lines.join("\n"),
            Self::User {
                request,
                progress_gap,
            } => {
                let mut lines = vec![format!("you: {request}")];
                append_display_gap(&mut lines, *progress_gap);
                lines.join("\n")
            }
            Self::Arena(arena) => arena.plain_text(),
            Self::Status { text } => text.clone(),
            Self::Reasoning { .. } | Self::Assistant { .. } => unreachable!("markdown block"),
        }
    }
}

#[cfg(test)]
impl NormalisedArenaBlock {
    fn plain_text(&self) -> String {
        let mut lines = vec![self.title.clone()];
        if let Some(status) = &self.status {
            lines.push(status.clone());
        }
        if let Some(source) = &self.source {
            lines.push(format!("code:\n{source}"));
        }
        append_display_gap(&mut lines, self.progress_gap);
        lines.extend(
            self.content
                .iter()
                .map(|content| format!("{}: {}", content.label, content.text)),
        );
        lines.extend(self.metadata.iter().cloned());
        lines.join("\n")
    }
}

struct RenderBlock {
    text: Text<'static>,
    style: Style,
    left_padding: u16,
    right_padding: u16,
    top_padding: u16,
    bottom_padding: u16,
    height: usize,
}

impl RenderBlock {
    fn new(
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
        Self {
            text,
            style,
            left_padding,
            right_padding,
            top_padding: vertical_padding,
            bottom_padding: vertical_padding,
            height: content_height.saturating_add(usize::from(vertical_padding).saturating_mul(2)),
        }
    }

    fn render(&self, viewport: Rect, top: usize, block_top: usize, buffer: &mut Buffer) {
        let block_bottom = block_top.saturating_add(self.height);
        let visible_top = block_top.max(top);
        let visible_bottom = block_bottom.min(top.saturating_add(usize::from(viewport.height)));
        if visible_top >= visible_bottom {
            return;
        }
        let y = viewport
            .y
            .saturating_add(u16::try_from(visible_top.saturating_sub(top)).unwrap_or(u16::MAX));
        let height = u16::try_from(visible_bottom.saturating_sub(visible_top)).unwrap_or(u16::MAX);
        let visible = Rect::new(viewport.x, y, viewport.width, height);
        buffer.set_style(visible, self.style);

        let text_top = block_top.saturating_add(usize::from(self.top_padding));
        let text_bottom = block_bottom.saturating_sub(usize::from(self.bottom_padding));
        let visible_text_top = visible_top.max(text_top);
        let visible_text_bottom = visible_bottom.min(text_bottom);
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

pub(crate) struct TranscriptLayout {
    blocks: Vec<RenderBlock>,
    total_height: usize,
}

impl TranscriptLayout {
    pub(crate) fn total_height(&self) -> usize {
        self.total_height
    }

    pub(crate) fn render(&self, viewport: Rect, top: usize, buffer: &mut Buffer) {
        if viewport.width == 0 || viewport.height == 0 {
            return;
        }
        buffer.set_style(viewport, page_style());
        let mut block_top = 0_usize;
        for block in &self.blocks {
            if block_top >= top.saturating_add(usize::from(viewport.height)) {
                break;
            }
            block.render(viewport, top, block_top, buffer);
            block_top = block_top.saturating_add(block.height);
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
    show_reasoning: bool,
    expand_arenas: bool,
    highlighter: PythonHighlighter,
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
            show_reasoning: true,
            expand_arenas: false,
            highlighter: PythonHighlighter::new(),
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

    pub(crate) fn toggle_reasoning(&mut self) -> bool {
        self.show_reasoning = !self.show_reasoning;
        self.blocks()
            .iter()
            .any(|block| matches!(block, TranscriptBlock::Reasoning { .. }))
    }

    pub(crate) fn toggle_arena_expansion(&mut self) -> bool {
        self.expand_arenas = !self.expand_arenas;
        self.blocks()
            .iter()
            .any(|block| matches!(block, TranscriptBlock::Arena(..)))
    }

    pub(crate) fn layout(&mut self, width: u16) -> TranscriptLayout {
        let mut blocks = Vec::new();
        for block in self.blocks().into_iter().map(TranscriptBlock::normalise) {
            let render_block = match block {
                NormalisedBlock::Startup { lines } | NormalisedBlock::Metadata { lines } => {
                    RenderBlock::new(
                        own_styled_text(Text::raw(lines.join("\n")), page_style()),
                        page_style(),
                        width,
                        0,
                        0,
                    )
                }
                NormalisedBlock::User {
                    request,
                    progress_gap,
                } => {
                    let mut lines = vec![format!("you: {request}")];
                    append_display_gap(&mut lines, progress_gap);
                    let style = card_style(USER);
                    RenderBlock::new(
                        own_styled_text(Text::raw(lines.join("\n")), style),
                        style,
                        width,
                        1,
                        1,
                    )
                }
                NormalisedBlock::Reasoning { markdown: source } => {
                    let style = muted_card_style(REASONING).add_modifier(Modifier::ITALIC);
                    let text = if self.show_reasoning {
                        own_styled_text(markdown(&source), style)
                    } else {
                        own_styled_text(
                            Text::raw("reasoning hidden · Ctrl-T to show"),
                            dim_card_style(REASONING).add_modifier(Modifier::ITALIC),
                        )
                    };
                    RenderBlock::new(text, style, width, 2, 0)
                }
                NormalisedBlock::Arena(arena) => {
                    let background = match arena.state {
                        ArenaState::InProgress => IN_PROGRESS_ARENA,
                        ArenaState::Complete => COMPLETE_ARENA,
                        ArenaState::Error => ERROR_ARENA,
                    };
                    let style = card_style(background);
                    RenderBlock::new(
                        arena_text(arena, self.expand_arenas, &mut self.highlighter, style),
                        style,
                        width,
                        1,
                        1,
                    )
                }
                NormalisedBlock::Assistant { markdown: source } => RenderBlock::new(
                    own_styled_text(markdown(&source), page_style()),
                    page_style(),
                    width,
                    0,
                    0,
                ),
                NormalisedBlock::Status { text } => RenderBlock::new(
                    own_styled_text(Text::raw(text), page_style()),
                    page_style(),
                    width,
                    0,
                    0,
                ),
            };
            blocks.push(render_block);
        }
        let total_height = blocks
            .iter()
            .fold(0_usize, |height, block| height.saturating_add(block.height));
        TranscriptLayout {
            blocks,
            total_height,
        }
    }

    fn blocks(&self) -> Vec<TranscriptBlock> {
        let mut blocks = self
            .startup
            .as_ref()
            .map(startup_block)
            .into_iter()
            .collect::<Vec<_>>();
        let mut attempts_by_run = BTreeMap::<RunId, Vec<&AttemptAccounting>>::new();
        for displayed in &self.records {
            let prior_attempts = attempts_by_run.entry(displayed.record.run_id).or_default();
            blocks.extend(record_blocks(displayed, prior_attempts));
            if let Record::CompletedExecution { attempts, .. } = &displayed.record.record {
                prior_attempts.extend(attempts);
            }
        }
        if let Some(live) = &self.live_step {
            let prior_attempts = attempts_by_run
                .get(&live.run_id)
                .map_or(&[][..], Vec::as_slice);
            blocks.extend(live_blocks(live, prior_attempts));
        }
        blocks
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
    TranscriptBlock::Startup { lines }
}

#[cfg(test)]
pub(crate) fn format_startup_configuration(snapshot: &StartupSnapshot) -> String {
    let NormalisedBlock::Startup { lines } = startup_block(snapshot).normalise() else {
        unreachable!("startup block")
    };
    lines.join("\n")
}

fn record_blocks(
    displayed: &DisplayedRecord,
    prior_attempts: &[&AttemptAccounting],
) -> Vec<TranscriptBlock> {
    let stored = &displayed.record;
    let run_id = stored.run_id.0;
    match &stored.record {
        Record::UserRequest { request } => vec![TranscriptBlock::User {
            request: request.clone(),
            progress_gap: displayed.progress_gap,
        }],
        Record::CompletedExecution {
            step_id,
            source,
            reasoning_summary,
            execution,
            attempts,
        } => {
            let mut blocks = Vec::new();
            append_reasoning(&mut blocks, reasoning_summary, "");
            blocks.push(TranscriptBlock::Arena(ArenaBlock {
                state: arena_state(execution),
                title: format!("[run {run_id} step {}]", step_id.0),
                status: None,
                source: Some(source.clone()),
                content: execution_content(execution),
                progress_gap: displayed.progress_gap,
                metadata: step_metadata(attempts, prior_attempts),
            }));
            blocks
        }
        Record::FinalMessage {
            step_id,
            text,
            reasoning_summary,
            terminal,
        } => {
            let mut blocks = Vec::new();
            append_reasoning(&mut blocks, reasoning_summary, "");
            terminal_evidence_blocks(
                &mut blocks,
                terminal.evidence.as_ref(),
                format!("[run {run_id} step {}]", step_id.0),
            );
            blocks.push(TranscriptBlock::Assistant {
                markdown: text.clone(),
            });
            append_gap_metadata(&mut blocks, displayed.progress_gap);
            append_metadata(
                &mut blocks,
                step_metadata(&terminal.attempts, prior_attempts),
            );
            blocks
        }
        Record::CancelledRun { terminal } => terminal_blocks(
            format!("[run {run_id}] cancelled"),
            terminal,
            prior_attempts,
            displayed.progress_gap,
        ),
        Record::FailedRun { message, terminal } => terminal_blocks(
            format!("[run {run_id}] failed: {message}"),
            terminal,
            prior_attempts,
            displayed.progress_gap,
        ),
    }
}

fn live_blocks(live: &LiveStep, prior_attempts: &[&AttemptAccounting]) -> Vec<TranscriptBlock> {
    let has_arena_content = live.phase == LivePhase::Executing
        || !live.decoded_python.is_empty()
        || !live.stdout.is_empty()
        || live.stdout_truncated;
    let mut metadata = Vec::new();
    if let Some(attempt) = live.attempt {
        metadata.push(format!("attempt: {attempt}"));
    }
    metadata.extend(step_metadata(&live.settled_attempts, prior_attempts));

    let mut blocks = Vec::new();
    if !has_arena_content {
        blocks.push(TranscriptBlock::Status {
            text: format!(
                "[run {} step {}] live: {}\nactivity: {}",
                live.run_id.0,
                live.step_id.0,
                live.phase.label(),
                live.activity,
            ),
        });
    }
    append_reasoning(&mut blocks, "", &live.reasoning);
    if has_arena_content {
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
            title: format!(
                "[run {} step {}] live: {}",
                live.run_id.0,
                live.step_id.0,
                live.phase.label(),
            ),
            status: Some(format!("activity: {}", live.activity)),
            source: (!live.decoded_python.is_empty()).then(|| live.decoded_python.clone()),
            content,
            progress_gap: live.progress_gap,
            metadata,
        }));
        if !live.assistant_text.is_empty() {
            blocks.push(TranscriptBlock::Assistant {
                markdown: live.assistant_text.clone(),
            });
        }
        return blocks;
    }

    append_gap_metadata(&mut blocks, live.progress_gap);
    if !live.assistant_text.is_empty() {
        blocks.push(TranscriptBlock::Assistant {
            markdown: live.assistant_text.clone(),
        });
    }
    append_metadata(&mut blocks, metadata);
    blocks
}

fn terminal_blocks(
    title: String,
    terminal: &TerminalDetails,
    prior_attempts: &[&AttemptAccounting],
    progress_gap: bool,
) -> Vec<TranscriptBlock> {
    let mut evidence_blocks = Vec::new();
    if let Some(arena_index) = terminal_evidence_blocks(
        &mut evidence_blocks,
        terminal.evidence.as_ref(),
        title.clone(),
    ) {
        let TranscriptBlock::Arena(arena) = &mut evidence_blocks[arena_index] else {
            unreachable!("terminal evidence arena")
        };
        arena.progress_gap = progress_gap;
        arena.metadata = step_metadata(&terminal.attempts, prior_attempts);
        evidence_blocks
    } else {
        let mut blocks = vec![TranscriptBlock::Status { text: title }];
        blocks.append(&mut evidence_blocks);
        append_gap_metadata(&mut blocks, progress_gap);
        append_metadata(
            &mut blocks,
            step_metadata(&terminal.attempts, prior_attempts),
        );
        blocks
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

fn append_metadata(blocks: &mut Vec<TranscriptBlock>, lines: Vec<String>) {
    if !lines.is_empty() {
        blocks.push(TranscriptBlock::Metadata { lines });
    }
}

fn terminal_evidence_blocks(
    blocks: &mut Vec<TranscriptBlock>,
    evidence: Option<&TerminalEvidence>,
    title: String,
) -> Option<usize> {
    match evidence {
        Some(TerminalEvidence::Generation(GenerationEvidence {
            reasoning_summary,
            reasoning,
            assistant_text,
            decoded_python,
        })) => {
            append_reasoning(blocks, reasoning_summary, reasoning);
            let arena_index = (!decoded_python.is_empty()).then(|| {
                let arena_index = blocks.len();
                blocks.push(TranscriptBlock::Arena(ArenaBlock {
                    state: ArenaState::Error,
                    title,
                    status: None,
                    source: Some(decoded_python.clone()),
                    content: Vec::new(),
                    progress_gap: false,
                    metadata: Vec::new(),
                }));
                arena_index
            });
            if !assistant_text.is_empty() {
                blocks.push(TranscriptBlock::Assistant {
                    markdown: assistant_text.clone(),
                });
            }
            arena_index
        }
        Some(TerminalEvidence::Execution(evidence)) => {
            append_reasoning(blocks, &evidence.reasoning_summary, "");
            let arena_index = blocks.len();
            blocks.push(TranscriptBlock::Arena(execution_evidence_block(
                title, evidence,
            )));
            Some(arena_index)
        }
        None => None,
    }
}

fn arena_state(execution: &Execution) -> ArenaState {
    match execution.outcome {
        ExecutionOutcome::Complete { .. } => ArenaState::Complete,
        ExecutionOutcome::PythonError(..) => ArenaState::Error,
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
    }
    content
}

fn execution_evidence_block(title: String, evidence: &ExecutionEvidence) -> ArenaBlock {
    let mut content = Vec::new();
    append_observation(&mut content, "stdout", &evidence.stdout);
    append_observation(&mut content, "stderr", &evidence.stderr);
    content.push(ArenaContent {
        label: "read",
        text: format_interrupted_read(&evidence.read),
    });
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
        title,
        status: Some(format!(
            "execution: {}",
            if evidence.began {
                "started"
            } else {
                "not started"
            }
        )),
        source: Some(evidence.source.clone()),
        content,
        progress_gap: false,
        metadata: Vec::new(),
    }
}

fn step_metadata(
    attempts: &[AttemptAccounting],
    prior_attempts: &[&AttemptAccounting],
) -> Vec<String> {
    let mut lines = Vec::new();
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
    lines
}

fn format_interrupted_read(read: &InterruptedRead) -> String {
    match read {
        InterruptedRead::NoRead => "none".to_owned(),
        InterruptedRead::Pending { path } => format!("pending {path}"),
        InterruptedRead::Completed { path, outcome } => match outcome {
            ReadOutcome::Success { line_count } => format!("completed {path} ({line_count} lines)"),
            ReadOutcome::Failure { message } => format!("completed {path} failed: {message}"),
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

fn observation_text(observation: &Observation) -> String {
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
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::widgets::{Paragraph, Widget, Wrap};

    use super::{
        ArenaBlock, ArenaContent, ArenaState, COMPLETE_ARENA, DIM, ERROR_ARENA, IN_PROGRESS_ARENA,
        MUTED, NormalisedBlock, PAGE, REASONING, RenderBlock, TEXT, Transcript, TranscriptBlock,
        TranscriptLayout, USER, markdown, normalise_terminal_text, own_styled_text, page_style,
    };
    use crate::agent::{ExecutionEvidence, ExecutionFailure, GenerationEvidence, TerminalEvidence};
    use crate::arena::{
        Execution, ExecutionOutcome, InterruptedRead, Observation, PythonError, ReadOutcome,
    };
    use crate::highlight::PythonHighlighter;
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

    fn rendered_text(transcript: &Transcript) -> String {
        transcript
            .blocks()
            .into_iter()
            .map(TranscriptBlock::normalise)
            .map(|block| match &block {
                NormalisedBlock::Reasoning { markdown: source }
                | NormalisedBlock::Assistant { markdown: source } => markdown(source).to_string(),
                block => block.plain_text(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_layout_text(transcript: &mut Transcript) -> String {
        transcript
            .layout(u16::MAX)
            .blocks
            .iter()
            .map(|block| block.text.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_buffer(transcript: &mut Transcript, width: u16) -> Buffer {
        let layout = transcript.layout(width);
        let height = u16::try_from(layout.total_height()).expect("test transcript height");
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        layout.render(buffer.area, 0, &mut buffer);
        buffer
    }

    fn row_containing(buffer: &Buffer, text: &str) -> usize {
        (0..buffer.area.height as usize)
            .find(|row| {
                buffer.content
                    [*row * buffer.area.width as usize..(*row + 1) * buffer.area.width as usize]
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains(text)
            })
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

        let history = rendered_text(&transcript);
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
            },
            false,
        );

        let history = rendered_text(&transcript);
        assert!(history.contains("settled\ndisplay gap"));
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

        let history = rendered_text(&transcript);
        assert_eq!(history.matches("attempt 1: model=gpt-5.6-sol").count(), 2);
        assert!(history.contains("Run estimated cost: Complete $0.0024 USD"));
        assert!(history.contains("[run 2 step 2]\ncode:"));
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

        let history = rendered_text(&transcript);
        for text in [
            "stdout: hello",
            "stderr: warning",
            "error: boom",
            "code:\nawait read(path)",
            "stderr: diagnostic",
            "read: completed bad    path failed: read failed",
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

        let layout = transcript.layout(80);
        let assistant = layout
            .blocks
            .iter()
            .find(|block| block.text.to_string().contains("Settled heading"))
            .expect("settled assistant block");
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
                TranscriptBlock::User { .. } => "user",
                TranscriptBlock::Reasoning { .. } => "reasoning",
                TranscriptBlock::Arena(..) => "arena",
                TranscriptBlock::Assistant { .. } => "assistant",
                TranscriptBlock::Status { .. } => "status",
                TranscriptBlock::Metadata { .. } => "metadata",
            })
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            [
                "user",
                "reasoning",
                "arena",
                "reasoning",
                "assistant",
                "metadata"
            ]
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
                    progress,
                },
                false,
            );
        }

        let live = transcript.layout(80);
        let live_markdown = live
            .blocks
            .iter()
            .filter(|block| {
                let text = block.text.to_string();
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
        let settled_layout = transcript.layout(80);
        let settled_reasoning = settled_layout
            .blocks
            .iter()
            .find(|block| block.text.to_string().contains("Settled reasoning"))
            .expect("settled reasoning");
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
                TranscriptBlock::User { .. } => "user",
                TranscriptBlock::Reasoning { .. } => "reasoning",
                TranscriptBlock::Arena(..) => "arena",
                TranscriptBlock::Assistant { .. } => "assistant",
                TranscriptBlock::Status { .. } => "status",
                TranscriptBlock::Metadata { .. } => "metadata",
            })
            .collect::<Vec<_>>();
        assert_eq!(roles, ["reasoning", "arena", "assistant"]);

        let [
            TranscriptBlock::Reasoning { .. },
            TranscriptBlock::Arena(arena),
            TranscriptBlock::Assistant { .. },
        ] = blocks.as_slice()
        else {
            unreachable!("live arena roles checked")
        };
        assert_eq!(arena.title, "[run 1 step 1] live: generating");
        assert!(arena.progress_gap);
        assert_eq!(arena.status.as_deref(), Some("activity: model attempt 1"));
        for line in [
            "attempt: 1",
            "attempt 1: model=gpt-5.6-sol",
            "Step estimated cost:",
            "Run estimated cost:",
        ] {
            assert!(
                arena
                    .metadata
                    .iter()
                    .any(|metadata| metadata.starts_with(line))
            );
        }

        let rendered = rendered_text(&transcript);
        assert_eq!(
            rendered.matches("[run 1 step 1] live: generating").count(),
            1
        );
        assert!(
            rendered.find("live reasoning").expect("reasoning")
                < rendered
                    .find("[run 1 step 1] live: generating")
                    .expect("arena")
        );
        assert!(
            rendered
                .find("[run 1 step 1] live: generating")
                .expect("arena")
                < rendered.find("live assistant").expect("assistant")
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
                blocks.iter().any(|block| matches!(block, TranscriptBlock::Assistant { markdown } if markdown == assistant)),
                !assistant.is_empty(),
                "mask {mask}"
            );
            assert_eq!(
                blocks.iter().any(|block| matches!(block, TranscriptBlock::Arena(ArenaBlock { source: Some(source), .. }) if source == code)),
                !code.is_empty(),
                "mask {mask}"
            );
        }
    }

    #[test]
    fn rendering_terminal_arenas_own_terminal_details() {
        let cases = [
            (
                1,
                "[run 1] cancelled".to_owned(),
                Record::CancelledRun {
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                            source: "cancelled_code()".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation(""),
                            stderr: observation(""),
                            read: InterruptedRead::NoRead,
                            failure: None,
                            cancellation_warning: None,
                        }))),
                        attempts: vec![usage(1, 1)],
                    },
                },
            ),
            (
                2,
                "[run 2] failed: model failed".to_owned(),
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
                    },
                },
            ),
        ];

        for (run_id, title, record) in cases {
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
            assert_eq!(rendered_text(&transcript).matches(&title).count(), 1);
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
            assert_eq!(arenas[0].title, title);
            assert!(arenas[0].progress_gap);
            assert!(
                arenas[0]
                    .metadata
                    .iter()
                    .any(|line| line.starts_with("attempt 1:"))
            );
        }
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
            matches!(block, TranscriptBlock::Status { text } if text == "[run 1] cancelled")
        }));
        assert!(blocks.iter().any(|block| {
            matches!(block, TranscriptBlock::Metadata { lines } if lines.len() == 1 && lines[0] == "display gap: some live progress was dropped")
        }));
        assert!(blocks.iter().any(|block| {
            matches!(block, TranscriptBlock::Metadata { lines } if lines.iter().any(|line| line.starts_with("Step estimated cost:")))
        }));
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, TranscriptBlock::Arena(..)))
        );
    }

    #[test]
    fn rendering_execution_started_without_visible_output_has_an_arena_role() {
        let mut transcript = Transcript::default();
        transcript.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        transcript.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(1),
                step_id: ModelStepId(1),
            },
            false,
        );

        assert!(transcript.blocks().iter().any(|block| {
            matches!(
                block,
                TranscriptBlock::Arena(ArenaBlock {
                    title,
                    source: None,
                    content,
                    ..
                }) if title == "[run 1 step 1] live: executing" && content.is_empty()
            )
        }));
    }

    #[test]
    fn rendering_keeps_execution_content_literal_and_plain() {
        let block = TranscriptBlock::Arena(ArenaBlock {
            state: ArenaState::Complete,
            title: "arena".to_owned(),
            status: None,
            source: Some("**literal source**".to_owned()),
            content: vec![
                ArenaContent {
                    label: "stdout",
                    text: "# [literal output](https://example.com)".to_owned(),
                },
                ArenaContent {
                    label: "stderr",
                    text: "~~literal stderr~~".to_owned(),
                },
                ArenaContent {
                    label: "result",
                    text: "**literal result**".to_owned(),
                },
                ArenaContent {
                    label: "error",
                    text: "# literal error".to_owned(),
                },
                ArenaContent {
                    label: "warning",
                    text: "[literal warning](https://example.com)".to_owned(),
                },
            ],
            progress_gap: false,
            metadata: vec!["**literal metadata**".to_owned()],
        })
        .normalise();
        let paragraph = block.paragraph();
        let height = paragraph.line_count(80);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, u16::try_from(height).unwrap()));
        paragraph.render(buffer.area, &mut buffer);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("**literal source**"));
        for text in [
            "# [literal output](https://example.com)",
            "~~literal stderr~~",
            "**literal result**",
            "# literal error",
            "[literal warning](https://example.com)",
            "**literal metadata**",
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
            RuntimeEvent::ToolStarted {
                run_id: RunId(99),
                step_id: ModelStepId(1),
                path: hostile.to_owned(),
            },
            false,
        );
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: hostile.to_owned(),
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
                        source: hostile.to_owned(),
                        reasoning_summary: hostile.to_owned(),
                        execution: Execution {
                            stdout: observation(hostile),
                            stderr: observation(hostile),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: hostile.to_owned(),
                                omitted_bytes: 0,
                            }),
                        },
                        attempts: vec![AttemptAccounting::KnownUsage(ModelUsage {
                            model: hostile.to_owned(),
                            service_tier: Some(hostile.to_owned()),
                            input_tokens: Some(1),
                            cached_input_tokens: Some(0),
                            cache_write_input_tokens: Some(0),
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
                                    source: hostile.to_owned(),
                                    reasoning_summary: hostile.to_owned(),
                                    began: true,
                                    stdout: observation(hostile),
                                    stderr: observation(hostile),
                                    read: InterruptedRead::Completed {
                                        path: hostile.to_owned(),
                                        outcome: ReadOutcome::Failure {
                                            message: hostile.to_owned(),
                                        },
                                    },
                                    failure: Some(ExecutionFailure::RecoveryFailed {
                                        original: hostile.to_owned(),
                                        recovery: hostile.to_owned(),
                                    }),
                                    cancellation_warning: Some(hostile.to_owned()),
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
        let Record::UserRequest { request } = &transcript.records[0].record.record else {
            unreachable!("user record")
        };
        assert_eq!(request, hostile);
    }

    #[test]
    fn rendering_matches_cjk_and_emoji_measurement() {
        let block = TranscriptBlock::Assistant {
            markdown: "日本語 🦀 日本語 🦀".to_owned(),
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
                RenderBlock::new(
                    own_styled_text(ratatui::text::Text::raw(text), page_style()),
                    page_style(),
                    20,
                    0,
                    0,
                )
            })
            .collect::<Vec<_>>();
        let layout = TranscriptLayout {
            total_height: blocks.iter().map(|block| block.height).sum(),
            blocks,
        };
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 1));
        layout.render(buffer.area, 1, &mut buffer);
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
    fn cards_use_pi_palette_and_full_width_fills() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "inspect".to_owned(),
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

        let layout = transcript.layout(40);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, layout.total_height() as u16));
        layout.render(buffer.area, 0, &mut buffer);

        let user = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "y")
            .expect("user request cell");
        assert_eq!(user.bg, Color::Rgb(52, 53, 65));
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(52, 53, 65));
    }

    #[test]
    fn cards_collapse_code_after_32_logical_lines() {
        let mut short = Transcript::default();
        let short_source = (1..=32)
            .map(|line| format!("short code line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        short.apply(
            completed_execution(1, 1, &format!("{short_source}\n"), "", Vec::new()),
            false,
        );
        assert!(!rendered_layout_text(&mut short).contains("code lines omitted"));

        let mut transcript = Transcript::default();
        let source = (1..=33)
            .map(|line| format!("code line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(completed_execution(1, 1, &source, "", Vec::new()), false);

        let history = rendered_layout_text(&mut transcript);
        assert!(history.contains("code line 32"));
        assert!(!history.contains("code line 33"));
        assert!(history.contains("[1 code lines omitted]"));
    }

    #[test]
    fn cards_render_code_omission_marker_outside_unclosed_source() {
        let mut transcript = Transcript::default();
        let source = (1..=31)
            .map(|line| format!("line_{line} = {line}"))
            .chain(std::iter::once("unterminated = \"\"\"".to_owned()))
            .chain(
                ["hidden = 33", "still_hidden = 34"]
                    .into_iter()
                    .map(str::to_owned),
            )
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(completed_execution(1, 1, &source, "", Vec::new()), false);

        let buffer = rendered_buffer(&mut transcript, 80);
        let marker = "[2 code lines omitted]";
        let marker_row = row_containing(&buffer, marker);
        for (offset, character) in marker.chars().enumerate() {
            let cell = &buffer[(offset as u16 + 1, marker_row as u16)];
            assert_eq!(cell.symbol(), character.to_string());
            assert_eq!(cell.fg, DIM);
            assert_eq!(cell.bg, COMPLETE_ARENA);
        }
    }

    #[test]
    fn cards_collapse_combined_execution_content_to_its_tail() {
        let mut short = Transcript::default();
        short.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(
                                &(1..=16)
                                    .map(|line| format!("short out {line}"))
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                            ),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::Complete { result: None },
                        },
                        attempts: Vec::new(),
                    },
                },
                progress_gap: false,
            },
            false,
        );
        assert!(!rendered_layout_text(&mut short).contains("execution lines omitted"));

        let mut transcript = Transcript::default();
        let stdout = (1..=15)
            .map(|line| format!("out {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(&stdout),
                            stderr: observation("err"),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("done")),
                            },
                        },
                        attempts: Vec::new(),
                    },
                },
                progress_gap: false,
            },
            false,
        );

        let history = rendered_layout_text(&mut transcript);
        assert!(!history.contains("stdout: out 1"));
        assert!(history.contains("[1 execution lines omitted]\nout 2"));
        assert!(history.contains("stderr: err\nresult: done"));
    }

    #[test]
    fn cards_keep_roles_padded_full_width_and_borderless() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::UserRequest {
                        request: "user request".to_owned(),
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
        transcript.apply(
            final_message(1, 2, "assistant response", Vec::new(), false),
            false,
        );

        let buffer = rendered_buffer(&mut transcript, 40);
        let user_row = row_containing(&buffer, "you: user request");
        assert_eq!(buffer[(0, user_row as u16)].symbol(), " ");
        assert_eq!(buffer[(1, user_row as u16)].symbol(), "y");
        assert_row_background(&buffer, user_row - 1, USER);
        assert_row_background(&buffer, user_row, USER);
        assert_row_background(&buffer, user_row + 1, USER);

        let reasoning_row = row_containing(&buffer, "reasoning");
        assert_eq!(buffer[(0, reasoning_row as u16)].symbol(), " ");
        assert_eq!(buffer[(1, reasoning_row as u16)].symbol(), " ");
        assert_eq!(buffer[(2, reasoning_row as u16)].symbol(), "r");
        assert_row_background(&buffer, reasoning_row, REASONING);
        assert_eq!(buffer[(2, reasoning_row as u16)].fg, MUTED);
        assert!(
            buffer[(2, reasoning_row as u16)]
                .style()
                .add_modifier
                .contains(Modifier::ITALIC)
        );

        let arena_row = row_containing(&buffer, "[run 1 step 1]");
        assert_eq!(buffer[(0, arena_row as u16)].symbol(), " ");
        assert_eq!(buffer[(1, arena_row as u16)].symbol(), "[");
        assert_row_background(&buffer, arena_row - 1, COMPLETE_ARENA);
        assert_row_background(&buffer, arena_row, COMPLETE_ARENA);
        assert!(!buffer.content.iter().any(|cell| {
            matches!(cell.symbol(), "┌" | "┐" | "└" | "┘" | "│" | "─")
        }));

        let assistant_row = row_containing(&buffer, "assistant response");
        assert_eq!(buffer[(0, assistant_row as u16)].symbol(), "a");
        assert_row_background(&buffer, assistant_row, PAGE);
    }

    #[test]
    fn cards_map_live_completed_and_terminal_evidence_to_typed_states() {
        let mut live = Transcript::default();
        live.apply(RuntimeEvent::RunStarted { run_id: RunId(1) }, false);
        live.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(1),
                step_id: ModelStepId(1),
                progress: ModelProgress::DecodedPythonChunk {
                    code: "live_source".to_owned(),
                },
            },
            false,
        );
        let live_buffer = rendered_buffer(&mut live, 40);
        assert_row_background(
            &live_buffer,
            row_containing(&live_buffer, "live_source"),
            IN_PROGRESS_ARENA,
        );

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
                        source: "python_error_source".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(""),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::PythonError(PythonError {
                                text: "error".to_owned(),
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
        for (run_id, source, record) in [
            (
                3,
                "cancelled_source",
                Record::CancelledRun {
                    terminal: TerminalDetails {
                        evidence: Some(TerminalEvidence::Execution(Box::new(ExecutionEvidence {
                            source: "cancelled_source".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation(""),
                            stderr: observation(""),
                            read: InterruptedRead::NoRead,
                            cancellation_warning: Some("cancelled".to_owned()),
                            failure: None,
                        }))),
                        attempts: Vec::new(),
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
                            source: "failed_source".to_owned(),
                            reasoning_summary: String::new(),
                            began: true,
                            stdout: observation(""),
                            stderr: observation(""),
                            read: InterruptedRead::NoRead,
                            cancellation_warning: None,
                            failure: Some(ExecutionFailure::Arena {
                                message: "failed".to_owned(),
                            }),
                        }))),
                        attempts: Vec::new(),
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
            COMPLETE_ARENA,
        );
        for source in ["python_error_source", "cancelled_source", "failed_source"] {
            assert_row_background(&buffer, row_containing(&buffer, source), ERROR_ARENA);
        }

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
                        },
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let decoded_buffer = rendered_buffer(&mut decoded, 60);
        assert_row_background(
            &decoded_buffer,
            row_containing(&decoded_buffer, "terminal_decoded_source"),
            ERROR_ARENA,
        );
    }

    #[test]
    fn cards_keep_short_source_and_execution_open_and_metadata_outside_budgets() {
        let mut transcript = Transcript::default();
        let source = (1..=32)
            .map(|line| format!("code {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let stdout = (1..=16)
            .map(|line| format!("output {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        source,
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation(&stdout),
                            stderr: observation(""),
                            outcome: ExecutionOutcome::Complete { result: None },
                        },
                        attempts: vec![usage(1, 1)],
                    },
                },
                progress_gap: true,
            },
            false,
        );
        let history = rendered_layout_text(&mut transcript);
        assert!(history.contains("code 32"));
        assert!(history.contains("stdout: output 1"));
        assert!(!history.contains("code lines omitted"));
        assert!(!history.contains("execution lines omitted"));
        assert!(history.contains("[run 1 step 1]"));
        assert!(history.contains("display gap: some live progress was dropped"));
        assert!(history.contains("attempt 1: model=gpt-5.6-sol"));
        assert!(history.contains("Step estimated cost:"));

        let mut title_only = Transcript::default();
        title_only.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        title_only.apply(
            RuntimeEvent::ExecutionStarted {
                run_id: RunId(2),
                step_id: ModelStepId(1),
            },
            false,
        );
        let title_only_history = rendered_layout_text(&mut title_only);
        assert!(title_only_history.contains("[run 2 step 1] live: executing"));
        assert!(!title_only_history.contains("code:"));
    }

    #[test]
    fn cards_preserve_output_section_order_and_tail_labels() {
        let mut transcript = Transcript::default();
        transcript.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(1),
                    record: Record::CompletedExecution {
                        step_id: ModelStepId(1),
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout: observation("first\nsecond"),
                            stderr: observation("third"),
                            outcome: ExecutionOutcome::Complete {
                                result: Some(observation("fourth")),
                            },
                        },
                        attempts: Vec::new(),
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let history = rendered_layout_text(&mut transcript);
        assert!(history.contains("stdout: first\nsecond\nstderr: third\nresult: fourth"));
        assert!(!history.contains("stdout: second"));

        let mut interrupted = Transcript::default();
        interrupted.apply(
            RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: RunId(2),
                    record: Record::CancelledRun {
                        terminal: TerminalDetails {
                            evidence: Some(TerminalEvidence::Execution(Box::new(
                                ExecutionEvidence {
                                    source: "pass".to_owned(),
                                    reasoning_summary: String::new(),
                                    began: true,
                                    stdout: observation("out"),
                                    stderr: observation("err"),
                                    read: InterruptedRead::Completed {
                                        path: "path".to_owned(),
                                        outcome: ReadOutcome::Failure {
                                            message: "read failed".to_owned(),
                                        },
                                    },
                                    failure: Some(ExecutionFailure::Arena {
                                        message: "failure".to_owned(),
                                    }),
                                    cancellation_warning: Some("warning".to_owned()),
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
        let interrupted_history = rendered_layout_text(&mut interrupted);
        assert!(interrupted_history.contains(
            "execution: started\ncode:\npass\nstdout: out\nstderr: err\nread: completed path failed: read failed\nfailure: failure\nwarning: warning"
        ));
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
                        source: "pass".to_owned(),
                        reasoning_summary: String::new(),
                        execution: Execution {
                            stdout,
                            stderr: observation(""),
                            outcome: ExecutionOutcome::Complete { result: None },
                        },
                        attempts: Vec::new(),
                    },
                },
                progress_gap: false,
            },
            false,
        );
        let collapsed = rendered_layout_text(&mut transcript);
        assert!(collapsed.contains("[19 execution lines omitted]"));
        assert!(transcript.toggle_arena_expansion());
        let expanded = rendered_layout_text(&mut transcript);
        assert!(!expanded.contains("execution lines omitted"));
        assert!(expanded.contains("[20 lines, 200 bytes omitted]"));
        assert!(expanded.contains("head 1"));
        assert!(expanded.contains("tail 17"));
    }

    #[test]
    fn cards_highlight_normalised_complete_and_incomplete_python_with_a_plain_fallback() {
        let mut complete = Transcript::default();
        complete.apply(
            completed_execution(1, 1, "def complete():\n    return 42", "", Vec::new()),
            false,
        );
        let complete_buffer = rendered_buffer(&mut complete, 60);
        let def = complete_buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "d" && cell.fg == Color::Magenta)
            .expect("highlighted def");
        assert_eq!(def.bg, COMPLETE_ARENA);

        let mut live = Transcript::default();
        live.apply(RuntimeEvent::RunStarted { run_id: RunId(2) }, false);
        live.apply(
            RuntimeEvent::ModelProgress {
                run_id: RunId(2),
                step_id: ModelStepId(1),
                progress: ModelProgress::DecodedPythonChunk {
                    code: "def incomplete(".to_owned(),
                },
            },
            false,
        );
        let live_buffer = rendered_buffer(&mut live, 60);
        assert!(live_buffer.content.iter().any(|cell| cell.symbol() == "d"
            && cell.fg == Color::Magenta
            && cell.bg == IN_PROGRESS_ARENA));

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
                .any(|cell| cell.symbol() == "d" && cell.fg == TEXT && cell.bg == COMPLETE_ARENA)
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
        assert!(automatic.contains("[1 code lines omitted]"));
        assert!(transcript.toggle_reasoning());
        let hidden = rendered_layout_text(&mut transcript);
        assert_eq!(
            hidden.matches("reasoning hidden · Ctrl-T to show").count(),
            1
        );
        assert!(!hidden.contains("reasoning\n"));
        assert!(transcript.toggle_arena_expansion());
        let expanded = rendered_layout_text(&mut transcript);
        assert!(expanded.contains("code 33"));
        let request = expanded.find("you: request").unwrap();
        let placeholder = expanded.find("reasoning hidden").unwrap();
        let arena = expanded.find("[run 1 step 1]").unwrap();
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
            let layout = transcript.layout(width);
            assert!(
                layout.blocks.iter().all(|block| {
                    block.left_padding.saturating_add(block.right_padding) < width
                })
            );
        }
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
