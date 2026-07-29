# Primitive contracts report (A1-A6)

Method: each test below asserts a DESIRED behavior and the runner decided
red/green — none of these were derived by reading implementation and
hard-coding the observed answer. Harnesses were copied from existing
analogous test suites (never inferred from source).

## A1 — Guarded enum routing — GREEN

**Test names:** `guarded_arm_is_taken_when_its_guard_matches`,
`unguarded_default_arm_is_taken_on_a_different_discriminant`
**Location:** `crates/praxec-core/tests/primitive_contracts.rs`

Harness: the self-contained `NoopExec`/`NoopReg` + `WorkflowRuntime` harness
copied from `tests/outcome_evaluation.rs`. Two deterministic candidates in one
state — `to_ts` guarded on `$.context.mode == 'ts'`, `to_default` unguarded —
exercise the real engine selection logic
(`WorkflowRuntime::select_deterministic_transition`, runtime_chain.rs).
`mode` is seeded into context from `input` at `start()` (the input→context
auto-seed rule in runtime.rs), so passing `input: {"mode": "ts"}` vs
`{"mode": "rust"}` deterministically proves the guarded arm is taken on match
and the single unguarded default is taken otherwise.

## A2 — `use.inputs` seeds the child — GREEN

**Test name:** `use_inputs_seeds_the_child_workflow_input`
**Location:** `crates/praxec-executors/tests/primitive_contract_a2_use_inputs.rs`

Hosted in `praxec-executors` (not `praxec-core`) — same reason
`scoped_capability_io_roundtrip.rs` lives there: exercising real `kind:
workflow` dispatch needs `WorkflowExecutor`, and `praxec-core` cannot depend
on `praxec-executors` (cycle). Harness (the `CapTestRegistry` /
`WorkflowRuntime` wiring) copied verbatim from
`scoped_capability_io_roundtrip.rs`.

There is no public seam that hands a child's raw `$.workflow.input` back to
the parent's caller, so the child's own deterministic transition reads
`$.workflow.input.k` and writes it to its own output (`echoed:
"$.workflow.input.k"`), which the host's `use.outputs` then projects to
`$.context.echoed_value`. The host seeds `use.inputs.k` from
`$.context.provided_value = "seeded-by-use-inputs"`; the round-tripped value
came back identical, proving the child actually saw
`$.workflow.input.k == "seeded-by-use-inputs"`.

## A3 — Goal renders from input at invocation — GREEN

**Test name:** `goal_renders_from_workflow_input_at_invocation`
**Location:** `crates/praxec-agents/src/executor.rs` (`executor::tests` module,
next to the existing `goal_is_templated_against_the_blackboard`)

Harness: `AgentExecutor` + `MockSessionRunner` + `MockModelResolver`, the
exact fixture pattern the file already used for goal-templating tests. An
agent step with `goal: "{{ $.workflow.input.instructions }}"` is dispatched
with `req.workflow.input = { "instructions": "refactor the parser module" }`;
`runner.sessions()[0].user_prompt` (the captured USER prompt actually sent to
the model) equals `"refactor the parser module"`.

## A4 — Unknown definitionId fails at LOAD — RED

**Test name:** `a_kind_workflow_transition_referencing_an_unknown_definition_id_fails_at_load`
**Location:** `crates/praxec-core/tests/primitive_contracts.rs`
**Ignore reason (verbatim):** `RED: V22 (validate_workflow_refs_resolve) only
runs on the repos-present branch of merge_declared_repos (config.rs); a
host-only config with no `repos:` block returns Ok(host) before V22 is ever
reached, so an unknown `kind: workflow` definitionId passes `praxec check`
clean and is only discovered at runtime dispatch. Fix: call
validate_workflow_refs_resolve unconditionally (also on the
repos.is_empty() early-return path).`

Harness: `praxec_core::config::load_resolved_with_repos` against a tempdir
host file, copied from `tests/multi_repo_loading.rs` (the same loader
`praxec check` calls). **Verified empirically, not assumed:** a config with
NO `repos:` block and a `kind: workflow` transition referencing
`definitionId: cap.does.not.exist` loads successfully (`Ok`) — the V22 check
(`config::validate_workflow_refs_resolve`, the "UNRESOLVED_WORKFLOW_REF"
error) is wired only inside the repos-present branch of
`merge_declared_repos`; the `repos.is_empty()` early-return path skips it
entirely, and `validate_workflows` (the separate diagnostics pass) has no
definitionId-existence check of its own (`validate_use_bindings` and
`validate_contract_hash_pins` both explicitly no-op on an unresolved target,
deferring to "V22's job"). **Engine change that would make it green:** call
`validate_workflow_refs_resolve` unconditionally in `merge_declared_repos`
(including on the no-`repos:` early-return path), not only after a
repo-merge.

## A5 — Unresolvable skills entry fails loud — GREEN (contradicts the prior-spike assumption)

**Test name:** `unresolvable_dollar_path_skills_entry_fails_loud_at_load`
**Location:** `crates/praxec-core/tests/primitive_contracts.rs`

Harness: `resolve_str` + `validate::validate_workflows`, copied from
`tests/use_binding.rs` / `tests/validation_rules.rs`. The task briefing
flagged this as "very likely RED" per a prior spike claiming a silent drop.
**Empirically it is not** for the literal fixture given
(`skills: ["$.workflow.input.missing"]`, a JSON *string*): `check_skills_refs`
(validate.rs) checks every `skills:` entry against the top-level `skills:`
library by literal string match — there is no `$.`-path templating step for
this field — so the unresolvable-looking path fails that membership check
like any other undeclared subject and produces a genuinely loud, load-time
error naming it verbatim:

```
error: workflow 'flow.host': state 's' references skills entry
'$.workflow.input.missing' which is not declared in the top-level `skills:`
library (SPEC §11)
```

This is both `praxec check`- and `serve`-startup-enforced (CMP-002), so it is
a real load-time gate, not deferred to runtime. The still-open, genuinely
silent gap (not asserted here, out of A5's literal scope) is a **non-string**
`skills:` array entry — both `check_skills_refs` (validate.rs) and
`push_scope_subjects` (runtime_links.rs, the runtime dispatch path) use
`entry.as_str() else { continue }`, so a non-string entry (e.g. an object)
is silently skipped with zero diagnostic and zero runtime error.

## A6 — DoD check is executor-agnostic — GREEN

**Test names:** `dod_check_is_unmet_before_any_evidence_is_recorded`,
`dod_check_flips_met_once_a_plain_deterministic_step_writes_the_evidence_shape`,
`dod_check_reads_not_met_when_the_recorded_status_is_fail`
**Location:** `crates/praxec-core/tests/primitive_contracts.rs`

Harness: same `NoopExec`/`NoopReg` + `WorkflowRuntime` harness as A1, copied
from `tests/outcome_evaluation.rs`. `$.context.ws_verify` is set by a plain
`kind: noop` deterministic transition's `output:` map (not any
verify-specific capability); the outcome `check:
"$.context.ws_verify.status == 'pass'"` flips `met: true` purely off that
shape, and reads `met: false` both before any evidence and when the status
is `"fail"`.

---

## Tally

A1, A2, A3, A5, A6 green; A4 red (1 test ignored, gap documented above and in
the `#[ignore]` reason).
