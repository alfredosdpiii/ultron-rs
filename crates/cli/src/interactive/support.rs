//! Interactive TUI helper operations: session import/export/share, credentials, trust, debug log, and plugins.

use super::PI_AGENT_DIR_ENV;
use crate::Env;
use client::session::SessionLink as Link;
use interactive::lane::{BashResult, LaneEvent, ModelInfo, PromptImage, SessionListing};
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::Arc;

/// `importPiSessionFile` (client-tui-sessions.ts): this process reads the Pi file and the server imports its text
/// (the server may run elsewhere); the answer says what was imported and where to switch.
pub fn import_pi(link: &Link, path: &str) -> LaneEvent {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            let message = jsrt::fs::error_message(&error, "open", std::path::Path::new(path));
            return LaneEvent::Status(format!("Error: Failed to import session: {message}"));
        }
    };
    let result = match link.import_pi(path, &content) {
        Ok(result) => result,
        Err(error) => return LaneEvent::Status(format!("Error: Failed to import session: {error}")),
    };
    let session_id = result["session"]["sessionId"].as_str().unwrap_or_default().to_string();
    let imported = if result["alreadyImported"] == true {
        format!("Already imported as session {session_id}")
    } else {
        format!(
            "Imported {} entries from {path} as session {session_id}",
            result["imported"].as_u64().unwrap_or(0)
        )
    };
    let current = session_id == link.session_id();
    LaneEvent::PiImported {
        imported,
        source: path.to_string(),
        session_file: if current {
            None
        } else {
            link.session_file(&session_id).ok().flatten()
        },
        current,
    }
}

/// Pi `ImageContent` blocks.
pub(super) fn image_values(images: Vec<PromptImage>) -> Vec<Value> {
    images
        .into_iter()
        .map(|i| json!({"type": "image", "data": i.data, "mimeType": i.mime_type}))
        .collect()
}

pub(super) fn model_info(model: &Value) -> ModelInfo {
    if let Ok(model) = serde_json::from_value::<ai::Model>(model.clone()) {
        return catalog_info(&model);
    }
    ModelInfo {
        provider: model["provider"].as_str().unwrap_or_default().to_string(),
        id: model["id"].as_str().unwrap_or_default().to_string(),
        name: model["name"].as_str().unwrap_or_default().to_string(),
        api: model["api"].as_str().unwrap_or_default().to_string(),
        reasoning: model["reasoning"] == true,
        context_window: model["contextWindow"].as_u64().unwrap_or(0),
        thinking_levels: Vec::new(),
    }
}

