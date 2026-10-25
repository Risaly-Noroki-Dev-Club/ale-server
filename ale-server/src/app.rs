//! axum application: shared state, lazy inference engine and HTTP routes.
//!
//! | Route | Auth | Notes |
//! | --- | --- | --- |
//! | `GET /health` | none | liveness probe, always `200` while the process serves |
//! | `GET /status` | none | redacted configuration + runtime summary |
//! | `GET /models` | none | configured model catalogue (`503` without engine) |
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
//!
//! The vendored `ale-core` engine is **remote-only**: it performs cloud chat,
//! vision and ASR calls but no longer ships local inference, TTS or a model
//! registry. `/tts/synthesize` therefore calls a downstream OpenAI-compatible
//! cloud TTS endpoint itself (see [`TtsPlan`]), and `/models` derives a
//! read-only catalogue from the engine configuration (cloud entries plus the
//! out-of-process `ale-modeld` entries declared under `model_scheduler`).

use crate::config::{Config, ConfigSource, EngineConfig, LoadedConfig};
use crate::device::{session_to_json, DeviceAccess, DEVICE_STATUS_AUDIT_TAIL};
use crate::egress::EgressState;
use crate::error;
use crate::secrets;
use ale_core::{
    config::{AppConfig as CoreAppConfig, CloudApiConfig},
    model_api::WireApi,
    AleEngine,
};
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
/// Compile-time cap for a downstream cloud TTS audio response. Speech for the
/// maximum text length is a few MiB; anything larger indicates a misbehaving
/// upstream and is rejected instead of being buffered without bound.
pub const MAX_TTS_AUDIO_BYTES: usize = 32 * 1024 * 1024;
/// Path appended to `cloud_api.api_url` for OpenAI-compatible speech synthesis.
const TTS_SPEECH_PATH: &str = "/audio/speech";
/// Downstream cloud TTS is pinned to the OpenAI-compatible speech model/voice;
/// the engine no longer selects a voice (request `voice`/`speed` are ignored).
const TTS_MODEL: &str = "tts-1";
const TTS_VOICE: &str = "alloy";
const TTS_RESPONSE_FORMAT: &str = "wav";
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

    Json(json!({ "models": model_catalog(engine.config()) })).into_response()
}

