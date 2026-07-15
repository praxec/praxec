//! Per-tool handler bodies. Methods live in a sibling `impl PraxecServer`
//! block (same crate, same type — see `lib.rs` for the struct definition
//! and `ServerHandler` trait impl).

use praxec_core::audit::AuditEvent;
use praxec_core::discovery::{DiscoveryKind, SearchRequest};
use praxec_core::embeddings::{EmbeddingProvider, entry_embed_text};
use praxec_core::model::{GetWorkflow, Principal, StartWorkflow, SubmitTransition};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::PraxecServer;
use crate::args::{
    CommandArgs, DescribeArgs, ExplainArgs, GetArgs, QueryArgs, SearchArgs, StartArgs, SubmitArgs,
};
use crate::tools::parse_kind;

/// Caller-supplied argument errors that must surface at the MCP boundary as
/// `invalid_params (-32602)` rather than `internal_error (-32603)`. The
/// terminal arm of [`PraxecServer::dispatch_call`] downcasts this so a
/// malformed/missing field maps to a 4xx-class protocol error (CMP-014 /
/// CMP-030). Handlers raise it via [`parse_args`] (serde failures) or
/// [`bad_request`] (missing required fields, unrecognized enum values).
#[derive(Debug)]
pub(crate) struct BadRequest(pub String);

impl std::fmt::Display for BadRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BadRequest {}

/// Construct a [`BadRequest`] anyhow error for a missing/invalid caller field.
pub(crate) fn bad_request(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(BadRequest(msg.into()))
}

/// Deserialize caller-supplied tool arguments, mapping any serde error to a
/// [`BadRequest`] (→ `invalid_params`) so a malformed field never masquerades
/// as an internal error.
pub(crate) fn parse_args<T: DeserializeOwned>(args: Value) -> anyhow::Result<T> {
    serde_json::from_value(args).map_err(|e| bad_request(format!("invalid arguments: {e}")))
}

/// Default event cap for one `observe` query window. Callers override with
/// `limit`; a truncated window carries `truncated: true` plus a `next_since`
/// cursor link so the client pages forward instead of receiving an unbounded
/// response.
const DEFAULT_OBSERVE_LIMIT: u64 = 200;

impl PraxecServer {
    pub(crate) async fn handle_home(&self) -> anyhow::Result<Value> {
        self.discovery.home().await
    }

    pub(crate) async fn handle_search(&self, args: Value) -> anyhow::Result<Value> {
        let parsed: SearchArgs = parse_args(args)?;
        let query = parsed.query.unwrap_or_default();
        // CMP-014 — an unrecognized `kind` must not silently degrade to an
        // unfiltered search. When `kind` is present but not a known filter,
        // reject with invalid_params rather than dropping it to None.
        let kind = match parsed.kind.as_deref() {
            Some(k) => Some(parse_kind(k).ok_or_else(|| {
                bad_request(format!(
                    "unknown kind '{k}'; expected one of: workflow, capability, connection \
                     (or skill / script / lexicon on praxec.query)"
                ))
            })?),
            None => None,
        };
        let limit = parsed.limit as usize;

        let mut hits = self
            .discovery
            .search(SearchRequest {
                query: query.clone(),
                kind,
                limit,
            })
            .await?;

        // Evidence loop, last hop — annotate workflow hits with the intent
        // index's historical track record so the caller chooses by evidence,
        // not blind. Never fails the search: missing/thin evidence is the
        // normal state (fresh system, non-file sink), silently omitted.
        self.attach_intent_evidence(&mut hits).await;

        // Selector (D6) — re-rank the annotated hits by the deterministic
        // relevance + evidence + topology blend and surface the explainable
        // `why`, instead of returning raw lexical order. `rank_candidates` is
        // total (one entry per hit) and stable, so every hit maps and the order
        // is fully determined. The registry is the one the gateway loaded from
        // `discovery.registry` (and re-loads on every reload); `None` when the
        // operator configured none, which zeroes the topology term uniformly and
        // leaves the relevance+evidence blend exactly as it was.
        //
        // D7 — and the learned selector policy, whose activation bar comes from
        // the operator's tuning (`intent.policy_min_runs`), exactly as the
        // annotator above takes `intent.min_runs`. A template with too little
        // accrued evidence falls through to the blend above, unchanged: on a
        // fresh install nothing here re-ranks anything.
        let registry = self.registry.current();
        let ranked = praxec_core::discovery::rank_candidates(
            &hits,
            registry.as_deref(),
            &praxec_core::discovery::SelectorPolicy::from_tuning(),
        );
        let hit_by_id: std::collections::HashMap<&str, &praxec_core::discovery::SearchHit> =
            hits.iter().map(|h| (h.item.id.as_str(), h)).collect();
        let items: Vec<Value> = ranked
            .iter()
            .filter_map(|r| {
                let hit = hit_by_id.get(r.id.as_str())?;
                let mut v = serde_json::to_value(hit).ok()?;
                if let Some(obj) = v.as_object_mut() {
                    obj.insert(
                        "ranking".to_string(),
                        json!({ "score": r.score, "why": r.why }),
                    );
                }
                Some(v)
            })
            .collect();

        Ok(json!({
            "query": query,
            "kind": kind.map(|k| k.as_str()),
            "items": items,
            "links": [
                { "rel": "home", "method": "praxec.query", "args": {} }
            ]
        }))
    }

