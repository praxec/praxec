//! Build a `Vec<DiscoveryItem>` from a parsed gateway config, and from those
//! items the [`DiscoveryIndex`] that config actually calls for —
//! [`build_discovery_index`] is the single seam BOTH gateway startup and gateway
//! hot-reload construct their index through.
//!
//! Honors the `discovery.include` config knob (`["proxy", "workflows",
//! "connections"]` by default for proxy + workflows, capped to those listed).

use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use serde_json::{Value, json};

use crate::audit::{AuditEvent, AuditSink};
use crate::discovery::{
    DiscoveryIndex, DiscoveryItem, DiscoveryKind, DiscoveryLink, InMemoryDiscoveryIndex,
    SemanticDiscoveryIndex,
};
use crate::embeddings::EmbeddingProvider;
use crate::proxy_workflow::DEFAULT_PROXY_WORKFLOW_ID;
use crate::registry_v3::Registry;
use crate::tool_descriptor::ToolDescriptor;

/// The set of tokens accepted in `discovery.include`. A token outside this set
/// is almost always a typo (e.g. `workflow` for `workflows`) which would
/// silently drop a whole category from the index — so we reject it rather than
/// ignore it (CMP-031).
const KNOWN_INCLUDE_TOKENS: &[&str] = &["proxy", "workflows", "connections"];

pub fn index_from_config(config: &Value) -> anyhow::Result<Vec<DiscoveryItem>> {
    let include = config
        .pointer("/discovery/include")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["proxy".into(), "workflows".into()]);

    // CMP-031 — validate include tokens against the known set. An unrecognized
    // token yields a partial/empty index that looks like a silent config bug;
    // fail fast with a named error instead.
    for token in &include {
        if !KNOWN_INCLUDE_TOKENS.contains(&token.as_str()) {
            bail!(
                "INVALID_DISCOVERY_INCLUDE: unknown discovery.include token '{token}'; \
                 supported: {}",
                KNOWN_INCLUDE_TOKENS.join(" | ")
            );
        }
    }

    let mut items = Vec::new();

    if include.iter().any(|s| s == "workflows") {
        if let Some(map) = config.pointer("/workflows").and_then(Value::as_object) {
            for (id, def) in map {
                items.push(workflow_item(id, def));
            }
        }
    }

    if include.iter().any(|s| s == "proxy") {
        if let Some(arr) = config.pointer("/proxy/expose").and_then(Value::as_array) {
            for exposure in arr {
                if let Some(item) = capability_item(exposure) {
                    items.push(item);
                }
            }
        }
    }

    if include.iter().any(|s| s == "connections") {
        if let Some(map) = config.pointer("/connections").and_then(Value::as_object) {
            for (name, conn) in map {
                items.push(connection_item(name, conn));
            }
        }
    }

    // Skills are always indexed when present — they have no opt-out switch in
    // `discovery.include` because they exist only when the author declares a
    // `skills:` block, which is itself the opt-in (SPEC v2 §5.3).
    if let Some(skills) = config.pointer("/skills").and_then(Value::as_object) {
        for (subject, entry) in skills {
            items.push(guidance_item(subject, entry));
        }
    }

    // SPEC §22 — scripts are always indexed when present, same opt-in
    // reasoning as skills. The DiscoveryKind::Script variant keeps them
    // distinct from guidance in search results (and lets gateway.describe
    // route correctly based on kind).
    if let Some(scripts) = config.pointer("/scripts").and_then(Value::as_object) {
        for (subject, entry) in scripts {
            items.push(script_item(subject, entry));
        }
    }

    Ok(items)
}

// ---------------------------------------------------------------------------
// The index seam — one function, both lifecycles.
//
// Semantic embeddings used to be built ONLY at gateway startup, while the
// hot-reload rebuild constructed a lexical `InMemoryDiscoveryIndex` — so every
// config/pack reload silently, permanently downgraded discovery to lexical.
// Two construction sites drifted apart; there is now exactly one, and both
// startup and reload call it. Reload therefore RE-EMBEDS, and a degrade is
// never sticky: the next reload with a healthy embedder restores semantics.
// ---------------------------------------------------------------------------

