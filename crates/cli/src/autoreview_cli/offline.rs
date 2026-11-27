//! Offline review, benchmark eval, JSON formatting, and label/register commands for `ultron autoreview`.

use super::*;

/// `describeModels`: the model each stage runs on, with its thinking level.
pub fn describe_models(config: &AutoreviewConfig) -> Vec<String> {
    let finder = config
        .model
        .clone()
        .unwrap_or_else(|| "the session's default model".into());
    let stage = |name: &str, model: &Option<String>, thinking: &str| {
        format!(
            "  {name}: {} (thinking {thinking})",
            model.clone().unwrap_or_else(|| finder.clone())
        )
    };
    let recommended = config
        .model
        .as_deref()
        .is_some_and(|model| RECOMMENDED_MODELS.contains(&model));
    vec![
        format!(
            "Models{}:",
            if recommended {
                " (the recommended set; it thinks high at every stage)"
            } else {
                ""
            }
        ),
        if config.finder_models.is_empty() {
            stage("finders", &config.model, &config.thinking)
        } else {
            format!(
                "  finders: {} in turn (thinking {})",
                config.finder_models.join(", "),
                config.thinking
            )
        },
        stage("investigators", &config.deep_model, &config.deep_thinking),
        stage("verifier", &config.verify_model, &config.verify_thinking),
        stage("planner (compiled)", &config.plan_model, &config.plan_thinking),
        stage("questions (compiled)", &config.ask_model, &config.ask_thinking),
    ]
}

