//! `validateToolArguments` (packages/ai/src/utils/validation.ts) and `prepareToolArguments` (ultron/rlm/extension-tools.ts
//! over it): a tool call's arguments are validated and coerced against the tool's JSON schema. Optional properties
//! that are `null` are dropped, primitives are coerced to the schema's type (`"5"` to 5 for a number, 5 to `"5"` for
//! a string, a union takes the first member a coerced value satisfies), and what still does not match fails with
//! typebox's errors ([`crate::errors`]). Coercion follows `type`, `properties`, `additionalProperties`, `items` and
//! `anyOf`/`oneOf`/`allOf`.

use crate::check::{SchemaError, check, errors};
use serde_json::{Map, Value};

pub fn prepare_tool_arguments(
    name: &str,
    schema: &Value,
    params: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let mut args = Value::Object(params.clone());
    normalize_optional_nulls(&mut args, schema);
    let args = coerce(args, schema);
    if check(schema, &args) {
        return Ok(match args {
            Value::Object(map) => map,
            _ => Map::new(),
        });
    }
    Err(failure(name, schema, &args, &Value::Object(params.clone())))
}

/// `validateToolArguments` for a tool whose schema is a TypeBox schema in TypeScript (the agent loop's tools): as
/// [`prepare_tool_arguments`], with TypeBox's `Value.Convert` (`crate::convert`) applied to an object
/// before the JSON-schema coercion. `args` may be any JSON value; the error shows it as received.
pub fn validate_tool_arguments(name: &str, schema: &Value, args: &Value) -> Result<Value, String> {
    let mut value = args.clone();
    normalize_optional_nulls(&mut value, schema);
    if value.is_object()
        && schema["type"] == "object"
        && schema["properties"].is_object()
        && schema.get("anyOf").is_none()
    {
        value = crate::convert::convert(schema, value);
    }
    let value = coerce(value, schema);
    if check(schema, &value) {
        return Ok(value);
    }
    Err(failure(name, schema, &value, args))
}

/// `formatValidationPath`: the instance path with dots; a `required` error names its first missing property.
fn error_path(error: &SchemaError) -> String {
    let path = error
        .instance_path
        .strip_prefix('/')
        .unwrap_or(&error.instance_path)
        .replace('/', ".");
    if error.keyword == "required"
        && let Some(first) = error.params["requiredProperties"][0].as_str().filter(|p| !p.is_empty())
    {
        return if path.is_empty() {
            first.into()
        } else {
            format!("{path}.{first}")
        };
    }
    if path.is_empty() { "root".into() } else { path }
}

/// The thrown message: typebox's errors for the prepared `value`, then the arguments as received.
fn failure(name: &str, schema: &Value, value: &Value, received: &Value) -> String {
    let lines: Vec<String> = errors(schema, value)
        .iter()
        .map(|error| format!("  - {}: {}", error_path(error), error.message))
        .collect();
    let lines = if lines.is_empty() {
        "Unknown validation error".to_string()
    } else {
        lines.join("\n")
    };
    format!(
        "Validation failed for tool \"{name}\":\n{lines}\n\nReceived arguments:\n{}",
        jsrt::json::stringify_pretty(received)
    )
}

