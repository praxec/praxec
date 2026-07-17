# Multi-provider load distribution: a credentialed member pool with reactive focus

Status: proposed design (2026-07-16) · Branch: feat/multi-provider-load-distribution

## Problem

A single serverless inference account has a fixed rate-limit bucket (RPM /
tokens-per-minute / concurrent requests). When a step's throughput approaches
that ceiling the provider returns HTTP 429 and the step stalls or fails. Today
praxec answers this **reactively and single-track**: a binding list is an
*ordered preference chain* (`walk.rs::try_next`) — always try index 0, advance
to the next binding only on an infrastructure failure (429 included, via
`FailureClass::RateLimit429.is_infrastructure()`), and a per-model circuit
breaker (`praxec-agents/src/breaker.rs`) benches a persistently-failing model
for a cooldown. That gives resilient *failover* but not *capacity*: all traffic
still targets one account until it breaks.

The goal is **aggregate throughput**: run a step against a **pool** of
interchangeable providers/accounts that each serve the same open-weight model,
distribute load evenly across the pool while every member is healthy, and
**focus in** — concentrate traffic on the survivors — the moment a member is
throttled, restoring it when the throttle clears. The pool must scale to three
or more members, and a member is identified not by provider alone but by
**account**: several accounts on one provider are distinct rate-limit buckets
and must count as distinct pool members.

OpenRouter is explicitly **not** central to this design. It becomes one optional
pool member among several first-party US inference providers. If a provider
won't let us provision the accounts we need to scale, we drop it and spread load
across others.

## Decisions (from the design interview)

- **The pool member is `(provider, model, account)`** — not `(provider,
  model)`. The rate-limit bucket lives at the account (API key), so the account
  is a first-class part of member identity, of breaker health, and of the audit
  record.
- **A chain gains an explicit selection strategy**: `ordered` (today's exact
  behavior, the default) or `distribute` (balance across healthy members).
  Existing chains with no strategy behave exactly as they do now.
- **Distribution is reactive, not proactive.** Even distribution while healthy;
  a member drops out of the pool when *its own* breaker opens on repeated 429s;
  survivors absorb the load; cooldown re-probe re-widens the pool. We do **not**
  model each account's documented rate-limit numbers or pace against them
  (proactive budgeting is out of scope — see Non-goals).
- **Pick by least-in-flight, never RNG or wall-clock.** The resolver is
  deliberately clock-free and random-free (the breaker insists on monotonic
  `Instant`). Distribution picks the healthy member with the fewest in-flight
  requests, tie-broken deterministically — so a resolution is reproducible from
  the observed call-arrival order.
- **No new fallback semantics.** Distribution changes only *which healthy member
  is tried first*. The existing Chain-of-Responsibility remainder-walk, the
  "429 → route around / content error → surface" classification, and the
  degrade-never-zero guarantee are all reused unchanged.
- **Selection + health become a shared resolver-layer concern** consulted by
  both the governed `kind: llm` path (`praxec-llm-executor`) and the `kind:
  agent` path (`praxec-agents`), so distribution and per-account cooldown apply
  uniformly. Today the breaker lives only in the agent path.
- **Providers stay a closed, poka-yoke enum.** Each US provider is one
  `ProviderId` variant carrying its base URL; the exhaustive `match` forces
  every surface to handle it at compile time. Model IDs, prices, and rate-limit
  numbers stay out of code (data files with override paths) per the
  data-in-config rule; provider **identity** (slug, display, base URL, key env)
  stays in the descriptor catalog where it already lives.

## Design

### 1. Provider fleet: OpenAI-compatible, table-driven, config-activated

Every serious US open-weight host speaks the OpenAI **Chat Completions** API —
exactly the path `openai_completions_client(base)` in `provider_factory.rs`
already builds (an `openai::CompletionsClient` at a custom base URL with an
explicit key). So the whole fleet is one code shape distinguished only by *data*
(base URL + key env), and a provider is **inert until the user configures an
account for it** (§3). Shipping a variant costs nothing at runtime; a user opts a
provider in by registering a key, not by a rebuild.

**Provider identity is the only thing that lives in code** (`providers.rs` is
already "provider identity only"); model IDs, prices, and rate-limit numbers stay
in data files per the data-in-config rule.

