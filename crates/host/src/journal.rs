//! The shell job journal (session value `ultron.module/jobs`) and job-done events, ported from
//! ultron/rlm/shell-jobs.ts (`#load`, `#persist`, `jobSummary`, `#fileTail`) and
//! experimental/session-worker.ts `jobEvent`.
//!
//! TypeScript: processes do not outlive the worker that owned them, so on restore a job the journal still lists as
//! running becomes `interrupted` ("worker restarted") and the journal is rewritten. The Rust host keeps a job's
//! process running past its worker (see `Host::with_journal`): a record that names its keeper process (`pid`,
//! `pidStart`) is left running here ([`restore_durable`]) and settled by the host from the process and its exit file.

use serde_json::{Value, json};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

pub const JOURNAL_NAMESPACE: &str = "ultron.module/jobs";
const JOURNAL_VERSION: u64 = 1;
const JOB_TEXT_BYTES: u64 = 16 * 1024;

/// A shell job's state, as journals and replies name it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    /// A TypeScript journal's running job after a restart (no process to follow).
    Interrupted,
    /// Its process ended while no worker followed it and left no exit status.
    Lost,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::TimedOut => "timed_out",
            Status::Interrupted => "interrupted",
            Status::Lost => "lost",
        }
    }

    fn from_name(name: &str) -> Status {
        match name {
            "running" => Status::Running,
            "completed" => Status::Completed,
            "cancelled" => Status::Cancelled,
            "timed_out" => Status::TimedOut,
            "interrupted" => Status::Interrupted,
            "lost" => Status::Lost,
            _ => Status::Failed,
        }
    }
}

/// One journaled job (camelCase on the wire, as TypeScript stores it).
#[derive(Clone, Debug, PartialEq)]
pub struct JobRecord {
    pub id: String,
    pub lane: String,
    pub root_id: Option<String>,
    pub command: String,
    pub cwd: String,
    pub timeout: Option<f64>,
    pub status: Status,
    pub exit_code: Option<i64>,
    pub started_at: u64,
    pub ended_at: Option<u64>,
    pub output_path: String,
    pub output_bytes: u64,
    pub retained_bytes: u64,
    pub tail: String,
    pub error: Option<String>,
    /// The job's keeper process (its process group leader) and that process's start time in clock ticks since boot,
    /// so a reopened worker can tell the job's own process from a reused pid. Absent in TypeScript journals.
    pub pid: Option<i64>,
    pub pid_start: Option<u64>,
}

