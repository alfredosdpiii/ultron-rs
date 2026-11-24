//! Supervisor for Ultron's persistent Python kernel: a port of the host side of
//! `packages/coding-agent/src/ultron/rlm/kernel.ts`. The Python side is the unchanged `runtime.py`.
//!
//! The kernel runs `python runtime.py --protocol-fds` in its own process group. Protocol frames (one JSON object per
//! line, at most 1 MiB) arrive on the kernel's fd 3 and host messages go to its fd 4; stdin is /dev/null, and fd 1 and
//! fd 2 are plain output attributed to the running cell. After a cell, runtime.py writes a flush marker naming the
//! cell's request id to fd 1 and fd 2 and then sends `done`, listing the markers it wrote; the cell resolves once the
//! host has read them, so the cell's raw output is complete.
//!
//! Calls block; cells run one at a time in call order. A [`Signal`] cancels a call: a queued call is rejected without
//! touching the kernel, a running one kills the kernel's process tree. Any kernel failure (exit, protocol violation,
//! startup timeout, CPU backstop) rejects every pending call with an explicit error and loses the Python state; the
//! next call starts a fresh kernel. Host requests from Python go to a [`HostHandler`], each on its own thread.
//!
//! Snapshots (kernel.ts `snapshot`/`restore`): runtime.py writes a data-only snapshot of the namespace; the host
//! signs it with an HMAC key the kernel never sees and verifies the signature before the kernel may read it back.
//! With `snapshot_path` a new kernel restores it before its first call; once a started kernel failed or a running
//! call was cancelled it does not (kernel.ts never silently resurrects an older checkpoint). A [`ContinuityPolicy`]
//! replaces that rule: checkpoints follow every cell, a dead kernel is restarted and restored at once, and the next
//! cell result reports what was restored and lost (see [`continuity`]).

mod config;
pub mod continuity;
pub mod env;
mod lifecycle;
pub mod ops;
mod output;
mod process;
mod protocol;
mod sandboxed;
mod signal;
pub mod snapshot;
pub mod snapshot_store;
pub mod store;
pub mod tree_memory;

pub use continuity::{ContinuityPolicy, ContinuityReport, LostName};
pub use process::kill_command_group;
use protocol::{entry_reject, finish_cell, handle_raw, read_chunks, take_raw_output};
pub use signal::{Link, Signal};
pub use snapshot::{
    DEFAULT_MAX_SNAPSHOT_BYTES, ENV_MAX_SNAPSHOT_BYTES, RestoreReport, SnapshotReport, is_orphan_snapshot_temp,
    load_or_create_snapshot_key, max_snapshot_bytes, process_snapshot_key, sign_snapshot, sweep_orphan_snapshots,
    verify_snapshot,
};
pub use tree_memory::TreeMemoryBackend;

pub use config::{KernelResourceLimits, LimitOverrides, kernel_environment, kernel_resource_limits, rlm_output_budget};

/// The root agent's lane; every other lane is a child's (the session journal's `session_journal::run::MAIN_LANE`).
pub const ROOT_LANE: &str = "main";

/// Whether `viewer`'s kernel sees work owned by `owner`: the root lane sees every lane's, a child lane its own.
pub fn sees(viewer: &str, owner: &str) -> bool {
    viewer == ROOT_LANE || viewer == owner
}

use command_fds::{CommandFdExt, FdMapping};
use continuity::{Crash, recovery_report};
use output::{
    MAX_FRAME_BYTES, MAX_ORPHAN_CHARS, RawCapture, Utf8Stream, append_bounded, find, flush_marker, keep_tail,
    marker_prefix_len, orphan_text,
};
use rustix::process::{WaitId, WaitIdOptions};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

/// A failed call: the kernel failed, was shut down, the call was cancelled, or its request could not be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelError {
    message: String,
}

impl KernelError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for KernelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for KernelError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CellError {
    pub ename: String,
    pub evalue: String,
    pub traceback: Vec<String>,
}

/// What a cell (or another request) produced. Serializes as kernel.ts `KernelExecutionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionResult {
    pub status: Status,
    pub stdout: String,
    pub stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CellError>,
}

/// A call's full reply: the result plus the snapshot or restore report a done frame carried (kernel.ts
/// `KernelExecutionResult` with `snapshot?` / `restore?`), and, on the first cell after a recovery, what it
/// restored and lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KernelReply {
    #[serde(flatten)]
    pub result: ExecutionResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<RestoreReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<ContinuityReport>,
}

impl KernelReply {
    fn from_result(result: ExecutionResult) -> Self {
        Self {
            result,
            snapshot: None,
            restore: None,
            recovery: None,
        }
    }
}

