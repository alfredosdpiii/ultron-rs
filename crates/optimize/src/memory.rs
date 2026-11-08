//! The memory pipeline: scoped recall on memorantado's `bench:ultron` set (sets/memory-ultron.json, a read-only copy
//! of its src/bench/ultronMemorySet.ts), through the [`MemoryBackend`] trait a session's memory uses.
//!
//! The set is loaded as Ultron writes memories (one document per key, tagged with its scope; a replacement re-retains
//! the key, an invalidation deletes it), then each query is recalled as a session turn recalls: the tags of the
//! session, its project and the global scope, any of them. Measured as memorantado measures it: recall@k and MRR over
//! the queries that expect a memory, scope leakage (results of a scope the session cannot see) and retired leakage
//! (an invalidated memory, or a replaced memory's old text).
//!
//! Failure surface: any leakage fails the run; a backend error makes it incomplete.
//!
//! Offline the backend is [`Reference`], an in-process store with plain word-overlap ranking: it pins the scorer and
//! the failure surface, not a real backend's quality. Live runs use a throwaway bank of the configured server.

use crate::point::{Outcome, Point};
use kernel::Signal;
use kernel_apis::MemoryBackend;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

/// The frozen set.
pub const SET: &str = include_str!("../sets/memory-ultron.json");

/// Results read per query (memorantado's TOP_K).
pub const DEFAULT_LIMIT: usize = 5;

/// The keys a memory variant's config may set.
const CONFIG_KEYS: [&str; 3] = ["backend", "limit", "minScore"];
/// The backends a memory variant may name (memorantado joins once its client implements `MemoryBackend`).
pub const BACKENDS: [&str; 2] = ["reference", "hindsight"];

#[derive(Clone, Debug)]
pub struct Memory {
    pub key: String,
    pub scope: String,
    pub subject: String,
    pub content: String,
    pub kind: String,
    pub evidence: String,
}

#[derive(Clone, Debug)]
pub struct Query {
    pub session: String,
    pub query: String,
    pub expected: Vec<String>,
}

pub struct MemorySet {
    pub hash: String,
    /// Scope name → its tag.
    pub scopes: BTreeMap<String, String>,
    /// Session → its project scope.
    pub session_project: BTreeMap<String, String>,
    pub memories: Vec<Memory>,
    /// `(key, new content)`.
    pub replacements: Vec<(String, String)>,
    pub invalidated: Vec<String>,
    pub queries: Vec<Query>,
}

fn strings(value: &Value, what: &str) -> Result<BTreeMap<String, String>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{what} must be an object"))?
        .iter()
        .map(|(k, v)| {
            v.as_str()
                .map(|v| (k.clone(), v.to_string()))
                .ok_or_else(|| format!("{what}.{k} must be a string"))
        })
        .collect()
}

fn text(value: &Value, key: &str, what: &str) -> Result<String, String> {
    value[key]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{what} needs {key}"))
}

