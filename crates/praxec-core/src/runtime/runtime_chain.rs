//! Chain, timeout, and on-enter helpers for [`WorkflowRuntime`].
//! Methods stay on the same `impl WorkflowRuntime` block split across
//! sibling files — see `runtime.rs` for the type definition and lifecycle
//! entry points (`start`, `submit`, `get`).

use anyhow::{anyhow, bail};
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::mapping::merge_output;
use crate::model::{Evidence, Principal, WorkflowInstance};
use crate::reliability::{ReliabilityPolicy, execute_with_reliability};
use crate::runtime::runtime_links::{is_terminal, pointer_escape};
use crate::runtime::runtime_records::{blackboard_delta, validate_blackboard_writes};
use crate::runtime::runtime_schema::required_str;
use crate::runtime::{
    ChainOutcome, ChainResult, ChainStep, TransitionRecordParams, WorkflowRuntime,
};

impl WorkflowRuntime {
    /// Resolve the next state for a transition. The transition's declared
    /// `target` is the default; if `branches: [{when, target}]` is present,
    /// the first branch whose `when` guard passes wins. Falls back to the
    /// declared `target` when no branch matches.
    ///
    /// Emits a `transition.branched` audit event when a branch fires so
    /// it's clear in logs which branch the runtime took.
    pub(crate) async fn resolve_target(
        &self,
        transition: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<String> {
        let default_target = required_str(transition, "/target")?.to_owned();
        let Some(branches) = transition.get("branches").and_then(Value::as_array) else {
            return Ok(default_target);
        };
        for (idx, branch) in branches.iter().enumerate() {
            let Some(when) = branch.get("when") else {
                continue;
            };
            let Some(branch_target) = branch.get("target").and_then(Value::as_str) else {
                continue;
            };
            let pass = self
                .guards
                .evaluate(when, instance, arguments, principal)
                .await?;
            if pass {
                self.record_or_self_event(
                    instance
                        .audit_event("transition.branched")
                        .with_correlation(correlation_id)
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "branchIndex": idx,
                            "fromState": instance.state,
                            "toState": branch_target,
                        })),
                )
                .await;
                return Ok(branch_target.to_string());
            }
        }
        Ok(default_target)
    }

    /// SPEC §26 — apply a `while: <guard>` loop on the FROM state.
    ///
    /// Called by `submit` after the executor's output has been merged
    /// into `next.context` and the next-state `target` has been
    /// resolved. If the FROM state declares `while:`, this:
    /// 1. Evaluates the while-guard against the post-output context.
    /// 2. If truthy, increments the iteration counter in synthetic
    ///    context slot `_while_iter.<state>` and returns the
    ///    REROUTED target (= from_state) so the workflow re-enters.
    /// 3. If iteration > `max_iterations`, fails fast with
    ///    `WHILE_ITERATION_CAP_EXCEEDED`.
    /// 4. If the guard is falsy and we're actually leaving the state,
    ///    clears the synthetic iteration counter.
    ///
    /// `max_iterations` is REQUIRED when `while:` is declared (config
    /// validation should enforce this; this runtime check is the
    /// defense-in-depth backstop).
    ///
    /// Returns `Ok(Some(rerouted_target))` when re-entry fires;
    /// `Ok(None)` when the workflow proceeds normally.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn apply_while_loop(
        &self,
        definition: &Value,
        from_state: &str,
        declared_target: &str,
        next: &mut WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let Some(state_def) =
            definition.pointer(&format!("/states/{}", pointer_escape(from_state)))
        else {
            return Ok(None);
        };
        let Some(while_guard) = state_def.get("while") else {
            // Nothing to do — no while-guard on this state.
            // But if we're leaving from a state that previously had a
            // while-iter counter, scrub it. Cheap: just remove the slot
            // if present.
            clear_while_iter(next, from_state);
            return Ok(None);
        };
        let max_iter = state_def
            .get("max_iterations")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                anyhow!(
                    "INVALID_STATE_CONFIG: state '{from_state}' declares `while:` \
                     but no `max_iterations:` cap. The cap is REQUIRED to prevent \
                     runaway loops; declare an explicit ceiling (SPEC §26)."
                )
            })? as u32;

        let truthy = self
            .guards
            .evaluate(while_guard, next, arguments, principal)
            .await?;

        if !truthy {
            // Guard went falsy — we're actually leaving. Clean up.
            clear_while_iter(next, from_state);
            return Ok(None);
        }

        // Guard is truthy: re-enter. Bump iteration counter.
        let current = read_while_iter(next, from_state)?;
        let next_iter = current.saturating_add(1);
        if next_iter > max_iter {
            bail!(
                "WHILE_ITERATION_CAP_EXCEEDED: state '{from_state}' has `while:` \
                 guard that remained truthy after {max_iter} iterations. Either \
                 the guard's exit condition is unreachable, or `max_iterations:` \
                 needs to be increased after operator review (SPEC §26)."
            );
        }
        write_while_iter(next, from_state, next_iter);

        let _ = declared_target; // we deliberately ignore the declared target on re-enter.
        self.record_or_self_event(
            next.audit_event("workflow.state.iteration")
                .with_correlation(correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "state":         from_state,
                    "iteration":     next_iter,
                    "max_iterations": max_iter,
                })),
        )
        .await;

        Ok(Some(from_state.to_string()))
    }

    /// Lazy workflow-level timeout check. If `definition.timeoutMs` is
    /// declared and the wall-clock interval since `instance.started_at`
    /// exceeds it, advance the workflow to `definition.onTimeout.target`
    /// and emit a `workflow.timed_out` audit event. Returns `Some(updated)`
    /// when a timeout fired (caller should respond from that snapshot),
    /// `None` otherwise.
    pub(crate) async fn check_and_apply_timeout(
        &self,
        definition: &Value,
        mut instance: WorkflowInstance,
        principal: &Principal,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let Some(timeout_ms) = definition.get("timeoutMs").and_then(Value::as_u64) else {
            return Ok(None);
        };
        // If the workflow already reached a terminal state, no timeout to apply.
        if is_terminal(definition, &instance.state) {
            return Ok(None);
        }
        let elapsed = Utc::now()
            .signed_duration_since(instance.started_at)
            .num_milliseconds();
        if elapsed < 0 || (elapsed as u64) < timeout_ms {
            return Ok(None);
        }

        let target = match definition
            .pointer("/onTimeout/target")
            .and_then(Value::as_str)
        {
            Some(t) => t.to_string(),
            // Without a declared onTimeout, the workflow can't recover
            // declaratively. Audit the timeout but leave the instance alone
            // so the caller still gets a meaningful `failed`-style response.
            None => {
                self.record_or_self_event(
                    instance
                        .audit_event("workflow.timed_out")
                        .with_actor(&principal.subject)
                        .with_payload(json!({
                            "elapsedMs": elapsed,
                            "timeoutMs": timeout_ms,
                            "fromState": instance.state,
                            "applied": false,
                        })),
                )
                .await;
                return Ok(None);
            }
        };

        let from_state = instance.state.clone();
        let expected_version = instance.version;
        instance.state = target.clone();
        instance.version += 1;

        // Record-first: emit the `workflow.transition` record BEFORE committing
        // the timeout state change. If the record write fails, leave the workflow
        // unchanged so the next timeout check retries it.
        let correlation_id = format!("cor_{}", Uuid::new_v4().simple());
        let transition_name = definition
            .pointer("/onTimeout/transition")
            .and_then(Value::as_str)
            .unwrap_or("onTimeout");
        let on_timeout_def = definition
            .pointer("/onTimeout")
            .cloned()
            .unwrap_or(Value::Null);
        if let Err(e) = self
            .emit_transition_record(TransitionRecordParams {
                instance: &instance,
                from_state: &from_state,
                transition_name,
                transition_def: &on_timeout_def,
                actor: "system",
                principal: None,
                arguments: &json!({}),
                blackboard_delta: Value::Object(serde_json::Map::new()),
                guard_results: Vec::new(),
                child_workflow_id: None,
                executor_outcome: None,
                correlation_id: &correlation_id,
            })
            .await
        {
            tracing::warn!(
                workflow = %instance.id,
                error = %e,
                "timeout transition record failed — skipping state commit to allow retry"
            );
            return Ok(None);
        }

        let saved = self
            .store
            .save_if_version(instance, expected_version)
            .await?;

        self.record_or_self_event(
            saved
                .audit_event("workflow.timed_out")
                .with_correlation(&correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "elapsedMs": elapsed,
                    "timeoutMs": timeout_ms,
                    "fromState": from_state,
                    "toState": target,
                    "applied": true,
                })),
        )
        .await;
        // P2 (Task C liveness) — if the timeout advanced this child to a
        // TERMINAL state and it was spawned by a `kind: workflow` transition,
        // re-drive the suspended parent so it advances past the sub-workflow
        // transition (the reuse path maps a timed-out child to `failed`, so the
        // parent fails-propagates rather than sticking). A timeout that advances
        // to a non-terminal recovery state must NOT resume the parent yet — the
        // child is still live. Re-entrancy is bounded: the parent's re-drive does
        // `get(child)`, whose `check_and_apply_timeout` returns early here because
        // the child is now `is_terminal` (line above), so it never re-fires.
        if is_terminal(definition, &saved.state) {
            self.resume_parent_if_any(&saved).await;
        }
        Ok(Some(saved))
    }

    pub(crate) async fn run_on_enter(
        &self,
        definition: Value,
        mut instance: WorkflowInstance,
        correlation_id: &str,
    ) -> anyhow::Result<WorkflowInstance> {
        let path = format!("/states/{}/onEnter", pointer_escape(&instance.state));
        let Some(on_enter) = definition.pointer(&path).cloned() else {
            return Ok(instance);
        };

        let Some(executor_config) = on_enter.get("executor") else {
            return Ok(instance);
        };

        let policy = ReliabilityPolicy::from_value(on_enter.get("reliability"))?;
        let result = execute_with_reliability(
            self.executors.as_ref(),
            &self.audit,
            &instance,
            None,
            &json!({}),
            executor_config.clone(),
            &policy,
            correlation_id,
        )
        .await
        .map_err(|e| anyhow!("onEnter executor failed: {e}"))?;

        // CMP-011 — `next_transition` is a chain continuation that only the
        // interactive `submit` dispatch loop consumes (see runtime_submit
        // `dispatch_once`). An onEnter executor has no submit loop to drive,
        // so a returned `next_transition` would be silently dropped. Fail-fast
        // instead of swallowing the intent.
        if result.next_transition.is_some() {
            bail!(
                "NEXT_TRANSITION_UNSUPPORTED: an onEnter executor returned a next_transition, \
                 which is only supported on interactive submit transitions"
            );
        }

        let on_enter_input = instance.input.clone();
        merge_output(
            &mut instance.context,
            on_enter.get("output"),
            &json!({}),
            &on_enter_input,
            &result.output,
        )?;
        if let Err((slot, reason)) =
            validate_blackboard_writes(&definition, on_enter.get("output"), &instance.context)
        {
            bail!("BLACKBOARD_TYPE_ERROR: onEnter output write to typed slot '{slot}': {reason}");
        }

        if let Some(estore) = &self.evidence {
            for ev in &result.evidence {
                if let Err(e) = estore.record(&instance.id, ev.clone()).await {
                    tracing::warn!(workflow = %instance.id, error = %e, "evidence record failed");
                }
            }
        }

        let expected_version = instance.version;
        instance.version += 1;
        self.store.save_if_version(instance, expected_version).await
    }

    // -----------------------------------------------------------------------
    // Deterministic chaining
    // -----------------------------------------------------------------------

    /// Run a deterministic chain starting from the current state. Keeps
    /// executing `actor: "deterministic"` transitions automatically until
    /// a decision point (any non-deterministic transition), terminal state,
    /// depth limit, or failure is reached.
    ///
    /// Returns a `ChainOutcome` — either `Completed` (normal stop) or
    /// `Failed` (executor/guard error with partial progress).
    pub(crate) async fn run_deterministic_chain(
        &self,
        definition: &Value,
        mut instance: WorkflowInstance,
        principal: &Principal,
        correlation_id: &str,
        max_depth: u64,
    ) -> anyhow::Result<ChainOutcome> {
        let mut steps: Vec<ChainStep> = Vec::new();
        let mut accumulated_evidence: Vec<Evidence> = Vec::new();

        loop {
            // Stop: terminal state
            if is_terminal(definition, &instance.state) {
                break;
            }

            // Stop: depth limit
            if steps.len() as u64 >= max_depth {
                break;
            }

            // Gather transitions for current state
            let transitions_path =
                format!("/states/{}/transitions", pointer_escape(&instance.state));
            let Some(transitions) = definition
                .pointer(&transitions_path)
                .and_then(Value::as_object)
            else {
                break; // No transitions defined
            };

            // Collect deterministic transitions
            let deterministic: Vec<(&String, &Value)> = transitions
                .iter()
                .filter(|(_, t)| t.get("actor").and_then(Value::as_str) == Some("deterministic"))
                .collect();

            // (1b) Auto-drivable agent transitions: skill-surfacing `actor: agent`
            // moves the gateway can drive itself via the `kind: agent` executor
            // when `auto_drive_agents` is on (excluding the conventional
            // `escalate` bail-out). This is what makes the v0.6 cap/orchestrator
            // composition model executable end-to-end: a sub-workflow's agent
            // state advances under the kind:workflow poll instead of hanging.
            let agent_drivable: Vec<(&String, &Value)> = if self.auto_drive_agents {
                transitions
                    .iter()
                    .filter(|(_, t)| t.get("actor").and_then(Value::as_str) == Some("agent"))
                    .filter(|(name, _)| name.as_str() != "escalate")
                    .collect()
            } else {
                Vec::new()
            };
            // Deterministic transitions take precedence; auto-drive only kicks in
            // when there are none to fire first.
            let use_agent_drive = deterministic.is_empty() && !agent_drivable.is_empty();

            // Stop: a non-deterministic state-changer we will NOT auto-drive
            // (human gates; or any agent move when auto-drive is off) is a
            // decision point for the external actor. SPEC §29 `lightweight`
            // interactions and auto-drivable agent moves don't count.
            let blocking_non_det = transitions
                .iter()
                .filter(|(_, t)| t.get("actor").and_then(Value::as_str) != Some("deterministic"))
                .filter(|(_, t)| {
                    !t.get("lightweight")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                })
                .filter(|(name, t)| {
                    !(self.auto_drive_agents
                        && t.get("actor").and_then(Value::as_str) == Some("agent")
                        && name.as_str() != "escalate")
                })
                .count();
            if blocking_non_det > 0 && !use_agent_drive {
                break;
            }

            // Stop: nothing to fire (no deterministic and no auto-drivable agent).
            if deterministic.is_empty() && !use_agent_drive {
                break;
            }

            // Select the transition for this hop + compute its arguments + actor.
            let transition_name: String;
            let transition_def: Value;
            let chain_arguments: Value;
            let chain_actor: &'static str;
            if !deterministic.is_empty() {
                match self
                    .select_deterministic_transition(
                        &deterministic,
                        &instance,
                        principal,
                        correlation_id,
                    )
                    .await
                {
                    Ok((n, d)) => {
                        transition_name = n.clone();
                        transition_def = d.clone();
                    }
                    Err(e) => {
                        self.record_or_self_event(
                            instance
                                .audit_event("chain.failed")
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "fromState": instance.state,
                                    "chainDepth": steps.len(),
                                    "errorClass": "selection_error",
                                    "message": e.to_string(),
                                })),
                        )
                        .await;
                        return Ok(ChainOutcome::Failed {
                            failed_transition: String::new(),
                            error: e.to_string(),
                            error_class: "selection_error".into(),
                            partial: ChainResult {
                                instance,
                                steps,
                                evidence: accumulated_evidence,
                            },
                        });
                    }
                }
                chain_arguments = json!({});
                chain_actor = "deterministic";
            } else {
                // Auto-drive the first agent move: invoke the `kind: agent`
                // executor to produce the submission, then feed its JSON-object
                // output as this transition's `arguments` so the cap's existing
                // `$.arguments.*` output mapping applies unchanged.
                let name = agent_drivable[0].0.clone();
                let def = agent_drivable[0].1.clone();
                let state_goal = definition
                    .pointer(&format!("/states/{}/goal", pointer_escape(&instance.state)))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // Declared JSON type per output key, from the transition
                // inputSchema.properties[key].type. Flows to the runner as
                // `expected_output_types` so it enforces the type at the
                // `final_answer` boundary and re-prompts on mismatch (no wasted
                // run on a wrong-type answer that the post-run contract rejects).
                let expected_output_types: std::collections::BTreeMap<String, String> = def
                    .pointer("/inputSchema/properties")
                    .and_then(Value::as_object)
                    .map(|props| {
                        props
                            .iter()
                            .filter_map(|(k, v)| {
                                v.get("type")
                                    .and_then(Value::as_str)
                                    .map(|t| (k.clone(), t.to_string()))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let required_keys: Vec<String> = def
                    .pointer("/inputSchema/required")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                // Instruct the model to call `final_answer` — the result contract
                // the runner enforces. Telling it to "return JSON text, no prose"
                // made the model skip the tool entirely → AGENT_NO_RESULT. The
                // required keys also flow to the runner as `expected_output_keys`,
                // so it can validate a salvaged answer and phrase precise feedback.
                let key_list = required_keys.join(", ");
                // The agent must see the data it is acting on, not just the
                // static goal. Inject the workflow's seed input (the brief) and
                // accumulated context (prior-phase artifacts: spec, plan, …) so
                // an auto-driven leaf is never asked to work from nothing —
                // without this it fails honestly ("no brief provided"). Internal
                // bookkeeping keys (`_`-prefixed: `_subworkflow_wait`, library
                // snapshots) are filtered so they never leak into the prompt.
                let input_block = render_agent_data_block("Workflow input", &instance.input);
                let context_block = render_agent_data_block(
                    "Accumulated context (prior-phase outputs)",
                    &instance.context,
                );
                let goal_text = format!(
                    "{state_goal}{input_block}{context_block}\n\nWhen the task is complete, call \
                     the `final_answer` tool with an `output` object containing these top-level \
                     keys: {key_list}. Each value must satisfy the capability's output contract.",
                );
                // Compute affinity once so the agent_config and the audit
                // payload cannot drift. Context wins over input, so a
                // running loop can escalate the model tier per-iteration
                // by writing $.context.affinity_override, even though the
                // seed input is immutable.
                // Per-step affinity: a state may declare its own `affinity:`
                // (e.g. `reviewing` → "review") so different steps of one cap use
                // different model chains without a core change.
                let state_affinity = definition
                    .pointer(&format!(
                        "/states/{}/affinity",
                        pointer_escape(&instance.state)
                    ))
                    .and_then(Value::as_str);
                let auto_affinity_tier = auto_affinity(
                    state_affinity,
                    &instance.context,
                    &instance.input,
                    &instance.definition_id,
                    &self.auto_drive_affinity,
                );
                // Per-task reasoning effort: a loop/mission can set
                // $.context.effort_override (or seed $.input.effort_override) to
                // force a thinking level for this step (e.g. "xhigh"); else the
                // provider default. Set only when present so the kind:agent config
                // (deny_unknown_fields, reasoning_effort: Option) stays clean.
                let effort = auto_effort(&instance.context, &instance.input);
                let mut agent_config = json!({
                    "kind": "agent",
                    "affinity": auto_affinity_tier,
                    "goal": goal_text,
                    "tools": self.auto_drive_tools,
                    "expected_output_keys": required_keys,
                    "expected_output_types": expected_output_types,
                    // Fail-fast bound: a non-converging auto-driven agent should
                    // surface in ~minutes, not spin to the 600s executor default.
                    "max_seconds": self.auto_drive_max_seconds,
                });
                if let Some(e) = &effort {
                    agent_config["reasoning_effort"] = json!(e);
                }
                // Observability: emit a start event so a live `audit tail` shows
                // exactly which agent step is running (and pinpoints a hang).
                let agent_started = std::time::Instant::now();
                self.record_or_self_event(
                    instance
                        .audit_event("agent.invoked")
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "transition": name,
                            "state": instance.state,
                            "affinity": auto_affinity_tier,
                            "max_seconds": self.auto_drive_max_seconds,
                            "tools": self.auto_drive_tools,
                        })),
                )
                .await;
                let policy = ReliabilityPolicy::from_value(def.get("reliability"))?;
                match execute_with_reliability(
                    self.executors.as_ref(),
                    &self.audit,
                    &instance,
                    Some(&name),
                    &json!({}),
                    agent_config,
                    &policy,
                    correlation_id,
                )
                .await
                {
                    Ok(result) => {
                        // Per-call cost telemetry (value-based model selection):
                        // fold the agent's realized tokens + USD cost into the
                        // audit so every governed run is cost-attributable.
                        // `cost_usd` is `null` when the model isn't catalogued —
                        // degrade gracefully, never block the run.
                        let (model, prompt_tokens, completion_tokens, cost_usd) =
                            match &result.telemetry {
                                Some(t) => (
                                    Value::from(t.model.clone()),
                                    Value::from(t.prompt_tokens),
                                    Value::from(t.completion_tokens),
                                    t.cost_usd.map(Value::from).unwrap_or(Value::Null),
                                ),
                                None => (Value::Null, Value::Null, Value::Null, Value::Null),
                            };
                        self.record_or_self_event(
                            instance
                                .audit_event("agent.completed")
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "transition": name,
                                    "affinity": auto_affinity_tier,
                                    "duration_ms": agent_started.elapsed().as_millis() as u64,
                                    "model": model,
                                    "prompt_tokens": prompt_tokens,
                                    "completion_tokens": completion_tokens,
                                    "cost_usd": cost_usd,
                                })),
                        )
                        .await;
                        transition_name = name;
                        transition_def = def;
                        chain_arguments = result.output;
                        chain_actor = "agent";
                    }
                    Err(e) => {
                        self.record_or_self_event(
                            instance
                                .audit_event("chain.failed")
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "fromState": instance.state,
                                    "transition": name.clone(),
                                    "chainDepth": steps.len(),
                                    "errorClass": e.class().token(),
                                    "message": e.to_string(),
                                })),
                        )
                        .await;
                        return Ok(ChainOutcome::Failed {
                            failed_transition: name,
                            error: e.to_string(),
                            error_class: e.class().token().to_string(),
                            partial: ChainResult {
                                instance,
                                steps,
                                evidence: accumulated_evidence,
                            },
                        });
                    }
                }
            }

            let from_state = instance.state.clone();

            // Audit: chain step beginning
            self.record_or_self_event(
                instance
                    .audit_event("chain.step")
                    .with_correlation(correlation_id)
                    .with_payload(json!({
                        "transition": transition_name,
                        "fromState": from_state,
                        "chainDepth": steps.len(),
                    })),
            )
            .await;

            // Snapshot pre-merge context so the transition record can carry
            // an accurate blackboardDelta (SPEC §7.2). Cheap clone — context
            // is bounded.
            let pre_context = instance.context.clone();
            let mut chain_child_workflow_id: Option<String> = None;
            let mut chain_executor_outcome: Option<(bool, u64)> = None;

            // Execute the transition's executor (if present)
            if let Some(executor_config) = transition_def.get("executor") {
                let policy = ReliabilityPolicy::from_value(transition_def.get("reliability"))?;
                let exec_started = std::time::Instant::now();
                match execute_with_reliability(
                    self.executors.as_ref(),
                    &self.audit,
                    &instance,
                    Some(&transition_name),
                    &chain_arguments, // {} for deterministic; agent output when auto-driven
                    executor_config.clone(),
                    &policy,
                    correlation_id,
                )
                .await
                {
                    Ok(result) => {
                        // P2 (chain path) — a `kind: workflow` executor whose
                        // child is non-terminal returns `suspend` instead of
                        // advancing. The chain MUST stop here and park the
                        // parent WITHOUT merging the child's (null) output or
                        // advancing to the transition target. The context
                        // accumulated by PRIOR chain steps is already committed
                        // in `instance` at its current version; the caller
                        // writes `_subworkflow_wait` and bumps the version by 1.
                        // Mirrors the direct-submit suspend path
                        // (runtime_submit.rs::suspend_on_subworkflow).
                        if let Some(suspend) = result.suspend.clone() {
                            let mut payload = json!({
                                "transition": transition_name,
                                "fromState": from_state,
                                "chainDepth": steps.len(),
                            });
                            match &suspend {
                                crate::model::StepSuspend::Subworkflow(s) => {
                                    payload["childWorkflowId"] = json!(s.child_workflow_id);
                                }
                                crate::model::StepSuspend::AgentAwait(a) => {
                                    payload["correlationId"] = json!(a.correlation_id);
                                    payload["prompt"] = json!(a.prompt);
                                }
                            }
                            self.record_or_self_event(
                                instance
                                    .audit_event("chain.suspended")
                                    .with_correlation(correlation_id)
                                    .with_payload(payload),
                            )
                            .await;
                            return Ok(ChainOutcome::Suspended {
                                suspend,
                                transition: transition_name,
                                partial: ChainResult {
                                    instance,
                                    steps,
                                    evidence: accumulated_evidence,
                                },
                            });
                        }
                        // CMP-011 — a deterministic-chain executor has no
                        // interactive submit loop to consume a chain
                        // continuation; the deterministic chain is driven by
                        // transition `target`s, not by `next_transition`.
                        // Fail-fast rather than silently dropping the intent.
                        if result.next_transition.is_some() {
                            bail!(
                                "NEXT_TRANSITION_UNSUPPORTED: a deterministic executor returned a \
                                 next_transition, which is only supported on interactive submit \
                                 transitions"
                            );
                        }
                        chain_executor_outcome =
                            Some((true, exec_started.elapsed().as_millis() as u64));
                        merge_output(
                            &mut instance.context,
                            transition_def.get("output"),
                            &chain_arguments,
                            &instance.input,
                            &result.output,
                        )?;
                        // P2 — the executor returned a TERMINAL child result
                        // (not a suspend), so this `kind: workflow` chain leaf is
                        // resolving and advancing. Drop the durable
                        // `_subworkflow_wait` it parked on so a LATER sequential
                        // `kind: workflow` leaf does not reuse this (now-done)
                        // child. Guarded by transition identity. Mirrors the
                        // direct-submit advance path in runtime_submit.rs.
                        crate::runtime::runtime_submit::clear_subworkflow_wait_on_advance(
                            &mut instance.context,
                            &transition_name,
                        );
                        // P12 R1.4 — likewise drop a consumed `_agent_await`
                        // (a resumed session completed and is now advancing).
                        crate::runtime::runtime_submit::clear_agent_await_on_advance(
                            &mut instance.context,
                            &transition_name,
                        );
                        chain_child_workflow_id = result.child_workflow_id.clone();
                        if let Err((slot, reason)) = validate_blackboard_writes(
                            definition,
                            transition_def.get("output"),
                            &instance.context,
                        ) {
                            let message = format!(
                                "BLACKBOARD_TYPE_ERROR: output write to typed slot '{slot}': {reason}"
                            );
                            self.record_or_self_event(
                                instance
                                    .audit_event("chain.failed")
                                    .with_correlation(correlation_id)
                                    .with_payload(json!({
                                        "transition": transition_name,
                                        "fromState": from_state,
                                        "chainDepth": steps.len(),
                                        "code": "BLACKBOARD_TYPE_ERROR",
                                        "message": message,
                                    })),
                            )
                            .await;
                            return Ok(ChainOutcome::Failed {
                                failed_transition: transition_name,
                                error: message,
                                error_class: "blackboard_type_error".to_string(),
                                partial: ChainResult {
                                    instance,
                                    steps,
                                    evidence: accumulated_evidence,
                                },
                            });
                        }
                        accumulated_evidence.extend(result.evidence);
                    }
                    Err(err) => {
                        self.record_or_self_event(
                            instance
                                .audit_event("chain.failed")
                                .with_correlation(correlation_id)
                                .with_payload(json!({
                                    "transition": transition_name,
                                    "fromState": from_state,
                                    "chainDepth": steps.len(),
                                    "errorClass": err.class().token(),
                                    "message": err.to_string(),
                                })),
                        )
                        .await;
                        return Ok(ChainOutcome::Failed {
                            failed_transition: transition_name,
                            error: err.to_string(),
                            error_class: err.class().token().to_string(),
                            partial: ChainResult {
                                instance,
                                steps,
                                evidence: accumulated_evidence,
                            },
                        });
                    }
                }
            } else {
                // A transition can carry `output:` writes (e.g. a round-counter
                // increment on a pure deterministic gate) WITHOUT an executor.
                // The chain historically merged output ONLY inside the executor
                // branch, so an executor-less `output:` was silently dropped —
                // a convergence-loop counter on such a gate never advanced and
                // the loop spun forever. Apply it here, mirroring the executor
                // path's merge + typed-blackboard validation. (No executor
                // output, so the mappings resolve against context/arguments.)
                merge_output(
                    &mut instance.context,
                    transition_def.get("output"),
                    &chain_arguments,
                    &instance.input,
                    &Value::Null,
                )?;
                if let Err((slot, reason)) = validate_blackboard_writes(
                    definition,
                    transition_def.get("output"),
                    &instance.context,
                ) {
                    let message = format!(
                        "BLACKBOARD_TYPE_ERROR: output write to typed slot '{slot}': {reason}"
                    );
                    self.record_or_self_event(
                        instance
                            .audit_event("chain.failed")
                            .with_correlation(correlation_id)
                            .with_payload(json!({
                                "transition": transition_name,
                                "fromState": from_state,
                                "chainDepth": steps.len(),
                                "code": "BLACKBOARD_TYPE_ERROR",
                                "message": message,
                            })),
                    )
                    .await;
                    return Ok(ChainOutcome::Failed {
                        failed_transition: transition_name,
                        error: message,
                        error_class: "blackboard_type_error".to_string(),
                        partial: ChainResult {
                            instance,
                            steps,
                            evidence: accumulated_evidence,
                        },
                    });
                }
            }

            // Resolve target state (auto-branching)
            let target = self
                .resolve_target(
                    &transition_def,
                    &instance,
                    &chain_arguments,
                    principal,
                    correlation_id,
                )
                .await?;

            let expected_version = instance.version;
            instance.state = target.clone();
            instance.version += 1;

            // Record-first: emit the transition record for this chain hop
            // BEFORE committing the snapshot. Deterministic transitions carry a
            // null principal. A record-write failure aborts the whole chain
            // before `save_if_version`, so the instance version stays unchanged.
            let delta = blackboard_delta(&pre_context, &instance.context);
            self.emit_transition_record(TransitionRecordParams {
                instance: &instance,
                from_state: &from_state,
                transition_name: &transition_name,
                transition_def: &transition_def,
                actor: chain_actor,
                principal: None,
                arguments: &chain_arguments,
                blackboard_delta: delta,
                guard_results: Vec::new(),
                child_workflow_id: chain_child_workflow_id,
                executor_outcome: chain_executor_outcome,
                correlation_id,
            })
            .await?;

            instance = self
                .store
                .save_if_version(instance, expected_version)
                .await?;

            // Persist evidence
            if let Some(estore) = &self.evidence {
                for ev in &accumulated_evidence {
                    if let Err(e) = estore.record(&instance.id, ev.clone()).await {
                        tracing::warn!(
                            workflow = %instance.id, error = %e,
                            "evidence record failed during chain"
                        );
                    }
                }
            }

            // Record the step
            steps.push(ChainStep {
                from_state: from_state.clone(),
                transition: transition_name.clone(),
                to_state: target.clone(),
                version: instance.version,
            });

            // Audit: transition completed
            self.record_or_self_event(
                instance
                    .audit_event("workflow.transitioned")
                    .with_correlation(correlation_id)
                    .with_actor(&principal.subject)
                    .with_payload(json!({
                        "transition": transition_name,
                        "state": instance.state,
                        "version": instance.version,
                        "deterministic": chain_actor == "deterministic",
                        "chainDepth": steps.len(),
                    })),
            )
            .await;

            // Run onEnter for the new state
            instance = self
                .run_on_enter(definition.clone(), instance, correlation_id)
                .await?;

            // Check lazy timeout
            if let Some(timeout_ms) = definition.get("timeoutMs").and_then(Value::as_u64) {
                let elapsed = Utc::now()
                    .signed_duration_since(instance.started_at)
                    .num_milliseconds();
                if elapsed >= 0 && (elapsed as u64) >= timeout_ms {
                    break;
                }
            }
        }

        // Emit chain.completed if any steps were taken
        if !steps.is_empty() {
            self.record_or_self_event(
                instance
                    .audit_event("chain.completed")
                    .with_correlation(correlation_id)
                    .with_payload(json!({
                        "steps": steps.len(),
                        "finalState": instance.state,
                    })),
            )
            .await;
        }

        Ok(ChainOutcome::Completed(ChainResult {
            instance,
            steps,
            evidence: accumulated_evidence,
        }))
    }

    /// Select which deterministic transition to execute when a state has
    /// one or more. With a single candidate, it's returned directly. With
    /// multiple, guards are evaluated and exactly one must pass.
    async fn select_deterministic_transition(
        &self,
        candidates: &[(&String, &Value)],
        instance: &WorkflowInstance,
        principal: &Principal,
        correlation_id: &str,
    ) -> anyhow::Result<(String, Value)> {
        if candidates.len() == 1 {
            let (name, def) = candidates[0];
            return Ok((name.clone(), (*def).clone()));
        }

        // Multiple candidates: evaluate guards to select. An UNGUARDED
        // deterministic transition is a DEFAULT (lowest precedence) — taken only
        // when no *guarded* transition is viable, never alongside one. This gives
        // a switch state a total fallthrough: a discriminant value outside the
        // enumerated arms (or an out-of-domain producer output) routes to the
        // default instead of dead-stalling with selection_error. It is what makes
        // V23's required default actually expressible — without it, an unguarded
        // arm would always be viable and render every selection ambiguous.
        let mut guarded_viable = Vec::new();
        let mut defaults = Vec::new();
        for (name, def) in candidates {
            let guards = def
                .get("guards")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            if guards.is_empty() {
                defaults.push(((*name).clone(), (*def).clone()));
                continue;
            }

            let mut all_pass = true;
            for guard in &guards {
                let pass = self
                    .guards
                    .evaluate(guard, instance, &json!({}), principal)
                    .await?;
                self.record_or_self_event(
                    instance
                        .audit_event("guard.evaluated")
                        .with_correlation(correlation_id)
                        .with_payload(json!({
                            "guard": guard,
                            "passed": pass,
                            "context": "deterministic_selection",
                        })),
                )
                .await;
                if !pass {
                    all_pass = false;
                    break;
                }
            }

            if all_pass {
                guarded_viable.push(((*name).clone(), (*def).clone()));
            }
        }

        match guarded_viable.len() {
            1 => Ok(guarded_viable
                .into_iter()
                .next()
                .expect("invariant: match arm `1 =>` guarantees one guarded candidate")),
            0 => match defaults.len() {
                // No guarded arm matched — fall through to the single default.
                1 => Ok(defaults
                    .into_iter()
                    .next()
                    .expect("invariant: match arm `1 =>` guarantees one default")),
                0 => bail!(
                    "no viable deterministic transition in state '{}': \
                     all {} candidates had failing guards and there is no default",
                    instance.state,
                    candidates.len()
                ),
                n => bail!(
                    "ambiguous default in state '{}': {} unguarded deterministic \
                     transitions; at most one default is allowed",
                    instance.state,
                    n
                ),
            },
            n => bail!(
                "ambiguous deterministic transition in state '{}': \
                 {} of {} candidates had passing guards; \
                 exactly one must be viable",
                instance.state,
                n,
                candidates.len()
            ),
        }
    }
}

