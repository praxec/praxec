//! Sole MCP wiring — makes praxec the only MCP server aether sees.
//!
//! This is the architectural invariant: the model has exactly one MCP tool
//! surface — Praxec's 7 stable tools. Every other capability (filesystem,
//! shell, external APIs, existing aether tools) is available only through
//! Praxec's `connections:` proxy layer.
//!
//! The model doesn't lose any capability. It gains governance over every
//! capability: typed workflows, guard evaluation, blackboard state,
//! transition records, and versioned definitions.
//!
//! ## How it works
//!
//! ```text
//! Model (Claude, OpenAI, Ollama...)
//!   │  calls: gateway.home / gateway.search / gateway.describe
//!   │          workflow.start / workflow.get / workflow.submit / workflow.explain
//!   ▼
//! praxec (child process, sole MCP server)
//!   │  ┌─ workflow state machine
//!   │  ├─ guidance delivery (inline + referenced skills)
//!   │  ├─ guard evaluation
//!   │  ├─ blackboard slots + versioned definitions
//!   │  ├─ transition records + audit sink
//!   │  └─ executor proxying via connections: CLI / MCP / REST / workflow
//!   ▼
//! External tools (configured in gateway YAML)
//!   ├─ shell commands    → kind: cli
//!   ├─ other MCP servers → kind: mcp
//!   ├─ REST APIs         → kind: rest
//!   ├─ nested workflows  → kind: workflow
//!   └─ human approval    → kind: human
//! ```
//!
//! ## Comparison with FrontRails
//!
//! FrontRails added their MCP gateway *alongside* aether's built-in tools
//! (filesystem, shell, etc.). The model could bypass governance by calling
//! aether's tools directly.
//!
//! Praxec replaces aether's tool surface entirely. There is no bypass path:
//! every tool call goes through a typed, guarded, auditable workflow
//! transition.

use aether_cli::mcp_config_args::McpConfigArgs;
use anyhow::{Result, anyhow};

/// Make praxec the **sole** MCP server.
///
/// Replaces whatever MCP config aether would normally use with a single
/// entry pointing at the praxec child process. The gateway YAML
/// (loaded by praxec) defines what downstream tools are reachable
/// through its executor proxy layer.
///
/// This is intentionally aggressive: clear + replace, not append.
///
/// Fails fast (B.3 in the audit-resolution plan) when the praxec
/// binary cannot be located AND `MCP_PRAXEC_PATH` is explicitly set but
/// points at a non-existent file. Plain "not in any well-known location"
/// is downgraded to a warning + bare PATH fallback — the actual spawn
/// failure will surface a more specific error than we can produce here.
pub fn set_as_sole_mcp(mcp_config: &mut McpConfigArgs) -> Result<()> {
    let config_json = sole_mcp_config()?;
    mcp_config.mcp_config_jsons.clear();
    mcp_config.mcp_config_jsons.push(config_json);
    Ok(())
}

/// Generate the MCP config JSON that wires aether to praxec as its
/// sole MCP server.
fn sole_mcp_config() -> Result<String> {
    let binary = find_praxec_binary()?;
    Ok(serde_json::json!({
        "mcpServers": {
            "praxec": {
                "command": binary,
                "env": {
                    "PRAXEC_CONFIG": default_config_path(),
                }
            }
        }
    })
    .to_string())
}

/// Locate the gateway binary (the unified `px`).
/// Resolution order:
/// 1. `MCP_PRAXEC_PATH` env var if set. **Errors fast** with
///    `MCP_PRAXEC_NOT_FOUND` if the path doesn't exist — operator-supplied
///    paths are an explicit contract, never silently fall back.
/// 2. Sibling next to `current_exe()` (bundled deployment).
/// 3. Bare string `"px"` (relies on PATH; spawn errors
///    will be actionable on their own).
///
/// Pub(crate) so the binary_discovery test suite can exercise it directly.
pub(crate) fn find_praxec_binary() -> Result<String> {
    // 1. Operator override via env var — strict.
    if let Ok(override_path) = std::env::var("MCP_PRAXEC_PATH")
        && !override_path.trim().is_empty()
    {
        let p = std::path::Path::new(&override_path);
        if !p.exists() {
            return Err(anyhow!(
                "MCP_PRAXEC_NOT_FOUND: MCP_PRAXEC_PATH is set to \
                     '{override_path}' but no file exists at that path. \
                     Unset the env var to fall back to discovery, or build the \
                     px binary."
            ));
        }
        return Ok(override_path);
    }

    // 2. Sibling next to our own binary (bundled deployment).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("px");
        if sibling.exists() {
            return Ok(sibling.to_string_lossy().to_string());
        }
        let sibling_exe = dir.join("px.exe");
        if sibling_exe.exists() {
            return Ok(sibling_exe.to_string_lossy().to_string());
        }
    }

    // 3. Assume on PATH. We don't pre-check via `which::which` to keep deps
    //    minimal; aether's spawn will surface a clean error if the lookup
    //    fails. We DO emit a hint via tracing so the user has context.
    tracing::debug!(
        "px not found as sibling of current_exe(); falling back to \
         PATH lookup. If spawn fails, set MCP_PRAXEC_PATH to the px binary."
    );
    Ok("px".to_string())
}

/// Default config path. When not overridden by PRAXEC_CONFIG env var,
/// praxec falls back to looking for `praxec.yaml` in the current
/// working directory.
fn default_config_path() -> String {
    std::env::current_dir()
        .map(|d| d.join("praxec.yaml").to_string_lossy().to_string())
        .unwrap_or_else(|_| "./praxec.yaml".to_string())
}
