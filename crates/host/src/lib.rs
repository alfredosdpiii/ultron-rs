//! Host operations the Python kernel API calls (RLM-first core), served as a [`kernel::HostHandler`].
//!
//! Ported from the TypeScript session worker's host handler (experimental/session-worker.ts), its blocking
//! `bash` (ultron/rlm/host-bash.ts over core/bash-executor.ts) and host-owned shell jobs (ultron/rlm/shell-jobs.ts).
//! `read`, `write` and `edit` run inside runtime.py; the host only answers their `files.before_write` check.
//! Requests that need a model, subagents or durable stores are refused with [`not_available`].
//!
//! Shell jobs are durable (`Host::with_journal`): each runs under a small keeper shell in its own process group that
//! writes the raw output to `<id>.out` and the exit status to `<id>.exit`, so the job does not depend on the worker
//! that started it. The host follows those files into the cleaned, bounded spill file `<id>.log` and journals every
//! start and end. A reopened worker settles each job the journal lists as running from what really happened: still
//! running (the keeper is alive: it is followed again), completed with the status the keeper wrote, or `lost` (the
//! process ended without one); never silently gone.

pub mod bash_notes;
pub mod cell;
pub mod cell_writes;
pub mod file_hooks;
mod file_hooks_support;
mod jobs;
pub mod journal;
pub mod loki_guard;
pub mod sandboxed;
pub mod secrets;
pub mod shell;
pub mod text;
pub mod view_image;

