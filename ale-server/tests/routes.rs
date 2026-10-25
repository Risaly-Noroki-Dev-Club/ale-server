//! Router smoke tests: status codes, error envelope, loopback gating and
//! redaction. No sockets are bound and no external network is used.

use ale_core::config::AppConfig as CoreAppConfig;
use ale_core::model_api::WireApi;
use ale_device::{AuditEntry, AuditKind, SessionDescriptor};
use ale_server::app::{build_router, AppState, MAX_JSON_BODY_BYTES};
use ale_server::config::{Config, EngineConfig};
use ale_server::device::{DeviceAccess, PairingInfo};
use ale_server::secrets::SecretStoreKind;
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

struct FakeDevice {
    token: &'static str,
}

impl DeviceAccess for FakeDevice {
    fn pairing_info(&self) -> PairingInfo {
        PairingInfo {
            code: "TESTCODE".to_string(),
            expires_in_s: 240,
        }
    }

    fn sessions(&self) -> Vec<SessionDescriptor> {
        vec![SessionDescriptor {
            session_id: "session-1".to_string(),
            device_name: "test-phone".to_string(),
            remote_addr: "192.0.2.10:5000".to_string(),
            connected_at_unix_ms: 1_700_000_000_000,
        }]
    }

    fn audit_tail(&self, limit: usize) -> Vec<AuditEntry> {
        let entries = vec![AuditEntry {
            time_unix_ms: 1_700_000_000_000,
            session_id: "session-1".to_string(),
            kind: AuditKind::Connected,
            detail: "connected".to_string(),
        }];
        entries.into_iter().take(limit).collect()
    }

    fn validate_token(&self, token: &str) -> Option<SessionDescriptor> {
        if token == self.token {
            self.sessions().into_iter().next()
        } else {
            None
        }
    }

    fn record_egress(&self, _session_id: &str, _detail: &str) {}
}

fn fake_device() -> Arc<dyn DeviceAccess> {
    Arc::new(FakeDevice {
        token: "test-token",
    })
}

fn test_config(engine_enabled: bool) -> Config {
    let mut config = Config::default();
    config.engine.enabled = engine_enabled;
    config
}

fn state(engine_enabled: bool, device: Option<Arc<dyn DeviceAccess>>) -> AppState {
    AppState::from_config(test_config(engine_enabled), None, device)
}

fn app(state: AppState) -> Router {
    build_router(state)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request")
}

fn get_with_origin(uri: &str, origin: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::ORIGIN, origin)
        .body(Body::empty())
        .expect("request")
}

fn get_from(uri: &str, remote: SocketAddr) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .extension(ConnectInfo(remote))
        .body(Body::empty())
        .expect("request")
}

fn post_json(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("request")
}

async fn call(app: Router, request: Request<Body>) -> (StatusCode, String, HeaderMap) {
    let response = app.oneshot(request).await.expect("router call");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("response body");
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    )
}

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

#[tokio::test]
async fn health_returns_200_and_runtime_fields() {
    let (status, body, _) = call(app(state(false, None)), get("/health")).await;
    assert_eq!(status, StatusCode::OK);
    let value = json(&body);
    assert_eq!(value["status"], "ok");
    assert_eq!(value["engine_enabled"], false);
    assert_eq!(value["engine_ready"], false);
    assert_eq!(value["device_enabled"], true);
    assert_eq!(value["device_ready"], false);
    assert!(value["version"].as_str().unwrap().contains('.'));
    assert!(value["uptime_s"].is_u64());
}

#[tokio::test]
async fn status_redacts_credential_values() {
    let env_name = "ALE_SERVER_TEST_STATUS_REDACTION_SECRET";
    std::env::set_var(env_name, "sk-status-secret-value");

    let mut config = test_config(false);
    config.egress.enabled = true;
    config.egress.upstream = "https://api.openai.com".to_string();
    config.egress.allowed_hosts = vec!["api.openai.com".to_string()];
    config.egress.api_key_env = env_name.to_string();
    let state = AppState::from_config(config, None, None);

    let (status, body, _) = call(app(state), get("/status")).await;
    std::env::remove_var(env_name);

    assert_eq!(status, StatusCode::OK);
    assert!(
        !body.contains("sk-status-secret-value"),
        "credential value leaked into /status"
    );
    let value = json(&body);
    assert_eq!(value["config"]["egress"]["api_key_env"], env_name);
    assert_eq!(value["config"]["egress"]["api_key_configured"], true);
    assert_eq!(value["config"]["server"]["bind"], "0.0.0.0:8080");
    // The raw key is never part of the config document.
    assert!(value["config"]["egress"].get("api_key").is_none());
}

