//! Encrypted terminal session server.
//!
//! Owner: session teammate.
//!
//! Responsibilities:
//! * accept WebSocket connections, run the Noise handshake with the current
//!   pairing code, reject expired/disabled pairing and back off after failures;
//! * per-connection request loop with an idle timeout and bounded concurrency;
//! * dispatch capture / display / action / health requests into `spawn_blocking`;
//! * per-request cancellation registry (`Cancel` message) and timeouts;
//! * frame freshness guard: reject `ActionPlanRequest` when `expected_frame_id`
//!   is not the most recent frame or is older than `frame_max_age_ms`;
//! * issue and validate egress bearer tokens (`validate_token`);
//! * record audit entries and bounded metrics for `Health`.
//!
//! The service must never panic on untrusted input and must survive `shutdown`.
//!
//! Connection lifecycle:
//!
//! 1. WebSocket upgrade (bounded frame size, 10 s timeout);
//! 2. pairing checks (enabled, per-IP backoff) — failures close the socket with a
//!    policy-violation close frame before the Noise handshake;
//! 3. first binary frame = Noise handshake message, answered with the responder
//!    handshake message (still unencrypted, this is the Noise handshake itself);
//! 4. encrypted `SessionInit`; protocol-version mismatch is answered with an
//!    encrypted `SessionRejected` and the socket is closed;
//! 5. encrypted `SessionReady` with a fresh session id + egress token; the
//!    pairing code is rotated so it can only be used once;
//! 6. request loop until idle timeout, protocol violation or shutdown.
//!
//! Shutdown: the accept loop stops, every connection is notified, all in-flight
//! `ExecutionControl`s are cancelled, responses are drained for a few seconds
//! and then the tasks are aborted.

use crate::audit::{AuditKind, AuditLog};
use crate::automation::{AutomationService, ExecutionControl};
use crate::capture::{now_unix_ms, CaptureRequest, CaptureService};
use crate::config::DeviceConfig;
use crate::crypto::{
    constant_time_eq, pairing_psk, psk_fingerprint, random_pairing_code, random_token,
    SecureChannel, MAX_ENCRYPTED_FRAME_BYTES,
};
use crate::error::{DeviceError, Result};
use crate::protocol::{
    error_code, DeviceCapabilities, DeviceMessage, ExecutionState, FrameMeta, HealthStatus,
    SessionReady, SessionRejected, DEVICE_PROTOCOL_VERSION, MAX_FRAME_BYTES, MAX_MESSAGE_BYTES,
};
use ale_core::actions::ActionPlan;
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_async_with_config, WebSocketStream};

/// Time allowed for the WebSocket upgrade, the Noise handshake and `SessionInit`.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-session request budget (requests per sliding window).
const RATE_LIMIT_REQUESTS: u32 = 30;
const RATE_LIMIT_WINDOW: Duration = Duration::from_secs(60);
/// Failed pairings are counted per client IP inside this window.
const PAIRING_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// Duration an address is blocked for once it exceeded `pairing.max_attempts`.
const PAIRING_BACKOFF: Duration = Duration::from_secs(60);
const MAX_TRACKED_PAIRING_CLIENTS: usize = 256;
/// Upper bound on issued egress tokens kept before the oldest is dropped.
const MAX_TRACKED_TOKENS: usize = 256;
/// How long graceful shutdown waits for connections to drain.
const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// A Noise handshake message from a terminal is a few hundred bytes at most.
const MAX_HANDSHAKE_FRAME_BYTES: usize = 4 * 1024;
const MAX_DEVICE_NAME_CHARS: usize = 128;
/// A message may not be larger than this even when config allows more; the
/// protocol constant hard-caps what one reassembled message can be.
const PROTOCOL_MESSAGE_LIMIT: usize = MAX_MESSAGE_BYTES;

type DeviceSocket = WebSocketStream<TcpStream>;
type CancelRegistry = Arc<Mutex<HashMap<String, ExecutionControl>>>;

/// Immutable description of one authenticated terminal session.
#[derive(Debug, Clone)]
pub struct SessionDescriptor {
    pub session_id: String,
    pub device_name: String,
    pub remote_addr: String,
    pub connected_at_unix_ms: u64,
}

struct PairingState {
    code: String,
    expires_at: Instant,
}

#[derive(Clone)]
struct TokenRecord {
    descriptor: SessionDescriptor,
    expires_at: Instant,
}

/// Pre-authentication per-IP connection tracking.
///
/// The Noise handshake is the attacker-controlled phase of a connection.
/// Without a per-IP cap a single peer could occupy every
/// `limits.max_connections` slot with unauthenticated sockets for up to
/// `HANDSHAKE_TIMEOUT`, denying pairing to legitimate terminals (SRV-05).
struct IpConnectionGuard {
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
}

impl Drop for IpConnectionGuard {
    fn drop(&mut self) {
        let mut counts = lock(&self.counts);
        let remove = match counts.get_mut(&self.ip) {
            Some(count) => {
                *count = count.saturating_sub(1);
                *count == 0
            }
            None => false,
        };
        if remove {
            counts.remove(&self.ip);
        }
    }
}

/// Reserve one pre-authentication slot for `ip`, or `None` when the per-IP cap
/// is reached.
fn try_register_ip_connection(
    counts: Arc<Mutex<HashMap<IpAddr, usize>>>,
    ip: IpAddr,
    max_per_ip: usize,
) -> Option<IpConnectionGuard> {
    {
        let mut guard = lock(&counts);
        let count = guard.entry(ip).or_insert(0);
        if *count >= max_per_ip.max(1) {
            return None;
        }
        *count += 1;
    }
    Some(IpConnectionGuard { counts, ip })
}

struct PairingFailure {
    failures: u32,
    window_started: Instant,
    blocked_until: Option<Instant>,
}