/// Serves host requests from Python (`await rlm.host_request(type, payload)` and every API built on it). Runs on its
/// own thread per request; `signal` is aborted when the requesting cell completes, Python cancels the request, or the
/// kernel fails. The outcome is still sent while the kernel lives (Python drops one nobody awaits). `Err` becomes a
/// `RuntimeError` with that message in Python.
pub trait HostHandler: Send + Sync {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String>;
}

impl<F> HostHandler for F
where
    F: Fn(&str, Map<String, Value>, &Signal) -> Result<Value, String> + Send + Sync,
{
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self(kind, payload, signal)
    }
}

/// Test handler: replies `{"type": kind, "payload": payload}`, and refuses `test.fail` with "refused test.fail".
pub struct EchoHandler;

impl HostHandler for EchoHandler {
    fn handle(&self, kind: &str, payload: Map<String, Value>, _signal: &Signal) -> Result<Value, String> {
        if kind == "test.fail" {
            return Err(format!("refused {kind}"));
        }
        let mut reply = Map::new();
        reply.insert("type".into(), Value::String(kind.into()));
        reply.insert("payload".into(), Value::Object(payload));
        Ok(Value::Object(reply))
    }
}

pub struct KernelOptions {
    pub cwd: PathBuf,
    pub runtime_path: PathBuf,
    /// Interpreter; default `ULTRON_PYTHON`, else /usr/bin/python3 when it exists, else `python3`.
    pub python: Option<PathBuf>,
    /// Bounds initialization, not execution.
    pub startup_timeout: Duration,
    /// Overrides `ULTRON_RLM_MAX_MEMORY_MB` / `ULTRON_RLM_MAX_CPU_SECONDS`; 0 disables a limit.
    pub limits: LimitOverrides,
    /// Extra environment for the kernel, applied after the credential filter.
    pub env: Vec<(String, String)>,
    /// The default path of [`Kernel::snapshot`] / [`Kernel::restore`]; a new kernel restores it before its first
    /// call (kernel.ts `snapshotPath`).
    pub snapshot_path: Option<PathBuf>,
    /// HMAC key that signs and verifies snapshots in the host; the kernel never receives it. Default: a key of this
    /// process, so snapshots then restore only within it.
    pub snapshot_key: Option<Vec<u8>>,
    /// Checkpoint after cells and recover from kernel death; requires `snapshot_path`.
    pub continuity: Option<ContinuityPolicy>,
    /// Start runtime.py inside this launcher's sandbox (the session's bash policy, plus the snapshot and spill
    /// directories writable and the interpreter readable), so a cell's direct `open()` is contained too. Docker
    /// cannot carry the protocol descriptors: with it (or nothing working) `auto` runs unsandboxed and `required`
    /// refuses to start. None: unsandboxed.
    pub sandbox: Option<Arc<sandbox::Launcher>>,
    /// The whole-tree memory cap (kernel.ts `maxTreeMemoryMb` / `treeMemoryBackend`); None: no tree cap.
    pub tree_memory: Option<TreeMemoryOptions>,
    /// The environment the kernel starts from (before the credential filter) and where its `ULTRON_*` settings
    /// (`ULTRON_PYTHON`, limits, output budget, tree memory) are read: a session's own environment, so the kernel
    /// never sees the server process's PATH or variables. None: this process's environment.
    pub base_env: Option<Vec<(String, String)>>,
}

/// How a kernel caps the memory of its whole process tree ([`tree_memory`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeMemoryOptions {
    /// The cap in MiB; overrides `ULTRON_RLM_MAX_TREE_MEMORY_MB`, whose default is twice the per-process limit.
    /// 0 disables it.
    pub max_mb: Option<u64>,
    /// `auto` | `cgroup` | `watchdog` | `off`; None: `ULTRON_RLM_TREE_MEMORY_BACKEND`, else `auto` (a capped cgroup
    /// scope when this system allows one, else the watchdog).
    pub backend: Option<String>,
    /// The scope launcher (`systemd-run`), probed once before its first use.
    pub launcher: PathBuf,
}

impl Default for TreeMemoryOptions {
    fn default() -> Self {
        Self {
            max_mb: None,
            backend: None,
            launcher: "systemd-run".into(),
        }
    }
}

impl KernelOptions {
    pub fn new(cwd: impl Into<PathBuf>, runtime_path: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            runtime_path: runtime_path.into(),
            python: None,
            startup_timeout: Duration::from_secs(10),
            limits: LimitOverrides::default(),
            env: Vec::new(),
            snapshot_path: None,
            snapshot_key: None,
            continuity: None,
            sandbox: None,
            tree_memory: None,
            base_env: None,
        }
    }

    /// `name` in the kernel's base environment ([`KernelOptions::base_env`], else this process's).
    pub fn var(&self, name: &str) -> Option<String> {
        match &self.base_env {
            Some(env) => env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()),
            None => std::env::var(name).ok(),
        }
    }

    /// The kernel's base environment ([`KernelOptions::base_env`], else this process's).
    fn base_environment(&self) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        match &self.base_env {
            Some(env) => env.iter().map(|(key, value)| (key.into(), value.into())).collect(),
            None => std::env::vars_os().collect(),
        }
    }
}

