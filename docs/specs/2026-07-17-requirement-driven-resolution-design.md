# Requirement-Driven Resolution + RouterPolicy Pools (praxec spec #2)

**Status:** design, grounded (file:line cited) + FMECA-vetted (iteration 1 — all risks Low)
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

**Fail-fast on an empty pool (R1).** If filtering yields no member, resolution
returns `ResolutionError::Unsatisfiable { spec, reason }` naming the facet/
constraint that emptied it (e.g. "no `local_only` model supports `effort: deep`").
It **never** falls back to a default model — a silent default is exactly the
agentic shortcut this design rejects.

**`distribute` pools only within the value band (R2).** Even-spreading a
cost-asymmetric pool would send load to expensive members, defeating the stance.
So `distribute` balances **only members inside the ranking's marginal band ε**
(`priorities.rs` ε, already "prefer stronger within ε"); members outside the band
are *lower failover bands*, reached by `ordered` advance, never distribution
targets. A cost-asymmetric requirement therefore distributes across near-equals
and fails over across tiers — the "ordered not distribute for cost-asymmetric
pools" guidance becomes a mechanism, not a doc note. (TRIZ: Local Quality —
segment the pool by value band.)

**Unreachable members are excluded (R5).** `account_available` (§4) drops members
whose account env var is absent at pool-build (like a breaker-open member); if
that empties the pool, R1 fires. Load-time warns on configured-but-unreachable
accounts.

**Effort capability is a complete filter (R4).** A member satisfies `effort: deep`
only if BOTH its `ModelEntry.reasoning_levels` includes the mapped level AND its
provider has an effort mapping in `tuning.rs` (`:144-193`). Missing catalog data ⇒
**exclude** (conservative), never assume-support. A catalog-lint flags effort-tier
models lacking `reasoning_levels`.

**Deterministic pool order (R8).** Ranking ties break by a stable sort
`(value desc, provider, model, account)` — so `ordered` failover order and tests
are reproducible, never hashmap-iteration-dependent.

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
- `advance_when` is the **same** `FailureClass::is_infrastructure` function the
  executor already uses (R7) — passed in, not reimplemented, so classification
  can't drift between the router and the rest of praxec.
- The per-member breaker keyed by the triad falls out of `Member` identity (no
  separate per-account breaker code — that was groups 3/4 of the companion spec,
  now subsumed).
- **Never a second load-balancer (R6).** `distribute` is served *only* by
  `RouterPolicy`. The existing agent escalation loop (`executor.rs:492-587`) is a
  *passive per-invocation planner*, deliberately off execution-policy
  (`breaker.rs:7`). `ordered` migrates onto `RouterPolicy::first_healthy` **only
  behind the byte-for-byte regression gate** (assertion 5); if the router's active
  wrapper can't cleanly express the agent's per-invocation re-resolution, the
  agent's `ordered` path stays the existing passive walk **unchanged** — it is not
  a load balancer, so there is nothing to duplicate. Either way there is exactly
  one distribution implementation and at most one failover implementation per
  surface.

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
10. Empty pool ⇒ `ResolutionError::Unsatisfiable` naming the facet — never a
    default-model fallback (R1).
11. `distribute` balances only within the value band ε; a cost-asymmetric pool
    fails over across bands instead of spreading (R2).
12. A bare `Vec<Binding>` in `activity:`/`overrides` still deserializes (as an
    `ordered` pool); the `{members, strategy}` mapping also parses; a malformed
    value errors with the dual-form message (R3).
13. A model missing `reasoning_levels`, or a provider with no `tuning.rs` effort
    mapping, is excluded from an `effort`-carrying pool — never assumed-capable (R4).
14. Pool ordering is deterministic under ranking ties (R8); an unknown effort
    grammar segment is a parse error, not a silent ignore (R9).

## 10. Decisions (resolved by the FMECA vet — §11)

1. **Config shape for pools** — **RESOLVED: extend `activity:`/`overrides` with
   dual-form serde (R3).** A bare `Vec<Binding>` deserializes as
   `{members: <bindings>, strategy: ordered}` (backward-compatible with every
   existing config); the mapping form `{members, strategy}` is explicit; a
   malformed value yields an actionable dual-form error. One place to look, no
   migration.
2. **Account registry location** — **RESOLVED: separate `accounts.yaml` / env
   scan**, not on `ProviderDescriptor`. Accounts are operator/environment data with
   a different rate of change than the curated provider catalog (data-in-config
   rule); secrets stay in env, referenced by name.
3. **Replace vs wrap the escalation loop** — **RESOLVED (R6): never a second
   load-balancer.** `distribute` is only `RouterPolicy`; `ordered` migrates onto
   `RouterPolicy::first_healthy` only behind the byte-for-byte regression gate
   (assertion 5), else the agent's passive walk stays unchanged. See §6.

## 11. FMECA vet record (iteration 1, 2026-07-17)

Vetted with the reliability-engineering methodology (FMECA → poka-yoke →
prevent/detect/fail-fast → TRIZ-if-trade-off), 9 failure modes across UX / runtime
/ architecture / delivery, **all reduced to residual Low in one iteration**. Key
hardening folded into this spec:
- **R1** empty pool ⇒ `ResolutionError::Unsatisfiable`, never a silent default.
- **R2** (TRIZ Local Quality) `distribute` balances only within the value band ε;
  cost-asymmetric members fail over, never spread — the guidance becomes a mechanism.
- **R3** dual-form serde keeps every existing `activity:`/`overrides` config valid.
- **R4** effort filter requires model `reasoning_levels` AND provider `tuning.rs`
  mapping; missing data excludes, never assumes.
- **R5** unreachable accounts are excluded at pool-build (⇒ R1 if it empties).
- **R6** exactly one distribution impl; `ordered` migration regression-gated.
- **R7** `advance_when` reuses the existing `is_infrastructure` (no drift).
- **R8** deterministic pool tie-break; **R9** effort-grammar parse errors, no silent ignore.

Systemic check — accuracy: no fabricated figures; complexity: additions are
fail-fast guards + reuse of existing lens/ε, not new machinery; capability: the
ordered-chain path is preserved (opt-in `distribute`), nothing removed.
