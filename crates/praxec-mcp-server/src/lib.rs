// T26 — restriction-category lint on production code only. See
// praxec-core/src/lib.rs for the rationale.
#![cfg_attr(not(test), warn(clippy::unwrap_used))]

//! MCP server tool surface for praxec.
//!
//! SPEC §32 — the public MCP surface is exactly **two tools** (`praxec.query`
//! and `praxec.command`), stable across configs by design (README invariant
//! 9). All workflow and discovery operations are reached by varying the args,
//! not the tool name.
//!
//! Module layout:
//! - `args` — sparse argument structs (`QueryArgs`, `CommandArgs`) + JSON
//!   Schema helpers.
//! - `tools` — two-tool-list construction + `parse_kind` + `instructions`.
//! - `handlers` — per-operation handler bodies (sibling `impl PraxecServer`)
//!   plus shape-routers `dispatch_query` / `dispatch_command`.

pub mod args;
pub mod degraded;
mod elicit;
mod handlers;
pub mod progress;
mod tools;

pub use degraded::{DegradedServer, HealthReport};

use handlers::{run_id_already_running, subject_needs_definition};

use std::sync::Arc;

use praxec_core::audit::AuditEvent;
use praxec_core::discovery::{DiscoveryIndex, InMemoryDiscoveryIndex};
use praxec_core::embeddings::{EmbeddingProvider, NoopEmbedder};
use praxec_core::model::Principal;
use praxec_core::runtime::WorkflowRuntime;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CreateElicitationRequestParams, ElicitationAction,
    Implementation, InitializeRequestParams, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, Peer, RequestContext, RoleServer};
use serde_json::{Value, json};

pub use progress::{ProgressPeer, progress_bridge};
pub use tools::tool_definitions;

/// SPEC §32 — read tool. Args dispatched by present-field shape via
/// `handlers::dispatch_query`. See SPEC §32 for the full dispatch table.
pub const TOOL_QUERY: &str = "praxec.query";

/// SPEC §32 — write tool. Args dispatched by present-field shape via
/// `handlers::dispatch_command`. See SPEC §32 for the full dispatch table.
pub const TOOL_COMMAND: &str = "praxec.command";

/// SPEC §32 — the public MCP surface is exactly two tools, stable
/// across configs by design (README invariant 9). All workflow and
/// discovery operations are reached by varying the args, not the tool
/// name.
pub const STABLE_TOOL_NAMES: &[&str] = &[TOOL_QUERY, TOOL_COMMAND];

/// P6 — in-band config reload hook. The serve path injects this (built in the
/// gateway binary, which owns `build_hot_components`/`apply_overlays`) so a
/// `praxec.command { reload: true }` fires the SAME gated rebuild+swap as
/// SIGHUP — no third MCP tool, the two-tool surface is preserved. Returns the
/// reload outcome as JSON (`{status: reloaded|rejected|failed, ...}`).
pub type ReloadHook = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>> + Send + Sync,
>;

/// P6b — lazy config-staleness recheck hook. The serve path injects a closure
/// that runs a TTL-throttled mtime probe (praxec-core's `StalenessTracker`)
/// and, when the config file set changed on disk, fires the SAME gated reload
/// as SIGHUP / `praxec.command { reload: true }`. Invoked at the top of every
/// `call_tool` — within the TTL window it returns immediately without touching
/// the filesystem, so per-request cost is a mutex lock + `Instant` compare.
/// This is the WSL-safe polling backup to fs-event watchers (which don't fire
/// reliably there); `None` (CLI / one-shot / tests) disables it.
pub type StalenessHook = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

