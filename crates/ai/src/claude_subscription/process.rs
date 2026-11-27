//! One `claude -p` process per model call (`directsdk.py` `Client._run`): locating the CLI,
//! the child environment (auth overrides refused, isolation and quiet flags set, credentials never touched), the
//! stable working directory, the private per-request files, the stream-json exchange, process-group cancellation
//! and the reconciliation of native's output with what the admission relay captured.

mod finalize;

use super::admission::{AbortHandle, Admission};
use super::catalog::native_model;
use super::history::{Body, PREFIX, conversation_session_id};
use super::{ClaudeError, Config, Env, INSTALL_HINT};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Settings variable naming the `claude` executable (an absolute path, or a name looked up on PATH).
pub const COMMAND_ENV: &str = "ULTRON_CLAUDE_COMMAND";
/// `0`/`false`/`no`/`off` turns Claude Code's telemetry and feature-flag fetch off (default off).
pub const TELEMETRY_ENV: &str = "ULTRON_CLAUDE_TELEMETRY";
/// Maximum concurrent `claude -p` turn processes per Ultron process (default 4).
pub const CONCURRENCY_ENV: &str = "ULTRON_CLAUDE_CONCURRENCY";
pub const DEFAULT_CONCURRENCY: usize = 4;

/// API credentials Claude Code would bill (the API, a gateway or Foundry) instead of the plan. They are removed from
/// the child's environment, never refused: the subscription stays primary, and `ANTHROPIC_API_KEY` is the API
/// credits fallback's key (`fallback`).
pub const API_CREDENTIALS: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_FOUNDRY_API_KEY"];
/// Conflicting endpoint overrides: refused (the relay owns `ANTHROPIC_BASE_URL`).
pub const AUTH_OVERRIDES: &[&str] = &["ANTHROPIC_BASE_URL"];
pub const BACKEND_OVERRIDES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Set whatever the telemetry setting: no self-update of the user's CLI, no feedback upload path, and no
/// background session-title request racing the turn for the one admitted upstream request.
pub const ALWAYS_QUIET: &[(&str, &str)] = &[
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_FEEDBACK_COMMAND", "1"),
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
];
/// Non-SDK caller identity (`anthropics/claude-code#97074`): `-p` defaults to `sdk-cli`, which subscriptions meter
/// roughly 2x heavier per token. `CLAUDE_CODE_DISABLE_ARTIFACT` keeps native's artifact scratchpad off.
pub const CALLER_IDENTITY: &[(&str, &str)] = &[
    ("CLAUDE_CODE_ENTRYPOINT", "ultron"),
    ("CLAUDE_CODE_DISABLE_ARTIFACT", "1"),
];
/// Set when telemetry is off (the default); any one of them turns the CLI's feature-flag fetch off (~0.25 s faster).
pub const QUIET_TRAFFIC: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
];
const ON: &[&str] = &["true", "1", "yes", "on"];
const REFRESH_LOCK_STALE: Duration = Duration::from_secs(60);

/// Inherited request knobs this provider owns; dropped with [`crate::claude_code_cli::PARENT_SESSION_ENV`].
const REQUEST_KNOB_ENV: [&str; 2] = ["CLAUDE_CODE_EXTRA_BODY", "CLAUDE_CODE_EFFORT_LEVEL"];

/// Install prefixes probed after PATH (a service or desktop launch inherits a PATH without them).
const HOME_PREFIXES: &[&str] = &[
    ".local/bin",
    ".claude/local",
    "bin",
    ".npm-global/bin",
    ".bun/bin",
    ".volta/bin",
];
const SYSTEM_PREFIXES: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];
const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// Native's own error codes for errors it answers without a relayed upstream status.
pub fn native_error_status(code: &str) -> Option<u16> {
    match code {
        "rate_limit" => Some(429),
        "billing_error" => Some(402),
        "authentication_failed" => Some(401),
        "overloaded" => Some(529),
        "server_error" => Some(503),
        _ => None,
    }
}

fn default_claude_command(env: &Env) -> Vec<String> {
    let named = |name: &str| env.get(name).filter(|v| !v.trim().is_empty()).cloned();
    let bin = named(COMMAND_ENV)
        .or_else(|| named(crate::claude_code_cli::CLAUDE_CODE_BIN_ENV).map(|v| v.trim().to_string()))
        .unwrap_or_else(|| "claude".into());
    vec![bin]
}

