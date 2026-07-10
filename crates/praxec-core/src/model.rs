use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type WorkflowId = String;
pub type WorkflowDefinitionId = String;
pub type StateName = String;
pub type TransitionName = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowInstance {
    pub id: WorkflowId,
    pub definition_id: WorkflowDefinitionId,
    pub definition_version: String,
    /// The resolved workflow definition snapshot this instance was started
    /// with (SPEC §8.2 / §8.3). Captured once at `workflow.start` from the
    /// `DefinitionStore` and persisted with the instance. Every in-flight
    /// operation (`get`, `submit`, deterministic chaining, timeout) resolves
    /// the definition from *this* field — never from the live config — so
    /// editing or hot-reloading config never disturbs a running instance.
    pub definition: Value,
    pub state: StateName,
    pub version: u64,
    pub input: Value,
    pub context: Value,
    /// When this workflow instance was created. Used by lazy timeout
    /// checks: if the next `submit` or `get` happens after
    /// `definition.timeoutMs` elapsed, the instance auto-transitions to
    /// `definition.onTimeout.target`. Defaults to `Utc::now()` for
    /// instances loaded from older stores that didn't persist this field.
    #[serde(default = "Utc::now")]
    pub started_at: DateTime<Utc>,
    /// SPEC §20.2 — caller-supplied trace id propagated to every audit
    /// event for this instance. Captured at `workflow.start` and persisted
    /// with the snapshot so it survives reload + drain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// SPEC §20.2 — caller-supplied run id, same lifecycle as `trace_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// Sub-workflow nesting depth (0 at the top level). Persisted so the
    /// recursion guard survives async re-drive — replaces the former
    /// `WORKFLOW_DEPTH` task-local, which assumed a synchronous call stack.
    /// `#[serde(default)]`: instances from older stores load at depth 0.
    #[serde(default)]
    pub depth: u32,
    /// T24 — when set, the workflow has been cancelled via
    /// `WorkflowRuntime::cancel`. The original `state` is preserved
    /// (recoverable); `result.status` in `get` responses surfaces as
    /// `"cancelled"` and `submit` calls are rejected with
    /// `WORKFLOW_CANCELLED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at: Option<DateTime<Utc>>,
    /// Operator-supplied reason for cancellation (audit trail). Paired
    /// with `cancelled_at`; only meaningful when that is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_reason: Option<String>,
    /// P2 — set on a child workflow spawned by a `kind: workflow` transition,
    /// so when this child terminates the runtime can re-drive the parent's
    /// pending transition (Task C liveness). `None` for top-level workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ParentLink>,
}

/// P2 — set on a child workflow spawned by a `kind: workflow` transition, so
/// when the child terminates the runtime can re-drive the parent's pending
/// transition. The re-drive reads the parent's LIVE version (not a stored one).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ParentLink {
    pub workflow_id: String,
    pub transition: String,
}

impl WorkflowInstance {
    /// SPEC §20.2 — build an audit event pre-decorated with this
    /// instance's `workflow_id`, `trace_id`, and `run_id`. Use this at
    /// emission sites so the three identifiers stay in sync without
    /// boilerplate at every call site.
    pub fn audit_event(&self, event_type: impl Into<String>) -> crate::audit::AuditEvent {
        let mut e = crate::audit::AuditEvent::new(event_type).with_workflow(&self.id);
        if let Some(t) = &self.trace_id {
            e = e.with_trace_id(t.clone());
        }
        if let Some(r) = &self.run_id {
            e = e.with_run_id(r.clone());
        }
        e
    }
}

