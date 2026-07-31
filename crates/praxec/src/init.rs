//! `praxec init` support — cross-platform single-command onboarding.
//!
//! Split in two layers so the file-writing half is unit-testable without a
//! TTY or a real filesystem probe:
//! - **pure scaffolding**: the exact `gateway.yaml` / `models.yaml` text, the
//!   idempotent (skip-unless-`--force`) file writer, and the editor MCP-config
//!   JSON merge (add/replace only the `praxec` server key, never clobber
//!   another server) — all plain functions over `Path`/`Value`.
//! - **interactive/detection seam** ([`InitIo`]): editor auto-detection,
//!   confirm-before-write, and the API-key prompt. [`RealInitIo`] is the one
//!   production implementation (stdin/stdout + `which`/home-dir probes); tests
//!   supply a fake instead of touching the real terminal or machine.
//!
//! The orchestration handler that ties these together (plus the `preflight`/
//! `doctor` epilogue, which needs `crate::preflight`) lives in
//! [`crate::gateway::init`].

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Value, json};

/// Resolve the scaffold target directory: `--dir` if given, else
/// `dirs::config_dir()/praxec` (`%APPDATA%\praxec` on Windows,
/// `~/.config/praxec` on Linux, `~/Library/Application Support/praxec` on
/// macOS). Does NOT create the directory — callers create it explicitly so
/// the "creating X" step is visible/failable on its own.
pub(crate) fn resolve_target_dir(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(d) = dir {
        return Ok(d);
    }
    dirs::config_dir().map(|d| d.join("praxec")).ok_or_else(|| {
        anyhow::anyhow!(
            "cannot locate a config directory on this machine; pass --dir <path> explicitly"
        )
    })
}

/// Quote an absolute path as a YAML double-quoted scalar, escaping `\` and
/// `"` so a Windows path (`C:\Users\...`) round-trips safely and a path with
/// spaces/colons never needs a plain-scalar edge case.
fn yaml_quoted(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// The scaffolded `gateway.yaml` content for target directory `dir`
/// (expected absolute). `repos:`/`connections:` are left as commented hints —
/// an empty-but-present `repos: []` would still be a config the operator has
/// to notice and remove; a comment is unambiguous "nothing configured yet".
pub(crate) fn gateway_yaml_content(dir: &Path) -> String {
    let models = yaml_quoted(&dir.join("models.yaml"));
    let audit = yaml_quoted(&dir.join("audit-logs"));
    let db = yaml_quoted(&dir.join("praxec.db"));
    format!(
        r#"version: "1.0.0"
gateway:
  principal: {{ subject: operator, roles: [human] }}
  models_yaml: {models}
praxec:
  embeddings: {{ enabled: false }}
  agents: {{ auto_drive: false }}
audit: {{ sink: file, path: {audit}, rotation: daily }}
store: {{ kind: sqlite, path: {db} }}
# repos: []        # add packs here (or run `praxec sync`)
# connections: {{}}  # add MCP tools here (or provision via flow.tools.provision)
"#
    )
}

/// The scaffolded starter `models.yaml`: a commodity-first default chain on
/// OpenRouter (SPEC §33 D9 model-cost-control defaults).
pub(crate) fn models_yaml_content() -> &'static str {
    r#"version: 1
default:
  - { provider: { name: openrouter }, model: z-ai/glm-5.2 }
  - { provider: { name: openrouter }, model: deepseek/deepseek-v4-pro, effort: high }
  - { provider: { name: openrouter }, model: anthropic/claude-haiku-4-5 }
"#
}

/// Outcome of writing (or skipping) one scaffolded file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScaffoldOutcome {
    Wrote,
    Skipped,
}

