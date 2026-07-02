# Pattern: Scripts + Script Acknowledgment Guard

SPEC §22. Curated, hash-pinned scripts with a `script_acknowledged` guard
that gates dangerous operations behind a review step.

## The pattern

```text
  review_deploy
    │ acknowledge
    ▼
  execute_deploy
    │ run deploy.production.rollout  (script executor)
    ▼
  done
```

The `acknowledge` transition carries a `script_acknowledged` guard. The LLM
must call `gateway.describe` on the script's subject before the guard passes.
If the script body is edited (hash flips), the acknowledgment is invalidated
and the guard requires re-review.

## Script features shown

| Feature | Demo |
|---------|------|
| Inline body | `build.cargo.release` |
| Hash computation | Auto-computed at load; optional manual hash with mismatch detection |
| `script` executor | `kind: script` with `env`, `workingDirectory`, `args` |
| Env var injection | `PRAXEC_SCRIPT_SUBJECT`, `PRAXEC_SCRIPT_HASH` auto-set |
| `script_acknowledged` guard | `kind: script_acknowledged` on the execute transition |
| `treatNonZeroAsFailure` | Default `true` — non-zero exit fails the transition |

## Script verbs (closed enum, §22.3)

`build`, `test`, `deploy`, `format`, `lint`, `install`, `verify`, `run`,
`inspect`, `search`, `fetch`, `audit`.

## Run it

```bash
praxec check --config examples/pattern-scripts/gateway.yaml
```
