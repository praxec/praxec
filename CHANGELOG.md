# Changelog

All notable changes to **praxec** are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
on the cargo crate version. The **config schema** is versioned
separately — see [`docs/reference/stability.md`](docs/reference/stability.md) for what is and isn't
covered by a stability commitment.

## [Unreleased]

> **Note on versioning.** This is a pre-1.0, greenfield project on the `0.0.x`
> line: nothing is API-stable, and any release may change anything (breaking
> changes are cut over cleanly, by design). The `0.0.6`–`0.0.13` sequence below
> reconstructs the June "Mission Control" development arc as dated milestones;
> none were tagged at the time. Versions `0.0.1`–`0.0.5` are the earlier
> development history, renumbered onto this line.

## [0.0.18] — 2026-07-11 — the optimization flywheel

Planned by dogfooding praxec's own planning surface
(`cognitive/cap.plan.build-graph` → `cognitive/cap.coordinate.cpm-plan`) and
built cohort-by-cohort against that dependency-ordered plan
(`docs/plan-v0.0.18.md`, `docs/test-plan-v0.0.18.md`). The release makes the
tool/workflow ecosystem *compounding*: discover → apply → gather evidence →
improve. Everything here is **additive** — with no embedder, no v3 registry, and
the selector policy below its evidence threshold, behavior is identical to 0.0.17.

### Added — semantic discovery (mechanism 1)

- **Dependable embedder.** `EmbeddingProvider` gains a mandatory `health_check()`
  (a real round-trip, deliberately with no default impl, so an unprobed provider
  cannot claim health). `HttpEmbedder` now builds its client with explicit connect
  + request timeouts and a bounded retry for *transient* failures only — a timeout
  is fatal, not retried, because retrying multiplies the very wait the budget
  exists to cap. This closes the flaky-endpoint **hang** that got embeddings cut
  from 0.0.17 (the client was previously built with no timeout at all).
- **Re-embed on reload.** Startup and hot-reload now build the discovery index
  through **one** seam (`discovery::build_discovery_index`). Previously reload
  rebuilt a *lexical* index and swapped it in, silently and permanently
  downgrading discovery from semantic to lexical on any config/pack reload. Any
  embedder failure now degrades to lexical **loudly** (audit `discovery.index_degraded`),
  never silently, and never stickily — the next reload with a healthy embedder
  restores semantics.
- **Hybrid semantic search over two surfaces.** Ranking blends lexical relevance
  with embedding cosine similarity (`0.5·lex + 0.5·cos`), over both (a) workflow /
  cap / skill descriptions and (b) tool / mcp / rest descriptors (`kind: "tool"`).
  The weighting preserves lexical precision by construction — a zero-lexical item
  can never outbid an exact keyword match — while letting a semantically-relevant
  item outrank a keyword *collision*. Fixes the observed case where
  `cognitive/inspect.git.status` outranked planning workflows on a shared "status"
  keyword, and where an existing capability was undiscoverable by meaning.

### Added — structural fingerprints (mechanism 2)

- **Canonical structural fingerprint + duplicate detection** over a workflow's
  actual graph (states, transitions, executor topology), reusing the existing
  `contract_hash` canonicalization. Declaration order and prose don't move the
  hash; graph structure does. Exact-duplicate grouping + Jaccard near-duplicate
  detection give praxec-meta a screening signal for dedup/cluster/merge, feeding
  `flow.optimize-*`. The *learned* structural embedding is intentionally deferred
  to corpus scale.

### Added — evidence-driven selection (mechanism 3)

- **Learned selector policy.** Accrued `{task_class, template} × success × cost`
  evidence from the intent index now actively re-ranks toward the highest-value
  composition — guarded by a per-pair **evidence-volume threshold**
  (`intent.policy_min_runs`, default 10). Below it, ranking is bit-for-bit the
  0.0.17 evidence-annotation blend (the cold-start guard: a policy on thin evidence
  selects worse than none). Activation is explainable in the `why` line, and the
  threshold is a tuning knob (set out of reach to disable — the kill switch).

### Added — registry topology wiring

- **Registry v3 is loaded and live.** The `praxec.packs/v3` loader (foundation-only
  in 0.0.17) is now loaded at gateway startup from `discovery.registry`, threaded
  into `rank_candidates`, and swapped atomically with the index on reload. The
  crossmatrix tool × workflow topology term — previously present but dead in
  production — now influences ranking, and the registry's tool descriptors become
  searchable through live discovery. A configured-but-unloadable registry fails
  fast rather than booting registry-less.

### Fixed — orchestrate / auto_drive multi-step hang

An investigation into a "multi-step reasoning `auto_drive` hangs at 0 CPU"
report found the headline lead ("timeout after 60 ms") to be a **cosmetic
mislabel**, not a functional bug — but it surfaced several real, distinct hangs.
The prior model-chain circuit breaker (30-min cooldown + half-open re-probe),
chain-walk escalation, and `host.tools()` setup timeout were already correct; the
gaps were elsewhere:

- **The stall watchdog is live again on the model call.** The provider factory
  drained rig's whole turn into a `Vec` *before* returning the stream, so the
  runner's per-event stall watchdog only ever polled an already-materialized
  buffer — the real model wait (including a hang at first token) happened outside
  it, bounded only by the 600s session wall. The factory now streams **lazily**
  (an `async_stream` generator), so a hung/silent reasoning call is caught at the
  `stall_timeout` and escalates, exactly as advertised.
- **Headless HITL gates no longer park forever.** A headless run that reached a
  `human_decision` gate parked on an unbounded `oneshot` the policy could never
  answer (P16 refuses a non-human resolver) — the driver sat at 0 CPU
  indefinitely, orphaning parent + child instances. The headless consumer now
  **abandons** an unanswerable gate (resolving it as declined, never a forged
  approval) so the mission terminates cleanly, and it survives a lagging event
  channel instead of silently dying and stranding every future park.
- **Per-call timeout on tool invocations.** A hung MCP tool server inside
  `host.call` was bounded only by the session wall. Each call now has a generous
  per-call ceiling; a timeout is a **non-fatal** tool error (the model sees it and
  can recover) rather than a silent 0-CPU block.
- **A working, server-side `cancel` verb.** `praxec.command
  { "intent": "cancel", "workflowId": "…" }` now cancels a running workflow (the
  `Runtime::cancel` primitive existed but was wired to no verb) — the operator's
  reap for an instance whose driver/CLI died. The CLI exposes it through the same
  passthrough (`px command '{"intent":"cancel","workflowId":"…"}'`).