pub struct SessionService {
    config: DeviceConfig,
    capture: Arc<CaptureService>,
    automation: Arc<AutomationService>,
    audit: Arc<AuditLog>,
    started_at: Instant,
    pairing: Mutex<PairingState>,
    pairing_failures: Mutex<HashMap<IpAddr, PairingFailure>>,
    sessions: Mutex<HashMap<String, SessionDescriptor>>,
    tokens: Mutex<HashMap<String, TokenRecord>>,
}

impl SessionService {
    pub fn new(
        config: DeviceConfig,
        capture: Arc<CaptureService>,
        automation: Arc<AutomationService>,
        audit: Arc<AuditLog>,
    ) -> Self {
        let ttl = Duration::from_secs(config.pairing.code_ttl_s.max(1));
        Self {
            config,
            capture,
            automation,
            audit,
            started_at: Instant::now(),
            pairing: Mutex::new(PairingState {
                code: random_pairing_code(),
                expires_at: Instant::now() + ttl,
            }),
            pairing_failures: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &DeviceConfig {
        &self.config
    }

    pub fn capture(&self) -> &Arc<CaptureService> {
        &self.capture
    }

    pub fn automation(&self) -> &Arc<AutomationService> {
        &self.automation
    }

    pub fn audit(&self) -> &Arc<AuditLog> {
        &self.audit
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Current pairing code; rotates automatically when expired.
    pub fn pairing_code(&self) -> String {
        let mut state = lock(&self.pairing);
        self.refresh_pairing_state(&mut state);
        state.code.clone()
    }

    /// Force a new pairing code (used by the admin CLI).
    pub fn rotate_pairing_code(&self) -> String {
        let code = random_pairing_code();
        let mut state = lock(&self.pairing);
        state.code = code.clone();
        state.expires_at = Instant::now() + self.code_ttl();
        code
    }

    pub fn pairing_expires_in(&self) -> Duration {
        let mut state = lock(&self.pairing);
        let now = Instant::now();
        self.refresh_pairing_state(&mut state);
        std::cmp::min(
            state
                .expires_at
                .checked_duration_since(now)
                .unwrap_or_default(),
            self.code_ttl(),
        )
    }

    /// Accept connections until `shutdown` flips to `true`.
    pub async fn serve(
        self: Arc<Self>,
        listener: TcpListener,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<()> {
        if *shutdown.borrow() {
            return Ok(());
        }
        let connection_slots = Arc::new(Semaphore::new(self.config.limits.max_connections.max(1)));
        let per_ip_connections: Arc<Mutex<HashMap<IpAddr, usize>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let max_per_ip = self.config.limits.max_connections_per_ip.max(1);
        let request_slots = Arc::new(Semaphore::new(
            self.config.limits.max_concurrent_requests.max(1),
        ));
        let local_addr = listener.local_addr().ok();
        tracing::info!(
            bind = ?local_addr,
            pairing_ttl_s = self.config.pairing.code_ttl_s,
            "device session listener started"
        );
        let mut tasks: JoinSet<()> = JoinSet::new();

        loop {
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let (stream, addr) = match accepted {
                        Ok(accepted) => accepted,
                        Err(error) => {
                            tracing::warn!(%error, "device listener accept failed");
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    };
                    let Some(ip_guard) =
                        try_register_ip_connection(Arc::clone(&per_ip_connections), addr.ip(), max_per_ip)
                    else {
                        tracing::warn!(%addr, "per-IP device connection limit reached; dropping connection");
                        self.audit.record(
                            "",
                            AuditKind::Denied,
                            format!("per-IP connection limit reached; dropped {addr}"),
                        );
                        drop(stream);
                        continue;
                    };
                    let permit = match Arc::clone(&connection_slots).try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            tracing::warn!(%addr, "device connection limit reached; dropping connection");
                            self.audit.record("", AuditKind::Denied, format!("connection limit reached; dropped {addr}"));
                            drop(stream);
                            continue;
                        }
                    };
                    let service = Arc::clone(&self);
                    let connection_shutdown = shutdown.clone();
                    let request_slots = Arc::clone(&request_slots);
                    tasks.spawn(async move {
                        let _permit = permit;
                        let _ip_guard = ip_guard;
                        if let Err(error) = service
                            .handle_connection(stream, addr, connection_shutdown, request_slots)
                            .await
                        {
                            tracing::debug!(%addr, %error, "device connection ended");
                        }
                    });
                }
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = joined {
                        tracing::error!(%error, "device connection task panicked");
                    }
                }
            }
        }

        // Stop accepting immediately, then give connections a bounded amount of
        // time to cancel their in-flight requests and close their sockets.
        drop(listener);
        tracing::info!("device session listener stopping");
        let deadline = tokio::time::Instant::now() + SHUTDOWN_DRAIN_TIMEOUT;
        while !tasks.is_empty() {
            match tokio::time::timeout_at(deadline, tasks.join_next()).await {
                Ok(Some(Ok(()))) => {}
                Ok(Some(Err(error))) => {
                    tracing::warn!(%error, "device connection task ended with error")
                }
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!("device connections did not stop in time; aborting them");
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
        tracing::info!("device session listener stopped");
        Ok(())
    }

    /// Authenticated session for an egress bearer token.
    pub fn validate_token(&self, token: &str) -> Option<SessionDescriptor> {
        if token.is_empty() {
            return None;
        }
        let now = Instant::now();
        let mut tokens = lock(&self.tokens);
        tokens.retain(|_, record| record.expires_at > now);
        let mut matched: Option<TokenRecord> = None;
        // Compare every candidate in constant time; token lookup is not a
        // timing-sensitive secret, but this keeps the comparison hardened.
        for (candidate, record) in tokens.iter() {
            if constant_time_eq(candidate.as_bytes(), token.as_bytes()) {
                matched = Some(record.clone());
            }
        }
        drop(tokens);
        let record = matched?;
        // A token is only valid while its session is alive: closing the
        // terminal must revoke egress access immediately even if the token TTL
        // (`pairing.token_ttl_s`, default 24 h) has not expired yet (SRV-01).
        if !lock(&self.sessions).contains_key(&record.descriptor.session_id) {
            return None;
        }
        Some(record.descriptor)
    }

    /// Drop every egress token that belongs to `session_id`.
    fn revoke_session_tokens(&self, session_id: &str) {
        let mut tokens = lock(&self.tokens);
        tokens.retain(|_, record| record.descriptor.session_id != session_id);
    }

    pub fn session_count(&self) -> usize {
        lock(&self.sessions).len()
    }

    pub fn sessions(&self) -> Vec<SessionDescriptor> {
        lock(&self.sessions).values().cloned().collect()
    }

    fn code_ttl(&self) -> Duration {
        Duration::from_secs(self.config.pairing.code_ttl_s.max(1))
    }

    fn refresh_pairing_state(&self, state: &mut PairingState) {
        let now = Instant::now();
        if state.expires_at <= now {
            state.code = random_pairing_code();
            state.expires_at = now + self.code_ttl();
            tracing::info!("device pairing code expired; rotated");
        }
    }

    fn pairing_backoff(&self, ip: IpAddr) -> Option<Duration> {
        let now = Instant::now();
        let mut failures = lock(&self.pairing_failures);
        let record = failures.get_mut(&ip)?;
        if let Some(until) = record.blocked_until {
            if until > now {
                return Some(until.saturating_duration_since(now));
            }
            record.blocked_until = None;
            record.failures = 0;
            record.window_started = now;
        }
        None
    }

    fn record_pairing_failure(&self, ip: IpAddr) {
        let max_attempts = self.config.pairing.max_attempts.max(1);
        let now = Instant::now();
        let mut failures = lock(&self.pairing_failures);
        if failures.len() >= MAX_TRACKED_PAIRING_CLIENTS {
            failures.retain(|_, record| record.blocked_until.is_some_and(|until| until > now));
            if failures.len() >= MAX_TRACKED_PAIRING_CLIENTS {
                failures.clear();
            }
        }
        let record = failures.entry(ip).or_insert_with(|| PairingFailure {
            failures: 0,
            window_started: now,
            blocked_until: None,
        });
        if now.saturating_duration_since(record.window_started) > PAIRING_FAILURE_WINDOW {
            record.failures = 0;
            record.window_started = now;
        }
        record.failures = record.failures.saturating_add(1);
        if record.failures >= max_attempts {
            record.failures = 0;
            record.blocked_until = Some(now + PAIRING_BACKOFF);
            tracing::warn!(%ip, "pairing attempts exceeded; backing off");
        }
    }

    fn record_pairing_success(&self, ip: IpAddr) {
        lock(&self.pairing_failures).remove(&ip);
    }

    fn register_session(&self, descriptor: &SessionDescriptor, token: &str) {
        let ttl = Duration::from_secs(self.config.pairing.token_ttl_s.max(1));
        lock(&self.sessions).insert(descriptor.session_id.clone(), descriptor.clone());
        let now = Instant::now();
        let mut tokens = lock(&self.tokens);
        tokens.retain(|_, record| record.expires_at > now);
        tokens.insert(
            token.to_string(),
            TokenRecord {
                descriptor: descriptor.clone(),
                expires_at: now + ttl,
            },
        );
        while tokens.len() > MAX_TRACKED_TOKENS {
            let Some(oldest) = tokens
                .iter()
                .min_by_key(|(_, record)| record.expires_at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            tokens.remove(&oldest);
        }
    }

    fn check_frame_freshness(
        &self,
        expected_frame_id: Option<&str>,
    ) -> std::result::Result<(), String> {
        let Some(expected) = expected_frame_id else {
            // Callers that do not derive the plan from a screenshot may omit
            // the frame id; terminals should always send it.
            return Ok(());
        };
        let Some(last_id) = self.capture.last_frame_id() else {
            return Err(format!(
                "expected frame {expected} but no screenshot has been captured yet"
            ));
        };
        if last_id != expected {
            return Err(format!(
                "expected frame {expected} but the most recent frame is {last_id}"
            ));
        }
        let Some(meta) = self.capture.last_frame_meta() else {
            return Err(format!("no cached metadata for frame {expected}"));
        };
        let age_ms = now_unix_ms().saturating_sub(meta.captured_at_unix_ms);
        let max_age_ms = self.config.automation.frame_max_age_ms;
        if age_ms > max_age_ms {
            return Err(format!(
                "frame {expected} is {age_ms} ms old (maximum {max_age_ms} ms)"
            ));
        }
        Ok(())
    }

    async fn handle_connection(
        self: Arc<Self>,
        stream: TcpStream,
        addr: SocketAddr,
        shutdown: watch::Receiver<bool>,
        request_slots: Arc<Semaphore>,
    ) -> Result<()> {
        let established = self.establish(stream, addr).await?;
        self.run_session(established, addr, shutdown, request_slots)
            .await
    }

    /// WebSocket upgrade, pairing handshake and `SessionInit`/`SessionReady`.
    async fn establish(&self, stream: TcpStream, addr: SocketAddr) -> Result<Established> {
        let websocket_config = WebSocketConfig {
            max_message_size: Some(MAX_ENCRYPTED_FRAME_BYTES),
            max_frame_size: Some(MAX_ENCRYPTED_FRAME_BYTES),
            ..Default::default()
        };
        let socket = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            accept_async_with_config(stream, Some(websocket_config)),
        )
        .await;
        let mut socket = match socket {
            Ok(Ok(socket)) => socket,
            Ok(Err(error)) => {
                self.audit.record(
                    "",
                    AuditKind::Error,
                    format!("websocket handshake failed from {addr}: {error}"),
                );
                return Err(DeviceError::Protocol(format!(
                    "websocket handshake failed: {error}"
                )));
            }
            Err(_) => {
                self.audit.record(
                    "",
                    AuditKind::Error,
                    format!("websocket handshake timed out from {addr}"),
                );
                return Err(DeviceError::Timeout);
            }
        };

        if !self.config.pairing.enabled {
            self.audit.record(
                "",
                AuditKind::Denied,
                format!("pairing disabled; rejected {addr}"),
            );
            close_socket(&mut socket, CloseCode::Policy, "pairing_disabled").await;
            return Err(DeviceError::Denied("pairing is disabled".into()));
        }
        if let Some(remaining) = self.pairing_backoff(addr.ip()) {
            self.audit.record(
                "",
                AuditKind::Denied,
                format!(
                    "pairing rate limited for {} ({} s remaining)",
                    addr.ip(),
                    remaining.as_secs()
                ),
            );
            close_socket(&mut socket, CloseCode::Policy, "too_many_attempts").await;
            return Err(DeviceError::Denied("too many pairing attempts".into()));
        }

        // First binary frame: the initiator Noise handshake message.
        let first_frame = match tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(error))) => {
                self.audit.record(
                    "",
                    AuditKind::Error,
                    format!("websocket read failed during pairing from {addr}: {error}"),
                );
                return Err(DeviceError::Protocol(format!(
                    "websocket read failed: {error}"
                )));
            }
            Ok(None) => {
                return Err(DeviceError::Protocol(
                    "connection closed before the Noise handshake".into(),
                ));
            }
            Err(_) => {
                self.record_pairing_failure(addr.ip());
                self.audit.record(
                    "",
                    AuditKind::PairingFailed,
                    format!("noise handshake timed out from {addr}"),
                );
                return Err(DeviceError::Timeout);
            }
        };
        if !first_frame.is_binary() || first_frame.len() > MAX_HANDSHAKE_FRAME_BYTES {
            self.record_pairing_failure(addr.ip());
            self.audit.record(
                "",
                AuditKind::PairingFailed,
                format!("invalid handshake frame from {addr}"),
            );
            close_socket(&mut socket, CloseCode::Protocol, "invalid_handshake").await;
            return Err(DeviceError::Protocol("invalid handshake frame".into()));
        }

