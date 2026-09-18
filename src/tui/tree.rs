use std::collections::BTreeMap;

use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::session::{SessionCatalogue, SessionCatalogueKind, SessionEntryId};
use crate::transcript::{muted_page_style, normalise_terminal_text, page_style, truncate_end};

use super::picker::{
    DynamicPickerItem, DynamicPickerRole, PICKER_MAX_WIDTH, Picker, PickerSavePolicy,
    canonical_search,
};

const PICKER_ROW_WIDTH: usize = PICKER_MAX_WIDTH as usize - 2;
const MAX_ANCESTOR_CONNECTORS: usize = 6;

struct TreeNode<T> {
    parent: Option<usize>,
    children: Vec<usize>,
    value: T,
    search_text: String,
    canonical_search_text: String,
    depth: usize,
    branch_head: Option<usize>,
    branch_heads: [Option<usize>; MAX_ANCESTOR_CONNECTORS],
    active_path: bool,
}

struct TreeProjection {
    direct_matches: Vec<bool>,
    visible: Vec<bool>,
    last_visible: Vec<bool>,
}

impl TreeProjection {
    fn all<T>(nodes: &[TreeNode<T>]) -> Self {
        Self::new(nodes, vec![true; nodes.len()], vec![true; nodes.len()])
    }

