//! Runtime configuration for the headless device service.
//!
//! This module is owned by the Lead. It defines the configuration schema that the
//! server binary loads from `ale-server.toml`; do not change field names without
//! updating `docs/CONTRACTS.md` and `deploy/config/ale-server.toml`.

use crate::error::{DeviceError, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Default port of the encrypted terminal channel.
pub const DEFAULT_DEVICE_PORT: u16 = 37_655;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    /// Enable the encrypted terminal channel (WS) listener.
    pub enabled: bool,
    /// Address the terminal channel binds to.
    pub bind: String,
    pub pairing: PairingConfig,
    pub capture: CaptureConfig,
    pub automation: AutomationConfig,
    pub limits: LimitsConfig,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind: format!("0.0.0.0:{DEFAULT_DEVICE_PORT}"),
            pairing: PairingConfig::default(),
            capture: CaptureConfig::default(),
            automation: AutomationConfig::default(),
            limits: LimitsConfig::default(),
        }
    }
}

impl DeviceConfig {
    pub fn validate(&self) -> Result<()> {
        self.socket_addr()?;
        self.pairing.validate()?;
        self.capture.validate()?;
        self.automation.validate()?;
        self.limits.validate()?;
        Ok(())
    }

    pub fn socket_addr(&self) -> Result<SocketAddr> {
        self.bind
            .parse()
            .map_err(|_| DeviceError::Config(format!("invalid device bind address: {}", self.bind)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PairingConfig {
    /// Allow new pairing attempts at all. Existing paired sessions keep working.
    pub enabled: bool,
    /// Lifetime of one pairing code.
    pub code_ttl_s: u64,
    /// Failed handshakes allowed per client address before backing off.
    pub max_attempts: u32,
    /// Token lifetime for the HTTP egress API obtained during pairing.
    pub token_ttl_s: u64,
}

impl Default for PairingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            code_ttl_s: 300,
            max_attempts: 5,
            token_ttl_s: 86_400,
        }
    }
}

impl PairingConfig {
    fn validate(&self) -> Result<()> {
        if self.code_ttl_s == 0 || self.code_ttl_s > 3_600 {
            return Err(DeviceError::Config(
                "pairing.code_ttl_s must be within 1..=3600".into(),
            ));
        }
        if self.max_attempts == 0 || self.max_attempts > 100 {
            return Err(DeviceError::Config(
                "pairing.max_attempts must be within 1..=100".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Force a capture backend (`grim`, `spectacle`, `gnome-screenshot`, `import`,
    /// `scrot`, `screencapture`). `None` auto-detects.
    pub preferred_backend: Option<String>,
    /// Screenshots are downscaled so their width never exceeds this value.
    pub max_width: u32,
    /// JPEG quality for lossy screenshots.
    pub jpeg_quality: u8,
    /// Hard timeout for one capture command.
    pub timeout_ms: u64,
    /// Reuse a capture younger than this value for `capture` (non-`now`) calls.
    pub cache_ttl_ms: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            preferred_backend: None,
            max_width: 1_600,
            jpeg_quality: 78,
            timeout_ms: 5_000,
            cache_ttl_ms: 0,
        }
    }
}

impl CaptureConfig {
    fn validate(&self) -> Result<()> {
        if self.max_width < 320 || self.max_width > 7_680 {
            return Err(DeviceError::Config(
                "capture.max_width must be within 320..=7680".into(),
            ));
        }
        if !(30..=100).contains(&self.jpeg_quality) {
            return Err(DeviceError::Config(
                "capture.jpeg_quality must be within 30..=100".into(),
            ));
        }
        if self.timeout_ms < 200 || self.timeout_ms > 60_000 {
            return Err(DeviceError::Config(
                "capture.timeout_ms must be within 200..=60000".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AutomationConfig {
    /// Force an automation backend (`xdotool`, `ydotool`, `dotool`, `wtype`).
    pub preferred_backend: Option<String>,
    /// Allow actions that touch the file system or close applications.
    pub allow_high_risk: bool,
    /// Allow opening applications and URLs.
    pub allow_process_actions: bool,
    /// Log actions instead of executing them.
    pub dry_run: bool,
    /// Hard timeout for one action plan.
    pub timeout_ms: u64,
    /// Maximum number of actions accepted in one plan.
    pub max_actions: usize,
    /// A plan that references a screenshot older than this is rejected.
    pub frame_max_age_ms: u64,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            preferred_backend: None,
            allow_high_risk: false,
            allow_process_actions: false,
            dry_run: false,
            timeout_ms: 30_000,
            max_actions: 20,
            frame_max_age_ms: 5_000,
        }
    }
}

impl AutomationConfig {
    fn validate(&self) -> Result<()> {
        if !(1..=20).contains(&self.max_actions) {
            return Err(DeviceError::Config(
                "automation.max_actions must be within 1..=20".into(),
            ));
        }
        if self.timeout_ms < 500 || self.timeout_ms > 300_000 {
            return Err(DeviceError::Config(
                "automation.timeout_ms must be within 500..=300000".into(),
            ));
        }
        if self.frame_max_age_ms < 500 || self.frame_max_age_ms > 120_000 {
            return Err(DeviceError::Config(
                "automation.frame_max_age_ms must be within 500..=120000".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub max_connections: usize,
    pub max_concurrent_requests: usize,
    /// Largest accepted single protocol message after decryption.
    pub max_message_bytes: usize,
    pub idle_timeout_s: u64,
    pub request_timeout_s: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 4,
            max_concurrent_requests: 2,
            max_message_bytes: 8 * 1024 * 1024,
            idle_timeout_s: 120,
            request_timeout_s: 90,
        }
    }
}

impl LimitsConfig {
    fn validate(&self) -> Result<()> {
        if !(1..=32).contains(&self.max_connections) {
            return Err(DeviceError::Config(
                "limits.max_connections must be within 1..=32".into(),
            ));
        }
        if !(1..=16).contains(&self.max_concurrent_requests) {
            return Err(DeviceError::Config(
                "limits.max_concurrent_requests must be within 1..=16".into(),
            ));
        }
        if !(64 * 1024..=64 * 1024 * 1024).contains(&self.max_message_bytes) {
            return Err(DeviceError::Config(
                "limits.max_message_bytes must be within 64 KiB..=64 MiB".into(),
            ));
        }
        if self.idle_timeout_s < 10 || self.idle_timeout_s > 3_600 {
            return Err(DeviceError::Config(
                "limits.idle_timeout_s must be within 10..=3600".into(),
            ));
        }
        if self.request_timeout_s < 5 || self.request_timeout_s > 600 {
            return Err(DeviceError::Config(
                "limits.request_timeout_s must be within 5..=600".into(),
            ));
        }
        Ok(())
    }
}
