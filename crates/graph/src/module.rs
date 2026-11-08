//! `knowledge.*` from the REPL (`rlm/knowledge_api.py`), over the session directory's git repository at `HEAD`:
//!
//! - `knowledge.relations {kinds, paths}`: the relations of the files under `paths` (crates/knowledge
//!   `relations`), pruned for the task, with the ones plain code could not settle as `pending` (each with its
//!   lines and the judge's question). The client asks the judge about those and settles them.
//! - `knowledge.settle {rev, verdicts}`: the judge's verdicts, cached with their relations.
//! - `knowledge.mirror {}`: the whole mirror again: the relations read so far at `HEAD`, and memorantado's memory
//!   relations.
//!
//! When `graph.neo4j.url` is set, `relations` and `settle` also mirror what they accepted (and remove what the
//! judge refused); a mirror that fails is reported in the reply (`mirror.error`) and the request still succeeds.

use crate::memory::Memorantado;
use crate::neo4j::{Neo4j, Password};
use blocking_http::Transport;
use config::keys::{MemorantadoSettings, Neo4jSettings};
use kernel::Signal;
use kernel_apis::{ApiModule, Request};
use knowledge::relations::{Kind, Relation};
use knowledge::{Settled, Settlement, Store};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// The services the mirror writes to and reads from, as the global settings name them now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mirrors {
    pub neo4j: Option<Neo4jSettings>,
    /// Set when memory lives in memorantado.
    pub memorantado: Option<MemorantadoSettings>,
}

pub type MirrorSource = Arc<dyn Fn() -> Mirrors + Send + Sync>;
/// The HTTP transport, built at the first mirror.
pub type TransportFactory = Arc<dyn Fn() -> Result<Arc<dyn Transport>, String> + Send + Sync>;

/// Paths, kinds and verdicts one request may carry.
pub const MAX_PATHS: usize = 200;
pub const MAX_VERDICTS: usize = 2000;

pub struct KnowledgeModule {
    db: PathBuf,
    cwd: PathBuf,
    mirrors: MirrorSource,
    build: TransportFactory,
    transport: Mutex<Option<Arc<dyn Transport>>>,
    password: Password,
}

/// The repository of the session directory: (its stable name in the store, its top-level directory).
fn repository(cwd: &Path) -> Result<(String, PathBuf), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "knowledge needs a git repository, and {} is not in one",
            cwd.display()
        ));
    }
    let top = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    // As autoreview names a checkout it reviews without a GitHub name, so both share the store's trees.
    Ok((format!("local:{}", top.display()), top))
}

fn head(top: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(top)
        .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("git: {error}"))?;
    if !output.status.success() {
        return Err("the repository has no commit yet: relations are read from HEAD".into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn strings(op: &str, field: &str, value: Option<&Value>, max: usize) -> Result<Vec<String>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) if items.len() <= max => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{op} {field} must be a list of strings"))
            })
            .collect(),
        Some(Value::Array(_)) => Err(format!("{op} {field} takes at most {max} entries")),
        Some(_) => Err(format!("{op} {field} must be a list of strings")),
    }
}

fn kinds(value: Option<&Value>) -> Result<Vec<Kind>, String> {
    let names = strings("knowledge.relations", "kinds", value, Kind::ALL.len())?;
    names
        .iter()
        .map(|name| {
            Kind::parse(name).ok_or_else(|| {
                let known: Vec<&str> = Kind::ALL.iter().map(|kind| kind.name()).collect();
                format!(
                    "knowledge.relations kinds: unknown kind {name:?} (known: {})",
                    known.join(", ")
                )
            })
        })
        .collect()
}

/// Repository-relative paths: no absolute path, no `..`.
fn paths(value: Option<&Value>) -> Result<Vec<String>, String> {
    let paths = strings("knowledge.relations", "paths", value, MAX_PATHS)?;
    for path in &paths {
        if path.starts_with('/') || path.split('/').any(|part| part == "..") {
            return Err(format!(
                "knowledge.relations paths are relative to the repository root: {path:?}"
            ));
        }
    }
    Ok(paths)
}

