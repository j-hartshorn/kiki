use std::collections::BTreeMap;

use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::session::{SessionCatalogue, SessionCatalogueKind, SessionEntryId};
use crate::transcript::{muted_page_style, normalise_terminal_text, page_style, truncate_end};

use super::picker::{
    DynamicPickerItem, PICKER_MAX_WIDTH, Picker, PickerSavePolicy, canonical_search,
};

const PICKER_ROW_WIDTH: usize = PICKER_MAX_WIDTH as usize - 2;

struct TreeNode<T> {
    parent: Option<usize>,
    children: Vec<usize>,
    value: T,
    search_text: String,
    direct_search: String,
    context_search: String,
    active_path: bool,
}

struct TreeProjection {
    visible: Vec<bool>,
    last_visible: Vec<bool>,
}

impl TreeProjection {
    fn new<T>(nodes: &[TreeNode<T>], visible_indexes: &[usize]) -> Self {
        let mut visible = vec![false; nodes.len()];
        let mut last_visible = vec![false; nodes.len()];
        let mut last_visible_child = vec![None; nodes.len()];
        for &index in visible_indexes {
            visible[index] = true;
            if let Some(parent) = nodes[index].parent {
                if let Some(previous) = last_visible_child[parent].replace(index) {
                    last_visible[previous] = false;
                }
                last_visible[index] = true;
            }
        }
        Self {
            visible,
            last_visible,
        }
    }
}

pub(crate) struct SessionTree<T> {
    nodes: Vec<TreeNode<T>>,
    preorder: Vec<usize>,
    active_index: usize,
    active_value: T,
}

impl<T: Copy + Eq> SessionTree<T> {
    pub(crate) fn from_catalogue(
        catalogue: &SessionCatalogue,
        mut value_for_entry: impl FnMut(SessionEntryId) -> T,
    ) -> Self {
        let entry_indexes = catalogue
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.entry_id, index))
            .collect::<BTreeMap<_, _>>();
        let mut nodes = catalogue
            .entries
            .iter()
            .map(|entry| {
                let search_text = row_text(&entry.kind);
                TreeNode {
                    parent: entry.parent_id.map(|parent| {
                        *entry_indexes
                            .get(&parent)
                            .expect("session catalogue parent exists")
                    }),
                    children: Vec::new(),
                    value: value_for_entry(entry.entry_id),
                    direct_search: canonical_search(&search_text),
                    search_text,
                    context_search: String::new(),
                    active_path: false,
                }
            })
            .collect::<Vec<_>>();
        for index in 0..nodes.len() {
            if let Some(parent) = nodes[index].parent {
                nodes[parent].children.push(index);
            }
        }
        for node in &mut nodes {
            node.children.sort_by_key(|child| {
                let entry = &catalogue.entries[*child];
                (entry.revision, entry.entry_id)
            });
        }

        let root = nodes
            .iter()
            .position(|node| node.parent.is_none())
            .expect("session catalogue root exists");
        let active = *entry_indexes
            .get(&catalogue.active_entry)
            .expect("session catalogue active entry exists");
        let active_value = nodes[active].value;
        let mut current = Some(active);
        while let Some(index) = current {
            nodes[index].active_path = true;
            current = nodes[index].parent;
        }

        let mut preorder = Vec::with_capacity(nodes.len());
        append_preorder(&nodes, root, &mut preorder);
        populate_context_search(&mut nodes, root);
        Self {
            nodes,
            preorder,
            active_index: active,
            active_value,
        }
    }

    pub(crate) fn picker(&self) -> Picker<T> {
        Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            self.items(&self.preorder),
            self.active_value,
            PickerSavePolicy::Disabled,
        )
    }

    pub(crate) fn refresh(&self, picker: &mut Picker<T>) {
        let matching_indexes = self.matching_indexes(picker);
        picker.replace_items(self.items(&matching_indexes));
    }

    fn matching_indexes(&self, picker: &Picker<T>) -> Vec<usize> {
        self.preorder
            .iter()
            .copied()
            .filter(|index| {
                let node = &self.nodes[*index];
                picker.matches_canonical_search(&node.direct_search, &node.context_search)
            })
            .collect()
    }

    fn items(&self, visible_indexes: &[usize]) -> Vec<DynamicPickerItem<T>> {
        let projection = TreeProjection::new(&self.nodes, visible_indexes);
        self.preorder
            .iter()
            .copied()
            .filter(|index| projection.visible[*index])
            .map(|index| {
                let node = &self.nodes[index];
                DynamicPickerItem {
                    row: Line::styled(
                        truncate_end(&self.display_text(index, &projection), PICKER_ROW_WIDTH),
                        self.style(index),
                    ),
                    direct_search: node.search_text.clone(),
                    context_search: node.context_search.clone(),
                    value: node.value,
                }
            })
            .collect()
    }

    fn display_text(&self, index: usize, projection: &TreeProjection) -> String {
        let node = &self.nodes[index];
        let mut text = self.connector(index, projection);
        if index == self.active_index {
            text.push_str("● ");
        }
        text.push_str(&node.search_text);
        text
    }

    fn connector(&self, index: usize, projection: &TreeProjection) -> String {
        let Some(parent) = self.nodes[index].parent else {
            return String::new();
        };
        let mut ancestors = Vec::new();
        let mut ancestor = Some(parent);
        while let Some(index) = ancestor {
            let node = &self.nodes[index];
            if node.parent.is_none() {
                break;
            }
            ancestors.push(index);
            ancestor = node.parent;
        }
        ancestors.reverse();

        let mut connector = String::new();
        for ancestor in ancestors {
            connector.push_str(if projection.last_visible[ancestor] {
                "   "
            } else {
                "│  "
            });
        }
        connector.push_str(if projection.last_visible[index] {
            "└─ "
        } else {
            "├─ "
        });
        connector
    }

    fn style(&self, index: usize) -> Style {
        let node = &self.nodes[index];
        if index == self.active_index {
            page_style()
        } else if node.active_path {
            page_style().fg(Color::Cyan)
        } else {
            muted_page_style()
        }
    }
}

