# Build Throughput for CPM-Driven Parallel Autonomous Development

Status: design/research pass, 2026-07-09. No code changed. Recommendations ranked in §5.

## 1. Problem

Driving a single praxec-crate deliverable through `flow.execute-cohorts` →
`flow.implement.deliverable` → `cap.implement.build-loop` takes 20–30+ minutes.
Two compounding causes:

1. **Every red/green measure step runs `cargo test --workspace`** over a
   12-crate, ~2,400-test, 173-integration-test-binary workspace — at minimum
   twice per slice (RED + GREEN), more on retries, plus `cargo clippy --workspace`
   on every GREEN.
2. **Drives run serially** because of a working rule ("one cargo at a time —
   concurrent invocations serialize on `target/debug/.cargo-lock`"), so the
   per-deliverable cost is not amortized across a multi-deliverable plan.

The verdict script's own comments record the failure mode this already caused:
whole-workspace testing per iteration hit the 600 s `TEST_TIMEOUT` and produced
null verdicts (dead-stalls) — see `scripts-library/verify.test.tdd-verdict.yaml`
lines 39–42 and 106–110.

## 2. What the current setup actually is (grounded)

Verified on this machine 2026-07-09 (read-only; no builds run):

| Fact | Evidence |
|---|---|
| Workspace: 12 crates, ~130 K LOC, 751 locked packages | root `Cargo.toml`, `Cargo.lock` |
| ~2,433 `#[test]`/`#[tokio::test]` fns; praxec-core holds 1,195 | grep count per crate |
| 173 integration-test **binaries** (each links the full dep graph); 90 in praxec-core alone | `crates/*/tests/*.rs` count |
| Each worktree has its own `target/` (`drive-a/target` = 47 G; main = 20 G; drive-b/p14/etc. currently have none → cold start on next build) | `ls .worktrees/*/target`, `du` |
| No `.cargo/config.toml` anywhere (repo has only `audit.toml`; `~/.cargo/config.toml` absent); no `rust-toolchain` file | filesystem |
| `sccache 0.13.0` **installed, daemon running, never used** (0 compile requests) | `sccache --show-stats` |
| `mold 2.41.0` installed, unwired; **cargo-nextest NOT installed** | `which`, `~/.cargo/bin` |
| rustc 1.97.0 **already links with lld by default** on x86_64-unknown-linux-gnu | `rustc --print link-args` shows `-fuse-ld=lld` |
| `[profile.dev] debug = 1` already set (reduced debug info) | root `Cargo.toml` |
| Verdict script accepts a `CARGO_SCOPE` arg ($6, default `--workspace`) for both `cargo test` and `cargo clippy` | `verify.test.tdd-verdict.yaml` :43, :104, :119 |
| **Wiring gap**: `cap.implement.build-loop` declares a `cargo_scope` input but both measuring steps pass the literal `"--workspace"` as arg 6 (`red_measuring` :162, `green_measuring` :287); `flow.implement.deliverable` passes `cargo_scope: ""` which the cap never reads | the three YAML files |
| cpm-planner circuit-breaker exists: `MAX_ATTEMPTS = 3`, spent lease budget → `DeliverableStatus::Failed` (permanent, poisoned out of the plan) | `cpm-planner/src/planner.rs` :70, :583 |
| Host: 16 cores, 23 G RAM (WSL2), 596 G disk free | `nproc`, `free`, `df` |

## 3. Findings per research question

**Hard gate applied to every finding below:** no optimization may weaken the
per-slice TDD determinism. RED must still observe the new test **compiling and
its assertion failing**; GREEN must still observe the tests **passing** — as
engine-measured evidence, per slice, every slice. Anything that batches away the
per-slice red/green measurement, or lets a slice through without its own
failing-then-passing evidence, is disqualified regardless of speed. Everything
recommended below is a *semantics-preserving* speedup: it changes how fast the
evidence is produced, never whether it is produced.

### Q1 — Is the serial constraint real? **No (across worktrees). It was a misdiagnosis.**

Cargo's build lock is a per-invocation `flock` on `target/<profile>/.cargo-lock`
— i.e. **per target directory**. Two worktrees with separate `target/` dirs hold
separate locks and never contend on builds. The only cross-process shared state
is `$CARGO_HOME` (registry/git caches), and since Cargo 1.74 the package cache
uses fine-grained locking (shared for reads, exclusive only while
downloading/mutating), so concurrent builds serialize only briefly on new
downloads — not on compilation.

The "one cargo at a time" rule is correct **within one target dir** (two cargo
invocations in the same worktree do queue on `.cargo-lock` and look hung). It
was over-generalized to all invocations. The earlier "runaway" symptom is far
better explained by the control plane: multiple orchestrate processes
re-leasing/retrying the same plan deliverables in a loop — exactly the failure
cpm-planner's circuit-breaker now bounds (`MAX_ATTEMPTS = 3`, permanent-fail
poisoning, `planner.rs:583`). That fix removes the actual runaway mechanism.

