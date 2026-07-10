//! FMECA T1 test: an `--agent` CLI override + on-disk `models.yaml` must be
//! mutually exclusive at startup. Exercises the pure validator that
//! agent-source resolution delegates to.

use praxec_core::model_resolver::{AmbiguousModelSourceError, validate_model_source_exclusivity};

#[test]
fn cli_flag_and_yaml_both_present_fails_startup() {
    let err = validate_model_source_exclusivity(true, true)
        .expect_err("yaml + --agent simultaneously must be an error");
    let msg = err.to_string();
    assert!(
        msg.contains("ambiguous"),
        "error message must name the ambiguity: {msg}"
    );
    assert!(
        msg.contains("--agent"),
        "error message must mention --agent: {msg}"
    );
    assert!(
        msg.contains("models.yaml"),
        "error message must mention models.yaml: {msg}"
    );
}

#[test]
fn yaml_only_passes() {
    validate_model_source_exclusivity(true, false).expect("yaml without --agent is fine");
}

#[test]
fn cli_flag_only_passes() {
    // Legacy v0.2 path — deprecated but still allowed.
    validate_model_source_exclusivity(false, true).expect("--agent without yaml is fine");
}

#[test]
fn neither_present_passes() {
    // Caller's job to handle the "no agents" case; the exclusivity
    // check itself is fine with neither.
    validate_model_source_exclusivity(false, false).expect("neither set is fine");
}

#[test]
fn error_type_is_zero_sized() {
    // The error carries no data — all the context is in the static
    // message. Pin the struct shape so future contributors don't
    // accidentally bloat it.
    assert_eq!(std::mem::size_of::<AmbiguousModelSourceError>(), 0);
}