    /// Attach per-`(task_class, template)` intent evidence to workflow search
    /// hits. Reuses the exact read + aggregate path of `praxec intent report`:
    /// [`AuditSink::try_list_events`] over the runtime's sink →
    /// [`observations_from_audit`] → [`aggregate`], gated by the tuning
    /// `intent.min_runs` threshold (the same "trust the rate" bar the report
    /// flags thin samples with).
    ///
    /// Graceful degrade by design — evidence is an annotation, never a
    /// precondition: a non-file sink (no stored events), an empty audit dir,
    /// or even a genuine read error all leave the hits unannotated and the
    /// search successful. A read error is logged (it IS a defect signal) but
    /// must not turn a working search into a failure.
    ///
    /// [`AuditSink::try_list_events`]: praxec_core::audit::AuditSink::try_list_events
    /// [`observations_from_audit`]: praxec_core::intent_index::observations_from_audit
    /// [`aggregate`]: praxec_core::intent_index::aggregate
    async fn attach_intent_evidence(&self, hits: &mut [praxec_core::discovery::SearchHit]) {
        use praxec_core::intent_index::{
            IntentParams, aggregate, annotate_hits_with_evidence, observations_from_audit,
        };
        // Only pay the audit read when a hit could carry evidence at all.
        if !hits
            .iter()
            .any(|h| h.item.kind == DiscoveryKind::Workflow && h.item.task_class().is_some())
        {
            return;
        }
        let events = match self.runtime.audit().try_list_events().await {
            Ok(Some(events)) if !events.is_empty() => events,
            // Non-file sink (events not stored) or no history yet — the
            // normal fresh-system state; evidence is simply absent.
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "intent-evidence audit read failed — returning search hits without evidence"
                );
                return;
            }
        };
        let models = praxec_core::model_catalog::model_catalog().models;
        let stats = aggregate(&observations_from_audit(&events, &models));
        annotate_hits_with_evidence(hits, &stats, IntentParams::from_tuning().min_runs);
    }

    pub(crate) async fn handle_describe(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: DescribeArgs = parse_args(args)?;
        let id = parsed.id.ok_or_else(|| bad_request("id is required"))?;

        // SPEC §5.8 — every `gateway.describe` call emits an audit record so
        // the authoring trail captures *which* guidance the model fetched.
        // Non-critical-path audit (per §7.3 terminology): sink failure does
        // NOT abort the describe, but emits an `audit.write_failed`
        // self-event so the failure is observable. The describe outcome
        // (ok/failed) is recorded after the lookup completes.
        let workflow_id_for_audit = parsed.workflow_id.clone();

        // SPEC §8.2: if the caller is acting inside a workflow, resolve
        // guidance bodies from the instance's pinned snapshot — the live
        // config could have drifted since `workflow.start`. Falls back to
        // the live discovery index when no workflowId is given or when the
        // subject is not in the snapshot (e.g. it's a workflow/capability
        // lookup, not a guidance fragment).
        //
        // Guidance responses use the SPEC §12 flat wire format:
        //   { kind: "guidance", subject, verb, body, lifecycle, hash }
        // Workflow / capability / connection lookups keep the existing
        // `{ id, item, links }` wrapper since they need the HATEOAS links
        // to drive the next call.
        if let Some(workflow_id) = parsed.workflow_id.as_deref() {
            // SPEC §22 — try scripts library first. If the subject lives
            // there, record the script-ack and return early. This is
            // checked BEFORE guidance because the two namespaces are
            // disjoint by design (skills use cognitive verbs, scripts use
            // action verbs); a subject in scripts can't also be in
            // skills, so the order is a clean fast path, not a precedence
            // decision.
            if let Some(mut body) = self
                .runtime
                .describe_script_for_workflow(workflow_id, &id)
                .await?
            {
                if let Some(ack) = self.script_ack_store.as_ref() {
                    if let Some(h) = body.get("hash").and_then(Value::as_str) {
                        // CMP-036 — this ack feeds the `script_acknowledged`
                        // guard; a silent write failure would leave the guard
                        // permanently unsatisfiable with no trace. Warn so the
                        // loss is observable.
                        if let Err(e) = ack.record(workflow_id, &id, h).await {
                            tracing::warn!(
                                workflow_id = %workflow_id,
                                subject = %id,
                                error = %e,
                                "script ack store write failed; script_acknowledged guard may not be satisfiable"
                            );
                        }
                    }
                }
                self.emit_describe_audit(
                    &id,
                    body.get("verb").and_then(Value::as_str),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "links".into(),
                        json!([
                            { "rel": "home", "method": "praxec.query", "args": {} },
                            {
                                "rel": "get",
                                "method": "praxec.query",
                                "args": { "workflowId": workflow_id }
                            }
                        ]),
                    );
                    // SPEC §30.10 — embed lexicon entry for this script's
                    // subject using the workflow's pinned lexicon snapshot.
                    if let Ok(instance) = self.runtime.load_instance(workflow_id).await {
                        let snapshot_lex = instance
                            .definition
                            .get("_lexiconLibrary")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let merged = json!({ "_lexiconLibrary": snapshot_lex });
                        let term = subject_portion_from_skill_key(&id).to_string();
                        if let Some(lex) = embed_lexicon_for_subjects(
                            &[term.as_str()],
                            &merged,
                            LEXICON_INLINE_BUDGET,
                        ) {
                            obj.insert("lexicon".into(), lex);
                        }
                    }
                }
                return Ok(body);
            }

            if let Some(mut body) = self
                .runtime
                .describe_guidance_for_workflow(workflow_id, &id)
                .await?
            {
                // SPEC §5.9 — record this fetch into the ack store, keyed
                // by (workflow_id, subject, body-hash). Hash-flip
                // invalidation makes the guard meaningful: a future edit
                // to the body changes the hash and the prior ack stops
                // satisfying the guard.
                if let Some(ack) = self.ack_store.as_ref() {
                    if let Some(h) = body.get("hash").and_then(Value::as_str) {
                        // CMP-036 — this ack feeds the `guidance_acknowledged`
                        // guard; a silent write failure would leave the guard
                        // permanently unsatisfiable with no trace. Warn so the
                        // loss is observable.
                        if let Err(e) = ack.record(workflow_id, &id, h).await {
                            tracing::warn!(
                                workflow_id = %workflow_id,
                                subject = %id,
                                error = %e,
                                "guidance ack store write failed; guidance_acknowledged guard may not be satisfiable"
                            );
                        }
                    }
                }
                self.emit_describe_audit(
                    &id,
                    body.get("verb").and_then(Value::as_str),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                // body is already SPEC §12 shape — just attach next-step
                // links alongside (preserves HATEOAS without breaking the
                // top-level shape).
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "links".into(),
                        json!([
                            { "rel": "home", "method": "praxec.query", "args": {} },
                            {
                                "rel": "get",
                                "method": "praxec.query",
                                "args": { "workflowId": workflow_id }
                            }
                        ]),
                    );
                    // SPEC §30.10 — embed lexicon entries for this guidance
                    // subject + its refs using the workflow's pinned snapshot.
                    if let Ok(instance) = self.runtime.load_instance(workflow_id).await {
                        let snapshot_lex = instance
                            .definition
                            .get("_lexiconLibrary")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        let merged = json!({ "_lexiconLibrary": snapshot_lex });
                        let term = subject_portion_from_skill_key(&id).to_string();
                        let terms = collect_describe_terms(&term, &merged);
                        let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
                        if let Some(lex) =
                            embed_lexicon_for_subjects(&term_refs, &merged, LEXICON_INLINE_BUDGET)
                        {
                            obj.insert("lexicon".into(), lex);
                        }
                    }
                }
                return Ok(body);
            }
        }

        let item = match self.discovery.describe(&id).await {
            Ok(item) => item,
            Err(e) => {
                self.emit_describe_audit(
                    &id,
                    None,
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "failed",
                    Some("GUIDANCE_DESCRIBE_FAILED"),
                )
                .await;
                return Err(e);
            }
        };

        // If the discovery layer surfaced a guidance fragment, reshape it
        // to SPEC §12 flat form. `DiscoveryKind::Guidance` items carry
        // `verb` and `body` directly on the item.
        if let Some(item) = &item {
            if matches!(item.kind, DiscoveryKind::Guidance) {
                self.emit_describe_audit(
                    &id,
                    item.verb.as_deref(),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                // SPEC §30.10 — embed lexicon entry for this guidance
                // subject + its refs using the live (merged) lexicon.
                let merged = self.lexicon_merged_definition();
                let term = subject_portion_from_skill_key(&id).to_string();
                let terms = collect_describe_terms(&term, &merged);
                let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
                let mut resp = json!({
                    "kind": "guidance",
                    "subject": item.id,
                    "verb": item.verb.as_deref().unwrap_or_default(),
                    "body": item.body.as_deref().unwrap_or_default(),
                    "links": [
                        { "rel": "home", "method": "praxec.query", "args": {} },
                        { "rel": "search", "method": "praxec.query", "args": { "query": "" } }
                    ]
                });
                if let Some(lex) =
                    embed_lexicon_for_subjects(&term_refs, &merged, LEXICON_INLINE_BUDGET)
                {
                    resp["lexicon"] = lex;
                }
                return Ok(resp);
            }
            // SPEC §22 — non-workflow-context script describe: surface
            // body from the live indexer. (For workflow-context script
            // describes, the snapshot path above is used and an ack
            // recorded.)
            if matches!(item.kind, DiscoveryKind::Script) {
                self.emit_describe_audit(
                    &id,
                    item.verb.as_deref(),
                    workflow_id_for_audit.as_deref(),
                    &principal,
                    "ok",
                    None,
                )
                .await;
                // SPEC §30.10 — embed lexicon entry for this script's
                // subject using the live (merged) lexicon.
                let merged = self.lexicon_merged_definition();
                let term = subject_portion_from_skill_key(&id).to_string();
                let mut resp = json!({
                    "kind": "script",
                    "subject": item.id,
                    "verb": item.verb.as_deref().unwrap_or_default(),
                    "body": item.body.as_deref().unwrap_or_default(),
                    "links": [
                        { "rel": "home", "method": "praxec.query", "args": {} },
                        { "rel": "search", "method": "praxec.query", "args": { "query": "" } }
                    ]
                });
                if let Some(lex) =
                    embed_lexicon_for_subjects(&[term.as_str()], &merged, LEXICON_INLINE_BUDGET)
                {
                    resp["lexicon"] = lex;
                }
                return Ok(resp);
            }
        }

        // Non-guidance describe (workflow/capability/connection) — audit as
        // a successful describe regardless of whether the item resolved.
        self.emit_describe_audit(
            &id,
            None,
            workflow_id_for_audit.as_deref(),
            &principal,
            "ok",
            None,
        )
        .await;

        Ok(json!({
            "id": id,
            "item": item,
            "links": [
                { "rel": "home", "method": "praxec.query", "args": {} },
                { "rel": "search", "method": "praxec.query", "args": { "query": "" } }
            ]
        }))
    }

    /// SPEC §5.8 — emit a `guidance.describe_requested` audit record for a
    /// `gateway.describe` call. **Non-critical-path audit** (§7.3): a sink
    /// failure during emission does NOT abort the describe — the body has
    /// already been fetched and is about to be returned to the caller. The
    /// failure is observable via an `audit.write_failed` self-event so
    /// silent loss is impossible.
    pub(crate) async fn emit_describe_audit(
        &self,
        subject: &str,
        verb: Option<&str>,
        workflow_id: Option<&str>,
        principal: &Principal,
        outcome: &str,
        error_code: Option<&str>,
    ) {
        let event = AuditEvent::new("guidance.describe_requested")
            .with_actor(&principal.subject)
            .with_payload(json!({
                "subject":    subject,
                "verb":       verb,
                "workflowId": workflow_id,
                "outcome":    outcome,
                "errorCode":  error_code,
            }));
        let event = if let Some(wf_id) = workflow_id {
            event.with_workflow(wf_id)
        } else {
            event
        };
        if let Err(e) = self.runtime.audit().record(event).await {
            // Self-event so the loss is observable. If this also fails, we
            // log via tracing — last-resort but not silent.
            let self_event = AuditEvent::new("audit.write_failed")
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "originalEvent": "guidance.describe_requested",
                    "subject":       subject,
                    "error":         e.to_string(),
                }));
            if let Err(inner) = self.runtime.audit().record(self_event).await {
                tracing::warn!(
                    subject = %subject,
                    primary_err = %e,
                    selfevt_err = %inner,
                    "guidance.describe audit emission failed and self-event also failed"
                );
            }
        }
    }

    /// Shared body for the discovery-listing search tools (scripts + skills).
    /// Returns refs (`{verb, subject, title, source}`), never bodies —
    /// progressive disclosure (§5.4). Filters by verb, subject root (first
    /// dotted segment), and source (exact match); honors the `limit` clamp.
    /// Callers differ only in the [`DiscoveryKind`] they list.
    async fn list_discovery_refs(&self, kind: DiscoveryKind, args: Value) -> anyhow::Result<Value> {
        let verb_filter = args.get("verb").and_then(Value::as_str).map(str::to_string);
        let subject_root_filter = args
            .get("subject_root")
            .and_then(Value::as_str)
            .map(str::to_string);
        let source_filter = args
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(200) as usize;

        let items = self.discovery.list(Some(kind)).await?;

        let mut refs: Vec<Value> = Vec::with_capacity(items.len());
        for item in items {
            // Filter by verb (closed enum, no synonym matching).
            if let Some(want) = &verb_filter {
                if item.verb.as_deref() != Some(want.as_str()) {
                    continue;
                }
            }
            // Filter by subject root: first dotted segment.
            if let Some(want_root) = &subject_root_filter {
                let root = item.id.split('.').next().unwrap_or("");
                if root != want_root {
                    continue;
                }
            }
            // SPEC §5.3 — DiscoveryItem.source carries the fragment's
            // provenance (`config`, `git+https://...`, etc.). Filter is
            // exact match. Items without a source field never match a
            // source-filtered query.
            if let Some(want_src) = &source_filter {
                if item.source.as_deref() != Some(want_src.as_str()) {
                    continue;
                }
            }

            // Progressive-disclosure invariant: NEVER emit body content
            // in the listing.
            refs.push(json!({
                "verb":    item.verb,
                "subject": item.id,
                "title":   if item.title.is_empty() { Value::Null } else { Value::String(item.title) },
                "source":  item.source,
            }));

            if refs.len() >= limit {
                break;
            }
        }

        Ok(json!({ "items": refs }))
    }

    /// SPEC §22 — gateway.scripts.search. Mirror of [`handle_skills_search`]
    /// but lists DiscoveryKind::Script items. Same progressive-disclosure
    /// invariant: returns refs (verb, subject, source), never bodies.
    /// Bodies are fetched on demand via gateway.describe.
    pub(crate) async fn handle_scripts_search(&self, args: Value) -> anyhow::Result<Value> {
        self.list_discovery_refs(DiscoveryKind::Script, args).await
    }

    /// SPEC §17.6 — gateway.skills.search. Returns refs (`{verb, subject,
    /// hash, source?}`), never bodies (progressive disclosure, §5.4).
    /// Authoring-time only; tool is not advertised unless
    /// `with_skills_search(true)` was set on the server.
    pub(crate) async fn handle_skills_search(&self, args: Value) -> anyhow::Result<Value> {
        self.list_discovery_refs(DiscoveryKind::Guidance, args)
            .await
    }

    pub(crate) async fn handle_start(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: StartArgs = parse_args(args)?;
        // CMP-030 — require `definitionId` explicitly. The dispatch shape
        // (dispatch_command) only routes here when definition_id is present, so
        // an absent one is a fat-fingered caller, not a request for the proxy
        // default. Defaulting to the proxy workflow here would silently start
        // the wrong workflow. The proxy-default decision belongs to the
        // dispatch/store layer, not this handler.
        let definition_id = parsed
            .definition_id
            .ok_or_else(|| bad_request("definitionId is required"))?;
        let input = parsed.input.unwrap_or_else(|| json!({}));

        self.runtime
            .start(StartWorkflow {
                definition_id,
                input,
                principal,
                // SPEC §20.2 — caller-supplied trace/run propagate to every
                // audit event for this workflow. Persisted on the instance.
                trace_id: parsed.trace_id,
                run_id: parsed.run_id,
                // Top-level start: depth 0. A `kind: workflow` spawn is the
                // only path that stamps a deeper child (parent.depth + 1).
                depth: 0,
                parent: None,
            })
            .await
    }

    /// SPEC §30.10 — embed lexicon entries for the current state's skill
    /// subjects into `resp["lexicon"]`. Loads the instance snapshot to read
    /// `_lexiconLibrary` and the current state's skills list. Non-critical:
    /// a load failure (rare) silently skips enrichment.
    async fn attach_state_lexicon(&self, resp: &mut Value, workflow_id: &str) {
        if let Ok(instance) = self.runtime.load_instance(workflow_id).await {
            let snapshot_lex = instance
                .definition
                .get("_lexiconLibrary")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let merged = json!({ "_lexiconLibrary": snapshot_lex });
            let terms = lexicon_terms_for_state(&instance.definition, &instance.state);
            let term_refs: Vec<&str> = terms.iter().map(String::as_str).collect();
            if let Some(lex) =
                embed_lexicon_for_subjects(&term_refs, &merged, LEXICON_INLINE_BUDGET)
            {
                resp["lexicon"] = lex;
            }
        }
    }

    pub(crate) async fn handle_get(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: GetArgs = parse_args(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| bad_request("workflowId is required"))?;

        let mut resp = self
            .runtime
            .get(GetWorkflow {
                workflow_id: workflow_id.clone(),
                principal,
                trace_id: parsed.trace_id,
                run_id: parsed.run_id,
            })
            .await?;

        self.attach_state_lexicon(&mut resp, &workflow_id).await;

        Ok(resp)
    }

    pub(crate) async fn handle_submit(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: SubmitArgs = parse_args(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| bad_request("workflowId is required"))?;
        let expected_version = parsed
            .expected_version
            .ok_or_else(|| bad_request("expectedVersion is required"))?;
        let transition = parsed
            .transition
            .ok_or_else(|| bad_request("transition is required"))?;
        let arguments = parsed.arguments.unwrap_or_else(|| json!({}));

        self.runtime
            .submit(SubmitTransition {
                workflow_id,
                expected_version,
                transition,
                arguments,
                principal,
                summary: parsed.summary,
                trace_id: parsed.trace_id,
                run_id: parsed.run_id,
            })
            .await
    }

    pub(crate) async fn handle_explain(&self, args: Value) -> anyhow::Result<Value> {
        let parsed: ExplainArgs = parse_args(args)?;
        let workflow_id = parsed
            .workflow_id
            .ok_or_else(|| bad_request("workflowId is required"))?;
        let transition = parsed
            .transition
            .ok_or_else(|| bad_request("transition is required"))?;
        let mut resp = self.runtime.explain(&workflow_id, &transition).await?;

        self.attach_state_lexicon(&mut resp, &workflow_id).await;

        Ok(resp)
    }

    // ── SPEC §30 — Lexicon tools ──────────────────────────────────────────

    /// SPEC §30.5 — keyword search across the merged lexicon
    /// (base ∪ overlay). Substring match on term + definition.
    pub(crate) async fn handle_lexicon_search(&self, args: Value) -> anyhow::Result<Value> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let bounded_context = args
            .get("bounded_context")
            .and_then(Value::as_str)
            .map(String::from);
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let merged = self.lexicon_merged_definition();
        let hits =
            praxec_core::lexicon::search_terms(&merged, &query, bounded_context.as_deref(), limit);
        Ok(json!({ "hits": hits }))
    }

    /// SPEC §30.5 — exact term lookup. Returns the entry or null.
    pub(crate) async fn handle_lexicon_lookup(&self, args: Value) -> anyhow::Result<Value> {
        let term = args
            .get("term")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_request("lexicon.lookup requires `term`"))?
            .to_string();
        let bounded_context = args
            .get("bounded_context")
            .and_then(Value::as_str)
            .map(String::from);
        let merged = self.lexicon_merged_definition();
        let entry = praxec_core::lexicon::lookup_term(&merged, &term, bounded_context.as_deref())
            .cloned()
            .unwrap_or(Value::Null);
        Ok(json!({ "term": term, "entry": entry }))
    }

    /// SPEC §30.6 — propose / set a term. Governance-gated: agent
    /// callers writing against `human-only` terms are rejected with
    /// `LEXICON_DEFINE_REQUIRES_HUMAN`. Successful writes land in the
    /// in-memory overlay (operators persist by editing praxec.yaml).
    pub(crate) async fn handle_lexicon_define(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let term = args
            .get("term")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_request("lexicon.define requires `term`"))?
            .to_string();
        let definition = args
            .get("definition_short")
            .and_then(Value::as_str)
            .ok_or_else(|| bad_request("lexicon.define requires `definition_short`"))?;
        let bounded_context = args
            .get("bounded_context")
            .and_then(Value::as_str)
            .map(String::from);
        let refs: Option<Vec<String>> = args.get("refs").and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        });
        let governance = args
            .get("governance")
            .and_then(Value::as_str)
            .map(String::from);

        // Governance gate. If the term EXISTS in base/overlay with a
        // governance: human-only marker, agents (non-human principals)
        // must be rejected. New terms inherit the DEFAULT_GOVERNANCE
        // (human-only); agent must go through a human transition.
        //
        // Exception (SPEC §30.10.7B): when the term is a PENDING_DEFINITION
        // placeholder (i.e., it appears in `pending_subjects`), the resolver
        // is filling in a gap — not overwriting a human-curated entry. The
        // governance gate is skipped so the agent that received
        // SUBJECT_NEEDS_DEFINITION can complete the `define_new` resolution.
        let is_pending = {
            let pending = self
                .pending_subjects
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.contains(&term)
        };
        if !is_pending {
            let merged = self.lexicon_merged_definition();
            if let Err(msg) =
                praxec_core::lexicon::define_allowed(&merged, &term, principal.is_human())
            {
                return Err(anyhow::anyhow!("{msg}"));
            }
        }

        // SPEC §30.10.10 — compute embedding when a non-noop backend is wired.
        let embedding = if self.embedder.backend_name() == "noop" {
            None
        } else {
            // VER-003 — fold the entry's aliases (`refs`, parsed above) into the
            // embedded text so semantic (Tier-3) matching covers alias terms,
            // not just the canonical subject.
            let aliases_str: Vec<String> = refs.clone().unwrap_or_default();
            let text = entry_embed_text(&term, &aliases_str, definition, None);
            match self.embedder.embed(&text).await {
                Ok(vec) => Some(vec),
                Err(e) => {
                    return Ok(json!({
                        "error": {
                            "code": "EMBEDDING_BACKEND_FAILED",
                            "message": format!(
                                "embedding backend '{}' failed: {e}",
                                self.embedder.backend_name()
                            ),
                        },
                        "links": []
                    }));
                }
            }
        };

        let entry = praxec_core::lexicon::build_entry(
            definition,
            bounded_context.as_deref(),
            refs.as_ref(),
            governance.as_deref(),
            embedding,
        )?;
        {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            overlay.insert(term.clone(), entry.clone());
        }
        // SPEC §30.5 durability — persist to disk when a lexicon dir is wired,
        // so the term survives a restart instead of living only in the overlay.
        let persisted_to = match self.persist_lexicon_term(&term, &entry) {
            Ok(Some(path)) => path,
            Ok(None) => "overlay".to_string(),
            Err(e) => {
                tracing::warn!(term = %term, error = %e, "lexicon term disk persist failed; overlay only");
                "overlay".to_string()
            }
        };
        // Audit the define so operators can replay vocabulary changes.
        // CMP-036 — warn on sink failure so a lost vocabulary-change record is
        // observable rather than silently dropped.
        if let Err(e) = self
            .runtime
            .audit()
            .record(
                AuditEvent::new("lexicon.defined")
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "term":            term,
                        "bounded_context": bounded_context,
                        "by_human":        principal.is_human(),
                    })),
            )
            .await
        {
            tracing::warn!(term = %term, error = %e, "lexicon.defined audit write failed");
        }
        Ok(json!({ "term": term, "entry": entry, "persisted_to": persisted_to }))
    }

    // ── SPEC §32 — shape-routing dispatchers ─────────────────────────────────

    /// Shape-route a `praxec.query` call to the appropriate handler.
    /// See SPEC §32 for the dispatch table.
    ///
    /// Dispatch table (first match wins):
    /// - `(none)`               → home
    /// - `observe: true` alone  → observe (bounded audit-stream replay)
    /// - `query` present        → search
    /// - `subject` only         → describe (browse-time, no audit)
    /// - `subject + workflowId` → describe-in-workflow (audit fires)
    /// - `workflowId + transition` → explain
    /// - `workflowId` alone     → get
    /// - anything else          → AMBIGUOUS_INTENT error
    pub async fn dispatch_query(&self, args: Value, principal: Principal) -> anyhow::Result<Value> {
        let parsed: QueryArgs = parse_args(args.clone())?;
        let q = parsed.query.is_some();
        let s = parsed.subject.is_some();
        let wid = parsed.workflow_id.is_some();
        let tr = parsed.transition.is_some();
        let d = parsed.definition_id.is_some();

        // Observe: `observe: true` is an exclusive shape (its only modifiers
        // are `since` and `limit`); mixed with any other intent field it's
        // ambiguous. `since` without `observe: true` is likewise rejected
        // rather than silently ignored (a no-op arg would read as a filter
        // that "worked").
        let ob = parsed.observe.unwrap_or(false);
        if ob && (q || s || wid || tr || d) {
            return Ok(ambiguous_intent_query());
        }
        if parsed.since.is_some() && !ob {
            return Ok(ambiguous_intent_query());
        }
        if ob {
            return self
                .handle_observe(parsed.since.as_deref(), parsed.limit)
                .await;
        }

        // Approvals: `approvals: true` is an exclusive shape (no modifiers) — the
        // MCP-native HITL queue. Mixed with any other intent field it's ambiguous.
        let ap = parsed.approvals.unwrap_or(false);
        if ap && (q || s || wid || tr || d || parsed.since.is_some()) {
            return Ok(ambiguous_intent_query());
        }
        if ap {
            return self.handle_approvals().await;
        }

        // Detect ambiguity: `query` (search intent) alongside subject/workflow
        // fields (describe/get/explain intent) is unresolvable.
        if q && (s || wid || tr) {
            return Ok(ambiguous_intent_query());
        }

        // Read-definition: `definitionId` alone returns the definition's body +
        // content hash (the edit basis, SPEC §8.4). Mixed with any other intent
        // field it's ambiguous.
        if d {
            if q || s || wid || tr {
                return Ok(ambiguous_intent_query());
            }
            let def_id = parsed.definition_id.expect("d is true");
            return self.handle_read_definition(&def_id).await;
        }

        match (q, s, wid, tr) {
            (false, false, false, false) => self.handle_home().await,
            (true, false, false, false) => {
                // Search: pass through only the search-relevant fields.
                // Omit null optionals so SearchArgs default kicks in for
                // `limit` (which has a `#[serde(default)]` but not Option).
                let mut search_args = serde_json::Map::new();
                if let Some(qv) = parsed.query {
                    search_args.insert("query".into(), Value::String(qv));
                }
                if let Some(k) = parsed.kind {
                    search_args.insert("kind".into(), Value::String(k));
                }
                if let Some(l) = parsed.limit {
                    search_args.insert("limit".into(), json!(l));
                }
                self.handle_search(Value::Object(search_args)).await
            }
            (false, true, false, false) => {
                // Browse-time describe: reshape subject → id.
                let describe_args = json!({
                    "id": parsed.subject,
                });
                self.handle_describe(describe_args, principal).await
            }
            (false, true, true, false) => {
                // Describe-in-workflow: subject + workflowId → audit fires.
                let describe_args = json!({
                    "id":         parsed.subject,
                    "workflowId": parsed.workflow_id,
                });
                self.handle_describe(describe_args, principal).await
            }
            (false, false, true, true) => {
                // Explain: workflowId + transition.
                let explain_args = json!({
                    "workflowId": parsed.workflow_id,
                    "transition": parsed.transition,
                });
                self.handle_explain(explain_args).await
            }
            (false, false, true, false) => {
                // Get: workflowId alone.
                let get_args = json!({
                    "workflowId": parsed.workflow_id,
                });
                self.handle_get(get_args, principal).await
            }
            _ => Ok(ambiguous_intent_query()),
        }
    }

    /// SPEC §8.4 — read a definition's current body + content hash. The read
    /// half of authoring: feed `definition` back as the edit workflow's
    /// `baseDefinition`, and the runtime's optimistic-concurrency guard checks
    /// `hash` is still current at publish.
    async fn handle_read_definition(&self, definition_id: &str) -> anyhow::Result<Value> {
        let definition = self
            .runtime
            .load_definition(definition_id)
            .await
            .map_err(|e| bad_request(format!("DEFINITION_NOT_FOUND: '{definition_id}': {e}")))?;
        let hash = praxec_core::config::compute_definition_hash(&definition);
        Ok(json!({
            "resource": { "type": "definition", "id": definition_id },
            "result": { "status": "ok" },
            "definitionId": definition_id,
            "definition": definition,
            "hash": hash,
            "links": [
                { "rel": "home", "method": "praxec.query", "args": {} },
                {
                    "rel": "edit",
                    "title": "Propose an edit to this definition",
                    "method": "praxec.command",
                    "args": { "definitionId": "authoring-edit", "input": {} }
                }
            ]
        }))
    }

    /// L1 observability — the `praxec.query { observe: true }` read: a bounded
    /// replay of the structured audit event stream (the pull complement to the
    /// CLI `observe --follow` tail — an MCP call returns a response, not an
    /// infinite stream). Reuses the file sink's dir-glob + newline-parse
    /// reader ([`AuditSink::try_list_events`]): rotated / per-writer files
    /// merged, heartbeat pulses excluded, timestamp-ordered. Fails fast with
    /// the same rich error as the CLI when the sink is not `file`
    /// (`praxec_core::audit::require_file_sink`).
    ///
    /// [`AuditSink::try_list_events`]: praxec_core::audit::AuditSink::try_list_events
    pub(crate) async fn handle_observe(
        &self,
        since: Option<&str>,
        limit: Option<u64>,
    ) -> anyhow::Result<Value> {
        let sink = self.runtime.audit();
        if let Err(e) = praxec_core::audit::require_file_sink(sink.sink_kind(), "the observe query")
        {
            // Structured 4xx-class response (per the AMBIGUOUS_INTENT /
            // LEXICON_WRITES_DISABLED pattern) so HATEOAS links stay
            // machine-parseable.
            return Ok(json!({
                "error": {
                    "code": "OBSERVE_REQUIRES_FILE_SINK",
                    "message": e.to_string(),
                    "hint": "Set `audit.sink: file` and `audit.path: <dir>` in the gateway config, then reload."
                },
                "links": [
                    { "rel": "home", "method": "praxec.query", "args": {} }
                ]
            }));
        }

        let floor = since
            .map(|s| {
                s.parse::<chrono::DateTime<chrono::Utc>>().map_err(|e| {
                    bad_request(format!(
                        "invalid `since` (want RFC3339, e.g. 2026-07-10T12:00:00Z): {e}"
                    ))
                })
            })
            .transpose()?;

        // A genuine directory-read failure PROPAGATES (internal error); a
        // missing/empty audit dir is an empty window, not an error.
        let mut events = sink.try_list_events().await?.unwrap_or_default();
        if let Some(floor) = floor {
            // Same `timestamp >= since` floor as the CLI `--since`: the cursor
            // event itself is re-included — consumers dedupe by event `id`.
            events.retain(|e| e.timestamp >= floor);
        }

        let total = events.len();
        let limit = limit.unwrap_or(DEFAULT_OBSERVE_LIMIT) as usize;
        let truncated = total > limit;
        // Window selection. Events are ascending by timestamp across ALL retained
        // logs (weeks of history). With NO `since` cursor the caller means "what's
        // happening now" — so default to the TAIL (the most recent `limit`),
        // NOT the oldest. Truncating from the front returned a 3-week-old window
        // (the first `server.initialized` from a long-dead session), which is why
        // `observe:true` looked stuck on stale events. With a `since` cursor the
        // caller is paging FORWARD, so keep oldest-first-from-floor. Either way
        // the returned window stays ascending, and `next_since` = its newest
        // timestamp lets a re-query pull only newer events (live tail).
        if since.is_none() && total > limit {
            events.drain(0..total - limit);
        } else {
            events.truncate(limit);
        }
        let next_since = events.last().map(|e| e.timestamp.to_rfc3339());

        let mut links = vec![json!({ "rel": "home", "method": "praxec.query", "args": {} })];
        if let Some(cursor) = &next_since {
            links.push(json!({
                "rel": "observe_next",
                "title": "Poll the next window (pull-tail): events at/after this cursor",
                "method": "praxec.query",
                "args": { "observe": true, "since": cursor }
            }));
        }

        Ok(json!({
            "resource": { "type": "observe", "id": "audit-events" },
            "result": { "status": "ok" },
            "count": events.len(),
            "truncated": truncated,
            "next_since": next_since,
            "events": events,
            "note": "Bounded replay window — an MCP call returns a response, not a stream. \
                     This is the pull complement to `praxec observe --follow`: re-query with \
                     since=next_since to tail. Rebuild the execution tree from workflow_id + \
                     parent_workflow_id + depth. Event schema: `praxec schema audit-event`.",
            "links": links
        }))
    }

    /// MCP-native HITL discovery: the store-derived queue of every live mission
    /// parked awaiting a human — an `actor: human` approval gate or an agent's
    /// elicitation. The pull/fallback complement to the push path (server-issued
    /// `elicitation/create`): a human driving through an agent lists what needs
    /// them and gets a ready-to-fire resolve link per gate. Also what a client
    /// that does NOT advertise the `elicitation` capability falls back to.
    pub(crate) async fn handle_approvals(&self) -> anyhow::Result<Value> {
        let gates = self.runtime.list_pending_human().await?;
        let items: Vec<Value> = gates
            .iter()
            .map(|g| {
                let mut item = serde_json::to_value(g).unwrap_or_else(|_| json!({}));
                // A ready-to-fire resolve call. It is an `actor: human` gate, so
                // the submit MUST carry a human-origin identity — otherwise the
                // runtime rejects it ACTOR_MISMATCH. Say so, and name the channel.
                if let Some(obj) = item.as_object_mut() {
                    obj.insert(
                        "resolve".into(),
                        json!({
                            "rel": "resolve_human_gate",
                            "method": "praxec.command",
                            "args": {
                                "workflowId": g.workflow_id,
                                "expectedVersion": g.expected_version,
                                "transition": g.transition,
                            },
                            "requiresHuman": true,
                            "note": "This is an actor:human gate. The submit must carry a \
                                     human-origin identity — the CLI `--human` flag, or the MCP \
                                     `_meta` principal claim `io.praxec/principal`. An anonymous \
                                     agent submit is rejected ACTOR_MISMATCH."
                        }),
                    );
                }
                item
            })
            .collect();

        Ok(json!({
            "resource": { "type": "approvals", "id": "pending-human" },
            "result": { "status": "ok" },
            "count": items.len(),
            "pending": items,
            "note": "Missions parked awaiting a human (oldest-first). A client that advertises \
                     the `elicitation` capability is instead prompted in-line via \
                     `elicitation/create` when a `praxec.command` hits a gate; this list is the \
                     pull complement for finding gates parked out-of-band.",
            "links": [ { "rel": "home", "method": "praxec.query", "args": {} } ]
        }))
    }

    /// Shape-route a `praxec.command` call to the appropriate handler.
    /// See SPEC §32 for the dispatch table.
    ///
    /// Dispatch table (exclusive shapes):
    /// - `definitionId` only (no workflowId, no subject)                           → start
    /// - `workflowId + transition + expectedVersion` (no subject)                   → submit
    /// - `subject` with `:` namespace + `definition` (no workflowId, no definitionId) → define
    /// - `intent == "cancel_pending_subject"` + `unknown_subject`                   → cancel
    /// - anything else                                                               → AMBIGUOUS_INTENT
    pub async fn dispatch_command(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: CommandArgs = parse_args(args.clone())?;

        let is_start = parsed.definition_id.is_some()
            && parsed.workflow_id.is_none()
            && parsed.subject.is_none();
        let is_submit = parsed.workflow_id.is_some()
            && parsed.transition.is_some()
            && parsed.expected_version.is_some()
            && parsed.subject.is_none();
        let is_define = parsed.subject.as_deref().is_some_and(|s| s.contains(':'))
            && parsed.definition.is_some()
            && parsed.workflow_id.is_none()
            && parsed.definition_id.is_none();
        let is_cancel = parsed.intent.as_deref() == Some("cancel_pending_subject")
            && parsed.unknown_subject.is_some();

        // T24 — cancel a RUNNING WORKFLOW (distinct from the lexicon
        // `cancel_pending_subject` above). Wire shape:
        // `{ "intent": "cancel", "workflowId": "wf_…", "summary"?: "<reason>" }`.
        // This is the operator's server-side reap: a run whose CLI/driver died
        // leaves a durable `running` instance in the store; without a working
        // cancel verb it orphans a zombie (killing the CLI does not cancel
        // server-side). Exclusive shape: it carries a `workflowId` but no
        // `transition` (so it can't be a submit) and a distinct intent.
        let is_cancel_workflow = parsed.intent.as_deref() == Some("cancel")
            && parsed.workflow_id.is_some()
            && parsed.transition.is_none();
        if is_cancel_workflow {
            let workflow_id = parsed.workflow_id.clone().expect("checked above");
            let reason = parsed.summary.clone().unwrap_or_else(|| {
                format!("cancelled via praxec.command by {}", principal.subject)
            });
            return self.handle_cancel_workflow(&workflow_id, &reason).await;
        }

        match (is_start, is_submit, is_define, is_cancel) {
            (true, false, false, false) => {
                // Start: reshape CommandArgs → StartArgs wire shape.
                let start_args = json!({
                    "definitionId": parsed.definition_id,
                    "input":        parsed.input,
                    "traceId":      parsed.trace_id,
                    "runId":        parsed.run_id,
                });
                self.handle_start(start_args, principal).await
            }
            (false, true, false, false) => {
                // Submit: reshape CommandArgs → SubmitArgs wire shape.
                let submit_args = json!({
                    "workflowId":      parsed.workflow_id,
                    "expectedVersion": parsed.expected_version,
                    "transition":      parsed.transition,
                    "arguments":       parsed.arguments,
                    "summary":         parsed.summary,
                    "traceId":         parsed.trace_id,
                    "runId":           parsed.run_id,
                });
                self.handle_submit(submit_args, principal).await
            }
            (false, false, true, false) => self.dispatch_lexicon_define(args, principal).await,
            (false, false, false, true) => {
                // Cancel pending subject placeholder.
                let subject = parsed.unknown_subject.expect("checked above");
                self.handle_cancel_pending_subject(&subject, principal)
                    .await
            }
            _ => Ok(ambiguous_intent_command()),
        }
    }

    /// T24 — cancel a running workflow (see the `is_cancel_workflow` shape in
    /// [`dispatch_command`]). Delegates to [`WorkflowRuntime::cancel`]: sets
    /// `cancelled_at` + `cancelled_reason`, bumps the version, and emits a
    /// `workflow.cancelled` audit event; subsequent `submit`s return
    /// `WORKFLOW_CANCELLED` and `get` surfaces `status: "cancelled"`.
    /// Idempotent (re-cancelling is a no-op). A missing workflow surfaces the
    /// store's not-found error.
    ///
    /// [`dispatch_command`]: Self::dispatch_command
    /// [`WorkflowRuntime::cancel`]: praxec_core::runtime::WorkflowRuntime::cancel
    async fn handle_cancel_workflow(
        &self,
        workflow_id: &str,
        reason: &str,
    ) -> anyhow::Result<Value> {
        self.runtime.cancel(workflow_id, reason).await?;
        Ok(json!({
            "workflowId": workflow_id,
            "status": "cancelled",
            "reason": reason,
        }))
    }

    /// Shim: extract `<term>` from `subject: "lexicon:<term>"` and delegate
    /// to the appropriate handler. Detects `aliases_add` in the definition
    /// body (SPEC §30.10.7A) and routes to `handle_alias_add`; otherwise
    /// falls through to the normal define path. Other subject namespaces
    /// (`script:`, `workflow:`, `skill:`) are reserved but have no writable
    /// primitive today — they return AMBIGUOUS_INTENT.
    async fn dispatch_lexicon_define(
        &self,
        args: Value,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        let parsed: CommandArgs = parse_args(args)?;
        let subject = parsed.subject.as_deref().unwrap_or("");
        match parse_subject_namespace(subject) {
            (Some("lexicon"), term) => {
                let def_obj = parsed.definition.as_ref();

                // SPEC §30.10.7A — alias-add path: definition carries
                // `aliases_add` array, not `definition_short`.
                if let Some(aliases_add) = def_obj
                    .and_then(|d| d.get("aliases_add"))
                    .and_then(Value::as_array)
                {
                    let aliases: Vec<String> = aliases_add
                        .iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect();
                    return self.handle_alias_add(term, &aliases, principal).await;
                }

                // Normal define path (define_new).
                // handle_lexicon_define expects: { term, definition_short (string),
                // bounded_context?, refs?, governance? }.
                // CommandArgs.definition is an object with primary field
                // `definition_short` (SPEC §30.10.1).
                //   { definition_short: "...", boundedContext: "...", refs: [...], governance: "..." }
                // CMP-014 — a missing `definition_short` must not write an
                // empty lexicon entry. Mirror handle_lexicon_define (~670):
                // surface it as invalid_params instead of silently defaulting
                // to "".
                let definition_str = def_obj
                    .and_then(|d| d.get("definition_short"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        bad_request("lexicon define requires `definition.definition_short`")
                    })?;
                let bounded_context = def_obj
                    .and_then(|d| d.get("boundedContext"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let refs = def_obj
                    .and_then(|d| d.get("refs"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let governance = def_obj
                    .and_then(|d| d.get("governance"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let reshape = json!({
                    "term":             term,
                    "definition_short": definition_str,
                    "bounded_context":  bounded_context,
                    "refs":             refs,
                    "governance":       governance,
                });
                let result = self.handle_lexicon_define(reshape, principal).await?;
                // SPEC §30.10.7B — if this was a PENDING_DEFINITION subject,
                // remove it from the pending set now that it has a real entry.
                {
                    let mut pending = self
                        .pending_subjects
                        .write()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    pending.remove(term);
                }
                Ok(result)
            }
            _ => Ok(ambiguous_intent_command()),
        }
    }

    /// SPEC §30.10.7A — add one or more aliases to an existing lexicon entry.
    ///
    /// Checks for same-bounded-context collision across the full overlay+base.
    /// On success, appends aliases to the entry in the overlay, removes any
    /// of the added aliases from the pending-subjects set, and emits
    /// `lexicon.alias_added` per alias.
    async fn handle_alias_add(
        &self,
        target_term: &str,
        aliases_to_add: &[String],
        principal: Principal,
    ) -> anyhow::Result<Value> {
        // Load the current entry for the target term.
        let merged = self.lexicon_merged_definition();
        let existing = merged
            .get("_lexiconLibrary")
            .and_then(Value::as_object)
            .and_then(|lib| lib.get(target_term))
            .cloned();
        let mut entry = match existing {
            Some(e) if e.get("state").and_then(Value::as_str) != Some("PENDING_DEFINITION") => {
                // Real entry — proceed.
                e.as_object().cloned().unwrap_or_default()
            }
            _ => {
                return Ok(json!({
                    "error": {
                        "code": "LEXICON_ENTRY_NOT_FOUND",
                        "message": format!(
                            "LEXICON_ENTRY_NOT_FOUND: no real entry for term '{target_term}'. \
                             link_as_alias requires an existing authored entry as target."
                        ),
                        "hint": "Use define_new to create the target term first."
                    }
                }));
            }
        };

        // Collision check: build the combined index for the target's bounded
        // context and verify none of the new aliases appear there already.
        let lib = merged
            .get("_lexiconLibrary")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let target_ctx = entry
            .get("bounded_context")
            .and_then(Value::as_str)
            .unwrap_or("");
        match praxec_core::lexicon::build_combined_index(&lib, target_ctx) {
            Err(collision_msg) => {
                // Collision already exists in the index — check if any of our
                // new aliases would conflict. Rerun with candidate aliases
                // added to a scratch map.
                return Ok(json!({
                    "error": {
                        "code": "LEXICON_ALIAS_COLLISION",
                        "message": collision_msg.to_string(),
                    }
                }));
            }
            Ok(index) => {
                // Check each new alias against the existing index.
                for alias in aliases_to_add {
                    if let Some(existing_entry) = index.get(alias.as_str()) {
                        // Alias is already taken by a term in this context.
                        let owner = existing_entry
                            .get("definition_short")
                            .and_then(Value::as_str)
                            .unwrap_or("?");
                        let _ = owner;
                        return Ok(json!({
                            "error": {
                                "code": "LEXICON_ALIAS_COLLISION",
                                "message": format!(
                                    "LEXICON_ALIAS_COLLISION: within bounded_context \
                                     '{target_ctx}', key '{alias}' is already claimed. \
                                     Aliases must be unique within a bounded context. \
                                     (SPEC §30.10.1)"
                                ),
                            }
                        }));
                    }
                }
            }
        }

        // Append aliases to the entry.
        let current_aliases = entry
            .get("aliases")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut new_aliases = current_aliases;
        for alias in aliases_to_add {
            let v = serde_json::Value::String(alias.clone());
            if !new_aliases.contains(&v) {
                new_aliases.push(v);
            }
        }
        let new_aliases_strings: Vec<String> = new_aliases
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        entry.insert("aliases".to_string(), serde_json::Value::Array(new_aliases));

        // SPEC §30.10.10 — re-embed when a non-noop backend is configured.
        // The embed text includes aliases (per entry_embed_text), so adding an
        // alias changes the text and the stored vector would go stale.
        if self.embedder.backend_name() != "noop" {
            let definition_short = entry
                .get("definition_short")
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = entry_embed_text(target_term, &new_aliases_strings, definition_short, None);
            match self.embedder.embed(&text).await {
                Ok(vec) => {
                    entry.insert("_embedding".to_string(), json!(vec));
                }
                Err(e) => {
                    return Ok(json!({
                        "error": {
                            "code": "EMBEDDING_BACKEND_FAILED",
                            "message": format!(
                                "embedding backend '{}' failed during alias re-embed: {e}",
                                self.embedder.backend_name()
                            ),
                        },
                        "links": []
                    }));
                }
            }
        }

        // Persist into the overlay.
        {
            let mut overlay = self
                .lexicon_overlay
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            overlay.insert(target_term.to_string(), serde_json::Value::Object(entry));
        }

        // Remove added aliases from pending-subjects set and emit audit events.
        {
            let mut pending = self
                .pending_subjects
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for alias in aliases_to_add {
                pending.remove(alias.as_str());
            }
        }
        for alias in aliases_to_add {
            // CMP-036 — warn on sink failure so a lost alias-change record is
            // observable rather than silently dropped.
            if let Err(e) = self
                .runtime
                .audit()
                .record(
                    AuditEvent::new("lexicon.alias_added")
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "term":      target_term,
                            "alias":     alias,
                            "principal": principal.subject,
                        })),
                )
                .await
            {
                tracing::warn!(
                    term = %target_term,
                    alias = %alias,
                    error = %e,
                    "lexicon.alias_added audit write failed"
                );
            }
        }

        Ok(json!({
            "term":    target_term,
            "aliases": aliases_to_add,
            "persisted_to": "overlay"
        }))
    }

    /// SPEC §30.10.7C — drop a PENDING_DEFINITION placeholder without creating
    /// or modifying a lexicon entry. Returns INVALID_RESOLUTION when the
    /// named subject is not in the known pending set (i.e., it is a real
    /// authored entry or unknown). Emits `lexicon.pending_cancelled`.
    async fn handle_cancel_pending_subject(
        &self,
        subject: &str,
        principal: Principal,
    ) -> anyhow::Result<Value> {
        // Check: the subject must be in the pending set.
        let was_pending = {
            let pending = self
                .pending_subjects
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.contains(subject)
        };

        if !was_pending {
            return Ok(json!({
                "error": {
                    "code": "INVALID_RESOLUTION",
                    "message": format!(
                        "INVALID_RESOLUTION: subject '{subject}' is not a pending \
                         placeholder. Cancel applies only to PENDING_DEFINITION \
                         subjects. (SPEC §30.10.9)"
                    ),
                    "hint": "Use praxec.query to inspect the lexicon entry."
                }
            }));
        }

        // Remove from pending set.
        {
            let mut pending = self
                .pending_subjects
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.remove(subject);
        }

        // Emit audit event.
        // CMP-036 — warn on sink failure so a lost cancellation record is
        // observable rather than silently dropped.
        if let Err(e) = self
            .runtime
            .audit()
            .record(
                AuditEvent::new("lexicon.pending_cancelled")
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "term":         subject,
                        "cancelled_by": principal.subject,
                    })),
            )
            .await
        {
            tracing::warn!(subject = %subject, error = %e, "lexicon.pending_cancelled audit write failed");
        }

        Ok(json!({
            "cancelled":   subject,
            "persisted_to": "pending_subjects"
        }))
    }

    /// Build a synthetic "workflow definition" carrying the merged
    /// `_lexiconLibrary` so the core `lookup_term` / `search_terms`
    /// helpers (which expect a workflow-definition shape) can be
    /// reused without duplication. Also used by `dispatch_call` to
    /// supply the lexicon snapshot for candidate ranking in
    /// `SUBJECT_NEEDS_DEFINITION` responses.
    pub(crate) fn lexicon_merged_definition(&self) -> Value {
        let base = self.lexicon_base.as_object().cloned().unwrap_or_default();
        let overlay_clone = {
            let overlay = self
                .lexicon_overlay
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            overlay.clone()
        };
        let mut merged = base;
        for (k, v) in overlay_clone {
            merged.insert(k, v);
        }
        json!({ "_lexiconLibrary": merged })
    }
}

