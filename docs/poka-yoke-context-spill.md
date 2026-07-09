# Poka-yoke: transparent spill — context explosion made unrepresentable

Status: implemented ([ADR-0012](architecture/adr/0012-bounded-agent-working-set.md)) · Scope: `praxec-agents` agent tool-loop · Author: dogfooding pass 2026-06-29

Implementation lives in `crates/praxec-agents/src/spill.rs` (the `SpillStore`
trait + in-memory impl) and `crates/praxec-agents/src/rig_runner.rs` (the ingress
chokepoint and history budget). Concrete anchors are cited inline below.

## Problem

The agent tool-loop (`RigSessionRunner::drain_turn`) re-sends the full conversation
history every turn. A single unbounded tool result (observed live: a filesystem
dump at 1.86M tokens) makes every subsequent turn 400 at the provider. We have two
guards today, both at the **wrong boundary** and both **lossy**:

- `truncate_tool_result` (`rig_runner.rs:189`) — caps one result at
  `MAX_TOOL_RESULT_BYTES` (64 KiB) and appends a "re-run with a narrower scope"
  marker. **Information is discarded**; the agent must redo work to get it back.
- `enforce_history_budget` (`rig_runner.rs:419`) — elides the oldest turn-pairs
  until the re-sent history fits `DEFAULT_MAX_HISTORY_BYTES` (1 MiB). **Earlier
  context is dropped on the floor.**

Both treat the symptom at request-assembly time — the last possible moment, after
the blob is already in the transcript. They keep us from crashing; they do not make
the bad state impossible, and they lose data to do it.

## Principle

> The conversation is a bounded **working set**; durable content lives **out of
> band** and is referenced by handle, never inlined.

`max_turns` is already capped (24). If every fragment that can enter the transcript
is also bounded, total context is provably `O(turns × max-fragment)` — the window
**cannot** be exceeded, by arithmetic, not by discipline or a reactive guard. Today
`max-fragment` is unbounded; the fix bounds it *without losing the content*.

The boundary moves **upstream** to where a result enters the conversation, and the
discarded bytes are **spilled** to a session-scoped store and replaced by a compact,
self-describing handle the agent can read back on demand. Retrieval discipline,
enforced by construction — the same retrieval-grounded principle as the
semantic-index / Evidence direction.

## Chosen shape: automatic, transparent spill

The agent needs **no cooperation and no prompt instruction**. It calls a tool as
usual; if the result is large, what comes back is a handle (head + summary + slot id
+ schema), and the full payload is retrievable via an injected `spill_read` tool.
This is a stronger poka-yoke than "forbid raw reads and make the agent compose
`search → read_range`," because correctness does not depend on the model behaving.

```
agent → read_file("big.rs")
runner ingress chokepoint:
  result > threshold?  ── no ──→ inline as today
                       └─ yes ─→ SpillStore.put(payload) → slot
                                  return HANDLE { head, summary, slot, bytes, schema?,
                                                  read: {tool:"spill_read", args:{slot, range}} }
agent → (optionally) spill_read { slot, range: [a,b] }  → that window only
```

## Mechanism

### 1. One universal chokepoint (prevent)

Every tool result flows through a single line — `rig_runner.rs:612`,
`spill_on_ingress(&spill, &c.name, raw)` (this replaced the old
`truncate_tool_result` chokepoint):

```rust
// session-scoped, injected into the runner alongside max_history_bytes
let out = spill_on_ingress(&spill, &c.name, out).await; // handle text if spilled, else `out`
```

`spill_on_ingress` writes the payload to the `SpillStore` and returns the handle text
(below threshold → returns the input untouched). Because this is the runner's chokepoint,
**no `ToolHost` implementation changes** — MCP tools (`CompositeToolHost`), file
tools (`FileEditToolHost`), and any future host are covered identically. The cap is
enforced once, where it cannot be bypassed.

### 2. Injected `spill_read` tool (transparent read-back)

The runner already injects `final_answer` as a synthetic tool the agent always has.
Inject a second synthetic tool, `spill_read`, backed by the same session `SpillStore`:

```
spill_read { slot: string, range?: [start, end] }  → bytes [start,end) of the slot
```

`range` defaults to the next window after the head already shown. This is the
`read_range` affordance generalized to *any* spilled result, not just files — a
24-MiB MCP tool response is now navigable too. The tool is added to `tool_conn` and
to the `tools` advertised to the model, exactly like `final_answer`.

### 3. The handle format

What re-enters the transcript when a result spills — small, fixed-size, self-describing:

```json
{
  "spilled": true,
  "tool": "read_file",
  "bytes": 1958400,
  "slot": "spill:7f3a…",
  "head": "use anyhow::Result;\npub struct …",        // first ~2 KiB, char-boundary safe
  "summary": "Rust source, 1.9 MB, 48k lines",          // optional; cheap heuristic now, model-free
  "read": { "tool": "spill_read", "args": { "slot": "spill:7f3a…", "range": [2048, 66560] } }
}
```

