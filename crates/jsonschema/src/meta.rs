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

const STRING_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "$anchor",
    "$dynamicAnchor",
    "$comment",
    "title",
    "description",
    "format",
    "contentEncoding",
    "contentMediaType",
];
const NONNEG_INT_KEYWORDS: &[&str] = &[
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minContains",
    "maxContains",
    "minProperties",
    "maxProperties",
];
const NUMBER_KEYWORDS: &[&str] = &["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"];
const BOOL_KEYWORDS: &[&str] = &["uniqueItems", "readOnly", "writeOnly", "deprecated"];
const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "$defs",
    "definitions",
    "properties",
    "patternProperties",
    "dependentSchemas",
];
const SCHEMA_LIST_KEYWORDS: &[&str] = &["allOf", "anyOf", "oneOf"];
const BOOL_OR_SCHEMA_KEYWORDS: &[&str] = &[
    "items",
    "additionalItems",
    "additionalProperties",
    "unevaluatedProperties",
];
const SUBSCHEMA_KEYWORDS: &[&str] = &[
    "propertyNames",
    "contains",
    "not",
    "if",
    "then",
    "else",
    "unevaluatedItems",
];
const UNCHECKED_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
    "dependencies",
];
const UNCHECKED_LIST_KEYWORDS: &[&str] = &["allOf", "anyOf", "oneOf", "prefixItems"];
const UNCHECKED_SUBSCHEMA_KEYWORDS: &[&str] = &[
    "additionalItems",
    "additionalProperties",
    "propertyNames",
    "contains",
    "not",
    "if",
    "then",
    "else",
];

fn require_condition(ok: bool, at: &str, detail: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(failure(at, detail)) }
}

fn validate_pattern(item: &Value, at: &str) -> Result<(), String> {
    let text = item.as_str().ok_or_else(|| failure(at, "must be a string"))?;
    require_condition(Regex::new(text).is_ok(), at, "must be a valid regular expression")
}

fn validate_type_array(names: &[Value], at: &str) -> Result<(), String> {
    if names.is_empty() || names.iter().any(|n| !n.as_str().is_some_and(|n| TYPES.contains(&n))) {
        return Err(failure(at, "must contain only JSON Schema types"));
    }
    if names.iter().enumerate().any(|(i, n)| names[..i].contains(n)) {
        return Err(failure(at, "must not contain duplicate types"));
    }
    Ok(())
}

fn validate_type_keyword(item: &Value, at: &str) -> Result<(), String> {
    match item {
        Value::String(name) if TYPES.contains(&name.as_str()) => Ok(()),
        Value::String(_) => Err(failure(at, "contains an unknown type")),
        Value::Array(names) => validate_type_array(names, at),
        _ => Err(failure(at, "must be a JSON Schema type or array of types")),
    }
}

fn validate_prefix_items(item: &Value, at: &str) -> Result<(), String> {
    let items = item.as_array().ok_or_else(|| failure(at, "must be an array"))?;
    items
        .iter()
        .enumerate()
        .try_for_each(|(index, nested)| validate_json_schema(nested, &format!("{at}[{index}]")))
}

fn validate_dependent_required(item: &Value, at: &str) -> Result<(), String> {
    let map = item.as_object().ok_or_else(|| failure(at, "must be an object"))?;
    map.iter()
        .try_for_each(|(key, nested)| string_array(nested, &format!("{at}.{key}")))
}

fn validate_dependencies(item: &Value, at: &str) -> Result<(), String> {
    let map = item.as_object().ok_or_else(|| failure(at, "must be an object"))?;
    for (key, nested) in map {
        let entry_path = format!("{at}.{key}");
        if nested.is_array() {
            string_array(nested, &entry_path)?;
        } else {
            validate_json_schema(nested, &entry_path)?;
        }
    }
    Ok(())
}

