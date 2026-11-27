use super::workflow::{BoundedLoop, ForEach, Node};
use super::*;

const WORKFLOW_FOREACH_DEFAULT_MAX: usize = 32;
const WORKFLOW_FOREACH_HARD_MAX: usize = 256;

type ReviseSpec = (String, Option<String>, Value, u64);
type WhenSpec = (String, Option<String>, Value);
type InputFrom = Option<Result<String, Vec<String>>>;

struct ParsedDeps {
    depends_on: Vec<String>,
    input_from: InputFrom,
    join_any: bool,
}

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

fn parse_rounds(value: Option<&Value>, label: &str) -> Result<u64, String> {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.fract() == 0.0 && *n >= 1.0 && *n <= WORKFLOW_MAX_ROUNDS as f64)
        .map(|n| n as u64)
        .ok_or_else(|| format!("{label} must be an integer between 1 and {WORKFLOW_MAX_ROUNDS}"))
}

fn parse_depends_on(value: Option<&Value>) -> Result<Vec<String>, String> {
    match value {
        None => Ok(Vec::new()),
        Some(Value::Array(list))
            if list
                .iter()
                .all(|d| d.as_str().is_some_and(|d| !string::trim(d).is_empty()))
                && list.iter().enumerate().all(|(i, d)| !list[..i].contains(d)) =>
        {
            Ok(list.iter().filter_map(text_of).collect())
        }
        Some(_) => Err("dependsOn must be an array of unique nonempty strings".into()),
    }
}

fn parse_input_from(value: Option<&Value>) -> Result<InputFrom, String> {
    match value {
        None => Ok(None),
        Some(Value::Array(list)) => {
            let sources = list
                .iter()
                .map(|s| nonempty_string(Some(s), "inputFrom entry"))
                .collect::<Result<Vec<_>, _>>()?;
            if sources.is_empty() || sources.iter().enumerate().any(|(i, s)| sources[..i].contains(s)) {
                return Err("inputFrom must be a nonempty array of unique dependencies".into());
            }
            Ok(Some(Err(sources)))
        }
        value => Ok(Some(Ok(nonempty_string(value, "inputFrom")?))),
    }
}

fn parse_join_any(value: Option<&Value>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(Value::String(join)) if join == "all" => Ok(false),
        Some(Value::String(join)) if join == "any" => Ok(true),
        Some(_) => Err("join must be \"all\" or \"any\"".into()),
    }
}

fn parse_dependencies(node: &Map<String, Value>) -> Result<ParsedDeps, String> {
    let depends_on = parse_depends_on(node.get("dependsOn"))?;
    let input_from = parse_input_from(node.get("inputFrom"))?;
    let join_any = parse_join_any(node.get("join"))?;
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

fn parse_until_condition(map: &Map<String, Value>, label: &str) -> Result<(Option<String>, Value), String> {
    let Some(until) = map.get("until") else {
        return Err(format!("{label} is required"));
    };
    let until = object_input(Some(until))?;
    jsrt::payload::fields(until, &["field", "equals"])?;
    parse_condition(until, label)
}

fn parse_revise(node: &Map<String, Value>) -> Result<Option<ReviseSpec>, String> {
    let Some(value) = node.get("revise") else {
        return Ok(None);
    };
    let lp = object_input(Some(value))?;
    jsrt::payload::fields(lp, &["from", "until", "max_rounds"])?;
    let rounds = parse_rounds(lp.get("max_rounds"), "revise.max_rounds")?;
    let (field, equals) = parse_until_condition(lp, "revise.until")?;
    let from = nonempty_string(lp.get("from"), "revise.from")?;
    Ok(Some((from, field, equals, rounds)))
}

fn parse_optional_nonempty(map: &Map<String, Value>, key: &str, label: &str) -> Result<Option<String>, String> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        value => Ok(Some(nonempty_string(value, label)?)),
    }
}

fn parse_foreach_max_items(value: Option<&Value>) -> Result<usize, String> {
    match value {
        None | Some(Value::Null) => Ok(WORKFLOW_FOREACH_DEFAULT_MAX),
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=WORKFLOW_FOREACH_HARD_MAX as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| format!("foreach.maxItems must be an integer between 1 and {WORKFLOW_FOREACH_HARD_MAX}")),
    }
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
    Ok(Some(ForEach {
        from,
        field: parse_optional_nonempty(fe, "field", "foreach.field")?,
        as_field: parse_optional_nonempty(fe, "as", "foreach.as")?,
        max_items: parse_foreach_max_items(fe.get("maxItems"))?,
    }))
}

fn parse_bounded_loop(node: &Map<String, Value>) -> Result<Option<BoundedLoop>, String> {
    let Some(value) = node.get("loop") else {
        return Ok(None);
    };
    let lp = object_input(Some(value))?;
    jsrt::payload::fields(lp, &["maxRounds", "max_rounds", "until"])?;
    let raw_rounds = lp.get("maxRounds").or_else(|| lp.get("max_rounds"));
    let max_rounds = parse_rounds(raw_rounds, "loop.maxRounds")?;
    let (until_field, until_equals) = parse_until_condition(lp, "loop.until")?;
    Ok(Some(BoundedLoop {
        max_rounds,
        until_field,
        until_equals,
    }))
}

