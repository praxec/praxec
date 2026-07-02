# Capability / Flow Composition Model

**Status:** Draft — pending plan
**Author:** Matthew Cochran
**Date:** 2026-05-26
**Affects:** `praxec` (runtime), `cognitive-architectures` (library), any third-party praxec resource repo

---

## 1. Purpose

The `cognitive-architectures` library today ships skills, scripts, patterns,
and demo workflows. Operators who want to drive a full SDLC lifecycle
(`add-feature`, `bugfix-from-error-log`, `safe-refactor`, `triage-issue`,
`dependency-upgrade`, …) have no canonical shape to follow: they
copy-paste pattern YAMLs into a host config and improvise the glue.

This spec defines a **two-tier composition model** so that operators can
assemble lifecycle workflows by combining small reusable capability
workflows — the way GitHub Actions composes reusable steps into pipelines.

**Goals**

- A clean tier boundary between *reusable capability snippets* and
  *lifecycle flows*.
- A typed contract on capabilities so I/O mismatches are caught at
  config-load, not runtime.
- A typed host blackboard so chained capabilities can see what slots are
  available and what type they hold.
- Multi-repo support: operators install N resource repos and praxec
  loads them as one library, with namespacing.
- One mistake-proofing rule that prevents the design from collapsing into
  nested-workflow spaghetti.

**Non-goals**

- Full `extends:` parameterization with multi-instance / type-checked
  params. That stays a v0.5 gap (G1); the design here is the lighter
  contract layer that ships first.
- A package manager for resource repos (lockfiles, dependency
  resolution). Repos are git clones; version pinning is the operator's
  responsibility, supported by an optional capability-contract hash.
- LLM-driving harness, agent runtime, or anything outside praxec's
  gateway-framework boundary.

---

## 2. The two-tier composition model

Every workflow is exactly one of:

- **Capability** (`cap.*`) — a reusable, verb-scoped sub-workflow with a
  typed input/output contract. Designed to be invoked by an flow.
  Examples: `cap.plan.vet`, `cap.tests.write-failing`,
  `cap.verify.workspace-green`, `cap.gate.human-signoff`.
- **Flow** (`flow.*`) — a lifecycle workflow that composes
  capabilities, scripts, MCP tools, and skills into an end-to-end loop.
  Examples: `flow.add-feature`, `flow.bugfix-from-error-log`,
  `flow.safe-refactor`.

The YAML schema is the same for both. Tier is signalled by the
**definitionId prefix** (the runtime check). The schema does not
introduce a `category:` field — the prefix carries the meaning
unambiguously.

---

## 3. Conventions

### 3.1 Identifier convention (runtime-enforced)

| Tier | Prefix | Body shape | Example |
|---|---|---|---|
| Capability | `cap.` | `cap.<verb>.<name>` | `cap.plan.vet` |
| Flow | `flow.` | `flow.<lifecycle>.<name>` *or* `flow.<name>` | `flow.add-feature` |

### 3.2 Directory layout (recommended)

Directory placement is an organizational convention for human scanning,
not runtime enforcement. The runtime works from `definitionId` prefix.

```
<repo>/
  praxec.repo.yaml            # repo manifest (§9)
  capabilities/
    plan.draft.yaml             # definitionId: cap.plan.draft
    plan.vet.yaml               # definitionId: cap.plan.vet
    tests.write-failing.yaml    # definitionId: cap.tests.write-failing
    edit.scope-bounded.yaml     # definitionId: cap.edit.scope-bounded
    verify.workspace-green.yaml # definitionId: cap.verify.workspace-green
    review.adversarial.yaml     # definitionId: cap.review.adversarial
    gate.human-signoff.yaml     # definitionId: cap.gate.human-signoff
    coordinate.pr-open.yaml     # definitionId: cap.coordinate.pr-open
  flows/
    add-feature.yaml            # definitionId: flow.add-feature
    bugfix-from-error-log.yaml  # definitionId: flow.bugfix-from-error-log
    safe-refactor.yaml          # definitionId: flow.safe-refactor
  skills/
    plan.specify.change-request.yaml
    ...
  scripts/
    verify.workspace.green.yaml
    ...
  connections/
    github-mcp.yaml
    ...
```

Filename SHOULD match the unprefixed body: `capabilities/plan.vet.yaml`
defines `cap.plan.vet`. This is convention, not enforcement.

### 3.3 Verb-subject consistency (runtime-enforced)

Every capability declares `verb:` (one of the 24, §4). The runtime
enforces `definitionId` matches `cap.<verb>.<name>` exactly. Mismatch =
config-load error. This makes the library navigable by id: every
`cap.plan.*` is a planning capability.

Flows do NOT declare a verb. They are lifecycle-shaped, not
purpose-scoped; a verb on an flow would either be redundant
(`flow.plan.feature`) or misleading (`flow.implement.feature` is more
than implementing). A `verb:` field on a `flow.*` workflow is a
config-load error.

---

## 4. The 24-verb cloud

Capability `verb:` must be one of:

**Cognitive (10)** — LLM is the actor; the verb describes what it does.

