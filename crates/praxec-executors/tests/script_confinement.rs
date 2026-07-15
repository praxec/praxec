//! ADR-0006 slice 1 — per-script confinement decision matrix for the `script`
//! executor. Atomic assertions for the FMECA constraint: the DEFAULT is
//! unconfined (existing scripts unchanged), confined scripts route through the
//! sandbox provider, and every other shape FAILS FAST naming the script — never
//! a silent deny or a silent unconfined run.
//!
//! Provider-routing is asserted with an injected recording provider, so these
//! are environment-independent (no real sandbox needed here; bubblewrap
//! confinement is validated by the spike + the bwrap-gated test).

#![cfg(unix)]

use std::sync::Mutex;

use async_trait::async_trait;
use chrono::Utc;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_core::sandbox::{Egress, Preflight, SandboxOutput, SandboxProvider, SandboxSpec};
use praxec_executors::ScriptExecutor;
use serde_json::{Value, json};
use std::sync::Arc;

fn instance(lib: Value) -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_x".into(),
        definition_id: "demo".into(),
        definition_version: "0".into(),
        definition: json!({ "_scriptsLibrary": lib }),
        state: "running".into(),
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

fn lib() -> Value {
    json!({
        "run.echo.hello": {
            "verb": "run", "lifecycle": "stable",
            "body": "echo hello from script",
            "hash": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "source": "config"
        }
    })
}

fn req(executor_config: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance(lib()),
        transition: Some("go".into()),
        arguments: json!({}),
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

/// Records the spec it was handed and returns a canned success — lets us assert
/// routing + spec shape without a real sandbox.
/// (args, env pairs, egress) captured from the spec the provider was handed.
type RecordedSpec = (Vec<String>, Vec<(String, String)>, Egress);

#[derive(Default)]
struct RecordingProvider {
    seen: Mutex<Option<RecordedSpec>>,
}
#[async_trait]
impl SandboxProvider for RecordingProvider {
    fn preflight(&self) -> Preflight {
        Preflight {
            usable: true,
            detail: "recording".into(),
            install_hint: None,
        }
    }
    async fn run(&self, spec: &SandboxSpec) -> anyhow::Result<SandboxOutput> {
        *self.seen.lock().unwrap() =
            Some((spec.command.clone(), spec.env.clone(), spec.egress.clone()));
        Ok(SandboxOutput {
            code: Some(0),
            success: true,
            stdout: b"confined-ok".to_vec(),
            stderr: vec![],
        })
    }
}

// ── Case 1: unprofiled + no provider → runs as TODAY (the High/High guard) ───

#[tokio::test]
async fn unprofiled_script_runs_unconfined_exactly_as_before() {
    let exec = ScriptExecutor::new(); // default: no provider, not strict
    let out = exec
        .execute(req(
            json!({ "kind": "script", "subject": "run.echo.hello" }),
        ))
        .await
        .expect("unprofiled script still runs");
    assert_eq!(out.output["success"], true);
    assert!(
        out.output["stdout"]
            .as_str()
            .unwrap()
            .contains("hello from script")
    );
}

// ── Case 2: profiled + provider → routes through the sandbox ─────────────────

#[tokio::test]
async fn confined_script_routes_through_the_provider() {
    let provider = Arc::new(RecordingProvider::default());
    let exec = ScriptExecutor::new().with_sandbox(provider.clone());
    let out = exec
        .execute(req(json!({
            "kind": "script", "subject": "run.echo.hello", "confinement": "confined"
        })))
        .await
        .expect("confined script runs via provider");

    // The executor adapted the provider's output, not bash's.
    assert_eq!(out.output["success"], true);
    assert!(
        out.output["stdout"]
            .as_str()
            .unwrap()
            .contains("confined-ok")
    );

    // The spec carried the real command, praxec env, and deny-all egress.
    let (command, env, egress) = provider
        .seen
        .lock()
        .unwrap()
        .clone()
        .expect("provider was called");
    assert!(
        command.iter().any(|c| c == "bash"),
        "bash invocation: {command:?}"
    );
    assert_eq!(egress, Egress::DenyAll);
    assert!(env.iter().any(|(k, _)| k == "PRAXEC_SCRIPT_SUBJECT"));
}

// ── Case 3: profiled + NO provider → fail-fast, naming the script ────────────

#[tokio::test]
async fn confined_script_without_a_provider_fails_fast() {
    let exec = ScriptExecutor::new(); // no provider configured
    let err = exec
        .execute(req(json!({
            "kind": "script", "subject": "run.echo.hello", "confinement": "confined"
        })))
        .await
        .expect_err("must refuse — never silently run unconfined");
    let msg = format!("{err:?}");
    assert!(msg.contains("CONFINEMENT_UNAVAILABLE"), "got: {msg}");
    assert!(msg.contains("run.echo.hello"), "names the script: {msg}");
}

// ── Case 4: strict mode + unprofiled → fail-fast, naming the script ──────────

#[tokio::test]
async fn strict_mode_unprofiled_script_fails_fast() {
    let exec = ScriptExecutor::new().require_confinement(true);
    let err = exec
        .execute(req(
            json!({ "kind": "script", "subject": "run.echo.hello" }),
        ))
        .await
        .expect_err("strict mode must refuse an unprofiled script");
    let msg = format!("{err:?}");
    assert!(msg.contains("CONFINEMENT_REQUIRED"), "got: {msg}");
    assert!(msg.contains("run.echo.hello"), "names the script: {msg}");
}

// ── Invalid profile → fail-fast (poka-yoke, not a silent default) ────────────

#[tokio::test]
async fn invalid_confinement_profile_fails_fast() {
    let exec = ScriptExecutor::new();
    let err = exec
        .execute(req(json!({
            "kind": "script", "subject": "run.echo.hello", "confinement": "sandboxed"
        })))
        .await
        .expect_err("typo'd profile must error");
    assert!(format!("{err:?}").contains("INVALID_CONFINEMENT"));
}