use jobs::{Job, StopReason, cancel, job_record, read_output, snapshot, visible, wait};
use journal::JobRecord;
use jsrt::sync;
use kernel::store::DocumentStore;
use kernel::{HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use text::{DEFAULT_MAX_BYTES, Decoder, clean, truncate_tail};

pub const JOB_TEXT_BYTES: usize = 16 * 1024;
pub const JOB_RETAINED_BYTES: usize = 4 * 1024 * 1024;
pub const JOB_MAX_ACTIVE: usize = 32;
pub const JOB_MAX_RETAINED: usize = 64;
const JOB_MAX_WAIT_SECONDS: f64 = 3600.0;
const MAX_TIMEOUT_SECONDS: f64 = 86_400.0;
/// How often a waiting thread re-checks its cell's abort signal and a running command its stop condition.
// ponytail: 10 ms polling instead of wiring the kernel Signal into a condvar; fine for a handful of waiters.
const POLL: Duration = Duration::from_millis(10);

/// The before-write and before-shell guards (Loki). Every method is required so wrappers like [`sandboxed::SandboxGuard`]
/// cannot silently drop forwarding when a new hook method is added; use [`AllowAll`] for a no-op guard.
pub trait Guard: Send + Sync {
    /// A command of the cell on `lane`: `Some(reason)` blocks it, "bash command was not run: {reason}".
    fn before_shell(&self, lane: &str, command: &str) -> Option<String>;
    /// A write of the cell on `lane`: `{blocked, reason?, notes?}` as runtime.py reads it.
    fn before_write(&self, lane: &str, path: &str, content: &str) -> Value;
    /// Each file-hook guard's statistics (`fileHooks.stats()`), for `rlm.pool`.
    fn guard_stats(&self) -> Vec<Value>;
    /// A new root turn began: per-turn guard times start from zero.
    fn begin_turn(&self);
    /// A cell on `lane` is starting (after-cell checks take their baseline).
    fn cell_started(&self, lane: &str);
    /// A cell on `lane` ended (after-cell checks start in the background).
    fn cell_ended(&self, lane: &str);
    /// What after-cell checks found for `lane` and it has not been shown yet.
    fn take_pending(&self, lane: &str) -> Option<String>;
    /// Waits for the after-cell checks already started.
    fn settled(&self);
    /// Whether any after-cell check is still running in the background.
    fn is_checking(&self) -> bool;
}

pub struct AllowAll;
impl Guard for AllowAll {
    fn before_shell(&self, _lane: &str, _command: &str) -> Option<String> {
        None
    }
    fn before_write(&self, _lane: &str, _path: &str, _content: &str) -> Value {
        json!({ "blocked": false })
    }
    fn guard_stats(&self) -> Vec<Value> {
        Vec::new()
    }
    fn begin_turn(&self) {}
    fn cell_started(&self, _lane: &str) {}
    fn cell_ended(&self, _lane: &str) {}
    fn take_pending(&self, _lane: &str) -> Option<String> {
        None
    }
    fn settled(&self) {}
    fn is_checking(&self) -> bool {
        false
    }
}

pub fn not_available(kind: &str) -> String {
    format!("Ultron RLM host request is not available in this host yet: {kind}")
}

pub struct Host {
    cwd: PathBuf,
    jobs_dir: PathBuf,
    spill_dir: PathBuf,
    guard: Box<dyn Guard>,
    /// Runs shell commands isolated (see [`Host::sandboxed`]); None runs them as before.
    launcher: Option<Arc<sandbox::Launcher>>,
    /// The `shellPath` setting (`~` expanded); None or empty runs the default shell ([`shell::shell_config`]).
    shell_path: Option<String>,
    /// Extra environment of every command (a worktree subagent's `ULTRON_WORKTREE`, `ULTRON_PARENT_REPO`,
    /// `ULTRON_WORKTREE_BRANCH`, `PWD`), over the inherited one.
    env: Vec<(String, String)>,
    /// The environment every command starts from (the session's own, [`Host::with_base_env`]); None: this
    /// process's.
    base_env: Option<Arc<Vec<(String, String)>>>,
    /// Whether this host owns its jobs table: a host sharing another's ([`Host::sharing_jobs`]) never stops them.
    owns_jobs: bool,
    shared: Arc<Shared>,
}

/// A job that ended, as `ShellJobs` `onEnd` reports it: its record, whether a caller was waiting on it (the result
/// came inline, so nothing is announced) and whether its root turn's abort stopped it.
#[derive(Clone, Debug, PartialEq)]
pub struct JobEnd {
    pub record: JobRecord,
    pub awaited: bool,
    pub root_aborted: bool,
}

impl JobEnd {
    /// The `job_done` event (`jobEvent`), unless the end was awaited or its root turn aborted.
    pub fn event(&self) -> Option<Value> {
        journal::job_event(&self.record, self.awaited, self.root_aborted)
    }
}

/// Sees every job end (not those stopped by the host's shutdown).
pub type JobEnds = Arc<dyn Fn(JobEnd) + Send + Sync>;
/// The usage root a job started by `lane` belongs to (`host.rootOf(caller)`): the root turn running on the root
/// lane, or the root of the task that owns another lane.
pub type RootOf = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What job threads share with the host: the jobs, the journal they are written to, whether the host released
/// its jobs to a next owner, and who hears of their ends.
pub(crate) struct Shared {
    pub(crate) jobs: Mutex<Vec<Arc<Job>>>,
    pub(crate) journal: Mutex<Option<Arc<dyn DocumentStore>>>,
    pub(crate) released: AtomicBool,
    pub(crate) ends: Mutex<Option<JobEnds>>,
    pub(crate) roots: Mutex<Option<RootOf>>,
    /// What each lane's running cell wrote through checked writes ([`cell_writes`]).
    writes: cell_writes::CellWrites,
}

impl Shared {
    /// Writes the journal (every job, tails left out: they are not even copied), serialized under the jobs lock so the
    /// newest state wins.
    pub(crate) fn persist(&self) {
        self.persist_locked(&sync::lock(&self.jobs));
    }

    /// [`Shared::persist`] with the jobs lock held: a job's end and the release to a next owner are decided under it,
    /// so exactly one of the two workers settles each job.
    pub(crate) fn persist_locked(&self, jobs: &[Arc<Job>]) {
        let Some(journal) = sync::lock(&self.journal).clone() else {
            return;
        };
        if self.released.load(Ordering::SeqCst) {
            return;
        }
        let records: Vec<JobRecord> = jobs.iter().map(|job| job_record(job, false)).collect();
        let _ = journal.write(&journal::persisted(&records));
    }
}

pub(crate) fn command_of(payload: &Map<String, Value>) -> Result<String, String> {
    match payload.get("command") {
        Some(Value::String(command)) if !command.trim().is_empty() => Ok(command.clone()),
        _ => Err("bash command must be a non-empty string".into()),
    }
}

/// bashTimeoutSeconds: absent or null is no limit; at most a day.
pub(crate) fn timeout_of(value: Option<&Value>) -> Result<Option<f64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(seconds) if seconds > 0.0 => Ok(Some(seconds.min(MAX_TIMEOUT_SECONDS))),
            _ => Err("bash timeout must be a positive number of seconds".into()),
        },
    }
}