fn probe_install_dirs(head: &str, env: &Env) -> Option<PathBuf> {
    if head != "claude" {
        return None;
    }
    let home = env.get("HOME").filter(|h| !h.is_empty() && Path::new(h).is_absolute());
    let prefixes: Vec<String> = home
        .into_iter()
        .flat_map(|home| HOME_PREFIXES.iter().map(move |p| format!("{home}/{p}")))
        .chain(SYSTEM_PREFIXES.iter().map(std::string::ToString::to_string))
        .collect();
    jsrt::process::which(head, &prefixes.join(":"))
}

/// The argv prefix that runs Claude Code (`_resolve`): `command`, else `ULTRON_CLAUDE_COMMAND`, else
/// `ULTRON_CLAUDE_CODE_BIN` (the binary the `claude-code` provider runs and is listed by), else `claude` on the
/// env's PATH, then in the usual install directories. Only the bare default name is probed outside PATH.
pub fn resolve_claude(command: Option<&[String]>, env: &Env) -> Option<Vec<String>> {
    let command = match command {
        Some(command) if !command.is_empty() => command.to_vec(),
        _ => default_claude_command(env),
    };
    let head = &command[0];
    let found = if Path::new(head).is_absolute() && jsrt::process::is_executable(Path::new(head)) {
        Some(PathBuf::from(head))
    } else {
        jsrt::process::which(head, env.get("PATH").map_or(DEFAULT_PATH, String::as_str))
    };
    let found = found.or_else(|| probe_install_dirs(head, env))?;
    let mut resolved = vec![found.to_string_lossy().into_owned()];
    resolved.extend(command[1..].iter().cloned());
    Some(resolved)
}

fn truthy_flag(value: Option<&String>) -> bool {
    value.is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

/// The names of conflicting overrides set in `env` (values are never read beyond emptiness).
pub fn conflicts(env: &Env) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = AUTH_OVERRIDES
        .iter()
        .copied()
        .filter(|k| env.get(*k).is_some_and(|v| !v.is_empty()))
        .collect();
    found.extend(BACKEND_OVERRIDES.iter().copied().filter(|k| truthy_flag(env.get(*k))));
    found
}

/// Whether telemetry and feature flags stay on: the config, else `ULTRON_CLAUDE_TELEMETRY`, default off.
pub fn telemetry_enabled(config: &Config, env: &Env) -> bool {
    config.telemetry.unwrap_or_else(|| {
        env.get(TELEMETRY_ENV)
            .is_some_and(|v| ON.contains(&v.trim().to_ascii_lowercase().as_str()))
    })
}

/// Carry the login identity Claude Code's local check reads (`USER`/`LOGNAME`): without `USER`, a logged-in CLI
/// under a daemon/systemd unit can report `loggedIn: false` immediately with no upstream request.
fn with_login_identity(env: &mut Env) {
    let name = env
        .get("USER")
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.is_empty()));
    if let Some(name) = name {
        env.entry("USER".into()).or_insert_with(|| name.clone());
        env.entry("LOGNAME".into()).or_insert(name);
    }
}

/// Remove an empty `.oauth_refresh.lock` directory left behind by a Claude Code process that exited mid-refresh
/// (`anthropics/claude-code#95236`), once it is past Claude Code's 60 s stale window. Non-empty or fresh locks are
/// never touched (`remove_dir` refuses non-empty directories).
pub fn reap_orphaned_refresh_lock(env: &Env) {
    let config_dir = env
        .get("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .filter(|h| !h.is_empty() && Path::new(h).is_absolute())
                .map(|h| PathBuf::from(h).join(".claude"))
        });
    let Some(dir) = config_dir else { return };
    let lock = dir.join(".oauth_refresh.lock");
    let Ok(meta) = std::fs::symlink_metadata(&lock) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    if meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age > REFRESH_LOCK_STALE)
    {
        let _ = std::fs::remove_dir(&lock);
    }
}

