//! Global and project settings (`settings-manager.ts`): load, migrate, deep-merge, typed reads with defaults, and
//! writes that merge only the keys this process changed into the file's current object.
//!
//! The file stays the TypeScript product's: `<agent>/settings.json` and `<cwd>/.pi/settings.json`, locked with
//! `proper-lockfile`'s `<file>.lock` directory, written as `JSON.stringify(settings, null, 2)`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::json::parse;
use crate::lock::{SYNC, acquire, lock_path_for};
use crate::paths::{CONFIG_DIR_NAME, expand_tilde, resolve};

pub type Settings = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn name(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
    fn index(self) -> usize {
        self as usize
    }
}

/// A load or write failure, kept for `drain_errors` (the TypeScript `SettingsError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError {
    pub scope: Scope,
    pub path: PathBuf,
    pub message: String,
}

/// Fields (and nested keys within them) changed by this process since the last successful write of a scope.
#[derive(Debug, Default, Clone)]
struct Modified {
    fields: Vec<String>,
    nested: Vec<(String, Vec<String>)>,
}

impl Modified {
    fn mark(&mut self, field: &str, nested_key: Option<&str>) {
        if !self.fields.iter().any(|f| f == field) {
            self.fields.push(field.to_string());
        }
        let Some(key) = nested_key else { return };
        let keys = match self.nested.iter().position(|(f, _)| f == field) {
            Some(i) => &mut self.nested[i].1,
            None => {
                self.nested.push((field.to_string(), Vec::new()));
                &mut self.nested.last_mut().expect("just pushed").1
            }
        };
        if !keys.iter().any(|k| k == key) {
            keys.push(key.to_string());
        }
    }
    fn nested_keys(&self, field: &str) -> Option<&[String]> {
        self.nested
            .iter()
            .find(|(f, _)| f == field)
            .map(|(_, keys)| keys.as_slice())
    }
}

/// Defaults under the files' settings for this whole process (`set_process_defaults`): a key either file sets
/// wins. Never written to a file.
static PROCESS_DEFAULTS: std::sync::OnceLock<Settings> = std::sync::OnceLock::new();

/// Give every settings manager of this process `defaults` under the global and project files (a session worker of
/// the durable instance: `persona: "ultron"`). Set once; a later call is ignored.
pub fn set_process_defaults(defaults: Settings) {
    let _ = PROCESS_DEFAULTS.set(defaults);
}

pub fn process_defaults() -> Option<&'static Settings> {
    PROCESS_DEFAULTS.get()
}

/// The durable instance's own default model (`durable.model`) when the user's settings name none: Claude Opus on
/// the Claude subscription (a `claude-code` lane with tools runs as `claude-subscription`, never on API credits).
pub const DURABLE_MODEL_DEFAULT: &str = crate::providers::CLAUDE_CODE_DEFAULT_REF;
/// Set (to `1`) in the (injected) environment of a client of the durable instance's own directory (`ultron durable
/// ...`, `ultron-s`; not one whose `ULTRON_SERVER_DIR` names another): it resolves a new session's model as the
/// instance's workers do.
pub const DURABLE_CLIENT_ENV: &str = "__ULTRON_DURABLE_SESSIONS";

/// Whether this process is a session worker of the durable instance (`set_durable_process`). A process flag, not an
/// environment variable: a command the session runs (an `ultron` in its shell) is not the durable instance's.
static DURABLE_PROCESS: std::sync::OnceLock<()> = std::sync::OnceLock::new();

/// Mark this process as a session worker of the durable instance: its new sessions start on `durable.model` and a
/// saved default model goes there. Set once.
pub fn set_durable_process() {
    let _ = DURABLE_PROCESS.set(());
}

pub fn is_durable_process() -> bool {
    DURABLE_PROCESS.get().is_some()
}

/// `deepMergeObjects`: overrides win; two objects at the same key merge recursively.
pub fn deep_merge(base: &Settings, overrides: &Settings) -> Settings {
    let mut result = base.clone();
    for (key, value) in overrides {
        let merged = match (base.get(key), value) {
            (Some(Value::Object(b)), Value::Object(o)) => Value::Object(deep_merge(b, o)),
            _ => value.clone(),
        };
        result.insert(key.clone(), merged);
    }
    result
}

