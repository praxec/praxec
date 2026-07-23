# Changelog

All notable changes to **praxec** are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
on the cargo crate version. The **config schema** is versioned
separately — see [`docs/reference/stability.md`](docs/reference/stability.md) for what is and isn't
covered by a stability commitment.

## [0.0.29] — 2026-07-23 — Close the ledger: snippet inputs, async-park push, packageable schema

Closes the two open dogfood findings from the v0.0.28 program and squares away
release plumbing.

### Fixed

- **Snippet inputs join the synthesized inputSchema (finding #10).** A capability
  built from a snippet whose `inputs:` declared defaults used to lose those
  declarations — `synthesize_input_schema` only read the top-level `inputs:`
  block, so snippet-declared defaults silently vanished from the contract. The
  synthesized schema now unions both homes (top-level wins on a shared key), and
  new **V37 `INPUT_DECLARATION_CONFLICT`** rejects at load a key declared in both
  places with *conflicting* definitions rather than letting one silently shadow
  the other.
- **Elicitation push reaches async-parked ancestor gates (finding #11).** When a
  child workflow's completion parked a *parent* mission on a human gate, the MCP
  call that drove the completion returned without pushing the parent's
  elicitation — the operator only saw the gate on the next unrelated call. The
  completing call now walks the ancestor chain (visited-set + depth cap, with
  `elicitation.ancestor_walk.cycle`/`.capped` audit events) and pushes any
  newly-parked gate exactly once per gate version.

### Changed

- **`praxec-schema` packages standalone.** The crate now carries its own
  in-crate `schemas/` copies (build.rs reads `CARGO_MANIFEST_DIR`, a byte-identity
  test fences them against the canonical top-level copies), so `cargo package`
  succeeds from a clean checkout. A `package-dry-run` CI job proves it on every
  PR. Publishing to crates.io remains a **manual, deliberate step** — no
  auto-publish on tag.
- **aether-agent-cli catch-up to 0.7.27.** The TUI sub-agent `RunConfig` gains
  the new `telemetry`/`trace_context` fields (explicitly disabled), so a bare
  `cargo install --path crates/praxec` no longer needs `--locked` to build.
  `aether-llm` stays within its `~0.7.x` review fence (resolves 0.7.22).

## [0.0.28] — 2026-07-22 — HITL elicitation context: gates that carry their own decision context

### Added — HITL elicitation context: human gates that carry their own decision context

A human gate used to park a mission with whatever prompt happened to be around and
hope the operator knew why they were being asked. This branch makes the gate itself
carry everything the decision needs — the question, the evidence, and the option
set — and makes the validator prove at **load time** that it always can. The full
contract, including the migration recipe for pack authors, is in
[`docs/hitl-elicitation.md`](docs/hitl-elicitation.md).

- **Prompt-source chain (E1).** A parked gate resolves its prompt through a fixed
  chain: the transition's `prompt`/`goal`/`title` → the instance context's `prompt`
  string (the caller-seeded convention) → the enclosing state's `goal`, rendered
  through the same template renderer as state guidance. **V33
  `HUMAN_GATE_NO_PROMPT_SOURCE`** rejects at load any transition-level
  `actor: human` gate with no statically-guaranteed link — a transition prompt key,
  a state `goal`, or a required-or-defaulted string `prompt` input (whose
  input→context seeding guarantees `$.context.prompt`). The now-unreachable runtime
  fallback is not deleted but instrumented: a firing on fresh config is reported as
  a validator↔runtime parity breach.
- **`presents:` (E2).** A gate transition declares which `$.context.<key>` values
  the operator sees alongside the question — a projection, not a context dump.
  Resolution is **all-or-nothing**: a malformed pointer, an unresolvable key, or a
  projection over the byte budget defect-marks the gate (`PRESENTS_UNRESOLVED`)
  rather than showing a partial view. **V34 `INVALID_PRESENTS`** rejects at load a
  malformed declaration, a pointer to a context key nothing in the workflow can
  have written, or a `presents:` on a non-human transition (a dead declaration).
- **`choices:` (E3).** A gate transition declares a typed option set drawn from a
  live context array (`{ field, from, value, title? }`); the elicitation form
  renders it as a titled single-select enum, and the chosen value is submitted as a
  plain string argument. **V35 `INVALID_CHOICES`** checks the declaration at load
  through the same parser the runtime uses, so the validator can never accept a
  shape the runtime rejects. The **`pick` output-mapping operator** selects the
  array element the chosen key names, preserving downstream `chosen: object`
  contracts while the human answers with a string. The **`CHOICE_MISMATCH` submit
  guard** rejects any submission whose choice is not among the live options — on
  the push (elicitation resume) and pull (hand-typed) paths alike, through the same
  parser/resolver pair the gate-time projection uses, so what was offered and what
  is accepted can never disagree. That guard is also what makes `pick`'s no-match
  Null unreachable for governed gate submits.
- **V36 `ELICITATION_INCOMPATIBLE_GATE` (Warning).** The "Accept can never
  succeed" smell: a human gate whose `inputSchema` requires a non-primitive
  property no elicitation form can collect — including the partially-doomed shape
  where `choices:` is declared but the schema *also* requires a non-primitive
  beyond the choice field. A Warning rather than an Error because pull-only object
  gates (resolved via CLI/approvals with full JSON arguments) are legitimate.
- **FormPlan push/skip fence.** Form construction now makes an explicit push/skip
  decision: a defect-marked gate, or a declared schema whose required fields no
  elicitation answer could satisfy, is **never pushed** as a form whose Accept is
  doomed — the mission stays parked with its pull handle and the reason. Presented
  context renders as labeled blocks under a per-value budget; an over-budget value
  is truncated with a self-announcing marker naming where the full value lives
  (`pending_human.presented`), never silently clipped.
- **First elicitation round-trip test coverage.** The push → answer → governed
  submit → advance loop is exercised end-to-end in tests rather than assumed.
- **`drop_prompt_source` mutation operator.** Deletes every prompt-source link of
  a human gate at once — transition keys, state `goal`, and the `prompt` input
  declaration — and the harness asserts V33 kills the mutant. V33's guarantee is
  measured, not assumed.

### Fixed — failure-path hardening from the same dogfood run (#95)

The program that built this release was executed *through* praxec + CPM, and its
failure telemetry closed three engine defects along the way:

- **`AGENT_CHAIN_EXHAUSTED` terminal classification.** A model chain that
  exhausts its wall across attempts no longer surfaces as the last attempt's
  clamped `timeout after Nms` — the typed terminal error leads with the walk
  summary: every model tried, each attempt's failure class and duration, wall
  consumed vs budget. Single-model chains keep genuine timeout semantics.
- **`agent.model_attempt` audit telemetry.** One event per finished model
  attempt, stamped with the child workflow id + correlation, so a failed run's
  audit stream shows *which* models were tried and how each ended — provider
  outage and hang-prone lead are now distinguishable from the operator's seat.
- **Cancelled children leave their parent recoverable.** `_subworkflow_wait`
  was only cleared on the success path, so a cancelled child was resurrected on
  every re-fire and the parent was permanently stuck. The dead wait is now
  consumed (version-checked, audited) and the next submit spawns a fresh child.
- **`purpose: ask` channels are not approval gates.** The SPEC §29
  `enable_human_ask` injected self-loops are exempt from V33 (their prompt is
  the agent's question, carried by the required AgentAwait marker) and are no
  longer surfaced as promptless pseudo-approvals in the pending list — both
  sides of the validator↔runtime parity fence, with fence tests.

## [0.0.27] — 2026-07-20 — dogfood hardening: three load-time poka-yokes

The follow-up release to v0.0.26's browser E2E substrate. Running that substrate
against real applications surfaced defects in the engine as well as the workflow
pack; this is the engine half. Every change here moves a failure **earlier** — from
mid-run to load time, or from one-at-a-time to all-at-once — and none of them
changes what is legal, so upgrading is behaviour-preserving for a config that was
already correct.

One correction worth recording, because it was the campaign's own headline finding:
the multi-model stall was attributed to unplumbed reasoning bounds. That diagnosis
was **wrong**. Reasoning effort has been plumbed end-to-end and capped at `"low"` by
default since v0.0.15 (`ReasoningTuning.default_effort`, pinned by a test), so the
stall has **no confirmed root cause** and should not be considered resolved by this
release. The per-state knob below is a real ergonomic gap that the campaign exposed —
it is not the stall fix.

### Added — per-state `reasoning_effort:`

- **Per-state `reasoning_effort:`** completes the per-state trio with `affinity:` and
  `tools:`. A state that is the hardest reasoning step of a loop (a diagnosis leaf)
  can now raise its own thinking budget from its capability YAML, without a core
  change or a gateway-wide default bump. Precedence: **state declaration** >
  `$.context`/`$.input.effort_override` > the configured `ReasoningTuning
  .default_effort` (which ships as `"low"`). An absent key is bit-identical to
  before, so this is purely additive.
- The effective effort is recorded on the `agent.invoked` audit event, the same
  parity rule that already applies to the effective `tools:` set — an audit of a
  raised-effort step can never report the gateway default.
- **V32 `UNKNOWN_REASONING_EFFORT`** — a load-time poka-yoke. The `ReasoningTuning`
  accessors deliberately fall back to `medium` for an unrecognized level, so an
  unvalidated typo (`xhig`) would silently become a no-op cap. The level vocabulary
  is **derived from the `tuning.reasoning` maps**, never hard-coded in Rust, so an
  operator who adds a level gets it accepted with no code change. The runtime rejects
  it too (`AUTO_DRIVE_STATE_REASONING_EFFORT_INVALID`), but V32 moves the failure to
  `praxec check` — before the human gate and before any lease is taken.

### Fixed — an unresolvable script reference no longer passes `praxec check`

- **`SCRIPT_SUBJECT_UNKNOWN`** — a `kind: script` executor naming a subject that no
  `scripts:` entry defines is an authoring typo, but `stamp_scripts_library` skipped
  it silently. `praxec check` reported `validation: ok` and the run failed much later
  with `SCRIPT_NOT_IN_SNAPSHOT` — an error whose own text blames collection and points
  straight back at the skip. The gate every workflow pack relies on had a blind spot
  on exactly the reference it exists to check. Found while dogfooding the browser-E2E
  QA family: a typo'd script subject validated green, and the only way to prove a new
  script loaded at all was to corrupt its YAML.
- Same poka-yoke class as V32 — a reference that resolves to nothing must fail at
  load, before the human gate and before any lease is taken. All unresolvable
  references across all workflows are reported in one error (matching the writable-
  repo-root change below), naming the workflow, the subject, and the declared
  subjects so the typo is obvious.

### Changed

- Boot now reports **every** unresolvable `writable: true` repo root in one error
  instead of short-circuiting on the first, so an operator fixes them in a single
  pass rather than one reboot per broken path. The fail-fast itself is unchanged: a
  run's root must be real, not a hopeful string.

## [0.0.26] — 2026-07-20 — browser E2E substrate: collision-free parallel runs across a leased pool

Enables running many browser-automation checks **in parallel across many projects
without collisions** — an architectural guarantee, with **zero browser knowledge in
the engine** (it enforces "exclusive resources must be leased"; config declares which
connections are exclusive via `exclusive: true` + `pool:`). A browser MCP server has
one global page pointer per process, so two runs sharing a connection silently corrupt
each other and any reproduction measurement taken through them; the run-scoped pool
lease gives each run its own server process. Proven live against a real app (simuli):
`flow.qa.explore` drove the browser end-to-end, with the tool-surface role split
(charter=file-only, survey/probe=leased-browser) observed in the audit.

### Added — run-scoped exclusive-pool lease

- A connection may declare `exclusive: true` + `pool: <name>`. A flow declares
  `exclusive_pools: [<name>]`; the runtime leases ONE member at the run boundary
  (`acquire_any`), binds it to the run-ambient `$.run.leased.<pool>` (which survives
  sub-workflow spawn), and releases on terminal/cancel. A browser state reaches it via
  `tools: ["{{ $.run.leased.browser }}"]`. Two concurrent runs get DISTINCT members;
  an exhausted pool fails fast (`POOL_EXHAUSTED`), never shares. The gateway derives
  the pool map from `connections:` (and on reload).
- **V31 `UNLEASED_EXCLUSIVE_ACCESS`** — a load-time poka-yoke: an exclusive resource
  may be reached ONLY through its lease. A direct `kind: mcp` connection, a literal
  exclusive connection in `tools:`, or a leased reference without the pool declared
  are all rejected.
- **`CYCLE_DETECTED`** — the `kind: workflow` reference graph must be acyclic (escape
  hatch: `recursive: true`, mirroring `while:`→`max_iterations`). This demotes the
  runtime depth guard to defense-in-depth.
- **Per-state `tools:`** — a state may replace the gateway-wide auto-drive tool set,
  scoping an agent leaf's reach (e.g. a fixer to `file:.../src`, a test-author to
  `file:.../tests`). Absent → the global set, unchanged.
- **`file-ro:<root>`** — a genuinely read-only file root (read tools only; a mutating
  call is rejected, not merely unexposed) for reviewer-style roles.
- **`$.run.artifacts_dir`** — a run-scoped evidence dir, engine-created at the run
  boundary so a probe/screenshot write never fails on a missing parent.
- **`run_ref`** — an engine-minted, always-present run-tree identity (the pool-lease
  holder key), separate from the optional caller-supplied `run_id` correlation, so the
  SPEC §20.2 audit contract is preserved.
- The pool lease emits `lock.acquired` / `lock.released` audit events.

### Fixed

- **`owned_files` was inert on the auto-drive chain path** — the lock gate existed only
  on the submit path, so an auto-driven agent leaf declaring `owned_files` took no lock.
  Hoisted to a shared acquire/release + `park_on_lock` + `ChainOutcome::WaitingOnLock`.
- **Symlink write-escape** — the file-tool root guard was purely lexical, so a symlink
  inside a root pointing outside it escaped confinement (a live hole). Now
  filesystem-aware (canonicalise + `symlink_metadata`), enforcing containment.
- **Sub-workflow recursion** now drives each child on a fresh `tokio::task`, so a cyclic
  graph fails fast at the depth guard instead of a debug-build stack abort.
- **`acquire_any` N>1 starvation** — the scheduler's readiness predicate was all-of;
  pool waiters now carry an any-of `AcquireMode` (the bug was invisible at pool size 1).

## [0.0.25] — 2026-07-18 — multi-provider load distribution: route across a credentialed pool

The headline release for the requirement-driven resolution work: an LLM step no
longer names one model and one provider. It declares what it *needs* — an affinity
(domain), an optional tier and thinking-effort — and the runtime resolves that to a
**pool** of `(provider, model, account)` members, then routes the turn across them
with health-aware failover or weighted distribution. When one provider throttles,
the turn fails over to the next member behind its own circuit breaker; secrets never
leave the environment. Built on the reliability primitives in the sibling
`execution-policy` crate (v0.0.6) so the governance spine (caps / audit / typed
failure) is unchanged.

### Added — requirement-driven pool resolution (spec #2)

A `kind: llm` (or agent) config can set `affinity:` (plus optional tier / `effort`)
instead of a literal `model:`. The resolver walks the configured `models.yaml`
first, then falls back to ranking the model catalog by value (fit / costᵝ within an
ε band), and expands each fit to concrete `(provider, model, account)` members.
Effort is two-faced — a capability *filter* (a member must support the requested
reasoning level) and an applied *knob* (it rides in `turn.reasoning`). An
unsatisfiable requirement fails loud (`ResolutionError::Unsatisfiable`), never a
silent default.

### Added — `strategy:` routing over the pool (the execute-trigger)

Setting `strategy:` on the step turns on pool routing at the streaming step:

- `ordered` — health-aware failover: try members in rank order, advancing to the
  next only on a *classified transient* error (throttle / timeout); auth / author
  bugs fail fast without burning the pool.
- `distribute` — weighted least-in-flight over the value band, so load spreads
  across equally-good members instead of hammering the top pick.

Routing runs through `execution-policy`'s `RouterPolicy`, over an opaque member id
(the crate stays domain-blind). The served member's model is what the audit log
records. Without `strategy:`, the single resolved model streams through the
unchanged direct path — the existing ordered agent walk is untouched.

### Added — a US OpenAI-compatible provider fleet

Fireworks plus a table-driven fleet of OpenAI-compatible providers (Together,
Baseten, DeepInfra, Groq, Cerebras, SambaNova, Hyperbolic, Parasail): adding one is
a single descriptor row (`base_url` + a typed `WireStyle`), all sharing one
completions client. The fleet rides the `rig` core path and is config-gated.

### Added — named accounts / per-account credentials

A pool member can carry a named `account`, resolving to an account-specific API-key
env var (`<PROVIDER>_API_KEY_<ACCOUNT>`, e.g. `FIREWORKS_API_KEY_WORK`) so one
provider can be driven under several credentials. Accounts are named in config;
**secrets stay in environment variables only** and are never inlined in YAML.

### Added — configurable, call-level-overridable tool-setup timeout

The MCP tool-setup phase (`host.tools()` discovery, run before the first model turn)
was bounded by a hardcoded 60s. It is now a per-step `tool_setup_seconds` knob
(default 60, clamped to the step wall): a step that talks to a slow or heavily
loaded tool server can raise it while other steps keep the default. Scoped to the
agent executor.

## [0.0.24] — 2026-07-16 — stall defense: no silent hangs, no runaway livelocks

Two liveness backstops surfaced by dogfooding, closing the last places a governed
run could burn the model indefinitely — one hanging silently, one livelocking.
Both reuse existing typed-failure / cancel machinery and add no new infrastructure.

### Fixed — an LLM leaf can no longer hang on stream establishment (FB-8)

The agent leaf's no-progress watchdog only started its clock once the model
stream object existed, so `factory.stream().await` — stream *establishment* —
was un-timed. A provider that accepts the request but never returns the first
frame (connect-but-no-token, the hang-prone-lead failure shape) was caught only
by the whole-session wall (`max_seconds`, ~600s) and burned it once per model in
the chain-walk. Establishment is now bounded by the same `stall_timeout` window
as inter-event silence and reclassified to `ExecutorError::Timeout`, so a
first-frame hang escalates to the next model in ~`stall_timeout` (~120s) instead
of the full session budget. (Same class as the previously-closed 47-minute
`host.tools()` hang; this was the remaining un-timed await in the leaf.)

### Fixed — a livelocking run is quarantined instead of burning forever (FB-9)

The deterministic chain was bounded only by `maxChainDepth` (default 50), which
counts a *single drive's* steps and resets on every submit and every restart. A
livelocking run (observed cycling states ~42 min at chain depth 27, all verdicts
false) re-armed that budget on each poll and **survived restarts** —
`reap_orphaned_runs` deliberately skips `_agent_await` instances, so the
livelocking agent loop auto-resumed straight back into the burn.

- A **cumulative** hop counter (`_chain_hops_total`) is now persisted in the
  instance context, so it survives re-drives and restarts. On reaching
  `livelockHopBudget` (default **300** — generous; legitimate flows finish in
  tens of hops, a large CPM program in the low hundreds) without a terminal
  state, the chain returns the new `ChainOutcome::Quarantined` and the run is
  **cancelled** (via the existing `cancel()` path: terminal, `workflow.cancelled`,
  wakes any suspended parent) with reason `livelock_quarantine`.
- `reap_orphaned_runs` gains a boot-time backstop that quarantines an instance
  already over budget **before** the engine-wait skip — closing the
  `_agent_await` auto-resume hole.
- Override per-definition with `livelockHopBudget`. The generous default is the
  false-positive margin: a run that terminates under budget is never touched
  (pinned by test).

## [0.0.23] — 2026-07-15 — dogfood ergonomics: writable code targets, worktree selectors, honest reload

An ergonomics + fail-loud release driven entirely by dogfooding praxec as the
implementation engine for a real external code repo (a C# checkout on a git
worktree). Every change below closes a silent-drift or foot-gun that surfaced in
one real setup+run session: `reload` that looked fine but didn't rewire the
writable set, a writable code target forced to carry a definition manifest,
worktree fan-out that forced pre-declaring every worktree, a connection reaped
mid-scan by a fixed idle timeout, declared connections invisible to discovery,
and `doctor` passing a config whose enabled feature had no model. The praise
item (fail-closed honesty on an unresolved root) was preserved, not weakened —
the theme is moving that same rejection *earlier* and making the happy paths
ergonomic.

### Fixed — `reload` now rewires the writable repo set (FB-1, dogfood find)

`writable_repo_roots` was applied to the runtime only at serve startup, so
`praxec.command { reload: true }` after declaring/changing a `writable: true`
repo had **no effect** — the operator got a bare `"reloaded"`, then a run got an
empty `$.run.repo_root` and died much later at the first file-leaf
(`FILE_TOOL_ROOT_UNRESOLVED`): a silent, deferred, mislocated failure.

- The runtime's writable set is now a hot-swappable slot (`Arc<RwLock<Vec<RepoRoot>>>`,
  matching the `Swappable*` reload idiom); `reload_gated` re-derives it from the new
  config and swaps it atomically with the definitions/executors — no restart.
- A writable repo that no longer resolves (`RepoRoot::new`: path missing) drops the
  reload to **repair-only** (`WRITABLE_REPO_INVALID`), keeping the previous set live —
  never a half-swap, exactly like a contract-dirty edit.
- The reload response now surfaces the resolved `writable_repos` (and audits them),
  so the operator sees what runs can actually write to, not a bare `"reloaded"`.
- (FB-1b) Confirmed + test-locked that `start`'s repo_root resolution already
  fails fast (`REPO_ROOT_REQUIRED`) on an empty set at every boundary — the
  earlier "empty root at start" symptom was the reload-not-rewiring bug above.
### Fixed — `doctor`/preflight now fails loud when auto-drive has no model (dogfood find)

Surfaced by dogfooding: a config with `praxec.agents.auto_drive: true` but no
`gateway.models_yaml` passed `praxec doctor` with **`preflight: ok`** — then every
auto-driven agent leaf would fail at runtime with no model, after burning setup
and wall-clock. A silent fail-open on a runtime binding — the same class as a
coding leaf handed an empty `repo_root`. Preflight now checks that, when
auto-drive is enabled, its `auto_drive_affinity` (default `reasoning`) resolves
to a concrete model through `gateway.models_yaml`; if it doesn't (key unset, file
won't load, or no binding for the affinity) it fails with `AUTO_DRIVE_NO_MODEL`
naming the affinity and the exact fix — the model analog of `REPO_ROOT_REQUIRED`.
Generalizes `doctor` from "validate the artifacts present" to "validate the
dependencies of enabled features."

### Added — a writable code target no longer needs a `praxec.repo.yaml` (FB-2, dogfood find)

`repos:` conflated "definition-providing pack" with "writable code target," so a
real code repo (no praxec manifest) hard-failed at config load
(`reading repo manifest <target>/praxec.repo.yaml: No such file or directory`) —
the workaround was to plant a dummy manifest inside the checkout and git-exclude
it, a foot-gun one `git add -A` away from a PR. A `repos:` entry may now set
`definitions: false` (default `true`): it skips manifest + layout loading
entirely but still registers the canonical path as a writable `repo_root`.
Requires `writable: true` on the same entry (a non-writable, non-definition repo
would be inert) — rejected loud otherwise.

### Added — `start`'s `repoRoot` selector resolves worktrees under a declared root (FB-3, dogfood find)

To run N parallel single-run flows on N git worktrees, the operator previously
had to pre-declare all N as `writable: true` repos. A `start` `repoRoot` selector
now also resolves to a **subpath of an already-declared writable root** (e.g. a
worktree checkout under it), so one declaration + a per-call worktree path covers
fan-out — matching the per-spawn `repoRoot` override `flow.drive-program` already
threads. Containment is component-wise (`Path::starts_with`, not string-prefix,
so `/repo-foo` is not "under" `/repo`); a path outside every declared root is
rejected `REPO_ROOT_OUTSIDE_ALLOWLIST` and an unknown selector `REPO_ROOT_UNKNOWN`.
Still an allowlist — never arbitrary free-text roots.

### Fixed — declared `connections:` are indexed in the default discovery surface (FB-6, dogfood find)

`discovery.include` defaulted to `["proxy", "workflows"]` while the docs promised
connections were searchable — a doc-vs-code drift that left a declared `kind: mcp`
connection absent from capability search (`praxec_query` returned no match for a
tool that was configured). The default is now `["proxy", "workflows", "connections"]`,
so declared connections are discoverable out of the box.

### Added — per-connection `startupTimeoutMs` (FB-7, dogfood find)

A slow deterministic MCP whose first output legitimately takes longer than the
30 s idle timeout (a repo scan) was reaped mid-work
(`idle for 30000ms with no activity`). A connection may now declare a distinct
`startupTimeoutMs` bounding the connect/initialize phase separately from
steady-state idle; resolution falls back `startupTimeoutMs → idleTimeoutMs → 30s`,
so long-starting scanners aren't killed before they produce anything.

## [0.0.22] — 2026-07-15 — hardening release: scope parity, cross-repo routing, and invariant proofs

A hardening release. It closes the `$.run.repo_root` validator↔runtime parity gap
that blocked pack authors, adds per-spawn `repo_root` routing for cross-repo work,
and — via adversarial probing and proof-through-induction — surfaces and fixes a
silent scope-drift defect and pins a suite of system invariants (link↔submit
parity, guard fail-closed, store differential parity, no-silent-wedge liveness,
serde round-trip, cross-position scope parity) so the whole classes stay closed.

### Hardening — proof-through-induction invariant suite

Six invariants pinned as property / metamorphic / inductive tests, each targeting
a place where two representations could silently drift or a safety property could
regress:

- **Link ↔ submit parity** — under `linkFilter: byGuards`, the surfaced HATEOAS
  links equal exactly the set of transitions a submit accepts; the two deliberate
  asymmetries (the actor gate; default `linkFilter: all` = discovery-not-
  enforcement) are pinned so a change to either is conscious, not silent.
- **Guard fail-closed** — every store-dependent guard kind (`evidence`,
  `guidance_acknowledged`, `script_acknowledged`) denies when its backing store is
  unwired; a governance gate can never pass by default.
- **Store differential parity** — the same op sequence yields the same observable
  state across the in-memory / file / sqlite `WorkflowStore` backends (set-
  compared). Directly targets the class that once escaped (sqlite querying
  `$._lock_wait` instead of `$.context._lock_wait`, silently stranding lock-
  suspended workflows on prod while the in-memory test stayed green).
- **No-silent-wedge liveness** — a run that structurally can reach a terminal but
  whose only exit is permanently guard-blocked never falsely reports `succeeded`;
  it stays put and surfaces no legal action. (A load-time guard-satisfiability
  rule is deliberately omitted — undecidable in general, so it would risk false-
  positives on valid packs; the structural liveness checks already run at load.)
- **Serde round-trip** — persisted `WorkflowInstance` re-serializes identically
  (the regression net for schema cutovers like the `run_env` field), and a
  snapshot lacking `run_env` is rejected rather than defaulted.
- **Cross-position scope parity** — the templating renderer (the one scope
  position outside the `read_in_scopes` family) resolves `$.run.repo_root`
  identically, completing the metamorphic scope-parity invariant.

0.0.21 shipped `$.run.repo_root` resolution in the runtime but did not teach the
**static validator** about it: the read-scope allowlists behind V28
(`use.inputs`), V29 (executor args), and the guard-scope check listed only
`$.context.*` / `$.arguments.*` / `$.workflow.input.*`. So a pack that wrote
`$.run.repo_root` in a `workingDirectory`, an `args` operand, a `use.inputs`
value, or a guard failed `praxec check` even though the engine would have
resolved it at runtime — the flagship feature was usable via the engine's own
`file:{{ … }}` auto-drive injection but not by pack authors. This release closes
the parity gap and hardens against the whole drift class.

### Fixed — `$.run.repo_root` accepted wherever the runtime resolves it

- V28 / V29 read-scope allowlist (`is_resolvable_use_input_scope`) and the guard
  validator (`is_resolvable_guard_scope`) now accept `$.run.repo_root`; the guard
  runtime (`resolve_operand`) resolves it from the instance's `RunEnv`, so guards
  reach parity with args / use.inputs / merge-output (V27 already had it).
- Diagnostic hints (V27/V28/V29 + `UNRESOLVABLE_GUARD_SCOPE`) now list
  `$.run.repo_root` so authors see it as a legal scope.

### Fixed — reject scope operands with surrounding whitespace (adversarial find)

Found by adversarially probing the scope machinery: the load-time validators
trim an operand before matching (`is_rooted_operand` / `is_resolvable_*` all call
`.trim()`), but the runtime scope-gate (`mapping::resolve_value`,
`guards::resolve_operand`) matches VERBATIM — `starts_with("$.")` on the untrimmed
string. So a padded operand like `"  $.context.x  "` passed `praxec check` yet, at
runtime, was treated as a *literal* and the raw string reached the tool/guard
instead of resolving — a silent wrong-value the clean-token parity test never saw.
`praxec check` now rejects any `$.`-rooted operand carrying surrounding whitespace
(`SCOPE_OPERAND_WHITESPACE`) across all five operand positions (guard `expr`,
`use.inputs`, executor args, merge-output/`output:`, and the `repoRoot` override),
with a message naming the operand and the exact trimmed form to write. Fail-loud
over guess-the-intent.

### Added — per-spawn `repo_root` override (cross-repo routing)

A `kind: workflow` transition may now carry a `repoRoot:` value that routes its
child sub-run to a **different declared writable repo**, instead of inheriting the
parent's. This is the cross-repo primitive: an orchestrator like
`flow.drive-program` routes each deliverable to its own repo, and the child then
uses `$.run.repo_root` uniformly (the routed repo becomes its ambient root) — no
per-cap `repo_path` fallback, no split single-vs-multi-repo capabilities. The
override value resolves at spawn time against the parent's scopes (`$.context.*`,
`$.arguments.*`, `$.workflow.input.*`, `$.run.repo_root`) and is then matched
against the declared writable repos with the **same invariant as a top-level
`repoRoot` selector** — a declared repo's canonical path only, never an arbitrary
or hallucinated path (`REPO_ROOT_OVERRIDE_INVALID` / `_UNRESOLVED` fail-fast at
spawn; `UNRESOLVABLE_REPO_ROOT_OVERRIDE_SCOPE` at `praxec check`). Absent → the
child inherits the parent's root as before; run/trace correlation is preserved.