/// Write `content` to `path` unless it already exists and `force` is false —
/// idempotent-safe default: `init` run twice never clobbers an operator's
/// edits without an explicit `--force`. Creates parent directories as needed.
pub(crate) fn scaffold_file(
    path: &Path,
    content: &str,
    force: bool,
) -> anyhow::Result<ScaffoldOutcome> {
    if path.exists() && !force {
        return Ok(ScaffoldOutcome::Skipped);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    std::fs::write(path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(ScaffoldOutcome::Wrote)
}

/// Build the `mcpServers.praxec` entry: the running binary + `serve --config
/// <gateway.yaml>`, both absolute paths (Windows-safe via `current_exe`, which
/// resolves the `.exe` suffix).
pub(crate) fn praxec_mcp_entry(exe: &Path, gateway_yaml: &Path) -> Value {
    json!({
        "command": exe.to_string_lossy(),
        "args": ["serve", "--config", gateway_yaml.to_string_lossy()],
    })
}

/// Merge `entry` into `existing`'s `mcpServers.praxec` key, preserving every
/// other top-level key and every other server entry untouched. `existing` of
/// `None` (or anything that isn't a JSON object) starts from an empty object —
/// a malformed *existing* value is the caller's problem (see
/// [`write_editor_mcp_config`], which refuses to silently discard one).
pub(crate) fn merge_mcp_servers(existing: Option<Value>, entry: Value) -> Value {
    let mut root = match existing {
        Some(Value::Object(m)) => Value::Object(m),
        _ => json!({}),
    };
    let servers = root
        .as_object_mut()
        .expect("root normalized to an object above")
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    if !servers.is_object() {
        *servers = json!({});
    }
    servers
        .as_object_mut()
        .expect("normalized to an object above")
        .insert("praxec".to_string(), entry);
    root
}

/// Outcome of writing one editor's MCP config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorWriteOutcome {
    Created,
    Merged,
}

/// Read `path` (if present, parse as JSON), merge in the `praxec` MCP server
/// entry, and write back pretty-printed JSON. A present-but-invalid-JSON file
/// fails fast rather than being silently overwritten — MERGE means "never
/// clobber", which includes never guessing past malformed input.
pub(crate) fn write_editor_mcp_config(
    path: &Path,
    exe: &Path,
    gateway_yaml: &Path,
) -> anyhow::Result<EditorWriteOutcome> {
    let existing = if path.exists() {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let parsed: Value = serde_json::from_str(&raw).with_context(|| {
            format!(
                "{} exists but is not valid JSON — fix or remove it before running \
                 `praxec init` (an existing file is only ever merged, never overwritten blind)",
                path.display()
            )
        })?;
        Some(parsed)
    } else {
        None
    };
    let outcome = if existing.is_some() {
        EditorWriteOutcome::Merged
    } else {
        EditorWriteOutcome::Created
    };
    let entry = praxec_mcp_entry(exe, gateway_yaml);
    let merged = merge_mcp_servers(existing, entry);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(&merged).context("serializing merged MCP config")?;
    std::fs::write(path, format!("{pretty}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(outcome)
}

/// Which editor(s) to wire, resolved from `--editor` / auto-detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditorTarget {
    Cursor,
    Claude,
}

impl EditorTarget {
    pub(crate) fn label(self) -> &'static str {
        match self {
            EditorTarget::Cursor => "Cursor",
            EditorTarget::Claude => "Claude Code",
        }
    }

    /// Resolve this editor's MCP config path. `global` only affects Cursor
    /// (its user-level config lives at `~/.cursor/mcp.json` on every OS —
    /// Cursor does not use the platform app-data dir for this file); Claude's
    /// is always the project-local file (no global variant requested here).
    pub(crate) fn config_path(self, cwd: &Path, global: bool) -> anyhow::Result<PathBuf> {
        match self {
            EditorTarget::Cursor if global => dirs::home_dir()
                .map(|h| h.join(".cursor").join("mcp.json"))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "cannot locate a home directory for Cursor's global MCP config \
                         (~/.cursor/mcp.json); wire it manually, or omit --global to use the \
                         project-local .cursor/mcp.json instead"
                    )
                }),
            EditorTarget::Cursor => Ok(cwd.join(".cursor").join("mcp.json")),
            EditorTarget::Claude => Ok(cwd.join(".mcp.json")),
        }
    }
}

/// Editor auto-detection + interactive confirm/prompt seam. Injected so the
/// orchestration in [`crate::gateway::init`] is exercised against a fake in
/// tests, never a real TTY or this machine's actual installed editors.
pub(crate) trait InitIo {
    /// Best-effort: is Cursor installed on this machine?
    fn detect_cursor(&self) -> bool;
    /// Best-effort: is Claude Code installed on this machine?
    fn detect_claude(&self) -> bool;
    /// Ask the operator to confirm writing MCP config for the named editor;
    /// only ever called when NOT `--yes` (a non-interactive run never blocks
    /// on this).
    fn confirm(&self, prompt: &str) -> bool;
    /// Prompt for the API key (interactive path only); `None` on a blank
    /// line or EOF (the documented "blank to skip").
    fn prompt_api_key(&self, prompt: &str) -> Option<String>;
    /// Read an env var — a seam so tests never touch the real process env.
    fn read_env(&self, key: &str) -> Option<String>;
}