| Verb | Subject root | Examples |
|---|---|---|
| `triage` | `cap.triage.*` | classify-severity, route-component |
| `diagnose` | `cap.diagnose.*` | parse-error, reproduce, localize |
| `plan` | `cap.plan.*` | draft, vet, track-gaps |
| `implement` | `cap.implement.*` | tdd-loop, scope-bounded |
| `review` | `cap.review.*` | adversarial, final-approval |
| `refactor` | `cap.refactor.*` | extract-module, rename-symbol |
| `explain` | `cap.explain.*` | summarize-change, walk-architecture |
| `compose` | `cap.compose.*` | integrate-plans, merge-reports |
| `research` | `cap.research.*` | docs-grill, context-assemble |
| `summarize` | `cap.summarize.*` | session-delta, transition-record |

**Deterministic (12)** — script or MCP tool is the actor.

| Verb | Subject root | Examples |
|---|---|---|
| `build` | `cap.build.*` | cargo-release |
| `test` | `cap.test.*` | cargo-workspace |
| `deploy` | `cap.deploy.*` | cargo-install |
| `format` | `cap.format.*` | rust-check |
| `lint` | `cap.lint.*` | rust-clippy-strict |
| `install` | `cap.install.*` | npm-ci |
| `verify` | `cap.verify.*` | workspace-green, regression-tests |
| `run` | `cap.run.*` | cargo-bench |
| `inspect` | `cap.inspect.*` | dependency-tree |
| `search` | `cap.search.*` | codebase-ripgrep |
| `fetch` | `cap.fetch.*` | docs-pull |
| `audit` | `cap.audit.*` | security-scan |

**Coordination (2 new)** — neither cognitive nor pure deterministic.

| Verb | Subject root | Description | Examples |
|---|---|---|---|
| `gate` | `cap.gate.*` | Awaits a permission signal: HITL approval, evidence quorum, manual ack. | `cap.gate.human-signoff`, `cap.gate.evidence-quorum` |
| `coordinate` | `cap.coordinate.*` | Emits a side effect to an external system (open PR, create issues, post comment). | `cap.coordinate.pr-open`, `cap.coordinate.create-issues` |

The vocabulary is **closed**. New verbs require a SPEC bump in
`praxec`. This matches how the existing 10-verb cognitive cloud
and 12-verb script cloud are governed today (SPEC §5, §22).

### 4.1 Runtime verb-shape check

To prevent verb drift across third-party repos (which may not run the
cognitive-architectures CI lint), the praxec runtime applies a
local verb-shape check at config-load. The check is intentionally
narrow — it inspects only each capability's **primary executor kind**
(the executor on the transition leaving the capability's initial
state) — to keep the runtime cost negligible while still catching
gross verb misuse:

| Verb category | Primary executor SHALL satisfy |
|---|---|
| Cognitive (10) | `kind: mcp` OR `kind: noop` that surfaces a skill (`guidance:` block referenced) |
| Deterministic (12) | `kind: script` OR `kind: mcp` |
| `gate` | At least one transition out of the initial state has `actor: human` OR `purpose: ask` |
| `coordinate` | `kind: mcp` AND the connection is declared `external: true` |

Mismatches are config-load errors. The check is **per-capability,
primary executor only** (TRIZ Local Quality resolution); it does not
walk every transition. Library-level richer verb-appropriateness
checks live in cognitive-architectures CI (`scripts/validate.sh`).

---

## 5. The snippet contract

Capabilities declare a `snippet:` block at the workflow level:

```yaml
workflows:
  - definitionId: cap.plan.vet
    verb: plan
    snippet:
      inputs:
        plan:
          type: object
          required: true
          description: structured plan to vet
        max_iterations:
          type: integer
          default: 3
      outputs:
        verdict:
          type: string
          enum: [pass, fail, needs-revision]
        findings:
          type: array
          items: { type: object }
    states: [...]
```

### 5.1 Contract rules

1. `snippet:` is REQUIRED on `cap.*` and FORBIDDEN on `flow.*`.
   Flows are endpoints; only capabilities are invokable as
   snippets.
2. Within `snippet:`, the `inputs:` and `outputs:` keys are BOTH
   required. Their value MAY be an empty object `{}` (a capability with
   no inputs or no outputs is valid). Omitting either key is a
   config-load error — no implicit defaults.
3. Every input/output is a typed slot (JSON-schema fragment).
4. Inputs may declare `required: true` (default false) and `default:`.

### 5.2 Scoped capability blackboard

A capability runs in its own private blackboard, populated from the
host's `use:.inputs` binding (§6). Capability-internal slots
(`vetter_findings`, `iteration_count`, …) never appear in the host
namespace. Only declared `outputs` propagate back, projected at the
paths the host's `use:.outputs` declares.

This is the slot-collision firewall. Two parallel invocations of the
same capability run in independent blackboards — no shared state, no
contamination.

