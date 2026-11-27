use super::*;

#[derive(Clone, Debug)]
pub(super) struct ForEach {
    pub(super) from: String,
    pub(super) field: Option<String>,
    pub(super) as_field: Option<String>,
    pub(super) max_items: usize,
}

#[derive(Clone, Debug)]
pub(super) struct BoundedLoop {
    pub(super) max_rounds: u64,
    pub(super) until_field: Option<String>,
    pub(super) until_equals: Value,
}

/// One validated workflow node.
#[derive(Clone, Debug)]
pub(super) struct Node {
    pub(super) id: String,
    pub(super) definition: String,
    pub(super) input: Option<Value>,
    pub(super) depends_on: Vec<String>,
    /// One dependency, or several (fan-in).
    pub(super) input_from: Option<Result<String, Vec<String>>>,
    pub(super) when: Option<(String, Option<String>, Value)>,
    pub(super) join_any: bool,
    pub(super) revise: Option<(String, Option<String>, Value, u64)>,
    pub(super) foreach: Option<ForEach>,
    pub(super) bounded_loop: Option<BoundedLoop>,
    pub(super) model: Option<String>,
    pub(super) key: Option<String>,
    pub(super) timeout_ms: u64,
    /// The node's brief as written (its hop is the workflow caller's plus one).
    pub(super) brief: Option<Value>,
    /// `worktree=True` or `"auto"`: an `rlm-child@1` node that runs as a subagent in its own worktree.
    pub(super) worktree: Option<Value>,
    /// `role`, `contract`, `hop` of an `rlm-child@1` node: it runs as a subagent whose contract the subagent host
    /// checks (as `rlm.spawn(role=, contract=, hop=)`; see [`crate::spawn_contract`]).
    pub(super) spawn_contract: Option<Map<String, Value>>,
    /// The lane that ran `workflows.run` (a worktree node is that lane's subagent).
    pub(super) lane: String,
}

fn notify_admitted(admitted: Option<&Sender<()>>) {
    if let Some(sender) = admitted {
        let _ = sender.send(());
    }
}

fn resolve_foreach_items<'a>(foreach: &ForEach, source: &'a Option<Value>) -> Result<&'a Vec<Value>, Value> {
    let target = match (source, &foreach.field) {
        (Some(val), Some(field)) => val.get(field),
        (Some(val), None) => Some(val),
        (None, _) => None,
    };
    let Some(items) = target.and_then(Value::as_array) else {
        return Err(failed(format!(
            "foreach source {}{} is not a JSON array",
            foreach.from,
            foreach.field.as_ref().map_or_default(|f| format!(".{f}"))
        )));
    };
    if items.len() > foreach.max_items {
        return Err(failed(format!(
            "foreach exceeded maxItems ({} > {})",
            items.len(),
            foreach.max_items
        )));
    }
    Ok(items)
}

fn build_foreach_inputs(node: &Node, foreach: &ForEach, items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .map(|item| match (&node.input, &foreach.as_field) {
            (Some(Value::Object(base)), as_name) => {
                let mut merged = base.clone();
                merged.insert(as_name.clone().unwrap_or_else(|| "item".to_string()), item.clone());
                Value::Object(merged)
            }
            (None, Some(as_name)) => json!({ as_name: item }),
            _ => item.clone(),
        })
        .collect()
}

fn summarize_foreach_outcomes(outcomes: Vec<Value>) -> Value {
    if outcomes.iter().all(|o| o["status"] == "succeeded") {
        let values: Vec<Value> = outcomes
            .iter()
            .map(|o| bound_value(Some(o)).unwrap_or(Value::Null))
            .collect();
        json!({ "status": "succeeded", "value": values, "items": outcomes })
    } else {
        let first_err = outcomes
            .iter()
            .find(|o| o["status"] != "succeeded")
            .and_then(|o| o.get("error").and_then(Value::as_str))
            .unwrap_or("one or more foreach items failed");
        json!({ "status": "failed", "error": first_err, "items": outcomes })
    }
}

