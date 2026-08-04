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

/// The scaffolded `gateway.yaml` content. `dir` is the config directory (expected
/// absolute); `project_dir` is the directory `praxec init` was run from — scaffolded
/// as a `writable: true` repo so the FIRST run has a `repo_root` and works with no
/// hand-editing (without it, every real command dies on `REPO_ROOT_REQUIRED`). The
/// repo is `definitions: false` — it is a WRITE TARGET (where agents edit code), not
/// a source of workflow definitions.
pub(crate) fn gateway_yaml_content(dir: &Path, project_dir: &Path) -> String {
    let models = yaml_quoted(&dir.join("models.yaml"));
    let audit = yaml_quoted(&dir.join("audit-logs"));
    let db = yaml_quoted(&dir.join("praxec.db"));
    let project = yaml_quoted(project_dir);
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
repos:
  # The project praxec operates on — a WRITE TARGET for agent edits (not a pack).
  # This is what gives a run its `repo_root`; point it at the repo you want praxec
  # to work in (add more `- path:` entries for more repos, or `praxec sync` for packs).
  - path: {project}
    writable: true
    definitions: false
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

/// The two **OPEN** starter packs `--with-starter-packs` wires under `repos:`,
/// always-latest (`{uri, ref: main}`). Deliberately excludes
/// `cognitive-architectures-max` (premium/paid) and `frontrails` (`include:`-
/// based, deferred to a fast-follow — design §7).
pub(crate) const STARTER_PACK_URIS: [&str; 2] = [
    "git+https://github.com/praxec/cognitive-architectures",
    "git+https://github.com/praxec/praxec-meta",
];

/// The always-latest packs registry `--with-starter-packs` points
/// `discovery.registry` at, as a `{uri, ref: main}` object — NOT hash-pinned
/// (design §5: currency over pinning).
pub(crate) const STARTER_REGISTRY_URI: &str = "git+https://github.com/praxec/packs";

/// The short id of a starter pack `uri` — its final `/`-segment (e.g.
/// `git+https://github.com/praxec/cognitive-architectures` →
/// `cognitive-architectures`). This is the token an operator selects with
/// `init --packs`.
pub(crate) fn starter_pack_short_id(uri: &str) -> &str {
    uri.rsplit('/').next().unwrap_or(uri)
}

/// The valid `--packs` short ids, in `STARTER_PACK_URIS` order — the single
/// source of the known open pack set (never a hand-maintained parallel list).
pub(crate) fn starter_pack_ids() -> Vec<&'static str> {
    STARTER_PACK_URIS
        .iter()
        .map(|u| starter_pack_short_id(u))
        .collect()
}

/// Resolve a comma-separated `--packs` selection of short ids into the matching
/// starter-pack `uri`s (deduped, in first-seen order). Fail-fast (listing the
/// valid ids) on any unknown id — `--packs` is short-id selection from the known
/// open set, NOT an arbitrary-uri channel (that is what `--pack <uri>` is for).
pub(crate) fn resolve_selected_packs(packs_csv: &str) -> anyhow::Result<Vec<String>> {
    let mut selected: Vec<String> = Vec::new();
    for raw in packs_csv.split(',') {
        let id = raw.trim();
        if id.is_empty() {
            continue;
        }
        match STARTER_PACK_URIS
            .iter()
            .find(|u| starter_pack_short_id(u) == id)
        {
            Some(uri) => {
                let uri = (*uri).to_string();
                if !selected.contains(&uri) {
                    selected.push(uri);
                }
            }
            None => {
                anyhow::bail!(
                    "unknown pack id `{id}` in --packs; valid starter pack ids are: {} \
                     (to wire an arbitrary pack uri instead, use --pack <uri>)",
                    starter_pack_ids().join(", ")
                );
            }
        }
    }
    Ok(selected)
}

