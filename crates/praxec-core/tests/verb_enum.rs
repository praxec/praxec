//! SPEC §5.4.1 — closed Verb enum. Atomic assertions per behavior.
//!
//! Every test names one observable property: a single positive accept, a
//! single negative reject, or one edge condition. No compound asserts.

use praxec_core::config;
use praxec_core::discovery::Verb;

fn skills_yaml(verb: &str) -> String {
    format!(
        r##"
version: "1.0.0"
skills:
  review.style.fixture:
    verb: {verb}
    lifecycle: stable
    body: "fixture body"
"##
    )
}

// ── Positive: each of the 10 blessed verbs loads cleanly. ─────────────────────

#[test]
fn verb_triage_loads() {
    config::resolve_str(&skills_yaml("triage")).expect("triage must load");
}

#[test]
fn verb_diagnose_loads() {
    config::resolve_str(&skills_yaml("diagnose")).expect("diagnose must load");
}

#[test]
fn verb_plan_loads() {
    config::resolve_str(&skills_yaml("plan")).expect("plan must load");
}

#[test]
fn verb_implement_loads() {
    config::resolve_str(&skills_yaml("implement")).expect("implement must load");
}

#[test]
fn verb_review_loads() {
    config::resolve_str(&skills_yaml("review")).expect("review must load");
}

#[test]
fn verb_refactor_loads() {
    config::resolve_str(&skills_yaml("refactor")).expect("refactor must load");
}

#[test]
fn verb_explain_loads() {
    config::resolve_str(&skills_yaml("explain")).expect("explain must load");
}

#[test]
fn verb_compose_loads() {
    config::resolve_str(&skills_yaml("compose")).expect("compose must load");
}

#[test]
fn verb_research_loads() {
    // SPEC §5.4.1 v0.3 — reconnaissance verb. Use a research.* subject so
    // strict-namespacing also accepts it (BLESSED_SUBJECT_ROOTS has
    // `research`).
    let yaml = r##"
version: "1.0.0"
skills:
  research.context.assemble:
    verb: research
    lifecycle: stable
    body: "fixture body"
"##;
    config::resolve_str(yaml).expect("research must load");
}

#[test]
fn verb_summarize_loads() {
    let yaml = r##"
version: "1.0.0"
skills:
  summarize.session.delta:
    verb: summarize
    lifecycle: stable
    body: "fixture body"
"##;
    config::resolve_str(yaml).expect("summarize must load");
}

// ── Negative: legacy verbs are rejected with explicit migration signal. ──────

#[test]
fn legacy_verb_apply_rejected() {
    let err = config::resolve_str(&skills_yaml("apply")).expect_err("apply must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("INVALID_VERB") && msg.contains("apply"),
        "error must name the rejected verb and the INVALID_VERB code; got: {msg}"
    );
}

#[test]
fn legacy_verb_check_rejected() {
    let err = config::resolve_str(&skills_yaml("check")).expect_err("check must be rejected");
    assert!(format!("{err}").contains("INVALID_VERB"));
}

#[test]
fn legacy_verb_avoid_rejected() {
    let err = config::resolve_str(&skills_yaml("avoid")).expect_err("avoid must be rejected");
    assert!(format!("{err}").contains("INVALID_VERB"));
}

#[test]
fn legacy_verb_follow_rejected() {
    let err = config::resolve_str(&skills_yaml("follow")).expect_err("follow must be rejected");
    assert!(format!("{err}").contains("INVALID_VERB"));
}

// ── Negative: error message lists all 10 allowed verbs. ──────────────────────

#[test]
fn invalid_verb_error_lists_all_ten() {
    let err = config::resolve_str(&skills_yaml("nonsense")).expect_err("nonsense must reject");
    let msg = format!("{err}");
    for v in Verb::ALL_TOKENS {
        assert!(
            msg.contains(v),
            "error must list verb '{v}' in the allowed set; got: {msg}"
        );
    }
}

// ── Edge: case-sensitivity. ─────────────────────────────────────────────────

#[test]
fn uppercase_verb_rejected() {
    let err = config::resolve_str(&skills_yaml("Review"))
        .expect_err("Review (capital R) must be rejected");
    assert!(format!("{err}").contains("INVALID_VERB"));
}

#[test]
fn allcaps_verb_rejected() {
    let err = config::resolve_str(&skills_yaml("REVIEW")).expect_err("REVIEW must be rejected");
    assert!(format!("{err}").contains("INVALID_VERB"));
}

// ── Edge: missing verb field. ───────────────────────────────────────────────

#[test]
fn missing_verb_field_rejected() {
    let yaml = r##"
version: "1.0.0"
skills:
  review.style.fixture:
    lifecycle: stable
    body: "no verb"
"##;
    let err = config::resolve_str(yaml).expect_err("missing verb must reject");
    assert!(
        format!("{err}").contains("MISSING_VERB"),
        "error must use MISSING_VERB code; got: {err}"
    );
}

// ── Helper enum invariant: from_token round-trips ──────────────────────────

#[test]
fn verb_from_token_roundtrips_every_value() {
    for token in Verb::ALL_TOKENS {
        let v = Verb::from_token(token).unwrap_or_else(|| panic!("from_token({token}) must parse"));
        assert_eq!(v.as_token(), *token, "{token} round-trip mismatch");
    }
}

#[test]
fn verb_from_token_rejects_unknown() {
    assert!(Verb::from_token("nonsense").is_none());
    assert!(Verb::from_token("").is_none());
    assert!(Verb::from_token("Review").is_none());
}
