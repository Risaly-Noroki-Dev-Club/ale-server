//! Server-owned model/browser loop, using local protocol mocks only.
use ale_server::{
    app::{build_router, AppState},
    config::Config,
};
use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
#[derive(Default)]
struct Mock {
    article: bool,
    calls: usize,
    closed: bool,
    bad_link: bool,
    full_text_sent: bool,
    navigation_polls: usize,
    new_tab: bool,
    switched: bool,
    direct: bool,
}
type Shared = Arc<Mutex<Mock>>;
async fn session() -> Json<Value> {
    Json(json!({"value":{"sessionId":"test-session"}}))
}
async fn empty() -> Json<Value> {
    Json(json!({"value":null}))
}
async fn handles(State(state): State<Shared>) -> Json<Value> {
    let state = state.lock().unwrap();
    Json(
        json!({"value": if state.new_tab && state.article {vec!["window-1","window-2"]} else {vec!["window-1"]}}),
    )
}
async fn switch(State(state): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["handle"], "window-2");
    state.lock().unwrap().switched = true;
    empty().await
}
async fn current_url(State(state): State<Shared>) -> Json<Value> {
    let mut state = state.lock().unwrap();
    state.navigation_polls += 1;
    Json(
        json!({"value": if state.article && state.navigation_polls>=3 && (!state.new_tab || state.switched) {"https://example.org/new/"} else {"https://example.org/"}}),
    )
}
async fn screenshot() -> Json<Value> {
    Json(json!({"value":"aW1hZ2U="}))
}
async fn script(State(state): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    let state = state.lock().unwrap();
    let observe_script = body["script"].as_str().unwrap_or("");
    if state.direct && observe_script.contains("querySelector('article')") {
        return Json(
            json!({"value":{"url":"https://example.org/","title":"Example","heading":"Newest article","article_present":true,"truncated":false,"text":"Full direct article. First section. Second section. End of article.","text_chars":100,"times":[{"datetime":"2026-10-25","text":"2026-10-25"}],"links":[]}}),
        );
    }
    if body["script"] == "return document.readyState;" {
        return Json(json!({"value":"complete"}));
    }
    if body["script"]
        .as_str()
        .unwrap()
        .contains(".find(a=>a.href===")
    {
        return Json(json!({"value":{"element-6066-11e4-a52e-4f735466cecf":"post-link"}}));
    }
    Json(
        json!({"value":{"url":if state.article {"https://example.org/new/"} else {"https://example.org/"},"title":"Example","heading":if state.article {"Newest article"} else {"Index"},"article_present":state.article,"truncated":false,"text":if state.article {"Full article. First section. Second section. End of article."} else {"New article 2026-10-25. Old article 2026-10-01."},"text_chars":100,"times":[{"datetime":"2026-10-25","text":"2026-10-25"}],"links":[{"label":"New article","url":"https://example.org/new/","context":"2026-10-25"},{"label":"Old article","url":"https://example.org/old/","context":"2026-10-01"}]}}),
    )
}
async fn click(State(state): State<Shared>) -> Json<Value> {
    state.lock().unwrap().article = true;
    empty().await
}
async fn close(State(state): State<Shared>) -> Json<Value> {
    state.lock().unwrap().closed = true;
    empty().await
}
async fn model(State(state): State<Shared>, Json(body): Json<Value>) -> Json<Value> {
    let mut state = state.lock().unwrap();
    state.calls += 1;
    if state.calls == 2 {
        state.full_text_sent = body.to_string().contains("End of article.");
    }
    let decision = if state.direct {
        json!({"action":"finish","source_url":"https://example.org/","latest_evidence":"2026-10-25 is the newest observed date","markdown":format!("# Newest article summary\n\nSource: https://example.org/\n\n{}","Directly opened article summary body. ".repeat(6))})
    } else if state.calls == 1 {
        json!({"action":"open_link","url":if state.bad_link {"https://example.org/invented/"} else {"https://example.org/new/"},"reason":"2026-10-25 is newer than 2026-10-01"})
    } else {
        json!({"action":"finish","source_url":"https://example.org/new/","latest_evidence":"2026-10-25 is newer than 2026-10-01","markdown":format!("# Newest article summary\n\nSource: https://example.org/new/\n\n{}","The summary covers first and second sections. ".repeat(6))})
    };
    Json(
        json!({"id":"test-response","status":"completed","model":"test-model","output":[{"type":"function_call","call_id":"test-call","name":"browser_task_step","arguments":decision.to_string()}],"usage":{"input_tokens":100,"output_tokens":20}}),
    )
}
#[tokio::test]
async fn submitted_goal_runs_server_loop_writes_markdown_and_closes_browser() {
    run(false, false, false).await;
}
#[tokio::test]
async fn invented_link_is_rejected_without_navigation_or_markdown_and_browser_is_closed() {
    run(true, false, false).await;
}
#[tokio::test]
async fn link_opening_a_new_tab_is_switched_before_reading() {
    run(false, true, false).await;
}
#[tokio::test]
async fn start_url_that_is_the_article_finishes_without_navigation() {
    run(false, false, true).await;
}
async fn run(bad_link: bool, new_tab: bool, direct: bool) {
    let state = Arc::new(Mutex::new(Mock {
        bad_link,
        new_tab,
        direct,
        ..Default::default()
    }));
    let mocks = Router::new()
        .route("/session", post(session))
        .route("/session/test-session/timeouts", post(empty))
        .route("/session/test-session/url", post(empty).get(current_url))
        .route("/session/test-session/window/handles", get(handles))
        .route("/session/test-session/window", post(switch))
        .route("/session/test-session/screenshot", get(screenshot))
        .route("/session/test-session/execute/sync", post(script))
        .route("/session/test-session/element/post-link/click", post(click))
        .route("/session/test-session", delete(close))
        .route("/responses", post(model))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mock_server = tokio::spawn(async move { axum::serve(listener, mocks).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let env_name = format!("AME_BROWSER_TEST_{}", uuid::Uuid::new_v4().simple());
    std::env::set_var(&env_name, "mock-capability");
    let mut config = Config::default();
    config.server.bind = "127.0.0.1:18766".parse().unwrap();
    config.device.automation.allow_process_actions = true;
    config.device.automation.dry_run = true;
    config.browser_tasks.enabled = true;
    config.browser_tasks.webdriver_url = base.clone();
    config.browser_tasks.model_url = base;
    config.browser_tasks.api_key_env = env_name.clone();
    config.browser_tasks.allowed_hosts = vec!["example.org".into()];
    config.browser_tasks.output_dir = temp.path().to_path_buf();
    config.validate().unwrap();
    let server = build_router(AppState::from_config(config, None, None));
    let request=Request::builder().method("POST").uri("/v1/tasks/browser").header("content-type","application/json").extension(ConnectInfo("127.0.0.1:9000".parse::<SocketAddr>().unwrap())).body(Body::from(json!({"goal":"Find latest article and summarize full text in Markdown","start_url":"https://example.org/","allow_cloud":true,"approve_read_only":true}).to_string())).unwrap();
    let response = server.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let result: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(state.lock().unwrap().closed);
    if direct {
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["status"], "completed");
        assert_eq!(result["source_url"], "https://example.org/");
        assert_eq!(state.lock().unwrap().calls, 1);
        assert_eq!(
            state.lock().unwrap().navigation_polls,
            0,
            "direct article must not need a second navigation"
        );
        let markdown = std::fs::read_to_string(result["markdown_path"].as_str().unwrap()).unwrap();
        assert!(markdown.contains("Directly opened article summary"));
    } else if bad_link {
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(!state.lock().unwrap().article);
        assert_eq!(state.lock().unwrap().calls, 1);
        assert!(!std::fs::read_dir(temp.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("summary.md")
            .exists());
    } else {
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["status"], "completed");
        assert!(state.lock().unwrap().full_text_sent);
        assert!(state.lock().unwrap().navigation_polls >= 3);
        assert_eq!(state.lock().unwrap().calls, 2);
        if new_tab {
            assert!(state.lock().unwrap().switched);
        }
        let path = result["markdown_path"].as_str().unwrap();
        let markdown = std::fs::read_to_string(path).unwrap();
        assert!(markdown.contains("first and second sections"));
        assert!(markdown.contains("https://example.org/new/"));
        assert_eq!(
            markdown.len(),
            result["markdown_bytes"].as_u64().unwrap() as usize
        );
    }
    std::env::remove_var(env_name);
    mock_server.abort();
}

/// `POST /v1/tasks/browser` must answer every rejection with the stable JSON
/// error envelope (SRV-06) instead of axum's plain-text defaults.
#[tokio::test]
async fn browser_task_rejections_use_the_stable_error_envelope() {
    let mut config = Config::default();
    config.server.bind = "127.0.0.1:18766".parse().unwrap();
    config.browser_tasks.enabled = false;
    let server = build_router(AppState::from_config(config, None, None));
    let peer: SocketAddr = "127.0.0.1:9000".parse().unwrap();

    // Malformed / unknown-field JSON -> 4xx INVALID_JSON envelope.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/tasks/browser")
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer))
        .body(Body::from(r#"{"goal":"x","unknown_field":1}"#))
        .unwrap();
    let response = server.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "INVALID_JSON", "{body}");

    // Oversized body -> 413 PAYLOAD_TOO_LARGE envelope.
    let oversized = format!(
        r#"{{"goal":"{}","start_url":"https://example.org/","allow_cloud":true,"approve_read_only":true}}"#,
        "a".repeat(64 * 1024)
    );
    let request = Request::builder()
        .method("POST")
        .uri("/v1/tasks/browser")
        .header("content-type", "application/json")
        .extension(ConnectInfo(peer))
        .body(Body::from(oversized))
        .unwrap();
    let response = server.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(body["error"]["code"], "PAYLOAD_TOO_LARGE", "{body}");
}
