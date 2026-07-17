# Requirement-Driven Resolution + RouterPolicy Pools (praxec spec #2)

**Status:** design, grounded in the current resolution spine (file:line cited)
**Date:** 2026-07-17
**Companion to:** `docs/specs/2026-07-16-multi-provider-load-distribution-design.md`
(this spec is the praxec **consumer** side; the reliability primitive shipped
separately as `execution-policy` 0.0.5 — `RouterPolicy` + `Pick` + `Meter`).

---

## 1. Summary

An agent/workflow should declare a **behavior requirement**, not a model id.
Resolution returns the best **binding** — `(provider, model@provider, account,
effort-params)` — and a capability maps to a **pool of members** that a
`RouterPolicy` (execution-policy 0.0.5) load-balances. Model identity becomes an
*output* of resolution; the model set behind a requirement is config that shifts
underneath without touching the agent.

The requirement is three facets plus hard constraints:

| facet | praxec type today | status |
|-------|-------------------|--------|
| **domain** (coding/reasoning/prose/…) | `Affinity` enum — `model_resolver/config.rs:27-95` | **reuse** |
| **tier** (frontier/standard/commoditized) | model `Tier` enum — `config.rs:133-162` | **reuse** |
| **effort** (deep/medium/fast) | — (only a per-call *knob*, not a key) | **net-new key** |
| hard constraints (budget cap, local-only) | `Priorities{budget_cap, local_only}` — `cockpit/src/priorities.rs:112-122` | **reuse** |

The heavy machinery already exists. This spec adds three narrow things and wires
them together; it does **not** reinvent ranking, effort application, or failover
classification.

## 2. Grounding — what exists vs what's net-new

**Exists (extend/reuse):**
- **Requirement key**: `ModelRef = Option<Affinity> × Option<Tier>`, parsed
  `affinity|tier|affinity-tier` — `walk.rs:38-88`. Specificity walk
  `walk.rs:164-204`.
- **Config**: `ModelsFile{default, overrides, activity}` — `config.rs:438-446`;
  `activity: BTreeMap<String, Vec<Binding>>` is an **open string-keyed chain**
  ("any activity its own escalation path without a core change", `config.rs:430-435`).
- **Best-model ranking (the recommendation lens)**: `suggest_by_value`
  (`value = fit ÷ blended_cost^β`, capability floor, marginal band) —
  `model_catalog.rs:179-240`; `suggest_for_needs` `:138-151`; `Stance` → `(β, ε)`
  — `priorities.rs:24-88`. Cost feedback: `cost_report.rs:62-117`,
  `deescalation.rs:119+` (governed `models.yaml` proposal, human gate).
- **Effort application**: `reasoning_effort` config on both executors
  (`llm-executor/src/config.rs:75`, `agents/src/config.rs:68`); level→native
  mapping `tuning.rs:144-193/338-351`; per-binding features
  (`config.rs:223-259`); runtime carrier `TurnRequest.reasoning` →
  `provider_factory.rs:238-240`. `ModelEntry.reasoning_levels`
  (`model_catalog.rs:44`) declares support.
- **Binding boundary**: `ModelResolver`/`AffinityResolver` traits return an
  **ordered `Vec<String>`** (`session.rs:177-191`, `affinity_resolver.rs:83/112`).
  The live escalation for-loop is `agents/src/executor.rs:492-587` (+ same-model
  retry `rig_runner.rs:719-760`).

**Net-new (build):**
1. **Effort as a resolution key** — `ModelRef` today is affinity×tier only; promote
   `effort` to a first-class facet the resolver keys on and returns.
2. **Named accounts** — `Credentials` maps a provider to *one* env var
   (`providers.rs:29-57`, `vendor_available` `:215-223`); N-named-credentials per
   provider does not exist. A pool member id is `(provider, model, account)`.
3. **Pool + RouterPolicy** — everything today is an *ordered* chain (cost-ascending
   failover); no pool, no in-flight accounting. Wire execution-policy 0.0.5
   `RouterPolicy` as a workspace dependency and let it own selection + failover,
   replacing the hand-rolled loop.

## 3. The requirement spec (the WHAT)

Extend the resolution key from `ModelRef` to a `RequirementSpec`:

