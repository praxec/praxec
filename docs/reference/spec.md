# Workflow guidance, blackboard, transition records & versioning — design spec

**Date:** 2026-05-22
**Status:** Draft v2 — revised after an FMECA architecture review; supersedes the
pre-review draft.
**Scope:** `praxec-core` and `praxec-executors`. No new MCP tools —
the two-tool surface (`praxec.query` / `praxec.command`) is unchanged.

## 1. Summary

Four additions to workflow definitions. All are opt-in; existing configs stay
valid. Each survived an architecture-validity review (§4).

1. **Guidance** — reusable "how to think" text, delivered in **two tiers**: a
   small *inline* payload (decision-critical, every response, `{{ }}`-templated)
   and a *referenced* tier (larger reusable fragments, surfaced as keys, fetched
   on demand via `praxec.query({subject})`). Inlining everything would
   re-create the context bloat this product exists to remove.
2. **Blackboard slot declaration** — name the workflow `context` slots so guards
   and templates are statically checkable. Per-slot typing is optional.
3. **Transition records** — every applied transition emits one typed, schema'd
   event through the **existing `AuditSink`** (not a new subsystem), written to
   date-rotated files. This is the basis for run reconstruction.
4. **Versioned definitions** — each workflow carries a version; an instance pins
   to — and carries — its creation-time definition; nothing is deleted.

## 2. Motivation

- **Guidance bloat.** Per-state `guidance` is re-authored and re-sent every
  turn. Reusable, on-demand-fetched guidance keeps the per-turn payload bounded.
- **Stringly-typed guards.** `expr` guards reference `$.context.X` against an
  untyped bag; a typo fails at runtime, not at `check` time.
- **Definition drift.** SIGHUP hot-reload can swap a workflow definition under a
  running instance — the state it sits in may vanish.
- **Traceability.** A snapshot-only store cannot answer "what did the model do
  on loop iteration 3, and why." A transition record stream can.

## 3. Non-goals

- **No LLM-driving, no autonomous learning.** The crate does not call, train, or
  tune a model, and the model never edits governance. Guidance improves only
  through human-authored new versions (§5.6).
- **No graph-free blackboard control.** The state→transition graph stays the
  control spine; the blackboard only feeds guards.
- **No event sourcing.** The `WorkflowStore` snapshot is authoritative; the
  transition record stream is a durable side-effect, never a recovery source.
- **No parallel abstractions.** Transition records ride the existing `AuditSink`
  — no second logging subsystem.
- **No archive lifecycle management.** The crate writes append-only files and
  never deletes them; retention, tiering, backup and legally mandated erasure of
  those files are the operator's filesystem responsibility.
- **No new MCP tools.** Two in, two out (`praxec.query` + `praxec.command`).

## 4. Considered and cut (FMECA review)

Recorded so they are not re-proposed:

| Element | Verdict | Reason |
|---|---|---|
| Standalone `TransitionLog` port + file/stdout/memory sinks | **Cut** | Duplicates `AuditSink` and its impls — a parallel abstraction. Transition records are a typed audit event instead (§7). |
| Declared migrations (`__migrate__`, totality `check`, `MIGRATION_FAILED`) | **Cut** | Version pinning + natural drain already makes hot-reload safe. Forced in-flight upgrade is a rare need; revisit with evidence. |
| Required `summary` on every agent transition | **Cut** | A breaking protocol change betting on an unproven behaviour. The slot stays, optional (§6.3). |
| Per-slot JSON-Schema typing as the default | **Demoted** | Slot *name* declaration is enough for the high-value `check`. Typing is opt-in (§6.2). |
| Skill packs / Agent-Skills interop / decomposition | **Deferred** | Speculative; depends on an ecosystem that does not exist. |
| Autonomous guidance learning | **Rejected** | Breaks the immutability invariant (§8), the trust boundary (model-authored governance content), and crate scope. |

## 5. Guidance

> **Note — "guidance" *is* "skills".** The two words name the same thing:
> reusable instruction text. They are not two features. The referenced tier is
> limited by the **same HATEOAS-inspired discovery the gateway already uses for
> MCP tool menus** — advertise a small key menu, fetch a body on demand — so
> guidance never bloats the model's context any more than the two-tool
> surface does. It is the founding principle applied consistently: don't dump
> the library, advertise it and let the client pull what it needs.

### 5.1 One concept, two tiers

From the LLM's perspective there is one thing: rendered instruction text. It
cannot distinguish inline text, a templated value, or a reusable fragment.
Guidance is therefore *one concept* delivered in two tiers, split by
**criticality**:

| Tier | What | Delivery | Rationale |
|---|---|---|---|
| **Inline** | `goal` + a short situational line; `{{ }}` live values | in every response | small, bounded, decision-critical *now* |
| **Referenced** | reusable fragments ("skills") — larger "how we work" text | a surfaced key; body fetched via `praxec.query({subject})` (see §32) | the repeat-offender bloat; fetched once, then in the model's own memory |

### 5.2 Inline tier — templated

`goal` and `guidance` on a state stay plain strings (unchanged shape) and become
**templates**: `{{ }}` placeholders interpolate against the live workflow before
the string is sent.

```yaml
states:
  ready_to_deploy:
    goal: Confirm deployment
    guidance: >
      Lint clean; {{ $.context.testCount }} tests green. Review before deploying.
```

Placeholders use the same `$.`-rooted paths as guards: `$.context.*`,
`$.workflow.input.*`, `$.workflow.*`. Interpolation is single-pass and
non-recursive (a value containing `{{ }}` is never re-expanded). An unresolved
placeholder renders as a marked stub — `(testCount: unset)` — never an error.
The `{{workflowId}}` substitution in `reliability.rs` is the existing primitive;
this generalises it.

### 5.3 Referenced tier — guidance fragments

A **guidance fragment** ("skill") is a named, reusable block of static markdown.
Fragments are declared once in a top-level `skills:` map:

```yaml
skills:
  review.style.house-voice:    # the map key IS the fragment's subject
    verb: review
    lifecycle: stable
    body: |
      # House voice
      Lead with the reader's problem. Short sentences. No hedging.
  deploy.safety.checklist:
    verb: review
    lifecycle: stable
    body: |
      # Deploy safety
      Confirm rollback path, error budget, and on-call coverage.
```

`body` is static — **referenced fragments are never templated**, so a body
fetched and cached on turn 3 can never be stale on turn 9. Live values belong in
the inline tier.

**Required fields** on every fragment: `verb`, `lifecycle`, `body`. All three
are required (no defaults — a missing field fails config-load with
`MISSING_VERB`, `MISSING_LIFECYCLE`, or `MISSING_BODY` respectively). An
optional `source` field records provenance for fragments pulled from external
libraries (see §19); fragments declared inline carry `source: "config"`
implicitly.

### 5.4 Surfaced refs — `verb` + `subject` (poka-yoke)

Every response surfaces the fragments in scope so the model knows what it *can*
fetch — the model cannot look up what it cannot see. Each surfaced ref is a
small object with bounded fields; no body content appears in the listing.

**Ref shape:**

```jsonc
{ "verb": "review",
  "subject": "review.style.house-voice",
  "title":   "House voice (optional human-readable label)",
  "hash":    "sha256:9c1d…" }
```

- **`subject`** — the fragment's `skills:` map key; also the `praxec.query({subject})`
  lookup handle (see §32). Required.
- **`verb`** — one of eight closed cognitive operations (see below). Required.
- **`hash`** — `sha256:` prefix + hex digest of the **normalized** body (see
  §5.7). Required. Enables cache invalidation: when the body is edited, the
  hash flips; previously-cached refs are stale.
- **`title`** — optional, human-readable. Never carries body content.

No `excerpt`, `preview`, or `body` field exists on a ref — by design. The
listing carries discovery metadata only; bodies cross the wire exactly once,
on demand, via `praxec.query({subject})` (§32 + §30.10).

#### 5.4.1 Closed `verb` vocabulary (poka-yoke)

`verb` is a **closed enum** of ten cognitive operations. Unknown verbs fail
config-load with `INVALID_VERB { verb, allowed: [...] }`. There is no escape
hatch — no `Other(String)` variant, no opt-in extension. Adding a verb requires
a deliberate spec amendment (see §23.7 for the criterion), not authoring
convention.

