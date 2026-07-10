//! ADR-0009 — the **interaction bus**. The channel fabric that connects running
//! **orchestrators** (publishers) to **consumers** (the mediator, or a headless
//! policy). It separates execution from interaction: an orchestrator emits events
//! and **parks** on a human request without knowing *who* answers; a consumer
//! subscribes, answers HITL requests, and the orchestrator **resumes**.
//!
//! Built on `tokio::sync` — no actor framework:
//! - **`broadcast`** fans every [`MissionEvent`] out to all subscribers.
//! - **`oneshot`** is the park/resume: an orchestrator awaits a reply channel.
//!
//! The key shape: a broadcast message is *cloned* to every subscriber, so it can't
//! carry a single-use reply channel. A parked interaction therefore carries a
//! [`RequestId`]; the reply `oneshot` lives in the hub, and a consumer answers via
//! [`Bus::answer`]. That keeps "who parked" and "who answers" fully decoupled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::{broadcast, oneshot};

use crate::model::Principal;

/// Identifies a parked interaction so a reply routes back to the right
/// orchestrator without putting the reply channel on the (cloning) broadcast.
pub type RequestId = u64;

/// The kind of human-in-the-loop interaction an orchestrator parks on
/// (ADR-0009 / SPEC §29 Hitl kinds).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionKind {
    /// A gate — approve, or send back.
    Approve,
    /// Answer a question the orchestrator asked.
    Answer,
    /// Fill a typed form (payload carried as JSON text in the reply).
    Form,
    /// Open-ended back-and-forth.
    Discuss,
}

impl InteractionKind {
    /// P16 — is this a `human_decision`: a human-only gate whose resolution must
    /// be bound to a proven-human identity? The guard rule: **reject a
    /// `human_decision` resolution whose principal is not a proven human,
    /// regardless of the connection's role.** [`Bus::answer`] enforces it.
    ///
    /// Only [`InteractionKind::Approve`] (the gate) is a decision; the
    /// conversational kinds may be relayed/consumed by a mediator and stay
    /// ungated. Exhaustive match, so a new kind forces an explicit choice here.
    pub fn requires_human(self) -> bool {
        match self {
            InteractionKind::Approve => true,
            InteractionKind::Answer | InteractionKind::Form | InteractionKind::Discuss => false,
        }
    }
}

/// P16 — why [`Bus::answer`] refused to deliver a reply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BusError {
    /// No parked interaction for this request (already answered or expired).
    #[error("no parked interaction for request {0} (already answered or expired)")]
    UnknownRequest(RequestId),
    /// `HITL_NON_HUMAN_RESOLVER` — a `human_decision` may be resolved only by a
    /// proven-human principal (one whose roles include
    /// [`Principal::HUMAN_ROLE`]). An agent/LLM/policy or role-less principal is
    /// refused fail-closed and the interaction **stays parked** for a later
    /// human drain. Relaying the request to a human is fine; answering is not.
    #[error(
        "HITL_NON_HUMAN_RESOLVER: request {request_id} is a human_decision \
         ({kind:?}) and principal `{subject}` is not a proven human (no `human` \
         role); the reply is refused and the interaction stays parked for a \
         human drain"
    )]
    NonHumanResolver {
        request_id: RequestId,
        kind: InteractionKind,
        subject: String,
    },
}

/// A human's reply to a parked interaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InteractionReply {
    /// For [`InteractionKind::Approve`]: accepted? (ignored by the others).
    pub approved: bool,
    /// Free-text answer (Answer / Discuss) or form payload as JSON (Form).
    pub text: String,
}

/// An event an orchestrator publishes to the bus. **Cloned to every subscriber**
/// (broadcast), so it carries NO reply channel — a parked interaction carries a
/// [`RequestId`] and is answered via [`Bus::answer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionEvent {
    /// Streaming model output from a mission's orchestrator.
    Chunk { mission_id: String, text: String },
    /// The mission's resolution status changed (ADR-0008 `running|waiting|…`).
    Status { mission_id: String, status: String },
    /// The orchestrator parked on a human interaction (HITL). The mission is now
    /// `waiting`; it resumes when a consumer answers `request_id`.
    Interaction {
        mission_id: String,
        request_id: RequestId,
        kind: InteractionKind,
        prompt: String,
    },
    /// The mission reached a terminal resolution.
    Resolved { mission_id: String, status: String },
}

