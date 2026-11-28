pub use crate::objects::*;
use serde_json::Value;
use stitch::*;

endpoint! {
    fn ws_decoder_new(is_client: bool) -> New<WsDecoder> {
        if is_client {
            New::new(websocket::MessageDecoder::client(64 * 1024))
        } else {
            New::new(websocket::MessageDecoder::server(64 * 1024))
        }
    }
}

endpoint! {
    fn ws_decoder_push_raw(decoder: Borrow<WsDecoder>, chunk: Bytes<512, WsBytes>) {
        let _ = decoder.push(&chunk);
    }
}

endpoint! {
    fn ws_encode_decode(
        decoder: Borrow<WsDecoder>,
        opcode: Range<0, 15>,
        payload: Bytes<256, WsBytes>,
        masked: bool,
    ) {
        let mask = if masked { Some([0x12, 0x34, 0x56, 0x78]) } else { None };
        let frame = websocket::encode_frame(*opcode as u8, &payload, mask);
        let _ = decoder.push(&frame);
    }
}

endpoint! {
    fn ws_close_and_handshake(code: u16, reason: Str<128, ThemeDoc>) {
        let _ = websocket::close_payload(Some(code), &reason);
        let _ = websocket::accept_key(&reason);
    }
}

endpoint! {
    fn chord_delta_validate_and_decode(raw: Str<512, JsonPayload>) {
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let _ = chord_delta::validate_wire_op(&value);
        let mut decoder = chord_delta::Decoder::default();
        if let Some(ops) = value.as_array() {
            let _ = decoder.decode(ops);
        }
    }
}

endpoint! {
    fn compaction_extract_and_serialize(raw: Str<1024, JsonPayload>) {
        let Ok(value) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        let mut ops = compaction::FileOperations::default();
        compaction::extract_file_ops_from_message(&value, &mut ops);
        let _ = compaction::compute_file_lists(&ops);
        if let Some(arr) = value.as_array() {
            let _ = compaction::serialize_conversation(arr);
            let _ = compaction::find_cut_point(arr, 0, arr.len(), 4096);
        }
    }
}

endpoint! {
    fn theme_parse_and_colors(raw: Str<512, ThemeDoc>) {
        let _ = theme::hex_to_rgb(&raw);
        let _ = theme::parse_auto_theme_setting(Some(&raw));
        let _ = theme::detect_terminal_background_from_env(Some(&raw));
        if let Ok(parsed) = theme::parse_theme_json_content("fuzz", &raw) {
            let _ = theme::resolve_theme_colors(&parsed);
            let _ = theme::theme_export_colors(&parsed);
        }
    }
}
