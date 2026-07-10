//! Tranche 3 — `px doctor` pre-flight checks.
//!
//! Verifies the doctor subcommand:
//! - All-green run against valid config + agents
//! - CONFIG_NOT_FOUND when path doesn't exist
//! - CONFIG_INVALID when YAML doesn't parse
//! - WORKFLOW_NOT_DECLARED when --workflow doesn't match
//! - MISSING_API_KEY when provider env var is absent
//! - lexicon coverage: LEXICON_PENDING_DEFINITIONS when unresolved subjects exist

use praxec_tui::doctor::{CheckStatus, DoctorArgs, count_failures, run_doctor};

fn find_status<'a>(
    results: &'a [praxec_tui::doctor::CheckResult],
    code: &str,
) -> Option<&'a praxec_tui::doctor::CheckResult> {
    results
        .iter()
        .find(|r| matches!(&r.status, CheckStatus::Fail(c) if c == code))
}

/// Absolute path to `examples/smoke-ete/gateway.yaml` in this repo.
/// CARGO_MANIFEST_DIR = <repo>/crates/praxec-tui.
fn smoke_ete_config() -> String {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // repo root
    p.push("examples/smoke-ete/gateway.yaml");
    p.to_string_lossy().into_owned()
}

#[tokio::test]
async fn doctor_passes_against_smoke_ete_with_anthropic_key_set() {
    // Temporarily set ANTHROPIC_API_KEY for this test if not already
    // present so the agent check passes. We use a placeholder value —
    // doctor only checks presence, not validity.
    let prior = std::env::var("ANTHROPIC_API_KEY").ok();
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-placeholder") };

    let args = DoctorArgs {
        config: Some(smoke_ete_config()),
        workflow: Some("smoke_ete".to_string()),
        agents: vec!["test=anthropic/claude-haiku-4-5-20251001".to_string()],
        refresh_agents: false,
    };
    let results = run_doctor(&args).await;
    let failures = count_failures(&results);

    // Restore env state.
    match prior {
        // FIXME: Audit that the environment access only happens in single-threaded code.
        Some(v) => unsafe { std::env::set_var("ANTHROPIC_API_KEY", v) },
        // FIXME: Audit that the environment access only happens in single-threaded code.
        None => unsafe { std::env::remove_var("ANTHROPIC_API_KEY") },
    }

    assert_eq!(
        failures, 0,
        "doctor must pass against smoke-ete config + test agent: {:#?}",
        results
    );
}

#[tokio::test]
async fn doctor_reports_config_not_found_for_missing_path() {
    let args = DoctorArgs {
        config: Some("/nonexistent/path/praxec.yaml".to_string()),
        workflow: None,
        agents: vec![],
        refresh_agents: false,
    };
    let results = run_doctor(&args).await;
    assert!(
        find_status(&results, "CONFIG_NOT_FOUND").is_some(),
        "expected CONFIG_NOT_FOUND; got: {:#?}",
        results
    );
}

#[tokio::test]
async fn doctor_reports_workflow_not_declared() {
    let args = DoctorArgs {
        config: Some(smoke_ete_config()),
        workflow: Some("nonexistent_workflow".to_string()),
        agents: vec![],
        refresh_agents: false,
    };
    let results = run_doctor(&args).await;
    let fail =
        find_status(&results, "WORKFLOW_NOT_DECLARED").expect("WORKFLOW_NOT_DECLARED must surface");
    assert!(
        fail.detail.contains("smoke_ete"),
        "failure detail must list available workflows; got: {}",
        fail.detail
    );
}

#[tokio::test]
async fn doctor_reports_missing_api_key_when_env_var_absent() {
    // Remove the env var (if present) for the duration of this test.
    let prior = std::env::var("OPENAI_API_KEY").ok();
    // FIXME: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("OPENAI_API_KEY") };

    let args = DoctorArgs {
        config: None,
        workflow: None,
        agents: vec!["planner=openai/gpt-4o".to_string()],
        refresh_agents: false,
    };
    let results = run_doctor(&args).await;
    let fail = find_status(&results, "MISSING_API_KEY");

    if let Some(v) = prior {
        // FIXME: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var("OPENAI_API_KEY", v) };
    }

    let fail = fail.expect("MISSING_API_KEY must surface when env var absent");
    assert!(
        fail.detail.contains("OPENAI_API_KEY"),
        "failure must name the missing env var; got: {}",
        fail.detail
    );
}

