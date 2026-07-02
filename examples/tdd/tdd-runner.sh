#!/usr/bin/env bash
#
# tdd-runner.sh — adapter that runs a test command and emits a JSON object
# the gateway can read. The TDD workflow gates on three signals:
#
#   passed  (bool)  — did the test command exit 0?
#   count   (int)   — how many tests are currently in the suite?
#   output  (str)   — combined stdout/stderr from the test command
#
# Configure two commands via env vars (or pass them as args):
#
#   TDD_TEST_CMD   what to run for the suite          (e.g. "cargo test --quiet")
#   TDD_COUNT_CMD  what to run to print the test count to stdout
#                                                    (e.g. "cargo test --quiet -- --list 2>/dev/null | grep -c ': test$'")
#
# The runner ALWAYS exits 0 — the gateway reads `passed` from the JSON,
# not the wrapper's exit code. Use `treatNonZeroAsFailure: false` on the
# CLI executor so the gateway lets the workflow branch on the JSON.
#
# Example:
#   TDD_TEST_CMD="cargo test --quiet" \
#   TDD_COUNT_CMD="cargo test --quiet -- --list 2>/dev/null | grep -c ': test$'" \
#       ./tdd-runner.sh

set +e

test_cmd="${1:-${TDD_TEST_CMD:-}}"
count_cmd="${2:-${TDD_COUNT_CMD:-}}"

if [[ -z "$test_cmd" ]]; then
  echo '{"passed":false,"count":0,"output":"tdd-runner: TDD_TEST_CMD not set","error":"missing test command"}'
  exit 0
fi

if [[ -z "$count_cmd" ]]; then
  count=0
else
  count=$(eval "$count_cmd" 2>/dev/null | tail -n 1 | tr -d '[:space:]')
  count=${count:-0}
  if ! [[ "$count" =~ ^[0-9]+$ ]]; then
    count=0
  fi
fi

# Run the test command, capture combined output + exit code.
output=$(eval "$test_cmd" 2>&1)
exit_code=$?

# Emit JSON. jq isn't always installed, so build it manually with python3 if
# present (handles escaping safely); otherwise fall back to a best-effort
# escape pass.
passed_lit="false"
[[ "$exit_code" -eq 0 ]] && passed_lit="true"

if command -v python3 >/dev/null 2>&1; then
  python3 - "$passed_lit" "$count" <<'PY' "$output"
import json, sys
passed = sys.argv[1] == "true"
count = int(sys.argv[2])
output = sys.stdin.read()
sys.stdout.write(json.dumps({"passed": passed, "count": count, "output": output}))
PY
else
  # Best-effort manual escape (newlines + quotes + backslashes).
  esc=$(printf '%s' "$output" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' | awk 'BEGIN{ORS=""}{ print (NR>1 ? "\\n" : "") $0 }')
  printf '{"passed":%s,"count":%s,"output":"%s"}\n' "$passed_lit" "$count" "$esc"
fi
