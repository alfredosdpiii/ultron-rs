//! Steps 3-4 of `ultron setup`: memory backend (memorantado / Hindsight) and Hindsight server setup.

use super::{Choice, InputOptions, SetupDeps, SetupQuit, SetupUi, SummaryLine, Tone, confirm, line, pick, step_title};
use crate::setup_core::{
    self, HINDSIGHT_CONTAINER, HINDSIGHT_IMAGE, HINDSIGHT_LLM_PROVIDERS, HindsightDockerOptions, HindsightLlmChoice,
};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
enum HindsightAction {
    Docker,
    Start,
    Url,
    Manual,
    Off,
    Skip,
}

const HINDSIGHT: &str = "Hindsight";
const MEMORY_BACKEND: &str = "Memory backend";

fn probe<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    message: String,
    url: &str,
) -> Result<Result<(), String>, SetupQuit> {
    let (fetch, url) = (deps.fetch.clone(), url.to_string());
    ui.busy(&message, move || setup_core::probe_hindsight(&url, &fetch))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendAction {
    Memorantado,
    Hindsight,
    Automatic,
    Url,
    Skip,
}

/// Saves `memory.backend` (`None` removes it: detected), keeping the other `memory.*` settings.
fn save_backend(deps: &mut SetupDeps, choice: Option<memorantado::Choice>) {
    deps.settings
        .set_global_nested("memory", "backend", choice.map(|c| Value::String(c.as_str().into())));
}

fn backend_choices(up: bool) -> Vec<(Choice, BackendAction)> {
    let mut choices = Vec::new();
    if up {
        choices.push((
            Choice::described("Use memorantado", "saves memory.backend: memorantado"),
            BackendAction::Memorantado,
        ));
    }
    choices.extend([
        (
            Choice::described("Use Hindsight", "saves memory.backend: hindsight"),
            BackendAction::Hindsight,
        ),
        (
            Choice::described(
                "Detect automatically",
                "memorantado once it answers (then kept), else Hindsight",
            ),
            BackendAction::Automatic,
        ),
        (
            Choice::new("memorantado runs elsewhere: set its URL"),
            BackendAction::Url,
        ),
        (Choice::new("Skip"), BackendAction::Skip),
    ]);
    choices
}

fn describe_backend_state(current: Option<memorantado::Choice>, detected: Option<memorantado::Choice>) -> &'static str {
    match (current, detected) {
        (Some(choice), _) => choice.as_str(),
        (None, Some(memorantado::Choice::Memorantado)) => "unset (memorantado, detected)",
        (None, _) => "unset (memorantado once it answers, else Hindsight)",
    }
}

fn prompt_memorantado_url<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps, url: String) -> Result<(), SetupQuit> {
    let options = InputOptions {
        initial: Some(url),
        validate: Some(Box::new(setup_core::validate_base_url)),
        ..Default::default()
    };
    if let Some(input) = ui.input("memorantado URL", options)? {
        let input = input.trim().trim_end_matches('/').to_string();
        deps.settings
            .set_global(memorantado::resolve::URL_SETTING, Some(Value::String(input)));
    }
    Ok(())
}

fn apply_backend_action<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    action: Option<BackendAction>,
    current: Option<memorantado::Choice>,
    url: String,
    now: &str,
) -> Result<Option<SummaryLine>, SetupQuit> {
    let (choice, detail) = match action {
        None | Some(BackendAction::Skip) => return Ok(Some(line(MEMORY_BACKEND, "skipped", now))),
        Some(BackendAction::Url) => {
            prompt_memorantado_url(ui, deps, url)?;
            return Ok(None);
        }
        Some(BackendAction::Memorantado) => (Some(memorantado::Choice::Memorantado), format!("memorantado at {url}")),
        Some(BackendAction::Hindsight) => (Some(memorantado::Choice::Hindsight), "Hindsight".to_string()),
        Some(BackendAction::Automatic) => (None, "detected".to_string()),
    };
    let status = if choice == current { "unchanged" } else { "configured" };
    save_backend(deps, choice);
    Ok(Some(line(MEMORY_BACKEND, status, detail)))
}

