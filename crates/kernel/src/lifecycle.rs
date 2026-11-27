//! Kernel process spawning, startup, resource-limit enforcement, checkpoints, and failure recovery.

use super::*;

type RegisteredGeneration = (u64, mpsc::Receiver<String>, Arc<Exit>, Arc<Mutex<HashMap<i32, u64>>>);

impl Shared {
    pub(super) fn cpu_exceeded(&self, generation: &Generation) -> bool {
        let limit = self.limits.max_cpu_seconds as f64;
        if limit <= 0.0 || generation.active_cell.is_none() {
            return false;
        }
        let Some(start) = generation.cell_cpu_start else {
            return false;
        };
        let grace = (limit * 0.1).clamp(2.0, 10.0);
        generation
            .cpu_pid
            .and_then(process::process_cpu_seconds)
            .is_some_and(|used| used - start > limit + grace)
    }

    /// Whether the tree cap has been hit: for a cgroup scope, an OOM kill in its memory.events since the runtime came
    /// up; for the watchdog, the summed memory of the kernel's tree is over the cap.
    pub(super) fn tree_memory_exceeded(&self, generation: &mut Generation) -> bool {
        match generation.tree_backend {
            TreeMemoryBackend::Off => false,
            TreeMemoryBackend::Watchdog => {
                watchdog_tree_bytes(generation.pid, &generation.known_descendants) > self.tree_memory_mb * 1024 * 1024
            }
            TreeMemoryBackend::Cgroup => {
                if generation.cgroup_dir.is_none() {
                    let dir = tree_memory::process_cgroup_dir(generation.pid)
                        .filter(|dir| tree_memory::is_foreign_scope(dir));
                    let Some(dir) = dir.filter(|_| generation.ready_received) else {
                        return false;
                    };
                    generation.oom_kills_at_start = tree_memory::cgroup_oom_kills(&dir).unwrap_or(0);
                    generation.cgroup_dir = Some(dir);
                }
                generation
                    .cgroup_dir
                    .as_deref()
                    .and_then(tree_memory::cgroup_oom_kills)
                    .is_some_and(|kills| kills > generation.oom_kills_at_start)
            }
        }
    }

