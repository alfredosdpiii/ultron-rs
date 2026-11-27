//! The Session worker's side of Pi's `/settings` and `/trust` (experimental/services/worker-settings.ts): read the
//! worker's settings (profile and project, the source of truth) as `WorkerSettingsRead`, persist one change through
//! the settings manager and report how it applies, plus the environment the worker shows (`modelEnvOverrides`,
//! session-control-provider.ts `debugEnvironment`).
//!
//! The changes the running session takes now come back as [`Change`]s; the worker applies them as session
//! operations. Evidence: `fixtures/generate-worker-settings.mjs` records the TypeScript over a recording harness;
//! `tests/worker_settings.rs` replays it.

use config::keys::{
    CACHE_WARMING_MODES, DISABLED_REQUEST_TIMEOUT_MS, DOUBLE_ESCAPE_ACTIONS, FULLSCREEN_EXIT_OUTPUTS,
    FULLSCREEN_SCROLLBARS, MERMAID_MODES, PROJECT_TRUST_DEFAULTS, QUEUE_MODES, TRANSPORTS, TREE_FILTER_MODES,
    TUI_MODES,
};
use config::settings::SettingsManager;
use model_scope::{DEFAULT_THINKING_LEVEL, THINKING_LEVELS};
use serde_json::{Map, Value, json};
use std::path::Path;

/// The worker's environment, by variable name.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What the running session reports for the settings it holds live (the harness's compaction, retry and queues).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    pub compaction: bool,
    pub retry: bool,
    pub steering_mode: String,
    pub follow_up_mode: String,
}

/// How a change applied (`WorkerSettingResult.applied`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    /// Saved and applied to the running session.
    Live,
    /// Saved; applies when the session worker restarts.
    Restart,
    /// Saved only (a presentation setting the client applies, or one the worker does not use).
    Saved,
}

impl Applied {
    pub fn name(self) -> &'static str {
        match self {
            Applied::Live => "live",
            Applied::Restart => "restart",
            Applied::Saved => "saved",
        }
    }
}

/// A change the running session takes now (what TypeScript's harness and lane calls do).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Compaction(bool),
    Retry(bool),
    SteeringMode(String),
    FollowUpMode(String),
    Transport(String),
    /// The process's HTTP idle timeout (`configureHttpDispatcher`), in ms (0: disabled).
    HttpIdleTimeout(u64),
    /// The model requests' timeout (`streamOptions.timeoutMs`).
    StreamTimeout(u64),
    /// The session's thinking level (an override for the current model).
    Thinking(String),
}

impl Change {
    /// The fixture's text for the change (`<what> <value>`).
    pub fn describe(&self) -> String {
        match self {
            Change::Compaction(on) => format!("compaction {on}"),
            Change::Retry(on) => format!("retry {on}"),
            Change::SteeringMode(mode) => format!("steeringMode {mode}"),
            Change::FollowUpMode(mode) => format!("followUpMode {mode}"),
            Change::Transport(transport) => format!("transport {transport}"),
            Change::HttpIdleTimeout(ms) => format!("httpIdleTimeout {ms}"),
            Change::StreamTimeout(ms) => format!("streamTimeout {ms}"),
            Change::Thinking(level) => format!("thinking {level}"),
        }
    }
}

fn get<'a>(settings: &'a SettingsManager, path: &[&str]) -> Option<&'a Value> {
    let mut value = settings.settings().get(path[0])?;
    for key in &path[1..] {
        value = value.as_object()?.get(*key)?;
    }
    (!value.is_null()).then_some(value)
}

fn global<'a>(settings: &'a SettingsManager, field: &str) -> Option<&'a Value> {
    settings.global_settings().get(field)
}

fn bool_or(settings: &SettingsManager, path: &[&str], default: bool) -> Value {
    get(settings, path).cloned().unwrap_or(Value::Bool(default))
}

fn floor_int(n: f64) -> Value {
    json!(n.floor() as i64)
}

fn text(value: Option<&Value>) -> Option<String> {
    let t = value?.as_str()?.trim();
    (!t.is_empty()).then(|| t.to_string())
}

fn one_of_or(value: Option<&Value>, allowed: &[&str], default: &str) -> Value {
    match value.and_then(Value::as_str) {
        Some(v) if allowed.contains(&v) => v.into(),
        _ => default.into(),
    }
}