fn finish_bounded_loop(outcome: &str, mut result: Value, rounds: &[Value], max_rounds: u64) -> Value {
    result["loop"] = json!({
        "outcome": outcome,
        "rounds": rounds.len(),
        "maxRounds": max_rounds,
    });
    result["rounds"] = Value::from(rounds.to_vec());
    result
}

impl Agents {
    /// `workflowInput`: the input a ready node runs with, or the explicit outcome that replaces running it.
    fn workflow_input(node: &Node, output: &Map<String, Value>) -> Result<Value, Value> {
        let status = |d: &str| output[d]["status"].as_str().unwrap_or_default().to_string();
        if !node.join_any {
            if let Some(unmet) = node.depends_on.iter().find(|d| status(d) != "succeeded") {
                return Err(json!({ "status": "skipped",
                    "reason": format!("Dependency {unmet} did not succeed ({})", status(unmet)) }));
            }
        } else if !node.depends_on.iter().any(|d| status(d) == "succeeded") {
            let listed: Vec<String> = node.depends_on.iter().map(|d| format!("{d}: {}", status(d))).collect();
            return Err(json!({ "status": "skipped",
                "reason": format!("No dependency succeeded ({})", listed.join(", ")) }));
        }
        if let Some((target, field, equals)) = &node.when {
            let label = match field {
                None => target.clone(),
                Some(field) => format!("{target}.{field}"),
            };
            if status(target) != "succeeded" {
                return Err(json!({ "status": "skipped",
                    "reason": format!("Route dependency {target} did not succeed ({})", status(target)) }));
            }
            if !condition_met(bound_value(output.get(target)).as_ref(), field.as_deref(), equals) {
                return Err(json!({ "status": "skipped", "reason": format!("Route condition on {label} not met") }));
            }
        }
        Ok(match &node.input_from {
            None => node.input.clone().unwrap_or(Value::Null),
            Some(Ok(source)) => bound_value(output.get(source)).unwrap_or(Value::Null),
            Some(Err(sources)) => Value::Object(
                sources
                    .iter()
                    .filter(|s| status(s) == "succeeded")
                    .map(|s| (s.clone(), bound_value(output.get(s)).unwrap_or(Value::Null)))
                    .collect(),
            ),
        })
    }

