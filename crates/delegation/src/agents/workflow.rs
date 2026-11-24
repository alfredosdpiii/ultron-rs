use super::*;

const WORKFLOW_FOREACH_DEFAULT_MAX: usize = 32;
const WORKFLOW_FOREACH_HARD_MAX: usize = 256;

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

type ReviseSpec = (String, Option<String>, Value, u64);
type WhenSpec = (String, Option<String>, Value);

fn parse_condition(value: &Map<String, Value>, name: &str) -> Result<(Option<String>, Value), String> {
    let equals = value
        .get("equals")
        .ok_or_else(|| format!("{name}.equals must be a JSON value"))?;
    let field = match value.get("field") {
        None => None,
        field => Some(nonempty_string(field, &format!("{name}.field"))?),
    };
    Ok((field, equals.clone()))
}

type InputFrom = Option<Result<String, Vec<String>>>;

struct ParsedDeps {
    depends_on: Vec<String>,
    input_from: InputFrom,
    join_any: bool,
}

fn parse_dependencies(node: &Map<String, Value>) -> Result<ParsedDeps, String> {
    let depends_on: Vec<String> = match node.get("dependsOn") {
        None => Vec::new(),
        Some(Value::Array(list))
            if list
                .iter()
                .all(|d| d.as_str().is_some_and(|d| !string::trim(d).is_empty()))
                && list.iter().enumerate().all(|(i, d)| !list[..i].contains(d)) =>
        {
            list.iter().filter_map(text_of).collect()
        }
        Some(_) => return Err("dependsOn must be an array of unique nonempty strings".into()),
    };
    let input_from = match node.get("inputFrom") {
        None => None,
        Some(Value::Array(list)) => {
            let sources = list
                .iter()
                .map(|s| nonempty_string(Some(s), "inputFrom entry"))
                .collect::<Result<Vec<_>, _>>()?;
            if sources.is_empty() || sources.iter().enumerate().any(|(i, s)| sources[..i].contains(s)) {
                return Err("inputFrom must be a nonempty array of unique dependencies".into());
            }
            Some(Err(sources))
        }
        value => Some(Ok(nonempty_string(value, "inputFrom")?)),
    };
    let join_any = match node.get("join") {
        None => false,
        Some(Value::String(join)) if join == "all" => false,
        Some(Value::String(join)) if join == "any" => true,
        Some(_) => return Err("join must be \"all\" or \"any\"".into()),
    };
    if join_any {
        if depends_on.len() < 2 {
            return Err("join \"any\" requires at least two dependencies".into());
        }
        if matches!(input_from, Some(Ok(_))) {
            return Err("join \"any\" requires inputFrom to be an array (fan-in of succeeded dependencies)".into());
        }
    }
    Ok(ParsedDeps {
        depends_on,
        input_from,
        join_any,
    })
}

fn parse_when(node: &Map<String, Value>, depends_on: &[String]) -> Result<Option<WhenSpec>, String> {
    let Some(value) = node.get("when") else {
        return Ok(None);
    };
    let route = object_input(Some(value))?;
    jsrt::payload::fields(route, &["node", "field", "equals"])?;
    let target = nonempty_string(route.get("node"), "when.node")?;
    if !depends_on.contains(&target) {
        return Err("when.node must name a dependency".into());
    }
    let (field, equals) = parse_condition(route, "when")?;
    Ok(Some((target, field, equals)))
}

fn parse_revise(node: &Map<String, Value>) -> Result<Option<ReviseSpec>, String> {
    let Some(value) = node.get("revise") else {
        return Ok(None);
    };
    let lp = object_input(Some(value))?;
    jsrt::payload::fields(lp, &["from", "until", "max_rounds"])?;
    let rounds = lp
        .get("max_rounds")
        .and_then(Value::as_f64)
        .filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= WORKFLOW_MAX_ROUNDS as f64)
        .ok_or_else(|| format!("revise.max_rounds must be an integer between 1 and {WORKFLOW_MAX_ROUNDS}"))?;
    let Some(until) = lp.get("until") else {
        return Err("revise.until is required".into());
    };
    let until = object_input(Some(until))?;
    jsrt::payload::fields(until, &["field", "equals"])?;
    let from = nonempty_string(lp.get("from"), "revise.from")?;
    let (field, equals) = parse_condition(until, "revise.until")?;
    Ok(Some((from, field, equals, rounds as u64)))
}

