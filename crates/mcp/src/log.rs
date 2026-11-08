//! Server log messages (`extensions/mcp/log.ts`): `notifications/message` from MCP servers, appended to `mcp.log` in
//! the agent directory. Several processes may write the same file, so every message is one append; the file is
//! rotated to `mcp.log.1` once it grows past 5 MiB. Write errors are ignored: logging must not break tools.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// `formatMcpLogMessage`: one log line for a `notifications/message` from `server`; continuation lines are indented.
pub fn format_log_message(server: &str, params: Option<&Value>, now_ms: i64) -> String {
    let wrapped;
    let message = match params {
        Some(Value::Object(_)) => params.unwrap_or(&Value::Null),
        other => {
            wrapped = serde_json::json!({ "data": other.cloned().unwrap_or(Value::Null) });
            &wrapped
        }
    };
    let level = message.get("level").and_then(Value::as_str).unwrap_or("info");
    let logger = message
        .get("logger")
        .and_then(Value::as_str)
        .filter(|l| !l.is_empty())
        .map(|l| format!(" {l}:"))
        .unwrap_or_default();
    let data = match message.get("data") {
        Some(Value::String(text)) => text.clone(),
        // `JSON.stringify(undefined)` is undefined, so `String(data)`.
        None => "undefined".into(),
        Some(other) => other.to_string(),
    };
    let text = data.replace("\r\n", "\n").replace('\n', "\n    ");
    format!("{} [{server}] {level}{logger} {text}\n", jsrt::date::iso_string(now_ms))
}

/// Appends server log messages to one file (`McpServerLog`).
pub struct ServerLog {
    pub path: PathBuf,
    size: Mutex<Option<u64>>,
}

impl ServerLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            size: Mutex::new(None),
        }
    }

    pub fn write(&self, server: &str, params: Option<&Value>) {
        let line = format_log_message(server, params, jsrt::date::now_ms());
        let mut size = self.size.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = self.append(&mut size, &line);
    }

    fn append(&self, size: &mut Option<u64>, line: &str) -> std::io::Result<()> {
        if size.is_none() {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            *size = Some(current_size(&self.path));
        }
        if size.unwrap_or(0) > MAX_LOG_BYTES {
            // Another process may have rotated it already; check before renaming.
            if current_size(&self.path) > MAX_LOG_BYTES {
                let mut rotated = self.path.clone().into_os_string();
                rotated.push(".1");
                std::fs::rename(&self.path, rotated)?;
            }
            *size = Some(current_size(&self.path));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(line.as_bytes())?;
        *size = Some(size.unwrap_or(0) + line.len() as u64);
        Ok(())
    }
}

fn current_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}
