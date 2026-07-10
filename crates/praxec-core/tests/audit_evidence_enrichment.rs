//! SPEC §20.1 + §20.2 — additive Evidence (digest/confidence) and
//! AuditEvent (trace_id/run_id) enrichments. Atomic assertions covering:
//!   - default-None construction
//!   - builder methods set the fields
//!   - serde omits absent fields (backward-compat wire format)
//!   - serde round-trip preserves present fields
//!   - confidence range validation rejects out-of-range
//!   - existing producers (without the new fields) still deserialize cleanly

use praxec_core::audit::AuditEvent;
use praxec_core::model::Evidence;
use serde_json::{Value, json};

// ── Evidence (§20.1) ────────────────────────────────────────────────────────

fn fresh_evidence() -> Evidence {
    Evidence {
        kind: "build_log".into(),
        id: "ev_1".into(),
        uri: None,
        summary: None,
        digest: None,
        confidence: None,
    }
}

#[test]
fn evidence_digest_defaults_to_none() {
    assert!(fresh_evidence().digest.is_none());
}

#[test]
fn evidence_confidence_defaults_to_none() {
    assert!(fresh_evidence().confidence.is_none());
}

#[test]
fn evidence_with_digest_round_trips_through_serde() {
    let mut e = fresh_evidence();
    e.digest = Some("sha256:abc123".into());
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert_eq!(serialized["digest"].as_str(), Some("sha256:abc123"));
    let deserialized: Evidence = serde_json::from_value(serialized).expect("deserialize");
    assert_eq!(deserialized.digest.as_deref(), Some("sha256:abc123"));
}

#[test]
fn evidence_with_confidence_round_trips_through_serde() {
    let mut e = fresh_evidence();
    e.confidence = Some(0.85);
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert!((serialized["confidence"].as_f64().unwrap() - 0.85).abs() < 1e-6);
    let deserialized: Evidence = serde_json::from_value(serialized).expect("deserialize");
    assert!((deserialized.confidence.unwrap() - 0.85).abs() < 1e-6);
}

#[test]
fn evidence_absent_digest_is_omitted_from_wire_format() {
    let e = fresh_evidence();
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert!(
        serialized.get("digest").is_none(),
        "absent digest must be omitted (backward-compat); got: {serialized}"
    );
}

#[test]
fn evidence_absent_confidence_is_omitted_from_wire_format() {
    let e = fresh_evidence();
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert!(serialized.get("confidence").is_none());
}

#[test]
fn evidence_legacy_payload_without_new_fields_deserializes() {
    // Simulate a payload produced before §20.1 landed.
    let legacy: Value = json!({
        "kind": "old_log",
        "id":   "ev_old",
        "uri":  null,
        "summary": null
    });
    let e: Evidence = serde_json::from_value(legacy).expect("legacy payload must round-trip");
    assert_eq!(e.kind, "old_log");
    assert!(e.digest.is_none());
    assert!(e.confidence.is_none());
}

#[test]
fn evidence_validate_confidence_accepts_zero() {
    let mut e = fresh_evidence();
    e.confidence = Some(0.0);
    assert!(e.validate_confidence().is_ok());
}

#[test]
fn evidence_validate_confidence_accepts_one() {
    let mut e = fresh_evidence();
    e.confidence = Some(1.0);
    assert!(e.validate_confidence().is_ok());
}

#[test]
fn evidence_validate_confidence_accepts_none() {
    assert!(fresh_evidence().validate_confidence().is_ok());
}

#[test]
fn evidence_validate_confidence_rejects_negative() {
    let mut e = fresh_evidence();
    e.confidence = Some(-0.1);
    let err = e.validate_confidence().expect_err("negative must reject");
    assert!((err - -0.1).abs() < 1e-6);
}

#[test]
fn evidence_validate_confidence_rejects_above_one() {
    let mut e = fresh_evidence();
    e.confidence = Some(1.1);
    let err = e.validate_confidence().expect_err("> 1.0 must reject");
    assert!((err - 1.1).abs() < 1e-6);
}

// ── AuditEvent (§20.2) ──────────────────────────────────────────────────────

#[test]
fn audit_event_trace_id_defaults_to_none() {
    let e = AuditEvent::new("workflow.test");
    assert!(e.trace_id.is_none());
}

#[test]
fn audit_event_run_id_defaults_to_none() {
    let e = AuditEvent::new("workflow.test");
    assert!(e.run_id.is_none());
}

#[test]
fn audit_event_with_trace_id_sets_the_field() {
    let e = AuditEvent::new("workflow.test").with_trace_id("trace_abc");
    assert_eq!(e.trace_id.as_deref(), Some("trace_abc"));
}

#[test]
fn audit_event_with_run_id_sets_the_field() {
    let e = AuditEvent::new("workflow.test").with_run_id("run_xyz");
    assert_eq!(e.run_id.as_deref(), Some("run_xyz"));
}

#[test]
fn audit_event_with_trace_and_run_chain() {
    let e = AuditEvent::new("workflow.test")
        .with_trace_id("trace_abc")
        .with_run_id("run_xyz");
    assert_eq!(e.trace_id.as_deref(), Some("trace_abc"));
    assert_eq!(e.run_id.as_deref(), Some("run_xyz"));
}

#[test]
fn audit_event_absent_trace_id_omitted_from_wire_format() {
    let e = AuditEvent::new("workflow.test");
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert!(
        serialized.get("trace_id").is_none(),
        "absent trace_id must be omitted (backward-compat); got: {serialized}"
    );
}

#[test]
fn audit_event_absent_run_id_omitted_from_wire_format() {
    let e = AuditEvent::new("workflow.test");
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert!(serialized.get("run_id").is_none());
}

#[test]
fn audit_event_present_trace_id_surfaces_in_wire_format() {
    let e = AuditEvent::new("workflow.test").with_trace_id("trace_42");
    let serialized = serde_json::to_value(&e).expect("serialize");
    assert_eq!(serialized["trace_id"].as_str(), Some("trace_42"));
}

#[test]
fn audit_event_round_trips_with_trace_and_run() {
    let original = AuditEvent::new("workflow.test")
        .with_trace_id("t_1")
        .with_run_id("r_1");
    let serialized = serde_json::to_value(&original).expect("serialize");
    let deserialized: AuditEvent = serde_json::from_value(serialized).expect("deserialize");
    assert_eq!(deserialized.trace_id.as_deref(), Some("t_1"));
    assert_eq!(deserialized.run_id.as_deref(), Some("r_1"));
}

#[test]
fn audit_event_legacy_payload_without_new_fields_deserializes() {
    // Simulate a payload from before §20.2.
    let legacy = json!({
        "id":             "evt_old",
        "timestamp":      "2026-05-24T14:03:11Z",
        "correlation_id": "cor_old",
        "event_type":     "workflow.test",
        "payload":        {}
    });
    let e: AuditEvent = serde_json::from_value(legacy).expect("legacy must deserialize");
    assert!(e.trace_id.is_none());
    assert!(e.run_id.is_none());
}

#[test]
fn audit_event_with_workflow_and_trace_independent_fields() {
    let e = AuditEvent::new("workflow.test")
        .with_workflow("wf_1")
        .with_trace_id("t_1");
    // workflow_id and trace_id must not collide.
    assert_eq!(e.workflow_id.as_deref(), Some("wf_1"));
    assert_eq!(e.trace_id.as_deref(), Some("t_1"));
}