/// Set the traffic flags on a child env. Never removes anything: an opt-out the user exported still applies.
pub fn apply_traffic_policy(env: &mut Env, telemetry: bool) {
    for (k, v) in ALWAYS_QUIET {
        env.insert(k.to_string(), v.to_string());
    }
    if !telemetry {
        for (k, v) in QUIET_TRAFFIC {
            env.insert(k.to_string(), v.to_string());
        }
    }
}

/// The env a request starts from: the config's env, else the process env; `overlay` on top (an empty value
/// removes a variable).
pub fn base_env(config: &Config, overlay: Option<&crate::env::ProviderEnv>) -> Env {
    let mut env: Env = config.env.clone().unwrap_or_else(|| std::env::vars().collect());
    for (k, v) in overlay.into_iter().flatten() {
        if v.is_empty() {
            env.remove(k);
        } else {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

/// Refuse conflicting overrides before anything starts; the error names variables, never values.
pub fn refuse_overrides(env: &Env) -> Result<(), ClaudeError> {
    let found = conflicts(env);
    if found.is_empty() {
        return Ok(());
    }
    Err(ClaudeError::Invalid(format!(
        "OAuth provider refuses conflicting native auth/backend overrides: {}. claude-subscription runs on the Claude Code login; unset them for this provider.",
        found.join(", ")
    )))
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Our own real directory, closed to others.
fn private_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == uid() && m.mode() & 0o077 == 0)
}

/// Close a directory this process just created, through a descriptor opened without following links (someone
/// who can write the parent could swap a symlink in after the mkdir). Best effort; `private_dir` decides.
fn tighten(path: &Path) {
    use rustix::fs::{Mode, OFlags};
    if let Ok(fd) = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        let _ = rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700));
    }
}

/// Create `path` closed to others; `false` when something already sits there.
fn create_private(path: &Path) -> std::io::Result<bool> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            tighten(path);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn touch(path: &Path) {
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.set_modified(SystemTime::now());
    }
}

/// One native cwd for every request of this OS user (`shared_workdir`): native writes its cwd into every request's
/// environment block, so a fresh directory per call would move the prompt-cache prefix. It must be our own real
/// directory, closed to others, in a parent nobody else can rename it out of; `None` falls back to a private one.
pub fn shared_workdir(temp_root: &Path) -> Option<PathBuf> {
    let path = temp_root.join(format!("ultron-claude-cwd-{}", uid()));
    let parent = std::fs::metadata(temp_root).ok()?.mode();
    if parent & 0o022 != 0 && parent & 0o1000 == 0 {
        return None;
    }
    create_private(&path).ok()?;
    if !private_dir(&path) {
        return None;
    }
    touch(&path);
    Some(path)
}

static PROCESS_WORKDIRS: LazyLock<Mutex<std::collections::HashMap<PathBuf, PathBuf>>> = LazyLock::new(Mutex::default);

fn private_temp(temp_root: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    loop {
        let path = temp_root.join(format!("{prefix}{}", jsrt::random::random_hex(6)));
        if create_private(&path)? {
            return Ok(path);
        }
    }
}

/// One private cwd for every request of this process when the shared one is refused (`process_workdir`):
/// recreated at the same path after a prune, never adopted when someone else recreated it.
pub fn process_workdir(temp_root: &Path) -> std::io::Result<PathBuf> {
    let mut table = jsrt::sync::lock(&PROCESS_WORKDIRS);
    if let Some(existing) = table.get(temp_root) {
        let _ = create_private(existing);
        if private_dir(existing) {
            touch(existing);
            return Ok(existing.clone());
        }
    }
    let fresh = private_temp(temp_root, "ultron-claude-cwd-")?;
    table.insert(temp_root.to_path_buf(), fresh.clone());
    Ok(fresh)
}

/// The shared cwd, else this process's private one.
pub fn workdir(temp_root: &Path) -> std::io::Result<PathBuf> {
    match shared_workdir(temp_root) {
        Some(path) => Ok(path),
        None => process_workdir(temp_root),
    }
}

/// A model call ready to run: the system prompt, the stream-json frames and the extra body.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The model as selected (`sonnet`, `claude-opus-5-5[1m]`, ...).
    pub model: String,
    pub system: String,
    pub frames: Vec<Value>,
    pub body: Body,
}

