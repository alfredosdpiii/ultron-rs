//! The meta-check of `validateJsonSchema` (definition-registry.ts): the keywords a runtime schema (a typed agent's
//! input and output, a spawn or `rlm.infer` contract) may use, and the shape of each. JSON cannot be cyclic, so
//! the TypeScript cycle check has nothing to find.

use fancy_regex::Regex;
use serde_json::Value;

const TYPES: [&str; 7] = ["null", "boolean", "object", "array", "number", "integer", "string"];

fn failure(path: &str, detail: &str) -> String {
    format!("Invalid JSON schema at {path}: {detail}")
}

fn non_negative_integer(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|n| n >= 0.0 && n.fract() == 0.0 && n <= 9_007_199_254_740_991.0)
}

fn string_array(value: &Value, path: &str) -> Result<(), String> {
    let Some(items) = value
        .as_array()
        .filter(|items| items.iter().all(|i| i.as_str().is_some_and(|s| !s.is_empty())))
    else {
        return Err(failure(path, "must be an array of nonempty strings"));
    };
    if items.iter().enumerate().any(|(i, item)| items[..i].contains(item)) {
        return Err(failure(path, "must not contain duplicates"));
    }
    Ok(())
}

fn schema_map(value: &Value, path: &str) -> Result<(), String> {
    let map = value.as_object().ok_or_else(|| failure(path, "must be an object"))?;
    map.iter()
        .try_for_each(|(key, nested)| validate_json_schema(nested, &format!("{path}.{key}")))
}

fn schema_list(value: &Value, path: &str) -> Result<(), String> {
    let items = value
        .as_array()
        .filter(|items| !items.is_empty())
        .ok_or_else(|| failure(path, "must be a nonempty schema array"))?;
    items
        .iter()
        .enumerate()
        .try_for_each(|(index, nested)| validate_json_schema(nested, &format!("{path}[{index}]")))
}

/// `validateJsonSchema(value, path)` of definition-registry.ts. JSON cannot be cyclic.
pub fn validate_json_schema(value: &Value, path: &str) -> Result<(), String> {
    // typebox's IsSchema takes an array too; its entries are then read as keywords named by index.
    if let Some(items) = value.as_array() {
        return match items.is_empty() {
            true => Ok(()),
            false => Err(failure(&format!("{path}.0"), "is not a supported JSON Schema keyword")),
        };
    }
    let schema = value
        .as_object()
        .ok_or_else(|| failure(path, "must be a JSON schema object"))?;
    for (keyword, item) in schema {
        let at = format!("{path}.{keyword}");
        match keyword.as_str() {
            "$schema" | "$id" | "$anchor" | "$dynamicAnchor" | "$comment" | "title" | "description" | "format"
            | "pattern" | "contentEncoding" | "contentMediaType" => {
                let Some(text) = item.as_str() else {
                    return Err(failure(&at, "must be a string"));
                };
                if keyword == "pattern" && Regex::new(text).is_err() {
                    return Err(failure(&at, "must be a valid regular expression"));
                }
            }
            "$ref" | "$dynamicRef" => return Err(failure(&at, "references are not supported for runtime definitions")),
            "$defs" | "definitions" | "properties" | "patternProperties" | "dependentSchemas" => schema_map(item, &at)?,
            "type" => match item {
                Value::String(name) if TYPES.contains(&name.as_str()) => {}
                Value::String(_) => return Err(failure(&at, "contains an unknown type")),
                Value::Array(names) => {
                    if names.is_empty() || names.iter().any(|n| !n.as_str().is_some_and(|n| TYPES.contains(&n))) {
                        return Err(failure(&at, "must contain only JSON Schema types"));
                    }
                    if names.iter().enumerate().any(|(i, n)| names[..i].contains(n)) {
                        return Err(failure(&at, "must not contain duplicate types"));
                    }
                }
                _ => return Err(failure(&at, "must be a JSON Schema type or array of types")),
            },
            "required" => string_array(item, &at)?,
            "enum" => {
                if !item.as_array().is_some_and(|items| !items.is_empty()) {
                    return Err(failure(&at, "must be a nonempty JSON array"));
                }
            }
            "const" => {}
            "items" | "additionalItems" => {
                if !item.is_boolean() {
                    validate_json_schema(item, &at)?;
                }
            }
            "prefixItems" => {
                let items = item.as_array().ok_or_else(|| failure(&at, "must be an array"))?;
                for (index, nested) in items.iter().enumerate() {
                    validate_json_schema(nested, &format!("{path}.prefixItems[{index}]"))?;
                }
            }
            "additionalProperties"
            | "unevaluatedProperties"
            | "propertyNames"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else"
            | "unevaluatedItems" => {
                if !(item.is_boolean() && (keyword == "additionalProperties" || keyword == "unevaluatedProperties")) {
                    validate_json_schema(item, &at)?;
                }
            }
            "allOf" | "anyOf" | "oneOf" => schema_list(item, &at)?,
            "dependentRequired" => {
                let map = item.as_object().ok_or_else(|| failure(&at, "must be an object"))?;
                map.iter()
                    .try_for_each(|(key, nested)| string_array(nested, &format!("{at}.{key}")))?;
            }
            "dependencies" => {
                let map = item.as_object().ok_or_else(|| failure(&at, "must be an object"))?;
                for (key, nested) in map {
                    let at = format!("{at}.{key}");
                    if nested.is_array() {
                        string_array(nested, &at)?;
                    } else {
                        validate_json_schema(nested, &at)?;
                    }
                }
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minContains" | "maxContains" | "minProperties"
            | "maxProperties" => {
                if !non_negative_integer(item) {
                    return Err(failure(&at, "must be a nonnegative integer"));
                }
            }
            "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                if !item.is_number() {
                    return Err(failure(&at, "must be a finite number"));
                }
            }
            "multipleOf" => {
                if !item.as_f64().is_some_and(|n| n > 0.0) {
                    return Err(failure(&at, "must be a positive finite number"));
                }
            }
            "uniqueItems" | "readOnly" | "writeOnly" | "deprecated" => {
                if !item.is_boolean() {
                    return Err(failure(&at, "must be a boolean"));
                }
            }
            "examples" => {
                if !item.is_array() {
                    return Err(failure(&at, "must be a JSON array"));
                }
            }
            _ if keyword.starts_with("x-") => {}
            _ => return Err(failure(&at, "is not a supported JSON Schema keyword")),
        }
    }
    Ok(())
}

/// The first keyword (as `<path>.<keyword>`) this checker would evaluate differently from typebox, if any:
/// `unevaluatedProperties` and `unevaluatedItems`, which need typebox's evaluated-key tracking.
pub fn unchecked_keyword(schema: &Value, path: &str) -> Option<String> {
    let map = schema.as_object()?;
    map.iter().find_map(|(keyword, item)| {
        let at = format!("{path}.{keyword}");
        match keyword.as_str() {
            "unevaluatedProperties" | "unevaluatedItems" => Some(at),
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" | "dependencies" => item
                .as_object()?
                .iter()
                .find_map(|(key, nested)| unchecked_keyword(nested, &format!("{at}.{key}"))),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => item
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(index, nested)| unchecked_keyword(nested, &format!("{at}[{index}]"))),
            "items" if item.is_array() => item
                .as_array()?
                .iter()
                .enumerate()
                .find_map(|(index, nested)| unchecked_keyword(nested, &format!("{at}[{index}]"))),
            "items"
            | "additionalItems"
            | "additionalProperties"
            | "propertyNames"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else" => unchecked_keyword(item, &at),
            _ => None,
        }
    })
}
