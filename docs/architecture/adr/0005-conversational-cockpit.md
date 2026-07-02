# ADR-0005: The conversational cockpit — chat spine, operations-as-MCP, dual-driver

**Status:** Accepted

**Date:** 2026-06-09

## Context

[ADR-0003](0003-mission-control-view-model.md) fixed the UI as a curated
view-state machine; [ADR-0004](0004-mission-control-map-paradigm.md) made
traversal a semantic map. This ADR settles the **interaction architecture** that
those sit inside: Mission Control is **chat-conducted** — a conversation is the
spine, and the map (and config, and detail) are widgets it drives.

The design question: how does a chat *drive* a deterministic harness without
becoming illegible (a free-form prompt that does hidden things), and how does
the human keep full keyboard control alongside it?

Framing principle (Norman's two gulfs): every interface must close the **gulf of
execution** (express intent) and the **gulf of evaluation** (perceive truth).
Chat-only closes execution but buries evaluation in a scrolling transcript;
map/keyboard-only closes evaluation but leaves execution effortful. The
resolution is **dual-coding** (Paivio): a verbal channel (chat) for intent and a
visual/spatial channel (the persistent map/widgets) for state — each carrying
what it is best at.

## Decision

**The cockpit is a conversational REPL over the fleet.** The chat is the input
spine; the stage (map / config / detail widgets) is the live output it
manipulates and the at-a-glance state surface.

1. **Two-region, chat-centric layout.** A persistent **stage** (the active
   widget — the map at Fleet/Mission, or a config widget, or a detail) plus a
   persistent **chat** (a scrollable thread with an Oatmeal-style **pinned
   input**). The chat conducts the stage; the stage reflects state. The map
   (ADR-0004) is a widget *on* the stage.

2. **One operation surface, two drivers.** The cockpit's operations are defined
   **once** (`zoom_into`, `zoom_out`, `pan`, `open`, `query`, `act_on_ask`,
   `set_view`, config ops, …) and have two front-ends: the **keyboard** (the
   human) and **MCP** (the LLM). The cockpit is **its own MCP server**; the chat
   LLM is just an MCP client of it. Consequences this is chosen for:
   - **Legible agency** (Amershi et al. observability): the LLM can do nothing
     you can't see and do yourself — it calls the same named, **undoable**
     operation you'd press. No hidden magic → trust.
   - **Mixed-initiative** (Horvitz): human and LLM are co-pilots on *one* control
     surface, with clean handoff and clear attribution of who acted.
   - **Externalizability:** because the ops are MCP, an external agent could
     drive the cockpit too.

3. **Navigation-local / action-governed split.** *Navigation* ops (zoom / pan /
   open / query — looking around) are **local view-state**, ungoverned.
   *Action* ops (act on a HITL ask, start/advance a mission) route through the
   runtime's `praxec.command` (governed, version-checked, audited). So the
   chat LLM mutating a real mission is bounded **identically to any agent**
   ([ADR-0001](0001-headless-runtime-surfaces-attach.md): human and LLM are the
   same governed mechanism).

4. **The chat LLM is the out-of-band Mission Control LLM**
   ([ADR-0002](0002-fleet-runtime-multiplexed-mission-context.md)'s distinguished
   principal), running on the in-tree **`aether-llm`** crate — *separate* from
   the execution LLMs that run inside workflows.

5. **Setup gate (first run) — minimize time-to-first-value.** Before the chat is
   usable, a focused widget assigns **vendor (SDK) → model → key**, using
   *pickers* (recognition over recall — choose from aether-llm's vendors and
   their models; don't type `provider:model`). It **detects existing config**
   (`models.yaml` / provider keys from the CLI) and pre-fills or skips entirely
   when a usable LLM already exists. The gate is the fewest steps to a working
   chat, not a wall (progressive disclosure).

6. **Config tab (Providers / Models) — the explicit in-system surface.** A TUI
   surface that visualizes and edits what the CLI does today (`models.yaml`,
   `set-provider-keys` / `providers.env`): which vendors/models are configured,
   which keys present. Once a minimal LLM is live, the **chat also facilitates
   config** ("use Opus", "add my Anthropic key") — config ops on the same shared
   surface (dual path, like the map's dual navigation).

## Consequences

- **Positive.** Both gulfs closed (say-it-or-press-it; see-it-always). Trust via
  legible agency and one observable op set. Governance is consistent — the chat
  LLM is a governed principal, nav stays local. Familiar REPL/notebook mental
  model → low onboarding. Keyboard parity preserved (mixed-initiative).
- **Costs.** Real work: extract the operation surface (one API behind keys +
  MCP); the chat-LLM loop (aether-llm + tool-calling); the in-process MCP
  server; the setup gate + config tab; the two-region layout restructure (the
  current map becomes a stage widget).
- **Sequencing (outside-in).** The shell — operation surface + chat-centric
  layout + a deterministic command driver — can land *before* the real LLM, to
  de-risk the layout and ops; then `aether-llm` + the setup gate + MCP replace
  the deterministic driver.

## Alternatives considered

- **Chat-only (transcript is the whole UI).** Rejected — buries the gulf of
  evaluation; a scrolling transcript is strictly worse at supervising parallel
  governed work than a persistent spatial map.
- **Map/keyboard-only (no chat).** Rejected — leaves the gulf of execution
  effortful; this is what Increment 1 shipped and what prompted this ADR.
- **LLM with privileged operations the user can't perform.** Rejected — illegible
  agency; the human must be able to see, do, and undo everything the LLM does.
- **A separate human-only or LLM-only control path.** Rejected — breaks the
  single-op-surface parity and the governance story (ADR-0001).

## Amendment (2026-06-10) — the cockpit is a self-contained MCP flow

Decision §2 said "the cockpit is its own MCP server; the chat LLM is just an MCP
client of it." Building it surfaced two refinements, accepted here:

1. **rmcp 1.7 has no in-process transport.** A real client↔server in one process
   would mean stdio or a subprocess — heavy machinery for a same-process call. So
   the operation surface is defined **once** as a canonical, transport-agnostic
   dispatch core (`op::op_tools` + `op::op_from_tool_call` in
   `praxec-cockpit`). The **in-process** Mission Control LLM calls that core
   **directly** (no MCP round-trip) — "approach B". The MCP *representation* is a
   separate crate, **`praxec-cockpit-mcp`**, an rmcp `ServerHandler` that
   exposes the same ops over **stdio** so an **external** agent can drive the
   cockpit (the ADR's "externalizability" / mixed-initiative intent, realized for
   external drivers). The cockpit binary does not depend on that crate; the crate
   depends on the cockpit — no cycle. Wiring the external server to *live* cockpit
   state is the fleet-runtime increment ([ADR-0002](0002-fleet-runtime-multiplexed-mission-context.md)).
   The schema is defined once and projected to both faces, so the two never drift.

2. **The cockpit + its MCP representation + its LLM are a self-contained
   flow** — its own package(s), separate from the praxec runtime/system,
   though free to depend on `core` for leaf primitives (provider catalog, keys
   file, `models.yaml`) since the UX is built on that information architecture.
   This is praxec's own pattern applied recursively: a runtime that exposes
   legal actions as MCP and lets a governed LLM drive within them — here the
   "legal actions" are the cockpit's navigation ops.

**Concurrency.** The render loop is synchronous crossterm; a turn is seconds of
network. A tokio runtime runs each turn off-thread; `App::submit_command` records
a `pending_turn` (no IO in `App`), the event loop spawns it, and results return
over a channel drained each tick — ops applied on the UI thread via the same
`op_from_tool_call` dispatch. One tool call per turn (mirrors the §33 executor);
the deterministic `op::parse_command` stays as the offline fallback.

## References

- aether-llm (the chat LLM engine): in-tree dependency, providers
  anthropic/openai/gemini/openrouter/ollama
- Model config reused: `models.yaml` / `model_resolver`, `set-provider-keys`
- Cockpit (UI + state + interaction model): `crates/praxec-cockpit`
- Cockpit MCP face (external-agent seam): `crates/praxec-cockpit-mcp`
- Relates to: [ADR-0001](0001-headless-runtime-surfaces-attach.md) …
  [ADR-0004](0004-mission-control-map-paradigm.md)
- Design umbrella: `docs/architecture/mission-control-design.md`
