//! axum application: shared state, lazy inference engine and HTTP routes.
//!
//! | Route | Auth | Notes |
//! | --- | --- | --- |
//! | `GET /health` | none | liveness probe, always `200` while the process serves |
//! | `GET /status` | none | redacted configuration + runtime summary |
//! | `GET /models` | none | inference model catalogue (`503` without engine) |
//! | `POST /asr/transcribe` | none | multipart audio → text |
//! | `POST /tts/synthesize` | none | JSON text → base64 audio |
//! | `POST /vlm/describe` | none | multipart image → description |
//! | `POST /vlm/ask` | none | multipart image + question → answer |
//! | `GET /v1/device/status` | loopback only | pairing expiry, sessions, audit tail |
//! | `POST /v1/egress/*` | Bearer token | allow-listed upstream proxy (see [`crate::egress`]) |
//!
//! The inference engine is created lazily on the first inference request inside a
//! [`tokio::sync::Mutex`]. Initialization failures (missing model files, no
//! D-Bus secret service, …) are logged and turned into `503 ENGINE_UNAVAILABLE`;
//! they must never crash the process, because `/health` and the device channel
//! have to keep working.

use crate::config::{Config, ConfigSource, EngineConfig, LoadedConfig};
use crate::device::{session_to_json, DeviceAccess, DEVICE_STATUS_AUDIT_TAIL};
use crate::egress::EgressState;
use crate::error;
use crate::secrets;
use ale_core::AleEngine;
use axum::{
    extract::{
        multipart::MultipartError, rejection::JsonRejection, ConnectInfo, DefaultBodyLimit,
        Multipart, State,
    },
    http::{header, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, MutexGuard};
use tower_http::{
    catch_panic::CatchPanicLayer,
    cors::{AllowOrigin, CorsLayer},
    trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer},
};
use tracing::Level;

/// Compile-time cap for JSON request bodies (`/tts/synthesize`).
pub const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;
/// Compile-time cap for multipart audio uploads (`/asr/transcribe`).
pub const MAX_AUDIO_BODY_BYTES: usize = 32 * 1024 * 1024;
/// Compile-time cap for multipart image uploads (`/vlm/*`).
pub const MAX_IMAGE_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of characters accepted by `/tts/synthesize`.
pub const MAX_TTS_TEXT_CHARS: usize = 4096;
/// How long engine initialization may take before it is treated as failed.
const ENGINE_INIT_TIMEOUT: Duration = Duration::from_secs(120);
/// After a failed initialization, wait this long before retrying (per request).
const ENGINE_RETRY_COOLDOWN: Duration = Duration::from_secs(30);
/// Error messages returned to clients are truncated to this length.
const MAX_ERROR_MESSAGE_CHARS: usize = 300;

/// Result of acquiring the inference engine.
#[derive(Debug, Clone)]
pub enum EngineError {
    /// `[engine].enabled = false`.
    Disabled,
    /// Enabled, but initialization failed (or timed out).
    Unavailable(String),
}

