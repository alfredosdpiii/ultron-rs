//! Daemon, service, status, doctor, prepare, single-PR review, and ban commands for `ultron autoreview`.

use super::*;

pub(super) fn model_lines(config: &AutoreviewConfig) -> Vec<String> {
    describe_models(config)
}

/// `age`: "never", "<n> s ago", "<n> min ago" or "<n> h ago".
pub fn age(iso: Option<&str>, now: i64) -> String {
    let Some(at) = iso.filter(|iso| !iso.is_empty()) else {
        return "never".into();
    };
    let parsed = jsrt::date::parse_date(at).map_or(f64::NAN, |ms| ms as f64);
    let rounded = jsrt::number::round((now as f64 - parsed) / 1000.0);
    let seconds = if rounded.is_nan() { rounded } else { rounded.max(0.0) };
    if seconds.is_nan() {
        return "NaN h ago".into();
    }
    if seconds < 90.0 {
        format!("{} s ago", jsrt::number::to_string(seconds))
    } else if seconds < 5400.0 {
        format!(
            "{} min ago",
            jsrt::number::to_string(jsrt::number::round(seconds / 60.0))
        )
    } else {
        format!(
            "{} h ago",
            jsrt::number::to_string(jsrt::number::round(seconds / 3600.0))
        )
    }
}

/// What tests replace (`AutoreviewEnvironment.runner` / `.engine` of cli.ts): `gh` and git, and the review engine.
#[derive(Default, Clone)]
pub struct Overrides {
    pub runner: Option<Arc<dyn review::runner::Runner>>,
    pub engine: Option<Arc<dyn ReviewEngine>>,
}

impl Overrides {
    pub(super) fn runner(&self) -> Arc<dyn review::runner::Runner> {
        self.runner
            .clone()
            .unwrap_or_else(|| Arc::new(review::runner::ProcessRunner))
    }
}

fn status_json(
    accounts: &[review::accounts::Account],
    state: &review::state::AutoreviewState,
    config: &AutoreviewConfig,
    bans: &review::ban::BanList,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let listed: Vec<Value> = accounts
        .iter()
        .map(|account| {
            let mut entry = serde_json::to_value(account).unwrap_or(Value::Null);
            if let (Some(entry), Some(saved)) = (
                entry.as_object_mut(),
                state.accounts.get(&review::accounts::account_key(account)),
            ) && let Ok(Value::Object(saved)) = serde_json::to_value(saved)
            {
                entry.extend(saved);
            }
            entry
        })
        .collect();
    let json = json!({
        "accounts": listed,
        "queue": state.queue.clone().unwrap_or_default(),
        "recent": serde_json::to_value(&state.recent).unwrap_or(Value::Null),
        "daemon": state.daemon.as_ref().map_or(Value::Null, |d| serde_json::to_value(d).unwrap_or(Value::Null)),
        "dryRun": config.dry_run,
        "ban": bans.rules().iter().map(|(key, logins)| (key.clone(), json!(logins))).collect::<Map<String, Value>>(),
    });
    writeln!(out, "{}", review::state::to_json_1(&json)).map_err(io)?;
    Ok(0)
}

fn format_account_line(
    account: &review::accounts::Account,
    state: &review::state::AutoreviewState,
    now: i64,
) -> String {
    let entry = state
        .accounts
        .get(&review::accounts::account_key(account))
        .cloned()
        .unwrap_or_default();
    let paused = entry
        .paused_until
        .as_deref()
        .filter(|until| jsrt::date::parse_date(until).is_some_and(|at| at > now))
        .map_or(String::new(), |until| format!(", paused until {until}"));
    let error = entry
        .last_error
        .as_deref()
        .filter(|error| !error.is_empty())
        .map_or(String::new(), |error| format!(", last error: {error}"));
    format!(
        "  {} ({}): last poll {}{paused}{error}",
        account.login,
        account.host,
        age(entry.last_poll_at.as_deref(), now)
    )
}

