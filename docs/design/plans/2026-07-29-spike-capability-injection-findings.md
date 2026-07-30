# SPIKE: `flow.change` build_cap/verify_cap/build_skills injection — engine viability findings

**Status:** DONE. Read-only investigation of engine source + a model-free runtime probe in a scratch pack (`/tmp/.../scratchpad/spike-pack/`). No engine source or real pack modified. Nothing committed.

**VERDICT:** The design AS ILLUSTRATED is **NOT VIABLE**. `definitionId` on a `kind: workflow` executor is a literal registry-lookup key, never templated (Q1 = NO, proven both from source and by a live run that fails with `workflow definition '$.workflow.input.target_cap' not found`). `scope.skills` (in reality a bare `skills:` array, not a `scope:`-wrapped field) is a static `Vec<String>` with no templating path; a `$.` string there is silently inert, not even an error (Q2 = NO). Parent-state `skills:` do not propagate into a `kind: workflow` child's own states — the child resolves skills purely from its own local definition (Q3 = NO). **However, a working variant exists and is PREFERRED**: pass instruction text (not a definitionId) through `use.inputs` into the child capability's blackboard, then have the child's `kind: agent` step template it into its `goal` (Q4 = VARIANT — viable today, zero engine changes, sidesteps Q3 entirely). Dynamic *capability selection* (`build_cap`/`verify_cap`) still has no live path other than a static `build_mode` enum routing to pre-declared alternative transitions.

---

## Q1 — Is `kind: workflow`'s `definitionId` templated before registry lookup?

**Answer: NO.**

- `crates/praxec-executors/src/workflow.rs:143-150` — the executor reads `definitionId` directly off the raw config value with no templating call at all:
  ```rust
  let definition_id = request
      .executor_config
      .get("definitionId")
      .and_then(Value::as_str)
      .ok_or_else(...)?
      .to_string();
  ```
  Contrast with the same function's handling of `use.inputs` (line 245-254, routed through `resolve_use_inputs`), the legacy `input:` block (line 256-269, routed through the local `resolve_input` which explicitly handles `$.`-prefixed strings), and `repoRoot` (line 282-312, routed through `praxec_core::mapping::read_in_scopes`). `definitionId` gets none of that treatment — it is the one field in this executor deliberately NOT resolved against any scope.
- That literal string is threaded straight into `StartWorkflow { definition_id: definition_id.clone(), ... }` (workflow.rs:333-358) and handed to `runtime.start(...)`.
- The registry lookup keyed by that literal string lives in `crates/praxec-core/src/store/store.rs:251` and `:306`: `.ok_or_else(|| anyhow!("workflow definition '{}' not found", definition_id))`.
- Static validation exists for `definitionId` (V22, `crates/praxec-core/src/config.rs:4297-4324`, `collect_unresolved_workflow_refs` at :4326-4339) that requires every `kind: workflow` reference to resolve to an already-loaded workflow id — but **this check only runs on the `repos:` cross-repo merge path** (called once, at `config.rs:3431`, inside the repo-merge function). A plain single-file config with a top-level `workflows:` block (no `repos:`) never calls it — confirmed empirically: `praxec check` accepted the spike config with `definitionId: "$.workflow.input.target_cap"` with `validation: ok`.

**Live proof (model-free):**
- Spike pack: `spike.wrap` (a flow) has one deterministic transition `build` with `executor: { kind: workflow, definitionId: "$.workflow.input.target_cap", use: {...} }`. `spike.target` is a trivial one-hop `actor: deterministic, kind: noop` capability.
- `praxec check --config spike.yaml` → `validation: ok` (as predicted above — no load-time catch outside the repos path).
- `praxec command start` with `{"definitionId":"spike.wrap","input":{"target_cap":"spike.target", ...}}` → the mission runs to **`failed`**:
  ```
  "error": {
    "attemptedTransition": "build",
    "code": "CHAIN_FAILED",
    "message": "permanent error: failed to start sub-workflow: workflow definition '$.workflow.input.target_cap' not found"
  },
  "result": { "reason": "error", "status": "failed" }
  ```
  The audit trail shows `cap.invoked` firing with `"definitionId":"$.workflow.input.target_cap"` (the raw unresolved string), then `executor.failed`/`cap.terminated` with the not-found error.
- **Control** (same pack, `definitionId: "spike.target"` literal instead of the `$.` path): the identical `use:`/output-projection plumbing runs to `succeeded`, `child_status: "pass"`, outcome `child_ran: met: true`. This isolates the failure to templating alone — the `kind: workflow` + `use:` mechanism itself works fine when the id is literal.

