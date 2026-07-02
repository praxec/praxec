# Example: governed content publishing

A non-coding example. An LLM helps draft, brand-review, get approval,
and publish a piece of content — and **cannot skip any step**, because
the workflow's links are the only legal next moves.

## Why this example

The TDD example pressure-tests cheat detection in a coding flow. This
one shows the same primitives applied to a use case that's purely
about **governance over tool calls** — no code generation, no
file-editing visibility issues. Every action the LLM takes here goes
through the gateway, so the gates are airtight (modulo the deployment
constraint that the LLM has only this gateway as its tool source —
see [`docs/architecture/mcp-control-architecture.md`](../../docs/architecture/mcp-control-architecture.md)).

Everyone understands content publishing — draft, review, approve,
publish — so this is also the example to show non-engineers when
explaining what the gateway is for.

## What this shows

- **Branching by expr guard** — `brand_reviewed` exposes either
  `revise_draft` or `request_approval` depending on whether the brand
  check passed. With `linkFilter: byGuards`, the model only sees the
  path that's currently legal.
- **Human-in-the-loop** — the `human` executor for `request_approval`
  records a pending approval in the `content-approvals` queue and
  emits `human.approval.requested`. The LLM has no path forward; only
  a human can advance the workflow.
- **Evidence-guarded action** — `publish` requires a `human_request`
  evidence record, so even if state ordering were tampered with, the
  publish path can't fire without a logged approval request.
- **Idempotent + retried publish** — the CMS call uses an idempotency
  key and is retried with exponential backoff on transient errors.
- **Heterogeneous executors composed** — REST (docs, CMS), MCP (brand
  review), human (approval), all behind one workflow.

## How it works

```text
   idea
     │ create_outline       (executor: brand-checker MCP → outline)
     ▼
   outlined
     │ write_draft          (executor: REST POST /documents → documentId)
     ▼
   drafted
     │ run_brand_review     (executor: brand-checker MCP → passed, notes)
     ▼
   brand_reviewed   (linkFilter: byGuards)
     │ revise_draft         (guard: brandPassed == false)
     │     → drafted
     │ request_approval     (guard: brandPassed == true; executor: human queue)
     │     → awaiting_approval
     ▼
   awaiting_approval
     │ approve              (actor: human)
     │     → approved
     │ request_changes      (actor: human)
     │     → drafted
     ▼
   approved
     │ publish              (guard: evidence[human_request]; executor: REST + retry)
     ▼
   published   (terminal)
```

## Failure modes and what catches them

| What an agent might try                                | How the workflow catches it                                                  |
|--------------------------------------------------------|------------------------------------------------------------------------------|
| Skip brand review                                      | `drafted` only links to `run_brand_review` — no other path                   |
| Approve its own piece                                  | `approve` requires `actor: human`; the LLM can't take human transitions     |
| Publish without approval                               | `published` is only reachable from `approved`, and `publish` requires evidence|
| Publish after rejection                                | `request_changes` routes back to `drafted` — there's no edge from there to publish |
| Retry a flaky publish twice and create a duplicate     | `idempotencyKey: true` — the CMS sees the same key on every retry           |
| Lie about the brand-review result                      | The brand-checker MCP is the source of truth; the LLM doesn't fill that field|
| Use an out-of-band tool to publish directly            | **Deployment concern** — see Hardening below                                 |

The first six are caught declaratively. The seventh is a deployment
requirement.

## Hardening (tool gating)

Same pattern as the TDD example: the gateway can only enforce
discipline on actions the LLM takes through it. For full enforcement,
configure the MCP host (Claude Desktop, IDE, etc.) so that this
gateway is its **only** tool source — file-editing, document storage,
and CMS access all live behind gateway transitions, not as
independent MCP servers.

If the LLM has a separate `cms.publish` tool, all bets are off. If it
only has `gateway.*` and `workflow.*`, the workflow's gates are
absolute.

## Running the example

The connections in `gateway.yaml` are placeholders — there's no
public `docs.example.com` or `brand-checker-mcp`. To run end-to-end
you'd swap them for your real systems. To validate the shape:

```bash
praxec check --config examples/content-publish/gateway.yaml
```

Expected:

```text
config: examples/content-publish/gateway.yaml
workflows (1):
  - content_publish
```

To run mechanically with stand-ins, replace `kind: mcp` and
`kind: rest` executors with `kind: noop` (returns `{}`) and walk the
states with an MCP inspector or a JSON-RPC stdio client. Useful for
exercising the state machine without the downstream services.

## Resolving approvals

When the workflow reaches `awaiting_approval`, the LLM stops. A human
resolves the approval using the `praxec approvals` subcommand:

```bash
# See what's waiting
praxec approvals list --config examples/content-publish/gateway.yaml

# Approve
praxec approvals resolve --config examples/content-publish/gateway.yaml \
  --id <event-id> --outcome approved

# Or reject with feedback
praxec approvals resolve --config examples/content-publish/gateway.yaml \
  --id <event-id> --outcome rejected
```

After resolution, the next `workflow.get` call returns links for the
new state. The approval event and its resolution are both recorded in
the audit log for a complete trail.

## Where to read more

- [`../../docs/architecture/concepts.md`](../../docs/architecture/concepts.md) — the mental model
- [`../../docs/reference/governance.md`](../../docs/reference/governance.md) — guards,
  evidence, branches, prefill
- [`../../docs/architecture/mcp-control-architecture.md`](../../docs/architecture/mcp-control-architecture.md)
  — composing this with team / project gateways
- [`../tdd/`](../tdd/) — coding-discipline counterpart (with caveats
  about agent navigation)
- [`../expense-approval/`](../expense-approval/) — multi-tenant
  governance counterpart
