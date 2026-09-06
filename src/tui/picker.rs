use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

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

pub(crate) struct Picker<T> {
    title: &'static str,
    empty_message: &'static str,
    items: Vec<PickerItem<T>>,
    filter: String,
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
        let items = items.into_iter().collect::<Vec<_>>();
        let focus = items
            .iter()
            .position(|item| item.value == current)
            .or_else(|| (!items.is_empty()).then_some(0));
        Self {
            title,
            empty_message,
            items,
            filter: String::new(),
            focus,
            scroll: 0,
            page_size: 1,
        }
    }

    pub(crate) fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_owned();
        self.ensure_focus();
    }

    pub(crate) fn empty_message(&self) -> &'static str {
        self.empty_message
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PickerAction<T> {
        match key.code {
            KeyCode::Esc => PickerAction::Close,
            KeyCode::Enter if key.modifiers.is_empty() => self.apply(false),
            KeyCode::Char('s' | 'S') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.apply(true)
            }
            KeyCode::Backspace if key.modifiers.is_empty() => {
                if self.filter.pop().is_some() {
                    self.ensure_focus();
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
                self.ensure_focus();
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

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect) {
        if area.is_empty() {
            return;
        }
        let group_headings = self
            .items
            .iter()
            .enumerate()
            .filter(|(index, item)| *index == 0 || self.items[*index - 1].group != item.group)
            .count();
        let popup = popup_area(area, self.items.len().saturating_add(group_headings).max(1));
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
            let focus = self.focus.expect("non-empty picker results have a focus");
            let focused_match = matches
                .iter()
                .position(|index| *index == focus)
                .expect("picker focus must be a matching item");
            let mut start = self.scroll.min(focused_match);
            let mut visible = self.visible_matches(&matches, start, result_rows);
            while !visible.contains(&focus) && start < focused_match {
                start += 1;
                visible = self.visible_matches(&matches, start, result_rows);
            }
            self.scroll = start;
            self.page_size = visible.len().max(1);

            let mut last_group = None;
            for index in visible {
                let item = self.items[index];
                if result_rows > 1 && last_group != Some(item.group) {
                    lines.push(Line::raw(item.group));
                    last_group = Some(item.group);
                }
                let style = (index == focus).then(|| Style::new().add_modifier(Modifier::REVERSED));
                lines.push(Line::styled(item.label, style.unwrap_or_default()));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn visible_matches(&self, matches: &[usize], start: usize, result_rows: usize) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut used_rows = 0_usize;
        let mut last_group = None;
        for index in matches[start..].iter().copied() {
            let item = self.items[index];
            let heading_rows = usize::from(result_rows > 1 && last_group != Some(item.group));
            if used_rows.saturating_add(heading_rows + 1) > result_rows {
                break;
            }
            used_rows += heading_rows + 1;
            last_group = Some(item.group);
            visible.push(index);
        }
        visible
    }

    fn apply(&self, save: bool) -> PickerAction<T> {
        let Some(index) = self.focus else {
            return PickerAction::NoMatch;
        };
        if save {
            PickerAction::Save(self.items[index].value)
        } else {
            PickerAction::Apply(self.items[index].value)
        }
    }

    fn move_focus(&mut self, offset: isize) -> PickerAction<T> {
        let matches = self.matches();
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
        let matches = self.matches();
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

    fn ensure_focus(&mut self) {
        let matches = self.matches();
        if self.focus.is_none_or(|focus| !matches.contains(&focus)) {
            self.focus = matches.first().copied();
        }
    }

    fn matches(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                item.label.to_lowercase().contains(&filter).then_some(index)
            })
            .collect()
    }
}

fn popup_area(area: Rect, content_rows: usize) -> Rect {
    let width = area.width.min(72);
    let desired_height = u16::try_from(content_rows.saturating_add(3)).unwrap_or(u16::MAX);
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
    use ratatui::style::Modifier;

    use super::{Picker, PickerAction, PickerItem};

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
