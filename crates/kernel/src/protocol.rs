//! Kernel wire framing, raw output capture/routing, host request dispatch, and cell completion.

use super::*;

impl Shared {
    pub(super) fn send(&self, state: &mut State, generation: u64, line: String) -> Result<(), KernelError> {
        let closed = state.closed;
        let failure = state.failure.clone();
        match state.current(generation) {
            Some(current) if !closed => current.writer.send(line).map_err(|_| KernelError::new(NOT_RUNNING)),
            _ => Err(match failure {
                Some((failed, error)) if failed == generation => error,
                _ => KernelError::new(NOT_RUNNING),
            }),
        }
    }

    pub(super) fn handle_data(self: &Arc<Self>, state: &mut State, generation: u64, chunk: &[u8]) {
        let mut offset = 0;
        while offset < chunk.len() {
            let Some(current) = state.current(generation) else {
                return;
            };
            let newline = chunk[offset..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map(|index| offset + index);
            let end = newline.unwrap_or(chunk.len());
            if current.buffer.len() + (end - offset) > MAX_FRAME_BYTES {
                let error = format!("RLM kernel protocol frame exceeds 1 MiB ({MAX_FRAME_BYTES} bytes)");
                self.fail(state, generation, KernelError::new(error));
                return;
            }
            current.buffer.extend_from_slice(&chunk[offset..end]);
            if newline.is_none() {
                return;
            }
            let line = std::mem::take(&mut current.buffer);
            let handled = match std::str::from_utf8(&line) {
                Ok(text) => self.handle_line(state, generation, text),
                Err(_) => Err("The encoded data was not valid for encoding utf-8".to_string()),
            };
            if let Err(message) = handled {
                let error = KernelError::new(format!("RLM kernel invalid protocol frame: {message}"));
                self.fail(state, generation, error);
            }
            offset = end + 1;
        }
    }

    pub(super) fn handle_line(self: &Arc<Self>, state: &mut State, generation: u64, line: &str) -> Result<(), String> {
        let parsed: Value =
            serde_json::from_str(&replace_lone_surrogates(line)).map_err(|error| format!("invalid JSON: {error}"))?;
        let Value::Object(frame) = parsed else {
            return Err("protocol frame must be a JSON object".into());
        };
        let Some(Value::String(event)) = frame.get("event") else {
            return Err("protocol frame is missing event".into());
        };
        if state.current(generation).is_none() {
            return Ok(());
        }
        match event.as_str() {
            "ready" => self.handle_ready_event(state, generation, &frame),
            "host_request" => {
                let request = parse_host_request(&frame)?;
                self.handle_host_request(state, generation, request);
                Ok(())
            }
            "host_cancel" => handle_host_cancel_event(state, generation, &frame),
            "stdout" | "stderr" | "result" | "error" | "done" => {
                self.handle_execution_event(state, generation, event, &frame)
            }
            other => Err(format!("unknown protocol event: {other}")),
        }
    }

    fn handle_ready_event(&self, state: &mut State, generation: u64, frame: &Map<String, Value>) -> Result<(), String> {
        let Some(current) = state.current(generation) else {
            return Ok(());
        };
        if current.ready_received {
            return Err("duplicate ready event".into());
        }
        if frame.contains_key("id") {
            return Err("ready event cannot have an id".into());
        }
        current.ready_received = true;
        current.initialized = !current.host_completes_startup;
        if current.tree_backend == TreeMemoryBackend::Cgroup {
            self.tree_memory_exceeded(current);
        }
        Ok(())
    }

    fn handle_execution_event(
        self: &Arc<Self>,
        state: &mut State,
        generation: u64,
        event: &str,
        frame: &Map<String, Value>,
    ) -> Result<(), String> {
        let id = required_non_empty_string(frame, "id", &format!("{event} event requires an id"))?;
        let status = frame.get("status").map(Value::as_str);
        if event == "done" && status.is_some_and(|status| status != Some("ok") && status != Some("error")) {
            return Err("done event has invalid status".into());
        }
        let Some(current) = state.current(generation) else {
            return Ok(());
        };
        if event == "done"
            && current.tree_backend == TreeMemoryBackend::Cgroup
            && current.pending.contains_key(&id)
            && self.tree_memory_exceeded(current)
        {
            let message = tree_memory::tree_memory_limit_message(self.tree_memory_mb, current.tree_backend);
            self.fail(state, generation, KernelError::new(message));
            return Ok(());
        }
        route_execution_frame(state, generation, &id, event, frame)
    }

    pub(super) fn handle_host_request(self: &Arc<Self>, state: &mut State, generation: u64, request: HostRequest) {
        let HostRequest {
            id,
            kind,
            payload,
            cell,
        } = request;
        let Some(current) = state.current(generation) else {
            return;
        };
        if current.host_requests.contains_key(&id) {
            let error = KernelError::new(format!("RLM kernel protocol duplicate host request id: {id}"));
            self.fail(state, generation, error);
            return;
        }
        let waiter = match resolve_host_request_waiter(current, cell.as_ref()) {
            Ok(waiter) => waiter,
            Err(message) => {
                self.send_host_error(state, generation, &id, message);
                return;
            }
        };
        if current.host_requests.len() >= MAX_HOST_REQUESTS {
            let message = format!("RLM host request concurrency exceeds {MAX_HOST_REQUESTS}");
            self.send_host_error(state, generation, &id, &message);
            return;
        }
        let handler = waiter.handler.clone().unwrap_or_else(|| self.handler.clone());
        let signal = linked_request_signal(&waiter.controller);
        current.host_requests.insert(id.clone(), signal.clone());
        self.spawn_host_request_worker(generation, id, kind, payload, handler, signal);
    }

    fn spawn_host_request_worker(
        self: &Arc<Self>,
        generation: u64,
        id: String,
        kind: String,
        payload: Map<String, Value>,
        handler: Arc<dyn HostHandler>,
        signal: Signal,
    ) {
        let shared = self.clone();
        std::thread::spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler.handle(&kind, payload, &signal)))
                    .unwrap_or_else(|_| Err(format!("RLM host handler for {kind} panicked")));
            shared.with_state(|state| {
                if state.current(generation).is_some() {
                    match result {
                        Ok(value) => {
                            let reply = object([
                                ("request", Value::from("host_reply")),
                                ("id", Value::from(id.clone())),
                                ("payload", value),
                            ]);
                            shared.send_host_reply(state, generation, &id, &reply);
                        }
                        Err(message) => shared.send_host_error(state, generation, &id, &message),
                    }
                }
                if let Some(current) = state.current(generation) {
                    current.host_requests.remove(&id);
                }
            });
        });
    }

    pub(super) fn send_host_reply(&self, state: &mut State, generation: u64, id: &str, reply: &Map<String, Value>) {
        let sent = encode_frame(reply).and_then(|line| self.send(state, generation, line));
        if let Err(error) = sent {
            if state.current(generation).is_none() {
                return;
            }
            self.send_host_error(state, generation, id, error.message());
        }
    }

    pub(super) fn send_host_error(&self, state: &mut State, generation: u64, id: &str, message: &str) {
        if state.current(generation).is_none() {
            return;
        }
        let reply = object([
            ("request", Value::from("host_reply")),
            ("id", Value::from(id)),
            ("error", Value::from(message)),
        ]);
        if let Err(error) = encode_frame(&reply).and_then(|line| self.send(state, generation, line)) {
            self.fail(state, generation, error);
        }
    }
}

