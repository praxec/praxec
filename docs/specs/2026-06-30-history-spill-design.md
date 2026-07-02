# History-spill: lossless elision with on-demand recall

Status: approved design (2026-06-30) · Branch: feat/intent-driven-loom

## Problem

The agent tool-loop re-sends the whole conversation history every turn. To stay
under the context window, `enforce_history_budget` (in
`crates/praxec-agents/src/tool_budget.rs`) elides the **oldest**
`[assistant(tool_calls), user(tool_results)]` turn-pairs when the cumulative
history exceeds a byte budget, keeping the goal (index 0) + the recent-N turns.

Today elision is a **silent drop** — those turns are gone. That violates two
goals at once:

1. **Maximum LLM performance + token frugality.** We want the live context as
   tight as possible (a full history dump degrades responses — lost-in-the-middle
   — and wastes tokens). So we do NOT want to keep everything in context.
2. **Recall of detail on demand.** The model sometimes needs to go back and look
   at a specific earlier detail. Dropping makes that impossible; dumping
   everything back defeats goal 1. We need *targeted* recall.
3. **A genuinely lossless, addressable store** (ADR-0012's "bounded working set
   over a lossless addressable store"). The current drop is lossy, so the
   principle is not actually honored.

This is the balance: a tight live context **plus** addressable, agent-initiated
recall of any elided detail — never a full re-dump.

## Decisions (from the design interview)

- Replacement artifact: **compact breadcrumb + targeted recall** (not silent
  audit-only spill, not a per-turn model summary).
- Agent-retrievable recall is a **hard requirement** (rules out audit-only).
- Ledger descriptors are **minimal and model-free** (tool names + args + size +
  slot) — no summarization call (a summary call would burn the very tokens we are
  trying to save, for marginal gain; the agent already knows the meaning of a
  tool call it made).

## Design

### Trigger (unchanged)

`enforce_history_budget` keeps its per-turn byte-budget check and its invariant:
always retain the goal (index 0) and the recent-N turn-pairs
(`HISTORY_KEEP_RECENT_MSGS`). Only the OLDEST pairs are elision candidates.

### Action (changed: spill instead of drop)

For each oldest `[assistant(tool_calls), user(tool_results)]` pair that must
leave the live window:

1. **Serialize** the pair and `put()` it into the per-run `SpillStore` →
   one **slot id per elided turn** (one slot = one recoverable turn-pair).
2. **Append one model-free line to a ledger** that lives appended to the **goal
   message (index 0)**. Index 0 is never elided, and keeping the ledger there
   preserves the strict user/assistant alternation AND tool_call↔tool_result
   pairing the function already guards (no new message is inserted mid-history).
   Line shape:

   ```
   elided #7 · tools: read_file(gateway.rs), grep("spill") → 2.1 KB · recall: spill_read slot=H7
   ```

   Fields, all extracted model-free: a monotonic elision index, the tool
   name(s) + a truncated args snippet from the assistant message, the
   tool_results byte size from the user message, and the slot id.

   The ledger is a **single marked section** within the goal message
   (e.g. delimited by a `--- recallable elided history ---` header). Each
   call **extends that section in place** with the newly-elided lines —
   find-or-create the section, never prepend a fresh header — so across many
   calls there is exactly one ledger that grows by one line per elision (each
   pair is elided exactly once, so lines never duplicate).
3. **Remove** the pair from the live window.

Loop until the history fits the budget (same loop condition as today).

### Recall (reuses what is already built)

No new tool. The already-injected `spill_read(slot, range?)` (in
`tool_budget.rs`, wired into `run()`) returns the spilled pair verbatim. The
agent reads the ledger on the goal message, picks the single slot it needs, and
pulls back only that turn. `spill_read`'s default range returns the whole
(small) turn; a range narrows it further.

### Losslessness & isolation

Every elided turn is `put()` into the **per-run `InMemorySpillStore`** — the
same store, with the same per-run isolation already proven under concurrency
(`a_slot_from_one_store_is_unknown_in_another`,
`concurrent_puts_into_one_store_all_round_trip`). So the run's full conversation
stays addressable: lossless and recall-able, honoring ADR-0012.

### The one signature change

`enforce_history_budget(history: &mut Vec<Message>, budget: usize)` becomes
`async fn enforce_history_budget(history: &mut Vec<Message>, budget: usize,
store: &dyn SpillStore)`. `run()` already owns the per-run `spill` and passes it
in. (The function is called once per turn before sending; `put()` is async.)

## Components & boundaries

- `enforce_history_budget` (tool_budget.rs): owns the elide→spill→ledger logic.
  Input: live history + budget + store. Effect: history mutated in place
  (shrunk + ledger appended to index 0); store gains one slot per elided turn.
- Ledger formatting + descriptor extraction: a small model-free helper
  (`describe_elided_pair(assistant, user) -> String`), unit-testable in
  isolation against constructed `Message`s.
- `SpillStore` / `spill_read`: unchanged (reused as-is).
- `run()`: passes `&spill` into the call; otherwise unchanged.

## Testing

1. **Spill-not-drop:** over-budget history → the oldest pair is `put()` into the
   store (a slot exists) and is removed from the live window; history now fits
   the budget.
2. **Ledger entry:** after elision, the goal message carries a ledger line for
   the elided turn, naming its tool(s) and slot.
3. **Lossless round-trip:** the slot named in the ledger, read via the store
   (`get`)/`spill_read`, returns the exact elided turn-pair content.
4. **Structural invariants:** after elision, history still starts with a user
   message and strictly alternates user/assistant, and every assistant
   `tool_calls` retains its paired `tool_results` (no orphaned half-pair).
5. **Retention invariant:** the goal (index 0) and the recent-N pairs are never
   spilled.
6. **Multi-elision:** a run that elides several times yields one slot + one
   ledger line per elided turn, and every elided turn remains recallable.

## Scope guard (YAGNI — explicitly out)

No model-generated summaries; no embeddings / semantic recall; no new tool; no
cross-run or persistent store. These stay out unless a concrete need appears.