// ── §32 dispatch helpers ──────────────────────────────────────────────────────

/// Parse a cross-primitive subject namespace per §32.
///
/// `"lexicon:churn"` → `(Some("lexicon"), "churn")`
/// `"swe_agent"` → `(None, "swe_agent")`
pub(crate) fn parse_subject_namespace(s: &str) -> (Option<&str>, &str) {
    match s.split_once(':') {
        Some((ns, term)) => (Some(ns), term),
        None => (None, s),
    }
}

/// Structured AMBIGUOUS_INTENT response body for `praxec.query` dispatch.
/// Per SPEC §32, this is a 4xx-class structured response — NOT an MCP
/// protocol error — so HATEOAS links remain machine-parseable by clients.
fn ambiguous_intent_query() -> Value {
    json!({
        "error": {
            "code": "AMBIGUOUS_INTENT",
            "message": "praxec.query args do not match a known dispatch shape",
            "hint": "see §32 dispatch table: home (no args), search (query), describe (subject), get (workflowId), explain (workflowId+transition), describe-in-workflow (subject+workflowId), read-definition (definitionId), observe (observe:true, optional since/limit)"
        },
        "links": [
            { "rel": "home",   "method": "praxec.query", "args": {} },
            { "rel": "search", "method": "praxec.query", "args": { "query": "" } }
        ]
    })
}

