use serde_json::Value;
use std::collections::{HashMap, HashSet};

use theme::{Fg, Theme};
use tui::strip_terminal_sequences;

use super::{AgentKind, AgentStatus, AgentView, MAIN_AGENT_ID};

/// The roster rows of `rlm.list_subagents` (`{subagents: [{id, definition, state, parentId?, result?}]}`), by
/// task state (`NativeTaskState`): admitted and running children run, completed ones are idle, failed or cancelled
/// ones aborted, and one left by a worker that died while it ran (`interrupted`) is parked, which the hub can revive.
pub fn agent_views(listed: &Value) -> Vec<AgentView> {
    listed["subagents"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| AgentView {
            id: a["id"].as_str().unwrap_or_default().to_string(),
            kind: AgentKind::Sub,
            parent_id: a["parentId"].as_str().map(str::to_string),
            status: match a["state"].as_str() {
                Some("admitted" | "running") => AgentStatus::Running,
                Some("completed") => AgentStatus::Idle,
                Some("failed" | "cancelled") => AgentStatus::Aborted,
                _ => AgentStatus::Parked,
            },
            unread: 0,
            task: a["result"]["summary"].as_str().map(str::to_string),
            last_activity: 0,
            model: None,
            thinking: None,
            metrics: None,
        })
        .collect()
}

/// The hub's notice for what a steer (`agent_message.send`'s `{state, steered}`) or a revive (`{id}`) of `id` answered.
pub fn agent_notice(id: &str, action: crate::lane::AgentAction, answer: Result<Option<Value>, String>) -> String {
    use crate::lane::AgentAction;
    match (action, answer) {
        (_, Err(error)) => format!(
            "Error: {}",
            error.strip_prefix("session_operation_failed: ").unwrap_or(&error)
        ),
        (AgentAction::Steer, Ok(answer)) if answer.as_ref().is_some_and(|a| a["steered"] == true) => {
            format!("Steered \"{id}\".")
        }
        (AgentAction::Steer, Ok(_)) => format!("\"{id}\" has no live run: the message waits in its inbox."),
        (AgentAction::Revive, Ok(answer)) => {
            let new = answer.as_ref().and_then(|a| a["id"].as_str()).unwrap_or_default();
            format!("Revived \"{id}\" as \"{new}\".")
        }
    }
}

/// `compareRosterAgents`: status, then most recent activity, then id.
pub fn compare_roster_agents(a: &AgentView, b: &AgentView) -> std::cmp::Ordering {
    a.status
        .order()
        .cmp(&b.status.order())
        .then(b.last_activity.cmp(&a.last_activity))
        .then_with(|| a.id.cmp(&b.id))
}

/// The roster ranking captured once (`#rowOrder`), so heartbeats never reorder rows under the cursor; each batch
/// of newcomers is sorted and ranked above every row already on screen.
#[derive(Clone, Debug, Default)]
pub struct RosterOrder {
    rank: Option<HashMap<String, i64>>,
    next_new: i64,
}

impl RosterOrder {
    /// Order `refs` (the main session excluded by the caller). `settled`: persisted agents finished loading.
    pub fn order(&mut self, mut refs: Vec<AgentView>, settled: bool) -> Vec<AgentView> {
        match &mut self.rank {
            None => {
                refs.sort_by(compare_roster_agents);
                if settled && !refs.is_empty() {
                    self.rank = Some(refs.iter().enumerate().map(|(i, r)| (r.id.clone(), i as i64)).collect());
                }
                refs
            }
            Some(rank) => {
                rank.retain(|id, _| refs.iter().any(|r| &r.id == id));
                let mut newcomers: Vec<&AgentView> = refs.iter().filter(|r| !rank.contains_key(&r.id)).collect();
                newcomers.sort_by(|a, b| compare_roster_agents(a, b));
                for r in newcomers.iter().rev() {
                    self.next_new -= 1;
                    rank.insert(r.id.clone(), self.next_new);
                }
                refs.sort_by_key(|r| rank[&r.id]);
                refs
            }
        }
    }

    /// Rank of `id` in the captured order.
    pub fn rank(&self, id: &str) -> Option<i64> {
        self.rank.as_ref().and_then(|r| r.get(id).copied())
    }
}