### Added — metamorphic parity test (kills the drift class)

A cross-position invariant asserts that every run-ambient scope token is accepted
by **all** validator allowlists **and** resolved to a non-null value by the
runtime — failing the build on drift in either direction (validator-rejects-but-
runtime-resolves, or the v0.0.19 validator-accepts-but-runtime-nulls silent-scope
bug). This is the structural fix for the class that produced the V6, V28/V29, and
guard-scope gaps.

## [0.0.21] — 2026-07-15 — run-ambient repo root + path-grounding gate

Dogfooding 0.0.20's delivery flows surfaced a blocker class: a coding agent leaf
got an **empty filesystem root** because `repo_path` was never hand-threaded
through a sub-workflow's `use.inputs`, so it burned its full step budget writing
nothing. That was a **plumbing** failure — a string that didn't get threaded —
not a reasoning one, so the fix is structural, not a smarter prompt. This release
makes the run's repo root **run-ambient**: a structurally-guaranteed, contained
write root established once at the boundary and propagated through every spawn, so
a coding leaf can never be handed a missing root and can never write outside it.

Two **independent** mechanisms ship together, with deliberately distinct jobs —
they are not two halves of one anti-hallucination trick:

- **`repo_root` is containment + guaranteed presence.** Its value is that it is
  always there (no un-threaded string to forget) and that it is a wall a run
  cannot write past — which matters most for untrusted, sandboxed write agents.
  It is *not* a scoping hint that keeps a model "on track"; a model that needs to
  focus on one area is told so in its prompt.