/// `describeStages`: "; map 1 s, tests 20 s, find 31 s" from the per-stage milliseconds (zero stages left out).
pub fn describe_stages(stages: Option<&Map<String, Value>>) -> String {
    let Some(stages) = stages else {
        return String::new();
    };
    let parts: Vec<String> = stages
        .iter()
        .filter_map(|(name, ms)| {
            let ms = ms.as_f64().filter(|ms| *ms > 0.0)?;
            let name = name.strip_suffix("Ms").unwrap_or(name);
            Some(format!("{name} {} s", jsrt::number::round(ms / 1000.0)))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("; {}", parts.join(", "))
    }
}

/// Present (even null): the TS `x === undefined ? {} : {key: x}`.
fn put_present(out: &mut Map<String, Value>, key: &str, from: &Map<String, Value>, field: &str) {
    if let Some(value) = from.get(field) {
        out.insert(key.into(), value.clone());
    }
}

/// `from[key]` copied when `wanted` says it carries something.
fn put_when(out: &mut Map<String, Value>, key: &str, from: &Map<String, Value>, wanted: impl Fn(&Value) -> bool) {
    if let Some(value) = from.get(key).filter(|value| wanted(value)) {
        out.insert(key.into(), value.clone());
    }
}

/// `x ?? fallback`: a missing or null value takes the fallback.
fn or_else(from: &Map<String, Value>, field: &str, fallback: Value) -> Value {
    match from.get(field) {
        None | Some(Value::Null) => fallback,
        Some(value) => value.clone(),
    }
}

fn object<'a>(value: Option<&'a Value>, empty: &'a Map<String, Value>) -> &'a Map<String, Value> {
    value.and_then(Value::as_object).unwrap_or(empty)
}

fn array_copy(value: Option<&Value>) -> Value {
    match value {
        Some(Value::Array(items)) => Value::Array(items.clone()),
        _ => Value::Array(Vec::new()),
    }
}

fn format_offline_finding(
    index: usize,
    finding: &Value,
    inline: &HashSet<usize>,
    in_body: &HashSet<usize>,
    ranks: &HashMap<usize, usize>,
    empty: &Map<String, Value>,
) -> Value {
    let finding = object(Some(finding), empty);
    let text = |field: &str| finding.get(field).and_then(Value::as_str);
    let level = level_of(text("level"), text("severity"));
    let mut out = Map::new();
    let posted = if inline.contains(&index) {
        "inline"
    } else if in_body.contains(&index) {
        "body"
    } else {
        "counted"
    };
    out.insert("posted".into(), posted.into());
    out.insert("rank".into(), ranks.get(&index).map_or(Value::Null, |rank| json!(rank)));
    put_present(&mut out, "unpinned", finding, "unpinned");
    put_present(&mut out, "consequence", finding, "consequence");
    if finding.get("unclear") == Some(&Value::Bool(true)) {
        out.insert("unclear".into(), Value::Bool(true));
    }
    put_present(&mut out, "file", finding, "file");
    put_present(&mut out, "line", finding, "line");
    put_present(&mut out, "endLine", finding, "endLine");
    out.insert("severity".into(), severity_of(level).into());
    out.insert("level".into(), level.into());
    out.insert(
        "finderSeverity".into(),
        or_else(finding, "finderSeverity", severity_of(level).into()),
    );
    out.insert("finderLevel".into(), or_else(finding, "finderLevel", level.into()));
    put_present(&mut out, "verifierLevel", finding, "verifierLevel");
    put_present(&mut out, "verifierScenarioHolds", finding, "verifierScenarioHolds");
    put_present(&mut out, "levelReason", finding, "levelReason");
    out.insert("scenario".into(), or_else(finding, "scenario", "".into()));
    put_present(&mut out, "category", finding, "category");
    put_present(&mut out, "claim", finding, "claim");
    put_present(&mut out, "why", finding, "why");
    put_present(&mut out, "suggestedFix", finding, "suggestedFix");
    put_present(&mut out, "verification", finding, "verification");
    put_present(&mut out, "confidence", finding, "confidence");
    put_present(&mut out, "alsoAt", finding, "alsoAt");
    out.insert("source".into(), or_else(finding, "source", "fast".into()));
    put_present(&mut out, "verifiedBy", finding, "verifiedBy");
    out.insert("evidence".into(), or_else(finding, "evidence", "".into()));
    out.insert("howVerified".into(), or_else(finding, "howVerified", "".into()));
    Value::Object(out)
}

fn format_offline_dropped(result: &Map<String, Value>, empty: &Map<String, Value>) -> Map<String, Value> {
    let dropped_in = object(result.get("dropped"), empty);
    let mut dropped = Map::new();
    put_present(&mut dropped, "rejected", dropped_in, "rejected");
    put_present(&mut dropped, "duplicates", dropped_in, "duplicates");
    dropped.insert("generic".into(), or_else(dropped_in, "generic", json!(0)));
    dropped.insert("refutedByTest".into(), or_else(dropped_in, "refutedByTest", json!(0)));
    dropped.insert("duplicateOf".into(), or_else(dropped_in, "duplicateOf", json!([])));
    put_present(&mut dropped, "preexisting", dropped_in, "preexisting");
    put_when(&mut dropped, "refutedByInvestigator", dropped_in, |entries| {
        entries.as_array().is_some_and(|items| !items.is_empty())
    });
    put_when(&mut dropped, "contradicted", dropped_in, |count| {
        count.as_u64().is_some_and(|n| n > 0)
    });
    dropped
}

fn format_offline_timing(timing_in: &Map<String, Value>, startup_ms: Option<u64>) -> Map<String, Value> {
    let mut timing = Map::new();
    for key in ["totalMs", "scopeMs", "findMs", "verifyMs"] {
        put_present(&mut timing, key, timing_in, key);
    }
    if let Some(ms) = startup_ms {
        timing.insert("startupMs".into(), json!(ms));
    }
    if let Some(stages) = timing_in.get("stages").filter(|stages| !stages.is_null()) {
        timing.insert("stages".into(), stages.clone());
    }
    timing.insert("frames".into(), array_copy(timing_in.get("frames")));
    timing.insert("investigators".into(), array_copy(timing_in.get("investigators")));
    if let Some(reference) = timing_in
        .get("reference")
        .filter(|value| jsrt::value::truthy(Some(value)))
    {
        timing.insert("reference".into(), reference.clone());
    }
    timing
}

fn format_offline_usage(result: &Map<String, Value>, empty: &Map<String, Value>) -> Map<String, Value> {
    let usage_in = object(result.get("usage"), empty);
    let mut usage = Map::new();
    put_present(&mut usage, "inputTokens", usage_in, "inputTokens");
    put_present(&mut usage, "outputTokens", usage_in, "outputTokens");
    put_present(&mut usage, "costUsd", usage_in, "costUsd");
    put_present(&mut usage, "frames", usage_in, "frames");
    if let Some(by_phase) = usage_in.get("byPhase") {
        usage.insert("byPhase".into(), by_phase.clone());
    }
    put_when(&mut usage, "judge", usage_in, |judge| {
        judge["calls"].as_u64().is_some_and(|n| n > 0)
    });
    usage
}

/// `offlineJson`: the JSON `review --repo-dir ... --json` prints, with where each finding would go (inline, named in
/// the body, or only counted) computed by the same ranking and cap the poster uses.
pub fn offline_json(
    raw: &Value,
    startup_ms: Option<u64>,
    block_at: Option<&str>,
    max_comments: Option<i64>,
) -> Result<Value, String> {
    let empty = Map::new();
    let result = object(Some(raw), &empty);
    let typed = EngineResult::parse(raw)?;
    let verdict = decide_verdict(&typed, block_at).verdict;
    let options = PlanOptions {
        self_authored: false,
        state: "open".into(),
        head_sha: "0".repeat(40),
        since_sha: None,
        signature: false,
        block_at: block_at.map(str::to_string),
        never_request_changes: false,
        max_comments,
        promotion: None,
        fresh_lines: None,
    };
    let plan = plan_review(&typed, &options);
    let inline: HashSet<usize> = plan.comments.iter().map(|comment| comment.finding).collect();
    let in_body: HashSet<usize> = plan.in_summary.iter().copied().collect();
    let ranks: HashMap<usize, usize> = rank_findings(&typed)
        .into_iter()
        .enumerate()
        .map(|(position, index)| (index, position + 1))
        .collect();
    let findings: Vec<Value> = result
        .get("findings")
        .and_then(Value::as_array)
        .map_or_default(Vec::as_slice)
        .iter()
        .enumerate()
        .map(|(index, finding)| format_offline_finding(index, finding, &inline, &in_body, &ranks, &empty))
        .collect();
    let dropped = format_offline_dropped(result, &empty);
    let timing_in = object(result.get("timing"), &empty);
    let timing = format_offline_timing(timing_in, startup_ms);
    let usage = format_offline_usage(result, &empty);
    let assurance: Vec<String> = result
        .get("assurance")
        .and_then(Value::as_array)
        .map_or_default(|items| items.iter().map(js_text).collect());
    let program = match result.get("program") {
        None | Some(Value::Null) => Value::Null,
        Some(program) => {
            let mut program = program.as_object().cloned().unwrap_or_default();
            program.insert("steps".into(), array_copy(timing_in.get("program")));
            Value::Object(program)
        }
    };
    let mut out = Map::new();
    out.insert("verdict".into(), serde_json::to_value(verdict).unwrap_or(Value::Null));
    put_present(&mut out, "complete", result, "complete");
    out.insert("findings".into(), Value::Array(findings));
    put_present(&mut out, "preexisting", result, "preexisting");
    out.insert("dropped".into(), Value::Object(dropped));
    out.insert("timing".into(), Value::Object(timing));
    out.insert("usage".into(), Value::Object(usage));
    put_present(&mut out, "model", result, "model");
    put_present(&mut out, "verifyModel", result, "verifyModel");
    if let Some(models) = result.get("finderModels").filter(|models| models.is_array()) {
        out.insert("finderModels".into(), models.clone());
    }
    out.insert("mode".into(), or_else(result, "mode", "fast".into()));
    out.insert("assurance".into(), assurance.join(" ").into());
    out.insert(
        "tests".into(),
        or_else(
            result,
            "tests",
            json!({"enabled": false, "mechanism": null, "note": null, "runs": []}),
        ),
    );
    for key in [
        "deepModel",
        "deepThinking",
        "thinking",
        "verifyThinking",
        "planModel",
        "planThinking",
        "askModel",
        "askThinking",
        "planStyle",
        "verification",
    ] {
        out.insert(key.into(), or_else(result, key, Value::Null));
    }
    out.insert("program".into(), program);
    put_when(&mut out, "static", result, |found| {
        found["findings"].as_u64().is_some_and(|n| n > 0)
            || found["contradicted"].as_array().is_some_and(|items| !items.is_empty())
    });
    put_when(&mut out, "blind", result, |blind| {
        blind["asked"].as_u64().is_some_and(|n| n > 0)
    });
    out.insert("notChecked".into(), array_copy(result.get("notChecked")));
    Ok(Value::Object(out))
}

/// `String(value)` for an array element joined by `Array.join`.
pub(super) fn js_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => jsrt::json::stringify(other),
    }
}