#[derive(Clone)]
pub struct PraxecServer {
    pub(crate) runtime: WorkflowRuntime,
    pub(crate) discovery: Arc<dyn DiscoveryIndex>,
    /// D6 — the loaded `praxec.packs/v3` registry, hot-swappable with the
    /// discovery index it also feeds. `handle_search` reads it for the selector's
    /// topology term. Default: an EMPTY swappable (no registry configured), which
    /// ranks with a uniform zero topology term — the pre-D6 behavior, unchanged.
    pub(crate) registry: Arc<praxec_core::hot_reload::SwappableRegistry>,
    server_name: String,
    server_version: String,
    /// SPEC §5.9 — optional store that records `gateway.describe` calls per
    /// workflow + subject, consumed by the `guidance_acknowledged` guard.
    /// When `None`, describes still emit audit records but the guard cannot
    /// be satisfied (returns false).
    pub(crate) ack_store: Option<Arc<dyn praxec_core::ports::GuidanceAcknowledgmentStore>>,
    /// SPEC §17.6 — when true, the `gateway.skills.search` tool is
    /// advertised in `list_tools`. Default false; authoring-time only.
    skills_search_enabled: bool,
    /// SPEC §22 — when true, `praxec.query` with `kind: "script"` is
    /// enabled. Default false; authoring-time only. Same rationale
    /// as skills_search_enabled.
    scripts_search_enabled: bool,
    /// SPEC §32 — when true, the `praxec.command` dispatch accepts
    /// `subject: "lexicon:<term>"` + `definition` shape (lexicon writes
    /// via MCP). Default OFF: production runtimes typically curate lexicon
    /// via the CLI or out-of-band processes. Authoring builds opt in.
    lexicon_writes_enabled: bool,
    /// SPEC §22 — optional store that records `gateway.describe` calls
    /// for SCRIPT subjects per workflow, consumed by the
    /// `script_acknowledged` guard. When `None`, describes still emit
    /// audit records but the guard cannot be satisfied (returns false).
    pub(crate) script_ack_store: Option<Arc<dyn praxec_core::ports::ScriptAcknowledgmentStore>>,
    /// SPEC §30.5 — runtime overlay over the config-stamped lexicon.
    /// `gateway.lexicon.define` writes here; `search` / `lookup` read
    /// the union (overlay wins on collision). Survives only for the
    /// runtime's lifetime — operators persist by editing
    /// `praxec.yaml` and reloading.
    pub(crate) lexicon_overlay: Arc<std::sync::RwLock<std::collections::HashMap<String, Value>>>,
    /// SPEC §30 — the config-loaded lexicon block (the persistent base).
    /// Empty when no `lexicon:` block was declared in the config.
    /// `search` / `lookup` read `lexicon_base` ∪ `lexicon_overlay`;
    /// overlay wins on collision.
    pub(crate) lexicon_base: Arc<Value>,
    /// SPEC §30.5 durability — optional directory under which MCP-defined
    /// lexicon terms are persisted as `<term>.json`. When set, `define`
    /// writes the term to disk and the server loads any existing terms
    /// into the overlay at construction, so vocabulary survives restarts.
    /// `None` keeps the legacy in-memory-only behavior.
    pub(crate) lexicon_dir: Option<std::path::PathBuf>,
    /// SPEC §30.10.3 — runtime-mutable set of subject names that were
    /// detected as PENDING_DEFINITION placeholders at config-load time.
    /// Resolution handlers (link_as_alias, define_new, cancel) remove
    /// entries from this set when they resolve a subject. Cancel uses it
    /// to distinguish "is a placeholder" from "is a real entry"
    /// (SPEC §30.10.9).
    pub(crate) pending_subjects: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// SPEC §30.10.10 — optional Tier 3 embedding backend. Defaults to
    /// `NoopEmbedder` (disabled). Set via `with_embedder(...)`. When a
    /// non-noop backend is configured, `handle_lexicon_define` computes and
    /// stores the embedding vector on each written entry, and
    /// `rank_candidates_with_embedding` fires Tier 3 in the
    /// SUBJECT_NEEDS_DEFINITION candidate response.
    pub(crate) embedder: Arc<dyn EmbeddingProvider>,
    /// CMP-001 — the identity used for a request that carries no `_meta`
    /// principal claim. Sourced from the gateway config's `gateway.principal`
    /// block (`with_principal`); defaults to [`Principal::anonymous`], which
    /// fails closed (no roles, no permissions, so `actor: human` and
    /// permission-guarded transitions are rejected). A per-request `_meta`
    /// claim (set by the trusted embedding host) overrides this default.
    pub(crate) default_principal: Principal,
    /// CMP-001 — whether to honor a per-request `_meta` principal claim. Default
    /// `true` (the host channel is trusted). An operator who does NOT trust the
    /// `_meta` channel (e.g. the gateway is reachable by something other than a
    /// vetted embedding host) can set `gateway.trust_meta_principal: false` to
    /// IGNORE all `_meta` claims and run every caller as the configured
    /// `default_principal` — collapsing the identity surface to config only.
    pub(crate) trust_meta_principal: bool,
    /// #18 — PUSH observability. Shared slot holding the connected MCP peer,
    /// captured per `call_tool`. When set (wired via [`Self::with_progress_peer`]
    /// on the serve path), the bridged audit sink forwards each event to the
    /// client as a logging notification, so a long auto-drive streams progress
    /// live. Default-empty: no peer, no push (CLI / tests).
    pub(crate) progress_peer: ProgressPeer,
    /// P6 — optional in-band config reload hook (serve-mode only). When set,
    /// `praxec.command { reload: true }` invokes it to run the gated
    /// rebuild+swap. `None` on the CLI/one-shot/test paths (no live server to
    /// reload); the command then returns `RELOAD_UNAVAILABLE`.
    pub(crate) reload_hook: Option<ReloadHook>,
    /// P6b — optional lazy staleness recheck (serve-mode only). When set,
    /// `call_tool` awaits it before dispatching, so an operator's on-disk
    /// config edit is picked up by the next request after the TTL elapses —
    /// no manual reload, no fs-event watcher (unreliable on WSL). `None` on
    /// the CLI/one-shot/test paths.
    pub(crate) staleness_hook: Option<StalenessHook>,
    /// #14 — repair-only gate, a SWAPPABLE slot. Holds `Some` when the loaded
    /// pack is contract-DIRTY but still buildable: the gateway stays live, but
    /// the ONLY workflows it will START are the operator-declared repair
    /// surface. Every other `start` is refused with the precise contract
    /// diagnostics, so the consuming LLM fixes the contracts (through the
    /// repair surface) before the functional surface reopens. `None` ⇒ clean,
    /// full surface. A slot (not a plain field) so a hot reload can flip it:
    /// clean reload clears it, dirty reload sets it (Fork C).
    pub(crate) repair_gate: RepairGateSlot,
}

/// #14 — the shared, hot-swappable repair-gate slot. Cloned into the serve
/// reload closure so a reload can flip the live server's serving posture
/// without rebuilding it.
pub type RepairGateSlot = Arc<std::sync::RwLock<Option<Arc<RepairGate>>>>;

/// #14 — the repair-only serving posture (poka-yoke). When the loaded pack has
/// contract errors, the gateway must not let any NON-repair work begin; it may
/// only be repaired. This carries the two things every gated call needs: the
/// exact contract violations to surface, and the set of definition ids that
/// remain startable (the declared repair surface, Fork B).
#[derive(Debug, Clone)]
pub struct RepairGate {
    /// The contract violations that put the gateway in repair-only mode —
    /// surfaced verbatim on `home` and on every gated refusal so the consuming
    /// LLM has the precise information needed to fix them.
    pub diagnostics: Vec<String>,
    /// Definition ids the caller MAY still `start` while dirty — the operator's
    /// declared repair surface. Everything else is refused until clean.
    pub repair_ids: std::collections::HashSet<String>,
}

