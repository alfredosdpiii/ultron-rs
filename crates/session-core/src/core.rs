//! The session actor: one session (format-4 journal), its lane (the `rlm` kernel over the composition root's host
//! stack) and the run in progress, owned by one thread. Every client operation is an [`Op`] answered through its
//! reply; the run's progress is reported as TypeScript lane-watch events (`run_start`, `message_*`, `tool_*`,
//! `entry_added`, `queue_update`, `compaction_*`, `config_update`, `run_end`) through the event sink, which the
//! session worker folds into its `ultron.transcript` state.
//!
//! The run executes the agent loop on a thread of its own; its events come back to the actor as [`Input::Agent`],
//! so operations keep being answered while a run streams. Runs and compactions are journaled as the harness journals
//! them ([`crate::journal`], [`crate::compaction_op`]); reads see the session as Pi entries ([`crate::pi_view`]).

use crate::compaction_op::CompactionOp;
use jsrt::sync;

pub const AUTO_NAME_ENV: &str = "ULTRON_AUTO_NAME";
pub const AUTO_REFINE_ENV: &str = "ULTRON_AUTO_REFINE";
pub const RECOVERY_RESUME_ENV: &str = "ULTRON_RECOVERY_RESUME";

#[path = "navigate.rs"]
mod navigate;
use crate::compose::{Runtime, Services};
use crate::durable_store::{SharedSession, lock as lock_session};
use crate::journal::RunJournal;
use crate::lane::{self, Lane, Provider, api_supported};
use crate::view;
use crate::{ModelFlags, RunEnv, naming, pi_view};
use agent::{AgentEvent, StreamFn};
use ai::{AbortSignal, AssistantMessage, Message, Model, SimpleStreamOptions, StreamOptions};
use jsrt::date::now_ms;
use protocol::pi_message::text as message_text;
use serde_json::{Map, Value, json};
use session_file::SessionManager;
use session_journal::ns::PENDING_ENTRY;
use session_journal::ns::{BRANCH_TIP, LANE_CONFIG, LANE_STATE};
use session_journal::repo::Metadata;
use session_journal::run::{self, MAIN_LANE};
use session_journal::{Session, SessionRepo, SystemClock, Write, names};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// `InvalidMessage` (`reason: "empty"`) of a queued message without text or image (lane.ts `enqueue`).
pub const EMPTY_QUEUED: &str = "Queued input must contain text or an image";
/// The error of a call to a [`Core`] whose thread has stopped.
pub const SESSION_CLOSED: &str = "Session is closed";

use protocol::watch::{Ended, Failure, QueueItem, QueueKind, WatchEvent};

/// What an operation answers: data for the response (`None`: no data), or the error text.
pub type OpResult = Result<Option<Value>, String>;
/// Where an operation's answer goes (called once, possibly later, from the actor's thread).
pub type Reply = Box<dyn FnOnce(OpResult) + Send>;

mod op;
pub use op::Op;

/// What reaches the actor.
pub enum Input {
    Op(Op, Reply),
    ObserverCompleted(observer::Completion),
    Agent(Box<AgentEvent>),
    /// The model's title for the session arrived.
    Title(Box<AssistantMessage>),
    /// The bug report summary with this number ended.
    BugSummaryEnded(u64),
    /// A background memory keep ended (crate::auto_memory::keep): shown to the user when it is worth a line.
    MemoryKept(crate::auto_memory::Outcome),
    /// A user's command ended: its `BashResult` (camelCase, as recorded), or the error that failed it.
    BashDone {
        command: String,
        exclude: bool,
        result: Result<Value, String>,
        reply: Reply,
    },
    /// A chunk of a running user command's output (`bash_execution_update`).
    BashOutput {
        id: Option<String>,
        delta: String,
    },
    /// Stop the lane; the sender is told when done.
    Close(std::sync::mpsc::Sender<()>),
    /// Stop the lane for a successor worker (its shell jobs keep running for it); the sender is told when done.
    HandOver(std::sync::mpsc::Sender<()>),
    /// The completion-event dispatcher's question about the root lane (crate::async_events).
    EventLane(event_lanes::LaneCall, std::sync::mpsc::Sender<event_lanes::LaneAnswer>),
    /// Report whether detached work is active (the worker keeps itself alive while it is).
    ActivityTick,
    /// The durable runtime running the turn (crate::turns).
    Turn(crate::turns::Request),
    /// The durable runtime has no work for the root conversation.
    HarnessIdle,
    /// A compaction's work off the actor ended (core::compacting).
    Woken,
    /// A fired schedule's prompt that waited for the run in progress (core::schedule_ops).
    Scheduled(Value),
    /// The turn-end checks numbered `id` ended (core::turn_guard): the labels and evidence of the ones that failed.
    TurnChecked {
        id: u64,
        failed: Vec<String>,
        /// What the after-cell checks of the turn's last cells found.
        findings: Option<String>,
    },
}

