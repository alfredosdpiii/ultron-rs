use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkflowStatus {
    Running,
    Stopped,
    Completed,
    Failed,
}

impl WorkflowStatus {
    fn as_str(self) -> &'static str {
        match self {
            WorkflowStatus::Running => "running",
            WorkflowStatus::Stopped => "stopped",
            WorkflowStatus::Completed => "completed",
            WorkflowStatus::Failed => "failed",
        }
    }
}

pub(super) struct ManagedWorkflow {
    pub(super) id: String,
    pub(super) nodes: Vec<Node>,
    pub(super) parent: Option<String>,
    pub(super) status: WorkflowStatus,
    pub(super) completed_outputs: Map<String, Value>,
    pub(super) cancel: Signal,
    pub(super) started_at: f64,
    pub(super) updated_at: f64,
    pub(super) ended_at: Option<f64>,
    pub(super) error: Option<String>,
}

impl Agents {
    fn managed_workflow_view(wf: &ManagedWorkflow) -> Value {
        let completed: Vec<String> = wf
            .nodes
            .iter()
            .filter(|n| wf.completed_outputs.contains_key(&n.id))
            .map(|n| n.id.clone())
            .collect();
        let pending: Vec<String> = wf
            .nodes
            .iter()
            .filter(|n| !wf.completed_outputs.contains_key(&n.id))
            .map(|n| n.id.clone())
            .collect();
        let mut out = json!({
            "id": wf.id,
            "status": wf.status.as_str(),
            "completed": completed,
            "pending": pending,
            "outputs": Value::Object(wf.completed_outputs.clone()),
            "startedAt": json::number(wf.started_at),
            "updatedAt": json::number(wf.updated_at),
        });
        if let Some(ended) = wf.ended_at {
            out["endedAt"] = json::number(ended);
        }
        if let Some(error) = &wf.error {
            out["error"] = json!(error);
        }
        out
    }

    fn spawn_managed_workflow(
        &self,
        id: String,
        nodes: Vec<Node>,
        initial: Map<String, Value>,
        cancel: Signal,
        parent: Option<String>,
    ) {
        let agents = self.clone();
        std::thread::spawn(move || {
            let outputs =
                agents.execute_workflow_graph(&id, &nodes, initial, Some(&cancel), parent.as_deref(), Some(&id));
            let now = agents.now();
            let mut state = agents.state();
            if let Some(record) = state.workflow_runs.iter_mut().find(|r| r["run"] == id.as_str()) {
                record["endedAt"] = json::number(now);
            }
            if let Some(wf) = state.managed_workflows.get_mut(&id) {
                wf.completed_outputs = outputs.clone();
                wf.updated_at = now;
                if cancel.is_aborted() {
                    wf.status = WorkflowStatus::Stopped;
                    wf.ended_at = Some(now);
                } else {
                    let any_failed = outputs
                        .values()
                        .any(|o| matches!(o["status"].as_str(), Some("failed" | "exhausted" | "cancelled")));
                    wf.status = if any_failed {
                        WorkflowStatus::Failed
                    } else {
                        WorkflowStatus::Completed
                    };
                    wf.ended_at = Some(now);
                    if any_failed {
                        wf.error = outputs
                            .values()
                            .find_map(|o| o.get("error").and_then(Value::as_str).map(str::to_string));
                    }
                }
            }
            agents.inner.changed.notify_all();
        });
    }

