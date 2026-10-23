//! Shared helpers for the session / secure-channel integration tests.
//!
//! Included as a module from the `session_*.rs` test targets with
//! `#[path = "session_support/mod.rs"] mod support;`; it is not a test binary
//! itself.
//!
//! The terminal side of the protocol is implemented here independently from the
//! library (a snow initiator plus its own `AME2` chunking), so the tests exercise
//! the server the same way a real terminal would: over a WebSocket, with Noise
//! framing, and without touching library internals.

#![allow(dead_code)]

use ale_core::actions::{Action, ActionPlan, RiskLevel};
use ale_device::audit::AuditLog;
use ale_device::automation::AutomationService;
use ale_device::capture::CaptureService;
use ale_device::config::DeviceConfig;
use ale_device::crypto::{pairing_psk, SecureChannel};
use ale_device::protocol::{
    ClientKind, DeviceMessage, SessionInit, SessionReady, DEVICE_PROTOCOL_VERSION,
};
use ale_device::session::SessionService;
use futures_util::{SinkExt, StreamExt};
use snow::Builder;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

pub const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const CHUNK_MAGIC: &[u8; 4] = b"AME2";
const CHUNK_HEADER_LEN: usize = 20;
const MAX_NOISE_PLAINTEXT: usize = 48 * 1024;
/// Matches `limits.max_message_bytes` used by the test harness.
pub const TEST_MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Terminal-side Noise channel
// ---------------------------------------------------------------------------

pub fn noise_params() -> snow::params::NoiseParams {
    NOISE_PATTERN.parse().expect("static noise pattern parses")
}

fn encrypt_payload(
    transport: &mut snow::TransportState,
    payload: &[u8],
) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; payload.len() + 16 + 64];
    let len = transport
        .write_message(payload, &mut out)
        .map_err(|error| error.to_string())?;
    out.truncate(len);
    Ok(out)
}

fn decrypt_payload(transport: &mut snow::TransportState, frame: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = vec![0u8; frame.len() + 16 + 64];
    let len = transport
        .read_message(frame, &mut out)
        .map_err(|error| error.to_string())?;
    out.truncate(len);
    Ok(out)
}

/// Initiator side of the Noise NNpsk0 handshake.
pub struct ClientHandshake {
    initiator: snow::HandshakeState,
}

impl ClientHandshake {
    /// Build the initiator and return the first handshake message to send.
    pub fn start(code: &str) -> (Self, Vec<u8>) {
        let psk = pairing_psk(code);
        let mut initiator = Builder::new(noise_params())
            .psk(0, &psk)
            .build_initiator()
            .expect("build noise initiator");
        let mut first = vec![0u8; 1024];
        let len = initiator
            .write_message(&[], &mut first)
            .expect("write noise handshake message");
        first.truncate(len);
        (Self { initiator }, first)
    }

    pub fn finish(mut self, reply: &[u8]) -> Result<ClientChannel, String> {
        let mut scratch = vec![0u8; 1024];
        self.initiator
            .read_message(reply, &mut scratch)
            .map_err(|error| format!("noise handshake reply rejected: {error}"))?;
        let transport = self
            .initiator
            .into_transport_mode()
            .map_err(|error| error.to_string())?;
        Ok(ClientChannel {
            transport,
            incoming: None,
        })
    }
}

struct ClientIncoming {
    id: u64,
    total_chunks: u32,
    next_chunk: u32,
    payload: Vec<u8>,
}

/// Terminal-side encrypted channel: `AME2` framing over a Noise transport.
pub struct ClientChannel {
    transport: snow::TransportState,
    incoming: Option<ClientIncoming>,
}

/// Payload bytes that fit in one `AME2` chunk.
pub const AME2_CHUNK_PAYLOAD: usize = MAX_NOISE_PLAINTEXT - CHUNK_HEADER_LEN;

/// JSON payload of a message, before encryption.
pub fn json_payload(message: &DeviceMessage) -> Vec<u8> {
    serde_json::to_vec(message).expect("DeviceMessage serializes")
}

