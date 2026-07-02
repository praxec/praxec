//! `px mcp init` — generate MCP client config files for the project.
//!
//! ACP-mode sessions (`px acp`) and most MCP-host editors (Cursor,
//! Claude Desktop, Claude Code, VS Code via Continue/Cline) source their
//! MCP server config from on-disk JSON files — `.mcp.json` at the project
//! root, `.cursor/mcp.json`, `claude_desktop_config.json`, etc. None of
//! them accept the MCP config via CLI args the way `px headless`
//! does, so the sole-MCP wiring has to happen via file generation rather
//! than programmatic injection.
//!
//! This module is the file-generator. `px mcp init` writes the
//! praxec MCP server entry to each requested config file, using the
//! same `find_praxec_binary` resolution rules as the headless path.
//!
//! Refuses to overwrite existing files by default — operators get a
//! clean error and can decide whether to merge by hand or pass `--force`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::praxec_mcp;

#[derive(clap::Args, Debug)]
pub struct McpInitArgs {
    /// Target directory (defaults to the current working directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,

    /// Also generate `.cursor/mcp.json` in the target directory.
    #[arg(long)]
    pub cursor: bool,

    /// Also generate `claude_desktop_config.json` snippet in the
    /// target directory. Note: Claude Desktop's actual config lives
    /// in a platform-specific location (e.g. `~/Library/Application
    /// Support/Claude/` on macOS); the file we generate is a copy
    /// you merge into the real config. The path is shown on stdout.
    #[arg(long = "claude-desktop")]
    pub claude_desktop: bool,

    /// Overwrite existing config files instead of erroring.
    #[arg(long)]
    pub force: bool,
}

/// Entry point. Resolves the target directory, builds the praxec
/// MCP server JSON, and writes it to every requested config file.
pub fn run_init(args: &McpInitArgs) -> Result<()> {
    let dir = match &args.dir {
        Some(p) => p.clone(),
        None => std::env::current_dir().context("resolving current working directory")?,
    };
    if !dir.exists() {
        bail!("target directory does not exist: {}", dir.display());
    }

    let config_json = build_praxec_mcp_config_json(&dir)?;

    // The base `.mcp.json` always lands at <dir>/.mcp.json — that's the
    // shape Claude Code, Continue, Cline, and any project-root MCP
    // client expects.
    let base_path = dir.join(".mcp.json");
    write_config_file(&base_path, &config_json, args.force)?;
    println!("wrote {}", base_path.display());

    if args.cursor {
        // Cursor uses .cursor/mcp.json inside the project root.
        let cursor_dir = dir.join(".cursor");
        std::fs::create_dir_all(&cursor_dir)
            .with_context(|| format!("creating {}", cursor_dir.display()))?;
        let cursor_path = cursor_dir.join("mcp.json");
        write_config_file(&cursor_path, &config_json, args.force)?;
        println!("wrote {}", cursor_path.display());
    }

    if args.claude_desktop {
        // Claude Desktop's real config lives in a platform-specific
        // location. We generate a snippet operators paste into the
        // real file (or merge with their existing mcpServers block).
        let desktop_path = dir.join("claude_desktop_config.json");
        write_config_file(&desktop_path, &config_json, args.force)?;
        println!("wrote {}", desktop_path.display());
        println!(
            "  note: Claude Desktop's actual config lives at:\n\
             \tmacOS:   ~/Library/Application Support/Claude/claude_desktop_config.json\n\
             \tLinux:   ~/.config/Claude/claude_desktop_config.json\n\
             \tWindows: %APPDATA%\\Claude\\claude_desktop_config.json\n\
             merge the `mcpServers.praxec` entry from the generated file \
             into your actual config and restart Claude Desktop."
        );
    }

    Ok(())
}

/// Build the MCP server config JSON that wires praxec as the sole
/// MCP server. Same shape as `praxec_mcp::sole_mcp_config()` so an
/// operator generating the file gets the same wiring `run_headless`
/// would inject programmatically.
///
/// The `dir` argument seeds `PRAXEC_CONFIG` — if there's a
/// `praxec.yaml` in that directory, we point at it; otherwise we
/// leave the env unset and let praxec's own resolution fall
/// back to cwd at spawn time.
fn build_praxec_mcp_config_json(dir: &Path) -> Result<String> {
    let binary = praxec_mcp::find_praxec_binary()?;
    let gateway_yaml = dir.join("praxec.yaml");

    let env = if gateway_yaml.exists() {
        json!({ "PRAXEC_CONFIG": gateway_yaml.to_string_lossy() })
    } else {
        json!({})
    };

    let body = json!({
        "mcpServers": {
            "praxec": {
                "command": binary,
                "env": env,
            }
        }
    });

    // Pretty-print: these files are operator-readable; they should
    // diff cleanly in version control.
    serde_json::to_string_pretty(&body).context("serializing MCP config JSON")
}

/// Write `contents` to `path`. Refuses to overwrite an existing file
/// unless `force` is set. Identical contents under non-force mode is
/// a silent no-op (idempotent).
fn write_config_file(path: &Path, contents: &str, force: bool) -> Result<()> {
    if path.exists() {
        // Idempotent: if the file already has exactly these contents,
        // nothing to do. (Trailing newline normalised for comparison.)
        let existing = std::fs::read_to_string(path)
            .with_context(|| format!("reading existing {}", path.display()))?;
        if existing.trim_end() == contents.trim_end() {
            return Ok(());
        }
        if !force {
            bail!(
                "refusing to overwrite existing {} (contents differ). \
                 Re-run with --force to overwrite, or merge by hand.",
                path.display()
            );
        }
    }
    // Ensure trailing newline so the file is well-formed text.
    let mut to_write = contents.to_string();
    if !to_write.ends_with('\n') {
        to_write.push('\n');
    }
    std::fs::write(path, to_write).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