/// Where lane-watch events go.
pub type EventSink = Box<dyn FnMut(Value) + Send>;

#[derive(Clone)]
struct Queued {
    kind: QueueKind,
    message: Value,
    /// The pending entry (`ultron.pending.entry`) the message waits as; it becomes the message's entry id.
    entry_id: String,
    /// The run's turn the loop took it for (`turn_start`s it had emitted, plus one); 0 while queued.
    turn: u64,
    /// The durable runtime's submission it waits as (none while no run is in progress).
    submission: Option<durable::SubmissionId>,
}

/// The steering and follow-up queues, drained by the running loop. `delivered` holds drained messages until their
/// `message_end` reaches the actor, so clients see them queued until then.
struct Queues {
    items: Vec<Queued>,
    delivered: Vec<Queued>,
    steering_all: bool,
    follow_up_all: bool,
}

impl Queues {
    fn visible(&self) -> Vec<&Queued> {
        self.delivered.iter().chain(&self.items).collect()
    }
}

/// `selectAcceptedInbox` (lane.ts): the queued messages a run's admission takes, in queue order: every next-run
/// message and, when `take_queued`, every steering and follow-up message (only the first of a kind whose mode is
/// one-at-a-time). Messages already waiting in the durable runtime's inbox belong to a run and are never taken.
fn select_accepted(queues: &Queues, take_queued: bool) -> Vec<(String, Value)> {
    let (mut steer_taken, mut follow_up_taken) = (false, false);
    let mut selected = Vec::new();
    for item in queues.items.iter().filter(|q| q.submission.is_none()) {
        let eligible = match item.kind {
            QueueKind::NextRun => true,
            QueueKind::Steer => take_queued && (queues.steering_all || !steer_taken),
            QueueKind::FollowUp => take_queued && (queues.follow_up_all || !follow_up_taken),
        };
        if eligible {
            steer_taken |= item.kind == QueueKind::Steer;
            follow_up_taken |= item.kind == QueueKind::FollowUp;
            selected.push((item.entry_id.clone(), item.message.clone()));
        }
    }
    selected
}

struct Running {
    id: String,
    signal: AbortSignal,
    journal: RunJournal,
    /// The run's prompt (its transcript message), announced when the first generation starts.
    prompt: Value,
    /// The queued messages its admission placed before the prompt, announced with it.
    captured: Vec<Value>,
    /// The durable runtime's submission of the prompt.
    submission: Option<durable::SubmissionId>,
    /// The run's own prompt has not reached `message_end` yet (later user messages are steering or follow-ups).
    prompt_pending: bool,
    /// The `turn_start`s the actor has processed; the loop counts its own in `Queues::turns`.
    turns: u64,
    started_tip: Value,
    /// The last assistant message's error, if the run ended on one.
    error: Option<String>,
    /// Answered when the run ends (`PromptAndWait`).
    waiter: Option<Reply>,
    /// A client aborted the run (it ends `aborted`, whatever its last response says).
    cancelled: bool,
    /// An automatic compaction failed: the run ends with its error and compacts no more.
    compaction_failed: bool,
    /// The worker died during an overflow compaction of this run: the next checkpoint compacts first, resuming
    /// this `resumeAfter` (crate::core::auto_compact).
    resume_overflow: Option<Value>,
    /// The provider message being hidden: the empty response an aborted run ends with (its `message_start` came,
    /// its `message_end` is swallowed). Per run: a start whose end never comes cannot hide the next run's message.
    hiding: bool,
    /// The run's last boundary is done; only its terminal transaction is outstanding (it failed once).
    ending: bool,
}