fn format_recent_line(record: &review::state::ReviewRecord) -> String {
    let ms = |value: Option<jsrt::json::Num>| value.map_or(0.0, |n| n.0);
    let stages = record.stages.as_ref().map(|stages| {
        stages
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Map<String, Value>>()
    });
    format!(
        "  {}  {} {} as {}: {}{}, {} findings, pipeline {} s, {}{}pickup to post {} s{}{}",
        record.at,
        record.pull,
        record.sha.chars().take(7).collect::<String>(),
        record.account.rsplit('/').next().unwrap_or_default(),
        record.outcome,
        record
            .verdict
            .as_deref()
            .filter(|v| !v.is_empty())
            .map_or(String::new(), |v| format!(" {v}")),
        jsrt::number::to_string(ms(record.findings)),
        jsrt::number::to_string(jsrt::number::round(ms(record.total_ms) / 1000.0)),
        record.tag_to_ack_ms.map_or(String::new(), |n| format!(
            "tag to ack {} s, ",
            jsrt::number::to_fixed(n.0 / 1000.0, 1)
        )),
        record.ack_to_post_ms.map_or(String::new(), |n| format!(
            "ack to review {} s, ",
            jsrt::number::to_string(jsrt::number::round(n.0 / 1000.0))
        )),
        jsrt::number::to_string(jsrt::number::round(ms(record.pickup_to_post_ms) / 1000.0)),
        record
            .cost_usd
            .filter(|cost| cost.0 != 0.0)
            .map_or(String::new(), |cost| format!(
                ", ${}",
                jsrt::number::to_fixed(cost.0, 2)
            )),
        describe_stages(stages.as_ref())
    )
}

/// `status`: accounts, last poll, queue and recent reviews (`--json`: one object).
pub(super) fn status(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    state_path: &str,
    bans: &review::ban::BanList,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let state = review::state::StateStore::new(state_path).read();
    let accounts = review::accounts::list_accounts(&*ov.runner(), config.accounts.as_deref()).unwrap_or_default();
    if parsed.json {
        return status_json(&accounts, &state, config, bans, out);
    }
    let now = jsrt::date::now_ms();
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let mut lines = vec![format!(
        "Posting: {}; poll every {} s; up to {} reviews at once; model {}",
        if config.dry_run { "off (dry run)" } else { "on" },
        jsrt::number::to_string(config.poll_seconds.0),
        jsrt::number::to_string(config.concurrency.0),
        config.model.clone().unwrap_or_else(|| "the default model".into())
    )];
    if let Some(daemon) = &state.daemon {
        lines.push(format!(
            "Daemon: pid {}, started {}{}",
            jsrt::number::to_string(daemon.pid.0),
            age(Some(&daemon.started_at), now),
            daemon.engine_start_ms.map_or(String::new(), |ms| format!(
                ", engine start {} ms",
                jsrt::number::to_string(ms.0)
            ))
        ));
    }
    lines.push(String::new());
    lines.push("Accounts:".into());
    if accounts.is_empty() {
        lines.push("  none (gh auth login)".into());
    }
    lines.extend(accounts.iter().map(|account| format_account_line(account, &state, now)));
    if !bans.is_empty() {
        lines.push(String::new());
        lines.push("Bans (autoreview.ban):".into());
        lines.extend(crate::autoreview_ban::ban_lines(bans));
    }
    let queue = state.queue.clone().unwrap_or_default();
    lines.push(String::new());
    lines.push(format!("Queue: {}", if queue.is_empty() { "empty" } else { "" }));
    lines.extend(queue.iter().map(|item| format!("  {item}")));
    lines.push(String::new());
    lines.push("Recent reviews:".into());
    if state.recent.is_empty() {
        lines.push("  none".into());
    }
    lines.extend(state.recent.iter().rev().take(15).map(format_recent_line));
    writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    Ok(0)
}

fn last_line(stdout: &str) -> String {
    stdout.trim().split('\n').next_back().unwrap_or_default().to_string()
}

fn run_doctor_script(
    runner: &dyn review::runner::Runner,
    python: &str,
    script: &str,
    test_image: Option<&String>,
) -> Result<Value, Failure> {
    let mut argv = vec![python.to_string(), script.to_string(), "doctor".to_string()];
    argv.extend(test_image.cloned());
    let result = runner.run(
        &argv,
        &review::runner::RunOptions {
            timeout_ms: Some(180_000),
            ..Default::default()
        },
    );
    serde_json::from_str(&last_line(&result.stdout)).map_err(|_| {
        let shown = if result.stderr.is_empty() {
            &result.stdout
        } else {
            &result.stderr
        };
        Failure::Error(format!(
            "the sandbox check did not run: {}",
            shown.trim().chars().take(300).collect::<String>()
        ))
    })
}

