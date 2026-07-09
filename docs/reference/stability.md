# Stability tiers

| Artifact | Version | Stability commitment |
|----------|---------|----------------------|
| Crate (`praxec` et al.) | 0.0.15 | Pre-1.0 — semver not yet in effect. Breaking changes may occur at any minor bump. |
| Config schema (`version` field) | "1.0.0" | Tier 1 stable — backward-compatible within the same minor schema version. |
| Two-tool MCP surface (`praxec.query` + `praxec.command`) | Stable | Fixed tool names, inputs, and output shapes. Operations are reached by varying args; removals follow a deprecation cycle. |

We distinguish three tiers of stability commitment. Every public
artifact — tools, config keys, schemas, doc links — falls into exactly
one tier. The tier decides how quickly you can depend on it and what
happens when it changes.

---

## Tier 1 — Committed

**Breaking changes require a deprecation cycle.** You can depend on
these without pinning a specific version.

| Artifact | Notes |
|----------|-------|
| Two MCP tool names and their input/output shapes | `praxec.query` (reads) and `praxec.command` (writes), per SPEC §32. All operations are reached by varying args, not the tool name; removals or incompatible changes go through the deprecation cycle. |
| Config schema `version` field | Must parse as a semver-compatible string. Backward-compatible within the same minor version. The `praxec check` command verifies this field exists. |
| Top-level config keys | `version`, `include`, `capabilities`, `connections`, `proxy`, `workflows`, `audit`, `discovery`, `store`. |
| Major guard kinds | `permission`, `role`, `expr`, `evidence`. The deprecated alias `jsonpath` is accepted but emits a warning. |
| Major executor kinds | `noop`, `cli`, `rest`, `mcp`, `human`. |
| `noop` executor as first-class no-op semantics | `noop` is **not** a stub or placeholder — it is the committed default for `proxy.expose` entries that don't specify an executor, and the load-bearing executor for transitions that exist to advance state without firing side effects (gate checks, human approvals, guidance-only transitions). Returning `{}` and `[]` evidence is the contract, not a TODO. |
| `(unset)` template placeholder rendering (SPEC §5.2) | When a `{{ $.path }}` template can't be resolved, the renderer emits a marked stub like `(testCount: unset)` instead of panicking, stripping the placeholder silently, or returning the raw `{{ }}` to the consumer. This is **graceful degradation by design** — the workflow continues with a visibly-unresolved value instead of failing or silently misbehaving. The stub format `(<lastSegment>: unset)` is the committed shape; the resolver MUST NOT silently strip and MUST NOT panic on unresolved templates. |
| Audit event taxonomy | Event type names and the shape of their payload. New event types may appear; existing ones don't change shape. |
| `WorkflowStore` trait | Implementations may be added; the trait's method signatures won't change incompatibly. |
| `Executor` and `ExecutorRegistry` traits | As above. |
| Intent-layer invariant (SPEC §23.6) | Verb taxonomy lives only on `skills` and `scripts`. The access layer (`connections`, `capabilities`, `executors`) is kind-typed. Workflows compose. Audit events describe. This is an architecture rule, not an implementation choice — breaking it requires a SPEC §23 amendment, not just an API deprecation. |
| Closed cognitive verb enum (SPEC §5.4.1) | 10 verbs as of v0.3: `triage`, `diagnose`, `plan`, `implement`, `review`, `refactor`, `explain`, `compose`, `research`, `summarize`. Adding a verb requires SPEC §23.7 amendment criteria. Removing one requires a deprecation cycle. |
| Closed script verb enum (SPEC §22.3) | 12 verbs as of v0.3: `build`, `test`, `deploy`, `format`, `lint`, `install`, `verify`, `run`, `inspect`, `search`, `fetch`, `audit`. Same amendment policy as cognitive verbs. |
| SPEC ↔ Rust enum drift detection | The `spec_enum_drift.rs` test asserts byte-equality between SPEC verb/root tables, JSON schema enums, and Rust closed enums. Drift fails build. Any change to the verb/root vocabularies must propagate to all three sources simultaneously. |
| `parallel` executor kind (SPEC §24) | Fan-out / fan-in inside one transition. Config shape (branches, join, max_concurrency, on_branch_failure, timeouts, max_recursion_depth), output shape (`{branches, summary}`), audit event taxonomy, and the §24.5 "fan-out inside one transition" invariant are committed. Join conditions: `all` / `any` / `{at_least: K}` / `{percent: P}` / `{expression: "<expr>"}` / `{aggregator: {kind: ...}}` — all committed. The aggregator slot is the **general form**; closed shortcuts are ergonomic sugar. Compensating transactions deferred to a future version. |
| `pipeline` executor kind (SPEC §25) | Sequential composition of N executor steps inside one transition; each step's `output` threads as the next step's `$.input`. Config shape (`steps:`, `on_step_failure: bail | continue`, `total_timeout_ms`), output shape (`{steps, final_output, summary}`), and audit event taxonomy (`pipeline.step.started/.completed/.failed`, `pipeline.completed`) are committed. |
| `while:` loop on a state (SPEC §26) | State-level guard re-evaluated after each transition fires; truthy guard re-routes target back to the same state. `max_iterations:` is REQUIRED on every state declaring `while:` (no default). Iteration counter lives in synthetic context slot `_while_iter.<state>`; cleared on actual exit. `workflow.state.iteration` audit event per re-entry. |
| State-local blackboard slots (SPEC §27) | Slot declarations on a state may carry `scope: state` (default: `workflow` for backward compat). State-local slots are cleared on state exit (transition to a different state) via the `workflow.slot.cleared` audit event; preserved across `while:` re-entry of the same state. `INVALID_SLOT_REDECLARATION` validator catching cross-scope name collisions is the follow-on tranche. |
| Lexicon top-level config block (SPEC §30) | `lexicon:` block holds `{<term>: {definition, bounded_context?, examples?, refs?, governance?}}` entries. Per-workflow snapshot stamping mirrors `_skillsLibrary` / `_scriptsLibrary` invariant — in-flight workflows see the lexicon as it existed at `workflow.start`. `INVALID_LEXICON_ENTRY` on load-time shape violations. |
| Lexicon operations (SPEC §30.5) | `search` (substring + bounded_context filter), `lookup` (exact term), `define` (governance-gated propose / set). Per SPEC §32 these dispatch through the two-tool surface — reads via `praxec.query` (`kind: "lexicon"`), the define write via `praxec.command` — not as standalone advertised tools. |
| Lexicon governance default (SPEC §30.6) | New / re-defined terms default to `governance: human-only`. Agent callers writing via the define operation (`praxec.command` with `subject: "lexicon:<term>"`) against a `human-only` term return `LEXICON_DEFINE_REQUIRES_HUMAN`; the workflow must route through an `actor: human` transition. `agent-may-propose` is the opt-in alternative for scratch / sandbox vocabularies. |
| Declarative slot constraints (SPEC §28) | Slot declarations may carry `constraint:` with one or more constraint kinds. Two committed kinds: `path_allowlist: {allow: [...], deny?: [...]}` (gitignore-syntax globs via the `globset` crate) and `subset_of: "<path>"` (dynamic-reference subset check, fail-fast on unset reference). Evaluated at write time, REJECT the transition with `SLOT_CONSTRAINT_VIOLATED` naming slot + kind + offending value. Composes conjunctively (multiple kinds = all must pass). Load-time validation catches empty `allow:`, malformed globs, unknown kinds via `INVALID_CONSTRAINT_DECLARATION`. JSON Schema primitives (regex, min/max, length, enum) use the slot's existing `type:` field; §28 covers only what JSON Schema cannot express. |
| Per-transition fire cap (SPEC §29.6) | Optional `max_fires_per_visit: N` on any transition. Runtime tracks per-state-entry count in synthetic `_fire_count.<state>.<transition>` context slot, scrubbed on state exit. Exceeding cap rejects with `TRANSITION_FIRE_CAP_EXCEEDED`. Generic — applies to any transition, prevents agent spamming on self-loops. |
| `enable_human_ask` workflow flag (SPEC §29.3) | Workflow-level `enable_human_ask: true` injects a self-loop `ask_human` transition into every non-terminal state at config-resolve time. Schema-enforces `{question, context_summary, attempted_alternatives}` on the agent's question so context arrives with every ask. `human_ask_cap` (default 5) sets the per-state `max_fires_per_visit`. Operator override per state takes precedence (existing `ask_human` is not clobbered). |
| Lightweight transition records (SPEC §29.4) | Transition decl may carry `lightweight: true`; runtime emits `workflow.interaction` audit event instead of `workflow.transition`. Lets audit consumers separate state-change events from interaction events (like `ask_human` self-loops). Same record payload; only event type differs. Non-lightweight transitions unchanged. |
| `purpose:` tag on transitions (SPEC §29.5) | Optional free-form string that propagates into the audit record's `purpose:` field. Enables dashboard / audit-consumer filtering. Open vocabulary; common values: `ask`, `approve`, `escalate`. |
| `branches.where` filter on `parallel.for_each` (SPEC §24.2) | Pre-fan-out predicate. Falsy elements are dropped BEFORE branches spawn. Two index views coexist: `$.branch.index` inside templates is the ORIGINAL source-array position; `branches[].index` in output is the dense fan-out position. |
| `[*]` bracket-wildcard mapping syntax (SPEC §24 / §6) | Path projection over arrays — `$.output.branches[*].field` returns an array of plucked values. Backward-compatible with existing paths. Future extensions (slicing `[0:5]`, filtering `[?cond]`) deliberately out of scope; would require an amendment. |
| `praxec.authoring.preferred_script_language` (SPEC §23.8) | Advisory config field for LLM-driven authoring workflows. Free-form string (no closed enum on the value itself); validated as non-empty when present (`INVALID_AUTHORING_PREFERENCE`). Surfaced to authoring skills via the `$.praxec.authoring.*` template substitution root. Snapshot-stamped onto every workflow at config-resolve time as `_authoringPrefs` so in-flight authoring workflows see the preference that existed at `workflow.start`. The field IS advisory — no runtime branch enforces it; authoring skills opt-in by templating the value into their body. Adding more `praxec.authoring.*` fields is additive. |

