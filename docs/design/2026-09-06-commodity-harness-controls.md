# Commodity-model harness: controls and evidence

Updated 2026-09-06. This reconciles the July evidence-gated-boundary design with
current source. Quality is the constraint; speed and total accepted-task cost
are measurements. A successful model call is a candidate, not a verified task.

## Criteria and enforcement

| Criterion | Control | What it does not establish |
| --- | --- | --- |
| Concrete task and usable context | Existing consuming JSON Schemas; ordinary agent dispatch rejects unresolved goal paths by default; empty goals fail. The bounded recipe requires nonblank task, context, acceptance. | That supplied source is current, accurate, or sufficient. Include current implementation and relevant design; distinguish historical proposals. |
| Typed handoff | Existing input/output schemas and HOP contracts. Use `required`, declared types, `minLength` plus `pattern: '\S'`, and enums for closed decisions. | Semantic correctness of arbitrary strings, citations, or paths. Do not create a competing handoff envelope for the same contract. |
| Bounded execution | Existing per-attempt, chain, tool-setup, cumulative hop and active-time limits; bounded recipe sets attempt/chain limits explicitly. | A dollar ceiling. A token price cap is also not a total task budget. |
| Useful continuation | New opt-in state `continuation.reads`; compare exact declared evidence before auto-drive, persist across successful hops/restarts, quarantine unchanged slices. | Semantic progress, automatic selection of meaningful fields, external-agent enforcement, or exactly-once execution. |
| Scoped effects | Existing explicit tool lists, owned-file controls, worktrees, and repository push policy. Tool-free recipe cannot mutate source. | Arbitrary external tools honoring scope, or whole-system effect confinement. Review each effectful connection. |
| Independent acceptance | Existing deterministic verifier guards and outcomes for task completion; new attributed human acceptance provenance for optimizer statistics. | Human attestation being correct, or model/self-reported success proving correctness. |
| Honest accounting | Successful direct attempts counted once alongside auto-drive completions; failed spend retained; missing usage stays unknown; effort preserved per chain occurrence. | Provider-invoiced cost, coordinator cost outside the audit, or task-level productivity savings. |
| Reversible optimization | Proposals remain proposals; preserve task class, workflow revision, model/effort, verification, rework, latency and costs before changing routing. | Controlled comparison or causal quality equivalence from observational pass rates. |

## Continuation contract

```yaml
states:
  repairing:
    continuation:
      reads:
        - $.context.artifact_digest
        - $.context.verifier_issues
        - $.context.selected_model_tier
    # Existing goal, skills and agent transition follow.
```

Every path must resolve; explicit null is allowed. Paths must select concrete
public context/input fields. Select stable diagnostic data or a deterministically
computed artifact digest. Counters, timestamps, random IDs, and model-authored
claims of progress can defeat a poorly chosen read slice. The engine cannot infer
which inputs represent meaningful progress for every workflow.

The snapshot is engine-owned and updated after successful auto-driven hops.
Unchanged inputs produce `chain.quarantined` with `continuation_unchanged`, before
another model call. Failed executor calls do not consume the slice; existing
bounded reliability/model-chain retries still apply. Repeated external re-drives
after failure require their own policy. A crash between model execution and
snapshot commit can replay a call. This is not an exactly-once guarantee.

## Acceptance for model optimization

An executor-free human transition may declare:

```yaml
accept:
  actor: human
  target: accepted
  model_acceptance:
    producer_transition: draft
    verdict: accepted
  inputSchema:
    type: object
    required: [producer_event_id, evidence]
    properties:
      producer_event_id: {type: string, minLength: 1}
      evidence: {type: string, pattern: '\S'}
```

Submit the latest successful producer audit event ID and verification evidence
through an authenticated human channel. The runtime checks attribution, freshness,
duplicates and human identity before changing state, then emits
`agent.acceptance_recorded` after persistence. `rejected` records a negative
quality result. Direct successful attempts are supported, as are auto-drive
completions. Workflow, correlation, transition and producer event ID must match.

The embedding channel owns identity trust. Never give a model human credentials
or forge a human principal to fill optimizer statistics. Evidence is an attributed
attestation, not proof that checks ran. Current deterministic/model-only review
paths remain unverified for positive optimizer statistics; the bounded recipe's
caller acceptance is useful task evidence but does not impersonate this human gate.
Historical completions without provenance are excluded, not treated as failures.
Duplicate protection is conservative across same-transition repetitions sharing
a correlation. Missing audit provenance safely loses an optimization opportunity.

Cheaper-model proposals additionally require complete valid pricing, engine-derived
cohort fingerprints (definition snapshot, definition ID, state and transition),
the same cohort set and observed task mix, enough evidence in every cohort, and
no per-cohort acceptance regression. Older events without cohort provenance cannot
justify lowering. Snapshot changes intentionally start a new comparison cohort.
This controls cross-workflow mixing and missing-price bias; it does not match
individual task difficulty or include external coordinator and retry costs in
worker-call means. Use task-level evaluation before adopting any proposal.

A future deterministic acceptance path needs authenticated verifier identity,
artifact/version binding and recorded check results before it can supply equivalent
provenance. Dynamically generated acceptance-criteria recognizers and the L3
apply-strategy tool remain separate proposals, not shipped guarantees.

## Migration and validation

Ordinary dispatch now defaults `enforce_input_grounding` to true. A pack relying
on unset placeholders must fix its bindings or explicitly select false shadow
mode; doctor warns on that compatibility escape. Literal source that resembles an
unset marker is not rejected, and substituted source is not recursively rendered.
The existing untrusted/resume paths have separate contracts; this change does not
claim to retrofit every execution mode.

The July design and plan index now distinguish shipped controls from proposals.
Targeted regression tests cover missing inputs, literal source, effort ladders,
direct-attempt accounting, null usage, attribution, rejected/stale acceptance,
restart continuation, changed evidence and counter-only loops. Pack verifier tests
exercise actual shell behavior with controlled process outcomes. Mock workflows
prove gate behavior, not commodity-model quality.

A live Praxec design review was rejected: it exceeded the response contract and
used historical design prose to misstate current implementation. Verified retry
findings were retained. This is direct evidence for providing current source in
review handoffs and keeping model completion separate from accepted work.
