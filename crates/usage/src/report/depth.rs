use super::*;

fn frame_level(
    trace: &Map<String, Value>,
    seen: &mut HashSet<String>,
    levels: &mut HashMap<String, f64>,
    traces: &IndexMap<String, &Map<String, Value>>,
    level_of_task: &dyn Fn(Option<&str>) -> f64,
) -> f64 {
    let id = trace.get("id").map_or_default(|v| jsrt::value::string(Some(v)));
    if let Some(known) = levels.get(&id) {
        return *known;
    }
    seen.insert(id.clone());
    let parent = trace
        .get("parentFrame")
        .and_then(Value::as_str)
        .filter(|p| !seen.contains(*p))
        .and_then(|p| traces.get(p).copied());
    let level = match parent {
        Some(parent) => frame_level(parent, seen, levels, traces, level_of_task) + 1.0,
        None => level_of_task(trace.get("callerTaskId").and_then(Value::as_str)) + 1.0,
    };
    levels.insert(id, level);
    level
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal report-stage helper taking disjoint precomputed tables"
)]
pub(super) fn build_depth(
    log: &SessionLog,
    tasks: &[Task],
    task_by_id: &HashMap<&str, &Task>,
    lane_models: &HashMap<String, String>,
    buckets: &Buckets<'_>,
    child_models: &HashMap<String, String>,
    process_children: &HashSet<String>,
    stats: Option<&Stats>,
    stats_cover: bool,
    unrecorded: &mut Map<String, Value>,
) -> Value {
    let children: Vec<&Task> = tasks.iter().filter(|t| t.definition == "rlm-child").collect();
    let child_depth = |task: &Task| -> f64 {
        let mut depth = 0.0;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut current = Some(task);
        while let Some(c) = current {
            if seen.contains(c.id.as_str()) {
                break;
            }
            seen.insert(&c.id);
            if c.definition == "rlm-child" {
                depth += 1.0;
            }
            current = c.parent_id.as_deref().and_then(|p| task_by_id.get(p).copied());
        }
        depth
    };
    let level_of_task =
        |id: Option<&str>| -> f64 { id.and_then(|i| task_by_id.get(i)).map_or(0.0, |t| child_depth(t)) };
    let state =
        |items: &[&Task], states: &[&str]| items.iter().filter(|t| states.contains(&t.state.as_str())).count() as f64;
    let mut verdicts = [0.0; 6];
    let mut claims = [0.0; 3];
    let mut worktrees = Vec::new();
    let mut child_by_model: IndexMap<String, ModelCount> = IndexMap::new();
    let mut durable_models: HashMap<&str, &str> = HashMap::new();
    for response in &log.durable_responses {
        if let Some(lane) = &response.lane {
            durable_models.entry(lane.as_str()).or_insert(response.model.as_str());
        }
    }
    for child in &children {
        let lane = child_lane(&child.id);
        let model = lane_models
            .get(lane.as_str())
            .cloned()
            .or_else(|| durable_models.get(lane.as_str()).map(std::string::ToString::to_string))
            .or_else(|| child_models.get(child.id.as_str()).cloned())
            .or_else(|| stats.and_then(|s| s.child_models.get(&child.id).cloned()))
            .unwrap_or_else(|| "not recorded".into());
        let tokens = buckets.by_lane.get(&lane).map(|b| b.total_tokens);
        let by_model = child_by_model.entry(model).or_default();
        by_model.count += 1.0;
        by_model.tokens += tokens.unwrap_or(0.0);
        if tokens.is_none() && process_children.contains(child.id.as_str()) && child.result.is_some() {
            by_model.unmeasured += 1.0;
        }
        let Some(result) = &child.result else { continue };
        let verdict = record(result.get("verdict"));
        let outcome = record(result.get("check"))
            .and_then(|c| c.get("outcome"))
            .and_then(Value::as_str);
        match (verdict, outcome) {
            (None, Some("invalid")) => verdicts[3] += 1.0,
            (None, _) => verdicts[5] += 1.0,
            (Some(_), Some("verified")) => verdicts[0] += 1.0,
            (Some(_), Some("contradicted")) => verdicts[1] += 1.0,
            (Some(_), _) => verdicts[4] += 1.0,
        }
        match verdict.and_then(|v| v.get("status")).and_then(Value::as_str) {
            Some("passed") => claims[0] += 1.0,
            Some("failed") => claims[1] += 1.0,
            Some("blocked") => claims[2] += 1.0,
            _ => {}
        }
        let worktree = record(result.get("worktree"));
        if let Some(w) = worktree
            && let Some(branch) = w.get("branch").and_then(Value::as_str)
        {
            let merge = stats.and_then(|s| s.merges.get(&child.id).cloned()).map_or_else(
                || {
                    if w.get("commit") == Some(&Value::Null) {
                        Value::String("empty".into())
                    } else {
                        Value::Null
                    }
                },
                Value::String,
            );
            worktrees.push(json!({
                "task": child.id,
                "branch": branch,
                "changedFiles": jsrt::json::number(w.get("changed_files").and_then(Value::as_array).map_or(0, Vec::len) as f64),
                "merge": merge,
            }));
        }
    }
    verdicts[2] = verdicts[3] + verdicts[4] + verdicts[5];
    let depths: Vec<f64> = children.iter().map(|c| child_depth(c)).collect();
    let max_child_depth = depths.iter().fold(0.0_f64, |m, d| m.max(*d));

    let mut trace_by_id: IndexMap<String, &Map<String, Value>> = IndexMap::new();
    for (key, value) in log.values_of(ns::RLM_FRAMES) {
        if key == "index" {
            continue;
        }
        if let Some(trace) = value.as_object()
            && let Some(id) = trace.get("id").and_then(Value::as_str)
        {
            trace_by_id.insert(id.to_string(), trace);
        }
    }
    let frame_tasks: Vec<&Task> = tasks.iter().filter(|t| t.definition == "rlm-frame").collect();
    let mut traced: HashSet<&str> = HashSet::new();
    let mut frames = [0.0; 7];
    let mut incomplete_reasons: IndexMap<String, f64> = IndexMap::new();
    let mut budget_tokens = 0.0;
    let mut frame_by_model: IndexMap<String, ModelCount> = IndexMap::new();
    let mut frame_levels: HashMap<String, f64> = HashMap::new();
    let mut max_frame_level: f64 = 0.0;
    let count_frame = |model: String, lane: Option<&str>, by_model: &mut IndexMap<String, ModelCount>| {
        let tokens = lane.and_then(|l| buckets.by_lane.get(l)).map(|b| b.total_tokens);
        let entry = by_model.entry(model).or_default();
        entry.count += 1.0;
        entry.tokens += tokens.unwrap_or(0.0);
    };
    for trace in trace_by_id.values() {
        frames[0] += 1.0;
        if let Some(task_id) = trace.get("taskId").and_then(Value::as_str) {
            traced.insert(task_id);
        }
        match trace.get("status").and_then(Value::as_str) {
            Some("complete") => frames[1] += 1.0,
            Some("incomplete") => {
                frames[2] += 1.0;
                let reason = trace.get("reason").and_then(Value::as_str).unwrap_or("unknown");
                *incomplete_reasons.entry(reason.to_string()).or_default() += 1.0;
            }
            Some("error") => frames[3] += 1.0,
            _ => frames[5] += 1.0,
        }
        if let Some(spent) = record(trace.get("spent"))
            .and_then(|s| s.get("tokens"))
            .and_then(Value::as_f64)
            && spent.is_finite()
        {
            budget_tokens += spent;
        }
        let level = frame_level(
            trace,
            &mut HashSet::new(),
            &mut frame_levels,
            &trace_by_id,
            &level_of_task,
        );
        if level > 1.0 {
            frames[6] += 1.0;
        }
        max_frame_level = max_frame_level.max(level);
        let lane = match (
            trace.get("lane").and_then(Value::as_str),
            trace.get("taskId").and_then(Value::as_str),
        ) {
            (Some(lane), _) => Some(lane.to_string()),
            (None, Some(task)) => Some(frame_lane(task)),
            _ => None,
        };
        let model = trace
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| lane.as_deref().and_then(|l| lane_models.get(l).cloned()))
            .unwrap_or_else(|| "not recorded".into());
        count_frame(model, lane.as_deref(), &mut frame_by_model);
    }
    for task in &frame_tasks {
        if traced.contains(task.id.as_str()) {
            continue;
        }
        frames[0] += 1.0;
        match task.state.as_str() {
            "completed" => frames[1] += 1.0,
            "cancelled" | "interrupted" => frames[4] += 1.0,
            "failed" => frames[3] += 1.0,
            _ => frames[5] += 1.0,
        }
        let level = level_of_task(task.parent_id.as_deref()) + 1.0;
        if level > 1.0 {
            frames[6] += 1.0;
        }
        max_frame_level = max_frame_level.max(level);
        let lane = frame_lane(&task.id);
        let model = lane_models
            .get(lane.as_str())
            .cloned()
            .unwrap_or_else(|| "not recorded".into());
        count_frame(model, Some(&lane), &mut frame_by_model);
    }

    let typed: Vec<&Task> = tasks
        .iter()
        .filter(|t| !["rlm-child", "rlm-frame", "background-job"].contains(&t.definition.as_str()))
        .collect();
    let mut by_definition: IndexMap<String, f64> = IndexMap::new();
    for task in &typed {
        *by_definition.entry(task.definition.clone()).or_default() += 1.0;
    }
    let background: Vec<&Task> = tasks.iter().filter(|t| t.definition == "background-job").collect();
    let other_level = typed
        .iter()
        .chain(&background)
        .fold(0.0_f64, |m, t| m.max(level_of_task(t.parent_id.as_deref()) + 1.0));
    let level = max_child_depth.max(max_frame_level).max(other_level);
    let nested_children = depths.iter().filter(|d| **d > 1.0).count() as f64;
    if !stats_cover {
        let reason = if stats.is_none() {
            "this session predates Ultron's runtime counters"
        } else {
            "the runtime counters began after this session did"
        };
        unrecorded.insert("depth.workflows".into(), reason.into());
        unrecorded.insert("depth.frames.calls".into(), reason.into());
    }
    let mut frames_value = json!({
        "count": jsrt::json::number(frames[0]),
        "complete": jsrt::json::number(frames[1]),
        "incomplete": jsrt::json::number(frames[2]),
        "failed": jsrt::json::number(frames[3]),
        "cancelled": jsrt::json::number(frames[4]),
        "running": jsrt::json::number(frames[5]),
        "nested": jsrt::json::number(frames[6]),
        "incompleteReasons": counts_value(&incomplete_reasons),
        "budgetTokens": jsrt::json::number(budget_tokens),
        "byModel": by_model_list(frame_by_model),
    });
    let host_call = |name: &str| jsrt::json::number(stats.and_then(|s| s.host_calls.get(name).copied()).unwrap_or(0.0));
    frames_value["calls"] = if stats_cover {
        json!({"infer": host_call("rlm.infer"), "map": host_call("rlm.map")})
    } else {
        Value::Null
    };
    json!({
        "verdict": depth_verdict(
            level,
            frames[0],
            children.len() as f64,
            nested_children,
            typed.len() as f64,
            background.len() as f64,
        ),
        "level": jsrt::json::number(level),
        "frames": frames_value,
        "subagents": {
            "count": jsrt::json::number(children.len() as f64),
            "maxDepth": jsrt::json::number(max_child_depth),
            "nested": jsrt::json::number(nested_children),
            "completed": jsrt::json::number(state(&children, &["completed"])),
            "failed": jsrt::json::number(state(&children, &["failed"])),
            "cancelled": jsrt::json::number(state(&children, &["cancelled"])),
            "interrupted": jsrt::json::number(state(&children, &["interrupted"])),
            "running": jsrt::json::number(state(&children, &["admitted", "running"])),
            "byModel": by_model_list(child_by_model),
            "verdicts": {
                "verified": jsrt::json::number(verdicts[0]),
                "contradicted": jsrt::json::number(verdicts[1]),
                "unverified": jsrt::json::number(verdicts[2]),
                "invalid": jsrt::json::number(verdicts[3]),
                "unchecked": jsrt::json::number(verdicts[4]),
                "none": jsrt::json::number(verdicts[5]),
            },
            "claims": {"passed": jsrt::json::number(claims[0]), "failed": jsrt::json::number(claims[1]), "blocked": jsrt::json::number(claims[2])},
            "worktrees": worktrees,
        },
        "workflows": if stats_cover { json!({"runs": host_call("workflows.run")}) } else { Value::Null },
        "typedAgents": {"count": jsrt::json::number(typed.len() as f64), "byDefinition": counts_value(&by_definition)},
        "backgroundJobs": {
            "count": jsrt::json::number(background.len() as f64),
            "completed": jsrt::json::number(state(&background, &["completed"])),
            "failed": jsrt::json::number(state(&background, &["failed"])),
            "cancelled": jsrt::json::number(state(&background, &["cancelled", "interrupted"])),
            "running": jsrt::json::number(state(&background, &["admitted", "running"])),
        },
    })
}