/// Fold the three pack-selection flags into one [`PackWiring`]:
/// - `--with-starter-packs` → all of [`STARTER_PACK_URIS`] + the registry pointer;
/// - `--packs <ids>` → the selected subset of the known open packs + the registry
///   pointer (so the selected packs' tools resolve);
/// - `--pack <uri>` → one arbitrary uri, no registry pointer of its own.
///
/// Combining any of these unions the pack `uri`s with no duplicates. Fail-fast
/// (listing valid ids) on an unknown `--packs` id.
pub(crate) fn resolve_pack_wiring(
    with_starter_packs: bool,
    packs: Option<&str>,
    pack: Option<&str>,
) -> anyhow::Result<PackWiring> {
    let mut uris: Vec<String> = Vec::new();
    let mut registry = with_starter_packs;
    if with_starter_packs {
        uris.extend(STARTER_PACK_URIS.iter().map(|s| s.to_string()));
    }
    if let Some(csv) = packs {
        let selected = resolve_selected_packs(csv)?;
        if !selected.is_empty() {
            registry = true; // selected packs' tools resolve via the registry
        }
        for uri in selected {
            if !uris.contains(&uri) {
                uris.push(uri);
            }
        }
    }
    if let Some(p) = pack {
        let p = p.to_string();
        if !uris.contains(&p) {
            uris.push(p);
        }
    }
    Ok(PackWiring {
        packs: uris,
        registry,
    })
}

/// What to wire into the scaffolded `gateway.yaml`: a set of pack `uri`s under
/// `repos:` and (for `--with-starter-packs`) the always-latest
/// `discovery.registry` pointer.
#[derive(Debug, Clone, Default)]
pub(crate) struct PackWiring {
    /// Pack `uri`s to union under `repos:` (each becomes `{uri, ref: main}`).
    pub packs: Vec<String>,
    /// Whether to set `discovery.registry` to the always-latest starter registry.
    pub registry: bool,
}

/// Result of merging pack wiring into a gateway.yaml: the new text plus what
/// actually changed (so `init` can report it and a re-run's idempotency is
/// observable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackWiringOutcome {
    /// The re-serialized `gateway.yaml` text.
    pub yaml: String,
    /// Pack `uri`s newly appended this call (absent already-present ones — the
    /// union is idempotent, so a re-run yields an empty list).
    pub added_packs: Vec<String>,
    /// `true` iff `discovery.registry` was (re-)written this call — `false` when
    /// not requested, or preserved because one already existed and `!force`.
    pub registry_wired: bool,
}