pub(super) fn catalog_info(m: &ai::Model) -> ModelInfo {
    ModelInfo {
        provider: m.provider.clone(),
        id: m.id.clone(),
        name: m.name.clone(),
        api: m.api.clone(),
        reasoning: m.reasoning,
        context_window: m.context_window,
        thinking_levels: ai::models::supported_thinking_levels(m)
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// Pi's profile directory (read-only here): `PI_CODING_AGENT_DIR`, else `~/.pi/agent` (client-tui-sessions.ts
/// `piAgentDir`).
pub fn pi_agent_dir(env: &Env) -> String {
    let dir = (env.var)(PI_AGENT_DIR_ENV).unwrap_or_else(|| env.home.join(".pi").join("agent").display().to_string());
    config::paths::resolve(&dir, &env.cwd, &env.home).display().to_string()
}

/// `SessionManager.list(cwd, piSessionDirForCwd(cwd, piAgentDir))`: Pi's sessions of this project, newest first, as
/// Pi's selector lists them. Pi's files are only read.
pub fn pi_sessions(env: &Env) -> Result<Vec<SessionListing>, String> {
    let cwd = env.cwd.display().to_string();
    let dir = session_file::default_session_dir_path(&session_file::SystemEnv, &cwd, &pi_agent_dir(env));
    let sessions =
        session_file::list_sessions(&session_file::SystemEnv, &cwd, Some(&dir)).map_err(|e| e.to_string())?;
    Ok(sessions
        .into_iter()
        .map(|s| SessionListing {
            path: s.path,
            id: s.id,
            name: s.name,
            modified: s.modified.unwrap_or(0),
            message_count: s.message_count,
            first_message: s.first_message,
            all_messages_text: s.all_messages_text,
            cwd: s.cwd,
        })
        .collect())
}

pub(super) fn session_call(link: &Link, service: &str, member: &str, args: Value) -> Result<Option<Value>, String> {
    let connection = link.connection();
    connection
        .session_target()
        .and_then(|target| connection.call(target, service, member, args))
        .map_err(client::lost::message)
}

/// `/trust`: the worker's `readSettings` and the trust choices for its cwd (`getProjectTrustOptions`).
pub(super) fn read_trust(link: &Link) -> Result<interactive::trust_selector::TrustView, String> {
    let read = session_call(link, protocol::names::SESSION_CONTROL, "readSettings", json!([]))?.unwrap_or_default();
    let cwd = read["cwd"].as_str().unwrap_or_default().to_string();
    let display = |p: &std::path::Path| p.display().to_string();
    let choices = config::trust::trust_options(std::path::Path::new(&cwd), false)
        .into_iter()
        .map(|option| interactive::trust_selector::TrustChoice {
            label: option.label,
            trusted: option.trusted,
            updates: option.updates.iter().map(|u| (display(&u.path), u.decision)).collect(),
            saved_path: option.saved_path.as_deref().map(display),
        })
        .collect();
    Ok(interactive::trust_selector::TrustView {
        cwd,
        choices,
        saved: read["savedTrust"]["path"]
            .as_str()
            .map(|path| (path.to_string(), read["savedTrust"]["decision"] == true)),
        project_trusted: read["projectTrusted"] == true,
    })
}

/// The socket of server `server_id` in this project's instance or the durable one, whichever has it.
pub(super) fn local_server_socket(env: &Env, server_id: &str) -> Option<std::path::PathBuf> {
    [false, true]
        .into_iter()
        .map(|durable| daemon::instance::socket_path(&crate::instance(env, durable).dir, server_id))
        .find(|p| p.exists())
}

/// `/debug`: the worker's `debugInfo` and this client's and server's facts with what the TUI shows, written to
/// `<agentDir>/ultron-debug.log` (0600).
pub(super) fn write_debug_log(
    link: &Link,
    env: &Env,
    agent_dir: &std::path::Path,
    capture: &interactive::debug_log::DebugCapture,
) -> LaneEvent {
    let worker = session_call(link, protocol::names::SESSION_CONTROL, "debugInfo", json!([]))
        .map(Option::unwrap_or_default)
        .map_err(|e| {
            if e == "Session is not attached" {
                "No Session is attached".into()
            } else {
                e
            }
        });
    let server_id = link.connection().server_id.clone();
    let socket = local_server_socket(env, &server_id);
    let client = interactive::debug_log::ClientFacts {
        time: jsrt::date::iso_string(jsrt::date::now_ms()),
        process: format!(
            "ultron {} · rust · {} · pid {}",
            config::version(),
            daemon::session_control::platform(),
            std::process::id()
        ),
        cwd: std::env::current_dir().map_or_default(|d| d.display().to_string()),
        server_id: Some(server_id),
        transport: Some(link.remote_route().map_or("unix", |r| r.transport()).into()),
        socket_path: match link.remote_route() {
            Some(route) => route.socket_path().map(|p| p.display().to_string()),
            None => socket.map(|p| p.display().to_string()),
        },
    };
    let (data, summary) = interactive::debug_log::debug_log(capture, &client, &worker);
    let path = config::paths::AgentPaths::new(agent_dir).debug_log();
    let written = std::fs::create_dir_all(agent_dir).and_then(|()| {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| std::io::Write::write_all(&mut file, data.as_bytes()))
    });
    match written {
        Ok(()) => LaneEvent::DebugLogWritten {
            path: path.display().to_string(),
            summary,
        },
        Err(error) => LaneEvent::Status(format!("Error: Could not write debug log: {error}")),
    }
}

/// `createPresentationFacetLoaders` (plugins/bundled.ts): the presentation-plugin data a server sent is an object
/// whose `presentationFacetBundles`, when present, is a list of facet bundle artifacts. Bundles are JavaScript facets
/// the TypeScript TUI loads; this client cannot run them, so data that selects any is refused (the Rust server never
/// selects any: plugin packages are not built by it).
pub fn apply_presentation_plugins(data: &Value) -> Result<(), String> {
    let Some(object) = data.as_object() else {
        return Err("Invalid presentation plugin data".into());
    };
    match object.get("presentationFacetBundles") {
        None => Ok(()),
        Some(Value::Array(bundles)) if bundles.is_empty() => Ok(()),
        Some(Value::Array(bundles)) => Err(format!(
            "{} presentation plugin bundle(s) cannot be loaded by this client",
            bundles.len()
        )),
        Some(_) => Err("Invalid presentation plugin bundle list".into()),
    }
}

/// `/reload` (slash-commands-provider.ts): the server rebuilds this client's prepared presentation plugins, the
/// attached worker reloads its session plugins, then the client applies the rebuilt presentation data
/// (`reloadPresentationPlugins(data)`, client-tui.ts:579-600).
pub(super) fn reload_plugins(link: &Link) -> Result<(), String> {
    let connection = link.connection();
    let data = connection
        .call(
            connection.server_target(),
            protocol::names::PRESENTATION_PLUGINS,
            "reload",
            json!([]),
        )
        .map_err(client::lost::message)?
        .unwrap_or(Value::Null);
    session_call(link, protocol::names::SESSION_PLUGINS, "reload", json!([]))?;
    apply_presentation_plugins(&data)
}

fn check_gh_auth() -> Result<(), String> {
    let auth = std::process::Command::new("gh")
        .args(["auth", "status"])
        .stdin(std::process::Stdio::null())
        .output();
    match auth {
        Err(_) => Err("Error: GitHub CLI (gh) is not installed. Install it from https://cli.github.com/".into()),
        Ok(out) if !out.status.success() => {
            Err("Error: GitHub CLI is not logged in. Run 'gh auth login' first.".into())
        }
        Ok(_) => Ok(()),
    }
}

fn parse_gist_output(out: &signals::child::Output, env: &Env) -> Result<String, String> {
    if !out.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let detail = if stderr.is_empty() { "Unknown error" } else { &stderr };
        return Err(format!("Error: Failed to create gist: {detail}"));
    }
    let gist_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let gist_id = gist_url
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "Error: Failed to parse gist ID from gh output".to_string())?;
    let preview = crate::export_html::share_viewer_url(&|k| (env.var)(k), gist_id);
    Ok(format!(
        "Share URL: {}\nGist: {}",
        tui::hyperlink(&preview, &preview),
        tui::hyperlink(&gist_url, &gist_url)
    ))
}

