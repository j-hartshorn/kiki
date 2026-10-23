use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::pricing::RunCostEstimate;
use crate::runtime::{Record, RestoredTimelineItem, RuntimeEvent};
use crate::subagents::{TaskId, TaskObservation, TaskRecord, TaskState};
use crate::theme::Theme;
use crate::transcript::{
    format_subagent_cost, format_subagent_elapsed, format_subagent_steps, muted_page_style,
    normalise_terminal_text, subagent_glyph, subagent_retry_count, subagent_state_label,
};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;

const MAX_ENTRIES: usize = 4;

#[derive(Clone, PartialEq, Eq)]
struct Observation {
    state: TaskState,
    step_count: u64,
    elapsed: Duration,
    cost: RunCostEstimate,
}

impl From<&TaskObservation> for Observation {
    fn from(observation: &TaskObservation) -> Self {
        Self {
            state: observation.state(),
            step_count: observation.step_count(),
            elapsed: observation.elapsed(),
            cost: observation.cost(),
        }
    }
}

#[derive(Clone)]
struct Entry {
    parent: Option<String>,
    prompt: String,
    model: String,
    retry_count: u64,
    record_revision: u64,
    observation_revision: u64,
    observation: Observation,
    observed_at: Instant,
}

impl Entry {
    fn from_record(record: &TaskRecord, now: Instant) -> Self {
        Self {
            parent: record.parent().map(ToString::to_string),
            prompt: record.parameters().prompt().to_owned(),
            model: record.parameters().selection().model().id.to_owned(),
            retry_count: subagent_retry_count(record),
            record_revision: record.revision(),
            observation_revision: record.revision(),
            observation: Observation::from(record.observation()),
            observed_at: now,
        }
    }

    fn elapsed(&self, now: Instant) -> Duration {
        if is_active(self.observation.state) {
            self.observation
                .elapsed
                .saturating_add(now.saturating_duration_since(self.observed_at))
        } else {
            self.observation.elapsed
        }
    }
}

#[derive(Default)]
pub(super) struct SubagentMonitor {
    entries: BTreeMap<String, Entry>,
    operationally_active: BTreeSet<String>,
    admission_order: Vec<String>,
    cleared_terminals: BTreeSet<String>,
    visible_elapsed: BTreeMap<String, u64>,
}

pub(super) struct MonitorLayout {
    lines: Vec<Line<'static>>,
}

impl MonitorLayout {
    pub(super) fn height(&self) -> u16 {
        u16::try_from(self.lines.len()).unwrap_or(u16::MAX)
    }

    pub(super) fn render(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        if !area.is_empty() {
            frame.render_widget(Paragraph::new(Text::from(self.lines.clone())), area);
        }
    }
}

impl SubagentMonitor {
    pub(super) fn restored(timeline: &[RestoredTimelineItem]) -> Self {
        let mut monitor = Self::default();
        let now = Instant::now();
        for item in timeline {
            match item {
                RestoredTimelineItem::Record(record) => {
                    monitor.apply(
                        &RuntimeEvent::RecordAppended {
                            record: record.clone(),
                            progress_gap: false,
                        },
                        now,
                    );
                }
                RestoredTimelineItem::SubagentSpawned(record) => {
                    if monitor.apply_record(record, now) {
                        monitor.mark_operationally_active(record.id());
                    }
                }
                RestoredTimelineItem::SubagentFinished(record) => {
                    monitor.apply_record(record, now);
                    monitor.operationally_active.remove(record.id().as_str());
                }
            }
        }
        monitor
    }

    pub(super) fn apply(&mut self, event: &RuntimeEvent, now: Instant) -> bool {
        match event {
            RuntimeEvent::SubagentSpawned { record } => self.apply_record(record, now),
            RuntimeEvent::SubagentRecord { record } => {
                let record_changed = self.apply_record(record, now);
                let operation_changed = record_changed
                    && record.observation().state().is_terminal()
                    && self.mark_operationally_active(record.id());
                record_changed || operation_changed
            }
            RuntimeEvent::SubagentFinished { record } => {
                let record_changed = self.apply_record(record, now);
                let operation_changed = self.mark_operationally_active(record.id());
                record_changed || operation_changed
            }
            RuntimeEvent::SubagentOperationSettled { id } => {
                self.operationally_active.remove(id.as_str())
            }
            RuntimeEvent::SubagentStatus {
                id,
                revision,
                observation,
            } => {
                let status_changed = self.apply_status(id, *revision, observation, now);
                let operation_changed = status_changed
                    && observation.state().is_terminal()
                    && self.mark_operationally_active(id);
                status_changed || operation_changed
            }
            RuntimeEvent::RecordAppended { record, .. }
                if matches!(record.record, Record::UserRequest { .. }) =>
            {
                self.clear_terminals()
            }
            _ => false,
        }
    }

    fn apply_record(&mut self, record: &TaskRecord, now: Instant) -> bool {
        let id = record.id().as_str();
        if self.cleared_terminals.contains(id) {
            return false;
        }
        let Some(entry) = self.entries.get_mut(id) else {
            self.admission_order.push(id.to_owned());
            self.entries
                .insert(id.to_owned(), Entry::from_record(record, now));
            return true;
        };
        if record.revision() <= entry.record_revision {
            return false;
        }
        entry.parent = record.parent().map(ToString::to_string);
        entry.prompt = record.parameters().prompt().to_owned();
        entry.model = record.parameters().selection().model().id.to_owned();
        entry.retry_count = subagent_retry_count(record);
        entry.record_revision = record.revision();
        if record.revision() >= entry.observation_revision {
            entry.observation_revision = record.revision();
            entry.observation = Observation::from(record.observation());
            entry.observed_at = now;
        }
        true
    }

    fn apply_status(
        &mut self,
        id: &TaskId,
        revision: u64,
        observation: &TaskObservation,
        now: Instant,
    ) -> bool {
        let id = id.as_str();
        if self.cleared_terminals.contains(id) {
            return false;
        }
        let Some(entry) = self.entries.get_mut(id) else {
            return false;
        };
        if revision <= entry.observation_revision {
            return false;
        }
        entry.observation_revision = revision;
        entry.observation = Observation::from(observation);
        entry.observed_at = now;
        true
    }

