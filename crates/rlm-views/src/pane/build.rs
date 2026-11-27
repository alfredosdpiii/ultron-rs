use std::collections::{HashMap, HashSet};

use crate::flat;
use crate::graph::{
    GraphKind, GraphNode, GraphProgress, GraphStatus, build_rlm_graph, count, is_finished, rollup, task_status,
};
use crate::visualizer::{RlmSnapshot, RlmTask, RlmWorkflowRun, short_id};

use super::{DagNode, DagRun, RunKind};

fn unescape_json_fragment(raw: &str) -> String {
    let text = raw.replace("\\n", " ");
    let mut out = String::new();
    let mut it = text.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            out.push(it.next().unwrap_or('\\'));
        } else {
            out.push(c);
        }
    }
    out
}

fn read_json_string_prefix(rest: &str) -> String {
    let mut raw = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => {
                let Some(next) = chars.next() else { break };
                raw.push('\\');
                raw.push(next);
            }
            c => raw.push(c),
        }
    }
    unescape_json_fragment(&raw)
}

fn extract_truncated_prompt(input: &str) -> Option<String> {
    for prefix in ["{\"prompt\":\"", "{\"task\":\""] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return Some(flat(&read_json_string_prefix(rest), 200));
        }
    }
    None
}

/// The prompt of a task input preview: the `prompt` (or `task`) of `{"prompt": ...}` (possibly cut), else the preview.
pub fn prompt_of(input: Option<&str>) -> Option<String> {
    let input = input?;
    if input.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::String(s)) => return Some(flat(&s, 200)),
        Ok(serde_json::Value::Object(map)) => {
            let prompt = map.get("prompt").filter(|v| !v.is_null()).or_else(|| map.get("task"));
            if let Some(serde_json::Value::String(p)) = prompt {
                return Some(flat(p, 200));
            }
        }
        Ok(_) => {}
        Err(_) => {
            if let Some(extracted) = extract_truncated_prompt(input) {
                return Some(extracted);
            }
        }
    }
    Some(flat(input, 200))
}

fn progress_of(statuses: &[GraphStatus]) -> (GraphProgress, GraphStatus) {
    let progress = count(statuses, statuses.len());
    let status = if statuses.is_empty() {
        GraphStatus::Pending
    } else {
        rollup(&progress)
    };
    (progress, status)
}

fn wave_depth(
    key: &str,
    by_key: &HashMap<&str, &Vec<String>>,
    level: &mut HashMap<String, usize>,
    visiting: &mut HashSet<String>,
) -> usize {
    if let Some(l) = level.get(key) {
        return *l;
    }
    if visiting.contains(key) {
        return 0;
    }
    visiting.insert(key.to_string());
    let mut value = 0;
    for dep in by_key.get(key).copied().into_iter().flatten() {
        if by_key.contains_key(dep.as_str()) && dep != key {
            value = value.max(wave_depth(dep, by_key, level, visiting) + 1);
        }
    }
    visiting.remove(key);
    level.insert(key.to_string(), value);
    value
}

/// Topological levels of `dependsOn` (unknown dependencies ignored; a cycle is cut where it closes).
pub fn compute_waves(nodes: &[(String, Vec<String>)]) -> Vec<Vec<String>> {
    let by_key: HashMap<&str, &Vec<String>> = nodes.iter().map(|(k, d)| (k.as_str(), d)).collect();
    let mut level: HashMap<String, usize> = HashMap::new();
    let mut visiting: HashSet<String> = HashSet::new();
    let mut waves: Vec<Vec<String>> = Vec::new();
    for (key, _) in nodes {
        let index = wave_depth(key, &by_key, &mut level, &mut visiting);
        while waves.len() <= index {
            waves.push(Vec::new());
        }
        waves[index].push(key.clone());
    }
    waves.into_iter().filter(|w| !w.is_empty()).collect()
}

fn task_fields(task: Option<&RlmTask>, node: &mut DagNode) {
    let Some(task) = task else { return };
    node.prompt = prompt_of(task.input.as_deref());
    node.model = task.model.clone();
    node.text = task.last_text.as_deref().map(|t| flat(t, 200));
    node.turns = task.turns;
    node.tool_calls = task.tool_call_count;
}

