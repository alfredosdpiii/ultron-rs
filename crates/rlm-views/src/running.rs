//! The running-work panel: what the session has running in the background, one line each, between the transcript
//! and the editor, so the user sees it while they keep chatting. It lists the running shell jobs (`agents.status`'s
//! `jobs`), the subagents and other tasks still active (`rlm.list_subagents` and `agents.status`'s `tasks`) and the
//! root lane's detached cells (`rlm.detached`: cells whose `rlm` call returned while they kept running; each end
//! arrives as a `cell_done` event). It draws from what the RLM views poll and has no lines while nothing runs.

use serde_json::Value;
use theme::Fg;
use tui::{truncate_to_width, visible_width};

use super::flat;
use super::visualizer::{RlmJob, RlmSnapshot, RlmStyle, RlmTask, format_duration, is_active_state, short_id};

/// Lines the panel takes at most (its `+N more` tail included).
pub const RUNNING_WORK_MAX_LINES: usize = 4;

/// A detached cell of the root lane (`rlm.detached`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DetachedCell {
    pub id: String,
    pub lane: String,
    /// Waiting on the host (an `rlm.collect` parked it), not running Python.
    pub parked: bool,
    /// Being cancelled.
    pub cancelling: bool,
    /// ms since the epoch.
    pub started_at: Option<f64>,
}