- **`path_grounding` is the anti-hallucination gate.** It deterministically
  checks that the paths a plan actually names exist under the root before a human
  sign-off. This is the piece that catches an *invented* path.

### Added — mandatory run-ambient `repo_root` (`$.run.repo_root`)

- New typed `RepoRoot` (a canonical, absolute, existing directory — illegal
  states unrepresentable) carried on a `RunEnv` that also holds the run/trace
  correlation ids. `RunEnv` rides the same structural rail as `depth`:
  established at `workflow.start`, persisted on the instance, and propagated
  **parent → child at every `kind: workflow` spawn** (which previously reset
  `run_id`/`trace_id` to `None`, breaking correlation across a run tree).
- Templates resolve `$.run.repo_root` in both scope systems (goal/tool strings
  and executor arg/prompt/body rendering). A coding leaf's file tool is now
  `file:{{ $.run.repo_root }}` — always resolves, so there is no un-threaded
  path to forget.
- The root is sourced at the boundary from the config's declared **writable
  repos** (never a free-text path an agent could invent): one writable repo is
  used automatically; multiple require a `repoRoot` selector on `start`; none
  declared → `start` fails fast (`REPO_ROOT_REQUIRED`). Every deployment now
  declares ≥1 `writable: true` repo.
- **`repo_root` is the *containment boundary* — declare the whole writable repo,
  not a subdirectory.** It is the outer wall, meant to be broad; narrowing a
  monorepo run to one sub-app is a *focus* concern (handled today by the agent's
  prompt), not a job for the root. A future `workdir` cursor *inside* the root
  will make focus structural without shrinking reach — so that shared-package
  reads and legitimate cross-directory edits are never walled off, and
  `path_grounding` (which grounds under the root) never false-blocks a real
  sibling path. Declaring a subdirectory as the writable repo to fake focus is
  unsupported: the git commit/push path resolves the enclosing `.git`, so a
  sub-repo root breaks it.

