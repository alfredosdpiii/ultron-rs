//! TypeBox 1.x `Value.Convert` (typebox/build/value/convert), the conversion `validateToolArguments` applies to a
//! TypeBox tool schema before its JSON-schema coercion. TypeBox dispatches on a schema's hidden `~kind`; a declared
//! tool schema is its JSON form, so the kind is read from the shape: `anyOf` is a Union, `const` a Literal, and
//! otherwise `type` (`object` with `properties`, `array` with one `items` schema, `string`, `number`, `integer`,
//! `boolean`, `null`). Other kinds (Record, Tuple, Ref, Intersect, Enum, BigInt) convert nothing, as TypeBox does for
//! a value of no kind.

use serde_json::Value;

use crate::tool::valid;

/// `FromType`: the converted value (TypeBox mutates objects in place and returns them; this returns the result).
pub(crate) fn convert(schema: &Value, value: Value) -> Value {
    if let Some(members) = schema["anyOf"].as_array() {
        return from_union(schema, members, value);
    }
    if let Some(constant) = schema.get("const") {
        return from_literal(constant, value);
    }
    match schema["type"].as_str() {
        Some("object") if schema["properties"].is_object() => from_object(schema, value),
        Some("array") if schema["items"].is_object() => from_array(&schema["items"], value),
        Some(kind) => from_primitive(kind, value),
        None => value,
    }
}

fn from_array(item_schema: &Value, value: Value) -> Value {
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    Value::Array(items.into_iter().map(|item| convert(item_schema, item)).collect())
}

fn from_primitive(kind: &str, value: Value) -> Value {
    match kind {
        "string" => try_string(&value).unwrap_or(value),
        "number" => try_number(&value).map_or(value, jsrt::json::number),
        "integer" => try_number(&value).map_or(value, |n| jsrt::json::number(n.trunc())),
        "boolean" => try_boolean(&value).map_or(value, Value::Bool),
        "null" if try_null(&value) => Value::Null,
        _ => value,
    }
}

/// `FromObject`: each present property by its schema; then, when `additionalProperties` is a schema, TypeBox's
/// `FromAdditionalProperties`, which converts every key that differs from a property name once per such property
/// (so a known property is converted again by the additional schema whenever another property exists).
fn from_object(schema: &Value, value: Value) -> Value {
    let Value::Object(mut object) = value else {
        return value;
    };
    let properties = schema["properties"].as_object().cloned().unwrap_or_default();
    let keys: Vec<String> = object.keys().cloned().collect();
    for (name, property) in &properties {
        for key in &keys {
            if key == name {
                let item = object[key].take();
                object[key] = convert(property, item);
            }
        }
    }
    if let Some(additional) = schema.get("additionalProperties").filter(|a| a.is_object()) {
        for name in properties.keys() {
            for key in &keys {
                if key != name {
                    let item = object[key].take();
                    object[key] = convert(additional, item);
                }
            }
        }
    }
    Value::Object(object)
}

/// `FromUnion`: unchanged when a member accepts the value; else the first member's conversion the union accepts.
fn from_union(schema: &Value, members: &[Value], value: Value) -> Value {
    if members.iter().any(|member| valid(&value, member)) {
        return value;
    }
    members
        .iter()
        .map(|member| convert(member, value.clone()))
        .find(|candidate| valid(candidate, schema))
        .unwrap_or(value)
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// `FromLiteral`: the literal when the value converts to exactly it.
fn from_literal(constant: &Value, value: Value) -> Value {
    if same(constant, &value) {
        return value;
    }
    let converted = match constant {
        Value::Bool(_) => try_boolean(&value).map(Value::Bool),
        Value::Number(_) => try_number(&value).map(jsrt::json::number),
        Value::String(_) => try_string(&value),
        _ => None,
    };
    match converted {
        Some(converted) if same(constant, &converted) => converted,
        _ => value,
    }
}

fn try_string(value: &Value) -> Option<Value> {
    match value {
        Value::Bool(b) => Some(Value::String(b.to_string())),
        Value::Number(n) => Some(Value::String(jsrt::number::to_string(n.as_f64().unwrap_or(0.0)))),
        Value::Null => Some(Value::String("null".into())),
        Value::String(_) => Some(value.clone()),
        _ => None,
    }
}

/// `TryNumber`: finite numbers only; a string by unary plus, then `true`/`false`, then a `123n` BigInt literal in the
/// safe range.
fn try_number(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite()),
        Value::Null => Some(0.0),
        Value::String(text) => {
            let coerced = jsrt::value::number(value);
            if coerced.is_finite() {
                return Some(coerced);
            }
            let lower = text.to_lowercase();
            if lower == "false" {
                return Some(0.0);
            }
            if lower == "true" {
                return Some(1.0);
            }
            big_int(text).filter(|n| n.abs() <= jsrt::number::MAX_SAFE_INTEGER as f64)
        }
        _ => None,
    }
}

