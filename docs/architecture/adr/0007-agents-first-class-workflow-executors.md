# ADR-0007: Agents as first-class, harness-bound workflow executors

**Status:** Accepted

**Date:** 2026-06-11

## Context

[ADR-0006](0006-execution-sandbox-and-authored-promotion.md) gave us the
confinement + authored-promotion boundary for agentic execution. This ADR settles
the layer above it: **what an agent _is_, how it executes, and how it relates to
workflows, skills, and scripts** — so the agent subsystem is first-class rather
than the aether-coupled, feature-gated artifact it is today
(`crates/praxec-agents/src/runner.rs` spawns the `aether` binary).

Grounding (this is designed against what the runtime already guarantees):

- A workflow snapshot **already carries its skills + scripts** — `_skillsLibrary`
  / `_scriptsLibrary` stamped at `workflow.start` (SPEC §8.2). "A complete
  workflow has all its skills and scripts available" is an existing invariant.
- Workflows **already** declare a top-level `inputSchema`, validate launch input
  against it (`runtime.rs:438`), are discoverable (`DiscoveryKind::Workflow`),
  embedding-searchable (`SemanticDiscoveryIndex`), and launchable (`start`).
- `RepoLockSpace` is **in-memory, single-process** (`Mutex<HashMap>`) — correct
  within one process, not across processes.
- Composition already exists: `kind: workflow` (sub-workflows), `kind: agent`
  (sub-agents), and the CPM `Planner::acquire_cohort` (disjoint parallel
  deliverables).

**Framing principle.** Separate the **durable governed thing** (the program + its
state) from the **ephemeral execution vehicle**. An agent is to a workflow as a
**process is to a program**: keep the vehicle dead-simple and stateless; put all
richness — state, composition, parallelism — in the governed program.

## Decision

1. **Agent definition.** An agent is an **independently-scheduled execution
   context** running **within a praxec harness**, executing **exactly one
   workflow instance** toward its goal, then leaving. "Within the harness" has two
   teeth: **governed** (it acts only through the workflow's legal transitions —
   the §32 surface) **and confined** (the ADR-0006 sandbox, per its trust tier).
   Its capabilities **are** the workflow's hash-pinned skills + scripts — nothing
   arbitrary. ("Agent = model binding" remains, as the agent's *configuration*:
   which model drives it.)

2. **Four first-class primitives, one lifecycle.** **skill** (hash-pinned
   guidance), **script** (hash-pinned executable body), **agent** (the *engine* —
   model binding + harness/sandbox config), **workflow** (the *program* — a
   governed state machine + its skills/scripts + an orchestrator + a goal/input
   schema). All four are declarable, hash-pinned, authored/promoted (the authoring
   track), discoverable (semantic search), and parameterized. Agent = engine;
   workflow = program; skills + scripts = capabilities.

3. **Execution substrate — task by default, confined process only for untrusted
   (TRIZ: separation by trust).** A **governed** agent (drives a workflow via
   hash-pinned skills/scripts; no arbitrary shell) runs as an **in-process,
   rig-driven tokio task** — light (async scheduling, no process spawn/OS context
   switch), rig-native, sharing the gateway's `LockSpace`. An **untrusted**
   exploration agent (runs arbitrary shell, Tier-1 discovery) runs as a **confined
   OS process** — the *only* case that needs a process, because confinement is a
   process boundary: **you cannot sandbox a task** (a task shares the process's
   memory/fds/FS). Unit of execution = task; unit of confinement = process,
   appearing **only** where confinement is required.

4. **Lock single-authority.** The in-memory `LockSpace` is owned by the one
   gateway/harness. In-process agents share it directly; untrusted processes
   **never hold a lock** — the **promotion bridge** (in the harness) acquires the
   *observed*-set lock at apply-time (the ADR-0006 coordinate-at-promotion model).
   So exactly one process ever holds locks → the in-memory `LockSpace` is correct
   **by construction**; no cross-process lock machinery (flock / DB) is introduced.

5. **rig, not aether; de-gate; decouple.** The governed agent loop is a **rig**
   loop (completing the `kind: llm` rig consolidation). The agent executor moves
   into the **default runtime** (no longer feature-gated), and the runner is
   **decoupled from aether** — an agent is *any* task/process speaking the harness
   protocol (the §32 governed surface), not "the aether binary."