fn validate_meta_keyword(keyword: &str, at: &str) -> Option<Result<(), String>> {
    if matches!(keyword, "$ref" | "$dynamicRef") {
        return Some(Err(failure(at, "references are not supported for runtime definitions")));
    }
    if keyword == "const" || keyword.starts_with("x-") {
        return Some(Ok(()));
    }
    None
}

fn validate_scalar_keyword(keyword: &str, item: &Value, at: &str) -> Option<Result<(), String>> {
    if keyword == "pattern" {
        return Some(validate_pattern(item, at));
    }
    if STRING_KEYWORDS.contains(&keyword) {
        return Some(require_condition(item.is_string(), at, "must be a string"));
    }
    if NONNEG_INT_KEYWORDS.contains(&keyword) {
        return Some(require_condition(
            non_negative_integer(item),
            at,
            "must be a nonnegative integer",
        ));
    }
    if NUMBER_KEYWORDS.contains(&keyword) {
        return Some(require_condition(item.is_number(), at, "must be a finite number"));
    }
    if BOOL_KEYWORDS.contains(&keyword) {
        return Some(require_condition(item.is_boolean(), at, "must be a boolean"));
    }
    if keyword == "multipleOf" {
        return Some(require_condition(
            item.as_f64().is_some_and(|n| n > 0.0),
            at,
            "must be a positive finite number",
        ));
    }
    if keyword == "examples" {
        return Some(require_condition(item.is_array(), at, "must be a JSON array"));
    }
    None
}

fn validate_composite_keyword(keyword: &str, item: &Value, at: &str) -> Option<Result<(), String>> {
    if SCHEMA_MAP_KEYWORDS.contains(&keyword) {
        return Some(schema_map(item, at));
    }
    if SCHEMA_LIST_KEYWORDS.contains(&keyword) {
        return Some(schema_list(item, at));
    }
    if keyword == "prefixItems" {
        return Some(validate_prefix_items(item, at));
    }
    if keyword == "dependentRequired" {
        return Some(validate_dependent_required(item, at));
    }
    if keyword == "dependencies" {
        return Some(validate_dependencies(item, at));
    }
    if keyword == "required" {
        return Some(string_array(item, at));
    }
    None
}

fn validate_subschema_or_value_keyword(keyword: &str, item: &Value, at: &str) -> Option<Result<(), String>> {
    if BOOL_OR_SCHEMA_KEYWORDS.contains(&keyword) {
        return Some(if item.is_boolean() {
            Ok(())
        } else {
            validate_json_schema(item, at)
        });
    }
    if SUBSCHEMA_KEYWORDS.contains(&keyword) {
        return Some(validate_json_schema(item, at));
    }
    if keyword == "type" {
        return Some(validate_type_keyword(item, at));
    }
    if keyword == "enum" {
        return Some(require_condition(
            item.as_array().is_some_and(|items| !items.is_empty()),
            at,
            "must be a nonempty JSON array",
        ));
    }
    None
}

fn validate_keyword(keyword: &str, item: &Value, at: &str) -> Result<(), String> {
    if let Some(res) = validate_meta_keyword(keyword, at) {
        return res;
    }
    if let Some(res) = validate_scalar_keyword(keyword, item, at) {
        return res;
    }
    if let Some(res) = validate_composite_keyword(keyword, item, at) {
        return res;
    }
    if let Some(res) = validate_subschema_or_value_keyword(keyword, item, at) {
        return res;
    }
    Err(failure(at, "is not a supported JSON Schema keyword"))
}

/// `validateJsonSchema(value, path)` of definition-registry.ts.
pub fn validate_json_schema(value: &Value, path: &str) -> Result<(), String> {
    if let Some(items) = value.as_array() {
        return if items.is_empty() {
            Ok(())
        } else {
            Err(failure(&format!("{path}.0"), "is not a supported JSON Schema keyword"))
        };
    }
    let schema = value
        .as_object()
        .ok_or_else(|| failure(path, "must be a JSON schema object"))?;
    for (keyword, item) in schema {
        validate_keyword(keyword, item, &format!("{path}.{keyword}"))?;
    }
    Ok(())
}