fn upload_share_gist(
    link: &Link,
    send: &dyn Fn(LaneEvent),
    cancel: &kernel::Signal,
    theme: Option<&str>,
    env: &Env,
    file: &std::path::Path,
) -> Result<Option<String>, String> {
    export_session(link, Some(&file.display().to_string()), theme, env)
        .map_err(|e| format!("Error: Failed to export session: {e}"))?;
    send(LaneEvent::ShareLoader(true));
    let out = gist_create(file, cancel);
    send(LaneEvent::ShareLoader(false));
    let Some(out) = out.map_err(|e| format!("Error: Failed to create gist: {e}"))? else {
        return Ok(None);
    };
    parse_gist_output(&out, env).map(Some)
}

/// Pi's `/share`: the session as HTML in a secret gist (`gh gist create --public=false`), then the viewer and gist
/// URLs.
pub(super) fn share_session(
    link: &Link,
    send: &dyn Fn(LaneEvent),
    cancel: &kernel::Signal,
    theme: Option<&str>,
    env: &Env,
) {
    let status = |text: String| send(LaneEvent::Status(text));
    if let Err(error) = check_gh_auth() {
        return status(error);
    }
    let dir = jsrt::fs::temp_dir().join(format!("ultron-share-{}", jsrt::random::uuid_v4()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        return status(format!("Error: Failed to export session: {error}"));
    }
    let file = dir.join("session.html");
    let result = upload_share_gist(link, send, cancel, theme, env, &file);
    let _ = std::fs::remove_dir_all(&dir);
    match result {
        Ok(Some(text)) | Err(text) => status(text),
        Ok(None) => {}
    }
}

/// `gh gist create --public=false <file>`, killed when `cancel` aborts (`None` then).
fn gist_create(file: &std::path::Path, cancel: &kernel::Signal) -> std::io::Result<Option<signals::child::Output>> {
    let out = cancel.run_child(
        std::process::Command::new("gh")
            .args(["gist", "create", "--public=false"])
            .arg(file)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped()),
        None,
        signals::child::Bound::default(),
    )?;
    Ok((out.end != signals::child::End::Cancelled).then_some(out))
}