/// The knowledge graph of `repo_dir` at `head` into `spec`, under the repository's name when one is given; a
/// failure leaves the review without it, and a directory that is not a git checkout is not indexed.
pub(super) fn index_knowledge(
    parsed: &Parsed,
    db: &str,
    repo_dir: &Path,
    head: &str,
    spec: &mut Value,
    err: &mut dyn Write,
) {
    if !repo_dir.join(".git").exists() {
        return;
    }
    let knowledge_repo = match &parsed.repo {
        Some(repo) => format!("github.com/{repo}").to_lowercase(),
        None => format!("local:{}", repo_dir.display()),
    };
    match knowledge::index_tree(Path::new(db), &knowledge_repo, repo_dir, head) {
        Ok(stats) => {
            if let Value::Object(map) = spec {
                map.insert(
                    "knowledge".into(),
                    json!({ "db": db, "repo": knowledge_repo, "rev": stats.rev }),
                );
            }
        }
        Err(error) => {
            let _ = writeln!(err, "the knowledge graph was not updated: {error}");
        }
    }
}

fn write_register_proposals(
    paths: &review::settings::AutoreviewPaths,
    proposals: &[review::register::Proposal],
) -> Result<PathBuf, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let file = Path::new(&paths.dir).join(review::register::PROPOSALS_FILE);
    if !proposals.is_empty() {
        let written = json!({
            "note": "Loki rules proposed from review rules that findings keep citing and people keep accepting. \
                     Nothing here is applied: add an approved tuple to loki-guardrails rules.",
            "proposals": proposals,
        });
        std::fs::create_dir_all(&paths.dir).map_err(io)?;
        std::fs::write(&file, jsrt::json::stringify_indented(&written, " ")).map_err(io)?;
    }
    Ok(file)
}

