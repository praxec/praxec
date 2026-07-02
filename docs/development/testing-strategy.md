# Testing Strategy — praxec

**Status:** Active plan · **Date:** 2026-06-12

The map of *what we test and how*, so the work is scoped end-to-end and nothing is
dropped halfway. Execute phase by phase; check items off here.

## 1. Philosophy

- **Narrow, single-seam integration tests at unit speed.** An integration test
  drives **one** unit against a **mock of its single counterpart**, everything else
  mocked, in-process, milliseconds. A failure points at exactly one seam.
- **Testing pyramid.** Base = unit (single unit, all collaborators mocked). Middle
  = narrow integration (one seam, two units). Apex = broad/live (the binary over
  stdio). Target **~95% at unit speed, ~5% live**.
- **Every mocked seam needs a CONTRACT test** pinning the mock to the real
  counterpart's shape (schema or recorded real response). Without it, fast tests
  give false confidence — the base of the pyramid becomes sand.
- **Data-driven via `rstest`.** Scenario *tables* compile to individual,
  individually-named, individually-failing `#[test]`s — breadth (exhaustive cases)
  *and* atomicity (one assertion / one cause per test).
- **Coverage = use-case/scenario, not line.** Enumerate **equivalence classes +
  boundary values**; take the **full grid only where dimensions interact** (truth
  tables); independent dimensions tested independently (pairwise at most).

## 2. The seam map (narrow integration points)

Each row: the two units, the contract they meet on, the mock that stands in, and
whether coverage exists. "Grids" = the data-driven scenario tables to build.

| # | Two units (seam) | Contract | Mock | Now | Grids to build |
|---|---|---|---|---|---|
| **S1** | **Cockpit ↔ Backend** | §32 response (`Gateway`) | scriptable `Gateway` | ⚠ static | launch→fleet→zoom→submit→resolve; rejected/stale/not-connected; status badge + outcomes render |
| S2 | Agent ↔ **LLM** | `ProviderFactory` `StreamEvent` | scripted stream | ✅ | text/reasoning/tool-call/usage/error/done; turn loop |
| S3 | Agent session ↔ **LLM** | `AgentSessionRunner` report | `MockSessionRunner` | ✅ | Completed/NoResult/TimedOut → `AgentResult` |
| S4 | Agent ↔ **MCP tools/external** | `ToolHost`/`McpToolCaller` | recording host | ⚠ partial | list; call success/error/structured/content; multi-turn tool loop |
| S5 | Agent ↔ model resolution | `AgentModelResolver` | stub | ⚠ partial | binding kinds; resolution failure |
| S6 | Orchestrator driver ↔ Backend | `MissionGateway` | `ScriptedGateway` | ✅ | drive outcomes (expand) |
| S7 | Orchestrator ↔ **LLM decision** | `TransitionChooser` (`final_answer.output`) | scripted | ✅ | chosen/illegal/none/no-result/timeout |
| S8 | Orchestrator ↔ Bus consumer | `Bus` / `run_headless_consumer` | real bus | ✅ | park/resume/policy/race |
| S9 | Cockpit mediator ↔ §32 | `waiting`+human links | constructed responses | ✅ | inbox membership/answer (expand) |
| S10 | Runtime ↔ Executor | `Executor`/`ExecutorRegistry` | in-memory + failing | ✅ | success/permanent/timeout/transient; chain outcomes |
| S11 | Runtime ↔ Store | `WorkflowStore` | in-memory + failing | ✅ | persistence; optimistic-version; recovery |
| S12 | Runtime ↔ Guards | `GuardEvaluator` | default + stub | ✅ | **every guard kind × pass/fail/error** |
| S13 | Runtime ↔ DefinitionStore | `DefinitionStore` | config + swappable | ✅ | lookup; snapshot; hot-reload swap |
| S14 | Runtime ↔ Evidence/Ack | `EvidenceStore`/ack stores | in-memory | ✅ | quorum; ack hash match/flip |
| S15 | Runtime ↔ Planner | `Planner` (CPM) | stub | ✅ | cohort; locks |
| S16 | MCP server ↔ Runtime | §32 handlers | in-memory runtime | ⚠ partial | start/get/submit/describe dispatch; error mapping |
| S17 | MCP transport boundary | rmcp tool-result ⇄ client | live (E1) | ✗ | serialization fidelity (covered by E1+contracts) |

## 3. Contract tests (the linchpins — do these first)