/// Structured `RUN_ID_ALREADY_RUNNING` response body for `praxec.command`
/// start. Per SPEC §32, this is a 4xx-class structured response — NOT an MCP
/// protocol error — so HATEOAS links remain machine-parseable by clients.
///
/// The `get` link points directly to the existing workflow instance so the
/// caller can resume or introspect without a second lookup.
pub(crate) fn run_id_already_running(run_id: &str, existing_workflow_id: &str) -> Value {
    json!({
        "error": {
            "code": "RUN_ID_ALREADY_RUNNING",
            "message": format!("An instance already exists with run_id '{run_id}'."),
            "hint": "Each run_id is single-use. Fetch the existing instance with the linked get, or retry with a fresh run_id."
        },
        "links": [
            {
                "rel": "get",
                "method": "praxec.query",
                "args": { "workflowId": existing_workflow_id }
            }
        ]
    })
}

/// SPEC §30.10.5 — structured SUBJECT_NEEDS_DEFINITION interaction response.
///
/// Returned when `WorkflowRuntime::start` detects a `PENDING_DEFINITION`
/// placeholder in the workflow's `_lexiconLibrary`. The workflow instance is
/// NOT created. The original tool-call args are echoed back verbatim as
/// `queued_command.args` so the resolver can retry unchanged after defining the
/// subject.
///
/// Three HATEOAS links guide resolution:
///
/// - `link_as_alias`  — link the unknown subject as a synonym for an existing term.
/// - `define_new`     — add a new first-class lexicon entry.
/// - `cancel`         — abandon the original command.
///
/// `merged_definition` is the synthetic `{ _lexiconLibrary: … }` value from
/// `PraxecServer::lexicon_merged_definition`. It is used to rank Tier 1/2/3/4
/// candidates (SPEC §30.10.10.4) — exact canonical, exact alias, semantic
/// (Tier 3), Levenshtein fuzzy ≤ 2. Pass `None` (or an empty object) to receive
/// an empty candidates array (backward-compatible fallback).
///
/// `embedder` — when `Some`, Tier 3 semantic ranking fires via
/// `rank_candidates_with_embedding`. Pass `None` or a `NoopEmbedder` to skip.
pub(crate) async fn subject_needs_definition(
    unknown_subject: &str,
    bounded_context: Option<&str>,
    workflow_id_context: &str,
    queued_args: &Value,
    merged_definition: Option<&Value>,
    embedder: Option<&dyn EmbeddingProvider>,
) -> Value {
    let lexicon_subject = format!("lexicon:{unknown_subject}");

    // Compute candidates from Tier 1, 2, 3 (if embedder), 4.
    let candidates: serde_json::Value = match merged_definition {
        Some(def) => {
            let ranked =
                praxec_core::lexicon_candidates::rank_candidates_from_definition_with_embedding(
                    unknown_subject,
                    def,
                    bounded_context,
                    embedder,
                )
                .await;
            praxec_core::lexicon_candidates::candidates_to_json(&ranked)
        }
        None => serde_json::Value::Array(vec![]),
    };

    json!({
        "interaction": {
            "kind": "SUBJECT_NEEDS_DEFINITION",
            "unknown_subject": unknown_subject,
            "context": {
                "encountered_in": workflow_id_context,
                "bounded_context": bounded_context
            },
            "candidates": candidates
        },
        "queued_command": {
            "method": "praxec.command",
            "args": queued_args
        },
        "links": [
            {
                "rel": "link_as_alias",
                "method": "praxec.command",
                "args": {
                    "subject": lexicon_subject,
                    "definition": { "aliases_add": [unknown_subject] }
                },
                "hint": "Use this if the unknown subject is a synonym for an existing term."
            },
            {
                "rel": "define_new",
                "method": "praxec.command",
                "args": {
                    "subject": lexicon_subject,
                    "definition": {
                        "definition_short": "<fill in>",
                        "boundedContext": bounded_context
                    }
                },
                "hint": "Use this if the unknown subject is a genuinely new concept."
            },
            {
                "rel": "cancel",
                "method": "praxec.command",
                "args": {
                    "intent": "cancel_pending_subject",
                    "unknown_subject": unknown_subject
                },
                "hint": "Abandon the original command — the subject was a mistake."
            }
        ]
    })
}

