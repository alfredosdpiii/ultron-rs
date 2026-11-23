use super::*;

/// Routes a cell's requests: subagents to the delegation host (as that lane), everything else to the host
/// operations. (The lane API — `rlm.lanes`, `rlm.lane.*`, `rlm.cells`,
/// `rlm.cell.cancel`, `rlm.recovery` — is answered by the lanes before a request gets here.)
pub(super) struct Router {
    pub(super) delegation: Arc<dyn HostHandler>,
    pub(super) host: Arc<Host>,
    /// The root lane's cell progress (crate::turns): shell output is published as the cell's partial output.
    pub(super) progress: Option<Arc<Mutex<Option<harness::chat::CellProgress>>>>,
    /// The lane the requests come from: its shell jobs are its own.
    pub(super) lane: String,
    pub(super) config: Arc<Config>,
    /// `swarm.*`, `board.*`, `notes.*`, `rlm.ask` and persistent children, once the durable runtime is open.
    pub(super) swarm: Option<Arc<crate::swarm_ops::SwarmOps>>,
    /// The cell these requests come from (none outside a cell): cancelling it stops its shell commands.
    pub(super) cell: Option<CellShell>,
}

/// Why a cancelled cell's shell request failed: Python raises it, so the cell ends at once with its kernel (and the
/// lane's variables) intact.
pub const CELL_CANCELLED_SHELL: &str = "the cell was cancelled; its shell command was stopped";

/// The shell work of one cell. Cancelling the cell (its wait signal aborts while the kernel still runs it) stops
/// its blocking `bash` at once and cancels the shell jobs it started; a cell that ends normally leaves its jobs
/// running (they outlive cells by design). The kernel aborts a cell's request signal before the lanes abort its
/// wait signal at a normal end, so a wait abort with the request signal live is a cancel.
#[derive(Clone)]
pub(super) struct CellShell {
    waits: Signal,
    /// Jobs this cell started, and whether the cancel listener is armed.
    jobs: Arc<Mutex<(Vec<String>, bool)>>,
}

impl CellShell {
    pub(super) fn new(waits: Signal) -> CellShell {
        CellShell {
            waits,
            jobs: Arc::default(),
        }
    }

    /// Whether the cell is being cancelled (as opposed to having ended): `request` is the cell's request signal.
    fn cancelled(&self, request: &Signal) -> bool {
        self.waits.is_aborted() && !request.is_aborted()
    }

    /// Runs one shell request under a signal that aborts with the request or with the cell's cancel; a cancelled
    /// cell's request fails with [`CELL_CANCELLED_SHELL`]. Jobs it starts are recorded.
    fn run(
        &self,
        host: &Arc<Host>,
        lane: &str,
        kind: &str,
        payload: Map<String, Value>,
        request: &Signal,
    ) -> Result<Value, String> {
        if self.cancelled(request) {
            return Err(CELL_CANCELLED_SHELL.into());
        }
        let combined = Signal::new();
        let follow = |source: &Signal| {
            let target = combined.clone();
            source.link(move |reason| target.abort(reason))
        };
        let links = (follow(request), follow(&self.waits));
        let result = host.handle_lane(lane, kind, payload, &combined);
        drop(links);
        if let Ok(value) = &result {
            let job = match kind {
                "shell.run" => value["id"].as_str(),
                _ => value["job"]["id"].as_str(),
            };
            if let Some(job) = job.filter(|_| kind != "shell.cancel") {
                self.track(host, lane, job, request);
            }
        }
        if self.cancelled(request) {
            return Err(CELL_CANCELLED_SHELL.into());
        }
        result
    }

    /// Records a job this cell started; the first one arms the cancel listener.
    fn track(&self, host: &Arc<Host>, lane: &str, job: &str, request: &Signal) {
        let mut jobs = sync::lock(&self.jobs);
        if !jobs.0.iter().any(|known| known == job) {
            jobs.0.push(job.to_string());
        }
        if jobs.1 {
            return;
        }
        jobs.1 = true;
        drop(jobs);
        let stop = {
            let (host, lane, request, list) = (host.clone(), lane.to_string(), request.clone(), self.jobs.clone());
            move |_reason: String| stop_cell_jobs(&host, &lane, &request, &list)
        };
        if self.waits.on_abort(stop).is_none() {
            stop_cell_jobs(host, lane, request, &self.jobs);
        }
    }
}

