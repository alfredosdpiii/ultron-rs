//! The poll of the read-only inspections the RLM views draw from: `#refreshRlm` of client-tui.ts (the native TUI,
//! whose answers arrive later as lane events) and `pollSnapshot` of watch.ts (`ultron watch`, which asks the
//! control socket and waits). One round asks `agents.status {graph: true}` (the plain listing once a worker rejects
//! the graph field), `rlm.frames {limit}`, `rlm.pool`, `ctx.state` and `instances.list`; a progress round asks
//! `progress.assess` of up to [`MAX_ASSESSED`] running tasks. [`RlmPoller::snapshot`] assembles what the views draw.
//!
//! Requests go out through an inspect callback ([`Inspect`]: tag, request, payload); each answer comes back to
//! [`RlmPoller::answer`] by its tag, which may send follow-ups through the same callback. [`RlmPoller::poll`] runs a
//! whole round against a blocking request function.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Value, json};

use crate::visualizer::{
    AgentsStatus, RlmClock, RlmContextState, RlmFrame, RlmPool, RlmProgress, RlmRootCell, RlmSnapshot, RlmTurn,
    parse_agents_status, parse_context_state, parse_frames, parse_pool, parse_progress, parse_retained,
};

/// Frame summaries the native TUI fetches per poll (client-tui.ts `RLM_FRAME_LIMIT`).
pub const TUI_FRAME_LIMIT: u64 = 200;
/// Frame summaries `ultron watch` fetches per poll (watch.ts `RLM_FRAME_LIMIT`; it polls every 500 ms).
pub const WATCH_FRAME_LIMIT: u64 = 20;
/// Running tasks assessed per progress round (`RLM_MAX_ASSESSED`).
pub const MAX_ASSESSED: usize = 8;

/// One inspection to send: `request` with `payload`, its answer to be given back to [`RlmPoller::answer`] with `tag`.
#[derive(Clone, Debug, PartialEq)]
pub struct Inspect {
    pub tag: String,
    pub request: &'static str,
    pub payload: Value,
}

/// What an answer changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answered {
    /// The `agents.status` listing (or its failure): a progress round may follow.
    Agents,
    /// The last answer of a progress round: the shown assessments were replaced.
    Progress,
    /// Another of the poller's inspections, or a stale progress answer.
    Other,
    /// Not one of the poller's tags.
    NotMine,
}

/// The polled state the views draw from, between polls.
#[derive(Debug, Default)]
pub struct RlmPoller {
    frame_limit: u64,
    frames: Vec<RlmFrame>,
    pool: Option<RlmPool>,
    context: Option<RlmContextState>,
    retained: HashSet<String>,
    /// `agents.status {graph: true}`: the session's tasks, usage, limits, jobs, tool calls and workflows.
    status: Option<AgentsStatus>,
    /// The last failure of `agents.status` (cleared by the next answer).
    error: Option<String>,
    /// The worker rejected `agents.status {graph: true}` (`#graphListing` false): the plain listing is polled.
    plain_listing: bool,
    /// The last complete progress round's assessments.
    progress: HashMap<String, RlmProgress>,
    /// The progress round in flight: its number, the answers so far and how many are still out.
    assessing: (u64, HashMap<String, RlmProgress>, usize),
}

impl RlmPoller {
    /// A poller asking `rlm.frames` for `frame_limit` frames ([`TUI_FRAME_LIMIT`] or [`WATCH_FRAME_LIMIT`]).
    pub fn new(frame_limit: u64) -> Self {
        RlmPoller {
            frame_limit,
            ..RlmPoller::default()
        }
    }

