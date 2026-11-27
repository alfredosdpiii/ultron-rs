//! The agent hub overlay (oh-my-pi packages/tui/src/overlays/agent-hub.ts, agent-hub-projection.ts,
//! agent-hub-renderer.ts, agent-transcript-viewer.ts and running-subagent-badge.ts).
//!
//! Ultron's agents are RLM children (`rlm.spawn`). The hub renders a plain view model ([`AgentView`]) so the client
//! can feed it from the daemon protocol: a roster of every agent except the main session (status, unread messages,
//! task, last activity, usage) ordered by status then recency, flat or grouped by parent; keyboard, wheel, hover
//! and click navigation; `r` revives a parked agent and `x` aborts and releases one. Enter opens the agent's
//! transcript with its own input line; submitting steers the agent. The hub never talks to the session: it returns
//! [`HubEvent`]s and the client feeds transcripts back with [`AgentHub::set_transcript`].

mod projection;
mod render;

use std::collections::HashSet;
use std::rc::Rc;

use crate::outline::FrameBrowser;
use theme::{Fg, Theme};
use tui::Input;
use tui::KeybindingsManager;
use tui::parse_sgr_mouse;

use crate::chat::{ChatStyle, ChatView};
use crate::copy_targets::TranscriptEntry;

pub use projection::{
    RankFn, RosterOrder, RosterWindow, TreeProjection, agent_notice, agent_views, compare_roster_agents, format_age,
    format_cost, project_agent_tree, roster_window, running_subagent_badge, running_subagent_ids, sanitize_single_line,
    task_summary, tree_prefix,
};

/// Identity of the ambient main session in the roster.
pub const MAIN_AGENT_ID: &str = "Main";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Running,
    Idle,
    Parked,
    Aborted,
}

impl AgentStatus {
    /// `STATUS_ORDER`.
    pub fn order(self) -> u8 {
        match self {
            AgentStatus::Running => 0,
            AgentStatus::Idle => 1,
            AgentStatus::Parked => 2,
            AgentStatus::Aborted => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Idle => "idle",
            AgentStatus::Parked => "parked",
            AgentStatus::Aborted => "aborted",
        }
    }

    /// Theme color of the status (`STATUS_TONE`).
    pub fn color(self) -> Fg {
        match self {
            AgentStatus::Running => Fg::Accent,
            AgentStatus::Idle => Fg::Success,
            AgentStatus::Parked => Fg::Muted,
            AgentStatus::Aborted => Fg::Error,
        }
    }

    /// omp's unicode status glyph.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentStatus::Running => "⟳",
            AgentStatus::Idle => "●",
            AgentStatus::Parked => "○",
            AgentStatus::Aborted => "⏹",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Main,
    /// An RLM child.
    Sub,
    /// A read-only transcript.
    Advisor,
}

/// Usage of one agent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentMetrics {
    pub tokens: u64,
    pub requests: u64,
    pub tools: u64,
    pub cost: f64,
}

