#!/usr/bin/env bash
# Tranche 2a — composition smoke driver (NO API key required).
#
# Drives examples/smoke-ete/gateway.yaml end-to-end through the
# in-process runtime + real executor registry (parallel + pipeline +
# noop). No live LLM. Fails loud on any composition gap.
#
# Run from workspace root:
#   ./examples/smoke-ete/walk.sh
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

echo "== Tranche 2a: ETE composition smoke =="
echo

echo "[1/3] Validate config..."
cargo run --quiet --bin px -- check --config examples/smoke-ete/gateway.yaml

echo
echo "[2/3] Run ete_smoke integration test..."
cargo test --quiet -p praxec-executors --test ete_smoke

echo
echo "[3/3] Audit-trail summary"
echo "  - parallel.fanout.completed asserted"
echo "  - pipeline.completed asserted"
echo "  - ask_human injection asserted on every non-terminal state"
echo "  - path_allowlist rejection asserted"
echo
echo "✓ Composition smoke passed. v0.4 primitives compose end-to-end."