**Safety conditions for N parallel drives** (all currently satisfiable):

1. One drive per worktree; each worktree keeps its default private `target/`
   (do NOT set a shared `CARGO_TARGET_DIR`).
2. Never two cargo invocations inside the same worktree (the original rule,
   correctly scoped — the build-loop is already serial per driver, so this holds
   by construction).
3. Distinct `caller_id` per driver so cpm-planner's file-disjoint locks hold.
4. Circuit-breaker present (it is: `MAX_ATTEMPTS = 3`).
5. **Resource cap: N = 2 on this box.** Each cargo uses all 16 cores by default
   and linking 90+ debug test binaries is RAM-hungry; 23 G RAM under WSL2 makes
   N = 3 an OOM/thrash risk. Optionally set `CARGO_BUILD_JOBS=8` per drive to
   split cores cleanly. Revisit N after measuring.

**Recommendation: allow parallel drives again (N = 2), and rescope the recorded
rule to "one cargo per target dir".** This is an operational change, zero code.

### Q2 — Shared compilation cache

- **(b) Shared `CARGO_TARGET_DIR` for all worktrees: REJECT.** It reintroduces
  the single `.cargo-lock` — N drives fully serialize again, which is the exact
  failure we're escaping. Worse: worktrees sit on different commits, so the same
  workspace crates alternately invalidate each other's fingerprints (rebuild
  ping-pong), making the "shared cache" actively negative. This trade
  (cache-hits for serialization + thrash) is strictly bad here.
