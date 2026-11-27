//! CLI resolution, capability probing (`--version`, `--help`, `auth status --json`), and empty working directory.

use super::{
    CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV, CLAUDE_CODE_BIN_ENV, Env, PROBE_FAILURE_TTL, PROBE_TIMEOUT, REQUIRED_FLAGS,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

static RESOLVED_CLI: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);
static PROBES: LazyLock<Mutex<HashMap<String, ProbeEntry>>> = LazyLock::new(Mutex::default);
static WORK_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(Mutex::default);

fn is_executable(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `resolveClaudeCli`: `ULTRON_CLAUDE_CODE_BIN`, else the first `claude` on PATH. Only a found CLI is remembered.
pub fn resolve_claude_cli(env: &Env) -> Result<String, String> {
    let configured = env
        .get(CLAUDE_CODE_BIN_ENV)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let key = format!(
        "{}\0{}",
        configured.as_deref().unwrap_or_default(),
        env.get("PATH").map_or_default(String::as_str)
    );
    if let Some(found) = jsrt::sync::lock(&RESOLVED_CLI).get(&key) {
        return Ok(found.clone());
    }
    let found = match &configured {
        Some(path) => is_executable(Path::new(path)).then(|| path.clone()),
        None => env
            .get("PATH")
            .map_or_default(String::as_str)
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join("claude"))
            .find(|candidate| is_executable(candidate))
            .map(|p| p.to_string_lossy().into_owned()),
    };
    match found {
        Some(found) => {
            jsrt::sync::lock(&RESOLVED_CLI).insert(key, found.clone());
            Ok(found)
        }
        None => Err(match configured {
            Some(path) => format!("claude-code provider: {CLAUDE_CODE_BIN_ENV}={path} is not an executable file."),
            None => "claude-code provider: the Claude Code CLI (`claude`) was not found on PATH. Install Claude Code and run `claude auth login`, or set ULTRON_CLAUDE_CODE_BIN.".into(),
        }),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeCliCapabilities {
    pub version: String,
    pub flags: HashSet<String>,
    pub warnings: Vec<String>,
}

struct ProbeEntry {
    result: Result<ClaudeCliCapabilities, String>,
    at: Instant,
}

struct ExecResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn exec_cli(bin: &str, args: &[&str], env: &Env, cwd: &Path) -> Result<ExecResult, String> {
    let bound = signals::child::Bound {
        timeout: Some(PROBE_TIMEOUT),
        ..Default::default()
    };
    let out = signals::child::run(
        Command::new(bin)
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        None,
        bound,
    )
    .map_err(|e| format!("claude-code provider: could not run {bin} {}: {e}", args.join(" ")))?;
    Ok(ExecResult {
        code: out.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn is_flag_start(chars: &[char], i: usize) -> bool {
    let boundary = i == 0 || chars[i - 1].is_whitespace() || chars[i - 1] == ',';
    boundary
        && chars.get(i) == Some(&'-')
        && chars.get(i + 1) == Some(&'-')
        && chars.get(i + 2).is_some_and(char::is_ascii_alphabetic)
}

fn scan_flag_chars(chars: &[char], mut pos: usize) -> usize {
    while chars.get(pos).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-') {
        pos += 1;
    }
    pos
}

fn parse_optional_suffix(chars: &[char], j: usize, flag: &str) -> Option<(String, usize)> {
    if chars.get(j) != Some(&'[') || chars.get(j + 1) != Some(&'-') {
        return None;
    }
    let k = scan_flag_chars(chars, j + 2);
    if k > j + 2 && chars.get(k) == Some(&']') {
        let suffix: String = chars[j + 1..k].iter().collect::<String>().to_lowercase();
        return Some((format!("{flag}{suffix}"), k + 1));
    }
    None
}

fn push_unique(flags: &mut Vec<String>, flag: String) {
    if !flags.contains(&flag) {
        flags.push(flag);
    }
}

/// `parseHelpFlags`: the long flags a help text lists, lower-cased.
pub fn parse_help_flags(help: &str) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let chars: Vec<char> = help.chars().collect();
    let mut i = 0;
    while i + 2 < chars.len() + 1 {
        if !is_flag_start(&chars, i) {
            i += 1;
            continue;
        }
        let mut j = scan_flag_chars(&chars, i + 3);
        let flag: String = chars[i..j].iter().collect::<String>().to_lowercase();
        push_unique(&mut flags, flag.clone());
        if let Some((expanded, next)) = parse_optional_suffix(&chars, j, &flag) {
            push_unique(&mut flags, expanded);
            j = next;
        }
        i = j;
    }
    flags
}

pub(super) fn tail(text: &str, limit: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > limit {
        String::from_utf16_lossy(&units[units.len() - limit..])
    } else {
        text.to_string()
    }
}

fn extract_version(stdout: &str) -> String {
    stdout
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .and_then(|line| line.split_whitespace().next())
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

fn parse_auth_status(auth_run: &ExecResult) -> Result<Value, String> {
    let auth_text = auth_run.stdout.trim();
    let auth = serde_json::from_str::<Value>(auth_text)
        .ok()
        .or_else(|| serde_json::from_str(&auth_text[auth_text.find("\n{")?..]).ok())
        .filter(|v| v.get("loggedIn").is_some_and(Value::is_boolean));
    let Some(auth) = auth else {
        let text = if auth_run.stderr.is_empty() {
            &auth_run.stdout
        } else {
            &auth_run.stderr
        };
        let code = auth_run.code.map_or("null".to_string(), |c| c.to_string());
        return Err(format!(
            "claude-code provider: `claude auth status --json` did not report a login state (exit {code}): {}",
            tail(text, 500)
        ));
    };
    if auth["loggedIn"] != true {
        return Err("claude-code provider: the Claude Code CLI is not logged in. Run `claude auth login` (or `claude` and /login) and try again.".into());
    }
    Ok(auth)
}

fn check_auth_warnings(auth: &Value, env: &Env) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    if let Some(provider) = auth
        .get("apiProvider")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty() && *p != "firstParty")
    {
        let message = format!("the Claude Code CLI is configured for {provider}, not a first-party Anthropic login");
        if env.get(CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV).map(String::as_str) != Some("1") {
            return Err(format!(
                "claude-code provider: {message}. Set {CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV}=1 to use it anyway."
            ));
        }
        warnings.push(message);
    }
    if let Some(method) = auth
        .get("authMethod")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty() && *m != "claude.ai")
    {
        warnings.push(format!(
            "the Claude Code CLI authenticates with {method}, not a claude.ai subscription login; calls may be billed to that account"
        ));
    }
    Ok(warnings)
}

fn probe_cli(bin: &str, env: &Env, cwd: &Path) -> Result<ClaudeCliCapabilities, String> {
    let version_run = exec_cli(bin, &["--version"], env, cwd)?;
    let help_run = exec_cli(bin, &["--help"], env, cwd)?;
    let auth_run = exec_cli(bin, &["auth", "status", "--json"], env, cwd)?;
    let version = extract_version(&version_run.stdout);
    let flags: HashSet<String> = parse_help_flags(&help_run.stdout).into_iter().collect();
    let missing: Vec<&str> = REQUIRED_FLAGS.iter().copied().filter(|f| !flags.contains(*f)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "claude-code provider: the installed Claude Code CLI ({bin}, version {version}) does not support {} (not listed in `claude --help`). Update Claude Code (`claude update`).",
            missing.join(", ")
        ));
    }
    let auth = parse_auth_status(&auth_run)?;
    let warnings = check_auth_warnings(&auth, env)?;
    Ok(ClaudeCliCapabilities {
        version,
        flags,
        warnings,
    })
}