pub struct OpenOptions {
    pub env: RunEnv,
    pub flags: ModelFlags,
    /// The session root (`<root>/--<cwd>--/<file>.jsonl`).
    pub session_root: PathBuf,
    pub metadata: Metadata,
    /// `print`, `json`, `rpc`, `interactive`.
    pub mode: String,
    pub services: Services,
    /// Name the session from its first prompt (and by the model's title); off for sessions removed at the end.
    pub auto_name: bool,
}

/// How long a quiet session's tail may take to reach Hindsight.
const IDLE_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

pub struct Core {
    /// The lane's stored model no longer resolves: each run fails with `model_unavailable` until a model is
    /// selected (see [`unavailable_model`]).
    model_unavailable: bool,
    env: RunEnv,
    /// The key of each request's provider: `--api-key` for the starting model's, the profile's credentials for
    /// every provider, resolved per request.
    keys: crate::request_keys::Keys,
    repo: SessionRepo,
    runtime: Runtime,
    provider: Arc<Provider>,
    model: Model,
    models: Vec<Model>,
    thinking: String,
    session: SharedSession,
    lane: Lane,
    tx: Sender<Input>,
    sink: EventSink,
    run: Option<Running>,
    /// Loki's one-time setup note, for the first root turn that follows it (crate::loki_notice).
    loki_notice: crate::loki_notice::LokiNotice,
    /// Automatic recall before user messages and retain of compaction summaries (crate::auto_memory).
    auto_memory: crate::auto_memory::AutoMemory,
    /// Memory paused at start (memorantado did not answer), for the first root turn (crate::auto_memory).
    memory_notice: Option<String>,
    /// Instruction refinements proposed after compactions and delivered before user messages (crate::auto_refine).
    auto_refine: crate::auto_refine::AutoRefine,
    queues: Arc<Mutex<Queues>>,
    auto_compaction: bool,
    auto_retry: bool,
    bash: Option<kernel::Signal>,
    /// The bug report summary in progress (its number, and its signal); cleared when it ends.
    bug_summary: Option<(u64, AbortSignal)>,
    /// How many of the session's entries were announced (`entry_added`).
    announced: usize,
    /// How many of the session's usage rows were announced (`usage`).
    announced_usage: usize,
    /// Queued messages committed with the generation that takes them, until their `message_end` arrives.
    taken: Vec<String>,
    auto_name: bool,
    naming: Option<naming::Pending>,
    /// What opening the session settled (crate::recovery), until [`Op::Recover`] acts on it.
    recovered: Option<crate::recovery::Recovered>,
    /// The durable runtime that runs every turn (crate::turns).
    durable: durable_turns::Durable,
    /// The run reopened from a dead worker's journal and resumed in the durable runtime.
    resumed: Option<String>,
    /// Completions of detached work, delivered to the lanes that own it (crate::async_events).
    events: Arc<crate::async_events::Dispatcher>,
    /// Model-owned context (`ctx.*`) and collapse on return on `main` (core::context_lane).
    context: Arc<kernel_apis::ContextControl>,
    branches: Arc<context_lane::RootBranches>,
    /// The actor's input, while `run_loop` serves it (a compaction's work reads it too, core::compacting).
    inputs: Option<std::sync::mpsc::Receiver<Input>>,
    /// Inputs that arrived during a compaction and wait for its end, in order.
    deferred: std::collections::VecDeque<Input>,
    /// The compaction or branch summary in progress (manual or automatic): `abort` cancels it, `state` reports it
    /// (`isCompacting`).
    compacting: Option<AbortSignal>,
    /// A tree navigation's branch summary is in progress (core::navigate).
    navigating: bool,
    /// What waits for the run in progress to end: a manual `compact` aborts the run first (AgentSession `compact`
    /// awaits `abort()`), then runs.
    after_run: Vec<Input>,
    /// The research-loop brake and the skill hint on `main`'s turns (crate::nudge).
    nudger: crate::nudge::ToolRoundNudger,
    skill_nudger: crate::nudge::SkillExtractionNudger,
    /// The persona's auto-nudge chain (core::persona_nudge).
    persona: resources::persona::Chain,
    /// The session's persona toggle and keep-going mode (the session value `ultron.module/persona`).
    persona_session: resources::keep_going::SessionPersona,
    observer: observer::Observer,
    /// What the host checks when a run ends (core::turn_guard).
    turn_guard: turn_guard::TurnGuard,
}

