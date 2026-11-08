//! memorantado's memory relations, read for the mirror: every memory of the project (active, superseded and
//! invalidated), which memory superseded which (`supersedesId`), and the open conflicts between them. Only read:
//! nothing here writes to memorantado.
//!
//! A small adapter over memorantado's REST list endpoints (`GET <url>/semantic-memories`, `GET
//! <url>/memory-conflicts`) until the memory backend exposes a list of its own.

use blocking_http::{HttpRequest, Transport, TransportError};
use kernel::Signal;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Memories asked for per page, and at most in all.
pub const PAGE: usize = 500;
pub const MAX_MEMORIES: usize = 20_000;
/// Characters of a memory's content kept in the mirror.
pub const CONTENT_CHARS: usize = 300;

#[derive(Clone, Debug, PartialEq)]
pub struct MemoryNode {
    pub id: i64,
    pub key: Option<String>,
    pub kind: String,
    pub subject: String,
    /// `active`, `superseded` or `invalidated`.
    pub status: String,
    pub content: String,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryGraph {
    pub memories: Vec<MemoryNode>,
    /// (newer, older): the newer memory replaced the older one.
    pub supersedes: Vec<(i64, i64)>,
    /// (memory, conflicting memory, reason), open conflicts only.
    pub conflicts: Vec<(i64, i64, String)>,
}

/// The memorantado memory lives in, as the session chose it (memorantado::resolve): its REST API and project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorantadoSettings {
    /// Its REST API (`<url>/semantic-memories`).
    pub url: String,
    pub project: String,
}

pub struct Memorantado {
    transport: Arc<dyn Transport>,
    settings: MemorantadoSettings,
}

impl Memorantado {
    pub fn new(transport: Arc<dyn Transport>, settings: MemorantadoSettings) -> Memorantado {
        Memorantado { transport, settings }
    }

    pub fn project(&self) -> &str {
        &self.settings.project
    }

    fn get(&self, path_and_query: &str, signal: &Signal) -> Result<Value, String> {
        let request = HttpRequest {
            method: "GET".into(),
            url: format!("{}{path_and_query}", self.settings.url),
            headers: vec![("accept".into(), "application/json".into())],
            body: None,
        };
        let response = self
            .transport
            .send(request, MAX_BYTES, TIMEOUT, signal)
            .map_err(|error| match error {
                TransportError::Timeout => format!("memorantado did not answer within {}s", TIMEOUT.as_secs()),
                TransportError::Aborted => "the memorantado request was cancelled".to_string(),
                TransportError::TooLarge => "memorantado's answer was too large".to_string(),
                TransportError::Failed => format!("memorantado at {} could not be reached", self.settings.url),
            })?;
        if !(200..300).contains(&response.status) {
            return Err(format!("memorantado answered HTTP {}", response.status));
        }
        serde_json::from_slice(&response.body).map_err(|_| "memorantado's answer is not JSON".to_string())
    }

    /// The project's memories and the links between them.
    pub fn read(&self, signal: &Signal) -> Result<MemoryGraph, String> {
        let project = jsrt::uri::encode_component(&self.settings.project);
        let mut graph = MemoryGraph::default();
        'statuses: for status in ["active", "superseded", "invalidated"] {
            let mut offset = 0;
            loop {
                let page = self.get(
                    &format!("/semantic-memories?project={project}&status={status}&limit={PAGE}&offset={offset}"),
                    signal,
                )?;
                let items = page.as_array().ok_or("memorantado's memory list is not a list")?;
                for item in items {
                    let Some(id) = item["id"].as_i64() else { continue };
                    let text = |key: &str| item[key].as_str().unwrap_or_default().to_string();
                    if let Some(older) = item["supersedesId"].as_i64() {
                        graph.supersedes.push((id, older));
                    }
                    graph.memories.push(MemoryNode {
                        id,
                        key: item["key"].as_str().map(str::to_string),
                        kind: text("kind"),
                        subject: text("subject"),
                        status: item["status"].as_str().unwrap_or(status).to_string(),
                        content: text("content").chars().take(CONTENT_CHARS).collect(),
                        tags: item["tags"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    });
                    if graph.memories.len() == MAX_MEMORIES {
                        break 'statuses;
                    }
                }
                if items.len() < PAGE {
                    break;
                }
                offset += PAGE;
            }
        }
        let conflicts = self.get(&format!("/memory-conflicts?project={project}"), signal)?;
        for conflict in conflicts.as_array().into_iter().flatten() {
            if let (Some(a), Some(b)) = (conflict["memoryId"].as_i64(), conflict["conflictingId"].as_i64()) {
                let reason = conflict["reason"].as_str().unwrap_or_default().to_string();
                graph.conflicts.push((a, b, reason));
            }
        }
        Ok(graph)
    }
}
