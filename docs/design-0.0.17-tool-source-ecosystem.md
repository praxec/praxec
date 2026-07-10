# Design — v0.0.17 tool-source ecosystem contract

**Status:** Design (no production code). **Date:** 2026-07-10. **Branch:** `feat/v0.0.17-grant-gate`.

This is the contract that turns praxec into a platform a capable model can extend
*through* praxec: onboard a CLI / MCP / REST tool from a source, surface it as a
callable through the stable two-tool surface, and mint the workflows that maximize
it — every activation gated by an operator grant. It builds directly on the
connection grant gate that just shipped (`config.rs`, SPEC §9.5) and the
intent-evidence surface (`intent_index.rs`).

The releases sequence toward one equation (see
`docs/roadmap-v0.0.18-optimization-flywheel.md`): **maximize
`human:intent × compiled-tool:determinism × model:generation`.** This design keeps
the split honest: the *seams* (descriptor schema, tool-source executor, registry,
selector rank, grant gate) are engine Rust — deterministic; the *authoring*
(`meta/flow.onboard-tool`, the suites) is dogfooded through praxec — model
generation; and every connection a tool needs is an operator grant — human intent.

## Grounding (what already exists — do not reinvent)

| Concern | Where it lives today |
|---|---|
| Grant gate (D3) | `crates/praxec-core/src/config.rs` — `merge_declared_repos` → `gate_repo_connections` → `stamp_ungranted_connections`; stamp at `/praxec/_ungrantedConnections`; `grant_connections:` on the `repos:` entry (`RepoDecl.grant_connections`). |
| Grant enforcement at consume time | `crates/praxec-executors/src/conn_util.rs` — `ungranted_from_config` + `connection_not_found_error` → typed `UNGRANTED_PACK_CONNECTION`. Each of `cli.rs` / `mcp.rs` / `rest.rs` carries the `ungranted` map and fails typed, never silent. |
| Connection kinds | `schemas/gateway-config.schema.json` `$defs`: `connection = oneOf [mcpConnection, cliConnection, restConnection]`. mcp: `command/args/url/env/idleTimeoutMs`; cli: `command/workingDirectory/env`; rest: `baseUrl/headers`. |
| Import a live MCP server's tools | `crates/praxec-executors/src/import.rs` — `proxy.import` names an existing `kind: mcp` connection, calls `tools/list`, yields `Capability{ source: Imported{ connection, tool } }` into the proxy compiler + discovery index. |
| Adapt an external artifact to a candidate | `crates/praxec-executors/src/ingest.rs` — returns a *candidate* fragment; never publishes; the calling workflow routes it through structural-analysis → dry-run → registry gates. |
| Write a definition (governed) | `crates/praxec-executors/src/registry_executor.rs` — `DefinitionStoreWritable`, `WRITE_DISABLED` fail-fast, `allowed_connections` provenance gate. |
| Discovery | `crates/praxec-core/src/discovery/discovery.rs` — closed `DiscoveryKind` enum + `DiscoveryItem`; lexical + opt-in semantic index. |
| Intent evidence | `crates/praxec-core/src/intent_index.rs` — `IntentStats{ task_class, template_id, runs, success_rate, mean_cost_usd, evidence_runs }`; `annotate_hits_with_evidence(hits, stats, min_runs)`; wired in `crates/praxec-mcp-server/src/handlers.rs::attach_intent_evidence`; `min_runs` from tuning (default 3). |
| Provision a tool | `crates/praxec/src/provision.rs::detect` (enumerate `kind: mcp`, PATH-check `command`); ADR-0013 (`praxec.packs/v2` registry, `requires[]` + `tools[]` catalog, `providers` chain `docker → release → cargo`, doctor resolve-and-offer with consent). |
| Pack layering | `repos:` entries deep-merged before host body; namespace uniqueness (V20/§9.4); `overrides:` acknowledgment (V23). |

**Design invariant, load-bearing:** `reach ≡ a literal connections entry`,
`provision_hint ≡ packs/v2 providers`. Onboarding a tool is **copy, never
transform** — the descriptor's `reach` block is *the exact* `connection` object the
merge already understands, so no new consumer path and no new trust surface is
created. The descriptor is a *manifest over* existing primitives, not a parallel
runtime.

---

## D1 — Tool descriptor schema (schema-first, `typify`-generated)