#[tokio::test]
async fn doctor_skips_workflow_check_when_no_config_argument() {
    let args = DoctorArgs::default();
    let results = run_doctor(&args).await;
    // No config + no workflow → all checks except praxec binary are skipped.
    // The binary check may pass or fail depending on the build env.
    // Either way, we should NOT see CONFIG_INVALID / WORKFLOW_NOT_DECLARED
    // (those require a config arg to even attempt).
    assert!(
        find_status(&results, "CONFIG_INVALID").is_none(),
        "should not attempt config-invalid check without a config arg"
    );
    assert!(
        find_status(&results, "WORKFLOW_NOT_DECLARED").is_none(),
        "should not attempt workflow-declared check without a config arg"
    );
}

// ── lexicon coverage checks ───────────────────────────────────────────────

/// Write a YAML config string to a temp file and return (TempDir, path string).
/// TempDir must be held for the duration of the test or the file is deleted.
fn write_temp_config(yaml: &str) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("gateway.yaml");
    std::fs::write(&path, yaml).unwrap();
    let path_str = path.to_string_lossy().into_owned();
    (dir, path_str)
}

#[tokio::test]
async fn doctor_warns_on_lexicon_pending_definitions() {
    // Config has a workflow executor referencing subject "evidence-foo", which
    // is neither defined nor lexicon-authored (genuinely-unknown vocabulary).
    // Doctor should surface a LEXICON_PENDING_DEFINITIONS warning.
    let yaml = r#"
version: "1.0.0"
praxec:
  strict_namespacing: false
lexicon: {}
workflows:
  demo:
    initialState: idle
    states:
      idle:
        transitions:
          go:
            target: done
            executor:
              kind: script
              subject: build.evidence-foo
      done:
        terminal: true
"#;
    let (_dir, path) = write_temp_config(yaml);
    let args = DoctorArgs {
        config: Some(path),
        workflow: None,
        agents: vec![],
        refresh_agents: false,
    };
    let results = run_doctor(&args).await;
    let check = results
        .iter()
        .find(|r| r.name == "lexicon coverage")
        .expect("lexicon coverage check must appear");
    // The check warns (is a Warn with LEXICON_PENDING_DEFINITIONS) because
    // "evidence-foo" has no lexicon entry. It must NOT be a Fail (that would
    // break CI for configs with unresolved subjects).
    assert!(
        matches!(&check.status, CheckStatus::Warn(code) if code == "LEXICON_PENDING_DEFINITIONS"),
        "expected Warn(LEXICON_PENDING_DEFINITIONS); got: {:?}",
        check.status
    );
    assert!(
        check.detail.contains("evidence-foo"),
        "detail must name the pending subject; got: {}",
        check.detail
    );
}

#[tokio::test]
async fn doctor_passes_lexicon_coverage_when_all_subjects_registered() {
    // Config has a script whose subject "evidence-foo" IS in the lexicon.
    // Doctor should pass the lexicon coverage check.
    let yaml = r#"
version: "1.0.0"
praxec:
  strict_namespacing: false
lexicon:
  evidence-foo:
    definition_short: "A real lexicon entry."
scripts:
  build.evidence-foo:
    verb: build
    lifecycle: experimental
    body: |
      #!/usr/bin/env bash
      echo hi
workflows:
  demo:
    initialState: idle
    states:
      idle:
        terminal: true
"#;
    let (_dir, path) = write_temp_config(yaml);
    let args = DoctorArgs {
        config: Some(path),
        workflow: None,
        agents: vec![],
        refresh_agents: false,
    };
    let results = run_doctor(&args).await;
    let check = results
        .iter()
        .find(|r| r.name == "lexicon coverage")
        .expect("lexicon coverage check must appear");
    assert!(
        matches!(check.status, CheckStatus::Pass),
        "lexicon coverage must pass when all subjects are registered; got: {:?}",
        check.status
    );
}
