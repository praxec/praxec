# History-spill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `enforce_history_budget` spill elided conversation turn-pairs into the per-run `SpillStore` (one slot each) plus a compact model-free recall ledger on the goal message, instead of silently dropping them — so the live context stays tight while every elided turn remains losslessly recallable via the already-injected `spill_read`.

**Architecture:** All changes are in `crates/praxec-agents`. The budget guard (`tool_budget.rs`) gains three model-free helpers (descriptor, ledger, rewritten guard) and becomes `async` taking `&dyn SpillStore`; the single call site in `rig_runner.rs::run()` passes the per-run `spill` it already owns. No new tool, no new store, no model calls.

**Tech Stack:** Rust, `rig-core` 0.38.2 message types (`Message`, `AssistantContent::ToolCall(ToolCall{ function: ToolFunction{ name, arguments } })`, `UserContent::Text(Text{ text })`), `serde_json`, `tokio` tests.

## Global Constraints

- All new code lives in `crates/praxec-agents/src/tool_budget.rs` (helpers + tests) and one line in `crates/praxec-agents/src/rig_runner.rs` (call site). Verbatim spec source: `docs/specs/2026-06-30-history-spill-design.md`.
- The budget guard MUST keep its existing invariants: never elide the goal (index 0) or the recent-N pairs (`HISTORY_KEEP_RECENT_MSGS`), and preserve strict user/assistant alternation + `tool_call`↔`tool_result` pairing (it elides whole `[assistant, user]` pairs from index 1).
- Descriptors are model-free — NO summarization/model calls.
- The ledger is a single marked section on the goal message, extended in place (find-or-create — never a fresh header per call).
- `SpillStore` is the per-run `InMemorySpillStore` already created in `run()`; do not add a new or cross-run store.
- Run `cargo test -p praxec-agents` and `cargo clippy -p praxec-agents --lib --bins -- -D warnings` before each commit; both must be clean.

---

### Task 1: `elided_descriptor` — model-free turn descriptor

**Files:**
- Modify: `crates/praxec-agents/src/tool_budget.rs` (add helper near `enforce_history_budget`; extend the `use rig::completion::…` import)
- Test: `crates/praxec-agents/src/tool_budget.rs` (`mod tests`)

**Interfaces:**
- Produces: `pub(crate) fn elided_descriptor(assistant: &Message, user: &Message) -> String` — a one-line, model-free description of an elided turn-pair: the tool calls the assistant made (`name(args-snippet)`, args truncated to ~48 chars) and the byte size of the paired tool-results message. Used by Task 3 to build a ledger line.

- [ ] **Step 1: Extend the rig import in `tool_budget.rs`**

At the top of the file, the existing line:
```rust
use rig::completion::{Message, ToolDefinition};
```
becomes:
```rust
use rig::completion::{AssistantContent, Message, ToolDefinition};
```

- [ ] **Step 2: Write the failing test** (append inside `mod tests`)

```rust
#[test]
fn elided_descriptor_names_the_tool_and_args() {
    use rig::completion::AssistantContent;
    use rig::message::{ToolResult, ToolResultContent, UserContent};
    use rig::OneOrMany;
    let assistant = Message::Assistant {
        id: None,
        content: OneOrMany::one(AssistantContent::tool_call(
            "c1",
            "read_file",
            json!({ "path": "gateway.rs" }),
        )),
    };
    let user = Message::User {
        content: OneOrMany::one(UserContent::ToolResult(ToolResult {
            id: "c1".into(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::text("X".repeat(2000))),
        })),
    };
    let d = elided_descriptor(&assistant, &user);
    assert!(
        d.contains("read_file") && d.contains("gateway.rs"),
        "descriptor must name the tool + args; got: {d}"
    );
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p praxec-agents elided_descriptor_names_the_tool_and_args`
Expected: FAIL — `cannot find function elided_descriptor`.

- [ ] **Step 4: Write the implementation** (place above `enforce_history_budget`)