/// `autoreviewValues`: reviews on, the acknowledgement comment, its art.
fn autoreview_values(settings: &SettingsManager, values: &mut Map<String, Value>) {
    let saved = review::settings::saved(settings.global_settings());
    let art = match saved.get("ackArt") {
        None => "logo",
        Some(Value::String(s)) if s == "logo" => "logo",
        Some(Value::Bool(false)) => "none",
        Some(Value::String(s)) if s == "none" => "none",
        Some(_) => "custom",
    };
    values.insert(
        "autoreview.enabled".into(),
        json!(saved.get("enabled") != Some(&json!(false))),
    );
    values.insert("autoreview.ack".into(), json!(saved.get("ack") != Some(&json!(false))));
    values.insert("autoreview.ackArt".into(), json!(art));
}

/// The effective values `/settings` shows, by setting key (`readWorkerSettings().values`), in TypeScript's order.
pub fn values(settings: &SettingsManager, live: &Live, env: Env) -> Result<Map<String, Value>, String> {
    let mut v = Map::new();
    let mut put = |key: &str, value: Value| {
        v.insert(key.into(), value);
    };
    put("compaction.enabled", json!(live.compaction));
    put("retry.enabled", json!(live.retry));
    put("steeringMode", json!(live.steering_mode));
    put("followUpMode", json!(live.follow_up_mode));
    put(
        "transport",
        get(settings, &["transport"]).cloned().unwrap_or("auto".into()),
    );
    put(
        "httpIdleTimeoutMs",
        json!(config::keys::http_idle_timeout_ms(settings.settings())?),
    );
    put(
        "cacheWarming",
        one_of_or(global(settings, "cacheWarming"), &CACHE_WARMING_MODES, "streaming"),
    );
    put(
        "modelThinkingLevels",
        get(settings, &["modelThinkingLevels"]).cloned().unwrap_or(json!({})),
    );
    put(
        "defaultThinkingLevel",
        get(settings, &["defaultThinkingLevel"])
            .cloned()
            .unwrap_or(DEFAULT_THINKING_LEVEL.into()),
    );
    put(
        "defaultProvider",
        get(settings, &["defaultProvider"]).cloned().unwrap_or(Value::Null),
    );
    put(
        "defaultModel",
        get(settings, &["defaultModel"]).cloned().unwrap_or(Value::Null),
    );
    put(
        "theme",
        get(settings, &["theme"])
            .filter(|t| t.is_string())
            .cloned()
            .unwrap_or(Value::Null),
    );
    put("hideThinkingBlock", bool_or(settings, &["hideThinkingBlock"], false));
    put(
        "terminal.showImages",
        bool_or(settings, &["terminal", "showImages"], true),
    );
    let width = get(settings, &["terminal", "imageWidthCells"])
        .and_then(Value::as_f64)
        .filter(|w| w.is_finite())
        .map_or(json!(60), |w| floor_int(w.floor().max(1.0)));
    put("terminal.imageWidthCells", width);
    put("images.autoResize", bool_or(settings, &["images", "autoResize"], true));
    put(
        "images.blockImages",
        bool_or(settings, &["images", "blockImages"], false),
    );
    put("enableSkillCommands", bool_or(settings, &["enableSkillCommands"], true));
    put(
        "markdown.mermaid",
        one_of_or(get(settings, &["markdown", "mermaid"]), &MERMAID_MODES, "streaming"),
    );
    put(
        "showCacheMissNotices",
        bool_or(settings, &["showCacheMissNotices"], false),
    );
    put("collapseChangelog", bool_or(settings, &["collapseChangelog"], false));
    put(
        "enableInstallTelemetry",
        bool_or(settings, &["enableInstallTelemetry"], true),
    );
    put("quietStartup", bool_or(settings, &["quietStartup"], false));
    put("rlmPaneAutoOpen", bool_or(settings, &["rlmPaneAutoOpen"], true));
    autoreview_values(settings, &mut v);
    let mut put = |key: &str, value: Value| {
        v.insert(key.into(), value);
    };
    put("defaultProjectTrust", json!(settings.default_project_trust()));
    put(
        "doubleEscapeAction",
        get(settings, &["doubleEscapeAction"]).cloned().unwrap_or("tree".into()),
    );
    put(
        "treeFilterMode",
        one_of_or(get(settings, &["treeFilterMode"]), &TREE_FILTER_MODES, "default"),
    );
    put(
        "showHardwareCursor",
        get(settings, &["showHardwareCursor"])
            .cloned()
            .unwrap_or_else(|| json!(env(config::keys::HARDWARE_CURSOR_ENV).as_deref() == Some("1"))),
    );
    put(
        "editorPaddingX",
        get(settings, &["editorPaddingX"]).cloned().unwrap_or(json!(0)),
    );
    put(
        "outputPad",
        json!(i32::from(get(settings, &["outputPad"]) != Some(&json!(0)))),
    );
    put(
        "autocompleteMaxVisible",
        get(settings, &["autocompleteMaxVisible"]).cloned().unwrap_or(json!(5)),
    );
    put(
        "terminal.clearOnShrink",
        get(settings, &["terminal", "clearOnShrink"])
            .cloned()
            .unwrap_or_else(|| json!(env(config::keys::CLEAR_ON_SHRINK_ENV).as_deref() == Some("1"))),
    );
    put(
        "terminal.showTerminalProgress",
        bool_or(settings, &["terminal", "showTerminalProgress"], false),
    );
    put("tuiMode", one_of_or(get(settings, &["tuiMode"]), &TUI_MODES, "regular"));
    put(
        "fullscreenExitOutput",
        one_of_or(
            get(settings, &["fullscreenExitOutput"]),
            &FULLSCREEN_EXIT_OUTPUTS,
            "transcript",
        ),
    );
    put(
        "fullscreenScrollbar",
        one_of_or(get(settings, &["fullscreenScrollbar"]), &FULLSCREEN_SCROLLBARS, "auto"),
    );
    put(
        "fullscreenCopyOnSelect",
        bool_or(settings, &["fullscreenCopyOnSelect"], true),
    );
    put("warnings", get(settings, &["warnings"]).cloned().unwrap_or(json!({})));
    put(
        "enabledModels",
        get(settings, &["enabledModels"]).cloned().unwrap_or(Value::Null),
    );
    let rlm = config::keys::rlm_model_settings(settings.global_settings());
    put(
        "observer.model",
        get(settings, &["observer", "model"]).cloned().unwrap_or(Value::Null),
    );
    put("rlm.frameModel", json!(rlm.frame_model));
    put("rlm.childModel", json!(rlm.child_model));
    put("rlm.frameThinking", json!(rlm.frame_thinking));
    put(
        "review.model",
        json!(config::keys::review_model(settings.global_settings())),
    );
    let claude = global(settings, "claudeCode").and_then(Value::as_object);
    for key in ["frameModel", "childModel", "ultronChildModel", "model"] {
        put(
            &format!("claudeCode.{key}"),
            json!(text(claude.and_then(|c| c.get(key)))),
        );
    }
    if config::settings::is_durable_process() {
        put("durable.model", json!(settings.durable_model()));
    }
    Ok(v)
}