Head + handle are themselves bounded, so a spilled result has a **fixed** transcript
cost regardless of payload size. That is what collapses `max-fragment` to a constant.

### 4. `SpillStore` — session working memory, not the Blackboard

`AgentExecutor` is **blackboard-pure** by contract (`executor.rs:1` — returns
`output`, holds no blackboard write handle). Spill must not break that. So spill is
**not** a governed Blackboard write; it is ephemeral session working memory:

```rust
#[async_trait]
pub trait SpillStore: Send + Sync {
    /// Store `payload`, return an opaque content-addressed slot id.
    async fn put(&self, payload: String) -> String;
    /// Read `[start, end)` bytes of a slot (char-boundary clamped). Unknown slot → Err.
    async fn get(&self, slot: &str, start: usize, end: usize) -> Result<String, String>;
}
```

- Default impl: in-memory, keyed by content hash, scoped to and dropped with the
  agent run. No durability, no governance, no cross-session leak.
- The durable Blackboard is unchanged: if the agent wants something to **persist**,
  it puts it in its `final_answer` `output` (governed) as it does today. Spill is
  the scratchpad *behind* the conversation, not a side-channel *around* the contract.

This separation is the load-bearing decision: working memory (ephemeral, ungoverned,
content-addressed) vs durable memory (Blackboard, governed, the output contract).

### 5. History budget becomes the backstop it should be (fail-fast)

With per-result spill in place, every fragment entering history is already small (a
handle, not a blob), so `O(turns × max-fragment)` has a tiny `max-fragment` and
`enforce_history_budget` should essentially **never fire**. Keep it as
defense-in-depth, but make elision **spill-then-drop**: an elided turn-pair is
written to the SpillStore under a `turn:<n>` slot before removal, and a single line
is left in place — `[turns 3–6 elided to spill:… — spill_read to recover]`. No
information is silently lost; recovery is one tool call away. If it fires at all,
that is a **detectable defect** (emit Evidence), not normal operation.

## FMECA layers → concrete code points

| Layer | Mechanism | Where |
|---|---|---|
| **Prevent** | spill-on-ingress replaces lossy truncate | `rig_runner.rs:612` (`spill_on_ingress`) |
| **Prevent** | transparent read-back | injected `spill_read` tool (mirror `final_answer` injection) |
| **Prevent** | working memory ≠ Blackboard | new `SpillStore`; `AgentExecutor` stays blackboard-pure |
| **Detect** | unsafe cap shape flagged before launch | `validate.rs` — see below |
| **Detect** | context-size + spill-rate visible | `Evidence` on each spill / elision |
| **Fail-fast** | budget guard = loud, recoverable, rare | `enforce_history_budget` → spill-then-drop + Evidence |

### Static validation rule (detect, before launch)

Same family as the terminal-reachability check already in `validate.rs`. A
`kind: agent` cap that wires unbounded readers into a long loop is structurally
fine **now** (spill protects it at runtime), so this is a **Warning, not an Error** —
advisory, with a fix hint: "high `max_turns` with whole-file readers and no
scratchpad slot — results will spill; prefer `search_file` + `read_range` for tighter
context." It informs authoring; the runtime is already safe regardless.

### Observability (detect)

Record an `Evidence` event per spill: `{tool, bytes, slot, turn}`, and per elision:
`{turns, bytes_reclaimed, slot}`. This makes context pressure a **measured, in-loop
signal** (it can feed the no-progress watchdog), not an invisible failure mode — and
it satisfies the "measurement must change a decision" bar: spill-rate tells an author
their cap is doing whole-file dumps when it should be doing targeted reads.

## Test plan

Extend the existing virtual-time tool-loop tests (`#[tokio::test(start_paused = true)]`):

- `HugeResultHost` / `BigResultHost` already exist. Rename
  `an_oversized_tool_result_is_truncated_before_it_reaches_the_model` →
  `…_is_spilled_…`: assert the transcript carries a `spilled: true` handle, **not**
  the payload, and that `history_bytes` stays bounded across the loop.
- `spill_read_returns_the_requested_window` — agent calls `spill_read`, gets exactly
  `[start,end)`, char-boundary clamped; unknown slot → tool error string.
- `transparent_spill_needs_no_prompt` — a host that returns a 2-MiB result with an
  agent that never mentions spill still completes without a 400 and without losing
  the head.
- `history_elision_spills_before_dropping` — drive past `max_history_bytes` (using
  `with_max_history_bytes`), assert elided turns are recoverable via `spill_read` and
  an Evidence event was emitted.
