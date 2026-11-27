//! Step 2 of `ultron setup`: provider authentication, custom endpoints, and default model selection.

use super::{
    Choice, InputOptions, LoginOutcome, ProfileRuntime, SetupDeps, SetupQuit, SetupUi, SummaryLine, Tone, confirm,
    describe, line, pick, step_title, text,
};
use crate::Env;
use crate::setup_core::{self, CustomEndpoint};
use ai::Model;
use config::settings::SettingsManager;
use interactive::selectors::AuthProvider;
use serde_json::Value;
use std::path::Path;

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

fn note_provider_status<U: SetupUi>(ui: &mut U, runtime: &ProfileRuntime, current: Option<&Model>) {
    if let Some(current) = current {
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
}

fn provider_path_choices(current: Option<&Model>) -> Vec<(Choice, ProviderPath)> {
    let mut choices = Vec::new();
    if let Some(current) = current {
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
    choices
}

fn keep_current_model<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    current: &Model,
) -> Result<(SummaryLine, bool), SetupQuit> {
    let tested = test_and_report(ui, deps, current)?;
    let detail = format!(
        "{}{}",
        describe(current),
        if tested { "" } else { " (live test failed)" }
    );
    Ok((line(PROVIDER_STEP, "unchanged", detail), tested))
}

fn run_provider_path<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    path: ProviderPath,
) -> Result<Option<(SummaryLine, bool)>, SetupQuit> {
    let provider = match path {
        ProviderPath::Custom => custom_endpoint(ui, deps)?,
        ProviderPath::OAuth => login_provider(ui, deps, true)?,
        _ => login_provider(ui, deps, false)?,
    };
    let Some(provider) = provider else {
        return Ok(None);
    };
    let runtime = load_runtime(ui, deps, "Reloading providers…")?;
    choose_default_model(ui, deps, &runtime, &provider)
}