## Tier 2 — Deprecation cycle

**Subject to change with a deprecation notice in the changelog and  
one minor-version grace period.** This is the right tier for features
we believe in but want room to refine based on real use.

| Artifact | Notes |
|----------|-------|
| `actor: "deterministic"` and chaining semantics | Auto-execution of deterministic transitions, `maxChainDepth`, `chain` response array, `CHAIN_FAILED` error code. |
| Phase guidance (`goal`, `guidance` on states) | `guidance` response object with `goal` and `instructions` fields. |
| Non-major guard kinds | Any guard kind not listed in Tier 1 above. |
| Non-major executor kinds | Any executor kind not listed in Tier 1 above. |
| Discovery index scoring | Scoring weights, prefix/fuzzy matching thresholds, and the `aliases` field may be tuned. |
| Config hot-reload (SIGHUP) | Swappable definitions, executors, and discovery index. The set of swappable components may expand. |
| Workflow graph validation | The set of `check` diagnostics (unreachable states, dangling targets, dead-ends) may grow. |
| Link-filter semantics | The `byGuards` filter's exact behaviour may be refined. |
| Stress-test scenarios | Test scenarios in `crates/praxec-core/tests/stress_guards_mapping.rs` and `stress_lifecycle.rs` — they document concurrency properties we commit to, but the exact scenario list may grow or shrink. |
| Examples | `examples/*` — illustrative and dogfooded, but the exact directory layout and filenames may change. |
| `delegate` on workflow states (SPEC §21) | Pass-through string surfaced on every workflow response. The shape (non-empty string) and the response surfacing are stable. The gateway never reads or branches on it; consumption is left to harnesses (e.g. the in-repo agentic runtime, the `praxec` TUI). |
| `scripts:` top-level block (SPEC §22) | Script library shape (verb / lifecycle / body \| uri+hash / source) is stable. The closed verb enum (12 values as of v0.3) is committed; adding verbs requires a spec amendment per SPEC §23.7. The blessed-script-roots set (15 values) may grow with the same strict-vs-lenient flag treatment as skill roots. |
| `script` executor kind (SPEC §22.6) | Subject lookup, temp-file materialization (chmod 0700), shebang-or-bash invocation, `script_output` Evidence emission with body hash. Output schema (`exitCode`/`success`/`stdout`/`stderr`/`json`/`scriptSubject`/`scriptHash`) is committed. Transition record's executor descriptor carries `subject`+`hash` for script executors (additive, optional, serde-`skip_serializing_if`-bypassed for non-script kinds). |
| `gateway.scripts.search` (SPEC §22.7) | Authoring-time discovery tool. Refs-only response (verb / subject / source); progressive disclosure invariant committed. Filter set may grow. |
| `script_acknowledged` guard (SPEC §22.8) | Hash-flip-invalidated review-before-execute gate. Distinct keyspace from `guidance_acknowledged`. |
| Script body URI schemes | Three committed: `file://<path>` (relative to config), `https://<url>` (blocking GET, 30 s timeout, sha256-verified), `git+https://<host>/<repo>(.git)?@<ref>#<path>` (`git archive` extraction, sha256-verified). `<ref>` is mandatory on git+https so snapshots are reproducible. New schemes are additive; existing scheme behaviour is committed. |
| `models.yaml` affinity / tier resolver | Closed-enum affinities/tiers, sparse `<affinity>-<tier>` overrides, specificity walk, and the structured `ModelResolutionExhausted` error. Lives in `crates/praxec-core/src/model_resolver/`. Enum additions are minor-version compatible; the resolution algorithm shape may be refined based on usage. |