fn check_nodes_acyclic(nodes: &[Node]) -> Result<(), String> {
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
    Ok(())
}

fn node_ancestors(nodes: &[Node], id: &str) -> Vec<String> {
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
}

fn validate_revision_loops(nodes: &[Node]) -> Result<(), String> {
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
        let before = node_ancestors(nodes, &node.id);
        if reviewer.depends_on.iter().any(|d| d != &node.id && !before.contains(d)) {
            return Err("A revision reviewer may depend only on the revised node and its ancestors".into());
        }
    }
    Ok(())
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
    check_nodes_acyclic(nodes)?;
    validate_revision_loops(nodes)
}

fn parse_worktree_mode(id: &str, value: Option<&Value>) -> Result<Option<Value>, String> {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(None),
        Some(mode @ Value::Bool(true)) => Ok(Some(mode.clone())),
        Some(mode @ Value::String(s)) if s == "auto" => Ok(Some(mode.clone())),
        Some(_) => Err(format!(
            "Workflow node {id} worktree must be True, False or \"auto\" (each worktree child gets its own private worktree)"
        )),
    }
}

fn parse_node_brief(id: &str, value: Option<&Value>) -> Result<Option<Value>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(brief) => {
            Brief::parse(brief, None).map_err(|error| format!("Workflow node {id}: {error}"))?;
            Ok(Some(brief.clone()))
        }
    }
}

fn validate_node_controls(
    id: &str,
    revise: bool,
    foreach: bool,
    bounded_loop: bool,
    input_from: bool,
) -> Result<(), String> {
    let control_count = usize::from(revise) + usize::from(foreach) + usize::from(bounded_loop);
    if control_count > 1 {
        return Err(format!(
            "Workflow node {id}: specify at most one of revise, foreach, or loop"
        ));
    }
    if foreach && input_from {
        return Err(format!("Workflow node {id}: foreach cannot be combined with inputFrom"));
    }
    Ok(())
}

struct NodeControls {
    deps: ParsedDeps,
    when: Option<WhenSpec>,
    revise: Option<ReviseSpec>,
    foreach: Option<ForEach>,
    bounded_loop: Option<BoundedLoop>,
}

fn parse_node_controls(id: &str, node: &Map<String, Value>) -> Result<NodeControls, String> {
    let deps = parse_dependencies(node)?;
    let when = parse_when(node, &deps.depends_on)?;
    let revise = parse_revise(node)?;
    let foreach = parse_foreach(node, &deps.depends_on)?;
    let bounded_loop = parse_bounded_loop(node)?;
    validate_node_controls(
        id,
        revise.is_some(),
        foreach.is_some(),
        bounded_loop.is_some(),
        deps.input_from.is_some(),
    )?;
    Ok(NodeControls {
        deps,
        when,
        revise,
        foreach,
        bounded_loop,
    })
}

struct NodeHeader {
    id: String,
    definition: String,
    item: Definition,
    spawn_contract: Option<Map<String, Value>>,
    worktree: Option<Value>,
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
        let Some(worktree) = parse_worktree_mode(id, node.get("worktree"))? else {
            return Ok(None);
        };
        if definition != "rlm-child@1" {
            return Err(format!(
                "Workflow node {id}: worktree applies only to rlm-child@1 nodes (subagents)"
            ));
        }
        if node.contains_key("revise") {
            return Err(format!(
                "Workflow node {id}: a revised node cannot use a worktree (each round would start over from your tree)"
            ));
        }
        if self.inner.worktree_children.get().is_none() {
            return Err(format!(
                "Workflow node {id}: worktree nodes are not available in this host; spawn the subagent with rlm.spawn(worktree=True)"
            ));
        }
        Ok(Some(worktree))
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

    fn parse_node_header(&self, node: &Map<String, Value>) -> Result<NodeHeader, String> {
        let id = nonempty_string(node.get("id"), "Workflow node ID")?;
        let definition = definition_key(node.get("definition"))?;
        let spawn_contract = self.parse_spawn_contract(&id, &definition, node)?;
        let worktree = self.parse_worktree(&id, &definition, node)?;
        let item = self.definition(&definition)?;
        self.state().registry.can_execute(&item)?;
        Ok(NodeHeader {
            id,
            definition,
            item,
            spawn_contract,
            worktree,
        })
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
        let header = self.parse_node_header(node)?;
        let controls = parse_node_controls(&header.id, node)?;
        self.validate_node_input(
            &header.id,
            node,
            &header.item,
            &controls.deps.depends_on,
            &controls.deps.input_from,
            &controls.foreach,
        )?;
        let (model, key, timeout_ms) = task_options(node)?;
        let brief = parse_node_brief(&header.id, node.get("brief"))?;
        Ok(Node {
            brief,
            worktree: header.worktree,
            spawn_contract: header.spawn_contract,
            lane: String::new(),
            key: key.or_else(|| workflow_key.map(|k| format!("{k}:{}", header.id))),
            id: header.id,
            definition: header.definition,
            input: node.get("input").cloned(),
            depends_on: controls.deps.depends_on,
            input_from: controls.deps.input_from,
            when: controls.when,
            join_any: controls.deps.join_any,
            revise: controls.revise,
            foreach: controls.foreach,
            bounded_loop: controls.bounded_loop,
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
}