**Scope of isolation.** Blackboard scoping is the ONLY isolation
guaranteed by this design. Resource-level isolation (connection pools,
caches, file handles, MCP client state) is the executor's
responsibility per SPEC §24. Capability authors MUST design as if
their internal state is fresh per invocation; cross-invocation state
(e.g., an LRU cache shared across runs) belongs in scripts or
connections, not in capabilities.

### 5.3 Runtime output validation

When a capability completes, the runtime validates every declared
output against its declared schema before projecting into the host
blackboard. A capability that produces `"verdict": "approved"` against a
schema declaring `enum: [pass, fail, needs-revision]` fires a
`cap.output.schema_violation` audit event with the full diff. The
flow can route the post-cap transition via a guard
(e.g., a `cap_error` self-loop with `recovery-escalation`).

**Unconditional enforcement.** Runtime output validation MUST be
unconditional: no `cfg!(debug_assertions)` gating, no environment-
variable toggle, no feature flag. A validation failure causes the
executor to return `ExecutorError::SchemaViolation` and the transition
fails closed — no partial outputs are projected into host slots. The
acceptance test `tests/cap_output_violation.rs` is required (see
§12.4 M3) and exercises a deliberately bad capability to assert the
audit event fires and the host blackboard remains untouched.

### 5.4 Authoring guidance

Capability shape, vocabulary, and quality conventions belong in
cognitive-architectures `CONTRIBUTING.md`, not in the praxec
runtime. A capability with 6 inputs isn't broken, just smelly; code
review catches smelly. The runtime enforces correctness
(contract-typing, slot reachability, verb-subject consistency,
no-nesting); the library style guide enforces taste.

### 5.5 Capability failure semantics

When a capability terminates abnormally (any executor error inside
its internal state machine, including but not limited to schema
violation, MCP transport failure, script non-zero exit, HITL timeout),
the host transition completes with a `cap.error` outcome:

- A structured audit event `cap.terminated` is emitted with the error
  kind, the capability's terminal internal state, and the parent
  correlation_id.
- NO partial outputs are projected into the host blackboard. Host
  slots that the `use:.outputs` block would have written remain
  untouched (unwritten if they did not previously exist; previous
  value retained if they did).
- The host flow's transition is treated as failed for routing
  purposes. The flow MAY declare guard expressions on
  subsequent transitions referencing `$.last_executor.error_kind` to
  route recovery (existing SPEC §29 error-routing semantics). There
  is NO implicit retry.

Worked example: a `cap.implement.tdd-loop` that exhausts
`max_iterations` without convergence emits `cap.terminated` with
`error_kind: tdd_no_convergence`. The flow can declare a
recovery transition:

```yaml
transitions:
  retry_with_smaller_scope:
    target: replanning
    actor: deterministic
    guards:
      - expr: "$.last_executor.error_kind == 'tdd_no_convergence'"
```

---

## 6. Cross-workflow invocation

### 6.1 The `use:` binding

Flows invoke capabilities through the existing `kind: workflow`
executor, extended with a `use:` block:

```yaml
states:
  vetting:
    transitions:
      run_vet:
        target: signoff
        actor: deterministic
        executor:
          kind: workflow
          definitionId: cap.plan.vet
          use:
            inputs:
              plan: "$.context.draft_plan"
              max_iterations: 3
            outputs:
              "$.context.vet_verdict": verdict
              "$.context.vet_findings": findings
```

The `use:` binding does three things:

1. Validates host's `inputs:` JSON paths resolve to slots whose types
   match the capability's input schema (load-time check; see §7).
2. Runs the capability in a fresh blackboard, populated from `inputs:`.
3. On completion, projects declared outputs back into the host
   blackboard at the paths on the left-hand side. Each projection is
   typed: the host slot inherits the capability's declared output type.

A `kind: workflow` executor targeting `cap.*` without a `use:` block is
a config-load error.

### 6.2 Optional contract-hash pinning

For operators who want strict version safety, a `use:` block may pin
the target capability's contract hash:

```yaml
executor:
  kind: workflow
  definitionId: cap.plan.vet
  expects_contract_hash: "sha256:f3a1…"
  use: { ... }
```

The contract hash is computed at config-load from the capability's
`snippet:` block alone (inputs + outputs schemas, sorted-key
canonicalization). It is surfaced by `praxec.query({ subject: … })`. If the actual
hash differs from the expected hash, config-load errors with both
values.

**Pinning is MANDATORY for `stable`-lifecycle targets.** A `use:` block
referencing a capability declared `lifecycle: stable` (existing SPEC
§22 lifecycle promotion discipline) MUST include
`expects_contract_hash:`. Config-load errors on missing hash for a
stable target, with the current hash inlined in the error message so
the author can paste it:

```
error: flow flow.add-feature state `vetting` references
stable capability swe/cap.plan.vet without expects_contract_hash.
Add: expects_contract_hash: "sha256:f3a1…"
```

Pinning remains optional for `experimental`-lifecycle targets — those
are expected to churn.

### 6.3 Actor and correlation_id semantics in nested capabilities

The host transition's `actor:` field describes who initiated the
capability invocation (the agent acting at that flow state),
not who runs the capability's internal transitions. A capability's
internal transitions use the `actor:` declared on each internal
transition.

