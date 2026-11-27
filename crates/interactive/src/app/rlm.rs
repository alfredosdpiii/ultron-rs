//! RLM visualizer, side-pane auto-open, status polling, and wave summary for [`super::App`].

use super::*;

impl App {
    pub(super) fn refresh_rlm(&mut self) {
        if self.rlm_round {
            self.rlm_polled = None;
            return;
        }
        self.rlm_round = true;
        self.rlm_polled = Some(self.now());
        self.send(LaneCommand::RlmStatus);
        let lane = &self.lane;
        self.rlm_poll.refresh(&mut |i| {
            let _ = lane.send(i.into());
        });
        self.send(LaneCommand::RlmInspect {
            tag: "detached".into(),
            request: "rlm.detached".into(),
            payload: serde_json::json!({}),
        });
    }

    /// `#scheduleRlmPolling`: poll every second while an RLM view or the live wave summary shows, else every five
    /// seconds. The run loop calls this on every pass; true when it polled.
    pub fn poll_rlm(&mut self) -> bool {
        let live = self.rlm_visible
            || matches!(self.panel, Some(Panel::RlmFocus(_)))
            || self.rlm_pane_open
            || self.wave_active
            || self.work_active;
        let interval = if live { RLM_POLL_VISIBLE } else { RLM_POLL_HIDDEN };
        let now = self.now();
        if self.session_id.is_none()
            || self.rlm_round
            || self
                .rlm_polled
                .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return false;
        }
        self.refresh_rlm();
        true
    }

    /// After `agents.status`'s answer (`#refreshRlm`): the running tasks' progress is assessed, and the pane, the
    /// wave summary and the running-work panel follow the new listing.
    pub(super) fn agents_status_inspected(&mut self) {
        self.assess_progress();
        self.auto_open_rlm_pane();
        self.wave_active = !self.wave_summary(200).is_empty();
        self.update_work_active();
    }

    /// "Progress assessments are cheap host reads; only fetch them for a few running tasks while visible": while the
    /// docked panel or the graph shows, `progress.assess` of the first [`rlm_views::poller::MAX_ASSESSED`] running tasks; else none.
    pub(super) fn assess_progress(&mut self) {
        let running: Vec<String> = if self.rlm_visible || matches!(self.panel, Some(Panel::RlmFocus(_))) {
            self.rlm_status()
                .tasks
                .into_iter()
                .filter(|task| rlm_views::visualizer::is_active_state(&task.state))
                .map(|task| task.id)
                .collect()
        } else {
            Vec::new()
        };
        let lane = &self.lane;
        self.rlm_poll.assess(&running, &mut |i| {
            let _ = lane.send(i.into());
        });
    }

    /// The session's work as the RLM views list it: `agents.status`, then the subagents it does not list.
    pub(super) fn rlm_status(&self) -> rlm_views::visualizer::AgentsStatus {
        use rlm_views::visualizer::parse_agents_status;
        let mut status = self.rlm_poll.status().cloned().unwrap_or_default();
        let subagents = parse_agents_status(&serde_json::json!({"tasks": self.rlm_agents["subagents"]}));
        for task in subagents.tasks {
            if !status.tasks.iter().any(|known| known.id == task.id) {
                status.tasks.push(task);
            }
        }
        status
    }

    /// An RLM inspection's answer (`#refreshRlm`): a failed one leaves the pool and context unknown.
    pub(super) fn rlm_inspected(&mut self, tag: &str, result: Result<Value, String>) {
        if let Some(key) = tag.strip_prefix("trace:") {
            if let Some(Panel::RlmFocus(focus)) = self.panel.as_mut() {
                match &result {
                    Ok(trace) => focus.set_trace(key, Ok(trace)),
                    Err(error) => focus.set_trace(key, Err(error)),
                }
            }
            return;
        }
        if tag == "detached" {
            self.rlm_round = false;
            self.detached_cells = result
                .ok()
                .map_or_default(|v| rlm_views::running::parse_detached_cells(&v));
            return self.update_work_active();
        }
        let lane = &self.lane;
        let answered = self.rlm_poll.answer(tag, result, &mut |i| {
            let _ = lane.send(i.into());
        });
        if answered == rlm_views::poller::Answered::Agents {
            self.agents_status_inspected();
        }
    }

    /// Whether the running-work panel has something to show (it then polls at the visible rate).
    pub(super) fn update_work_active(&mut self) {
        let status = self.rlm_status();
        self.work_active = rlm_views::running::any_running_work(&status.tasks, &status.jobs, &self.detached_cells);
    }

    /// The running-work panel under the transcript, one column in: the session's running shell jobs, subagents and
    /// tasks, and detached cells. Hidden while the docked RLM panel or the graph shows (they list the same work).
    pub(super) fn running_work_lines(
        &self,
        snapshot: Option<&rlm_views::visualizer::RlmSnapshot>,
        width: usize,
    ) -> Vec<String> {
        let Some(snapshot) = snapshot else { return Vec::new() };
        if self.rlm_visible || matches!(self.panel, Some(Panel::RlmFocus(_))) {
            return Vec::new();
        }
        let work = rlm_views::running::running_work(snapshot, &self.detached_cells);
        let style = rlm_views::visualizer::RlmStyle::themed(Rc::clone(&self.theme));
        rlm_views::running::render_running_work(
            &work,
            width.saturating_sub(2).max(1),
            &style,
            snapshot.now,
            rlm_views::running::RUNNING_WORK_MAX_LINES,
        )
        .into_iter()
        .map(|line| format!(" {line}"))
        .collect()
    }

    /// `#toggleRlm`: the docked RLM panel under the status.
    pub(super) fn toggle_rlm(&mut self) {
        self.rlm_visible = !self.rlm_visible;
        self.refresh_rlm();
    }

    /// Whether the RLM pane shows beside the chat (open, and the terminal wide enough). The split-viewport painter
    /// keeps finished rows in the terminal's scrollback, which a side column cannot share, so it has no pane.
    pub(super) fn rlm_pane_shown(&self) -> bool {
        self.rlm_pane_open && self.cols >= RLM_PANE_MIN_COLUMNS && !self.qol.viewport_split
    }

    /// `#toggleRlmPane`: open the RLM pane with the focus; again, the focus moves between the pane and the chat (the
    /// pane stays open). On a narrow terminal the full-screen graph opens instead.
    pub(super) fn rlm_pane(&mut self) {
        if self.cols < RLM_PANE_MIN_COLUMNS || self.qol.viewport_split {
            self.rlm_pane_open = false;
            self.rlm_pane_focused = false;
            self.open_rlm_focus();
            return;
        }
        if !self.rlm_pane_open {
            self.rlm_pane_open = true;
            self.rlm_pane_focused = true;
        } else {
            self.rlm_pane_focused = !self.rlm_pane_focused;
        }
        self.refresh_rlm();
    }

    /// `#closeRlmPane`: closed during a turn, that turn does not auto-open it again (a later turn may).
    pub(super) fn close_rlm_pane(&mut self) {
        if let Some(operation) = &self.operation {
            self.rlm_pane_handled.insert(format!("op:{operation}"));
        }
        self.rlm_pane_open = false;
        self.rlm_pane_focused = false;
    }

    /// `#autoOpenRlmPane`: work the pane shows (a child, a background task, a workflow node, a map fan-out) that
    /// starts in a run the pane has not opened for opens it beside the chat without the focus. Only on a terminal
    /// wide enough to split, on the chat screen with the graph closed; a pane the user closed stays closed for the
    /// rest of that turn and for the runs it already showed. It never closes the pane.
    pub(super) fn auto_open_rlm_pane(&mut self) {
        let ids = self.rlm_pane_work_ids();
        if !ids.iter().any(|id| !self.rlm_pane_handled.contains(id)) {
            return;
        }
        if self.rlm_pane_open {
            self.rlm_pane_handled.extend(ids);
            return;
        }
        if !self.can_auto_open_rlm_pane() {
            return;
        }
        self.rlm_pane_handled.extend(ids);
        if self
            .operation
            .as_ref()
            .is_some_and(|op| self.rlm_pane_handled.contains(&format!("op:{op}")))
        {
            return;
        }
        self.rlm_pane_open = true;
        self.rlm_pane_focused = false;
    }

    fn rlm_pane_work_ids(&mut self) -> Vec<String> {
        use rlm_views::pane::{RunKind, build_dag_runs, pane_work_runs};
        let snapshot = self.rlm_snapshot();
        let runs = build_dag_runs(&snapshot);
        pane_work_runs(&runs, self.operation_started_ms)
            .into_iter()
            .map(|run| match run.kind {
                RunKind::Turn => format!(
                    "turn:{}",
                    self.operation
                        .clone()
                        .or_else(|| run.started_at.map(|s| s.to_string()))
                        .unwrap_or_else(|| run.id.clone())
                ),
                _ => run.key.clone(),
            })
            .collect()
    }

    fn can_auto_open_rlm_pane(&self) -> bool {
        self.rlm_pane_auto
            && self.cols >= RLM_PANE_MIN_COLUMNS
            && !self.qol.viewport_split
            && self.selection.is_none()
            && self.panel.is_none()
            && self.overlay.is_none()
    }

    /// `#renderWaveSummary`: the live wave summary above the editor, hidden while the docked panel or the graph
    /// shows.
    pub(super) fn wave_summary(&mut self, width: usize) -> Vec<String> {
        if self.rlm_visible || matches!(self.panel, Some(Panel::RlmFocus(_))) {
            return Vec::new();
        }
        let snapshot = self.rlm_snapshot();
        let runs = rlm_views::pane::build_dag_runs(&snapshot);
        let style = rlm_views::visualizer::RlmStyle::themed(Rc::clone(&self.theme));
        rlm_views::pane::render_wave_summary(&runs, width.saturating_sub(2).max(1), &style, snapshot.now, 4)
            .into_iter()
            .map(|line| format!(" {line}"))
            .collect()
    }

    /// `#openRlmFocus`: the full-screen RLM graph in the editor's place; Esc returns to the chat.
    pub(super) fn open_rlm_focus(&mut self) {
        if matches!(self.panel, Some(Panel::RlmFocus(_))) {
            return;
        }
        self.set_panel(Some(Panel::RlmFocus(Box::new(rlm_views::focus::RlmGraphFocus::new()))));
        self.editor.focused = false;
        self.refresh_rlm();
    }

    /// The focus view's height: the screen above the footer (`#openRlmFocus`).
    pub(super) fn rlm_focus_height(&self) -> usize {
        let footer = render_footer(&self.theme, &self.footer_view(), self.cols.max(1)).len();
        self.rows.saturating_sub(footer + 1).max(10)
    }

    /// `#buildRlmSnapshot`: the session's subagents (polled) and the root turn's `rlm` cells (the transcript).
    pub(super) fn rlm_snapshot(&mut self) -> rlm_views::visualizer::RlmSnapshot {
        use rlm_views::visualizer::{RunningTool, TranscriptView, extract_root_cell, extract_turn};
        let now = jsrt::date::now_ms() as f64;
        let running = self.operation.as_ref().map(|_| {
            let done: std::collections::HashSet<&str> = self
                .transcript
                .iter()
                .filter(|m| m["role"] == "toolResult")
                .filter_map(|m| m["toolCallId"].as_str())
                .collect();
            self.transcript
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant")
                .and_then(|m| m["content"].as_array())
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "toolCall")
                .filter(|b| !done.contains(b["id"].as_str().unwrap_or("")))
                .map(|b| RunningTool {
                    status: "running".into(),
                    tool_call_id: b["id"].as_str().unwrap_or("").to_string(),
                    tool_name: b["name"].as_str().unwrap_or("").to_string(),
                    args: b["arguments"].clone(),
                    ..RunningTool::default()
                })
                .collect()
        });
        let view = TranscriptView::from_messages(
            &self.transcript[self.context_from.min(self.transcript.len())..],
            running,
        );
        let (turn, cells) = extract_turn(Some(&view), &mut self.rlm_clock, now, 30);
        let root_cell = extract_root_cell(Some(&view), &mut self.rlm_clock, now);
        let status = self.rlm_status();
        self.rlm_poll
            .snapshot(status, &mut self.rlm_clock, now, root_cell, cells, turn)
    }

    pub(super) fn rlm_options(&self) -> rlm_views::graph::RlmViewOptions {
        rlm_views::graph::RlmViewOptions {
            style: rlm_views::visualizer::RlmStyle::themed(Rc::clone(&self.theme)),
            spinner_frame: (self.ticks * 80 / 100) as i64,
            focus_key: self.keys.get_keys("app.rlm.focus").first().cloned(),
            ..Default::default()
        }
    }
}