pub(super) fn provider_step<U: SetupUi>(ui: &mut U, deps: &mut SetupDeps) -> Result<(SummaryLine, bool), SetupQuit> {
    ui.heading(&step_title(deps, 2, PROVIDER_STEP));
    let runtime = load_runtime(ui, deps, "Loading providers…")?;
    let current = current_default_model(&runtime, &deps.settings);
    note_provider_status(ui, &runtime, current.as_ref());
    let choices = provider_path_choices(current.as_ref());
    loop {
        match pick(ui, "How should Ultron reach a model?", &choices, None)? {
            None | Some(ProviderPath::Skip) => {
                let detail = current
                    .as_ref()
                    .map_or_else(|| "no model".to_string(), |c| format!("kept {}", describe(c)));
                return Ok((line(PROVIDER_STEP, "skipped", detail), false));
            }
            Some(ProviderPath::Keep) => {
                if let Some(current) = &current {
                    return keep_current_model(ui, deps, current);
                }
            }
            Some(path) => {
                if let Some(outcome) = run_provider_path(ui, deps, path)? {
                    return Ok(outcome);
                }
            }
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

fn confirm_existing_provider<U: SetupUi>(ui: &mut U, option: &AuthProvider) -> Result<bool, SetupQuit> {
    let Some((kind, source)) = &option.status else {
        return Ok(false);
    };
    confirm(
        ui,
        &format!(
            "{} is already configured ({}).",
            option.name,
            source.as_deref().unwrap_or(kind)
        ),
        "Use it as it is",
        "Sign in again and replace it",
    )
}

fn report_login_outcome<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    option: &AuthProvider,
    outcome: LoginOutcome,
) -> bool {
    match outcome {
        LoginOutcome::Ok => {
            let auth = config::paths::AgentPaths::new(deps.agent_dir()).auth();
            let msg = if option.auth_type == "oauth" {
                format!("✓ Signed in to {}. Saved to {}.", option.name, auth.display())
            } else {
                format!("✓ Saved the API key for {} to {}.", option.name, auth.display())
            };
            ui.note(&msg, Tone::Success);
            true
        }
        LoginOutcome::Cancelled => {
            ui.note(
                &format!("Sign-in to {} cancelled; nothing was saved.", option.name),
                Tone::Dim,
            );
            false
        }
        LoginOutcome::Error(error) => {
            ui.note(&format!("✗ {}: {error}", option.name), Tone::Error);
            false
        }
    }
}

fn external_api_key_name(option: &AuthProvider) -> Option<&str> {
    if option.auth_type != "api_key" || option.api_key_method.as_ref().is_some_and(|(_, login)| *login) {
        return None;
    }
    Some(
        option
            .api_key_method
            .as_ref()
            .map_or(option.name.as_str(), |(name, _)| name.as_str()),
    )
}

/// Pi's `/login` for one provider; its id once credentials are saved.
fn login_provider<U: SetupUi>(ui: &mut U, deps: &SetupDeps, oauth: bool) -> Result<Option<String>, SetupQuit> {
    let options = login_provider_options(&deps.env, oauth);
    if options.is_empty() {
        let msg = if oauth {
            "No subscription providers are available."
        } else {
            "No API-key providers are available."
        };
        ui.note(msg, Tone::Warning);
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
        if confirm_existing_provider(ui, &option)? {
            return Ok(Some(option.id));
        }
        if let Some(name) = external_api_key_name(&option) {
            ui.note(
                &format!("{name} is configured outside Ultron (environment variables or cloud credentials)."),
                Tone::Warning,
            );
            continue;
        }
        let outcome = ui.login(&deps.env, &option)?;
        if report_login_outcome(ui, deps, &option, outcome) {
            return Ok(Some(option.id));
        }
    }
}

fn ask_provider_id<U: SetupUi>(ui: &mut U, models_path: &Path) -> Result<Option<String>, SetupQuit> {
    let mut provider_id: Option<String> = None;
    loop {
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
        if !setup_core::custom_provider_exists(models_path, &id) {
            return Ok(Some(id));
        }
        if confirm(
            ui,
            &format!("models.json already has a provider named \"{id}\"."),
            "Replace it",
            "Choose another name",
        )? {
            return Ok(Some(id));
        }
    }
}

fn note_listed_models<U: SetupUi>(ui: &mut U, listed: Option<&Vec<String>>) {
    match listed.filter(|l| !l.is_empty()) {
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
}

fn ask_endpoint_model_ids<U: SetupUi>(
    ui: &mut U,
    listed: Option<&Vec<String>>,
) -> Result<Option<Vec<String>>, SetupQuit> {
    let initial = match listed.map(Vec::as_slice) {
        Some([only]) => only.clone(),
        _ => String::new(),
    };
    loop {
        let Some(answer) = ui.input(
            "Model ids to use (comma-separated)",
            InputOptions {
                initial: Some(initial.clone()),
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
        let unknown: Vec<String> =
            listed.map_or_default(|l| ids.iter().filter(|id| !l.contains(id)).cloned().collect());
        if unknown.is_empty()
            || confirm(
                ui,
                &format!("The endpoint does not list {}.", unknown.join(", ")),
                "Use them anyway",
                "Edit the list",
            )?
        {
            return Ok(Some(ids));
        }
    }
}

fn save_custom_endpoint<U: SetupUi>(ui: &mut U, models_path: &Path, endpoint: &CustomEndpoint) -> bool {
    let result = (|| -> Result<bool, String> {
        let existing = setup_core::read_text_file(models_path).map_err(|e| e.to_string())?;
        let (merged, replaced) = setup_core::merge_custom_endpoint(existing.as_deref(), endpoint)?;
        setup_core::write_private_file(models_path, &merged).map_err(|e| e.to_string())?;
        Ok(replaced)
    })();
    match result {
        Ok(replaced) => {
            ui.note(
                &format!(
                    "✓ {} provider \"{}\" in {}.",
                    if replaced { "Replaced" } else { "Added" },
                    endpoint.provider_id,
                    models_path.display()
                ),
                Tone::Success,
            );
            true
        }
        Err(error) => {
            ui.note(
                &format!("✗ {error} ({} was not changed)", models_path.display()),
                Tone::Error,
            );
            false
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
    let Some(provider_id) = ask_provider_id(ui, &models_path)? else {
        return Ok(None);
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
    let (fetch, url, key) = (deps.fetch.clone(), base_url.clone(), api_key.clone());
    let listed = ui.busy(&format!("Asking {base_url} for its models…"), move || {
        setup_core::list_endpoint_models(&url, Some(&key), &fetch)
    })?;
    note_listed_models(ui, listed.as_ref());
    let Some(model_ids) = ask_endpoint_model_ids(ui, listed.as_ref())? else {
        return Ok(None);
    };
    let endpoint = CustomEndpoint {
        provider_id: provider_id.clone(),
        base_url,
        api_key: Some(api_key).filter(|k| !jsrt::string::trim(k).is_empty()),
        api: api.to_string(),
        model_ids,
    };
    Ok(save_custom_endpoint(ui, &models_path, &endpoint).then_some(provider_id))
}

fn confirm_after_failed_test<U: SetupUi>(
    ui: &mut U,
    deps: &SetupDeps,
    model: &Model,
) -> Result<Option<bool>, SetupQuit> {
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
        None | Some("other") => Ok(None),
        Some("retry") => Ok(test_and_report(ui, deps, model)?.then_some(true)),
        Some(_) => Ok(Some(false)),
    }
}

fn save_default_model<U: SetupUi>(
    ui: &mut U,
    deps: &mut SetupDeps,
    model: &Model,
    thinking: Option<&str>,
    tested: bool,
) -> (SummaryLine, bool) {
    deps.settings
        .set_global("defaultProvider", Some(Value::String(model.provider.clone())));
    deps.settings
        .set_global("defaultModel", Some(Value::String(model.id.clone())));
    if let Some(thinking) = thinking {
        deps.settings
            .set_global("defaultThinkingLevel", Some(Value::String(thinking.to_string())));
    }
    ui.note(
        &format!(
            "✓ Default model saved: {}{}.",
            describe(model),
            thinking.map_or_default(|t| format!(", thinking {t}"))
        ),
        Tone::Success,
    );
    let detail = format!(
        "{}{}",
        describe(model),
        if tested {
            ", live test passed"
        } else {
            ", live test failed"
        }
    );
    (line(PROVIDER_STEP, "configured", detail), tested)
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
    let title = format!("Default model ({} from {name})", models.len());
    loop {
        let Some(model) = pick(ui, &title, &choices, None)? else {
            return Ok(None);
        };
        let thinking = choose_thinking(ui, deps, &model)?;
        let tested = if test_and_report(ui, deps, &model)? {
            true
        } else if let Some(after) = confirm_after_failed_test(ui, deps, &model)? {
            after
        } else {
            continue;
        };
        return Ok(Some(save_default_model(ui, deps, &model, thinking.as_deref(), tested)));
    }
}

fn choose_thinking<U: SetupUi>(ui: &mut U, deps: &SetupDeps, model: &Model) -> Result<Option<String>, SetupQuit> {
    let levels = ai::models::supported_thinking_levels(model);
    if levels.len() <= 1 {
        return Ok(None);
    }
    let current = config::keys::default_thinking_level(deps.settings.settings())
        .unwrap_or_else(|| model_scope::DEFAULT_THINKING_LEVEL.to_string());
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
        let reply = result.reply.as_deref().map_or_default(|r| {
            let collapsed = r
                .split(jsrt::string::is_space)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            let joined = collapsed.join(" ");
            jsrt::utf16::head(jsrt::string::trim(&joined), 60).to_string()
        });
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