fn parse_foreach(node: &Map<String, Value>, depends_on: &[String]) -> Result<Option<ForEach>, String> {
    let Some(value) = node.get("foreach") else {
        return Ok(None);
    };
    let fe = object_input(Some(value))?;
    jsrt::payload::fields(fe, &["from", "field", "as", "maxItems"])?;
    let from = nonempty_string(fe.get("from"), "foreach.from")?;
    if !depends_on.contains(&from) {
        return Err("foreach.from must name a dependency".into());
    }
    let field = match fe.get("field") {
        None | Some(Value::Null) => None,
        f => Some(nonempty_string(f, "foreach.field")?),
    };
    let as_field = match fe.get("as") {
        None | Some(Value::Null) => None,
        a => Some(nonempty_string(a, "foreach.as")?),
    };
    let max_items = match fe.get("maxItems") {
        None | Some(Value::Null) => WORKFLOW_FOREACH_DEFAULT_MAX,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=WORKFLOW_FOREACH_HARD_MAX as u64).contains(n))
            .ok_or_else(|| format!("foreach.maxItems must be an integer between 1 and {WORKFLOW_FOREACH_HARD_MAX}"))?
            as usize,
    };
    Ok(Some(ForEach {
        from,
        field,
        as_field,
        max_items,
    }))
}

fn parse_bounded_loop(node: &Map<String, Value>) -> Result<Option<BoundedLoop>, String> {
    let Some(value) = node.get("loop") else {
        return Ok(None);
    };
    let lp = object_input(Some(value))?;
    jsrt::payload::fields(lp, &["maxRounds", "max_rounds", "until"])?;
    let raw_rounds = lp.get("maxRounds").or_else(|| lp.get("max_rounds"));
    let max_rounds = raw_rounds
        .and_then(Value::as_f64)
        .filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= WORKFLOW_MAX_ROUNDS as f64)
        .ok_or_else(|| format!("loop.maxRounds must be an integer between 1 and {WORKFLOW_MAX_ROUNDS}"))?
        as u64;
    let Some(until) = lp.get("until") else {
        return Err("loop.until is required".into());
    };
    let until = object_input(Some(until))?;
    jsrt::payload::fields(until, &["field", "equals"])?;
    let (until_field, until_equals) = parse_condition(until, "loop.until")?;
    Ok(Some(BoundedLoop {
        max_rounds,
        until_field,
        until_equals,
    }))
}

fn validate_workflow_graph(nodes: &[Node]) -> Result<(), String> {
    for (i, node) in nodes.iter().enumerate() {
        if nodes[..i].iter().any(|other| other.id == node.id) {
            return Err("Duplicate workflow node ID".into());
        }
    }
    for node in nodes {
        if node.depends_on.iter().any(|d| !nodes.iter().any(|n| &n.id == d)) {
            return Err("Unknown workflow dependency".into());
        }
    }
    let mut pending: Vec<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    while !pending.is_empty() {
        let ready: Vec<&str> = nodes
            .iter()
            .filter(|n| pending.contains(&n.id.as_str()) && n.depends_on.iter().all(|d| !pending.contains(&d.as_str())))
            .map(|n| n.id.as_str())
            .collect();
        if ready.is_empty() {
            return Err("Workflow contains a cycle".into());
        }
        pending.retain(|id| !ready.contains(id));
    }
    let ancestors = |id: &str| {
        let mut seen: Vec<String> = Vec::new();
        let mut stack = vec![id.to_string()];
        while let Some(current) = stack.pop() {
            for dependency in nodes
                .iter()
                .find(|n| n.id == current)
                .map_or(&[][..], |n| &n.depends_on[..])
            {
                if !seen.contains(dependency) {
                    seen.push(dependency.clone());
                    stack.push(dependency.clone());
                }
            }
        }
        seen
    };
    let mut reviewers: Vec<String> = Vec::new();
    for node in nodes {
        let Some((from, ..)) = &node.revise else { continue };
        let reviewer = nodes
            .iter()
            .find(|n| &n.id == from)
            .ok_or("revise.from must name a workflow node")?;
        if !reviewer.depends_on.contains(&node.id) {
            return Err("revise.from must depend on the revised node".into());
        }
        if reviewer.revise.is_some() {
            return Err("A revision reviewer cannot itself be revised".into());
        }
        if reviewer.when.is_some() {
            return Err("A revision reviewer cannot have a when route".into());
        }
        if reviewers.contains(&reviewer.id) {
            return Err("A node can review only one revision loop".into());
        }
        reviewers.push(reviewer.id.clone());
        let before = ancestors(&node.id);
        if reviewer.depends_on.iter().any(|d| d != &node.id && !before.contains(d)) {
            return Err("A revision reviewer may depend only on the revised node and its ancestors".into());
        }
    }
    Ok(())
}

