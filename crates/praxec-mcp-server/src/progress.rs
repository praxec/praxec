//! #18 — PUSH observability. During a long auto-drive a single `praxec.command`
//! call can run for minutes (a multi-turn agent, model escalation, sub-workflows).
//! The MCP protocol returns the tool result only when that call completes, so the
//! controlling model has no *live* view of progress — only after-the-fact `observe`.
//!
//! This bridges the existing audit stream to the MCP client: an [`AuditSink`]
//! decorator that delegates to the real sink (durable trail unchanged) **and**
//! best-effort forwards every event to the connected peer as a
//! `notifications/message` (logging) notification. The runtime emits an audit
//! event per transition hop / agent step *during* the drive, so the client sees
//! it stream in real time — a true push channel alongside the pull `observe`.
//!
//! Scoped to the serve path only (wired in `build_oneshot_server`); CLI
//! `command`/`check`/`observe` have no peer and keep the bare sink.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::Peer;
use rmcp::model::{
    LoggingLevel, LoggingMessageNotificationParam, ProgressNotificationParam, ProgressToken,
};
use rmcp::service::RoleServer;
use serde_json::json;

use praxec_core::audit::{AuditEvent, AuditSink};

/// Shared slot holding the connected MCP peer. The [`PraxecServer`] writes it
/// per `call_tool` (the peer is per-connection, available only in the request
/// context); the [`PeerBridgeAuditSink`] reads it to forward events. A cheap
/// `Clone` handle over one inner slot.
///
/// [`PraxecServer`]: crate::PraxecServer
#[derive(Clone, Default)]
pub struct ProgressPeer {
    peer: Arc<Mutex<Option<Peer<RoleServer>>>>,
    /// THIS call's `_meta.progressToken`, if the client requested progress.
    /// `notifications/progress` on this token is what resets the client's idle
    /// timeout during a long auto-drive (logging notifications don't) — so a
    /// multi-minute run with live sub-workflow events never trips the abort.
    token: Arc<Mutex<Option<ProgressToken>>>,
    /// Monotonic progress counter — MCP requires `progress` to strictly increase
    /// per token. Reset per call in [`Self::set_progress_token`].
    counter: Arc<AtomicU64>,
}

impl ProgressPeer {
    /// Record the connected peer (idempotent; cheap `Peer` clone). Called by the
    /// server on each `call_tool` so the bridge always has the live peer.
    pub fn set(&self, peer: Peer<RoleServer>) {
        if let Ok(mut slot) = self.peer.lock() {
            *slot = Some(peer);
        }
    }

    /// Capture THIS call's progress token (and reset the counter). `None` when
    /// the client did not request progress — then only the durable record +
    /// best-effort logging happen, no progress push.
    pub fn set_progress_token(&self, token: Option<ProgressToken>) {
        if let Ok(mut slot) = self.token.lock() {
            *slot = token;
        }
        self.counter.store(0, Ordering::Relaxed);
    }

    fn get(&self) -> Option<Peer<RoleServer>> {
        self.peer
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().cloned())
    }

    fn token(&self) -> Option<ProgressToken> {
        self.token.lock().ok().and_then(|slot| slot.clone())
    }

    fn next_progress(&self) -> f64 {
        self.counter.fetch_add(1, Ordering::Relaxed) as f64 + 1.0
    }

    /// The connected upstream peer, if a `call_tool` is (or was) in flight.
    /// Public so the #11 elicitation relay (wired in the `praxec` binary) can
    /// forward a downstream server's `elicitation/create` to it.
    pub fn current(&self) -> Option<Peer<RoleServer>> {
        self.get()
    }
}

/// [`AuditSink`] decorator that pushes each event to the connected MCP client as
/// a logging notification, after delegating to the wrapped sink. Build it with
/// [`progress_bridge`].
pub struct PeerBridgeAuditSink {
    inner: Arc<dyn AuditSink>,
    peer: ProgressPeer,
}

