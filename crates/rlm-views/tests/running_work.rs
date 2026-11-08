//! The running-work panel (crates/rlm-views/src/running.rs): the session's running shell jobs, subagents,
//! tasks and detached cells, one line each under the transcript, capped with a `+N more` tail; no lines while
//! nothing runs.

use rlm_views::running::{
    DetachedCell, RUNNING_WORK_MAX_LINES, WorkKind, any_running_work, parse_detached_cells, render_running_work,
    running_work,
};
use rlm_views::visualizer::{RlmSnapshot, RlmStyle, parse_agents_status};
use serde_json::{Value, json};
use tui::utils::{strip_terminal_sequences, visible_width};

const NOW: f64 = 1_000_000.0;

fn snapshot(status: Value) -> RlmSnapshot {
    let status = parse_agents_status(&status);
    RlmSnapshot {
        now: NOW,
        tasks: status.tasks,
        jobs: status.jobs,
        ..RlmSnapshot::default()
    }
}

fn render(snapshot: &RlmSnapshot, cells: &[DetachedCell], width: usize) -> Vec<String> {
    let work = running_work(snapshot, cells);
    render_running_work(&work, width, &RlmStyle::plain(), NOW, RUNNING_WORK_MAX_LINES)
        .iter()
        .map(|line| strip_terminal_sequences(line))
        .collect()
}

/// `left`, then `right` at the right edge of `width` columns.
fn edge(left: &str, right: &str, width: usize) -> String {
    let gap = width - visible_width(left) - visible_width(right);
    format!("{left}{}{right}", " ".repeat(gap))
}

fn job(id: &str, status: &str, command: &str, started: f64) -> Value {
    json!({"id": id, "status": status, "command": command, "startedAt": started})
}

#[test]
fn interactive_running_work_has_no_lines_while_nothing_runs() {
    assert!(render(&RlmSnapshot::default(), &[], 80).is_empty());
    // Finished work is not running work.
    let finished = snapshot(json!({
        "tasks": [
            {"id": "ultron-task-done", "definition": "rlm-child@1", "state": "completed"},
            {"id": "ultron-task-failed", "definition": "background-job@1", "state": "failed"},
        ],
        "jobs": [job("job-00000001", "completed", "true", NOW - 1000.0), job("job-00000002", "failed", "false", NOW)],
    }));
    assert!(render(&finished, &[], 80).is_empty());
    assert!(!any_running_work(&finished.tasks, &finished.jobs, &[]));
}

#[test]
fn interactive_running_work_lists_subagents_tasks_jobs_and_detached_cells() {
    let snapshot = snapshot(json!({
        "tasks": [
            {"id": "ultron-task-ffff0000", "definition": "rlm-child@1", "state": "queued", "name": "tester"},
            {"id": "ultron-task-abcdef123456", "definition": "rlm-child@1", "state": "running", "name": "reviewer",
             "model": "anthropic/claude-sonnet-4-5", "startedAt": NOW - 72_000.0},
            {"id": "ultron-task-0011223344", "definition": "background-job@1", "state": "running"},
            {"id": "ultron-task-old", "definition": "rlm-child@1", "state": "completed", "name": "done"},
        ],
        "jobs": [
            job("job-1a2b3c4d", "running", "npm run build -- --watch", NOW - 5_000.0),
            job("job-9f9f9f9f", "completed", "cargo test", NOW - 9_000.0),
        ],
    }));
    let cells = parse_detached_cells(&json!({"cells": [
        {"cell": "cell-7", "lane": "main", "parked": true, "startedAt": NOW - 125_000.0},
        {"cell": "cell-9", "lane": "side", "parked": false, "cancelling": true, "startedAt": NOW - 400.0},
    ]}));
    assert!(any_running_work(&snapshot.tasks, &snapshot.jobs, &cells));
    let work = running_work(&snapshot, &cells);
    assert_eq!(
        work.iter().map(|w| w.kind).collect::<Vec<_>>(),
        [
            WorkKind::Agent,
            WorkKind::Agent,
            WorkKind::Task,
            WorkKind::Job,
            WorkKind::Cell,
            WorkKind::Cell
        ]
    );
    let lines = render_running_work(&work, 80, &RlmStyle::plain(), NOW, 10);
    assert_eq!(
        lines,
        [
            edge(
                "● agent abcdef12  reviewer · anthropic/claude-sonnet-4-5 · running",
                "1m12s",
                80
            ),
            "○ agent ffff0000  tester · queued".to_string(),
            "● task  00112233  background-job · running".to_string(),
            edge("● job   job-1a2b3c4d  npm run build -- --watch", "5.0s", 80),
            edge("○ cell  cell-7  detached · waiting on the host", "2m5s", 80),
            edge("● cell  cell-9  cancelling · lane side", "400ms", 80),
        ]
    );
}