    fn clear_terminals(&mut self) -> bool {
        let mut ancestors = BTreeSet::new();
        for (id, entry) in &self.entries {
            if !self.is_operationally_active(id, entry) {
                continue;
            }
            let mut parent = entry.parent.clone();
            while let Some(id) = parent {
                if !ancestors.insert(id.clone()) {
                    break;
                }
                parent = self.entries.get(&id).and_then(|entry| entry.parent.clone());
            }
        }

        let mut cleared = false;
        let operationally_active = &self.operationally_active;
        self.entries.retain(|id, entry| {
            if is_active(entry.observation.state)
                || operationally_active.contains(id)
                || ancestors.contains(id)
            {
                true
            } else {
                self.cleared_terminals.insert(id.clone());
                cleared = true;
                false
            }
        });
        self.admission_order
            .retain(|id| self.entries.contains_key(id));
        self.visible_elapsed
            .retain(|id, _| self.entries.contains_key(id));
        cleared
    }

    pub(super) fn layout(
        &mut self,
        theme: &Theme,
        width: u16,
        available_rows: u16,
        now: Instant,
    ) -> MonitorLayout {
        self.visible_elapsed.clear();
        if self.entries.is_empty() || available_rows == 0 {
            return MonitorLayout { lines: Vec::new() };
        }

        let forest = self.forest();
        let selected_path = selected_path(&forest);
        let available_rows = usize::from(available_rows);
        let (projection, padding) = self.fit_projection(&forest, selected_path, available_rows);

        let (mut lines, visible_elapsed) =
            self.render_projection(theme, &forest, &projection, width, now);
        if padding {
            lines.insert(0, Line::raw(""));
            lines.push(Line::raw(""));
        }
        self.visible_elapsed.extend(visible_elapsed);
        MonitorLayout { lines }
    }

    pub(super) fn has_active_entries(&self) -> bool {
        self.entries
            .iter()
            .any(|(id, entry)| self.is_operationally_active(id, entry))
    }

    fn mark_operationally_active(&mut self, id: &TaskId) -> bool {
        self.entries.contains_key(id.as_str()) && self.operationally_active.insert(id.to_string())
    }

    fn is_operationally_active(&self, id: &str, entry: &Entry) -> bool {
        is_active(entry.observation.state) || self.operationally_active.contains(id)
    }

    pub(super) fn tick(&self, now: Instant) -> bool {
        self.visible_elapsed.iter().any(|(id, second)| {
            self.entries.get(id).is_some_and(|entry| {
                is_active(entry.observation.state) && entry.elapsed(now).as_secs() != *second
            })
        })
    }

    #[cfg(test)]
    pub(super) fn insert_for_test(&mut self, id: &str, state: TaskState, now: Instant) {
        self.admission_order.push(id.to_owned());
        self.entries.insert(
            id.to_owned(),
            Entry {
                parent: None,
                prompt: "test child".to_owned(),
                model: "openai/gpt-5.6-sol".to_owned(),
                retry_count: 0,
                record_revision: 1,
                observation_revision: 1,
                observation: Observation {
                    state,
                    step_count: 0,
                    elapsed: Duration::ZERO,
                    cost: RunCostEstimate::Unavailable,
                },
                observed_at: now,
            },
        );
    }

    fn forest(&self) -> Forest {
        let entries = self
            .entries
            .iter()
            .map(|(id, entry)| (id.as_str(), entry))
            .collect::<HashMap<_, _>>();
        let mut forest = Forest::default();
        for (admission, id) in self.admission_order.iter().enumerate() {
            let Some(entry) = entries.get(id.as_str()) else {
                continue;
            };
            forest.admission.insert(id.clone(), admission);
            if self.is_operationally_active(id, entry) {
                forest.active.insert(id.clone());
            }
            if let Some(parent) = entry
                .parent
                .as_deref()
                .filter(|parent| entries.contains_key(*parent))
            {
                forest
                    .children
                    .entry(parent.to_owned())
                    .or_default()
                    .push(id.clone());
                forest.parents.insert(id.clone(), parent.to_owned());
            } else {
                forest.roots.push(id.clone());
            }
        }

        let mut active_roots = HashSet::new();
        for root in &forest.roots {
            let mut stack = vec![root.as_str()];
            while let Some(id) = stack.pop() {
                if forest.active.contains(id) {
                    active_roots.insert(root.clone());
                    break;
                }
                stack.extend(forest.children(id).iter().map(String::as_str));
            }
        }
        forest.active_roots = active_roots;
        forest
    }

    fn fit_projection(
        &self,
        forest: &Forest,
        selected_path: Vec<String>,
        available_rows: usize,
    ) -> (Projection, bool) {
        let mut projection = Projection::new(forest, selected_path);
        let line_count = projection.line_count();
        if line_count <= available_rows {
            return (projection, line_count.saturating_add(2) <= available_rows);
        }

        for root in projection.roots.clone().into_iter().rev() {
            projection.collapsed_root_summaries.remove(&root);
            if projection.line_count() <= available_rows {
                return (projection, false);
            }
        }
        projection.show_anchor_descendants = false;
        if projection.line_count() <= available_rows {
            return (projection, false);
        }

        for depth in projection.sibling_summaries.clone().into_iter().rev() {
            projection.sibling_summaries.remove(&depth);
            if projection.line_count() <= available_rows {
                return (projection, false);
            }
        }
        projection.show_root_summary = false;
        if projection.line_count() <= available_rows {
            return (projection, false);
        }

        while projection.roots.pop().is_some() {
            if projection.line_count() <= available_rows {
                return (projection, false);
            }
        }

        while projection.selected_count > 1 {
            projection.selected_count = projection.selected_count.saturating_sub(1);
            projection.show_deeper = true;
            if projection.line_count() <= available_rows {
                return (projection, false);
            }
        }

        projection.show_deeper = false;
        (projection, false)
    }

