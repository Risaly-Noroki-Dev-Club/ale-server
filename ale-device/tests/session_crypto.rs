//! Cryptographic framing tests for the terminal secure channel.
//!
//! These tests pair a server-side [`ale_device::crypto::SecureChannel`] with an
//! independently implemented terminal-side initiator (`tests/session_support`),
//! so they check the real wire behaviour: Noise NNpsk0, JSON messages, `AME2`
//! chunking, reassembly strictness and the pairing PSK derivation.

#[path = "session_support/mod.rs"]
mod support;

use ale_device::crypto::{
    constant_time_eq, pairing_psk, psk_fingerprint, random_pairing_code, random_token,
    SecureChannel, MAX_ENCRYPTED_FRAME_BYTES, MAX_NOISE_PLAINTEXT,
};
use ale_device::protocol::{DeviceMessage, SessionRejected};
use support::*;

/// Split a JSON payload into AME2 chunk payloads.
fn chunk_payloads(payload: &[u8]) -> Vec<&[u8]> {
    payload.chunks(AME2_CHUNK_PAYLOAD).collect()
}

fn large_server_message() -> DeviceMessage {
    DeviceMessage::SessionRejected(SessionRejected {
        code: "TEST_LARGE".to_string(),
        message: "s".repeat(200 * 1024),
    })
}

fn large_client_message() -> DeviceMessage {
    DeviceMessage::Error {
        request_id: Some("large".to_string()),
        code: "TEST_LARGE".to_string(),
        message: "c".repeat(200 * 1024),
    }
}

#[test]
fn noise_plaintext_limits_match_the_ame2_contract() {
    assert_eq!(MAX_NOISE_PLAINTEXT, 48 * 1024);
    assert_eq!(MAX_ENCRYPTED_FRAME_BYTES, MAX_NOISE_PLAINTEXT + 16);
}

#[test]
fn server_encrypts_and_terminal_reassembles_large_message() {
    let (mut server, mut client) = secure_pair("ABCD1234");
    let frames = server.encrypt(&large_server_message()).expect("encrypt");
    assert!(
        frames.len() >= 4,
        "200 KiB needs several chunks, got {} frames",
        frames.len()
    );
    for frame in &frames {
        assert!(frame.len() <= MAX_ENCRYPTED_FRAME_BYTES);
    }

    let mut decoded = None;
    for (index, frame) in frames.iter().enumerate() {
        let next = client.recv(frame).expect("terminal decrypts frame");
        if index + 1 < frames.len() {
            assert!(
                next.is_none(),
                "message must only decode after the last chunk"
            );
        }
        decoded = next.or(decoded);
    }
    match decoded.expect("decoded message") {
        DeviceMessage::SessionRejected(rejected) => {
            assert_eq!(rejected.code, "TEST_LARGE");
            assert_eq!(rejected.message.len(), 200 * 1024);
        }
        other => panic!("expected SessionRejected, got {other:?}"),
    }
}