#[tokio::test]
async fn models_returns_engine_unavailable_when_engine_disabled() {
    let (status, body, _) = call(app(state(false, None)), get("/models")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&body)["error"]["code"], "ENGINE_UNAVAILABLE");
}

#[tokio::test]
async fn inference_route_returns_engine_unavailable_when_engine_disabled() {
    let request = post_json("/tts/synthesize", r#"{"text":"hello"}"#.to_string());
    let (status, body, _) = call(app(state(false, None)), request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&body)["error"]["code"], "ENGINE_UNAVAILABLE");
}

#[tokio::test]
async fn unknown_route_returns_json_404() {
    let (status, body, _) = call(app(state(false, None)), get("/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["error"]["code"], "NOT_FOUND");
}

#[tokio::test]
async fn wrong_method_returns_405() {
    let (status, _, _) = call(app(state(false, None)), get("/tts/synthesize")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn oversized_inference_json_body_returns_413() {
    let big_text = "a".repeat(MAX_JSON_BODY_BYTES + 1024);
    let request = post_json("/tts/synthesize", format!(r#"{{"text":"{big_text}"}}"#));
    let (status, body, _) = call(app(state(false, None)), request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json(&body)["error"]["code"], "PAYLOAD_TOO_LARGE");
}

#[tokio::test]
async fn device_status_rejects_non_loopback_remote() {
    let remote: SocketAddr = "203.0.113.9:4444".parse().unwrap();
    let request = get_from("/v1/device/status", remote);
    let (status, body, _) = call(app(state(false, Some(fake_device()))), request).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json(&body)["error"]["code"], "FORBIDDEN");
}

#[tokio::test]
async fn device_status_returns_pairing_and_audit_for_loopback() {
    let remote: SocketAddr = "127.0.0.1:50000".parse().unwrap();
    let request = get_from("/v1/device/status", remote);
    let (status, body, _) = call(app(state(false, Some(fake_device()))), request).await;
    assert_eq!(status, StatusCode::OK);
    let value = json(&body);
    assert_eq!(value["pairing"]["code"], "TESTCODE");
    assert_eq!(value["pairing"]["expires_in_s"], 240);
    assert_eq!(value["sessions"][0]["session_id"], "session-1");
    assert_eq!(value["audit_tail"][0]["kind"], "connected");
}

#[tokio::test]
async fn device_status_returns_503_when_device_disabled() {
    let remote: SocketAddr = "[::1]:50000".parse().unwrap();
    let request = get_from("/v1/device/status", remote);
    let (status, body, _) = call(app(state(false, None)), request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&body)["error"]["code"], "DEVICE_DISABLED");
}

#[tokio::test]
async fn cors_is_not_permissive_by_default() {
    let request = get_with_origin("/health", "https://evil.example");
    let (status, _, headers) = call(app(state(false, None)), request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn cors_allows_configured_origins_only() {
    let mut config = test_config(false);
    config.server.cors_origin = vec!["https://admin.example.com".to_string()];
    let state = AppState::from_config(config, None, None);

    let allowed = get_with_origin("/health", "https://admin.example.com");
    let (status, _, headers) = call(app(state.clone()), allowed).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|value| value.to_str().ok()),
        Some("https://admin.example.com")
    );

    let denied = get_with_origin("/health", "https://evil.example");
    let (_, _, headers) = call(app(state), denied).await;
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

// ---------------------------------------------------------------------------
// Downstream cloud TTS and configuration-derived model catalogue
// ---------------------------------------------------------------------------

/// Minimal WAV header: `audio_format` sniffs the `RIFF`/`WAVE` magic.
const MOCK_WAV: &[u8] = b"RIFF\x24\x00\x00\x00WAVEfmt ";

#[derive(Clone, Default)]
struct MockSpeechUpstream {
    bodies: Arc<Mutex<Vec<Value>>>,
    authorizations: Arc<Mutex<Vec<Option<String>>>>,
}

async fn mock_speech_handler(
    State(mock): State<MockSpeechUpstream>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Ok(mut bodies) = mock.bodies.lock() {
        bodies.push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    }
    if let Ok(mut authorizations) = mock.authorizations.lock() {
        authorizations.push(
            headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        );
    }
    (
        [(header::CONTENT_TYPE, "audio/wav")],
        axum::body::Bytes::from_static(MOCK_WAV),
    )
        .into_response()
}

async fn spawn_mock_speech() -> (SocketAddr, MockSpeechUpstream) {
    let mock = MockSpeechUpstream::default();
    let router = Router::new()
        .route("/v1/audio/speech", post(mock_speech_handler))
        .with_state(mock.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (addr, mock)
}

/// Write an ale-core config + file credential store so the lazy engine can be
/// initialized against a local mock endpoint (no external network).
fn engine_config_with(
    dir: &tempfile::TempDir,
    api_key: Option<&str>,
    configure: impl FnOnce(&mut CoreAppConfig),
) -> EngineConfig {
    let mut core_config = CoreAppConfig::default();
    configure(&mut core_config);
    std::fs::write(
        dir.path().join("engine-config.json"),
        serde_json::to_vec(&core_config).expect("serialize ale-core config"),
    )
    .expect("write ale-core config");
    let secrets_path = dir.path().join("secrets.json");
    if let Some(api_key) = api_key {
        std::fs::write(
            &secrets_path,
            serde_json::to_vec(&json!({ "api_key": api_key })).expect("serialize secrets"),
        )
        .expect("write secrets file");
    }
    EngineConfig {
        enabled: true,
        config_path: Some(dir.path().join("engine-config.json")),
        secret_store: SecretStoreKind::File,
        secret_store_path: Some(secrets_path),
    }
}

fn state_with_engine(engine: EngineConfig) -> AppState {
    let config = Config {
        engine,
        ..Default::default()
    };
    AppState::from_config(config, None, None)
}

#[tokio::test]
async fn tts_synthesize_calls_configured_cloud_endpoint() {
    let (addr, mock) = spawn_mock_speech().await;
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_config_with(&dir, Some("sk-tts-test"), |core| {
        core.cloud_api.provider = "openai".to_string();
        core.cloud_api.api_url = format!("http://{addr}/v1");
        core.cloud_api.model = "gpt-4o".to_string();
    });

    let request = post_json("/tts/synthesize", r#"{"text":"hello TTS"}"#.to_string());
    let (status, body, _) = call(app(state_with_engine(engine)), request).await;

    assert_eq!(status, StatusCode::OK, "{body}");
    let value = json(&body);
    assert_eq!(value["format"], "wav");
    assert_eq!(value["bytes"], MOCK_WAV.len());
    assert_eq!(
        BASE64
            .decode(value["audio_base64"].as_str().unwrap())
            .unwrap(),
        MOCK_WAV
    );

    let bodies = mock.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["model"], "tts-1");
    assert_eq!(bodies[0]["voice"], "alloy");
    assert_eq!(bodies[0]["response_format"], "wav");
    assert_eq!(bodies[0]["input"], "hello TTS");
    assert_eq!(
        mock.authorizations.lock().unwrap()[0].as_deref(),
        Some("Bearer sk-tts-test")
    );
}

#[tokio::test]
async fn tts_synthesize_rejects_non_openai_wire_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_config_with(&dir, Some("sk-test"), |core| {
        core.cloud_api.provider = "custom".to_string();
        core.cloud_api.wire_api = WireApi::AnthropicMessages;
        core.cloud_api.api_url = "https://api.example.com/v1".to_string();
    });

    let request = post_json("/tts/synthesize", r#"{"text":"hello"}"#.to_string());
    let (status, body, _) = call(app(state_with_engine(engine)), request).await;

    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(json(&body)["error"]["code"], "UNSUPPORTED_WIRE_API");
    assert!(json(&body)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("anthropic_messages"));
}

#[tokio::test]
async fn tts_synthesize_without_cloud_key_returns_inference_failed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_config_with(&dir, None, |_| {});

    let request = post_json("/tts/synthesize", r#"{"text":"hello"}"#.to_string());
    let (status, body, _) = call(app(state_with_engine(engine)), request).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(json(&body)["error"]["code"], "INFERENCE_FAILED");
    assert!(json(&body)["error"]["message"]
        .as_str()
        .unwrap()
        .contains("api_key"));
}

#[tokio::test]
async fn models_lists_cloud_and_local_entries_from_engine_config() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_config_with(&dir, Some("sk-test"), |core| {
        core.cloud_api.model = "gpt-4o-mini".to_string();
        core.cloud_api.api_url = "https://api.example.com/v1".to_string();
    });

    let (status, body, _) = call(app(state_with_engine(engine)), get("/models")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let models = json(&body)["models"].as_array().unwrap().clone();
    // cloud chat model + default scheduler entries (qwen, qwen large, grounding).
    assert_eq!(models.len(), 4);
    assert_eq!(models[0]["id"], "gpt-4o-mini");
    assert_eq!(models[0]["source"], "cloud");
    assert_eq!(models[0]["purpose"], "chat");
    assert!(models[0]["size_bytes"].is_null());
    assert_eq!(models[1]["source"], "local-modeld");
    assert_eq!(models[1]["purpose"], "planning");
    assert_eq!(models[3]["purpose"], "grounding");
    assert!(models.iter().all(|model| model["size_bytes"].is_null()));
}
