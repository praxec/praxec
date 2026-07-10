//! ADR-0009 (c) — the **headless orchestrator driver**. The loop that drives one
//! mission to its outcomes via the §32 surface, publishing progress to the
//! [`Bus`] and **parking** when the next move belongs to a human. No UI: this is
//! the execution engine, the same path whether a cockpit observes it or it runs
//! in CI/cron.
//!
//! Three seams keep it testable without a live LLM or gateway:
//! - [`MissionGateway`] — the §32 read/write surface (query / command).
//! - [`TransitionChooser`] — the orchestrator's brain (which transition advances
//!   toward the outcomes). The production impl wraps an
//!   [`AgentSessionRunner`](crate::session::AgentSessionRunner); tests script it.
//! - [`Bus`] — where progress + HITL requests flow to a consumer.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use praxec_core::bus::{Bus, InteractionKind, InteractionReply, MissionEvent};

use crate::session::{AgentRunOutcome, AgentSession, AgentSessionRunner};

/// One legal next move on a mission (a HATEOAS link, §32). `actor == "human"`
/// marks a move only a human may make → the driver parks on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegalAction {
    pub transition: String,
    pub actor: String,
}

/// One outcome on the mission's definition of done (ADR-0008), with its live mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeView {
    pub id: String,
    pub statement: String,
    pub met: bool,
}

/// The driver's view of a mission at one step — the parsed §32 response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissionState {
    pub mission_id: String,
    /// ADR-0008 resolution status: `running | waiting | succeeded | failed`.
    pub status: String,
    pub reason: Option<String>,
    /// The instance version — the optimistic-concurrency guard for `submit`.
    pub version: u64,
    pub goal: Option<String>,
    pub outcomes: Vec<OutcomeView>,
    pub legal_actions: Vec<LegalAction>,
}

impl MissionState {
    /// The mission has reached a terminal resolution — stop.
    pub fn resolved(&self) -> bool {
        matches!(self.status.as_str(), "succeeded" | "failed")
    }

    /// Every legal move belongs to a human (or it's a `waiting` gate) — the
    /// orchestrator must park rather than act.
    pub fn human_turn(&self) -> bool {
        !self.legal_actions.is_empty() && self.legal_actions.iter().all(|a| a.actor == "human")
    }

    /// The agent-actionable moves — `actor: agent` decision points the
    /// orchestrator chooses among via the chooser. Excludes `human` (park) and
    /// `deterministic` (auto-fire, see [`Self::deterministic_actions`]).
    pub fn agent_actions(&self) -> Vec<&LegalAction> {
        self.legal_actions
            .iter()
            .filter(|a| a.actor == "agent")
            .collect()
    }

    /// Deterministic legal-now moves the driver must fire itself when the
    /// server-side chain has halted (e.g. it stopped at a guard-gated branch and
    /// nothing re-entered it). Firing one re-enters `run_deterministic_chain`
    /// server-side and advances. This is the poka-yoke against the stranded
    /// `running` instance: a fireable deterministic move must never be left for
    /// nothing to fire.
    pub fn deterministic_actions(&self) -> Vec<&LegalAction> {
        self.legal_actions
            .iter()
            .filter(|a| a.actor == "deterministic")
            .collect()
    }

    /// Parse the driver's view from a §32 gateway response (`praxec.query` /
    /// `praxec.command`). Tolerant of absent fields — a response with no
    /// `outcomes`/`links` simply yields empty lists.
    pub fn from_response(value: &Value) -> Self {
        let status = value
            .pointer("/result/status")
            .and_then(Value::as_str)
            .unwrap_or("running")
            .to_string();
        let reason = value
            .pointer("/result/reason")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mission_id = value
            .pointer("/workflow/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let version = value
            .pointer("/workflow/version")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let goal = value
            .pointer("/guidance/goal")
            .and_then(Value::as_str)
            .map(str::to_string);
        let outcomes = value
            .get("outcomes")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|o| OutcomeView {
                        id: o
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        statement: o
                            .get("statement")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        met: o.get("met").and_then(Value::as_bool).unwrap_or(false),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let legal_actions = value
            .get("links")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| {
                        let transition = l.get("rel").and_then(Value::as_str)?.to_string();
                        let actor = l
                            .get("actor")
                            .and_then(Value::as_str)
                            .unwrap_or("agent")
                            .to_string();
                        Some(LegalAction { transition, actor })
                    })
                    .collect()
            })
            .unwrap_or_default();
        MissionState {
            mission_id,
            status,
            reason,
            version,
            goal,
            outcomes,
            legal_actions,
        }
    }
}

/// The §32 surface the driver acts through (a governed principal). In-process
/// against the runtime for headless; the cockpit's gateway is the same shape.
#[async_trait]
pub trait MissionGateway: Send + Sync {
    async fn query(&self, mission_id: &str) -> Result<MissionState, String>;
    /// Submit a transition. `expected_version` is the optimistic-concurrency
    /// guard (the version the driver last read) — a stale value is rejected.
    async fn command(
        &self,
        mission_id: &str,
        transition: &str,
        expected_version: u64,
    ) -> Result<MissionState, String>;
}

/// The orchestrator's decision:
/// - `Ok(Some(t))` — the transition that best advances the outcomes.
/// - `Ok(None)` — no good move (give up honestly; an authoring dead-end).
/// - `Err(source)` — the chooser itself FAILED (the runner errored: missing API
///   key, 401, model-resolution, network). This is NOT "no move" — it must
///   surface as its own signal, never be collapsed into a misleading give-up.
///
/// The production impl runs one agent session; tests script the choice.
#[async_trait]
pub trait TransitionChooser: Send + Sync {
    async fn choose(&self, state: &MissionState) -> Result<Option<String>, String>;
}

