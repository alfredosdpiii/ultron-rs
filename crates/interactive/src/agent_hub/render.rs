use std::rc::Rc;

use crate::components::bold;
use crate::hub_frame::{bottom_border, boxed_row, divider, top_border};
use crate::outline::{Anchor, BrowserFrame, key_glyph};
use theme::{Bg, Fg};
use tui::Component;
use tui::{truncate_to_width, visible_width};

use super::projection::{format_age, format_cost, roster_window, sanitize_single_line, task_summary, tree_prefix};
use super::{AgentHub, AgentKind, AgentStatus, AgentView, MAIN_AGENT_ID, ViewMode};

impl AgentHub {
    /// The hub (or the open transcript) at `width` x `height`; `now` is epoch milliseconds for activity ages.
    pub fn render(&mut self, width: usize, height: usize, now: i64) -> Vec<String> {
        self.now = now;
        let width = width.max(1);
        let height = height.max(1);
        if self.viewer.is_some() {
            return self.render_viewer(width, height);
        }
        let t = Rc::clone(&self.theme);
        let inner = width.saturating_sub(4).max(1);
        let content_rows = height.saturating_sub(4).max(1);
        let mut body: Vec<String> = vec![self.render_summary_line()];
        let mut hits: Vec<Option<usize>> = vec![None];
        if content_rows >= 8 {
            body.push(String::new());
            hits.push(None);
        }
        let notice = self.render_notice_lines(inner);
        let budget = content_rows.saturating_sub(body.len() + notice.len());
        self.render_roster_body(inner, budget, &mut body, &mut hits);
        for n in notice {
            body.push(n);
            hits.push(None);
        }
        body.truncate(content_rows);
        hits.truncate(content_rows);
        while body.len() < content_rows {
            body.push(String::new());
            hits.push(None);
        }
        let mut out = vec![top_border(&t, width, "Agent Hub")];
        self.hit_rows = vec![None];
        for (line, hit) in body.iter().zip(hits) {
            out.push(boxed_row(&t, line, width));
            self.hit_rows.push(hit);
        }
        out.push(divider(&t, width));
        out.push(boxed_row(&t, &t.fg(Fg::Dim, &self.footer()), width));
        out.push(bottom_border(&t, width));
        self.hit_rows.extend([None, None, None]);
        out.truncate(height);
        out
    }

    fn render_summary_line(&self) -> String {
        let t = &self.theme;
        let active = |s: &str| bold(&t.fg(Fg::Accent, &format!(" {s} ")));
        let inactive = |s: &str| t.fg(Fg::Muted, &format!(" {s} "));
        let projection = match self.view {
            ViewMode::Roster => format!("{}{}{}", active("Flat"), t.fg(Fg::Dim, "/"), inactive("By parent")),
            ViewMode::Tree => format!("{}{}{}", inactive("Flat"), t.fg(Fg::Dim, "/"), active("By parent")),
        };
        let mut counts = Vec::new();
        for status in [
            AgentStatus::Running,
            AgentStatus::Idle,
            AgentStatus::Parked,
            AgentStatus::Aborted,
        ] {
            let n = self.rows.iter().filter(|r| r.status == status).count();
            if n > 0 {
                counts.push(t.fg(status.color(), &format!("{n} {}", status.name())));
            }
        }
        let dot = t.fg(Fg::Dim, " · ");
        let mut summary = format!("{}{dot}{projection}", bold("Roster"));
        if !counts.is_empty() {
            summary.push_str(&format!("{dot}{}", counts.join(&dot)));
        }
        summary
    }

    fn render_notice_lines(&self, inner: usize) -> Vec<String> {
        let t = &self.theme;
        self.notice
            .iter()
            .map(|n| {
                t.fg(
                    Fg::Error,
                    &truncate_to_width(&sanitize_single_line(n), inner.max(10), "…", false),
                )
            })
            .collect()
    }

    fn render_roster_body(&self, inner: usize, budget: usize, body: &mut Vec<String>, hits: &mut Vec<Option<usize>>) {
        if self.rows.is_empty() {
            let t = &self.theme;
            let empty = [
                format!("{} {}", t.fg(Fg::Muted, "○"), bold("No agents in this session")),
                t.fg(
                    Fg::Dim,
                    "Spawned RLM children (rlm.spawn) appear here while the session runs.",
                ),
            ];
            for line in empty.into_iter().take(budget) {
                body.push(line);
                hits.push(None);
            }
        } else if budget > 0 {
            self.render_windowed_entries(inner, budget, body, hits);
        }
    }

