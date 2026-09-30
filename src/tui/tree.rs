use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
use std::cell::Cell;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use crate::session::{CompactionOutcome, SessionCatalogue, SessionCatalogueKind, SessionEntryId};
use crate::transcript::{
    compaction_reason_label, format_token_count, normalise_terminal_text, observation_text,
    tool_call_label, truncate_front,
};

use super::picker::{
    DynamicPickerItem, DynamicPickerRole, PICKER_MAX_WIDTH, Picker, PickerAction, canonical_search,
};

#[cfg(test)]
const PICKER_ROW_WIDTH: usize = PICKER_MAX_WIDTH as usize - 2;
const INITIAL_ROW_WIDTH: usize = PICKER_MAX_WIDTH as usize - 2;
const LOADING_ENTRY: SessionEntryId = SessionEntryId(0);

const TEXT: Color = Color::Rgb(212, 212, 212);
const ACCENT: Color = Color::Rgb(138, 190, 183);
const SUCCESS: Color = Color::Rgb(181, 189, 104);
const ERROR: Color = Color::Rgb(204, 102, 102);
const WARNING: Color = Color::Rgb(255, 255, 0);
const COMPACTION: Color = Color::Rgb(118, 150, 86);
const MUTED: Color = Color::Rgb(128, 128, 128);
const DIM: Color = Color::Rgb(102, 102, 102);
const SELECTED_BACKGROUND: Color = Color::Rgb(58, 58, 74);
const BORDER: Color = Color::Rgb(95, 135, 255);
const HEADER_SEPARATOR: &str = " · ";
const HEADER_HINTS: [&str; 7] = [
    "Enter restore",
    "Esc clear/close",
    "↑/↓ move",
    "←/→ branch",
    "Ctrl-X copy",
    "Ctrl-U conversation",
    "PgUp/PgDn page",
];

struct TreeNode {
    entry: SessionEntryId,
    kind: SessionCatalogueKind,
    canonical_search_text: String,
    active_path: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TreeMode {
    Full,
    Conversation,
}

impl TreeMode {
    fn toggled(self) -> Self {
        match self {
            Self::Full => Self::Conversation,
            Self::Conversation => Self::Full,
        }
    }
}

struct TreeTopology {
    eligible: Vec<bool>,
    parents: Vec<Option<usize>>,
    children: Vec<Vec<usize>>,
}

impl TreeTopology {
    fn full(catalogue: &SessionCatalogue, entry_indexes: &BTreeMap<SessionEntryId, usize>) -> Self {
        let parents = catalogue
            .entries
            .iter()
            .map(|entry| {
                entry.parent_id.map(|parent| {
                    *entry_indexes
                        .get(&parent)
                        .expect("session catalogue parent exists")
                })
            })
            .collect::<Vec<_>>();
        let mut children = (0..parents.len()).map(|_| Vec::new()).collect::<Vec<_>>();
        for (index, parent) in parents.iter().enumerate() {
            if let Some(parent) = parent {
                children[*parent].push(index);
            }
        }
        Self {
            eligible: vec![true; parents.len()],
            parents,
            children,
        }
    }

    fn conversation(
        nodes: &[TreeNode],
        full: &Self,
        preorder: &[usize],
        active_index: usize,
    ) -> Self {
        let mut eligible = nodes
            .iter()
            .map(|node| !matches!(&node.kind, SessionCatalogueKind::Execution { .. }))
            .collect::<Vec<_>>();
        eligible[active_index] = true;
        let mut parents = vec![None; nodes.len()];
        let mut children = (0..nodes.len()).map(|_| Vec::new()).collect::<Vec<_>>();

        for &index in preorder {
            let parent = full.parents[index]
                .and_then(|parent| eligible[parent].then_some(parent).or(parents[parent]));
            parents[index] = parent;
            if eligible[index]
                && let Some(parent) = parent
            {
                children[parent].push(index);
            }
        }

        Self {
            eligible,
            parents,
            children,
        }
    }
}

struct TreeProjection<'a> {
    topology: &'a TreeTopology,
    direct_matches: Vec<bool>,
    visible: Vec<bool>,
    last_visible: Vec<bool>,
}

impl<'a> TreeProjection<'a> {
    #[cfg(test)]
    fn all(topology: &'a TreeTopology) -> Self {
        Self::new(
            topology,
            vec![true; topology.children.len()],
            vec![true; topology.children.len()],
        )
    }

    fn new(topology: &'a TreeTopology, direct_matches: Vec<bool>, visible: Vec<bool>) -> Self {
        let mut last_visible = vec![false; topology.children.len()];
        for children in &topology.children {
            if let Some(&child) = children.iter().rev().find(|&&child| visible[child]) {
                last_visible[child] = true;
            }
        }
        Self {
            topology,
            direct_matches,
            visible,
            last_visible,
        }
    }

    fn with_hidden_descendants(
        &self,
        preorder: &[usize],
        is_folded: impl Fn(usize) -> bool,
    ) -> Self {
        let mut visible = self.visible.clone();
        let mut hidden = vec![false; visible.len()];
        for &index in preorder {
            if let Some(parent) = self.topology.parents[index] {
                hidden[index] = hidden[parent] || (self.visible[parent] && is_folded(parent));
            }
            if hidden[index] {
                visible[index] = false;
            }
        }
        Self::new(self.topology, self.direct_matches.clone(), visible)
    }

    fn visible_children(&self, index: usize) -> impl Iterator<Item = usize> + '_ {
        self.topology.children[index]
            .iter()
            .copied()
            .filter(|child| self.visible[*child])
    }

    fn is_foldable(&self, index: usize) -> bool {
        self.visible[index]
            && self.visible_children(index).next().is_some()
            && match self.topology.parents[index] {
                None => true,
                Some(parent) => self.visible_children(parent).nth(1).is_some(),
            }
    }

    fn enclosing_segment_start(&self, selected: usize) -> usize {
        let mut current = selected;
        loop {
            let Some(parent) = self.topology.parents[current] else {
                return current;
            };
            if current != selected && self.visible_children(parent).nth(1).is_some() {
                return current;
            }
            current = parent;
        }
    }

    fn first_child_segment_start(&self, selected: usize) -> usize {
        let mut current = selected;
        loop {
            let mut children = self.visible_children(current);
            let Some(first) = children.next() else {
                return current;
            };
            if children.next().is_some() {
                return first;
            }
            current = first;
        }
    }
}

struct SessionTree {
    entry_indexes: BTreeMap<SessionEntryId, usize>,
    nodes: Vec<TreeNode>,
    full_topology: TreeTopology,
    conversation_topology: TreeTopology,
    preorder: Vec<usize>,
    active_index: usize,
    #[cfg(test)]
    row_item_constructions: Cell<usize>,
}