mod auto_compact;
mod background;
mod checkpoints;
pub use checkpoints::{RESTORE_IN_SANDBOX, RESTORE_WHILE_STREAMING};
mod agent_ops;
pub(crate) mod budget_gate;
mod compacting;
mod context_children;
mod context_lane;
mod continue_from;
pub mod durable_turns;
pub mod event_lanes;
mod observer;
mod open;
mod ops;
mod persona_nudge;
mod run_lifecycle;
mod schedule_ops;
pub mod turn_guard;
mod user_bash;

/// The fork marker the session a client forked to carries until its first worker reads it
/// (`forkedSessionStart` of session-start.ts).
const SESSION_START: (&str, &str) = ("ultron.session.start", "fork");

/// A user message with text and images, as the transcript stores it.
pub fn user_message(text: String, images: Vec<Value>) -> Value {
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend(images);
    json!({"role": "user", "content": content, "timestamp": now_ms()})
}

/// A transcript message as stored: a failed tool's result carries no `details`.
fn stored(message: &Message) -> Value {
    let mut value = serde_json::to_value(message).unwrap_or(Value::Null);
    if value["role"] == "toolResult" && value["isError"] == true && value["details"] == json!({}) {
        value.as_object_mut().map(|m| m.remove("details"));
    }
    value
}

/// generation.ts `configurationError`'s message for `model_unavailable`.
const MODEL_UNAVAILABLE: &str = "The configured model is unavailable in this process";

/// `ULTRON_ROOT=claude` (what `ultron --claude` sets for the session workers it starts): the root lane runs on
/// Claude Code.
pub const ROOT_ENV: &str = "ULTRON_ROOT";

/// worker-root.ts `claudeRootRequested`: [`ROOT_ENV`] is `claude`.
pub fn claude_root_requested(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var(ROOT_ENV).is_some_and(|v| jsrt::string::trim(&v).to_lowercase() == "claude")
}