One JSON Schema, `schemas/tool-descriptor.schema.json`, is the source of truth;
Rust types are generated from it via `typify` (schema-first, code follows schema —
consistent with `gateway-config.schema.json`). The descriptor spans **cli / mcp /
rest** with a single closed `kind` discriminator.

### Proposed top-level shape

```jsonc
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://praxec.dev/schemas/tool-descriptor.schema.json",
  "title": "PraxecToolDescriptor",
  "type": "object",
  "required": ["schema_version", "name", "version", "kind", "reach", "operations"],
  "additionalProperties": false,
  "properties": {
    "schema_version": { "const": "praxec.tool/v1" },

    // ---- identity ------------------------------------------------------
    "name":        { "type": "string", "pattern": "^[a-z0-9][a-z0-9._-]*$" },
    "version":     { "type": "string" },        // tool version (semver-ish, pinned)
    "source_repo": { "type": "string" },        // git URL / registry coordinate of origin
    "description": { "type": "string", "default": "" },
    "tags":        { "type": "array", "items": {"type": "string"}, "default": [] },
    "aliases":     { "type": "array", "items": {"type": "string"}, "default": [] },

    // ---- kind (closed enum → exhaustive match in Rust) ----------------
    "kind": { "enum": ["cli", "mcp", "rest"] },

    // ---- connection requirement (ties to D3 grant model) --------------
    "reach": { "$ref": "#/$defs/reach" },

    // ---- provisioning hint (== packs/v2 providers) --------------------
    "provision": { "$ref": "#/$defs/provision" },   // optional; absent ⇒ operator supplies reach by hand

    // ---- invocation topology ------------------------------------------
    "operations": {
      "type": "array", "minItems": 1,
      "items": { "$ref": "#/$defs/operation" }
    },

    // ---- workflows that maximize this tool ----------------------------
    "suggested_workflows": {
      "type": "array", "default": [],
      "items": { "type": "string" }   // workflow definitionIds (namespace-qualified)
    },

    // ---- v0.0.18 forward-compat (reserved; never required) ------------
    "embedding":              { "type": "array", "items": {"type": "number"} },
    "structural_fingerprint": { "type": "string" }
  },

  "$defs": {
    "reach": {
      "type": "object",
      "required": ["connection_name", "grant_as", "connection"],
      "additionalProperties": false,
      "properties": {
        "connection_name": { "type": "string" },   // key the tool's operations reference
        "grant_as":        { "type": "string" },    // bare name the operator writes in grant_connections:
        // The LITERAL connections entry — copied verbatim into /connections on install.
        // Same $ref the gateway config already validates. kind here MUST match top-level kind.
        "connection": { "$ref": "gateway-config.schema.json#/$defs/connection" },
        // Auth the connection needs, declared by NAME not value (operator fills via D4a).
        "auth": { "$ref": "#/$defs/authRequirement" }
      }
    },
    "authRequirement": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "scheme": { "enum": ["none", "env", "header", "bearer"] },
        // Names only — env var keys / header names. NEVER secret values.
        "env":     { "type": "array", "items": {"type": "string"}, "default": [] },
        "headers": { "type": "array", "items": {"type": "string"}, "default": [] }
      },
      "required": ["scheme"]
    },
    "provision": {   // mirrors ADR-0013 praxec.packs/v2 tool entry — no new model
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "mcp_registry_id": { "type": "string" },     // dev.praxec/<tool>
        "version":         { "type": "string" },
        "providers": {                               // ordered chain, first-available wins
          "type": "array",
          "items": { "enum": ["docker", "release", "cargo", "npx", "uvx"] }
        }
      }
    },
    "operation": {
      "type": "object",
      "required": ["id", "input_schema", "output_schema"],
      "additionalProperties": false,
      "properties": {
        "id":     { "type": "string" },
        "verb":   { "enum": [ /* ScriptVerb tokens: build,test,deploy,format,lint,
                                install,verify,run,inspect,search,fetch,audit */ ] },
        "input_schema":  { "type": "object" },   // JSON Schema for arguments
        "output_schema": { "type": "object" },   // JSON Schema for the result
        // kind-specific dispatch coordinates (exactly one present, matching kind):
        "mcp_tool": { "type": "string" },                          // kind: mcp
        "rest":     { "type": "object",                            // kind: rest
                      "properties": { "method": {"type":"string"}, "path": {"type":"string"} } },
        "cli":      { "type": "object",                            // kind: cli
                      "properties": { "args": {"type":"array","items":{"type":"string"}} } }
      }
    }
  }
}
```

