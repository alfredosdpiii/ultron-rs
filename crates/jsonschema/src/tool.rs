//! `validateToolArguments` (packages/ai/src/utils/validation.ts) and `prepareToolArguments` (ultron/rlm/extension-tools.ts
//! over it): a tool call's arguments are validated and coerced against the tool's JSON schema. Optional properties
//! that are `null` are dropped, primitives are coerced to the schema's type (`"5"` to 5 for a number, 5 to `"5"` for
//! a string, a union takes the first member a coerced value satisfies), and what still does not match fails with
//! typebox's errors ([`crate::errors`]). Coercion follows `type`, `properties`, `additionalProperties`, `items` and
//! `anyOf`/`oneOf`/`allOf`.

use crate::check::{SchemaError, check, errors};
use config::json::stringify_pretty;
use serde_json::{Map, Number, Value};
use session_file::js::{format_f64, trim};

/// A JavaScript number as JSON: integral values in the safe range as integers (so they serialize without a fraction).
pub(crate) fn js_number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        Value::Number(Number::from(value as i64))
    } else {
        Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

/// Validate and coerce `params` for tool `name` with parameter schema `schema`.
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
    // TypeScript discards `Convert`'s return value: only the in-place conversion of an object schema's object counts.
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
        stringify_pretty(received)
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

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    if let Value::Array(items) = value {
        match &schema["items"] {
            Value::Array(schemas) => {
                for (item, item_schema) in items.iter_mut().zip(schemas) {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            item_schema @ Value::Object(_) => {
                for item in items {
                    normalize_optional_nulls(item, item_schema);
                }
            }
            _ => {}
        }
        return;
    }
    let (Value::Object(object), Some(properties)) = (value, schema["properties"].as_object()) else {
        return;
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for (key, property) in properties {
        let Some(item) = object.get_mut(key) else { continue };
        if item.is_null()
            && !required.contains(&key.as_str())
            && !property["$ref"].is_string()
            && !valid(&Value::Null, property)
        {
            object.shift_remove(key);
        } else {
            normalize_optional_nulls(item, property);
        }
    }
}

/// `Number(text)` for a string that is not blank; `None` when it is not a finite number.
fn finite_number(text: &str) -> Option<f64> {
    if trim(text).is_empty() {
        return None;
    }
    let n = session_file::js::to_number(&Value::String(text.into()));
    n.is_finite().then_some(n)
}

fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match (kind, value) {
        ("number" | "integer", Value::Null) => js_number(0.0),
        ("number", Value::String(s)) => finite_number(s).map_or_else(|| value.clone(), js_number),
        ("integer", Value::String(s)) => finite_number(s)
            .filter(|n| n.fract() == 0.0)
            .map_or_else(|| value.clone(), js_number),
        ("number" | "integer", Value::Bool(b)) => js_number(f64::from(u8::from(*b))),
        ("boolean", Value::Null) => Value::Bool(false),
        ("boolean", Value::String(s)) if s == "true" => Value::Bool(true),
        ("boolean", Value::String(s)) if s == "false" => Value::Bool(false),
        ("boolean", Value::Number(n)) if n.as_f64() == Some(1.0) => Value::Bool(true),
        ("boolean", Value::Number(n)) if n.as_f64() == Some(0.0) => Value::Bool(false),
        ("string", Value::Null) => Value::String(String::new()),
        ("string", Value::Number(n)) => Value::String(format_f64(n.as_f64().unwrap_or(0.0))),
        ("string", Value::Bool(b)) => Value::String(b.to_string()),
        ("null", Value::String(s)) if s.is_empty() => Value::Null,
        ("null", Value::Number(n)) if n.as_f64() == Some(0.0) => Value::Null,
        ("null", Value::Bool(false)) => Value::Null,
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

/// `coerceWithJsonSchema`.
fn coerce(mut value: Value, schema: &Value) -> Value {
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
    let kinds = types(schema);
    let union_member = kinds.len() > 1 && kinds.iter().any(|k| matches_type(&value, k));
    if !kinds.is_empty() && !union_member {
        for kind in &kinds {
            let candidate = coerce_primitive(&value, kind);
            if candidate != value {
                value = candidate;
                break;
            }
        }
    }
    if kinds.contains(&"object")
        && let Value::Object(object) = &mut value
    {
        let properties = schema["properties"].as_object();
        if let Some(properties) = properties {
            for (key, property) in properties {
                if let Some(item) = object.get_mut(key) {
                    *item = coerce(item.take(), property);
                }
            }
        }
        if let Some(additional) = schema["additionalProperties"].as_object() {
            let additional = Value::Object(additional.clone());
            for (key, item) in object.iter_mut() {
                if properties.is_some_and(|p| p.contains_key(key)) {
                    continue;
                }
                *item = coerce(item.take(), &additional);
            }
        }
    }
    if kinds.contains(&"array")
        && let Value::Array(items) = &mut value
    {
        match &schema["items"] {
            Value::Array(schemas) => {
                for (item, item_schema) in items.iter_mut().zip(schemas) {
                    *item = coerce(item.take(), item_schema);
                }
            }
            item_schema @ Value::Object(_) => {
                for item in items.iter_mut() {
                    *item = coerce(item.take(), item_schema);
                }
            }
            _ => {}
        }
    }
    value
}
