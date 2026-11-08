//! `auth.json` (`auth-storage.ts`): a JSON object of provider id to credential, `api_key` or `oauth`, locked with
//! `proper-lockfile`'s `auth.json.lock` directory and written with mode 0600. No network refresh happens here.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value};

use crate::json::parse;
use crate::lock::{LockGuard, Options, Retry, SYNC, Until, acquire, lock_path_for};

pub type AuthData = Map<String, Value>;

/// The async TypeScript lock (`withLockAsync`): stale after 30 s, retried for up to 30 s.
const AUTH_LOCK: Options = Options {
    stale: Duration::from_secs(30),
    retry: Retry {
        until: Until::Tries(1500),
        ..SYNC.retry
    },
    update: None,
};

/// `auth.json` held under its lock (`FileAuthStorageBackend.withLockAsync`): the parent (0700) and an empty `{}`
/// file (0600) are created when missing; dropping it releases the lock.
pub struct LockedAuthFile {
    path: PathBuf,
    _lock: LockGuard,
}

impl LockedAuthFile {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(dir) = path.parent().filter(|d| !d.exists()) {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| e.to_string())?;
        }
        crate::paths::create_private_if_missing(path, "{}").map_err(|e| e.to_string())?;
        let lock = acquire(&lock_path_for(path), AUTH_LOCK).map_err(|e| e.to_string())?;
        Ok(LockedAuthFile {
            path: path.to_path_buf(),
            _lock: lock,
        })
    }

    pub fn read(&self) -> Result<String, String> {
        fs::read_to_string(&self.path).map_err(|e| e.to_string())
    }

    /// Replace the file's text (mode 0600, atomically).
    pub fn write(&self, text: &str) -> Result<(), String> {
        crate::paths::write_private(&self.path, text).map_err(|e| e.to_string())
    }
}

/// Run `f` on the file's text under the lock; `Some(next)` is written before the lock is released.
pub(crate) fn with_locked_file<T>(
    path: &Path,
    f: impl FnOnce(&str) -> Result<(T, Option<String>), String>,
) -> Result<T, String> {
    let file = LockedAuthFile::open(path)?;
    let (result, next) = f(&file.read()?)?;
    if let Some(next) = next {
        file.write(&next)?;
    }
    Ok(result)
}

pub(crate) fn parse_data(content: &str) -> Result<AuthData, String> {
    if content.is_empty() {
        return Ok(AuthData::new());
    }
    match parse(content).map_err(|e| e.to_string())? {
        Value::Object(map) => Ok(map),
        other => Err(format!("Invalid auth.json: expected an object, found {other}")),
    }
}

pub struct AuthStorage {
    path: PathBuf,
    data: AuthData,
}

impl AuthStorage {
    pub fn create(path: impl Into<PathBuf>) -> Self {
        let mut storage = AuthStorage {
            path: path.into(),
            data: AuthData::new(),
        };
        let _ = storage.reload();
        storage
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file; a malformed file leaves the previous data in place.
    pub fn reload(&mut self) -> Result<(), String> {
        let data = with_locked_file(&self.path, |current| Ok((parse_data(current)?, None)))?;
        self.data = data;
        Ok(())
    }

    /// The stored credential, with an `api_key` template (`$VAR`, `${VAR}`) resolved against its `env` and the
    /// process environment.
    pub fn read(&mut self, provider: &str) -> Result<Option<Value>, String> {
        self.reload()?;
        let Some(credential) = self.data.get(provider).cloned() else {
            return Ok(None);
        };
        resolve_stored_api_key(credential, &|name| std::env::var(name).ok())
    }

    /// Provider ids and credential types, in file order.
    pub fn list(&mut self) -> Result<Vec<(String, Value)>, String> {
        self.reload()?;
        Ok(self
            .data
            .iter()
            .map(|(id, c)| (id.clone(), c.get("type").cloned().unwrap_or(Value::Null)))
            .collect())
    }

    /// Replace one provider's credential with `f(current)` under the lock, keeping every other entry as the file
    /// has it now; `None` leaves the file untouched and returns the current credential.
    pub fn modify(
        &mut self,
        provider: &str,
        f: impl FnOnce(Option<&Value>) -> Option<Value>,
    ) -> Result<Option<Value>, String> {
        let (result, data) = with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            match f(data.get(provider)) {
                None => {
                    let current = data.get(provider).cloned();
                    Ok(((current, data), None))
                }
                Some(next) => {
                    data.insert(provider.to_string(), next.clone());
                    let text = jsrt::json::stringify_pretty(&Value::Object(data.clone()));
                    Ok(((Some(next), data), Some(text)))
                }
            }
        })?;
        self.data = data;
        Ok(result)
    }

    pub fn delete(&mut self, provider: &str) -> Result<(), String> {
        let data = with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            data.shift_remove(provider);
            let text = jsrt::json::stringify_pretty(&Value::Object(data.clone()));
            Ok((data, Some(text)))
        })?;
        self.data = data;
        Ok(())
    }
}

