//! SPEC §19 — ingest executor tests. Frontmatter parsing, verb mapping,
//! subject inference, error codes.

use chrono::Utc;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::IngestExecutor;
use serde_json::{json, Value};

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_stub".into(),
        definition_id: "stub".into(),
        definition_version: "0".into(),
        definition: Value::Null,
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

async fn ingest(args: Value) -> Result<Value, praxec_core::error::ExecutorError> {
    IngestExecutor
        .execute(ExecuteRequest {
            workflow: instance_stub(),
            transition: None,
            arguments: args,
            executor_config: Value::Null,
            idempotency_key: None,
            correlation_id: None,
        })
        .await
        .map(|r| r.output)
}

// ── Positive: frontmatter + body produces valid fragment ────────────────────

#[tokio::test]
async fn frontmatter_with_explicit_verb_produces_fragment() {
    let body = "---\nname: house-voice\nverb: review\n---\nLead with the reader's problem.";
    let out = ingest(json!({
        "source_path": "ignored",
        "source_body": body,
        "subject":     "review.style.house-voice",
    }))
    .await
    .expect("ingest succeeds");
    let f = &out["fragment"];
    assert_eq!(f["subject"].as_str(), Some("review.style.house-voice"));
    assert_eq!(f["verb"].as_str(), Some("review"));
    assert_eq!(f["lifecycle"].as_str(), Some("experimental"));
    assert!(f["body"]
        .as_str()
        .unwrap_or_default()
        .contains("reader's problem"));
    assert!(f["hash"]
        .as_str()
        .unwrap_or_default()
        .starts_with("sha256:"));
}

// ── Positive: verb synonym mapping with VERB_MAPPED diagnostic ──────────────

#[tokio::test]
async fn synonym_verb_mapped_with_diagnostic() {
    let body = "---\nname: fix\n---\nfix the thing";
    let out = ingest(json!({
        "source_path": "ignored",
        "source_body": body,
        "subject":     "import.x.fixer",
    }))
    .await
    .expect("ingest succeeds");
    assert_eq!(out["fragment"]["verb"].as_str(), Some("implement"));
    let diags = out["diagnostics"].as_array().expect("diagnostics array");
    let mapped: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"].as_str() == Some("VERB_MAPPED"))
        .collect();
    assert_eq!(mapped.len(), 1);
    assert_eq!(mapped[0]["from"].as_str(), Some("fix"));
    assert_eq!(mapped[0]["to"].as_str(), Some("implement"));
}

#[tokio::test]
async fn closed_verb_passes_through_without_diagnostic() {
    let body = "---\nverb: triage\n---\nbody";
    let out = ingest(json!({
        "source_path": "ignored",
        "source_body": body,
        "subject":     "triage.bug.report",
    }))
    .await
    .expect("ingest succeeds");
    assert_eq!(out["fragment"]["verb"].as_str(), Some("triage"));
    let diags = out["diagnostics"].as_array().expect("diagnostics array");
    let mapped: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"].as_str() == Some("VERB_MAPPED"))
        .collect();
    assert!(
        mapped.is_empty(),
        "closed-set verb must not emit VERB_MAPPED"
    );
}

// ── Subject inference from path ────────────────────────────────────────────

#[tokio::test]
async fn subject_inferred_from_path() {
    let body = "---\nverb: diagnose\n---\nbody";
    let out = ingest(json!({
        "source_path": ".claude/skills/engineering/diagnose/SKILL.md",
        "source_body": body,
    }))
    .await
    .expect("ingest succeeds");
    // The leaf is `diagnose` (SKILL.md filename stripped); mid is `engineering`.
    assert_eq!(
        out["fragment"]["subject"].as_str(),
        Some("import.engineering.diagnose")
    );
}

#[tokio::test]
async fn explicit_subject_arg_wins_over_path() {
    let body = "---\nverb: diagnose\n---\nbody";
    let out = ingest(json!({
        "source_path": ".claude/skills/engineering/diagnose/SKILL.md",
        "source_body": body,
        "subject":     "review.x.y",
    }))
    .await
    .expect("ingest succeeds");
    assert_eq!(out["fragment"]["subject"].as_str(), Some("review.x.y"));
}

