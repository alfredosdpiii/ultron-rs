//! `ultron setup` (cli/setup/wizard.ts): a guided first configuration in six steps (environment, provider and
//! model, Hindsight, the judge seat (Rust only), Loki guardrails, summary). Every step can be skipped with Esc and the wizard can be run again;
//! nothing is overwritten without asking, and secrets are read masked and never printed. The questions go through
//! [`SetupUi`], so the flow is the same in the terminal (`setup_tui`) and in tests.

use crate::Env;
use crate::setup_core::{
    self, CustomEndpoint, Fetch, HINDSIGHT_CONTAINER, HINDSIGHT_IMAGE, HINDSIGHT_LLM_PROVIDERS, HindsightDockerOptions,
    ProbeCommand,
};
use ai::Model;
use config::settings::SettingsManager;
use interactive::selectors::AuthProvider;
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Warning,
    Error,
    Dim,
}

/// The user quit the whole wizard (Ctrl+C).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupQuit;

/// One choice of a question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub description: Option<String>,
}

impl Choice {
    pub fn new(label: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            description: None,
        }
    }

    pub fn described(label: impl Into<String>, description: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            description: Some(description.into()),
        }
    }
}

pub type Validate = Box<dyn Fn(&str) -> Option<String>>;

#[derive(Default)]
pub struct InputOptions {
    /// Read masked: the value is never drawn.
    pub secret: bool,
    pub initial: Option<String>,
    pub description: Option<String>,
    /// An error message for an unacceptable value; the question is asked again.
    pub validate: Option<Validate>,
}

/// How a login ended (`LoginOutcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoginOutcome {
    Ok,
    Cancelled,
    Error(String),
}

/// What the wizard asks through (`SetupUi`). `Err(SetupQuit)` is Ctrl+C; `Ok(None)` is Esc (skip or go back).
pub trait SetupUi {
    fn heading(&mut self, title: &str);
    fn note(&mut self, text: &str, tone: Tone);
    fn select(
        &mut self,
        title: &str,
        choices: &[Choice],
        description: Option<&str>,
    ) -> Result<Option<usize>, SetupQuit>;
    fn input(&mut self, title: &str, options: InputOptions) -> Result<Option<String>, SetupQuit>;
    /// Pi's `/login` provider picker for one kind of login; the index of the picked provider.
    fn pick_login_provider(&mut self, providers: &[AuthProvider], title: &str) -> Result<Option<usize>, SetupQuit>;
    /// Pi's login dialog (browser/device flows, masked API-key prompt); saves to the profile's auth.json.
    fn login(&mut self, env: &Env, option: &AuthProvider) -> Result<LoginOutcome, SetupQuit>;
    fn busy<T: Send + 'static>(
        &mut self,
        message: &str,
        task: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, SetupQuit>;
}

fn pick<U: SetupUi, T: Clone>(
    ui: &mut U,
    title: &str,
    choices: &[(Choice, T)],
    description: Option<&str>,
) -> Result<Option<T>, SetupQuit> {
    let labels: Vec<Choice> = choices.iter().map(|(c, _)| c.clone()).collect();
    Ok(ui
        .select(title, &labels, description)?
        .and_then(|i| choices.get(i).map(|(_, v)| v.clone())))
}

