# Pack provenance-recording (replaces the rejected lockfile)

**Context:** the Cargo-style lockfile was rejected for workflows — pinning to old SHAs is
counterproductive when newer = more capable and upstream CI gates bad updates (see
[[project_workflow_currency]]). The governance-grade answer to "reproducibility" for a governance
engine is **provenance, not pinning**: record which pack version drove each run so you can reconstruct
the PAST, without freezing the FUTURE. Currency stays the default (`pack update` pulls latest); this
just makes "what exactly ran" always answerable.

## Goal
For every run, be able to answer: *which git SHA of which pack drove this operation?* — from the audit
trail (durable) and from a live query (what's loaded right now). No constraint on what loads; pure record.

## Mechanism (three surfaces, minimal→optional)

### 1. Load-time provenance audit event (the durable record) — REQUIRED
At config load / reload, for each git-backed loaded pack, capture:
`{ namespace, source (uri|path), resolved_sha, ref, dirty }`.
Emit ONE audit event (e.g. `pack.provenance`) listing all loaded packs' provenance, with the load
timestamp + correlation. Because the audit trail already timestamps every run, "load provenance at T"
+ "run happened at T" reconstructs the version that drove any run. This is the durable governance record.
- **Reuse:** option-1's `git_currency` (staleness feature) already computes branch/SHA/dirty per
  `path:`/git-backed pack — provenance emits the SAME data as a record instead of a warning. Build
  AFTER option 1 merges to avoid duplicating that introspection. For remote `uri:` packs, the resolved
  SHA is `git rev-parse HEAD` in the clone cache dir (`repo_git`).

### 2. `home()` / query surface (live "what am I running") — REQUIRED
`discovery::home()` today reports only the gateway BINARY version (discovery.rs:503-509). Add a
`loaded_packs` section: per pack `{ namespace, source, sha (short), ref, dirty }`. So `praxec.query {}`
answers "what workflow versions am I running" live — the operator-facing complement to the audit record.

### 3. Per-run SHA stamping (stronger, OPTIONAL follow-on)
Stamp the driving pack SHA(s) directly into each mission's run metadata / audit events, so a run CARRIES
its provenance rather than being time-correlated to a load event. Stronger for high-concurrency / rapid-
reload cases, but more invasive. Defer unless the time-correlation of (1) proves insufficient.

## Non-goals (explicit)
- **No pinning / no lockfile / no constraint on loading.** Provenance RECORDS, never CONSTRAINS.
- Hard version pinning remains a separate, OPT-IN concern for a future production/release gateway only.

## Composition with the rest of the currency work
- **Staleness warning (opt 1):** "you're drifted" (warn). **Provenance:** "here's exactly what's loaded"
  (record). Same `git_currency` introspection, two outputs. Provenance builds on opt 1.
- **Remote sourcing (opt 2, #149) + `pack update` (opt 4, pull-latest):** keep you current; provenance
  records whatever currency you landed on. Together: stay latest, always know what ran.

## Build increments (assert-first, after opt 1 merges)
- **P1:** load-time `pack.provenance` audit event (reuse `git_currency`); assert: a config with a
  git-backed pack emits an event carrying that pack's namespace + resolved SHA + ref + dirty.
- **P2:** `home()`/query `loaded_packs` surface; assert: `home()` includes each loaded pack's sha/ref.
- **P3 (optional):** per-run SHA stamping — only if (1)+(2) prove insufficient.
Engine work → praxec dev feature branch; non-blocking, additive; offline-safe (local git rev-parse only).
