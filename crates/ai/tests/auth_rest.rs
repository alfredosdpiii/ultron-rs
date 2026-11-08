//! Replays the `fileOps` of `fixtures/models-auth.json` (recorded by `fixtures/generate-rest.mjs auth`): the
//! `auth.json` store must leave the file coding-agent's `AuthStorage` leaves.

use ai::auth::{Credential, CredentialStore, FileCredentialStore};
use ai::json::normalize_numbers;
use serde_json::Value;
use std::sync::LazyLock;

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/models-auth.json")).expect("fixture parses"));

#[test]
fn ai_auth_json_store_writes_what_coding_agent_writes() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent").join("auth.json");
    let store = FileCredentialStore::new(&path);
    for entry in FIXTURE["fileOps"].as_array().unwrap() {
        let step = &entry["step"];
        let provider = step["provider"].as_str().unwrap_or_default();
        let out = runtime.block_on(async {
            match step["op"].as_str().unwrap() {
                "modify" => {
                    let credential: Credential = serde_json::from_value(step["credential"].clone()).unwrap();
                    store
                        .modify(
                            provider,
                            Box::new(move |_| Box::pin(async move { Ok(Some(credential)) })),
                            None,
                        )
                        .await
                        .unwrap();
                    Value::Null
                }
                "read" => serde_json::to_value(store.read(provider).await.unwrap()).unwrap(),
                "delete" => {
                    store.delete(provider).await.unwrap();
                    Value::Null
                }
                "list" => serde_json::to_value(store.list().await.unwrap()).unwrap(),
                other => panic!("unknown op {other}"),
            }
        });
        assert_eq!(
            normalize_numbers(out),
            normalize_numbers(entry["out"].clone()),
            "{step}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        FIXTURE["fileText"].as_str().unwrap()
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}
