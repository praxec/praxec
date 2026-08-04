//! `praxec models bind <affinity>` + the `doctor --fix` binding path.
//!
//! onboarding-hardening — the "requirement travels, the key stays local"
//! wiring. A pack DECLARES the affinities its definitions use plus a
//! `recommended:` binding (carried into the resolved config as
//! `/praxec/_packAffinities`). This module writes that recommendation into the
//! OPERATOR's `models.yaml` — using the operator's EXISTING provider env, never
//! fabricating a key — so a pulled pack self-wires in one command.
//!
//! Invariants (poka-yoke):
//! - NEVER overwrites an existing binding (idempotent + non-clobbering): a
//!   bound affinity yields [`BindOutcome::AlreadyBound`], nothing is written.
//! - NEVER fabricates a key: when the recommended provider needs an env var the
//!   operator doesn't have, it prints the manual snippet
//!   ([`BindOutcome::NoProviderKey`]) rather than writing a dead binding.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::affinity_resolver::{AgentsYamlAffinityResolver, resolve_affinity_to_model};

/// The result of a bind attempt. Every arm is a first-class, reported outcome —
/// none is a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOutcome {
    /// Wrote `activity: { <affinity>: [ { provider, model } ] }` into models.yaml.
    Wrote {
        affinity: String,
        provider: String,
        model: String,
        path: PathBuf,
    },
    /// The affinity already resolves to a model — never clobbered.
    AlreadyBound { affinity: String },
    /// No loaded pack recommends this affinity (nothing to write).
    NoRecommendation { affinity: String },
    /// `gateway.models_yaml` is unset — there is no bindings file to write into.
    NoModelsYaml { affinity: String },
    /// The recommended provider needs an env var the operator doesn't have — the
    /// manual snippet is surfaced instead of writing a dead binding.
    NoProviderKey {
        affinity: String,
        provider: String,
        missing_vars: Vec<String>,
        snippet: String,
    },
}

/// Look up the pack `recommended:` binding for `affinity` in the resolved
/// config's `/praxec/_packAffinities` (searching every namespace). Returns the
/// first `"<provider>/<model-id>"` recommendation found.
pub fn recommended_for(config: &Value, affinity: &str) -> Option<String> {
    let by_ns = config
        .pointer("/praxec/_packAffinities")
        .and_then(Value::as_object)?;
    for ns_map in by_ns.values() {
        if let Some(rec) = ns_map
            .pointer(&format!("/{}/recommended", pointer_escape(affinity)))
            .and_then(Value::as_str)
        {
            return Some(rec.to_string());
        }
    }
    None
}

/// The `capability:` blurb a pack declares for `affinity`, if any (for messaging).
pub fn capability_for(config: &Value, affinity: &str) -> Option<String> {
    let by_ns = config
        .pointer("/praxec/_packAffinities")
        .and_then(Value::as_object)?;
    for ns_map in by_ns.values() {
        if let Some(cap) = ns_map
            .pointer(&format!("/{}/capability", pointer_escape(affinity)))
            .and_then(Value::as_str)
        {
            return Some(cap.to_string());
        }
    }
    None
}

/// Escape a JSON-pointer path segment (`~` → `~0`, `/` → `~1`) so an affinity
/// name is looked up literally even if it (unusually) contains those chars.
fn pointer_escape(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// Split a `"<provider>/<model-id>"` recommendation. The model-id itself may
/// contain `/` (e.g. `openrouter/z-ai/glm-5.2` → `("openrouter", "z-ai/glm-5.2")`),
/// so only the FIRST segment is the provider.
pub fn split_recommended(recommended: &str) -> Option<(&str, &str)> {
    let (provider, model) = recommended.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider, model))
}

/// The exact `models.yaml` snippet that binds `affinity` to `provider`/`model`
/// under `activity:` — the copy-pasteable remedy surfaced by the keystone and
/// `models bind`'s manual-fallback path.
pub fn activity_snippet(affinity: &str, provider: &str, model: &str) -> String {
    format!(
        "activity:\n  {affinity}:\n    - provider: {{ name: {provider} }}\n      model: {model}"
    )
}

/// Production entry: bind over the process env (providers.env is loaded into env
/// at startup, so this sees file + env keys).
pub fn bind_affinity(config: &Value, affinity: &str) -> anyhow::Result<BindOutcome> {
    bind_affinity_with(config, affinity, |v| std::env::var(v).is_ok())
}