/// The production [`TransitionChooser`]: runs ONE agent session per decision via
/// an [`AgentSessionRunner`] (rig-backed in production), and reads the chosen
/// transition from the agent's `final_answer` output
/// (`{ "transition": "<name>" }`). A missing or illegal choice yields `Ok(None)`
/// (give up) — never a wrong submit. A runner ERROR (missing API key, 401,
/// model-resolution, network) is PROPAGATED as `Err`, never swallowed into a
/// misleading give-up.
pub struct AgentChooser {
    runner: Arc<dyn AgentSessionRunner>,
    /// Resolved `"provider:model"` for the orchestrator's binding.
    model: String,
    timeout: Duration,
}

impl AgentChooser {
    pub fn new(
        runner: Arc<dyn AgentSessionRunner>,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            runner,
            model: model.into(),
            timeout,
        }
    }
}

#[async_trait]
impl TransitionChooser for AgentChooser {
    async fn choose(&self, state: &MissionState) -> Result<Option<String>, String> {
        let actions = state.agent_actions();
        if actions.is_empty() {
            return Ok(None);
        }
        let session = AgentSession {
            model: self.model.clone(),
            system_prompt: Some(decision_system_prompt()),
            user_prompt: decision_user_prompt(state, &actions),
            tools: vec![],
            reasoning_effort: None,
            timeout: self.timeout,
            // A decision call is a single short turn; the total timeout governs
            // it, so the no-progress watchdog rides at the same bound (a separate
            // sub-timeout would add no signal here).
            stall_timeout: self.timeout,
            expected_output_keys: vec![],
            expected_output_types: Default::default(),
        };
        // PROPAGATE a runner error (missing API key, 401, model-resolution,
        // network) as `Err` — the pre-fix `.ok()?` swallowed EVERY such error
        // into `None`, which `drive_mission` turned into a misleading `GaveUp`
        // ("no actionable move"). An honest error must reach the operator.
        let report = self.runner.run(session).await.map_err(|e| e.to_string())?;
        let AgentRunOutcome::Completed(result) = report.outcome else {
            // A non-erroring run that produced no conforming answer (NoResult /
            // TimedOut) is a genuine "no good move" — give up, don't error.
            return Ok(None);
        };
        // The agent names exactly one legal transition in `output.transition`;
        // anything else is treated as "no good move".
        let Some(chosen) = result.output.get("transition").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(actions
            .iter()
            .find(|a| a.transition == chosen)
            .map(|a| a.transition.clone()))
    }
}

fn decision_system_prompt() -> String {
    "You are the orchestrator driving a flow toward its outcomes. Choose exactly \
     one of the legal transitions that best advances the unmet outcomes. Respond \
     via final_answer with output { \"transition\": \"<name>\" } naming one legal \
     transition verbatim."
        .to_string()
}

fn decision_user_prompt(state: &MissionState, actions: &[&LegalAction]) -> String {
    let mut s = String::new();
    if let Some(goal) = &state.goal {
        s.push_str(&format!("Goal: {goal}\n"));
    }
    if !state.outcomes.is_empty() {
        s.push_str("Outcomes (✓ met / ○ unmet):\n");
        for o in &state.outcomes {
            let mark = if o.met { '✓' } else { '○' };
            s.push_str(&format!("  {mark} {}\n", o.statement));
        }
    }
    s.push_str("Legal transitions:\n");
    for a in actions {
        s.push_str(&format!("  - {}\n", a.transition));
    }
    s.push_str("Which transition?");
    s
}

/// How a **headless** run answers a parked HITL request — there's no human, so a
/// policy stands in for the mediator (ADR-0009: the consumer is swappable).
///
/// P16 — a policy is NOT a proven human, so the bus refuses its reply on a
/// `human_decision` gate ([`InteractionKind::requires_human`]): the gate stays
/// parked and accumulates for a later human drain, regardless of policy. The
/// policy still answers the conversational (non-decision) kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessPolicy {
    /// Approve every non-decision interaction (autonomous CI/cron runs that
    /// trust the flow). Cannot resolve a `human_decision` gate.
    AutoApprove,
    /// Decline every non-decision interaction (a run that must not act without
    /// a real human).
    Decline,
}

impl HeadlessPolicy {
    fn reply(self) -> InteractionReply {
        InteractionReply {
            approved: self == HeadlessPolicy::AutoApprove,
            text: String::new(),
        }
    }
}