/// runtime.py exit codes for limits it could not report in-band.
const EXIT_MEMORY: i32 = 86;
const EXIT_CPU: i32 = 87;
const SIGKILL: i32 = 9;
const SIGTERM: i32 = 15;
const SIGXCPU: i32 = 24;
const MAX_HOST_REQUESTS: usize = 16;
/// How long a finished cell waits for its flush markers on fd 1 and fd 2.
const RAW_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a closed protocol channel waits for the exit status, so a limit's exit code is what gets reported.
const CLOSED_CHANNEL_GRACE: Duration = Duration::from_millis(200);
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);
const PLACEHOLDER_ID: &str = "00000000-0000-0000-0000-000000000000";
const SHUT_DOWN: &str = "RLM kernel is shut down";
const NOT_RUNNING: &str = "RLM kernel is not running";

fn memory_limit_message(limits: &KernelResourceLimits) -> String {
    format!(
        "RLM kernel exceeded its memory limit ({} MiB per process, ULTRON_RLM_MAX_MEMORY_MB) and was stopped; the next cell starts a fresh kernel without earlier Python state",
        limits.max_memory_mb
    )
}

fn cpu_limit_message(limits: &KernelResourceLimits) -> String {
    format!(
        "RLM kernel exceeded its CPU limit ({} CPU-seconds per cell, ULTRON_RLM_MAX_CPU_SECONDS) and was stopped; the next cell starts a fresh kernel without earlier Python state",
        limits.max_cpu_seconds
    )
}

/// A frame value as text: strings as they are, null as empty, anything else as JSON.
fn frame_string(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

fn encode_frame(frame: &Map<String, Value>) -> Result<String, KernelError> {
    let line = serde_json::to_string(frame)
        .map_err(|error| KernelError::new(format!("RLM kernel cannot encode outbound frame: {error}")))?;
    if line.len() > MAX_FRAME_BYTES {
        return Err(KernelError::new(format!(
            "RLM kernel outbound frame exceeds 1 MiB ({MAX_FRAME_BYTES} bytes)"
        )));
    }
    Ok(line + "\n")
}

fn object(entries: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

/// A random (version 4) UUID: the request id, which also names the cell's flush marker.
fn random_uuid() -> Result<String, KernelError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| KernelError::new(format!("RLM kernel request id: {error}")))?;
    Ok(jsrt::random::uuid_v4_from(bytes))
}

/// Lone UTF-16 surrogate escapes (which Python's json.dumps emits for surrogate-escaped text) become U+FFFD: a Rust
/// string cannot hold them, and refusing the frame would end the kernel.
fn replace_lone_surrogates(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("\\u") {
        return line.into();
    }
    let hex = |index: usize| -> Option<u16> {
        let digits = line.get(index..index + 6)?.strip_prefix("\\u")?;
        u16::from_str_radix(digits, 16).ok()
    };
    let bytes = line.as_bytes();
    let mut text = String::with_capacity(line.len());
    let (mut index, mut copied) = (0, 0);
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            index += 1;
            continue;
        }
        match hex(index) {
            Some(0xd800..=0xdbff) if matches!(hex(index + 6), Some(0xdc00..=0xdfff)) => index += 12,
            Some(0xd800..=0xdfff) => {
                text.push_str(&line[copied..index]);
                text.push_str("\\ufffd");
                index += 6;
                copied = index;
            }
            _ => index += 2,
        }
    }
    if copied == 0 {
        return line.into();
    }
    text.push_str(&line[copied..]);
    text.into()
}

/// Exit of the kernel's root process. `exited` is set (under the state lock) before the process is reaped, so a
/// kill under that lock that sees it unset cannot hit a reused pid.
#[derive(Default)]
struct Exit {
    state: Mutex<(bool, Option<ExitStatus>)>,
    changed: Condvar,
}

impl Exit {
    fn exited(&self) -> bool {
        jsrt::sync::lock(&self.state).0
    }

    /// Waits up to `timeout` for the exit status; whether it is known.
    fn wait_reaped(&self, timeout: Duration) -> bool {
        let state = jsrt::sync::lock(&self.state);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.1.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.1.is_some()
    }
}

#[derive(Default)]
struct RawStream {
    decoder: Utf8Stream,
    /// Bytes that may start the running cell's flush marker, held until the next chunk.
    held: Vec<u8>,
    /// Output from while no cell owned the stream; reported at the start of the next cell.
    orphan: String,
    orphan_dropped: bool,
    /// Whether the runtime's startup marker has arrived; output before it is startup diagnostics.
    started: bool,
}

