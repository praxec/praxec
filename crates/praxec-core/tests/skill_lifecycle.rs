//! SPEC §5.3 — required `lifecycle` field on every fragment, closed enum
//! `experimental` | `stable` | `deprecated`, no silent default.

use praxec_core::config;
use praxec_core::discovery::Lifecycle;

fn skills_yaml(lifecycle: &str) -> String {
    format!(
        r##"
version: "1.0.0"
skills:
  review.style.fixture:
    verb: review
    lifecycle: {lifecycle}
    body: "fixture body"
"##
    )
}

// ── Positive: each of three lifecycle values loads ──────────────────────────

#[test]
fn lifecycle_experimental_loads() {
    config::resolve_str(&skills_yaml("experimental")).expect("experimental must load");
}

#[test]
fn lifecycle_stable_loads() {
    config::resolve_str(&skills_yaml("stable")).expect("stable must load");
}

#[test]
fn lifecycle_deprecated_loads() {
    config::resolve_str(&skills_yaml("deprecated")).expect("deprecated must load");
}

// ── Negative: missing lifecycle fails fast (no silent default) ──────────────

#[test]
fn missing_lifecycle_field_rejected() {
    let yaml = r##"
version: "1.0.0"
skills:
  review.style.fixture:
    verb: review
    body: "fixture body"
"##;
    let err = config::resolve_str(yaml).expect_err("missing lifecycle must reject");
    let msg = format!("{err}");
    assert!(
        msg.contains("MISSING_LIFECYCLE"),
        "error must use MISSING_LIFECYCLE code; got: {msg}"
    );
}

// ── Negative: unknown lifecycle value rejected ──────────────────────────────

#[test]
fn unknown_lifecycle_value_rejected() {
    let err = config::resolve_str(&skills_yaml("beta")).expect_err("'beta' must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("INVALID_LIFECYCLE") && msg.contains("beta"),
        "error must use INVALID_LIFECYCLE and name the value; got: {msg}"
    );
}

#[test]
fn invalid_lifecycle_error_lists_all_three() {
    let err = config::resolve_str(&skills_yaml("alpha")).expect_err("alpha must reject");
    let msg = format!("{err}");
    for v in Lifecycle::ALL_TOKENS {
        assert!(
            msg.contains(v),
            "error must list lifecycle value '{v}'; got: {msg}"
        );
    }
}

// ── Edge: case-sensitive ────────────────────────────────────────────────────

#[test]
fn uppercase_lifecycle_rejected() {
    let err =
        config::resolve_str(&skills_yaml("Stable")).expect_err("Stable (capital S) must reject");
    assert!(format!("{err}").contains("INVALID_LIFECYCLE"));
}

// ── Positive: stamped library carries the lifecycle value ───────────────────

#[test]
fn lifecycle_round_trips_to_skills_library() {
    let resolved = config::resolve_str(&skills_yaml("experimental")).expect("config resolves");
    // The top-level `skills:` map preserves the field.
    let v = resolved
        .pointer("/skills/review.style.fixture/lifecycle")
        .and_then(serde_json::Value::as_str)
        .expect("top-level skills entry must carry lifecycle");
    assert_eq!(v, "experimental");
}

// ── Helper enum invariant ───────────────────────────────────────────────────

#[test]
fn lifecycle_from_token_roundtrips() {
    for token in Lifecycle::ALL_TOKENS {
        let l = Lifecycle::from_token(token)
            .unwrap_or_else(|| panic!("Lifecycle::from_token({token}) must parse"));
        assert_eq!(l.as_token(), *token);
    }
}

#[test]
fn lifecycle_from_token_rejects_unknown() {
    assert!(Lifecycle::from_token("beta").is_none());
    assert!(Lifecycle::from_token("").is_none());
    assert!(Lifecycle::from_token("Stable").is_none());
}