fn append_preorder<T>(nodes: &[TreeNode<T>], index: usize, preorder: &mut Vec<usize>) {
    preorder.push(index);
    for child in &nodes[index].children {
        append_preorder(nodes, *child, preorder);
    }
}

fn populate_context_search<T>(nodes: &mut [TreeNode<T>], index: usize) {
    let children = nodes[index].children.clone();
    for &child in &children {
        populate_context_search(nodes, child);
    }

    let mut context = String::new();
    for child in children {
        if !context.is_empty() {
            context.push('\n');
        }
        context.push_str(&nodes[child].direct_search);
        if !nodes[child].context_search.is_empty() {
            context.push('\n');
            context.push_str(&nodes[child].context_search);
        }
    }
    nodes[index].context_search = context;
}

fn row_text(kind: &SessionCatalogueKind) -> String {
    match kind {
        SessionCatalogueKind::Start => "Session start".to_owned(),
        SessionCatalogueKind::Request { text } => preview("User", text),
        SessionCatalogueKind::Execution {
            commentary,
            tool_summaries,
        } => execution_row(commentary.as_deref(), tool_summaries),
        SessionCatalogueKind::Final { text } => preview("Kiki", text),
        SessionCatalogueKind::Cancelled => "Cancelled".to_owned(),
        SessionCatalogueKind::Failed => "Failed".to_owned(),
    }
}

fn preview(prefix: &str, text: &str) -> String {
    let text = normalise_single_line(text);
    if text.is_empty() {
        format!("{prefix}:")
    } else {
        format!("{prefix}: {text}")
    }
}

fn execution_row(
    commentary: Option<&str>,
    tool_summaries: &[crate::arena::ToolCallSummary],
) -> String {
    let commentary = commentary
        .map(normalise_single_line)
        .filter(|text| !text.is_empty());
    let failed = tool_summaries
        .iter()
        .filter(|summary| matches!(summary, crate::arena::ToolCallSummary::Failed))
        .count();
    let succeeded = tool_summaries.len().saturating_sub(failed);
    let prefix = match (tool_summaries.len(), failed) {
        (0, _) => "Python".to_owned(),
        (total, 0) => format!("Python: [tools: {total}]"),
        (_, failed) if succeeded == 0 => format!("Python: [tools: {failed}✗]"),
        (_, failed) => format!("Python: [tools: {succeeded}✓ {failed}✗]"),
    };
    match commentary {
        Some(commentary) if prefix == "Python" => format!("Python: {commentary}"),
        Some(commentary) => format!("{prefix} {commentary}"),
        None => prefix,
    }
}

