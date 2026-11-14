//! Language server diagnostics for every REPL file write. A proposed `edit()`/`write()` of a source file goes to
//! the language server of its project as the file's new text, and the errors the server reports come back as a
//! note on that write; the write itself is never refused. Servers start on first use, from the session's PATH or,
//! when missing there, through `mise exec` (which installs them), and are shared by every lane of the session.

pub mod client;
pub mod servers;

use client::{Client, Diagnostic, Unready};
use host::file_hooks::{FileWriteGuard, Verdict, inside};
use jsrt::sync;
use serde_json::{Value, json};
use servers::Spec;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// `off` (or `0`, `false`, `no`, `none`) turns the diagnostics off; overrides the `lsp.enabled` setting.
pub const LSP_ENV: &str = "ULTRON_LSP";
const OFF_VALUES: [&str; 5] = ["off", "0", "false", "no", "none"];

/// How long one write waits for its diagnostics: under the 5 s a guard gets before the write proceeds unchecked.
pub const BUDGET: Duration = Duration::from_secs(4);
/// A server on PATH that has not answered `initialize` by then is not one (a shim without its tool).
const START_LIMIT: Duration = Duration::from_secs(15);
/// `mise exec` may download and build the server first.
const INSTALL_LIMIT: Duration = Duration::from_secs(600);
/// ponytail: the least recently used server is stopped for a fifth one, and none stops for being idle; a timer if
/// an idle session's servers cost too much memory.
const MAX_LIVE: usize = 4;
const MAX_SHOWN: usize = 10;
const MAX_MESSAGE_CHARS: usize = 240;

/// Whether sessions run language servers: `ULTRON_LSP` when set, else the `lsp.enabled` setting (default true).
/// Never with a sandbox: the servers read the project and run its toolchain outside it.
pub fn enabled(env: Option<&str>, settings: &Value) -> bool {
    if sandbox::Settings::from_settings(settings).mode != sandbox::settings::Mode::Off {
        return false;
    }
    match env.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if !value.is_empty() => !OFF_VALUES.contains(&value.as_str()),
        _ => settings.pointer("/lsp/enabled") != Some(&Value::Bool(false)),
    }
}

/// What the language server of a file's project says about a proposed text of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Report {
    /// No language server checks this file.
    Unwatched,
    Errors {
        server: &'static str,
        errors: Vec<Diagnostic>,
    },
    /// The server is starting, being installed, or loading the project.
    Starting {
        server: &'static str,
        root: PathBuf,
    },
    /// No answer in time.
    Unanswered {
        server: &'static str,
    },
    Failed {
        server: &'static str,
        reason: String,
    },
}