#[async_trait]
impl AuditSink for PeerBridgeAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        // Durable trail FIRST — a notification failure (closed transport, no
        // client) must never drop the governance record or fail the drive.
        let result = self.inner.record(event.clone()).await;

        if let Some(peer) = self.peer.get() {
            // Progress notification FIRST — this is the channel that resets the
            // client's idle timeout. Every audit event (each transition hop /
            // agent step / sub-workflow — they all carry `workflow_id` +
            // `parent_workflow_id` + `depth`) becomes a heartbeat, so a long
            // auto-drive with live nested activity never trips the client abort.
            // Only possible when the client requested progress (sent a token).
            if let Some(progress_token) = self.peer.token() {
                let _ = peer
                    .notify_progress(ProgressNotificationParam {
                        progress_token,
                        progress: self.peer.next_progress(),
                        total: None,
                        message: Some(match &event.workflow_id {
                            Some(id) => format!("{} · {id}", event.event_type),
                            None => event.event_type.clone(),
                        }),
                    })
                    .await;
            }

            // Also mirror to the logging channel (unchanged) for clients that
            // consume it. Best-effort; ignore send errors (client gone).
            // TODO(SEP-2577): rmcp deprecated logging notifications with no
            // replacement — the progress channel above is now the live heartbeat
            // and the durable audit record is the source of truth.
            let data = json!({
                "event_type": event.event_type,
                "workflow_id": event.workflow_id,
                "actor": event.actor,
                "timestamp": event.timestamp,
                "payload": event.payload,
            });
            #[allow(deprecated)]
            let _ = peer
                .notify_logging_message(LoggingMessageNotificationParam {
                    level: LoggingLevel::Info,
                    logger: Some("praxec".to_string()),
                    data,
                })
                .await;
        }

        result
    }

    async fn list_events(&self) -> Option<Vec<AuditEvent>> {
        self.inner.list_events().await
    }

    async fn try_list_events(&self) -> anyhow::Result<Option<Vec<AuditEvent>>> {
        self.inner.try_list_events().await
    }

    fn sink_kind(&self) -> &'static str {
        // Decorator — the observability fail-fast must see the REAL sink kind,
        // not the bridge.
        self.inner.sink_kind()
    }
}

/// Wrap an audit sink so its events ALSO push to the connected MCP client.
/// Returns the wrapped sink (use it as the runtime's audit sink) plus the shared
/// [`ProgressPeer`] slot — hand the slot to the [`PraxecServer`] so it captures
/// the peer on each call.
///
/// [`PraxecServer`]: crate::PraxecServer
pub fn progress_bridge(inner: Arc<dyn AuditSink>) -> (Arc<dyn AuditSink>, ProgressPeer) {
    let peer = ProgressPeer::default();
    let sink = Arc::new(PeerBridgeAuditSink {
        inner,
        peer: peer.clone(),
    });
    (sink, peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use praxec_core::audit::MemoryAuditSink;

    fn event(kind: &str) -> AuditEvent {
        AuditEvent {
            id: "e1".into(),
            timestamp: chrono::Utc::now(),
            workflow_id: Some("wf_1".into()),
            correlation_id: "c1".into(),
            actor: None,
            event_type: kind.into(),
            payload: json!({"k": "v"}),
            trace_id: None,
            run_id: None,
            parent_workflow_id: None,
            depth: 0,
        }
    }

    #[tokio::test]
    async fn delegates_to_inner_when_no_peer_connected() {
        let inner = Arc::new(MemoryAuditSink::default());
        let (sink, _peer) = progress_bridge(inner.clone());
        // No peer set → record must still succeed (best-effort push is a no-op).
        sink.record(event("workflow.transition")).await.unwrap();
        let events = sink.list_events().await.expect("memory sink lists");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "workflow.transition");
    }

    #[tokio::test]
    async fn list_events_reads_through_to_inner() {
        let inner = Arc::new(MemoryAuditSink::default());
        let (sink, _peer) = progress_bridge(inner);
        sink.record(event("a")).await.unwrap();
        sink.record(event("b")).await.unwrap();
        assert_eq!(sink.list_events().await.map(|e| e.len()), Some(2));
    }
}