    fn render_projection(
        &self,
        theme: &Theme,
        forest: &Forest,
        projection: &Projection,
        width: u16,
        now: Instant,
    ) -> (Vec<Line<'static>>, Vec<(String, u64)>) {
        let mut lines = Vec::new();
        let mut visible_elapsed = Vec::new();
        for (depth, id) in projection
            .selected_path
            .iter()
            .take(projection.selected_count)
            .enumerate()
        {
            render_node(
                theme,
                &mut lines,
                &mut visible_elapsed,
                (id, &self.entries[id]),
                depth,
                width,
                now,
            );
        }

        let anchor = &projection.selected_path[projection.selected_count.saturating_sub(1)];
        if projection.show_deeper && projection.selected_count < projection.selected_path.len() {
            let (descendant_count, active_descendant_count) = forest.descendant_counts(anchor);
            lines.push(summary_line(
                projection.selected_count,
                &format!("+{descendant_count} deeper · {active_descendant_count} active"),
                width,
            ));
        } else if projection.show_anchor_descendants {
            let (descendant_count, _) = forest.descendant_counts(anchor);
            lines.push(summary_line(
                projection.selected_count,
                &format!("+{descendant_count} descendants"),
                width,
            ));
        }

        for depth in (1..projection.selected_count).rev() {
            if projection.sibling_summaries.contains(&depth) {
                let parent = &projection.selected_path[depth.saturating_sub(1)];
                let child = &projection.selected_path[depth];
                let siblings = forest
                    .children(parent)
                    .iter()
                    .filter(|id| *id != child)
                    .count();
                lines.push(summary_line(depth, &format!("+{siblings} siblings"), width));
            }
        }

        for root in &projection.roots {
            render_node(
                theme,
                &mut lines,
                &mut visible_elapsed,
                (root, &self.entries[root]),
                0,
                width,
                now,
            );
            if projection.collapsed_root_summaries.contains(root) {
                lines.push(summary_line(
                    1,
                    &format!("+{} descendants", forest.descendant_counts(root).0),
                    width,
                ));
            }
        }

        if projection.show_root_summary {
            let omitted = forest
                .roots
                .len()
                .saturating_sub(1)
                .saturating_sub(projection.roots.len());
            lines.push(Line::raw(clipped(&format!("  … +{omitted} roots"), width)));
        }

        (lines, visible_elapsed)
    }
}

#[derive(Default)]
struct Forest {
    roots: Vec<String>,
    children: HashMap<String, Vec<String>>,
    parents: HashMap<String, String>,
    admission: HashMap<String, usize>,
    active: HashSet<String>,
    active_roots: HashSet<String>,
}

impl Forest {
    fn children(&self, id: &str) -> &[String] {
        self.children.get(id).map(Vec::as_slice).unwrap_or(&[])
    }

    fn descendant_counts(&self, id: &str) -> (usize, usize) {
        let mut descendant_count = 0_usize;
        let mut active_descendant_count = 0_usize;
        let mut stack = self
            .children(id)
            .iter()
            .rev()
            .map(String::as_str)
            .collect::<Vec<_>>();
        while let Some(child) = stack.pop() {
            descendant_count = descendant_count.saturating_add(1);
            active_descendant_count =
                active_descendant_count.saturating_add(usize::from(self.active.contains(child)));
            stack.extend(self.children(child).iter().rev().map(String::as_str));
        }
        (descendant_count, active_descendant_count)
    }
}

#[derive(Clone)]
struct Projection {
    selected_path: Vec<String>,
    selected_count: usize,
    roots: Vec<String>,
    collapsed_root_summaries: BTreeSet<String>,
    sibling_summaries: BTreeSet<usize>,
    show_anchor_descendants: bool,
    show_deeper: bool,
    show_root_summary: bool,
}

impl Projection {
    fn new(forest: &Forest, selected_path: Vec<String>) -> Self {
        let selected_count = selected_path.len().min(MAX_ENTRIES);
        let selected_root = selected_path
            .first()
            .expect("a retained forest has a selected root");
        let (active_roots, terminal_roots): (Vec<_>, Vec<_>) = forest
            .roots
            .iter()
            .filter(|root| *root != selected_root)
            .cloned()
            .partition(|root| forest.active_roots.contains(root));
        let roots = active_roots
            .into_iter()
            .chain(terminal_roots)
            .take(MAX_ENTRIES.saturating_sub(selected_count))
            .collect::<Vec<_>>();
        let collapsed_root_summaries = roots
            .iter()
            .filter(|root| !forest.children(root).is_empty())
            .cloned()
            .collect();
        let sibling_summaries = (1..selected_count)
            .filter(|depth| {
                let parent = &selected_path[depth.saturating_sub(1)];
                let child = &selected_path[*depth];
                forest.children(parent).iter().any(|id| id != child)
            })
            .collect();
        let anchor = &selected_path[selected_count.saturating_sub(1)];
        let show_anchor_descendants =
            selected_count == selected_path.len() && !forest.children(anchor).is_empty();
        let show_root_summary = forest.roots.len() > roots.len().saturating_add(1);
        Self {
            selected_path,
            selected_count,
            roots,
            collapsed_root_summaries,
            sibling_summaries,
            show_anchor_descendants,
            show_deeper: true,
            show_root_summary,
        }
    }

    fn line_count(&self) -> usize {
        let mut count = self.selected_count.saturating_add(self.roots.len());
        if self.show_deeper && self.selected_count < self.selected_path.len() {
            count = count.saturating_add(1);
        }
        if self.show_anchor_descendants {
            count = count.saturating_add(1);
        }
        count = count.saturating_add(self.sibling_summaries.len());
        count = count.saturating_add(self.collapsed_root_summaries.len());
        count.saturating_add(usize::from(self.show_root_summary))
    }
}

fn selected_path(forest: &Forest) -> Vec<String> {
    let mut active_candidate = None;
    let mut leaf_candidate = None;
    let mut stack = forest
        .roots
        .iter()
        .rev()
        .map(|root| (root.as_str(), 0))
        .collect::<Vec<_>>();

    while let Some((id, depth)) = stack.pop() {
        if forest.active.contains(id) {
            choose_candidate(&mut active_candidate, id, depth, forest);
        }
        let children = forest.children(id);
        if children.is_empty() {
            choose_candidate(&mut leaf_candidate, id, depth, forest);
        } else {
            stack.extend(
                children
                    .iter()
                    .rev()
                    .map(|child| (child.as_str(), depth.saturating_add(1))),
            );
        }
    }

    let mut id = active_candidate
        .or(leaf_candidate)
        .expect("a retained forest has a candidate")
        .0;
    let mut path = vec![id.clone()];
    while let Some(parent) = forest.parents.get(&id) {
        path.push(parent.clone());
        id = parent.clone();
    }
    path.reverse();
    path
}

fn choose_candidate(
    selected: &mut Option<(String, usize)>,
    id: &str,
    depth: usize,
    forest: &Forest,
) {
    if selected.as_ref().is_none_or(|(current, current_depth)| {
        depth > *current_depth
            || (depth == *current_depth && forest.admission[id] < forest.admission[current])
    }) {
        *selected = Some((id.to_owned(), depth));
    }
}

