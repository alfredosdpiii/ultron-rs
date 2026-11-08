//! The client side of the handshake (packages/client connection.ts `#handleMessage` while connecting): the client
//! sends `hello`; the server's first message must be its `hello` for the expected server ID, or a `hello_error`.

use crate::framing::{DEFAULT_MAX_FRAME_LENGTH, MAX_SERVER_FRAME_LENGTH};
use crate::message::{
    ClientMessage, PROTOCOL_VERSION, ProtocolError, ServerMessage, ServerMessageDecoder, encode_client_message,
};
use std::fmt;
use std::io::{self, ErrorKind, Read, Write};

#[derive(Debug)]
pub enum HelloError {
    /// The server's `hello_error`.
    Rejected(ProtocolError),
    /// The peer broke the protocol (`ProtocolValidationError`).
    Invalid(String),
    /// The transport ended before the server's hello.
    Closed,
    Io(io::Error),
}

impl fmt::Display for HelloError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HelloError::Rejected(error) => f.write_str(&error.message),
            HelloError::Invalid(message) => f.write_str(message),
            HelloError::Closed => f.write_str("Byte transport closed"),
            HelloError::Io(error) => error.fmt(f),
        }
    }
}

/// The server's messages after its hello: a connection decodes the rest of the stream with `decoder`.
pub struct Connected {
    pub decoder: ServerMessageDecoder,
    pub queued: Vec<ServerMessage>,
}

pub fn client_hello() -> Vec<u8> {
    encode_client_message(
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        },
        DEFAULT_MAX_FRAME_LENGTH,
    )
    .expect("a hello frame encodes")
}

/// Wait on `read` for the server's answer to a client hello already sent: its hello for `server_id`.
pub fn read_hello(read: &mut dyn Read, server_id: &str) -> Result<Connected, HelloError> {
    let mut decoder = ServerMessageDecoder::new(MAX_SERVER_FRAME_LENGTH).unwrap_or_default();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut queued = Vec::new();
    while queued.is_empty() {
        let n = match read.read(&mut buffer) {
            Ok(n) => n,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(HelloError::Io(error)),
        };
        if n == 0 {
            return Err(match decoder.end() {
                Err(error) => HelloError::Invalid(error.0),
                Ok(()) => HelloError::Closed,
            });
        }
        queued = decoder.push(&buffer[..n]).map_err(|e| HelloError::Invalid(e.0))?;
    }
    match queued.remove(0) {
        ServerMessage::HelloError { error } => Err(HelloError::Rejected(error)),
        ServerMessage::Hello { server_id: id, .. } if id == server_id => Ok(Connected { decoder, queued }),
        ServerMessage::Hello { server_id: id, .. } => Err(HelloError::Invalid(format!(
            "Connected server {id:?} does not match {server_id:?}"
        ))),
        _ => Err(HelloError::Invalid("Expected server hello as first message".into())),
    }
}

pub fn hello<S: Read + Write>(stream: &mut S, server_id: &str) -> Result<Connected, HelloError> {
    stream.write_all(&client_hello()).map_err(HelloError::Io)?;
    read_hello(stream, server_id)
}
