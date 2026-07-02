# ADR-0006: Execution sandbox & authored promotion — two-tier trust for agentic models

**Status:** Accepted (sandbox *runtime* deferred to a spike — §Decision 5)

**Date:** 2026-06-11

## Context

A new class of frontier models is **agentic by execution**: they work by running
shell/commands, reading the result, and iterating. That loop *is* the
capability. We want to leverage it — but we will not run the whole workflow on
top of the model (that abandons the deterministic harness, which is the thing
praxec *is*, [ADR-0001](0001-headless-runtime-surfaces-attach.md)), and we will
not let ad-hoc execution touch the host, production data, or secrets.

Grounding the current spine (so this is designed against the real system):

- **Nothing is sandboxed today.** `ScriptExecutor` writes a temp file and runs
  `bash` in the gateway's own process environment
  (`crates/praxec-executors/src/script.rs`); `kind: agent` `spawn()`s the
  agent binary with full host privileges
  (`crates/praxec-agents/src/runner.rs`). Either path runs with the keys to
  the kingdom.
- **The promotion machinery, however, already exists** (built on
  `feat/mission-control`). The **provenance gate**
  (`untrusted_execution.rs::untrusted_execution_reason`) forbids a *published*
  definition from introducing a raw command — no inline `kind: cli`/`kind: mcp`,
  no smuggled `connections:`; it may only run **hash-pinned `kind: script`** +
  operator-declared connections. The authoring track turns a proposal into such
  an artifact: `propose → structural_analysis → dry_run → script-ack → publish`,
  and the **`script_acknowledged` guard with hash-flip invalidation**
  (`guards.rs`) means a human approves the *exact bytes* and any change re-arms
  the gate.

So praxec already has the half that says *"the model builds the scripts/skills
it needs, the user approves them inside the harness."* What it lacks is the half
that lets the model **discover** those scripts — the iterative shell loop — without
exposing the system.

**Framing principle (TRIZ).** This is a *physical contradiction*: we want
**maximal autonomy** (run any command, iterate) and **zero blast radius**
(nothing reaches host/prod/secrets) from the *same* execution. Stated as one
knob those fight. TRIZ says: do not trade the contradiction on a slider —
**separate** it so each pole is total in its own region. We separate in **space**
(a sandbox), in **time** (before vs after approval), and by **condition**
(resource/network bounds).

## Decision

1. **Two trust tiers, separated in space and time.**
   - **Tier 1 — Discovery.** The model's exploratory shell loop runs inside an
     **ephemeral, isolated sandbox** (confined filesystem, scrubbed secrets,
     egress allowlist, resource/time budget). Inside it, *anything goes*; the
     blast radius is the sandbox, which is **discarded**. Capability is not
     kneecapped — it is quarantined.
   - **Tier 2 — Production.** What the model produces runs only as a
     **hash-pinned, acknowledged, least-privilege** artifact through the existing
     governed path. Total governance, no ambient autonomy.

2. **Trust-boundary contract: agent output is a _candidate_, never a command.**
   The sandbox yields **artifacts** (a script, a skill, a diff) — not authority
   to act. Production execution is *earned* only by authoring an artifact that
   survives `structural_analysis → dry_run → ack → hash-pin`. This is the line
   that makes the whole thing safe, and the provenance gate already enforces its
   backstop ("no raw commands in a published definition"). The model never
   crosses from Tier 1 to Tier 2 by *running* something — only by *publishing*
   something that passed the gate.

3. **Where the boundary attaches.**
   - a. A **sandbox around `kind: agent`** (the exploratory model subprocess):
     filesystem confinement to a **disposable worktree** — a copy/overlay of the
     repo at a pinned commit, **never the live host working tree or `.git`** —
     with the host FS otherwise invisible; secret scrubbing (`env_clear` +
     explicit allowlist); **egress deny-by-default** (allowlist only); and a
     resource/wall-clock budget. The only thing that leaves the sandbox is a
     **reviewed artifact** (a diff/script/skill), via the gate.
   - b. **Least-privilege for `ScriptExecutor`.** Even an *approved* script runs
     confined — not as the gateway. This is a real gap **today**, agents or not,
     and is in scope here because it is the same "execution isolation" concern at
     the production tier.

