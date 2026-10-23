//! End-to-end encrypted terminal session tests.
//!
//! Each test starts a real [`ale_device::session::SessionService`] on an
//! ephemeral loopback `TcpListener` and drives it with the independent terminal
//! client from `tests/session_support`: WebSocket upgrade, Noise NNpsk0
//! handshake, `SessionInit`/`SessionReady`, then protocol requests. Capture uses
//! a generated PNG and a fake backend script; no display server is required.

#![cfg(unix)]

#[path = "session_support/mod.rs"]
mod support;

use ale_device::audit::AuditKind;
use ale_device::capture::{decode_dimensions, CaptureRequest, ImageMime};
use ale_device::protocol::{
    DeviceMessage, ExecutionState, SessionInit, SessionRejected, DEVICE_PROTOCOL_VERSION,
};
use base64::Engine as _;
use futures_util::SinkExt;
use std::time::{Duration, Instant};
use support::*;
use tokio_tungstenite::tungstenite::Message;

fn expect_error(message: DeviceMessage, expected_code: &str) -> (Option<String>, String) {
    match message {
        DeviceMessage::Error {
            request_id,
            code,
            message,
        } => {
            assert_eq!(code, expected_code, "unexpected error code: {message}");
            (request_id, message)
        }
        other => panic!("expected Error, got {other:?}"),
    }
}

#[tokio::test]
async fn full_session_health_capture_token_and_shutdown() {
    let harness = start_service_with_fake_capture().await;
    let code = harness.service.pairing_code();
    let (mut connection, ready) = TestConnection::pair(harness.addr, &code).await;

    // --- SessionReady ------------------------------------------------------
    assert_eq!(ready.protocol_version, DEVICE_PROTOCOL_VERSION);
    assert!(!ready.session_id.is_empty());
    assert!(!ready.token.is_empty());
    assert_eq!(ready.server_version, env!("CARGO_PKG_VERSION"));
    assert!(
        ready.capabilities.capture.available,
        "fake capture backend should be available"
    );
    assert!(ready.capabilities.capture.backend.is_some());
    assert_eq!(harness.service.session_count(), 1);
    let sessions = harness.service.sessions();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, ready.session_id);
    assert_eq!(sessions[0].device_name, "test-terminal");
    assert!(sessions[0].remote_addr.starts_with("127.0.0.1:"));

    // A successful pairing consumes the code: it must have been rotated.
    assert_ne!(
        harness.service.pairing_code(),
        code,
        "pairing code must rotate after a successful pairing"
    );

    // --- egress token validation ------------------------------------------
    let descriptor = harness
        .service
        .validate_token(&ready.token)
        .expect("issued token must validate");
    assert_eq!(descriptor.session_id, ready.session_id);
    assert!(harness.service.validate_token("not-a-real-token").is_none());
    assert!(harness.service.validate_token("").is_none());

    // --- health ------------------------------------------------------------
    connection
        .send(&DeviceMessage::HealthRequest {
            request_id: "health-1".to_string(),
        })
        .await
        .expect("send health request");
    match connection
        .next_message(Duration::from_secs(5))
        .await
        .expect("health response")
    {
        DeviceMessage::Health { request_id, status } => {
            assert_eq!(request_id, "health-1");
            assert_eq!(status.protocol_version, DEVICE_PROTOCOL_VERSION);
            assert_eq!(status.server_version, env!("CARGO_PKG_VERSION"));
            assert_eq!(status.sessions, 1);
            assert!(status.capture_backend.is_some());
            assert!(status.uptime_s <= harness.service.uptime().as_secs() + 1);
        }
        other => panic!("expected Health, got {other:?}"),
    }

    // --- capture -----------------------------------------------------------
    connection
        .send(&DeviceMessage::CaptureRequest {
            request_id: "capture-1".to_string(),
            request: CaptureRequest {
                format: ImageMime::Png,
                fresh: true,
                max_width: Some(800),
                ..CaptureRequest::default()
            },
        })
        .await
        .expect("send capture request");
    let captured = match connection
        .next_message(Duration::from_secs(10))
        .await
        .expect("capture response")
    {
        DeviceMessage::CaptureResult {
            request_id,
            frame,
            data_base64,
        } => {
            assert_eq!(request_id, "capture-1");
            assert_eq!(frame.mime, ImageMime::Png);
            assert_eq!((frame.width, frame.height), (800, 600));
            assert!(!frame.frame_id.is_empty());
            assert!(frame.backend.ends_with("fake-capture"));
            let data = base64::engine::general_purpose::STANDARD
                .decode(&data_base64)
                .expect("capture data is valid base64");
            assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(decode_dimensions(&data).expect("decode PNG"), (800, 600));
            (frame.frame_id, data.len())
        }
        other => panic!("expected CaptureResult, got {other:?}"),
    };
    assert_eq!(
        harness.service.capture().last_frame_id().as_deref(),
        Some(captured.0.as_str()),
        "the captured frame is cached for the action-plan freshness guard"
    );
    assert!(harness
        .service
        .audit()
        .snapshot()
        .iter()
        .any(|entry| entry.kind == AuditKind::Capture));

    // --- display list ------------------------------------------------------
    connection
        .send(&DeviceMessage::DisplayListRequest {
            request_id: "displays-1".to_string(),
        })
        .await
        .expect("send display list request");
    match connection
        .next_message(Duration::from_secs(5))
        .await
        .expect("display list response")
    {
        DeviceMessage::DisplayList { request_id, .. } => assert_eq!(request_id, "displays-1"),
        other => panic!("expected DisplayList, got {other:?}"),
    }

    // --- ping / pong -------------------------------------------------------
    connection
        .send(&DeviceMessage::Ping {
            request_id: "ping-1".to_string(),
            nonce: 0x1234_5678,
        })
        .await
        .expect("send ping");
    match connection
        .next_message(Duration::from_secs(5))
        .await
        .expect("pong response")
    {
        DeviceMessage::Pong { request_id, nonce } => {
            assert_eq!(request_id, "ping-1");
            assert_eq!(nonce, 0x1234_5678);
        }
        other => panic!("expected Pong, got {other:?}"),
    }

    // --- graceful shutdown -------------------------------------------------
    harness.shutdown.send(true).expect("signal device shutdown");
    let server_result = tokio::time::timeout(Duration::from_secs(15), harness.server)
        .await
        .expect("server did not stop after shutdown")
        .expect("server task panicked");
    assert!(server_result.is_ok(), "serve returned {server_result:?}");
    assert_eq!(harness.service.session_count(), 0);
    assert!(
        harness
            .service
            .audit()
            .snapshot()
            .iter()
            .any(|entry| entry.kind == AuditKind::Disconnected),
        "disconnect must be audited"
    );
    // Egress tokens live for `pairing.token_ttl_s`, independent of the session.
    assert!(harness.service.validate_token(&ready.token).is_some());
}

