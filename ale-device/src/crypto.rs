//! Noise-based secure channel and pairing key derivation.
//!
//! Owner: session teammate. Ported from the desktop GUI's `remote_crypto.rs`
//! (Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s, `AME2` chunking) and adapted for the
//! terminal protocol. Note that the vendored upstream `ale_core::remote` is now
//! Misaka-Network v4 and uses `MSKN` chunk framing; this terminal protocol keeps
//! `AME2` and is intentionally not wire-compatible with it (see
//! `docs/PROTOCOL.md`).
//!
//! Pairing model:
//! * the service prints an 8-character base32 pairing code (`pairing::code()`);
//! * the terminal derives `psk = HKDF-SHA256("ale-device-pairing-v1", code)`;
//! * the code expires after `pairing.code_ttl_s` and failed handshakes are
//!   counted per client address.
//!
//! The code is short-lived and high-entropy enough (40 bits) that an offline
//! brute force against a captured handshake is infeasible inside the TTL. The
//! pairing handshake itself is authenticated by the PSK: a terminal that used
//! the wrong code fails during the first Noise message.
//!
//! Transport framing:
//! * one Noise transport message per WebSocket binary frame;
//! * up to [`MAX_NOISE_PLAINTEXT`] bytes of JSON fit in one frame;
//! * larger payloads are split into `AME2` chunks
//!   `magic(4) | message_id(8 BE) | index(4 BE) | total(4 BE) | payload`
//!   with at most `MAX_NOISE_PLAINTEXT - 20` payload bytes per chunk;
//! * the receiver rejects out-of-order, interleaved, oversized and
//!   sequence-corrupt messages.

use crate::error::{DeviceError, Result};
use crate::protocol::DeviceMessage;
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};

const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const CHUNK_MAGIC: &[u8; 4] = b"AME2";
const CHUNK_HEADER_LEN: usize = 20;
/// Domain-separation salt for the pairing PSK derivation.
const PAIRING_SALT: &[u8] = b"ale-device-pairing-v1";
/// Crockford base32: excludes `I`, `L`, `O` and `U` (ambiguous or easily
/// confused with `1`/`0`).
const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Pairing codes carry 40 random bits (8 characters x 5 bits).
const PAIRING_CODE_CHARS: usize = 8;
/// The first Noise handshake message is tiny; never buffer more than this.
const MAX_HANDSHAKE_BYTES: usize = 4 * 1024;
/// Hard protocol ceiling for one reassembled plaintext message.
const MAX_SECURE_MESSAGE_BYTES: usize = crate::protocol::MAX_MESSAGE_BYTES;
const TOKEN_BYTES: usize = 32;
const AES_TAG_BYTES: usize = 16;

/// Largest plaintext that fits one Noise transport message.
pub const MAX_NOISE_PLAINTEXT: usize = 48 * 1024;
/// Largest encrypted frame accepted from the network.
pub const MAX_ENCRYPTED_FRAME_BYTES: usize = MAX_NOISE_PLAINTEXT + AES_TAG_BYTES;

pub struct SecureChannel {
    transport: snow::TransportState,
    incoming: Option<IncomingMessage>,
}

struct IncomingMessage {
    id: u64,
    total_chunks: u32,
    next_chunk: u32,
    payload: Vec<u8>,
}

impl SecureChannel {
    /// Complete the responder side of the Noise handshake. `client_message` is
    /// the first handshake message received from the terminal.
    pub fn server_respond(psk: &[u8; 32], client_message: &[u8]) -> Result<(Self, Vec<u8>)> {
        if client_message.is_empty() || client_message.len() > MAX_HANDSHAKE_BYTES {
            return Err(protocol_violation(
                "INVALID_HANDSHAKE: bad handshake length",
            ));
        }
        let params = noise_params()?;
        let mut handshake = snow::Builder::new(params).psk(0, psk).build_responder()?;
        let mut scratch = vec![0u8; MAX_NOISE_PLAINTEXT];
        handshake.read_message(client_message, &mut scratch)?;
        let mut reply = vec![0u8; 256];
        let len = handshake.write_message(&[], &mut reply)?;
        reply.truncate(len);
        let transport = handshake.into_transport_mode()?;
        Ok((
            Self {
                transport,
                incoming: None,
            },
            reply,
        ))
    }

