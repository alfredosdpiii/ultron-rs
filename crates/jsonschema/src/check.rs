//! typebox 1.3's `Value.Check` / `Value.Errors` (`typebox/build/schema/engine`): every keyword evaluated in
//! typebox's order, its `en_US` messages, and its eight-errors-per-call cap (`Settings.maxErrors`, which the
//! compiled validator of `validateToolArguments` uses too). Strings are measured in typebox's grapheme count,
//! `pattern` and `patternProperties` are ECMAScript `u` regular expressions ([`crate::formats::js_regex`]), and
//! `format` is every format typebox registers ([`crate::formats`]). Object keys are visited in JavaScript property
//! order. Not evaluated: `$ref`/`$dynamicRef` (refused by the meta-check) and `unevaluated*` (refused by
//! [`crate::unchecked_keyword`]).

use crate::formats::{self, js_regex};
use jsrt::json::object_entries;
use serde_json::{Map, Value, json};

/// typebox `Settings.maxErrors`.
const MAX_ERRORS: usize = 8;

/// One typebox validation error (`TLocalizedValidationError` without `schemaPath`).
#[derive(Clone, Debug, PartialEq)]
pub struct SchemaError {
    pub keyword: String,
    pub instance_path: String,
    pub params: Value,
    pub message: String,
}

impl SchemaError {
    pub fn to_json(&self) -> Value {
        json!({ "keyword": self.keyword, "instancePath": self.instance_path, "params": self.params,
                "message": self.message })
    }
}

/// typebox `Value.Check(schema, value)`.
pub fn check(schema: &Value, value: &Value) -> bool {
    let mut context = Context::checking();
    eval(schema, value, "", &mut context)
}

/// typebox `Value.Errors(schema, value)`, in typebox's order (empty when the value checks).
pub fn errors(schema: &Value, value: &Value) -> Vec<SchemaError> {
    let mut context = Context::collecting();
    eval(schema, value, "", &mut context);
    context.errors.unwrap_or_default()
}

/// An error context: `None` errors when only checking.
struct Context {
    errors: Option<Vec<SchemaError>>,
}

impl Context {
    fn checking() -> Self {
        Context { errors: None }
    }
    fn collecting() -> Self {
        Context {
            errors: Some(Vec::new()),
        }
    }
    /// A fresh context of the same mode (typebox's per-operand context of anyOf, allOf, oneOf, if).
    fn nested(&self) -> Self {
        Context {
            errors: self.errors.as_ref().map(|_| Vec::new()),
        }
    }
    fn at_capacity(&self) -> bool {
        self.errors.as_ref().is_some_and(|errors| errors.len() >= MAX_ERRORS)
    }
    /// `AddError`: records the error (under the cap) and returns false.
    fn add(&mut self, keyword: &str, path: &str, params: Value) -> bool {
        if let Some(errors) = self.errors.as_mut()
            && errors.len() < MAX_ERRORS
        {
            let message = message(keyword, &params);
            errors.push(SchemaError {
                keyword: keyword.into(),
                instance_path: path.into(),
                params,
                message,
            });
        }
        false
    }
    fn absorb(&mut self, other: Context) {
        for error in other.errors.into_iter().flatten() {
            if let Some(errors) = self.errors.as_mut()
                && errors.len() < MAX_ERRORS
            {
                errors.push(error);
            }
        }
    }
}

fn joined(value: &Value) -> String {
    value.as_array().map_or_default(|items| {
        items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                other => jsrt::value::string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(", ")
    })
}

const STATIC_MESSAGES: &[(&str, &str)] = &[
    ("additionalProperties", "must not have additional properties"),
    ("anyOf", "must match a schema in anyOf"),
    ("boolean", "schema is false"),
    ("const", "must be equal to constant"),
    ("contains", "must contain at least 1 valid item"),
    ("enum", "must be equal to one of the allowed values"),
    ("not", "must not be valid"),
    ("oneOf", "must match exactly one schema in oneOf"),
    ("uniqueItems", "must not have duplicate items"),
];

const LIMIT_MESSAGES: &[(&str, &str, &str)] = &[
    ("maxItems", "not have more than", "items"),
    ("maxLength", "not have more than", "characters"),
    ("maxProperties", "not have more than", "properties"),
    ("minItems", "not have fewer than", "items"),
    ("minLength", "not have fewer than", "characters"),
    ("minProperties", "not have fewer than", "properties"),
];

