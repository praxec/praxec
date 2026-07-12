# Test plan — praxec v0.0.18 "the optimization flywheel"

Companion to `docs/plan-v0.0.18.md` (CPM `plan_id: plan_f043b6a5fc154be29bb5c85cb4031a90`). Acceptance criteria below are the `acceptance_criteria[]` emitted by the `cognitive/cap.plan.build-graph` run (`wf_c19648b8b6184651967b8094555d1658`), mapped to concrete test cases per deliverable, plus end-to-end validation for each of the three mechanisms.

Conventions: unit tests live beside the module or in the deliverable's owned `crates/*/tests/*.rs` file; integration tests use the existing gateway test harness in `crates/praxec/tests/`. No live network in CI — the "dependable embedder" is exercised against a local stub HTTP server; the deterministic `NoopEmbedder` (`crates/praxec-core/src/embeddings.rs`) backs pure-logic tests.

## Per-deliverable acceptance criteria → test cases

### D1-embedder-trait (`crates/praxec-embeddings/src/lib.rs`)
**Acceptance:** `RigEmbedder`/`EmbeddingChoice` exposes a health-check; a chosen embedder is either a local model or a fixed endpoint.

| # | test | kind |
|---|---|---|
| D1-T1 | `EmbeddingChoice::build()` for each supported vendor/model returns an embedder whose reported dims match the choice | unit |
| D1-T2 | health-check on a reachable embedder returns Ok; on an unreachable one returns Err (no panic, bounded time) | unit (stub server) |
| D1-T3 | `load_choice`/`save_choice` round-trips the persisted embedding choice (`choice_path`) | unit |
| D1-T4 | `recommend()` over `available_options()` is deterministic and never selects an unavailable option | unit |

### D2-http-embedder (`crates/praxec-core/src/embeddings.rs`)
**Acceptance:** `HttpEmbedder` wraps a fixed endpoint with timeout/retry and a reachable health probe that fails fast on startup if the endpoint is unreachable.

| # | test | kind |
|---|---|---|
| D2-T1 | request to a stub endpoint that delays past the timeout → typed `EmbeddingError` within the timeout bound (no hang) | unit (stub) |
| D2-T2 | transient 5xx then success → retry succeeds; retries are bounded (no infinite loop) | unit (stub) |
| D2-T3 | startup health probe against a dead port fails fast with a diagnosable error (fail-fast, not silent lexical) | unit |
| D2-T4 | `parse_embeddings_config` rejects malformed config with a clear error; absent config → `None` (embeddings off) unchanged | unit (regression) |
| D2-T5 | both `RequestFormat` variants serialize/parse against recorded stub fixtures | unit |

### D3-reembed-on-reload (`crates/praxec-core/src/hot_reload.rs`, `discovery/discovery_indexer.rs`)
**Acceptance:** hot-reload rebuilds the semantic index (not the lexical fallback) whenever definitions change; stale embeddings are invalidated and rebuilt.

| # | test | kind |
|---|---|---|
| D3-T1 | with an embedder configured, reload swaps in a `SemanticDiscoveryIndex` (assert index type/behavior via a query only semantics can answer), not `InMemoryDiscoveryIndex` — the gateway.rs:1424 defect | integration |
| D3-T2 | reload with NO embedder configured still builds the lexical index — existing behavior unchanged | integration (regression) |
| D3-T3 | item whose description changed on reload gets a fresh embedding; unchanged items may reuse cached vectors (invalidation correctness) | unit |
| D3-T4 | embedder failing mid-reload → reload completes on lexical, emits a loud audit event (degrade detectably, never wedge reload) | integration |
| D3-T5 | concurrent search during reload never observes a torn index (`SwappableDiscoveryIndex` swap atomicity) | unit |

### D4-semantic-discovery-surfaces (`discovery/discovery.rs`, `tool_descriptor.rs`)
**Acceptance:** ranking accepts embeddings from BOTH workflow/cap/skill descriptions AND tool/mcp/rest descriptors; vector similarity contributes to scoring alongside lexical.

| # | test | kind |
|---|---|---|
| D4-T1 | query term appearing in neither item's text but semantically close → semantic index finds it, lexical does not (extends the existing "speed" test at discovery.rs:966) | unit |
| D4-T2 | tool/mcp/rest descriptor descriptions are embedded and searchable via the same hybrid path (surface b) | unit |
| D4-T3 | `ToolDescriptor.embedding` slot populated at index time; a descriptor serialized without it still deserializes (`#[serde(default)]` regression) | unit |
| D4-T4 | per-item embed failure skips that item to lexical without failing the index build (existing documented behavior preserved) | unit (regression) |
| D4-T5 | hybrid score is monotone in each component: raising semantic sim (lexical fixed) never lowers rank, and vice versa | unit (property) |

### D5-structural-fingerprints (`catalog.rs`)
**Acceptance:** canonical fingerprint over every workflow graph (states, transitions, executor topology) reusing `contract_hash` canonicalization; `structural_fingerprint` populated for all persisted workflows; exact and near-dup detection functional.