fn text_of(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn int_of(value: &Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
}

/// A number as JSON.stringify writes it: integral values without a fraction.
fn js_number(value: f64) -> Value {
    jsrt::json::number(value)
}

impl JobRecord {
    pub fn from_value(value: &Value) -> JobRecord {
        JobRecord {
            id: text_of(value, "id"),
            lane: text_of(value, "lane"),
            root_id: value.get("rootId").and_then(Value::as_str).map(str::to_owned),
            command: text_of(value, "command"),
            cwd: text_of(value, "cwd"),
            timeout: value.get("timeout").and_then(Value::as_f64),
            status: Status::from_name(&text_of(value, "status")),
            exit_code: value.get("exitCode").and_then(Value::as_i64),
            started_at: int_of(value, "startedAt").unwrap_or(0),
            ended_at: int_of(value, "endedAt"),
            output_path: text_of(value, "outputPath"),
            output_bytes: int_of(value, "outputBytes").unwrap_or(0),
            retained_bytes: int_of(value, "retainedBytes").unwrap_or(0),
            tail: text_of(value, "tail"),
            error: value.get("error").and_then(Value::as_str).map(str::to_owned),
            pid: value.get("pid").and_then(Value::as_i64),
            pid_start: int_of(value, "pidStart"),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut out = json!({
            "id": self.id,
            "lane": self.lane,
            "rootId": self.root_id,
            "command": self.command,
            "cwd": self.cwd,
            "timeout": self.timeout.map(js_number),
            "status": self.status.as_str(),
            "exitCode": self.exit_code,
            "startedAt": self.started_at,
            "endedAt": self.ended_at,
            "outputPath": self.output_path,
            "outputBytes": self.output_bytes,
            "retainedBytes": self.retained_bytes,
            "tail": self.tail,
        });
        if let Some(error) = &self.error {
            out["error"] = json!(error);
        }
        if let Some(pid) = self.pid {
            out["pid"] = json!(pid);
        }
        if let Some(start) = self.pid_start {
            out["pidStart"] = json!(start);
        }
        out
    }
}

fn read_text(path: &str, cursor: u64, max: u64) -> String {
    let mut buffer = Vec::new();
    if let Ok(mut file) = File::open(path)
        && file.seek(SeekFrom::Start(cursor)).is_ok()
    {
        let _ = file.take(max).read_to_end(&mut buffer);
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The last `max` bytes of `text`, starting on a character boundary.
fn keep_tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// The last bytes of a job's spill file, when the file holds all of its output.
fn file_tail(record: &JobRecord) -> String {
    if record.retained_bytes != record.output_bytes || record.retained_bytes == 0 {
        return String::new();
    }
    let half = JOB_TEXT_BYTES / 2;
    let text = read_text(
        &record.output_path,
        record.retained_bytes.saturating_sub(half + 4),
        half + 4,
    );
    keep_tail(&text, half as usize).to_owned()
}

/// The jobs restored from a journal, and the document to write back when restoring changed a record.
pub struct Restored {
    pub records: Vec<JobRecord>,
    pub persist: Option<Value>,
}

/// Restore the journal (`saved` is the session value, None when unset) at time `now` (ms) for a host that keeps job
/// processes past their worker: a running record that names its keeper process stays `running` for the host to
/// settle; one without (a TypeScript journal) becomes `interrupted` ("worker restarted"), as TypeScript restores
/// every running job.
pub fn restore_durable(saved: Option<&Value>, now: u64) -> Result<Restored, String> {
    let jobs = match saved {
        Some(Value::Object(document)) => {
            if let Some(version) = document.get("version")
                && version.as_f64() != Some(JOURNAL_VERSION as f64)
            {
                return Err(format!(
                    "Cannot resume: session value {JOURNAL_NAMESPACE} has format version {version}, but this Ultron reads only format version {JOURNAL_VERSION}. It was left unchanged; open the session with the Ultron version that wrote it, or upgrade."
                ));
            }
            document
                .get("jobs")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    let mut changed = false;
    let records = jobs
        .iter()
        .map(|saved| {
            let mut job = JobRecord::from_value(saved);
            if job.tail.is_empty() {
                job.tail = file_tail(&job);
            }
            if job.status == Status::Running && job.pid.is_none() {
                changed = true;
                job.status = Status::Interrupted;
                job.ended_at = Some(job.ended_at.unwrap_or(now));
                job.error = Some("worker restarted".into());
            }
            job
        })
        .collect::<Vec<_>>();
    let persist = changed.then(|| persisted(&records));
    Ok(Restored { records, persist })
}

/// The journal document: every record with its tail left out (it would be rewritten with every job).
pub fn persisted(records: &[JobRecord]) -> Value {
    let jobs: Vec<Value> = records
        .iter()
        .map(|record| {
            let mut value = record.to_value();
            value["tail"] = json!("");
            value
        })
        .collect();
    json!({ "version": JOURNAL_VERSION, "jobs": jobs })
}

/// One-line completion summary: the last three non-empty output lines, at most `max_chars` UTF-16 units.
pub fn job_summary(record: &JobRecord, max_chars: usize) -> String {
    let lines: Vec<&str> = record
        .tail
        .split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let text = lines[lines.len().saturating_sub(3)..].join(" | ");
    let status = match (record.status, &record.error) {
        (Status::Completed, _) => format!(
            "exit {}",
            record.exit_code.map_or("null".to_owned(), |code| code.to_string())
        ),
        (status, Some(error)) if !error.is_empty() => format!("{}: {error}", status.as_str()),
        (status, _) => status.as_str().to_owned(),
    };
    let units: Vec<u16> = text.encode_utf16().collect();
    let body = if units.len() > max_chars {
        format!("…{}", String::from_utf16_lossy(&units[units.len() - (max_chars - 1)..]))
    } else {
        text
    };
    if body.is_empty() {
        status
    } else {
        format!("{status}; {body}")
    }
}

/// The `job_done` event for a job that ended while nobody waited on it and its root turn was not aborted.
pub fn job_event(record: &JobRecord, awaited: bool, root_aborted: bool) -> Option<Value> {
    if awaited || root_aborted {
        return None;
    }
    let mut event = json!({
        "kind": "job_done",
        "id": record.id,
        "status": record.status.as_str(),
        "summary": job_summary(record, 240),
        "fetch": format!("await rlm.job(\"{}\")", record.id),
        "lane": record.lane,
    });
    if let Some(root) = &record.root_id {
        event["rootId"] = json!(root);
    }
    Some(event)
}