/// `modelEnvOverrides` as `{setting: {name, value}}`.
pub fn model_env_overrides(env: Env) -> Map<String, Value> {
    config::env::model_env_overrides(env)
        .into_iter()
        .map(|(key, name, value)| (key.to_string(), json!({"name": name, "value": value})))
        .collect()
}

/// `readWorkerSettings`: `{values, cwd, agentDir, projectTrusted, savedTrust, envOverrides}`.
pub fn read(settings: &SettingsManager, live: &Live, cwd: &Path, agent_dir: &Path, env: Env) -> Result<Value, String> {
    let values = values(settings, live, env)?;
    let saved = config::trust::ProjectTrustStore::new(agent_dir).get_entry(cwd)?;
    Ok(json!({
        "values": values,
        "cwd": cwd.display().to_string(),
        "agentDir": agent_dir.display().to_string(),
        "projectTrusted": settings.is_project_trusted(),
        "savedTrust": saved.map(|(path, decision)| json!({"path": path.display().to_string(), "decision": decision})),
        "envOverrides": model_env_overrides(env),
    }))
}

fn bool_value(key: &str, value: &Value) -> Result<bool, String> {
    value.as_bool().ok_or_else(|| format!("{key} must be true or false"))
}

fn int(key: &str, value: &Value, min: f64) -> Result<f64, String> {
    match value.as_f64() {
        Some(n) if n.is_finite() && n >= min && value.is_number() => Ok(n.floor()),
        _ => Err(format!("{key} must be a number of at least {min}")),
    }
}

fn string(key: &str, value: Option<&Value>) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        _ => Err(format!("{key} must be a non-empty string")),
    }
}

fn thinking(key: &str, value: Option<&Value>) -> Result<String, String> {
    let level = string(key, value)?;
    if !THINKING_LEVELS.contains(&level.as_str()) {
        return Err(format!("{key}: invalid thinking level {level}"));
    }
    Ok(level)
}

fn one_of(key: &str, value: &Value, allowed: &[&str]) -> Result<String, String> {
    match value.as_str() {
        Some(v) if allowed.contains(&v) => Ok(v.to_string()),
        _ => Err(format!("{key} must be one of: {}", allowed.join(", "))),
    }
}