#[test]
fn interactive_running_work_caps_its_lines_with_a_more_tail() {
    let jobs: Vec<Value> = (0..6)
        .map(|i| {
            job(
                &format!("job-0000000{i}"),
                "running",
                &format!("sleep {i}"),
                NOW - 1000.0 * f64::from(i),
            )
        })
        .collect();
    let lines = render(&snapshot(json!({"jobs": jobs})), &[], 60);
    assert_eq!(lines.len(), RUNNING_WORK_MAX_LINES);
    // Oldest first.
    assert_eq!(lines[0], edge("● job   job-00000005  sleep 5", "5.0s", 60));
    assert_eq!(lines[2], edge("● job   job-00000003  sleep 3", "3.0s", 60));
    assert_eq!(lines[3], "  +3 more");
    // Exactly the cap: no tail.
    let four: Vec<Value> = (0..4)
        .map(|i| job(&format!("job-0000000{i}"), "running", "sleep", NOW))
        .collect();
    let lines = render(&snapshot(json!({"jobs": four})), &[], 60);
    assert_eq!(lines.len(), 4);
    assert!(lines.iter().all(|l| l.starts_with("● job")), "{lines:?}");
}

#[test]
fn interactive_running_work_lines_fit_narrow_widths() {
    let snapshot = snapshot(json!({
        "tasks": [{"id": "ultron-task-abcdef123456", "definition": "rlm-child@1", "state": "running",
                   "name": "a-very-long-subagent-name", "startedAt": NOW - 3_600_000.0}],
        "jobs": [job("job-1a2b3c4d", "running", "while true; do echo tick; sleep 1; done", NOW - 61_000.0)],
    }));
    for width in [1, 8, 20, 30, 44] {
        for line in render(&snapshot, &[], width) {
            assert!(visible_width(&line) <= width, "{width}: {line:?}");
        }
    }
    let lines = render(&snapshot, &[], 30);
    assert_eq!(lines[0], edge("● agent abcdef12  a-very-…", "1h", 30));
    assert_eq!(lines[1], edge("● job   job-1a2b3c4d  w…", "1m1s", 30));
    // No room for the time: the head alone, cut.
    assert_eq!(render(&snapshot, &[], 12)[1], "● job   job…");
}

#[test]
fn interactive_running_work_parses_detached_cells_defensively() {
    let cells = parse_detached_cells(&json!({"cells": [
        {"cell": "cell-3", "lane": "main", "parked": false, "startedAt": 5.0},
        {"lane": "main"},
        {"cell": "cell-4"},
    ]}));
    assert_eq!(
        cells,
        [
            DetachedCell {
                id: "cell-3".into(),
                lane: "main".into(),
                parked: false,
                cancelling: false,
                started_at: Some(5.0),
            },
            DetachedCell {
                id: "cell-4".into(),
                lane: "main".into(),
                ..DetachedCell::default()
            },
        ]
    );
    assert!(parse_detached_cells(&json!("Not an inspection request: rlm.detached")).is_empty());
}
