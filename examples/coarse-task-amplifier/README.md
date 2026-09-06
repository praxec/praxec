# Coarse-task amplifier

One model produces a complete proposal. A script checks its required structure
and preserves the actual tool result. The harness advances automatically after
verification; no model is paid to choose the next mechanical step.

This example checks nonempty sections and explicit risks, **not semantic quality**.
Use real acceptance checks for a production deliverable. For code, check the
actual changed artifact and revision with the relevant build/tests and MCP tools.

The config uses JSON syntax, which is valid YAML. With a binary built from the
amplifier-profile source (the existing multi-agent example supplies illustrative
model bindings for the config check):

```bash
praxec check --config examples/coarse-task-amplifier/gateway.yaml
```

For live use, copy the config to a persistent run directory, replace ephemeral
storage with SQLite/file audit, and point `gateway.models_yaml` to your existing
operator bindings. The worker has no tools and produces a proposal only. Start
`coarse_task` with nonblank string fields `intent`, `context`, `deliverable`, and
`acceptance`; submit its returned `produce` link with the current version.
Verification then runs deterministically. With agent auto-drive enabled, the
declared producer runs directly without an additional argument-generation model.

The `amplifier_contract` preflight guard makes older standard binaries reject the
workflow before generation instead of silently ignoring the new profile.
The [design contract](../../docs/design/2026-09-06-coarse-task-amplifier.md)
describes the exact controls and limits.