pub(crate) fn wait_seconds(value: Option<&Value>, name: &str) -> Result<Option<f64>, String> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(seconds) if seconds >= 0.0 => Ok(Some(seconds.min(JOB_MAX_WAIT_SECONDS))),
            _ => Err(format!("{name} must be a non-negative number of seconds")),
        },
    }
}

/// `Number.isSafeInteger(value) && value >= 0`, or `default` when absent.
pub(crate) fn safe_integer(value: Option<&Value>, default: u64) -> Option<u64> {
    match value {
        None => Some(default),
        Some(value) => jsrt::number::safe_integer(value).and_then(|n| u64::try_from(n).ok()),
    }
}

/// The verdict token of an `ultron mcp --child` server (`ULTRON_PARENT_TOKEN`), removed from every command's
/// environment.
pub const PARENT_TOKEN_ENV: &str = "ULTRON_PARENT_TOKEN";

/// `argv` as it runs inside a sandbox: every sandbox clears the environment to its allowlist (bwrap `--clearenv`,
/// `env -i` under unshare and in a Docker container), so the command's own variables (a worktree subagent's
/// `ULTRON_WORKTREE`, `ULTRON_PARENT_REPO`, `ULTRON_WORKTREE_BRANCH`, `PWD`) are set inside, by `env`.
pub fn sandboxed_argv(argv: Vec<String>, env: &[(String, String)]) -> Vec<String> {
    if env.is_empty() {
        return argv;
    }
    std::iter::once("env".to_string())
        .chain(env.iter().map(|(key, value)| format!("{key}={value}")))
        .chain(argv)
        .collect()
}

/// Where a command runs: its directory, the base environment (None: this process's) and its own variables.
#[derive(Clone, Copy)]
pub(crate) struct CommandEnv<'a> {
    pub(crate) cwd: &'a Path,
    pub(crate) base: Option<&'a [(String, String)]>,
    pub(crate) env: &'a [(String, String)],
}

fn path_of(env: &[(String, String)]) -> Option<String> {
    env.iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.clone())
}

/// Gives `process` its environment: an unisolated command gets the base environment (when there is one) and then
/// `env`. A sandboxed one keeps the launcher's own (the sandbox crate's `launch_env`, as its mechanism probe uses):
/// inside, the command sees the policy's allowlist, valued from the base environment ([`Host::sandboxed`]), and its
/// own variables ([`sandboxed_argv`]).
pub(crate) fn command_env(
    process: &mut Command,
    base: Option<&[(String, String)]>,
    env: &[(String, String)],
    isolated: bool,
) {
    if let (Some(base), false) = (base, isolated) {
        process.env_clear().envs(base.iter().map(|(key, value)| (key, value)));
    }
    process.envs(env.iter().map(|(key, value)| (key, value)));
}

