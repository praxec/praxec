//! `path_grounding` executor — deterministic path-hallucination gate (#7).
//!
//! Root-grounding (the mandatory `$.run.repo_root`) guarantees the run's repo
//! ROOT is real. This gate guarantees every file PATH an agent referenced in a
//! plan/draft actually EXISTS under that root — catching the second
//! hallucination class: a plan that invents `src/sysadmin/.../CatalogView.tsx`
//! when the real tree is `src/_components/tools/SysadminSignalCatalog/`.
//!
//! Deterministic and fail-closed: wired as an `actor: deterministic` transition
//! immediately BEFORE a human sign-off, it reads the referenced-path set from
//! config-named context pointers and checks each against the filesystem. Any
//! missing path (or a `..`/absolute escape) returns `ExecutorError::Permanent`
//! → `CHAIN_FAILED`, so the instance stays put and never advances past the gate
//! on invented paths. No model, no budget.
//!
//! Data-driven: `groundedPaths` names one or more context pointers holding path
//! strings (or arrays / arrays-of-objects with a `path`/`file` field). Paths in
//! `groundedPaths` must EXIST (edit/read targets); paths in the optional
//! `createPaths` are to-be-created, so only their PARENT dir must exist (this is
//! the FM-7 mitigation — a legitimate new file is not false-blocked).

use std::path::Path;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::mapping::read_in_scopes;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::path_safety::resolve_under;
use praxec_core::ports::Executor;
use serde_json::{Value, json};

/// Deterministic, stateless. Needs no live gateway handle, so it wires straight
/// into the default registry (unlike `inventory`, which needs the discovery
/// index).
#[derive(Default)]
pub struct PathGroundingExecutor;

impl PathGroundingExecutor {
    pub fn new() -> Self {
        Self
    }
}

/// Flatten a resolved pointer value into a set of path strings, tolerant of
/// plan shape: a string → one path; an array → its elements (recursively); an
/// object → its `path` or `file` field. Anything else is ignored (robust to
/// plan-schema drift) — the empty-set case is caught by `requireNonEmpty`.
fn collect_paths(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(arr) => {
            for e in arr {
                collect_paths(e, out);
            }
        }
        Value::Object(o) => {
            if let Some(Value::String(p)) = o.get("path").or_else(|| o.get("file")) {
                out.push(p.clone());
            }
        }
        _ => {}
    }
}

/// Resolve every declared pointer under `key` and flatten to a path set.
fn gather(request: &ExecuteRequest, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let cfg = &request.executor_config;
    for ptr in cfg.get(key).and_then(Value::as_array).into_iter().flatten() {
        let Some(expr) = ptr.as_str() else { continue };
        if let Some(val) = read_in_scopes(
            expr,
            &request.arguments,
            &request.workflow.context,
            &request.workflow.input,
            None,
            Some(&request.workflow.run_env),
        ) {
            collect_paths(&val, &mut out);
        }
    }
    out
}