/// What follows saving a credential (`runLoginDialog`): where it went, the refreshed catalog, and whether the session's
/// model is that provider's.
pub(super) fn credentials_saved(
    link: &Link,
    env: &Env,
    send: &dyn Fn(LaneEvent),
    action: &str,
    provider: &str,
    name: &str,
) {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let done = format!("{action} {name}. Credentials saved to {}", auth.display());
    send(LaneEvent::Status(format!(
        "{done}. Reloading the Session's credentials…"
    )));
    send(LaneEvent::Catalog(catalog(env)));
    let model = link.op(&Op::Describe).ok().flatten().map(|d| model_info(&d["model"]));
    let hint = match model {
        None => format!(". Use /model to select one of {name}'s models"),
        Some(m) if m.id.is_empty() => format!(". Use /model to select one of {name}'s models"),
        Some(m) if m.provider == provider => String::new(),
        Some(m) => format!(
            ". The Session still uses {}/{}; /model switches to {name}'s models",
            m.provider, m.id
        ),
    };
    send(LaneEvent::Status(format!("{done}{hint}")));
}

fn builtin_login_options(
    stored: &std::collections::HashMap<String, String>,
    source: &ai::env::EnvSource<'_>,
) -> Vec<interactive::selectors::AuthProvider> {
    use interactive::selectors::{AuthProvider, provider_logins};
    let mut options = Vec::new();
    for p in provider_logins() {
        let status = match stored.get(&p.id) {
            Some(kind) => Some((kind.clone(), Some("stored credential".to_string()))),
            None if p.id == ai::claude_code::PROVIDER => ai::claude_code::configured(source)
                .then(|| ("api_key".to_string(), Some(ai::claude_code::AUTH_SOURCE.to_string()))),
            None => ai::env::find_env_keys_in(&p.id, source)
                .filter(|keys| !keys.is_empty())
                .map(|keys| ("api_key".to_string(), Some(keys.join(", ")))),
        };
        let base = AuthProvider {
            id: p.id.clone(),
            name: p.name.clone(),
            auth_type: String::new(),
            status,
            api_key_method: p.api_key.clone(),
            login_label: p.oauth.clone().flatten(),
            generic_login: !interactive::api_key_login::MULTI_FIELD_PROVIDERS.contains(&p.id.as_str()),
        };
        if p.oauth.is_some() {
            options.push(AuthProvider {
                auth_type: "oauth".into(),
                ..base.clone()
            });
        }
        if p.api_key.is_some() {
            options.push(AuthProvider {
                auth_type: "api_key".into(),
                ..base
            });
        }
    }
    options
}

fn radius_login_options(
    registry: &ai::model_registry::ModelRegistry,
    stored: &std::collections::HashMap<String, String>,
    source: &ai::env::EnvSource<'_>,
) -> Vec<interactive::selectors::AuthProvider> {
    use interactive::selectors::{AuthProvider, provider_logins};
    let mut options = Vec::new();
    let radius = provider_logins().into_iter().find(|p| p.id == "radius");
    for id in registry.config().provider_ids() {
        let builtin = ai::models::builtin_providers().any(|p| p == id);
        let (Some((name, _)), Some(radius), false) = (registry.radius_gateway(id), radius.as_ref(), builtin) else {
            continue;
        };
        let status = match stored.get(id) {
            Some(kind) => Some((kind.clone(), Some("stored credential".to_string()))),
            None => ai::env::find_env_keys_in("radius", source)
                .filter(|keys| !keys.is_empty())
                .map(|keys| ("api_key".to_string(), Some(keys.join(", ")))),
        };
        let base = AuthProvider {
            id: id.to_string(),
            name,
            auth_type: "oauth".into(),
            status,
            api_key_method: radius.api_key.clone(),
            login_label: radius.oauth.clone().flatten(),
            generic_login: true,
        };
        let api_key = radius.api_key.is_some().then(|| AuthProvider {
            auth_type: "api_key".into(),
            ..base.clone()
        });
        options.push(base);
        options.extend(api_key);
    }
    options
}

