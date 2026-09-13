use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::pricing::RunCostEstimate;
use crate::runtime::{Record, RuntimeEvent};
use crate::subagents::{TaskId, TaskObservation, TaskRecord, TaskState};
use crate::transcript::{format_footer_usd, muted_page_style, normalise_terminal_text};
use ratatui::style::Style;
use ratatui::text::{Line, Text};
use ratatui::widgets::Paragraph;

const PREVIEW_PREFIX: &str = "    └ ";
const PREVIEW_GUTTER_WIDTH: usize = 6;
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
            prompt: record.parameters().prompt().to_owned(),
            model: record.parameters().selection().model().id.to_owned(),
            retry_count: retry_count(record),
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
    pub(super) fn apply(&mut self, event: &RuntimeEvent, now: Instant) -> bool {
        match event {
            RuntimeEvent::SubagentSpawned { record }
            | RuntimeEvent::SubagentRecord { record }
            | RuntimeEvent::SubagentFinished { record } => self.apply_record(record, now),
            RuntimeEvent::SubagentStatus {
                id,
                revision,
                observation,
            } => self.apply_status(id, *revision, observation, now),
            RuntimeEvent::RecordAppended { record, .. }
                if matches!(record.record, Record::UserRequest { .. }) =>
            {
                self.clear_terminals()
            }
            _ => false,
        }
    }

    fn apply_record(&mut self, record: &TaskRecord, now: Instant) -> bool {
        let id = record.id().to_string();
        if self.cleared_terminals.contains(&id) {
            return false;
        }
        let Some(entry) = self.entries.get_mut(&id) else {
            self.admission_order.push(id.clone());
            self.entries.insert(id, Entry::from_record(record, now));
            return true;
        };
        if record.revision() <= entry.record_revision {
            return false;
        }
        entry.prompt = record.parameters().prompt().to_owned();
        entry.model = record.parameters().selection().model().id.to_owned();
        entry.retry_count = retry_count(record);
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
        let mut cleared = false;
        self.entries.retain(|id, entry| {
            if is_active(entry.observation.state) {
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
        width: u16,
        available_rows: u16,
        now: Instant,
    ) -> MonitorLayout {
        self.visible_elapsed.clear();
        let entries = self.ordered_entries();
        if entries.is_empty() || available_rows == 0 {
            return MonitorLayout { lines: Vec::new() };
        }

        let available_rows = usize::from(available_rows);
        let Some((entry_count, overflow)) = fitting_entries(entries.len(), available_rows) else {
            return MonitorLayout {
                lines: vec![Line::raw(clipped(
                    &summary(
                        entries
                            .iter()
                            .filter(|(_, entry)| is_active(entry.observation.state))
                            .count(),
                        entries.len(),
                    ),
                    width,
                ))],
            };
        };
        let content_rows = entry_count.saturating_mul(2) + usize::from(overflow > 0);
        let padding = usize::from(available_rows >= content_rows.saturating_add(2));
        let mut lines = Vec::with_capacity(content_rows.saturating_add(padding.saturating_mul(2)));
        let mut visible_elapsed = Vec::new();
        if padding > 0 {
            lines.push(Line::raw(""));
        }
        for (id, entry) in entries.into_iter().take(entry_count) {
            let elapsed = entry.elapsed(now);
            let header = header(id, entry, elapsed, width);
            if header.shows_elapsed {
                visible_elapsed.push((id.clone(), elapsed.as_secs()));
            }
            let style = terminal_style(entry.observation.state);
            lines.push(Line::styled(header.text, style));
            lines.push(Line::styled(prompt_preview(&entry.prompt, width), style));
        }
        if overflow > 0 {
            lines.push(Line::raw(clipped(&overflow_label(overflow), width)));
        }
        if padding > 0 {
            lines.push(Line::raw(""));
        }
        self.visible_elapsed.extend(visible_elapsed);
        MonitorLayout { lines }
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

    fn ordered_entries(&self) -> Vec<(&String, &Entry)> {
        let mut active = Vec::new();
        let mut terminal = Vec::new();
        for id in &self.admission_order {
            let Some(entry) = self.entries.get(id) else {
                continue;
            };
            if is_active(entry.observation.state) {
                active.push((id, entry));
            } else {
                terminal.push((id, entry));
            }
        }
        active.extend(terminal);
        active
    }
}

fn retry_count(record: &TaskRecord) -> u64 {
    u64::try_from(
        record
            .attempt_evidence()
            .iter()
            .filter(|attempt| attempt.provider_attempt() > 1)
            .count(),
    )
    .unwrap_or(u64::MAX)
}

fn fitting_entries(total: usize, available_rows: usize) -> Option<(usize, usize)> {
    for count in (1..=total.min(MAX_ENTRIES)).rev() {
        let overflow = total.saturating_sub(count);
        if count.saturating_mul(2) + usize::from(overflow > 0) <= available_rows {
            return Some((count, overflow));
        }
    }
    None
}

fn is_active(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::Starting | TaskState::Running | TaskState::Stopping
    )
}

fn terminal_style(state: TaskState) -> Style {
    (!is_active(state))
        .then(muted_page_style)
        .unwrap_or_default()
}

struct Header {
    text: String,
    shows_elapsed: bool,
}

fn header(id: &str, entry: &Entry, elapsed: Duration, width: u16) -> Header {
    let prefix = format!(
        "  {} subagent:{}{}",
        indicator(entry.observation.state),
        id,
        state_label(entry.observation.state),
    );
    let model = entry.model.clone();
    let mut steps = Some(format_step_count(
        entry.observation.step_count,
        entry.retry_count,
    ));
    let mut runtime = Some(format_elapsed(elapsed));
    let mut cost = format_cost(entry.observation.cost);

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
        > usize::from(width)
    {
        if steps.take().is_some() || cost.take().is_some() || runtime.take().is_some() {
            continue;
        }
        let prefix_width = Line::raw(&prefix).width();
        if prefix_width >= usize::from(width) {
            return Header {
                text: clipped(&prefix, width),
                shows_elapsed: false,
            };
        }
        let model_width = usize::from(width)
            .saturating_sub(prefix_width)
            .saturating_sub(2);
        let model =
            (model_width > 0).then(|| crate::transcript::truncate_front(&model, model_width));
        return Header {
            text: join_fields(&prefix, [model.as_deref(), None, None, None]),
            shows_elapsed: false,
        };
    }

    Header {
        text: join_fields(
            &prefix,
            [
                Some(model.as_str()),
                steps.as_deref(),
                runtime.as_deref(),
                cost.as_deref(),
            ],
        ),
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

fn indicator(state: TaskState) -> &'static str {
    match state {
        TaskState::Starting | TaskState::Running => "•",
        TaskState::Stopping => "…",
        TaskState::Completed => "✓",
        TaskState::Failed | TaskState::TimedOut => "!",
        TaskState::Cancelled => "×",
    }
}

fn state_label(state: TaskState) -> &'static str {
    match state {
        TaskState::Starting => " starting",
        TaskState::Running | TaskState::Completed => "",
        TaskState::Stopping => " stopping",
        TaskState::Failed => " failed",
        TaskState::Cancelled => " cancelled",
        TaskState::TimedOut => " timed out",
    }
}

fn format_step_count(count: u64, retries: u64) -> String {
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

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn format_cost(cost: RunCostEstimate) -> Option<String> {
    match cost {
        RunCostEstimate::Complete(cost) => Some(format_footer_usd(cost.as_nanos())),
        RunCostEstimate::LowerBound(cost) => {
            Some(format!("≥{}", format_footer_usd(cost.as_nanos())))
        }
        RunCostEstimate::Unavailable => None,
    }
}

fn prompt_preview(prompt: &str, width: u16) -> String {
    let prompt = normalise_terminal_text(prompt)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let width = usize::from(width);
    if width < PREVIEW_GUTTER_WIDTH.saturating_mul(2) {
        return clipped(&format!("{PREVIEW_PREFIX}{prompt}"), width as u16);
    }
    let content_width = width.saturating_sub(PREVIEW_GUTTER_WIDTH.saturating_mul(2));
    format!(
        "{PREVIEW_PREFIX}{}{}",
        crate::transcript::truncate_end(&prompt, content_width),
        " ".repeat(PREVIEW_GUTTER_WIDTH)
    )
}

fn summary(active: usize, total: usize) -> String {
    format!(
        "  Subagents: {active} active, {} finished",
        total.saturating_sub(active)
    )
}

fn overflow_label(count: usize) -> String {
    format!(
        "  +{count} more subagent{}",
        if count == 1 { "" } else { "s" }
    )
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
    use crate::runtime::{Record, RuntimeEvent, StoredRecord};
    use crate::subagents::{TaskRecord, TaskState};

    fn entry(state: TaskState, cost: RunCostEstimate, now: Instant) -> Entry {
        Entry {
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

    fn insert(monitor: &mut SubagentMonitor, id: &str, entry: Entry) {
        monitor.admission_order.push(id.to_owned());
        monitor.entries.insert(id.to_owned(), entry);
    }

    #[test]
    fn preview_uses_six_cell_gutters_and_grapheme_ellipsis() {
        let preview = prompt_preview("日本🦀e\u{301}abcdefgh", 20);

        assert!(preview.starts_with("    └ "));
        assert!(preview.ends_with("      "));
        assert_eq!(Line::raw(&preview).width(), 20);
        assert!(preview.contains('…'));
        assert!(!preview.contains("e\u{301}a"));
        assert_eq!(
            prompt_preview("first\nsecond\t\x1b", 80).trim_end(),
            "    └ first second \\x1B"
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
            "  • subagent:child1  openai/gpt-5.6-luna  6 steps  04:47  $0.00002400"
        );
        assert!(lower_bound.text.ends_with("≥$0.00002400"));
        assert!(retries.text.contains("6 steps, 2 retries"));
        assert!(!unavailable.text.contains('$'));
    }

    #[test]
    fn header_labels_each_lifecycle_state() {
        let now = Instant::now();
        for (state, indicator, label) in [
            (TaskState::Starting, "•", " starting"),
            (TaskState::Running, "•", ""),
            (TaskState::Stopping, "…", " stopping"),
            (TaskState::Completed, "✓", ""),
            (TaskState::Failed, "!", " failed"),
            (TaskState::TimedOut, "!", " timed out"),
            (TaskState::Cancelled, "×", " cancelled"),
        ] {
            let header = header(
                "child1",
                &entry(state, RunCostEstimate::Unavailable, now),
                Duration::from_secs(287),
                120,
            );
            assert!(
                header
                    .text
                    .starts_with(&format!("  {indicator} subagent:child1{label}")),
                "{}",
                header.text
            );
        }
    }

    #[test]
    fn width_pressure_drops_fields_before_front_truncating_the_model() {
        let now = Instant::now();
        let entry = entry(TaskState::Running, priced_cost(false), now);
        let complete = header("child1", &entry, Duration::from_secs(287), 120);
        let without_steps = header("child1", &entry, Duration::from_secs(287), 60);
        let prefix = "  • subagent:child1";
        let shortened_model = header(
            "child1",
            &entry,
            Duration::from_secs(287),
            u16::try_from(Line::raw(prefix).width() + 3).unwrap(),
        );
        let without_model = header(
            "child1",
            &entry,
            Duration::from_secs(287),
            u16::try_from(Line::raw(prefix).width()).unwrap(),
        );

        assert!(complete.text.contains("6 steps"));
        assert!(!without_steps.text.contains("6 steps"));
        assert!(without_steps.text.contains("$0.00002400"));
        assert!(shortened_model.text.ends_with("  …"));
        assert_eq!(without_model.text, prefix);
    }

    #[test]
    fn active_entries_sort_before_terminals_and_overflow_in_admission_order() {
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

        let layout = monitor.layout(120, 11, now);
        let text = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");

        let positions = ["run001", "stop01", "done01", "fail01"].map(|id| text.find(id).unwrap());
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{text}");
        assert!(text.contains("+1 more subagent"), "{text}");
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

        let layout = monitor.layout(120, 4, now);

        assert_eq!(layout.lines[1].style, crate::transcript::muted_page_style());
        assert_eq!(layout.lines[2].style, crate::transcript::muted_page_style());
    }

    #[test]
    fn small_layouts_use_a_summary_or_omit_the_list_without_underflowing() {
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

        let summary = monitor.layout(80, 1, now);
        assert_eq!(
            summary.lines[0].to_string(),
            "  Subagents: 1 active, 1 finished"
        );
        assert!(monitor.layout(80, 0, now).lines.is_empty());
        for width in 0..12 {
            let layout = monitor.layout(width, 4, now);
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

        monitor.layout(120, 4, now);
        assert!(!monitor.tick(now + Duration::from_millis(999)));
        assert!(monitor.tick(now + Duration::from_secs(1)));
        monitor.layout(120, 4, now + Duration::from_secs(1));
        assert!(!monitor.tick(now + Duration::from_secs(1) + Duration::from_millis(999)));
        monitor.layout(120, 1, now + Duration::from_secs(2));
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
                .layout(120, 4, now)
                .lines
                .iter()
                .any(|line| line.to_string().contains("00:10"))
        );
        assert!(monitor.apply(
            &RuntimeEvent::RecordAppended {
                record: StoredRecord {
                    run_id: crate::runtime::RunId(1),
                    record: Record::UserRequest {
                        request: "accepted".to_owned(),
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert!(!monitor.apply(&RuntimeEvent::SubagentFinished { record: status }, now,));
        assert!(monitor.layout(120, 4, now).lines.is_empty());
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
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert_eq!(monitor.admission_order, ["run001"]);
        monitor.entries.get_mut("run001").unwrap().observation.state = TaskState::Completed;
        let layout = monitor.layout(120, 4, now + Duration::from_secs(10));
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
                    },
                },
                progress_gap: false,
            },
            now,
        ));
        assert!(monitor.admission_order.is_empty());
    }
}