6. **Cardinality: 1:1 ephemeral.** An agent executes exactly one workflow
   instance, then leaves. There is **no multiplexing agent**. Multiplicity is
   expressed by:
   - **Parallelism = count** — N concurrent workflows = N agents, mapping onto the
     CPM cohort (disjoint deliverables → disjoint observed-set locks).
   - **"Many workflows" = composition** — a workflow spawns sub-workflows
     (`kind: workflow`) or delegates steps to sub-agents (`kind: agent`); the
     *workflow* is the unit of composition, the agent stays simple.
   - **Persistence/context = governed workflow state** (blackboard / evidence), not
     an agent's RAM — so it's durable + auditable.
   - **Warmth (model context / cache / connections) = resource pooling** at the rig
     layer, orthogonal to agent lifecycle.

7. **Workflow orchestrator + goal (the new build delta — everything else
   reuses).** A workflow declares `orchestrator: <agentRef | modelRef>` (a bare
   model is the degenerate agent; a gateway default applies). Launch carries a
   `goal` (the orchestrator's NL objective) + structured `input` validated by the
   **existing** top-level `inputSchema`. Discovery gains `DiscoveryKind::Agent`;
   the workflow's discovery item surfaces its **goal/input schema + orchestrator +
   the existing `start` launch link**, so a caller — human *or another agent* —
   can discover and launch a goal-directed workflow with the right parameters,
   exactly as it describes/executes a skill/script.

## Consequences

- **Positive.** Agents are simple (ephemeral, 1:1) and fully governed; parallelism,
  composition, and persistence are preserved in layers that already exist; the
  in-memory lock stays correct with no new machinery; rig consolidation is
  completed for agents; only the genuinely dangerous (untrusted) case pays for a
  process. Launch / discover / parameterize mostly **reuse** `inputSchema`,
  semantic search, and the `start` affordance.
- **Costs.** De-gate + decouple the runner from aether; build the in-process rig
  agent loop; add the workflow `orchestrator` binding, the `goal` surfacing, and
  `DiscoveryKind::Agent`; then the untrusted-tier confined-process path + the
  promotion bridge (ADR-0006).
- **Sequencing.** Governed **in-process rig agent** first (the common case — no
  sandbox needed); then the **untrusted-exploration** confined-process tier +
  promotion bridge.

## Alternatives considered

- **Long-lived multiplexing agent (1:N).** Rejected — pulls governed state into an
  ungoverned process and adds lifecycle complexity; its value (persistence,
  warmth) relocates cleanly to governed workflow state + resource pooling.
- **Agent as a separate process _always_** (a naive ADR-0006 reading). Rejected —
  "you can't sandbox a task" is true, but *most* agents are governed and need no
  sandbox; always-process is heavy and breaks the in-memory lock.
- **Conflate agent and workflow into one thing.** Rejected — loses the
  ephemeral-vehicle / durable-program separation that keeps state governed and
  auditable.
- **Cross-process locks (flock / DB) for multi-process agents.** Rejected —
  unnecessary under single-authority; adds machinery for a case that doesn't arise
  (only the gateway ever holds locks).

## References

- Agent runner (aether-coupled, to decouple): `crates/praxec-agents/src/runner.rs`
- Snapshot skills/scripts invariant (§8.2): `config.rs` `stamp_skills_library` /
  `stamp_scripts_library` (`_skillsLibrary` / `_scriptsLibrary`)
- Workflow `inputSchema` validation at start: `runtime.rs:438`
- In-memory single-process lock: `repo_locks.rs` `RepoLockSpace`
- Discovery + semantic search: `discovery/discovery.rs` (`DiscoveryKind`),
  `SemanticDiscoveryIndex`
- Composition: `kind: workflow` (`WorkflowExecutor`), `kind: agent`, CPM
  `Planner::acquire_cohort`
- Relates to: [ADR-0001](0001-headless-runtime-surfaces-attach.md) (governed
  surfaces — human and agent are the same mechanism),
  [ADR-0005](0005-conversational-cockpit.md) (the out-of-band conductor),
  [ADR-0006](0006-execution-sandbox-and-authored-promotion.md) (confinement +
  promotion).
