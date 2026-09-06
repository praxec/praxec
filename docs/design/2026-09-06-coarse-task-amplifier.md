# Coarse-task amplifier: intent, generation, deterministic execution

Status: implemented as an opt-in profile in source; not a claim about installed
v0.0.51 binaries or live model quality. Supersedes any interpretation of the July
change-atom designs that assigns a model to every mechanical operation.

The unit of delegation is a coherent deliverable with sufficient context and a
clear acceptance contract: implement a feature, diagnose a failure, or produce a
complete proposal. Humans own intent and consequential changes to that intent.
Models generate solutions and exercise judgment. MCPs, scripts, and the runtime
perform mechanical execution, routing, and verification. A task may include
several tool operations without becoming several model tasks.

## Enforced profile

Declare `amplifier: {max_model_steps: 1}` on a workflow. The limit (1–16) counts
generative stages along each possible graph path, not provider fallback attempts
within a stage. Required workflow input strings are `intent`, `context`,
`deliverable`, and `acceptance`. Missing, null, and blank inputs fail before model
dispatch even when an embedding bypasses normal config loading.

Every initial transition must be an effect-free deterministic noop guarded by
`amplifier_contract`. The new guard validates the profile and input. Older standard
runtimes reject this unknown guard at load/dispatch rather than silently ignoring
new metadata and spending on an unprotected model call. Candidate and verification
are declared typed object blackboard slots.

Each transition declares `work_kind`:

| Role | Enforced execution contract |
| --- | --- |
| `generative` | `actor: agent`, direct `kind: agent`, grounded goal containing all four immutable workflow input bindings, explicit attempt/chain time bounds, complete output mapped to `candidate`. |
| `deterministic` | `actor: deterministic`, direct script, MCP, or noop executor. No paid argument chooser. |
| `verification` | Auto-driven script with an operator-defined subject, explicit nonzero-exit failure, arguments bound to the current candidate and original acceptance contract, and complete script output retained as `verification`. |
| `intent` | Optional executor-free human decision without context writes. No mandatory approval at each task stage. |

Auto-drive selects legal generative transitions using the existing deterministic
guard selector and runs the declared worker directly. It does not synthesize an
extra model invocation to prepare arguments. Existing workflows outside this
profile retain their behavior.

All success paths must generate a candidate and subsequently verify it. Every
later generative or mechanical transition invalidates the prior verification;
another verifier must run before success. The analysis includes guarded branches,
including branches whose guards currently appear false. Timeout handlers may
only enter an explicit failure terminal without execution. Terminal outcomes
must be explicit.

The first profile deliberately rejects opaque composite executors, fallback
executors, hidden `onEnter`/`delegate` work, state-local slots, `while` and graph
cycles, and the separate `untrusted` agent command mode. Verification cannot
overwrite the candidate or perform additional output mappings. These restrictions
close concrete bypasses of generation bounds and evidence preservation; they are
not instructions to fragment a coarse task. The durable halt command remains
available.

## Boundary of the guarantee

Configuration and executor implementations are trusted. A script that exits zero
without meaningful checks is still a bad verifier. Static role labels cannot
recognize semantic task coarseness, determine whether an MCP server internally
uses a model, or establish that prose acceptance criteria are met. Test adequacy,
artifact identity and checkout scope belong in the operator's verification
implementation. For repository work, check the actual artifact and revision,
not merely a model summary. Preserve scoped tool permissions and owned paths.

`max_model_steps` bounds successful graph traversal; attempt and chain seconds
bound each generative invocation. Explicit re-submission after failure, recovery
after crashes, and separate new runs require a caller-level retry/spend policy.
This profile is not an exactly-once or dollar-budget guarantee. Do not repeat a
failed attempt without addressing the missing context, capability, or evidence.

Tool-verified workflow completion does not automatically become positive semantic
quality evidence in the cost optimizer. Its existing independent acceptance
provenance remains separate. Humans need not approve every routine task merely
to execute it. When comparing routes, record acceptance appropriate to the task,
all attempts, review and rework, elapsed time, actual models/effort, and unknown
costs. Do not treat missing usage as zero.

## Verification and next extension

`tests/amplifier_contract.rs` exercises graph bypasses, hidden loops, stale or
fabricated verification, input rejection before spend, model-stage bounds, legacy
compatibility, one-call auto-drive, and failure stopping completion. The example
at `examples/coarse-task-amplifier` illustrates structural verification only.

The bounded commodity design-review run on 2026-09-06 exhausted its 300-second
chain budget across two models without producing a candidate. It supplies no
positive quality evidence. A subsequent independent source review identified the
double-invocation path and several static-analysis bypasses; regression tests
cover those corrections. Neither exercise proves cost savings.

Next, qualify real coarse-task verification adapters (including FrontRails MCP
results) against artifact revisions and a shared acceptance rubric. Permit a
broader executor shape only after its bypass tests exist. Compare orchestrated
and direct strong-model runs on the same tasks before retaining routing changes.
