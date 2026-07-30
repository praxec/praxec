# MCP Tool Discovery & Lifecycle — Design

**Status:** draft for review · **Date:** 2026-07-30 · **Depends on:** v0.0.43 doctor tool-currency (the *maintenance* verb of this surface)

## 1. Problem

praxec governs MCP connections declaratively — a `kind: mcp` connection with
`command`/`args`/`url`/`env` — but the operator does everything *around* that by
hand: find a tool, install it, discover its required secrets/config, wire the
connection, and later notice it's stale. There is no discovery, no evaluation of
fit, no governed provisioning, and (until v0.0.43) no currency check.

The opportunity is to make the **whole tool lifecycle** a first-class, governed
surface, grounded in primitives praxec already has:

```
discover → evaluate → provision → maintain (currency ✅ v0.0.43) → deprovision
```

v0.0.43 shipped the maintenance verb. This spec designs the two ends.

## 2. Principles (praxec's own stances, applied)

- **Data, not code.** Registries and the catalog are *data* (typed adapters),
  never hardcoded to one provider — same rule as `models.yaml`, the pricing
  catalog, and pack remote-sourcing.
- **Preserve the two-tool surface (SPEC §32).** No new top-level MCP tools. New
  **query verbs** for reads; **governed workflows** for the mutations.
- **Ground the recommendation.** "Useful" is a *deterministic capability match*
  against the user's configured workflows and their unmet needs — not a
  generative guess. A suggestion must tie to a concrete gap (measurement must
  change a decision).
- **Fail-safe + poka-yoke.** Currency is advisory; provisioning is HITL-gated;
  secrets are env-referenced, never inlined.
- **Trust tiers.** Verified (Official Index) and org-published tools are a
  trusted lane; community/third-party tools are untrusted → sandboxed and
  operator-approved. Provisioning arbitrary code is a supply-chain surface
  (ADR-0006 two-tier trust).

## 3. Architecture

### 3.1 Registry adapters — the data-driven catalog

A declarative `registries:` block: a list of typed adapters, each normalizing
its native response into one common `ToolCandidate`. Adapters compose like
`models.yaml` pools or the `repos:` list — add/reorder/remove in data.

| adapter `kind` | queries | notes |
|---|---|---|
| `mcp-registry` | the official Anthropic MCP Server Index | canonical, **verified** tier |
| `github-org` | enumerate an org's repos (generalized from the praxec sweep) | **org** tier |
| `github` | a repo's server list/README | for the Index's list form |
| `smithery` | Smithery API | has a CLI installer → informs *provision* |
| `glama` | Glama directory | ships config snippets → `install_recipe` |
| `pulsemcp` | PulseMCP | categorized by use-case → feeds *evaluate* |
| `crates` / `npm` | registry search (mcp-tagged) | published tools |
| `rest` | **any** REST endpoint returning candidates | reuses praxec's `kind: rest` executor |
| `direct` / `static` | operator-declared candidates inline | **custom/private** MCPs, no remote registry |

Each adapter is small: hit the source, map fields, emit `ToolCandidate`s. The
normalization is the whole trick — everything downstream is registry-agnostic.

Registries are themselves remote data, so they inherit the **pack-currency**
concerns (cache, staleness, refresh) already solved for `uri:`/`ref:` packs.

### 3.2 The normalized `ToolCandidate`

```
ToolCandidate {
  name, description,
  transport:    stdio | docker | remote | rest,
  source:       { repo | crate | npm | image | url },   # how to obtain it
  capabilities: [ ... ],  tags: [ ... ],                 # for evaluate / match
  trust_tier:   verified | org | community,
  install_recipe: { ... },                               # per-transport install steps
  requires: {                                            # what wiring it needs
    secrets: [ { name, description } ],                  #   e.g. FIGMA_TOKEN
    config:  [ { name, description, kind } ],            #   e.g. a Figma file URL
  },
  provenance:   { registry, url },
}
```

`requires` is the field the operator's instinct demanded: provisioning a
third-party MCP is **not** just installing a binary — Figma needs a token *and* a
file URL; GitHub needs a PAT. Declaring those requirements is what lets
provisioning collect and validate them (§3.4).

### 3.3 Discover + evaluate (reads — new **query** verbs)

- `praxec.query { discover: "<intent>" }` → search the composed catalog; return
  ranked `ToolCandidate`s (reuses the existing discovery/search machinery, over
  a new catalog instead of workflows/caps).
- `praxec.query { evaluate: <workflow-or-gap> }` → **deterministic** match: given
  a workflow's declared steps/capabilities and their unmet needs, rank
  candidates whose `capabilities` fill the gaps. Grounded, not generative.

  **Vocabulary (decided): reuse the existing verb + capability model.** A tool
  sits one level below a capability — a `cap` *uses* a tool as substrate — so a
  `ToolCandidate` is described by the cap-**verbs**/domains it can serve. That is
  the join: a workflow needs a cap of verb X; a candidate offers to serve verb X.
  One taxonomy across workflows, caps, and tools; the candidate's `tags` add the
  finer axis (browser vs screenshot vs PDF, all `diagnose`/`research`). No new
  vocabulary to invent.

**Authoring integration (a first-class use, not a side feature).** praxec-meta's
`flow.author-flow` / `flow.author-capability` gain a `suggest-tools` step: once
the capability graph is drafted, it calls `evaluate` on that graph and offers
matching candidates inline — "this `verify` step has no browser tool wired; here
are two, verified tier." Discovery becomes *contextual to what you're building*,
not a directory you go browse.

