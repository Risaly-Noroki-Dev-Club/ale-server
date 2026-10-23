//! Cancellation, timeout and dry-run behaviour of the guarded executor.

#![cfg(unix)]

#[path = "automation_support/mod.rs"]
mod support;

use ale_core::actions::{Action, MouseButton};
use ale_device::automation::{AutomationService, ExecutionControl};
use ale_device::config::AutomationConfig;
use ale_device::error::DeviceError;
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::*;

#[test]
fn cancelling_a_long_wait_returns_promptly_and_reports_cancelled() {
    let cfg = AutomationConfig {
        timeout_ms: 30_000,
        ..test_config()
    };
    let service = Arc::new(service(cfg));
    let action_plan = plan(vec![Action::Wait { ms: 5_000 }]);
    let control = ExecutionControl::new();
    let worker = {
        let service = Arc::clone(&service);
        let control = control.clone();
        std::thread::spawn(move || service.execute_guarded(&action_plan, true, &control))
    };

    std::thread::sleep(Duration::from_millis(100));
    let cancelled_at = Instant::now();
    control.cancel();
    let outcome = worker
        .join()
        .expect("executor thread must not panic")
        .expect("cancellation is reported as a cancelled outcome, not an error");
    assert!(outcome.cancelled);
    assert_eq!(outcome.actions_executed, 0);
    assert!(!outcome.dry_run);
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "cancellation took {:?}",
        cancelled_at.elapsed()
    );
}

#[test]
fn cancelling_during_a_backend_command_kills_the_child() {
    let directory = tempdir();
    let log = directory.path().join("xdotool.log");
    let executable = logging_script(directory.path(), "xdotool", &log, "sleep 5");
    let service = Arc::new(service(backend_config(&executable)));
    let action_plan = plan(vec![move_to(10.0, 10.0)]);
    let control = ExecutionControl::new();
    let worker = {
        let service = Arc::clone(&service);
        let control = control.clone();
        std::thread::spawn(move || service.execute_guarded(&action_plan, false, &control))
    };

    // Give the executor time to spawn the sleeping fake backend.
    std::thread::sleep(Duration::from_millis(200));
    let cancelled_at = Instant::now();
    control.cancel();
    let outcome = worker
        .join()
        .expect("executor thread must not panic")
        .expect("cancellation is reported as a cancelled outcome");
    assert!(outcome.cancelled);
    assert_eq!(outcome.actions_executed, 0);
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(2),
        "cancellation took {:?}",
        cancelled_at.elapsed()
    );
}

#[test]
fn overall_timeout_kills_a_slow_backend() {
    let directory = tempdir();
    let log = directory.path().join("xdotool.log");
    let executable = logging_script(directory.path(), "xdotool", &log, "sleep 5");
    let cfg = AutomationConfig {
        timeout_ms: 150,
        ..backend_config(&executable)
    };
    let service = service(cfg);
    let started = Instant::now();
    let error = service
        .execute_guarded(
            &plan(vec![move_to(10.0, 10.0)]),
            false,
            &ExecutionControl::new(),
        )
        .expect_err("a slow backend must hit the overall deadline");
    assert!(matches!(error, DeviceError::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "timeout took {:?}",
        started.elapsed()
    );
}

#[test]
fn timeout_also_applies_to_wait_actions() {
    let cfg = AutomationConfig {
        timeout_ms: 200,
        ..test_config()
    };
    let service = service(cfg);
    let started = Instant::now();
    let error = service
        .execute_guarded(
            &plan(vec![Action::Wait { ms: 5_000 }]),
            true,
            &ExecutionControl::new(),
        )
        .expect_err("a long wait must hit the overall deadline");
    assert!(matches!(error, DeviceError::Timeout));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "timeout took {:?}",
        started.elapsed()
    );
}

#[test]
fn backend_failure_is_reported_as_a_backend_error() {
    let directory = tempdir();
    let executable = write_script(directory.path(), "xdotool", "#!/bin/sh\nexit 7\n");
    let service = service(backend_config(&executable));
    let error = service
        .execute_guarded(
            &plan(vec![move_to(10.0, 10.0)]),
            false,
            &ExecutionControl::new(),
        )
        .expect_err("a failing backend must fail the plan");
    match error {
        DeviceError::Backend(message) => {
            assert!(message.contains("exited with status"), "{message}")
        }
        other => panic!("expected a backend error, got {other:?}"),
    }
}

#[test]
fn dry_run_never_spawns_a_process() {
    // PATH is process-global: keep the mutation isolated from other tests.
    let _guard = lock_env();
    let directory = tempdir();
    let marker = directory.path().join("executed");
    let body = format!("#!/bin/sh\ntouch {}\nexit 1\n", shell_quote(&marker));
    // Fake xdotool that would fail loudly (and leave a marker) if it ran.
    write_script(directory.path(), "xdotool", &body);
    let _path_guard = PathGuard::prepend(directory.path());

    let cfg = AutomationConfig {
        dry_run: true,
        ..test_config()
    };
    // The environment lock above is already held for the PATH mutation, so do
    // not call `support::service` here: it would re-lock the non-reentrant
    // mutex. Dry-run mode performs no display probing either.
    let service = AutomationService::new(cfg);
    let action_plan = plan(vec![
        move_to(10.0, 10.0),
        click(10.0, 10.0, MouseButton::Left),
        Action::Type {
            text: "secret".to_string(),
        },
    ]);
    let outcome = service
        .execute_guarded(&action_plan, true, &ExecutionControl::new())
        .expect("dry-run plans succeed without executing anything");
    assert_eq!(outcome.actions_executed, 3);
    assert!(outcome.dry_run);
    assert!(!outcome.cancelled);
    assert!(!marker.exists(), "dry run must never spawn the backend");
    assert!(service.capabilities().available);
}

#[test]
fn pre_cancelled_control_returns_a_cancelled_outcome_without_spawning() {
    let directory = tempdir();
    let log = directory.path().join("xdotool.log");
    let executable = logging_script(directory.path(), "xdotool", &log, "");
    let service = service(backend_config(&executable));
    let control = ExecutionControl::new();
    control.cancel();
    let outcome = service
        .execute_guarded(&plan(vec![move_to(10.0, 10.0)]), false, &control)
        .expect("pre-cancelled execution is a cancelled outcome");
    assert!(outcome.cancelled);
    assert_eq!(outcome.actions_executed, 0);
    assert!(read_log(&log).is_empty());
}
