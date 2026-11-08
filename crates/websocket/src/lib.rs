//! RFC 6455 WebSocket framing without I/O: frame encoding (masked as a client sends, unmasked as a server sends), a
//! decoder that reassembles fragmented messages and validates frames, close payloads, and the handshake's
//! `Sec-WebSocket-Accept` value. The Radius relay's host and client (crates/server, crates/client) and the providers'
//! WebSocket client (crates/ai) speak it over an upgraded HTTP connection; tests use it for fake relays and servers.

/// The GUID appended to `Sec-WebSocket-Key` for `Sec-WebSocket-Accept`.
const ACCEPT_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
pub const OPCODE_CLOSE: u8 = 0x8;
pub const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xa;

/// A whole message (fragments reassembled) or a control frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Binary(Vec<u8>),
    /// A close frame: its status code (none when the payload is empty) and reason.
    Close(Option<u16>, String),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
}

/// One frame on the wire; `mask` is set for frames a client sends.
pub fn encode_frame(opcode: u8, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | (opcode & 0x0f));
    let mask_bit = if mask.is_some() { 0x80 } else { 0 };
    match payload.len() {
        len @ 0..=125 => out.push(mask_bit | len as u8),
        len @ 126..=0xffff => {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        len => {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        }
        None => out.extend_from_slice(payload),
    }
    out
}

pub fn encode_message(message: &Message, mask: Option<[u8; 4]>) -> Vec<u8> {
    match message {
        Message::Text(text) => encode_frame(OPCODE_TEXT, text.as_bytes(), mask),
        Message::Binary(bytes) => encode_frame(OPCODE_BINARY, bytes, mask),
        Message::Close(code, reason) => encode_frame(OPCODE_CLOSE, &close_payload(*code, reason), mask),
        Message::Ping(bytes) => encode_frame(OPCODE_PING, bytes, mask),
        Message::Pong(bytes) => encode_frame(OPCODE_PONG, bytes, mask),
    }
}

/// A close frame's payload: the code (big-endian) and the UTF-8 reason, or nothing without a code.
pub fn close_payload(code: Option<u16>, reason: &str) -> Vec<u8> {
    let Some(code) = code else {
        return Vec::new();
    };
    let mut out = code.to_be_bytes().to_vec();
    out.extend_from_slice(reason.as_bytes());
    out
}

/// Decodes a byte stream into messages; any protocol violation is an error and the stream is unusable afterwards.
#[derive(Debug)]
pub struct MessageDecoder {
    buffer: Vec<u8>,
    max_message: usize,
    /// The opcode and payload of a fragmented message under way.
    fragments: Option<(u8, Vec<u8>)>,
    expect_masked: bool,
}