/// `AuthStorage.read`'s key resolution: an `api_key` credential's `key` template (`$VAR`, `${VAR}`) resolved
/// against its own `env`, then `process_env`, or its `!command` run (auth-storage.ts:446); a key naming an unset
/// variable, or whose command fails, is removed. Other credentials as given.
pub fn resolve_stored_api_key(
    mut credential: Value,
    process_env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<Value>, String> {
    if credential.get("type").and_then(Value::as_str) != Some("api_key") {
        return Ok(Some(credential));
    }
    let Some(key) = credential.get("key").and_then(Value::as_str).map(str::to_string) else {
        return Ok(Some(credential));
    };
    let env = credential
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let lookup = |name: &str| {
        env.get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .or_else(|| process_env(name))
    };
    let resolved = resolve_config_value(&key, &lookup)?;
    let object = credential.as_object_mut().expect("api_key credential is an object");
    match resolved {
        Some(value) => object.insert("key".into(), value.into()),
        None => object.shift_remove("key"),
    };
    Ok(Some(credential))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Literal(String),
    Env(String),
}

fn env_name_len(s: &str) -> usize {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return 0,
    }
    chars
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(s.len(), |(i, _)| i)
}

fn push_literal(parts: &mut Vec<TemplatePart>, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some(TemplatePart::Literal(previous)) = parts.last_mut() {
        previous.push_str(value);
    } else {
        parts.push(TemplatePart::Literal(value.to_string()));
    }
}

/// `parseConfigValueTemplate`: `$NAME`, `${NAME}`, `$$` and `$!` escapes; anything else stays literal.
fn parse_template(config: &str) -> Vec<TemplatePart> {
    let mut parts = Vec::new();
    let mut rest = config;
    while let Some(dollar) = rest.find('$') {
        push_literal(&mut parts, &rest[..dollar]);
        let after = &rest[dollar + 1..];
        if after.starts_with('$') || after.starts_with('!') {
            push_literal(&mut parts, &after[..1]);
            rest = &after[1..];
        } else if let Some(braced) = after.strip_prefix('{') {
            match braced.find('}') {
                None => {
                    push_literal(&mut parts, "$");
                    rest = after;
                }
                Some(end) => {
                    let name = &braced[..end];
                    if !name.is_empty() && env_name_len(name) == name.len() {
                        parts.push(TemplatePart::Env(name.to_string()));
                    } else {
                        push_literal(&mut parts, &rest[dollar..dollar + end + 3]);
                    }
                    rest = &braced[end + 1..];
                }
            }
        } else {
            let len = env_name_len(after);
            if len > 0 {
                parts.push(TemplatePart::Env(after[..len].to_string()));
            } else {
                push_literal(&mut parts, "$");
            }
            rest = &after[len..];
        }
    }
    push_literal(&mut parts, rest);
    parts
}

/// How long a `!command` value may run (`execSync`'s `timeout: 10000`).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// `commandResultCache`: every `!command` runs once per process; failures are cached too.
static COMMAND_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, Option<String>>>> =
    std::sync::Mutex::new(None);