fn normalise_single_line(text: &str) -> String {
    normalise_terminal_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Modifier};

    use super::SessionTree;
    use crate::arena::ToolCallSummary;
    use crate::session::{
        SessionCatalogue, SessionCatalogueEntry, SessionCatalogueKind, SessionEntryId,
    };
    use crate::tui::picker::{Picker, PickerAction};

    #[test]
    fn renders_chronological_depth_first_tree_with_active_path() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                7,
                [
                    entry(
                        6,
                        Some(5),
                        6,
                        SessionCatalogueKind::Final {
                            text: "first reply".to_owned(),
                        },
                    ),
                    entry(
                        7,
                        Some(3),
                        7,
                        SessionCatalogueKind::Execution {
                            commentary: Some("second step".to_owned()),
                            tool_summaries: vec![ToolCallSummary::Count(1)],
                        },
                    ),
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Request {
                            text: "first branch".to_owned(),
                        },
                    ),
                    entry(
                        5,
                        Some(4),
                        5,
                        SessionCatalogueKind::Execution {
                            commentary: None,
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request {
                            text: "original".to_owned(),
                        },
                    ),
                    entry(
                        3,
                        Some(2),
                        3,
                        SessionCatalogueKind::Execution {
                            commentary: Some("first step".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "└─ User: original",
                "   └─ Python: first step",
                "      ├─ User: first branch",
                "      │  └─ Python",
                "      │     └─ Kiki: first reply",
                "      └─ ● Python: [tools: 1] second step",
            ]
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(7))
        );

        let buffer = rendered_buffer(&mut picker, 72, 12);
        let (root_column, root_row) = cell_start(&buffer, "Session start");
        let (alternative_column, alternative_row) = cell_start(&buffer, "first branch");
        let (current_column, current_row) = cell_start(&buffer, "second step");
        assert_eq!(buffer[(root_column, root_row)].fg, Color::Cyan);
        assert_eq!(
            buffer[(alternative_column, alternative_row)].fg,
            Color::Rgb(128, 128, 128)
        );
        assert_eq!(
            buffer[(current_column, current_row)].fg,
            Color::Rgb(212, 212, 212)
        );
        assert!(
            buffer[(current_column, current_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn marks_active_root_by_node_index_when_values_coincide() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                1,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request {
                            text: "alternative".to_owned(),
                        },
                    ),
                ],
            ),
            |_| SessionEntryId(0),
        );
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            ["● Session start", "└─ User: alternative"]
        );

        let buffer = rendered_buffer(&mut picker, 72, 8);
        let (root_column, root_row) = cell_start(&buffer, "Session start");
        let (alternative_column, alternative_row) = cell_start(&buffer, "alternative");
        assert_eq!(
            buffer[(root_column, root_row)].fg,
            Color::Rgb(212, 212, 212)
        );
        assert!(
            buffer[(root_column, root_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(
            buffer[(alternative_column, alternative_row)].fg,
            Color::Rgb(128, 128, 128)
        );
        assert!(
            !buffer[(alternative_column, alternative_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn keeps_returned_branches_together_in_chronological_depth_first_order() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                5,
                [
                    entry(
                        9,
                        Some(3),
                        6,
                        SessionCatalogueKind::Request {
                            text: "same revision higher id".to_owned(),
                        },
                    ),
                    entry(
                        5,
                        Some(4),
                        5,
                        SessionCatalogueKind::Execution {
                            commentary: Some("second".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        4,
                        Some(1),
                        4,
                        SessionCatalogueKind::Request {
                            text: "second".to_owned(),
                        },
                    ),
                    entry(
                        8,
                        Some(3),
                        6,
                        SessionCatalogueKind::Request {
                            text: "same revision lower id".to_owned(),
                        },
                    ),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request {
                            text: "first".to_owned(),
                        },
                    ),
                    entry(
                        3,
                        Some(2),
                        3,
                        SessionCatalogueKind::Execution {
                            commentary: Some("first".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "├─ User: first",
                "│  └─ Python: first",
                "│     ├─ User: same revision lower id",
                "│     └─ User: same revision higher id",
                "└─ User: second",
                "   └─ ● Python: second",
            ]
        );
    }

    #[test]
    fn renders_complete_row_vocabulary_and_normalises_text() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                11,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request {
                            text: " request\n\ttext \u{001b}".to_owned(),
                        },
                    ),
                    entry(
                        3,
                        Some(1),
                        3,
                        SessionCatalogueKind::Execution {
                            commentary: Some(" \n \t ".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(
                        4,
                        Some(1),
                        4,
                        SessionCatalogueKind::Execution {
                            commentary: Some("# comment\n only".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(
                        5,
                        Some(1),
                        5,
                        SessionCatalogueKind::Execution {
                            commentary: None,
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(
                        6,
                        Some(1),
                        6,
                        SessionCatalogueKind::Execution {
                            commentary: Some("all passed".to_owned()),
                            tool_summaries: vec![
                                ToolCallSummary::Pending,
                                ToolCallSummary::Exit(0),
                                ToolCallSummary::Exit(7),
                                ToolCallSummary::Signal,
                                ToolCallSummary::Count(0),
                                ToolCallSummary::Created,
                                ToolCallSummary::Replaced,
                                ToolCallSummary::Applied,
                                ToolCallSummary::Subagent("00".to_owned()),
                            ],
                        },
                    ),
                    entry(
                        7,
                        Some(1),
                        7,
                        SessionCatalogueKind::Execution {
                            commentary: Some("mixed".to_owned()),
                            tool_summaries: vec![
                                ToolCallSummary::Exit(7),
                                ToolCallSummary::Signal,
                                ToolCallSummary::Failed,
                            ],
                        },
                    ),
                    entry(
                        8,
                        Some(1),
                        8,
                        SessionCatalogueKind::Execution {
                            commentary: None,
                            tool_summaries: vec![ToolCallSummary::Failed, ToolCallSummary::Failed],
                        },
                    ),
                    entry(
                        9,
                        Some(1),
                        9,
                        SessionCatalogueKind::Final {
                            text: "reply\r\n\ttext \u{001b}".to_owned(),
                        },
                    ),
                    entry(10, Some(1), 10, SessionCatalogueKind::Cancelled),
                    entry(11, Some(1), 11, SessionCatalogueKind::Failed),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();

        assert_eq!(
            labels(&mut picker),
            [
                "Session start",
                "User: request text \\x1B",
                "Python",
                "Python: # comment only",
                "Python",
                "Python: [tools: 9] all passed",
                "Python: [tools: 2✓ 1✗] mixed",
                "Python: [tools: 2✗]",
                "Kiki: reply text \\x1B",
                "Cancelled",
                "Failed",
            ]
        );
    }

    #[test]
    fn filtering_retains_context_recomputes_connectors_and_recovers_entries() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                5,
                [
                    entry(
                        5,
                        Some(1),
                        5,
                        SessionCatalogueKind::Request {
                            text: "need elsewhere".to_owned(),
                        },
                    ),
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Request {
                            text: "needle target".to_owned(),
                        },
                    ),
                    entry(
                        3,
                        Some(2),
                        3,
                        SessionCatalogueKind::Execution {
                            commentary: Some("context".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request {
                            text: "branch".to_owned(),
                        },
                    ),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();

        picker.set_filter("needle");
        tree.refresh(&mut picker);
        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "└─ User: branch",
                "   └─ Python: context",
                "      └─ User: needle target",
            ]
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(4))
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            PickerAction::None
        );

        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            PickerAction::Changed
        );
        tree.refresh(&mut picker);
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            PickerAction::Changed
        );
        tree.refresh(&mut picker);
        assert!(labels(&mut picker).contains(&"User: need elsewhere".to_owned()));

        for _ in 0..4 {
            assert_eq!(
                picker.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
                PickerAction::Changed
            );
            tree.refresh(&mut picker);
        }
        assert_eq!(
            labels(&mut picker),
            [
                "Session start",
                "User: branch",
                "Python: context",
                "User: needle target",
                "User: need elsewhere",
            ]
        );
    }

    #[test]
    fn finds_untruncated_previews_and_preserves_graphemes() {
        let request = "🦀e\u{0301}".repeat(25) + " needle";
        let tree = SessionTree::from_catalogue(
            &catalogue(
                1,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request { text: request },
                    ),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();
        let buffer = rendered_buffer(&mut picker, 72, 8);
        let (user_column, user_row) = cell_start(&buffer, "User:");
        assert_eq!(buffer[(user_column + 65, user_row)].symbol(), "e\u{301}");
        assert_eq!(buffer[(user_column + 66, user_row)].symbol(), "…");

        picker.set_filter("needle");
        tree.refresh(&mut picker);
        let buffer = rendered_buffer(&mut picker, 72, 8);
        let (_, user_row) = cell_start(&buffer, "User:");
        let row = (buffer.area.left()..buffer.area.right())
            .map(|column| buffer[(column, user_row)].symbol())
            .collect::<String>();
        assert!(!row.contains("needle"));
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(2))
        );
    }

    #[test]
    fn disables_saving_and_handles_empty_and_narrow_results() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                1,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        2,
                        Some(1),
                        2,
                        SessionCatalogueKind::Request {
                            text: "selectable".to_owned(),
                        },
                    ),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            ["● Session start", "└─ User: selectable"]
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(1))
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            PickerAction::None
        );

        picker.set_filter("absent");
        tree.refresh(&mut picker);
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::NoMatch
        );
        assert!(rendered(&mut picker, 72, 8).contains("No matching session entries"));
        for (width, height) in [(1, 1), (1, 2), (2, 3), (3, 3)] {
            let _ = rendered_buffer(&mut picker, width, height);
        }
    }

    fn catalogue(
        active_entry: u64,
        entries: impl IntoIterator<Item = SessionCatalogueEntry>,
    ) -> SessionCatalogue {
        SessionCatalogue {
            active_entry: SessionEntryId(active_entry),
            entries: entries.into_iter().collect(),
        }
    }

    fn entry(
        entry_id: u64,
        parent_id: Option<u64>,
        revision: u64,
        kind: SessionCatalogueKind,
    ) -> SessionCatalogueEntry {
        SessionCatalogueEntry {
            entry_id: SessionEntryId(entry_id),
            parent_id: parent_id.map(SessionEntryId),
            revision,
            kind,
        }
    }

    fn tree_rows(picker: &mut Picker<SessionEntryId>) -> Vec<String> {
        let buffer = rendered_buffer(picker, 72, 40);
        buffer
            .content
            .chunks(72)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .map(|row| {
                row.strip_prefix('│')
                    .and_then(|row| row.strip_suffix('│'))
                    .unwrap_or(&row)
                    .trim_end()
                    .to_owned()
            })
            .filter(|row| {
                let row = row.trim_start();
                row == "Session start"
                    || row == "● Session start"
                    || row.starts_with("├─ ")
                    || row.starts_with("└─ ")
                    || row.starts_with('│')
            })
            .collect()
    }

    fn labels(picker: &mut Picker<SessionEntryId>) -> Vec<String> {
        tree_rows(picker)
            .into_iter()
            .map(|row| {
                let label = row
                    .rsplit_once("─ ")
                    .map_or(row.as_str(), |(_, label)| label);
                label.strip_prefix("● ").unwrap_or(label).to_owned()
            })
            .collect()
    }

    fn rendered(picker: &mut Picker<SessionEntryId>, width: u16, height: u16) -> String {
        let buffer = rendered_buffer(picker, width, height);
        buffer
            .content
            .chunks(usize::from(width))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn rendered_buffer(picker: &mut Picker<SessionEntryId>, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .expect("render picker");
        terminal.backend().buffer().clone()
    }

    fn cell_start(buffer: &Buffer, text: &str) -> (u16, u16) {
        let symbols = text
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        for row in buffer.area.top()..buffer.area.bottom() {
            for column in buffer.area.left()..buffer.area.right() {
                if symbols.iter().enumerate().all(|(offset, symbol)| {
                    let offset = u16::try_from(offset).expect("test terminal offset");
                    column.saturating_add(offset) < buffer.area.right()
                        && buffer[(column + offset, row)].symbol() == symbol
                }) {
                    return (column, row);
                }
            }
        }
        panic!("rendered text: {text}")
    }
}