/// A headless bus **consumer**: the stand-in for the mediator when no human is
/// present. It answers each parked [`MissionEvent::Interaction`] per `policy` and
/// returns once a mission resolves.
///
/// Takes a pre-made `events` receiver (the caller [`Bus::subscribe`]s *before*
/// starting the driver) so no parked request can race ahead of the subscription.
pub async fn run_headless_consumer(
    mut events: tokio::sync::broadcast::Receiver<MissionEvent>,
    bus: Bus,
    policy: HeadlessPolicy,
) {
    // P16 — the policy answers under its OWN identity: a role-less, non-human
    // principal by construction. The bus therefore refuses it on human_decision
    // gates (they stay parked for a human drain) while non-decision
    // interactions still flow. Never impersonate a human here.
    let principal = praxec_core::model::Principal {
        subject: "headless-policy".to_string(),
        roles: Vec::new(),
        permissions: Vec::new(),
    };
    while let Ok(event) = events.recv().await {
        match event {
            MissionEvent::Interaction {
                request_id,
                ref kind,
                ref prompt,
                ..
            } => match bus.answer(request_id, policy.reply(), &principal) {
                Ok(()) => eprintln!(
                    "[mission] HITL interaction ({kind:?}): {prompt} — auto-answered per policy"
                ),
                Err(e) => eprintln!("[mission] HITL interaction ({kind:?}): {prompt} — {e}"),
            },
            MissionEvent::Status {
                ref mission_id,
                ref status,
            } => {
                eprintln!("[mission {mission_id}] status: {status}");
            }
            MissionEvent::Chunk { ref text, .. } => {
                // Streaming orchestrator-model output — live observability.
                eprint!("{text}");
            }
            MissionEvent::Resolved {
                ref mission_id,
                ref status,
            } => {
                eprintln!("\n[mission {mission_id}] RESOLVED: {status}");
                break;
            }
        }
    }
}

/// The production [`MissionGateway`]: the in-process §32 surface against a live
/// [`WorkflowRuntime`](praxec_core::runtime::WorkflowRuntime) — headless, no
/// MCP round-trip. `query` = `get`, `command` = `submit`; both parse the §32
/// response via [`MissionState::from_response`]. The driver acts as a governed
/// principal: every move goes through the same legal-transition gate the §32
/// surface enforces.
pub struct RuntimeMissionGateway {
    runtime: Arc<praxec_core::runtime::WorkflowRuntime>,
    principal: praxec_core::model::Principal,
}

impl RuntimeMissionGateway {
    pub fn new(
        runtime: Arc<praxec_core::runtime::WorkflowRuntime>,
        principal: praxec_core::model::Principal,
    ) -> Self {
        Self { runtime, principal }
    }

    /// Surface the deterministic legal-now moves the §32 `links` projection
    /// hides, so the driver can fire one when the server-side chain has halted
    /// (the stranded-`running` poka-yoke). Best-effort: a lookup failure leaves
    /// the state unchanged — the driver's fail-loud backstop still applies.
    async fn augment_deterministic(
        &self,
        mission_id: &str,
        mut state: MissionState,
    ) -> MissionState {
        if let Ok(transitions) = self.runtime.deterministic_legal_now(mission_id).await {
            for t in transitions {
                if !state.legal_actions.iter().any(|a| a.transition == t) {
                    state.legal_actions.push(LegalAction {
                        transition: t,
                        actor: "deterministic".to_string(),
                    });
                }
            }
        }
        state
    }
}