fn custom_login_options(
    registry: &ai::model_registry::ModelRegistry,
    stored: &std::collections::HashMap<String, String>,
    env: &Env,
) -> Vec<interactive::selectors::AuthProvider> {
    use interactive::selectors::AuthProvider;
    let mut options = Vec::new();
    for id in registry.config().provider_ids() {
        let builtin = ai::models::builtin_providers().any(|p| p == id);
        if builtin || registry.radius_gateway(id).is_some() {
            continue;
        }
        let config = registry.config().provider(id);
        let name = config
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string();
        let status = match stored.get(id) {
            Some(kind) => Some((kind.clone(), Some("stored credential".to_string()))),
            None => config
                .and_then(|c| c.get("apiKey"))
                .and_then(Value::as_str)
                .and_then(|key| configured_key_status(key, &*env.var)),
        };
        options.push(AuthProvider {
            id: id.to_string(),
            name,
            auth_type: "api_key".into(),
            status,
            api_key_method: Some(("API key".into(), true)),
            login_label: None,
            generic_login: true,
        });
    }
    options
}

/// `loginProviderOptions`: every built-in provider's sign-in options with what is configured (a stored credential,
/// else the variables that supply a key), sorted by name.
pub fn login_options(env: &Env, auth: &std::path::Path) -> Vec<interactive::selectors::AuthProvider> {
    let stored: std::collections::HashMap<String, String> = config::auth::AuthStorage::create(auth.to_path_buf())
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|(id, c)| (id, c["type"].as_str().unwrap_or("api_key").to_string()))
        .collect();
    let source = ai::env::EnvSource {
        lookup: &*env.var,
        exists: &|p: &std::path::Path| p.exists(),
        home: Some(env.home.clone()),
    };
    let registry = session_core::lane::registry(env);
    let mut options = builtin_login_options(&stored, &source);
    options.extend(radius_login_options(&registry, &stored, &source));
    options.extend(custom_login_options(&registry, &stored, env));
    options.sort_by(|a, b| jsrt::collate::locale_compare(&a.name, &b.name));
    options
}

/// `configuredRequestAuthStatus` of a models.json `apiKey`: a `!command`, the environment variables a template names
/// (when all are set), or the key itself.
fn configured_key_status(key: &str, var: &dyn Fn(&str) -> Option<String>) -> Option<(String, Option<String>)> {
    if key.starts_with('!') {
        return Some(("api_key".into(), Some("models_json_command".into())));
    }
    let names = config::auth::env_names(key);
    if names.is_empty() {
        return Some(("api_key".into(), Some("models_json_key".into())));
    }
    names
        .iter()
        .all(|name| var(name).is_some_and(|v| !v.is_empty()))
        .then(|| ("api_key".into(), Some(names.join(", "))))
}

/// The models the providers with credentials make available.
pub(super) fn catalog(env: &Env) -> Vec<ModelInfo> {
    session_core::lane::available_models(env)
        .iter()
        .map(catalog_info)
        .collect()
}

/// An operation of the attached session that any thread can run (the link stays with its thread).
pub(super) fn session_op_runner(link: &Link) -> Arc<dyn Fn(Op) -> Result<Option<Value>, String> + Send + Sync> {
    let connection = link.connection();
    Arc::new(move |op: Op| {
        let session = connection.session_target().map_err(client::lost::message)?;
        let request = connection
            .start(session, protocol::names::SESSION_OPS, "op", json!([op.to_json()]))
            .map_err(client::lost::message)?;
        connection.wait(&request).map_err(client::lost::message)
    })
}