/// `projectAgentTree`: parent-before-child rows with depth, parent and last-sibling facts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeProjection {
    /// Indices into the input rows.
    pub rows: Vec<usize>,
    pub depth: HashMap<String, usize>,
    pub parent: HashMap<String, String>,
    pub last_sibling: HashMap<String, bool>,
}

/// A roster rank lookup by agent id.
pub type RankFn<'a> = dyn Fn(&str) -> Option<i64> + 'a;

type ParentChildren = (HashMap<String, String>, HashMap<String, Vec<usize>>, Vec<String>);

fn build_parent_children(refs: &[AgentView]) -> ParentChildren {
    let index: HashMap<&str, usize> = refs.iter().enumerate().map(|(i, r)| (r.id.as_str(), i)).collect();
    let mut parent: HashMap<String, String> = HashMap::new();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut child_order: Vec<String> = Vec::new();
    for (i, r) in refs.iter().enumerate() {
        let p = match &r.parent_id {
            Some(p) if p != MAIN_AGENT_ID && index.contains_key(p.as_str()) => p.clone(),
            _ => MAIN_AGENT_ID.to_string(),
        };
        parent.insert(r.id.clone(), p.clone());
        if !children.contains_key(&p) {
            child_order.push(p.clone());
        }
        children.entry(p).or_default().push(i);
    }
    (parent, children, child_order)
}

fn child_order_for_parent(
    cur: usize,
    refs: &[AgentView],
    children: &HashMap<String, Vec<usize>>,
    subtree: &HashMap<usize, usize>,
    rank: Option<&RankFn<'_>>,
) -> usize {
    let rank_of = |i: usize| rank.and_then(|f| f(&refs[i].id));
    let mut order = cur;
    let parent_rank = rank_of(cur);
    for &child in children.get(&refs[cur].id).map_or(&[][..], Vec::as_slice) {
        let child_rank = rank_of(child);
        if let (Some(pr), Some(cr)) = (parent_rank, child_rank)
            && cr < 0
            && cr < pr
        {
            continue;
        }
        order = order.min(subtree.get(&child).copied().unwrap_or(usize::MAX));
    }
    order
}