### 3.4 Provision + deprovision (writes — a governed **workflow**)

A `flow.tools.provision` orchestrator — a workflow, not a tool call, so the
mutation is HITL-gated and auditable:

1. **select** the candidate (from discover/evaluate).
2. **trust gate** — verified/org → trusted lane; community → require explicit
   operator approval + sandboxed execution (ADR-0006).
3. **install** — per transport, from `install_recipe`: `cargo install` /
   `docker pull` / `npm` / clone+build / register a `url`. (Same recipes the
   currency check reads.)
4. **collect secrets + config** — **elicitation** over `requires`: prompt the
   operator for each declared secret/config value; land them in the connection's
   `env:` (secrets, as env-refs — never inline plaintext), `args:`/`url:`
   (config). Reuses the provider-keys stance: piggyback on the operator's own
   credential store; praxec stores no secret material in config.
5. **validate** — run the doctor credential-check (`guard_provider_credentials`
   generalized to connection secrets) + a bounded `initialize` connect probe,
   BEFORE wiring the tool live. Fail-fast here, not at first use.
6. **wire** — write the `connections:` entry; record provenance so the tool is
   currency-checkable from then on.

`flow.tools.deprovision` reverses it: remove the connection, optionally uninstall
the binary/image, and purge or retain the secrets on request.

### 3.5 Maintain — currency (shipped)

doctor's v0.0.43 currency check is the maintenance verb. A tool provisioned
through §3.4 records enough (`source`/`provenance`) to be currency-checked
automatically, closing the loop.

**Freshness on a TTL (decided, ~24h).** The lifecycle workflow carries a refresh
step that fires when data is older than a day, split by read vs execute:

- **Catalog / registry metadata** is a pure read → auto-refresh freely on the
  TTL (fresh discovery without hammering registries on every call).
- **Installed tools** are execution + mutation (a rebuild/repull *runs new
  code*) → the step always *surfaces* staleness (this is the v0.0.43 currency
  check running on the TTL), but applies an update only inside the trust gate:
  auto-update is opt-in for already-approved trusted/verified tools, and
  surface-for-approval for community — never a silent reinstall of third-party
  code on a timer. Same don't-silently-mutate line as provisioning.

## 4. Trust & secrets

- **Trust tiers → execution posture.** Verified/org tools run in the trusted
  lane; community/third-party run untrusted → sandboxed (ADR-0006), and are never
  auto-installed — provisioning them is always an explicit, approved operator
  action. Third-party MCP code is a supply-chain surface; the design treats it
  as one.
- **Secrets.** Generalize the existing provider-key mechanism (`providers.env`,
  `guard_provider_credentials`, `px set-provider-keys`) to *connection secrets*.
  Values are env-referenced from the connection's `env:`; no secret material
  lives in a config or a candidate. **Decided:** extend `providers.env` — it is
  already an env-var file loaded at startup, so it simply holds arbitrary
  connection secrets (a Figma token, a PAT) alongside provider keys, referenced
  from a connection's `env:` and validated by the same doctor credential-check.
  No new store.

## 5. What it reuses (the real spine — nothing is greenfield)

- **Connection model** — `kind: mcp` with `command`/`args`/`url`/`env` (the
  provision target).
- **Two-tool surface** — `discover`/`evaluate` are `praxec.query` verbs;
  provision/deprovision are governed workflows.
- **Discovery/search** — the existing ranked search, over a new catalog.
- **Provider-keys + doctor** — the credential-collection and validation model.
- **elicitation-mcp** — collects `requires` secrets/config.
- **`kind: rest` executor** — the `rest` registry adapter *and* REST-wrapped
  tools.
- **`uri:`/`ref:` remote-sourcing + pack currency** — registries are remote data
  with the same staleness/refresh story.
- **Currency (v0.0.43)** — the maintenance verb.
- **ADR-0006 two-tier trust + sandbox** — the provisioning trust model.
- **praxec-meta authoring flows** — the `suggest-tools` integration point.

## 6. Phasing (each its own release)

- **Phase 1 — catalog + reads.** Registry-adapter framework + `ToolCandidate` +
  `discover`/`evaluate` query verbs (read-only). First adapters: `github-org`
  (praxec org), `mcp-registry` (Official Index), one `rest` generic.
- **Phase 2 — governed provisioning, trusted lane.** `flow.tools.provision` for
  verified/org tools: install + secrets/config elicitation + validate + wire.
  `flow.tools.deprovision`.
- **Phase 3 — community lane + authoring + breadth.** Sandbox + approval for
  third-party; Smithery/Glama/PulseMCP adapters; the `suggest-tools` authoring
  integration.

## 7. Decisions (from spec review) + remaining open questions

**Decided:**

- **Capability vocabulary** — reuse the existing verb + capability model; a tool
  is described by the cap-verbs it serves, with `tags` for the finer axis
  (§3.3). No new taxonomy.
- **Secrets store** — extend `providers.env` (§4).
- **Catalog freshness** — a ~24h-TTL refresh step: auto for metadata reads,
  trust-gated for tool updates (§3.5).

**Still open:**

- **Uninstall semantics** — shared binaries / docker images / npx (nothing to
  uninstall) differ; deprovision needs a per-transport story.
- **Ranking** — how `discover`/`evaluate` rank across composed registries and
  trust tiers (verified-first? capability-match score? provenance weight?).