const PARAM_MESSAGES: &[(&str, &str, &str, &str)] = &[
    ("format", "must match format \"", "format", "\""),
    ("if", "must match \"", "failingKeyword", "\" schema"),
    ("multipleOf", "must be multiple of ", "multipleOf", ""),
    ("pattern", "must match pattern \"", "pattern", "\""),
];

fn param_text(params: &Value, key: &str) -> String {
    match &params[key] {
        Value::String(text) => text.clone(),
        other => jsrt::value::string(Some(other)),
    }
}

fn type_message(params: &Value) -> String {
    match &params["type"] {
        Value::String(name) => format!("must be {name}"),
        names => format!(
            "must be either {}",
            names
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" or ")
        ),
    }
}

fn relational_message(keyword: &str, params: &Value) -> Option<String> {
    match keyword {
        "dependencies" | "dependentRequired" => Some(format!(
            "must have properties {} when property {} is present",
            joined(&params["dependencies"]),
            param_text(params, "property")
        )),
        "exclusiveMaximum" | "exclusiveMinimum" | "maximum" | "minimum" => Some(format!(
            "must be {} {}",
            param_text(params, "comparison"),
            param_text(params, "limit")
        )),
        "propertyNames" => Some(format!(
            "property names {} are invalid",
            joined(&params["propertyNames"])
        )),
        "required" => Some(format!(
            "must have required properties {}",
            joined(&params["requiredProperties"])
        )),
        "type" => Some(type_message(params)),
        _ => None,
    }
}

/// typebox's `en_US` locale.
fn message(keyword: &str, params: &Value) -> String {
    if let Some((_, text)) = STATIC_MESSAGES.iter().find(|(k, _)| *k == keyword) {
        return (*text).into();
    }
    if let Some((_, cmp, unit)) = LIMIT_MESSAGES.iter().find(|(k, _, _)| *k == keyword) {
        return format!("must {cmp} {} {unit}", param_text(params, "limit"));
    }
    if let Some((_, prefix, key, suffix)) = PARAM_MESSAGES.iter().find(|(k, _, _, _)| *k == keyword) {
        return format!("{prefix}{}{suffix}", param_text(params, key));
    }
    relational_message(keyword, params).unwrap_or_else(|| "an unknown validation error occurred".into())
}

/// JSON equality with JavaScript number semantics and unordered object keys (typebox `IsDeepEqual`).
pub fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_equal(x, y)),
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| json_equal(v, w)))
        }
        _ => a == b,
    }
}

fn is_integer(value: &Value) -> bool {
    value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0 && n.is_finite())
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_number() && is_integer(value),
        "number" => value.is_number(),
        "null" => value.is_null(),
        "string" => value.is_string(),
        "bigint" | "constructor" | "function" | "symbol" | "undefined" | "void" => false,
        _ => true,
    }
}

fn utf16_code_point(units: &[u16], index: usize) -> u32 {
    let first = u32::from(units[index]);
    if (0xD800..=0xDBFF).contains(&first) && index + 1 < units.len() {
        let second = u32::from(units[index + 1]);
        if (0xDC00..=0xDFFF).contains(&second) {
            return 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
        }
    }
    first
}

fn utf16_width(point: u32) -> usize {
    if point > 0xFFFF { 2 } else { 1 }
}

fn is_grapheme_modifier(point: u32) -> bool {
    (0x0300..=0x036F).contains(&point)
        || (0x1AB0..=0x1AFF).contains(&point)
        || (0x1DC0..=0x1DFF).contains(&point)
        || (0xFE20..=0xFE2F).contains(&point)
        || (0xFE00..=0xFE0F).contains(&point)
}

fn is_regional_indicator(point: u32) -> bool {
    (0x1F1E6..=0x1F1FF).contains(&point)
}

fn skip_grapheme_modifiers(units: &[u16], mut index: usize) -> usize {
    while index < units.len() {
        let point = utf16_code_point(units, index);
        if !is_grapheme_modifier(point) {
            break;
        }
        index += utf16_width(point);
    }
    index
}