/// Runs `command` with `shell` in its own process group, stdout and stderr on one pipe. `on_text` gets each decoded,
/// cleaned chunk and the raw byte count; `stop` is polled and kills the process group when it turns true.
/// Returns the exit code (None when killed by a signal).
fn run_shell(
    shell: &shell::ShellConfig,
    command: &str,
    CommandEnv { cwd, base, env }: CommandEnv<'_>,
    launcher: Option<&sandbox::Launcher>,
    mut on_text: impl FnMut(&str, usize),
    stop: impl Fn() -> bool,
) -> Result<Option<i32>, String> {
    let argv = shell.argv(command);
    let mut process = match launcher {
        Some(launcher) => launcher.command(&sandboxed_argv(argv, env), cwd)?,
        None => {
            let mut process = Command::new(&argv[0]);
            process.args(&argv[1..]).current_dir(cwd);
            process
        }
    };
    let isolated = launcher.is_some_and(|launcher| launcher.mechanism().is_some());
    command_env(&mut process, base, env, isolated);
    process.env_remove(PARENT_TOKEN_ENV);
    let (reader, writer) = std::io::pipe().map_err(|error| error.to_string())?;
    let stderr = writer.try_clone().map_err(|error| error.to_string())?;
    let mut child = process
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| error.to_string())?;
    drop(process);
    let group = i32::try_from(child.id()).unwrap_or(0);
    let (sender, chunks) = mpsc::channel::<Vec<u8>>();
    let pump = std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = vec![0u8; 64 * 1024];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut decoder = Decoder::default();
    let mut killed = false;
    loop {
        match chunks.recv_timeout(POLL) {
            Ok(data) => on_text(&clean(&decoder.decode(&data, true)), data.len()),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !killed && stop() {
            killed = true;
            kernel::kill_command_group(group);
        }
    }
    let _ = pump.join();
    on_text(&clean(&decoder.decode(&[], false)), 0);
    let status = child.wait().map_err(|error| error.to_string())?;
    Ok(status.code())
}

impl Host {
    /// Each file-hook guard's statistics (`fileHooks.stats()`).
    pub fn guard_stats(&self) -> Vec<Value> {
        self.guard.guard_stats()
    }

    /// A new root turn began: per-turn guard times start from zero (`fileHooks.beginTurn()`).
    pub fn begin_turn(&self) {
        self.guard.begin_turn();
    }

    /// A cell on `lane` is starting: the after-cell file hooks take their baseline picture if they have none.
    pub fn cell_started(&self, lane: &str) {
        self.shared.writes.begin(lane);
        self.guard.cell_started(lane);
    }

    /// Record what each cell writes through checked writes, for [`Host::take_cell_writes`] (off by default).
    pub fn recording_cell_writes(self, on: bool) -> Self {
        self.shared.writes.set_enabled(on);
        self
    }

    /// Where commands run and relative paths resolve.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The files `lane`'s cell wrote through checked writes, with their texts (and forget them).
    pub fn take_cell_writes(&self, lane: &str) -> Vec<cell_writes::CellWrite> {
        self.shared.writes.take(lane)
    }

    /// A cell on `lane` ended: the after-cell file hooks check what it changed, in the background.
    pub fn cell_ended(&self, lane: &str) {
        self.guard.cell_ended(lane);
    }

    /// Findings of after-cell checks not yet shown to `lane` (and forget them), for its next cell result.
    pub fn take_pending(&self, lane: &str) -> Option<String> {
        self.guard.take_pending(lane)
    }

    /// Findings of the after-cell checks of `lane`'s cells so far, once those checks have finished: what the last
    /// cell of a turn changed has no next cell result to arrive with.
    pub fn settled_pending(&self, lane: &str) -> Option<String> {
        self.guard.settled();
        self.guard.take_pending(lane)
    }

    /// Whether any after-cell check is still running in the background.
    pub fn is_checking(&self) -> bool {
        self.guard.is_checking()
    }