// ── Hash + source fields ────────────────────────────────────────────────────

#[tokio::test]
async fn fragment_carries_source_path() {
    let body = "---\nverb: review\n---\nbody";
    let out = ingest(json!({
        "source_path": "path/to/skill.md",
        "source_body": body,
        "subject":     "review.x.y",
    }))
    .await
    .expect("ingest succeeds");
    assert_eq!(out["fragment"]["source"].as_str(), Some("path/to/skill.md"));
}

#[tokio::test]
async fn fragment_hash_matches_normalize_for_hash_of_body() {
    use praxec_core::config::compute_skill_hash;
    let body_text = "the actual body content";
    let raw = format!("---\nverb: review\n---\n{body_text}");
    let out = ingest(json!({
        "source_path": "x.md",
        "source_body": raw,
        "subject":     "review.x.y",
    }))
    .await
    .expect("ingest succeeds");
    let expected = compute_skill_hash(body_text);
    assert_eq!(out["fragment"]["hash"].as_str(), Some(expected.as_str()));
}

// ── Negative: error paths ───────────────────────────────────────────────────

#[tokio::test]
async fn missing_source_path_errors() {
    let err = ingest(json!({}))
        .await
        .expect_err("missing source_path must error");
    assert!(format!("{err:?}").contains("source_path"));
}

#[tokio::test]
async fn unknown_verb_errors_with_invalid_verb_code() {
    let body = "---\nverb: bogus-action\n---\nbody";
    let err = ingest(json!({
        "source_path": "x.md",
        "source_body": body,
        "subject":     "review.x.y",
    }))
    .await
    .expect_err("unknown verb must error");
    assert!(format!("{err:?}").contains("INGEST_INVALID_VERB"));
}

#[tokio::test]
async fn empty_body_errors_with_empty_body_code() {
    let body = "---\nverb: review\n---\n   \n";
    let err = ingest(json!({
        "source_path": "x.md",
        "source_body": body,
        "subject":     "review.x.y",
    }))
    .await
    .expect_err("empty body must error");
    assert!(format!("{err:?}").contains("INGEST_EMPTY_BODY"));
}

#[tokio::test]
async fn no_subject_no_path_no_frontmatter_subject_errors() {
    // Path that yields nothing usable. Use a path with no segments.
    let body = "---\nverb: review\n---\nbody";
    let err = ingest(json!({
        "source_path": "",
        "source_body": body,
    }))
    .await
    .expect_err("must error when no subject can be inferred");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("INGEST_CANNOT_INFER_SUBJECT") || msg.contains("source_path"),
        "expected INGEST_CANNOT_INFER_SUBJECT or empty-path error; got: {msg}"
    );
}

// ── Negative: malformed frontmatter YAML fails loud (CMP-048) ───────────────
// A `---` fence with un-parseable YAML inside must surface
// INGEST_INVALID_FRONTMATTER rather than silently swallowing the frontmatter
// to an empty map (which would then lose the declared subject/verb).

#[tokio::test]
async fn malformed_frontmatter_yaml_errors() {
    // `name: [unclosed` is invalid YAML (unterminated flow sequence). The
    // closing `\n---` fence is present so this IS treated as frontmatter.
    let body = "---\nname: [unclosed\nverb: review\n---\nbody content here";
    let err = ingest(json!({
        "source_path": "x.md",
        "source_body": body,
        "subject":     "review.x.y",
    }))
    .await
    .expect_err("malformed frontmatter YAML must error");
    assert!(
        format!("{err:?}").contains("INGEST_INVALID_FRONTMATTER"),
        "got: {err:?}"
    );
}

// ── Caller-supplied synonym override ────────────────────────────────────────

#[tokio::test]
async fn caller_synonyms_extend_defaults() {
    let body = "---\nverb: bizarre\n---\nbody";
    let out = ingest(json!({
        "source_path":   "x.md",
        "source_body":   body,
        "subject":       "review.x.y",
        "verb_synonyms": { "bizarre": "review" },
    }))
    .await
    .expect("caller-supplied synonym must work");
    assert_eq!(out["fragment"]["verb"].as_str(), Some("review"));
}