fn record<'a>(key: &str, value: &'a Value) -> Result<&'a Map<String, Value>, String> {
    value.as_object().ok_or_else(|| format!("{key} must be an object"))
}

/// A `provider/model` value, or `None` for null (clear the setting).
fn model_value(key: &str, value: &Value) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    match value.as_str().map(str::trim) {
        Some(v) if model_scope::is_model_ref(v) => Ok(Some(v.to_string())),
        _ => Err(format!("{key} must be provider/model (or null to clear it)")),
    }
}

/// A Claude Code model alias (`claude --model`), from `claude-code/<alias>` or the bare alias; null clears it.
fn claude_alias_value(key: &str, value: &Value) -> Result<Option<String>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let alias = string(key, Some(value))?;
    let alias = alias.trim();
    let alias = alias.strip_prefix("claude-code/").unwrap_or(alias);
    let mut chars = alias.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '[' | ']' | '-'));
    if !valid {
        return Err(format!("{key} must be a Claude Code model such as opus or sonnet"));
    }
    Ok(Some(alias.to_string()))
}

fn opt(value: Option<String>) -> Option<Value> {
    value.map(Value::String)
}

fn apply_autoreview_patch(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    let patch = record(key, value)?;
    let mut next: Vec<(&str, Option<Value>)> = Vec::new();
    for (name, item) in patch {
        match name.as_str() {
            "enabled" => {
                let parsed = match item {
                    Value::Null => None,
                    item => Some(json!(bool_value(&format!("{key}.{name}"), item)?)),
                };
                next.push(("enabled", parsed));
            }
            "accounts" => {
                let valid = item.is_null() || item.as_array().is_some_and(|a| a.iter().all(Value::is_string));
                if !valid {
                    return Err(format!("{key}.{name} must be an array of strings or null"));
                }
                next.push(("accounts", (!item.is_null()).then(|| item.clone())));
            }
            _ => return Err(format!("{key}.{name} cannot be set this way")),
        }
    }
    settings.patch_global_object(review::settings::KEY, &next);
    Ok(Applied::Live)
}

fn apply_autoreview(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    match key {
        "autoreview.enabled" | "autoreview.ack" => {
            let flag = bool_value(key, value)?;
            settings.patch_global_object(
                review::settings::KEY,
                &[(&key["autoreview.".len()..], Some(json!(flag)))],
            );
            Ok(Applied::Live)
        }
        "autoreview.ackArt" => {
            let art = one_of(key, value, &["logo", "none"])?;
            let art = if art == "none" { json!(false) } else { json!("logo") };
            settings.patch_global_object(review::settings::KEY, &[("ackArt", Some(art))]);
            Ok(Applied::Live)
        }
        "autoreview" => apply_autoreview_patch(settings, key, value),
        _ => Err(format!("Unknown setting: {key}")),
    }
}

fn parse_judge_provider(key: &str, value: &Value) -> Result<Option<Value>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => {
            let mut order: Vec<Value> = Vec::new();
            for item in items {
                let name = one_of(key, item, &config::keys::JUDGE_BACKENDS)?;
                if !order.contains(&json!(name)) {
                    order.push(json!(name));
                }
            }
            Ok(Some(Value::Array(order)))
        }
        _ => Err(format!("{key} must be a list of jev, d1 (or null for both, Jev first)")),
    }
}

fn parse_judge_d1_url(key: &str, value: &Value) -> Result<Option<Value>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let url = string(key, Some(value))?;
    let url = url.trim().trim_end_matches('/');
    let host = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://"));
    if host.is_none_or(|host| host.is_empty() || host.contains(char::is_whitespace)) {
        return Err(format!("{key} must be an http(s) URL"));
    }
    config::auth::secure_url(url).map_err(|why| format!("{key}: {why}"))?;
    Ok(Some(json!(url)))
}

fn set_nested_subkey(settings: &mut SettingsManager, section: &str, sub: &str, field: &str, value: Option<Value>) {
    let mut map = get(settings, &[section, sub])
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    match value {
        Some(value) => map.insert(field.into(), value),
        None => map.shift_remove(field),
    };
    settings.set_global_nested(section, sub, (!map.is_empty()).then_some(Value::Object(map)));
}

fn validate_judge_thresholds(key: &str, value: &Value) -> Result<Option<Value>, String> {
    let unit = |v: &Value| v.as_f64().is_some_and(|n| (0.0..=1.0).contains(&n));
    let valid = value.is_null()
        || value.as_object().is_some_and(|map| {
            map.values()
                .all(|v| unit(v) || v.as_object().is_some_and(|kind| kind.values().all(unit)))
        });
    if !valid {
        return Err(format!(
            "{key} must map backends (and kinds to backends) to numbers from 0 to 1 (or null to clear it)"
        ));
    }
    Ok((!value.is_null()).then(|| value.clone()))
}

