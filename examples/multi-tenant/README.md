# Multi-tenant identity wiring

This example demonstrates how to wire identity for a multi-tenant
praxec deployment where different humans share one gateway.

## The problem

In a single-user setup, every caller is anonymous and every permission
check passes. In a multi-tenant deployment, you need to know:

1. **Who** is calling (the principal's subject)
2. **What roles** they hold
3. **What permissions** they have

The gateway enforces `permission` and `role` guards based on the
`Principal` attached to each `workflow.submit` call. The MCP transport
must supply this principal — see
[docs/architecture/mcp-control-architecture.md](../../docs/architecture/mcp-control-architecture.md#identity-in-multi-tenant-deployments)
for the architecture.

## Config walkthrough

This config defines two tenants (`tenant_a` and `tenant_b`), each with
their own database connection. Capabilities are defined once with
generic guards and then exposed per-tenant with tenant-specific
executor configs.

### Guards in play

| Guard | Purpose |
|-------|---------|
| `role: tenant_admin` | Only users with the `tenant_admin` role can read or write. |
| `role: super_admin` | Only super-admins can write to tenant B's database. |
| `permission: tenant.write` | The caller must hold the `tenant.write` permission. |

### Testing with curl

The gateway speaks MCP over stdio by default. To test with HTTP,
configure a Streamable HTTP transport:

```bash
# Start the gateway with HTTP transport
praxec serve --config examples/multi-tenant/gateway.yaml \
  --transport http --port 8080
```

Then use curl to call tools:

```bash
# Search for available capabilities
curl -X POST http://localhost:8080/tools/call \
  -H "Content-Type: application/json" \
  -H "X-Principal-Subject: alice@tenant-a.com" \
  -H "X-Principal-Roles: tenant_admin" \
  -H "X-Principal-Permissions: tenant.write" \
  -d '{
    "name": "gateway.search",
    "arguments": { "query": "tenant" }
  }'

# Start a proxy session and read tenant A data
curl -X POST http://localhost:8080/tools/call \
  -H "Content-Type: application/json" \
  -H "X-Principal-Subject: alice@tenant-a.com" \
  -H "X-Principal-Roles: tenant_admin" \
  -H "X-Principal-Permissions: tenant.write" \
  -d '{
    "name": "workflow.start",
    "arguments": {
      "definitionId": "proxy_default",
      "input": {}
    }
  }'
```

### Principal resolution

The gateway resolves the `Principal` from the MCP transport. For
stdio transport, the principal is always anonymous (single-user mode).
For Streamable HTTP, the gateway reads identity from headers:

- `X-Principal-Subject` — the caller's identifier (email, user ID)
- `X-Principal-Roles` — comma-separated role list
- `X-Principal-Permissions` — comma-separated permission list

> **Note:** The HTTP header-based identity resolution is a reference
> implementation. Production deployments should use a proper identity
> proxy (Envoy, OAuth2-proxy, or similar) that validates tokens and
> injects verified headers.

## Running

```bash
# Validate the config
cargo run -p praxec -- check --config examples/multi-tenant/gateway.yaml

# Serve (stdio — anonymous principal, all guards pass)
cargo run -p praxec -- serve --config examples/multi-tenant/gateway.yaml
```

## Validation

The identity and authorization logic demonstrated in this example is validated
by integration tests in
[`crates/praxec-core/tests/multi_tenant.rs`](../../crates/praxec-core/tests/multi_tenant.rs).

These tests prove:

- Different principals see different link surfaces from the same workflow
  state based on their roles and permissions (`linkFilter: byGuards`).
- `role` guards correctly filter transitions by principal role.
- `permission` guards correctly filter transitions by principal permission.
- `all_of` guards require all sub-guards to pass.
- Anonymous principals see no guarded links.
- Guard enforcement on `workflow.submit` rejects unauthorized transitions
  with `GUARD_REJECTED`.

Run the tests with:

```bash
cargo test -p praxec-core --test multi_tenant
```

## See also

- [MCP-CONTROL-ARCHITECTURE.md](../../docs/architecture/mcp-control-architecture.md#identity-in-multi-tenant-deployments)
- [GOVERNANCE.md](../../docs/reference/governance.md#guards-preconditions)
- [CONFIG.md](../../docs/reference/configuration.md#capabilities)