//! The usage ledger as session values. The ledger is one document (`{version, roots: {<rootId>: root}, history}`),
//! and the session file is a log that stores a value whole on every write, so the document is kept in parts:
//! - the head at `ultron.usage/root`: every field but the roots;
//! - each root at `ultron.usage/roots/<rootId>` as `{index, root}`, `index` being its place in the document and
//!   `root` the root without its call records;
//! - a root's call records as the list `ultron.usage/calls/<rootId>`, one append per call;
//! - a root the ledger folded into `history` at `ultron.usage/folded/<rootId>`, whole, for the usage report: the
//!   ledger does not load it again, so a restart cannot fold it twice.
//!
//! A change then appends what changed. Sessions written before this hold every root inside the head; they read the
//! same, and their first ledger write moves the roots out.

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use crate::ns::USAGE;
use crate::{Session, Write};

const HEAD: &str = "root";
pub const ROOT_PREFIX: &str = "roots/";
pub const FOLDED_PREFIX: &str = "folded/";
pub const CALLS_PREFIX: &str = "calls/";

/// `root` with `calls` after its reservations, where the ledger writes them; a root that has its calls is returned
/// as it is.
fn with_calls(root: &Value, calls: Vec<Value>) -> Value {
    let Some(fields) = root.as_object().filter(|fields| !fields.contains_key("calls")) else {
        return root.clone();
    };
    let mut calls = Some(Value::Array(calls));
    let mut whole = Map::new();
    for (name, value) in fields {
        whole.insert(name.clone(), value.clone());
        if name == "reservations" {
            whole.extend(calls.take().map(|calls| ("calls".to_string(), calls)));
        }
    }
    whole.extend(calls.map(|calls| ("calls".to_string(), calls)));
    Value::Object(whole)
}

fn without_calls(root: &Value) -> Value {
    let mut body = root.clone();
    if let Some(fields) = body.as_object_mut() {
        fields.shift_remove("calls");
    }
    body
}

/// The ledger document from its stored parts: the head, the keyed roots (`(rootId, {index, root})`) and each root's
/// call records.
pub fn assemble<'a>(
    head: Option<&Value>,
    keyed: impl IntoIterator<Item = (&'a str, &'a Value)>,
    calls: impl Fn(&str) -> Vec<Value>,
) -> Option<Value> {
    let mut keyed: Vec<(&str, &Value)> = keyed.into_iter().collect();
    if head.is_none() && keyed.is_empty() {
        return None;
    }
    keyed.sort_by_key(|(_, stored)| stored["index"].as_u64());
    let mut document = head.cloned().unwrap_or_else(|| json!({"version": 1}));
    let fields = document.as_object_mut()?;
    if !fields.get("roots").is_some_and(Value::is_object) {
        fields.insert("roots".into(), json!({}));
    }
    for (id, stored) in keyed {
        document["roots"][id] = with_calls(&stored["root"], calls(id));
    }
    Some(document)
}

fn stored_calls(session: &Session, id: &str) -> Vec<Value> {
    session
        .state()
        .list(USAGE, &format!("{CALLS_PREFIX}{id}"))
        .iter()
        .map(|element| element.value.clone())
        .collect()
}

fn read_parts(session: &Session, prefixes: &[&str]) -> Option<Value> {
    let state = session.state();
    let keyed: Vec<(&str, &Value)> = prefixes
        .iter()
        .flat_map(|prefix| {
            state
                .scan_values(USAGE, prefix)
                .into_iter()
                .map(|stored| (&stored.key[prefix.len()..], &stored.value))
        })
        .collect();
    assemble(session.value(USAGE, HEAD), keyed, |id| stored_calls(session, id))
}

/// The ledger document the usage ledger works on: the roots it has not folded.
pub fn read(session: &Session) -> Option<Value> {
    read_parts(session, &[ROOT_PREFIX])
}

/// The ledger document with the folded roots too, as the usage report reads it.
pub fn read_all(session: &Session) -> Option<Value> {
    read_parts(session, &[FOLDED_PREFIX, ROOT_PREFIX])
}

/// One root of the session's ledger, without its call records unless an old head holds it.
pub fn root(session: &Session, id: &str) -> Option<Value> {
    match session.value(USAGE, &format!("{ROOT_PREFIX}{id}")) {
        Some(stored) => Some(stored["root"].clone()),
        None => session.value(USAGE, HEAD)?["roots"].get(id).cloned(),
    }
}

