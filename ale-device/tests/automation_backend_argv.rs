//! Exact backend argv generation, tested through fake executables passed via
//! `automation.preferred_backend` (absolute paths, so no real display server or
//! input tool is needed).

#![cfg(unix)]

#[path = "automation_support/mod.rs"]
mod support;

use ale_core::actions::{Action, MouseButton};
use ale_device::automation::ExecutionControl;
use ale_device::config::AutomationConfig;
use ale_device::error::DeviceError;
use support::*;

/// Fake backend that records `ARGV <arg> ...` per invocation.
fn recording_backend(
    directory: &std::path::Path,
    name: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = directory.join(format!("{name}.log"));
    let executable = logging_script(directory, name, &log, "");
    (executable, log)
}

#[test]
fn xdotool_generates_exact_argv() {
    let directory = tempdir();
    let (executable, log) = recording_backend(directory.path(), "xdotool");
    let service = service(backend_config(&executable));
    assert_eq!(service.backend_name().as_deref(), Some("xdotool"));
    assert!(service.capabilities().available);

    let plan = plan(vec![
        move_to(100.0, 200.0),
        click(100.0, 200.0, MouseButton::Left),
        Action::DoubleClick { x: 10.0, y: 20.0 },
        Action::Scroll {
            x: 1.0,
            y: 2.0,
            delta_x: 0.0,
            delta_y: 3.0,
        },
        Action::Type {
            text: "hello".to_string(),
        },
        Action::Key {
            key: "t".to_string(),
            modifiers: vec!["ctrl".to_string(), "shift".to_string()],
        },
    ]);
    let outcome = service
        .execute_guarded(&plan, true, &ExecutionControl::new())
        .expect("plan should execute");
    assert_eq!(outcome.actions_executed, 6);
    assert!(!outcome.cancelled);
    assert!(!outcome.dry_run);

    assert_eq!(
        read_log(&log),
        vec![
            "ARGV <mousemove> <--sync> <100> <200>",
            "ARGV <mousemove> <--sync> <100> <200>",
            "ARGV <click> <--clearmodifiers> <1>",
            "ARGV <mousemove> <--sync> <10> <20>",
            "ARGV <click> <--clearmodifiers> <--repeat> <2> <1>",
            "ARGV <mousemove> <--sync> <1> <2>",
            "ARGV <click> <--clearmodifiers> <--repeat> <3> <5>",
            "ARGV <type> <--clearmodifiers> <--> <hello>",
            "ARGV <key> <--clearmodifiers> <ctrl+shift+t>",
        ]
    );
}

#[test]
fn xdotool_negative_scroll_and_buttons_are_mapped() {
    let directory = tempdir();
    let (executable, log) = recording_backend(directory.path(), "xdotool");
    let service = service(backend_config(&executable));

    let plan = plan(vec![
        click(10.0, 20.0, MouseButton::Right),
        click(10.0, 20.0, MouseButton::Middle),
        Action::Scroll {
            x: 5.0,
            y: 6.0,
            delta_x: -2.0,
            delta_y: -1.0,
        },
    ]);
    service
        .execute_guarded(&plan, true, &ExecutionControl::new())
        .expect("plan should execute");

    assert_eq!(
        read_log(&log),
        vec![
            "ARGV <mousemove> <--sync> <10> <20>",
            "ARGV <click> <--clearmodifiers> <3>",
            "ARGV <mousemove> <--sync> <10> <20>",
            "ARGV <click> <--clearmodifiers> <2>",
            "ARGV <mousemove> <--sync> <5> <6>",
            "ARGV <click> <--clearmodifiers> <4>",
            "ARGV <click> <--clearmodifiers> <--repeat> <2> <6>",
        ]
    );
}

