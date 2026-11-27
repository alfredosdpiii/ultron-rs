use super::*;

pub(super) struct CollectedUsage<'s> {
    pub(super) buckets: Buckets<'s>,
    pub(super) root_models: IndexMap<String, f64>,
    pub(super) child_models: HashMap<String, String>,
    pub(super) process_children: HashSet<String>,
}

fn collect_entry_usage<'a>(
    log: &'a SessionLog,
    lanes: &[Option<String>],
    lane_models: &HashMap<String, String>,
    buckets: &mut Buckets<'_>,
    root_models: &mut IndexMap<String, f64>,
) -> HashSet<&'a str> {
    let mut attributed: HashSet<&'a str> = HashSet::new();
    for (entry, lane) in log.entries.iter().zip(lanes) {
        let Some(usage) = &entry.usage else { continue };
        let lane = lane.as_deref();
        let kind = lane_kind(lane);
        if entry.kind == EntryKind::Assistant {
            let model = format!(
                "{}/{}",
                entry.provider.as_deref().unwrap_or("unknown"),
                entry.model.as_deref().unwrap_or("unknown")
            );
            buckets.add(kind, &model, usage, lane);
            if kind == Lane::Root {
                *root_models.entry(model).or_default() += 1.0;
            }
        } else {
            let model = lane
                .and_then(|l| lane_models.get(l))
                .map_or("unknown (summaries)", String::as_str);
            buckets.add(kind, model, usage, lane);
        }
        attributed.insert(&entry.id);
    }
    attributed
}

fn collect_usage_rows(
    log: &SessionLog,
    lanes: &[Option<String>],
    entry_by_id: &HashMap<&str, &LogEntry>,
    lane_models: &HashMap<String, String>,
    attributed: &HashSet<&str>,
    buckets: &mut Buckets<'_>,
) {
    let lane_of_id: HashMap<&str, Option<&str>> = log
        .entries
        .iter()
        .zip(lanes)
        .map(|(e, l)| (e.id.as_str(), l.as_deref()))
        .collect();
    for row in &log.usage_rows {
        if row.entry_id.as_deref().is_some_and(|id| attributed.contains(id)) {
            continue;
        }
        match row.entry_id.as_deref().filter(|id| entry_by_id.contains_key(id)) {
            None => buckets.add(
                Lane::Other,
                if row.adjustment {
                    "imported history"
                } else {
                    "unattributed"
                },
                &row.usage,
                None,
            ),
            Some(id) => {
                let lane = lane_of_id.get(id).copied().flatten();
                let model = lane.and_then(|l| lane_models.get(l)).map_or("unknown", String::as_str);
                buckets.add(lane_kind(lane), model, &row.usage, lane);
            }
        }
    }
}

fn measured_child_usage(measured: &Map<String, Value>) -> Option<LogUsage> {
    let mut usage = Map::new();
    for (to, from) in [
        ("input", "inputTokens"),
        ("output", "outputTokens"),
        ("totalTokens", "totalTokens"),
    ] {
        if let Some(v) = measured.get(from) {
            usage.insert(to.into(), v.clone());
        }
    }
    if let Some(cost) = measured.get("cost").filter(|c| c.is_number()) {
        usage.insert("cost".into(), json!({"total": cost}));
    }
    log_usage(Some(&Value::Object(usage)))
}

fn collect_external_children(
    tasks: &[Task],
    lane_models: &HashMap<String, String>,
    usage_roots: &Map<String, Value>,
    external: bool,
    stats: Option<&Stats>,
    buckets: &mut Buckets<'_>,
) -> (HashMap<String, String>, HashSet<String>) {
    let mut child_models: HashMap<String, String> = HashMap::new();
    let mut process_children: HashSet<String> = HashSet::new();
    if !external {
        return (child_models, process_children);
    }
    let ledger_calls: Vec<&Value> = usage_roots
        .values()
        .filter_map(|root| root.get("calls").and_then(Value::as_array))
        .flatten()
        .collect();
    for task in tasks
        .iter()
        .filter(|t| t.definition == "rlm-child" && !lane_models.contains_key(child_lane(&t.id).as_str()))
    {
        process_children.insert(task.id.clone());
        let model = stats
            .and_then(|s| s.child_models.get(&task.id).cloned())
            .unwrap_or_else(|| "claude-code/(model not recorded)".into());
        child_models.insert(task.id.clone(), model.clone());
        let call = ledger_calls.iter().find(|item| {
            item.get("kind").and_then(Value::as_str) == Some("model")
                && item.get("taskId").and_then(Value::as_str) == Some(&task.id)
        });
        let measured =
            record(call.and_then(|c| c.get("usage"))).filter(|m| m.get("totalTokens").is_some_and(Value::is_number));
        let Some(measured) = measured else {
            if task.state != "admitted" && task.state != "running" {
                buckets.unmeasured(Lane::Subagents, &model);
            }
            continue;
        };
        if let Some(usage) = measured_child_usage(measured) {
            buckets.add(Lane::Subagents, &model, &usage, Some(&child_lane(&task.id)));
        }
    }
    (child_models, process_children)
}

