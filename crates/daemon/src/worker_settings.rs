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
        json!(if get(settings, &["outputPad"]) == Some(&json!(0)) {
            0
        } else {
            1
        }),
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
    // `modelSettingValues`: global only.
    let rlm = config::keys::rlm_model_settings(settings.global_settings());
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
    // Only the durable instance's sessions have one (and start on it).
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

/// `applyWorkerSetting`: persist `key` through the settings manager and report how it applies, with the changes the
/// running session takes now. `current_model` is the session's model (`provider`, `id`).
pub fn apply(
    settings: &mut SettingsManager,
    key: &str,
    value: &Value,
    current_model: Option<(&str, &str)>,
) -> Result<(Applied, Vec<Change>), String> {
    let mut applied = Applied::Saved;
    let mut changes = Vec::new();
    // Errors recorded before this change (a malformed file at startup) are not this change's failure.
    settings.drain_errors();
    let set = |settings: &mut SettingsManager, field: &str, value: Value| settings.set_global(field, Some(value));
    let nested = |settings: &mut SettingsManager, field: &str, k: &str, v: Option<Value>| {
        settings.set_global_nested(field, k, v)
    };
    match key {
        "compaction.enabled" => {
            let enabled = bool_value(key, value)?;
            nested(settings, "compaction", "enabled", Some(json!(enabled)));
            changes.push(Change::Compaction(enabled));
            applied = Applied::Live;
        }
        "retry.enabled" => {
            let enabled = bool_value(key, value)?;
            nested(settings, "retry", "enabled", Some(json!(enabled)));
            changes.push(Change::Retry(enabled));
            applied = Applied::Live;
        }
        "steeringMode" | "followUpMode" => {
            let mode = one_of(key, value, &QUEUE_MODES)?;
            set(settings, key, json!(mode));
            changes.push(if key == "steeringMode" {
                Change::SteeringMode(mode)
            } else {
                Change::FollowUpMode(mode)
            });
            applied = Applied::Live;
        }
        "transport" => {
            let transport = one_of(key, value, &TRANSPORTS)?;
            set(settings, key, json!(transport));
            changes.push(Change::Transport(transport));
            applied = Applied::Live;
        }
        "httpIdleTimeoutMs" => {
            let timeout = int(key, value, 0.0)? as u64;
            set(settings, key, json!(timeout));
            changes.push(Change::HttpIdleTimeout(timeout));
            // Model requests carry the idle timeout unless `retry.provider.timeoutMs` pins one.
            if get(settings, &["retry", "provider", "timeoutMs"]).is_none() {
                changes.push(Change::StreamTimeout(if timeout == 0 {
                    DISABLED_REQUEST_TIMEOUT_MS
                } else {
                    timeout
                }));
            }
            applied = Applied::Live;
        }
        // Saved for Pi; the Ultron worker does not warm provider caches.
        "cacheWarming" => set(settings, key, json!(one_of(key, value, &CACHE_WARMING_MODES)?)),
        "modelThinkingLevel" => {
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
            // As in Pi, an override for the current model applies to the session now.
            if current_model == Some((provider.as_str(), model_id.as_str())) {
                let level = level.unwrap_or_else(|| {
                    get(settings, &["defaultThinkingLevel"])
                        .and_then(Value::as_str)
                        .unwrap_or(DEFAULT_THINKING_LEVEL)
                        .to_string()
                });
                changes.push(Change::Thinking(level));
                applied = Applied::Live;
            }
        }
        "defaultThinkingLevel" => {
            set(settings, key, json!(thinking(key, Some(value))?));
            applied = Applied::Restart;
        }
        "theme" => set(settings, key, json!(string(key, Some(value))?)),
        "hideThinkingBlock"
        | "enableSkillCommands"
        | "showCacheMissNotices"
        | "collapseChangelog"
        | "enableInstallTelemetry"
        | "quietStartup"
        | "rlmPaneAutoOpen"
        | "showHardwareCursor"
        | "fullscreenCopyOnSelect" => set(settings, key, json!(bool_value(key, value)?)),
        "terminal.showImages"
        | "images.autoResize"
        | "images.blockImages"
        | "terminal.clearOnShrink"
        | "terminal.showTerminalProgress" => {
            let (field, k) = key.split_once('.').expect("dotted key");
            nested(settings, field, k, Some(json!(bool_value(key, value)?)));
        }
        "terminal.imageWidthCells" => {
            let width = int(key, value, 1.0)?.max(1.0);
            nested(settings, "terminal", "imageWidthCells", Some(floor_int(width)));
        }
        "markdown.mermaid" => {
            let mode = one_of(key, value, &MERMAID_MODES)?;
            nested(settings, "markdown", "mermaid", Some(json!(mode)));
        }
        // The worker renders its system prompt again on the next request.
        "engineering" => {
            set(settings, key, json!(bool_value(key, value)?));
            applied = Applied::Live;
        }
        // The persona (resources::persona): `ultron`, or `off`/null to remove it; the worker renders its system
        // prompt again on the next request and reads it when a run ends.
        "persona" => {
            let persona = match value {
                Value::Null => None,
                value => Some(one_of(key, value, &[resources::persona::ULTRON, "off"])?),
            };
            // Off is kept as `"off"` where a process default would turn it on again (the durable instance).
            let defaulted = config::settings::process_defaults().is_some_and(|d| d.contains_key(key));
            settings.set_global(key, persona.filter(|p| p != "off" || defaulted).map(Value::String));
            applied = Applied::Live;
        }
        "autoreview.enabled" | "autoreview.ack" => {
            let flag = bool_value(key, value)?;
            settings.patch_global_object(
                review::settings::KEY,
                &[(&key["autoreview.".len()..], Some(json!(flag)))],
            );
            applied = Applied::Live;
        }
        "autoreview.ackArt" => {
            let art = one_of(key, value, &["logo", "none"])?;
            let art = if art == "none" { json!(false) } else { json!("logo") };
            settings.patch_global_object(review::settings::KEY, &[("ackArt", Some(art))]);
            applied = Applied::Live;
        }
        "autoreview" => {
            // A patch of `autoreview.*` (`/autoreview on|off`): `enabled` and `accounts`, where null removes the key.
            let patch = record(key, value)?;
            let mut next: Vec<(&str, Option<Value>)> = Vec::new();
            for (name, item) in patch {
                match name.as_str() {
                    "enabled" => next.push((
                        "enabled",
                        match item {
                            Value::Null => None,
                            item => Some(json!(bool_value(&format!("{key}.{name}"), item)?)),
                        },
                    )),
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
            applied = Applied::Live;
        }
        "defaultProjectTrust" => {
            set(settings, key, json!(one_of(key, value, &PROJECT_TRUST_DEFAULTS)?));
            applied = Applied::Restart;
        }
        "doubleEscapeAction" => set(settings, key, json!(one_of(key, value, &DOUBLE_ESCAPE_ACTIONS)?)),
        "treeFilterMode" => set(settings, key, json!(one_of(key, value, &TREE_FILTER_MODES)?)),
        "editorPaddingX" => {
            let padding = int(key, value, 0.0)?.clamp(0.0, 3.0);
            set(settings, key, floor_int(padding));
        }
        "outputPad" => {
            let pad = if int(key, value, 0.0)? == 0.0 { 0 } else { 1 };
            set(settings, key, json!(pad));
        }
        "autocompleteMaxVisible" => {
            let visible = int(key, value, 1.0)?.clamp(3.0, 20.0);
            set(settings, key, floor_int(visible));
        }
        "tuiMode" => set(settings, key, json!(one_of(key, value, &TUI_MODES)?)),
        "fullscreenExitOutput" => set(settings, key, json!(one_of(key, value, &FULLSCREEN_EXIT_OUTPUTS)?)),
        "fullscreenScrollbar" => set(settings, key, json!(one_of(key, value, &FULLSCREEN_SCROLLBARS)?)),
        "warnings" => {
            let warnings = record(key, value)?;
            let next = match warnings.get("anthropicExtraUsage") {
                None => json!({}),
                Some(flag) => json!({"anthropicExtraUsage": bool_value(key, flag)?}),
            };
            set(settings, key, next);
        }
        "rlm.frameModel" | "rlm.childModel" => {
            let field = &key["rlm.".len()..];
            nested(settings, "rlm", field, opt(model_value(key, value)?));
            // The runtime reads it for every frame and spawn.
            applied = Applied::Live;
        }
        "rlm.frameThinking" => {
            let level = match value {
                Value::Null => None,
                value => Some(one_of(key, value, &THINKING_LEVELS)?),
            };
            nested(settings, "rlm", "frameThinking", opt(level));
            applied = Applied::Live;
        }
        "review.model" => {
            nested(settings, "review", "model", opt(model_value(key, value)?));
            applied = Applied::Live;
        }
        // Rust-only: the subagent race's models, read for every race.
        "rlm.raceModels" => {
            let models = match value {
                Value::Null => None,
                Value::Array(items) => Some(
                    items
                        .iter()
                        .map(|item| match model_value(key, item)? {
                            Some(model) => Ok(Value::String(model)),
                            None => Err(format!("{key} must be a list of provider/model (or null to clear it)")),
                        })
                        .collect::<Result<Vec<_>, String>>()?,
                ),
                _ => return Err(format!("{key} must be a list of provider/model (or null to clear it)")),
            };
            nested(settings, "rlm", "raceModels", models.map(Value::Array));
            applied = Applied::Live;
        }
        // Rust-only token caps: `rlm.mapTokens` is read for every frame, `rootBudget` when the session starts.
        "rlm.mapTokens" | "rootBudget.maxTotalTokens" => {
            let cap = match value {
                Value::Null => None,
                value => Some(json!(config::settings::token_cap(Some(value)).ok_or_else(|| {
                    format!("{key} must be a positive whole number (or null to clear it)")
                })?)),
            };
            let (field, k) = key.split_once('.').expect("dotted key");
            nested(settings, field, k, cap);
            applied = if field == "rlm" {
                Applied::Live
            } else {
                Applied::Restart
            };
        }
        "claudeCode.frameModel" => nested(settings, "claudeCode", "frameModel", opt(model_value(key, value)?)),
        "claudeCode.ultronChildModel" => nested(
            settings,
            "claudeCode",
            "ultronChildModel",
            opt(model_value(key, value)?),
        ),
        "claudeCode.childModel" | "claudeCode.model" => nested(
            settings,
            "claudeCode",
            &key["claudeCode.".len()..],
            opt(claude_alias_value(key, value)?),
        ),
        "durable.model" => {
            let model = model_value(key, value)?;
            nested(settings, "durable", "model", opt(model));
            // A new durable session starts on it; the running one keeps its model.
            applied = Applied::Restart;
        }
        "enabledModels" => {
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
        }
        _ => return Err(format!("Unknown setting: {key}")),
    }
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
