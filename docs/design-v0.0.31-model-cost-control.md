# Design — v0.0.31: Model cost & control

Status: **DRAFT for review** · Owner: praxec · Supersedes: the ad-hoc
campaign-scoped `models.yaml` reorders.

## Why

Two production incidents motivate this release:

1. **Uncontrolled frontier spend.** Auto-drive and a QA campaign ran GPT/Sonnet
   (frontier, $15–$30/M output) and burned ~$120 with no gate. The gateway will
   happily lead an agent chain with a frontier model when adequate commodity
   models exist. Nothing warns, nothing blocks.
2. **A model demoted on a bad measurement.** The live `models.yaml` demoted
   reasoning models (e.g. `z-ai/glm-5.2`) to escalation-only because they
   "stalled." A code + audit-log review (see §4) shows the evidence does **not**
   support that: the models were *actively streaming* and got cut by a
   **total-time budget**, then mislabeled as a stall. We nearly made a permanent
   product decision on a configuration artifact.

Both trace to the same root: **the runtime conflates three orthogonal
concerns — liveness, convergence, and cost — into one vague "timeout/stall"
verdict, and it treats a spend ceiling as a model-health failure.**

This release fixes the defaults, adds a real cost gate, and re-architects the
kill machinery so a spend decision can never again masquerade as a model failure.

## Scope (four workstreams)

| WS | Deliverable | Gated on |
|----|-------------|----------|
| **WS1** | Diversified commodity-first default `models.yaml` + setup writes it | WS4 for the *reasoning-lead* slot only |
| **WS2** | `$5/M` cost cap: data-driven classification, warn (doctor/check/setup) + block (park-for-human-approval at runtime) | — |
| **WS3** | Kill-switch redesign: one progress monitor, three **typed** outcomes | shares the park-for-approval seam with WS2 |
| **WS4** | Reasoning-model test B: does a reasoning model earn the lead? | — (informs WS1) |

WS1's commodity defaults and WS2's gate ship **regardless** of WS4. WS4 decides
*only* whether reasoning models lead the `reasoning`/`default` chains or sit as
escalation rungs.

---

## WS1 — Diversified commodity-first defaults

**Principle:** specialized models for specialized activities; commodity leads,
frontier is an approval-gated ceiling only. Not everything-to-DeepSeek.

Derived from `crates/praxec-core/data/model_catalog.json` (22 models, captured
2026-06-23; Artificial-Analysis-style `scores{agentic,coding,prose,reasoning}` +
`input/output_usd_per_million` + `speed_tps` + `reasoning_levels`). The `$5/M`
output line splits 13 commodity models from the frontier tier.

Best-in-class commodity per activity:

| Activity | Lead (specialist) | Score | Cost | Escalate → | Ceiling (🔒 approval) |
|---|---|---|---|---|---|
| coding | `qwen/qwen3-coder` | code 64 | $1.80/M | `glm-5.2` | `sonnet-4-6` |
| reasoning¹ | `deepseek/deepseek-v4-pro` | — | $0.87/M | `deepseek-r1-0528` (reason 60) → `glm-5.2` | `sonnet-4-6` |
| agentic | `moonshotai/kimi-k2.6` | agent 58 | $2.50/M | `minimax-m3` → `glm-5.2` | `opus-4-8` |
| review² | `z-ai/glm-5.2` | intel 56 | $3.00/M | — | `sonnet-4-6` → `opus-4-8` |
| prose | `z-ai/glm-4.7` | prose 52 | $1.75/M | `glm-5.2` | *(none)* |
| default | `deepseek/deepseek-v4-pro` | — | $0.87/M | `glm-5.2` | `sonnet-4-6` |

🔒 = crosses `$5/M` → parks on human approval (WS2). Work is spread across **GLM,
DeepSeek, Kimi, Qwen, MiniMax**.

¹² Two judgment calls, **pending WS4**:
- **¹ reasoning/default lead with a *non*-reasoning model (`deepseek-v4-pro`).**
  Provisional, based on the prior (flawed) stall evidence. WS4 decides whether a
  reasoning model (`deepseek-r1`/`glm-5.2`) takes the lead instead.
- **² review leads with `glm-5.2`** (senior evaluator, producer≠evaluator), with
  frontier as the approval-gated adjudication rung — where the QA campaign wanted
  Sonnet, but now as a human decision, not an always-on default.