/// Build the `/models` catalogue from the ale-core configuration.
///
/// `ale-core` exposes configuration only, not a downloaded-model registry, so
/// this is a *configuration view*:
///
/// * `cloud` — the primary cloud chat/vision model when `cloud_api.api_key` is
///   set, plus the independent transcription model when transcription is
///   enabled and configured.
/// * `local-modeld` — the out-of-process models declared under
///   `model_scheduler`; they are listed only while the scheduler is enabled
///   because this host cannot serve them otherwise.
///
/// `size_bytes` is always `null`: the server knows model identifiers, not the
/// size of downloaded files. `purpose` is one of `chat`, `asr`, `planning` or
/// `grounding`; `source` distinguishes cloud entries from `ale-modeld` entries.
/// The legacy fields (`quantization`, `recommended_for`) are kept as `null` so
/// existing clients keep parsing the catalogue.
fn model_catalog(config: &CoreAppConfig) -> Vec<Value> {
    let mut models = Vec::new();
    let mut seen: Vec<&str> = Vec::new();

    let cloud = &config.cloud_api;
    if !cloud.api_key.trim().is_empty() && !cloud.model.trim().is_empty() {
        let model = cloud.model.trim();
        seen.push(model);
        models.push(json!({
            "id": model,
            "name": model,
            "description": format!(
                "Cloud chat/vision model (provider `{}`, wire_api `{}`, base URL `{}`)",
                cloud.provider.trim(),
                cloud.effective_wire_api().id(),
                cloud.api_url.trim()
            ),
            "size_bytes": null,
            "purpose": "chat",
            "quantization": null,
            "recommended_for": null,
            "source": "cloud",
        }));
    }

    let transcription = &config.transcription;
    let endpoint = &transcription.endpoint;
    if transcription.enabled
        && !endpoint.api_key.trim().is_empty()
        && !endpoint.model.trim().is_empty()
        && !seen
            .iter()
            .any(|id| id.eq_ignore_ascii_case(endpoint.model.trim()))
    {
        let model = endpoint.model.trim();
        seen.push(model);
        models.push(json!({
            "id": model,
            "name": model,
            "description": format!(
                "Cloud speech-to-text model (wire_api `{}`, base URL `{}`)",
                endpoint.effective_wire_api().id(),
                endpoint.api_url.trim()
            ),
            "size_bytes": null,
            "purpose": "asr",
            "quantization": null,
            "recommended_for": null,
            "source": "cloud",
        }));
    }

    if config.model_scheduler.enabled {
        let scheduler = &config.model_scheduler;
        for (model, purpose, description) in [
            (
                scheduler.qwen_model.as_str(),
                "planning",
                "Local vision-language planning model served by ale-modeld (GPU-only)",
            ),
            (
                scheduler.qwen_large_model.as_str(),
                "planning",
                "Local large vision-language planning model served by ale-modeld (GPU-only)",
            ),
            (
                scheduler.grounding_model.as_str(),
                "grounding",
                "Local element-grounding model served by ale-modeld",
            ),
        ] {
            let model = model.trim();
            if model.is_empty() || seen.iter().any(|seen| seen.eq_ignore_ascii_case(model)) {
                continue;
            }
            seen.push(model);
            models.push(json!({
                "id": model,
                "name": model,
                "description": description,
                "size_bytes": null,
                "purpose": purpose,
                "quantization": null,
                "recommended_for": null,
                "source": "local-modeld",
            }));
        }
    }

    models
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
    /// Accepted for API compatibility. The engine no longer selects a voice and
    /// downstream cloud TTS pins `tts-1`/`alloy`, so this field has no effect.
    #[serde(default)]
    #[allow(dead_code)]
    voice: Option<String>,
    /// Accepted for API compatibility; has no effect (see `voice`).
    #[serde(default)]
    #[allow(dead_code)]
    speed: Option<f32>,
}

/// Fully resolved downstream cloud TTS request.
struct TtsPlan {
    url: reqwest::Url,
    api_key: String,
    timeout: Duration,
}

/// Why a TTS plan cannot be built from the engine configuration.
#[derive(Debug, PartialEq, Eq)]
enum TtsPlanError {
    /// `cloud_api.api_key` is blank, i.e. cloud TTS is not configured.
    MissingApiKey,
    /// The effective `wire_api` has no OpenAI-compatible speech adapter.
    UnsupportedProtocol(&'static str),
    /// `cloud_api.api_url` is not a usable base URL.
    InvalidEndpoint,
}

impl TtsPlan {
    /// Resolve the downstream TTS endpoint from the engine's cloud configuration.
    ///
    /// Only [`WireApi::OpenaiChatCompletions`] is supported: it is the protocol
    /// for which the OpenAI `/audio/speech` payload is defined. Other wire
    /// protocols are rejected with a stable `501` instead of sending a request
    /// the upstream cannot interpret.
    fn from_cloud_config(config: &CloudApiConfig) -> Result<Self, TtsPlanError> {
        if config.api_key.trim().is_empty() {
            return Err(TtsPlanError::MissingApiKey);
        }
        let wire_api = config.effective_wire_api();
        if wire_api != WireApi::OpenaiChatCompletions {
            return Err(TtsPlanError::UnsupportedProtocol(wire_api.id()));
        }
        let base = config.api_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(TtsPlanError::InvalidEndpoint);
        }
        let url = reqwest::Url::parse(&format!("{base}{TTS_SPEECH_PATH}"))
            .map_err(|_| TtsPlanError::InvalidEndpoint)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(TtsPlanError::InvalidEndpoint);
        }
        Ok(Self {
            url,
            api_key: config.api_key.trim().to_string(),
            timeout: Duration::from_secs(u64::from(config.timeout.clamp(1, 80))),
        })
    }
}