## Tier 3 — Internal

**No stability promises. May change or disappear without notice.**  
Use at your own risk.

| Artifact | Notes |
|----------|-------|
| Internal crate APIs | Anything not re-exported from `praxec-core/src/lib.rs` or the top-level crate. |
| Unstable config keys | Any key not listed in Tier 1 above. |
| Benchmark internals | `crates/praxec-core/benches/*` — we report numbers but the exact benchmark harness is an internal tool. |
| CI configuration | `.github/workflows/*` — workflow files are operational, not a public API. |
| Dev-only doc pages | `docs/development/stress-tests.md`, `docs/reference/invariants.md` — accurate but may be restructured. |

---

## Verification coverage

What we've tested per tier and how we decide each tier's verification
standard.

### Tier 1 — Stable surface

| Area | Verified | How |
|------|----------|-----|
| Two MCP tools | Yes | Invariant 9 test in `crates/praxec-mcp-server/tests/stable_tool_surface.rs` |
| 10 core invariants | Yes | `crates/praxec-core/tests/invariants_actor_audit.rs`, `invariants_governance.rs`, `invariants_proxy.rs` |
| Capability wrapping and `wraps:` chain | Yes | `crates/praxec-core/tests/capability.rs` |
| `include:` multi-file composition | Yes | `crates/praxec-core/tests/composability.rs` |
| Discovery indexing and search | Yes | `crates/praxec-core/tests/discovery.rs` |
| Evidence guard | Yes | `crates/praxec-core/tests/evidence_guard.rs` |
| File- and SQLite-backed WorkflowStore | Yes | `crates/praxec-core/tests/persistent_stores.rs` |
| REST executor | Yes | `crates/praxec-executors/tests/rest_executor.rs` |
| Human executor audit event | Yes | `crates/praxec-executors/tests/human_audit.rs` |
| TDD example dogfood transcript | Yes | CI runs `examples/tdd/dogfood-drive.py` |
| Deterministic chaining | Yes | `crates/praxec-core/tests/chain_basic.rs`, `chain_audit.rs`, `chain_audit_criticality.rs`, `chain_guidance.rs`, `chain_loop.rs` |
| Phase guidance in responses | Yes | Covered by deterministic chain tests |