/// Audit event emitted when an index that should have been semantic came out
/// lexical. Degrading is allowed (a dead embedder must not take discovery down
/// with it); degrading *quietly* is the defect — that is what made "embeddings
/// are on" stop being true without anyone noticing.
pub const DISCOVERY_INDEX_DEGRADED: &str = "discovery.index_degraded";

/// Whole-of-rebuild budget: health probe + embedding every item, together.
///
/// The reload runs on the request path (SIGHUP, the in-band `reload` command, and
/// the lazy staleness recheck all await it), so an embedder that is merely *slow*
/// must not hold a request open indefinitely. [`HttpEmbedder`]'s own timeouts bound
/// each individual call, but nothing bounds the pass — and an arbitrary
/// [`EmbeddingProvider`] impl may bound nothing at all. Exceeding this budget is a
/// loud degrade to lexical, recoverable on the next reload, never a wedge.
///
/// [`HttpEmbedder`]: crate::embeddings::HttpEmbedder
pub const SEMANTIC_REBUILD_BUDGET: Duration = Duration::from_secs(60);

/// Build the discovery index this config calls for: a [`SemanticDiscoveryIndex`]
/// when a live embedder is registered AND proves healthy, a lexical
/// [`InMemoryDiscoveryIndex`] otherwise.
///
/// `registry` is the loaded D4b [`Registry`] (D6), or `None` when the operator
/// configured none. Its tool descriptors join the SAME catalog the config's
/// workflows/capabilities live in, so `praxec.query {kind: "tool"}` answers from
/// one index through one scorer — semantic *and* lexical, because a registry is
/// searchable whether or not embeddings are on. This is the only place tools
/// enter discovery: one index-construction seam, as with D3.
///
/// The two failure modes are deliberately distinguished:
///
/// - **No embedder** (`NoopEmbedder`): lexical is the *configured* answer, not a
///   degrade. Silent, because nothing went wrong.
/// - **Embedder present but unusable** (unhealthy, timed out, or every item
///   failed to embed): lexical is a *degrade* — emitted at WARN and audited as
///   [`DISCOVERY_INDEX_DEGRADED`], so the operator sees the semantic surface is
///   off and why. The (re)load itself always completes.
///
/// A config-level fault (unknown `discovery.include` token) still fails fast:
/// that's a defect to fix, not a runtime condition to degrade around. (A broken
/// *registry* fails even earlier — the caller never gets a `Registry` to pass.)
pub async fn build_discovery_index(
    config: &Value,
    registry: Option<&Registry>,
    embedder: &Arc<dyn EmbeddingProvider>,
    audit: &Arc<dyn AuditSink>,
) -> anyhow::Result<Arc<dyn DiscoveryIndex>> {
    let items = index_from_config(config)?;
    // Owned: `build_with_tools` stamps each descriptor's `embedding` slot with
    // the vector it was indexed by, so it needs `&mut`.
    let mut tools: Vec<ToolDescriptor> =
        registry.map(Registry::tool_descriptors).unwrap_or_default();

    if embedder.backend_name() == NOOP_BACKEND {
        return Ok(Arc::new(InMemoryDiscoveryIndex::new(catalog(
            &items, &tools,
        ))));
    }

    // One deadline across both awaits below, so the worst case is the budget —
    // not the budget once per phase.
    let deadline = tokio::time::Instant::now() + SEMANTIC_REBUILD_BUDGET;

    // Prove the backend answers NOW (the D2 health contract). Doing this before
    // the N item-embeds means a dead endpoint costs one round-trip, not N.
    match tokio::time::timeout_at(deadline, embedder.health_check()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let items = catalog(&items, &tools);
            return Ok(degrade("health_check_failed", e.to_string(), items, embedder, audit).await);
        }
        Err(_) => {
            let items = catalog(&items, &tools);
            return Ok(degrade(
                "health_check_timeout",
                format!("embedder did not answer within {SEMANTIC_REBUILD_BUDGET:?}"),
                items,
                embedder,
                audit,
            )
            .await);
        }
    }

    // `items` / `tools` are kept intact for the lexical fallback: on timeout the
    // build future (and the items moved into it) is dropped. Awaited into a
    // binding first so the `&mut tools` borrow ends before the arms below reach
    // for `&tools` again.
    let catalog_len = items.len() + tools.len();
    let built = tokio::time::timeout_at(
        deadline,
        SemanticDiscoveryIndex::build_with_tools(items.clone(), &mut tools, embedder.clone()),
    )
    .await;
    let semantic = match built {
        Ok(Ok(index)) => index,
        Ok(Err(e)) => {
            let items = catalog(&items, &tools);
            return Ok(degrade("embed_failed", e.to_string(), items, embedder, audit).await);
        }
        Err(_) => {
            let items = catalog(&items, &tools);
            return Ok(degrade(
                "embed_timeout",
                format!("embedding {catalog_len} items exceeded {SEMANTIC_REBUILD_BUDGET:?}"),
                items,
                embedder,
                audit,
            )
            .await);
        }
    };

    // The backend passed its probe and then died: `build` tolerates per-item
    // failures, so it hands back an index with zero vectors, which ranks purely
    // lexically while presenting as semantic. That is the silent downgrade under
    // another name — degrade explicitly instead.
    if semantic.embedded_count() == 0 && catalog_len > 0 {
        let items = catalog(&items, &tools);
        return Ok(degrade(
            "all_items_failed_to_embed",
            format!("{catalog_len} items, 0 embedded"),
            items,
            embedder,
            audit,
        )
        .await);
    }

    tracing::info!(
        backend = embedder.backend_name(),
        items = catalog_len,
        tools = tools.len(),
        embedded = semantic.embedded_count(),
        "semantic discovery index built"
    );
    Ok(Arc::new(semantic))
}

