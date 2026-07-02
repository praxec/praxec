# Example: deploy pipeline with deterministic chaining

A deployment pipeline where the LLM makes one call and gets a
decision — not three round trips of babysitting.

## Why this example

Most pipeline steps don't need judgment. Lint either passes or it
doesn't. Tests either pass or they don't. Building an artifact is
computable. Without chaining, the LLM has to read each result, reason
about the "choice" (there is none), pick the transition, and submit.
That's three wasted round trips — real tokens, real latency, zero
value.

This example shows how `actor: deterministic` eliminates those round
trips entirely.

## What this shows

- **Deterministic chaining** — lint, test, and build are all tagged
  `actor: deterministic`. The runtime chains through them
  automatically in a single `workflow.start` call.
- **Phase guidance** — each state carries `goal` and `guidance`
  strings. The LLM arrives at the deploy decision with context
  about what to look at (lint report, test count, coverage,
  artifact ID).
- **Chain trace** — the response includes a `chain` array showing
  each auto-executed step and its result, so the LLM has full
  context for the decision.
- **Mixed actor pipeline** — deterministic steps run automatically;
  the `agent` step at `ready_to_deploy` stops the chain and hands
  control back to the LLM.
- **Failure recovery** — if a chain step fails, the response
  includes the partial trace and a recovery link for the failed step.

## Run it

```bash
cargo build --release
./target/release/praxec serve --config examples/deploy-pipeline/gateway.yaml
```

Wire it into your MCP host (see root README) and ask the model to
deploy a service. It calls `workflow.start` once and gets the result
at the deploy decision — three executor calls, zero LLM round trips.

## Config walkthrough

The pipeline has six states:

| State | Actor | What happens |
|-------|-------|-------------|
| `lint` | deterministic | Runs linter, auto-advances on success |
| `test` | deterministic | Runs test suite, auto-advances on success |
| `build` | deterministic | Builds artifact, auto-advances on success |
| `ready_to_deploy` | agent | Chain stops — LLM reviews results and decides |
| `deployed` | — | Terminal |
| `aborted` | — | Terminal |

`maxChainDepth: 10` caps the chain as a safety net against
misconfigured cycles.
