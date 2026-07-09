# Runtime invariants

Things you can rely on no matter what's in the config. Each invariant
has a corresponding test — if you build on top of praxec, these
are the contracts.

---

1. **Proxy exposure compiles to a null-op workflow transition.** The
   simple `proxy.expose: [...]` form is just sugar for a single-state
   workflow named `proxy_default`. Same engine, same guarantees.

2. **All transitions validate `inputSchema` before execution.** Bad
   input never reaches your executor; you get a `INPUT_SCHEMA_VIOLATION`
   rejection with the schema's complaint and current legal links.

3. **Guards run before executor dispatch.** A failing guard means the
   executor doesn't fire. No half-executed transitions on guard rejection.

4. **Executors never decide workflow legality.** When an executor
   fails, the workflow stays in its current state with
   `result.status = "failed"`. State only advances on successful
   transitions.

5. **Invalid transitions return current legal links.** Even on
   rejection, the response carries the `links` array for the current
   state so the caller can recover without restarting the workflow.

6. **Every submit requires `expectedVersion`.** Stale versions are
   rejected with `STALE_WORKFLOW_VERSION`. This gives you optimistic
   concurrency control for free.

7. **Every successful transition increments `workflow.version`.** Used
   for the optimistic-locking check above.

8. **Terminal states return no links.** When `result.status =
   "succeeded"` (or `"failed"`), the workflow is done. The model knows to stop.

9. **The MCP-facing tool list is exactly two names** —
   `praxec.query` (reads) and `praxec.command` (writes) — regardless
   of config (SPEC §32). All discovery and workflow operations are reached
   by varying args, not the tool name.
   Capabilities surface through links inside response payloads
   (HATEOAS-inspired; see [../architecture/concepts.md](../architecture/concepts.md)), not as new MCP
   tools. Your model never has to learn a new tool list when the config
   grows.

10. **Downstream tools are only reachable through configured
    transitions.** No backdoor execution paths. If the YAML doesn't
    declare it, the gateway can't run it.

---

## Where the tests live

- `crates/praxec-core/tests/invariants_actor_audit.rs`,
  `invariants_governance.rs`, `invariants_proxy.rs` — invariants 1–8 and
  10, plus audit-event emission for rejection / success / fallback paths.
- `crates/praxec-mcp-server/tests/stable_tool_surface.rs` —
  invariant 9. Asserts the rmcp tool list has exactly the two
  documented names with valid `inputSchema` for each.
- `crates/praxec-core/tests/composability.rs` — capability
  references, `wraps:`, `include:`, and end-to-end dispatch through
  capability refs. Proves invariants 2–4 still hold under composition.
- `crates/praxec-core/tests/persistent_stores.rs` — round-trip
  + optimistic locking semantics for file-backed and SQLite stores.
  Proves invariants 6–7 hold across persistent backends.
- `crates/praxec-core/tests/chain_basic.rs`, `chain_audit.rs`,
  `chain_audit_criticality.rs`, `chain_guidance.rs`, `chain_loop.rs` —
  deterministic chaining (auto-execute, mixed-state stop, depth limit,
  failure recovery, phase guidance, audit events).

Run them: `cargo test --workspace`.

---

## Why these matter

If you're building anything that depends on praxec's
links-and-governance contract — a domain-specific MCP server, a
multi-gateway control plane, an embedded engine — these are the rules
that won't change without a major-version bump. Build to them.
