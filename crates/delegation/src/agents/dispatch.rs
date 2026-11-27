use super::*;
use kernel::HostHandler;

impl Agents {
    /// The kernel host handler of `lane`: this host's requests, everything else to `inner`.
    pub fn handler(&self, lane: &str, inner: Arc<dyn HostHandler>) -> impl HostHandler + use<> {
        let (host, lane) = (self.clone(), lane.to_string());
        move |kind: &str, payload: Map<String, Value>, signal: &Signal| {
            if Agents::serves(kind) || (kind == "rlm.finish" && host.owns_lane(&lane)) {
                host.handle(kind, Value::Object(payload), &lane, Some(signal))
            } else {
                inner.handle(kind, payload, signal)
            }
        }
    }

    /// Whether this host answers `kind` for any lane (`rlm.finish` only for its own lanes).
    pub fn serves(kind: &str) -> bool {
        Request::of(kind).is_some_and(|request| !matches!(request, Request::Finish))
    }

    /// Serve one host request from `lane`; `cell` is the requesting cell's signal.
    pub fn handle(&self, kind: &str, payload: Value, lane: &str, cell: Option<&Signal>) -> Result<Value, String> {
        if self.state().closed {
            return Err("Ultron task host is closed".into());
        }
        let Value::Object(payload) = payload else {
            return Err("Host payload must be a JSON object".into());
        };
        self.load_tasks()?;
        if self.state().closed {
            return Err("Ultron task host is closed".into());
        }
        let parent = self.caller_task(lane);
        let records = self.state().journal.list()?;
        validate_header_fields(kind, &payload)?;
        let Some(request) = Request::of(kind) else {
            return Err(kernel::ops::not_wired(kind));
        };
        let ctx = DispatchCtx {
            kind,
            payload,
            lane,
            cell,
            parent: parent.as_deref(),
            records: &records,
        };
        self.dispatch_request(request, ctx)
    }

    fn dispatch_request(&self, request: Request, ctx: DispatchCtx<'_>) -> Result<Value, String> {
        match request {
            Request::Ping
            | Request::List
            | Request::Status
            | Request::Register
            | Request::Finish
            | Request::BudgetEstimate => self.dispatch_core(request, ctx),
            Request::BackgroundStart
            | Request::BackgroundList
            | Request::BackgroundJob
            | Request::Start
            | Request::Task => self.dispatch_tasks(request, ctx),
            Request::WorkflowsRun
            | Request::WorkflowsStart
            | Request::WorkflowsStatus
            | Request::WorkflowsStop
            | Request::WorkflowsResume => self.dispatch_workflows(request, ctx),
        }
    }

    fn dispatch_core(&self, request: Request, ctx: DispatchCtx<'_>) -> Result<Value, String> {
        match request {
            Request::Ping => Ok(json!({ "ok": true })),
            Request::List => Ok(Value::from(self.state().registry.list())),
            Request::Status => self.status(&ctx.payload, ctx.lane, ctx.parent, ctx.records),
            Request::Register => {
                jsrt::payload::fields(&ctx.payload, &["definition"])?;
                self.state()
                    .registry
                    .register(ctx.payload.get("definition").unwrap_or(&Value::Null))
            }
            Request::Finish => self.finish_request(ctx.payload, ctx.parent),
            Request::BudgetEstimate => self.estimate(&ctx.payload, ctx.parent),
            _ => unreachable!("routed by dispatch_request"),
        }
    }

    fn dispatch_tasks(&self, request: Request, ctx: DispatchCtx<'_>) -> Result<Value, String> {
        match request {
            Request::BackgroundStart => self.handle_background_start(&ctx.payload, ctx.cell, ctx.parent),
            Request::BackgroundList => self.handle_background_list(&ctx.payload, ctx.records, ctx.parent),
            Request::BackgroundJob => {
                self.handle_background_job(ctx.kind, &ctx.payload, ctx.records, ctx.cell, ctx.parent)
            }
            Request::Start => self.handle_start(ctx.kind, &ctx.payload, ctx.cell, ctx.parent),
            Request::Task => self.handle_task(ctx.kind, &ctx.payload, ctx.records, ctx.lane, ctx.cell, ctx.parent),
            _ => unreachable!("routed by dispatch_request"),
        }
    }

    fn dispatch_workflows(&self, request: Request, ctx: DispatchCtx<'_>) -> Result<Value, String> {
        match request {
            Request::WorkflowsRun => {
                let nodes = self.workflow_for_lane(&ctx.payload, ctx.lane)?;
                self.run_workflow(nodes, ctx.cell, ctx.parent)
            }
            Request::WorkflowsStart => {
                let nodes = self.workflow_for_lane(&ctx.payload, ctx.lane)?;
                self.start_workflow(nodes, ctx.parent)
            }
            Request::WorkflowsStatus => self.workflow_status(&ctx.payload),
            Request::WorkflowsStop => self.stop_workflow(&ctx.payload),
            Request::WorkflowsResume => self.resume_workflow(&ctx.payload),
            _ => unreachable!("routed by dispatch_request"),
        }
    }

