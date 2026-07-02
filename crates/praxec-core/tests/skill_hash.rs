//! SPEC §5.7 — content-addressed skill bodies. Normalization, identity,
//! mismatch detection, and surfacing on emitted refs.

use praxec_core::config::{self, compute_skill_hash, normalize_for_hash};
use serde_json::{json, Value};

fn config_with_body(body: &str) -> Value {
    json!({
        "version": "1.0.0",
        // Lexicon entry for 'style.fixture' so the pre-start walk (SPEC §30.10.4)
        // does not block the workflow start in the async emitted_ref test.
        "lexicon": {
            "style.fixture": { "definition_short": "Fixture skill for tests." }
        },
        "skills": {
            "review.style.fixture": {
                "verb": "review",
                "lifecycle": "stable",
                "body": body,
            }
        },
        "workflows": {
            "wf": {
                "initialState": "s",
                "skills": ["review.style.fixture"],
                "states": { "s": { "terminal": true } }
            }
        }
    })
}

fn stamped_hash(cfg: Value) -> String {
    let resolved = config::resolve(cfg).expect("config resolves");
    resolved
        .pointer("/workflows/wf/_skillsLibrary/review.style.fixture/hash")
        .and_then(Value::as_str)
        .expect("hash must be stamped on library entry")
        .to_string()
}

// ── Positive: identical body → identical hash ────────────────────────────────

#[test]
fn identical_body_produces_identical_hash() {
    let h1 = stamped_hash(config_with_body("Lead with the reader's problem."));
    let h2 = stamped_hash(config_with_body("Lead with the reader's problem."));
    assert_eq!(h1, h2);
}

// ── Positive: whitespace-only diff → identical hash ──────────────────────────

#[test]
fn trailing_whitespace_normalized_away() {
    let h1 = stamped_hash(config_with_body("body content"));
    let h2 = stamped_hash(config_with_body("body content   "));
    assert_eq!(h1, h2);
}

#[test]
fn leading_whitespace_normalized_away() {
    let h1 = stamped_hash(config_with_body("body content"));
    let h2 = stamped_hash(config_with_body("   body content"));
    assert_eq!(h1, h2);
}

#[test]
fn internal_whitespace_runs_collapsed() {
    let h1 = stamped_hash(config_with_body("a b c"));
    let h2 = stamped_hash(config_with_body("a    b\t\tc"));
    let h3 = stamped_hash(config_with_body("a\nb  c"));
    assert_eq!(h1, h2);
    assert_eq!(h1, h3);
}

#[test]
fn trailing_newline_stripped() {
    let h1 = stamped_hash(config_with_body("body"));
    let h2 = stamped_hash(config_with_body("body\n"));
    assert_eq!(h1, h2);
}

// ── Positive: semantic edit → different hash ────────────────────────────────

#[test]
fn semantic_edit_produces_different_hash() {
    let h1 = stamped_hash(config_with_body("original text"));
    let h2 = stamped_hash(config_with_body("original tixt"));
    assert_ne!(h1, h2);
}

// ── Positive: hash carries `sha256:` prefix + hex digest ────────────────────

#[test]
fn hash_has_sha256_prefix_and_hex_digest() {
    let h = stamped_hash(config_with_body("anything"));
    assert!(
        h.starts_with("sha256:"),
        "expected sha256: prefix; got: {h}"
    );
    let hex = &h["sha256:".len()..];
    assert_eq!(
        hex.len(),
        64,
        "expected 64 hex chars; got {} in: {h}",
        hex.len()
    );
    assert!(
        hex.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
        "expected lowercase-hex digest; got: {hex}"
    );
}

// ── Positive: emitted ref carries hash field ────────────────────────────────

struct NoopRegistry;
impl praxec_core::ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<std::sync::Arc<dyn praxec_core::Executor>> {
        None
    }
}

#[tokio::test]
async fn emitted_ref_carries_hash_field() {
    use praxec_core::audit::{AuditSink, MemoryAuditSink};
    use praxec_core::guards::DefaultGuardEvaluator;
    use praxec_core::model::{Principal, StartWorkflow};
    use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use praxec_core::WorkflowRuntime;
    use std::sync::Arc;

    let resolved = config::resolve(config_with_body("ref-carry test")).expect("config resolves");
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(NoopRegistry);
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit as Arc<dyn AuditSink>,
    );

    let resp = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .expect("start succeeds");

    let refs = resp["guidance"]["refs"]
        .as_array()
        .expect("guidance.refs must be present");
    let fixture_ref = refs
        .iter()
        .find(|r| r["subject"].as_str() == Some("review.style.fixture"))
        .expect("fixture ref must be surfaced");
    let hash = fixture_ref["hash"]
        .as_str()
        .expect("emitted ref MUST carry hash (SPEC §5.4)");
    assert!(hash.starts_with("sha256:"));
}

// ── Negative: stored-hash mismatch fails fast ───────────────────────────────

#[test]
fn stored_hash_mismatch_fails_at_load() {
    let cfg = json!({
        "version": "1.0.0",
        "skills": {
            "review.style.fixture": {
                "verb": "review",
                "lifecycle": "stable",
                "body": "actual body",
                "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
            }
        }
    });
    let err = config::resolve(cfg).expect_err("mismatched stored hash must fail load");
    let msg = format!("{err}");
    assert!(
        msg.contains("HASH_MISMATCH"),
        "error must use HASH_MISMATCH code; got: {msg}"
    );
}

// ── Edge: empty body → deterministic empty hash, not error ──────────────────

#[test]
fn empty_body_hashes_deterministically() {
    let h = compute_skill_hash("");
    assert_eq!(
        h,
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

#[test]
fn whitespace_only_body_hashes_same_as_empty() {
    assert_eq!(compute_skill_hash("   \n\t  "), compute_skill_hash(""));
}

// ── Property: normalize_for_hash is idempotent ──────────────────────────────

#[test]
fn normalize_for_hash_is_idempotent() {
    let fixtures = [
        "",
        "x",
        "a b c",
        "  trim me  ",
        "many    spaces",
        "line\nbreak",
        "trailing\nnewline\n",
        "mixed \t whitespace \n run",
    ];
    for s in fixtures {
        let once = normalize_for_hash(s);
        let twice = normalize_for_hash(&once);
        assert_eq!(once, twice, "normalize is not idempotent for: {s:?}");
    }
}

// ── Cross-impl invariant: read-side and stamp-side agree ────────────────────

#[test]
fn config_hash_equals_direct_compute() {
    let body = "single source of truth check";
    let stamped = stamped_hash(config_with_body(body));
    let direct = compute_skill_hash(body);
    assert_eq!(
        stamped, direct,
        "stamped hash must equal compute_skill_hash(body) — shared normalize function (SPEC §5.7)"
    );
}
