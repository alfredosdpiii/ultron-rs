//! Small JavaScript runtime semantics the ported providers depend on: `TextDecoder` streaming UTF-8 decoding and the
//! Latin-1 binary string of `atob`. (`atob` / `btoa`, `trim` and V8's `JSON.parse` messages are `jsrt::base64`,
//! `jsrt::string` and `jsrt::json::parse`.)

/// `new TextDecoder()` used with `{ stream: true }`: invalid sequences become U+FFFD, an incomplete sequence at the end
/// of a chunk waits for the next one.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        let mut rest: &[u8] = &self.pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    out.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    out.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &after[len..];
                        }
                        None => {
                            rest = after;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        out
    }

    /// `decoder.decode()`: flush what is left, lossily.
    pub fn finish(&mut self) -> String {
        let out = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        out
    }
}

/// The Latin-1 "binary string" `atob` returns, as JavaScript sees it.
pub fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// `/first.?second/i`: `first`, at most one character, then `second`, case-insensitively.
pub fn loose_match(text: &str, first: &str, second: &str) -> bool {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let first: Vec<char> = first.chars().collect();
    let second: Vec<char> = second.chars().collect();
    let matches_at = |i: usize, pattern: &[char]| lower.get(i..i + pattern.len()) == Some(pattern);
    (0..lower.len()).any(|i| {
        matches_at(i, &first) && (matches_at(i + first.len(), &second) || matches_at(i + first.len() + 1, &second))
    })
}