/// A parked interaction awaiting an answer. Carries its [`InteractionKind`] so
/// [`Bus::answer`] can enforce the P16 origin rule (a `human_decision` resolves
/// only to a proven-human principal) at the single choke point where every
/// reply is accepted.
struct Pending {
    kind: InteractionKind,
    tx: oneshot::Sender<InteractionReply>,
}

struct Inner {
    events: broadcast::Sender<MissionEvent>,
    pending: Mutex<HashMap<RequestId, Pending>>,
    next_id: AtomicU64,
}

/// RAII cleanup for a parked interaction: removes the `pending` entry on drop, so
/// a cancelled/timed-out `request_interaction` future never leaves its
/// `oneshot::Sender` stranded in the map (which would leak in a long-lived bus).
/// A no-op once `answer()` has already taken the entry.
struct PendingGuard {
    inner: Arc<Inner>,
    request_id: RequestId,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let mut pending = self
            .inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.remove(&self.request_id);
    }
}

/// The interaction bus — a hub orchestrators publish to and consumers subscribe
/// to. A cheap `Clone` handle (shares one inner hub), so publishers and consumers
/// just hold their own clone.
#[derive(Clone)]
pub struct Bus {
    inner: Arc<Inner>,
}

impl Bus {
    /// A new bus. `capacity` bounds the broadcast backlog; a subscriber that lags
    /// past it sees a `Lagged` error and resumes from the newest events (events are
    /// advisory — the durable mission state lives in the store).
    pub fn with_capacity(capacity: usize) -> Self {
        let (events, _) = broadcast::channel(capacity.max(1));
        Self {
            inner: Arc::new(Inner {
                events,
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// A bus with a sensible default backlog (256 events).
    pub fn new() -> Self {
        Self::with_capacity(256)
    }

    fn pending(&self) -> MutexGuard<'_, HashMap<RequestId, Pending>> {
        // Recover from a poisoned lock rather than panic (the map is plain data).
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Consumer side: subscribe to the event stream.
    pub fn subscribe(&self) -> broadcast::Receiver<MissionEvent> {
        self.inner.events.subscribe()
    }

    /// Orchestrator side: emit a non-blocking event (chunk / status / resolved).
    /// Dropped if there are no subscribers — that's fine (headless, no observer).
    pub fn publish(&self, event: MissionEvent) {
        let _ = self.inner.events.send(event);
    }

    /// Orchestrator side: **park** on a human interaction. Registers a reply
    /// channel, announces the request on the bus, then awaits the answer — the
    /// mission is `waiting` until a consumer calls [`Bus::answer`]. Returns the
    /// reply, or a default (declined / empty) reply if the bus is torn down.
    pub async fn request_interaction(
        &self,
        mission_id: impl Into<String>,
        kind: InteractionKind,
        prompt: impl Into<String>,
    ) -> InteractionReply {
        let (tx, rx) = oneshot::channel();
        let request_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        // Register BEFORE announcing, so a consumer that answers the instant it
        // sees the event always finds the pending entry. The lock guard is dropped
        // at `;` (never held across the await below).
        self.pending().insert(request_id, Pending { kind, tx });
        // RAII cleanup: if this future is dropped while still parked (the mission
        // was cancelled / timed out mid-interaction), the guard removes the pending
        // entry so its `oneshot::Sender` can't accumulate in a long-lived bus. On
        // the normal path `answer()` removes the entry first; the guard then no-ops.
        let _guard = PendingGuard {
            inner: self.inner.clone(),
            request_id,
        };
        self.publish(MissionEvent::Interaction {
            mission_id: mission_id.into(),
            request_id,
            kind,
            prompt: prompt.into(),
        });
        rx.await.unwrap_or_default()
    }

    /// Consumer side: **answer** a parked interaction, resuming its orchestrator.
    /// `principal` is WHO is answering — it must come from the consumer's
    /// authenticated channel (e.g. the gateway's configured identity), never
    /// from model-authored content.
    ///
    /// P16 origin enforcement: when the parked interaction is a
    /// `human_decision` ([`InteractionKind::requires_human`]), the reply is
    /// accepted only from a proven-human principal ([`Principal::is_human`]).
    /// Any other principal — an agent, an LLM (including the top, human-facing
    /// one running headless), a policy, or an anonymous/role-less caller — gets
    /// [`BusError::NonHumanResolver`] and the interaction **stays parked**
    /// (fail-closed): the orchestrator keeps waiting for a human drain.
    ///
    /// Errors with [`BusError::UnknownRequest`] when the request has no parked
    /// entry (already answered / expired).
    pub fn answer(
        &self,
        request_id: RequestId,
        reply: InteractionReply,
        principal: &Principal,
    ) -> Result<(), BusError> {
        let mut pending = self.pending();
        let entry = pending
            .get(&request_id)
            .ok_or(BusError::UnknownRequest(request_id))?;
        if entry.kind.requires_human() && !principal.is_human() {
            // Reject WITHOUT removing the entry — the interaction stays parked
            // so a proven human can still resolve it later.
            return Err(BusError::NonHumanResolver {
                request_id,
                kind: entry.kind,
                subject: principal.subject.clone(),
            });
        }
        let entry = pending
            .remove(&request_id)
            .ok_or(BusError::UnknownRequest(request_id))?;
        drop(pending);
        // A closed receiver means the parked future was dropped (cancelled /
        // timed out) — the request is effectively expired.
        entry
            .tx
            .send(reply)
            .map_err(|_| BusError::UnknownRequest(request_id))
    }

    /// Count of parked (unanswered) interactions — a consumer's inbox depth.
    pub fn pending_count(&self) -> usize {
        self.pending().len()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ────────────────────────────────────────────────────────────
    // Spawn a parked interaction and return the bus, the parked task's handle,
    // and the announced Interaction event (so each test asserts ONE thing).

    /// A proven-human principal (roles contain `human`).
    fn human() -> Principal {
        Principal {
            subject: "operator".into(),
            roles: vec![Principal::HUMAN_ROLE.into()],
            permissions: Vec::new(),
        }
    }

    /// A non-human principal — an agent identity with roles, none of them human.
    fn agent() -> Principal {
        Principal {
            subject: "agent:orchestrator".into(),
            roles: vec!["agent".into()],
            permissions: Vec::new(),
        }
    }

    async fn parked() -> (Bus, tokio::task::JoinHandle<InteractionReply>, MissionEvent) {
        parked_as(InteractionKind::Approve).await
    }

    async fn parked_as(
        kind: InteractionKind,
    ) -> (Bus, tokio::task::JoinHandle<InteractionReply>, MissionEvent) {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let handle = {
            let bus = bus.clone();
            tokio::spawn(async move { bus.request_interaction("m1", kind, "ship it?").await })
        };
        let event = loop {
            match rx.recv().await {
                Ok(ev @ MissionEvent::Interaction { .. }) => break ev,
                Ok(_) => continue,
                Err(_) => {
                    break MissionEvent::Status {
                        mission_id: String::new(),
                        status: String::new(),
                    };
                }
            }
        };
        (bus, handle, event)
    }

    fn request_id_of(ev: &MissionEvent) -> RequestId {
        match ev {
            MissionEvent::Interaction { request_id, .. } => *request_id,
            _ => 0,
        }
    }

    // ── publish / subscribe ──────────────────────────────────────────────────

    #[tokio::test]
    async fn a_published_event_reaches_a_subscriber() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        bus.publish(MissionEvent::Chunk {
            mission_id: "m1".into(),
            text: "hi".into(),
        });
        assert!(matches!(rx.recv().await, Ok(MissionEvent::Chunk { .. })));
    }

    #[tokio::test]
    async fn a_published_chunk_preserves_its_text() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        bus.publish(MissionEvent::Chunk {
            mission_id: "m1".into(),
            text: "hello".into(),
        });
        let text = match rx.recv().await {
            Ok(MissionEvent::Chunk { text, .. }) => text,
            _ => String::new(),
        };
        assert_eq!(text, "hello");
    }

    #[tokio::test]
    async fn a_published_chunk_preserves_its_mission_id() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        bus.publish(MissionEvent::Chunk {
            mission_id: "m7".into(),
            text: "x".into(),
        });
        let id = match rx.recv().await {
            Ok(MissionEvent::Chunk { mission_id, .. }) => mission_id,
            _ => String::new(),
        };
        assert_eq!(id, "m7");
    }

    #[tokio::test]
    async fn a_second_subscriber_also_receives_the_event() {
        let bus = Bus::new();
        let _a = bus.subscribe();
        let mut b = bus.subscribe();
        bus.publish(MissionEvent::Status {
            mission_id: "m1".into(),
            status: "running".into(),
        });
        assert!(matches!(b.recv().await, Ok(MissionEvent::Status { .. })));
    }

    #[tokio::test]
    async fn an_event_published_before_a_subscriber_joins_is_not_delivered_to_it() {
        let bus = Bus::new();
        bus.publish(MissionEvent::Chunk {
            mission_id: "m1".into(),
            text: "early".into(),
        });
        let mut late = bus.subscribe();
        bus.publish(MissionEvent::Chunk {
            mission_id: "m1".into(),
            text: "later".into(),
        });
        let text = match late.recv().await {
            Ok(MissionEvent::Chunk { text, .. }) => text,
            _ => String::new(),
        };
        assert_eq!(text, "later");
    }

    // ── park (request_interaction) ───────────────────────────────────────────

    #[tokio::test]
    async fn parking_announces_an_interaction_event() {
        let (_bus, _h, event) = parked().await;
        assert!(matches!(event, MissionEvent::Interaction { .. }));
    }

    #[tokio::test]
    async fn the_interaction_event_carries_the_kind() {
        let (_bus, _h, event) = parked().await;
        let kind = match event {
            MissionEvent::Interaction { kind, .. } => Some(kind),
            _ => None,
        };
        assert_eq!(kind, Some(InteractionKind::Approve));
    }

    #[tokio::test]
    async fn the_interaction_event_carries_the_prompt() {
        let (_bus, _h, event) = parked().await;
        let prompt = match event {
            MissionEvent::Interaction { prompt, .. } => prompt,
            _ => String::new(),
        };
        assert_eq!(prompt, "ship it?");
    }

    #[tokio::test]
    async fn the_interaction_event_carries_the_mission_id() {
        let (_bus, _h, event) = parked().await;
        let id = match event {
            MissionEvent::Interaction { mission_id, .. } => mission_id,
            _ => String::new(),
        };
        assert_eq!(id, "m1");
    }

    #[tokio::test]
    async fn the_interaction_event_carries_a_nonzero_request_id() {
        let (_bus, _h, event) = parked().await;
        assert_ne!(request_id_of(&event), 0);
    }

    #[tokio::test]
    async fn a_parked_request_is_pending() {
        let (bus, _h, _event) = parked().await;
        assert_eq!(bus.pending_count(), 1);
    }

    #[tokio::test]
    async fn an_abandoned_parked_interaction_cleans_up_its_pending_entry() {
        // The leak fix: a parked orchestrator that is cancelled (mission timed out /
        // aborted) must not strand its oneshot sender in `pending`.
        let (bus, handle, _event) = parked().await;
        handle.abort();
        let _ = handle.await; // the cancelled future — and its PendingGuard — drops here
        assert_eq!(bus.pending_count(), 0);
    }

    // ── answer (resume) ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn answer_succeeds_for_a_parked_request() {
        let (bus, _h, event) = parked().await;
        assert!(
            bus.answer(request_id_of(&event), InteractionReply::default(), &human())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn answer_clears_the_pending_entry() {
        let (bus, _h, event) = parked().await;
        let _ = bus.answer(request_id_of(&event), InteractionReply::default(), &human());
        assert_eq!(bus.pending_count(), 0);
    }

    #[tokio::test]
    async fn answer_delivers_the_approved_flag_to_the_parked_orchestrator() {
        let (bus, handle, event) = parked().await;
        let _ = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: true,
                text: String::new(),
            },
            &human(),
        );
        assert!(handle.await.unwrap_or_default().approved);
    }

    #[tokio::test]
    async fn answer_delivers_the_reply_text_to_the_parked_orchestrator() {
        let (bus, handle, event) = parked().await;
        let _ = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: false,
                text: "go".into(),
            },
            &human(),
        );
        assert_eq!(handle.await.unwrap_or_default().text, "go");
    }

    #[tokio::test]
    async fn answer_errors_for_an_unknown_request() {
        let bus = Bus::new();
        assert_eq!(
            bus.answer(999, InteractionReply::default(), &human()),
            Err(BusError::UnknownRequest(999))
        );
    }

    #[tokio::test]
    async fn answering_an_unknown_request_leaves_a_parked_one_pending() {
        let (bus, _h, _event) = parked().await;
        let _ = bus.answer(424242, InteractionReply::default(), &human());
        assert_eq!(bus.pending_count(), 1);
    }

    // ── P16 origin enforcement (human_decision resolves only to a human) ─────

    #[tokio::test]
    async fn a_human_principal_may_resolve_a_human_decision() {
        let (bus, handle, event) = parked_as(InteractionKind::Approve).await;
        let result = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: true,
                text: String::new(),
            },
            &human(),
        );
        assert!(result.is_ok());
        assert!(handle.await.unwrap_or_default().approved);
    }

    #[tokio::test]
    async fn an_agent_principal_answering_a_human_decision_is_rejected() {
        let (bus, _h, event) = parked_as(InteractionKind::Approve).await;
        let result = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: true,
                text: String::new(),
            },
            &agent(),
        );
        assert!(matches!(result, Err(BusError::NonHumanResolver { .. })));
    }

    #[tokio::test]
    async fn the_rejection_names_the_offending_subject() {
        let (bus, _h, event) = parked_as(InteractionKind::Approve).await;
        let err = bus
            .answer(request_id_of(&event), InteractionReply::default(), &agent())
            .expect_err("a non-human resolver must be rejected");
        assert!(matches!(
            err,
            BusError::NonHumanResolver { ref subject, .. } if subject == "agent:orchestrator"
        ));
    }

    #[tokio::test]
    async fn an_anonymous_principal_answering_a_human_decision_is_rejected() {
        // Fail-closed: no identity claim at all → anonymous, role-less → refused.
        let (bus, _h, event) = parked_as(InteractionKind::Approve).await;
        let result = bus.answer(
            request_id_of(&event),
            InteractionReply::default(),
            &Principal::anonymous(),
        );
        assert!(matches!(result, Err(BusError::NonHumanResolver { .. })));
    }

    #[tokio::test]
    async fn a_roleless_principal_answering_a_human_decision_is_rejected() {
        // A subject with NO roles is not a proven human (fail-closed).
        let (bus, _h, event) = parked_as(InteractionKind::Approve).await;
        let roleless = Principal {
            subject: "operator".into(),
            roles: Vec::new(),
            permissions: Vec::new(),
        };
        let result = bus.answer(
            request_id_of(&event),
            InteractionReply::default(),
            &roleless,
        );
        assert!(matches!(result, Err(BusError::NonHumanResolver { .. })));
    }

    #[tokio::test]
    async fn a_rejected_non_human_answer_leaves_the_request_parked() {
        let (bus, _h, event) = parked_as(InteractionKind::Approve).await;
        let _ = bus.answer(request_id_of(&event), InteractionReply::default(), &agent());
        assert_eq!(bus.pending_count(), 1);
    }

    #[tokio::test]
    async fn a_human_can_still_resolve_after_a_non_human_was_rejected() {
        let (bus, handle, event) = parked_as(InteractionKind::Approve).await;
        let _ = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: true,
                text: String::new(),
            },
            &agent(),
        );
        let result = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: true,
                text: String::new(),
            },
            &human(),
        );
        assert!(result.is_ok());
        assert!(handle.await.unwrap_or_default().approved);
    }

    #[tokio::test]
    async fn a_non_human_may_answer_a_non_decision_interaction() {
        // Only human_decision kinds are gated; a conversational Answer is not.
        let (bus, handle, event) = parked_as(InteractionKind::Answer).await;
        let result = bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: false,
                text: "42".into(),
            },
            &agent(),
        );
        assert!(result.is_ok());
        assert_eq!(handle.await.unwrap_or_default().text, "42");
    }

    #[tokio::test]
    async fn only_the_approve_kind_is_a_human_decision() {
        assert!(InteractionKind::Approve.requires_human());
        assert!(!InteractionKind::Answer.requires_human());
        assert!(!InteractionKind::Form.requires_human());
        assert!(!InteractionKind::Discuss.requires_human());
    }

    #[tokio::test]
    async fn request_ids_are_monotonic() {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        for _ in 0..2 {
            let bus = bus.clone();
            tokio::spawn(async move {
                bus.request_interaction("m1", InteractionKind::Answer, "?")
                    .await
            });
        }
        let mut ids = Vec::new();
        while ids.len() < 2 {
            if let Ok(ev @ MissionEvent::Interaction { .. }) = rx.recv().await {
                ids.push(request_id_of(&ev));
            }
        }
        ids.sort_unstable();
        assert!(ids.first() < ids.last());
    }

    #[tokio::test]
    async fn a_fresh_bus_has_no_pending_requests() {
        assert_eq!(Bus::new().pending_count(), 0);
    }
}