impl EngineError {
    fn public_message(&self) -> String {
        match self {
            Self::Disabled => "inference engine is disabled in the configuration".to_string(),
            Self::Unavailable(message) => {
                format!("inference engine unavailable: {}", truncate(message, 200))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct EngineFailure {
    message: String,
    at: Instant,
}

/// Lazily initialized inference engine shared by every HTTP handler.
///
/// The engine itself lives in `Arc<tokio::sync::Mutex<Option<AleEngine>>>`; an
/// initialization failure is kept in `failure` so `/health` can report
/// `degraded` without retrying the (potentially expensive) initialization on
/// every health probe. Retries are rate limited by [`ENGINE_RETRY_COOLDOWN`].
#[derive(Clone)]
pub struct EngineState {
    config: Arc<EngineConfig>,
    engine: Arc<Mutex<Option<AleEngine>>>,
    init_lock: Arc<Mutex<()>>,
    failure: Arc<std::sync::Mutex<Option<EngineFailure>>>,
    ready: Arc<AtomicBool>,
}

impl EngineState {
    pub fn new(config: &EngineConfig) -> Self {
        Self {
            config: Arc::new(config.clone()),
            engine: Arc::new(Mutex::new(None)),
            init_lock: Arc::new(Mutex::new(())),
            failure: Arc::new(std::sync::Mutex::new(None)),
            ready: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Whether the engine was successfully initialized.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Last initialization failure, if any (used by `/health` and `/status`).
    pub fn last_error(&self) -> Option<String> {
        self.failure
            .lock()
            .ok()
            .and_then(|failure| failure.as_ref().map(|failure| failure.message.clone()))
    }

    /// Acquire the engine, initializing it on first use.
    ///
    /// Initialization is serialized by `init_lock`; concurrent first requests
    /// wait for one attempt instead of stampeding model loading.
    pub async fn acquire(&self) -> Result<MutexGuard<'_, Option<AleEngine>>, EngineError> {
        if !self.config.enabled {
            return Err(EngineError::Disabled);
        }

        {
            let guard = self.engine.lock().await;
            if guard.is_some() {
                return Ok(guard);
            }
        }

        if let Some(failure) = self.failure.lock().ok().and_then(|failure| failure.clone()) {
            if failure.at.elapsed() < ENGINE_RETRY_COOLDOWN {
                return Err(EngineError::Unavailable(failure.message));
            }
        }

        let _init = self.init_lock.lock().await;
        {
            let guard = self.engine.lock().await;
            if guard.is_some() {
                return Ok(guard);
            }
        }

        match self.init_engine().await {
            Ok(engine) => {
                let mut guard = self.engine.lock().await;
                *guard = Some(engine);
                self.ready.store(true, Ordering::Release);
                if let Ok(mut failure) = self.failure.lock() {
                    *failure = None;
                }
                tracing::info!("inference engine initialized");
                Ok(guard)
            }
            Err(message) => {
                tracing::error!(
                    error = %message,
                    "inference engine initialization failed; inference routes will answer 503"
                );
                if let Ok(mut failure) = self.failure.lock() {
                    *failure = Some(EngineFailure {
                        message: message.clone(),
                        at: Instant::now(),
                    });
                }
                Err(EngineError::Unavailable(message))
            }
        }
    }

    async fn init_engine(&self) -> Result<AleEngine, String> {
        // Resolve the headless-safe credential store before touching ale-core:
        // on a system without a D-Bus Secret Service, `auto` falls back to a
        // private file store instead of failing engine initialization.
        let secret_store = secrets::resolve_secret_store(&self.config).await;
        let config_path = self.config.resolved_config_path();
        tracing::info!(
            path = %config_path.display(),
            secret_store = self.config.secret_store.as_str(),
            "initializing inference engine"
        );
        let create = AleEngine::new_with_secret_store(&config_path, secret_store);
        match tokio::time::timeout(ENGINE_INIT_TIMEOUT, create).await {
            Ok(Ok(engine)) => Ok(engine),
            Ok(Err(error)) => Err(format!("engine creation failed: {error}")),
            Err(_) => Err(format!(
                "engine initialization timed out after {}s",
                ENGINE_INIT_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub engine: EngineState,
    pub egress: Option<EgressState>,
    pub device: Option<Arc<dyn DeviceAccess>>,
    pub started: Instant,
    pub config_source: ConfigSource,
    pub config_path: PathBuf,
}

impl AppState {
    /// Build state from a fully loaded configuration.
    pub fn from_loaded(
        loaded: &LoadedConfig,
        egress: Option<EgressState>,
        device: Option<Arc<dyn DeviceAccess>>,
    ) -> Self {
        Self::assemble(
            loaded.config.clone(),
            egress,
            device,
            loaded.path.clone(),
            loaded.source,
        )
    }

    /// Build state from an in-memory configuration (tests, embedded use).
    pub fn from_config(
        config: Config,
        egress: Option<EgressState>,
        device: Option<Arc<dyn DeviceAccess>>,
    ) -> Self {
        Self::assemble(
            config,
            egress,
            device,
            PathBuf::from(crate::config::DEFAULT_CONFIG_PATH),
            ConfigSource::Defaults,
        )
    }

    fn assemble(
        config: Config,
        egress: Option<EgressState>,
        device: Option<Arc<dyn DeviceAccess>>,
        config_path: PathBuf,
        config_source: ConfigSource,
    ) -> Self {
        let engine = EngineState::new(&config.engine);
        Self {
            config: Arc::new(config),
            engine,
            egress,
            device,
            started: Instant::now(),
            config_source,
            config_path,
        }
    }

    /// Whole seconds since the process state was created.
    pub fn uptime_s(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// Whether the device terminal channel is running.
    pub fn device_ready(&self) -> bool {
        self.device.is_some()
    }
}

/// Build the complete router for `state`.
pub fn build_router(state: AppState) -> Router {
    let cors = cors_layer(&state.config.server.cors_origin);
    let egress_body_limit = state.config.egress.max_body_bytes;

    let router: Router<AppState> = Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/models", get(models))
        .route(
            "/asr/transcribe",
            post(asr_transcribe).layer(DefaultBodyLimit::max(MAX_AUDIO_BODY_BYTES)),
        )
        .route(
            "/tts/synthesize",
            post(tts_synthesize).layer(DefaultBodyLimit::max(MAX_JSON_BODY_BYTES)),
        )
        .route(
            "/vlm/describe",
            post(vlm_describe).layer(DefaultBodyLimit::max(MAX_IMAGE_BODY_BYTES)),
        )
        .route(
            "/vlm/ask",
            post(vlm_ask).layer(DefaultBodyLimit::max(MAX_IMAGE_BODY_BYTES)),
        )
        .route("/v1/device/status", get(device_status))
        .merge(crate::egress::router(egress_body_limit))
        .fallback(not_found);

    let router = match cors {
        Some(cors) => router.layer(cors),
        None => router,
    };

    router
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_response(DefaultOnResponse::new().level(Level::INFO)),
        )
        .layer(CatchPanicLayer::custom(handle_panic))
        .with_state(state)
}

fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        // No permissive default: without an explicit allow-list no CORS headers
        // are emitted at all.
        return None;
    }
    let values: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.trim_end_matches('/').parse::<HeaderValue>().ok())
        .collect();
    if values.is_empty() {
        return None;
    }
    Some(
        CorsLayer::new()
            .allow_origin(AllowOrigin::list(values))
            .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
            .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
    )
}

fn handle_panic(_panic: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("panic caught in HTTP handler; returning 500");
    error::internal_error("internal server error")
}

fn truncate(message: &str, limit: usize) -> String {
    if message.chars().count() <= limit {
        return message.to_string();
    }
    let mut out: String = message.chars().take(limit).collect();
    out.push('…');
    out
}

fn engine_error_response(error: EngineError) -> Response {
    error::engine_unavailable(error.public_message())
}

fn inference_error(error: ale_core::AleError) -> Response {
    tracing::warn!(error = %error, "inference request failed");
    error::error_response(
        StatusCode::BAD_GATEWAY,
        error::INFERENCE_FAILED,
        truncate(
            &format!("inference failed: {error}"),
            MAX_ERROR_MESSAGE_CHARS,
        ),
    )
}

fn invalid_request(message: impl Into<String>) -> Response {
    error::error_response(
        StatusCode::BAD_REQUEST,
        error::INVALID_REQUEST,
        message.into(),
    )
}

// ---------------------------------------------------------------------------
// /health, /status, /models
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
    engine_enabled: bool,
    engine_ready: bool,
    engine_error: Option<String>,
    device_enabled: bool,
    device_ready: bool,
    uptime_s: u64,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    let engine_error = state.engine.last_error();
    let degraded =
        state.config.engine.enabled && !state.engine.is_ready() && engine_error.is_some();
    Json(HealthResponse {
        status: if degraded { "degraded" } else { "ok" },
        version: env!("CARGO_PKG_VERSION"),
        engine_enabled: state.config.engine.enabled,
        engine_ready: state.engine.is_ready(),
        engine_error: engine_error.map(|message| truncate(&message, MAX_ERROR_MESSAGE_CHARS)),
        device_enabled: state.config.device.enabled,
        device_ready: state.device_ready(),
        uptime_s: state.uptime_s(),
    })
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    let engine_error = state
        .engine
        .last_error()
        .map(|message| truncate(&message, MAX_ERROR_MESSAGE_CHARS));
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_s": state.uptime_s(),
        "config_source": state.config_source,
        "config_path": state.config_path.display().to_string(),
        "config": state.config.to_redacted_json(),
        "runtime": {
            "engine_enabled": state.config.engine.enabled,
            "engine_ready": state.engine.is_ready(),
            "engine_error": engine_error,
            "device_enabled": state.config.device.enabled,
            "device_ready": state.device_ready(),
            "egress_enabled": state.config.egress.enabled,
            "egress_ready": state.egress.is_some(),
        },
    }))
}

async fn models(State(state): State<AppState>) -> Response {
    let guard = match state.engine.acquire().await {
        Ok(guard) => guard,
        Err(error) => return engine_error_response(error),
    };
    let Some(engine) = guard.as_ref() else {
        return error::engine_unavailable("inference engine is not initialized");
    };

    let models = engine.available_models().await;
    let models: Vec<Value> = models
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                "name": model.name,
                "description": model.description,
                "size_bytes": model.size,
                "purpose": model.purpose,
                "quantization": model.quantization,
                "recommended_for": model.recommended_for,
            })
        })
        .collect();
    Json(json!({ "models": models })).into_response()
}