impl MessageDecoder {
    /// A client's decoder (it reads unmasked server frames).
    pub fn client(max_message: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_message,
            fragments: None,
            expect_masked: false,
        }
    }

    /// A server's decoder (it reads masked client frames).
    pub fn server(max_message: usize) -> Self {
        Self {
            expect_masked: true,
            ..Self::client(max_message)
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Message>, String> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((fin, opcode, payload)) = self.next_frame()? {
            if let Some(message) = self.assemble(fin, opcode, payload)? {
                out.push(message);
            }
        }
        Ok(out)
    }

    fn next_frame(&mut self) -> Result<Option<(bool, u8, Vec<u8>)>, String> {
        let b = &self.buffer;
        if b.len() < 2 {
            return Ok(None);
        }
        if b[0] & 0x70 != 0 {
            return Err("Reserved bits must be clear".into());
        }
        let fin = b[0] & 0x80 != 0;
        let opcode = b[0] & 0x0f;
        let masked = b[1] & 0x80 != 0;
        if masked != self.expect_masked {
            return Err(if masked {
                "A server must not mask frames".into()
            } else {
                "A client must mask frames".into()
            });
        }
        let (len, mut at) = match b[1] & 0x7f {
            126 if b.len() < 4 => return Ok(None),
            126 => (u16::from_be_bytes([b[2], b[3]]) as u64, 4),
            127 if b.len() < 10 => return Ok(None),
            127 => {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(&b[2..10]);
                (u64::from_be_bytes(bytes), 10)
            }
            len => (len as u64, 2),
        };
        let control = opcode >= 0x8;
        if control && (len > 125 || !fin) {
            return Err("Invalid control frame".into());
        }
        if len > self.max_message as u64 {
            return Err("Message too large".into());
        }
        let key = if masked {
            if b.len() < at + 4 {
                return Ok(None);
            }
            let key = [b[at], b[at + 1], b[at + 2], b[at + 3]];
            at += 4;
            Some(key)
        } else {
            None
        };
        let len = len as usize;
        if b.len() < at + len {
            return Ok(None);
        }
        let mut payload = b[at..at + len].to_vec();
        if let Some(key) = key {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[i % 4];
            }
        }
        self.buffer.drain(..at + len);
        Ok(Some((fin, opcode, payload)))
    }

    fn assemble(&mut self, fin: bool, opcode: u8, payload: Vec<u8>) -> Result<Option<Message>, String> {
        match opcode {
            OPCODE_CLOSE => {
                if payload.len() == 1 {
                    return Err("Invalid close payload".into());
                }
                if payload.is_empty() {
                    return Ok(Some(Message::Close(None, String::new())));
                }
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                let reason = String::from_utf8(payload[2..].to_vec()).map_err(|_| "Invalid close reason")?;
                Ok(Some(Message::Close(Some(code), reason)))
            }
            OPCODE_PING => Ok(Some(Message::Ping(payload))),
            OPCODE_PONG => Ok(Some(Message::Pong(payload))),
            OPCODE_TEXT | OPCODE_BINARY => {
                if self.fragments.is_some() {
                    return Err("Expected a continuation frame".into());
                }
                if fin {
                    return finish(opcode, payload).map(Some);
                }
                self.fragments = Some((opcode, payload));
                Ok(None)
            }
            OPCODE_CONTINUATION => {
                let Some((first, mut data)) = self.fragments.take() else {
                    return Err("Unexpected continuation frame".into());
                };
                data.extend_from_slice(&payload);
                if data.len() > self.max_message {
                    return Err("Message too large".into());
                }
                if fin {
                    return finish(first, data).map(Some);
                }
                self.fragments = Some((first, data));
                Ok(None)
            }
            _ => Err(format!("Unknown opcode {opcode}")),
        }
    }
}

fn finish(opcode: u8, payload: Vec<u8>) -> Result<Message, String> {
    if opcode == OPCODE_TEXT {
        String::from_utf8(payload)
            .map(Message::Text)
            .map_err(|_| "Invalid UTF-8 in a text message".into())
    } else {
        Ok(Message::Binary(payload))
    }
}

/// A client's opening handshake (RFC 6455 section 4.1) under one fresh `Sec-WebSocket-Key`.
pub struct ClientHandshake {
    key: String,
    protocol: Option<String>,
}

impl ClientHandshake {
    /// A handshake asking for `protocol` (`Sec-WebSocket-Protocol`) when given.
    pub fn new(protocol: Option<&str>) -> Self {
        Self {
            key: jsrt::base64::encode(jsrt::random::bytes::<16>()),
            protocol: protocol.map(str::to_string),
        }
    }

    /// The upgrade request's own headers, sent after the caller's.
    pub fn request_headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("connection", "upgrade".to_string()),
            ("upgrade", "websocket".to_string()),
            ("sec-websocket-key", self.key.clone()),
            ("sec-websocket-version", "13".to_string()),
        ];
        if let Some(protocol) = &self.protocol {
            headers.push(("sec-websocket-protocol", protocol.clone()));
        }
        headers
    }

    /// Whether the response completes the handshake: `101`, this key's `Sec-WebSocket-Accept`, no extension, and
    /// exactly the subprotocol asked for (none when none was). `header` reads a response header by its lowercase name.
    pub fn accepted(&self, status: u16, header: &dyn Fn(&str) -> Option<String>) -> bool {
        status == 101
            && header("sec-websocket-accept").as_deref() == Some(accept_key(&self.key).as_str())
            && header("sec-websocket-extensions").is_none()
            && header("sec-websocket-protocol") == self.protocol
    }
}

/// `Sec-WebSocket-Accept` for a `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}{ACCEPT_GUID}").as_bytes(),
    );
    jsrt::base64::encode(digest.as_ref())
}