/// The full lexical catalog — the config's items plus the registry's tools,
/// projected through the SAME [`DiscoveryItem::from_tool_descriptor`] the
/// semantic builder uses. Every non-semantic path (no embedder, and each
/// degrade) goes through here, so a degraded index loses the *vectors*, never a
/// whole category of item.
fn catalog(items: &[DiscoveryItem], tools: &[ToolDescriptor]) -> Vec<DiscoveryItem> {
    let mut all = items.to_vec();
    all.extend(tools.iter().map(DiscoveryItem::from_tool_descriptor));
    all
}

/// The `backend_name()` of the disabled embedder — the one value that means
/// "lexical was asked for", as opposed to "semantic was asked for and failed".
const NOOP_BACKEND: &str = "noop";

/// Fall back to lexical, loudly: WARN for the operator's console, an audit event
/// for anything watching the stream. Returns the lexical index so the caller's
/// (re)load completes.
async fn degrade(
    reason: &'static str,
    detail: String,
    items: Vec<DiscoveryItem>,
    embedder: &Arc<dyn EmbeddingProvider>,
    audit: &Arc<dyn AuditSink>,
) -> Arc<dyn DiscoveryIndex> {
    tracing::warn!(
        backend = embedder.backend_name(),
        reason,
        detail = %detail,
        items = items.len(),
        "discovery index DEGRADED to lexical: an embedder is registered but unusable — \
         semantic search is OFF until a reload finds it healthy again"
    );
    let _ = audit
        .record(
            AuditEvent::new(DISCOVERY_INDEX_DEGRADED).with_payload(json!({
                "backend": embedder.backend_name(),
                "reason": reason,
                "detail": detail,
                "items": items.len(),
            })),
        )
        .await;
    Arc::new(InMemoryDiscoveryIndex::new(items))
}