| # | Pins | How |
|---|---|---|
| **C1** | real §32 response ↔ `workflow-response.schema.json` | validate every E1 response against the schema |
| **C2** | cockpit `GatewayResponse` ↔ schema | the typed mirror deserializes a schema-valid response; every field the cockpit reads is present |
| **C3** | `ScriptedGateway` mock ↔ real shape | the mock's responses are schema-valid (so S1 can't drift) |
| **C4** | scripted `ProviderFactory` ↔ real `StreamEvent` | the mock stream uses the real event vocabulary |
| **C5** | `ToolHost` mock ↔ real rmcp `Tool`→`ToolDefinition` | the mock's tool defs match the real mapping |

## 4. Pure-logic truth-table grids (data-driven, no mocks)

These are deterministic functions whose *whole point* is exhaustive enumeration —
do the full grid.

- **G1 — status derivation:** `(hint, terminal, outcome-marker, outcomes-met,
  awaiting-human) → MissionStatus`. Full truth table.
- **G2 — guard `expr`:** `(expr, context) → bool` over operator × operand-type ×
  edge (unset slot, explicit null, type mismatch, quoted op, missing path).
- **G3 — `from_response` / cockpit parse:** field × present/absent/malformed.
- **G4 — config validation:** every V-rule + outcome rule × pass/fail.
- **G5 — outcome evaluation:** outcomes met/unmet/unset-slot → met flag + all-met.
- **G6 — mediator inbox membership:** status × actor-mix → in/out.

## 5. Broad / live E2E (the 5%)

- **E1 — live binary over stdio:** spawn `praxec serve`, drive `hello-flow`
  via `StdioGateway` end-to-end (`command{definitionId}` → submit → human gate →
  resolve), asserting status/outcomes/links. Exercises the untested `StdioGateway`
  + the real MCP boundary, and **feeds C1/C2 real responses**.
- **E2 — headless `orchestrate`:** the CLI end-to-end against `hello-flow` with a
  scripted provider (deterministic), asserting the printed `DriveOutcome`.
- **E2E #1 — durable lifecycle:** boot against an on-disk SQLite store, drive to
  `succeeded`, **restart a fresh process** against the same DB, assert recovery.
  The filesystem-all-the-way-down path (E1 is ephemeral). ✅

## 5b. Resource lifecycle & leaks → [`resource-leak-test-plan.md`](resource-leak-test-plan.md)

A separate dimension: when a mission ends / cancels / times out, do we reap every
OS child + task and avoid unbounded in-memory growth? That audit already found +
fixed two real bugs (bus abandoned-park leak, sandbox timeout orphan) and added an
MCP `close()`; the Tier-A harness (`/proc` reaping check + `CountedGuard`) and the
complete/cancel/timeout process-reaping matrix live in that plan.

## 6. Shared fixtures + tooling

- **`rstest`** dev-dependency (parameterized atomic tests).
- **Schema validation** helper for contract tests (a `jsonschema`-style check
  against `schemas/workflow-response.schema.json`).
- **Scriptable mock library** (reusable): `ScriptedGateway` (response sequence +
  recorded commands), scripted `ProviderFactory`/`AgentSessionRunner` (exist),
  recording `ToolHost`.
- **`examples/hello-flow.yaml`** — self-contained `start → review[actor:human] →
  done`, with an `outcomes` block and no LLM/external tools. Shared by S1 scripts,
  G-grids, E1, E2.
- **Golden responses** — recorded real §32 responses (from E1) as contract fixtures.

## 7. Per-seam definition of done

A seam is "done" when: (a) its narrow integration tests cover every equivalence
class + boundary (data-driven), (b) a contract test pins its mock to the real
shape, (c) error/failure paths are covered, (d) all atomic (one assertion/test).

## 8. Phased execution

- **Phase 0 — tooling & fixtures:** add `rstest`; the schema-validation helper; the
  scriptable mock library; `hello-flow.yaml`.
- **Phase 1 — contracts (C1–C5):** linchpins first, so every later mock is trusted.
  (C1/C2 may need a first E1 to record a golden response.)
- **Phase 2 — Cockpit ↔ Backend (S1):** scriptable `Gateway` + exhaustive
  cockpit-flow grid. *(Weakest current coverage — start here.)*
- **Phase 3 — truth-table grids (G1–G6):** cheap, high-signal; surfaces edges by
  forcing every cell to be named.
- **Phase 4 — orchestration seams (S6–S9):** expand existing to full grids.
- **Phase 5 — LLM/tool seams (S2–S5):** complete S4/S5; grid S2/S3.
- **Phase 6 — runtime internals (S10–S15):** complete + grid-ify (much exists).
- **Phase 7 — MCP server seam (S16):** §32 dispatch + error-mapping grid.
- **Phase 8 — live E2E (E1–E2):** the 5%; record golden responses for C1/C2.