    /// Polls the tree cap and kills the whole tree (through `fail`) when it is exceeded: the scope's memory.events
    /// every 500 ms (the kernel enforces MemoryMax itself); the watchdog faster as the tree nears the cap. The tree is
    /// measured outside the state lock; the poll ends with the process or once another generation runs.
    pub(super) fn start_tree_memory_watchdog(
        self: &Arc<Self>,
        id: u64,
        pid: i32,
        backend: TreeMemoryBackend,
        known: Arc<Mutex<HashMap<i32, u64>>>,
        exit: Arc<Exit>,
    ) {
        let shared = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut delay = Duration::from_millis(500);
            loop {
                if exit.wait_reaped(delay) {
                    return;
                }
                let Some(shared) = shared.upgrade() else {
                    return;
                };
                let cap_bytes = shared.tree_memory_mb * 1024 * 1024;
                let bytes = (backend == TreeMemoryBackend::Watchdog).then(|| watchdog_tree_bytes(pid, &known));
                let mut state = jsrt::sync::lock(&shared.state);
                let Some(current) = state.current(id) else {
                    return;
                };
                let exceeded = match bytes {
                    Some(bytes) => {
                        delay = Duration::from_millis(tree_memory::watchdog_poll_ms(bytes, cap_bytes));
                        bytes > cap_bytes
                    }
                    None => shared.tree_memory_exceeded(current),
                };
                if exceeded {
                    let message = tree_memory::tree_memory_limit_message(shared.tree_memory_mb, backend);
                    shared.fail(&mut state, id, KernelError::new(message));
                    drop(state);
                    shared.changed.notify_all();
                    return;
                }
            }
        });
    }

    /// Waits until `generation` is initialized, its startup times out, or it (or the call `op`) fails.
    pub(super) fn wait_started<'a>(
        self: &'a Arc<Self>,
        mut state: MutexGuard<'a, State>,
        generation: u64,
        op: Option<u64>,
    ) -> Result<MutexGuard<'a, State>, KernelError> {
        loop {
            if let Some(op) = op
                && let Some(outcome) = Self::take_outcome(&mut state, op)
            {
                return Err(outcome.err().unwrap_or_else(|| KernelError::new(NOT_RUNNING)));
            }
            let Some(current) = state.current(generation) else {
                return Err(match &state.failure {
                    Some((failed, error)) if *failed == generation => error.clone(),
                    _ => KernelError::new(NOT_RUNNING),
                });
            };
            if current.initialized {
                return Ok(state);
            }
            if current.ready_received && !current.completing {
                current.completing = true;
                let restore_path = current.restore_path.clone();
                drop(state);
                self.complete_startup(generation, restore_path);
                state = jsrt::sync::lock(&self.state);
                continue;
            }
            let deadline = current.startup_deadline;
            let now = Instant::now();
            if now >= deadline {
                let error = self.startup_timeout(&state);
                self.fail(&mut state, generation, error);
                continue;
            }
            state = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    pub(super) fn startup_timeout(&self, state: &State) -> KernelError {
        KernelError::new(format!(
            "RLM kernel startup timed out after {}ms{}",
            self.options.startup_timeout.as_millis(),
            state.diagnostics()
        ))
    }

    /// The running generation, or a new one; `restore` lets a new one restore the configured snapshot.
    pub(super) fn ensure_started(self: &Arc<Self>, state: &mut State, restore: bool) -> Result<u64, KernelError> {
        if state.closed {
            return Err(KernelError::new(SHUT_DOWN));
        }
        if let Some(generation) = &state.generation {
            return Ok(generation.id);
        }
        self.spawn(state, restore)
    }

    fn run_startup_restore(self: &Arc<Self>, generation: u64, path: &Path) -> Result<KernelReply, KernelError> {
        let op = {
            let mut state = jsrt::sync::lock(&self.state);
            state.next_op += 1;
            let op = state.next_op;
            state.ops.insert(
                op,
                Op {
                    running: true,
                    outcome: None,
                    cell: None,
                    handler: None,
                    recovery: None,
                },
            );
            op
        };
        let outcome = self.verified_restore(op, generation, path);
        jsrt::sync::lock(&self.state).ops.remove(&op);
        outcome
    }

    fn record_startup_recovery(&self, state: &mut State, restore: Result<Option<RestoreReport>, String>) {
        let crash = state.continuity.crash.take();
        let restored_something = matches!(&restore, Ok(Some(report)) if !report.missing);
        if crash.is_none() && !restored_something && restore.is_ok() {
            return;
        }
        let crash = crash.unwrap_or_else(|| Crash {
            reason: "kernel started from its checkpoint".into(),
            interrupted_cell: false,
            unsaved_cells: 0,
            restore_error: None,
        });
        let path = self
            .options
            .snapshot_path
            .as_ref()
            .map_or_default(|path| path.display().to_string());
        let report = recovery_report(crash, path, restore, state.continuity.checkpoint.as_ref());
        state.continuity.notice = Some(report.clone());
        state.continuity.last_recovery = Some(report);
    }

    /// Completes a generation's startup after `ready`: restores `restore_path` (kernel.ts `completeStartup`) and, with
    /// continuity, prepares the recovery report for the next cell.
    pub(super) fn complete_startup(self: &Arc<Self>, generation: u64, restore_path: Option<PathBuf>) {
        let restored = restore_path
            .as_ref()
            .map(|path| self.run_startup_restore(generation, path));
        let mut state = jsrt::sync::lock(&self.state);
        if state.current(generation).is_none() {
            return;
        }
        let skipped_restore = state
            .continuity
            .crash
            .as_ref()
            .and_then(|crash| crash.restore_error.clone());
        let restore = match interpret_startup_restore(restored, skipped_restore) {
            Ok(restore) => restore,
            Err(error) => {
                self.fail(&mut state, generation, error);
                drop(state);
                self.changed.notify_all();
                return;
            }
        };
        let continuity = self.options.continuity.is_some();
        if let Err(message) = &restore
            && !continuity
        {
            let error = KernelError::new(format!("RLM kernel startup restore failed: {message}"));
            self.fail(&mut state, generation, error);
            drop(state);
            self.changed.notify_all();
            return;
        }
        if continuity {
            self.record_startup_recovery(&mut state, restore);
        }
        if let Some(current) = state.current(generation) {
            current.initialized = true;
        }
        drop(state);
        self.changed.notify_all();
    }

    fn verify_and_write_signed_snapshot(
        &self,
        unsigned: &Path,
        path: &Path,
        reported_sha: Option<&str>,
    ) -> Result<(), String> {
        let limit = snapshot::max_snapshot_bytes();
        if limit > 0
            && let Ok(meta) = std::fs::metadata(unsigned)
            && meta.len() > limit
        {
            return Err(format!(
                "snapshot skipped: {} bytes exceeds limit of {} bytes ({})",
                meta.len(),
                limit,
                snapshot::ENV_MAX_SNAPSHOT_BYTES
            ));
        }
        let content = std::fs::read(unsigned).map_err(|error| format!("snapshot signing failed: {error}"))?;
        if reported_sha != Some(jsrt::hash::sha256_hex(&content).as_str()) {
            return Err("snapshot changed after the kernel wrote it; refusing to sign it".into());
        }
        jsrt::fs::atomic_write_mode(path, sign_snapshot(&self.snapshot_key, &content), 0o600)
            .map_err(|error| format!("snapshot signing failed: {error}"))
    }

    fn record_checkpoint(&self, path: &Path, report: Option<&SnapshotReport>) {
        if self.options.snapshot_path.as_deref() != Some(path) {
            return;
        }
        let mut state = jsrt::sync::lock(&self.state);
        let continuity = &mut state.continuity;
        continuity.checkpoint = report.cloned();
        continuity.checkpoint_cells = continuity.cells;
        continuity.last_checkpoint = Some(Instant::now());
        continuity.dirty = false;
        if self.options.continuity.is_some() {
            state.no_auto_restore = false;
        }
    }

    fn finalize_signed_snapshot(&self, reply: KernelReply, unsigned: &Path, path: &Path) -> KernelReply {
        if reply.result.status != Status::Ok {
            return reply;
        }
        let reported = reply
            .snapshot
            .as_ref()
            .and_then(|report| report.content_sha256.as_deref());
        if let Err(error) = self.verify_and_write_signed_snapshot(unsigned, path, reported) {
            return KernelReply::from_result(snapshot::snapshot_error(error));
        }
        self.record_checkpoint(path, reply.snapshot.as_ref());
        reply
    }

    /// kernel.ts `signedSnapshot`: the kernel writes an unsigned temporary file; the host checks it is exactly what
    /// the kernel reported, signs it, and atomically replaces `path`. A failure leaves the previous snapshot intact.
    pub(super) fn signed_snapshot(
        self: &Arc<Self>,
        op: u64,
        generation: u64,
        path: &Path,
    ) -> Result<KernelReply, KernelError> {
        let directory = path.parent().unwrap_or(Path::new(""));
        let name = path
            .file_name()
            .map_or_default(|name| name.to_string_lossy().into_owned());
        let unsigned = directory.join(format!(".{name}.{}.unsigned", random_uuid()?));
        let frame = object([
            ("request", Value::from("snapshot")),
            ("path", Value::from(unsigned.to_string_lossy().into_owned())),
        ]);
        let outcome = self
            .request(op, generation, frame)
            .map(|reply| self.finalize_signed_snapshot(reply, &unsigned, path));
        let _ = std::fs::remove_file(&unsigned);
        outcome
    }

    /// kernel.ts `verifiedRestore`: refuses unsigned or tampered snapshots before the kernel reads them; the namespace
    /// stays untouched. The kernel re-hashes what it reads, so a file swapped after this check is refused too.
    pub(super) fn verified_restore(
        self: &Arc<Self>,
        op: u64,
        generation: u64,
        path: &Path,
    ) -> Result<KernelReply, KernelError> {
        let shown = path.to_string_lossy().into_owned();
        let raw = match std::fs::read(path) {
            Ok(raw) => raw,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                let frame = object([("request", Value::from("restore")), ("path", Value::from(shown))]);
                return self.request(op, generation, frame);
            }
            Err(error) => {
                return Ok(KernelReply::from_result(snapshot::snapshot_error(format!(
                    "snapshot cannot be read: {error}"
                ))));
            }
        };
        match verify_snapshot(&self.snapshot_key, &raw) {
            Err(error) => Ok(KernelReply::from_result(snapshot::snapshot_error(error.0))),
            Ok(content_sha256) => {
                let frame = object([
                    ("request", Value::from("restore")),
                    ("path", Value::from(shown)),
                    ("content_sha256", Value::from(content_sha256)),
                ]);
                self.request(op, generation, frame)
            }
        }
    }

    pub(super) fn python(&self) -> PathBuf {
        self.options
            .python
            .clone()
            .unwrap_or_else(|| crate::env::python(|name| self.options.var(name)).into())
    }

    fn resolve_tree_backend(&self, mechanism: Option<sandbox::Mechanism>) -> TreeMemoryBackend {
        let Some(tree) = &self.options.tree_memory else {
            return TreeMemoryBackend::Off;
        };
        let requested = tree
            .backend
            .clone()
            .or_else(|| self.options.var(crate::env::RLM_TREE_MEMORY_BACKEND_ENV));
        tree_memory::tree_memory_backend(self.tree_memory_mb, requested.as_deref(), || {
            mechanism.is_none() && tree_memory::cgroup_scope_available(tree.launcher.as_os_str())
        })
    }

    fn build_direct_command(
        &self,
        argv: &[std::ffi::OsString],
        tree_backend: TreeMemoryBackend,
        fixed_env: Vec<(String, String)>,
    ) -> Command {
        let options = &self.options;
        let mut command = match (&options.tree_memory, tree_backend) {
            (Some(tree), TreeMemoryBackend::Cgroup) => {
                let mut command = Command::new(&tree.launcher);
                command
                    .args(&tree_memory::cgroup_scope_command(self.tree_memory_mb)[1..])
                    .args(argv);
                command
            }
            _ => {
                let mut command = Command::new(&argv[0]);
                command.args(&argv[1..]);
                command
            }
        };
        command
            .current_dir(&options.cwd)
            .env_clear()
            .envs(kernel_environment(options.base_environment()))
            .envs(fixed_env)
            .envs(options.env.iter().map(|(name, value)| (name, value)));
        command
    }

    fn build_spawn_command(
        &self,
        python: &Path,
        mechanism: Option<sandbox::Mechanism>,
        tree_backend: TreeMemoryBackend,
    ) -> Command {
        let options = &self.options;
        let argv: Vec<std::ffi::OsString> = vec![
            python.to_path_buf().into(),
            options.runtime_path.clone().into(),
            "--protocol-fds".into(),
        ];
        let budget = rlm_output_budget(options.var(crate::env::RLM_OUTPUT_BYTES_ENV).as_deref());
        let fixed_env: Vec<(String, String)> = vec![
            ("NO_COLOR".into(), "1".into()),
            ("PYTHONDONTWRITEBYTECODE".into(), "1".into()),
            (crate::env::IN_KERNEL_ENV.into(), "1".into()),
            (
                crate::env::RLM_MAX_MEMORY_MB_ENV.into(),
                self.limits.max_memory_mb.to_string(),
            ),
            (
                crate::env::RLM_MAX_CPU_SECONDS_ENV.into(),
                self.limits.max_cpu_seconds.to_string(),
            ),
            (crate::env::RLM_OUTPUT_BYTES_ENV.into(), budget.to_string()),
        ];
        let mut command = match (mechanism, &options.sandbox) {
            (Some(mechanism), Some(launcher)) => sandboxed::command(sandboxed::Launch {
                launcher,
                mechanism,
                python,
                argv,
                cwd: &options.cwd,
                runtime: &options.runtime_path,
                snapshot_path: options.snapshot_path.as_deref(),
                fixed_env,
                extra_env: &options.env,
                base_env: options.base_environment(),
            }),
            _ => self.build_direct_command(&argv, tree_backend, fixed_env),
        };
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn spawn_protocol_writer(
        self: &Arc<Self>,
        id: u64,
        mut protocol_in: std::io::PipeWriter,
        lines: mpsc::Receiver<String>,
    ) {
        let shared = self.clone();
        std::thread::spawn(move || {
            for line in lines {
                if let Err(error) = protocol_in
                    .write_all(line.as_bytes())
                    .and_then(|()| protocol_in.flush())
                {
                    shared.with_state(|state| shared.fail(state, id, KernelError::new(error.to_string())));
                    return;
                }
            }
        });
    }

    fn spawn_protocol_reader(self: &Arc<Self>, id: u64, protocol_out: std::io::PipeReader, exit: Arc<Exit>) {
        let shared = self.clone();
        std::thread::spawn(move || {
            if let Err(error) = read_chunks(protocol_out, |chunk| {
                shared.with_state(|state| shared.handle_data(state, id, chunk));
            }) {
                shared.with_state(|state| shared.fail(state, id, KernelError::new(error.to_string())));
                return;
            }
            if !exit.wait_reaped(CLOSED_CHANNEL_GRACE) {
                shared.with_state(|state| {
                    let error = KernelError::new(format!(
                        "RLM kernel protocol channel (fd 3) closed{}",
                        state.diagnostics()
                    ));
                    shared.fail(state, id, error);
                });
            }
        });
    }

    fn spawn_raw_readers(
        self: &Arc<Self>,
        id: u64,
        raw_stdout: Option<std::process::ChildStdout>,
        stderr: Option<std::process::ChildStderr>,
    ) {
        if let Some(stdout) = raw_stdout {
            let shared = self.clone();
            std::thread::spawn(move || {
                let _ = read_chunks(stdout, |chunk| {
                    shared.with_state(|state| handle_raw(state, id, 0, chunk));
                });
            });
        }
        if let Some(stderr) = stderr {
            let shared = self.clone();
            std::thread::spawn(move || {
                let _ = read_chunks(stderr, |chunk| {
                    shared.with_state(|state| {
                        if state.current(id).is_none() {
                            return;
                        }
                        handle_raw(state, id, 1, chunk);
                    });
                });
            });
        }
    }

    fn handle_child_exit(&self, state: &mut State, id: u64, status: &std::io::Result<ExitStatus>) {
        let Some(current) = state.current(id) else {
            return;
        };
        let killed = status
            .as_ref()
            .ok()
            .and_then(std::os::unix::process::ExitStatusExt::signal)
            .is_some_and(|signal| signal == SIGKILL || signal == SIGTERM);
        let tree_kill = if killed {
            self.tree_memory_exceeded(current).then_some(current.tree_backend)
        } else {
            None
        };
        let error = match tree_kill {
            Some(backend) => KernelError::new(tree_memory::tree_memory_limit_message(self.tree_memory_mb, backend)),
            None => self.exit_error(state, status.as_ref().ok().copied()),
        };
        self.fail(state, id, error);
    }

    fn maybe_restart_after_exit(self: &Arc<Self>) {
        let mut state = jsrt::sync::lock(&self.state);
        if std::mem::take(&mut state.continuity.restart)
            && !state.closed
            && state.generation.is_none()
            && let Ok(generation) = self.ensure_started(&mut state, true)
        {
            drop(self.wait_started(state, generation, None));
        }
    }

    fn spawn_exit_reaper(self: &Arc<Self>, id: u64, pid: i32, mut child: std::process::Child, exit: Arc<Exit>) {
        let shared = self.clone();
        std::thread::spawn(move || {
            wait_for_pid_exit(pid);
            shared.with_state(|_| jsrt::sync::lock(&exit.state).0 = true);
            let status = child.wait();
            jsrt::sync::lock(&exit.state).1 =
                Some(status.as_ref().copied().unwrap_or_else(|_| ExitStatus::from_raw(0)));
            exit.changed.notify_all();
            shared.with_state(|state| shared.handle_child_exit(state, id, &status));
            shared.maybe_restart_after_exit();
        });
    }

    fn resolve_sandbox_mechanism(&self) -> Result<Option<sandbox::Mechanism>, KernelError> {
        match &self.options.sandbox {
            Some(launcher) => sandboxed::mechanism(launcher),
            None => Ok(None),
        }
    }

    fn register_generation(
        &self,
        state: &mut State,
        pid: i32,
        mechanism: Option<sandbox::Mechanism>,
        tree_backend: TreeMemoryBackend,
        restore: bool,
    ) -> RegisteredGeneration {
        let options = &self.options;
        state.next_generation += 1;
        let id = state.next_generation;
        let restore_path = (restore && !state.no_auto_restore)
            .then(|| options.snapshot_path.clone())
            .flatten();
        let host_completes_startup =
            restore_path.is_some() || (options.continuity.is_some() && state.continuity.crash.is_some());
        let (writer, lines) = mpsc::channel::<String>();
        let exit = Arc::new(Exit::default());
        let known_descendants: Arc<Mutex<HashMap<i32, u64>>> = Arc::default();
        state.generation = Some(Generation {
            id,
            pid,
            tree_backend,
            cgroup_dir: None,
            oom_kills_at_start: 0,
            known_descendants: known_descendants.clone(),
            writer,
            exit: exit.clone(),
            raw: Default::default(),
            output: None,
            pending: HashMap::new(),
            active_cell: None,
            host_requests: HashMap::new(),
            ready_received: false,
            initialized: false,
            startup_deadline: Instant::now() + options.startup_timeout,
            cell_cpu_start: None,
            cpu_pid: mechanism.is_none().then_some(pid),
            sandbox: mechanism,
            buffer: Vec::new(),
            restore_path,
            host_completes_startup,
            completing: false,
        });
        state.stderr.clear();
        (id, lines, exit, known_descendants)
    }

    pub(super) fn spawn(self: &Arc<Self>, state: &mut State, restore: bool) -> Result<u64, KernelError> {
        let python = self.python();
        let mechanism = self.resolve_sandbox_mechanism()?;
        let tree_backend = self.resolve_tree_backend(mechanism);
        let mut command = self.build_spawn_command(&python, mechanism, tree_backend);
        let (protocol_out, protocol_in) = attach_protocol_pipes(&mut command)?;
        let mut child = spawn_child(command, mechanism)?;
        let pid = i32::try_from(child.id()).map_err(|error| KernelError::new(error.to_string()))?;
        let stderr = child.stderr.take();
        let raw_stdout = child.stdout.take();
        let (id, lines, exit, known_descendants) =
            self.register_generation(state, pid, mechanism, tree_backend, restore);
        self.spawn_protocol_writer(id, protocol_in, lines);
        self.spawn_protocol_reader(id, protocol_out, exit.clone());
        self.spawn_raw_readers(id, raw_stdout, stderr);
        if tree_backend != TreeMemoryBackend::Off {
            self.start_tree_memory_watchdog(id, pid, tree_backend, known_descendants, exit.clone());
        }
        self.spawn_exit_reaper(id, pid, child, exit);
        Ok(id)
    }

    pub(super) fn exit_error(&self, state: &State, status: Option<ExitStatus>) -> KernelError {
        let mut code = status.and_then(|status| status.code());
        let mut signal = status.and_then(|status| status.signal());
        if state.generation.as_ref().and_then(|generation| generation.sandbox) == Some(sandbox::Mechanism::Bwrap)
            && let Some(number) = code.filter(|code| (129..=192).contains(code))
        {
            (code, signal) = (None, Some(number - 128));
        }
        if code == Some(EXIT_MEMORY) {
            return KernelError::new(memory_limit_message(&self.limits));
        }
        if code == Some(EXIT_CPU) || signal == Some(SIGXCPU) {
            return KernelError::new(cpu_limit_message(&self.limits));
        }
        let hint = if signal == Some(SIGKILL) {
            "; the system out-of-memory killer may have stopped it"
        } else {
            ""
        };
        let how = match (signal, code) {
            (Some(signal), _) => jsrt::process::signal_name(signal),
            (None, Some(code)) => code.to_string(),
            (None, None) => "unknown".into(),
        };
        KernelError::new(format!("RLM kernel exited ({how}){hint}{}", state.diagnostics()))
    }

    fn record_failure_continuity(&self, state: &mut State, failed: &Generation, error: &KernelError) {
        if self.options.continuity.is_none() {
            if failed.initialized {
                state.no_auto_restore = true;
            }
            return;
        }
        if state.closed {
            return;
        }
        update_crash_state(state, failed, error);
        if failed.completing && !failed.initialized && failed.restore_path.is_some() {
            state.no_auto_restore = true;
            if let Some(crash) = state.continuity.crash.as_mut() {
                crash.restore_error = Some(format!("the kernel failed while restoring: {}", error.message()));
            }
        }
        state.continuity.restart = failed.initialized;
    }

    /// Ends `generation` with `error`: rejects every call, aborts its cells' host work, and kills its process tree.
    pub(super) fn fail(&self, state: &mut State, generation: u64, error: KernelError) {
        if state.current(generation).is_none() {
            return;
        }
        let Some(failed) = state.generation.take() else {
            return;
        };
        state.failure = Some((generation, error.clone()));
        self.record_failure_continuity(state, &failed, &error);
        for waiter in failed.pending.values() {
            waiter.controller.abort(error.message());
        }
        if let Some(waiter) = failed.output.as_ref().and_then(|output| output.finished.as_ref()) {
            waiter.controller.abort(error.message());
        }
        state.reject_ops(&error);
        let known = jsrt::sync::lock(&failed.known_descendants).clone();
        process::kill_process_tree(
            failed.pid,
            true,
            !failed.exit.exited(),
            process::KillExtras {
                known: Some(&known),
                cgroup_dir: failed.cgroup_dir.as_deref(),
            },
        );
    }
}