- **(a) sccache (local disk mode): ADOPT.** This is the sweet spot because it
  decouples the two axes: each worktree keeps its own target dir and lock
  (parallelism preserved) while the **compiler outputs for the 751-package
  dependency graph are shared** through a content-addressed local cache.
  Concretely: a fresh worktree's cold build — currently a from-scratch compile
  of the entire dep graph, and drive-b/p14 are cold right now — becomes mostly
  cache hits. Honest caveats: sccache does not cache incrementally-compiled
  crates (the workspace's own crates in dev profile pass through uncached —
  fine, they're the small part) and does not cache linking. So it attacks
  cold-start and dep-graph recompiles, not the per-slice inner loop. Local disk
  mode needs **no external infra** (the daemon self-spawns; it's already
  running). Wire it as operator-machine config (`~/.cargo/config.toml`
  `build.rustc-wrapper = "sccache"` + `SCCACHE_CACHE_SIZE="40G"`), **not** as a
  repo-committed `.cargo/config.toml` — praxec is a local-binary product and
  users must not be required to install sccache.
- **(c) Incremental compilation: change nothing.** It's on by default in dev
  profile and is what makes the warm per-slice rebuild cheap. Do not set
  `CARGO_INCREMENTAL=0` for sccache's benefit; that would trade the inner loop
  for the cold path.

### Q3 — Scope the cargo verb: the wiring gap is two literals

The plumbing already exists end-to-end except for the last inch: the script
takes `CARGO_SCOPE` ($6) and applies it to **both** `cargo test` and
`cargo clippy`; the cap declares a `cargo_scope` input; but `red_measuring` and
`green_measuring` hardcode `"--workspace"` as arg 6 instead of
`"$.workflow.input.cargo_scope"`. The comments in both YAMLs show this was
half-built deliberately (to fix an unresolved-arg-path crash) and never
finished.

**Win mechanism** — the per-iteration cost of `--workspace` is not mostly *running*
2,400 tests; it's **building and linking 173 test binaries**, most of which are
untouched by the slice. `-p praxec-executors` builds/links 31 binaries and runs
224 tests; `-p praxec-core` still runs 1,195 tests but skips every downstream
crate's binaries. Estimated 3–10× on the measure step for leaf-crate
deliverables, less (maybe 1.5–2×) for praxec-core itself, times ≥2 measure steps
per slice times every retry. It also directly removes the observed 600 s-timeout
dead-stall mode.

**Correctness tradeoff and the rule:** `-p <crate>` misses reverse-dependency
breakage. So: **scope narrow per slice** (fast feedback where the loop iterates)
and **run `--workspace` once per deliverable as the final gate before
`plan.mark_status complete`** (see Q5). A deliverable is never marked complete
on scoped evidence alone.

**Threading design:** `cargo_scope` rides in `deliverable.metadata` (where the
build spec already lives), `flow.implement.deliverable` passes
`"$.workflow.input.deliverable.cargo_scope"` through to the cap, and the two
measuring args become `"$.workflow.input.cargo_scope"`. Poka-yoke the known
trap: a missing metadata key is an unresolved-arg-path **permanent error** (the
exact bug that previously killed every cohort deliverable), so the planning step
must always emit the key (empty string ⇒ script defaults to `--workspace`) and
plan ingestion should validate its presence. Two coherence notes, both already
safe: `prev_test_count` monotonicity is scope-relative, which is consistent
because scope is fixed for the life of a deliverable and the baseline starts at
0; the atomic scan is already scoped separately via `SCAN_TARGET` ($7).

### Q4 — Faster tooling, item by item

- **`cargo nextest`: DEFER.** Its real advantage here is that `cargo test` runs
  the 173 test *binaries* sequentially while nextest schedules all tests across
  binaries in parallel — a genuine win for full `--workspace` gates. But (1) it
  is not installed; (2) the verdict script parses libtest's `test result: N
  passed; M failed` lines, so adoption means rewriting the parsing (nextest's
  machine-readable output differs); (3) it adds an install prerequisite for
  every pack user unless the script detect-and-falls-back; (4) after Q3 scoping,
  the inner loop's run phase is small. Revisit only if measured timings show the
  once-per-deliverable workspace gate is run-dominated rather than link-dominated.
- **`mold`/`lld`: CHANGE NOTHING.** rustc 1.97 already links with lld by
  default on this target (verified via `--print link-args`). mold is at best a
  modest further improvement over lld, and wiring it (repo rustflags) would
  invalidate every existing target-dir fingerprint for a one-time full rebuild
  and impose a toolchain assumption on users. Not worth it; optionally
  experiment later on the operator machine only.
- **`cargo check` for RED: REJECT as a replacement.** The RED verdict is not
  "does it compile" — it is "a new test compiles **and its assertion fails**"
  (`failed_count ≥ 1`, count growth, tautology detection). `cargo check` can
  observe "doesn't compile" but can never observe a runtime assertion failure,
  so a check-only RED **weakens the red gate** — it converts the engine-observed
  discipline back into an agent promise and re-admits tautologies. It would only
  be admissible *paired with* a scoped test run (check for fast compile triage,
  then the test run for the fail evidence) — at which point the scoped test run
  alone (Q3) is simpler and already carries both signals. The right way to make
  RED cheap is Q3 scoping, which keeps the full verdict semantics.
- **clippy on every GREEN: KEEP, but it inherits the Q3 scope.** The script
  already applies `$CARGO_SCOPE` to clippy. Per-slice clippy keeps lint fixes in
  the producing agent's context; deferring lint to the final gate batches debt
  to a context-free end and adds an extra loop tier. After the first clippy pass
  in a worktree its check-mode artifacts are cached, so the marginal cost is low.
- **Dev profile: CHANGE NOTHING.** `debug = 1` is already the sensible setting.

### Q5 — Milestone-batched builds: get the benefit, keep the determinism

Dropping per-slice verdicts is rejected outright: RED-compiles-and-fails,
atomic-assertion, and monotonic-test-count are the *product* — the unfakeable,
engine-observed discipline that stops code-first/batch-assert/delete-tests
convergence. Batching slices before any build would hand correctness back to
agent self-report.

But the batching **benefit** (pay the expensive build rarely) is achievable with
a **staged verdict**, and Q3 is precisely the mechanism:

- **Per slice (hot loop):** scoped `cargo test -p <crate>` for RED and GREEN,
  scoped clippy, fmt. Full verdict semantics preserved; cost proportional to the
  crate under change.
- **Per deliverable (the "milestone" boundary):** one full
  `cargo test --workspace --no-fail-fast` + `cargo clippy --workspace` gate,
  added to `flow.implement.deliverable` between `building` and
  `marking_complete`. Pass → mark complete. Fail → the cross-crate breakage
  becomes issues fed back into the build loop (or `needs_human` at a bound) —
  never a silent mark.
- **Per milestone/release-train:** the existing hard CI gate, unchanged.

This is the user's milestone idea implemented safely: the milestone is the
deliverable/mark_status boundary, the batch is the slices within it, and no
slice ever goes unverified — it's verified against its crate instead of the
world. Note `--workspace` is exactly correct today for any deliverable whose
`cargo_scope` is empty, so this is a strict generalization, not a behavior
change for existing plans.

## 4. Explicit "change nothing" calls

- **Linker** — lld is already the default in rustc 1.97; mold not worth the churn.
- **Incremental compilation** — already on and load-bearing for the warm loop.
- **`[profile.dev] debug = 1`** — already tuned.
- **Per-slice TDD verdict structure** (RED must run and fail; clippy per GREEN;
  QA ladder; circuit breakers) — load-bearing determinism; do not weaken.
- **Per-worktree private target dirs** — keep; the isolation is what makes
  parallel drives safe. (Disk cost is real — 47 G/worktree — but 596 G is free
  and `cargo-sweep` is already installed if it ever becomes pressure.)
- **cpm-planner control plane** (max_count 1 per driver, file-disjoint locks,
  MAX_ATTEMPTS circuit-break) — already the correct design; it is what makes
  recommendation #1 safe.

## 5. Prioritized recommendations

Ranking gate, applied first: **anything that weakens per-slice TDD determinism
is out regardless of speed** (that is what disqualifies check-only RED, shared
target dir side-effects on verdict scope, and unverified batching below). Among
survivors, ranked by (expected wall-clock win × probability) ÷ (implementation +
maintenance cost):

| # | Change | Win | Prob. | Cost | Infra |
|---|---|---|---|---|---|
| 1 | **Wire `cargo_scope` end-to-end + add the per-deliverable `--workspace` gate** (staged verdict, Q3+Q5) | 3–10× per measure step on leaf-crate deliverables; kills the 600 s-timeout stall mode | High — mechanism is direct, plumbing 90% exists | ~2 literal edits in the cap, 1 input thread in the flow, 1 new state, planner metadata convention + validation | None |
| 2 | **Re-enable parallel drives, N = 2** (Q1) — operational; rescope the "one cargo at a time" rule to "one cargo per target dir" | ~2× plan throughput | High — lock mechanism is per-target-dir; runaway cause is circuit-broken | Zero code; update the recorded rule + drive runbook | None |
| 3 | **sccache, local disk mode, operator machine only** (Q2a) | Large on fresh-worktree cold starts (751-package dep graph; drive-b/p14 are cold now); no inner-loop effect | High for cold path | One env/config wiring on the dev box; already installed & daemon running | None (local disk) |
| 4 | **Emit `duration_s` from the verdict script** | Enables every future decision here (incl. whether nextest ever earns its place) | Certain | ~3 lines | None |
| 5 | `cargo nextest` for the workspace gate | Moderate (parallel across 173 binaries) | Medium | Install + verdict-parsing rewrite + pack-user prerequisite or fallback | Local |
| — | Shared `CARGO_TARGET_DIR` | negative | — | — | **Reject** |
| — | `cargo check` replacing RED test-run | breaks verdict semantics | — | — | **Reject** |
| — | Unverified slice batching to milestones | breaks determinism | — | — | **Reject** (staged verdict instead) |
| — | mold, profile tweaks, nextest-now | marginal over current defaults | — | — | **Change nothing** |

Items 1+2 compound: scoped inner loops × 2 parallel drives ≈ 6–20× on
multi-deliverable plans touching leaf crates, before sccache even enters.

## 6. Concrete minimal first step

One small PR to `cognitive-architectures` plus one operational change:

1. In `capabilities/cap.implement.build-loop.yaml`, replace the literal
   `"--workspace"` with `"$.workflow.input.cargo_scope"` in `red_measuring`
   (:162) and `green_measuring` (:287).
2. In `orchestrators/flow.implement.deliverable.yaml`, change
   `cargo_scope: ""` to `cargo_scope: "$.workflow.input.deliverable.cargo_scope"`,
   and add a `workspace_gate` state (verdict script, phase=green, scope
   `--workspace`) between `building` and `marking_complete`, routing failure to
   the build loop / `needs_human`, never to `mark`.
3. Make the planning step always emit `cargo_scope` in each deliverable's
   metadata (`""` when genuinely workspace-wide) and validate presence at plan
   ingestion — the unresolved-arg-path trap is a known killer.
4. Add `duration_s` to the verdict JSON.
5. Operationally: launch the next plan with two drivers (distinct caller_ids,
   distinct worktrees) and correct the recorded rule to "one cargo invocation
   per **target dir**".

Everything else (sccache wiring, nextest, N = 3) waits for the timings that
step 4 starts collecting.
