//! Wire protocol between a control terminal and the computer service.
//!
//! Owner: Lead (message shapes) + session teammate (handshake/routing). The
//! protocol is versioned independently from `ale_core::remote`, which is the
//! phone-facing chat protocol of the desktop GUI.
//!
//! Transport: WebSocket binary frames on `device.bind`, each frame carrying one
//! Noise-encrypted payload (see `crypto.rs`). Messages larger than one Noise
//! plaintext are chunked; the receiving side reassembles them before decoding.

use crate::automation::AutomationCapabilities;
use crate::capture::{CaptureCapabilities, CaptureRequest, ImageMime};
use crate::display::DisplayInfo;
use ale_core::actions::ActionPlan;
use serde::{Deserialize, Serialize};

pub const DEVICE_PROTOCOL_VERSION: u32 = 1;
/// Largest accepted reassembled message (also bounded by `limits.max_message_bytes`).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
/// Largest accepted encoded screenshot payload.
pub const MAX_FRAME_BYTES: usize = 6 * 1024 * 1024;

pub mod error_code {
    pub const PROTOCOL_INCOMPATIBLE: &str = "PROTOCOL_INCOMPATIBLE";
    pub const NOT_PAIRED: &str = "NOT_PAIRED";
    pub const PAIRING_DISABLED: &str = "PAIRING_DISABLED";
    pub const PAIRING_EXPIRED: &str = "PAIRING_EXPIRED";
    pub const TOO_MANY_ATTEMPTS: &str = "TOO_MANY_ATTEMPTS";
    pub const REQUEST_TIMEOUT: &str = "REQUEST_TIMEOUT";
    pub const CANCELLED: &str = "CANCELLED";
    pub const STALE_FRAME: &str = "STALE_FRAME";
    pub const DENIED: &str = "DENIED";
    pub const CAPABILITY_UNAVAILABLE: &str = "CAPABILITY_UNAVAILABLE";
    pub const BACKEND_ERROR: &str = "BACKEND_ERROR";
    pub const MESSAGE_TOO_LARGE: &str = "MESSAGE_TOO_LARGE";
    pub const RATE_LIMITED: &str = "RATE_LIMITED";
    pub const PROTOCOL_VIOLATION: &str = "PROTOCOL_VIOLATION";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Terminal,
    Mobile,
    Cli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInit {
    pub protocol_version: u32,
    pub device_name: String,
    pub client_kind: ClientKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub capture: CaptureCapabilities,
    pub automation: AutomationCapabilities,
    pub displays: Vec<DisplayInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReady {
    pub protocol_version: u32,
    pub session_id: String,
    /// Bearer token for the HTTP egress proxy.
    pub token: String,
    pub server_version: String,
    pub capabilities: DeviceCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRejected {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameMeta {
    pub frame_id: String,
    pub display_id: Option<u32>,
    pub mime: ImageMime,
    pub width: u32,
    pub height: u32,
    pub captured_at_unix_ms: u64,
    pub backend: String,
}

impl From<&crate::capture::CapturedFrame> for FrameMeta {
    fn from(frame: &crate::capture::CapturedFrame) -> Self {
        Self {
            frame_id: frame.frame_id.clone(),
            display_id: frame.display_id,
            mime: frame.mime,
            width: frame.width,
            height: frame.height,
            captured_at_unix_ms: frame.captured_at_unix_ms,
            backend: frame.backend.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatus {
    pub server_version: String,
    pub protocol_version: u32,
    pub uptime_s: u64,
    pub sessions: usize,
    pub capture_backend: Option<String>,
    pub automation_backend: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceMessage {
    SessionInit(SessionInit),
    SessionReady(SessionReady),
    SessionRejected(SessionRejected),

    CaptureRequest {
        request_id: String,
        request: CaptureRequest,
    },
    CaptureResult {
        request_id: String,
        frame: FrameMeta,
        data_base64: String,
    },

    DisplayListRequest {
        request_id: String,
    },
    DisplayList {
        request_id: String,
        displays: Vec<DisplayInfo>,
    },

    ActionPlanRequest {
        request_id: String,
        plan: ActionPlan,
        approved: bool,
        /// Screenshot the plan was derived from; rejected when stale.
        expected_frame_id: Option<String>,
    },
    ActionProgress {
        request_id: String,
        executed: usize,
        total: usize,
    },
    ActionResult {
        request_id: String,
        actions_executed: usize,
        state: ExecutionState,
        dry_run: bool,
    },

    Cancel {
        request_id: String,
    },

    HealthRequest {
        request_id: String,
    },
    Health {
        request_id: String,
        status: HealthStatus,
    },

    Ping {
        request_id: String,
        nonce: u64,
    },
    Pong {
        request_id: String,
        nonce: u64,
    },

    Error {
        request_id: Option<String>,
        code: String,
        message: String,
    },
}