fn relation_json(relation: &Relation) -> Value {
    json!({
        "id": relation.id,
        "kind": relation.kind.name(),
        "subject": relation.subject,
        "object": relation.object,
        "path": relation.path,
        "line": relation.line,
        "evidence": relation.evidence(),
        "quote": relation.quote,
        "status": relation.status,
        "checked_by": relation.checked_by,
        "rung": relation.rung,
        "model": relation.model,
        "confidence": relation.confidence,
    })
}

fn settlement(value: &Value) -> Result<Settlement, String> {
    let invalid = || {
        "knowledge.settle verdicts are {id, status: accepted|rejected|review, backend, rung, model, confidence}"
            .to_string()
    };
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    Ok(Settlement {
        id: value.get("id").and_then(Value::as_i64).ok_or_else(invalid)?,
        status: text("status").and_then(|s| Settled::parse(&s)).ok_or_else(invalid)?,
        backend: text("backend"),
        rung: text("rung"),
        model: text("model"),
        confidence: value.get("confidence").and_then(Value::as_f64),
    })
}

impl KnowledgeModule {
    /// The module over the store at `db`, for the repository of `cwd`. `password` is Neo4j's (env or auth.json).
    pub fn new(
        db: PathBuf,
        cwd: PathBuf,
        mirrors: MirrorSource,
        build: TransportFactory,
        password: Password,
    ) -> KnowledgeModule {
        KnowledgeModule {
            db,
            cwd,
            mirrors,
            build,
            transport: Mutex::new(None),
            password,
        }
    }

    fn transport(&self) -> Result<Arc<dyn Transport>, String> {
        let mut slot = self.transport.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(transport) = slot.as_ref() {
            return Ok(transport.clone());
        }
        let built = (self.build)()?;
        *slot = Some(built.clone());
        Ok(built)
    }

    /// The Neo4j mirror, when `graph.neo4j.url` is set.
    fn neo4j(&self, mirrors: &Mirrors) -> Option<Result<Neo4j, String>> {
        let settings = mirrors.neo4j.clone()?;
        Some(
            self.transport()
                .map(|transport| Neo4j::new(transport, settings, self.password.clone())),
        )
    }

    /// Runs `work` on the mirror: `null` when there is none, else `{mirrored}` or `{error}`.
    fn mirror(&self, work: impl FnOnce(&Neo4j) -> Result<usize, String>) -> Value {
        match self.neo4j(&(self.mirrors)()) {
            None => Value::Null,
            Some(Ok(neo4j)) => match work(&neo4j) {
                Ok(count) => json!({"mirrored": count}),
                Err(error) => json!({"error": error}),
            },
            Some(Err(error)) => json!({"error": error}),
        }
    }

    fn relations(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("knowledge.relations", request.payload)?;
        let kinds = kinds(request.payload.get("kinds"))?;
        let scope = paths(request.payload.get("paths"))?;
        let (repo, top) = repository(&self.cwd)?;
        let rev = head(&top)?;
        let found = knowledge::relations(&self.db, &repo, &top, &rev, &kinds, &scope).map_err(|e| e.to_string())?;
        let kept = &found.pruned.kept;
        let accepted: Vec<Relation> = kept.iter().filter(|r| r.status == "accepted").cloned().collect();
        let mirror = if accepted.is_empty() {
            Value::Null
        } else {
            self.mirror(|neo4j| neo4j.upsert_relations(&repo, &found.rev, &accepted, request.signal))
        };
        let pending: Vec<Value> = kept
            .iter()
            .filter(|r| r.status == "proposed")
            .map(|r| {
                json!({
                    "id": r.id,
                    "kind": r.kind.name(),
                    "subject": r.subject,
                    "object": r.object,
                    "path": r.path,
                    "line": r.line,
                    "context": r.context,
                    "question": r.kind.question(&r.subject, &r.object),
                })
            })
            .collect();
        Ok(json!({
            "repo": repo,
            "rev": found.rev,
            "relations": kept.iter().map(relation_json).collect::<Vec<_>>(),
            "pending": pending,
            "pruned": found.pruned.dropped,
            "files": found.files,
            "read": found.read,
            "truncated": found.truncated,
            "mirror": mirror,
        }))
    }