/// The writes that store `document`: its head when that changed, each root that differs from the stored one, and
/// the call records a root gained. A stored root the document lacks stays, unless `folded` names it: that one moves
/// to the folded roots.
pub fn writes(session: &Session, document: &Value, folded: &BTreeSet<String>) -> Vec<Write> {
    let Some(fields) = document.as_object() else {
        return vec![Write::set(USAGE, HEAD, document.clone())];
    };
    let head: Map<String, Value> = fields
        .iter()
        .map(|(name, value)| (name.clone(), if name == "roots" { json!({}) } else { value.clone() }))
        .collect();
    let head = Value::Object(head);
    let stored_head = session.value(USAGE, HEAD);
    let mut writes = Vec::new();
    if stored_head != Some(&head) {
        writes.push(Write::set(USAGE, HEAD, head));
    }
    let state = session.state();
    let mut next = [ROOT_PREFIX, FOLDED_PREFIX]
        .iter()
        .flat_map(|prefix| state.scan_values(USAGE, prefix))
        .filter_map(|stored| stored.value["index"].as_u64())
        .max()
        .map_or(0, |last| last + 1);
    let mut mint = || {
        next += 1;
        json!(next - 1)
    };
    let roots = fields.get("roots").and_then(Value::as_object);
    let old_head = stored_head.and_then(|head| head["roots"].as_object());
    for id in folded {
        let key = format!("{ROOT_PREFIX}{id}");
        let calls_key = format!("{CALLS_PREFIX}{id}");
        let (index, root) = match (state.value(USAGE, &key), old_head.and_then(|roots| roots.get(id))) {
            (Some(stored), _) => {
                writes.push(Write::delete(USAGE, &key));
                let root = with_calls(&stored.value["root"], stored_calls(session, id));
                (stored.value["index"].clone(), root)
            }
            (None, Some(root)) => (mint(), root.clone()),
            (None, None) => continue,
        };
        if !state.list(USAGE, &calls_key).is_empty() {
            writes.push(Write::delete_list(USAGE, &calls_key));
        }
        writes.push(Write::set(
            USAGE,
            &format!("{FOLDED_PREFIX}{id}"),
            json!({"index": index, "root": root}),
        ));
    }
    // Roots still inside an old head come first: they were there before any root this document adds.
    let inside_head = old_head
        .into_iter()
        .flatten()
        .filter(|(id, _)| !folded.contains(*id) && !roots.is_some_and(|roots| roots.contains_key(*id)));
    for (id, root) in inside_head.chain(roots.into_iter().flatten()) {
        let key = format!("{ROOT_PREFIX}{id}");
        let body = without_calls(root);
        match state.value(USAGE, &key) {
            Some(stored) if stored.value["root"] == body => {}
            Some(stored) => writes.push(Write::set(
                USAGE,
                &key,
                json!({"index": stored.value["index"], "root": body}),
            )),
            None => writes.push(Write::set(USAGE, &key, json!({"index": mint(), "root": body}))),
        }
        // A root without the field (one read with [`root`]) leaves its records as they are.
        let Some(calls) = root["calls"].as_array() else {
            continue;
        };
        let calls_key = format!("{CALLS_PREFIX}{id}");
        let stored = state.list(USAGE, &calls_key);
        let kept = stored.len() <= calls.len() && stored.iter().zip(calls).all(|(old, new)| old.value == *new);
        if !kept {
            writes.push(Write::delete_list(USAGE, &calls_key));
        }
        let from = if kept { stored.len() } else { 0 };
        writes.extend(
            calls[from..]
                .iter()
                .map(|call| Write::append(USAGE, &calls_key, call.clone())),
        );
    }
    writes
}

/// The writes that store one root, leaving the rest of the ledger as it is.
pub fn root_writes(session: &Session, id: &str, root: Value) -> Vec<Write> {
    let mut document = match session.value(USAGE, HEAD) {
        Some(Value::Object(head)) => {
            let mut head = head.clone();
            head.insert("roots".into(), json!({}));
            Value::Object(head)
        }
        _ => json!({"version": 1, "roots": {}}),
    };
    document["roots"][id] = root;
    writes(session, &document, &BTreeSet::new())
}