- **Honest error labels.** `ExecutorError::Timeout` is milliseconds everywhere
  (matching every other construction site); two sites fed `.as_secs()`, printing a
  real 60-second timeout as "timeout after 60 ms" (the report's red herring). The
  `orchestrate` credentials-path hint now reports the actual resolved
  `providers.env` path (XDG-first) instead of the stale legacy `~/.praxec` one.

## [0.0.17] — 2026-07-10 — tool-source ecosystem & governed connections

> **This release bundles every 0.0.16 improvement.** There is no separate 0.0.16
> cut: the 0.0.16 self-improvement program (observability, self-healing, durable
> planning, telemetry) ships here alongside the 0.0.17 tool-source ecosystem.

### Added — the tool-source ecosystem (headline)

- **Tool descriptor schema (`praxec.tool/v1`)** — a schema-first descriptor
  (`schemas/tool-descriptor.schema.json`) that describes a **cli, mcp, or rest**
  tool uniformly: identity, `kind`, its connection requirement (`reach`, which
  embeds the existing gateway connection shape verbatim — install = copy, never
  transform), invocation `operations[]`, and `suggested_workflows[]`. Typed
  loader with fail-fast `validate()` in `praxec-core::tool_descriptor`.
- **Tool-source executor** — ingests a descriptor and surfaces its operations as
  a callable tool through the gateway, dispatching per kind by reusing the
  existing mcp/cli/rest transports. Fail-fasts (never auto-grants) when the
  required connection is absent or ungranted.
- **Registry v3 (`praxec.packs/v3`) — schema + loader foundation.** A compatible
  superset of the v2 pack registry: each tool may carry a descriptor (so the
  registry can span cli + rest, not just mcp), plus per-tool `suggested_workflows`
  and a top-level `crossmatrix` (tool × workflow) topology. Ships as the typed
  loader (`praxec-core::registry_v3`, `workflows_for_tool` / `tools_for_workflow`).
  **Foundation only in 0.0.17** — the runtime wiring that loads a v3 registry and
  feeds its topology into discovery ranking lands in **v0.0.18** (the optimization
  flywheel; see `docs/roadmap-v0.0.18-optimization-flywheel.md`).
- **Evidence-aware selector.** Deterministic candidate ranking (`rank_candidates`,
  wired into `praxec.query` discovery search) combining lexical relevance with
  item1 intent-evidence, carrying an explainable `why` line. The compiled-tool-
  determinism middle of human-intent × tool-determinism × model-generation. The
  registry-**topology** term is present but neutral until a registry is wired into
  the runtime (v0.0.18).
- **`px connections add` / `px connections grant`** — a governed connection
  write path. `add` writes a connection **staged/ungranted**; `grant` is the
  separate, explicit, auditable trust act (emits `connections.granted`).

### Security

- **Operator grant gate for repo-contributed connections.** Repo/pack-declared
  connections are no longer auto-trusted — a supply-chain hole. They are stamped
  `_ungrantedConnections` at load and every consumer (cli/mcp/rest) fail-fasts
  with `UNGRANTED_PACK_CONNECTION` until the operator grants them. A
  CLI-staged connection (`px connections add`) is treated identically until
  granted, so no code path can silently obtain a trusted connection.

### Fixed — v0.0.17 dogfood hardening

Found by dogfooding the release *before* tagging (see
`docs/v0.0.17-functional-validation.md`):

- **Credential path — the reasoning-agent hang trigger.** `providers.env` is now
  resolved from the XDG config dir (`~/.config/praxec/`) first, so keys stored
  beside the gateway config are auto-loaded; previously only `~/.praxec/` was
  consulted → no credentials → agent model calls fast-failed and multi-step
  auto-drive appeared to hang.
- **Silent-drop of unloadable pack YAML.** A definition file in a remapped/
  unscanned tier directory now emits `UNSCANNED_DEFINITION_DIR` at load + `check`
  instead of vanishing with no feedback.
- **Pack freshness.** The staleness recheck now watches `repos:` definition files,
  so a pack edit triggers the same gated reload as a config edit.
- **Validated connection promotion.** A granted staged connection body is
  validated against the gateway connection shape before going live
  (`INVALID_STAGED_CONNECTION`).
- **`connections grant` requires operator origin** — a non-interactive caller must
  pass `--yes`; a human at a TTY is unaffected (the audit records how origin was
  proven).

### Deferred to v0.0.18 (foundation shipped, wiring pending)

- Tool-descriptor **`auth`** block: **removed** from the 0.0.17 surface (it was
  parsed but never enforced — advertising unenforced auth is a footgun). Returns
  in v0.0.18 as *enforce-then-declare*.
- Tool-descriptor **`provision`** block: schema/foundation only in 0.0.17.
- **Registry topology** in discovery ranking + semantic-search embeddings — the
  optimization flywheel (`docs/roadmap-v0.0.18-optimization-flywheel.md`).

### Added — observability (0.0.16)

- **Structured harness-event stream** (not LLM tokens) exposing execution
  topology, cross-platform: `agent.heartbeat` liveness pulses, execution-tree
  linkage (`parent_workflow_id` + `depth`), audit-granule rotation + retention,
  a published `AuditEvent` JSON Schema (`praxec schema audit-event`),
  `praxec observe --follow`, and the MCP `praxec.query { observe }` read.
- **Intent-evidence on discovery** — `praxec.query` workflow hits carry
  `evidence:{runs, success_rate, mean_cost}` from recorded outcomes (gated at a
  minimum run count), so discovery is no longer blind to what actually worked.

### Added / Changed / Fixed — 0.0.16 self-improvement program

- Durable CPM control plane (sqlite + retry circuit-breaker), INCOSE/SEBoK Vee
  flow, per-model cooldown breaker over the chain-walk, bounded reasoning-stall,
  credential preflight + `praxec doctor`, cost/affinity telemetry, staged
  `cargo_scope` build-loop throughput, and pack-wide guard-failure→`outcome:
  failure` correctness. See `docs/v0.0.16-dogfooding-report.md` for the full
  program + honest A/B findings.

## [0.0.15] — 2026-07-09 — resilient serve & self-healing misconfiguration

### Fixed — HOP: FM-7 exempts the resolved slot cap (typed `snippet.outputs`)

- **`SLOT_KEY_ENGINE_OWNED` no longer rejects the resolved slot cap.** FM-7
  exempted only `hop_slot:`-declared transitions, but the cap a `hop_slot: <slot>`
  flow resolves to (`cap.verify.<stack>`) declares
  `snippet.outputs.<slot>: { $ref: praxec://hop#/$defs/<slot>Out }` and writes
  `output.<slot>` — the sanctioned typed production, runtime-validated against the
  same contract by `validate_outputs_against_snippet`. Both shipped packs use this
  shape, so a live gateway failed config load (surfacing as an opaque MCP
  `-32000`). The lint now exempts a slot-key write whose enclosing workflow
  declares the canonical `<slot>Out` `snippet.outputs`; an untyped declaration does
  **not** earn the exemption (the forge hole stays closed).

### Added — misconfiguration is a live, self-documenting state (degraded serve)

- **`serve` no longer hard-crashes on a bad config.** A config fault (parse
  error, the durability guard, or a validation lint like `SLOT_KEY_ENGINE_OWNED`)
  used to abort **before** the MCP transport came up, so the client saw an opaque
  transport `-32000` with no diagnosis. Now `serve` captures the fault and comes
  up **DEGRADED**: a live server that completes the handshake and answers **every**
  call with a precise, self-documenting `HealthReport` — code, location, detail,
  ordered remedies, and the reload path — as both a rich message and structured
  `data`, so an LLM operator can self-heal and reconnect. It does zero governed
  work; it refuses everything, loudly and precisely (not a fallback). Recovery is
  a reconnect — a fresh process loads the corrected config. The declarative repair
  loop lives in praxec-meta (`meta/flow.repair-workflow-health`).

### Added — default reasoning effort for agent turns

- **`kind: agent` turns now default to `low` reasoning effort** via the new
  `ReasoningTuning.default_effort` config field. A *reasoning* model leading a
  chain would otherwise spend the whole turn budget on hidden reasoning, which
  surfaces as empty content and an `AGENT_NO_RESULT` stall. A step's explicit
  `reasoning_effort` still wins; setting `default_effort: ""` opts out (provider
  default). `low` (not `medium`) because `medium` is a no-op (≡ provider
  default in `reasoning_params`).

### Fixed — agent-execution setup: make it work and fail honestly

- **Chooser failures surface honestly instead of masquerading as "gave up."**
  `TransitionChooser::choose` now returns `Result`, and `drive_mission` maps a
  runner failure (missing API key, 401, model-resolution, network) to a new
  `DriveOutcome::ChooserFailed { source }` that renders the real error — instead
  of the misleading "no actionable move… legal actions: […]" (the old `.ok()?`
  swallowed every error into a false give-up). A legitimate no-move still reads
  as give-up.
- **`praxec check` flags agent steps with no `gateway.models_yaml`** at load
  (`AGENT_MODELS_YAML_REQUIRED`), rather than only failing at first dispatch with
  `AGENT_NO_AGENTS_YAML`.
- **The `praxec` gateway binary loads `~/.praxec/providers.env` at startup**
  (previously only `px` did), so provider keys set via `px set-provider-keys`
  reach `serve`/`orchestrate`/`command`. Environment still wins over the file.
- **One canonical models path.** `meta/flow.configure-models` now writes
  `.praxec/models.yaml` (was `.praxec/agents.yaml`), matching `px doctor`
  discovery and the `gateway.models_yaml` runtime key — one name for one
  `ModelsFile` schema.
- Corrected `docs/reference/configuration.md`: `kind: agent` runs a governed
  in-process rig session, not a subprocess.

### Added — value-based model selection

- **`value = fit(needs) / blended_cost^β`** over the model catalog: among
  tool-capable, reachable models the suggestor now trades capability against a
  blended (input+output) cost, with a price-sensitivity exponent `β` and a
  marginal-value band that keeps the stronger model when a cheaper one is only
  marginally better. The cockpit's capability/cost stances route through the
  value selector.

### Added — cost governance: `cost report` + `cost propose`

- **`cost report`** (`praxec cost report --config <gw> [--workflow] [--since] [--json]`)
  — aggregates realized cost from `agent.completed` audit telemetry: total,
  by-model, by-step, plus the **counterfactual** — the same realized tokens
  repriced at the most-capable ("ceiling") catalog model, reported as
  "saved Z% vs ceiling". Uncatalogued models are flagged and excluded, never
  panic. Reuses `model_catalog::cost_usd_in`.
- **`cost propose`** (`praxec cost propose --config <gw> [--json] [--request-approval]`)
  — the governed **slow loop**: aggregate per-`(affinity, model)` run count,
  pass-rate (next-transition advanced vs `chain.failed`/abort), and mean cost
  from the audit, then propose **conservative, bidirectional** base-model
  changes — *lower* a base only when a cheaper catalogued model clears the bar
  with pass-rate ≥ the base's AND material savings; *raise* when the base is
  chronically failing. Never edits `models.yaml`; with `--request-approval` it
  files `human.approval.requested` events into the existing approvals gate.
  Thresholds are data (`tuning.deescalation`). "Passed" is the independent
  acceptance bar — never a model grading itself.
- **Per-call cost telemetry** — `agent.completed` now carries realized
  `prompt_tokens` / `completion_tokens` / `cost_usd` on the agent auto-drive
  path; this is the signal both cost loops consume.

### Changed — model catalog refresh (2026-06 OpenRouter / AA v4.1)

- Re-priced `qwen3-coder` and `glm-5.2`; de-rated second-tier open-weight
  intelligence (`minimax-m3`, `deepseek-v4-pro`, `kimi-k2.6`) against AA
  Intelligence Index v4.1; populated the `prose` affinity sub-score on every
  entry (calibrated estimates — AA publishes no prose axis; header notes the
  provenance).

### Added — provider-resilience NFR contract

- Typed `Auth` provider error, retry **jitter**, `Retry-After` handling, and an
  NFR contract test pinning the resilience behaviour.

### Fixed — runtime & durability

- Deterministic chain honors `ExecuteResult.suspend` (no longer advances past a
  suspending step).
- Sequential `kind: workflow` leaves clear `_subworkflow_wait`, so each spawns a
  fresh child instead of re-binding the previous one.
- `serve` rejects ephemeral store paths (durability poka-yoke).
- Script executor resolves `$.` expressions in `workingDirectory`.
- Workspace cleared of pre-existing fmt + clippy lints.

### Removed — `/spikes` validation scratch directory

- The `spikes/0006-sandbox-exec` coordination/mechanism proof is removed from
  source control; the ADR-0006 + source provenance notes that referenced it were
  updated to drop the dead path.

## [0.0.14] — 2026-07-08 — HOP typed-core & the `hop_slot` primitive

### Added — stack-aware specialization: the HOP typed-core (Spec A.1)

- **Canonical HOP vocabulary (`schemas/hop.schema.json`), shipped and runtime-
  registered.** A standalone JSON Schema (draft 2020-12) defining the shared
  building blocks (`severity`, `gateStatus`, `schemaBound`, `stackProvenance`,
  `finding`, `criterion`) and the ten per-slot `In`/`Out` contracts
  (`verify`/`detect`/`scaffold`/`implement`/`lint_format`). It is embedded in
  `praxec-core` and prepared once into a process-wide `jsonschema` registry under
  the alias `praxec://hop`, forced at serve startup so a malformed shipped schema
  fails at boot rather than mid-run.
- **The `hop_slot:` primitive — the unbypassable contract.** A transition marked
  `hop_slot: <name>` has, at config load, its canonical `In` contract injected as
  the transition `inputSchema` and its `Out` contract injected as the
  `$.context.<name>` typed blackboard slot; the existing per-transition input and
  blackboard-write seams (now registry-aware, resolving `praxec://hop` `$ref`s)
  then enforce both with no new runtime code. An unknown slot name is a hard
  load-time error listing the valid names.

## [0.0.13] — 2026-06-16 — release hygiene

Greenfield cleanup before consolidating onto `main`. Breaking config changes —
each carries an inline migration note.

### Removed — postgres store backend (BREAKING: `store.kind: postgres`)

- **Dropped the postgres `WorkflowStore` rung and the `sqlx` dependency.** For a
  locally-installed binary, `file` and `sqlite` are both just files on disk;
  postgres was the only true database-server backend, the weakest fit, and
  already half-baked — it persisted only the workflow store while evidence and
  acknowledgments silently fell back to in-memory, and it was never in the config
  schema enum. `store.kind: postgres` now fails fast: `unknown store kind 'postgres'`.
  Removing `sqlx` also let us drop the standing `RUSTSEC-2023-0071` (rsa) audit
  ignore it dragged into the lockfile.

### Changed — serve refuses a durable workflow store paired with ephemeral governance state

- **`store.kind: file` is no longer accepted by `serve`.** A file workflow store
  persists workflows but keeps evidence/acknowledgments in memory — a
  durable/ephemeral split that silently lost governance state on restart. `serve`
  now refuses it and points at `store.kind: sqlite` (the only durable governance
  backend; it carries workflows, evidence, and acks in one DB file).
  `gateway.allow_ephemeral` still overrides for dev/testing. The evidence/ack
  store builders also fail fast on an unknown `store.kind` instead of silently
  returning in-memory.

### Changed — audit sink `stdout` → `stderr` (BREAKING: `audit.sink` value + type name)

- **The console audit sink is now named for what it actually does.** It has always written to **stderr** (stdout is the structured channel for the `serve` stdio MCP transport and the one-shot `command` / `query` driver's JSON response), but the config value and Rust type both said `stdout`. The config value is now `audit.sink: stderr`, the exported type is `StderrAuditSink`, and `stderr` is the default when `audit.sink` is unset. Loading `audit.sink: stdout` now fails fast: `unknown audit sink 'stdout' — valid values are: stderr, memory, file, none`.

### Migration

- Replace `store: { kind: postgres, url: … }` with `store: { kind: sqlite, path: … }`
  (durable, bundled, single file — and the only backend with durable evidence/acks).
- For a `serve` deployment on `store.kind: file`, switch to `sqlite`, or set
  `gateway.allow_ephemeral: true` if ephemeral governance state is acceptable.
- In any gateway config, replace `audit.sink: stdout` with `audit.sink: stderr`.
  Configs that omit `audit.sink` are unaffected (the default moved with it).

## [0.0.12] — 2026-06-15 — async `kind: workflow` + dogfooding engine hardening (P1–P5)

### Added — Async `kind: workflow` + dogfooding engine hardening (P1–P5)

From dogfooding praxec against the Simuli build:

- **Async `kind: workflow`** — a non-terminal child durably *suspends* the parent
  transition (a recorded dependency any stateless worker reconstitutes via the
  `save_if_version` CAS) instead of poll-blocking a worker; the parent is
  re-driven when the child reaches terminal via any path (completion / timeout /
  cancellation) and recovered on restart. The blocking poll loop is gone.
- **Durable governance stores** — sqlite-backed `EvidenceStore` + acknowledgment
  stores (guidance + script), wired from config.
- **Driver CLI** — `praxec command` / `query`: one governed contract call
  per process against the config store, persisting across invocations via sqlite.
  Built on a shared `build_oneshot_server` extracted from the serve path.
- **Reliability** — an idle-timeout primitive bounding MCP connect + every tool
  call; atomic `run_id` uniqueness in `create()` (closes a TOCTOU); MCP tool-name
  sanitization for provider patterns; a tool-result size bound to protect model
  context; mission-event streaming + `agent.invoked` / `agent.completed` audit
  events.

## [0.0.11] — 2026-06-12 — testing strategy + production-readiness pass

### Fixed — production-readiness pass (2026-06-12: fail-fast over fail-open)

A spine-wide FMECA sweep replacing agentic shortcuts and silent fallbacks with
fail-fast behaviour:

- **Stop reporting failures as success** — agents, discovery, and embeddings no
  longer swallow errors into a success result; the parallel aggregator propagates
  execution errors instead of flattening them to a failed verdict.
- **Durable spine** — fail-fast durable suspend + closed governance fail-opens;
  `WorkflowStore` run-id / lock methods are now required (locks + keys hardened);
  the `workflow` kind is registered in the gateway and the serve path wires
  ack / pending / backfill.
- **Honest gates** — no silent HITL auto-approve (answerable inbox, honest
  doctor, dead UI trimmed); a real `GET /models` preflight auth probe, so a
  revoked key blocks startup instead of failing mid-run.
- **Validation** — cap lifecycle required; fail-fast on malformed mappings,
  unresolved paths, and nested untrusted execution.
- **Lifecycle** — two confirmed resource leaks plugged + graceful MCP close.

### Testing

- **Full-scope integration suite** (`docs/testing-strategy.md`) — contract tests
  (C1–C5: response ↔ schema, the cockpit's §32 mirror, the rmcp
  `Tool → ToolDefinition` map), guard grids (G1 status-derivation, G2 `expr`
  evaluation, G5 outcome aggregation), live E2E (E1/E2: the real binary drives
  `hello-flow` over stdio; a durable headless lifecycle survives a restart), and
  seam tests (S1/S4/S5). The contract layer caught real schema drift and two live
  gaps. Plus the Simuli dogfood "spine" tests that run a simulation end-to-end
  *through* a Praxec workflow.

## [0.0.10] — 2026-06-12 — missions, outcomes & the interaction bus

### Added — Missions, outcomes & resolution status (ADR-0008)

- **Typed mission vocabulary** — validated `outcomes:` + a terminal `outcome`;
  typed mission status (`running | waiting | succeeded | failed(reason)`) folded
  from the live outcomes.
- **Cockpit surfacing** — a mission status badge + an outcomes checklist; launch
  a workflow from Build mode and a real fleet map builds from the launch roster.

### Added — Interaction bus, orchestrator & mediator (ADR-0009)

- **Execution vs interaction layering** — the **orchestrator** drives missions
  headless; the **mediator** mediates the cockpit; the **bus** is tokio channels
  with oneshot HITL park/resume.
- **Headless driver** — `RuntimeMissionGateway` drives a mission to terminal with
  no UI; a rig-backed `AgentChooser` + §32 `MissionState` parsing; a headless
  `orchestrate` CLI (`--definition` starts then drives); a policy bus consumer for
  HITL when no human is present.
- **Cross-mission mediator inbox** — a Needs-You inbox surfaced in the cockpit
  header.

### Changed — repo layout `orchestrators/` → `flows/` (BREAKING, ADR-0009)

- **The orchestrator tier is renamed `flows/`.** ADR-0009 separates *execution*
  (the orchestrator drives) from *interaction* (the mediator mediates), and the
  authored tier is renamed across repo layout, specs, docs, fixtures, and tests;
  `orchestrator:` bindings now surface as `flows`. `flow.*` definition ids and
  `kind: workflow` references are unchanged.

### Migration

- In any loaded repo, rename the `orchestrators/` layout directory to `flows/`.

## [0.0.9] — 2026-06-11 — agents, sandbox & authoring as first-class

### Added — Agents as first-class workflow executors (ADR-0007)

- **`kind: agent`** — agents are harness-bound workflow executors, de-feature-
  gated into the default runtime. `DiscoveryKind::Agent` + a workflow
  orchestrator route to them.
- **In-process rig runner** — `RigSessionRunner` runs an agent in-process with a
  real MCP tool loop (`McpToolHost` wires tool agents); conversation + reasoning
  wired through.
- **Untrusted branch** — `kind: agent` runs through the promotion bridge
  (ADR-0006): `DisposableCopy` + capture, an untrusted-side pipeline, and
  `run_untrusted_agent` end to end.

### Added — Execution sandbox & authored promotion (ADR-0006)

- **Two-tier trust** — agent output is a *candidate*, not a command. A
  `SandboxProvider` seam confines per-script execution; **bubblewrap**
  (`BwrapProvider`) and **OCI** (`OciProvider`) backends freeze the trait
  (validation spikes 7/7 and 4/4).
- **Fail-closed preflight** — provisioning *instructs, doesn't install*; an
  unusable sandbox blocks startup rather than silently running unconfined. cgroup
  limits via a `systemd-run` scope.
- **Coordinate-at-promotion** — free exploration in the disposable copy;
  coordination happens only at promotion.

### Added — Authoring write path (SPEC §8.4, §9)

- **`RepoDefinitionStore`** — the missing authoring write keystone, with
  audit-before-commit (§8.4), wired under `write_enabled`.
- **Safe edits** — an optimistic-concurrency hash-guard + a review diff; an
  edit-existing flow (diff executor + edit workflow); a read-definition verb and a
  Build-mode "open & edit" gesture.
- **Import / push (§9)** — import remote repos and push authored commits,
  piggybacking on the operator's own git (no praxec-managed credentials).

## [0.0.8] — 2026-06-10 — semantic discovery & guided model selection

### Added — Semantic discovery & guided model selection

- **Semantic discovery** — a rig-backed embedder + `SemanticDiscoveryIndex`
  add-on, activated in the serve path when an embedding model is registered;
  an opt-in / skippable bootstrap gate (no per-boot embedding calls when off,
  which matters most for one-shot `command` / `query`).
- **Guided model selection** — model catalogs moved out of Rust into data files
  behind a shared `core::catalog` loader; a recommendation engine ranks on
  MTEB-Retrieval score + cost magnitude; a **Priorities stance** lens tailors
  recommendations to the operator's intent; chat-model + embedding catalogs carry
  sourced, dated numbers; reasoning-effort selection. Model selection centralizes
  on `Affinity`, with a step's `needs:` driving full `affinity_fit` ranking, and
  tuning knobs (reasoning budgets, cost buckets) moved from magic numbers to
  config.
- **aether-llm → rig migration** — the cockpit chat loop and the governed
  `kind: llm` executor both move to **rig**; aether-llm leaves the governed
  runtime (its only remaining use relocated to the TUI's reasoning map).

## [0.0.7] — 2026-06-09 — Mission Control cockpit

The repositioning from "an MCP gateway" to a **deterministic execution harness
with a cockpit**: missions are driven by a headless orchestrator and observed /
steered through a two-mode control plane (ADR-0001…0005).

### Added — Mission Control cockpit (ADR-0001…0005)

- **Control-plane repositioning (P0…P1)** — praxec leads as a *control plane,
  not an agent*. New `praxec-cockpit` crate: a two-mode ratatui shell —
  **Build** browses/authors the layered library, **Run** observes and steers
  live missions (tree-view home, per-kind spinners, status chips, a Needs-You
  sidebar, plain-language UX).
- **The semantic map (ADR-0004)** — a Fleet⇄Mission zoom UI with
  container-transform easing and altitude dispatch: an L0 fleet tile-grid with
  health + pins, an L1 real mission view, breadcrumb + zoom-ladder chrome.
- **Conversational cockpit (ADR-0005)** — a chat-centric layout over a typed
  operation surface (`CockpitOp` + `App::apply`). The ops are exposed both as
  LLM tools and as an MCP face (new `praxec-cockpit-mcp` crate), so an
  in-process Mission Control driver — or an external agent — can steer the
  cockpit. Replies stream token-by-token.
- **Live gateway** — `StdioGateway` runs the real gateway as a thin MCP-stdio
  client; `--workflow <id>` shows a real HATEOAS mission and submits real
  transitions (read → act), replacing the demo tree.
- **Typed HITL panel** — a master-detail "your move, by kind" inbox with an
  inline scoped chat to reply / discuss before acting.
- **Headless-first architecture (ADR-0001/0002/0003)** — the runtime is
  headless; UIs attach as governed clients over a curated view-state model.

### Added — Global repo locks + durable suspend/resume

- **`RepoLockSpace`** — an atomic, TTL-bounded global lock primitive; a repo-lock
  gate with durable suspend/resume and surfacing, wired live into the serving
  runtime. Contention is first-class: queue + auto-resume, not fail.

## [0.0.6] — 2026-06-06 — open-core unwind + unified provider catalog

The agentic runtime comes back into the public repo (open-core unwound), and the
provider surface becomes one typed catalog. Breaking `agents.yaml` provider slugs
— see migration.

### Added — Open-core unwind (agents + TUI in-repo)

- **Vendored crates** — `praxec-agents` and `praxec-tui` now ship in
  the workspace (behind the `agents` feature); the open-core / paid boundary is
  removed, and CI guards lean and agents-only builds for feature independence.
- **Shared exclusive binding** — a typed `Delegate` affinity with one shared
  XOR-binding validator across the `kind: llm` doctor and the agents path.

### Changed — unified provider catalog (BREAKING: `agents.yaml` provider slugs)

- **Single source of truth for LLM providers** — `crates/praxec-core/src/providers.rs` introduces a typed `ProviderId` catalog (`anthropic, openai, gemini, openrouter, ollama, llamacpp, bedrock`) that the `kind: llm` factory, `set-provider-keys`, and the `agents.yaml` resolver all project from via exhaustive `match`. Adding or removing a provider is now a compile error until every surface agrees, replacing three hand-maintained lists that had drifted. Canonical slugs equal aether-llm's `ModelProviderParser` tokens; a boundary test (`crates/praxec-tui/tests/provider_catalog_aether_seam.rs`) guards the seam against future drift.
- **`agents.yaml` `provider:` is now `Known(<slug>) | custom { endpoint }`** — `provider: { name: <slug> }` for a catalog member, or `provider: { name: custom, endpoint: <url> }` for any OpenAI-compatible endpoint.

### Fixed — agent-path provider routing (BREAKING slug renames)

- **`google` → `gemini`** — the agent path emitted `google:<model>`, but aether-llm's parser only registers the `gemini` token, so Gemini `delegate:` sub-agents failed at spawn. The canonical slug is now `gemini` (the API-key var `GEMINI_API_KEY` is unchanged). `provider: { name: google }` now fails to load with a helpful error.
- **`lmstudio` retired → `custom`** — `lmstudio` was never an aether parser token. LM Studio (and any OpenAI-compatible local server) is now reached via `provider: { name: custom, endpoint: "http://localhost:1234/v1" }`. `provider: { name: lmstudio }` now fails to load.
- **`openrouter` and `llamacpp` added to the `agents.yaml` resolver** — both were already wired in the `kind: llm` factory but couldn't be named as agent-path providers. `openrouter` is now a first-class, key-managed provider (`OPENROUTER_API_KEY`) across `set-provider-keys`, preflight, and the resolver.

### Migration

- In `agents.yaml`, replace `provider: { name: google }` with `provider: { name: gemini }`.
- Replace `provider: { name: lmstudio }` with `provider: { name: custom, endpoint: "<your LM Studio /v1 URL>" }`.

## [0.0.5] — 2026-05-29 — in-runtime LLM executor (SPEC §33)

Ships SPEC §33 — the in-runtime LLM executor — and repositions praxec as a governed LLM orchestration runtime that also exposes an MCP surface. The MCP-server framing remains accurate; the LLM-runtime piece is this release's addition. See the updated SPEC §33 prose (status flipped from DRAFT) and §33.11 for the implementation deviation from the original design.

### Added — `praxec-llm-executor` crate (SPEC §33)

- **New `executor: { kind: llm, ... }`** config shape. A workflow state's available transitions become the model's tool list at the current turn; the model picks exactly one and the runtime advances. The tool surface is closed by design — `tools:` is rejected at config parse via `deny_unknown_fields` (FMECA F3); the model can't see anything beyond the workflow's declared transitions.
- **`LlmExecutorConfig`** fields: `model` / `affinity`, `prompt_template`, `max_iterations` (default 3), `max_seconds`, `max_tokens`, `max_cost_usd`, `reasoning_effort`, `capture_reasoning`. Each enforced at the layer named in the FMECA pass — see `site/src/content/docs/reference/executors.mdx` for the operator-facing table.
- **`llm.invocation` audit event** — one per turn regardless of outcome. Payload contract: `event_type`, `workflow_id`, `state`, `model`, `tokens_in`, `tokens_out`, `tokens_reasoning`, `latency_ms`, `cost_usd`, `usage_present`, `stop_reason`, `tool_call_emitted`, `error_code`, `reasoning` (or sentinel `"<elided>"` when `capture_reasoning: false`), `correlation_id`.
- **Cost catalog with freshness gates** — `crates/praxec-llm-executor/src/cost.rs` maps `provider:model` strings to USD-per-token rates with a `verified_at` ISO date. Doctor rejects workflow load with `COST_CATALOG_MISSING_ENTRY` / `COST_CATALOG_STALE` (older than 90 days) when `max_cost_usd` is set — silent budget-cap bypass is closed (FMECA F8).
- **Synthetic `_llm.*` blackboard slots** with reserved-prefix enforcement. The executor writes `_llm.cumulative_tokens`, `_llm.cumulative_cost_usd`, `_llm.cumulative_iterations`, `_llm.consecutive_no_tool_call`, and `_llm.session.<state>.started_at`; workflow-author blackboard slots whose names start with `_llm.` fail at load so the synthetic namespace can't be forged.
- **`examples/issue_triager.yaml`** — reference workflow with three LLM-driven triage transitions (bug / feature / noise) demonstrating the executor end-to-end against the mock provider.

### Added — `praxec-plan` supporting crate (SPEC §33 Phase A)

- **CPM planner** ported from cctx with file-locking discipline (concurrent acquire safe, lock-fault tolerant). `BasicCpmPlanner` implements the new `Planner` trait.
- **MCP server façade** exposes the planner so external orchestrators can drive it via the same `praxec.query` / `praxec.command` shape used everywhere else.
- **`plan_basic` example** demonstrates end-to-end planner usage.

### Added — Runtime turn-chaining for `kind: llm` (SPEC §33 D3)

- **`RuntimeTransitionResolver`** in `praxec-core` — chains LLM-driven turns by feeding each `ExecuteResult.next_transition` back through the runtime's `submit_once()` dispatch path, so every transition the model picks travels the same code path as every other transition (full guard run, blackboard update, audit fire, version bump). See SPEC §33.11 for why the loop lives in the runtime rather than the executor.
- **`max_chained_llm_turns`** runtime cap — surfaces `LLM_CHAIN_DEPTH_EXCEEDED` if a workflow's LLM-driven chain doesn't terminate within the configured budget.
- **`dispatch_once()`** extracted from `submit()` so the chain loop has a re-entrant hook without breaking the existing single-turn invariant.

### Added — Core port surfaces (SPEC §33 D2 + D1)

- **`TransitionResolver` + `Planner` traits** in `praxec-core` — the new abstraction the LLM executor + runtime chain loop sit behind. Both are extension points for future executor backends (rig, custom in-process providers) without touching the runtime.
- **`NextTransition`** type in `ExecuteResult` — the per-turn handoff the executor returns; the runtime applies it as a normal transition. Adopted by all existing executor implementations (with `next_transition: None` carry through test sites).
- **`LlmErrorCode`** enum — typed error codes (`LLM_NO_TOOL_CALL`, `LLM_MULTI_TOOL_CALL`, `LLM_EXECUTION_EXHAUSTED`, `LLM_EXECUTOR_FORBIDDEN_TOOLS`, `LLM_USAGE_MISSING_FOR_BUDGET`, `LLM_CHAIN_DEPTH_EXCEEDED`, etc.) used in audit `error_code` field and surfaced through `ExecutorError::Llm`.
- **`agent_resolver` now lives in core** — the `agents.yaml` affinity / tier resolver lives in `crates/praxec-core/src/agent_resolver/` so the LLM executor (a future D9.x integration) can reuse it.

### Locked design decisions (SPEC §33.10)

- **Streaming output** — final-only. The runtime captures full output + chosen tool call into `llm.invocation`; per-token streaming was rejected (no operator-attached display in the runtime process).
- **Reasoning capture** — captured into audit by default; `capture_reasoning: false` opts out per workflow (sentinel `"<elided>"` keeps the elision visible in the audit log).
- **Multi-tool-call turns** — rejected with `LLM_MULTI_TOOL_CALL`. The dispatch contract is one tool call per turn so guards, audit, and version bumps stay one-to-one with transitions.
- **MCP-from-inside-executor** — closed by design. The executor cannot inject `praxec.*` tools; operators who want the LLM to see praxec's MCP surface use the external-agent path (§32).

### Documentation

- SPEC §33 status flipped from DRAFT to shipped; new §33.11 documents the runtime-drives-the-loop deviation from §33.2.
- `site/src/content/docs/reference/executors.mdx` lists `llm` and carries the full operator reference (config schema, audit event, FMECA mitigations, caps + reserved-prefix enforcement, cost catalog freshness gate).
- README repositioned: tagline now leads with "governed LLM orchestration runtime"; the opening paragraph names both surfaces (MCP server + in-runtime LLM executor) without dropping the MCP framing.

### Added — `clippy::unwrap_used` enforced on production code

- **Per-crate lint** via `#![cfg_attr(not(test), warn(clippy::unwrap_used))]` in `praxec-core`, `praxec-mcp-server`, and `praxec-executors` lib roots. `cfg(not(test))` keeps the lint off when `cargo test` builds (test modules use `.unwrap()` as the deliberate panic pattern); production builds enforce.
- **Audit + fix of pre-existing production unwraps**: `mapping.rs:43` (context-is-object invariant), `mapping.rs:76` (single-key map invariant), `runtime_chain.rs:716` (match-arm-1 invariant), `tools.rs` ×3 (json!()-literal-is-object invariant), `doctor.rs:320` (user_present-checked invariant). Each became `.expect("invariant: ...")` naming the proof.
- **`praxec-schema`** skipped — typify-generated `include!()` blocks contain unwraps we can't refactor; the existing `#![allow(clippy::all)]` covers them. The deferred-comment in workspace `[workspace.lints.clippy]` is replaced with a pointer to the per-crate directive.

### Added — Active timeout watchdog + activated timeout test

- **`WorkflowRuntime::spawn_timeout_watchdog`** — when a workflow definition declares `timeoutMs`, `start()` now spawns a tokio task that sleeps the timeout, then calls `get()` once. The internal call triggers the existing lazy timeout check; the workflow transitions to `onTimeout.target` and emits `workflow.timed_out` without needing any external caller to poke it. Fire-and-forget: handle detached, self-cleans when the task returns. Lost watchdogs across process restarts are still covered by the existing lazy check on next get/submit.
- **Activated previously-ignored test**: `tests/workflow_failure_paths.rs::runtime_timeout_transitions_workflow_to_terminal` is now `#[tokio::test]` (was `#[ignore]`), starting a `timeoutMs: 50` workflow, sleeping past it, and asserting the state machine landed on `timed_out`.

### Added — `WorkflowRuntime::cancel(workflow_id, reason)` API

- **`WorkflowRuntime::cancel`** — sets `cancelled_at` + `cancelled_reason` on the instance without changing `state` (recoverable: the original position is preserved). Subsequent `get()` returns `result.status: "cancelled"` with the reason in `error.cancelled_reason`; subsequent `submit()` refuses with `WORKFLOW_CANCELLED` so retry loops don't poll forever. Idempotent — re-cancelling an already-cancelled workflow returns Ok without re-emitting the audit event.
- **`workflow.cancelled` audit event** — emitted on first cancel, carrying the reason + `state_at_cancel` + `version_at_cancel` so the audit trail records exactly where the workflow stopped.
- **`WorkflowInstance.cancelled_at` + `cancelled_reason` fields** — new optional persisted fields (`#[serde(default, skip_serializing_if = "Option::is_none")]`), so existing store rows continue to deserialize.
- **Activated previously-ignored test**: `tests/workflow_failure_paths.rs::cancellation_mid_walk_leaves_recoverable_state` is now `#[tokio::test]` (was `#[ignore]`), exercising cancel + get + submit + re-cancel-idempotence in one walk.

### Added — Sibling: praxec-meta capability-harness scaffolding

- The sibling [`praxec-meta`](https://github.com/praxec/praxec-meta) repo now ships `cap.verify.capability-harness` + a starter `contracts/` directory (reasoning / coding / prose). `flow.configure-models` gained an optional `capability_contract` input that, when set, runs the named contract against the just-written `agents.yaml`. Empty default keeps the flow's existing auto-mode path unchanged. See `praxec-meta/CHANGELOG.md` for the full diff; fixture copies under `crates/praxec-core/tests/fixtures/praxec-meta/` are synced so the meta-orchestrator e2e covers the new transitions.

## [0.0.4] — 2026-05-27 — agent resolver + production hardening

Adds the FMECA-vetted agent-resolver design: `agents.yaml` with closed-enum affinities/tiers, sparse overrides keyed by `<affinity>-<tier>`, eager auth preflight, and a guided-setup orchestrator (`meta/flow.configure-models`) in the sibling [praxec-meta](https://github.com/praxec/praxec-meta) repo.

### Added — Agent resolver (`agents.yaml`)

- **`crates/praxec-core/src/agent_resolver/`** — new module with sub-modules `config`, `classify`, `walk`, `preflight`. Loads `.praxec/agents.yaml` (project) or `~/.praxec/agents.yaml` (user); project shadows user whole-file.
- **Closed enums** — `Affinity` (`coding | reasoning | prose | web-search | recon`), `Tier` (`frontier | standard | commoditized`), `Provider` (`anthropic | openai | google | ollama | lmstudio | custom`). Enum additions are minor-version compatible per the documented policy.
- **Specificity walk** — `<affinity>-<tier>` → `<affinity>` → `<tier>` → `default`. Affinity wins tiebreaker. Opt-in `strict_specificity: true` upgrades the fall-through to a load-time error.
- **`FailureClass`** — closed enum `Auth401 | Auth403 | RateLimit429 | NotFound404 | NetworkTimeout | ContentSchema | ContentSafety | ContentOther`. Unknown response status defaults to `ContentOther` (surface, never fall through).
- **Eager auth preflight** at workflow load — every primary (index 0) binding referenced by any declared `delegate:` is auth-probed once. 401/403 is a startup error, never a runtime fall-through. `PRAXEC_SKIP_PREFLIGHT=1` escape for CI / disconnected dev.
- **Per-provider feature structs** with `#[serde(deny_unknown_fields)]` — `extended_thinking`, `reasoning_effort`, etc. Typos fail at load with the offending key named.
- **Structured `AgentResolutionExhausted`** carrying `delegate`, `walked_levels`, `attempts: Vec<AttemptRecord { binding, class, detail }>`.

### Added — Doctor checks

- **`agents.yaml`** — loads project + user files; reports binding/override counts and `strict_specificity` status.
- **`agents.yaml shadow`** — names the shadowed file when both project and user files exist.
- **`workflow delegates`** — runs each `delegate:` state through `resolver.walk()` and reports the specificity level chosen (names every delegate whose only match is a less-specific fallback).

### Added — `meta/flow.configure-models` orchestrator (in [praxec-meta](https://github.com/praxec/praxec-meta))

- Five caps: `cap.research.model-inventory`, `cap.plan.suggest-bindings`, `cap.gate.human-approve-plan`, `cap.implement.write-agents-config`, `cap.verify.auth-only-smoke-test`.
- One orchestrator wiring them: inventory → plan → approve (`mode: auto` or `review_plan`) → atomic write + round-trip → 1-token smoke per binding.
- Smoke-test output names its limitation explicitly: **auth verified, capability not tested**. v0.4 roadmap replaces it with a capability harness.
- E2E walked-to-terminal test in `crates/praxec-executors/tests/meta_orchestrators_e2e.rs::meta_flow_configure_models_walks_to_terminal_in_auto_mode`.

### Documentation

- **`site/src/content/docs/guides/agent-config.mdx`** — migration story, closed-enum reference, strict-mode discipline, `flow.configure-models` walkthrough.

### Production hardening (post-audit 2026-05-27)

The 2026-05-27 four-agent production-readiness audit flagged eleven items; ten landed in this release and one is documented as honestly deferred.

- **HTTP `connect_timeout(10s)`** added to both `reqwest::Client::builder()` sites in the workspace (`crates/praxec-executors/src/rest.rs` and `crates/praxec-core/src/config.rs`). The pre-existing total timeouts (120s + 30s) stand; the new connect_timeout guards against hung DNS / TCP handshakes that the total timeout couldn't catch.
- **Lock-poisoning signal preserved.** 33 `RwLock`/`Mutex` `.unwrap()` sites in `crates/praxec-core/src/` converted to `.expect("LOCK_POISONED: <holder>")` so a poisoned-panic message names the originating subsystem (`workflow store`, `audit event buffer`, `sqlite connection`, etc.). The no-I/O-under-lock invariant is documented at the top of `crates/praxec-core/src/lib.rs`. The workspace `clippy::unwrap_used` lint was deferred — too many pre-existing `Option`/`Result` unwraps to enable cleanly in this commit; targeted for v0.4.
- **Workflow failure-path tests.** New `crates/praxec-executors/tests/workflow_failure_paths.rs`: 2 active tests confirm permanent executor failures don't silently report `status="completed"` and guard rejection blocks advance via `submit()`. 2 `#[ignore]`'d honest stubs name v0.4 gaps — the runtime timeout is lazy-poll not watchdog, and there's no cancellation API yet. Each stub body shows the test shape for when the API lands.
- **ScriptExecutor integration tests for the three meta scripts.** The orchestrator e2e bypasses scripts via the `CapShortCircuit` fixture; new `meta_scripts_integration.rs` exercises `fetch.provider-model-inventory`, `install.agents-config`, and `verify.auth-only-smoke-test` against `std::net::TcpListener`-backed mock providers. The atomic-rollback contract on the write script is now test-pinned. Companion `*_BASE_URL` env-var overrides shipped in praxec-meta — also useful as a corporate-proxy escape hatch.
- **Doctor reference page.** New `/reference/doctor/` site page documenting all 9 checks the binary runs, their failure codes, and the operator action for each. Until now only 3 of 9 were documented.
- **Nightly CI workflow.** New `.github/workflows/nightly.yml` runs `cargo test --workspace -- --include-ignored` + `examples/smoke-ete/walk-live.sh` against real provider credentials at 04:00 UTC daily. Auto-files a labeled GitHub issue on failure so live-path regressions surface within 24h. Required secrets (`ANTHROPIC_API_KEY_CI`, `OPENAI_API_KEY_CI`, `GOOGLE_API_KEY_CI`) documented in `CONTRIBUTING.md`. Fork-gated so PRs from forks don't accidentally trigger live API calls.

### Honest deferrals (v0.3.1 / v0.4 roadmap)

- Capability-quality harness replacing the auth-only smoke test (v0.4).



## [0.0.3] — 2026-05-26 — two-tier composition + multi-repo

The **two-tier composition
model** (capabilities + orchestrators) lands with the v0.6 spec, plus
multi-repo loading, a 24-verb capability cloud, a typed slot table,
contract-hash pinning, and an end-to-end acceptance suite against the
sibling [cognitive-architectures](https://github.com/praxec/cognitive-architectures)
library and the new [praxec-meta](https://github.com/praxec/praxec-meta)
self-authoring repo.

This milestone also rolled up several internal development markers
(never tagged) from the early window — see the development-markers note below.
Cumulative diff:

- The typed skills surface (SPEC §5)
- The scripts surface (SPEC §22) and the verb-taxonomy expansion
- The lexicon / ubiquitous-language primitive (SPEC §30)
- Deterministic chaining, hot-reload via SIGHUP, dynamic fan-out
- Trace/run id plumbing, evidence enrichment
- — plus the v0.6 composition headline below

### Added — Multi-repo loading (SPEC §9)

- **Repo manifest** (`praxec.repo.yaml`) declares a `namespace`,
  `version`, and `layout` of directories where capabilities,
  orchestrators, skills, scripts, and connections live. Each repo's
  loaded definitionIds are namespace-prefixed `<namespace>/<id>`
  before being merged into the gateway registry.
- **Top-level `repos:` block** on gateway configs accepts an array of
  `{ path: <dir> }` entries. Relative paths resolve against the host
  config's directory; `~/` expands to `$HOME`.
- **Top-level `overrides:` block** lists fully-qualified ids the host
  config explicitly shadows after a repo provides them. Anonymous
  shadowing — defining `<ns>/<id>` locally without listing it in
  `overrides:` — is a config-load error (V23). Stale overrides that
  don't collide are also rejected.
- **Cross-namespace references**: `kind: workflow` `definitionId:`
  references inside a repo-loaded workflow are namespace-prefixed at
  load time. Unprefixed names bind to the workflow's own namespace;
  unresolved refs fail at load (V22).
- **Load-time rules V19–V23** enforced by
  `praxec-core::repo` and `config::load_resolved_with_repos`.
  Binary's `serve` and `check` subcommands now call the multi-repo
  loader transparently.

### Added — Two-tier composition (SPEC §3, §5–§6)

- **Capability workflows** (`cap.<verb>.<name>`) declare a typed
  `snippet: { inputs, outputs }` contract. Capabilities are
  composition leaves and may NOT invoke other workflows (V10).
- **Orchestrator workflows** (`flow.<name>`) declare an `inputs:`
  block defining their entry signature. Orchestrators invoke
  capabilities via `kind: workflow` executors with `use: { inputs,
  outputs }` bindings. Orchestrators may not invoke other
  orchestrators (V11).
- **`use:` bindings** thread typed inputs from host context to the
  capability's snippet, and project declared outputs back into host
  slots at the LHS paths. Capabilities run in their own private
  blackboard (the scoping firewall); only declared outputs propagate.
- **Snippet output validation (V17)** — every projected cap output is
  schema-checked against `snippet.outputs` at runtime. A failure
  emits `cap.output.schema_violation` audit, returns the new
  `ExecutorError::SchemaViolation` variant, and leaves the host
  blackboard untouched (no partial projection).
- **Capability termination semantics (V18)** — abnormal cap
  termination emits `cap.terminated` with `error_kind` +
  `parent_correlation_id`, no partial output projection.
- **The 24-verb cloud** (`cap_verb` module) — 10 cognitive + 12
  deterministic + 2 coordination tokens (`gate`, `coordinate`).
  V6 primary-executor verb-shape check enforces per-category
  executor kinds (Cognitive→mcp/noop, Deterministic→script/mcp,
  Gate→human/ask actor, Coordinate→mcp).

### Added — Slot table + contract hash (SPEC §6.2, §7)

- **Per-orchestrator slot table** (`slot_table` module) seeded from
  the orchestrator's `inputs:` block + every state's `use:.outputs`
  declarations. Powers V13 reachability (every `use:.inputs` host
  path must resolve to a declared slot) and V14 type consistency
  (two states writing the same host slot must declare structurally
  identical schemas).
- **Contract hash** (`contract_hash` module) — sorted-key canonical
  JSON + SHA-256 over a capability's `snippet:` block, formatted as
  `sha256:<hex>`. Stability is part of the public contract; pinned
  by `tests/contract_hash_canonical.rs` so refactors that change the
  encoding surface as test failures.
- **`expects_contract_hash:` pin** on `use:` blocks. V15 fires when
  the pin doesn't match the loaded capability's hash; V16 fires when
  a `stable`-lifecycle capability is invoked without any pin.

### Added — Validation cloud V1–V23

- Rule-keyed dispatcher in `validate.rs` with one private fn per
  rule. Centralised via `validate_workflows` and called from the
  `check` subcommand.
- **Validation-rule parity scanner** (`scripts/check-validation-parity.sh`)
  enforces that every rule V1–V23 has at least one accepts test AND
  one rejects test. Wired into CI before `cargo test`.

### Added — Library content (sibling repos)

- **cognitive-architectures v0.6** — 22 capabilities + 4 lifecycle
  orchestrators (`flow.add-feature`, `flow.bugfix-from-error-log`,
  `flow.safe-refactor`, `flow.triage-issue`) covering the main
  inbound surfaces of an engineering team. Loaded by operators via
  `repos: [{ path: /repos/cognitive-architectures }]`.
- **praxec-meta v0.1** — new sibling repo shipping four
  meta-authoring orchestrators (`flow.author-capability`,
  `flow.author-flow`, `flow.optimize-capability`,
  `flow.optimize-flow`) that compose 10 meta caps including
  introspect-the-gateway primitives (`cap.research.tool-inventory`)
  + typed wrappers over `gateway.lexicon.{lookup,define}`. Adapts
  to whatever tools the operator actually has reachable rather
  than assuming a fixed stack.
- **Vendored fixtures** under `crates/praxec-core/tests/fixtures/`
  for both libraries; e2e tests walk every shipping orchestrator to
  its terminal state.

### Changed

- Binary entrypoints (`serve`, `check`) now call
  `load_resolved_with_repos` instead of `load_resolved`. Hosts with
  no `repos:` block round-trip unchanged.
- `ExecutorError::SchemaViolation(String)` variant added; classifies
  as `ErrorClass::Permanent` (never retryable). All `class()`
  dispatch sites picked up automatically.
- Config-resolve gains `expand_use_bindings` pass: walks every
  transition with a `kind: workflow` + `use:` executor; synthesises
  the transition-level `output:` mapping from `use.outputs` so the
  existing `merge_output` projection layer drives writes; embeds
  the target capability's `snippet.outputs` schema as `_snippetOutputs`
  on the executor config (no DefinitionStore lookup needed at run
  time).
- Workspace cleared of all `clippy --workspace --all-targets -- -D
  warnings` errors. CI's clippy gate now passes.

### Fixed

- WorkflowExecutor previously polled `runtime.get` indefinitely when
  a sub-workflow's start auto-chain failed (start returned
  `status: failed` but subsequent get returned
  `status: waiting_for_action`). Now detects the failed start
  response and short-circuits with `ExecutorError::Permanent` +
  `cap.terminated` audit event.

### Test surface

- **30+ new integration tests** across `multi_repo_loading`,
  `snippet_contract`, `use_binding`, `validation_rules`,
  `slot_table_rules`, `contract_hash_canonical`,
  `cap_output_violation`, `cap_terminated`,
  `scoped_capability_io_roundtrip`, `flow_orchestrators_e2e`,
  `meta_orchestrators_e2e`. Cumulative workspace test count: 826.
- New unit-test modules for `cap_verb`, `tier`, `slot_table`,
  `contract_hash`, `use_binding`, `repo`.

## Earlier development markers (never released)

The early development window (between `0.0.2` and `0.0.3`) carried several
internal version markers (`0.2.0-dev`, `0.3.0-dev`, `0.4.0-dev`) that were never
tagged. Their cumulative diff is summarized in `0.0.3` above — the typed skills
surface (§5), the scripts surface (§22), the lexicon (§30), deterministic
chaining, hot-reload, and dynamic fan-out. The detailed per-marker entries were
folded away in this reorganization; they remain in git history.

## [0.0.2] — initial gateway hardening

### Added

- CI workflow (`.github/workflows/ci.yml`) covering build, clippy, fmt,
  workspace tests, and a mechanical dogfood transcript artifact.
- `CHANGELOG.md`, `SECURITY.md`, `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`,
  `CONFIDENCE.md`, `ADOPTION.md`, `STABILITY.md` — trust-signal files.
- README transcript section ("What the model sees") demonstrating the
  HATEOAS walk through the `content-publish` example.
- Runtime actor enforcement: `workflow.submit` now rejects with
  `ACTOR_MISMATCH` when a transition is tagged `actor: "human"` and
  the submitting principal lacks the `human` role
  (`Principal::HUMAN_ROLE`). Previously the actor field was advisory —
  surfaced in link responses but not enforced at submit time. The
  executor never runs and the workflow state never advances on
  rejection; a `transition.rejected` audit event is emitted with the
  `ACTOR_MISMATCH` code.
- `Principal::is_human()` helper and `Principal::HUMAN_ROLE` constant
  (`"human"`). Embedders wiring identity per request should tag human
  principals with this role; see `docs/EMBEDDING.md`.
- `BACKLOG.md` — open invitations for graduating the Postgres store to
  Tier 2 and recruiting design-partner case studies.

### Changed

- Tagline: "framework for building governed MCP interfaces" →
  "composable MCP control layer that governs how LLMs use tools".
- README "What the model sees" walkthrough updated to describe the
  `ACTOR_MISMATCH` enforcement explicitly, plus the defense-in-depth
  layering with the `human` executor and `permission` guards.
- `s03_multi_approver_quorum` stress scenario now submits approvals
  with a human principal (`Principal::HUMAN_ROLE`), matching the
  stricter actor gate.

## [0.0.1] — 2026-05-10 — initial MCP gateway

### Added

- Initial public release.
- Five crates: `praxec-schema`, `praxec-core`,
  `praxec-executors`, `praxec-mcp-server`, `praxec`.
- Seven-tool MCP surface: `gateway.home`, `gateway.search`,
  `gateway.describe`, `workflow.start`, `workflow.get`,
  `workflow.submit`, `workflow.explain`.
- Executors: `cli`, `rest`, `mcp`, `human`, `workflow`, `noop`.
- Stores: `memory`, `sqlite`.
- Audit sinks: `stdout`, `file`, `memory`, `null`.
- YAML config schema v1.0 with JSON Schema at
  `schemas/gateway-config.schema.json`.
- Examples: `content-publish/`, `expense-approval/`, `tdd/`,
  plus `simple-proxy.yaml`, `governed-change.yaml`,
  `import-and-discovery.yaml`.
- Docs: `CONCEPTS`, `CONFIG`, `CONNECTIONS`, `DEVELOPMENT`,
  `EMBEDDING`, `GOVERNANCE`, `INVARIANTS`, `LLM-GUIDANCE`,
  `MCP-CONTROL-ARCHITECTURE`, `STRESS-TESTS`.