fn format_register_lines(
    register: &review::register::Register,
    proposals: &[review::register::Proposal],
    file: &Path,
) -> Vec<String> {
    let proven = register.checks.iter().filter(|check| check.proven).count();
    let mut lines = vec![format!(
        "{} recorded review(s), {} fate(s); {proven} of {} checks count (each caught its planted bug).",
        register.reviews,
        register.fates,
        register.checks.len()
    )];
    lines.extend(register.checks.iter().filter(|check| !check.proven).map(|check| {
        format!(
            "  not counted: {} ({}): it did not catch its planted bug",
            check.id, check.rule
        )
    }));
    if register.modes.is_empty() {
        lines.push("No failure mode recorded.".into());
    }
    for mode in &register.modes {
        lines.push(format!("{:>5}  {}  [{}]", mode.count, mode.description, mode.source));
        lines.extend(mode.examples.iter().map(|example| format!("         {example}")));
    }
    if !proposals.is_empty() {
        lines.push(format!(
            "{} Loki rule(s) proposed, written to {}:",
            proposals.len(),
            file.display()
        ));
        lines.extend(proposals.iter().map(|proposal| {
            format!(
                "  {}: {} (accepted {}, rejected {}){}",
                proposal.rule,
                proposal.message,
                proposal.accepted,
                proposal.rejected,
                if proposal.pattern.is_none() {
                    "; needs a pattern"
                } else {
                    ""
                }
            )
        }));
    }
    lines
}

