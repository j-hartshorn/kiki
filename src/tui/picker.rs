use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

pub(crate) const PICKER_MAX_WIDTH: u16 = 72;

#[derive(Clone, Copy)]
pub(crate) struct PickerItem<T> {
    pub(crate) group: &'static str,
    pub(crate) label: &'static str,
    pub(crate) value: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerAction<T> {
    None,
    Changed,
    Apply(T),
    Save(T),
    Close,
    NoMatch,
}

#[derive(Clone, Copy)]
pub(crate) enum DynamicPickerRole {
    Direct,
    Context,
}

pub(crate) struct DynamicPickerItem<T> {
    pub(crate) row: Line<'static>,
    pub(crate) role: DynamicPickerRole,
    pub(crate) value: T,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickerSavePolicy {
    Enabled,
    Disabled,
}

enum PickerRowRole {
    SearchableLabel(String),
    Dynamic(DynamicPickerRole),
}

struct PickerRow<T> {
    group: Option<&'static str>,
    row: Line<'static>,
    role: PickerRowRole,
    value: T,
}

struct PickerMatch {
    index: usize,
    direct: bool,
}

pub(crate) struct Picker<T> {
    title: &'static str,
    empty_message: &'static str,
    items: Vec<PickerRow<T>>,
    filter: String,
    canonical_filter: String,
    save_policy: PickerSavePolicy,
    focus: Option<usize>,
    scroll: usize,
    page_size: usize,
}

impl<T: Copy + Eq> Picker<T> {
    pub(crate) fn new(
        title: &'static str,
        empty_message: &'static str,
        items: impl IntoIterator<Item = PickerItem<T>>,
        current: T,
    ) -> Self {
        let items = items.into_iter().map(PickerRow::from).collect::<Vec<_>>();
        let focus = Self::initial_focus(&items, current);
        Self {
            title,
            empty_message,
            items,
            filter: String::new(),
            canonical_filter: String::new(),
            save_policy: PickerSavePolicy::Enabled,
            focus,
            scroll: 0,
            page_size: 1,
        }
    }

    pub(crate) fn new_dynamic(
        title: &'static str,
        empty_message: &'static str,
        items: impl IntoIterator<Item = DynamicPickerItem<T>>,
        current: T,
        save_policy: PickerSavePolicy,
    ) -> Self {
        let items = items.into_iter().map(PickerRow::from).collect::<Vec<_>>();
        let focus = Self::initial_focus(&items, current);
        Self {
            title,
            empty_message,
            items,
            filter: String::new(),
            canonical_filter: String::new(),
            save_policy,
            focus,
            scroll: 0,
            page_size: 1,
        }
    }

    pub(crate) fn replace_items(&mut self, items: impl IntoIterator<Item = DynamicPickerItem<T>>) {
        let focused_value = self.focus.map(|index| self.items[index].value);
        self.items = items.into_iter().map(PickerRow::from).collect();
        let focusable = self.focusable_matches();
        self.focus = focused_value
            .and_then(|value| {
                focusable
                    .iter()
                    .copied()
                    .find(|index| self.items[*index].value == value)
            })
            .or_else(|| focusable.first().copied());
    }

    pub(super) fn matches_canonical_text(&self, canonical_text: &str) -> bool {
        canonical_text.contains(&self.canonical_filter)
    }

    pub(crate) fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        self.filter_changed();
    }

    pub(crate) fn empty_message(&self) -> &'static str {
        self.empty_message
    }