### Fixed — V6 blocked deterministic executors from backing a capability

The primary-executor contract (V6) predated the deterministic-executor family,
so its allowlists (`Cognitive → mcp|noop`, `Deterministic → script|mcp`) rejected
every model-free authoring/introspection/gate kind — including the `inventory`
executor 0.0.20 added for `cap.research.tool-inventory` and the `path_grounding`
gate this release adds. A capability could therefore only use them via a flow's
inline transition, never as its own primary — half their intended value, and the
reason the deterministic-tool-inventory capability could not validate at all. V6
now accepts the deterministic family (`inventory`, `path_grounding`,
`structural_analysis`, `ingest`, `diff`, `dry_run`, `registry`, `tool_source`)
as a strictly-safe primary for any Cognitive or Deterministic verb, driven by a
`DETERMINISTIC_PRIMARY_KINDS` set (poka-yoke: a build-failing test cross-checks it
against the executor registry). `script`/`cli`/`rest` stay category-bound as
before — you still can't shell your way to a `plan`.

### Added — `path_grounding` gate (#7)

A deterministic, fail-closed executor that checks every file path an agent
referenced in a plan actually exists under the run's `repo_root` **before** a
human sign-off — catching invented paths (e.g. a plan naming
`src/sysadmin/.../CatalogView.tsx` when the tree has
`src/_components/tools/SysadminSignalCatalog/`). Data-driven via `groundedPaths`
context pointers; edit-targets must exist, optional `createPaths` need only their
parent dir; a `..`/absolute escape is a distinct hard refusal. A missing path
fails the chain (`PATH_NOT_GROUNDED`) so the instance never advances past the
gate. `resolve_under` is lifted to a shared `praxec-core::path_safety` module
used by both the gate and the file-edit tool host.