fn inspect_lent_checkout(
    runner: &dyn review::runner::Runner,
    python: &str,
    script: &str,
    config: &AutoreviewConfig,
    home: &str,
    repo: &str,
) -> Result<(String, Option<String>, Vec<Value>), Failure> {
    let target = review::github::parse_pull_target(&format!("{repo}#1"), "github.com")
        .ok_or_else(|| Failure::Usage(format!("--repo takes owner/name, not {repo}")))?;
    let roots: Vec<String> = config
        .checkout_roots
        .iter()
        .map(|root| review::local::expand_path(root, home))
        .collect();
    let checkout = review::local::find_checkout(runner, &roots, &target);
    let environments = checkout.as_deref().map_or_default(|dir| {
        let listed = runner.run(
            &[python.into(), script.into(), "environments".into(), dir.into()],
            &review::runner::RunOptions {
                timeout_ms: Some(60_000),
                ..Default::default()
            },
        );
        match serde_json::from_str::<Value>(&last_line(&listed.stdout)) {
            Ok(Value::Array(items)) => items,
            _ => Vec::new(),
        }
    });
    Ok((repo.to_string(), checkout, environments))
}

fn format_lent_lines(lent: Option<&(String, Option<String>, Vec<Value>)>) -> Vec<String> {
    match lent {
        None => Vec::new(),
        Some((repo, None, _)) => vec![format!(
            "Local checkout of {repo}: none under autoreview.checkoutRoots with a matching remote"
        )],
        Some((repo, Some(checkout), environments)) => {
            let mut lines = vec![format!("Local checkout of {repo}: {checkout}")];
            if environments.is_empty() {
                lines
                    .push("  no prepared environment (.venv, venv, node_modules) found: nothing would be bound".into());
            }
            for item in environments {
                let text = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
                let interpreter = text("interpreter");
                let with = if !interpreter.is_empty() && interpreter != "system" {
                    format!(", with its interpreter {interpreter}")
                } else {
                    String::new()
                };
                lines.push(format!("  bound read-only: {} ({}{with})", text("path"), text("kind")));
            }
            lines.push("  never bound: the checkout's source, .git, .env files".into());
            lines
        }
    }
}

fn write_doctor_json(
    report: &Value,
    config: &AutoreviewConfig,
    lent: Option<&(String, Option<String>, Vec<Value>)>,
    prepared: Option<&review::prepare_env::Prepared>,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let mut json = report.as_object().cloned().unwrap_or_default();
    json.insert("runTests".into(), config.run_tests.into());
    if let Some((repo, checkout, environments)) = lent {
        json.insert(
            "lent".into(),
            json!({"repo": repo, "checkout": checkout, "environments": environments}),
        );
    }
    if let Some(prepared) = prepared {
        json.insert("prepared".into(), prepared.json.clone());
    }
    writeln!(out, "{}", jsrt::json::stringify(&Value::Object(json))).map_err(io)
}

fn format_doctor_lines(
    report: &Value,
    config: &AutoreviewConfig,
    model_lines: &[String],
    lent_lines: Vec<String>,
    prepared_lines: Vec<String>,
    ok: bool,
) -> Vec<String> {
    let mut lines = model_lines.to_vec();
    if report.get("mechanism").is_some_and(Value::is_null) {
        let message = report.get("message").and_then(Value::as_str).unwrap_or_default();
        lines.push(format!("Sandbox: none. {message}"));
        lines.extend(lent_lines);
        lines.extend(prepared_lines);
        return lines;
    }
    let check = report.get("selfCheck").cloned().unwrap_or(json!({}));
    let shown = |key: &str| match check.get(key) {
        None => "undefined".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => jsrt::json::stringify(other),
    };
    let owners = if config.test_owners.is_empty() {
        String::new()
    } else {
        format!(" and owners {}", config.test_owners.join(", "))
    };
    let tests_line = if !config.run_tests {
        "off (autoreview.runTests)".to_string()
    } else if ok {
        format!("run for repositories the account can push to{owners}")
    } else {
        "not run (the self-check failed)".to_string()
    };
    lines.extend([
        format!(
            "Sandbox: {}",
            report.get("isolation").and_then(Value::as_str).unwrap_or("undefined")
        ),
        format!("Self-check: {}", if ok { "passed" } else { "FAILED" }),
        format!("  network: {}", shown("network")),
        format!("  canary file in the real home: {}", shown("canary")),
        format!("  token-like variable of the caller: {}", shown("token")),
        format!("  home inside the sandbox: {}", shown("home")),
        format!(
            "  exported commit: {}; system directories: {}",
            shown("workdir"),
            shown("system")
        ),
        format!("  Docker socket: {}", shown("dockerSocket")),
        format!("Tests in reviews: {tests_line}"),
    ]);
    lines.extend(lent_lines);
    lines.extend(prepared_lines);
    lines
}

