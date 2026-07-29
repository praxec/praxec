# Workflow-pack currency: how developers stay current — investigation (read-only)

Date: 2026-07-29
Status: investigation only — no design committed, no code changed.

## The question

As workflow changes land on a pack repo's dev/main (e.g. `cognitive-architectures`), how does
a developer using praxec get the latest workflows locally — detect the change, pull/update, and
reload so everything's current? Could it read them remotely instead? And how do multiple
developers keep both the BINARY and ALL the workflow packs current?

This session hit the concrete failure mode directly: this repo's own dogfood harness
(`/home/mc/working/mcp-flowgate/.fg-harness/gateway.yaml:20-21`) points `repos:` at
`/home/mc/working/cognitive-architectures`, and that checkout is currently sitting on
`feat/react-review-external-sync-discriminator` with uncommitted local edits — not `dev`, not
`main` — verified live:

```
$ cd /home/mc/working/cognitive-architectures && git branch --show-current
feat/react-review-external-sync-discriminator
$ git status --short | head
 M capabilities/cap.implement.build-loop.yaml
 M capabilities/cap.review.react-antipatterns.yaml
 ...
```

Nothing in praxec noticed or warned about this. That's the concrete shape of the gap.

---

## 1. How packs/workflows are LOADED

- Config schema: top-level `repos:` array. Each entry is exactly one of `path:` (local dir),
  `uri:` (remote git import), or `worktrees_of:` (identity-first live-worktree discovery) —
  mutually exclusive, enforced at parse (`crates/praxec-core/src/config.rs:4196-4249`,
  `parse_repo_entry`).
- **Remote IS supported today**, not just local paths: `- uri: git+https://github.com/acme/workflows@main`
  with an optional `ref:` (defaults to `"main"`) — `config.rs:4200-4210`. A `Remote` entry is
  resolved by `crate::repo_git::clone_or_update(&uri, &gitref, &dest)`
  (`config.rs:3247-3253`), cached under `<host>/.praxec/repos/<slug>` where `slug` is a
  filesystem-safe hash of the URI (`repo_git.rs:24-37`).
- `clone_or_update` (`crates/praxec-core/src/repo_git.rs:61-102`) is real, not a stub: clone on
  first use; on subsequent calls it `git fetch origin <gitref>` then
  `git reset --hard FETCH_HEAD` (`repo_git.rs:77-83`) — i.e. it **always re-pulls the ref's tip
  on every config load**, not just on first clone. It shells out to the operator's own git, no
  credential handling of its own (`repo_git.rs:1-8`).
- A **pack manifest** is `praxec.repo.yaml` at the repo root, schema `praxec.repo/v1`
  (`crates/praxec-core/src/repo.rs:24-45`), carrying `namespace` (definitionId prefix),
  `version` (doc'd as "Surfaced via `gateway.describe`" — **this claim is stale/inaccurate**,
  see §3), `description`, and a `layout` of directory names. Real examples:
  `cognitive-architectures/praxec.repo.yaml` (`namespace: cognitive`, `version: 0.0.1`),
  `cognitive-architectures-max/praxec.repo.yaml` (`namespace: cognitive-max`, `version: 0.0.1`).
- Load entrypoints: `load_resolved_with_repos` (Strict — used by `praxec check`) vs
  `load_resolved_with_repos_resilient` (Resilient — used by `serve`; a single bad `repos:` entry
  is skipped with a `REPO_LOAD_SKIPPED` warning rather than failing the whole gateway)
  (`config.rs:2995-3016`).
- Real config wiring, confirmed in this repo and the pack repos: `.fg-harness/gateway.yaml:20-21`
  (`repos: - path: /home/mc/working/cognitive-architectures`);
  `cognitive-architectures-max/ci-check.yaml:13-17` layers a base + overlay by `path:` with
  `priority:` for `hop_slot:` tie-breaking; the pack registry's 1-command installer
  (`/home/mc/working/packs/packs.yaml`, `setup.sh`) always writes a **local `path:`** entry after
  `git clone`/`git pull`, never a `uri:` entry (see §4).

## 2. RELOAD

There is real hot-reload machinery — `crates/praxec-core/src/hot_reload.rs` — with three trigger
paths, all funneling into one gated reload (`crates/praxec/src/gateway.rs:1533` `reload_gated`):

