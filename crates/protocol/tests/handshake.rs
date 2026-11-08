//! The client handshake every client shares (`protocol::handshake`): the client and the probes of the server, the
//! CLI and the daemon. Its failures read as connection.ts's.

use protocol::handshake::{HelloError, hello};
use protocol::{DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ProtocolError, ServerMessage, encode_server_message};
use std::io::{Cursor, Read, Write};

const A: &str = "00000000-0000-4000-8000-00000000000a";
const B: &str = "00000000-0000-4000-8000-00000000000b";

/// A peer that answers with `input` and records what the client wrote.
struct Peer {
    input: Cursor<Vec<u8>>,
    written: Vec<u8>,
}

impl Read for Peer {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for Peer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn peer(messages: &[ServerMessage]) -> Peer {
    let input = messages
        .iter()
        .flat_map(|m| encode_server_message(m, DEFAULT_MAX_FRAME_LENGTH).unwrap())
        .collect();
    Peer {
        input: Cursor::new(input),
        written: Vec::new(),
    }
}

fn server_hello(id: &str) -> ServerMessage {
    ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        server_id: id.into(),
    }
}

#[test]
fn protocol_handshake_keeps_what_follows_the_hello() {
    let attachment = ServerMessage::Attachment { attachment: None };
    let mut stream = peer(&[server_hello(A), attachment.clone()]);
    let connected = hello(&mut stream, A).unwrap();
    assert_eq!(connected.queued, vec![attachment]);
    let sent = protocol::ClientMessageDecoder::default().push(&stream.written).unwrap();
    assert_eq!(
        sent,
        vec![protocol::ClientMessage::Hello {
            version: PROTOCOL_VERSION
        }]
    );
}

/// The runtime connection used to answer another server's hello with "Expected server hello as first message".
#[test]
fn protocol_handshake_failures_read_as_typescript() {
    let message = |messages: &[ServerMessage]| match hello(&mut peer(messages), A) {
        Err(error) => error.to_string(),
        Ok(_) => panic!("handshake succeeded"),
    };
    assert_eq!(
        message(&[server_hello(B)]),
        format!("Connected server {B:?} does not match {A:?}")
    );
    assert_eq!(
        message(&[ServerMessage::Attachment { attachment: None }]),
        "Expected server hello as first message"
    );
    assert_eq!(message(&[]), "Byte transport closed");
    let rejected = ServerMessage::HelloError {
        error: ProtocolError::new("version", "Unsupported protocol version"),
    };
    assert!(matches!(hello(&mut peer(&[rejected]), A), Err(HelloError::Rejected(e)) if e.code == "version"));
}
