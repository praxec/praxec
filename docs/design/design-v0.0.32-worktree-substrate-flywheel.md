# v0.0.32 — worktree-proof dogfood substrate + adaptive flywheel

**Status:** approved (goal-directed), in build.
**Shape:** two logical phases developed on one branch (`feat/v0032-substrate-flywheel`),
built + locally installed after Phase 1 so Phase 2 is genuinely dogfooded on the
substrate, then **cut once** — no intermediary tag (the substrate release would be
superseded immediately). Single shipped version: **0.0.32**, CHANGELOG carrying both
phases.

**Non-negotiable quality rule:** no loss in any quality gate — only improvements.
Every fix ships with tests; the workspace barrier (`cargo fmt --check` +
`cargo clippy --workspace --all-targets -D warnings` + `cargo test --workspace`) stays
green; the nightly goes from red to green; each engine change carries an FMECA
prevent→detect→fail-fast note. No fallbacks, no no-op knobs (per project norms).

---

## Phase 1 — trustworthy dogfood substrate (WS A–E)

These make *developing praxec through praxec* reliable: green gate, worktree-proof
config, right-repo writes, heartbeating drives, correct snippet defaults.

### WS-A — green the nightly (CI/harness; small)

**Root cause (confirmed, not a product bug):** `crates/praxec-core/tests/walk_examples.rs`
builds its runtime with `AlwaysNoopRegistry`/`NoopExecutor` (`walk_examples.rs:30-60`),
which returns one fixed payload for every executor kind. It never dispatches the real
`parallel` executor or runs `cli` scripts, so the 4 pattern examples' output mappings
resolve to `null`, and the typed-slot guard *correctly* rejects writing `null` into a
`type: string`/`array`/… slot (`runtime_records.rs:47`, `:89-94`;
`runtime_chain.rs:1050-1051`,`:1127-1128`). All four are already `#[ignore]`d for this
reason. The bug is only that the nightly force-runs them:
`.github/workflows/nightly.yml:46` — `cargo test --workspace -- --include-ignored`.

Affected examples: `examples/pattern-recovery/gateway.yaml` (`attempt_result: string`
← `$.output.result`), `pattern-parallel`, `pattern-dynamic-fanout`,
`pattern-evidence-quorum`. Real coverage lives in `crates/praxec-executors/tests/`,
`evidence_guard.rs`, `reliability.rs`.

**Fix (quality-improving, not a mask):** convert the 4 bare `#[ignore]` walks to an
**env-gated skip** matching the nightly job's advertised "env-gated tests" contract —
each returns early with a clear log unless a real executor registry is available (an
env flag like `PRAXEC_WALK_WITH_REGISTRY`). This makes intent explicit and stops
`--include-ignored` from force-running harness-limited tests.
**Improvement rider (in-scope, higher value):** also add a *positive* walk that gives
`walk_examples` a real `ExecutorRegistry` (dev-dep `praxec-executors` on `praxec-core`
tests only) for the deterministic patterns, so the shipped examples ARE walked
end-to-end rather than merely skipped — net gain in coverage, not a deletion.
**Also:** de-dupe/auto-close the 17 stale `nightly-failure` issues once green.