### Why these choices

- **`kind` is a closed enum** (`cli|mcp|rest`) → Rust `enum ToolKind` with an
  **exhaustive `match`**, mirroring `DiscoveryKind` / `ScriptVerb` in
  `discovery.rs`. Adding a fourth kind is a deliberate schema amendment, not a
  config-time string (poka-yoke over fail-loud).
- **`reach.connection` `$ref`s the *existing* `gateway-config.schema.json#/$defs/connection`.**
  This is the whole design in one line: the descriptor doesn't describe a
  *new* connection format, it embeds the one the merge already validates and the
  executors already consume. Install = `copy reach.connection into /connections`.
- **`reach.grant_as` forces the D3 grant.** Onboarding never activates a tool. The
  descriptor names the bare grant token the operator must add to
  `grant_connections:`; until then the connection is diverted to
  `/praxec/_ungrantedConnections` and every operation fails typed
  `UNGRANTED_PACK_CONNECTION`. **Onboarding a tool cannot bypass the human:intent
  boundary — by construction, not by check.**
- **`auth` is names-only.** The descriptor declares *which* env vars / headers the
  tool needs, never their values. Secret material enters exactly one place: the
  operator's `px connections add` (D4a). This keeps community-shared descriptors
  credential-free.
- **`operations[].{input,output}_schema` are the typed I/O contract** the selector
  and the authoring flow read to wire a tool into a workflow's blackboard —
  consistent with how `hop_slot` typed I/O already gate composition.
- **`verb` reuses the closed `ScriptVerb` vocabulary** (`ScriptVerb::ALL_TOKENS`),
  so a tool's operations classify into the same action taxonomy scripts already use
  — no parallel vocabulary.

### D1 implementation surface (`owned_files`)

```
schemas/tool-descriptor.schema.json                     # canonical schema (typify source)
crates/praxec-core/src/tool_descriptor.rs               # typify-generated types + ToolKind
                                                        #   (closed enum, exhaustive match),
                                                        #   loader, validate(), grant-token extraction
crates/praxec-core/src/lib.rs                           # `pub mod tool_descriptor;`
crates/praxec-core/tests/tool_descriptor.rs             # parse/validate + kind-mismatch + grant-token tests
crates/praxec-core/tests/tool_descriptor_schema_snapshot.rs  # schema-drift guard (mirrors spec_enum_drift.rs)
```

---

## D2 — Tool-source executor

A descriptor is ingested from a **source** and surfaced as a callable through the
two-tool surface (`praxec.query` / `praxec.command`, SPEC §32). D2 is *not* a new
runtime path — it composes existing executors:

1. **Fetch + parse.** A `kind: ingest` variant (extend `ingest.rs`, which already
   "adapt[s] external guidance sources to the Praxec fragment shape" and *returns a
   candidate, never publishes*) reads a descriptor file/URL and validates it against
   D1. Output is a *candidate* — routed by the calling `meta/flow.onboard-tool`
   through the same structural-analysis → dry-run → registry gates every authored
   artifact passes.
2. **Install = copy reach.** On operator acceptance, the descriptor's
   `reach.connection` is written verbatim into the pack's `connections:` block and
   `reach.grant_as` is surfaced as the grant the operator must add. Nothing is
   transformed; the connection is now a first-class `/connections` entry the
   merge + grant gate govern.
3. **Surface as callable.** For `kind: mcp` the existing `import.rs` path
   (`proxy.import` → `tools/list` → `CapabilitySource::Imported`) already turns a
   live MCP connection's tools into discoverable capabilities + proxy transitions —
   D2 reuses it once the connection is granted. For `kind: cli` / `kind: rest`, the
   descriptor's `operations[]` compile into workflow steps whose `executor.kind` is
   `cli` / `rest` and whose `executor.connection` is `reach.connection_name` — the
   executors already resolve those and already fail typed on ungranted.

**Reused patterns:** `ingest.rs` (candidate, never-publish), `registry_executor.rs`
(governed write behind `write_enabled`, `allowed_connections` provenance),
`import.rs` (MCP tool surfacing), `provision.rs` + ADR-0013 doctor chain
(materialize the binary). D2 writes **zero** new trust surface: every byte the tool
can reach flows through a granted `/connections` entry.