struct Waiter {
    op: u64,
    state: KernelReply,
    controller: Signal,
    handler: Option<Arc<dyn HostHandler>>,
}

/// Raw output of the execute cell that owns fd 1 and fd 2 until its flush markers arrive (or time out).
struct CellOutput {
    request_id: String,
    marker: Vec<u8>,
    captures: [RawCapture; 2],
    orphan: [String; 2],
    /// Set by the done frame: the markers runtime.py wrote and the host still waits for.
    awaiting: Option<[bool; 2]>,
    deadline: Option<Instant>,
    /// The finished cell, once its done frame has arrived.
    finished: Option<Waiter>,
}

struct Generation {
    id: u64,
    pid: i32,
    /// How the tree cap is enforced for this process.
    tree_backend: TreeMemoryBackend,
    /// The cgroup v2 directory of the kernel's scope, resolved once the runtime is up, and its OOM kills then.
    cgroup_dir: Option<PathBuf>,
    oom_kills_at_start: u64,
    /// Descendants seen by watchdog scans (pid to start time), killed with the tree even if they left it.
    known_descendants: Arc<Mutex<HashMap<i32, u64>>>,
    writer: mpsc::Sender<String>,
    exit: Arc<Exit>,
    raw: [RawStream; 2],
    output: Option<CellOutput>,
    pending: HashMap<String, Waiter>,
    active_cell: Option<String>,
    /// In-flight host requests and the signal each one's handler gets (aborted by its cell's end or a cancel).
    host_requests: HashMap<String, Signal>,
    ready_received: bool,
    initialized: bool,
    startup_deadline: Instant,
    cell_cpu_start: Option<f64>,
    /// The process the CPU backstop measures: the root, or under a sandbox the Python process found at the first cell.
    cpu_pid: Option<i32>,
    sandbox: Option<sandbox::Mechanism>,
    buffer: Vec<u8>,
    /// The snapshot this generation restores before it counts as started.
    restore_path: Option<PathBuf>,
    /// After `ready`, startup is completed by a waiting caller (restore, recovery report) instead of the reader.
    host_completes_startup: bool,
    /// A caller is completing startup.
    completing: bool,
}

/// A `host_request` frame: `cell` is the execute request whose code made it (absent outside a cell).
struct HostRequest {
    id: String,
    kind: String,
    payload: Map<String, Value>,
    cell: Option<String>,
}

struct Op {
    running: bool,
    outcome: Option<Result<KernelReply, KernelError>>,
    /// The controller of the cell this call runs.
    cell: Option<Signal>,
    handler: Option<Arc<dyn HostHandler>>,
    /// A recovery report taken by this execute call when its cell was sent.
    recovery: Option<ContinuityReport>,
}

/// Continuity bookkeeping (see [`continuity`]).
#[derive(Default)]
struct Continuity {
    /// The latest checkpoint written to the configured snapshot path.
    checkpoint: Option<SnapshotReport>,
    /// Cells completed, and the count at the latest checkpoint.
    cells: u64,
    checkpoint_cells: u64,
    last_checkpoint: Option<Instant>,
    /// A cell completed since the latest checkpoint.
    dirty: bool,
    snapshot_queued: bool,
    idle_flush_scheduled: bool,
    /// The failure the next recovery reports.
    crash: Option<Crash>,
    /// A recovery report for the next cell.
    notice: Option<ContinuityReport>,
    last_recovery: Option<ContinuityReport>,
    /// The failed generation was running: start a new one without waiting for a call.
    restart: bool,
}

#[derive(Default)]
struct State {
    generation: Option<Generation>,
    next_generation: u64,
    /// The error the latest failed generation reported.
    failure: Option<(u64, KernelError)>,
    closed: bool,
    stderr: String,
    ops: HashMap<u64, Op>,
    queue: VecDeque<u64>,
    next_op: u64,
    /// kernel.ts `autoRestore = false`: never restore the configured snapshot into a later generation.
    no_auto_restore: bool,
    continuity: Continuity,
}

impl State {
    fn current(&mut self, generation: u64) -> Option<&mut Generation> {
        self.generation.as_mut().filter(|current| current.id == generation)
    }

    fn diagnostics(&self) -> String {
        if self.stderr.is_empty() {
            String::new()
        } else {
            format!(": {}", self.stderr)
        }
    }

    fn resolve(&mut self, waiter: Waiter) {
        if let Some(op) = self.ops.get_mut(&waiter.op)
            && op.outcome.is_none()
        {
            op.outcome = Some(Ok(waiter.state));
        }
    }

    fn reject_ops(&mut self, error: &KernelError) {
        for op in self.ops.values_mut() {
            op.outcome.get_or_insert_with(|| Err(error.clone()));
        }
        self.queue.clear();
    }
}

