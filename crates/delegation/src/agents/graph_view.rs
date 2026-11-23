use super::*;
use crate::estimate::Unit;

pub(super) const GRAPH_TASK_LIMIT: usize = 300;
pub(super) const GRAPH_PREVIEW_CHARS: usize = 240;
pub(super) const GRAPH_TEXT_CHARS: usize = 160;
pub(super) const GRAPH_WORKFLOW_LIMIT: usize = 20;
pub(super) const GRAPH_WORKFLOW_NODE_LIMIT: usize = 64;
const CONTROLS: [&str; 7] = [
    "permissionPrompts",
    "riskBlocking",
    "capabilityEnforcement",
    "budgetEnforcement",
    "completionGates",
    "refinementApproval",
    "sandboxRequired",
];

pub(super) fn fetch_hint(definition: &str, id: &str) -> String {
    match definition {
        "rlm-child@1" => format!("await rlm.collect([\"{id}\"])"),
        "background-job@1" => format!("await background.result(\"{id}\")"),
        _ => format!("await agents.result(\"{id}\")"),
    }
}

/// `publicTask`: id, definition, state, parentId?, result?.
pub(super) fn public_task(record: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "definition", "state", "parentId", "result"] {
        if let Some(value) = record.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// `publicRecord`: id, key, fingerprint, definition, state, result?, parentId?.
pub(super) fn public_record(record: &Value) -> Value {
    let mut out = Map::new();
    for key in ["id", "key", "fingerprint", "definition", "state", "result", "parentId"] {
        if let Some(value) = record.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

impl Agents {
    /// `budget.estimate`: a plan's cost (`{plan}`), or a workflow's (`{workflow: {nodes, key?}}`, validated as
    /// `workflows.run` validates it), beside the cost cap and call ceilings of the caller's root.
    pub(super) fn estimate(&self, payload: &Map<String, Value>, parent: Option<&str>) -> Result<Value, String> {
        jsrt::payload::fields(payload, &["plan", "workflow"])?;
        let units = match (payload.get("plan"), payload.get("workflow")) {
            (Some(plan), None) => crate::estimate::parse_plan(plan)?,
            (None, Some(Value::Object(workflow))) => self.workflow_units(&self.workflow(workflow)?),
            _ => return Err("budget.estimate takes a plan or a workflow ({nodes}), not both".into()),
        };
        let status = match &self.inner.usage {
            Some(usage) => {
                let root = self.status_root(&self.state(), parent);
                Some(usage.status(root.as_deref())?)
            }
            None => None,
        };
        Ok(crate::estimate::estimate(
            &units,
            self.inner.pricing.as_ref(),
            status.as_ref(),
        ))
    }

    /// The model work of a node: a subagent (`child`), a frame, one completion (`chat`), or none (deterministic).
    fn node_unit(&self, node: &Node, calls: u64) -> Option<Unit> {
        let kind = match self.definition(&node.definition).ok()?.strategy() {
            Strategy::Deterministic => return None,
            Strategy::Predict => "chat",
            Strategy::Rlm if node.definition == "rlm-frame@1" => "frame",
            Strategy::Rlm => "child",
        };
        let mut unit = Unit::new(kind, calls, node.model.clone());
        unit.node = Some(node.id.clone());
        Some(unit)
    }

    /// A workflow's units: each node once, a revision or bounded loop's node once per round at most.
    fn workflow_units(&self, nodes: &[Node]) -> Vec<Unit> {
        let rounds = |node: &Node| {
            if let Some(lp) = &node.bounded_loop {
                return lp.max_rounds;
            }
            nodes
                .iter()
                .find_map(|n| {
                    n.revise
                        .as_ref()
                        .filter(|r| n.id == node.id || r.0 == node.id)
                        .map(|r| r.3)
                })
                .unwrap_or(1)
        };
        nodes
            .iter()
            .filter_map(|node| self.node_unit(node, rounds(node)))
            .collect()
    }

    /// What one run of `node` is expected to cost, when its model has a price.
    pub(super) fn node_estimate(&self, node: &Node) -> Option<f64> {
        let unit = self.node_unit(node, 1)?;
        let pricing = self.inner.pricing.as_ref()?;
        crate::estimate::call_cost(&unit, &pricing(&unit.kind, unit.model.as_deref()))
    }

    pub(super) fn status(
        &self,
        payload: &Map<String, Value>,
        lane: &str,
        parent: Option<&str>,
        records: &[Value],
    ) -> Result<Value, String> {
        let root = {
            let state = self.state();
            self.status_root(&state, parent)
        };
        let usage = match &self.inner.usage {
            Some(ledger) => Some(ledger.status(root.as_deref())?),
            None => None,
        };
        let state = self.state();
        let shown: Vec<&Value> = records
            .iter()
            .filter(|r| Self::visible(records, r["id"].as_str().unwrap_or_default(), parent))
            .collect();
        let graph = payload.get("graph") == Some(&Value::Bool(true));
        let listed = if graph && shown.len() > GRAPH_TASK_LIMIT {
            &shown[shown.len() - GRAPH_TASK_LIMIT..]
        } else {
            &shown[..]
        };
        let tasks: Vec<Value> = listed
            .iter()
            .map(|record| {
                if graph {
                    let id = record["id"].as_str().unwrap_or_default();
                    self.graph_task(state.task(id), record)
                } else {
                    public_task(record)
                }
            })
            .collect();
        let mut out = Map::new();
        out.insert("definitions".into(), Value::from(state.registry.list()));
        out.insert("tasks".into(), Value::from(tasks));
        if graph && shown.len() > listed.len() {
            out.insert("truncatedTasks".into(), json!(shown.len() - listed.len()));
        }
        if graph {
            let workflows: Vec<Value> = state
                .workflow_runs
                .iter()
                .filter(|run| match run["parentId"].as_str() {
                    None => parent.is_none(),
                    Some(owner) => Self::visible(records, owner, parent),
                })
                .map(graph_workflow)
                .collect();
            out.insert("workflows".into(), Value::from(workflows));
        }
        out.insert("usage".into(), usage.clone().unwrap_or(Value::Null));
        out.insert(
            "limits".into(),
            usage.as_ref().map_or(Value::Null, |u| u["limits"].clone()),
        );
        out.insert(
            "spend".into(),
            match &usage {
                None => Value::Null,
                Some(u) => json!({
                    "root": u["rootId"], "turns": u["turns"]["turns"], "tokens": u["turns"]["tokens"],
                    "costUsd": u["cost"]["spentUsd"], "maxTotalTurns": u["limits"]["maxTotalTurns"],
                    "maxTotalTokens": u["limits"]["maxTotalTokens"], "maxCostUsd": u["limits"]["maxCostUsd"],
                }),
            },
        );
        drop(state);
        if let Some(extras) = &self.inner.status_extras {
            out.extend(extras(lane));
        }
        out.insert(
            "controls".into(),
            Value::Object(CONTROLS.iter().map(|k| (k.to_string(), json!(false))).collect()),
        );
        Ok(Value::Object(out))
    }

    /// `graphTask`: the bounded graph view of a task.
    fn graph_task(&self, task: Option<&Task>, record: &Value) -> Value {
        let mut out = public_task(record);
        if let Some(map) = out.as_object_mut() {
            map.shift_remove("result");
        }
        let id = record["id"].as_str().unwrap_or_default();
        let definition = record["definition"].as_str().unwrap_or_default();
        if let Some(task) = task {
            let mut put = |key: &str, value: Option<Value>| {
                if let Some(value) = value {
                    out[key] = value;
                }
            };
            put("lane", task.lane.clone().map(Value::from));
            put("startedAt", task.started_at.map(json::number));
            put("endedAt", task.ended_at.map(json::number));
            put("cost", task.cost.map(json::number));
            put("tokens", task.tokens.map(json::number));
            put("input", task.input_preview.clone().map(Value::from));
            put("workflow", task.workflow.clone());
            put("model", task.model.clone().map(Value::from));
            put("turns", task.turns.map(Value::from));
            put("toolCallCount", task.tool_calls.map(Value::from));
            put("lastText", task.last_text.clone().map(Value::from));
        }
        if let Some(result) = record.get("result") {
            let mut view = json!({ "status": result["status"] });
            if let Some(error) = result.get("error") {
                view["error"] = json!(json::preview(error, GRAPH_PREVIEW_CHARS));
            }
            if let Some(value) = result.get("value") {
                view["preview"] = json!(json::preview(value, GRAPH_PREVIEW_CHARS));
            }
            out["result"] = view;
        }
        out["fetch"] = json!(fetch_hint(definition, id));
        out
    }
}
