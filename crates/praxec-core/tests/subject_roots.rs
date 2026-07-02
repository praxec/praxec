//! SPEC §5.4.2 — blessed subject root namespace, with `strict_namespacing`
//! flag governing whether unblessed roots error or warn.

use praxec_core::config;
use praxec_core::discovery::BLESSED_SUBJECT_ROOTS;

fn skills_yaml(subject: &str, strict: Option<bool>) -> String {
    let praxec_block = match strict {
        Some(b) => format!("praxec:\n  strict_namespacing: {b}\n"),
        None => String::new(),
    };
    format!(
        r##"
version: "1.0.0"
{praxec_block}skills:
  {subject}:
    verb: review
    lifecycle: stable
    body: "fixture body"
"##
    )
}

// ── Positive: each blessed root accepted ────────────────────────────────────

#[test]
fn blessed_root_review_accepted() {
    config::resolve_str(&skills_yaml("review.style.x", None)).expect("review.* must accept");
}

#[test]
fn blessed_root_authoring_accepted() {
    config::resolve_str(&skills_yaml("authoring.skill.x", None)).expect("authoring.* must accept");
}

#[test]
fn blessed_root_debug_accepted() {
    config::resolve_str(&skills_yaml("debug.repro.x", None)).expect("debug.* must accept");
}

#[test]
fn blessed_root_deploy_accepted() {
    config::resolve_str(&skills_yaml("deploy.safety.x", None)).expect("deploy.* must accept");
}

#[test]
fn blessed_root_import_accepted() {
    config::resolve_str(&skills_yaml("import.mattpocock.x", None)).expect("import.* must accept");
}

#[test]
fn blessed_root_lifecycle_accepted() {
    config::resolve_str(&skills_yaml("lifecycle.drafting.x", None))
        .expect("lifecycle.* must accept");
}

#[test]
fn blessed_root_plan_accepted() {
    config::resolve_str(&skills_yaml("plan.execute.pr-scope", None)).expect("plan.* must accept");
}

#[test]
fn blessed_root_plan_specify_subpath_accepted() {
    config::resolve_str(&skills_yaml("plan.specify.adr.architecture", None))
        .expect("plan.specify.* must accept");
}

#[test]
fn blessed_root_plan_execute_subpath_accepted() {
    config::resolve_str(&skills_yaml("plan.execute.sprint.breakdown", None))
        .expect("plan.execute.* must accept");
}

// ── SPEC §5.4.1 v0.3 expansion: research + summarize roots ─────────────────

#[test]
fn blessed_root_research_accepted() {
    config::resolve_str(&skills_yaml("research.context.assemble", None))
        .expect("research.* must accept");
}

#[test]
fn blessed_root_summarize_accepted() {
    config::resolve_str(&skills_yaml("summarize.session.delta", None))
        .expect("summarize.* must accept");
}

// ── Negative under strict (default): unblessed root rejected ─────────────────

#[test]
fn unblessed_root_rejected_under_default_strict() {
    let err = config::resolve_str(&skills_yaml("nonsense.foo.bar", None))
        .expect_err("unblessed root must reject under default strict_namespacing");
    let msg = format!("{err}");
    assert!(
        msg.contains("INVALID_SUBJECT_ROOT"),
        "error must use INVALID_SUBJECT_ROOT code; got: {msg}"
    );
    assert!(
        msg.contains("nonsense"),
        "error must name the unblessed root; got: {msg}"
    );
}

#[test]
fn unblessed_root_rejected_under_explicit_strict() {
    let err = config::resolve_str(&skills_yaml("nonsense.foo.bar", Some(true)))
        .expect_err("unblessed root must reject under strict_namespacing: true");
    assert!(format!("{err}").contains("INVALID_SUBJECT_ROOT"));
}

#[test]
fn invalid_subject_root_error_lists_all_blessed_roots() {
    let err = config::resolve_str(&skills_yaml("nonsense.foo.bar", None)).expect_err("error path");
    let msg = format!("{err}");
    for root in BLESSED_SUBJECT_ROOTS {
        assert!(
            msg.contains(root),
            "error must list blessed root '{root}'; got: {msg}"
        );
    }
}

// ── Permissive: unblessed root accepted when strict=false ────────────────────

#[test]
fn unblessed_root_accepted_when_strict_off() {
    // Under strict_namespacing: false the subject loads but a diagnostic
    // surfaces via the check pass (verified separately).
    config::resolve_str(&skills_yaml("nonsense.foo.bar", Some(false)))
        .expect("strict_namespacing: false must permit unblessed root");
}

// ── Edge: subject pattern (dotted, kebab) enforced regardless of strictness ─

#[test]
fn empty_subject_rejected() {
    // An empty key isn't directly expressible in YAML; use the JSON path
    // and route through `resolve` directly.
    use serde_json::json;
    let cfg = json!({
        "version": "1.0.0",
        "skills": {
            "": { "verb": "review", "lifecycle": "stable", "body": "x" }
        }
    });
    let err = config::resolve(cfg).expect_err("empty subject must reject");
    assert!(format!("{err}").contains("EMPTY_SUBJECT"));
}

#[test]
fn single_segment_subject_rejected() {
    // SPEC §5.4.2: subject MUST have at least two dotted segments.
    let err = config::resolve_str(&skills_yaml("review", None))
        .expect_err("single-segment subject must reject (must be dotted)");
    assert!(
        format!("{err}").contains("must match"),
        "error must name the pattern; got: {err}"
    );
}

#[test]
fn uppercase_subject_segment_rejected() {
    let err = config::resolve_str(&skills_yaml("Review.style.x", None))
        .expect_err("uppercase first segment must reject");
    let msg = format!("{err}");
    assert!(
        msg.contains("Review"),
        "error must name the offending subject; got: {msg}"
    );
}

#[test]
fn whitespace_in_subject_rejected() {
    use serde_json::json;
    let cfg = json!({
        "version": "1.0.0",
        "skills": {
            "review.style.with space": {
                "verb": "review", "lifecycle": "stable", "body": "x"
            }
        }
    });
    let err = config::resolve(cfg).expect_err("subject with whitespace must reject");
    assert!(format!("{err}").contains("must match"));
}