/// [`bind_affinity`] with an injectable env lookup (test seam) — the decision
/// logic is unit-testable without touching the process env.
pub fn bind_affinity_with(
    config: &Value,
    affinity: &str,
    has_env: impl Fn(&str) -> bool,
) -> anyhow::Result<BindOutcome> {
    // 1. A recommendation must exist to bind (models bind is pack-driven).
    let Some(recommended) = recommended_for(config, affinity) else {
        return Ok(BindOutcome::NoRecommendation {
            affinity: affinity.to_string(),
        });
    };
    let Some((provider, model)) = split_recommended(&recommended) else {
        anyhow::bail!(
            "pack recommendation `{recommended}` for affinity `{affinity}` is not a \
             `<provider>/<model-id>` string"
        );
    };

    // 2. A models.yaml path is required to write a binding into.
    let Some(models_yaml) = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    else {
        return Ok(BindOutcome::NoModelsYaml {
            affinity: affinity.to_string(),
        });
    };
    let models_path = Path::new(models_yaml);

    // 3. NEVER clobber: if the affinity already resolves to a model, stop.
    //    Uses the SAME resolution the runtime does (activity/override/default),
    //    so `already bound` means exactly `already runnable`.
    if let Ok(loaded) = AgentsYamlAffinityResolver::from_path(models_path) {
        if resolve_affinity_to_model(loaded.resolver(), affinity).is_some() {
            return Ok(BindOutcome::AlreadyBound {
                affinity: affinity.to_string(),
            });
        }
    }

    // 4. NEVER fabricate a key: bind only if the recommended provider's
    //    credential is resolvable (keyless providers need nothing).
    let missing_vars = provider_missing_env(provider, &has_env);
    if !missing_vars.is_empty() {
        return Ok(BindOutcome::NoProviderKey {
            affinity: affinity.to_string(),
            provider: provider.to_string(),
            missing_vars,
            snippet: activity_snippet(affinity, provider, model),
        });
    }

    // 5. Write the binding under `activity:` — additive, non-clobbering. Load the
    //    existing file (or start a minimal one), insert only if the key is absent.
    //    The write reports whether it actually inserted: an already-present key is
    //    AlreadyBound even when the resolver above could not load the file (e.g. a
    //    models.yaml with no `default:` section) — the non-clobbering guarantee is
    //    the write's `contains_key` guard, not resolver load success.
    if write_activity_binding(models_path, affinity, provider, model)? {
        Ok(BindOutcome::Wrote {
            affinity: affinity.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            path: models_path.to_path_buf(),
        })
    } else {
        Ok(BindOutcome::AlreadyBound {
            affinity: affinity.to_string(),
        })
    }
}

/// Which of the recommended provider's required env vars are NOT resolvable.
/// A keyless/local provider (or an unknown slug with no curated key) returns an
/// empty vec — nothing to gate on.
fn provider_missing_env(provider: &str, has_env: &impl Fn(&str) -> bool) -> Vec<String> {
    use praxec_core::providers::ProviderId;
    match ProviderId::from_slug(provider) {
        Some(p) => p
            .credentials()
            .env_vars()
            .iter()
            .filter(|v| !has_env(v))
            .map(|v| v.to_string())
            .collect(),
        None => Vec::new(),
    }
}

