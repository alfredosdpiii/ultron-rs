//! RFC 6455 framing with the RFC's own literal examples (section 1.3 handshake, section 5.7 frames), plus the
//! decoder's protocol checks.

use websocket::{ClientHandshake, Message, MessageDecoder, accept_key, encode_frame, encode_message};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn websocket_accept_key_is_the_rfc_example() {
    assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

#[test]
fn websocket_frames_are_the_rfc_examples() {
    assert_eq!(
        hex(&encode_message(&Message::Text("Hello".into()), None)),
        "810548656c6c6f"
    );
    assert_eq!(
        hex(&encode_message(
            &Message::Text("Hello".into()),
            Some([0x37, 0xfa, 0x21, 0x3d])
        )),
        "818537fa213d7f9f4d5158"
    );
    assert_eq!(
        hex(&encode_message(&Message::Ping(b"Hello".to_vec()), None)),
        "890548656c6c6f"
    );
    assert_eq!(&encode_frame(0x2, &[0; 256], None)[..4], &[0x82, 0x7e, 0x01, 0x00]);
    assert_eq!(
        &encode_frame(0x2, &[0; 65536], None)[..10],
        &[0x82, 0x7f, 0, 0, 0, 0, 0, 1, 0, 0]
    );
    assert_eq!(
        hex(&encode_message(&Message::Close(Some(1000), "bye".into()), None)),
        "880503e8627965"
    );
    assert_eq!(hex(&encode_message(&Message::Close(None, String::new()), None)), "8800");
}

#[test]
fn websocket_decoder_reassembles_and_unmasks() {
    let mut client = MessageDecoder::client(1 << 20);
    // A fragmented unmasked text message ("Hel" + "lo"), split mid-frame, with a ping between the fragments.
    let bytes = [
        0x01, 0x03, 0x48, 0x65, 0x6c, 0x89, 0x00, 0x80, 0x02, 0x6c, 0x6f, 0x88, 0x02, 0x03, 0xe8,
    ];
    assert_eq!(client.push(&bytes[..4]).unwrap(), vec![]);
    assert_eq!(
        client.push(&bytes[4..]).unwrap(),
        vec![
            Message::Ping(vec![]),
            Message::Text("Hello".into()),
            Message::Close(Some(1000), String::new()),
        ]
    );
    let mut server = MessageDecoder::server(1 << 20);
    assert_eq!(
        server
            .push(&encode_message(&Message::Binary(vec![1, 2, 3]), Some([9, 8, 7, 6])))
            .unwrap(),
        vec![Message::Binary(vec![1, 2, 3])]
    );
}

#[test]
fn websocket_decoder_rejects_protocol_violations() {
    let push = |mut decoder: MessageDecoder, bytes: &[u8]| decoder.push(bytes).unwrap_err();
    // A server must not mask; a client must.
    assert!(
        push(
            MessageDecoder::client(64),
            &encode_message(&Message::Text("x".into()), Some([1, 2, 3, 4]))
        )
        .contains("mask")
    );
    assert!(
        push(
            MessageDecoder::server(64),
            &encode_message(&Message::Text("x".into()), None)
        )
        .contains("mask")
    );
    assert_eq!(
        push(MessageDecoder::client(64), &[0xc1, 0x00]),
        "Reserved bits must be clear"
    );
    assert_eq!(push(MessageDecoder::client(64), &[0x83, 0x00]), "Unknown opcode 3");
    assert_eq!(
        push(MessageDecoder::client(64), &[0x80, 0x00]),
        "Unexpected continuation frame"
    );
    assert_eq!(
        push(MessageDecoder::client(64), &[0x01, 0x00, 0x81, 0x00]),
        "Expected a continuation frame"
    );
    assert_eq!(push(MessageDecoder::client(64), &[0x09, 0x00]), "Invalid control frame");
    assert_eq!(
        push(MessageDecoder::client(4), &[0x82, 0x05, 1, 2, 3, 4, 5]),
        "Message too large"
    );
    assert_eq!(
        push(MessageDecoder::client(64), &[0x88, 0x01, 0x03]),
        "Invalid close payload"
    );
}

/// A response completes a client handshake only with 101, the key's accept value, no extension and exactly the
/// subprotocol asked for.
#[test]
fn websocket_client_handshake_checks_the_upgrade_response() {
    for protocol in [None, Some("relay.v1")] {
        let handshake = ClientHandshake::new(protocol);
        let headers = handshake.request_headers();
        let key = headers
            .iter()
            .find(|(name, _)| *name == "sec-websocket-key")
            .unwrap()
            .1
            .clone();
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| *name == "sec-websocket-protocol")
                .map(|(_, v)| v.as_str()),
            protocol
        );
        let respond = |status: u16, extra: &[(&str, &str)]| {
            let mut answer: Vec<(String, String)> = vec![("sec-websocket-accept".into(), accept_key(&key))];
            if let Some(protocol) = protocol {
                answer.push(("sec-websocket-protocol".into(), protocol.into()));
            }
            for (name, value) in extra {
                answer.retain(|(n, _)| n != name);
                answer.push((name.to_string(), value.to_string()));
            }
            handshake.accepted(status, &|name| {
                answer
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
                    .filter(|v| !v.is_empty())
            })
        };
        assert!(respond(101, &[]));
        assert!(!respond(200, &[]));
        assert!(!respond(101, &[("sec-websocket-accept", "wrong")]));
        assert!(!respond(101, &[("sec-websocket-extensions", "permessage-deflate")]));
        let other = if protocol.is_some() { "" } else { "relay.v1" };
        assert!(!respond(101, &[("sec-websocket-protocol", other)]));
    }
}
