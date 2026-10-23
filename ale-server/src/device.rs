//! Adapter between the `ale-device` terminal channel and the HTTP server.
//!
//! The HTTP layer talks to the device service through the [`DeviceAccess`] trait
//! so that:
//!
//! * the server can be tested without a real terminal channel or display;
//! * `ale-device` internals stay behind the frozen public API.
//!
//! [`DeviceRuntime`] is the production implementation: it owns the
//! [`SessionService`] (encrypted WS channel), the shared [`AuditLog`] and reports
//! pairing/session/audit snapshots for the loopback-only `/v1/device/status`.
//!
//! Security: pairing codes are only ever returned by this module to the
//! loopback-only device-status handler and to the startup/pairing logs (the
//! operator needs them to pair a terminal). They are never exposed on a
//! non-loopback interface.

use ale_device::{
    AuditEntry, AuditKind, AuditLog, AutomationService, CaptureService, DeviceConfig,
    SessionDescriptor, SessionService,
};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// Capacity of the bounded in-memory audit log.
pub const AUDIT_CAPACITY: usize = 128;
/// How many audit entries `GET /v1/device/status` returns by default.
pub const DEVICE_STATUS_AUDIT_TAIL: usize = 50;
/// How often the pairing watcher checks whether the code rotated.
pub const PAIRING_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Snapshot of the pairing code and its remaining lifetime.
#[derive(Debug, Clone)]
pub struct PairingInfo {
    pub code: String,
    pub expires_in_s: u64,
}

/// Everything the HTTP layer needs from the device channel.
///
/// Implementations must be cheap and non-blocking: the methods are called from
/// request handlers and from the runtime status endpoints.
pub trait DeviceAccess: Send + Sync {
    /// Current pairing code and remaining lifetime.
    fn pairing_info(&self) -> PairingInfo;
    /// Active authenticated sessions.
    fn sessions(&self) -> Vec<SessionDescriptor>;
    /// Most recent audit entries, newest first.
    fn audit_tail(&self, limit: usize) -> Vec<AuditEntry>;
    /// Resolve a Bearer token issued in `SessionReady` to its session.
    fn validate_token(&self, token: &str) -> Option<SessionDescriptor>;
    /// Record an egress event in the bounded audit log.
    fn record_egress(&self, session_id: &str, detail: &str);
}

/// Production device runtime.
pub struct DeviceRuntime {
    session: Arc<SessionService>,
    audit: Arc<AuditLog>,
    started: Instant,
}

impl DeviceRuntime {
    /// Build the capture/automation/audit services and the session server from
    /// the `[device]` configuration. This does not bind any socket yet.
    pub fn new(config: DeviceConfig) -> Self {
        let capture = Arc::new(CaptureService::new(config.capture.clone()));
        let automation = Arc::new(AutomationService::new(config.automation.clone()));
        let audit = Arc::new(AuditLog::new(AUDIT_CAPACITY));
        let session = Arc::new(SessionService::new(
            config,
            capture,
            automation,
            audit.clone(),
        ));
        Self {
            session,
            audit,
            started: Instant::now(),
        }
    }

    /// The underlying session service; used by the binary to `serve()` the WS
    /// listener and by `--print-pairing`.
    pub fn session_service(&self) -> &Arc<SessionService> {
        &self.session
    }

    /// Shared audit log.
    pub fn audit_log(&self) -> &Arc<AuditLog> {
        &self.audit
    }

    /// Time since the device runtime was created.
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Force a new pairing code (admin `--print-pairing`).
    pub fn rotate_pairing(&self) -> PairingInfo {
        let code = self.session.rotate_pairing_code();
        PairingInfo {
            code,
            expires_in_s: self.session.pairing_expires_in().as_secs(),
        }
    }

    fn current_pairing(&self) -> PairingInfo {
        PairingInfo {
            code: self.session.pairing_code(),
            expires_in_s: self.session.pairing_expires_in().as_secs(),
        }
    }
}

impl DeviceAccess for DeviceRuntime {
    fn pairing_info(&self) -> PairingInfo {
        self.current_pairing()
    }

    fn sessions(&self) -> Vec<SessionDescriptor> {
        self.session.sessions()
    }

    fn audit_tail(&self, limit: usize) -> Vec<AuditEntry> {
        let mut entries = self.audit.snapshot();
        entries.truncate(limit.min(AUDIT_CAPACITY));
        entries
    }

    fn validate_token(&self, token: &str) -> Option<SessionDescriptor> {
        self.session.validate_token(token)
    }

    fn record_egress(&self, session_id: &str, detail: &str) {
        self.audit.record(session_id, AuditKind::Egress, detail);
    }
}

/// Serialize a session descriptor for the device-status endpoint.
///
/// `SessionDescriptor` intentionally does not derive `Serialize` (frozen
/// `ale-device` API), so the HTTP representation is defined here.
pub fn session_to_json(descriptor: &SessionDescriptor) -> Value {
    json!({
        "session_id": descriptor.session_id,
        "device_name": descriptor.device_name,
        "remote_addr": descriptor.remote_addr,
        "connected_at_unix_ms": descriptor.connected_at_unix_ms,
    })
}

/// Log the active pairing code and its expiry at INFO level.
///
/// This is the only place (besides `/v1/device/status`) where the code is
/// emitted; the operator needs it to pair a terminal on a headless box.
pub fn log_pairing(device: &dyn DeviceAccess, context: &str) {
    let pairing = device.pairing_info();
    tracing::info!(
        pairing_code = %pairing.code,
        expires_in_s = pairing.expires_in_s,
        context,
        "device pairing code active"
    );
}

/// Background watcher that logs pairing-code rotation (auto-rotation after the
/// configured TTL or an explicit rotation via `--print-pairing` otherwise).
///
/// Stops when the shutdown watch is set to `true` or its sender is dropped.
pub fn spawn_pairing_watcher(
    device: Arc<DeviceRuntime>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut current = device.current_pairing().code;
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(PAIRING_LOG_INTERVAL) => {}
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                    continue;
                }
            }
            let pairing = device.current_pairing();
            if pairing.code != current {
                tracing::info!(
                    pairing_code = %pairing.code,
                    expires_in_s = pairing.expires_in_s,
                    "device pairing code rotated"
                );
                current = pairing.code;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> SessionDescriptor {
        SessionDescriptor {
            session_id: "session-1".to_string(),
            device_name: "test-phone".to_string(),
            remote_addr: "192.0.2.10:5000".to_string(),
            connected_at_unix_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn session_descriptor_serializes_stably() {
        let value = session_to_json(&descriptor());
        assert_eq!(value["session_id"], "session-1");
        assert_eq!(value["device_name"], "test-phone");
        assert_eq!(value["remote_addr"], "192.0.2.10:5000");
        assert_eq!(value["connected_at_unix_ms"], 1_700_000_000_000_i64);
    }
}