/// `migrateSettings`: queueMode, the websockets boolean, the old skills object, and retry.maxDelayMs.
pub fn migrate(mut settings: Settings) -> Settings {
    if settings.contains_key("queueMode") && !settings.contains_key("steeringMode") {
        let value = settings.shift_remove("queueMode").unwrap_or(Value::Null);
        settings.insert("steeringMode".into(), value);
    }
    if !settings.contains_key("transport")
        && let Some(Value::Bool(websockets)) = settings.get("websockets")
    {
        let transport = if *websockets { "websocket" } else { "sse" };
        settings.insert("transport".into(), transport.into());
        settings.shift_remove("websockets");
    }
    if let Some(Value::Object(skills)) = settings.get("skills").cloned() {
        if let Some(enable) = skills.get("enableSkillCommands")
            && !settings.contains_key("enableSkillCommands")
        {
            settings.insert("enableSkillCommands".into(), enable.clone());
        }
        match skills.get("customDirectories") {
            Some(Value::Array(dirs)) if !dirs.is_empty() => {
                settings.insert("skills".into(), Value::Array(dirs.clone()));
            }
            _ => {
                settings.shift_remove("skills");
            }
        }
    }
    if let Some(Value::Object(retry)) = settings.get_mut("retry") {
        let provider: Option<Map<String, Value>> = match retry.get("provider") {
            Some(Value::Object(p)) => Some(p.clone()),
            // `{ ...array }` spreads the indices.
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, v)| (i.to_string(), v.clone()))
                    .collect(),
            ),
            _ => None,
        };
        let has_cap = provider
            .as_ref()
            .and_then(|p| p.get("maxRetryDelayMs"))
            .is_some_and(|v| !v.is_null());
        if let Some(max) = retry.get("maxDelayMs").filter(|v| v.is_number()).cloned()
            && !has_cap
        {
            let mut provider = provider.unwrap_or_default();
            provider.insert("maxRetryDelayMs".into(), max);
            retry.insert("provider".into(), Value::Object(provider));
        }
        retry.shift_remove("maxDelayMs");
    }
    settings
}

fn read_locked(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let _lock = acquire(&lock_path_for(path), SYNC).map_err(|e| e.to_string())?;
    fs::read_to_string(path).map(Some).map_err(|e| e.to_string())
}

fn parse_settings(content: &str) -> Result<Settings, String> {
    match parse(content).map_err(|e| e.to_string())? {
        Value::Object(map) => Ok(migrate(map)),
        other => Err(format!("settings must be a JSON object, found {other}")),
    }
}

fn load(path: &Path) -> Result<Settings, String> {
    match read_locked(path)? {
        Some(content) if !content.is_empty() => parse_settings(&content),
        _ => Ok(Settings::new()),
    }
}

/// `MODEL_REF_PATTERN`: a trimmed `provider/model[/more]` reference.
pub fn model_ref(value: Option<&Value>) -> Option<String> {
    let trimmed = jsrt::string::trim(value?.as_str()?);
    model_scope::is_model_ref(trimmed).then(|| trimmed.to_string())
}

/// A token cap setting (`rlm.mapTokens`): a positive safe integer; anything else (unset, `"off"`) is no cap.
pub fn token_cap(value: Option<&Value>) -> Option<u64> {
    value?
        .as_f64()
        .filter(|n| jsrt::number::is_safe_integer(*n) && *n >= 1.0)
        .map(|n| n as u64)
}

/// A local service's URL (Hindsight, memorantado): the environment, else the global setting, else `default`; blank
/// values are unset, and "off", "none", "0" or "false" (any case) turn the service off (`None`).
pub fn service_url(env: Option<&str>, setting: Option<&str>, default: &str) -> Option<String> {
    fn given(value: Option<&str>) -> Option<&str> {
        value.map(jsrt::string::trim).filter(|v| !v.is_empty())
    }
    match given(env).or_else(|| given(setting)) {
        None => Some(default.into()),
        Some(value)
            if ["off", "none", "0", "false"]
                .iter()
                .any(|off| value.eq_ignore_ascii_case(off)) =>
        {
            None
        }
        Some(value) => Some(value.into()),
    }
}

pub struct SettingsManager {
    paths: [PathBuf; 2],
    global: Settings,
    project: Settings,
    settings: Settings,
    project_trusted: bool,
    modified: [Modified; 2],
    load_failed: [bool; 2],
    errors: Vec<SettingsError>,
}

