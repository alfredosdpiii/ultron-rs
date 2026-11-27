//! Steps 4-5 of `ultron setup`: judge seat (Jev, d1) and Loki guardrails.

use super::{Choice, InputOptions, SetupDeps, SetupQuit, SetupUi, SummaryLine, Tone, line, pick, step_title};
use crate::setup_core;
use serde_json::{Map, Value};

const JUDGE: &str = "Judge";

#[derive(Clone, Copy, PartialEq, Eq)]
enum JudgeAction {
    JevKey,
    D1,
    Check,
    Done,
}

fn is_usable_origin(origin: &str) -> bool {
    matches!(origin, "from the environment" | "in auth.json")
}

/// Where a judge service's key comes from now, as the step shows it (never the key): `url` is the server it goes
/// to, and a stored key saved for another server does not count.
fn key_origin(deps: &SetupDeps, key: config::auth::ServiceKey, url: Option<&str>) -> &'static str {
    let auth = config::paths::AgentPaths::new(deps.agent_dir()).auth();
    match config::auth::key_origin(&key, &|name| deps.var(name), &auth) {
        config::auth::KeyOrigin::Env => "from the environment",
        config::auth::KeyOrigin::Stored(origin) if url.and_then(config::auth::url_origin).as_ref() == Some(&origin) => {
            "in auth.json"
        }
        config::auth::KeyOrigin::Stored(_) => "in auth.json for another server",
        config::auth::KeyOrigin::Unset => "not set",
    }
}

/// Probe the configured judge backends; each outcome as a note, and as the summary's detail.
fn probe_judge<U: SetupUi>(ui: &mut U, deps: &SetupDeps, backends: &[&str]) -> Result<Vec<String>, SetupQuit> {
    let mut outcomes = Vec::new();
    for backend in backends {
        let Some(probe) = deps.probe_judge.clone() else {
            break;
        };
        let (env, name) = (deps.env.clone(), backend.to_string());
        let label = if *backend == "jev" { "Jev" } else { "d1" };
        match ui.busy(&format!("Asking {label} one question…"), move || probe(&env, &name))? {
            Ok(answered) => {
                ui.note(&format!("✓ {label}: {answered}."), Tone::Success);
                outcomes.push(format!("{backend} ok"));
            }
            Err(reason) => {
                ui.note(&format!("✗ {label} did not answer ({reason})."), Tone::Warning);
                outcomes.push(format!("{backend} not answering"));
            }
        }
    }
    Ok(outcomes)
}

fn configure_jev_key<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    changed: &mut Vec<&'static str>,
) -> Result<(), SetupQuit> {
    let Some(key) = ui.input(
        "TypeSafe API key",
        InputOptions {
            secret: true,
            validate: Some(Box::new(|value: &str| {
                jsrt::string::trim(value)
                    .is_empty()
                    .then(|| "Paste the key, or press Esc to go back".to_string())
            })),
            ..Default::default()
        },
    )?
    else {
        return Ok(());
    };
    let auth = config::paths::AgentPaths::new(deps.agent_dir()).auth();
    match config::auth::save_service_key(&auth, &config::auth::TYPESAFE_KEY, &key, judge::backend::JEV_URL) {
        Ok(()) => {
            ui.note("✓ Saved the TypeSafe key in auth.json.", Tone::Success);
            changed.push("jev");
            probe_judge(ui, deps, &["jev"])?;
        }
        Err(error) => ui.note(&format!("✗ Could not save the key: {error}"), Tone::Error),
    }
    Ok(())
}

