# Embedding praxec

A guide for **library users** — engineers building their own
HATEOAS-inspired MCP APIs on top of praxec. If the README explains
how to run the gateway as a binary, this doc explains how to use the
crates as a library.

You'll learn how to:

1. Embed the runtime with a YAML config loaded at runtime (default).
2. Embed the runtime with a YAML config **baked in at compile time**, so
   end users can't edit your governance rules.
3. Build your own MCP server surface (your own tool names, your own
   semantics) backed by the workflow engine.
4. Plug in custom executors, guards, audit sinks, and stores.
5. Mix and match: keep most of the gateway, replace the parts you care
   about.

For *design* guidance — when to define a capability vs. a workflow, how to
nest gateways, etc. — see [`../architecture/mcp-control-architecture.md`](../architecture/mcp-control-architecture.md).

---

## Table of contents

1. [When to embed](#1-when-to-embed)
2. [The crate map](#2-the-crate-map)
3. [Quick start: runtime YAML config](#3-quick-start-runtime-yaml-config)
4. [Compile-time YAML: rules baked in](#4-compile-time-yaml-rules-baked-in)
5. [Building your own MCP API surface](#5-building-your-own-mcp-api-surface)
6. [Custom traits](#6-custom-traits)
7. [Build-time validation](#7-build-time-validation)
8. [Recipes](#8-recipes)

---

## 1. When to embed

Use the binary if you want a turnkey gateway driven by an editable YAML
file. Embed the crates as a library when:

- **You're shipping a domain-specific MCP server.** "Vendor's CRM Agent
  Bridge" wants the workflow engine but exposes its own tool surface
  with its own brand.
- **You need rules that can't be tampered with.** Bake the YAML in at
  compile time so end users (or operators with shell access) can't
  bypass governance by editing a config file.
- **You're embedding the runtime in a larger app.** Your service already
  has identity, request routing, and an HTTP front door — you just want
  link-driven workflow semantics layered on a few endpoints.
- **You want a custom MCP tool surface.** The two-tool stable surface
  is great as a default; if your domain prefers different shape (e.g. one
  tool per capability for legacy clients), you write the surface and the
  runtime drives the semantics.

If none of those apply, just use `praxec serve --config X.yaml` and
move on.

---

## 2. The crate map

```
praxec-core         runtime, ports, audit, reliability, capability,
                          discovery, config preprocessor, evidence,
                          in-memory + file + sqlite stores
praxec-executors    cli / mcp / rest / human / noop executors,
                          tools/list importer
praxec-mcp-server   the default two-tool ServerHandler
                          (PraxecServer)
praxec-schema       typify-generated Rust types from JSON schemas
                          (optional convenience for callers)
```

You'll typically depend on `praxec-core` plus
`praxec-executors`. Add `praxec-mcp-server` only if you want
the default two-tool surface; skip it if you're rolling your own.

```toml
[dependencies]
praxec-core      = "0.0"
praxec-executors = "0.0"
praxec-mcp-server = "0.0"   # optional: only for PraxecServer
rmcp                    = "1.7"   # to serve over MCP
tokio                   = { version = "1", features = ["full"] }
serde_json              = "1"
serde_yaml              = "0.9"
anyhow                  = "1"
```

---

## 3. Quick start: runtime YAML config

The simplest non-binary embedding — read YAML from disk at startup,
expose the standard two-tool surface over stdio:

```rust
use std::sync::Arc;

use praxec_core::{
    audit::StderrAuditSink,
    config::load_resolved,
    discovery::{DiscoveryIndex, InMemoryDiscoveryIndex},
    guards::DefaultGuardEvaluator,
    ports::EvidenceStore,
    store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore},
    WorkflowRuntime,
};
use praxec_executors::default_registry;
use praxec_mcp_server::PraxecServer;
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Resolves `include:`, `capabilities:`, and `wraps:` into the inline
    // shapes the runtime understands.
    let config = load_resolved("gateway.yaml")?;

    let evidence: Arc<dyn EvidenceStore> = Arc::new(InMemoryEvidenceStore::new());
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&config)),
        Arc::new(InMemoryWorkflowStore::new()),
        default_registry(&config),
        Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone())),
        Arc::new(StderrAuditSink),
    )
    .with_evidence(evidence);

    let discovery: Arc<dyn DiscoveryIndex> =
        Arc::new(InMemoryDiscoveryIndex::from_config(&config));

    let server = PraxecServer::new(runtime).with_discovery(discovery);
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}
```

This is roughly what the binary does. Swap `InMemoryWorkflowStore` for
`FileWorkflowStore::new(dir)?` or `SqliteWorkflowStore::open(path)?` for
durable instance state.

---

## 4. Compile-time YAML: rules baked in

For binaries you ship to others, runtime YAML loading is a hole in your
governance: anyone with shell access can edit the file and bypass guards.
praxec is designed to support **compile-time** config too: bake
your YAML into the binary as a string constant, parse it at startup, and
end users can't change behavior without recompiling.

### Single-file pattern

```rust
use praxec_core::config::resolve_str;

const CONFIG_YAML: &str = include_str!("../config/gateway.yaml");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = resolve_str(CONFIG_YAML)?;
    // ... build runtime exactly as in §3
}
```

`include_str!` reads the file at compile time and embeds it as a
`&'static str`. Cargo automatically rebuilds when the YAML changes.

### Multi-file pattern (locked `include:`)

`include:` directives in YAML need filesystem access at runtime, so they
won't work with `include_str!`. Pre-merge instead:

```rust
use praxec_core::config::{deep_merge, resolve};
use serde_json::Value;

const BASE: &str   = include_str!("../config/base.connections.yaml");
const POLICY: &str = include_str!("../config/team.policy.yaml");
const MAIN: &str   = include_str!("../config/gateway.yaml");

fn embedded_config() -> anyhow::Result<Value> {
    let base:   Value = serde_yaml::from_str(BASE)?;
    let policy: Value = serde_yaml::from_str(POLICY)?;
    let main:   Value = serde_yaml::from_str(MAIN)?;

    // Same semantics as the file-based `include:` chain: maps merge,
    // arrays concatenate, scalars: last writer wins.
    let merged = deep_merge(deep_merge(base, policy), main);
    Ok(resolve(merged)?)
}
```

Strip the `include:` keys from your YAML files in this case — they're
handled by your build code, not by the loader.

### Why this matters

A model talking to your gateway can only do what your YAML allows. If
the YAML is in the binary, an attacker would need to:

- modify the binary itself, or
- replace the binary with a malicious one,

both of which are detectable by code-signing and binary-attestation
toolchains. Editing a config file is not.

This is the right shape for "compliance MCP" — gateways shipped to
customers where the rules are part of the product, not a deploy-time
choice.

---

## 5. Building your own MCP API surface

The default `PraxecServer` exposes two stable tools
(`praxec.query` for reads + `praxec.command` for writes; each routes
to home/search/describe/get/explain or start/submit/define by arg-shape).
That's a great default — but it's not required. The runtime is just
"start a workflow, submit a transition, read the response." Wrap that
in any tool surface that fits your product.

### Pattern: replace the two-tool surface with your own

Implement `rmcp::ServerHandler` directly. Your `list_tools` returns
whatever you want; your `call_tool` translates each call into runtime
operations.

```rust
use std::borrow::Cow;
use std::sync::Arc;

use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::WorkflowRuntime;
use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListToolsResult, PaginatedRequestParams, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{json, Value};

pub struct CrmAgentServer {
    runtime: WorkflowRuntime,
}

impl ServerHandler for CrmAgentServer {
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Domain-specific tools; the runtime is invisible to callers.
        let tools = vec![
            Tool::new(
                Cow::Borrowed("crm.find_account"),
                Cow::Borrowed("Look up a customer account by email."),
                schema(json!({
                    "type": "object",
                    "required": ["email"],
                    "properties": { "email": { "type": "string" } }
                })),
            ),
            Tool::new(
                Cow::Borrowed("crm.escalate_ticket"),
                Cow::Borrowed("Open a tier-2 escalation."),
                schema(json!({
                    "type": "object",
                    "required": ["ticketId", "reason"],
                    "properties": {
                        "ticketId": { "type": "string" },
                        "reason":   { "type": "string" }
                    }
                })),
            ),
        ];
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args: Value = request
            .arguments
            .as_ref()
            .map(|m| Value::Object(m.clone()))
            .unwrap_or_else(|| json!({}));

        let result = match request.name.as_ref() {
            "crm.find_account" => {
                // Run the find_account workflow to completion in one call.
                let started = self
                    .runtime
                    .start(StartWorkflow {
                        definition_id: "find_account".into(),
                        input: args,
                        principal: Principal::anonymous(),
                    })
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                started
            }
            "crm.escalate_ticket" => {
                // Multi-step workflow exposed as a single MCP call: start
                // the escalation workflow, run a `submit` transition, hand
                // back the final response. (Real impl would loop until the
                // workflow is `completed` and return the last response.)
                let started = self
                    .runtime
                    .start(StartWorkflow {
                        definition_id: "escalate_ticket".into(),
                        input: args.clone(),
                        principal: Principal::anonymous(),
                    })
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
                let version = started["workflow"]["version"].as_u64().unwrap();
                self.runtime
                    .submit(SubmitTransition {
                        workflow_id,
                        expected_version: version,
                        transition: "open".into(),
                        arguments: args,
                        principal: Principal::anonymous(),
                    })
                    .await
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown tool '{other}'"),
                    None,
                ));
            }
        };

        Ok(CallToolResult::structured(result))
    }
}

fn schema(value: Value) -> Arc<rmcp::model::JsonObject> {
    Arc::new(value.as_object().unwrap().clone())
}
```

Some tradeoffs:

- **You give up automatic link-driven discovery** for these tools. The
  model can't see the `links` array of the inner workflow response
  unless you return it. (You can! Just include the workflow response
  shape in your `CallToolResult` payload and document the link
  semantics for the caller.)
- **You give up the two-tool stability invariant** because your tool
  list is your own. That's fine — invariants are about clients of the
  default `PraxecServer`, not about the runtime itself.
- **You keep all the governance**: input-schema validation, guards,
  reliability, audit, evidence, persistent stores. The runtime doesn't
  care which surface drives it.

### Pattern: link-driven domain tools

Want navigable, HATEOAS-inspired responses but with your own naming?
Return a domain-shaped response that includes links describing the
caller's legal next moves:

```rust
let response = json!({
    "account": {
        "id": account_id,
        "email": email,
    },
    "_links": [
        { "rel": "open_ticket",      "method": "crm.open_ticket",      "args": { "accountId": account_id } },
        { "rel": "view_history",     "method": "crm.view_history",     "args": { "accountId": account_id } },
        { "rel": "escalate_ticket",  "method": "crm.escalate_ticket",  "args": { "accountId": account_id } },
    ]
});
```

The runtime doesn't care what the response looks like — that's your
product's contract.

---

## 6. Custom traits

Each cross-cutting concern is a trait in `praxec_core::ports`.
Implement to plug in a custom backend.

### Custom executor

```rust
use async_trait::async_trait;
use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::ports::Executor;

pub struct GraphqlExecutor { /* client, endpoint, ... */ }

#[async_trait]
impl Executor for GraphqlExecutor {
    async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        // Read req.executor_config for query / variables, run it, return.
        // Classify failures correctly so reliability policies retry well:
        //   - timeout / 504              -> ExecutorError::Timeout(_)
        //   - rate limited (429, "RATE_LIMITED" code) -> ExecutorError::RateLimited
        //   - 5xx, network blips         -> ExecutorError::Transient
        //   - 4xx (other)                -> ExecutorError::Permanent
        todo!()
    }
}
```

Register it alongside the defaults:

```rust
use std::sync::Arc;
use praxec_executors::HashMapExecutorRegistry;
use praxec_core::ports::ExecutorRegistry;

let registry = HashMapExecutorRegistry::new()
    .with("graphql", Arc::new(GraphqlExecutor::new()))
    .with("noop",    Arc::new(praxec_executors::NoopExecutor));
let executors: Arc<dyn ExecutorRegistry> = Arc::new(registry);
```

YAML can now reference `kind: graphql` from any executor block.

### Custom guard kind

```rust
use async_trait::async_trait;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, WorkflowInstance};
use praxec_core::ports::GuardEvaluator;
use serde_json::Value;

pub struct OpaGuard {
    inner: DefaultGuardEvaluator,
    /* OPA client */
}

#[async_trait]
impl GuardEvaluator for OpaGuard {
    async fn evaluate(
        &self,
        guard: &Value,
        instance: &WorkflowInstance,
        arguments: &Value,
        principal: &Principal,
    ) -> anyhow::Result<bool> {
        match guard.get("kind").and_then(Value::as_str) {
            Some("opa") => {
                // call OPA, return decision
                todo!()
            }
            _ => self.inner.evaluate(guard, instance, arguments, principal).await,
        }
    }
}
```

### Custom audit sink

```rust
use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink};

pub struct OtelAuditSink { /* tracer */ }

#[async_trait]
impl AuditSink for OtelAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        // emit a span / event to your telemetry backend
        Ok(())
    }
}
```

Combine multiple sinks if you want both stderr AND your sink:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use praxec_core::audit::{AuditEvent, AuditSink};

pub struct TeeAuditSink {
    sinks: Vec<Arc<dyn AuditSink>>,
}

#[async_trait]
impl AuditSink for TeeAuditSink {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        for sink in &self.sinks {
            let _ = sink.record(event.clone()).await; // ignore individual failures
        }
        Ok(())
    }
}
```

### Custom workflow store

Implement `praxec_core::ports::WorkflowStore` to back instance
state with Redis, a SQL database, etc. The trait has three methods:
`create`, `load`, `save_if_version`. The version-check is the
optimistic-locking primitive — make sure your impl rejects writes
when the stored version doesn't match `expected_version`.

### Custom evidence store

Same pattern with `EvidenceStore` (`record` and `list`) for durable
evidence backing the `evidence` guard.

---

## 7. Build-time validation

For compile-time configs, catch YAML mistakes at `cargo build` instead
of at startup. A small `build.rs`:

```rust
// build.rs
use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=config/gateway.yaml");
    let yaml = fs::read_to_string("config/gateway.yaml")
        .expect("config/gateway.yaml must exist");
    let value: serde_yaml::Value = serde_yaml::from_str(&yaml)
        .expect("config/gateway.yaml is malformed");
    // Optional: round-trip through resolve() to validate capability refs.
    // Requires depending on praxec-core in [build-dependencies].
}
```

For deeper validation, add `praxec-core` to `[build-dependencies]`
and call `praxec_core::config::resolve_str(&yaml)` in `build.rs`.
Unknown capability references and `wraps:` cycles will fail the build.

For schema validation against the JSON schema in `schemas/`, add the
`jsonschema` crate to `[build-dependencies]` and validate before
embedding. Belt-and-suspenders for shipped binaries.

---

## 8. Recipes

### 8a. Hybrid: keep PraxecServer's two tools, add custom ones

You don't have to choose. Wrap `PraxecServer` in your own
`ServerHandler` that delegates the standard two tools and adds your
own:

```rust
pub struct HybridServer {
    inner: PraxecServer,
    /* domain state */
}

impl ServerHandler for HybridServer {
    async fn list_tools(&self, /*…*/) -> Result<ListToolsResult, McpError> {
        let mut tools = praxec_mcp_server::tool_definitions();
        tools.push(/* your domain tool */);
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(&self, request: CallToolRequestParams, ctx: RequestContext<RoleServer>)
        -> Result<CallToolResult, McpError> {
        if praxec_mcp_server::STABLE_TOOL_NAMES.contains(&request.name.as_ref()) {
            self.inner.call_tool(request, ctx).await
        } else {
            // your domain tool
            todo!()
        }
    }
    // ... other ServerHandler methods delegate to self.inner
}
```

### 8b. Locked rules + per-tenant runtime config

Sometimes you want core rules baked in but per-tenant connections
loaded at runtime. Compile in the `capabilities:` and `workflows:`
blocks; load `connections:` from a tenant config file and merge:

```rust
use praxec_core::config::{deep_merge, resolve};

const FIXED: &str = include_str!("../fixed.yaml");

fn build_for_tenant(tenant_yaml: &str) -> anyhow::Result<Value> {
    let fixed:  Value = serde_yaml::from_str(FIXED)?;
    let tenant: Value = serde_yaml::from_str(tenant_yaml)?;
    // Tenant supplies connections; can't override the locked rules
    // because the embedded YAML's keys take precedence after we put
    // the tenant config first and embedded second.
    Ok(resolve(deep_merge(tenant, fixed))?)
}
```

This makes the *connections* per-tenant but the *workflows and guards*
non-negotiable.

### 8c. Use the runtime as a state machine engine, no MCP at all

Want HATEOAS-inspired, link-driven state-machine semantics in a non-MCP
context? Just hold a `WorkflowRuntime` in your service and call
`start` / `submit` / `get` / `explain` directly from your HTTP
handlers. The MCP server is optional packaging.

```rust
async fn http_post_workflow(state: AppState, body: StartBody) -> Json<Value> {
    let response = state
        .runtime
        .start(StartWorkflow {
            definition_id: body.definition_id,
            input: body.input,
            principal: state.principal_from_auth(),
        })
        .await
        .unwrap();
    Json(response)
}
```

Your HTTP API gets navigable links describing legal next moves, audit,
reliability, guards, evidence, and persistent state — for free.

### 8d. Identity: wiring `Principal` into a custom server surface

The bundled `PraxecServer` treats every caller as
`Principal::anonymous()` — fine for a single-tenant local setup, but
inert for `permission` / `role` guards. To make those guards live,
build your own `ServerHandler` (per §5) and source the principal from
whatever your transport gives you.

```rust
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};

fn principal_from_request(ctx: &RequestContext<RoleServer>) -> Principal {
    // Examples by transport. Pick whichever matches yours.
    //
    // 1. Streamable-HTTP behind a reverse proxy that injects identity:
    //    let sub = ctx.headers().get("x-forwarded-user").to_string();
    //    let perms = ctx.headers().get_all("x-forwarded-permissions").collect();
    //    Principal { subject: sub, roles: vec![], permissions: perms }
    //
    // 2. JWT validated by an outer layer, claims forwarded as headers:
    //    let claims = decode_and_verify(ctx.headers().get("authorization"))?;
    //    Principal { subject: claims.sub, roles: claims.roles, permissions: claims.scope }
    //
    // 3. mTLS where the peer cert subject is your identity:
    //    let cn = ctx.peer_cert_subject_cn();
    //    Principal { subject: cn, roles: lookup_roles(cn), permissions: lookup_perms(cn) }
    //
    // 4. Single-tenant with a static service identity from env:
    //    Principal {
    //        subject: std::env::var("PRAXEC_PRINCIPAL").unwrap_or_default(),
    //        roles: env_csv("PRAXEC_ROLES"),
    //        permissions: env_csv("PRAXEC_PERMISSIONS"),
    //    }
    Principal::anonymous()
}
```

Pass the resolved principal to every `runtime.start` / `runtime.submit`
/ `runtime.get` call. Guards see populated `roles` / `permissions` and
the audit log records `actor` accurately. No runtime code change is
needed — the trust boundary is the call site that constructs the
`Principal`.

> **Don't trust model-asserted identity.** If your MCP host can be
> driven by an LLM, never source the principal from a tool argument or
> a field the model controls. The principal must come from a
> transport-level credential the model can't influence: a verified JWT,
> mTLS cert, mutually-authenticated session, or out-of-band header
> injected by an upstream that the model cannot reach.

### 8e. Test your YAML with the runtime in a unit test

You can drive the runtime in tests just like the integration tests do:

```rust
#[tokio::test]
async fn deploy_workflow_blocks_without_approval() {
    let config = praxec_core::config::resolve_str(include_str!("../my.yaml"))?;
    // build runtime, start workflow, submit transition, assert on response.error.code
}
```

This is the lowest-cost way to catch governance regressions.

---

## In one sentence

> Use `PraxecServer` for the standard surface; replace it when you need
> domain-specific tools; bake your YAML in with `include_str!` +
> `resolve_str` when end users shouldn't be able to edit it.