impl HostHandler for Router {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind == "rlm.find_models" {
            return Ok(self.config.find_models());
        }
        if kind == "rlm.race_models" {
            return Ok(self.config.race_models());
        }
        if let Some(swarm) = &self.swarm
            && let Some(result) = swarm.handle(&self.lane, kind, &payload, signal)
        {
            return result;
        }
        if kind == crate::turns::CELL_OUTPUT {
            if let Some(progress) = &self.progress {
                crate::turns::publish_output(
                    progress,
                    payload.get("text").and_then(Value::as_str).unwrap_or(""),
                    &session_mask(&self.config),
                );
            }
            return Ok(Value::Null);
        }
        if Delegation::serves(kind) {
            return self.delegation.handle(kind, payload, signal);
        }
        if kind == "ext.call" || kind == "ext.list" {
            return Err(EXTENSIONS_UNSUPPORTED.into());
        }
        let shell = matches!(kind, "bash" | "shell.run" | "shell.result");
        let result = match &self.cell {
            Some(cell) if shell => cell.run(&self.host, &self.lane, kind, payload, signal),
            _ => self.host.handle_lane(&self.lane, kind, payload, signal),
        };
        if shell
            && let (Some(progress), Ok(value)) = (&self.progress, &result)
            && let Some(output) = value["output"].as_str().filter(|o| !o.is_empty())
            && let Some(progress) = sync::lock(progress).as_ref()
        {
            let output = session_mask(&self.config)(output);
            progress.output(
                if output.ends_with('\n') {
                    output
                } else {
                    format!("{output}\n")
                }
                .as_str(),
            );
        }
        result
    }
}

/// Cancels the shell jobs a cancelled cell started (`shell.cancel`: the host stops each job's own process group).
/// Nothing when `request` is aborted: the cell ended (or its kernel failed) rather than being cancelled.
fn stop_cell_jobs(host: &Arc<Host>, lane: &str, request: &Signal, jobs: &Mutex<(Vec<String>, bool)>) {
    if request.is_aborted() {
        return;
    }
    let ids = std::mem::take(&mut sync::lock(jobs).0);
    for id in ids {
        let mut payload = Map::new();
        payload.insert("id".into(), Value::String(id));
        let _ = host.handle_lane(lane, "shell.cancel", payload, &Signal::new());
    }
}

/// Every host request of a lane's kernel, observed for the situational hints (what a cell waited on, polled or
/// detached).
pub(super) struct ObservedByHints {
    pub(super) inner: Arc<dyn HostHandler>,
    pub(super) hints: Arc<kernel_apis::cell_hints::CellHints>,
    pub(super) lane: String,
}

impl HostHandler for ObservedByHints {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        let started = jsrt::date::now_ms();
        let observed = payload.clone();
        let result = self.inner.handle(kind, payload, signal);
        self.hints
            .observe(&self.lane, kind, &observed, result.as_ref().ok(), started);
        result
    }
}

/// `rlm.infer` / `rlm.map` under a cell: the frames see an abort of the request or of the cell (the cell's host
/// waits end as soon as it is cancelled, before its kernel is interrupted).
pub(super) struct FramesOfCell {
    pub(super) inner: Arc<dyn HostHandler>,
    pub(super) cell: Signal,
}

impl HostHandler for FramesOfCell {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind != "rlm.infer" && kind != "rlm.map" {
            return self.inner.handle(kind, payload, signal);
        }
        let combined = Signal::new();
        let done = Signal::new();
        let (cell, request, abort, finished) = (self.cell.clone(), signal.clone(), combined.clone(), done.clone());
        let watcher = std::thread::spawn(move || {
            while !finished.wait(Duration::from_millis(20)) {
                for source in [&cell, &request] {
                    if source.is_aborted() {
                        abort.abort(source.reason().unwrap_or_else(|| "the cell was cancelled".into()));
                        return;
                    }
                }
            }
        });
        let result = self.inner.handle(kind, payload, &combined);
        done.abort("finished");
        let _ = watcher.join();
        result
    }
}