### Added — dispatch-time fail-fast for an unresolved file root

If an agent leaf's `file:` connection renders to a non-absolute root (an
un-migrated `file:{{ $.workflow.input.repo_path }}` with no `repo_path`), the
executor now refuses to dispatch (`FILE_TOOL_ROOT_UNRESOLVED`) **before** spending
any step budget — converting the silent budget-burn defect into an instant
diagnostic. The prevention half (a mandatory root that always resolves) plus this
detection half close the class.

### Changed

- `WorkflowInstance` / `StartWorkflow` carry `run_env` in place of the standalone
  `trace_id` / `run_id` fields. **Breaking for durable stores**: `run_env` has no
  serde default, so a pre-0.0.21 persisted instance fails to deserialize — a
  documented store-wipe on upgrade (greenfield, dev-tier stores). The sqlite
  run-id index/query now reads `$.run_env.run_id`.

## [0.0.20] — 2026-07-14 — observability, HITL, and contract-clean gating

A consumer-dogfooding release: driving 0.0.19 to mint and deliver real workflows
(a C# authoring pack, a React feature) surfaced a cluster of blockers — the
authoring pack couldn't survey its own tools, human gates were invisible to the
MCP-driving agent, and a contract-dirty pack served a broken surface silently.
This release closes all of them, adds the full MCP-native HITL story
(elicitation push + relay), and makes a dirty pack **un-operable-but-repairable**
by construction.

### Fixed — the authoring pack could not survey its own tools (BLOCKER)

`cap.research.tool-inventory` (step 1 of every authoring flow) asked an LLM agent
to "survey the live gateway for reachable tools" — but the agent had no tool with
which to enumerate the gateway, so it burned the full 900s step budget
(`AGENT_STEP_BUDGET_EXHAUSTED`) and produced nothing, making the entire meta
authoring pack unusable. Surveying the gateway's own registry is inherently
deterministic: a new **`inventory` executor** reads the live `DiscoveryIndex`
directly and emits the typed `{ mcp_tools, cli_scripts, skills, capabilities,
connections, workflows, agents, counts }` inventory in one instant, governed
step — no model, no budget, no hallucinated tools. The cap is now an
`actor: deterministic` step that completes on `start` via the deterministic
chain. Verified against the exact consumer repro.

### Added — MCP-native human-in-the-loop

A workflow that parks on a human gate (an `actor: human` approval or an agent's
in-workflow question) is now visible and resolvable to the agent driving over
MCP, four ways:

- **Typed pull-list** — `praxec.query { approvals: true }` enumerates every
  parked gate as a typed `PendingHumanGate`, and a `waiting` response carries the
  gate inline as `pending_human` with a ready-to-fire `resolve` handle.
- **Elicitation push** — when a `praxec.command` parks on a gate and the client
  advertises the MCP `elicitation` capability (SEP-1319 / rmcp 1.8), praxec turns
  the gate into an `elicitation/create` round-trip and **resumes the mission
  in-band** on accept (under a human principal); a non-capable client keeps the
  pull-list. The form is built from the resolving transition's declared
  `inputSchema`.
- **Sub-workflow gate surfacing** — a parent parked waiting on a `kind: workflow`
  child that holds the gate used to show `links: []` (the actionable transition
  lived on the child). The parent now surfaces the child's gate with
  `onChildWorkflow: true` and a `resolve` handle targeting the child.
- **Elicitation relay** — praxec is a middle node: a downstream `kind: mcp` tool
  that itself prompts a human now has its `elicitation/create` **proxied through
  praxec** up to praxec's own upstream client. Tool-agnostic by construction (it
  forwards opaque params), so any eliciting MCP server reaches a human through
  the gateway.