#[tokio::test]
async fn protocol_mismatch_is_rejected_with_session_rejected() {
    let harness = start_service(test_config()).await;
    let mut connection = TestConnection::start(harness.addr, &harness.service.pairing_code())
        .await
        .expect("noise handshake");
    connection
        .send(&DeviceMessage::SessionInit(SessionInit {
            protocol_version: DEVICE_PROTOCOL_VERSION + 1,
            device_name: "old-terminal".to_string(),
            client_kind: ale_device::protocol::ClientKind::Terminal,
        }))
        .await
        .expect("send future SessionInit");

    match connection
        .next_message(Duration::from_secs(5))
        .await
        .expect("session rejected")
    {
        DeviceMessage::SessionRejected(SessionRejected { code, .. }) => {
            assert_eq!(code, "PROTOCOL_INCOMPATIBLE");
        }
        other => panic!("expected SessionRejected, got {other:?}"),
    }
    let reason = connection
        .expect_close(Duration::from_secs(5))
        .await
        .expect("server should close after rejecting the protocol");
    assert_eq!(reason, "protocol_incompatible");
    assert_eq!(harness.service.session_count(), 0);
}

#[tokio::test]
async fn wrong_pairing_code_fails_the_handshake() {
    let harness = start_service(test_config()).await;
    let error = TestConnection::start(harness.addr, "ZZZZZZZZ")
        .await
        .err()
        .expect("wrong pairing code must not establish a session");
    assert!(
        error.contains("closed") || error.contains("websocket"),
        "unexpected handshake error: {error}"
    );
    assert_eq!(harness.service.session_count(), 0);
    assert!(
        harness
            .service
            .audit()
            .snapshot()
            .iter()
            .any(|entry| entry.kind == AuditKind::PairingFailed),
        "failed pairing must be audited"
    );

    // The correct code still works afterwards (default max_attempts = 5).
    let code = harness.service.pairing_code();
    let (_connection, ready) = TestConnection::pair(harness.addr, &code).await;
    assert_eq!(ready.protocol_version, DEVICE_PROTOCOL_VERSION);
}

#[tokio::test]
async fn pairing_attempts_back_off_after_max_attempts() {
    let mut config = test_config();
    config.pairing.max_attempts = 1;
    let harness = start_service(config).await;

    let first = TestConnection::start(harness.addr, "00000000").await;
    assert!(first.is_err(), "the first wrong code must fail");

    // The next connection is refused before the Noise handshake even starts.
    let mut socket = raw_socket(harness.addr).await;
    let reason = read_close(&mut socket, Duration::from_secs(5))
        .await
        .expect("server should close the rate-limited connection");
    assert_eq!(reason, "too_many_attempts");
    assert_eq!(harness.service.session_count(), 0);
    assert!(harness
        .service
        .audit()
        .snapshot()
        .iter()
        .any(|entry| entry.kind == AuditKind::Denied && entry.detail.contains("rate limited")));
}