## Q2 — Can `scope.skills` be a templated `$.` value instead of a static array?

**Answer: NO.**

- First mismatch from the illustrative design: there is no `scope:` wrapper in the real schema. `skills:` is a bare key directly on the workflow, state, or transition object (`crates/praxec-core/src/config.rs:2929-2956`, `crates/praxec-core/src/validate.rs:2380-2397`, `crates/praxec-core/src/runtime/runtime_links.rs:239-268`).
- Every read site pulls it via `.and_then(Value::as_array)` / `.as_array()` (e.g. `runtime_links.rs:258`, `config.rs:2949`, `validate.rs:2350`) and then `entry.as_str()` per element (`runtime_links.rs:262`, `config.rs:2951`). There is no code path anywhere that hands a `skills:` value to `mapping::read_in_scopes`, `mapping::resolve_value`, or `render_template` — the three templating entry points used elsewhere in the config (e.g. `prefill:`, `use.inputs`, `goal`).
- Consequence: a value like `skills: "$.workflow.input.build_skills"` is a JSON **string**, not an array. `Value::as_array()` on a string returns `None`, so `collect_skills_strings`/`push_scope_subjects`/`check_scope` all silently treat it as "no skills declared" — **not an error**, at load time or at runtime. This is a poka-yoke gap worth flagging on its own (a typo'd or well-intentioned-but-wrong `skills:` shape fails silently rather than loud), independent of the injection design.
- **Live proof:** the spike's `building_injected` state declared `skills: "$.workflow.input.injected_skills"` with a real array (`["sk.fake.one"]`) passed as the flow input. The response's `guidance` block contained only `{"goal": "..."}` — no `refs` key at all — confirming zero skills were resolved, with no diagnostic anywhere in the run.

## Q3 — Do a parent state's `skills:` propagate into a `kind: workflow` child's own states?

**Answer: NO.**

- Skills-in-scope for an agent step are computed by `collect_in_scope_skill_subjects(definition, state, transition)` (`crates/praxec-core/src/runtime/runtime_links.rs:232-247`), called from `assemble_system_message` (`crates/praxec-core/src/skills.rs:29-34`), called from the agent executor at `crates/praxec-agents/src/executor.rs:705-721`.
- All three arguments — `definition`, `state`, `transition` — are **always the currently-executing workflow instance's own values** (`request.workflow.definition` / `request.workflow.state` / `request.transition` at executor.rs:706-708). When a `kind: workflow` transition spawns a child (`workflow.rs:333-358`), the child runs as an entirely separate `WorkflowInstance` with its own `definition` (the CHILD's compiled config) — there is no parent-context parameter threaded into the child's skill resolution at all. The parent's `skills:` only ever govern an agent step that fires **inside the parent's own states**; they have zero reach into a spawned sub-mission's states.
- No composition/merge step exists anywhere that folds a parent's skills into a child's `_skillsLibrary` snapshot or its resolved subject list.

## Q4 (added mid-spike, preferred mechanism) — Skills as an explicit input threaded into the agent's prompt

**Answer: VARIANT — viable today, with a caveat about system- vs. user-message placement.**

- The agent executor's system message is built exclusively by `compose_system_message(skills: &Option<String>, requires_file_write: bool)` (`crates/praxec-agents/src/rig_runner.rs:185-195`), fed by `session.system_prompt` (`crates/praxec-agents/src/session.rs:120`), which is set **exactly once**, from `assemble_system_message(&request.workflow.definition, &request.workflow.state, request.transition...)` at `crates/praxec-agents/src/executor.rs:705-721`. This call is 100% blind to runtime data — `request.workflow.input`/`request.workflow.context`/`request.arguments` never enter it. So: **NO**, there is no existing path for a caller-supplied value to reach the actual system message. `AgentExecutorConfig` (`crates/praxec-agents/src/config.rs:16-89`) is `#[serde(deny_unknown_fields)]` and has no `skills`/generic-input field — adding one would need an engine change and `deny_unknown_fields` would reject it today.
- BUT the agent's **user prompt** (`goal:`) is templated per-invocation against the live instance: `let user_prompt = render_template(&cfg.goal, &request.workflow);` (`executor.rs:724`). `render_template` (`crates/praxec-core/src/templating.rs:29-137`) resolves `{{ $.context.* }}` and `{{ $.workflow.input.* }}` against `instance.context`/`instance.input` — and for a capability invoked via `use:`, that instance IS the child, and its `input` is exactly what `use.inputs` seeded from the parent's `resolve_use_inputs` call (`workflow.rs:245-254`).
- This means a caller CAN, with zero engine changes, do:
  ```yaml
  # parent (flow.change)
  executor:
    kind: workflow
    definitionId: cap.build.something
    use:
      inputs:
        extra_instructions: "$.workflow.input.build_skills"   # array or text
  ```
  and inside `cap.build.something`'s own `kind: agent` step:
  ```yaml
  goal: >
    Build the deliverable. Additional instructions: {{ $.workflow.input.extra_instructions }}
  ```
  `resolve_template_path` (`templating.rs:108-136`) calls `Value::to_string()` on a non-string match, so an array renders as raw JSON (e.g. `["do X","do Y"]`) — readable by a model but not the same clean prose a real skill body gives; for best results the parent should pass already-composed instruction TEXT, not skill IDs (there is no dynamic id→body library lookup on this path — that lookup only happens inside the static `assemble_system_message`/`_skillsLibrary` mechanism).