/// typebox's `GraphemeCount` (its own segmentation over UTF-16 code units).
pub fn grapheme_count(text: &str) -> usize {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut count = 0;
    let mut index = 0;
    while index < units.len() {
        let start = utf16_code_point(&units, index);
        let mut end = skip_grapheme_modifiers(&units, index + utf16_width(start));
        while end + 1 < units.len() && utf16_code_point(&units, end) == 0x200D {
            let next = utf16_code_point(&units, end + 1);
            end = skip_grapheme_modifiers(&units, end + 1 + utf16_width(next));
        }
        if is_regional_indicator(start) && end < units.len() && is_regional_indicator(utf16_code_point(&units, end)) {
            end += utf16_width(utf16_code_point(&units, end));
        }
        index = end;
        count += 1;
    }
    count
}

fn multiple_of(dividend: f64, divisor: f64) -> bool {
    if dividend.fract() == 0.0 && (1.0 / divisor).fract() == 0.0 {
        return true;
    }
    let modulo = dividend % divisor;
    modulo.abs().min((modulo - divisor).abs()).min((modulo + divisor).abs()) < 1e-10
}

/// `new RegExp(pattern, "u").test(text)` (typebox's `UnicodeRegExp`); a pattern that does not compile matches nothing.
fn regex_test(pattern: &str, text: &str) -> bool {
    js_regex(pattern).is_some_and(|regex| regex.is_match(text).unwrap_or(false))
}

/// `GetPropertiesPattern`: a key covered by `properties` or a `patternProperties` pattern.
fn declared(schema: &Map<String, Value>, key: &str) -> bool {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .is_some_and(|properties| properties.contains_key(key))
        || schema
            .get("patternProperties")
            .and_then(Value::as_object)
            .is_some_and(|patterns| patterns.keys().any(|pattern| regex_test(pattern, key)))
}

fn eval_type(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    let Some(types) = schema.get("type") else {
        return true;
    };
    let matches = match types {
        Value::Array(names) => names
            .iter()
            .any(|name| name.as_str().is_some_and(|name| type_matches(name, value))),
        Value::String(name) => type_matches(name, value),
        _ => true,
    };
    if matches {
        true
    } else {
        context.add("type", path, json!({ "type": types }))
    }
}

fn eval_value_kind(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    match value {
        Value::Object(object) => eval_object(schema, object, path, context),
        Value::Array(items) => eval_array(schema, items, path, context),
        Value::String(text) => eval_string(schema, text, path, context),
        Value::Number(number) => eval_number(schema, number.as_f64().unwrap_or(f64::NAN), path, context),
        _ => true,
    }
}

