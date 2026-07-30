# Pack currency, the Cargo-like model: remote sourcing, lockfile, `pack update`

Date: 2026-07-29. Status: **design spec — not built.** Read-only investigation against `dev`
(current tag v0.0.40); no engine source touched. Builds directly on
`docs/design/2026-07-29-workflow-currency-investigation.md` (the current-state map) — every claim
below is grounded in that investigation plus fresh reads of `repo_git.rs`, `config.rs`,
`hot_reload.rs`, and the CLI surface (`gateway_config.rs`) done for this spec.

## Framing: this is Cargo, already half-built

| Cargo | praxec today | praxec after this spec |
|---|---|---|
| `Cargo.toml` `[dependencies]` | `repos:` (`path:` / `uri:`+`ref:` / `worktrees_of:`) | unchanged schema; `uri:` becomes the recommended default |
| `Cargo.lock` | **absent** | new `praxec.lock` |
| `cargo update` | **absent** for remote packs (`praxec sync` exists but is a different, narrower thing — see below) | new `praxec pack update` |
| `cargo build` (resolves lock, no network if satisfied) | `serve`/`check` load (`merge_declared_repos`) | same, made lock-aware |
| path dependency (`{ path = "../foo" }`, unlocked, mutable) | `path:` repo entry | unchanged — deliberately stays unlocked |

One correction to the prior investigation surfaces here that matters for scoping: **a `praxec
sync` command already exists** (`crates/praxec/src/gateway_config.rs:130-134`, impl
`crates/praxec/src/gateway.rs:2165-2233`, shipped at v0.0.20, `b127dab`). It fetches + fast-forwards
`path:` repo checkouts to `origin/main`, but only when the tree is clean and already on `main` — a
convenience wrapper around the developer's own git hygiene for **local checkouts**. It does *not*
touch `uri:` repos (prints "refreshed on every load; nothing to do" and skips — `gateway.rs:2182-2185`),
does not resolve or write anything to disk beyond the git checkout itself, and does not call the
reload machinery (it relies on the existing mtime-staleness tracker noticing the fast-forwarded
files on the next request, since `path:` repos are in the tracked set —
`config.rs:259-278`). **`pack update` is a different, complementary command**: it operates on
`uri:` (remote-cache) repos, and its job is producing/refreshing a **lockfile** of resolved SHAs —
something `sync` has no concept of. The two should coexist, not merge: `sync` = "fast-forward my
local dev checkout," `pack update` = "re-resolve and pin my remote dependencies." This distinction
must be documented explicitly wherever `pack update` ships, or operators will reasonably ask why
there are two update-ish commands.

---

## A — Remote sourcing: making `uri:`/`ref:` the recommended default

### Grounding: what's real today

- Schema: `repos:` entries are `path:` XOR `uri:`(+optional `ref:`, default `"main"`) XOR
  `worktrees_of:`, enforced at parse — `crates/praxec-core/src/config.rs:4195-4254`
  (`parse_repo_entry`). A `uri:` entry becomes `RepoSource::Remote { uri, gitref }`
  (`config.rs:4200-4210`).
- Resolution: `merge_declared_repos` resolves a `Remote` source to
  `<host_dir>/.praxec/repos/<cache_dir_name(uri)>` and calls
  `crate::repo_git::clone_or_update(&uri, &gitref, &dest)` —
  `crates/praxec-core/src/config.rs:3247-3253`. This runs **on every config load** (every
  `check`/`serve` start, and every reload), not just once.
- `clone_or_update` (`crates/praxec-core/src/repo_git.rs:61-102`) is real and tested
  (`repo_git.rs:157-253`): clone on first use (`git clone --branch <gitref> --single-branch`,
  `repo_git.rs:88-99`); on every subsequent call, `git fetch origin <gitref>` then
  `git reset --hard FETCH_HEAD` (`repo_git.rs:77-83`) — **always jumps to the ref's current tip**,
  discarding any local drift in the cache (by design: the cache is not meant to be hand-edited).
  A `.git`-present-but-unhealthy cache fails loud with a `REPO_CACHE_CORRUPT` remedy
  (`repo_git.rs:63-75`) rather than silently misbehaving.
