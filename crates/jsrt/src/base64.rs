//! `btoa` / `Buffer.toString("base64")`, base64url, `atob`, and `Buffer.from(text, "base64")`.

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding: `btoa` over bytes, `Buffer.toString("base64")`.
pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    STANDARD.encode(bytes)
}

/// base64url without padding (`Buffer.toString("base64url")`, JWT segments, PKCE).
pub fn encode_url(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `atob` (WHATWG forgiving-base64 decode): ASCII whitespace is ignored and padding is optional, but `=` is only
/// allowed as one or two final characters of a multiple-of-4 input, a lone final character is refused, and so is
/// anything outside the standard alphabet. `None` where `atob` throws `InvalidCharacterError`.
pub fn atob(text: &str) -> Option<Vec<u8>> {
    if !text.is_ascii() {
        return None;
    }
    let mut data: Vec<u8> = text
        .bytes()
        .filter(|b| !matches!(b, b'\t' | b'\n' | b'\x0c' | b'\r' | b' '))
        .collect();
    if data.len().is_multiple_of(4) {
        if data.ends_with(b"==") {
            data.truncate(data.len() - 2);
        } else if data.ends_with(b"=") {
            data.truncate(data.len() - 1);
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for byte in data {
        let value = ALPHABET.iter().position(|&a| a == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Node's `Buffer.from(text, "base64")`: never fails. Both alphabets are accepted (`-`/`_` as `+`/`/`), any
/// other character (whitespace included) is skipped, decoding stops at the first `=`, and leftover bits are
/// dropped.
pub fn buffer_from(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        buffer = (buffer << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsrt_base64_encode_is_standard_padded() {
        for (bytes, want) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"\xff\xfe\xfd\xfc", "//79/A=="),
            (b"\xff\xfe\xfd\x00", "//79AA=="),
            ("ü😀".as_bytes(), "w7zwn5iA"),
        ] {
            assert_eq!(encode(bytes), want);
        }
    }

    #[test]
    fn jsrt_base64_encode_url_has_no_padding() {
        assert_eq!(encode_url([0xfb, 0xff]), "-_8");
        assert_eq!(encode_url(b"f"), "Zg");
        assert_eq!(encode_url(br#"{"alg":"RS256"}"#), "eyJhbGciOiJSUzI1NiJ9");
    }

    #[test]
    fn jsrt_atob_round_trips_encode() {
        for bytes in [&b""[..], b"f", b"fo", b"foo", b"\xff\xfe\xfd\xfc", "ü😀".as_bytes()] {
            assert_eq!(atob(&encode(bytes)).as_deref(), Some(bytes));
        }
    }

    /// From ai's `ai_oauth_codex_account_id_reads_jwt_claim`: node's atob accepts a JWT payload bare and with one
    /// `=`, and throws for `==` and for the URL-safe alphabet.
    #[test]
    fn jsrt_atob_padding_and_alphabet_follow_node() {
        let payload = "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF8xMjMifX0";
        let want = br#"{"https://api.openai.com/auth":{"chatgpt_account_id":"acct_123"}}"#;
        assert_eq!(atob(payload).as_deref(), Some(&want[..]));
        assert_eq!(atob(&format!("{payload}=")).as_deref(), Some(&want[..]));
        assert_eq!(atob(&format!("{payload}==")), None);
        assert_eq!(atob(&payload.replacen('Y', "-", 1)), None);
    }

    #[test]
    fn jsrt_atob_is_forgiving_base64() {
        assert_eq!(atob(" Zm9v\n\tYg ==").as_deref(), Some(&b"foob"[..]));
        assert_eq!(atob("Zg").as_deref(), Some(&b"f"[..]));
        assert_eq!(
            atob("Zh==").as_deref(),
            Some(&b"f"[..]),
            "non-zero trailing bits are allowed"
        );
        assert_eq!(atob("Zg="), None, "padding only on a multiple of four");
        assert_eq!(atob("Z"), None, "a lone final character");
        assert_eq!(atob("Zg==Zg=="), None, "padding in the middle");
        assert_eq!(atob("Zm9v\u{a0}"), None, "non-ASCII is not whitespace");
        assert_eq!(atob("Zm9v\u{b}"), None, "vertical tab is not ASCII whitespace");
    }

    #[test]
    fn jsrt_buffer_from_is_lenient_like_node() {
        assert_eq!(buffer_from("Zm9v"), b"foo");
        assert_eq!(buffer_from("Zm9v\r\n"), b"foo");
        assert_eq!(buffer_from("Zm*9v!"), b"foo");
        assert_eq!(buffer_from("Zg==Zm9v"), b"f", "stops at padding");
        assert_eq!(buffer_from("-_8"), [0xfb, 0xff]);
        assert_eq!(buffer_from("+/8"), [0xfb, 0xff]);
        assert_eq!(buffer_from("Zm9"), b"fo");
        assert_eq!(buffer_from("Z"), b"");
    }
}