    fn render_windowed_entries(
        &self,
        inner: usize,
        budget: usize,
        body: &mut Vec<String>,
        hits: &mut Vec<Option<usize>>,
    ) {
        let t = &self.theme;
        let entries: Vec<Vec<String>> = (0..self.rows.len()).map(|i| self.entry_lines(i, inner)).collect();
        let heights: Vec<usize> = entries.iter().map(Vec::len).collect();
        let w = roster_window(&heights, self.selected, budget);
        if w.clipped {
            for line in entries[self.selected].iter().take(budget) {
                body.push(line.clone());
                hits.push(Some(self.selected));
            }
            return;
        }
        if w.top {
            body.push(t.fg(Fg::Dim, &format!("… {} more", w.start)));
            hits.push(None);
        }
        for (i, entry) in entries.iter().enumerate().take(w.end).skip(w.start) {
            for line in entry {
                body.push(line.clone());
                hits.push(Some(i));
            }
        }
        if w.bottom {
            body.push(t.fg(Fg::Dim, &format!("… {} more", self.rows.len() - w.end)));
            hits.push(None);
        }
    }

    fn first_key(&self, id: &str) -> String {
        self.keys.get_keys(id).first().map_or_default(|k| key_glyph(k))
    }

    fn footer(&self) -> String {
        let filter = if self.filter_editing || !self.filter.value().is_empty() {
            format!("/{}  ·  ", self.filter.value())
        } else {
            String::new()
        };
        let next = match self.view {
            ViewMode::Roster => "by parent",
            ViewMode::Tree => "flat",
        };
        format!(
            "{filter}{}/{}:select  {}:open  {}:{next}  {}/{}:manage  {}:filter  {}:close",
            self.first_key("app.agentHub.down"),
            self.first_key("app.agentHub.up"),
            self.first_key("tui.select.confirm"),
            self.first_key("app.agentHub.tree"),
            self.first_key("app.agentHub.revive"),
            self.first_key("app.agentHub.kill"),
            self.first_key("app.agentHub.filter"),
            self.first_key("app.agentHub.close"),
        )
    }

    fn entry_header_line(&self, index: usize, width: usize, branch: &str) -> String {
        let t = &self.theme;
        let r = &self.rows[index];
        let selected = index == self.selected;
        let cursor = if selected { t.fg(Fg::Accent, "❯") } else { " ".into() };
        let id = sanitize_single_line(&r.id);
        let styled = if selected {
            bold(&t.fg(Fg::Accent, &id))
        } else {
            bold(&id)
        };
        let mut fields = vec![format!(
            "{cursor} {branch}{} {styled}",
            t.fg(r.status.color(), r.status.glyph())
        )];
        if self.view != ViewMode::Tree
            && let Some(p) = r.parent_id.as_deref().filter(|p| *p != MAIN_AGENT_ID)
        {
            fields.push(t.fg(Fg::Dim, &format!("↳ {}", sanitize_single_line(p))));
        }
        if r.kind == AgentKind::Advisor {
            fields.push(t.fg(Fg::Warning, "read-only"));
        }
        if r.unread > 0 {
            fields.push(t.fg(Fg::Warning, &format!("✉ {}", r.unread)));
        }
        let left = fields.join("  ");
        let right = self.entry_model_badge(r);
        let (lw, rw) = (visible_width(&left), visible_width(&right));
        if rw > 0 && lw + 2 + rw <= width {
            format!("{left}{}{right}", " ".repeat(width - lw - rw))
        } else {
            truncate_to_width(&left, width, "…", false)
        }
    }

    fn entry_model_badge(&self, r: &AgentView) -> String {
        let t = &self.theme;
        r.model.as_deref().map_or_default(|m| {
            let model = t.fg(Fg::Muted, &sanitize_single_line(m));
            match r.thinking.as_deref().filter(|l| *l != "off" && *l != "inherit") {
                Some(level) => format!("{model} {}", t.thinking_border(level, level)),
                None => model,
            }
        })
    }