/// Merge `wiring` into the `existing` `gateway.yaml` text idempotently:
///
/// - **`repos:`** — union each requested pack `uri` (never duplicate an entry
///   whose `uri` already appears), appending `{uri, ref: main}` for the new
///   ones. An existing `repos:` block is preserved and extended, never
///   clobbered (poka-yoke against losing an operator's hand-added packs).
/// - **`discovery.registry`** — when `wiring.registry`, set it to the
///   always-latest `{uri: praxec/packs, ref: main}` object. An existing
///   `registry` value is PRESERVED unless `force` (matching the gateway/models
///   `--force` semantics).
///
/// Parses + re-serializes through `serde_yaml` so the merge is structural (a
/// real YAML union), not a fragile text splice; comments in the base scaffold
/// are not preserved once packs are wired, which is acceptable — the wired file
/// still loads cleanly (the scaffold's placeholders were comments anyway).
pub(crate) fn merge_pack_wiring(
    existing: &str,
    wiring: &PackWiring,
    force: bool,
) -> anyhow::Result<PackWiringOutcome> {
    use serde_yaml::Value as Yaml;

    let mut root: Yaml =
        serde_yaml::from_str(existing).context("parsing existing gateway.yaml for pack wiring")?;
    let map = root
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("gateway.yaml is not a YAML mapping; cannot wire packs"))?;

    // ── repos: union each requested uri ──────────────────────────────────────
    let mut added_packs = Vec::new();
    if !wiring.packs.is_empty() {
        let repos = map
            .entry(Yaml::from("repos"))
            .or_insert_with(|| Yaml::Sequence(Vec::new()));
        let seq = repos.as_sequence_mut().ok_or_else(|| {
            anyhow::anyhow!(
                "gateway.yaml `repos:` is present but not a sequence; refusing to clobber it"
            )
        })?;
        let present: std::collections::BTreeSet<String> = seq
            .iter()
            .filter_map(|e| e.get("uri").and_then(Yaml::as_str).map(str::to_string))
            .collect();
        for uri in &wiring.packs {
            if present.contains(uri) {
                continue; // idempotent — already wired
            }
            let mut entry = serde_yaml::Mapping::new();
            entry.insert(Yaml::from("uri"), Yaml::from(uri.as_str()));
            entry.insert(Yaml::from("ref"), Yaml::from("main"));
            seq.push(Yaml::Mapping(entry));
            added_packs.push(uri.clone());
        }
    }

    // ── discovery.registry: always-latest {uri, ref} ─────────────────────────
    let mut registry_wired = false;
    if wiring.registry {
        let discovery = map
            .entry(Yaml::from("discovery"))
            .or_insert_with(|| Yaml::Mapping(serde_yaml::Mapping::new()));
        let dmap = discovery.as_mapping_mut().ok_or_else(|| {
            anyhow::anyhow!("gateway.yaml `discovery:` is present but not a mapping")
        })?;
        if dmap.get("registry").is_none() || force {
            let mut reg = serde_yaml::Mapping::new();
            reg.insert(Yaml::from("uri"), Yaml::from(STARTER_REGISTRY_URI));
            reg.insert(Yaml::from("ref"), Yaml::from("main"));
            dmap.insert(Yaml::from("registry"), Yaml::Mapping(reg));
            registry_wired = true;
        }
    }

    let yaml = serde_yaml::to_string(&root).context("serializing wired gateway.yaml")?;
    Ok(PackWiringOutcome {
        yaml,
        added_packs,
        registry_wired,
    })
}