        let psk = pairing_psk(&self.pairing_code());
        let (mut secure, handshake_reply) =
            match SecureChannel::server_respond(&psk, &first_frame.into_data()) {
                Ok(handshake) => handshake,
                Err(error) => {
                    self.record_pairing_failure(addr.ip());
                    self.audit.record(
                        "",
                        AuditKind::PairingFailed,
                        format!(
                            "noise handshake failed from {addr} (expected psk fingerprint {})",
                            psk_fingerprint(&psk)
                        ),
                    );
                    close_socket(&mut socket, CloseCode::Policy, "pairing_failed").await;
                    return Err(error);
                }
            };
        if let Err(error) = socket.send(Message::Binary(handshake_reply)).await {
            return Err(DeviceError::Protocol(format!(
                "failed to send noise handshake reply: {error}"
            )));
        }

        let max_message_bytes = self
            .config
            .limits
            .max_message_bytes
            .min(PROTOCOL_MESSAGE_LIMIT);
        let init_message = match next_decrypted_message(
            &mut socket,
            &mut secure,
            max_message_bytes,
            HANDSHAKE_TIMEOUT,
        )
        .await
        {
            Ok(message) => message,
            Err(error) => {
                self.audit.record(
                    "",
                    AuditKind::Error,
                    format!("session init failed from {addr}: {error}"),
                );
                close_socket(&mut socket, CloseCode::Protocol, "invalid_session_init").await;
                return Err(error);
            }
        };
        let init = match init_message {
            DeviceMessage::SessionInit(init) => init,
            other => {
                self.reject_session(
                    &mut socket,
                    &mut secure,
                    error_code::PROTOCOL_VIOLATION,
                    "expected SessionInit as the first encrypted message",
                    CloseCode::Protocol,
                    "session_init_required",
                )
                .await;
                self.audit.record(
                    "",
                    AuditKind::PairingFailed,
                    format!("{addr} sent {} before SessionInit", message_kind(&other)),
                );
                return Err(DeviceError::Protocol("SessionInit required".into()));
            }
        };
        if init.protocol_version != DEVICE_PROTOCOL_VERSION {
            self.reject_session(
                &mut socket,
                &mut secure,
                error_code::PROTOCOL_INCOMPATIBLE,
                &format!(
                    "server speaks protocol version {DEVICE_PROTOCOL_VERSION}, client sent {}",
                    init.protocol_version
                ),
                CloseCode::Policy,
                "protocol_incompatible",
            )
            .await;
            self.audit.record(
                "",
                AuditKind::Denied,
                format!(
                    "protocol mismatch from {addr}: client {} server {DEVICE_PROTOCOL_VERSION}",
                    init.protocol_version
                ),
            );
            return Err(DeviceError::Protocol("protocol version mismatch".into()));
        }