impl ClientChannel {
    /// Encrypt one `AME2` chunk with an explicit id/index/total. Tests use this
    /// to deliver chunks in a hostile order; a normal terminal never does.
    pub fn send_chunk(
        &mut self,
        message_id: u64,
        index: u32,
        total_chunks: u32,
        payload: &[u8],
    ) -> Result<Vec<u8>, String> {
        let mut framed = Vec::with_capacity(CHUNK_HEADER_LEN + payload.len());
        framed.extend_from_slice(CHUNK_MAGIC);
        framed.extend_from_slice(&message_id.to_be_bytes());
        framed.extend_from_slice(&index.to_be_bytes());
        framed.extend_from_slice(&total_chunks.to_be_bytes());
        framed.extend_from_slice(payload);
        encrypt_payload(&mut self.transport, &framed)
    }

    /// Encrypt one single-frame (unframed) message.
    pub fn send_plain(&mut self, message: &DeviceMessage) -> Result<Vec<u8>, String> {
        let payload = json_payload(message);
        encrypt_payload(&mut self.transport, &payload)
    }

    pub fn send(&mut self, message: &DeviceMessage) -> Result<Vec<Vec<u8>>, String> {
        let payload = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        if payload.len() <= MAX_NOISE_PLAINTEXT {
            return Ok(vec![encrypt_payload(&mut self.transport, &payload)?]);
        }
        let chunk_payload_size = MAX_NOISE_PLAINTEXT - CHUNK_HEADER_LEN;
        let total_chunks = payload.len().div_ceil(chunk_payload_size);
        let total_chunks = u32::try_from(total_chunks).map_err(|error| error.to_string())?;
        let message_id = rand::random::<u64>();
        let mut frames = Vec::with_capacity(total_chunks as usize);
        for (index, chunk) in payload.chunks(chunk_payload_size).enumerate() {
            let mut framed = Vec::with_capacity(CHUNK_HEADER_LEN + chunk.len());
            framed.extend_from_slice(CHUNK_MAGIC);
            framed.extend_from_slice(&message_id.to_be_bytes());
            framed.extend_from_slice(&(index as u32).to_be_bytes());
            framed.extend_from_slice(&total_chunks.to_be_bytes());
            framed.extend_from_slice(chunk);
            frames.push(encrypt_payload(&mut self.transport, &framed)?);
        }
        Ok(frames)
    }