impl SessionTree {
    fn from_catalogue(catalogue: &SessionCatalogue) -> Self {
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
                    entry: entry.entry_id,
                    kind: entry.kind.clone(),
                    canonical_search_text,
                    active_path: false,
                }
            })
            .collect::<Vec<_>>();
        let mut full_topology = TreeTopology::full(catalogue, &entry_indexes);
        let latest_revisions = subtree_latest_revisions(&full_topology, catalogue);
        for children in &mut full_topology.children {
            children.sort_by(|left, right| {
                latest_revisions[*right]
                    .cmp(&latest_revisions[*left])
                    .then_with(|| {
                        catalogue.entries[*right]
                            .revision
                            .cmp(&catalogue.entries[*left].revision)
                    })
                    .then_with(|| {
                        catalogue.entries[*right]
                            .entry_id
                            .cmp(&catalogue.entries[*left].entry_id)
                    })
            });
        }

        let root = full_topology
            .parents
            .iter()
            .position(Option::is_none)
            .expect("session catalogue root exists");
        let active = *entry_indexes
            .get(&catalogue.active_entry)
            .expect("session catalogue active entry exists");
        let mut current = Some(active);
        while let Some(index) = current {
            nodes[index].active_path = true;
            current = full_topology.parents[index];
        }

        let preorder = build_preorder(&full_topology, root);
        let conversation_topology =
            TreeTopology::conversation(&nodes, &full_topology, &preorder, active);
        Self {
            entry_indexes,
            nodes,
            full_topology,
            conversation_topology,
            preorder,
            active_index: active,
            #[cfg(test)]
            row_item_constructions: Cell::new(0),
        }
    }

    #[cfg(test)]
    fn picker(&self) -> Picker<SessionEntryId> {
        self.picker_for_width(PICKER_ROW_WIDTH, TreeMode::Full, &BTreeSet::new())
    }

    fn picker_for_width(
        &self,
        row_width: usize,
        mode: TreeMode,
        folded_heads: &BTreeSet<SessionEntryId>,
    ) -> Picker<SessionEntryId> {
        let projection = self.filtered_projection(mode, "");
        Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            self.items(&projection, row_width, folded_heads),
            self.nodes[self.active_index].entry,
        )
    }

    #[cfg(test)]
    fn refresh(&self, picker: &mut Picker<SessionEntryId>) {
        let query = picker.filter().to_owned();
        self.refresh_for_query(
            picker,
            TreeMode::Full,
            &query,
            PICKER_ROW_WIDTH,
            &BTreeSet::new(),
        );
    }

    fn refresh_for_query(
        &self,
        picker: &mut Picker<SessionEntryId>,
        mode: TreeMode,
        query: &str,
        row_width: usize,
        folded_heads: &BTreeSet<SessionEntryId>,
    ) -> TreeProjection<'_> {
        let projection = self.filtered_projection(mode, query);
        picker.replace_items(self.items(&projection, row_width, folded_heads));
        projection
    }

    #[cfg(test)]
    fn row_item_constructions(&self) -> usize {
        self.row_item_constructions.get()
    }

    fn filtered_projection(&self, mode: TreeMode, query: &str) -> TreeProjection<'_> {
        let topology = match mode {
            TreeMode::Full => &self.full_topology,
            TreeMode::Conversation => &self.conversation_topology,
        };
        let tokens = query
            .split_whitespace()
            .map(canonical_search)
            .collect::<Vec<_>>();
        let mut direct_matches = vec![false; self.nodes.len()];
        for &index in &self.preorder {
            direct_matches[index] = topology.eligible[index]
                && tokens
                    .iter()
                    .all(|token| self.nodes[index].canonical_search_text.contains(token));
        }

        let mut visible = direct_matches.clone();
        for &index in self.preorder.iter().rev() {
            if visible[index]
                && let Some(parent) = topology.parents[index]
            {
                visible[parent] = true;
            }
        }
        TreeProjection::new(topology, direct_matches, visible)
    }

    fn items(
        &self,
        projection: &TreeProjection<'_>,
        row_width: usize,
        folded_heads: &BTreeSet<SessionEntryId>,
    ) -> Vec<DynamicPickerItem<SessionEntryId>> {
        enum ProjectionStep {
            Enter(usize),
            Exit(usize),
        }

        let displayed = projection.with_hidden_descendants(&self.preorder, |index| {
            folded_heads.contains(&self.nodes[index].entry)
        });
        let root = *self
            .preorder
            .first()
            .expect("session tree preorder has a root");
        if !displayed.visible[root] {
            return Vec::new();
        }
        let mut items = Vec::new();
        let mut traversal = vec![ProjectionStep::Enter(root)];
        let mut connectors = Vec::new();

        while let Some(step) = traversal.pop() {
            match step {
                ProjectionStep::Exit(length) => connectors.truncate(length),
                ProjectionStep::Enter(index) => {
                    let connector_length = connectors.len();
                    let (row, focused_row) = self.row(
                        index,
                        &displayed,
                        projection,
                        folded_heads,
                        &connectors,
                        row_width,
                    );
                    if self.draws_joint(index, displayed.topology) {
                        connectors.push(!displayed.last_visible[index]);
                    }
                    #[cfg(test)]
                    self.row_item_constructions
                        .set(self.row_item_constructions.get() + 1);
                    items.push(DynamicPickerItem {
                        row,
                        focused_row: Some(focused_row),
                        role: if projection.direct_matches[index] {
                            DynamicPickerRole::Direct
                        } else {
                            DynamicPickerRole::Context
                        },
                        value: self.nodes[index].entry,
                    });

                    traversal.push(ProjectionStep::Exit(connector_length));
                    for &child in displayed.topology.children[index].iter().rev() {
                        if displayed.visible[child] {
                            traversal.push(ProjectionStep::Enter(child));
                        }
                    }
                }
            }
        }

        items
    }

    fn row(
        &self,
        index: usize,
        projection: &TreeProjection<'_>,
        pre_fold: &TreeProjection<'_>,
        folded_heads: &BTreeSet<SessionEntryId>,
        connectors: &[bool],
        row_width: usize,
    ) -> (Line<'static>, Line<'static>) {
        let active = self.nodes[index].active_path;
        let connector_style = colour_style(if active { ACCENT } else { DIM });
        let content_style = colour_style(if active { TEXT } else { MUTED });
        let mut body = StyledRow::new(row_width.saturating_sub(2));

        for &continues in connectors {
            body.push(if continues { "│  " } else { "   " }, connector_style);
            if body.is_truncated() {
                return row_variants(body.into_spans(), row_width);
            }
        }
        let folded = folded_heads.contains(&self.nodes[index].entry);
        if self.draws_joint(index, projection.topology) {
            body.push(
                match (
                    projection.last_visible[index],
                    pre_fold.is_foldable(index),
                    folded,
                ) {
                    (true, _, true) => "└⊞ ",
                    (false, _, true) => "├⊞ ",
                    (true, true, false) => "└⊟ ",
                    (false, true, false) => "├⊟ ",
                    (true, false, false) => "└─ ",
                    (false, false, false) => "├─ ",
                },
                connector_style,
            );
        } else if projection.topology.parents[index].is_none() && folded {
            body.push("⊞ ", connector_style);
        }
        if !body.is_truncated() {
            if index == self.active_index {
                body.push("● ", colour_style(ACCENT));
            } else if active {
                body.push("• ", colour_style(ACCENT));
            }
        }
        if !body.is_truncated() {
            append_kind_row(&mut body, &self.nodes[index].kind, content_style);
        }
        row_variants(body.into_spans(), row_width)
    }

    fn draws_joint(&self, index: usize, topology: &TreeTopology) -> bool {
        let Some(parent) = topology.parents[index] else {
            return false;
        };
        topology.parents[parent].is_none() || topology.children[parent].len() > 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TreeNavigatorAction {
    None,
    Changed,
    Close,
    Copy(String),
    Navigate(SessionEntryId),
}

pub(crate) struct TreeNavigator {
    revision: u64,
    row_width: usize,
    mode: TreeMode,
    folded_heads: BTreeSet<SessionEntryId>,
    state: TreeNavigatorState,
}

enum TreeNavigatorState {
    Loading {
        picker: Picker<SessionEntryId>,
    },
    Ready {
        tree: Box<SessionTree>,
        picker: Picker<SessionEntryId>,
    },
}

impl TreeNavigator {
    pub(crate) fn new(revision: u64, query: &str) -> Self {
        let loading_row = row_variants(
            vec![Span::styled("Loading…", colour_style(TEXT))],
            INITIAL_ROW_WIDTH,
        )
        .0;
        let mut picker = Picker::new_dynamic(
            "Session tree",
            "No matching session entries",
            [DynamicPickerItem {
                row: loading_row,
                focused_row: None,
                role: DynamicPickerRole::Context,
                value: LOADING_ENTRY,
            }],
            LOADING_ENTRY,
        );
        picker.set_filter(query);
        Self {
            revision,
            row_width: INITIAL_ROW_WIDTH,
            mode: TreeMode::Full,
            folded_heads: BTreeSet::new(),
            state: TreeNavigatorState::Loading { picker },
        }
    }

    pub(crate) fn is_loading(&self) -> bool {
        matches!(self.state, TreeNavigatorState::Loading { .. })
    }

    pub(crate) fn install_catalogue(
        &mut self,
        revision: u64,
        catalogue: &SessionCatalogue,
    ) -> bool {
        if self.revision != revision {
            return false;
        }
        let TreeNavigatorState::Loading { picker } = &self.state else {
            return false;
        };
        let query = picker.filter().to_owned();
        let tree = SessionTree::from_catalogue(catalogue);
        let mut picker = tree.picker_for_width(self.row_width, self.mode, &self.folded_heads);
        picker.set_filter(&query);
        tree.refresh_for_query(
            &mut picker,
            self.mode,
            &query,
            self.row_width,
            &self.folded_heads,
        );
        self.state = TreeNavigatorState::Ready {
            tree: Box::new(tree),
            picker,
        };
        true
    }

    pub(crate) fn reject_catalogue(&self, revision: u64) -> bool {
        self.revision == revision && self.is_loading()
    }

    fn refresh_width(&mut self, row_width: usize) {
        if self.row_width == row_width {
            return;
        }
        self.row_width = row_width;
        let mode = self.mode;
        let TreeNavigatorState::Ready { tree, picker } = &mut self.state else {
            return;
        };
        let query = picker.filter().to_owned();
        tree.refresh_for_query(picker, mode, &query, row_width, &self.folded_heads);
    }

    pub(super) fn content_height(&self) -> u16 {
        self.picker().content_height()
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, dock: Rect, notice: Option<&str>) {
        if dock.width > 0 {
            self.refresh_width(usize::from(dock.width));
        }
        if dock.is_empty() {
            return;
        }
        frame.render_widget(Clear, dock);
        render_rule(frame, Rect::new(dock.x, dock.y, dock.width, 1));
        if dock.height == 1 {
            return;
        }
        frame.render_widget(
            Paragraph::new(self.header(dock.width, notice)),
            Rect::new(dock.x, dock.y.saturating_add(1), dock.width, 1),
        );
        if dock.height == 2 {
            return;
        }
        render_rule(
            frame,
            Rect::new(dock.x, dock.y.saturating_add(2), dock.width, 1),
        );
        if dock.height > 3 {
            self.picker_mut().render_frameless(
                frame,
                Rect::new(
                    dock.x,
                    dock.y.saturating_add(3),
                    dock.width,
                    dock.height.saturating_sub(3),
                ),
            );
        }
    }

    fn header(&self, width: u16, notice: Option<&str>) -> String {
        let notice = notice
            .map(normalise_single_line)
            .filter(|notice| !notice.is_empty());
        let query_text = (!self.picker().filter().is_empty())
            .then(|| normalise_header_line(self.picker().filter()));
        let query = query_text.as_ref().map(|query| format!("Search: {query}"));
        let position = (!self.is_loading()).then(|| {
            let (selected, total) = self.picker().position();
            format!("{selected}/{total}")
        });
        let mode = (self.mode == TreeMode::Conversation).then(|| "[conversation]".to_owned());

        let mut core = vec!["Session tree".to_owned()];
        if let Some(notice) = &notice {
            core.push(notice.clone());
        }
        if let Some(query) = &query {
            core.push(query.clone());
        }
        if let Some(position) = &position {
            core.push(position.clone());
        }
        if let Some(mode) = &mode {
            core.push(mode.clone());
        }

        let mut with_hints = core.clone();
        if notice.is_none() {
            for hint in HEADER_HINTS {
                let mut candidate = with_hints.clone();
                candidate.push(hint.to_owned());
                if display_width(&header_text(&candidate)) <= usize::from(width) {
                    with_hints = candidate;
                } else {
                    break;
                }
            }
        }
        let header = header_text(&with_hints);
        if display_width(&header) <= usize::from(width) {
            return header;
        }

        let mut without_position = vec!["Session tree".to_owned()];
        if let Some(notice) = &notice {
            without_position.push(notice.clone());
        }
        if let Some(query) = &query {
            without_position.push(query.clone());
        }
        if let Some(mode) = &mode {
            without_position.push(mode.clone());
        }
        let header = header_text(&without_position);
        if display_width(&header) <= usize::from(width) {
            return header;
        }

        if let Some(query_text) = query_text {
            let mut without_query = vec!["Session tree".to_owned()];
            if let Some(notice) = &notice {
                without_query.push(notice.clone());
            }
            if let Some(mode) = &mode {
                without_query.push(mode.clone());
            }
            let query_width = usize::from(width)
                .saturating_sub(display_width(&header_text(&without_query)))
                .saturating_sub(display_width(HEADER_SEPARATOR));
            let prefix_width = display_width("Search: ");
            if query_width > prefix_width {
                let mut truncated = vec!["Session tree".to_owned()];
                if let Some(notice) = &notice {
                    truncated.push(notice.clone());
                }
                truncated.push(format!(
                    "Search: {}",
                    truncate_front(&query_text, query_width.saturating_sub(prefix_width))
                ));
                if let Some(mode) = &mode {
                    truncated.push(mode.clone());
                }
                let header = header_text(&truncated);
                if display_width(&header) <= usize::from(width) {
                    return header;
                }
            }
            without_position = without_query;
        }
        clip_end(&header_text(&without_position), usize::from(width))
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> TreeNavigatorAction {
        if self.is_loading() {
            return if key.code == KeyCode::Esc {
                TreeNavigatorAction::Close
            } else {
                TreeNavigatorAction::None
            };
        }
        if matches!(key.code, KeyCode::Char('x' | 'X'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return self
                .copy_focused()
                .map_or(TreeNavigatorAction::None, TreeNavigatorAction::Copy);
        }
        if matches!(key.code, KeyCode::Char('s' | 'S'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return TreeNavigatorAction::None;
        }
        if matches!(key.code, KeyCode::Char('u' | 'U'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.mode = self.mode.toggled();
            self.folded_heads.clear();
            let mode = self.mode;
            let row_width = self.row_width;
            let (folded_heads, state) = (&self.folded_heads, &mut self.state);
            let TreeNavigatorState::Ready { tree, picker } = state else {
                unreachable!("ready tree navigator has ready state");
            };
            let focused = picker.focused_value();
            let query = picker.filter().to_owned();
            let projection = tree.refresh_for_query(picker, mode, &query, row_width, folded_heads);
            if let Some(focused) = focused {
                let focused = tree.entry_indexes[&focused];
                if !projection.topology.eligible[focused] {
                    let mut ancestor = projection.topology.parents[focused];
                    while let Some(index) = ancestor {
                        if projection.direct_matches[index] {
                            picker.focus_value(tree.nodes[index].entry);
                            break;
                        }
                        ancestor = projection.topology.parents[index];
                    }
                }
            }
            return TreeNavigatorAction::Changed;
        }
        if key.code == KeyCode::Esc {
            if self.picker().filter().is_empty() {
                return TreeNavigatorAction::Close;
            }
            self.folded_heads.clear();
            let mode = self.mode;
            let row_width = self.row_width;
            let (folded_heads, state) = (&self.folded_heads, &mut self.state);
            let TreeNavigatorState::Ready { tree, picker } = state else {
                unreachable!("ready tree navigator has ready state");
            };
            picker.set_filter("");
            tree.refresh_for_query(picker, mode, "", row_width, folded_heads);
            return TreeNavigatorAction::Changed;
        }
        if matches!(key.code, KeyCode::Left | KeyCode::Right) && key.modifiers.is_empty() {
            let mode = self.mode;
            let row_width = self.row_width;
            let (folded_heads, state) = (&mut self.folded_heads, &mut self.state);
            let TreeNavigatorState::Ready { tree, picker } = state else {
                unreachable!("ready tree navigator has ready state");
            };
            let Some(focused) = picker.focused_value() else {
                return TreeNavigatorAction::None;
            };
            let selected = tree.entry_indexes[&focused];
            let query = picker.filter().to_owned();
            let projection = tree.filtered_projection(mode, &query);
            match key.code {
                KeyCode::Left
                    if projection.is_foldable(selected) && !folded_heads.contains(&focused) =>
                {
                    folded_heads.insert(focused);
                    tree.refresh_for_query(picker, mode, &query, row_width, folded_heads);
                    TreeNavigatorAction::Changed
                }
                KeyCode::Left => {
                    let target = projection.enclosing_segment_start(selected);
                    if target != selected && projection.direct_matches[target] {
                        picker.focus_value(tree.nodes[target].entry);
                        TreeNavigatorAction::Changed
                    } else {
                        TreeNavigatorAction::None
                    }
                }
                KeyCode::Right if folded_heads.remove(&focused) => {
                    tree.refresh_for_query(picker, mode, &query, row_width, folded_heads);
                    TreeNavigatorAction::Changed
                }
                KeyCode::Right => {
                    let target = projection.first_child_segment_start(selected);
                    if target != selected && projection.direct_matches[target] {
                        picker.focus_value(tree.nodes[target].entry);
                        TreeNavigatorAction::Changed
                    } else {
                        TreeNavigatorAction::None
                    }
                }
                _ => unreachable!("tree branch key was checked"),
            }
        } else {
            let mode = self.mode;
            let row_width = self.row_width;
            let (folded_heads, state) = (&mut self.folded_heads, &mut self.state);
            let TreeNavigatorState::Ready { tree, picker } = state else {
                unreachable!("ready tree navigator has ready state");
            };
            let filter = picker.filter().to_owned();
            match picker.handle_key(key) {
                PickerAction::None | PickerAction::NoMatch | PickerAction::Save(_) => {
                    TreeNavigatorAction::None
                }
                PickerAction::Changed => {
                    if picker.filter() != filter {
                        folded_heads.clear();
                        let query = picker.filter().to_owned();
                        tree.refresh_for_query(picker, mode, &query, row_width, folded_heads);
                    }
                    TreeNavigatorAction::Changed
                }
                PickerAction::Apply(entry) => TreeNavigatorAction::Navigate(entry),
                PickerAction::Close => unreachable!("tree navigator handles Escape"),
            }
        }
    }

    #[cfg(test)]
    fn row_item_constructions(&self) -> usize {
        let TreeNavigatorState::Ready { tree, .. } = &self.state else {
            unreachable!("catalogue installation creates ready tree");
        };
        tree.row_item_constructions()
    }

    fn copy_focused(&self) -> Option<String> {
        let TreeNavigatorState::Ready { tree, picker } = &self.state else {
            return None;
        };
        let entry = picker.focused_value()?;
        Some(copy_text(&tree.nodes[tree.entry_indexes[&entry]].kind))
    }

    fn picker(&self) -> &Picker<SessionEntryId> {
        match &self.state {
            TreeNavigatorState::Loading { picker } | TreeNavigatorState::Ready { picker, .. } => {
                picker
            }
        }
    }

    fn picker_mut(&mut self) -> &mut Picker<SessionEntryId> {
        match &mut self.state {
            TreeNavigatorState::Loading { picker } | TreeNavigatorState::Ready { picker, .. } => {
                picker
            }
        }
    }
}

fn render_rule(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(Style::new().fg(BORDER)),
        area,
    );
}

fn header_text(segments: &[String]) -> String {
    segments.join(HEADER_SEPARATOR)
}

fn display_width(text: &str) -> usize {
    Line::raw(text).width()
}

fn clip_end(text: &str, width: usize) -> String {
    let mut clipped = String::new();
    let mut used = 0_usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if used.saturating_add(grapheme_width) > width {
            break;
        }
        clipped.push_str(grapheme);
        used = used.saturating_add(grapheme_width);
    }
    clipped
}

struct StyledRow {
    spans: Vec<Span<'static>>,
    width: usize,
    max_width: usize,
    truncated: bool,
}

impl StyledRow {
    fn new(max_width: usize) -> Self {
        Self {
            spans: Vec::new(),
            width: 0,
            max_width,
            truncated: false,
        }
    }

    fn push(&mut self, content: &str, style: Style) {
        if self.truncated || content.is_empty() {
            return;
        }
        if self.max_width == 0 {
            self.truncated = true;
            return;
        }
        for grapheme in content.graphemes(true) {
            let grapheme_width = Line::raw(grapheme).width();
            if self.width >= self.max_width
                || self.width.saturating_add(grapheme_width) > self.max_width
            {
                self.ellipsis(style);
                self.truncated = true;
                return;
            }
            self.spans.push(Span::styled(grapheme.to_owned(), style));
            self.width = self.width.saturating_add(grapheme_width);
        }
    }

    fn is_truncated(&self) -> bool {
        self.truncated
    }

    fn into_spans(self) -> Vec<Span<'static>> {
        self.spans
    }

    fn ellipsis(&mut self, style: Style) {
        while self.width >= self.max_width {
            let span = self.spans.pop().expect("visible row has a span");
            self.width = self.width.saturating_sub(Line::raw(span.content).width());
        }
        self.spans.push(Span::styled("…", style));
        self.width += 1;
    }
}

fn row_variants(body: Vec<Span<'static>>, row_width: usize) -> (Line<'static>, Line<'static>) {
    let normal_gutter = match row_width {
        0 => "",
        1 => " ",
        _ => "  ",
    };
    let focused_gutter = match row_width {
        0 => "",
        1 => "›",
        _ => "› ",
    };
    let mut normal = vec![Span::styled(normal_gutter, colour_style(TEXT))];
    normal.extend(body.iter().cloned());
    let mut focused = vec![Span::styled(focused_gutter, colour_style(ACCENT))];
    focused.extend(body);
    let mut focused = Line::from(focused);
    for span in &mut focused.spans {
        span.style = span.style.bg(SELECTED_BACKGROUND);
    }
    (Line::from(normal), focused)
}

fn append_kind_row(row: &mut StyledRow, kind: &SessionCatalogueKind, content_style: Style) {
    match kind {
        SessionCatalogueKind::Start => row.push("Session start", content_style),
        SessionCatalogueKind::Request { text } => {
            row.push("User:", colour_style(ACCENT));
            append_preview(row, text, content_style);
        }
        SessionCatalogueKind::SkillActivation { name } => {
            row.push("Skill:", colour_style(ACCENT));
            append_preview(row, name, content_style);
        }
        SessionCatalogueKind::Execution {
            commentary,
            execution,
            ..
        } if matches!(
            execution.outcome,
            crate::arena::ExecutionOutcome::TypeCheckError(_)
        ) =>
        {
            row.push("Type check failed", colour_style(ERROR));
            if let Some(commentary) = commentary {
                append_preview(row, commentary, content_style);
            }
        }
        SessionCatalogueKind::Execution {
            commentary,
            execution,
            ..
        } => append_execution_row(
            row,
            commentary.as_deref(),
            &execution.tool_calls,
            content_style,
        ),
        SessionCatalogueKind::Final { text } => {
            row.push("Kiki:", colour_style(SUCCESS));
            append_preview(row, text, content_style);
        }
        SessionCatalogueKind::Cancelled => row.push("Cancelled", colour_style(WARNING)),
        SessionCatalogueKind::Failed => row.push("Failed", colour_style(ERROR)),
        SessionCatalogueKind::Compaction { outcome } => {
            row.push(&compaction_row(outcome), colour_style(COMPACTION));
        }
    }
}

fn append_preview(row: &mut StyledRow, text: &str, content_style: Style) {
    let text = normalise_single_line(text);
    if !text.is_empty() {
        row.push(" ", content_style);
        row.push(&text, content_style);
    }
}

fn append_execution_row(
    row: &mut StyledRow,
    commentary: Option<&str>,
    tool_calls: &[crate::arena::ToolCall],
    content_style: Style,
) {
    let commentary = commentary
        .map(normalise_single_line)
        .filter(|text| !text.is_empty());
    let failed = tool_calls
        .iter()
        .filter(|call| matches!(call.summary, crate::arena::ToolCallSummary::Failed))
        .count();
    let succeeded = tool_calls.len().saturating_sub(failed);

    if tool_calls.is_empty() && commentary.is_none() {
        row.push("Python", colour_style(MUTED));
        return;
    }
    row.push("Python:", colour_style(MUTED));
    if !tool_calls.is_empty() {
        row.push(" [tools: ", colour_style(DIM));
        match (tool_calls.len(), failed) {
            (total, 0) => row.push(&total.to_string(), colour_style(DIM)),
            (_, failed) if succeeded == 0 => {
                row.push(&failed.to_string(), colour_style(DIM));
                row.push("✗", colour_style(ERROR));
            }
            (_, failed) => {
                row.push(&succeeded.to_string(), colour_style(DIM));
                row.push("✓", colour_style(SUCCESS));
                row.push(" ", colour_style(DIM));
                row.push(&failed.to_string(), colour_style(DIM));
                row.push("✗", colour_style(ERROR));
            }
        }
        row.push("]", colour_style(DIM));
    }
    if let Some(commentary) = commentary {
        row.push(" ", content_style);
        row.push(&commentary, content_style);
    }
}

fn colour_style(colour: Color) -> Style {
    Style::new().fg(colour)
}

fn subtree_latest_revisions(topology: &TreeTopology, catalogue: &SessionCatalogue) -> Vec<u64> {
    let mut latest_revisions = catalogue
        .entries
        .iter()
        .map(|entry| entry.revision)
        .collect::<Vec<_>>();
    let mut remaining_children = topology.children.iter().map(Vec::len).collect::<Vec<_>>();
    let mut stack = topology
        .children
        .iter()
        .enumerate()
        .filter_map(|(index, children)| children.is_empty().then_some(index))
        .collect::<Vec<_>>();

    while let Some(index) = stack.pop() {
        let Some(parent) = topology.parents[index] else {
            continue;
        };
        latest_revisions[parent] = latest_revisions[parent].max(latest_revisions[index]);
        remaining_children[parent] -= 1;
        if remaining_children[parent] == 0 {
            stack.push(parent);
        }
    }

    latest_revisions
}

fn build_preorder(topology: &TreeTopology, root: usize) -> Vec<usize> {
    let mut preorder = Vec::with_capacity(topology.children.len());
    let mut stack = vec![root];
    while let Some(index) = stack.pop() {
        preorder.push(index);
        stack.extend(topology.children[index].iter().rev().copied());
    }
    preorder
}

fn row_text(kind: &SessionCatalogueKind) -> String {
    match kind {
        SessionCatalogueKind::Start => "Session start".to_owned(),
        SessionCatalogueKind::Request { text } => preview("User", text),
        SessionCatalogueKind::SkillActivation { name } => format!("Skill: {name}"),
        SessionCatalogueKind::Execution {
            commentary,
            execution,
            ..
        } if matches!(
            execution.outcome,
            crate::arena::ExecutionOutcome::TypeCheckError(_)
        ) =>
        {
            match commentary.as_deref().filter(|text| !text.is_empty()) {
                Some(text) => preview("Type check failed", text),
                None => "Type check failed".to_owned(),
            }
        }
        SessionCatalogueKind::Execution {
            commentary,
            execution,
            ..
        } => execution_row(commentary.as_deref(), &execution.tool_calls),
        SessionCatalogueKind::Final { text } => preview("Kiki", text),
        SessionCatalogueKind::Cancelled => "Cancelled".to_owned(),
        SessionCatalogueKind::Failed => "Failed".to_owned(),
        SessionCatalogueKind::Compaction { outcome } => compaction_row(outcome),
    }
}

fn compaction_row(outcome: &CompactionOutcome) -> String {
    match outcome {
        CompactionOutcome::Applied {
            context_tokens,
            reason,
            ..
        } => format!(
            "Compaction · {} · applied · {} tokens",
            compaction_reason_label(*reason),
            format_token_count(u128::from(*context_tokens))
        ),
        CompactionOutcome::Failed { reason, .. } => {
            format!("Compaction · {} · failed", compaction_reason_label(*reason))
        }
        CompactionOutcome::Cancelled { reason, .. } => {
            format!(
                "Compaction · {} · cancelled",
                compaction_reason_label(*reason)
            )
        }
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

fn execution_row(commentary: Option<&str>, tool_calls: &[crate::arena::ToolCall]) -> String {
    let commentary = commentary
        .map(normalise_single_line)
        .filter(|text| !text.is_empty());
    let failed = tool_calls
        .iter()
        .filter(|call| matches!(call.summary, crate::arena::ToolCallSummary::Failed))
        .count();
    let succeeded = tool_calls.len().saturating_sub(failed);
    let prefix = match (tool_calls.len(), failed) {
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

fn copy_text(kind: &SessionCatalogueKind) -> String {
    match kind {
        SessionCatalogueKind::Start => "Session start".to_owned(),
        SessionCatalogueKind::Request { text } | SessionCatalogueKind::Final { text } => {
            normalise_terminal_text(text)
        }
        SessionCatalogueKind::SkillActivation { name } => format!("Skill: {name}"),
        SessionCatalogueKind::Cancelled => "Cancelled".to_owned(),
        SessionCatalogueKind::Failed => "Failed".to_owned(),
        SessionCatalogueKind::Compaction { outcome } => match outcome {
            CompactionOutcome::Applied { summary, .. } => normalise_terminal_text(summary),
            CompactionOutcome::Failed { diagnostic, .. } => normalise_terminal_text(diagnostic),
            CompactionOutcome::Cancelled { reason, .. } => {
                normalise_terminal_text(compaction_reason_label(*reason))
            }
        },
        SessionCatalogueKind::Execution {
            source,
            commentary,
            execution,
        } => {
            let mut sections = Vec::new();
            if let Some(commentary) = commentary.as_deref().filter(|text| !text.is_empty()) {
                sections.push(copy_section("Commentary", commentary));
            }
            sections.push(copy_section("Python", source));
            if !execution.tool_calls.is_empty() {
                let tools = execution
                    .tool_calls
                    .iter()
                    .map(tool_call_label)
                    .collect::<Vec<_>>()
                    .join("\n");
                sections.push(copy_section("Tools", &tools));
            }
            if execution.stdout.total_bytes > 0 {
                sections.push(copy_section("Stdout", &observation_text(&execution.stdout)));
            }
            if execution.stderr.total_bytes > 0 {
                sections.push(copy_section("Stderr", &observation_text(&execution.stderr)));
            }
            match &execution.outcome {
                crate::arena::ExecutionOutcome::Complete {
                    result: Some(result),
                } => sections.push(copy_section("Result", &observation_text(result))),
                crate::arena::ExecutionOutcome::Complete { result: None } => {}
                crate::arena::ExecutionOutcome::PythonError(error)
                | crate::arena::ExecutionOutcome::TypeCheckError(error) => {
                    let mut text = error.text.clone();
                    if error.omitted_bytes > 0 {
                        text.push_str(&format!("\n[{} bytes omitted]", error.omitted_bytes));
                    }
                    let heading = if matches!(
                        execution.outcome,
                        crate::arena::ExecutionOutcome::TypeCheckError(_)
                    ) {
                        "Type check"
                    } else {
                        "Error"
                    };
                    sections.push(copy_section(heading, &text));
                }
            }
            sections.join("\n\n")
        }
    }
}

fn copy_section(heading: &str, content: &str) -> String {
    format!("{heading}:\n{}", normalise_terminal_text(content))
}

fn normalise_single_line(text: &str) -> String {
    normalise_terminal_text(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalise_header_line(text: &str) -> String {
    normalise_terminal_text(text).replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::{Color, Modifier};

    use super::{
        PICKER_ROW_WIDTH, SessionTree, TreeNavigator, TreeNavigatorAction, TreeProjection,
    };
    use crate::arena::{
        Execution, ExecutionOutcome, Observation, PythonError, ToolCall, ToolCallSummary, ToolName,
    };
    use crate::session::{
        CompactionOutcome, CompactionReason, SessionCatalogue, SessionCatalogueEntry,
        SessionCatalogueKind, SessionEntryId,
    };
    use crate::tui::picker::{Picker, PickerAction};

    #[test]
    fn navigator_tokenises_search_and_clears_it_before_closing() {
        let mut loading = TreeNavigator::new(7, "credentials python");
        assert_eq!(
            loading.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::None
        );
        assert_eq!(
            loading.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Close
        );

        let mut navigator = TreeNavigator::new(8, "  CREDENTIALS\tpython ");
        assert!(navigator.install_catalogue(
            8,
            &catalogue(
                3,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("authorise access")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("Inspect credentials"), Vec::new()),
                    ),
                ],
            )
        ));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(3))
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::None
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Close
        );
    }

    #[test]
    fn navigator_does_not_rebuild_rows_for_focus_movement() {
        let mut navigator = TreeNavigator::new(23, "");
        assert!(navigator.install_catalogue(
            23,
            &catalogue(
                1,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("needle")),
                ],
            )
        ));
        let constructions = navigator.row_item_constructions();

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(navigator.picker().focused_value(), Some(SessionEntryId(2)));
        assert_eq!(navigator.row_item_constructions(), constructions);

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(navigator.picker().filter(), "n");
        assert!(navigator.row_item_constructions() > constructions);
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(2))
        );
    }

    #[test]
    fn header_prioritises_live_state_on_one_line() {
        let mut navigator = TreeNavigator::new(17, "credentials");
        assert!(navigator.install_catalogue(
            17,
            &catalogue(
                3,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("authorise access")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("inspect credentials"), Vec::new()),
                    ),
                ],
            )
        ));

        for (width, expected) in [
            (
                200,
                "Session tree · Search: credentials · 1/1 · Enter restore · Esc clear/close · ↑/↓ move · ←/→ branch · Ctrl-X copy · Ctrl-U conversation · PgUp/PgDn page",
            ),
            (
                95,
                "Session tree · Search: credentials · 1/1 · Enter restore · Esc clear/close · ↑/↓ move",
            ),
            (32, "Session tree · Search: …dentials"),
            (10, "Session tr"),
        ] {
            assert_eq!(navigator.header(width, None), expected);
        }
        assert_eq!(
            navigator.header(200, Some(" Copied\n\t\u{1b}")),
            "Session tree · Copied \\x1B · Search: credentials · 1/1"
        );
        assert_eq!(navigator.header(16, Some("🧑‍💻")), "Session tree · ");
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.header(200, None),
            "Session tree · Search: credentials · 1/1 · [conversation] · Enter restore · Esc clear/close · ↑/↓ move · ←/→ branch · Ctrl-X copy · Ctrl-U conversation · PgUp/PgDn page"
        );

        let loading = TreeNavigator::new(18, "credentials");
        assert_eq!(
            loading.header(200, None),
            "Session tree · Search: credentials · Enter restore · Esc clear/close · ↑/↓ move · ←/→ branch · Ctrl-X copy · Ctrl-U conversation · PgUp/PgDn page"
        );
        let mut no_match = TreeNavigator::new(19, "missing");
        assert!(no_match.install_catalogue(
            19,
            &catalogue(1, [entry(1, None, 1, SessionCatalogueKind::Start)])
        ));
        assert_eq!(
            no_match.header(200, None),
            "Session tree · Search: missing · 0/0 · Enter restore · Esc clear/close · ↑/↓ move · ←/→ branch · Ctrl-X copy · Ctrl-U conversation · PgUp/PgDn page"
        );

        let unicode = TreeNavigator::new(20, "needle 🧑‍💻");
        assert_eq!(unicode.header(25, None), "Session tree · Search: …");

        let repeated_spaces = TreeNavigator::new(21, "  repeated  spaces  \nnext  ");
        let repeated_header = repeated_spaces.header(200, None);
        assert!(
            repeated_header.contains("Search:   repeated  spaces  \\nnext  "),
            "{repeated_header}"
        );
        assert!(!repeated_header.contains('\n'), "{repeated_header}");

        let mut whitespace_only = TreeNavigator::new(22, "   ");
        assert!(whitespace_only.install_catalogue(
            22,
            &catalogue(1, [entry(1, None, 1, SessionCatalogueKind::Start)])
        ));
        assert!(
            whitespace_only.header(200, None).contains("Search:    "),
            "{}",
            whitespace_only.header(200, None)
        );
        assert_eq!(
            whitespace_only.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert!(!whitespace_only.header(200, None).contains("Search:"));
    }

    #[test]
    fn dock_renders_full_width_rules_and_frameless_content() {
        let mut navigator = TreeNavigator::new(21, "");
        assert!(navigator.install_catalogue(
            21,
            &catalogue(
                2,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("frameless row")),
                ],
            )
        ));
        let mut terminal = Terminal::new(TestBackend::new(40, 6)).unwrap();
        terminal
            .draw(|frame| navigator.render(frame, frame.area(), None))
            .unwrap();
        let buffer = terminal.backend().buffer();

        for row in [0, 2] {
            for column in 0..40 {
                assert_eq!(buffer[(column, row)].symbol(), "─");
                assert_eq!(buffer[(column, row)].fg, Color::Rgb(95, 135, 255));
            }
        }
        assert!(row_text(buffer, 1).starts_with("Session tree"));
        assert_eq!(buffer[(0, 3)].symbol(), " ");
        assert!(row_text(buffer, 3).contains("Session start"));
        assert!(row_text(buffer, 4).contains("frameless row"));
        assert!(!row_text(buffer, 3).contains("Filter:"));
        assert_ne!(buffer[(0, 0)].symbol(), "┌");
    }

    #[test]
    fn copy_formats_complete_entries_without_display_truncation() {
        let omitted = Observation {
            head: "first output".to_owned(),
            tail: Some("last output".to_owned()),
            head_lines: 1,
            tail_lines: 1,
            total_lines: 4,
            total_bytes: 30,
            omitted_lines: 2,
            omitted_bytes: 9,
            head_ends_mid_line: false,
            tail_starts_mid_line: false,
            partial_lines: false,
        };
        let cases = vec![
            (
                detailed_execution(
                    "print('first')\r\nprint('second')",
                    Some("explain\nthis"),
                    omitted,
                    observation("diagnostic"),
                    ExecutionOutcome::Complete {
                        result: Some(observation("result value")),
                    },
                    vec![
                        ToolCall {
                            tool: ToolName::Run,
                            summary: ToolCallSummary::Exit(7),
                        },
                        ToolCall {
                            tool: ToolName::Read,
                            summary: ToolCallSummary::Failed,
                        },
                        ToolCall {
                            tool: ToolName::Write,
                            summary: ToolCallSummary::Signal,
                        },
                    ],
                ),
                "Commentary:\nexplain\nthis\n\nPython:\nprint('first')\nprint('second')\n\nTools:\nrun · exit 7\nread · failed\nwrite · signal\n\nStdout:\nfirst output\n[2 lines, 9 bytes omitted]\nlast output\n\nStderr:\ndiagnostic\n\nResult:\nresult value",
            ),
            (
                detailed_execution(
                    "pass",
                    None,
                    observation(""),
                    observation(""),
                    ExecutionOutcome::Complete { result: None },
                    Vec::new(),
                ),
                "Python:\npass",
            ),
            (
                detailed_execution(
                    "raise ValueError('bad')",
                    None,
                    observation(""),
                    observation(""),
                    ExecutionOutcome::PythonError(PythonError {
                        text: "bad\tvalue\u{1b}".to_owned(),
                        omitted_bytes: 5,
                    }),
                    Vec::new(),
                ),
                "Python:\nraise ValueError('bad')\n\nError:\nbad    value\\x1B\n[5 bytes omitted]",
            ),
            (
                detailed_execution(
                    "wrong: int = 'text'",
                    None,
                    observation(""),
                    observation(""),
                    ExecutionOutcome::TypeCheckError(PythonError {
                        text: "invalid-assignment".to_owned(),
                        omitted_bytes: 0,
                    }),
                    Vec::new(),
                ),
                "Python:\nwrong: int = 'text'\n\nType check:\ninvalid-assignment",
            ),
            (request("first\r\nsecond\t\u{1b}"), "first\nsecond    \\x1B"),
            (
                SessionCatalogueKind::Final {
                    text: "first\r\nsecond\u{9d}".to_owned(),
                },
                "first\nsecond\\x9D",
            ),
            (SessionCatalogueKind::Start, "Session start"),
            (SessionCatalogueKind::Cancelled, "Cancelled"),
            (SessionCatalogueKind::Failed, "Failed"),
        ];

        for (kind, expected) in cases {
            assert_eq!(
                copy_action(kind),
                TreeNavigatorAction::Copy(expected.to_owned())
            );
        }
    }

    #[test]
    fn compaction_rows_search_and_copy_use_only_the_durable_outcome_fields() {
        let kinds = [
            (
                SessionCatalogueKind::Compaction {
                    outcome: CompactionOutcome::Applied {
                        summary: "summary\u{1b}".to_owned(),
                        first_retained: SessionEntryId(2),
                        context_tokens: 1_250,
                        reason: CompactionReason::Manual,
                        attempts: Vec::new(),
                    },
                },
                "Compaction · manual · applied · 1.25k tokens",
                "summary\\x1B",
            ),
            (
                SessionCatalogueKind::Compaction {
                    outcome: CompactionOutcome::Failed {
                        diagnostic: "failed\u{1b}".to_owned(),
                        reason: CompactionReason::AutomaticThreshold,
                        attempts: Vec::new(),
                    },
                },
                "Compaction · automatic threshold · failed",
                "failed\\x1B",
            ),
            (
                SessionCatalogueKind::Compaction {
                    outcome: CompactionOutcome::Cancelled {
                        reason: CompactionReason::ContextOverflow,
                        attempts: Vec::new(),
                    },
                },
                "Compaction · context overflow · cancelled",
                "context overflow",
            ),
        ];

        for (kind, row, copy) in kinds {
            assert_eq!(
                super::compaction_row(match &kind {
                    SessionCatalogueKind::Compaction { outcome } => outcome,
                    _ => unreachable!("compaction kind"),
                }),
                row
            );
            let tree = SessionTree::from_catalogue(&catalogue(
                2,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, kind.clone()),
                ],
            ));
            assert_eq!(
                tree.nodes[1].canonical_search_text,
                super::canonical_search(row)
            );
            let TreeNavigatorAction::Copy(copied) = copy_action(kind) else {
                panic!("compaction must be copyable")
            };
            assert_eq!(copied, copy);
        }
    }

    #[test]
    fn compaction_outcomes_are_tree_entries_in_both_modes() {
        let outcomes = [
            SessionCatalogueKind::Compaction {
                outcome: CompactionOutcome::Applied {
                    summary: "hidden summary".to_owned(),
                    first_retained: SessionEntryId(2),
                    context_tokens: 1_250,
                    reason: CompactionReason::Manual,
                    attempts: Vec::new(),
                },
            },
            SessionCatalogueKind::Compaction {
                outcome: CompactionOutcome::Failed {
                    diagnostic: "hidden diagnostic".to_owned(),
                    reason: CompactionReason::AutomaticThreshold,
                    attempts: Vec::new(),
                },
            },
            SessionCatalogueKind::Compaction {
                outcome: CompactionOutcome::Cancelled {
                    reason: CompactionReason::ContextOverflow,
                    attempts: Vec::new(),
                },
            },
        ];
        let catalogue = catalogue(
            5,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, request("compact")),
                entry(3, Some(2), 3, outcomes[0].clone()),
                entry(4, Some(3), 4, outcomes[1].clone()),
                entry(5, Some(4), 5, outcomes[2].clone()),
            ],
        );
        let mut navigator = TreeNavigator::new(5, "");
        assert!(navigator.install_catalogue(5, &catalogue));
        for expected in ["applied", "failed", "cancelled"] {
            assert!(
                tree_rows(navigator.picker_mut())
                    .iter()
                    .any(|row| row.contains(expected))
            );
        }
        assert!(navigator.picker_mut().focus_value(SessionEntryId(4)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(4))
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        for expected in ["applied", "failed", "cancelled"] {
            assert!(
                tree_rows(navigator.picker_mut())
                    .iter()
                    .any(|row| row.contains(expected))
            );
        }

        let mut hidden = TreeNavigator::new(5, "hidden summary");
        assert!(hidden.install_catalogue(5, &catalogue));
        assert!(tree_rows(hidden.picker_mut()).is_empty());

        let mut row = super::StyledRow::new(PICKER_ROW_WIDTH);
        super::append_kind_row(&mut row, &outcomes[0], super::colour_style(super::TEXT));
        assert_eq!(super::COMPACTION, Color::Rgb(118, 150, 86));
        assert_ne!(super::COMPACTION, super::ACCENT);
        assert_ne!(super::COMPACTION, super::SUCCESS);
        assert_ne!(super::COMPACTION, super::ERROR);
        assert_ne!(super::COMPACTION, super::WARNING);
        assert!(
            row.spans
                .iter()
                .all(|span| span.style.fg == Some(super::COMPACTION))
        );
    }

    #[test]
    fn rejected_code_is_labelled_as_unexecuted_in_tree_rows() {
        let kind = detailed_execution(
            "wrong: int = 'text'",
            None,
            observation(""),
            observation(""),
            ExecutionOutcome::TypeCheckError(PythonError {
                text: "invalid-assignment".to_owned(),
                omitted_bytes: 0,
            }),
            Vec::new(),
        );
        assert_eq!(super::row_text(&kind), "Type check failed");
        let mut row = super::StyledRow::new(PICKER_ROW_WIDTH);
        super::append_kind_row(&mut row, &kind, super::colour_style(super::TEXT));
        assert_eq!(
            row.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "Type check failed"
        );
        assert!(
            row.spans
                .iter()
                .all(|span| span.style.fg == Some(super::ERROR))
        );
    }

    #[test]
    fn execution_paths_remain_private_in_rows_search_and_copy() {
        let kind = SessionCatalogueKind::Execution {
            source: "pass".to_owned(),
            commentary: None,
            execution: Box::new(Execution {
                stdout: observation(""),
                stderr: observation(""),
                outcome: ExecutionOutcome::Complete { result: None },
                tool_calls: Vec::new(),
                read_paths: vec![PathBuf::from("private/effect-sentinel")],
                modified_paths: vec![PathBuf::from("private/mutation-sentinel")],
                changed_bindings: Vec::new(),
            }),
        };
        let catalogue = catalogue(
            2,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, kind.clone()),
            ],
        );
        let tree = SessionTree::from_catalogue(&catalogue);
        let mut picker = tree.picker();
        let rows = tree_rows(&mut picker);
        assert!(
            rows.iter()
                .all(|row| !row.contains("private/effect-sentinel"))
        );
        assert!(
            tree.nodes[1]
                .canonical_search_text
                .find("private/effect-sentinel")
                .is_none()
        );
        let TreeNavigatorAction::Copy(copy) = copy_action(kind) else {
            panic!("execution must be copyable");
        };
        assert!(!copy.contains("private/effect-sentinel"));
        assert!(!copy.contains("private/mutation-sentinel"));
    }

    #[test]
    fn copy_ignores_loading_and_no_match_trees() {
        let mut loading = TreeNavigator::new(13, "");
        assert_eq!(
            loading.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::None
        );

        let mut no_match = TreeNavigator::new(14, "missing");
        assert!(no_match.install_catalogue(
            14,
            &catalogue(1, [entry(1, None, 1, SessionCatalogueKind::Start)])
        ));
        assert_eq!(
            no_match.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::None
        );
    }

    #[test]
    fn copy_preserves_ready_tree_view_state() {
        let mut navigator = TreeNavigator::new(16, "needle");
        assert!(navigator.install_catalogue(
            16,
            &catalogue(
                4,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("needle request")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("needle execution"), Vec::new())
                    ),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Final {
                            text: "needle final".to_owned(),
                        },
                    ),
                    entry(5, Some(1), 5, request("needle sibling")),
                ],
            )
        ));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert!(navigator.picker_mut().focus_value(SessionEntryId(2)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        let folded_rows = tree_rows(navigator.picker_mut());

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Copy("needle request".to_owned())
        );
        assert_eq!(navigator.picker().filter(), "needle");
        assert_eq!(navigator.picker().focused_value(), Some(SessionEntryId(2)));
        assert_eq!(tree_rows(navigator.picker_mut()), folded_rows);

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert!(
            tree_rows(navigator.picker_mut())
                .iter()
                .all(|row| !row.contains("Python"))
        );
    }

    #[test]
    fn conversation_mode_reparents_history_and_keeps_current_python() {
        let mut navigator = TreeNavigator::new(4, "");
        assert!(navigator.install_catalogue(
            4,
            &catalogue(
                5,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("conversation")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("omitted execution"), Vec::new()),
                    ),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Final {
                            text: "reconnected reply".to_owned(),
                        },
                    ),
                    entry(
                        5,
                        Some(4),
                        5,
                        execution(Some("current execution"), Vec::new()),
                    ),
                ],
            )
        ));
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "└─ User: conversation",
                "   Python: omitted execution",
                "   Kiki: reconnected reply",
                "   ● Python: current execution",
            ]
        );

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "└─ User: conversation",
                "   Kiki: reconnected reply",
                "   ● Python: current execution",
            ]
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(5))
        );

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "└─ User: conversation",
                "   Python: omitted execution",
                "   Kiki: reconnected reply",
                "   ● Python: current execution",
            ]
        );
    }

    #[test]
    fn conversation_mode_keeps_terminal_outcomes_and_reconstructs_forks() {
        let mut navigator = TreeNavigator::new(5, "");
        assert!(navigator.install_catalogue(
            5,
            &catalogue(
                6,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("terminal outcomes")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("omitted branch point"), Vec::new()),
                    ),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Final {
                            text: "older reply".to_owned(),
                        },
                    ),
                    entry(5, Some(3), 5, SessionCatalogueKind::Cancelled),
                    entry(6, Some(3), 6, SessionCatalogueKind::Failed),
                ],
            )
        ));

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "└─ User: terminal outcomes",
                "   ├─ ● Failed",
                "   ├─ Cancelled",
                "   └─ Kiki: older reply",
            ]
        );
    }

    #[test]
    fn conversation_mode_filtered_focus_prefers_a_matching_ancestor_or_first_match() {
        let mut ancestor = TreeNavigator::new(6, "needle");
        assert!(ancestor.install_catalogue(
            6,
            &catalogue(
                4,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("needle ancestor")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("needle focused execution"), Vec::new()),
                    ),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Final {
                            text: "current reply".to_owned(),
                        },
                    ),
                ],
            )
        ));
        assert_eq!(
            ancestor.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            ancestor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            ancestor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(2))
        );

        let mut fallback = TreeNavigator::new(7, "needle");
        assert!(fallback.install_catalogue(
            7,
            &catalogue(
                3,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(
                        2,
                        Some(1),
                        2,
                        execution(Some("needle focused execution"), Vec::new()),
                    ),
                    entry(
                        3,
                        Some(1),
                        3,
                        SessionCatalogueKind::Final {
                            text: "needle fallback".to_owned(),
                        },
                    ),
                ],
            )
        ));
        assert_eq!(
            fallback.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            fallback.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            fallback.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(3))
        );
    }

    #[test]
    fn conversation_mode_searches_eligible_rows_and_keeps_context() {
        let mut navigator = TreeNavigator::new(7, "");
        assert!(navigator.install_catalogue(
            7,
            &catalogue(
                4,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("conversation context")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("hidden execution"), Vec::new()),
                    ),
                    entry(
                        4,
                        Some(3),
                        4,
                        SessionCatalogueKind::Final {
                            text: "matching response".to_owned(),
                        },
                    ),
                ],
            )
        ));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        for character in "matching".chars() {
            assert_eq!(
                navigator.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                TreeNavigatorAction::Changed
            );
        }
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "└─ User: conversation context",
                "   ● Kiki: matching response",
            ]
        );

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        for character in "hidden".chars() {
            assert_eq!(
                navigator.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                TreeNavigatorAction::Changed
            );
        }
        assert!(tree_rows(navigator.picker_mut()).is_empty());
    }

    #[test]
    fn conversation_mode_reuses_deep_topology_across_query_and_width_refreshes() {
        let depth = 2_048;
        let active_entry = SessionEntryId(depth * 2 + 1);
        let mut navigator = TreeNavigator::new(8, "");
        assert!(navigator.install_catalogue(8, &deep_conversation_catalogue(depth),));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );

        for row_width in [8, 16, 32, PICKER_ROW_WIDTH] {
            navigator.refresh_width(row_width);
            for character in "reply".chars() {
                assert_eq!(
                    navigator
                        .handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                    TreeNavigatorAction::Changed
                );
            }
            assert_eq!(
                navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                TreeNavigatorAction::Navigate(active_entry)
            );
            assert_eq!(
                navigator.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
                TreeNavigatorAction::Changed
            );
        }
    }

    #[test]
    fn conversation_mode_preserves_complete_durable_recency_order() {
        let mut navigator = TreeNavigator::new(8, "");
        assert!(navigator.install_catalogue(
            8,
            &catalogue(
                7,
                [
                    entry(1, None, 1, SessionCatalogueKind::Start),
                    entry(2, Some(1), 2, request("older branch")),
                    entry(
                        3,
                        Some(2),
                        3,
                        execution(Some("older execution"), Vec::new()),
                    ),
                    entry(
                        4,
                        Some(3),
                        9,
                        SessionCatalogueKind::Final {
                            text: "latest durable descendant".to_owned(),
                        },
                    ),
                    entry(5, Some(1), 4, request("newer branch")),
                    entry(
                        6,
                        Some(5),
                        5,
                        execution(Some("newer execution"), Vec::new()),
                    ),
                    entry(
                        7,
                        Some(6),
                        6,
                        SessionCatalogueKind::Final {
                            text: "older durable descendant".to_owned(),
                        },
                    ),
                ],
            )
        ));

        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "├⊟ User: older branch",
                "│  Kiki: latest durable descendant",
                "└⊟ User: newer branch",
                "   ● Kiki: older durable descendant",
            ]
        );
    }

    #[test]
    fn left_and_right_fold_and_traverse_branch_segments() {
        let catalogue = catalogue(
            8,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, request("segment head needle")),
                entry(3, Some(2), 3, request("flat continuation needle")),
                entry(4, Some(3), 4, request("fork needle")),
                entry(5, Some(4), 5, request("older segment needle")),
                entry(6, Some(5), 6, request("older descendant needle")),
                entry(7, Some(4), 7, request("newer segment needle")),
                entry(8, Some(7), 8, request("active hidden descendant needle")),
                entry(9, Some(1), 9, request("root sibling needle")),
            ],
        );
        let mut navigator = TreeNavigator::new(9, "");
        assert!(navigator.install_catalogue(9, &catalogue));
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "├─ User: root sibling needle",
                "└⊟ User: segment head needle",
                "   User: flat continuation needle",
                "   User: fork needle",
                "   ├⊟ User: newer segment needle",
                "   │  ● User: active hidden descendant needle",
                "   └⊟ User: older segment needle",
                "      User: older descendant needle",
            ]
        );

        assert!(navigator.picker_mut().focus_value(SessionEntryId(2)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "├─ User: root sibling needle",
                "└⊞ User: segment head needle",
            ]
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(2))
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            tree_rows(navigator.picker_mut()),
            [
                "Session start",
                "├─ User: root sibling needle",
                "└⊟ User: segment head needle",
                "   User: flat continuation needle",
                "   User: fork needle",
                "   ├⊟ User: newer segment needle",
                "   │  ● User: active hidden descendant needle",
                "   └⊟ User: older segment needle",
                "      User: older descendant needle",
            ]
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(2))
        );

        assert!(navigator.picker_mut().focus_value(SessionEntryId(4)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(2))
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(7))
        );

        assert!(navigator.picker_mut().focus_value(SessionEntryId(1)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(tree_rows(navigator.picker_mut()), ["⊞ Session start"]);
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::None
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(9))
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            TreeNavigatorAction::None
        );

        assert!(navigator.picker_mut().focus_value(SessionEntryId(2)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert!(
            tree_rows(navigator.picker_mut())
                .iter()
                .any(|row| row.contains("flat continuation needle"))
        );
        assert!(navigator.picker_mut().focus_value(SessionEntryId(2)));
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            navigator.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert!(
            tree_rows(navigator.picker_mut())
                .iter()
                .any(|row| row.contains("flat continuation needle"))
        );

        let mut escaped = TreeNavigator::new(10, "needle");
        assert!(escaped.install_catalogue(10, &catalogue));
        assert!(escaped.picker_mut().focus_value(SessionEntryId(2)));
        assert_eq!(
            escaped.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            escaped.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert!(
            tree_rows(escaped.picker_mut())
                .iter()
                .any(|row| row.contains("flat continuation needle"))
        );

        let mut conversation = TreeNavigator::new(11, "");
        assert!(conversation.install_catalogue(11, &catalogue));
        assert!(conversation.picker_mut().focus_value(SessionEntryId(2)));
        assert_eq!(
            conversation.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::Changed
        );
        assert_eq!(
            conversation.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            TreeNavigatorAction::Changed
        );
        assert!(
            tree_rows(conversation.picker_mut())
                .iter()
                .any(|row| row.contains("flat continuation needle"))
        );

        let mut filtered = TreeNavigator::new(12, "flat continuation");
        assert!(filtered.install_catalogue(12, &catalogue));
        assert_eq!(
            filtered.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            TreeNavigatorAction::None
        );
        assert_eq!(
            filtered.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            TreeNavigatorAction::Navigate(SessionEntryId(3))
        );
    }

    #[test]
    fn semantic_rows_preserve_colours_under_subtle_selection() {
        let tree = SessionTree::from_catalogue(&catalogue(
            11,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, request("alternative request")),
                entry(
                    3,
                    Some(1),
                    3,
                    execution(
                        Some("alternative execution"),
                        vec![ToolCallSummary::Count(1)],
                    ),
                ),
                entry(
                    4,
                    Some(1),
                    4,
                    SessionCatalogueKind::Final {
                        text: "alternative final".to_owned(),
                    },
                ),
                entry(5, Some(1), 5, SessionCatalogueKind::Cancelled),
                entry(6, Some(1), 6, SessionCatalogueKind::Failed),
                entry(7, Some(1), 7, request("active request")),
                entry(
                    8,
                    Some(7),
                    8,
                    execution(
                        Some("active execution"),
                        vec![ToolCallSummary::Count(1), ToolCallSummary::Failed],
                    ),
                ),
                entry(
                    9,
                    Some(8),
                    9,
                    SessionCatalogueKind::Final {
                        text: "active final".to_owned(),
                    },
                ),
                entry(10, Some(9), 10, SessionCatalogueKind::Cancelled),
                entry(11, Some(10), 11, SessionCatalogueKind::Failed),
            ],
        ));
        let mut picker = tree.picker();
        let buffer = rendered_buffer(&mut picker, 72, 20);

        let (failed_column, failed_row) = cell_start(&buffer, "Failed");
        assert_eq!(buffer[(1, failed_row)].symbol(), "›");
        assert_eq!(buffer[(1, failed_row)].fg, Color::Rgb(138, 190, 183));
        assert_eq!(buffer[(1, failed_row)].bg, Color::Rgb(58, 58, 74));
        assert_eq!(
            buffer[(failed_column, failed_row)].fg,
            Color::Rgb(204, 102, 102)
        );
        assert_eq!(
            buffer[(failed_column, failed_row)].bg,
            Color::Rgb(58, 58, 74)
        );
        assert!(
            !buffer[(failed_column, failed_row)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(buffer[(failed_column + 6, failed_row)].bg, Color::Reset);
        let (current_marker_column, current_marker_row) = cell_start(&buffer, "● Failed");
        assert_eq!(
            buffer[(current_marker_column, current_marker_row)].fg,
            Color::Rgb(138, 190, 183)
        );
        assert_eq!(
            buffer[(current_marker_column, current_marker_row)].bg,
            Color::Rgb(58, 58, 74)
        );

        let (root_marker_column, root_marker_row) = cell_start(&buffer, "• Session start");
        assert_eq!(
            buffer[(root_marker_column, root_marker_row)].fg,
            Color::Rgb(138, 190, 183)
        );
        assert_eq!(
            buffer[(root_marker_column + 2, root_marker_row)].fg,
            Color::Rgb(212, 212, 212)
        );

        let (active_connector_column, active_connector_row) =
            cell_start(&buffer, "├⊟ • User: active request");
        assert_eq!(
            buffer[(active_connector_column, active_connector_row)].fg,
            Color::Rgb(138, 190, 183)
        );
        assert_eq!(
            buffer[(active_connector_column + 3, active_connector_row)].fg,
            Color::Rgb(138, 190, 183)
        );
        assert_eq!(
            buffer[(active_connector_column + 5, active_connector_row)].fg,
            Color::Rgb(138, 190, 183)
        );
        assert_eq!(
            buffer[(active_connector_column + 11, active_connector_row)].fg,
            Color::Rgb(212, 212, 212)
        );

        let (python_column, python_row) =
            cell_start(&buffer, "Python: [tools: 1✓ 1✗] active execution");
        assert_eq!(
            buffer[(python_column, python_row)].fg,
            Color::Rgb(128, 128, 128)
        );
        assert_eq!(
            buffer[(python_column + 9, python_row)].fg,
            Color::Rgb(102, 102, 102)
        );
        assert_eq!(
            buffer[(python_column + 17, python_row)].fg,
            Color::Rgb(181, 189, 104)
        );
        assert_eq!(
            buffer[(python_column + 20, python_row)].fg,
            Color::Rgb(204, 102, 102)
        );
        assert_eq!(
            buffer[(python_column + 23, python_row)].fg,
            Color::Rgb(212, 212, 212)
        );

        let (kiki_column, kiki_row) = cell_start(&buffer, "Kiki: active final");
        assert_eq!(
            buffer[(kiki_column, kiki_row)].fg,
            Color::Rgb(181, 189, 104)
        );
        assert_eq!(
            buffer[(kiki_column + 6, kiki_row)].fg,
            Color::Rgb(212, 212, 212)
        );
        let (cancelled_column, cancelled_row) = cell_start(&buffer, "Cancelled");
        assert_eq!(
            buffer[(cancelled_column, cancelled_row)].fg,
            Color::Rgb(255, 255, 0)
        );

        let (alternative_connector_column, alternative_row) =
            cell_start(&buffer, "└─ User: alternative request");
        assert_eq!(buffer[(1, alternative_row)].symbol(), " ");
        assert_eq!(buffer[(2, alternative_row)].symbol(), " ");
        assert_eq!(
            buffer[(alternative_connector_column, alternative_row)].fg,
            Color::Rgb(102, 102, 102)
        );
        assert_eq!(
            buffer[(alternative_connector_column + 3, alternative_row)].fg,
            Color::Rgb(138, 190, 183)
        );
        assert_eq!(
            buffer[(alternative_connector_column + 9, alternative_row)].fg,
            Color::Rgb(128, 128, 128)
        );
        let alternative = (buffer.area.left()..buffer.area.right())
            .map(|column| buffer[(column, alternative_row)].symbol())
            .collect::<String>();
        assert!(!alternative.contains('•'));
        assert!(!alternative.contains('●'));

        let (alternative_python_column, alternative_python_row) =
            cell_start(&buffer, "Python: [tools: 1] alternative execution");
        assert_eq!(
            buffer[(alternative_python_column, alternative_python_row)].fg,
            Color::Rgb(128, 128, 128)
        );
        assert_eq!(
            buffer[(alternative_python_column + 19, alternative_python_row)].fg,
            Color::Rgb(128, 128, 128)
        );

        let (alternative_kiki_column, alternative_kiki_row) =
            cell_start(&buffer, "Kiki: alternative final");
        assert_eq!(
            buffer[(alternative_kiki_column, alternative_kiki_row)].fg,
            Color::Rgb(181, 189, 104)
        );
        assert_eq!(
            buffer[(alternative_kiki_column + 6, alternative_kiki_row)].fg,
            Color::Rgb(128, 128, 128)
        );
        let cancelled_rows = cell_starts(&buffer, "Cancelled");
        let (alternative_cancelled_column, alternative_cancelled_row) =
            *cancelled_rows.last().expect("alternative cancelled row");
        assert_eq!(
            buffer[(alternative_cancelled_column, alternative_cancelled_row)].fg,
            Color::Rgb(255, 255, 0)
        );
        assert_eq!(
            buffer[(alternative_cancelled_column, alternative_cancelled_row)].bg,
            Color::Reset
        );
        let failed_rows = cell_starts(&buffer, "Failed");
        let (alternative_failed_column, alternative_failed_row) =
            *failed_rows.last().expect("alternative failed row");
        assert_eq!(
            buffer[(alternative_failed_column, alternative_failed_row)].fg,
            Color::Rgb(204, 102, 102)
        );
        assert_eq!(
            buffer[(alternative_failed_column, alternative_failed_row)].bg,
            Color::Reset
        );
    }

    #[test]
    fn orders_sibling_branches_by_latest_descendant_revision() {
        let tree = SessionTree::from_catalogue(&catalogue(
            4,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, request("older continued")),
                entry(3, Some(1), 3, request("older active")),
                entry(4, Some(3), 4, request("older active leaf")),
                entry(5, Some(1), 5, request("recent sibling")),
                entry(6, Some(5), 6, request("recent sibling leaf")),
                entry(7, Some(2), 7, request("latest continuation")),
            ],
        ));
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "├⊟ User: older continued",
                "│  User: latest continuation",
                "├⊟ User: recent sibling",
                "│  User: recent sibling leaf",
                "└⊟ User: older active",
                "   ● User: older active leaf",
            ]
        );
    }

    #[test]
    fn renders_linear_history_at_first_level_without_false_joints() {
        let tree = SessionTree::from_catalogue(&catalogue(
            4,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, request("first request")),
                entry(
                    3,
                    Some(2),
                    3,
                    execution(Some("first execution"), Vec::new()),
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
        ));
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
    fn renders_one_fork_with_linear_continuations_by_latest_descendant() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
                    execution(Some("second step"), vec![ToolCallSummary::Count(1)]),
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
                entry(5, Some(4), 5, execution(None, Vec::new())),
                entry(
                    2,
                    Some(1),
                    2,
                    SessionCatalogueKind::Request {
                        text: "original".to_owned(),
                    },
                ),
                entry(3, Some(2), 3, execution(Some("first step"), Vec::new())),
            ],
        ));
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "└─ User: original",
                "   Python: first step",
                "   ├─ ● Python: [tools: 1] second step",
                "   └⊟ User: first branch",
                "      Python",
                "      Kiki: first reply",
            ]
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(7))
        );
    }

    #[test]
    fn marks_the_active_root() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
        ));
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            ["● Session start", "└─ User: alternative"]
        );
    }

    #[test]
    fn orders_equal_latest_siblings_by_direct_revision_and_entry_id() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
                entry(5, Some(4), 6, execution(Some("second"), Vec::new())),
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
                entry(3, Some(2), 3, execution(Some("first"), Vec::new())),
            ],
        ));
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "├⊟ User: second",
                "│  ● Python: second",
                "└⊟ User: first",
                "   Python: first",
                "   ├─ User: same revision higher id",
                "   └─ User: same revision lower id",
            ]
        );
    }

    #[test]
    fn renders_complete_row_vocabulary_and_normalises_text() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
                entry(3, Some(1), 3, execution(Some(" \n \t "), Vec::new())),
                entry(
                    4,
                    Some(1),
                    4,
                    execution(Some("# comment\n only"), Vec::new()),
                ),
                entry(5, Some(1), 5, execution(None, Vec::new())),
                entry(
                    6,
                    Some(1),
                    6,
                    execution(
                        Some("all passed"),
                        vec![
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
                    ),
                ),
                entry(
                    7,
                    Some(1),
                    7,
                    execution(
                        Some("mixed"),
                        vec![
                            ToolCallSummary::Exit(7),
                            ToolCallSummary::Signal,
                            ToolCallSummary::Failed,
                        ],
                    ),
                ),
                entry(
                    8,
                    Some(1),
                    8,
                    execution(None, vec![ToolCallSummary::Failed, ToolCallSummary::Failed]),
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
        ));
        let mut picker = tree.picker();

        assert_eq!(
            labels(&mut picker),
            [
                "Session start",
                "Failed",
                "Cancelled",
                "Kiki: reply text \\x1B",
                "Python: [tools: 2✗]",
                "Python: [tools: 2✓ 1✗] mixed",
                "Python: [tools: 9] all passed",
                "Python",
                "Python: # comment only",
                "Python",
                "User: request text \\x1B",
            ]
        );
    }

    #[test]
    fn renders_complete_connectors_at_depths_seven_and_eight() {
        let tree = SessionTree::from_catalogue(&nested_fork_catalogue(8));
        let mut picker = tree.picker();
        let rows = tree_rows(&mut picker);

        assert_eq!(
            &rows[7..9],
            [
                "   │  │  │  │  │  ├⊟ User: depth 7",
                "   │  │  │  │  │  │  ├─ ● User: depth 8",
            ]
        );
        assert!(rows.iter().all(|row| !row.contains('…')));
    }

    #[test]
    fn keeps_nested_parents_before_descendants() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
        ));
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            [
                "Session start",
                "├─ User: leading sibling",
                "└⊟ User: leading ancestor",
                "   User: linear before fork",
                "   ├─ User: outer second",
                "   └⊟ User: outer first",
                "      User: outer continuation",
                "      ├─ User: inner second",
                "      └⊟ User: inner first",
                "         ● User: inner continuation",
            ]
        );
    }

    #[test]
    fn filters_a_4096_entry_chain_to_its_matching_leaf() {
        let tree = SessionTree::from_catalogue(&linear_catalogue(4_096));
        assert_eq!(tree.preorder.len(), 4_096);
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
    fn projects_a_large_mixed_tree_without_quadratic_connector_materialisation() {
        let tree = SessionTree::from_catalogue(&nested_fork_catalogue(2_048));
        assert_eq!(tree.preorder.len(), 4_096);

        let mut positions = vec![0; tree.nodes.len()];
        for (position, &index) in tree.preorder.iter().enumerate() {
            positions[index] = position;
        }
        for (position, &index) in tree.preorder.iter().enumerate() {
            if let Some(parent) = tree.full_topology.parents[index] {
                assert!(positions[parent] < position);
            }
        }

        let items = tree.items(
            &TreeProjection::all(&tree.full_topology),
            PICKER_ROW_WIDTH,
            &BTreeSet::new(),
        );
        assert_eq!(items.len(), tree.preorder.len());
        assert!(
            items
                .iter()
                .all(|item| item.row.width() <= PICKER_ROW_WIDTH)
        );
        assert!(
            items
                .iter()
                .all(|item| !item.row.to_string().contains("… "))
        );
    }

    #[test]
    fn filtering_keeps_full_tree_fork_levels_and_recomputes_endings() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
                entry(3, Some(2), 3, execution(Some("context"), Vec::new())),
                entry(
                    2,
                    Some(1),
                    2,
                    SessionCatalogueKind::Request {
                        text: "branch".to_owned(),
                    },
                ),
            ],
        ));
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
                "User: need elsewhere",
                "User: needle target",
            ]
        );
    }

    #[test]
    fn finds_untruncated_previews_and_preserves_graphemes() {
        let request = "界🦀e\u{0301}".repeat(25) + " needle";
        let tree = SessionTree::from_catalogue(&catalogue(
            2,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(
                    2,
                    Some(1),
                    2,
                    SessionCatalogueKind::Request { text: request },
                ),
            ],
        ));
        let items = tree.items(
            &TreeProjection::all(&tree.full_topology),
            19,
            &BTreeSet::new(),
        );
        assert_eq!(items[1].row.to_string(), "  └─ ● User: 界🦀e\u{0301}…");
        assert_eq!(items[1].row.width(), 19);
        assert_eq!(
            items[1].row.spans.last().expect("ellipsis span").style.fg,
            Some(Color::Rgb(212, 212, 212))
        );

        let mut picker = tree.picker();
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
    fn handles_empty_and_narrow_results() {
        let tree = SessionTree::from_catalogue(&catalogue(
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
        ));
        let mut picker = tree.picker();

        assert_eq!(
            tree_rows(&mut picker),
            ["● Session start", "└─ User: selectable"]
        );
        assert_eq!(
            picker.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            PickerAction::Apply(SessionEntryId(1))
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

    #[test]
    fn builds_rows_at_tiny_widths() {
        let tree = SessionTree::from_catalogue(&catalogue(
            2,
            [
                entry(1, None, 1, SessionCatalogueKind::Start),
                entry(2, Some(1), 2, request("narrow")),
            ],
        ));
        let projection = TreeProjection::all(&tree.full_topology);

        for row_width in [0, 1, 2] {
            let items = tree.items(&projection, row_width, &BTreeSet::new());
            assert!(items.iter().all(|item| item.row.width() <= row_width));
            assert!(items.iter().all(|item| {
                item.focused_row
                    .as_ref()
                    .is_some_and(|row| row.width() <= row_width)
            }));
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

    fn deep_conversation_catalogue(depth: u64) -> SessionCatalogue {
        let mut entries = vec![entry(1, None, 1, SessionCatalogueKind::Start)];
        let mut parent = 1;
        for entry_id in 2..=depth + 1 {
            entries.push(entry(
                entry_id,
                Some(parent),
                entry_id,
                execution(None, Vec::new()),
            ));
            parent = entry_id;
        }
        for entry_id in depth + 2..=depth * 2 + 1 {
            entries.push(entry(
                entry_id,
                Some(parent),
                entry_id,
                SessionCatalogueKind::Final {
                    text: format!("reply {entry_id}"),
                },
            ));
        }
        catalogue(depth * 2 + 1, entries)
    }

    fn request(text: &str) -> SessionCatalogueKind {
        SessionCatalogueKind::Request {
            text: text.to_owned(),
        }
    }

    fn copy_action(kind: SessionCatalogueKind) -> TreeNavigatorAction {
        let active_entry = if matches!(&kind, SessionCatalogueKind::Start) {
            1
        } else {
            2
        };
        let mut entries = vec![entry(1, None, 1, SessionCatalogueKind::Start)];
        if active_entry == 2 {
            entries.push(entry(2, Some(1), 2, kind));
        }
        let mut navigator = TreeNavigator::new(15, "");
        assert!(navigator.install_catalogue(15, &catalogue(active_entry, entries)));
        navigator.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
    }

    fn detailed_execution(
        source: &str,
        commentary: Option<&str>,
        stdout: Observation,
        stderr: Observation,
        outcome: ExecutionOutcome,
        tool_calls: Vec<ToolCall>,
    ) -> SessionCatalogueKind {
        SessionCatalogueKind::Execution {
            source: source.to_owned(),
            commentary: commentary.map(str::to_owned),
            execution: Box::new(Execution {
                stdout,
                stderr,
                outcome,
                tool_calls,
                read_paths: Vec::new(),
                modified_paths: Vec::new(),
                changed_bindings: Vec::new(),
            }),
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

    fn execution(
        commentary: Option<&str>,
        tool_summaries: Vec<ToolCallSummary>,
    ) -> SessionCatalogueKind {
        let observation = Observation {
            head: String::new(),
            tail: None,
            head_lines: 0,
            tail_lines: 0,
            total_lines: 0,
            total_bytes: 0,
            omitted_lines: 0,
            omitted_bytes: 0,
            head_ends_mid_line: false,
            tail_starts_mid_line: false,
            partial_lines: false,
        };
        SessionCatalogueKind::Execution {
            source: String::new(),
            commentary: commentary.map(str::to_owned),
            execution: Box::new(Execution {
                stdout: observation.clone(),
                stderr: observation,
                outcome: ExecutionOutcome::Complete { result: None },
                tool_calls: tool_summaries
                    .into_iter()
                    .map(|summary| ToolCall {
                        tool: ToolName::Read,
                        summary,
                    })
                    .collect(),
                read_paths: Vec::new(),
                modified_paths: Vec::new(),
                changed_bindings: Vec::new(),
            }),
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

    fn row_text(buffer: &Buffer, row: u16) -> String {
        (buffer.area.left()..buffer.area.right())
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    fn tree_rows(picker: &mut Picker<SessionEntryId>) -> Vec<String> {
        let buffer = rendered_buffer(picker, 72, 40);
        buffer
            .content
            .chunks(72)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .map(|row| {
                let row = row
                    .strip_prefix('│')
                    .and_then(|row| row.strip_suffix('│'))
                    .unwrap_or(&row)
                    .trim_end();
                let row = row
                    .strip_prefix("› ")
                    .or_else(|| row.strip_prefix("  "))
                    .unwrap_or(row);
                row.replace("• ", "")
            })
            .filter(|row| {
                [
                    "Session start",
                    "User:",
                    "Python",
                    "Kiki:",
                    "Cancelled",
                    "Failed",
                    "Compaction",
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
        *cell_starts(buffer, text)
            .first()
            .unwrap_or_else(|| panic!("rendered text: {text}"))
    }

    fn cell_starts(buffer: &Buffer, text: &str) -> Vec<(u16, u16)> {
        let symbols = text
            .chars()
            .map(|character| character.to_string())
            .collect::<Vec<_>>();
        let mut starts = Vec::new();
        for row in buffer.area.top()..buffer.area.bottom() {
            for column in buffer.area.left()..buffer.area.right() {
                if symbols.iter().enumerate().all(|(offset, symbol)| {
                    let offset = u16::try_from(offset).expect("test terminal offset");
                    column.saturating_add(offset) < buffer.area.right()
                        && buffer[(column + offset, row)].symbol() == symbol
                }) {
                    starts.push((column, row));
                }
            }
        }
        starts
    }
}