/// A model the stored lane names that no longer resolves (removed from the catalog or models.json): the session
/// still opens on it, and its next generation fails with `model_unavailable` (generation.ts `prepareGeneration`), so
/// `/model` can pick another.
fn unavailable_model(provider: &str, id: &str) -> Model {
    Model {
        id: id.to_string(),
        name: id.to_string(),
        api: String::new(),
        provider: provider.to_string(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec!["text".to_string()],
        input_limits: None,
        cost: Default::default(),
        prompt_cache: None,
        context_window: 0,
        max_tokens: 0,
        sampling_params: None,
        headers: None,
        compat: None,
        request: None,
        extra: Default::default(),
    }
}

/// The lane configuration a session runs with: the session's own for a resumed session without model flags, else
/// the flags' (written for a new session). Returns the stored configuration and the model it names.
pub fn lane_config(flags: &ModelFlags, env: &RunEnv, session: Option<&mut Session>) -> Result<(Value, Model), String> {
    open::lane_config_resolving(flags, env, session).map(|(config, model, _)| (config, model))
}

/// Commit `usage` (an `ai::Usage` as JSON) as a usage row of its own, outside any turn, with `details` saying what
/// spent it (`details.source`: `session-naming`, `judge`).
pub fn record_usage_row(session: &mut Session, usage: Value, details: Value) -> Result<(), String> {
    let id = session_journal::lock_clock(&session.clock()).next_id();
    let mut row = Write::usage(&id, usage, None);
    if let Write::Usage(fields) = &mut row {
        fields.insert("details".into(), details);
    }
    session.commit(&[row]).map(|_| ())
}

/// Apply a naming response: its usage as a row of its own, and its title while the name is still automatic.
pub fn record_naming(session: &mut Session, response: &AssistantMessage) -> Result<(), String> {
    let usage = serde_json::to_value(&response.usage).map_err(|e| e.to_string())?;
    record_usage_row(
        session,
        usage,
        json!({"source": "session-naming", "model": format!("{}/{}", response.provider, response.model)}),
    )?;
    if let Some(title) = naming::title_of(response) {
        names::replace_auto_name(session, &title)?;
    }
    Ok(())
}

impl Core {
    pub fn model(&self) -> &Model {
        &self.model
    }

    pub fn thinking_level(&self) -> &str {
        &self.thinking
    }

    pub fn session(&self) -> SharedSession {
        self.session.clone()
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    /// The lane snapshot transcript state starts from (`LaneSnapshot`): the branch's entries, configuration, usage.
    pub fn snapshot(&self) -> Value {
        let entries = lock_session(&self.session)
            .branch_entries(MAIN_LANE)
            .unwrap_or_default();
        let mut usage = session_journal::usage::empty_usage();
        for row in lock_session(&self.session).state().usage_rows() {
            usage = session_journal::usage::add_usage(&usage, &row["usage"]);
        }
        json!({
            "lane": MAIN_LANE,
            "transcript": entries,
            "tipId": lock_session(&self.session).branch_tip(MAIN_LANE).flatten(),
            "configuration": {"model": {"provider": self.model.provider, "modelId": self.model.id}, "thinkingLevel": self.thinking, "activeToolNames": ["rlm"]},
            "stats": {"messageCount": entries.iter().filter(|e| e["type"] == "message").count(), "usage": usage},
            "operation": null,
            "queues": [],
            "faulted": false,
        })
    }

    /// Serve inputs until a close (or the input ends).
    pub fn run_loop(mut self, inputs: std::sync::mpsc::Receiver<Input>) {
        self.inputs = Some(inputs);
        while let Some(input) = self.next_input() {
            if self.dispatch_input(input) {
                return;
            }
        }
        self.close();
    }

    fn dispatch_input(&mut self, input: Input) -> bool {
        match input {
            Input::Close(done) => {
                self.close();
                let _ = done.send(());
                true
            }
            Input::HandOver(done) => {
                self.close_for_successor();
                let _ = done.send(());
                true
            }
            other => {
                if let Err(unhandled) = self.dispatch_run_input(other) {
                    self.dispatch_aux_input(unhandled);
                }
                false
            }
        }
    }

    fn dispatch_run_input(&mut self, input: Input) -> Result<(), Input> {
        match input {
            Input::Op(op, reply) => self.op(op, reply),
            Input::ObserverCompleted(completion) => self.observer_completed(completion),
            Input::Agent(event) => self.on_agent_event(*event),
            Input::Title(response) => self.on_title_received(&response),
            Input::Turn(request) => self.on_turn(request),
            Input::HarnessIdle => self.harness_idle(),
            Input::TurnChecked { id, failed, findings } => self.turn_checked(id, failed, findings),
            other => return Err(other),
        }
        Ok(())
    }

    fn dispatch_aux_input(&mut self, input: Input) {
        match input {
            Input::BugSummaryEnded(number) => self.on_bug_summary_ended(number),
            Input::MemoryKept(outcome) => self.memory_kept(outcome),
            Input::BashDone {
                command,
                exclude,
                result,
                reply,
            } => self.bash_done(command, exclude, result, reply),
            Input::BashOutput { id, delta } => self.emit_bash_output(id, delta),
            Input::EventLane(call, reply) => self.event_lane(call, reply),
            Input::ActivityTick => self.on_activity_tick(),
            Input::Scheduled(message) => {
                let _ = self.schedule_start(message);
            }
            Input::Woken => {}
            _ => unreachable!("handled in dispatch_input or dispatch_run_input"),
        }
    }

    fn on_bug_summary_ended(&mut self, number: u64) {
        if self.bug_summary.as_ref().is_some_and(|(current, _)| *current == number) {
            self.bug_summary = None;
        }
    }

    fn on_activity_tick(&mut self) {
        if self.run.as_ref().is_some_and(|run| run.ending) {
            self.run_end();
        }
        self.report_activity();
    }

    fn on_title_received(&mut self, response: &AssistantMessage) {
        self.naming = None;
        if record_naming(&mut lock_session(&self.session), response).is_ok() {
            self.name_changed();
        }
    }

    /// What waited for a compaction first, then the actor's input.
    fn next_input(&mut self) -> Option<Input> {
        if let Some(input) = self.deferred.pop_front() {
            return Some(input);
        }
        loop {
            let due = self.run.as_ref().and_then(|run| run.journal.held_until());
            let inputs = self.inputs.as_ref()?;
            let Some(due) = due else {
                return inputs.recv().ok();
            };
            match inputs.recv_timeout(due.saturating_duration_since(std::time::Instant::now())) {
                Ok(input) => return Some(input),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(run) = self.run.as_mut() {
                        run.journal.flush_held(&mut lock_session(&self.session));
                    }
                }
            }
        }
    }

    fn emit(&mut self, event: WatchEvent) {
        (self.sink)(event.to_value());
    }

    /// The session's name now (`session_name`, a Rust-only event the worker serves as `ultron.session-ops` state,
    /// not on the transcript).
    fn name_changed(&mut self) {
        let name = lock_session(&self.session).name();
        self.emit(WatchEvent::SessionName {
            lane: MAIN_LANE.into(),
            name,
        });
    }

    /// AgentSession `setSessionName`: `session_info_changed` to clients (Pi's RPC event).
    fn session_info_changed(&mut self) {
        let name = lock_session(&self.session).name();
        self.emit(WatchEvent::SessionInfoChanged {
            lane: MAIN_LANE.into(),
            name,
        });
    }

    fn api_key(&self) -> Option<String> {
        self.keys.key(&self.model.provider)
    }

    fn run_id(&self) -> Option<String> {
        self.run.as_ref().map(|r| r.id.clone())
    }

    /// Announce the entries and usage rows committed since the last announcement, as `entry_added` and `usage`
    /// events (a usage event carries the lane's totals after it).
    fn announce(&mut self) {
        let entries: Vec<Value> = lock_session(&self.session).state().entries()[self.announced..].to_vec();
        self.announced += entries.len();
        for entry in entries {
            let _ = self.context.entry_added(MAIN_LANE, &entry);
            self.emit(WatchEvent::EntryAdded {
                lane: MAIN_LANE.into(),
                entry,
            });
        }
        let rows: Vec<Value> = lock_session(&self.session)
            .state()
            .usage_rows()
            .into_iter()
            .cloned()
            .collect();
        for index in self.announced_usage..rows.len() {
            let totals = rows[..=index]
                .iter()
                .fold(session_journal::usage::empty_usage(), |total, row| {
                    session_journal::usage::add_usage(&total, &row["usage"])
                });
            self.emit(WatchEvent::Usage {
                lane: MAIN_LANE.into(),
                row: rows[index].clone(),
                totals,
            });
        }
        self.announced_usage = rows.len();
    }

    fn tip(&self) -> Option<String> {
        lock_session(&self.session).branch_tip(MAIN_LANE).flatten()
    }

    /// The lane's stored state (`ultron.lane.state`), idle when none is stored.
    fn lane_state(&self) -> Value {
        lock_session(&self.session)
            .value(LANE_STATE, MAIN_LANE)
            .cloned()
            .unwrap_or_else(|| json!({"currentOperationId": null, "lastOperationId": null, "inbox": []}))
    }

    /// Append a message to `main` outside a run (the lane's `appendMessage`: the entry, the new tip and the lane's
    /// state in one transaction) and announce the entry.
    fn append(&mut self, message: Value) -> Result<(), String> {
        let id = session_journal::lock_clock(&lock_session(&self.session).clock()).next_id();
        let entry = Write::message_entry(&id, self.tip().as_deref(), message);
        let lane_state = self.lane_state();
        lock_session(&self.session).commit(&[
            entry,
            Write::set(BRANCH_TIP, MAIN_LANE, id.into()),
            Write::set(LANE_STATE, MAIN_LANE, lane_state),
        ])?;
        self.announce();
        Ok(())
    }

    /// A user message's `message_end`: the run's prompt (committed when the run was admitted), a queued message
    /// the runtime placed (`queued`: its pending entry; it joins the branch when the loop takes it, see
    /// [`Core::begin_generation`]), or another user message (appended now).
    pub(super) fn user_message_end(&mut self, value: Value, queued: Option<String>) {
        let shown = self.persona_shown(value.clone());
        self.emit(WatchEvent::MessageEnd {
            run_id: self.run_id(),
            lane: MAIN_LANE.into(),
            message: shown,
        });
        let delivered = queued.and_then(|entry_id| {
            let mut queues = sync::lock(&self.queues);
            let found = queues.delivered.iter().position(|q| q.entry_id == entry_id);
            found.map(|i| queues.delivered.remove(i))
        });
        let first = self
            .run
            .as_mut()
            .is_some_and(|run| std::mem::take(&mut run.prompt_pending));
        let committed = match &delivered {
            Some(queued) if self.taken.contains(&queued.entry_id) => {
                self.taken.retain(|id| *id != queued.entry_id);
                Ok(())
            }
            Some(queued) => self.commit_queued(queued, value),
            None if !first => lock_session(&self.session).append_message(MAIN_LANE, value).map(drop),
            None => Ok(()),
        };
        if let Err(error) = committed {
            self.fail_run(error);
        }
        self.announce();
        if delivered.is_some() {
            self.queue_update();
        }
    }

    /// A queued message taken by the loop joins the branch under its pending entry's id.
    fn commit_queued(&mut self, queued: &Queued, message: Value) -> Result<(), String> {
        let entry = Write::message_entry(&queued.entry_id, self.tip().as_deref(), message);
        lock_session(&self.session).commit(&[
            entry,
            Write::delete(PENDING_ENTRY, &queued.entry_id),
            Write::set(BRANCH_TIP, MAIN_LANE, queued.entry_id.clone().into()),
        ])?;
        Ok(())
    }

    fn queue_update(&mut self) {
        let queues = sync::lock(&self.queues)
            .visible()
            .into_iter()
            .map(|q| QueueItem {
                item: "message".into(),
                kind: q.kind,
                entry_id: q.entry_id.clone(),
                message: Some(q.message.clone()),
                custom_type: None,
            })
            .collect();
        self.emit(WatchEvent::QueueUpdate {
            lane: MAIN_LANE.into(),
            queues,
        });
    }

    /// Store the model and thinking level as the lane's configuration (`ultron.lane.config`).
    fn store_lane_config(&mut self) -> Result<(), String> {
        let mut config = lock_session(&self.session)
            .value(LANE_CONFIG, MAIN_LANE)
            .cloned()
            .unwrap_or_else(|| run::lane_config("", "", "", &["rlm"]));
        config["model"] = json!({"provider": self.model.provider, "modelId": self.model.id});
        config["thinkingLevel"] = json!(self.thinking);
        if lock_session(&self.session).value(LANE_CONFIG, MAIN_LANE) != Some(&config) {
            lock_session(&self.session).commit(&[Write::set(LANE_CONFIG, MAIN_LANE, config)])?;
        }
        Ok(())
    }

    fn idle(&self) -> Result<(), String> {
        if self.run.is_some() {
            return Err("Agent is already processing".into());
        }
        Ok(())
    }

    /// `appendCustomEntry`: a `custom` entry at the tip of `main`, announced; after the run in progress (a custom
    /// entry joins the branch at a boundary).
    fn append_custom(&mut self, custom_type: String, data: Value, reply: Reply) {
        if self.run.is_some() {
            self.after_run
                .push(Input::Op(Op::AppendCustom { custom_type, data }, reply));
            return;
        }
        let id = session_journal::lock_clock(&lock_session(&self.session).clock()).next_id();
        let entry = Write::custom_entry(
            &id,
            self.tip().as_deref(),
            &custom_type,
            Some(data).filter(|d| !d.is_null()),
        );
        let lane_state = self.lane_state();
        let committed = lock_session(&self.session).commit(&[
            entry,
            Write::set(session_journal::ns::BRANCH_TIP, MAIN_LANE, id.into()),
            Write::set(session_journal::ns::LANE_STATE, MAIN_LANE, lane_state),
        ]);
        if committed.is_ok() {
            self.announce();
        }
        reply(committed.map(|_| None));
    }

    fn bash_done(&mut self, command: String, exclude: bool, result: Result<Value, String>, reply: Reply) {
        self.bash = None;
        let result = match result {
            Ok(result) => result,
            Err(e) => return reply(Err(e)),
        };
        let message = user_bash::bash_message(&command, &result, exclude, now_ms());
        self.announce_message(&message);
        if let Err(error) = self.append(message) {
            return reply(Err(error));
        }
        reply(Ok(Some(result)));
    }

    /// A message outside a run: its `message_start` and `message_end`.
    fn announce_message(&mut self, message: &Value) {
        self.emit(WatchEvent::MessageStart {
            run_id: None,
            lane: MAIN_LANE.into(),
            message: message.clone(),
        });
        self.emit(WatchEvent::MessageEnd {
            run_id: None,
            lane: MAIN_LANE.into(),
            message: message.clone(),
        });
    }

    /// AgentSession's `bash_execution_update`: a chunk of the running command's output, under its id.
    fn emit_bash_output(&mut self, id: Option<String>, delta: String) {
        self.emit(WatchEvent::BashExecutionUpdate {
            lane: MAIN_LANE.into(),
            id,
            delta,
        });
    }

    /// Stop the lane (kernels, subagents).
    pub fn close(&mut self) {
        self.stop(false);
    }

    /// As [`Core::close`] when a successor worker takes the session over: shell jobs keep running for it.
    pub fn close_for_successor(&mut self) {
        self.stop(true);
    }

    fn stop(&mut self, hand_over: bool) {
        self.observer.shutdown();
        if !hand_over && self.run.is_none() {
            self.drain_pending_title();
            self.retain_tail(false);
        }
        if let Some(run) = &self.run {
            run.signal.abort();
        }
        self.turn_guard_cancel();
        if hand_over && let Some(values) = &self.runtime.services.values {
            values.hand_over();
        }
        self.shutdown_durable_and_lane(hand_over);
    }

    fn drain_pending_title(&mut self) {
        let Some(pending) = self.naming.take() else { return };
        if !pending.wait_on_close() {
            return;
        }
        let Some(inputs) = self.inputs.take() else { return };
        while let Ok(input) = inputs.try_recv() {
            if let Input::Title(response) = input
                && record_naming(&mut lock_session(&self.session), &response).is_ok()
            {
                self.name_changed();
            }
        }
    }

    fn shutdown_durable_and_lane(&mut self, hand_over: bool) {
        self.durable.stop.abort();
        for id in self.durable.chat.provider_session_ids().unwrap_or_default() {
            ai::openai_codex_responses::close_openai_codex_websocket_sessions(Some(&id));
        }
        let _ = self.durable.chat.close();
        if let Some(signal) = &self.bash {
            signal.abort("closing");
        }
        if hand_over {
            self.lane.hand_over();
        } else {
            self.lane.shutdown();
        }
        self.events.close();
    }

    /// The Pi view of the session the read operations use.
    fn pi(&self) -> SessionManager {
        pi_view::pi_manager(&lock_session(&self.session))
    }

    fn session_file(&self) -> String {
        lock_session(&self.session).metadata().path.display().to_string()
    }
}

/// Start an actor for `options` on its own thread; returns its input, or the open error.
pub fn spawn(options: OpenOptions, sink: EventSink) -> Result<(Sender<Input>, JoinHandle<()>, Value), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let input = tx.clone();
    let trace_path = (options.env.var)(crate::startup_trace::TRACE_ENV);
    let handle = std::thread::spawn(move || match Core::open(options, input, sink) {
        Ok(core) => {
            crate::startup_trace::trace_startup(trace_path.as_deref(), "worker.ready");
            let _ = ready_tx.send(Ok((
                core.snapshot(),
                core.model().clone(),
                core.thinking_level().to_string(),
            )));
            core.run_loop(rx);
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
        }
    });
    let ready = ready_rx.recv().map_err(|e| e.to_string())??;
    let (snapshot, model, thinking) = ready;
    let mut summary = json!({"snapshot": snapshot, "model": serde_json::to_value(&model).unwrap_or(Value::Null)});
    summary["thinkingLevel"] = json!(thinking);
    Ok((tx, handle, summary))
}

/// Run `op` on the actor and wait for its answer.
pub fn call(tx: &Sender<Input>, op: Op) -> OpResult {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    tx.send(Input::Op(
        op,
        Box::new(move |result| {
            let _ = reply_tx.send(result);
        }),
    ))
    .map_err(|_| SESSION_CLOSED.to_string())?;
    reply_rx.recv().map_err(|_| SESSION_CLOSED.to_string())?
}
