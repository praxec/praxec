//! SPEC §5.4.2 + audit-resolution C.2 — `strict_namespacing: false`
//! surfaces unblessed-subject-root issues as soft diagnostics from
//! `resolve_with_diagnostics`. The plain `resolve()` API stays
//! backward-compatible (silent acceptance of the same input).

use praxec_core::config::{self, DiagnosticSeverity};
use serde_json::json;

fn skills_yaml_with(subject: &str, strict: bool) -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": strict },
        "skills": {
            subject: {
                "verb": "review",
                "lifecycle": "stable",
                "body": "x"
            }
        }
    })
}

// ── Backward-compat: resolve() returns Value unchanged ─────────────────────

#[test]
fn resolve_in_lenient_mode_with_unblessed_root_still_succeeds() {
    let cfg = skills_yaml_with("nonsense.foo.bar", false);
    config::resolve(cfg).expect("resolve in lenient mode must accept unblessed root");
}

#[test]
fn resolve_in_strict_mode_with_unblessed_root_fails() {
    let cfg = skills_yaml_with("nonsense.foo.bar", true);
    let err = config::resolve(cfg).expect_err("strict mode rejects");
    assert!(format!("{err}").contains("INVALID_SUBJECT_ROOT"));
}

// ── Diagnostic surface (lenient mode) ──────────────────────────────────────

#[test]
fn lenient_mode_emits_invalid_subject_root_warning() {
    let cfg = skills_yaml_with("nonsense.foo.bar", false);
    let (_resolved, diagnostics) = config::resolve_with_diagnostics(cfg).expect("resolve succeeds");
    let warn = diagnostics
        .iter()
        .find(|d| d.code == "INVALID_SUBJECT_ROOT")
        .expect("expected an INVALID_SUBJECT_ROOT diagnostic");
    assert_eq!(warn.severity, DiagnosticSeverity::Warn);
    assert!(warn.message.contains("nonsense"));
}

#[test]
fn diagnostic_includes_location_pointing_at_offending_subject() {
    let cfg = skills_yaml_with("nonsense.foo.bar", false);
    let (_resolved, diagnostics) = config::resolve_with_diagnostics(cfg).expect("resolve");
    let warn = diagnostics
        .iter()
        .find(|d| d.code == "INVALID_SUBJECT_ROOT")
        .expect("present");
    assert_eq!(
        warn.location.as_deref(),
        Some("/skills/nonsense.foo.bar"),
        "location must JSON-Pointer the offending subject"
    );
}

#[test]
fn closest_blessed_root_suggestion_when_one_exists() {
    // `revoew` shares the prefix `re` with `review` → suggestion fires.
    let cfg = skills_yaml_with("revoew.style.x", false);
    let (_resolved, diagnostics) = config::resolve_with_diagnostics(cfg).expect("resolve");
    let warn = diagnostics
        .iter()
        .find(|d| d.code == "INVALID_SUBJECT_ROOT")
        .expect("present");
    let suggestion = warn
        .suggestion
        .as_deref()
        .expect("suggestion present for prefix-share candidate");
    assert!(
        suggestion.contains("review"),
        "expected 'review' suggestion for 'revoew'; got: {suggestion}"
    );
}

#[test]
fn no_suggestion_when_no_prefix_overlap() {
    // `zzzz` shares no prefix with any blessed root.
    let cfg = skills_yaml_with("zzzz.foo.bar", false);
    let (_resolved, diagnostics) = config::resolve_with_diagnostics(cfg).expect("resolve");
    let warn = diagnostics
        .iter()
        .find(|d| d.code == "INVALID_SUBJECT_ROOT")
        .expect("present");
    assert!(
        warn.suggestion.is_none(),
        "no suggestion expected when no prefix overlap; got: {:?}",
        warn.suggestion
    );
}

// ── Lenient-mode passes still produce no diagnostics for blessed roots ─────

#[test]
fn blessed_root_in_lenient_mode_produces_no_warning() {
    let cfg = skills_yaml_with("review.style.house-voice", false);
    let (_resolved, diagnostics) = config::resolve_with_diagnostics(cfg).expect("resolve");
    let invalid: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == "INVALID_SUBJECT_ROOT")
        .collect();
    assert!(
        invalid.is_empty(),
        "blessed root must not produce INVALID_SUBJECT_ROOT diagnostic; got: {invalid:?}"
    );
}

// ── Multiple unblessed roots produce multiple diagnostics ──────────────────

#[test]
fn multiple_unblessed_roots_each_produce_a_diagnostic() {
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "skills": {
            "nonsense.a.b": { "verb": "review", "lifecycle": "stable", "body": "x" },
            "other.c.d":    { "verb": "review", "lifecycle": "stable", "body": "x" }
        }
    });
    let (_resolved, diagnostics) = config::resolve_with_diagnostics(cfg).expect("resolve");
    let count = diagnostics
        .iter()
        .filter(|d| d.code == "INVALID_SUBJECT_ROOT")
        .count();
    assert_eq!(count, 2, "expected one diagnostic per unblessed subject");
}

// ── resolve_str (string-form) returns hard errors only; loses diagnostics ──

#[test]
fn resolve_str_silently_discards_diagnostics_for_backcompat() {
    // resolve_str -> resolve. Soft diagnostics are dropped. Verify the
    // call succeeds and produces a config with the unblessed subject.
    let yaml = r#"
version: "1.0.0"
praxec:
  strict_namespacing: false
skills:
  nonsense.x.y:
    verb: review
    lifecycle: stable
    body: "x"
"#;
    let resolved = config::resolve_str(yaml).expect("resolve_str");
    assert!(resolved.pointer("/skills/nonsense.x.y").is_some());
}
