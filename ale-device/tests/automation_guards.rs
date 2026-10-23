//! Guard-pipeline matrix for `AutomationService::validate_plan`.

#[path = "automation_support/mod.rs"]
mod support;

use ale_core::actions::{Action, FileOp, MouseButton, RiskLevel};
use ale_device::automation::{inferred_risk, ExecutionControl};
use ale_device::config::AutomationConfig;
use ale_device::error::DeviceError;
use support::*;

fn denied(result: std::result::Result<(), DeviceError>) -> String {
    match result {
        Ok(()) => panic!("expected the plan to be denied, but it was accepted"),
        Err(DeviceError::Denied(message)) => message,
        Err(other) => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn empty_plan_is_denied() {
    let service = service(test_config());
    let message = denied(service.validate_plan(&plan(Vec::new()), true));
    assert!(message.contains("invalid action plan"), "{message}");
}

#[test]
fn low_risk_plan_is_allowed_without_approval() {
    let service = service(test_config());
    let low_risk = plan(vec![move_to(100.0, 200.0), Action::Wait { ms: 10 }]);
    assert!(service.validate_plan(&low_risk, false).is_ok());
}

#[test]
fn approved_medium_plan_is_allowed() {
    let service = service(test_config());
    let medium = plan(vec![click(100.0, 200.0, MouseButton::Left)]);
    assert!(service.validate_plan(&medium, true).is_ok());
}

#[test]
fn unapproved_medium_plan_is_denied() {
    let service = service(test_config());
    let medium = plan(vec![click(100.0, 200.0, MouseButton::Left)]);
    let message = denied(service.validate_plan(&medium, false));
    assert!(message.contains("approval"), "{message}");
}

#[test]
fn claimed_low_risk_does_not_bypass_the_medium_gate() {
    let service = service(test_config());
    let mut lying_plan = plan(vec![Action::Type {
        text: "hello".to_string(),
    }]);
    lying_plan.risk_level = RiskLevel::Low;
    lying_plan.requires_confirmation = false;
    assert_eq!(inferred_risk(&lying_plan), RiskLevel::Medium);
    let message = denied(service.validate_plan(&lying_plan, false));
    assert!(message.contains("approval"), "{message}");
    assert!(service.validate_plan(&lying_plan, true).is_ok());
}

#[test]
fn requires_confirmation_is_denied_even_for_low_inferred_risk() {
    let service = service(test_config());
    let mut flagged = plan(vec![move_to(1.0, 1.0)]);
    flagged.requires_confirmation = true;
    let message = denied(service.validate_plan(&flagged, false));
    assert!(message.contains("approval"), "{message}");
    assert!(service.validate_plan(&flagged, true).is_ok());
}

#[test]
fn high_risk_requires_allow_high_risk() {
    let service = service(test_config());
    let close = plan(vec![Action::CloseApp {
        name: "firefox".to_string(),
    }]);
    let message = denied(service.validate_plan(&close, true));
    assert!(message.contains("allow_high_risk"), "{message}");
}

#[test]
fn high_risk_is_allowed_when_configured() {
    let cfg = AutomationConfig {
        allow_high_risk: true,
        ..test_config()
    };
    let service = service(cfg);
    let close = plan(vec![Action::CloseApp {
        name: "firefox".to_string(),
    }]);
    assert!(service.validate_plan(&close, true).is_ok());
}

#[test]
fn process_actions_require_allow_process_actions() {
    let service = service(test_config());
    let open_url = plan(vec![Action::OpenUrl {
        url: "https://example.com".to_string(),
    }]);
    let message = denied(service.validate_plan(&open_url, true));
    assert!(message.contains("allow_process_actions"), "{message}");

    let open_app = plan(vec![Action::OpenApp {
        name: "firefox".to_string(),
    }]);
    let message = denied(service.validate_plan(&open_app, true));
    assert!(message.contains("allow_process_actions"), "{message}");
}

#[test]
fn process_actions_are_allowed_when_configured() {
    let cfg = AutomationConfig {
        allow_process_actions: true,
        ..test_config()
    };
    let service = service(cfg);
    let open_url = plan(vec![Action::OpenUrl {
        url: "https://example.com".to_string(),
    }]);
    assert!(service.validate_plan(&open_url, true).is_ok());
    let open_app = plan(vec![Action::OpenApp {
        name: "firefox".to_string(),
    }]);
    assert!(service.validate_plan(&open_app, true).is_ok());
}

#[test]
fn controlled_test_click_is_always_denied() {
    let cfg = AutomationConfig {
        allow_high_risk: true,
        allow_process_actions: true,
        ..test_config()
    };
    let service = service(cfg);
    let controlled = plan(vec![Action::ControlledTestClick {
        x: 10.0,
        y: 10.0,
        window_title: "CONTROLLED".to_string(),
        target_name: "SAVE".to_string(),
        snapshot_id: "snapshot-1".to_string(),
    }]);
    let message = denied(service.validate_plan(&controlled, true));
    assert!(message.contains("ControlledTestClick"), "{message}");
}

#[test]
fn action_count_limit_is_enforced() {
    let cfg = AutomationConfig {
        max_actions: 2,
        ..test_config()
    };
    let service = service(cfg);
    let three = plan(vec![
        move_to(1.0, 1.0),
        move_to(2.0, 2.0),
        move_to(3.0, 3.0),
    ]);
    let message = denied(service.validate_plan(&three, true));
    assert!(message.contains("max_actions"), "{message}");

    let two = plan(vec![move_to(1.0, 1.0), move_to(2.0, 2.0)]);
    assert!(service.validate_plan(&two, true).is_ok());
}

#[test]
fn non_finite_coordinates_are_denied() {
    let service = service(test_config());
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let plan = plan(vec![click(bad, 10.0, MouseButton::Left)]);
        let message = denied(service.validate_plan(&plan, true));
        assert!(message.contains("invalid action plan"), "{message}");
    }
}

#[test]
fn out_of_bounds_coordinates_are_denied_with_tolerance() {
    let service = service(test_config());
    // The virtual desktop is 1920x1080 at (0, 0); 4 px of slack is accepted.
    let last_valid = 1919.0 + 4.0;
    assert!(service
        .validate_plan(&plan(vec![move_to(last_valid, 100.0)]), false)
        .is_ok());
    let message =
        denied(service.validate_plan(&plan(vec![move_to(last_valid + 1.0, 100.0)]), false));
    assert!(message.contains("outside the virtual desktop"), "{message}");

    assert!(service
        .validate_plan(&plan(vec![click(-4.0, 100.0, MouseButton::Left)]), true)
        .is_ok());
    let message =
        denied(service.validate_plan(&plan(vec![click(-5.0, 100.0, MouseButton::Left)]), true));
    assert!(message.contains("outside the virtual desktop"), "{message}");

    let message = denied(service.validate_plan(&plan(vec![move_to(5_000.0, 100.0)]), false));
    assert!(message.contains("outside the virtual desktop"), "{message}");
    let message = denied(service.validate_plan(&plan(vec![move_to(100.0, 5_000.0)]), false));
    assert!(message.contains("outside the virtual desktop"), "{message}");
}

#[test]
fn bounds_check_is_skipped_when_bounds_are_unknown() {
    // In dry-run mode the service deliberately does not probe the display
    // server, so the coordinate bounds are unknown and the check is skipped.
    let cfg = AutomationConfig {
        dry_run: true,
        ..test_config()
    };
    let service = service(cfg);
    assert!(service
        .validate_plan(&plan(vec![move_to(5_000.0, 5_000.0)]), false)
        .is_ok());
}

#[test]
fn file_operations_are_denied_at_execution() {
    let cfg = AutomationConfig {
        allow_high_risk: true,
        ..test_config()
    };
    let service = service(cfg);
    let file_plan = plan(vec![Action::FileOperation {
        operation: FileOp::Delete,
        path: "/tmp/ale-test".to_string(),
        target: None,
    }]);
    // The guard pipeline allows it once high risk is configured, but this build
    // deliberately refuses to mutate the file system.
    assert!(service.validate_plan(&file_plan, true).is_ok());
    let error = service
        .execute_guarded(&file_plan, true, &ExecutionControl::new())
        .expect_err("file operations must be refused");
    match error {
        DeviceError::Denied(message) => assert!(message.contains("file operations"), "{message}"),
        other => panic!("expected a denial, got {other:?}"),
    }
}

#[test]
fn capabilities_report_configured_policy() {
    let cfg = AutomationConfig {
        dry_run: true,
        allow_high_risk: true,
        allow_process_actions: true,
        ..test_config()
    };
    let capabilities: ale_device::automation::AutomationCapabilities = service(cfg).capabilities();
    assert!(capabilities.available);
    assert!(capabilities.dry_run);
    assert!(capabilities.allow_high_risk);
    assert!(capabilities.allow_process_actions);
}