- **Auth**: confirmed by reading the module doc-comment — `repo_git.rs:1-8` states explicitly
  "Everything shells out to `git`, inheriting the operator's existing git auth (SSH key /
  credential helper / cached token / `gh`). Praxec never stores or manages git credentials." No
  praxec-side credential store, token, or keychain integration exists anywhere in this path —
  confirmed, there is nothing to configure. **This already fully supports private pack repos**:
  if `git clone git@github.com:acme/private-packs.git` works in the operator's shell (SSH key
  loaded, or an HTTPS credential helper caching a PAT), `clone_or_update` works identically,
  headless or interactive, CI or laptop.
- **`ref:` semantics today**: passed straight through as a `git` ref argument to both `clone
  --branch <gitref>` and `fetch origin <gitref>`. Git branches, tags, AND full SHAs are all legal
  values for `git fetch origin <ref>` — but `git clone --branch <ref>` (the first-clone path,
  `repo_git.rs:88-96`) does **not** accept an arbitrary commit SHA; `--branch` only resolves
  branches and tags, not bare SHAs. So **today, pinning `ref: <full-sha>` in a fresh (never-cloned)
  config half-works and half-doesn't**: it would fail on first clone with a git error, but would
  have worked on `fetch` had the repo already been cloned by some other ref. This is a **real,
  concrete gap** — confirmed by reading `git clone --help` semantics against the exact args used at
  `repo_git.rs:88-96`, not asserted from memory. It matters for this spec because §B's lockfile
  wants to pin exact SHAs, and the naive "just put the resolved SHA in `ref:`" shortcut does not
  reliably work through `clone_or_update` as written today for a cold cache.
- **Cache location**: `<host_dir>/.praxec/repos/<slug>`, `slug = cache_dir_name(uri)` — a
  filesystem-safe, deterministic (order-preserving, non-alphanumerics→`-`) transform of the URI
  (`repo_git.rs:22-37`, `config.rs:3248-3251`). Same URI always maps to the same cache dir; no ref
  in the slug, so two `ref:` values for the *same* `uri:` in different configs would collide on one
  cache dir and fight over `reset --hard` — an existing latent multi-ref sharp edge, out of scope
  here but worth flagging (see Note at end of §A).
- **Offline behavior today**: none, deliberately — every load calls `clone_or_update`, which always
  attempts `fetch`. A cold network fails the whole repo load (Strict: hard error; Resilient:
  `REPO_LOAD_SKIPPED` warn, `config.rs:3276-3309`). There is no "cache exists, ref unchanged, skip
  the network round-trip" fast path. This is the offline gap §B closes (with a locked SHA, offline
  becomes possible and correct — see below).
