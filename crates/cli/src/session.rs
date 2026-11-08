//! Native session discovery and selectors from packages/coding-agent/src/native-command.ts.

use crate::Env;
use session_journal::catalog::HeaderLine;
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

/// Whether a session selector names a session file rather than an id (`sessionIdFromSelector`): it has a path
/// separator or ends in `.jsonl`.
pub fn names_file(selector: &str) -> bool {
    selector.contains('/') || selector.contains('\\') || selector.ends_with(".jsonl")
}

/// The first line of a session file, with Node's error text when it cannot be read.
pub fn read_header(path: &Path) -> Result<HeaderLine, String> {
    HeaderLine::read(path).map_err(|e| jsrt::fs::error_message(&e, "open", path))
}

/// `sessionIdFromSelector`: a path ([`names_file`]) names a session file whose header id is taken; otherwise an
/// exact id, else a unique id prefix, among `sessions`.
pub fn session_id_from_selector(selector: &str, sessions: &[Metadata], cwd: &Path) -> Result<String, String> {
    if names_file(selector) {
        let path = resolve(cwd, selector);
        return read_header(&path)?
            .native()
            .and_then(|header| Some(header.get("id")?.as_str()?.to_string()))
            .ok_or_else(|| format!("Session file is not a valid ultron session: {}", path.display()));
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
