# Design: The Change Building Block — evidence-gated, deterministic-frame system mutation

Status: DRAFT for FMECA/poka-yoke/TRIZ vetting. Not approved for implementation.
Date: 2026-07-28.

## 1. Context & problem

A lead commodity coder (glm-5.2) reported success, burned ~$1.88/253s, and wrote **zero
files**. We shipped a runtime forcing function (`requires_file_write`) that catches a coding
agent signing off with no file-mutation evidence. It works. But subsequent dogfood surfaced the
*same shape* wearing different clothes:

- a surface **name** (`"organization-payment"`) reached the coder where **file paths** were
  expected (semantic-string confusion) → the coder could only STOP;
- the verify step discarded the real build error, so the retry loop fed the agent "build: fail"
  with nothing to fix → livelock to `AGENT_CHAIN_EXHAUSTED` (434s wasted).

Across every incident the disease is one thing: **an agent is handed something that isn't
usable truth, drives its own execution against it, and the system burns silently.** Patching
instances (we shipped two pack fixes, cognitive-architectures#65) treats symptoms. This design
targets the class.

### The recurring failure classes
1. **Narrated success** — claims done without doing it. *(patched by the forcing function)*
2. **Semantic-string confusion** — a value with the wrong *meaning* flows into a slot; the type
   system (`string==string`) can't see it.
3. **Context starvation** — agent/loop fed empty/useless context → livelock to budget exhaustion.
4. **Silent fallback** — an empty/ungrounded value silently becomes a default that hides the
   anomaly.
5. **Feedback-loop starvation** — a retry re-dispatches the agent with no new information.

## 2. The paradigm

The shipped forcing function gates the **exit** boundary of an expensive step (success needs
deterministic evidence). The reset: apply the same principle at the **entry** and
**continuation** boundaries, turning the blackboard from a stringly-typed mutable dict into a
**typed, provenance-aware dataflow** where every value an agent acts on is validated against
ground truth before a token is spent.

Three boundary gates (mirror images of the exit gate we already ship):
- **Entry** — no agent is dispatched on inputs that don't provably resolve to real referents
  (empty / name-as-path / `(unset)` stub → refused, zero tokens).
- **Continuation** — no loop iterates without new information (a retry is illegal unless its
  evidence slot changed).
- **Exit** — success needs evidence *(already shipped; becomes moot for the ops in §5, see §6).*

This is not greenfield: it realizes the already-staged "Gate1 evidence-binding" direction and
reuses existing spine (`templating.rs`'s `(unset)` stub is the smoking gun; `reads.rs` derives
read-sets; evidence guards fail-closed; `$optional`, snapshot-versions, the lexicon are
precedent).

## 3. The Change Building Block (the atom)

**A step that mutates the system is a micro-waterfall: `plan → execute → verify`, enforced as a
contract.** This is not a new engine concept — a praxec workflow already *is* a plan/execute/
verify state machine with guards, evidence, and HATEOAS. We turn that primitive inward and make
it the mandatory, **fractal** unit of a mutation:

```
mission  ▷ workflow of deliverables
 deliverable ▷ workflow of steps
  step   ▷ micro-waterfall: plan → execute → verify   (enforced seams, typed handoffs)
```

- **Scope:** the building block is the **mutation** primitive. A step that only reads/reports/
  decides does not go through it. A step where a model does judgment on a change does.
- **Only agentic steps recurse.** A deterministic step (a script, a git op) is already atomic.
- **Deterministic-by-default stages.** When the file-set is already known (CPM `owned_files`),
  the plan stage is a *deterministic projection*, not a model call. A model plan stage fires only
  when the step genuinely needs decomposition. This keeps the block a *structural* invariant
  without being a *cost* invariant (avoids the historical "ceremony failure" where mandatory
  sign-off steps failed the chain).
- **Composition → methodologies.** TDD is a *molecule*: a RED block (verify = "test exists and
  fails for the intended reason") then a GREEN block (verify = "that test flips red→green, nothing
  regresses"). The inter-block handoff (`a proven-failing test`) makes red-before-green a
  *contract on the seam*, not a convention a model can skip. Refactor-under-green, migration, and
  feature-slice are other molecules of the same atom.

### 3.1 Scope & generality (UNDER-TESTED — a key vetting target)

praxec orchestrates *diverse* workflows (triage, external-tool orchestration, review, elicitation,
planning), not just code. The design must state honestly what is general vs mutation-specific vs
file-specific — three layers of decreasing generality:

- **L1 — the paradigm (ALL workflows).** Evidence-gated boundaries (entry/continuation/exit) and
  **typed, validated handoffs instead of blackboard dumps** apply to *every* step. A triage step's
  classification is a handoff artifact that must be a member of a *closed label set* and grounded
  in the issue; a routing target must resolve to a real workflow. This layer is general.
- **L2 — the mutation building block (mutation steps only).** The `plan → execute → verify`
  micro-waterfall governs steps that *change* a system. **Non-mutating steps do NOT go through it.**
  Triage is `gather → classify → route`, not `plan → execute → verify`; its "verify" is that the
  decision artifact is well-typed and grounded (a valid label, an existing route), not a build.
  Triage may *terminate in* routing to a mutation block.
- **L3 — the file interpreter (file/code changes only).** The `ImplementationStrategy` enum + the
  `kind: change` interpreter (§5–6) are tuned for **filesystem** mutation and do not generalize
  as-is.

**External effects (3rd-party MCP mutations) — the frontier.** A step that mutates an *external*
system (post a Slack message, open a GitHub PR, write a DB row, drive a browser) IS a mutation, but
not a file one. It is a distinct operation family (`ExternalEffect`) governed by the **same
admissibility rule** (§5.3) — and that rule is exactly the `RunCommand` classifier generalized:
- **Admissible** when the effect is **read-back verifiable + idempotent**: the interpreter makes the
  structured call, then a deterministic state predicate confirms it (`message exists in channel`,
  `PR #N exists with head=X`, `row present`). Effect-scope = the external resource; containment is
  best-effort (often you cannot prove *nothing else* in the external system changed — state that
  limit honestly).
- **Inadmissible as a deterministic op** when the only witness is a fire-and-forget response or a
  human/model judgment → it routes to verify + human park-approval, **labeled unverified**, never
  masquerading as a checked op. (Same boundary the rule draws for `RunCommand` and migration
  data-safety.)

**Open question the FMECA must answer:** is "THE core building block" over-claimed? Should L2 be
demoted to "ONE building-block shape (mutation) among several (gather-decide-route,
external-effect)"? Is L3 an executor (`kind: change`) that is *useful* but not *essential*, when
the free-form `kind: agent` already exists? Phase-1 architecture-validity decides this, tested
against triage, external-MCP orchestration, and review flows — not just code.

## 4. The handoff contract (typed artifacts, validated against ground truth)

The incident was a **handoff failure** — the plan *had* the right `owned_files`; the executor
*could* write; the seam between them lost the truth (an ambient blackboard slot a verify-knob
clobbered). A blackboard slot has no producer contract, no consumer validation, no immutability.
The fix is to promote handoffs from *slots* to *typed artifacts with contracts*:

A handoff artifact has: a **named type** (with a recognizer, not `string`); a **single
authoritative producer** (write-once); **non-empty + grounded** (references resolve to real
things); **consumer-validated on entry** (the next stage refuses an ungrounded/empty artifact
*before* spending a token); **immutable + hashed** (so continuation can prove new information
crossed).

**Handoff validation is deterministic and runs against the codebase at the seam.** A plan naming
files that don't exist, or a hallucinated surface-name where paths belong, is an *invalid
handoff* — a typed refusal, zero cost. This is the entry-evidence gate applied to the plan
artifact, and it structurally kills the incident: `"organization-payment"` expressed as an
operation names paths that don't resolve → rejected before any executor runs.

## 5. The Execution Strategy contract (`Vec<ImplementationStrategy>`)

The plan→execute handoff is an **Execution Strategy**: a command-pattern enum that reifies
INTENT (describe the change; do not execute it). Operations are the vehicle for §4's contract.

### 5.1 The slot ladder (the load-bearing idea)
The generative slot handed to the model is not one thing — it's a ladder, and every op sits as
low (as constrained) as the change permits. **Slot width is a function of how much deterministic
machinery the frame owns** — investing in the frame deletes generative surface:

| Rung | Slot | Ops | Model returns | Placement risk |
|---|---|---|---|---|
| 0 | NULL | `DeleteFile`, `MoveFile`, `UpdateReferences` | nothing | none |
| 1 | PARTITION | `SplitFile`, `MergeFile` | assignment of *existing* bytes | none (system moves) |
| 2 | EDIT-OP | `ModifyFile` | anchored `{old→new}` | anchor-miss (fails closed) |
| 3 | CONTENT | `CreateFile` | whole artifact | none (path is system-owned) |

### 5.2 Operation families — axed on (effect-scope × conformance-predicate), NOT file-verbs
The shipped `implement.edit.constrained` skill already grew to 8 ops that aren't file-family —
evidence the file axis is wrong. Families:
- **File**: `CreateFile{path, purpose, content:Generated}`, `ModifyFile{path, anchor,
  replacement:Generated}` (anchor is interpreter-owned; unique-match checked *before* generation),
  `DeleteFile{path}`, `MoveFile{from,to}` (byte-preserving), `SplitFile`, `MergeFile`
  (byte-conserving; PARTITION slot).
- **Reference** (the load-bearing addition): `UpdateReferences{rename|moves, scope}` — index/LSP-
  backed, **discovers** the reference sites, rewrites deterministically, conformance = global
  predicate (`refs(old)==0 ∧ count conserved`). This is how the "ripple" (fix all importers) stops
  being model-diligence-and-hope and becomes a checkable receipt. Moves/renames/deletes' wiring
  consequences are *separate* `UpdateReferences` ops, not baked into the move.
- **Structured-data**: `DependencyChange`, `ConfigSet` (really one `StructuredEdit{format, keypath,
  value}` family — parse-set-reparse; conformance = `==`).
- **Gated Tool**: `Codemod`, `Migration` — admissible **only** if they carry a declared
  state-predicate postcondition / schema-delta as a **non-optional field** (an un-checkable one is
  *unconstructible* — V38/V39-style poka-yoke).
- **Rejected**: `RunCommand` — its only witness is an exit code, no bounded effect-scope. Its
  absence is a feature. "Run something" must be re-expressed as run-as-machinery + check-resulting-
  state, or escalated to a human/verify boundary and *labeled unverified*.

### 5.3 Admissibility rule (the classifier)
An operation kind is admissible only if it declares (a) a **deterministic conformance check** — a
predicate over introspectable **state**, never an exit code — and (b) a **two-sided effect-scope**
(positive: what must change; containment: `changed_set ⊆ scope`, nothing outside changed). The
rule *is* the classifier: it ejects `RunCommand`, gates `Codemod`/`Migration`, and honestly
reports its frontier — migration **data**-safety and cross-op parity (ORM↔schema) are NOT
single-op-checkable and route to verify + human park-approval.

### 5.4 Two-tier verification (the honest split)
"Deterministic conformance" for `ModifyFile` is tautological if it means intent — "the bytes I
said would change, changed" proves the op executed itself, not that the bug is fixed. So:
- **Structural op-conformance** — deterministic, per-op: the op applied as specified (patch applies
  to a pinned base-hash; anchor unique; byte-conservation; `refs==0`; effect-scope respected).
- **Intent verification** — behavioral, per-**slice**: compile + test + lint (with the real error
  surfaced, per #65). Cannot run per-op.
- **The real waterfall unit is the SLICE** (a coherent set of ops), not the individual op.

### 5.5 Interdependence
A flat `Vec` of pre-filled content values fails when op B's content depends on op A's *executed*
result. Resolution: content-dependent changes go in **sequenced blocks** (contract-block executes,
then impl-block plans against the real signature — the TDD shape); **ripples** are discovered-
effect ops (`UpdateReferences`), not pre-enumerated edits. Within a block the strategy is a bounded
*ordered* program (importers before deletes) when scopes are dependent, an unordered set when
disjoint. We do **not** build one mega-DAG with deferred closures.

## 6. The `kind: change` interpreter (engine fit)

A new `ChangeExecutor` (`kind: change`) registers as a peer to `kind: agent`. Its body is a
**deterministic interpreter of `Vec<ImplementationStrategy>`**, plugged in as the `edit` closure
of the existing trusted-promotion bridge (`promotion.rs::run_trusted_agent` already takes
`edit: FnOnce(PathBuf)->Fut`). Execution:

```
for strat in plan:
  strat.check_preconditions(tree)      # deterministic gate against the REAL disposable copy
  for slot in strat.pending_slots():
      value = generator.fill(slot, tree, intent)   # THE ONE model call site; None for Delete/Move
      strat.bind(slot, value)
  strat.check_generation(tree)         # deterministic conformance of the filled value
  strat.apply(tree)                    # the SYSTEM writes (fs::write / replacen / git mv)
  assert tree.touched() ⊆ strat.effect_scope()
# → capture_patch → promote (lock observed set, git apply --3way) → Applied|Conflict|Locked
```

The model is invoked at **exactly one site**, returns a **typed value only** (`FileBody` /
`SpanText` / `GlueEdits`) — no filesystem tools, no path, no "done" signal — validated before the
system applies it. Escalation happens **per generative slot** (a non-conforming fill escalates
down the model chain via the existing `Capability` path). Coupled slots (`Split`/`Merge`) type
their generation as a single `Generated<GlueEdits>` (one grouped call); everything else is
one-value-one-call. No "fill the whole plan" mega-call.

### Reuse / moot / replace
- **REUSE (unchanged):** promotion/sandbox candidate-patch flow; `owned_files` locks (assert
  `observed_files ⊆ effect_scope` as a pre-merge post-condition); path-escape safety
  (`resolve_under_no_symlink_escape`); the chain-walk/breaker/cost-gate/effort stack (the
  `SlotGenerator` runs single-shot completions through it).
- **MOOT (dead weight for these ops):** the entire coding-evidence forcing function
  (`CODING_WRITE_PROTOCOL`, `writes_seen`, `MAX_WRITE_NUDGES`, `NoFileWrites`), `sign_off_ceremony`,
  `salvage_result`, `force_final`, `COMPLETION_PROTOCOL` — narrated-write is structurally
  impossible when the system does the writing. (They remain for free-form `kind: agent`.)
- **REPLACE (for these ops only):** `write_file`/`edit_file` as a *model-facing* surface — the
  interpreter calls `fs` directly; `file_tools` survives for the free-form untrusted tier.

## 7. Constrained vs unconstrained — the TRIZ resolution

**Physical contradiction:** the model should be *free* (leverage the agentic loop it's trained on
→ effectiveness) AND *constrained* (deterministic frame → correctness). Resolved by separation:
- **In TIME:** unconstrained **plan** (full agentic exploration — read, search, reason, decide the
  strategy); constrained **execute** (interpreter drives; model fills narrow slots). The micro-
  waterfall *is* this separation.
- **On the slot ladder:** constrain PLACEMENT/ORCHESTRATION (the model's weakness — scope, paths,
  sequencing, remembering to write) and FREE GENERATION (its strength — write this file/span).
  **Ideal Final Result:** the model does only the irreducibly-generative part it's best at; the
  system does the chores it's worst at. The constraint removes failure-prone responsibilities, not
  creative ones.
- **By CONDITION:** keep BOTH modes (`kind: change` constrained; `kind: agent` free-form escape
  hatch for the exploratory tail); route by change-type; let the flywheel learn.

**We do not assume constrained wins everywhere.** Compiler-feedback iteration moves from turn-
level to block-level (re-plan with the real error). For some changes that may be worse. This is
**empirical**: A/B constrained vs the free-form baseline on real changes, measure success-rate ×
cost, let data decide the routing boundary.

## 8. Phased build

- **Phase 1 — kill the classes cheaply + prove the interpreter where it's pure upside.**
  (a) Fallible render / non-empty consume for agent bindings (kill starvation/silent-fallback at
  the root — one choke point); (b) the re-entry delta gate (kill feedback-starvation/livelock;
  subsumes the anti-livelock candidate; reuses `reads.rs`); (c) the fallback-ledger invariant
  (silence → validation error). Plus the `ChangeExecutor` + `Delete`/`Move`/`Create` (Delete/Move
  = zero model; Create = one simplest slot). Dogfood on `flow.refactor.god-file` **against the
  free-form baseline** to measure the effectiveness delta.
- **Phase 2 — extend the interpreter where measured to help.** `ModifyFile` (lift `edit_file`'s
  unique-match to a pre-generation precondition); `UpdateReferences` (index-backed); the
  `StructuredEdit` family; two-tier verify hardening; the handoff-validation gate as a first-class
  contract.
- **Phase 3 — the frontier, only if data supports.** Gated `Codemod`/`Migration` (with mandatory
  postconditions); `Split`/`Merge`; and the broader manifest/authority/brand typing if incidents
  still slip the earlier gates.

## 9. Honest edges / non-goals

- **Migration data-safety and ORM↔schema parity are not single-op-checkable** → verify + human
  park-approval. The contract *reports* this boundary; it does not pretend to certify it.
- **Not built now:** the untrusted-subprocess NoChanges hole (no cap uses it); referent brands as
  parametric types (closed enum or nothing); full provenance value-envelopes (audit already
  captures); auto-mining guards from failures (human-initiated); forward-only state ratchet
  (reachability lint instead); fleet-canary infra (`check --against-corpus` optional CI); dynamic
  budget-reallocation markets.
- **The two pack fixes (#65) stand** as the immediate instance patch; this design makes the class
  structurally impossible.

## 10. Grounding (existing spine this builds on)
`promotion.rs` (edit-closure seam, atomic 3-way promote, `observed_files` locks); `file_tools.rs`
(`edit_file` unique-match conformance to lift; write host to replace as a model surface);
`executor.rs` (chain-walk/breaker/cost-gate/effort to reuse in `SlotGenerator`); `ports.rs`
(`Executor` one-method trait for `kind: change`); the shipped forcing function (now moot for these
ops); V38/V39 (load-time forcing precedent); `$optional`, snapshot-versions, lexicon, `reads.rs`
(precedent + machinery).