**Delivery:** ship a template `models.yaml` (repo + embedded), and have the packs
`setup.sh` write it. Setup runs the WS2 warning so a customer sees the cost
posture up front.

---

## WS2 — The `$5/M` cost cap

**Threshold:** a model is "frontier/expensive" when its catalog
`output_usd_per_million ≥ CAP`. `CAP` defaults to **$5.00/M**, data-driven from
`model_catalog`, overridable via config (`gateway.cost.frontier_cap_usd_per_m`)
and env. No hardcoded model lists — classification reads the catalog, same source
of truth as pricing and the suggestor.

**Two enforcement points:**

1. **Warn (static, loud):** `doctor` / `check` / `setup` emit
   `warn[FRONTIER_LEAD]` when any affinity chain or `auto_drive` *leads* with an
   over-cap model, naming the commodity alternative. Mirrors the
   `EPHEMERAL_STORAGE` warning shipped in v0.0.30.
2. **Block (runtime governance):** when resolution/escalation would actually
   *use* an over-cap model, the runtime **parks on a human-approval gate** — the
   existing approvals/HITL park-resume machinery — rather than auto-spending. A
   human (not the LLM) authorizes the expensive call. Below the cap runs free.

This is the single most important guarantee: **an over-cap model is a human
decision by construction.** "$120 uncontrolled" becomes structurally impossible,
not merely visible.

**Open decision:** threshold on `output` price (recommended — output dominates
realized cost) vs a blended input+output estimate.

---

## WS3 — Kill-switch redesign (progress monitor)

### The defect

Three switches, scattered across three layers, all collapsing into one vague
verdict:

- **stall** — `drain_turn` dead-air window (`DEFAULT_STALL_SECONDS = 120`),
  `crates/praxec-agents/src/rig_runner.rs`. *(Correct: true dead-air, resets on
  any stream event including `StreamEvent::Reasoning`.)*
- **total wall** — `DEFAULT_MAX_SECONDS = 600` / `step_budget = 900`,
  `crates/praxec-agents/src/executor.rs`. **Progress-blind; a cost ceiling used
  as a health verdict.**
- **turn heuristic** — `stalled_no_progress = final_answer.is_none() &&
  tool_calls.is_empty()`, per-turn, `rig_runner.rs:774`.

They surface indistinguishably as `Timeout` / `AGENT_NO_RESULT` / "stall." A
budget-cap of an actively-streaming model reads identically to a dead model —
which is exactly how `glm-5.2` got demoted (§4).

### The design

The three *concerns* are genuinely orthogonal and cannot be merged without losing
signal. The fix is not "fewer switches" — it is **one monitor, one currency
(forward-progress events), three distinct typed outcomes**:

| Concern | Signal | Outcome | Routing |
|---|---|---|---|
| Liveness | dead-air ≥ window (no event of any kind) | `STALLED` | escalate to next model — the *only* "model failed" |
| Convergence | K consecutive turns with **no decision** (no tool-call / final_answer / state change) — a **count, not a timer** | `NOT_CONVERGING` | force-final, then human |
| Cost | cumulative $ or time ≥ ceiling | `BUDGET_EXCEEDED` | **park for human approval (= WS2 gate)** + **preserve partial work** |

Principles:

1. **Progress is the health currency, not wall-clock.** Any stream event
   (thinking/text/tool-call/usage) is progress. A slow-but-streaming model is
   *alive* by definition and is never a "stall."
2. **Budget is policy, never a health verdict.** On the ceiling it parks for a
   human and **returns partial work** instead of discarding it as `NoResult`.
   "We chose to stop spending" ≠ "the model broke."
3. **Distinct outcomes, no collapse.** The operator always sees which fired.

**Payoff:** `glm-5.2` would surface as `BUDGET_EXCEEDED (still streaming, 526s,
effort=high)` — obviously a budget/effort decision, not a stall. The
misdiagnosis becomes impossible by construction.

**Scoping:** (a) *minimum* — make outcomes typed/distinct and stop discarding
partial work on budget (kills the misdiagnosis, unblocks WS4's honest
measurement); (b) *full* — unify the scattered timers into the single monitor.
`BUDGET_EXCEEDED → park-for-approval` is the **same seam** as WS2.

---

## WS4 — Does a reasoning model earn the lead? (test)

### A — Retrospective (done; evidence, not assumption)

