# MCP Control Architecture

A guide for engineers designing MCP gateways for **maximum reusability,
maintainability, and ergonomics**.

This document is task-shaped: it teaches you how to think about the building
blocks, when to reach for which one, and how to compose them into systems
that grow without becoming a tangle.

If the README answers "what is praxec?", this doc answers "how do I
**design** with it?".

---

## Table of contents

1. [The composition model in one diagram](#1-the-composition-model-in-one-diagram)
2. [The trichotomy: capability, exposure, workflow](#2-the-trichotomy-capability-exposure-workflow)
3. [Choosing the right primitive](#3-choosing-the-right-primitive)
4. [Capability wrappers: layering policy](#4-capability-wrappers-layering-policy)
5. [Micro-workflows and how to nest them](#5-micro-workflows-and-how-to-nest-them)
6. [Constraints that earn reusability](#6-constraints-that-earn-reusability)
7. [Hierarchical gateway pattern](#7-hierarchical-gateway-pattern)
8. [Multi-file config: the include layer cake](#8-multi-file-config-the-include-layer-cake)
9. [Anti-patterns and how to avoid them](#9-anti-patterns-and-how-to-avoid-them)
10. [A worked example: from one tool to a governed enterprise control plane](#10-a-worked-example)

---

## 1. The composition model in one diagram

```
                    upstream MCP host (model, IDE, agent runner)
                                          │
                                          │  speaks MCP
                                          ▼
        ┌─────────────────────────────────────────────────────────┐
        │                       this gateway                       │
        │  ───────────────────────────────────────────────────     │
        │  exposes 2 stable MCP tools (praxec.query + .command)  │
        │                                                          │
        │  workflows ────────── governance: when can it run?       │
        │      │                                                    │
        │      ▼                                                    │
        │  exposures ──────────  what's discoverable & callable?    │
        │      │                                                    │
        │      ▼                                                    │
        │  capabilities ───────  what does the gateway know how to do? │
        │      │                                                    │
        │      ▼                                                    │
        │  connections ────────  where does the gateway reach to?   │
        │                                                          │
        └────────────────────────┬─────────────────────────────────┘
                                  │
                                  │  speaks MCP / CLI / REST / HTTP-MCP
                                  ▼
              another gateway, an MCP server, a CLI, an HTTP service…
```

**Every gateway is both an MCP server (upward) and an MCP/CLI/REST client
(downward).** That symmetry is what makes them stack.

---

## 2. The trichotomy: capability, exposure, workflow

Three different concepts that often get conflated. Keeping them distinct is
the single biggest leverage point in your config.

| Concept        | Question it answers                                  | Where it lives                              |
|----------------|------------------------------------------------------|---------------------------------------------|
| **Capability** | *What can this gateway do?*                          | `capabilities:` block — defined once, used many times |
| **Exposure**   | *What does this gateway publish to its callers?*     | `proxy.expose:` array                       |
| **Workflow**   | *Under what state and conditions can capabilities run?* | `workflows:` block                       |

### Capability — the thing

A capability is a named, reusable description of something the gateway can
do. It bundles:

- the **executor** (how to actually do it: `mcp` call, `cli` invocation,
  `rest` request, etc.)
- an **inputSchema** (what arguments it accepts)
- intrinsic **guards** that should always run before this thing can fire
- intrinsic **reliability** (timeout / retry / fallback) that's a property
  of the thing itself, not the calling context
- metadata: title, description, tags, examples

```yaml
capabilities:
  github.create_pr:
    title: Create GitHub PR
    description: Open a pull request on GitHub.
    tags: [github, write]
    inputSchema:
      type: object
      required: [title, head, base]
      properties:
        title: { type: string }
        head:  { type: string }
        base:  { type: string }
    executor:
      kind: mcp
      connection: github
      tool: create_pull_request
    guards:
      - { kind: permission, permission: github.write }
    reliability:
      timeoutMs: 30000
      retry: { maxAttempts: 3, backoff: exponential, initialDelayMs: 500 }
```

### Exposure — making it discoverable

An exposure publishes a capability to upstream callers. It can be:

- **inline** (legacy form, still valid): redeclare everything
- **by reference**: name a capability and optionally alias it

```yaml
proxy:
  expose:
    # By reference — recommended.
    - capability: github.create_pr

    # Aliased — same capability, different name in this gateway's surface.
    - capability: github.create_pr
      as: pr.create
      tags: [primary]

    # Inline — fine for one-offs.
    - name: hello.echo
      executor: { kind: noop }
```

`praxec.query({ query: "…" })` finds these; `praxec.command({ definitionId: "proxy_default" })` runs them.

### Workflow — when may it run

A workflow is a state machine. Each transition uses a capability (by
reference or inline) and adds **transition-specific** guards / output
mapping / reliability.

```yaml
workflows:
  safe_pr:
    initialState: tested
    states:
      tested:
        transitions:
          create_pr:
            target: review
            guards:
              - { kind: evidence, requires: [tests_passed, security_scanned] }
            executor: { capability: github.create_pr }
      review:
        transitions:
          merge:
            actor: human
            target: merged
      merged:
        terminal: true
```

The workflow doesn't redefine what `github.create_pr` does — it controls
*when* it can fire and what extra rules apply for this use.

---

## 3. Choosing the right primitive

Every time you're about to add something to your config, ask:

> *Is this about what the gateway can do, what it publishes, or when it's
> allowed to run?*

| If the answer is...                                                | Reach for...           |
|--------------------------------------------------------------------|------------------------|
| "It's a thing the gateway should know how to do."                  | `capabilities.<name>`  |
| "Callers should be able to discover and run it directly."          | `proxy.expose`         |
| "It should only fire after some other steps, with guards / state." | `workflows.<id>`       |
| "It needs different policy in different contexts."                 | Multiple **wrappers** of one base capability — see §4 |
| "It needs a specific sequence of internal steps to be useful."     | A **micro-workflow** — see §5 |

A common mistake: putting policy on a capability that should sometimes run
without it, then having to fork the capability when one workflow needs it
without that policy. Capability = invariant facts. Policy that varies by use
goes on the wrapper or the transition.

---

## 4. Capability wrappers: layering policy

`wraps:` on a capability inherits the executor (and metadata, and guards,
and reliability) and **adds** more. Use it when the same underlying action
needs different governance in different contexts.

### Pattern: raw → wrapped → exposed/used

```yaml
capabilities:
  raw.create_pr:
    executor:
      kind: mcp
      connection: github
      tool: create_pull_request

  safe.create_pr:
    wraps: raw.create_pr
    guards:
      - { kind: evidence, requires: [tests_passed] }
    reliability:
      retry: { maxAttempts: 3 }

  audited.create_pr:
    wraps: safe.create_pr
    guards:
      - { kind: permission, permission: github.write }

proxy:
  expose:
    # Different teams get different wrapper levels.
    - capability: audited.create_pr   # full policy stack
      as: pr.create.audited
    - capability: safe.create_pr      # tests-required only
      as: pr.create.safe
    - capability: raw.create_pr       # no governance — admin-only
      as: pr.create.raw
      guards:
        - { kind: role, role: admin }
```

### Stacking semantics

When `safe.create_pr` is used through any path (exposure or workflow), the
guards run in order: `raw.create_pr.guards` → `safe.create_pr.guards` →
exposure/transition guards. **All must pass.**

Reliability uses "more specific wins": transition > wrapper > base. Set it
once at the level where the policy actually lives.

### When NOT to wrap

If your "wrapper" is just a single transition's local guard, don't make a
wrapper — put the guard on the transition. Wrappers earn their keep when
the same composition is needed in **multiple** places.

---

## 5. Micro-workflows and how to nest them

A workflow doesn't have to be the macro-narrative ("a code change is
planned, reviewed, approved, deployed, verified"). Workflows can be small —
one or two transitions that bundle a tiny invariant.

### Pattern: a workflow whose only job is to bundle preconditions

```yaml
workflows:
  with_artifact_lock:
    description: Acquire a build artifact lock before doing X, release after.
    initialState: free
    states:
      free:
        transitions:
          acquire:
            target: held
            executor: { kind: cli, connection: lockd, args: [acquire, "$.arguments.key"] }
      held:
        transitions:
          release:
            target: free
            executor: { kind: cli, connection: lockd, args: [release, "$.arguments.key"] }
```

This is reusable infrastructure. Other workflows can call into it via a
top-level `praxec.command({ definitionId: "with_artifact_lock" })` from a
human or orchestration layer.

### Pattern: nesting via capability ref + a sub-state

When you need composition *within* one workflow, model the sub-flow as a
sequence of states inside the same definition. Each transition uses a
shared capability:

```yaml
workflows:
  governed_change:
    initialState: planning
    states:
      planning:
        transitions:
          submit_plan:
            target: tested
            executor: { capability: planner.normalize }

      tested:
        transitions:
          create_pr:
            target: reviewing
            guards: [{ kind: evidence, requires: [tests_passed] }]
            executor: { capability: github.create_pr }

      reviewing:
        transitions:
          merge:
            actor: human
            target: deployed
            guards: [{ kind: permission, permission: change.approve }]

      deployed:
        terminal: true
```

The capabilities are reused; the workflow contributes the **ordering and
preconditions**.

### Pattern: cross-gateway nesting

To break apart a large workflow across gateways:

1. Inner gateway publishes the inner workflow at its `proxy_default`
   surface (or as a named workflow).
2. Outer gateway has a `kind: mcp` connection pointing at the inner
   gateway's URL.
3. Outer gateway's executor calls `praxec.command` (submit form) against
   the inner gateway's stable tool surface — the inner workflow runs to completion
   and the outer gateway sees the final result.

This is "nested workflows" without any new abstraction: each gateway
exposes the two stable tools — `praxec.query` (reads) and
`praxec.command` (writes) — so any gateway can drive any other
gateway's workflows.

### Pattern: native sub-workflow via `workflow` executor

With the `workflow` executor kind, a workflow can spawn a sub-workflow
declaratively without needing a second gateway:

```yaml
workflows:
  deploy_with_lock:
    initialState: acquire_lock
    states:
      acquire_lock:
        transitions:
          start:
            target: deploying
            executor:
              kind: workflow
              definitionId: with_artifact_lock
              input:
                artifact: "$.context.artifact_name"
                owner: "$.workflow.input.user"
              timeoutMs: 60000
      deploying:
        transitions:
          deploy:
            target: release_lock
            executor: { kind: cli, connection: deployer, args: ["deploy", "$.context.artifact_name"] }
      release_lock:
        transitions:
          release:
            target: done
            executor:
              kind: workflow
              definitionId: with_artifact_lock
              input:
                artifact: "$.context.artifact_name"
                owner: "$.workflow.input.user"
      done:
        terminal: true
```

The `workflow` executor:
1. Calls `praxec.command` (start form) internally with the given
   `definitionId` and `input` (path expressions resolved against the
   parent's context and arguments).
2. Polls `praxec.query({ workflowId: … })` until the sub-workflow reaches
   a terminal state.
3. Returns the sub-workflow's final `context` as `ExecuteResult.output`.
4. Emits `sub_workflow.started`, `sub_workflow.completed` (or
   `sub_workflow.failed`) audit events.
5. If the sub-workflow times out, returns `ExecutorError::Timeout`.

This enables the "acquire lock → deploy → release lock" pattern as
composable, governed workflows — each stage is its own workflow with
its own guards, timeouts, and audit trail.

**Recursion safety.** A sub-workflow can itself use a `workflow`
executor. To prevent infinite recursion, a maximum depth of 10 is
enforced (tracked via a counter in the executor metadata). Exceeding
the limit returns `ExecutorError::Permanent("max workflow depth
exceeded")`.

**Principal.** The sub-workflow runs as `Principal::anonymous()`.
This is safe for multi-tenant deployments: `permission`/`role` guards
in the sub-workflow always fail, so the sub-workflow can only use
`expr` and `evidence` guards. If single-user deployments need to
inherit the parent's principal, this can be made configurable in a
future iteration.

---

## 6. Constraints that earn reusability

The conventions below are what make composability pleasant in practice.

### 6.1 Capabilities are pure functions over arguments + connection state

A capability should not assume anything about what state a workflow is in.
It should be runnable from any context that has the right inputs. That's
what makes the same `github.create_pr` useful from a `praxec.command`
start (proxy mode) and from a five-state governance workflow.

> Rule of thumb: if a capability needs to know what state it's in, that
> belongs in a workflow transition, not in the capability.

### 6.2 Names are namespaced; namespaces mirror ownership

```yaml
capabilities:
  raw.github.create_pr:        # raw.<vendor>.<tool>
  team.safe.github.create_pr:  # team.<policy>.<vendor>.<tool>
  proj.deploy.production:      # proj.<area>.<verb>
```

When you find yourself reading a name and wondering "is this the wrapped
version or the raw one?", the namespace was wrong.

### 6.3 Inline executors are a smell at config size > ~10

For a tiny config, inline is fine. As soon as you're declaring the same
executor in two places, hoist it into `capabilities:` and reference it.
Diff hygiene gets dramatically better — changing an MCP tool name now
happens in one place.

### 6.4 Guards belong where the rule is invariant

- Always-required-for-this-action → **capability** guard.
- Always-required-when-this-policy-is-on → **wrapper** guard.
- Required-only-when-going-from-state-A-to-B → **transition** guard.

If you're tempted to add guards everywhere "to be safe", you'll end up
duplicating policy and forking on small differences.

### 6.5 Reliability lives where the failure mode lives

If a downstream service is flaky, that's a property of the **capability**.
If a particular workflow step needs a tighter timeout for UX reasons,
that's a property of the **transition**. Put each at its right level and
the wrapper layer can stay clean.

### 6.6 Discovery names should be searchable

The search operation (`praxec.query({ query: "…" })`) lexically scores against title, id, tags, description, and
indexed text. The biggest signal is **title**. The biggest cost is
overlapping titles across capabilities. Aim for: short, memorable title;
descriptive description; comma-separated tags that name the domain.

```yaml
capabilities:
  github.create_pr:
    title: Create GitHub PR              # short, memorable
    description: Open a pull request on a GitHub repo.   # what + where
    tags: [github, source-control, write, pr]            # domain dimensions
    examples: ["create a PR for branch feat/x"]          # phrase a human might say
```

---

## 7. Hierarchical gateway pattern

The most powerful composition pattern: **gateways stack**. Each layer adds
exactly the governance it owns and delegates everything else.

```
   enterprise gateway
        ├─ identity / SSO / SIEM export / global rate-limit
        ▼
   team gateway
        ├─ team RBAC / team audit / approval queues
        ▼
   project gateway
        ├─ project workflows / resource locks / scoped capabilities
        ▼
   local-dev gateway
        ├─ filesystem / git / dotnet / cargo / npm
        ▼
   actual MCP servers and CLIs
```

### How to design a layer

For each layer, answer:

1. **What governance does THIS layer own?** (Just that. Nothing else.)
2. **What does it pass through unchanged?** (Most things.)
3. **What's the outbound connection?** (URL to the next layer down.)
4. **What's the upward surface?** (Always: the two stable tools — `praxec.query` read + `praxec.command` write.)

A "team gateway" config might look like:

```yaml
version: "1.0.0"

include:
  - team.connections.yaml   # connection to project gateway
  - team.audit.yaml         # team-wide audit policy

connections:
  project:
    kind: mcp
    url: http://project-gateway.team.svc/mcp

# Team-level capability wrappers add team policy on top of whatever the
# project gateway exposes.
capabilities:
  team.review_required.github.create_pr:
    wraps: project.github.create_pr      # imported via proxy.import below
    guards:
      - { kind: evidence, requires: [reviewed] }
    audit:
      level: full

proxy:
  import:
    - connection: project
      prefix: project

  expose:
    - capability: team.review_required.github.create_pr
      as: github.create_pr
```

### Why this works

Each layer's config is small because it's only doing **its own job**. The
layer below already enforces everything below it. The two-tool stable
surface means upstream layers don't care how deep the stack is — calling
`praxec.command` (submit form) to the team gateway hits the team gateway, which uses
its connection to the project gateway, etc.

### Identity in multi-tenant deployments

The hierarchical pattern only earns its keep when **different humans
drive the same gateway** over a shared transport — typically Streamable
HTTP. That's where `permission` / `role` guards become load-bearing,
because "Alice can deploy prod, Bob can't" is the actual policy
question.

The bundled `PraxecServer` treats every caller as
`Principal::anonymous()`. For local single-user setups (one human
driving one MCP host that talks to a stdio gateway) this is the right
default — the OS user is the principal, and the guards you actually
want there (`evidence`, `human`, `expr`, `inputSchema`) work
without identity. **Permission and role guards exist for the
multi-tenant case described in this section, not for laptop use.**

To make `permission` / `role` enforceable in a multi-tenant
deployment, build a custom `ServerHandler` that sources a populated
`Principal` from your transport's authenticated identity (verified
JWT, mTLS subject, mutually-authenticated session, upstream-injected
header). See
[../guides/embeddings.md §8d](../guides/embeddings.md#8d-identity-wiring-principal-into-a-custom-server-surface)
for four concrete patterns and the warning about model-asserted
identity.

A common arrangement: identity terminates at the **enterprise**
layer (SSO + JWT minting), each lower layer forwards the verified
identity through a standard header, and only the enterprise layer
does actual identity work. Lower layers just propagate the principal
into runtime calls. This keeps the team / project / local layers
identity-agnostic — they trust the principal that arrives.

---

## 8. Multi-file config: the include layer cake

A gateway can grow past a single file. Use `include:` to compose
responsibility-shaped slices:

```yaml
# gateway.yaml — the entry point
version: "1.0.0"
include:
  - base.connections.yaml      # what we can reach
  - team.policy.yaml           # baseline team policy (audit, RBAC roles)
  - project.proxy.yaml         # project-specific imports & exposures
  - workflow.safe-pr.yaml      # one workflow per file
  - workflow.deploy.yaml
```

### Merge semantics

- **Maps** merge recursively. Later wins on key collisions.
- **Arrays** concatenate. (`proxy.expose`, `proxy.import`,
  `workflows.x.states.y.transitions.z.guards`, etc.)
- **Scalars**: later wins.

The order of operations is: includes are loaded in declaration order, then
the main file's body merges on top — meaning the main file is the **last
writer** and can override anything from the includes.

### When to split

A good split has each file owning one concern:

- one file per **workflow**
- one file per **bounded context** (auth, billing, CI, …)
- one file per **environment** (with `include:` chains: `prod.yaml`
  includes `base.yaml` and adds prod-specific overrides)

Not great splits: by alphabet, by date, by who-wrote-it.

---

## 9. Anti-patterns and how to avoid them

### "I'll inline this just this once"

You won't. Hoist into `capabilities:` from the start.

### Ten capabilities that all wrap a single base

If you have `safe.X`, `audited.X`, `team.X`, `prod.X`, `secure.X` — that's
not five capabilities, it's one capability with five different policy
contexts. Often the simpler shape is **one base capability + a single
wrapper per environment** that you switch via different `include:` files.

### Putting workflow-specific guards on a capability

If `safe.create_pr` requires `tests_passed`, but `governed_change.tested.create_pr`
*also* needs to require `tests_passed` — you've doubled the rule. Decide
which level owns the rule:

- if "you can never call this without tests_passed", it's a capability
  guard.
- if "this particular workflow step requires it", it's a transition guard.

Pick one. Re-using is great; duplicating is debt.

### A workflow that's just a long list of one-state transitions

If your workflow has one state with twenty transitions and they don't
relate to each other, that's actually a bunch of capabilities, not a
workflow. Move them to `proxy.expose` or split the workflow by domain.

### Rebuilding what `praxec.query` already does

Don't add tools to enumerate or describe capabilities. The two-tool
surface is stable on purpose. If a model needs to learn about your
capabilities, that's `praxec.query({ query: "…" })` (search) and
`praxec.query({ subject: "…" })` (describe). If the model
needs to find tools you don't have, that's a config problem, not an MCP
problem.

### Per-vendor "runtime" abstractions

The gateway doesn't know about Docker, Podman, npx, or uvx. They're all
just `(command, args, env)`. Don't introduce schema fields like
`runtime: docker, image: foo`. The point is that the substrate is uniform
because it's compositional.

---

## 10. A worked example

Start: one tool, no governance.

```yaml
# gateway.yaml — v1
version: "1.0.0"
connections:
  github:
    kind: mcp
    command: github-mcp-server
proxy:
  expose:
    - name: github.create_pr
      executor:
        kind: mcp
        connection: github
        tool: create_pull_request
```

A model calls `praxec.query({ query: "create pr" })`, finds it, calls
`praxec.command({ definitionId: "proxy_default" })`, then
`praxec.command({ workflowId: …, transition: "github.create_pr", expectedVersion: …, … })`.
Done.

### Add: tests-must-pass policy

Hoist into a capability + a wrapper:

```yaml
# v2
capabilities:
  raw.github.create_pr:
    executor: { kind: mcp, connection: github, tool: create_pull_request }
  safe.github.create_pr:
    wraps: raw.github.create_pr
    guards: [{ kind: evidence, requires: [tests_passed] }]

proxy:
  expose:
    - capability: safe.github.create_pr
      as: github.create_pr
```

The exposure name didn't change, so callers don't notice. The new policy
applies on the next call.

### Add: a multi-step approved-change workflow

```yaml
# v3
workflows:
  governed_change:
    initialState: planning
    states:
      planning:
        transitions:
          submit_plan:
            target: tested
            executor: { capability: planner.normalize }
      tested:
        transitions:
          create_pr:
            target: reviewing
            executor: { capability: safe.github.create_pr }
      reviewing:
        transitions:
          merge:
            actor: human
            target: merged
            guards: [{ kind: permission, permission: change.approve }]
      merged:
        terminal: true
```

The capability is reused; the workflow contributes ordering. No tool name
changed for callers; they just use a different `definitionId` to opt into
the governed flow.

### Split into files

```yaml
# gateway.yaml
include:
  - connections.yaml
  - capabilities.yaml
  - proxy.yaml
  - workflows/governed-change.yaml
```

Each concern in its own file. New workflows are new files; new exposures
edit `proxy.yaml`; new capabilities edit `capabilities.yaml`.

### Stack with a team gateway

A team gateway runs at `team-gateway.svc:8000`. It imports the project
gateway and adds team-level audit + a stricter wrapper:

```yaml
# team-gateway.yaml
version: "1.0.0"

connections:
  project:
    kind: mcp
    url: http://project-gateway.svc:8000/mcp

proxy:
  import:
    - connection: project
      prefix: project

capabilities:
  team.create_pr:
    wraps: project.github.create_pr   # imported with prefix
    guards: [{ kind: evidence, requires: [security_scanned] }]

audit:
  sink: file
  path: /var/log/team-gateway/audit.jsonl
```

Six lines of meaningful code. Everything else came from the gateway below
it. **That is the architecture.**

---

## A note on expression languages

You'll notice the guard / output mapping / prefill expression syntax
is small: `$.scope.path` references, basic comparisons, a handful of
operator objects (`add`, `subtract`, `multiply`, `divide`, `set`).
That's deliberate.

It's tempting to swap in a full expression language — [CEL](https://github.com/google/cel-spec)
is the obvious candidate — and let workflow authors write
`ctx.tests_passed && ctx.coverage > 80` instead of two guard lines.
We didn't, for four reasons:

1. **Declarative purity.** A YAML map is data; a CEL expression is
   code embedded in data. The whole system's value is being
   inspectable, diff-able, and reasonable about. CEL erodes that.
2. **Validation cost.** Our mini-DSL can be statically scanned at
   config load (do these `$.scope.path` references point at known
   scopes? are these operator objects shaped right?) without a
   parser. CEL needs an interpreter and a typed schema for
   meaningful checks.
3. **Audit clarity.** Audit events carry the literal expression. A
   single CEL line that does work spread across operators is harder
   to reason about than two small `expr` guards that document
   themselves.
4. **Smuggling risk.** Once expressions can do real work, application
   logic creeps from executors (where it belongs) into config
   (where it shouldn't).

When the mini-DSL hits a wall, the right answer is *more declarative
primitives*, not a more powerful expression language. Auto-branches,
the operator object form for output mappings, and string + bool
comparison in `expr` guards are all examples — each closes a real gap
without expanding the syntax surface.

If you genuinely need a CEL-shaped capability — say, regex matching
on context strings — the escape hatch is `GuardEvaluator`. Implement
the trait, register `kind: cel` (or whatever) on top of the default
evaluator, and your config can use it. The runtime doesn't care; the
default surface stays simple.

---

## In one sentence

> Capabilities define what's possible; exposures publish what's discoverable;
> workflows govern what's allowed; gateways stack so each layer owns one
> concern.