    /// `cwd` is where commands run; job spill files go to `jobs_dir/<id>.log`, blocking-bash spill files to
    /// `spill_dir/pi-bash-<hex>.log` (TS: the OS temp dir).
    pub fn new(
        cwd: impl Into<PathBuf>,
        jobs_dir: impl Into<PathBuf>,
        spill_dir: impl Into<PathBuf>,
        guard: Box<dyn Guard>,
    ) -> Self {
        let jobs_dir = jobs_dir.into();
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&jobs_dir);
        let cwd = cwd.into();
        Host {
            shared: Arc::new(Shared {
                jobs: Mutex::new(Vec::new()),
                journal: Mutex::new(None),
                released: AtomicBool::new(false),
                ends: Mutex::new(None),
                roots: Mutex::new(None),
                writes: cell_writes::CellWrites::default(),
            }),
            cwd,
            jobs_dir,
            spill_dir: spill_dir.into(),
            guard,
            launcher: None,
            shell_path: None,
            env: Vec::new(),
            base_env: None,
            owns_jobs: true,
        }
    }

    /// Every command (blocking bash, shell jobs, the sandbox's allowlisted variables) starts from `env` instead of
    /// this process's environment, and the shell and the sandbox launcher are found on its PATH: a session's
    /// commands see the session's environment, never the server's. Call before [`Host::sandboxed`].
    pub fn with_base_env(mut self, env: Vec<(String, String)>) -> Self {
        self.base_env = Some(Arc::new(env));
        self
    }

    /// Every command (blocking bash and shell jobs) runs with `env` over the inherited environment, as
    /// `withShellEnv(operations, laneWorkspace(lane).env)` runs a worktree subagent's.
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    /// This host's shell jobs join `session`'s one table (TypeScript's single `ShellJobs` for every lane): the root
    /// lane lists them, the root turn's abort cancels them, their ends are announced and journaled by the session's
    /// hooks. The jobs keep running after this host is dropped (they are the session's).
    pub fn sharing_jobs(mut self, session: &Host) -> Self {
        self.shared = session.shared.clone();
        self.owns_jobs = false;
        self
    }

    /// Commands run under `shell_path` (the `shellPath` setting), resolved for each command as TypeScript's
    /// `createLocalBashOperations({ shellPath })` does: a path that does not exist fails the command.
    pub fn with_shell_path(mut self, shell_path: Option<String>) -> Self {
        self.shell_path = shell_path;
        self
    }

    pub(crate) fn shell(&self) -> Result<shell::ShellConfig, String> {
        self.shell_named(self.shell_path.as_deref())
    }

    /// The shell `custom` names, else the default one, with `bash` looked up on the base environment's PATH.
    pub(crate) fn shell_named(&self, custom: Option<&str>) -> Result<shell::ShellConfig, String> {
        match &self.base_env {
            Some(env) => shell::shell_config_on_path(custom, path_of(env).as_deref()),
            None => shell::system_shell_config(custom),
        }
    }

    /// A request of `lane`'s kernel: its jobs belong to it (and to its root turn), and a lane other than the root's
    /// sees only its own.
    pub fn handle_lane(
        &self,
        lane: &str,
        kind: &str,
        payload: Map<String, Value>,
        signal: &Signal,
    ) -> Result<Value, String> {
        if kind == "files.before_write" {
            let writes = match payload.get("writes") {
                Some(Value::Array(writes)) if (1..=64).contains(&writes.len()) => writes,
                _ => return Err("files.before_write needs 1 to 64 writes".into()),
            };
            let mut proposed = Vec::new();
            for write in writes {
                match (write.get("path"), write.get("content")) {
                    (Some(Value::String(path)), Some(Value::String(content)))
                        if !path.is_empty() && !path.contains('\0') =>
                    {
                        proposed.push((path.as_str(), content.as_str()));
                    }
                    _ => return Err("files.before_write writes need a path and text content".into()),
                }
            }
            let results: Vec<Value> = proposed
                .into_iter()
                .map(|(path, content)| {
                    let result = self.guard.before_write(lane, path, content);
                    if result["blocked"] != true {
                        let path = file_hooks::resolve_lexical(&self.cwd, path);
                        self.shared.writes.record(lane, &path, content);
                    }
                    result
                })
                .collect();
            return Ok(json!({ "results": results }));
        }
        if matches!(kind, "bash" | "shell.run")
            && let Some(Value::String(command)) = payload.get("command")
            && let Some(reason) = self.guard.before_shell(lane, command)
        {
            return Err(format!("bash command was not run: {reason}"));
        }
        let requested = payload.get("command").and_then(Value::as_str).map(str::to_string);
        let mut result = self.dispatch_shell(lane, kind, payload, signal)?;
        if matches!(
            kind,
            "bash" | "shell.run" | "shell.result" | "shell.get" | "shell.cancel"
        ) {
            let command = requested.unwrap_or_else(|| result["command"].as_str().unwrap_or_default().to_string());
            let policy = self
                .launcher
                .as_deref()
                .filter(|launcher| launcher.mechanism().is_some())
                .map(|launcher| &launcher.policy);
            bash_notes::annotate(&mut result, &command, policy);
        }
        Ok(result)
    }

    fn dispatch_shell(
        &self,
        lane: &str,
        kind: &str,
        payload: Map<String, Value>,
        signal: &Signal,
    ) -> Result<Value, String> {
        match kind {
            "bash" if payload.contains_key("yield_after") => self.shell_bash(lane, &payload, signal),
            "bash" => self.blocking_bash(&payload, signal, &mut |_| {}),
            "shell.run" => {
                kernel::ops::fields(kind, &payload)?;
                let command = command_of(&payload)?;
                let timeout = timeout_of(payload.get("timeout"))?;
                let yield_after = wait_seconds(payload.get("yield_after"), "yield_after")?.unwrap_or(0.0);
                let job = self.start_job(lane, command, timeout)?;
                wait(&job, Some(yield_after), signal);
                Ok(snapshot(&job))
            }
            "shell.result" => {
                kernel::ops::fields(kind, &payload)?;
                let job = self.find_job(lane, payload.get("id"))?;
                if sync::lock(&job.record).status == journal::Status::Running {
                    wait(&job, wait_seconds(payload.get("wait"), "wait")?, signal);
                }
                Ok(snapshot(&job))
            }
            "shell.get" => {
                kernel::ops::fields(kind, &payload)?;
                let job = self.find_job(lane, payload.get("id"))?;
                Ok(snapshot(&job))
            }
            "shell.cancel" => {
                kernel::ops::fields(kind, &payload)?;
                let job = self.find_job(lane, payload.get("id"))?;
                cancel(&job, StopReason::Cancelled, &self.shared);
                Ok(snapshot(&job))
            }
            "shell.read" => {
                kernel::ops::fields(kind, &payload)?;
                let job = self.find_job(lane, payload.get("id"))?;
                read_output(&job, payload.get("cursor"), payload.get("max_bytes"))
            }
            "shell.list" => {
                kernel::ops::fields(kind, &payload)?;
                let jobs = sync::lock(&self.shared.jobs).clone();
                Ok(Value::Array(
                    jobs.iter()
                        .rev()
                        .filter(|job| visible(job, lane))
                        .map(|job| {
                            let mut snapshot = snapshot(job);
                            snapshot["text"] = Value::Null;
                            snapshot
                        })
                        .collect(),
                ))
            }
            other if other.starts_with("shell.") => Err(kernel::ops::not_wired(other)),
            other => Err(not_available(other)),
        }
    }

    /// A user's own command (RPC `bash`, the TUI's `!`; AgentSession `executeBash`): the shell guard, then the
    /// blocking bash, each cleaned output chunk passed to `on_chunk` as it arrives (bash-executor.ts `onChunk`, which
    /// AgentSession streams as `bash_execution_update`).
    pub fn user_bash(&self, command: &str, signal: &Signal, on_chunk: &mut dyn FnMut(&str)) -> Result<Value, String> {
        if let Some(reason) = self.guard.before_shell(kernel::ROOT_LANE, command) {
            return Err(format!("bash command was not run: {reason}"));
        }
        let mut payload = Map::new();
        payload.insert("command".into(), command.into());
        self.blocking_bash(&payload, signal, on_chunk)
    }

    /// host-bash.ts runHostBash over bash-executor.ts: wait for the command, keep the tail, spill the whole output.
    fn blocking_bash(
        &self,
        payload: &Map<String, Value>,
        signal: &Signal,
        on_chunk: &mut dyn FnMut(&str),
    ) -> Result<Value, String> {
        let command = command_of(payload)?;
        let timeout = timeout_of(payload.get("timeout"))?;
        let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
        let timed_out = std::cell::Cell::new(false);
        let mut chunks: std::collections::VecDeque<String> = Default::default();
        let mut kept_units = 0usize;
        let mut total_bytes = 0usize;
        let mut spill: Option<(PathBuf, File)> = None;
        let open_spill =
            |spill: &mut Option<(PathBuf, File)>, chunks: &std::collections::VecDeque<String>| -> Result<(), String> {
                if spill.is_none() {
                    let path = self
                        .spill_dir
                        .join(format!("pi-bash-{}.log", jsrt::random::random_hex(8)));
                    let mut file = File::create(&path).map_err(|error| error.to_string())?;
                    for chunk in chunks {
                        let _ = file.write_all(chunk.as_bytes());
                    }
                    *spill = Some((path, file));
                }
                Ok(())
            };
        let shell = self.shell()?;
        let mut failure = None;
        let exit = run_shell(
            &shell,
            &command,
            CommandEnv {
                cwd: &self.cwd,
                base: self.base_env.as_deref().map(Vec::as_slice),
                env: &self.env,
            },
            self.launcher.as_deref(),
            |text, raw| {
                if raw > 0 || !text.is_empty() {
                    on_chunk(text);
                }
                total_bytes += raw;
                if total_bytes > DEFAULT_MAX_BYTES
                    && let Err(error) = open_spill(&mut spill, &chunks)
                {
                    failure = Some(error);
                }
                if let Some((_, file)) = spill.as_mut() {
                    let _ = file.write_all(text.as_bytes());
                }
                chunks.push_back(text.to_owned());
                kept_units += text.encode_utf16().count();
                while kept_units > DEFAULT_MAX_BYTES * 2 && chunks.len() > 1 {
                    if let Some(removed) = chunks.pop_front() {
                        kept_units -= removed.encode_utf16().count();
                    }
                }
            },
            || {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    timed_out.set(true);
                }
                timed_out.get() || signal.is_aborted()
            },
        )?;
        if let Some(error) = failure {
            return Err(error);
        }
        let full: String = chunks.iter().map(String::as_str).collect();
        let truncated = truncate_tail(&full);
        if truncated.is_some() {
            open_spill(&mut spill, &chunks)?;
        }
        let stopped = timed_out.get() || signal.is_aborted();
        Ok(json!({
            "output": truncated.clone().unwrap_or(full),
            "exit_code": if stopped { None } else { exit },
            "cancelled": stopped && !timed_out.get(),
            "timed_out": timed_out.get(),
            "truncated": truncated.is_some(),
            "full_output_path": spill.map(|(path, _)| path.to_string_lossy().into_owned()),
        }))
    }
}

impl HostHandler for Host {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self.handle_lane(kernel::ROOT_LANE, kind, payload, signal)
    }
}

impl Drop for Host {
    /// Worker shutdown: stop every running job, unless they were released to the next owner.
    fn drop(&mut self) {
        if self.owns_jobs && !self.shared.released.load(Ordering::SeqCst) {
            self.stop_jobs();
        }
    }
}