    pub fn recv(&mut self, frame: &[u8]) -> Result<Option<DeviceMessage>, String> {
        let payload = decrypt_payload(&mut self.transport, frame)?;
        if !payload.starts_with(CHUNK_MAGIC) {
            if self.incoming.take().is_some() {
                return Err("interleaved message".into());
            }
            return serde_json::from_slice(&payload)
                .map(Some)
                .map_err(|error| error.to_string());
        }
        if payload.len() < CHUNK_HEADER_LEN {
            self.incoming = None;
            return Err("truncated chunk".into());
        }
        let message_id = u64::from_be_bytes(
            payload[4..12]
                .try_into()
                .map_err(|error| format!("bad message id: {error}"))?,
        );
        let index = u32::from_be_bytes(
            payload[12..16]
                .try_into()
                .map_err(|error| format!("bad chunk index: {error}"))?,
        );
        let total_chunks = u32::from_be_bytes(
            payload[16..20]
                .try_into()
                .map_err(|error| format!("bad chunk count: {error}"))?,
        );
        if index == 0 {
            self.incoming = Some(ClientIncoming {
                id: message_id,
                total_chunks,
                next_chunk: 0,
                payload: Vec::new(),
            });
        }
        let incoming = self
            .incoming
            .as_mut()
            .ok_or_else(|| "missing chunk start".to_string())?;
        if incoming.id != message_id
            || incoming.total_chunks != total_chunks
            || incoming.next_chunk != index
        {
            self.incoming = None;
            return Err("out-of-order chunk".into());
        }
        incoming
            .payload
            .extend_from_slice(&payload[CHUNK_HEADER_LEN..]);
        incoming.next_chunk += 1;
        if incoming.next_chunk != incoming.total_chunks {
            return Ok(None);
        }
        let payload = self
            .incoming
            .take()
            .ok_or_else(|| "missing chunk start".to_string())?
            .payload;
        serde_json::from_slice(&payload)
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

/// In-process server/client pair without a socket, for framing unit tests.
pub fn secure_pair(code: &str) -> (SecureChannel, ClientChannel) {
    let (handshake, first) = ClientHandshake::start(code);
    let psk = pairing_psk(code);
    let (server, reply) =
        SecureChannel::server_respond(&psk, &first).expect("server noise handshake");
    let client = handshake.finish(&reply).expect("client noise handshake");
    (server, client)
}

// ---------------------------------------------------------------------------
// Terminal-side WebSocket client
// ---------------------------------------------------------------------------

pub struct TestConnection {
    pub socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    pub channel: ClientChannel,
}

impl TestConnection {
    /// Connect, run the Noise handshake and return the ready channel.
    pub async fn start(addr: SocketAddr, code: &str) -> Result<Self, String> {
        let (mut socket, _) = connect_async(format!("ws://{addr}/"))
            .await
            .map_err(|error| format!("websocket connect failed: {error}"))?;
        let (handshake, first) = ClientHandshake::start(code);
        socket
            .send(Message::Binary(first))
            .await
            .map_err(|error| format!("failed to send handshake: {error}"))?;
        let reply = next_binary(&mut socket, Duration::from_secs(5))
            .await
            .map_err(|error| format!("no handshake reply: {error}"))?;
        let channel = handshake.finish(&reply)?;
        Ok(Self { socket, channel })
    }

    /// Complete a full pairing: Noise handshake + `SessionInit` -> `SessionReady`.
    pub async fn pair(addr: SocketAddr, code: &str) -> (Self, SessionReady) {
        let mut connection = Self::start(addr, code)
            .await
            .unwrap_or_else(|error| panic!("terminal handshake failed: {error}"));
        connection
            .send_session_init(DEVICE_PROTOCOL_VERSION)
            .await
            .expect("send SessionInit");
        match connection
            .next_message(Duration::from_secs(10))
            .await
            .expect("read SessionReady")
        {
            DeviceMessage::SessionReady(ready) => (connection, ready),
            other => panic!("expected SessionReady, got {other:?}"),
        }
    }

    pub async fn send_session_init(&mut self, protocol_version: u32) -> Result<(), String> {
        self.send(&DeviceMessage::SessionInit(SessionInit {
            protocol_version,
            device_name: "test-terminal".to_string(),
            client_kind: ClientKind::Terminal,
        }))
        .await
    }

    pub async fn send(&mut self, message: &DeviceMessage) -> Result<(), String> {
        for frame in self.channel.send(message)? {
            self.socket
                .send(Message::Binary(frame))
                .await
                .map_err(|error| format!("websocket send failed: {error}"))?;
        }
        Ok(())
    }

    pub async fn send_frame(&mut self, frame: Vec<u8>) -> Result<(), String> {
        self.socket
            .send(Message::Binary(frame))
            .await
            .map_err(|error| format!("websocket send failed: {error}"))
    }

    /// Read the next decrypted protocol message, skipping Noise chunks and
    /// WebSocket control frames.
    pub async fn next_message(&mut self, timeout: Duration) -> Result<DeviceMessage, String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let frame = self.next_raw(deadline).await?;
            match frame {
                Message::Binary(data) => {
                    if let Some(message) = self.channel.recv(&data)? {
                        return Ok(message);
                    }
                }
                Message::Close(frame) => {
                    return Err(format!("connection closed: {frame:?}"));
                }
                _ => {}
            }
        }
    }

    async fn next_raw(&mut self, deadline: tokio::time::Instant) -> Result<Message, String> {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timed out waiting for a server message".into());
        }
        tokio::time::timeout(remaining, self.socket.next())
            .await
            .map_err(|_| "timed out waiting for a server message".to_string())?
            .ok_or_else(|| "connection closed".to_string())?
            .map_err(|error| format!("websocket read failed: {error}"))
    }

    /// Wait for the server to close the connection and return its close reason.
    pub async fn expect_close(&mut self, timeout: Duration) -> Result<String, String> {
        read_close(&mut self.socket, timeout).await
    }
}

async fn next_binary(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let read = async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Binary(data))) => return Ok(data),
                Some(Ok(Message::Close(frame))) => {
                    return Err(format!("server closed: {frame:?}"));
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(format!("websocket error: {error}")),
                None => return Err("connection closed".to_string()),
            }
        }
    };
    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| "timed out waiting for handshake reply".to_string())?
}

/// Connect a raw WebSocket without running the Noise handshake.
pub async fn raw_socket(addr: SocketAddr) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let (socket, _) = connect_async(format!("ws://{addr}/"))
        .await
        .expect("raw websocket connect");
    socket
}

