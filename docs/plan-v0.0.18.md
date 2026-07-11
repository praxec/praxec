# Build plan — praxec v0.0.18 "the optimization flywheel"

Planned 2026-07-11 through praxec's own planning workflows (dogfood). Scope source of truth:
`docs/roadmap-v0.0.18-optimization-flywheel.md`.

- Deliverables DAG: produced by `cognitive/cap.plan.build-graph` — workflow `wf_c19648b8b6184651967b8094555d1658`, `result.status: succeeded`.
- CPM schedule: submitted via `cognitive/cap.coordinate.cpm-plan` — workflow `wf_81d967b0fc064240b35165b8fcb65f66`, **`plan_id: plan_f043b6a5fc154be29bb5c85cb4031a90`** (cpm-planner `plan.submit` confirmed by evidence record `638e3ad5`).
- Overall risk: **medium**. Blast radius: the discovery/selection surface (every `praxec.query` search + auto-selection ranking) and the hot-reload path; no engine execution semantics change. All schema slots (`embedding`, `structural_fingerprint`) already exist as `#[serde(default)]` — populating them is non-breaking.

## Scope recap (three mechanisms + wiring debt)

1. **Semantic-search description embeddings** — re-enable the existing embedding discovery. Grounded defect: `SemanticDiscoveryIndex::build` runs only at gateway startup (`crates/praxec/src/gateway.rs:1031`); the hot-reload rebuild path (`gateway.rs:1424-1425`) constructs a lexical `InMemoryDiscoveryIndex`, so any reload silently drops semantics. Hard prerequisite: a dependable embedder + re-embed-on-reload.
2. **Structural fingerprints** — canonical structural fingerprint/hash (exact + near-dup detection) reusing `contract_hash` canonicalization; populate the existing `structural_fingerprint` slot (`crates/praxec-core/src/tool_descriptor.rs:309-311`). The **learned** structural embedding is deferred to corpus scale — NOT in 0.0.18.
3. **Learned selector policy** — turn `intent_index` evidence `{task_class, template} × success × cost` into a selection policy. Sequenced **last** (needs evidence volume that accrues only after 0.0.17 onboarding/drives run).
4. **Carried wiring debt** — `registry_v3` (`praxec.packs/v3`) loader exists (`crates/praxec-core/src/registry_v3.rs`) but is unwired: `rank_candidates(hits, registry: Option<&Registry>)` in `crates/praxec-core/src/discovery/selector.rs:164` already accepts the crossmatrix registry, yet nothing outside tests constructs one. Load at gateway startup and feed topology into ranking.

## Deliverables DAG

| id | owned_files (disjoint) | prerequisites | effort (h) | risk |
|---|---|---|---|---|
| D1-embedder-trait | `crates/praxec-embeddings/src/lib.rs` | — | 3 | medium — provider surface (`RigEmbedder`/`EmbeddingChoice`), health-check contract |
| D2-http-embedder | `crates/praxec-core/src/embeddings.rs` | D1 | 3 | medium — `HttpEmbedder` timeout/retry + startup health probe; must fail fast, not hang (the original flaky-endpoint failure mode) |
| D3-reembed-on-reload | `crates/praxec-core/src/hot_reload.rs`, `crates/praxec-core/src/discovery/discovery_indexer.rs` | D2 | 5 | **high** — the reload swap path (`SwappableDiscoveryIndex`); a blocking or failing re-embed must degrade to lexical, never wedge reload |
| D4-semantic-discovery-surfaces | `crates/praxec-core/src/discovery/discovery.rs`, `crates/praxec-core/src/tool_descriptor.rs` | D3 | 5 | medium — hybrid scoring changes every search result ordering; two surfaces (workflow/cap/skill descriptions + tool/mcp/rest descriptors) |
| D5-structural-fingerprints | `crates/praxec-core/src/catalog.rs` | — | 4 | low — new pure function over the workflow graph, reuses `contract_hash` canonicalization |
| D6-registry-v3-wiring | `crates/praxec-core/src/registry_v3.rs`, `crates/praxec/src/gateway.rs` | D4 | 4 | medium — gateway.rs wiring; topology in ranking changes selection results |
| D7-learned-selector-policy | `crates/praxec-core/src/discovery/selector.rs`, `crates/praxec-core/src/intent_index.rs` | D4, D6 | 5 | **high** — changes what gets selected; cold-start must fall back to current ranking below an evidence-volume threshold |
| D8-embedding-pipeline-tests | `crates/praxec-embeddings/tests/embedder_tests.rs`, `crates/praxec-core/tests/embedding_tests.rs`, `crates/praxec-core/tests/discovery_tests.rs` | D4 | 4 | low |
| D9-structural-fingerprint-tests | `crates/praxec-core/tests/catalog_tests.rs` | D5 | 2 | low |
| D10-registry-gateway-integration-tests | `crates/praxec/tests/gateway_integration_tests.rs` | D6, D8 | 3 | low |
| D11-selector-policy-tests | `crates/praxec-core/tests/selector_tests.rs` | D7 | 3 | low |