/// The summary of a task's end event (`taskEvent` of session-worker.ts): the task's definition, then its verdict
/// tag (`[passed, verified]`, `[unverified]`, ...) when it has one, then its reply (a non-string value as JSON) or,
/// unless it succeeded, its error.
pub fn task_event_summary(task: &Value) -> String {
    let result = &task["result"];
    let detail = if result["status"] == "succeeded" {
        match &result["value"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    } else {
        result["error"].as_str().unwrap_or_default().to_string()
    };
    let tag = if result.is_object() {
        delegation::verdict::verdict_tag(result)
    } else {
        None
    };
    let definition = task["definition"].as_str().unwrap_or("undefined");
    match tag {
        Some(tag) => format!("{definition}: {tag} {detail}"),
        None => format!("{definition}: {detail}"),
    }
}

/// The session's masking of text that reaches the model: values MCP servers resolved (`registerSecretValues`) and
/// the shipped rules, unless `ULTRON_MASK_SECRETS` turns it off.
pub fn session_mask(config: &Config) -> crate::async_events::MaskText {
    let (masker, flag) = (config.masker.clone(), config.mask_secrets.clone());
    Arc::new(move |text: &str| masker.mask_counted(text, flag.as_deref()).0)
}

/// Completions of work a run started that outlives its cell: typed tasks nobody awaited (`task_done`), subagents
/// (`child_done`) and detached cells (`cell_done`), of every lane. Each completion continues its lane's run as a
/// `<runtime_event>` message. `ULTRON_ASYNC_EVENTS=off` turns this off: nothing waits and nothing is announced.
pub struct RuntimeEvents {
    ends: Arc<(Mutex<Vec<RuntimeEvent>>, Condvar)>,
    enabled: bool,
    mask: crate::async_events::MaskText,
}

impl RuntimeEvents {
    /// Over the three hosts' end channels; each is read on a thread of its own until its host goes away.
    pub(super) fn new(
        (task_ends, child_ends, cell_ends): (
            Receiver<delegation::agents::TaskEnd>,
            Receiver<DetachedEnd>,
            Receiver<CellEnd>,
        ),
        enabled: bool,
        mask: crate::async_events::MaskText,
    ) -> Self {
        let ends: Arc<(Mutex<Vec<RuntimeEvent>>, Condvar)> = Arc::default();
        fn read<T: Send + 'static>(
            from: Receiver<T>,
            into: &Arc<(Mutex<Vec<RuntimeEvent>>, Condvar)>,
            event: fn(T) -> Option<RuntimeEvent>,
        ) {
            let into = into.clone();
            std::thread::spawn(move || {
                for end in from {
                    if let Some(event) = event(end) {
                        sync::lock(&into.0).push(event);
                        into.1.notify_all();
                    }
                }
            });
        }
        read(task_ends, &ends, task_end_event);
        read(child_ends, &ends, |end| Some(child_end_event(end)));
        read(cell_ends, &ends, |end| Some(cell_end_event(end)));
        RuntimeEvents { ends, enabled, mask }
    }

    /// Every completion that arrived, for the event dispatcher (crate::async_events), each for the lane that owns
    /// the work: waits up to `wait` for the first when none has. A subagent's or cell's root is left for the caller.
    pub fn recv(&self, wait: Duration) -> Vec<RuntimeEvent> {
        let ends = sync::lock(&self.ends.0);
        let (mut ends, _) = self
            .ends
            .1
            .wait_timeout_while(ends, wait, |ends| ends.is_empty())
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *ends)
    }

    /// The `<runtime_event>` lines of the root lane's `events`.
    pub fn root_lines(&self, events: &[RuntimeEvent]) -> Vec<String> {
        events
            .iter()
            .filter(|e| e.lane == session_journal::run::MAIN_LANE)
            .map(|e| {
                crate::async_events::runtime_event_line(&e.kind, &e.id, &e.status, &e.summary, &e.fetch, &self.mask)
            })
            .collect()
    }

    /// Whether events are on (`ULTRON_ASYNC_EVENTS`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }
}

/// A typed task's end nobody awaited (`task_done`, as session-worker.ts `onDetachedEnd`), with its root.
fn task_end_event(end: delegation::agents::TaskEnd) -> Option<RuntimeEvent> {
    if end.awaited {
        return None;
    }
    let task = &end.task;
    Some(RuntimeEvent {
        kind: end.kind.into(),
        id: task["id"].as_str().unwrap_or_default().into(),
        status: task_status(task).into(),
        summary: task_event_summary(task),
        fetch: end.fetch.clone(),
        lane: end.owner_lane.clone(),
        root_id: end.root_id.clone(),
    })
}

/// A subagent's end (`child_done`).
fn child_end_event(end: DetachedEnd) -> RuntimeEvent {
    let task = &end.task;
    RuntimeEvent {
        kind: end.kind.into(),
        id: task["id"].as_str().unwrap_or_default().into(),
        status: task_status(task).into(),
        summary: task_event_summary(task),
        fetch: end.fetch.clone(),
        lane: end.owner_lane.clone(),
        root_id: None,
    }
}

/// A detached cell's end (`cell_done`).
fn cell_end_event(end: CellEnd) -> RuntimeEvent {
    let (status, summary) = match &end.outcome {
        Ok(result) => (
            if result.status == kernel::Status::Ok {
                "completed"
            } else {
                "failed"
            },
            [result.stdout.as_str(), result.result.as_deref().unwrap_or_default()].join(" "),
        ),
        Err(error) => ("failed", error.clone()),
    };
    RuntimeEvent {
        kind: "cell_done".into(),
        id: end.cell,
        status: status.into(),
        summary,
        fetch: "await rlm.cells()".into(),
        lane: end.lane,
        root_id: None,
    }
}

/// A task's status: its result's, else its state, else `ended`.
fn task_status(task: &Value) -> &str {
    task["result"]["status"]
        .as_str()
        .or(task["state"].as_str())
        .unwrap_or("ended")
}

/// Whether a value lists exactly the `rlm` tool: the invariant every request body keeps.
pub fn only_rlm(tools: &[Value]) -> bool {
    tools.len() == 1 && tools[0]["name"] == json!("rlm")
}

/// Add `more` (each guard's `fileHooks.stats()` entry) into `totals`: the same guard by position and name, a new one
/// appended.
pub(super) fn add_guard_stats(totals: &mut Vec<Value>, more: &[Value]) {
    const COUNTERS: [&str; 7] = [
        "checks",
        "blocked",
        "unchecked",
        "afterChecks",
        "afterFindings",
        "totalMs",
        "turnMs",
    ];
    for (index, stats) in more.iter().enumerate() {
        match totals.get_mut(index).filter(|t| t["name"] == stats["name"]) {
            Some(total) => {
                for key in COUNTERS {
                    let sum = total[key].as_u64().unwrap_or(0) + stats[key].as_u64().unwrap_or(0);
                    total[key] = json!(sum);
                }
            }
            None => totals.push(stats.clone()),
        }
    }
}