| # | test | kind |
|---|---|---|
| D5-T1 | fingerprint is deterministic: same definition → same hash across runs | unit |
| D5-T2 | canonicalization invariance: reordering YAML keys / renaming non-structural metadata (title, description, tags) → SAME fingerprint | unit |
| D5-T3 | structural change (add/remove a state or transition, change an executor kind) → DIFFERENT fingerprint | unit |
| D5-T4 | exact-dup: two copies of `hello-flow.yaml` under different ids detected as exact duplicates | unit |
| D5-T5 | near-dup: clone with one transition changed → grouped as near-duplicate; a structurally unrelated flow → not grouped (both directions) | unit |
| D5-T6 | every workflow in the shipped cognitive pack gets a non-empty `structural_fingerprint` after cataloging (not serde-default) | integration |

### D6-registry-v3-wiring (`registry_v3.rs`, `crates/praxec/src/gateway.rs`)
**Acceptance:** gateway loads registry_v3 at startup and passes topology into `rank_candidates` (no longer `None`); crossmatrix topology visible in ranking.

| # | test | kind |
|---|---|---|
| D6-T1 | gateway startup with a v3 registry file (`praxec.packs/v3`) → `rank_candidates` receives `Some(registry)` (assert via ranking difference or instrumented seam) | integration |
| D6-T2 | startup with NO v3 registry → `None` path unchanged, no error (regression) | integration |
| D6-T3 | malformed v3 registry → fail-fast at load with a clear error, not silent `None` | unit |
| D6-T4 | a workflow crossmatrix-linked to tools matching the query outranks an otherwise-equal unlinked workflow (topology affects rank; extends `crates/praxec-core/tests/selector_rank.rs`) | unit |
| D6-T5 | registry survives hot-reload: topology still feeds ranking after a reload cycle (interlock with D3) | integration |

### D7-learned-selector-policy (`discovery/selector.rs`, `intent_index.rs`)
**Acceptance:** evidence tuples `{task_class, template, success, cost}` accumulate per selection; policy prefers higher success-rate / lower cost; policy engages ONLY above a configurable evidence-volume threshold.

| # | test | kind |
|---|---|---|
| D7-T1 | `aggregate()` over synthetic `OutcomeObservation`s yields correct per-`{task_class, template}` success-rate and mean-cost | unit |
| D7-T2 | cold start: evidence below threshold → ranking identical to current (pre-policy) `rank_candidates` output | unit |
| D7-T3 | warm: above threshold, template A (90% success, low cost) recommended over B (40%, high cost) for the same task_class | unit |
| D7-T4 | cost/success trade-off tie-break is deterministic and documented (same evidence → same recommendation) | unit |
| D7-T5 | threshold is config-tunable (`tuning.rs` path); setting it high disables the policy entirely | unit |
| D7-T6 | evidence from `observations_from_audit` on a real audit fixture feeds the policy end-to-end (no synthetic-only proof) | integration |

### D8–D11 (test deliverables)
D8, D9, D10, D11 ARE the test files above: D8 = D1–D4 unit/pipeline suites; D9 = D5 suite; D10 = the gateway integration cases (D3-T1/T2/T4, D6-T1/T2/T5, D5-T6); D11 = D7 suite. Their own acceptance criterion: all listed cases implemented and green, plus the whole existing suite passes (no-regression criterion from the build-graph run).

## End-to-end validation per mechanism

### E2E-1: Semantic search embeddings (mechanism 1)
1. **Embedder-down → lexical fallback:** start gateway with an embedder config pointing at a dead endpoint. Expect: fail-fast diagnosable startup error for the health probe path; in degraded-allowed mode, search still answers lexically and the degradation is audit-visible — never a silent lexical downgrade. `praxec.query {query}` keeps working throughout.
2. **Re-embed-on-reload:** start with a working (stub) embedder → add a new workflow definition file to a watched repo → hot-reload fires → query with a paraphrase that shares no keywords with the new workflow's description → the new workflow is found (proves semantic index rebuilt post-reload, closing the gateway.rs:1424 lexical-fallback defect).
3. **Both surfaces:** the paraphrase test passes for (a) a workflow/cap description and (b) a tool descriptor description.
4. **Live symptom regression:** the query `"cpm schedule critical path status"` no longer ranks `cognitive/inspect.git.status` above planning workflows (the lexical keyword-collision observed while producing this plan).

### E2E-2: Structural fingerprints (mechanism 2)
1. Catalog the shipped cognitive pack → every workflow has a non-empty fingerprint.
2. Copy one flow YAML to a new id, reload → exact-duplicate pair reported.
3. Modify one transition in the copy, reload → pair now reported as near-duplicate, not exact.
4. Fingerprints appear in the `structural_fingerprint` slot of serialized descriptors and round-trip through serde (forward-compat slot populated, non-breaking).

### E2E-3: Learned selector policy (mechanism 3)
1. Seed an audit log with synthetic `outcome.recorded` events: task_class X where template A wins 9/10 cheap, template B wins 3/10 expensive.
2. Below the evidence threshold: `praxec.query` ranking for X matches pre-policy output (cold-start guard).
3. Above threshold: A is recommended over B, and the recommendation surfaces the evidence (`{success_rate, mean_cost, runs}` annotation from 0.0.17) so it is auditable, not oracular.
4. Kill-switch: threshold set to ∞ (or policy disabled in tuning) → behavior identical to 0.0.17 evidence-annotation.

### Cross-mechanism regression gate
Full workspace suite green (`cargo test --workspace`) with embeddings OFF — the entire 0.0.18 feature set must be additive: no embedder configured + no v3 registry + policy below threshold ≡ 0.0.17 behavior.