/// `doctor [--repo owner/name]`: the sandbox test runs would use and its self-check, as the review's own test
/// launcher reports them (`autoreview_tests.py doctor`, the script reviews run), and with `--repo`, the local checkout
/// whose prepared environments would be bound read-only (`autoreview_tests.py environments`).
pub(super) fn doctor(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    model_lines: &[String],
    env: &Env,
    cache_dir: &str,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let runner = ov.runner();
    let python = kernel::env::python(|name| (env.var)(name));
    let script = env
        .package_file("src/ultron/rlm/autoreview_tests.py")
        .display()
        .to_string();
    let report = run_doctor_script(&*runner, &python, &script, config.test_image.as_ref())?;
    let home = env.home.display().to_string();
    let lent = parsed
        .repo
        .as_deref()
        .map(|repo| inspect_lent_checkout(&*runner, &python, &script, config, &home, repo))
        .transpose()?;
    let prepared = lent
        .as_ref()
        .and_then(|(_, checkout, _)| checkout.as_deref())
        .and_then(|checkout| review::prepare_env::cached_environment(checkout, cache_dir, None));
    let ok = report.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if parsed.json {
        write_doctor_json(&report, config, lent.as_ref(), prepared.as_ref(), out)?;
    } else {
        let lent_lines = format_lent_lines(lent.as_ref());
        let prepared_lines = doctor_prepared_lines(lent.is_some(), prepared.as_ref());
        let lines = format_doctor_lines(&report, config, model_lines, lent_lines, prepared_lines, ok);
        let io = |error: std::io::Error| Failure::Error(error.to_string());
        writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    }
    Ok(i32::from(!ok))
}

/// The environment `prepareEnvironment` reads: PATH, where mise, uv and the toolchains are looked for (the
/// processes it runs inherit the rest).
pub(super) fn prepare_vars(env: &Env) -> HashMap<String, String> {
    (env.var)("PATH").map_or_default(|path| HashMap::from([("PATH".to_string(), path)]))
}

/// `prepareEnvironment` of `repo_dir` as the command line runs it: log lines to stderr.
#[expect(
    clippy::too_many_arguments,
    reason = "internal wiring helper; parameters match distinct caller inputs"
)]
pub(super) fn run_prepare(
    repo_dir: &str,
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    cache_dir: &str,
    ov: &Overrides,
    err: &mut dyn Write,
    force: bool,
) -> Result<review::prepare_env::Prepared, Failure> {
    let runner = ov.runner();
    let err = std::cell::RefCell::new(err);
    let log = |line: &str| {
        let _ = writeln!(err.borrow_mut(), "{line}");
    };
    let options = review::prepare_env::PrepareOptions {
        runner: &*runner,
        cache_dir: cache_dir.to_string(),
        python: parsed.python.clone(),
        mise: Some(config.mise.clone()),
        env: prepare_vars(env),
        home: env.home.display().to_string(),
        log: &log,
        timeout_ms: None,
        force,
        now: &jsrt::date::now_ms,
    };
    review::prepare_env::prepare_environment(repo_dir, &options).map_err(Failure::Error)
}

/// `prepare <repo-dir> [--python 3.x]`: build the test environment now (always rebuilt), and say what was prepared;
/// exit 1 when anything failed.
pub(super) fn prepare_command(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    cache_dir: &str,
    ov: &Overrides,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    let Some(target) = &parsed.target else {
        return Err(Failure::Usage("prepare needs the repository directory".into()));
    };
    let repo_dir = absolute(&env.cwd, target).display().to_string();
    let prepared = run_prepare(&repo_dir, parsed, config, env, cache_dir, ov, err, true)?;
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        writeln!(out, "{}", jsrt::json::stringify(&prepared.json)).map_err(io)?;
    } else {
        let mut lines = vec![format!(
            "Environment {} at {}{}",
            prepared.hash,
            prepared.dir,
            if prepared.cached { " (cached)" } else { "" }
        )];
        lines.extend(prepared.prepared.iter().map(|item| format!("  prepared: {item}")));
        lines.extend(
            review::prepare::describe_toolchain(&prepared.toolchain)
                .into_iter()
                .map(|line| format!("  toolchain: {line}")),
        );
        lines.extend(prepared.failures.iter().map(|item| format!("  failed: {item}")));
        lines.push(format!(
            "  {} s",
            jsrt::number::to_string(jsrt::number::round(prepared.ms as f64 / 1000.0))
        ));
        writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    }
    Ok(i32::from(!prepared.failures.is_empty()))
}