/// Map a [`TtsPlanError`] to the stable HTTP error envelope.
///
/// * empty key → `502 INFERENCE_FAILED`, matching other cloud calls made
///   without a configured key (not a client error);
/// * unsupported protocol → `501 UNSUPPORTED_WIRE_API`;
/// * invalid configured URL → `500 INTERNAL_ERROR`, matching the egress proxy's
///   handling of a broken configured upstream URL.
fn tts_plan_error(error: TtsPlanError) -> Response {
    match error {
        TtsPlanError::MissingApiKey => error::error_response(
            StatusCode::BAD_GATEWAY,
            error::INFERENCE_FAILED,
            "cloud TTS is not configured: ale-core cloud_api.api_key is empty",
        ),
        TtsPlanError::UnsupportedProtocol(wire_api) => error::error_response(
            StatusCode::NOT_IMPLEMENTED,
            error::UNSUPPORTED_WIRE_API,
            format!(
                "cloud TTS requires wire_api `openai_chat_completions`; configured wire_api is `{wire_api}`"
            ),
        ),
        TtsPlanError::InvalidEndpoint => {
            error::internal_error("configured cloud TTS endpoint URL is invalid")
        }
    }
}

/// JSON body for the OpenAI-compatible `/audio/speech` request.
fn tts_request_body(text: &str) -> Value {
    json!({
        "model": TTS_MODEL,
        "input": text,
        "voice": TTS_VOICE,
        "response_format": TTS_RESPONSE_FORMAT,
    })
}

/// Build the HTTP client used for the downstream TTS request.
fn tts_http_client(timeout: Duration) -> Result<reqwest::Client, Response> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(5).min(timeout))
        // No redirects: the configured base URL must stay the destination.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("ale-server/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| error::internal_error("failed to build the cloud TTS HTTP client"))
}

fn tts_upstream_error(message: impl Into<String>) -> Response {
    error::error_response(
        StatusCode::BAD_GATEWAY,
        error::INFERENCE_FAILED,
        truncate(&message.into(), MAX_ERROR_MESSAGE_CHARS),
    )
}

fn tts_audio_too_large() -> Response {
    tts_upstream_error(format!(
        "cloud TTS audio response exceeds the {MAX_TTS_AUDIO_BYTES} byte limit"
    ))
}

/// POST the synthesis request and buffer the audio response with a size cap.
///
/// Transport errors, timeouts and non-2xx upstream statuses become
/// `502 INFERENCE_FAILED`. The upstream body is never forwarded or logged (it
/// may echo prompt text).
async fn request_tts_audio(plan: &TtsPlan, text: &str) -> Result<Vec<u8>, Response> {
    let client = tts_http_client(plan.timeout)?;
    let host = plan.url.host_str().unwrap_or_default().to_string();
    tracing::info!(host, "requesting downstream cloud TTS");
    let response = client
        .post(plan.url.clone())
        .bearer_auth(&plan.api_key)
        .json(&tts_request_body(text))
        .send()
        .await
        .map_err(|error| {
            let kind = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else {
                "request"
            };
            tracing::warn!(host, kind, "cloud TTS upstream request failed");
            tts_upstream_error(format!("cloud TTS upstream request failed ({kind})"))
        })?;

    let status = response.status();
    if !status.is_success() {
        tracing::warn!(
            host,
            status = status.as_u16(),
            "cloud TTS upstream returned an error"
        );
        return Err(tts_upstream_error(format!(
            "cloud TTS upstream returned HTTP {}",
            status.as_u16()
        )));
    }
    if let Some(length) = response.content_length() {
        if length as usize > MAX_TTS_AUDIO_BYTES {
            return Err(tts_audio_too_large());
        }
    }
    let audio = response.bytes().await.map_err(|error| {
        let kind = if error.is_timeout() {
            "timeout"
        } else {
            "read"
        };
        tracing::warn!(host, kind, "failed to read the cloud TTS audio response");
        tts_upstream_error(format!(
            "failed to read the cloud TTS audio response ({kind})"
        ))
    })?;
    if audio.len() > MAX_TTS_AUDIO_BYTES {
        return Err(tts_audio_too_large());
    }
    if audio.is_empty() {
        return Err(tts_upstream_error(
            "cloud TTS upstream returned empty audio",
        ));
    }
    Ok(audio.to_vec())
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

    // Snapshot the cloud configuration and release the engine lock before the
    // network call: synthesis can take seconds and must not serialize the other
    // inference routes behind it.
    let plan = {
        let guard = match state.engine.acquire().await {
            Ok(guard) => guard,
            Err(error) => return engine_error_response(error),
        };
        let Some(engine) = guard.as_ref() else {
            return error::engine_unavailable("inference engine is not initialized");
        };
        match TtsPlan::from_cloud_config(&engine.config().cloud_api) {
            Ok(plan) => plan,
            Err(error) => return tts_plan_error(error),
        }
    };

    match request_tts_audio(&plan, &request.text).await {
        Ok(audio) => Json(tts_audio_response_body(&audio)).into_response(),
        Err(response) => response,
    }
}