fn memory_backend_round<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    probe: &super::ProbeMemorantado,
) -> Result<Option<SummaryLine>, SetupQuit> {
    let current = memorantado::resolve::choice(&deps.settings);
    let Some(endpoint) = memorantado::Endpoint::configured(&|name| deps.var(name), &deps.settings) else {
        ui.note("memorantado is turned off (ULTRON_MEMORANTADO_URL).", Tone::Info);
        return Ok(Some(line(MEMORY_BACKEND, "unchanged", "Hindsight (memorantado off)")));
    };
    let url = endpoint.url;
    let (target, probe_fn) = (url.clone(), probe.clone());
    let up = ui.busy(&format!("Looking for memorantado at {url}…"), move || {
        probe_fn(&target)
    })?;
    let (status_msg, tone) = if up {
        (format!("✓ memorantado is running at {url}."), Tone::Success)
    } else {
        (format!("memorantado is not reachable at {url}."), Tone::Warning)
    };
    ui.note(&status_msg, tone);
    let now = describe_backend_state(current, memorantado::resolve::remembered(&deps.agent_dir()));
    ui.note(&format!("memory.backend: {now}."), Tone::Dim);
    let choices = backend_choices(up);
    let action = pick(ui, "Memory backend", &choices, None)?;
    apply_backend_action(ui, deps, action, current, url, now)
}

/// Which memory server `memory.*`, automatic memory and dreaming use: memorantado (claims with keys, curation,
/// file evidence; dreaming writes its briefs) or Hindsight (its own model writes briefs); unset, memorantado once it
/// answers at the start of a session (kept from then on: memorantado::resolve), else Hindsight.
pub(super) fn memory_backend_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    let judge = usize::from(deps.probe_judge.is_some());
    ui.heading(&format!("Step 3/{}: Memory backend", 6 + judge));
    ui.note(
        "Memory lives in memorantado (scoped claims that dreaming re-checks against your files) or in Hindsight. Unset, memorantado is used once it answers and kept from then on (unless you set a Hindsight URL); until then, Hindsight. Once memorantado is the backend, memory pauses while it is down: nothing goes to Hindsight instead.",
        Tone::Info,
    );
    let Some(probe) = deps.probe_memorantado.clone() else {
        return Ok(line(MEMORY_BACKEND, "skipped", "no probe"));
    };
    loop {
        if let Some(done) = memory_backend_round(ui, deps, &probe)? {
            return Ok(done);
        }
    }
}

fn hindsight_running_choice<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    url: &str,
) -> Result<Option<SummaryLine>, SetupQuit> {
    ui.note(&format!("✓ Hindsight is running at {url}."), Tone::Success);
    let next = pick(
        ui,
        "Hindsight",
        &[
            (Choice::new("Use it"), false),
            (Choice::new("Use a different URL"), true),
        ],
        None,
    )?;
    if next == Some(true) {
        return custom_hindsight_url(ui, deps);
    }
    Ok(Some(line(HINDSIGHT, "unchanged", format!("running at {url}"))))
}

fn hindsight_unreachable_choices(deps: &SetupDeps, url: &str) -> (bool, Vec<(Choice, HindsightAction)>) {
    let docker_available = (deps.probe)("docker", &["--version"]).is_some();
    let filter = format!("name=^{HINDSIGHT_CONTAINER}$");
    let existing = docker_available
        .then(|| (deps.probe)("docker", &["ps", "-a", "--filter", &filter, "--format", "{{.Status}}"]))
        .flatten()
        .filter(|s| !s.is_empty());
    let mut choices = Vec::new();
    if docker_available && setup_core::is_local_url(url) {
        match existing {
            Some(status) => choices.push((
                Choice::described(
                    format!("Start the existing \"{HINDSIGHT_CONTAINER}\" container"),
                    status,
                ),
                HindsightAction::Start,
            )),
            None => choices.push((
                Choice::described("Install it with Docker", HINDSIGHT_IMAGE),
                HindsightAction::Docker,
            )),
        }
    }
    choices.extend([
        (Choice::new("It runs elsewhere: set its URL"), HindsightAction::Url),
        (Choice::new("Show manual install instructions"), HindsightAction::Manual),
        (
            Choice::described("Turn memory off", "saves hindsightUrl: off"),
            HindsightAction::Off,
        ),
        (Choice::new("Skip"), HindsightAction::Skip),
    ]);
    (docker_available, choices)
}

