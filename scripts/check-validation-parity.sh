#!/usr/bin/env bash
# SPEC §11 / PR3 — every validation rule V1..V23 MUST have at least one
# accepts test AND at least one rejects test, named per the convention
# `fn v<N>_(accepts|rejects)_<topic>`. This scanner finds gaps before
# they ship.
#
# Run from the repo root. Exits non-zero with a named gap list on
# failure; silent + zero exit on success.

set -euo pipefail

ROOT_DIR="${ROOT_DIR:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT_DIR"

# All rule numbers the design contract requires coverage for.
MAX_RULE=23
MIN_RULE=1

# Rules that were deliberately RELAXED to accept-only — they no longer reject
# anything, so a `rejects` test cannot meaningfully exist. Exempt them from the
# rejects requirement (accepts coverage is still required).
#   V11: flows-must-not-invoke-flows was relaxed so flows may nest other flows
#        via `kind: workflow` (the loom composition); see validate.rs (Tier::Flow)
#        and the `v11_now_allows_flow_invoking_another_flow` test.
REJECTS_EXEMPT=(11)
is_rejects_exempt() {
  local n="$1"
  for e in "${REJECTS_EXEMPT[@]}"; do
    [ "$e" = "$n" ] && return 0
  done
  return 1
}

# Search the integration test trees of every crate that participates in
# validation-rule coverage. Add more paths here as new crates host tests.
SEARCH_PATHS=(
  "crates/praxec-core/tests"
  "crates/praxec-executors/tests"
)

missing=()
for n in $(seq $MIN_RULE $MAX_RULE); do
  rule="v${n}"
  accepts_count=0
  rejects_count=0
  for p in "${SEARCH_PATHS[@]}"; do
    if [ -d "$p" ]; then
      # `-h` suppresses filename; `-c` counts matches per-file; sum with awk.
      # `|| true` neutralises grep's exit-1 on zero matches under pipefail.
      accepts_count=$((accepts_count + $(grep -rhE "fn ${rule}_accepts_" "$p" 2>/dev/null | wc -l || true)))
      rejects_count=$((rejects_count + $(grep -rhE "fn ${rule}_rejects_" "$p" 2>/dev/null | wc -l || true)))
    fi
  done
  if [ "$accepts_count" -lt 1 ]; then
    missing+=("V${n}: no accepts test")
  fi
  if [ "$rejects_count" -lt 1 ] && ! is_rejects_exempt "$n"; then
    missing+=("V${n}: no rejects test")
  fi
done

if [ "${#missing[@]}" -gt 0 ]; then
  echo "validation-parity scanner: missing coverage" >&2
  for m in "${missing[@]}"; do
    echo "  - $m" >&2
  done
  echo >&2
  echo "Each rule V1..V${MAX_RULE} requires at least one fn v<N>_accepts_* AND one fn v<N>_rejects_*" >&2
  echo "test. Search paths:" >&2
  for p in "${SEARCH_PATHS[@]}"; do
    echo "  - $p" >&2
  done
  exit 1
fi