    /// Encrypt one message into one or more WebSocket binary frames.
    pub fn encrypt(&mut self, message: &DeviceMessage) -> Result<Vec<Vec<u8>>> {
        let payload = serde_json::to_vec(message)?;
        if payload.len() > MAX_SECURE_MESSAGE_BYTES {
            return Err(message_too_large(payload.len(), MAX_SECURE_MESSAGE_BYTES));
        }
        if payload.len() <= MAX_NOISE_PLAINTEXT {
            return Ok(vec![self.encrypt_payload(&payload)?]);
        }

        let chunk_payload_size = MAX_NOISE_PLAINTEXT - CHUNK_HEADER_LEN;
        let total_chunks = payload.len().div_ceil(chunk_payload_size);
        let total_chunks = u32::try_from(total_chunks)
            .map_err(|_| protocol_violation("MESSAGE_TOO_LARGE: too many chunks"))?;
        let message_id = rand::random::<u64>();
        let mut frames = Vec::with_capacity(total_chunks as usize);
        for (index, chunk) in payload.chunks(chunk_payload_size).enumerate() {
            let mut framed = Vec::with_capacity(CHUNK_HEADER_LEN + chunk.len());
            framed.extend_from_slice(CHUNK_MAGIC);
            framed.extend_from_slice(&message_id.to_be_bytes());
            framed.extend_from_slice(&(index as u32).to_be_bytes());
            framed.extend_from_slice(&total_chunks.to_be_bytes());
            framed.extend_from_slice(chunk);
            frames.push(self.encrypt_payload(&framed)?);
        }
        Ok(frames)
    }

    fn encrypt_payload(&mut self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; payload.len() + AES_TAG_BYTES + 64];
        let len = self.transport.write_message(payload, &mut out)?;
        out.truncate(len);
        Ok(out)
    }

    /// Decrypt one frame. Returns `Ok(None)` while a chunked message is still
    /// incomplete, and `Err` on protocol violations.
    pub fn decrypt(
        &mut self,
        frame: &[u8],
        max_message_bytes: usize,
    ) -> Result<Option<DeviceMessage>> {
        if frame.len() > MAX_ENCRYPTED_FRAME_BYTES {
            return Err(message_too_large(frame.len(), MAX_ENCRYPTED_FRAME_BYTES));
        }
        let limit = max_message_bytes.clamp(CHUNK_HEADER_LEN, MAX_SECURE_MESSAGE_BYTES);
        let mut out = vec![0u8; frame.len() + AES_TAG_BYTES + 64];
        let len = self.transport.read_message(frame, &mut out)?;
        out.truncate(len);

        if !out.starts_with(CHUNK_MAGIC) {
            // A plain, single-frame message while a chunked message is in
            // flight is an interleaving violation.
            if self.incoming.take().is_some() {
                return Err(protocol_violation(
                    "INVALID_CHUNK_SEQUENCE: interleaved single-frame message",
                ));
            }
            if out.len() > limit {
                return Err(message_too_large(out.len(), limit));
            }
            return decode_message(&out);
        }

        if out.len() < CHUNK_HEADER_LEN {
            self.incoming = None;
            return Err(protocol_violation("INVALID_CHUNK_HEADER: truncated chunk"));
        }
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&out[4..12]);
        let id = u64::from_be_bytes(id_bytes);
        let mut index_bytes = [0u8; 4];
        index_bytes.copy_from_slice(&out[12..16]);
        let index = u32::from_be_bytes(index_bytes);
        let mut total_bytes = [0u8; 4];
        total_bytes.copy_from_slice(&out[16..20]);
        let total_chunks = u32::from_be_bytes(total_bytes);

        if total_chunks == 0 || index >= total_chunks {
            self.incoming = None;
            return Err(protocol_violation(
                "INVALID_CHUNK_SEQUENCE: bad chunk index or count",
            ));
        }

        if index == 0 {
            if self.incoming.is_some() {
                self.incoming = None;
                return Err(protocol_violation(
                    "INVALID_CHUNK_SEQUENCE: interleaved chunked message",
                ));
            }
            self.incoming = Some(IncomingMessage {
                id,
                total_chunks,
                next_chunk: 0,
                payload: Vec::new(),
            });
        }
        let Some(incoming) = self.incoming.as_mut() else {
            return Err(protocol_violation("MISSING_CHUNK_START"));
        };
        if incoming.id != id
            || incoming.total_chunks != total_chunks
            || incoming.next_chunk != index
        {
            self.incoming = None;
            return Err(protocol_violation(
                "INVALID_CHUNK_SEQUENCE: out-of-order chunk",
            ));
        }
        let chunk_len = out.len() - CHUNK_HEADER_LEN;
        let next_len = incoming.payload.len().saturating_add(chunk_len);
        if next_len > limit {
            self.incoming = None;
            return Err(message_too_large(next_len, limit));
        }
        incoming.payload.extend_from_slice(&out[CHUNK_HEADER_LEN..]);
        incoming.next_chunk += 1;

        if incoming.next_chunk != incoming.total_chunks {
            return Ok(None);
        }
        let payload = self
            .incoming
            .take()
            .ok_or_else(|| protocol_violation("MISSING_CHUNK_START"))?
            .payload;
        decode_message(&payload)
    }
}

