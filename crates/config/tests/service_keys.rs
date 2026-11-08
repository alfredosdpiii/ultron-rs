//! Services' credentials (config::auth `service_key`): names no model provider uses, the environment before
//! auth.json, a stored credential bound to the server it was saved for, https unless the server is this machine;
//! and the URL and backend-name helpers the memory services share. New in Rust: literal expectations.

use config::auth::{
    D1_KEY, KeyOrigin, NEO4J_PASSWORD, TYPESAFE_KEY, display_url, key_origin, save_service_key, secure_url,
    service_key, url_origin,
};
use serde_json::json;

fn fake(label: &str) -> String {
    ["fake-", label, "-key"].concat()
}

#[test]
fn config_service_keys_never_share_a_model_providers_name() {
    for key in [TYPESAFE_KEY, D1_KEY, NEO4J_PASSWORD] {
        assert!(key.entry.contains(':'), "{key:?}");
    }
    assert_eq!(
        [TYPESAFE_KEY.env, D1_KEY.env, NEO4J_PASSWORD.env],
        ["TYPESAFE_API_KEY", "ULTRON_D1_KEY", "NEO4J_PASSWORD"]
    );
}

#[test]
fn config_a_stored_service_key_goes_only_to_the_server_it_was_saved_for() {
    let dir = tempfile::tempdir().unwrap();
    let auth = dir.path().join("auth.json");
    let none = |_: &str| None;
    let server = "https://mini.tail.example:18091";
    assert_eq!(service_key(&D1_KEY, &none, &auth, server), Ok(None));
    assert_eq!(key_origin(&D1_KEY, &none, &auth), KeyOrigin::Unset);
    save_service_key(&auth, &D1_KEY, &format!(" {} ", fake("d1")), &format!("{server}/")).unwrap();
    let stored: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&auth).unwrap()).unwrap();
    assert_eq!(
        stored,
        json!({"judge:d1": {"type": "api_key", "key": fake("d1"), "origin": server}})
    );
    assert_eq!(service_key(&D1_KEY, &none, &auth, server), Ok(Some(fake("d1"))));
    assert_eq!(key_origin(&D1_KEY, &none, &auth), KeyOrigin::Stored(server.to_string()));
    // Pointed at another server (a cell that rewrote judge.d1.url), the stored key is not sent.
    assert_eq!(
        service_key(&D1_KEY, &none, &auth, "https://elsewhere.example"),
        Err(format!(
            "the judge:d1 credential in auth.json was saved for {server}, not https://elsewhere.example (save it again \
             for this server with `ultron setup`, or set ULTRON_D1_KEY)"
        ))
    );
    // Nor over plain http to another machine, even the environment's.
    let env_key = fake("env");
    let env = |name: &str| (name == "ULTRON_D1_KEY").then(|| env_key.clone());
    assert_eq!(
        service_key(&D1_KEY, &env, &auth, "https://any.example"),
        Ok(Some(fake("env")))
    );
    assert_eq!(key_origin(&D1_KEY, &env, &auth), KeyOrigin::Env);
    assert_eq!(
        service_key(&D1_KEY, &env, &auth, "http://any.example"),
        Err("http://any.example is not https: a credential goes over plain http only to this machine".into())
    );
    // An entry without an origin, or not an api key, is no usable key; another service's entry is not this one's.
    std::fs::write(
        &auth,
        json!({"judge:d1": {"type": "api_key", "key": fake("old")}, "judge:typesafe": {"type": "oauth"}}).to_string(),
    )
    .unwrap();
    assert!(service_key(&D1_KEY, &none, &auth, server).is_err());
    assert_eq!(service_key(&TYPESAFE_KEY, &none, &auth, server), Ok(None));
    assert_eq!(service_key(&NEO4J_PASSWORD, &none, &auth, server), Ok(None));
}

#[test]
fn config_service_urls_are_https_unless_local_and_shown_without_userinfo() {
    for local in [
        "http://127.0.0.1:7474",
        "http://localhost:18091/x",
        "http://[::1]:3789",
        "https://neo4j.example",
    ] {
        assert_eq!(secure_url(local), Ok(()), "{local}");
    }
    for remote in [
        "http://neo4j.example",
        "http://10.0.0.2:7474",
        "ftp://h",
        "neo4j.example",
    ] {
        assert!(secure_url(remote).is_err(), "{remote}");
    }
    assert_eq!(
        display_url("https://neo4j:hunter2@Neo4j.example:7473/db?x=1"),
        "https://Neo4j.example:7473/db?x=1"
    );
    assert_eq!(display_url("https://neo4j.example"), "https://neo4j.example");
    assert_eq!(
        url_origin("HTTPS://user:pw@Mini.Tail.Example:18091/decide"),
        Some("https://mini.tail.example:18091".into())
    );
    assert_eq!(url_origin("mini"), None);
}

#[test]
fn config_service_url_and_memory_backend_names_are_resolved_once() {
    use config::settings::service_url;
    assert_eq!(service_url(None, None, "http://d"), Some("http://d".into()));
    assert_eq!(
        service_url(Some(" "), Some(" http://s "), "http://d"),
        Some("http://s".into())
    );
    assert_eq!(
        service_url(Some("http://e"), Some("http://s"), "http://d"),
        Some("http://e".into())
    );
    for off in ["off", "NONE", "0", "False"] {
        assert_eq!(service_url(Some(off), Some("http://s"), "http://d"), None, "{off}");
    }
    assert_eq!(config::keys::memory_backend_name(" Memorantado "), Some("memorantado"));
    assert_eq!(config::keys::memory_backend_name("detect"), None);
    let global = json!({"memory": {"backend": "HINDSIGHT"}});
    assert_eq!(
        config::keys::memory_backend(global.as_object().unwrap()),
        Some("hindsight")
    );
}
