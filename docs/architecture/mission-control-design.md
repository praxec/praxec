# Praxec Mission Control — Design

**Date:** 2026-06-08
**Status:** Design (approved in brainstorm; pending written review)
**Scope:** Reposition Praxec's UX from "a coding agent" to a **mission-control control plane**, grounded in the existing functional spine. Designed big; built in small increments (outside-in, UX-first).

---

## 1. Thesis

The market does not need another coding agent. It needs a way to **govern** them.

**Positioning — the inversion.** Every other tool is *generation-first*: generate, then maybe govern. Praxec's wedge is flipping that order — **control-first / harness-first**. The harness comes first; generation happens inside it. That *inversion of the industry approach* — not "we do governance too" — is the primary distinguisher, and it should lead the language of the whole app.

Today's agentic coding **generates without a harness** — an LLM in a loop, hoping. Praxec inverts this: **generation only ever happens *inside* a deterministic execution harness, and recursively** — every dive into a sub-problem generates a fresh sub-plan that can again only execute within the harness. Pluggable governance packs can wrap that same generation in further harnesses (intent, structure, security).

**Unbounded generative planning married to bounded deterministic execution, nested all the way down the SDLC** — that is the thing current tools cannot do, and it is what Praxec exists to make possible.

The user should feel: *"I am not chatting with an AI. I am directing a governed software-delivery system."*

### 1.1 The harness framing (the unifying concept)

Everything here is a **harness** — a frame that bounds what may happen.

- **Praxec = the deterministic *execution* harness.** It bounds *what is legal to execute next*: states, gates, evidence, locks. The Flow gate **is** this harness.
- **Governance harnesses** plug in: **Intent**, **Structure**, **Security**. They attach through the already-shipped seam — MCP connections + `evidence` guards + skills — not through any change to the guard kernel.

### 1.2 The inversion, concretely

Today's pattern: *LLM first → give it tools → ask it to plan → hope it stays on task → patch with prompts, permissions, retries.* The LLM is the operating system.

Praxec flips it: *Harness first → define mission, workflows, state, locks, and legal actions → expose only the current affordances → let LLMs execute bounded reasoning inside the harness → validate every transition and artifact.* **The LLM is a pluggable, replaceable reasoning engine inside a deterministic runtime — not the controller.**

Four concrete inversions:

| Industry default | Praxec |
|---|---|
| The LLM decides what the process is | The **workflow** defines the process; the LLM helps execute steps inside it |
| The LLM gets a bag of tools | The **runtime exposes legal next actions** from state (the affordance surface the human and model both see) |
| The LLM owns context | The **blackboard, workflow state, and artifacts own context**; the LLM receives only the relevant slice |
| Use the best model for everything | Use the **cheapest sufficient executor per step** — script, small/local model, coding model, or frontier model. This is exactly what `agents.yaml` affinity×tier already encodes. |

The defining question is not *"How do we give an LLM enough tools to build software?"* but ***"What harness must exist so any LLM can safely contribute to software delivery?"***

Because each step is bounded and state-specific, a worker never has to understand the whole mission — which is precisely why older, cheaper, local, or specialized models become useful. The harness turns LLMs into **replaceable workers**.

**Category / taglines:** *"A deterministic harness for nondeterministic intelligence."* · *"Harness first. Models second."* · *"The runtime leads. The models execute."* · *"Put agents on rails before they write code."*
**Precise positioning:** *"Praxec is a harness-first runtime for AI software delivery: workflows, rails, locks, and state come first; LLMs execute inside those constraints."*

---

## 2. Core ontology

Two natures of "stuff," kept strictly distinct because conflating them is what muddles every comparable tool:

| | Nature | Provenance | Reuse | Lives in |
|---|---|---|---|---|
| **Flow** (`flow.*`) | known recipe | authored / curated | reusable | Build |
| **Capability** (`cap.*`) | known sub-recipe | authored / curated | reusable | Build |
| **Skill** | known intelligent step | authored / curated | reusable | Build |
| **Tool** | known atomic callable | authored / curated | reusable | Build |
| **Plan** | **generated strategy** | *figured out for this goal* | one-time *(promotable)* | Run |
| **Flow** | **governed execution** | the plan, running | this instance | Run |

Key identity: **an Flow is a *frozen Plan*; a Plan is a *live, generated Orchestration*.** Same shape (a composition of workflow-tools); they differ only in provenance and reuse. This is why Plan and Flow must be distinct objects, and why there is a **promotion path**: a proven Plan can be crystallized into a reusable Flow, growing the deterministic library out of generative one-offs.