1. **SIGHUP** — `gateway.rs:1426-1479` registers a `SIGHUP` handler that calls `reload_gated`.
2. **In-band `praxec.command { reload: true }`** — the two-tool MCP surface accepts an
   undocumented (relative to the SPEC §32 summary) `reload` field: on the write tool, `reload:
   true` fires the same gated rebuild+swap
   (`crates/praxec-mcp-server/src/lib.rs:1119-1129`), returning the reload outcome as JSON. If
   the runtime wasn't started with a reload hook it returns `RELOAD_UNAVAILABLE`
   (`lib.rs:1124-1128`).
3. **Lazy TTL staleness probe** ("P6b") — because filesystem watchers are unreliable on WSL,
   `serve` does NOT use `notify`/fs-events. Instead, at the top of every request handled, a
   `StalenessTracker` (`hot_reload.rs:83-130`) checks — at most once per 10s TTL
   (`STALENESS_TTL`, `hot_reload.rs:28`) — whether any *tracked file's mtime* advanced; if so it
   triggers the same gated reload (`gateway.rs:1373-1420`).

What "tracked" means matters a lot for the gap: `local_config_file_set`
(`config.rs:204-278`) walks the top-level config, its `include:` files, and —
**only for `repos:` entries that have a `path:` key** — every definition YAML under that repo
(`config.rs:257-277`, `repo.get("path")`). A `uri:` (remote) repo entry has no `path` key and is
**never added to the tracked set**.

Net: for a **local-path** pack, editing a workflow file on disk is auto-detected and
hot-reloaded within ~10s with **no restart, no explicit command** — genuinely hot. For a
**remote (`uri:`) pack**, an upstream commit is invisible to the staleness tracker; the gateway
only re-pulls it when a reload is explicitly triggered (SIGHUP or `praxec.command {reload:
true}}`), at which point `merge_declared_repos` re-runs and `clone_or_update` does the
fetch+reset. **Restart is never required** for any repo kind once you know to trigger a reload —
but for a remote pack, *nothing tells you a reload is warranted*.

The reload itself is safe/atomic: definitions/executors/discovery-index/registry are each held
behind a `SwappableX` (`Arc<RwLock<Arc<dyn X>>>`) and swapped as a unit
(`hot_reload.rs:132-302`), proven by a concurrent-search test that a reader never observes a
torn mix of old/new (`hot_reload.rs:332-399`). A reload that fails validation drops to
"repair-only" mode, keeping the previous good definitions live (`gateway.rs:1548-1618`) rather
than bricking the gateway.

`praxec check` (`crates/praxec/src/gateway.rs:2402` `fn check`) is a one-shot, non-serving CLI
command — it loads once and prints workflow ids/version/imports; it has no relationship to the
reload mechanism (it's Strict-mode, used pre-deploy, not against a live process).

## 3. VERSIONING / PINNING / DRIFT DETECTION

- `RepoManifest.version` is parsed (`repo.rs:41-43`) but **never read anywhere else in the
  codebase** — confirmed by grep: no compat check, no comparison, no stamping into the merged
  config. `stamp_repo_priority` stamps `namespace → priority` for `hop_slot:` tie-breaking
  (`config.rs:3097-3098, 3336, 3709`) but there is no analogous `namespace → version` stamp.
  The doc comment claiming version is "Surfaced via `gateway.describe`" (`repo.rs:42-43`) is
  **stale/inaccurate** — the actual `home()` HATEOAS response only reports the **running
  gateway binary's own version** (`env!("CARGO_PKG_VERSION")`,
  `crates/praxec-core/src/discovery/discovery.rs:503-509`), nothing about loaded pack versions
  or namespaces.
- Cross-pack compatibility today is **prose only**: `cognitive-architectures-max/praxec.repo.yaml`
  says "Compatibility: requires the same praxec as cognitive-architectures" and
  `cognitive-architectures/praxec.repo.yaml` says "requires praxec 0.0.22 or later" — both are
  human-readable comments in the manifest description, never machine-checked.
- No lockfile, no checksum/hash of a loaded repo's content or commit, anywhere in the `repos:`
  path (contrast: `include:` DOES support a `{ uri, hash: "sha256:..." }` object form for
  individually hashed remote files — `config.rs:79, 1941` — but this is a different, narrower
  mechanism and is not applied to whole `repos:` entries).
