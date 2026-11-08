//! A test [`CellExecutor`] over persistent Python kernels (`crates/kernel`): one kernel per REPL lane, opened on first
//! use, so these tests run the durable REPL cell contract of `harness::chat` over a real kernel without the session
//! stack (the product's executor is `session_core::turns::TurnCells`). A cell's host requests go to the test's
//! handler, except `cell.output`, which publishes durable partial output into the cell's `ultron.live` slot (what an
//! interrupted cell's result carries after a crash).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use ai::Tool;
use durable::ConversationId;
use kernel::{ExecutionResult, HostHandler, Kernel, Signal, Status};
use serde_json::{Map, Value, json};

use harness::chat::{CellCall, CellExecutor, CellProgress, CellResult};
use harness::scheduler::Cancel;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Opens the kernel of one conversation's lane: every conversation (the root, each child) has its own kernels.
pub type OpenLane = Arc<dyn Fn(ConversationId, &str) -> Result<Arc<Kernel>, String> + Send + Sync>;
/// The host handler serving one cell's requests (the composition root's stack, delegation operations).
pub type CellHandlers = Arc<dyn Fn(&CellCall) -> Arc<dyn HostHandler> + Send + Sync>;

/// The `rlm` tool declaration: Python code for the persistent REPL, and an optional lane.
pub fn rlm_tool(description: &str) -> Tool {
    Tool {
        name: "rlm".into(),
        description: description.into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "Python code for the persistent REPL" },
            },
            "required": ["code"],
        }),
        constrained_sampling: None,
    }
}

/// Cells run in per-lane kernels.
pub struct KernelCells {
    tool: Tool,
    open: OpenLane,
    handlers: Option<CellHandlers>,
    lanes: Mutex<HashMap<(ConversationId, String), Arc<Kernel>>>,
}

impl KernelCells {
    /// `open` starts a lane's kernel; `handlers` serve the cells' other host requests.
    pub fn new(tool: Tool, open: OpenLane, handlers: Option<CellHandlers>) -> KernelCells {
        KernelCells {
            tool,
            open,
            handlers,
            lanes: Mutex::new(HashMap::new()),
        }
    }

    /// A conversation lane's kernel, opening it on first use.
    pub fn lane(&self, conversation: ConversationId, name: &str) -> Result<Arc<Kernel>, String> {
        let key = (conversation, name.to_owned());
        if let Some(kernel) = lock(&self.lanes).get(&key) {
            return Ok(kernel.clone());
        }
        let kernel = (self.open)(conversation, name)?;
        Ok(lock(&self.lanes).entry(key).or_insert(kernel).clone())
    }

    /// Lanes with a kernel, as `(conversation, lane)`.
    pub fn open_lanes(&self) -> Vec<(ConversationId, String)> {
        lock(&self.lanes).keys().cloned().collect()
    }

    /// Shut every lane's kernel down.
    pub fn shutdown(&self) {
        for kernel in lock(&self.lanes).drain().map(|(_, kernel)| kernel) {
            kernel.shutdown();
        }
    }
}

/// Serves one cell's host requests: `cell.output` to its progress, the rest to the root's handler.
struct CellHandler {
    progress: CellProgress,
    inner: Option<Arc<dyn HostHandler>>,
}

impl HostHandler for CellHandler {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        if kind == "cell.output" {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .ok_or("cell.output needs a `text` string")?;
            self.progress.output(text);
            return Ok(Value::Null);
        }
        match &self.inner {
            Some(inner) => inner.handle(kind, payload, signal),
            None => Err(format!("Unknown host request {kind}")),
        }
    }
}

/// The text the model reads for a cell: output, the last expression's value, and the error.
pub fn cell_text(result: &ExecutionResult) -> String {
    let mut parts: Vec<String> = Vec::new();
    for stream in [&result.stdout, &result.stderr] {
        let stream = stream.trim_end_matches('\n');
        if !stream.is_empty() {
            parts.push(stream.to_owned());
        }
    }
    if let Some(value) = result.result.as_ref().filter(|value| !value.is_empty()) {
        parts.push(value.clone());
    }
    if let Some(error) = &result.error {
        if error.traceback.is_empty() {
            parts.push(format!("{}: {}", error.ename, error.evalue));
        } else {
            parts.push(error.traceback.join("\n"));
        }
    }
    if parts.is_empty() {
        "(no result)".into()
    } else {
        parts.join("\n")
    }
}

impl CellExecutor for KernelCells {
    fn tool(&self) -> Tool {
        self.tool.clone()
    }

    fn execute(&self, call: &CellCall, progress: &CellProgress, signal: &Cancel) -> Result<CellResult, String> {
        let kernel = self.lane(call.conversation_id, call.lane())?;
        // The invocation's cancellation aborts the cell: a watcher waits for it until the cell is done.
        let kernel_signal = Signal::new();
        let done = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (signal, bridge, done) = (signal.clone(), kernel_signal.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::SeqCst) {
                    if let Some(reason) = signal.wait_timeout(Duration::from_millis(5)) {
                        bridge.abort(reason.to_string());
                        return;
                    }
                }
            })
        };
        let handler: Arc<dyn HostHandler> = Arc::new(CellHandler {
            progress: progress.clone(),
            inner: self.handlers.as_ref().map(|handlers| handlers(call)),
        });
        let executed = kernel.execute(call.code(), Some(&kernel_signal), Some(handler));
        done.store(true, Ordering::SeqCst);
        let _ = watcher.join();
        let result = executed.map_err(|error| error.to_string())?;
        Ok(CellResult {
            is_error: result.status == Status::Error,
            ..CellResult::text(cell_text(&result))
        })
    }
}
