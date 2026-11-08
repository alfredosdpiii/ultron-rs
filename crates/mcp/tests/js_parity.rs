//! Outputs that follow JavaScript once the helpers moved to `jsrt` (expected values printed by node).

use mcp::results::{format_size, to_model_content};
use serde_json::{Value, json};

#[test]
fn mcp_format_size_rounds_ties_like_to_fixed() {
    // `formatSize` of truncate.ts: `(1280 / 1024).toFixed(1)` is "1.3" (`{:.1}` rounded the exact tie 1.25 to even).
    assert_eq!(format_size(1280), "1.3KB");
    assert_eq!(format_size(1_310_720), "1.3MB");
    assert_eq!(format_size(1126), "1.1KB");
    assert_eq!(format_size(1023), "1023B");
}

#[test]
fn mcp_binary_resource_decodes_like_buffer_from() {
    // `Buffer.from("aGVs*bG8=", "base64").toString()` is "hello": Node skips the stray `*`; it used to decode to "".
    let block = json!({
        "type": "resource",
        "resource": { "uri": "file:///x.txt", "mimeType": "text/plain", "blob": "aGVs*bG8=" }
    });
    let save = |_: &[u8], _: &str| -> Result<String, String> { Err("unused".into()) };
    let content = to_model_content("s", &[block], false, &save);
    assert_eq!(content, [json!({ "type": "text", "text": "hello" })]);
}

#[test]
fn mcp_structured_content_prints_like_json_stringify() {
    // `JSON.stringify({ b: 1.0, 10: 2 }, null, 2)`: index keys first, `1.0` as `1`.
    let content = mcp::content::to_llm_content(&json!({ "structuredContent": { "b": 1.0, "10": 2 } }));
    assert_eq!(content[0]["text"], "{\n  \"10\": 2,\n  \"b\": 1\n}");
}

#[test]
fn mcp_config_edit_rounds_large_integers_like_json_stringify() {
    // `JSON.stringify(JSON.parse("12345678901234567890"))` is "12345678901234567000".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, "{\n    \"big\": 12345678901234567890\n}\n").unwrap();
    let Value::Object(config) = json!({ "command": "x" }) else {
        unreachable!()
    };
    mcp::edit::add_server_config(&path, "a", config).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\n    \"big\": 12345678901234567000,\n"), "{text}");
}

#[test]
fn mcp_log_line_prints_iso_years_like_to_iso_string() {
    // `new Date(253402300800000).toISOString()` is "+010000-01-01T00:00:00.000Z".
    let line = mcp::log::format_log_message(
        "s",
        Some(&json!({ "level": "info", "data": "hi" })),
        253_402_300_800_000,
    );
    assert!(line.starts_with("+010000-01-01T00:00:00.000Z [s] info"), "{line}");
}
