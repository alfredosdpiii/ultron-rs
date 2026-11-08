//! Agent directory and per-user config paths (`config.ts`), plus Node's lexical path rules they rely on.

use std::path::{Path, PathBuf};

/// Project-local config directory (`.pi`, upstream-compatible).
pub const CONFIG_DIR_NAME: &str = ".pi";
/// User config root under the home directory.
pub const AGENT_CONFIG_DIR_NAME: &str = ".ultron";
/// `${APP_NAME.toUpperCase()}_CODING_AGENT_DIR` for the `ultron` app name.
pub const ENV_AGENT_DIR: &str = "ULTRON_CODING_AGENT_DIR";
/// `${APP_NAME.toUpperCase()}_CODING_AGENT_SESSION_DIR`.
pub const ENV_SESSION_DIR: &str = "ULTRON_CODING_AGENT_SESSION_DIR";
/// An instance directory (`daemon::instance`); `PI_SERVER_DIR` is its older name.
pub const ENV_SERVER_DIR: &str = "ULTRON_SERVER_DIR";
pub const LEGACY_ENV_SERVER_DIR: &str = "PI_SERVER_DIR";

fn text(path: &Path) -> std::borrow::Cow<'_, str> {
    path.to_string_lossy()
}

/// Node's `path.normalize` without the trailing slash (`path.resolve` for an absolute path).
pub fn normalize(path: &Path) -> PathBuf {
    let normalized = jsrt::path::normalize(&text(path));
    match normalized.strip_suffix('/') {
        Some(rest) if !rest.is_empty() => PathBuf::from(rest),
        _ => PathBuf::from(normalized),
    }
}

/// `normalizePath`: `~` and `~/x` expand to the home directory; `file://` URLs become paths.
pub fn expand_tilde(input: &str, home: &Path) -> PathBuf {
    PathBuf::from(jsrt::path::normalize_path(input, &text(home)))
}

/// `resolvePath(input, base)`: tilde expansion, then Node's `path.resolve`.
pub fn resolve(input: &str, base: &Path, home: &Path) -> PathBuf {
    PathBuf::from(jsrt::path::resolve_path(input, &text(base), &text(home)))
}

/// `resolveSessionDirectory`: the session store root is `flag` (`--session-dir`, `--sessions-root`), else
/// `$ULTRON_CODING_AGENT_SESSION_DIR` (`var` looks it up), else `default`, resolved against `cwd` with `~` expanded.
pub fn session_root(
    flag: Option<&str>,
    var: &dyn Fn(&str) -> Option<String>,
    default: &Path,
    cwd: &Path,
    home: &Path,
) -> PathBuf {
    match flag.map(str::to_string).or_else(|| var(ENV_SESSION_DIR)) {
        Some(dir) => resolve(&dir, cwd, home),
        None => resolve(&text(default), cwd, home),
    }
}

/// The user's agent directory: `$ULTRON_CODING_AGENT_DIR` (tilde-expanded) when set and non-empty, else
/// `~/.ultron/agent`. `env` looks up an environment variable.
pub fn agent_dir(env: impl Fn(&str) -> Option<String>, home: &Path) -> PathBuf {
    match env(ENV_AGENT_DIR) {
        Some(dir) if !dir.is_empty() => expand_tilde(&dir, home),
        _ => home.join(AGENT_CONFIG_DIR_NAME).join("agent"),
    }
}

/// `name.replace(/[^A-Za-z0-9._-]/g, "_")`.
pub fn safe_file_name(name: &str) -> impl Iterator<Item = char> + '_ {
    name.chars().map(|c| {
        if c.is_ascii_alphanumeric() || "._-".contains(c) {
            c
        } else {
            '_'
        }
    })
}

/// Files and directories under the agent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPaths {
    pub agent_dir: PathBuf,
}

impl AgentPaths {
    pub fn new(agent_dir: impl Into<PathBuf>) -> Self {
        Self {
            agent_dir: agent_dir.into(),
        }
    }
    fn join(&self, name: &str) -> PathBuf {
        normalize(&self.agent_dir.join(name))
    }
    pub fn custom_themes_dir(&self) -> PathBuf {
        self.join("themes")
    }
    pub fn models(&self) -> PathBuf {
        self.join("models.json")
    }
    pub fn auth(&self) -> PathBuf {
        self.join("auth.json")
    }
    pub fn settings(&self) -> PathBuf {
        self.join("settings.json")
    }
    pub fn bin_dir(&self) -> PathBuf {
        self.join("bin")
    }
    pub fn sessions_dir(&self) -> PathBuf {
        self.join("sessions")
    }
    pub fn debug_log(&self) -> PathBuf {
        self.join("ultron-debug.log")
    }
    pub fn trust(&self) -> PathBuf {
        self.join("trust.json")
    }
    pub fn mcp(&self) -> PathBuf {
        self.join("mcp.json")
    }
    /// The user's skills (`<agent dir>/skills/<name>/SKILL.md`).
    pub fn skills_dir(&self) -> PathBuf {
        self.join("skills")
    }
    /// Procedure skills waiting for the user's approval (`delegation::procedure::proposals`).
    pub fn skill_proposals(&self) -> PathBuf {
        self.join("skill-proposals")
    }
    /// The session store of the durable and explicit instances and of `ultron claude` without
    /// `ULTRON_CODING_AGENT_SESSION_DIR`.
    pub fn experimental_sessions_dir(&self) -> PathBuf {
        self.join("experimental/sessions")
    }
    /// Where `ultron claude` remembers the Ultron session of a Claude Code session, by Claude Code's session id.
    pub fn claude_session_map(&self, claude_session_id: &str) -> PathBuf {
        let safe: String = safe_file_name(claude_session_id).collect();
        self.join("claude-code/sessions").join(format!("{safe}.json"))
    }
}

/// This process's executable for a command another process runs while this one lives (session workers, the inert
/// MCP server of a subscription lane, Claude Code hooks): `current_exe`, or, once that file is gone, the kernel's
/// handle on this process's image (`/proc/<pid>/exe`). An install replaces the binary under a long-lived server:
/// `current_exe` then names `<path> (deleted)`, which no spawn can run (every new durable session failed with
/// ENOENT); the handle still runs this same version.
pub fn own_executable() -> std::io::Result<PathBuf> {
    let image = PathBuf::from(format!("/proc/{}/exe", std::process::id()));
    Ok(running_image(std::env::current_exe()?, image, &|path| path.exists()))
}

/// [`own_executable`] over a probe: `exe` while it exists, else `image` when that exists, else `exe` (the spawn's
/// own error then names it).
pub fn running_image(exe: PathBuf, image: PathBuf, exists: &dyn Fn(&Path) -> bool) -> PathBuf {
    if !exists(&exe) && exists(&image) { image } else { exe }
}

/// `os.tmpdir()` of an environment: `TMPDIR` without its trailing slash, else `/tmp`.
pub fn tmpdir(var: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match var("TMPDIR").filter(|v| !v.is_empty()) {
        Some(dir) if dir.len() > 1 => PathBuf::from(dir.trim_end_matches('/')),
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from("/tmp"),
    }
}

/// `mkdtempSync(join(base, prefix))`: a new directory only this user can enter (0700), `prefix` plus six random hex
/// digits.
pub fn private_temp_dir(base: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    loop {
        let path = base.join(format!("{prefix}{}", jsrt::random::random_hex(3)));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Write `content` to `path`, created or truncated, readable only by the user (0600).
pub fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(content.as_bytes())
}