fn task_name(task: Option<&RlmTask>, node: &GraphNode) -> String {
    let Some(task) = task else { return node.label.clone() };
    let id = short_id(&task.id);
    match node.kind {
        GraphKind::Child => format!("child {id}"),
        GraphKind::Background => format!("background {id}"),
        _ => {
            let def = match task.definition.rfind('@') {
                Some(at)
                    if task.definition[at + 1..].chars().all(|c| c.is_ascii_digit())
                        && at + 1 < task.definition.len() =>
                {
                    &task.definition[..at]
                }
                _ => task.definition.as_str(),
            };
            format!("{def} {id}")
        }
    }
}

fn strip_nodes_suffix(label: &str) -> String {
    let Some(head) = label.strip_suffix(" nodes") else {
        return label.to_string();
    };
    let Some(sp) = head.rfind(' ') else {
        return label.to_string();
    };
    if sp + 1 < head.len() && head[sp + 1..].chars().all(|c| c.is_ascii_digit()) {
        head[..sp].to_string()
    } else {
        label.to_string()
    }
}

fn from_graph(node: &GraphNode, tasks: &HashMap<String, &RlmTask>, depends_on: Vec<String>, origin: &str) -> DagNode {
    let task = node.key.strip_prefix("task:").and_then(|id| tasks.get(id).copied());
    let mut dag = DagNode {
        key: node.key.clone(),
        kind: Some(node.kind),
        status: Some(node.status),
        depends_on,
        origin: origin.to_string(),
        started_at: node.started_at,
        ended_at: node.ended_at,
        note: node
            .note
            .as_ref()
            .map(|n| n.strip_prefix("→ ").unwrap_or(n).to_string()),
        progress: node.progress,
        ..DagNode::default()
    };
    match node.kind {
        GraphKind::Task | GraphKind::Child | GraphKind::Background => {
            dag.name = task_name(task, node);
            dag.category = task.map_or(node.kind.name().to_string(), |t| t.definition.clone());
            task_fields(task, &mut dag);
        }
        GraphKind::Fanout => {
            dag.name = node.label.clone();
            dag.category = "rlm.map".into();
            dag.prompt = node.note.clone();
        }
        GraphKind::Infer => {
            dag.name = node.label.clone();
            dag.category = "rlm.infer".into();
        }
        GraphKind::Job => {
            dag.name = node.label.clone();
            dag.category = "job".into();
            dag.prompt = Some(node.label.strip_prefix("bash ").unwrap_or(&node.label).to_string());
        }
        GraphKind::Workflow => {
            dag.name = strip_nodes_suffix(&node.label);
            dag.category = "workflows.run".into();
            let n = node.children.len();
            dag.prompt = Some(format!("{n} node{}", if n == 1 { "" } else { "s" }));
        }
        _ => {
            dag.name = node.label.clone();
            dag.category = node.kind.name().to_string();
        }
    }
    dag
}