### Added — V30 `USE_BINDING_CONTRACT_DRIFT` + contract-clean gating (poka-yoke)

- **V30** joins the V25–V29 silent-scope series: a static, load-time check that
  cross-validates every `kind: workflow` step's `use` block against the
  referenced definition's declared `snippet` contract — a mapped input the child
  doesn't declare, a required child input the host omits, or a `use.outputs` name
  the child never produces are all hard errors (previously silent). It caught a
  latent dead `filter` binding in the shipped meta pack.
- **Contract-clean gating**: a pack with ANY contract error no longer serves a
  broken functional surface. The gateway comes up in **repair-only mode** — it
  refuses to `start` anything except the operator-declared repair surface
  (`praxec.repair_surface` allowlist, or a per-workflow `repair: true` marker),
  surfacing the precise diagnostics + callable repair links on every gated call
  and on `home`. A clean reload reopens the full surface; a dirty reload drops
  to repair-only (never exits). You cannot operate a dirty pack — only repair it.

### Added — observability + operations

- `praxec.query { observe: true }` now **tails the live** audit window (was
  returning a stale window); `praxec health` and the MCP discovery `home` report
  the running binary's **version**, so a consumer can detect a stale server.
- **`praxec cleanup`** prunes residual rotated audit-log files (dry-run by
  default, `--force` to delete; fail-safe — never touches undateable or recent
  files).
- **`praxec sync`** fast-forwards local git-backed pack repos to `origin/main`
  (fail-safe: only a clean checkout on `main`), and startup warns when a local
  pack repo is checked out off `main` — the stale-pack failure mode.

### Companion pack + org changes

- **praxec-meta** — `cap.research.tool-inventory` reshaped to the deterministic
  `inventory` executor; the dead `filter` binding removed from
  `flow.author-capability`/`flow.author-flow`.