/// `register`: the failure-mode register and the Loki proposals (written beside the state, never applied).
pub(super) fn register_command(
    parsed: &Parsed,
    paths: &review::settings::AutoreviewPaths,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let repo = parsed.repo.as_deref().map(review::register::repo_key);
    let (register, proposals) = review::register::from_disk(paths, repo.as_deref());
    let file = write_register_proposals(paths, &proposals)?;
    if parsed.json {
        let value = json!({ "register": register, "proposals": proposals });
        writeln!(out, "{}", jsrt::json::stringify(&value)).map_err(io)?;
        return Ok(0);
    }
    for line in format_register_lines(&register, &proposals, &file) {
        writeln!(out, "{line}").map_err(io)?;
    }
    Ok(0)
}

fn load_label_cases(
    path: &Path,
) -> Result<Vec<(review::eval_labels::LabelCase, review::eval_labels::Recorded)>, Failure> {
    let text = std::fs::read_to_string(path).map_err(|error| Failure::Error(format!("{}: {error}", path.display())))?;
    let labels: review::eval_labels::Labels =
        serde_json::from_str(&text).map_err(|error| Failure::Error(format!("{}: {error}", path.display())))?;
    let base = path.parent().map_or_default(PathBuf::from);
    let mut cases = Vec::new();
    for case in labels.cases {
        let file = base.join(&case.record);
        let raw: Value = std::fs::read_to_string(&file)
            .map_err(|error| error.to_string())
            .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
            .map_err(|error| Failure::Error(format!("{}: {error}", file.display())))?;
        let recorded = review::eval_labels::parse_record(&raw)
            .map_err(|error| Failure::Error(format!("{}: {error}", file.display())))?;
        cases.push((case, recorded));
    }
    Ok(cases)
}

fn write_lines(out: &mut dyn Write, lines: impl IntoIterator<Item = String>) -> Result<(), Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    for line in lines {
        writeln!(out, "{line}").map_err(io)?;
    }
    Ok(())
}

fn write_label_score_result(
    parsed: &Parsed,
    repo_given: bool,
    result: Result<Value, Vec<String>>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    match result {
        Ok(scored) if parsed.json => {
            writeln!(out, "{scored}").map_err(io)?;
            Ok(0)
        }
        Ok(scored) => {
            if !repo_given {
                writeln!(
                    err,
                    "no repository given: the fixes were not checked to come after their heads"
                )
                .map_err(io)?;
            }
            write_lines(out, review::eval_labels::summary_lines(&scored))?;
            Ok(0)
        }
        Err(problems) => {
            write_lines(err, problems)?;
            Ok(1)
        }
    }
}

/// `eval --labels <file> [<repo-dir>]`: recorded reviews scored against adjudicated labels (review::eval_labels), with
/// no review and no model call. `--packet` prints the blinded findings to label instead.
pub(super) fn labels_command(
    parsed: &Parsed,
    env: &Env,
    ov: &Overrides,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    use review::eval_labels::{packet, score};
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let path = absolute(&env.cwd, parsed.labels.as_deref().unwrap_or_default());
    let cases = load_label_cases(&path)?;
    if parsed.packet {
        let packets: Vec<Value> = cases.iter().map(|(_, recorded)| packet(recorded)).collect();
        writeln!(out, "{:#}", json!({ "cases": packets })).map_err(io)?;
        return Ok(0);
    }
    let repo = parsed
        .target
        .as_deref()
        .map(|target| absolute(&env.cwd, target).display().to_string());
    let runner = ov.runner();
    let result = score(&cases, repo.as_deref().map(|repo| (&*runner, repo)));
    write_label_score_result(parsed, repo.is_some(), result, out, err)
}