/// Start a long call of the attached session (one the worker runs on its own thread, so later calls such as
/// `abortBash` are not queued behind it) without waiting for it.
pub(super) fn start_call(
    connection: &Arc<client::runtime::Connection>,
    service: &str,
    member: &str,
    args: Value,
) -> Result<(Arc<client::runtime::Connection>, String), String> {
    let session = connection.session_target().map_err(client::lost::message)?;
    let request = connection
        .start(session, service, member, args)
        .map_err(client::lost::message)?;
    Ok((connection.clone(), request))
}

pub(super) fn bash_result(value: &Value) -> BashResult {
    BashResult {
        output: value["output"].as_str().unwrap_or_default().to_string(),
        exit_code: value["exitCode"].as_i64(),
        cancelled: value["cancelled"] == true,
        truncated: value["truncated"] == true,
        full_output_path: value["fullOutputPath"].as_str().map(str::to_string),
    }
}

/// `#forkSession`: a new session from this one's `forkPath`, the fork recorded in it, then attached.
pub(super) fn fork_to(link: &mut Link, fork_path: Value) -> Result<(), String> {
    let source = link.session_id();
    let created = link.create(json!({"forkFromSessionId": source, "forkPath": fork_path}))?;
    if let Some(file) = link.session_file(&created)? {
        link.op(&Op::Forked { target: file })?;
    }
    link.attach(&created)
}

fn session_file_stem(described: &Value, session_id: &str) -> String {
    described["sessionFile"]
        .as_str()
        .and_then(|f| std::path::Path::new(f).file_name())
        .map_or_else(
            || session_id.to_string(),
            |n| {
                let name = n.to_string_lossy();
                match name.rfind('.') {
                    Some(dot) if dot > 0 => name[..dot].to_string(),
                    _ => name.into_owned(),
                }
            },
        )
}

fn export_jsonl(cwd: &std::path::Path, path: &str, header: &Value, entries: &[Value]) -> Result<String, String> {
    let path = config::paths::normalize(&cwd.join(path));
    let lines: Vec<String> = std::iter::once(header).chain(entries).map(Value::to_string).collect();
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

/// Pi's `/export` (`exportSession`): HTML through the exporter, or the Pi-format JSONL, from the session's entries
/// as Pi entries; returns the path written.
pub(super) fn export_session(
    link: &Link,
    output: Option<&str>,
    theme: Option<&str>,
    env: &Env,
) -> Result<String, String> {
    let read = link.op(&Op::Entries { since: None })?.unwrap_or_default();
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    let session_id = link.session_id();
    let entries = read["entries"].as_array().cloned().unwrap_or_default();
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let timestamp = entries
        .first()
        .and_then(|e| e["timestamp"].as_str())
        .map_or_else(|| jsrt::date::iso_string(jsrt::date::now_ms()), str::to_string);
    let header = json!({"type": "session", "version": 3, "id": session_id, "timestamp": timestamp,
        "cwd": cwd.display().to_string()});
    if let Some(path) = output.filter(|p| p.ends_with(".jsonl")) {
        return export_jsonl(&cwd, path, &header, &entries);
    }
    let stem = session_file_stem(&described, &session_id);
    let data = json!({"header": header, "entries": entries, "leafId": read["leafId"]});
    let html = crate::export_html::export_page(&data, theme, env)?;
    let path = config::paths::normalize(
        &cwd.join(output.map_or_else(|| format!("ultron-session-{stem}.html"), str::to_string)),
    );
    std::fs::write(&path, html).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

/// `/usage`: the session report of the session's file (`renderSessionReport`), as `ultron usage` prints it.
pub(super) fn usage_report(link: &Link, env: &Env) -> Result<String, String> {
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    let file = described["sessionFile"]
        .as_str()
        .ok_or("The Session worker returned no session report")?;
    let log = usage::log::read_session_log(std::path::Path::new(file)).map_err(|e| e.message().to_string())?;
    let providers = session_core::lane::subscription_providers(env);
    let report = usage::report::build_session_report(&log, &providers);
    let zone = jsrt::localtime::Zone::from_env(std::env::var("TZ").ok().as_deref());
    let home = std::env::var("HOME").unwrap_or_default();
    let options = usage::text::TextOptions {
        utc: false,
        home: &home,
        zone: &zone,
    };
    Ok(usage::text::render_session_report(&report, &options).join("\n"))
}