/// The doctor's lines about the prepared environment of the lent checkout: none without `--repo`.
pub fn doctor_prepared_lines(repo_given: bool, prepared: Option<&review::prepare_env::Prepared>) -> Vec<String> {
    if !repo_given {
        return Vec::new();
    }
    let Some(prepared) = prepared else {
        return vec![format!(
            "Prepared environment: none (run `{APP_NAME} autoreview prepare <repo-dir>`)"
        )];
    };
    let mut lines = vec![format!("Prepared environment: {}", prepared.dir)];
    lines.extend(prepared.prepared.iter().map(|item| format!("  {item}")));
    lines.extend(
        review::prepare::describe_toolchain(&prepared.toolchain)
            .into_iter()
            .map(|line| format!("  toolchain: {line}")),
    );
    lines
}

/// `outcomeJson`: a posted or dry-run review as the offline JSON plus what was posted; otherwise kind and reason.
pub fn outcome_json(outcome: &review::reviewer::Outcome) -> Value {
    match outcome.reviewed() {
        Some(reviewed) => {
            let mut out = Map::new();
            out.insert("kind".into(), outcome.kind().into());
            out.insert("sha".into(), reviewed.sha.clone().into());
            out.insert("event".into(), reviewed.plan.event.into());
            if let Ok(Value::Object(offline)) = offline_json(&reviewed.result, None, None, None) {
                out.extend(offline);
            }
            out.insert("verdict".into(), reviewed.verdict.into());
            out.insert("inlineComments".into(), json!(reviewed.plan.comments.len()));
            if let Some(id) = reviewed.review_id {
                out.insert("reviewId".into(), json!(id));
            }
            if let Some(path) = &reviewed.path {
                out.insert("files".into(), json!([format!("{path}.json"), format!("{path}.md")]));
            }
            Value::Object(out)
        }
        None => json!({"kind": outcome.kind(), "reason": outcome.reason().unwrap_or_default()}),
    }
}

pub fn outcome_text(name: &str, outcome: &review::reviewer::Outcome) -> String {
    if let Some(reason) = outcome.reason() {
        return format!("{name}: {} ({reason})", outcome.kind());
    }
    let Some(reviewed) = outcome.reviewed() else {
        return format!("{name}: {}", outcome.kind());
    };
    let short: String = reviewed.sha.chars().take(7).collect();
    let findings = reviewed
        .result
        .get("findings")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if outcome.kind() == "posted" {
        format!(
            "Posted {} on {name} at {short}: {findings} findings, {} inline.",
            reviewed.plan.event,
            reviewed.plan.comments.len()
        )
    } else {
        format!(
            "Dry run of {name} at {short}: {}, {findings} findings. Written to {}.md and .json",
            reviewed.plan.event,
            reviewed.path.clone().unwrap_or_default()
        )
    }
}

fn select_pull_account(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    ov: &Overrides,
    host: &str,
) -> Result<review::accounts::Account, Failure> {
    let only = match &parsed.account {
        Some(account) => Some(vec![account.clone()]),
        None => config.accounts.clone(),
    };
    let accounts: Vec<review::accounts::Account> = review::accounts::list_accounts(&*ov.runner(), only.as_deref())
        .map_err(|error| Failure::Error(error.to_string()))?
        .into_iter()
        .filter(|account| account.host == host)
        .collect();
    accounts
        .iter()
        .find(|a| a.active)
        .or(accounts.first())
        .cloned()
        .ok_or_else(|| {
            Failure::Error(format!(
                "gh is not logged in to {host}{}",
                parsed.account.as_ref().map_or(String::new(), |a| format!(" as {a}"))
            ))
        })
}

fn write_review_outcome(
    parsed: &Parsed,
    pull: &review::github::PullRef,
    outcome: &review::reviewer::Outcome,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        writeln!(out, "{}", jsrt::json::stringify(&outcome_json(outcome))).map_err(io)
    } else {
        writeln!(out, "{}", outcome_text(&review::github::pull_key(pull), outcome)).map_err(io)
    }
}

