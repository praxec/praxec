# ADR-0001: Headless runtime; UI surfaces attach as governed clients

**Status:** Accepted

**Date:** 2026-06-09

## Context

Praxec's runtime is the source of truth for governed work: `praxec serve`
(`crates/praxec/src/gateway.rs`) runs an MCP server over stdio that owns
the workflow engine, executors, the guard kernel, the audit sink, the durable
store. Its entire
governance contract — transition validation, optimistic-locking versions,
actor/guard gating, evidence, audit — is enforced behind exactly two MCP
tools: `praxec.query` (read: home/search/describe/get/explain by arg-shape)
and `praxec.command` (write: start/submit/define), per SPEC §30/§32 and
`crates/praxec-mcp-server/src/tools.rs`. An external agent (Claude Code,
Cursor, anything that speaks MCP) drives the runtime *only* through that surface
and sees only the legal next moves the runtime hands back as HATEOAS links.

The Mission Control design
(`docs/architecture/mission-control-design.md`) introduces two
human surfaces: the **cockpit** (`crates/praxec-cockpit`, a ratatui TUI —
the directing/supervising home) and a **chat** (the existing aether-tui ACP TUI
in `crates/praxec-tui`, dropped into for a bounded coding session). The
architectural question this ADR settles: where does the runtime live relative
to these surfaces, and how do humans direct missions without becoming a
privileged backdoor around the governance the runtime exists to enforce?

Today the cockpit is fixture-backed: `gateway.rs::FakeGateway` returns a static
`GatewayResponse`, with the live path explicitly deferred ("the live in-process
/ MCP-stdio gateway is the next increment"). That deferral makes this the right
moment to fix the boundary before a live client is built against the wrong one.

## Decision

The runtime is **headless** and is the single source of truth. UI surfaces
**attach** to a running runtime as clients; they observe and direct, never host.

1. **The runtime runs without any UI.** `praxec serve` + executors + workflow
   engine + CPM plan is the complete, authoritative runtime. It has no
   dependency on the cockpit, the chat, or any display surface, and is fully
   functional driven by an MCP agent alone.

2. **The cockpit is a view + controller, never the runtime.**
   `praxec-cockpit` must never become or embed the engine, the store, the
   guard evaluator, or the executors. It holds no governance state of record.
   Its `Gateway` trait is a *client* of `praxec.query`/`praxec.command` —
   `FakeGateway` is replaced by a live client that attaches to a running
   runtime, not by importing `WorkflowRuntime` in-process as an authority. (An
   in-process attach for single-binary convenience is permitted *only* as a
   client over the same two-tool surface, with identical validation — never as
   a privileged side door into engine internals.)

3. **Surfaces attach via the same governed MCP surface agents use.** A human
   directing from the cockpit, or chatting, issues
   `praxec.command`/`praxec.query` calls exactly as an agent does. A human
   is therefore just **another governed principal**: their transitions are
   actor/guard-validated, version-checked, and audited identically. There is no
   human-only mutation path that bypasses transition validation. This is the
   harness thesis applied to the UI itself — "the LLM is bounded" and "the human
   is bounded" are the *same* mechanism, which is what makes the bound visible
   rather than asserted.

4. **Surfaces are peers with a clean handoff, not a hierarchy.** The cockpit
   owns the terminal; opening a chat session suspends it (drops alt-screen),
   hands the terminal to the chat for that bounded session, and resumes on exit
   (the `git`→`$EDITOR` pattern). Each surface owns its own event loop. Neither
   embeds the other; both attach to the same runtime.

5. **Vocabulary.** "wisp" (aether-tui) is the chat *technology* and stays an
   implementation detail. The user-facing experience is "a chat." The tech name
   never leaks into UX strings, help text, or labels. Surfaces are "the cockpit"
   and "the chat."

## Consequences

**Positive.** One enforcement point: every mutation — human or agent — passes
the same guards, locks, and audit, so the governance story has no asterisk. The
runtime is independently testable and deployable headless (CI, sidecar, server)
with no UI in the build. Multiple surfaces (and multiple humans/agents) can
attach to one mission concurrently and all see the same legal-actions set. The
cockpit stays thin: a bug in a view can't corrupt mission state of record.

**Costs / constraints.** The cockpit needs a real MCP/stdio (or
in-process-over-the-two-tools) client to replace `FakeGateway` — currently
unbuilt; until then the cockpit shows fixtures only. The two-tool surface must
expose everything the cockpit needs to render Mission/Plan/Flow/Trace; any
cockpit need that tempts a direct engine reach-in is a signal to extend
`praxec.query`, not to break the boundary. Human-as-principal requires the
cockpit to carry an identity claim (`_meta` principal / `gateway.principal`) so
its commands are attributable in the audit trail.

**Enforced by review.** No `WorkflowRuntime`, store, guard-evaluator, or
executor type may be constructed as an authority inside `praxec-cockpit`;
the crate depends only on the client surface. Violations are an architecture
regression, not a refactor.

## Alternatives considered

- **Cockpit embeds the runtime (UI hosts the engine).** Rejected: makes the TUI
  the source of truth, so headless/CI/server deployments lose governance, and a
  second surface (chat, a second operator) can't share one authoritative
  mission. Also recreates the "chat with extra panels" muddle the Mission
  Control design explicitly escapes.
- **A separate human-only control API beside the two MCP tools.** Rejected: a
  privileged mutation path that skips transition/actor/guard validation is
  exactly the backdoor the harness exists to deny; it would make "the human is
  governed too" false and unauditable. Humans go through `praxec.command` like
  everyone else.
- **Embed the chat TUI inside the cockpit (one event loop).** Rejected per the
  design's non-goals — peer surfaces with suspend/handoff keep each app's event
  loop and rendering clean and avoid coupling two terminal lifecycles.
- **Keep the cockpit fixture-only / no live attach.** Rejected as an end state:
  it can't direct a real mission. Acceptable only as the current interim while
  the live client is the next increment.

## References

- Runtime / serve path: `crates/praxec/src/gateway.rs`
- MCP surface (the two tools): `crates/praxec-mcp-server/src/tools.rs`
- Cockpit client seam: `crates/praxec-cockpit/src/gateway.rs`
- Chat handoff: `crates/praxec-tui/src/main.rs`
- Design umbrella: `docs/architecture/mission-control-design.md` (§3.1, §8)