/// Read WebSocket frames until the server closes the connection and return the
/// close reason.
pub async fn read_close(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<String, String> {
    let read = async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(frame))) => {
                    return Ok(frame
                        .map(|frame| frame.reason.to_string())
                        .unwrap_or_default());
                }
                Some(Ok(_)) => continue,
                Some(Err(error)) => return Err(format!("websocket error: {error}")),
                None => return Err("connection closed without a close frame".to_string()),
            }
        }
    };
    tokio::time::timeout(timeout, read)
        .await
        .map_err(|_| "timed out waiting for close frame".to_string())?
}

// ---------------------------------------------------------------------------
// Server harness
// ---------------------------------------------------------------------------

pub struct ServiceHarness {
    pub service: Arc<SessionService>,
    pub addr: SocketAddr,
    pub shutdown: watch::Sender<bool>,
    pub server: tokio::task::JoinHandle<ale_device::Result<()>>,
    /// Keeps the fake backend script/fixture alive for the whole test.
    pub directory: Option<tempfile::TempDir>,
}

/// Device config tuned for tests: no real display server, short command
/// timeouts, a loopback listener.
/// Make display discovery deterministic and fast inside this test binary only.
///
/// The fake backends make real display probing unnecessary; pretending to be an
/// X11 session means the service probes for (absent) `xrandr`/`xdpyinfo` instead
/// of running the comparatively slow macOS `system_profiler`. Each integration
/// test binary is a separate process, so this cannot leak into other test
/// binaries, and [`Once`] keeps the write single-threaded.
pub fn pin_test_environment() {
    static ENV_INIT: std::sync::Once = std::sync::Once::new();
    ENV_INIT.call_once(|| {
        std::env::set_var("XDG_SESSION_TYPE", "x11");
    });
}

pub fn test_config() -> DeviceConfig {
    let mut config = DeviceConfig {
        bind: "127.0.0.1:0".to_string(),
        ..DeviceConfig::default()
    };
    config.capture.timeout_ms = 5_000;
    config.capture.max_width = 800;
    config.capture.cache_ttl_ms = 0;
    config.limits.idle_timeout_s = 30;
    config.limits.request_timeout_s = 5;
    config
}

pub fn build_service(config: DeviceConfig) -> Arc<SessionService> {
    pin_test_environment();
    let capture = Arc::new(CaptureService::new(config.capture.clone()));
    let automation = Arc::new(AutomationService::new(config.automation.clone()));
    let audit = Arc::new(AuditLog::new(512));
    Arc::new(SessionService::new(config, capture, automation, audit))
}

pub async fn start_service(config: DeviceConfig) -> ServiceHarness {
    let service = build_service(config);
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("listener address");
    let (shutdown, shutdown_rx) = watch::channel(false);
    let server_service = Arc::clone(&service);
    let server = tokio::spawn(async move { server_service.serve(listener, shutdown_rx).await });
    ServiceHarness {
        service,
        addr,
        shutdown,
        server,
        directory: None,
    }
}

/// Service whose capture backend is a script that prints a generated PNG.
pub async fn start_service_with_fake_capture() -> ServiceHarness {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let backend = write_fake_capture_backend(directory.path());
    let mut config = test_config();
    config.capture.preferred_backend = Some(backend.display().to_string());
    let mut harness = start_service(config).await;
    harness.directory = Some(directory);
    harness
}

/// Generate `frame.png` (800x600) and a `#!/bin/sh` backend that prints it.
pub fn write_fake_capture_backend(directory: &Path) -> PathBuf {
    let fixture = directory.join("frame.png");
    let image = image::RgbImage::from_fn(800, 600, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 32])
    });
    image.save(&fixture).expect("write PNG fixture");

    let script = directory.join("fake-capture");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ncat \"{}\"\n", fixture.display()),
    )
    .expect("write fake capture backend");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake capture backend");
    }
    script
}

/// A minimal, always-valid low-risk plan used by action-plan tests.
pub fn wait_plan(ms: u64) -> ActionPlan {
    ActionPlan {
        actions: vec![Action::Wait { ms }],
        risk_level: RiskLevel::Low,
        explanation: "integration test wait".to_string(),
        requires_confirmation: false,
    }
}

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}