/// One roster row as the daemon reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentView {
    pub id: String,
    pub kind: AgentKind,
    pub parent_id: Option<String>,
    pub status: AgentStatus,
    pub unread: usize,
    pub task: Option<String>,
    /// Last activity time, epoch milliseconds.
    pub last_activity: i64,
    /// Model id and thinking level that produced the work.
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub metrics: Option<AgentMetrics>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubEvent {
    Ignored,
    Render,
    Close,
    /// The transcript view of `id` opened: feed it with [`AgentHub::set_transcript`].
    OpenTranscript(String),
    /// Revive a parked agent.
    Revive(String),
    /// Abort (when running) and release an agent.
    Kill(String),
    /// Send `text` to the agent: steers a running one, prompts an idle one, revives a parked one first.
    Steer {
        id: String,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Roster,
    Tree,
}

/// The per-agent transcript with its input line.
pub(super) struct Viewer {
    pub(super) id: String,
    pub(super) entries: Vec<TranscriptEntry>,
    pub(super) chat: ChatView,
    pub(super) browser: FrameBrowser,
    pub(super) input: Input,
    pub(super) expanded: bool,
    /// Follow appended entries while at the bottom.
    pub(super) follow: bool,
}

pub struct AgentHub {
    pub(super) theme: Rc<Theme>,
    pub(super) keys: KeybindingsManager,
    pub(super) agents: Vec<AgentView>,
    order: RosterOrder,
    pub(super) rows: Vec<AgentView>,
    pub(super) tree: TreeProjection,
    pub(super) children: HashSet<String>,
    pub(super) view: ViewMode,
    pub(super) selected: usize,
    pub(super) hovered: Option<usize>,
    pub(super) filter: Input,
    pub(super) filter_editing: bool,
    pub(super) notice: Option<String>,
    settled: bool,
    /// Screen row -> roster row of the last frame.
    pub(super) hit_rows: Vec<Option<usize>>,
    pub(super) viewer: Option<Viewer>,
    chat_style: Rc<dyn Fn() -> ChatStyle>,
    pub(super) now: i64,
}

impl AgentHub {
    /// `chat_style` builds the transcript viewer's chat styling.
    pub fn new(theme: Rc<Theme>, keys: KeybindingsManager, chat_style: Rc<dyn Fn() -> ChatStyle>) -> Self {
        let mut filter = Input::new(keys.clone()).with_options(Some(""), None, None, None);
        filter.focused = true;
        AgentHub {
            theme,
            keys,
            agents: Vec::new(),
            order: RosterOrder::default(),
            rows: Vec::new(),
            tree: TreeProjection::default(),
            children: HashSet::new(),
            view: ViewMode::Roster,
            selected: 0,
            hovered: None,
            filter,
            filter_editing: false,
            notice: None,
            settled: true,
            hit_rows: Vec::new(),
            viewer: None,
            chat_style,
            now: 0,
        }
    }

    /// Replace the roster (every registry change). `settled`: persisted agents have loaded.
    pub fn set_agents(&mut self, agents: Vec<AgentView>, settled: bool) {
        self.agents = agents;
        self.settled = settled;
        self.refresh_rows();
    }

    pub fn rows(&self) -> &[AgentView] {
        &self.rows
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|r| r.id.as_str())
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The agent whose transcript is open.
    pub fn viewing(&self) -> Option<&str> {
        self.viewer.as_ref().map(|v| v.id.as_str())
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    fn refresh_rows(&mut self) {
        let selected_id = self.rows.get(self.selected).map(|r| r.id.clone());
        let refs: Vec<AgentView> = self.agents.iter().filter(|a| a.id != MAIN_AGENT_ID).cloned().collect();
        let ordered = self.order.order(refs, self.settled);
        let query = self.filter.value().trim().to_lowercase();
        let roster: Vec<AgentView> = if query.is_empty() {
            ordered
        } else {
            ordered
                .into_iter()
                .filter(|r| tui::fuzzy_match(&query, &r.id.to_lowercase()).matches)
                .collect()
        };
        self.children = roster.iter().filter_map(|r| r.parent_id.clone()).collect();
        match self.view {
            ViewMode::Tree => {
                let order = &self.order;
                let rank = |id: &str| order.rank(id);
                self.tree = project_agent_tree(&roster, Some(&rank));
                self.rows = self.tree.rows.iter().map(|&i| roster[i].clone()).collect();
            }
            ViewMode::Roster => {
                self.tree = TreeProjection::default();
                self.rows = roster;
            }
        }
        let kept = selected_id.and_then(|id| self.rows.iter().position(|r| r.id == id));
        self.selected = kept.unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
    }

    /// Set the agent's transcript (opening or a full reload).
    pub fn set_transcript(&mut self, id: &str, entries: Vec<TranscriptEntry>) {
        let style = (self.chat_style)();
        if let Some(v) = self.viewer.as_mut().filter(|v| v.id == id) {
            let mut chat = ChatView::new(style);
            for e in &entries {
                replay(&mut chat, e);
            }
            chat.set_tools_expanded(v.expanded);
            v.chat = chat;
            v.entries = entries;
        }
    }

    /// Append one live entry to an open transcript.
    pub fn push_transcript_entry(&mut self, id: &str, entry: TranscriptEntry) {
        if let Some(v) = self.viewer.as_mut().filter(|v| v.id == id) {
            replay(&mut v.chat, &entry);
            v.entries.push(entry);
        }
    }

    fn k(&self, data: &str, id: &str) -> bool {
        self.keys.matches(data, id)
    }

    fn handle_roster_mouse(&mut self, mouse: tui::SgrMouse) -> HubEvent {
        let row = usize::try_from(mouse.row)
            .ok()
            .and_then(|r| self.hit_rows.get(r).copied().flatten());
        if let Some(wheel) = mouse.wheel {
            self.hovered = None;
            if !self.rows.is_empty() {
                self.selected = (self.selected as i64 + wheel).clamp(0, self.rows.len() as i64 - 1) as usize;
            }
            return HubEvent::Render;
        }
        if mouse.motion {
            let changed = self.hovered != row;
            self.hovered = row;
            return if changed { HubEvent::Render } else { HubEvent::Ignored };
        }
        if mouse.left_click
            && let Some(index) = row
        {
            self.hovered = Some(index);
            self.selected = index;
            return self.activate();
        }
        HubEvent::Ignored
    }

    fn handle_filter_input(&mut self, data: &str) -> HubEvent {
        if self.k(data, "app.agentHub.close") || self.k(data, "tui.select.confirm") {
            self.filter_editing = false;
            if self.k(data, "app.agentHub.close") {
                self.filter.set_value("");
            }
        } else {
            let before = self.filter.value().to_string();
            self.filter.handle_input(data);
            if self.filter.value() == before {
                return HubEvent::Render;
            }
        }
        self.refresh_rows();
        HubEvent::Render
    }

    fn handle_roster_mode_key(&mut self, data: &str) -> Option<HubEvent> {
        if self.k(data, "app.agentHub.close") {
            if !self.filter.value().is_empty() {
                self.filter.set_value("");
                self.refresh_rows();
                return Some(HubEvent::Render);
            }
            return Some(HubEvent::Close);
        }
        if self.k(data, "app.agentHub.filter") {
            self.filter_editing = true;
            return Some(HubEvent::Render);
        }
        if self.k(data, "app.agentHub.tree") {
            self.hovered = None;
            self.view = match self.view {
                ViewMode::Roster => ViewMode::Tree,
                ViewMode::Tree => ViewMode::Roster,
            };
            self.refresh_rows();
            return Some(HubEvent::Render);
        }
        None
    }

    fn handle_roster_action_key(&mut self, data: &str) -> HubEvent {
        if self.k(data, "app.agentHub.down") || self.k(data, "tui.select.down") {
            self.hovered = None;
            if !self.rows.is_empty() {
                self.selected = (self.selected + 1).min(self.rows.len() - 1);
            }
            return HubEvent::Render;
        }
        if self.k(data, "app.agentHub.up") || self.k(data, "tui.select.up") {
            self.hovered = None;
            self.selected = self.selected.saturating_sub(1);
            return HubEvent::Render;
        }
        if self.k(data, "tui.select.confirm") {
            return self.activate();
        }
        if self.k(data, "app.agentHub.revive") {
            return self.revive();
        }
        if self.k(data, "app.agentHub.kill") {
            return self.kill();
        }
        HubEvent::Ignored
    }

    pub fn handle_input(&mut self, data: &str) -> HubEvent {
        if self.viewer.is_some() {
            return self.handle_viewer_input(data);
        }
        if let Some(mouse) = parse_sgr_mouse(data) {
            return self.handle_roster_mouse(mouse);
        }
        if self.k(data, "app.agents.hub") {
            return HubEvent::Close;
        }
        if self.filter_editing {
            return self.handle_filter_input(data);
        }
        if let Some(event) = self.handle_roster_mode_key(data) {
            return event;
        }
        self.handle_roster_action_key(data)
    }

    fn activate(&mut self) -> HubEvent {
        let Some(row) = self.rows.get(self.selected) else {
            return HubEvent::Ignored;
        };
        self.notice = None;
        let id = row.id.clone();
        let mut input = Input::new(self.keys.clone()).with_options(Some("› "), Some("message this agent…"), None, None);
        input.focused = true;
        self.viewer = Some(Viewer {
            id: id.clone(),
            entries: Vec::new(),
            chat: ChatView::new((self.chat_style)()),
            browser: FrameBrowser::default(),
            input,
            expanded: false,
            follow: true,
        });
        HubEvent::OpenTranscript(id)
    }

    fn revive(&mut self) -> HubEvent {
        let Some(row) = self.rows.get(self.selected) else {
            return HubEvent::Ignored;
        };
        if row.kind == AgentKind::Advisor {
            self.notice = Some(format!(
                "\"{}\" is a read-only advisor transcript — nothing to revive.",
                row.id
            ));
            return HubEvent::Render;
        }
        if row.status != AgentStatus::Parked {
            self.notice = Some(format!(
                "Agent \"{}\" is {} — only parked agents can be revived.",
                row.id,
                row.status.name()
            ));
            return HubEvent::Render;
        }
        self.notice = None;
        HubEvent::Revive(row.id.clone())
    }

    fn kill(&mut self) -> HubEvent {
        let Some(row) = self.rows.get(self.selected) else {
            return HubEvent::Ignored;
        };
        if row.kind == AgentKind::Advisor {
            self.notice = Some(format!(
                "\"{}\" is a read-only advisor transcript — cannot be killed.",
                row.id
            ));
            return HubEvent::Render;
        }
        self.notice = None;
        HubEvent::Kill(row.id.clone())
    }

    fn handle_viewer_input(&mut self, data: &str) -> HubEvent {
        if self.k(data, "app.agents.hub") {
            self.viewer = None;
            return HubEvent::Close;
        }
        let keys = self.keys.clone();
        let Some(v) = self.viewer.as_mut() else {
            return HubEvent::Ignored;
        };
        if let Some(mouse) = parse_sgr_mouse(data) {
            return handle_viewer_mouse(v, mouse);
        }
        if keys.matches(data, "app.agentHub.close") {
            if !v.input.value().is_empty() {
                v.input.set_value("");
                return HubEvent::Render;
            }
            self.viewer = None;
            return HubEvent::Render;
        }
        if keys.matches(data, "app.tools.expand") {
            v.expanded = !v.expanded;
            v.chat.set_tools_expanded(v.expanded);
            return HubEvent::Render;
        }
        if v.input.value().trim().is_empty() && handle_viewer_scroll(&keys, v, data) {
            return HubEvent::Render;
        }
        if keys.matches(data, "tui.input.submit") {
            return submit_viewer_input(v);
        }
        v.input.handle_input(data);
        HubEvent::Render
    }
}

fn handle_viewer_mouse(v: &mut Viewer, mouse: tui::SgrMouse) -> HubEvent {
    if let Some(wheel) = mouse.wheel {
        v.browser.body.scroll(wheel * 3);
        v.follow = v.browser.body.offset >= v.browser.body.max_offset();
        return HubEvent::Render;
    }
    HubEvent::Ignored
}

fn viewer_scroll_delta(keys: &KeybindingsManager, data: &str) -> Option<i64> {
    if keys.matches(data, "app.agentHub.down") || keys.matches(data, "tui.select.down") {
        Some(1)
    } else if keys.matches(data, "app.agentHub.up") || keys.matches(data, "tui.select.up") {
        Some(-1)
    } else if keys.matches(data, "app.transcript.scrollUp") {
        Some(-5)
    } else if keys.matches(data, "app.transcript.scrollDown") {
        Some(5)
    } else {
        None
    }
}

fn handle_viewer_scroll(keys: &KeybindingsManager, v: &mut Viewer, data: &str) -> bool {
    let body = &mut v.browser.body;
    let moved = if let Some(delta) = viewer_scroll_delta(keys, data) {
        Some(body.scroll(delta))
    } else if keys.matches(data, "tui.altScreen.pageUp") {
        Some(body.page(-1))
    } else if keys.matches(data, "tui.altScreen.pageDown") {
        Some(body.page(1))
    } else if keys.matches(data, "app.agentHub.top") {
        Some(body.to_top())
    } else if keys.matches(data, "app.agentHub.bottom") {
        Some(body.to_bottom())
    } else {
        None
    };
    if moved.is_some() {
        v.follow = v.browser.body.offset >= v.browser.body.max_offset();
        return true;
    }
    false
}

fn submit_viewer_input(v: &mut Viewer) -> HubEvent {
    let text = v.input.value().trim().to_string();
    v.input.set_value("");
    if text.is_empty() {
        return HubEvent::Render;
    }
    v.follow = true;
    HubEvent::Steer { id: v.id.clone(), text }
}

fn replay(chat: &mut ChatView, entry: &TranscriptEntry) {
    match entry.role() {
        "user" | "assistant" | "toolResult" => chat.add_message(&entry.message),
        _ => {
            let text = protocol::pi_message::text(&entry.message);
            if !text.trim().is_empty() {
                chat.append_notice_text(&text);
            }
        }
    }
}
