//! CLI behaviour tests (run the real binary, but never bind production ports).

use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_ale-server")
}

fn write_config(dir: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
    let path = dir.path().join("ale-server.toml");
    std::fs::write(&path, contents).expect("write test config");
    path
}

#[test]
fn version_exits_zero() {
    let output = Command::new(binary())
        .arg("--version")
        .output()
        .expect("run ale-server --version");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ale-server"), "stdout: {stdout}");
}

#[test]
fn check_config_prints_redacted_summary_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"
[server]
bind = "127.0.0.1:18080"

[engine]
enabled = false

[device]
enabled = false

[egress]
enabled = true
upstream = "https://api.openai.com"
allowed_hosts = ["api.openai.com"]
api_key_env = "ALE_SERVER_TEST_CLI_SECRET"

[logging]
level = "info"
format = "json"
"#,
    );

    let output = Command::new(binary())
        .args(["--check-config", "--config"])
        .arg(&path)
        .env("ALE_SERVER_TEST_CLI_SECRET", "sk-cli-secret-value")
        .env("ALE_SERVER__LOGGING__LEVEL", "warn")
        .output()
        .expect("run --check-config");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected success, stderr: {stderr}"
    );
    assert!(stdout.contains("127.0.0.1:18080"), "stdout: {stdout}");
    assert!(stdout.contains("\"level\": \"warn\""), "stdout: {stdout}");
    assert!(
        stdout.contains("\"api_key_configured\": true"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("sk-cli-secret-value") && !stderr.contains("sk-cli-secret-value"),
        "credential value leaked: stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn invalid_configuration_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(
        &dir,
        r#"
[egress]
enabled = true
upstream = "https://evil.example.com"
allowed_hosts = ["api.openai.com"]
api_key_env = "TEST_KEY"
"#,
    );

    let output = Command::new(binary())
        .args(["--check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run --check-config");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid configuration"), "stderr: {stderr}");
}

#[test]
fn malformed_configuration_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(&dir, "[server]\nbind = \"not-a-socket-address\"\n");

    let output = Command::new(binary())
        .args(["--check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run --check-config");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to load configuration"),
        "stderr: {stderr}"
    );
}

#[test]
fn missing_explicit_configuration_exits_non_zero() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does-not-exist.toml");

    let output = Command::new(binary())
        .args(["--check-config", "--config"])
        .arg(&path)
        .output()
        .expect("run --check-config");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed to load configuration"),
        "stderr: {stderr}"
    );
}

#[test]
fn check_config_and_print_pairing_conflict() {
    let output = Command::new(binary())
        .args(["--check-config", "--print-pairing"])
        .output()
        .expect("run ale-server");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "stderr: {stderr}"
    );
}
