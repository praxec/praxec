//! `tool-suggest` executor — deterministic INSTALLABLE-tool surfacing.
//!
//! P3.3a of the tool lifecycle: "suggest tools during authoring" needs an
//! engine piece that, given a set of cap-verbs a workflow author is missing
//! coverage for, surfaces installable candidates from the gateway's
//! configured `registries:` — the SAME catalog `praxec.query { evaluate }`
//! reads (`praxec_core::tool_catalog`). Mirrors [`crate::inventory`] exactly:
//! a deterministic executor that surveys gateway-adjacent state (here, the
//! configured registries rather than the live discovery index) and emits a
//! typed result in one governed step — no model, no budget, no hallucinated
//! tools.
//!
//! Fail-safe throughout: a failing registry (rate-limited GitHub org, bad
//! network) becomes a warning string in the output — never an abort — because
//! [`praxec_core::tool_catalog::assemble`] already guarantees that; this
//! executor just surfaces the warnings it returns.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;
use praxec_core::tool_catalog::{CatalogIo, RegistrySpec, assemble, evaluate};
use serde_json::{Value, json};

/// Reads the configured `registries:` (the same `Arc<Vec<RegistrySpec>>` the
/// gateway parses once via `tool_catalog::registries_from`) plus an injectable
/// [`CatalogIo`] (production: [`praxec_core::tool_catalog::RealCatalogIo`]),
/// assembles the catalog, and ranks it against the requested cap-verbs.
pub struct ToolSuggestExecutor {
    registries: Arc<Vec<RegistrySpec>>,
    io: Arc<dyn CatalogIo>,
}

impl ToolSuggestExecutor {
    pub fn new(registries: Arc<Vec<RegistrySpec>>, io: Arc<dyn CatalogIo>) -> Self {
        Self { registries, io }
    }
}

