//! Pi's session tree selector (`TreeSelectorComponent`, modes/interactive/components/tree-selector.ts): every branch
//! of the session as an indented tree with connectors and gutters, the active path marked, folding, filters, search
//! and labels. It reads Pi's `getTree()` nodes (`{entry, children, label?, labelTimestamp?}`) and returns
//! [`TreeAction`]s; the client navigates, copies or saves labels.

mod format;
mod render;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use jsrt::date;
use serde_json::Value;
use theme::{Fg, Theme};
use tui::Input;
use tui::KeybindingsManager;
use tui::truncate_to_width;
use tui::{Component, Text};

use crate::components::bold;
use format::{copy_text, has_text, searchable_text};
use render::{help_lines, render_label_editor};

type VisibleTree = (HashMap<String, Option<String>>, HashMap<Option<String>, Vec<String>>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl FilterMode {
    /// The `treeFilterMode` setting value.
    pub fn parse(value: &str) -> FilterMode {
        match value {
            "no-tools" => FilterMode::NoTools,
            "user-only" => FilterMode::UserOnly,
            "labeled-only" => FilterMode::LabeledOnly,
            "all" => FilterMode::All,
            _ => FilterMode::Default,
        }
    }
}

const MODES: [FilterMode; 5] = [
    FilterMode::Default,
    FilterMode::NoTools,
    FilterMode::UserOnly,
    FilterMode::LabeledOnly,
    FilterMode::All,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    Selected(String),
    Cancelled,
    /// Copy the selected entry's text (`None`: it has none).
    Copy(Option<String>),
    /// A label was saved (`None` removes it).
    Label(String, Option<String>),
    /// Restore the working tree's files to the selected entry's checkpoint (after a confirmation).
    RestoreFiles(String),
}

#[derive(Clone, Debug)]
pub(super) struct Gutter {
    pub(super) position: usize,
    pub(super) show: bool,
}

/// One session tree node.
pub(super) struct Node {
    pub(super) entry: Value,
    /// The client marked the entry as one with a working-tree checkpoint (Rust-only `checkpoint: true`).
    pub(super) checkpoint: bool,
    pub(super) children: Vec<usize>,
    pub(super) label: Option<String>,
    pub(super) label_timestamp: Option<String>,
}

#[derive(Clone)]
pub(super) struct Flat {
    pub(super) node: usize,
    pub(super) indent: usize,
    pub(super) show_connector: bool,
    pub(super) is_last: bool,
    pub(super) gutters: Vec<Gutter>,
    pub(super) virtual_root_child: bool,
}

pub(super) fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

pub(super) fn id_of(entry: &Value) -> &str {
    s(entry, "id").unwrap_or("")
}

pub(super) fn parent_of(entry: &Value) -> Option<&str> {
    s(entry, "parentId")
}

pub(super) struct TreeList {
    pub(super) theme: Rc<Theme>,
    pub(super) keys: KeybindingsManager,
    pub(super) home: String,
    pub(super) nodes: Vec<Node>,
    pub(super) flat: Vec<Flat>,
    pub(super) filtered: Vec<Flat>,
    pub(super) selected: usize,
    pub(super) leaf: Option<String>,
    pub(super) max_visible: usize,
    pub(super) filter: FilterMode,
    pub(super) query: String,
    pub(super) tool_calls: HashMap<String, (String, Value)>,
    pub(super) multiple_roots: bool,
    pub(super) show_label_timestamps: bool,
    pub(super) active_path: HashSet<String>,
    pub(super) visible_parent: HashMap<String, Option<String>>,
    pub(super) visible_children: HashMap<Option<String>, Vec<String>>,
    pub(super) last_selected: Option<String>,
    pub(super) folded: HashSet<String>,
}

impl TreeList {
    fn flat_by_id(&self) -> HashMap<String, usize> {
        self.flat
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(&self.nodes[f.node].entry).to_string(), i))
            .collect()
    }

    fn entry(&self, f: &Flat) -> &Value {
        &self.nodes[f.node].entry
    }

    fn find_nearest_visible(&self, entry_id: Option<&str>) -> usize {
        if self.filtered.is_empty() {
            return 0;
        }
        let by_id = self.flat_by_id();
        let visible: HashMap<&str, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(self.entry(f)), i))
            .collect();
        let mut current = entry_id.map(str::to_string);
        while let Some(id) = current {
            if let Some(i) = visible.get(id.as_str()) {
                return *i;
            }
            let Some(&at) = by_id.get(&id) else { break };
            current = parent_of(self.entry(&self.flat[at])).map(str::to_string);
        }
        self.filtered.len() - 1
    }

    fn build_active_path(&mut self) {
        self.active_path.clear();
        let by_id = self.flat_by_id();
        let mut current = self.leaf.clone();
        while let Some(id) = current {
            self.active_path.insert(id.clone());
            let Some(&at) = by_id.get(&id) else { break };
            current = parent_of(self.entry(&self.flat[at])).map(str::to_string);
        }
    }

    fn compute_leaf_ancestors(&self, roots: &[usize]) -> HashMap<usize, bool> {
        let mut contains: HashMap<usize, bool> = HashMap::new();
        let mut order = Vec::new();
        let mut stack: Vec<usize> = roots.to_vec();
        while let Some(n) = stack.pop() {
            order.push(n);
            for &c in self.nodes[n].children.iter().rev() {
                stack.push(c);
            }
        }
        for &n in order.iter().rev() {
            let is_leaf = self.leaf.as_deref() == Some(id_of(&self.nodes[n].entry));
            let has_child = self.nodes[n].children.iter().any(|c| contains.get(c) == Some(&true));
            contains.insert(n, is_leaf || has_child);
        }
        contains
    }

    fn record_tool_calls(&mut self, entry: &Value) {
        if entry["type"] != "message" || entry["message"]["role"] != "assistant" {
            return;
        }
        for block in entry["message"]["content"].as_array().into_iter().flatten() {
            if block["type"] == "toolCall" {
                self.tool_calls.insert(
                    s(block, "id").unwrap_or("").to_string(),
                    (s(block, "name").unwrap_or("").to_string(), block["arguments"].clone()),
                );
            }
        }
    }

    fn flatten(&mut self, roots: &[usize]) {
        let mut result = Vec::new();
        self.tool_calls.clear();
        let contains = self.compute_leaf_ancestors(roots);
        let multiple = roots.len() > 1;
        let mut ordered: Vec<usize> = roots.to_vec();
        ordered.sort_by_key(|r| !contains.get(r).copied().unwrap_or(false));
        type Item = (usize, usize, bool, bool, bool, Vec<Gutter>, bool);
        let mut stack: Vec<Item> = Vec::new();
        for (i, &r) in ordered.iter().enumerate().rev() {
            let last = i == ordered.len() - 1;
            stack.push((r, usize::from(multiple), multiple, multiple, last, Vec::new(), multiple));
        }
        while let Some((n, indent, just_branched, show_connector, is_last, gutters, virtual_root)) = stack.pop() {
            let entry = self.nodes[n].entry.clone();
            self.record_tool_calls(&entry);
            result.push(Flat {
                node: n,
                indent,
                show_connector,
                is_last,
                gutters: gutters.clone(),
                virtual_root_child: virtual_root,
            });
            let children = &self.nodes[n].children;
            let multiple_children = children.len() > 1;
            let ordered: Vec<usize> = children
                .iter()
                .filter(|c| contains.get(c) == Some(&true))
                .chain(children.iter().filter(|c| contains.get(c) != Some(&true)))
                .copied()
                .collect();
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };
            let child_gutters = self.child_gutters(indent, show_connector, virtual_root, is_last, &gutters);
            for (i, &c) in ordered.iter().enumerate().rev() {
                let last = i == ordered.len() - 1;
                stack.push((
                    c,
                    child_indent,
                    multiple_children,
                    multiple_children,
                    last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }
        self.flat = result;
    }

    fn child_gutters(
        &self,
        indent: usize,
        show_connector: bool,
        virtual_root: bool,
        is_last: bool,
        gutters: &[Gutter],
    ) -> Vec<Gutter> {
        let displayed = show_connector && !virtual_root;
        let display_indent = if self.multiple_roots {
            indent.saturating_sub(1)
        } else {
            indent
        };
        let mut out = gutters.to_vec();
        if displayed {
            out.push(Gutter {
                position: display_indent.saturating_sub(1),
                show: !is_last,
            });
        }
        out
    }

    fn is_empty_non_leaf_assistant(&self, entry: &Value, kind: &str, role: &str) -> bool {
        if kind != "message" || role != "assistant" {
            return false;
        }
        if self.leaf.as_deref() == Some(id_of(entry)) {
            return false;
        }
        let stop = s(&entry["message"], "stopReason");
        let error_or_aborted = stop.is_some_and(|r| !r.is_empty() && r != "stop" && r != "toolUse");
        !has_text(&entry["message"]["content"]) && !error_or_aborted
    }

    fn matches_filter_mode(&self, node: &Node, kind: &str, role: &str) -> bool {
        let settings = matches!(
            kind,
            "label" | "context_edit" | "custom" | "model_change" | "thinking_level_change" | "session_info"
        );
        match self.filter {
            FilterMode::UserOnly => kind == "message" && role == "user",
            FilterMode::NoTools => !settings && !(kind == "message" && role == "toolResult"),
            FilterMode::LabeledOnly => node.label.is_some(),
            FilterMode::All => true,
            FilterMode::Default => !settings,
        }
    }

    fn matches_entry(&self, node: &Node, tokens: &[&str]) -> bool {
        let entry = &node.entry;
        let kind = s(entry, "type").unwrap_or("");
        if kind == "usage" {
            return false;
        }
        let role = entry["message"]["role"].as_str().unwrap_or("");
        if self.is_empty_non_leaf_assistant(entry, kind, role) {
            return false;
        }
        if !self.matches_filter_mode(node, kind, role) {
            return false;
        }
        if tokens.is_empty() {
            return true;
        }
        let text = searchable_text(node).to_lowercase();
        tokens.iter().all(|t| text.contains(t))
    }

    fn prune_folded(&self, filtered: &mut Vec<Flat>) {
        if self.folded.is_empty() {
            return;
        }
        let mut skip: HashSet<String> = HashSet::new();
        for f in &self.flat {
            let entry = &self.nodes[f.node].entry;
            if let Some(parent) = parent_of(entry)
                && (self.folded.contains(parent) || skip.contains(parent))
            {
                skip.insert(id_of(entry).to_string());
            }
        }
        filtered.retain(|f| !skip.contains(id_of(&self.nodes[f.node].entry)));
    }

    fn apply_filter(&mut self) {
        if !self.filtered.is_empty() {
            self.last_selected = self
                .filtered
                .get(self.selected)
                .map(|f| id_of(&self.nodes[f.node].entry).to_string())
                .or(self.last_selected.take());
        }
        let lower = self.query.to_lowercase();
        let tokens: Vec<&str> = lower.split_whitespace().collect();
        let mut filtered: Vec<Flat> = self
            .flat
            .iter()
            .filter(|f| self.matches_entry(&self.nodes[f.node], &tokens))
            .cloned()
            .collect();
        self.prune_folded(&mut filtered);
        self.filtered = filtered;
        self.recalculate();
        if let Some(last) = self.last_selected.clone() {
            self.selected = self.find_nearest_visible(Some(&last));
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        if !self.filtered.is_empty() {
            self.last_selected = self
                .filtered
                .get(self.selected)
                .map(|f| id_of(&self.nodes[f.node].entry).to_string());
        }
    }

    fn build_visible_tree(&self) -> VisibleTree {
        let visible: HashSet<String> = self
            .filtered
            .iter()
            .map(|f| id_of(&self.nodes[f.node].entry).to_string())
            .collect();
        let by_id = self.flat_by_id();
        let parent_id = |id: &str| -> Option<String> {
            by_id
                .get(id)
                .and_then(|&i| parent_of(&self.nodes[self.flat[i].node].entry))
                .map(str::to_string)
        };
        let mut visible_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut visible_children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        visible_children.insert(None, Vec::new());
        for f in &self.filtered {
            let id = id_of(&self.nodes[f.node].entry).to_string();
            let mut ancestor = parent_id(&id);
            while let Some(a) = &ancestor {
                if visible.contains(a) {
                    break;
                }
                ancestor = parent_id(a);
            }
            visible_parent.insert(id.clone(), ancestor.clone());
            visible_children.entry(ancestor).or_default().push(id);
        }
        (visible_parent, visible_children)
    }

    fn apply_visual_layout(&mut self, roots: &[String], visible_children: &HashMap<Option<String>, Vec<String>>) {
        let index: HashMap<String, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(&self.nodes[f.node].entry).to_string(), i))
            .collect();
        type Item = (String, usize, bool, bool, bool, Vec<Gutter>, bool);
        let multiple = self.multiple_roots;
        let mut stack: Vec<Item> = Vec::new();
        for (i, r) in roots.iter().enumerate().rev() {
            let last = i == roots.len() - 1;
            stack.push((
                r.clone(),
                usize::from(multiple),
                multiple,
                multiple,
                last,
                Vec::new(),
                multiple,
            ));
        }
        while let Some((id, indent, just_branched, show_connector, is_last, gutters, virtual_root)) = stack.pop() {
            let Some(&at) = index.get(&id) else { continue };
            let child_gutters = self.child_gutters(indent, show_connector, virtual_root, is_last, &gutters);
            let f = &mut self.filtered[at];
            f.indent = indent;
            f.show_connector = show_connector;
            f.is_last = is_last;
            f.gutters = gutters;
            f.virtual_root_child = virtual_root;
            let children = visible_children.get(&Some(id)).cloned().unwrap_or_default();
            let multiple_children = children.len() > 1;
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };
            for (i, c) in children.iter().enumerate().rev() {
                let last = i == children.len() - 1;
                stack.push((
                    c.clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }
    }

    /// `recalculateVisualStructure`.
    fn recalculate(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let (visible_parent, visible_children) = self.build_visible_tree();
        let roots = visible_children[&None].clone();
        self.multiple_roots = roots.len() > 1;
        self.apply_visual_layout(&roots, &visible_children);
        self.visible_parent = visible_parent;
        self.visible_children = visible_children;
    }

    pub(super) fn status_labels(&self) -> String {
        let mut labels = match self.filter {
            FilterMode::NoTools => " [no-tools]",
            FilterMode::UserOnly => " [user]",
            FilterMode::LabeledOnly => " [labeled]",
            FilterMode::All => " [all]",
            FilterMode::Default => "",
        }
        .to_string();
        if self.show_label_timestamps {
            labels += " [+label time]";
        }
        labels
    }

    pub(super) fn is_foldable(&self, id: &str) -> bool {
        let Some(children) = self.visible_children.get(&Some(id.to_string())) else {
            return false;
        };
        if children.is_empty() {
            return false;
        }
        match self.visible_parent.get(id).cloned().flatten() {
            None => true,
            Some(parent) => self.visible_children.get(&Some(parent)).is_some_and(|s| s.len() > 1),
        }
    }

    fn branch_segment_start(&self, up: bool) -> usize {
        let Some(selected) = self.filtered.get(self.selected) else {
            return self.selected;
        };
        let index: HashMap<String, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(&self.nodes[f.node].entry).to_string(), i))
            .collect();
        let mut current = id_of(&self.nodes[selected.node].entry).to_string();
        if !up {
            loop {
                let children = self
                    .visible_children
                    .get(&Some(current.clone()))
                    .cloned()
                    .unwrap_or_default();
                match children.len() {
                    0 => return index[&current],
                    1 => current = children[0].clone(),
                    _ => return index[&children[0]],
                }
            }
        }
        loop {
            let Some(parent) = self.visible_parent.get(&current).cloned().flatten() else {
                return index[&current];
            };
            let siblings = self
                .visible_children
                .get(&Some(parent.clone()))
                .cloned()
                .unwrap_or_default();
            if siblings.len() > 1 {
                let start = index[&current];
                if start < self.selected {
                    return start;
                }
            }
            current = parent;
        }
    }

    fn set_filter(&mut self, mode: FilterMode) {
        self.filter = mode;
        self.folded.clear();
        self.apply_filter();
    }

    fn selected_entry_id(&self) -> Option<String> {
        self.filtered
            .get(self.selected)
            .map(|f| id_of(&self.nodes[f.node].entry).to_string())
    }

    fn fold_or_up(&mut self) {
        match self.selected_entry_id() {
            Some(id) if self.is_foldable(&id) && !self.folded.contains(&id) => {
                self.folded.insert(id);
                self.apply_filter();
            }
            _ => self.selected = self.branch_segment_start(true),
        }
    }

    fn unfold_or_down(&mut self) {
        match self.selected_entry_id() {
            Some(id) if self.folded.contains(&id) => {
                self.folded.remove(&id);
                self.apply_filter();
            }
            _ => self.selected = self.branch_segment_start(false),
        }
    }

    fn step_selection(&mut self, up: bool) {
        let n = self.filtered.len();
        self.selected = if up {
            if self.selected == 0 {
                n.saturating_sub(1)
            } else {
                self.selected - 1
            }
        } else if self.selected + 1 >= n {
            0
        } else {
            self.selected + 1
        };
    }

    fn handle_nav_input(&mut self, data: &str) -> bool {
        let k = |id: &str| self.keys.matches(data, id);
        let n = self.filtered.len();
        if k("tui.select.up") {
            self.step_selection(true);
        } else if k("tui.select.down") {
            self.step_selection(false);
        } else if k("app.tree.foldOrUp") {
            self.fold_or_up();
        } else if k("app.tree.unfoldOrDown") {
            self.unfold_or_down();
        } else if k("tui.editor.cursorLeft") || k("tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(self.max_visible);
        } else if k("tui.editor.cursorRight") || k("tui.select.pageDown") {
            self.selected = (self.selected + self.max_visible).min(n.saturating_sub(1));
        } else {
            return false;
        }
        true
    }

    fn handle_filter_input(&mut self, data: &str) -> bool {
        let k = |id: &str| self.keys.matches(data, id);
        if k("app.tree.filter.default") {
            self.set_filter(FilterMode::Default);
        } else if k("app.tree.filter.noTools") {
            self.toggle(FilterMode::NoTools);
        } else if k("app.tree.filter.userOnly") {
            self.toggle(FilterMode::UserOnly);
        } else if k("app.tree.filter.labeledOnly") {
            self.toggle(FilterMode::LabeledOnly);
        } else if k("app.tree.filter.all") {
            self.toggle(FilterMode::All);
        } else if k("app.tree.filter.cycleBackward") {
            let i = MODES.iter().position(|m| *m == self.filter).unwrap_or(0);
            self.set_filter(MODES[(i + MODES.len() - 1) % MODES.len()]);
        } else if k("app.tree.filter.cycleForward") {
            let i = MODES.iter().position(|m| *m == self.filter).unwrap_or(0);
            self.set_filter(MODES[(i + 1) % MODES.len()]);
        } else {
            return false;
        }
        true
    }

    fn handle_query_input(&mut self, data: &str) {
        if self.keys.matches(data, "tui.editor.deleteCharBackward") {
            if self.query.pop().is_some() {
                self.folded.clear();
                self.apply_filter();
            }
        } else if self.keys.matches(data, "app.tree.toggleLabelTimestamp") {
            self.show_label_timestamps = !self.show_label_timestamps;
        } else if is_printable_query(data) {
            self.query.push_str(data);
            self.folded.clear();
            self.apply_filter();
        }
    }

    fn handle_input(&mut self, data: &str) -> Option<TreeAction> {
        if self.keys.matches(data, "tui.select.confirm") {
            return self.selected_entry_id().map(TreeAction::Selected);
        }
        if self.keys.matches(data, "app.message.copy") {
            return Some(TreeAction::Copy(
                self.filtered
                    .get(self.selected)
                    .and_then(|f| copy_text(&self.nodes[f.node])),
            ));
        }
        if self.keys.matches(data, "tui.select.cancel") {
            if self.query.is_empty() {
                return Some(TreeAction::Cancelled);
            }
            self.query.clear();
            self.folded.clear();
            self.apply_filter();
            return None;
        }
        if !self.handle_nav_input(data) && !self.handle_filter_input(data) {
            self.handle_query_input(data);
        }
        None
    }

    fn toggle(&mut self, mode: FilterMode) {
        self.set_filter(if self.filter == mode { FilterMode::Default } else { mode });
    }
}