/// `ensureClaudeCli`: probe once per CLI and config dir; a failure is retried after a short delay.
pub async fn ensure_claude_cli(bin: &str, env: &Env, cwd: &Path) -> Result<ClaudeCliCapabilities, String> {
    let key = format!("{bin}\0{}", env.get("CLAUDE_CONFIG_DIR").map_or_default(String::as_str));
    if let Some(entry) = jsrt::sync::lock(&PROBES).get(&key)
        && (entry.result.is_ok() || entry.at.elapsed() < PROBE_FAILURE_TTL)
    {
        return entry.result.clone();
    }
    let (bin_owned, env_owned, cwd_owned) = (bin.to_string(), env.clone(), cwd.to_path_buf());
    let result = tokio::task::spawn_blocking(move || probe_cli(&bin_owned, &env_owned, &cwd_owned))
        .await
        .map_err(|e| e.to_string())?;
    jsrt::sync::lock(&PROBES).insert(
        key,
        ProbeEntry {
            result: result.clone(),
            at: Instant::now(),
        },
    );
    result
}

/// One empty directory per user, so no project CLAUDE.md or settings are near the call.
pub fn empty_work_dir() -> Result<PathBuf, String> {
    let mut guard = jsrt::sync::lock(&WORK_DIR);
    if let Some(dir) = guard.as_ref() {
        return Ok(dir.clone());
    }
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let user = std::fs::metadata("/proc/self").map_or_else(|_| "user".into(), |m| m.uid().to_string());
    let dir = jsrt::fs::temp_dir().join(format!("ultron-claude-code-{user}"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| e.to_string())?;
    *guard = Some(dir.clone());
    Ok(dir)
}

pub(super) fn reset_probe_state() {
    jsrt::sync::lock(&RESOLVED_CLI).clear();
    jsrt::sync::lock(&PROBES).clear();
    *jsrt::sync::lock(&WORK_DIR) = None;
}
