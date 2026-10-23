//! Allow-listed, size-capped egress proxy to OpenAI-compatible upstreams.
//!
//! # Security model
//!
//! * **Authentication** — every route requires
//!   `Authorization: Bearer <token>` where the token was issued in the device
//!   channel's `SessionReady` message and is validated by
//!   [`ale_device::SessionService::validate_token`] (through [`crate::device::DeviceAccess`]).
//! * **Policy switch** — `[egress].enabled = false` answers `403 EGRESS_DISABLED`;
//!   the route table is registered either way so the denial is explicit.
//! * **Host allow-list** — the resolved upstream URL host must exactly match one
//!   of `[egress].allowed_hosts` (case-insensitive). The check runs on the parsed
//!   URL, never on a client-supplied string.
//! * **No redirects** — the reqwest client uses `redirect::Policy::none()`, so a
//!   `302` cannot move the request to a non-allow-listed host.
//! * **Credentials** — the upstream API key is read from the environment
//!   variable named by `[egress].api_key_env` on each request and injected as a
//!   Bearer header. It is never stored in the config, `/status` or logs.
//! * **Size caps** — request bodies are limited by `[egress].max_body_bytes`
//!   (`413`), upstream responses by `[egress].max_response_bytes` (`502
//!   EGRESS_RESPONSE_TOO_LARGE`).
//! * **Logging** — only route, upstream host, status and byte counts are logged;
//!   request/response bodies and credentials never are.
//!
//! Request JSON is mapped to OpenAI-compatible endpoints:
//!
//! | Route | Upstream |
//! | --- | --- |
//! | `POST /v1/egress/chat` | `POST /v1/chat/completions` (body forwarded) |
//! | `POST /v1/egress/vision` | `POST /v1/chat/completions` (`image_url` data URL) |
//! | `POST /v1/egress/tts` | `POST /v1/audio/speech` (raw audio response) |
//! | `POST /v1/egress/asr` | `POST /v1/audio/transcriptions` (multipart) |

use crate::app::AppState;
use crate::config::EgressConfig;
use crate::error;
use ale_device::SessionDescriptor;
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use base64::Engine as _;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

/// OpenAI-compatible chat/completions endpoint.
pub const CHAT_PATH: &str = "/v1/chat/completions";
/// OpenAI-compatible speech synthesis endpoint.
pub const SPEECH_PATH: &str = "/v1/audio/speech";
/// OpenAI-compatible transcription endpoint.
pub const TRANSCRIPTIONS_PATH: &str = "/v1/audio/transcriptions";

/// Upper bound for error text copied from an upstream response.
const MAX_UPSTREAM_ERROR_CHARS: usize = 200;

/// Egress runtime: a hardened reqwest client plus the immutable policy config.
#[derive(Clone)]
pub struct EgressState {
    client: reqwest::Client,
    config: Arc<EgressConfig>,
}

impl EgressState {
    /// Build the HTTP client. Fails only on invalid TLS/reqwest configuration.
    pub fn new(config: &EgressConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_s))
            // Never follow redirects: a redirect could escape the host allow-list.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ale-server/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| format!("failed to build the egress HTTP client: {error}"))?;
        Ok(Self {
            client,
            config: Arc::new(config.clone()),
        })
    }

    pub fn config(&self) -> &EgressConfig {
        &self.config
    }

    /// Resolve `path` against the configured upstream and enforce the allow-list.
    fn resolve(&self, path: &str) -> Result<reqwest::Url, Response> {
        let base = self.config.upstream.trim_end_matches('/');
        let url = reqwest::Url::parse(&format!("{base}{path}"))
            .map_err(|_| error::internal_error("configured egress upstream URL is invalid"))?;
        let host = url
            .host_str()
            .ok_or_else(|| error::internal_error("configured egress upstream URL has no host"))?;
        if !self.config.host_allowed(host) {
            return Err(error::forbidden(
                error::EGRESS_DENIED,
                format!("upstream host `{host}` is not allow-listed"),
            ));
        }
        Ok(url)
    }

    /// Read the upstream credential from the configured environment variable.
    fn api_key(&self) -> Result<String, Response> {
        self.config.api_key().ok_or_else(|| {
            error::error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                error::EGRESS_NOT_CONFIGURED,
                format!(
                    "upstream credential environment variable `{}` is not set",
                    self.config.api_key_env
                ),
            )
        })
    }
}

