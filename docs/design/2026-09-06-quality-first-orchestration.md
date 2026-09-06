# Commodity-model productivity amplification

The objective is more independently accepted work per dollar, with quality as a
constraint. Codex owns decomposition, context preparation, acceptance, and
integration. Praxec executes bounded worker steps using existing model bindings.
Commodity workers are the starting point for suitable work; stronger models are
an escalation when better context or deterministic checks do not resolve a gap.

## Implemented

- Added a reusable [orchestration skill](../skills/praxec-orchestrate/SKILL.md)
  and repository instructions in `AGENTS.md`. It discovers relevant workflows,
  drives legal command/query links directly, and improves task-local workflow
  variants from observed results.
- Added a preparation helper for tool-free review, analysis, and proposed patches.
  Each worker receives task, source context, and acceptance criteria. SQLite and
  audit artifacts are isolated per run. The caller must separately accept or
  reject the candidate and provide verification evidence. This is a controlled
  entry point, not an autonomous repository-editing agent or automatic optimizer.
- Fixed the agent executor's model-to-effort lookup: duplicate occurrences of a
  model at different reasoning efforts now preserve their own effort. Previously
  the final occurrence overwrote every earlier effort. The new regression covers
  low effort, inherited effort, medium effort, and the breaker's single all-open
  probe. This enables economical effort ladders without silently raising the
  first attempt's reasoning setting.

## Evidence and limits

On 2026-09-06, the installed Praxec 0.0.51 delegated a review of the executor
patch and relevant source to the operator's `commoditized` binding. The served
model was `openrouter:z-ai/glm-4.7-flash`, effort `low`. Its attempt event recorded
5,539 prompt tokens, 7,607 completion tokens, 55,127 ms, and estimated model cost
of $0.00337514. It reported no introduced bug and disclosed that it had not run
tests. Codex checked the code and regression tests before accepting the result.
The review's hypothetical identical-model/different-breaker-entry concern was
discarded because the registry is keyed by model id.

This is one feasibility smoke, not a demonstrated speedup or quality comparison.
Cost is Praxec's catalog estimate, not an invoice; coordinator time/tokens are not
included. An initial sandbox DNS failure was inspected as a cancelled workflow
before a fresh, network-authorized run. Its failed-attempt spend is unknown.
Local prompts, responses, telemetry, and acceptance notes are under ignored
`.praxec-runs/routing-review/`. The live smoke used the first gate version;
required acceptance evidence was added afterward and verified offline.

Validation: 48 agent executor tests pass, including the new regression. Four
Python tests cover preparation, no-overwrite behavior, runtime startup, and
acceptance evidence rejection/persistence. Praxec schema validation passes.
Mock graph coverage reaches all three transitions. The generic fuzz integration
smoke reports violations at the evidence gate because its driver does not supply
the required arguments; explicit runtime scenarios exercise the gate successfully.
The skill frontmatter and Rust formatting checks pass. The installed Praxec
binary was used for the smoke; the Rust change is tested in source, not installed.

## Review findings affecting the amplifier

Update: the [harness hardening](2026-09-06-commodity-harness-controls.md) implements
source fixes for findings 2 and 3 below. These findings describe the installed
0.0.51 baseline, not the behavior of the patched source.

1. **Effort identity was lossy.** Fixed in this patch. A model alone is not the
   identity of an execution configuration; effort must remain paired per hop.
2. **Worker completion is not independent acceptance.** In
   `praxec-core/src/deescalation.rs`, the aggregation treats `agent.completed` as
   a pass. A subsequent rejection by the caller is not sufficient evidence in
   that aggregation. Join final acceptance and its checks before adopting a
   cheaper route from a cost proposal.
3. **Direct-agent accounting is incomplete in the report path.** The smoke
   emitted `agent.model_attempt` with usage and price, while `cost report` returned
   zero runs. The skill requires attempt-level audit inspection when this occurs.
   Consolidating these telemetry paths is a useful next engine change.
4. **CLI success codes do not guarantee workflow success.** Both executor-error
   responses and fuzz violations can arrive with exit zero. The driver must check
   JSON status/error fields and query after interruptions before retrying.
5. **Process startup has a cost.** Separate CLI calls reconstruct executor state;
   the breaker registry is process-local. For sustained orchestration, a persistent
   Praxec MCP server should retain connection/breaker state and avoid repeated
   gateway loading. CLI is a working fallback in this session, which exposes no
   Praxec MCP tools.

## Upstream workflow fit

Reviewed the organization inventory, pack registry, and selected relevant
definitions on their `dev` branches, not every workflow's execution behavior.

| Source | Reuse and qualification |
| --- | --- |
| [Pack registry](https://github.com/praxec/packs/blob/dev/packs.yaml) | Discover SWE, meta-authoring, design, UX, and FrontRails packs; load only the task's dependencies. |
| [Safe refactor](https://github.com/praxec/cognitive-architectures/blob/dev/orchestrators/flow.safe-refactor.yaml) | Reuse baseline-before-edit and compare/review gates; its final PR step makes the full lifecycle broader than a local edit. |
| [Parallel critics](https://github.com/praxec/cognitive-architectures/blob/dev/patterns/parallel-critic-review/pattern.yaml) | Bounded aspect-specific review; requires an operator-supplied `critique_aspect` workflow. Aggregating successful executions alone does not establish finding validity. |
| [Optimize flow](https://github.com/praxec/praxec-meta/blob/dev/orchestrators/flow.optimize-flow.yaml) | Audit mining, alternative shapes, emit/write/check/review is a useful improvement loop. The full flow includes human selection and PR publication; use task-local variants or bounded constituent capabilities as appropriate. |
| [Design bindings](https://github.com/praxec/design/blob/dev/praxec.repo.yaml) | Already distinguishes frontier creation from commodity rollout; recommendations require operator binding and live validation. |
| [UX pack](https://github.com/praxec/cognitive-architectures-max/blob/dev/praxec.repo.yaml) | Layers on the SWE pack and external tools; use when the task actually needs its UX research flow. |

The local GitHub Actions pipeline remains deterministic. Adding model calls to
format/build/test checks would not improve this objective. Its separate doctest
invocation after `cargo test --workspace` is redundant under ordinary Cargo
behavior; that CI cleanup was left outside this orchestration change.

## How to retain an optimization

Keep a task-class baseline and change one meaningful variable: context packet,
step shape, model/effort, or concurrency. Measure independent acceptance, material
defects, rework, elapsed time, and all observed cost. Preserve unknown costs as
unknown. Retain a cheaper or faster variant only when it meets the same quality
bar on representative tasks; otherwise restore the baseline or escalate the
particular step. One successful cheap review is a reason to evaluate further,
not to downgrade every coding or review task.
