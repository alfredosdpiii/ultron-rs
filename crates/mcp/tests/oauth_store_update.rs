//! A provider's change to a server's OAuth state is one read-modify-write under the file lock: what another process
//! wrote after this one read the state (a refresh's rotated tokens) is kept, not written back over.

use mcp::oauth::flow::Provider;
use mcp::oauth::store::CredentialStore;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const URL: &str = "https://mcp.example.test/mcp";

#[test]
fn mcp_oauth_provider_change_keeps_tokens_another_process_wrote_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let other = Arc::new(CredentialStore::new(dir.path(), None)).for_server("svc", URL);
    other
        .save(&json!({ "serverUrl": URL, "tokens": { "access_token": "a1", "refresh_token": "r1" }, "codeVerifier": "v" }))
        .unwrap();
    // The masker hears every state this process reads: the first time, "another process" rotates the tokens. With a
    // load and a save under two lock holds, that write landed between them and was overwritten.
    let rotated = Arc::new(AtomicBool::new(false));
    let rotate: Arc<Mutex<Option<mcp::oauth::store::ServerStore>>> = Arc::new(Mutex::new(Some(other.clone())));
    let sink: mcp::connection::SecretSink = {
        let rotated = rotated.clone();
        let rotate = rotate.clone();
        Arc::new(move |_| {
            if let Some(other) = rotate.lock().unwrap().take() {
                let mut state = other.load().unwrap().unwrap();
                state["tokens"] = json!({ "access_token": "a2", "refresh_token": "r2" });
                other.save(&state).unwrap();
                rotated.store(true, Ordering::SeqCst);
            }
        })
    };
    let store = Arc::new(CredentialStore::new(dir.path(), Some(sink))).for_server("svc", URL);
    let provider = Provider::new(
        URL,
        "http://127.0.0.1:1/callback",
        "c",
        None,
        None,
        &store,
        Box::new(|_| {}),
    );
    provider.invalidate("verifier").unwrap();
    assert!(rotated.load(Ordering::SeqCst));
    let state = other.load().unwrap().unwrap();
    assert_eq!(state["tokens"]["refresh_token"], "r2");
    assert_eq!(state.get("codeVerifier"), None::<&Value>);
}