Correlation_id chaining is hierarchical:

- The host transition emits a `transition.fired` audit event with its
  own `correlation_id` (CID-H).
- The capability invocation emits a `cap.invoked` audit event with
  `correlation_id` CID-C and `parent_correlation_id: CID-H`.
- Each of the capability's internal transitions emits its own
  `transition.fired` event under CID-C.
- `cap.terminated` or `cap.completed` closes the capability scope
  under CID-C and links the outputs back to CID-H.

`praxec.query({ subject: … })` renders the parent/child correlation_id
chain so audit traces are walkable from flow to capability internals.

---

## 7. Host blackboard typing

The host flow's `$.context.*` slots are typed. This is what
makes chained capabilities composable: an author writing state F can
see exactly which typed slots are available because every preceding
write declared them.

### 7.1 Flow `inputs:` block

Flows declare their entry signature — the slots provided by the
initial call to `praxec.command` (start form):

```yaml
workflows:
  - definitionId: flow.add-feature
    inputs:
      feature_brief:
        type: string
        required: true
      base_ref:
        type: string
        default: main
      lexicon:
        type: object
        default: {}
    initialState: drafting_plan
    states: [...]
```

This is the ONE place where typed slots cannot be inferred from a
capability's outputs — they enter from outside. Every other typed slot
comes from a `use:.outputs` declaration.

### 7.2 Slot-table construction

At config-load, praxec builds a per-flow slot table:

1. Seed the table with the flow's `inputs:` block. Each declared
   input becomes `(path, type, source: input)`.
2. Walk every state and every transition. For each transition whose
   executor has a `use:.outputs` block, add one entry per output:
   `(host_path, capability.outputs[output_name].type, source: state(<state_id>))`.

The table is **flat** — no graph walk, no topological ordering. Slots
are typed by their declared write site.

### 7.3 Validation (load-time)

Two checks run against the slot table:

**Check A — Reachability.** For every transition's `use:.inputs` block,
every RHS JSON path (`$.context.X`) must resolve to a slot in the
table. If not, error:

```
flow.add-feature: state `vetting` transition `vet` references
$.context.draft_plan, which is never written by any state and is not
declared in `inputs:`.
```

This catches the silent-undefined-slot class entirely.

**Check B — Type consistency.** If two states both write to the same
host slot path (e.g., two states both set `$.context.verdict` via
`use:.outputs`), their declared output types MUST be the same schema
(structural equality on the JSON-schema fragment). If different, error:

```
flow.foo: $.context.verdict is written by state `vet` (string,
enum: [pass, fail]) and state `re_review` (string, enum: [approved,
rejected]) with incompatible types.
```

Resolve by renaming one of the slots or making both write the same
union type.

### 7.4 Cycle safety

State graph cycles (TDD loops, revise-and-retry) do not participate in
type inference. A slot written inside a loop is typed at its write
site; downstream references resolve against the slot table without
regard to graph topology. Loops do not cause inference ill-definedness.

### 7.5 Discoverability (future, in TUI)

`praxec.query({ subject: … })` exposes the per-flow slot table. The TUI's
state authoring view can render "slots available at state F" by
filtering the table to writes from states reachable in the state graph
from initial state to F (a graph reachability query, well-defined for
any graph including cyclic ones). This is a future TUI improvement; not
in scope for this spec.

---

## 8. The pokayoke rule: one level of indirection

The only standalone pokayoke rule. (Verb-subject consistency and the
snippet contract requirements are consequences of §3.3 and §5.1
respectively; they are not separate rules.)

| From | May invoke | May NOT invoke |
|---|---|---|
| Flow (`flow.*`) | capabilities, scripts, MCP tools, skills, HITL gates | other flows, itself |
| Capability (`cap.*`) | scripts, MCP tools, skills, HITL gates | other capabilities, flows |

**Check:** walk every workflow's transitions. For each executor with
`kind: workflow`, look at the host workflow's id and the target's id:

- Host id `cap.*` + any `kind: workflow` target → error
  ("capability cannot invoke another workflow").
- Host id `flow.*` + target id `flow.*` → error
  ("flow cannot invoke another flow").
- Host id `flow.*` + target id `cap.*` → OK.

Indirect cycles via MCP tools that re-enter the gateway are out of
scope for this static check — they are caught at runtime by the
existing SPEC §26 caps (`max_iterations`, `max_fires_per_visit`,
`max_recursion_depth`).

---

## 9. Multi-repo loading

### 9.1 Repo manifest

Each resource repo ships a `praxec.repo.yaml` at its root:

```yaml
# praxec.repo.yaml
schema: praxec.repo/v1
name: swe-core
version: 0.3.0
namespace: swe
layout:
  capabilities: capabilities/
  flows: flows/
  skills: skills/
  scripts: scripts/
  connections: connections/
description: |
  Core SWE capabilities + lifecycle flows for plan-driven
  feature delivery, bugfix-from-error-log, safe-refactor.
```

