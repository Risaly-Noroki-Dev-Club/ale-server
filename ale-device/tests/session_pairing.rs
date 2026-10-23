//! Pairing-code lifecycle tests: format, expiry-driven rotation and forced
//! rotation. These use the service directly (no sockets), so they stay fast and
//! deterministic.

#[path = "session_support/mod.rs"]
mod support;

use std::time::Duration;
use support::*;

#[test]
fn pairing_code_has_crockford_base32_shape() {
    let service = build_service(test_config());
    let code = service.pairing_code();
    assert_eq!(code.len(), 8);
    assert!(code
        .chars()
        .all(|character| character.is_ascii_digit() || character.is_ascii_uppercase()));
    for ambiguous in ['I', 'L', 'O', 'U'] {
        assert!(!code.contains(ambiguous), "ambiguous character in {code}");
    }
}

#[test]
fn pairing_code_expires_in_reports_remaining_ttl() {
    let service = build_service(test_config());
    let remaining = service.pairing_expires_in();
    assert!(
        remaining <= Duration::from_secs(300),
        "remaining {remaining:?} must not exceed the configured TTL"
    );
    assert!(
        remaining > Duration::from_secs(250),
        "a fresh code should be near its full TTL, got {remaining:?}"
    );
}

#[test]
fn rotate_pairing_code_forces_a_new_code() {
    let service = build_service(test_config());
    let first = service.pairing_code();
    let second = service.rotate_pairing_code();
    assert_ne!(first, second);
    assert_eq!(service.pairing_code(), second);
    assert!(service.pairing_expires_in() <= Duration::from_secs(300));
}

#[test]
fn pairing_code_rotates_after_ttl() {
    let mut config = test_config();
    config.pairing.code_ttl_s = 1;
    let service = build_service(config);

    let first = service.pairing_code();
    assert_eq!(
        service.pairing_code(),
        first,
        "code is stable before expiry"
    );
    assert!(service.pairing_expires_in() <= Duration::from_secs(1));

    std::thread::sleep(Duration::from_millis(1_100));
    let second = service.pairing_code();
    assert_ne!(first, second, "an expired pairing code must rotate");
    assert!(service.pairing_expires_in() <= Duration::from_secs(1));

    // Forcing a rotation again always produces a new code.
    let third = service.rotate_pairing_code();
    assert_ne!(second, third);
}
