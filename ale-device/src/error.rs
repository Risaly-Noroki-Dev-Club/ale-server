//! Error types shared by the headless device service.

pub type Result<T> = std::result::Result<T, DeviceError>;

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("capability unavailable: {0}")]
    Unavailable(String),
    #[error("backend error: {0}")]
    Backend(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("request timed out")]
    Timeout,
    #[error("request cancelled")]
    Cancelled,
    #[error("request denied: {0}")]
    Denied(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl DeviceError {
    /// Stable machine-readable code used on the wire.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Config(_) => "CONFIG_INVALID",
            Self::Unavailable(_) => "CAPABILITY_UNAVAILABLE",
            Self::Backend(_) => "BACKEND_ERROR",
            Self::Protocol(_) => "PROTOCOL_VIOLATION",
            Self::Timeout => "REQUEST_TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::Denied(_) => "DENIED",
            Self::Io(_) => "IO_ERROR",
            Self::Json(_) => "INVALID_JSON",
        }
    }
}