`schema`, `name`, and `namespace` are required. `layout` keys default
to the directory names above; declare only the ones that differ.
`version` is required; it informs the contract-hash provenance and
deprecation handling.

### 9.2 Gateway config `repos:` field

The gateway config gains a top-level `repos:` field:

```yaml
# gateway-config.yaml
version: "1.0.0"
repos:
  - path: ~/repos/swe-core
  - path: ~/repos/security-pack
  - path: ~/repos/perf-toolkit
include:
  - ./local-overrides.yaml
```

Loading order:

1. For each repo path, load `<path>/praxec.repo.yaml` and validate.
2. For each declared layout directory, glob `*.yaml`, load each file,
   merge into the gateway's workflow / skill / script / connection sets.
3. Prefix every loaded `definitionId` with `<namespace>/`. `swe-core`'s
   `cap.plan.vet` registers as `swe/cap.plan.vet`. Same for skills and
   scripts.
4. Repos load in declaration order.
5. Host `include:` files load LAST so the operator can override
   anything from the repos.

### 9.3 Cross-namespace references

| Reference shape | Resolves to |
|---|---|
| `cap.plan.vet` (unprefixed) | a capability in the CURRENT namespace |
| `swe/cap.plan.vet` (prefixed) | `cap.plan.vet` from the `swe` namespace |
| `swe/sk.plan.specify.change-request` | skill, prefixed |
| `swe/sc.verify.workspace.green` | script, prefixed |

**Strict resolution.** Unprefixed refs MUST resolve to the current
namespace. If the unprefixed name does not exist in the current
namespace, config-load errors — there is no fallback search across
other namespaces. This prevents silent cross-repo misresolution when
two repos happen to define same-named capabilities.

### 9.4 Collision rules

- **Two repos declaring the same `namespace`** → config-load error.
- **Two namespaces both defining `cap.plan.vet`** → no collision (fully
  qualified ids differ: `swe/cap.plan.vet` ≠ `quality/cap.plan.vet`).
- **Same repo defining the same id twice** (e.g., two files both declare
  `definitionId: cap.plan.vet`) → config-load error.
- **Host `include:` overriding a repo-provided id** → allowed ONLY
  when accompanied by an explicit `overrides:` declaration listing
  the fully qualified ids being shadowed:

  ```yaml
  # local-overrides.yaml
  overrides:
    - swe/cap.plan.vet     # explicit shadowing declaration
  workflows:
    - definitionId: swe/cap.plan.vet
      ...                  # operator's customized version
  ```

  Anonymous shadowing — defining `swe/cap.plan.vet` in `include:`
  without listing it in `overrides:` — is a config-load error. This
  closes the supply-chain backdoor: an operator cannot silently shadow
  a vendored capability with a different contract.
  `praxec.query({ subject: … })` surfaces every override and contract-hash
  diff at startup.

---

## 10. Worked example — `flow.add-feature`

Demonstrates: flow `inputs:` block, capability invocation with
`use:` bindings, sub-loop for TDD inside `cap.implement.tdd-loop`, HITL
gate as a `cap.gate.*` capability, deterministic verification, PR
creation as a `cap.coordinate.*` capability.