    /// `workflows.start`: validates the graph and runs it asynchronously, returning `{id, status, ...}` at once.
    pub(super) fn start_workflow(&self, nodes: Vec<Node>, parent: Option<&str>) -> Result<Value, String> {
        let id = format!("wf-{}", &random::uuid_v4()[..8]);
        let now = self.now();
        let cancel = Signal::new();
        self.record_workflow_start(&id, &nodes, parent);
        let view = {
            let mut state = self.state();
            let wf = ManagedWorkflow {
                id: id.clone(),
                nodes: nodes.clone(),
                parent: parent.map(str::to_string),
                status: WorkflowStatus::Running,
                completed_outputs: Map::new(),
                cancel: cancel.clone(),
                started_at: now,
                updated_at: now,
                ended_at: None,
                error: None,
            };
            let view = Self::managed_workflow_view(&wf);
            state.managed_workflows.insert(id.clone(), wf);
            view
        };
        self.spawn_managed_workflow(id, nodes, Map::new(), cancel, parent.map(str::to_string));
        Ok(view)
    }

    /// `workflows.status`: inspects a managed workflow's live state and completed node outputs.
    pub(super) fn workflow_status(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["id"])?;
        let id = nonempty_string(payload.get("id"), "id")?;
        let state = self.state();
        let wf = state
            .managed_workflows
            .get(&id)
            .ok_or_else(|| format!("Unknown workflow: {id}"))?;
        Ok(Self::managed_workflow_view(wf))
    }

    /// `workflows.stop`: signals a running managed workflow to stop while preserving completed node outputs.
    pub(super) fn stop_workflow(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["id", "reason"])?;
        let id = nonempty_string(payload.get("id"), "id")?;
        let reason = payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Workflow stopped")
            .to_string();
        let cancel = {
            let mut state = self.state();
            let wf = state
                .managed_workflows
                .get_mut(&id)
                .ok_or_else(|| format!("Unknown workflow: {id}"))?;
            let cancel = wf.cancel.clone();
            if wf.status == WorkflowStatus::Running {
                wf.status = WorkflowStatus::Stopped;
                wf.updated_at = self.now();
                wf.ended_at = Some(wf.updated_at);
            }
            cancel
        };
        cancel.abort(&reason);
        let state = self.state();
        let wf = state
            .managed_workflows
            .get(&id)
            .ok_or_else(|| format!("Unknown workflow: {id}"))?;
        Ok(Self::managed_workflow_view(wf))
    }

    /// `workflows.resume`: resumes a stopped or failed managed workflow, skipping already-succeeded nodes.
    pub(super) fn resume_workflow(&self, payload: &Map<String, Value>) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["id"])?;
        let id = nonempty_string(payload.get("id"), "id")?;
        let (nodes, initial, cancel, parent, resumed_from, view) = {
            let mut state = self.state();
            let wf = state
                .managed_workflows
                .get_mut(&id)
                .ok_or_else(|| format!("Unknown workflow: {id}"))?;
            if wf.status == WorkflowStatus::Running {
                return Err(format!("Workflow {id} is already running"));
            }
            if wf.status == WorkflowStatus::Completed {
                return Err(format!("Workflow {id} already completed"));
            }
            let succeeded: Map<String, Value> = wf
                .completed_outputs
                .iter()
                .filter(|(_, v)| v["status"] == "succeeded")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let resumed_from: Vec<String> = succeeded.keys().cloned().collect();
            let cancel = Signal::new();
            wf.completed_outputs = succeeded.clone();
            wf.cancel = cancel.clone();
            wf.status = WorkflowStatus::Running;
            wf.error = None;
            wf.ended_at = None;
            wf.updated_at = self.now();
            let view = Self::managed_workflow_view(wf);
            (
                wf.nodes.clone(),
                succeeded,
                cancel,
                wf.parent.clone(),
                resumed_from,
                view,
            )
        };
        self.spawn_managed_workflow(id, nodes, initial, cancel, parent);
        let mut out = view;
        out["resumedFrom"] = json!(resumed_from);
        Ok(out)
    }
}

#[cfg(test)]
mod workflow_state_machine_tests {
    use super::*;
    use kernel::store::MemoryDocumentStore;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn test_options() -> AgentsOptions {
        let mut opts = AgentsOptions::new(Box::new(MemoryDocumentStore::default()));
        opts.definition_store = Some(Box::new(MemoryDocumentStore::default()));
        opts
    }