        let capabilities = DeviceCapabilities {
            capture: self.capture.capabilities(),
            automation: self.automation.capabilities(),
            displays: self.current_displays(),
        };
        let session_id = uuid::Uuid::new_v4().to_string();
        let token = random_token();
        let device_name: String = init
            .device_name
            .chars()
            .take(MAX_DEVICE_NAME_CHARS)
            .collect();
        let descriptor = SessionDescriptor {
            session_id: session_id.clone(),
            device_name: device_name.clone(),
            remote_addr: addr.to_string(),
            connected_at_unix_ms: now_unix_ms(),
        };
        let ready = DeviceMessage::SessionReady(SessionReady {
            protocol_version: DEVICE_PROTOCOL_VERSION,
            session_id: session_id.clone(),
            token: token.clone(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities,
        });
        if let Err(error) = send_socket_message(&mut socket, &mut secure, &ready).await {
            self.audit.record(
                "",
                AuditKind::Error,
                format!("failed to send SessionReady to {addr}: {error}"),
            );
            close_socket(&mut socket, CloseCode::Away, "send_failed").await;
            return Err(error);
        }

        self.register_session(&descriptor, &token);
        self.record_pairing_success(addr.ip());
        self.rotate_pairing_code();
        self.audit.record(
            &session_id,
            AuditKind::Connected,
            format!(
                "terminal {:?} from {addr} device={device_name}",
                init.client_kind
            ),
        );
        tracing::info!(session = %session_id, %addr, "terminal session established");

        Ok(Established {
            session_id,
            socket,
            secure,
        })
    }