pub(super) fn entry_reject(state: &mut State, op: u64, error: KernelError) {
    if let Some(entry) = state.ops.get_mut(&op) {
        entry.outcome.get_or_insert(Err(error));
    }
}

pub(super) fn read_chunks(mut stream: impl Read, mut handle: impl FnMut(&[u8])) -> std::io::Result<()> {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => handle(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn required_non_empty_string(frame: &Map<String, Value>, key: &str, err: &str) -> Result<String, String> {
    match frame.get(key) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(err.into()),
    }
}

fn parse_host_request(frame: &Map<String, Value>) -> Result<HostRequest, String> {
    let id = required_non_empty_string(frame, "id", "host_request requires an id")?;
    let kind = required_non_empty_string(frame, "type", "host_request requires a type")?;
    let payload = match frame.get("payload") {
        None => Map::new(),
        Some(Value::Object(payload)) => payload.clone(),
        Some(_) => return Err("host_request payload must be an object".into()),
    };
    let cell = match frame.get("cell") {
        None => None,
        Some(Value::String(cell)) if !cell.is_empty() => Some(cell.clone()),
        Some(_) => return Err("host_request cell must be a request id".into()),
    };
    Ok(HostRequest {
        id,
        kind,
        payload,
        cell,
    })
}

fn handle_host_cancel_event(state: &mut State, generation: u64, frame: &Map<String, Value>) -> Result<(), String> {
    let Some(Value::String(id)) = frame.get("id") else {
        return Err("host_cancel requires an id".into());
    };
    if let Some(request) = state
        .current(generation)
        .and_then(|current| current.host_requests.get(id))
    {
        request.abort("RLM host request cancelled");
    }
    Ok(())
}

fn resolve_host_request_waiter<'a>(current: &'a Generation, cell: Option<&String>) -> Result<&'a Waiter, &'static str> {
    let waiter = match cell {
        Some(cell) if current.active_cell.as_ref() != Some(cell) => {
            return Err("RLM host request from a cell that has finished");
        }
        Some(cell) => current.pending.get(cell),
        None => current.active_cell.as_ref().and_then(|cell| current.pending.get(cell)),
    };
    waiter.ok_or("RLM host request has no active cell")
}

