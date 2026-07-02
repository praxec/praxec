# Pattern: Evidence Quorum

SPEC §20. Gate a transition on the presence of N evidence records of a
given kind. Used for quorum-based approvals, verifier artifact requirements,
or any multi-source attestation.

## The pattern

```text
┌─────────────┐
│ collect_a   │──→ writes evidence[kind: report_a]
├─────────────┤
│ collect_b   │──→ writes evidence[kind: report_b]
├─────────────┤
│ collect_c   │──→ writes evidence[kind: report_c]
└─────────────┘
       │
       ▼
  ┌─────────────────────────────────┐
  │ state with guard:               │
  │   kind: evidence                │
  │   requires:                     │
  │     - { kind: report_a, count: 1}
  │     - { kind: report_b, count: 1}
  │     - { kind: report_c, count: 1}
  └─────────────────────────────────┘
       │ all satisfied
       ▼
     done
```

## Guard clauses

| Clause | Meaning |
|--------|---------|
| `kind` | Evidence type to count |
| `count` | How many records needed (default 1) |
| `min_confidence` | Filter by model-stated confidence (0..1) |
| `require_digest` | Require a content-identity digest on each record |

## Use cases

- **Quorum approval** — two finance approvers must sign off on a high-value expense.
- **Artifact attestation** — verifier must produce a JUnit XML AND a SARIF file AND coverage JSON.
- **Multi-source confirmation** — three separate monitors must confirm a deployment is healthy.

## Run it

```bash
praxec check --config examples/pattern-evidence-quorum/gateway.yaml
```