    fn prepare_workflow_task(&self, node: &Node, input: &Value, parent: Option<&str>) -> Result<Option<Brief>, Value> {
        let definition = self
            .definition(&node.definition)
            .map_err(|error| failed(format!("Admission refused: {error}")))?;
        {
            let state = self.state();
            if !state.registry.is_valid_input(&definition, input) {
                return Err(failed(format!(
                    "Bound input rejected: {}",
                    state.registry.validation_error(&definition, input, "input")
                )));
            }
        }
        match &node.brief {
            None => Ok(None),
            Some(brief) => Brief::parse(brief, Some(self.hop_below(parent)))
                .map(Some)
                .map_err(|error| failed(format!("Admission refused: {error}"))),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn run_worktree_node(
        &self,
        children: &crate::host::Delegation,
        node: &Node,
        mode: &Value,
        input: &Value,
        brief: Option<&Brief>,
        estimate: Option<f64>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        gate: bool,
        admitted: Option<&Sender<()>>,
    ) -> Value {
        if let (Some(usage), Some(estimate)) = (&self.inner.usage, estimate) {
            let root = self.status_root(&self.state(), parent);
            let unit = format!("workflow node {}", node.id);
            match usage.stop_line(root.as_deref(), estimate, &unit) {
                Ok(None) => {}
                Ok(Some(reason)) | Err(reason) => {
                    notify_admitted(admitted);
                    return failed(format!("Admission refused: {reason}"));
                }
            }
        }
        let base = input["prompt"].as_str().unwrap_or_default().to_string();
        let prompt = match brief {
            Some(brief) => brief.prompt(&base),
            None => base,
        };
        let spawned = children.spawn_worktree_node(
            &node.lane,
            &node.id,
            &prompt,
            node.model.as_deref(),
            node.timeout_ms,
            mode,
            node.spawn_contract.as_ref().unwrap_or(&Map::new()),
        );
        notify_admitted(admitted);
        let result = match spawned {
            Ok(id) => children.worktree_node_result(&id, cell),
            Err(error) => return failed(format!("Admission refused: {error}")),
        };
        if gate { verdict_gate(result) } else { result }
    }

    /// `workflowTask`: validates a bound input, admits the node's task, and waits for its durable result.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn workflow_task(
        &self,
        node: &Node,
        input: Value,
        key: Option<String>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        gate: bool,
        admitted: Option<&Sender<()>>,
    ) -> Value {
        let brief = match self.prepare_workflow_task(node, &input, parent) {
            Ok(brief) => brief,
            Err(err) => {
                notify_admitted(admitted);
                return err;
            }
        };
        let subagent = match (&node.worktree, &node.spawn_contract) {
            (Some(mode), _) => Some(mode.clone()),
            (None, Some(_)) => Some(Value::Bool(false)),
            (None, None) => None,
        };
        let estimate = self.node_estimate(node);
        if let (Some(mode), Some(children)) = (&subagent, self.inner.worktree_children.get()) {
            return self.run_worktree_node(
                children,
                node,
                mode,
                &input,
                brief.as_ref(),
                estimate,
                cell,
                parent,
                gate,
                admitted,
            );
        }
        let request = TaskRequest {
            definition: node.definition.clone(),
            input,
            model: node.model.clone(),
            key,
            timeout_ms: node.timeout_ms,
            workflow: Some(json!({ "run": run, "node": node.id, "dependsOn": node.depends_on,
                                   "join": if node.join_any { "any" } else { "all" } })),
            brief,
            estimate_usd: estimate,
            ..TaskRequest::default()
        };
        let spawned = self.spawn_task(request, cell, parent, None);
        notify_admitted(admitted);
        let id = match spawned {
            Ok(id) => id,
            Err(error) => return failed(format!("Admission refused: {error}")),
        };
        let result = match self.result_of(&id, cell.map(|c| (c, true))) {
            Ok(result) => result,
            Err(error) => failed(error),
        };
        if gate { verdict_gate(result) } else { result }
    }

    /// `workflowLoop`: the revised node, then its reviewer, until `until` holds or the rounds run out.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn workflow_loop(
        &self,
        work: &Node,
        reviewer: &Node,
        first: Value,
        output: &Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        admitted: Option<&Sender<()>>,
    ) -> (Value, Value) {
        let Some((_, until_field, until_equals, max_rounds)) = work.revise.clone() else {
            return (failed("not a revision loop"), failed("not a revision loop"));
        };
        let round_key = |node: &Node, round: u64| node.key.as_ref().map(|k| format!("{k}:round-{round}"));
        let mut rounds: Vec<Value> = Vec::new();
        let finish = |outcome: &str, mut work_result: Value, review: Value, rounds: &[Value]| {
            work_result["revision"] = json!({ "outcome": outcome, "rounds": rounds.len(), "max_rounds": max_rounds });
            work_result["rounds"] = Value::from(rounds.to_vec());
            (work_result, review)
        };
        let mut input = first.clone();
        let mut round = 1;
        loop {
            let first_round = if round == 1 { admitted } else { None };
            let work_result = self.workflow_task(
                work,
                input.clone(),
                round_key(work, round),
                cell,
                parent,
                run,
                true,
                first_round,
            );
            if work_result["status"] != "succeeded" {
                let skipped = json!({ "status": "skipped",
                    "reason": format!("Dependency {} did not succeed ({})", work.id, work_result["status"].as_str().unwrap_or_default()) });
                rounds.push(json!({ "round": round, "work": work_result }));
                return finish("failed", work_result, skipped, &rounds);
            }
            let mut view = output.clone();
            view.insert(work.id.clone(), work_result.clone());
            let review = match Self::workflow_input(reviewer, &view) {
                Err(outcome) => outcome,
                Ok(review_input) => self.workflow_task(
                    reviewer,
                    review_input,
                    round_key(reviewer, round),
                    cell,
                    parent,
                    run,
                    false,
                    None,
                ),
            };
            rounds.push(json!({ "round": round, "work": work_result, "review": review }));
            if review["status"] != "succeeded" {
                let mut failure = json!({ "status": "failed" });
                if let Some(value) = work_result.get("value") {
                    failure["value"] = value.clone();
                }
                failure["error"] = json!(format!(
                    "Reviewer {} did not succeed in round {round} ({})",
                    reviewer.id,
                    review["status"].as_str().unwrap_or_default()
                ));
                failure["verification"] = json!("unverified");
                return finish("failed", failure, review, &rounds);
            }
            if condition_met(
                bound_value(Some(&review)).as_ref(),
                until_field.as_deref(),
                &until_equals,
            ) {
                return finish("converged", work_result, review, &rounds);
            }
            if round == max_rounds {
                let mut exhausted = json!({ "status": "exhausted" });
                if let Some(value) = work_result.get("value") {
                    exhausted["value"] = value.clone();
                }
                exhausted["verification"] = json!("unverified");
                return finish("exhausted", exhausted, review, &rounds);
            }
            input = json!({ "input": first, "previous": bound_value(Some(&work_result)).unwrap_or(Value::Null),
                            "review": bound_value(Some(&review)).unwrap_or(Value::Null), "round": round + 1 });
            round += 1;
        }
    }