// ---------------------------------------------------------------------------
// Multipart helpers
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct FormField {
    name: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

#[derive(Debug, Default)]
struct FormData {
    fields: Vec<FormField>,
}

impl FormData {
    /// First field whose name matches one of `names`.
    fn first(&self, names: &[&str]) -> Option<&FormField> {
        names.iter().find_map(|name| {
            self.fields
                .iter()
                .find(|field| !field.bytes.is_empty() && field.name == *name)
        })
    }

    fn first_bytes(&self, names: &[&str]) -> Option<&[u8]> {
        self.first(names).map(|field| field.bytes.as_slice())
    }

    fn first_text(&self, names: &[&str]) -> Option<String> {
        self.first(names)
            .and_then(|field| String::from_utf8(field.bytes.clone()).ok())
            .map(|text| text.trim().to_string())
    }

    fn field(&self, name: &str) -> Option<&FormField> {
        self.fields.iter().find(|field| field.name == name)
    }
}

async fn collect_multipart(
    mut multipart: Multipart,
    max_field_bytes: usize,
) -> Result<FormData, Response> {
    let mut form = FormData::default();
    loop {
        let next = multipart
            .next_field()
            .await
            .map_err(|error| multipart_error(error, "<body>"))?;
        let Some(field) = next else {
            break;
        };
        let name = field.name().unwrap_or_default().to_string();
        let content_type = field.content_type().map(str::to_string);
        let bytes = field
            .bytes()
            .await
            .map_err(|error| multipart_error(error, &name))?;
        if bytes.len() > max_field_bytes {
            return Err(error::payload_too_large(format!(
                "multipart field `{name}` exceeds the {max_field_bytes} byte limit"
            )));
        }
        form.fields.push(FormField {
            name,
            content_type,
            bytes: bytes.to_vec(),
        });
    }
    Ok(form)
}

fn multipart_error(error: MultipartError, field: &str) -> Response {
    let status = error.status();
    let message = format!("invalid multipart field `{field}`: {}", error.body_text());
    if status == StatusCode::PAYLOAD_TOO_LARGE {
        error::payload_too_large(message)
    } else {
        error::error_response(status, error::INVALID_MULTIPART, message)
    }
}

// ---------------------------------------------------------------------------
// /asr, /tts, /vlm
// ---------------------------------------------------------------------------

async fn asr_transcribe(State(state): State<AppState>, multipart: Multipart) -> Response {
    let guard = match state.engine.acquire().await {
        Ok(guard) => guard,
        Err(error) => return engine_error_response(error),
    };
    let form = match collect_multipart(multipart, MAX_AUDIO_BODY_BYTES).await {
        Ok(form) => form,
        Err(response) => return response,
    };
    let Some(audio) = form.first_bytes(&["audio", "file"]) else {
        return invalid_request("missing multipart field `audio` (or `file`)");
    };
    if audio.len() > MAX_AUDIO_BODY_BYTES {
        return error::payload_too_large(format!(
            "audio upload exceeds the {MAX_AUDIO_BODY_BYTES} byte limit"
        ));
    }
    let Some(engine) = guard.as_ref() else {
        return error::engine_unavailable("inference engine is not initialized");
    };

    match engine.transcribe(audio).await {
        Ok(text) => Json(json!({ "text": text })).into_response(),
        Err(error) => inference_error(error),
    }
}

#[derive(Debug, Deserialize)]
struct TtsRequest {
    text: String,
    /// Accepted for API compatibility; voice selection is part of the engine
    /// configuration today.
    #[serde(default)]
    #[allow(dead_code)]
    voice: Option<String>,
    /// Accepted for API compatibility; see `voice`.
    #[serde(default)]
    #[allow(dead_code)]
    speed: Option<f32>,
}

async fn tts_synthesize(
    State(state): State<AppState>,
    payload: Result<Json<TtsRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            let status = rejection.status();
            let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                error::PAYLOAD_TOO_LARGE
            } else {
                error::INVALID_JSON
            };
            return error::error_response(status, code, truncate(&rejection.body_text(), 200));
        }
    };
    if request.text.trim().is_empty() {
        return invalid_request("`text` must not be empty");
    }
    if request.text.chars().count() > MAX_TTS_TEXT_CHARS {
        return invalid_request(format!(
            "`text` must be at most {MAX_TTS_TEXT_CHARS} characters"
        ));
    }

    let guard = match state.engine.acquire().await {
        Ok(guard) => guard,
        Err(error) => return engine_error_response(error),
    };
    let Some(engine) = guard.as_ref() else {
        return error::engine_unavailable("inference engine is not initialized");
    };

    match engine.synthesize(&request.text).await {
        Ok(audio) => {
            let format = audio_format(&audio);
            Json(json!({
                "audio_base64": base64::engine::general_purpose::STANDARD.encode(&audio),
                "format": format,
                "bytes": audio.len(),
            }))
            .into_response()
        }
        Err(error) => inference_error(error),
    }
}