fn is_printable_query(data: &str) -> bool {
    !data.is_empty()
        && !data
            .chars()
            .any(|c| (c as u32) < 32 || c as u32 == 0x7f || (0x80..=0x9f).contains(&(c as u32)))
}

/// `TreeSelectorComponent`.
pub struct TreeSelector {
    theme: Rc<Theme>,
    list: TreeList,
    /// `(entry id, input)` while a label is edited.
    label_input: Option<(String, Input)>,
}

impl TreeSelector {
    /// `tree`: Pi's `getTree()` nodes; the current leaf and the entry to select first; `rows`: the terminal height.
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        tree: &Value,
        (leaf, initial): (Option<&str>, Option<&str>),
        rows: usize,
        filter: FilterMode,
        home: &str,
    ) -> Self {
        let mut nodes = Vec::new();
        fn add(nodes: &mut Vec<Node>, value: &Value) -> usize {
            let index = nodes.len();
            nodes.push(Node {
                entry: value["entry"].clone(),
                checkpoint: value["checkpoint"] == true,
                children: Vec::new(),
                label: value["label"].as_str().map(str::to_string),
                label_timestamp: value["labelTimestamp"].as_str().map(str::to_string),
            });
            let children: Vec<usize> = value["children"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| add(nodes, c))
                .collect();
            nodes[index].children = children;
            index
        }
        let roots: Vec<usize> = tree
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| add(&mut nodes, r))
            .collect();
        let mut list = TreeList {
            theme: Rc::clone(&theme),
            keys,
            home: home.to_string(),
            nodes,
            flat: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            leaf: leaf.map(str::to_string),
            max_visible: (rows / 2).max(5),
            filter,
            query: String::new(),
            tool_calls: HashMap::new(),
            multiple_roots: roots.len() > 1,
            show_label_timestamps: false,
            active_path: HashSet::new(),
            visible_parent: HashMap::new(),
            visible_children: HashMap::new(),
            last_selected: None,
            folded: HashSet::new(),
        };
        list.flatten(&roots);
        list.build_active_path();
        list.apply_filter();
        let target = initial.map(str::to_string).or(list.leaf.clone());
        list.selected = list.find_nearest_visible(target.as_deref());
        list.last_selected = list.selected_entry_id();
        TreeSelector {
            theme,
            list,
            label_input: None,
        }
    }

    fn handle_label_input(&mut self, data: &str) -> Option<TreeAction> {
        let keys = &self.list.keys;
        let (id, input) = self.label_input.as_mut()?;
        if keys.matches(data, "tui.select.confirm") {
            let value = input.value().trim().to_string();
            let label = (!value.is_empty()).then_some(value);
            let id = id.clone();
            for node in &mut self.list.nodes {
                if id_of(&node.entry) == id {
                    node.label_timestamp = label.as_ref().map(|_| date::iso_string(date::now_ms()));
                    node.label = label.clone();
                }
            }
            self.label_input = None;
            return Some(TreeAction::Label(id, label));
        }
        if keys.matches(data, "tui.select.cancel") {
            self.label_input = None;
            return None;
        }
        input.handle_input(data);
        None
    }

    pub fn handle_input(&mut self, data: &str) -> Option<TreeAction> {
        if self.label_input.is_some() {
            return self.handle_label_input(data);
        }
        if self.list.keys.matches(data, "app.tree.restoreFiles") {
            return self.list.selected_entry_id().map(TreeAction::RestoreFiles);
        }
        if self.list.keys.matches(data, "app.tree.editLabel") {
            if let Some(f) = self.list.filtered.get(self.list.selected) {
                let node = &self.list.nodes[f.node];
                let mut input = Input::new(self.list.keys.clone());
                if let Some(label) = &node.label {
                    input.set_value(label);
                }
                self.label_input = Some((id_of(&node.entry).to_string(), input));
            }
            return None;
        }
        self.list.handle_input(data)
    }
}

impl Component for TreeSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = t.fg(Fg::Border, &"─".repeat(width.max(1)));
        let mut lines = vec![String::new(), border.clone()];
        lines.extend(Text::new(&bold("  Session Tree"), 1, 0, None).render(width));
        let checkpoints = self.list.nodes.iter().any(|n| n.checkpoint);
        lines.extend(help_lines(&t, &self.list.keys, width, checkpoints));
        let query = &self.list.query;
        let search = if query.is_empty() {
            format!("  {}", t.fg(Fg::Muted, "Type to search:"))
        } else {
            format!("  {} {}", t.fg(Fg::Muted, "Type to search:"), t.fg(Fg::Accent, query))
        };
        lines.push(truncate_to_width(&search, width, "...", false));
        lines.push(border.clone());
        lines.push(String::new());
        match self.label_input.as_mut() {
            None => lines.extend(self.list.render(width)),
            Some((_, input)) => lines.extend(render_label_editor(&t, &self.list.keys, input, width)),
        }
        lines.push(String::new());
        lines.push(border);
        lines
    }
}
