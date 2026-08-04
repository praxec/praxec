//! Config-readiness invariants — the onboarding-hardening keystone + D1.
//!
//! These turn "0 errors but nothing can run" LOUD. They are pure functions over
//! the resolved config `Value` so BOTH `check` and `doctor` call the exact same
//! logic (they must never disagree about whether a config is runnable).
//!
//! - **D1 ([`models_yaml_load_finding`])** — a DECLARED but unreadable
//!   `gateway.models_yaml` (missing / unreadable / unparseable) is a hard
//!   `MODELS_YAML_LOAD_FAILED` error, not a silent runtime WARN. Key absent ⇒ no
//!   finding (a config may legitimately run with no affinity/agent bindings).
//! - **Keystone ([`agent_readiness_findings`])** — for every MOUNTED definition
//!   with a `kind: agent` step or an `affinity:` step, the affinity MUST resolve
//!   to a binding in the in-force `models.yaml` (an `activity:`/`overrides:`
//!   entry OR the `default:` chain). An affinity that resolves to nothing is
//!   `AFFINITY_UNBOUND` — surfaced with the pack's recommendation (from
//!   `/praxec/_packAffinities`) + the exact snippet, not a silent commodity
//!   fallback. NO agent/affinity steps ⇒ no finding (no false positive).
//!
//! ADDITIVE ONLY: these add validation + output; they never remove or relax an
//! existing gate. The keystone runs only when models.yaml LOADS — an unset key
//! or unloadable file is already covered by `AGENT_MODELS_YAML_REQUIRED` /
//! `MODELS_YAML_LOAD_FAILED`, so there is no double-report.

use std::collections::BTreeSet;
use std::path::Path;

use praxec_core::validate::{Diagnostic, for_each_executor_site};
use serde_json::Value;

use crate::affinity_resolver::{AgentsYamlAffinityResolver, resolve_affinity_to_model};
use crate::models_bind::{activity_snippet, split_recommended};

/// D1 — a declared-but-unreadable `gateway.models_yaml` is a hard error.
///
/// Returns `Some(MODELS_YAML_LOAD_FAILED)` iff the key is PRESENT and the file
/// cannot be loaded (missing, unreadable, or unparseable). Key absent ⇒ `None`
/// (the keystone / `AGENT_MODELS_YAML_REQUIRED` handle the "need a models.yaml"
/// case; a config with no agent/affinity step legitimately needs none).
pub fn models_yaml_load_finding(config: &Value) -> Option<Diagnostic> {
    let path = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)?;
    match AgentsYamlAffinityResolver::from_path(Path::new(path)) {
        Ok(_) => None,
        Err(err) => Some(Diagnostic::Error(format!(
            "MODELS_YAML_LOAD_FAILED: gateway.models_yaml = `{path}` is declared but could not be \
             loaded ({err}). `kind: agent` model bindings and affinity-resolved `kind: llm` steps \
             cannot resolve, so every such step would fail at dispatch. Fix the path or the file \
             (or remove the `gateway.models_yaml` key) before serving."
        ))),
    }
}

/// One MOUNTED agent-step affinity that resolves to no model — the structured
/// keystone finding (de-duplicated per (pack, affinity)). `doctor --fix` reads
/// these to know which affinities to bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnboundAffinity {
    /// The definition id the step lives in (namespaced `<ns>/<id>` for a pack).
    pub wf_id: String,
    /// Human-readable step location (from [`for_each_executor_site`]).
    pub location: String,
    /// The pack namespace (`<ns>`), or `None` for a host-defined definition.
    pub pack: Option<String>,
    /// The unbound affinity name.
    pub affinity: String,
}

/// Keystone core — the structured list of MOUNTED agent-step affinities that
/// resolve to no model. De-duplicated per (pack, affinity), deterministic order.
///
/// Runs only when `gateway.models_yaml` LOADS — otherwise D1 /
/// `AGENT_MODELS_YAML_REQUIRED` own that case (no double-report). A definition
/// with no in-scope step contributes nothing (no false positive).
pub fn unbound_affinities(config: &Value) -> Vec<UnboundAffinity> {
    let mut out = Vec::new();

    let Some(path) = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    else {
        return out; // unset — the keystone has no in-force models.yaml to check
    };
    let Ok(loaded) = AgentsYamlAffinityResolver::from_path(Path::new(path)) else {
        return out; // unloadable — D1 (MODELS_YAML_LOAD_FAILED) owns this
    };
    let resolver = loaded.resolver();

    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return out;
    };

    // Deterministic iteration + de-dup per (pack, affinity) so one requirement
    // reports once regardless of how many steps use it.
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let mut wf_ids: Vec<&String> = workflows.keys().collect();
    wf_ids.sort();

    for wf_id in wf_ids {
        let def = &workflows[wf_id];
        // A mounted pack definition is namespaced `<ns>/<id>`; a host-defined one
        // has no `/`. The namespace (when present) attributes the recommendation.
        let pack = wf_id.split_once('/').map(|(ns, _)| ns.to_string());
        for_each_executor_site(def, |site| {
            // In scope: a `kind: agent` step OR any step declaring `affinity:`.
            // Since a `kind: agent` step always carries its `affinity:`, requiring
            // an affinity to be present covers both — and a step with no affinity
            // has nothing to resolve (a `kind: agent` step missing its affinity is
            // an agent-config error surfaced elsewhere — never a false
            // AFFINITY_UNBOUND here).
            let affinity = site
                .executor
                .get("affinity")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let Some(aff) = affinity else {
                return;
            };
            // Bound — including via the `default:` chain — is runnable: not flagged.
            if resolve_affinity_to_model(resolver, aff).is_some() {
                return;
            }
            let pack_key = pack.clone().unwrap_or_else(|| "<host>".to_string());
            if !seen.insert((pack_key, aff.to_string())) {
                return;
            }
            out.push(UnboundAffinity {
                wf_id: wf_id.clone(),
                location: site.location.clone(),
                pack: pack.clone(),
                affinity: aff.to_string(),
            });
        });
    }
    out
}