    async fn reject_session(
        &self,
        socket: &mut DeviceSocket,
        secure: &mut SecureChannel,
        code: &str,
        message: &str,
        close_code: CloseCode,
        close_reason: &'static str,
    ) {
        let rejected = DeviceMessage::SessionRejected(SessionRejected {
            code: code.to_string(),
            message: message.to_string(),
        });
        let _ = send_socket_message(socket, secure, &rejected).await;
        close_socket(socket, close_code, close_reason).await;
    }

    fn current_displays(&self) -> Vec<crate::display::DisplayInfo> {
        let mut displays = match self.capture.displays() {
            Ok(set) => set.displays,
            Err(error) => {
                tracing::debug!(%error, "display discovery failed during session init");
                Vec::new()
            }
        };
        if displays.is_empty() {
            displays = self
                .capture
                .last_frame_meta()
                .map(|frame| frame.displays)
                .unwrap_or_default();
        }
        displays
    }

    /// Request loop for one authenticated session.
    async fn run_session(
        self: Arc<Self>,
        established: Established,
        addr: SocketAddr,
        mut shutdown: watch::Receiver<bool>,
        request_slots: Arc<Semaphore>,
    ) -> Result<()> {
        let Established {
            session_id,
            socket,
            secure,
        } = established;
        let (mut sink, mut stream) = socket.split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Outbound>();
        let mut writer = tokio::spawn(async move {
            while let Some(outbound) = out_rx.recv().await {
                match outbound {
                    Outbound::Binary(bytes) => {
                        if sink.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Outbound::Close { code, reason } => {
                        let _ = sink
                            .send(Message::Close(Some(CloseFrame {
                                code,
                                reason: reason.into(),
                            })))
                            .await;
                        let _ = sink.close().await;
                        break;
                    }
                }
            }
            let _ = sink.flush().await;
        });

        let channel = Arc::new(Mutex::new(secure));
        let registry: CancelRegistry = Arc::new(Mutex::new(HashMap::new()));
        let idle_timeout = Duration::from_secs(self.config.limits.idle_timeout_s.max(1));
        let request_timeout = Duration::from_secs(self.config.limits.request_timeout_s.max(1));
        let max_message_bytes = self
            .config
            .limits
            .max_message_bytes
            .min(PROTOCOL_MESSAGE_LIMIT);
        let mut idle_deadline = tokio::time::Instant::now() + idle_timeout;
        let mut rate = RateLimit::new();
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut violations: u32 = 0;
        let exit_reason;

        loop {
            if *shutdown.borrow() {
                exit_reason = "server shutdown";
                break;
            }
            tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        exit_reason = "server shutdown";
                        break;
                    }
                }
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(Err(error)) = joined {
                        if error.is_panic() {
                            tracing::error!(session = %session_id, %error, "session request task panicked");
                            self.audit.record(&session_id, AuditKind::Error, "request worker panicked");
                        }
                    }
                }
                _ = tokio::time::sleep_until(idle_deadline) => {
                    exit_reason = "idle timeout";
                    break;
                }
                frame = stream.next() => {
                    let frame = match frame {
                        Some(Ok(frame)) => frame,
                        Some(Err(error)) => {
                            tracing::debug!(session = %session_id, %error, "websocket read error");
                            exit_reason = "websocket error";
                            break;
                        }
                        None => {
                            exit_reason = "peer closed";
                            break;
                        }
                    };
                    if frame.is_close() {
                        exit_reason = "peer closed";
                        break;
                    }
                    if !frame.is_binary() {
                        continue;
                    }
                    idle_deadline = tokio::time::Instant::now() + idle_timeout;
                    let decrypted = {
                        let mut secure = lock(&channel);
                        secure.decrypt(&frame.into_data(), max_message_bytes)
                    };
                    let message = match decrypted {
                        Ok(Some(message)) => message,
                        Ok(None) => continue,
                        Err(error) => {
                            tracing::debug!(session = %session_id, %error, "failed to decrypt frame");
                            self.audit.record(&session_id, AuditKind::Error, format!("decrypt failed: {error}"));
                            let _ = send_error(&channel, &out_tx, None, error_code_for(&error), error.to_string());
                            exit_reason = "decrypt failure";
                            break;
                        }
                    };
                    match message {
                        DeviceMessage::Ping { request_id, nonce } => {
                            if let Err(error) = send_message(&channel, &out_tx, &DeviceMessage::Pong { request_id, nonce }) {
                                tracing::debug!(session = %session_id, %error, "failed to send pong");
                                exit_reason = "send failure";
                                break;
                            }
                        }
                        DeviceMessage::Cancel { request_id } => {
                            let control = lock(&registry).remove(&request_id);
                            if let Some(control) = control {
                                control.cancel();
                                self.audit.record(&session_id, AuditKind::Cancelled, format!("request {request_id} cancelled by client"));
                            }
                        }
                        DeviceMessage::CaptureRequest { request_id, request } => {
                            if !self.admit(&session_id, &mut rate, &channel, &out_tx, &registry, &request_id) {
                                continue;
                            }
                            let Some(permit) = acquire_request_slot(&request_slots) else {
                                let _ = send_error(&channel, &out_tx, Some(&request_id), error_code::RATE_LIMITED, "too many concurrent requests");
                                continue;
                            };
                            let control = ExecutionControl::new();
                            lock(&registry).insert(request_id.clone(), control.clone());
                            tasks.spawn(run_request(
                                Arc::clone(&self),
                                Arc::clone(&channel),
                                out_tx.clone(),
                                Arc::clone(&registry),
                                session_id.clone(),
                                permit,
                                PendingRequest::Capture { request_id, request },
                                control,
                                request_timeout,
                            ));
                        }
                        DeviceMessage::DisplayListRequest { request_id } => {
                            if !self.admit(&session_id, &mut rate, &channel, &out_tx, &registry, &request_id) {
                                continue;
                            }
                            let Some(permit) = acquire_request_slot(&request_slots) else {
                                let _ = send_error(&channel, &out_tx, Some(&request_id), error_code::RATE_LIMITED, "too many concurrent requests");
                                continue;
                            };
                            let control = ExecutionControl::new();
                            lock(&registry).insert(request_id.clone(), control.clone());
                            tasks.spawn(run_request(
                                Arc::clone(&self),
                                Arc::clone(&channel),
                                out_tx.clone(),
                                Arc::clone(&registry),
                                session_id.clone(),
                                permit,
                                PendingRequest::DisplayList { request_id },
                                control,
                                request_timeout,
                            ));
                        }
                        DeviceMessage::HealthRequest { request_id } => {
                            if !self.admit(&session_id, &mut rate, &channel, &out_tx, &registry, &request_id) {
                                continue;
                            }
                            let Some(permit) = acquire_request_slot(&request_slots) else {
                                let _ = send_error(&channel, &out_tx, Some(&request_id), error_code::RATE_LIMITED, "too many concurrent requests");
                                continue;
                            };
                            let control = ExecutionControl::new();
                            lock(&registry).insert(request_id.clone(), control.clone());
                            tasks.spawn(run_request(
                                Arc::clone(&self),
                                Arc::clone(&channel),
                                out_tx.clone(),
                                Arc::clone(&registry),
                                session_id.clone(),
                                permit,
                                PendingRequest::Health { request_id },
                                control,
                                request_timeout,
                            ));
                        }
                        DeviceMessage::ActionPlanRequest { request_id, plan, approved, expected_frame_id } => {
                            if !self.admit(&session_id, &mut rate, &channel, &out_tx, &registry, &request_id) {
                                continue;
                            }
                            if let Err(reason) = self.check_frame_freshness(expected_frame_id.as_deref()) {
                                let _ = send_error(&channel, &out_tx, Some(&request_id), error_code::STALE_FRAME, reason.clone());
                                self.audit.record(&session_id, AuditKind::Denied, format!("stale action plan {request_id} rejected: {reason}"));
                                continue;
                            }
                            let Some(permit) = acquire_request_slot(&request_slots) else {
                                let _ = send_error(&channel, &out_tx, Some(&request_id), error_code::RATE_LIMITED, "too many concurrent requests");
                                continue;
                            };
                            let control = ExecutionControl::new();
                            lock(&registry).insert(request_id.clone(), control.clone());
                            tasks.spawn(run_request(
                                Arc::clone(&self),
                                Arc::clone(&channel),
                                out_tx.clone(),
                                Arc::clone(&registry),
                                session_id.clone(),
                                permit,
                                PendingRequest::ActionPlan { request_id, plan, approved },
                                control,
                                request_timeout,
                            ));
                        }
                        other => {
                            violations = violations.saturating_add(1);
                            self.audit.record(&session_id, AuditKind::Error, format!("unexpected {} message", message_kind(&other)));
                            let _ = send_error(&channel, &out_tx, None, error_code::PROTOCOL_VIOLATION, format!("unexpected {} message", message_kind(&other)));
                            if violations >= 3 {
                                exit_reason = "protocol violations";
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Cancel everything still running, close the socket, then drain.
        for (_, control) in lock(&registry).drain() {
            control.cancel();
        }
        let _ = out_tx.send(Outbound::Close {
            code: CloseCode::Away,
            reason: "session_closed",
        });
        drop(out_tx);
        let drain_tasks = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, drain_tasks)
            .await
            .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        if tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut writer)
            .await
            .is_err()
        {
            writer.abort();
        }
        lock(&self.sessions).remove(&session_id);
        self.revoke_session_tokens(&session_id);
        self.audit.record(
            &session_id,
            AuditKind::Disconnected,
            format!("{exit_reason}; remote {addr}"),
        );
        tracing::info!(session = %session_id, remote = %addr, reason = exit_reason, "terminal session ended");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn admit(
        &self,
        session_id: &str,
        rate: &mut RateLimit,
        channel: &Mutex<SecureChannel>,
        out_tx: &mpsc::UnboundedSender<Outbound>,
        registry: &CancelRegistry,
        request_id: &str,
    ) -> bool {
        if lock(registry).contains_key(request_id) {
            let _ = send_error(
                channel,
                out_tx,
                Some(request_id),
                error_code::PROTOCOL_VIOLATION,
                "duplicate request_id",
            );
            self.audit.record(
                session_id,
                AuditKind::Error,
                format!("duplicate request_id {request_id}"),
            );
            return false;
        }
        if !rate.allow() {
            let _ = send_error(
                channel,
                out_tx,
                Some(request_id),
                error_code::RATE_LIMITED,
                "request rate limit exceeded; retry later",
            );
            self.audit.record(
                session_id,
                AuditKind::Denied,
                format!("rate limited request {request_id}"),
            );
            return false;
        }
        true
    }
}

struct Established {
    session_id: String,
    socket: DeviceSocket,
    secure: SecureChannel,
}

enum Outbound {
    Binary(Vec<u8>),
    Close {
        code: CloseCode,
        reason: &'static str,
    },
}

struct RateLimit {
    window_started: Instant,
    count: u32,
}

impl RateLimit {
    fn new() -> Self {
        Self {
            window_started: Instant::now(),
            count: 0,
        }
    }

    fn allow(&mut self) -> bool {
        let now = Instant::now();
        if now.saturating_duration_since(self.window_started) >= RATE_LIMIT_WINDOW {
            self.window_started = now;
            self.count = 0;
        }
        if self.count >= RATE_LIMIT_REQUESTS {
            false
        } else {
            self.count += 1;
            true
        }
    }
}

enum PendingRequest {
    Capture {
        request_id: String,
        request: CaptureRequest,
    },
    DisplayList {
        request_id: String,
    },
    Health {
        request_id: String,
    },
    ActionPlan {
        request_id: String,
        plan: ActionPlan,
        approved: bool,
    },
}

impl PendingRequest {
    fn request_id(&self) -> &str {
        match self {
            Self::Capture { request_id, .. }
            | Self::DisplayList { request_id }
            | Self::Health { request_id }
            | Self::ActionPlan { request_id, .. } => request_id,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_request(
    service: Arc<SessionService>,
    channel: Arc<Mutex<SecureChannel>>,
    out_tx: mpsc::UnboundedSender<Outbound>,
    registry: CancelRegistry,
    session_id: String,
    permit: OwnedSemaphorePermit,
    request: PendingRequest,
    control: ExecutionControl,
    timeout: Duration,
) {
    let _permit = permit;
    let request_id = request.request_id().to_string();
    let response = match tokio::time::timeout(
        timeout,
        dispatch_request(Arc::clone(&service), session_id, request, control.clone()),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => {
            control.cancel();
            error_message(Some(request_id.clone()), &DeviceError::Timeout)
        }
    };
    if let Err(error) = send_message(&channel, &out_tx, &response) {
        tracing::debug!(%error, "failed to send session response");
    }
    lock(&registry).remove(&request_id);
}

async fn dispatch_request(
    service: Arc<SessionService>,
    session_id: String,
    request: PendingRequest,
    control: ExecutionControl,
) -> DeviceMessage {
    match request {
        PendingRequest::Capture {
            request_id,
            request,
        } => {
            let capture = Arc::clone(&service.capture);
            let outcome = run_blocking(move || capture.capture(&request)).await;
            match outcome {
                Ok(frame) => {
                    let encoded_len = frame.data.len();
                    let estimated = encoded_len.saturating_mul(4) / 3 + 1024;
                    if encoded_len > MAX_FRAME_BYTES || estimated > PROTOCOL_MESSAGE_LIMIT {
                        service.audit.record(
                            &session_id,
                            AuditKind::Error,
                            format!(
                                "capture frame is too large for the protocol ({encoded_len} bytes)"
                            ),
                        );
                        return error_message(
                            Some(request_id),
                            &DeviceError::Protocol(format!(
                                "MESSAGE_TOO_LARGE: encoded frame is {encoded_len} bytes, \
                                 exceeds the protocol limit"
                            )),
                        );
                    }
                    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&frame.data);
                    service.audit.record(
                        &session_id,
                        AuditKind::Capture,
                        format!(
                            "request={request_id} frame={} {}x{} mime={:?} bytes={encoded_len} backend={}",
                            frame.frame_id, frame.width, frame.height, frame.mime, frame.backend
                        ),
                    );
                    DeviceMessage::CaptureResult {
                        request_id,
                        frame: FrameMeta::from(&frame),
                        data_base64,
                    }
                }
                Err(error) => {
                    service.audit.record(
                        &session_id,
                        AuditKind::Error,
                        format!("capture failed: {error}"),
                    );
                    error_message(Some(request_id), &error)
                }
            }
        }
        PendingRequest::DisplayList { request_id } => {
            let capture = Arc::clone(&service.capture);
            let mut displays = match run_blocking(move || capture.displays()).await {
                Ok(set) => set.displays,
                Err(error) => {
                    tracing::debug!(%error, "display discovery failed; falling back");
                    Vec::new()
                }
            };
            if displays.is_empty() {
                displays = service
                    .capture
                    .last_frame_meta()
                    .map(|frame| frame.displays)
                    .unwrap_or_default();
            }
            DeviceMessage::DisplayList {
                request_id,
                displays,
            }
        }
        PendingRequest::Health { request_id } => {
            let capture = Arc::clone(&service.capture);
            let automation = Arc::clone(&service.automation);
            let uptime_s = service.uptime().as_secs();
            let sessions = service.session_count();
            let status = run_blocking(move || {
                Ok::<HealthStatus, DeviceError>(HealthStatus {
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol_version: DEVICE_PROTOCOL_VERSION,
                    uptime_s,
                    sessions,
                    capture_backend: capture.backend_name(),
                    automation_backend: automation.backend_name(),
                })
            })
            .await;
            match status {
                Ok(status) => DeviceMessage::Health { request_id, status },
                Err(error) => error_message(Some(request_id), &error),
            }
        }
        PendingRequest::ActionPlan {
            request_id,
            plan,
            approved,
        } => {
            let automation = Arc::clone(&service.automation);
            let worker_control = control.clone();
            let outcome =
                run_blocking(move || automation.execute_guarded(&plan, approved, &worker_control))
                    .await;
            let dry_run = service.automation.config().dry_run;
            match outcome {
                Ok(outcome) => {
                    let cancelled = outcome.cancelled || control.is_cancelled();
                    let state = if cancelled {
                        ExecutionState::Cancelled
                    } else {
                        ExecutionState::Completed
                    };
                    service.audit.record(
                        &session_id,
                        if cancelled {
                            AuditKind::Cancelled
                        } else {
                            AuditKind::Action
                        },
                        format!(
                            "request={request_id} actions={} dry_run={} cancelled={cancelled} approved={approved}",
                            outcome.actions_executed, outcome.dry_run
                        ),
                    );
                    DeviceMessage::ActionResult {
                        request_id,
                        actions_executed: outcome.actions_executed,
                        state,
                        dry_run: outcome.dry_run,
                    }
                }
                Err(DeviceError::Cancelled) => {
                    service.audit.record(
                        &session_id,
                        AuditKind::Cancelled,
                        format!("request={request_id} action plan cancelled"),
                    );
                    DeviceMessage::ActionResult {
                        request_id,
                        actions_executed: 0,
                        state: ExecutionState::Cancelled,
                        dry_run,
                    }
                }
                Err(error) => {
                    service.audit.record(
                        &session_id,
                        if matches!(error, DeviceError::Denied(_)) {
                            AuditKind::Denied
                        } else {
                            AuditKind::Error
                        },
                        format!("request={request_id} action plan rejected: {error}"),
                    );
                    error_message(Some(request_id), &error)
                }
            }
        }
    }
}

async fn run_blocking<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(error) => Err(DeviceError::Backend(format!(
            "request worker failed: {error}"
        ))),
    }
}

fn acquire_request_slot(slots: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    Arc::clone(slots).try_acquire_owned().ok()
}

fn error_message(request_id: Option<String>, error: &DeviceError) -> DeviceMessage {
    DeviceMessage::Error {
        request_id,
        code: error_code_for(error).to_string(),
        message: error.to_string(),
    }
}

/// Map a [`DeviceError`] to the stable wire code. Protocol detail strings that
/// start with a stable code (chunking uses `MESSAGE_TOO_LARGE`) keep that code
/// instead of the generic `PROTOCOL_VIOLATION`.
fn error_code_for(error: &DeviceError) -> &'static str {
    match error {
        DeviceError::Protocol(detail) if detail.starts_with("MESSAGE_TOO_LARGE") => {
            error_code::MESSAGE_TOO_LARGE
        }
        other => other.code(),
    }
}

fn send_message(
    channel: &Mutex<SecureChannel>,
    out_tx: &mpsc::UnboundedSender<Outbound>,
    message: &DeviceMessage,
) -> Result<()> {
    let frames = {
        let mut secure = lock(channel);
        secure.encrypt(message)?
    };
    for frame in frames {
        out_tx
            .send(Outbound::Binary(frame))
            .map_err(|_| DeviceError::Protocol("session is closed".into()))?;
    }
    Ok(())
}

fn send_error(
    channel: &Mutex<SecureChannel>,
    out_tx: &mpsc::UnboundedSender<Outbound>,
    request_id: Option<&str>,
    code: &str,
    message: impl Into<String>,
) -> Result<()> {
    send_message(
        channel,
        out_tx,
        &DeviceMessage::Error {
            request_id: request_id.map(str::to_string),
            code: code.to_string(),
            message: message.into(),
        },
    )
}

async fn send_socket_message(
    socket: &mut DeviceSocket,
    channel: &mut SecureChannel,
    message: &DeviceMessage,
) -> Result<()> {
    let frames = channel.encrypt(message)?;
    for frame in frames {
        socket
            .send(Message::Binary(frame))
            .await
            .map_err(|error| DeviceError::Protocol(format!("websocket send failed: {error}")))?;
    }
    Ok(())
}

async fn next_decrypted_message(
    socket: &mut DeviceSocket,
    channel: &mut SecureChannel,
    max_message_bytes: usize,
    timeout: Duration,
) -> Result<DeviceMessage> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DeviceError::Timeout);
        }
        let frame = tokio::time::timeout(remaining, socket.next())
            .await
            .map_err(|_| DeviceError::Timeout)?
            .ok_or_else(|| DeviceError::Protocol("connection closed".into()))?
            .map_err(|error| DeviceError::Protocol(format!("websocket read failed: {error}")))?;
        if frame.is_close() {
            return Err(DeviceError::Protocol("connection closed".into()));
        }
        if !frame.is_binary() {
            continue;
        }
        if let Some(message) = channel.decrypt(&frame.into_data(), max_message_bytes)? {
            return Ok(message);
        }
    }
}

