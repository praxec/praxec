//! FMECA R1 tests: failure classification + the "unknown surfaces, never
//! falls through" rule that prevents a whole class of silent-fallback
//! bugs (see the design plan).

use praxec_core::model_resolver::FailureClass;

#[test]
fn auth_failures_classified_as_infrastructure() {
    assert!(FailureClass::from_status(401).is_infrastructure());
    assert!(FailureClass::from_status(403).is_infrastructure());
    assert_eq!(FailureClass::from_status(401), FailureClass::Auth401);
    assert_eq!(FailureClass::from_status(403), FailureClass::Auth403);
}

#[test]
fn rate_limit_classified_as_infrastructure() {
    assert!(FailureClass::from_status(429).is_infrastructure());
    assert_eq!(FailureClass::from_status(429), FailureClass::RateLimit429);
}

#[test]
fn not_found_classified_as_infrastructure() {
    assert!(FailureClass::from_status(404).is_infrastructure());
    assert_eq!(FailureClass::from_status(404), FailureClass::NotFound404);
}

#[test]
fn bad_request_classified_as_content_other() {
    assert!(!FailureClass::from_status(400).is_infrastructure());
    assert_eq!(FailureClass::from_status(400), FailureClass::ContentOther);
}

#[test]
fn unprocessable_entity_surfaces() {
    assert!(!FailureClass::from_status(422).is_infrastructure());
    assert_eq!(FailureClass::from_status(422), FailureClass::ContentOther);
}

#[test]
fn unknown_4xx_defaults_to_content_other() {
    // 418 is a real HTTP status (I'm a teapot) but we don't enumerate
    // it; the classifier MUST default-surface rather than guess
    // "looks 4xx-ish, must be content-related, maybe try-next…".
    assert_eq!(FailureClass::from_status(418), FailureClass::ContentOther);
    assert!(!FailureClass::from_status(418).is_infrastructure());

    // Same for an unmapped 5xx (501 Not Implemented) — surface, not retry.
    assert_eq!(FailureClass::from_status(501), FailureClass::ContentOther);
    assert!(!FailureClass::from_status(501).is_infrastructure());
}

#[test]
fn five_oh_three_classified_as_network_timeout() {
    // 502/503/504 ARE classified as infrastructure (upstream gateway
    // failures, generally transient). That's the one exception to the
    // "5xx surfaces" rule and is documented in the classifier source.
    assert!(FailureClass::from_status(502).is_infrastructure());
    assert!(FailureClass::from_status(503).is_infrastructure());
    assert!(FailureClass::from_status(504).is_infrastructure());
    assert_eq!(FailureClass::from_status(503), FailureClass::NetworkTimeout);
}

#[test]
fn network_timeout_classified_from_io_error() {
    use std::io::ErrorKind;
    assert!(FailureClass::from_io_error(ErrorKind::TimedOut).is_infrastructure());
    assert!(FailureClass::from_io_error(ErrorKind::ConnectionRefused).is_infrastructure());
    assert!(FailureClass::from_io_error(ErrorKind::ConnectionReset).is_infrastructure());
}

#[test]
fn unrelated_io_error_surfaces() {
    // An IO error that doesn't look network-related (e.g., PermissionDenied
    // from a misconfigured cert path) shouldn't be treated as a transient
    // network blip — surface so the operator can fix the underlying issue.
    use std::io::ErrorKind;
    assert!(!FailureClass::from_io_error(ErrorKind::PermissionDenied).is_infrastructure());
    assert_eq!(
        FailureClass::from_io_error(ErrorKind::PermissionDenied),
        FailureClass::ContentOther
    );
}
