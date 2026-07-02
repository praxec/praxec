#!/usr/bin/env bash
set -euo pipefail

echo "=== praxec Zed gateway verification ==="

# 1. Config compiles
echo "-> Checking config..."
cargo run -p praxec -- check --config examples/zed-gateway/gateway.yaml

# 2. Config has expected workflows
echo "-> Checking workflows..."
cargo run -p praxec -- check --config examples/zed-gateway/gateway.yaml \
  | grep -q "tdd" || { echo "FAIL: tdd workflow not found"; exit 1; }
cargo run -p praxec -- check --config examples/zed-gateway/gateway.yaml \
  | grep -q "governed_change" || { echo "FAIL: governed_change workflow not found"; exit 1; }

# 3. Config has expected proxy capabilities
echo "-> Checking proxy capabilities..."
cargo run -p praxec -- check --config examples/zed-gateway/gateway.yaml \
  | grep -q "fs.read" || { echo "FAIL: fs.read capability not found"; exit 1; }
cargo run -p praxec -- check --config examples/zed-gateway/gateway.yaml \
  | grep -q "fs.write" || { echo "FAIL: fs.write capability not found"; exit 1; }

echo "All checks passed"
echo ""
echo "To use with Zed, copy zed-settings.json into your Zed config:"
echo "  ~/.config/zed/settings.json"