/// Incremental output while native streams.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    Text(Option<u64>, String),
    Thinking(Option<u64>, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeCall {
    pub id: String,
    /// The host tool name (`rlm`).
    pub name: String,
    pub input: Value,
}

/// The completed native turn.
#[derive(Clone, Debug)]
pub struct NativeResponse {
    /// Native assistant messages as they replay (the carrier), tool names in the offered form.
    pub assistants: Vec<Value>,
    pub blocks: Vec<Value>,
    pub calls: Vec<NativeCall>,
    pub refusal: Option<String>,
    pub finish: Finish,
    pub usage: Value,
    pub cost: Value,
    pub admission: Value,
    /// Text the final message holds beyond what streamed.
    pub tail: String,
    pub warnings: Vec<String>,
}

/// Cancels one request from any thread: the relay first, then native's whole process group.
#[derive(Default)]
pub struct Handle {
    cancelled: AtomicBool,
    child: Mutex<Option<signals::child::Watched>>,
    admission: Mutex<Option<AbortHandle>>,
    /// The request's last `rate_limit_event` (the footer's subscription usage).
    rate_limit_event: Mutex<Option<Value>>,
}

impl Handle {
    pub fn new() -> Arc<Handle> {
        Arc::new(Handle::default())
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(admission) = jsrt::sync::lock(&self.admission).as_ref() {
            admission.abort();
        }
        self.kill();
    }

    /// SIGKILL native's process group while its leader is not reaped (a shim's node child dies with it).
    fn kill(&self) {
        if let Some(child) = self.watched() {
            child.stop(signals::child::Stop {
                group: true,
                term_grace: None,
            });
        }
    }

    fn watched(&self) -> Option<signals::child::Watched> {
        jsrt::sync::lock(&self.child).clone()
    }

    /// The request's last `rate_limit_event`, whether the call succeeded or not.
    pub fn rate_limit_event(&self) -> Option<Value> {
        jsrt::sync::lock(&self.rate_limit_event).clone()
    }
}

enum Event {
    Line(Value),
    Invalid(String),
    Eof,
}

fn remove_tree(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

/// Removes the per-request directory and stops the relay and native, whatever happened.
struct Cleanup<'a> {
    handle: &'a Handle,
    dir: Option<PathBuf>,
    admission: Option<Admission>,
    reader: Option<std::thread::JoinHandle<()>>,
    err_reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        self.handle.cancel();
        if let Some(mut admission) = self.admission.take() {
            admission.close();
        }
        if let Some(child) = self.handle.watched() {
            child.wait_timeout(Duration::from_secs(5));
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(err_reader) = self.err_reader.take() {
            let _ = err_reader.join();
        }
        if let Some(dir) = self.dir.take() {
            remove_tree(&dir);
        }
    }
}

fn stderr_tail(lines: &Mutex<VecDeque<String>>) -> String {
    let guard = jsrt::sync::lock(lines);
    let joined: String = guard
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if joined.is_empty() {
        String::new()
    } else {
        let clipped: String = joined.chars().take(400).collect();
        format!(" (native stderr: {clipped})")
    }
}

fn text_of(blocks: &[Value], kind: &str, field: &str) -> String {
    blocks
        .iter()
        .filter(|b| b["type"] == kind)
        .filter_map(|b| b[field].as_str())
        .collect()
}

fn python_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

struct ConcurrencyPool {
    held: Mutex<usize>,
    ready: Condvar,
}

static CONCURRENCY_POOL: LazyLock<ConcurrencyPool> = LazyLock::new(|| ConcurrencyPool {
    held: Mutex::new(0),
    ready: Condvar::new(),
});

struct SlotGuard;

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let mut held = jsrt::sync::lock(&CONCURRENCY_POOL.held);
        *held = held.saturating_sub(1);
        CONCURRENCY_POOL.ready.notify_all();
    }
}

fn max_concurrent(env: &Env) -> usize {
    env.get(CONCURRENCY_ENV)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_CONCURRENCY)
}