#[async_trait]
impl MissionGateway for RuntimeMissionGateway {
    async fn query(&self, mission_id: &str) -> Result<MissionState, String> {
        let resp = self
            .runtime
            .get(praxec_core::model::GetWorkflow {
                workflow_id: mission_id.to_string(),
                principal: self.principal.clone(),
                trace_id: None,
                run_id: None,
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(self
            .augment_deterministic(mission_id, MissionState::from_response(&resp))
            .await)
    }

    async fn command(
        &self,
        mission_id: &str,
        transition: &str,
        expected_version: u64,
    ) -> Result<MissionState, String> {
        let resp = self
            .runtime
            .submit(praxec_core::model::SubmitTransition {
                workflow_id: mission_id.to_string(),
                expected_version,
                transition: transition.to_string(),
                arguments: serde_json::json!({}),
                principal: self.principal.clone(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(self
            .augment_deterministic(mission_id, MissionState::from_response(&resp))
            .await)
    }
}

/// How a drive ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOutcome {
    /// Reached a terminal resolution (`succeeded` / `failed` + reason).
    Resolved {
        status: String,
        reason: Option<String>,
    },
    /// A human declined at a HITL gate.
    Declined,
    /// The orchestrator had no good move (or a dead-end non-terminal state).
    GaveUp,
    /// Hit the step bound without resolving.
    MaxSteps,
    /// A §32 call errored.
    Error(String),
    /// The chooser itself FAILED — its agent runner errored (missing API key,
    /// 401, model-resolution, network). Distinct from `GaveUp` (a legitimate "no
    /// good move"): this is an infrastructure/config fault the operator must see,
    /// not a dead-end flow.
    ChooserFailed { source: String },
}

/// Drive a mission to resolution (ADR-0009 c). Each step: read state → publish
/// status → if resolved, stop → if it's the human's turn, **park** on the bus and
/// resume on the reply → else the orchestrator chooses and we submit. Bounded by
/// `max_steps`; the outcomes are the real stop condition.
pub async fn drive_mission(
    gateway: &dyn MissionGateway,
    chooser: &dyn TransitionChooser,
    bus: &Bus,
    mission_id: &str,
    max_steps: usize,
) -> DriveOutcome {
    let mut state = match gateway.query(mission_id).await {
        Ok(s) => s,
        Err(e) => return DriveOutcome::Error(e),
    };

    for _ in 0..max_steps {
        bus.publish(MissionEvent::Status {
            mission_id: mission_id.to_string(),
            status: state.status.clone(),
        });

        if state.resolved() {
            bus.publish(MissionEvent::Resolved {
                mission_id: mission_id.to_string(),
                status: state.status.clone(),
            });
            return DriveOutcome::Resolved {
                status: state.status,
                reason: state.reason,
            };
        }

        // HITL — the next move belongs to a human. Park on the bus; a consumer
        // (the mediator, or a headless policy) answers, and we resume.
        if state.human_turn() {
            let Some(action) = state.legal_actions.first().cloned() else {
                return DriveOutcome::GaveUp;
            };
            let prompt = state
                .goal
                .clone()
                .unwrap_or_else(|| format!("Approve `{}`?", action.transition));
            let reply = bus
                .request_interaction(mission_id, InteractionKind::Approve, prompt)
                .await;
            if !reply.approved {
                return DriveOutcome::Declined;
            }
            state = match gateway
                .command(mission_id, &action.transition, state.version)
                .await
            {
                Ok(s) => s,
                Err(e) => return DriveOutcome::Error(e),
            };
            continue;
        }

        // Autonomous. Prefer an agent decision point (the chooser). With none,
        // fire a deterministic legal-now move if one exists — re-entering the
        // server-side chain — rather than giving up. This is the poka-yoke
        // against stranding a `running` instance that still holds a fireable
        // transition (the chain halted, e.g. at a guard-gated branch, and the
        // agentic driver can't see deterministic moves in the §32 links).
        if state.agent_actions().is_empty() {
            if let Some(det) = state.deterministic_actions().first() {
                state = match gateway
                    .command(mission_id, &det.transition, state.version)
                    .await
                {
                    Ok(s) => s,
                    Err(e) => return DriveOutcome::Error(e),
                };
                continue;
            }
            // Fail-loud: a non-terminal `running` state with no agent, human, or
            // deterministic move is a genuine dead end — an authoring bug the
            // terminal-reachability validator should have caught. Give up
            // explicitly (the CLI maps this to a non-zero error); never silently
            // leave the instance running.
            return DriveOutcome::GaveUp;
        }
        let transition = match chooser.choose(&state).await {
            Ok(Some(t)) => t,
            Ok(None) => return DriveOutcome::GaveUp,
            // The chooser's runner errored — surface the REAL fault (honest
            // error) instead of masquerading it as a give-up.
            Err(source) => return DriveOutcome::ChooserFailed { source },
        };
        bus.publish(MissionEvent::Chunk {
            mission_id: mission_id.to_string(),
            text: format!("→ {transition}"),
        });
        state = match gateway
            .command(mission_id, &transition, state.version)
            .await
        {
            Ok(s) => s,
            Err(e) => return DriveOutcome::Error(e),
        };
    }

    DriveOutcome::MaxSteps
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use praxec_core::bus::InteractionReply;

    use super::*;

    // ── builders ─────────────────────────────────────────────────────────────

    /// The proven-human principal tests answer HITL gates as (P16: only a
    /// principal with the `human` role may resolve a human_decision).
    fn human_principal() -> praxec_core::model::Principal {
        praxec_core::model::Principal {
            subject: "operator".into(),
            roles: vec![praxec_core::model::Principal::HUMAN_ROLE.into()],
            permissions: Vec::new(),
        }
    }

    fn ms(status: &str, actions: &[(&str, &str)]) -> MissionState {
        MissionState {
            mission_id: "m1".into(),
            status: status.into(),
            reason: None,
            version: 1,
            goal: None,
            outcomes: vec![],
            legal_actions: actions
                .iter()
                .map(|(t, a)| LegalAction {
                    transition: (*t).into(),
                    actor: (*a).into(),
                })
                .collect(),
        }
    }

    fn full_response() -> Value {
        serde_json::json!({
            "workflow": { "id": "m1", "version": 3 },
            "result": { "status": "failed", "reason": "guard_unmet" },
            "guidance": { "goal": "do the thing" },
            "outcomes": [ { "id": "x", "statement": "it works", "met": true } ],
            "links": [ { "rel": "go", "actor": "agent" }, { "rel": "approve", "actor": "human" } ]
        })
    }

    // ── test doubles ─────────────────────────────────────────────────────────

    struct ScriptedGateway {
        states: Mutex<VecDeque<MissionState>>,
    }
    impl ScriptedGateway {
        fn new(states: Vec<MissionState>) -> Self {
            Self {
                states: Mutex::new(states.into()),
            }
        }
        fn pop(&self) -> Result<MissionState, String> {
            self.states
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .pop_front()
                .ok_or_else(|| "exhausted".to_string())
        }
    }
    #[async_trait]
    impl MissionGateway for ScriptedGateway {
        async fn query(&self, _id: &str) -> Result<MissionState, String> {
            self.pop()
        }
        async fn command(&self, _id: &str, _t: &str, _v: u64) -> Result<MissionState, String> {
            self.pop()
        }
    }

    /// Always hands back a non-terminal `running` state with a `go` move — never
    /// resolves (for the max-steps bound).
    struct LoopGateway;
    #[async_trait]
    impl MissionGateway for LoopGateway {
        async fn query(&self, _id: &str) -> Result<MissionState, String> {
            Ok(ms("running", &[("go", "agent")]))
        }
        async fn command(&self, _id: &str, _t: &str, _v: u64) -> Result<MissionState, String> {
            Ok(ms("running", &[("go", "agent")]))
        }
    }

    struct QueryErrGateway;
    #[async_trait]
    impl MissionGateway for QueryErrGateway {
        async fn query(&self, _id: &str) -> Result<MissionState, String> {
            Err("query boom".into())
        }
        async fn command(&self, _id: &str, _t: &str, _v: u64) -> Result<MissionState, String> {
            Ok(ms("running", &[]))
        }
    }

    struct CommandErrGateway;
    #[async_trait]
    impl MissionGateway for CommandErrGateway {
        async fn query(&self, _id: &str) -> Result<MissionState, String> {
            Ok(ms("running", &[("go", "agent")]))
        }
        async fn command(&self, _id: &str, _t: &str, _v: u64) -> Result<MissionState, String> {
            Err("command boom".into())
        }
    }

    struct FirstActionChooser;
    #[async_trait]
    impl TransitionChooser for FirstActionChooser {
        async fn choose(&self, state: &MissionState) -> Result<Option<String>, String> {
            Ok(state.agent_actions().first().map(|a| a.transition.clone()))
        }
    }

    struct NoneChooser;
    #[async_trait]
    impl TransitionChooser for NoneChooser {
        async fn choose(&self, _state: &MissionState) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    /// A chooser whose decision FAILS (its runner errored). Proves
    /// `drive_mission` surfaces the fault as `ChooserFailed`, never `GaveUp`.
    struct ErrChooser;
    #[async_trait]
    impl TransitionChooser for ErrChooser {
        async fn choose(&self, _state: &MissionState) -> Result<Option<String>, String> {
            Err("no API key configured".into())
        }
    }

    fn running_then(states: Vec<MissionState>) -> ScriptedGateway {
        ScriptedGateway::new(states)
    }

    // ── MissionState::resolved ───────────────────────────────────────────────

    #[test]
    fn resolved_is_true_for_succeeded() {
        assert!(ms("succeeded", &[]).resolved());
    }
    #[test]
    fn resolved_is_true_for_failed() {
        assert!(ms("failed", &[]).resolved());
    }
    #[test]
    fn resolved_is_false_for_running() {
        assert!(!ms("running", &[]).resolved());
    }
    #[test]
    fn resolved_is_false_for_waiting() {
        assert!(!ms("waiting", &[]).resolved());
    }

    // ── MissionState::human_turn ─────────────────────────────────────────────

    #[test]
    fn human_turn_is_true_when_every_action_is_human() {
        assert!(ms("waiting", &[("approve", "human")]).human_turn());
    }
    #[test]
    fn human_turn_is_false_with_a_mixed_actor_set() {
        assert!(!ms("running", &[("go", "agent"), ("approve", "human")]).human_turn());
    }
    #[test]
    fn human_turn_is_false_when_there_are_no_actions() {
        assert!(!ms("running", &[]).human_turn());
    }

    // ── MissionState::agent_actions ──────────────────────────────────────────

    #[test]
    fn agent_actions_excludes_human_actions() {
        let s = ms("running", &[("go", "agent"), ("approve", "human")]);
        assert_eq!(s.agent_actions().len(), 1);
    }
    #[test]
    fn agent_actions_keeps_the_agent_transition() {
        let s = ms("running", &[("go", "agent"), ("approve", "human")]);
        assert_eq!(
            s.agent_actions().first().map(|a| a.transition.as_str()),
            Some("go")
        );
    }

    // ── MissionState::from_response (one field per test) ─────────────────────

    #[test]
    fn from_response_parses_the_status() {
        assert_eq!(
            MissionState::from_response(&full_response()).status,
            "failed"
        );
    }
    #[test]
    fn from_response_parses_the_reason() {
        assert_eq!(
            MissionState::from_response(&full_response())
                .reason
                .as_deref(),
            Some("guard_unmet")
        );
    }
    #[test]
    fn from_response_parses_the_version() {
        assert_eq!(MissionState::from_response(&full_response()).version, 3);
    }
    #[test]
    fn from_response_parses_the_mission_id() {
        assert_eq!(
            MissionState::from_response(&full_response()).mission_id,
            "m1"
        );
    }
    #[test]
    fn from_response_parses_the_goal() {
        assert_eq!(
            MissionState::from_response(&full_response())
                .goal
                .as_deref(),
            Some("do the thing")
        );
    }
    #[test]
    fn from_response_parses_the_outcome_count() {
        assert_eq!(
            MissionState::from_response(&full_response()).outcomes.len(),
            1
        );
    }
    #[test]
    fn from_response_parses_an_outcome_statement() {
        let s = MissionState::from_response(&full_response());
        assert_eq!(
            s.outcomes.first().map(|o| o.statement.as_str()),
            Some("it works")
        );
    }
    #[test]
    fn from_response_parses_an_outcome_met_flag() {
        let s = MissionState::from_response(&full_response());
        assert_eq!(s.outcomes.first().map(|o| o.met), Some(true));
    }
    #[test]
    fn from_response_parses_the_legal_action_count() {
        assert_eq!(
            MissionState::from_response(&full_response())
                .legal_actions
                .len(),
            2
        );
    }
    #[test]
    fn from_response_parses_a_legal_action_transition() {
        let s = MissionState::from_response(&full_response());
        assert_eq!(
            s.legal_actions.first().map(|a| a.transition.as_str()),
            Some("go")
        );
    }
    #[test]
    fn from_response_parses_a_human_actor() {
        let s = MissionState::from_response(&full_response());
        assert_eq!(
            s.legal_actions.get(1).map(|a| a.actor.as_str()),
            Some("human")
        );
    }
    #[test]
    fn from_response_defaults_status_to_running_when_absent() {
        assert_eq!(
            MissionState::from_response(&serde_json::json!({})).status,
            "running"
        );
    }
    #[test]
    fn from_response_defaults_a_link_actor_to_agent_when_absent() {
        let resp = serde_json::json!({ "links": [ { "rel": "go" } ] });
        let s = MissionState::from_response(&resp);
        assert_eq!(
            s.legal_actions.first().map(|a| a.actor.as_str()),
            Some("agent")
        );
    }
    #[test]
    fn from_response_yields_no_outcomes_when_absent() {
        assert!(
            MissionState::from_response(&serde_json::json!({}))
                .outcomes
                .is_empty()
        );
    }
    #[test]
    fn from_response_yields_no_legal_actions_when_absent() {
        assert!(
            MissionState::from_response(&serde_json::json!({}))
                .legal_actions
                .is_empty()
        );
    }

    // ── drive_mission outcomes ───────────────────────────────────────────────

    #[tokio::test]
    async fn drives_to_succeeded() {
        let gw = running_then(vec![
            ms("running", &[("go", "agent")]),
            ms("succeeded", &[]),
        ]);
        let out = drive_mission(&gw, &FirstActionChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(
            out,
            DriveOutcome::Resolved {
                status: "succeeded".into(),
                reason: None
            }
        );
    }

    #[tokio::test]
    async fn drives_to_failed_with_its_reason() {
        let mut failed = ms("failed", &[]);
        failed.reason = Some("guard_unmet".into());
        let gw = running_then(vec![ms("running", &[("go", "agent")]), failed]);
        let out = drive_mission(&gw, &FirstActionChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(
            out,
            DriveOutcome::Resolved {
                status: "failed".into(),
                reason: Some("guard_unmet".into())
            }
        );
    }

    #[tokio::test]
    async fn gives_up_at_a_non_terminal_dead_end() {
        let gw = running_then(vec![ms("running", &[])]);
        let out = drive_mission(&gw, &FirstActionChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(out, DriveOutcome::GaveUp);
    }

    #[tokio::test]
    async fn fires_a_deterministic_move_instead_of_giving_up() {
        // A running state whose only legal move is deterministic — the server
        // chain halted and left it for the driver. The driver must FIRE it (not
        // strand), advancing to resolution. NoneChooser proves the agent chooser
        // is never consulted on this path. Pre-fix this returned GaveUp.
        let gw = running_then(vec![
            ms("running", &[("vet_spec", "deterministic")]),
            ms("succeeded", &[]),
        ]);
        let out = drive_mission(&gw, &NoneChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(
            out,
            DriveOutcome::Resolved {
                status: "succeeded".into(),
                reason: None
            }
        );
    }

    #[tokio::test]
    async fn gives_up_when_the_chooser_declines() {
        let gw = running_then(vec![ms("running", &[("go", "agent")])]);
        let out = drive_mission(&gw, &NoneChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(out, DriveOutcome::GaveUp);
    }

    #[tokio::test]
    async fn surfaces_a_chooser_failure_as_chooser_failed_not_gave_up() {
        // FIX 1 — a chooser whose runner errors must surface the REAL error, not
        // masquerade as `GaveUp` ("no actionable move"). Pre-fix `.ok()?`
        // swallowed the error into `None` → GaveUp.
        let gw = running_then(vec![ms("running", &[("go", "agent")])]);
        let out = drive_mission(&gw, &ErrChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(
            out,
            DriveOutcome::ChooserFailed {
                source: "no API key configured".into()
            }
        );
    }

    #[tokio::test]
    async fn stops_at_the_step_bound() {
        let out = drive_mission(&LoopGateway, &FirstActionChooser, &Bus::new(), "m1", 3).await;
        assert_eq!(out, DriveOutcome::MaxSteps);
    }

    #[tokio::test]
    async fn returns_error_on_a_query_failure() {
        let out = drive_mission(&QueryErrGateway, &FirstActionChooser, &Bus::new(), "m1", 10).await;
        assert_eq!(out, DriveOutcome::Error("query boom".into()));
    }

    #[tokio::test]
    async fn returns_error_on_a_command_failure() {
        let out = drive_mission(
            &CommandErrGateway,
            &FirstActionChooser,
            &Bus::new(),
            "m1",
            10,
        )
        .await;
        assert_eq!(out, DriveOutcome::Error("command boom".into()));
    }

    // ── drive_mission bus events ─────────────────────────────────────────────

    #[tokio::test]
    async fn drive_publishes_a_status_event() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let gw = running_then(vec![
            ms("running", &[("go", "agent")]),
            ms("succeeded", &[]),
        ]);
        drive_mission(&gw, &FirstActionChooser, &bus, "m1", 10).await;
        let mut saw = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, MissionEvent::Status { .. }) {
                saw = true;
            }
        }
        assert!(saw);
    }

    #[tokio::test]
    async fn drive_publishes_a_resolved_event_on_resolution() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let gw = running_then(vec![
            ms("running", &[("go", "agent")]),
            ms("succeeded", &[]),
        ]);
        drive_mission(&gw, &FirstActionChooser, &bus, "m1", 10).await;
        let mut saw = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, MissionEvent::Resolved { .. }) {
                saw = true;
            }
        }
        assert!(saw);
    }

    #[tokio::test]
    async fn drive_publishes_a_chunk_on_an_autonomous_move() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let gw = running_then(vec![
            ms("running", &[("go", "agent")]),
            ms("succeeded", &[]),
        ]);
        drive_mission(&gw, &FirstActionChooser, &bus, "m1", 10).await;
        let mut saw = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, MissionEvent::Chunk { .. }) {
                saw = true;
            }
        }
        assert!(saw);
    }

    // ── drive_mission HITL park/resume ───────────────────────────────────────

    #[tokio::test]
    async fn resumes_and_succeeds_when_the_human_approves() {
        let mut gate = ms("waiting", &[("approve", "human")]);
        gate.goal = Some("approve?".into());
        let gw = running_then(vec![gate, ms("succeeded", &[])]);
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let driver = {
            let bus = bus.clone();
            tokio::spawn(
                async move { drive_mission(&gw, &FirstActionChooser, &bus, "m1", 10).await },
            )
        };
        while let Ok(ev) = rx.recv().await {
            if let MissionEvent::Interaction { request_id, .. } = ev {
                // The test plays the proven human answering the gate (P16: a
                // human_decision resolves only to a human-role principal).
                bus.answer(
                    request_id,
                    InteractionReply {
                        approved: true,
                        text: String::new(),
                    },
                    &human_principal(),
                )
                .expect("a human reply to a parked gate must be accepted");
                break;
            }
        }
        assert_eq!(
            driver.await.unwrap_or(DriveOutcome::GaveUp),
            DriveOutcome::Resolved {
                status: "succeeded".into(),
                reason: None
            }
        );
    }

    #[tokio::test]
    async fn declines_when_the_human_does_not_approve() {
        let gate = ms("waiting", &[("approve", "human")]);
        let gw = running_then(vec![gate]);
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let driver = {
            let bus = bus.clone();
            tokio::spawn(
                async move { drive_mission(&gw, &FirstActionChooser, &bus, "m1", 10).await },
            )
        };
        while let Ok(ev) = rx.recv().await {
            if let MissionEvent::Interaction { request_id, .. } = ev {
                bus.answer(
                    request_id,
                    InteractionReply {
                        approved: false,
                        text: String::new(),
                    },
                    &human_principal(),
                )
                .expect("a human reply to a parked gate must be accepted");
                break;
            }
        }
        assert_eq!(
            driver.await.unwrap_or(DriveOutcome::GaveUp),
            DriveOutcome::Declined
        );
    }

    // ── headless consumer ────────────────────────────────────────────────────

    #[tokio::test]
    async fn headless_policy_cannot_resolve_a_human_decision() {
        // P16 — the policy is not a proven human, so the bus refuses its reply
        // on an Approve gate: the interaction stays PARKED (it accumulates for
        // a later human drain) instead of being auto-resolved.
        let bus = Bus::new();
        let events = bus.subscribe();
        let _c = tokio::spawn(run_headless_consumer(
            events,
            bus.clone(),
            HeadlessPolicy::AutoApprove,
        ));
        let parked = {
            let bus = bus.clone();
            tokio::spawn(async move {
                bus.request_interaction("m1", InteractionKind::Approve, "?")
                    .await
            })
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_millis(200), parked).await;
        assert!(
            outcome.is_err(),
            "a human_decision must stay parked under a headless policy, not auto-resolve"
        );
        assert_eq!(bus.pending_count(), 1);
    }

    #[tokio::test]
    async fn headless_auto_approve_answers_a_non_decision_interaction_as_approved() {
        let bus = Bus::new();
        let events = bus.subscribe();
        let _c = tokio::spawn(run_headless_consumer(
            events,
            bus.clone(),
            HeadlessPolicy::AutoApprove,
        ));
        let reply = bus
            .request_interaction("m1", InteractionKind::Answer, "?")
            .await;
        assert!(reply.approved);
    }

    #[tokio::test]
    async fn headless_decline_answers_a_non_decision_interaction_as_not_approved() {
        let bus = Bus::new();
        let events = bus.subscribe();
        let _c = tokio::spawn(run_headless_consumer(
            events,
            bus.clone(),
            HeadlessPolicy::Decline,
        ));
        let reply = bus
            .request_interaction("m1", InteractionKind::Answer, "?")
            .await;
        assert!(!reply.approved);
    }

    #[tokio::test]
    async fn headless_consumer_stops_after_a_resolved_event() {
        let bus = Bus::new();
        let events = bus.subscribe();
        let consumer = tokio::spawn(run_headless_consumer(
            events,
            bus.clone(),
            HeadlessPolicy::AutoApprove,
        ));
        bus.publish(MissionEvent::Resolved {
            mission_id: "m1".into(),
            status: "succeeded".into(),
        });
        assert!(consumer.await.is_ok());
    }

    // ── AgentChooser ─────────────────────────────────────────────────────────

    fn chooser_returning(output: Value) -> AgentChooser {
        use crate::session::testing::MockSessionRunner;
        use crate::session::{AgentResult, AgentStatus};
        let runner = Arc::new(MockSessionRunner::completed(AgentResult {
            status: AgentStatus::Success,
            output,
            internal_monologue: None,
        }));
        AgentChooser::new(runner, "anthropic:claude", Duration::from_secs(5))
    }

    #[tokio::test]
    async fn agent_chooser_returns_the_named_legal_transition() {
        let chooser = chooser_returning(serde_json::json!({ "transition": "go" }));
        let chosen = chooser.choose(&ms("running", &[("go", "agent")])).await;
        assert_eq!(chosen, Ok(Some("go".to_string())));
    }

    #[tokio::test]
    async fn agent_chooser_declines_an_illegal_transition() {
        let chooser = chooser_returning(serde_json::json!({ "transition": "nope" }));
        let chosen = chooser.choose(&ms("running", &[("go", "agent")])).await;
        assert_eq!(chosen, Ok(None));
    }

    #[tokio::test]
    async fn agent_chooser_declines_when_output_has_no_transition() {
        let chooser = chooser_returning(serde_json::json!({}));
        let chosen = chooser.choose(&ms("running", &[("go", "agent")])).await;
        assert_eq!(chosen, Ok(None));
    }

    #[tokio::test]
    async fn agent_chooser_declines_when_there_are_no_agent_actions() {
        let chooser = chooser_returning(serde_json::json!({ "transition": "go" }));
        let chosen = chooser
            .choose(&ms("waiting", &[("approve", "human")]))
            .await;
        assert_eq!(chosen, Ok(None));
    }

    #[tokio::test]
    async fn agent_chooser_declines_when_the_runner_reports_no_result() {
        use crate::session::testing::MockSessionRunner;
        let runner = Arc::new(MockSessionRunner::no_result());
        let chooser = AgentChooser::new(runner, "anthropic:claude", Duration::from_secs(5));
        let chosen = chooser.choose(&ms("running", &[("go", "agent")])).await;
        // NoResult is a legitimate "no good move" — give up, not an error.
        assert_eq!(chosen, Ok(None));
    }

    #[tokio::test]
    async fn agent_chooser_declines_when_the_runner_times_out() {
        use crate::session::testing::MockSessionRunner;
        let runner = Arc::new(MockSessionRunner::timed_out());
        let chooser = AgentChooser::new(runner, "anthropic:claude", Duration::from_secs(5));
        let chosen = chooser.choose(&ms("running", &[("go", "agent")])).await;
        assert_eq!(chosen, Ok(None));
    }

    #[tokio::test]
    async fn agent_chooser_surfaces_a_runner_error_as_err() {
        // FIX 1 — a runner ERROR (e.g. missing API key) must PROPAGATE as `Err`,
        // never collapse into `Ok(None)` (give up). This is the root-cause test:
        // pre-fix `.ok()?` erased the error here.
        use crate::session::{AgentRunReport, AgentSession, AgentSessionRunner};
        use praxec_core::error::ExecutorError;

        struct ErroringRunner;
        #[async_trait]
        impl AgentSessionRunner for ErroringRunner {
            async fn run(&self, _session: AgentSession) -> Result<AgentRunReport, ExecutorError> {
                Err(ExecutorError::Permanent(
                    "AGENT_NO_API_KEY: no provider key configured".into(),
                ))
            }
        }

        let chooser = AgentChooser::new(
            Arc::new(ErroringRunner),
            "anthropic:claude",
            Duration::from_secs(5),
        );
        let chosen = chooser.choose(&ms("running", &[("go", "agent")])).await;
        match chosen {
            Err(e) => assert!(e.contains("AGENT_NO_API_KEY"), "unexpected error text: {e}"),
            other => panic!("expected Err surfacing the runner error, got {other:?}"),
        }
    }

    // ── end-to-end against a real in-memory runtime ──────────────────────────

    #[tokio::test]
    async fn drives_a_real_in_memory_workflow_to_succeeded() {
        use praxec_core::audit::{AuditSink, MemoryAuditSink};
        use praxec_core::error::ExecutorError;
        use praxec_core::guards::DefaultGuardEvaluator;
        use praxec_core::model::{ExecuteRequest, ExecuteResult, Principal, StartWorkflow};
        use praxec_core::ports::{Executor, ExecutorRegistry};
        use praxec_core::runtime::WorkflowRuntime;
        use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};

        struct NoopExec;
        #[async_trait]
        impl Executor for NoopExec {
            async fn execute(&self, _r: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
                Ok(ExecuteResult::default())
            }
        }
        struct NoopReg;
        impl ExecutorRegistry for NoopReg {
            fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
                Some(Arc::new(NoopExec))
            }
        }

        let config = serde_json::json!({
            "workflows": { "demo": {
                "initialState": "start",
                "outcomes": [
                    { "id": "done", "statement": "reached done", "check": "$.workflow.state == 'done'" }
                ],
                "states": {
                    "start": { "transitions": { "go": {
                        "target": "done", "actor": "agent", "executor": { "kind": "noop" }
                    } } },
                    "done": { "terminal": true, "outcome": "success" }
                }
            }}
        });

        let runtime = Arc::new(WorkflowRuntime::new(
            Arc::new(ConfigDefinitionStore::from_config(&config)),
            Arc::new(InMemoryWorkflowStore::new()),
            Arc::new(NoopReg),
            Arc::new(DefaultGuardEvaluator::new()),
            Arc::new(MemoryAuditSink::new()) as Arc<dyn AuditSink>,
        ));
        let start = runtime
            .start(StartWorkflow {
                definition_id: "demo".to_string(),
                input: serde_json::json!({}),
                principal: Principal::anonymous(),
                trace_id: None,
                run_id: None,
                depth: 0,
                parent: None,
            })
            .await
            .unwrap_or_else(|_| serde_json::json!({}));
        let mission_id = start
            .pointer("/workflow/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let gw = RuntimeMissionGateway::new(runtime, Principal::anonymous());
        let out = drive_mission(&gw, &FirstActionChooser, &Bus::new(), &mission_id, 10).await;
        assert_eq!(
            out,
            DriveOutcome::Resolved {
                status: "succeeded".into(),
                reason: None
            }
        );
    }
}
