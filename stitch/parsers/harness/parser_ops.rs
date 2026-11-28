pub use crate::objects::*;
use serde_json::Value;
use stitch::*;

endpoint! {
    fn jsrt_json_and_repair(raw: Str<512, JsonInput>) {
        let _ = jsrt::json_parse::parse_json(&raw);
        let repaired = jsrt::json_parse::repair_json(&raw);
        let _ = jsrt::json_parse::parse_json_with_repair(&repaired);
        let _ = jsrt::json_parse::parse_streaming_json(&raw);
        let _ = jsrt::json_parse::partial_parse(&raw);
    }
}

endpoint! {
    fn jsrt_uri_date_number(raw: Str<256, TextInput>, digits: Range<0, 20>) {
        let encoded = jsrt::uri::encode_component(&raw);
        let _ = jsrt::uri::decode(&encoded);
        let _ = jsrt::uri::decode_component(&raw);
        if let Some(ms) = jsrt::date::parse_date(&raw) {
            let _ = jsrt::date::iso_string(ms);
        }
        let parsed = jsrt::number::parse(&raw);
        let _ = jsrt::number::parse_float(&raw);
        if parsed.is_finite() {
            let _ = jsrt::number::to_string(parsed);
            let _ = jsrt::number::to_fixed(parsed, *digits as usize);
        }
    }
}

endpoint! {
    fn mcp_jsonrpc_parse(raw: Str<512, JsonInput>) {
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let _ = mcp::jsonrpc::is_request(&value);
        let _ = mcp::jsonrpc::is_notification(&value);
        let _ = mcp::jsonrpc::is_response(&value);
        let _ = mcp::jsonrpc::id_text(&value);
        let _ = mcp::jsonrpc::parse_message(value);
    }
}

endpoint! {
    fn model_scope_and_usage_parsers(raw: Str<512, TextInput>) {
        let _ = model_scope::is_model_ref(&raw);
        let _ = model_scope::is_valid_thinking_level(&raw);
        let _ = usage::stats::cell_apis(&raw);
        let _ = usage::stats::classify_nudge(&raw);
        let _ = usage::stats::stats_lane(&raw);
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            let _ = model_scope::thinking_level(&value);
            let _ = model_scope::model_ref(&value);
            let _ = usage::stats::parse_session_stats(Some(&value));
        }
    }
}
