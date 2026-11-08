//! Settings values printed, trimmed and rewritten as JavaScript does (through `jsrt`); expected values from node.

use config::keys::{http_idle_timeout_ms, parse_timeout_ms, websocket_connect_timeout_ms};
use config::models_store::ModelsStore;
use config::settings::{Settings, model_ref};
use serde_json::{Value, json};

fn settings(key: &str, value: Value) -> Settings {
    std::iter::once((key.to_string(), value)).collect()
}

#[test]
fn config_invalid_setting_errors_print_values_like_js_string() {
    // D-S2: `String(-1)` is "-1" where serde printed the float as "-1.0".
    let value: Value = serde_json::from_str("-1.0").unwrap();
    assert_eq!(
        websocket_connect_timeout_ms(&settings("websocketConnectTimeoutMs", value)),
        Err("Invalid websocketConnectTimeoutMs setting: -1".into())
    );
    // D-S1: `String([1, null, [2, 3]])` is "1,,2,3" where the copy printed "1,null,[2,3]".
    assert_eq!(
        http_idle_timeout_ms(&settings("httpIdleTimeoutMs", json!([1, null, [2, 3]]))),
        Err("Invalid httpIdleTimeoutMs setting: 1,,2,3".into())
    );
    // A present `null` is invalid, as `parseTimeoutSetting` throws for any value but `undefined` (the daemon's
    // `/settings` copy read it as the default).
    assert_eq!(
        http_idle_timeout_ms(&settings("httpIdleTimeoutMs", Value::Null)),
        Err("Invalid httpIdleTimeoutMs setting: null".into())
    );
}

/// `parseHttpIdleTimeoutMs` (every timeout setting): literal cases; a string is read as `Number(text)`.
#[test]
fn config_timeout_settings_parse_like_typescript() {
    let cases = [
        (json!(30_000), Some(30_000.0)),
        (json!(1.9), Some(1.0)),
        (json!(0), Some(0.0)),
        (json!(-1), None),
        (json!(" 1500 "), Some(1500.0)),
        (json!("0x10"), Some(16.0)),
        (json!("1e3"), Some(1000.0)),
        (json!("Disabled"), Some(0.0)),
        (json!(""), None),
        (json!("soon"), None),
        (json!("Infinity"), None),
        (json!(true), None),
        (json!(null), None),
    ];
    for (value, expected) in cases {
        assert_eq!(parse_timeout_ms(&value), expected, "{value}");
    }
}

#[test]
fn config_model_ref_keeps_nel_as_javascript_trim_does() {
    // D-W1: `"p/m\u0085".trim()` keeps U+0085 and `/\s/` does not match it, so the reference keeps it; the old trim
    // dropped it.
    assert_eq!(model_ref(Some(&json!(" p/m\u{85} "))), Some("p/m\u{85}".to_string()));
    assert_eq!(model_ref(Some(&json!("\u{feff}p/m\u{a0}"))), Some("p/m".to_string()));
}

#[test]
fn config_store_rewrites_round_large_integers_like_json_stringify() {
    // D-J2: `JSON.stringify(JSON.parse("12345678901234567890"))` is "12345678901234567000".
    let dir = tempfile::tempdir().unwrap();
    let store = ModelsStore::new(dir.path().join("models-store.json"));
    store
        .write("p", serde_json::from_str(r#"{"limit":12345678901234567890}"#).unwrap())
        .unwrap();
    let text = std::fs::read_to_string(store.path()).unwrap();
    assert!(text.contains("\"limit\": 12345678901234567000"), "{text}");
}