fn linked_request_signal(cell_signal: &Signal) -> Signal {
    let signal = Signal::new();
    let request = signal.clone();
    if cell_signal.on_abort(move |reason| request.abort(reason)).is_none() {
        signal.abort(cell_signal.reason().unwrap_or_default());
    }
    signal
}

fn apply_error_frame(result: &mut ExecutionResult, frame: &Map<String, Value>) {
    result.status = Status::Error;
    let traceback = match frame.get("traceback") {
        Some(Value::Array(lines)) => lines
            .iter()
            .filter_map(|line| line.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    };
    result.error = Some(CellError {
        ename: frame_string(frame.get("ename")),
        evalue: frame_string(frame.get("evalue")),
        traceback,
    });
}

fn apply_done_status(result: &mut ExecutionResult, frame: &Map<String, Value>) {
    if frame.get("status") == Some(&Value::from("error")) {
        result.status = Status::Error;
    }
    if result.status == Status::Error
        && result.error.is_none()
        && let Some(error) = frame.get("error").filter(|error| !error.is_null())
    {
        result.error = Some(CellError {
            ename: "RuntimeError".into(),
            evalue: frame_string(Some(error)),
            traceback: Vec::new(),
        });
    }
}

fn handle_done_frame(state: &mut State, generation: u64, request_id: &str, frame: &Map<String, Value>) {
    let Some(current) = state.current(generation) else {
        return;
    };
    let Some(mut waiter) = current.pending.remove(request_id) else {
        return;
    };
    apply_done_status(&mut waiter.state.result, frame);
    if let Some(report) = snapshot::snapshot_report(frame.get("snapshot")) {
        waiter.state.snapshot = Some(report);
    }
    if let Some(report) = snapshot::restore_report(frame.get("restore")) {
        waiter.state.restore = Some(report);
    }
    if current.active_cell.as_deref() == Some(request_id) {
        current.active_cell = None;
    }
    waiter.controller.abort("RLM cell completed");
    let Some(output) = current.output.as_mut().filter(|output| output.request_id == request_id) else {
        state.resolve(waiter);
        return;
    };
    let flush = frame.get("flush").and_then(Value::as_object);
    let flushed = |name: &str| flush.and_then(|flush| flush.get(name)) == Some(&Value::Bool(true));
    output.awaiting = Some([flushed("stdout"), flushed("stderr")]);
    output.deadline = Some(Instant::now() + RAW_FLUSH_TIMEOUT);
    output.finished = Some(waiter);
    finish_cell_if_flushed(state, generation);
}

pub(super) fn route_execution_frame(
    state: &mut State,
    generation: u64,
    request_id: &str,
    event: &str,
    frame: &Map<String, Value>,
) -> Result<(), String> {
    let Some(current) = state.current(generation) else {
        return Ok(());
    };
    let Some(waiter) = current.pending.get_mut(request_id) else {
        return Err(format!("protocol frame references unknown request id: {request_id}"));
    };
    let result = &mut waiter.state.result;
    match event {
        "stdout" => append_bounded(&mut result.stdout, &frame_string(frame.get("text"))),
        "stderr" => append_bounded(&mut result.stderr, &frame_string(frame.get("text"))),
        "result" => result.result = Some(frame_string(frame.get("result"))),
        "error" => apply_error_frame(result, frame),
        _ => handle_done_frame(state, generation, request_id, frame),
    }
    Ok(())
}

/// The cell owns fd 1 and fd 2 output from now on, and reports the output from outside a cell that came before it.
pub(super) fn take_raw_output(generation: &mut Generation, request_id: &str) {
    let orphan = generation.raw.each_mut().map(|stream| {
        let text = orphan_text(&stream.orphan, stream.orphan_dropped);
        stream.orphan.clear();
        stream.orphan_dropped = false;
        text
    });
    generation.output = Some(CellOutput {
        request_id: request_id.to_string(),
        marker: flush_marker(request_id),
        captures: Default::default(),
        orphan,
        awaiting: None,
        deadline: None,
        finished: None,
    });
}

/// Raw bytes from fd 1 (`index` 0) or fd 2 (1). They belong to the cell that owns the output until that cell's flush
/// marker arrives on the stream; anything else is output from outside a cell.
pub(super) fn handle_raw(state: &mut State, generation: u64, index: usize, chunk: &[u8]) {
    let State {
        generation: current,
        stderr,
        ..
    } = state;
    let Some(current) = current.as_mut().filter(|current| current.id == generation) else {
        return;
    };
    let Generation { raw, output, .. } = current;
    let stream = &mut raw[index];
    let mut data = std::mem::take(&mut stream.held);
    data.extend_from_slice(chunk);
    let mut data = &data[..];
    if !stream.started {
        let startup = flush_marker("ready");
        let found = find(data, &startup);
        let end = found.unwrap_or_else(|| data.len() - marker_prefix_len(data, &startup));
        if index == 1 {
            keep_tail(stderr, &String::from_utf8_lossy(&data[..end]));
        }
        let Some(found) = found else {
            stream.held = data[end..].to_vec();
            return;
        };
        stream.started = true;
        data = &data[found + startup.len()..];
    }
    let Some(cell) = output.as_mut().filter(|cell| !cell.captures[index].marker_seen) else {
        route_raw(stream, stderr, index, data, None);
        return;
    };
    if let Some(found) = find(data, &cell.marker) {
        route_raw(stream, stderr, index, &data[..found], Some(&mut cell.captures[index]));
        cell.captures[index].marker_seen = true;
        route_raw(stream, stderr, index, &data[found + cell.marker.len()..], None);
        finish_cell_if_flushed(state, generation);
        return;
    }
    let held = marker_prefix_len(data, &cell.marker);
    stream.held = data[data.len() - held..].to_vec();
    route_raw(
        stream,
        stderr,
        index,
        &data[..data.len() - held],
        Some(&mut cell.captures[index]),
    );
}

pub(super) fn route_raw(
    stream: &mut RawStream,
    stderr: &mut String,
    index: usize,
    bytes: &[u8],
    capture: Option<&mut RawCapture>,
) {
    if bytes.is_empty() {
        return;
    }
    let text = stream.decoder.decode(bytes);
    if index == 1 {
        keep_tail(stderr, &text);
    }
    if let Some(capture) = capture {
        capture.push(&text);
        return;
    }
    let used = jsrt::utf16::len(&stream.orphan);
    let room = MAX_ORPHAN_CHARS.saturating_sub(used);
    if jsrt::utf16::len(&text) > room {
        stream.orphan_dropped = true;
    }
    if room > 0 {
        let kept = jsrt::utf16::head(&text, room);
        stream.orphan.push_str(kept);
    }
}

/// Resolves the finished cell once every flush marker its done frame announced has arrived.
pub(super) fn finish_cell_if_flushed(state: &mut State, generation: u64) {
    let Some(output) = state.current(generation).and_then(|current| current.output.as_ref()) else {
        return;
    };
    let (Some(_), Some(awaiting)) = (&output.finished, output.awaiting) else {
        return;
    };
    if (0..2).any(|index| awaiting[index] && !output.captures[index].marker_seen) {
        return;
    }
    finish_cell(state, generation);
}

/// Combines the output from outside a cell, the cell's captured frames and its raw output, and resolves the cell.
pub(super) fn finish_cell(state: &mut State, generation: u64) {
    let State {
        generation: current,
        stderr,
        ..
    } = state;
    let Some(current) = current.as_mut().filter(|current| current.id == generation) else {
        return;
    };
    let Some(output) = current.output.take() else {
        return;
    };
    for (index, stream) in current.raw.iter_mut().enumerate() {
        if stream.started {
            let held = std::mem::take(&mut stream.held);
            route_raw(stream, stderr, index, &held, None);
        }
    }
    let Some(mut waiter) = output.finished else {
        return;
    };
    let [orphan_out, orphan_err] = &output.orphan;
    waiter.state.result.stdout = format!(
        "{orphan_out}{}{}",
        waiter.state.result.stdout,
        output.captures[0].text(1)
    );
    waiter.state.result.stderr = format!(
        "{orphan_err}{}{}",
        waiter.state.result.stderr,
        output.captures[1].text(2)
    );
    state.resolve(waiter);
}
