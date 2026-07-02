# Example: TDD enforcement workflow (with anti-cheating defenses)

A real example of using praxec to enforce red → green → refactor
discipline against an LLM agent that may try to cheat. It's all
declarative — one YAML file plus a small wrapper script — and the
runtime catches the obvious cheating modes.

> **Honest status, 2026-05.**
> The mechanical dogfood (`dogfood-drive.py`) verifies: the config
> loads, the MCP handshake succeeds, `workflow.start` and the first
> `workflow.submit` work, and the runner emits parseable JSON. What's
> *not* verified: a live LLM navigating the full red → green → refactor
> cycle. Treat this as a runtime-expressiveness demo until you've vetted
> it with your specific agent + model.

## What this shows

This example pressure-tests the gateway against the failure modes a
real TDD enforcer has to defend against:

- The agent **deletes the failing test** to make the suite pass.
- The agent **trivializes the test** (e.g. `assert True`).
- The agent **writes the implementation first** then claims a TDD cycle.
- The agent **lies about the test result**.
- The agent **uses tools outside the workflow** to bypass it.

## Files

- `gateway.yaml` — the workflow definition.
- `tdd-runner.sh` — bash wrapper that runs your test command and emits
  `{passed: bool, count: int, output: str}`.

## How it works

The workflow tracks two baselines:

- **`baseline_count`** — the suite's test count at the start of *this*
  cycle. Rolls forward after every successful green.
- **`session_baseline_count`** — captured **once** at the first
  `start_cycle` and **never rolls back**. Dropping below it means tests
  from session start were removed.

```text
    idle
      │ start_cycle
      ▼
    red_pending
      │ confirm_red
      │   count < session_baseline       →  cheated
      │   count < baseline_count         →  cheated
      │   tests fail AND count > baseline →  red
      │   else                            →  red_pending
      ▼
    red
      │ confirm_green
      │   count < session_baseline                    →  cheated
      │   pass AND count ≤ baseline                   →  cheated
      │   pass AND count > baseline                   →  green
      │   fail                                        →  red
      ▼
    green
      │   start_new_cycle    →  red_pending
      │   start_refactor     →  refactoring
      │   finish             →  done   (gated: cycles_completed ≥ 1)
      ▼
    refactoring
      │ confirm_refactor
      │   count < session_baseline   →  cheated
      │   count < baseline           →  cheated
      │   pass AND count ≥ baseline  →  green
      │   fail                       →  refactoring
      ▼
    done   ‖   cheated   (terminal)
```

## Failure modes

| What an agent might try | How the workflow catches it |
|---|---|
| Pretend tests passed without running them | Runner is the source of truth; `output.json.passed` comes from the runner |
| Delete a pre-existing test | `last_count < session_baseline_count` → `cheated` |
| Delete the new failing test during impl | `passed == true` AND `count ≤ baseline` → `cheated` |
| Disable / skip the failing test | Same as deletion — count drops → `cheated` |
| Slow cross-cycle erosion (add 1, delete 1) | `session_baseline_count` never rolls back |
| Skip writing a failing test | `confirm_red` requires `count > baseline_count` (strict) |
| Write implementation first | `confirm_red` requires `passed == false` |
| Trivialize the assertion (`assert True`) | **Not catchable by counts alone** — mutation testing territory |

## Hardening

Two open doors that are closed by config changes, not code changes:

1. **Agent controls the test commands.** The example's `inputSchema`
   accepts `test_cmd`, `count_cmd`, `runner_path` at start. For real
   enforcement, hardcode these in the YAML so the agent has no say.

2. **Agent uses tools outside the gateway.** The workflow only governs
   actions taken *through it*. Lock the agent's MCP host to this
   gateway as its only tool source, or accept the gateway as auditor
   rather than enforcer.

## Running the example

```bash
# 1. Validate
praxec check --config examples/tdd/gateway.yaml

# 2. Test the runner directly
chmod +x examples/tdd/tdd-runner.sh
TDD_TEST_CMD="cargo test --quiet" \
TDD_COUNT_CMD="cargo test --quiet -- --list 2>/dev/null | grep -c ': test\$'" \
  examples/tdd/tdd-runner.sh

# 3. Serve
praxec serve --config examples/tdd/gateway.yaml
```

Wire into your MCP host:

```json
{
  "mcpServers": {
    "tdd-gate": {
      "command": "/abs/path/praxec",
      "args": ["serve", "--config", "/abs/path/examples/tdd/gateway.yaml"]
    }
  }
}
```

## Where to read more

- [`docs/architecture/concepts.md`](../../docs/architecture/concepts.md) — mental model
- [`docs/reference/governance.md`](../../docs/reference/governance.md) — guards, branches
- [`docs/reference/configuration.md`](../../docs/reference/configuration.md) — full config reference