/// The one production [`InitIo`]: real stdin/stdout + `which`/home-dir
/// presence probes.
pub(crate) struct RealInitIo;

impl InitIo for RealInitIo {
    fn detect_cursor(&self) -> bool {
        which::which("cursor").is_ok()
            || dirs::home_dir().is_some_and(|h| h.join(".cursor").is_dir())
    }

    fn detect_claude(&self) -> bool {
        which::which("claude").is_ok()
            || dirs::home_dir().is_some_and(|h| h.join(".claude").is_dir())
    }

    fn confirm(&self, prompt: &str) -> bool {
        use std::io::Write as _;
        print!("{prompt} [Y/n] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        let line = line.trim().to_ascii_lowercase();
        line.is_empty() || line == "y" || line == "yes"
    }

    fn prompt_api_key(&self, prompt: &str) -> Option<String> {
        use std::io::Write as _;
        print!("{prompt} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        // NEVER log/echo the value itself — only ever assign to `line` here.
        let trimmed = line.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    fn read_env(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── gateway.yaml / models.yaml content ──────────────────────────────────

    #[test]
    fn gateway_yaml_content_embeds_absolute_paths_under_the_target_dir() {
        let dir = Path::new("/tmp/praxec-init-test-xyz");
        let yaml = gateway_yaml_content(dir);
        assert!(yaml.contains("version: \"1.0.0\""));
        assert!(yaml.contains("\"/tmp/praxec-init-test-xyz/models.yaml\""));
        assert!(yaml.contains("\"/tmp/praxec-init-test-xyz/audit-logs\""));
        assert!(yaml.contains("\"/tmp/praxec-init-test-xyz/praxec.db\""));
        assert!(yaml.contains("embeddings: { enabled: false }"));
        assert!(yaml.contains("auto_drive: false"));
        assert!(yaml.contains("sink: file"));
        assert!(yaml.contains("kind: sqlite"));
        // Empty-but-present would still be a config to notice/undo; comment instead.
        assert!(yaml.contains("# repos: []"));
        assert!(yaml.contains("# connections: {}"));
    }

    #[test]
    fn gateway_yaml_content_escapes_windows_style_backslash_paths() {
        // `Path::join` inserts the HOST platform's separator (`/` when this
        // test runs on Linux/macOS, `\` on Windows) — this asserts only the
        // thing `yaml_quoted` is actually responsible for: every backslash
        // already in the input is doubled so the YAML double-quoted scalar
        // round-trips, independent of which separator the join used.
        let dir = Path::new(r"C:\Users\op\AppData\Roaming\praxec");
        let yaml = gateway_yaml_content(dir);
        let escaped_dir = r"C:\\Users\\op\\AppData\\Roaming\\praxec";
        assert!(
            yaml.contains(escaped_dir),
            "expected escaped dir prefix {escaped_dir:?} in:\n{yaml}"
        );
        assert!(yaml.contains("models.yaml"));
    }

    #[test]
    fn models_yaml_content_is_the_commodity_default_chain() {
        let yaml = models_yaml_content();
        assert!(yaml.contains("version: 1"));
        assert!(yaml.contains("z-ai/glm-5.2"));
        assert!(yaml.contains("deepseek/deepseek-v4-pro"));
        assert!(yaml.contains("effort: high"));
        assert!(yaml.contains("anthropic/claude-haiku-4-5"));
        assert!(yaml.contains("openrouter"));
    }

    /// The scaffolded gateway.yaml PARSES and passes the same resolved-config
    /// loader `praxec check` uses, with zero diagnostics errors — the
    /// spec's hard acceptance bar for the scaffolder output.
    #[test]
    fn scaffolded_gateway_yaml_parses_and_passes_config_load() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("praxec");
        std::fs::create_dir_all(&target).unwrap();
        let gateway_path = target.join("gateway.yaml");
        std::fs::write(&gateway_path, gateway_yaml_content(&target)).unwrap();
        std::fs::write(target.join("models.yaml"), models_yaml_content()).unwrap();

        let (config, soft_diagnostics) =
            praxec_core::config::load_resolved_with_repos(&gateway_path)
                .expect("scaffolded gateway.yaml must load cleanly");
        assert_eq!(
            config.pointer("/version").and_then(Value::as_str),
            Some("1.0.0")
        );
        assert!(
            soft_diagnostics.is_empty(),
            "expected zero soft diagnostics, got: {soft_diagnostics:?}"
        );

        let hard_diagnostics = crate::gateway_config::collect_diagnostics_with(&config, &[]);
        let errors: Vec<_> = hard_diagnostics
            .iter()
            .filter(|d| matches!(d, praxec_core::validate::Diagnostic::Error(_)))
            .collect();
        assert!(
            errors.is_empty(),
            "expected zero diagnostic errors, got: {errors:?}"
        );
    }

    // ── idempotent scaffold_file ─────────────────────────────────────────────

    #[test]
    fn scaffold_file_writes_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("gateway.yaml");
        let outcome = scaffold_file(&path, "content-v1", false).unwrap();
        assert_eq!(outcome, ScaffoldOutcome::Wrote);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content-v1");
    }

    #[test]
    fn scaffold_file_skips_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.yaml");
        std::fs::write(&path, "original").unwrap();

        let outcome = scaffold_file(&path, "new-content", false).unwrap();

        assert_eq!(outcome, ScaffoldOutcome::Skipped);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "original",
            "must never overwrite without --force"
        );
    }