    /// `workflowForeach`: fans out `node` over an array from `foreach.from` (up to `max_items`) in parallel.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn workflow_foreach(
        &self,
        node: &Node,
        foreach: &ForEach,
        output: &Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        admitted: Option<&Sender<()>>,
    ) -> Value {
        let source = bound_value(output.get(&foreach.from));
        let items = match resolve_foreach_items(foreach, &source) {
            Ok(items) => items,
            Err(err) => {
                notify_admitted(admitted);
                return err;
            }
        };
        if items.is_empty() {
            notify_admitted(admitted);
            return json!({ "status": "succeeded", "value": [], "items": [] });
        }
        let item_inputs = build_foreach_inputs(node, foreach, items);
        notify_admitted(admitted);
        let outcomes: Vec<Value> = std::thread::scope(|scope| {
            let handles: Vec<_> = item_inputs
                .into_iter()
                .enumerate()
                .map(|(idx, input)| {
                    let item_key = node.key.as_ref().map(|k| format!("{k}:item-{idx}"));
                    scope.spawn(move || self.workflow_task(node, input, item_key, cell, parent, run, true, None))
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| failed("foreach worker panicked")))
                .collect()
        });
        summarize_foreach_outcomes(outcomes)
    }

    fn next_loop_input(&self, node: &Node, first: &Value, current_val: Option<Value>, round: u64) -> Value {
        let next_val = current_val.unwrap_or(Value::Null);
        let direct_valid = self
            .definition(&node.definition)
            .ok()
            .is_some_and(|def| self.state().registry.is_valid_input(&def, &next_val));
        if direct_valid {
            next_val
        } else {
            json!({
                "input": first,
                "previous": next_val,
                "round": round + 1,
            })
        }
    }

    /// `workflowBoundedLoop`: repeats a single node up to `max_rounds` until `until` matches its output.
    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn workflow_bounded_loop(
        &self,
        node: &Node,
        lp: &BoundedLoop,
        first: Value,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        admitted: Option<&Sender<()>>,
    ) -> Value {
        let mut rounds: Vec<Value> = Vec::new();
        let mut input = first.clone();
        for round in 1..=lp.max_rounds {
            if cell.is_some_and(Signal::is_aborted) {
                if round == 1 {
                    notify_admitted(admitted);
                }
                return finish_bounded_loop("cancelled", json!({ "status": "cancelled" }), &rounds, lp.max_rounds);
            }
            let round_key = node.key.as_ref().map(|k| format!("{k}:loop-{round}"));
            let first_round = if round == 1 { admitted } else { None };
            let result = self.workflow_task(node, input.clone(), round_key, cell, parent, run, true, first_round);
            rounds.push(json!({ "round": round, "result": result }));
            if result["status"] != "succeeded" {
                return finish_bounded_loop("failed", result, &rounds, lp.max_rounds);
            }
            let current_val = bound_value(Some(&result));
            if condition_met(current_val.as_ref(), lp.until_field.as_deref(), &lp.until_equals) {
                return finish_bounded_loop("converged", result, &rounds, lp.max_rounds);
            }
            if round == lp.max_rounds {
                let mut exhausted = json!({ "status": "exhausted" });
                if let Some(val) = result.get("value") {
                    exhausted["value"] = val.clone();
                }
                return finish_bounded_loop("exhausted", exhausted, &rounds, lp.max_rounds);
            }
            input = self.next_loop_input(node, &first, current_val, round);
        }
        finish_bounded_loop("exhausted", json!({ "status": "exhausted" }), &rounds, lp.max_rounds)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal wiring helper; parameters match distinct caller inputs"
    )]
    fn execute_ready_node(
        &self,
        node: &Node,
        nodes: &[Node],
        snapshot: &Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        run: &str,
        admitted: &Sender<()>,
    ) -> Vec<(String, Value)> {
        let reviewer = node
            .revise
            .as_ref()
            .and_then(|(from, ..)| nodes.iter().find(|n| &n.id == from));
        let input = match Self::workflow_input(node, snapshot) {
            Ok(input) => input,
            Err(outcome) => {
                let _ = admitted.send(());
                let mut out = vec![(node.id.clone(), outcome.clone())];
                if let Some(reviewer) = reviewer {
                    out.push((
                        reviewer.id.clone(),
                        json!({ "status": "skipped",
                            "reason": format!("Dependency {} did not succeed ({})", node.id,
                                outcome["status"].as_str().unwrap_or_default()) }),
                    ));
                }
                return out;
            }
        };
        if let Some(reviewer) = reviewer {
            let (work, review) = self.workflow_loop(node, reviewer, input, snapshot, cell, parent, run, Some(admitted));
            return vec![(node.id.clone(), work), (reviewer.id.clone(), review)];
        }
        let outcome = if let Some(foreach) = &node.foreach {
            self.workflow_foreach(node, foreach, snapshot, cell, parent, run, Some(admitted))
        } else if let Some(lp) = &node.bounded_loop {
            self.workflow_bounded_loop(node, lp, input, cell, parent, run, Some(admitted))
        } else {
            self.workflow_task(node, input, node.key.clone(), cell, parent, run, true, Some(admitted))
        };
        vec![(node.id.clone(), outcome)]
    }

    fn record_workflow_results(
        &self,
        run: &str,
        managed_id: Option<&str>,
        results: Vec<Vec<(String, Value)>>,
        output: &mut Map<String, Value>,
    ) {
        let mut state = self.state();
        let now = self.now();
        for (id, result) in results.into_iter().flatten() {
            let reason = result
                .get("reason")
                .and_then(Value::as_str)
                .or_else(|| result.get("error").and_then(Value::as_str));
            if let Some(record) = state.workflow_runs.iter_mut().find(|r| r["run"] == run) {
                let mut ended = json!({ "status": result["status"] });
                if let Some(reason) = reason {
                    ended["reason"] = json!(json::preview(&json!(reason), GRAPH_TEXT_CHARS));
                }
                record["ended"][&id] = ended;
            }
            if let Some(mid) = managed_id
                && let Some(wf) = state.managed_workflows.get_mut(mid)
            {
                wf.completed_outputs.insert(id.clone(), result.clone());
                wf.updated_at = now;
            }
            output.insert(id, result);
        }
    }

    pub(super) fn execute_workflow_graph(
        &self,
        run: &str,
        nodes: &[Node],
        initial: Map<String, Value>,
        cell: Option<&Signal>,
        parent: Option<&str>,
        managed_id: Option<&str>,
    ) -> Map<String, Value> {
        let reviewers: Vec<String> = nodes
            .iter()
            .filter_map(|n| n.revise.as_ref().map(|(from, ..)| from.clone()))
            .collect();
        let mut output = initial;
        while output.len() < nodes.len() {
            if cell.is_some_and(Signal::is_aborted) {
                break;
            }
            let ready: Vec<&Node> = nodes
                .iter()
                .filter(|n| {
                    !output.contains_key(&n.id)
                        && !reviewers.contains(&n.id)
                        && n.depends_on.iter().all(|d| output.contains_key(d))
                })
                .collect();
            if ready.is_empty() {
                break;
            }
            let snapshot = output.clone();
            let results: Vec<Vec<(String, Value)>> =
                std::thread::scope(|scope| {
                    let mut handles = Vec::new();
                    for node in &ready {
                        let snapshot = &snapshot;
                        let (admitted, wait) = channel::<()>();
                        handles.push(scope.spawn(move || {
                            self.execute_ready_node(node, nodes, snapshot, cell, parent, run, &admitted)
                        }));
                        let _ = wait.recv();
                    }
                    handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
                });
            self.record_workflow_results(run, managed_id, results, &mut output);
        }
        output
    }

    pub(super) fn record_workflow_start(&self, run: &str, nodes: &[Node], parent: Option<&str>) {
        let mut state = self.state();
        let mut record = json!({ "run": run });
        if let Some(parent) = parent {
            record["parentId"] = json!(parent);
        }
        record["startedAt"] = json::number(self.now());
        record["nodes"] = Value::from(
            nodes
                .iter()
                .map(|n| {
                    json!({ "id": n.id, "definition": n.definition, "dependsOn": n.depends_on,
                                 "join": if n.join_any { "any" } else { "all" } })
                })
                .collect::<Vec<_>>(),
        );
        record["ended"] = json!({});
        state.workflow_runs.push(record);
        let excess = state.workflow_runs.len().saturating_sub(GRAPH_WORKFLOW_LIMIT);
        state.workflow_runs.drain(..excess);
    }

    /// `runWorkflow`: ready nodes run together; a dependency is satisfied only by its durable terminal result.
    pub(super) fn run_workflow(
        &self,
        nodes: Vec<Node>,
        cell: Option<&Signal>,
        parent: Option<&str>,
    ) -> Result<Value, String> {
        let run = format!("wf-{}", &random::uuid_v4()[..8]);
        self.record_workflow_start(&run, &nodes, parent);
        let output = self.execute_workflow_graph(&run, &nodes, Map::new(), cell, parent, None);
        let mut state = self.state();
        let now = self.now();
        if let Some(record) = state.workflow_runs.iter_mut().find(|r| r["run"] == run.as_str()) {
            record["endedAt"] = json::number(now);
        }
        Ok(Value::Object(output))
    }
}

/// `graphWorkflow`: a run's planned nodes (bounded) and how the finished ones ended.
pub(super) fn graph_workflow(record: &Value) -> Value {
    let nodes = record["nodes"].as_array().cloned().unwrap_or_default();
    let shown: Vec<Value> = nodes
        .iter()
        .take(GRAPH_WORKFLOW_NODE_LIMIT)
        .map(|node| {
            let mut node = node.clone();
            if let Some(ended) = record["ended"]
                .get(node["id"].as_str().unwrap_or_default())
                .and_then(Value::as_object)
            {
                for (key, value) in ended {
                    node[key] = value.clone();
                }
            }
            node
        })
        .collect();
    let mut out = json!({ "run": record["run"] });
    if let Some(parent) = record.get("parentId") {
        out["parentId"] = parent.clone();
    }
    out["startedAt"] = record["startedAt"].clone();
    if let Some(ended) = record.get("endedAt") {
        out["endedAt"] = ended.clone();
    }
    out["nodes"] = Value::from(shown);
    if nodes.len() > GRAPH_WORKFLOW_NODE_LIMIT {
        out["truncatedNodes"] = json!(nodes.len() - GRAPH_WORKFLOW_NODE_LIMIT);
    }
    out
}