    fn workflow_for_lane(&self, payload: &Map<String, Value>, lane: &str) -> Result<Vec<Node>, String> {
        let mut nodes = self.workflow(payload)?;
        for node in &mut nodes {
            node.lane = lane.to_string();
        }
        Ok(nodes)
    }

    fn handle_background_start(
        &self,
        payload: &Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["prompt", "model", "key", "timeout_ms"])?;
        let prompt = nonempty_string(payload.get("prompt"), "prompt")?;
        if utf16::len(&prompt) > 65_536 {
            return Err("Background prompt exceeds 65536 characters".into());
        }
        let key = payload
            .get("key")
            .and_then(Value::as_str)
            .map(|k| format!("background:{k}"));
        if key.as_ref().is_some_and(|k| utf16::len(k) > 264) {
            return Err("Background key is too long".into());
        }
        let request = TaskRequest {
            definition: "background-job@1".into(),
            input: json!({ "prompt": prompt }),
            model: payload.get("model").and_then(Value::as_str).map(String::from),
            key,
            timeout_ms: payload
                .get("timeout_ms")
                .and_then(Value::as_f64)
                .map_or(DEFAULT_TIMEOUT_MS, |t| t as u64),
            ..TaskRequest::default()
        };
        let (_, admitted) = self.spawn_task_with_record(request, cell, parent, Some("task_done"))?;
        Ok(public_task(&admitted))
    }

    fn handle_background_list(
        &self,
        payload: &Map<String, Value>,
        records: &[Value],
        parent: Option<&str>,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &[])?;
        Ok(Value::from(
            records
                .iter()
                .filter(|r| r["definition"] == "background-job@1")
                .filter(|r| Self::visible(records, r["id"].as_str().unwrap_or_default(), parent))
                .map(public_task)
                .collect::<Vec<_>>(),
        ))
    }

    fn handle_background_job(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        records: &[Value],
        cell: Option<&Signal>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["id"])?;
        let id = nonempty_string(payload.get("id"), "id")?;
        let stored = records
            .iter()
            .find(|r| r["id"] == id.as_str() && r["definition"] == "background-job@1");
        let Some(stored) = stored.filter(|_| Self::visible(records, &id, parent)) else {
            return Err("Unknown background job".into());
        };
        match kind {
            "background.inspect" => Ok(public_task(&self.record(&id))),
            "background.stop" => {
                let result = self.cancel(&id, "Background job stopped")?;
                Ok(json!({ "cancelled": result["status"] == "cancelled" }))
            }
            _ => match self.is_live(&id) {
                true => self.wait_outcome(&id, cell.map(|c| (c, false))),
                false => Ok(stored.get("result").cloned().unwrap_or(Value::Null)),
            },
        }
    }

    fn handle_start(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        let spawn = kind == "agents.spawn";
        let request = self.request(payload, parent)?;
        let (id, admitted) = self.spawn_task_with_record(request, cell, parent, spawn.then_some("task_done"))?;
        if spawn {
            return Ok(json!({ "id": id, "state": admitted["state"] }));
        }
        self.result_of(&id, cell.map(|c| (c, true)))
    }

    fn handle_task(
        &self,
        kind: &str,
        payload: &Map<String, Value>,
        records: &[Value],
        lane: &str,
        cell: Option<&Signal>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["id"])?;
        let id = nonempty_string(payload.get("id"), "id")?;
        let stored = records.iter().find(|r| r["id"] == id.as_str());
        let Some(stored) = stored.filter(|_| Self::visible(records, &id, parent)) else {
            if let Some(children) = self.inner.worktree_children.get()
                && let Some(answer) = children.agents_request(kind, &id, lane, cell)
            {
                return answer;
            }
            return Err("Unknown Ultron task".into());
        };
        match kind {
            "agents.inspect" => Ok(public_task(stored)),
            "agents.cancel" => {
                let result = self.cancel(&id, "Ultron task cancelled")?;
                Ok(json!({ "cancelled": result["status"] == "cancelled" }))
            }
            _ => match self.is_live(&id) {
                true => self.wait_outcome(&id, cell.map(|c| (c, false))),
                false => Ok(stored.get("result").cloned().unwrap_or(Value::Null)),
            },
        }
    }
}

struct DispatchCtx<'a> {
    kind: &'a str,
    payload: Map<String, Value>,
    lane: &'a str,
    cell: Option<&'a Signal>,
    parent: Option<&'a str>,
    records: &'a [Value],
}

fn validate_header_fields(kind: &str, payload: &Map<String, Value>) -> Result<(), String> {
    if ["ping", "agents.list", "agents.tasks"].contains(&kind) {
        jsrt::payload::fields(payload, &[])?;
    }
    if kind == "agents.status" {
        jsrt::payload::fields(payload, &["graph"])?;
    }
    Ok(())
}