/// Wait for a free `claude -p` concurrency slot, respecting cancellation and timeout.
fn acquire_slot(limit: usize, handle: &Handle, timeout: Duration) -> Result<SlotGuard, ClaudeError> {
    let deadline = Instant::now() + timeout;
    let mut held = jsrt::sync::lock(&CONCURRENCY_POOL.held);
    loop {
        if handle.cancelled() {
            return Err(ClaudeError::Cancelled);
        }
        if *held < limit {
            *held += 1;
            return Ok(SlotGuard);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClaudeError::Timeout);
        }
        let (guard, _) = CONCURRENCY_POOL
            .ready
            .wait_timeout(held, remaining.min(Duration::from_millis(100)))
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held = guard;
    }
}

struct PreparedLaunch {
    env: Env,
    argv: Vec<String>,
    warnings: Vec<String>,
}

struct StreamOutcome {
    assistants: Vec<Value>,
    results: Vec<Value>,
    stopped: bool,
    emitted: String,
    native_error: Option<(String, Option<String>)>,
    signals: super::limit::Signals,
    deadline: Instant,
}

struct SpawnedNative {
    stdin: std::process::ChildStdin,
    events: Receiver<Event>,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

fn write_request_file(root: &Path, name: &str, text: &str) -> Result<PathBuf, ClaudeError> {
    let path = root.join(name);
    std::fs::write(&path, text).map_err(|e| ClaudeError::Failed(e.to_string()))?;
    Ok(path)
}

fn build_mcp_config(config: &Config, manifest: &[Value], root: &Path) -> Result<Value, ClaudeError> {
    if manifest.is_empty() {
        return Ok(json!({"mcpServers": {}}));
    }
    let manifest_path = write_request_file(root, "tools.json", &Value::Array(manifest.to_vec()).to_string())?;
    let mut inert = match &config.inert_mcp {
        Some(command) => command.clone(),
        None => {
            let exe = config::paths::own_executable().map_err(|e| ClaudeError::Failed(e.to_string()))?;
            vec![
                exe.to_string_lossy().into_owned(),
                "__internal".into(),
                "inert-mcp".into(),
            ]
        }
    };
    inert.push(manifest_path.to_string_lossy().into_owned());
    Ok(json!({"mcpServers": {"ultron": {"command": inert[0], "args": inert[1..]}}}))
}

fn configure_child_env(config: &Config, env: &mut Env) {
    with_login_identity(env);
    reap_orphaned_refresh_lock(env);
    for key in crate::claude_code_cli::PARENT_SESSION_ENV
        .iter()
        .chain(&REQUEST_KNOB_ENV)
    {
        env.remove(*key);
    }
    for (k, v) in [
        ("ENABLE_TOOL_SEARCH", "false"),
        ("CLAUDE_CODE_MAX_RETRIES", "0"),
        ("DISABLE_AUTO_COMPACT", "1"),
        ("DISABLE_COMPACT", "1"),
        ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off"),
        ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
    ]
    .iter()
    .chain(CALLER_IDENTITY)
    {
        env.insert((*k).into(), (*v).into());
    }
    let telemetry = telemetry_enabled(config, env);
    apply_traffic_policy(env, telemetry);
}

fn build_launch_argv(
    mut resolved: Vec<String>,
    model: &str,
    system: &Path,
    settings: &Path,
    session_id: &str,
    mcp: &Value,
    effort: Option<&str>,
) -> Vec<String> {
    resolved.extend(
        [
            "-p",
            "--model",
            model,
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--tools",
            "",
            "--system-prompt-file",
            &system.to_string_lossy(),
            "--settings",
            &settings.to_string_lossy(),
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--max-turns",
            "1",
            "--permission-mode",
            "dontAsk",
            "--no-session-persistence",
            "--session-id",
            session_id,
            "--mcp-config",
            &mcp.to_string(),
        ]
        .map(str::to_string),
    );
    if let Some(effort) = effort {
        resolved.extend(["--effort".to_string(), effort.to_string()]);
    }
    resolved
}

fn prepare_launch(
    config: &Config,
    mut env: Env,
    prepared: &Prepared,
    resolved: Vec<String>,
    model: &str,
    root: &Path,
    cleanup: &mut Cleanup<'_>,
) -> Result<PreparedLaunch, ClaudeError> {
    let mcp = build_mcp_config(config, &prepared.body.manifest, root)?;
    configure_child_env(config, &mut env);
    let queried = prepared
        .frames
        .last()
        .and_then(|f| f["message"]["content"].as_array())
        .cloned();
    let admission =
        Admission::new(&config.upstream, config.timeout, queried, config.idle, &env).map_err(ClaudeError::Invalid)?;
    env.insert("ANTHROPIC_BASE_URL".into(), admission.url.clone());
    let warnings = admission.warnings.clone();
    *jsrt::sync::lock(&cleanup.handle.admission) = Some(admission.abort_handle());
    cleanup.admission = Some(admission);
    let settings = write_request_file(
        root,
        "settings.json",
        &json!({"env": {"CLAUDE_CODE_EXTRA_BODY": prepared.body.encoded}}).to_string(),
    )?;
    let system = write_request_file(root, "system.md", &prepared.system)?;
    let parsed = prepared.body.value();
    if let Some(max) = parsed.get("max_tokens") {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), max.to_string());
    }
    let session_id = conversation_session_id(&prepared.system, &prepared.frames);
    let argv = build_launch_argv(
        resolved,
        model,
        &system,
        &settings,
        &session_id,
        &mcp,
        parsed["output_config"]["effort"].as_str(),
    );
    Ok(PreparedLaunch { env, argv, warnings })
}

