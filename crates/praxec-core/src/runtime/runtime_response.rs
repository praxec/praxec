//! Response-builder and guard-evaluation helpers for [`WorkflowRuntime`].
//! Methods stay on the same `impl WorkflowRuntime` block split across
//! sibling files — see `runtime.rs` for the type definition and lifecycle
//! entry points (`start`, `submit`, `get`).

use serde_json::{json, Value};

use crate::error::ExecutorError;
use crate::mission::{derive_mission_status, StatusHint, TerminalOutcome};
use crate::model::{Principal, WorkflowInstance};
use crate::runtime::runtime_links::{
    collect_guidance_refs, is_terminal, link_filter_byguards, links, pointer_escape,
    transition_definition,
};
use crate::runtime::WorkflowRuntime;
use crate::templating::render_template;

/// SPEC §9 + §20.4 — outcome of evaluating a transition's full `guards:`
/// list. Carries pass/fail plus per-guard `{kind, result}` records (for
/// transition-record `guards` field) plus an optional §20.4 diagnostic
/// code when the rejection has a filter-attributable cause.
pub(crate) struct GuardsOutcome {
    pub pass: bool,
    pub evaluated: Vec<Value>,
    pub diagnostic: Option<String>,
}

impl WorkflowRuntime {
    pub(crate) async fn guards_pass(
        &self,
        transition: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<GuardsOutcome> {
        let guards = transition
            .get("guards")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut evaluated = Vec::with_capacity(guards.len());
        for (idx, guard) in guards.iter().enumerate() {
            // SPEC §20.1 — `evaluate_with_diagnostic` returns a
            // §20.4 error code when the guard rejected for a specific
            // named reason (e.g. EVIDENCE_DIGEST_REQUIRED). Default-impl
            // returns None for everything else; preserved-behavior for
            // existing guard kinds.
            let (pass, diagnostic) = self
                .guards
                .evaluate_with_diagnostic(guard, instance, arguments, principal)
                .await?;
            let kind = guard
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            evaluated.push(json!({ "kind": kind, "result": pass }));
            self.audit
                .record(
                    instance
                        .audit_event("guard.evaluated")
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "guardIndex": idx,
                            "guard": guard,
                            "passed": pass,
                        })),
                )
                .await?;
            if !pass {
                return Ok(GuardsOutcome {
                    pass: false,
                    evaluated,
                    diagnostic,
                });
            }
        }

        Ok(GuardsOutcome {
            pass: true,
            evaluated,
            diagnostic: None,
        })
    }

    /// ADR-0008 — evaluate the mission's declared `outcomes` against the live
    /// instance: a `[{id, statement, met}]` surface for the response plus an
    /// `all_met` flag for the status derivation. Returns `(None, true)` when no
    /// outcomes are declared (vacuously met). Each `check` runs through the same
    /// guard `expr` evaluator; an unset slot or any eval error reads as "not yet
    /// met" (`met: false`) rather than erroring the whole response.
    async fn evaluate_outcomes(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        principal: &Principal,
    ) -> (Option<Value>, bool) {
        let Some(arr) = definition.get("outcomes").and_then(Value::as_array) else {
            return (None, true);
        };
        let mut surface = Vec::with_capacity(arr.len());
        let mut all_met = true;
        for oc in arr {
            let id = oc.get("id").and_then(Value::as_str).unwrap_or("");
            let statement = oc.get("statement").and_then(Value::as_str).unwrap_or("");
            let check = oc.get("check").and_then(Value::as_str).unwrap_or("");
            let guard = json!({ "kind": "expr", "expr": check });
            // CMP-018 — distinguish a guard EVALUATION ERROR from a
            // deliberate `Ok(false)`. Both read as "outcome not yet met"
            // (so a broken check can't flip a succeeded mission to Failed
            // by erroring the whole response), but an Err means the check
            // is *broken* (unset slot, unparseable expr, store failure) and
            // must be observable — mirror `filter_links_by_guards`. A silent
            // swallow here would hide an outcome that can never be met.
            let met = match self
                .guards
                .evaluate(&guard, instance, &Value::Null, principal)
                .await
            {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        target: "praxec_core::runtime",
                        workflow = %instance.id,
                        outcome = %id,
                        check = %check,
                        error = %e,
                        "outcome check evaluation errored; treating outcome as \
                         not-met (a guard bug, not a deliberate false)"
                    );
                    false
                }
            };
            if !met {
                all_met = false;
            }
            surface.push(json!({ "id": id, "statement": statement, "met": met }));
        }
        (Some(Value::Array(surface)), all_met)
    }

    /// ADR-0008 / intent-index — emit the `outcome.recorded` terminal event: the
    /// deterministic outcome done-signal (`outcomes_met`) plus the process/template
    /// identity the intent index learns over. Called once, beside the
    /// `workflow.completed` emit, at each terminal-reaching site (so it inherits
    /// that once-per-terminal guard). Non-critical audit path — a sink failure
    /// self-events and never fails the mission.
    pub(crate) async fn emit_outcome_recorded(
        &self,
        hint: StatusHint,
        definition: &Value,
        instance: &WorkflowInstance,
        correlation_id: &str,
        principal: &Principal,
    ) {
        let state_path = format!("/states/{}", pointer_escape(&instance.state));
        let terminal_outcome = definition
            .pointer(&state_path)
            .and_then(|s| s.get("outcome"))
            .and_then(Value::as_str)
            .and_then(TerminalOutcome::from_token);
        let (outcomes_surface, outcomes_met) = self
            .evaluate_outcomes(definition, instance, principal)
            .await;
        let outcomes_total = outcomes_surface
            .as_ref()
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        // awaiting_human = false: a terminal state hands the move to no one.
        let status = derive_mission_status(hint, true, terminal_outcome, outcomes_met, false);
        // The `process`/`taskClass` tag — `None` until Phase 2 adds the field.
        let task_class = definition
            .get("process")
            .or_else(|| definition.get("taskClass"))
            .and_then(Value::as_str);
        let payload = crate::intent_index::outcome_recorded_payload(
            &instance.definition_id,
            task_class,
            outcomes_met,
            outcomes_total,
            status.as_str(),
            status.reason().map(|r| r.as_str()),
        );
        self.record_or_self_event(
            instance
                .audit_event(crate::intent_index::OUTCOME_RECORDED)
                .with_correlation(correlation_id)
                .with_payload(payload),
        )
        .await;
    }

    /// Build the response body, including link filtering when the workflow
    /// or state declares `linkFilter: byGuards`. Always evaluated against
    /// the provided principal so "what could THIS caller do next" is what
    /// surfaces.
    pub(crate) async fn response(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        hint: StatusHint,
        error: Option<Value>,
        principal: &Principal,
    ) -> Value {
        // ADR-0008 — derive the typed mission status from the transient hint
        // plus the instance's resolved-ness and its outcomes. The current state
        // def carries the terminal `outcome:` marker and the `actor:` gate.
        let terminal = is_terminal(definition, &instance.state);
        let state_path = format!("/states/{}", pointer_escape(&instance.state));
        let state_def_opt = definition.pointer(&state_path);
        let terminal_outcome = if terminal {
            state_def_opt
                .and_then(|s| s.get("outcome"))
                .and_then(Value::as_str)
                .and_then(TerminalOutcome::from_token)
        } else {
            None
        };
        let awaiting_human = state_def_opt
            .and_then(|s| s.get("actor"))
            .and_then(Value::as_str)
            == Some("human");
        let (outcomes_surface, outcomes_met) = self
            .evaluate_outcomes(definition, instance, principal)
            .await;
        let mission = derive_mission_status(
            hint,
            terminal,
            terminal_outcome,
            outcomes_met,
            awaiting_human,
        );

        let mut all_links = links(definition, instance);
        if link_filter_byguards(definition, &instance.state) {
            all_links = self
                .filter_links_by_guards(all_links, definition, instance, principal)
                .await;
        }

        let mut body = json!({
            "workflow": {
                "id": instance.id,
                "definitionId": instance.definition_id,
                "definitionVersion": instance.definition_version,
                "state": instance.state,
                "version": instance.version,
            },
            "result": mission.to_result(),
            "context": instance.context,
            "links": all_links,
            "evidence": [],
        });

        // ADR-0008 — surface the mission's outcomes live (the cockpit checklist
        // + the orchestrator's target focus), present only when declared.
        if let Some(outcomes) = outcomes_surface {
            body["outcomes"] = outcomes;
        }

        // ADR-0009 — surface the workflow's `orchestrator` binding (the actor that
        // drives the mission) so a mediator/observer can show "driven by X".
        // Present only when declared (validated as a non-empty string at load).
        if let Some(orchestrator) = definition.get("orchestrator").and_then(Value::as_str) {
            if !orchestrator.is_empty() {
                body["orchestrator"] = Value::String(orchestrator.to_string());
            }
        }

        // SPEC §6.3 — surface the reserved `summary` slot at top level so an
        // LLM resuming a workflow cold sees the last human-readable summary
        // without having to dig through context. Absent when never set.
        if let Some(summary) = instance.context.get("summary").and_then(Value::as_str) {
            body["summary"] = Value::String(summary.to_string());
        }

        if let Some(err) = error {
            body["error"] = err;
        }

        // Phase guidance: attach goal/instructions from the current state.
        // `{{ }}` placeholders are interpolated at render time against the
        // live instance; stored strings are never mutated (SPEC v2 §5.2).
        // (`state_def_opt` was resolved above for the mission-status derivation.)
        let mut guidance = serde_json::Map::new();
        if let Some(state_def) = state_def_opt {
            if let Some(g) = state_def.get("goal").and_then(Value::as_str) {
                guidance.insert("goal".into(), json!(render_template(g, instance)));
            }
            if let Some(g) = state_def.get("guidance").and_then(Value::as_str) {
                guidance.insert("instructions".into(), json!(render_template(g, instance)));
            }
            // SPEC §21 — `delegate` is a pass-through pointer to an agent
            // config name. The gateway never branches on it; a consuming
            // agentic runtime uses it to spawn an isolated sub-agent.
            // Empty/non-string entries are rejected at config load by
            // `INVALID_DELEGATE`, so any value reaching this code is a
            // non-empty string.
            if let Some(d) = state_def.get("delegate").and_then(Value::as_str) {
                if !d.is_empty() {
                    body["delegate"] = Value::String(d.to_string());
                }
            }
        }

        // Skills refs: surface workflow-scope + active-state-scope refs
        // (SPEC v2 §5.5). Each ref pairs `subject` (the gateway.describe
        // lookup) with `verb` (the mode). Verbs are resolved from the
        // `_skillsLibrary` stamped onto the snapshot at config-resolve.
        let refs = collect_guidance_refs(definition, state_def_opt);
        if !refs.is_empty() {
            guidance.insert("refs".into(), Value::Array(refs));
        }

        if !guidance.is_empty() {
            body["guidance"] = Value::Object(guidance);
        }

        // Surface the global repo locks currently held — real `🔒` data for the
        // cockpit. Best-effort; absent when no lock space is wired.
        if let Some(locks) = &self.repo_locks {
            let held: Vec<Value> = locks
                .held()
                .await
                .into_iter()
                .map(|h| json!({ "file": h.file.to_string_lossy(), "holder": h.holder }))
                .collect();
            body["locks"] = Value::Array(held);
        }

        body
    }

    /// Evaluate each link's transition guards silently (no audit) and keep
    /// only those that would currently pass. Argument-dependent guards are
    /// evaluated against `{}` since arguments aren't known at link-gen
    /// time — those typically end up filtered out, which is the right
    /// answer for "show me what I could do *right now* without thinking."
    pub(crate) async fn filter_links_by_guards(
        &self,
        links: Vec<Value>,
        definition: &Value,
        instance: &WorkflowInstance,
        principal: &Principal,
    ) -> Vec<Value> {
        let empty_args = json!({});
        let mut out = Vec::with_capacity(links.len());
        for link in links {
            let rel = match link.get("rel").and_then(Value::as_str) {
                Some(r) => r,
                None => continue,
            };
            let transition = match transition_definition(definition, &instance.state, rel) {
                Some(t) => t,
                None => continue,
            };
            let guards = transition
                .get("guards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut all_pass = true;
            for guard in guards {
                match self
                    .guards
                    .evaluate(&guard, instance, &empty_args, principal)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        all_pass = false;
                        break;
                    }
                    // CMP-018 — a guard EVALUATION ERROR is treated as
                    // "filtered" (same as a deliberate `false`, the safe
                    // default for "what could I do right now"), but it is NOT
                    // the same thing: a deliberate false is expected, an error
                    // means the guard is broken. Surface it with a warn so a
                    // guard bug silently removing a transition is observable.
                    Err(e) => {
                        tracing::warn!(
                            target: "praxec_core::runtime",
                            workflow = %instance.id,
                            rel = %rel,
                            guard = %guard,
                            error = %e,
                            "link guard evaluation errored; treating link as \
                             filtered (a guard bug, not a deliberate false)"
                        );
                        all_pass = false;
                        break;
                    }
                }
            }
            if all_pass {
                out.push(link);
            }
        }
        out
    }

    pub(crate) async fn invalid_response(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        code: &str,
        message: String,
        attempted_transition: Option<&str>,
        principal: &Principal,
    ) -> Value {
        self.response(
            definition,
            instance,
            StatusHint::Rejected,
            Some(json!({
                "code": code,
                "message": message,
                "attemptedTransition": attempted_transition,
            })),
            principal,
        )
        .await
    }

    /// Audit-aware version of `invalid_response`. Records `transition.rejected`
    /// before building the response body. Errors recording the event are
    /// swallowed to ensure the caller still gets a useful response — the
    /// rejection itself is the primary signal.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn record_rejected(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        code: &str,
        message: String,
        attempted_transition: &str,
        correlation_id: &str,
        principal: &Principal,
    ) -> Value {
        self.record_or_self_event(
            instance
                .audit_event("transition.rejected")
                .with_correlation(correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "transition": attempted_transition,
                    "code": code,
                    "message": message,
                    "fromState": instance.state,
                })),
        )
        .await;
        self.invalid_response(
            definition,
            instance,
            code,
            message,
            Some(attempted_transition),
            principal,
        )
        .await
    }

    pub(crate) async fn failed_response(
        &self,
        definition: &Value,
        instance: &WorkflowInstance,
        err: &ExecutorError,
        attempted_transition: &str,
        principal: &Principal,
    ) -> Value {
        self.response(
            definition,
            instance,
            StatusHint::Failed,
            Some(json!({
                "code": "EXECUTOR_FAILED",
                "message": err.to_string(),
                "errorClass": err.class().token(),
                "attemptedTransition": attempted_transition,
            })),
            principal,
        )
        .await
    }
}