Total effort: 41h. Acceptance criteria per deliverable are in `docs/test-plan-v0.0.18.md` (emitted by the same build-graph run).

## CPM schedule (plan_f043b6a5fc154be29bb5c85cb4031a90)

Computed forward/backward pass (see Dogfood findings — the schedule detail was recomputed deterministically because the cap response carried only the plan_id):

**Project duration: 28h** (with unlimited parallelism).

**Critical path (zero slack):**
`D1-embedder-trait → D2-http-embedder → D3-reembed-on-reload → D4-semantic-discovery-surfaces → D6-registry-v3-wiring → D7-learned-selector-policy → D11-selector-policy-tests`

| deliverable | ES | EF | LS | LF | slack |
|---|---|---|---|---|---|
| D1 | 0 | 3 | 0 | 3 | 0 (critical) |
| D2 | 3 | 6 | 3 | 6 | 0 (critical) |
| D3 | 6 | 11 | 6 | 11 | 0 (critical) |
| D4 | 11 | 16 | 11 | 16 | 0 (critical) |
| D5 | 0 | 4 | 22 | 26 | 22 |
| D6 | 16 | 20 | 16 | 20 | 0 (critical) |
| D7 | 20 | 25 | 20 | 25 | 0 (critical) |
| D8 | 16 | 20 | 21 | 25 | 5 |
| D9 | 4 | 6 | 26 | 28 | 22 |
| D10 | 20 | 23 | 25 | 28 | 5 |
| D11 | 25 | 28 | 25 | 28 | 0 (critical) |

**Parallel cohorts (topological waves):**

1. **Cohort 1 — foundation:** D1-embedder-trait ∥ D5-structural-fingerprints *(dependable embedder is the hard prerequisite; fingerprints are fully independent — 22h slack)*
2. **Cohort 2:** D2-http-embedder ∥ D9-structural-fingerprint-tests
3. **Cohort 3:** D3-reembed-on-reload
4. **Cohort 4:** D4-semantic-discovery-surfaces
5. **Cohort 5:** D6-registry-v3-wiring ∥ D8-embedding-pipeline-tests
6. **Cohort 6:** D7-learned-selector-policy ∥ D10-registry-gateway-integration-tests
7. **Cohort 7:** D11-selector-policy-tests

**Schedule note (soft dependency flag):** the generated graph serializes D6-registry-v3-wiring behind D4 (the `rank_candidates` signature/scoring stabilization). Functionally the registry loading half of D6 is independent of embeddings; if that edge were relaxed, D6 could start in cohort 1 and the critical path would shorten to 24h (D1→D2→D3→D4→D7→D11). Kept as scheduled — the shared ranking seam in `selector.rs` makes the conservative ordering cheap insurance against merge churn — but a builder starved for parallel work can safely pull D6's loader half forward.

## Recommended build sequence