### 2.1 The composition ladder (the reusable library)

One ladder; each level composes the one below. This is the *Build* surface.

```
Tools         deterministic/atomic callables:  planner.critical-path ·
              structureos.analyze_god_file · github.* · scripts · cli/rest
Skills        one governed intelligent step:   llm/agent + bound tools + guidance
Capabilities  reusable sub-workflow (cap.*):    a small state machine of steps
Flows top-level workflow (flow.*):      composes capabilities
```

Plus supporting definitions — **Connections** (endpoints tools live on) and **Agents** (model bindings) — and **Sources** (packs that contribute all of the above).

### 2.2 The recursive marriage: Plan ⇄ Flow, all the way down

A Plan arranges workflow-tools; executing one is a Flow; any state in that Flow that is *novel* opens a **sub-plan** (generate again) → which runs a **sub-Flow** → and so on. Generative and deterministic alternate at each level of decomposition:

```
Recipe exists  → run the workflow      (deterministic)
No recipe      → generate a plan       (generative)
```

Same node, two modes, chosen per-step as you descend. The planner (`planner.critical-path`, a deterministic tool) **arranges** the chosen tools into parallel/sequential structure; the LLM **selects** which tools. Generative + deterministic, cooperating.

### 2.3 The planner is a tool, the critical path is a property of a plan

`planner.critical-path` is *just another Tool* (deterministic, per "build a tool, don't trust prompts"). The **critical path** it computes is the *structure of a Plan* — not a top-level concept. It is surfaced as the **Graph lens** on Flow, not as a separate "Plan panel" that competes with execution.

---

## 3. Two peer surfaces

These are two genuinely different modes of engagement. Forcing one TUI to serve both produces the "chat with extra panels" muddle we are escaping.

- **Mission Control** (NEW — `ratatui`, Praxec-native): the **home**. Directing, configuring, supervising. This is where Praxec's identity lives. **Not a chat.**
- **Coding/chat** (EXISTING — `aether-wisp` ACP TUI): an agent *executing*. Conversational, fast. You **drop into** it for one bounded session, then return.

**Relationship — peer surfaces with a clean handoff (chosen; not embedding).** Mission Control owns the terminal; opening a coding session *suspends* it (drops alt-screen), hands the terminal to wisp for that session, and returns on exit — the way `git` shells out to `$EDITOR`. Each app owns its own event loop; we never embed wisp.

### 3.1 Data layer

Mission Control is a **second client of the same two gateway tools the model uses** — `praxec.query` (read: home/search/describe/get/explain by arg-shape) and `praxec.command` (write: start/submit). The human cockpit and the LLM see the **same legal next actions**, which is what makes "the LLM is bounded" *visible* rather than asserted. Implemented in `crates/praxec-mcp-server/src/tools.rs`; links/affordances built in `crates/praxec-core/src/runtime/runtime_links.rs`.

---

## 4. The two modes

### 4.1 BUILD — configure your Mission Control

Nav: **Sources · Flows · Capabilities · Skills · Tools · Connections · Agents**

Grounded directly in the real pack taxonomy (`praxec.repo.yaml` `schema: praxec.repo/v1`, `namespace`, `layout:`; gateway `repos: [{path}]`; namespace-prefixed ids like `cognitive/flow.add-feature`). Authoring is backed by existing executors: import via `ingest`, validate via `structural_analysis`, test via `dry_run`, publish via `registry`.