/// Egress routes, mounted by the main router.
///
/// `max_body_bytes` comes from `[egress].max_body_bytes`; the handlers also
/// enforce it manually so that oversized requests fail with the JSON envelope.
pub fn router(max_body_bytes: usize) -> Router<AppState> {
    Router::new()
        .route("/v1/egress/chat", post(chat))
        .route("/v1/egress/vision", post(vision))
        .route("/v1/egress/tts", post(tts))
        .route("/v1/egress/asr", post(asr))
        .layer(DefaultBodyLimit::max(max_body_bytes))
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    (!token.is_empty()).then_some(token)
}

/// Authenticate an egress request. Order matters for the status codes:
/// `401` (no token) → `403` (egress disabled) → `503` (device channel disabled)
/// → `401` (unknown/expired token).
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(EgressState, SessionDescriptor), Response> {
    let Some(token) = bearer_token(headers) else {
        return Err(error::unauthorized(
            "missing `Authorization: Bearer <session token>` header",
        ));
    };
    let Some(egress) = state.egress.as_ref() else {
        return Err(error::forbidden(
            error::EGRESS_DISABLED,
            "network egress is disabled in the configuration",
        ));
    };
    let Some(device) = state.device.as_ref() else {
        return Err(error::error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            error::DEVICE_DISABLED,
            "device terminal channel is disabled; egress tokens cannot be issued",
        ));
    };
    match device.validate_token(token) {
        Some(session) => Ok((egress.clone(), session)),
        None => Err(error::unauthorized("invalid or expired session token")),
    }
}

fn record_egress(state: &AppState, session: &SessionDescriptor, detail: &str) {
    if let Some(device) = state.device.as_ref() {
        device.record_egress(&session.session_id, detail);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

async fn read_body(body: Body, limit: usize) -> Result<Bytes, Response> {
    axum::body::to_bytes(body, limit).await.map_err(|_| {
        error::payload_too_large(format!(
            "request body exceeds the configured {limit} byte limit"
        ))
    })
}

fn parse_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(bytes).map_err(|error| {
        error::error_response(
            StatusCode::BAD_REQUEST,
            error::INVALID_JSON,
            format!("invalid JSON request body: {error}"),
        )
    })
}

fn invalid_request(message: impl Into<String>) -> Response {
    error::error_response(
        StatusCode::BAD_REQUEST,
        error::INVALID_REQUEST,
        message.into(),
    )
}

fn truncate(message: &str, limit: usize) -> String {
    if message.chars().count() <= limit {
        return message.to_string();
    }
    let mut out: String = message.chars().take(limit).collect();
    out.push('…');
    out
}

fn upstream_error(message: impl Into<String>) -> Response {
    error::error_response(
        StatusCode::BAD_GATEWAY,
        error::EGRESS_UPSTREAM_ERROR,
        truncate(&message.into(), MAX_UPSTREAM_ERROR_CHARS),
    )
}

fn response_too_large(limit: usize) -> Response {
    error::error_response(
        StatusCode::BAD_GATEWAY,
        error::EGRESS_RESPONSE_TOO_LARGE,
        format!("upstream response exceeds the configured {limit} byte limit"),
    )
}

/// A buffered, size-capped successful upstream response.
struct UpstreamResponse {
    bytes: Bytes,
    content_type: Option<String>,
}

/// Send one request and buffer a size-capped, 2xx-only response.
async fn send(
    egress: &EgressState,
    request: reqwest::RequestBuilder,
    host: &str,
) -> Result<UpstreamResponse, Response> {
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            let kind = if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else {
                "request"
            };
            tracing::warn!(host, kind, "egress upstream request failed");
            return Err(upstream_error(format!("upstream request failed ({kind})")));
        }
    };

    let status = response.status();
    if !status.is_success() {
        // Upstream error bodies may contain sensitive prompt fragments: do not
        // forward or log them, only the status code.
        tracing::warn!(
            host,
            status = status.as_u16(),
            "egress upstream returned an error"
        );
        return Err(upstream_error(format!(
            "upstream returned HTTP {}",
            status.as_u16()
        )));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = read_upstream_body(response, egress.config.max_response_bytes).await?;
    tracing::info!(
        host,
        status = status.as_u16(),
        bytes = bytes.len(),
        content_type = content_type.as_deref().unwrap_or("unknown"),
        "egress upstream responded"
    );
    Ok(UpstreamResponse {
        bytes,
        content_type,
    })
}

async fn read_upstream_body(response: reqwest::Response, limit: usize) -> Result<Bytes, Response> {
    if let Some(length) = response.content_length() {
        if length as usize > limit {
            return Err(response_too_large(limit));
        }
    }
    let mut out = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if out.len().saturating_add(chunk.len()) > limit {
                    return Err(response_too_large(limit));
                }
                out.extend_from_slice(&chunk);
            }
            Err(_) => {
                tracing::warn!("failed while streaming the egress upstream response");
                return Err(upstream_error("failed to read the upstream response"));
            }
        }
    }
    Ok(Bytes::from(out))
}