fn interpret_startup_restore(
    restored: Option<Result<KernelReply, KernelError>>,
    skipped_restore: Option<String>,
) -> Result<Result<Option<RestoreReport>, String>, KernelError> {
    match restored {
        None => Ok(skipped_restore.map_or(Ok(None), Err)),
        Some(Ok(reply)) if reply.result.status == Status::Error => Ok(Err(reply
            .result
            .error
            .map_or_else(|| "unknown restore error".to_string(), |error| error.evalue))),
        Some(Ok(reply)) => Ok(Ok(reply.restore)),
        Some(Err(error)) => Err(error),
    }
}

fn spawn_error(error: std::io::Error) -> KernelError {
    KernelError::new(format!("RLM kernel spawn failed: {error}"))
}

fn attach_protocol_pipes(command: &mut Command) -> Result<(std::io::PipeReader, std::io::PipeWriter), KernelError> {
    let (protocol_out, kernel_out) = std::io::pipe().map_err(spawn_error)?;
    let (kernel_in, protocol_in) = std::io::pipe().map_err(spawn_error)?;
    command
        .fd_mappings(vec![
            FdMapping {
                parent_fd: kernel_out.into(),
                child_fd: 3,
            },
            FdMapping {
                parent_fd: kernel_in.into(),
                child_fd: 4,
            },
        ])
        .map_err(|error| KernelError::new(error.to_string()))?;
    Ok((protocol_out, protocol_in))
}