- Keep `the_history_budget_bounds_the_request_across_a_long_tool_loop` green — it now
  rarely needs to elide because results are handles.

## Prior art & how we beat it

This design is not novel infrastructure — it assembles the **proven** industry stack
and adds three deliberate separations the standard leaves on the table. The decision
is recorded permanently in [ADR-0012](architecture/adr/0012-bounded-agent-working-set.md);
this section is the evidence behind it.

### What the field has proven (2025–2026)

- **Files + agent-driven iterative search beat specialized vector/graph memory.**
  Letta's controlled benchmark: a filesystem with grep/search tools scored **74.0%
  on LoCoMo vs Mem0's graph variant at 68.5%** — agents are strong at *reformulating
  queries and searching iteratively* (multi-hop); vector memory is single-hop. The
  proven primitive is search-over-files-on-demand, not embeddings.
- **Clear tool *results*, keep *references*, back it with files.** Anthropic's
  shipped context editing + memory tool: **+39% combined, +29% editing-alone, 84%
  fewer tokens** on a 100-turn task that otherwise fails from context exhaustion.
  This is transparent spill, externally validated.
- **More context actively *degrades quality*, not just cost.** Chroma's "context
  rot" study (18 frontier models incl. Claude 4): degradation at every length
  increment *below* the window; **lost-in-the-middle = >30% drop** (U-shaped
  position bias); **distractor interference** — similar-but-irrelevant content
  misleads. So the goal is a *small, high-signal* working set, not merely a
  sub-window one.
- **Ahead-of-time summarization is a named failure mode.** Compressing history
  before the future task is known discards details that later matter ("Beyond
  Static Summarization," "Active Context Compression," 2025–26).
- **No memory-SaaS moat.** Mem0 / Zep / Letta publicly dispute each other's LoCoMo
  numbers; the *mechanism* matters more than the vendor.

### The three TRIZ separations that exceed the standard

| Contradiction in the standard | Resolution (and TRIZ principle) |
|---|---|
| Compress to fit **vs** lose future-relevant detail | Compress only *orientation* (running-state, regenerable); keep *detail* lossless + addressable. *#1 segmentation, #34 discard-and-recover* |
| Retrieve relevant chunks **vs** distractor interference + "model can't query what it can't see" | **Addressable handles** = directed retrieval (the handle says what's there + how to read it); beats both vector RAG and blind grep. *#24 intermediary, #25 self-service* |
| Inject retrieved context **vs** lost-in-the-middle eats 30% | We **own assembly position**: goal + state at the start, freshest verbatim at the end, spill the middle; default to *not* loading. *#13 the-other-way-around* |

Plus the durable edge no external memory layer offers: **every spill / elision /
state-refresh is an `Evidence` event** — governed, auditable, reproducible memory,
which directly answers the field's context-rot reproducibility gap (TRIZ: use the
proven-simplest substrate — files + search — that we already own, and add governance
the SaaS products structurally cannot).

### Sources

- [Letta — Is a Filesystem All You Need?](https://www.letta.com/blog/benchmarking-ai-agent-memory/)
- [Anthropic / Claude — Managing context (editing + memory tool)](https://claude.com/blog/context-management)
- [Chroma context rot — Morph](https://www.morphllm.com/context-rot) · [Redis](https://redis.io/blog/context-rot/)
- [Beyond Static Summarization (arXiv)](https://arxiv.org/pdf/2601.04463) · [Active Context Compression (arXiv)](https://arxiv.org/abs/2601.07190)
- [Mem0 — Context Engineering guide](https://mem0.ai/blog/context-engineering-ai-agents-guide) · [Zep — Is Mem0 really SOTA?](https://blog.getzep.com/lies-damn-lies-statistics-is-mem0-really-sota-in-agent-memory/) · [Weaviate — Context Engineering](https://weaviate.io/blog/context-engineering)

## Non-goals / open decisions

- **Summary quality.** `summary` starts as a cheap, model-free heuristic (size, line
  count, sniffed kind). A model-generated summary is a later option, gated on cost —
  not required for the poka-yoke to hold.
- **Spill durability.** Default is in-memory, run-scoped. A sqlite-backed `SpillStore`
  (consistent with the store-backend stance: memory/file/sqlite only) is a follow-up
  if cross-turn spill needs to survive a process restart; not needed for the invariant.
- **Threshold tuning.** Reuse `MAX_TOOL_RESULT_BYTES` (64 KiB) as the spill threshold
  initially; revisit per-model once context-size Evidence is flowing.

## Why this closes the hole for good

`max_turns` bounded × `max-fragment` bounded (handle-sized) ⇒ assembled request is
bounded **by construction**. The agent cannot represent an over-window context, and —
because spill is transparent — it never has to. The budget cap remains as
defense-in-depth, demoted to the rare-and-loud backstop it was always meant to be.