1. **Cohort 1 first, exactly as the roadmap sequences it:** the dependable embedder (D1→D2) is the hard prerequisite mechanism #1 blocks on — health-probe + fail-fast so the flaky-endpoint failure that got embeddings cut from 0.0.17 cannot recur silently. D5 fingerprints ride in parallel (independent file-set, huge slack).
2. **Re-embed-on-reload (D3) before any semantic surface work (D4):** fixing the reload-falls-back-to-lexical defect is what makes embeddings *stay* on; shipping D4 without D3 recreates the 0.0.17 symptom.
3. **Wiring debt (D6) + embedding tests (D8) in parallel** once the ranking seam is stable.
4. **Selector policy (D7) last** — deliberately at the end of the critical path: it needs the evidence volume from 0.0.17 drives, and it must ship with the cold-start threshold (below evidence volume → current ranking unchanged).
5. Integration/policy tests (D10, D11) close the release.

**Explicitly NOT in 0.0.18:** the learned structural embedding (corpus scale only) and any embedder beyond the minimal dependable form.

## Top risks

1. **D3 re-embed-on-reload (high):** the reload path currently degrades silently to lexical; the fix must neither wedge hot-reload on a slow/failed embed nor reintroduce silent degradation — degrade loudly (audit event) and recover on next reload.
2. **D7 selector policy (high):** a learned policy that activates on thin evidence will make worse selections than the current evidence-annotation. The configurable evidence-volume threshold with fall-through to existing ranking is the guard.
3. **D2 embedder dependability (medium):** the root cause of the 0.0.17 cut. Startup health probe must fail fast (fail-fast, no fallback masking) and the per-query path must degrade per-item to lexical, matching the existing documented behavior in `discovery.rs`.

## Dogfood findings

Honest record of the praxec runs behind this plan (all on 2026-07-11):

1. **Auto-drive collapsed the HATEOAS ceremony (works, but the stepped path was untestable).** `praxec.command {definitionId: "cognitive/cap.plan.build-graph", input:{spec:...}}` returned `status: succeeded` with `chain: [ready→done via submit_graph]` in a *single* call — the in-workflow model generated and submitted the `graph` itself from the spec input. I never got a `waiting` state with links to submit the graph via `workflowId + expectedVersion + transition` as the documented flow describes. The output graph was high quality (it honored the grounded module paths and sequencing hints from the spec), so this is a UX observation, not a defect — but the "follow the returned links" contract was unexercisable for this cap.
2. **`blast_radius` not emitted despite being required by the cap contract.** `cognitive/cap.plan.build-graph`'s description mandates emitting `risk` + `blast_radius`; the succeeded run returned `"blast_radius": null` and no gate rejected it. The blast-radius statement in this document was written by hand.
3. **`cognitive/cap.coordinate.cpm-plan` returns only the plan_id, not the schedule.** The cap's description promises "the computed schedule (critical path, bottlenecks, parallel cohorts)", and the evidence record shows `cpm-planner.plan.submit` was called — but `context.schedule` contained exactly `{"plan_id": "plan_f043b6a5fc154be29bb5c85cb4031a90"}`. The critical path/slack/cohort detail was never projected into the workflow context.
4. **The plan is not queryable after the fact.** `praxec.query {subject: "plan_f043b6a5fc154be29bb5c85cb4031a90"}` → `item: null`, and no `plan.get` / `plan.status` capability is discoverable via search. **Completed by hand:** the CPM forward/backward pass and cohort table above were recomputed with a deterministic script from the exact submitted graph (not LLM arithmetic); the plan_id is real, the schedule numbers in this doc are the by-hand recomputation.
5. **Embeddings-off symptom observed live (mechanism #1's own motivation).** `praxec.query {query: "cpm schedule critical path status"}` ranked the `cognitive/inspect.git.status` shell script as the #2 hit (score 14) — a pure lexical keyword collision on "status", semantically irrelevant. Discovery is confirmed lexical-only today.
6. **Soft-dependency pessimism in the generated graph.** The auto-driven decomposition added D4→D6 (registry wiring behind semantic surfaces), serializing functionally independent work; see the schedule note above. Accepted as-scheduled, flagged for the builder.
7. **`cognitive/cap.plan.technical-design` was skipped (optional per the task).** The build-graph output plus the roadmap's own mechanism split covered the design layer sufficiently; running it would have added ceremony, not information, at this scope.

No broken transitions, no executor failures, no cpm-planner errors were hit. Both workflow runs succeeded on the first attempt.