fn configure_d1_server<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    d1_url: Option<&str>,
    changed: &mut Vec<&'static str>,
) -> Result<(), SetupQuit> {
    let Some(url) = ui.input(
        "d1 server URL",
        InputOptions {
            initial: Some(d1_url.unwrap_or_default().to_string()),
            description: Some("For example a tailscale serve address of the machine running d1.".into()),
            validate: Some(Box::new(|value: &str| {
                setup_core::validate_base_url(value)
                    .or_else(|| config::auth::secure_url(jsrt::string::trim(value)).err())
            })),
            ..Default::default()
        },
    )?
    else {
        return Ok(());
    };
    let url = jsrt::string::trim(&url).trim_end_matches('/').to_string();
    let mut d1 = deps
        .settings
        .global_settings()
        .get("judge")
        .and_then(|j| j.get("d1"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    d1.insert("url".into(), Value::String(url.clone()));
    deps.settings.set_global_nested("judge", "d1", Some(Value::Object(d1)));
    ui.note(&format!("✓ Saved judge.d1.url ({url})."), Tone::Success);
    let kept = key_origin(deps, config::auth::D1_KEY, Some(&url));
    let prompt = if is_usable_origin(kept) {
        "d1 key (Esc keeps the current one)"
    } else {
        "d1 key"
    };
    let key = ui.input(
        prompt,
        InputOptions {
            secret: true,
            ..Default::default()
        },
    )?;
    if let Some(key) = key.filter(|k| !jsrt::string::trim(k).is_empty()) {
        let auth = config::paths::AgentPaths::new(deps.agent_dir()).auth();
        match config::auth::save_service_key(&auth, &config::auth::D1_KEY, &key, &url) {
            Ok(()) => ui.note("✓ Saved the d1 key in auth.json.", Tone::Success),
            Err(error) => ui.note(&format!("✗ Could not save the key: {error}"), Tone::Error),
        }
    }
    changed.push("d1");
    probe_judge(ui, deps, &["d1"])?;
    Ok(())
}

fn judge_status_and_choices<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    changed_empty: bool,
) -> (Option<String>, Vec<&'static str>, Vec<(Choice, JudgeAction)>) {
    let d1_url = config::keys::judge_settings(deps.settings.global_settings()).d1_url;
    let jev = key_origin(deps, config::auth::TYPESAFE_KEY, Some(judge::backend::JEV_URL));
    let d1_key = key_origin(deps, config::auth::D1_KEY, d1_url.as_deref());
    ui.note(
        &format!(
            "Jev key: {jev}. d1: {}, key {d1_key}.",
            d1_url
                .as_deref()
                .map_or("no server".to_string(), |u| format!("server {u}"))
        ),
        Tone::Dim,
    );
    let mut configured = Vec::new();
    if is_usable_origin(jev) {
        configured.push("jev");
    }
    if d1_url.is_some() && is_usable_origin(d1_key) {
        configured.push("d1");
    }
    let mut choices = vec![
        (
            Choice::described("Set the Jev key", "a TypeSafe API key"),
            JudgeAction::JevKey,
        ),
        (
            Choice::described("Set the d1 server", "its URL and key"),
            JudgeAction::D1,
        ),
    ];
    if !configured.is_empty() {
        choices.push((
            Choice::described("Check the configured backends", "one real question each"),
            JudgeAction::Check,
        ));
    }
    choices.push((
        Choice::new(if changed_empty { "Skip" } else { "Done" }),
        JudgeAction::Done,
    ));
    (d1_url, configured, choices)
}

pub(super) fn judge_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(deps, 4, "Judge seat (Jev, d1)"));
    ui.note(
        "The judge answers closed questions (is this memory still true? does this code show the finding?) for batch and background work, never on every turn: Jev (TypeSafe) first, a d1 server when Jev cannot. Keys go to auth.json and are never shown.",
        Tone::Info,
    );
    let mut changed = Vec::new();
    loop {
        let (d1_url, configured, choices) = judge_status_and_choices(ui, deps, changed.is_empty());
        match pick(ui, "Judge seat", &choices, None)? {
            None | Some(JudgeAction::Done) => break,
            Some(JudgeAction::Check) => {
                let outcomes = probe_judge(ui, deps, &configured)?;
                if changed.is_empty() {
                    return Ok(line(JUDGE, "unchanged", outcomes.join(", ")));
                }
            }
            Some(JudgeAction::JevKey) => configure_jev_key(ui, deps, &mut changed)?,
            Some(JudgeAction::D1) => configure_d1_server(ui, deps, d1_url.as_deref(), &mut changed)?,
        }
    }
    Ok(if changed.is_empty() {
        line(JUDGE, "skipped", "Jev and d1 unchanged")
    } else {
        line(JUDGE, "configured", changed.join(", "))
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LokiAction {
    Keep,
    AutoInit,
    AutoCommit,
    Advise,
    Enforce,
    Off,
    On,
}

pub fn describe_loki(settings: &Map<String, Value>) -> String {
    let mode = settings.get("mode").and_then(Value::as_str).unwrap_or("on");
    if mode == "off" {
        return "off".into();
    }
    let off = |key: &str| settings.get(key) == Some(&Value::Bool(false));
    [
        if mode == "advise" {
            "advise-only".to_string()
        } else {
            "blocking".to_string()
        },
        format!("auto-install {}", if off("autoInit") { "off" } else { "on" }),
        format!("auto-commit {}", if off("autoCommit") { "off" } else { "on" }),
    ]
    .join(", ")
}

fn note_loki_environment<U: SetupUi>(ui: &mut U, deps: &SetupDeps) {
    match &deps.loki_version {
        None => ui.note(
            "✗ This installation has no bundled Loki engine; guardrails run only where a repository has its own .loki/.",
            Tone::Warning,
        ),
        Some(version) => ui.note(&format!("✓ Bundled Loki {version}"), Tone::Success),
    }
    for name in [
        session_core::compose::LOKI_ENV,
        loki::auto_setup::LOKI_AUTOINIT_ENV,
        loki::auto_setup::LOKI_AUTOCOMMIT_ENV,
    ] {
        if let Some(value) = deps.var(name).filter(|v| !jsrt::string::trim(v).is_empty()) {
            ui.note(
                &format!("{name}={value} is set; it overrides the settings saved here."),
                Tone::Warning,
            );
        }
    }
    let analyzers = setup_core::check_loki_analyzers(&deps.probe);
    for analyzer in &analyzers {
        let msg = if analyzer.found {
            format!("✓ {} ({})", analyzer.name, analyzer.language)
        } else {
            format!(
                "– {} ({}): not found. {}",
                analyzer.name, analyzer.language, analyzer.hint
            )
        };
        ui.note(&msg, if analyzer.found { Tone::Success } else { Tone::Dim });
    }
    if analyzers.iter().any(|a| !a.found) {
        ui.note(
            "Missing analyzers are reported as NOT CHECKED, never as clean.",
            Tone::Dim,
        );
    }
}

fn loki_choices(current: &Map<String, Value>) -> Vec<(Choice, LokiAction)> {
    let mode = current.get("mode").and_then(Value::as_str).unwrap_or("on");
    if mode == "off" {
        return vec![
            (Choice::new("Keep Loki off"), LokiAction::Keep),
            (Choice::new("Turn Loki on"), LokiAction::On),
        ];
    }
    let off = |key: &str| current.get(key) == Some(&Value::Bool(false));
    vec![
        (Choice::new("Keep these settings"), LokiAction::Keep),
        (
            Choice::described(
                format!("Turn auto-install {}", if off("autoInit") { "on" } else { "off" }),
                "Create .loki/ (engine and default policy) in a Git repository that lacks it",
            ),
            LokiAction::AutoInit,
        ),
        (
            Choice::described(
                format!("Turn auto-commit {}", if off("autoCommit") { "on" } else { "off" }),
                "Commit the .loki/ Ultron created, and only it; on a fork the commit rides along in pull requests",
            ),
            LokiAction::AutoCommit,
        ),
        if mode == "advise" {
            (Choice::new("Block unsafe writes again"), LokiAction::Enforce)
        } else {
            (Choice::new("Advise only (report, never block)"), LokiAction::Advise)
        },
        (Choice::new("Turn Loki off"), LokiAction::Off),
    ]
}

fn apply_loki_action(current: &mut Map<String, Value>, action: LokiAction) {
    let off = |m: &Map<String, Value>, key: &str| m.get(key) == Some(&Value::Bool(false));
    match action {
        LokiAction::AutoInit => {
            let value = off(current, "autoInit");
            current.insert("autoInit".into(), Value::Bool(value));
        }
        LokiAction::AutoCommit => {
            let value = off(current, "autoCommit");
            current.insert("autoCommit".into(), Value::Bool(value));
        }
        LokiAction::Advise => {
            current.insert("mode".into(), "advise".into());
        }
        LokiAction::Off => {
            current.insert("mode".into(), "off".into());
        }
        LokiAction::Keep | LokiAction::Enforce | LokiAction::On => {
            current.insert("mode".into(), "on".into());
        }
    }
}

pub(super) fn loki_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(deps, 5, "Loki guardrails"));
    ui.note(
        "Loki checks every file the agent writes with deterministic rules and real analyzers: edit() and write() are checked before the file changes, other writes after the cell.",
        Tone::Info,
    );
    note_loki_environment(ui, deps);
    let initial = loki::settings::saved(deps.settings.global_settings());
    let mut current = initial.clone();
    loop {
        ui.note(&format!("Loki: {}.", describe_loki(&current)), Tone::Info);
        let choices = loki_choices(&current);
        let action = match pick(ui, "Loki guardrails", &choices, None)? {
            None | Some(LokiAction::Keep) => break,
            Some(action) => action,
        };
        apply_loki_action(&mut current, action);
    }
    if describe_loki(&current) == describe_loki(&initial) {
        return Ok(line("Loki", "unchanged", describe_loki(&current)));
    }
    let patch: Vec<(&str, Option<Value>)> = current.iter().map(|(k, v)| (k.as_str(), Some(v.clone()))).collect();
    let loki = loki::settings::patched(deps.settings.global_settings(), &patch);
    deps.settings.set_global(loki::settings::KEY, Some(loki));
    Ok(line("Loki", "configured", describe_loki(&current)))
}
