---
name: praxec-orchestrate
description: Use Praxec to delegate bounded work to other models and improve task workflows from acceptance evidence. Use for substantial coding, investigation, or review tasks that benefit from model orchestration, or explicit Praxec workflow optimization.
---

# Praxec orchestration

Optimize in this order: accepted quality, completion time, total cost. Keep Codex
responsible for the task contract, integration, and independent verification.
Use another model when a bounded deliverable can advance useful work; handle
small tasks directly when delegation overhead exceeds the benefit.

Delegate coarse, well-defined deliverables, not individual commands or edits.
Humans supply intent; models generate solutions; tools execute mechanical work
and verify artifacts. Keep a worker responsible for a coherent result through
its internal tool operations. Do not split formatting, extraction, test running,
or a known next transition into separate paid model tasks.

## Discover only what this task needs

Use exposed `praxec.query` / `praxec.command` tools, or the equivalent installed
CLI. Check `praxec --version` and subcommand `--help` when needed. Discover the
operator's gateway and models config; do not print provider credentials or
replace existing bindings. Run `praxec check --config <gateway>` before use.

Search the mounted catalog with `praxec query --config <gateway>
'{"query":"<task intent>"}'`. Follow returned links to inspect the exact inputs,
outputs, prerequisites, and side effects of a candidate definition. The GitHub
pack registry is `https://github.com/praxec/packs/blob/dev/packs.yaml`; inspect
only relevant pack definitions. A registry description is not proof that a flow
loads or its tools are available. In particular, SWE lifecycle flows can include
commit, push, or PR steps. Choose a bounded capability or local variant when
those actions are outside the task's authorization.

## Delegate with a concrete contract

Define the deliverable, relevant source context, acceptance checks, owned paths
if editing, and an attempt/time bound. Choose an existing affinity/activity
binding based on the task: start with commodity workers for well-scoped work,
giving them sufficient source context and explicit acceptance checks;
ambiguous architecture or consequential review needs stronger reasoning. Inspect
the resolved chain: affinity labels do not guarantee distinct models, quality,
price, or availability. Preserve per-binding reasoning effort and cost controls.

Prefer deterministic tools for formatting, compilation, tests, and data
extraction. Parallelize independent reading/review or disjoint edits only when
there is useful work to overlap. Start with a small cohort; size it to actual
dependencies and provider capacity. Give reviewers the original task and evidence,
not merely the producer's explanation. Do not multiply full-context agents.

Use `command`/`query` to drive known legal steps directly; a separate paid
transition-chooser model is unnecessary when Codex knows the next move. Read
fresh workflow versions and returned links before each submission. CLI exit zero
does not prove success: inspect JSON `error` and `result.status` as well. Do not blindly
retry a timed-out mutating CLI call: query its workflow first. Record the workflow
id and resume it rather than launching duplicate work. `orchestrate` defaults to
`auto-approve`; use stepwise driving when gates need independent decisions.

For a self-contained analysis, review, or proposed patch with no worker tool
access, use [the bounded delegation recipe](references/bounded-delegation.md).
It uses the existing model config and persists a candidate awaiting Codex's
acceptance. For real file edits, use a suitable pack capability and scoped tools;
the recipe's tool-free worker cannot inspect a checkout or execute tests.

## Accept work and improve the workflow

Inspect the actual artifact. Run the relevant independent checks and resolve
review findings before accepting. A worker's `success` or a second model's vote
is not evidence that tests passed. If information is missing, supply the missing
context; if capability is inadequate, use a stronger suitable binding. Bound
rework and stop repeating attempts that add no evidence.

Record task class, workflow/config revision, actual model and effort, acceptance
checks/results, rework, elapsed time, and token/cost observations in a local run
record. `praxec cost report --config <gateway> --json` and `praxec cost propose
--config <gateway> --json` provide evidence; proposals do not apply themselves.
Direct `kind: agent` calls may emit `agent.model_attempt` without a corresponding
`agent.completed`; a zero-run cost report is not proof of zero spend. Inspect the
file audit for that workflow's attempt events and preserve missing prices as unknown.
Keep unpriced attempts as unknown, and include failed attempts and coordinator
work when comparing total cost. Distinguish worker completion from accepted work.
In the installed v0.0.51, the cost-proposal aggregation counts `agent.completed` as a pass;
it does not establish that a later caller acceptance gate passed. Join the final
accept/reject decision and its verification evidence into your evaluation record
before using these proposals to justify a quality-preserving model downgrade.

Optimize task-local workflow copies as you go when observed failures justify it:
improve context handoffs, replace mechanical model calls with scripts, overlap
independent steps, narrow tools, or adjust a specific model/effort binding. Run
`praxec check`, mock graph checks (`praxec fuzz`), and relevant acceptance scenarios
after structural changes. Mock validation does not establish live model quality.
The v0.0.51 generic fuzz smoke can stall at required transition arguments (such
as acceptance evidence). Inspect its JSON violation flags even when exit is zero;
exercise those gates with explicit runtime scenarios instead of weakening them.
Compare the baseline and candidate on representative tasks with the same quality
bar before retaining a cheaper/faster route. Preserve the prior version and
evidence so regressions can be reversed. Keep shared pack publication, global
model-policy changes, and external writes within the user's authorized scope.

## Current source controls versus installed releases

The harness hardening source changes count direct successful attempts and exclude
unverified completion from optimizer quality statistics. Its explicit
`model_acceptance` path requires an executor-free human transition, a trusted
human principal, the latest `producer_event_id`, and nonblank evidence. This
recipe's caller decision is not that trusted-human optimization attestation; do
not impersonate a human to populate optimizer passes. A release older than these
changes still needs the manual audit precautions above.

Ordinary agent dispatch defaults to enforced grounding in the patched source.
The recipe sets it explicitly for older supported binaries. For auto-driven
retrying states, declare `continuation.reads` over meaningful evidence (such as
artifact digest, verifier diagnostics, and selected model tier). Counters and
timestamps alone are not progress. The guard detects exact repeated inputs;
it cannot prove semantic progress. Use current source excerpts alongside design
documents in reviewer handoffs; historical designs can misdescribe today's code.

For source builds supporting `amplifier`, use the
[coarse-task profile](../../design/2026-09-06-coarse-task-amplifier.md) when its
supported execution shapes fit. It enforces classified work and tool verification
on every success path and avoids the extra auto-drive argument-generation call.
It does not prove task coarseness or semantic quality, and older installed
binaries must not be assumed to enforce it. Keep independent quality measurement
separate from routine tool verification; do not add human approval to every step.
