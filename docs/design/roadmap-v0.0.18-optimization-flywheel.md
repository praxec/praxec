# Roadmap: v0.0.16 → v0.0.17 → v0.0.18

The releases sequence toward one thesis: **maximize `human:intent × compiled-tool:determinism × model:generation`** — humans own intent, deterministic tools own verification/computation/structure, models own generation, and praxec orchestrates a clean, extensible, self-improving composition.

## v0.0.16 — foundation (NOT released separately; ships inside 0.0.17)
The governed loop made real: durable await/resume + HITL origin enforcement (intent), the build-loop ceremony fix so the loop can *finish* what it writes, model chain-walk + per-model cooldown breaker, credential preflight/doctor, self-provision detection, cost+intent telemetry, and **execution-tree observability** (heartbeat + node-linkage + `observe --follow` + a `praxec.query` observe endpoint). The FMECA-hardened lesson: the cross-process "event bus" collapsed to *reusing the existing file `AuditSink`* — no socket, no hub, no `seq`.

## v0.0.17 — self-extension ecosystem (the release)
praxec becomes a platform a capable model can extend *through* praxec:
- **Connection grant gate** (fixes a live supply-chain gap: repo-contributed connections were auto-trusted — now the operator must grant each; this is the `human:intent` trust boundary).
- **Generalized tool descriptor** where `reach ≡ a literal connections entry` and `provision_hint ≡ packs/v2 providers` (install = copy, never transform).
- **Tool-source integration contract** (a source declared by reach + a static field-map + `search`; install reuses `provision::detect` + doctor + the ADR-0013 provider chain).
- **Layerable tool-packs** — a `repos:` layer may carry `{ tool integrations + the workflow suites that maximize them }`; community-contributable, safe by the grant gate.
- **Structured registry** (`praxec/packs` v3 + a crossmatrix topology of tools × capabilities × workflows).
- **`meta/flow.onboard-tool`** (dogfooded) and a **meta-proof pack** (the acceptance test: fable onboards a tool + mints a workflow using it, through praxec).
- **Evidence surfacing** — `praxec.query` annotates workflow hits with the already-computed `{success_rate, mean_cost, runs}` so selection is evidence-based, not blind.

## v0.0.18 — the optimization flywheel (this document's focus: direction, not build)
Make the ecosystem *compounding*: **discovery → application → evidence → improvement → mint-better → repeat.** The flywheel has three mechanisms, deliberately distinguished:

### 1. Semantic search embeddings — *description* embeddings (discovery)
Re-enable + extend praxec's **existing** embedding discovery (currently OFF because the embedder endpoint is flaky and hot-reload falls back to lexical):
- **(a) workflow-description embeddings** — semantic search over flows/caps/skills.
- **(b) tool/mcp/rest-description embeddings** — semantic search over the tool catalog.
- **Hard prerequisite (what killed it before):** a *dependable* embedder — a local model or a fixed endpoint — plus **re-embed-on-reload** (today embeddings are startup-only). Until that lands, lexical + crossmatrix + evidence-annotation (v0.0.17) suffice for a fable-class model, which is why embeddings were correctly cut from v0.0.17.

### 2. Structural fingerprints → structural embeddings (meta-management)
A *different* mechanism from semantic search — for praxec-meta to **compare / cluster / dedup / merge** workflows by their actual graph (states, transitions, executor topology), "beyond human search," feeding `flow.optimize-*`:
- **Minimal form first:** a **canonical structural fingerprint/hash** (exact + near-duplicate detection) + the **crossmatrix relationships** (which tools/caps a workflow composes — already typed). At today's corpus scale (~17 flows) a model can read the YAML directly + use these.
- **Learned structural embedding** earns its place only at **corpus scale** (find the N most structurally-similar among thousands of community workflows without reading them all). De-solution: build the fingerprint now-ish; the learned embedding when the ecosystem is large.

### 3. The learned selector policy (evidence → decision)
v0.0.17 ships *annotate/rank* over the intent-index evidence; v0.0.18 turns accrued `{task_class, template} × success × cost` volume into a **selection policy** that recommends the highest-value composition. It needs the evidence volume that only accrues after v0.0.17's onboarding + drives run — hence sequenced here.

### The flywheel, assembled
- **Semantic half** — the description embeddings (find by meaning).
- **Evidence half** — the intent-index (which composition *actually wins*, by success × cost).
- **Meta-management half** — structural fingerprints/embeddings (dedup, cluster, optimize).
- **The engine** — praxec-meta's `flow.optimize-*` closes the loop: discover → evaluate → improve → mint a better process with better tools, and the improvement itself is measured (its own `outcome.recorded`) so the flywheel is self-correcting.

### Forward-compat locked in v0.0.17 (cheap, so done there)
The tool descriptor and workflow metadata reserve `#[serde(default)]` slots for `embedding` / `structural_fingerprint` (alongside `topology_refs` / `suggested_workflows`), so v0.0.18 populates them with **no breaking schema change**.

## Guardrails carried through
- **Trust boundary is invariant:** every connection activation is an operator grant (the grant gate), even for community packs — no auto-wiring, ever.
- **Deterministic-tools-over-LLM:** the seams (grant gate, descriptor schema, field-map, selector ranking, fingerprints) are engine Rust; the *authoring* (onboard-flow, suites, optimize-flows) is dogfooded via praxec. That split IS the equation.
- **De-solution discipline:** each release ships the minimal form; the richer mechanism waits for a proven need (structural *embeddings* at scale, a learned *policy* after evidence volume, semantic search after a reliable embedder).
