//! Concurrency limiting, `claude` child process management, and warm spare pool.

use super::probe::tail;
use super::{EXIT_GRACE, Env, KILL_GRACE, RuntimeConfig, STDERR_TAIL_CHARS};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write as _};
use std::path::Path;
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

#[derive(Default)]
struct Limiter {
    active: Mutex<u64>,
    notify: Notify,
}

static LIMITER: LazyLock<Arc<Limiter>> = LazyLock::new(Arc::default);

pub(super) struct Permit(Arc<Limiter>);

impl Drop for Permit {
    fn drop(&mut self) {
        *jsrt::sync::lock(&self.0.active) -= 1;
        self.0.notify.notify_one();
    }
}

pub(super) async fn acquire(limit: u64, signal: Option<&crate::http::AbortSignal>) -> Result<Permit, String> {
    let limiter = LIMITER.clone();
    loop {
        let notified = limiter.notify.notified();
        {
            let mut active = jsrt::sync::lock(&limiter.active);
            if *active < limit {
                *active += 1;
                return Ok(Permit(limiter.clone()));
            }
        }
        tokio::select! {
            () = notified => {}
            () = crate::http::cancelled(signal) => return Err(crate::provider::OPERATION_ABORTED.into()),
        }
    }
}

pub(super) enum ChildEvent {
    Line(Value),
    Exited(Option<i32>, Option<i32>),
}

/// One `claude` process: stdout lines that are JSON objects become events; stderr keeps a tail.
pub struct CliProcess {
    /// `None` when it could not be spawned ([`CliProcess::spawn_error`]).
    child: Option<signals::child::Watched>,
    stdin: Option<ChildStdin>,
    pub(super) events: tokio::sync::mpsc::UnboundedReceiver<ChildEvent>,
    pub(super) stderr: Arc<Mutex<String>>,
    pub(super) spawn_error: Option<String>,
}

/// SIGTERM to its process group, SIGKILL after [`KILL_GRACE`].
const STOP: signals::child::Stop = signals::child::Stop {
    group: true,
    term_grace: Some(KILL_GRACE),
};

impl CliProcess {
    pub(super) fn spawn(bin: &str, args: &[String], env: &Env, cwd: &Path) -> Self {
        use std::os::unix::process::CommandExt;
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let spawned = Command::new(bin)
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let _ = sender.send(ChildEvent::Exited(None, None));
                return CliProcess {
                    child: None,
                    stdin: None,
                    events,
                    stderr: stderr_tail,
                    spawn_error: Some(error.to_string()),
                };
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = signals::child::Watched::new(child);
        let (stderr_closed, stderr_done) = std::sync::mpsc::channel::<()>();
        if let Some(stderr) = stderr {
            let tail_buffer = stderr_tail.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut chunk = String::new();
                while reader.read_line(&mut chunk).is_ok_and(|n| n > 0) {
                    let mut buffer = jsrt::sync::lock(&tail_buffer);
                    let joined = format!("{buffer}{chunk}");
                    *buffer = tail(&joined, STDERR_TAIL_CHARS);
                    chunk.clear();
                }
                drop(stderr_closed);
            });
        }
        {
            let child = child.clone();
            std::thread::spawn(move || {
                if let Some(stdout) = stdout {
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else { break };
                        let line = line.trim();
                        if line.starts_with('{')
                            && let Ok(value) = serde_json::from_str::<Value>(line)
                        {
                            let _ = sender.send(ChildEvent::Line(value));
                        }
                    }
                }
                let status = child.wait();
                let _ = stderr_done.recv_timeout(signals::child::PIPE_GRACE);
                use std::os::unix::process::ExitStatusExt;
                let _ = sender.send(ChildEvent::Exited(
                    status.and_then(|s| s.code()),
                    status.and_then(|s| s.signal()),
                ));
            });
        }
        CliProcess {
            child: Some(child),
            stdin,
            events,
            stderr: stderr_tail,
            spawn_error: None,
        }
    }

    fn alive(&self) -> bool {
        self.child.as_ref().is_some_and(|child| !child.ended())
    }

    pub(super) fn send(&mut self, message: &Value) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{message}");
            let _ = stdin.flush();
        }
    }

    /// Close stdin so the CLI exits by itself; stop it if it lingers.
    pub(super) fn finish(mut self) {
        self.stdin.take();
        let Some(child) = self.child.clone() else { return };
        std::thread::spawn(move || {
            if !child.wait_timeout(EXIT_GRACE) {
                child.stop(STOP);
            }
        });
    }

    /// SIGTERM to the process group, SIGKILL after a grace period.
    pub(super) fn kill(&self) {
        if let Some(child) = self.child.clone().filter(|_| self.alive()) {
            std::thread::spawn(move || child.stop(STOP));
        }
    }
}

struct Spare {
    key: String,
    process: CliProcess,
    expires: Instant,
}

static SPARES: LazyLock<Mutex<Vec<Spare>>> = LazyLock::new(Mutex::default);
pub(super) static PROVEN_KEYS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

pub(super) fn take_spare(key: &str) -> Option<CliProcess> {
    let mut spares = jsrt::sync::lock(&SPARES);
    let mut index = 0;
    while index < spares.len() {
        if spares[index].key != key {
            index += 1;
            continue;
        }
        let spare = spares.remove(index);
        if spare.process.alive() && spare.expires > Instant::now() {
            return Some(spare.process);
        }
        spare.process.kill();
    }
    None
}

pub(super) fn start_spare(key: &str, bin: &str, args: &[String], env: &Env, cwd: &Path, config: RuntimeConfig) {
    if config.warm_spares == 0 {
        return;
    }
    let mut spares = jsrt::sync::lock(&SPARES);
    let now = Instant::now();
    spares.retain(|spare| {
        let keep = spare.expires > now && spare.process.alive();
        if !keep {
            spare.process.kill();
        }
        keep
    });
    if spares.iter().any(|s| s.key == key) {
        return;
    }
    while spares.len() as u64 >= config.warm_spares {
        let oldest = spares.remove(0);
        oldest.process.kill();
    }
    spares.push(Spare {
        key: key.to_string(),
        process: CliProcess::spawn(bin, args, env, cwd),
        expires: now + Duration::from_millis(config.warm_ttl_ms),
    });
}

pub(super) fn reset_process_state() {
    jsrt::sync::lock(&PROVEN_KEYS).clear();
    for spare in jsrt::sync::lock(&SPARES).drain(..) {
        spare.process.kill();
    }
}

/// `claudeCodeCliStats`: requests in flight and live spares.
pub fn claude_code_cli_stats() -> (u64, usize) {
    let in_flight = *jsrt::sync::lock(&LIMITER.active);
    let spares = jsrt::sync::lock(&SPARES).iter().filter(|s| s.process.alive()).count();
    (in_flight, spares)
}
