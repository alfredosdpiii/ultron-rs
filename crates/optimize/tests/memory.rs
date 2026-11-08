//! The memory pipeline: memorantado's bench:ultron set loaded through `MemoryBackend` and recalled as a session turn
//! recalls; the reference backend offline, and backends that leak, fail or rank differently.

use kernel::Signal;
use kernel_apis::MemoryBackend;
use optimize::Outcome;
use optimize::memory::{MemorySet, Reference, SET, check_config, load, measure};
use serde_json::{Map, Value, json};
use std::sync::Mutex;

fn set() -> MemorySet {
    MemorySet::load(SET).unwrap()
}

fn reference(config: Value) -> optimize::Point {
    let set = set();
    let backend = Reference::default();
    load(&backend, &set, &Signal::new()).unwrap();
    measure(&backend, &set, &config, &Signal::new())
}

#[test]
fn the_memory_set_is_frozen() {
    let set = set();
    assert_eq!(
        set.hash, "fd4a8c566b9afadbdd52c2ab165a9ac42aebee7ada7482aa6a6d7613b3031050",
        "memory-ultron.json changed: a frozen set is never edited (add a new set instead)"
    );
    assert_eq!((set.memories.len(), set.queries.len()), (17, 12));
    assert_eq!(set.invalidated, ["p1-journal"]);
}

#[test]
fn the_reference_backend_recalls_within_scope_and_never_a_retired_memory() {
    let point = reference(json!({"backend": "reference"}));
    assert_eq!(point.outcome, Outcome::Passed, "{:?}", point.failures);
    assert_eq!(point.penalty, 0.0);
    assert_eq!(point.metrics["scopeLeakage"], 0.0);
    assert_eq!(point.metrics["retiredLeakage"], 0);
    assert_eq!(point.cost_usd, None);
    let recall = point.metrics["recallAtK"].as_f64().unwrap();
    assert!(recall > 0.5, "{}", point.metrics);
    assert_eq!(point.quality, recall);
    assert!(point.metrics["mrr"].as_f64().unwrap() <= recall);
    // Misses are kept as failures, with what came back.
    for failure in &point.failures {
        assert_eq!(failure["failure"], "missed", "{failure}");
    }
}

#[test]
fn a_higher_min_score_returns_less() {
    let all = reference(json!({"backend": "reference"}));
    let strict = reference(json!({"backend": "reference", "minScore": 1.0}));
    assert!(strict.quality <= all.quality);
    assert!(strict.failures.len() >= all.failures.len());
}

/// Ignores tags and retirements: returns every document ever written, the oldest text of each.
#[derive(Default)]
struct Leaky {
    documents: Mutex<Vec<(String, String)>>,
}

impl MemoryBackend for Leaky {
    fn namespace(&self) -> String {
        "leaky".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        Map::new()
    }
    fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        let documents = self.documents.lock().unwrap();
        let results: Vec<Value> = documents
            .iter()
            .map(|(id, text)| json!({"id": id, "document_id": id, "text": text}))
            .collect();
        Ok(json!({"results": results}))
    }
    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        let item = &request["items"][0];
        let id = item["document_id"].as_str().unwrap().to_string();
        let mut documents = self.documents.lock().unwrap();
        if !documents.iter().any(|(d, _)| *d == id) {
            documents.push((id, item["content"].as_str().unwrap().to_string()));
        }
        Ok(json!({}))
    }
    fn get(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
    fn delete(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn operation(&self, _: &str, _: &Signal) -> Result<Value, String> {
        Err("UNSUPPORTED_API".into())
    }
}

#[test]
fn a_backend_that_leaks_scopes_or_retired_memories_fails() {
    let set = set();
    let backend = Leaky::default();
    load(&backend, &set, &Signal::new()).unwrap();
    let point = measure(
        &backend,
        &set,
        &json!({"backend": "reference", "limit": 50}),
        &Signal::new(),
    );
    assert_eq!(point.outcome, Outcome::Failed);
    assert!(point.metrics["outOfScope"].as_u64().unwrap() > 0);
    assert!(
        point.metrics["retiredLeakage"].as_u64().unwrap() > 0,
        "the invalidated and the replaced texts"
    );
    let kinds: Vec<&str> = point.failures.iter().filter_map(|f| f["failure"].as_str()).collect();
    assert!(
        kinds.contains(&"out of scope") && kinds.contains(&"retired memory recalled"),
        "{kinds:?}"
    );
    assert_eq!(
        point.penalty,
        (point.metrics["outOfScope"].as_u64().unwrap() + point.metrics["retiredLeakage"].as_u64().unwrap()) as f64
    );
}

/// Loads, then cannot recall.
struct Down(Reference);

impl MemoryBackend for Down {
    fn namespace(&self) -> String {
        "down".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        Map::new()
    }
    fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        Err("HTTP_ERROR".into())
    }
    fn retain(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        self.0.retain(request, signal)
    }
    fn get(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.0.get(id, signal)
    }
    fn delete(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.0.delete(id, signal)
    }
    fn operation(&self, id: &str, signal: &Signal) -> Result<Value, String> {
        self.0.operation(id, signal)
    }
}

#[test]
fn a_backend_that_errors_is_incomplete() {
    let set = set();
    let backend = Down(Reference::default());
    load(&backend, &set, &Signal::new()).unwrap();
    let point = measure(&backend, &set, &json!({"backend": "reference"}), &Signal::new());
    assert_eq!(point.outcome, Outcome::Incomplete);
    assert_eq!(point.metrics["errors"], 12);
    assert_eq!(point.failures[0]["error"], "HTTP_ERROR");
}

#[test]
fn memory_variant_configs_are_checked() {
    check_config(&json!({"backend": "hindsight", "limit": 10, "minScore": 0.3})).unwrap();
    for (config, expected) in [
        (json!({}), "backend"),
        (json!({"backend": "memorantado"}), "backend"),
        (json!({"backend": "reference", "limit": 0}), "limit"),
        (json!({"backend": "reference", "minScore": 2}), "minScore"),
        (json!({"backend": "reference", "query": "x"}), "not query"),
    ] {
        let error = check_config(&config).err().unwrap();
        assert!(error.contains(expected), "{config}: {error}");
    }
    assert!(MemorySet::load(r#"{"scopes": {}}"#).is_err());
}
