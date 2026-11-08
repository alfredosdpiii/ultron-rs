//! Payload checks that follow JavaScript once the helpers moved to `jsrt`.

use host::{AllowAll, Host};
use kernel::{HostHandler, Signal};
use serde_json::{Value, json};

fn call(host: &Host, kind: &str, payload: Value) -> Result<Value, String> {
    let Value::Object(payload) = payload else {
        unreachable!()
    };
    host.handle(kind, payload, &Signal::new())
}

#[test]
fn host_shell_read_cursor_must_be_a_safe_integer() {
    // `Number.isSafeInteger(2 ** 60)` is false: a cursor that only fits a u64 is refused like any other bad cursor.
    let tmp = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(tmp.path()).unwrap();
    let host = Host::new(&dir, dir.join(".jobs"), &dir, Box::new(AllowAll));
    let started = call(&host, "shell.run", json!({ "command": "true", "yield_after": 0 })).unwrap();
    let id = started["id"].clone();
    for cursor in [json!(1u64 << 60), json!(u64::MAX)] {
        let read = call(&host, "shell.read", json!({ "id": id, "cursor": cursor }));
        assert_eq!(read, Err("cursor must be a non-negative integer".into()));
    }
    assert!(call(&host, "shell.read", json!({ "id": id, "cursor": 0 })).is_ok());
}
