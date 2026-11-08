//! The Python clients of runtime.py against `kernel::ops::OPS`: `ops_clients.py` reads every
//! `bridge.request("<op>", payload)` call from the source, with the payload keys it can send and the reply keys it
//! reads, and each must be a row of the table and stay within it; every row must still have a client.

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

fn clients() -> serde_json::Map<String, Value> {
    let rlm = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm");
    let mut sources: Vec<_> = std::fs::read_dir(&rlm)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "py"))
        .collect();
    sources.sort();
    let output = std::process::Command::new(kernel::env::python(|name| std::env::var(name).ok()))
        .arg("-I")
        .arg("-W")
        .arg("ignore")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ops_clients.py"))
        .args(&sources)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    serde_json::from_slice::<Value>(&output.stdout)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

fn names(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn kernel_ops_table_covers_every_python_client_request() {
    let clients = clients();
    let mut problems = Vec::new();
    for (name, sent) in &clients {
        let sites = &sent["sites"];
        let Some(op) = kernel::ops::find(name) else {
            problems.push(format!("{name} ({sites}) is not in kernel::ops::OPS"));
            continue;
        };
        let payload: BTreeSet<String> = op.payload.iter().map(|key| key.to_string()).collect();
        let extra: Vec<String> = names(&sent["payload"]).difference(&payload).cloned().collect();
        if !extra.is_empty() {
            problems.push(format!(
                "{name} ({sites}) sends payload keys outside its row: {extra:?}"
            ));
        }
        let reply: BTreeSet<String> = op.reply.iter().map(|key| key.to_string()).collect();
        let extra: Vec<String> = names(&sent["reply"]).difference(&reply).cloned().collect();
        if !extra.is_empty() {
            problems.push(format!("{name} ({sites}) reads reply keys outside its row: {extra:?}"));
        }
    }
    for op in kernel::ops::OPS {
        if !clients.contains_key(op.name) {
            problems.push(format!("{} ({}) has no Python client", op.name, op.owner));
        }
    }
    let mut seen = BTreeSet::new();
    for op in kernel::ops::OPS {
        if !seen.insert(op.name) {
            problems.push(format!("{} is in kernel::ops::OPS twice", op.name));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