FMECA: prevent (env-gate = tests can't be force-run in the wrong context) → detect
(the positive registry walk actually exercises the examples) → fail-fast (a real
type error in an example now fails a test that *can* pass).

### WS-B — worktree-proof repo + path resolution (engine; large; the centerpiece)

**Root cause:** three couplings make absolute worktree paths load-bearing:
1. **Repo identity ≡ filesystem path.** Config addresses repos by literal path
   (`parse_repo_entry` → `RepoDecl`, `config.rs:3543-3712`; only `~` expansion,
   `config.rs:3748-3751`). Writable roots hard-fail boot when the path is dead
   (`writable_repo_roots_from_config`, `gateway.rs:1797-1827`; `RepoRoot::new`
   canonicalize+is_dir, `run_env.rs:55-69`). A pruned worktree = boot failure.
   (Asymmetry: dead *definition* repos are skipped with `REPO_LOAD_SKIPPED`
   (`config.rs:3067-3100`) but dead *writable* roots are fatal.)
2. **Connection args are opaque verbatim strings** (`mcp.rs:173-181`, spawn `:326`;
   children process-cached for the lifetime `:401-404`). No interpolation → operators
   pin `--storage-state` absolutely → pin dies with the worktree.
3. **Selector gap:** `CommandArgs` has **no `repo_root` field** (`args.rs:258-308`);
   `dispatch_command`'s start reshape (`handlers.rs:1254-1259`) omits it. With N>1
   writable repos, every `praxec.command` start is `REPO_ROOT_AMBIGUOUS`
   (`resolve_run_repo_root`, `runtime.rs:524-583`) with no in-band remedy. CLI
   orchestrate passes `None` too (`gateway.rs:582`, `:2930`).

**Design (Fable — B+C+selector, 4 sub-phases, dependency-ordered):**

- **B1 — close the selector gap (small, standalone, unblocks the live 3-writable
  config immediately).** Add `repo_root: Option<String>` to `CommandArgs`
  (`args.rs:258`); thread it through the `dispatch_command` start reshape
  (`handlers.rs:1254-1259`); add a `--repo-root` flag on the CLI orchestrate call
  sites (`gateway.rs:582`,`:2930`). The selector accepts **a repo name** as well as a
  path (names become durable in B3). Reuse the FB-3 subpath allowlist
  (`runtime.rs:546-563`) — never inject an out-of-tree root.
- **B2 — path interpolation + durable state dir (removes the `--storage-state`
  pin).** Two token families resolved at **config-resolve time** (not run time —
  connection children are process-cached, so run-scoped tokens in `args` are
  structurally wrong):
  - `${repo:<name>.root}` — legal in `connections.*.args`/`env`; resolves to the
    named repo's current root; unresolvable at first lease → typed
    `CONNECTION_REPO_UNRESOLVED` naming token+repo.
  - `${praxec.state_dir}` — a durable, worktree-independent operator-state dir
    (`~/.local/state/praxec/`, XDG). The correct home for `--storage-state` (auth is
    operator credential material, not repo data). `praxec check` validates every token
    against declared repo names at load (hard error on typo).
- **B3 — identity-first repos + declared discovery (the "works regardless of worktree
  churn" core).** Config names a repo by identity; a discovery rule supplies the
  current path; the worktree-local `praxec.repo.yaml` stub is the identity witness:
  ```yaml
  repos:
    - name: autopilot-qa-target
      worktrees_of: /mnt/c/Working/Autopilot/autopilot-beta   # durable anchor
      writable: true
  ```
  Resolution at boot/reload: `git worktree list --porcelain` on the anchor, keep
  worktrees whose `praxec.repo.yaml` `name:` **equals** the declared name,
  canonicalize via `RepoRoot::new`. **Zero matches is a legal boot state**
  (declared-unresolved, visible in status/home); starts against it fail typed at the
  run boundary (`REPO_UNRESOLVED: repo '<name>' declared via worktrees_of=<anchor>
  matched no worktree carrying praxec.repo.yaml name=… — plant the stub or create the
  worktree`). **Two matches → `REPO_IDENTITY_AMBIGUOUS`** listing both (never a silent
  pick). Path-declared repos keep today's hard boot failure. Builds on FB-2 stubs
  (`config.rs:3053-3061`) + FB-3 subpath allowlist.
- **B4 — stub `scaffold:` auto-create (convenience, dirs only).** A stub may declare
  `scaffold: [.praxec/qa-auth, .praxec/qa-artifacts]`; the engine `create_dir_all`s
  the **directories** idempotently when the root resolves (extends the v0.0.26
  `artifacts_dir` precedent, `run_env.rs:187-196`). **Files never auto-create** — a
  missing `storage-state.json` is a fail-fast with the seed remedy (the engine cannot
  mint credentials; a silently-empty auth state produces a false-green "auth
  required").

**Poka-yoke/fail-fast:** discovery is bounded (declared anchor, exact stub-name match);
ambiguity is a typed refusal listing candidates; every unresolved state is typed and
carries its remedy; no no-op knobs (a zero-match `worktrees_of:` repo is visible and
fails loudly on use); `RepoRoot`'s invariant is untouched (discovery only emits values
through `RepoRoot::new`). Ambient CWD/`git rev-parse` resolution is **rejected** as the
primary mechanism — it is the anti-poka-yoke that silently picks the wrong repo (the
#69 corruption class).

**Reload interaction:** `reload_gated` (`gateway.rs:1588-1652`) must additionally
respawn any cached MCP connection whose interpolated args changed, else a repoint
serves stale children.

**Backward compatible:** `path:` entries unchanged; `_writableRepos` gains `name`;
`from_persisted` (in-flight instances, `run_env.rs:75-82`) untouched; two-tool schema
gains one additive optional field.

**FMECA highlights:** discovery→wrong worktree (exact stub-name match +
`REPO_IDENTITY_AMBIGUOUS`); stub accidentally committed (git-exclude convention +
ambiguity catch); worktree pruned mid-run (`from_persisted` keeps run loadable, file
tools fail typed on dead path, **engine boots fine — the headline win**); stale cached
MCP child after repoint (reload diff → respawn); token typo (`check` hard error);
missing seed file (lease-time existence check → typed error w/ seed command).

### WS-C — #69 build-loop wrong-repo write (pack; one line)

`cap.implement.build-loop.yaml` correctly uses `$.run.repo_root` everywhere. The bug is
the caller: `flow.implement.deliverable.yaml:133-165` (`building` state) spawns
build-loop as `kind: workflow` **without** a `repoRoot:` override, so the child
inherits the parent root and writes/commits there — ignoring the deliverable's
`backstop_cwd` (`flow.implement.deliverable.yaml:70`). Engine already supports the
per-spawn override (`workflow.rs:271-312`; resolves+bounds via `runtime.rs:524-583`).
**Fix (pack, cognitive-architectures):** add `repoRoot: "$.workflow.input.backstop_cwd"`
to that spawn. Add a regression test that the child's `$.run.repo_root` is the routed
repo. No engine change.

### WS-D — #70 build-loop heartbeat + honor client abort (engine; medium)

The multi-slice loop is the engine's auto-drive chain walker
(`run_deterministic_chain`, `runtime_chain.rs:393-527`, the `loop {` at `:405`), not
pack YAML. Two gaps:
1. **No cancellation observation.** The loop never checks `instance.cancelled_at` nor a
   token between hops. The rmcp client-abort token is *dropped*: `call_tool` captures
   only `context.peer` (`lib.rs:1289-1296`) and never `context.ct`. An aborted
   `praxec.command` keeps driving.
2. **No loop-level heartbeat.** Only per-agent-*turn* heartbeats exist
   (`rig_runner.rs:84`,`:94-130`,`:672-681`,`:1282-1344`) bridged to the peer by
   `PeerBridgeAuditSink` (`progress.rs:59-94`). Gaps *between* hops (verdict scripts,
   commits, review routing) emit no pulse.

**Fix (engine):** capture `context.ct` at `lib.rs:1289-1296` and thread into the drive;
observe it at the loop top (`runtime_chain.rs:405-411`, next to `refresh_run_leases`) —
re-read `cancelled_at`/token and return a `Cancelled`-style outcome mirroring the
livelock-quarantine early-return (`runtime_chain.rs:437-464`); emit a `chain.heartbeat`
audit event per hop (rides the existing `progress.rs` bridge like `agent.heartbeat`).
Tests: abort mid-drive stops the burn; a long deterministic hop still pulses.

### WS-E — #71 snippet-input null-default clobber (engine; small)

The merge mechanism already landed in v0.0.29 (`synthesize_input_schema`,
`config.rs:745-787`; `apply_schema_defaults` at start, `runtime.rs:1016-1019`). Residual
gap: **present-but-null clobber.** `apply_schema_defaults` (`runtime_schema.rs:17-25`)
applies the default only on the *absent* (`None`) arm; a key present as `Value::Null`
takes the recurse arm (`:24`) and the scalar default is not applied. When a caller maps
an optional snippet input to a scope path that resolves to null (e.g.
`flow.implement.deliverable.yaml:158` `cargo_scope: "$.workflow.input.deliverable.cargo_scope"`
with `cargo_scope` omitted), the injected `null` survives and defeats the default →
spurious unresolved-arg permanent error (`arg_render.rs:49-52`).
**Fix (engine):** in `apply_schema_defaults` treat `Some(Value::Null)` as absent for a
scalar-typed property (fill the schema default over `null`). Test the null-clobber case.
Clean up the now-redundant pack workarounds (`cap.implement.build-loop.yaml:33-52`,
`flow.implement.deliverable.yaml:62-70,148-163`).

---

## Phase 2 — adaptive model-selection flywheel (WS F–G)

Built + dogfooded on the locally-installed Phase-1 substrate. The flywheel
(`crates/praxec-core/src/deescalation.rs`, wired via `praxec cost propose`) already
works; these close its two blind spots.

### Shared prerequisite — model-identity normalization

`load_current_chains` builds chain identity as `"{provider.display_name()}:{model}"`
(`gateway.rs:3336`); the catalog uses `"{vendor}:{model}"` (`model_catalog.rs:56-58`);
several catalog fns already normalize by stripping the `vendor:` prefix
(`model_catalog.rs:132`). Establish one canonical model-identity normalization used by
both flywheel keying (F) and catalog dedup (G) so a fair-trial candidate can't be
proposed for a model already in the chain and evidence dedups correctly.

### WS-F — #12 effort-aware flywheel (engine; medium)

**Blocker (confirmed):** two efforts exist. The composer's phase effort is written to
`agent.invoked` (`runtime_chain.rs:746-747`,`:829-830`) — the *intent*. The executor's
**applied** per-hop effort is resolved independently in the chain-walk
(`executor.rs:782-819`, `hop_effort`/`applied_effort`) and threaded into
`attempt_cfg.reasoning_effort` — but **never emitted to any audit event**
(`ExecutorTelemetry` has no effort field, `executor.rs:480-489`; `agent.completed` has
no effort, `runtime_chain.rs:876-888`). **Keying nuance:** on a pass,
`observations_from_audit` takes model+cost from `agent.completed` (the *actual*
walked model, which can differ from the composer's), `deescalation.rs:146-152,168-169`
— so the applied effort must be paired with THAT model, i.e. emitted on
`agent.completed`, not `agent.invoked`.

**Touch points:**
1. Add `effort: Option<String>` to `ExecutorTelemetry`; populate from `applied_effort`
   (`executor.rs:480-489`/success branch `:836-855`).
2. Add `"reasoning_effort"` to the `agent.completed` payload
   (`runtime_chain.rs:866-888`).
3. `observations_from_audit` — read it in the `agent.completed` arm
   (`deescalation.rs:146-152`); add `effort` to `StepObservation` (`:173-179`).
4. `aggregate` — key on `(affinity, model, effort)` (`deescalation.rs:194-197`); carry
   effort onto `ModelStats`.
5. `load_current_chains` — fold each binding's `.effort` into chain identity
   (`gateway.rs:3336`); `propose` base-lookup/candidate-filter
   (`deescalation.rs:244-259`) match on the same `model@effort` identity.

Leave the composer effort on `agent.invoked` (it's the intent) but key **only** on the
`agent.completed` effort — mixing double-counts a model across efforts that never ran.
Tests: same model at two efforts produces two distinct buckets; escalated-model pass
keys on the walked model's effort.

### WS-G — #13 fair-trial exploration (engine; medium)

`propose` (`deescalation.rs:234-398`) is pure-exploit: base must have `runs >= min_runs`
(`:244-253`), candidates filtered to `runs >= min_runs` (`:254-259`); the catalog is not
referenced. A zero-evidence catalog entrant can never surface.

**Fix:** thread the model catalog into `propose` (extend signature or sibling fn; caller
`cost_propose_cmd`, `gateway.rs:3399`, already has chains+stats+params). Add a branch in
the per-affinity loop (after RAISE/LOWER, `deescalation.rs:393-395`) that surfaces an
untried, catalog-fit, under-cap model with `< min_runs` evidence as a **governed
recommendation** (never auto-applied — reuses the existing `--request-approval`
`model-base-change` queue). Catalog API: `model_catalog()` → `.models`;
`ModelEntry::fit(&[Affinity])` / `Affinity::score` (`config.rs:55-65`);
`output_usd_per_million` + `is_frontier(m, cap)` (under-cap = `!is_frontier`);
`effort_supported`; `ModelEntry::model_string()`. Affinity→dimension mapping exists
(`Affinity` enum + `score`, `config.rs:27,44-65`; `from_str` aliases `:82-95`). Dedup
via the shared normalization. Tests: a fit under-cap entrant is recommended; an
already-charted or over-cap model is not; recommendation is governed, not auto-applied.

---

## Quality gates (only improvements permitted)

- Workspace barrier green: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, `cargo test --workspace`.
- **Nightly goes green** (WS-A) — a net improvement to the safety net; the positive
  registry walk adds coverage.
- Every WS adds tests for its fix (listed per-WS above). No pre-existing test is
  weakened or deleted; the WS-A env-gate preserves the ignored tests' real coverage
  (which already lives elsewhere) and adds a positive walk.
- Each engine WS carries an FMECA prevent→detect→fail-fast note; new failure states are
  typed and diagnosable (no fallbacks / no-op knobs).
- `praxec check`/`doctor` extended where WS-B adds config surface (token validation,
  stub-name uniqueness).

## Dogfood execution (using praxec to develop praxec)

Drive through the installed `praxec` from a dedicated worktree, stepwise via
`praxec command` (never `orchestrate` — it strands on human/deterministic gates). CPM
plan → file-disjoint waves via `flow.cohort.compiled-stack`. Engine-core WS (B, D, F, G)
are complex-core class — expect some hand-completion where the build-loop can't sign off
(final_answer ceremony, per prior findings); the plan/verify/commit/mark spine still
runs through praxec either way, and that boundary is itself a dogfood finding.

**Bootstrap ordering:** WS-B fixes the very worktree-drive breakage the dogfood relies
on, so **B1 (selector gap) lands first** (partly by hand) to make command-surface starts
selectable, then the rest drives on it. Suggested sequence:
1. WS-A (green gate) + WS-C (pack one-liner) + WS-E (small engine) — quick wins, restore
   the safety net.
2. WS-B1→B2→B3→B4 — the substrate centerpiece.
3. WS-D — heartbeat/abort.
4. **Build + locally install** the Phase-1 substrate binary.
5. WS-F then WS-G on the substrate — dogfood the flywheel.
6. Cut **0.0.32** once (bump workspace + 8 inter-crate pins, finalize CHANGELOG with
   both phases), PR feat→dev, dev→main, tag, release binaries, install.

## Out of scope / separate track

**Immediate QA-harness repair** (current binary, operational — not engine work):
recreate `qa-seed-auth.mjs` in the version-controlled pack dir (not git-excluded in the
checkout), seed `storage-state.json`, fix the two stale seed-comments in
`gateway.yaml:197-198,214-215`, de-ambiguate the 3 live writables (B1 gives the durable
fix). Surfaced to the operator; not executed against the live config without a go.
