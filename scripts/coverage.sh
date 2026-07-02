#!/usr/bin/env bash
# Per-crate line/region coverage via cargo-llvm-cov.
#
# Part of the "behavioral coverage of every execution type" effort
# (plan: okay-the-skill-executor-joyful-falcon). The goal is a behavioral
# assertion for every documented path/alternative; this script makes
# "covered" measurable. Any line left uncovered must be either (a) reachable
# and covered, or (b) annotated with a justification (intentional `panic!`,
# OS-syscall failure branches that can't be portably forced — e.g. chmod
# failure, a real MCP transport spawn).
#
# Usage:
#   scripts/coverage.sh            # workspace summary (text)
#   scripts/coverage.sh --html     # also write target/llvm-cov/html
#   scripts/coverage.sh -p praxec-executors   # one crate
set -euo pipefail
cd "$(dirname "$0")/.."

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "cargo-llvm-cov is not installed. Install it with:" >&2
  echo "    cargo install cargo-llvm-cov" >&2
  echo "    rustup component add llvm-tools-preview" >&2
  exit 127
fi

EXTRA=()
HTML=0
for arg in "$@"; do
  case "$arg" in
    --html) HTML=1 ;;
    *) EXTRA+=("$arg") ;;
  esac
done

# Default to the whole workspace if no -p/path filter was passed.
if [ "${#EXTRA[@]}" -eq 0 ]; then
  EXTRA=(--workspace)
fi

echo "=== cargo llvm-cov (${EXTRA[*]}) ==="
cargo llvm-cov "${EXTRA[@]}" --summary-only

if [ "$HTML" -eq 1 ]; then
  cargo llvm-cov "${EXTRA[@]}" --html
  echo "HTML report: target/llvm-cov/html/index.html"
fi
