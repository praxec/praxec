# Pack staleness/drift WARNING — build report

Branch: `feat/pack-staleness-warning` (off `dev`), worktree `/home/mc/working/mcp-flowgate-staleness`.

## What this closes

`docs/design/2026-07-29-workflow-currency-investigation.md` documented a real, live gap: this
session's own dogfood harness had `cognitive-architectures` loaded from a `path:` checkout sitting
on `feat/react-review-external-sync-discriminator` with uncommitted edits and 28 commits behind
`origin/dev` — and neither `praxec check` nor `serve` said a word. This change adds a non-blocking
WARNING for exactly that condition, at `check`/`doctor` time, for any git-backed `path:` pack.

## Assert-first: the behavioral assertions (red → green)

### Unit level — `crates/praxec-core/src/repo_git.rs` (`mod tests`, 8 new tests)

Exercise the new `git_currency(path) -> Option<GitCurrency>` primitive directly, with real
throwaway git repos built per-test under `tempfile::tempdir()`:

- `non_git_dir_is_none` — a plain tempdir → `None` (no warning, no error).
- `subdir_of_a_larger_repo_is_none` — `CARGO_MANIFEST_DIR` (a subdirectory of this very
  workspace's own git repo) → `None`. This is the guard that stops a plain-dir `path:` pack that
  happens to live *inside* an unrelated enclosing repo from spuriously inheriting that repo's
  branch/dirty state (see "Design decision" below — this is what makes the existing
  `multi_repo_loading.rs` fixtures, which live inside this workspace, stay silent).
- `clean_checkout_on_dev_has_no_drift_no_dirt_unknown_upstream` — on `dev`, clean, no `origin` →
  `on_default_branch: true`, `dirty: false`, `behind_upstream: None` (unknown, not zero).
- `feature_branch_checkout_is_off_default_branch` — `dev` exists locally but HEAD is on
  `feat/react-review` → `branch: Some("feat/react-review")`, `default_branch: "dev"`,
  `on_default_branch: false`.
- `detached_head_reports_no_branch` — checked out to a raw SHA → `branch: None`,
  `on_default_branch: false`.
- `uncommitted_changes_are_dirty` — an unstaged modification → `dirty: true`.
- `behind_known_upstream_reports_commit_count` — clone tracks `origin/dev`; origin advances one
  commit; the *test* does a local `git fetch` (arrange step only) → `behind_upstream: Some(1)`.

All ran red (function didn't exist) before `git_currency` was implemented; all green after.

### Config-merge level — `crates/praxec-core/tests/pack_staleness_warning.rs` (8 new tests)

Exercise the real load path, `load_resolved_with_repos`, whose soft-diagnostics return value is
the exact channel `check`/`doctor` already print from:

- `clean_pack_on_dev_has_no_currency_warning` — clean, on `dev` → `diagnostics.is_empty()`.
- `feature_branch_pack_warns_branch_drift_naming_the_branch` — asserts the `PACK_BRANCH_DRIFT`
  message contains both the actual branch (`feat/react-review`) and the default (`dev`).
- `detached_head_pack_warns_branch_drift` — detached HEAD also fires `PACK_BRANCH_DRIFT`.
- `dirty_pack_warns_dirty_tree` — an untracked file → `PACK_DIRTY_TREE`.
- `behind_upstream_pack_warns_with_commit_count` — asserts `PACK_BEHIND_UPSTREAM` names "1
  commit", and that a clean, on-branch, merely-behind pack does NOT also fire branch-drift/dirty.
- `composed_packs_on_different_branches_warn_mismatch` — base on `dev`, overlay on
  `feat/overlay-wip` → `PACK_COMPOSITION_BRANCH_MISMATCH` fires (alongside the overlay's own
  `PACK_BRANCH_DRIFT` — both classes can legitimately co-occur).
- `composed_packs_on_the_same_non_default_branch_have_no_mismatch_warning` — both packs
  intentionally on `feat/shared-wip` → no mismatch warning (each still gets its own
  `PACK_BRANCH_DRIFT`, but composition is consistent — not a cross-pack signal).
- `non_git_path_pack_has_no_warning_and_no_error` — a plain directory (no `.git`) with a valid
  manifest → `diagnostics.is_empty()`, load still succeeds.

### CLI / binary level — `crates/praxec/tests/pack_staleness_cli.rs` (2 new tests)

Run the real `praxec` binary end-to-end (`env!("CARGO_BIN_EXE_praxec")`):

- `check_warns_on_feature_branch_pack_but_still_exits_success` — a pack on
  `feat/react-review-external-sync-discriminator` (the exact branch name from the investigation
  doc's live repro) → `praxec check --config ...` exit status **success**, stdout contains
  `PACK_BRANCH_DRIFT` and the literal branch name.
- `check_on_clean_dev_pack_has_no_staleness_warning` — clean on `dev` → exit success, stdout
  contains none of the three warning codes.

## Where it hooks in

- `crates/praxec-core/src/repo_git.rs` — new `pub struct GitCurrency` + `pub fn
  git_currency(path: &Path) -> Option<GitCurrency>` (added after `push()`, before `mod tests`).
  Pure git plumbing, same module as `clone_or_update`/`push`, same `Command::new("git")`
  shell-out style, no new dependency.
- `crates/praxec-core/src/config.rs`:
  - `fn git_currency_diagnostics(entry_desc, currency) -> Vec<Diagnostic>` (~line 3055, just
    before `merge_declared_repos`) — turns one repo's `GitCurrency` into 0–3 WARN diagnostics.
  - `merge_declared_repos` (the function that already emits `REPO_LOAD_SKIPPED` /
    `STALE_OVERRIDE` etc.): captures `is_local_path` right where `entry_desc` is computed
    (~line 3199, before `source` is moved), collects `(entry_desc, GitCurrency)` into a new
    `git_currency_checks: Vec<...>` right after `repo_path` is resolved for a successfully-loaded
    entry (~line 3394, applies uniformly to both definition repos and FB-2 bare-writable
    targets), then after the `for` loop closes (~line 3453) emits the per-repo diagnostics plus
    one `PACK_COMPOSITION_BRANCH_MISMATCH` diagnostic if ≥2 git-backed local packs disagree on
    branch.
  - Reuses the existing `Diagnostic { severity, code, message, location, suggestion }` /
    `DiagnosticSeverity::Warn` channel — the same one `resolve_with_diagnostics` and
    `REPO_LOAD_SKIPPED`/`STALE_OVERRIDE` already populate. No parallel warnings channel.
- `crates/praxec/src/gateway.rs`'s `check()` (line 2402) already prints every entry from
  `soft_diagnostics` under a "soft warnings (resolve-time):" banner and folds their count into
  `validation: N error(s), M warning(s), K soft warning(s)` — **zero changes needed there**; the
  new diagnostics ride the existing print path for free. `doctor` shares the same
  `load_resolved_with_repos` load, so it inherits the same warnings.

## Git helpers reused

`crates/praxec-core/src/repo_git.rs`'s existing `Command::new("git").arg("-C").arg(cwd)` shelling
pattern (same style as `run_git`/`clone_or_update`/`push`) — two small new private helpers,
`git_ok` and `git_stdout`, follow that exact convention. No new dependency; `clone_or_update`
itself is untouched (remote `uri:` repos are explicitly skipped — see below).

## Exact warning messages

- `PACK_BRANCH_DRIFT`: `pack repo '<path>' is on <branch|a detached HEAD>, not its default branch
  '<dev|main>'[<upstream note>]` — suggestion: `git -C <path> checkout <default>`.
- `PACK_DIRTY_TREE`: `pack repo '<path>' has uncommitted changes[<upstream note>]` — suggestion:
  `git -C <path> status`.
- `PACK_BEHIND_UPSTREAM`: `pack repo '<path>' is <N> commit(s) behind origin/<default>` —
  suggestion: `git -C <path> pull`.
- `PACK_COMPOSITION_BRANCH_MISMATCH`: `composed git-backed packs are on different branches:
  <path>@<branch>, <path>@<branch>, ...` — suggestion: check out the same branch in every
  composed pack repo.

Live confirmation (manual, against the *actual* dogfood harness pack the investigation doc
found, `/home/mc/working/cognitive-architectures`, still on
`feat/react-review-external-sync-discriminator` with local edits and 28 commits behind):

```
$ ./target/debug/praxec check --config /tmp/manual-staleness-check.yaml
...
soft warnings (resolve-time):
  warn[PACK_BRANCH_DRIFT]: pack repo '/home/mc/working/cognitive-architectures' is on feat/react-review-external-sync-discriminator, not its default branch 'dev' (git -C /home/mc/working/cognitive-architectures checkout dev)
  warn[PACK_DIRTY_TREE]: pack repo '/home/mc/working/cognitive-architectures' has uncommitted changes (git -C /home/mc/working/cognitive-architectures status)
  warn[PACK_BEHIND_UPSTREAM]: pack repo '/home/mc/working/cognitive-architectures' is 28 commit(s) behind origin/dev (git -C /home/mc/working/cognitive-architectures pull)

validation: 0 error(s), 3 warning(s), 3 soft warning(s)
$ echo $?
0
```

Exactly the failure mode the investigation doc described, now caught, all three classes at once,
and `check` still exits 0.

## Offline-safety and edge cases

- **No fetch, ever.** `git_currency` only runs `rev-parse`, `symbolic-ref`, `show-ref`, `status
  --porcelain`, and `rev-list --count` — never `fetch`/`pull`/`clone`. `behind_upstream` is
  computed only if `refs/remotes/origin/<default>` is *already* known locally (`show-ref
  --verify` gate before the `rev-list`); if not, it's `None` ("upstream unknown"), never an
  error and never a fabricated `0`.
- **"Upstream unknown" never invents a warning.** A repo with no `origin` configured at all
  (legit for a local-only pack author) that's clean and on its default branch stays completely
  silent — `clean_checkout_on_dev_has_no_drift_no_dirt_unknown_upstream` /
  `clean_pack_on_dev_has_no_currency_warning` both assert this. When the unknown-upstream repo
  *does* independently warn (branch drift or dirty), the message carries an
  `(upstream unknown — run git fetch to check)` note as a courtesy, per the task's requested
  phrasing — it never becomes its own standalone diagnostic.
- **Not a git repo at all** (`git rev-parse --show-toplevel` fails) → `None`. No warning, no
  error — a legitimate plain-dir `path:` pack.
- **A subdirectory of some larger enclosing repo** (`path:` points somewhere *inside* a git
  working tree, but not at its root) → also `None`. This is a deliberate design decision beyond
  the literal task wording: without it, every existing plain-dir test fixture under
  `crates/praxec-core/tests/fixtures/repos/*` — which live inside this very workspace's git
  repo — would spuriously inherit *this workspace's* branch/dirty state the moment this feature
  shipped, breaking `multi_repo_loading.rs`'s existing `diagnostics.is_empty()` assertions. The
  check is: canonicalize `path`, run `git rev-parse --show-toplevel` from it, and require the
  toplevel to canonicalize back to the same path. A real pack checkout (its own clone, its own
  `.git` at its root) always satisfies this; a nested fixture directory never does.
  `subdir_of_a_larger_repo_is_none` is the unit test proving it, and the full-workspace green run
  proves it didn't regress `multi_repo_loading.rs`.
- **Detached HEAD** → `branch: None`, `on_default_branch: false` → fires `PACK_BRANCH_DRIFT`
  naming it "a detached HEAD".
- **`uri:` (remote) repos are explicitly excluded**, not merely "gracefully skipped" —
  `is_local_path = matches!(source, RepoSource::Local(_))` is checked before the git-currency
  call. A remote repo's local cache under `.praxec/repos/<slug>` genuinely is a git repo, but
  warning about its staleness would be noise: `clone_or_update` already re-pulls its ref's tip on
  every load.
- **`worktrees_of:` entries** never reach the check — they `continue` earlier in the loop (their
  own resolution is identity-based, not a literal path).
- **Symlinks** — `Path::canonicalize()` resolves them before any comparison/command, so a
  symlinked `path:` entry behaves identically to the real directory; no special-casing needed.
- **No panics anywhere** — every git invocation goes through `git_ok`/`git_stdout`, which map a
  failed spawn or non-zero exit to `false`/`None` rather than unwrapping.

## Non-blocking guarantee

Every new diagnostic is `DiagnosticSeverity::Warn`. Nothing in the new code path returns `Err`.
`check`'s existing `errors > 0` gate (the only thing that makes it `bail!`) never sees these —
they only ever land in the `warnings`/`soft_warnings` counts. Proven by the CLI test
`check_warns_on_feature_branch_pack_but_still_exits_success` (asserts `out.status.success()`
*and* the warning text present) and the live manual run above (exit 0 with 3 warnings).

## Test result

- `cargo test -p praxec-core --lib repo_git::` — 11/11 passed (8 new).
- `cargo test -p praxec-core --test pack_staleness_warning` — 8/8 passed.
- `cargo test -p praxec --test pack_staleness_cli` — 2/2 passed.
- `cargo test --workspace` — full suite green, 0 failed across every crate (unit + integration +
  doctests).
- `cargo fmt` — applied to `praxec-core` and `praxec` (touched files only; reformatted the
  multi-line git-arg vecs this change authored).
- `cargo clippy -p praxec-core -p praxec --tests` — clean, no warnings.