- **Real-world usage today**: every config found in the investigation (`.fg-harness/gateway.yaml`,
  `cognitive-architectures-max/ci-check.yaml`, the pack registry's `setup.sh`-generated config) uses
  `path:` to a manually-managed clone. `uri:` is implemented, unit-tested, and **essentially
  unused** in practice.

### The concrete gap and the fix

The gap is not "remote sourcing doesn't work" — it does. The gap is **nothing steers an operator
toward it**, there's no example config to copy, and a first-clone SHA pin doesn't actually work.
Recommendation:

1. **Docs + example config (no engine change).** Add a `repos: [{ uri: ..., ref: ... }]` example
   next to the existing `path:` examples in `README.md` (§"Ship them as Git repos," `README.md:135-141`)
   and in whichever guide covers `.fg-harness/gateway.yaml`-style onboarding. Show both forms:
   `ref: main` (moving, for "always latest") and `ref: v1.2.0` (a tag, for a soft pin) — and note
   that a raw commit SHA is the true pin but goes through the **lockfile** (§B), not a bare `ref:`
   edit, once B ships.
2. **`setup.sh` should stop downgrading remote to local.** The pack registry's one-liner installer
   currently always writes a `path:` entry after `git clone` (`packs/setup.sh`, per the
   investigation). That script lives in a sibling repo (`/home/mc/working/packs`), out of this
   repo's engine-change scope, but the spec should say plainly: once B/C ship, `setup.sh`'s
   generated config should emit `repos: [{ uri: ..., ref: <tag-or-sha> }]` plus a generated
   `praxec.lock`, not a `path:` clone. Flagged as a **docs/tooling follow-up in the sibling repo**,
   not engine work.
3. **The one real engine-adjacent fix**: make `clone_or_update`'s first-clone path accept a bare
   SHA. Concretely, when `gitref` is not resolvable via `--branch` (or, simpler and more robust:
   always `git init` + `git remote add origin <url>` + `git fetch origin <gitref>` + `git reset
   --hard FETCH_HEAD` instead of `git clone --branch`), a cold cache can be seeded directly at a
   pinned SHA. This is a small, contained change to `repo_git.rs::clone_or_update`'s first-clone
   branch only (~15-20 lines); the fetch/update branch already works correctly for a SHA. It is
   listed here for completeness but is properly **part of increment B** (the lockfile is what
   actually needs to seed a cold cache at an exact SHA) — see the B assertions below.

Nothing else in §A requires engine work. It is deliberately the smallest increment: docs, an
example config, and one narrow fix to a function that already exists, folded into B's build since
B is what actually exercises it.

**Note (latent, out of scope):** two `repos:` entries with the same `uri:` but different `ref:`
values across configs (or even within one host's multiple gateways) currently share one cache dir
and will thrash each other via `reset --hard` on every load. This predates and is orthogonal to this
spec (it's a `cache_dir_name` collision, not a lockfile problem) — the lockfile in §B doesn't fix
it either, since the cache path is keyed on `uri` alone. Worth a follow-up ticket, not blocking here.

---

## B — The lockfile (the crux)

### Format + location

**Recommendation: `praxec.lock`, YAML, sibling to the gateway config** (same directory as the
`gateway.yaml`/`.fg-harness/gateway.yaml` it locks — one lockfile per gateway config, discovered by
convention: `<config_dir>/praxec.lock`, analogous to how `Cargo.lock` sits beside `Cargo.toml`).

Rationale for YAML over TOML: every other config surface in this codebase is YAML — the gateway
config itself, `praxec.repo.yaml` manifests, `packs.yaml` registry files, `include:` bodies. TOML
would be the "Cargo-familiar" choice, but praxec has no TOML parser/writer in its runtime dependency
graph today (only `Cargo.toml` itself, handled by `cargo`, not by praxec's own code) and introducing
one purely for lockfile-familiarity is exactly the "no premature infra" stance the program has held
consistently (`repo_git.rs`'s own doc comment: no server-managed credentials, no new plumbing beyond
what's needed). Consistency with the rest of the config surface — same parser
(`serde_yaml`, already a dependency), same diagnostics/error patterns, same human-editable/diffable
property — wins over surface-level Cargo mimicry. The *model* is Cargo-like; the *serialization*
stays praxec-native.

Location rationale: one lockfile per gateway config (not one per repo, not a single global
lockfile) because a `repos:` list is scoped to a config, and different configs on the same machine
may legitimately want different pins of the same pack (e.g. a `check`-only CI config pinned to a
release tag vs a dogfood `.fg-harness/gateway.yaml` intentionally tracking `dev` HEAD). Discovery-
by-convention (same dir, fixed filename) needs no new config key and mirrors how `Cargo.lock` is
found relative to `Cargo.toml` without being named in it.

### What it pins, per remote (`uri:`) repo source

```yaml
schema: praxec.lock/v1
repos:
  - uri: "git+https://github.com/acme/workflows"
    ref: "main"              # the REQUESTED ref, as declared in repos:
    sha: "a1b2c3d4e5f6..."    # the RESOLVED commit this ref pointed to, last time it was resolved
    resolved_at: "2026-07-29T18:04:00Z"   # when the resolution happened (audit trail, not enforced)
    namespace: "cognitive"    # the repo's declared namespace (praxec.repo.yaml), for human-readable diffs
```

Field-by-field justification:

- **`uri`** — the join key back to the config's `repos:` entry (a lockfile with no config
  correlate is dead weight; matched by exact URI string, same as the cache-slug keying already
  used in `cache_dir_name`).
- **`ref`** (requested) — recorded so the lockfile is self-documenting and so `pack update`
  (§C) knows what constraint to re-resolve against (a branch tracks its tip; a tag is expected
  stable; re-resolving a tag to a *different* SHA is itself a signal worth surfacing). Also lets
  a lockfile diff show "ref changed from `v1.2.0` to `v1.3.0`" as a human-reviewable line, not just
  an opaque SHA change — this is the PR-reviewable, diffable requirement the original investigation
  flagged as the actual "multi-dev parity" gap (`workflow-currency-investigation.md`'s option (d)
  rationale).
- **`sha`** — the reproducibility pin itself: the exact commit every developer/CI run loads,
  independent of what the branch/tag currently points to. This is the field that actually answers
  "are we all on the same packs."
- **`resolved_at`** — cheap, useful for "how stale is this pin" at a glance in a PR review; not
  load-bearing for any enforcement logic (no TTL expiry — that would contradict the offline/
  reproducibility goal). Optional in spirit but always written by anything that (re)writes the
  lock, since it costs nothing.
- **`namespace`** — not strictly required for correctness (the `uri` is the real key) but makes a
  lockfile diff human-legible without cross-referencing the pack's `praxec.repo.yaml` — "cognitive
  moved from a1b2c3d to f00dfac" reads better than the bare URI. Cheap to carry since
  `merge_declared_repos` already loads the manifest (`config.rs:3272-3273`) at the moment it would
  write this.
- **No manifest/content checksum field.** Considered and rejected for v1: the `sha` already
  transitively pins 100% of the repo's content (git's own content-addressing *is* the checksum —
  a hash-of-the-tree would be redundant with what `sha` already guarantees, since `reset --hard
  <sha>` is deterministic). A separate content hash would only earn its keep if praxec needed to
  detect *manual tampering of the cache directory after clone* (someone hand-editing files inside
  `.praxec/repos/<slug>` without git noticing) — a threat model nothing else in this codebase
  defends against (the cache is explicitly not meant to be hand-edited, same as `node_modules` or
  `target/`), so out of scope. If it's ever wanted, `git rev-parse HEAD` after a fresh checkout
  already re-derives the same content-address; nothing new to add.
- **`path:` and `worktrees_of:` repos are NOT recorded in the lockfile at all** — not merely
  "unlocked," genuinely absent as entries. See next section for why.

### `path:` (local-dev) packs: deliberately absent from the lock

Cargo precedent: a `{ path = "../foo" }` dependency is never written into `Cargo.lock`'s pinned-
version set — it's resolved fresh from disk on every build, by design, because the whole point of a
path dependency is "I'm actively editing this, don't freeze it." Same reasoning applies directly
here, and it's reinforced by an existing praxec mechanism: `path:` repos are the ones added to
`local_config_file_set`'s tracked-mtime set (`config.rs:259-278`), which is precisely what makes
local-pack editing hot-reload in ~10s with zero ceremony (the investigation's §2 finding). A `path:`
repo entering the lockfile would either (a) be meaningless — there's no `sha` to resolve for an
arbitrary directory that may not even be a git repo, or if it is one, pinning it would fight the
exact live-edit workflow pack authors rely on — or (b) require constant lockfile churn as a repo
author edits their pack, defeating the "diffable, meaningful PR review" goal the lockfile exists
for. So: `pack update` and lockfile resolution skip `path:` entries entirely (same skip `run_sync`
already does the inverse of — `sync` handles `path:` and ignores `uri:`; the lockfile machinery
handles `uri:` and ignores `path:` — cleanly complementary, not overlapping).

`worktrees_of:` entries are excluded for the same underlying reason (they're always writable run
targets by construction — `config.rs:4222-4231` requires `writable: true` — never a
definitions source to pin).

### Resolution: `ref` → `sha`

A thin, reusable addition to `repo_git.rs`: `resolve_ref_to_sha(uri: &str, gitref: &str) ->
anyhow::Result<String>`, implemented as `git ls-remote <url> <gitref>` (no clone needed — resolves
a remote ref to its SHA over the network without materializing a working tree) with a fallback to
`git rev-parse <gitref>` against an already-cloned cache dir if `ls-remote` doesn't return a match
(covers the case where `gitref` is already a raw SHA — `ls-remote` can't resolve a bare SHA that
isn't advertised as a ref, but a cache that's already been fetched to that SHA can `rev-parse` it
locally). This reuses the exact `run_git`/`Command::new("git")` shape already established in
`repo_git.rs:39-56` — no new dependency, no new auth handling (same operator-git-auth inheritance
the module doc-comment already commits to).

### Load semantics (the reproducibility core)

1. **Lockfile present, `uri:` entry has a lock line** → resolve the cache dir as today
   (`cache_dir_name(uri)`), but instead of `clone_or_update(uri, gitref, dest)` (which always resets
   to the *ref's current tip*), call the locked-SHA variant: `clone_or_update(uri, &locked_sha,
   dest)` — reusing `clone_or_update` unchanged is possible because `git fetch origin <ref>` /
   `git reset --hard FETCH_HEAD` and (once §A's fix lands) the first-clone path both already accept
   a bare SHA as `gitref`. **This is the reproducibility guarantee**: even if `origin/main` has
   advanced upstream, the loaded content is pinned to exactly the recorded commit — "a config with
   a lockfile loads each remote pack at the locked SHA even when the branch has advanced" (the
   assertion named in the task, and it falls out of `clone_or_update` needing no change beyond §A's
   fix — reuse, not new plumbing).
2. **Lockfile present, cache dir's current HEAD ≠ locked SHA** (someone else's `pack update` ran,
   or the cache was manually poked) → **reset to the locked SHA wins**, unconditionally. This is
   exactly what step 1 already does (`clone_or_update` always resets), so this isn't a distinct code
   path — it's the same call, and it's why "reproducibility wins" requires no special-casing: the
   locked SHA is just what gets passed as `gitref`.
3. **Lockfile absent (or the `uri:` entry has no matching lock line — e.g. a newly-added `repos:`
   entry)** → fall back to today's behavior: resolve to `ref`'s current tip via the existing
   `clone_or_update(uri, ref, dest)` path, **and** — this is a `check`/`serve`-time write, not a
   hidden side effect — offer/perform a **lockfile write-if-absent**, the same "only if missing"
   posture `setup.sh` already uses for its generated gateway config ("keeping existing $CFG"
   otherwise). Concretely: `praxec check` and `serve`'s first successful load, when no lockfile
   exists, resolve + **write one** (equivalent to Cargo's "no Cargo.lock yet → generate it on first
   build"); when a lockfile exists, they do NOT silently rewrite it — only `pack update` (§C)
   rewrites an existing lock. This mirrors Cargo exactly (`cargo build` will *create* a missing
   lockfile but will not silently *change* an existing one to newer versions — that's `cargo
   update`'s job).
4. **Offline**: if the locked SHA is already present in the cache (verified cheaply via `git
   cat-file -e <sha>` or checking current HEAD against it before touching the network), load with
   **no network call at all** — skip `clone_or_update`'s `fetch` entirely. Only fall through to a
   real `fetch` when the SHA is missing from the cache (first time this machine has seen this pin,
   or the cache was pruned). This is the offline-safe behavior the "local-binary product" stance
   requires and which is impossible today (every load unconditionally fetches — §A's offline-
   behavior finding). This is the other named assertion: "an offline load with the SHA cached needs
   no network."
5. **Strict vs Resilient composition**: unchanged in spirit — a `uri:` repo that fails to resolve
   (network down AND SHA not cached, or a lock references a SHA that's been force-pushed away
   upstream) is Strict-mode hard-fail / Resilient-mode `REPO_LOAD_SKIPPED`, same as any other
   `clone_or_update` failure today (`config.rs:3276-3309`). No new failure-mode taxonomy needed —
   this composes with the existing one.

### Composition with the staleness warning (currency option (a), in flight separately)

The staleness-warning work (option (a) from the investigation — comparing a `path:` repo's local
git state against its upstream branch) is a **separate, parallel** piece of currency work, scoped
to `path:` repos and orthogonal to this lockfile (which is scoped to `uri:` repos). But there's one
place they must agree: **once a lockfile exists, any staleness/drift comparison for a `uri:` repo
should compare the loaded content's SHA against the *locked* SHA, not against "how far behind is
the branch tip."** A `uri:` repo intentionally pinned to a 3-month-old SHA via the lock is not
"stale" in the actionable sense — it's exactly what the team agreed to run; a warning that fires
"you're 40 commits behind origin/main" on every load would be noise that trains people to ignore the
warning entirely. The actionable drift signal for a locked repo is "the on-disk cache's HEAD no
longer matches what `praxec.lock` says it should be" (which, per the load semantics above, is
actually self-healing — the next load resets it) — so in practice this composition point mostly
matters for the **report line** `pack update` and `check` print (§C), not for a new independent
staleness check. Recommendation: when a `uri:` repo has a lock entry, suppress any generic
"behind origin/<ref>" comparison for it; only surface drift relative to the lock. `path:` repos are
untouched by this — they keep whatever drift-vs-branch warning ships from the parallel effort.

---

## C — `praxec pack update`

```
praxec pack update [<pack>] [--config <path>] [--dry-run]
```

- **Behavior**: for every `uri:` repo declared in the config (or just the one matching `<pack>` —
  matched against the repo's declared `name:` if present, else its manifest `namespace`, else the
  bare `uri:` string, in that preference order — mirroring how `entry_desc` already renders repo
  identity for diagnostics at `config.rs:3116-3122`), re-resolve `ref` → current SHA via
  `resolve_ref_to_sha` (§B), compare to the lockfile's existing `sha` for that `uri`, and:
  - if changed: fetch the new SHA into the cache (`clone_or_update(uri, &new_sha, dest)` — same
    reused primitive), rewrite that repo's line in `praxec.lock` (`sha`, `resolved_at`), and print
    `old_sha → new_sha` (short form, à la `git log --oneline`, e.g. `a1b2c3d → f00dfac`).
  - if unchanged: print `<pack> — already at <sha> (up to date)`, no lockfile write for that entry.
- **`--dry-run`**: run the same resolution (network `ls-remote`, no fetch/clone/checkout) and print
  the same old→new report, but **never** write the lockfile or touch the cache — a pure preview,
  matching the `Sync` command's existing "report, don't act unless it's safe" ethos
  (`gateway.rs:2165-2233` never mutates without an explicit fast-forward check) and the `Cleanup`
  command's existing dry-run-by-default convention (`gateway_config.rs:107-122`, "DRY-RUN by
  default... pass `--force` to actually delete"). For consistency with `Cleanup`'s established
  pattern, consider making `pack update` itself dry-run-by-default with a `--write`/`--apply` flag
  rather than opt-in `--dry-run` — a **decision point to confirm with the user before building**,
  since it's a real behavioral choice (Cargo's `cargo update` mutates by default, `--dry-run` is
  opt-in preview; praxec's own `Cleanup` precedent goes the other way). This spec defaults to
  matching Cargo (`update` mutates, `--dry-run` previews) since the whole point of the command is
  Cargo-familiarity, flagged here for explicit confirmation rather than silently picking one.
- **No named pack given**: update every `uri:` repo in the config (Cargo's `cargo update` with no
  package argument updates the whole lockfile).
- **Respecting `ref:`**: `pack update` never changes what `ref:` a config declares — it only
  re-resolves that ref to whatever SHA it *currently* points to. Moving from `ref: v1.2.0` to
  `ref: v1.3.0` is a config edit (a `repos:` change), not something `pack update` does on its own;
  `pack update` re-locks the *declared* ref, it doesn't upgrade the ref itself. This mirrors
  Cargo's `~`/`^` semver-constraint model: `cargo update` moves the lock within the declared
  constraint, `cargo upgrade` (a different, cargo-edit-provided command) changes the constraint
  itself. Praxec's `ref:` is the constraint; there is no analogous "upgrade the constraint" command
  in this spec (out of scope — a config-editing concern, not a currency concern).
- **Reload**: `pack update` does **not** implicitly reload a running `serve` process (it's a
  separate one-shot CLI invocation, same posture as `praxec check`/`praxec sync` — neither of those
  reaches into a running server either). Document that after `pack update`, the operator triggers a
  reload the existing way (SIGHUP, or `praxec.command {reload: true}}` in-band) — this is
  composition, not new plumbing, exactly like `Sync`'s existing reliance on the staleness tracker
  picking up its filesystem change on the next request. (Whether a `--reload` convenience flag that
  shells out to send itself a SIGHUP-equivalent is worth adding is a small follow-on, not core to
  this spec.)
- **Exit/reporting**: exit 0 if every named (or all) packs resolved successfully (including the
  "nothing changed" case); exit non-zero if any pack's resolution failed (network down, ref doesn't
  exist upstream, etc.) — same fail-loud posture as every other CLI verb in `gateway_config.rs`.
  Report format: one line per pack (`<name> — <old-sha>..<new-sha>` / `<name> — up to date` /
  `<name> — FAILED: <reason>`), a summary line at the end (`pack update: N updated, M unchanged, K
  failed`) — directly modeled on `run_sync`'s existing summary line
  (`gateway.rs:2231`, `"sync: {updated} repo(s) updated, {attention} needing attention."`).

---

## Build increments (assert-first, red-first per [[assert-don't-derive]])

**A — remote-ergonomics + example config + docs.** *Smallest, mostly non-engine.* Deliverables:
`repos: [{uri:, ref:}]` example in README/guides; the one narrow `repo_git.rs::clone_or_update`
first-clone fix (accept a bare SHA — replace `git clone --branch <ref>` with `init` + `remote add`
+ `fetch` + `reset --hard` for the cold-cache case, or detect ref-vs-SHA and branch the two clone
strategies). Key assertion to write red-first: *"cloning a never-before-seen remote repo with
`ref:` set to a full commit SHA succeeds and lands exactly that commit"* (currently fails — verified
by reading `repo_git.rs:88-99`, not run, since `--branch <sha>` is not valid git). No lockfile, no
new CLI command yet. Reuse point: `repo_git.rs:61-102` (`clone_or_update`), touched minimally.

**B — the lockfile: format + `ref`→SHA resolution + load-at-locked-SHA + generation.**
*Architectural — the core of this spec.* New `praxec.lock` YAML type + (de)serialization; new
`repo_git::resolve_ref_to_sha`; a locked-SHA branch threaded into `merge_declared_repos`'s
`RepoSource::Remote` arm (`config.rs:3247-3253`) that swaps in the lock's `sha` for `gitref` when a
lock entry matches; write-if-absent on first successful load with no lock. Key assertions to write
red-first:
- *"a config with a `praxec.lock` loads each `uri:` pack at the locked SHA even when `origin/<ref>`
  has advanced"* — seed a local git "origin" fixture (same `seed_origin` pattern already used in
  `repo_git.rs:132-155`'s tests), lock it at commit 1, advance origin to commit 2, load the config,
  assert the loaded cache is still at commit 1.
- *"an offline load whose locked SHA is already in the cache needs no network call"* — this is the
  one assertion that needs a *fakeable/interceptable* git-fetch boundary to prove absence of a
  network call; simplest proof: point `origin` at a URI that doesn't resolve (or remove the fixture
  dir) after the cache is warm, and assert the second load still succeeds (proves no fetch was
  attempted, since a real fetch attempt against a dead remote would fail loud).
- *"a config with no `praxec.lock` and a `uri:` repo writes one on first successful `check`/serve
  load; a config with an existing lock does not get it silently rewritten by a plain load."*
- *"a `path:` repo never appears in `praxec.lock`, even when it's a git working tree."*

Reuse points: `repo_git.rs:39-56` (`run_git` for the new `ls-remote`/`rev-parse` resolver),
`config.rs:3247-3253` (the one call site that threads `gitref` into `clone_or_update` — this is
where locked-SHA substitution slots in), `config.rs:3272-3273` (manifest load, for the lock's
`namespace` field), the existing `Diagnostic`/`REPO_LOAD_SKIPPED` pattern
(`config.rs:3276-3309`) for lock-resolution failures.

**C — `praxec pack update` command.** *Depends on B; small once B exists* — it's almost entirely
composition of B's `resolve_ref_to_sha` + `clone_or_update` + lockfile writer, plus a new
`Command::PackUpdate` variant in `gateway_config.rs` (same shape as the existing `Sync`/`Cleanup`
variants — config path, optional pack-name positional, `--dry-run` flag) and its `run_pack_update`
impl in `gateway.rs`, modeled directly on `run_sync` (`gateway.rs:2165-2233`). Key assertions to
write red-first:
- *"`pack update` with a named pack whose upstream ref has moved rewrites only that pack's `sha` in
  the lock and reports old→new; other locked packs are untouched."*
- *"`pack update --dry-run` reports the same old→new diff but the lockfile on disk is byte-identical
  before and after."*
- *"after `pack update` rewrites the lock, a subsequent `serve`/`check` load (or an explicit
  reload) picks up the new SHA"* — this is really re-exercising B's load-at-locked-SHA assertion
  with the lock file `pack update` just wrote, proving the two increments actually compose rather
  than each being independently correct but not wired together.
- *"`pack update` on a pack with no lock entry yet (newly added `repos:` uri) resolves and adds one,
  rather than erroring."*

Reuse points: `gateway_config.rs:123-134` + `gateway.rs:2165-2233` (`Sync`'s existing CLI-shape and
report-line precedent — `pack update` should read like `Sync`'s remote-side twin, not a bespoke new
style), B's resolver + lockfile writer.

**Sequencing respects the stated stances**: local-binary product (no daemon, no background
polling — `pack update` is an explicit one-shot CLI verb, same as `sync`/`check`), fail-fast (a
failed resolution is reported non-zero, never silently skipped in Strict mode), offline-safe (B's
locked-SHA-in-cache fast path), reuse over new plumbing (`repo_git.rs`'s three functions
(`clone_or_update`, and B's new `resolve_ref_to_sha`) are the only git-shelling primitives needed;
`config.rs`'s existing `RepoSource::Remote` arm and `Diagnostic`/skip machinery are reused
unchanged in shape), no premature infra (YAML not TOML, no new dependency, no registry service).
Engine work lands on a `feat/pack-lockfile` (or similarly named) branch off `dev` per gitflow, in
commit groups roughly matching A→B→C, not a stack of separate PRs.