---

## D4b — Registry-v3 (`praxec/packs` structured GitHub registry)

Registry-v3 extends the ADR-0013 `praxec.packs/v2` registry from a *tool-provider
catalog* to a **topology**: tools × capabilities × workflows, plus layerable
community tool-packs.

- **A pack is a `repos:` layer** carrying `{ tool descriptors (D1) + the workflow
  suites that maximize them }`. It composes through the *existing* repo machinery:
  deep-merge, namespace uniqueness (V20), `overrides:` acknowledgment (V23), and —
  critically — the grant gate. A community pack can ship a descriptor whose `reach`
  declares an MCP connection, and **that connection is inert until the operator adds
  it to `grant_connections:`** for that repo. Layering a hostile pack cannot
  auto-wire anything.
- **The crossmatrix** is the registry-level index: for each tool, the capabilities
  it exposes (`operations[]`) and the `suggested_workflows[]` that compose it; for
  each workflow, the tools it depends on (derivable from its steps'
  `executor.connection` refs). This is the `topology_refs` the selector reads. At
  today's corpus scale (~17 flows) it is a static index — a model reads the YAML
  directly; no learned structure needed (de-solution, per the roadmap).
- **Composition with D3:** the registry describes *what a pack offers*; the
  `grant_connections:` list on the operator's `repos:` entry decides *what
  activates*. Registry-v3 never grants — it makes the grant *legible* (the
  descriptor's `grant_as` + `auth` names tell the operator exactly what they're
  authorizing and what secrets it will need).

Registry-v3 owns `packs.yaml` (schema `praxec.packs/v3` = v2 + `descriptors[]` +
`crossmatrix`) in the `praxec/packs` GitHub repo; praxec is a *consumer* of it, as
with the MCP registry (ADR-0013 §4).

---

## D6 — Selector annotate/rank

The selector ranks tool/workflow candidates using **already-computed** signals —
it introduces no new scoring model in v0.0.17.

- **Evidence (existing).** `annotate_hits_with_evidence` already attaches
  `IntentEvidence{ runs, success_rate, mean_cost_usd }` to `kind: workflow` search
  hits, gated at `min_runs` (default 3) so thin samples read as *absent*, never as
  `0%` noise. The selector orders workflow candidates by
  `(success_rate desc, mean_cost_usd asc)` among those clearing the evidence bar,
  falling back to lexical/topology score when no evidence exists.
- **Topology (new, deterministic).** For a task that needs a tool, rank tools by
  crossmatrix fit: does an `operation`'s `output_schema` satisfy the step's typed
  input? how many `suggested_workflows` already compose it (adoption)? The rank is a
  pure function over the registry index + the evidence table — no model in the loop
  (deterministic tools own selection; the model owns *generation* of the resulting
  workflow).
