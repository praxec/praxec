//! SPEC §20.1 — `min_confidence` and `require_digest` clauses on the
//! `evidence` guard's `requires:` entries. Atomic assertions covering:
//!   - default behavior (no new clauses) unchanged from existing semantics
//!   - `require_digest: true` excludes records missing digest
//!   - `min_confidence: N` excludes records below threshold or with no
//!     confidence at all
//!   - both clauses combine
//!   - quorum counting respects the filters

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Evidence, Principal, WorkflowInstance};
use praxec_core::ports::{EvidenceStore, GuardEvaluator};
use serde_json::{Value, json};

/// In-memory evidence store seeded at construction time so tests can pin
/// exactly which records the guard sees.
struct FixedEvidence(Vec<Evidence>);

#[async_trait]
impl EvidenceStore for FixedEvidence {
    async fn record(&self, _wf: &str, _e: Evidence) -> anyhow::Result<()> {
        Ok(())
    }
    async fn list(&self, _wf: &str) -> anyhow::Result<Vec<Evidence>> {
        Ok(self.0.clone())
    }
}

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_test".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: Value::Null,
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        run_env: praxec_core::RunEnv::for_test(),
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn ev(kind: &str, digest: Option<&str>, confidence: Option<f32>) -> Evidence {
    Evidence {
        kind: kind.into(),
        id: format!("ev_{kind}_{}", uuid::Uuid::new_v4().simple()),
        uri: None,
        summary: None,
        digest: digest.map(str::to_string),
        confidence,
    }
}

fn build_evaluator(records: Vec<Evidence>) -> DefaultGuardEvaluator {
    DefaultGuardEvaluator::with_evidence(Arc::new(FixedEvidence(records)) as Arc<dyn EvidenceStore>)
}

async fn evaluate(records: Vec<Evidence>, guard: Value) -> bool {
    let evaluator = build_evaluator(records);
    evaluator
        .evaluate(
            &guard,
            &instance_stub(),
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate")
}

// ── Baseline (no new clauses): behavior preserved ──────────────────────────

#[tokio::test]
async fn legacy_string_requirement_still_works() {
    let records = vec![ev("approval", None, None)];
    let pass = evaluate(
        records,
        json!({ "kind": "evidence", "requires": ["approval"] }),
    )
    .await;
    assert!(pass, "string requirement of an existing record must pass");
}

#[tokio::test]
async fn legacy_count_requirement_still_works() {
    let records = vec![ev("approval", None, None), ev("approval", None, None)];
    let pass = evaluate(
        records,
        json!({ "kind": "evidence", "requires": [{ "kind": "approval", "count": 2 }] }),
    )
    .await;
    assert!(pass);
}

#[tokio::test]
async fn legacy_count_fails_short_of_quorum() {
    let records = vec![ev("approval", None, None)];
    let pass = evaluate(
        records,
        json!({ "kind": "evidence", "requires": [{ "kind": "approval", "count": 2 }] }),
    )
    .await;
    assert!(!pass);
}

// ── require_digest ────────────────────────────────────────────────────────

#[tokio::test]
async fn require_digest_excludes_records_with_no_digest() {
    let records = vec![ev("build", None, None)];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 1, "require_digest": true }]
        }),
    )
    .await;
    assert!(
        !pass,
        "record without digest must not satisfy require_digest"
    );
}

#[tokio::test]
async fn require_digest_accepts_records_with_digest() {
    let records = vec![ev("build", Some("sha256:abc"), None)];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 1, "require_digest": true }]
        }),
    )
    .await;
    assert!(pass);
}

#[tokio::test]
async fn require_digest_false_accepts_records_without_digest() {
    let records = vec![ev("build", None, None)];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 1, "require_digest": false }]
        }),
    )
    .await;
    assert!(pass);
}

#[tokio::test]
async fn require_digest_counts_only_digested_records_toward_quorum() {
    let records = vec![
        ev("build", Some("sha256:a"), None),
        ev("build", None, None),
        ev("build", Some("sha256:b"), None),
    ];
    // Two records have digests, one doesn't. Quorum of 2 must pass.
    let pass = evaluate(
        records.clone(),
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 2, "require_digest": true }]
        }),
    )
    .await;
    assert!(pass);

    // Quorum of 3 must fail (only 2 digested records).
    let pass3 = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 3, "require_digest": true }]
        }),
    )
    .await;
    assert!(!pass3);
}