#[test]
fn server_reassembles_large_terminal_message() {
    let (mut server, mut client) = secure_pair("EFGH5678");
    let frames = client
        .send(&large_client_message())
        .expect("terminal encrypt");
    assert!(frames.len() >= 4);

    let mut decoded = None;
    for frame in &frames {
        let next = server
            .decrypt(frame, TEST_MAX_MESSAGE_BYTES)
            .expect("server decrypts frame");
        decoded = next.or(decoded);
    }
    match decoded.expect("decoded message") {
        DeviceMessage::Error { code, message, .. } => {
            assert_eq!(code, "TEST_LARGE");
            assert_eq!(message.len(), 200 * 1024);
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn small_messages_fit_one_encrypted_frame() {
    let (mut server, _client) = secure_pair("IJKL9012");
    let frames = server
        .encrypt(&DeviceMessage::SessionRejected(SessionRejected {
            code: "SMALL".to_string(),
            message: "hello".to_string(),
        }))
        .expect("encrypt");
    assert_eq!(frames.len(), 1);
    assert!(frames[0].len() <= MAX_ENCRYPTED_FRAME_BYTES);
}

#[test]
fn rejects_reassembled_message_over_configured_limit() {
    let (mut server, mut client) = secure_pair("MNOP3456");
    let frames = client
        .send(&large_client_message())
        .expect("terminal encrypt");
    let mut error = None;
    for frame in &frames {
        match server.decrypt(frame, 64 * 1024) {
            Ok(Some(_)) => panic!("message over the limit must not decode"),
            Ok(None) => {}
            Err(next) => {
                error = Some(next.to_string());
                break;
            }
        }
    }
    let error = error.expect("expected a size violation");
    assert!(
        error.contains("MESSAGE_TOO_LARGE"),
        "unexpected error: {error}"
    );
}

#[test]
fn rejects_chunk_that_does_not_start_a_message() {
    let (mut server, mut client) = secure_pair("QRST7890");
    let payload = json_payload(&large_client_message());
    let chunks = chunk_payloads(&payload);
    assert!(chunks.len() >= 3);
    let total = chunks.len() as u32;

    // Deliver chunk 1 as the first frame of the channel (nonce order is kept;
    // only the AME2 sequence is hostile).
    let orphan = client
        .send_chunk(7, 1, total, chunks[1])
        .expect("encrypt orphan chunk");
    let error = server
        .decrypt(&orphan, TEST_MAX_MESSAGE_BYTES)
        .expect_err("chunk 1 before chunk 0 must be rejected");
    assert!(error.to_string().contains("MISSING_CHUNK_START"));

    // After the violation the channel recovers and accepts a fresh start.
    let start = client
        .send_chunk(8, 0, total, chunks[0])
        .expect("encrypt chunk 0");
    assert!(server
        .decrypt(&start, TEST_MAX_MESSAGE_BYTES)
        .expect("a fresh chunk 0 is accepted")
        .is_none());
}

#[test]
fn rejects_out_of_order_chunks() {
    let (mut server, mut client) = secure_pair("UVWX1234");
    let payload = json_payload(&large_client_message());
    let chunks = chunk_payloads(&payload);
    assert!(chunks.len() >= 4);
    let total = chunks.len() as u32;

    let first = client
        .send_chunk(11, 0, total, chunks[0])
        .expect("encrypt chunk 0");
    assert!(server
        .decrypt(&first, TEST_MAX_MESSAGE_BYTES)
        .expect("chunk 0 accepted")
        .is_none());

    // Skipping chunk 1 must be a hard protocol violation, even though the
    // Noise nonce order itself is intact.
    let skipped = client
        .send_chunk(11, 2, total, chunks[2])
        .expect("encrypt chunk 2");
    let error = server
        .decrypt(&skipped, TEST_MAX_MESSAGE_BYTES)
        .expect_err("out-of-order chunk must be rejected");
    assert!(error.to_string().contains("INVALID_CHUNK_SEQUENCE"));
}

#[test]
fn rejects_interleaved_chunked_messages() {
    let (mut server, mut client) = secure_pair("YZAB5678");
    let payload = json_payload(&large_client_message());
    let chunks = chunk_payloads(&payload);
    assert!(chunks.len() >= 2);
    let total = chunks.len() as u32;

    let first_a = client
        .send_chunk(21, 0, total, chunks[0])
        .expect("encrypt message A chunk 0");
    let first_b = client
        .send_chunk(22, 0, total, chunks[0])
        .expect("encrypt message B chunk 0");

    assert!(server
        .decrypt(&first_a, TEST_MAX_MESSAGE_BYTES)
        .expect("message A chunk 0 accepted")
        .is_none());
    let error = server
        .decrypt(&first_b, TEST_MAX_MESSAGE_BYTES)
        .expect_err("interleaved chunked message must be rejected");
    assert!(error.to_string().contains("INVALID_CHUNK_SEQUENCE"));
}

#[test]
fn rejects_plain_message_while_chunked_message_is_in_flight() {
    let (mut server, mut client) = secure_pair("CDEF9012");
    let payload = json_payload(&large_client_message());
    let chunks = chunk_payloads(&payload);
    let total = chunks.len() as u32;
    let first = client
        .send_chunk(31, 0, total, chunks[0])
        .expect("encrypt chunk 0");
    assert!(server
        .decrypt(&first, TEST_MAX_MESSAGE_BYTES)
        .expect("chunk 0 accepted")
        .is_none());

    let small = client
        .send_plain(&DeviceMessage::Error {
            request_id: None,
            code: "INTERLEAVED".to_string(),
            message: "small".to_string(),
        })
        .expect("encrypt small message");
    let error = server
        .decrypt(&small, TEST_MAX_MESSAGE_BYTES)
        .expect_err("single-frame message must not interleave a chunked one");
    assert!(error.to_string().contains("INVALID_CHUNK_SEQUENCE"));
}

#[test]
fn rejects_tampered_frames() {
    let (mut server, mut client) = secure_pair("GHIJ3456");
    let frames = client
        .send(&large_client_message())
        .expect("terminal encrypt");
    assert!(frames.len() >= 4);
    assert!(server
        .decrypt(&frames[0], TEST_MAX_MESSAGE_BYTES)
        .expect("chunk 0 accepted")
        .is_none());
    assert!(server
        .decrypt(&frames[1], TEST_MAX_MESSAGE_BYTES)
        .expect("chunk 1 accepted")
        .is_none());

    let mut tampered = frames[2].clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    let error = server
        .decrypt(&tampered, TEST_MAX_MESSAGE_BYTES)
        .expect_err("tampered ciphertext must fail authentication");
    assert!(
        error.to_string().contains("noise"),
        "expected a Noise authentication error, got: {error}"
    );
}

#[test]
fn rejects_oversized_encrypted_frame_before_decrypting() {
    let (mut server, _client) = secure_pair("KLMN7890");
    let oversized = vec![0u8; MAX_ENCRYPTED_FRAME_BYTES + 1];
    let error = server
        .decrypt(&oversized, TEST_MAX_MESSAGE_BYTES)
        .expect_err("oversized frame must be rejected");
    assert!(error.to_string().contains("MESSAGE_TOO_LARGE"));
}

#[test]
fn wrong_pairing_code_fails_the_noise_handshake() {
    let (handshake, first) = ClientHandshake::start("ABCD1234");
    let error = SecureChannel::server_respond(&pairing_psk("WXYZ6789"), &first)
        .err()
        .expect("a terminal with the wrong pairing code must not complete the handshake");
    assert!(
        error.to_string().contains("noise"),
        "expected a Noise error, got: {error}"
    );
    // The initiator state is not needed after the failure, but make sure the
    // helper type is not optimised away silently.
    let _ = handshake;
}

#[test]
fn pairing_psk_is_hkdf_sha256_of_the_code() {
    // Independently computed with Python: HKDF-SHA256(salt="ale-device-pairing-v1",
    // ikm="ABCD1234", info="", L=32).
    assert_eq!(
        hex_encode(&pairing_psk("ABCD1234")),
        "b7cda0a88ab50b5162443e34b36d312f0cb1bf05d4243264f6b3027b7e6dce2e"
    );
    assert_ne!(pairing_psk("ABCD1234"), pairing_psk("ABCD1235"));
}

#[test]
fn psk_fingerprint_is_short_hex_and_stable() {
    let psk = pairing_psk("ABCD1234");
    let fingerprint = psk_fingerprint(&psk);
    assert_eq!(fingerprint, "234a1a29");
    assert_eq!(fingerprint.len(), 8);
    assert!(fingerprint
        .chars()
        .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()));
    assert_eq!(fingerprint, psk_fingerprint(&psk), "deterministic");
    assert_ne!(fingerprint, psk_fingerprint(&[0u8; 32]));
}

#[test]
fn random_pairing_codes_are_crockford_base32() {
    const ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut seen = std::collections::HashSet::new();
    for _ in 0..200 {
        let code = random_pairing_code();
        assert_eq!(code.len(), 8, "code must be 8 characters: {code}");
        assert!(
            code.chars().all(|character| ALPHABET.contains(character)),
            "code is not Crockford base32: {code}"
        );
        for ambiguous in ['I', 'L', 'O', 'U'] {
            assert!(
                !code.contains(ambiguous),
                "code {code} contains ambiguous character {ambiguous}"
            );
        }
        seen.insert(code);
    }
    assert!(
        seen.len() > 150,
        "random codes should be unique, saw {} distinct",
        seen.len()
    );
}

#[test]
fn random_tokens_are_urlsafe_base64() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let token = random_token();
        // 32 random bytes -> 43 base64url characters without padding.
        assert_eq!(token.len(), 43, "unexpected token length: {token}");
        assert!(
            token
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '-'
                    || character == '_'),
            "token is not URL-safe base64: {token}"
        );
        assert!(!token.contains('='), "token must not be padded");
        seen.insert(token);
    }
    assert_eq!(seen.len(), 100, "tokens must be unique");
}

#[test]
fn constant_time_eq_compares_slices() {
    assert!(constant_time_eq(b"secret", b"secret"));
    assert!(!constant_time_eq(b"secret", b"secreT"));
    assert!(!constant_time_eq(b"secret", b"secret "));
    assert!(!constant_time_eq(b"", b"x"));
    assert!(constant_time_eq(b"", b""));
}
