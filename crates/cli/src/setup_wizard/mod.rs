//! `ultron setup` (cli/setup/wizard.ts): a guided first configuration in six steps (environment, provider and
//! model, Hindsight, the judge seat (Rust only), Loki guardrails, summary). Every step can be skipped with Esc and the wizard can be run again;
//! nothing is overwritten without asking, and secrets are read masked and never printed. The questions go through
//! [`SetupUi`], so the flow is the same in the terminal (`setup_tui`) and in tests.

mod extras;
mod memory;
mod provider;

pub use extras::describe_loki;
pub use provider::login_provider_options;

use crate::Env;
use crate::setup_core::{self, Fetch, ProbeCommand};
use ai::Model;
use config::settings::SettingsManager;
use interactive::selectors::AuthProvider;
use serde_json::Value;
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

pub(super) fn pick<U: SetupUi, T: Clone>(
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

pub(super) fn confirm<U: SetupUi>(ui: &mut U, title: &str, yes: &str, no: &str) -> Result<bool, SetupQuit> {
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
    pub(super) fn agent_dir(&self) -> PathBuf {
        self.env.agent_dir()
    }

    pub(super) fn var(&self, name: &str) -> Option<String> {
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

pub(super) fn line(step: &str, status: &'static str, detail: impl Into<String>) -> SummaryLine {
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
pub(super) fn step_title(deps: &SetupDeps, index: usize, title: &str) -> String {
    let judge = usize::from(deps.probe_judge.is_some());
    let memory = usize::from(deps.probe_memorantado.is_some());
    let index = if index > 4 { index - 1 + judge } else { index };
    let index = if index >= 3 { index + memory } else { index };
    format!("Step {index}/{}: {title}", 5 + judge + memory)
}

pub(super) fn describe(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

pub(super) fn text(value: Option<Value>) -> Option<String> {
    value.and_then(|v| v.as_str().map(str::to_string))
}

/// Run the wizard; `SetupQuit` ends it early with what was done so far.
pub fn run_setup_wizard<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> SetupResult {
    let mut lines = Vec::new();
    let mut model_ready = false;
    let mut completed = false;
    let steps = (|| -> Result<(), SetupQuit> {
        environment_step(ui, deps);
        let (model_line, ready) = provider::provider_step(ui, deps)?;
        lines.push(model_line);
        model_ready = ready;
        if deps.probe_memorantado.is_some() {
            lines.push(memory::memory_backend_step(ui, deps)?);
        }
        lines.push(memory::hindsight_step(ui, deps)?);
        if deps.probe_judge.is_some() {
            lines.push(extras::judge_step(ui, deps)?);
        }
        lines.push(extras::loki_step(ui, deps)?);
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

fn environment_step<U: SetupUi>(ui: &mut U, deps: &SetupDeps) {
    ui.heading(&step_title(deps, 1, "Environment"));
    for check in setup_core::check_environment(&deps.probe, &deps.python) {
        let (mark, tone) = match (check.ok, check.optional) {
            (true, _) => ("✓", Tone::Success),
            (false, true) => ("–", Tone::Dim),
            (false, false) => ("✗", Tone::Error),
        };
        ui.note(&format!("{mark} {}: {}", check.label, check.detail), tone);
        if let (false, Some(fix)) = (check.ok, &check.fix) {
            ui.note(&format!("  {fix}"), Tone::Warning);
        }
    }
}

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
    finish_model_test(message, elapsed(started))
}

fn finish_model_test(message: Result<ai::AssistantMessage, String>, ms: u64) -> ModelTestResult {
    let message = match message {
        Ok(message) => message,
        Err(error) => {
            return ModelTestResult {
                ok: false,
                reply: None,
                error: Some(error),
                ms,
            };
        }
    };
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
    let rust_only = (env.var)(interactive::qol::PARITY_ENV).as_deref().map(str::trim) != Some("1");
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
        hindsight_wait: Duration::from_mins(5),
        hindsight_interval: Duration::from_secs(3),
        loki_version: Some(loki::analyzers::bundled_version()),
        probe_judge: rust_only.then(|| {
            Arc::new(|env: &Env, backend: &str| {
                let seat = crate::judge_cli::seat(env, false, None)?;
                judge::bench::probe(&seat, backend, &kernel::Signal::new())
            }) as ProbeJudge
        }),
        probe_memorantado: rust_only.then(|| Arc::new(memorantado::resolve::probe) as ProbeMemorantado),
    }
}