fn spawn_stderr_reader(
    stderr: std::process::ChildStderr,
    err_sink: Arc<Mutex<VecDeque<String>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let mut guard = jsrt::sync::lock(&err_sink);
            if guard.len() >= 40 {
                guard.pop_front();
            }
            guard.push_back(line);
        }
    })
}

fn spawn_stdout_reader(
    stdout: std::process::ChildStdout,
    sender: std::sync::mpsc::Sender<Event>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut preamble: Option<String> = None;
        let mut started = false;
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let event = match serde_json::from_str::<Value>(&line) {
                Ok(value) => {
                    started = true;
                    Event::Line(value)
                }
                Err(_) if !started => {
                    preamble.get_or_insert(line);
                    continue;
                }
                Err(_) => Event::Invalid(line.chars().take(300).collect()),
            };
            if sender.send(event).is_err() {
                return;
            }
        }
        if let Some(line) = preamble.filter(|_| !started) {
            let _ = sender.send(Event::Invalid(line.chars().take(300).collect()));
        }
        let _ = sender.send(Event::Eof);
    })
}

fn spawn_native(
    argv: &[String],
    env: &Env,
    cwd: &Path,
    handle: &Arc<Handle>,
    cleanup: &mut Cleanup<'_>,
) -> Result<SpawnedNative, ClaudeError> {
    use std::os::unix::process::CommandExt;
    let (sender, events) = std::sync::mpsc::channel::<Event>();
    let native_stderr: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::with_capacity(40)));
    let mut child_slot = jsrt::sync::lock(&handle.child);
    if handle.cancelled() {
        return Err(ClaudeError::Cancelled);
    }
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| ClaudeError::Failed(format!("Could not start Claude Code ({}): {e}", argv[0])))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");
    let stdin = child.stdin.take().expect("stdin is piped");
    *child_slot = Some(signals::child::Watched::new(child));
    cleanup.err_reader = Some(spawn_stderr_reader(stderr, native_stderr.clone()));
    cleanup.reader = Some(spawn_stdout_reader(stdout, sender));
    Ok(SpawnedNative {
        stdin,
        events,
        stderr: native_stderr,
    })
}

fn receive_event(
    events: &Receiver<Event>,
    handle: &Handle,
    timeout: Duration,
    deadline: &mut Instant,
) -> Result<Option<Value>, ClaudeError> {
    loop {
        if handle.cancelled() {
            return Err(ClaudeError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClaudeError::Timeout);
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) | Ok(Event::Eof) => return Ok(None),
            Ok(Event::Invalid(line)) => {
                return Err(ClaudeError::Failed(format!(
                    "Invalid native stream-json output: '{line}'"
                )));
            }
            Ok(Event::Line(value)) => {
                *deadline = Instant::now() + timeout;
                return Ok(Some(value));
            }
        }
    }
}

