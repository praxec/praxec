#!/usr/bin/env bash
# Tranche 2b — LIVE smoke driver. Runs `px walk` against a real
# model via the production AetherSubAgentSpawner.
#
# Prerequisites:
#   1. ANTHROPIC_API_KEY (or matching env var for whichever provider
#      you wire in --agent ...) must be set.
#   2. `cargo build --release -p praxec -p praxec-tui`
#      so the `praxec` and `praxec-tui` binaries exist.
#   3. The `px doctor` subcommand passes (Tranche 3).
#
# What this exercises:
#   - The real rmcp child-process PraxecChildCaller
#   - The real AetherSubAgentSpawner
#   - The full v0.4 primitive surface against a live model
#
# Run from workspace root:
#   ANTHROPIC_API_KEY=... ./examples/smoke-ete/walk-live.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "ERROR: ANTHROPIC_API_KEY is not set. Live smoke requires a real model key."
  echo "Set it and re-run:"
  echo "  ANTHROPIC_API_KEY=sk-... $0"
  exit 1
fi

echo "== Tranche 2b: LIVE ETE smoke =="
echo

echo "[1/4] Build release binaries..."
cargo build --release -p praxec -p praxec-tui

PRAXEC_BIN="$(pwd)/target/release/praxec"
PRAXEC_TUI="$(pwd)/target/release/praxec-tui"

if [[ ! -x "$PRAXEC_BIN" ]]; then
  echo "ERROR: praxec binary not at $PRAXEC_BIN"
  exit 2
fi
if [[ ! -x "$PRAXEC_TUI" ]]; then
  echo "ERROR: praxec-tui binary not at $PRAXEC_TUI"
  exit 2
fi

echo
echo "[2/4] Pre-flight via px doctor (if available)..."
if "$PRAXEC_TUI" doctor \
    --workflow smoke_ete \
    --config examples/smoke-ete/gateway.yaml \
    --agent test=anthropic/claude-haiku-4-5-20251001 2>/dev/null; then
  echo "  doctor passed"
else
  echo "  (doctor not yet available — skipping pre-flight; falling through)"
fi

echo
echo "[3/4] Run px walk against live model..."
MCP_PRAXEC_PATH="$PRAXEC_BIN" \
  "$PRAXEC_TUI" walk \
  --workflow smoke_ete \
  --config examples/smoke-ete/gateway.yaml \
  --input '{"queries": ["alpha", "beta"]}' \
  --agent test=anthropic/claude-haiku-4-5-20251001 \
  --max-sub-agent-seconds 120 \
  --max-sub-agent-steps 30 \
  2>&1 | tee /tmp/praxec-walk-live.log

echo
echo "[4/4] Assert terminal state..."
if grep -q '"state":\s*"ship"' /tmp/praxec-walk-live.log; then
  echo "✓ Live walk reached terminal state 'ship'."
else
  echo "✗ Live walk did NOT reach terminal state 'ship'. See /tmp/praxec-walk-live.log"
  exit 3
fi

echo
echo "Live smoke passed. Real-LLM ETE coverage validated."
