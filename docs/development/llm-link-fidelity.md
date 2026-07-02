# LLM Link-Following Fidelity Report

**Status: open research question — experiment design ready, runs pending.**

This is an empirical question, not a production blocker. The
architecture of praxec assumes LLMs can reliably:

1. Read a `links` array from a response
2. Pick the correct link based on `rel` and context
3. Submit it verbatim (including `workflowId`, `expectedVersion`,
   `transition`, prefilled `arguments`)
4. Handle rejection by reading the new `links` array and recovering
5. Stop when `result.status` is `"completed"` or when only
   `"actor": "human"` links remain

The mechanical driver (`examples/tdd/dogfood-drive.py`) proves the
gateway returns correct bytes. Whether live LLMs follow links
reliably across providers and model tiers is a model-evaluation
question, distinct from gateway correctness.

## Experiment design

**Models:** Claude 4 Opus, Claude 4 Sonnet, GPT-4o.

**Workflows:**

| Workflow | States | Special challenge |
|----------|--------|-------------------|
| `simple-proxy` | 1 | Baseline link-following |
| `content-publish` | 7 | `linkFilter: byGuards`, human-gate stop |
| `tdd` | 7 | Adversarial; count-based cheat detection |

**Runs:** 10 × 3 models × 3 workflows = 90 runs.

**Metrics per run:**

- Success rate (completed without human intervention)
- Turns to completion
- Invalid transition attempts
- Stale `expectedVersion` errors
- Hallucinated transition names
- Premature stop (gave up with legal links remaining)
- Human-gate violations (tried to call `"actor": "human"` links)
- Recovery from rejection

## Harness

A future harness directory will hold:

- `driver.py` — spawns `praxec serve` as a subprocess, speaks
  JSON-RPC over stdio, exposes a thin HTTP API to the agent.
- `agent.py` — drives a model directly via its API (not through an
  MCP host) so we control the exact bytes the model sees and emits.
- `workflows/*.yaml` — gateway configs (noop executors).

Direct API calls (not Claude Desktop / Zed) isolate the model's
link-following ability from host-specific tool-use formatting.

## Cost estimate

90 runs × ~10–20 turns each ≈ $50–$150 across the three providers.

## Why this matters

If all three models follow links reliably, the architecture is
model-agnostic. If only the strongest reasoning model can, that's a
documentable model-tier dependency — operators pick a tier that
matches their workflow's complexity. Either result is useful and
neither blocks shipping the gateway, which is correct by construction
regardless of which model drives it.

## Next steps

Run the experiment, fill in the results, replace the README's
"we won't claim it for you" language with concrete per-model numbers.

Until then, this document tracks the open research question. It is
**not** a blocker on production-readiness of praxec itself —
the gateway returns correct HATEOAS bytes; the question is which
models will follow them.