    fn register_det(agents: &Agents, id: &str, input_schema: Value, output_schema: Value) {
        let def = json!({
            "id": id,
            "version": "1",
            "strategy": "deterministic",
            "instructions": "test",
            "inputSchema": input_schema,
            "outputSchema": output_schema,
            "maxRepairs": 0,
            "inputDescription": "test",
            "outputDescription": "test"
        });
        agents
            .handle("agents.register", json!({ "definition": def }), "main", None)
            .unwrap();
    }

    fn call(agents: &Agents, kind: &str, body: Value) -> Result<Value, String> {
        agents.handle(kind, body, "main", None)
    }

    #[test]
    fn workflow_foreach_fans_out_and_enforces_max_items() {
        let mut opts = test_options();
        opts.deterministic = Some(Arc::new(|req| match req.definition["id"].as_str() {
            Some("list-items") => Ok(json!({ "items": [2, 3, 5] })),
            Some("double-item") => {
                let n = req.input["x"].as_i64().unwrap();
                let mul = req.input["mul"].as_i64().unwrap_or(2);
                Ok(json!({ "out": n * mul }))
            }
            other => Err(format!("unexpected def {other:?}")),
        }));
        let (agents, _rx) = Agents::new(opts);
        let any_obj = json!({ "type": "object" });
        register_det(&agents, "list-items", any_obj.clone(), any_obj.clone());
        register_det(&agents, "double-item", any_obj.clone(), any_obj);

        let result = call(
            &agents,
            "workflows.run",
            json!({
                "nodes": [
                    { "id": "src", "definition": "list-items@1", "input": {} },
                    {
                        "id": "mapped",
                        "definition": "double-item@1",
                        "dependsOn": ["src"],
                        "input": { "mul": 10 },
                        "foreach": { "from": "src", "field": "items", "as": "x", "maxItems": 8 }
                    }
                ]
            }),
        )
        .unwrap();
        assert_eq!(result["mapped"]["status"], "succeeded");
        assert_eq!(
            result["mapped"]["value"],
            json!([{ "out": 20 }, { "out": 30 }, { "out": 50 }])
        );

        let capped = call(
            &agents,
            "workflows.run",
            json!({
                "nodes": [
                    { "id": "src", "definition": "list-items@1", "input": {} },
                    {
                        "id": "mapped",
                        "definition": "double-item@1",
                        "dependsOn": ["src"],
                        "foreach": { "from": "src", "field": "items", "as": "x", "maxItems": 2 }
                    }
                ]
            }),
        )
        .unwrap();
        assert_eq!(capped["mapped"]["status"], "failed");
        assert!(capped["mapped"]["error"].as_str().unwrap().contains("maxItems"));
    }

    #[test]
    fn workflow_bounded_loop_converges_and_exhausts() {
        let mut opts = test_options();
        opts.deterministic = Some(Arc::new(|req| {
            let n = req.input["n"].as_i64().unwrap_or(0) + 1;
            Ok(json!({ "n": n, "done": n >= 3 }))
        }));
        let (agents, _rx) = Agents::new(opts);
        let schema = json!({ "type": "object", "required": ["n"] });
        register_det(&agents, "step", schema, json!({ "type": "object" }));

        let converged = call(
            &agents,
            "workflows.run",
            json!({
                "nodes": [{
                    "id": "iter",
                    "definition": "step@1",
                    "input": { "n": 0 },
                    "loop": { "maxRounds": 5, "until": { "field": "done", "equals": true } }
                }]
            }),
        )
        .unwrap();
        assert_eq!(converged["iter"]["status"], "succeeded");
        assert_eq!(converged["iter"]["value"]["n"], 3);
        assert_eq!(converged["iter"]["loop"]["outcome"], "converged");
        assert_eq!(converged["iter"]["loop"]["rounds"], 3);

        let exhausted = call(
            &agents,
            "workflows.run",
            json!({
                "nodes": [{
                    "id": "iter",
                    "definition": "step@1",
                    "input": { "n": 0 },
                    "loop": { "maxRounds": 2, "until": { "field": "done", "equals": true } }
                }]
            }),
        )
        .unwrap();
        assert_eq!(exhausted["iter"]["status"], "exhausted");
        assert_eq!(exhausted["iter"]["loop"]["outcome"], "exhausted");
        assert_eq!(exhausted["iter"]["loop"]["rounds"], 2);
    }

