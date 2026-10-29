//! Audit trail for the device service.
//!
//! Owner: Lead. Records bounded, privacy-conscious entries; never stores typed
//! text, screenshot bytes or pairing codes.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Mutex;

impl AuditKind {
    /// Stable lowercase identifier used in structured audit logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disconnected => "disconnected",
            Self::PairingFailed => "pairing_failed",
            Self::Capture => "capture",
            Self::Action => "action",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
            Self::Egress => "egress",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    Connected,
    Disconnected,
    PairingFailed,
    Capture,
    Action,
    Denied,
    Cancelled,
    Error,
    Egress,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub time_unix_ms: u64,
    pub session_id: String,
    pub kind: AuditKind,
    pub detail: String,
}

#[derive(Default)]
pub struct AuditLog {
    capacity: usize,
    entries: Mutex<VecDeque<AuditEntry>>,
}

impl AuditLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: Mutex::new(VecDeque::with_capacity(capacity.max(1))),
        }
    }

    pub fn record(&self, session_id: &str, kind: AuditKind, detail: impl Into<String>) {
        let detail = redact(&detail.into());
        // Mirror security-relevant audit events into the structured service log
        // (journald) so they survive the bounded in-memory ring (SRV-16).
        // `detail` was already passed through `redact` (single line, 512 chars)
        // and never contains pairing codes, tokens or screenshot bytes.
        tracing::info!(
            audit_kind = kind.as_str(),
            session_id,
            detail = %detail,
            "device audit event"
        );
        let entry = AuditEntry {
            time_unix_ms: crate::capture::now_unix_ms(),
            session_id: session_id.to_string(),
            kind,
            detail,
        };
        if let Ok(mut entries) = self.entries.lock() {
            if entries.len() == self.capacity {
                entries.pop_back();
            }
            entries.push_front(entry);
        }
    }

    pub fn snapshot(&self) -> Vec<AuditEntry> {
        self.entries
            .lock()
            .map(|entries| entries.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Replace control characters and clamp length so logs stay single-line.
fn redact(detail: &str) -> String {
    let mut out: String = detail
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .take(512)
        .collect();
    if detail.chars().count() > 512 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_log_is_bounded_and_single_line() {
        let log = AuditLog::new(2);
        log.record("s", AuditKind::Connected, "line\nbreak");
        log.record("s", AuditKind::Capture, "capture");
        log.record("s", AuditKind::Action, "action");
        let entries = log.snapshot();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].detail, "action");
        assert!(!log
            .snapshot()
            .iter()
            .any(|entry| entry.detail.contains('\n')));
    }
}