fn show_manual_hindsight<U: SetupUi>(
    ui: &mut U,
    url: &str,
    docker_available: bool,
) -> Result<Option<SummaryLine>, SetupQuit> {
    if !docker_available {
        ui.note("Docker was not found on this machine.", Tone::Dim);
    }
    for instruction in setup_core::hindsight_manual_instructions(url) {
        ui.note(&instruction, Tone::Dim);
    }
    if confirm(
        ui,
        "Check again once it is running?",
        "Check again",
        "Continue without it",
    )? {
        return Ok(None);
    }
    Ok(Some(line(HINDSIGHT, "skipped", "manual install instructions shown")))
}

fn wait_for_hindsight_container<U: SetupUi>(ui: &mut U, deps: &SetupDeps, url: &str) -> Result<SummaryLine, SetupQuit> {
    let (fetch, wait, interval, target) = (
        deps.fetch.clone(),
        deps.hindsight_wait,
        deps.hindsight_interval,
        url.to_string(),
    );
    let minutes = (deps.hindsight_wait.as_millis() as f64 / 60_000.0).round();
    let up = ui.busy(
        &format!("Waiting for Hindsight at {url} (the first start downloads models; up to {minutes} min)…"),
        move || setup_core::wait_for_hindsight(&target, wait, interval, &fetch),
    )?;
    if up {
        ui.note(&format!("✓ Hindsight is running at {url}."), Tone::Success);
        return Ok(line(
            HINDSIGHT,
            "configured",
            format!("Docker container \"{HINDSIGHT_CONTAINER}\" at {url}"),
        ));
    }
    ui.note(
        &format!(
            "Hindsight did not answer within the wait. It may still be starting: `docker logs -f {HINDSIGHT_CONTAINER}`."
        ),
        Tone::Warning,
    );
    Ok(line(
        HINDSIGHT,
        "failed",
        format!("container \"{HINDSIGHT_CONTAINER}\" started, not answering yet"),
    ))
}

fn run_hindsight_action<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    url: &str,
    action: HindsightAction,
    docker_available: bool,
) -> Result<Option<SummaryLine>, SetupQuit> {
    match action {
        HindsightAction::Skip => Ok(Some(line(HINDSIGHT, "skipped", format!("not reachable at {url}")))),
        HindsightAction::Manual => show_manual_hindsight(ui, url, docker_available),
        HindsightAction::Off => {
            deps.settings
                .set_global("hindsightUrl", Some(Value::String("off".into())));
            ui.note(
                "Memory is turned off. Run `ultron setup` again to turn it on.",
                Tone::Info,
            );
            Ok(Some(line(HINDSIGHT, "configured", "off")))
        }
        HindsightAction::Url => custom_hindsight_url(ui, deps),
        HindsightAction::Start | HindsightAction::Docker => {
            let started = if action == HindsightAction::Start {
                start_existing_container(ui, deps)?
            } else {
                install_with_docker(ui, deps, url)?
            };
            match started {
                None => Ok(None),
                Some(false) => Ok(Some(line(HINDSIGHT, "failed", "the container did not start"))),
                Some(true) => wait_for_hindsight_container(ui, deps, url).map(Some),
            }
        }
    }
}