fn spawn_child(
    mut command: Command,
    mechanism: Option<sandbox::Mechanism>,
) -> Result<std::process::Child, KernelError> {
    if mechanism.is_some() {
        return sandboxed::spawn(command).map_err(spawn_error);
    }
    let child = command.spawn().map_err(spawn_error)?;
    drop(command);
    Ok(child)
}

fn wait_for_pid_exit(pid: i32) {
    if let Some(process) = rustix::process::Pid::from_raw(pid) {
        while let Err(rustix::io::Errno::INTR) =
            rustix::process::waitid(WaitId::Pid(process), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT)
        {}
    }
}

fn update_crash_state(state: &mut State, failed: &Generation, error: &KernelError) {
    let continuity = &mut state.continuity;
    let unsaved_cells = continuity.cells - continuity.checkpoint_cells;
    let interrupted_cell = failed.active_cell.is_some();
    match continuity.crash.as_mut() {
        Some(crash) => {
            crash.interrupted_cell |= interrupted_cell;
            crash.unsaved_cells = unsaved_cells;
        }
        None => {
            continuity.crash = Some(Crash {
                reason: error.message().to_string(),
                interrupted_cell,
                unsaved_cells,
                restore_error: None,
            });
        }
    }
}

/// The watchdog's measure of `root`'s tree; remembers the descendants it saw (pid to start time) so they are killed
/// with the tree even after they left it.
fn watchdog_tree_bytes(root: i32, known: &Mutex<HashMap<i32, u64>>) -> u64 {
    let (bytes, descendants) = tree_memory::tree_memory_usage(root);
    let mut known = jsrt::sync::lock(known);
    known.retain(|pid, start| process::process_stat(*pid).is_some_and(|stat| stat.start_time == *start));
    for pid in descendants {
        if let Some(stat) = process::process_stat(pid) {
            known.insert(pid, stat.start_time);
        }
    }
    bytes
}