fn case_kind_label(kind: review::eval::CaseKind) -> &'static str {
    match kind {
        review::eval::CaseKind::Reverted => "reverted",
        review::eval::CaseKind::FixInducing => "fix-inducing",
        review::eval::CaseKind::Control => "control",
    }
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

fn filter_eval_cases(cases: &mut Vec<review::eval::Case>, only: Option<&[String]>, err: &mut dyn Write) -> bool {
    let Some(only) = only else {
        return true;
    };
    cases.retain(|case| {
        only.iter()
            .any(|prefix| !prefix.is_empty() && case.sha.starts_with(prefix.as_str()))
    });
    if cases.is_empty() {
        let _ = writeln!(err, "no case's commit starts with {}", only.join(", "));
        return false;
    }
    true
}

fn write_eval_list(parsed: &Parsed, cases: &[review::eval::Case], out: &mut dyn Write) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        writeln!(out, "{}", json!({ "cases": cases })).map_err(io)?;
        return Ok(0);
    }
    for case in cases {
        let by: Vec<String> = case.by.iter().map(|sha| short_sha(sha)).collect();
        let by = if by.is_empty() {
            String::new()
        } else {
            format!(" (by {})", by.join(", "))
        };
        writeln!(
            out,
            "{} {} {}{by}",
            case_kind_label(case.kind),
            short_sha(&case.sha),
            case.subject
        )
        .map_err(io)?;
    }
    Ok(0)
}

fn format_eval_progress(case: &review::eval::Case, result: &Result<EngineResult, String>, block_at: &str) -> String {
    match result {
        Ok(typed) => {
            let got = review::eval::score(case, typed, block_at);
            let located = match got.located {
                Some(true) => ", located",
                Some(false) => ", not located",
                None => "",
            };
            format!(
                "{}{located}, {} confirmed",
                if got.blocked { "blocked" } else { "not blocked" },
                got.confirmed
            )
        }
        Err(error) => format!("failed: {error}"),
    }
}

fn run_eval_workers(
    engine: &Arc<dyn ReviewEngine>,
    cases: &[review::eval::Case],
    specs: &[Value],
    config: &AutoreviewConfig,
    err: &mut dyn Write,
) -> Vec<Option<Result<EngineResult, String>>> {
    let workers = (config.concurrency.0.max(1.0) as usize).min(cases.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (sender, received) = std::sync::mpsc::channel::<(usize, Result<EngineResult, String>)>();
    let mut results: Vec<Option<Result<EngineResult, String>>> = vec![None; cases.len()];
    let _ = writeln!(err, "reviewing {} commit(s), {workers} at a time", cases.len());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let (sender, engine, next) = (sender.clone(), engine.clone(), &next);
            scope.spawn(move || eval_worker_loop(sender, engine, specs, next));
        }
        drop(sender);
        for (done, (index, result)) in received.iter().enumerate() {
            let case = &cases[index];
            let line = format_eval_progress(case, &result, &config.block_at);
            let _ = writeln!(
                err,
                "[{}/{}] {} {} {}: {line}",
                done + 1,
                cases.len(),
                case_kind_label(case.kind),
                short_sha(&case.sha),
                case.subject
            );
            results[index] = Some(result);
        }
    });
    results
}