fn hindsight_when_off<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.note("Memory is turned off.", Tone::Info);
    if !confirm(ui, "Turn memory on?", "Yes, set a Hindsight URL", "No, leave it off")? {
        return Ok(line(HINDSIGHT, "unchanged", "off"));
    }
    Ok(custom_hindsight_url(ui, deps)?.unwrap_or_else(|| line(HINDSIGHT, "unchanged", "off")))
}

fn hindsight_probe_loop<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps, url: &str) -> Result<SummaryLine, SetupQuit> {
    loop {
        match probe(ui, deps, format!("Looking for Hindsight at {url}…"), url)? {
            Ok(()) => {
                if let Some(done) = hindsight_running_choice(ui, deps, url)? {
                    return Ok(done);
                }
            }
            Err(reason) => {
                ui.note(
                    &format!("Hindsight is not reachable at {url} ({reason})."),
                    Tone::Warning,
                );
                let (docker_available, choices) = hindsight_unreachable_choices(deps, url);
                let action = pick(ui, "Set up Hindsight", &choices, None)?.unwrap_or(HindsightAction::Skip);
                if let Some(done) = run_hindsight_action(ui, deps, url, action, docker_available)? {
                    return Ok(done);
                }
            }
        }
    }
}

pub(super) fn hindsight_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(deps, 3, "Hindsight memory server"));
    ui.note(
        "Hindsight stores what the model keeps on purpose (memory.propose) and returns it when the model asks (memory.prepare).",
        Tone::Info,
    );
    let (effective, source) = hindsight::resolve::resolve_in(&|name| deps.var(name), &deps.settings);
    if source == hindsight::resolve::Source::Env {
        ui.note(
            &format!(
                "ULTRON_HINDSIGHT_URL is set ({}); it overrides anything saved here.",
                deps.var(hindsight::resolve::HINDSIGHT_URL_ENV).unwrap_or_default()
            ),
            Tone::Warning,
        );
    }
    match effective {
        None => hindsight_when_off(ui, deps),
        Some(url) => hindsight_probe_loop(ui, deps, &url),
    }
}

