//! Instances (user requirement, a deliberate change from TypeScript, which shares one server directory): a plain
//! `ultron` in a directory uses its PROJECT instance, `~/.ultron/projects/<name>-<12 hex of sha256(root)>/`, where
//! the root is the git top level of the cwd, else the canonical cwd; `ultron durable` uses the global DURABLE
//! instance `~/.ultron/server`. Each instance has its own server profile and sockets, workers and session store;
//! `ULTRON_SERVER_DIR` names an instance directory directly. Auth, settings, skills, themes and models stay shared.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

use config::paths::{ENV_SERVER_DIR, LEGACY_ENV_SERVER_DIR};
/// `sockaddr_un.sun_path` holds 108 bytes including the terminator.
const MAX_SOCKET_PATH: usize = 107;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A project's own instance.
    Project { root: PathBuf },
    /// The global durable instance.
    Durable,
    /// `ULTRON_SERVER_DIR`.
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub kind: Kind,
    pub dir: PathBuf,
}

pub fn ultron_home(home: &Path) -> PathBuf {
    home.join(".ultron")
}

/// The project root of `cwd`: `git rev-parse --show-toplevel`, else the canonical cwd.
pub fn project_root(cwd: &Path) -> PathBuf {
    let git = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(output) = git
        && output.status.success()
    {
        let top = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !top.is_empty() {
            return std::fs::canonicalize(&top).unwrap_or_else(|_| PathBuf::from(top));
        }
    }
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())
}

fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() { "root".into() } else { cleaned }
}

/// `~/.ultron/projects/<sanitized basename>-<first 12 hex of sha256(root)>`.
pub fn project_dir(home: &Path, root: &Path) -> PathBuf {
    let hex = &jsrt::hash::sha256_hex(root.as_os_str().as_encoded_bytes())[..12];
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ultron_home(home)
        .join("projects")
        .join(format!("{}-{hex}", sanitize(&name)))
}

/// Which instance a command uses: `ULTRON_SERVER_DIR` (or `PI_SERVER_DIR`) wins, then `durable`, then the project.
pub fn resolve(var: &dyn Fn(&str) -> Option<String>, home: &Path, cwd: &Path, durable: bool) -> Instance {
    if let Some(dir) = var(ENV_SERVER_DIR)
        .or_else(|| var(LEGACY_ENV_SERVER_DIR))
        .filter(|d| !d.is_empty())
    {
        return Instance {
            kind: Kind::Explicit,
            dir: config::paths::normalize(&cwd.join(dir)),
        };
    }
    if durable {
        return Instance {
            kind: Kind::Durable,
            dir: ultron_home(home).join("server"),
        };
    }
    let root = project_root(cwd);
    Instance {
        dir: project_dir(home, &root),
        kind: Kind::Project { root },
    }
}

impl Instance {
    /// The session store when neither `--session-dir` nor `ULTRON_CODING_AGENT_SESSION_DIR` is given: a project
    /// keeps its own; the durable and explicit instances use `<agent dir>/experimental/sessions` as TypeScript does.
    pub fn default_session_dir(&self, agent_dir: &Path) -> PathBuf {
        match self.kind {
            Kind::Project { .. } => self.dir.join("sessions"),
            _ => config::paths::AgentPaths::new(agent_dir).experimental_sessions_dir(),
        }
    }

    pub fn root(&self) -> Option<&Path> {
        match &self.kind {
            Kind::Project { root } => Some(root),
            _ => None,
        }
    }

    /// Create the directory (owner-only) and, for a project, record its root in `project.json`.
    pub fn prepare(&self) -> std::io::Result<()> {
        crate::paths::ensure_private_directory(&self.dir)?;
        if let Some(root) = self.root() {
            let record = json!({"root": root.display().to_string()});
            std::fs::write(self.dir.join("project.json"), format!("{record}\n"))?;
        }
        Ok(())
    }
}

/// Where a server directory's sockets live: the directory itself, unless its longest socket path would not fit in
/// a Unix socket address; then a private runtime directory named after it.
pub fn socket_dir(server_dir: &Path) -> PathBuf {
    if server_dir.as_os_str().len() + 1 + protocol::sockets::LONGEST_NAME <= MAX_SOCKET_PATH {
        return server_dir.to_path_buf();
    }
    let hex = &jsrt::hash::sha256_hex(server_dir.as_os_str().as_encoded_bytes())[..12];
    std::env::temp_dir()
        .join(format!("ultron-{}", rustix::process::getuid().as_raw()))
        .join(hex)
}

pub fn socket_path(server_dir: &Path, server_id: &str) -> PathBuf {
    socket_dir(server_dir).join(protocol::sockets::endpoint_name(server_id))
}

/// A running server generation as its record describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Running {
    pub dir: PathBuf,
    pub kind: &'static str,
    pub root: Option<String>,
    pub pid: u32,
    pub server_id: String,
    pub sessions: usize,
}

fn read_records(dir: &Path, kind: &'static str, out: &mut Vec<Running>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    names.sort();
    for path in names {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !(name.starts_with("generation-") && name.ends_with(".json")) {
            continue;
        }
        let Some(record) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            continue;
        };
        let pid = record["pid"].as_u64().unwrap_or(0) as u32;
        if !signals::pid_alive(pid) {
            continue;
        }
        let session_dir = PathBuf::from(record["sessionDir"].as_str().unwrap_or_default());
        out.push(Running {
            dir: dir.to_path_buf(),
            kind,
            root: record["root"].as_str().map(str::to_string),
            pid,
            server_id: record["serverId"].as_str().unwrap_or_default().to_string(),
            sessions: crate::catalog::list(&session_dir).len(),
        });
    }
}

/// The running server generations recorded in the instance directory `dir`.
pub fn generations(dir: &Path) -> Vec<Running> {
    let mut out = Vec::new();
    read_records(dir, "instance", &mut out);
    out
}

/// `ultron instances`: running server generations of the durable instance and of every project instance.
pub fn running(home: &Path, explicit: Option<&Path>) -> Vec<Running> {
    let mut out = Vec::new();
    if let Some(dir) = explicit {
        read_records(dir, "explicit", &mut out);
    }
    read_records(&ultron_home(home).join("server"), "durable", &mut out);
    let mut projects: Vec<PathBuf> = std::fs::read_dir(ultron_home(home).join("projects"))
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    projects.sort();
    for dir in projects {
        read_records(&dir, "project", &mut out);
    }
    out
}

/// One line per running instance: `kind<TAB>root<TAB>pid<TAB>sessions<TAB>directory`.
pub fn render(running: &[Running]) -> String {
    let mut out = String::new();
    for r in running {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            r.kind,
            r.root.as_deref().unwrap_or("-"),
            r.pid,
            r.sessions,
            r.dir.display()
        ));
    }
    out
}