/// The cap-verbs to match: the step's arguments, then the workflow blackboard
/// (`diff`/`registry`'s fallback shape), then the workflow's `start`-time
/// `input`. The third scope matters here specifically — like `inventory`,
/// `tool-suggest` is meant to fire as an `actor: deterministic` chain step,
/// and the deterministic chain always runs its executor with EMPTY
/// `arguments` (`chain_arguments = {}` — see `runtime_chain.rs`), so a
/// single-transition `cap.*` capability's caller-supplied verbs land in
/// `workflow.input`, not `arguments` (same reason `path_grounding` reads
/// `workflow.input` too). Missing or malformed input degrades to an empty
/// verb list (no matches) rather than an error: a step that forgot `verbs`
/// should get an empty, typed `suggestions: []`, not a hard failure.
fn verbs_from(request: &ExecuteRequest) -> Vec<String> {
    request
        .arguments
        .get("verbs")
        .or_else(|| request.workflow.context.get("verbs"))
        .or_else(|| request.workflow.input.get("verbs"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Executor for ToolSuggestExecutor {
    async fn execute(&self, request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        let verbs = verbs_from(&request);
        let registries = self.registries.clone();
        let io = self.io.clone();
        // Catalog assembly does blocking network IO (RealCatalogIo uses
        // `reqwest::blocking`) — run it on the blocking pool, mirroring
        // `praxec-mcp-server`'s `handle_discover`/`handle_evaluate`, so it
        // never stalls the async worker.
        let (catalog, warnings) =
            tokio::task::spawn_blocking(move || assemble(&registries, io.as_ref()))
                .await
                .map_err(|e| {
                    ExecutorError::Permanent(format!(
                        "tool-suggest: catalog assembly task panicked: {e}"
                    ))
                })?;

        let suggestions = evaluate(&catalog, &verbs);

        Ok(ExecuteResult {
            output: json!({ "suggestions": suggestions, "warnings": warnings }),
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
    use praxec_core::tool_catalog::{
        GhRepo, Requires, ToolCandidate, ToolSource, Transport, TrustTier,
    };
    use serde_json::json;

    /// A [`CatalogIo`] that always errors — used to prove a failing registry
    /// downgrades to a warning rather than aborting the executor.
    struct ErroringIo;
    impl CatalogIo for ErroringIo {
        fn github_org_repos(&self, _org: &str) -> Result<Vec<GhRepo>, String> {
            Err("rate limited".into())
        }
        fn fetch_json(&self, _url: &str) -> Result<Value, String> {
            Err("unused".into())
        }
    }

    fn candidate(name: &str, verbs: &[&str], trust_tier: TrustTier) -> ToolCandidate {
        ToolCandidate {
            name: name.to_string(),
            description: format!("{name} description"),
            transport: Transport::Stdio,
            source: ToolSource::Crate {
                name: name.to_string(),
            },
            verbs: verbs.iter().map(|v| v.to_string()).collect(),
            tags: vec![],
            trust_tier,
            requires: Requires::default(),
            provenance: "test-registry".into(),
        }
    }

    fn request(verbs: Value) -> ExecuteRequest {
        ExecuteRequest {
            workflow: WorkflowInstance {
                id: "wf_suggest".into(),
                definition_id: "cap.author.tool-suggest".into(),
                definition_version: "0".into(),
                definition: Value::Null,
                state: "ready".into(),
                version: 0,
                input: json!({}),
                context: json!({}),
                started_at: Utc::now(),
                run_env: praxec_core::RunEnv::for_test(),
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            transition: Some("submit_suggest".to_string()),
            arguments: json!({ "verbs": verbs }),
            executor_config: json!({ "kind": "tool-suggest" }),
            idempotency_key: None,
            correlation_id: None,
        }
    }

    #[tokio::test]
    async fn matches_candidates_by_verb_overlap() {
        let registries = Arc::new(vec![RegistrySpec::Static {
            name: "local".into(),
            candidates: vec![
                candidate("browser-mcp", &["diagnose", "browse"], TrustTier::Community),
                candidate("unrelated", &["deploy"], TrustTier::Verified),
            ],
        }]);
        let exec = ToolSuggestExecutor::new(registries, Arc::new(ErroringIo));
        let result = exec
            .execute(request(json!(["diagnose"])))
            .await
            .expect("tool-suggest runs");

        let suggestions = result.output["suggestions"].as_array().unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0]["name"], "browser-mcp");
        assert!(result.output["warnings"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ranks_higher_overlap_and_trust_first() {
        let registries = Arc::new(vec![RegistrySpec::Static {
            name: "local".into(),
            candidates: vec![
                candidate("a", &["diagnose"], TrustTier::Community),
                candidate("b", &["diagnose", "verify"], TrustTier::Org),
            ],
        }]);
        let exec = ToolSuggestExecutor::new(registries, Arc::new(ErroringIo));
        let result = exec
            .execute(request(json!(["diagnose", "verify"])))
            .await
            .expect("tool-suggest runs");

        let suggestions = result.output["suggestions"].as_array().unwrap();
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0]["name"], "b"); // 2 overlaps beats 1
    }

    /// Regression: on the deterministic chain (how `tool-suggest` is meant to
    /// fire, mirroring `inventory`), `ExecuteRequest.arguments` is always `{}`
    /// — the caller's verbs arrive via `workflow.input` (the `start` call's
    /// `input`), not `arguments`. Prove that scope resolves too.
    #[tokio::test]
    async fn resolves_verbs_from_workflow_input_on_the_deterministic_chain() {
        let registries = Arc::new(vec![RegistrySpec::Static {
            name: "local".into(),
            candidates: vec![candidate(
                "browser-mcp",
                &["diagnose"],
                TrustTier::Community,
            )],
        }]);
        let exec = ToolSuggestExecutor::new(registries, Arc::new(ErroringIo));

        let mut req = request(json!([]));
        req.arguments = json!({}); // arguments carries no `verbs` key at all...
        req.workflow.input = json!({ "verbs": ["diagnose"] }); // ...workflow.input does.

        let result = exec.execute(req).await.expect("tool-suggest runs");
        let suggestions = result.output["suggestions"].as_array().unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0]["name"], "browser-mcp");
    }

    #[tokio::test]
    async fn no_verbs_yields_empty_typed_suggestions() {
        let registries = Arc::new(vec![RegistrySpec::Static {
            name: "local".into(),
            candidates: vec![candidate("x", &["diagnose"], TrustTier::Community)],
        }]);
        let exec = ToolSuggestExecutor::new(registries, Arc::new(ErroringIo));
        let result = exec
            .execute(request(json!([])))
            .await
            .expect("tool-suggest runs");
        assert!(result.output["suggestions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failing_registry_becomes_a_warning_not_an_abort() {
        let registries = Arc::new(vec![
            RegistrySpec::GithubOrg {
                name: "gh".into(),
                org: "org".into(),
            },
            RegistrySpec::Static {
                name: "local".into(),
                candidates: vec![candidate("x", &["diagnose"], TrustTier::Community)],
            },
        ]);
        let exec = ToolSuggestExecutor::new(registries, Arc::new(ErroringIo));
        let result = exec
            .execute(request(json!(["diagnose"])))
            .await
            .expect("tool-suggest runs");

        assert_eq!(result.output["warnings"].as_array().unwrap().len(), 1);
        assert_eq!(result.output["suggestions"].as_array().unwrap().len(), 1);
    }
}