```yaml
# flows/add-feature.yaml
version: "1.0.0"

workflows:
  - definitionId: flow.add-feature
    inputs:
      feature_brief: { type: string, required: true }
      base_ref:      { type: string, default: main }
      lexicon:       { type: object, default: {} }
    initialState: drafting_plan
    description: |
      Plan-driven feature delivery: draft → vet → human signoff →
      TDD implementation → gap reconciliation → verify → review → PR.
    states:
      drafting_plan:
        goal: Produce a structured implementation plan from the brief.
        transitions:
          draft:
            target: vetting_plan
            actor: agent
            executor:
              kind: workflow
              definitionId: cap.plan.draft
              use:
                inputs:
                  brief:   "$.context.feature_brief"
                  lexicon: "$.context.lexicon"
                outputs:
                  "$.context.draft_plan":      plan
                  "$.context.draft_artifacts": artifacts

      vetting_plan:
        goal: Adversarial review of the draft plan.
        transitions:
          vet:
            target: awaiting_signoff
            actor: deterministic
            executor:
              kind: workflow
              definitionId: cap.plan.vet
              use:
                inputs:
                  plan:           "$.context.draft_plan"
                  max_iterations: 3
                outputs:
                  "$.context.vet_verdict":  verdict
                  "$.context.vet_findings": findings
            guards:
              - expr: "$.context.vet_verdict == 'pass'"
          revise:
            target: drafting_plan
            actor: deterministic
            guards:
              - expr: "$.context.vet_verdict == 'needs-revision'"

      awaiting_signoff:
        goal: Human approves the vetted plan before implementation.
        transitions:
          signoff:
            target: implementing
            actor: human
            executor:
              kind: workflow
              definitionId: cap.gate.human-signoff
              use:
                inputs:
                  artifact: "$.context.draft_plan"
                  prompt:   "Approve plan for implementation?"
                outputs:
                  "$.context.signoff_decision": decision
            guards:
              - expr: "$.context.signoff_decision == 'approved'"

      implementing:
        goal: TDD implementation against the approved plan.
        transitions:
          tdd_loop:
            target: tracking_gaps
            actor: agent
            executor:
              kind: workflow
              definitionId: cap.implement.tdd-loop
              use:
                inputs:
                  plan:        "$.context.draft_plan"
                  scope_paths: "$.context.draft_plan.scope_paths"
                outputs:
                  "$.context.implementation_result": result
                  "$.context.tests_added":           tests_added

      tracking_gaps:
        goal: Compare implementation to plan; identify deltas.
        transitions:
          track:
            target: verifying
            actor: agent
            executor:
              kind: workflow
              definitionId: cap.plan.track-gaps
              use:
                inputs:
                  plan:   "$.context.draft_plan"
                  result: "$.context.implementation_result"
                outputs:
                  "$.context.gap_report": report
            guards:
              - expr: "$.context.gap_report.unresolved_gaps == 0"
          close_gap:
            target: implementing
            actor: deterministic
            guards:
              - expr: "$.context.gap_report.unresolved_gaps > 0"

      verifying:
        goal: Workspace-green deterministic gate.
        transitions:
          verify:
            target: reviewing
            actor: deterministic
            executor:
              kind: workflow
              definitionId: cap.verify.workspace-green
              use:
                inputs: {}
                outputs:
                  "$.context.verify_ok": ok
            guards:
              - expr: "$.context.verify_ok == true"

      reviewing:
        goal: Adversarial code review of the diff.
        transitions:
          review:
            target: opening_pr
            actor: agent
            executor:
              kind: workflow
              definitionId: cap.review.adversarial
              use:
                inputs:
                  diff_against: "$.context.base_ref"
                outputs:
                  "$.context.review_verdict":  verdict
                  "$.context.review_findings": findings
            guards:
              - expr: "$.context.review_verdict == 'approved'"

      opening_pr:
        terminal: false
        goal: Open the PR and report status.
        transitions:
          open:
            target: done
            actor: deterministic
            executor:
              kind: workflow
              definitionId: cap.coordinate.pr-open
              use:
                inputs:
                  title: "$.context.draft_plan.title"
                  body:  "$.context.draft_plan.summary"
                  base:  "$.context.base_ref"
                outputs:
                  "$.context.pr_url": url

      done:
        terminal: true
```

The flow uses **eight capabilities** (`cap.plan.draft`,
`cap.plan.vet`, `cap.gate.human-signoff`, `cap.implement.tdd-loop`,
`cap.plan.track-gaps`, `cap.verify.workspace-green`,
`cap.review.adversarial`, `cap.coordinate.pr-open`) — all leaves. The
flow never invokes another flow. Internal to
`cap.implement.tdd-loop` there is a TDD red-green-refactor self-loop,
but that's the capability's internal shape, not visible at the
flow level.

---

## 11. Validation surface

All checks run at config-load. Hard errors abort startup; warnings
print but allow startup.

| # | Check | Tier | Outcome | Detection point |
|---|---|---|---|---|
| V1 | `verb:` is one of the 24 | cap | error | load |
| V2 | `definitionId` matches `cap.<verb>.<name>` | cap | error | load |
| V3 | `snippet:` block present | cap | error | load |
| V4 | `snippet:` block has BOTH `inputs:` AND `outputs:` keys (may be `{}`) | cap | error | load |
| V5 | Each input/output is JSON-schema-shaped | cap | error | load |
| V6 | Primary-executor verb-shape check (§4.1) | cap | error | load |
| V7 | `definitionId` matches `flow.<name>` | flow | error | load |
| V8 | `snippet:` block absent | flow | error | load |
| V9 | `verb:` absent | flow | error | load |
| V10 | Capability invokes another workflow | cap | error | load (Rule 1) |
| ~~V11~~ | ~~Flow invokes another flow~~ — **RELAXED**: flows MAY now invoke other flows (nested-flow composition, e.g. the loom). Recursion bounded by the runtime sub-workflow `max_depth` cap; a nested flow types its `use.outputs` from a top-level `outputs:` block (harvested alongside cap `snippet.outputs`). | flow | (allowed) | — |
| V12 | `kind: workflow` executor targeting `cap.*` without `use:` | both | error | load |
| V13 | `use:.inputs` paths resolve to slot table (Check A) | both | error | load (§7.3) |
| V14 | `use:.outputs` writes to a slot already typed differently (Check B) | both | error | load (§7.3) |
| V15 | `expects_contract_hash` matches actual | both | error | load |
| V16 | `use:` block omits `expects_contract_hash` for `stable`-lifecycle target | both | error | load |
| V17 | Output value matches declared output schema | cap | runtime audit event `cap.output.schema_violation`; executor returns `SchemaViolation` | runtime |
| V18 | Capability abnormal termination → `cap.terminated` audit event, no partial output projection | cap | runtime audit event | runtime |
| V19 | Repo manifest schema valid | repo | error | load |
| V20 | Two repos with same `namespace` | gateway | error | load |
| V21 | Duplicate `definitionId` in one repo | repo | error | load |
| V22 | Unprefixed cross-repo ref | gateway | error | load |
| V23 | `include:` shadows a repo-provided id without `overrides:` declaration | gateway | error | load (§9.4) |