- **cognitive-architectures** — the delivery flow is now **stack-aware** (detects
  Rust/TS/.NET and routes to `cap.verify.{rust,ts,dotnet}` instead of hardcoded
  cargo; `cap.verify.ts` added), threads `repo_path` to the implement agent, and
  guards against a failed/no-op implement reaching "complete" with honest
  outcomes. A deterministic **path-grounding gate** blocks a plan with
  hallucinated file paths from reaching signoff.
- **/praxec org repos** standardized on **rmcp 1.8** (SEP-1319 elicitation) so
  the relay proxies elicitation with uniform typed params.

## [0.0.19] — 2026-07-14 — the silent-scope hardening

A dogfooding-driven consolidation release. Running 0.0.18 against a real
.NET/React/C# repo surfaced a class of defect — a scope the resolver quietly
coalesces to `null` (or ships as a literal) — that this release closes on every
surface (guard, output, use.inputs, executor args), each new validator paired
with a mutation operator that must kill it. It also finishes the two harnesses
that let the class hide: `praxec fuzz` is driven to fully green on the pack, and
the mutation score is made honest against a real baseline.

### Hardening — close the silent/fail-open scope gaps (V25–V29)

The theme is one lesson: **a scope the resolver quietly coalesces to `null` (or
ships as a literal) is a bug it hides.** The 0.0.18 dogfooding found a guard that
read `$.input.mode` — a scope the evaluator resolves to `null`, making the guard
permanently false and wedging the cap. That was one instance of a class, and
v0.0.19 closes the class **everywhere** it appears — guard, output, use.inputs,
executor args — with a mutation operator behind each rule so it stays honest. An
FMECA sweep of both the core and executor crates found every coalescing site;
none were left as a "follow-on." `$.input.*` turns out to be a bound scope
**nowhere** (not even in a pipeline); the canonical spelling is
`$.workflow.input.*`, and the validators now enforce that uniformly.

- **V25 `UNRESOLVABLE_GUARD_SCOPE`.** A load-time error on any guard `expr`
  operand that is `$.`-rooted but names no resolvable scope (`$.context.*`,
  `$.arguments.*`, `$.workflow.input.*`, `$.workflow.{id,state,version}`). The
  resolvable set lives in one predicate the evaluator and the validator both
  consult — a poka-yoke test keeps them from drifting. The evaluator also now
  **fails fast** on such an operand instead of coalescing to `null`.
- **V26 `SCALAR_OUTPUT_FROM_OPTIONAL_SOURCE`.** A warning: V24 proves an output is
  *written*; V26 catches one *written from a source that can be null*. The
  repair rung coerces a missing array/object to `[]`/`{}`, but it cannot repair a
  scalar — so a declared `summary: {type: string}` sourced from an optional
  argument lands `null` at terminal when the agent omits it. Found exactly three
  in the pack (survey-confirmed), each fixed with a `default`.
- **V27 `UNRESOLVABLE_WRITE_SCOPE`.** The write-side twin of V25, from an FMECA
  sweep of the resolver code. `output:` / `onEnter.output:` / `prefill:` mappings
  had **no** scope validator — an unrecognized `$.`-rooted path (a typo, an
  `$.input.x`) silently wrote `null`. V27 caught a real bug on first run: the same
  human-approve cap wrote `plan_final: "$.input.plan"`, dropping the operator's
  approved plan on the auto path. It also flagged two of praxec's own test
  fixtures that had been silently writing null.

- **V28 `UNRESOLVABLE_USE_INPUT_SCOPE`** and **V29 `UNRESOLVABLE_EXECUTOR_ARG_SCOPE`.**
  The read-side companions. `use.inputs` values and executor `args:`/`map:`/
  `query:`/`body:` path strings resolve against `{context, arguments,
  workflow.input}` only — an unrecognized `$.`-rooted scope seeded a null or (for
  executor args) reached the shell/tool/endpoint as its **literal token**. V29
  caught the confirmed bug behind this: `arg_render` *documented* `$.input.x`
  support the code never delivered, so thirteen shipped caps were passing
  `$.input.gateway_config_path` to the shell verbatim. All fixed.

Alongside the load-time validators, every executor resolver that used to coalesce
silently now **fails fast** at runtime, mirroring the `mcp` `map:` model that
already did: `arg_render` (script/cli args), `rest` (query + body — previously no
guard at all), and the legacy sub-workflow `input:` path.

Each rule ships with a mutation operator that must be killed by it —
`retarget_guard_scope`, `weaken_output_source`, `retarget_output_scope`,
`retarget_use_input_scope`, `retarget_executor_arg_scope`. The mutation report on
the live pack is **100% across all fourteen operators**, so the rules are
attacked, not assumed.

### Fixes from dogfooding 0.0.18 against a real .NET/React/C# repo

The engine was *silent* where it should have been *loud*.

### Fixed — the multi-turn fix-loop stall on reasoning models

A reasoning model on a real fix-loop would do the work, emit the diff as text,
and never call the `final_answer` tool. The turn budget exhausted, the step
reported `AGENT_NO_RESULT`, that classified as `Capability` (escalatable), and
the chain-walk quietly retried the whole thing on the next model — three 600s
walls, ~30 minutes, no verdict, and nobody told. Two independent defects were
conflated there; both are fixed.

- **Sign-off ceremony.** On turn exhaustion the runner now takes one more turn on
  the *same* model with reasoning disabled and `tool_choice: Required`, purely to
  capture the sign-off. Thinking-mode models reject a forced `tool_choice` with a
  hard 400 — which is why the in-loop `force_final` steer can only ever *offer*
  `final_answer` — but turning reasoning off makes the force legal. The model that
  did the work signs off on it. This is robust handling of the ceremony, not a
  capability restriction: reasoning models stay fully in play. Every miss path
  (transport error, still no tool call, non-conforming output) degrades to exactly
  the previous `Exhausted`/`AGENT_NO_RESULT`, so a model that still refuses ends
  where it ended before.
- **Hard per-step budget.** The chain-walk had no wall-clock bound: each model in
  the chain got its own `max_seconds`, so an all-`NoResult` walk burned N × 600s in
  silence. An agent step now carries a budget (`step_budget_seconds`, default
  **900s**); each attempt's wall is clamped to what remains, and escalation stops
  once too little is left to try again, surfacing a new terminal
  `AGENT_STEP_BUDGET_EXHAUSTED`. That code deliberately does **not** classify as
  `Capability`, so it routes to a human instead of feeding the churn it exists to
  stop.

### Fixed — a capability's output contract is now enforced on a direct run

A capability's declared `snippet.outputs` was validated **only** on the compose
path, against the host's `use.outputs` projection. A direct top-level run
validated nothing. So an author could run a cap on its own, see green, and only
discover the contract violation once someone wrapped it in a `use:` block —
which is how a perfectly good `verify` verdict got discarded downstream over a
single stray provenance key.