fn compute_subtree_order(
    refs: &[AgentView],
    children: &HashMap<String, Vec<usize>>,
    rank: Option<&RankFn<'_>>,
) -> HashMap<usize, usize> {
    let mut subtree: HashMap<usize, usize> = HashMap::new();
    let mut visiting: HashSet<usize> = HashSet::new();
    let mut ranked: HashSet<usize> = HashSet::new();
    for start in 0..refs.len() {
        if ranked.contains(&start) {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((cur, expanded)) = stack.pop() {
            if expanded {
                let order = child_order_for_parent(cur, refs, children, &subtree, rank);
                subtree.insert(cur, order);
                visiting.remove(&cur);
                ranked.insert(cur);
                continue;
            }
            if !ranked.contains(&cur) && visiting.insert(cur) {
                stack.push((cur, true));
                push_unvisited_children(cur, refs, children, &ranked, &visiting, &mut stack);
            }
        }
    }
    subtree
}

fn push_unvisited_children(
    cur: usize,
    refs: &[AgentView],
    children: &HashMap<String, Vec<usize>>,
    ranked: &HashSet<usize>,
    visiting: &HashSet<usize>,
    stack: &mut Vec<(usize, bool)>,
) {
    if let Some(desc) = children.get(&refs[cur].id) {
        for &child in desc.iter().rev() {
            if !ranked.contains(&child) && !visiting.contains(&child) {
                stack.push((child, false));
            }
        }
    }
}

fn compute_last_sibling(refs: &[AgentView], children: &HashMap<String, Vec<usize>>) -> HashMap<String, bool> {
    let mut last_sibling = HashMap::new();
    for siblings in children.values() {
        for (k, &i) in siblings.iter().enumerate() {
            last_sibling.insert(refs[i].id.clone(), k == siblings.len() - 1);
        }
    }
    last_sibling
}

fn visit_tree_branch(
    root: usize,
    refs: &[AgentView],
    children: &HashMap<String, Vec<usize>>,
    visited: &mut HashSet<usize>,
    depth: &mut HashMap<String, usize>,
    rows: &mut Vec<usize>,
) {
    let mut stack = vec![(root, 0usize)];
    while let Some((cur, d)) = stack.pop() {
        if !visited.insert(cur) {
            continue;
        }
        depth.insert(refs[cur].id.clone(), d);
        rows.push(cur);
        if let Some(desc) = children.get(&refs[cur].id) {
            for &child in desc.iter().rev() {
                stack.push((child, d + 1));
            }
        }
    }
}

/// `projectAgentTree(refs, rosterRank)`: groups keep the position of their earliest row; a child spawned after its
/// parent was ranked (negative rank below the parent's) cannot move the parent's group; parent cycles stay visible.
pub fn project_agent_tree(refs: &[AgentView], rank: Option<&RankFn<'_>>) -> TreeProjection {
    let (parent, mut children, child_order) = build_parent_children(refs);
    let subtree = compute_subtree_order(refs, &children, rank);
    for p in &child_order {
        if let Some(siblings) = children.get_mut(p) {
            siblings.sort_by_key(|&i| (subtree.get(&i).copied().unwrap_or(usize::MAX), i));
        }
    }
    let last_sibling = compute_last_sibling(refs, &children);
    let mut rows = Vec::new();
    let mut visited: HashSet<usize> = HashSet::new();
    let mut depth = HashMap::new();
    for &root in children.get(MAIN_AGENT_ID).map_or(&[][..], Vec::as_slice) {
        visit_tree_branch(root, refs, &children, &mut visited, &mut depth, &mut rows);
    }
    for i in 0..refs.len() {
        visit_tree_branch(i, refs, &children, &mut visited, &mut depth, &mut rows);
    }
    TreeProjection {
        rows,
        depth,
        parent,
        last_sibling,
    }
}

const TREE_SEGMENT_WIDTH: usize = 4;

fn collect_ancestor_rails(id: &str, tree: &TreeProjection, segments: &mut Vec<&'static str>) {
    let mut ancestry: HashSet<&str> = HashSet::new();
    let mut p = tree.parent.get(id).map(String::as_str);
    while let Some(par) = p {
        if par == MAIN_AGENT_ID || ancestry.contains(par) {
            break;
        }
        let Some(g) = tree.parent.get(par).map(String::as_str).filter(|g| *g != MAIN_AGENT_ID) else {
            break;
        };
        ancestry.insert(par);
        segments.push(if tree.last_sibling.get(par).copied().unwrap_or(false) {
            "    "
        } else {
            "│   "
        });
        p = Some(g);
    }
}

/// `treePrefix`: the bash `tree`-style ancestry of a row (`continuation` rows carry rails only), unstyled.
pub fn tree_prefix(id: &str, max_width: usize, tree: &TreeProjection, continuation: bool) -> String {
    if tree.depth.get(id).copied().unwrap_or(0) == 0 {
        return String::new();
    }
    let last = tree.last_sibling.get(id).copied().unwrap_or(false);
    let mut segments: Vec<&'static str> = vec![match (continuation, last) {
        (true, true) => "    ",
        (true, false) => "│   ",
        (false, true) => "└── ",
        (false, false) => "├── ",
    }];
    collect_ancestor_rails(id, tree, &mut segments);
    let max_segments = (max_width.saturating_sub(2).max(TREE_SEGMENT_WIDTH) / TREE_SEGMENT_WIDTH).max(1);
    let omitted = segments.len().saturating_sub(max_segments);
    let shown: Vec<&str> = segments.iter().take(max_segments).rev().copied().collect();
    let omitted_prefix = match (omitted > 0, continuation) {
        (true, true) => "  ",
        (true, false) => "… ",
        (false, _) => "",
    };
    format!("{omitted_prefix}{}", shown.concat())
}

