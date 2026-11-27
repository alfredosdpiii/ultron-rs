//! Kernel request queue, cancellation, checkpoint scheduling, and execution waiting.

use super::*;

impl Shared {
    pub(super) fn with_state(&self, run: impl FnOnce(&mut State)) {
        let mut state = jsrt::sync::lock(&self.state);
        run(&mut state);
        drop(state);
        self.changed.notify_all();
    }

    fn register_op(&self, handler: Option<Arc<dyn HostHandler>>) -> Result<u64, KernelError> {
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
        Ok(op)
    }

    fn link_abort_signal(self: &Arc<Self>, op: u64, signal: Option<&Signal>) -> Option<Link> {
        signal.map(|signal| {
            let shared: Weak<Self> = Arc::downgrade(self);
            signal.link(move |reason| {
                if let Some(shared) = shared.upgrade() {
                    shared.abort_op(op, reason);
                }
            })
        })
    }

    fn finish_enqueued_op(self: &Arc<Self>, op: u64, kind: &str, result: &mut Result<KernelReply, KernelError>) {
        let mut state = jsrt::sync::lock(&self.state);
        let recovery = state.ops.remove(&op).and_then(|entry| entry.recovery);
        if state.queue.front() == Some(&op) {
            state.queue.pop_front();
        }
        match result {
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
    }

    pub(super) fn enqueue(
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
        let op = self.register_op(handler)?;
        let kind = frame
            .get("request")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let link = self.link_abort_signal(op, signal);
        let mut result = self.run(op, frame);
        drop(link);
        self.finish_enqueued_op(op, &kind, &mut result);
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

    pub(super) fn take_outcome(state: &mut State, op: u64) -> Option<Result<KernelReply, KernelError>> {
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

    fn register_request(
        &self,
        state: &mut State,
        op: u64,
        generation: u64,
        request_id: &str,
        execute: bool,
    ) -> Result<(), KernelError> {
        let handler = state.ops.get(&op).and_then(|entry| entry.handler.clone());
        let controller = Signal::new();
        let Some(current) = state.current(generation) else {
            return Err(Self::take_outcome(state, op)
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
            request_id.to_string(),
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
            current.active_cell = Some(request_id.to_string());
            take_raw_output(current, request_id);
            let notice = state.continuity.notice.take();
            if let Some(entry) = state.ops.get_mut(&op) {
                entry.cell = Some(controller);
                entry.recovery = notice;
            }
        }
        Ok(())
    }

    fn send_or_fail(&self, state: &mut State, op: u64, generation: u64, request_id: &str, line: String) {
        if let Err(error) = self.send(state, generation, line) {
            if let Some(current) = state.current(generation) {
                current.pending.remove(request_id);
                if current.active_cell.as_deref() == Some(request_id) {
                    current.active_cell = None;
                }
            }
            entry_reject(state, op, error.clone());
            self.fail(state, generation, error);
        }
    }

    fn check_startup_deadline(&self, state: &mut State, generation: u64, now: Instant) -> Option<Duration> {
        let current = state.current(generation)?;
        if current.initialized {
            return Some(WATCHDOG_INTERVAL);
        }
        let deadline = current.startup_deadline;
        if deadline <= now {
            let error = self.startup_timeout(state);
            self.fail(state, generation, error);
            return None;
        }
        Some(deadline - now)
    }

    fn check_output_deadline(state: &mut State, generation: u64, now: Instant) -> Option<Duration> {
        let deadline = state
            .current(generation)
            .and_then(|current| current.output.as_ref())
            .and_then(|output| output.deadline)?;
        if deadline <= now {
            finish_cell(state, generation);
            return None;
        }
        Some(deadline - now)
    }

    fn check_cpu_backstop(&self, state: &mut State, generation: u64, now: Instant, cpu_checked: &mut Instant) -> bool {
        if now.duration_since(*cpu_checked) < WATCHDOG_INTERVAL {
            return false;
        }
        *cpu_checked = now;
        let exceeded = state
            .current(generation)
            .is_some_and(|current| self.cpu_exceeded(current));
        if exceeded {
            let error = KernelError::new(cpu_limit_message(&self.limits));
            self.fail(state, generation, error);
        }
        exceeded
    }

    fn next_wait_timeout(
        &self,
        state: &mut State,
        generation: u64,
        now: Instant,
        cpu_checked: &mut Instant,
    ) -> Option<Duration> {
        if state.current(generation).is_none() {
            return Some(WATCHDOG_INTERVAL);
        }
        let startup = self.check_startup_deadline(state, generation, now)?;
        let mut timeout = WATCHDOG_INTERVAL.min(startup);
        if state
            .current(generation)
            .and_then(|current| current.output.as_ref())
            .and_then(|output| output.deadline)
            .is_some()
        {
            let output = Self::check_output_deadline(state, generation, now)?;
            timeout = timeout.min(output);
        }
        if self.check_cpu_backstop(state, generation, now, cpu_checked) {
            return None;
        }
        Some(timeout)
    }

    /// Sends one request of call `op` to `generation` and waits for its done frame.
    pub(super) fn request(
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
        self.register_request(&mut state, op, generation, &request_id, execute)?;
        self.send_or_fail(&mut state, op, generation, &request_id, line);
        let mut cpu_checked = Instant::now();
        loop {
            if let Some(outcome) = Self::take_outcome(&mut state, op) {
                return outcome;
            }
            let Some(timeout) = self.next_wait_timeout(&mut state, generation, Instant::now(), &mut cpu_checked) else {
                continue;
            };
            state = self
                .changed
                .wait_timeout(state, timeout)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}
