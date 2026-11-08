//! Config value resolution for MCP server `env`, `headers` and `oauth.clientSecret` (`core/resolve-config-value.ts`,
//! the uncached `OrThrow` path the MCP runtime uses), and `~` expansion of stdio paths (`runtime.ts` `expandHome`).

use config::auth::TemplatePart;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Environment lookup: `env` first, then the process environment; empty counts as unset.
fn env_value(name: &str, env: &HashMap<String, String>) -> Option<String> {
    env.get(name)
        .filter(|v| !v.is_empty())
        .cloned()
        .or_else(|| std::env::var(name).ok().filter(|v| !v.is_empty()))
}

/// Runs `command` with `/bin/sh -c` and `env` added to the environment, stdin and stderr ignored, for at most 10 s. Trimmed stdout, or None on failure
/// or empty output.
fn run_command(command: &str, env: &HashMap<String, String>) -> Option<String> {
    let bound = signals::child::Bound {
        timeout: Some(COMMAND_TIMEOUT),
        ..Default::default()
    };
    let out = signals::child::run(
        Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .envs(env)
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

/// `resolveConfigValueUncached`: `!cmd` runs a shell command, anything else is a template. `env` overlays the
/// process environment.
pub fn resolve_config_value(config: &str, env: &HashMap<String, String>) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command, env);
    }
    let mut out = String::new();
    for part in config::auth::parse_template(config) {
        match part {
            TemplatePart::Literal(text) => out.push_str(&text),
            TemplatePart::Env(name) => out.push_str(&env_value(&name, env)?),
        }
    }
    Some(out)
}

/// `resolveConfigValueOrThrow`, with its error messages.
pub fn resolve_config_value_or_err(
    config: &str,
    description: &str,
    env: &HashMap<String, String>,
) -> Result<String, String> {
    if let Some(value) = resolve_config_value(config, env) {
        return Ok(value);
    }
    if let Some(command) = config.strip_prefix('!') {
        return Err(format!("Failed to resolve {description} from shell command: {command}"));
    }
    let mut missing: Vec<String> = Vec::new();
    for part in config::auth::parse_template(config) {
        if let TemplatePart::Env(name) = part
            && !missing.contains(&name)
            && env_value(&name, env).is_none()
        {
            missing.push(name);
        }
    }
    Err(match missing.len() {
        0 => format!("Failed to resolve {description}"),
        1 => format!(
            "Failed to resolve {description} from environment variable: {}",
            missing[0]
        ),
        _ => format!(
            "Failed to resolve {description} from environment variables: {}",
            missing.join(", ")
        ),
    })
}

/// `resolveHeadersOrThrow`: every header resolved, None when there are none.
pub fn resolve_headers_or_err(
    headers: &[(String, String)],
    description: &str,
    env: &HashMap<String, String>,
) -> Result<Option<Vec<(String, String)>>, String> {
    let mut out = Vec::new();
    for (key, value) in headers {
        out.push((
            key.clone(),
            resolve_config_value_or_err(value, &format!("{description} header \"{key}\""), env)?,
        ));
    }
    Ok((!out.is_empty()).then_some(out))
}

/// `~` and `~/…` name `home`, like in a shell.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}
