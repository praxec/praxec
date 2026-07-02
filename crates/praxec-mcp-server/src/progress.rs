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

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rmcp::model::{LoggingLevel, LoggingMessageNotificationParam};
use rmcp::service::RoleServer;
use rmcp::Peer;
use serde_json::json;

use praxec_core::audit::{AuditEvent, AuditSink};

/// Shared slot holding the connected MCP peer. The [`PraxecServer`] writes it
/// per `call_tool` (the peer is per-connection, available only in the request
/// context); the [`PeerBridgeAuditSink`] reads it to forward events. A cheap
/// `Clone` handle over one inner slot.
///
/// [`PraxecServer`]: crate::PraxecServer
#[derive(Clone, Default)]
pub struct ProgressPeer(Arc<Mutex<Option<Peer<RoleServer>>>>);

impl ProgressPeer {
    /// Record the connected peer (idempotent; cheap `Peer` clone). Called by the
    /// server on each `call_tool` so the bridge always has the live peer.
    pub fn set(&self, peer: Peer<RoleServer>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(peer);
        }
    }

    fn get(&self) -> Option<Peer<RoleServer>> {
        self.0.lock().ok().and_then(|slot| slot.as_ref().cloned())
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
            let data = json!({
                "event_type": event.event_type,
                "workflow_id": event.workflow_id,
                "actor": event.actor,
                "timestamp": event.timestamp,
                "payload": event.payload,
            });
            // Best-effort push; ignore send errors (client gone / not subscribed).
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