fn wait_for_replay_ack(
    events: &Receiver<Event>,
    handle: &Handle,
    timeout: Duration,
    deadline: &mut Instant,
) -> Result<(), ClaudeError> {
    loop {
        let Some(ack) = receive_event(events, handle, timeout, deadline)? else {
            return Err(ClaudeError::Failed("Native exited before replay acknowledgment".into()));
        };
        if ack["type"] == "result" {
            if ack["num_turns"] != json!(0) || jsrt::value::truthy(ack.get("is_error")) {
                return Err(ClaudeError::Failed(
                    "Native history replay not supported: expected zero-turn acknowledgment".into(),
                ));
            }
            return Ok(());
        }
    }
}

fn replay_frames(
    mut stdin: std::process::ChildStdin,
    events: &Receiver<Event>,
    frames: &[Value],
    handle: &Handle,
    timeout: Duration,
    deadline: &mut Instant,
) -> Result<(), ClaudeError> {
    let last = frames.len().saturating_sub(1);
    for (index, frame) in frames.iter().enumerate() {
        let mut frame = frame.clone();
        let parked = frame["type"] == "user" && index < last;
        if parked {
            frame["shouldQuery"] = json!(false);
        }
        let written = writeln!(stdin, "{frame}").and_then(|()| stdin.flush());
        if written.is_err() && !parked {
            break;
        }
        if parked {
            wait_for_replay_ack(events, handle, timeout, deadline)?;
        }
    }
    Ok(())
}