fn render_node(
    theme: &Theme,
    lines: &mut Vec<Line<'static>>,
    visible_elapsed: &mut Vec<(String, u64)>,
    node: (&str, &Entry),
    depth: usize,
    width: u16,
    now: Instant,
) {
    let (id, entry) = node;
    let elapsed = entry.elapsed(now);
    let indent = header_indent(depth);
    let lead_width = Line::raw(&indent).width().saturating_add(1);
    let header = header_at(id, entry, elapsed, width, lead_width);
    if is_active(entry.observation.state) && header.shows_elapsed {
        visible_elapsed.push((id.to_owned(), elapsed.as_secs()));
    }
    let style = terminal_style(theme, entry.observation.state);
    let glyph = subagent_glyph(theme, entry.observation.state);
    if usize::from(width) <= lead_width {
        lines.push(Line::styled(
            clipped(&format!("{indent}{}", glyph.content), width),
            style,
        ));
        return;
    }
    let mut spans = vec![
        Span::styled(indent, style),
        glyph,
        Span::styled(header.text, style),
    ];
    if let Some(prompt) = header.prompt {
        spans.push(Span::styled(prompt, muted_page_style(theme)));
    }
    lines.push(Line::from(spans));
}

fn header_indent(depth: usize) -> String {
    if depth == 0 {
        "  ".to_owned()
    } else {
        format!("  {}└─", "  ".repeat(depth))
    }
}

fn summary_line(summary_depth: usize, label: &str, width: u16) -> Line<'static> {
    Line::raw(clipped(
        &format!("  {}└─… {label}", "  ".repeat(summary_depth)),
        width,
    ))
}

fn is_active(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Starting | TaskState::Running | TaskState::Stopping
    )
}

fn terminal_style(theme: &Theme, state: TaskState) -> Style {
    if is_active(state) {
        Style::default()
    } else {
        muted_page_style(theme)
    }
}

struct Header {
    text: String,
    prompt: Option<String>,
    shows_elapsed: bool,
}

const MIN_PROMPT_WIDTH: usize = 8;

#[cfg(test)]
fn header(id: &str, entry: &Entry, elapsed: Duration, width: u16) -> Header {
    header_at(id, entry, elapsed, width, 3)
}

fn header_at(id: &str, entry: &Entry, elapsed: Duration, width: u16, lead_width: usize) -> Header {
    let width = usize::from(width).saturating_sub(lead_width);
    let prefix = format!(
        " subagent:{id}{}",
        subagent_state_label(entry.observation.state)
    );
    let model = entry.model.clone();
    let mut steps = Some(format_subagent_steps(
        entry.observation.step_count,
        entry.retry_count,
    ));
    let mut runtime = Some(format_subagent_elapsed(elapsed));
    let mut cost = format_subagent_cost(entry.observation.cost);

    while Line::raw(join_fields(
        &prefix,
        [
            Some(model.as_str()),
            steps.as_deref(),
            runtime.as_deref(),
            cost.as_deref(),
        ],
    ))
    .width()
        > width
    {
        if steps.take().is_some() || cost.take().is_some() || runtime.take().is_some() {
            continue;
        }
        let prefix_width = Line::raw(&prefix).width();
        if prefix_width >= width {
            return Header {
                text: crate::transcript::truncate_end(&prefix, width),
                prompt: None,
                shows_elapsed: false,
            };
        }
        let model_width = width.saturating_sub(prefix_width).saturating_sub(2);
        let model =
            (model_width > 0).then(|| crate::transcript::truncate_front(&model, model_width));
        return Header {
            text: join_fields(&prefix, [model.as_deref(), None, None, None]),
            prompt: None,
            shows_elapsed: false,
        };
    }

    let text = join_fields(
        &prefix,
        [
            Some(model.as_str()),
            steps.as_deref(),
            runtime.as_deref(),
            cost.as_deref(),
        ],
    );
    let prompt_width = width
        .saturating_sub(Line::raw(&text).width())
        .saturating_sub(2);
    Header {
        prompt: (prompt_width >= MIN_PROMPT_WIDTH)
            .then(|| format!("  {}", prompt_preview(&entry.prompt, prompt_width))),
        text,
        shows_elapsed: runtime.is_some(),
    }
}

fn join_fields<'a>(prefix: &str, fields: impl IntoIterator<Item = Option<&'a str>>) -> String {
    let mut text = prefix.to_owned();
    for field in fields.into_iter().flatten() {
        text.push_str("  ");
        text.push_str(field);
    }
    text
}

fn prompt_preview(prompt: &str, width: usize) -> String {
    let prompt = normalise_terminal_text(prompt)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    crate::transcript::truncate_end(&prompt, width)
}

