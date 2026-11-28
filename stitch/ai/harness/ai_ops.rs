pub use crate::objects::*;
use serde_json::Value;
use stitch::*;

endpoint! {
    fn ai_strip_json_comments_and_order(raw: Str<512, WireText>) {
        let stripped = ai::model_config::strip_json_comments(&raw);
        if let Ok(val) = serde_json::from_str::<Value>(&stripped) {
            let _ = ai::model_config::js_order(val);
        }
    }
}

endpoint! {
    fn ai_http_wire_and_dates(
        enc_idx: Range<0, 3>,
        raw: Bytes<512, WireRaw>,
        date_str: Str<128, WireText>,
    ) {
        let enc = match *enc_idx {
            0 => Some("zstd"),
            1 => Some("gzip"),
            2 => Some("identity"),
            _ => None,
        };
        let _ = ai::http::decode_wire_body(enc, &raw);
        let _ = ai::http::parse_http_date(&date_str);
        let _ = ai::azure_openai_responses::normalize_azure_base_url(&date_str);
    }
}

endpoint! {
    fn ai_claude_and_google_parsers(raw: Str<512, WireText>, vertex: bool) {
        let Ok(val) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let _ = ai::claude_code_cli::usage_from_result(&val);
        let _ = ai::claude_code_cli::rate_limit_info(&val);
        if let Some(arr) = val.as_array() {
            let _ = ai::google_shared::contents_to_wire(arr, vertex);
        }
    }
}
