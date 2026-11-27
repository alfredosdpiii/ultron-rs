use jsrt::json_parse::parse_json;
use serde_json::{Map, Value, json};

pub(super) const REASONING_FIELDS: &[&str] = &["reasoning", "reasoning_content", "reasoning_text"];

fn is_reasoning_detail_kind(detail: &Map<String, Value>) -> bool {
    match detail.get("type").and_then(Value::as_str) {
        Some("reasoning.summary") => detail.get("summary").is_some_and(Value::is_string),
        Some("reasoning.encrypted") => detail.get("data").is_some_and(Value::is_string),
        Some("reasoning.text") => {
            detail.get("text").is_some_and(Value::is_string)
                && detail
                    .get("signature")
                    .is_none_or(|sig| sig.is_null() || sig.is_string())
        }
        _ => false,
    }
}

/// A valid OpenRouter `reasoning_details` entry.
pub(super) fn is_reasoning_detail(detail: &Value) -> bool {
    let Some(detail_obj) = detail.as_object() else {
        return false;
    };
    let common = detail_obj.get("id").is_none_or(|val| val.is_null() || val.is_string())
        && detail_obj.get("format").is_none_or(Value::is_string)
        && detail_obj.get("index").is_none_or(Value::is_number);
    common && is_reasoning_detail_kind(detail_obj)
}

pub(super) fn parse_reasoning_details(signature: Option<&str>) -> Option<Vec<Value>> {
    let parsed = parse_json(signature.filter(|sig| !sig.is_empty())?).ok()?;
    let items = parsed.as_array()?;
    (!items.is_empty() && items.iter().all(is_reasoning_detail)).then(|| items.clone())
}

pub(super) fn parse_legacy_encrypted_detail(signature: Option<&str>) -> Option<Value> {
    let parsed = parse_json(signature.filter(|sig| !sig.is_empty())?).ok()?;
    let ok = is_reasoning_detail(&parsed)
        && parsed["type"] == "reasoning.encrypted"
        && parsed
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
        && parsed
            .get("data")
            .and_then(Value::as_str)
            .is_some_and(|data| !data.is_empty());
    ok.then_some(parsed)
}

fn fill_common_fields(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    if target.get("id").is_none_or(Value::is_null)
        && let Some(id) = source.get("id")
    {
        target.insert("id".into(), id.clone());
    }
    if !jsrt::value::truthy(target.get("format"))
        && let Some(format) = source.get("format")
    {
        target.insert("format".into(), format.clone());
    }
    if target.get("index").is_none_or(Value::is_null)
        && let Some(index) = source.get("index")
    {
        target.insert("index".into(), index.clone());
    }
}

fn merge_reasoning_detail(last: &mut Map<String, Value>, source: &Map<String, Value>, kind: &str) {
    let field = if kind == "reasoning.text" { "text" } else { "summary" };
    let joined = format!(
        "{}{}",
        last[field].as_str().unwrap_or_default(),
        source[field].as_str().unwrap_or_default()
    );
    last.insert(field.into(), json!(joined));
    if kind == "reasoning.text"
        && !jsrt::value::truthy(last.get("signature"))
        && let Some(signature) = source.get("signature")
    {
        last.insert("signature".into(), signature.clone());
    }
    fill_common_fields(last, source);
}

pub(super) fn append_reasoning_detail(details: &mut Vec<Value>, detail: &Value) {
    let kind = detail["type"].as_str().unwrap_or_default();
    if let Some(Value::Object(last)) = details.last_mut()
        && last.get("type").and_then(Value::as_str) == Some(kind)
        && (kind == "reasoning.text" || kind == "reasoning.summary")
    {
        let source = detail.as_object().cloned().unwrap_or_default();
        merge_reasoning_detail(last, &source, kind);
        return;
    }
    details.push(detail.clone());
}
