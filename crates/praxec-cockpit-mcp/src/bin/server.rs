//! `praxec-cockpit-mcp` — serve the cockpit's interaction model over MCP
//! (stdio) so an external agent can drive its navigation ops.
//!
//! Today this resolves tool calls against the demo fleet snapshot (the
//! dispatch + protocol binding are real; wiring to a live running cockpit is
//! the fleet-runtime increment, ADR-0002).

use praxec_cockpit_mcp::CockpitServer;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    CockpitServer::demo().serve_stdio().await
}