impl Agents {
    fn parse_spawn_contract(
        &self,
        id: &str,
        definition: &str,
        node: &Map<String, Value>,
    ) -> Result<Option<Map<String, Value>>, String> {
        let spawn_contract: Map<String, Value> = crate::spawn_contract::SPAWN_KEYS
            .iter()
            .filter_map(|key| {
                node.get(*key)
                    .filter(|v| !v.is_null())
                    .map(|v| (key.to_string(), v.clone()))
            })
            .collect();
        if spawn_contract.is_empty() {
            return Ok(None);
        }
        if definition != "rlm-child@1" {
            return Err(format!(
                "Workflow node {id}: role, contract and hop apply only to rlm-child@1 nodes (subagents)"
            ));
        }
        if node.contains_key("revise") {
            return Err(format!("Workflow node {id}: a revised node cannot have a contract hop"));
        }
        if self.inner.worktree_children.get().is_none() {
            return Err(format!(
                "Workflow node {id}: contracts are not available in this host; spawn the subagent with rlm.spawn(contract=...)"
            ));
        }
        crate::spawn_contract::SpawnContract::parse(&spawn_contract)
            .map_err(|error| format!("Workflow node {id}: {error}"))?;
        Ok(Some(spawn_contract))
    }

    fn parse_worktree(&self, id: &str, definition: &str, node: &Map<String, Value>) -> Result<Option<Value>, String> {
        let worktree = match node.get("worktree") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => None,
            Some(mode @ Value::Bool(true)) => Some(mode.clone()),
            Some(mode @ Value::String(s)) if s == "auto" => Some(mode.clone()),
            Some(_) => {
                return Err(format!(
                    "Workflow node {id} worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)"
                ));
            }
        };
        if worktree.is_some() && definition != "rlm-child@1" {
            return Err(format!(
                "Workflow node {id}: worktree applies only to rlm-child@1 nodes (subagents)"
            ));
        }
        if worktree.is_some() && node.contains_key("revise") {
            return Err(format!(
                "Workflow node {id}: a revised node cannot use a worktree (each round would start over from your tree)"
            ));
        }
        if worktree.is_some() && self.inner.worktree_children.get().is_none() {
            return Err(format!(
                "Workflow node {id}: worktree nodes are not available in this host; spawn the subagent with rlm.spawn(worktree=True)"
            ));
        }
        Ok(worktree)
    }

    fn validate_node_input(
        &self,
        id: &str,
        node: &Map<String, Value>,
        item: &Definition,
        depends_on: &[String],
        input_from: &Option<Result<String, Vec<String>>>,
        foreach: &Option<ForEach>,
    ) -> Result<(), String> {
        match input_from {
            Some(sources) => {
                let list = match sources {
                    Ok(one) => vec![one.clone()],
                    Err(many) => many.clone(),
                };
                if list.iter().any(|s| !depends_on.contains(s)) {
                    return Err("inputFrom must name a dependency".into());
                }
                if node.contains_key("input") {
                    return Err("Specify input or inputFrom, not both".into());
                }
            }
            None if foreach.is_some() => {
                if let Some(input) = node.get("input")
                    && !input.is_object()
                {
                    return Err(format!(
                        "Workflow node {id}: foreach static input must be a JSON object"
                    ));
                }
            }
            None => {
                let Some(input) = node.get("input") else {
                    return Err("Agent input is not JSON".into());
                };
                let state = self.state();
                if !state.registry.is_valid_input(item, input) {
                    return Err(state.registry.validation_error(item, input, "input"));
                }
            }
        }
        Ok(())
    }

    fn parse_workflow_node(&self, value: &Value, workflow_key: Option<&str>) -> Result<Node, String> {
        let node = object_input(Some(value))?;
        jsrt::payload::fields(
            node,
            &[
                "id",
                "definition",
                "input",
                "model",
                "key",
                "timeout_ms",
                "dependsOn",
                "inputFrom",
                "when",
                "join",
                "revise",
                "foreach",
                "loop",
                "worktree",
                "brief",
                "role",
                "contract",
                "hop",
            ],
        )?;
        let id = nonempty_string(node.get("id"), "Workflow node ID")?;
        let definition = definition_key(node.get("definition"))?;
        let spawn_contract = self.parse_spawn_contract(&id, &definition, node)?;
        let worktree = self.parse_worktree(&id, &definition, node)?;
        let item = self.definition(&definition)?;
        self.state().registry.can_execute(&item)?;
        let ParsedDeps {
            depends_on,
            input_from,
            join_any,
        } = parse_dependencies(node)?;
        let when = parse_when(node, &depends_on)?;
        let revise = parse_revise(node)?;
        let foreach = parse_foreach(node, &depends_on)?;
        let bounded_loop = parse_bounded_loop(node)?;
        let control_count =
            usize::from(revise.is_some()) + usize::from(foreach.is_some()) + usize::from(bounded_loop.is_some());
        if control_count > 1 {
            return Err(format!(
                "Workflow node {id}: specify at most one of revise, foreach, or loop"
            ));
        }
        if foreach.is_some() && input_from.is_some() {
            return Err(format!("Workflow node {id}: foreach cannot be combined with inputFrom"));
        }
        self.validate_node_input(&id, node, &item, &depends_on, &input_from, &foreach)?;
        let (model, key, timeout_ms) = task_options(node)?;
        let brief = match node.get("brief") {
            None | Some(Value::Null) => None,
            Some(brief) => {
                Brief::parse(brief, None).map_err(|error| format!("Workflow node {id}: {error}"))?;
                Some(brief.clone())
            }
        };
        Ok(Node {
            brief,
            worktree,
            spawn_contract,
            lane: String::new(),
            key: key.or_else(|| workflow_key.map(|k| format!("{k}:{id}"))),
            id,
            definition,
            input: node.get("input").cloned(),
            depends_on,
            input_from,
            when,
            join_any,
            revise,
            foreach,
            bounded_loop,
            model,
            timeout_ms,
        })
    }

    /// `workflow(payload)`: the whole graph is validated before any effect.
    pub(super) fn workflow(&self, payload: &Map<String, Value>) -> Result<Vec<Node>, String> {
        jsrt::payload::fields(payload, &["nodes", "key"])?;
        let items = payload
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or("nodes must be an array")?;
        let workflow_key = match payload.get("key") {
            None => None,
            value => Some(nonempty_string(value, "Workflow key")?),
        };
        let nodes: Vec<Node> = items
            .iter()
            .map(|value| self.parse_workflow_node(value, workflow_key.as_deref()))
            .collect::<Result<_, _>>()?;
        validate_workflow_graph(&nodes)?;
        Ok(nodes)
    }

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
        let signal_admitted = || {
            if let Some(sender) = admitted {
                let _ = sender.send(());
            }
        };
        let definition = match self.definition(&node.definition) {
            Ok(definition) => definition,
            Err(error) => {
                signal_admitted();
                return failed(format!("Admission refused: {error}"));
            }
        };
        {
            let state = self.state();
            if !state.registry.is_valid_input(&definition, &input) {
                signal_admitted();
                return failed(format!(
                    "Bound input rejected: {}",
                    state.registry.validation_error(&definition, &input, "input")
                ));
            }
        }
        let brief = match &node.brief {
            None => None,
            Some(brief) => match Brief::parse(brief, Some(self.hop_below(parent))) {
                Ok(brief) => Some(brief),
                Err(error) => {
                    signal_admitted();
                    return failed(format!("Admission refused: {error}"));
                }
            },
        };
        let subagent = match (&node.worktree, &node.spawn_contract) {
            (Some(mode), _) => Some(mode.clone()),
            (None, Some(_)) => Some(Value::Bool(false)),
            (None, None) => None,
        };
        let estimate = self.node_estimate(node);
        if let (Some(mode), Some(children)) = (&subagent, self.inner.worktree_children.get()) {
            if let (Some(usage), Some(estimate)) = (&self.inner.usage, estimate) {
                let root = self.status_root(&self.state(), parent);
                let unit = format!("workflow node {}", node.id);
                match usage.stop_line(root.as_deref(), estimate, &unit) {
                    Ok(None) => {}
                    Ok(Some(reason)) | Err(reason) => {
                        signal_admitted();
                        return failed(format!("Admission refused: {reason}"));
                    }
                }
            }
            let base = input["prompt"].as_str().unwrap_or_default().to_string();
            let prompt = match &brief {
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
            signal_admitted();
            let result = match spawned {
                Ok(id) => children.worktree_node_result(&id, cell),
                Err(error) => return failed(format!("Admission refused: {error}")),
            };
            return if gate { verdict_gate(result) } else { result };
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
        signal_admitted();
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
        let signal_admitted = || {
            if let Some(sender) = admitted {
                let _ = sender.send(());
            }
        };
        let source = bound_value(output.get(&foreach.from));
        let target = match (&source, &foreach.field) {
            (Some(val), Some(field)) => val.get(field),
            (Some(val), None) => Some(val),
            (None, _) => None,
        };
        let Some(items) = target.and_then(Value::as_array) else {
            signal_admitted();
            return failed(format!(
                "foreach source {}{} is not a JSON array",
                foreach.from,
                foreach.field.as_ref().map_or_default(|f| format!(".{f}"))
            ));
        };
        if items.len() > foreach.max_items {
            signal_admitted();
            return failed(format!(
                "foreach exceeded maxItems ({} > {})",
                items.len(),
                foreach.max_items
            ));
        }
        if items.is_empty() {
            signal_admitted();
            return json!({ "status": "succeeded", "value": [], "items": [] });
        }
        let item_inputs: Vec<Value> = items
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
            .collect();
        signal_admitted();
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
        let all_ok = outcomes.iter().all(|o| o["status"] == "succeeded");
        if all_ok {
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
        let finish = |outcome: &str, mut result: Value, rounds: &[Value]| {
            result["loop"] = json!({
                "outcome": outcome,
                "rounds": rounds.len(),
                "maxRounds": lp.max_rounds,
            });
            result["rounds"] = Value::from(rounds.to_vec());
            result
        };
        for round in 1..=lp.max_rounds {
            if cell.is_some_and(Signal::is_aborted) {
                if round == 1
                    && let Some(sender) = admitted
                {
                    let _ = sender.send(());
                }
                return finish("cancelled", json!({ "status": "cancelled" }), &rounds);
            }
            let round_key = node.key.as_ref().map(|k| format!("{k}:loop-{round}"));
            let first_round = if round == 1 { admitted } else { None };
            let result = self.workflow_task(node, input.clone(), round_key, cell, parent, run, true, first_round);
            rounds.push(json!({ "round": round, "result": result }));
            if result["status"] != "succeeded" {
                return finish("failed", result, &rounds);
            }
            let current_val = bound_value(Some(&result));
            if condition_met(current_val.as_ref(), lp.until_field.as_deref(), &lp.until_equals) {
                return finish("converged", result, &rounds);
            }
            if round == lp.max_rounds {
                let mut exhausted = json!({ "status": "exhausted" });
                if let Some(val) = result.get("value") {
                    exhausted["value"] = val.clone();
                }
                return finish("exhausted", exhausted, &rounds);
            }
            let next_val = current_val.unwrap_or(Value::Null);
            let direct_valid = self
                .definition(&node.definition)
                .ok()
                .is_some_and(|def| self.state().registry.is_valid_input(&def, &next_val));
            input = if direct_valid {
                next_val
            } else {
                json!({
                    "input": first,
                    "previous": next_val,
                    "round": round + 1,
                })
            };
        }
        finish("exhausted", json!({ "status": "exhausted" }), &rounds)
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
            let results: Vec<Vec<(String, Value)>> = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for node in &ready {
                    let snapshot = &snapshot;
                    let (admitted, wait) = channel::<()>();
                    handles.push(scope.spawn(move || {
                        let reviewer = node
                            .revise
                            .as_ref()
                            .and_then(|(from, ..)| nodes.iter().find(|n| &n.id == from));
                        match Self::workflow_input(node, snapshot) {
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
                                out
                            }
                            Ok(input) => {
                                if let Some(reviewer) = reviewer {
                                    let (work, review) = self.workflow_loop(
                                        node,
                                        reviewer,
                                        input,
                                        snapshot,
                                        cell,
                                        parent,
                                        run,
                                        Some(&admitted),
                                    );
                                    vec![(node.id.clone(), work), (reviewer.id.clone(), review)]
                                } else if let Some(foreach) = &node.foreach {
                                    vec![(
                                        node.id.clone(),
                                        self.workflow_foreach(
                                            node,
                                            foreach,
                                            snapshot,
                                            cell,
                                            parent,
                                            run,
                                            Some(&admitted),
                                        ),
                                    )]
                                } else if let Some(lp) = &node.bounded_loop {
                                    vec![(
                                        node.id.clone(),
                                        self.workflow_bounded_loop(node, lp, input, cell, parent, run, Some(&admitted)),
                                    )]
                                } else {
                                    vec![(
                                        node.id.clone(),
                                        self.workflow_task(
                                            node,
                                            input,
                                            node.key.clone(),
                                            cell,
                                            parent,
                                            run,
                                            true,
                                            Some(&admitted),
                                        ),
                                    )]
                                }
                            }
                        }
                    }));
                    let _ = wait.recv();
                }
                handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
            });
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