fn confirm<U: SetupUi>(ui: &mut U, title: &str, yes: &str, no: &str) -> Result<bool, SetupQuit> {
    Ok(pick(ui, title, &[(Choice::new(yes), true), (Choice::new(no), false)], None)? == Some(true))
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelTestResult {
    pub ok: bool,
    pub reply: Option<String>,
    pub error: Option<String>,
    pub ms: u64,
}

pub type TestModel = Arc<dyn Fn(&Env, &Model) -> ModelTestResult + Send + Sync>;
pub type RunDocker = Arc<dyn Fn(&[String], &[(String, String)]) -> (bool, String) + Send + Sync>;
/// One real call to a judge backend (`jev`, `d1`) of the profile: what answered and how fast, or why nothing did.
pub type ProbeJudge = Arc<dyn Fn(&Env, &str) -> Result<String, String> + Send + Sync>;
/// Whether a memorantado server answers at a URL (its health check).
pub type ProbeMemorantado = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub struct SetupDeps {
    /// The profile: its agent dir holds settings.json, auth.json and models.json; `var` is the environment.
    pub env: Env,
    pub probe: ProbeCommand,
    /// The interpreter the REPL runs (probed with `--version`).
    pub python: String,
    pub fetch: Fetch,
    /// Global settings: default model and thinking level, the Hindsight URL, Loki.
    pub settings: SettingsManager,
    /// One tiny live request.
    pub test_model: TestModel,
    /// Run docker with extra environment (for the API key): whether it exited 0, and its output.
    pub run_docker: RunDocker,
    /// How long to wait for a new Hindsight container to answer, and how often to ask.
    pub hindsight_wait: Duration,
    pub hindsight_interval: Duration,
    /// The Loki engine version bundled with this installation, when it has one.
    pub loki_version: Option<String>,
    /// The judge seat step's probe; `None` leaves the step out (the TypeScript-parity build, `ULTRON_TUI_PARITY=1`).
    pub probe_judge: Option<ProbeJudge>,
    /// The memory backend step's probe; `None` leaves the step out (as `probe_judge`).
    pub probe_memorantado: Option<ProbeMemorantado>,
}

impl SetupDeps {
    fn agent_dir(&self) -> PathBuf {
        self.env.agent_dir()
    }

    fn var(&self, name: &str) -> Option<String> {
        (self.env.var)(name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryLine {
    pub step: String,
    /// `configured`, `unchanged`, `skipped` or `failed`.
    pub status: &'static str,
    pub detail: String,
}

fn line(step: &str, status: &'static str, detail: impl Into<String>) -> SummaryLine {
    SummaryLine {
        step: step.into(),
        status,
        detail: detail.into(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupResult {
    pub completed: bool,
    pub lines: Vec<SummaryLine>,
    /// A default model is saved and its live test passed (or it was kept as it was).
    pub model_ready: bool,
}

/// The profile's models as the session worker sees them (`createProfileRuntime`), read fresh from disk.
#[derive(Clone)]
pub struct ProfileRuntime {
    env: Env,
    pub available: Vec<Model>,
    pub error: Option<String>,
}

impl ProfileRuntime {
    pub fn load(env: &Env) -> ProfileRuntime {
        let registry = session_core::lane::registry(env);
        ProfileRuntime {
            env: env.clone(),
            available: session_core::lane::available_models(env),
            error: registry.error(),
        }
    }

    /// `getModel`: a catalog or models.json model, ready for requests when it is available.
    pub fn get_model(&self, provider: &str, id: &str) -> Option<Model> {
        if let Some(model) = self.available.iter().find(|m| m.provider == provider && m.id == id) {
            return Some(model.clone());
        }
        session_core::lane::registry(&self.env).find(provider, id)
    }

    pub fn has_configured_auth(&self, provider: &str) -> bool {
        self.available.iter().any(|m| m.provider == provider)
    }

    /// `getAvailable(provider)`.
    pub fn available_of(&self, provider: &str) -> Vec<Model> {
        self.available
            .iter()
            .filter(|m| m.provider == provider)
            .cloned()
            .collect()
    }
}

/// `Step n/total: title`; without the judge step (the TypeScript-parity build) the later steps move up one, and
/// the memory backend step (before Hindsight, when there is one) moves the steps from Hindsight on down one.
fn step_title(deps: &SetupDeps, index: usize, title: &str) -> String {
    let judge = usize::from(deps.probe_judge.is_some());
    let memory = usize::from(deps.probe_memorantado.is_some());
    let index = if index > 4 { index - 1 + judge } else { index };
    let index = if index >= 3 { index + memory } else { index };
    format!("Step {index}/{}: {title}", 5 + judge + memory)
}

fn describe(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn text(value: Option<Value>) -> Option<String> {
    value.and_then(|v| v.as_str().map(str::to_string))
}

/// Run the wizard; `SetupQuit` ends it early with what was done so far.
pub fn run_setup_wizard<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> SetupResult {
    let mut lines = Vec::new();
    let mut model_ready = false;
    let mut completed = false;
    let steps = (|| -> Result<(), SetupQuit> {
        environment_step(ui, deps);
        let (model_line, ready) = provider_step(ui, deps)?;
        lines.push(model_line);
        model_ready = ready;
        if deps.probe_memorantado.is_some() {
            lines.push(memory_backend_step(ui, deps)?);
        }
        lines.push(hindsight_step(ui, deps)?);
        if deps.probe_judge.is_some() {
            lines.push(judge_step(ui, deps)?);
        }
        lines.push(loki_step(ui, deps)?);
        completed = true;
        Ok(())
    })();
    if steps.is_err() {
        ui.note(
            "Setup stopped. Anything saved before this point is kept.",
            Tone::Warning,
        );
    }
    summary_step(ui, deps, &lines, completed);
    SetupResult {
        completed,
        lines,
        model_ready,
    }
}

// ------------------------------------------------------------------------------------------------- 1. environment

fn environment_step<U: SetupUi>(ui: &mut U, deps: &SetupDeps) {
    ui.heading(&step_title(deps, 1, "Environment"));
    for check in setup_core::check_environment(&deps.probe, &deps.python) {
        let mark = if check.ok {
            "✓"
        } else if check.optional {
            "–"
        } else {
            "✗"
        };
        let tone = if check.ok {
            Tone::Success
        } else if check.optional {
            Tone::Dim
        } else {
            Tone::Error
        };
        ui.note(&format!("{mark} {}: {}", check.label, check.detail), tone);
        if let (false, Some(fix)) = (check.ok, &check.fix) {
            ui.note(&format!("  {fix}"), Tone::Warning);
        }
    }
}

// -------------------------------------------------------------------------------------------- 2. provider and model

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProviderPath {
    OAuth,
    ApiKey,
    Custom,
    Keep,
    Skip,
}

const PROVIDER_STEP: &str = "Provider and model";

fn load_runtime<U: SetupUi>(ui: &mut U, deps: &SetupDeps, message: &str) -> Result<ProfileRuntime, SetupQuit> {
    let env = deps.env.clone();
    ui.busy(message, move || ProfileRuntime::load(&env))
}

fn provider_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<(SummaryLine, bool), SetupQuit> {
    ui.heading(&step_title(deps, 2, PROVIDER_STEP));
    let mut runtime = load_runtime(ui, deps, "Loading providers…")?;
    let current = current_default_model(&runtime, &deps.settings);
    if let Some(current) = &current {
        ui.note(&format!("Current default: {}", describe(current)), Tone::Info);
    } else if !runtime.available.is_empty() {
        ui.note(
            &format!(
                "{} models are available, but no default is saved.",
                runtime.available.len()
            ),
            Tone::Info,
        );
    } else {
        ui.note("No provider is configured yet.", Tone::Warning);
    }
    loop {
        let mut choices = Vec::new();
        if let Some(current) = &current {
            choices.push((Choice::new(format!("Keep {}", describe(current))), ProviderPath::Keep));
        }
        choices.extend([
            (
                Choice::described(
                    "Sign in with a subscription",
                    "Claude Pro/Max, ChatGPT Plus/Pro, GitHub Copilot, … (Pi's /login)",
                ),
                ProviderPath::OAuth,
            ),
            (
                Choice::described("Use an API key", "Anthropic, OpenAI, Google, OpenRouter, …"),
                ProviderPath::ApiKey,
            ),
            (
                Choice::described(
                    "Custom OpenAI-compatible endpoint",
                    "A proxy, Ollama, LM Studio, vLLM, … (written to models.json)",
                ),
                ProviderPath::Custom,
            ),
            (Choice::new("Skip this step"), ProviderPath::Skip),
        ]);
        let path = pick(ui, "How should Ultron reach a model?", &choices, None)?;
        let path = match path {
            None | Some(ProviderPath::Skip) => {
                let detail = current
                    .as_ref()
                    .map_or_else(|| "no model".to_string(), |c| format!("kept {}", describe(c)));
                return Ok((line(PROVIDER_STEP, "skipped", detail), false));
            }
            Some(path) => path,
        };
        if path == ProviderPath::Keep {
            let current = current.clone().expect("Keep is offered with a current default");
            let tested = test_and_report(ui, deps, &current)?;
            let detail = format!(
                "{}{}",
                describe(&current),
                if tested { "" } else { " (live test failed)" }
            );
            return Ok((line(PROVIDER_STEP, "unchanged", detail), tested));
        }
        let provider = if path == ProviderPath::Custom {
            custom_endpoint(ui, deps)?
        } else {
            login_provider(ui, deps, path == ProviderPath::OAuth)?
        };
        let Some(provider) = provider else {
            continue;
        };
        runtime = load_runtime(ui, deps, "Reloading providers…")?;
        if let Some(outcome) = choose_default_model(ui, deps, &runtime, &provider)? {
            return Ok(outcome);
        }
    }
}

fn current_default_model(runtime: &ProfileRuntime, settings: &SettingsManager) -> Option<Model> {
    let provider = text(settings.default_provider())?;
    let id = text(settings.default_model())?;
    let model = runtime.get_model(&provider, &id)?;
    runtime.has_configured_auth(&model.provider).then_some(model)
}

/// `loginProviderOptions(runtime, authType)`.
pub fn login_provider_options(env: &Env, oauth: bool) -> Vec<AuthProvider> {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let kind = if oauth { "oauth" } else { "api_key" };
    crate::interactive::login_options(env, &auth)
        .into_iter()
        .filter(|p| p.auth_type == kind)
        .collect()
}

/// Pi's `/login` for one provider; its id once credentials are saved.
fn login_provider<U: SetupUi>(ui: &mut U, deps: &SetupDeps, oauth: bool) -> Result<Option<String>, SetupQuit> {
    let options = login_provider_options(&deps.env, oauth);
    if options.is_empty() {
        ui.note(
            if oauth {
                "No subscription providers are available."
            } else {
                "No API-key providers are available."
            },
            Tone::Warning,
        );
        return Ok(None);
    }
    let title = if oauth {
        "Sign in with a subscription"
    } else {
        "Choose the provider for your API key"
    };
    loop {
        let Some(index) = ui.pick_login_provider(&options, title)? else {
            return Ok(None);
        };
        let option = options[index].clone();
        if let Some((kind, source)) = &option.status {
            let keep = confirm(
                ui,
                &format!(
                    "{} is already configured ({}).",
                    option.name,
                    source.as_deref().unwrap_or(kind)
                ),
                "Use it as it is",
                "Sign in again and replace it",
            )?;
            if keep {
                return Ok(Some(option.id));
            }
        }
        if option.auth_type == "api_key" && !option.api_key_method.as_ref().is_some_and(|(_, login)| *login) {
            let name = option
                .api_key_method
                .as_ref()
                .map_or(option.name.as_str(), |(name, _)| name.as_str());
            ui.note(
                &format!("{name} is configured outside Ultron (environment variables or cloud credentials)."),
                Tone::Warning,
            );
            continue;
        }
        match ui.login(&deps.env, &option)? {
            LoginOutcome::Ok => {
                let auth = config::paths::AgentPaths::new(deps.agent_dir()).auth();
                let text = if option.auth_type == "oauth" {
                    format!("✓ Signed in to {}. Saved to {}.", option.name, auth.display())
                } else {
                    format!("✓ Saved the API key for {} to {}.", option.name, auth.display())
                };
                ui.note(&text, Tone::Success);
                return Ok(Some(option.id));
            }
            LoginOutcome::Cancelled => {
                ui.note(
                    &format!("Sign-in to {} cancelled; nothing was saved.", option.name),
                    Tone::Dim,
                );
            }
            LoginOutcome::Error(error) => ui.note(&format!("✗ {}: {error}", option.name), Tone::Error),
        }
    }
}

/// Ask for a custom OpenAI-compatible endpoint and write it to models.json; its provider id.
fn custom_endpoint<U: SetupUi>(ui: &mut U, deps: &SetupDeps) -> Result<Option<String>, SetupQuit> {
    let models_path = config::paths::AgentPaths::new(deps.agent_dir()).models();
    ui.note(
        &format!(
            "The endpoint is saved in {} (readable only by you).",
            models_path.display()
        ),
        Tone::Dim,
    );
    let Some(base_url) = ui.input(
        "Base URL of the endpoint (including /v1)",
        InputOptions {
            initial: Some("http://localhost:11434/v1".into()),
            validate: Some(Box::new(setup_core::validate_base_url)),
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let Some(api_key) = ui.input(
        "API key (leave empty if the endpoint needs none)",
        InputOptions {
            secret: true,
            ..Default::default()
        },
    )?
    else {
        return Ok(None);
    };
    let mut provider_id: Option<String> = None;
    let provider_id = loop {
        let Some(id) = ui.input(
            "A short name for this provider",
            InputOptions {
                initial: Some(provider_id.clone().unwrap_or_else(|| "custom".into())),
                validate: Some(Box::new(setup_core::validate_provider_id)),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        provider_id = Some(id.clone());
        if !setup_core::custom_provider_exists(&models_path, &id) {
            break id;
        }
        if confirm(
            ui,
            &format!("models.json already has a provider named \"{id}\"."),
            "Replace it",
            "Choose another name",
        )? {
            break id;
        }
    };
    let apis = [
        (
            Choice::described("OpenAI Chat Completions", "most proxies, Ollama, vLLM, LM Studio"),
            "openai-completions",
        ),
        (
            Choice::described("OpenAI Responses", "/v1/responses"),
            "openai-responses",
        ),
    ];
    let Some(api) = pick(ui, "Which API does it speak?", &apis, None)? else {
        return Ok(None);
    };
    let fetch = deps.fetch.clone();
    let (url, key) = (base_url.clone(), api_key.clone());
    let listed = ui.busy(&format!("Asking {base_url} for its models…"), move || {
        setup_core::list_endpoint_models(&url, Some(&key), &fetch)
    })?;
    match listed.as_ref().filter(|l| !l.is_empty()) {
        Some(listed) => {
            let shown = listed.iter().take(20).cloned().collect::<Vec<_>>().join(", ");
            ui.note(
                &format!(
                    "The endpoint lists {} model{}: {shown}{}",
                    listed.len(),
                    if listed.len() == 1 { "" } else { "s" },
                    if listed.len() > 20 { ", …" } else { "" }
                ),
                Tone::Info,
            );
        }
        None => ui.note(
            "The endpoint did not list its models; enter the ids yourself.",
            Tone::Dim,
        ),
    }
    let model_ids = loop {
        let initial = match listed.as_deref() {
            Some([only]) => only.clone(),
            _ => String::new(),
        };
        let Some(answer) = ui.input(
            "Model ids to use (comma-separated)",
            InputOptions {
                initial: Some(initial),
                validate: Some(Box::new(|value: &str| {
                    setup_core::parse_model_ids(value)
                        .is_empty()
                        .then(|| "Enter at least one model id".to_string())
                })),
                ..Default::default()
            },
        )?
        else {
            return Ok(None);
        };
        let ids = setup_core::parse_model_ids(&answer);
        let unknown: Vec<String> = match &listed {
            Some(listed) => ids.iter().filter(|id| !listed.contains(id)).cloned().collect(),
            None => Vec::new(),
        };
        if unknown.is_empty() {
            break ids;
        }
        if confirm(
            ui,
            &format!("The endpoint does not list {}.", unknown.join(", ")),
            "Use them anyway",
            "Edit the list",
        )? {
            break ids;
        }
    };
    let existing = match setup_core::read_text_file(&models_path) {
        Ok(text) => text,
        Err(error) => {
            ui.note(
                &format!("✗ {error} ({} was not changed)", models_path.display()),
                Tone::Error,
            );
            return Ok(None);
        }
    };
    let endpoint = CustomEndpoint {
        provider_id: provider_id.clone(),
        base_url,
        api_key: Some(api_key).filter(|k| !jsrt::string::trim(k).is_empty()),
        api: api.to_string(),
        model_ids,
    };
    let (merged, replaced) = match setup_core::merge_custom_endpoint(existing.as_deref(), &endpoint) {
        Ok(merged) => merged,
        Err(error) => {
            ui.note(
                &format!("✗ {error} ({} was not changed)", models_path.display()),
                Tone::Error,
            );
            return Ok(None);
        }
    };
    if let Err(error) = setup_core::write_private_file(&models_path, &merged) {
        ui.note(
            &format!("✗ {error} ({} was not changed)", models_path.display()),
            Tone::Error,
        );
        return Ok(None);
    }
    ui.note(
        &format!(
            "✓ {} provider \"{provider_id}\" in {}.",
            if replaced { "Replaced" } else { "Added" },
            models_path.display()
        ),
        Tone::Success,
    );
    Ok(Some(provider_id))
}

/// `Ok(None)`: back to the choice of path.
fn choose_default_model<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    runtime: &ProfileRuntime,
    provider: &str,
) -> Result<Option<(SummaryLine, bool)>, SetupQuit> {
    if let Some(error) = &runtime.error {
        ui.note(error, Tone::Error);
    }
    let models = runtime.available_of(provider);
    if models.is_empty() {
        ui.note(
            &format!("No models of \"{provider}\" are available yet; check its credentials."),
            Tone::Error,
        );
        return Ok(None);
    }
    let name = interactive::selectors::provider_name(provider);
    let choices: Vec<(Choice, Model)> = models
        .iter()
        .map(|m| {
            let choice = if m.name != m.id {
                Choice::described(m.id.clone(), m.name.clone())
            } else {
                Choice::new(m.id.clone())
            };
            (choice, m.clone())
        })
        .collect();
    loop {
        let title = format!("Default model ({} from {name})", models.len());
        let Some(model) = pick(ui, &title, &choices, None)? else {
            return Ok(None);
        };
        let thinking = choose_thinking(ui, deps, &model)?;
        let mut tested = test_and_report(ui, deps, &model)?;
        if !tested {
            let next = pick(
                ui,
                "The live test failed.",
                &[
                    (Choice::new("Try another model"), "other"),
                    (Choice::new("Run the test again"), "retry"),
                    (Choice::new("Save it anyway"), "save"),
                ],
                None,
            )?;
            match next {
                None | Some("other") => continue,
                Some("retry") => {
                    tested = test_and_report(ui, deps, &model)?;
                    if !tested {
                        continue;
                    }
                }
                Some(_) => {}
            }
        }
        deps.settings
            .set_global("defaultProvider", Some(Value::String(model.provider.clone())));
        deps.settings
            .set_global("defaultModel", Some(Value::String(model.id.clone())));
        if let Some(thinking) = &thinking {
            deps.settings
                .set_global("defaultThinkingLevel", Some(Value::String(thinking.clone())));
        }
        ui.note(
            &format!(
                "✓ Default model saved: {}{}.",
                describe(&model),
                thinking.as_ref().map(|t| format!(", thinking {t}")).unwrap_or_default()
            ),
            Tone::Success,
        );
        let detail = format!(
            "{}{}",
            describe(&model),
            if tested {
                ", live test passed"
            } else {
                ", live test failed"
            }
        );
        return Ok(Some((line(PROVIDER_STEP, "configured", detail), tested)));
    }
}

fn choose_thinking<U: SetupUi>(ui: &mut U, deps: &SetupDeps, model: &Model) -> Result<Option<String>, SetupQuit> {
    let levels = ai::models::supported_thinking_levels(model);
    if levels.len() <= 1 {
        return Ok(None);
    }
    let current = deps
        .settings
        .settings()
        .get("defaultThinkingLevel")
        .and_then(Value::as_str)
        .unwrap_or(model_scope::DEFAULT_THINKING_LEVEL)
        .to_string();
    let mut choices = vec![(Choice::new(format!("Keep {current}")), None)];
    choices.extend(
        levels
            .iter()
            .filter(|l| **l != current)
            .map(|l| (Choice::new(*l), Some(l.to_string()))),
    );
    Ok(pick(ui, "Thinking level", &choices, None)?.flatten())
}

fn test_and_report<U: SetupUi>(ui: &mut U, deps: &SetupDeps, model: &Model) -> Result<bool, SetupQuit> {
    let (test, env, target) = (deps.test_model.clone(), deps.env.clone(), model.clone());
    let result = ui.busy(
        &format!("Testing {} with one tiny request…", describe(model)),
        move || test(&env, &target),
    )?;
    let seconds = jsrt::number::to_fixed(result.ms as f64 / 1000.0, 1);
    if result.ok {
        let reply = result
            .reply
            .as_deref()
            .map(|r| {
                let collapsed = r
                    .split(jsrt::string::is_space)
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>();
                let joined = collapsed.join(" ");
                jsrt::utf16::head(jsrt::string::trim(&joined), 60).to_string()
            })
            .unwrap_or_default();
        let shown = if reply.is_empty() {
            String::new()
        } else {
            format!(": \"{reply}\"")
        };
        ui.note(&format!("✓ Live test passed in {seconds}s{shown}"), Tone::Success);
    } else {
        ui.note(
            &format!(
                "✗ Live test failed after {seconds}s: {}",
                result.error.as_deref().unwrap_or("unknown error")
            ),
            Tone::Error,
        );
    }
    Ok(result.ok)
}

// ------------------------------------------------------------------------------------------------------ 3. Hindsight

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

fn probe<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    message: String,
    url: &str,
) -> Result<Result<(), String>, SetupQuit> {
    let (fetch, url) = (deps.fetch.clone(), url.to_string());
    ui.busy(&message, move || setup_core::probe_hindsight(&url, &fetch))
}

const MEMORY_BACKEND: &str = "Memory backend";

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

/// Which memory server `memory.*`, automatic memory and dreaming use: memorantado (claims with keys, curation,
/// file evidence; dreaming writes its briefs) or Hindsight (its own model writes briefs); unset, memorantado once it
/// answers at the start of a session (kept from then on: memorantado::resolve), else Hindsight.
fn memory_backend_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
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
        let current = memorantado::resolve::choice(&deps.settings);
        let Some(endpoint) = memorantado::Endpoint::configured(&|name| deps.var(name), &deps.settings) else {
            ui.note("memorantado is turned off (ULTRON_MEMORANTADO_URL).", Tone::Info);
            return Ok(line(MEMORY_BACKEND, "unchanged", "Hindsight (memorantado off)"));
        };
        let url = endpoint.url;
        let (target, probe) = (url.clone(), probe.clone());
        let up = ui.busy(&format!("Looking for memorantado at {url}…"), move || probe(&target))?;
        ui.note(
            &if up {
                format!("✓ memorantado is running at {url}.")
            } else {
                format!("memorantado is not reachable at {url}.")
            },
            if up { Tone::Success } else { Tone::Warning },
        );
        let detected = memorantado::resolve::remembered(&deps.agent_dir());
        let now = match (current, detected) {
            (Some(choice), _) => choice.as_str(),
            (None, Some(memorantado::Choice::Memorantado)) => "unset (memorantado, detected)",
            (None, _) => "unset (memorantado once it answers, else Hindsight)",
        };
        ui.note(&format!("memory.backend: {now}."), Tone::Dim);
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
        let (choice, detail) = match pick(ui, "Memory backend", &choices, None)? {
            None | Some(BackendAction::Skip) => return Ok(line(MEMORY_BACKEND, "skipped", now)),
            Some(BackendAction::Url) => {
                let options = InputOptions {
                    initial: Some(url.clone()),
                    validate: Some(Box::new(setup_core::validate_base_url)),
                    ..Default::default()
                };
                let Some(input) = ui.input("memorantado URL", options)? else {
                    continue;
                };
                let input = input.trim().trim_end_matches('/').to_string();
                deps.settings
                    .set_global(memorantado::resolve::URL_SETTING, Some(Value::String(input)));
                continue;
            }
            Some(BackendAction::Memorantado) => {
                (Some(memorantado::Choice::Memorantado), format!("memorantado at {url}"))
            }
            Some(BackendAction::Hindsight) => (Some(memorantado::Choice::Hindsight), "Hindsight".to_string()),
            Some(BackendAction::Automatic) => (None, "detected".to_string()),
        };
        let status = if choice == current { "unchanged" } else { "configured" };
        save_backend(deps, choice);
        return Ok(line(MEMORY_BACKEND, status, detail));
    }
}

fn hindsight_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
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
    let Some(url) = effective else {
        ui.note("Memory is turned off.", Tone::Info);
        if !confirm(ui, "Turn memory on?", "Yes, set a Hindsight URL", "No, leave it off")? {
            return Ok(line(HINDSIGHT, "unchanged", "off"));
        }
        return Ok(custom_hindsight_url(ui, deps)?.unwrap_or_else(|| line(HINDSIGHT, "unchanged", "off")));
    };
    loop {
        let probed = probe(ui, deps, format!("Looking for Hindsight at {url}…"), &url)?;
        let reason = match probed {
            Ok(()) => {
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
                    if let Some(saved) = custom_hindsight_url(ui, deps)? {
                        return Ok(saved);
                    }
                    continue;
                }
                return Ok(line(HINDSIGHT, "unchanged", format!("running at {url}")));
            }
            Err(reason) => reason,
        };
        ui.note(
            &format!("Hindsight is not reachable at {url} ({reason})."),
            Tone::Warning,
        );
        let docker_available = (deps.probe)("docker", &["--version"]).is_some();
        let filter = format!("name=^{HINDSIGHT_CONTAINER}$");
        let existing = if docker_available {
            (deps.probe)("docker", &["ps", "-a", "--filter", &filter, "--format", "{{.Status}}"])
        } else {
            None
        }
        .filter(|s| !s.is_empty());
        let local = setup_core::is_local_url(&url);
        let mut choices = Vec::new();
        if docker_available && local {
            match &existing {
                Some(status) => choices.push((
                    Choice::described(
                        format!("Start the existing \"{HINDSIGHT_CONTAINER}\" container"),
                        status.clone(),
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
        let action = match pick(ui, "Set up Hindsight", &choices, None)? {
            None | Some(HindsightAction::Skip) => {
                return Ok(line(HINDSIGHT, "skipped", format!("not reachable at {url}")));
            }
            Some(action) => action,
        };
        match action {
            HindsightAction::Manual => {
                if !docker_available {
                    ui.note("Docker was not found on this machine.", Tone::Dim);
                }
                for instruction in setup_core::hindsight_manual_instructions(&url) {
                    ui.note(&instruction, Tone::Dim);
                }
                if confirm(
                    ui,
                    "Check again once it is running?",
                    "Check again",
                    "Continue without it",
                )? {
                    continue;
                }
                return Ok(line(HINDSIGHT, "skipped", "manual install instructions shown"));
            }
            HindsightAction::Off => {
                deps.settings
                    .set_global("hindsightUrl", Some(Value::String("off".into())));
                ui.note(
                    "Memory is turned off. Run `ultron setup` again to turn it on.",
                    Tone::Info,
                );
                return Ok(line(HINDSIGHT, "configured", "off"));
            }
            HindsightAction::Url => {
                if let Some(saved) = custom_hindsight_url(ui, deps)? {
                    return Ok(saved);
                }
                continue;
            }
            _ => {}
        }
        let started = if action == HindsightAction::Start {
            start_existing_container(ui, deps)?
        } else {
            install_with_docker(ui, deps, &url)?
        };
        let Some(started) = started else {
            continue;
        };
        if !started {
            return Ok(line(HINDSIGHT, "failed", "the container did not start"));
        }
        let (fetch, wait, interval, target) = (
            deps.fetch.clone(),
            deps.hindsight_wait,
            deps.hindsight_interval,
            url.clone(),
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
        return Ok(line(
            HINDSIGHT,
            "failed",
            format!("container \"{HINDSIGHT_CONTAINER}\" started, not answering yet"),
        ));
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
    let mut base_url = None;
    if choice.needs_base_url {
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
        base_url = Some(answer);
    }
    let mut api_key = None;
    if choice.needs_key {
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
        api_key = Some(answer);
    }
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
    let env: Vec<(String, String)> = api_key
        .map(|k| {
            vec![(
                "HINDSIGHT_API_LLM_API_KEY".to_string(),
                jsrt::string::trim(&k).to_string(),
            )]
        })
        .unwrap_or_default();
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

// ---------------------------------------------------------------------------------------------------- 4. judge seat

const JUDGE: &str = "Judge";

#[derive(Clone, Copy, PartialEq, Eq)]
enum JudgeAction {
    JevKey,
    D1,
    Check,
    Done,
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

fn judge_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(deps, 4, "Judge seat (Jev, d1)"));
    ui.note(
        "The judge answers closed questions (is this memory still true? does this code show the finding?) for batch and background work, never on every turn: Jev (TypeSafe) first, a d1 server when Jev cannot. Keys go to auth.json and are never shown.",
        Tone::Info,
    );
    let mut changed = Vec::new();
    loop {
        let d1_url = config::keys::judge_settings(deps.settings.global_settings()).d1_url;
        let jev = key_origin(deps, config::auth::TYPESAFE_KEY, Some(judge::backend::JEV_URL));
        let d1_key = key_origin(deps, config::auth::D1_KEY, d1_url.as_deref());
        let usable = |origin: &str| matches!(origin, "from the environment" | "in auth.json");
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
        if usable(jev) {
            configured.push("jev");
        }
        if d1_url.is_some() && usable(d1_key) {
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
            Choice::new(if changed.is_empty() { "Skip" } else { "Done" }),
            JudgeAction::Done,
        ));
        match pick(ui, "Judge seat", &choices, None)? {
            None | Some(JudgeAction::Done) => break,
            Some(JudgeAction::Check) => {
                let outcomes = probe_judge(ui, deps, &configured)?;
                if changed.is_empty() {
                    return Ok(line(JUDGE, "unchanged", outcomes.join(", ")));
                }
            }
            Some(JudgeAction::JevKey) => {
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
                    continue;
                };
                let auth = config::paths::AgentPaths::new(deps.agent_dir()).auth();
                match config::auth::save_service_key(&auth, &config::auth::TYPESAFE_KEY, &key, judge::backend::JEV_URL)
                {
                    Ok(()) => {
                        ui.note("✓ Saved the TypeSafe key in auth.json.", Tone::Success);
                        changed.push("jev");
                        probe_judge(ui, deps, &["jev"])?;
                    }
                    Err(error) => ui.note(&format!("✗ Could not save the key: {error}"), Tone::Error),
                }
            }
            Some(JudgeAction::D1) => {
                let Some(url) = ui.input(
                    "d1 server URL",
                    InputOptions {
                        initial: Some(d1_url.clone().unwrap_or_default()),
                        description: Some("For example a tailscale serve address of the machine running d1.".into()),
                        // Its key goes there: https, or plain http to this machine only.
                        validate: Some(Box::new(|value: &str| {
                            setup_core::validate_base_url(value)
                                .or_else(|| config::auth::secure_url(jsrt::string::trim(value)).err())
                        })),
                        ..Default::default()
                    },
                )?
                else {
                    continue;
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
                // A stored key is bound to the server it was saved for: a new server needs it entered again.
                let kept = key_origin(deps, config::auth::D1_KEY, Some(&url));
                let key = ui.input(
                    if usable(kept) {
                        "d1 key (Esc keeps the current one)"
                    } else {
                        "d1 key"
                    },
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
            }
        }
    }
    Ok(if changed.is_empty() {
        line(JUDGE, "skipped", "Jev and d1 unchanged")
    } else {
        line(JUDGE, "configured", changed.join(", "))
    })
}

// ----------------------------------------------------------------------------------------------------------- 5. Loki

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

fn loki_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<SummaryLine, SetupQuit> {
    ui.heading(&step_title(deps, 5, "Loki guardrails"));
    ui.note(
        "Loki checks every file the agent writes with deterministic rules and real analyzers: edit() and write() are checked before the file changes, other writes after the cell.",
        Tone::Info,
    );
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
        let text = if analyzer.found {
            format!("✓ {} ({})", analyzer.name, analyzer.language)
        } else {
            format!(
                "– {} ({}): not found. {}",
                analyzer.name, analyzer.language, analyzer.hint
            )
        };
        ui.note(&text, if analyzer.found { Tone::Success } else { Tone::Dim });
    }
    if analyzers.iter().any(|a| !a.found) {
        ui.note(
            "Missing analyzers are reported as NOT CHECKED, never as clean.",
            Tone::Dim,
        );
    }
    let initial = loki::settings::saved(deps.settings.global_settings());
    let mut current = initial.clone();
    loop {
        ui.note(&format!("Loki: {}.", describe_loki(&current)), Tone::Info);
        let mode = current.get("mode").and_then(Value::as_str).unwrap_or("on").to_string();
        let off = |key: &str| current.get(key) == Some(&Value::Bool(false));
        let choices: Vec<(Choice, LokiAction)> = if mode == "off" {
            vec![
                (Choice::new("Keep Loki off"), LokiAction::Keep),
                (Choice::new("Turn Loki on"), LokiAction::On),
            ]
        } else {
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
        };
        let action = match pick(ui, "Loki guardrails", &choices, None)? {
            None | Some(LokiAction::Keep) => break,
            Some(action) => action,
        };
        match action {
            LokiAction::AutoInit => {
                let value = off("autoInit");
                current.insert("autoInit".into(), Value::Bool(value));
            }
            LokiAction::AutoCommit => {
                let value = off("autoCommit");
                current.insert("autoCommit".into(), Value::Bool(value));
            }
            LokiAction::Advise => {
                current.insert("mode".into(), "advise".into());
            }
            LokiAction::Off => {
                current.insert("mode".into(), "off".into());
            }
            _ => {
                current.insert("mode".into(), "on".into());
            }
        }
    }
    if describe_loki(&current) == describe_loki(&initial) {
        return Ok(line("Loki", "unchanged", describe_loki(&current)));
    }
    let patch: Vec<(&str, Option<Value>)> = current.iter().map(|(k, v)| (k.as_str(), Some(v.clone()))).collect();
    let loki = loki::settings::patched(deps.settings.global_settings(), &patch);
    deps.settings.set_global(loki::settings::KEY, Some(loki));
    Ok(line("Loki", "configured", describe_loki(&current)))
}

// -------------------------------------------------------------------------------------------------------- 6. summary

fn summary_step<U: SetupUi>(ui: &mut U, deps: &SetupDeps, lines: &[SummaryLine], completed: bool) {
    ui.heading(&step_title(deps, 6, "Summary"));
    for l in lines {
        let tone = match l.status {
            "configured" => Tone::Success,
            "failed" => Tone::Error,
            _ => Tone::Dim,
        };
        ui.note(&format!("{}: {} — {}", l.step, l.status, l.detail), tone);
    }
    if !completed {
        ui.note("Not all steps ran.", Tone::Warning);
    }
    ui.note(
        &format!(
            "Configuration lives in {} (settings.json, auth.json, models.json).",
            deps.agent_dir().display()
        ),
        Tone::Info,
    );
    ui.note(
        "Run `ultron setup` again at any time to change it; start working with `ultron`.",
        Tone::Info,
    );
}

// ------------------------------------------------------------------------------------------------- the live test

const MODEL_TEST_TIMEOUT: Duration = Duration::from_secs(90);

/// `testModel`: one short request, as small as the provider allows.
pub fn test_model(env: &Env, model: &Model) -> ModelTestResult {
    use agent::StreamFn;
    let started = std::time::Instant::now();
    let elapsed = |started: std::time::Instant| started.elapsed().as_millis() as u64;
    let key = session_core::lane::api_key(&session_core::ModelFlags::default(), env, &model.provider);
    let provider = session_core::lane::Provider::with_key(key.clone());
    let context = ai::TranscriptContext {
        messages: vec![ai::Message::User(ai::UserMessage {
            content: ai::UserMessageContent::Text("Reply with the single word: ok".into()),
            timestamp: jsrt::date::now_ms(),
        })],
    };
    let abort = ai::AbortSignal::new();
    let mut options = ai::SimpleStreamOptions::default();
    options.base.api_key = key;
    options.base.signal = Some(abort.clone());
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            return ModelTestResult {
                ok: false,
                reply: None,
                error: Some(error.to_string()),
                ms: elapsed(started),
            };
        }
    };
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = runtime.block_on(async {
        let stream = provider.stream(model, &context, &options, &mut sink);
        match tokio::time::timeout(MODEL_TEST_TIMEOUT, stream).await {
            Ok(message) => Ok(message),
            Err(_) => {
                abort.abort();
                Err("The operation was aborted due to timeout".to_string())
            }
        }
    });
    let ms = elapsed(started);
    match message {
        Err(error) => ModelTestResult {
            ok: false,
            reply: None,
            error: Some(error),
            ms,
        },
        Ok(message) => {
            let stop = serde_json::to_value(message.stop_reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            if stop == "error" || stop == "aborted" {
                return ModelTestResult {
                    ok: false,
                    reply: None,
                    error: Some(message.error_message.unwrap_or_else(|| format!("request {stop}"))),
                    ms,
                };
            }
            let reply = message
                .content
                .iter()
                .filter_map(|c| match c {
                    ai::AssistantContent::Text(text) => Some(text.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            ModelTestResult {
                ok: true,
                reply: Some(reply),
                error: None,
                ms,
            }
        }
    }
}

/// `runDocker`: docker with the run's PATH and `extra` environment; exit status and stdout+stderr.
pub fn run_docker(path: Option<String>) -> RunDocker {
    Arc::new(move |args: &[String], extra: &[(String, String)]| {
        let mut command = std::process::Command::new("docker");
        command.args(args).stdin(std::process::Stdio::null());
        if let Some(path) = &path {
            command.env("PATH", path);
        }
        for (k, v) in extra {
            command.env(k, v);
        }
        match command.output() {
            Ok(out) => (
                out.status.success(),
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ),
            ),
            Err(error) => (false, error.to_string()),
        }
    })
}

/// `createSetupDeps`: the real probes, fetch, live test and docker for `env`'s profile.
pub fn create_setup_deps(env: &Env) -> SetupDeps {
    let path = (env.var)("PATH");
    let probe_path = path.clone();
    SetupDeps {
        env: env.clone(),
        probe: Arc::new(move |command: &str, args: &[&str]| {
            setup_core::probe_command(probe_path.as_deref(), command, args)
        }),
        python: kernel::env::python(|name| (env.var)(name)),
        fetch: setup_core::http_fetch_with(ai::net::ProxyEnv::from_lookup(&*env.var)),
        settings: SettingsManager::create(&env.cwd, &env.agent_dir(), false),
        test_model: Arc::new(test_model),
        run_docker: run_docker(path),
        hindsight_wait: Duration::from_secs(5 * 60),
        hindsight_interval: Duration::from_secs(3),
        loki_version: Some(loki::analyzers::bundled_version()),
        probe_judge: ((env.var)(interactive::qol::PARITY_ENV).as_deref().map(str::trim) != Some("1")).then(|| {
            Arc::new(|env: &Env, backend: &str| {
                let seat = crate::judge_cli::seat(env, false, None)?;
                judge::bench::probe(&seat, backend, &kernel::Signal::new())
            }) as ProbeJudge
        }),
        probe_memorantado: ((env.var)(interactive::qol::PARITY_ENV).as_deref().map(str::trim) != Some("1"))
            .then(|| Arc::new(memorantado::resolve::probe) as ProbeMemorantado),
    }
}