fn eval_const_enum(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    if let Some(constant) = schema.get("const")
        && !json_equal(value, constant)
    {
        ok &= context.add("const", path, json!({ "allowedValue": constant }));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.iter().any(|option| json_equal(value, option))
    {
        ok &= context.add("enum", path, json!({ "allowedValues": options }));
    }
    ok
}

fn eval_conditional_and_not(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    if let Some(condition) = schema.get("if") {
        let then = schema.get("then").unwrap_or(&Value::Bool(true));
        let otherwise = schema.get("else").unwrap_or(&Value::Bool(true));
        let mut inner = context.nested();
        let passed = if eval(condition, value, path, &mut inner) {
            eval(then, value, path, &mut inner) || context.add("if", path, json!({ "failingKeyword": "then" }))
        } else {
            eval(otherwise, value, path, context) || context.add("if", path, json!({ "failingKeyword": "else" }))
        };
        ok &= passed;
    }
    if let Some(negated) = schema.get("not")
        && eval(negated, value, path, &mut Context::checking())
    {
        ok &= context.add("not", path, json!({}));
    }
    ok
}

fn eval_all_of(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    let Some(options) = schema.get("allOf").and_then(Value::as_array) else {
        return true;
    };
    let mut failed = Vec::new();
    for option in options {
        let mut inner = context.nested();
        if !eval(option, value, path, &mut inner) {
            failed.push(inner);
        }
    }
    if failed.is_empty() {
        return true;
    }
    for inner in failed {
        context.absorb(inner);
    }
    false
}

fn eval_any_of(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    let Some(options) = schema.get("anyOf").and_then(Value::as_array) else {
        return true;
    };
    let mut failed = Vec::new();
    let mut passed = 0;
    for option in options {
        let mut inner = context.nested();
        if eval(option, value, path, &mut inner) {
            passed += 1;
        } else {
            failed.push(inner);
        }
    }
    if passed > 0 {
        return true;
    }
    for inner in failed {
        context.absorb(inner);
    }
    context.add("anyOf", path, json!({}))
}

fn eval_one_of(schema: &Map<String, Value>, value: &Value, path: &str, context: &mut Context) -> bool {
    let Some(options) = schema.get("oneOf").and_then(Value::as_array) else {
        return true;
    };
    let mut failed = Vec::new();
    let mut passing = Vec::new();
    for (index, option) in options.iter().enumerate() {
        let mut inner = context.nested();
        if eval(option, value, path, &mut inner) {
            passing.push(index);
        } else {
            failed.push(inner);
        }
    }
    if passing.len() == 1 {
        return true;
    }
    if passing.is_empty() {
        for inner in failed {
            context.absorb(inner);
        }
    }
    context.add("oneOf", path, json!({ "passingSchemas": passing }))
}

/// `ErrorSchema` / `CheckSchema`: every keyword is evaluated (typebox's bitwise `&`), in typebox's order.
fn eval(schema: &Value, value: &Value, path: &str, context: &mut Context) -> bool {
    if context.at_capacity() {
        return false;
    }
    let schema = match schema {
        Value::Bool(true) => return true,
        Value::Bool(false) => return context.add("boolean", path, json!({})),
        Value::Object(schema) => schema,
        _ => return true,
    };
    let mut ok = true;
    ok &= eval_type(schema, value, path, context);
    ok &= eval_value_kind(schema, value, path, context);
    ok &= eval_const_enum(schema, value, path, context);
    ok &= eval_conditional_and_not(schema, value, path, context);
    ok &= eval_all_of(schema, value, path, context);
    ok &= eval_any_of(schema, value, path, context);
    ok &= eval_one_of(schema, value, path, context);
    ok
}

fn eval_required(schema: &Map<String, Value>, object: &Map<String, Value>, path: &str, context: &mut Context) -> bool {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return true;
    };
    let missing: Vec<&Value> = required
        .iter()
        .filter(|key| key.as_str().is_some_and(|key| !object.contains_key(key)))
        .collect();
    if missing.is_empty() {
        true
    } else {
        context.add("required", path, json!({ "requiredProperties": missing }))
    }
}

fn eval_additional_properties(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(additional) = schema.get("additionalProperties") else {
        return true;
    };
    let mut rejected = Vec::new();
    for (key, item) in object_entries(object) {
        if !declared(schema, key) && !eval(additional, item, &format!("{path}/{key}"), context) {
            rejected.push(key.clone());
        }
    }
    if rejected.is_empty() {
        true
    } else {
        context.add(
            "additionalProperties",
            path,
            json!({ "additionalProperties": rejected }),
        )
    }
}

fn eval_dependency_array(
    key: &str,
    dependency: &Value,
    keys: &[Value],
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    for needed in keys {
        if !needed.as_str().is_some_and(|needed| object.contains_key(needed)) {
            return context.add(
                "dependencies",
                path,
                json!({ "property": key, "dependencies": dependency }),
            );
        }
    }
    true
}

fn eval_dependencies(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(dependencies) = schema.get("dependencies").and_then(Value::as_object) else {
        return true;
    };
    let mut every = true;
    for (key, dependency) in dependencies {
        if !object.contains_key(key) {
            continue;
        }
        match dependency {
            Value::Array(keys) => every &= eval_dependency_array(key, dependency, keys, object, path, context),
            nested => every &= eval(nested, &Value::Object(object.clone()), path, context),
        }
    }
    object.is_empty() || every
}

fn eval_dependent_required_entry(
    key: &str,
    keys: &Value,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let mut every = true;
    for needed in keys.as_array().into_iter().flatten() {
        if !needed.as_str().is_some_and(|needed| object.contains_key(needed)) {
            every &= context.add(
                "dependentRequired",
                path,
                json!({ "property": key, "dependencies": keys }),
            );
        }
    }
    every
}