/// `executeWithDefaultShell`: `/bin/sh -c <command>` with `envs` added to the environment, stdin and stderr ignored,
/// for at most ten seconds; its trimmed stdout, or `None` when it fails, times out, or prints nothing. Uncached
/// (models.json and MCP values, `resolveConfigValueUncached`); [`command_value`] caches it.
fn command_output(command: &str, envs: &[(String, String)]) -> Option<String> {
    use std::process::{Command, Stdio};
    let bound = signals::child::Bound {
        timeout: Some(COMMAND_TIMEOUT),
        ..Default::default()
    };
    let out = signals::child::run(
        Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .envs(envs.iter().map(|(k, v)| (k, v)))
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        None,
        bound,
    )
    .ok()?;
    if !out.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// `executeCommand`: a `!command` value (the command without its `!`), cached for the process's lifetime.
pub fn command_value(command: &str) -> Option<String> {
    let key = format!("!{command}");
    if let Some(cached) = COMMAND_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|cache| cache.get(&key))
    {
        return cached.clone();
    }
    let value = command_output(command, &[]);
    COMMAND_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(key, value.clone());
    value
}

/// `clearConfigValueCache`: forget every cached `!command` result.
pub fn clear_command_cache() {
    *COMMAND_CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// `resolveConfigValue`: a `!command`'s trimmed output (run once per process, failures included), or the template
/// with its variables filled; `None` when the command fails or prints nothing, or a referenced variable is unset or
/// empty.
pub fn resolve_config_value(config: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Option<String>, String> {
    match config.strip_prefix('!') {
        Some(command) => Ok(command_value(command)),
        None => Ok(fill_template(config, env)),
    }
}

/// A template with its variables filled; `None` when a referenced variable is unset or empty.
fn fill_template(config: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let mut out = String::new();
    for part in parse_template(config) {
        match part {
            TemplatePart::Literal(s) => out.push_str(&s),
            TemplatePart::Env(name) => out.push_str(&env(&name).filter(|v| !v.is_empty())?),
        }
    }
    Some(out)
}

/// `!command` values run a shell command; everything else is a template.
pub fn is_command(config: &str) -> bool {
    config.starts_with('!')
}

/// `getConfigValueEnvVarNames`: the variables a template names, first occurrence first.
pub fn env_names(config: &str) -> Vec<String> {
    if is_command(config) {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    for part in parse_template(config) {
        if let TemplatePart::Env(name) = part
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    names
}

/// `resolveConfigValueUncached`: `!command` output (run with `envs` added to its environment), or the template with
/// its variables filled from `lookup` (None when one is unset or empty).
pub fn resolve_uncached(
    config: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    envs: &[(String, String)],
) -> Option<String> {
    match config.strip_prefix('!') {
        Some(command) => command_output(command, envs),
        None => fill_template(config, lookup),
    }
}

/// `resolveConfigValueOrThrow`, with its messages.
pub fn resolve_or_err(
    config: &str,
    description: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    envs: &[(String, String)],
) -> Result<String, String> {
    if let Some(value) = resolve_uncached(config, lookup, envs) {
        return Ok(value);
    }
    if let Some(command) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {description} from shell command: {command}"));
    }
    let missing: Vec<String> = env_names(config)
        .into_iter()
        .filter(|name| lookup(name).is_none_or(|v| v.is_empty()))
        .collect();
    Err(match missing.as_slice() {
        [] => format!("Failed to resolve {description}"),
        [one] => format!("Failed to resolve {description} from environment variable: {one}"),
        many => format!(
            "Failed to resolve {description} from environment variables: {}",
            many.join(", ")
        ),
    })
}

/// `resolveHeadersOrThrow`: every header resolved, in order; None when there are none.
pub fn resolve_headers_or_err(
    headers: &[(String, String)],
    description: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
    envs: &[(String, String)],
) -> Result<Option<Vec<(String, String)>>, String> {
    let mut out = Vec::new();
    for (key, value) in headers {
        let resolved = resolve_or_err(value, &format!("{description} header \"{key}\""), lookup, envs)?;
        out.push((key.clone(), resolved));
    }
    Ok((!out.is_empty()).then_some(out))
}

/// `ReadOnlyAuthStorage`'s validation: every entry an `api_key` (string key, string env map) or a complete `oauth`
/// credential (string access/refresh, finite numeric expires).
pub fn validate_auth_file(content: &str) -> Result<AuthData, String> {
    let parsed = parse(content).map_err(|e| format!("Failed to read auth.json: {e}"))?;
    let Value::Object(data) = parsed else {
        return Err("Invalid auth.json: expected an object".into());
    };
    for (provider, credential) in &data {
        let valid = match credential {
            Value::Object(c) => match c.get("type").and_then(Value::as_str) {
                Some("api_key") => {
                    c.get("key").is_none_or(Value::is_string)
                        && c.get("env")
                            .is_none_or(|e| e.as_object().is_some_and(|m| m.values().all(Value::is_string)))
                }
                Some("oauth") => {
                    c.get("access").is_some_and(Value::is_string)
                        && c.get("refresh").is_some_and(Value::is_string)
                        && c.get("expires").and_then(Value::as_f64).is_some_and(f64::is_finite)
                }
                _ => false,
            },
            _ => false,
        };
        if !valid {
            return Err(format!("Invalid auth.json credential for provider \"{provider}\""));
        }
    }
    Ok(data)
}

/// `readStoredCredential`: the raw entry, or `None` when the file is missing or malformed.
pub fn read_stored_credential(path: &Path, provider: &str) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    parse(&content).ok()?.get(provider).cloned()
}

/// The authority of `url` (`host[:port]`, lowercased, without `user:pass@`) and its scheme; `None` when `url` is
/// not `scheme://authority...`.
fn authority(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = jsrt::string::trim(url).split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest[..end].rsplit_once('@').map_or(&rest[..end], |(_, host)| host);
    (!scheme.is_empty() && !authority.is_empty()).then(|| (scheme.to_ascii_lowercase(), authority.to_ascii_lowercase()))
}

/// `scheme://host[:port]` of `url`, the part a stored key is bound to.
pub fn url_origin(url: &str) -> Option<String> {
    authority(url).map(|(scheme, authority)| format!("{scheme}://{authority}"))
}

/// `url` without its `user:pass@`, for messages.
pub fn display_url(url: &str) -> String {
    match jsrt::string::trim(url).split_once("://") {
        Some((scheme, rest)) => {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            match rest[..end].rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://{host}{}", &rest[end..]),
                None => url.to_string(),
            }
        }
        None => url.to_string(),
    }
}

/// A service a credential may be sent to: https, or plain http to this machine only (`localhost`, `127.0.0.0/8`,
/// `[::1]`).
pub fn secure_url(url: &str) -> Result<(), String> {
    let (scheme, authority) = authority(url).ok_or_else(|| format!("{} is not a URL", display_url(url)))?;
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default().to_string(),
        None => authority
            .rsplit_once(':')
            .map_or(authority.as_str(), |(host, _)| host)
            .to_string(),
    };
    let loopback =
        host == "localhost" || host == "::1" || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback());
    match scheme.as_str() {
        "https" => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(format!(
            "{} is not https: a credential goes over plain http only to this machine",
            display_url(url)
        )),
    }
}

