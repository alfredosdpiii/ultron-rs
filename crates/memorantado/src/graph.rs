//! memorantado's memory relations, read for the knowledge graph's Neo4j mirror (crates/graph): every memory of the
//! project (active, superseded and invalidated), which memory superseded which (`supersedesId`), and the open
//! conflicts between them. Only read: nothing here writes to memorantado.

use crate::backend::{MemorantadoBackend, MemorantadoOptions};
use crate::resolve::Endpoint;
use blocking_http::Transport;
use kernel::Signal;
use kernel_apis::MemoryError;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// A list request's deadline and the largest answer read.
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Memories asked for per page, and at most in all.
pub const PAGE: usize = 500;
pub const MAX_MEMORIES: usize = 20_000;
/// Characters of a memory's content kept.
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

impl Endpoint {
    /// A client for [`MemorantadoBackend::project_graph`] (no scope: it lists the whole project), over `transport`
    /// (`None`: a real one).
    pub fn graph_backend(&self, transport: Option<Arc<dyn Transport>>) -> Result<MemorantadoBackend, MemoryError> {
        MemorantadoBackend::new(MemorantadoOptions {
            timeout: TIMEOUT,
            max_response_bytes: MAX_BYTES,
            transport,
            ..MemorantadoOptions::new(&self.url, &self.project, Map::new())
        })
    }
}

impl MemorantadoBackend {
    /// The project's memories (at most [`MAX_MEMORIES`], their content cut at [`CONTENT_CHARS`]) and the links
    /// between them, page by page.
    pub fn project_graph(&self, signal: &Signal) -> Result<MemoryGraph, String> {
        let mut graph = MemoryGraph::default();
        'statuses: for status in ["active", "superseded", "invalidated"] {
            let mut offset = 0;
            loop {
                let query = [
                    ("status", status.to_string()),
                    ("limit", PAGE.to_string()),
                    ("offset", offset.to_string()),
                ];
                let page = self
                    .call("GET", "/semantic-memories", &query, None, false, signal)?
                    .unwrap_or(Value::Null);
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
        let conflicts = self
            .call("GET", "/memory-conflicts", &[], None, false, signal)?
            .unwrap_or(Value::Null);
        for conflict in conflicts.as_array().into_iter().flatten() {
            if let (Some(a), Some(b)) = (conflict["memoryId"].as_i64(), conflict["conflictingId"].as_i64()) {
                let reason = conflict["reason"].as_str().unwrap_or_default().to_string();
                graph.conflicts.push((a, b, reason));
            }
        }
        Ok(graph)
    }
}
