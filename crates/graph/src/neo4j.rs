//! Neo4j through its HTTP Query API (`POST <url>/db/<database>/query/v2`, one Cypher statement per request): the
//! mirror of the knowledge store's accepted relations and of memorantado's memory relations. Nothing is read back;
//! the mirror is for people and graph tools.
//!
//! Relations: `(:Code {repo, name})-[:READS_CONFIG|SERVES_ROUTE|TOUCHES_TABLE|TESTS {repo, path, line, rev, blob,
//! evidence, quote, checked_by, source: "ultron"}]->(:ConfigKey|Route|Table|Code {repo, name})`. Memories:
//! `(:Memory {source: "memorantado", project, id, key, kind, subject, status, content, tags})` with `SUPERSEDES` and
//! `CONFLICTS_WITH {reason}` between them.

use crate::memory::MemoryGraph;
use blocking_http::{HttpRequest, Transport, TransportError};
use config::keys::Neo4jSettings;
use kernel::Signal;
use knowledge::relations::{Kind, Relation};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// One statement's deadline and the largest answer read.
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_BYTES: usize = 1024 * 1024;
/// Rows sent in one statement.
pub const BATCH: usize = 500;

/// The password of `graph.neo4j.user`, read at each request; none when Neo4j runs without authentication.
pub type Password = Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub struct Neo4j {
    transport: Arc<dyn Transport>,
    settings: Neo4jSettings,
    password: Password,
}

