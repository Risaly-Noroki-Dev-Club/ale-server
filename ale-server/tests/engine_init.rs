//! Lazy inference engine tests: initialization failures must never crash the
//! process, they only make inference routes unavailable.

use ale_server::app::{EngineError, EngineState};
use ale_server::config::EngineConfig;
use ale_server::secrets::SecretStoreKind;

/// Point the engine at an un-writable configuration path so initialization fails
/// fast and deterministically (no model loading, no network, no keyring).
fn failing_engine_config(dir: &tempfile::TempDir) -> EngineConfig {
    let blocker = dir.path().join("not-a-directory");
    std::fs::write(&blocker, b"blocker").unwrap();
    EngineConfig {
        enabled: true,
        config_path: Some(blocker.join("config.json")),
        secret_store: SecretStoreKind::File,
        secret_store_path: Some(dir.path().join("secrets.json")),
    }
}

#[tokio::test]
async fn engine_init_failure_is_non_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let engine = EngineState::new(&failing_engine_config(&dir));

    let error = match engine.acquire().await {
        Ok(_) => panic!("initialization must fail for the blocked config path"),
        Err(error) => error,
    };
    match error {
        EngineError::Unavailable(message) => {
            assert!(!message.is_empty());
        }
        EngineError::Disabled => panic!("engine was enabled"),
    }
    assert!(!engine.is_ready());
    assert!(engine.last_error().is_some());

    // A second call while the retry cooldown is active must fail fast without
    // re-running initialization (and still must not panic).
    let second = match engine.acquire().await {
        Ok(_) => panic!("cooldown must keep the engine unavailable"),
        Err(error) => error,
    };
    assert!(matches!(second, EngineError::Unavailable(_)));
    assert!(!engine.is_ready());
}

#[tokio::test]
async fn disabled_engine_is_reported_without_initializing() {
    let config = EngineConfig {
        enabled: false,
        ..Default::default()
    };
    let engine = EngineState::new(&config);
    match engine.acquire().await {
        Ok(_) => panic!("disabled engine must not initialize"),
        Err(error) => assert!(matches!(error, EngineError::Disabled)),
    }
    assert!(engine.last_error().is_none());
    assert!(!engine.is_ready());
}