#[async_trait]
impl Executor for PathGroundingExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let cfg = &request.executor_config;

        // The root to ground against — defaults to the run-ambient repo root.
        let root_expr = cfg
            .get("repoRoot")
            .and_then(Value::as_str)
            .unwrap_or("$.run.repo_root");
        let root = read_in_scopes(
            root_expr,
            &request.arguments,
            &request.workflow.context,
            &request.workflow.input,
            None,
            Some(&request.workflow.run_env),
        )
        .and_then(|v| v.as_str().map(str::to_string))
        .ok_or_else(|| {
            ExecutorError::Permanent(format!(
                "PATH_GROUNDING_NO_ROOT: `repoRoot` ({root_expr}) did not resolve to a path; \
                 cannot ground referenced paths. A run always carries `$.run.repo_root`."
            ))
        })?;
        let root_path = Path::new(&root);

        let grounded = gather(&request, "groundedPaths");
        let creates = gather(&request, "createPaths");

        // Declared-but-empty is fail-closed by default: a plan that references
        // zero files, gated for grounding, is suspicious — not vacuously OK.
        let require_non_empty = cfg
            .get("requireNonEmpty")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if require_non_empty && grounded.is_empty() && creates.is_empty() {
            return Err(ExecutorError::Permanent(
                "PATH_GROUNDING_EMPTY: no referenced paths resolved to check. A plan gated for \
                 grounding must reference at least one path; refusing to sign off ungrounded."
                    .into(),
            ));
        }

        let mut missing: Vec<(String, String)> = Vec::new();
        let mut checked = 0usize;

        // Edit/read targets: the file itself must exist.
        for p in &grounded {
            checked += 1;
            match resolve_under(root_path, p) {
                // An escape is a distinct, louder class than "missing" — a plan
                // path that leaves the repo is never a hallucination to re-mint,
                // it's a hard refusal.
                Err(e) => {
                    return Err(ExecutorError::Permanent(format!(
                        "PATH_ESCAPE: referenced path `{p}` escapes the repo root ({root}): {e}"
                    )));
                }
                Ok(abs) => {
                    if !abs.exists() {
                        missing.push((p.clone(), "does not exist under repo root".into()));
                    }
                }
            }
        }
        // Create targets: the file is new, so only its parent dir must exist.
        for p in &creates {
            checked += 1;
            match resolve_under(root_path, p) {
                Err(e) => {
                    return Err(ExecutorError::Permanent(format!(
                        "PATH_ESCAPE: create-target `{p}` escapes the repo root ({root}): {e}"
                    )));
                }
                Ok(abs) => {
                    let parent_exists = abs.parent().map(Path::exists).unwrap_or(false);
                    if !parent_exists {
                        missing.push((
                            p.clone(),
                            "parent directory does not exist under repo root (create target)"
                                .into(),
                        ));
                    }
                }
            }
        }

        if !missing.is_empty() {
            let lines: Vec<String> = missing
                .iter()
                .map(|(p, why)| format!("  - {p} ({why})"))
                .collect();
            return Err(ExecutorError::Permanent(format!(
                "PATH_NOT_GROUNDED: {} of {checked} referenced path(s) do not exist under \
                 repo_root ({root}):\n{}\nThe plan references files that aren't in the tree — fix \
                 the paths (or re-mint the plan) against the real repo, then retry.",
                missing.len(),
                lines.join("\n")
            )));
        }

        Ok(ExecuteResult {
            output: json!({
                "grounding": { "grounded": true, "checked": checked, "missing": [] }
            }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use praxec_core::model::WorkflowInstance;
    use praxec_core::{RepoRoot, RunEnv};
    use serde_json::json;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// A repo tree with `src/_components/tools/SysadminSignalCatalog/View.tsx`
    /// present — the real path from the motivating hallucination.
    fn repo() -> TempDir {
        let td = TempDir::new().unwrap();
        let real = td
            .path()
            .join("src/_components/tools/SysadminSignalCatalog");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("View.tsx"), b"// real\n").unwrap();
        td
    }

    fn request(root: &std::path::Path, cfg: Value, context: Value) -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf_pg".into(),
                definition_id: "flow.add-feature".into(),
                definition_version: "0".into(),
                definition: Value::Null,
                state: "grounding_plan".into(),
                version: 0,
                input: json!({}),
                context,
                started_at: Utc::now(),
                run_env: RunEnv::new(RepoRoot::new(root).unwrap(), None, None),
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("ground".into()),
            arguments: json!({}),
            executor_config: cfg,
            idempotency_key: None,
            correlation_id: None,
        }
    }

    fn cfg(grounded: Value) -> Value {
        json!({ "kind": "path_grounding", "groundedPaths": grounded })
    }

    #[tokio::test]
    async fn all_referenced_paths_exist_grounds() {
        let td = repo();
        let ctx = json!({ "draft_plan": {
            "files": ["src/_components/tools/SysadminSignalCatalog/View.tsx"]
        }});
        let req = request(td.path(), cfg(json!(["$.context.draft_plan.files"])), ctx);
        let out = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect("all paths exist → grounds");
        assert_eq!(out.output["grounding"]["grounded"], true);
        assert_eq!(out.output["grounding"]["checked"], 1);
    }

    #[tokio::test]
    async fn a_missing_path_fails_closed_and_names_it() {
        let td = repo();
        // The invented path from the motivating example.
        let invented = "src/sysadmin/signal-catalog/components/CatalogView.tsx";
        let ctx = json!({ "draft_plan": { "files": [
            "src/_components/tools/SysadminSignalCatalog/View.tsx",
            invented,
        ]}});
        let req = request(td.path(), cfg(json!(["$.context.draft_plan.files"])), ctx);
        let err = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect_err("a missing path must fail closed");
        match err {
            ExecutorError::Permanent(m) => {
                assert!(m.contains("PATH_NOT_GROUNDED"), "{m}");
                assert!(m.contains(invented), "must name the missing path: {m}");
                // The real one must NOT be listed as missing.
                assert!(!m.contains("- src/_components/tools"), "{m}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_parent_escape_is_a_distinct_hard_refusal() {
        let td = repo();
        let ctx = json!({ "draft_plan": { "files": ["../../etc/passwd"] }});
        let req = request(td.path(), cfg(json!(["$.context.draft_plan.files"])), ctx);
        let err = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect_err("escape must refuse");
        assert!(
            matches!(err, ExecutorError::Permanent(m) if m.contains("PATH_ESCAPE")),
            "escape must be its own class"
        );
    }

    #[tokio::test]
    async fn empty_path_set_fails_closed_by_default() {
        let td = repo();
        // Pointer resolves to nothing.
        let req = request(
            td.path(),
            cfg(json!(["$.context.draft_plan.files"])),
            json!({ "draft_plan": {} }),
        );
        let err = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect_err("empty set fails closed");
        assert!(matches!(err, ExecutorError::Permanent(m) if m.contains("PATH_GROUNDING_EMPTY")));
    }

    #[tokio::test]
    async fn create_target_grounds_when_parent_exists() {
        let td = repo();
        // A NEW file under an existing dir — parent exists, file does not.
        let ctx = json!({
            "draft_plan": { "files": ["src/_components/tools/SysadminSignalCatalog/View.tsx"] },
            "new_files": ["src/_components/tools/SysadminSignalCatalog/NewView.tsx"]
        });
        let mut c = cfg(json!(["$.context.draft_plan.files"]));
        c["createPaths"] = json!(["$.context.new_files"]);
        let req = request(td.path(), c, ctx);
        let out = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect("create target with existing parent grounds");
        assert_eq!(out.output["grounding"]["checked"], 2);
    }

    #[tokio::test]
    async fn create_target_with_missing_parent_fails_closed() {
        let td = repo();
        let ctx = json!({
            "draft_plan": { "files": ["src/_components/tools/SysadminSignalCatalog/View.tsx"] },
            "new_files": ["src/imaginary/deep/tree/NewView.tsx"]
        });
        let mut c = cfg(json!(["$.context.draft_plan.files"]));
        c["createPaths"] = json!(["$.context.new_files"]);
        let req = request(td.path(), c, ctx);
        let err = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect_err("create target under a nonexistent parent fails");
        assert!(matches!(err, ExecutorError::Permanent(m) if m.contains("parent directory")));
    }

    #[tokio::test]
    async fn missing_root_fails_closed() {
        // `repoRoot` points somewhere that doesn't resolve.
        let td = repo();
        let mut c = cfg(json!(["$.context.draft_plan.files"]));
        c["repoRoot"] = json!("$.context.nope");
        let req = request(
            td.path(),
            c,
            json!({ "draft_plan": { "files": ["View.tsx"] } }),
        );
        let err = PathGroundingExecutor::new()
            .execute(req)
            .await
            .expect_err("unresolvable root fails closed");
        assert!(matches!(err, ExecutorError::Permanent(m) if m.contains("PATH_GROUNDING_NO_ROOT")));
    }

    #[test]
    fn collect_paths_handles_strings_arrays_and_objects() {
        let mut out = Vec::new();
        collect_paths(&json!("a.rs"), &mut out);
        collect_paths(
            &json!(["b.rs", { "path": "c.rs" }, { "file": "d.rs" }]),
            &mut out,
        );
        collect_paths(&json!({ "path": "e.rs" }), &mut out);
        assert_eq!(out, vec!["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"]);
        let _ = PathBuf::new();
    }
}