    pub(crate) fn filter(&self) -> &str {
        &self.filter
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PickerAction<T> {
        match key.code {
            KeyCode::Esc => PickerAction::Close,
            KeyCode::Enter if key.modifiers.is_empty() => self.apply(false),
            KeyCode::Char('s' | 'S')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.save_policy == PickerSavePolicy::Enabled =>
            {
                self.apply(true)
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                if self.filter.pop().is_some() {
                    self.filter_changed();
                    PickerAction::Changed
                } else {
                    PickerAction::None
                }
            }
            KeyCode::Char(character)
                if !key.modifiers.intersects(
                    KeyModifiers::CONTROL | KeyModifiers::SUPER | KeyModifiers::META,
                ) =>
            {
                self.filter.push(character);
                self.filter_changed();
                PickerAction::Changed
            }
            KeyCode::Up if key.modifiers.is_empty() => self.move_focus(-1),
            KeyCode::Down if key.modifiers.is_empty() => self.move_focus(1),
            KeyCode::PageUp if key.modifiers.is_empty() => {
                self.move_focus(-isize::try_from(self.page_size).unwrap_or(isize::MAX))
            }
            KeyCode::PageDown if key.modifiers.is_empty() => {
                self.move_focus(isize::try_from(self.page_size).unwrap_or(isize::MAX))
            }
            KeyCode::Home if key.modifiers.is_empty() => self.focus_boundary(false),
            KeyCode::End if key.modifiers.is_empty() => self.focus_boundary(true),
            _ => PickerAction::None,
        }
    }

    pub(crate) fn desired_height(&self) -> u16 {
        u16::try_from(
            self.items
                .len()
                .saturating_add(self.group_heading_count())
                .max(1)
                .saturating_add(3),
        )
        .unwrap_or(u16::MAX)
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.render_in(frame, popup_area(area, self.desired_height()));
    }

    pub(crate) fn render_in(&mut self, frame: &mut Frame, popup: Rect) {
        if popup.is_empty() {
            return;
        }
        frame.render_widget(Clear, popup);
        let block = Block::bordered().title(self.title);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        if inner.is_empty() {
            return;
        }

        let result_rows = usize::from(inner.height.saturating_sub(1));
        let mut lines = vec![Line::raw(format!("Filter: {}", self.filter))];
        let matches = self.matches();
        if matches.is_empty() {
            if result_rows > 0 {
                lines.push(Line::raw(self.empty_message));
            }
        } else {
            let focus = self.focus;
            let focused_match = focus.and_then(|focus| {
                matches
                    .iter()
                    .position(|matched| matched.index == focus && matched.direct)
            });
            let mut start = focused_match.map_or_else(
                || self.scroll.min(matches.len().saturating_sub(1)),
                |focused_match| self.scroll.min(focused_match),
            );
            let mut visible = self.visible_matches(&matches, start, result_rows);
            if let Some(focused_match) = focused_match {
                while !visible.contains(&matches[focused_match].index) && start < focused_match {
                    start += 1;
                    visible = self.visible_matches(&matches, start, result_rows);
                }
            }
            self.scroll = start;
            self.page_size = visible
                .iter()
                .filter(|index| self.is_direct_match(**index))
                .count()
                .max(1);

            let mut last_group = None;
            for index in visible {
                let item = &self.items[index];
                if result_rows > 1 && item.group.is_some_and(|group| last_group != Some(group)) {
                    lines.push(Line::raw(item.group.expect("group was checked")));
                    last_group = item.group;
                }
                let mut row = item.row.clone();
                if focus == Some(index) {
                    row.style = row.style.add_modifier(Modifier::REVERSED);
                }
                lines.push(row);
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn initial_focus(items: &[PickerRow<T>], current: T) -> Option<usize> {
        items
            .iter()
            .position(|item| item.is_selectable() && item.value == current)
            .or_else(|| items.iter().position(PickerRow::is_selectable))
    }

    fn group_heading_count(&self) -> usize {
        let mut last_group = None;
        self.items
            .iter()
            .filter(|item| {
                let heading = item.group.is_some_and(|group| last_group != Some(group));
                if item.group.is_some() {
                    last_group = item.group;
                }
                heading
            })
            .count()
    }

    fn visible_matches(
        &self,
        matches: &[PickerMatch],
        start: usize,
        result_rows: usize,
    ) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut used_rows = 0_usize;
        let mut last_group = None;
        for matched in &matches[start..] {
            let item = &self.items[matched.index];
            let heading_rows = usize::from(
                result_rows > 1 && item.group.is_some_and(|group| last_group != Some(group)),
            );
            if used_rows.saturating_add(heading_rows + 1) > result_rows {
                break;
            }
            used_rows += heading_rows + 1;
            if item.group.is_some() {
                last_group = item.group;
            }
            visible.push(matched.index);
        }
        visible
    }

    fn apply(&self, save: bool) -> PickerAction<T> {
        let Some(index) = self.focus.filter(|index| self.is_direct_match(*index)) else {
            return PickerAction::NoMatch;
        };
        if save {
            PickerAction::Save(self.items[index].value)
        } else {
            PickerAction::Apply(self.items[index].value)
        }
    }

    fn move_focus(&mut self, offset: isize) -> PickerAction<T> {
        let matches = self.focusable_matches();
        let Some(current) = self
            .focus
            .and_then(|focus| matches.iter().position(|item| *item == focus))
        else {
            return PickerAction::None;
        };
        let target = current
            .saturating_add_signed(offset)
            .min(matches.len().saturating_sub(1));
        if target == current {
            PickerAction::None
        } else {
            self.focus = Some(matches[target]);
            PickerAction::Changed
        }
    }

    fn focus_boundary(&mut self, end: bool) -> PickerAction<T> {
        let matches = self.focusable_matches();
        let Some(target) = matches.get(if end {
            matches.len().saturating_sub(1)
        } else {
            0
        }) else {
            return PickerAction::None;
        };
        if self.focus == Some(*target) {
            PickerAction::None
        } else {
            self.focus = Some(*target);
            PickerAction::Changed
        }
    }

    fn filter_changed(&mut self) {
        self.canonical_filter = canonical_search(&self.filter);
        self.ensure_focus();
    }

    fn ensure_focus(&mut self) {
        let matches = self.focusable_matches();
        if self.focus.is_none_or(|focus| !matches.contains(&focus)) {
            self.focus = matches.first().copied();
        }
    }

    fn focusable_matches(&self) -> Vec<usize> {
        self.matches()
            .into_iter()
            .filter_map(|matched| matched.direct.then_some(matched.index))
            .collect()
    }

    fn is_direct_match(&self, index: usize) -> bool {
        self.item_match(&self.items[index]) == Some(true)
    }

    fn matches(&self) -> Vec<PickerMatch> {
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                self.item_match(item)
                    .map(|direct| PickerMatch { index, direct })
            })
            .collect()
    }

    fn item_match(&self, item: &PickerRow<T>) -> Option<bool> {
        match &item.role {
            PickerRowRole::SearchableLabel(canonical_text) => {
                self.matches_canonical_text(canonical_text).then_some(true)
            }
            PickerRowRole::Dynamic(DynamicPickerRole::Direct) => Some(true),
            PickerRowRole::Dynamic(DynamicPickerRole::Context) => Some(false),
        }
    }
}

impl<T> PickerRow<T> {
    fn is_selectable(&self) -> bool {
        !matches!(
            self.role,
            PickerRowRole::Dynamic(DynamicPickerRole::Context)
        )
    }
}

impl<T> From<PickerItem<T>> for PickerRow<T> {
    fn from(item: PickerItem<T>) -> Self {
        Self {
            group: Some(item.group),
            row: Line::raw(item.label),
            role: PickerRowRole::SearchableLabel(canonical_search(item.label)),
            value: item.value,
        }
    }
}

impl<T> From<DynamicPickerItem<T>> for PickerRow<T> {
    fn from(item: DynamicPickerItem<T>) -> Self {
        Self {
            group: None,
            row: item.row,
            role: PickerRowRole::Dynamic(item.role),
            value: item.value,
        }
    }
}

pub(super) fn canonical_search(text: &str) -> String {
    text.to_lowercase()
}

fn popup_area(area: Rect, desired_height: u16) -> Rect {
    let width = area.width.min(PICKER_MAX_WIDTH);
    let height = area.height.min(desired_height.max(1));
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    use super::{
        DynamicPickerItem, DynamicPickerRole, Picker, PickerAction, PickerItem, PickerSavePolicy,
    };

    const ITEMS: [PickerItem<&str>; 4] = [
        PickerItem {
            group: "OpenAI",
            label: "openai/gpt-5.6-sol",
            value: "sol",
        },
        PickerItem {
            group: "OpenAI",
            label: "openai/gpt-5.6-terra",
            value: "terra",
        },
        PickerItem {
            group: "Anthropic",
            label: "anthropic/claude",
            value: "claude",
        },
        PickerItem {
            group: "Anthropic",
            label: "anthropic/haiku",
            value: "haiku",
        },
    ];

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn picker(current: &str) -> Picker<&str> {
        Picker::new("Models", "No matching models", ITEMS, current)
    }

    fn dynamic_item(
        value: &'static str,
        label: &'static str,
        role: DynamicPickerRole,
    ) -> DynamicPickerItem<&'static str> {
        DynamicPickerItem {
            row: Line::raw(label.to_owned()),
            role,
            value,
        }
    }