- **Annotation, not gatekeeping.** Following the shipped pattern
  (`handlers.rs::attach_intent_evidence` — "evidence is an annotation, never a
  filter"), the selector *surfaces* rank + evidence on `praxec.query` hits so the
  caller (human or model) chooses informed, and never silently drops a candidate.

**Producer ≠ evaluator** is preserved: "success" is the runtime's deterministic
outcome done-signal (`outcome.recorded`, ≥1 declared outcome), never a model
grading itself — inherited unchanged from `intent_index.rs`.

---

## D4a — Connections-write (`px connections add`) seam

`px connections add` (built in parallel) is the operator-facing write path for a
`/connections` entry. The seam D1 must expose to it:

- The descriptor's `reach.connection` is the **exact payload** `px connections add`
  writes — same `$defs/connection` shape. `connections add --from-descriptor
  <path>` reads a D1 descriptor and writes `reach.connection` under
  `reach.connection_name`, then prints the `grant_connections:` line the operator
  must add (`reach.grant_as`) and the `auth` names they must populate.
- **The grant is a separate, explicit step.** `connections add` writes the
  connection body; it does **not** write the grant. This mirrors the config-merge
  split (a pack *declares*; the operator *grants*) so the CLI cannot become a
  grant-laundering path. The typed `UNGRANTED_PACK_CONNECTION` remedy string
  (`conn_util.rs`) is the exact text the CLI echoes.
- **Auth values** are supplied here and only here (env/header names come from the
  descriptor's `auth`; values from the operator's shell/secret store), keeping
  shared descriptors credential-free.

---

## v0.0.18 forward-compat hooks (attachment points only)

Reserved `#[serde(default)]` slots so v0.0.18 populates them with **no breaking
schema change** (see roadmap §"Forward-compat locked in v0.0.17"):

| v0.0.18 mechanism | Attachment point (reserved now) |
|---|---|
| Workflow-description embeddings | `DiscoveryItem` already embeds via `item_embed_text` (`discovery.rs`); `SemanticDiscoveryIndex` is the consumer — no descriptor change. |
| Tool-description embeddings | `tool_descriptor.embedding: [number]` (reserved above); indexed by the same `SemanticDiscoveryIndex` once a dependable embedder + re-embed-on-reload lands. |
| Workflow-structure embeddings / dedup | `tool_descriptor.structural_fingerprint: string` (reserved) + a canonical structural fingerprint on workflow definitions; feeds praxec-meta `flow.optimize-*`. |
| Learned selector policy | D6's annotate/rank is the input surface; the accrued `{task_class, template} × success × cost` volume becomes the policy's training signal. No schema change — it reads the same `IntentStats`. |

These are *attachment points*, not implementations. v0.0.17 ships lexical +
crossmatrix + evidence-annotation, which the roadmap establishes as sufficient for
a fable-class model — embeddings are correctly deferred until the embedder is
dependable.

---

## FMECA-lite — the contract's top failure modes

| # | Failure mode | Prevent | Detect | Fail-fast |
|---|---|---|---|---|
| FM1 | Onboarding a tool auto-activates its connection (supply-chain bypass) | `reach` embeds a connection that the grant gate diverts to `/praxec/_ungrantedConnections` until granted — activation is impossible without an operator grant, by construction | `multi_repo_loading.rs`-style test: ungranted descriptor connection is absent from live `/connections` | Every operation fails typed `UNGRANTED_PACK_CONNECTION` with the exact grant remedy (`conn_util.rs`) |
| FM2 | `kind` and `reach.connection.kind` disagree (cli descriptor, mcp connection) | Schema: top-level `kind` and `reach.connection.kind` cross-checked in `validate()` | Load-time validation over the descriptor | Reject at parse with `TOOL_KIND_MISMATCH`; no partial install |
| FM3 | Secret values leak into a shared descriptor | `auth` is names-only (schema forbids values); values enter only via `px connections add` | Schema `additionalProperties: false` on `authRequirement`; review test asserts no value-shaped fields | Descriptor with a value-bearing auth field fails schema validation |
| FM4 | Descriptor references a workflow / operation that doesn't resolve | `suggested_workflows[]` are definitionId refs validated against the merged registry (reuse `validate_workflow_refs_resolve`) | Config-load walk (same site as V22) | `STALE_TOOL_SUGGESTION` at load, mirroring `STALE_OVERRIDE` |
| FM5 | Silent tool provisioning (binary installed behind operator's back) | Provisioning reuses ADR-0013 doctor *offer-with-consent*; never runs a provider command implicitly | `provision::detect` reports present/missing; doctor shows the exact command | Missing binary → connection fails at spawn with a doctor pointer; no auto-install |
| FM6 | Thin/absent evidence read as a real signal in selection | `annotate_hits_with_evidence` omits pairs below `min_runs` (default 3); 0-outcome runs are never success evidence | `intent_index.rs` tests (`zero_outcome_runs_are_counted_but_not_success_evidence`) | Selector ranks by lexical/topology when evidence is absent — never surfaces `0%` as evidence |
| FM7 | Registry topology drifts from the actual pack contents | Crossmatrix is *derived* from descriptors + workflow step refs, not hand-maintained | Schema-snapshot + a registry-consistency test | Divergent entry fails the registry build; pack is not layerable until consistent |

## The equation, honored

- **Humans own intent** — every connection activation is an operator grant (D3);
  onboarding surfaces the grant, never performs it.
- **Deterministic tools own verification** — the descriptor schema, grant gate,
  field-copy install, typed I/O, selector rank, and provision chain are engine Rust;
  evidence is the runtime's deterministic outcome signal, never a model self-grade.
- **Models own generation** — `meta/flow.onboard-tool` and the workflow suites are
  authored *through* praxec, gated by the same structural-analysis → dry-run →
  registry path as all authoring.