## 9. Progress

**Terminal state of the build-out: complete.** Phases 0–8 are all done/covered.
E2's live smoke exists as an `#[ignore]`-gated artifact (runs on demand with a
real provider key); CI relies on its deterministic equivalent (S6/S7/S8). New this build-out:
C1 (caught real schema drift), C2, C5 (extracted `McpToolHost` map → lib), E1
(caught 2 real bugs), G1/G2/G5, S1 seam+edges+full-flow, S5 failure path — all
green. C3/C4 are satisfied-by-construction (documented). Full workspace green
(modulo one pre-existing timing flake, `s10_workflow_level_lazy_timeout`, that
passes in isolation — unrelated to this work).


- [x] Phase 0 — tooling & fixtures (rstest, `ScriptedGateway`+`GatewayLog`, `hello-flow.yaml`)
- [x] Phase 1 — contracts (ALL closed): **C1 done** (caught + fixed real schema drift); **C2 done** (cockpit mirror ↔ schema, 11 atomic read-contracts); **C3 folded into C2** (`GatewayResponse` is a `Deserialize`-only strict subset, so the real shape conforming + deserializing is the contract; the mock is safe-by-construction subset); **C4 satisfied-by-construction** (the `mock_provider` builds the real `StreamEvent` enum directly — a typed *closed* enum, so a renamed/removed variant fails to compile; the live guard is the turn loop's exhaustive match over it, S2). **C5 done** — the rmcp `Tool` → rig `ToolDefinition` mapping was untested in the binary (`McpToolHost`); extracted to `agents::rig_runner::tool_definition_from` (added a light `rmcp` protocol-types dep), `main.rs` now calls it, and 4 contract tests pin it (`name` passthrough, description-less → `""`, present description carried, `input_schema`→`parameters`). **All of Phase 1 contracts are closed.**
- [x] Phase 2 — S1 cockpit↔backend: the §32 backend seam is covered — launch/answer/inbox-membership grids, error edges (launch rejection no-rosters, answer not-connected/rejected/out-of-range — all narrated, none panic), and the **full-flow seam sequence** (launch→auto-refresh→inbox→answer records launch+command in order). The remaining zoom→detail step is local UI *animation* (not a backend seam) — out of S1's scope; a focused render/animation test, if wanted, is separate.
- [x] Phase 3 — grids: **G1 (status), G2 (guard expr), G5 (outcome-eval aggregation) done**; G3 (from_response) + G4 (config validation) + G6 (inbox) already covered elsewhere. Grid phase complete.
- [x] Phase 4 — orchestration S6–S9: **covered** (74 tests across orchestrator driver / `Bus` park-resume / headless consumer / mediator inbox; the §32-drive seam + the oneshot-keyed interaction bus). Further grid-widening is optional polish, not a gap.
- [x] Phase 5 — LLM/tool S2–S5: S2/S3 covered; **S5 closed** (61 core resolver tests + the agent-side resolver-failure path); **S4 closed** — the list→`ToolDefinition` mapping is now C5-pinned in the lib, and the call-routing / multi-turn tool loop is covered by `rig_runner`'s RecordingHost tests. (The binary's `McpToolHost::call` result-extraction helper is the one minor sliver still in `main.rs`.)
- [x] Phase 6 — runtime S10–S15: **covered** (267 core lib tests + core integration suites — executor/store/guards/evidence/planner/definition-store; G2 + G5 added here). Further grid-ify is optional polish, not a gap.
- [x] Phase 7 — MCP server S16: **re-assessed as covered** (183 mcp-server tests; `dispatch_shape.rs` is the full routing grid — home/search/describe/describe-in-workflow/explain/get/start/submit/define + ambiguity errors; runtime-rejection→McpError mapping in `lib.rs`; structured-rejection pass-through proven live by E1's ACTOR_MISMATCH). The "⚠ partial" was conservative; no synthetic tests added.
- [x] Phase 8 — live E2E: **E1 done** (live binary drives `hello-flow` over stdio to `succeeded`; caught two real gaps — MCP boundary enforces actor roles, `from_claim` needs a subject). **E2 landed as an `#[ignore]`-gated artifact** — runs the headless `orchestrate` CLI end-to-end against a real provider (`PRAXEC_E2E_MODEL` + key, `--ignored`), asserting a `Resolved{succeeded}` DriveOutcome; CI skips it (clean skip when unset). Its deterministic equivalent is S6/S7/S8.