fn optional_f64(value: &Value, pred: impl FnOnce(f64) -> bool, err: String) -> Result<Option<Value>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let number = value.as_f64().filter(|n| n.is_finite() && pred(*n)).ok_or(err)?;
    Ok(Some(json!(number)))
}

fn optional_token_cap(key: &str, value: &Value) -> Result<Option<Value>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let cap = config::settings::token_cap(Some(value))
        .ok_or_else(|| format!("{key} must be a positive whole number (or null to clear it)"))?;
    Ok(Some(json!(cap)))
}

fn apply_judge_backend(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<bool, String> {
    match key {
        "judge.provider" => settings.set_global_nested("judge", "provider", parse_judge_provider(key, value)?),
        "judge.d1.url" => set_nested_subkey(settings, "judge", "d1", "url", parse_judge_d1_url(key, value)?),
        "judge.model" => {
            let model = match value {
                Value::Null => None,
                value => Some(json!(string(key, Some(value))?.trim())),
            };
            settings.set_global_nested("judge", "model", model);
        }
        "judge.escalateModel" | "judge.fallbackModel" => {
            settings.set_global_nested("judge", &key["judge.".len()..], opt(model_value(key, value)?));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_judge_cost(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<bool, String> {
    match key {
        "judge.budgetUsd" => {
            let err = format!("{key} must be a positive number of US dollars (or null to clear it)");
            settings.set_global_nested("judge", "budgetUsd", optional_f64(value, |n| n > 0.0, err)?);
        }
        "judge.jev.pricePerMTok" => {
            let err = format!("{key} must be a number of US dollars per million tokens (or null to clear it)");
            let price = optional_f64(value, |n| n >= 0.0, err)?;
            set_nested_subkey(settings, "judge", "jev", "pricePerMTok", price);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_judge(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    if apply_judge_backend(settings, key, value)? || apply_judge_cost(settings, key, value)? {
        return Ok(Applied::Live);
    }
    match key {
        "judge.thresholds" => {
            settings.set_global_nested("judge", "thresholds", validate_judge_thresholds(key, value)?);
        }
        "judge.maxCalls" => settings.set_global_nested("judge", "maxCalls", optional_token_cap(key, value)?),
        _ => return Err(format!("Unknown setting: {key}")),
    }
    Ok(Applied::Live)
}

fn parse_race_models(key: &str, value: &Value) -> Result<Option<Value>, String> {
    match value {
        Value::Null => Ok(None),
        Value::Array(items) => {
            let list = items
                .iter()
                .map(|item| match model_value(key, item)? {
                    Some(model) => Ok(Value::String(model)),
                    None => Err(format!("{key} must be a list of provider/model (or null to clear it)")),
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Some(Value::Array(list)))
        }
        _ => Err(format!("{key} must be a list of provider/model (or null to clear it)")),
    }
}

fn apply_rlm_model_setting(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
) -> Result<Option<Applied>, String> {
    match key {
        "rlm.frameModel" | "rlm.childModel" => {
            settings.set_global_nested("rlm", &key["rlm.".len()..], opt(model_value(key, value)?));
            Ok(Some(Applied::Live))
        }
        "rlm.frameThinking" => {
            let level = (!value.is_null())
                .then(|| one_of(key, value, &THINKING_LEVELS))
                .transpose()?;
            settings.set_global_nested("rlm", "frameThinking", opt(level));
            Ok(Some(Applied::Live))
        }
        "rlm.raceModels" => {
            settings.set_global_nested("rlm", "raceModels", parse_race_models(key, value)?);
            Ok(Some(Applied::Live))
        }
        _ => Ok(None),
    }
}

fn apply_rlm_setting(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    if let Some(applied) = apply_rlm_model_setting(settings, key, value)? {
        return Ok(applied);
    }
    match key {
        "rlm.resultFooter" => {
            let on = (!value.is_null()).then(|| bool_value(key, value)).transpose()?;
            settings.set_global_nested("rlm", "resultFooter", on.map(|b| json!(b)));
            Ok(Applied::Restart)
        }
        "rlm.mapTokens" => {
            settings.set_global_nested("rlm", "mapTokens", optional_token_cap(key, value)?);
            Ok(Applied::Live)
        }
        _ => Err(format!("Unknown setting: {key}")),
    }
}

fn apply_claude_code_setting(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    match key {
        "claudeCode.frameModel" | "claudeCode.ultronChildModel" => {
            settings.set_global_nested("claudeCode", &key["claudeCode.".len()..], opt(model_value(key, value)?));
            Ok(Applied::Saved)
        }
        "claudeCode.childModel" | "claudeCode.model" => {
            settings.set_global_nested(
                "claudeCode",
                &key["claudeCode.".len()..],
                opt(claude_alias_value(key, value)?),
            );
            Ok(Applied::Saved)
        }
        _ => Err(format!("Unknown setting: {key}")),
    }
}

fn apply_runtime_model_setting(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
) -> Result<Option<Applied>, String> {
    match key {
        "observer.model" => {
            settings.set_global_nested("observer", "model", opt(model_value(key, value)?));
            Ok(Some(Applied::Live))
        }
        "review.model" => {
            settings.set_global_nested("review", "model", opt(model_value(key, value)?));
            Ok(Some(Applied::Live))
        }
        "durable.model" => {
            settings.set_global_nested("durable", "model", opt(model_value(key, value)?));
            Ok(Some(Applied::Restart))
        }
        _ => Ok(None),
    }
}

fn apply_rlm_or_runtime(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    if key.starts_with("rlm.") {
        return apply_rlm_setting(settings, key, value);
    }
    if key.starts_with("claudeCode.") {
        return apply_claude_code_setting(settings, key, value);
    }
    if let Some(applied) = apply_runtime_model_setting(settings, key, value)? {
        return Ok(applied);
    }
    match key {
        "rootBudget.maxTotalTokens" => {
            settings.set_global_nested("rootBudget", "maxTotalTokens", optional_token_cap(key, value)?);
            Ok(Applied::Restart)
        }
        "memory.backend" => {
            let backend = (!value.is_null())
                .then(|| one_of(key, value, &config::keys::MEMORY_BACKENDS))
                .transpose()?;
            settings.set_global_nested("memory", "backend", opt(backend));
            Ok(Applied::Restart)
        }
        _ => Err(format!("Unknown setting: {key}")),
    }
}

const UI_GLOBAL_BOOLS: &[&str] = &[
    "hideThinkingBlock",
    "enableSkillCommands",
    "showCacheMissNotices",
    "collapseChangelog",
    "enableInstallTelemetry",
    "quietStartup",
    "rlmPaneAutoOpen",
    "showHardwareCursor",
    "fullscreenCopyOnSelect",
];

const UI_NESTED_BOOLS: &[&str] = &[
    "terminal.showImages",
    "images.autoResize",
    "images.blockImages",
    "terminal.clearOnShrink",
    "terminal.showTerminalProgress",
];

fn apply_ui_choice(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<bool, String> {
    if key == "markdown.mermaid" {
        let mode = one_of(key, value, &MERMAID_MODES)?;
        settings.set_global_nested("markdown", "mermaid", Some(json!(mode)));
        return Ok(true);
    }
    let choices: &[(&str, &[&str])] = &[
        ("doubleEscapeAction", &DOUBLE_ESCAPE_ACTIONS),
        ("treeFilterMode", &TREE_FILTER_MODES),
        ("tuiMode", &TUI_MODES),
        ("fullscreenExitOutput", &FULLSCREEN_EXIT_OUTPUTS),
        ("fullscreenScrollbar", &FULLSCREEN_SCROLLBARS),
    ];
    let Some((_, allowed)) = choices.iter().find(|(name, _)| *name == key) else {
        return Ok(false);
    };
    settings.set_global(key, Some(json!(one_of(key, value, allowed)?)));
    Ok(true)
}

fn apply_ui_numeric(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<bool, String> {
    match key {
        "terminal.imageWidthCells" => {
            let width = int(key, value, 1.0)?.max(1.0);
            settings.set_global_nested("terminal", "imageWidthCells", Some(floor_int(width)));
        }
        "editorPaddingX" => {
            let padding = int(key, value, 0.0)?.clamp(0.0, 3.0);
            settings.set_global(key, Some(floor_int(padding)));
        }
        "outputPad" => {
            let pad = i32::from(int(key, value, 0.0)? != 0.0);
            settings.set_global(key, Some(json!(pad)));
        }
        "autocompleteMaxVisible" => {
            let visible = int(key, value, 1.0)?.clamp(3.0, 20.0);
            settings.set_global(key, Some(floor_int(visible)));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_ui_special(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<bool, String> {
    if apply_ui_numeric(settings, key, value)? {
        return Ok(true);
    }
    match key {
        "theme" => settings.set_global(key, Some(json!(string(key, Some(value))?))),
        "warnings" => {
            let warnings = record(key, value)?;
            let next = match warnings.get("anthropicExtraUsage") {
                None => json!({}),
                Some(flag) => json!({"anthropicExtraUsage": bool_value(key, flag)?}),
            };
            settings.set_global(key, Some(next));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_ui(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Option<Applied>, String> {
    if UI_GLOBAL_BOOLS.contains(&key) {
        settings.set_global(key, Some(json!(bool_value(key, value)?)));
        return Ok(Some(Applied::Saved));
    }
    if UI_NESTED_BOOLS.contains(&key) {
        let (field, k) = key.split_once('.').expect("dotted key");
        settings.set_global_nested(field, k, Some(json!(bool_value(key, value)?)));
        return Ok(Some(Applied::Saved));
    }
    if apply_ui_choice(settings, key, value)? || apply_ui_special(settings, key, value)? {
        return Ok(Some(Applied::Saved));
    }
    Ok(None)
}

fn apply_model_thinking_level(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    current_model: Option<(&str, &str)>,
    changes: &mut Vec<Change>,
) -> Result<Applied, String> {
    let target = record(key, value)?;
    let provider = string(key, target.get("provider"))?;
    let model_id = string(key, target.get("modelId"))?;
    let level = match target.get("level") {
        Some(Value::Null) => None,
        level => Some(thinking(key, level)?),
    };
    match &level {
        None => settings.remove_model_thinking_level(&provider, &model_id),
        Some(level) => settings.set_model_thinking_level(&provider, &model_id, level),
    }
    if current_model == Some((provider.as_str(), model_id.as_str())) {
        let level = level.unwrap_or_else(|| {
            get(settings, &["defaultThinkingLevel"])
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_THINKING_LEVEL)
                .to_string()
        });
        changes.push(Change::Thinking(level));
        return Ok(Applied::Live);
    }
    Ok(Applied::Saved)
}

fn apply_http_idle_timeout(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    changes: &mut Vec<Change>,
) -> Result<(), String> {
    let timeout = int(key, value, 0.0)? as u64;
    settings.set_global(key, Some(json!(timeout)));
    changes.push(Change::HttpIdleTimeout(timeout));
    if get(settings, &["retry", "provider", "timeoutMs"]).is_none() {
        let stream = if timeout == 0 {
            DISABLED_REQUEST_TIMEOUT_MS
        } else {
            timeout
        };
        changes.push(Change::StreamTimeout(stream));
    }
    Ok(())
}

fn apply_queue_mode(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    changes: &mut Vec<Change>,
) -> Result<(), String> {
    let mode = one_of(key, value, &QUEUE_MODES)?;
    settings.set_global(key, Some(json!(mode)));
    changes.push(if key == "steeringMode" {
        Change::SteeringMode(mode)
    } else {
        Change::FollowUpMode(mode)
    });
    Ok(())
}

fn apply_nested_bool_live(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    changes: &mut Vec<Change>,
) -> Result<bool, String> {
    match key {
        "compaction.enabled" => {
            let enabled = bool_value(key, value)?;
            settings.set_global_nested("compaction", "enabled", Some(json!(enabled)));
            changes.push(Change::Compaction(enabled));
        }
        "retry.enabled" => {
            let enabled = bool_value(key, value)?;
            settings.set_global_nested("retry", "enabled", Some(json!(enabled)));
            changes.push(Change::Retry(enabled));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_session_live(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    changes: &mut Vec<Change>,
) -> Result<Option<Applied>, String> {
    if apply_nested_bool_live(settings, key, value, changes)? {
        return Ok(Some(Applied::Live));
    }
    match key {
        "steeringMode" | "followUpMode" => apply_queue_mode(settings, key, value, changes)?,
        "transport" => {
            let transport = one_of(key, value, &TRANSPORTS)?;
            settings.set_global(key, Some(json!(transport)));
            changes.push(Change::Transport(transport));
        }
        "httpIdleTimeoutMs" => apply_http_idle_timeout(settings, key, value, changes)?,
        _ => return Ok(None),
    }
    Ok(Some(Applied::Live))
}

fn apply_enabled_models(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    let patterns = match value {
        Value::Null => None,
        Value::Array(items) => Some(
            items
                .iter()
                .map(|p| string(key, Some(p)).map(Value::String))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        _ => return Err(format!("{key} must be a list")),
    };
    settings.set_global(key, patterns.map(Value::Array));
    Ok(Applied::Saved)
}

fn apply_persona(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Applied, String> {
    let persona = (!value.is_null())
        .then(|| one_of(key, value, &config::keys::PERSONA_VALUES))
        .transpose()?;
    let defaulted = config::settings::process_defaults().is_some_and(|d| d.contains_key(key));
    settings.set_global(
        key,
        persona
            .filter(|p| p != config::keys::PERSONA_OFF || defaulted)
            .map(Value::String),
    );
    Ok(Applied::Live)
}

fn apply_session_simple(settings: &mut SettingsManager, key: &str, value: &Value) -> Result<Option<Applied>, String> {
    match key {
        "cacheWarming" => {
            settings.set_global(key, Some(json!(one_of(key, value, &CACHE_WARMING_MODES)?)));
            Ok(Some(Applied::Saved))
        }
        "defaultThinkingLevel" => {
            settings.set_global(key, Some(json!(thinking(key, Some(value))?)));
            Ok(Some(Applied::Restart))
        }
        "engineering" => {
            settings.set_global(key, Some(json!(bool_value(key, value)?)));
            Ok(Some(Applied::Live))
        }
        "defaultProjectTrust" => {
            settings.set_global(key, Some(json!(one_of(key, value, &PROJECT_TRUST_DEFAULTS)?)));
            Ok(Some(Applied::Restart))
        }
        _ => Ok(None),
    }
}

fn apply_session(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    current_model: Option<(&str, &str)>,
    changes: &mut Vec<Change>,
) -> Result<Applied, String> {
    if let Some(applied) = apply_session_live(settings, key, value, changes)? {
        return Ok(applied);
    }
    if let Some(applied) = apply_session_simple(settings, key, value)? {
        return Ok(applied);
    }
    match key {
        "modelThinkingLevel" => apply_model_thinking_level(settings, key, value, current_model, changes),
        config::keys::PERSONA_KEY => apply_persona(settings, key, value),
        "enabledModels" => apply_enabled_models(settings, key, value),
        _ => Err(format!("Unknown setting: {key}")),
    }
}

fn is_rlm_or_runtime_key(key: &str) -> bool {
    key.starts_with("rlm.")
        || key.starts_with("claudeCode.")
        || matches!(
            key,
            "observer.model" | "review.model" | "rootBudget.maxTotalTokens" | "memory.backend" | "durable.model"
        )
}

fn dispatch_apply(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    current_model: Option<(&str, &str)>,
    changes: &mut Vec<Change>,
) -> Result<Applied, String> {
    if key == "autoreview" || key.starts_with("autoreview.") {
        apply_autoreview(settings, key, value)
    } else if key.starts_with("judge.") {
        apply_judge(settings, key, value)
    } else if is_rlm_or_runtime_key(key) {
        apply_rlm_or_runtime(settings, key, value)
    } else if let Some(applied) = apply_ui(settings, key, value)? {
        Ok(applied)
    } else {
        apply_session(settings, key, value, current_model, changes)
    }
}

/// `applyWorkerSetting`: persist `key` through the settings manager and report how it applies, with the changes the
/// running session takes now. `current_model` is the session's model (`provider`, `id`).
pub fn apply(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    current_model: Option<(&str, &str)>,
) -> Result<(Applied, Vec<Change>), String> {
    let mut changes = Vec::new();
    settings.drain_errors();
    let applied = dispatch_apply(settings, key, value, current_model, &mut changes)?;
    let errors = settings.drain_errors();
    if !errors.is_empty() {
        let messages: Vec<String> = errors.into_iter().map(|e| e.message).collect();
        return Err(format!("Could not save {key}: {}", messages.join("; ")));
    }
    Ok((applied, changes))
}

/// `SECRET_NAME`, case-insensitively.
fn secret_name(name: &str) -> bool {
    let upper = name.to_uppercase();
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "COOKIE",
        "SESSION_KEY",
    ]
    .iter()
    .any(|part| upper.contains(part))
}

/// `debugEnvironment`: `ULTRON_*` and `PI_*` variables for `/debug`, sorted as `localeCompare` sorts them, with the
/// values of secret-looking names redacted.
pub fn debug_environment(vars: impl IntoIterator<Item = (String, String)>) -> Map<String, Value> {
    let mut picked: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(name, _)| name.starts_with("ULTRON_") || name.starts_with("PI_"))
        .collect();
    picked.sort_by(|(a, _), (b, _)| jsrt::collate::locale_compare(a, b));
    picked
        .into_iter()
        .map(|(name, value)| {
            let value = if secret_name(&name) {
                "<redacted>".to_string()
            } else {
                value
            };
            (name, Value::String(value))
        })
        .collect()
}