// ── min_confidence ────────────────────────────────────────────────────────

#[tokio::test]
async fn min_confidence_excludes_record_below_threshold() {
    let records = vec![ev("approval", None, Some(0.5))];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "approval", "count": 1, "min_confidence": 0.7 }]
        }),
    )
    .await;
    assert!(!pass);
}

#[tokio::test]
async fn min_confidence_accepts_record_at_or_above_threshold() {
    let records = vec![ev("approval", None, Some(0.85))];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "approval", "count": 1, "min_confidence": 0.7 }]
        }),
    )
    .await;
    assert!(pass);
}

#[tokio::test]
async fn min_confidence_accepts_record_at_exact_threshold() {
    let records = vec![ev("approval", None, Some(0.70))];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "approval", "count": 1, "min_confidence": 0.70 }]
        }),
    )
    .await;
    assert!(pass, "equal-to-threshold must satisfy");
}

#[tokio::test]
async fn min_confidence_excludes_records_with_no_confidence_field() {
    let records = vec![ev("approval", None, None)];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "approval", "count": 1, "min_confidence": 0.5 }]
        }),
    )
    .await;
    assert!(
        !pass,
        "missing confidence must NOT satisfy min_confidence (explicit opt-in)"
    );
}

#[tokio::test]
async fn min_confidence_counts_only_qualifying_records_toward_quorum() {
    let records = vec![
        ev("approval", None, Some(0.9)),
        ev("approval", None, Some(0.4)),
        ev("approval", None, Some(0.85)),
        ev("approval", None, None),
    ];
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "approval", "count": 2, "min_confidence": 0.8 }]
        }),
    )
    .await;
    assert!(pass, "two records ≥ 0.8 satisfy quorum-of-2");
}

// ── Combined ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn combined_filters_intersect() {
    // Records: one with both, one missing each, one with neither.
    let records = vec![
        ev("build", Some("sha256:a"), Some(0.9)), // both → counts
        ev("build", Some("sha256:b"), Some(0.4)), // low confidence → drops
        ev("build", None, Some(0.95)),            // no digest → drops
        ev("build", None, None),                  // both missing → drops
    ];
    let pass = evaluate(
        records.clone(),
        json!({
            "kind": "evidence",
            "requires": [{
                "kind": "build", "count": 1,
                "require_digest": true, "min_confidence": 0.7
            }]
        }),
    )
    .await;
    assert!(pass, "exactly one record satisfies both filters");

    // Quorum of 2 must fail — only one record qualifies.
    let pass2 = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [{
                "kind": "build", "count": 2,
                "require_digest": true, "min_confidence": 0.7
            }]
        }),
    )
    .await;
    assert!(!pass2);
}

// ── Multi-requirement: each requires entry is ANDed ─────────────────────────

#[tokio::test]
async fn multiple_requires_entries_must_all_satisfy() {
    let records = vec![ev("build", Some("sha256:a"), None), ev("test", None, None)];
    // build with digest passes; test without digest fails because of the
    // require_digest on its requirement.
    let pass = evaluate(
        records,
        json!({
            "kind": "evidence",
            "requires": [
                { "kind": "build", "count": 1, "require_digest": true },
                { "kind": "test",  "count": 1, "require_digest": true }
            ]
        }),
    )
    .await;
    assert!(
        !pass,
        "AND of two requires: even if build satisfies, test must also"
    );
}

// ── SPEC §20.4 — diagnostic codes via evaluate_with_diagnostic ──────────────

async fn evaluate_with_diag(records: Vec<Evidence>, guard: Value) -> (bool, Option<String>) {
    let evaluator = build_evaluator(records);
    evaluator
        .evaluate_with_diagnostic(
            &guard,
            &instance_stub(),
            &json!({}),
            &Principal::anonymous(),
        )
        .await
        .expect("evaluate_with_diagnostic")
}