- A remote (`uri:`) repo pins a **branch name** via `ref:` (default `"main"`,
  `config.rs:4200-4204`), not a commit SHA — so even the "remote read" path is a moving pointer,
  not a reproducible pin, unless the operator manually puts a SHA in `ref:` (untested, likely
  works since it's passed straight to `git fetch origin <gitref>` / `git clone --branch
  <gitref>`, but nothing in docs or examples does this).
- `praxec check` prints the **gateway config's own** `version:` field
  (`gateway.rs:2409-2419`, e.g. `.fg-harness/gateway.yaml:4` → `version: "1.0.0"`) — this is the
  config schema version, unrelated to pack content versions. It does not print per-repo
  namespace/version/git-ref/mtime, and does not compare anything to an upstream.
- `px doctor` (`crates/praxec-tui/src/main.rs:141`, ADR-0013
  `docs/architecture/adr/0013-doctor-provisions-pack-tools.md`) resolves and offers to
  provision **MCP tool binaries** a pack's connections need (cpm-planner, fmeca-mcp, …) via a
  registry-driven provider chain (docker → release binary → cargo). It says nothing about pack
  **content** staleness — ADR-0013 itself frames a pack as "pure YAML — nothing to install."

## 4. DISTRIBUTION

**Binary:**
- GitHub Releases via `.github/workflows/release.yml` — confirmed working per prior program
  memory (5-platform binaries, cross-compile fixed).
- `.github/workflows/publish.yml` exists, titled "Publish to crates.io when a `v*` tag is
  pushed," gated on a `CRATES_IO_TOKEN` repo secret
  (`publish.yml:1-6`) — **not actually wired** (no token configured; this session did not find
  evidence it has ever succeeded a real publish step beyond the PR-only `package-dry-run` job).
  No Docker image publish for the `praxec` gateway binary itself was found.
- `make install` (`Makefile:21-30`) discovers every workspace crate with a `[[bin]]` and runs
  `cargo install --path <dir> --bins` for each — installs from **whatever source checkout you
  have**, at "the current source version" (`Makefile:30`), no central pin.
- The registry-driven one-liner `curl -fsSL
  https://raw.githubusercontent.com/praxec/praxec/main/install.sh | sh` is referenced from
  `packs/setup.sh`'s own preflight (`setup.sh` header comment) as the binary bootstrap.

**Workflow packs:**
- `/home/mc/working/packs/packs.yaml` (schema `praxec.packs/v2`) is a **registry file**, not a
  package format: it lists pack `id`, `namespace`, `repo` (a plain GitHub URL), `requires:`
  (MCP tool ids), `extends:` (pack composition), `tier`. It is fetched, parsed, and consumed by
  `setup.sh` — the only mechanized consumer found.
- `setup.sh` (`/home/mc/working/packs/setup.sh`) is the entire "distribution mechanism" for pack
  **content**: it does `git clone -q "$PACK_REPO" "$PACK_DIR"` on first run, or `git -C
  "$PACK_DIR" pull -q || true` if `$PACK_DIR/.git` already exists (setup.sh, "clone the pack"
  section) — clones/pulls whatever the remote's **default branch** currently is, no ref pin, no
  lockfile. It then writes (only if absent — "Keeping existing $CFG" otherwise) a gateway config
  with `repos: - path: $PACK_DIR` (a **local path**, even though the pack was fetched over
  network) and runs `praxec check` once to validate.
- **Re-running the one-liner IS the documented "update a pack" workflow** — there is no separate
  `praxec pack update` or equivalent; "update" == "re-run setup.sh, which does `git pull`, then
  manually trigger a reload/restart of any running `serve` process" (the script does not itself
  send SIGHUP or call `reload`).
- README.md documents multi-repo loading conceptually (`README.md:135-141, 230-236`: "Ship them
  as Git repos with a `praxec.repo.yaml` manifest; operators load any number with a top-level
  `repos:` block") and links out to `praxec.dev/guides/multi-repo-loading/` (external site, not
  checked in-repo) — no in-repo doc walks through "how do I update a pack I already have."

## 5. CROSS-PACK composition currency

- `cognitive-architectures-max`'s manifest explicitly documents the two-repo pattern operators
  must hand-wire: `repos: [{path: .../cognitive-architectures}, {path:
  .../cognitive-architectures-max}]` (`cognitive-architectures-max/praxec.repo.yaml`,
  description block).
