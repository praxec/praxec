//! #206 — a portable gateway config must work unchanged from linked Git
//! worktrees, with durable governance state shared through Git's common dir.

use std::process::Command;

use serde_json::Value;

const HELLO_FLOW: &str = include_str!("../../../examples/hello-flow.yaml");

fn git(cwd: &std::path::Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|err| panic!("running git {args:?}: {err}"));
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn command_from(worktree: &std::path::Path, request: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_praxec"))
        .current_dir(worktree)
        .args(["command", "--config", "gateway.yaml", request])
        .env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json")
        .output()
        .expect("run praxec command");
    assert!(
        out.status.success(),
        "command failed from {}\nstdout: {}\nstderr: {}",
        worktree.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("command output is JSON")
}

fn query_from(worktree: &std::path::Path, workflow_id: &str) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_praxec"))
        .current_dir(worktree)
        .args([
            "query",
            "--config",
            "gateway.yaml",
            &format!(r#"{{"workflowId":"{workflow_id}"}}"#),
        ])
        .env("PRAXEC_EMBEDDING_FILE", "/tmp/praxec-no-embed.json")
        .output()
        .expect("run praxec query");
    assert!(
        out.status.success(),
        "query failed from {}\nstdout: {}\nstderr: {}",
        worktree.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("query output is JSON")
}

#[test]
fn linked_worktrees_share_portable_governance_without_config_edits() {
    let dir = tempfile::tempdir().unwrap();
    let primary = dir.path().join("primary");
    std::fs::create_dir_all(&primary).unwrap();
    git(&primary, &["init", "--quiet"]);
    git(&primary, &["config", "user.email", "test@example.invalid"]);
    git(&primary, &["config", "user.name", "Praxec test"]);
    // The shared fixture carries legacy internal `_writableRepos` metadata for
    // older tests. Remove it here: #206 specifically proves a host-only,
    // portable config discovers each invocation worktree without any root path.
    let portable_flow = HELLO_FLOW.replace(
        "praxec:\n  _writableRepos:\n    - root: \".\"\n      push: false\n\n",
        "",
    );
    std::fs::write(
        primary.join("gateway.yaml"),
        format!(
            "{portable_flow}\naudit: {{ sink: file, path: .praxec/audit-logs, rotation: daily }}\n"
        ),
    )
    .unwrap();
    git(&primary, &["add", "gateway.yaml"]);
    git(&primary, &["commit", "--quiet", "-m", "portable config"]);

    let linked = dir.path().join("linked");
    git(
        &primary,
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );

    let started = command_from(&primary, r#"{"definitionId":"hello_flow"}"#);
    let id = started
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .unwrap();
    let observed = query_from(&linked, id);
    assert_eq!(
        observed.pointer("/workflow/id").and_then(Value::as_str),
        Some(id)
    );
}