/// Success payload for `/tts/synthesize` (format is sniffed from the bytes).
fn tts_audio_response_body(audio: &[u8]) -> Value {
    json!({
        "audio_base64": base64::engine::general_purpose::STANDARD.encode(audio),
        "format": audio_format(audio),
        "bytes": audio.len(),
    })
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

    // `ask_about_image_with_tools` is the surviving engine entry point: it runs
    // the same cloud vision request plus context preparation. No tools are
    // supplied on this route, so `tool_calls` in the response is always `null`;
    // the field stays in the payload for API compatibility.
    match engine
        .ask_about_image_with_tools(image, &question, Vec::new())
        .await
    {
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

// ---------------------------------------------------------------------------
// Pure helpers: unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ale_core::config::TranscriptionConfig;

    fn cloud_config(api_url: &str, wire_api: WireApi) -> CloudApiConfig {
        CloudApiConfig {
            wire_api,
            provider: "custom".to_string(),
            api_key: "test-key".to_string(),
            api_url: api_url.to_string(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
            timeout: 30,
        }
    }

    #[test]
    fn tts_plan_appends_speech_path_and_trims_trailing_slash() {
        let config = cloud_config(
            "https://api.example.com/v1/",
            WireApi::OpenaiChatCompletions,
        );
        let plan = TtsPlan::from_cloud_config(&config).expect("supported configuration");
        assert_eq!(plan.url.as_str(), "https://api.example.com/v1/audio/speech");
        assert_eq!(plan.api_key, "test-key");
        assert_eq!(plan.timeout, Duration::from_secs(30));
    }

    #[test]
    fn tts_plan_rejects_empty_api_key() {
        let mut config = cloud_config("https://api.example.com/v1", WireApi::OpenaiChatCompletions);
        config.api_key = "   ".to_string();
        assert!(matches!(
            TtsPlan::from_cloud_config(&config),
            Err(TtsPlanError::MissingApiKey)
        ));
    }

    #[test]
    fn tts_plan_rejects_non_openai_wire_protocols() {
        for (wire_api, id) in [
            (WireApi::OpenaiResponses, "openai_responses"),
            (WireApi::AnthropicMessages, "anthropic_messages"),
            (WireApi::GoogleGenerateContent, "google_generate_content"),
        ] {
            let config = cloud_config("https://api.example.com/v1", wire_api);
            assert!(
                matches!(
                    TtsPlan::from_cloud_config(&config),
                    Err(TtsPlanError::UnsupportedProtocol(reported)) if reported == id
                ),
                "wire_api {id} must be rejected"
            );
        }
    }

    #[test]
    fn tts_plan_follows_provider_preset_over_explicit_wire_api() {
        // Valid ale-core configurations reject this conflict, but the TTS plan
        // derives the transport from `effective_wire_api` so a preset change can
        // never silently send a Google payload to an OpenAI speech endpoint.
        let mut config = cloud_config("https://api.example.com/v1", WireApi::OpenaiChatCompletions);
        config.provider = "anthropic".to_string();
        assert!(matches!(
            TtsPlan::from_cloud_config(&config),
            Err(TtsPlanError::UnsupportedProtocol("anthropic_messages"))
        ));
    }

    #[test]
    fn tts_plan_rejects_unusable_base_urls() {
        for api_url in [
            "",
            "not a url",
            "ftp://api.example.com/v1",
            "https://user:pass@api.example.com/v1",
            "https://api.example.com/v1?key=secret",
            "https://api.example.com/v1#fragment",
        ] {
            let config = cloud_config(api_url, WireApi::OpenaiChatCompletions);
            assert!(
                matches!(
                    TtsPlan::from_cloud_config(&config),
                    Err(TtsPlanError::InvalidEndpoint)
                ),
                "api_url {api_url:?} must be rejected"
            );
        }
    }

    #[test]
    fn tts_plan_clamps_timeout_to_supported_range() {
        let mut config = cloud_config("https://api.example.com/v1", WireApi::OpenaiChatCompletions);
        config.timeout = 0;
        assert_eq!(
            TtsPlan::from_cloud_config(&config).unwrap().timeout,
            Duration::from_secs(1)
        );
        config.timeout = 999;
        assert_eq!(
            TtsPlan::from_cloud_config(&config).unwrap().timeout,
            Duration::from_secs(80)
        );
    }

    #[test]
    fn tts_request_body_is_pinned_to_openai_speech_shape() {
        let body = tts_request_body("hello world");
        assert_eq!(body["model"], "tts-1");
        assert_eq!(body["voice"], "alloy");
        assert_eq!(body["response_format"], "wav");
        assert_eq!(body["input"], "hello world");
        assert_eq!(body.as_object().unwrap().len(), 4);
    }

    #[test]
    fn tts_audio_response_body_sniffs_wav_and_reports_length() {
        let wav = b"RIFF\x00\x00\x00\x00WAVEfmt ";
        let body = tts_audio_response_body(wav);
        assert_eq!(body["format"], "wav");
        assert_eq!(body["bytes"], wav.len());
        assert_eq!(
            body["audio_base64"],
            base64::engine::general_purpose::STANDARD.encode(wav)
        );
    }

    #[test]
    fn model_catalog_lists_cloud_and_local_entries() {
        let mut config = CoreAppConfig::default();
        config.cloud_api.api_key = "sk-test".to_string();
        config.cloud_api.model = "gpt-4o".to_string();
        config.transcription = TranscriptionConfig {
            enabled: true,
            endpoint: CloudApiConfig {
                api_key: "asr-key".to_string(),
                model: "whisper-1".to_string(),
                ..Default::default()
            },
        };

        let models = model_catalog(&config);
        let ids: Vec<&str> = models
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids[0], "gpt-4o");
        assert_eq!(models[0]["source"], "cloud");
        assert_eq!(models[0]["purpose"], "chat");
        assert!(models[0]["size_bytes"].is_null());
        assert_eq!(ids[1], "whisper-1");
        assert_eq!(models[1]["purpose"], "asr");
        assert_eq!(models[1]["source"], "cloud");
        // Defaults from ModelSchedulerConfig: qwen, qwen large, grounding.
        assert_eq!(ids.len(), 5);
        assert_eq!(models[2]["id"], "Qwen2.5-VL-3B-Instruct");
        assert_eq!(models[2]["purpose"], "planning");
        assert_eq!(models[2]["source"], "local-modeld");
        assert_eq!(models[3]["purpose"], "planning");
        assert_eq!(models[4]["id"], "ShowUI-2B");
        assert_eq!(models[4]["purpose"], "grounding");
    }

    #[test]
    fn model_catalog_hides_unconfigured_and_disabled_entries() {
        let mut config = CoreAppConfig::default();
        config.model_scheduler.enabled = false;
        assert!(model_catalog(&config).is_empty());

        config.model_scheduler.enabled = true;
        config.cloud_api.api_key = String::new();
        config.cloud_api.model = "gpt-4o".to_string();
        let models = model_catalog(&config);
        assert_eq!(models.len(), 3);
        assert!(models.iter().all(|model| model["source"] == "local-modeld"));
        assert!(models.iter().all(|model| model["size_bytes"].is_null()));
    }

    #[test]
    fn model_catalog_deduplicates_local_ids() {
        let mut config = CoreAppConfig::default();
        config.model_scheduler.qwen_model = "same-model".to_string();
        config.model_scheduler.qwen_large_model = "same-model".to_string();
        config.model_scheduler.grounding_model = "Same-Model".to_string();
        let models = model_catalog(&config);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["id"], "same-model");
    }
}