/// SPEC §22 — convert a `scripts:` entry into a DiscoveryItem. Mirror of
/// [`guidance_item`] with two differences: kind is `Script` and the body
/// may come from either inline `body:` or external `uri:` (already
/// materialized into a `body` field by [`stamp_scripts_library`] at load
/// time — so by the time the indexer runs, every script has an inline
/// body in the snapshot).
///
/// Note: this indexer reads from the TOP-LEVEL `scripts:` block (which
/// still carries the original inline vs uri shape, NOT from the stamped
/// `_scriptsLibrary` on workflow snapshots). For uri-sourced scripts, the
/// inline body isn't present in the top-level entry — only in the
/// stamped library. We surface the `verb` + `source` regardless; `body`
/// is only populated when inline, and `gateway.describe(subject,
/// workflowId)` is the path to get a uri-sourced body (it reads from the
/// instance's stamped library, mirroring how guidance bodies are
/// resolved).
fn script_item(subject: &str, entry: &Value) -> DiscoveryItem {
    // CMP-overflow note: `verb` is `unwrap_or_default()` (empty string) rather
    // than erroring. This is safe because the indexer always runs on a config
    // that has already passed schema validation at load time (the `scripts:`
    // block requires `verb`); a missing `verb` here would be a loader bug, not
    // user input. An empty verb degrades the description string only, never the
    // index structure.
    let verb = entry
        .get("verb")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let body = entry
        .get("body")
        .and_then(Value::as_str)
        .map(str::to_string);
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("config")
        .to_string();
    DiscoveryItem {
        id: subject.to_string(),
        kind: DiscoveryKind::Script,
        title: subject.to_string(),
        description: format!("Curated script '{subject}' (verb: {verb})."),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: format!("{subject} {verb}"),
        links: vec![],
        verb: Some(verb),
        body,
        source: Some(source),
        structural_fingerprint: None,
    }
}

fn guidance_item(subject: &str, entry: &Value) -> DiscoveryItem {
    // CMP-overflow note: same as `script_item` — `verb` defaults to empty only
    // because this runs post config-load validation (the `skills:` block
    // requires `verb`). A missing value would indicate a loader bug, and the
    // empty fallback affects the description string only, not index structure.
    let verb = entry
        .get("verb")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let body = entry
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // SPEC §5.3 — surface the fragment's provenance string. Default is
    // `config` (declared inline). Git-ingested fragments override this
    // when the config loader stamps a `source: "git+https://…"` value
    // onto the entry.
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("config")
        .to_string();
    DiscoveryItem {
        id: subject.to_string(),
        kind: DiscoveryKind::Guidance,
        title: subject.to_string(),
        description: format!("Guidance fragment '{subject}' (verb: {verb})."),
        tags: vec![],
        examples: vec![],
        aliases: vec![],
        text: format!("{subject} {verb}"),
        links: vec![],
        verb: Some(verb),
        body: Some(body),
        source: Some(source),
        structural_fingerprint: None,
    }
}