fn types(schema: &Value) -> Vec<&str> {
    match &schema["type"] {
        Value::String(t) => vec![t.as_str()],
        Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

pub(crate) fn valid(value: &Value, schema: &Value) -> bool {
    check(schema, value)
}

fn normalize_array_nulls(items: &mut [Value], items_schema: &Value) {
    match items_schema {
        Value::Array(schemas) => {
            for (item, item_schema) in items.iter_mut().zip(schemas) {
                normalize_optional_nulls(item, item_schema);
            }
        }
        Value::Object(_) => {
            for item in items {
                normalize_optional_nulls(item, items_schema);
            }
        }
        _ => {}
    }
}

fn should_drop_optional_null(key: &str, item: &Value, property: &Value, required: &[&str]) -> bool {
    let optional = !required.contains(&key) && !property["$ref"].is_string();
    item.is_null() && optional && !valid(&Value::Null, property)
}

fn normalize_object_nulls(object: &mut Map<String, Value>, schema: &Value) {
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map_or_default(|r| r.iter().filter_map(Value::as_str).collect());
    for (key, property) in properties {
        let Some(item) = object.get_mut(key) else {
            continue;
        };
        if should_drop_optional_null(key, item, property, &required) {
            object.shift_remove(key);
        } else {
            normalize_optional_nulls(item, property);
        }
    }
}

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    match value {
        Value::Array(items) => normalize_array_nulls(items, &schema["items"]),
        Value::Object(object) => normalize_object_nulls(object, schema),
        _ => {}
    }
}

/// `Number(text)` for a string that is not blank; `None` when it is not a finite number.
fn finite_number(text: &str) -> Option<f64> {
    if jsrt::string::trim(text).is_empty() {
        return None;
    }
    let n = jsrt::number::parse(text);
    n.is_finite().then_some(n)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NumericTarget {
    Number,
    Integer,
}

fn coerce_to_number(value: &Value, target: NumericTarget) -> Value {
    match value {
        Value::Null => jsrt::json::number(0.0),
        Value::Bool(b) => jsrt::json::number(f64::from(u8::from(*b))),
        Value::String(s) => {
            let parsed = finite_number(s);
            let filtered = if target == NumericTarget::Integer {
                parsed.filter(|n| n.fract() == 0.0)
            } else {
                parsed
            };
            filtered.map_or_else(|| value.clone(), jsrt::json::number)
        }
        _ => value.clone(),
    }
}

fn parse_bool_str(text: &str) -> Option<bool> {
    match text {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_bool_num(num: Option<f64>) -> Option<bool> {
    match num {
        Some(1.0) => Some(true),
        Some(0.0) => Some(false),
        _ => None,
    }
}

fn coerce_to_boolean(value: &Value) -> Value {
    match value {
        Value::Null => Value::Bool(false),
        Value::String(s) => parse_bool_str(s).map_or_else(|| value.clone(), Value::Bool),
        Value::Number(n) => parse_bool_num(n.as_f64()).map_or_else(|| value.clone(), Value::Bool),
        _ => value.clone(),
    }
}

fn coerce_to_string(value: &Value) -> Value {
    match value {
        Value::Null => Value::String(String::new()),
        Value::Number(n) => Value::String(jsrt::number::to_string(n.as_f64().unwrap_or(0.0))),
        Value::Bool(b) => Value::String(b.to_string()),
        _ => value.clone(),
    }
}

fn coerce_to_null(value: &Value) -> Value {
    let is_falsy =
        value.as_str().is_some_and(str::is_empty) || value.as_f64() == Some(0.0) || *value == Value::Bool(false);
    if is_falsy { Value::Null } else { value.clone() }
}

fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match kind {
        "number" => coerce_to_number(value, NumericTarget::Number),
        "integer" => coerce_to_number(value, NumericTarget::Integer),
        "boolean" => coerce_to_boolean(value),
        "string" => coerce_to_string(value),
        "null" => coerce_to_null(value),
        _ => value.clone(),
    }
}

fn coerce_union(value: Value, schemas: &[Value]) -> Value {
    if schemas.iter().any(|schema| valid(&value, schema)) {
        return value;
    }
    for schema in schemas {
        let candidate = coerce(value.clone(), schema);
        if valid(&candidate, schema) {
            return candidate;
        }
    }
    value
}

fn coerce_combinators(mut value: Value, schema: &Value) -> Value {
    if let Some(all) = schema["allOf"].as_array() {
        for nested in all {
            value = coerce(value, nested);
        }
    }
    if let Some(any) = schema["anyOf"].as_array() {
        value = coerce_union(value, any);
    }
    if let Some(one) = schema["oneOf"].as_array() {
        value = coerce_union(value, one);
    }
    value
}

fn coerce_schema_types(value: Value, kinds: &[&str]) -> Value {
    let union_member = kinds.len() > 1 && kinds.iter().any(|k| matches_type(&value, k));
    if kinds.is_empty() || union_member {
        return value;
    }
    for kind in kinds {
        let candidate = coerce_primitive(&value, kind);
        if candidate != value {
            return candidate;
        }
    }
    value
}

fn coerce_object(object: &mut Map<String, Value>, schema: &Value) {
    let properties = schema["properties"].as_object();
    if let Some(properties) = properties {
        for (key, property) in properties {
            if let Some(item) = object.get_mut(key) {
                *item = coerce(item.take(), property);
            }
        }
    }
    let Some(additional) = schema["additionalProperties"].as_object() else {
        return;
    };
    let additional = Value::Object(additional.clone());
    for (key, item) in object.iter_mut() {
        if properties.is_some_and(|p| p.contains_key(key)) {
            continue;
        }
        *item = coerce(item.take(), &additional);
    }
}

fn coerce_array(items: &mut [Value], items_schema: &Value) {
    match items_schema {
        Value::Array(schemas) => {
            for (item, item_schema) in items.iter_mut().zip(schemas) {
                *item = coerce(item.take(), item_schema);
            }
        }
        Value::Object(_) => {
            for item in items.iter_mut() {
                *item = coerce(item.take(), items_schema);
            }
        }
        _ => {}
    }
}

/// `coerceWithJsonSchema`.
fn coerce(value: Value, schema: &Value) -> Value {
    let mut value = coerce_combinators(value, schema);
    let kinds = types(schema);
    value = coerce_schema_types(value, &kinds);
    match &mut value {
        Value::Object(object) if kinds.contains(&"object") => coerce_object(object, schema),
        Value::Array(items) if kinds.contains(&"array") => coerce_array(items, &schema["items"]),
        _ => {}
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_normalize_nulls_array_order() {
        let schema = json!({
            "type": "array",
            "items": {
                "type": "object",
                "required": ["req"],
                "properties": {
                    "a": { "type": "string" },
                    "opt": { "type": "string" },
                    "req": { "type": "string" },
                    "z": { "type": "number" }
                }
            }
        });
        let mut val = json!([{ "a": "x", "opt": null, "req": null, "z": 1 }]);
        normalize_optional_nulls(&mut val, &schema);
        assert_eq!(val, json!([{ "a": "x", "req": null, "z": 1 }]));
        let Some(obj) = val[0].as_object() else {
            panic!("expected object");
        };
        let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["a", "req", "z"]);
    }

    #[test]
    fn test_normalize_nulls_tuples() {
        let schema = json!({
            "type": "array",
            "items": [
                {
                    "type": "object",
                    "properties": { "drop": { "type": "string" }, "keep": { "$ref": "#/$defs/x" } }
                },
                { "type": "object", "properties": { "nullable": { "type": ["string", "null"] } } }
            ]
        });
        let mut val = json!([{ "drop": null, "keep": null }, { "nullable": null }]);
        normalize_optional_nulls(&mut val, &schema);
        assert_eq!(val, json!([{ "keep": null }, { "nullable": null }]));
    }

    #[test]
    fn test_coerce_combinators() {
        let all_of_schema = json!({
            "allOf": [
                { "type": "object", "properties": { "a": { "type": "integer" } } },
                { "type": "object", "properties": { "b": { "type": "boolean" } } }
            ]
        });
        assert_eq!(
            coerce(json!({"a": "7", "b": "true"}), &all_of_schema),
            json!({"a": 7, "b": true})
        );

        let one_of_schema = json!({ "oneOf": [{ "type": "integer" }, { "type": "boolean" }] });
        assert_eq!(coerce(json!("42"), &one_of_schema), json!(42));
        assert_eq!(coerce(json!("false"), &one_of_schema), json!(false));

        let null_schema = json!({ "type": "null" });
        assert_eq!(coerce(json!(""), &null_schema), Value::Null);
        assert_eq!(coerce(json!(0.0), &null_schema), Value::Null);
        assert_eq!(coerce(json!(false), &null_schema), Value::Null);
        assert_eq!(coerce(json!("x"), &null_schema), json!("x"));
    }

    #[test]
    fn test_coerce_primitive_edges() {
        assert_eq!(coerce_primitive(&json!("TRUE"), "boolean"), json!("TRUE"));
        assert_eq!(coerce_primitive(&json!(1), "boolean"), json!(true));
        assert_eq!(coerce_primitive(&json!(0), "boolean"), json!(false));
        assert_eq!(coerce_primitive(&json!("3.5"), "integer"), json!("3.5"));
        assert_eq!(coerce_primitive(&json!("3.5"), "number"), json!(3.5));
        assert_eq!(coerce_primitive(&json!("  "), "number"), json!("  "));
    }
}