// ── SPEC §30.10 lexicon-embedding helpers ─────────────────────────────────────

/// SPEC §30.10 — inline budget per entry (bytes of `definition_short`).
const LEXICON_INLINE_BUDGET: usize = 200;

/// SPEC §30.10 — build the `lexicon` field for describe/get/explain responses.
///
/// `terms` is the list of lexicon term names (e.g. `["change-request"]`, NOT
/// full `verb.subject` keys). For each term:
///
/// - Skip `PENDING_DEFINITION` placeholders entirely.
/// - If `definition_short` fits within `budget` bytes → inline as
///   `{definition_short: "..."}`.
/// - Otherwise → compact shape with a `lookup_link` so the caller can fetch
///   on demand, plus a `hash` for cache-busting:
///   `{hash: "sha256:...", lookup_link: {rel: "lexicon", method: "praxec.query", args: {subject: "lexicon:<term>"}}}`.
///
/// Returns `None` when no terms have an entry in the lexicon (so callers can
/// omit the `lexicon` field entirely rather than emitting `lexicon: {}`).
pub(crate) fn embed_lexicon_for_subjects(
    terms: &[&str],
    merged_def: &Value,
    budget: usize,
) -> Option<Value> {
    let lib = merged_def
        .get("_lexiconLibrary")
        .and_then(Value::as_object)?;

    let mut out = serde_json::Map::new();

    for &term in terms {
        let Some(entry) = lib.get(term) else { continue };
        // Skip placeholders — they have no definition to embed.
        if entry.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION") {
            continue;
        }
        let definition_short = entry
            .get("definition_short")
            .and_then(Value::as_str)
            .unwrap_or("");
        if definition_short.len() <= budget {
            out.insert(
                term.to_string(),
                json!({ "definition_short": definition_short }),
            );
        } else {
            // Over budget — emit hash + lookup_link.
            let hash = praxec_core::contract_hash::compute_contract_hash(entry);
            out.insert(
                term.to_string(),
                json!({
                    "hash": hash,
                    "lookup_link": {
                        "rel": "lexicon",
                        "method": "praxec.query",
                        "args": { "subject": format!("lexicon:{term}") }
                    }
                }),
            );
        }
    }

    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