/// Insert `activity: { <affinity>: [ { provider: { name }, model } ] }` into the
/// models.yaml at `path`, preserving everything else. Non-clobbering: if the
/// `activity:` map already carries this affinity key, it is left untouched.
fn write_activity_binding(
    path: &Path,
    affinity: &str,
    provider: &str,
    model: &str,
) -> anyhow::Result<bool> {
    use serde_yaml::{Mapping, Value as Yaml};

    let mut doc: Yaml = match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => serde_yaml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {} to add a binding: {e}", path.display()))?,
        // Absent/empty file → start a minimal document.
        _ => Yaml::Mapping(Mapping::new()),
    };
    let Yaml::Mapping(root) = &mut doc else {
        anyhow::bail!(
            "{} is not a YAML mapping — cannot add an affinity binding",
            path.display()
        );
    };
    // Ensure a version marker exists (a fresh minimal file).
    root.entry(Yaml::String("version".into()))
        .or_insert(Yaml::Number(1.into()));

    let activity = root
        .entry(Yaml::String("activity".into()))
        .or_insert_with(|| Yaml::Mapping(Mapping::new()));
    let Yaml::Mapping(activity) = activity else {
        anyhow::bail!(
            "`activity:` in {} is not a mapping — cannot add an affinity binding",
            path.display()
        );
    };
    let key = Yaml::String(affinity.to_string());
    if activity.contains_key(&key) {
        // Non-clobbering: an already-present activity key is never overwritten.
        return Ok(false);
    }
    let mut provider_map = Mapping::new();
    provider_map.insert(Yaml::String("name".into()), Yaml::String(provider.into()));
    let mut binding = Mapping::new();
    binding.insert(Yaml::String("provider".into()), Yaml::Mapping(provider_map));
    binding.insert(Yaml::String("model".into()), Yaml::String(model.into()));
    activity.insert(key, Yaml::Sequence(vec![Yaml::Mapping(binding)]));

    let rendered = serde_yaml::to_string(&doc)
        .map_err(|e| anyhow::anyhow!("serializing updated {}: {e}", path.display()))?;
    std::fs::write(path, rendered)
        .map_err(|e| anyhow::anyhow!("writing updated {}: {e}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config_with(models_yaml: &str, affinities: Value) -> Value {
        json!({
            "gateway": { "models_yaml": models_yaml },
            "praxec": { "_packAffinities": affinities },
        })
    }

    #[test]
    fn split_recommended_keeps_multi_segment_model_id() {
        assert_eq!(
            split_recommended("openrouter/z-ai/glm-5.2"),
            Some(("openrouter", "z-ai/glm-5.2"))
        );
        assert_eq!(split_recommended("noslash"), None);
        assert_eq!(split_recommended("provider/"), None);
    }

    #[test]
    fn binds_recommended_into_activity_when_key_present() {
        let td = tempfile::tempdir().unwrap();
        let models = td.path().join("models.yaml");
        std::fs::write(
            &models,
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\n",
        )
        .unwrap();
        let cfg = config_with(
            models.to_str().unwrap(),
            json!({ "design": { "design": { "recommended": "openrouter/anthropic/claude-sonnet-4-5" } } }),
        );
        // Env present for openrouter → writes.
        let out = bind_affinity_with(&cfg, "design", |v| v == "OPENROUTER_API_KEY").unwrap();
        assert!(
            matches!(&out, BindOutcome::Wrote { provider, model, .. }
                if provider == "openrouter" && model == "anthropic/claude-sonnet-4-5"),
            "got {out:?}"
        );
        // The binding now resolves.
        let loaded = AgentsYamlAffinityResolver::from_path(&models).unwrap();
        assert_eq!(
            resolve_affinity_to_model(loaded.resolver(), "design").as_deref(),
            Some("openrouter:anthropic/claude-sonnet-4-5")
        );
    }

    #[test]
    fn is_idempotent_and_never_clobbers_an_existing_binding() {
        let td = tempfile::tempdir().unwrap();
        let models = td.path().join("models.yaml");
        std::fs::write(
            &models,
            "version: 1\ndefault:\n  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }\nactivity:\n  design:\n    - { provider: { name: openai }, model: gpt-5 }\n",
        )
        .unwrap();
        let cfg = config_with(
            models.to_str().unwrap(),
            json!({ "design": { "design": { "recommended": "openrouter/anthropic/claude-sonnet-4-5" } } }),
        );
        let out = bind_affinity_with(&cfg, "design", |_| true).unwrap();
        assert_eq!(
            out,
            BindOutcome::AlreadyBound {
                affinity: "design".into()
            }
        );
        // The pre-existing binding is untouched — never overwritten.
        let text = std::fs::read_to_string(&models).unwrap();
        assert!(
            text.contains("gpt-5"),
            "existing binding preserved:\n{text}"
        );
        assert!(
            !text.contains("claude-sonnet-4-5"),
            "recommendation must NOT clobber:\n{text}"
        );
    }

    #[test]
    fn prints_manual_snippet_when_provider_key_absent() {
        let td = tempfile::tempdir().unwrap();
        let models = td.path().join("models.yaml");
        std::fs::write(&models, "version: 1\ndefault: []\n").unwrap();
        let cfg = config_with(
            models.to_str().unwrap(),
            json!({ "design": { "design": { "recommended": "openrouter/z-ai/glm-5.2" } } }),
        );
        // No env → do NOT fabricate a key; surface the manual snippet.
        let out = bind_affinity_with(&cfg, "design", |_| false).unwrap();
        match out {
            BindOutcome::NoProviderKey {
                provider, snippet, ..
            } => {
                assert_eq!(provider, "openrouter");
                assert!(snippet.contains("activity:"), "snippet: {snippet}");
                assert!(snippet.contains("z-ai/glm-5.2"), "snippet: {snippet}");
            }
            other => panic!("expected NoProviderKey, got {other:?}"),
        }
        // Nothing was written.
        let text = std::fs::read_to_string(&models).unwrap();
        assert!(!text.contains("activity"), "must not write:\n{text}");
    }

    #[test]
    fn no_recommendation_is_a_reported_outcome() {
        let td = tempfile::tempdir().unwrap();
        let models = td.path().join("models.yaml");
        std::fs::write(&models, "version: 1\n").unwrap();
        let cfg = config_with(models.to_str().unwrap(), json!({}));
        let out = bind_affinity_with(&cfg, "design", |_| true).unwrap();
        assert_eq!(
            out,
            BindOutcome::NoRecommendation {
                affinity: "design".into()
            }
        );
    }
}