fn eval_worker_loop(
    sender: std::sync::mpsc::Sender<(usize, Result<EngineResult, String>)>,
    engine: Arc<dyn ReviewEngine>,
    specs: &[Value],
    next: &std::sync::atomic::AtomicUsize,
) {
    loop {
        let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Some(spec) = specs.get(index) else {
            break;
        };
        let reviewed = engine.review(spec, None).and_then(|raw| EngineResult::parse(&raw));
        if sender.send((index, reviewed)).is_err() {
            break;
        }
    }
}

fn score_eval_results(
    cases: &[review::eval::Case],
    results: Vec<Option<Result<EngineResult, String>>>,
    block_at: &str,
) -> (Vec<(review::eval::Case, review::eval::CaseScore)>, Vec<Value>) {
    let mut scored = Vec::new();
    let mut rows = Vec::new();
    for (case, result) in cases.iter().zip(results) {
        let mut row = serde_json::to_value(case).unwrap_or(Value::Null);
        match result {
            Some(Ok(typed)) => {
                let got = review::eval::score(case, &typed, block_at);
                row["score"] = serde_json::to_value(&got).unwrap_or(Value::Null);
                scored.push((case.clone(), got));
            }
            Some(Err(error)) => row["error"] = error.into(),
            None => row["error"] = "not reviewed".into(),
        }
        rows.push(row);
    }
    (scored, rows)
}

fn write_eval_report(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    repo_dir: &Path,
    dir: &Path,
    rows: Vec<Value>,
    totals: &Value,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let name = repo_dir
        .file_name()
        .map_or_else(|| "repo".into(), |name| name.to_string_lossy().into_owned());
    let report_dir = dir.join("evals");
    let report = report_dir.join(format!("{name}-{}.json", jsrt::date::now_ms()));
    let document = json!({
        "repo": repo_dir.display().to_string(), "blockAt": config.block_at, "mode": config.mode,
        "model": config.model, "cases": rows, "summary": totals,
    });
    std::fs::create_dir_all(&report_dir)
        .and_then(|()| std::fs::write(&report, format!("{document:#}\n")))
        .map_err(io)?;
    if parsed.json {
        let mut shown = document;
        shown["report"] = report.display().to_string().into();
        writeln!(out, "{shown}").map_err(io)?;
    } else {
        write_lines(out, review::eval::summary_lines(totals, &config.block_at))?;
        writeln!(out, "Report: {}", report.display()).map_err(io)?;
    }
    Ok(0)
}

/// `eval <repo-dir>`: the reviewer measured on the repository's own mistakes (review::eval). The cases come from
/// history with no model call (`--list` stops there); each is then reviewed as `review --repo-dir` would review it,
/// up to `autoreview.concurrency` at once on one engine, and scored. The report is kept under the autoreview directory (`evals/`).
#[expect(
    clippy::too_many_arguments,
    reason = "internal wiring helper; parameters match distinct caller inputs"
)]
pub(super) fn eval_command(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    use review::eval::{EvalOptions, build_cases, summary};
    let target = parsed
        .target
        .as_deref()
        .ok_or_else(|| UsageError("eval needs a repository directory".into()))?;
    let repo_dir = absolute(&env.cwd, target);
    let limit = parsed.limit.map_or(review::eval::DEFAULT_LIMIT, |n| n as usize);
    let options = EvalOptions {
        scan: parsed.scan.map_or(review::eval::DEFAULT_SCAN, |n| n as usize),
        limit,
        controls: parsed.controls.map_or(limit, |n| n as usize),
        ..EvalOptions::default()
    };
    let mut cases = build_cases(&*ov.runner(), &repo_dir.display().to_string(), &options).map_err(Failure::Error)?;
    if !filter_eval_cases(&mut cases, parsed.only.as_deref(), err) {
        return Ok(1);
    }
    if parsed.list {
        return write_eval_list(parsed, &cases, out);
    }
    if cases.is_empty() {
        let _ = writeln!(
            err,
            "no reverted or fixed commits (or controls) in the last {} commits",
            options.scan
        );
        return Ok(1);
    }
    let log = logger(&env.var);
    let dir = PathBuf::from(&paths.dir);
    let engine: Arc<dyn ReviewEngine> = match &ov.engine {
        Some(engine) => engine.clone(),
        None => Arc::new(open_engine(config, saved, env, &dir, log).map_err(Failure::Error)?),
    };
    let specs: Vec<Value> = cases
        .iter()
        .map(|case| {
            let mut one = parsed.clone();
            one.base = Some(case.parent.clone());
            one.head = Some(case.sha.clone());
            let mut spec = offline_spec(&one, config, env, &repo_dir, None, None);
            index_knowledge(&one, &paths.knowledge, &repo_dir, &case.sha, &mut spec, err);
            spec
        })
        .collect();
    let results = run_eval_workers(&engine, &cases, &specs, config, err);
    let (scored, rows) = score_eval_results(&cases, results, &config.block_at);
    let totals = summary(&scored);
    write_eval_report(parsed, config, &repo_dir, &dir, rows, &totals, out)
}