```rust
struct RequirementSpec {
    affinity: Option<Affinity>,   // domain — reuse existing enum
    tier: Option<Tier>,           // reuse existing enum
    effort: Option<Effort>,       // NEW facet
    // hard constraints reuse Priorities semantics (filter, not rank):
    // budget_cap, local_only sourced from the run/stance context.
}

/// NEW closed enum. Maps to the existing reasoning-tuning levels (tuning.rs),
/// so the *apply* path is unchanged — only the *key* is new.
enum Effort { Fast, Medium, Deep }
```

- Parsing extends the `affinity|tier` grammar to an optional third segment
  (`coding|frontier|deep`), backward-compatible (effort omitted ⇒ today's behavior).
- `Effort` is poka-yoke typed (closed enum, exhaustive match), never a free string.
- A model satisfies an `effort` requirement iff its `ModelEntry.reasoning_levels`
  (`model_catalog.rs:44`) includes the mapped level — this becomes a **capability
  filter**, distinct from the applied `reasoning_effort` knob.

Effort is two-faced (from the design dialogue): a **filter facet** (can this model
think deeply at all?) and an **applied param** (`reasoning_effort` on the call).
The filter is new; the apply is `tuning.rs`, unchanged. **Effort does not split
the breaker key** — rate-limit health is per `(provider, model, account)`; effort
only shifts the meter + applied params.

## 4. Members & named accounts

A pool member is the triad `(ProviderId, model@provider, AccountName)`. The model
string is **provider-specific** (Fireworks `accounts/fireworks/models/qwen3-coder`
≠ OpenRouter `qwen/qwen3-coder`), so all three axes travel together.

**Named accounts (net-new)** extend `Credentials` (`providers.rs:29-57`):
- A provider may declare N named accounts, each a distinct env-var reference.
  Secrets stay in the environment; YAML references accounts **by name only**
  (never inlines a secret).
- `account_available(provider, account)` generalizes `vendor_available`
  (`:215-223`) from one env var to the named account's env var.
- **Load-time validation (fail-fast, poka-yoke):** an `account:` on a provider
  with no named-account registry (or on a keyless/local provider) is a config
  load error, not a silent default.
- **ToS caveat (operator responsibility):** multiple accounts on one provider to
  raise throughput is a documented gray zone; the spec surfaces it, the operator
  owns the decision. (Carried from the companion spec.)

## 5. Resolution → pool (not a single chain)

A new resolver implementation of the existing `ModelResolver`/`AffinityResolver`
trait (`session.rs:177-191`) — the exact seam that today returns an ordered
`Vec<String>` — instead returns a **pool** for a `RequirementSpec`:

1. **Filter** the catalog to members satisfying the hard constraints
   (`budget_cap`, `local_only`) and the facet capabilities (`affinity_fit`
   `config.rs:122-130`, `Tier`, `reasoning_levels` for effort).
2. **Rank** the survivors with the existing lens — `suggest_by_value` +
   Stance `(β, ε)` (`model_catalog.rs:179-240`, `priorities.rs:80-88`). Do **not**
   reinvent ranking.
3. **Materialize a pool** = the ranked set of members (each a
   `(provider, model, account)` triad), plus a `strategy` (`ordered` |
   `distribute`) — instead of collapsing to one cost-ordered chain.

**Config surface:** reuse the open `activity:` map (`config.rs:435`) and
`overrides` — a capability/requirement key resolves to a **member set + strategy**,
not just a `Vec<Binding>` chain. `ordered` preserves today's cost-ascending
failover exactly (a degenerate pool); `distribute` opts into load balancing.
Backward-compatible: an existing chain is an `ordered` pool.

## 6. RouterPolicy wiring (the consumer of execution-policy 0.0.5)

praxec takes a workspace dependency on `execution-policy` 0.0.5 (currently *not* a
dependency — `agents/src/breaker.rs:7`; failover hand-rolled at
`executor.rs:492-587` and `rig_runner.rs:719-760`).

- Hold **one canonical `execution_policy::Member` per `(provider, model, account)`**
  (its `ExecutionPolicy` carries retry/timeout/breaker), keyed in a per-process
  registry. Cross-pool sharing (a member in several capability pools) is the
  crate's `Member::clone` (shared `Arc` breaker + load) — §5 of the 0.0.5 spec.
- Per requirement, build a `RouterPolicy` over the resolved pool's members;
  `strategy: ordered → Pick::first_healthy()`, `distribute →
  Pick::weighted_least_in_flight()`.
- `router.run(async |member| call_provider(member, effort_params).await)` — the
  **effort params live inside the run-closure** (`TurnRequest.reasoning` via
  `tuning.rs`), never in the crate. The crate stays domain-blind.
- This **replaces** the hand-rolled escalation loop: `RouterPolicy` owns
  health-filter, selection, advance-on-classified-transient (feed it praxec's
  `FailureClass::is_infrastructure` as `advance_when`), and the park hint. The
  per-member breaker keyed by the triad falls out of `Member` identity (no
  separate per-account breaker code — that was groups 3/4 of the companion spec,
  now subsumed).
- Reactive throttle failover + even-distribution are then both properties of one
  `RouterPolicy`, replacing two hand-rolled loops with one vetted primitive.

## 7. Where it slots in (SDLC)

`agent declares RequirementSpec → resolver filters+ranks (suggest_by_value/Stance)
→ pool of members → RouterPolicy → call with effort params`. The agent config
gains an optional `requirement:` (affinity|tier|effort + constraints) alongside
the existing `agent`/`affinity`/`needs` XOR set (`agents/src/config.rs:18-68`,
`llm-executor/src/config.rs:20-87`) — additive, the existing pins keep working.

## 8. Non-goals / scope

- **Keep the ordered-chain path working** — `distribute` is opt-in; `ordered` is
  the default and reproduces today's behavior. No forced migration.
- **No cross-process load state** — per-process pools (execution-policy non-goal).
- **No auto-tuning of weights** — static config; de-escalation stays the governed,
  human-gated cost loop it is today.
- **hop_slot is a separate axis** — it resolves cap ids, not models
  (`config.rs:1085-1160`); untouched here.
- **No new ranking engine** — `suggest_by_value` is the ranker.

## 9. Behavioral assertions (TDD, pre-FMECA)

1. `RequirementSpec` parse: `coding|frontier|deep` → all three facets; effort
   omitted ⇒ identical to today's `ModelRef`.
2. Effort filter: a model whose `reasoning_levels` lacks the requested level is
   excluded from the pool.
3. Named account: `account:` on a provider with no registry ⇒ load error; a valid
   named account resolves to its env var; missing env ⇒ member unreachable
   (breaker-open equivalent), not a crash.
4. Pool ranking = `suggest_by_value` order under the active Stance (differential vs
   a hand-ordered expectation).
5. `strategy: ordered` reproduces the current cost-ascending failover byte-for-byte
   (regression against the hand-rolled loop).
6. `strategy: distribute` spreads across healthy members and focuses-in on a 429
   (RouterPolicy behavior, end-to-end through a fake provider).
7. Cross-pool: one member in two capability pools shares one breaker (a trip via
   pool A is Open in pool B).
8. Effort params reach the provider call (`TurnRequest.reasoning`) unchanged by the
   pool routing.
9. `advance_when` = `FailureClass::is_infrastructure` — a content failure surfaces,
   an infrastructure failure advances (parity with `executor.rs` classification).

## 10. Open decisions

1. **Config shape for pools** — extend `activity:`/`overrides` values from
   `Vec<Binding>` to `{members, strategy}`, or a new `pools:` map? (Lean: extend,
   for backward-compat and one place to look.)
2. **Account registry location** — on `ProviderDescriptor` (static) vs a separate
   `accounts.yaml`/env-scan (dynamic, rate-of-change separation per the
   data-in-config rule). Lean: separate, since accounts are operator/env data.
3. **Replace vs wrap the escalation loop** — does `RouterPolicy` fully replace
   `executor.rs:492-587`, or wrap it behind the trait so `ordered` stays on the old
   path and only `distribute` uses RouterPolicy? (Lean: one path — RouterPolicy for
   both, `ordered` = `first_healthy`, to avoid two failover implementations.)
4. **FMECA vet** — run the reliability-engineering methodology on this spec (as we
   did for 0.0.5) before the implementation plan.