// ── SPEC §27 helpers — state-local blackboard slot lifecycle ──────────────

impl WorkflowRuntime {
    /// SPEC §27 — when a transition leaves a state, clear every slot
    /// declared `scope: state` on that state. State-local slots are
    /// initialized on enter, persist across `while:`-re-entry of the
    /// same state, and are cleared on exit (including chain-hop exits).
    ///
    /// Called by `submit` AFTER the final target is determined and
    /// AFTER `apply_while_loop` has had a chance to re-route back to
    /// the same state. When `from_state == target`, this is a no-op
    /// (re-entry preserves state-local values).
    ///
    /// Emits a `workflow.slot.cleared` audit event with the list of
    /// cleared slot names so operators can correlate state exits
    /// with their cumulative blackboard footprint.
    pub(crate) async fn clear_state_local_slots_on_exit(
        &self,
        definition: &Value,
        from_state: &str,
        target: &str,
        next: &mut WorkflowInstance,
        correlation_id: &str,
        principal: &Principal,
    ) {
        if from_state == target {
            // While-re-entry or self-loop — keep state-local slots and
            // keep per-state fire counters (the counter's whole purpose
            // is to bound self-loops).
            return;
        }
        // SPEC §29 — scrub synthetic per-transition fire counters for
        // this state. They're keyed `_fire_count.<state>.<transition>`
        // and only mean anything inside one state-entry; clear when we
        // leave. Generic — applies to any transition that declared
        // `max_fires_per_visit`, not just HITL.
        let fire_prefix = format!("_fire_count.{from_state}.");
        if let Some(ctx) = next.context.as_object_mut() {
            ctx.retain(|k, _| !k.starts_with(&fire_prefix));
        }
        let Some(state_def) =
            definition.pointer(&format!("/states/{}", pointer_escape(from_state)))
        else {
            return;
        };
        let Some(slots) = state_def.get("slots").and_then(Value::as_object) else {
            return;
        };
        let mut cleared: Vec<String> = Vec::new();
        if let Some(ctx) = next.context.as_object_mut() {
            for (name, decl) in slots {
                let scope = decl
                    .get("scope")
                    .and_then(Value::as_str)
                    .unwrap_or("workflow");
                if scope == "state" && ctx.remove(name).is_some() {
                    cleared.push(name.clone());
                }
            }
        }
        if cleared.is_empty() {
            return;
        }
        self.record_or_self_event(
            next.audit_event("workflow.slot.cleared")
                .with_correlation(correlation_id)
                .with_actor(&principal.subject)
                .with_payload(json!({
                    "state": from_state,
                    "slots": cleared,
                })),
        )
        .await;
    }
}

