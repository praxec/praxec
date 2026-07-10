# Praxec documentation

New here? Start with the [README](../README.md) for what Praxec is and a
60-second try, then **[core concepts](architecture/concepts.md)** for the mental
model. This index is the map; each layer below goes deeper — read down only as
far as you need.

## I want to…

| …do this | Go to |
|---|---|
| Understand the core ideas | [Concepts](architecture/concepts.md) |
| Write a workflow config | [Configuration reference](reference/configuration.md) · [Concepts](architecture/concepts.md) |
| Reach a tool / CLI / API | [Connections](guides/connections.md) |
| Add guards, approvals, the trust model | [Governance](reference/governance.md) |
| Steer an LLM that authors transitions | [LLM authoring guidance](guides/llm-guidance.md) |
| Run agents in a workflow (models + keys) | [Agents & models](guides/agents-and-models.md) |
| Check a workflow is sound | [Checking workflows](guides/checking-workflows.md) |
| Compose many configs at scale | [MCP control architecture](architecture/mcp-control-architecture.md) |
| Embed the crates as a library | [Embedding](guides/embeddings.md) |
| Drive the agentic TUI | [TUI agent](guides/tui-agent.md) |
| Work on the codebase | [Repo internals](development/internals.md) · [Testing strategy](development/testing-strategy.md) |

## Guides — do a task

Task-oriented how-tos. Concrete, shallow, safe to skim.

- [Connections](guides/connections.md) — wire MCP / CLI / REST downstream services
- [Embeddings](guides/embeddings.md) — embed the gateway crates in your own binary
- [LLM authoring guidance](guides/llm-guidance.md) — prefill, phase guidance, authoring patterns
- [Agents & models](guides/agents-and-models.md) — `models.yaml`, provider keys, `orchestrate` vs `auto_drive`
- [Checking workflows](guides/checking-workflows.md) — `praxec fuzz` / `praxec test`
- [TUI agent](guides/tui-agent.md) — the interactive agentic runtime

## Reference — the contract

Normative "what." The surfaces that don't change without a version bump.

- [Spec](reference/spec.md) — the canonical specification
- [Configuration](reference/configuration.md) — every config knob
- [Governance](reference/governance.md) — guards, actors, output mapping, the execution trust model
- [Invariants](reference/invariants.md) — runtime guarantees
- [Stability](reference/stability.md) — what is and isn't stable per the 0.0.x line
- [Performance](reference/performance.md) — the `gateway_overhead` benchmark + runbook

## Architecture — why it's built this way

Explanation and decisions. The deepest layer: rationale, design umbrellas, and
Architecture Decision Records.

- [Concepts](architecture/concepts.md) — the mental model (start here)
- [MCP control architecture](architecture/mcp-control-architecture.md) — composing at scale
- [Mission Control design](architecture/mission-control-design.md) — the cockpit design umbrella
- [Capability/orchestrator composition](architecture/capability-orchestrator.md)
- [Semantic catalog & guided settings](architecture/semantic-catalog.md)
- [Workflow test/fuzz design](architecture/workflow-test-design.md)
- [TUI agent design](architecture/tui-agent-design.md)
- [Research / positioning](architecture/research.md)
- **Decision records:** [`architecture/adr/`](architecture/adr/) (ADR-0001 … ADR-0012)

## Development — working on Praxec

Contributor-facing.

- [Repo internals](development/internals.md) — workspace layout, crates, test map
- [Testing strategy](development/testing-strategy.md) — the canonical test plan
- [Publishing](development/publishing.md) — the release runbook
- [Stress tests](development/stress-tests.md) — pressure scenarios
- [Resource-leak test plan](development/resource-leak-test-plan.md)
- [LLM link-fidelity](development/llm-link-fidelity.md) — open research tracker