/// The install-consent gate for `init`'s provisioning step: OFFER by default,
/// INSTALL (`Consent::Granted`) only under `--install-tools` or `--yes`.
/// Consent by construction — a plain `init --with-starter-packs` can never
/// mutate the machine (design §3 principle 3).
pub(crate) fn install_consent(install_tools: bool, yes: bool) -> bool {
    install_tools || yes
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
pub(crate) fn praxec_mcp_entry(exe: &Path, gateway_yaml: &Path, providers_env: &Path) -> Value {
    // `env.PRAXEC_PROVIDER_KEYS_FILE` pins the provider-keys file the editor-
    // launched `serve` reads, so an install into a non-default `--dir` is NOT
    // silently keyless at serve time (the runtime resolver otherwise only knows
    // the XDG/legacy home paths). Harmless for a default-dir install (it points at
    // the same file the resolver would find anyway).
    json!({
        "command": exe.to_string_lossy(),
        "args": ["serve", "--config", gateway_yaml.to_string_lossy()],
        "env": { "PRAXEC_PROVIDER_KEYS_FILE": providers_env.to_string_lossy() },
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
    providers_env: &Path,
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
    let entry = praxec_mcp_entry(exe, gateway_yaml, providers_env);
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
        let yaml = gateway_yaml_content(dir, dir);
        assert!(yaml.contains("version: \"1.0.0\""));
        assert!(yaml.contains("\"/tmp/praxec-init-test-xyz/models.yaml\""));
        assert!(yaml.contains("\"/tmp/praxec-init-test-xyz/audit-logs\""));
        assert!(yaml.contains("\"/tmp/praxec-init-test-xyz/praxec.db\""));
        assert!(yaml.contains("embeddings: { enabled: false }"));
        assert!(yaml.contains("auto_drive: false"));
        assert!(yaml.contains("sink: file"));
        assert!(yaml.contains("kind: sqlite"));
        // The project dir is scaffolded as a writable repo so the first run works.
        assert!(yaml.contains("repos:"));
        assert!(yaml.contains("writable: true"));
        assert!(yaml.contains("definitions: false"));
        assert!(
            yaml.contains("\"/tmp/praxec-init-test-xyz\""),
            "the project dir is the repo path:\n{yaml}"
        );
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
        let yaml = gateway_yaml_content(dir, dir);
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
        std::fs::write(&gateway_path, gateway_yaml_content(&target, &target)).unwrap();
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
        let entry = praxec_mcp_entry(
            Path::new("/usr/bin/praxec"),
            Path::new("/cfg/gateway.yaml"),
            Path::new("/cfg/providers.env"),
        );
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
        let entry = praxec_mcp_entry(
            Path::new("/usr/bin/praxec"),
            Path::new("/cfg/gateway.yaml"),
            Path::new("/cfg/providers.env"),
        );
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
        let entry = praxec_mcp_entry(
            Path::new("/usr/bin/praxec"),
            Path::new("/cfg/gateway.yaml"),
            Path::new("/cfg/providers.env"),
        );
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
            Path::new("/cfg/providers.env"),
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
            Path::new("/cfg/providers.env"),
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
            Path::new("/cfg/providers.env"),
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

    // ── pack wiring (Task 5) ─────────────────────────────────────────────────

    /// Parse a merged gateway.yaml back to a `serde_json::Value` for structural
    /// assertions (via the YAML→JSON bridge the loader itself uses).
    fn as_json(yaml: &str) -> Value {
        let y: serde_yaml::Value = serde_yaml::from_str(yaml).expect("merged yaml parses");
        serde_json::to_value(y).expect("yaml→json")
    }

    /// The `uri` of every `repos:` entry in a merged config, in order.
    fn repo_uris(cfg: &Value) -> Vec<String> {
        cfg["repos"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|e| e["uri"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn starter_set_is_exactly_the_two_open_packs_no_max_no_frontrails() {
        // Contract 5 — the premium (`-max`) and `include:`-based (frontrails)
        // packs are deliberately NOT in the starter set.
        assert_eq!(
            STARTER_PACK_URIS,
            [
                "git+https://github.com/praxec/cognitive-architectures",
                "git+https://github.com/praxec/praxec-meta",
            ]
        );
        for uri in STARTER_PACK_URIS {
            assert!(
                !uri.contains("cognitive-architectures-max"),
                "premium -max pack must never be in the starter set: {uri}"
            );
            assert!(
                !uri.contains("frontrails"),
                "include:-based frontrails must never be in the starter set: {uri}"
            );
        }
    }

    #[test]
    fn with_starter_packs_wires_both_packs_and_the_always_latest_registry() {
        // Contract 1 (structure) — both starter packs under `repos:` as
        // `{uri, ref: main}`, and `discovery.registry` as the `{uri, ref: main}`
        // object (NOT a hash pin, NOT a bare string path).
        let base = gateway_yaml_content(Path::new("/tmp/px"), Path::new("/tmp/px"));
        let wiring = PackWiring {
            packs: STARTER_PACK_URIS.iter().map(|s| s.to_string()).collect(),
            registry: true,
        };
        let outcome = merge_pack_wiring(&base, &wiring, false).unwrap();
        let cfg = as_json(&outcome.yaml);

        assert_eq!(
            repo_uris(&cfg),
            [
                "git+https://github.com/praxec/cognitive-architectures",
                "git+https://github.com/praxec/praxec-meta",
            ],
            "both starter packs wired under repos: {}",
            outcome.yaml
        );
        // Only the PACK entries (uri-based) carry a ref; the scaffolded writable
        // project repo is path-based and has none.
        for entry in cfg["repos"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["uri"].is_string())
        {
            assert_eq!(entry["ref"], json!("main"), "always-latest ref: {entry}");
        }
        assert_eq!(
            cfg["discovery"]["registry"]["uri"],
            json!(STARTER_REGISTRY_URI),
            "discovery.registry is the {{uri, ref}} object form: {}",
            outcome.yaml
        );
        assert_eq!(cfg["discovery"]["registry"]["ref"], json!("main"));
        assert!(
            cfg["discovery"]["registry"]["hash"].is_null(),
            "the registry is always-latest, never hash-pinned"
        );
        assert_eq!(outcome.added_packs.len(), 2);
        assert!(outcome.registry_wired);
    }

    #[test]
    fn pack_flag_wires_exactly_that_one_pack() {
        // Contract 2 — `--pack <uri>` alone wires exactly one repo entry and,
        // being pack-only (no --with-starter-packs), no registry pointer.
        let base = gateway_yaml_content(Path::new("/tmp/px"), Path::new("/tmp/px"));
        let wiring = PackWiring {
            packs: vec!["git+https://github.com/acme/private-pack".to_string()],
            registry: false,
        };
        let outcome = merge_pack_wiring(&base, &wiring, false).unwrap();
        let cfg = as_json(&outcome.yaml);

        assert_eq!(
            repo_uris(&cfg),
            ["git+https://github.com/acme/private-pack"],
            "exactly the one requested pack: {}",
            outcome.yaml
        );
        assert_eq!(
            cfg["repos"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["uri"].is_string())
                .unwrap()["ref"],
            json!("main")
        );
        assert!(
            cfg["discovery"].is_null(),
            "pack-only wiring adds no discovery.registry: {}",
            outcome.yaml
        );
        assert!(!outcome.registry_wired);
    }

    #[test]
    fn re_run_merges_without_duplicating_repos_or_clobbering_the_registry() {
        // Contract 3 — a second merge over the first's output is a no-op union:
        // no duplicate repos, the existing registry preserved (not re-written).
        let base = gateway_yaml_content(Path::new("/tmp/px"), Path::new("/tmp/px"));
        let wiring = PackWiring {
            packs: STARTER_PACK_URIS.iter().map(|s| s.to_string()).collect(),
            registry: true,
        };
        let first = merge_pack_wiring(&base, &wiring, false).unwrap();
        let second = merge_pack_wiring(&first.yaml, &wiring, false).unwrap();

        let cfg = as_json(&second.yaml);
        assert_eq!(
            repo_uris(&cfg),
            [
                "git+https://github.com/praxec/cognitive-architectures",
                "git+https://github.com/praxec/praxec-meta",
            ],
            "re-run must not duplicate repos: {}",
            second.yaml
        );
        assert!(
            second.added_packs.is_empty(),
            "nothing newly added on the idempotent re-run"
        );
        assert!(
            !second.registry_wired,
            "an existing registry is preserved, not re-written, without --force"
        );
        // The registry pointer survives the re-run unchanged.
        assert_eq!(
            cfg["discovery"]["registry"]["uri"],
            json!(STARTER_REGISTRY_URI)
        );
    }

    #[test]
    fn pack_wiring_preserves_a_hand_added_repo_entry() {
        // Poka-yoke — an operator's existing `repos:` entry is preserved and the
        // starter packs are unioned in beside it, never clobbering it.
        // A config that already carries an operator's `repos:` entry (a single
        // repos: block — the scaffold's own writable repo is exercised elsewhere).
        let base = "version: \"1.0.0\"\ngateway: { allow_ephemeral: true }\n\
                    repos:\n  - { uri: \"git+https://github.com/acme/mine\", ref: dev }\n"
            .to_string();
        let wiring = PackWiring {
            packs: STARTER_PACK_URIS.iter().map(|s| s.to_string()).collect(),
            registry: false,
        };
        let outcome = merge_pack_wiring(&base, &wiring, false).unwrap();
        let cfg = as_json(&outcome.yaml);
        let uris = repo_uris(&cfg);
        assert!(
            uris.contains(&"git+https://github.com/acme/mine".to_string()),
            "the hand-added repo must be preserved: {}",
            outcome.yaml
        );
        assert_eq!(
            uris.len(),
            3,
            "one existing + two unioned: {}",
            outcome.yaml
        );
        // The operator's non-`main` ref is untouched.
        let mine = cfg["repos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["uri"] == json!("git+https://github.com/acme/mine"))
            .unwrap();
        assert_eq!(mine["ref"], json!("dev"), "existing ref not rewritten");
    }

    #[test]
    fn force_resets_an_existing_registry_pointer() {
        let mut base = gateway_yaml_content(Path::new("/tmp/px"), Path::new("/tmp/px"));
        base.push_str("discovery:\n  registry: /some/local/packs.yaml\n");
        let wiring = PackWiring {
            packs: Vec::new(),
            registry: true,
        };
        // Without force: the existing local-path registry is preserved.
        let kept = merge_pack_wiring(&base, &wiring, false).unwrap();
        assert!(!kept.registry_wired);
        assert_eq!(
            as_json(&kept.yaml)["discovery"]["registry"],
            json!("/some/local/packs.yaml")
        );
        // With force: reset to the always-latest object.
        let reset = merge_pack_wiring(&base, &wiring, true).unwrap();
        assert!(reset.registry_wired);
        assert_eq!(
            as_json(&reset.yaml)["discovery"]["registry"]["uri"],
            json!(STARTER_REGISTRY_URI)
        );
    }

    // ── --packs pack-level selection (Task A3) ───────────────────────────────

    #[test]
    fn starter_pack_short_id_is_the_final_path_segment() {
        // The `--packs` token is the last `/`-segment of each starter uri —
        // derived from STARTER_PACK_URIS, never a hand-maintained parallel list.
        assert_eq!(
            starter_pack_short_id("git+https://github.com/praxec/cognitive-architectures"),
            "cognitive-architectures"
        );
        assert_eq!(
            starter_pack_short_id("git+https://github.com/praxec/praxec-meta"),
            "praxec-meta"
        );
        assert_eq!(
            starter_pack_ids(),
            ["cognitive-architectures", "praxec-meta"]
        );
    }

    #[test]
    fn packs_selects_exactly_one_pack_plus_the_registry_pointer() {
        // Test 1 — `--packs cognitive-architectures` wires exactly that one pack
        // (NOT praxec-meta) and the always-latest registry pointer; the wired
        // YAML has the expected shape.
        let wiring = resolve_pack_wiring(false, Some("cognitive-architectures"), None).unwrap();
        assert_eq!(
            wiring.packs,
            ["git+https://github.com/praxec/cognitive-architectures"],
            "exactly the one selected pack, not praxec-meta"
        );
        assert!(wiring.registry, "selected packs point discovery.registry");

        let base = gateway_yaml_content(Path::new("/tmp/px"), Path::new("/tmp/px"));
        let outcome = merge_pack_wiring(&base, &wiring, false).unwrap();
        let cfg = as_json(&outcome.yaml);
        assert_eq!(
            repo_uris(&cfg),
            ["git+https://github.com/praxec/cognitive-architectures"],
            "scaffolded config wires only the selected pack: {}",
            outcome.yaml
        );
        assert_eq!(
            cfg["repos"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["uri"].is_string())
                .unwrap()["ref"],
            json!("main")
        );
        assert_eq!(
            cfg["discovery"]["registry"]["uri"],
            json!(STARTER_REGISTRY_URI),
            "the selected pack's tools resolve via the always-latest registry: {}",
            outcome.yaml
        );
        assert_eq!(cfg["discovery"]["registry"]["ref"], json!("main"));
    }

    #[test]
    fn packs_both_ids_equals_with_starter_packs_no_duplicates() {
        // Test 2 — `--packs cognitive-architectures,praxec-meta` wires both, no
        // duplicates, and is identical to `--with-starter-packs`.
        let by_ids =
            resolve_pack_wiring(false, Some("cognitive-architectures,praxec-meta"), None).unwrap();
        let by_all = resolve_pack_wiring(true, None, None).unwrap();
        assert_eq!(
            by_ids.packs,
            STARTER_PACK_URIS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "both selected packs, in order"
        );
        assert_eq!(
            by_ids.packs, by_all.packs,
            "--packs a,b equals --with-starter-packs"
        );
        assert!(by_ids.registry && by_all.registry);
        // A repeated id is deduped by resolve_selected_packs.
        let dup =
            resolve_selected_packs("cognitive-architectures,cognitive-architectures").unwrap();
        assert_eq!(dup.len(), 1, "a repeated id is not duplicated");
    }

    #[test]
    fn packs_unknown_id_fails_fast_listing_valid_ids_and_wires_nothing() {
        // Test 3 — an unknown id is rejected (naming the valid ids); the wiring
        // is never produced, so nothing is scaffolded/wired downstream.
        let err = resolve_pack_wiring(false, Some("bogus"), None)
            .expect_err("an unknown pack id must fail fast");
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "names the offending id: {msg}");
        assert!(
            msg.contains("cognitive-architectures") && msg.contains("praxec-meta"),
            "lists the valid ids: {msg}"
        );
        // A valid id mixed with a bogus one still fails (no partial wiring).
        assert!(resolve_selected_packs("cognitive-architectures,bogus").is_err());
    }

    #[test]
    fn packs_unions_with_with_starter_packs_without_duplicates() {
        // Test 4 — `--packs` + `--with-starter-packs` union to exactly the
        // starter set, with no duplicate `repos:` entries.
        let wiring =
            resolve_pack_wiring(true, Some("cognitive-architectures,praxec-meta"), None).unwrap();
        assert_eq!(
            wiring.packs,
            STARTER_PACK_URIS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
            "the union is the starter set, no duplicates"
        );
        assert!(wiring.registry);

        let base = gateway_yaml_content(Path::new("/tmp/px"), Path::new("/tmp/px"));
        let outcome = merge_pack_wiring(&base, &wiring, false).unwrap();
        assert_eq!(
            repo_uris(&as_json(&outcome.yaml)),
            [
                "git+https://github.com/praxec/cognitive-architectures",
                "git+https://github.com/praxec/praxec-meta",
            ],
            "no duplicate repos entries: {}",
            outcome.yaml
        );
    }

    #[test]
    fn packs_unions_with_arbitrary_pack_uri_and_keeps_registry() {
        // `--packs` (known open subset) composes with `--pack <uri>` (arbitrary),
        // deduped, and still points the registry (from the --packs selection).
        let wiring = resolve_pack_wiring(
            false,
            Some("cognitive-architectures"),
            Some("git+https://github.com/acme/private-pack"),
        )
        .unwrap();
        assert_eq!(
            wiring.packs,
            [
                "git+https://github.com/praxec/cognitive-architectures",
                "git+https://github.com/acme/private-pack",
            ]
        );
        assert!(wiring.registry);
    }

    #[test]
    fn install_consent_is_offer_only_unless_install_tools_or_yes() {
        // Contract 4 (consent mapping) — default is offer-only; `--install-tools`
        // or `--yes` grants install consent.
        assert!(!install_consent(false, false), "default → offer-only");
        assert!(install_consent(true, false), "--install-tools → install");
        assert!(install_consent(false, true), "--yes → install");
        assert!(install_consent(true, true));
    }

    // ── full resolve — the wired config parses AND loads (mocked transport) ───

    fn git(args: &[&str], cwd: &Path) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Build a bare git repo at `bare_dir` whose single `main` commit carries
    /// `files` (relative path → contents). Returns the `file://` URL — the exact
    /// shape `repo_git::clone_url` yields once a `git+https://` uri is redirected
    /// (mocks the git *transport*, never skips the resolve).
    fn build_bare(bare_dir: &Path, files: &[(&str, &str)]) -> String {
        let seed = tempfile::tempdir().unwrap();
        for (rel, body) in files {
            let p = seed.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
        }
        git(&["init", "--quiet", "-b", "main"], seed.path());
        git(&["add", "-A"], seed.path());
        git(
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--quiet",
                "-m",
                "seed",
            ],
            seed.path(),
        );
        let out = std::process::Command::new("git")
            .args([
                "clone",
                "--quiet",
                "--bare",
                &seed.path().display().to_string(),
                &bare_dir.display().to_string(),
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "bare clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        format!("file://{}", bare_dir.display())
    }

    /// Contract 1 (load) — the `--with-starter-packs` wired config resolves fully
    /// through the SAME `load_resolved_with_repos` path `praxec check` uses:
    /// both packs' workflows AND the always-latest registry's tools land. The git
    /// transport is local `file://` bare repos (mocked, never skipped) — the FULL
    /// resolve runs. Mirrors `remote_example_validates` + Task 1's `registry_source`.
    #[test]
    fn with_starter_packs_scaffolded_config_parses_and_loads() {
        let host = tempfile::tempdir().unwrap();
        let target = host.path().join("praxec");
        std::fs::create_dir_all(&target).unwrap();

        // Bare repos standing in for the three praxec.github remotes.
        let cog_bare = host.path().join("cog.git");
        let meta_bare = host.path().join("meta.git");
        let packs_bare = host.path().join("packs.git");
        let cog_uri = build_bare(
            &cog_bare,
            &[
                (
                    "praxec.repo.yaml",
                    "schema: praxec.repo/v1\nname: cog\nnamespace: cognitive\nversion: 0.0.1\n",
                ),
                (
                    "flows/flow.hello.yaml",
                    "workflows:\n  flow.hello:\n    title: Hello\n    description: trivial\n    initialState: ready\n    states:\n      ready:\n        terminal: true\n",
                ),
            ],
        );
        let meta_uri = build_bare(
            &meta_bare,
            &[
                (
                    "praxec.repo.yaml",
                    "schema: praxec.repo/v1\nname: meta\nnamespace: meta\nversion: 0.0.1\n",
                ),
                (
                    "flows/flow.hello.yaml",
                    "workflows:\n  flow.hello:\n    title: Hello\n    description: trivial\n    initialState: ready\n    states:\n      ready:\n        terminal: true\n",
                ),
            ],
        );
        let packs_uri = build_bare(
            &packs_bare,
            &[(
                "packs.yaml",
                "schema: praxec.packs/v3\ntools:\n  - id: ripgrep\n    name: ripgrep\n    description: Fast search.\n    command: rg\n    version: 14.1.0\n",
            )],
        );

        // Scaffold + wire, then redirect the three `git+https://…praxec/…` uris to
        // the local bare repos (the transport mock).
        let base = gateway_yaml_content(&target, &target);
        let wiring = PackWiring {
            packs: STARTER_PACK_URIS.iter().map(|s| s.to_string()).collect(),
            registry: true,
        };
        let wired = merge_pack_wiring(&base, &wiring, false).unwrap().yaml;
        let redirected = wired
            .replace(
                "git+https://github.com/praxec/cognitive-architectures",
                &cog_uri,
            )
            .replace("git+https://github.com/praxec/praxec-meta", &meta_uri)
            .replace("git+https://github.com/praxec/packs", &packs_uri);

        let gateway_path = target.join("gateway.yaml");
        std::fs::write(&gateway_path, &redirected).unwrap();
        std::fs::write(target.join("models.yaml"), models_yaml_content()).unwrap();

        let (config, soft) = praxec_core::config::load_resolved_with_repos(&gateway_path)
            .expect("the wired, transport-mocked gateway.yaml must load cleanly");
        assert!(
            soft.is_empty(),
            "expected zero soft diagnostics, got: {soft:?}"
        );

        // The packs resolved: both namespaced workflows are present.
        let workflows = config["workflows"].as_object().expect("workflows object");
        assert!(
            workflows.contains_key("cognitive/flow.hello"),
            "cognitive-architectures pack resolved: {:?}",
            workflows.keys().collect::<Vec<_>>()
        );
        assert!(
            workflows.contains_key("meta/flow.hello"),
            "praxec-meta pack resolved: {:?}",
            workflows.keys().collect::<Vec<_>>()
        );

        // The always-latest registry resolved: the object was rewritten to the
        // clone's on-disk `packs.yaml` path, which loads.
        let reg_path = config["discovery"]["registry"]
            .as_str()
            .expect("discovery.registry resolved to a path string");
        assert!(
            reg_path.ends_with("packs.yaml"),
            "registry resolved to the clone's packs.yaml: {reg_path}"
        );
        praxec_core::registry_v3::Registry::load_path(Path::new(reg_path))
            .expect("the always-latest-sourced registry loads");
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