Analysis of `~/.local/share/praxec/audit-logs*` (`agent.model_attempt`,
`agent.heartbeat.seconds_since_last_output`) **contradicts** "reasoning models
uniquely stall":

- Failures are `NetworkTimeout` / `Capability` / `AGENT_NO_RESULT` (contract
  miss), spread across **every** model — including the non-reasoning "safe" lead
  `deepseek-v4-pro` (8 NetworkTimeout, its own `AGENT_NO_RESULT`, runs of
  763s/789s past the 600s wall).
- `glm-5.2`'s failed runs lasted **165–526s**, killed by large timeouts
  (337s/517s) — **not** the 120s stall window. Across 543 heartbeats its **max
  silence was 109s — it never crossed the 120s stall line.** It was *streaming
  the whole time* and cut by the total budget: a **premature cap of a working
  model**, not a stall.
- All of this predates `reasoning_effort` being bounded to `"low"` — so those
  long runs were almost certainly *unbounded* reasoning the effort-cap now
  shortens.

Caveats: small samples (`glm-5.2` n=8); logs may not be the exact 07-20 campaign;
`glm-5.2`'s 0/8 completion is a real flag — "was streaming" ≠ "would finish." The
effort-bounding fix is **untested**. Hence B.

### B — Controlled live experiment

- **Harness:** each model through the *real* agent loop on ~5 representative
  auto-drive tasks (a real pack `cap.*` step, deterministic inputs), N=3.
- **Models:** `glm-5.2`, `deepseek-r1-0528`, `qwen3-235b-thinking`, `minimax-m3`
  (reasoning) + `deepseek-v4-pro` (non-reasoning control).
- **Conditions:** `reasoning_effort ∈ {low, high}` × `budget ∈ {600s, 1800s}`.
- **Per-run instrumentation** (unambiguous kill classification — enabled by WS3's
  typed outcomes): which timer fired (`STALLED` vs `BUDGET_EXCEEDED`);
  `seconds_since_last_output` at termination; count of `StreamEvent::Reasoning`
  (0 ⇒ provider doesn't stream thinking ⇒ that model *is* stall-prone by
  construction); time-to-first-token; total duration; conforming `final_answer`?
- **Effectiveness:** same tasks, reasoning-lead vs `deepseek-v4-pro`-lead, scored
  on completion rate + adjudicated quality + total $.

**Decision rule:** a reasoning model leads its chain **iff** (under bounded
effort + adequate budget) it completes reliably **and** beats the non-reasoning
lead on quality-per-dollar. Otherwise it is an escalation rung. WS4 spends only a
few dollars (all commodity); capped and reported.

---

## Sequencing & dependencies

1. **WS3-minimum** first — typed outcomes + preserve-partial-work. Unblocks WS4's
   honest measurement and is a prerequisite for the WS2 `BUDGET_EXCEEDED` park.
2. **WS2** — cost classification + warn + park (shares WS3's park seam).
3. **WS4** — run B with the WS3 instrumentation; produce the reasoning-lead
   verdict.
4. **WS1** — ship the default `models.yaml` with the WS4 verdict folded into the
   reasoning/default lead slot; wire `setup.sh`.
5. **WS3-full** — unify the timers (can trail into a later release if needed).

## Open decisions (need sign-off before code)

1. **WS2 threshold basis:** `output_usd_per_million` (recommended) vs blended.
2. **WS4 task:** a real pack `cap.*` step vs a synthetic fixed task.
3. **WS1 ¹/²:** the reasoning-lead and review-lead calls (¹ resolved by WS4).
4. **Live-config revert:** is the 07-20 QA campaign done? If yes, restore the
   live `~/.config/praxec/models.yaml` `reasoning`/`review` chains to
   commodity-lead as part of WS1. If not, leave the live config untouched and
   ship only the shipped default + guard.

## Acceptance

- Commodity-lead defaults ship; `doctor`/`check`/`setup` warn on a frontier lead;
  a run that resolves an over-cap model **parks for human approval** and never
  auto-spends (proof: an over-cap chain parks; an under-cap chain runs clean).
- `STALLED` / `NOT_CONVERGING` / `BUDGET_EXCEEDED` are distinct in audit output;
  a budget-capped **streaming** run reports `BUDGET_EXCEEDED`, never a stall, and
  its partial work survives.
- WS4 B produces a signed-off reasoning-lead verdict with the per-run evidence.
- No known frontier-lead defaults at release (the v0.0.30 "no known defects at
  tag" bar).
