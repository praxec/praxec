//! Production models.yaml-backed [`AffinityResolver`].
//!
//! SPEC §33 D9 — when the gateway config sets `gateway.models_yaml: <path>`,
//! the binary loads that file and injects this resolver into the
//! `LlmExecutor`, so a `kind: llm` executor's `affinity:` field resolves to a
//! concrete `provider:model-id` string via a Chain-of-Responsibility walk
//! over `models.yaml`. When the key is absent, the executor keeps its fail-loud
//! [`praxec_llm_executor::affinity::RejectingAffinityResolver`] default.
//!
//! The emitted prefix is the canonical catalog slug, which equals the factory
//! key [`praxec_llm_executor`]'s `DefaultProviderFactory` matches on
//! (Gemini → `"gemini"`), so [`provider_prefix`] is a pass-through over
//! `Provider::display_name()`.

use std::path::Path;

#[cfg(feature = "llm-executor")]
use praxec_core::error::ExecutorError;
use praxec_core::model_resolver::config::Provider;
use praxec_core::model_resolver::{ConfigSource, ModelRef, ModelsFile, Resolver};
#[cfg(feature = "llm-executor")]
use praxec_llm_executor::affinity::AffinityResolver;

/// models.yaml-backed affinity → `provider:model-id` resolver.
pub struct AgentsYamlAffinityResolver {
    resolver: Resolver,
}

impl AgentsYamlAffinityResolver {
    /// Load + build the resolver from an `models.yaml` on disk. The path is
    /// recorded as the `ConfigSource::Project` so resolution-exhaustion errors
    /// name the file the operator wrote.
    pub fn from_path(path: &Path) -> anyhow::Result<Self> {
        let file = ModelsFile::from_path(path)?;
        Ok(Self {
            resolver: Resolver::from_loaded(file, ConfigSource::Project(path.to_path_buf())),
        })
    }

    /// Borrow the loaded [`Resolver`] so the load-time cost-doctor can
    /// build its SYNC affinity→model closure off the SAME models.yaml the
    /// runtime resolver uses (see `main.rs::collect_diagnostics`).
    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// Test-friendly constructor: build from an in-memory YAML string. The
    /// `ConfigSource` is a synthetic `<inline>` marker (no file on disk).
    pub fn from_yaml_str(yaml: &str) -> anyhow::Result<Self> {
        let file = ModelsFile::from_yaml(yaml)?;
        Ok(Self {
            resolver: Resolver::from_loaded(
                file,
                ConfigSource::Project(Path::new("<inline>").to_path_buf()),
            ),
        })
    }
}

#[cfg(feature = "llm-executor")]
#[async_trait::async_trait]
impl AffinityResolver for AgentsYamlAffinityResolver {
    async fn resolve(&self, affinity: &str) -> Result<String, ExecutorError> {
        // Both the async runtime path AND the SYNC load-time cost-doctor
        // closure (built in main.rs) MUST format the resolved model the
        // same way, so the resolution lives in one shared, sync function.
        resolve_affinity_to_model(&self.resolver, affinity).ok_or_else(|| {
            ExecutorError::Permanent(format!(
                "LLM executor: affinity `{affinity}` could not be resolved against models.yaml"
            ))
        })
    }
}

/// Resolve an `affinity:` string to a concrete `"provider:model-id"`
/// against a loaded [`Resolver`], or `None` if it doesn't parse / walk /
/// bind. The single source of truth for affinity → model: the runtime
/// [`AffinityResolver`] impl AND the load-time cost-doctor SYNC closure
/// (built in `main.rs::collect_diagnostics`) both call through here so
/// they can never drift on the provider-prefix mapping or head-binding
/// selection. The `walk` is synchronous, so this is callable from the
/// sync `doctor_check` path without forcing it async.
pub fn resolve_affinity_to_model(resolver: &Resolver, affinity: &str) -> Option<String> {
    // Open activity key wins: an `activity:` chain in models.yaml keyed by any
    // string (e.g. `review`, `marketing-copy`) — head binding here.
    if let Some(bindings) = resolver.file().activity.get(affinity) {
        let b = bindings.first()?;
        return Some(format!("{}:{}", provider_prefix(&b.provider), b.model));
    }
    let delegate = ModelRef::parse(affinity).ok()?;
    let (bindings, _level) = resolver.walk(&delegate).ok()?;
    // The primary (index-0) binding is the concrete model the executor
    // spawns against. Per-list Chain-of-Responsibility fall-through is the
    // resolver's concern when the *agent* runtime probes auth; the LLM
    // executor consumes a single concrete model string, so we take the head.
    let binding = bindings.first()?;
    Some(format!(
        "{}:{}",
        provider_prefix(&binding.provider),
        binding.model
    ))
}

/// Resolve an `affinity:` string to an **ordered** list of
/// `"provider:model-id"` strings — the full chain the `Resolver::walk`
/// returns, with every binding mapped to its canonical prefix, cheapest-
/// effective first.
///
/// Returns an **empty `Vec`** when the affinity doesn't parse or the walk
/// fails, mirroring [`resolve_affinity_to_model`]'s `None` handling but as
/// a vec so callers can cheaply check `chain.is_empty()`.
pub fn resolve_affinity_to_chain(resolver: &Resolver, affinity: &str) -> Vec<String> {
    // Open activity key wins: a full `activity:` chain keyed by any string.
    if let Some(bindings) = resolver.file().activity.get(affinity) {
        return bindings
            .iter()
            .map(|b| format!("{}:{}", provider_prefix(&b.provider), b.model))
            .collect();
    }
    let Ok(delegate) = ModelRef::parse(affinity) else {
        return Vec::new();
    };
    let Ok((bindings, _level)) = resolver.walk(&delegate) else {
        return Vec::new();
    };
    bindings
        .iter()
        .map(|b| format!("{}:{}", provider_prefix(&b.provider), b.model))
        .collect()
}

/// Map a config [`Provider`] to the prefix the `LlmExecutor`'s
/// `DefaultProviderFactory` matches on.
///
/// `display_name()` now returns the canonical catalog slug (which equals the
/// aether parser token / factory key), so this is a straight pass-through.
/// Custom has no factory arm — emitting `"custom"` lets the factory reject with
/// its clear "provider not wired" message, which is the acceptable failure mode.
/// Guarded by the `google_binding_maps_to_gemini_prefix` test in
/// `tests/affinity_resolver.rs`.
fn provider_prefix(p: &Provider) -> &'static str {
    p.display_name()
}
