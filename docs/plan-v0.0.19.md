# plan — v0.0.19 (hardening)

> Status: **BUILT.** All five items shipped (PR #51 + companions praxec-meta #7,
> cognitive-architectures #15). Each traces to a real defect or fail-open path
> surfaced while driving `praxec fuzz` to fully green in the 0.0.18 dogfooding
> follow-up — nothing speculative. Per-item status is marked ✅ below.
>
> Outcome: V25/V26/V27 close the silent-scope class on both read and write sides;
> each has a mutation operator that must kill it (live-pack mutation report 100%
> across all twelve operators). V27 caught a real bug on first run (`plan_final:
> $.input.plan` dropping the operator's approved plan) plus two latent null-writes
> in praxec's own test fixtures.

## Theme

**Close the silent / fail-open gaps, and give every new invariant a mutation
operator.** The 0.0.18 follow-up proved the thesis the hard way: a gate you never
attack is a gate you only *assume* works. The fuzz was blind to `$ref` contracts;
the mutation score was inflated by a pre-existing baseline; a guard read an
unresolvable scope and silently evaluated to `null`. Each was invisible until
something forced it into the light. v0.0.19 turns those one-off catches into
standing guarantees.

## Backlog (dependency-ordered)

### H1 — V25: reject an unresolvable guard scope at load time  ✅ DONE

**The bug it generalizes.** `cap.gate.human-approve-plan` guarded on
`$.input.mode`. The guard evaluator (`guards.rs::resolve_operand`) resolves
`$.context.*`, `$.arguments.*`, `$.workflow.input.*`, and `$.workflow.{id,state,
version}` — and for anything else returns `Ok(Value::Null)` (line ~635,
**fail-open**). So `$.input.mode == 'auto'` was `null == 'auto'` → always false;
both transitions were dead and the cap wedged. It only surfaced because the fuzz
stopped masking it.

**The fix.** A load-time validator that walks every guard `expr` (and
`branches[].when`) and errors on any `$.`-rooted operand whose scope is not in the
resolvable set. The resolvable set is defined once in `resolve_operand`; V25
mirrors it (and a poka-yoke test asserts the two lists match, so they can't
drift). Fail at `praxec check`, not on the one run that takes the dead branch.

**Open decision.** Bare `$.input.*` *does* resolve in executor `args`/`output`
context (`resolve_output_operand` uses a generic `strip_prefix("$.")`), which is
why authors reach for it. Two ways to end the asymmetry:
  - (a) **Forbid** — V25 rejects `$.input.*` in guards; authors write
    `$.workflow.input.*`. Minimal, explicit.
  - (b) **Alias** — make the guard evaluator accept `$.input.*` as a synonym for
    `$.workflow.input.*`, so the two contexts agree. More forgiving, more surface.

Recommend **(a)** plus a V25 message that names the correct spelling — smallest
blast radius, and it teaches the canonical form.

### H2 — V26: warn on a scalar output written from an optional source  ✅ DONE

**The class.** V24 catches an output *never* written on some path. It does **not**
catch an output written from a source that can be *null*: a scalar
`snippet.outputs.summary: {type: string}` mapped from `$.arguments.summary`, where
`summary` is an optional (non-`required`, non-`default`) agent input. The
deterministic-repair rung coerces a missing array/object to `[]`/`{}` — but it
**cannot** repair a scalar, so an omitted optional lands `null` at terminal and
fails the contract. (This is exactly why the fuzz's per-edge probe had to supply
*all* arguments, not just required ones.)

**Scope.** A load-time check flagging a declared **scalar** output whose writer
sources it from an optional argument. Emit a warning (not an error) with the
one-line fix: mark the source `required`, give it a `default`, or make the output
nullable.

**Confirmed true-positive set** (exhaustive scan, 152 workflows, 51 declaring
scalar outputs — exactly **3** genuine hits; the two `result` candidates are
`type: object` and repairable, so correctly excluded):

| workflow | output (type) | writer → terminal | optional source |
|---|---|---|---|
| `cap.review.completeness` | `health_score` (integer) | `ready.submit_review → done` | `$.arguments.health_score` — not in `required: [verdict, findings]`, no default |
| `cap.review.analysis` | `summary` (string) | `analyzing.submit_analysis → done` | `$.arguments.summary` — not in `required: [findings]`, no default |
| `cap.implement.resolve-conflicts` | `resolution_summary` (string) | `resolving.submit_resolution → done` | `$.arguments.resolution_summary` — not in `required: [resolved_files]`, no default |

**Exact shape V26 must catch:** a declared **scalar** output → a transition
`output:` mapping `<out>: "$.arguments.<field>"` → `<field>` present in
`inputSchema.properties` but (a) absent from `inputSchema.required` **and** (b)
carrying no `default`. Zero `$.context.<unseeded>` cases exist in the pack, so the
first cut can scope to the `$.arguments` shape (context-slot sourcing is a
possible-future extension, not a current true positive). All three write straight
to the terminal, so the null lands at the tightest point.

Each of the three also has a trivial pack fix (add a `default`, or list the field
`required`) that ships alongside V26 so the rule is green on the live pack from
day one.

### H3 — mutation operators for V25/V26/V27  ✅ DONE (one per rule)

The standing lesson. Each new invariant gets an operator that *should* be killed
by it, so the mutation report proves the rule works rather than assuming it:
  - `retarget_guard_scope` — rewrite a guard's `$.workflow.input.x` →
    `$.input.x`. Must be KILLED by V25. (Today it would silently survive.)
  - `weaken_output_source_to_optional` — drop a `required`/`default` from an
    inputSchema property feeding a scalar output. Must be caught by V26.

Without these, H1/H2 are gates we're back to assuming work.

### H4 — runtime posture on an unresolvable guard scope  ✅ DONE (fail-fast)

Even with V25 at load time, `resolve_operand` still returns `Ok(Null)` for an
unknown scope at *eval* time — a fail-open path. Options: keep lenient (V25 is the
gate), or fail-fast at eval like `$.context.*` already does for an unset slot
(`UnsetSlotError`). Lean fail-fast per doctrine, but it's a behavior change worth
a deliberate call. Small; do it alongside H1.

### H5 — FMECA sweep for silent-null / fail-open operand paths  ✅ DONE (→ V27)

H1 found one. The systematic version: audit every operand/path resolver
(`guards.rs`, `mapping.rs`, `resolve_output_operand`) for `unwrap_or(Null)` /
`unwrap_or_default()` on a *reference* that should have resolved, and classify
each prevent → detect → fail-fast. Bounded, high-signal, matches the 0.0.12
prod-readiness pass's method.

## Non-goals for 0.0.19

- The **44 advisory smoke wedges** on agent-heavy flows are *not* a hardening
  target — a mock chooser cannot make an agent's decisions; that is what
  `--live --model` is for, and they are already excluded from the exit code.
- No new features. Hardening only: validators, mutation coverage, fail-fast
  posture. Anything that adds surface waits.

## Sequencing

H1 + H3(retarget_guard_scope) + H4 land together (one coherent "guard scope is
now checked, attacked, and fail-fast" change). H2 + H3(weaken_output_source) land
together once the survey pins the true-positive set. H5 is an independent sweep,
runnable in parallel. Every item ends the same way: `praxec check` on the live
pack is clean, `praxec fuzz` exits 0, and the mutation report shows the new
operator at 100%.