### Tier 2 — Deprecation-cycle surface

| Area | Verified | How |
|------|----------|-----|
| Stress tests under concurrency | Yes | `crates/praxec-core/tests/stress_guards_mapping.rs`, `stress_lifecycle.rs` |
| Discovery prefix/fuzzy/alias matching | Yes | `crates/praxec-core/tests/discovery.rs` (3 new tests) |
| Workflow graph validation | Yes | `crates/praxec-core/src/validate.rs` (10 unit tests) |
| Hot-reload swap mechanism | Yes | `crates/praxec-core/src/hot_reload.rs` (unit test) |

### What we don't test (and why)

- **LLM behaviour.** Whether a model follows HATEOAS links is a
  model-level property, not a gateway property. The gateway returns
  correct links; the dogfood transcript mechanically verifies the bytes.
- **Throughput under load.** Stress tests cover correctness under
  concurrency, not throughput. See `docs/reference/performance.md` for latency numbers.

---

## Deprecation process (Tier 1 and Tier 2)

1. The breaking change is announced in `CHANGELOG.md` under
   `## [Unreleased]` with `### Deprecated`.
2. For Tier 1, the old behaviour is maintained for at least one minor
   release after the announcement.
3. For Tier 2, the old behaviour is maintained for at least one patch
   release after the announcement.
4. After the grace period, the old behaviour is removed and the
   changelog entry moves to `### Removed` in the release where it
   actually disappears.