impl MemorySet {
    pub fn load(source: &str) -> Result<MemorySet, String> {
        let file: Value = serde_json::from_str(source).map_err(|e| format!("memory set: not JSON: {e}"))?;
        let list = |key: &str| {
            file[key]
                .as_array()
                .cloned()
                .ok_or(format!("memory set: {key} must be a list"))
        };
        let scopes = strings(&file["scopes"], "scopes")?;
        let session_project = strings(&file["sessionProject"], "sessionProject")?;
        let memories = list("memories")?
            .iter()
            .map(|m| {
                let memory = Memory {
                    key: text(m, "key", "a memory")?,
                    scope: text(m, "scope", "a memory")?,
                    subject: text(m, "subject", "a memory")?,
                    content: text(m, "content", "a memory")?,
                    kind: text(m, "type", "a memory")?,
                    evidence: text(m, "evidence", "a memory")?,
                };
                if scopes.contains_key(&memory.scope) {
                    Ok(memory)
                } else {
                    Err(format!("memory {} has an unknown scope", memory.key))
                }
            })
            .collect::<Result<Vec<_>, String>>()?;
        let known = |key: &str| memories.iter().any(|m| m.key == key);
        let replacements = list("replacements")?
            .iter()
            .map(|r| Ok((text(r, "key", "a replacement")?, text(r, "content", "a replacement")?)))
            .collect::<Result<Vec<_>, String>>()?;
        let invalidated: Vec<String> = list("invalidated")?
            .iter()
            .map(|k| {
                k.as_str()
                    .map(str::to_string)
                    .ok_or("invalidated must list keys".to_string())
            })
            .collect::<Result<_, _>>()?;
        if let Some(key) = replacements
            .iter()
            .map(|(k, _)| k)
            .chain(&invalidated)
            .find(|k| !known(k))
        {
            return Err(format!("memory set: {key} is not a memory"));
        }
        let queries = list("queries")?
            .iter()
            .map(|q| {
                let session = text(q, "session", "a query")?;
                if !session_project.contains_key(&session) || !scopes.contains_key(&session) {
                    return Err(format!("query session {session} has no scope or project"));
                }
                Ok(Query {
                    query: text(q, "query", "a query")?,
                    expected: q["expected"]
                        .as_array()
                        .ok_or("a query needs expected")?
                        .iter()
                        .filter_map(|k| k.as_str().map(str::to_string))
                        .collect(),
                    session,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(MemorySet {
            hash: jsrt::hash::sha256_hex(source),
            scopes,
            session_project,
            memories,
            replacements,
            invalidated,
            queries,
        })
    }

    /// The tags a session's turn may recall: its own, its project's and the global scope's.
    fn visible(&self, session: &str) -> Vec<String> {
        [
            Some(session),
            self.session_project.get(session).map(String::as_str),
            Some("g"),
        ]
        .into_iter()
        .flatten()
        .filter_map(|scope| self.scopes.get(scope).cloned())
        .collect()
    }
}

/// A variant's config checked.
pub fn check_config(config: &Value) -> Result<(), String> {
    let map = config
        .as_object()
        .ok_or("a memory variant's config must be an object")?;
    if let Some(key) = map.keys().find(|k| !CONFIG_KEYS.contains(&k.as_str())) {
        return Err(format!("memory variants set {}; not {key}", CONFIG_KEYS.join(", ")));
    }
    if !map
        .get("backend")
        .and_then(Value::as_str)
        .is_some_and(|b| BACKENDS.contains(&b))
    {
        return Err(format!("backend must be one of {}", BACKENDS.join(", ")));
    }
    if map
        .get("limit")
        .is_some_and(|l| l.as_u64().is_none_or(|l| !(1..=50).contains(&l)))
    {
        return Err("limit must be a whole number from 1 to 50".into());
    }
    if map
        .get("minScore")
        .is_some_and(|s| s.as_f64().is_none_or(|s| !(0.0..=1.0).contains(&s)))
    {
        return Err("minScore must be a number from 0 to 1".into());
    }
    Ok(())
}

/// Write the set into `backend` as Ultron writes memories: the memories, then the replacements, then the
/// invalidations.
pub fn load(backend: &dyn MemoryBackend, set: &MemorySet, signal: &Signal) -> Result<(), String> {
    let retain = |memory: &Memory, content: &str| {
        let tags = json!([set.scopes[&memory.scope]]);
        let request = json!({
            "items": [{
                "content": content, "context": memory.subject, "document_id": memory.key,
                "tags": tags, "observation_scopes": [tags], "update_mode": "replace",
                "metadata": {"ultron_type": memory.kind, "ultron_evidence_class": memory.evidence},
            }],
            "async": false,
        });
        backend
            .retain(&request, signal)
            .map_err(|e| format!("retain {}: {e}", memory.key))
    };
    for memory in &set.memories {
        retain(memory, &memory.content)?;
    }
    for (key, content) in &set.replacements {
        let memory = set.memories.iter().find(|m| &m.key == key).expect("checked at load");
        retain(memory, content)?;
    }
    for key in &set.invalidated {
        backend
            .delete(key, signal)
            .map_err(|e| format!("invalidate {key}: {e}"))?;
    }
    Ok(())
}

/// Recall every query of `set` from `backend` (already [`load`]ed) under `config`, and score it.
pub fn measure(backend: &dyn MemoryBackend, set: &MemorySet, config: &Value, signal: &Signal) -> Point {
    let limit = config["limit"].as_u64().map_or(DEFAULT_LIMIT, |l| l as usize);
    let min_score = config["minScore"].as_f64();
    let scope_of = |key: &str| {
        set.memories
            .iter()
            .find(|m| m.key == key)
            .map(|m| &set.scopes[&m.scope])
    };
    let old_texts: HashSet<&str> = set
        .replacements
        .iter()
        .filter_map(|(key, _)| set.memories.iter().find(|m| &m.key == key))
        .map(|m| m.content.as_str())
        .collect();
    let (mut recall, mut reciprocal, mut scored) = (0.0, 0.0, 0usize);
    let (mut returned, mut out_of_scope, mut retired) = (0usize, 0usize, 0usize);
    let (mut failures, mut ms, mut errors) = (Vec::new(), Vec::new(), 0usize);
    for (index, query) in set.queries.iter().enumerate() {
        let allowed = set.visible(&query.session);
        let mut request = json!({
            "query": query.query, "tags": allowed, "tags_match": "any_strict",
            "types": ["observation", "world", "experience"], "budget": "low", "max_tokens": 1024,
        });
        if let Some(min) = min_score {
            request["min_scores"] = json!({"final": min});
        }
        let started = Instant::now();
        let response = backend.recall(&request, signal);
        ms.push(started.elapsed().as_secs_f64() * 1000.0);
        let items = match response.as_ref().map(|r| r["results"].as_array()) {
            Ok(Some(items)) => items.clone(),
            other => {
                errors += 1;
                let error = other.err().cloned().unwrap_or_else(|| "no results list".into());
                failures.push(json!({"query": index, "failure": "recall failed", "error": error}));
                continue;
            }
        };
        // Results in rank order, one per document (a backend may return several facts of one document).
        let mut seen = HashSet::new();
        let mut ranked: Vec<(Option<String>, String)> = Vec::new();
        for item in &items {
            let key = item["document_id"].as_str().map(str::to_string);
            if key.as_ref().is_some_and(|k| !seen.insert(k.clone())) {
                continue;
            }
            ranked.push((key, item["text"].as_str().unwrap_or_default().to_string()));
            if ranked.len() == limit {
                break;
            }
        }
        returned += ranked.len();
        for (key, text) in &ranked {
            let leaked = key
                .as_deref()
                .and_then(scope_of)
                .is_some_and(|tag| !allowed.contains(tag));
            let stale = key.as_ref().is_some_and(|k| set.invalidated.contains(k)) || old_texts.contains(text.as_str());
            if leaked {
                out_of_scope += 1;
                failures.push(json!({"query": index, "failure": "out of scope", "key": key, "session": query.session}));
            }
            if stale {
                retired += 1;
                failures.push(json!({"query": index, "failure": "retired memory recalled", "key": key}));
            }
        }
        if !query.expected.is_empty() {
            scored += 1;
            let keys: Vec<Option<&str>> = ranked.iter().map(|(k, _)| k.as_deref()).collect();
            let found = query
                .expected
                .iter()
                .filter(|e| keys.contains(&Some(e.as_str())))
                .count();
            recall += found as f64 / query.expected.len() as f64;
            let first = keys
                .iter()
                .position(|k| k.is_some_and(|k| query.expected.iter().any(|e| e == k)));
            reciprocal += first.map_or(0.0, |rank| 1.0 / (rank + 1) as f64);
            if found < query.expected.len() {
                failures.push(json!({
                    "query": index, "failure": "missed", "expected": query.expected,
                    "got": keys, "text": query.query, "session": query.session,
                }));
            }
        }
    }
    ms.sort_by(f64::total_cmp);
    let scored_n = scored.max(1) as f64;
    let leakage = out_of_scope + retired;
    let metrics = json!({
        "quality": "recall@k", "penalty": "leaked results",
        "limit": limit, "queries": set.queries.len(), "recallAtK": recall / scored_n, "mrr": reciprocal / scored_n,
        "scopeLeakage": out_of_scope as f64 / returned.max(1) as f64, "outOfScope": out_of_scope,
        "retiredLeakage": retired, "errors": errors,
        "latencyMs": if ms.is_empty() { Value::Null } else { json!({"p50": ms[ms.len() / 2].round(), "max": ms[ms.len() - 1].round()}) },
    });
    Point {
        pipeline: "memory",
        variant: String::new(),
        version: 0,
        set_hash: set.hash.clone(),
        mode: "offline",
        outcome: if leakage > 0 {
            Outcome::Failed
        } else if errors > 0 {
            Outcome::Incomplete
        } else {
            Outcome::Passed
        },
        quality: recall / scored_n,
        penalty: leakage as f64,
        cost_usd: None,
        metrics,
        failures,
    }
}

/// Words that say nothing about a memory.
const STOPWORDS: [&str; 24] = [
    "the", "and", "for", "with", "what", "how", "can", "should", "are", "was", "this", "that", "who", "where", "when",
    "why", "which", "does", "into", "from", "our", "your", "you", "its",
];

fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3 && !STOPWORDS.contains(w))
        .map(str::to_string)
        .collect()
}

struct Document {
    content: String,
    context: String,
    tags: Vec<String>,
}

/// The offline backend: documents by id, ranked by the share of the query's words they hold (content and
/// context), filtered by tags (any of them; an untagged document never matches).
#[derive(Default)]
pub struct Reference {
    documents: Mutex<BTreeMap<String, Document>>,
}

impl MemoryBackend for Reference {
    fn namespace(&self) -> String {
        "reference".into()
    }

    fn scope_tags(&self) -> Map<String, Value> {
        Map::new()
    }

    fn supports(&self, call: &str) -> bool {
        matches!(call, "recall" | "retain" | "delete")
    }

    fn recall(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let query = words(request["query"].as_str().unwrap_or_default());
        let tags: Vec<&str> = request["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let min = request["min_scores"]["final"].as_f64().unwrap_or(0.0);
        let documents = jsrt::sync::lock(&self.documents);
        let mut hits: Vec<(f64, &String, &Document)> = documents
            .iter()
            .filter(|(_, d)| d.tags.iter().any(|t| tags.contains(&t.as_str())))
            .filter_map(|(id, d)| {
                let held = words(&format!("{} {}", d.content, d.context));
                let score = query.intersection(&held).count() as f64 / query.len().max(1) as f64;
                (score > 0.0 && score >= min).then_some((score, id, d))
            })
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let results: Vec<Value> = hits
            .iter()
            .map(|(score, id, d)| json!({"id": id, "document_id": id, "text": d.content, "tags": d.tags, "score": score}))
            .collect();
        Ok(json!({"results": results}))
    }

    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let mut documents = jsrt::sync::lock(&self.documents);
        for item in request["items"].as_array().ok_or("INVALID_INPUT")? {
            let id = item["document_id"].as_str().ok_or("INVALID_INPUT")?;
            documents.insert(
                id.to_string(),
                Document {
                    content: item["content"].as_str().unwrap_or_default().to_string(),
                    context: item["context"].as_str().unwrap_or_default().to_string(),
                    tags: item["tags"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t.as_str().map(str::to_string))
                        .collect(),
                },
            );
        }
        Ok(json!({"success": true}))
    }

    fn get(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }

    fn delete(&self, document_id: &str, _: &Signal) -> Result<Value, String> {
        match jsrt::sync::lock(&self.documents).remove(document_id) {
            Some(_) => Ok(json!({"success": true})),
            None => Err("UNKNOWN_MEMORY".into()),
        }
    }

    fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
}