/// `sanitizeDisplaySingleLine`: escape sequences and control characters removed, tabs expanded, line breaks as
/// spaces.
pub fn sanitize_single_line(text: &str) -> String {
    let stripped = strip_terminal_sequences(text);
    let mut out = String::with_capacity(stripped.len());
    let mut in_break = false;
    for c in stripped.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
            continue;
        }
        in_break = false;
        if c == '\t' {
            out.push_str("   ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// `taskSummary`: the spawn prompt's fixed preamble is not part of the task.
pub fn task_summary(task: &str) -> String {
    let trimmed = task.trim_start();
    let prefix = "complete assignment thoroughly:";
    let rest = if trimmed.len() >= prefix.len()
        && trimmed.is_char_boundary(prefix.len())
        && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        trimmed[prefix.len()..].trim_start()
    } else {
        task
    };
    sanitize_single_line(rest)
}

/// omp `formatAge(seconds)`.
pub fn format_age(seconds: i64) -> String {
    if seconds == 0 {
        return String::new();
    }
    let mins = seconds.div_euclid(60);
    let hours = mins.div_euclid(60);
    let days = hours.div_euclid(24);
    let (weeks, months) = (days.div_euclid(7), days.div_euclid(30));
    if months > 0 {
        format!("{months}mo ago")
    } else if weeks > 0 {
        format!("{weeks}w ago")
    } else if days > 0 {
        format!("{days}d ago")
    } else if hours > 0 {
        format!("{hours}h ago")
    } else if mins > 0 {
        format!("{mins}m ago")
    } else {
        "just now".into()
    }
}

/// omp `formatCost`.
pub fn format_cost(cost: f64) -> String {
    let amount = if cost.is_finite() { cost } else { 0.0 };
    if amount < 0.01 {
        format!("${amount:.4}")
    } else if amount < 1.0 {
        format!("${amount:.3}")
    } else {
        format!("${amount:.2}")
    }
}

/// The rows of a roster window: entries `start..end` around the selection, plus overflow markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RosterWindow {
    pub start: usize,
    pub end: usize,
    /// `… N more` above / below.
    pub top: bool,
    pub bottom: bool,
    /// The selected entry alone overflows the budget and is cut to it.
    pub clipped: bool,
}

fn grow_roster_window(heights: &[usize], budget: usize, start: &mut usize, end: &mut usize, used: &mut usize) {
    let n = heights.len();
    let mut grew = true;
    while grew {
        grew = false;
        if *end < n && *used + heights[*end] <= budget {
            *used += heights[*end];
            *end += 1;
            grew = true;
        }
        if *start > 0 && *used + heights[*start - 1] <= budget {
            *start -= 1;
            *used += heights[*start];
            grew = true;
        }
    }
}

fn trim_roster_window(
    heights: &[usize],
    selected: usize,
    budget: usize,
    start: &mut usize,
    end: &mut usize,
    used: &mut usize,
) {
    let n = heights.len();
    loop {
        let markers = usize::from(*start > 0) + usize::from(*end < n);
        if *used + markers <= budget || *start >= *end {
            break;
        }
        if *end - 1 > selected {
            *end -= 1;
            *used -= heights[*end];
        } else if *start < selected {
            *used -= heights[*start];
            *start += 1;
        } else {
            break;
        }
    }
}

/// `#renderRosterWindow`: grow a window of whole entries around `selected` within `budget` rows (only visible
/// entries render, so a 5,000-agent roster keeps bounded paint cost), then trim the farthest neighbors so both
/// overflow labels fit.
pub fn roster_window(heights: &[usize], selected: usize, budget: usize) -> RosterWindow {
    let n = heights.len();
    let (mut start, mut end) = (selected, selected + 1);
    let mut used = heights[selected];
    if used > budget {
        return RosterWindow {
            start,
            end,
            top: false,
            bottom: false,
            clipped: true,
        };
    }
    grow_roster_window(heights, budget, &mut start, &mut end, &mut used);
    trim_roster_window(heights, selected, budget, &mut start, &mut end, &mut used);
    let top = start > 0 && used < budget;
    let bottom = end < n && used + usize::from(top) < budget;
    RosterWindow {
        start,
        end,
        top,
        bottom,
        clipped: false,
    }
}

/// `getRunningSubagentBadgeAgentIds`: running RLM children, for the status-line badge.
pub fn running_subagent_ids(agents: &[AgentView]) -> Vec<String> {
    agents
        .iter()
        .filter(|a| a.kind == AgentKind::Sub && a.status == AgentStatus::Running)
        .map(|a| a.id.clone())
        .collect()
}

/// The status-line badge: `⟳ 2 agents` while children run, nothing otherwise.
pub fn running_subagent_badge(theme: &Theme, agents: &[AgentView]) -> Option<String> {
    let n = running_subagent_ids(agents).len();
    (n > 0).then(|| theme.fg(Fg::Accent, &format!("⟳ {n} agent{}", if n == 1 { "" } else { "s" })))
}