4. **Approval policy — human-in-loop by default, with a policy-bounded
   auto-approve as the "don't kneecap" knob.** An artifact wholly within declared
   bounds (worktree-only, no network, no secrets, under budget) MAY auto-ack;
   anything outside the bounds requires a human. The bounds are operator config
   and **fail-closed** (unknown → human). This is what keeps frontier models fast
   without surrendering the gate.

5. **The sandbox _runtime_ is deferred to a time-boxed spike.** Decisions 2–4
   (the boundary contract, the attachment points, the policy shape) are the
   durable, hard-to-reverse commitments; the *mechanism beneath them* — container
   (podman/docker + mounted worktree), microVM (firecracker), or lighter Linux
   primitives (`bwrap`/`landlock`, `unshare` namespaces) — is **swappable under a
   stable contract** and is the part most worth choosing empirically. The spike
   validates the *mechanism*: a script runs confined (no host FS beyond a
   worktree, no secret env, no network) and still produces output — proving the
   shape before we commit a runtime.

## Failure modes (FMECA) — irreversible operations from the sandbox

The governing answer to *"what if the model runs `git commit`, `rm -rf`, `git
push`, `curl`…?"*: the **entire Tier-1 region is disposable, and the only egress
is a reviewed artifact.** The model holds no handle that can cause an irreversible
effect on real state — durable effects are *constructed* to be reversible (a patch
you can decline or `git revert`) and gated. Per the codebase's prevent → detect →
fail-fast convention:

| Failure mode | Prevent | Detect | Fail-fast |
|---|---|---|---|
| `git commit` in the worktree | worktree is a **disposable copy/overlay** at a pinned commit, not the host `.git` | — (in-tree, harmless) | discarded on teardown; only a **diff** leaves, via the gate |
| `git push` / network exfiltration | **egress deny-by-default** + scrubbed credentials (`env_clear`) | egress attempt logged | no route, no creds → fails |
| write/delete host files (`rm -rf`, redirects) | host FS **not mounted**; only the worktree is visible (namespace / landlock) | access outside worktree denied | `EACCES`/`ENOENT`; blast radius = disposable tree |
| resource abuse (fork bomb, fill disk, spin) | cgroup/quota + wall-clock budget | budget breach | killed; tree discarded |
| **external side effect** (call an API, send mail, delete a cloud resource) | egress deny-by-default; external effects only via **operator-declared, governed connections** — never raw `curl` | allowlist miss logged | blocked; the effect must be *authored* as a `kind: script`/connection and pass the gate |
| published artifact ≠ what was tested | hash-pin + `dry_run` + `diff` review of the **exact bytes** | hash-flip re-arms the ack | `script_acknowledged` guard rejects on mismatch |
| sandbox escape (confinement vuln) | runtime choice (microVM > container > namespaces) — the residual that drives Decision 5 | host syscall/IDS audit | defense-in-depth; the spike + threat model size this |

**Principle: reversibility by construction + artifact-only egress.** A `git
commit` inside the sandbox is exactly as harmless as a note on a whiteboard we
then wipe; what survives is the photo we *chose* to keep — the reviewed artifact,
applied by a human (or a policy within declared bounds) and itself revertible. The
single genuinely-hard residual is the **external side effect** (network egress):
the sandbox cannot reason about whether a reachable endpoint is idempotent, so
egress is **deny-by-default** and any real-world effect must leave Tier 1 as an
authored, governed connection — never an ad-hoc command. That is the one failure
mode the boundary cannot make reversible, so it is the one it forbids outright.

## Consequences

- **Positive.** Autonomy is not kneecapped (full freedom in the cage) and the
  ad-hoc blast radius is zero. Reuses the authoring track — the promotion half is
  already built and tested. Closes a latent `ScriptExecutor` privilege gap.
  "Agent output = candidate" is an auditable, enforceable contract, not a
  convention. The runtime stays swappable beneath a stable boundary, so the
  hardest-to-reverse parts are decided first and the most-reversible last.
- **Costs.** A sandbox runtime + lifecycle (create / mount / scrub / teardown);
  the discovery→authoring bridge (emitting sandbox artifacts as candidates); the
  auto-approve policy engine + its bound vocabulary; per-OS portability of the
  confinement primitive; first-promotion latency.
- **Sequencing.** Spike a **sandboxed `ScriptExecutor`** first (worktree +
  env-scrub + no-network default) — the smallest real thing that exercises
  confinement and surfaces the ergonomics — then the `kind: agent` boundary, the
  discovery→authoring bridge, and the policy engine.

## Alternatives considered

- **Run the whole workflow on the frontier model (let it orchestrate).**
  Rejected — abandons the deterministic, governed harness; that harness is the
  product.
- **Let the model shell out ad-hoc with gateway privileges (today's default).**
  Rejected — unbounded blast radius; one prompt-injection or bug reaches host,
  prod, and secrets.
- **Approve every command (pure HITL, no sandbox).** Rejected — per-command
  approval latency kills the iterative loop; this is the kneecapping we are
  avoiding.
- **Sandbox only, trust its output (no promotion gate).** Rejected — a sandbox
  confines the *discovery* blast radius but does not make the output safe to run
  against **production**; the hash-pin + ack are still required to run the
  discovered artifact in a real workflow.
- **One autonomy-vs-safety slider.** Rejected — that *is* the contradiction;
  separation beats compromise (the whole point of the TRIZ framing).

## Amendment (2026-06-11) — cross-platform: a per-OS provider over an always-Linux sandbox

praxec must run on **Linux, macOS, and Windows**, and there is **no single
native lightweight sandbox primitive across all three**: the validated Linux
mechanism (bubblewrap) is kernel-namespace-based and Linux-only; macOS
(`sandbox-exec`/Seatbelt, or a VM) and Windows (AppContainer/Job Objects, or a
VM) are different in kind. Decision 5 already split the durable **contract** from
the swappable **runtime**; this amendment fixes the cross-platform shape of the
runtime:

1. **The sandbox _target_ is always Linux.** The agent's work is Linux shell /
   git / build-tool work, so the confined environment is Linux on every host.
   That yields **one** confinement model and **one** threat model instead of
   three; the host OS determines only how that Linux is *provisioned*.

2. **A `SandboxProvider` seam** with backends behind the OS-agnostic boundary
   contract: `bwrap` (Linux-native fast path — rootless, daemonless, instant),
   `oci-container` (**portable default** — a Linux sandbox on every host), and
   `microvm` (hardened). The contract never changes per OS; only which backend
   enforces it.

3. **Per-OS provisioning of the Linux sandbox:**
   - **Linux** → namespaces / bubblewrap, or an OCI container.
   - **macOS** → a Linux VM (Apple Virtualization.framework via Lima/Colima, or
     Docker Desktop).
   - **Windows** → a WSL2 / Hyper-V Linux VM (Docker Desktop or WSL2).
   On macOS/Windows the VM boundary is **stronger** than bwrap — it closes the
   kernel-escape residual (Decision 5 / FMECA) for free.

4. **The disposable worktree is copied into the Linux sandbox**, so the host's
   real working tree — whatever its OS, NTFS or APFS or ext4 — is never the
   sandbox FS. This keeps the FMECA contract ("never the live host tree") intact
   across OSes by construction.

The spike validated bwrap **7/7 rootless on WSL2** — which is *also* the Windows
provisioning path (WSL2), so that one run covers Linux-native **and**
Windows-via-WSL2; the OCI-container backend is the portable default that also
covers macOS. `SandboxProvider` is the swap point ADR-0006 Decision 5 anticipated.

## Preflight & provisioning (2026-06-11)

The sandbox depends on host infrastructure (bubblewrap / a container runtime / a
VM). praxec must therefore **fail closed with an actionable remedy** when it's
absent — never crash mid-execution and never silently skip the boundary. This
extends the existing `px doctor` pattern
(`crates/praxec-tui/src/doctor.rs`: structured `CheckResult`
Pass/Warn/Fail/Skip, each tied to a failure mode and carrying a remedy; it
already does *live probes*, not mere presence checks).

1. **Each `SandboxProvider` backend owns `preflight()` and `install_hint()`.** The
   doctor iterates registered providers asking "can you run here, and if not how
   do I fix it?" — single source of truth: the component that *enforces* the
   boundary is the one that *checks and provisions* it. No per-OS logic hardcoded
   in the doctor.

2. **Presence ≠ functional — preflight smoke-tests.** The check runs a trivial
   confined operation and asserts isolation (as the spike did), not `which bwrap`.
   This catches the real failure: e.g. `bwrap` present but **unprivileged user
   namespaces disabled**, which passes a presence check and fails at runtime.
   Mirrors the doctor's existing live-probe philosophy.

3. **Capability-gating, fail-closed — the sandbox gates a *capability*, not the
   product.** With no validated sandbox, the **untrusted agentic-execution tier**
   (`kind: agent`; the future sandboxed `ScriptExecutor`) is **disabled with a
   clear remedy**; governed workflows and hash-pinned scripts still run. (Mirrors
   the cockpit's opt-in embedding gate — a missing capability narrows what you can
   do, it doesn't stop praxec from starting.)

4. **Provisioning policy: detect + instruct by default; opt-in to run; never
   auto-install.** `px doctor` reports what's missing and the exact remedy;
   `px doctor --fix` may **offer** to run that exact command, but **only
   with explicit user consent** — it never installs silently, never auto-escalates
   privileges. Installing bubblewrap / Docker / WSL2 is system-level and
   outward-facing: the operator's call to run, not praxec's to do behind their
   back. Consistent with the git-auth model ([ADR is git-piggyback — praxec
   manages no credentials]) and the project's fail-fast / no-silent-fallback
   stance. Per-OS hints the backends surface: Linux → `apt install bubblewrap`
   (+ enable unprivileged userns) or a container runtime; macOS → Lima/Colima or
   Docker Desktop; Windows → enable WSL2 or Docker Desktop.

## Coordination: free exploration, coordinate-at-promotion (2026-06-11)

praxec already coordinates concurrent *trusted* work by **file-set locks**
(`repo_locks::LockSpace` — atomic, all-or-nothing, TTL'd, deadlock-free; the
Planner's `acquire_cohort` only hands out **mutually disjoint** sets). The
tempting move — have the agent's sandbox acquire a lock on its files so its diff
merges cleanly — **fights the sandbox's purpose**: a lock requires declaring the
file-set *up front*, but an exploring model *discovers* what it touches. Declare-
then-explore kneecaps the freedom the sandbox exists to give. This is a real
contradiction; the resolution is again **separation in time (do-then-declare)**.

**The model needs no lock to *explore* — only to *promote*.** Inside a private
disposable copy there is nothing to coordinate against; `git commit` / `rm -rf` /
overwrite are all harmless (the confinement spike proved the host is untouched).
Coordination is a *promotion* concern, not an exploration one. So:

1. **Tier-1 exploration is lock-free and unconstrained.** The model touches
   anything in its copy; the harness never imposes a lock and the model never
   sees one.
2. **At promotion, coordinate on the *observed* set.** By then the file-set is
   *known* — the harness watched it (`git diff --name-only`). The **promotion
   bridge** acquires a `LockSpace` lock on exactly those files for the brief apply
   window and **3-way merges** the diff onto the live tree, then releases.

Consequences:

- **Both freedoms, no compromise.** The model runs fully free; **disjoint
  observed-sets still apply concurrently conflict-free** — the same partition
  merge-freedom `LockSpace` gives, computed on the *observed* set instead of a
  *declared* one. Only genuine overlap produces a normal 3-way merge conflict,
  surfaced at the review gate already in the promotion path — never a silent
  clobber. The optimistic hash-guard (`CONFLICT_STALE`) is the single-agent
  backstop; the apply-time lock is the multi-agent one.
- **Contract gets cleaner.** The lock moves **out of `SandboxSpec`** — the
  sandbox is pure confinement; the promotion bridge owns "lock observed set →
  3-way merge → apply." Removing a concept from the contract (rather than adding
  one) confirms the layering.
- **Residual:** a long, divergent exploration raises apply-conflict odds, exactly
  like a long-lived branch. Optional, non-required mitigations: periodically
  rebase the sandbox; surface non-binding scope *hints*. The cost only bites the
  overlap case.

Validated by the sandbox-exec coordination spike: a freely-edited
disposable worktree promotes by 3-way merge — disjoint touched-sets apply clean,
genuine overlap is detected as a conflict (with markers), never silently merged.

## FMECA hardening — binding pre-implementation requirements (2026-06-11)

A due-diligence FMECA surfaced failure modes that must be *prevented by
construction*, not discovered in production. These are binding on the build:

1. **Confinement must not silently break existing scripts (FM: regression).**
   `ScriptExecutor` confines via a **per-script declared profile**; the default
   MUST preserve today's behavior (no blanket egress-deny / env-scrub applied to
   every `kind: script`, which would break governed workflows that legitimately
   use network/secrets, e.g. `deploy.production.rollout`). A "confined" gateway
   encountering a script with **no declared profile** fails fast, naming the
   script — never a silent deny.

2. **Resource limits are a REQUIRED provider capability (FM: host DoS).** The
   confinement spike confirmed bubblewrap does *not* cgroup — a fork-bomb /
   disk-fill inside the "sandbox" would exhaust the **host**. So a
   `SandboxProvider` MUST enforce cgroup-v2 / `systemd-run --scope` resource +
   wall-clock limits; a backend that cannot **fails preflight** for the agentic
   tier (no unlimited "sandbox" is ever offered as confined).

3. **`DisposableCopy` is a separate clone, never a shared `.git` (FM: confinement
   breach).** The coordination spike used a `git worktree` (shared object store)
   for convenience — **dev-only**. Production MUST copy/clone with no path back to
   the host `.git`; a test asserts the sandbox cannot reach the host object store.

4. **Edit basis must match the publish guard (FM: false `CONFLICT_STALE`).**
   `praxec.query { definitionId }` currently reads the merged
   `ConfigDefinitionStore`, while the publish hash-guard checks the writable
   `RepoDefinitionStore` (on disk). For a *writable* definition these can diverge
   (published-but-not-reloaded), rejecting a legitimate edit. The read-definition
   path MUST source a writable def's basis from the **same** store the guard
   checks (or fail fast with a "reload" remedy). Until then the hash-guard is the
   safe backstop — it fails *closed*, never silently overwrites.

5. **Lock the `SandboxProvider` trait only after a SECOND backend validates it
   (FM: contract churn). — SATISFIED.** `SandboxSpec` is proven against
   bubblewrap *and* an **OCI/container** backend (`OciProvider::run_args` maps the
   same spec to `docker/podman run` — `--network none`, `-v` mounts, `-e` env,
   `--pids-limit`, `-w` workdir, exit-code capture; unit-tested). The abstraction
   is not bwrap-shaped; the trait is frozen. (The OCI backend additionally
   enforces cgroup pids limits, closing part of FMECA §2 the bwrap backend can't.)

(Code-level fallbacks found in the shipped authoring code — silent
`unwrap_or_default()` on the definition hash + diff serializers, and a
presence-only repo-cache heuristic — were fixed to fail-fast in the same pass.)

## References

- Unsandboxed execution today: `crates/praxec-executors/src/script.rs`
  (bash in-process), `crates/praxec-agents/src/runner.rs` (agent subprocess).
- Promotion machinery (already built): provenance gate
  (`crates/praxec-executors/src/untrusted_execution.rs`); authoring
  workflows (`examples/authoring-workflow.yaml`,
  `examples/authoring-edit-workflow.yaml`); `registry`/`dry_run`/`diff` executors;
  `script_acknowledged` guard + hash-flip (`crates/praxec-core/src/guards.rs`).
- `kind: agent` = subprocess sub-agent (the exploratory-model seam).
- TRIZ: physical contradiction → separation principles (space / time / condition).
- Validating spike: the sandbox-exec mechanism proof (§Decision 5).
- Relates to: [ADR-0001](0001-headless-runtime-surfaces-attach.md) (governed
  surfaces — human and LLM are the same governed mechanism),
  [ADR-0005](0005-conversational-cockpit.md) (legible agency).