```
╭─ Praxec ────────────────────────────────[ ▸Build │ Run ]── mission control ─╮
│  Sources   Flows   Capabilities   Skills   Tools   Connections   Agents │
╞════════════════════════════════════════════════════════════════════════════════╡
│ SOURCES · packs wired into this workspace                       [ + add pack ]  │
│ ┌────────────────────────────────────────────────────────────────────────────┐ │
│ │ ● cognitive    cognitive-architectures   v0.6.0   git ✓ pinned @a3f1    ▸   │ │
│ │     praxec.repo/v1 · 4 flows · 9 capabilities · 19 skills · 5 conn │ │
│ │ ● governance   governance-praxec-pack  v0.6.0   local (path)          ▸   │ │
│ │     Intent · Structure · Security harnesses   (mcp connections + skills)     │ │
│ │ ○ local        .praxec/                 —        workspace             ▸   │ │
│ └────────────────────────────────────────────────────────────────────────────┘ │
│ LIBRARY · composing ▸  Tools → Skills → Capabilities → Flows             │
│ ┌ Flows (flow.*) ─────┐ ┌ Capabilities (cap.*) ──┐ ┌ Skills ───────────┐│
│ │ cognitive/flow.add-feature   │ │ cap.plan.vet           │ │ refactor.scope-b… ││
│ │ cognitive/flow.safe-refactor │ │ cap.refactor.plan      │ │ review.code.adv…  ││
│ └──────────────────────────────┘ └────────────────────────┘ └───────────────────┘│
│ ┌ Tools ──────────────────────────────────┐ ┌ Connections ─┐ ┌ Agents ──────────┐│
│ │ planner.critical-path   built-in · det.  │ │ structureos  │ │ coding-frontier  ││
│ │ structureos.analyze_god_file  mcp        │ │ github-mcp   │ │ reasoning        ││
│ └──────────────────────────────────────────┘ └──────────────┘ └──────────────────┘│
╰─ allumata-saas · main · 2 packs ─────────────────────────────────── ?:help q:quit╯
```

#### Skill detail (the centerpiece — skill as executable primitive)

A skill is promoted from passive guidance to a **named, reusable capability you *execute***: executing it launches a model (LLM by default, agent opt-in) **bound to a specific toolset and a calling contract.** This wires the dormant `SkillExecutor` and enriches the schema (`target` + structured `tools`).

```
╭─ Praxec · Build · Skills ─────────────────────────────────────────────────────╮
│ ‹ skills        refactor.scope-bounded             cognitive  ·  verb: refactor  │
╞════════════════════════════════════════════════════════════════════════════════╡
│ RUNS ON   llm · affinity: reasoning           (agent opt-in)     lifecycle: stable│
│ GUIDANCE  hash-pinned · sha a3f1…                                       [ view ]  │
│ TOOLS     this skill may call                                       [ + add tool ]│
│ ┌──────────────────────────────────────────────────────────────────────────────┐│
│ │ structureos · analyze_god_file              mcp · pack: governance             ││
│ │   static    threshold: 400                                                     ││
│ │   generate  target_path  → "file the current state is editing"                 ││
│ │ constrained-edit · apply_patch              cli · pack: cognitive              ││
│ │   generate  ops[]        → "the ≤8 allowed edit operations"                    ││
│ └──────────────────────────────────────────────────────────────────────────────┘│
│ USED BY   cognitive/flow.safe-refactor (state: editing) · cap.refactor.plan       │
╰─ › test this skill (dry_run)…                                  legal actions: 3 ⏎ ╯
```

