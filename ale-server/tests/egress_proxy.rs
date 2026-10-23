//! Egress proxy tests against a local mock OpenAI-compatible upstream on
//! `127.0.0.1:0`. No external network is used.

use ale_device::{AuditEntry, SessionDescriptor};
use ale_server::app::{build_router, AppState};
use ale_server::config::{Config, EgressConfig};
use ale_server::device::{DeviceAccess, PairingInfo};
use ale_server::egress::EgressState;
use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    Router,
};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::net::TcpListener;
use tower::ServiceExt;

struct FakeDevice {
    token: &'static str,
}

impl DeviceAccess for FakeDevice {
    fn pairing_info(&self) -> PairingInfo {
        PairingInfo {
            code: "PAIRCODE".to_string(),
            expires_in_s: 120,
        }
    }

    fn sessions(&self) -> Vec<SessionDescriptor> {
        Vec::new()
    }

    fn audit_tail(&self, _limit: usize) -> Vec<AuditEntry> {
        Vec::new()
    }

    fn validate_token(&self, token: &str) -> Option<SessionDescriptor> {
        if token == self.token {
            Some(SessionDescriptor {
                session_id: "session-1".to_string(),
                device_name: "test-phone".to_string(),
                remote_addr: "127.0.0.1:1".to_string(),
                connected_at_unix_ms: 0,
            })
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

#[derive(Clone)]
struct MockUpstream {
    status: StatusCode,
    body: String,
    content_type: &'static str,
    hits: Arc<AtomicUsize>,
    last_authorization: Arc<Mutex<Option<String>>>,
}

async fn mock_handler(State(mock): State<MockUpstream>, headers: HeaderMap) -> Response {
    mock.hits.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut last) = mock.last_authorization.lock() {
        *last = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
    }
    (
        mock.status,
        [(header::CONTENT_TYPE, mock.content_type)],
        mock.body.clone(),
    )
        .into_response()
}

/// Spawn a mock upstream on an ephemeral loopback port.
async fn spawn_mock_upstream(
    status: StatusCode,
    body: impl Into<String>,
) -> (String, MockUpstream) {
    let mock = MockUpstream {
        status,
        body: body.into(),
        content_type: "application/json",
        hits: Arc::new(AtomicUsize::new(0)),
        last_authorization: Arc::new(Mutex::new(None)),
    };
    let router = Router::new()
        .fallback(mock_handler)
        .with_state(mock.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream address");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), mock)
}

fn egress_config(upstream: &str, allowed: &[&str], api_key_env: &str) -> EgressConfig {
    EgressConfig {
        enabled: true,
        upstream: upstream.to_string(),
        allowed_hosts: allowed.iter().map(|host| host.to_string()).collect(),
        api_key_env: api_key_env.to_string(),
        timeout_s: 5,
        max_body_bytes: 1024,
        max_response_bytes: 1024,
    }
}

fn state_with_egress(egress: EgressConfig, device: Option<Arc<dyn DeviceAccess>>) -> AppState {
    let egress_state = EgressState::new(&egress).expect("egress client");
    let mut config = Config::default();
    config.engine.enabled = false;
    config.egress = egress;
    AppState::from_config(config, Some(egress_state), device)
}

fn chat_request(token: Option<&str>, body: String) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/egress/chat")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).expect("request")
}

async fn call(app: Router, request: Request<Body>) -> (StatusCode, String) {
    let response = app.oneshot(request).await.expect("router call");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("response body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

#[tokio::test]
async fn egress_requires_bearer_token() {
    let (upstream, mock) = spawn_mock_upstream(StatusCode::OK, r#"{"ok":true}"#).await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], "ALE_SERVER_TEST_EGRESS_UNUSED"),
        Some(fake_device()),
    );
    let (status, body) = call(
        build_router(state),
        chat_request(None, r#"{"messages":[]}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json(&body)["error"]["code"], "UNAUTHORIZED");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn egress_rejects_unknown_token() {
    let (upstream, mock) = spawn_mock_upstream(StatusCode::OK, r#"{"ok":true}"#).await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], "ALE_SERVER_TEST_EGRESS_UNUSED"),
        Some(fake_device()),
    );
    let (status, body) = call(
        build_router(state),
        chat_request(Some("wrong-token"), r#"{"messages":[]}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json(&body)["error"]["code"], "UNAUTHORIZED");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn egress_rejects_disallowed_upstream_host() {
    let (upstream, mock) = spawn_mock_upstream(StatusCode::OK, r#"{"ok":true}"#).await;
    let state = state_with_egress(
        egress_config(
            &upstream,
            &["api.example.com"],
            "ALE_SERVER_TEST_EGRESS_UNUSED",
        ),
        Some(fake_device()),
    );
    let (status, body) = call(
        build_router(state),
        chat_request(Some("test-token"), r#"{"messages":[]}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json(&body)["error"]["code"], "EGRESS_DENIED");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn egress_rejects_oversized_request_body() {
    let (upstream, mock) = spawn_mock_upstream(StatusCode::OK, r#"{"ok":true}"#).await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], "ALE_SERVER_TEST_EGRESS_UNUSED"),
        Some(fake_device()),
    );
    let body = format!(
        r#"{{"messages":[{{"role":"user","content":"{}"}}]}}"#,
        "x".repeat(4096)
    );
    let (status, response) =
        call(build_router(state), chat_request(Some("test-token"), body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json(&response)["error"]["code"], "PAYLOAD_TOO_LARGE");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn egress_forwards_chat_and_injects_api_key() {
    let env_name = "ALE_SERVER_TEST_EGRESS_KEY_FORWARD";
    std::env::set_var(env_name, "test-secret-key");
    let (upstream, mock) = spawn_mock_upstream(
        StatusCode::OK,
        r#"{"choices":[{"message":{"content":"hello"}}]}"#,
    )
    .await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], env_name),
        Some(fake_device()),
    );
    let body = r#"{"model":"test-model","messages":[{"role":"user","content":"hi"}]}"#;
    let (status, response) = call(
        build_router(state),
        chat_request(Some("test-token"), body.to_string()),
    )
    .await;
    std::env::remove_var(env_name);

    assert_eq!(status, StatusCode::OK);
    let value = json(&response);
    assert_eq!(value["choices"][0]["message"]["content"], "hello");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        mock.last_authorization.lock().unwrap().as_deref(),
        Some("Bearer test-secret-key")
    );
}

#[tokio::test]
async fn egress_maps_upstream_http_error_to_502() {
    let env_name = "ALE_SERVER_TEST_EGRESS_KEY_UPSTREAM_ERROR";
    std::env::set_var(env_name, "test-secret-key");
    let (upstream, mock) =
        spawn_mock_upstream(StatusCode::INTERNAL_SERVER_ERROR, r#"{"error":"boom"}"#).await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], env_name),
        Some(fake_device()),
    );
    let (status, body) = call(
        build_router(state),
        chat_request(Some("test-token"), r#"{"messages":[]}"#.to_string()),
    )
    .await;
    std::env::remove_var(env_name);

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json(&body)["error"]["code"], "EGRESS_UPSTREAM_ERROR");
    assert!(
        !body.contains("boom"),
        "upstream error body must not be forwarded"
    );
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn egress_caps_upstream_response_size() {
    let env_name = "ALE_SERVER_TEST_EGRESS_KEY_RESPONSE_CAP";
    std::env::set_var(env_name, "test-secret-key");
    let (upstream, mock) = spawn_mock_upstream(StatusCode::OK, "x".repeat(8192)).await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], env_name),
        Some(fake_device()),
    );
    let (status, body) = call(
        build_router(state),
        chat_request(Some("test-token"), r#"{"messages":[]}"#.to_string()),
    )
    .await;
    std::env::remove_var(env_name);

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(json(&body)["error"]["code"], "EGRESS_RESPONSE_TOO_LARGE");
    assert_eq!(mock.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn egress_disabled_returns_403() {
    let mut config = Config::default();
    config.engine.enabled = false;
    config.egress.enabled = false;
    let state = AppState::from_config(config, None, Some(fake_device()));
    let (status, body) = call(
        build_router(state),
        chat_request(Some("test-token"), r#"{"messages":[]}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(json(&body)["error"]["code"], "EGRESS_DISABLED");
}

#[tokio::test]
async fn egress_without_device_channel_returns_503() {
    let (upstream, _mock) = spawn_mock_upstream(StatusCode::OK, r#"{"ok":true}"#).await;
    let state = state_with_egress(
        egress_config(&upstream, &["127.0.0.1"], "ALE_SERVER_TEST_EGRESS_UNUSED"),
        None,
    );
    let (status, body) = call(
        build_router(state),
        chat_request(Some("test-token"), r#"{"messages":[]}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(&body)["error"]["code"], "DEVICE_DISABLED");
}