fn clipped(text: &str, width: u16) -> String {
    crate::transcript::truncate_end(text, usize::from(width))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use ratatui::text::Line;

    use super::{Entry, Observation, SubagentMonitor, header, prompt_preview};
    use crate::config::{ModelSelection, models::SOL};
    use crate::model::{AttemptAccounting, ModelUsage};
    use crate::pricing::{RunCostEstimate, estimate_run_cost};
    use crate::runtime::{Record, RestoredTimelineItem, RunId, RuntimeEvent, StoredRecord};
    use crate::subagents::{TaskRecord, TaskState};

    fn entry(state: TaskState, cost: RunCostEstimate, now: Instant) -> Entry {
        Entry {
            parent: None,
            prompt: "Review the cancellation paths and identify every missing edge case".to_owned(),
            model: "openai/gpt-5.6-luna".to_owned(),
            retry_count: 0,
            record_revision: 1,
            observation_revision: 1,
            observation: Observation {
                state,
                step_count: 6,
                elapsed: Duration::from_secs(287),
                cost,
            },
            observed_at: now,
        }
    }

    fn priced_cost(lower_bound: bool) -> RunCostEstimate {
        let attempt = AttemptAccounting::KnownUsage(ModelUsage {
            model: SOL.id.to_owned(),
            service_tier: Some("default".to_owned()),
            input_tokens: Some(1),
            cached_input_tokens: Some(0),
            cache_write_input_tokens: Some(0),
            cache_write_1h_input_tokens: 0,
            output_tokens: Some(1),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(2),
        });
        if lower_bound {
            estimate_run_cost([&attempt, &AttemptAccounting::UsageUnavailable])
        } else {
            estimate_run_cost([&attempt])
        }
    }

    #[test]
    fn restored_monitor_preserves_task_finished_after_later_request() {
        let task = TaskRecord::for_test(
            "0",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let request = |run_id, request: &str| {
            RestoredTimelineItem::Record(StoredRecord {
                run_id: RunId(run_id),
                record: Record::UserRequest {
                    request: request.to_owned(),
                    selection: None,
                },
            })
        };

        let mut monitor = SubagentMonitor::restored(&[
            request(1, "request A"),
            RestoredTimelineItem::SubagentSpawned(task.clone()),
            request(2, "request B"),
            RestoredTimelineItem::SubagentFinished(task),
        ]);

        assert_eq!(
            monitor
                .entries
                .get("0")
                .map(|entry| entry.observation.state),
            Some(TaskState::Completed)
        );
        assert!(!monitor.has_active_entries());
        assert!(layout_text(&mut monitor, 80, 4, Instant::now()).contains("✓ subagent:0"));
    }

    #[test]
    fn restored_monitor_honours_later_user_request_boundaries() {
        let first = TaskRecord::for_test(
            "00",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let second = TaskRecord::for_test(
            "01",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let request = |run_id| {
            RestoredTimelineItem::Record(StoredRecord {
                run_id: RunId(run_id),
                record: Record::UserRequest {
                    request: "saved request".to_owned(),
                    selection: None,
                },
            })
        };

        let monitor = SubagentMonitor::restored(&[
            request(1),
            RestoredTimelineItem::SubagentSpawned(first.clone()),
            RestoredTimelineItem::SubagentFinished(first),
            request(2),
            RestoredTimelineItem::SubagentSpawned(second),
        ]);

        assert!(!monitor.entries.contains_key("00"));
        assert!(monitor.entries.contains_key("01"));
    }

    #[test]
    fn restored_monitor_keeps_a_late_finished_task_visible() {
        let task = TaskRecord::for_test(
            "00",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let request = RestoredTimelineItem::Record(StoredRecord {
            run_id: RunId(1),
            record: Record::UserRequest {
                request: "saved request".to_owned(),
                selection: None,
            },
        });

        let mut monitor = SubagentMonitor::restored(&[
            request,
            RestoredTimelineItem::SubagentSpawned(task.clone()),
            RestoredTimelineItem::SubagentFinished(task),
        ]);
        assert_eq!(
            monitor
                .entries
                .get("00")
                .map(|entry| entry.observation.state),
            Some(TaskState::Completed)
        );
        assert!(!monitor.has_active_entries());
        assert!(layout_text(&mut monitor, 80, 4, Instant::now()).contains("✓ subagent:00"));
    }

    fn insert(monitor: &mut SubagentMonitor, id: &str, entry: Entry) {
        monitor.admission_order.push(id.to_owned());
        monitor.entries.insert(id.to_owned(), entry);
    }

    fn insert_task(
        monitor: &mut SubagentMonitor,
        id: &str,
        state: TaskState,
        parent: Option<&str>,
        now: Instant,
    ) {
        let mut task = entry(state, RunCostEstimate::Unavailable, now);
        task.parent = parent.map(str::to_owned);
        insert(monitor, id, task);
    }

    fn layout_text(monitor: &mut SubagentMonitor, width: u16, rows: u16, now: Instant) -> String {
        monitor
            .layout(&crate::theme::DARK, width, rows, now)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn monitor_rows_lead_with_shared_glyphs_and_end_with_the_prompt() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        monitor.insert_for_test("c1", TaskState::Running, now);
        monitor.insert_for_test("c2", TaskState::Failed, now);

        let layout = monitor.layout(&crate::theme::DARK, 80, 10, now);

        assert_eq!(
            layout
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "",
                "  \u{2026} subagent:c1  openai/gpt-5.6-sol  0 steps  00:00  test child",
                "  \u{2717} subagent:c2 failed  openai/gpt-5.6-sol  0 steps  00:00  test child",
                "",
            ]
        );
        assert_eq!(
            layout.lines[1].spans[1].style.fg,
            Some(crate::theme::DARK.warning)
        );
        assert_eq!(
            layout.lines[2].spans[1].style.fg,
            Some(crate::theme::DARK.error)
        );
        assert_eq!(
            layout.lines[1].spans.last().unwrap().style.fg,
            Some(crate::theme::DARK.muted)
        );
    }

    #[test]
    fn preview_collapses_whitespace_and_keeps_grapheme_ellipsis() {
        let preview = prompt_preview("日本🦀e\u{301}abcdefgh", 10);

        assert_eq!(preview, "日本🦀e\u{301}ab…");
        assert_eq!(Line::raw(&preview).width(), 10);
        assert_eq!(
            prompt_preview("first\nsecond\t\x1b", 80),
            "first second \\x1B"
        );
    }

    #[test]
    fn header_uses_qualified_model_steps_and_footer_cost_formatting() {
        let now = Instant::now();
        let complete = header(
            "child1",
            &entry(TaskState::Running, priced_cost(false), now),
            Duration::from_secs(287),
            120,
        );
        let lower_bound = header(
            "child1",
            &entry(TaskState::Running, priced_cost(true), now),
            Duration::from_secs(287),
            120,
        );
        let unavailable = header(
            "child1",
            &entry(TaskState::Running, RunCostEstimate::Unavailable, now),
            Duration::from_secs(287),
            120,
        );
        let mut retries = entry(TaskState::Running, RunCostEstimate::Unavailable, now);
        retries.retry_count = 2;
        let retries = header("child1", &retries, Duration::from_secs(287), 120);

        assert_eq!(
            complete.text,
            " subagent:child1  openai/gpt-5.6-luna  6 steps  04:47  $0.00002400"
        );
        assert_eq!(
            complete.prompt.as_deref(),
            Some("  Review the cancellation paths and identify every…")
        );
        assert!(lower_bound.text.ends_with("≥$0.00002400"));
        assert!(retries.text.contains("6 steps, 2 retries"));
        assert!(!unavailable.text.contains('$'));
    }

    #[test]
    fn rows_label_each_lifecycle_state() {
        let now = Instant::now();
        for (state, glyph, label) in [
            (TaskState::Starting, "…", " starting"),
            (TaskState::Running, "…", ""),
            (TaskState::Stopping, "…", " stopping"),
            (TaskState::Completed, "✓", ""),
            (TaskState::Failed, "✗", " failed"),
            (TaskState::TimedOut, "✗", " timed out"),
            (TaskState::Cancelled, "⊘", " cancelled"),
        ] {
            let mut monitor = SubagentMonitor::default();
            monitor.insert_for_test("child1", state, now);
            let text = layout_text(&mut monitor, 120, 1, now);
            assert_eq!(
                text,
                format!(
                    "  {glyph} subagent:child1{label}  openai/gpt-5.6-sol  0 steps  00:00  test child"
                )
            );
        }
    }

    #[test]
    fn width_pressure_drops_fields_before_front_truncating_the_model() {
        let now = Instant::now();
        let entry = entry(TaskState::Running, priced_cost(false), now);
        let complete = header("child1", &entry, Duration::from_secs(287), 120);
        let without_steps = header("child1", &entry, Duration::from_secs(287), 60);
        let prefix = " subagent:child1";
        let shortened_model = header(
            "child1",
            &entry,
            Duration::from_secs(287),
            u16::try_from(Line::raw(prefix).width() + 6).unwrap(),
        );
        let without_model = header(
            "child1",
            &entry,
            Duration::from_secs(287),
            u16::try_from(Line::raw(prefix).width() + 3).unwrap(),
        );

        assert!(complete.text.contains("6 steps"));
        assert!(!without_steps.text.contains("6 steps"));
        assert!(without_steps.text.contains("$0.00002400"));
        assert!(shortened_model.text.ends_with("  …"));
        assert_eq!(without_model.text, prefix);
    }

    #[test]
    fn deepest_active_branch_renders_before_an_earlier_root() {
        let now = Instant::now();
        let record = |id, parent| {
            TaskRecord::for_test(
                id,
                1,
                ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
                parent,
                0,
                Vec::new(),
            )
        };
        let mut monitor = SubagentMonitor::default();
        for record in [
            record("00", None),
            record("01", Some("00")),
            record("02", Some("01")),
            record("03", None),
        ] {
            assert!(monitor.apply(&RuntimeEvent::SubagentSpawned { record }, now));
        }
        monitor.entries.get_mut("02").unwrap().observation.state = TaskState::Running;
        monitor.entries.get_mut("03").unwrap().observation.state = TaskState::Running;

        let text = monitor
            .layout(&crate::theme::DARK, 120, 12, now)
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            text.find("subagent:00").unwrap() < text.find("subagent:03").unwrap(),
            "{text}"
        );
        assert!(text.contains("    └─✓ subagent:01"), "{text}");
        assert!(text.contains("      └─… subagent:02"), "{text}");
    }

    #[test]
    fn equal_depth_active_branches_choose_the_first_admitted_leaf() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Completed, None, now);
        insert_task(&mut monitor, "01", TaskState::Running, Some("00"), now);
        insert_task(&mut monitor, "02", TaskState::Completed, None, now);
        insert_task(&mut monitor, "03", TaskState::Running, Some("02"), now);

        let text = layout_text(&mut monitor, 120, 4, now);
        let rows = text.lines().collect::<Vec<_>>();

        assert_eq!(rows.len(), 4, "{text}");
        for (row, prefix) in rows.iter().zip([
            "  ✓ subagent:00",
            "    └─… subagent:01",
            "  ✓ subagent:02",
            "    └─… +1 descendants",
        ]) {
            assert!(row.starts_with(prefix), "expected `{prefix}`: {text}");
        }
    }

    #[test]
    fn active_root_precedes_an_earlier_terminal_root() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Completed, None, now);
        insert_task(&mut monitor, "01", TaskState::Running, None, now);

        let text = layout_text(&mut monitor, 120, 8, now);

        assert!(
            text.find("subagent:01").unwrap() < text.find("subagent:00").unwrap(),
            "{text}"
        );
    }

    #[test]
    fn terminal_fallback_selects_the_deepest_retained_branch() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Completed, None, now);
        insert_task(&mut monitor, "01", TaskState::Failed, Some("00"), now);
        insert_task(&mut monitor, "02", TaskState::Cancelled, Some("01"), now);
        insert_task(&mut monitor, "03", TaskState::Completed, None, now);

        let text = layout_text(&mut monitor, 120, 12, now);

        assert!(
            text.find("subagent:00").unwrap() < text.find("subagent:03").unwrap(),
            "{text}"
        );
        assert!(text.contains("      └─⊘ subagent:02 cancelled"), "{text}");
    }

    #[test]
    fn selected_path_uses_the_real_node_budget_before_collapsed_roots() {
        let now = Instant::now();
        for (depth, expected_roots) in [(1, 4), (2, 3), (3, 2), (4, 1)] {
            let mut monitor = SubagentMonitor::default();
            let mut parent = None;
            for number in 0..depth {
                let id = format!("{number:02x}");
                insert_task(
                    &mut monitor,
                    &id,
                    if number + 1 == depth {
                        TaskState::Running
                    } else {
                        TaskState::Completed
                    },
                    parent.as_deref(),
                    now,
                );
                parent = Some(id);
            }
            for number in depth..depth + 4 {
                insert_task(
                    &mut monitor,
                    &format!("{number:02x}"),
                    TaskState::Completed,
                    None,
                    now,
                );
            }

            let text = layout_text(&mut monitor, 120, 20, now);
            let root_headers = text
                .lines()
                .filter(|line| line.starts_with("  ") && !line.starts_with("    "))
                .filter(|line| line.contains("subagent:"))
                .count();
            assert_eq!(root_headers, expected_roots, "depth {depth}: {text}");
            assert_eq!(
                text.matches("subagent:").count(),
                4,
                "depth {depth}: {text}"
            );
        }
    }

    #[test]
    fn tree_rows_use_the_specified_indentation_and_end_with_the_prompt() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Completed, None, now);
        insert_task(&mut monitor, "01", TaskState::Running, Some("00"), now);
        monitor.entries.get_mut("00").unwrap().prompt = "root prompt".to_owned();
        monitor.entries.get_mut("01").unwrap().prompt = "child prompt".to_owned();

        let text = layout_text(&mut monitor, 120, 8, now);

        assert_eq!(
            text.split('\n').collect::<Vec<_>>(),
            [
                "",
                "  ✓ subagent:00  openai/gpt-5.6-luna  6 steps  04:47  root prompt",
                "    └─… subagent:01  openai/gpt-5.6-luna  6 steps  04:47  child prompt",
                "",
            ]
        );
    }

    #[test]
    fn summaries_count_siblings_collapsed_descendants_and_omitted_roots() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Completed, None, now);
        insert_task(&mut monitor, "01", TaskState::Completed, Some("00"), now);
        insert_task(&mut monitor, "02", TaskState::Running, Some("01"), now);
        insert_task(&mut monitor, "03", TaskState::Completed, Some("00"), now);
        insert_task(&mut monitor, "04", TaskState::Completed, Some("01"), now);
        insert_task(&mut monitor, "05", TaskState::Completed, Some("02"), now);
        insert_task(&mut monitor, "06", TaskState::Completed, None, now);
        insert_task(&mut monitor, "07", TaskState::Completed, Some("06"), now);
        insert_task(&mut monitor, "08", TaskState::Completed, None, now);

        let text = layout_text(&mut monitor, 120, 20, now);

        assert_eq!(text.matches("siblings").count(), 2, "{text}");
        assert!(text.contains("        └─… +1 descendants"), "{text}");
        assert!(text.contains("    └─… +1 descendants"), "{text}");
        assert!(text.contains("  … +1 roots"), "{text}");
    }

    #[test]
    fn deeper_summary_counts_hidden_descendants_and_active_work() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        for (id, parent, state) in [
            ("00", None, TaskState::Completed),
            ("01", Some("00"), TaskState::Completed),
            ("02", Some("01"), TaskState::Completed),
            ("03", Some("02"), TaskState::Completed),
            ("04", Some("03"), TaskState::Running),
            ("05", Some("04"), TaskState::Completed),
            ("06", Some("03"), TaskState::Completed),
            ("07", None, TaskState::Completed),
            ("08", None, TaskState::Completed),
        ] {
            insert_task(&mut monitor, id, state, parent, now);
        }

        let text = layout_text(&mut monitor, 120, 20, now);

        assert!(text.contains("        └─… +3 deeper · 1 active"), "{text}");
        assert!(text.contains("  … +2 roots"), "{text}");
    }

    #[test]
    fn active_work_below_four_terminal_ancestors_keeps_their_tree_context() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        let mut parent = None;
        for number in 0..5 {
            let id = format!("{number:02x}");
            insert_task(
                &mut monitor,
                &id,
                if number == 4 {
                    TaskState::Running
                } else {
                    TaskState::Completed
                },
                parent.as_deref(),
                now,
            );
            parent = Some(id);
        }

        let text = layout_text(&mut monitor, 120, 12, now);

        for id in ["00", "01", "02", "03"] {
            assert!(text.contains(&format!("subagent:{id}")), "{text}");
        }
        assert!(text.contains("+1 deeper · 1 active"), "{text}");
    }

    #[test]
    fn selected_active_leaf_summarises_its_terminal_children_as_descendants() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Running, None, now);
        insert_task(&mut monitor, "01", TaskState::Completed, Some("00"), now);
        insert_task(&mut monitor, "02", TaskState::Failed, Some("01"), now);

        let with_summary = layout_text(&mut monitor, 120, 2, now);
        let rows = with_summary.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), 2, "{with_summary}");
        assert!(rows[0].starts_with("  … subagent:00  "), "{with_summary}");
        assert_eq!(rows[1], "    └─… +2 descendants");

        let root_only = layout_text(&mut monitor, 120, 1, now);
        assert_eq!(root_only.lines().count(), 1, "{root_only}");
        assert!(root_only.starts_with("  … subagent:00  "), "{root_only}");
    }

    #[test]
    fn shortening_counts_a_hidden_active_leaf_and_its_terminal_children() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        for (id, parent, state) in [
            ("00", None, TaskState::Completed),
            ("01", Some("00"), TaskState::Completed),
            ("02", Some("01"), TaskState::Running),
            ("03", Some("02"), TaskState::Completed),
            ("04", Some("02"), TaskState::Failed),
        ] {
            insert_task(&mut monitor, id, state, parent, now);
        }

        let text = layout_text(&mut monitor, 120, 2, now);

        assert!(text.contains("subagent:00"), "{text}");
        assert!(!text.contains("subagent:01"), "{text}");
        assert!(!text.contains("subagent:02"), "{text}");
        assert!(text.contains("    └─… +4 deeper · 1 active"), "{text}");
    }

    #[test]
    fn unavailable_parent_degrades_to_a_root() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert_task(&mut monitor, "00", TaskState::Running, Some("ff"), now);
        insert_task(&mut monitor, "01", TaskState::Completed, None, now);

        let text = layout_text(&mut monitor, 120, 8, now);

        assert!(text.contains("  … subagent:00"), "{text}");
        assert!(!text.contains("└─… subagent:00"), "{text}");
    }

    #[test]
    fn finished_tasks_and_ancestors_persist_until_operation_settlement() {
        let now = Instant::now();
        let record = |id, parent| {
            TaskRecord::for_test(
                id,
                1,
                ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
                parent,
                0,
                Vec::new(),
            )
        };
        let request = |run_id| RuntimeEvent::RecordAppended {
            record: StoredRecord {
                run_id: crate::runtime::RunId(run_id),
                record: Record::UserRequest {
                    request: "accepted".to_owned(),
                    selection: None,
                },
            },
            progress_gap: false,
        };
        let root = record("00", None);
        let child = record("01", Some("00"));
        let mut monitor = SubagentMonitor::default();
        for record in [root.clone(), child.clone()] {
            assert!(monitor.apply(&RuntimeEvent::SubagentSpawned { record }, now));
        }

        assert!(monitor.apply(
            &RuntimeEvent::SubagentFinished {
                record: child.clone(),
            },
            now,
        ));
        assert!(!monitor.apply(&request(1), now));
        assert!(monitor.entries.contains_key("00"));
        assert!(monitor.entries.contains_key("01"));

        assert!(monitor.apply(
            &RuntimeEvent::SubagentOperationSettled {
                id: child.id().clone(),
            },
            now,
        ));
        assert!(monitor.apply(&request(2), now));
        assert!(monitor.entries.is_empty());
        assert!(!monitor.apply(&RuntimeEvent::SubagentFinished { record: root }, now));
    }

    #[test]
    fn height_shortening_reanchors_the_deeper_summary() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        let mut parent = None;
        for number in 0..6 {
            let id = format!("{number:02x}");
            insert_task(
                &mut monitor,
                &id,
                if number == 5 {
                    TaskState::Running
                } else {
                    TaskState::Completed
                },
                parent.as_deref(),
                now,
            );
            parent = Some(id);
        }

        for (rows, visible, deeper) in [
            (7, 4, Some("+2 deeper · 1 active")),
            (5, 4, Some("+2 deeper · 1 active")),
            (4, 3, Some("+3 deeper · 1 active")),
            (3, 2, Some("+4 deeper · 1 active")),
            (2, 1, Some("+5 deeper · 1 active")),
            (1, 1, None),
        ] {
            let text = layout_text(&mut monitor, 120, rows, now);
            assert_eq!(
                text.matches("subagent:").count(),
                visible,
                "rows {rows}: {text}"
            );
            assert_eq!(
                text.contains("deeper"),
                deeper.is_some(),
                "rows {rows}: {text}"
            );
            if let Some(deeper) = deeper {
                assert!(text.contains(deeper), "rows {rows}: {text}");
            }
        }
    }

    #[test]
    fn root_order_keeps_active_trees_before_terminal_trees() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        for (id, state) in [
            ("done01", TaskState::Completed),
            ("run001", TaskState::Running),
            ("fail01", TaskState::Failed),
            ("stop01", TaskState::Stopping),
            ("done02", TaskState::Completed),
        ] {
            insert(
                &mut monitor,
                id,
                entry(state, RunCostEstimate::Unavailable, now),
            );
        }

        let layout = monitor.layout(&crate::theme::DARK, 120, 11, now);
        let text = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        let positions = ["run001", "stop01", "done01", "fail01"].map(|id| text.find(id).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(text.contains("… +1 roots"), "{text}");
        assert!(!text.contains("done02"), "{text}");
    }

    #[test]
    fn terminal_rows_use_the_muted_style() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert(
            &mut monitor,
            "done01",
            entry(TaskState::Completed, RunCostEstimate::Unavailable, now),
        );

        let layout = monitor.layout(&crate::theme::DARK, 120, 4, now);

        let spans = &layout.lines[1].spans;
        assert_eq!(spans.len(), 4);
        for index in [0, 2, 3] {
            assert_eq!(
                spans[index].style,
                crate::transcript::muted_page_style(&crate::theme::DARK)
            );
        }
    }

    #[test]
    fn small_layouts_keep_the_active_row_or_omit_the_list_without_underflowing() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert(
            &mut monitor,
            "run001",
            entry(TaskState::Running, RunCostEstimate::Unavailable, now),
        );
        insert(
            &mut monitor,
            "done01",
            entry(TaskState::Completed, RunCostEstimate::Unavailable, now),
        );

        let single = monitor.layout(&crate::theme::DARK, 80, 1, now);
        assert_eq!(single.lines.len(), 1);
        assert_eq!(
            single.lines[0].to_string(),
            "  … subagent:run001  openai/gpt-5.6-luna  6 steps  04:47  Review the cancellati…"
        );
        assert!(
            monitor
                .layout(&crate::theme::DARK, 80, 0, now)
                .lines
                .is_empty()
        );
        for width in 0..12 {
            let layout = monitor.layout(&crate::theme::DARK, width, 4, now);
            assert!(
                layout
                    .lines
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
        }
    }

    #[test]
    fn timer_requests_redraws_only_for_visible_whole_active_seconds() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert(
            &mut monitor,
            "run001",
            entry(TaskState::Running, RunCostEstimate::Unavailable, now),
        );

        monitor.layout(&crate::theme::DARK, 120, 4, now);
        assert!(!monitor.tick(now + Duration::from_millis(999)));
        assert!(monitor.tick(now + Duration::from_secs(1)));
        monitor.layout(&crate::theme::DARK, 120, 4, now + Duration::from_secs(1));
        assert!(!monitor.tick(now + Duration::from_secs(1) + Duration::from_millis(999)));
        monitor.layout(&crate::theme::DARK, 30, 4, now + Duration::from_secs(2));
        assert!(!monitor.tick(now + Duration::from_secs(3)));
    }

    #[test]
    fn reliable_records_and_newer_statuses_survive_stale_updates_and_terminal_reset() {
        let now = Instant::now();
        let original = TaskRecord::for_test(
            "c1",
            1,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        );
        let status = TaskRecord::for_test(
            "c1",
            3,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        )
        .with_observation_for_test(Duration::from_secs(10), 0);
        let stale_record = TaskRecord::for_test(
            "c1",
            2,
            ModelSelection::new(&SOL, SOL.default_thinking).unwrap(),
            None,
            1,
            Vec::new(),
        )
        .with_observation_for_test(Duration::from_secs(2), 0);
        let mut monitor = SubagentMonitor::default();

        assert!(monitor.apply(
            &RuntimeEvent::SubagentSpawned {
                record: original.clone(),
            },
            now,
        ));
        assert!(monitor.apply(
            &RuntimeEvent::SubagentStatus {
                id: original.id().clone(),
                revision: 3,
                observation: status.observation().clone(),
            },
            now,
        ));
        assert!(monitor.apply(
            &RuntimeEvent::SubagentRecord {
                record: stale_record,
            },
            now,
        ));
        assert!(
            monitor
                .layout(&crate::theme::DARK, 120, 4, now)
                .lines
                .iter()
                .any(|line| line.to_string().contains("00:10"))
        );
        assert!(monitor.apply(
            &RuntimeEvent::SubagentOperationSettled {
                id: original.id().clone(),
            },
            now,
        ));
        assert!(monitor.apply(
            &RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: crate::runtime::RunId(1),
                    record: Record::UserRequest {
                        request: "accepted".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert!(!monitor.apply(&RuntimeEvent::SubagentFinished { record: status }, now,));
        assert!(
            monitor
                .layout(&crate::theme::DARK, 120, 4, now)
                .lines
                .is_empty()
        );
    }

    #[test]
    fn terminal_elapsed_is_frozen_and_resets_prune_stale_admission_order_ids() {
        let now = Instant::now();
        let mut monitor = SubagentMonitor::default();
        insert(
            &mut monitor,
            "done01",
            entry(TaskState::Completed, RunCostEstimate::Unavailable, now),
        );
        insert(
            &mut monitor,
            "run001",
            entry(TaskState::Running, RunCostEstimate::Unavailable, now),
        );

        assert!(monitor.apply(
            &RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: crate::runtime::RunId(1),
                    record: Record::UserRequest {
                        request: "accepted".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert!(!monitor.entries.contains_key("done01"));
        assert!(monitor.entries.contains_key("run001"));
        assert_eq!(monitor.admission_order, ["run001"]);
        insert(
            &mut monitor,
            "done02",
            entry(TaskState::Completed, RunCostEstimate::Unavailable, now),
        );
        assert!(monitor.apply(
            &RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: crate::runtime::RunId(2),
                    record: Record::UserRequest {
                        request: "another accepted request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert_eq!(monitor.admission_order, ["run001"]);
        monitor.entries.get_mut("run001").unwrap().observation.state = TaskState::Completed;
        let layout = monitor.layout(&crate::theme::DARK, 120, 4, now + Duration::from_secs(10));
        assert!(
            layout
                .lines
                .iter()
                .any(|line| line.to_string().contains("04:47"))
        );
        assert!(!monitor.tick(now + Duration::from_secs(11)));
        assert!(monitor.apply(
            &RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: crate::runtime::RunId(3),
                    record: Record::UserRequest {
                        request: "final accepted request".to_owned(),
                        selection: None,
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert!(monitor.admission_order.is_empty());
    }
}