fn unchecked_in_map(item: &Value, at: &str) -> Option<String> {
    item.as_object()?
        .iter()
        .find_map(|(key, nested)| unchecked_keyword(nested, &format!("{at}.{key}")))
}

fn unchecked_in_array(item: &Value, at: &str) -> Option<String> {
    item.as_array()?
        .iter()
        .enumerate()
        .find_map(|(index, nested)| unchecked_keyword(nested, &format!("{at}[{index}]")))
}

fn unchecked_in_items(item: &Value, at: &str) -> Option<String> {
    if item.is_array() {
        unchecked_in_array(item, at)
    } else {
        unchecked_keyword(item, at)
    }
}

fn unchecked_entry(keyword: &str, item: &Value, at: &str) -> Option<String> {
    if matches!(keyword, "unevaluatedProperties" | "unevaluatedItems") {
        return Some(at.to_string());
    }
    if UNCHECKED_MAP_KEYWORDS.contains(&keyword) {
        return unchecked_in_map(item, at);
    }
    if UNCHECKED_LIST_KEYWORDS.contains(&keyword) {
        return unchecked_in_array(item, at);
    }
    if keyword == "items" {
        return unchecked_in_items(item, at);
    }
    if UNCHECKED_SUBSCHEMA_KEYWORDS.contains(&keyword) {
        return unchecked_keyword(item, at);
    }
    None
}

/// The first keyword (as `<path>.<keyword>`) this checker would evaluate differently from typebox, if any:
/// `unevaluatedProperties` and `unevaluatedItems`, which need typebox's evaluated-key tracking.
pub fn unchecked_keyword(schema: &Value, path: &str) -> Option<String> {
    let map = schema.as_object()?;
    map.iter()
        .find_map(|(keyword, item)| unchecked_entry(keyword, item, &format!("{path}.{keyword}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_meta_bool_subschemas() {
        for kw in [
            "items",
            "additionalItems",
            "additionalProperties",
            "unevaluatedProperties",
        ] {
            assert_eq!(validate_json_schema(&json!({ kw: false }), "root"), Ok(()));
        }
        for kw in [
            "propertyNames",
            "contains",
            "not",
            "if",
            "then",
            "else",
            "unevaluatedItems",
        ] {
            let err = validate_json_schema(&json!({ kw: true }), "root");
            assert_eq!(
                err,
                Err(format!(
                    "Invalid JSON schema at root.{kw}: must be a JSON schema object"
                ))
            );
        }
        let prefix_err = validate_json_schema(&json!({ "prefixItems": [true] }), "root");
        assert_eq!(
            prefix_err,
            Err("Invalid JSON schema at root.prefixItems[0]: must be a JSON schema object".into())
        );
    }

    #[test]
    fn test_meta_patterns_dependencies() {
        assert!(validate_json_schema(&json!([]), "root").is_ok());
        assert!(validate_json_schema(&json!([1]), "root").is_err());
        assert!(validate_json_schema(&json!({ "pattern": "[" }), "root").is_err());
        assert!(
            validate_json_schema(
                &json!({ "dependencies": { "a": ["b"], "c": { "type": "string" } } }),
                "root"
            )
            .is_ok()
        );
    }

    #[test]
    fn test_unchecked_nested_paths() {
        let schema = json!({
            "items": [{ "type": "string" }, { "unevaluatedItems": false }]
        });
        assert_eq!(
            unchecked_keyword(&schema, "root"),
            Some("root.items[1].unevaluatedItems".into())
        );
        let mapped = json!({
            "properties": { "x": { "unevaluatedProperties": false } }
        });
        assert_eq!(
            unchecked_keyword(&mapped, "root"),
            Some("root.properties.x.unevaluatedProperties".into())
        );
    }
}