    fn entry_detail_indent(&self, r: &AgentView, width: usize, branch: &str) -> (String, usize) {
        let t = &self.theme;
        let tree = self.view == ViewMode::Tree;
        let detail_indent = (4 + visible_width(branch)).min(width.saturating_sub(1));
        let rail = if self.children.contains(&r.id) {
            t.fg(Fg::Dim, "│ ")
        } else {
            "  ".into()
        };
        let continuation = if tree {
            format!(
                "  {}{rail}",
                t.fg(Fg::Dim, &tree_prefix(&r.id, width, &self.tree, true))
            )
        } else {
            String::new()
        };
        let indent = if tree && visible_width(&continuation) == detail_indent {
            continuation
        } else {
            " ".repeat(detail_indent)
        };
        (indent, width.saturating_sub(detail_indent).max(1))
    }

    fn entry_meta_line(&self, r: &AgentView, indent: &str, detail_width: usize) -> String {
        let t = &self.theme;
        let age = format_age(((self.now - r.last_activity) as f64 / 1000.0).round().max(1.0) as i64);
        let meta = match r.metrics {
            Some(m) => format!(
                "{} · {} req · {} tools · {} tok · {age}",
                format_cost(m.cost),
                m.requests,
                m.tools,
                m.tokens
            ),
            None => format!("usage · {age}"),
        };
        format!(
            "{indent}{}",
            t.fg(Fg::Dim, &truncate_to_width(&meta, detail_width, "…", false))
        )
    }

    fn highlight_hovered(&self, lines: Vec<String>, width: usize) -> Vec<String> {
        let t = &self.theme;
        lines
            .into_iter()
            .map(|l| {
                let w = visible_width(&l);
                let padded = if w < width {
                    format!("{l}{}", " ".repeat(width - w))
                } else {
                    l
                };
                t.bg(Bg::SelectedBg, &padded)
            })
            .collect()
    }

    /// One roster entry: identity (cursor, tree branch, status, id, parent, unread, model), task, activity age and
    /// usage (`#renderEntry`).
    fn entry_lines(&self, index: usize, width: usize) -> Vec<String> {
        let t = &self.theme;
        let r = &self.rows[index];
        let branch = if self.view == ViewMode::Tree {
            t.fg(Fg::Dim, &tree_prefix(&r.id, width, &self.tree, false))
        } else {
            String::new()
        };
        let mut lines = vec![self.entry_header_line(index, width, &branch)];
        let (indent, detail_width) = self.entry_detail_indent(r, width, &branch);
        if let Some(task) = r.task.as_deref().map(task_summary).filter(|s| !s.trim().is_empty()) {
            lines.push(format!(
                "{indent}{}",
                t.fg(Fg::Muted, &truncate_to_width(&task, detail_width, "…", false))
            ));
        }
        lines.push(self.entry_meta_line(r, &indent, detail_width));
        if self.hovered == Some(index) {
            return self.highlight_hovered(lines, width);
        }
        lines
    }

    fn render_viewer(&mut self, width: usize, height: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let row = self
            .viewer
            .as_ref()
            .and_then(|v| self.agents.iter().find(|a| a.id == v.id))
            .cloned();
        let footer_hint = format!(
            "{}/{} scroll  {} send  {} expand  {} back  {} close hub",
            self.first_key("app.agentHub.down"),
            self.first_key("app.agentHub.up"),
            self.first_key("tui.input.submit"),
            self.first_key("app.tools.expand"),
            self.first_key("app.agentHub.close"),
            self.first_key("app.agents.hub"),
        );
        let Some(v) = self.viewer.as_mut() else {
            return Vec::new();
        };
        let status = row.as_ref().map_or_default(|r| {
            format!(
                " {}",
                t.fg(r.status.color(), &format!("{} {}", r.status.glyph(), r.status.name()))
            )
        });
        let header = vec![format!(
            "{}{status}",
            bold(&format!("Agent Hub · {}", sanitize_single_line(&v.id)))
        )];
        let body: Vec<String> = v.chat.render_item_rows(width).into_iter().flatten().collect();
        let body = if body.is_empty() {
            vec![t.fg(Fg::Muted, "  (no messages yet)")]
        } else {
            body
        };
        let input = v.input.render(width).into_iter().next().unwrap_or_default();
        let total = body.len();
        let anchor = v.follow.then(|| Anchor {
            id: format!("tail:{total}"),
            start: total.saturating_sub(1),
            end: total,
        });
        let frame = BrowserFrame {
            header,
            body,
            anchor,
            footer: vec![input, t.fg(Fg::Dim, &footer_hint)],
        };
        v.browser.render(&frame, width, height)
    }
}