/// Database names Neo4j allows (letters, digits, dots, dashes, underscores).
fn valid_database(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn relation_row(relation: &Relation) -> Value {
    json!({
        "subject": relation.subject,
        "object": relation.object,
        "path": relation.path,
        "line": relation.line,
        "blob": relation.blob,
        "evidence": relation.evidence(),
        "quote": relation.quote,
        "checked_by": relation.checked_by,
    })
}

impl Neo4j {
    pub fn new(transport: Arc<dyn Transport>, settings: Neo4jSettings, password: Password) -> Neo4j {
        Neo4j {
            transport,
            settings,
            password,
        }
    }

    /// Runs one statement; an error says what failed, without the password.
    pub fn run(&self, statement: &str, parameters: Value, signal: &Signal) -> Result<(), String> {
        let database = &self.settings.database;
        if !valid_database(database) {
            return Err(format!("graph.neo4j.database {database:?} is not a database name"));
        }
        let mut headers = vec![
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];
        if let Some(password) = (self.password)() {
            let credentials = jsrt::base64::encode(format!("{}:{password}", self.settings.user));
            headers.push(("authorization".to_string(), format!("Basic {credentials}")));
        }
        let request = HttpRequest {
            method: "POST".into(),
            url: format!("{}/db/{database}/query/v2", self.settings.url),
            headers,
            body: Some(
                json!({"statement": statement, "parameters": parameters})
                    .to_string()
                    .into_bytes(),
            ),
        };
        let response = self
            .transport
            .send(request, MAX_BYTES, TIMEOUT, signal)
            .map_err(|error| match error {
                TransportError::Timeout => format!("Neo4j did not answer within {}s", TIMEOUT.as_secs()),
                TransportError::Aborted => "the Neo4j request was cancelled".to_string(),
                TransportError::TooLarge => "Neo4j's answer was too large".to_string(),
                TransportError::Failed => format!("Neo4j at {} could not be reached", self.settings.url),
            })?;
        match response.status {
            200..=299 => {}
            401 | 403 => return Err(format!("Neo4j refused the credentials of {}", self.settings.user)),
            status => return Err(format!("Neo4j answered HTTP {status}")),
        }
        let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
        match body
            .get("errors")
            .and_then(Value::as_array)
            .and_then(|errors| errors.first())
        {
            Some(error) => Err(format!(
                "Neo4j: {}",
                error["message"].as_str().or(error["code"].as_str()).unwrap_or("error")
            )),
            None => Ok(()),
        }
    }

    /// Mirrors accepted relations of `repo` at `rev` (merged: mirroring again changes nothing). Returns how many.
    pub fn upsert_relations(
        &self,
        repo: &str,
        rev: &str,
        relations: &[Relation],
        signal: &Signal,
    ) -> Result<usize, String> {
        for kind in Kind::ALL {
            let rows: Vec<Value> = relations.iter().filter(|r| r.kind == kind).map(relation_row).collect();
            let statement = format!(
                "UNWIND $rows AS row \
                 MERGE (s:Code {{repo: $repo, name: row.subject}}) \
                 MERGE (o:{label} {{repo: $repo, name: row.object}}) \
                 MERGE (s)-[r:{edge} {{repo: $repo, path: row.path, line: row.line}}]->(o) \
                 SET r.source = 'ultron', r.rev = $rev, r.blob = row.blob, r.evidence = row.evidence, \
                     r.quote = row.quote, r.checked_by = row.checked_by",
                label = kind.object_label(),
                edge = kind.edge(),
            );
            for chunk in rows.chunks(BATCH) {
                self.run(&statement, json!({"repo": repo, "rev": rev, "rows": chunk}), signal)?;
            }
        }
        Ok(relations.len())
    }

    /// Removes relations of `repo` from the mirror (the judge refused them, or sent them for review).
    pub fn remove_relations(&self, repo: &str, relations: &[Relation], signal: &Signal) -> Result<usize, String> {
        for kind in Kind::ALL {
            let rows: Vec<Value> = relations.iter().filter(|r| r.kind == kind).map(relation_row).collect();
            let statement = format!(
                "UNWIND $rows AS row \
                 MATCH (:Code {{repo: $repo, name: row.subject}})-[r:{edge} {{repo: $repo, path: row.path, line: row.line}}]->(:{label} {{repo: $repo, name: row.object}}) \
                 DELETE r",
                label = kind.object_label(),
                edge = kind.edge(),
            );
            for chunk in rows.chunks(BATCH) {
                self.run(&statement, json!({"repo": repo, "rows": chunk}), signal)?;
            }
        }
        Ok(relations.len())
    }

    /// Makes the mirror of `repo` exactly `relations` (at `rev`): its earlier relations go, then nodes left
    /// without any.
    pub fn replace_relations(
        &self,
        repo: &str,
        rev: &str,
        relations: &[Relation],
        signal: &Signal,
    ) -> Result<usize, String> {
        self.run(
            "MATCH ()-[r {repo: $repo, source: 'ultron'}]->() DELETE r",
            json!({"repo": repo}),
            signal,
        )?;
        let mirrored = self.upsert_relations(repo, rev, relations, signal)?;
        self.run(
            "MATCH (n {repo: $repo}) WHERE (n:Code OR n:ConfigKey OR n:Route OR n:Table) AND NOT (n)--() DELETE n",
            json!({"repo": repo}),
            signal,
        )?;
        Ok(mirrored)
    }

    /// Makes the mirror of memorantado `project` exactly `graph`. Returns how many memories it holds.
    pub fn replace_memories(&self, project: &str, graph: &MemoryGraph, signal: &Signal) -> Result<usize, String> {
        self.run(
            "MATCH (m:Memory {source: 'memorantado', project: $project}) DETACH DELETE m",
            json!({"project": project}),
            signal,
        )?;
        let memories: Vec<Value> = graph
            .memories
            .iter()
            .map(|m| {
                json!({"id": m.id, "key": m.key, "kind": m.kind, "subject": m.subject, "status": m.status,
                    "content": m.content, "tags": m.tags})
            })
            .collect();
        for chunk in memories.chunks(BATCH) {
            self.run(
                "UNWIND $rows AS row CREATE (:Memory {source: 'memorantado', project: $project, id: row.id, \
                 key: row.key, kind: row.kind, subject: row.subject, status: row.status, content: row.content, \
                 tags: row.tags})",
                json!({"project": project, "rows": chunk}),
                signal,
            )?;
        }
        let supersedes: Vec<Value> = graph
            .supersedes
            .iter()
            .map(|(newer, older)| json!({"from": newer, "to": older}))
            .collect();
        for chunk in supersedes.chunks(BATCH) {
            self.run(
                "UNWIND $rows AS row \
                 MATCH (a:Memory {source: 'memorantado', project: $project, id: row.from}), \
                       (b:Memory {source: 'memorantado', project: $project, id: row.to}) \
                 CREATE (a)-[:SUPERSEDES]->(b)",
                json!({"project": project, "rows": chunk}),
                signal,
            )?;
        }
        let conflicts: Vec<Value> = graph
            .conflicts
            .iter()
            .map(|(a, b, reason)| json!({"from": a, "to": b, "reason": reason}))
            .collect();
        for chunk in conflicts.chunks(BATCH) {
            self.run(
                "UNWIND $rows AS row \
                 MATCH (a:Memory {source: 'memorantado', project: $project, id: row.from}), \
                       (b:Memory {source: 'memorantado', project: $project, id: row.to}) \
                 CREATE (a)-[:CONFLICTS_WITH {reason: row.reason}]->(b)",
                json!({"project": project, "rows": chunk}),
                signal,
            )?;
        }
        Ok(graph.memories.len())
    }
}