#[test]
fn ydotool_generates_exact_argv() {
    let directory = tempdir();
    let (executable, log) = recording_backend(directory.path(), "ydotool");
    let service = service(backend_config(&executable));
    assert_eq!(service.backend_name().as_deref(), Some("ydotool"));

    let plan = plan(vec![
        click(100.0, 200.0, MouseButton::Right),
        Action::DoubleClick { x: 10.0, y: 20.0 },
        Action::Scroll {
            x: 1.0,
            y: 2.0,
            delta_x: -1.0,
            delta_y: 2.0,
        },
        Action::Type {
            text: "hello".to_string(),
        },
        Action::Key {
            key: "t".to_string(),
            modifiers: vec!["ctrl".to_string()],
        },
    ]);
    service
        .execute_guarded(&plan, true, &ExecutionControl::new())
        .expect("plan should execute");

    assert_eq!(
        read_log(&log),
        vec![
            "ARGV <mousemove> <--absolute> <-x> <100> <-y> <200>",
            "ARGV <click> <0xC1>",
            "ARGV <mousemove> <--absolute> <-x> <10> <-y> <20>",
            "ARGV <click> <0xC0>",
            "ARGV <click> <0xC0>",
            "ARGV <mousemove> <--absolute> <-x> <1> <-y> <2>",
            "ARGV <mousemove> <--wheel> <-x> <-1> <-y> <-2>",
            "ARGV <type> <hello>",
            "ARGV <key> <29:1> <20:1> <20:0> <29:0>",
        ]
    );
}

#[test]
fn wtype_backend_types_text_and_presses_keys() {
    let directory = tempdir();
    let (executable, log) = recording_backend(directory.path(), "wtype");
    let service = service(backend_config(&executable));
    assert_eq!(service.backend_name().as_deref(), Some("wtype"));

    let action_plan = plan(vec![
        Action::Type {
            text: "hello".to_string(),
        },
        Action::Key {
            key: "Escape".to_string(),
            modifiers: Vec::new(),
        },
    ]);
    service
        .execute_guarded(&action_plan, true, &ExecutionControl::new())
        .expect("plan should execute");
    assert_eq!(
        read_log(&log),
        vec!["ARGV <--> <hello>", "ARGV <-k> <Escape>"]
    );

    // wtype cannot move the pointer; that must be a clear backend error, not a
    // panic or a silent success.
    let error = service
        .execute_guarded(
            &plan(vec![move_to(10.0, 10.0)]),
            false,
            &ExecutionControl::new(),
        )
        .expect_err("wtype must refuse pointer actions");
    match error {
        DeviceError::Backend(message) => assert!(message.contains("cannot move"), "{message}"),
        other => panic!("expected a backend error, got {other:?}"),
    }
}

#[test]
fn unapproved_medium_plan_is_not_executed() {
    let directory = tempdir();
    let (executable, log) = recording_backend(directory.path(), "xdotool");
    let service = service(backend_config(&executable));

    let error = service
        .execute_guarded(
            &plan(vec![click(10.0, 10.0, MouseButton::Left)]),
            false,
            &ExecutionControl::new(),
        )
        .expect_err("unapproved medium plan must be rejected");
    assert!(matches!(error, DeviceError::Denied(_)));
    assert!(
        read_log(&log).is_empty(),
        "nothing may be spawned before approval"
    );
}

#[test]
fn missing_preferred_backend_reports_unavailable() {
    let cfg = AutomationConfig {
        preferred_backend: Some("/nonexistent/ale/xdotool".to_string()),
        ..test_config()
    };
    let service = service(cfg);
    assert_eq!(service.backend_name(), None);
    let capabilities = service.capabilities();
    assert!(!capabilities.available);
    assert!(capabilities.backend.is_none());
    assert!(capabilities.error.is_some());

    let error = service
        .execute_guarded(
            &plan(vec![move_to(0.0, 0.0)]),
            false,
            &ExecutionControl::new(),
        )
        .expect_err("a missing backend must fail execution");
    assert!(matches!(error, DeviceError::Unavailable(_)));
}

#[test]
fn unsupported_key_names_are_rejected() {
    let directory = tempdir();
    let (executable, log) = recording_backend(directory.path(), "xdotool");
    let service = service(backend_config(&executable));

    let error = service
        .execute_guarded(
            &plan(vec![Action::Key {
                key: "totally-not-a-key".to_string(),
                modifiers: Vec::new(),
            }]),
            true,
            &ExecutionControl::new(),
        )
        .expect_err("unsupported keys must be rejected");
    assert!(matches!(error, DeviceError::Denied(_)));
    assert!(read_log(&log).is_empty());
}
