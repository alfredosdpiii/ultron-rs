//! `encodeURIComponent`, `decodeURIComponent` and the WHATWG percent-decode that `fileURLToPath` and URL credentials use.

/// `encodeURIComponent`: every UTF-8 byte outside `A-Za-z0-9-_.!~*'()` as `%XX`.
pub fn encode_component(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn escaped_byte(bytes: &[u8], at: usize) -> Option<u8> {
    let hex = |b: &u8| (*b as char).to_digit(16);
    let high = bytes.get(at + 1).and_then(hex)?;
    let low = bytes.get(at + 2).and_then(hex)?;
    Some((high * 16 + low) as u8)
}

/// WHATWG percent-decode: `%XX` becomes its byte, any other `%` stays, and the bytes are read as lossy UTF-8.
pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = escaped_byte(bytes, i)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `decodeURIComponent`: `None` where JavaScript throws a `URIError` (a malformed escape or invalid UTF-8).
pub fn decode_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(escaped_byte(bytes, i)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_component_round_trips_like_node() {
        assert_eq!(encode_component("a b/c?d=é!'()*~"), "a%20b%2Fc%3Fd%3D%C3%A9!'()*~");
        assert_eq!(decode("a%20b%zz%2"), "a b%zz%2");
        assert_eq!(decode("%+1"), "%+1");
        assert_eq!(decode_component("a%20%C3%A9").as_deref(), Some("a é"));
        assert_eq!(decode_component("a%zz"), None);
        assert_eq!(decode_component("%C3"), None);
    }
}