#[tokio::test]
async fn diagnostic_none_when_guard_passes() {
    let records = vec![ev("build", Some("sha256:a"), Some(0.9))];
    let (pass, diag) = evaluate_with_diag(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 1, "require_digest": true,
                           "min_confidence": 0.5 }]
        }),
    )
    .await;
    assert!(pass);
    assert!(diag.is_none());
}

#[tokio::test]
async fn diagnostic_evidence_digest_required_when_only_undigested_match() {
    // Records of the right kind exist but lack digest; would-pass without
    // the require_digest filter, so the diagnostic must attribute the
    // failure to that filter.
    let records = vec![ev("build", None, None), ev("build", None, None)];
    let (pass, diag) = evaluate_with_diag(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 1, "require_digest": true }]
        }),
    )
    .await;
    assert!(!pass);
    assert_eq!(diag.as_deref(), Some("EVIDENCE_DIGEST_REQUIRED"));
}

#[tokio::test]
async fn diagnostic_evidence_confidence_below_threshold() {
    let records = vec![ev("approval", None, Some(0.4))];
    let (pass, diag) = evaluate_with_diag(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "approval", "count": 1, "min_confidence": 0.7 }]
        }),
    )
    .await;
    assert!(!pass);
    assert_eq!(diag.as_deref(), Some("EVIDENCE_CONFIDENCE_BELOW_THRESHOLD"));
}

#[tokio::test]
async fn diagnostic_none_when_no_records_of_kind_exist() {
    // Generic quorum miss (no records of the required kind at all):
    // diagnostic stays None so the caller renders GUARD_REJECTED.
    let records = vec![ev("approval", None, None)];
    let (pass, diag) = evaluate_with_diag(
        records,
        json!({
            "kind": "evidence",
            "requires": [{ "kind": "build", "count": 1 }]
        }),
    )
    .await;
    assert!(!pass);
    assert!(
        diag.is_none(),
        "non-filter-attributable miss must not surface §20.4 code"
    );
}

#[tokio::test]
async fn diagnostic_none_when_no_filter_clauses_present() {
    // Plain legacy `requires: ["approval"]` — no filters, so any failure
    // must stay on the generic path.
    let (pass, diag) = evaluate_with_diag(
        vec![],
        json!({ "kind": "evidence", "requires": ["approval"] }),
    )
    .await;
    assert!(!pass);
    assert!(diag.is_none());
}

#[tokio::test]
async fn diagnostic_digest_required_takes_precedence_when_both_apply() {
    // Two records of the kind: one missing digest, one with low confidence.
    // Neither would satisfy a quorum of 1 under both filters; but absent
    // the digest filter alone there'd still be no qualifying record.
    // Check the attribution chosen: digest filter takes precedence in our
    // implementation because it's evaluated first.
    let records = vec![
        ev("build", None, Some(0.9)), // dropped: no digest
    ];
    let (pass, diag) = evaluate_with_diag(
        records,
        json!({
            "kind": "evidence",
            "requires": [{
                "kind": "build", "count": 1,
                "require_digest": true, "min_confidence": 0.5
            }]
        }),
    )
    .await;
    assert!(!pass);
    assert_eq!(diag.as_deref(), Some("EVIDENCE_DIGEST_REQUIRED"));
}

// ── SPEC §20.1 — Evidence::validate_confidence ──────────────────────────────

#[test]
fn validate_confidence_accepts_in_range() {
    let mut e = ev("k", None, None);
    e.confidence = Some(0.5);
    assert!(e.validate_confidence().is_ok());
}

#[test]
fn validate_confidence_rejects_negative() {
    let mut e = ev("k", None, None);
    e.confidence = Some(-0.001);
    let bad = e.validate_confidence().expect_err("must reject");
    assert!((bad - -0.001).abs() < 1e-6);
}

#[test]
fn validate_confidence_rejects_above_one() {
    let mut e = ev("k", None, None);
    e.confidence = Some(1.001);
    let bad = e.validate_confidence().expect_err("must reject");
    assert!((bad - 1.001).abs() < 1e-6);
}