/// Forward a successful upstream body verbatim (content type preserved).
fn forward(bytes: Bytes, content_type: Option<String>) -> Response {
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    if let Some(content_type) = content_type {
        if let Ok(value) = content_type.parse() {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
    }
    response
}

// ---------------------------------------------------------------------------
// Chat
// ---------------------------------------------------------------------------

async fn chat(State(state): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let (egress, session) = match authorize(&state, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let bytes = match read_body(body, egress.config.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let payload: Value = match parse_json(&bytes) {
        Ok(payload) => payload,
        Err(response) => return response,
    };
    let Some(object) = payload.as_object() else {
        return invalid_request("request body must be a JSON object");
    };
    if !object.contains_key("messages") {
        return invalid_request("`messages` is required");
    }
    if object.get("stream").and_then(Value::as_bool) == Some(true) {
        return invalid_request("streaming responses are not supported");
    }

    let url = match egress.resolve(CHAT_PATH) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let host = url.host_str().unwrap_or_default().to_string();
    let api_key = match egress.api_key() {
        Ok(api_key) => api_key,
        Err(response) => return response,
    };
    let request = egress.client.post(url).bearer_auth(api_key).json(&payload);

    match send(&egress, request, &host).await {
        Ok(upstream) => {
            record_egress(
                &state,
                &session,
                &format!("chat -> {host} ({} bytes)", upstream.bytes.len()),
            );
            forward(upstream.bytes, upstream.content_type)
        }
        Err(response) => {
            record_egress(&state, &session, &format!("chat upstream error on {host}"));
            response
        }
    }
}

// ---------------------------------------------------------------------------
// Vision
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct VisionRequest {
    /// Base64-encoded image bytes.
    image: String,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

async fn vision(State(state): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let (egress, session) = match authorize(&state, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let bytes = match read_body(body, egress.config.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let request: VisionRequest = match parse_json(&bytes) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.image.trim().is_empty() {
        return invalid_request("`image` must contain base64-encoded image bytes");
    }
    let question = request
        .question
        .or(request.prompt)
        .map(|question| question.trim().to_string())
        .filter(|question| !question.is_empty())
        .unwrap_or_else(|| "Describe this image in detail.".to_string());
    let mime = request.mime.unwrap_or_else(|| "image/png".to_string());
    if !mime.starts_with("image/") {
        return invalid_request("`mime` must be an image/* media type");
    }

    let mut payload = json!({
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": question },
                { "type": "image_url", "image_url": {
                    "url": format!("data:{mime};base64,{}", request.image)
                }},
            ],
        }],
    });
    if let Some(model) = request.model {
        payload["model"] = json!(model);
    }
    if let Some(max_tokens) = request.max_tokens {
        payload["max_tokens"] = json!(max_tokens);
    }

    let url = match egress.resolve(CHAT_PATH) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let host = url.host_str().unwrap_or_default().to_string();
    let api_key = match egress.api_key() {
        Ok(api_key) => api_key,
        Err(response) => return response,
    };
    let upstream = egress.client.post(url).bearer_auth(api_key).json(&payload);

    match send(&egress, upstream, &host).await {
        Ok(upstream) => {
            record_egress(
                &state,
                &session,
                &format!("vision -> {host} ({} bytes)", upstream.bytes.len()),
            );
            forward(upstream.bytes, upstream.content_type)
        }
        Err(response) => {
            record_egress(
                &state,
                &session,
                &format!("vision upstream error on {host}"),
            );
            response
        }
    }
}

// ---------------------------------------------------------------------------
// Text-to-speech
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SpeechRequest {
    text: String,
    #[serde(default)]
    voice: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    speed: Option<f32>,
    #[serde(default)]
    response_format: Option<String>,
}

async fn tts(State(state): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let (egress, session) = match authorize(&state, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let bytes = match read_body(body, egress.config.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let request: SpeechRequest = match parse_json(&bytes) {
        Ok(request) => request,
        Err(response) => return response,
    };
    if request.text.trim().is_empty() {
        return invalid_request("`text` must not be empty");
    }

    let mut payload = json!({
        "model": request.model.unwrap_or_else(|| "tts-1".to_string()),
        "input": request.text,
        "voice": request.voice.unwrap_or_else(|| "alloy".to_string()),
        "response_format": request.response_format.unwrap_or_else(|| "wav".to_string()),
    });
    if let Some(speed) = request.speed {
        if !(0.25..=4.0).contains(&speed) {
            return invalid_request("`speed` must be within 0.25..=4.0");
        }
        payload["speed"] = json!(speed);
    }

    let url = match egress.resolve(SPEECH_PATH) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let host = url.host_str().unwrap_or_default().to_string();
    let api_key = match egress.api_key() {
        Ok(api_key) => api_key,
        Err(response) => return response,
    };
    let upstream = egress.client.post(url).bearer_auth(api_key).json(&payload);

    match send(&egress, upstream, &host).await {
        Ok(upstream) => {
            record_egress(
                &state,
                &session,
                &format!("tts -> {host} ({} bytes)", upstream.bytes.len()),
            );
            // Speech endpoints return raw audio; default to audio/wav when the
            // upstream does not send a content type.
            let content_type = upstream
                .content_type
                .or_else(|| Some("audio/wav".to_string()));
            forward(upstream.bytes, content_type)
        }
        Err(response) => {
            record_egress(&state, &session, &format!("tts upstream error on {host}"));
            response
        }
    }
}

// ---------------------------------------------------------------------------
// Speech-to-text
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TranscriptionRequest {
    /// Base64-encoded audio bytes.
    audio: String,
    #[serde(default)]
    mime: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
}

async fn asr(State(state): State<AppState>, headers: HeaderMap, body: Body) -> Response {
    let (egress, session) = match authorize(&state, &headers).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let bytes = match read_body(body, egress.config.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(response) => return response,
    };
    let request: TranscriptionRequest = match parse_json(&bytes) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let audio = match base64::engine::general_purpose::STANDARD.decode(request.audio.trim()) {
        Ok(audio) => audio,
        Err(_) => return invalid_request("`audio` must be valid base64"),
    };
    if audio.is_empty() {
        return invalid_request("`audio` must not be empty");
    }
    if audio.len() > egress.config.max_body_bytes {
        return error::payload_too_large(format!(
            "decoded audio exceeds the configured {} byte limit",
            egress.config.max_body_bytes
        ));
    }

    let mut part =
        reqwest::multipart::Part::bytes(audio).file_name(audio_file_name(request.mime.as_deref()));
    if let Some(mime) = request.mime.as_deref() {
        match part.mime_str(mime) {
            Ok(updated) => part = updated,
            Err(_) => return invalid_request("`mime` is not a valid media type"),
        }
    }
    let mut form = reqwest::multipart::Form::new().part("file", part).text(
        "model",
        request.model.unwrap_or_else(|| "whisper-1".to_string()),
    );
    if let Some(language) = request.language {
        form = form.text("language", language);
    }
    if let Some(prompt) = request.prompt {
        form = form.text("prompt", prompt);
    }

    let url = match egress.resolve(TRANSCRIPTIONS_PATH) {
        Ok(url) => url,
        Err(response) => return response,
    };
    let host = url.host_str().unwrap_or_default().to_string();
    let api_key = match egress.api_key() {
        Ok(api_key) => api_key,
        Err(response) => return response,
    };
    let upstream = egress.client.post(url).bearer_auth(api_key).multipart(form);

    match send(&egress, upstream, &host).await {
        Ok(upstream) => {
            record_egress(
                &state,
                &session,
                &format!("asr -> {host} ({} bytes)", upstream.bytes.len()),
            );
            forward(upstream.bytes, upstream.content_type)
        }
        Err(response) => {
            record_egress(&state, &session, &format!("asr upstream error on {host}"));
            response
        }
    }
}

fn audio_file_name(mime: Option<&str>) -> &'static str {
    let mime = mime.unwrap_or_default().to_ascii_lowercase();
    if mime.contains("wav") {
        "audio.wav"
    } else if mime.contains("mpeg") || mime.contains("mp3") {
        "audio.mp3"
    } else if mime.contains("webm") {
        "audio.webm"
    } else if mime.contains("ogg") {
        "audio.ogg"
    } else if mime.contains("m4a") || mime.contains("mp4") {
        "audio.m4a"
    } else {
        "audio.wav"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_parsing_is_strict() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer  ".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer token-123".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("token-123"));
        headers.insert(header::AUTHORIZATION, "bearer lower".parse().unwrap());
        assert_eq!(bearer_token(&headers), Some("lower"));
    }

    #[test]
    fn audio_filename_follows_mime() {
        assert_eq!(audio_file_name(Some("audio/wav")), "audio.wav");
        assert_eq!(audio_file_name(Some("audio/mpeg")), "audio.mp3");
        assert_eq!(audio_file_name(Some("audio/webm")), "audio.webm");
        assert_eq!(audio_file_name(None), "audio.wav");
    }
}