- Net effect: this **lands in the user turn, not the system turn**, and works only for literal/already-resolved text data, not for "resolve these skill ids against the library at runtime." But it is a real, live, already-wired mechanism — it does not depend on any cross-workflow scope propagation (sidesteps Q3 entirely), and is functionally adequate for injecting per-run guidance into a builder agent, since frontier models don't meaningfully distinguish system vs. user instructions for compliance.

---

## What was run (for reproducibility)

Scratch pack: `/tmp/claude-1000/-home-mc-working-mcp-flowgate/67fab687-1618-460f-aa4a-63e03e82718e/scratchpad/spike-pack/spike.yaml` (+ `spike-control.yaml`, the literal-`definitionId` control). Built binary: pre-existing `./target/debug/praxec` (no rebuild needed).

```
./target/debug/praxec check --config spike.yaml
# → "validation: ok" (no load-time rejection of the templated definitionId — V22 only runs on the repos: merge path)

./target/debug/praxec command --config spike.yaml \
  '{"definitionId":"spike.wrap","input":{"target_cap":"spike.target","injected_skills":["sk.fake.one"]}}'
# → mission status "failed", CHAIN_FAILED:
#   "failed to start sub-workflow: workflow definition '$.workflow.input.target_cap' not found"
#   guidance.refs absent (skills: templated value silently produced zero skills)

./target/debug/praxec command --config spike-control.yaml \
  '{"definitionId":"spike.wrap","input":{"target_cap":"spike.target","injected_skills":["sk.fake.one"]}}'
# → mission status "succeeded", child_status: "pass", outcome child_ran met: true
#   (identical use:/output-projection plumbing, literal definitionId — isolates the failure to templating)
```

## Recommendation for the `flow.change` generalization

1. **`build_cap`/`verify_cap` as designed (dynamic `definitionId`) — drop it.** Use a static enum instead: declare `build_mode: standard | <alt1> | <alt2>` as a flow input, and pre-author one alternative `kind: workflow` transition per mode (a small, closed set of literal `definitionId`s), gated by a guard on `build_mode`. This is mechanically identical to existing guard-gated-transition patterns already proven in the engine and needs no engine change.
2. **`build_skills` as designed (dynamic `scope.skills`) — drop it.** Use Q4's variant instead: thread the instruction content through `use.inputs` into the child capability, and reference it from the child's `goal:` via `{{ $.workflow.input.<key> }}`. Prefer passing composed instruction text (or a small set of pre-rendered skill bodies assembled by a deterministic step beforehand) over raw skill ids, since there is no runtime id→body resolution on this path.
3. If a future engine change is ever wanted for the DESIGNED (fully dynamic) shape, the two touch points are precise: `crates/praxec-executors/src/workflow.rs:143-150` (route `definitionId` through `mapping::read_in_scopes` the way `repoRoot` already does, guarding the registry-lookup-key trust boundary), and `crates/praxec-core/src/runtime/runtime_links.rs:232-247`/`crates/praxec-core/src/skills.rs:29-34` (thread instance input/context into `collect_in_scope_skill_subjects` for a true dynamic-skills-in-system-message feature). Both are non-trivial trust/validation changes (static analysis like V22 and the skills-library existence check currently assume `definitionId`/`skills` are known at config-load time) — not recommended as a quick patch.