`praxec check --config <path>` exposes all load-time checks for
CI use.

---

## 12. Rollout

### 12.1 Changes in `praxec` (3 PRs)

**PR1 — Multi-repo loading (~250 LOC).** `praxec.repo.yaml` schema +
loader, glob-driven directory loading, `repos:` config field, namespace
prefixing of loaded ids. Crate: `praxec-core::config`. No
downstream consumers required.

**PR2 — Snippet contract + scoped blackboard + `use:` bindings (~350 LOC).**
Workflow schema additions (`snippet:` block on `cap.*`, `inputs:` block
on `flow.*`), executor changes to scope the capability's blackboard and
project outputs back through `use:.outputs`. Crates:
`praxec-core::validate`, `praxec-executors`. Depends on
PR1's infrastructure but not its semantics.

**PR3 — Pokayoke rule + verb additions + slot-table validation (~150 LOC).**
Tier check (Rule 1), 24-verb cloud (add `gate` + `coordinate`),
slot-table construction (§7.2) + reachability check (Check A) +
type-consistency check (Check B), `expects_contract_hash:` pinning
(optional for experimental, mandatory for stable per §6.2), runtime
output validation (V17) + abnormal-termination handling (V18),
primary-executor verb-shape check (§4.1), explicit `overrides:`
declaration check (V23). Crates: `praxec-core::validate`,
`praxec-core::lexicon`. Depends on PR2.

Estimated total: ~700 lines of additive change in praxec. SPEC
updates required for new verbs and two-tier model.

### 12.2 cognitive-architectures v0.6 release

The library is restructured as the canonical first resource repo:

- Add `praxec.repo.yaml` declaring `namespace: cognitive`, version,
  and layout.
- Reorganize into `capabilities/`, `flows/`, plus the existing
  `skills/`, `scripts/`, `connections/` per §3.2.
- Existing demos (`tdd.yaml`, `vet-plan.yaml`, `parallel-review.yaml`,
  …) recast as capabilities under the `cap.<verb>.<name>` convention.
  The existing `swe-agent.yaml` becomes the basis of
  `flow.add-feature` (refactored to use the typed `use:` bindings).
- Ship the four lifecycle flows (§12.6) and the ~22
  capabilities they collectively require. The set MUST be complete
  and runnable at release — no flow may reference a
  capability that isn't shipped.
- Update `README.md` and `MATTPOCOCK-COVERAGE.md` to reference the
  two-tier model and the new lifecycle flows (this is the
  point at which the library claims "composable, not just per-skill"
  parity).
- Add a verb-appropriateness CI lint to `scripts/validate.sh` for
  library-side richer checks (the per-cap primary-executor check is
  in praxec runtime per §4.1; the library's lint catches the more
  nuanced "is this REALLY the right verb for this body" cases).
- Add a CONTRIBUTING.md section covering capability authoring style:
  ≤5 I/O guidance, naming, verb selection, scope discipline.

### 12.3 Initial release (v0.6)

praxec is pre-1.0; the v0.6 release is the first version with
the capability/flow model and is the canonical baseline. No
deprecation grace period and no migration shim for pre-v0.6 workflow
shapes — every workflow loaded in v0.6 MUST be `cap.*` or `flow.*`
shaped and is subject to all V1–V23 validation rules. CHANGELOG names
v0.6 as the introduction of the two-tier model.

### 12.4 Acceptance milestones

Implementation is divided into five accept/reject milestones. Each
milestone has a SINGLE binary acceptance test that the implementing
agent cannot shortcut. A milestone is "done" only when its acceptance
test passes against the merged code on `main`.

| # | Milestone | Acceptance test | PR |
|---|---|---|---|
| M1 | Multi-repo loading | `tests/multi_repo_loading.rs::two_repos_with_distinct_namespaces_load_both_capabilities` — loads two fixture repos and asserts both prefixed ids are reachable via `praxec.query({ subject: … })`. Also asserts duplicate-namespace fixture errors at load. | PR1 |
| M2 | Scoped capability blackboard + `use:` bindings | `tests/walk_examples.rs::scoped_capability_io_roundtrip` — runs a host flow that invokes a capability whose internal blackboard sets a "secret" slot; asserts the secret slot is NOT present in the host blackboard post-cap and that declared outputs ARE projected at the host paths declared in `use:.outputs`. | PR2 |
| M3 | Validation rule coverage | `tests/validation_rules.rs` — one positive + one negative test per validation rule V1–V16 + V19–V23 (the load-time rules). Plus `tests/cap_output_violation.rs` for V17 and `tests/cap_terminated.rs` for V18. Test count parity enforced per §12.5. | PR3 |
| M4 | Library completeness — end-to-end flows | `crates/praxec-core/tests/flow_flows_e2e.rs` runs each of the four shipping flows (§12.6) against a fixture cognitive-architectures repo and asserts each transitions correctly through its full lifecycle to its terminal state. | cognitive-architectures release PR |