/// `rlm.detached`'s `{cells: [{cell, lane, parked, startedAt, cancelling?}]}`; anything unreadable is left out.
pub fn parse_detached_cells(value: &Value) -> Vec<DetachedCell> {
    value["cells"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cell| {
            Some(DetachedCell {
                id: cell["cell"].as_str()?.to_string(),
                lane: cell["lane"].as_str().unwrap_or("main").to_string(),
                parked: cell["parked"] == true,
                cancelling: cell["cancelling"] == true,
                started_at: cell["startedAt"].as_f64().filter(|at| at.is_finite()),
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkKind {
    /// An `rlm.spawn` child.
    Agent,
    /// A typed agent task or background job (`agents.*`, `background.*`).
    Task,
    /// A host shell job (`rlm.jobs`).
    Job,
    /// A detached cell.
    Cell,
}

impl WorkKind {
    pub fn name(self) -> &'static str {
        match self {
            WorkKind::Agent => "agent",
            WorkKind::Task => "task",
            WorkKind::Job => "job",
            WorkKind::Cell => "cell",
        }
    }
}

/// One line of the panel.
#[derive(Clone, Debug, PartialEq)]
pub struct RunningWork {
    pub kind: WorkKind,
    /// What identifies it: the job or cell id, the short task id.
    pub id: String,
    /// What it is: the command, `name · model · state`, the definition, the cell's state.
    pub detail: String,
    /// Running (the accent dot), else waiting (a muted ring) or stopping (a warning dot).
    pub state: WorkState,
    /// ms since the epoch.
    pub started_at: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkState {
    Running,
    Waiting,
    Stopping,
}

fn running_job(job: &RlmJob) -> bool {
    job.status == "running"
}

/// Whether the panel has anything to show.
pub fn any_running_work(tasks: &[RlmTask], jobs: &[RlmJob], cells: &[DetachedCell]) -> bool {
    !cells.is_empty() || jobs.iter().any(running_job) || tasks.iter().any(|t| is_active_state(&t.state))
}

/// The definition without its `@version`.
fn definition_name(definition: &str) -> &str {
    match definition.rsplit_once('@') {
        Some((name, version)) if !name.is_empty() && version.chars().all(|c| c.is_ascii_digit()) => name,
        _ => definition,
    }
}

/// What runs, in panel order: subagents, other tasks, shell jobs, detached cells; each kind oldest first.
pub fn running_work(snapshot: &RlmSnapshot, cells: &[DetachedCell]) -> Vec<RunningWork> {
    let mut work = Vec::new();
    for task in snapshot.tasks.iter().filter(|t| is_active_state(&t.state)) {
        let child = task.definition == "rlm-child@1";
        let mut detail: Vec<&str> = Vec::new();
        if child {
            detail.extend(task.name.as_deref());
        } else {
            detail.push(
                task.workflow
                    .as_ref()
                    .map_or(definition_name(&task.definition), |w| w.node.as_str()),
            );
        }
        detail.extend(task.model.as_deref());
        detail.push(&task.state);
        work.push(RunningWork {
            kind: if child { WorkKind::Agent } else { WorkKind::Task },
            id: short_id(&task.id),
            detail: detail.join(" · "),
            state: if task.state == "running" {
                WorkState::Running
            } else {
                WorkState::Waiting
            },
            started_at: task
                .started_at
                .or_else(|| snapshot.timing.get(&task.id).and_then(|t| t.started_at)),
        });
    }
    for job in snapshot.jobs.iter().filter(|j| running_job(j)) {
        work.push(RunningWork {
            kind: WorkKind::Job,
            id: job.id.clone(),
            detail: flat(&job.command, 200),
            state: WorkState::Running,
            started_at: job.started_at,
        });
    }
    for cell in cells {
        let mut detail = vec![
            if cell.cancelling {
                "cancelling"
            } else if cell.parked {
                "detached · waiting on the host"
            } else {
                "detached"
            }
            .to_string(),
        ];
        if cell.lane != "main" {
            detail.push(format!("lane {}", cell.lane));
        }
        work.push(RunningWork {
            kind: WorkKind::Cell,
            id: cell.id.clone(),
            detail: detail.join(" · "),
            state: match (cell.cancelling, cell.parked) {
                (true, _) => WorkState::Stopping,
                (false, true) => WorkState::Waiting,
                (false, false) => WorkState::Running,
            },
            started_at: cell.started_at,
        });
    }
    let rank = |k: WorkKind| match k {
        WorkKind::Agent => 0,
        WorkKind::Task => 1,
        WorkKind::Job => 2,
        WorkKind::Cell => 3,
    };
    // Stable: a kind's items without a start keep their listing order, after those with one.
    work.sort_by(|a, b| {
        rank(a.kind).cmp(&rank(b.kind)).then(
            a.started_at
                .unwrap_or(f64::MAX)
                .total_cmp(&b.started_at.unwrap_or(f64::MAX)),
        )
    });
    work
}

/// The panel, `width` columns, at most `max_lines` (at least 1) lines: one per item, `● kind  id  detail` with its
/// elapsed time at the right edge; when more run than fit, the last line counts the rest (`+N more`).
pub fn render_running_work(
    work: &[RunningWork],
    width: usize,
    style: &RlmStyle,
    now: f64,
    max_lines: usize,
) -> Vec<String> {
    if work.is_empty() {
        return Vec::new();
    }
    let max = max_lines.max(1);
    let shown = if work.len() > max { &work[..max - 1] } else { work };
    let mut lines: Vec<String> = shown.iter().map(|item| work_line(item, width, style, now)).collect();
    if shown.len() < work.len() {
        let more = format!("  +{} more", work.len() - shown.len());
        lines.push(style.fg(Fg::Dim, &truncate_to_width(&more, width.max(1), "…", false)));
    }
    lines
}

fn work_line(item: &RunningWork, width: usize, style: &RlmStyle, now: f64) -> String {
    let width = width.max(1);
    let (glyph, color) = match item.state {
        WorkState::Running => ("●", Fg::Accent),
        WorkState::Waiting => ("○", Fg::Muted),
        WorkState::Stopping => ("●", Fg::Warning),
    };
    let elapsed = match item.started_at {
        Some(start) if now >= start => format_duration(now - start),
        _ => String::new(),
    };
    let kind = format!("{:<5}", item.kind.name());
    let head = format!(
        "{} {} {}",
        style.fg(color, glyph),
        style.fg(Fg::Muted, &kind),
        style.fg(Fg::Text, &item.id)
    );
    let head_width = visible_width(glyph) + 1 + kind.len() + 1 + visible_width(&item.id);
    let tail_width = if elapsed.is_empty() {
        0
    } else {
        visible_width(&elapsed) + 2
    };
    if head_width + tail_width > width {
        // Too narrow for the time: the head alone, cut.
        return truncate_to_width(&head, width, "…", false);
    }
    // The detail takes what the head and the time leave (after a two-column gap), else it is left out.
    let room = width - head_width - tail_width;
    let detail = if room > 2 && !item.detail.is_empty() {
        truncate_to_width(&style.fg(Fg::Muted, &item.detail), room - 2, "…", false)
    } else {
        String::new()
    };
    let mut line = head;
    let mut used = head_width;
    if !detail.is_empty() {
        line.push_str("  ");
        line.push_str(&detail);
        used += 2 + visible_width(&detail);
    }
    if !elapsed.is_empty() {
        line.push_str(&" ".repeat(width - used - visible_width(&elapsed)));
        line.push_str(&style.fg(Fg::Dim, &elapsed));
    }
    line
}