fn eval_dependent_required(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(dependencies) = schema.get("dependentRequired").and_then(Value::as_object) else {
        return true;
    };
    let mut every = true;
    for (key, keys) in dependencies {
        if object.contains_key(key) {
            every &= eval_dependent_required_entry(key, keys, object, path, context);
        }
    }
    object.is_empty() || every
}

fn eval_dependent_schemas(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(dependents) = schema.get("dependentSchemas").and_then(Value::as_object) else {
        return true;
    };
    let mut every = true;
    for (key, nested) in dependents {
        if object.contains_key(key) {
            every &= eval(nested, &Value::Object(object.clone()), path, context);
        }
    }
    object.is_empty() || every
}

fn eval_pattern_properties(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(patterns) = schema.get("patternProperties").and_then(Value::as_object) else {
        return true;
    };
    let mut ok = true;
    for (pattern, nested) in patterns {
        for (key, item) in object_entries(object) {
            if regex_test(pattern, key) {
                ok &= eval(nested, item, &format!("{path}/{key}"), context);
            }
        }
    }
    ok
}

fn eval_properties(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return true;
    };
    let mut ok = true;
    for (key, nested) in properties {
        if let Some(item) = object.get(key) {
            ok &= eval(nested, item, &format!("{path}/{key}"), context);
        }
    }
    ok
}

fn eval_property_names(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let Some(names) = schema.get("propertyNames") else {
        return true;
    };
    let mut rejected = Vec::new();
    for (key, _) in object_entries(object) {
        if !eval(names, &Value::String(key.clone()), &format!("{path}/{key}"), context) {
            rejected.push(key.clone());
        }
    }
    if rejected.is_empty() {
        true
    } else {
        context.add("propertyNames", path, json!({ "propertyNames": rejected }))
    }
}

fn eval_property_count_bounds(
    schema: &Map<String, Value>,
    object: &Map<String, Value>,
    path: &str,
    context: &mut Context,
) -> bool {
    let mut ok = true;
    let count = object.len() as f64;
    if let Some(limit) = schema
        .get("minProperties")
        .filter(|l| l.as_f64().is_some_and(|l| count < l))
    {
        ok &= context.add("minProperties", path, json!({ "limit": limit }));
    }
    if let Some(limit) = schema
        .get("maxProperties")
        .filter(|l| l.as_f64().is_some_and(|l| count > l))
    {
        ok &= context.add("maxProperties", path, json!({ "limit": limit }));
    }
    ok
}

fn eval_object(schema: &Map<String, Value>, object: &Map<String, Value>, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    ok &= eval_required(schema, object, path, context);
    ok &= eval_additional_properties(schema, object, path, context);
    ok &= eval_dependencies(schema, object, path, context);
    ok &= eval_dependent_required(schema, object, path, context);
    ok &= eval_dependent_schemas(schema, object, path, context);
    ok &= eval_pattern_properties(schema, object, path, context);
    ok &= eval_properties(schema, object, path, context);
    ok &= eval_property_names(schema, object, path, context);
    ok &= eval_property_count_bounds(schema, object, path, context);
    ok
}

fn item_path(path: &str, index: usize) -> String {
    format!("{path}/{index}")
}

fn count_contains(contains: &Value, items: &[Value], path: &str) -> usize {
    items
        .iter()
        .filter(|item| eval(contains, item, path, &mut Context::checking()))
        .count()
}

fn eval_additional_items(schema: &Map<String, Value>, items: &[Value], path: &str, context: &mut Context) -> bool {
    let (Some(additional), Some(Value::Array(sized))) = (schema.get("additionalItems"), schema.get("items")) else {
        return true;
    };
    for (index, item) in items.iter().enumerate().skip(sized.len()) {
        if !eval(additional, item, &item_path(path, index), context) {
            return false;
        }
    }
    true
}

fn eval_contains_presence(schema: &Map<String, Value>, items: &[Value], path: &str, context: &mut Context) -> bool {
    let Some(contains) = schema.get("contains") else {
        return true;
    };
    if schema.get("minContains").and_then(Value::as_f64) == Some(0.0) {
        return true;
    }
    if count_contains(contains, items, path) == 0 {
        return context.add("contains", path, json!({ "minContains": 1 }));
    }
    true
}

