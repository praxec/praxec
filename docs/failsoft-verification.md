# Fail-soft verification (feedback #4 / P13)

**Question (feedback #4):** does the gateway quarantine a broken def + serve the rest +
surface the diagnostic via MCP init/health, or still fail-whole (opaque `-32000` crash)?

**Verdict: fail-soft IS landed — it does NOT fail-whole-crash.**

## Evidence
- `praxec check` on a config with one broken workflow (a dangling transition target) +
  one good workflow: reports a **precise, machine-actionable error** — `transition 'go'
  in state 'start' targets 'nonexistent_state' which is not in states` — plus reachability
  warnings, still lists the good workflow, exits cleanly. No panic, no crash.
- Runtime (`crates/praxec-mcp-server/src/degraded.rs`): a config fault brings up a
  **`DegradedServer`** that completes the MCP handshake and answers every call with a
  self-documenting `HealthReport` (the exact fault + where + how to fix), so an LLM
  operator can self-heal (e.g. `meta/flow.repair-workflow-health`). Explicitly **not a
  fallback** — the degraded server does zero governed work; it refuses loudly with a
  diagnostic instead of a bare transport error.

## Honest nuance
This is fail-soft-to-**degraded-whole**, not partial-serve/quarantine-one-def: a structural
Error anywhere in the config rejects the WHOLE config to the degraded state (the mint/reload
gate refuses to swap in a config carrying Error diagnostics — same policy at boot and on
hot-reload). That is a safe design (refuse-all-loudly-with-diagnostic > partial-serve-silently),
but if per-def quarantine ("serve the good defs, isolate the bad one") is desired, that is a
distinct future feature — the current guarantee is "no fail-whole crash; degraded + diagnostic."