impl kernel_apis::TaskHost for Agents {
    fn caller_task_id(&self, lane: &str) -> Option<String> {
        self.caller_task(lane)
    }

    fn task_lane(&self, task_id: &str) -> Option<String> {
        self.state().task(task_id).and_then(|t| t.lane.clone())
    }

    fn strategy(&self, definition: &str) -> Result<String, String> {
        self.load_tasks()?;
        let key = definition_key(Some(&json!(definition)))?;
        Ok(self.definition(&key)?.strategy().as_str().into())
    }

    fn tasks(&self) -> Result<Vec<Value>, String> {
        self.load_tasks()?;
        self.state().journal.list()
    }

    /// Module work (instance invocations, skill runs, schedule firings, verifiers) is detached: it is collected
    /// later or awaited explicitly, and stops with its parent task's subtree.
    fn spawn(&self, request: &Value, parent: Option<&str>) -> Result<Value, String> {
        self.load_tasks()?;
        let definition = definition_key(request.get("definition"))?;
        let item = self.definition(&definition)?;
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        {
            let state = self.state();
            if !state.registry.is_valid_input(&item, &input) {
                return Err(state.registry.validation_error(&item, &input, "input"));
            }
        }
        let task = TaskRequest {
            definition,
            input,
            model: request.get("model").and_then(Value::as_str).map(String::from),
            key: request.get("key").and_then(Value::as_str).map(String::from),
            timeout_ms: request
                .get("timeoutMs")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS),
            lane: request.get("lane").and_then(Value::as_str).map(String::from),
            ..TaskRequest::default()
        };
        let id = self.spawn_task(task, None, parent, None)?;
        Ok(public_record(&self.record(&id)))
    }

    fn result(&self, task_id: &str) -> Result<Value, String> {
        if self.state().task(task_id).is_none() {
            return Err("Unknown Ultron task".into());
        }
        self.result_of(task_id, None)
    }

    fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String> {
        if self.state().task(task_id).is_none() {
            return Err("Unknown Ultron task".into());
        }
        Agents::cancel(self, task_id, reason)
    }

    fn steer(&self, task_id: &str, message: &str) -> Result<bool, String> {
        let lane = {
            let state = self.state();
            match state.task(task_id) {
                Some(task) if task.outcome.is_none() => task.lane.clone(),
                _ => None,
            }
        };
        Ok(match (lane, &self.inner.steer) {
            (Some(lane), Some(steer)) => steer(&lane, message),
            _ => false,
        })
    }

    fn usage(&self) -> Result<Option<Value>, String> {
        let root = {
            let state = self.state();
            self.status_root(&state, None)
        };
        match &self.inner.usage {
            Some(ledger) => ledger.status(root.as_deref()).map(Some),
            None => Ok(None),
        }
    }

    fn now(&self) -> i64 {
        Agents::now(self) as i64
    }

    fn pin_lane(&self, lane: &str, holder: &str) -> bool {
        self.inner.pin_lane.as_ref().is_some_and(|pin| pin(lane, holder))
    }

    fn unpin_lane(&self, lane: &str, holder: &str) {
        if let Some(unpin) = &self.inner.unpin_lane {
            unpin(lane, holder);
        }
    }
}

/// A host request [`Agents`] answers; the kinds a variant groups are told apart where it is served.
#[derive(Clone, Copy)]
enum Request {
    Ping,
    List,
    Status,
    Register,
    BackgroundStart,
    BackgroundList,
    BackgroundJob,
    Start,
    Task,
    Finish,
    WorkflowsRun,
    WorkflowsStart,
    WorkflowsStatus,
    WorkflowsStop,
    WorkflowsResume,
    BudgetEstimate,
}

const REQUEST_KINDS: &[(&str, Request)] = &[
    ("ping", Request::Ping),
    ("agents.list", Request::List),
    ("agents.status", Request::Status),
    ("agents.tasks", Request::Status),
    ("agents.register", Request::Register),
    ("background.start", Request::BackgroundStart),
    ("background.list", Request::BackgroundList),
    ("background.inspect", Request::BackgroundJob),
    ("background.result", Request::BackgroundJob),
    ("background.stop", Request::BackgroundJob),
    ("agents.spawn", Request::Start),
    ("agents.invoke", Request::Start),
    ("agents.inspect", Request::Task),
    ("agents.result", Request::Task),
    ("agents.cancel", Request::Task),
    ("rlm.finish", Request::Finish),
    ("workflows.run", Request::WorkflowsRun),
    ("workflows.start", Request::WorkflowsStart),
    ("workflows.status", Request::WorkflowsStatus),
    ("workflows.stop", Request::WorkflowsStop),
    ("workflows.resume", Request::WorkflowsResume),
    ("budget.estimate", Request::BudgetEstimate),
];

impl Request {
    fn of(kind: &str) -> Option<Request> {
        REQUEST_KINDS
            .iter()
            .find_map(|(name, request)| (*name == kind).then_some(*request))
    }
}