struct Shared {
    options: KernelOptions,
    handler: Arc<dyn HostHandler>,
    limits: KernelResourceLimits,
    /// The tree cap in MiB (0: none).
    tree_memory_mb: u64,
    snapshot_key: Vec<u8>,
    state: Mutex<State>,
    changed: Condvar,
}

/// One kernel: a sequence of Python processes (generations), each replaced after a failure.
pub struct Kernel {
    shared: Arc<Shared>,
}

impl Kernel {
    pub fn new(options: KernelOptions, handler: Arc<dyn HostHandler>) -> Result<Self, KernelError> {
        let limits = kernel_resource_limits(|name| options.var(name), options.limits)?;
        if options.startup_timeout.is_zero() || options.startup_timeout > Duration::from_millis(2_147_483_647) {
            return Err(KernelError::new(
                "RLM startupTimeoutMs must be a positive, finite timer duration",
            ));
        }
        if options.continuity.is_some() && options.snapshot_path.is_none() {
            return Err(KernelError::new("RLM kernel continuity requires a snapshot path"));
        }
        let snapshot_key = options.snapshot_key.clone().unwrap_or_else(process_snapshot_key);
        let tree_memory_mb = match &options.tree_memory {
            Some(tree) => tree_memory::kernel_tree_memory_limit(
                options.var(crate::env::RLM_MAX_TREE_MEMORY_MB_ENV).as_deref(),
                limits.max_memory_mb,
                tree.max_mb,
            )?,
            None => 0,
        };
        Ok(Self {
            shared: Arc::new(Shared {
                options,
                handler,
                limits,
                tree_memory_mb,
                snapshot_key,
                state: Mutex::new(State::default()),
                changed: Condvar::new(),
            }),
        })
    }

    pub fn limits(&self) -> KernelResourceLimits {
        self.shared.limits
    }

    /// Whether a kernel process is up and initialized.
    pub fn is_running(&self) -> bool {
        let state = jsrt::sync::lock(&self.shared.state);
        !state.closed
            && state
                .generation
                .as_ref()
                .is_some_and(|generation| generation.initialized)
    }

    /// The bounded tail of the kernel's raw stderr, for diagnostics.
    pub fn stderr_tail(&self) -> String {
        jsrt::sync::lock(&self.shared.state).stderr.clone()
    }

    /// How many calls (cells, snapshots, checkpoints) are running or queued, in call order.
    pub fn pending_calls(&self) -> usize {
        jsrt::sync::lock(&self.shared.state).queue.len()
    }

    pub fn pid(&self) -> Option<u32> {
        let state = jsrt::sync::lock(&self.shared.state);
        state
            .generation
            .as_ref()
            .and_then(|generation| u32::try_from(generation.pid).ok())
    }

    /// Proportional memory of the kernel's process tree (Linux, read from /proc) and the tree cap in bytes; None while
    /// no kernel process runs (kernel.ts `memoryUsage`, for `rlm.pool`).
    pub fn memory_usage(&self) -> Option<(u64, Option<u64>)> {
        let pid = {
            let state = jsrt::sync::lock(&self.shared.state);
            if state.closed {
                return None;
            }
            state.generation.as_ref()?.pid
        };
        if !cfg!(target_os = "linux") {
            return None;
        }
        let cap = self.shared.tree_memory_mb;
        Some((
            tree_memory::tree_memory_usage(pid).0,
            (cap > 0).then(|| cap * 1024 * 1024),
        ))
    }

    /// How the running kernel's tree cap is enforced; None while no kernel process runs.
    pub fn tree_memory_backend(&self) -> Option<TreeMemoryBackend> {
        let state = jsrt::sync::lock(&self.shared.state);
        state.generation.as_ref().map(|generation| generation.tree_backend)
    }

    /// The sandbox the running kernel is in; None when unsandboxed or not running.
    pub fn sandbox(&self) -> Option<sandbox::Mechanism> {
        let state = jsrt::sync::lock(&self.shared.state);
        state.generation.as_ref().and_then(|generation| generation.sandbox)
    }

    /// Starts the kernel process if none runs and waits until it is ready.
    pub fn start(&self) -> Result<(), KernelError> {
        let mut state = jsrt::sync::lock(&self.shared.state);
        let generation = self.shared.ensure_started(&mut state, true)?;
        self.shared.wait_started(state, generation, None).map(drop)
    }

    /// Runs a cell. `handler`, when given, serves this cell's host requests instead of the kernel's handler. The first
    /// cell after a recovery starts its stderr with the recovery notice ([`ContinuityReport::render`]).
    pub fn execute(
        &self,
        code: &str,
        signal: Option<&Signal>,
        handler: Option<Arc<dyn HostHandler>>,
    ) -> Result<ExecutionResult, KernelError> {
        self.execute_cell(code, signal, handler).map(|reply| reply.result)
    }

