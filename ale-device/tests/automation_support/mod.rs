//! Shared helpers for the automation integration tests.
//!
//! Included as a module from the `automation_*.rs` test targets with
//! `#[path = "automation_support/mod.rs"] mod support;`; it is not a test
//! binary itself.

#![allow(dead_code)]

use ale_core::actions::{Action, ActionPlan, MouseButton, RiskLevel};
use ale_device::automation::AutomationService;
use ale_device::config::AutomationConfig;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Serialises tests that mutate process-wide environment variables (`PATH`,
/// `ALE_AUTOMATION_VIRTUAL_DESKTOP`).
static ENV_LOCK: Mutex<()> = Mutex::new(());

pub const BOUNDS_ENV: &str = "ALE_AUTOMATION_VIRTUAL_DESKTOP";
/// Fixed virtual desktop used by the validation tests.
pub const TEST_BOUNDS: &str = "0,0,1920,1080";

pub fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Pin the virtual desktop so coordinate guards are deterministic and start-up
/// never probes a real display server.
pub fn with_test_bounds<T>(function: impl FnOnce() -> T) -> T {
    let _guard = lock_env();
    std::env::set_var(BOUNDS_ENV, TEST_BOUNDS);
    function()
}

pub fn test_config() -> AutomationConfig {
    AutomationConfig {
        timeout_ms: 5_000,
        ..AutomationConfig::default()
    }
}

pub fn plan(actions: Vec<Action>) -> ActionPlan {
    ActionPlan {
        actions,
        risk_level: RiskLevel::Low,
        explanation: "integration test plan".to_string(),
        requires_confirmation: false,
    }
}

pub fn move_to(x: f64, y: f64) -> Action {
    Action::MouseMove { x, y }
}

pub fn click(x: f64, y: f64, button: MouseButton) -> Action {
    Action::Click { x, y, button }
}

pub fn service(cfg: AutomationConfig) -> AutomationService {
    with_test_bounds(|| AutomationService::new(cfg))
}

/// Config that forces one specific (usually fake) backend executable.
pub fn backend_config(program: &Path) -> AutomationConfig {
    AutomationConfig {
        preferred_backend: Some(program.display().to_string()),
        ..test_config()
    }
}

pub fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("create temp dir")
}

pub fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Write an executable script file inside `directory`.
pub fn write_script(directory: &Path, name: &str, body: &str) -> PathBuf {
    let path = directory.join(name);
    std::fs::write(&path, body).expect("write fake backend");
    make_executable(&path);
    path
}

/// Fake backend that appends one `ARGV <arg> <arg> ...` line per invocation to
/// `log`, then runs `extra` before exiting 0.
pub fn logging_script(directory: &Path, name: &str, log: &Path, extra: &str) -> PathBuf {
    let body = format!(
        "#!/bin/sh\n\
         {{ printf 'ARGV'; printf ' <%s>' \"$@\"; printf '\\n'; }} >> {log}\n\
         {extra}\n\
         exit 0\n",
        log = shell_quote(log)
    );
    write_script(directory, name, &body)
}

pub fn read_log(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake backend");
    }
}

/// Prepends a directory to `PATH` and restores the previous value on drop.
pub struct PathGuard {
    previous: Option<std::ffi::OsString>,
}

impl PathGuard {
    pub fn prepend(directory: &Path) -> Self {
        let previous = std::env::var_os("PATH");
        let mut entries = vec![directory.to_path_buf()];
        if let Some(previous) = previous.as_ref() {
            entries.extend(std::env::split_paths(previous));
        }
        let joined = std::env::join_paths(entries).expect("join PATH entries");
        std::env::set_var("PATH", joined);
        Self { previous }
    }
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => std::env::set_var("PATH", previous),
            None => std::env::remove_var("PATH"),
        }
    }
}
