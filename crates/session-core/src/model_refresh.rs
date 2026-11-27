//! `ultron update --models` (`refreshModelCatalogs` of package-manager-cli.ts): every provider that refreshes its
//! catalog and has a credential refreshes it from the network, as `ModelRuntime.refresh({ allowNetwork: true,
//! force: true })` does: built-in providers revalidate their pi.dev catalog, Radius providers (the built-in one and
//! models.json `"oauth": "radius"` ones) load `<gateway>/v1/config` with their key (an expired OAuth token is
//! refreshed first). Results land in models-store.json, which every later start restores.
//!
//! Evidence: `crates/ai/fixtures/generate-model-refresh.mjs`; `tests/model_refresh.rs` replays it.

use crate::RunEnv as Env;
use crate::{ModelFlags, lane};
use ai::model_refresh::{
    DEFAULT_CATALOG_BASE_URL, load_radius_gateway_config, radius_models_from_config, refresh_remote_catalog,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// The whole refresh's time budget (`refreshModelCatalogs`' 15 s abort).
const REFRESH_BUDGET: Duration = Duration::from_secs(15);

/// [`refresh_catalogs`]' error when its whole budget ran out (`result.aborted` after the timer in TS).
pub const REFRESH_TIMED_OUT: &str = "Model catalog refresh timed out.";

/// Refresh every provider's stored catalog. `Err` is the message `ultron update --models` prints after `Error: `.
pub fn refresh_model_catalogs(env: &Env) -> Result<(), String> {
    let errors = refresh_catalogs_from(env, DEFAULT_CATALOG_BASE_URL, true)?;
    if errors.is_empty() {
        return Ok(());
    }
    let details: Vec<String> = errors.iter().map(|(p, e)| format!("{p}: {e}")).collect();
    Err(format!("Could not refresh model catalogs: {}", details.join("; ")))
}

/// The model runtime's refresh: [`refresh_catalogs_from`] pi.dev, or nothing when offline (`PI_OFFLINE`).
pub fn refresh_catalogs(env: &Env) -> Result<Vec<(String, String)>, String> {
    if config::env::offline(&*env.var) {
        return Ok(Vec::new());
    }
    refresh_catalogs_from(env, DEFAULT_CATALOG_BASE_URL, false)
}

/// [`refresh_model_catalogs`]' per-provider failures, in provider order; `Err` when the whole refresh timed out.
/// Without `force`, a pi.dev catalog checked within the last four hours is not asked again (a login's refresh).
pub fn refresh_catalogs_from(env: &Env, catalog_base_url: &str, force: bool) -> Result<Vec<(String, String)>, String> {
    let user_agent = ai::model_refresh::management_user_agent();
    let registry = lane::registry(env);
    let store = config::models_store::ModelsStore::beside(&config::paths::AgentPaths::new(env.agent_dir()).models());
    let transport: Arc<dyn ai::Transport> = Arc::new(ai::ReqwestTransport::for_env(&*env.var, None));
    let providers = refreshable_providers(&registry);
    let work = async {
        let mut errors: Vec<(String, String)> = Vec::new();
        for provider in &providers {
            refresh_one_provider(
                env,
                &registry,
                &store,
                &*transport,
                provider,
                catalog_base_url,
                force,
                &user_agent,
                &mut errors,
            )
            .await;
        }
        errors
    };
    run_with_refresh_budget(work)
}

fn refreshable_providers(registry: &ai::model_registry::ModelRegistry) -> Vec<String> {
    let builtin: Vec<&str> = ai::models::builtin_providers().collect();
    registry
        .provider_ids()
        .into_iter()
        .filter(|p| builtin.contains(&p.as_str()) || registry.radius_gateway(p).is_some())
        .filter(|p| p != ai::claude_code::PROVIDER)
        .collect()
}

#[expect(clippy::too_many_arguments, reason = "internal provider refresh helper")]
async fn refresh_one_provider(
    env: &Env,
    registry: &ai::model_registry::ModelRegistry,
    store: &config::models_store::ModelsStore,
    transport: &dyn ai::Transport,
    provider: &str,
    catalog_base_url: &str,
    force: bool,
    user_agent: &str,
    errors: &mut Vec<(String, String)>,
) {
    let key = match lane::request_key(&ModelFlags::default(), env, provider) {
        Ok(Some(key)) => key,
        Ok(None) => return,
        Err(error) => {
            errors.push((provider.to_string(), error));
            return;
        }
    };
    if let Some((_, gateway)) = registry.radius_gateway(provider) {
        refresh_radius_provider(store, transport, provider, &gateway, &key, errors).await;
        return;
    }
    refresh_remote_provider(store, transport, provider, catalog_base_url, force, user_agent, errors).await;
}

async fn refresh_radius_provider(
    store: &config::models_store::ModelsStore,
    transport: &dyn ai::Transport,
    provider: &str,
    gateway: &str,
    key: &str,
    errors: &mut Vec<(String, String)>,
) {
    match load_radius_gateway_config(gateway, Some(key), transport).await {
        Ok(config) => {
            let models = radius_models_from_config(provider, &config).unwrap_or_default();
            if let Err(error) = store.write(provider, json!({"models": models, "checkedAt": jsrt::date::now_ms()})) {
                errors.push((provider.to_string(), error));
            }
        }
        Err(error) => errors.push((provider.to_string(), error)),
    }
}

async fn refresh_remote_provider(
    store: &config::models_store::ModelsStore,
    transport: &dyn ai::Transport,
    provider: &str,
    catalog_base_url: &str,
    force: bool,
    user_agent: &str,
    errors: &mut Vec<(String, String)>,
) {
    let now = || jsrt::date::now_ms();
    let stored = store.read(provider).ok().flatten();
    let outcome = refresh_remote_catalog(
        provider,
        stored.as_ref(),
        catalog_base_url,
        force,
        user_agent,
        &now,
        transport,
    )
    .await;
    if let Some(entry) = outcome.persist
        && let Err(error) = store.write(provider, entry)
    {
        errors.push((provider.to_string(), error));
    }
    if let Some(error) = outcome.error {
        errors.push((provider.to_string(), error));
    }
}

fn run_with_refresh_budget<F: std::future::Future<Output = Vec<(String, String)>> + Send>(
    work: F,
) -> Result<Vec<(String, String)>, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| e.to_string())?;
                runtime
                    .block_on(async { tokio::time::timeout(REFRESH_BUDGET, work).await })
                    .map_err(|_| REFRESH_TIMED_OUT.to_string())
            })
            .join()
            .unwrap_or_else(|_| Err("Model catalog refresh failed".into()))
    })
}