#[tokio::test]
async fn invalid_handshake_frame_is_rejected_and_closed() {
    let harness = start_service(test_config()).await;
    let mut socket = raw_socket(harness.addr).await;
    let garbage = vec![0xABu8; 64];
    socket
        .send(Message::Binary(garbage))
        .await
        .expect("send garbage handshake frame");
    let reason = read_close(&mut socket, Duration::from_secs(5))
        .await
        .expect("server should close after an invalid handshake");
    assert_eq!(reason, "pairing_failed");
    assert!(harness
        .service
        .audit()
        .snapshot()
        .iter()
        .any(|entry| entry.kind == AuditKind::PairingFailed));
}

#[tokio::test]
async fn stale_frame_action_plan_is_rejected() {
    let harness = start_service(test_config()).await;
    let (mut connection, _ready) =
        TestConnection::pair(harness.addr, &harness.service.pairing_code()).await;

    connection
        .send(&DeviceMessage::ActionPlanRequest {
            request_id: "plan-stale".to_string(),
            plan: wait_plan(1),
            approved: true,
            expected_frame_id: Some("frame-that-does-not-exist".to_string()),
        })
        .await
        .expect("send stale action plan");
    let (request_id, message) = expect_error(
        connection
            .next_message(Duration::from_secs(5))
            .await
            .expect("stale plan response"),
        "STALE_FRAME",
    );
    assert_eq!(request_id.as_deref(), Some("plan-stale"));
    assert!(
        message.contains("no screenshot has been captured")
            || message.contains("STALE_FRAME")
            || message.contains("latest frame"),
        "unexpected stale-frame message: {message}"
    );
    assert!(harness
        .service
        .audit()
        .snapshot()
        .iter()
        .any(|entry| entry.kind == AuditKind::Denied));
}

#[tokio::test]
async fn cancel_wait_action_plan_stops_promptly() {
    let harness = start_service(test_config()).await;
    let (mut connection, _ready) =
        TestConnection::pair(harness.addr, &harness.service.pairing_code()).await;

    connection
        .send(&DeviceMessage::ActionPlanRequest {
            request_id: "slow-plan".to_string(),
            plan: wait_plan(5_000),
            approved: true,
            expected_frame_id: None,
        })
        .await
        .expect("send long-running action plan");
    // Give the request a moment to start executing the Wait action.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let cancel_started = Instant::now();
    connection
        .send(&DeviceMessage::Cancel {
            request_id: "slow-plan".to_string(),
        })
        .await
        .expect("send cancel");

    match connection
        .next_message(Duration::from_secs(3))
        .await
        .expect("cancelled action result")
    {
        DeviceMessage::ActionResult {
            request_id, state, ..
        } => {
            assert_eq!(request_id, "slow-plan");
            assert_eq!(state, ExecutionState::Cancelled);
        }
        other => panic!("expected cancelled ActionResult, got {other:?}"),
    }
    assert!(
        cancel_started.elapsed() < Duration::from_secs(2),
        "cancellation took {:?}",
        cancel_started.elapsed()
    );
    assert!(harness
        .service
        .audit()
        .snapshot()
        .iter()
        .any(|entry| entry.kind == AuditKind::Cancelled));
}

#[tokio::test]
async fn rate_limit_rejects_excess_requests() {
    let harness = start_service(test_config()).await;
    let (mut connection, _ready) =
        TestConnection::pair(harness.addr, &harness.service.pairing_code()).await;

    for index in 0..30 {
        let request_id = format!("health-{index}");
        connection
            .send(&DeviceMessage::HealthRequest {
                request_id: request_id.clone(),
            })
            .await
            .expect("send health request");
        match connection
            .next_message(Duration::from_secs(5))
            .await
            .expect("health response")
        {
            DeviceMessage::Health { request_id: id, .. } => assert_eq!(id, request_id),
            other => panic!("expected Health, got {other:?}"),
        }
    }

    connection
        .send(&DeviceMessage::HealthRequest {
            request_id: "health-over-limit".to_string(),
        })
        .await
        .expect("send over-limit request");
    let (request_id, _message) = expect_error(
        connection
            .next_message(Duration::from_secs(5))
            .await
            .expect("rate limited response"),
        "RATE_LIMITED",
    );
    assert_eq!(request_id.as_deref(), Some("health-over-limit"));
}
