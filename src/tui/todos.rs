use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use unicode_segmentation::UnicodeSegmentation;

use super::normalise_single_line;
use super::picker::{DynamicPickerItem, DynamicPickerRole, Picker, PickerAction};
use crate::theme::Theme;
use crate::todos::{TodoDisplay, TodoPolicy, TodoSnapshot, TodoStatus};
use crate::transcript::normalise_terminal_text;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TodoOwnerId {
    Root,
    Child(u64),
}

pub(super) struct TodoOwner<'a> {
    pub(super) id: TodoOwnerId,
    pub(super) label: String,
    pub(super) status: Option<&'static str>,
    pub(super) policy: TodoPolicy,
    pub(super) items: Option<&'a TodoSnapshot>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TodoViewerAction {
    None,
    Changed,
    Close,
}

fn progress(items: &TodoSnapshot) -> String {
    let completed = items
        .iter()
        .filter(|item| item.status == TodoStatus::Completed)
        .count();
    format!("{completed}/{}", items.len())
}

fn marker(status: TodoStatus) -> &'static str {
    match status {
        TodoStatus::Completed => "✓",
        TodoStatus::InProgress => "◐",
        TodoStatus::Pending => "○",
    }
}

fn item_style(theme: Option<&Theme>, status: TodoStatus) -> Style {
    theme.map_or_else(Style::default, |theme| {
        Style::default().fg(match status {
            TodoStatus::Completed => theme.muted,
            TodoStatus::InProgress => theme.accent,
            TodoStatus::Pending => theme.text,
        })
    })
}

fn branch(last: bool) -> &'static str {
    if last { "└─" } else { "├─" }
}

fn clipped(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if Line::raw(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0_usize;
    for grapheme in text.graphemes(true) {
        let columns = Line::raw(grapheme).width();
        if used.saturating_add(columns) > width - 1 {
            break;
        }
        result.push_str(grapheme);
        used += columns;
    }
    result.push('…');
    result
}

pub(super) fn checklist_lines(
    theme: &Theme,
    items: &TodoSnapshot,
    policy: TodoPolicy,
    display: TodoDisplay,
    width: u16,
    max_rows: u16,
) -> Vec<Line<'static>> {
    if !policy.enabled() || display == TodoDisplay::None || width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let eligible = items
        .iter()
        .filter(|item| display == TodoDisplay::All || item.status == TodoStatus::InProgress)
        .collect::<Vec<_>>();
    if eligible.is_empty() {
        return Vec::new();
    }
    let available = usize::from(max_rows.saturating_sub(1));
    let overflow = eligible.len() > available;
    let visible = if overflow {
        available.saturating_sub(1)
    } else {
        eligible.len()
    };
    let suffix = if available == 0 { " …" } else { "" };
    let mut rows = vec![Line::styled(
        clipped(&format!("● Todos ({}){suffix}", progress(items)), width),
        Style::default().fg(theme.text),
    )];
    for (index, item) in eligible.iter().take(visible).enumerate() {
        let text = format!(
            "{} {} {}",
            branch(!overflow && index + 1 == visible),
            marker(item.status),
            normalise_single_line(&item.text)
        );
        rows.push(Line::from(Span::styled(
            clipped(&text, width),
            item_style(Some(theme), item.status),
        )));
    }
    if overflow && available > 0 {
        rows.push(Line::styled(
            clipped(
                &format!("└─ {} more · /todos", eligible.len() - visible),
                width,
            ),
            Style::default().fg(theme.muted),
        ));
    }
    rows
}

