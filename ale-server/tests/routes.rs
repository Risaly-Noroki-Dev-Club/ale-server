//! Router smoke tests: status codes, error envelope, loopback gating and
//! redaction. No sockets are bound and no external network is used.

use ale_device::{AuditEntry, AuditKind, SessionDescriptor};
use ale_server::app::{build_router, AppState, MAX_JSON_BODY_BYTES};
use ale_server::config::Config;
use ale_server::device::{DeviceAccess, PairingInfo};
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, HeaderMap, Request, StatusCode},
    Router,
};
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc};
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
