//! SPEC §22 — `script` executor kind. FMECA-style atomic assertions for
//! subject resolution, hash audit-trail surfacing, args templating, exit
//! handling, and the SCRIPT_NOT_IN_SNAPSHOT poka-yoke.
//!
//! No mocks — the tests exec real `bash` (skip on Windows). The runtime
//! tests use real instance shapes per the no-shortcuts lint convention.

#![cfg(unix)]

use chrono::Utc;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::ScriptExecutor;
use serde_json::{Value, json};

fn instance_with_scripts_library(lib: Value) -> WorkflowInstance {
    let definition = json!({
        "_scriptsLibrary": lib,
    });
    WorkflowInstance {
        id: "wf_x".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition,
        state: "running".into(),
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

fn req(workflow: WorkflowInstance, executor_config: Value, arguments: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow,
        transition: Some("go".into()),
        arguments,
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

// ── Subject lookup → exec → stdout captured ──────────────────────────────

#[tokio::test]
async fn inline_body_executes_and_captures_stdout() {
    let instance = instance_with_scripts_library(json!({
        "run.echo.hello": {
            "verb": "run",
            "lifecycle": "stable",
            "body": "echo hello from script",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "run.echo.hello" }),
            json!({}),
        ))
        .await
        .expect("script runs");
    assert_eq!(result.output["success"], true);
    assert_eq!(result.output["exitCode"], 0);
    let stdout = result.output["stdout"].as_str().unwrap();
    assert!(stdout.contains("hello from script"), "stdout: {stdout}");
}

// ── Audit trail: scriptSubject + scriptHash on output ────────────────────

#[tokio::test]
async fn output_carries_script_subject_and_hash_for_audit_trail() {
    let hash = "sha256:abc1234567890123456789012345678901234567890123456789012345678901";
    let instance = instance_with_scripts_library(json!({
        "build.test.echo": {
            "verb": "build",
            "lifecycle": "stable",
            "body": "echo ok",
            "hash": hash,
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "build.test.echo" }),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(result.output["scriptSubject"], "build.test.echo");
    assert_eq!(result.output["scriptHash"], hash);

    // Evidence also carries the hash via the digest field.
    assert_eq!(result.evidence.len(), 1);
    assert_eq!(result.evidence[0].kind, "script_output");
    assert_eq!(result.evidence[0].digest.as_deref(), Some(hash));
}

// ── Missing subject → SCRIPT_NOT_IN_SNAPSHOT (poka-yoke) ─────────────────

#[tokio::test]
async fn unstamped_subject_rejects_with_script_not_in_snapshot() {
    let instance = instance_with_scripts_library(json!({})); // empty library
    let exec = ScriptExecutor::new();
    let err = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "build.ghost.notreal" }),
            json!({}),
        ))
        .await
        .expect_err("missing subject must error");
    let s = format!("{err:?}");
    assert!(s.contains("SCRIPT_NOT_IN_SNAPSHOT"), "got: {s}");
    assert!(s.contains("build.ghost.notreal"), "got: {s}");
}

// ── Snapshot entry missing `hash` → SCRIPT_NOT_IN_SNAPSHOT (CMP-017) ─────
// A library entry with a `body` but no `hash` is a malformed snapshot. The
// executor must fail-fast (like a missing `body`) rather than substitute an
// all-zeros poison sentinel that would silently pollute the audit trail.

#[tokio::test]
async fn missing_hash_field_rejects_with_script_not_in_snapshot() {
    let instance = instance_with_scripts_library(json!({
        "run.nohash.here": {
            "verb": "run",
            "lifecycle": "stable",
            "body": "echo hi",
            // no "hash" key
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let err = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "run.nohash.here" }),
            json!({}),
        ))
        .await
        .expect_err("missing hash must error, not substitute a sentinel");
    let s = format!("{err:?}");
    assert!(s.contains("SCRIPT_NOT_IN_SNAPSHOT"), "got: {s}");
    assert!(s.contains("hash"), "got: {s}");
}

// ── Non-string `hash` → SCRIPT_NOT_IN_SNAPSHOT (CMP-017) ─────────────────

#[tokio::test]
async fn non_string_hash_field_rejects_with_script_not_in_snapshot() {
    let instance = instance_with_scripts_library(json!({
        "run.badhash.here": {
            "verb": "run",
            "lifecycle": "stable",
            "body": "echo hi",
            "hash": 12345, // not a string
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let err = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "run.badhash.here" }),
            json!({}),
        ))
        .await
        .expect_err("non-string hash must error");
    assert!(
        format!("{err:?}").contains("SCRIPT_NOT_IN_SNAPSHOT"),
        "got: {err:?}"
    );
}

// ── Missing subject FIELD → INVALID_SCRIPT_INVOCATION ────────────────────

#[tokio::test]
async fn missing_subject_field_rejects_with_invalid_invocation() {
    let instance = instance_with_scripts_library(json!({}));
    let exec = ScriptExecutor::new();
    let err = exec
        .execute(req(
            instance,
            json!({ "kind": "script" }), // no subject
            json!({}),
        ))
        .await
        .expect_err("missing subject must error");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_SCRIPT_INVOCATION"), "got: {s}");
}

// ── Non-zero exit → ExecutorError::Permanent (treatNonZeroAsFailure default) ─

#[tokio::test]
async fn nonzero_exit_defaults_to_failure() {
    let instance = instance_with_scripts_library(json!({
        "test.fails.always": {
            "verb": "test",
            "lifecycle": "stable",
            "body": "exit 1",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let err = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "test.fails.always" }),
            json!({}),
        ))
        .await
        .expect_err("non-zero exit must fail by default");
    assert!(format!("{err:?}").contains("exited with code"));
}

// ── treatNonZeroAsFailure: false → returns Ok with success=false ─────────

#[tokio::test]
async fn nonzero_exit_with_treat_false_returns_ok() {
    let instance = instance_with_scripts_library(json!({
        "test.exit.42": {
            "verb": "test",
            "lifecycle": "stable",
            "body": "exit 42",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({
                "kind": "script",
                "subject": "test.exit.42",
                "treatNonZeroAsFailure": false
            }),
            json!({}),
        ))
        .await
        .expect("treatNonZeroAsFailure: false should not error");
    assert_eq!(result.output["success"], false);
    assert_eq!(result.output["exitCode"], 42);
}

// ── Args rendering against scopes ────────────────────────────────────────

#[tokio::test]
async fn args_template_from_context_scope() {
    let mut instance = instance_with_scripts_library(json!({
        "run.echo.arg": {
            "verb": "run",
            "lifecycle": "stable",
            "body": "echo got=\"$1\"",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    }));
    instance.context = json!({ "target": "production" });
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({
                "kind": "script",
                "subject": "run.echo.arg",
                "args": ["$.context.target"]
            }),
            json!({}),
        ))
        .await
        .unwrap();
    let stdout = result.output["stdout"].as_str().unwrap();
    assert!(
        stdout.contains("got=production"),
        "expected template rendering; got: {stdout}"
    );
}

// ── Shebang honored: a Python script body runs via python ────────────────

#[tokio::test]
async fn shebang_honored_for_python_body() {
    // Only run if python3 is available.
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("python3 not available; skipping shebang test");
        return;
    }
    let instance = instance_with_scripts_library(json!({
        "run.python.print": {
            "verb": "run",
            "lifecycle": "stable",
            "body": "#!/usr/bin/env python3\nprint('hello from python')\n",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "run.python.print" }),
            json!({}),
        ))
        .await
        .expect("python script runs");
    let stdout = result.output["stdout"].as_str().unwrap();
    assert!(stdout.contains("hello from python"), "stdout: {stdout}");
}

// ── PRAXEC_SCRIPT_SUBJECT + PRAXEC_SCRIPT_HASH exposed to script ────

#[tokio::test]
async fn praxec_env_vars_exposed_to_script_body() {
    let hash = "sha256:deadbeef00000000000000000000000000000000000000000000000000000000";
    let instance = instance_with_scripts_library(json!({
        "verify.env.exposed": {
            "verb": "verify",
            "lifecycle": "stable",
            "body": "echo \"sub=$PRAXEC_SCRIPT_SUBJECT hash=$PRAXEC_SCRIPT_HASH\"",
            "hash": hash,
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "verify.env.exposed" }),
            json!({}),
        ))
        .await
        .unwrap();
    let stdout = result.output["stdout"].as_str().unwrap();
    assert!(
        stdout.contains("sub=verify.env.exposed"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains(&format!("hash={hash}")), "stdout: {stdout}");
}

// ── Stdout JSON auto-parsed when valid ────────────────────────────────────

#[tokio::test]
async fn stdout_json_auto_parses_into_output_json_field() {
    let instance = instance_with_scripts_library(json!({
        "verify.emits.json": {
            "verb": "verify",
            "lifecycle": "stable",
            "body": "echo '{\"passed\":true,\"count\":42}'",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    }));
    let exec = ScriptExecutor::new();
    let result = exec
        .execute(req(
            instance,
            json!({ "kind": "script", "subject": "verify.emits.json" }),
            json!({}),
        ))
        .await
        .unwrap();
    assert_eq!(result.output["json"]["passed"], true);
    assert_eq!(result.output["json"]["count"], 42);
}