fn insert_test_env(
    spec: &mut Map<String, Value>,
    parsed: &Parsed,
    env: &Env,
    repo_dir: &Path,
    lent: Option<&str>,
    prepared: Option<&review::prepare_env::Prepared>,
) {
    if let Some(test_env) = &parsed.test_env {
        spec.insert(
            "testEnv".into(),
            absolute(&env.cwd, test_env).display().to_string().into(),
        );
        spec.insert("testEnvKind".into(), "testEnv".into());
    } else if let Some(prepared) = prepared {
        spec.insert("testCheckout".into(), prepared.dir.clone().into());
        let paths: Vec<Value> = prepared
            .toolchain
            .iter()
            .map(|entry| entry.path.clone().into())
            .collect();
        spec.insert("testToolchain".into(), Value::Array(paths));
        spec.insert("testEnvKind".into(), "prepared".into());
        if !prepared.failures.is_empty() {
            spec.insert("testEnvFailures".into(), prepared.failures.clone().into());
        }
    } else {
        let checkout = lent.map_or_else(|| repo_dir.display().to_string(), String::from);
        spec.insert("testCheckout".into(), checkout.into());
        spec.insert("testEnvKind".into(), "checkout".into());
    }
}

/// The engine spec of a local review (`review --repo-dir`).
pub(super) fn offline_spec(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    repo_dir: &Path,
    lent: Option<&str>,
    prepared: Option<&review::prepare_env::Prepared>,
) -> Value {
    let mut spec = Map::new();
    spec.insert("repoDir".into(), repo_dir.display().to_string().into());
    spec.insert("base".into(), parsed.base.clone().unwrap_or_default().into());
    spec.insert("head".into(), parsed.head.clone().unwrap_or_default().into());
    if let Ok(Value::Object(settings)) = serde_json::to_value(engine_settings(config)) {
        spec.extend(settings);
    }
    let run_tests = config.mode != "fast" && parsed.run_tests.unwrap_or(config.run_tests);
    spec.insert("runTests".into(), run_tests.into());
    insert_test_env(&mut spec, parsed, env, repo_dir, lent, prepared);
    if !config.guides.is_empty() {
        let home = env.home.display().to_string();
        let guides: Vec<Value> = config
            .guides
            .iter()
            .map(|path| expand_path(path, &home, &env.cwd).into())
            .collect();
        spec.insert("guides".into(), Value::Array(guides));
    }
    if let Some(repo) = &parsed.repo {
        spec.insert("repo".into(), repo.clone().into());
    }
    if let Some(path) = &parsed.program_path {
        spec.insert(
            "programPath".into(),
            absolute(&env.cwd, path).display().to_string().into(),
        );
    }
    if let Some(path) = &parsed.dump_program_path {
        spec.insert(
            "dumpProgramPath".into(),
            absolute(&env.cwd, path).display().to_string().into(),
        );
    }
    Value::Object(spec)
}
