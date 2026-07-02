# LLM guidance

How to design workflows so a model only has to generate what's actually
unknown — and gets pre-built guidance for everything else.

This doc is for workflow **authors**. The audience for the workflow's
output is the LLM that calls it.

---

## The principle

> Every value the LLM doesn't have to reason about is a value you can
> compute, look up, or set ahead of time.

A naive HATEOAS-inspired link asks the model to read an `inputSchema`
and synthesize every required field. For a `create_pull_request` call
that means reasoning about repo, base branch, head branch, title, and
body — even though *most* of those are deterministic given the workflow's
state.

The aim of this design is the opposite: **maximize the deterministic
preparation** so the model only generates the genuinely-creative
fields. The link arrives pre-shaped; the LLM just fills in the
"actually-LLM-decision" pieces.

The mechanism is the `prefill` block on a transition.

---

## prefill in 30 seconds

```yaml
transitions:
  create_pr:
    target: review
    inputSchema:
      type: object
      required: [repo, base, head, title, body]
      properties: { … }
    prefill:
      repo: "$.workflow.input.repo"      # came in with the workflow
      base: "main"                        # literal, project convention
      head: "$.context.branch_name"       # set by an earlier step
    executor: { kind: mcp, connection: github, tool: create_pull_request }
```

When the runtime renders the link for `create_pr`, it resolves each
prefill value against the workflow's current scopes and embeds the
result under `link.args.arguments`:

```json
{
  "rel": "create_pr",
  "method": "praxec.command",
  "args": {
    "workflowId": "wf_3f8b…",
    "expectedVersion": 4,
    "transition": "create_pr",
    "arguments": {
      "repo": "owner/repo",
      "base": "main",
      "head": "feat/bugfix"
    }
  },
  "inputSchema": {
    "type": "object",
    "required": ["repo", "base", "head", "title", "body"],
    "properties": { … }
  }
}
```

The model takes `args.arguments` as the starting point and only
generates `title` and `body` (the two `required` fields not already
filled in). Then it submits with the full merged `arguments`.

---

## What you can put in prefill

The same expression syntax as output mappings:

| Form                                          | Meaning                                                         |
|-----------------------------------------------|-----------------------------------------------------------------|
| `"$.workflow.input.foo"`                      | Read from the workflow's input.                                 |
| `"$.context.foo"`                             | Read from accumulated context.                                  |
| `"a literal"`                                 | Plain string — used verbatim.                                   |
| `42` / `true` / `null`                        | Plain JSON literal — used verbatim.                             |
| `["one", "two"]` / `{ "k": "v" }`             | Plain literal array / object — used verbatim.                   |
| `{ set: <any value> }`                        | Explicit literal (rarely needed; bare literals work).           |
| `{ add: [a, b] }`, `subtract`, `multiply`, `divide` | Arithmetic; operands may be paths or literals.            |

Resolution happens at **link-generation time**, so you only have access
to `$.workflow.input.*` and `$.context.*` (no arguments, no
executor output). That's intentional: prefill is "what's known about
this workflow right now," not a place to defer computation.

---

## Patterns

### Pattern 1: project conventions are constants

```yaml
prefill:
  base: "main"
  signature: "Co-authored-by: bot <bot@example.com>"
  draft: false
```

The model never has to "remember" your team's PR conventions; the
workflow declares them.

### Pattern 2: thread input through

```yaml
inputSchema:
  type: object
  properties:
    repo: { type: string }

# in a downstream transition:
prefill:
  repo: "$.workflow.input.repo"
```

If the workflow started with `{ repo: "owner/repo" }`, every transition
that needs `repo` gets it without the model re-deriving it.

### Pattern 3: chain through context

When an earlier executor produces a value, stash it in context, then
reference from prefill:

```yaml
states:
  planning:
    transitions:
      submit_plan:
        target: tested
        executor: { kind: mcp, connection: planner, tool: normalize_plan }
        output:
          plan_id:     "$.output.plan_id"
          branch_name: "$.output.branch_name"

  tested:
    transitions:
      create_pr:
        target: review
        prefill:
          plan_id: "$.context.plan_id"
          head:    "$.context.branch_name"
          base:    "main"
        executor: { kind: mcp, connection: github, tool: create_pull_request }
```

The model arrives at `tested`, sees `plan_id`, `head`, and `base`
already filled in, and just generates the LLM-required pieces.

### Pattern 4: derived counters / coordinates

```yaml
prefill:
  attempt:    { add: ["$.context.attempts", 1] }    # incremented for the next call
  next_index: { add: ["$.context.cursor", 10] }
```

The arithmetic is yours; the LLM doesn't have to count.

### Pattern 5: composing per-call branding

A single-path value resolves to its target; a bare string that doesn't
look like `$....` is taken as a literal. For everything in between —
templated messages mixing literals and paths — use the `concat`
operator:

```yaml
prefill:
  comment_body:
    concat:
      - "Auto-generated by workflow "
      - "$.workflow.id"
      - " from change "
      - "$.context.change_id"
      - "."
```

Each element is resolved independently: paths read from
`arguments` / `context` / `workflow.input`, numbers and bools are
stringified, and `null` becomes the literal `"null"` so missing data
is visible rather than silently dropped.

### Pattern 6: presets vs. multiple variants

If you want to offer the model multiple pre-shaped options for the same
underlying action (e.g. "create draft PR" vs "create ready PR"),
declare them as **separate transitions** sharing a capability:

```yaml
capabilities:
  github_create_pr:
    executor: { kind: mcp, connection: github, tool: create_pull_request }

transitions:
  create_draft_pr:
    target: review
    title: Open the PR as a draft
    prefill: { base: "main", draft: true,  head: "$.context.branch" }
    executor: { capability: github_create_pr }

  create_ready_pr:
    target: review
    title: Open the PR ready for review
    prefill: { base: "main", draft: false, head: "$.context.branch" }
    executor: { capability: github_create_pr }
```

