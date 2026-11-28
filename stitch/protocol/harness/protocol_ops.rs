pub use crate::objects::*;
use protocol::{
    CborOptions, FrameDecoder, FrameError, decode_cbor, encode_cbor, encode_frame,
    parse_client_message, parse_server_message, parse_service_call,
};
use stitch::*;

endpoint! {
    fn cbor_decode_roundtrip(raw: Bytes<512, WireBytes>) {
        let opts = CborOptions::default();
        if let Ok(value) = decode_cbor(&raw, &opts) {
            if let Ok(reencoded) = encode_cbor(&value, &opts) {
                let _ = decode_cbor(&reencoded, &opts);
            }
            let _ = parse_client_message(&value);
            let _ = parse_server_message(&value);
            let _ = parse_service_call(&value);
        }
    }
}

endpoint! {
    fn frame_decoder_new() -> Result<New<WireFrameDecoder>, FrameError> {
        Ok(New::new(FrameDecoder::new(64 * 1024)?))
    }
}

endpoint! {
    fn frame_decoder_push_raw(decoder: Borrow<WireFrameDecoder>, chunk: Bytes<512, WireBytes>) {
        let _ = decoder.push(&chunk);
    }
}

endpoint! {
    fn frame_decoder_push_framed(decoder: Borrow<WireFrameDecoder>, payload: Bytes<512, WireBytes>) {
        if let Ok(framed) = encode_frame(&payload) {
            let _ = decoder.push(&framed);
        }
    }
}

endpoint! {
    fn session_file_parse_and_migrate(jsonl: Str<1024, JsonlText>) {
        let mut entries = session_file::parse_session_entries(&jsonl);
        let mut env = session_file::SystemEnv;
        let _ = session_file::migrate_session_entries(&mut entries, &mut env);
        let _ = session_file::build_session_context(&entries, session_file::Leaf::Last);
    }
}