- There is **no guard anywhere** that checks the base pack's git branch, commit, or manifest
  `version` against what the overlay pack expects — confirmed by the same grep that found no
  runtime use of `manifest.version` (§3). `merge_declared_repos` only enforces: no duplicate
  `namespace` (V20-style hard error), every `kind: workflow` `definitionId:` reference resolves
  (V22), and `hop_slot:` cap collisions resolve by `priority` (Spec A §5) — none of these are a
  staleness or version-drift check, they're structural-integrity checks against whatever content
  happens to be on disk *right now*, however stale.
- This session's live repro (top of this doc) is exactly the failure mode: the base pack
  checkout can silently be on a feature branch, missing a flow the overlay/operator expects, and
  nothing in `praxec check` or `serve` surfaces that — it would only show up as a downstream
  "definitionId not found" (V22) or a workflow behaving unexpectedly, with no signal pointing at
  "your base pack checkout is stale/wrong-branch."

---

## Honest gap assessment

| Capability | Verdict | Why |
|---|---|---|
| **Detect upstream change** (pack repo got new commits on dev/main) | **ABSENT** | No polling, no webhook, no version/SHA comparison against the pack's remote. The TTL staleness tracker only watches **local mtimes** of already-loaded files (`config.rs:257-277`) — it cannot see a commit that landed upstream until something re-clones/fetches. |
| **Pull/update** (get the new content onto disk) | **PARTIAL** | Real mechanism exists (`git pull` in `setup.sh`, or `clone_or_update`'s fetch+reset for `uri:` repos), but it's **manual-trigger only** — re-run `setup.sh`, or trigger a reload on a `uri:`-configured gateway. Nothing pulls proactively or on a schedule. And the dominant real-world pattern (`path:` to a manual `git clone`) has praxec itself doing **zero** pulling — that's 100% on the developer's own git hygiene. |
| **Reload without restart** | **SUPPORTED for local-path repos**; **PARTIAL for remote (`uri:`) repos** | Genuinely hot: SIGHUP, in-band `praxec.command {reload:true}`, and an automatic TTL-based mtime probe (`hot_reload.rs`), swapped atomically, degrading to repair-only on a bad reload rather than crashing. For `uri:` repos, reload is available but never auto-triggered (not in the tracked-file set), so remote content updates only alongside an explicit SIGHUP/reload call. |
| **Remote/registry read** (praxec reads packs over the network instead of a local clone) | **SUPPORTED, but rarely used in practice** | `repos: [{ uri: "git+https://...", ref: "..." }]` is real, implemented, and tested (`repo_git.rs`) — but every real config found in this investigation (`.fg-harness/gateway.yaml`, `ci-check.yaml`, `setup.sh`'s generated config) uses `path:` to a manual clone instead. There is no artifact-registry / MCP-registry-style publish format for pack *content* — only for the MCP *tool binaries* a pack depends on (ADR-0013). |
| **Pack versioning/pinning** | **ABSENT** (as a functioning mechanism) — a version field exists in name only | `RepoManifest.version` is parsed and never used for any comparison, compatibility gate, or drift signal; not surfaced via `describe`/`home` despite a doc comment claiming it is. `ref:` on a remote repo pins a branch, not a commit, by default. No lockfile of resolved commit SHAs anywhere. |
| **Multi-dev binary + pack currency** | **ABSENT** as a mechanized guarantee | Binary: `make install` builds from whatever source you have checked out — no shared pin beyond a GitHub release tag a person manually installs. Pack: no lockfile, no shared "known-good" ref; two developers pointing `repos:` at independently-managed clones (as this repo's own harness does, and as `cognitive-architectures-max` explicitly documents as the composition pattern) can silently diverge — proven live in this session (base pack on a feature branch with uncommitted edits, unnoticed by `praxec check`). |

**Bottom line the report should be direct about:** the user's concern lands. Praxec has real,
well-engineered hot-reload plumbing (atomic swap, TTL-based local detection, SIGHUP, in-band
reload) and a real remote-git-import path — but the *detection* and *currency-guarantee* layer
that would make "as workflow changes land upstream, I automatically know / automatically get
them / automatically reload" true does not exist. Today the model is: git is the source of
truth, the developer is the sync mechanism (clone once, remember to `git pull`, remember to
trigger a reload or restart), and nothing in praxec tells you when that's gone stale — including
across composed packs, where a stale/wrong-branch base checkout produces no warning at all,
only downstream symptoms.

---

## Design options (sketch — not for build yet)

Ordered roughly small → architectural. Praxec's stated stance ("local-binary product,"
stores = memory/file/sqlite only, binary via GitHub releases, no server-managed credentials —
`repo_git.rs:5-8` "praxec never stores or manages git credentials") should bound anything
proposed: no praxec-hosted registry service, no daemon, no background polling process distinct
from the gateway itself.

**(a) Pack manifest version + `praxec check`/`doctor` staleness warning vs upstream** — *small,
fits the stance.* Add `git rev-parse HEAD` (or `git status -sb` ahead/behind against the
tracked upstream) as a cheap, best-effort check at `check`/reload time for any `path:` repo that
happens to be a git working tree. Surface as a WARN diagnostic ("`cognitive-architectures` repo
is N commits behind `origin/dev`" / "on branch `feat/x`, not `dev`/`main`"), the same
diagnostic-severity pattern already used for `REPO_LOAD_SKIPPED` etc. Would have caught this
session's exact failure. Cheap because it reuses the same `git` shell-out pattern
`repo_git.rs` already establishes; no new dependency, no schema change required for `path:`
repos (git state is discovered, not declared). Doesn't help non-git `path:` repos, but that's a
narrow case.

**(b) Explicit `praxec pack update` / richer reload command (git pull + hot registry swap)** —
*small-medium, fits the stance.* For a `path:` repo that IS a git checkout, a thin wrapper that
does `git pull` (respecting the operator's git auth exactly like `repo_git.rs` already does) then
calls the existing `reload_gated` path. This closes "pull" without inventing new plumbing — it's
composition of two things that already exist (git pull, gated reload) behind one command/CLI
verb instead of "go do it by hand in two shells."

**(c) Remote/registry pack loading as the default onboarding path (git-ref pin, ideally SHA not
branch) over local-clone-only** — *medium, mostly a documentation/default-config change, not new
code.* The `uri:`/`ref:` mechanism already exists and is tested; the gap is that nothing steers
operators toward it — `setup.sh` always downgrades a remote fetch into a local `path:` clone.
Changing `setup.sh`'s generated config to emit `repos: [{uri: ..., ref: <pinned-sha-or-tag>}]`
instead of `path:` would make every subsequent reload auto-pull on trigger, and would make the
pin visible/diffable in the gateway config itself (version-controllable, unlike an
untracked-by-config local clone). Tradeoff: loses the "just edit the file and it hot-reloads
in 10s" workflow pack authors currently get from local-path staleness tracking — so this is a
per-operator choice (author on `path:`, consume on `uri:`), not a universal default.

**(d) A pack lockfile for reproducible multi-dev pins** — *architectural.* A
`praxec.lock` (or similar) recording, per declared repo, the resolved commit SHA (and maybe a
content hash of the loaded definition set) at last successful load — analogous to Cargo.lock/
package-lock.json. `praxec check --update-lock` (or a flag) would refresh it; plain `check`/serve
would warn (or, in a stricter mode, refuse) if the live resolved SHA diverges from the lock. This
is the piece that would give a *team* — not just one operator — a shared, diffable, PR-reviewable
answer to "are we all on the same packs." It's the biggest lift of the four: new file format,
new CLI surface, a decision about strict-vs-warn enforcement, and interaction with the existing
`RepoLoadMode::Strict/Resilient` split. It's also the only option here that actually answers the
"multiple developers" half of the original question, not just the single-operator "did I notice
upstream changed" half — (a)-(c) all still leave two developers free to silently diverge unless
they separately discipline themselves to keep lockfile-equivalent info in sync by hand.

**Sequencing note:** (a) is almost pure benefit for near-zero cost and should be considered
independent of the rest. (b) is a convenience wrapper around existing verified primitives. (c) is
mostly a default/docs change that trades one workflow (instant local hot-reload for pack authors)
for another (pinned, reload-triggered remote consumption) — a real tradeoff to present, not a
strict improvement. (d) is the only one that closes the "multi-dev parity" gap for real, and is
architectural enough to warrant its own elicit/design pass rather than being bundled here.