fn noise_params() -> Result<snow::params::NoiseParams> {
    NOISE_PATTERN.parse().map_err(|error: snow::Error| {
        DeviceError::Protocol(format!("invalid noise params: {error}"))
    })
}

fn decode_message(payload: &[u8]) -> Result<Option<DeviceMessage>> {
    let message: DeviceMessage = serde_json::from_slice(payload)
        .map_err(|error| DeviceError::Protocol(format!("INVALID_MESSAGE: {error}")))?;
    Ok(Some(message))
}

fn protocol_violation(detail: &str) -> DeviceError {
    DeviceError::Protocol(detail.to_owned())
}

fn message_too_large(actual: usize, limit: usize) -> DeviceError {
    DeviceError::Protocol(format!("MESSAGE_TOO_LARGE: {actual} bytes exceeds {limit}"))
}

/// Derive the pairing PSK from a pairing code.
pub fn pairing_psk(code: &str) -> [u8; 32] {
    let hkdf = Hkdf::<Sha256>::new(Some(PAIRING_SALT), code.as_bytes());
    let mut psk = [0u8; 32];
    if hkdf.expand(&[], &mut psk).is_err() {
        // HKDF-Expand for a 32-byte output over HMAC-SHA256 cannot fail; if it
        // ever does we return an all-zero PSK rather than panicking.
        tracing::error!("HKDF expansion for the pairing PSK failed");
    }
    psk
}

/// Derive a short, non-reversible fingerprint of the PSK for audit logs.
pub fn psk_fingerprint(psk: &[u8; 32]) -> String {
    let digest = Sha256::digest(psk);
    let mut fingerprint = String::with_capacity(8);
    for byte in &digest[..4] {
        use std::fmt::Write as _;
        let _ = write!(fingerprint, "{byte:02x}");
    }
    fingerprint
}

/// Random pairing code: 8 characters of Crockford base32 (40 bits).
pub fn random_pairing_code() -> String {
    let mut entropy = [0u8; 5];
    rand::rngs::OsRng.fill_bytes(&mut entropy);
    let value = u64::from_be_bytes([
        0, 0, 0, entropy[0], entropy[1], entropy[2], entropy[3], entropy[4],
    ]);
    let mut code = String::with_capacity(PAIRING_CODE_CHARS);
    for group in 0..PAIRING_CODE_CHARS {
        let shift = 5 * (PAIRING_CODE_CHARS - 1 - group);
        let index = ((value >> shift) & 0x1f) as usize;
        code.push(CROCKFORD_ALPHABET[index] as char);
    }
    code
}

/// Constant-time comparison for tokens/codes.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Generate an opaque session token for the HTTP egress API.
pub fn random_token() -> String {
    use base64::Engine as _;
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

impl From<snow::Error> for DeviceError {
    fn from(error: snow::Error) -> Self {
        DeviceError::Protocol(format!("noise: {error}"))
    }
}