/// A service's credential (a judge backend's key, the Neo4j mirror's password): its environment variable and its
/// auth.json entry, named so that no model provider can use it (a provider's keys are looked up by provider id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServiceKey {
    pub entry: &'static str,
    pub env: &'static str,
}

/// TypeSafe's Jev.
pub const TYPESAFE_KEY: ServiceKey = ServiceKey {
    entry: "judge:typesafe",
    env: "TYPESAFE_API_KEY",
};
/// A d1 server.
pub const D1_KEY: ServiceKey = ServiceKey {
    entry: "judge:d1",
    env: "ULTRON_D1_KEY",
};
/// The password of `graph.neo4j.user`.
pub const NEO4J_PASSWORD: ServiceKey = ServiceKey {
    entry: "graph:neo4j",
    env: "NEO4J_PASSWORD",
};

/// Where a service's credential comes from now (never the credential itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOrigin {
    Env,
    /// In auth.json, bound to this origin.
    Stored(String),
    Unset,
}

/// A credential that is not empty.
fn given(value: Option<String>) -> Option<String> {
    value.filter(|v| !jsrt::string::trim(v).is_empty())
}

/// The stored entry of `key`: its value (an `api_key`, `$VAR` or `!command` resolved) and the origin it was saved
/// for.
fn stored(key: &ServiceKey, auth_path: &Path) -> Option<(String, Option<String>)> {
    let stored = AuthStorage::create(auth_path).read(key.entry).ok()??;
    let value = given(stored.get("key").and_then(Value::as_str).map(str::to_string))
        .filter(|_| stored.get("type").and_then(Value::as_str) == Some("api_key"))?;
    Some((value, stored.get("origin").and_then(Value::as_str).map(str::to_string)))
}