    #[test]
    fn groups_items_and_focuses_the_current_or_first_match() {
        let mut current = picker("terra");
        assert_eq!(current.apply(false), PickerAction::Apply("terra"));
        assert!(rendered(&mut current, 60, 10).contains("OpenAI"));

        let mut missing = picker("missing");
        assert_eq!(missing.apply(false), PickerAction::Apply("sol"));
        assert!(rendered(&mut missing, 60, 10).contains("OpenAI"));
        assert!(rendered(&mut missing, 60, 10).contains("Anthropic"));
    }

    #[test]
    fn filters_case_insensitively_and_backspace_restores_results() {
        let mut filtered = picker("sol");
        for character in "TERRA".chars() {
            assert_eq!(
                filtered.handle_key(key(KeyCode::Char(character))),
                PickerAction::Changed
            );
        }
        assert_eq!(filtered.apply(false), PickerAction::Apply("terra"));
        for _ in 0..5 {
            assert_eq!(
                filtered.handle_key(key(KeyCode::Backspace)),
                PickerAction::Changed
            );
        }
        assert_eq!(filtered.apply(false), PickerAction::Apply("terra"));

        let mut non_contiguous = picker("sol");
        for character in "haiku".chars() {
            non_contiguous.handle_key(key(KeyCode::Char(character)));
        }
        assert!(rendered(&mut non_contiguous, 20, 5).contains("anthropic/haiku"));
    }