fn is_decimal_int(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let all_digits = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    all_digits && (digits == "0" || !digits.starts_with('0'))
}

fn big_int_digits(text: &str) -> Option<&str> {
    if let Some(body) = text.strip_suffix('n') {
        return is_decimal_int(body).then_some(body);
    }
    if let Some((whole, fraction)) = text.split_once('.') {
        let valid_fraction = !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit());
        return (is_decimal_int(whole) && valid_fraction).then_some(whole);
    }
    is_decimal_int(text).then_some(text)
}

/// `TryBigInt` of a string that `+value` rejected: `-?(0|[1-9]\d*)n`, a decimal, or an integer.
fn big_int(text: &str) -> Option<f64> {
    if let Some(digits) = big_int_digits(text) {
        return digits.parse::<f64>().ok();
    }
    match text.to_lowercase().as_str() {
        "false" => Some(0.0),
        "true" => Some(1.0),
        _ => None,
    }
}

fn number_to_boolean(number: &serde_json::Number) -> Option<bool> {
    match number.as_f64() {
        Some(0.0) => Some(false),
        Some(1.0) => Some(true),
        _ => None,
    }
}

fn string_to_boolean(text: &str) -> Option<bool> {
    match text.to_lowercase().as_str() {
        "false" | "0" => Some(false),
        "true" | "1" => Some(true),
        _ => None,
    }
}

fn try_boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(number) => number_to_boolean(number),
        Value::Null => Some(false),
        Value::String(text) => string_to_boolean(text),
        _ => None,
    }
}

fn try_null(value: &Value) -> bool {
    match value {
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Null => true,
        Value::String(text) => {
            let lower = text.to_lowercase();
            lower == "undefined" || lower == "null" || text.is_empty() || text == "0"
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_big_int_formats() {
        assert_eq!(big_int("42n"), Some(42.0));
        assert_eq!(big_int("-0n"), Some(-0.0));
        assert_eq!(big_int("01n"), None);
        assert_eq!(big_int("12.34"), Some(12.0));
        assert_eq!(big_int("01.2"), None);
        assert_eq!(big_int("12."), None);
        assert_eq!(big_int(".5"), None);
        assert_eq!(big_int("TRUE"), Some(1.0));
        assert_eq!(big_int("false"), Some(0.0));
    }

    #[test]
    fn test_try_boolean_values() {
        assert_eq!(try_boolean(&json!(0)), Some(false));
        assert_eq!(try_boolean(&json!(1.0)), Some(true));
        assert_eq!(try_boolean(&json!(2)), None);
        assert_eq!(try_boolean(&json!("0")), Some(false));
        assert_eq!(try_boolean(&json!("1")), Some(true));
        assert_eq!(try_boolean(&json!("True")), Some(true));
        assert_eq!(try_boolean(&json!("FALSE")), Some(false));
        assert_eq!(try_boolean(&json!("yes")), None);
        assert_eq!(try_boolean(&Value::Null), Some(false));
    }

    #[test]
    fn test_convert_guards() {
        let obj_no_props = json!({ "type": "object" });
        assert_eq!(convert(&obj_no_props, json!({"a": "1"})), json!({"a": "1"}));

        let arr_no_items = json!({ "type": "array" });
        assert_eq!(convert(&arr_no_items, json!("x")), json!("x"));

        let arr_with_items = json!({ "type": "array", "items": { "type": "number" } });
        assert_eq!(convert(&arr_with_items, json!("42")), json!([42]));
        assert_eq!(convert(&arr_with_items, json!(["1", "2"])), json!([1, 2]));
    }
}