fn tools_node(key: &str, tools: &[&GraphNode], origin: &str) -> DagNode {
    let statuses: Vec<GraphStatus> = tools.iter().map(|t| t.status).collect();
    let (progress, status) = progress_of(&statuses);
    let mut names: Vec<String> = Vec::new();
    for t in tools {
        let name = t
            .details
            .iter()
            .find(|(l, _)| l == "tool")
            .map_or(t.label.clone(), |(_, v)| v.clone());
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let starts: Vec<f64> = tools.iter().filter_map(|t| t.started_at).collect();
    let ends: Vec<f64> = tools.iter().filter_map(|t| t.ended_at).collect();
    DagNode {
        key: key.to_string(),
        name: format!("{} tool call{}", tools.len(), if tools.len() == 1 { "" } else { "s" }),
        kind: Some(GraphKind::Tools),
        status: Some(status),
        origin: origin.to_string(),
        category: "tools".into(),
        prompt: Some(names.join(", ")),
        progress: Some(progress),
        started_at: starts.iter().copied().reduce(f64::min),
        ended_at: if is_finished(status) {
            ends.iter().copied().reduce(f64::max)
        } else {
            None
        },
        ..DagNode::default()
    }
}

const MAX_NESTED: usize = 3;

struct TurnAccumulator {
    nodes: Vec<DagNode>,
    waves: Vec<Vec<String>>,
    edges: Vec<(String, String)>,
}

impl TurnAccumulator {
    fn next_nested_level<'a>(
        &mut self,
        level: &[(&'a GraphNode, DagNode)],
        tasks: &HashMap<String, &RlmTask>,
        origin: &str,
    ) -> Vec<(&'a GraphNode, DagNode)> {
        let mut next: Vec<(&'a GraphNode, DagNode)> = Vec::new();
        for (parent, parent_dag) in level {
            if parent.kind == GraphKind::Workflow {
                continue;
            }
            for child in &parent.children {
                if child.kind == GraphKind::Tool || child.kind == GraphKind::Earlier {
                    continue;
                }
                let dag = from_graph(child, tasks, vec![parent_dag.key.clone()], origin);
                self.edges.push((parent_dag.key.clone(), dag.key.clone()));
                next.push((child, dag));
            }
        }
        next
    }

    fn add_group(&mut self, children: &[&GraphNode], tasks: &HashMap<String, &RlmTask>, origin: &str, group_key: &str) {
        let tools: Vec<&GraphNode> = children.iter().copied().filter(|c| c.kind == GraphKind::Tool).collect();
        let mut level: Vec<(&GraphNode, DagNode)> = children
            .iter()
            .copied()
            .filter(|c| c.kind != GraphKind::Tool && c.kind != GraphKind::Earlier)
            .map(|n| (n, from_graph(n, tasks, Vec::new(), origin)))
            .collect();
        let mut top: Vec<DagNode> = level.iter().map(|(_, d)| d.clone()).collect();
        if !tools.is_empty() {
            top.push(tools_node(&format!("tools:{group_key}"), &tools, origin));
        }
        if top.is_empty() {
            return;
        }
        self.waves.push(top.iter().map(|n| n.key.clone()).collect());
        self.nodes.extend(top);
        for _ in 0..MAX_NESTED {
            if level.is_empty() {
                break;
            }
            let next = self.next_nested_level(&level, tasks, origin);
            if next.is_empty() {
                break;
            }
            self.nodes.extend(next.iter().map(|(_, d)| d.clone()));
            self.waves.push(next.iter().map(|(_, d)| d.key.clone()).collect());
            level = next;
        }
    }
}

fn turn_run(snapshot: &RlmSnapshot, graph: &GraphNode, tasks: &HashMap<String, &RlmTask>) -> Option<DagRun> {
    let mut acc = TurnAccumulator {
        nodes: Vec::new(),
        waves: Vec::new(),
        edges: Vec::new(),
    };
    let cells: Vec<&GraphNode> = graph.children.iter().filter(|c| c.kind == GraphKind::Cell).collect();
    for (index, cell) in cells.iter().enumerate() {
        let children: Vec<&GraphNode> = cell.children.iter().collect();
        acc.add_group(&children, tasks, &format!("cell {}", index + 1), &cell.key);
    }
    let loose: Vec<&GraphNode> = graph
        .children
        .iter()
        .filter(|c| c.kind != GraphKind::Cell && c.kind != GraphKind::Earlier)
        .collect();
    if !loose.is_empty() {
        acc.add_group(&loose, tasks, "turn", "turn");
    }
    if acc.nodes.is_empty() {
        return None;
    }
    let statuses: Vec<GraphStatus> = acc.nodes.iter().map(DagNode::status).collect();
    let prompt = snapshot
        .turn
        .as_ref()
        .and_then(|t| t.prompt.as_deref())
        .filter(|p| !p.is_empty());
    let id = match snapshot
        .usage
        .as_ref()
        .and_then(|u| u.root_id.as_deref())
        .filter(|r| !r.is_empty())
    {
        Some(r) => short_id(r.strip_prefix("root-").unwrap_or(r)),
        None => "turn".into(),
    };
    Some(DagRun {
        key: "turn".into(),
        kind: RunKind::Turn,
        id,
        title: prompt.map_or("current turn".to_string(), |p| flat(p, 120)),
        nodes: acc.nodes,
        waves: acc.waves,
        edges: acc.edges,
        status: progress_of(&statuses).1,
        started_at: graph.started_at,
    })
}

fn ended_status(status: Option<&str>) -> GraphStatus {
    match status {
        Some("succeeded") => GraphStatus::Done,
        Some("skipped" | "cancelled") => GraphStatus::Cancelled,
        Some("exhausted" | "interrupted") => GraphStatus::Incomplete,
        None => GraphStatus::Pending,
        _ => GraphStatus::Failed,
    }
}

/// A workflow node as planned: (id, definition, dependsOn, status, reason).
type PlannedNode = (String, String, Vec<String>, Option<String>, Option<String>);

struct WorkflowMembers<'a> {
    order: Vec<String>,
    by_run: HashMap<String, HashMap<String, &'a RlmTask>>,
    nodes: HashMap<String, Vec<String>>,
}

fn collect_workflow_members(snapshot: &RlmSnapshot) -> WorkflowMembers<'_> {
    let mut order: Vec<String> = Vec::new();
    let mut by_run: HashMap<String, HashMap<String, &RlmTask>> = HashMap::new();
    let mut nodes: HashMap<String, Vec<String>> = HashMap::new();
    for task in &snapshot.tasks {
        let Some(w) = &task.workflow else { continue };
        if !by_run.contains_key(&w.run) {
            order.push(w.run.clone());
        }
        let run = by_run.entry(w.run.clone()).or_default();
        let node_order = nodes.entry(w.run.clone()).or_default();
        if !node_order.contains(&w.node) {
            node_order.push(w.node.clone());
        }
        run.insert(w.node.clone(), task);
    }
    WorkflowMembers { order, by_run, nodes }
}

fn planned_nodes_for(
    run: &str,
    plan: Option<&&RlmWorkflowRun>,
    tasks: &HashMap<String, &RlmTask>,
    member_nodes: &HashMap<String, Vec<String>>,
) -> Vec<PlannedNode> {
    match plan {
        Some(p) => p
            .nodes
            .iter()
            .map(|n| {
                (
                    n.id.clone(),
                    n.definition.clone(),
                    n.depends_on.clone(),
                    n.status.clone(),
                    n.reason.clone(),
                )
            })
            .collect(),
        None => member_nodes
            .get(run)
            .into_iter()
            .flatten()
            .filter_map(|node| {
                let t = tasks.get(node)?;
                let w = t.workflow.as_ref()?;
                Some((w.node.clone(), t.definition.clone(), w.depends_on.clone(), None, None))
            })
            .collect(),
    }
}

fn workflow_node_note(task: Option<&RlmTask>, ended: Option<&str>, reason: Option<&String>) -> Option<String> {
    if let Some(error) = task.and_then(|t| t.result.as_ref()).and_then(|r| r.error.as_deref()) {
        return Some(flat(error, 160));
    }
    if task.is_none() && ended == Some("skipped") {
        return Some(format!("skipped{}", reason.map_or_default(|r| format!(": {r}"))));
    }
    if task.is_none()
        && let Some(r) = reason
    {
        return Some(flat(r, 160));
    }
    task.and_then(|t| t.result.as_ref())
        .and_then(|r| r.preview.as_deref())
        .map(|p| flat(p, 160))
}

fn build_workflow_node(
    run: &str,
    planned: &PlannedNode,
    ids: &HashSet<&str>,
    tasks: &HashMap<String, &RlmTask>,
    snapshot: &RlmSnapshot,
) -> DagNode {
    let (id, definition, depends_on, ended, reason) = planned;
    let task = tasks.get(id).copied();
    let timing = task.and_then(|t| snapshot.timing.get(&t.id));
    let status = match task {
        Some(t) => task_status(&t.state),
        None => ended_status(ended.as_deref()),
    };
    let mut dag = DagNode {
        key: format!("wf:{run}:{id}"),
        name: id.clone(),
        kind: Some(GraphKind::Task),
        status: Some(status),
        depends_on: depends_on
            .iter()
            .filter(|d| ids.contains(d.as_str()))
            .map(|d| format!("wf:{run}:{d}"))
            .collect(),
        origin: "Start node".into(),
        category: task.map_or(definition.clone(), |t| t.definition.clone()),
        started_at: task.and_then(|t| t.started_at).or(timing.and_then(|t| t.started_at)),
        ended_at: task.and_then(|t| t.ended_at).or(timing.and_then(|t| t.ended_at)),
        ..DagNode::default()
    };
    task_fields(task, &mut dag);
    dag.note = workflow_node_note(task, ended.as_deref(), reason.as_ref());
    dag
}

fn build_workflow_run(
    run: &str,
    plan: Option<&&RlmWorkflowRun>,
    tasks: &HashMap<String, &RlmTask>,
    member_nodes: &HashMap<String, Vec<String>>,
    snapshot: &RlmSnapshot,
    turn_start: Option<f64>,
) -> DagRun {
    let planned = planned_nodes_for(run, plan, tasks, member_nodes);
    let ids: HashSet<&str> = planned.iter().map(|p| p.0.as_str()).collect();
    let nodes: Vec<DagNode> = planned
        .iter()
        .map(|p| build_workflow_node(run, p, &ids, tasks, snapshot))
        .collect();
    let edges: Vec<(String, String)> = nodes
        .iter()
        .flat_map(|n| n.depends_on.iter().map(move |dep| (dep.clone(), n.key.clone())))
        .collect();
    let started = plan
        .and_then(|p| p.started_at)
        .or_else(|| nodes.iter().filter_map(|n| n.started_at).reduce(f64::min));
    let current = matches!((turn_start, started), (Some(t), Some(s)) if s >= t - 1000.0);
    let prompt = snapshot
        .turn
        .as_ref()
        .and_then(|t| t.prompt.as_deref())
        .filter(|p| !p.is_empty());
    let statuses: Vec<GraphStatus> = nodes.iter().map(DagNode::status).collect();
    let wave_input: Vec<(String, Vec<String>)> = nodes.iter().map(|n| (n.key.clone(), n.depends_on.clone())).collect();
    DagRun {
        key: format!("workflow:{run}"),
        kind: RunKind::Workflow,
        id: run.strip_prefix("wf-").unwrap_or(run).to_string(),
        title: match (current, prompt) {
            (true, Some(p)) => flat(p, 120),
            _ => format!("workflow {run}"),
        },
        nodes,
        waves: compute_waves(&wave_input),
        edges,
        status: progress_of(&statuses).1,
        started_at: started,
    }
}

fn workflow_runs(snapshot: &RlmSnapshot) -> Vec<DagRun> {
    let members = collect_workflow_members(snapshot);
    let plans: HashMap<&str, &RlmWorkflowRun> = snapshot.workflows.iter().map(|p| (p.run.as_str(), p)).collect();
    let mut plan_order: Vec<String> = Vec::new();
    for p in &snapshot.workflows {
        if !plan_order.contains(&p.run) {
            plan_order.push(p.run.clone());
        }
    }
    let order: Vec<String> = plan_order
        .into_iter()
        .chain(
            members
                .order
                .iter()
                .filter(|r| !plans.contains_key(r.as_str()))
                .cloned(),
        )
        .collect();
    let turn_start = snapshot.turn.as_ref().and_then(|t| t.started_at);
    let empty = HashMap::new();
    order
        .iter()
        .map(|run| {
            let tasks = members.by_run.get(run).unwrap_or(&empty);
            build_workflow_run(
                run,
                plans.get(run.as_str()),
                tasks,
                &members.nodes,
                snapshot,
                turn_start,
            )
        })
        .collect()
}

/// Every run the pane can show: the current turn (when it started work), then each workflow run, oldest first.
pub fn build_dag_runs(snapshot: &RlmSnapshot) -> Vec<DagRun> {
    let graph = build_rlm_graph(snapshot);
    let tasks: HashMap<String, &RlmTask> = snapshot.tasks.iter().map(|t| (t.id.clone(), t)).collect();
    let mut runs: Vec<DagRun> = turn_run(snapshot, &graph, &tasks).into_iter().collect();
    runs.extend(workflow_runs(snapshot));
    runs
}

pub(super) fn is_active(status: GraphStatus) -> bool {
    matches!(status, GraphStatus::Running | GraphStatus::Pending)
}

/// The run shown by default: the newest run with live work, else the newest run.
pub fn default_run(runs: &[DagRun]) -> Option<&DagRun> {
    runs.iter()
        .rev()
        .find(|r| r.nodes.iter().any(|n| is_active(n.status())))
        .or(runs.last())
}