    #[test]
    fn dynamic_roles_keep_non_focusable_context_rows() {
        let normal = Style::new().fg(Color::White);
        let accent = Style::new().fg(Color::Cyan);
        let muted = Style::new().fg(Color::DarkGray);
        let mut picker = Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            [
                DynamicPickerItem {
                    row: Line::from(vec![
                        Span::styled("├─ ".to_owned(), muted),
                        Span::styled("Session γ".to_owned(), normal),
                        Span::styled(" α".to_owned(), accent),
                    ]),
                    role: DynamicPickerRole::Context,
                    value: "start",
                },
                DynamicPickerItem {
                    row: Line::from(vec![
                        Span::styled("└─ ".to_owned(), muted),
                        Span::styled("Python needle".to_owned(), normal),
                    ]),
                    role: DynamicPickerRole::Direct,
                    value: "step",
                },
                DynamicPickerItem {
                    row: Line::from(vec![
                        Span::styled("└─ ".to_owned(), muted),
                        Span::styled("Reply".to_owned(), normal),
                    ]),
                    role: DynamicPickerRole::Context,
                    value: "tail",
                },
            ],
            "start",
            PickerSavePolicy::Disabled,
        );

        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Apply("step")
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            PickerAction::None
        );
        let buffer = rendered_buffer(&mut picker, 60, 10);
        let (muted_column, muted_row) = cell_with_symbol(&buffer, "├");
        let (normal_column, normal_row) = cell_with_symbol(&buffer, "γ");
        let (accent_column, accent_row) = cell_with_symbol(&buffer, "α");
        let (focused_column, focused_row) = cell_with_symbol(&buffer, "P");
        let (next_row_column, next_row) = cell_with_symbol(&buffer, "R");
        assert_eq!(buffer[(muted_column, muted_row)].fg, Color::DarkGray);
        assert_eq!(buffer[(normal_column, normal_row)].fg, Color::White);
        assert_eq!(buffer[(accent_column, accent_row)].fg, Color::Cyan);
        assert!(
            buffer[(focused_column, focused_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(buffer[(next_row_column, next_row)].fg, Color::White);
        assert!(
            !buffer[(next_row_column, next_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn context_rows_remain_visible_without_focus() {
        let mut picker = Picker::new_dynamic(
            "Picker",
            "No matching items",
            [
                DynamicPickerItem {
                    row: Line::raw("First"),
                    role: DynamicPickerRole::Direct,
                    value: "first",
                },
                DynamicPickerItem {
                    row: Line::raw("Loading…"),
                    role: DynamicPickerRole::Context,
                    value: "loading",
                },
                DynamicPickerItem {
                    row: Line::raw("Last"),
                    role: DynamicPickerRole::Direct,
                    value: "last",
                },
            ],
            "loading",
            PickerSavePolicy::Enabled,
        );

        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Apply("first")
        );
        let initial = rendered_buffer(&mut picker, 60, 10);
        let (loading_column, loading_row) = cell_start(&initial, "Loading…");
        assert!(
            !initial[(loading_column, loading_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );

        for code in [KeyCode::Down, KeyCode::PageDown, KeyCode::End] {
            let _ = picker.handle_key(key(code));
            assert_eq!(
                picker.handle_key(key(KeyCode::Enter)),
                PickerAction::Apply("last")
            );
            assert_eq!(
                picker.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                PickerAction::Save("last")
            );
        }
        for code in [KeyCode::Up, KeyCode::PageUp, KeyCode::Home] {
            let _ = picker.handle_key(key(code));
            assert_eq!(
                picker.handle_key(key(KeyCode::Enter)),
                PickerAction::Apply("first")
            );
            assert_eq!(
                picker.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
                PickerAction::Save("first")
            );
        }

        let mut loading = Picker::new_dynamic(
            "Picker",
            "No matching items",
            [DynamicPickerItem {
                row: Line::raw("Loading…"),
                role: DynamicPickerRole::Context,
                value: "loading",
            }],
            "loading",
            PickerSavePolicy::Enabled,
        );
        assert_eq!(
            loading.handle_key(key(KeyCode::Enter)),
            PickerAction::NoMatch
        );
        assert_eq!(
            loading.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            PickerAction::NoMatch
        );
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert_eq!(loading.handle_key(key(code)), PickerAction::None);
        }
        let buffer = rendered_buffer(&mut loading, 60, 10);
        let (loading_column, loading_row) = cell_start(&buffer, "Loading…");
        assert!(
            !buffer[(loading_column, loading_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn dynamic_navigation_skips_context_rows_and_keeps_focus_visible() {
        let mut picker = Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            [
                dynamic_item("root", "Session start", DynamicPickerRole::Context),
                dynamic_item("one", "Python needle one", DynamicPickerRole::Direct),
                dynamic_item("branch", "Alternative branch", DynamicPickerRole::Context),
                dynamic_item("two", "Python needle two", DynamicPickerRole::Direct),
            ],
            "root",
            PickerSavePolicy::Disabled,
        );
        let _ = rendered(&mut picker, 60, 6);

        assert_eq!(picker.apply(false), PickerAction::Apply("one"));
        assert_eq!(
            picker.handle_key(key(KeyCode::PageDown)),
            PickerAction::Changed
        );
        assert_eq!(picker.apply(false), PickerAction::Apply("two"));
        assert_eq!(picker.handle_key(key(KeyCode::Home)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("one"));
        assert_eq!(picker.handle_key(key(KeyCode::End)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("two"));
        assert_eq!(
            picker.handle_key(key(KeyCode::PageUp)),
            PickerAction::Changed
        );
        assert_eq!(picker.apply(false), PickerAction::Apply("one"));
        assert_eq!(picker.handle_key(key(KeyCode::Down)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("two"));
        assert_eq!(picker.handle_key(key(KeyCode::Up)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("one"));

        let buffer = rendered_buffer(&mut picker, 60, 6);
        let (column, row) = cell_start(&buffer, "Python needle one");
        assert!(buffer[(column, row)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn context_rows_remain_nonfocusable_after_replacement() {
        let mut picker = Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            [
                dynamic_item("root", "Session start", DynamicPickerRole::Context),
                dynamic_item("step", "Python needle", DynamicPickerRole::Direct),
                dynamic_item("other", "Python other", DynamicPickerRole::Context),
            ],
            "step",
            PickerSavePolicy::Disabled,
        );
        assert_eq!(picker.apply(false), PickerAction::Apply("step"));

        picker.replace_items([dynamic_item(
            "root",
            "Session start",
            DynamicPickerRole::Context,
        )]);
        assert_eq!(picker.apply(false), PickerAction::NoMatch);
        assert!(rendered(&mut picker, 60, 6).contains("Session start"));
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Home,
            KeyCode::End,
        ] {
            assert_eq!(picker.handle_key(key(code)), PickerAction::None);
        }
        for (width, height) in [(1, 1), (1, 2), (2, 3), (3, 3)] {
            let _ = rendered(&mut picker, width, height);
        }
    }

    #[test]
    fn replacing_dynamic_items_preserves_direct_focus_by_value() {
        let mut picker = Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            [
                dynamic_item("root", "Session start", DynamicPickerRole::Context),
                dynamic_item(
                    "current",
                    "Python needle current",
                    DynamicPickerRole::Direct,
                ),
            ],
            "current",
            PickerSavePolicy::Disabled,
        );
        picker.replace_items([
            dynamic_item("root", "Session start", DynamicPickerRole::Context),
            dynamic_item(
                "current",
                "Python needle current",
                DynamicPickerRole::Direct,
            ),
            dynamic_item("later", "Python needle later", DynamicPickerRole::Direct),
        ]);
        assert_eq!(picker.apply(false), PickerAction::Apply("current"));

        picker.replace_items([
            dynamic_item("root", "Session start", DynamicPickerRole::Context),
            dynamic_item("current", "Python renamed", DynamicPickerRole::Context),
            dynamic_item("later", "Python needle later", DynamicPickerRole::Direct),
        ]);
        assert_eq!(picker.apply(false), PickerAction::Apply("later"));
    }

    #[test]
    fn navigates_results_by_rows_pages_and_boundaries() {
        let mut picker = picker("sol");
        let _ = rendered(&mut picker, 60, 7);
        assert_eq!(picker.handle_key(key(KeyCode::Down)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("terra"));
        assert_eq!(
            picker.handle_key(key(KeyCode::PageDown)),
            PickerAction::Changed
        );
        assert_eq!(picker.apply(false), PickerAction::Apply("haiku"));
        assert_eq!(
            picker.handle_key(key(KeyCode::PageUp)),
            PickerAction::Changed
        );
        assert_eq!(picker.apply(false), PickerAction::Apply("terra"));
        assert_eq!(picker.handle_key(key(KeyCode::End)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("haiku"));
        assert_eq!(picker.handle_key(key(KeyCode::Home)), PickerAction::Changed);
        assert_eq!(picker.apply(false), PickerAction::Apply("sol"));
        assert_eq!(picker.handle_key(key(KeyCode::Up)), PickerAction::None);
    }

    #[test]
    fn small_grouped_viewport_keeps_focus_visible_and_pages_by_visible_items() {
        let mut current = picker("sol");
        let _ = rendered(&mut current, 60, 7);
        assert_eq!(
            current.handle_key(key(KeyCode::Down)),
            PickerAction::Changed
        );
        assert_eq!(
            current.handle_key(key(KeyCode::Down)),
            PickerAction::Changed
        );

        let before = rendered_buffer(&mut current, 60, 7);
        let (column, claude_row) = cell_start(&before, "anthropic/claude");
        assert!(
            before[(column, claude_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );

        assert_eq!(current.handle_key(key(KeyCode::Up)), PickerAction::Changed);
        let after = rendered_buffer(&mut current, 60, 7);
        let (column, terra_row) = cell_start(&after, "openai/gpt-5.6-terra");
        assert!(
            after[(column, terra_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(terra_row < claude_row);

        let mut page_picker = picker("sol");
        let _ = rendered(&mut page_picker, 60, 7);
        assert_eq!(
            page_picker.handle_key(key(KeyCode::PageDown)),
            PickerAction::Changed
        );
        assert_eq!(page_picker.apply(false), PickerAction::Apply("claude"));
        assert_eq!(
            page_picker.handle_key(key(KeyCode::PageUp)),
            PickerAction::Changed
        );
        assert_eq!(page_picker.apply(false), PickerAction::Apply("sol"));
    }

    #[test]
    fn reports_empty_results_and_returns_apply_save_or_close_actions() {
        let mut filtered = picker("sol");
        assert_eq!(
            filtered.handle_key(key(KeyCode::Char('z'))),
            PickerAction::Changed
        );
        assert_eq!(filtered.apply(false), PickerAction::NoMatch);
        assert_eq!(filtered.apply(true), PickerAction::NoMatch);
        assert_eq!(filtered.focus, None);
        assert!(rendered(&mut filtered, 20, 5).contains("No matching models"));

        let mut picker = picker("sol");
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            PickerAction::Apply("sol")
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            PickerAction::Save("sol")
        );
        assert_eq!(picker.handle_key(key(KeyCode::Esc)), PickerAction::Close);
    }

    fn rendered(picker: &mut Picker<&str>, width: u16, height: u16) -> String {
        let buffer = rendered_buffer(picker, width, height);
        buffer
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_buffer(picker: &mut Picker<&str>, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn cell_with_symbol(buffer: &Buffer, symbol: &str) -> (u16, u16) {
        for row in buffer.area.top()..buffer.area.bottom() {
            for column in buffer.area.left()..buffer.area.right() {
                if buffer[(column, row)].symbol() == symbol {
                    return (column, row);
                }
            }
        }
        panic!("rendered symbol: {symbol}")
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
}