Each becomes its own link with its own pre-shaped `args.arguments`. The
model sees a *menu* of pre-built calls and picks the one it wants.

---

## When NOT to prefill

- **Values the LLM should think about.** A `title` that summarizes a
  diff is genuinely an LLM job. Don't prefill it with placeholder text;
  let the schema's `required` list signal "you need to fill this in."
- **Sensitive values that shouldn't appear in tool-list responses.**
  Prefill is visible to the model as part of the link. If you're
  surfacing a token or PII, push it into the executor (via a
  connection's `headers` or `env`) instead.
- **Values that depend on `$.arguments`.** At link-gen time, no caller
  arguments exist. Use schema `default` or in-executor logic instead.

---

## Reading the LLM's behavior

When prefill works, your model's prompt completion should:

1. Read `link.args.arguments` as "already known."
2. Look at `link.inputSchema.required` minus the keys present in
   `args.arguments` — that's the actual work.
3. Generate values for those keys.
4. Construct `praxec.command({ workflowId, expectedVersion, transition, arguments: <merged> })`.

If your model is "burning tokens" thinking about repo names or branch
conventions, the workflow is missing prefill. If your model is generating
fields that are already in `args.arguments` (and getting it wrong), the
prompt template surrounding the gateway needs to call out "use the
prefilled args verbatim and only add what's missing."

---

## Determinism vs. flexibility

Prefill is not a lock. The runtime doesn't enforce that a `submit`
includes the prefilled values — it just validates the final
`arguments` against `inputSchema`. If the LLM overrides a prefilled
value, that's allowed.

If you actually want a value enforced regardless of what the model
sends, two declarative paths:

1. **Drop it from `inputSchema`** so the executor's `map: { … }`
   sources it from `$.context.*` or `$.workflow.input.*` and the
   model can't influence it.
2. **Add an `expr` guard** that checks the field equals the
   workflow's value, e.g.
   `expr: "$.arguments.base == $.workflow.input.base"`. (String
   operations are supported too — `==`, `!=`, `starts_with`, `contains`
   — alongside numeric comparisons.)

For most cases prefill is "guidance," and trusting the model to take
it is fine. For "this value cannot be wrong," go through the executor.

---

## Phase guidance: pre-shape reasoning, not just arguments

Prefill pre-shapes the model's *arguments*. Phase guidance pre-shapes
the model's *reasoning* about what to do at each state.

```yaml
states:
  ready_to_deploy:
    goal: Confirm deployment
    guidance: >
      All automated checks passed. Review the lint report, test
      results, and build artifact in the context before deciding
      to deploy or abort. The context contains lintReport, testCount,
      coverage, and artifactId.
    transitions:
      deploy:
        title: Deploy to environment
        actor: agent
        prefill: { artifact: "$.context.artifactId" }
        executor: { … }
      abort:
        title: Abort deployment
        actor: agent
```

The response includes:

```json
{
  "guidance": {
    "goal": "Confirm deployment",
    "instructions": "All automated checks passed. Review the lint report..."
  },
  "links": [ … ]
}
```

The model reads `guidance.goal` to understand what it should accomplish,
`guidance.instructions` for how to reason about it, and then picks from
the prefilled links. This is especially powerful after a deterministic
chain — the model arrives at a decision point with full context and
clear instructions about what to do with it.

**When to use goal vs. guidance:**

- `goal` alone: simple states where the objective is self-evident
- `goal` + `guidance`: states where the model needs to review context
  or make a non-obvious decision
- Neither: states where the transition titles and descriptions are
  sufficient

Both fields are indexed by the search operation (via `praxec.query`),
so they improve discoverability as well as runtime guidance.

---

## Combining with other knobs

Prefill composes cleanly with everything else in the system:

- **Schema defaults**: defaults apply *after* the model submits, so a
  prefilled value doesn't conflict with a field's `default`. If the
  prefill is missing AND the model omits, the default kicks in.
- **Guards**: a transition's guards run after the model submits. If
  the LLM overrides a prefilled value with something that fails a
  guard, the rejection comes back with `links` for recovery (including
  the same transition again with refreshed prefill).
- **Wraps**: capability wrappers add guards / reliability but don't
  inherit `prefill` — that lives at the transition level. (Transitions
  that share a capability can declare different prefill blocks; see
  Pattern 6.)
- **Audit**: the prefilled link is visible in the response carrier
  events but not specifically logged. The submit's `transition.requested`
  audit event captures whatever the model actually sent.
- **Deterministic chaining**: deterministic transitions don't use
  prefill (they take no arguments from the LLM). Prefill matters on
  the transitions *after* the chain stops — the decision point where
  the model takes over. Combined with phase guidance, the model
  arrives at a decision point with pre-shaped arguments *and*
  pre-shaped reasoning.

---

## Quick checklist

When designing a new workflow with LLM callers:

- [ ] What does the LLM *actually* need to generate? (Title? Body?
      Decision among options?)
- [ ] What does the workflow already know from input + context that
      it can prefill?
- [ ] Are there project / team conventions worth declaring as
      literals in `prefill`? (Base branch, default labels, signature,
      draft mode.)
- [ ] Does each transition's `inputSchema.required` overlap with
      `prefill`? Good — the model only fills the difference.
- [ ] If you're offering multiple variants of the same call (draft vs
      ready, dry-run vs apply), is each a separate transition with its
      own `prefill`?

If all five answers are clean, your workflow is providing maximum
deterministic preparation and your model is thinking about the right
things.