/// Where `key` comes from now: its environment variable, else its auth.json entry.
pub fn key_origin(key: &ServiceKey, var: &dyn Fn(&str) -> Option<String>, auth_path: &Path) -> KeyOrigin {
    if given(var(key.env)).is_some() {
        return KeyOrigin::Env;
    }
    match stored(key, auth_path) {
        Some((_, origin)) => KeyOrigin::Stored(origin.unwrap_or_default()),
        None => KeyOrigin::Unset,
    }
}

/// `key` for a request to `url`, read now: its environment variable, else its auth.json entry when that was saved
/// for `url`'s origin. `Err` when the credential must not go to `url`: it is not https (and not this machine), or
/// the stored one was saved for another server. `Ok(None)`: there is none.
pub fn service_key(
    key: &ServiceKey,
    var: &dyn Fn(&str) -> Option<String>,
    auth_path: &Path,
    url: &str,
) -> Result<Option<String>, String> {
    let from_env = given(var(key.env));
    let stored = match from_env {
        Some(_) => None,
        None => stored(key, auth_path),
    };
    if from_env.is_none() && stored.is_none() {
        return Ok(None);
    }
    secure_url(url)?;
    if let Some(value) = from_env {
        return Ok(Some(value));
    }
    let (value, origin) = stored.expect("checked above");
    let target = url_origin(url);
    if origin.is_none() || origin != target {
        return Err(format!(
            "the {} credential in auth.json was saved for {}, not {} (save it again for this server with `ultron setup`, or set {})",
            key.entry,
            origin.as_deref().unwrap_or("no server"),
            target.as_deref().unwrap_or(url),
            key.env
        ));
    }
    Ok(Some(value))
}

/// An environment's lookup, shared across threads.
pub type Vars = std::sync::Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// A service's credential for the URL a request goes to ([`service_key`]), read at each request.
pub type ServiceKeySource = std::sync::Arc<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// [`service_key`] of `key` with the environment `var` and the auth.json at `auth_path`.
pub fn service_key_source(key: ServiceKey, var: Vars, auth_path: PathBuf) -> ServiceKeySource {
    std::sync::Arc::new(move |url: &str| service_key(&key, &*var, &auth_path, url))
}

/// Save `value` as `key`'s auth.json credential, bound to `url`'s origin (the setup wizard).
pub fn save_service_key(auth_path: &Path, key: &ServiceKey, value: &str, url: &str) -> Result<(), String> {
    let origin = url_origin(url).ok_or_else(|| format!("{} is not a URL", display_url(url)))?;
    AuthStorage::create(auth_path)
        .modify(key.entry, |_| {
            Some(serde_json::json!({"type": "api_key", "key": jsrt::string::trim(value), "origin": origin}))
        })
        .map(|_| ())
}
