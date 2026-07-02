# Example: expense reimbursement with two-tier approval

The **multi-tenant** counterpart to `content-publish`. Where content
publishing has one human role (the approver), this example has two —
manager and finance — so `permission` and `role` guards become
load-bearing. To run end-to-end you need identity wired into the
gateway; with the bundled binary's anonymous principal, every
approval rejects.

## Why this example

Most realistic governed processes have **multiple distinct
principals** with different authority. A sales rep submits; their
manager approves; finance signs off above a threshold; sometimes a
quorum of two finance approvers is required. This is the deployment
shape MCP-CONTROL-ARCHITECTURE.md describes: a team / project / org
gateway over Streamable HTTP, where different humans share one
gateway and the principal carries authority.

## What this shows

- **`permission` guards in their natural habitat** — `expense.approve.manager`
  and `expense.approve.finance` are real role-distinct permissions
  that gate two different transitions.
- **Branching by classifier output** — high-value claims route through
  `finance_review`; low-value route directly to reimbursement. The
  classifier's verdict (a context value) drives the routing.
- **Quorum evidence** — very-high-value claims (`requiresQuorum: true`)
  require *two* finance approvals before advancing. The
  `requires: [{ kind: human_request, count: 2 }]` form expresses the
  quorum declaratively.
- **Idempotent payment** — payroll is called with `idempotencyKey: true`,
  so retrying a timed-out call doesn't double-pay.
- **`linkFilter: byGuards`** — at every state, the model only sees the
  transitions whose guards currently pass. No guessing legal moves.

## How it works

```text
   submitted   (onEnter: classify against policy)
     │ to_manager_review        (guard: policyOk == true)
     │     → manager_review
     │ policy_reject            (guard: policyOk == false; executor: notifier)
     │     → rejected
     ▼
   manager_review   (linkFilter: byGuards)
     │ manager_approve_to_finance   (perm + requiresFinance==true; executor: human)
     │     → finance_review
     │ manager_approve_low_value    (perm + requiresFinance==false; executor: human)
     │     → reimbursement
     │ manager_reject               (perm; executor: notifier)
     │     → rejected
     ▼
   finance_review   (linkFilter: byGuards)
     │ finance_approve              (perm + requiresQuorum==false; executor: human)
     │     → reimbursement
     │ finance_approve_quorum       (perm + requiresQuorum==true; executor: human)
     │     → finance_review        (loops; awaiting second approval)
     │ finance_quorum_complete      (evidence: 2x human_request + requiresQuorum)
     │     → reimbursement
     │ finance_reject               (perm; executor: notifier)
     │     → rejected
     ▼
   reimbursement   (onEnter: idempotent payroll POST + retry)
     │ mark_paid   → paid
     ▼
   paid (terminal)    rejected (terminal)
```

## Failure modes and what catches them

| What an agent might try                             | How the workflow catches it                                                          |
|-----------------------------------------------------|--------------------------------------------------------------------------------------|
| Self-approve the expense                            | `manager_approve_*` transitions need `permission: expense.approve.manager`           |
| Skip finance review on a high-value claim           | `manager_approve_low_value` requires `requiresFinance == false`; the alternative path goes through `finance_review` |
| Single-approve a quorum-required claim              | `finance_approve` requires `requiresQuorum == false`; the quorum path needs evidence of two `human_request` records |
| Pretend the classifier said it's low-value          | The classifier's output is written into context by the workflow, not the LLM         |
| Retry a flaky payroll call and double-pay           | `idempotencyKey: true` — payroll dedupes on the key                                  |
| Bypass the workflow and call payroll directly       | **Deployment concern** — payroll must only be reachable through the gateway          |

## Identity wiring (required for this example)

`permission` guards check `principal.permissions` against the guard's
`permission:` field. The bundled `PraxecServer` always presents
`Principal::anonymous()`, which has no permissions, so every approval
guard rejects. To run this example end-to-end, you need to:

1. Build a custom `ServerHandler` (per [`docs/guides/embeddings.md §8d`](../../docs/guides/embeddings.md#8d-identity-wiring-principal-into-a-custom-server-surface))
   that sources the principal from your transport.
2. Populate `Principal { subject, roles, permissions }` from your
   identity provider — JWT claims, mTLS subject, an upstream-injected
   header from a trusted proxy, etc.
3. Pass the principal to every `runtime.start` / `runtime.submit`
   call.

A common arrangement: identity terminates at an enterprise SSO layer,
each lower gateway forwards the verified identity through a header,
and only the outermost gateway does identity work. The
expense-approval gateway then trusts the principal that arrives.

## Running the example

The connections are placeholders. To validate the shape:

```bash
praxec check --config examples/expense-approval/gateway.yaml
```

To run mechanically with stand-ins, swap connections for `kind: noop`
executors. Without identity wired, you can still walk the
`policy_reject` branch (no permission guard) and observe audit
events. The approval branches will reject with `GUARD_REJECTED` —
that's the expected behavior on the bundled binary.

## Where to read more

- [`../../docs/architecture/mcp-control-architecture.md`](../../docs/architecture/mcp-control-architecture.md)
  — multi-tenant deployment patterns and the identity discussion
- [`../../docs/guides/embeddings.md`](../../docs/guides/embeddings.md#8d-identity-wiring-principal-into-a-custom-server-surface)
  — concrete `Principal` sourcing recipes
- [`../../docs/reference/governance.md`](../../docs/reference/governance.md) — guards,
  quorum evidence, branches
- [`../content-publish/`](../content-publish/) — single-human
  governance counterpart
- [`../tdd/`](../tdd/) — coding-discipline counterpart