/// `review <owner/repo#N | URL>`: one pull request now, as the host's active account (or `--account`).
pub(super) fn review_pull_request(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let target = parsed
        .target
        .as_deref()
        .ok_or_else(|| Failure::Usage("review needs a pull request (owner/repo#N or its URL) or --repo-dir".into()))?;
    let pull = review::github::parse_pull_target(target, "github.com").ok_or_else(|| {
        Failure::Usage(format!(
            "not a pull request: {target} (use owner/repo#N or the pull request URL)"
        ))
    })?;
    let account = select_pull_account(parsed, config, ov, &pull.host)?;
    let deps = reviewer_deps(config, saved, env, paths, ov)?;
    let candidate = review::reviewer::Candidate {
        account,
        pull: pull.clone(),
        reasons: vec!["manual".into()],
        picked_at: jsrt::date::now_ms(),
        notified_at: None,
    };
    let options = review::reviewer::ReviewOptions {
        force: true,
        dry_run: Some(config.dry_run),
    };
    let outcome =
        review::reviewer::review_pull(&deps, &candidate, options).map_err(|error| Failure::Error(error.to_string()))?;
    write_review_outcome(parsed, &pull, &outcome, out)?;
    Ok(i32::from(!matches!(outcome.kind(), "posted" | "dry-run")))
}

fn daemon_outcome_recorder(outcomes: Arc<std::sync::Mutex<Vec<Value>>>) -> review::daemon::OnOutcome {
    Arc::new(move |candidate, reported| {
        let mut entry = Map::new();
        entry.insert("pull".into(), review::github::pull_key(&candidate.pull).into());
        entry.insert("account".into(), candidate.account.login.clone().into());
        let details = match reported {
            review::daemon::Reported::Outcome(outcome) => outcome_json(outcome),
            review::daemon::Reported::Failed(error) => json!({"kind": "failed", "error": error}),
        };
        if let Value::Object(details) = details {
            entry.extend(details);
        }
        jsrt::sync::lock(&outcomes).push(Value::Object(entry));
    })
}

fn run_daemon_once(
    daemon: &review::daemon::Daemon,
    lock: review::state::DaemonLock,
    parsed: &Parsed,
    outcomes: &std::sync::Mutex<Vec<Value>>,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    daemon.once();
    lock.release();
    if parsed.json {
        let reviews = jsrt::sync::lock(outcomes).clone();
        writeln!(out, "{}", jsrt::json::stringify(&json!({"reviews": reviews}))).map_err(io)?;
    }
    Ok(0)
}

/// `once` (one poll cycle, then exit) and `run` (poll and review until SIGINT/SIGTERM): the daemon over every
/// account gh is logged in to (or `autoreview.accounts`), holding the agent directory's daemon lock.
pub(super) fn daemon_command(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let mut deps = reviewer_deps(config, saved, env, paths, ov)?;
    let mask_setting = (env.var)(session_core::compose::MASK_SECRETS_ENV);
    let masker = known_secrets();
    deps.log = review::daemon::create_logger(
        Some(paths.logs.clone()),
        Arc::new(jsrt::date::now_ms),
        review::daemon::stderr_writer(),
        Arc::new(move |line: &str| masker.mask_counted(line, mask_setting.as_deref()).0),
    );
    let log = deps.log.clone();
    let store = deps.store.clone();
    let accounts = review::accounts::list_accounts(&*ov.runner(), config.accounts.as_deref())
        .map_err(|error| Failure::Error(error.to_string()))?;
    if accounts.is_empty() {
        return Err(Failure::Error(
            "gh is not logged in to any account (gh auth login)".into(),
        ));
    }
    let outcomes: Arc<std::sync::Mutex<Vec<Value>>> = Arc::default();
    let daemon = review::daemon::Daemon::new(review::daemon::DaemonDeps {
        reviewer: deps,
        accounts: accounts.clone(),
        sleep: None,
        on_outcome: Some(daemon_outcome_recorder(outcomes.clone())),
    });
    let lock = review::state::acquire_daemon_lock(&paths.dir).map_err(|error| match error {
        review::github::Error::DaemonRunning(_) => Failure::Error(error.to_string()),
        other => Failure::Error(format!("cannot lock {}: {other}", paths.dir)),
    })?;
    let names: Vec<String> = accounts.iter().map(|a| format!("{}@{}", a.login, a.host)).collect();
    log(&format!(
        "autoreview {}: {}; posting {}",
        parsed.command,
        names.join(", "),
        if config.dry_run { "off (dry run)" } else { "on" }
    ));
    if parsed.command == "once" {
        return run_daemon_once(&daemon, lock, parsed, &outcomes, out);
    }
    let _ = review::daemon::record_daemon_start(&store, std::process::id(), jsrt::date::now_ms(), None);
    let stopper = daemon.stopper();
    signals::set_handler(Arc::new(move |_| stopper.stop()));
    daemon.run();
    signals::clear_handler();
    let _ = review::daemon::record_daemon_stop(&store);
    log("autoreview stopped");
    lock.release();
    Ok(0)
}