    fn new<T>(nodes: &[TreeNode<T>], direct_matches: Vec<bool>, visible: Vec<bool>) -> Self {
        let mut last_visible = vec![false; nodes.len()];
        for node in nodes {
            if let Some(&child) = node.children.iter().rev().find(|&&child| visible[child]) {
                last_visible[child] = true;
            }
        }
        Self {
            direct_matches,
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
                let canonical_search_text = canonical_search(&search_text);
                TreeNode {
                    parent: entry.parent_id.map(|parent| {
                        *entry_indexes
                            .get(&parent)
                            .expect("session catalogue parent exists")
                    }),
                    children: Vec::new(),
                    value: value_for_entry(entry.entry_id),
                    search_text,
                    canonical_search_text,
                    depth: 0,
                    branch_head: None,
                    branch_heads: [None; MAX_ANCESTOR_CONNECTORS],
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

        let preorder = build_preorder(&mut nodes, root);
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
            self.items(&TreeProjection::all(&self.nodes), PICKER_ROW_WIDTH),
            self.active_value,
            PickerSavePolicy::Disabled,
        )
    }

    pub(crate) fn refresh(&self, picker: &mut Picker<T>) {
        self.refresh_for_width(picker, PICKER_ROW_WIDTH);
    }

    pub(crate) fn refresh_for_width(&self, picker: &mut Picker<T>, row_width: usize) {
        let projection = self.filtered_projection(picker);
        picker.replace_items(self.items(&projection, row_width));
    }

    fn filtered_projection(&self, picker: &Picker<T>) -> TreeProjection {
        let mut direct_matches = vec![false; self.nodes.len()];
        for &index in &self.preorder {
            direct_matches[index] =
                picker.matches_canonical_text(&self.nodes[index].canonical_search_text);
        }

        let mut visible = direct_matches.clone();
        for &index in self.preorder.iter().rev() {
            if visible[index]
                && let Some(parent) = self.nodes[index].parent
            {
                visible[parent] = true;
            }
        }
        TreeProjection::new(&self.nodes, direct_matches, visible)
    }

    fn items(&self, projection: &TreeProjection, row_width: usize) -> Vec<DynamicPickerItem<T>> {
        self.preorder
            .iter()
            .copied()
            .filter(|index| projection.visible[*index])
            .map(|index| {
                let node = &self.nodes[index];
                let direct = projection.direct_matches[index];
                DynamicPickerItem {
                    row: Line::styled(
                        truncate_end(&self.display_text(index, projection), row_width),
                        self.style(index),
                    ),
                    role: if direct {
                        DynamicPickerRole::Direct
                    } else {
                        DynamicPickerRole::Context
                    },
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
        let node = &self.nodes[index];
        if node.parent.is_none() {
            return String::new();
        }

        let parent = node.parent.expect("non-root tree node has a parent");
        let draws_joint =
            self.nodes[parent].parent.is_none() || self.nodes[parent].children.len() > 1;
        let ancestor_count = node.depth.saturating_sub(1);
        let mut connector = String::new();
        for ancestor in node.branch_heads.iter().take(ancestor_count).flatten() {
            connector.push_str(if projection.last_visible[*ancestor] {
                "   "
            } else {
                "│  "
            });
        }
        if ancestor_count > MAX_ANCESTOR_CONNECTORS {
            connector.push_str("… ");
        }
        if draws_joint {
            connector.push_str(if projection.last_visible[index] {
                "└─ "
            } else {
                "├─ "
            });
        } else {
            let branch_head = node
                .branch_head
                .expect("tree continuation has a branch head");
            connector.push_str(if projection.last_visible[branch_head] {
                "   "
            } else {
                "│  "
            });
        }
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

fn build_preorder<T>(nodes: &mut [TreeNode<T>], root: usize) -> Vec<usize> {
    let mut preorder = Vec::with_capacity(nodes.len());
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        preorder.push(index);
        let (depth, branch_head, branch_heads, is_root, branches) = {
            let node = &nodes[index];
            (
                node.depth,
                node.branch_head,
                node.branch_heads,
                node.parent.is_none(),
                node.children.len() > 1,
            )
        };
        let child_depth = if is_root {
            1
        } else {
            depth.saturating_add(usize::from(branches))
        };
        for child_index in (0..nodes[index].children.len()).rev() {
            let child = nodes[index].children[child_index];
            let child_branch_head = if is_root || branches {
                child
            } else {
                branch_head.expect("tree continuation has a branch head")
            };
            let mut child_branch_heads = branch_heads;
            if child_depth <= MAX_ANCESTOR_CONNECTORS {
                child_branch_heads[child_depth - 1] = Some(child_branch_head);
            }
            nodes[child].depth = child_depth;
            nodes[child].branch_head = Some(child_branch_head);
            nodes[child].branch_heads = child_branch_heads;
            stack.push(child);
        }
    }
    preorder
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
    fn renders_linear_history_at_first_level_without_false_joints() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                4,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("first request")),
                    entry(
                        3,
                        Some(2),
                        3,
                        SessionCatalogueKind::Execution {
                            commentary: Some("first execution".to_owned()),
                            tool_summaries: Vec::new(),
                        },
                    ),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Final {
                            text: "final reply".to_owned(),
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
                "└─ User: first request",
                "   Python: first execution",
                "   ● Kiki: final reply",
            ]
        );
    }

    #[test]
    fn renders_one_fork_with_linear_continuations_in_chronological_order() {
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
                "   Python: first step",
                "   ├─ User: first branch",
                "   │  Python",
                "   │  Kiki: first reply",
                "   └─ ● Python: [tools: 1] second step",
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
                "│  Python: first",
                "│  ├─ User: same revision lower id",
                "│  └─ User: same revision higher id",
                "└─ User: second",
                "   ● Python: second",
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
    fn caps_deep_nested_fork_connectors_without_hiding_labels_or_active_marker() {
        let tree = SessionTree::from_catalogue(&nested_fork_catalogue(32), |entry_id| entry_id);
        let mut picker = tree.picker();

        let wide = rendered(&mut picker, 72, 80);
        assert!(wide.contains("…"));
        assert!(wide.contains("User: depth 31"));
        assert!(wide.contains("└─ ● User: depth 32"));

        let narrow = rendered(&mut picker, 32, 8);
        let visible_rows = narrow
            .lines()
            .filter(|row| row.contains("User:"))
            .collect::<Vec<_>>();
        assert_eq!(visible_rows.len(), 5);
        assert!(visible_rows.iter().all(|row| row.contains("User:")));
        assert!(narrow.contains("● User:"));
    }

    #[test]
    fn deep_continuations_keep_capped_branch_alignment() {
        let mut catalogue = nested_fork_catalogue(8);
        catalogue
            .entries
            .push(entry(17, Some(16), 17, request("continued depth")));
        catalogue.active_entry = SessionEntryId(17);
        let tree = SessionTree::from_catalogue(&catalogue, |entry_id| entry_id);
        let mut picker = tree.picker();
        let rows = tree_rows(&mut picker);

        assert_eq!(
            &rows[rows.len() - 2..],
            [
                "                  … └─ User: depth 8",
                "                  …    ● User: continued depth",
            ]
        );
    }

    #[test]
    fn renders_nested_forks_with_linear_continuations() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                7,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("leading ancestor")),
                    entry(3, Some(2), 3, request("linear before fork")),
                    entry(4, Some(3), 4, request("outer first")),
                    entry(5, Some(4), 5, request("outer continuation")),
                    entry(6, Some(5), 6, request("inner first")),
                    entry(7, Some(6), 7, request("inner continuation")),
                    entry(8, Some(5), 8, request("inner second")),
                    entry(9, Some(3), 9, request("outer second")),
                    entry(10, Some(1), 10, request("leading sibling")),
                ],
            ),
            |entry_id| entry_id,
        );
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "├─ User: leading ancestor",
                "│  User: linear before fork",
                "│  ├─ User: outer first",
                "│  │  User: outer continuation",
                "│  │  ├─ User: inner first",
                "│  │  │  ● User: inner continuation",
                "│  │  └─ User: inner second",
                "│  └─ User: outer second",
                "└─ User: leading sibling",
            ]
        );
    }

    #[test]
    fn filters_a_4096_entry_chain_to_its_matching_leaf() {
        let tree = SessionTree::from_catalogue(&linear_catalogue(4_096), |entry_id| entry_id);
        assert_eq!(tree.nodes.last().expect("deepest node exists").depth, 1);
        let mut picker = tree.picker();

        picker.set_filter("depth 4096");
        tree.refresh(&mut picker);

        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(4_096))
        );
        assert!(rendered(&mut picker, 32, 8).contains("● User:"));
    }

    #[test]
    fn filtering_keeps_full_tree_fork_levels_and_recomputes_endings() {
        let tree = SessionTree::from_catalogue(
            &catalogue(
                5,
                [
                    entry(
                        5,
                        Some(3),
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

        picker.set_filter("USER: NEEDLE");
        tree.refresh(&mut picker);
        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "└─ User: branch",
                "   Python: context",
                "   └─ User: needle target",
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

    fn linear_catalogue(length: u64) -> SessionCatalogue {
        catalogue(
            length,
            (1..=length).map(|entry_id| {
                entry(
                    entry_id,
                    (entry_id > 1).then_some(entry_id - 1),
                    entry_id,
                    if entry_id == 1 {
                        SessionCatalogueKind::Start
                    } else {
                        SessionCatalogueKind::Request {
                            text: format!("depth {entry_id}"),
                        }
                    },
                )
            }),
        )
    }

    fn nested_fork_catalogue(depth: u64) -> SessionCatalogue {
        let mut entries = vec![entry(1, None, 1, SessionCatalogueKind::Start)];
        let mut parent = 1;
        let mut entry_id = 2;
        for level in 1..=depth {
            if level > 1 {
                entries.push(entry(
                    entry_id,
                    Some(parent),
                    entry_id,
                    request(&format!("sibling {level}")),
                ));
                entry_id += 1;
            }
            entries.push(entry(
                entry_id,
                Some(parent),
                entry_id,
                request(&format!("depth {level}")),
            ));
            parent = entry_id;
            entry_id += 1;
        }
        catalogue(parent, entries)
    }

    fn request(text: &str) -> SessionCatalogueKind {
        SessionCatalogueKind::Request {
            text: text.to_owned(),
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
                [
                    "Session start",
                    "User:",
                    "Python",
                    "Kiki:",
                    "Cancelled",
                    "Failed",
                ]
                .iter()
                .any(|label| row.contains(label))
            })
            .collect()
    }

    fn labels(picker: &mut Picker<SessionEntryId>) -> Vec<String> {
        tree_rows(picker)
            .into_iter()
            .map(|row| {
                let label = row
                    .rsplit_once("─ ")
                    .map_or(row.as_str(), |(_, label)| label)
                    .trim_start_matches([' ', '│', '…']);
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