fn eval_tuple_items(sized: &[Value], items: &[Value], path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    for (index, nested) in sized.iter().enumerate() {
        if let Some(item) = items.get(index) {
            ok &= eval(nested, item, &item_path(path, index), context);
        }
    }
    ok
}

fn eval_items(schema: &Map<String, Value>, items: &[Value], path: &str, context: &mut Context) -> bool {
    match schema.get("items") {
        Some(Value::Array(sized)) => eval_tuple_items(sized, items, path, context),
        Some(each) => {
            let offset = schema.get("prefixItems").and_then(Value::as_array).map_or(0, Vec::len);
            let mut ok = true;
            for (index, item) in items.iter().enumerate().skip(offset) {
                ok &= eval(each, item, &item_path(path, index), context);
            }
            ok
        }
        None => true,
    }
}

fn eval_max_contains_and_max_items(
    schema: &Map<String, Value>,
    items: &[Value],
    path: &str,
    context: &mut Context,
) -> bool {
    let mut ok = true;
    if let Some(contains) = schema.get("contains")
        && let Some(limit) = schema.get("maxContains").and_then(Value::as_f64)
        && count_contains(contains, items, path) as f64 > limit
    {
        let min = schema.get("minContains").cloned().unwrap_or(json!(1));
        ok &= context.add(
            "contains",
            path,
            json!({ "minContains": min, "maxContains": schema["maxContains"] }),
        );
    }
    let length = items.len() as f64;
    if let Some(limit) = schema
        .get("maxItems")
        .filter(|l| l.as_f64().is_some_and(|l| length > l))
    {
        ok &= context.add("maxItems", path, json!({ "limit": limit }));
    }
    ok
}

fn eval_min_contains_and_min_items(
    schema: &Map<String, Value>,
    items: &[Value],
    path: &str,
    context: &mut Context,
) -> bool {
    let mut ok = true;
    if let Some(contains) = schema.get("contains")
        && let Some(limit) = schema.get("minContains").and_then(Value::as_f64)
        && (count_contains(contains, items, path) as f64) < limit
    {
        ok &= context.add("contains", path, json!({ "minContains": schema["minContains"] }));
    }
    let length = items.len() as f64;
    if let Some(limit) = schema
        .get("minItems")
        .filter(|l| l.as_f64().is_some_and(|l| length < l))
    {
        ok &= context.add("minItems", path, json!({ "limit": limit }));
    }
    ok
}

fn eval_prefix_and_unique_items(
    schema: &Map<String, Value>,
    items: &[Value],
    path: &str,
    context: &mut Context,
) -> bool {
    let mut ok = true;
    if let Some(prefix) = schema.get("prefixItems").and_then(Value::as_array)
        && !items.is_empty()
    {
        for (index, nested) in prefix.iter().enumerate() {
            if let Some(item) = items.get(index) {
                ok &= eval(nested, item, &item_path(path, index), context);
            }
        }
    }
    if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
        let duplicates: Vec<usize> = (0..items.len())
            .filter(|&i| items[..i].iter().any(|seen| json_equal(seen, &items[i])))
            .collect();
        if !duplicates.is_empty() {
            ok &= context.add("uniqueItems", path, json!({ "duplicateItems": duplicates }));
        }
    }
    ok
}

fn eval_array(schema: &Map<String, Value>, items: &[Value], path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    ok &= eval_additional_items(schema, items, path, context);
    ok &= eval_contains_presence(schema, items, path, context);
    ok &= eval_items(schema, items, path, context);
    ok &= eval_max_contains_and_max_items(schema, items, path, context);
    ok &= eval_min_contains_and_min_items(schema, items, path, context);
    ok &= eval_prefix_and_unique_items(schema, items, path, context);
    ok
}