fn workflow_item(id: &str, def: &Value) -> DiscoveryItem {
    let title = def
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_string();
    let description = def
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut tags = string_array(def.get("tags"));
    let examples = string_array(def.get("examples"));
    let aliases = string_array(def.get("aliases"));

    // Thread the `process`/`taskClass` tag into the catalog so a flow is
    // filterable by task-class (via the existing tag search) — read back via
    // `DiscoveryItem::task_class`. Validity (non-empty, ≥1 outcome) is enforced
    // by `validate::v_process_metadata`; here we only index what's present.
    let process = def
        .get("process")
        .or_else(|| def.get("taskClass"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut text = Vec::<String>::new();
    if let Some(p) = process {
        tags.push(format!("{}{p}", crate::discovery::PROCESS_TAG_PREFIX));
        text.push(p.to_string());
    }
    if let Some(states) = def.pointer("/states").and_then(Value::as_object) {
        for (state_name, state) in states {
            text.push(state_name.clone());
            if let Some(d) = state.get("description").and_then(Value::as_str) {
                text.push(d.to_string());
            }
            if let Some(g) = state.get("goal").and_then(Value::as_str) {
                text.push(g.to_string());
            }
            if let Some(g) = state.get("guidance").and_then(Value::as_str) {
                text.push(g.to_string());
            }
            if let Some(ts) = state.pointer("/transitions").and_then(Value::as_object) {
                for (tname, t) in ts {
                    text.push(tname.clone());
                    if let Some(t_title) = t.get("title").and_then(Value::as_str) {
                        text.push(t_title.to_string());
                    }
                    if let Some(t_desc) = t.get("description").and_then(Value::as_str) {
                        text.push(t_desc.to_string());
                    }
                }
            }
        }
    }

    let input_schema = def.get("inputSchema").cloned();

    let mut start_args = serde_json::Map::new();
    start_args.insert("definitionId".into(), Value::String(id.to_string()));
    start_args.insert("input".into(), Value::Object(serde_json::Map::new()));

    DiscoveryItem {
        id: id.to_string(),
        kind: DiscoveryKind::Workflow,
        title,
        description,
        tags,
        examples,
        aliases,
        text: text.join(" "),
        links: vec![DiscoveryLink {
            rel: "start".into(),
            title: Some(format!("Start workflow '{id}'")),
            description: None,
            method: "praxec.command".into(),
            args: Value::Object(start_args),
            input_schema,
        }],
        verb: None,
        body: None,
        source: None,
        // v0.0.18 mechanism #2 — every cataloged workflow carries the canonical
        // fingerprint of its graph. Computed here, at the one seam both startup
        // and hot-reload build the catalog through, so a reloaded/newly-minted
        // flow is fingerprinted the moment it enters the catalog.
        structural_fingerprint: Some(crate::structural_fingerprint::fingerprint(def)),
    }
}

fn capability_item(exposure: &Value) -> Option<DiscoveryItem> {
    let name = exposure.get("name").and_then(Value::as_str)?.to_string();
    let title = exposure
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_string();
    let description = exposure
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tags = string_array(exposure.get("tags"));
    let examples = string_array(exposure.get("examples"));
    let aliases = string_array(exposure.get("aliases"));
    let input_schema = exposure.get("inputSchema").cloned();

    let mut start_args = serde_json::Map::new();
    start_args.insert(
        "definitionId".into(),
        Value::String(DEFAULT_PROXY_WORKFLOW_ID.to_string()),
    );
    start_args.insert("input".into(), Value::Object(serde_json::Map::new()));

    Some(DiscoveryItem {
        id: name.clone(),
        kind: DiscoveryKind::Capability,
        title,
        description,
        tags,
        examples,
        aliases,
        text: name.clone(),
        links: vec![DiscoveryLink {
            rel: "start_proxy_session".into(),
            title: Some("Start proxy_default to use this capability".into()),
            description: Some(format!(
                "After starting, submit transition '{name}' from the 'ready' state."
            )),
            method: "praxec.command".into(),
            args: Value::Object(start_args),
            input_schema,
        }],
        verb: None,
        body: None,
        source: None,
        structural_fingerprint: None,
    })
}

fn connection_item(name: &str, conn: &Value) -> DiscoveryItem {
    let kind = conn
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    DiscoveryItem {
        id: format!("connection:{name}"),
        kind: DiscoveryKind::Connection,
        title: name.to_string(),
        description: format!("Configured {kind} connection '{name}'."),
        tags: vec![kind.clone()],
        examples: vec![],
        aliases: vec![],
        text: format!("{name} {kind}"),
        links: vec![],
        verb: None,
        body: None,
        source: None,
        structural_fingerprint: None,
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod index_seam_tests {
    //! D3 (re-embed-on-reload). Every case here runs the SAME call the gateway's
    //! hot-reload makes — `build_discovery_index` over a freshly-loaded config —
    //! because the defect was that reload built its index somewhere else.

    use super::*;
    use crate::audit::MemoryAuditSink;
    use crate::discovery::SearchRequest;
    use crate::embeddings::{EmbeddingError, EmbeddingProvider};
    use async_trait::async_trait;

    /// Deterministic 2-axis fake (mirrors `discovery::semantic_tests`): axis 0
    /// fires on speed/cache words, axis 1 on auth words. So the query "speed"
    /// matches an item that only ever says "cache" — a question ONLY a semantic
    /// index can answer, which is how these tests tell the two indexes apart
    /// without reaching for the concrete type.
    struct HealthyEmbedder;

    #[async_trait]
    impl EmbeddingProvider for HealthyEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            let t = text.to_lowercase();
            let speed = ["speed", "fast", "cache", "latency"]
                .iter()
                .any(|w| t.contains(w)) as i32 as f32;
            let auth = ["auth", "login", "identity"].iter().any(|w| t.contains(w)) as i32 as f32;
            Ok(vec![speed, auth])
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Ok(())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "healthy-fake"
        }
    }

    /// The endpoint is down: the probe says so, and every embed would too.
    struct DeadEmbedder;

    #[async_trait]
    impl EmbeddingProvider for DeadEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
            Err(EmbeddingError::BackendFailed("connection refused".into()))
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Err(EmbeddingError::HealthCheckFailed(
                "connection refused".into(),
            ))
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "dead-fake"
        }
    }

    /// Passes its probe, then dies — the "fails mid-reload" case. Without the
    /// zero-embedding check this yields a `SemanticDiscoveryIndex` holding no
    /// vectors at all: lexical behaviour wearing a semantic label, silently.
    struct DiesAfterProbeEmbedder;

    #[async_trait]
    impl EmbeddingProvider for DiesAfterProbeEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
            if text == crate::embeddings::HEALTH_PROBE_TEXT {
                return Ok(vec![1.0, 0.0]);
            }
            Err(EmbeddingError::BackendFailed("backend died".into()))
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            Ok(())
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "dies-after-probe"
        }
    }

    /// Never answers. The wedge the reload path must be immune to.
    struct StallingEmbedder;

    #[async_trait]
    impl EmbeddingProvider for StallingEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbeddingError> {
            tokio::time::sleep(Duration::from_secs(86_400)).await;
            unreachable!("the rebuild budget must expire first")
        }
        async fn health_check(&self) -> Result<(), EmbeddingError> {
            tokio::time::sleep(Duration::from_secs(86_400)).await;
            unreachable!("the rebuild budget must expire first")
        }
        fn dimensions(&self) -> usize {
            2
        }
        fn backend_name(&self) -> &'static str {
            "stalling"
        }
    }

    fn noop() -> Arc<dyn EmbeddingProvider> {
        Arc::new(crate::embeddings::NoopEmbedder)
    }

    /// Two workflows with no keyword overlap with the query "speed".
    fn config(cache_description: &str) -> Value {
        json!({
            "workflows": {
                "optimize": { "title": "Optimize", "description": cache_description },
                "login": { "title": "Login", "description": "auth and identity" },
            }
        })
    }

    async fn ids_for(index: &Arc<dyn DiscoveryIndex>, query: &str) -> Vec<String> {
        index
            .search(SearchRequest {
                query: query.into(),
                kind: None,
                limit: 10,
            })
            .await
            .expect("search")
            .into_iter()
            .map(|h| h.item.id)
            .collect()
    }

    fn degrade_reasons(audit: &MemoryAuditSink) -> Vec<String> {
        audit
            .snapshot()
            .into_iter()
            .filter(|e| e.event_type == DISCOVERY_INDEX_DEGRADED)
            .map(|e| e.payload["reason"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// D3-T1 — the regression test for the actual defect (`gateway.rs:1424`
    /// rebuilt a lexical index). With a healthy embedder the rebuilt index
    /// answers a query no lexical index can.
    #[tokio::test]
    async fn rebuild_with_healthy_embedder_is_semantic_not_lexical() {
        let audit_sink = MemoryAuditSink::new();
        let audit: Arc<dyn AuditSink> = Arc::new(audit_sink.clone());
        let cfg = config("add a cache layer to responses");

        // Baseline: the lexical index the reload path used to build finds
        // nothing for "speed" — no item contains the word.
        let lexical: Arc<dyn DiscoveryIndex> =
            Arc::new(InMemoryDiscoveryIndex::from_config(&cfg).unwrap());
        assert!(
            ids_for(&lexical, "speed").await.is_empty(),
            "precondition: 'speed' is answerable only semantically"
        );

        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(HealthyEmbedder);
        let index = build_discovery_index(&cfg, None, &embedder, &audit)
            .await
            .expect("rebuild completes");

        assert_eq!(
            ids_for(&index, "speed").await.first().map(String::as_str),
            Some("optimize"),
            "a reload with a live embedder must produce a SEMANTIC index"
        );
        assert!(
            degrade_reasons(&audit_sink).is_empty(),
            "a healthy rebuild is not a degrade"
        );
    }

    /// D3-T2 — no embedder configured: lexical, exactly as before, and silent.
    /// Lexical-because-unconfigured is not a degrade and must not be shouted
    /// about (or the signal that matters gets drowned).
    #[tokio::test]
    async fn no_embedder_configured_stays_lexical_and_silent() {
        let audit_sink = MemoryAuditSink::new();
        let audit: Arc<dyn AuditSink> = Arc::new(audit_sink.clone());
        let cfg = config("add a cache layer to responses");

        let index = build_discovery_index(&cfg, None, &noop(), &audit)
            .await
            .expect("rebuild completes");

        assert!(
            ids_for(&index, "speed").await.is_empty(),
            "no embedder → lexical behaviour, unchanged"
        );
        assert_eq!(
            ids_for(&index, "cache").await,
            vec!["optimize"],
            "lexical search still works"
        );
        assert!(
            audit_sink.snapshot().is_empty(),
            "nothing degraded — nothing to report"
        );
    }

    /// D3-T4 — the embedder is down at reload time: the reload COMPLETES, the
    /// index degrades to lexical, and the degrade is audited with a reason. The
    /// assertion is on the observable signal, not merely on "it didn't crash".
    #[tokio::test]
    async fn dead_embedder_completes_the_rebuild_and_degrades_loudly() {
        let audit_sink = MemoryAuditSink::new();
        let audit: Arc<dyn AuditSink> = Arc::new(audit_sink.clone());
        let cfg = config("add a cache layer to responses");
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(DeadEmbedder);

        let index = build_discovery_index(&cfg, None, &embedder, &audit)
            .await
            .expect("a dead embedder must never fail the reload");

        assert!(
            ids_for(&index, "speed").await.is_empty(),
            "degraded to lexical"
        );
        assert_eq!(
            ids_for(&index, "cache").await,
            vec!["optimize"],
            "discovery still answers lexically — the degrade is not an outage"
        );
        assert_eq!(degrade_reasons(&audit_sink), vec!["health_check_failed"]);
        let event = audit_sink
            .snapshot()
            .into_iter()
            .find(|e| e.event_type == DISCOVERY_INDEX_DEGRADED)
            .expect("degrade event");
        assert_eq!(event.payload["backend"], "dead-fake");
        assert!(
            event.payload["detail"]
                .as_str()
                .unwrap()
                .contains("connection refused"),
            "the audit event names the underlying reason: {}",
            event.payload
        );
    }

    /// D3-T4 (second failure mode) — probe passes, backend dies before the item
    /// embeds land. An index with zero vectors ranks lexically; shipping it as
    /// "semantic" would be the silent downgrade all over again.
    #[tokio::test]
    async fn backend_dying_after_the_probe_still_degrades_loudly() {
        let audit_sink = MemoryAuditSink::new();
        let audit: Arc<dyn AuditSink> = Arc::new(audit_sink.clone());
        let cfg = config("add a cache layer to responses");
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(DiesAfterProbeEmbedder);

        let index = build_discovery_index(&cfg, None, &embedder, &audit)
            .await
            .expect("rebuild completes");

        assert_eq!(
            ids_for(&index, "cache").await,
            vec!["optimize"],
            "still answers lexically"
        );
        assert_eq!(
            degrade_reasons(&audit_sink),
            vec!["all_items_failed_to_embed"]
        );
    }

    /// A stalled embedder must not hold the reload open. The clock is paused, so
    /// the budget expires without the test sleeping for real: the rebuild returns
    /// (lexical + audited), it does not hang.
    #[tokio::test(start_paused = true)]
    async fn stalled_embedder_cannot_wedge_the_rebuild() {
        let audit_sink = MemoryAuditSink::new();
        let audit: Arc<dyn AuditSink> = Arc::new(audit_sink.clone());
        let cfg = config("add a cache layer to responses");
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(StallingEmbedder);

        let index = build_discovery_index(&cfg, None, &embedder, &audit)
            .await
            .expect("the rebuild completes despite the stall");

        assert_eq!(ids_for(&index, "cache").await, vec!["optimize"]);
        assert_eq!(degrade_reasons(&audit_sink), vec!["health_check_timeout"]);
    }

    /// D3 requirement 3 — a degrade is NOT sticky. The same gateway, reloading
    /// again once the embedder is back, gets its semantic index back. (Before
    /// this deliverable the downgrade was permanent until restart.)
    #[tokio::test]
    async fn degrade_is_not_sticky_the_next_reload_restores_semantics() {
        let audit_sink = MemoryAuditSink::new();
        let audit: Arc<dyn AuditSink> = Arc::new(audit_sink.clone());
        let cfg = config("add a cache layer to responses");

        let dead: Arc<dyn EmbeddingProvider> = Arc::new(DeadEmbedder);
        let degraded = build_discovery_index(&cfg, None, &dead, &audit)
            .await
            .unwrap();
        assert!(ids_for(&degraded, "speed").await.is_empty());

        let healthy: Arc<dyn EmbeddingProvider> = Arc::new(HealthyEmbedder);
        let restored = build_discovery_index(&cfg, None, &healthy, &audit)
            .await
            .unwrap();
        assert_eq!(
            ids_for(&restored, "speed")
                .await
                .first()
                .map(String::as_str),
            Some("optimize"),
            "the next reload with a healthy embedder restores semantic search"
        );
    }

    /// D3-T3 — an item whose description changed is re-embedded against the NEW
    /// text. A stale vector carried over from the previous index would keep
    /// answering the old question; nothing is cached, so nothing can go stale.
    #[tokio::test]
    async fn changed_description_is_re_embedded_on_reload() {
        let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
        let embedder: Arc<dyn EmbeddingProvider> = Arc::new(HealthyEmbedder);

        // v1: `optimize` is about auth — semantically nothing to do with "speed".
        let v1 =
            build_discovery_index(&config("auth and identity checks"), None, &embedder, &audit)
                .await
                .unwrap();
        assert!(
            ids_for(&v1, "speed").await.is_empty(),
            "v1 has no speed-adjacent item"
        );

        // v2: the operator edits the description to talk about caching.
        let v2 = build_discovery_index(&config("add a cache layer"), None, &embedder, &audit)
            .await
            .unwrap();
        assert_eq!(
            ids_for(&v2, "speed").await.first().map(String::as_str),
            Some("optimize"),
            "the edited description was embedded fresh, not reused from v1"
        );
    }
}