impl RepairGate {
    /// Whether `definition_id` is on the repair surface (startable while dirty).
    pub fn allows(&self, definition_id: &str) -> bool {
        self.repair_ids.contains(definition_id)
    }
}

/// CMP-001 — reverse-DNS `_meta` key under which the embedding host passes a
/// per-request identity claim `{ subject, roles, permissions }`. Read from the
/// MCP request `_meta` (host-controlled), never from the tool `arguments`
/// (agent-controlled).
pub const PRINCIPAL_META_KEY: &str = "io.praxec/principal";

impl PraxecServer {
    /// Build a server with a default empty in-memory discovery index. The
    /// gateway.* tools still work but return no items.
    pub fn new(runtime: WorkflowRuntime) -> Self {
        Self {
            runtime,
            discovery: Arc::new(InMemoryDiscoveryIndex::default()),
            registry: Arc::new(praxec_core::hot_reload::SwappableRegistry::new(None)),
            server_name: "praxec".to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
            ack_store: None,
            skills_search_enabled: false,
            scripts_search_enabled: false,
            lexicon_writes_enabled: false,
            script_ack_store: None,
            lexicon_overlay: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            lexicon_base: Arc::new(json!({})),
            lexicon_dir: None,
            pending_subjects: Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            embedder: Arc::new(NoopEmbedder),
            default_principal: Principal::anonymous(),
            trust_meta_principal: true,
            progress_peer: ProgressPeer::default(),
            reload_hook: None,
            staleness_hook: None,
            repair_gate: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// #14 — put the server in repair-only mode. When set, only the gate's
    /// declared repair surface may be `start`ed; every other start is refused
    /// with the contract diagnostics. Cleared (`None`) once contracts
    /// re-validate clean, reopening the full surface.
    pub fn with_repair_gate(self, gate: Option<RepairGate>) -> Self {
        *self
            .repair_gate
            .write()
            .expect("LOCK_POISONED: repair_gate") = gate.map(Arc::new);
        self
    }

    /// #14 — the shared repair-gate slot, so the serve reload path can flip the
    /// live server's posture (dirty → set, clean → clear) without rebuilding it.
    pub fn repair_gate_slot(&self) -> RepairGateSlot {
        self.repair_gate.clone()
    }

    /// The current repair gate, if the gateway is in repair-only mode.
    pub(crate) fn current_repair_gate(&self) -> Option<Arc<RepairGate>> {
        self.repair_gate
            .read()
            .expect("LOCK_POISONED: repair_gate")
            .clone()
    }

    /// P6 — wire the in-band config reload hook (serve path only). With it set,
    /// `praxec.command { reload: true }` fires the same gated rebuild+swap as
    /// SIGHUP. Omit it (CLI/one-shot/tests) and reload returns RELOAD_UNAVAILABLE.
    pub fn with_reload_hook(mut self, hook: ReloadHook) -> Self {
        self.reload_hook = Some(hook);
        self
    }

    /// P6b — wire the lazy staleness recheck (serve path only). With it set,
    /// every `call_tool` first runs a TTL-throttled mtime probe of the config
    /// file set and fires the gated reload when the operator edited it on
    /// disk. Omit it (CLI/one-shot/tests) and requests dispatch directly.
    pub fn with_staleness_hook(mut self, hook: StalenessHook) -> Self {
        self.staleness_hook = Some(hook);
        self
    }

    /// #18 — wire the shared peer slot the bridged audit sink reads, so events
    /// recorded during a drive push to the MCP client as logging notifications.
    /// The binary builds the slot via [`progress_bridge`] (wrapping the runtime's
    /// audit sink) and hands the same slot here; `call_tool` then captures the
    /// live peer into it. Omit it (default) and pushing is a no-op.
    pub fn with_progress_peer(mut self, peer: ProgressPeer) -> Self {
        self.progress_peer = peer;
        self
    }

    /// CMP-001 — set whether per-request `_meta` principal claims are honored
    /// (default `true`). `false` ignores `_meta` and runs every caller as the
    /// configured default principal.
    pub fn with_trust_meta_principal(mut self, trust: bool) -> Self {
        self.trust_meta_principal = trust;
        self
    }

    /// CMP-001 — set the default principal used when a request carries no
    /// `_meta` identity claim. The binary wires this from the gateway config's
    /// `gateway.principal` block so single-tenant operators can assert "this
    /// instance serves <subject> with <roles>". Omit it and the default stays
    /// anonymous (fail-closed).
    pub fn with_principal(mut self, principal: Principal) -> Self {
        self.default_principal = principal;
        self
    }

    /// CMP-001 — resolve the caller's identity for one request. A claim in the
    /// request `_meta` (set by the trusted embedding host) wins; otherwise the
    /// configured [`Self::default_principal`]. The agent-controlled tool
    /// `arguments` are deliberately NOT consulted — only the host can assert
    /// identity, so an agent cannot escalate to `human`/permissions.
    pub fn resolve_principal(&self, meta: &rmcp::model::Meta) -> Principal {
        if !self.trust_meta_principal {
            // Operator opted out of the _meta identity channel: config-only.
            return self.default_principal.clone();
        }
        meta.get(PRINCIPAL_META_KEY)
            .and_then(Principal::from_claim)
            .unwrap_or_else(|| self.default_principal.clone())
    }

    /// HITL elicitation push. Given a command `result` that MAY carry a
    /// `pending_human` gate, turn it into an `elicitation/create` round-trip when
    /// the client supports it and resume the mission in-band on accept.
    ///
    /// Returns the ORIGINAL parked result unchanged when: the result carries no
    /// gate, the client does not advertise `elicitation`, the elicitation call
    /// fails, or the human declines/cancels. In every one of those cases the
    /// `pending_human` block and its `resolve` handle remain, so the pull-list
    /// fallback (`praxec.query { approvals: true }`) still works.
    async fn drive_human_elicitation(
        &self,
        peer: &Peer<RoleServer>,
        principal: &Principal,
        result: Value,
    ) -> Result<Value, McpError> {
        let Some(pending) = result.get("pending_human").cloned() else {
            return Ok(result);
        };
        // Capability negotiation (MCP 2025-11-25 / SEP-1319): push only to a
        // client that declared the top-level `elicitation` capability. Everyone
        // else keeps the pull handle.
        let supports = peer
            .peer_info()
            .map(|info| info.capabilities.elicitation.is_some())
            .unwrap_or(false);
        if !supports {
            return Ok(result);
        }
        let gate: praxec_core::hitl::PendingHumanGate = match serde_json::from_value(pending) {
            Ok(gate) => gate,
            // A malformed gate is not worth failing the whole call over — the
            // caller still has the raw `pending_human` block to act on.
            Err(e) => {
                tracing::warn!(error = %e, "pending_human did not deserialize; skipping elicitation push");
                return Ok(result);
            }
        };

        let params = CreateElicitationRequestParams::FormElicitationParams {
            meta: None,
            message: elicit::message(&gate),
            requested_schema: elicit::form_schema(&gate),
        };
        let elicited = match peer.create_elicitation(params).await {
            Ok(elicited) => elicited,
            Err(e) => {
                tracing::warn!(error = %e, "elicitation/create failed; returning parked result");
                return Ok(result);
            }
        };

        match elicited.action {
            ElicitationAction::Accept => {
                // Resume: fire the gate transition under a HUMAN principal (the
                // person answered the form — that IS the human channel), passing
                // the elicited content as the submit's `arguments`. Reuses the
                // fully-governed submit path (guards, actor gate, audit).
                let content = elicited.content.unwrap_or_else(|| json!({}));
                let submit = build_gate_submit(&gate, content);
                self.dispatch_call_with_principal(submit, human_principal(principal))
                    .await
            }
            // Declined / cancelled: leave the mission parked, hand back the
            // original result so the operator can still resolve out-of-band.
            ElicitationAction::Decline | ElicitationAction::Cancel => Ok(result),
        }
    }

    /// SPEC §30.10.10 — wire an embedding backend. Default is `NoopEmbedder`
    /// (Tier 3 disabled). Pass an `Arc<HttpEmbedder>` or any custom
    /// `EmbeddingProvider` to enable semantic candidate ranking.
    pub fn with_embedder(mut self, embedder: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedder = embedder;
        self
    }

    /// SPEC §30 — wire the persistent (config-loaded) lexicon base.
    /// Callers pass the resolved config's `lexicon:` block (or an empty
    /// object when none was declared). Runtime writes via
    /// `gateway.lexicon.define` go into a separate overlay; reads
    /// merge both.
    pub fn with_lexicon(mut self, lexicon: Value) -> Self {
        self.lexicon_base = Arc::new(lexicon);
        self
    }

    pub fn with_discovery(mut self, discovery: Arc<dyn DiscoveryIndex>) -> Self {
        self.discovery = discovery;
        self
    }

    /// D6 — wire the hot-swappable `praxec.packs/v3` registry the gateway loaded
    /// from `discovery.registry`. The SAME handle the reload path swaps, so the
    /// selector's topology term never drifts from the tool catalog in the index.
    /// Omit it (CLI/tests/no registry configured) and ranking has no topology
    /// term — the pre-D6 behavior.
    pub fn with_registry(
        mut self,
        registry: Arc<praxec_core::hot_reload::SwappableRegistry>,
    ) -> Self {
        self.registry = registry;
        self
    }

    pub fn with_identity(mut self, name: impl Into<String>, version: impl Into<String>) -> Self {
        self.server_name = name.into();
        self.server_version = version.into();
        self
    }

    /// SPEC §5.9 — wire a guidance-acknowledgment store. Required for
    /// workflows that use the `guidance_acknowledged` guard.
    pub fn with_ack_store(
        mut self,
        ack_store: Arc<dyn praxec_core::ports::GuidanceAcknowledgmentStore>,
    ) -> Self {
        self.ack_store = Some(ack_store);
        self
    }

    /// SPEC §17.6 — enable the `gateway.skills.search` tool. Default off.
    /// Authoring-time only — the runtime guidance surface uses push-not-pull
    /// (§5.4). Enabling this for runtime workflows reintroduces the
    /// pull-discovery anti-pattern.
    pub fn with_skills_search(mut self, enabled: bool) -> Self {
        self.skills_search_enabled = enabled;
        self
    }

    /// SPEC §22 — enable scripts search via `praxec.query` with
    /// `kind: "script"`. Default off, same authoring-time-only rationale
    /// as `with_skills_search`.
    pub fn with_scripts_search(mut self, enabled: bool) -> Self {
        self.scripts_search_enabled = enabled;
        self
    }

    /// SPEC §32 — enable lexicon-define commands via MCP. Default OFF.
    /// Mirror of the `with_skills_search` / `with_scripts_search` opt-ins.
    pub fn with_lexicon_writes(mut self, enabled: bool) -> Self {
        self.lexicon_writes_enabled = enabled;
        self
    }

    /// SPEC §30.5 durability — persist MCP-defined lexicon terms under
    /// `<dir>/<term>.json` and load any already on disk into the overlay now,
    /// so vocabulary survives process restarts. Each file holds
    /// `{ "term": "...", "entry": { ... } }`. Malformed files are skipped
    /// with a warning rather than failing the boot.
    pub fn with_lexicon_dir(mut self, dir: std::path::PathBuf) -> Self {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for entry in rd.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                match std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|txt| serde_json::from_str::<Value>(&txt).ok())
                {
                    Some(v) => {
                        if let (Some(term), Some(ent)) =
                            (v.get("term").and_then(Value::as_str), v.get("entry"))
                        {
                            overlay.insert(term.to_string(), ent.clone());
                        } else {
                            tracing::warn!(path = %path.display(), "lexicon dir: file missing term/entry; skipping");
                        }
                    }
                    None => {
                        tracing::warn!(path = %path.display(), "lexicon dir: unreadable/invalid JSON; skipping");
                    }
                }
            }
        }
        self.lexicon_dir = Some(dir);
        self
    }

    /// Persist one lexicon term to `<lexicon_dir>/<term>.json` as
    /// `{ term, entry }`. Returns the written path (for `persisted_to`),
    /// or `None` if no dir is configured. Errors are surfaced to the caller.
    pub(crate) fn persist_lexicon_term(
        &self,
        term: &str,
        entry: &Value,
    ) -> std::io::Result<Option<String>> {
        let Some(dir) = &self.lexicon_dir else {
            return Ok(None);
        };
        std::fs::create_dir_all(dir)?;
        // Terms are kebab-case vocabulary; sanitize defensively so a stray
        // separator can't escape the directory.
        let safe: String = term
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let path = dir.join(format!("{safe}.json"));
        let body = serde_json::to_string_pretty(&json!({ "term": term, "entry": entry }))
            .unwrap_or_else(|_| "{}".to_string());
        std::fs::write(&path, body)?;
        Ok(Some(path.display().to_string()))
    }

    /// SPEC §30.10.3 — seed the set of pending (PENDING_DEFINITION) subjects
    /// detected at config-load time. Callers pass the list returned by
    /// `praxec_core::lexicon::pending_subjects_from_resolved(config)`.
    /// Resolution handlers remove entries from this set; cancel uses it to
    /// distinguish bookkeeping placeholders from authored entries.
    ///
    /// The same `Arc` is shared into the embedded `WorkflowRuntime` so that
    /// the runtime's pre-start subject walk reflects resolved state immediately
    /// when a resolution handler removes an entry from the set — no config
    /// reload needed (SPEC §30.10.4, Gap 2 fix).
    ///
    /// When `subjects` is empty, the server still wires the live set into the
    /// runtime (as an empty `Some(Arc)`) so that Phase 1 (live-set check) is
    /// used and future additions to the set are observable. This is correct:
    /// a config with no pending subjects should start workflows without the
    /// snapshot fallback blocking them.
    pub fn with_pending_subjects(mut self, subjects: Vec<String>) -> Self {
        let shared: Arc<std::sync::RwLock<std::collections::HashSet<String>>> =
            Arc::new(std::sync::RwLock::new(subjects.into_iter().collect()));
        self.pending_subjects = shared.clone();
        // Share the same Arc into the runtime. WorkflowRuntime::with_pending_subjects
        // sets pending_subjects to Some(arc), switching the runtime to Phase 1
        // (live-set) subject checks (SPEC §30.10.4 Gap 2 fix).
        self.runtime = self.runtime.with_pending_subjects(shared);
        self
    }

    /// SPEC §22 — wire a script-acknowledgment store. Required for
    /// workflows that use the `script_acknowledged` guard.
    pub fn with_script_ack_store(
        mut self,
        store: Arc<dyn praxec_core::ports::ScriptAcknowledgmentStore>,
    ) -> Self {
        self.script_ack_store = Some(store);
        self
    }

    /// H5 — whether a guidance-acknowledgment store is wired. The binary's
    /// `serve_with` asserts this is true at boot whenever any workflow gates on
    /// the `guidance_acknowledged` guard, so a config that needs the guard can
    /// never start with a permanently-unsatisfiable gate (prevent → detect →
    /// fail-fast). Mirrors the write side: the server records describe-acks into
    /// this store, the guard evaluator reads them — both must see the same Arc.
    pub fn has_guidance_ack_store(&self) -> bool {
        self.ack_store.is_some()
    }

    /// H5 — whether a script-acknowledgment store is wired. See
    /// [`has_guidance_ack_store`](Self::has_guidance_ack_store).
    pub fn has_script_ack_store(&self) -> bool {
        self.script_ack_store.is_some()
    }

    /// SPEC §30.10.10 — config-load embedding backfill.
    ///
    /// Walks every entry in `lexicon_base` (and the current overlay). For
    /// each entry that is missing `_embedding`, computes and stores the
    /// vector. Failures are logged as warnings and do NOT abort — backfill
    /// is best-effort.
    ///
    /// No-ops when the active embedder is `NoopEmbedder`. Callers should
    /// invoke this once after `PraxecServer::new(...).with_lexicon(...)
    /// .with_embedder(...)` before serving requests.
    pub async fn backfill_lexicon_embeddings(&self) {
        if self.embedder.backend_name() == "noop" {
            return;
        }

        // Collect (term, entry) pairs that are missing _embedding.
        // We read base and overlay independently then merge for the full picture.
        let base_entries: Vec<(String, serde_json::Value)> = {
            self.lexicon_base
                .as_object()
                .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default()
        };

        // Process base entries first.
        for (term, entry) in base_entries {
            if entry.get("_embedding").is_some() {
                continue; // already has embedding
            }
            if entry.get("state").and_then(serde_json::Value::as_str) == Some("PENDING_DEFINITION")
            {
                continue; // skip placeholders
            }
            let definition_short = entry
                .get("definition_short")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let aliases: Vec<String> = entry
                .get("aliases")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let text =
                praxec_core::embeddings::entry_embed_text(&term, &aliases, definition_short, None);
            match self.embedder.embed(&text).await {
                Ok(vec) => {
                    let mut updated = entry.clone();
                    if let Some(obj) = updated.as_object_mut() {
                        obj.insert("_embedding".to_string(), json!(vec));
                    }
                    let mut overlay = self
                        .lexicon_overlay
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Only write to overlay if not already present there
                    // (overlay would have a more-current version).
                    overlay.entry(term.clone()).or_insert(updated);
                }
                Err(e) => {
                    tracing::warn!(
                        term = %term,
                        error = %e,
                        "backfill_lexicon_embeddings: failed to embed term '{}'; skipping",
                        term
                    );
                }
            }
        }

        // Process overlay entries (may have been added at runtime, also missing _embedding).
        let overlay_snapshot: Vec<(String, serde_json::Value)> = {
            let overlay = self
                .lexicon_overlay
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            overlay
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        let mut overlay_updates: Vec<(String, serde_json::Value)> = Vec::new();
        for (term, entry) in overlay_snapshot {
            if entry.get("_embedding").is_some() {
                continue;
            }
            if entry.get("state").and_then(serde_json::Value::as_str) == Some("PENDING_DEFINITION")
            {
                continue;
            }
            let definition_short = entry
                .get("definition_short")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let aliases: Vec<String> = entry
                .get("aliases")
                .and_then(serde_json::Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let text =
                praxec_core::embeddings::entry_embed_text(&term, &aliases, definition_short, None);
            match self.embedder.embed(&text).await {
                Ok(vec) => {
                    let mut updated = entry.clone();
                    if let Some(obj) = updated.as_object_mut() {
                        obj.insert("_embedding".to_string(), json!(vec));
                    }
                    overlay_updates.push((term.clone(), updated));
                }
                Err(e) => {
                    tracing::warn!(
                        term = %term,
                        error = %e,
                        "backfill_lexicon_embeddings: failed to embed overlay term '{}'; skipping",
                        term
                    );
                }
            }
        }

        // Batch-write overlay updates.
        if !overlay_updates.is_empty() {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (term, updated) in overlay_updates {
                overlay.insert(term, updated);
            }
        }
    }

    /// Transport-free entry point that mirrors what `ServerHandler::call_tool`
    /// does, minus the `CallToolResult` wrapping. Lets parity tests assert on
    /// per-tool argument parsing and response shape without spinning up an
    /// rmcp transport. Behaviorally identical to `call_tool` — same dispatch
    /// table, same error mapping.
    ///
    /// This entry has no `_meta` channel, so it runs as the configured
    /// [`Self::default_principal`]; use [`Self::dispatch_call_with_principal`]
    /// to exercise a specific identity.
    pub async fn dispatch_call(&self, request: CallToolRequestParams) -> Result<Value, McpError> {
        self.dispatch_call_with_principal(request, self.default_principal.clone())
            .await
    }

    /// CMP-001 — dispatch a tool call under an explicit resolved identity.
    /// `call_tool` resolves the principal from the request `_meta` (host
    /// channel) and calls here.
    pub async fn dispatch_call_with_principal(
        &self,
        request: CallToolRequestParams,
        principal: Principal,
    ) -> Result<Value, McpError> {
        let args: Value = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or_else(|| json!({}));

        // Retain a clone of the original args so the error-handler block below
        // can echo them back in structured error responses (e.g.
        // SUBJECT_NEEDS_DEFINITION queued_command.args) even after `args` has
        // been moved into a dispatch call.
        let original_args = args.clone();

        let result = match request.name.as_ref() {
            TOOL_QUERY => {
                // §32: Some `kind` values and `subject: "lexicon:..."` need
                // specialized routing before the generic shape-router:
                //
                //  kind="skill"    → handle_skills_search (flag-gated)
                //  kind="script"   → handle_scripts_search (flag-gated)
                //  kind="lexicon"  → handle_lexicon_search
                //  subject="lexicon:<term>" (no query/wid/tr) → handle_lexicon_lookup
                //
                // All other args fall through to dispatch_query.
                let kind = args
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let subject_is_lexicon = args
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| s.starts_with("lexicon:"));
                let has_query = args.get("query").is_some();
                let has_wid = args.get("workflowId").is_some();
                let has_tr = args.get("transition").is_some();

                match kind.as_deref() {
                    Some("skill") => {
                        if !self.skills_search_enabled {
                            return Err(McpError::invalid_params(
                                "praxec.query with kind='skill' is disabled. \
                                 Enable it in the gateway config: \
                                 `praxec.authoring.skills_search: true` \
                                 (authoring-time only — the runtime guidance surface is \
                                 push-not-pull, SPEC §5.4)."
                                    .to_string(),
                                None,
                            ));
                        }
                        self.handle_skills_search(args).await
                    }
                    Some("script") => {
                        if !self.scripts_search_enabled {
                            return Err(McpError::invalid_params(
                                "praxec.query with kind='script' is disabled. \
                                 Enable it in the gateway config: \
                                 `praxec.authoring.scripts_search: true` \
                                 (authoring-time only)."
                                    .to_string(),
                                None,
                            ));
                        }
                        self.handle_scripts_search(args).await
                    }
                    Some("lexicon") => {
                        // Lexicon search: pass query + limit through.
                        self.handle_lexicon_search(args).await
                    }
                    _ if subject_is_lexicon && !has_query && !has_wid && !has_tr => {
                        // Lexicon lookup: subject = "lexicon:<term>". Reshape
                        // to the expected { term } arg shape.
                        let term = args["subject"]
                            .as_str()
                            .and_then(|s| s.strip_prefix("lexicon:"))
                            .unwrap_or("")
                            .to_string();
                        // CMP-014 — `subject: "lexicon:"` with an empty term is
                        // a malformed lookup, not a request to look up "".
                        if term.is_empty() {
                            return Err(McpError::invalid_params(
                                "lexicon lookup requires a non-empty term: \
                                 subject must be 'lexicon:<term>'"
                                    .to_string(),
                                None,
                            ));
                        }
                        self.handle_lexicon_lookup(json!({ "term": term })).await
                    }
                    _ => self.dispatch_query(args, principal).await.map(|mut resp| {
                        // #14 — when in repair-only mode, stamp the read surface
                        // (home included) with the contract diagnostics + the
                        // callable repair surface, so the LLM sees WHY the
                        // functional surface is withheld and WHERE to fix it.
                        if let Some(gate) = self.current_repair_gate() {
                            if let Some(obj) = resp.as_object_mut() {
                                obj.insert("repair_required".into(), repair_mode_block(&gate));
                            }
                        }
                        resp
                    }),
                }
            }
            TOOL_COMMAND => {
                // §32: `define` shape (subject namespaced + definition) is gated
                // by with_lexicon_writes(true). Default-off in production (safe
                // by construction); authoring builds opt in via the builder.
                // CMP-014 — parse ONCE and surface a malformed-args error as
                // invalid_params. The previous `.unwrap_or(...all None...)`
                // masked the real serde error and let a malformed command fall
                // through to dispatch as if it were empty.
                let parsed: crate::args::CommandArgs = serde_json::from_value(args.clone())
                    .map_err(|e| {
                        McpError::invalid_params(format!("invalid arguments: {e}"), None)
                    })?;
                // P6 — in-band config reload. `reload: true` on the two-tool
                // surface fires the same gated rebuild+swap as SIGHUP; no third
                // tool is added. Returns the reload outcome as JSON.
                if parsed.reload.unwrap_or(false) {
                    return match &self.reload_hook {
                        Some(hook) => Ok(hook().await),
                        None => Ok(json!({
                            "error": {
                                "code": "RELOAD_UNAVAILABLE",
                                "message": "Config reload is a serve-mode capability; this runtime was started without a reload hook."
                            }
                        })),
                    };
                }
                // #14 — repair-only gate. While the pack is contract-dirty, only
                // the declared repair surface may be STARTED. `reload` (above)
                // and `define` (below) stay open — they are how contracts get
                // fixed. Any other `start` is refused with the precise
                // diagnostics so the LLM repairs the contracts first, then the
                // full surface reopens on a clean reload.
                if let Some(gate) = self.current_repair_gate() {
                    if let Some(def_id) = parsed.definition_id.as_deref() {
                        if !gate.allows(def_id) {
                            return Ok(repair_required_response(def_id, &gate));
                        }
                    }
                }
                let is_lexicon_define = parsed
                    .subject
                    .as_deref()
                    .is_some_and(|s| s.starts_with("lexicon:"))
                    && parsed.definition.is_some();
                if is_lexicon_define && !self.lexicon_writes_enabled {
                    Ok(json!({
                        "error": {
                            "code": "LEXICON_WRITES_DISABLED",
                            "message": "This runtime does not accept lexicon define commands.",
                            "hint": "Operators add lexicon terms via the `px lexicon define` CLI subcommand, or enable MCP lexicon writes in the gateway config with `praxec.authoring.lexicon_writes: true` (authoring-time only)."
                        },
                        "links": [
                            {
                                "rel": "operator_path",
                                "method": "cli",
                                "args": { "command": "px lexicon define <term> <definition>" }
                            },
                            {
                                "rel": "lookup",
                                "method": "praxec.query",
                                "args": { "subject": parsed.subject.unwrap_or_default() }
                            }
                        ]
                    }))
                } else {
                    self.dispatch_command(args, principal).await
                }
            }
            other => {
                return Err(McpError::invalid_params(
                    format!(
                        "Unknown tool '{other}'. Available: {} (see SPEC §32).",
                        STABLE_TOOL_NAMES.join(", ")
                    ),
                    None,
                ));
            }
        };

        match result {
            Ok(v) => Ok(v),
            Err(e) => {
                // SPEC §32 — RUN_ID_ALREADY_RUNNING is a structured response
                // at the MCP boundary (per the AMBIGUOUS_INTENT /
                // LEXICON_WRITES_DISABLED pattern). Downcast before falling
                // through to the generic internal_error mapper.
                if let Some(praxec_core::RuntimeError::RunIdAlreadyRunning {
                    run_id,
                    existing_workflow_id,
                }) = e.downcast_ref::<praxec_core::RuntimeError>()
                {
                    return Ok(run_id_already_running(run_id, existing_workflow_id));
                }

                // SPEC §30.10.5 — SUBJECT_NEEDS_DEFINITION is a structured
                // interaction response. The original `original_args` (the full
                // CommandArgs JSON) are echoed back as `queued_command.args`
                // so the caller can retry unchanged once the subject is defined.
                if let Some(praxec_core::RuntimeError::SubjectNeedsDefinition {
                    unknown_subject,
                    bounded_context,
                    workflow_id_context,
                }) = e.downcast_ref::<praxec_core::RuntimeError>()
                {
                    let merged = self.lexicon_merged_definition();
                    return Ok(subject_needs_definition(
                        unknown_subject,
                        bounded_context.as_deref(),
                        workflow_id_context,
                        &original_args,
                        Some(&merged),
                        Some(self.embedder.as_ref()),
                    )
                    .await);
                }

                // CMP-014 / CMP-030 — caller-supplied malformed/missing params
                // surface as invalid_params (-32602), not internal_error. Any
                // handler that raised a `BadRequest` (via parse_args / bad_request)
                // is reporting a 4xx-class client error, not a server fault.
                if let Some(handlers::BadRequest(msg)) = e.downcast_ref::<handlers::BadRequest>() {
                    return Err(McpError::invalid_params(msg.clone(), None));
                }

                Err(McpError::internal_error(e.to_string(), None))
            }
        }
    }
}