// ── SPEC §26 helpers — while-loop iteration counter ────────────────────────

/// Render a labeled JSON block of `data` for an auto-driven agent's prompt,
/// dropping internal `_`-prefixed keys (e.g. `_subworkflow_wait`, library
/// snapshots) and emitting nothing when there is no operator-relevant data.
/// Keeps engine bookkeeping out of the model's context window.
fn render_agent_data_block(label: &str, data: &Value) -> String {
    let Some(obj) = data.as_object() else {
        return String::new();
    };
    let filtered: serde_json::Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if filtered.is_empty() {
        return String::new();
    }
    let json = serde_json::to_string_pretty(&Value::Object(filtered)).unwrap_or_default();
    format!("\n\n{label}:\n{json}")
}

fn while_iter_key(state: &str) -> String {
    format!("_while_iter.{state}")
}

fn read_while_iter(instance: &WorkflowInstance, state: &str) -> anyhow::Result<u32> {
    let key = while_iter_key(state);
    let n = crate::model::read_counter_slot(&instance.context, &key)?;
    // Saturate rather than truncate: a slot beyond u32::MAX still trips the
    // cap below instead of wrapping to a small value and bypassing it.
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

fn write_while_iter(instance: &mut WorkflowInstance, state: &str, value: u32) {
    let key = while_iter_key(state);
    if let Some(ctx) = instance.context.as_object_mut() {
        ctx.insert(key, json!(value));
    }
}

fn clear_while_iter(instance: &mut WorkflowInstance, state: &str) {
    let key = while_iter_key(state);
    if let Some(ctx) = instance.context.as_object_mut() {
        ctx.remove(&key);
    }
}

/// Derive the model-affinity tier from a cap `definition_id`.
///
/// Cap ids follow the pattern `cognitive/cap.<verb>.<name>` (or a bare
/// `<verb>` — anything that can't be parsed falls through to `default`).
/// Verbs that are primarily coding work (`implement`, `refactor`, `run`,
/// `scaffold`) are routed to the `"coding"` tier (qwen3-coder); every other
/// recognised or unrecognised verb falls back to `default` (the operator-
/// configured `auto_drive_affinity`, typically `"reasoning"`).
pub(crate) fn affinity_for(definition_id: &str, default: &str) -> String {
    // Try to extract the verb from `cognitive/cap.<verb>.<rest>` or
    // plain `<verb>.<rest>` or bare `<verb>` patterns.
    let verb = definition_id
        // strip a leading path component ending with `/cap.`
        .split("/cap.")
        .last()
        // now take only the first dot-separated segment as the verb
        .and_then(|s| s.split('.').next())
        .unwrap_or("");

    match verb {
        "implement" | "refactor" | "run" | "scaffold" => "coding".to_string(),
        _ => default.to_string(),
    }
}

/// Per-task affinity override: checks `context["affinity_override"]` first,
/// then `input["affinity_override"]`, then falls back to [`affinity_for`].
/// The first non-empty (trimmed) string wins so a running loop (e.g. the
/// build-loop) can escalate the model tier across iterations by writing to
/// `$.context.affinity_override`, even though the seed `input` is immutable.
/// Absent keys and blank strings are both treated as "not set" and skipped.
///
/// # Example
/// ```text
/// // context wins over input, which wins over the verb mapping:
/// let ctx = json!({"affinity_override": "reasoning"});
/// let input = json!({"affinity_override": "coding"});
/// auto_affinity(&ctx, &input, "cognitive/cap.implement.tdd", "reasoning")
/// // → "reasoning"  (context override wins, despite input wanting "coding"
/// //                 and the verb mapping to "coding")
/// ```
pub(crate) fn auto_effort(
    context: &serde_json::Value,
    input: &serde_json::Value,
) -> Option<String> {
    if let Some(effort) = context.get("effort_override").and_then(|v| v.as_str()) {
        if !effort.is_empty() {
            return Some(effort.to_string());
        }
    }
    if let Some(effort) = input.get("effort_override").and_then(|v| v.as_str()) {
        if !effort.is_empty() {
            return Some(effort.to_string());
        }
    }
    None
}

fn auto_affinity(
    state_affinity: Option<&str>,
    context: &serde_json::Value,
    input: &serde_json::Value,
    definition_id: &str,
    default: &str,
) -> String {
    // Per-step: a state's declared `affinity:` wins (e.g. a `reviewing` state
    // declares `affinity: review` so it resolves to the senior review chain),
    // then context/input overrides, then the verb-derived default.
    if let Some(s) = state_affinity {
        let t = s.trim();
        if !t.is_empty() {
            return t.to_owned();
        }
    }
    if let Some(s) = context.get("affinity_override").and_then(|v| v.as_str()) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    if let Some(s) = input.get("affinity_override").and_then(|v| v.as_str()) {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    affinity_for(definition_id, default)
}

#[test]
fn test_auto_effort_context_override() {
    let context = serde_json::json!({"effort_override": "xhigh"});
    let input = serde_json::json!({});
    assert_eq!(auto_effort(&context, &input), Some("xhigh".to_string()));
}

#[test]
fn test_auto_effort_input_override() {
    let context = serde_json::json!({});
    let input = serde_json::json!({"effort_override": "high"});
    assert_eq!(auto_effort(&context, &input), Some("high".to_string()));
}

#[test]
fn test_auto_effort_empty_string() {
    let context = serde_json::json!({"effort_override": ""});
    let input = serde_json::json!({"effort_override": "medium"});
    assert_eq!(auto_effort(&context, &input), Some("medium".to_string()));

    let context = serde_json::json!({"effort_override": "low"});
    let input = serde_json::json!({"effort_override": ""});
    assert_eq!(auto_effort(&context, &input), Some("low".to_string()));

    let context = serde_json::json!({"effort_override": ""});
    let input = serde_json::json!({"effort_override": ""});
    assert_eq!(auto_effort(&context, &input), None);
}

#[test]
fn test_auto_effort_neither() {
    let context = serde_json::json!({});
    let input = serde_json::json!({});
    assert_eq!(auto_effort(&context, &input), None);
}

#[cfg(test)]
mod tests {
    use super::{affinity_for, auto_affinity};

    #[test]
    fn affinity_for_implement_gives_coding() {
        assert_eq!(
            affinity_for("cognitive/cap.implement.build-loop", "reasoning"),
            "coding"
        );
    }

    #[test]
    fn affinity_for_plan_gives_default() {
        assert_eq!(
            affinity_for("cognitive/cap.plan.technical-design", "reasoning"),
            "reasoning"
        );
    }

    #[test]
    fn affinity_for_refactor_gives_coding() {
        assert_eq!(
            affinity_for("cognitive/cap.refactor.cleanup", "reasoning"),
            "coding"
        );
    }

    #[test]
    fn affinity_for_run_gives_coding() {
        assert_eq!(
            affinity_for("cognitive/cap.run.tests", "reasoning"),
            "coding"
        );
    }

    #[test]
    fn affinity_for_scaffold_gives_coding() {
        assert_eq!(
            affinity_for("cognitive/cap.scaffold.new-service", "reasoning"),
            "coding"
        );
    }

    #[test]
    fn affinity_for_unknown_id_gives_default() {
        assert_eq!(
            affinity_for("something/totally/unknown", "reasoning"),
            "reasoning"
        );
    }

    #[test]
    fn affinity_for_empty_gives_default() {
        assert_eq!(affinity_for("", "reasoning"), "reasoning");
    }

    // ── auto_affinity tests ───────────────────────────────────────────────

    /// (1) Context `affinity_override` wins over input override AND the
    /// verb mapping — a running loop can escalate the tier per-iteration.
    #[test]
    fn auto_affinity_context_wins_over_input_and_verb() {
        let ctx = serde_json::json!({"affinity_override": "reasoning"});
        let input = serde_json::json!({"affinity_override": "coding"});
        // "implement" verb would normally map to "coding" …
        assert_eq!(
            auto_affinity(
                None,
                &ctx,
                &input,
                "cognitive/cap.implement.tdd.discipline",
                "reasoning"
            ),
            "reasoning"
        );
    }

    /// (2) When context lacks the key, input override still applies.
    #[test]
    fn auto_affinity_input_wins_when_context_absent() {
        let ctx = serde_json::json!({});
        let input = serde_json::json!({"affinity_override": "coding"});
        assert_eq!(
            auto_affinity(
                None,
                &ctx,
                &input,
                "cognitive/cap.plan.technical-design",
                "reasoning"
            ),
            "coding"
        );
    }

    /// (3) When neither has it, falls back to affinity_for.
    #[test]
    fn auto_affinity_falls_back_when_both_absent() {
        let ctx = serde_json::json!({});
        let input = serde_json::json!({});
        // "implement" verb → "coding"
        assert_eq!(
            auto_affinity(
                None,
                &ctx,
                &input,
                "cognitive/cap.implement.build-loop",
                "reasoning"
            ),
            "coding"
        );
    }

    /// (4) Empty-string context override is skipped; input takes over.
    #[test]
    fn auto_affinity_empty_context_skipped_input_applies() {
        let ctx = serde_json::json!({"affinity_override": ""});
        let input = serde_json::json!({"affinity_override": "coding"});
        assert_eq!(
            auto_affinity(
                None,
                &ctx,
                &input,
                "cognitive/cap.plan.technical-design",
                "reasoning"
            ),
            "coding"
        );
    }

    /// (5) Empty-string context and blank input fall through to verb.
    #[test]
    fn auto_affinity_empty_context_empty_input_falls_back() {
        let ctx = serde_json::json!({"affinity_override": "  "});
        let input = serde_json::json!({"affinity_override": ""});
        // "implement" verb → "coding"
        assert_eq!(
            auto_affinity(
                None,
                &ctx,
                &input,
                "cognitive/cap.implement.build-loop",
                "reasoning"
            ),
            "coding"
        );
    }

    /// (6) Context override wins even when input override is also set
    /// (the loop escalated the tier).
    #[test]
    fn auto_affinity_context_override_wins_over_input_override() {
        let ctx = serde_json::json!({"affinity_override": "reasoning"});
        let input = serde_json::json!({"affinity_override": "coding"});
        // "implement" verb would normally map to "coding", but context wins
        assert_eq!(
            auto_affinity(
                None,
                &ctx,
                &input,
                "cognitive/cap.implement.build-loop",
                "reasoning"
            ),
            "reasoning"
        );
    }
}
