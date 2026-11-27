use std::collections::{HashMap, HashSet};

use jsrt::{number, utf16};
use serde_json::Value;

use crate::flat;
use crate::visualizer::{CellStatus, RlmFrame, RlmJob, RlmRootCell, RlmSnapshot, RlmTask, RlmToolCall, Tri, short_id};

use super::{GraphKind, GraphNode, GraphProgress, GraphStatus, count, d, is_finished, rollup, task_status};

fn frame_status(frame: &RlmFrame) -> GraphStatus {
    match frame.status.as_str() {
        "running" => {
            if frame.task_id == Tri::Null {
                GraphStatus::Pending
            } else {
                GraphStatus::Running
            }
        }
        "complete" => GraphStatus::Done,
        "incomplete" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

fn job_status(job: &RlmJob) -> GraphStatus {
    match job.status.as_str() {
        "running" => GraphStatus::Running,
        "completed" => {
            if job.exit_code == Some(0.0) {
                GraphStatus::Done
            } else {
                GraphStatus::Failed
            }
        }
        "cancelled" => GraphStatus::Cancelled,
        "interrupted" | "timed_out" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

fn first_code_line(code: &str) -> String {
    code.split('\n')
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
        .to_string()
}

pub(super) fn min_defined(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().fold(None, |m, v| Some(m.map_or(v, |m: f64| m.min(v))))
}

pub(super) fn max_defined(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().fold(None, |m, v| Some(m.map_or(v, |m: f64| m.max(v))))
}

pub(super) fn sum(values: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    values.flatten().fold(None, |s, v| Some(s.unwrap_or(0.0) + v))
}

fn non_empty(v: &Option<String>) -> Option<&str> {
    v.as_deref().filter(|s| !s.is_empty())
}

fn task_kind(task: &RlmTask) -> GraphKind {
    match task.definition.as_str() {
        "rlm-child@1" => GraphKind::Child,
        "background-job@1" => GraphKind::Background,
        _ => GraphKind::Task,
    }
}

fn task_label(task: &RlmTask) -> String {
    let id = short_id(&task.id);
    match task_kind(task) {
        GraphKind::Child => format!("rlm.spawn child {id}"),
        GraphKind::Background => format!("background {id}"),
        _ => format!(
            "{} {} {id}",
            task.workflow.as_ref().map_or("agent", |w| w.node.as_str()),
            task.definition
        ),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn task_note(task: &RlmTask) -> Option<String> {
    let result = task.result.as_ref()?;
    if let Some(e) = non_empty(&result.error) {
        return Some(flat(e, 120));
    }
    if let Some(p) = non_empty(&result.preview) {
        return Some(format!("→ {}", flat(p, 120)));
    }
    match &result.value {
        None | Some(Value::Null) => None,
        Some(v) => Some(format!("→ {}", flat(&value_text(v), 120))),
    }
}

fn fetch_hint(task: &RlmTask) -> String {
    if let Some(f) = non_empty(&task.fetch) {
        return f.to_string();
    }
    match task.definition.as_str() {
        "rlm-child@1" => format!("await rlm.collect([\"{}\"])", task.id),
        "background-job@1" => format!("await background.result(\"{}\")", task.id),
        _ => format!("await agents.result(\"{}\")", task.id),
    }
}

fn task_result_detail(task: &RlmTask) -> Option<(String, String)> {
    let r = task.result.as_ref()?;
    if let Some(e) = non_empty(&r.error) {
        return Some(d("error", flat(e, 400)));
    }
    if let Some(p) = non_empty(&r.preview) {
        return Some(d("result", p));
    }
    r.value
        .as_ref()
        .filter(|v| !v.is_null())
        .map(|v| d("result", flat(&value_text(v), 400)))
}

fn task_details(task: &RlmTask, snapshot: &RlmSnapshot) -> Vec<(String, String)> {
    let mut details = vec![
        d("id", &task.id),
        d("definition", &task.definition),
        d("state", &task.state),
    ];
    for (label, value) in [
        ("lane", &task.lane),
        ("parent", &task.parent_id),
        ("worktree", &task.worktree),
    ] {
        if let Some(v) = non_empty(value) {
            details.push(d(label, v));
        }
    }
    if let Some(w) = &task.workflow {
        let after = if w.depends_on.is_empty() {
            String::new()
        } else {
            format!(" after {} ({})", w.depends_on.join(", "), w.join)
        };
        details.push(d("workflow", format!("{} node {}{after}", w.run, w.node)));
    }
    if let Some(i) = non_empty(&task.input) {
        details.push(d("input", i));
    }
    details.extend(task_result_detail(task));
    if snapshot.retained.contains(&task.id) {
        details.push(d("retained", "open instance keeps this lane's kernel"));
    }
    if let Some(p) = snapshot.progress.get(&task.id) {
        details.push(d("progress", format!("{} · {} receipts", p.classification, p.receipts)));
    }
    details.push(d("fetch", fetch_hint(task)));
    details
}

fn task_node(task: &RlmTask, snapshot: &RlmSnapshot) -> GraphNode {
    let timing = snapshot.timing.get(&task.id);
    let mut node = GraphNode::new(
        format!("task:{}", task.id),
        task_kind(task),
        task_label(task),
        task_status(&task.state),
    );
    node.started_at = task.started_at.or(timing.and_then(|t| t.started_at));
    node.ended_at = task.ended_at.or(timing.and_then(|t| t.ended_at));
    node.tokens = task.tokens;
    node.cost = task.cost;
    node.note = task_note(task);
    if let Some(w) = task.workflow.as_ref().filter(|w| !w.depends_on.is_empty()) {
        let suffix = if w.depends_on.len() > 1 {
            format!(" ({})", w.join)
        } else {
            String::new()
        };
        node.join = Some(format!("⇐ {}{suffix}", w.depends_on.join("+")));
    }
    node.details = task_details(task, snapshot);
    node
}

fn frame_budget_detail(first: &RlmFrame, calls: f64, tokens: f64) -> (String, String) {
    match &first.budget {
        Some(b) => {
            let id = b.id.strip_prefix("budget-").unwrap_or(&b.id);
            d(
                "budget",
                format!(
                    "{} · calls {}/{} · tokens {}/{} · depth {}",
                    utf16::head_lossy(id, 8),
                    number::to_string(calls),
                    b.calls.map_or("∞".to_string(), number::to_string),
                    number::to_string(tokens),
                    b.tokens.map_or("∞".to_string(), number::to_string),
                    number::to_string(b.depth)
                ),
            )
        }
        None => d(
            "spent",
            format!(
                "{} calls · {} tokens",
                number::to_string(calls),
                number::to_string(tokens)
            ),
        ),
    }
}

fn frame_group_details(
    first: &RlmFrame,
    frames: &[&RlmFrame],
    batch: usize,
    is_map: bool,
    progress: &GraphProgress,
    calls: f64,
    tokens: f64,
) -> Vec<(String, String)> {
    let mut details = Vec::new();
    if is_map {
        details.push(d(
            "frames",
            format!(
                "{batch} ({} done · {} running · {} queued · {} incomplete · {} failed)",
                progress.done, progress.running, progress.pending, progress.incomplete, progress.failed
            ),
        ));
    }
    details.push(d("task", flat(&first.task, 300)));
    details.push(frame_budget_detail(first, calls, tokens));
    let troubled: Vec<&&RlmFrame> = frames
        .iter()
        .filter(|f| f.status == "incomplete" || f.status == "error")
        .take(3)
        .collect();
    for f in &troubled {
        let reason = f.reason.as_ref().map_or_default(|r| format!(" ({r})"));
        details.push(d(&f.status, format!("{}{reason}", f.id)));
    }
    if is_map {
        details.push(d("traces", "await rlm.frames(limit=200)"));
        if let Some(t) = troubled.first() {
            details.push(d("fetch", format!("await rlm.frames(\"{}\")", t.id)));
        }
    } else {
        details.push(d("trace", &first.id));
        if let Some(r) = non_empty(&first.reason) {
            details.push(d("reason", r));
        }
        details.push(d("fetch", format!("await rlm.frames(\"{}\")", first.id)));
    }
    details
}

fn unique_frame_reasons(frames: &[&RlmFrame]) -> Vec<String> {
    let mut reasons: Vec<String> = Vec::new();
    for r in frames.iter().filter_map(|f| f.reason.clone()) {
        if !reasons.contains(&r) {
            reasons.push(r);
        }
    }
    reasons
}

fn frame_group_node(key: &str, frames: &[&RlmFrame]) -> GraphNode {
    let first = frames[0];
    let batch = (first.batch.unwrap_or(frames.len() as f64) as usize).max(frames.len());
    let is_map = first.kind.as_deref() == Some("map") || batch > 1;
    let statuses: Vec<GraphStatus> = frames.iter().map(|f| frame_status(f)).collect();
    let progress = count(&statuses, batch);
    let status = rollup(&progress);
    let calls = sum(frames.iter().map(|f| Some(f.spent_calls))).unwrap_or(0.0);
    let tokens = sum(frames.iter().map(|f| Some(f.spent_tokens))).unwrap_or(0.0);
    let label = if is_map {
        format!("rlm.map {batch} frame{}", if batch == 1 { "" } else { "s" })
    } else {
        format!(
            "rlm.infer {}",
            utf16::head_lossy(first.id.strip_prefix("frame-").unwrap_or(&first.id), 8)
        )
    };
    let mut node = GraphNode::new(
        format!("frames:{key}"),
        if is_map { GraphKind::Fanout } else { GraphKind::Infer },
        label,
        status,
    );
    node.started_at = min_defined(frames.iter().map(|f| f.started_at));
    node.ended_at = is_finished(status)
        .then(|| max_defined(frames.iter().map(|f| f.ended_at)))
        .flatten();
    node.tokens = Some(tokens);
    if is_map {
        node.progress = Some(progress);
        node.note = Some(flat(&first.task, 80));
    } else {
        let reasons = unique_frame_reasons(frames);
        node.note = Some(if reasons.is_empty() {
            flat(&first.task, 80)
        } else {
            reasons.join(", ")
        });
        node.trace_id = Some(first.id.clone());
    }
    node.details = frame_group_details(first, frames, batch, is_map, &progress, calls, tokens);
    node
}

fn job_details(job: &RlmJob, exit: &str) -> Vec<(String, String)> {
    let mut details = vec![
        d("id", &job.id),
        d("command", flat(&job.command, 400)),
        d(
            "status",
            if job.status == "completed" {
                format!("exit {exit}")
            } else {
                job.status.clone()
            },
        ),
    ];
    if let Some(l) = non_empty(&job.lane) {
        details.push(d("lane", l));
    }
    if let Some(t) = non_empty(&job.tail) {
        details.push(d("output", flat(t, 300)));
    }
    details.push(d("fetch", format!("await (await rlm.job(\"{}\")).read()", job.id)));
    details
}

fn job_node(job: &RlmJob, now: f64) -> GraphNode {
    let status = job_status(job);
    let exit = job.exit_code.map_or("null".to_string(), number::to_string);
    let mut node = GraphNode::new(
        format!("job:{}", job.id),
        GraphKind::Job,
        format!("bash {}", flat(&job.command, 60)),
        status,
    );
    node.started_at = job.started_at;
    node.ended_at = match job.ended_at {
        Some(e) => Some(e),
        None if status == GraphStatus::Running => None,
        None => Some(now),
    };
    node.note = if job.status == "completed" && job.exit_code != Some(0.0) {
        Some(format!("exit {exit}"))
    } else if job.status != "completed" && job.status != "running" {
        Some(job.status.clone())
    } else {
        None
    };
    node.details = job_details(job, &exit);
    node
}

fn tool_call_status(call: &RlmToolCall) -> GraphStatus {
    match call.status.as_str() {
        "running" => GraphStatus::Running,
        "completed" => GraphStatus::Done,
        "cancelled" => GraphStatus::Cancelled,
        "interrupted" => GraphStatus::Incomplete,
        _ => GraphStatus::Failed,
    }
}

fn tool_call_details(call: &RlmToolCall, status: GraphStatus) -> Vec<(String, String)> {
    let via = if call.native {
        "model tool call"
    } else {
        "rlm cell (Python)"
    };
    let mut details = vec![
        d("id", &call.id),
        d("tool", &call.name),
        d("via", via),
        d("status", &call.status),
    ];
    if let Some(l) = non_empty(&call.lane) {
        details.push(d("lane", l));
    }
    if let Some(i) = non_empty(&call.input) {
        details.push(d("input", flat(i, 400)));
    }
    if let Some(e) = non_empty(&call.error) {
        details.push(d("error", flat(e, 400)));
    } else if let Some(p) = non_empty(&call.preview) {
        let label = if status == GraphStatus::Running {
            "progress"
        } else {
            "result"
        };
        details.push(d(label, flat(p, 400)));
    }
    if !call.native {
        details.push(d("fetch", format!("await tools.result(\"{}\")", call.id)));
    }
    details
}

fn tool_call_node(call: &RlmToolCall, now: f64) -> GraphNode {
    let status = tool_call_status(call);
    let note = match (&call.error, non_empty(&call.preview)) {
        (Some(e), _) => Some(flat(e, 100)),
        (None, Some(p)) if status == GraphStatus::Done => Some(format!("→ {}", flat(p, 100))),
        _ => None,
    };
    let mut node = GraphNode::new(
        format!("tool:{}", call.id),
        GraphKind::Tool,
        flat(&call.label, 80),
        status,
    );
    node.started_at = call.started_at;
    node.ended_at = match call.ended_at {
        Some(e) => Some(e),
        None if status == GraphStatus::Running => None,
        None => Some(now),
    };
    node.note = note;
    node.details = tool_call_details(call, status);
    node
}

fn cell_node(cell: &RlmRootCell, index: usize) -> GraphNode {
    let status = match cell.status {
        CellStatus::Running => GraphStatus::Running,
        CellStatus::Ok => GraphStatus::Done,
        CellStatus::Error => GraphStatus::Failed,
    };
    let code: Vec<String> = cell
        .code
        .split('\n')
        .map(|l| l.replace('\t', "  ").trim_end().to_string())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let mut details = vec![d("call", &cell.tool_call_id)];
    for l in code.iter().take(6) {
        details.push(d("code", l));
    }
    if code.len() > 6 {
        details.push(d("code", format!("… {} more lines", code.len() - 6)));
    }
    if let Some(o) = non_empty(&cell.output) {
        let label = if cell.status == CellStatus::Error {
            "error"
        } else {
            "output"
        };
        details.push(d(label, flat(o, 400)));
    }
    let mut node = GraphNode::new(
        format!("cell:{}", cell.tool_call_id),
        GraphKind::Cell,
        format!("cell {}  {}", index + 1, flat(&first_code_line(&cell.code), 100)),
        status,
    );
    node.started_at = cell.started_at;
    node.ended_at = cell.ended_at;
    if cell.status == CellStatus::Error
        && let Some(o) = non_empty(&cell.output)
    {
        node.note = Some(flat(o.split('\n').next_back().unwrap_or(""), 100));
    }
    node.details = details;
    node
}

fn sort_chronological(nodes: &mut [GraphNode]) {
    let key = |n: &GraphNode| n.started_at.unwrap_or(9_007_199_254_740_991.0);
    nodes.sort_by(|a, b| key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal));
    for n in nodes {
        sort_chronological(&mut n.children);
    }
}

struct GraphArena {
    nodes: Vec<GraphNode>,
    kids: Vec<Vec<usize>>,
    items: Vec<(usize, Option<String>, Option<f64>, bool)>,
    frame_group_of: HashMap<String, usize>,
    task_nodes: HashMap<String, usize>,
}

impl GraphArena {
    fn new() -> Self {
        GraphArena {
            nodes: Vec::new(),
            kids: Vec::new(),
            items: Vec::new(),
            frame_group_of: HashMap::new(),
            task_nodes: HashMap::new(),
        }
    }

    fn add(&mut self, node: GraphNode) -> usize {
        self.nodes.push(node);
        self.kids.push(Vec::new());
        self.nodes.len() - 1
    }

    fn add_frame_groups(&mut self, snapshot: &RlmSnapshot) {
        let mut group_order: Vec<String> = Vec::new();
        let mut groups: HashMap<String, Vec<&RlmFrame>> = HashMap::new();
        for f in &snapshot.frames {
            let key = f.budget.as_ref().map_or(f.id.clone(), |b| b.id.clone());
            if !groups.contains_key(&key) {
                group_order.push(key.clone());
            }
            groups.entry(key).or_default().push(f);
        }
        for key in &group_order {
            let frames = &groups[key];
            let node = frame_group_node(key, frames);
            let at = node.started_at;
            let index = self.add(node);
            for f in frames {
                if let Some(id) = f.task_id.as_str() {
                    self.frame_group_of.insert(id.to_string(), index);
                }
            }
            let caller = frames[0]
                .caller_task_id
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            self.items.push((index, caller, at, false));
        }
    }

    fn collect_tasks_and_orphans<'a>(&mut self, snapshot: &'a RlmSnapshot) -> Vec<&'a RlmTask> {
        let frame_task_ids: HashSet<&str> = snapshot.frames.iter().filter_map(|f| f.task_id.as_str()).collect();
        let mut orphan_order: Vec<String> = Vec::new();
        let mut orphans: HashMap<String, Vec<&RlmTask>> = HashMap::new();
        let mut tasks: Vec<&RlmTask> = Vec::new();
        for t in &snapshot.tasks {
            if t.definition == "rlm-frame@1" {
                if !frame_task_ids.contains(t.id.as_str()) {
                    let parent = t.parent_id.clone().unwrap_or_default();
                    if !orphans.contains_key(&parent) {
                        orphan_order.push(parent.clone());
                    }
                    orphans.entry(parent).or_default().push(t);
                }
                continue;
            }
            tasks.push(t);
        }
        for parent in &orphan_order {
            let list = &orphans[parent];
            let statuses: Vec<GraphStatus> = list.iter().map(|t| task_status(&t.state)).collect();
            let progress = count(&statuses, statuses.len());
            let started = min_defined(
                list.iter()
                    .map(|t| t.started_at.or(snapshot.timing.get(&t.id).and_then(|x| x.started_at))),
            );
            let mut node = GraphNode::new(
                format!(
                    "frametasks:{}",
                    if parent.is_empty() { "root" } else { parent.as_str() }
                ),
                GraphKind::Fanout,
                format!("frames {}", list.len()),
                rollup(&progress),
            );
            node.started_at = started;
            node.progress = Some(progress);
            node.details = vec![
                d("frames", format!("{} rlm-frame tasks", list.len())),
                d("traces", "await rlm.frames()"),
            ];
            let index = self.add(node);
            self.items
                .push((index, (!parent.is_empty()).then(|| parent.clone()), started, false));
        }
        tasks
    }

    fn add_workflow_group(&mut self, run: &str, group_tasks: &[&RlmTask], group_nodes: &[usize]) {
        let statuses: Vec<GraphStatus> = group_nodes.iter().map(|&i| self.nodes[i].status).collect();
        let progress = count(&statuses, statuses.len());
        let started = min_defined(group_nodes.iter().map(|&i| self.nodes[i].started_at));
        let status = rollup(&progress);
        let ended = is_finished(status)
            .then(|| max_defined(group_nodes.iter().map(|&i| self.nodes[i].ended_at)))
            .flatten();
        let joined: Vec<&&RlmTask> = group_tasks
            .iter()
            .filter(|t| t.workflow.as_ref().is_some_and(|w| w.depends_on.len() > 1))
            .collect();
        let mut node = GraphNode::new(
            format!("workflow:{run}"),
            GraphKind::Workflow,
            format!(
                "workflow {} {} nodes",
                run.strip_prefix("wf-").unwrap_or(run),
                group_nodes.len()
            ),
            status,
        );
        node.started_at = started;
        node.ended_at = ended;
        node.tokens = sum(group_nodes.iter().map(|&i| self.nodes[i].tokens));
        node.cost = sum(group_nodes.iter().map(|&i| self.nodes[i].cost));
        node.progress = Some(progress);
        if !joined.is_empty() {
            node.note = Some(format!(
                "{} join{}",
                joined.len(),
                if joined.len() == 1 { "" } else { "s" }
            ));
        }
        let join_text = joined
            .iter()
            .filter_map(|t| t.workflow.as_ref())
            .map(|w| format!("{} ⇐ {} ({})", w.node, w.depends_on.join("+"), w.join))
            .collect::<Vec<_>>()
            .join("; ");
        let node_names = group_tasks
            .iter()
            .map(|t| t.workflow.as_ref().map_or_default(|w| w.node.clone()))
            .collect::<Vec<_>>()
            .join(", ");
        node.details = vec![
            d("run", run),
            d("nodes", node_names),
            d(
                "joins",
                if join_text.is_empty() {
                    "none".to_string()
                } else {
                    join_text
                },
            ),
            d("fetch", "the workflows.run(...) call returns every node's outcome"),
        ];
        let index = self.add(node);
        self.kids[index] = group_nodes.to_vec();
        let parent = group_tasks[0].parent_id.clone().filter(|p| !p.is_empty());
        self.items.push((index, parent, started, false));
    }

    fn add_tasks_and_workflows(&mut self, tasks: &[&RlmTask], snapshot: &RlmSnapshot) {
        let mut workflow_order: Vec<String> = Vec::new();
        let mut workflows: HashMap<String, (Vec<&RlmTask>, Vec<usize>)> = HashMap::new();
        for t in tasks {
            let node = task_node(t, snapshot);
            let at = node.started_at;
            let index = self.add(node);
            self.task_nodes.insert(t.id.clone(), index);
            if let Some(w) = &t.workflow {
                if !workflows.contains_key(&w.run) {
                    workflow_order.push(w.run.clone());
                }
                let group = workflows.entry(w.run.clone()).or_default();
                group.0.push(t);
                group.1.push(index);
                continue;
            }
            self.items
                .push((index, t.parent_id.clone().filter(|p| !p.is_empty()), at, false));
        }
        for run in &workflow_order {
            let (group_tasks, group_nodes) = &workflows[run];
            self.add_workflow_group(run, group_tasks, group_nodes);
        }
    }

    fn add_jobs_and_tool_calls(&mut self, tasks: &[&RlmTask], snapshot: &RlmSnapshot) {
        let mut lane_task: HashMap<String, String> = HashMap::new();
        for t in tasks {
            if let Some(lane) = non_empty(&t.lane) {
                lane_task.insert(lane.to_string(), t.id.clone());
            }
        }
        let owner_of = |lane: &Option<String>| {
            non_empty(lane)
                .filter(|l| *l != "main")
                .and_then(|l| lane_task.get(l).cloned())
        };
        for job in &snapshot.jobs {
            let index = self.add(job_node(job, snapshot.now));
            self.items.push((index, owner_of(&job.lane), job.started_at, false));
        }
        for call in &snapshot.tool_calls {
            let index = self.add(tool_call_node(call, snapshot.now));
            self.items
                .push((index, owner_of(&call.lane), call.started_at, call.native));
        }
    }

    fn break_task_cycles(&mut self, tasks: &[&RlmTask]) {
        let parent_of: HashMap<&str, Option<&str>> =
            tasks.iter().map(|t| (t.id.as_str(), t.parent_id.as_deref())).collect();
        for item in &mut self.items {
            let key = &self.nodes[item.0].key;
            if let Some(id) = key.strip_prefix("task:")
                && item.1.is_some()
                && task_in_cycle(id, &parent_of)
            {
                item.1 = None;
            }
        }
    }

    fn attach_items(
        &mut self,
        cell_list: &[RlmRootCell],
        cell_indexes: &[usize],
        turn_start: Option<f64>,
        now: f64,
    ) -> (Vec<usize>, Vec<usize>) {
        let mut turn_children: Vec<usize> = Vec::new();
        let mut earlier: Vec<usize> = Vec::new();
        let items = std::mem::take(&mut self.items);
        for (index, parent_task, at, outside) in items {
            if let Some(p) = parent_task
                .as_ref()
                .and_then(|parent| self.task_nodes.get(parent).or_else(|| self.frame_group_of.get(parent)))
                .copied()
                .filter(|&p| p != index)
            {
                self.kids[p].push(index);
                continue;
            }
            if let Some(c) = matching_cell_index(cell_list, at, outside, now) {
                self.kids[cell_indexes[c]].push(index);
                continue;
            }
            let current = turn_start.is_none() || at.is_some_and(|a| a >= turn_start.unwrap_or(0.0) - 1000.0);
            if current || !is_finished(self.nodes[index].status) {
                turn_children.push(index);
            } else {
                earlier.push(index);
            }
        }
        (turn_children, earlier)
    }

    fn build_node(&self, i: usize, placed: &mut HashSet<usize>) -> GraphNode {
        placed.insert(i);
        let mut node = self.nodes[i].clone();
        let children: Vec<usize> = self.kids[i].iter().filter(|c| !placed.contains(c)).copied().collect();
        node.children = children.into_iter().map(|c| self.build_node(c, placed)).collect();
        node
    }
}

fn task_in_cycle(id: &str, parent_of: &HashMap<&str, Option<&str>>) -> bool {
    let mut seen = HashSet::new();
    let mut current = Some(id);
    while let Some(c) = current {
        if !seen.insert(c) {
            return true;
        }
        current = parent_of.get(c).copied().flatten();
    }
    false
}

fn matching_cell_index(cell_list: &[RlmRootCell], at: Option<f64>, outside: bool, now: f64) -> Option<usize> {
    let at = at.filter(|_| !outside)?;
    cell_list.iter().position(|c| {
        c.started_at
            .is_some_and(|s| at >= s - 250.0 && at <= c.ended_at.unwrap_or(now) + 250.0)
    })
}

fn earlier_group_node(nodes: Vec<GraphNode>) -> GraphNode {
    let statuses: Vec<GraphStatus> = nodes.iter().map(|n| n.status).collect();
    let mut node = GraphNode::new(
        "earlier".into(),
        GraphKind::Earlier,
        format!("earlier turns · {} finished", nodes.len()),
        rollup(&count(&statuses, statuses.len())),
    );
    node.details = vec![d("items", format!("{} nodes started before this turn", nodes.len()))];
    node.children = nodes;
    node
}

fn assemble_turn_node(
    snapshot: &RlmSnapshot,
    mut children: Vec<GraphNode>,
    cell_nodes: Vec<GraphNode>,
    loose: Vec<GraphNode>,
    turn_start: Option<f64>,
) -> GraphNode {
    let statuses: Vec<GraphStatus> = cell_nodes.iter().chain(loose.iter()).map(|n| n.status).collect();
    let active = statuses
        .iter()
        .any(|s| matches!(s, GraphStatus::Running | GraphStatus::Pending));
    let usage = snapshot.usage.as_ref();
    let last_end = max_defined(cell_nodes.iter().chain(loose.iter()).map(|n| n.ended_at));
    let tokens = usage.and_then(|u| {
        u.turn_tokens
            .filter(|t| *t != 0.0)
            .or(u.usage_total_tokens.filter(|t| *t != 0.0))
    });
    let prompt = snapshot.turn.as_ref().and_then(|t| non_empty(&t.prompt));
    let mut details = Vec::new();
    if let Some(p) = prompt {
        details.push(d("prompt", flat(p, 300)));
    }
    if let Some(r) = usage.and_then(|u| non_empty(&u.root_id)) {
        details.push(d("root", r));
    }
    details.push(d("cells", cell_nodes.len().to_string()));
    if let Some(t) = snapshot.truncated_tasks.filter(|t| *t != 0.0) {
        details.push(d("listing", format!("{} older tasks not listed", number::to_string(t))));
    }
    let status = if active {
        GraphStatus::Running
    } else if statuses.contains(&GraphStatus::Failed) {
        GraphStatus::Failed
    } else if statuses.is_empty() {
        GraphStatus::Pending
    } else {
        GraphStatus::Done
    };
    let mut turn = GraphNode::new(
        "turn".into(),
        GraphKind::Turn,
        match prompt {
            Some(p) => format!("turn  “{}”", flat(p, 60)),
            None => "turn".into(),
        },
        status,
    );
    turn.started_at = turn_start.or(usage.and_then(|u| u.started_at));
    turn.ended_at = (!active).then_some(last_end).flatten();
    turn.tokens = tokens;
    turn.cost = usage.and_then(|u| u.spent_usd.or(u.usage_cost)).filter(|c| *c > 0.0);
    turn.details = details;
    children.extend(cell_nodes);
    children.extend(loose);
    turn.children = children;
    turn
}

pub fn build_rlm_graph(snapshot: &RlmSnapshot) -> GraphNode {
    let turn_start = snapshot.turn.as_ref().and_then(|t| t.started_at);
    let mut arena = GraphArena::new();
    arena.add_frame_groups(snapshot);
    let tasks = arena.collect_tasks_and_orphans(snapshot);
    arena.add_tasks_and_workflows(&tasks, snapshot);
    arena.add_jobs_and_tool_calls(&tasks, snapshot);
    arena.break_task_cycles(&tasks);

    let cell_list: Vec<RlmRootCell> = if !snapshot.cells.is_empty() {
        snapshot.cells.clone()
    } else {
        snapshot.root_cell.clone().into_iter().collect()
    };
    let cell_indexes: Vec<usize> = cell_list
        .iter()
        .enumerate()
        .map(|(i, c)| arena.add(cell_node(c, i)))
        .collect();
    let (turn_children, earlier) = arena.attach_items(&cell_list, &cell_indexes, turn_start, snapshot.now);

    let mut placed = HashSet::new();
    let mut children: Vec<GraphNode> = Vec::new();
    if !earlier.is_empty() {
        let mut nodes: Vec<GraphNode> = earlier.iter().map(|&i| arena.build_node(i, &mut placed)).collect();
        sort_chronological(&mut nodes);
        children.push(earlier_group_node(nodes));
    }
    let mut cell_nodes: Vec<GraphNode> = cell_indexes.iter().map(|&i| arena.build_node(i, &mut placed)).collect();
    for c in &mut cell_nodes {
        sort_chronological(&mut c.children);
    }
    let mut loose: Vec<GraphNode> = turn_children
        .iter()
        .map(|&i| arena.build_node(i, &mut placed))
        .collect();
    sort_chronological(&mut loose);
    assemble_turn_node(snapshot, children, cell_nodes, loose, turn_start)
}