fn eval_string(schema: &Map<String, Value>, text: &str, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    let length = || grapheme_count(text) as f64;
    if let Some(limit) = schema
        .get("maxLength")
        .filter(|l| l.as_f64().is_some_and(|l| length() > l))
    {
        ok &= context.add("maxLength", path, json!({ "limit": limit }));
    }
    if let Some(limit) = schema
        .get("minLength")
        .filter(|l| l.as_f64().is_some_and(|l| length() < l))
    {
        ok &= context.add("minLength", path, json!({ "limit": limit }));
    }
    if let Some(format) = schema.get("format").and_then(Value::as_str)
        && !formats::matches(format, text)
    {
        ok &= context.add("format", path, json!({ "format": format }));
    }
    if let Some(pattern) = schema.get("pattern").and_then(Value::as_str)
        && !regex_test(pattern, text)
    {
        ok &= context.add("pattern", path, json!({ "pattern": pattern }));
    }
    ok
}

/// A numeric bound keyword, its comparison, and whether a value satisfies it.
type Bound = (&'static str, &'static str, fn(f64, f64) -> bool);

fn eval_number(schema: &Map<String, Value>, n: f64, path: &str, context: &mut Context) -> bool {
    let mut ok = true;
    let bounds: [Bound; 4] = [
        ("exclusiveMaximum", "<", |n, b| n < b),
        ("exclusiveMinimum", ">", |n, b| n > b),
        ("maximum", "<=", |n, b| n <= b),
        ("minimum", ">=", |n, b| n >= b),
    ];
    for (keyword, comparison, holds) in bounds {
        if let Some(limit) = schema.get(keyword)
            && !limit.as_f64().is_some_and(|b| holds(n, b))
        {
            ok &= context.add(keyword, path, json!({ "comparison": comparison, "limit": limit }));
        }
    }
    if let Some(divisor) = schema.get("multipleOf")
        && !divisor.as_f64().is_some_and(|d| multiple_of(n, d))
    {
        ok &= context.add("multipleOf", path, json!({ "multipleOf": divisor }));
    }
    ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_if_then_else_absorption() {
        let schema = json!({
            "if": { "type": "number" },
            "then": { "minimum": 10 },
            "else": { "minLength": 3 }
        });
        let then_errs = errors(&schema, &json!(5));
        assert_eq!(then_errs.len(), 1);
        assert_eq!(then_errs[0].keyword, "if");
        assert_eq!(then_errs[0].params, json!({ "failingKeyword": "then" }));

        let else_errs = errors(&schema, &json!("ab"));
        assert_eq!(else_errs.len(), 2);
        assert_eq!(else_errs[0].keyword, "minLength");
        assert_eq!(else_errs[1].keyword, "if");
        assert_eq!(else_errs[1].params, json!({ "failingKeyword": "else" }));
    }

    #[test]
    fn test_dependencies_vs_dependent_required() {
        let deps_schema = json!({ "dependencies": { "a": ["b", "c"] } });
        let dep_errs = errors(&deps_schema, &json!({ "a": 1 }));
        assert_eq!(dep_errs.len(), 1);
        assert_eq!(dep_errs[0].keyword, "dependencies");

        let req_schema = json!({ "dependentRequired": { "a": ["b", "c"] } });
        let req_errs = errors(&req_schema, &json!({ "a": 1 }));
        assert_eq!(req_errs.len(), 2);
        assert_eq!(req_errs[0].keyword, "dependentRequired");
        assert_eq!(req_errs[1].keyword, "dependentRequired");
    }

    #[test]
    fn test_array_contains_item_order() {
        let schema = json!({
            "contains": { "type": "number" },
            "maxContains": 1,
            "maxItems": 1,
            "minItems": 4
        });
        let errs = errors(&schema, &json!([1, 2]));
        let keywords: Vec<&str> = errs.iter().map(|e| e.keyword.as_str()).collect();
        assert_eq!(keywords, vec!["contains", "maxItems", "minItems"]);
    }

    #[test]
    fn test_grapheme_count_clusters() {
        assert_eq!(grapheme_count("e\u{0301}"), 1);
        assert_eq!(grapheme_count("\u{1F1FA}\u{1F1F8}"), 1);
        assert_eq!(grapheme_count("\u{1F469}\u{200D}\u{1F52C}"), 1);
    }

    #[test]
    fn test_message_formats() {
        let msg = message("type", &json!({ "type": ["string", "number"] }));
        assert_eq!(msg, "must be either string or number");
        assert_eq!(
            message("maxItems", &json!({ "limit": 3 })),
            "must not have more than 3 items"
        );
        assert_eq!(message("unknown", &json!({})), "an unknown validation error occurred");
    }
}
