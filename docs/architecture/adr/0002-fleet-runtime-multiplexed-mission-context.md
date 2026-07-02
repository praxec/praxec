# ADR-0002: Fleet runtime — multiplexed, per-connection principal, switchable mission context

**Status:** Accepted; implementation pending

> **Implementation note (2026-06-17):** This decision is accepted but not
> yet built. Today `praxec serve` / `StdioGateway` spawns its own child
> `praxec` process per cockpit — single-peer stdio with an anonymous
> principal — rather than a multiplexed attach to a shared running fleet.
> The fleet-runtime multiplexing described below lands in a later increment.

**Date:** 2026-06-09

## Context

[ADR-0001](0001-headless-runtime-surfaces-attach.md) established that the
runtime is headless and UI surfaces attach as governed clients over the two MCP
tools. But today `praxec serve` (`crates/praxec/src/gateway.rs`) is
**single-peer stdio**: one connection, no per-connection principal, no event
stream. Mission Control's premise — observe and direct work across *all* agents
and missions in the system, switching between them — is not buildable on that.

"Single-mission cockpit" is a misnomer. What's needed is a **switchable mission
context over a fleet**: a high-level view of everything executing, with drill-in
to any one mission (progressive disclosure — see
[ADR-0003](0003-mission-control-view-model.md)).

## Decision

The runtime becomes a **fleet runtime**: it serves many concurrent governed
clients and exposes the whole fleet of missions, not one.

1. **Multiplexed transport.** The runtime accepts multiple concurrent attached
   clients — execution agents *and* UI surfaces — over a multi-client transport.
   (Single-client stdio remains one valid transport for the agent-drives-one-
   workflow case; it is not the only one.)

2. **Per-connection principal.** Each attached client carries its identity
   (`agent:*`, `human:cockpit`, `agent:mission-control`, …) on its connection,
   so every action across the fleet is attributable and governed identically
   per ADR-0001 §3. There is no anonymous or shared mutation path.

3. **Mission context is switchable, not singular.** The runtime exposes the
   **fleet**: a list/overview of all live missions (workflows) with their state,
   and per-mission drill-in. The cockpit selects a *current mission context*; it
   is never bound to "the" mission.

4. **"See what's executing" = subscription on the audit/transition stream.**
   Liveness is delivered by a subscription on the existing audit sink (the
   system of record) plus a **fleet read-mode** on `praxec.query` (list
   missions). This deliberately does *not* use true MCP resource-subscriptions —
   the static two-tool surface (ADR-0001) stays intact; the stream is an
   attach-time capability, not a third tool.

## Consequences

- **Positive.** The cockpit can observe and direct the whole fleet; multiple
  operators and the out-of-band Mission Control LLM attach as peer governed
  clients (ADR-0001 §3); one mission can be watched by several surfaces at once,
  all seeing the same legal-actions set.
- **Costs.** This is the real budget of Mission Control — multiplexed transport
  + session/connection management + per-connection principal threading, and an
  audit sink that supports subscription/fan-out. The fleet read-mode extends
  `praxec.query` rather than adding a tool.
- **Sequencing.** This runtime work blocks the "watch the fleet / administer
  out-of-band" premise; until it lands, the cockpit can only drive a single
  attached workflow. It is the first increment, not a follow-up.

## Alternatives considered

- **Stay single-peer stdio.** Rejected — cannot watch a fleet or attach a second
  surface; reduces Mission Control to a prettier single-workflow TUI.
- **True MCP `resources/subscribe`.** Rejected — breaks the static two-tool
  invariant the harness is built on; the audit stream already is the system of
  record, so subscribe there.
- **Cockpit polls per-workflow `query` in a loop.** Rejected — no fleet view, no
  real liveness, and N× the runtime load.

## References

- Runtime / serve path: `crates/praxec/src/gateway.rs`
- MCP surface: `crates/praxec-mcp-server/src/tools.rs`
- Audit sink (subscription target): `crates/praxec-core` audit
- Relates to: [ADR-0001](0001-headless-runtime-surfaces-attach.md),
  [ADR-0003](0003-mission-control-view-model.md)
