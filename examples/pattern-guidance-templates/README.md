# Pattern: Guidance Templates + Guidance Acknowledgment Guard

SPEC §5.2, §5.9. `goal` and `guidance` strings are templates — `{{ }}`
placeholders resolve against live workflow context. The `guidance_acknowledged`
guard gates transitions behind confirmed review of a named skill fragment.

## Template syntax

```yaml
guidance: >
  {{ $.context.testCount }} tests run. {{ $.context.testErrors }} failures.
  Review the patch; target state: {{ $.workflow.input.targetEnvironment }}.
```

**Interpolation roots:**

| Root | Source |
|------|--------|
| `$.context.*` | Blackboard slots |
| `$.workflow.input.*` | Initial workflow input |
| `$.workflow.*` | Workflow metadata (id, version, state) |

Unresolved placeholders render as `(slotName: unset)`, never an error.

## Guidance acknowledgment guard

```yaml
guards:
  - kind: guidance_acknowledged
    subject: review.security.checklist
```

The LLM must call `gateway.describe` on `subject` before submitting a
transition with this guard. If the skill body is edited (hash flips),
the ack is invalidated — the model must re-review.

**Distinct from `script_acknowledged`** — skills and scripts use separate
ack stores. Acknowledging a skill does not satisfy a script guard, and
vice versa.

## Reference fragments

`skills:` declared at top level. Referenced at three scopes:
- `workflow:` — every response in the workflow
- `state:` — only in this state
- `transition:` — only on this link

## Run it

```bash
praxec check --config examples/pattern-guidance-templates/gateway.yaml
```
