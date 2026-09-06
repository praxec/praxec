# Tool-free worker with caller acceptance

Use `scripts/prepare.py` relative to this skill to prepare a fresh run directory:

```bash
python3 <skill>/scripts/prepare.py <run-directory> \
  --models <existing-models.yaml> --affinity <existing-binding>
praxec check --config <run-directory>/gateway.yaml
```

Choose a persistent writable artifact directory, ignored by git when inside a
checkout. No provider keys are copied. The script writes a YAML-compatible JSON
config with SQLite state and file audit, and refuses to overwrite an existing
config. Its worker has no tools. It returns a proposal; Codex applies and tests
any proposed edits. The 180-second attempt and 300-second chain budgets can be
overridden at preparation time; they are time bounds, not dollar ceilings.

Start with a JSON object containing `definitionId: "delegated_task"` and `input`
with three nonempty strings: `task`, `context`, `acceptance`. Use structured MCP
arguments or a subprocess argument list, not shell interpolation of task text:

```python
payload = {"definitionId": "delegated_task", "input": {
    "task": "Review the supplied patch for correctness regressions.",
    "context": patch_and_relevant_source,
    "acceptance": "Return actionable findings with evidence, or no findings; state limits."
}}
result = subprocess.run(
    ["praxec", "command", "--config", str(config), json.dumps(payload)],
    check=True, capture_output=True, text=True,
)
```

Read the returned workflow id, current version, and legal links. Submit `run`
with that `workflowId`, `expectedVersion`, and `transition`. Save the response;
the delegated output is mapped into `context.candidate`. Inspect/query the
workflow after provider errors or driver timeouts before taking another action.
The installed CLI, gateway, and providers may need sandbox/network permission.

At `awaiting_review`, verify the candidate independently and retain the evidence.
Submit `accept` only when the acceptance criteria are met; otherwise submit
`reject`. Both transitions require `arguments: {"evidence": "<checks, outcomes,
artifact references or failure reason>"}`; the workflow retains this as
`context.verification` and emits independent decision evidence. This records the
caller's justification; it cannot prove the caller actually ran the checks.
Use the freshly returned version. The worker cannot submit either decision because it has no
Praxec tools. Do not give an unattended chooser control of this acceptance gate.

Each new task or workflow variant gets a fresh directory. A reject is an honest
terminal failure, not an automatic retry loop. The root agent may prepare an
improved attempt within the authorized task budget. This recipe supplies the
execution and acceptance boundary; catalog selection, parallel scheduling,
artifact integration, and workflow experiments remain Codex's responsibility.