```rust
/// A model-free one-line descriptor of an elided turn-pair: the tool calls the
/// assistant made (name + a truncated args snippet) and the byte size of the
/// paired tool-results message. Enough signal for the agent to decide whether
/// to `spill_read` the slot — with no model call (token-frugal by design).
pub(crate) fn elided_descriptor(assistant: &Message, user: &Message) -> String {
    let mut tools: Vec<String> = Vec::new();
    if let Message::Assistant { content, .. } = assistant {
        for c in content.iter() {
            if let AssistantContent::ToolCall(tc) = c {
                let mut args = tc.function.arguments.to_string();
                if args.len() > 48 {
                    let mut end = 48;
                    while !args.is_char_boundary(end) {
                        end -= 1;
                    }
                    args.truncate(end);
                    args.push('…');
                }
                tools.push(format!("{}({})", tc.function.name, args));
            }
        }
    }
    let tools_desc = if tools.is_empty() {
        "(no tool calls)".to_string()
    } else {
        tools.join(", ")
    };
    let bytes = serde_json::to_string(user).map(|s| s.len()).unwrap_or(0);
    format!("tools: {tools_desc} → {bytes} B")
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p praxec-agents elided_descriptor_names_the_tool_and_args`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/praxec-agents/src/tool_budget.rs
git commit -m "feat(agents): model-free elided_descriptor for the recall ledger"
```

---

### Task 2: goal-message recall ledger (find-or-create, extend in place)

**Files:**
- Modify: `crates/praxec-agents/src/tool_budget.rs`
- Test: same file (`mod tests`)

**Interfaces:**
- Produces:
  - `fn goal_text(history: &[Message]) -> String` — the text of the goal message (history[0]); empty if absent.
  - `pub(crate) fn elided_count(history: &[Message]) -> usize` — how many turns have already been elided this run (one ledger line each), so Task 3's index stays monotonic across calls.
  - `pub(crate) fn append_to_goal_ledger(history: &mut Vec<Message>, line: &str)` — append one ledger line to the goal, creating the marked section on first use and extending it in place thereafter.

- [ ] **Step 1: Write the failing test** (in `mod tests`)

```rust
#[test]
fn the_ledger_is_one_section_extended_in_place() {
    let mut history = vec![Message::user("GOAL")];
    append_to_goal_ledger(&mut history, "elided #1 · tools: a → 5 B · recall: spill_read slot=H1");
    append_to_goal_ledger(&mut history, "elided #2 · tools: b → 6 B · recall: spill_read slot=H2");
    let g = goal_text(&history);
    assert_eq!(
        g.matches("--- recallable elided history").count(),
        1,
        "exactly one ledger section, extended in place; got:\n{g}"
    );
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p praxec-agents the_ledger_is_one_section_extended_in_place`
Expected: FAIL — `cannot find function append_to_goal_ledger`.

- [ ] **Step 3: Write the implementation** (place above `enforce_history_budget`)

```rust
/// Header marking the single recall-ledger section appended to the goal message.
const LEDGER_HEADER: &str = "\n\n--- recallable elided history (use spill_read with the slot) ---";

/// The text of the goal message (history[0]). Empty when history is empty or the
/// first message is not a text user message.
fn goal_text(history: &[Message]) -> String {
    use rig::message::UserContent;
    match history.first() {
        Some(Message::User { content, .. }) => content
            .iter()
            .find_map(|c| match c {
                UserContent::Text(t) => Some(t.text.clone()),
                _ => None,
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// How many turns have already been elided this run — one `elided #` line per
/// turn in the goal's ledger. Lets the elision index stay monotonic across the
/// many per-turn budget passes.
pub(crate) fn elided_count(history: &[Message]) -> usize {
    goal_text(history).matches("\nelided #").count()
}

/// Append one ledger line to the goal message (history[0]). Creates the marked
/// section on first use and extends it in place thereafter — so across many
/// budget passes there is exactly one ledger that grows by one line per elision.
pub(crate) fn append_to_goal_ledger(history: &mut Vec<Message>, line: &str) {
    let mut text = goal_text(history);
    if !text.contains(LEDGER_HEADER) {
        text.push_str(LEDGER_HEADER);
    }
    text.push('\n');
    text.push_str(line);
    if let Some(first) = history.first_mut() {
        *first = Message::user(text);
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p praxec-agents the_ledger_is_one_section_extended_in_place`
Expected: PASS.

- [ ] **Step 5: Add a monotonic-index test**

```rust
#[test]
fn elided_count_tracks_appended_ledger_lines() {
    let mut history = vec![Message::user("GOAL")];
    assert_eq!(elided_count(&history), 0);
    append_to_goal_ledger(&mut history, "elided #1 · tools: a → 5 B · recall: spill_read slot=H1");
    assert_eq!(elided_count(&history), 1);
}
```
Run: `cargo test -p praxec-agents elided_count_tracks_appended_ledger_lines`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/praxec-agents/src/tool_budget.rs
git commit -m "feat(agents): goal-message recall ledger (find-or-create, extend in place)"
```

---

### Task 3: rewrite `enforce_history_budget` → async, spill + ledger

**Files:**
- Modify: `crates/praxec-agents/src/tool_budget.rs` (replace the body of `enforce_history_budget`; extend the `use crate::spill::…` import)
- Test: same file (`mod tests`)

**Interfaces:**
- Consumes: `elided_descriptor` (Task 1), `append_to_goal_ledger` + `elided_count` (Task 2), `SpillStore::put` (`crates/praxec-agents/src/spill.rs`), `history_bytes` + `HISTORY_KEEP_RECENT_MSGS` (existing in this file).
- Produces: `pub(crate) async fn enforce_history_budget(history: &mut Vec<Message>, budget: usize, store: &dyn SpillStore)` — same elision policy as before (oldest pairs first, keep goal + recent-N), but each elided pair is `put()` into `store` (lossless) and recorded as a ledger line on the goal instead of dropped.

- [ ] **Step 1: Extend the spill import**

The file already has `use crate::spill::SpillStore;`. Confirm it is present; no change needed if so. (It is used by `spill_on_ingress`.)

- [ ] **Step 2: Write the failing test** (in `mod tests`)

```rust
#[tokio::test]
async fn enforce_history_budget_spills_the_oldest_pair_recall_ably() {
    use rig::completion::AssistantContent;
    use rig::message::{ToolResult, ToolResultContent, UserContent};
    use rig::OneOrMany;
    let store = MemSpillStore::new();
    // goal + 3 turn-pairs (7 msgs). A tiny budget forces eliding the oldest.
    let mut history = vec![Message::user("GOAL")];
    for i in 0..3 {
        history.push(Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::tool_call(
                format!("c{i}"),
                "read_file",
                json!({ "n": i }),
            )),
        });
        history.push(Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: format!("c{i}"),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text("X".repeat(500))),
            })),
        });
    }
    enforce_history_budget(&mut history, 300, &store).await;
    // The ledger names a slot; reading it back yields the exact elided turn.
    let g = goal_text(&history);
    let slot = g
        .split("slot=")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .expect("a ledger line with a slot");
    let recalled = store.get(slot, 0, 1_000_000).await.unwrap();
    assert!(
        recalled.contains("read_file"),
        "the spilled slot must round-trip to the elided turn; got: {recalled}"
    );
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p praxec-agents enforce_history_budget_spills_the_oldest_pair_recall_ably`
Expected: FAIL — `enforce_history_budget` takes 2 args / is not async (compile error).

- [ ] **Step 4: Replace the `enforce_history_budget` body**

Replace the existing function:
```rust
pub(crate) fn enforce_history_budget(history: &mut Vec<Message>, budget: usize) {
    while history_bytes(history) > budget
        && history.len() >= 3
        && history.len() > HISTORY_KEEP_RECENT_MSGS + 1
    {
        history.remove(1); // oldest assistant (carries the tool_calls)
        history.remove(1); // its paired user message (the matching tool_results)
    }
}
```
with:
```rust
pub(crate) async fn enforce_history_budget(
    history: &mut Vec<Message>,
    budget: usize,
    store: &dyn SpillStore,
) {
    while history_bytes(history) > budget
        && history.len() >= 3
        && history.len() > HISTORY_KEEP_RECENT_MSGS + 1
    {
        // Elide the oldest pair, but SPILL it (lossless + addressable) instead
        // of dropping it: the whole [assistant(tool_calls), user(tool_results)]
        // pair goes to one slot, and a compact model-free ledger line on the
        // goal tells the agent how to recall it via spill_read.
        let assistant = history.remove(1);
        let user = history.remove(1);
        let descriptor = elided_descriptor(&assistant, &user);
        let payload = serde_json::to_string(&[&assistant, &user]).unwrap_or_default();
        let slot = store.put(payload).await;
        let index = elided_count(history) + 1;
        let line =
            format!("elided #{index} · {descriptor} · recall: spill_read slot={slot}");
        append_to_goal_ledger(history, &line);
    }
}
```

- [ ] **Step 5: Run to verify it passes**

Run: `cargo test -p praxec-agents enforce_history_budget_spills_the_oldest_pair_recall_ably`
Expected: PASS.

- [ ] **Step 6: Add the structural-invariant test**

```rust
#[tokio::test]
async fn enforce_history_budget_keeps_alternation_after_eliding() {
    use rig::completion::AssistantContent;
    use rig::message::{ToolResult, ToolResultContent, UserContent};
    use rig::OneOrMany;
    let store = MemSpillStore::new();
    let mut history = vec![Message::user("GOAL")];
    for i in 0..3 {
        history.push(Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::tool_call(
                format!("c{i}"), "read_file", json!({ "n": i }),
            )),
        });
        history.push(Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: format!("c{i}"), call_id: None,
                content: OneOrMany::one(ToolResultContent::text("X".repeat(500))),
            })),
        });
    }
    enforce_history_budget(&mut history, 300, &store).await;
    let starts_with_user = matches!(history.first(), Some(Message::User { .. }));
    assert!(starts_with_user, "history must still start with the user goal");
}
```
Run: `cargo test -p praxec-agents enforce_history_budget_keeps_alternation_after_eliding`
Expected: PASS.

- [ ] **Step 7: Add the retention test (goal + recent-N never spilled)**

```rust
#[tokio::test]
async fn enforce_history_budget_never_spills_the_goal_or_recent_turns() {
    use rig::completion::AssistantContent;
    use rig::message::{ToolResult, ToolResultContent, UserContent};
    use rig::OneOrMany;
    let store = MemSpillStore::new();
    let mut history = vec![Message::user("GOAL")];
    for i in 0..3 {
        history.push(Message::Assistant {
            id: None,
            content: OneOrMany::one(AssistantContent::tool_call(
                format!("c{i}"), "read_file", json!({ "n": i }),
            )),
        });
        history.push(Message::User {
            content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                id: format!("c{i}"), call_id: None,
                content: OneOrMany::one(ToolResultContent::text("X".repeat(500))),
            })),
        });
    }
    // A budget far below any single pair would still never strip the goal +
    // recent-N: the length guard stops at HISTORY_KEEP_RECENT_MSGS + 1.
    enforce_history_budget(&mut history, 1, &store).await;
    assert!(
        history.len() >= HISTORY_KEEP_RECENT_MSGS + 1,
        "goal + recent-N must survive even an impossibly small budget; len={}",
        history.len()
    );
}
```
Run: `cargo test -p praxec-agents enforce_history_budget_never_spills_the_goal_or_recent_turns`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add crates/praxec-agents/src/tool_budget.rs
git commit -m "feat(agents): enforce_history_budget spills elided turns (lossless) + ledger"
```

---

### Task 4: wire the new signature into `run()` and verify end-to-end

**Files:**
- Modify: `crates/praxec-agents/src/rig_runner.rs:414` (call site)
- Test: `crates/praxec-agents/src/rig_runner/tests.rs` (re-run/adjust the existing budget test)

**Interfaces:**
- Consumes: `enforce_history_budget(history, budget, store).await` (Task 3) and the per-run `spill` local already created in `run()` (`let spill = crate::spill::InMemorySpillStore::new();`).

- [ ] **Step 1: Update the call site**

In `crates/praxec-agents/src/rig_runner.rs`, line ~414, replace:
```rust
                enforce_history_budget(&mut history, self.max_history_bytes);
```
with:
```rust
                enforce_history_budget(&mut history, self.max_history_bytes, &spill).await;
```
(`spill` is declared before `loop_fut`, and `loop_fut` is an `async {}` block, so `.await` is in scope and `&spill` is captured by reference.)

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p praxec-agents`
Expected: success (no signature/await errors).

- [ ] **Step 3: Run the existing end-to-end budget test**

Run: `cargo test -p praxec-agents the_history_budget_bounds_the_request_across_a_long_tool_loop`
Expected: PASS, **unchanged**. This test only asserts (a) the loop still completes, (b) it ran ≥ TURNS turns, and (c) `max_history_bytes <= BUDGET + 2*CHUNK` (= 48 KB). It does NOT assert elided content is absent. The new ledger adds ~80 B per elision to the goal (≤ ~1.3 KB over the run), which the guard compensates for by eliding other pairs to keep `history_bytes <= BUDGET` — so the measured max stays inside the existing `+2*CHUNK` margin. No edit needed; if it unexpectedly fails on the bound, that is a real regression, not an assertion to relax.

- [ ] **Step 4: Run the full agents suite + clippy**

Run: `cargo test -p praxec-agents`
Expected: PASS (all prior tests + the 5 new ones).
Run: `cargo clippy -p praxec-agents --lib --bins -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add crates/praxec-agents/src/rig_runner.rs crates/praxec-agents/src/rig_runner/tests.rs
git commit -m "feat(agents): wire per-run spill store into the history budget guard"
```

---

## Notes for the implementer

- **Termination:** the `while` loop still stops on the length guard (`history.len() > HISTORY_KEEP_RECENT_MSGS + 1`), so even if a tiny budget is smaller than the goal+ledger alone, it cannot infinite-loop — it stops once only the goal + recent-N remain (identical bound to the pre-change code).
- **Why the ledger lives on the goal:** the goal is index 0 and is never elided, and appending there (rather than inserting a new message) preserves strict user/assistant alternation + `tool_call`↔`tool_result` pairing that providers require.
- **`OneOrMany::iter()`** is the rig API used by `history_bytes` already; `.iter()` over message content is available.
- **No new tool / store / model call** — recall reuses the already-injected `spill_read`; spilling reuses the per-run `InMemorySpillStore`.