    /// One round of inspections, in the order the native TUI sends them.
    pub fn refresh(&self, inspect: &mut dyn FnMut(Inspect)) {
        self.inspect_agents_status(inspect);
        let mut send = |tag: &str, request: &'static str, payload: Value| {
            inspect(Inspect {
                tag: tag.into(),
                request,
                payload,
            })
        };
        send("frames", "rlm.frames", json!({ "limit": self.frame_limit }));
        send("pool", "rlm.pool", json!({}));
        send("context", "ctx.state", json!({}));
        send("instances", "instances.list", json!({}));
    }

    /// `agents.status {graph: true}`, or the plain listing once the worker rejected the graph field.
    fn inspect_agents_status(&self, inspect: &mut dyn FnMut(Inspect)) {
        let payload = if self.plain_listing {
            json!({})
        } else {
            json!({ "graph": true })
        };
        inspect(Inspect {
            tag: "agents".into(),
            request: "agents.status",
            payload,
        });
    }

    /// The answer to the inspection tagged `tag`. A failed `agents.status` keeps the last listing and shows the
    /// error; one that rejects `{graph: true}` is asked again at once for the plain listing. A failed `rlm.pool` or
    /// `ctx.state` leaves them unknown; a failed `rlm.frames` or `instances.list` keeps the last answer.
    pub fn answer(&mut self, tag: &str, result: Result<Value, String>, inspect: &mut dyn FnMut(Inspect)) -> Answered {
        if let Some((round, id)) = tag.strip_prefix("progress:").and_then(|rest| rest.split_once(':')) {
            return self.progress_answered(round.parse().unwrap_or_default(), id, result);
        }
        match (tag, result) {
            ("agents", Ok(value)) => {
                self.status = Some(parse_agents_status(&value));
                self.error = None;
                return Answered::Agents;
            }
            ("agents", Err(error)) => {
                if !self.plain_listing && error.contains("Unknown payload field") {
                    self.plain_listing = true;
                    self.inspect_agents_status(inspect);
                }
                self.error = Some(error);
                return Answered::Agents;
            }
            ("frames", Ok(value)) => self.frames = parse_frames(&value),
            ("pool", result) => self.pool = result.ok().and_then(|v| parse_pool(&v)),
            ("context", result) => self.context = result.ok().and_then(|v| parse_context_state(&v)),
            ("instances", Ok(value)) => self.retained = parse_retained(&value),
            ("frames" | "instances", Err(_)) => {}
            _ => return Answered::NotMine,
        }
        Answered::Other
    }

    /// A new progress round: `progress.assess` of `running` (the caller's running tasks, at most [`MAX_ASSESSED`]
    /// of them asked). An empty list clears the shown assessments, as when no RLM view shows.
    pub fn assess(&mut self, running: &[String], inspect: &mut dyn FnMut(Inspect)) {
        let round = self.assessing.0 + 1;
        self.assessing = (round, HashMap::new(), 0);
        let running = &running[..running.len().min(MAX_ASSESSED)];
        if running.is_empty() {
            self.progress.clear();
            return;
        }
        self.assessing.2 = running.len();
        for id in running {
            inspect(Inspect {
                tag: format!("progress:{round}:{id}"),
                request: "progress.assess",
                payload: json!({ "task_id": id }),
            });
        }
    }

    /// One `progress.assess` answer of round `round`; the round's assessments replace the shown ones when the last
    /// answer arrives (a failed or unreadable one is left out).
    fn progress_answered(&mut self, round: u64, id: &str, result: Result<Value, String>) -> Answered {
        let (current, answers, left) = &mut self.assessing;
        if *current != round || *left == 0 {
            return Answered::Other;
        }
        if let Some(progress) = result.ok().and_then(|value| parse_progress(&value)) {
            answers.insert(id.to_string(), progress);
        }
        *left -= 1;
        if *left > 0 {
            return Answered::Other;
        }
        self.progress = std::mem::take(answers);
        Answered::Progress
    }

    /// A whole round against `request` (a blocking `inspect(request, payload)`), follow-ups included. No progress
    /// round: `ultron watch` does not assess.
    pub fn poll(&mut self, request: &mut dyn FnMut(&str, Value) -> Result<Value, String>) {
        let mut queue = VecDeque::new();
        self.refresh(&mut |inspect| queue.push_back(inspect));
        while let Some(next) = queue.pop_front() {
            let result = request(next.request, next.payload);
            self.answer(&next.tag, result, &mut |inspect| queue.push_back(inspect));
        }
    }

    /// The last `agents.status` listing, if any arrived.
    pub fn status(&self) -> Option<&AgentsStatus> {
        self.status.as_ref()
    }

    /// The last `agents.status` failure, until the next listing.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// `#buildRlmSnapshot`: what the views draw at `now`, from `status` (the listing, with whatever the caller adds
    /// to it), the root turn's cells, and the polled frames, pool, context, retained instances, progress and error.
    /// The task timings come from `clock`.
    pub fn snapshot(
        &self,
        status: AgentsStatus,
        clock: &mut RlmClock,
        now: f64,
        root_cell: Option<RlmRootCell>,
        cells: Vec<RlmRootCell>,
        turn: Option<RlmTurn>,
    ) -> RlmSnapshot {
        let timing = clock.timings(&status.tasks, status.usage.as_ref(), now);
        RlmSnapshot {
            now,
            tasks: status.tasks,
            usage: status.usage,
            limits: status.limits,
            jobs: status.jobs,
            tool_calls: status.tool_calls,
            workflows: status.workflows,
            truncated_tasks: status.truncated_tasks,
            timing,
            root_cell,
            cells,
            turn,
            frames: self.frames.clone(),
            pool: self.pool.clone(),
            context: self.context.clone(),
            retained: self.retained.clone(),
            progress: self.progress.clone(),
            error: self.error.clone(),
        }
    }
}