/// Extract the post-first-dot subject portion from a `verb.subject[.etc]` key.
///
/// `"plan.change-request"` → `"change-request"`
/// `"review.style.house-voice"` → `"style.house-voice"`
/// `"no-dot"` → `"no-dot"` (no dot: return as-is; config validation already
/// enforces `verb.subject` form for real skill keys)
fn subject_portion_from_skill_key(key: &str) -> &str {
    match key.find('.') {
        Some(idx) => &key[idx + 1..],
        None => key,
    }
}

/// Collect the full set of lexicon terms to embed for a `describe` response.
///
/// Starting from `primary_term` (the subject portion of the described id),
/// walks the entry's `refs` field in the merged lexicon and returns the
/// combined list: `[primary_term, ref1, ref2, ...]`. Unknown refs are silently
/// dropped (they'll just be absent from the embedded lexicon). Deduplicates.
pub(crate) fn collect_describe_terms(primary_term: &str, merged_def: &Value) -> Vec<String> {
    let mut terms = vec![primary_term.to_string()];
    let lib = merged_def.get("_lexiconLibrary").and_then(Value::as_object);
    if let Some(lib) = lib {
        if let Some(entry) = lib.get(primary_term) {
            if let Some(refs) = entry.get("refs").and_then(Value::as_array) {
                for r in refs {
                    if let Some(ref_term) = r.as_str() {
                        if !terms.contains(&ref_term.to_string()) {
                            terms.push(ref_term.to_string());
                        }
                    }
                }
            }
        }
    }
    terms
}