    /// [`Kernel::execute`] with the structured recovery report of the first cell after a recovery.
    pub fn execute_cell(
        &self,
        code: &str,
        signal: Option<&Signal>,
        handler: Option<Arc<dyn HostHandler>>,
    ) -> Result<KernelReply, KernelError> {
        let frame = object([("request", Value::from("execute")), ("code", Value::from(code))]);
        self.shared.enqueue(frame, signal, handler)
    }

    /// Clears invocation scratch while keeping the API bindings and the declared `state` dict.
    pub fn reset_scratch(&self) -> Result<ExecutionResult, KernelError> {
        self.shared
            .enqueue(object([("request", Value::from("reset_scratch"))]), None, None)
            .map(|reply| reply.result)
    }

    /// Saves the namespace's restorable names to `path` (default: the configured snapshot path), signed by the host.
    /// The reply's `snapshot` lists saved names and every skipped name with a reason; a failure leaves the previous
    /// file intact.
    pub fn snapshot(&self, path: Option<&Path>) -> Result<KernelReply, KernelError> {
        let path = self.snapshot_target(path)?;
        self.shared.enqueue(
            object([("request", Value::from("snapshot")), ("path", Value::from(path))]),
            None,
            None,
        )
    }

    /// Restores a host-verified snapshot from `path` (default: the configured snapshot path). Unsigned, tampered or
    /// foreign-key snapshots are refused before the kernel reads them, and the namespace stays untouched.
    pub fn restore(&self, path: Option<&Path>) -> Result<KernelReply, KernelError> {
        let path = self.snapshot_target(path)?;
        self.shared.enqueue(
            object([("request", Value::from("restore")), ("path", Value::from(path))]),
            None,
            None,
        )
    }