type Key = (&'static str, PathBuf);

struct Candidate {
    argv: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    limit: Duration,
}

enum Slot {
    Starting,
    /// With `Inner::uses` at its last check.
    Ready(Arc<Client>, u64),
    Failed(String),
}

#[derive(Default)]
struct Slots {
    by_key: HashMap<Key, Slot>,
    uses: u64,
    /// Servers that exited once and were started again.
    restarted: HashSet<Key>,
    /// One-time notes already shown.
    told: HashSet<Key>,
}

struct Inner {
    /// The session's environment: the servers' whole environment, and where they are looked up.
    env: Vec<(String, String)>,
    slots: Mutex<Slots>,
    changed: Condvar,
}

/// The session's language servers, as a write guard that only ever adds notes.
#[derive(Clone)]
pub struct LspGuard {
    inner: Arc<Inner>,
}

impl Inner {
    fn var(&self, name: &str) -> Option<String> {
        self.env
            .iter()
            .rev()
            .find(|(known, _)| known == name)
            .map(|(_, value)| value.clone())
    }

    /// The commands that may run `spec`'s server in `root`, best first, each with its environment, the directory
    /// it starts in and its start limit.
    fn candidates(&self, spec: &Spec, root: &Path) -> Vec<Candidate> {
        let path = self.var("PATH").unwrap_or_default();
        let on_path = |program: &str| {
            let found = sandbox::launcher::resolve_program(program, &path);
            (found != Path::new(program)).then(|| found.to_string_lossy().into_owned())
        };
        let args = spec.args.iter().map(|arg| arg.to_string());
        let mut out = Vec::new();
        if let Some(server) = on_path(spec.command) {
            out.push(Candidate {
                argv: std::iter::once(server).chain(args.clone()).collect(),
                env: self.env.clone(),
                cwd: root.to_path_buf(),
                limit: START_LIMIT,
            });
        }
        if let Some(mise) = on_path("mise") {
            // mise starts in `/`, and the server it runs changes into the project. Started in the project,
            // `mise exec` marks the project's own mise.toml as trusted and runs the commands in it, unasked.
            let enter = ["--", "/bin/sh", "-c", r#"cd "$1" && shift && exec "$@""#, "sh"].map(String::from);
            let argv = [mise, "exec".into()]
                .into_iter()
                .chain(spec.mise.iter().map(|tool| tool.to_string()))
                .chain(enter)
                .chain([root.to_string_lossy().into_owned(), spec.command.to_string()])
                .chain(args)
                .collect();
            let mut env = self.env.clone();
            // Install what is missing, whatever the user's mise settings say.
            env.push(("MISE_EXEC_AUTO_INSTALL".into(), "1".into()));
            out.push(Candidate {
                argv,
                env,
                cwd: PathBuf::from("/"),
                limit: INSTALL_LIMIT,
            });
        }
        out
    }

    fn start(&self, spec: &'static Spec, root: &Path) -> Result<Client, String> {
        let candidates = self.candidates(spec, root);
        if candidates.is_empty() {
            return Err(format!(
                "`{}` is not on PATH, and neither is mise to install it",
                spec.command
            ));
        }
        let settings = servers::settings(spec, root, &|name| self.var(name));
        let init = settings.get(spec.name).cloned().unwrap_or_else(|| json!({}));
        let mut failures = Vec::new();
        for Candidate { argv, env, cwd, limit } in candidates {
            let launch = client::Launch {
                argv: &argv,
                env: &env,
                cwd: &cwd,
                root,
            };
            match Client::start(launch, spec.status, settings.clone(), init.clone(), limit) {
                Ok(client) => return Ok(client),
                Err(error) => failures.push(format!("`{}`: {error}", argv[..argv.len().min(2)].join(" "))),
            }
        }
        Err(failures.join("; "))
    }
}

impl LspGuard {
    /// `env` is the session's environment (`Config::child_env`), never this process's.
    pub fn new(env: Vec<(String, String)>) -> LspGuard {
        LspGuard {
            inner: Arc::new(Inner {
                env,
                slots: Mutex::default(),
                changed: Condvar::new(),
            }),
        }
    }

    fn launch(&self, spec: &'static Spec, key: Key) {
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let slot = match inner.start(spec, &key.1) {
                Ok(client) => Slot::Ready(Arc::new(client), 0),
                Err(reason) => Slot::Failed(reason),
            };
            // Unless it was given up on meanwhile.
            if let Some(known) = sync::lock(&inner.slots).by_key.get_mut(&key) {
                *known = slot;
            }
            inner.changed.notify_all();
        });
    }

    /// What the server of `path`'s project says about `content` as its text, waiting at most `budget`. The first
    /// call for a project starts its server.
    pub fn diagnose(&self, path: &Path, content: &str, budget: Duration) -> Report {
        let Some((spec, language)) = servers::for_file(path) else {
            return Report::Unwatched;
        };
        let Some(root) = servers::root_of(spec, path) else {
            return Report::Unwatched;
        };
        let spec = servers::in_project(spec, &root);
        let server = spec.name;
        let key: Key = (server, root.clone());
        let deadline = Instant::now() + budget;
        let mut slots = sync::lock(&self.inner.slots);
        let first = !slots.by_key.contains_key(&key);
        if first {
            let ready = |slot: &Slot| matches!(slot, Slot::Ready(..));
            if slots.by_key.values().filter(|slot| ready(slot)).count() >= MAX_LIVE {
                let oldest = slots.by_key.iter().filter_map(|(key, slot)| match slot {
                    Slot::Ready(_, used) => Some((*used, key.clone())),
                    _ => None,
                });
                if let Some((_, oldest)) = oldest.min() {
                    slots.by_key.remove(&oldest);
                }
            }
            slots.by_key.insert(key.clone(), Slot::Starting);
            self.launch(spec, key.clone());
            while matches!(slots.by_key.get(&key), Some(Slot::Starting)) {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                slots = self
                    .inner
                    .changed
                    .wait_timeout(slots, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        slots.uses += 1;
        let uses = slots.uses;
        let client = match slots.by_key.get_mut(&key) {
            None | Some(Slot::Starting) => return Report::Starting { server, root },
            Some(Slot::Failed(reason)) => {
                return Report::Failed {
                    server,
                    reason: reason.clone(),
                };
            }
            Some(Slot::Ready(client, used)) => {
                *used = uses;
                client.clone()
            }
        };
        drop(slots);
        // Only the write that started a server waits for it to load the project.
        if !first && client.loading() {
            return Report::Starting { server, root };
        }
        match client.check(path, language, content, deadline) {
            Ok(errors) => Report::Errors {
                server,
                errors: servers::reliable(spec, errors),
            },
            Err(Unready::Loading) => Report::Starting { server, root },
            Err(Unready::Slow) => Report::Unanswered { server },
            Err(Unready::Exited) => {
                let mut slots = sync::lock(&self.inner.slots);
                if slots.restarted.insert(key.clone()) {
                    // The next write starts it again, once.
                    slots.by_key.remove(&key);
                    return Report::Unanswered { server };
                }
                let reason = "it exited twice".to_string();
                slots.by_key.insert(key, Slot::Failed(reason.clone()));
                Report::Failed { server, reason }
            }
        }
    }

    /// The note of a write of `content` to `path`, for the model.
    fn note(&self, path: &Path, content: &str, cwd: &Path) -> Option<String> {
        let shown = |path: &Path| inside(cwd, path).unwrap_or_else(|| path.display().to_string());
        let once = |server: &'static str, root: PathBuf| sync::lock(&self.inner.slots).told.insert((server, root));
        let file = shown(path);
        match self.diagnose(path, content, BUDGET) {
            Report::Unwatched => None,
            Report::Errors { server, errors } => describe(server, &file, &errors),
            Report::Starting { server, root } => once(server, root.clone()).then(|| {
                let place = if root == cwd { ".".to_string() } else { shown(&root) };
                format!("{server} is starting for {place}: errors it finds are shown with later writes.")
            }),
            Report::Unanswered { server } => Some(format!(
                "{server} did not answer within {} s: {file} was not checked for errors.",
                BUDGET.as_secs()
            )),
            // Shown once, under a key no project root has.
            Report::Failed { server, reason } => once(server, PathBuf::new()).then(|| {
                format!("no language server errors for files like {file}: {server} did not start ({reason}).")
            }),
        }
    }
}

/// `server: 2 errors in src/a.rs with this write:` and a line per error (1-based `line:column`).
fn describe(server: &str, file: &str, errors: &[Diagnostic]) -> Option<String> {
    if errors.is_empty() {
        return None;
    }
    let count = match errors.len() {
        1 => "1 error".to_string(),
        n => format!("{n} errors"),
    };
    let mut lines = vec![format!("{server}: {count} in {file} with this write:")];
    for error in errors.iter().take(MAX_SHOWN) {
        let message = error.message.split_whitespace().collect::<Vec<_>>().join(" ");
        let message: String = match message.char_indices().nth(MAX_MESSAGE_CHARS) {
            Some((cut, _)) => format!("{}…", &message[..cut]),
            None => message,
        };
        let code = error.code.as_ref().map(|code| format!("{code} ")).unwrap_or_default();
        lines.push(format!("  {}:{} {code}{message}", error.line + 1, error.column + 1));
    }
    if errors.len() > MAX_SHOWN {
        lines.push(format!("  … and {} more", errors.len() - MAX_SHOWN));
    }
    Some(lines.join("\n"))
}

impl FileWriteGuard for LspGuard {
    fn name(&self) -> &str {
        "LSP"
    }

    fn checks_shell(&self) -> bool {
        false
    }

    fn before_write(&self, path: &Path, content: &str, cwd: &Path, _lane: &str) -> Result<Verdict, String> {
        Ok(Verdict::Allow(self.note(path, content, cwd)))
    }
}