    #[test]
    fn workflow_async_start_status_stop_and_resume_reuses_completed_nodes() {
        let a_runs = Arc::new(AtomicUsize::new(0));
        let b_runs = Arc::new(AtomicUsize::new(0));
        let (b_started_tx, b_started_rx) = channel::<()>();
        let b_started_tx = Arc::new(Mutex::new(Some(b_started_tx)));
        let a_counter = a_runs.clone();
        let b_counter = b_runs.clone();

        let mut opts = test_options();
        opts.deterministic = Some(Arc::new(move |req| match req.definition["id"].as_str() {
            Some("node-a") => {
                a_counter.fetch_add(1, Ordering::SeqCst);
                Ok(json!({ "val": 42 }))
            }
            Some("node-b") => {
                let attempt = b_counter.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    if let Some(tx) = jsrt::sync::lock(&b_started_tx).take() {
                        let _ = tx.send(());
                    }
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while !req.signal.is_aborted() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    return Err("stopped mid-flight".into());
                }
                let v = req.input["val"].as_i64().unwrap();
                Ok(json!({ "final": v + 8 }))
            }
            other => Err(format!("unexpected {other:?}")),
        }));
        let (agents, _rx) = Agents::new(opts);
        let obj = json!({ "type": "object" });
        register_det(&agents, "node-a", obj.clone(), obj.clone());
        register_det(&agents, "node-b", obj.clone(), obj);

        let started = call(
            &agents,
            "workflows.start",
            json!({
                "nodes": [
                    { "id": "a", "definition": "node-a@1", "input": {} },
                    { "id": "b", "definition": "node-b@1", "dependsOn": ["a"], "inputFrom": "a" }
                ]
            }),
        )
        .unwrap();
        let wf_id = started["id"].as_str().unwrap().to_string();
        assert_eq!(started["status"], "running");

        b_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("node_b should start after node_a completes");

        let mid = call(&agents, "workflows.status", json!({ "id": wf_id })).unwrap();
        assert_eq!(mid["completed"], json!(["a"]));
        assert_eq!(mid["pending"], json!(["b"]));

        let stopped = call(&agents, "workflows.stop", json!({ "id": wf_id, "reason": "pause" })).unwrap();
        assert_eq!(stopped["status"], "stopped");

        let wait_stopped = Instant::now() + Duration::from_secs(5);
        while b_runs.load(Ordering::SeqCst) < 1 && Instant::now() < wait_stopped {
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(50));

        let resumed = call(&agents, "workflows.resume", json!({ "id": wf_id })).unwrap();
        assert_eq!(resumed["status"], "running");
        assert_eq!(resumed["resumedFrom"], json!(["a"]));

        let deadline = Instant::now() + Duration::from_secs(5);
        let done = loop {
            let st = call(&agents, "workflows.status", json!({ "id": wf_id })).unwrap();
            if st["status"] != "running" {
                break st;
            }
            assert!(Instant::now() < deadline, "resumed workflow did not finish: {st}");
            std::thread::sleep(Duration::from_millis(15));
        };
        assert_eq!(done["status"], "completed");
        assert_eq!(done["outputs"]["b"]["value"]["final"], 50);
        assert_eq!(a_runs.load(Ordering::SeqCst), 1, "node_a must NOT re-run on resume");
        assert_eq!(
            b_runs.load(Ordering::SeqCst),
            2,
            "node_b should run once stopped, once resumed"
        );
    }
}