    fn snapshot_target(&self, path: Option<&Path>) -> Result<String, KernelError> {
        path.or(self.shared.options.snapshot_path.as_deref())
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| KernelError::new("RLM snapshot path is not configured"))
    }

    /// The latest recovery's report, also after it was delivered with a cell.
    pub fn last_recovery(&self) -> Option<ContinuityReport> {
        jsrt::sync::lock(&self.shared.state).continuity.last_recovery.clone()
    }

    /// The latest checkpoint written to the configured snapshot path.
    pub fn checkpoint(&self) -> Option<SnapshotReport> {
        jsrt::sync::lock(&self.shared.state).continuity.checkpoint.clone()
    }

    /// Sends an in-band interrupt frame and `SIGINT` to the currently executing cell without killing the kernel
    /// process, so an awaiting or looping cell can unwind while keeping the kernel PID and namespace alive.
    pub fn interrupt_active(&self) -> bool {
        let (writer, frame, pid) = {
            let state = jsrt::sync::lock(&self.shared.state);
            let Some(generation) = state.generation.as_ref() else {
                return false;
            };
            let Some(cell_id) = generation.active_cell.clone() else {
                return false;
            };
            let frame = object([("request", Value::from("interrupt")), ("id", Value::from(cell_id))]);
            let target_pid = generation.cpu_pid.unwrap_or(generation.pid);
            (generation.writer.clone(), frame, target_pid)
        };
        let mut sent = false;
        if let Ok(line) = encode_frame(&frame)
            && writer.send(line).is_ok()
        {
            sent = true;
        }
        if process::interrupt_process(pid) {
            sent = true;
        }
        sent
    }

    /// Permanently closes this kernel: kills its process tree, rejects every call, and waits (bounded) for the exit.
    pub fn shutdown(&self) {
        let exit = {
            let mut state = jsrt::sync::lock(&self.shared.state);
            state.closed = true;
            let error = KernelError::new(SHUT_DOWN);
            let exit = match state
                .generation
                .as_ref()
                .map(|generation| (generation.id, generation.exit.clone()))
            {
                Some((id, exit)) => {
                    self.shared.fail(&mut state, id, error);
                    Some(exit)
                }
                None => {
                    state.reject_ops(&error);
                    None
                }
            };
            self.shared.changed.notify_all();
            exit
        };
        if let Some(exit) = exit {
            exit.wait_reaped(SHUTDOWN_TIMEOUT);
        }
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Shared {
    fn with_state(&self, run: impl FnOnce(&mut State)) {
        let mut state = jsrt::sync::lock(&self.state);
        run(&mut state);
        drop(state);
        self.changed.notify_all();
    }

    fn enqueue(
        self: &Arc<Self>,
        frame: Map<String, Value>,
        signal: Option<&Signal>,
        handler: Option<Arc<dyn HostHandler>>,
    ) -> Result<KernelReply, KernelError> {
        if jsrt::sync::lock(&self.state).closed {
            return Err(KernelError::new(SHUT_DOWN));
        }
        if let Some(reason) = signal.and_then(Signal::reason) {
            return Err(KernelError::new(reason));
        }
        let mut probe = frame.clone();
        probe.insert("id".into(), Value::from(PLACEHOLDER_ID));
        encode_frame(&probe)?;
        let op = {
            let mut state = jsrt::sync::lock(&self.state);
            if state.closed {
                return Err(KernelError::new(SHUT_DOWN));
            }
            state.next_op += 1;
            let op = state.next_op;
            state.ops.insert(
                op,
                Op {
                    running: false,
                    outcome: None,
                    cell: None,
                    handler,
                    recovery: None,
                },
            );
            state.queue.push_back(op);
            op
        };
        let kind = frame
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let link = signal.map(|signal| {
            let shared: Weak<Self> = Arc::downgrade(self);
            signal.link(move |reason| {
                if let Some(shared) = shared.upgrade() {
                    shared.abort_op(op, reason);
                }
            })
        });
        let mut result = self.run(op, frame);
        drop(link);
        let mut state = jsrt::sync::lock(&self.state);
        let recovery = state.ops.remove(&op).and_then(|entry| entry.recovery);
        if state.queue.front() == Some(&op) {
            state.queue.pop_front();
        }
        match &mut result {
            Ok(reply) => {
                if let Some(report) = recovery {
                    reply.result.stderr = format!("{}{}", report.render(), reply.result.stderr);
                    reply.recovery = Some(report);
                }
                if kind == "execute" || kind == "reset_scratch" {
                    self.cell_completed(&mut state);
                }
            }
            Err(_) => {
                if state.continuity.notice.is_none() {
                    state.continuity.notice = recovery;
                }
            }
        }
        drop(state);
        self.changed.notify_all();
        result
    }

    /// Continuity: a cell may have changed the namespace; checkpoint now, or once the debounce interval has passed.
    fn cell_completed(self: &Arc<Self>, state: &mut State) {
        let Some(policy) = self.options.continuity else {
            return;
        };
        let continuity = &mut state.continuity;
        continuity.cells += 1;
        continuity.dirty = true;
        if continuity.snapshot_queued || state.closed {
            return;
        }
        let due = continuity
            .last_checkpoint
            .map(|last| last + policy.min_interval)
            .filter(|due| *due > Instant::now());
        match due {
            None => self.queue_checkpoint(state, true),
            Some(due) if !continuity.idle_flush_scheduled => {
                continuity.idle_flush_scheduled = true;
                let shared = Arc::downgrade(self);
                std::thread::spawn(move || {
                    std::thread::sleep(due.saturating_duration_since(Instant::now()));
                    let Some(shared) = shared.upgrade() else {
                        return;
                    };
                    let mut state = jsrt::sync::lock(&shared.state);
                    state.continuity.idle_flush_scheduled = false;
                    if state.continuity.dirty && !state.continuity.snapshot_queued && !state.closed {
                        shared.queue_checkpoint(&mut state, false);
                    }
                });
            }
            Some(_) => {}
        }
    }

    /// Queues a checkpoint of the configured snapshot path, at the front of the queue or at its end, and runs it on
    /// its own thread so the cell that triggered it returns at once.
    fn queue_checkpoint(self: &Arc<Self>, state: &mut State, front: bool) {
        let Some(path) = self.options.snapshot_path.as_ref() else {
            return;
        };
        state.next_op += 1;
        let op = state.next_op;
        state.ops.insert(
            op,
            Op {
                running: false,
                outcome: None,
                cell: None,
                handler: None,
                recovery: None,
            },
        );
        if front {
            state.queue.push_front(op);
        } else {
            state.queue.push_back(op);
        }
        state.continuity.snapshot_queued = true;
        let frame = object([
            ("request", Value::from("snapshot")),
            ("path", Value::from(path.to_string_lossy().into_owned())),
        ]);
        let shared = self.clone();
        std::thread::spawn(move || {
            let _ = shared.run(op, frame);
            let mut state = jsrt::sync::lock(&shared.state);
            state.ops.remove(&op);
            if state.queue.front() == Some(&op) {
                state.queue.pop_front();
            }
            state.continuity.snapshot_queued = false;
            drop(state);
            shared.changed.notify_all();
        });
    }

    fn abort_op(&self, op: u64, reason: String) {
        self.with_state(|state| {
            let error = KernelError::new(reason);
            let Some(entry) = state.ops.get_mut(&op) else {
                return;
            };
            if entry.outcome.is_some() {
                return;
            }
            if !entry.running {
                entry.outcome = Some(Err(error));
                state.queue.retain(|queued| *queued != op);
                return;
            }
            if let Some(cell) = &entry.cell {
                cell.abort(error.message());
            }
            if self.options.continuity.is_none() {
                state.no_auto_restore = true;
            }
            match state.generation.as_ref().map(|generation| generation.id) {
                Some(generation) => self.fail(state, generation, error),
                None => entry_reject(state, op, error),
            }
        });
    }

    fn take_outcome(state: &mut State, op: u64) -> Option<Result<KernelReply, KernelError>> {
        state.ops.get_mut(&op).and_then(|entry| entry.outcome.take())
    }

    fn run(self: &Arc<Self>, op: u64, frame: Map<String, Value>) -> Result<KernelReply, KernelError> {
        let mut state = jsrt::sync::lock(&self.state);
        loop {
            if let Some(outcome) = Self::take_outcome(&mut state, op) {
                return outcome;
            }
            if state.queue.front() == Some(&op) {
                break;
            }
            state = self.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        if let Some(entry) = state.ops.get_mut(&op) {
            entry.running = true;
        }
        let kind = frame
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let generation = self.ensure_started(&mut state, kind != "restore")?;
        let state = self.wait_started(state, generation, Some(op))?;
        drop(state);
        let path = frame.get("path").and_then(Value::as_str).unwrap_or_default();
        match kind.as_str() {
            "snapshot" => self.signed_snapshot(op, generation, Path::new(path)),
            "restore" => self.verified_restore(op, generation, Path::new(path)),
            _ => self.request(op, generation, frame),
        }
    }

    /// Sends one request of call `op` to `generation` and waits for its done frame.
    fn request(
        self: &Arc<Self>,
        op: u64,
        generation: u64,
        mut frame: Map<String, Value>,
    ) -> Result<KernelReply, KernelError> {
        let mut state = jsrt::sync::lock(&self.state);
        let execute = frame.get("request") == Some(&Value::from("execute"));
        let request_id = random_uuid()?;
        frame.insert("id".into(), Value::from(request_id.clone()));
        let line = encode_frame(&frame)?;
        let handler = state.ops.get(&op).and_then(|entry| entry.handler.clone());
        let controller = Signal::new();
        let Some(current) = state.current(generation) else {
            return Err(Self::take_outcome(&mut state, op)
                .and_then(Result::err)
                .unwrap_or_else(|| KernelError::new(NOT_RUNNING)));
        };
        let state_ok = KernelReply::from_result(ExecutionResult {
            status: Status::Ok,
            stdout: String::new(),
            stderr: String::new(),
            result: None,
            error: None,
        });
        current.pending.insert(
            request_id.clone(),
            Waiter {
                op,
                state: state_ok,
                controller: controller.clone(),
                handler,
            },
        );
        if execute {
            if current.cpu_pid.is_none() {
                current.cpu_pid = sandboxed::python_pid(current.pid, &self.python(), &self.options.runtime_path);
            }
            current.cell_cpu_start = current.cpu_pid.and_then(process::process_cpu_seconds);
            current.active_cell = Some(request_id.clone());
            take_raw_output(current, &request_id);
            let notice = state.continuity.notice.take();
            if let Some(entry) = state.ops.get_mut(&op) {
                entry.cell = Some(controller);
                entry.recovery = notice;
            }
        }
        if let Err(error) = self.send(&mut state, generation, line) {
            if let Some(current) = state.current(generation) {
                current.pending.remove(&request_id);
                if current.active_cell.as_deref() == Some(request_id.as_str()) {
                    current.active_cell = None;
                }
            }
            entry_reject(&mut state, op, error.clone());
            self.fail(&mut state, generation, error);
        }
        let mut cpu_checked = Instant::now();
        loop {
            if let Some(outcome) = Self::take_outcome(&mut state, op) {
                return outcome;
            }
            let now = Instant::now();
            let mut timeout = WATCHDOG_INTERVAL;
            if let Some(current) = state.current(generation) {
                if !current.initialized {
                    let deadline = current.startup_deadline;
                    if deadline <= now {
                        let error = self.startup_timeout(&state);
                        self.fail(&mut state, generation, error);
                        continue;
                    }
                    timeout = timeout.min(deadline - now);
                }
                let Some(current) = state.current(generation) else {
                    continue;
                };
                if let Some(deadline) = current.output.as_ref().and_then(|output| output.deadline) {
                    if deadline <= now {
                        finish_cell(&mut state, generation);
                        continue;
                    }
                    timeout = timeout.min(deadline - now);
                }
                if now.duration_since(cpu_checked) >= WATCHDOG_INTERVAL {
                    cpu_checked = now;
                    if self.cpu_exceeded(current) {
                        let error = KernelError::new(cpu_limit_message(&self.limits));
                        self.fail(&mut state, generation, error);
                        continue;
                    }
                }
            }
            state = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}