/// SPEC §26/§29 — read a gateway-maintained synthetic counter slot
/// (`_while_iter.<state>`, `_fire_count.<state>.<transition>`) from a
/// workflow `context`.
///
/// These counters back runaway-loop caps. An *absent* slot legitimately
/// means "zero so far". A slot that is *present but not a non-negative
/// integer* means the persisted context was corrupted or tampered with —
/// silently coercing that to 0 (the old `as_u64().unwrap_or(0)` behavior)
/// would reset the counter and bypass the cap. We surface it as an error
/// instead, so corruption fails loud rather than disabling the guard.
pub fn read_counter_slot(context: &Value, key: &str) -> anyhow::Result<u64> {
    match context.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => v.as_u64().ok_or_else(|| {
            anyhow::anyhow!(
                "CORRUPT_COUNTER_SLOT: context slot `{key}` holds a non-numeric \
                 value ({v}); refusing to silently reset the counter to 0, which \
                 would bypass a runaway-loop cap (SPEC §26/§29)."
            )
        }),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Principal {
    pub subject: String,
    pub roles: Vec<String>,
    pub permissions: Vec<String>,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self {
            subject: "anonymous".to_string(),
            roles: Vec::new(),
            permissions: Vec::new(),
        }
    }

    /// Role marker convention used by the runtime to recognise a human
    /// principal. Embedders that wire identity per request (see
    /// `docs/guides/embeddings.md`) tag human callers with this role; agent-driven
    /// invocations leave it absent. `actor: "human"` transitions reject
    /// submissions from principals without this role.
    pub const HUMAN_ROLE: &'static str = "human";

    pub fn is_human(&self) -> bool {
        self.roles.iter().any(|r| r == Self::HUMAN_ROLE)
    }

    /// CMP-001 — parse a caller-supplied identity claim of the shape
    /// `{ "subject": "...", "roles": [...], "permissions": [...] }` into a
    /// `Principal`. Returns `None` when `subject` is absent or empty (a claim
    /// with no subject is not a usable identity); `roles`/`permissions` default
    /// to empty arrays.
    ///
    /// This only PARSES the claim — it does not establish trust. The CHANNEL
    /// the claim arrived on is what makes it trustworthy (e.g. the MCP server
    /// reads it from request `_meta`, which the embedding host sets, never from
    /// the agent-controlled tool `arguments`).
    pub fn from_claim(value: &Value) -> Option<Principal> {
        let obj = value.as_object()?;
        let subject = obj
            .get("subject")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())?;
        let string_list = |key: &str| -> Vec<String> {
            obj.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Principal {
            subject: subject.to_string(),
            roles: string_list("roles"),
            permissions: string_list("permissions"),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct StartWorkflow {
    pub definition_id: WorkflowDefinitionId,
    pub input: Value,
    pub principal: Principal,
    /// SPEC §20.2 — optional trace id propagated to every audit event
    /// for the created instance. Persisted on the instance.
    pub trace_id: Option<String>,
    /// SPEC §20.2 — optional run id, same lifecycle as `trace_id`.
    pub run_id: Option<String>,
    /// Nesting depth to stamp onto the created instance. 0 for a top-level
    /// start; a `kind: workflow` spawn passes `parent.depth + 1`.
    pub depth: u32,
    /// P2 — when this start is a `kind: workflow` spawn, links the created
    /// child back to the parent transition to re-drive on child termination.
    /// `None` for top-level starts.
    pub parent: Option<ParentLink>,
}

#[derive(Debug, Clone, Default)]
pub struct GetWorkflow {
    pub workflow_id: WorkflowId,
    pub principal: Principal,
    /// SPEC §20.2 — optional trace id for any audit events this call
    /// emits (the existing instance's persisted trace_id is preserved
    /// and used unless this is explicitly set to override).
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SubmitTransition {
    pub workflow_id: WorkflowId,
    pub expected_version: u64,
    pub transition: TransitionName,
    pub arguments: Value,
    pub principal: Principal,
    /// SPEC §6.3 — optional model-authored summary. When present, the runtime
    /// stores it to `context.summary` on commit. It is **never** a guard input
    /// (model-authored content is untrusted); `check` errors on any guard that
    /// reads `$.context.summary`.
    pub summary: Option<String>,
    /// SPEC §20.2 — optional per-submit trace id. The instance's
    /// persisted `trace_id` is used by default; this override lets a
    /// caller stitch a single submit into a different trace
    /// (e.g. a re-evaluation run replaying a recorded session).
    pub trace_id: Option<String>,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// SPEC §20.1 — content-identity of the artifact this evidence
    /// references. Convention: `sha256:` prefix + lowercase-hex digest of
    /// the artifact bytes. Optional; populate when the artifact is
    /// byte-stable (verifier-produced JUnit, SARIF, coverage JSON, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// SPEC §20.1 — model-stated confidence (0.0..=1.0) that this evidence
    /// supports the claim it's attached to. Out-of-range values fail
    /// validation with `INVALID_CONFIDENCE`. Deterministic executors
    /// typically omit; model-authored evidence SHOULD populate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

impl Evidence {
    /// SPEC §20.1 — validate that `confidence` (if present) is within the
    /// allowed range. Producers MUST call this before persisting.
    /// Returns the offending value on rejection so error messages can name
    /// the violator.
    ///
    /// ```
    /// use praxec_core::model::Evidence;
    ///
    /// let ok = Evidence {
    ///     kind: "test".into(),
    ///     id: "ev_1".into(),
    ///     uri: None,
    ///     summary: None,
    ///     digest: None,
    ///     confidence: Some(0.85),
    /// };
    /// assert!(ok.validate_confidence().is_ok());
    ///
    /// let too_high = Evidence { confidence: Some(1.5), ..ok.clone() };
    /// assert_eq!(too_high.validate_confidence(), Err(1.5));
    ///
    /// let absent = Evidence { confidence: None, ..ok };
    /// assert!(absent.validate_confidence().is_ok());
    /// ```
    pub fn validate_confidence(&self) -> Result<(), f32> {
        match self.confidence {
            Some(c) if !(0.0..=1.0).contains(&c) => Err(c),
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecuteRequest {
    pub workflow: WorkflowInstance,
    pub transition: Option<String>,
    pub arguments: Value,
    pub executor_config: Value,
    /// Idempotency key for this execute call. Computed once per
    /// `execute_with_reliability` invocation, identical across retries and
    /// across primary + fallback candidates so downstream services can
    /// dedupe. None when the executor config didn't request one.
    pub idempotency_key: Option<String>,
    /// SPEC §24 (v0.4) — the parent transition's correlation_id, threaded
    /// through so executors that fan out (`kind: parallel`) can emit
    /// per-branch audit events that share the parent's correlation. The
    /// runtime sets this when invoking executors through
    /// `execute_with_reliability`. Tests that build `ExecuteRequest`
    /// directly may leave it `None`; per-branch audit events fall back to
    /// emitting under a synthetic `"unset-corr"` value (clearly broken in
    /// production but acceptable for direct-executor unit tests).
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExecuteResult {
    pub output: Value,
    pub evidence: Vec<Evidence>,
    /// SPEC §7.2 — when this executor is `kind: workflow`, the id of the
    /// sub-workflow it started. Surfaced on the parent's transition record
    /// as `childWorkflowId` so audit reconstruction can follow the chain.
    /// `None` for every other executor kind.
    pub child_workflow_id: Option<String>,
    /// SPEC §33 — when the executor selects a transition rather than (or
    /// in addition to) producing output, the runtime's submit pipeline
    /// chains into another submit cycle dispatching this transition.
    /// Used by `kind: llm`; every other executor leaves it `None`.
    ///
    /// The runtime-driven loop (vs the spec's original executor-driven
    /// loop in §33.2) avoids the `expected_version` re-entry hazard
    /// `runtime_submit::submit` would suffer if the executor called back
    /// into the runtime. Each turn becomes a real audited transition;
    /// caps and audit invariants stay intact.
    pub next_transition: Option<NextTransition>,
    /// The ONE step-suspend channel (`docs/await-resume-architecture.md`):
    /// `Some(_)` means the executor durably parked instead of advancing, and
    /// the runtime responds `waiting` — never a failure. `None` for every
    /// other outcome. (`ExecuteResult` is an in-process runtime value, never
    /// serialized, so no serde attribute is needed; `Default` supplies `None`.)
    pub suspend: Option<StepSuspend>,
    /// Per-call cost telemetry — realized token usage + USD cost for an
    /// executor that ran a model (currently `kind: agent` auto-drive). The
    /// runtime folds these into the `agent.completed` audit event so every
    /// governed run is cost-attributable. `None` for executors that run no
    /// model (`Default` supplies it).
    pub telemetry: Option<ExecutorTelemetry>,
}

/// Realized model-call telemetry for one executor invocation. Carried up from
/// the executor to the runtime's audit emit site. `cost_usd` is `None` when the
/// model isn't in the catalog — degrade gracefully, never fail the run (mirrors
/// the `kind: llm` executor's degrade-to-None contract).
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorTelemetry {
    /// The resolved `"provider:model"` the call ran on.
    pub model: String,
    /// Prompt (input) tokens summed across the call's turns.
    pub prompt_tokens: u64,
    /// Completion (output) tokens summed across the call's turns.
    pub completion_tokens: u64,
    /// Computed USD cost, or `None` when the model isn't catalogued.
    pub cost_usd: Option<f64>,
}

/// Why an executor suspended its step instead of advancing — the typed,
/// exhaustively-matched set of suspend sources (`docs/await-resume-architecture.md`:
/// one primitive, pluggable signal sources). Every variant maps to the SAME
/// runtime representation: a durable context wait-marker + a `waiting`
/// response (`MissionStatus::Waiting`), never a failure.
#[derive(Debug, Clone, PartialEq)]
pub enum StepSuspend {
    /// P2 — a `kind: workflow` child is non-terminal; park the parent on it.
    Subworkflow(SubworkflowSuspend),
    /// P12 R1.4 — a `kind: agent` session hit `await_human`; its conversation
    /// is already durably parked in the [`ParkedSessionStore`](crate::ports::ParkedSessionStore)
    /// under `correlation_id`. Park the workflow on the awaited human reply.
    AgentAwait(AgentAwaitSuspend),
}

impl StepSuspend {
    /// The sub-workflow suspend, when that is this suspend's source.
    pub fn as_subworkflow(&self) -> Option<&SubworkflowSuspend> {
        match self {
            StepSuspend::Subworkflow(s) => Some(s),
            StepSuspend::AgentAwait(_) => None,
        }
    }

    /// The agent-await suspend, when that is this suspend's source.
    pub fn as_agent_await(&self) -> Option<&AgentAwaitSuspend> {
        match self {
            StepSuspend::AgentAwait(a) => Some(a),
            StepSuspend::Subworkflow(_) => None,
        }
    }
}

/// P2 — a `kind: workflow` executor whose child is non-terminal returns this
/// instead of advancing: the parent is durably suspended on the child
/// (recorded as `_subworkflow_wait` context data) and re-driven when the child
/// terminates. `None` for every other outcome.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct SubworkflowSuspend {
    pub child_workflow_id: String,
}

/// P12 R1.4 — a `kind: agent` step parked on `await_human`. The runtime
/// records this as an `_agent_await` context marker (mirrors
/// `_subworkflow_wait`) and responds `waiting`; a human later resumes by
/// re-submitting the SAME transition with `arguments.reply`, which the agent
/// executor routes to the runner's correlated `resume`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct AgentAwaitSuspend {
    /// Routes the human's reply back to the exact parked agent frame.
    pub correlation_id: String,
    /// What the agent asked the human — surfaced to the approvals drain.
    pub prompt: String,
}

/// P12 R1.4 (`docs/await-resume-architecture.md`) — one durably parked agent
/// tool-loop session, awaiting an out-of-band signal (a human reply). Written
/// by the agent runner when a session hits its suspend signal; keyed by
/// `correlation_id`, which routes the later reply back to this exact frame.
///
/// `session` and `conversation` are OPAQUE JSON blobs owned by the agents
/// crate — core stores them durably but never interprets agentic state (the
/// same layering that keeps autonomy out of the substrate).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParkedAgentSession {
    /// Routes a later signal back to this exact suspended frame.
    pub correlation_id: String,
    /// What the agent asked the human — the surfacable context for the
    /// pending-awaits drain (interactive mediator or headless `px approvals`).
    pub prompt: String,
    /// Serialized `AgentSession` (opaque to core).
    pub session: Value,
    /// Serialized conversation state — message history + pending tool
    /// results (opaque to core).
    pub conversation: Value,
    /// When the frame parked.
    pub parked_at: DateTime<Utc>,
}

/// SPEC §33 D2 — the transition the executor selected this turn, plus
/// its arguments. The runtime maps this onto the same dispatch path
/// `praxec.command` uses: guards run, blackboard updates, audit
/// fires, state advances. The `summary` is the executor's optional
/// summary of its reasoning for this turn; surfaced into the audit
/// event but not the transition record.
#[derive(Debug, Clone)]
pub struct NextTransition {
    pub transition: String,
    pub arguments: Value,
    pub summary: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn counter_slot_absent_is_zero() {
        let ctx = json!({ "other": 3 });
        assert_eq!(read_counter_slot(&ctx, "_fire_count.s.t").unwrap(), 0);
    }

    #[test]
    fn counter_slot_null_is_zero() {
        let ctx = json!({ "_while_iter.s": null });
        assert_eq!(read_counter_slot(&ctx, "_while_iter.s").unwrap(), 0);
    }

    #[test]
    fn counter_slot_valid_number_is_read() {
        let ctx = json!({ "_while_iter.s": 7 });
        assert_eq!(read_counter_slot(&ctx, "_while_iter.s").unwrap(), 7);
    }

    #[test]
    fn counter_slot_corrupt_string_is_error_not_zero() {
        // STUB-105/106 — a non-numeric value must NOT silently reset to 0
        // (which would bypass the SPEC §26/§29 runaway caps).
        let ctx = json!({ "_while_iter.s": "tampered" });
        let err = read_counter_slot(&ctx, "_while_iter.s").unwrap_err();
        assert!(
            err.to_string().contains("CORRUPT_COUNTER_SLOT"),
            "expected corruption error, got: {err}"
        );
    }

    #[test]
    fn counter_slot_negative_is_error() {
        let ctx = json!({ "_fire_count.s.t": -1 });
        assert!(read_counter_slot(&ctx, "_fire_count.s.t").is_err());
    }

    #[test]
    fn principal_from_claim_parses_subject_roles_permissions() {
        let p = Principal::from_claim(&json!({
            "subject": "user:alice",
            "roles": ["human", "reviewer"],
            "permissions": ["approve"],
        }))
        .expect("valid claim");
        assert_eq!(p.subject, "user:alice");
        assert!(p.is_human());
        assert_eq!(p.permissions, vec!["approve".to_string()]);
    }

    #[test]
    fn principal_from_claim_defaults_missing_lists_to_empty() {
        let p = Principal::from_claim(&json!({ "subject": "agent:x" })).expect("valid claim");
        assert_eq!(p.subject, "agent:x");
        assert!(p.roles.is_empty());
        assert!(p.permissions.is_empty());
        assert!(!p.is_human());
    }

    #[test]
    fn principal_from_claim_rejects_missing_or_empty_subject() {
        // A claim without a usable subject is not an identity.
        assert!(Principal::from_claim(&json!({ "roles": ["human"] })).is_none());
        assert!(Principal::from_claim(&json!({ "subject": "" })).is_none());
        assert!(Principal::from_claim(&json!("not-an-object")).is_none());
    }
}
