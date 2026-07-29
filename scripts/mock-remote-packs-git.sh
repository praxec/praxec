#!/usr/bin/env bash
set -euo pipefail

# Mocks the two `uri:` git remotes that examples/remote-packs/gateway.yaml
# sources packs from, so `praxec check` can validate the FULL example
# offline instead of skipping it (see .github/workflows/ci.yml's "schema
# check on examples" step). The hermetic Rust equivalent of this same
# mechanism lives in
# crates/praxec-core/tests/remote_example_validates.rs.
#
# The example's `repos:` entries are:
#   - uri: "git+https://github.com/praxec/cognitive-architectures"  ref: main
#   - uri: "git+https://github.com/praxec/praxec-meta"              ref: v1.0.0
#
# `praxec-core::repo_git::clone_url` strips only the `git+` scheme prefix
# before shelling out to `git` — so the exact URL string praxec passes to
# `git remote add origin <url>` is the `https://...` form with `git+`
# removed and nothing else changed. This script builds one bare repo per
# mock pack fixture (under examples/remote-packs/_mock/<pack>/, committed
# to this repo — minimal but genuinely valid packs) and registers a GLOBAL
# git `url.<bare>.insteadOf <exact-url>` redirect for each, so
# `clone_or_update`'s `fetch`/`reset --hard` calls transparently resolve to
# the local bare repo instead of touching the network. The example file
# itself is never modified — only the git transport is mocked.
#
# Run this BEFORE the `praxec check` sweep. Safe to re-run: each mock repo
# is rebuilt into a fresh temp dir every invocation (the previous
# `insteadOf` entries are simply overwritten by `git config --global`,
# which replaces a single-value key).

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mock_root="$repo_root/examples/remote-packs/_mock"
work_dir="$(mktemp -d)"

# Build a bare git repo at `bare_dir` seeded with the contents of
# `fixture_dir`, optionally tagging the single commit `tag`.
build_bare_repo() {
    local fixture_dir="$1"
    local bare_dir="$2"
    local tag="${3:-}"

    local seed_dir
    seed_dir="$(mktemp -d)"
    cp -r "$fixture_dir"/. "$seed_dir"/
    git -C "$seed_dir" init --quiet -b main
    git -C "$seed_dir" add -A
    git -C "$seed_dir" \
        -c user.email=ci@praxec.local -c user.name="praxec CI mock" \
        commit --quiet -m "mock pack seed"
    if [[ -n "$tag" ]]; then
        git -C "$seed_dir" tag "$tag"
    fi
    git clone --quiet --bare "$seed_dir" "$bare_dir"
}

cog_bare="$work_dir/cognitive-architectures.git"
meta_bare="$work_dir/praxec-meta.git"

build_bare_repo "$mock_root/cognitive-architectures" "$cog_bare"
build_bare_repo "$mock_root/praxec-meta" "$meta_bare" "v1.0.0"

# These MUST match repo_git::clone_url()'s output for the example's two
# `uri:` values exactly (the `git+` prefix stripped, nothing else).
git config --global url."file://$cog_bare".insteadOf "https://github.com/praxec/cognitive-architectures"
git config --global url."file://$meta_bare".insteadOf "https://github.com/praxec/praxec-meta"

echo "mocked git remotes (offline):"
echo "  https://github.com/praxec/cognitive-architectures -> file://$cog_bare"
echo "  https://github.com/praxec/praxec-meta              -> file://$meta_bare"