| Verb | Cognitive posture | Use when… (vs neighbor) |
|---|---|---|
| `triage` | classify, prioritize, route | something needs categorization or routing (not "find root cause" — that's `diagnose`) |
| `diagnose` | find root cause | answering "why is X broken?" (not "what do we know about X?" — that's `research`) |
| `plan` | design approach before acting | sequencing steps before execution (not "explore options" — that's `research`) |
| `implement` | produce / generate the artifact | creating the artifact (not restructuring existing — that's `refactor`) |
| `review` | evaluate against criteria | judging a proposed artifact (not grading a scan — that's the `audit` script verb) |
| `refactor` | restructure preserving behavior | reshaping code/text without changing semantics (not creating new — that's `implement`) |
| `explain` | build understanding (self-explain or teach others) | expanding a concept (not condensing — that's `summarize`) |
| `compose` | assemble parts into a whole | integration / synthesis from existing parts |
| `research` *(v0.3)* | gather context from sources (web, local, docs) | open-ended information-gathering (not specific "why" — that's `diagnose`; not classification — that's `triage`) |
| `summarize` *(v0.3)* | condense | compressing what's known (not expanding — that's `explain`) |

The verbs are **cognitive postures**, not methodologies. Methodologies (TDD,
spec-driven, design-by-contract) are workflow shapes that sequence the ten
verbs — see §17. Posture *modifiers* (speedrun, improvise, code-golf) belong
in the body of a fragment or the framing of a workflow state, not in the verb
metadata.

The model reads a ref as `"{verb} {subject}"` — `review review.style.house-voice`
— and fetches the body with `praxec.query({subject: "review.style.house-voice"})`
only if relevant (§32).

#### 5.4.2 Blessed `subject` namespace roots (poka-yoke)

`subject` is a dotted namespace. The first segment is a **blessed root**;
segments below the root are free-form.

Blessed roots:

| Root | Scope |
|---|---|
| `review.*` | evaluation guidance (code, plan, security, data, style…) |
| `authoring.*` | composing workflows or skills |
| `debug.*` | diagnosis / triage / reproduction |
| `deploy.*` | release-time guidance |
| `import.*` | external-source ingest |
| `lifecycle.*` | drafting / completing / archiving |
| `plan.*` | design — with two conventional second-level paths: `plan.specify.*` for durable artifacts (ADRs, RFCs, contracts, interfaces, acceptance tests), `plan.execute.*` for short-term sequencing (PR scope, sprint breakdown) |

Additional verb-mirror roots (the runtime accepts every cognitive verb token
as a blessed root for symmetry with the verb taxonomy): `triage.*`,
`diagnose.*`, `implement.*`, `refactor.*`, `explain.*`, `compose.*`,
`research.*` *(v0.3)*, `summarize.*` *(v0.3)*. Total: 15 blessed roots.

A subject whose first segment is outside the blessed set produces a diagnostic.
Behavior depends on `praxec.strict_namespacing` (default `true`):

- `strict_namespacing: true` — unblessed root fails config-load with
  `INVALID_SUBJECT_ROOT { subject, blessed_roots: [...] }`. **This is the
  default.**
- `strict_namespacing: false` — unblessed root surfaces a warning diagnostic
  in `startup_diagnostics()` and via the `gateway.diagnostics` tool, but load
  succeeds. The diagnostic message includes the Levenshtein-closest blessed
  root as a suggested alternative.

**Poka-yoke — malformed descriptors are unrepresentable, not merely linted.**
`subject` is constrained by schema pattern
`^[a-z][a-z0-9-]+(\.[a-z][a-z0-9-]+)+$` — lowercase, kebab, dotted, at least
two segments, **no whitespace**. The empty subject is rejected with
`EMPTY_SUBJECT`.

### 5.5 Scopes & response shape

Fragments are referenced at three scopes; the surfaced ref appears wherever the
scope is active:

```yaml
workflows:
  content_publish:
    skills: [review.style.house-voice]    # workflow scope — every response
    states:
      drafting:
        goal: Write the draft
        skills: [review.editorial.checklist] # state scope — in this state
        transitions:
          submit_draft:
            target: reviewing
            skills: [review.style.tone-for-review] # transition scope — on this link
```

The response `guidance` object carries the inline tier and the referenced-tier
menu together:

```jsonc
"guidance": {
  "goal": "Write the draft",
  "instructions": "…rendered inline guidance…",
  "refs": [
    { "verb": "review", "subject": "review.style.house-voice",
      "hash": "sha256:9c1d…" },
    { "verb": "review", "subject": "review.editorial.checklist",
      "hash": "sha256:a8f2…" }
  ]
}
```

`check` lints: a `skills:` ref with no matching `skills:` entry → **error**;
more than ~4 refs surfaced at one scope → **warn** (the menu is itself
payload); a `subject` outside blessed roots → **error** under default
`strict_namespacing` (warning otherwise).

### 5.6 Guidance evolution (emergent, not a feature)

Guidance improves through a human-and-version-driven loop, which is an *emergent
property* of the other sections, not a component:

- **observe** — transition records (§7) show which guidance preceded which
  outcomes;
- **refine** — a human edits guidance;
- **apply safely** — the edit is a new definition version (§8); in-flight
  instances ignore it, new ones adopt it; archive-never-delete allows comparing
  version N against N+1.

An "LLM proposes a guidance diff → human approves → new version" flow is
expressible *on* praxec as an ordinary human-approval workflow (the
`content-publish` example with a guidance diff as the content); it ships as an
`examples/` config, not as crate code.

### 5.7 Content-addressed bodies + cache invalidation

Every fragment's body is **normalized**, then SHA256-hashed; the hash is
attached to every emitted ref. The model sees a fresh hash whenever the body
changes — its cached body is invalidated by virtue of the ref being different.

**Normalization rule** (single canonical implementation; see TRIZ note in §5.8
for why it is centralized):

1. Trim leading and trailing whitespace from the body.
2. Replace each run of internal whitespace (spaces, tabs, line breaks) of
   length ≥1 with a single space.
3. Strip a trailing newline if present after step 2.

The hash is `sha256:` followed by the lowercase-hex digest of the normalized
body's UTF-8 bytes. A whitespace-only edit produces an identical hash; a
semantic edit produces a different hash. Whitespace within fenced code blocks
follows the same rule — guidance bodies are not source code; they are prose
the model reads, and whitespace stability matters more than verbatim
preservation of formatting.

The hash is **required** on every fragment ref (no `Option<String>`). At
config-load, the gateway recomputes hashes from bodies and compares against
any stored hash; a mismatch fails fast with `HASH_MISMATCH { subject, stored,
computed }`.

**Cross-implementation invariant:** every component that hashes a body MUST
import the same `normalize_for_hash()` function. Two independent
implementations of "normalize whitespace" produce the same hash with
probability 1 only by exhaustive accident; the spec mandates a single
source-of-truth function and a test that asserts read-side and write-side
agree on a fixture corpus.

### 5.8 Audit of body retrieval

`gateway.describe { subject }` is a body-retrieval call. Every call emits a
typed audit event so a workflow's authoring trail captures *which* guidance
the model fetched, *when*, and under *which* correlation:

```jsonc
{ "eventType":    "guidance.describe_requested",
  "subject":      "review.style.house-voice",
  "verb":         "review",
  "workflowId":   "wf_8f3a",        // null when called outside a workflow context
  "correlationId": "cor_a91…",       // null when called outside a workflow context
  "principal":    "agent:claude",
  "outcome":      "ok",              // "ok" | "failed"
  "errorCode":    null,              // "GUIDANCE_DESCRIBE_FAILED" on failure
  "timestamp":    "2026-05-24T14:03:11Z" }
```

`praxec.query` with a `subject` argument (the describe mode, §32) is a
non-critical-path audit (per §7.3 terminology): a sink failure during the
describe-audit emission **does not** abort the describe call, but it MUST emit
an `audit.write_failed` self-event so the failure is observable. This differs
from `workflow.transition` records, which abort the transition on sink failure (§7.3).

### 5.9 Acknowledgment as a guard kind — semantic limit (TRIZ-bounded)

For workflows where reading the guidance before acting genuinely matters
(e.g. a review-style workflow that *requires* the reviewer to have consulted
the rubric), the runtime exposes a `guidance_acknowledged` guard kind (full
guard mechanics in §17). This guard fails until
`praxec.query({ subject: "<subject>" })` (§32) has been called for the named
subject within the **same workflow correlation**.

**Semantic limit (irreducible, documented as a constraint):** the gateway can
verify the model *fetched* the body. It cannot verify the model *read* or
*comprehended* it. The guard is a fetch-and-freshness proof, not a
comprehension proof.

**TRIZ resolution (Asymmetry — treat ack as time-bounded scope, not
permanent):** the ack is tied to `(correlation_id, subject, body-hash-at-ack-time)`.
If the body's hash changes after the ack but before the gated transition, the
ack is invalidated and the model must re-fetch. This converts the gate from
"trust that one describe call satisfies forever" into "trust that the
description seen was the current one." The semantic limit remains; the
TRIZ-resolved gate is meaningful within its scope.

## 6. Blackboard slots

### 6.1 Slot declaration

The "blackboard" is the existing `WorkflowInstance.context` — `output:` mappings
write it, `expr` guards read `$.context.X`. The only addition is **declaring the
slot names**, so guards and templates can be statically checked:

```yaml
workflows:
  deploy_pipeline:
    blackboard: [lintPassed, testCount, coverage, artifactId]
```

`check` warns when an `output:` mapping writes a slot absent from `blackboard:`.

### 6.2 Optional typing

A slot may optionally carry a JSON-Schema fragment instead of a bare name:

```yaml
    blackboard:
      testCount: { type: integer }
      artifactId: { type: string }
```

When a slot is typed, `output:` writes to it are validated and a mismatch raises
`BLACKBOARD_TYPE_ERROR` before the transition advances. Untyped (name-only)
slots are the default and are sufficient for use-before-def (§9).

### 6.3 The optional `summary` slot

`summary` is a reserved, **optional** string slot. `praxec.command` (submit
mode) accepts an optional top-level `summary`; when present it is stored to
`context.summary` and surfaced in every response and `praxec.query({workflowId})`,
letting a model resume a workflow cold. It is **never** a guard input (model-authored content is untrusted; this
is why guards may not read `$.context.summary` — `check` errors on that). It is
not required and has no enforcing config knob.

## 7. Transition records

### 7.1 A typed audit event — not a subsystem

`praxec` already has `AuditSink` (`Null`/`Stderr`/`Memory`/`File`). A
transition record is **one well-typed audit event** (`event_type:
"workflow.transition"`) carrying a payload that conforms to a canonical schema.
No `TransitionLog` port, no parallel sink tree.

### 7.2 Record shape

```jsonc
{
  "workflowId":        "wf_8f3a",
  "definitionId":      "content_publish",
  "definitionVersion": "2026-05-22",
  "seq":               5,                 // == resulting WorkflowInstance.version
  "timestamp":         "2026-05-22T14:03:11Z",
  "fromState":         "drafted",
  "toState":           "review",
  "transition":        "submit_draft",
  "actor":             "agent",           // agent | deterministic | human | system
  "principal":         "user:matt",
  "guards":            [ { "kind": "expr", "result": true } ],
  "arguments":         { "draft": "…" },
  "blackboardDelta":   { "documentId": "doc_2291" },
  "executor":          { "kind": "rest", "ok": true, "durationMs": 142 },
  "childWorkflowId":   null,              // set when executor kind == workflow
  "correlationId":     "…"
}
```

This payload is a **canonical schema** — `transition-record.schema.json`,
`typify`-generated (§10). Each applied transition — including each deterministic
chain hop — increments `version` by one and emits exactly one record; `seq` is
that `version`.

### 7.3 Snapshot authoritative; at-least-once; fail-fast

The `WorkflowStore` snapshot stays authoritative (`save_if_version` optimistic
locking). The record is a **durable side-effect of commit**, ordered
record-first:

1. durably write the transition record;
2. commit the snapshot.

If step 1 fails the transition **fails fast** (`RECORD_WRITE_FAILED`) and the
action does not happen — there is no path to a committed-but-unrecorded
transition. If step 1 succeeds but step 2 fails, the retry re-writes the record;
readers de-dupe by `(workflowId, seq)`. Recovery loads the snapshot, as today —
the record stream is never replayed for live state.

### 7.4 Date-rotated files

The `File` audit sink gains date rotation: `YYYY-MM-DD-{name}.log`, interval
configurable (`daily` default; `hourly`/`weekly`). Transition events route to
`…-transitions.log`, other audit events to `…-audit.log` — one rotating-file
writer, two `{name}`s. Files are append-only and never deleted by the crate.

### 7.5 Reconstruction

For any transition at any past time the system reconstructs **what the model
did, when, and why**, from retained files alone:

| Question | Source |
|---|---|
| what / when | the transition record |
| why it was legal | the retained definition version (§8) + recorded guard results |
| what the model reasoned over | blackboard at that `seq`, replayed from `blackboardDelta` |
| what the model was told | guidance for that state, re-derived from the retained definition version |

Because the gateway *is* the governance layer, "why" is causal: it knows the
legal moves it offered, which guards passed, and what guidance it served.

## 8. Versioned definitions

### 8.1 Version discriminator

Each workflow definition carries `version:` — an opaque unique string; an ISO
"as-of" date is the recommended convention (`version: 2026-05-22`). A workflow
without `version:` gets a default and behaves as today.

A workflow may also declare a top-level `inputs:` block as a concise alternative to `inputSchema:`. At config load, `inputs:` is compiled into a synthesized `inputSchema` (JSON Schema `object` with `properties` for each named input, carrying `type`, `default`, and any other declared fields). A per-input `required: true` is lifted to the schema-level `required` array. This means `inputs.<name>.default` values are applied and types validated at `workflow.start` via the same `apply_schema_defaults` / `validate_schema` path that handles transition-level `inputSchema`. If the workflow already declares an explicit `inputSchema`, `inputs:` is ignored.

### 8.2 Instances carry their definition snapshot

A workflow definition version is a **complete, immutable, self-contained
snapshot** — states, transitions, guards, blackboard slots, guidance and the
*resolved fragment bodies* it references. At `praxec.command({definitionId})`
(workflow start) the resolved snapshot is stored **with the instance** in the
`WorkflowStore`. The §30.10.4 pre-start subject walk runs at this point,
ensuring every reachable lexicon subject is fully defined before the snapshot
is pinned — guaranteeing the snapshot-immutability invariant is never
violated by a `PENDING_DEFINITION` placeholder.

Consequence (FMECA mitigation): a running instance never depends on an external
definition file. Editing config, or deleting archived files, cannot strand an
in-flight workflow — it carries everything it needs. Editing a fragment or a
guard has no effect on running instances; it reaches only instances started
under a new version.

### 8.3 Hot-reload is additive; archive-never-delete

On SIGHUP, the incoming config's definitions are *added*; new
`praxec.command({definitionId})` calls use the newest version; in-flight
instances are untouched and drain on their pinned version. Superseded definition versions are retained on disk, never
deleted by the crate (their lifecycle is the operator's — §3). There are no
declared migrations (§4): pinning plus natural drain is the whole mechanism.

### 8.4 Bypass path: authoring-workflow registry writes

The reference authoring workflow (§17) needs to *publish* new definitions
back to the gateway. Two safeguards make this safe:

1. **Feature flag, default off.** `praxec.authoring.write_enabled` is the
   single switch. Default `false`. The flag is read at gateway startup and
   is **not runtime-mutable**: a workflow YAML that contains this key
   anywhere within `workflows:` is rejected at config-load with
   `CONFIG_FLAG_NOT_RUNTIME_MUTABLE`. An LLM-authored workflow cannot
   silently enable its own write path.

2. **Audit-before-commit ordering** (mirrors §7.3 record-first):
   - The `registry` executor (§17.2) emits `definition.published` to the
     audit sink BEFORE the new snapshot becomes loadable.
   - If audit emission fails, the commit is aborted; the new definition is
     NOT made loadable; the executor returns `RECORD_WRITE_FAILED`.
   - Successful commit fires `definition.loadable` post-commit (best-effort
     audit, mirrors §5.8 non-critical-path semantics).

Trait shape:

```rust
// crates/praxec-core/src/ports.rs
#[async_trait]
pub trait DefinitionStoreWritable: DefinitionStore {
    async fn register(&self, id: &str, definition: Value) -> Result<(), DefinitionStoreError>;
}
```

The writable variant is constructed only when the flag is on; runtime call
sites hold `Option<Arc<dyn DefinitionStoreWritable>>` and pass `None` when
disabled. The registry executor checks for `None` and fails fast with
`WRITE_DISABLED`.

**Bypass-path-of-the-bypass-path:** in a deployment where the authoring
workflow itself becomes unrunnable (e.g. malformed by a published edit), the
operator may set `praxec.authoring.write_enabled: true` AND author a
fix via the standard config-reload path (§8.3). The audit event
`definition.bypass_published` fires for any registry write made by a
principal carrying the `authoring` role, so direct-write usage is always
visible.

## 9. Control & guards

The control spine is unchanged: the declared state→transition graph with
`guards:` lists and `linkFilter`. The one addition is static checkability.

`check` gains **use-before-def**: an `expr` guard or `{{ }}` template that reads
`$.context.X` must have a reachable predecessor transition whose `output:`
writes `X`. A guard referencing an undeclared slot, or `$.context.summary`, is a
`check` error. The runtime remains the backstop — a guard hitting an unset slot
fails fast with rich context, never a silent `false`.

## 10. Schema surfaces

Boundary contracts get canonical JSON Schemas in `/schemas`, `typify`-generated;
internal types stay hand-written Rust.

| Schema | Boundary | Status |
|---|---|---|
| `gateway-config.schema.json` | author → gateway | exists; extended |
| `workflow-response.schema.json` | gateway → MCP client | exists; extended (`guidance.refs`) |
| `transition-record.schema.json` | gateway → disk / trace tooling | **new** |

The request schemas (tool argument shapes) remain Rust-first in
`praxec-mcp-server`; that pre-existing asymmetry is real tech debt but is
**out of scope for this spec** — a separate ticket.

## 11. `check` additions

| Check | Level |
|---|---|
| `skills:` ref resolves to a declared fragment | error |
| `verb` is one of the eight closed cognitive verbs (§5.4.1) | error (load-time) |
| `subject` matches `^[a-z][a-z0-9-]+(\.[a-z][a-z0-9-]+)+$` | error (load-time) |
| `subject` first segment is a blessed root (§5.4.2) | error if `strict_namespacing: true` (default); warn otherwise |
| `lifecycle` is one of `experimental`/`stable`/`deprecated` | error (load-time) |
| fragment `hash` matches `normalize_for_hash(body)` recomputed at load | error (load-time) |
| guard / template `$.context.X` resolves to a declared slot | error |
| guard reads `$.context.summary` | error |
| use-before-def: guard/template slot has a reachable writer | error |
| `output:` writes an undeclared slot | warn |
| more than ~4 refs surfaced at one scope | warn |

## 12. Wire format

```jsonc
→ praxec.query { "workflowId": "wf_8f3a" }
← { "workflow": { "id": "wf_8f3a", "version": 4, "state": "drafting" },
    "guidance": {
      "goal": "Write the draft",
      "instructions": "Draft from the approved outline.",
      "refs": [ { "verb": "review", "subject": "review.style.house-voice",
                  "hash": "sha256:9c1d…" } ] },
    "links": [ { "rel": "submit_draft", "method": "praxec.command", … } ] }

→ praxec.query { "subject": "review.style.house-voice" }    // model chooses to fetch
← { "kind":     "guidance",
    "subject":  "review.style.house-voice",
    "verb":     "review",
    "lifecycle": "stable",
    "hash":     "sha256:9c1d…",
    "body":     "# House voice\n…" }
```

The `praxec.query({subject})` call (describe mode) emits a
`guidance.describe_requested` audit event — note: `"workflow.transition"` in
the event_type field is an audit-payload value, distinct from the tool name.
(See §5.8.) The body is fetched once per workflow's life; subsequent
references to the same subject from the same correlation reuse the cached body
unless the ref's `hash` differs (cache invalidation, §5.7).

## 13. Config additions & error codes

| Key | Location | Notes |
|---|---|---|
| `skills:` | top level | fragment library — `{ <subject>: { verb, lifecycle, body, source? } }` |
| `skills:` | workflow / state / transition | list of subject references |
| `blackboard:` | workflow | slot names, or `{ name: <schema> }` for typed slots |
| `version:` | workflow | version discriminator; ISO date recommended |
| `summary` | `praxec.command` (submit mode) arg | optional model-written string |
| `rotation:` | `audit:` | `daily` (default) / `hourly` / `weekly` |
| `strict_namespacing:` | `praxec:` (top level) | `true` (default) / `false` — controls whether unblessed `subject` roots error or warn (§5.4.2) |
| `delegate:` | workflow state | optional non-empty string — delegate this state to a sub-agent; value is a model reference (affinity-tier or named binding). Pass-through only — see §21 |
| `scripts:` | top level | curated script library — `{ <subject>: { verb, lifecycle, body \| (uri+hash), source? } }`. See §22 |

**Error codes.**

| Code | When |
|---|---|
| `RECORD_WRITE_FAILED` | transition record not durably written — transition aborts |
| `BLACKBOARD_TYPE_ERROR` | typed-slot write violates schema |
| `INVALID_VERB` | `verb` field not in the closed eight (§5.4.1); payload includes `allowed` list |
| `MISSING_VERB` | `verb` field absent from a fragment declaration |
| `INVALID_SUBJECT_ROOT` | first segment of `subject` not blessed; raised under `strict_namespacing: true` |
| `EMPTY_SUBJECT` | `subject` string is empty after trim |
| `MISSING_LIFECYCLE` | `lifecycle` field absent from a fragment declaration (no silent default) |
| `INVALID_LIFECYCLE` | `lifecycle` value not in `experimental`/`stable`/`deprecated` |
| `MISSING_BODY` | `body` field absent from a fragment declaration |
| `MISSING_SKILL_HASH` | a fragment ref reaches the runtime without a `hash` field |
| `HASH_MISMATCH` | stored `hash` does not match `normalize_for_hash(body)` at load |
| `GUIDANCE_DESCRIBE_FAILED` | `praxec.query({subject})` could not resolve a body (snapshot lookup failure) |
| `GUIDANCE_NOT_ACKNOWLEDGED` | `guidance_acknowledged` guard fired; payload names the unacknowledged subject and the current vs acknowledged hash |
| `GUIDANCE_SUBJECT_UNKNOWN` | `guidance_acknowledged` guard names a subject absent from the instance's snapshot |
| `CONFIG_FLAG_NOT_RUNTIME_MUTABLE` | a flag scoped to `praxec:` top level (e.g. `strict_namespacing`, `authoring.write_enabled`) appears within `workflows:` |
| `INVALID_DELEGATE` | a state's `delegate` value is present but not a non-empty string |

Existing codes unchanged. `SUMMARY_REQUIRED` and `MIGRATION_FAILED` are **not**
introduced (§4).

## 14. Compatibility

- `skills:`, `blackboard:`, `version:` are optional; configs without them behave
  identically.
- `goal` / `guidance` strings are now templates — strings with no `{{ }}` are
  unaffected.
- **Behaviour change:** `version` increments once per applied transition
  (including chain hops). Drivers must read `version` from the response — the
  prefilled links already do.

## 15. Open questions

- **Per-run outcome tag** — a success/failure signal per run would make the §5.6
  loop quantitative. Derivable from terminal states in the records today; a
  first-class tag is deferred until there is demand.
- **State-local blackboard slots** — **closed** in §27. Phase 1 (declaration, exit-cleanup, audit event) shipped; full-validator support (`INVALID_SLOT_REDECLARATION`) follows in the next cycle.
- **Structured `summary`** — `summary` is a plain string; a schema'd summary is
  a later option.
- **Request-schema unification** (§10) — separate tech-debt ticket.

## 16. Implementation order

1. Blackboard slot declaration + `output:` name-check + `check` slot checks.
2. Transition records: the `transition-record.schema.json` schema, the typed
   `workflow.transition` audit event, record-first commit ordering with
   `RECORD_WRITE_FAILED` fail-fast.
3. Date-rotated `File` audit sink (`YYYY-MM-DD-{name}.log`), shared by both
   event streams.
4. Versioned definitions: `version:` discriminator, the per-instance definition
   snapshot, additive hot-reload.
5. Guidance: templated inline tier; the `skills:` fragment library; surfaced
   `verb`/`subject` refs; `praxec.query({subject})` describe fetch; `check` lints.
6. use-before-def analysis in `check`.

Each step is independently shippable and rollback-able. A phased, test-first
implementation plan should be produced from this spec before code is written.

## 17. Authoring as a workflow

The LLM is a first-class workflow author. Authoring is **just another
Praxec workflow** — same primitives, same guards, same audit log. No
special runtime path, no privileged escape hatch outside the bypass-flag
mechanism in §8.

### 17.1 Reference workflow shape

```
drafting → reviewing_structure → reviewed → validating → ready → published
                  ↑                              ↑          ↓
                  └──────── (issues found) ──────┘     (gates fail → drafting)
```

| State | Inbound action | Gating |
|---|---|---|
| `drafting` | LLM proposes a workflow YAML or skill fragment | input schema (well-formed YAML) |
| `reviewing_structure` | `structural_analysis` executor (see §18) runs against the draft | guard fails if any required structural issue surfaces |
| `validating` | `dry_run` executor (see §17.3) instantiates an isolated runtime and runs scripted inputs against the draft | guard fails on executor errors or unexpected traces |
| `ready` | author has acknowledged the change; awaits publish | `guidance_acknowledged` (§5.9), optional human-actor |
| `published` | `registry` executor (see §17.4) writes the new definition through the writable store (§8) | requires `praxec.authoring.write_enabled: true` (§8) |

### 17.2 Required executor kinds

Four new executor kinds make authoring expressible as a workflow:

| Kind | Purpose | Mutates state? |
|---|---|---|
| `structural_analysis` | static checks on a candidate definition; returns `{ issues: [{ rule, severity, location, message }] }` | no |
| `dry_run` | instantiates an in-memory runtime and runs a scripted input set against the candidate; returns the audit trace | no (see §17.3) |
| `ingest` | reads an external guidance source (mattpocock-style markdown, etc.) and emits a Praxec-shaped fragment; see §19 | no |
| `registry` | writes a new (or updated) definition through `DefinitionStoreWritable` (§8); fails fast with `WRITE_DISABLED` if the bypass flag is off | yes (gated) |

### 17.3 Isolation invariant for `dry_run`

The `dry_run` executor MUST construct an isolated `WorkflowRuntime` per
invocation, backed by `InMemoryWorkflowStore` and `MemoryAuditSink`. It MUST
NOT accept caller-supplied store or audit references. The signature is
intentionally narrow:

```rust
async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError>
// req.arguments.definition: Value     — the candidate workflow YAML
// req.arguments.script:     [Value]   — scripted inputs to drive
```

The isolation guarantee is enforced by type — there is no parameter through
which the caller can pass production state. The author cannot accidentally
"reuse the live runtime to save time" because the constructor signature
forbids it. See FMECA FM-6 in the implementation plan.

### 17.4 Required guards on the authoring workflow

At minimum the reference authoring workflow uses these guards:

- `structural_analysis_passes`: `expr` guard reading `$.context.structural_issues == []`.
- `dry_run_passes`: `expr` guard reading `$.context.dry_run_failed != true`.
- `guidance_acknowledged`: as defined in §5.9; required before `publish`.
- (Optional) actor-gated transitions: `publish` may be `actor: human` for orgs
  that require human-in-the-loop sign-off.

### 17.5 Meta-circularity & bootstrap

The authoring workflow is itself a Praxec definition. Two consequences:

1. **The first authoring workflow ships with the gateway.** A reference
   `authoring-workflow.yaml` is provided in `examples/`; users can fork it.
2. **Bypass path for recovery.** If the deployed authoring workflow becomes
   uneditable (because it requires itself to publish a fix), §8 defines a
   privileged write path gated by `praxec.authoring.write_enabled` AND a
   principal with `authoring` role. Audit-flagged loudly (`definition.bypass_published`)
   so misuse is impossible to hide.

## 18. Structural analysis

`structural_analysis` is an executor that validates a candidate definition
(workflow or skill fragment) against a closed set of structural rules.
Output shape:

```jsonc
{ "issues": [
    { "rule":     "CYCLE_DETECTED",
      "severity": "error",          // "error" | "warning"
      "location": "/workflows/demo/states/foo/transitions/bar/target",
      "message":  "transition path forms a cycle: foo → bar → foo" } ] }
```

An empty `issues` array means the candidate passed.

### 18.1 Required rule set

Every implementation MUST execute these rules. A rule that fails to execute
returns an error (not an empty issue list), so coverage gaps are visible
rather than silent — see FMECA FM-5.

| Rule | Severity | Detects |
|---|---|---|
| `CYCLE_DETECTED` | error | non-loop-intent cycle in transition graph |
| `DEAD_STATE` | error | state with no inbound transition (and not initial) |
| `UNDEFINED_TARGET` | error | transition `target:` names a state not in `states:` |
| `UNDECLARED_SLOT_READ` | error | guard or template reads `$.context.X` where `X` is not in `blackboard:` |
| `UNBLESSED_SUBJECT_ROOT` | warning | skill fragment subject's first segment not in `BLESSED_SUBJECT_ROOTS` (§5.4.2) |
| `NO_TRANSITIONS` | error | workflow has zero transitions |
| `OVERSIZED_STATE` | warning | state with > N outbound transitions (N defaults to 8) |

### 18.2 Extensibility hook (T3)

The core rule set is fixed; additional rules may be registered via config
under `praxec.structural_rules:`. Custom rules carry the same shape:
`{ rule, severity, location, message }`. Registration shape and lifetime
defined when extensibility ships in T3.

### 18.3 Self-check invariant

Implementations MUST ship a "rules-self-check" test: a fixture workflow
that triggers every required rule. If the analysis output omits any
required rule for that fixture, the test fails. This prevents the
oversimplification failure where an executor ships with two rules and
declares itself done.

## 19. Ingest transforms

`ingest` is an executor that adapts external guidance sources to Praxec
fragment shape. The first-party adapter handles mattpocock-style
`.claude/skills/*.md` (frontmatter `name`, `description`; body is the
markdown body). Future adapters (Cursor rules, internal wikis) follow the
same pattern.

### 19.1 Input

```jsonc
{ "source_path": "path/to/external/skill.md",
  "subject":     "review.style.house-voice",   // optional; if absent, inferred from source path
  "verb_synonyms": { "fix": "implement", ... } // optional caller override }
```

### 19.2 Output

```jsonc
{ "fragment": {
    "subject":   "review.style.house-voice",
    "verb":      "review",            // either explicit in source, or mapped from synonym
    "lifecycle": "experimental",      // ingested fragments default to experimental
    "body":      "…markdown body…",
    "hash":      "sha256:…",
    "source":    "path/to/external/skill.md"
  },
  "diagnostics": [
    { "level": "info", "code": "VERB_MAPPED", "from": "fix", "to": "implement" }
  ] }
```

### 19.3 Verb synonym mapping

A small built-in synonym table maps common external verbs to the closed
eight (§5.4.1). Mappings emit a `VERB_MAPPED` info diagnostic so the
author can audit the rename:

| External verb | Mapped to |
|---|---|
| `fix` | `implement` |
| `verify`, `validate`, `test`, `audit` | `review` |
| `cleanup`, `tidy`, `improve` | `refactor` |
| `document`, `teach`, `walkthrough` | `explain` |
| `assemble`, `bundle`, `integrate` | `compose` |
| `investigate`, `inspect`, `analyze` | `diagnose` |
| `prioritize`, `classify`, `route` | `triage` |
| `design`, `spec`, `plan` | `plan` |

A source-side verb that's already in the closed eight passes through with
no `VERB_MAPPED` diagnostic. A source-side verb absent from both the closed
set and the synonym table fails with `INGEST_INVALID_VERB`.

### 19.4 Error codes

| Code | When |
|---|---|
| `INGEST_CANNOT_INFER_SUBJECT` | no `subject` argument and source path doesn't yield one |
| `INGEST_INVALID_VERB` | source verb is neither in closed eight nor in synonym table |
| `INGEST_SUBJECT_COLLISION` | proposed subject already exists in the live skill library |
| `INGEST_EMPTY_BODY` | source has no body content after frontmatter strip |

Ingest does NOT publish — it returns the fragment to the calling workflow,
which routes it through the rest of the authoring workflow (structural
analysis, dry-run, registry). This keeps the gates uniform regardless of
authoring source.

## 20. Audit & Evidence Enrichment for Downstream Analysis

Three additive fields enable hierarchical observability and richer
evidence-weighted decisions without breaking existing producers. Every
field is `Option<_>` with serde `skip_serializing_if = "Option::is_none"`,
so historical payloads round-trip unchanged.

### 20.1 Evidence enrichment

The existing `Evidence` struct (`crates/praxec-core/src/model.rs`)
gains two optional fields:

| Field | Type | Meaning |
|---|---|---|
| `digest` | `Option<String>` | Content-identity of the evidence artifact. Convention: `sha256:` prefix + lowercase-hex digest of the artifact's bytes. Useful for verifier-produced artifacts (JUnit XML, SARIF, coverage JSON) where the consumer wants to deduplicate or attest. Producers SHOULD populate when the artifact is byte-stable. |
| `confidence` | `Option<f32>` | The producing model's stated confidence (0.0..=1.0) that this evidence supports the claim it's attached to. Out-of-range values fail validation with `INVALID_CONFIDENCE`. Producers SHOULD populate when the evidence is model-authored; deterministic executors typically omit. |

The `evidence` guard kind (§9) is extended with two optional clauses
that compose with the existing `requires: [{kind, count}]` shape:

```yaml
guards:
  - kind: evidence
    requires:
      - { kind: approval, count: 2, min_confidence: 0.7 }   # NEW: min_confidence
      - { kind: build-log, count: 1, require_digest: true } # NEW: require_digest
```

`min_confidence` rejects any evidence record whose `confidence` is below
the threshold (records with no `confidence` are also rejected when this
clause is set — explicit opt-in to model-authored evidence). `require_digest`
rejects evidence records missing a `digest`.

### 20.2 AuditEvent enrichment

The existing `AuditEvent` struct (`crates/praxec-core/src/audit.rs`)
gains two optional hierarchical-identity fields:

| Field | Type | Meaning |
|---|---|---|
| `trace_id` | `Option<String>` | Caller-supplied trace id spanning multiple workflows in one logical operation (e.g. a CI build that launches N sub-workflows). The gateway is opaque to the value; it writes through unchanged. |
| `run_id` | `Option<String>` | Caller-supplied id for grouping related workflow instances (e.g. one model-evaluation run that exercises 100 workflows). Same opacity semantics as `trace_id`. |

Both are surfaced via builder methods on `AuditEvent` (`with_trace_id`,
`with_run_id`) mirroring the existing `with_workflow`/`with_correlation`
pattern. Sinks that serialize to JSON include the fields when present and
omit them otherwise.

**MCP server plumbing.** The MCP-server-level tools (`praxec.query` and
`praxec.command`) accept optional `traceId` / `runId` arguments. When
present, the server propagates them to every `AuditEvent` produced by the
resulting workflow operation. When absent, the fields stay `None`. The
plumbing is mechanical and does not change existing semantics for callers
that omit the fields.

### 20.3 Metric extraction contract

The audit log carries everything the standard SWE-agent scorecard
(`docs/architecture/research.md`) needs. No new metrics service ships with Praxec.
Instead this section specifies the contract:

**Producers guarantee** that every transition record carries:
- `event_type` = `"workflow.transition"` (per §7.2),
- `workflow_id`, `correlation_id`, `actor`, `transition_name`,
- `executor_outcome.duration_ms` and `executor_outcome.ok` when an
  executor ran (per the §7.2 ordering),
- `timestamp` (ISO-8601, UTC).

**Consumers** derive metrics like the following from the log alone:

| Metric | Derivation |
|---|---|
| `resolved_rate` | count(workflows reaching a `terminal: true` state with no `error`) ÷ count(workflows started) |
| `time_to_reviewer_ready_patch` | `timestamp(first audit event in workflow with `state == "ready"`)` − `timestamp(workflow.started)` |
| `retry_count` | count(`transition.requested` with name `retry` per `workflow_id`) |
| `cost_per_accepted_fix` | Σ(`executor_outcome.duration_ms` × tier-cost) ÷ count(workflows completed). Tier-cost is a caller-side lookup; Praxec does not assign monetary value to executor kinds. |
| `mutation_score` | Extract from `evidence[kind="mutation"]` records on verifying-state transitions. |
| `human_escalation_rate` | count(`transition.requested` whose target is a state with `actor: human`) ÷ count(all transitions). |
| `pass_to_pass_failure_rate` | Read `evidence[kind="pass-to-pass-failed"]` records; report fraction of verifier runs producing one. |

No gateway code change is needed for any of these. The contract is
sufficient because the audit log is already SPEC §7.4 date-rotated and
already passes through the existing `AuditSink` trait — any downstream
consumer (jq pipeline, Vector route, Prometheus exporter) can tail it.

### 20.4 Error codes added by §20

| Code | When |
|---|---|
| `INVALID_CONFIDENCE` | An `Evidence.confidence` value is outside `0.0..=1.0`. |
| `EVIDENCE_DIGEST_REQUIRED` | An `evidence` guard with `require_digest: true` saw a record missing a `digest`. |
| `EVIDENCE_CONFIDENCE_BELOW_THRESHOLD` | An `evidence` guard with `min_confidence: N` saw a record with no confidence or confidence < N. |

All three are surfaced as transition rejection codes (mirroring
`GUARD_REJECTED`) when the rejecting guard is the `evidence` kind with
the new clauses.

## 21. Sub-agent delegation (pass-through field)

A workflow state MAY declare a `delegate: <string>` field. The gateway
treats it as **pass-through only**:

- It is read at response-build time and surfaced verbatim at the top
  level of every `praxec.query({workflowId})` / `praxec.command({definitionId})`
  / `praxec.command({workflowId, transition})` response for that state.
- It is **not** validated against any agent registry, **not** acted on
  by the gateway, **not** required to be present for the workflow to
  function. A state with no `delegate` is identical to today.
- Empty strings and non-string values are rejected at config load with
  `INVALID_DELEGATE` (§13).

The gateway itself does **not** consume `delegate`. It is designed for an
**agentic harness** that walks the workflow and decides, per state, whether
to spawn an isolated **sub-agent session** instead of driving the workflow
inline. The reference consumer is the in-repo agentic runtime (the
`praxec` TUI, in the `praxec-tui` crate); a
typical harness gives each sub-agent the state's `goal` and `guidance` as
system-prompt material, the blackboard at spawn time, and the same two
Praxec MCP tools (`praxec.query` + `praxec.command`) — no extra tools,
no out-of-band access. The sub-agent runs until it calls `praxec.command`
with a transition (advancing the workflow) or hits its timeout / step limit.

**The field vs. the value.** `delegate:` names *one* concept — "delegate
this state to a sub-agent (a worker)." Its value names *which model* that
worker runs on, in either of two forms the consumer resolves: an
**affinity-tier model reference** (`coding`, `frontier`, `coding-frontier`),
parsed into a `ModelRef` and resolved against `models.yaml` by the model
resolver; or a **named binding**, resolved through the agent registry. The
worker is the *agent*; the `ModelRef` is the *model* — keeping the two named
distinctly is why the parsed affinity-tier type is `ModelRef`, not "delegate".

The pass-through design is deliberate: it lets the gateway stay
model-agnostic and harness-agnostic. The `praxec` TUI is one consumer;
other harnesses (IDE integrations, batch runners) MAY consume `delegate`
with different semantics. The gateway prescribes the field's shape and
surfacing, not the policy.

**Rationale.** Putting `delegate` on the workflow state (where the
guidance lives) rather than on the agent invocation (where the
provider/model live) means workflow authors declare *where the work
is done* and operators declare *who does it* — clean separation of
concerns. A workflow shipped in the `cognitive-architectures`
library names `planning-agent` / `editing-agent` / etc., and any
operator can plug in any combination of provider/model behind those
names without editing the workflow.

## 22. Curated scripts (deterministic peer to skills)

A workflow MAY declare a top-level `scripts:` block. Each entry is a
curated, hash-pinned script body invokable by a workflow's `script`
executor. The peer relationship to skills is intentional:

> *Skills tell the LLM what to think; scripts tell the workflow what
> to do.* Both are vocabulary the harness composes; both are content-
> identified by hash; both surface through the same discovery, describe,
> and acknowledgment patterns.

### 22.1 Shape

```yaml
scripts:
  build.cargo.release:
    verb: build                          # closed enum, see §22.3
    lifecycle: stable                    # experimental | stable | deprecated
    source: cognitive-architectures      # optional provenance
    body: |                              # inline literal OR uri+hash
      cargo build --release --locked
```

Or with an external source:

```yaml
scripts:
  build.cargo.release:
    verb: build
    lifecycle: stable
    source: cognitive-architectures
    uri: file://./scripts/build/cargo.sh # v1: file:// only
    hash: sha256:<64 lowercase hex chars> # REQUIRED with uri
```

### 22.2 Body source: inline vs uri+hash

**Exactly one** of `body` (inline literal) OR `uri + hash` (external
reference) is required.

- **Inline `body:`** — the literal script content. Hash is computed at
  load time. If the author also provides `hash:`, mismatch is
  `SCRIPT_HASH_MISMATCH`. v1 supports only `body:` and `uri: file://`.
- **External `uri:`** — `hash:` is **required**; the runtime fetches
  the body, computes its hash, and rejects on mismatch
  (`SCRIPT_HASH_MISMATCH`). Three schemes:
  - `file://<path>` — resolved relative to the config file at load time.
  - `https://<url>` — load-time HTTP GET (blocking, 30 s timeout,
    anonymous). The declared `hash:` is what makes this safe — fetched
    bytes are verified before they enter the snapshot.
  - `git+https://<host>/<repo>(.git)?@<ref>#<path>` — load-time
    `git archive --remote=<https-url> <ref> <path>` extraction.
    `<ref>` MUST be specified (no implicit `HEAD` / `main`) so the
    snapshot is reproducible. Many forges (GitHub, GitLab.com) disable
    `upload-archive` for security; when that happens the load fails
    with `GIT_ARCHIVE_NOT_SUPPORTED` and the operator's workaround is
    to host via plain `https://` (raw.githubusercontent.com URLs etc.)
    or run a self-hosted mirror that permits `upload-archive`.

**Normalization** is stricter than the skill hash. Shell scripts treat
whitespace as load-bearing (`if [[ x ]]` ≠ `if [[  x  ]]`), so the
normalization rule is:

1. Preserve all internal whitespace exactly.
2. Collapse trailing newlines to exactly one terminal newline.
3. No leading-whitespace trim.

This means an inline edit changing tabs to spaces WILL invalidate a
uri-sourced hash — by design. The skill hash collapses whitespace; the
script hash does not.

### 22.3 Closed verb enum

Scripts use a distinct verb set from skills (cognitive verbs vs action
verbs). Twelve closed values:

| Verb | When | Use this when… (vs neighbor) |
|------|------|---|
| `build` | Compile, package, generate artifacts | producing a deliverable artifact |
| `test` | Exercise the system against assertions | the script's contract is pass/fail on assertions (not "report findings" — that's `audit`) |
| `deploy` | Promote artifacts to an environment | moving things between environments |
| `format` | Apply style transformations (writes) | the script WRITES style fixes (not "report style issues" — that's `lint`) |
| `lint` | Inspect for static issues (reads, binary verdict) | binary pass/fail on issue presence (not "graded findings" — that's `audit`; not "data gathering" — that's `inspect`) |
| `install` | Provision dependencies / toolchains | side-effectful environment setup |
| `verify` | Confirm an externally-asserted property | proving a specific claim (not "find issues" — that's `lint`/`audit`) |
| `run` | Catch-all for runnable operations | nothing more specific fits (use sparingly) |
| `inspect` *(v0.3)* | Read-only local introspection | gathering system state / dep tree / symbols (not "find content matching pattern" — that's `search`; not "binary verdict" — that's `lint`) |
| `search` *(v0.3)* | Content discovery — codebase grep, web search, doc search | finding unknown matches (not "retrieve known resource" — that's `fetch`; not "system state" — that's `inspect`) |
| `fetch` *(v0.3)* | Retrieve known resource by URL / path | the target is identified by id (not "discover candidates" — that's `search`) |
| `audit` *(v0.3)* | Graded compliance / security / quality scan emitting structured findings | emitting a report with severity grades (not "binary pass/fail" — that's `lint`) |

Adding a verb is a spec amendment (see §23.7 for the criterion). Unknown
verbs reject with `INVALID_SCRIPT_VERB`.

### 22.4 Blessed subject roots

Scripts subjects follow the same dotted pattern as skills, but the
blessed-root set is action-flavored. **Verb-mirror roots** (one per
ScriptVerb) plus **domain-themed roots** for operational categories:

```
verb-mirror:    build, test, deploy, format, lint, install, verify, run,
                inspect, search, fetch, audit
domain-themed:  release, migrate, ci
```

Total: 15 blessed script roots.

`strict_namespacing: true` (default) rejects unblessed roots with
`INVALID_SCRIPT_SUBJECT_ROOT`; lenient mode warns with the closest-
blessed-root suggestion.

### 22.5 Snapshot stamping

The SPEC §8.2 invariant applies: every workflow that references a
script gets a stamped `_scriptsLibrary` on its definition snapshot at
config-load time. The library contains the materialized body, hash,
verb, lifecycle, and source. In-flight instances are immune to edits of
either the top-level `scripts:` block or the external file behind a
`uri:`.

### 22.6 The `script` executor

```yaml
transitions:
  build:
    executor:
      kind: script
      subject: build.cargo.release        # required
      args: ["{{$.context.features}}"]    # optional, templated
      workingDirectory: /path/to/repo     # optional
      env: { CI: "true" }                 # optional
      treatNonZeroAsFailure: true         # optional (default true)
```

Execute flow: look up subject in instance's `_scriptsLibrary` →
materialize body to temp file (`chmod 0700`) → exec via shebang if
present, else `bash <path>` → capture stdout/stderr/exit → emit
`script_output` Evidence with body hash for audit replay. Temp file is
deleted on Drop.

Subject not in snapshot → `SCRIPT_NOT_IN_SNAPSHOT` (poka-yoke: workflow
references must be collectible by `collect_referenced_script_subjects`).
Missing subject field → `INVALID_SCRIPT_INVOCATION`.

The executor exposes two env vars to the script body so it can
self-identify in logs/metrics without parsing argv:

- `PRAXEC_SCRIPT_SUBJECT` — the dotted subject name
- `PRAXEC_SCRIPT_HASH` — the `sha256:...` body hash

### 22.7 `gateway.scripts.search` (authoring-time tool)

Mirror of `gateway.skills.search`. Returns refs (`{verb, subject,
source}`) filterable by `verb` / `subject_root` / `source`. Never
emits bodies — bodies are fetched on demand via
`praxec.query({subject})` (progressive disclosure; §32).

Advertised only when `PraxecServer::with_scripts_search(true)` is
set. Default off; authoring-time only.

### 22.8 `script_acknowledged` guard

```yaml
guards:
  - { kind: script_acknowledged, subject: deploy.production.rollout }
```

Passes iff `praxec.query({subject})` (§32) was called for `subject`
against this workflow AND the recorded body hash matches the current
snapshot's hash. Hash flip invalidates the prior ack — editing the
script body forces re-review. Use case: review-before-execute gates on
destructive scripts.

The guard requires a `ScriptAcknowledgmentStore` wired via
`PraxecServer::with_script_ack_store(...)`. Without one, the guard
cannot pass (returns false rather than silently succeeding).

Script and guidance acknowledgments use **distinct keyspaces** —
recording a script ack does not satisfy a guidance-guard for the same
subject, and vice versa.

### 22.9 Error codes added by §22

| Code | When |
|---|---|
| `INVALID_SCRIPT_VERB` | `verb` field not in the closed eight |
| `MISSING_SCRIPT_VERB` | `verb` field absent |
| `INVALID_SCRIPT_SUBJECT_ROOT` | First segment not blessed; raised under `strict_namespacing: true` |
| `EMPTY_SCRIPT_SUBJECT` | `subject` empty after trim |
| `MISSING_SCRIPT_LIFECYCLE` | `lifecycle` absent |
| `INVALID_SCRIPT_LIFECYCLE` | `lifecycle` not in experimental/stable/deprecated |
| `SCRIPT_BODY_SOURCE_AMBIGUOUS` | Both `body` and `uri` present, or neither |
| `MISSING_SCRIPT_HASH` | `uri` present without `hash` |
| `UNSUPPORTED_SCRIPT_URI_SCHEME` | `uri` scheme not in {`file://`} for v1 |
| `INVALID_SCRIPT_HASH_FORMAT` | Hash not matching `^sha256:[0-9a-f]{64}$` |
| `SCRIPT_HASH_MISMATCH` | Declared hash ≠ computed hash for resolved body |
| `SCRIPT_NOT_IN_SNAPSHOT` | Executor invoked for subject not in workflow's `_scriptsLibrary` |
| `INVALID_SCRIPT_INVOCATION` | Executor config missing `subject` field |
| `SCRIPT_SUBJECT_UNKNOWN` | `script_acknowledged` guard references a subject not in `_scriptsLibrary` |

## 23. Intent layer invariant

This section locks the architectural rule that distinguishes which surfaces
get a closed verb taxonomy and which surfaces don't. Adding it as a numbered
invariant prevents drift toward "verb every typed thing for symmetry" — a
common reflex that conflates transport with intent.

### 23.1 Two-layer split

The gateway has two distinct typing layers:

| Layer | Surfaces | Typed by | Why |
|---|---|---|---|
| **Access** — "what can be reached, how" | `connections`, `capabilities`, `executors` | `kind` (mcp / cli / rest / script / ...) | Transport / protocol is structural. A `cli` connection is a `cli` connection regardless of which subcommand is invoked. The kind tells you the rules for invoking; the intent is decided elsewhere. |
| **Intent** — "what kind of work" | `skills`, `scripts` | closed verb enum | Cognition and action have natural semantic categories. Verbing makes search, discovery, and governance precise. Closed enum forces authors to pick instead of inventing synonyms. |

### 23.2 Why only skills and scripts get verbs

Verbs live at the intent layer because that's where the *what* (semantically)
matters. A `cargo` CLI connection would be doubly-classified if verbed — is
it a `build` connection? a `test` connection? Both. Neither in particular.
Putting the verb on the access layer forces the choice prematurely; the
verb belongs at each individual USAGE (script invocation), not at the
declaration (connection).

### 23.3 Indeterminate + determinate intent

The two intent surfaces partition cleanly along determinism:

| Surface | Determinism | Means |
|---|---|---|
| **Skills** | Indeterminate | LLM-driven. The skill body is guidance; the model interprets it. Same skill, different sessions, different outputs. |
| **Scripts** | Determinate | Deterministic. The script body is executable bytes; identical inputs produce identical outputs (modulo time / IO). Hash-pinned for replay-by-hash. |

Together skills and scripts form the **complete intent vocabulary**:
indeterminate cognition (skills) + determinate action (scripts) = everything
the workflow can express intentionally. No third intent surface is needed,
and no fourth.

### 23.4 Workflows are a meta-layer

Workflows sequence skills + scripts + access into a state machine. They are
NOT verbed because they are implicitly always `compose` — they assemble the
other vocabulary. Verbing workflows would be self-referential. A workflow's
`states` and `transitions` are the structural shape; what each state DOES
is expressed through skills, scripts, and executor invocations.

### 23.5 Audit events are event-shaped, not verb-shaped

`workflow.started`, `transition.requested`, `guard.evaluated` are a closed
taxonomy, but they describe *what happened*, not *what was intended*. Event
descriptions are noun-shaped (subject + state-change); intent descriptions
are verb-shaped (action category). Different kinds of closed enum, different
purposes.

### 23.6 The numbered invariant

> **Verb taxonomy lives only on skills and scripts.** The access layer
> (`connections`, `capabilities`, `executors`) is kind-typed. Workflows
> compose. Audit events describe. No surface gets two of these
> classifications at once.

A pull request that proposes adding `verb:` to a connection, capability,
executor, workflow, or audit event MUST be rejected on §23 grounds, even if
the proposed verb is plausible. The right answer is always to push the
classification down into the script or skill that USES the access-layer
surface, or up into the workflow shape that composes it.

### 23.7 Closed-enum amendment criterion

The skill verb enum (§5.4.1) and script verb enum (§22.3) are closed. Adding
a verb is a spec amendment. Required PR contents for any future verb
addition:

1. **Documented gap.** Existing verbs forced an awkward classification on
   ≥1 concrete subject in the cognitive-architectures library OR a downstream
   adopter. Cite the subject and explain why each existing verb was the
   wrong fit.
2. **Distinct semantic category.** The proposed verb is not a synonym of
   any existing verb. Provide a one-row disambiguation against the closest
   neighbor ("use X when …, not Y when …") matching the format of the §5.4.1
   / §22.3 tables.
3. **≥2 example subjects.** Concrete subject names that would use the
   proposed verb naturally. If only one example surfaces, the verb is
   premature.

The criterion is intentionally a sieve, not a checklist — meeting it doesn't
guarantee acceptance, missing it does guarantee rejection. The goal is to
keep "I want my own verb" debates out of PR review.

### 23.8 Authoring preferences (advisory)

LLM-driven authoring workflows that generate new skills, scripts, or
workflows benefit from a steering signal — "operators here prefer
Python, not bash" — without that signal being a hard constraint
(authoring tasks that specifically require a different runtime should
still be writable).

The gateway carries this signal at the top level:

```yaml
praxec:
  authoring:
    preferred_script_language: bash   # or "python3" / "powershell" / "node"
```

The field is **advisory only**. No validator rejects a script for not
matching it; no runtime guard branches on it. The single mechanism is
**template substitution** (§5.2): authoring skills include the
preference in their bodies via `{{$.praxec.authoring.preferred_script_language}}`,
and the substituted value is what the LLM sees in its system prompt.

Snapshot stamping (SPEC §8.2): `praxec.authoring` is copied onto
each workflow's snapshot as `_authoringPrefs` at config-resolve time.
In-flight instances see the preferences that existed at
`praxec.command({definitionId})` start time — editing the gateway
config doesn't mutate what an already-running authoring workflow sees.

Shape validation: `preferred_script_language` must be a non-empty
string if present (`INVALID_AUTHORING_PREFERENCE` at config load).
The value itself is free-form — `bash`, `sh`, `python3`,
`powershell`, `node`, `deno`, anything makes sense to the operator.
Closed-enum discipline doesn't apply here because the preference is
about what code a downstream tool will EMIT, not about what the
gateway will validate.

## 24. Parallel execution (fan-out / fan-in)

The workflow runtime is sequential at the state-machine level — one
active state, one transition at a time. The `parallel` executor kind
adds fan-out **inside a single executor invocation**: N branches run
concurrently, results aggregate, and the runtime sees one
slow-but-normal executor call. CPM ("critical path method") "crashing"
is the mental model: parallelizing independent activities compresses
wall-clock without changing CPU work or any state-machine invariant.

### 24.1 Why parallel

Five use cases that motivate the primitive:

- **Parallel research.** N SCIP queries against the codebase, results
  aggregated into one evidence pack.
- **Parallel validation.** A patch tested against N scenarios
  concurrently; aggregate verdict drives the next transition.
- **Parallel PR review** (Greptile-style, local pre-PR). Multiple
  reviewers / critics / checks fan-out against a diff.
- **Parallel simulation / pressure-testing** at multiple abstraction
  levels (concepts, plans, specs, copy, docs). New agentic-interaction
  paradigm.
- **Parallelized FMECA** to explore potential real failure modes of
  code execution — meta-applies the discipline to itself.

### 24.2 The executor

```yaml
executor:
  kind: parallel
  branches:                            # static list OR {for_each, do}
    - { kind: script, subject: ... }
    - { kind: mcp,    connection: ..., tool: ... }
    - { kind: workflow, definitionId: critique-agent, input: { ... } }
  join: all                            # all (default) | any | { at_least: K } |
                                       # { percent: P } | { expression: "<expr>" }
  max_concurrency: 4                   # REQUIRED when branches.len() >= 10
  on_branch_failure: bail              # bail (default) | continue
  total_timeout_ms: 60000              # optional
  max_recursion_depth: 3               # optional, default 3
```

Dynamic-branches form:

```yaml
branches:
  for_each: "$.context.queries"        # path resolving to a JSON array
  do:                                  # per-branch executor template
    kind: mcp
    connection: scip
    tool: lookup
    args: { symbol: "$.branch.value" }
```

`$.branch.value` and `$.branch.index` are template substitution
markers replaced per branch when the `do:` template is expanded.

**Join conditions:**

- `all` (default) — every branch must succeed. Any failure fails the whole.
- `any` — first success wins; siblings cancelled.
- `{at_least: K}` — succeeds iff K or more branches succeed; failures
  making K unreachable fail early.
- `{percent: P}` — succeeds iff `ok_count >= ceil(P * n / 100)`.
  Ceiling division avoids silent rounding (e.g. `51% of 3` requires 2
  successes, not 1). `percent: 0` is the explicit "never fail by
  quorum" escape hatch; vacuous fan-out (n=0) succeeds.
- `{expression: "<expr>"}` — operator-supplied predicate evaluated
  **post-completion** against the aggregated value (paths `$.branches[]`,
  `$.ok_count`, `$.failed_count`, `$.cancelled_count`, `$.n`). Same
  binary-comparison surface as `expr` guards plus bare-path
  truthiness. **No early exit** — every branch runs before the
  expression evaluates. This guarantees the expression cannot observe
  a still-running branch (structural answer to the prior amendment
  criterion concern).

**Failure modes:**

- `on_branch_failure: bail` (default) — first failure cancels in-flight
  siblings, executor returns error immediately.
- `on_branch_failure: continue` — all branches run regardless of failures;
  verdict computed per join condition over the aggregate.

**Branches accept any executor kind**, including nested `parallel`. The
schema is recursive — `branches[*]` references the same `executor` $def.

### 24.3 Output shape

```json
{
  "branches": [
    { "ok": true,  "index": 0, "output": { ... } },
    { "ok": false, "index": 1, "error": { "code": "timeout", "message": "..." } },
    { "ok": true,  "index": 2, "output": { ... } }
  ],
  "summary": {
    "n":                      3,
    "ok_count":               2,
    "failed_count":           1,
    "cancelled_count":        0,
    "durationMs":             4321,
    "first_failure_index":    1,
    "max_in_flight_observed": 2,
    "join":                   "all",
    "verdict":                "failed"
  }
}
```

Output mappings can pluck per-branch fields via the **`[*]` array-
projection** extension to path expressions: `$.output.branches[*].ok`
returns `[true, false, true]`. See §6 (output mappings) for the syntax.

### 24.4 Audit event taxonomy

| Event | When | Payload |
|---|---|---|
| `parallel.branch.started` | Each branch begins | `transition`, `branch_index`, `branch_executor_kind` |
| `parallel.branch.completed` | Branch returns Ok | `transition`, `branch_index`, `durationMs` |
| `parallel.branch.failed` | Branch returns Err | `transition`, `branch_index`, `durationMs`, `error` |
| `parallel.branch.cancelled` | Branch cancelled (any-join success or bail-failure) | `transition`, `branch_index`, elapsed |
| `parallel.fanout.completed` | Aggregate done, before parent transition record | `transition`, `summary` |
| `parallel.fanout.empty` | Dynamic `for_each` resolved to `[]` | `transition`, `for_each` |

All per-branch events carry the **parent transition's correlation_id**
plus the `branch_index` payload field. Combined with the parent
transition's `seq`, the pair `(seq, branch_index)` groups all events
for one branch's invocation; intra-branch ordering (the `started` →
`completed`/`failed`/`cancelled` sequence + any `executor.started`/
`executor.succeeded` from the reliability stack) is timestamp-monotonic.
Replay tools sort by `(seq, branch_index, timestamp)`.

### 24.5 Snapshot + version invariants

> **Load-bearing rule:** fan-out happens inside one executor invocation.
> Branches NEVER touch the WorkflowStore. The parent `ParallelExecutor`
> returns one `ExecuteResult`; the runtime does exactly one
> `save_if_version` post-aggregation. The transition record is written
> once, the workflow version bumps once.

This preserves every existing invariant — optimistic locking still
works, deterministic chaining still works, audit ordering still works.
Multi-active-state workflow execution is explicitly OUT of scope; the
constraint keeps the system coherent.

**Defensive assert:** the parallel executor hashes the snapshot bytes
at fan-out start and re-hashes at aggregation. Mismatch raises
`PARALLEL_SNAPSHOT_MUTATED` — a runtime invariant violation that should
be impossible in safe Rust (the snapshot is in an `Arc`) but is checked
anyway as a future-regression safety net.

### 24.6 Compensating transactions (operator-responsibility)

`parallel` does NOT provide distributed-saga compensation in v1. If
your branches have side effects:

- Prefer **idempotent branches** (replay-safe — same input always produces
  same downstream effect).
- Use `on_branch_failure: bail` for STRICT semantics (no partial
  commits past the first failure).
- For `on_branch_failure: continue`, design follow-up cleanup workflows
  that consume `summary.failed_branches` (the aggregate output lists
  every branch's outcome — operators compose cleanup based on the
  audit log).

Distributed-saga support (undo handlers, compensating actions) is v2
work. v1 explicitly does not silently hide partial-commit cases —
every branch's outcome is in the audit log.

### 24.7 MCP transport bottleneck note

`parallel` of `kind: mcp` branches against the same MCP **connection**
is bounded by that connection's concurrency. Typical MCP servers are
single-connection / serialized. Operators wanting true MCP parallelism
should either:

- Configure N separate connections to the SAME MCP server, OR
- Use an MCP server that supports concurrent in-flight requests on one
  connection.

Per-branch `durationMs` in the audit log reveals serialization — if
every branch's start time staggers by roughly its predecessor's
duration, transport is serialising. No new metric needed; the existing
audit fields surface the symptom.

### 24.7.1 Sub-agent stdout interleaving on `parallel` + `delegate`

`parallel` branches resolving to `kind: workflow` against a workflow
whose states declare `delegate:` will run sub-agents concurrently —
**and their stdout streams may interleave on the parent's stdout**,
depending on how the agentic runtime's sub-agent spawner writes output
(a spawner that writes directly to the parent's stdout with no
line-prefixing will interleave).

Operators wanting deterministic per-branch attribution should:

- **Use the audit log, not stdout, for branch identification.** Every
  branch event carries `branch_index` + parent `correlation_id` +
  per-branch `correlation_id`; the audit log is the canonical source.
- **For human-readable per-branch transcripts**, configure the audit
  sink to file-rotate per-correlation-id (see §20.6), then read the
  branch's file post-walk.
- **Or run delegate sub-agents sequentially** — keep `delegate:` out
  of `parallel` branches; use `parallel` only with `kind: script` /
  `kind: cli` / `kind: mcp` branches that don't stream LLM tokens to
  stdout.

Per-branch stdout multiplexing in the agentic runtime itself is out of
scope for this gateway — it would require either a stdout multiplexer
wrapping the runtime's output sink or capturing each sub-agent's output
into a buffer per branch (significant memory cost for long runs).
Audit-log-based attribution is the gateway-level answer.

### 24.8 Recursion-depth cap + amendment criterion

`max_recursion_depth` (default 3) caps parallel-of-parallels nesting.
Exceeding it raises `PARALLEL_DEPTH_EXCEEDED`. The default is
**speculative** — three levels is the deepest a sane architecture
should need; operators with deeper-nesting use cases override
explicitly. The cap exists to catch authoring bugs that produce
exponential fan-out by accident.

**Shipped since this section was drafted** (v0.4 cycle):
- `{percent: P}` join — quorum expressed as percentage. Threshold
  uses ceiling division (`ceil(P * n / 100)`) so `51% of 3` rounds to
  `2 required`, not `1`. `percent: 0` is the explicit "never fail by
  quorum" escape hatch. Vacuous fan-out (n=0) succeeds.
- `{expression: "<expr>"}` join — operator-supplied predicate
  evaluated **post-completion** against `{branches[], ok_count,
  failed_count, cancelled_count, n}`. Same binary-comparison surface
  as `expr` guards (`==`, `!=`, `<`, `<=`, `>`, `>=`, `starts_with`,
  `contains`) plus bare-path truthiness check. **NO early exit** —
  expression cannot observe a still-running branch by construction.
  This is the structural answer to the "guard reads still-running
  branch's output" concern that previously deferred this feature.

**Amendment criterion** (mirrors §23.7) for future v0.5+ additions:
- Compensating transactions — distributed-saga shape proposal
- Multi-active-state workflow execution — strong justification
  required; preserves the §24.5 invariant or explicitly amends it.
- Streaming / mid-flight join expressions — would require branch
  cancellation semantics for partially-evaluated predicates;
  deliberately not done in v0.4 (see §24.2 expression timing note).

### 24.9 Error codes added by §24

| Code | When |
|---|---|
| `INVALID_PARALLEL_CONFIG` | Malformed `parallel` executor config (missing `branches`, bad `join`, unbounded fan-out without `max_concurrency`, etc.) |
| `JOIN_THRESHOLD_NOT_MET` | `join: at_least: K` and fewer than K branches succeeded |
| `PARALLEL_DEPTH_EXCEEDED` | Nested `parallel` exceeded `max_recursion_depth` |
| `PARALLEL_SNAPSHOT_MUTATED` | Defensive: snapshot bytes diverged during fan-out (runtime invariant violation) |
| `PARALLEL_EXECUTOR_NOT_WIRED` | Registry wasn't set on `ParallelExecutor` after construction (deployment bug) |

`[*]` array-projection mapping errors fall under the existing mapping
contract (`None` for unresolvable paths); no new error code needed in
v1.

## 25. Pipeline executor (sequential composition)

### 25.1 Why pipeline

Workflows already express sequential steps via N states with
auto-advance. That's correct but expensive: N transitions, N version
bumps, N transition records, N storage round-trips. When the steps
are tightly coupled ("compile, then test, then publish — atomic
unit"), authors want one transition that runs the whole chain.

`kind: pipeline` is the FP-compose primitive: N executors run in
order, each step's `output` threads as the next step's `$.input`.
Inside one transition. One version bump. One transition record.

Mirrors `kind: parallel` (§24): both encapsulate sub-execution
inside a single outer transition; both preserve all workflow
invariants; both reuse the executor registry via back-reference.

### 25.2 Config

```yaml
executor:
  kind: pipeline
  steps:                                # array of executor configs
    - { kind: script, subject: build.cargo.release }
    - { kind: cli,    connection: shell, command: "verify" }
    - { kind: mcp,    connection: notifier, tool: report }
  on_step_failure: bail                 # bail (default) | continue
  total_timeout_ms: 60000               # optional
```

Each `steps[]` entry is any valid executor config — including nested
`pipeline` or `parallel`. The schema is recursive.

### 25.3 Output shape

```json
{
  "steps": [
    { "ok": true,  "index": 0, "output": { ... } },
    { "ok": true,  "index": 1, "output": { ... } },
    { "ok": false, "index": 2, "error": { "code": "...", "message": "..." } }
  ],
  "final_output": { ... last successful step's output ... },
  "summary": {
    "n":                   3,
    "ok_count":            2,
    "failed_count":        1,
    "durationMs":          420,
    "first_failure_index": 2,
    "verdict":             "succeeded" | "failed"
  }
}
```

`final_output` is the last successful step's `output`, OR `null` if
the first step failed. Workflows that want only the terminal value
can `output: $.output.final_output` without walking `steps[]`.

### 25.4 Audit events

- `pipeline.step.started`  — `{step_index, step_kind}`
- `pipeline.step.completed` — `{step_index}`
- `pipeline.step.failed`   — `{step_index, error_code}`
- `pipeline.completed`     — `{summary}` (final rollup)

All events share the parent transition's `correlation_id`. Steps run
sequentially so the audit ordering is naturally `(seq, step_index)` —
no per-event sub-counter needed (contrast with §24.4 parallel events
where concurrency requires a three-tuple).

### 25.5 Failure modes

- `on_step_failure: bail` (default) — first failure stops the
  pipeline; verdict is failed; subsequent steps are not started.
- `on_step_failure: continue` — every step runs regardless of
  upstream failures. Failed steps' `output` is **not** threaded
  forward; the next step gets the most-recent SUCCESSFUL output as
  its `$.input` (failures don't erase context built so far).

### 25.6 Error codes

| Code | When |
|---|---|
| `INVALID_PIPELINE_CONFIG` | Malformed config (missing `steps`, empty `steps`, bad `on_step_failure`) |
| `PIPELINE_EXECUTOR_NOT_WIRED` | Registry wasn't set on `PipelineExecutor` post-construction |

Sub-step failures surface their own executor error codes within
`steps[].error.code` — pipeline does not remap them.

## 26. State-level `while:` loop

### 26.1 Why a while loop on a state

Polling, retry-until-success, and "wait for converge" patterns
currently require an N-state ping-pong or critic cycle. A direct
loop primitive — "stay in this state until the guard goes false" —
collapses that pattern to one state with a guard.

### 26.2 Config

```yaml
states:
  polling:
    while: { kind: expr, expr: "$.context.poll_result == 'in_progress'" }
    max_iterations: 30        # REQUIRED with while: — no default
    transitions:
      check:
        target: done           # declared target IS USED when while goes false
        executor:
          kind: rest
          # ... polls upstream, writes result to context.poll_result
```

### 26.3 Semantics

After ANY transition fires from a state declaring `while:`:

1. The runtime merges the executor's output into context (as today).
2. The runtime resolves the transition's `target` (as today, including
   `branches: [{when, target}]` resolution).
3. **NEW:** the runtime evaluates the FROM-state's `while:` guard
   against the post-output context.
4. If `while:` is **truthy**, the runtime overrides the target to the
   FROM state — workflow re-enters the same state. Iteration counter
   in synthetic context slot `_while_iter.<state>` increments.
5. If `while:` is **falsy**, the workflow proceeds to the resolved
   target. Iteration counter is cleared.
6. If iteration count > `max_iterations`,
   `WHILE_ITERATION_CAP_EXCEEDED` fails the transition.

### 26.4 `max_iterations` is REQUIRED

There is no default. Operators MUST commit to a ceiling because:
- An unbounded loop is the classic poka-yoke failure (a guard
  condition that's wrong silently loops forever instead of failing).
- The audit log fills with iteration events; budget pressure surfaces.
- A wrong cap is recoverable (raise it after operator review); no cap
  is unrecoverable (it eats all available resources).

Configs that declare `while:` without `max_iterations:` fail at load
with `INVALID_STATE_CONFIG`.

### 26.5 Audit events

- `workflow.state.iteration` — fired each time the runtime re-enters
  the state due to a truthy while-guard. Payload:
  `{state, iteration, max_iterations}`. Each iteration is a distinct
  audited transition (separate `workflow.transition` record), so
  per-iteration observability is automatic.

### 26.6 Error codes

| Code | When |
|---|---|
| `INVALID_STATE_CONFIG` | State declares `while:` without `max_iterations:` |
| `WHILE_ITERATION_CAP_EXCEEDED` | While-guard remained truthy after `max_iterations` iterations |

### 26.7 Composition

`while:` composes with everything else:
- **`while:` + `parallel:`** — parallel fans out branches inside a
  state; while-guard re-enters that state after the parallel
  completes, so the parallel re-runs each iteration.
- **`while:` + `pipeline:`** — same; pipeline runs once per iteration.
- **`while:` + `branches:`** — branch-based target picking is
  evaluated first; while-guard overrides the picked target.
- **`while:` + `delegate:`** — sub-agent re-spawns each iteration
  (mind your budget — N sub-agent runs, N times the tool-call cost).

## 27. State-local blackboard slots (closes §15 open question)

### 27.1 Status

SPEC §15 had "State-local blackboard slots — deferred (lifecycle
complexity)" as an open question. The lifecycle was the blocker, not
the slot declaration itself. §27 closes the question with an explicit
lifecycle:

- **Declaration**: a state may declare `slots:` with `scope: state`
  entries (default scope is `workflow`, the current behaviour).
- **Visibility**: a state-local slot is visible to guards, executors,
  and templates ONLY while the workflow is in (or descended-from)
  that state.
- **Initialization**: cleared on state ENTER; values persist across
  iterations of `while:` re-entry on the same state.
- **Cleanup**: cleared on state EXIT — including chain-hop exits and
  `while:` falsy-guard exits.
- **Audit**: a `workflow.slot.cleared` event fires on exit, naming
  each slot that was cleared.

### 27.2 Declaration

```yaml
states:
  retrieving:
    slots:
      retrieval_attempts:
        type: integer
        default: 0
        scope: state              # NEW — defaults to workflow if absent
      partial_results:
        type: object
        scope: state
    transitions:
      retry:
        # writes to context.retrieval_attempts increment per try
        executor: ...
```

### 27.3 Lifecycle semantics

| Event | Action |
|---|---|
| `praxec.command({definitionId})` (workflow start) lands in state S with `slots: { scope: state }` declarations | Each declared slot is initialized to its `default:` value (or omitted if no default). |
| Transition fires from state S to state T (T ≠ S) | Every state-local slot declared on S is cleared from context. `workflow.slot.cleared` audit event fired with `{state: S, slots: [<names>]}`. |
| Transition fires from S back to S (via `while:` re-entry or explicit self-loop) | State-local slots PERSIST. Iteration n+1 sees iteration n's values. |
| Chain hop S → T → U (S has state-local slots; T does too) | S's slots cleared on S→T; T's slots cleared on T→U. Standard transition cleanup. |

### 27.4 Namespace

State-local slot names share the context namespace with workflow-scope
slots — the runtime tracks scope via the snapshot's slot declarations,
not via a name prefix. **Name collision** between a state-local slot and
a workflow-scope slot (or another state's local slot with the same
name) is rejected at config-load with `INVALID_SLOT_REDECLARATION`.

This means operators see clean `$.context.retrieval_attempts` paths in
guards and templates, with the runtime ensuring the slot is bound to
the right scope.

### 27.5 Migration

Existing configs with no `scope:` field on slot declarations continue
to behave as before (workflow-scoped). The feature is fully additive;
operators opt in by adding `scope: state`.

### 27.6 Why this matters

- **Polling state counters** — `retrieval_attempts` is meaningful in
  the `retrieving` state, noise everywhere else. Without scoping, it
  leaks into downstream state context.
- **Sub-task intermediate results** — `partial_results` exists while a
  state is converging; cleared once the state hands off a finalized
  output. Prevents stale-intermediate-state contamination.
- **`while:` loop accumulators** — counter slots like `consecutive_failures`
  belong to the looping state's lifetime; clearing them on exit is
  the correct semantic.

### 27.7 Implementation status

- **Declaration parsing**: shipped (this section).
- **Runtime ENTER initialization + EXIT cleanup**: phase 1 implementation
  in `runtime_submit.rs` (cleared on transition to different state).
- **Audit event emission**: shipped (`workflow.slot.cleared`).
- **`INVALID_SLOT_REDECLARATION` validator**: pending — runtime allows
  redeclarations today; validator catches them as a follow-up tranche.

## 28. Declarative slot constraints

### 28.1 Why

Without slot-level constraints, "this slot may only ever hold X" turns
into an external verifier script per slot. Procedural. Invisible at
authoring time. Reports failure via script exit code rather than a
structured event.

Slot constraints make the predicate declarative, evaluated at the
moment of harm (write time), and surface failure via a typed
`SLOT_CONSTRAINT_VIOLATED` event naming the slot, kind, and value.

### 28.2 What this is NOT

JSON Schema overlap — `pattern`, `minimum`, `maximum`, `minLength`,
`maxLength`, `enum` — is handled by the slot's existing `type:` field
(see SPEC §6.2 / `validate_blackboard_writes`). Constraint kinds in
§28 are scoped to things JSON Schema CANNOT express:

- File-path allowlist with glob patterns (`path_allowlist`)
- Subset-of dynamic-path reference (`subset_of`)

Power-user constraint expressions (full guard syntax over slot
values) are deferred until a real operator demand surfaces.

### 28.3 Declaration

```yaml
blackboard:
  changed_files:
    type: array
    items: { type: string }
    constraint:
      path_allowlist:
        allow:                              # required, non-empty
          - "auth/**"
          - "tests/auth/**"
        deny:                               # optional, applied within allow
          - "auth/legacy/**"

  active_features:
    type: array
    constraint:
      subset_of: "$.context.declared_features"

  # State-local slots (SPEC §27) carry constraints with the same shape.
states:
  editing:
    slots:
      edited_files:
        type: array
        scope: state
        constraint:
          path_allowlist:
            allow: ["src/auth/**"]
```

### 28.4 Constraint kinds

#### `path_allowlist: { allow: [<glob>...], deny?: [<glob>...] }`

- Slot value MUST be a JSON array of strings.
- Every element MUST match at least one `allow:` glob.
- If `deny:` is present, no element may match a `deny:` glob.
- Glob syntax: gitignore-compatible (via the `globset` crate that
  ripgrep / cargo use). `*` matches anything within a path segment;
  `**` recurses; `?` matches one character; `[...]` matches a class.
- `allow:` may NOT be empty — an allow-everything constraint is
  misconfiguration, not a feature. Empty `allow:` rejects at load.

#### `subset_of: "<path>"`

- Value MUST be a JSON array.
- Referenced path resolves against the post-write context.
- Every element of the constrained slot's value MUST appear in the
  referenced array.
- An unresolvable reference (path is `null` / unset) is **fail-fast**,
  not silent-pass — `SLOT_CONSTRAINT_VIOLATED` names the unset path so
  the operator can fix ordering.
- Supported reference prefixes: `$.context.*`. Other prefixes resolve
  to null (silent — fail-fast then catches).

### 28.5 Composition

Multiple constraint kinds on one slot compose **conjunctively** — every
declared kind must pass. The first failing kind short-circuits and
surfaces. (No need to enumerate all failures; the operator sees one
clear violation, fixes it, retries; subsequent failures surface on
later iterations.)

### 28.6 Evaluation timing

Constraints are evaluated at the SAME hook as typed-slot schema
validation (`validate_blackboard_writes`, SPEC §6.2): AFTER the
executor's output is merged into context, BEFORE the transition
commits. A violation aborts the transition exactly like
`BLACKBOARD_TYPE_ERROR` — the version stays at pre-transition.

### 28.7 Load-time validation

Constraints are also validated at config load. Catches:

- Empty `allow:` (misconfiguration)
- Malformed glob patterns
- Unknown constraint kinds
- `subset_of` value that isn't a string path

→ `INVALID_CONSTRAINT_DECLARATION` at load, never at runtime.

### 28.8 Error code

| Code | When |
|---|---|
| `INVALID_CONSTRAINT_DECLARATION` | Load-time: shape, kind, or pattern is malformed |
| `SLOT_CONSTRAINT_VIOLATED` | Runtime: the slot's post-write value violates a declared constraint |

### 28.9 Audit

Violations emit the existing `transition.rejected` event with code
`SLOT_CONSTRAINT_VIOLATED`. Per-slot violation rates can be
aggregated by audit-log consumers to spot over-tight constraints
(e.g. "slot X rejects 30% of agent outputs — pattern may need
widening").

## 29. Human-in-the-loop interaction

### 29.1 Two HITL surfaces, two purposes

| Surface | When | Mechanism |
|---|---|---|
| **State-change HITL** — `actor: human` transition that advances state | Approvals, gates, merges (architectural decisions) | Existing — SPEC §6.x |
| **Interaction HITL** — `actor: human` self-loop transition that returns to the same state | Mid-reasoning clarifications, judgment calls | NEW — this section |

§29 closes the gap of "agent encounters ambiguity mid-reasoning; today
must guess silently or escalate to a heavy state-change gate."

### 29.2 Why this isn't a new MCP tool

A self-loop `actor: human` transition gives the same effect as a
"human.ask" tool would, AND:

- reuses existing transition machinery (validation, audit, reliability)
- reuses existing seven-tool MCP surface (no STABILITY commitment to an 8th tool)
- inherits existing `actor: human` gating (only humans can submit)
- inherits existing `inputSchema`/`outputSchema` (questions arrive typed)
- inherits existing timeout machinery (`definition.timeoutMs` + `onTimeout`)

The cost: per-state declaration burden. §29.3 solves that.

### 29.3 `enable_human_ask: true` workflow flag

When a workflow declares `enable_human_ask: true` at root level, the
runtime auto-injects a self-loop `ask_human` transition into every
non-terminal state at config-resolve time:

```yaml
workflows:
  agentic_change:
    enable_human_ask: true        # ← all non-terminal states gain ask_human
    human_ask_cap: 5              # ← optional; default 5, used as max_fires_per_visit
    initialState: planning
    states:
      planning: { ... }           # auto-gains ask_human
      editing:  { ... }           # auto-gains ask_human
      done:     { terminal: true } # excluded — no questions on terminal states
```

The injected transition:

```yaml
ask_human:
  target:              <same state>      # self-loop
  actor:               human
  purpose:             ask               # for dashboard/client filtering (§29.5)
  lightweight:         true              # emits workflow.interaction (§29.4)
  max_fires_per_visit: <human_ask_cap>   # per §29.6
  inputSchema:                           # forces context with every question
    type: object
    required: [question, context_summary, attempted_alternatives]
    properties:
      question:               { type: string, maxLength: 2000 }
      context_summary:        { type: string, maxLength: 1000 }
      attempted_alternatives: { type: string, maxLength: 1000 }
  outputSchema:
    type: object
    required: [answer]
    properties:
      answer:                 { type: string }
```

**Operator override**: if a state already declares an `ask_human`
transition, the injection skips it. Workflow author can supply a
tighter schema or a different cap per state.

### 29.4 Lightweight transition records

Self-loop interaction transitions pollute the state-change audit
signal if recorded as `workflow.transition` events. The `lightweight:
true` field on a transition declaration changes its audit event type
to `workflow.interaction` while keeping the same record payload.

Audit-log consumers can:
- Filter by event type for the clean state-change story
- Filter by `purpose:` for specific interaction kinds (`purpose: ask`)
- See both via `correlationId` join for full history

**No behavior change to non-lightweight transitions.** Existing
workflows continue emitting `workflow.transition` unchanged.

### 29.5 `purpose:` tag

Optional string on any transition. When present, propagates into the
audit record's `purpose` field. The convention is short identifiers
operators can filter on:

- `purpose: ask` — agent → human clarification
- `purpose: approve` — human approval gate
- `purpose: escalate` — bail-out path

Not a closed enum — operators add purposes as their workflows evolve.

### 29.6 Per-state fire cap (`max_fires_per_visit`)

**Generic field** on any transition (not HITL-specific). When declared,
the runtime tracks per-state-entry fire counts in synthetic context
slot `_fire_count.<state>.<transition>`. Counter resets when the
workflow leaves the state. Exceeding the cap rejects with
`TRANSITION_FIRE_CAP_EXCEEDED`.

**Why generic, not HITL-specific:** any transition that can re-fire
on a self-loop or via `while:` could benefit from a cap. The generic
mechanism applies to `ask_human` (default cap 5 via `human_ask_cap`)
but also to operator-defined self-loops like `retry_extraction`.

**`attempted_alternatives` field** (in the injected `inputSchema`) is
the TRIZ #25 (Self-Service) poka-yoke for HITL specifically: agents
must DEMONSTRATE effort before interrupting humans. Audit captures
the field so post-hoc review can spot agents that over-ask without
trying alternatives.

### 29.7 When to promote to a first-class MCP tool

If documented evidence from ≥3 operator workflows shows the
self-loop-transition pattern is too verbose for the ad-hoc
clarification use case, promote to a first-class MCP tool
`human.ask`. The criterion is concrete; not speculative.

Until then, §29 is the canonical interaction-HITL pattern.

### 29.8 Error codes

| Code | When |
|---|---|
| `TRANSITION_FIRE_CAP_EXCEEDED` | Transition fired ≥ `max_fires_per_visit` times in current state-entry |

(`ACTOR_MISMATCH` already applies — only humans can submit
`actor: human` transitions including `ask_human`.)

## 30. Lexicon / Ubiquitous Language

### 30.1 Why a runtime primitive

A skill can extract terms via Socratic questioning. To be reusable
across runs, the result needs a STORE that:

1. **Snapshot-stamps** onto in-flight workflows so a run started
   before a redefine keeps the old understanding (same invariant as
   `_skillsLibrary` per §8.2, `_scriptsLibrary` per §22.5).
2. **Is searchable** from any workflow via `praxec.query({kind: "lexicon", query})`.
3. **Is human-governed by default** so vocabulary doesn't drift
   silently as agents propose definitions.
4. **Is version-controllable** (Tier 1: lives in `praxec.yaml`,
   operators commit + review via PR).

That's a runtime primitive — declarative storage + governance + MCP
tools — not a prompt template.

### 30.2 Tier 1 — Per-config (shipped in v0.4.x)

The top-level `lexicon:` block in `praxec.yaml`:

```yaml
lexicon:
  connector:
    bounded_context: gateway          # DDD bounded context (optional)
    definition: |
      A unit of integration between the gateway and an external system.
    examples:                          # optional
      - { kind: mcp, name: scip-server }
    refs:                              # optional cross-links to other terms
      - capability
      - executor
    governance: human-only             # default: human-only; alternative: agent-may-propose
```

Tier 2 (per-operator file store) and Tier 3 (multi-tenant DB) follow
the same shape; the in-config form is the canonical v0.4.x.

### 30.3 Validation

At config load, `lexicon.<term>` entries are validated:

- `definition` is REQUIRED and must be non-empty
- `governance`, when set, must be `human-only` or `agent-may-propose`
- `refs`, when set, must be an array of strings (term names)

Violation → `INVALID_LEXICON_ENTRY` naming the offending term and
field.

### 30.4 Snapshot stamping

At config-load, every workflow gets a `_lexiconLibrary` snapshot on
its definition. In-flight workflows are immune to edits of the
top-level `lexicon:` block — they see the lexicon that existed at
`praxec.command({definitionId})` start time. Same invariant as
`_skillsLibrary` / `_scriptsLibrary`.

### 30.5 MCP tools

Lexicon operations dispatch through the two-tool surface (§32) rather
than dedicated tools:

| Operation | Call | Args | Returns |
|---|---|---|---|
| Search | `praxec.query({kind:"lexicon", query, bounded_context?, limit?})` | query string | `{hits: [{term, definition_short, aliases?, ...}]}` |
| Lookup | `praxec.query({subject:"lexicon:<term>", bounded_context?})` | exact term | `{term, entry}` (entry may be null) |
| Define | `praxec.command({subject:"lexicon:<term>", definition:{definition_short, definition_long?, aliases?, refs?, bounded_context?, governance?}})` | term + entry fields | `{term, entry, persisted_to: "overlay"}` |

The `definition` object in the define call aligns with §30.10.1
(`aliases: string[]`) and §30.10.10.1 (`definition_short` required
one-sentence summary; `definition_long` optional multi-paragraph
detail). Search results use `definition_short` for inline previews.
Full lookup responses include `definition_long` when present.

Search and lookup read the union of the config-loaded base + a
runtime overlay (overlay wins on collision). Define writes to the
overlay only; operators persist by editing `praxec.yaml` and
reloading. Overlay survives only for the runtime's lifetime.

### 30.6 Governance gate

The `governance:` field on each lexicon entry is either:

- `human-only` (default) — agent callers writing via
  `praxec.command({subject:"lexicon:<term>", definition:{...}})` get
  `LEXICON_DEFINE_REQUIRES_HUMAN`. The workflow must route through an
  `actor: human` transition (or a human-principal call surface) to
  commit the change.
- `agent-may-propose` — agents can define directly. Suitable for
  scratch / sandbox contexts.

`human-only` is the load-bearing default because vocabulary is a
human contract. Operators opting into `agent-may-propose` are making
an explicit choice to accept faster iteration over discipline.

**Placeholder-fill bypass (§30.10.5).** When the runtime surfaces a
`SUBJECT_NEEDS_DEFINITION` interaction, the `define_new` link it
returns is pre-filled with `praxec.command({subject:"lexicon:<term>",
definition:{...}})`. Following this link is NOT subject to the
`human-only` governance gate on the placeholder — the guard system
treats a `PENDING_DEFINITION`-sourced define call as a first-write
(the term has no governance field yet; the caller supplies one). The
result adopts whichever `governance` value the caller provides, and
subsequent edits honor it normally.

### 30.7 Audit

Every successful `praxec.command({subject:"lexicon:<term>", definition:{...}})`
emits a `lexicon.defined` event with payload `{term, bounded_context, by_human}`.
Combined with the existing `workflow.transition` audit-event payload (note:
`"workflow.transition"` is an `event_type` value in the audit payload, not a
tool name) per route-to-human, this gives operators full replay of vocabulary
changes.

### 30.8 Error codes

| Code | When |
|---|---|
| `INVALID_LEXICON_ENTRY` | Load-time: missing definition / unknown governance / bad refs |
| `LEXICON_DEFINE_REQUIRES_HUMAN` | Runtime: agent attempted to define a `human-only` term |

### 30.9 Future tiers (out of scope for v0.4.x)

- **Tier 2 — Per-operator file store**: `~/.praxec/lexicon/<context>.yaml`
  files merged at config-resolve time; accumulates across configs
- **Tier 3 — Multi-tenant DB**: `LexiconStore` trait + SQLite
  backends; queryable, multi-operator, audit-integrated

Tier shape will be additive. Tier-1 configs remain valid against
Tier-2/3 deployments.

### 30.10 Aliases, placeholders, and the `SUBJECT_NEEDS_DEFINITION` interaction (queued for v0.5)

Three additions sharpen the lexicon from "supplementary documentation"
into the **schema for the system's vocabulary**, enforced by the runtime
without breaking ergonomics for greenfield configs that haven't yet
authored every entry.

#### 30.10.1 Aliases field on lexicon entries

The §30.5 entry schema gains an `aliases: string[]` field. Aliases
are recognized surface forms of the canonical `term` — singular vs.
plural, hyphen vs. underscore vs. space variants, common abbreviations.
A lookup against any alias returns the same entry as a lookup against
the canonical term.

```yaml
lexicon:
  evidence-pack:
    definition: "A bundle of facts the editor uses to plan a change."
    aliases: ["evidence-packs", "evidence pack", "evidence packs"]
    bounded_context: swe-agent
    refs: ["acceptance-criteria"]
    governance: human-only
```

**Load-time validation:** within a single bounded context, no alias may
collide with another entry's term or alias. Violation →
`LEXICON_ALIAS_COLLISION` naming both entries. Aliases across different
bounded contexts may overlap.

**Implementation:** at snapshot-pin time, build a single
`HashMap<String, &LexiconEntry>` keyed by canonical term + every alias,
all pointing at the same entry. O(1) lookup against any surface form.

#### 30.10.2 The verb-subject pair: only the subject is in scope

Praxec already maintains closed-enum verb taxonomies (`cognitive-verbs`,
`cap-verbs`, `script-verbs` per §17). The lexicon registers **subjects
only** — the noun half of every `<verb>.<subject>` identifier in the
system. The verb half is validated against the existing taxonomies.

Validation walks every config-level subject reference (script subjects,
skill subjects, capability subjects, transition delegate targets,
workflow `system`/`subject` metadata) and confirms the subject portion
is a registered lexicon entry. Unregistered subjects do not hard-fail
the load (see §30.10.3); they become placeholders.

#### 30.10.3 `PENDING_DEFINITION` placeholders

At config load, every subject referenced by the resolved config but not
yet defined in the lexicon receives a placeholder entry:

```rust
LexiconEntry {
    term: "evidence-foo",
    state: PENDING_DEFINITION,
    referenced_in: vec![/* file:line locations */],
    bounded_context: /* inherited from referencing config */,
    /* definition, aliases, refs all unset */
}
```

Load succeeds; the placeholder is enumerable via the lexicon query
surface. **Doctor reports the placeholder list** under a new check
`lexicon coverage`, including which workflows reference them. The
placeholder's presence blocks execution of any workflow whose reachable
subject set includes it (see §30.10.4).

`praxec.query({ kind: "lexicon", state: "PENDING_DEFINITION" })`
lists placeholders for operator review.

#### 30.10.4 Pre-start subject walk: no execution without lexical clarity

When `praxec.command` is called with `definitionId` (a workflow
start), the runtime walks the workflow definition's **reachable
subjects** — every typed subject reference plus every alias that
guidance bodies in scope might match against. If any reachable
subject resolves to a `PENDING_DEFINITION` placeholder, the start is
**paused, not executed**: the runtime returns a structured
`SUBJECT_NEEDS_DEFINITION` interaction (see §30.10.5).

This invariant ensures the pinned snapshot at `praxec.command({definitionId})`
start time (§8.2) is always complete — every subject reachable from
the workflow definition has a real lexicon entry. There is no in-flight
workflow with a partially-resolved lexicon, and no need to retroactively
mutate a pinned snapshot.

Mid-workflow surprises (a guard expression that introduces a dynamic
subject; a workflow whose purpose IS lexicon authoring per §17) hit
the same flow: the transition pauses, surfaces
`SUBJECT_NEEDS_DEFINITION`, waits for resolution, then resumes. The
current state does not advance during resolution.

#### 30.10.5 `SUBJECT_NEEDS_DEFINITION` interaction protocol

When the runtime detects an unresolved subject in the path of a
command, it returns a structured response with:

```json
{
  "interaction": {
    "kind": "SUBJECT_NEEDS_DEFINITION",
    "unknown_subject": "evidence-foo",
    "context": {
      "encountered_in": "workflow:swe_agent state:retrieving",
      "bounded_context": "swe-agent"
    },
    "candidates": [
      { "term": "evidence-pack", "distance": 2, "match_kind": "fuzzy_close",
        "definition_preview": "A bundle of facts the editor uses…" },
      { "term": "evidence-record", "distance": 3, "match_kind": "fuzzy_loose",
        "definition_preview": "A persisted record of an evidence event…" }
    ]
  },
  "queued_command": {
    "method": "praxec.command",
    "args": { /* original command verbatim — replay after resolution */ }
  },
  "links": [
    {
      "rel": "link_as_alias",
      "method": "praxec.command",
      "args": {
        "subject": "lexicon:evidence-pack",
        "definition": { "aliases_add": ["evidence-foo"] }
      },
      "hint": "Use this if 'evidence-foo' is a synonym for 'evidence-pack'."
    },
    {
      "rel": "define_new",
      "method": "praxec.command",
      "args": {
        "subject": "lexicon:evidence-foo",
        "definition": {
          "definition": "<fill in>",
          "boundedContext": "swe-agent"
        }
      },
      "hint": "Use this if 'evidence-foo' is a genuinely new concept."
    },
    {
      "rel": "cancel",
      "method": "praxec.command",
      "args": {
        "intent": "cancel_pending_subject",
        "unknown_subject": "evidence-foo"
      },
      "hint": "Abandon the original command — the subject was a mistake."
    }
  ]
}
```

The model (or operator) follows one link. Resolution updates the live
lexicon (or drops the placeholder for `cancel`). The client retries
the original command. The retry passes the subject walk; the snapshot
pins; the workflow starts.

#### 30.10.6 Levenshtein candidate ranking

The `candidates` list is populated by Levenshtein distance against
canonical terms + aliases within the current bounded context (fallback
to global context if the bounded one yields no hits). Default
threshold: distance ≤ 2, top 5 candidates. `match_kind`:

- `fuzzy_close` — distance ≤ 1.
- `fuzzy_loose` — distance ≤ 2.

Candidates are advisory; the resolver picks any link, not necessarily
a candidate.

#### 30.10.7 Resolution handlers

- `link_as_alias`: append the unknown subject to an existing entry's
  `aliases` list. Audit: `lexicon.alias_added { term, alias }`. The
  alias becomes a recognized surface form for future lookups.
- `define_new`: upgrade the placeholder to a real entry with the
  provided definition. Audit: `lexicon.defined { term, bounded_context, by_principal }`.
- `cancel`: drop the placeholder. The original command remains
  un-executed. Audit: `lexicon.pending_cancelled { term, cancelled_by }`.

#### 30.10.8 Audit + observability

Every `SUBJECT_NEEDS_DEFINITION` interaction emits a
`lexicon.subject_unresolved` audit event with the unknown subject, the
encountered-in context, the candidates surfaced, and (on retry) the
resolution chosen. Combined with the audit trail from
`lexicon.alias_added` / `lexicon.defined` / `lexicon.pending_cancelled`,
operators can review the system's vocabulary evolution.

#### 30.10.9 Error codes added

| Code | When |
|---|---|
| `LEXICON_ALIAS_COLLISION` | Load-time: an alias collides with another entry's term or alias within the same bounded context |
| `SUBJECT_NEEDS_DEFINITION` | Runtime: a command's reachable subjects include a `PENDING_DEFINITION` placeholder (returned as `Ok(structured response)`, not an MCP protocol error) |
| `INVALID_RESOLUTION` | Runtime: a resolution call (`link_as_alias` / `define_new` / `cancel`) targets a subject that is not currently pending, or the resolution payload is malformed |

#### 30.10.10 Optional semantic embeddings (queued for v0.5, opt-in at the end)

Vector embeddings make the `SUBJECT_NEEDS_DEFINITION` candidates list
much smarter — the runtime can surface terms that are semantically
close even when they're lexically distant (e.g., "proof bundle"
matches `evidence-pack`; "user retention metric" matches `churn`).

**Embeddings are completely optional and OFF by default.** Because
they come with a real cost — either a third-party API bill or the
operator standing up a local embedding service — the system is
designed to be fully functional without them. The
`SUBJECT_NEEDS_DEFINITION` interaction, the alias system, and the
Levenshtein candidate ranking (Tiers 1, 2, 4) all work with
`embeddings.backend: none` (the default). Operators who want better
candidate quality flip the switch; everyone else gets a working
system with zero embedding overhead.

Implementation lands as the **last** step of Group 3 so that the
preceding lexicon discipline ships independently — if the embedding
work slips or stalls, the rest of Group 3 still delivers.

##### 30.10.10.1 Definition split

To support both human ergonomics and embedding quality, the
`definition` field splits into two:

| Field | Purpose |
|---|---|
| `definition_short` | One-sentence summary. Required. Used in candidate previews, inline lexicon embeds (200-byte budget per §30.10.6), `--list` output. |
| `definition_long` | Optional multi-paragraph detail. Used in full lookup responses + as additional embedding source. |

Embedding source (when enabled): `<canonical term> + <aliases joined> + <definition_short> + <definition_long if present>`. One embedding per entry.

##### 30.10.10.2 Backend configuration

Top-level `embeddings` config block (server-level, not per-bounded-context):

```yaml
embeddings:
  backend: http
  url: http://localhost:11434/api/embeddings   # or any provider endpoint
  model: nomic-embed-text                       # provider's model name
  dimensions: 768                               # validation guard; runtime rejects mismatched vectors
  request_format: ollama                        # ollama | openai_compatible
  api_key_env: OPENROUTER_API_KEY               # optional; absent for unauth'd local Ollama
```

Backends:

- **`none`** (default) — embeddings disabled; Tier 3 skipped.
- **`http`** — POST to an external embedding service.

No `local` backend in v0.5. Operators wanting a local model run their own server (Ollama, `llama-server`, vLLM) and point `http` at it.

Request adapters:

- **`ollama`** — POST `{ model, prompt }` → `{ embedding: [float; N] }`.
- **`openai_compatible`** — POST `{ model, input }` → `{ data: [{ embedding: [float; N] }] }`. Covers OpenAI, OpenRouter, Together, and most hosted providers.

##### 30.10.10.3 Lifecycle

- **At lexicon write** (`praxec.command({ subject: "lexicon:<term>", definition: {...} })`): synchronously compute the embedding before persisting. On embed-backend failure → reject the write with `EMBEDDING_BACKEND_FAILED`.
- **At config load** for entries that don't have a stored embedding yet (backwards compat, migration): batch-compute. Doctor reports the migration count + any failures.
- **At `SUBJECT_NEEDS_DEFINITION` time** for the unknown subject: synchronously embed the unknown subject + surrounding context (`"<verb> <unknown_subject>"`), nearest-neighbor against the per-bounded-context index.

##### 30.10.10.4 Tiered candidate ranking

The candidates list combines all available tiers, sorted by a unified score:

| Tier | Strategy | `match_kind` | When |
|---|---|---|---|
| 1 | Exact canonical | `exact` | Always. |
| 2 | Exact alias | `alias` | Always. |
| 3 | Semantic (embedding) | `semantic` | When `embeddings.backend != none`. Threshold default 0.85 cosine similarity. |
| 4 | Levenshtein fuzzy | `fuzzy_close` (≤1) / `fuzzy_loose` (≤2) | Always (fallback / tiebreaker). |

When Tier 3 is available, it dominates the ranking because semantic captures meaning, not just surface form. Tier 4 fills in when Tier 3 has no high-confidence match.

##### 30.10.10.5 New error code

| Code | When |
|---|---|
| `EMBEDDING_BACKEND_FAILED` | Runtime: embedding backend returned an error, timed out, or returned a vector of unexpected dimensionality. Lexicon-write commands are rejected; candidate ranking degrades to non-semantic tiers. |

##### 30.10.10.6 MCP parity

The embedding pipeline lives entirely in the runtime. MCP-only consumers, agentic runtimes, the CLI, and any future client all see the same response shape — they never compute embeddings themselves, never pass them in. The wire contract for `praxec.command` (write) and `SUBJECT_NEEDS_DEFINITION` (read) is unchanged whether embeddings are configured or not; only the *quality* of the candidates list shifts.

#### 30.10.11 SPEC §32 implications

This section resolves §32 open question OQ3 ("Lexicon term extraction
performance"). The replacement design is the typed pre-start walk
described in §30.10.4, not the regex-based prose scan originally
sketched. Aliases (§30.10.1) carry the surface-form variation that
the regex was meant to recover. Optional embeddings (§30.10.10) add
the semantic layer on top.

## 31. Pattern fragments + `extends:` (DRAFT — queued for v0.5)

**Status:** Design draft. The cognitive-architectures pattern-library
work (R2 / R4 / R7-CA of the 2026-05-25 plan) surfaced this as the
one real composition gap: pattern YAMLs must be `include:`d in full,
which means operators copy-edit when they want a tweaked variant.

### Why a praxec primitive

Today's composition story (via `include:`) loads a pattern's WHOLE
workflow definition into the host. Operators wanting to combine
multiple patterns or instantiate the same pattern twice with
different parameters end up copy-pasting YAML and editing slot
names / state names / glob patterns to avoid collisions.

A first-class `patterns:` + `extends:` mechanism eliminates the
copy-paste:

```yaml
patterns:
  scope_bounded_edit:                    # named pattern, parameterizable
    parameters:
      allow:                             # required
        type: array
        items: { type: string }
      deny:
        type: array
        items: { type: string }
        default: []
    workflow:                            # the template, references parameters
      initialState: editing
      blackboard:
        changed_files:
          type: array
          constraint:
            path_allowlist:
              allow: { $pattern_param: allow }
              deny:  { $pattern_param: deny }
      states:
        # ... pattern body ...

workflows:
  my_auth_change:
    extends: scope_bounded_edit          # instantiate with overrides
    parameters:
      allow: ["src/auth/**", "tests/auth/**"]
      deny:  ["src/auth/legacy/**"]
    # ...host workflow may add additional states / overrides...
```

### Proposed semantics

| Concept | Mechanism |
|---|---|
| Pattern definition | New top-level `patterns:` block. Each entry: `{parameters: <jsonschema>, workflow: <workflow-template>}` |
| Pattern reference | `extends: <pattern-name>` on a workflow declaration |
| Parameter substitution | `{$pattern_param: <name>}` placeholders in the pattern body, replaced at config-resolve time |
| Override | The extending workflow can ADD states / transitions; CAN'T contradict the pattern (override = error) |
| Instantiation | Each `extends:` materializes a full workflow definition; the resolved snapshot is what runs (operator-level patterns become snapshot-time substitutions, not runtime indirection) |
| Multi-instance | Same pattern extended by N workflows → N independent materializations; no slot-name collision |

### Gaps closed

- **G1 (no reusable fragments)** — solved by `patterns:` block
- **G3 (`extends:` for parameterization)** — solved by `extends:` field

### Errors

| Code | When |
|---|---|
| `UNKNOWN_PATTERN` | `extends: X` references a pattern not declared in `patterns:` |
| `MISSING_PATTERN_PARAMETER` | Required parameter not provided |
| `PATTERN_PARAMETER_TYPE_MISMATCH` | Provided value doesn't match parameter schema |
| `PATTERN_OVERRIDE_CONFLICT` | Extending workflow tries to redefine a pattern-declared state |

### FMECA notes

| Risk | Mitigation |
|---|---|
| Parameter substitution explodes (template inside template) | Single-pass substitution; nested patterns require explicit `extends:` chain |
| Pattern + workflow scope collision (slot names) | Pattern declares its slots; host's slots merge or error on collision |
| Runtime drift between pattern source + extending workflows | Pattern body is resolved into each extending workflow at config-load; snapshot-stamped per SPEC §8.2; pattern-source edits only affect new starts |

### Implementation plan (v0.5)

1. New module `crates/praxec-core/src/patterns.rs` — parse, validate, resolve `patterns:` block
2. Config-resolve step: for each workflow with `extends:`, materialize the pattern with parameter substitutions, then merge any host-added states / transitions
3. Schema: `$defs/patternDefinition`, `$defs/patternReference`
4. Drift test extension: pattern names + parameter shapes
5. CA library refactor: convert R2/R4 patterns to use the `patterns:` block instead of full workflow YAMLs

### Out of scope for this draft

- Cross-config pattern import (Tier 2 / Tier 3 lexicon-style)
- Parameter constraints beyond JSON Schema
- Pattern composition (patterns extending patterns)

Tracked: this section will be promoted to a full SPEC chapter when v0.5
implementation begins. Until then, the `include:` + copy-edit workflow
is canonical.

## 32. Tool-surface consolidation: `praxec.query` + `praxec.command`

**Status:** Shipped. Replaces the former 10-tool surface
(`gateway.home / .search / .describe`, `workflow.start / .get / .submit /
.explain`, `gateway.lexicon.search / .lookup / .define`) with **two**
tools split by CQRS: `praxec.query` for reads, `praxec.command` for
state-changing writes. Lexicon stays on the surface as a `subject`-
namespaced primitive (`subject: "lexicon:<term>"` on both query and
command) and additionally rides along as an embedded `lexicon` field in
describe/get/explain responses to reduce follow-up lookup chatter.
Greenfield clean cut — no deprecation aliases.

### Why

Two motivations land at the same answer:

1. **Project invariant 9** says the tool count is stable regardless of
   how many capabilities you wire in. Ten tools is more than needed —
   the model never *picks* a tool by semantics; it picks by reading the
   response's `links[].method + args` and copying. The tool name carries
   nearly no decision weight.
2. **MCP hosts gate permissions per tool name.** The honest axis for
   that gate is read vs. write — operators want to auto-approve
   read-only browsing and require confirmation for state-changing
   moves. Three or seven tools over-segment that axis; one tool
   collapses it. Two tools split exactly on it.

### The two tools

#### `praxec.query`

Side-effect-free reads. Audit fires on **describe-in-workflow** only —
calls whose args contain BOTH `subject` AND `workflowId`. Browse-time
describe (`subject` alone, no `workflowId`) does NOT audit; it's
operator/model exploration of the catalog, not a "guidance fetched
for use" event per §5.8. This matches today's `handlers.rs:74-114`
behavior, which gates the audit emission on `workflow_id` being
present. Audit does not fire on home, search, get, or explain in any
form.

Operations covered: `home`, `search`, `describe`, `get`, `explain`.
Lexicon search (`query` + `kind: "lexicon"`) and lexicon lookup
(`subject: "lexicon:<term>"`) ride on top of `search` and `describe`
respectively — no separate dispatch rows. Lexicon lookup audits only
when invoked with a `workflowId` (consistent with the rule above).

Schema (sparse args; required-field shape determines which operation
runs; all fields optional in JSON Schema):

```json
{
  "query":      "string",   // search
  "kind":       "string",   // search filter (workflow | skill | script | capability | …)
  "subject":    "string",   // describe
  "workflowId": "string",   // get + explain
  "transition": "string",   // explain (alongside workflowId)
  "limit":      "integer"   // search
}
```

Dispatch:

| Args present | Operation |
|---|---|
| (none) | `home` |
| `query` | `search` |
| `subject` only | `describe` (live config) |
| `subject` + `workflowId` | `describe` (against the workflow instance's pinned snapshot, per §8.2) |
| `workflowId` + `transition` | `explain` |
| `workflowId` alone | `get` |

**Modifiers vs. dispatch keys:** the rows above list the *required*
fields that select an operation. Other schema fields are optional
modifiers on the matched operation — `kind` filters the search,
`limit` bounds it, `trace_id` / `run_id` (when present at the query
schema in the future) thread through to audit. Adding a modifier does
not change which row matches.

Any combination of required fields not in the table returns a
structured error response with a HATEOAS link suggesting the corrected
call.

#### Subject namespace (cross-primitive discriminator)

The `subject` field admits a colon-prefixed namespace to disambiguate
which primitive the lookup targets:

| Prefix | Resolves to |
|---|---|
| (none) | guidance fragment, workflow definition, capability, or script — the discovery index decides by collision-free naming |
| `lexicon:<term>` | a single lexicon entry |
| `workflow:<id>` | a workflow definition (explicit form when an unprefixed name would collide) |
| `script:<subject>` | a curated script body (explicit form) |
| `skill:<subject>` | a guidance fragment (explicit form) |

Unprefixed subjects keep working — the prefix is only required when an
explicit namespace is needed to defeat a collision or to target the
lexicon (which has no naming overlap with the other primitives but
benefits from the explicit prefix for clarity in HATEOAS links). The
same prefix scheme applies to `praxec.command`'s `subject` field for
writes (today: lexicon-define only; future writable primitives slot in
without surface change).

#### `praxec.command`

State-changing writes.

Operations covered: `start` (workflow), `submit` (workflow transition),
`define` (lexicon entry — and any future writable primitive that fits
the subject-namespace pattern).

Schema (sparse args; the present-fields shape selects the operation):

```json
{
  "definitionId":    "string",   // start
  "input":           "object",   // start
  "workflowId":      "string",   // submit
  "expectedVersion": "integer",  // submit (optimistic concurrency)
  "transition":      "string",   // submit
  "arguments":       "object",   // submit
  "subject":         "string",   // define — e.g. "lexicon:<term>"
  "definition":      "object",   // define — body shape per §30.5: { definition, bounded_context?, refs?, governance? }
  "summary":         "string",   // submit
  "trace_id":        "string",   // any
  "run_id":          "string"    // any (also uniqueness assertion on start, see below)
}
```

Dispatch:

| Args present | Operation |
|---|---|
| `definitionId`, no `workflowId`, no `subject` | `start` |
| `workflowId` + `transition` + `expectedVersion` | `submit` |
| `subject` (namespaced, e.g. `lexicon:<term>`) + `definition` | `define` |

**Modifiers vs. dispatch keys:** the rows list the *required* fields
that select an operation. Other schema fields are optional modifiers on
the matched operation — `input` on `start`, `arguments` / `summary` on
`submit`, `trace_id` / `run_id` on any (with `run_id` additionally
serving as the idempotency token on `start`). Adding a modifier does
not change which row matches.

Mutually-exclusive combinations of required fields (e.g., both
`definitionId` and `workflowId`, or `subject` set alongside workflow
fields) return a structured error with a HATEOAS link.

### Uniqueness on `run_id`

The existing `run_id` field — already threaded through `StartArgs` and
`SubmitArgs` per §20.2 — becomes a **uniqueness assertion** for `start`
commands, not a PUT-style idempotency token. Behavior:

- `command({ definitionId: X, run_id: R })` called when no instance
  exists with that `run_id`: creates and returns the new instance.
- `command({ definitionId: X, run_id: R })` called when an instance
  with `run_id == R` already exists (regardless of `definitionId`,
  state, or input): **returns a structured error** —
  `RUN_ID_ALREADY_RUNNING` — with a HATEOAS link to `get` so the
  caller can inspect what's already there:

  ```json
  {
    "error": {
      "code": "RUN_ID_ALREADY_RUNNING",
      "message": "An instance already exists with run_id 'r-abc123'.",
      "hint": "Each run_id is single-use. Fetch the existing instance with the linked get, or retry with a fresh run_id."
    },
    "links": [
      { "rel": "get", "method": "praxec.query", "args": { "workflowId": "<existing>" } }
    ]
  }
  ```

- `run_id` omitted: server mints one. No uniqueness guarantee — the
  caller opted out of the assertion.
- `submit` already has optimistic concurrency via `expectedVersion`;
  `run_id` on submit threads through to audit per §20.2 but does not
  add an additional uniqueness constraint there (submits within the
  same workflow legitimately reuse a session-level `run_id`).

Why explicit-fail over silent-return: an operator debugging a retry
storm wants to *see* the collision, not have it papered over. Same
posture as `EXPECTED_VERSION_MISMATCH` on submit — both are
optimistic-concurrency primitives that surface conflict to the caller.

Implementation: in `WorkflowRuntime::start`, look up the store by
`run_id` before issuing a new ID. If found, return
`RUN_ID_ALREADY_RUNNING`. Otherwise create and proceed.

### Lexicon — embedded reads; writes through `praxec.command`

Lexicon is a first-class primitive under the query/command split,
identical in shape to workflow start/get/submit. No special carve-out:

- **Reads** ride on top of the existing query dispatch via the
  `subject` namespace. Lexicon search is `praxec.query({ query,
  kind: "lexicon" })`; lexicon lookup is `praxec.query({ subject:
  "lexicon:<term>" })`. Both go through the same handler code paths
  the rest of describe/search use.
- **Writes** are `praxec.command({ subject: "lexicon:<term>",
  definition: {...} })`. The inner `definition` shape is the canonical
  lexicon entry per §30.5: `{ definition: string, bounded_context?:
  string, refs?: string[], governance?: "human-only" | "agent-may-propose" }`.
  The `term` is parsed from `subject` (the part after `lexicon:`), so
  it doesn't repeat inside `definition`. Audit fires as today (the
  existing `lexicon.defined` event), keyed off the subject prefix
  rather than a tool name.

#### Embedded definitions in describe/get/explain responses

Beyond the explicit lookup path, definitions for terms **in scope at
the call site** are embedded directly in `describe`, `get`, and
`explain` response bodies as a `lexicon` field — so the model rarely
needs to make a follow-up lookup call:

```json
{
  "kind": "guidance",
  "subject": "plan.specify.change-request",
  "body": "...",
  "lexicon": {
    "acceptance-criteria": "Pass/fail conditions a change must meet…",
    "blackboard":          { "hash": "sha256:…", "lookup_link": { "rel": "lexicon", "method": "praxec.query", "args": { "subject": "lexicon:blackboard" } } }
  },
  "links": [ … ]
}
```

The runtime extracts referenced terms by scanning guidance bodies at
load time and resolves them against the workflow's pinned snapshot
(SPEC §8.2). Inline definitions are included up to a configurable
size budget; oversized definitions become a `lookup_link` the model
can follow via a `subject: "lexicon:<term>"` query call.

#### Governance (when to forbid LLM-driven lexicon writes)

Whether the LLM should be allowed to extend the lexicon is a **policy
question, not an architecture question** — it belongs in the same
mechanism that governs `praxec.command` (submit mode): the guard system.
Two layers of control:

1. **Per-workflow guards.** A workflow author who wants a
   knowledge-curation step can declare the transition that emits a
   lexicon-define command; one that doesn't, can't. The same guard
   primitives (`{ kind: expr, expr: "$.principal.role == 'author'" }`
   etc.) gate the move.
2. **Runtime feature flag.** `PraxecServer::with_lexicon_writes(bool)`
   gates whether the runtime accepts `define` commands at all,
   mirroring the existing `with_skills_search` / `with_scripts_search`
   pattern. Default-on for authoring builds, default-off for
   production deployments where lexicon is curated content owned by
   operators.

**Reads are not gated.** Lexicon search and lookup are always
available — same posture as the other discovery surfaces (workflow
catalog, capability index). Operators who want to restrict which
*subjects* the LLM can fetch use guards on the workflow that issues
the query (the same lever that gates any other describe-shaped call).

**Define-when-disabled error shape.** When `with_lexicon_writes(false)`,
a `define` command returns a structured error with a HATEOAS link
pointing at the operator-facing alternative:

```json
{
  "error": {
    "code": "LEXICON_WRITES_DISABLED",
    "message": "This runtime does not accept lexicon define commands.",
    "hint": "Operators add lexicon terms via the `px lexicon define` CLI subcommand."
  },
  "links": [
    { "rel": "operator_path", "method": "cli",            "args": { "command": "px lexicon define <term> <definition>" } },
    { "rel": "lookup",        "method": "praxec.query", "args": { "subject": "lexicon:<term>" } }
  ]
}
```

The `cli` rel is informational (not a tool the model can call); the
`lookup` rel keeps the read path discoverable so the model can confirm
whether the term already exists before escalating.

A `px lexicon define <term> <definition>` CLI subcommand exists
as a **convenience for operators** who prefer not to drive lexicon
authoring through MCP — it's an alternative path, not the only one.
Both paths emit the same `lexicon.defined` audit event.

### HATEOAS contract (preserved + extended)

Every response — success or error — carries a `links[]` array of
`{ rel, method: "praxec.query" | "praxec.command", args: {...} }`
entries. The args object is pre-filled with the exact shape the next
legal operation needs. **Models chain by copying `link.args`
verbatim**; they never derive the next call from the schema.

The schema's job is to declare the universe of valid argument
combinations; the runtime's job (via responses) is to tell the model
exactly which subset is valid right now. Static schemas + dynamic
responses = no `tools/list_changed` plumbing required.

### Error response shape

Ambiguous / invalid arg combinations return a structured 4xx-class
response, not an MCP protocol error:

```json
{
  "error": {
    "code": "AMBIGUOUS_INTENT",
    "message": "both definitionId and workflowId set; pick one",
    "hint": "use definitionId to start a new instance, or workflowId+expectedVersion+transition to submit a transition on an existing instance"
  },
  "links": [
    { "rel": "start",  "method": "praxec.command", "args": { "definitionId": "swe_agent" } },
    { "rel": "submit", "method": "praxec.command", "args": { "workflowId": "...", "expectedVersion": 3 } }
  ]
}
```

The links point at exactly the two corrected calls. The model
recovers by following one.

### Authoring opt-ins (skills.search, scripts.search)

The current flag-gated tools `gateway.skills.search` and
`gateway.scripts.search` (SPEC §17.6, §22) collapse into the `search`
mode of `praxec.query` with a `kind` filter:

```json
{ "kind": "skill",  "query": "review.code" }
{ "kind": "script", "query": "build.cargo" }
```

The opt-in machinery (`with_skills_search(true)`,
`with_scripts_search(true)`) gates whether the runtime accepts these
`kind` values; default-off behavior preserved. A search with a
disabled `kind` returns an empty result + a `links[]` hint explaining
how to enable it.

### Migration: clean cut

praxec is greenfield; the old surface is removed in the same
release the new one ships. No deprecation aliases, no compatibility
shims, no parallel surfaces. The dispatch table maps each old tool to
the corresponding unified call as a one-time refactor:

| Old (removed) | New |
|---|---|
| `gateway.home` | `praxec.query({})` |
| `gateway.search` | `praxec.query({ query, kind, limit })` |
| `gateway.describe` | `praxec.query({ subject })` (audit fires on `subject` presence) |
| `workflow.start` | `praxec.command({ definitionId, input })` |
| `workflow.get` | `praxec.query({ workflowId })` |
| `workflow.submit` | `praxec.command({ workflowId, expectedVersion, transition, arguments })` |
| `workflow.explain` | `praxec.query({ workflowId, transition })` |
| `gateway.lexicon.search` | `praxec.query({ kind: "lexicon", query })` |
| `gateway.lexicon.lookup` | `praxec.query({ subject: "lexicon:<term>" })` |
| `gateway.lexicon.define` | `praxec.command({ subject: "lexicon:<term>", definition })` (gated by `with_lexicon_writes(true)`) |

Internal call sites (the HATEOAS link emission in `handlers.rs`,
`runtime.rs`, `runtime_links.rs`, `runtime_submit.rs`, `discovery.rs`,
`discovery_indexer.rs`) get updated in lockstep with the surface
change. The `TOOL_*` constants in `lib.rs:54-77` collapse to
`TOOL_QUERY` + `TOOL_COMMAND`. Tests update their string literals
in the same PR.

### Open questions

1. **Lexicon size budget.** What's the right default for inline-vs-link
   threshold per term in embedded `lexicon` fields? Suggest 200 bytes
   inline, `lookup_link` otherwise.
2. **`summary` field on start.** Today `StartArgs` accepts `summary`
   but the field is unused by the runtime. Drop it from `command`'s
   schema, or keep it as a no-op for forward compat?
3. **Lexicon term extraction performance.** **Resolved by §30.10.**
   The original "regex prose scan" framing is replaced by the typed
   pre-start subject walk (§30.10.4) against config-level identifiers
   plus the alias map (§30.10.1). Cost is bounded by the workflow
   definition's reachable surface, not the size of guidance prose.
   Cache per pinned snapshot, populated at start.
4. **`with_lexicon_writes` default.** Default-on for authoring builds
   (matches `with_skills_search` precedent) or default-off everywhere
   so production deployments are safe by construction?

### Implementation order

One landing PR, sequenced internally so each step compiles green:

1. Replace `TOOL_*` constants in `lib.rs:54-77` with `TOOL_QUERY` +
   `TOOL_COMMAND`. Add `praxec.query` / `praxec.command` to the
   `dispatch_call` match; delete the old arms (gateway/workflow/lexicon
   handlers stay — they're called from the new dispatch).
2. Add `subject` + `definition` to `CommandArgs`; add subject-namespace
   prefix routing (`lexicon:` first; `script:`/`workflow:`/`skill:`
   reserved). Wire `with_lexicon_writes(bool)` flag through
   `PraxecServer`.
3. Update every HATEOAS link emission site to use the new tool names:
   `handlers.rs` (multiple), `runtime.rs:406`, `runtime_links.rs:74`,
   `runtime_submit.rs:648`, `discovery.rs:404/420/426`,
   `discovery_indexer.rs:216/265`.
4. Any agentic harness that drives the gateway (e.g. the in-repo
   `praxec` TUI runtime) updates to the new tool names
   in lockstep — it depends on this crate, so the rename surfaces at
   its call sites.
5. `run_id` idempotency in `WorkflowRuntime::start` (check for existing
   instance by `(definitionId, run_id)` before creating).
6. Embedded `lexicon` field in describe/get/explain response bodies.
   Term extraction at `praxec.command({definitionId})` start time
   (regex first-pass + cache per pinned snapshot).
7. `px lexicon define <term> <definition>` CLI subcommand as
   operator-friendly convenience wrapper for the same handler.
8. Documentation: README + SPEC §5/§8.2/§12/§17/§22/§30 references
   updated. Site refresh: 10 → 2 tools, no deprecation note required.
   Operator-facing note: audit dashboards keyed on `tool_name =
   "gateway.describe"` rebuild to filter by `tool_name = "praxec.query"
   AND args.subject IS NOT NULL AND args.workflowId IS NOT NULL` — the
   audit *payload* schema is unchanged, only the discriminator the
   dashboard reads. Same shape for the other consolidated tools.
9. Test updates: every integration test that calls a tool by name
   updates its string literal in the same PR.

Shipped: `praxec.query` + `praxec.command` are the only MCP tools the
gateway advertises; this is the canonical surface.


## 33. In-runtime LLM executor

**Status:** shipped in v0.6 as the in-runtime LLM executor (crate
`praxec-llm-executor`), wired through runtime-drives-the-loop
(D3) per the architecture choice documented in §33.11 below.
Introduces a new `executor: { kind: llm, … }` config shape that lets a
workflow state dispatch to an LLM directly, in-process, with the
workflow's current transitions as the model's tool surface.
Repositions praxec from "MCP server that an external LLM drives" to
"runtime that hosts governed LLM execution alongside its MCP surface"
— without removing the MCP path.

### 33.1 Why

The §32 surface (`praxec.query` + `praxec.command`) gives external
LLM clients (Claude Code, Cursor, custom harnesses) a clean governed
interface. But when consumed externally, our governance is
**advisory** — the LLM in the client can:

- Ignore guidance returned by `describe`.
- Call other MCP tools instead of following HATEOAS links.
- Skip the workflow entirely.
- Use its own context rather than our pinned snapshot.

We hand it advice; it does what it wants. When governance matters
(compliance pipelines, audited automation, deterministic SWE-agent
loops), advisory-only is a meaningful weakness — operators have asked
for "the LLM literally cannot do anything except what the workflow
permits."

Hosting the LLM inside the runtime makes governance **enforced**:

- The tool surface the model sees IS the workflow's available
  transitions at the current state — nothing more.
- The context the model sees IS the pinned snapshot + embedded
  lexicon — never something it brought from outside.
- Cost / time / step limits are real, not hoped-for.
- The audit trail is complete because we wrote every line.

The existing infrastructure was designed for this without us realizing
it. Each piece — typed transitions, guards, blackboard, audit, pinned
snapshots, lexicon discipline — slots into LLM-execution naturally.

### 33.2 The Executor kind

A new `Executor` impl, `LlmExecutor`, registered in the executor
registry under `kind: llm`. Same trait as every other executor
(`crates/praxec-core/src/ports.rs::Executor`), so no core
changes — the integration is plug-in, not architectural.

```yaml
states:
  triaging:
    goal: "Decide whether the issue is a bug, feature request, or noise."
    transitions:
      mark_as_bug:
        target: investigating
        # ...
      mark_as_feature:
        target: backlog
        # ...
      close_as_noise:
        target: closed
        # ...
    executor:
      kind: llm
      model: anthropic/claude-sonnet-4-6     # or affinity: triage (resolved via models.yaml)
      prompt_template: |
        You are triaging a new issue. The issue body is in
        $.blackboard.issue_body. Pick exactly one transition.
      max_iterations: 3
      max_seconds: 60
      max_tokens: 2000                        # cost cap
```

The executor's job at runtime:

1. Read the current state's `goal`, `prompt_template`, and the
   blackboard slots the prompt references.
2. Resolve the available transitions at the current state into a tool
   list. Each transition becomes one tool with a JSON-Schema input
   matching its `inputSchema` (if any) — defaulting to `{}` for
   transitions that take no arguments.
3. Call `aether_llm::Provider::stream_response(context)` with the
   prompt + tools.
4. Interpret the streamed response. If the model selects a
   transition, dispatch it (runs guards, updates blackboard, advances
   state). If the model emits a final answer without a transition,
   surface an error — the model was supposed to pick a transition.
5. Loop until terminal state or `max_iterations` / `max_seconds` /
   `max_tokens` is reached.

### 33.3 Tool surface = available transitions

Critical invariant: **the LLM's tool list at each turn is exactly the
set of transitions the workflow allows at the current state.**

- Guards already ran (or will run when the transition is dispatched).
- A transition whose guards currently reject is omitted from the tool
  list — the model can't even see it. State-aware tool narrowing,
  enforced.
- New states bring new tool lists. The model never sees a tool that
  doesn't apply to where it is right now.

This is the §32 HATEOAS pattern at the LLM-tool layer: the model
chooses by picking a link, the runtime executes it, the next state
produces the next tool list.

### 33.4 Configuration: the shared `models.yaml` resolver

`models.yaml` resolves `name: provider/model` bindings via the affinity /
tier resolver in `crates/praxec-core/src/model_resolver/` (public).
The `kind: llm` executor reuses that same resolver:

```yaml
executor:
  kind: llm
  affinity: triage           # resolved via models.yaml
  prompt_template: "..."
```

This means **one config file** and **one resolver** drive both the
`kind: llm` executor and the agentic runtime's sub-agent
spawning. Operators don't configure providers twice.

Direct `model:` references (`model: anthropic/claude-sonnet-4-6`)
also work — they bypass models.yaml and use the provider name + model
directly. Useful for one-off cases; models.yaml is preferred for the
shared-config posture.

Provider authentication reads from standard provider env vars (e.g.
`ANTHROPIC_API_KEY`) / a `~/.praxec/providers.env` file — both
aether-llm (chat) and our `HttpEmbedder` (embeddings) read from the same
env vars. One auth setup; both consumers.

### 33.5 Optional cargo feature: `embeddings-llm-executor`

The `LlmExecutor` lives in a new crate `praxec-llm-executor`
behind a default-on cargo feature:

```toml
# crates/praxec/Cargo.toml
[features]
default = ["llm-executor"]
llm-executor = ["dep:praxec-llm-executor"]
```

- **Default install** includes the executor — `cargo install
  praxec` gets you a runtime that can host LLM calls.
- **Opt-out** for operators who don't want the aether-llm dep:
  `cargo install praxec --no-default-features`. The executor
  registry simply doesn't have a `kind: llm` entry; workflows
  referencing it fail at config load with a clear error.
- **Replace with rig** (hypothetical future): a parallel
  `praxec-llm-executor-rig` crate could provide the same trait
  with rig as the backend. Operators pick one feature or the other.

### 33.6 Composition with existing executors

`LlmExecutor` is just another executor. It composes naturally with the
existing executor primitives:

- **`parallel`** — fan out multiple `LlmExecutor` calls concurrently
  (e.g., consult three models in parallel; the join condition
  decides which result advances the workflow).
- **`pipeline`** — chain `LlmExecutor → script → mcp` to extract
  data with the LLM, validate with a script, call out to an MCP
  service.
- **`script`** — pre-process inputs / post-process outputs around
  the LLM call.
- **`human`** — gate an LLM transition behind a human approval
  (compliance pattern).

The `delegate:` sub-agent pattern (SPEC §21) is the existing way to
spawn an isolated agent session for a state. The `LlmExecutor` is the
in-process alternative — same governance, no subprocess. Both stay
available; operators pick per state based on isolation requirements.

### 33.7 Tool-call → transition dispatch contract

The LLM emits a tool call shaped per the JSON Schema we provided:

```json
{
  "tool_call": {
    "name": "mark_as_bug",
    "arguments": { "severity": "high", "labels": ["bug", "p1"] }
  }
}
```

The runtime treats this as `praxec.command({
  workflowId, expectedVersion, transition: "mark_as_bug",
  arguments: { severity: "high", labels: ["bug", "p1"] }
})` — same dispatch path the MCP surface uses. Guards run; blackboard
updates; audit fires; state advances.

If the model emits something that isn't a recognized transition:

- **Unknown tool name** — runtime returns a structured error to the
  model in the next turn: "tool 'X' is not available at this state;
  valid: [...]." This is the model's chance to correct itself.
- **Malformed arguments** — same shape: validation error returned;
  next turn fires.
- **No tool call (final answer instead)** — error: "you must select
  one of [...] transitions to advance; emit a tool call."

Three retries (configurable via `max_iterations`); then the executor
fails with `LLM_EXECUTION_EXHAUSTED` and the workflow's failure path
(if any) takes over.

### 33.8 Audit + cost tracking

Every LLM call emits an audit event:

```json
{
  "event_type": "llm.invocation",
  "workflow_id": "wf_01H...",
  "state": "triaging",
  "model": "anthropic/claude-sonnet-4-6",
  "tokens_in": 1842,
  "tokens_out": 87,
  "latency_ms": 1230,
  "cost_usd": 0.0042,
  "tool_call_emitted": "mark_as_bug"
}
```

Per-workflow cost limits (SPEC §21's `max_sub_agent_seconds` /
`max_sub_agent_steps` already exist; extend with `max_llm_cost_usd` /
`max_llm_tokens` per workflow instance). When a limit is hit, the
workflow transitions to its declared overrun handler (or fails if
none).

### 33.9 Implementation order

1. **Crate skeleton** — `praxec-llm-executor` with `Cargo.toml`,
   `lib.rs`, empty `LlmExecutor` impl returning `Unimplemented`.
   Register under `kind: llm`. Optional cargo feature. Test that the
   feature flag works.
2. **Aether-llm wiring** — implement `LlmExecutor::execute` to
   construct an `aether_llm::Context` from the workflow state + tools,
   call `stream_response`, parse the response, dispatch the tool call.
3. **Transition-as-tool resolution** — for each transition available
   at the current state, generate a `ToolDefinition` from its
   `inputSchema` + name. Guards already exclude rejected transitions
   from the available set.
4. **Loop + termination conditions** — `max_iterations`,
   `max_seconds`, `max_tokens`. Error responses for malformed tool
   calls.
5. **Models.yaml resolution** — reuse the shared resolver in
   `crates/praxec-core/src/model_resolver/` for affinity-based
   model selection.
6. **Audit events** — emit `llm.invocation` per call with token
   counts + latency + cost.
7. **Cost limits** — `max_llm_cost_usd` / `max_llm_tokens` per
   workflow instance; check before each call; transition to overrun
   handler when exceeded.
8. **Examples + integration tests** — at minimum an "issue triager"
   example workflow demonstrating the executor end-to-end against a
   mock LLM provider.
9. **Documentation** — SPEC §33 prose + site reference page
   (`site/src/content/docs/reference/executors.mdx` already lists the
   existing kinds; add `llm`). Update README's executor list.
10. **Reposition** (deliberate, last) — update the README tagline /
    project_scope_boundary memory to acknowledge praxec as a
    governed LLM orchestration platform that also exposes an MCP
    surface. This is the moment we publicly become a different
    product. Land only after the executor is proven in use.

### 33.10 Open questions — locked decisions

Each question below carried into implementation; the resolution sits
next to it. Items marked LOCKED are now part of the v0.6 contract and
won't change without a spec revision.

1. **Streaming output to the operator** — **LOCKED: final-only.** The
   executor captures the full final output plus the chosen tool call
   into the `llm.invocation` audit event. Token-by-token streaming
   was rejected: the runtime process has no attached operator display
   to consume it, and bolting on a streaming audit channel would
   complicate the durable-audit contract that downstream sinks
   already depend on. Operators who need live visibility into a long
   call use the per-turn `latency_ms` + `tokens_*` fields.
2. **Reasoning models** — **LOCKED: captured into audit by default,
   opt-out per workflow.** The audit `reasoning` field carries the
   captured reasoning text when present. Operators with compliance
   constraints set `capture_reasoning: false` on the executor; the
   audit then records the literal sentinel `"<elided>"` in place of
   the captured text so the elision is visible in the audit log
   rather than indistinguishable from "no reasoning emitted." Default
   is capture-on; the opt-out is the explicit privacy lever.
3. **Multi-tool-call turns** — **LOCKED: reject.** When a provider
   emits more than one tool call per turn, the executor surfaces
   `LLM_MULTI_TOOL_CALL` and the turn fails. The dispatch contract
   requires one tool call per turn so guards, audit, and version
   bumps stay one-to-one with transitions. Sequential dispatch of
   multiple calls in a single turn would have required either
   re-entering the runtime mid-turn or batching with after-the-fact
   guard checks — both worse than asking the model to take its next
   turn after the runtime advances.
4. **Idempotency on retry** — **deferred to a future spec revision.**
   The `expectedVersion` check already covers the "state advanced
   between turns" case; the cross-retry dedup story (same tool call
   re-emitted after a transport-layer failure) needs more thought
   than the v0.6 cycle had room for. No data corruption surface in
   the meantime — duplicate dispatches collide on `expectedVersion`
   and fail cleanly.
5. **Cost prediction** — **addressed by D8 catalog freshness gates.**
   The live-side concern (operators set a `max_cost_usd` cap against
   an unknown model and silently bypass budget enforcement) is
   covered by the doctor check: any `kind: llm` executor with
   `max_cost_usd` and a model name absent from the cost catalog —
   or whose catalog entry's `verified_at` is older than 90 days —
   fails workflow load with `COST_CATALOG_MISSING_ENTRY` /
   `COST_CATALOG_STALE`. Soft budget hints at config-load time
   remain a future-spec question; the silent-bypass hole is closed.
6. **MCP surface from inside the executor** — **closed by design.**
   The executor cannot inject `praxec.*` tools into the LLM's tool
   list. The tool surface is exactly the set of available transitions
   plus nothing; the `tools:` config field is rejected at parse time
   via `deny_unknown_fields` (FMECA F3). Operators who want the LLM
   to see praxec's MCP surface use the external-agent path (§32)
   instead. Hosting an LLM inside praxec to then have it call
   praxec from the inside is an antipattern the spec deliberately
   forecloses.

### 33.11 Implementation deviation from §33.2 — runtime drives the loop

The §33.2 design described the executor as the loop driver: receive
the workflow state, call the provider, dispatch the tool call by
re-entering the runtime, loop until terminal. Implementation made one
deliberate architectural change.

**The conflict.** Re-entering `runtime_submit::submit` from inside the
executor would have raced the executor's view of `expected_version`
against the runtime's own version bumps. Each successful tool-call
dispatch advances the workflow; the next iteration of the
executor-driven loop would have to re-fetch state and reconcile —
either by passing `expected_version` through every call (deepening
the surface area between executor and runtime) or by ignoring it
inside re-entrant calls (weakening optimistic concurrency for every
other dispatch path that depends on it). Neither was acceptable.

**The fix: runtime drives the loop.** Each `LlmExecutor::execute()`
call now runs **exactly one turn**: build the tool list from the
current available transitions, call the provider, parse the response,
return an `ExecuteResult` carrying a `NextTransition` (the chosen
transition plus its arguments) on success. The runtime's submit
pipeline notices the `NextTransition`, applies it as a normal
transition dispatch (full guard run, blackboard update, audit fire,
version bump), and — if the new state has another `kind: llm`
executor — re-invokes the executor for the next turn. The chain
terminates when the executor returns no `NextTransition` (the model
emitted a final answer that satisfies the workflow's terminal state),
when the workflow reaches a terminal state, or when the runtime's
`max_chained_llm_turns` cap fires (`LLM_CHAIN_DEPTH_EXCEEDED`).

**What this preserves.** Every transition the model picks travels the
same dispatch path as every other transition in praxec. The audit
log shows one event per transition; `expected_version` checks fire
exactly once per dispatch; guards and blackboard mutations stay
serializable. The optimistic-concurrency invariants the rest of the
runtime relies on never have to make an exception for "the LLM is
calling us from the inside."

**What it changes versus §33.2.** The executor's responsibility
narrowed: it owns prompt rendering, provider invocation, response
parsing, and per-turn cap enforcement (synthetic `_llm.*` slots
written via the post-execute output mapping). The runtime owns turn
chaining, terminal detection, and the global `max_chained_llm_turns`
limit. The split is documented in `ExecuteResult.next_transition`
(executors crate) and `RuntimeTransitionResolver` (core crate);
read those two surfaces together to understand the contract.

Tracked: this section shipped in v0.6.

### 33.12 Models, Skills, and Prompts — the three-slot contract

A `kind: llm` step's model call is assembled from three things, each
already a first-class concept in praxec. Keeping them distinct is what
lets the same skill declaration coach both the orchestrating agent and an
in-runtime model. A **skill (persona) running on a model (engine)** is an
**agent** — the worker; this section defines the three slots that compose
into one.

> **Invariant: a model has no instructions; a skill has no engine.**

| Slot | Source | Nature |
|------|--------|--------|
| **model** | a `provider/model` binding from `models.yaml`, selected by the executor's `affinity:` field (§33.4) | context: *which engine runs*. A model carries no instructions/persona — only the engine choice (and provider feature toggles). |
| **system message** | the **skill(s)** in scope — `skills: [subject]` declared at workflow / state / transition level, resolved from the instance's `_skillsLibrary` (§5) | instructions/persona: *how to do this kind of work*. Static and hash-pinned, so a skill is reusable and `guidance_acknowledged` (§5.8) can detect edits. |
| **user message** | the **`prompt_template`** | the specific task/goal at hand: rendered against the live blackboard (`{{ $.context.* }}` etc.). |

**Refs outward, bodies inward.** The same `skills:` declaration feeds two
different consumers:

- The **orchestrating agent** (the MCP client driving the workflow)
  receives skill **refs** — `{verb, subject, hash}` on `guidance.refs` and
  on each transition link (§5.5) — and fetches a body on demand via
  `praxec.query {subject, workflowId}`. It is interactive, so refs keep
  the response small and let it choose what to read.
- The inner **`kind: llm` agent** receives the skill **bodies** injected
  directly as its system message. It is a one-shot call that cannot "go
  fetch," so the bodies travel with the call.

Scope resolution is shared: `runtime_links::collect_in_scope_skill_subjects`
returns the workflow + state + transition subjects (broad→specific,
de-duped) that both the ref builder and the executor's system-message
builder consume, so the two views cannot drift. A subject declared in scope
but absent from `_skillsLibrary`, or present with an empty body, fails loud
(`LLM_SKILL_SUBJECT_UNKNOWN` / `LLM_SKILL_BODY_MISSING`) rather than
silently dropping the agent's instructions.

**Consequences of the contract:**

- **There is no `kind: skill` executor.** A skill is the instruction layer
  of an `llm` agent, not a standalone step. Declaring `skills:` at scope on
  a `kind: llm` step is the only way a skill reaches a model.
- **Skills are never templated.** Injecting a body verbatim preserves the
  hash `guidance_acknowledged` pins. Run-specific variability belongs in the
  `prompt_template` (the user message); if a skill seems to need a
  parameter, that parameter belongs in the task or the skill should be split.
- **Injection does not satisfy `guidance_acknowledged`.** That guard gates
  the *orchestrating* agent's attestation that it read the guidance;
  injecting bodies into a *different* agent's system message is unrelated.

Tracked: this section shipped in v0.6.