async fn close_socket(socket: &mut DeviceSocket, code: CloseCode, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

fn message_kind(message: &DeviceMessage) -> &'static str {
    match message {
        DeviceMessage::SessionInit(_) => "session_init",
        DeviceMessage::SessionReady(_) => "session_ready",
        DeviceMessage::SessionRejected(_) => "session_rejected",
        DeviceMessage::CaptureRequest { .. } => "capture_request",
        DeviceMessage::CaptureResult { .. } => "capture_result",
        DeviceMessage::DisplayListRequest { .. } => "display_list_request",
        DeviceMessage::DisplayList { .. } => "display_list",
        DeviceMessage::ActionPlanRequest { .. } => "action_plan_request",
        DeviceMessage::ActionProgress { .. } => "action_progress",
        DeviceMessage::ActionResult { .. } => "action_result",
        DeviceMessage::Cancel { .. } => "cancel",
        DeviceMessage::HealthRequest { .. } => "health_request",
        DeviceMessage::Health { .. } => "health",
        DeviceMessage::Ping { .. } => "ping",
        DeviceMessage::Pong { .. } => "pong",
        DeviceMessage::Error { .. } => "error",
    }
}

/// Recover from a poisoned mutex instead of panicking; the protected state is
/// plain data that stays consistent even if another thread panicked mid-update.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_ip_connection_cap_rejects_extra_and_releases_slots() {
        let counts: Arc<Mutex<HashMap<IpAddr, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let ip: IpAddr = "192.0.2.10".parse().unwrap();
        let other: IpAddr = "192.0.2.11".parse().unwrap();

        let first = try_register_ip_connection(Arc::clone(&counts), ip, 2).expect("first slot");
        let second = try_register_ip_connection(Arc::clone(&counts), ip, 2).expect("second slot");
        assert!(
            try_register_ip_connection(Arc::clone(&counts), ip, 2).is_none(),
            "third unauthenticated connection from one IP must be rejected"
        );
        // Other peers are unaffected by one noisy source.
        let _third = try_register_ip_connection(Arc::clone(&counts), other, 2).expect("other ip");

        drop(first);
        let reused = try_register_ip_connection(Arc::clone(&counts), ip, 2)
            .expect("a released slot must be reusable");
        drop(second);
        drop(reused);
        drop(_third);
        assert!(lock(&counts).is_empty(), "all slots must be released");
    }
}