/// Keystone — the agent-readiness invariant as reportable diagnostics. For every
/// MOUNTED definition with a `kind: agent` step or a step declaring `affinity:`,
/// assert the affinity resolves to a model in the in-force models.yaml. An
/// unbound affinity is `AFFINITY_UNBOUND`, surfaced with the pack's
/// recommendation when one is declared. See [`unbound_affinities`].
pub fn agent_readiness_findings(config: &Value) -> Vec<Diagnostic> {
    let path = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
        .unwrap_or("<unset>");
    unbound_affinities(config)
        .into_iter()
        .map(|u| {
            Diagnostic::Error(unbound_message(
                config,
                &u.wf_id,
                &u.location,
                u.pack.as_deref(),
                &u.affinity,
                path,
            ))
        })
        .collect()
}

/// Build the `AFFINITY_UNBOUND` message, surfacing the pack's recommendation +
/// the exact models.yaml snippet when the pack declared one.
fn unbound_message(
    config: &Value,
    wf_id: &str,
    location: &str,
    pack: Option<&str>,
    affinity: &str,
    models_yaml: &str,
) -> String {
    let mut msg = format!(
        "AFFINITY_UNBOUND: definition `{wf_id}` ({location}) uses affinity `{affinity}`, but it \
         resolves to no model in the in-force models.yaml (`{models_yaml}`) — no \
         `activity:`/`overrides:` entry and no `default:` chain covers it. Every such step would \
         fail at first dispatch with no model."
    );
    let pack_label = pack.unwrap_or("<host>");
    match crate::models_bind::recommended_for(config, affinity) {
        Some(rec) => {
            let cap = crate::models_bind::capability_for(config, affinity);
            let cap_suffix = cap.map(|c| format!(" ({c})")).unwrap_or_default();
            msg.push_str(&format!(
                "\n    pack `{pack_label}` recommends `{rec}` for `{affinity}`{cap_suffix}."
            ));
            if let Some((provider, model)) = split_recommended(&rec) {
                let snippet = activity_snippet(affinity, provider, model);
                let indented = snippet
                    .lines()
                    .map(|l| format!("      {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                msg.push_str(&format!(
                    "\n    add to models.yaml:\n{indented}\n    or run `praxec models bind {affinity}`."
                ));
            } else {
                msg.push_str(&format!(
                    " Run `praxec models bind {affinity}` to write it."
                ));
            }
        }
        None => {
            msg.push_str(&format!(
                "\n    bind `{affinity}` in models.yaml (add an `activity: {affinity}:` list or a \
                 `default:` chain)."
            ));
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_models(dir: &Path, body: &str) -> std::path::PathBuf {
        let p = dir.join("models.yaml");
        std::fs::write(&p, body).unwrap();
        p
    }

    // ── D1 ──────────────────────────────────────────────────────────────────

    #[test]
    fn d1_unset_key_is_clean() {
        assert!(models_yaml_load_finding(&json!({})).is_none());
    }

    #[test]
    fn d1_declared_but_missing_file_is_a_hard_error() {
        // The exact report repro: gateway.models_yaml points at a path that does
        // not exist. Today the runtime only WARNs; this makes it a hard error.
        let cfg = json!({ "gateway": { "models_yaml": "/no/such/NONEXISTENT.yaml" } });
        let finding = models_yaml_load_finding(&cfg).expect("dangling path must error");
        assert!(
            matches!(&finding, Diagnostic::Error(m) if m.contains("MODELS_YAML_LOAD_FAILED")),
            "got {finding:?}"
        );
    }

    #[test]
    fn d1_declared_but_unparseable_file_is_a_hard_error() {
        let td = tempfile::tempdir().unwrap();
        let p = write_models(td.path(), "this: is: not: valid: models: yaml: [");
        let cfg = json!({ "gateway": { "models_yaml": p.to_str().unwrap() } });
        assert!(
            matches!(models_yaml_load_finding(&cfg), Some(Diagnostic::Error(m)) if m.contains("MODELS_YAML_LOAD_FAILED"))
        );
    }

    #[test]
    fn d1_declared_and_loadable_is_clean() {
        let td = tempfile::tempdir().unwrap();
        let p = write_models(
            td.path(),
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n",
        );
        let cfg = json!({ "gateway": { "models_yaml": p.to_str().unwrap() } });
        assert!(models_yaml_load_finding(&cfg).is_none());
    }

    // ── keystone ────────────────────────────────────────────────────────────

    fn cfg_with_agent_step(models_path: &str, affinity: &str, pack_affinities: Value) -> Value {
        json!({
            "gateway": { "models_yaml": models_path },
            "praxec": { "_packAffinities": pack_affinities },
            "workflows": {
                "design/flow.anneal": { "states": { "s": { "transitions": {
                    "go": { "target": "done", "executor": {
                        "kind": "agent", "affinity": affinity, "goal": "do it"
                    } }
                } } } }
            }
        })
    }

    #[test]
    fn keystone_no_agent_steps_is_clean() {
        // A config whose only step is a plain `kind: noop` (no agent, no affinity)
        // must NOT newly error even with a loadable models.yaml.
        let td = tempfile::tempdir().unwrap();
        let p = write_models(
            td.path(),
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n",
        );
        let cfg = json!({
            "gateway": { "models_yaml": p.to_str().unwrap() },
            "workflows": { "wf": { "states": { "s": { "transitions": {
                "go": { "target": "done", "executor": { "kind": "noop" } }
            } } } } }
        });
        assert!(agent_readiness_findings(&cfg).is_empty());
    }

    #[test]
    fn keystone_unbound_pack_affinity_fires_with_recommendation() {
        let td = tempfile::tempdir().unwrap();
        // models.yaml has a default chain but NO binding for the `design` open key.
        let p = write_models(
            td.path(),
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n",
        );
        let cfg = cfg_with_agent_step(
            p.to_str().unwrap(),
            "design",
            json!({ "design": { "design": { "capability": "UI design annealing", "recommended": "openrouter/anthropic/claude-sonnet-4-5" } } }),
        );
        let findings = agent_readiness_findings(&cfg);
        assert_eq!(findings.len(), 1, "one unbound affinity: {findings:?}");
        let Diagnostic::Error(m) = &findings[0] else {
            panic!("expected error")
        };
        assert!(m.contains("AFFINITY_UNBOUND"), "{m}");
        assert!(m.contains("`design`"), "names the affinity: {m}");
        assert!(m.contains("design/flow.anneal"), "names the def: {m}");
        assert!(
            m.contains("openrouter/anthropic/claude-sonnet-4-5"),
            "surfaces the recommendation: {m}"
        );
        assert!(
            m.contains("praxec models bind design"),
            "offers the fix: {m}"
        );
        assert!(
            m.contains("UI design annealing"),
            "surfaces capability: {m}"
        );
    }

    #[test]
    fn keystone_default_chain_bound_affinity_does_not_fire() {
        // `coding` is a KNOWN affinity token: with no override/activity entry it
        // resolves via the `default:` chain — bound, must NOT flag.
        let td = tempfile::tempdir().unwrap();
        let p = write_models(
            td.path(),
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n",
        );
        let cfg = cfg_with_agent_step(p.to_str().unwrap(), "coding", json!({}));
        assert!(
            agent_readiness_findings(&cfg).is_empty(),
            "a default-chain-bound affinity must not be flagged"
        );
    }

    #[test]
    fn keystone_activity_bound_affinity_does_not_fire() {
        let td = tempfile::tempdir().unwrap();
        let p = write_models(
            td.path(),
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\nactivity:\n  design:\n    - { provider: { name: openrouter }, model: anthropic/claude-sonnet-4-5 }\n",
        );
        let cfg = cfg_with_agent_step(p.to_str().unwrap(), "design", json!({}));
        assert!(agent_readiness_findings(&cfg).is_empty());
    }

    #[test]
    fn keystone_unbound_without_recommendation_still_fires() {
        let td = tempfile::tempdir().unwrap();
        let p = write_models(
            td.path(),
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n",
        );
        let cfg = cfg_with_agent_step(p.to_str().unwrap(), "rollout", json!({}));
        let findings = agent_readiness_findings(&cfg);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let Diagnostic::Error(m) = &findings[0] else {
            panic!()
        };
        assert!(
            m.contains("AFFINITY_UNBOUND") && m.contains("`rollout`"),
            "{m}"
        );
    }

    #[test]
    fn keystone_skipped_when_models_yaml_unloadable() {
        // Unloadable models.yaml is D1's concern — the keystone must not
        // double-report (returns empty so only MODELS_YAML_LOAD_FAILED fires).
        let cfg = cfg_with_agent_step("/no/such/models.yaml", "design", json!({}));
        assert!(agent_readiness_findings(&cfg).is_empty());
    }
}