fn record_assistant_event(outcome: &mut StreamOutcome, event: &Value) {
    let failed = jsrt::value::truthy(event.get("error")) || jsrt::value::truthy(event["message"].get("error"));
    if failed {
        let detail = event["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        outcome.native_error = Some((detail, event["error"].as_str().map(str::to_string)));
    } else {
        outcome.assistants.push(event["message"].clone());
    }
}

fn record_stream_event(outcome: &mut StreamOutcome, event: &Value, on_delta: &mut dyn FnMut(Delta)) {
    let native = &event["event"];
    if native["type"] == "message_stop" {
        outcome.stopped = true;
    }
    let index = native["index"].as_u64();
    let delta = &native["delta"];
    match delta["type"].as_str() {
        Some("text_delta") => {
            let text = delta["text"].as_str().unwrap_or_default().to_string();
            outcome.emitted.push_str(&text);
            on_delta(Delta::Text(index, text));
        }
        Some("thinking_delta") => {
            on_delta(Delta::Thinking(
                index,
                delta["thinking"].as_str().unwrap_or_default().to_string(),
            ));
        }
        _ => {}
    }
}

fn replay_and_collect(
    stdin: std::process::ChildStdin,
    events: &Receiver<Event>,
    prepared: &Prepared,
    handle: &Handle,
    timeout: Duration,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<StreamOutcome, ClaudeError> {
    let mut deadline = Instant::now() + timeout;
    replay_frames(stdin, events, &prepared.frames, handle, timeout, &mut deadline)?;
    let mut outcome = StreamOutcome {
        assistants: Vec::new(),
        results: Vec::new(),
        stopped: false,
        emitted: String::new(),
        native_error: None,
        signals: super::limit::Signals::default(),
        deadline,
    };
    while let Some(event) = receive_event(events, handle, timeout, &mut outcome.deadline)? {
        match event["type"].as_str() {
            Some("rate_limit_event") => {
                if let Some(info) = event.get("rate_limit_info").filter(|i| i.is_object()) {
                    outcome.signals.event = Some(info.clone());
                    *jsrt::sync::lock(&handle.rate_limit_event) = Some(event);
                }
            }
            Some("assistant") => record_assistant_event(&mut outcome, &event),
            Some("result") => outcome.results.push(event),
            Some("stream_event") => record_stream_event(&mut outcome, &event, on_delta),
            _ => {}
        }
    }
    Ok(outcome)
}

fn extract_calls(assistants: &mut [Value], names: &std::collections::BTreeSet<String>) -> Vec<NativeCall> {
    let mut calls = Vec::new();
    for assistant in assistants {
        for block in assistant["content"].as_array_mut().into_iter().flatten() {
            if block["type"] != "tool_use" {
                continue;
            }
            let name = block["name"].as_str().unwrap_or_default().to_string();
            if names.contains(name.as_str()) {
                block["name"] = json!(format!("{PREFIX}{name}"));
            } else if let Some(rest) = name.strip_prefix("mcp__")
                && !name.starts_with(PREFIX)
                && names.contains(rest)
            {
                block["name"] = json!(format!("{PREFIX}{rest}"));
            }
            let host = block["name"].as_str().unwrap_or_default();
            calls.push(NativeCall {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: host.strip_prefix(PREFIX).unwrap_or(host).to_string(),
                input: block.get("input").cloned().unwrap_or(json!({})),
            });
        }
    }
    calls
}

fn prepare_run_env(mut env: Env) -> Result<Env, ClaudeError> {
    refuse_overrides(&env)?;
    for key in AUTH_OVERRIDES.iter().chain(BACKEND_OVERRIDES).chain(API_CREDENTIALS) {
        env.remove(*key);
    }
    Ok(env)
}

fn wait_for_native_exit(
    handle: &Handle,
    deadline: Instant,
    cleanup: &mut Cleanup<'_>,
) -> Result<Option<i32>, ClaudeError> {
    let child = handle.watched().expect("native was spawned");
    let ended = child.wait_timeout(deadline.saturating_duration_since(Instant::now()));
    if !ended {
        return Err(if handle.cancelled() {
            ClaudeError::Cancelled
        } else {
            ClaudeError::Timeout
        });
    }
    let code = child.status().and_then(|s| s.code());
    if let Some(reader) = cleanup.reader.take() {
        let _ = reader.join();
    }
    if let Some(err_reader) = cleanup.err_reader.take() {
        let _ = err_reader.join();
    }
    if handle.cancelled() {
        return Err(ClaudeError::Cancelled);
    }
    Ok(code)
}

struct RunParams {
    env: Env,
    resolved: Vec<String>,
    model: String,
}

fn validate_run_params(config: &Config, env: Env, prepared: &Prepared) -> Result<RunParams, ClaudeError> {
    if config.timeout.is_zero() {
        return Err(ClaudeError::Invalid("timeout must be positive seconds".into()));
    }
    let env = prepare_run_env(env)?;
    let resolved =
        resolve_claude(config.command.as_deref(), &env).ok_or_else(|| ClaudeError::Missing(INSTALL_HINT.into()))?;
    let model = native_model(&prepared.model).map_err(ClaudeError::Invalid)?;
    Ok(RunParams { env, resolved, model })
}

/// Run one model call against native (blocking). `upstream` is where the admitted request goes.
pub fn run(
    config: &Config,
    env: Env,
    prepared: &Prepared,
    handle: &Arc<Handle>,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<NativeResponse, ClaudeError> {
    let params = validate_run_params(config, env, prepared)?;
    let timeout = config.timeout;
    let _slot = acquire_slot(max_concurrent(&params.env), handle, timeout)?;
    let temp_root = config.temp_root.clone().unwrap_or_else(std::env::temp_dir);
    let mut cleanup = Cleanup {
        handle,
        dir: None,
        admission: None,
        reader: None,
        err_reader: None,
    };
    let root = private_temp(&temp_root, "ultron-claude-").map_err(|e| ClaudeError::Failed(e.to_string()))?;
    cleanup.dir = Some(root.clone());
    let launch = prepare_launch(
        config,
        params.env,
        prepared,
        params.resolved,
        &params.model,
        &root,
        &mut cleanup,
    )?;
    let cwd = workdir(&temp_root).map_err(|e| ClaudeError::Failed(e.to_string()))?;
    let spawned = spawn_native(&launch.argv, &launch.env, &cwd, handle, &mut cleanup)?;
    let stream = replay_and_collect(spawned.stdin, &spawned.events, prepared, handle, timeout, on_delta)?;
    let code = wait_for_native_exit(handle, stream.deadline, &mut cleanup)?;
    let err_tail = stderr_tail(&spawned.stderr);
    let admission = cleanup.admission.as_mut().expect("the relay is open");
    admission.close();
    let outcome = admission.outcome();
    finalize::finalize_response(stream, &outcome, prepared, code, &err_tail, launch.warnings)
}