fn collect_durable_responses(log: &SessionLog, subscription: &HashSet<&str>, buckets: &mut Buckets<'_>) -> Bucket {
    let mut durable_other = Bucket::default();
    for response in &log.durable_responses {
        let kind = match lane_kind(response.lane.as_deref()) {
            Lane::Frames => Lane::Frames,
            _ if response.owned => Lane::Subagents,
            _ => Lane::Other,
        };
        buckets.add(kind, &response.model, &response.usage, response.lane.as_deref());
        if kind == Lane::Other {
            let notional = subscription.contains(provider_of(&response.model));
            durable_other.add(&response.usage, notional);
        }
    }
    durable_other
}

fn reconcile_ledger_usage(
    usage_roots: &Map<String, Value>,
    root_models: &IndexMap<String, f64>,
    durable_other: &Bucket,
    subscription: &HashSet<&str>,
    buckets: &mut Buckets<'_>,
) {
    let field = |root: &Value, name: &str| {
        root.get("turns")
            .and_then(|t| t.get(name))
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
    };
    let (ledger_responses, ledger_tokens, ledger_cost) =
        usage_roots.values().fold((0.0, 0.0, 0.0), |(r, t, c), root| {
            (
                r + field(root, "count"),
                t + field(root, "tokens"),
                c + field(root, "cost"),
            )
        });
    let seen = [
        &buckets.lanes[Buckets::index(Lane::Root)],
        &buckets.lanes[Buckets::index(Lane::Frames)],
        &buckets.lanes[Buckets::index(Lane::Subagents)],
        durable_other,
    ];
    let seen_tokens: f64 = seen.iter().map(|b| b.total_tokens).sum();
    let seen_responses: f64 = seen.iter().map(|b| b.responses).sum();
    let seen_cost: f64 = seen
        .iter()
        .map(|b| b.reported_usd.unwrap_or(0.0) + b.subscription_usd.unwrap_or(0.0))
        .sum();
    if ledger_tokens > seen_tokens && ledger_responses > seen_responses {
        let notional = root_models
            .keys()
            .next()
            .is_some_and(|model| subscription.contains(provider_of(model)));
        buckets.add_totals(
            Lane::Subagents,
            DURABLE_CHILDREN_MODEL,
            ledger_responses - seen_responses,
            ledger_tokens - seen_tokens,
            (ledger_cost - seen_cost).max(0.0),
            notional,
        );
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "internal report-stage helper taking disjoint precomputed tables"
)]
pub(super) fn collect_usage<'s>(
    log: &SessionLog,
    lanes: &[Option<String>],
    entry_by_id: &HashMap<&str, &LogEntry>,
    lane_models: &HashMap<String, String>,
    tasks: &[Task],
    usage_roots: &Map<String, Value>,
    external: bool,
    stats: Option<&Stats>,
    subscription: &'s HashSet<&'s str>,
) -> CollectedUsage<'s> {
    let mut buckets = Buckets {
        subscription,
        lanes: Default::default(),
        models: IndexMap::new(),
        by_lane: HashMap::new(),
        total: Bucket::default(),
    };
    let mut root_models: IndexMap<String, f64> = IndexMap::new();
    let attributed = collect_entry_usage(log, lanes, lane_models, &mut buckets, &mut root_models);
    collect_usage_rows(log, lanes, entry_by_id, lane_models, &attributed, &mut buckets);
    let (child_models, process_children) =
        collect_external_children(tasks, lane_models, usage_roots, external, stats, &mut buckets);
    let durable_other = collect_durable_responses(log, subscription, &mut buckets);
    if !external {
        reconcile_ledger_usage(usage_roots, &root_models, &durable_other, subscription, &mut buckets);
    }
    CollectedUsage {
        buckets,
        root_models,
        child_models,
        process_children,
    }
}
