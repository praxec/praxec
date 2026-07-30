# Mock-validate the remote-packs example offline instead of skipping it

## The regression this fixes

Commit `54db460` added a `case ... examples/remote-packs/*) ... continue`
skip to the "schema check on examples" step in `.github/workflows/ci.yml`
because `examples/remote-packs/gateway.yaml` sources two packs over real
network git remotes and CI must stay offline. Skipping meant the file's
*other* potential errors (bad workflow refs, schema drift, broken guards)
went permanently unvalidated. That skip is now removed — the sweep is back
to the plain `for f in examples/*.yaml examples/*/gateway.yaml; do ... done`
loop, with no `remote-packs` special case. The file is now validated by
mocking the git *transport* only, not the config's content.

## The exact URL string praxec passes to `git`

`crates/praxec-core/src/repo_git.rs::clone_url`:

```rust
pub fn clone_url(uri: &str) -> String {
    uri.strip_prefix("git+")
        .map(str::to_string)
        .unwrap_or_else(|| uri.to_string())
}
```

It strips **only** the `git+` scheme prefix — nothing else. `config.rs`'s
`repos:` parsing (`take_repo_entry`, ~line 4197) reads `uri:` as a bare
string and passes it straight through to
`repo_git::clone_or_update(&uri, &gitref, &dest)`, which calls `clone_url`
internally before `git remote add origin <url>` (both the cold-clone and
warm-update paths — `clone_or_update` never calls `git clone`, only
`init`/`remote add`/`fetch`/`reset --hard FETCH_HEAD`).

For the two `uri:`s in the example:

| `uri:` in the example | exact string passed to `git` |
|---|---|
| `git+https://github.com/praxec/cognitive-architectures` | `https://github.com/praxec/cognitive-architectures` |
| `git+https://github.com/praxec/praxec-meta` | `https://github.com/praxec/praxec-meta` |

These are the exact values a `git config --global url."file://<bare>".insteadOf "<value>"` redirect must match. Locked in by a dedicated test:
`clone_url_strips_git_prefix_for_the_remote_packs_example_uris` in
`crates/praxec-core/tests/remote_example_validates.rs`.

## Mock fixtures

`examples/remote-packs/_mock/`:
- `cognitive-architectures/` — `praxec.repo.yaml` (`schema: praxec.repo/v1`, `namespace: cognitive`) + `flows/flow.mock-hello.yaml` (one trivial `terminal: true` workflow).
- `praxec-meta/` — same shape, `namespace: meta`.

Both namespaces (`cognitive`, `meta`) match the real packs' actual
namespaces (cross-checked against the full fixtures already in
`crates/praxec-core/tests/fixtures/{cognitive-architectures,praxec-meta}/`
used by other tests) — the mocks are deliberately minimal, not full
replicas, but namespace-consistent. The shape (manifest + one
`workflows:` entry with a single `terminal: true` state) mirrors the
proven-valid `crates/praxec-core/tests/fixtures/repos/swe-core` fixture.

## CI setup

New step `mock remote-packs git remotes (offline)` in `.github/workflows/ci.yml`,
inserted **before** the "schema check on examples" step, runs
`scripts/mock-remote-packs-git.sh`. That script:
1. Builds one bare git repo per mock pack fixture (seed → `git init`/`add`/`commit`
   → for `praxec-meta`, also `git tag v1.0.0` → `git clone --bare` into a temp dir).
2. Registers `git config --global url."file://<bare-cog>".insteadOf "https://github.com/praxec/cognitive-architectures"` and the equivalent for `praxec-meta`.

The existing sweep then runs `./target/release/praxec check --config examples/remote-packs/gateway.yaml` completely unmodified — `git`'s own URL-rewriting machinery transparently redirects the two `fetch`es to local bare repos. The committed example file is untouched; only the git transport is mocked.

Also added `.praxec/` to `.gitignore` — the `uri:`-repo clone cache
(`<host_dir>/.praxec/repos/<slug>`) that both CI and local `praxec check`
runs against this example now create.

## Hermetic Rust test (red → green)

`crates/praxec-core/tests/remote_example_validates.rs`, two tests:

1. `clone_url_strips_git_prefix_for_the_remote_packs_example_uris` — locks in the exact-string mapping above.
2. `remote_packs_example_validates_fully_offline_via_mocked_git_transport`:
   - Reads the **real, unmodified** `examples/remote-packs/gateway.yaml` text.
   - Writes a private temp copy with only the two `github.com` `uri:` hosts rewritten to `file://<temp-bare-repo>` (same `ref:` pins, `main` / `v1.0.0`, untouched).
   - **RED**: calls `config::load_resolved_with_repos` (the same Strict path `praxec check` uses) against that temp copy *before* the bare repos exist on disk — asserts `Err` (proves the test isn't vacuously green).
   - Builds the two bare repos from the `_mock/` fixtures (tagging `v1.0.0` on the `praxec-meta` one).
   - **GREEN**: the identical load now asserts `Ok`, 0 diagnostics, and that both `cognitive/flow.mock-hello` and `meta/flow.mock-hello` are present in the resolved `workflows` map.

This test does NOT touch global git config (a per-process global-config edit
would race other tests in the same binary) — it rewrites the `uri:` host
in a private temp file instead, which exercises the identical
`clone_or_update` → `merge_declared_repos` → `load_resolved_with_repos`
path with zero network access, hermetically and parallel-test-safely.

Confirmed by direct run: both tests pass (`cargo test -p praxec-core --test remote_example_validates` → `2 passed; 0 failed`).

## Comment/doc updates

- `examples/remote-packs/gateway.yaml` (former lines ~17-21): no longer claims exclusion from the sweep; now states it IS validated via a mocked git transport, and points at the mock fixtures + CI script + hermetic test.
- `docs/remote-pack-sourcing.md`: same correction — the "That file isn't swept by CI's example-validation test on purpose" claim is replaced with an explanation of the mock mechanism.

## Verification performed

1. **CI-step simulation, offline, disconnect-proof**: ran
   `bash scripts/mock-remote-packs-git.sh` locally (built two ephemeral bare
   repos, set global `url.insteadOf`), then ran
   `http_proxy=http://127.0.0.1:1 https_proxy=http://127.0.0.1:1 GIT_TERMINAL_PROMPT=0 ./target/debug/praxec check --config examples/remote-packs/gateway.yaml`
   — a poisoned HTTP(S) proxy as a network canary (any real network attempt
   would hang/fail against it; `file://` URLs never touch it). Result:
   ```
   config version: 1.0.0
   config: examples/remote-packs/gateway.yaml
   workflows (2):
     - cognitive/flow.mock-hello
     - meta/flow.mock-hello
   validation: ok
   ```
   Exit code 0, **0 errors**, fully offline.
2. Removed the global `url.insteadOf` entries afterward (`git config --global --get-regexp '^url\.'` → empty) and re-ran `cargo test -p praxec-core --test remote_example_validates` — both tests still pass, confirming the Rust test is hermetic and independent of any global git config left behind by the CI-simulation step.
3. `cargo fmt --all -- --check` — clean.
4. `cargo clippy -p praxec-core --tests -- -D warnings` — clean. Also ran full `cargo clippy --workspace --all-targets -- -D warnings` — clean (only a pre-existing third-party `proc-macro-error2` future-incompat note, unrelated).
5. `cargo test --workspace` — **all green**, including the 2 new tests in `remote_example_validates.rs` (240 `test result: ok` blocks across the workspace, 0 failures).

## Commit

`test(ci): mock-validate the remote-packs example offline instead of skipping it` on `feat/pack-remote-ergonomics`.