A milestone is NOT considered complete on the basis of "the
implementation looks right" or "manual smoke test passes." The named
test must exist, be CI-wired, and pass. PR descriptions MUST cite the
milestone(s) they advance and link to the new test files.

### 12.5 TDD coverage parity rule

Every validation rule in §11 (V1–V23) MUST have at least one positive
test (rule accepts a valid input) AND at least one negative test
(rule rejects a specifically crafted bad input) in
`crates/praxec-core/tests/`. Test naming convention:

```
<rule_id>_accepts_<good_case>
<rule_id>_rejects_<bad_case>
```

For example:

```
v6_accepts_cognitive_verb_with_mcp_executor
v6_rejects_cognitive_verb_with_only_script_executor
v16_rejects_stable_target_without_contract_hash
v16_accepts_stable_target_with_contract_hash
```

CI enforces the parity invariant: count of test functions matching
the convention must be ≥ 2 × 23 = 46, with at least one accepts and
one rejects per rule id V1 through V23. The PR introducing a rule
must introduce the test pair in the same commit. A PR that adds a
validation rule without the corresponding accepts/rejects pair fails
CI.

For runtime rules (V17, V18) the same parity applies to integration
tests in `tests/`, not unit tests.

### 12.6 Library deliverables (cognitive-architectures v0.6)

The library MUST ship with four lifecycle flows at v0.6
release, covering the main inbound surfaces of an engineering team.
Each flow is publicly invokable, has e2e test coverage (M4),
and ships with the capabilities it requires.

| Flow | Lifecycle | Trigger | Capabilities required |
|---|---|---|---|
| `flow.add-feature` | Plan-driven feature delivery | Feature brief from operator | `cap.plan.draft`, `cap.plan.vet`, `cap.gate.human-signoff`, `cap.implement.tdd-loop`, `cap.plan.track-gaps`, `cap.verify.workspace-green`, `cap.review.adversarial`, `cap.coordinate.pr-open` (the eight from §10) |
| `flow.bugfix-from-error-log` | Incident response | Error log / stack trace | `cap.diagnose.parse-error`, `cap.diagnose.reproduce`, `cap.diagnose.localize`, `cap.plan.fix`, `cap.implement.scope-bounded`, `cap.verify.regression-tests`, `cap.review.adversarial`, `cap.coordinate.pr-open` |
| `flow.safe-refactor` | Code-health | Scope description (paths or component) | `cap.research.context-assemble`, `cap.refactor.draft`, `cap.tests.baseline-snapshot`, `cap.implement.scope-bounded`, `cap.tests.compare-baseline`, `cap.review.adversarial`, `cap.coordinate.pr-open` |
| `flow.triage-issue` | Intake | New issue / ticket | `cap.triage.classify-severity`, `cap.triage.route-component`, `cap.gate.human-disambiguate` (HITL on ambiguity), `cap.coordinate.label-and-route` |

Shared capabilities across flows (e.g., `cap.review.adversarial`,
`cap.coordinate.pr-open`) are defined once and referenced by all
consumers — this is the composition the two-tier model exists to
enable.

Capabilities total at v0.6: ~22 distinct (eight unique to add-feature,
six new to bugfix, five new to refactor, three new to triage, with
adversarial-review and PR-open shared across the first three).

Additional flows (`flow.security-review`, `flow.docs-sync`,
`flow.dependency-upgrade`, `flow.incident-postmortem`,
`flow.architecture-review`, `flow.prd-to-issues`,
`flow.test-coverage-improvement`) are scoped for v0.7+ as community
contributions or follow-up library work.

---

## 13. Open questions

- **Lexicon scope across repos.** Single shared lexicon for v1 (matches
  current SPEC §30). If repos contribute terms with conflicting
  definitions, the operator resolves via host `include:` override
  today. Per-repo lexicons are a possible follow-up if collisions
  surface in practice.
- **TUI slot-dictionary UX.** §7.5 sketches the goal; the implementation
  belongs in a TUI design follow-up after the runtime PRs land. Not
  scoped here.
- **Contract-hash canonicalization.** PR3 will pick a canonical
  JSON-schema serialization (likely sorted-key + `serde_json`'s
  `to_string`); the exact algorithm is implementation-detail for that
  PR but operators relying on hash stability across praxec versions
  need it documented in the SPEC update.

---

## 14. Future work (explicitly deferred)

- `extends:` parameterization (praxec G1). Once that lands,
  capabilities gain typed parameters and multi-instance composition.
  This spec is forward-compatible: a parameterized capability still has
  a `snippet:` contract; `extends:` adds a `params:` block alongside.
- Package manager (lockfile, version resolution, cached clones). Today
  operators manage repo versions through git directly (clone a tag, pin
  a commit). The `expects_contract_hash` mechanism gives per-reference
  pinning without a lockfile.
- Auto-import from mattpocock-style skill directories. The existing
  `ingest` executor adapts those; whether the imported artifacts surface
  as capabilities or skills depends on shape — out of scope for this
  spec.