async fn vlm_describe(State(state): State<AppState>, multipart: Multipart) -> Response {
    let guard = match state.engine.acquire().await {
        Ok(guard) => guard,
        Err(error) => return engine_error_response(error),
    };
    let form = match collect_multipart(multipart, MAX_IMAGE_BODY_BYTES).await {
        Ok(form) => form,
        Err(response) => return response,
    };
    let Some(image) = form.first_bytes(&["image", "file"]) else {
        return invalid_request("missing multipart field `image` (or `file`)");
    };
    if image.len() > MAX_IMAGE_BODY_BYTES {
        return error::payload_too_large(format!(
            "image upload exceeds the {MAX_IMAGE_BODY_BYTES} byte limit"
        ));
    }
    let Some(engine) = guard.as_ref() else {
        return error::engine_unavailable("inference engine is not initialized");
    };

    match engine.describe_image(image).await {
        Ok(description) => Json(json!({ "description": description })).into_response(),
        Err(error) => inference_error(error),
    }
}

async fn vlm_ask(State(state): State<AppState>, multipart: Multipart) -> Response {
    let guard = match state.engine.acquire().await {
        Ok(guard) => guard,
        Err(error) => return engine_error_response(error),
    };
    let form = match collect_multipart(multipart, MAX_IMAGE_BODY_BYTES).await {
        Ok(form) => form,
        Err(response) => return response,
    };
    let Some(image) = form.first_bytes(&["image", "file"]) else {
        return invalid_request("missing multipart field `image`");
    };
    if image.len() > MAX_IMAGE_BODY_BYTES {
        return error::payload_too_large(format!(
            "image upload exceeds the {MAX_IMAGE_BODY_BYTES} byte limit"
        ));
    }
    let Some(question) = form.first_text(&["question", "prompt"]) else {
        return invalid_request("missing multipart text field `question`");
    };
    if question.is_empty() {
        return invalid_request("`question` must not be empty");
    }
    // Keep the per-field metadata visible to the engine request path.
    let _image_mime = form
        .field("image")
        .and_then(|field| field.content_type.clone());
    let Some(engine) = guard.as_ref() else {
        return error::engine_unavailable("inference engine is not initialized");
    };

    match engine.ask_about_image(image, &question).await {
        Ok(vision) => match serde_json::to_value(&vision) {
            Ok(value) => Json(value).into_response(),
            Err(_) => error::internal_error("failed to encode vision response"),
        },
        Err(error) => inference_error(error),
    }
}

