//! Native session discovery and selectors from packages/coding-agent/src/native-command.ts.

use crate::Env;
use serde_json::Value;
use session_journal::repo::Metadata;
use std::path::{Path, PathBuf};

/// `path.resolve(base, input)` without symlink resolution.
fn resolve(base: &Path, input: &str) -> PathBuf {
    config::paths::normalize(&base.join(input))
}

/// `resolveSessionDirectory`: `--session-dir`, else `ULTRON_CODING_AGENT_SESSION_DIR`, else the instance's store
/// (a project instance keeps its own; otherwise `<agent dir>/experimental/sessions`), resolved against the cwd.
pub fn session_directory(session_dir: Option<&str>, env: &Env) -> PathBuf {
    session_directory_of(session_dir, env, &crate::instance(env, false))
}

pub fn session_directory_of(session_dir: Option<&str>, env: &Env, instance: &daemon::instance::Instance) -> PathBuf {
    let dir = session_dir
        .map(str::to_string)
        .or_else(|| (env.var)(config::paths::ENV_SESSION_DIR))
        .unwrap_or_else(|| instance.default_session_dir(&env.agent_dir()).display().to_string());
    resolve(&env.cwd, &dir)
}

/// The first line of a file (without its newline), with Node's error text when it cannot be opened.
pub fn read_first_line(path: &Path) -> Result<String, String> {
    session_journal::catalog::first_line(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, open '{}'", path.display()),
        _ => e.to_string(),
    })
}

/// `sessionIdFromSelector`: a path (contains a separator or ends in `.jsonl`) names a session file whose header id is
/// taken; otherwise an exact id, else a unique id prefix, among `sessions`.
pub fn session_id_from_selector(selector: &str, sessions: &[Metadata], cwd: &Path) -> Result<String, String> {
    if selector.contains('/') || selector.contains('\\') || selector.ends_with(".jsonl") {
        let path = resolve(cwd, selector);
        let line = read_first_line(&path)?;
        let invalid = || format!("Session file is not a valid ultron session: {}", path.display());
        let value: Value = serde_json::from_str(&line).map_err(|_| invalid())?;
        if value.get("kind").and_then(Value::as_str) != Some("header") {
            return Err(invalid());
        }
        return value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(invalid);
    }
    if let Some(exact) = sessions.iter().find(|s| s.id == selector) {
        return Ok(exact.id.clone());
    }
    match sessions
        .iter()
        .filter(|s| s.id.starts_with(selector))
        .collect::<Vec<_>>()[..]
    {
        [only] => Ok(only.id.clone()),
        _ => Err(format!("No native session found matching '{selector}'")),
    }
}