| `ProviderId` | slug | default key env | base URL (`OpenAiCompletions`) |
|---|---|---|---|
| `Fireworks` | `fireworks` | `FIREWORKS_API_KEY` | `https://api.fireworks.ai/inference/v1` |
| `Together` | `together` | `TOGETHER_API_KEY` | `https://api.together.xyz/v1` |
| `Baseten` | `baseten` | `BASETEN_API_KEY` | `https://inference.baseten.co/v1` |
| `DeepInfra` | `deepinfra` | `DEEPINFRA_API_KEY` | `https://api.deepinfra.com/v1/openai` |
| `Groq` | `groq` | `GROQ_API_KEY` | `https://api.groq.com/openai/v1` |
| `Cerebras` | `cerebras` | `CEREBRAS_API_KEY` | `https://api.cerebras.ai/v1` |
| `SambaNova` | `sambanova` | `SAMBANOVA_API_KEY` | `https://api.sambanova.ai/v1` |
| `Hyperbolic` | `hyperbolic` | `HYPERBOLIC_API_KEY` | `https://api.hyperbolic.xyz/v1` |
| `Parasail` | `parasail` | `PARASAIL_API_KEY` | `https://api.parasail.io/v1` |

The base URL / key-env in each row is **data verified against that provider's
current docs at the moment its variant is wired** — no design decision depends on
any specific string being correct today (accuracy over fabricated precision).

Mechanics that make adding one straightforward:

- Extend `ProviderDescriptor` with `base_url: Option<&'static str>` and a `wire:
  WireStyle` discriminator (`NativeResponses` for api.openai.com / anthropic /
  gemini native SDKs; `OpenAiCompletions` for the compat fleet).
- Each fleet member is **one descriptor row + one `ProviderId` variant**. The
  exhaustive `match` at every projection site (descriptor, factory, feature
  struct, `set-provider-keys`, `doctor`) is a compile error until the variant is
  handled — so "add a provider" is a mechanical, poka-yoke'd change, never a
  scattered edit that can be half-done.
- The factory's `OpenAiCompletions` arm is **one branch for the whole fleet**:
  `openai_completions_client(descriptor.base_url)` with the resolved account's
  key (§3). No per-provider client code.
- `Availability` is `Always` for every fleet variant (it is just a base URL). A
  member is *usable* only when one of its accounts has a present key
  (`account_available`, §3); an unconfigured provider is shipped but never
  selectable, and `doctor` reports exactly which providers are configured.

First variant wired end-to-end: **Fireworks** (models
`accounts/fireworks/models/...`); the remaining rows are added on demand by the
same one-row recipe as users ask for them.

### 2. The chain becomes a typed pool with a strategy

Today a chain is a bare `Vec<Binding>` under `default`, each `overrides[key]`
(affinity/tier), and each `activity[key]` (open string). Introduce a `Chain`:

```yaml
# ordered (shorthand — a bare sequence, = today's behavior)
coding-frontier:
  - { provider: { name: fireworks }, model: accounts/fireworks/models/qwen3-coder }
  - { provider: { name: openrouter }, model: qwen/qwen3-coder }

# distribute (the new form)
coding-frontier:
  strategy: distribute
  members:
    - { provider: { name: fireworks }, model: accounts/fireworks/models/qwen3-coder, account: fw-a }
    - { provider: { name: fireworks }, model: accounts/fireworks/models/qwen3-coder, account: fw-b }
    - { provider: { name: together },  model: Qwen/Qwen3-Coder,                      account: tg-a }
```

`Chain` deserializes from **either** a sequence (→ `strategy: ordered`) **or** a
mapping `{ strategy, members }`. The mapping keeps `deny_unknown_fields`. This
preserves every existing `models.yaml` verbatim (bare sequences remain valid and
mean `ordered`) — a sensible default, not a back-compat shim. The custom
deserializer distinguishes the two forms explicitly and, on a mapping that
carries binding keys (`provider`/`model`) instead of `{strategy, members}`,
fails with an actionable message ("a chain is a list of bindings *or* `{strategy,
members}`; got a mapping with key `provider`") rather than a bare
`deny_unknown_fields` error — the common "I wrote one binding where a chain was
expected" mistake is named, not left cryptic.

`Binding` gains an optional `account: Option<String>` naming an entry in the
accounts registry (§3). Omitted → the provider's single default key (the common
single-account case, identical to today).

### 3. Named accounts: N credentials per provider

The credential model is currently one env var per provider
(`Credentials::Single("FIREWORKS_API_KEY")`). Generalize to a registry that maps
a provider slug to several **named accounts**, each pointing at an env var
(secrets stay in the environment / `set-provider-keys`, never in YAML):

```yaml
accounts:
  fireworks:
    - { name: fw-a, key_env: FIREWORKS_A }
    - { name: fw-b, key_env: FIREWORKS_B }
  together:
    - { name: tg-a, key_env: TOGETHER_A }