fn audio_format(audio: &[u8]) -> &'static str {
    if audio.starts_with(b"RIFF") && audio.get(8..12) == Some(b"WAVE") {
        "wav"
    } else if audio.starts_with(b"OggS") {
        "ogg"
    } else if audio.starts_with(b"ID3") || audio.starts_with(&[0xFF, 0xFB]) {
        "mp3"
    } else if audio.starts_with(b"fLaC") {
        "flac"
    } else {
        "bin"
    }
}

// ---------------------------------------------------------------------------
// Device admin status (loopback only)
// ---------------------------------------------------------------------------

async fn device_status(
    State(state): State<AppState>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
) -> Response {
    if !remote.ip().is_loopback() {
        tracing::warn!(remote = %remote, "rejected non-loopback /v1/device/status request");
        return error::forbidden(
            error::FORBIDDEN,
            "device status is only available over loopback",
        );
    }
    let Some(device) = state.device.as_ref() else {
        return error::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            error::DEVICE_DISABLED,
            "device terminal channel is disabled",
        );
    };

    let pairing = device.pairing_info();
    let sessions: Vec<Value> = device.sessions().iter().map(session_to_json).collect();
    let audit = device.audit_tail(DEVICE_STATUS_AUDIT_TAIL);
    Json(json!({
        "device_enabled": true,
        "device_ready": true,
        "pairing": {
            "code": pairing.code,
            "expires_in_s": pairing.expires_in_s,
        },
        "sessions": sessions,
        "audit_tail": audit,
        "audit_capacity": crate::device::AUDIT_CAPACITY,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Fallback
// ---------------------------------------------------------------------------

async fn not_found(method: Method, uri: axum::http::Uri) -> Response {
    error::error_response(
        StatusCode::NOT_FOUND,
        error::NOT_FOUND,
        format!("no route for {} {}", method, uri.path()),
    )
}
