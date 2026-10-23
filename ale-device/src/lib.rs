//! Headless device service for Ale, My Eyes!
//!
//! This crate is the computer-side half of the split architecture: it exposes
//! screen capture, guarded automation and bounded network egress to an
//! authenticated control terminal (phone, dedicated hardware, or CLI). It has no
//! GUI dependency and never loads a model itself.
//!
//! Module ownership is recorded in `docs/CONTRACTS.md`.

pub mod audit;
pub mod automation;
pub mod capture;
pub mod config;
pub mod crypto;
pub mod display;
pub mod error;
pub mod protocol;
pub mod session;

pub use audit::{AuditEntry, AuditKind, AuditLog};
pub use automation::{ActionOutcome, AutomationCapabilities, AutomationService, ExecutionControl};
pub use capture::{CaptureCapabilities, CaptureRequest, CaptureService, CapturedFrame, ImageMime};
pub use config::{CaptureConfig, DeviceConfig, LimitsConfig, PairingConfig};
pub use display::{DisplayInfo, DisplaySet, SessionKind};
pub use error::{DeviceError, Result};
pub use protocol::{
    ClientKind, DeviceCapabilities, DeviceMessage, FrameMeta, SessionInit, SessionReady,
    DEVICE_PROTOCOL_VERSION,
};
pub use session::{SessionDescriptor, SessionService};
