//! Library half of the `ale-server` binary.
//!
//! The binary (`src/main.rs`) is a thin CLI/runtime wrapper; everything that is
//! worth testing lives here:
//!
//! * [`config`] — TOML configuration, environment overrides, validation and the
//!   redacted summary served by `GET /status`;
//! * [`app`] — axum router, application state, lazy inference engine and the
//!   `/health`, `/status`, `/models`, `/asr`, `/tts`, `/vlm` and device-status
//!   routes;
//! * [`egress`] — allow-listed, size-capped proxy to OpenAI-compatible upstreams;
//! * [`device`] — adapter that wires the `ale-device` terminal channel into the
//!   server without leaking `ale-device` types into the HTTP layer;
//! * [`error`] — the stable JSON error envelope and error codes.
//!
//! See `docs/API.md` for the wire contract.

// `Response` is axum's response type and is large on the egress paths by design;
// returning it in `Err` keeps error mapping in one place.
#![allow(clippy::result_large_err)]

pub mod app;
pub mod browser_tasks;
pub mod config;
pub mod device;
pub mod egress;
pub mod error;
pub mod secrets;