fn owner_rows(owners: &[TodoOwner<'_>]) -> Vec<DynamicPickerItem<TodoOwnerId>> {
    owners
        .iter()
        .map(|owner| {
            let detail = if !owner.policy.enabled() {
                "off".to_owned()
            } else {
                let count = owner
                    .items
                    .map_or_else(|| "No recorded list".to_owned(), progress);
                owner.status.map_or(count.clone(), |status| {
                    format!("{count} · {}", normalise_single_line(status))
                })
            };
            DynamicPickerItem {
                row: Line::raw(format!(
                    "{} · {detail}",
                    normalise_single_line(&owner.label)
                )),
                focused_row: None,
                role: if owner.policy.enabled() {
                    DynamicPickerRole::Searchable
                } else {
                    DynamicPickerRole::Context
                },
                value: owner.id,
            }
        })
        .collect()
}

fn full_lines(owner: Option<&TodoOwner<'_>>, theme: Option<&Theme>) -> Vec<Line<'static>> {
    let Some(owner) = owner else {
        return vec![Line::raw("Owner no longer available")];
    };
    if !owner.policy.enabled() {
        return vec![Line::raw("TODO list is off")];
    }
    let Some(items) = owner.items else {
        return vec![Line::raw("No recorded list")];
    };
    if items.is_empty() {
        return vec![Line::raw("No items")];
    }
    let mut lines = vec![Line::raw(format!("● Todos ({})", progress(items)))];
    for (index, item) in items.iter().enumerate() {
        let text = normalise_terminal_text(&item.text);
        for (part, text) in text.split('\n').enumerate() {
            let prefix = if part == 0 {
                format!(
                    "{} {} ",
                    branch(index + 1 == items.len()),
                    marker(item.status)
                )
            } else {
                "     ".to_owned()
            };
            lines.push(Line::styled(
                format!("{prefix}{text}"),
                item_style(theme, item.status),
            ));
        }
    }
    lines
}

pub(super) struct TodoViewer {
    picker: Picker<TodoOwnerId>,
    selected: Option<TodoOwnerId>,
    has_selector: bool,
    scroll: usize,
    width: u16,
    page_size: u16,
}

impl TodoViewer {
    pub(super) fn open(owners: &[TodoOwner<'_>]) -> Option<Self> {
        let first = owners.iter().find(|owner| owner.policy.enabled())?.id;
        let has_selector = owners
            .iter()
            .any(|owner| matches!(owner.id, TodoOwnerId::Child(_)));
        Some(Self {
            picker: Picker::new_dynamic(
                "TODO owners",
                "No matching owners",
                owner_rows(owners),
                first,
            ),
            selected: (!has_selector).then_some(first),
            has_selector,
            scroll: 0,
            width: 1,
            page_size: 1,
        })
    }

    fn owner<'a, 'b>(&self, owners: &'a [TodoOwner<'b>]) -> Option<&'a TodoOwner<'b>> {
        owners.iter().find(|owner| Some(owner.id) == self.selected)
    }

    fn max_scroll(&self, owners: &[TodoOwner<'_>]) -> usize {
        Paragraph::new(full_lines(self.owner(owners), None))
            .wrap(Wrap { trim: false })
            .line_count(self.width)
            .saturating_sub(usize::from(self.page_size))
            .min(usize::from(u16::MAX))
    }

    pub(super) fn handle_key(
        &mut self,
        key: KeyEvent,
        owners: &[TodoOwner<'_>],
    ) -> TodoViewerAction {
        self.picker.replace_items(owner_rows(owners));
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('s' | 'S'))
        {
            return TodoViewerAction::None;
        }
        if self.selected.is_none() {
            return match self.picker.handle_key(key) {
                PickerAction::Apply(id) => {
                    self.selected = Some(id);
                    self.scroll = 0;
                    TodoViewerAction::Changed
                }
                PickerAction::Close => TodoViewerAction::Close,
                PickerAction::Changed => TodoViewerAction::Changed,
                _ => TodoViewerAction::None,
            };
        }
        let max = self.max_scroll(owners);
        self.scroll = self.scroll.min(max);
        if key.code == KeyCode::Esc {
            if !self.has_selector {
                return TodoViewerAction::Close;
            }
            self.selected = None;
            self.scroll = 0;
            return TodoViewerAction::Changed;
        }
        if !key.modifiers.is_empty() {
            return TodoViewerAction::None;
        }
        let previous = self.scroll;
        self.scroll = match key.code {
            KeyCode::Up => self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll.saturating_add(1).min(max),
            KeyCode::PageUp => self.scroll.saturating_sub(usize::from(self.page_size)),
            KeyCode::PageDown => self
                .scroll
                .saturating_add(usize::from(self.page_size))
                .min(max),
            KeyCode::Home => 0,
            KeyCode::End => max,
            _ => self.scroll,
        };
        if previous == self.scroll {
            TodoViewerAction::None
        } else {
            TodoViewerAction::Changed
        }
    }

    pub(super) fn render(
        &mut self,
        theme: &Theme,
        frame: &mut Frame,
        area: Rect,
        owners: &[TodoOwner<'_>],
    ) {
        if area.is_empty() {
            self.width = 0;
            self.page_size = 1;
            self.scroll = 0;
            return;
        }
        frame.render_widget(Clear, area);
        self.picker.replace_items(owner_rows(owners));
        if self.selected.is_none() {
            self.picker.render(theme, frame, area);
            return;
        }
        let label = self.owner(owners).map_or_else(
            || "Unavailable owner".to_owned(),
            |owner| normalise_single_line(&owner.label),
        );
        let block = Block::bordered()
            .title(format!("Todos · {label} · Esc back/close"))
            .style(Style::default().fg(theme.text));
        let inner = block.inner(area);
        self.width = inner.width;
        self.page_size = inner.height.max(1);
        self.scroll = self.scroll.min(self.max_scroll(owners));
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        let paragraph = Paragraph::new(full_lines(self.owner(owners), Some(theme)))
            .style(Style::default().fg(theme.text))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll as u16, 0));
        frame.render_widget(paragraph, inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{DARK, LIGHT};
    use crate::todos::TodoItem;
    use ratatui::{Terminal, backend::TestBackend};

    fn items() -> TodoSnapshot {
        [
            TodoStatus::Completed,
            TodoStatus::Pending,
            TodoStatus::InProgress,
            TodoStatus::InProgress,
        ]
        .into_iter()
        .enumerate()
        .map(|(i, status)| TodoItem {
            id: i as i64 + 1,
            text: format!("item {i}"),
            status,
        })
        .collect()
    }
    fn owner(id: TodoOwnerId, items: Option<&TodoSnapshot>) -> TodoOwner<'_> {
        TodoOwner {
            id,
            label: format!("{id:?}"),
            status: Some("running"),
            policy: TodoPolicy::ENABLED,
            items,
        }
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn lines(items: &TodoSnapshot, mode: TodoDisplay, width: u16, rows: u16) -> Vec<String> {
        checklist_lines(&DARK, items, TodoPolicy::ENABLED, mode, width, rows)
            .iter()
            .map(ToString::to_string)
            .collect()
    }
    fn rendered(
        viewer: &mut TodoViewer,
        owners: &[TodoOwner<'_>],
        theme: &Theme,
        width: u16,
        height: u16,
    ) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new("STALE SECRET"), frame.area());
                viewer.render(theme, frame, frame.area(), owners);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width).max(1))
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn all_preserves_status_order_counts_branches_and_theme_roles() {
        let list = items();
        let before = list.clone();
        for theme in [&DARK, &LIGHT] {
            let rows = checklist_lines(theme, &list, TodoPolicy::ENABLED, TodoDisplay::All, 80, 10);
            assert_eq!(
                rows.iter().map(ToString::to_string).collect::<Vec<_>>(),
                [
                    "● Todos (1/4)",
                    "├─ ✓ item 0",
                    "├─ ○ item 1",
                    "├─ ◐ item 2",
                    "└─ ◐ item 3"
                ]
            );
            let mut terminal = Terminal::new(TestBackend::new(80, 5)).unwrap();
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new(rows.clone()), frame.area()))
                .unwrap();
            for (symbol, color) in [("✓", theme.muted), ("◐", theme.accent), ("○", theme.text)]
            {
                let cell = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .find(|cell| cell.symbol() == symbol)
                    .unwrap();
                assert_eq!(cell.fg, color);
            }
            assert_eq!(rows[1].spans.last().unwrap().style.fg, Some(theme.muted));
            assert_eq!(rows[2].spans.last().unwrap().style.fg, Some(theme.text));
            assert_eq!(rows[3].spans.last().unwrap().style.fg, Some(theme.accent));
        }
        assert_eq!(list, before);
        let completed = vec![list[0].clone()];
        assert_eq!(lines(&completed, TodoDisplay::All, 80, 10).len(), 2);
    }
    #[test]
    fn current_has_no_fallback_and_keeps_multiple_explicit_items() {
        let mut list = items();
        assert_eq!(
            lines(&list, TodoDisplay::Current, 80, 10),
            ["● Todos (1/4)", "├─ ◐ item 2", "└─ ◐ item 3"]
        );
        list.retain(|item| item.status != TodoStatus::InProgress);
        assert!(lines(&list, TodoDisplay::Current, 80, 10).is_empty());
    }
    #[test]
    fn hidden_empty_and_zero_areas_have_no_header_or_badge() {
        let list = items();
        assert!(lines(&list, TodoDisplay::None, 80, 10).is_empty());
        assert!(
            checklist_lines(
                &DARK,
                &list,
                TodoPolicy::capture(false),
                TodoDisplay::All,
                80,
                10
            )
            .is_empty()
        );
        assert!(lines(&vec![], TodoDisplay::All, 80, 10).is_empty());
        assert!(lines(&list, TodoDisplay::All, 0, 10).is_empty());
        assert!(lines(&list, TodoDisplay::All, 80, 0).is_empty());
    }
    #[test]
    fn row_budget_overflow_and_unicode_clipping_are_honest() {
        let mut list = items();
        list[0].text = "界雪🦀e\u{301}\n\x1b[31m\x07 a long tail".to_owned();
        let before = list.clone();
        assert_eq!(lines(&list, TodoDisplay::All, 80, 1), ["● Todos (1/4) …"]);
        assert_eq!(
            lines(&list, TodoDisplay::All, 80, 3),
            [
                "● Todos (1/4)",
                "├─ ✓ 界雪🦀e\u{301} \\x1B[31m\\x07 a long tail",
                "└─ 3 more · /todos"
            ]
        );
        for width in 1..24 {
            let rows = checklist_lines(
                &DARK,
                &list,
                TodoPolicy::ENABLED,
                TodoDisplay::All,
                width,
                3,
            );
            assert_eq!(rows.len(), 3);
            assert!(rows.iter().all(|r| r.width() <= usize::from(width)));
            assert!(rows.iter().all(|r| !r.to_string().contains('\x1b')));
            if usize::from(width) < Line::raw("└─ 3 more · /todos").width() {
                assert!(rows.last().unwrap().to_string().ends_with('…'));
            }
        }
        assert_eq!(list, before);
    }
    #[test]
    fn root_direct_child_selector_stable_identity_and_search_refresh() {
        let list = items();
        let root = [owner(TodoOwnerId::Root, Some(&list))];
        let mut direct = TodoViewer::open(&root).unwrap();
        assert!(rendered(&mut direct, &root, &DARK, 60, 15).contains("✓ item 0"));
        assert_eq!(
            direct.handle_key(key(KeyCode::Esc), &root),
            TodoViewerAction::Close
        );
        let owners = [
            owner(TodoOwnerId::Root, Some(&list)),
            owner(TodoOwnerId::Child(77), Some(&list)),
            owner(TodoOwnerId::Child(3), Some(&list)),
        ];
        let mut viewer = TodoViewer::open(&owners).unwrap();
        viewer.handle_key(key(KeyCode::Down), &owners);
        assert_eq!(viewer.picker.focused_value(), Some(TodoOwnerId::Child(77)));
        let reordered = [
            owner(TodoOwnerId::Child(3), Some(&list)),
            owner(TodoOwnerId::Child(77), Some(&list)),
            owner(TodoOwnerId::Root, Some(&list)),
        ];
        rendered(&mut viewer, &reordered, &LIGHT, 60, 15);
        assert_eq!(viewer.picker.focused_value(), Some(TodoOwnerId::Child(77)));
        for c in "77".chars() {
            viewer.handle_key(key(KeyCode::Char(c)), &owners);
        }
        rendered(&mut viewer, &reordered, &DARK, 60, 15);
        assert_eq!(viewer.picker.filter(), "77");
        viewer.handle_key(key(KeyCode::Enter), &reordered);
        assert!(rendered(&mut viewer, &reordered, &DARK, 60, 15).contains("item 0"));
        assert_eq!(
            viewer.handle_key(key(KeyCode::Esc), &reordered),
            TodoViewerAction::Changed
        );
        assert_eq!(viewer.picker.filter(), "77");
        assert_eq!(
            viewer.handle_key(key(KeyCode::Esc), &reordered),
            TodoViewerAction::Close
        );
    }
    #[test]
    fn viewer_live_updates_disabled_missing_empty_and_unrecorded_are_private() {
        let list = items();
        let owners = [owner(TodoOwnerId::Root, Some(&list))];
        let mut viewer = TodoViewer::open(&owners).unwrap();
        let changed = vec![TodoItem {
            id: 99,
            text: "fresh\n**literal**\x1b[31m".into(),
            status: TodoStatus::Pending,
        }];
        let updated = [owner(TodoOwnerId::Root, Some(&changed))];
        let output = rendered(&mut viewer, &updated, &DARK, 60, 15);
        assert!(output.contains("fresh") && output.contains("**literal**\\x1B[31m"));
        assert!(!output.contains("item 0") && !output.contains("STALE SECRET"));
        let disabled = [TodoOwner {
            policy: TodoPolicy::capture(false),
            ..owner(TodoOwnerId::Root, Some(&list))
        }];
        assert!(TodoViewer::open(&disabled).is_none());
        let output = rendered(&mut viewer, &disabled, &DARK, 60, 15);
        assert!(output.contains("off") && !output.contains("item 0") && !output.contains("1/4"));
        assert!(!rendered(&mut viewer, &[], &DARK, 60, 15).contains("fresh"));
        let empty = vec![];
        assert!(
            rendered(
                &mut viewer,
                &[owner(TodoOwnerId::Root, Some(&empty))],
                &DARK,
                60,
                15
            )
            .contains("No items")
        );
        assert!(
            rendered(
                &mut viewer,
                &[owner(TodoOwnerId::Root, None)],
                &DARK,
                60,
                15
            )
            .contains("No recorded list")
        );
    }
    #[test]
    fn disabled_selector_rows_have_no_counts_and_cannot_be_selected_or_saved() {
        let list = items();
        let owners = [
            TodoOwner {
                policy: TodoPolicy::capture(false),
                ..owner(TodoOwnerId::Root, Some(&list))
            },
            owner(TodoOwnerId::Child(8), None),
        ];
        let mut viewer = TodoViewer::open(&owners).unwrap();
        let text = rendered(&mut viewer, &owners, &DARK, 60, 15);
        assert!(text.contains("Root · off"));
        assert!(!text.contains("1/4"));
        viewer.handle_key(key(KeyCode::Home), &owners);
        assert_eq!(viewer.picker.focused_value(), Some(TodoOwnerId::Child(8)));
        assert_eq!(
            viewer.handle_key(
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &owners
            ),
            TodoViewerAction::None
        );
    }
    #[test]
    fn full_text_wrap_scroll_boundaries_resize_and_back_close() {
        let list = vec![TodoItem {
            id: 1,
            text: (0..70)
                .map(|i| format!("line{i:02} 界 long word wrapping\n"))
                .collect(),
            status: TodoStatus::Pending,
        }];
        let before = list.clone();
        let owners = [owner(TodoOwnerId::Root, Some(&list))];
        let mut viewer = TodoViewer::open(&owners).unwrap();
        assert!(rendered(&mut viewer, &owners, &DARK, 24, 8).contains("line00"));
        for code in [KeyCode::Down, KeyCode::PageDown, KeyCode::End] {
            assert_eq!(
                viewer.handle_key(key(code), &owners),
                TodoViewerAction::Changed
            );
        }
        assert!(rendered(&mut viewer, &owners, &LIGHT, 24, 8).contains("line69"));
        for code in [KeyCode::Up, KeyCode::PageUp, KeyCode::Home] {
            viewer.handle_key(key(code), &owners);
        }
        assert!(rendered(&mut viewer, &owners, &DARK, 24, 8).contains("line00"));
        for (w, h) in [(1, 1), (2, 2), (5, 3), (0, 0), (80, 100)] {
            rendered(&mut viewer, &owners, &DARK, w, h);
        }
        viewer.handle_key(key(KeyCode::End), &owners);
        rendered(
            &mut viewer,
            &[owner(TodoOwnerId::Root, Some(&vec![]))],
            &DARK,
            24,
            8,
        );
        assert_eq!(viewer.scroll, 0);
        assert_eq!(list, before);
    }
}
