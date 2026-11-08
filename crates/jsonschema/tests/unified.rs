//! Where the three former ports (delegation, inference, mcp) disagreed, the merged checker keeps typebox's behavior.
//! The TypeScript-recorded fixtures stay with their crates (agent, delegation, inference, mcp).

use serde_json::json;

#[test]
fn jsonschema_tool_errors_stop_at_typebox_max_errors() {
    let properties: serde_json::Map<String, serde_json::Value> = (0..10)
        .map(|i| (format!("p{i}"), json!({ "type": "string", "minLength": 3 })))
        .collect();
    let schema = json!({ "type": "object", "properties": properties });
    let args: serde_json::Map<String, serde_json::Value> = (0..10).map(|i| (format!("p{i}"), json!("x"))).collect();
    let error = jsonschema::prepare_tool_arguments("t", &schema, &args).unwrap_err();
    let lines = error.lines().filter(|l| l.starts_with("  - ")).count();
    assert_eq!(lines, 8, "{error}");
}

#[test]
fn jsonschema_tool_arguments_check_one_of_like_typebox() {
    let schema = json!({
        "type": "object",
        "properties": { "v": { "oneOf": [{ "type": "string" }, { "type": "string", "minLength": 1 }] } }
    });
    let args = json!({ "v": "ab" });
    let error = jsonschema::prepare_tool_arguments("t", &schema, args.as_object().unwrap()).unwrap_err();
    assert!(
        error.contains("  - v: must match exactly one schema in oneOf"),
        "{error}"
    );
}

#[test]
fn jsonschema_required_error_names_the_first_missing_property() {
    let schema = json!({
        "type": "object",
        "properties": { "o": { "type": "object", "required": ["a", "b"] } },
        "required": ["o"]
    });
    let args = json!({ "o": {} });
    let error = jsonschema::prepare_tool_arguments("t", &schema, args.as_object().unwrap()).unwrap_err();
    assert!(error.contains("  - o.a: must have required properties a, b"), "{error}");
}

#[test]
fn jsonschema_time_format_rejects_a_multibyte_seconds_field() {
    assert!(!jsonschema::formats::matches("time", "00:00:0\u{e9}Z"));
    assert!(jsonschema::formats::matches("time", "00:00:00Z"));
    let schema = json!({ "type": "string", "format": "date-time" });
    assert!(!jsonschema::check(&schema, &json!("2024-01-01T00:00:0\u{e9}")));
}

#[test]
fn jsonschema_meta_check_refuses_default_like_definition_registry() {
    assert_eq!(
        jsonschema::validate_json_schema(&json!({ "default": 1 }), "$.contract"),
        Err("Invalid JSON schema at $.contract.default: is not a supported JSON Schema keyword".into())
    );
}

#[test]
fn jsonschema_formats_and_patterns_are_checked_not_refused() {
    let schema =
        json!({ "type": "object", "properties": { "h": { "type": "string", "format": "hostname", "pattern": "^a" } } });
    assert_eq!(jsonschema::unchecked_keyword(&schema, "$"), None);
    assert!(jsonschema::check(&schema, &json!({ "h": "a.example" })));
    let errors = jsonschema::errors(&schema, &json!({ "h": "b_" }));
    let messages: Vec<&str> = errors.iter().map(|e| e.message.as_str()).collect();
    assert_eq!(
        messages,
        ["must match format \"hostname\"", "must match pattern \"^a\""]
    );
    let unevaluated = json!({ "properties": { "a": { "unevaluatedProperties": false } } });
    assert_eq!(
        jsonschema::unchecked_keyword(&unevaluated, "$").as_deref(),
        Some("$.properties.a.unevaluatedProperties")
    );
}

#[test]
fn jsonschema_string_lengths_count_graphemes() {
    let schema = json!({ "type": "string", "maxLength": 1 });
    assert!(jsonschema::check(&schema, &json!("e\u{301}")));
}

#[test]
fn jsonschema_limits_print_as_javascript_numbers() {
    let errors = jsonschema::errors(&json!({ "minimum": 1e-7 }), &json!(0));
    assert_eq!(errors[0].message, "must be >= 1e-7");
}