A definition now owes its declared outputs at its **own** terminal state, whether
or not anything composed it. The check is expressed as the existing compose check
evaluated under a synthesized *full identity binding* — it reuses
`repair_outputs_against_snippet` and `validate_outputs_against_snippet` verbatim
rather than reimplementing them, so the two paths cannot drift, and a full binding
is the strictest host any composer could be. That buys the property worth having:

> **a green direct run implies a green composed run.**

A violation fails the run as `cap_output_schema_violation` with recovery links and
a `cap.output.schema_violation` audit event naming the offending slot — the same
event the compose path emits, because it is the same defect. The
deterministic-repair rung runs first, exactly as it does under `use:`, so the
terminal check is never harsher than the compose check it mirrors.

### Added — V24 `UNWRITTEN_DECLARED_OUTPUT` (the compile-time half)

The terminal check above cannot catch every unwritten output, and that is not a
flaw in it: the deterministic-repair rung coerces a missing `array`/`object` to
`[]`/`{}` *before* validating, because a composing host repairs too and the
terminal check must never be harsher than the compose check it mirrors. So a
never-written `report: {type: object}` goes green at runtime and the caller reads
an empty report instead of an error. Only a static analysis sees that the slot has
no writer at all.

V24 proves from the state graph that every declared output is written on **every**
path to **every** terminal — a MUST dataflow over the declared-output lattice,
computed as a *greatest* fixpoint so a retry cycle cannot launder an unwritten slot
into a written one. It caught a broken fixture in praxec's own corpus on first run.

### Fixed — the mutation score was measuring nothing

`praxec-test`'s mutation harness credited a kill for *any* diagnostic or fuzz
failure, absolutely. Run that against a real corpus with a single pre-existing
complaint and every mutant is "killed" by a defect that was already there. The tell
was the two semantic operators — which exist to document what the tool *cannot*
catch — also reporting 100%.

Kills are now credited only for a complaint the mutant **caused**: every gate is
diffed against what it already said about the unmutated config. Two new CONTRACT
operators (`drop_output_write`, `drop_initial_context_seed`) delete a declared
output's only writer — the class that shipped the bug above, and which no operator
previously modeled, which is precisely why nothing warned us. A gate you never
attack is a gate you are only assuming works.

### Fixed — the fuzz mock could not produce the contracts it was mocking

Three bugs, all the same shape, each making `praxec fuzz` report failures the
definitions were not guilty of. The dummy synthesizer had no `$ref` arm, so every
slot capability in the pack (they all spell their contract as
`$ref: praxec://hop#/$defs/verifyOut`) got `null` and looked like a contract
violation. The output plan could only hold an *object*, so a `kind: mcp` leaf whose
result is legitimately an array (`corpus_search`) could only ever emit `{}`. And
the isolated prober fabricated a mid-flow context that skipped the states it was
pretending had already run. On the live pack: **152 → 116** fuzz failures, with
zero coming from the new terminal check. A bare `CHAIN_FAILED` now also carries its
error class and message, which is how all three were diagnosed.

### Fixed — `praxec fuzz` is now fully green on the pack (116 → 0 hard failures)

The remaining 116 per-transition failures were all the mock failing to reconstruct
the state a real prior chain would have produced — not defects in the definitions.
Each was a distinct fidelity gap, now closed:

- **Nested guard reads.** A guard on `$.context.out.status` was seeded only at
  `out` (a `{}` dummy), leaving `.status` unset → `GUARD_UNSET_SLOT`. The seeder
  now writes at the full dotted path, and the mock output plan plants a
  guard-satisfying value *inside* the field it writes.
- **Definition-wide context.** An isolated probe fires one edge and lets the chain
  run, but seeded only the probed edge's reads — so any downstream guard hit an
  unseeded slot. The probe now starts from a context covering every slot the
  definition reads, typed from the guards that compare it.
- **Slot-vs-slot and input guards.** `iter >= iter_cap` (neither side a literal)
  and `$.workflow.input.stack == 'rust'` (input-scoped) were invisible to the
  satisfier; both are now seeded — the input per-edge, since siblings gate the same
  slot on exclusive values.
- **The violating-context probe.** It set a bool to "violate" a guard, but a bool
  does not violate `status != 'pass'`, so the transition fired and the fuzz cried
  "guard did not reject". It now computes a value that genuinely fails the clause,
  and skips the check when it cannot guarantee one.
- **`$ref` / nested / `minProperties` arguments.** A `hop_slot` transition's
  `inputSchema` is a bare `{$ref: verifyIn}`; the arguments builder now resolves the
  ref, includes optional properties (so an output mapped from an optional argument
  isn't null), and honors `minProperties`.

Left green the honest way: this surfaced one **real** pack bug —
`cap.gate.human-approve-plan` guarded on `$.input.mode`, a scope the guard
evaluator resolves to `null` (it recognizes `$.workflow.input.*`, not bare
`$.input.*`), so both its transitions were dead. Fixed in the pack. The 44
remaining smoke wedges on agent-heavy flows stay advisory — a mock chooser cannot
make an agent's decisions, which is what `--live --model` is for.

With a clean fuzz baseline, the mutation score is now meaningful rather than
inflated: 100% across all nine operators (1434 mutants), and the two guard
operators are genuinely *killed* by the violating-context probe — they were never
really blind spots, only older than the harness.

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

### Fixed — orchestrate observability & recovery (defense in depth)

- **Mission heartbeat + no-progress watchdog.** A single autonomous decision now
  pulses a "still working (Ns)" heartbeat to the mission bus every 15s, so a
  client can tell a slow reasoning call from a hung one, and is bounded by a
  mission-level backstop — a wedged step ends the drive as `TimedOut` instead of
  looping. (The per-step agent timeout still normally fires first; this is the
  layer above it.)
- **Startup orphan reap.** An instance a driver/CLI left mid-`running` (its
  process died) is a durable zombie no live owner will advance. On a fresh boot
  there are no in-process drivers, so `serve` now cancels the orphaned *running*
  instances at startup (auditable, via the same cancel path). It deliberately
  never touches work that legitimately persists across restarts: terminal or
  cancelled instances, human gates (a person may return), and engine-waits that
  self-resume (lock / subworkflow / agent-await) — classified against each
  instance's own definition snapshot.
- **Repo load reports every invalid file at once.** A malformed flow/cap file in
  a repo aborted the load at the *first* bad file, so an author fixed one,
  restarted, and hit the next. The loader now accumulates and names *every*
  invalid file in one error. It stays fail-whole — an invalid file never loads a
  partial config (no fail-open) — it just no longer masks its siblings.

Note: a "force the fallback model to be non-reasoning" item from the report was
deliberately **not** taken. Its premise (praxec can't handle reasoning models)
was already false and is doubly so after the stall-watchdog fix — reasoning
models are first-class. Resilience against a flaky model comes from correct
response handling + real timeouts + chain-walk escalation + the circuit breaker,
never from restricting which model classes the system may use.

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