    #[test]
    fn scaffold_file_overwrites_existing_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.yaml");
        std::fs::write(&path, "original").unwrap();

        let outcome = scaffold_file(&path, "new-content", true).unwrap();

        assert_eq!(outcome, ScaffoldOutcome::Wrote);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new-content");
    }

    // ── editor MCP config merge ──────────────────────────────────────────────

    #[test]
    fn merge_mcp_servers_creates_from_scratch() {
        let entry = praxec_mcp_entry(Path::new("/usr/bin/praxec"), Path::new("/cfg/gateway.yaml"));
        let merged = merge_mcp_servers(None, entry);
        assert_eq!(
            merged["mcpServers"]["praxec"]["command"],
            json!("/usr/bin/praxec")
        );
        assert_eq!(
            merged["mcpServers"]["praxec"]["args"],
            json!(["serve", "--config", "/cfg/gateway.yaml"])
        );
    }

    /// The load-bearing invariant: merging into a file with an unrelated,
    /// existing MCP server must preserve that server untouched.
    #[test]
    fn merge_mcp_servers_preserves_an_existing_unrelated_server() {
        let existing = json!({
            "mcpServers": {
                "some-other-tool": { "command": "other-cmd", "args": ["--flag"] }
            },
            "unrelatedTopLevelKey": "preserved"
        });
        let entry = praxec_mcp_entry(Path::new("/usr/bin/praxec"), Path::new("/cfg/gateway.yaml"));
        let merged = merge_mcp_servers(Some(existing), entry);

        assert_eq!(
            merged["mcpServers"]["some-other-tool"]["command"],
            json!("other-cmd"),
            "unrelated server must be preserved verbatim"
        );
        assert_eq!(merged["unrelatedTopLevelKey"], json!("preserved"));
        assert_eq!(
            merged["mcpServers"]["praxec"]["command"],
            json!("/usr/bin/praxec")
        );
    }

    #[test]
    fn merge_mcp_servers_replaces_a_prior_praxec_entry() {
        let existing =
            json!({ "mcpServers": { "praxec": { "command": "stale-path", "args": [] } } });
        let entry = praxec_mcp_entry(Path::new("/usr/bin/praxec"), Path::new("/cfg/gateway.yaml"));
        let merged = merge_mcp_servers(Some(existing), entry);
        assert_eq!(
            merged["mcpServers"]["praxec"]["command"],
            json!("/usr/bin/praxec")
        );
    }

    #[test]
    fn write_editor_mcp_config_merges_into_an_existing_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".cursor").join("mcp.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "mcpServers": { "figma": { "command": "figma-mcp", "args": [] } }
            }))
            .unwrap(),
        )
        .unwrap();

        let outcome = write_editor_mcp_config(
            &path,
            Path::new("/usr/bin/praxec"),
            Path::new("/cfg/gateway.yaml"),
        )
        .unwrap();

        assert_eq!(outcome, EditorWriteOutcome::Merged);
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            written["mcpServers"]["figma"]["command"],
            json!("figma-mcp")
        );
        assert_eq!(
            written["mcpServers"]["praxec"]["command"],
            json!("/usr/bin/praxec")
        );
    }

    #[test]
    fn write_editor_mcp_config_creates_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");

        let outcome = write_editor_mcp_config(
            &path,
            Path::new("/usr/bin/praxec"),
            Path::new("/cfg/gateway.yaml"),
        )
        .unwrap();

        assert_eq!(outcome, EditorWriteOutcome::Created);
        assert!(path.exists());
    }

    #[test]
    fn write_editor_mcp_config_fails_fast_on_malformed_existing_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        std::fs::write(&path, "{ not valid json").unwrap();

        let err = write_editor_mcp_config(
            &path,
            Path::new("/usr/bin/praxec"),
            Path::new("/cfg/gateway.yaml"),
        )
        .expect_err("malformed existing JSON must fail fast, never be silently clobbered");
        assert!(err.to_string().contains("not valid JSON"));
        // The original malformed content must be untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not valid json");
    }

    // ── target dir resolution ────────────────────────────────────────────────

    #[test]
    fn resolve_target_dir_honors_explicit_dir() {
        let resolved = resolve_target_dir(Some(PathBuf::from("/explicit/path"))).unwrap();
        assert_eq!(resolved, PathBuf::from("/explicit/path"));
    }

    // ── editor target config paths ──────────────────────────────────────────

    #[test]
    fn cursor_project_scope_is_cwd_dot_cursor() {
        let cwd = Path::new("/repo");
        let path = EditorTarget::Cursor.config_path(cwd, false).unwrap();
        assert_eq!(path, PathBuf::from("/repo/.cursor/mcp.json"));
    }

    #[test]
    fn claude_scope_is_always_cwd_dot_mcp_json_regardless_of_global() {
        let cwd = Path::new("/repo");
        assert_eq!(
            EditorTarget::Claude.config_path(cwd, false).unwrap(),
            PathBuf::from("/repo/.mcp.json")
        );
        assert_eq!(
            EditorTarget::Claude.config_path(cwd, true).unwrap(),
            PathBuf::from("/repo/.mcp.json")
        );
    }

    #[test]
    fn cursor_global_scope_is_under_home_not_cwd() {
        let cwd = Path::new("/repo");
        let path = EditorTarget::Cursor.config_path(cwd, true).unwrap();
        assert!(!path.starts_with(cwd));
        assert!(path.ends_with(".cursor/mcp.json"));
    }

    // ── InitIo-driven orchestration seam (fake, no TTY / no real env) ───────

    struct FakeInitIo {
        cursor: bool,
        claude: bool,
        confirm: bool,
        api_key: Option<String>,
        env: std::collections::BTreeMap<String, String>,
    }

    impl InitIo for FakeInitIo {
        fn detect_cursor(&self) -> bool {
            self.cursor
        }
        fn detect_claude(&self) -> bool {
            self.claude
        }
        fn confirm(&self, _prompt: &str) -> bool {
            self.confirm
        }
        fn prompt_api_key(&self, _prompt: &str) -> Option<String> {
            self.api_key.clone()
        }
        fn read_env(&self, key: &str) -> Option<String> {
            self.env.get(key).cloned()
        }
    }

    #[test]
    fn fake_init_io_reports_only_what_it_is_configured_with() {
        let io = FakeInitIo {
            cursor: true,
            claude: false,
            confirm: true,
            api_key: Some("sk-test-123".to_string()),
            env: std::collections::BTreeMap::new(),
        };
        assert!(io.detect_cursor());
        assert!(!io.detect_claude());
        assert!(io.confirm("anything"));
        assert_eq!(
            io.prompt_api_key("anything"),
            Some("sk-test-123".to_string())
        );
        assert_eq!(io.read_env("OPENROUTER_API_KEY"), None);
    }
}
