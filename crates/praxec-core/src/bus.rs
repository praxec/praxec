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

struct Inner {
    events: broadcast::Sender<MissionEvent>,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<InteractionReply>>>,
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

    fn pending(&self) -> MutexGuard<'_, HashMap<RequestId, oneshot::Sender<InteractionReply>>> {
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
        self.pending().insert(request_id, tx);
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
    /// Returns `false` if the request is unknown (already answered / expired).
    pub fn answer(&self, request_id: RequestId, reply: InteractionReply) -> bool {
        let tx = self.pending().remove(&request_id);
        match tx {
            Some(tx) => tx.send(reply).is_ok(),
            None => false,
        }
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
    // Spawn a parked Approve interaction and return the bus, the parked task's
    // handle, and the announced Interaction event (so each test asserts ONE thing).

    async fn parked() -> (Bus, tokio::task::JoinHandle<InteractionReply>, MissionEvent) {
        let bus = Bus::new();
        let mut rx = bus.subscribe();
        let handle = {
            let bus = bus.clone();
            tokio::spawn(async move {
                bus.request_interaction("m1", InteractionKind::Approve, "ship it?")
                    .await
            })
        };
        let event = loop {
            match rx.recv().await {
                Ok(ev @ MissionEvent::Interaction { .. }) => break ev,
                Ok(_) => continue,
                Err(_) => {
                    break MissionEvent::Status {
                        mission_id: String::new(),
                        status: String::new(),
                    }
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
    async fn answer_returns_true_for_a_parked_request() {
        let (bus, _h, event) = parked().await;
        assert!(bus.answer(request_id_of(&event), InteractionReply::default()));
    }

    #[tokio::test]
    async fn answer_clears_the_pending_entry() {
        let (bus, _h, event) = parked().await;
        bus.answer(request_id_of(&event), InteractionReply::default());
        assert_eq!(bus.pending_count(), 0);
    }

    #[tokio::test]
    async fn answer_delivers_the_approved_flag_to_the_parked_orchestrator() {
        let (bus, handle, event) = parked().await;
        bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: true,
                text: String::new(),
            },
        );
        assert!(handle.await.unwrap_or_default().approved);
    }

    #[tokio::test]
    async fn answer_delivers_the_reply_text_to_the_parked_orchestrator() {
        let (bus, handle, event) = parked().await;
        bus.answer(
            request_id_of(&event),
            InteractionReply {
                approved: false,
                text: "go".into(),
            },
        );
        assert_eq!(handle.await.unwrap_or_default().text, "go");
    }

    #[tokio::test]
    async fn answer_returns_false_for_an_unknown_request() {
        let bus = Bus::new();
        assert!(!bus.answer(999, InteractionReply::default()));
    }

    #[tokio::test]
    async fn answering_an_unknown_request_leaves_a_parked_one_pending() {
        let (bus, _h, _event) = parked().await;
        bus.answer(424242, InteractionReply::default());
        assert_eq!(bus.pending_count(), 1);
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