/// The user service that runs `ultron autoreview run` on `platform`, or None where neither systemd nor launchd
/// applies (`ultron autoreview install`, `/autoreview on`).
pub fn user_service(env: &Env, platform: &str) -> Result<Option<review::service::ServiceFile>, String> {
    let me = claude_root::self_cmd::self_command(&*env.var)?;
    let me = review::service::SelfCommand {
        command: me.command,
        args: me.args,
    };
    let service_env: HashMap<String, String> = ["PATH", config::paths::ENV_AGENT_DIR, "XDG_CONFIG_HOME"]
        .into_iter()
        .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
        .collect();
    Ok(review::service::service_file(
        &me,
        platform,
        &env.home.display().to_string(),
        &service_env,
    ))
}

/// `install | uninstall`: the user service file is written or removed; enabling it is printed, never run.
pub(super) fn service(
    parsed: &Parsed,
    config: &AutoreviewConfig,
    env: &Env,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let platform = jsrt::process::platform();
    let Some(file) = user_service(env, platform).map_err(Failure::Error)? else {
        return Err(Failure::Error(format!(
            "no user service on {platform}; run \"{APP_NAME} autoreview run\" under your own supervisor"
        )));
    };
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.command == "install" {
        review::service::install_service(&file).map_err(Failure::Error)?;
        let enable: Vec<String> = file.enable.iter().map(|line| format!("  {line}")).collect();
        writeln!(
            out,
            "Wrote {}\n{}\nNot enabled. To start it now and at login:\n{}",
            file.path,
            describe_models(config).join("\n"),
            enable.join("\n")
        )
        .map_err(io)?;
    } else if review::service::uninstall_service(&file) {
        let disable: Vec<String> = file.disable.iter().map(|line| format!("  {line}")).collect();
        writeln!(
            out,
            "Removed {}\nIf it is still running:\n{}",
            file.path,
            disable.join("\n")
        )
        .map_err(io)?;
    } else {
        writeln!(out, "No service file at {}", file.path).map_err(io)?;
    }
    Ok(0)
}

fn list_bans(parsed: &Parsed, env: &Env, out: &mut dyn Write) -> Result<i32, Failure> {
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let list = crate::autoreview_ban::current(env);
    if parsed.json {
        let mut ban = Map::new();
        for (key, logins) in list.rules() {
            ban.insert(key.clone(), json!(logins));
        }
        writeln!(out, "{}", jsrt::json::stringify(&Value::Object(ban))).map_err(io)?;
    } else {
        let mut lines = vec!["Bans (autoreview.ban):".to_string()];
        lines.extend(crate::autoreview_ban::ban_lines(&list));
        writeln!(out, "{}", lines.join("\n")).map_err(io)?;
    }
    Ok(0)
}

/// `ban <login> [--account <a>]`, `unban <login> [--account <a>]` and `bans [--json]`: `autoreview.ban` in the
/// global settings file.
pub(super) fn ban_command(parsed: &Parsed, env: &Env, out: &mut dyn Write) -> Result<i32, Failure> {
    if parsed.command == "bans" {
        return list_bans(parsed, env, out);
    }
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    let login = parsed
        .target
        .as_deref()
        .ok_or_else(|| Failure::Usage(format!("{} needs a GitHub login", parsed.command)))?;
    let message = crate::autoreview_ban::change(env, login, parsed.account.as_deref(), parsed.command == "ban")
        .map_err(|refused| match refused {
            crate::autoreview_ban::Refused::Usage(message) => Failure::Usage(message),
            crate::autoreview_ban::Refused::Error(message) => Failure::Error(message),
        })?;
    writeln!(out, "{message}").map_err(io)?;
    Ok(0)
}