    fn settle(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("knowledge.settle", request.payload)?;
        let rev = request
            .payload
            .get("rev")
            .and_then(Value::as_str)
            .ok_or("knowledge.settle rev must be the commit knowledge.relations answered")?;
        let verdicts = match request.payload.get("verdicts") {
            Some(Value::Array(items)) if items.len() <= MAX_VERDICTS => {
                items.iter().map(settlement).collect::<Result<Vec<_>, _>>()?
            }
            _ => {
                return Err(format!(
                    "knowledge.settle verdicts must be a list of at most {MAX_VERDICTS}"
                ));
            }
        };
        let (repo, _) = repository(&self.cwd)?;
        let mut store = Store::open(&self.db).map_err(|e| e.to_string())?;
        let tree = store
            .tree_id(&repo, rev)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("knowledge.settle: no relations were read at {rev}"))?;
        let settled = store.settle_relations(&verdicts).map_err(|e| e.to_string())?;
        let ids: std::collections::HashSet<i64> = verdicts.iter().map(|v| v.id).collect();
        let changed: Vec<Relation> = store
            .relations(tree, None)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|r| ids.contains(&r.id))
            .collect();
        let (accepted, removed): (Vec<Relation>, Vec<Relation>) =
            changed.into_iter().partition(|r| r.status == "accepted");
        let mirror = self.mirror(|neo4j| {
            neo4j.remove_relations(&repo, &removed, request.signal)?;
            neo4j.upsert_relations(&repo, rev, &accepted, request.signal)
        });
        Ok(json!({"settled": settled, "mirror": mirror}))
    }

    /// The relations read so far at `HEAD`, mirrored as they are now (judged ones by their verdicts).
    fn mirror_relations(&self, neo4j: &Neo4j, signal: &Signal) -> Result<usize, String> {
        let (repo, top) = repository(&self.cwd)?;
        let rev = head(&top)?;
        let read = Store::open(&self.db)
            .and_then(|store| match store.tree_id(&repo, &rev)? {
                Some(tree) => store.relation_paths(tree),
                None => Ok(Vec::new()),
            })
            .map_err(|e| e.to_string())?;
        let accepted: Vec<Relation> = if read.is_empty() {
            Vec::new()
        } else {
            knowledge::relations(&self.db, &repo, &top, &rev, &[], &read)
                .map_err(|e| e.to_string())?
                .pruned
                .kept
                .into_iter()
                .filter(|r| r.status == "accepted")
                .collect()
        };
        neo4j.replace_relations(&repo, &rev, &accepted, signal)
    }

    fn mirror_all(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("knowledge.mirror", request.payload)?;
        let mirrors = (self.mirrors)();
        let neo4j = match self.neo4j(&mirrors) {
            None => return Err("knowledge.mirror: graph.neo4j.url is not set, so there is no mirror".into()),
            Some(neo4j) => neo4j?,
        };
        let mut out = Map::new();
        let mut errors = Vec::new();
        match self.mirror_relations(&neo4j, request.signal) {
            Ok(count) => {
                out.insert("relations".into(), json!(count));
            }
            Err(error) => errors.push(format!("relations: {error}")),
        }
        if let Some(settings) = mirrors.memorantado {
            let memorantado = Memorantado::new(self.transport()?, settings);
            let mirrored = memorantado
                .read(request.signal)
                .and_then(|graph| neo4j.replace_memories(memorantado.project(), &graph, request.signal));
            match mirrored {
                Ok(count) => {
                    out.insert("memories".into(), json!(count));
                }
                Err(error) => errors.push(format!("memories: {error}")),
            }
        }
        out.insert("errors".into(), json!(errors));
        Ok(Value::Object(out))
    }
}

impl ApiModule for KnowledgeModule {
    fn prefixes(&self) -> &[&str] {
        &["knowledge."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "knowledge.relations" => self.relations(request),
            "knowledge.settle" => self.settle(request),
            "knowledge.mirror" => self.mirror_all(request),
            other => Err(format!("Unknown knowledge request: {other}")),
        }
    }
}