impl ServerHandler for PraxecServer {
    fn get_info(&self) -> ServerInfo {
        let mut server_info =
            Implementation::new(self.server_name.clone(), self.server_version.clone());
        server_info.title = Some("praxec".to_string());
        server_info.description =
            Some("Configurable MCP gateway with HATEOAS workflow governance".to_string());

        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(tools::instructions().to_string());
        info
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        // CMP-036 — warn on sink failure so a lost lifecycle record is
        // observable rather than silently dropped.
        if let Err(e) = self
            .runtime
            .audit()
            .record(AuditEvent::new("server.initialized").with_payload(json!({
                "name": self.server_name,
                "version": self.server_version,
            })))
            .await
        {
            tracing::warn!(error = %e, "server.initialized audit write failed");
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // §32 — always exactly two tools. Skills / scripts search are gated
        // paths within praxec.query (kind="skill" / kind="script"), not
        // separate tool entries. The skills_search_enabled /
        // scripts_search_enabled flags govern dispatch, not tool advertising.
        Ok(ListToolsResult::with_all_items(tool_definitions()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // #18 — capture the connected peer so the bridged audit sink can push
        // events recorded DURING this (possibly long) call to the client.
        self.progress_peer.set(context.peer.clone());
        // P6b — lazy staleness recheck BEFORE dispatch: if the operator edited
        // the config on disk (and the TTL elapsed), the gated reload runs now,
        // so THIS request already sees the new config. Within the TTL window
        // this returns immediately (no filesystem access).
        if let Some(check) = &self.staleness_hook {
            check().await;
        }
        // CMP-001 — resolve identity from the request `_meta` (host channel)
        // falling back to the configured default, then dispatch under it.
        let principal = self.resolve_principal(&context.meta);
        // Only a command can park a mission on a human gate; capture the tool
        // name before `request` is moved into dispatch so the elicitation push
        // knows whether to inspect the result.
        let is_command = request.name.as_ref() == TOOL_COMMAND;
        let result = self
            .dispatch_call_with_principal(request, principal.clone())
            .await?;
        // HITL — if the command parked on a human gate AND this client speaks
        // elicitation, turn the gate into an `elicitation/create` round-trip and
        // resume in-band on accept. A non-capable client is untouched (it keeps
        // the `pending_human` block + its `resolve` handle — the pull fallback).
        let result = if is_command {
            self.drive_human_elicitation(&context.peer, &principal, result)
                .await?
        } else {
            result
        };
        Ok(CallToolResult::structured(result))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_definitions().into_iter().find(|t| t.name == name)
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {
        tracing::info!("praxec client initialized");
    }
}

/// #14 — the structured refusal returned when a `start` is blocked by the
/// repair-only gate. Carries the precise contract diagnostics AND the callable
/// repair surface (with ready-to-fire `start_repair` links), so the consuming
/// LLM has everything it needs to fix the contracts through the gateway.
fn repair_required_response(def_id: &str, gate: &RepairGate) -> Value {
    let mut resp = repair_mode_block(gate);
    if let Some(obj) = resp.as_object_mut() {
        obj.insert(
            "error".into(),
            json!({
                "code": "CONTRACT_DIRTY_REPAIR_ONLY",
                "message": format!(
                    "Gateway is in repair-only mode: the loaded pack has {} contract violation(s). \
                     Starting '{def_id}' is refused until the contracts are 100% clean — only the \
                     declared repair surface may run now. Fix the contracts (the diagnostics name \
                     the exact definition, binding, and expected contract), then reload.",
                    gate.diagnostics.len()
                ),
            }),
        );
    }
    resp
}

/// #14 — the shared repair-mode payload: the precise contract diagnostics, the
/// declared repair surface, and ready-to-fire `start_repair` links. Stamped on
/// a gated `start` refusal AND on every read while in repair-only mode.
fn repair_mode_block(gate: &RepairGate) -> Value {
    let mut repair: Vec<&String> = gate.repair_ids.iter().collect();
    repair.sort();
    let links: Vec<Value> = repair
        .iter()
        .map(|id| {
            json!({
                "rel": "start_repair",
                "title": format!("Start repair capability '{id}'"),
                "method": "praxec.command",
                "args": { "definitionId": id, "input": {} }
            })
        })
        .collect();
    json!({
        "mode": "repair_only",
        "diagnostics": gate.diagnostics,
        "repairSurface": repair,
        "links": links,
    })
}

/// Build the `praxec.command` submit that resolves a human gate: the exact
/// `resolve` args the `pending_human` block advertises, plus the human's
/// elicited answer as `arguments`.
fn build_gate_submit(
    gate: &praxec_core::hitl::PendingHumanGate,
    arguments: Value,
) -> CallToolRequestParams {
    let mut args = serde_json::Map::new();
    args.insert("workflowId".into(), json!(gate.workflow_id));
    args.insert("expectedVersion".into(), json!(gate.expected_version));
    args.insert("transition".into(), json!(gate.transition));
    args.insert("arguments".into(), arguments);
    let mut request = CallToolRequestParams::new(TOOL_COMMAND);
    request.arguments = Some(args);
    request
}

/// Elevate the caller to a human principal for the resume submit. The person
/// physically accepted the elicitation form, so the elicitation IS the human
/// channel: tag the principal with [`Principal::HUMAN_ROLE`] so it passes the
/// `actor: human` gate, giving it a subject when the caller was anonymous.
fn human_principal(base: &Principal) -> Principal {
    let mut principal = base.clone();
    if !principal.is_human() {
        principal.roles.push(Principal::HUMAN_ROLE.to_string());
    }
    if principal.subject.is_empty() || principal.subject == "anonymous" {
        principal.subject = "elicited-human".to_string();
    }
    principal
}