impl SettingsManager {
    /// `SettingsManager.create(cwd, agentDir, { projectTrusted })`.
    pub fn create(cwd: &Path, agent_dir: &Path, project_trusted: bool) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let base = std::env::current_dir().unwrap_or_default();
        let cwd = resolve(&cwd.to_string_lossy(), &base, &home);
        let agent_dir = resolve(&agent_dir.to_string_lossy(), &base, &home);
        let paths = [
            crate::paths::AgentPaths::new(agent_dir).settings(),
            cwd.join(CONFIG_DIR_NAME).join("settings.json"),
        ];
        let mut manager = SettingsManager {
            paths,
            global: Settings::new(),
            project: Settings::new(),
            settings: Settings::new(),
            project_trusted,
            modified: Default::default(),
            load_failed: [false; 2],
            errors: Vec::new(),
        };
        match load(&manager.paths[0]) {
            Ok(settings) => manager.global = settings,
            Err(e) => {
                manager.load_failed[0] = true;
                manager.record(Scope::Global, e);
            }
        }
        if project_trusted {
            match load(&manager.paths[1]) {
                Ok(settings) => manager.project = settings,
                Err(e) => {
                    manager.load_failed[1] = true;
                    manager.record(Scope::Project, e);
                }
            }
        }
        manager.remerge();
        manager
    }

    fn record(&mut self, scope: Scope, message: String) {
        self.errors.push(SettingsError {
            scope,
            path: self.paths[scope.index()].clone(),
            message,
        });
    }

    fn remerge(&mut self) {
        self.settings = deep_merge(&self.global, &self.project);
        if let Some(defaults) = PROCESS_DEFAULTS.get() {
            self.settings = deep_merge(defaults, &self.settings);
        }
    }

    /// The session worker's settings: the project's are loaded when [`crate::trust::worker_project_trusted`] says so.
    pub fn for_worker(cwd: &Path, agent_dir: &Path, home: &Path) -> SettingsManager {
        let trusted = crate::trust::worker_project_trusted(cwd, agent_dir, home);
        SettingsManager::create(cwd, agent_dir, trusted)
    }

    pub fn path(&self, scope: Scope) -> &Path {
        &self.paths[scope.index()]
    }
    /// Effective settings: global, then project, then overrides.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }
    pub fn global_settings(&self) -> &Settings {
        &self.global
    }
    pub fn project_settings(&self) -> &Settings {
        &self.project
    }
    pub fn is_project_trusted(&self) -> bool {
        self.project_trusted
    }
    pub fn drain_errors(&mut self) -> Vec<SettingsError> {
        std::mem::take(&mut self.errors)
    }

    pub fn set_project_trusted(&mut self, trusted: bool) {
        if self.project_trusted == trusted {
            return;
        }
        self.project_trusted = trusted;
        self.modified[1] = Modified::default();
        self.load_failed[1] = false;
        self.project = Settings::new();
        if trusted {
            match load(&self.paths[1]) {
                Ok(settings) => self.project = settings,
                Err(e) => {
                    self.load_failed[1] = true;
                    self.record(Scope::Project, e);
                }
            }
        }
        self.remerge();
    }

    /// Re-read both files; a file that fails to parse keeps the previous settings and reports an error.
    pub fn reload(&mut self) {
        match load(&self.paths[0]) {
            Ok(settings) => {
                self.global = settings;
                self.load_failed[0] = false;
            }
            Err(e) => {
                self.load_failed[0] = true;
                self.record(Scope::Global, e);
            }
        }
        self.modified = Default::default();
        let project = if self.project_trusted {
            load(&self.paths[1])
        } else {
            Ok(Settings::new())
        };
        match project {
            Ok(settings) => {
                self.project = settings;
                self.load_failed[1] = false;
            }
            Err(e) => {
                self.load_failed[1] = true;
                self.record(Scope::Project, e);
            }
        }
        self.remerge();
    }

    /// Merge this process's modified keys of `scope` into the file's current object and write it.
    fn persist(&self, scope: Scope) -> Result<(), String> {
        let path = &self.paths[scope.index()];
        let snapshot = if scope == Scope::Global {
            &self.global
        } else {
            &self.project
        };
        let modified = &self.modified[scope.index()];
        let exists = path.exists();
        let mut lock = if exists {
            Some(acquire(&lock_path_for(path), SYNC).map_err(|e| e.to_string())?)
        } else {
            None
        };
        let current = match exists {
            true => match fs::read_to_string(path).map_err(|e| e.to_string())? {
                content if content.is_empty() => Settings::new(),
                content => parse_settings(&content)?,
            },
            false => Settings::new(),
        };
        let mut merged = current.clone();
        for field in &modified.fields {
            let value = snapshot.get(field);
            match (modified.nested_keys(field), value) {
                (Some(keys), Some(Value::Object(in_memory))) => {
                    let mut nested = match current.get(field) {
                        Some(Value::Object(base)) => base.clone(),
                        _ => Map::new(),
                    };
                    for key in keys {
                        match in_memory.get(key) {
                            Some(v) => nested.insert(key.clone(), v.clone()),
                            None => nested.shift_remove(key),
                        };
                    }
                    merged.insert(field.clone(), Value::Object(nested));
                }
                (_, Some(v)) => {
                    merged.insert(field.clone(), v.clone());
                }
                (_, None) => {
                    merged.shift_remove(field);
                }
            }
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        if lock.is_none() {
            lock = Some(acquire(&lock_path_for(path), SYNC).map_err(|e| e.to_string())?);
        }
        let written = fs::write(path, jsrt::json::stringify_pretty(&Value::Object(merged))).map_err(|e| e.to_string());
        drop(lock);
        written
    }

    fn save(&mut self, scope: Scope) {
        self.remerge();
        if self.load_failed[scope.index()] {
            return;
        }
        match self.persist(scope) {
            Ok(()) => self.modified[scope.index()] = Modified::default(),
            Err(e) => self.record(scope, e),
        }
    }

    /// Set (or with `None` remove) one top-level global setting and write it.
    pub fn set_global(&mut self, field: &str, value: Option<Value>) {
        match value {
            Some(v) => self.global.insert(field.to_string(), v),
            None => self.global.shift_remove(field),
        };
        self.modified[0].mark(field, None);
        self.save(Scope::Global);
    }

    /// Set (or remove) `field.key` in global settings, merging only that key on write (`setCompactionEnabled`).
    pub fn set_global_nested(&mut self, field: &str, key: &str, value: Option<Value>) {
        self.patch_global_object(field, &[(key, value)]);
    }

    /// Merge each patched key into the file's current `field` object (`null` removes it) and write it
    /// (`setAutoreviewSettings`).
    pub fn patch_global_object(&mut self, field: &str, patch: &[(&str, Option<Value>)]) {
        let mut current = match self.global.get(field) {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        for (key, value) in patch {
            match value {
                Some(v) => current.insert(key.to_string(), v.clone()),
                None => current.shift_remove(*key),
            };
            self.modified[0].mark(field, Some(key));
        }
        self.global.insert(field.to_string(), Value::Object(current));
        self.save(Scope::Global);
    }

    pub fn set_model_thinking_level(&mut self, provider: &str, model_id: &str, level: &str) {
        let mut levels = match self.global.get("modelThinkingLevels") {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        };
        levels.insert(format!("{provider}/{model_id}"), level.into());
        self.set_global("modelThinkingLevels", Some(Value::Object(levels)));
    }

    pub fn remove_model_thinking_level(&mut self, provider: &str, model_id: &str) {
        let Some(Value::Object(levels)) = self.global.get("modelThinkingLevels") else {
            return;
        };
        let mut levels = levels.clone();
        levels.shift_remove(&format!("{provider}/{model_id}"));
        let value = (!levels.is_empty()).then_some(Value::Object(levels));
        self.set_global("modelThinkingLevels", value);
    }

    /// Set (or remove) one project setting; refused when the project is not trusted.
    pub fn set_project(&mut self, field: &str, value: Option<Value>) -> Result<(), String> {
        if !self.project_trusted {
            return Err("Project is not trusted; refusing to write project settings".into());
        }
        match value {
            Some(v) => self.project.insert(field.to_string(), v),
            None => self.project.shift_remove(field),
        };
        self.modified[1].mark(field, None);
        self.save(Scope::Project);
        Ok(())
    }

    fn get(&self, field: &str) -> Option<&Value> {
        self.settings.get(field).filter(|v| !v.is_null())
    }

    fn nested(&self, field: &str, key: &str) -> Option<&Value> {
        self.get(field)?.as_object()?.get(key).filter(|v| !v.is_null())
    }

    fn truthy(value: Option<&Value>) -> Option<&Value> {
        value.filter(|v| jsrt::value::truthy(Some(v)))
    }

    pub fn steering_mode(&self) -> Value {
        Self::truthy(self.get("steeringMode"))
            .cloned()
            .unwrap_or(crate::keys::QUEUE_ONE_AT_A_TIME.into())
    }
    pub fn follow_up_mode(&self) -> Value {
        Self::truthy(self.get("followUpMode"))
            .cloned()
            .unwrap_or(crate::keys::QUEUE_ONE_AT_A_TIME.into())
    }
    pub fn theme_setting(&self) -> Option<String> {
        self.get("theme")?.as_str().map(str::to_string)
    }
    /// A fixed theme name; slash-separated automatic theme settings are not one.
    pub fn theme(&self) -> Option<String> {
        self.theme_setting().filter(|t| !t.contains('/'))
    }
    pub fn transport(&self) -> Value {
        self.get("transport").cloned().unwrap_or("auto".into())
    }
    pub fn default_provider(&self) -> Option<Value> {
        self.settings.get("defaultProvider").cloned()
    }
    pub fn default_model(&self) -> Option<Value> {
        self.settings.get("defaultModel").cloned()
    }
    pub fn compaction_enabled(&self) -> Value {
        self.nested("compaction", "enabled")
            .cloned()
            .unwrap_or(Value::Bool(true))
    }
    pub fn retry_settings(&self) -> Value {
        let or = |key: &str, default: Value| self.nested("retry", key).cloned().unwrap_or(default);
        serde_json::json!({
            "enabled": or("enabled", true.into()),
            "maxRetries": or("maxRetries", 3.into()),
            "baseDelayMs": or("baseDelayMs", 2000.into()),
            "maxAgentDelayMs": or("maxAgentDelayMs", 60_000.into()),
        })
    }
    pub fn branch_summary_settings(&self) -> Value {
        let or = |key: &str, default: Value| self.nested("branchSummary", key).cloned().unwrap_or(default);
        serde_json::json!({ "reserveTokens": or("reserveTokens", 16384.into()), "skipPrompt": or("skipPrompt", false.into()) })
    }
    /// `sessionDir` with `~` expanded.
    pub fn session_dir(&self, home: &Path) -> Option<PathBuf> {
        let dir = Self::truthy(self.settings.get("sessionDir"))?.as_str()?;
        Some(expand_tilde(dir, home))
    }
    /// `getLastChangelogVersion` (merged: a project value is seen too).
    pub fn last_changelog_version(&self) -> Option<String> {
        self.settings
            .get("lastChangelogVersion")
            .and_then(Value::as_str)
            .map(str::to_string)
    }
    /// `setLastChangelogVersion`: written to the global settings.
    pub fn set_last_changelog_version(&mut self, version: &str) {
        self.set_global("lastChangelogVersion", Some(version.into()));
    }
    /// Global only: a repository cannot pick its own trust default.
    pub fn default_project_trust(&self) -> &'static str {
        match self.global.get("defaultProjectTrust").and_then(Value::as_str) {
            Some("always") => "always",
            Some("never") => "never",
            _ => "ask",
        }
    }

    /// Global only `claude.apiFallback` (default `true`): a Claude subscription call that is out of usage continues
    /// on Anthropic API credits. Only an explicit `false` turns it off; a repository cannot spend the user's credits.
    pub fn claude_api_fallback(&self) -> bool {
        !matches!(
            self.global.get("claude").and_then(|c| c.get("apiFallback")),
            Some(Value::Bool(false))
        )
    }

    /// Global only `durable.model`: the model a new session of the durable instance starts on when no `--model` or
    /// `--provider` names one, ahead of `defaultProvider`/`defaultModel` (else [`DURABLE_MODEL_DEFAULT`]). Global
    /// only, as `claude.apiFallback`: a repository cannot move the user's durable sessions off their subscription.
    pub fn durable_model(&self) -> String {
        model_ref(self.global.get("durable").and_then(|d| d.get("model")))
            .unwrap_or_else(|| DURABLE_MODEL_DEFAULT.to_string())
    }

    /// Save `provider/id` as `durable.model` (a model chosen in a durable session), leaving the other `durable` keys.
    pub fn set_durable_model(&mut self, provider: &str, id: &str) {
        self.set_global_nested("durable", "model", Some(Value::String(format!("{provider}/{id}"))));
    }
}
