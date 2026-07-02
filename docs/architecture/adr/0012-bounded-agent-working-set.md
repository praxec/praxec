# ADR-0012: Agent context is a bounded working set over a lossless, addressable store

**Status:** Accepted

**Date:** 2026-06-29

## Context

The agent tool-loop (`RigSessionRunner::drain_turn`) re-sends the full
conversation history every turn. A single unbounded tool result (observed live: a
filesystem dump at 1.86M tokens) makes every subsequent turn 400 at the provider.
Two guards exist, both at the wrong boundary (request-assembly, the last possible
moment) and both **lossy**: `truncate_tool_result` discards bytes past 64 KiB, and
`enforce_history_budget` elides the oldest turn-pairs. They prevent the crash but
lose data to do it, and they do nothing about quality degradation *below* the
window limit.

The wider problem is not just "fit in the window." The industry evidence
(2025–2026) is consistent on three points:

- **More context actively degrades quality**, not just cost. Chroma's "context
  rot" study (18 frontier models incl. Claude 4) shows degradation at every length
  increment below the window, a >30% "lost-in-the-middle" accuracy drop, and
  *distractor interference* — semantically similar but irrelevant content misleads
  the model.
- **Ahead-of-time summarization is a proven failure mode**: compressing history
  before the future task is known discards details that later matter.
- **The proven recall mechanism is files + agent-driven iterative search**, which
  out-benchmarks specialized vector/graph memory (Letta filesystem 74.0% vs Mem0
  graph 68.5% on LoCoMo), and **clear-results / keep-references + file-backed
  memory** is shipped and validated (Anthropic context editing: +39% combined,
  84% fewer tokens on a 100-turn task).

No specialized memory product (Mem0, Zep, Letta) holds a defensible moat — the
*mechanism* matters more than the vendor, and none offer governed, reproducible
memory operations. See `docs/poka-yoke-context-spill.md` for the full prior-art
survey and citations.

## Decision

**The conversation is a bounded working set; durable content lives out of band and
is referenced by handle, never inlined.** `max_turns` is already capped (24); we
also bound the per-fragment size, so total context is provably
`O(turns × max-fragment)` — an over-window request becomes unrepresentable by
arithmetic, not by a reactive guard. We adopt the proven industry stack
(clear-results + file-backed memory + iterative search) and add three deliberate
separations that exceed it, each grounded in a TRIZ resolution:

1. **Transparent spill, not lossy truncation.** At the runner's single result
   chokepoint, an oversized tool result is written to a session-scoped, ephemeral,
   content-addressed `SpillStore` and replaced in the transcript by a compact,
   self-describing **handle** (`head + summary + slot + bytes + read affordance`).
   A synthetic injected `spill.read { slot, range }` tool (mirroring how
   `final_answer` is injected) gives transparent, on-demand read-back. The agent
   needs no prompt cooperation. *(TRIZ #34 discard-and-recover.)*

2. **Addressable handles as primary recall; search as fallback.** A handle *tells*
   the model what exists and exactly how to fetch it — directed retrieval, not
   embedding-similarity guessing — which sidesteps both distractor interference and
   the "model doesn't know what to query" failure. Iterative `search_file` /
   `read_range` over the lossless store is the fuzzy-recall fallback.
   *(TRIZ #24 intermediary + #25 self-service.)*

3. **Split orientation from detail; own the assembly position.** A small structured
   **running-state** object (`{done, pending, findings, next, open_questions}`)
   carries orientation and is *regenerated from source* every K turns (never
   summary-of-summary), bounding drift to a K-turn window. Detail is never
   compressed — only spilled, lossless and addressable. Prompt assembly anchors
   goal + running-state at the **start** and freshest verbatim turns at the **end**,
   deliberately exploiting the U-shaped attention curve. *(TRIZ #1 segmentation +
   #13 the-other-way-around.)*

`SpillStore` and the running-state are **session working memory**, not the
Blackboard: `AgentExecutor` stays blackboard-pure (it still returns only its
`output` contract; anything durable goes through the governed `final_answer`).
Working memory is ephemeral, ungoverned, content-addressed; durable memory is the
Blackboard, governed, the output contract — the two never merge.

`enforce_history_budget` is retained as defense-in-depth but demoted: it becomes
**spill-then-drop** (elided turns are recoverable via `spill.read`, never silently
lost) and should essentially never fire once results are handles. If it fires, that
is a detectable defect that emits `Evidence`, not normal operation.

## Consequences

- An over-window request is structurally impossible, and quality-degrading context
  bloat is bounded — we target a *small, high-signal* working set, not merely a
  sub-window one, directly answering context rot rather than just the 400.
- No information is discarded: lossy truncation and lossy elision are replaced by
  lossless spill plus directed recovery.
- Reuses the proven-winning substrate already in-repo (file tools, `search_file` /
  `read_range`, sqlite stores) — no memory-service dependency, no model training.
- Every spill / elision / state-refresh is an `Evidence` event, making memory
  operations **governed, auditable, and reproducible** — a property no external
  memory layer offers, and the answer to the field's non-determinism/context-rot
  reproducibility gap.
- A new advisory `validate.rs` check (Warning, not Error — runtime is already safe)
  flags `kind: agent` caps that combine whole-file readers with long loops and no
  scratchpad, steering authors toward targeted reads at definition time.
- The embedding/semantic-index path (currently off) gains a concrete consumer as
  the fuzzy-recall fallback, justifying re-enabling it.

## References

- `docs/poka-yoke-context-spill.md` — implementation design + full prior-art survey
- `crates/praxec-agents/src/rig_runner.rs` — `drain_turn`, the result
  chokepoint (`truncate_tool_result`), `enforce_history_budget`, `final_answer`
  injection (the model for `spill.read`)
- `crates/praxec-agents/src/executor.rs` — `AgentExecutor` blackboard-purity
- `crates/praxec-core/src/validate.rs` — advisory cap-shape check
- Builds on ADR-0006 (execution sandbox: agent output is a candidate, not a command)
  and the validity-first / FMECA engine discipline.