fn custom_hindsight_url<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<Option<SummaryLine>, SetupQuit> {
    let initial = hindsight::resolve::setting_url(&deps.settings)
        .unwrap_or_else(|| hindsight::resolve::DEFAULT_HINDSIGHT_URL.into());
    let Some(url) = ui.input(
        "Hindsight URL",
        InputOptions {
            initial: Some(initial),
            validate: Some(Box::new(setup_core::validate_base_url)),
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let trimmed = jsrt::string::trim(&url).trim_end_matches('/').to_string();
    let probed = probe(ui, deps, format!("Checking {trimmed}…"), &trimmed)?;
    match &probed {
        Err(reason) => {
            ui.note(
                &format!("Hindsight is not reachable at {trimmed} ({reason})."),
                Tone::Warning,
            );
            if !confirm(ui, "Save this URL anyway?", "Save it", "Back")? {
                return Ok(None);
            }
        }
        Ok(()) => ui.note(&format!("✓ Hindsight is running at {trimmed}."), Tone::Success),
    }
    deps.settings
        .set_global("hindsightUrl", Some(Value::String(trimmed.clone())));
    ui.note(
        &format!(
            "✓ Saved hindsightUrl in {}.",
            config::paths::AgentPaths::new(deps.agent_dir()).settings().display()
        ),
        Tone::Success,
    );
    let detail = format!("{trimmed}{}", if probed.is_ok() { "" } else { " (not reachable yet)" });
    Ok(Some(line(HINDSIGHT, "configured", detail)))
}

fn last_lines(output: &str) -> String {
    let lines: Vec<&str> = output.trim().split('\n').collect();
    lines[lines.len().saturating_sub(3)..].join(" ")
}

fn start_existing_container<U: SetupUi>(ui: &mut U, deps: &SetupDeps) -> Result<Option<bool>, SetupQuit> {
    let docker = deps.run_docker.clone();
    let (ok, output) = ui.busy(&format!("docker start {HINDSIGHT_CONTAINER}"), move || {
        docker(&["start".to_string(), HINDSIGHT_CONTAINER.to_string()], &[])
    })?;
    if !ok {
        ui.note(&format!("✗ docker start failed: {}", last_lines(&output)), Tone::Error);
    }
    Ok(Some(ok))
}

type HindsightLlmInputs = (Option<String>, Option<String>, String);

fn ask_hindsight_llm_inputs<U: SetupUi>(
    ui: &mut U,
    choice: &HindsightLlmChoice,
) -> Result<Option<HindsightLlmInputs>, SetupQuit> {
    let base_url = if choice.needs_base_url {
        let Some(answer) = ui.input(
            "LLM base URL, as seen from inside the container",
            InputOptions {
                initial: Some(choice.default_base_url.unwrap_or("").to_string()),
                description: Some("Use host.docker.internal for a server on this machine.".into()),
                validate: Some(Box::new(setup_core::validate_base_url)),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        Some(answer)
    } else {
        None
    };
    let api_key = if choice.needs_key {
        let Some(answer) = ui.input(
            &format!("API key for {}", choice.label),
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
            return Ok(None);
        };
        Some(answer)
    } else {
        None
    };
    let Some(model) = ui.input(
        "Model for Hindsight (empty: Hindsight's default for the provider)",
        InputOptions {
            initial: Some(String::new()),
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    Ok(Some((base_url, api_key, model)))
}

/// Ask for Hindsight's own LLM, confirm the exact command, then `docker run` it. `Ok(None)`: backed out.
fn install_with_docker<U: SetupUi>(ui: &mut U, deps: &SetupDeps, url: &str) -> Result<Option<bool>, SetupQuit> {
    ui.note(
        "Hindsight needs its own LLM to extract and summarise memories. Its key is passed to the container only.",
        Tone::Info,
    );
    let choices: Vec<(Choice, usize)> = HINDSIGHT_LLM_PROVIDERS
        .iter()
        .enumerate()
        .map(|(i, p)| (Choice::described(p.label, p.provider), i))
        .collect();
    let Some(index) = pick(ui, "LLM provider for Hindsight", &choices, None)? else {
        return Ok(None);
    };
    let choice = &HINDSIGHT_LLM_PROVIDERS[index];
    let Some((base_url, api_key, model)) = ask_hindsight_llm_inputs(ui, choice)? else {
        return Ok(None);
    };
    let args = setup_core::hindsight_docker_args(&HindsightDockerOptions {
        port: setup_core::hindsight_port(url),
        provider: choice.provider.into(),
        model: Some(model),
        base_url: base_url.filter(|b| !b.is_empty()),
        has_api_key: api_key.is_some(),
    });
    ui.note(
        "This will run (the key is passed through the environment, not the command line):",
        Tone::Info,
    );
    ui.note(&format!("  {}", setup_core::format_command("docker", &args)), Tone::Dim);
    ui.note(
        "The image is large (several GB for the full image); the first pull takes a while.",
        Tone::Dim,
    );
    if !confirm(ui, "Run it now?", "Run docker", "Back")? {
        return Ok(None);
    }
    let docker = deps.run_docker.clone();
    let env: Vec<(String, String)> = api_key.map_or_default(|k| {
        vec![(
            "HINDSIGHT_API_LLM_API_KEY".to_string(),
            jsrt::string::trim(&k).to_string(),
        )]
    });
    let (ok, output) = ui.busy("Pulling and starting Hindsight (docker run)…", move || {
        docker(&args, &env)
    })?;
    if !ok {
        ui.note(&format!("✗ docker run failed: {}", last_lines(&output)), Tone::Error);
        return Ok(Some(false));
    }
    ui.note(
        &format!("✓ Started container \"{HINDSIGHT_CONTAINER}\"."),
        Tone::Success,
    );
    Ok(Some(true))
}