/// Collect lexicon term names for the skills referenced in `state_name` of
/// `definition`. Returns a `Vec<String>` of term names (post-first-dot portion
/// of each full `verb.subject` skill key). Used by the get and explain
/// embedding paths to identify which lexicon entries are in scope.
pub(crate) fn lexicon_terms_for_state(definition: &Value, state_name: &str) -> Vec<String> {
    let state_path = format!("/states/{}", pointer_escape(state_name));
    let state_def = match definition.pointer(&state_path) {
        Some(s) => s,
        None => return vec![],
    };
    let Some(skills) = state_def.get("skills").and_then(Value::as_array) else {
        return vec![];
    };
    skills
        .iter()
        .filter_map(Value::as_str)
        .map(|key| subject_portion_from_skill_key(key).to_string())
        .collect()
}

/// Use [`pointer_escape`] from the runtime_links module via the re-exported
/// helper in praxec_core. (The function is `pub(crate)` there so we
/// replicate the minimal escaping logic here rather than adding a new public
/// export just for this one-liner.)
fn pointer_escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// Structured AMBIGUOUS_INTENT response body for `praxec.command` dispatch.
/// Per SPEC §32, this is a 4xx-class structured response — NOT an MCP
/// protocol error — so HATEOAS links remain machine-parseable by clients.
fn ambiguous_intent_command() -> Value {
    json!({
        "error": {
            "code": "AMBIGUOUS_INTENT",
            "message": "praxec.command args do not match a known dispatch shape",
            "hint": "see §32 dispatch table: start (definitionId only), submit (workflowId+expectedVersion+transition), define (subject namespaced + definition)"
        },
        "links": [
            { "rel": "start_example",  "method": "praxec.command", "args": { "definitionId": "<your-workflow>" } },
            { "rel": "submit_example", "method": "praxec.command", "args": { "workflowId": "<id>", "expectedVersion": 0, "transition": "<name>" } },
            { "rel": "define_example", "method": "praxec.command", "args": { "subject": "lexicon:<term>", "definition": { "definition_short": "..." } } }
        ]
    })
}