```

- A member's `account:` must resolve to a registered account for its provider —
  **validated at config load** (`check`), not at resolve time (poka-yoke: a typo
  fails loud, early, with the offending name).
- `vendor_available` generalizes to `account_available(provider, account)` —
  reachable iff that account's `key_env` is set. A member whose key is absent is
  simply not in the pool (it never enters the healthy set).
- The factory no longer calls `Client::from_env()` for compat members; the
  resolver hands it a `ResolvedMember { provider, model, key_env, features }` and
  the factory reads that specific key and builds the client at the provider's
  base URL. Native-SDK providers (anthropic/gemini) keep `from_env` single-key
  until multi-account is needed there (out of scope now).
- **`account:` on a `NativeResponses`-wire provider is a load error, never a
  silent ignore.** Native SDK clients auth via `from_env` and cannot honor a
  named account; letting an operator write `account: acct-b` on an Anthropic
  member and quietly using the default key is exactly the paper-over-instead-of-
  fail-fast trap. `check` rejects it: "provider `anthropic` does not support
  multi-account (`account:`); it authenticates via `ANTHROPIC_API_KEY`."

### 4. Selection: strategy over the breaker-defined healthy set

Health and selection compose as two orthogonal concerns:

- **Health = the breaker.** `breaker.rs` already partitions members into
  closed/half-open/open and, in `plan_chain`, "degrades, never zeroes out." Its
  key changes from a model string to the `(provider, model, account)` triple, so
  throttling `fw-a` benches **only** `fw-a`, never `fw-b` or the model globally.
  Nothing else in the state machine changes.
- **Selection = the strategy, run over the healthy set.**
  - `ordered`: pick the healthy, lowest-index member (today).
  - `distribute`: pick the healthy member with the fewest **in-flight**
    requests; tie-break by a shared round-robin counter, then by index. A
    per-member atomic in-flight counter is incremented on dispatch and
    decremented on completion via an **RAII guard** (so a panicked/cancelled
    request cannot permanently inflate a member's load and starve it).
    Least-in-flight is chosen deliberately because it **self-corrects for
    latency**: a slow member's requests linger, its in-flight count stays high,
    and it sheds new traffic without any latency measurement.

**Concurrency (this is a throughput path — it must not self-bottleneck).** The
per-member in-flight counters are **lock-free `AtomicUsize`**, read/written on
every dispatch; they are *not* held under the breaker's `Mutex`. The breaker's
`Mutex<HashMap>` is consulted only to read/transition health (far less often
than the per-request counter bump), so the hot path (pick + increment) never
serializes on a global lock. The breaker map's key space is **bounded to the
configured members** (accounts × models in `models.yaml` are finite and static);
resolution never mints a key from an arbitrary request string, so the map cannot
grow with traffic.

Only the *first* attempt is strategy-selected. If it fails with an
infrastructure class, the CoR remainder-walk (`try_next`) proceeds over the rest
of the pool exactly as today; a content-class failure surfaces immediately (no
silent sibling retry). Under sustained 429s a member's breaker opens after the
threshold, it leaves the healthy set, and `distribute` naturally concentrates on
the survivors — the "focus in when throttled" behavior — until the cooldown
half-open probe restores it.

### 5. One shared holder — the `ProviderFactory` seam

The single most dangerous failure in this design is *two* holders: if
`praxec-llm-executor` and `praxec-agents` each construct their own breaker +
in-flight state, a member throttled on the agent path still looks healthy to the
`kind: llm` path, distribution and cooldown silently stop coordinating, and the
"uniform across both paths" decision quietly fails. So the home is not left open:

- **Pure** decision logic (breaker transitions — already `Instant`/`Duration`-
  injected — and the least-in-flight pick) lives in
  `praxec-core::model_resolver`, beside the existing pure `walk`/`try_next`.
- The **stateful** holder lives in the seam both paths *already* share:
  `ProviderFactory` (ADR-0007 — "one provider wiring shared with the agent
  runner"). Both `kind: llm` and `kind: agent` already resolve providers through
  the one `DefaultProviderFactory`; the breaker map + in-flight atomics hang off
  that same instance.
- **Construction poka-yoke:** the holder is built exactly once at gateway wiring
  and handed to both executors by `Arc`. No executor constructs its own — there
  is no per-path constructor to call. A cross-path visibility test (§ Behavioral
  assertions) pins it: a failure recorded through the agent path must be visible
  to the llm path's next pick, and vice-versa.

### 6. Audit

`MODEL_RESOLVER_WALK` records the chosen member **including account**, and a
reason tag: `balanced` (distribution pick among healthy members) vs `health`
(forced because higher-preference members were open) vs `degraded` (all open,
least-recently-failed yielded). Reproducibility is a product property here, not
a nicety — a governance run must be explainable after the fact.

## FMECA (prevent → detect → fail-fast)

| Mode | Effect | Mitigation |
|---|---|---|
| "Same model" differs across providers (quant, context, tool-calling fidelity) | Nondeterministic step behavior run-to-run | `distribute` is **opt-in per chain** — enabled only where the operator accepts variance; `ordered` chains are byte-for-byte deterministic as today. **Residual is irreducible by construction** — distribution trades reproducibility for capacity; see the systemic note below |
| A member returns 200 but *lower-quality* output (heavy quant, silent truncation) — not an error, so it never trips the breaker | Silent quality erosion; least-in-flight keeps routing to it | Cannot be *prevented* structurally (a conforming-but-worse answer is undetectable at the boundary). Non-conforming output already routes around (`AGENT_RESULT_FAILED` → `Capability`). Detection: **every resolution records which member served** (audit, §6), so a quality regression is attributable to a member and that member can be pulled from the pool. Honest residual: Medium |
| Even split across cost-asymmetric members | Operator adds a cheap + an expensive provider to a `distribute` pool expecting savings, gets 50/50 and a *higher* bill | `distribute` is opt-in; per-member spend is visible via the existing `cost_report` (attribution by member); **docs recommend `ordered` — not `distribute` — when members differ materially in price**. Even split is a capacity tool, not a cost tool |
| A throttled account benches its healthy sibling | Lost capacity, false focus | Breaker keyed by `(provider, model, account)` — one account's health is independent |
| Content error from a distributed member | Silent wrong-answer retry on a sibling | Content classes still surface immediately; distribution spreads *attempts*, never weakens surfacing (the R1 no-silent-fallback invariant is preserved) |
| All members throttled/open | Step has nothing to run | Degrade-never-zero (existing `plan_chain`) yields the least-recently-failed member for one attempt |
| In-flight counter leaked on panic/cancel | Member permanently looks "busy" and starves | Counter decrement via RAII guard drop, not manual bookkeeping |
| `account:` typo / unregistered account | Resolve-time failure deep in a run | Validated at config **load** (`check`) — fails loud and early with the name |
| Account key absent at startup | Member silently dead or a confusing resolve error | `account_available` gates membership; a missing-key member never enters the pool, surfaced by `doctor` |
| Multi-account on one provider to dodge per-account caps | Provider ToS violation → keys banned | Operator responsibility; **documented risk**. Prefer providers whose terms permit multiple keys; treat same-org account-fragmentation as a gray zone to verify before relying on it |

## Non-goals (YAGNI)

- **Proactive rate-limit budgeting.** No tracking of each account's documented
  RPM/TPM and no pre-emptive spill before a 429. Distribution is reactive by
  design. *If added later*, those ceilings are **sourced data** in a file with
  an override path, never Rust consts.
- **Cost/capacity weighting.** Distribution starts **even** (least-in-flight).
  Optional static per-member weights (to bias toward cheaper/faster members) are
  a documented future refinement, not built now.
- **Multi-account for native-SDK providers** (Anthropic, Gemini). The pool's
  multi-account path is the OpenAI-compat completions fleet; native providers
  keep single-key `from_env` until a concrete need appears.
- **Provider auto-discovery / dynamic pool membership.** The pool is authored in
  `models.yaml`; no runtime provider registration.

## Rollout (commit groups on one branch)

Sequenced so each group is independently shippable **and** the highest-risk
integration (one shared holder) lands *before* anything depends on it — a
member-keyed breaker must never ship on top of a not-yet-shared holder.

1. **Fleet plumbing** — `WireStyle` + `base_url` on `ProviderDescriptor`;
   table-driven factory dispatch; `ProviderId::Fireworks`. Ordered chains can
   name Fireworks immediately — reactive 429-failover works with zero new
   selection code. Pure additive value, no breaker/state change.
2. **Named accounts** — registry type, load-time validation (incl. rejecting
   `account:` on native-SDK providers), `account_available`, `ResolvedMember`
   threaded into the factory, `set-provider-keys`/`doctor` surfaces.
3. **One shared holder** — move the breaker (+ in-flight atomics scaffold) onto
   the shared `ProviderFactory` seam, `Arc`-shared to both executors; land the
   cross-path visibility assertion. *Precedes* per-account keying deliberately.
4. **Per-account breaker key** — `(provider, model, account)`. Safe now that a
   single holder is shared; ships with the per-account isolation assertion.
5. **`distribute` strategy** — `Chain` type + `strategy` (sequence-or-mapping
   deserialize with the actionable dual-form error) + least-in-flight pick with
   lock-free counters and RAII guards.
6. **Observability + docs** — audit reason tags; docs for the ToS caveat, the
   accounts config, and the "ordered, not distribute, for cost-asymmetric pools"
   guidance.

## Behavioral assertions (TDD — pin every invariant before implementation)

Each is an atomic, declarative unit assertion written *before* its group's code,
so the governance invariants cannot silently regress:

- **content-error-surfaces:** a distributed member returning a content-class
  failure surfaces immediately; no sibling is tried (the R1 no-silent-fallback
  invariant, now under `distribute`).
- **per-account-isolation:** throttling account `fw-a` opens only `fw-a`'s
  breaker; `fw-b` (same provider+model) stays selectable.
- **cross-path-visibility:** a failure recorded via the `kind: agent` path is
  visible to the `kind: llm` path's next pick (proves one shared holder).
- **focus-in-on-throttle:** with a member's breaker open, `distribute` sends
  zero traffic to it and splits across the survivors; cooldown half-open re-adds
  it after one probe success.
- **degrade-never-zero:** all members open → exactly one attempt is still
  yielded (least-recently-failed), then `ModelResolutionExhausted` surfaces (no
  hang, no infinite re-probe).
- **native-account-rejected:** `account:` on an Anthropic/Gemini member fails at
  `check` with the actionable message, never a silent default-key fallback.
- **chain-form-error:** a chain written as a single-binding mapping fails load
  with the "list of bindings *or* `{strategy, members}`" diagnostic.
- **in-flight-guard:** a dropped/cancelled request decrements its member's
  in-flight counter (no permanent inflation).
- **ordered-unchanged:** an existing bare-sequence chain resolves byte-for-byte
  identically to pre-change behavior (no regression for non-adopters).

## FMECA vet (2026-07-16, iteration 1)

Reviewed under the reliability-engineer methodology (FMECA → poka-yoke → TRIZ →
CPM), hunting specifically for fallbacks/shortcuts masquerading as production-
ready. Twelve failure modes across UX, runtime, architecture, and delivery; the
mitigations are folded into the sections above. Highlights:

- **High/Med — divergent per-path holder** (two breakers). → §5: one holder on
  the shared `ProviderFactory`, `Arc`-injected, cross-path visibility assertion.
- **High/Med — governance invariants regress silently.** → Behavioral-assertions
  section pins each invariant test-first.
- **High/Med — silently-worse member** (200 but low quality). → irreducible;
  detection via per-member audit attribution; honest residual Medium.
- **Med/Med — `account:` silently ignored on native providers.** → load-time
  rejection (poka-yoke), not a default-key paper-over.
- **Med/Med — throughput path self-bottlenecks on a global lock / unbounded map.**
  → lock-free per-member atomics off the hot path; key space bounded to config.
- **Med/Med — cost surprise from even split.** → opt-in + `cost_report`
  attribution + "ordered for cost-asymmetric pools" guidance.
- **Med/Med — chain form ambiguity.** → actionable dual-form deserialize error.
- **CPM — reorder** so the shared holder (group 3) precedes per-account keying
  (group 4).

Residual after iteration 1: no High remains; two Mediums are *constraint-bound,
not defects* — (a) cross-provider quality variance is the intrinsic price of
distribution (opt-in, attributable), (b) multi-account-on-one-provider ToS risk
is operator/legal, not a code property. Iteration stopped (stop-condition 1 & 4:
agentic-shortcut modes addressed; no further structural reduction available
without removing the capability).

## Open questions

- Second and third providers to wire after Fireworks (candidate set in §1) —
  driven by which models we actually serve and which providers permit the
  account counts we need. (Resolved this pass: the shared selection/health holder
  lives on the `ProviderFactory` seam — §5 — not left open.)