Enriched schema (evolves today's `{verb, lifecycle, source, body}`):

```yaml
skills:
  refactor.scope-bounded:
    verb: refactor
    target: { affinity: reasoning }      # NEW — substrate; llm-default, agent opt-in
    body: | ...                          # existing hash-pinned guidance
    tools:                               # NEW — structured (was prose-only)
      - ref: { connection: structureos, tool: analyze_god_file }
        args:    { threshold: 400 }      # static — pinned by the skill
        generate:                        # model-filled; schema INHERITED from the tool (no copy → no drift)
          target_path: { describe: "file the current state is editing" }
```

DRY guardrail: the skill **references** the tool and overlays only static-vs-generative split + per-arg guidance. It never copies the tool's own JSON schema (poka-yoke against drift).

### 4.2 RUN — supervise the mission

Nav: **Mission · Plan · Flow · Agents · Blackboard · Trace · Artifacts**

Each word means exactly one thing: Mission = the goal; **Plan = the generated bespoke strategy**; Flow = governed execution; the reusable recipes live in Build.

Run is a **navigable plan/flow stack**, not flat areas. The recursion *is* the navigation:

```
Mission: Add OAuth login
 └ Plan ▸ flow.add-feature ▸ state: backend  ↳ needs planning
     └ Sub-plan ▸ cap.implement-backend ▸ state: editing
         └ skill refactor.scope-bounded (llm)
```

A breadcrumb pushes/pops frames as you dive. Plan and Flow are *lenses on the current frame*.

- **Plan** is where you **watch and steer the generation** of the strategy (what it's selecting, why, what runs parallel) and **approve it** before/as it executes — the supervision moment, "what needs my decision" at the strategy level. Degenerate case: when a known flow already fits the goal, the plan is simply *"run `flow.safe-refactor`"* — the generative machinery only engages for novel/composite goals (so it is never heavyweight for trivial work).
- **Flow** is the governed execution, with two lenses: **Gate** (current state, harnesses, legal actions) and **Graph** (critical path / what's parallel-ready now).

#### Run dashboard (the five questions at a glance)

```
╭─ Praxec ────────────────────────────────[ Build │ ▸Run ]── mission control ─╮
│  Mission   Plan   Flow   Agents   Blackboard   Trace   Artifacts               │
╞════════════════════════════════════════════════════════════════════════════════╡
│ MISSION  Safe-refactor AuthService                                  ◷ 8m        │
│ cognitive/flow.safe-refactor · 5 states · running · 1 needs you                  │
│ ┌ PLAN · critical path ────────┐  ┌ FLOW · current gate ───────────────────────┐│
│ │ ✓ triage                      │  │ state     editing                          ││
│ │ ✓ plan (cap.refactor.plan)    │  │ running   skill refactor.scope-bounded·llm ││
│ │ ▶ editing       ~reasoning     │  │ HARNESSES                                  ││
│ │ ⏸ verify   ⟂ editing           │  │   execution (praxec)  legal: 2 · locks ✓ ││
│ │ ⏸ review                       │  │   intent    (governance) ✓                 ││
│ │                               │  │   structure (governance) ⚠ god-file risk    ││
│ │                               │  │   security  (governance) ✓                 ││
│ └───────────────────────────────┘  └────────────────────────────────────────────┘│
│ ┌ NEEDS YOU ───────────────────────────────────────────────────────────────────┐│
│ │ ⚠ structure harness — StructureOS flagged AuthService as god-file risk         ││
│ │   [ open coding session ]   [ re-run structure check ]   [ override + note ]    ││
│ └─────────────────────────────────────────────────────────────────────────────────┘│
│ ┌ BLACKBOARD ────────────────┐  ┌ TRACE ──────────────────────────────────────┐ │
│ │ ◆ plan  extract TokenStore  │  │ 12:04 skill refactor.scope-bounded   llm    │ │
│ │ 🔒 editing owns src/auth/**  │  │ 12:04 mcp  structureos.analyze_god_file  ⚠  │ │
│ └─────────────────────────────┘  └─────────────────────────────────────────────┘ │
│ › Direct the mission…                                         legal actions: 2 ⏎ │
╰─ allumata-saas · main · flow.safe-refactor ──────────────────────── ?:help q:quit╯
```

The bottom is a **command bar** ("Direct the mission…"), not a chat box, showing a live **legal-actions count** so the control protocol is always visible. The Flow gate's governance section is the unified **Harnesses** panel: Praxec's own execution harness plus any pluggable governance harnesses (intent/structure/security). `+ add harness` is the extension seam, and it reads as exactly what it is.

---

## 5. Pluggable governance harnesses (the extension seam)

Governance plugs in **as MCP**, as first-class integrations — *not* as new guard kinds (the guard kernel is closed by design: 10 kinds, compiler-enforced, no plugin seam — `crates/praxec-core/src/guards.rs`). The real, already-shipped seam:

1. **Connections** — structure/intent/security analysers are MCP servers wired as `kind: mcp` connections (e.g. `connections/structureos.yaml`, which already exists in the cognitive-architectures pack).
2. **Skills** — Governance ships skills that bind those tools with calling contracts.
3. **Evidence guards** — the existing `evidence` guard gates transitions on what those tools produced (quorum, min-confidence, digest).

A Governance "harness" is therefore: a shipped MCP tool + a shipped skill that binds it + an `evidence` guard that gates on its output. Nothing in the kernel changes; the harnesses are composable, declarative, and arrive through a pack like any other.

---

## 6. Grounding: what exists vs. what is new

**Exists today (the spine — verified in code):**
- Primitives / executor kinds: `cli, mcp, rest, script, noop, human, workflow, parallel, pipeline, llm, agent` (+ authoring `dry_run, structural_analysis, registry, ingest`) — `crates/praxec-executors`, `crates/praxec-llm-executor`, `crates/praxec-agents`.
- Declarative workflow YAML: states → transitions → executor + guards + `inputSchema` + `output` mappings — `examples/swe-agent.yaml`.
- Sub-workflows + composition: `workflow` (`use:` I/O scoping, depth cap 10), `parallel`, `pipeline`.
- Gateway = 2 MCP tools (`praxec.query` / `praxec.command`); links/affordances filtered by guards — `crates/praxec-mcp-server`, `runtime_links.rs`.
- Guards: closed 10-kind set incl. `evidence` — `crates/praxec-core/src/guards.rs`.
- Agents: `agents.yaml` bindings (closed Affinity×Tier) — `crates/praxec-core/src/model_resolver`.
- Pack/source model: `praxec.repo.yaml` manifest + `repos:` config + namespacing.
- CPM planner: the standalone `cpm-planner` MCP server (separate repo).
- Chat TUI: `aether-wisp` via `praxec-agent acp` handoff (`crates/praxec-tui/src/main.rs`).

**New work (the gaps this design implies):**
1. **Mission Control ratatui app** + the two-mode shell + nav + the gateway-client data layer.
2. **Plan as a first-class object** — the generative planning layer that *selects* library items, calls `planner.critical-path` to *arrange* them, is steerable/approvable, and is **promotable** to an Flow.
3. **Recursive Plan⇄Flow navigation** (the plan/flow stack) — a step that opens a sub-plan instead of a fixed sub-workflow.
4. **Skill-as-executable-primitive** — wire `SkillExecutor`; enrich skill schema with `target` + structured `tools`.
5. **Harness verdict layer** — unify execution + Governance verdicts into the Flow gate's "Harnesses" panel.
6. **Git-native source fetch/pin/update** (today: clone then reference by `path:`).
7. **Clean handoff** — suspend Mission Control → wisp → return.
8. **Language/positioning pass** — strip "coding agent" framing everywhere (CLI `about`/`long_about`, module docs, "tools"→"affordances/legal actions"/"harnesses", taglines, README hero). *Independent, ships first.*

---

## 7. Decomposition (design big, build small)

Sequential specs; this document is the umbrella. Each increment is independently shippable and fits the known whole.

- **P0 — Language & positioning pass.** Apply the harness/control-plane frame to all user-facing strings. Independent, low-risk, lands first; mostly execution (frame already settled). Checklist, not a heavyweight spec.
- **P1 — Mission Control core.** The ratatui app spine: two-mode shell, nav, the `praxec.query`/`command` data layer, the Run dashboard (read-only over a running mission). Load-bearing.
- **P2 — Build surface.** The library over the pack taxonomy (Sources/Flows/Capabilities/Skills/Tools/Connections/Agents); read + author (`ingest`/`structural_analysis`/`dry_run`).
- **P3 — Skill-as-executable-primitive.** Wire `SkillExecutor`; enriched schema; the Skill detail screen.
- **P4 — Plan + recursive descent.** The generative Plan object, the plan/flow stack navigation, promotion-to-Flow.
- **P5 — Handoff.** Suspend ↔ wisp ↔ return.
- **P6 — Harness panel + Governance seam polish + git-source fetch/pin.**

Order rationale: P0 ships the repositioning immediately; P1 is the spine everything hangs off; P2–P6 layer capability without re-architecting. P4 (generative planning) is the highest-novelty increment and deliberately sequenced after the deterministic surfaces are real.

---

## 8. Non-goals / YAGNI

- **Not** embedding the chat TUI inside Mission Control (peer surfaces, not one event loop).
- **Not** adding a guard-plugin registry (the kernel stays closed; Governance arrives via MCP + evidence).
- **Not** building the governance harnesses themselves here — Praxec ships the seam; the harnesses are separate products.
- **Not** making every mission go through generative planning — known recipes run directly.

---

## 9. Open questions

1. **Plan persistence** — is a generated Plan stored as a first-class instance record (for audit/promotion), or ephemeral until promoted? (Leaning: recorded per-mission, promotable on demand.)
2. **Promotion mechanics** — what exactly does "promote Plan → Flow" capture (states + bindings + the arrangement), and how is it namespaced into a pack vs. `.praxec/` local?
3. **ratatui app boundary** — new crate (`praxec-mission` / `-cockpit`) vs. extending `praxec-tui`? (Leaning: new crate; the chat TUI and the cockpit are separate surfaces.)
4. **Default landing** — bare `praxec` lands in Build (no mission) or a mission picker? Onboarding starts with "what are you trying to build/change?", not config.
5. **Git-source trust** — pinning + update/review UX for team/remote packs (drift, like the agents.yaml provider-drift concern).
