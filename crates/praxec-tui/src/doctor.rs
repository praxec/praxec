//! `px doctor` — pre-flight checks for `px walk`.
//!
//! Each check returns a structured `CheckResult` so callers (the CLI
//! subcommand, tests) can format output and assert on specific failures.
//!
//! Contract (SPEC §29 / Tranche 3): if `doctor` passes, `walk` will at
//! least START successfully. Doctor does NOT claim walk will SUCCEED
//! (that depends on the model). Each check ties to a specific failure
//! mode `walk` would surface less clearly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

use crate::doctor_probe_cache::{
    self, ProbeCache, ProbeStatus, default_cache_path, read_cache, refresh_cache, write_cache,
};
use crate::praxec_mcp::find_praxec_binary;
use praxec_core::model_resolver::{ConfigSource, ModelRef, ModelsFile, Resolver};

/// `models.yaml` is flagged stale this many days after the last
/// recorded probe. 7 days is loose enough to not nag operators but
/// tight enough to catch model deprecations within a typical sprint.
pub const PROBE_STALE_AFTER_DAYS: u64 = 7;

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    Pass,
    Warn(String), // non-blocking advisory; identifier like LEXICON_PENDING_DEFINITIONS
    Fail(String), // identifier like MCP_PRAXEC_NOT_FOUND for assertions
    Skip(String), // not applicable (e.g. workflow not specified)
}

impl CheckResult {
    fn pass(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }
    fn warn(name: impl Into<String>, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Warn(code.into()),
            detail: detail.into(),
        }
    }
    fn fail(name: impl Into<String>, code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail(code.into()),
            detail: detail.into(),
        }
    }
    fn skip(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Skip(reason.into()),
            detail: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DoctorArgs {
    pub config: Option<String>,
    pub workflow: Option<String>,
    pub agents: Vec<String>,
    /// When true, re-probe every binding in models.yaml against its
    /// provider's `/v1/models` endpoint, write the result to
    /// `~/.praxec/agents-last-probe.json`, and emit per-binding
    /// `CheckResult`s. When false, doctor only reads the cache to
    /// surface stale-since-N-days warnings (cheap, no I/O).
    pub refresh_agents: bool,
}

/// Run all pre-flight checks in order. Returns the per-check results.
/// Caller decides exit code based on whether any `Fail` is present.
pub async fn run_doctor(args: &DoctorArgs) -> Vec<CheckResult> {
    let mut results = Vec::new();

    // 1. praxec binary discoverable
    match find_praxec_binary() {
        Ok(path) => results.push(CheckResult::pass("praxec binary", path)),
        Err(e) => results.push(CheckResult::fail(
            "praxec binary",
            "MCP_PRAXEC_NOT_FOUND",
            format!("{e} — install with `cargo install praxec` or set MCP_PRAXEC_PATH"),
        )),
    }

    // 2. Config file
    let config_path = args
        .config
        .clone()
        .or_else(|| std::env::var("PRAXEC_CONFIG").ok());
    let resolved_config: Option<Value> = match &config_path {
        None => {
            results.push(CheckResult::skip(
                "config file",
                "no --config or PRAXEC_CONFIG; checks 3-5 will be skipped",
            ));
            None
        }
        Some(p) => {
            let path = Path::new(p);
            if !path.exists() {
                results.push(CheckResult::fail(
                    "config file",
                    "CONFIG_NOT_FOUND",
                    format!("{p} does not exist"),
                ));
                None
            } else {
                results.push(CheckResult::pass("config file", p));
                // 3. Config parses + resolves
                match resolve_config(path) {
                    Ok(cfg) => {
                        let n_workflows = cfg
                            .pointer("/workflows")
                            .and_then(Value::as_object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let n_skills = cfg
                            .pointer("/skills")
                            .and_then(Value::as_object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        let n_scripts = cfg
                            .pointer("/scripts")
                            .and_then(Value::as_object)
                            .map(|m| m.len())
                            .unwrap_or(0);
                        results.push(CheckResult::pass(
                            "config parses + resolves",
                            format!(
                                "{n_workflows} workflows, {n_skills} skills, {n_scripts} scripts"
                            ),
                        ));
                        Some(cfg)
                    }
                    Err(e) => {
                        results.push(CheckResult::fail(
                            "config parses + resolves",
                            "CONFIG_INVALID",
                            e.to_string(),
                        ));
                        None
                    }
                }
            }
        }
    };

    // 4. Workflow declared
    if let Some(cfg) = &resolved_config {
        if let Some(wf_name) = &args.workflow {
            if cfg.pointer(&format!("/workflows/{wf_name}")).is_some() {
                results.push(CheckResult::pass("workflow declared", wf_name));
            } else {
                let available: Vec<&str> = cfg
                    .pointer("/workflows")
                    .and_then(Value::as_object)
                    .map(|m| m.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                results.push(CheckResult::fail(
                    "workflow declared",
                    "WORKFLOW_NOT_DECLARED",
                    format!("--workflow '{wf_name}' not found in config. Available: {available:?}"),
                ));
            }
        } else {
            results.push(CheckResult::skip(
                "workflow declared",
                "no --workflow argument",
            ));
        }
    }

    // 5. Per-agent API key present (parse `name=provider/model`)
    if args.agents.is_empty() {
        results.push(CheckResult::skip("agent API keys", "no --agent arguments"));
    } else {
        for spec in &args.agents {
            let parts: Vec<&str> = spec.splitn(2, '=').collect();
            let Some(name) = parts.first() else { continue };
            let Some(rest) = parts.get(1) else {
                results.push(CheckResult::fail(
                    format!("agent: {spec}"),
                    "AGENT_SPEC_INVALID",
                    format!("expected `name=provider/model`, got '{spec}'"),
                ));
                continue;
            };
            let prov_model: Vec<&str> = rest.splitn(2, '/').collect();
            let Some(provider) = prov_model.first().filter(|s| !s.is_empty()) else {
                results.push(CheckResult::fail(
                    format!("agent: {name}"),
                    "AGENT_SPEC_INVALID",
                    format!("missing provider in '{spec}'"),
                ));
                continue;
            };
            let env_var = provider_env_var(provider);
            if std::env::var(env_var).is_ok() {
                results.push(CheckResult::pass(
                    format!("agent: {name}"),
                    format!("{env_var} set"),
                ));
            } else {
                results.push(CheckResult::fail(
                    format!("agent: {name}"),
                    "MISSING_API_KEY",
                    format!("provider '{provider}' needs {env_var}"),
                ));
            }
        }
    }

    // 7. models.yaml (v0.3+) presence + parse. Both project and user
    //    files are reported; mutual-presence is flagged as a shadow.
    let resolver = check_models_yaml(&mut results);

    // 7b. Live-probe cache freshness OR refresh, per --refresh-agents.
    if let Some(r) = &resolver {
        check_agents_probe_cache(&mut results, r.file(), args.refresh_agents).await;
    }

    // 8. Workflow delegates ↔ resolver coverage. For each `delegate:`
    //    string in the resolved workflow, run the resolver's walk and
    //    report the chosen level. Names every delegate whose only
    //    match is `default` (operator-visible "silent downgrade" list).
    if let (Some(r), Some(cfg), Some(wf_name)) = (&resolver, &resolved_config, &args.workflow) {
        check_workflow_delegate_coverage(&mut results, r, cfg, wf_name);
    }

    // 6. Script URIs (file:// only — https / git+https are load-time fetched)
    if let Some(cfg) = &resolved_config
        && let Some(scripts) = cfg.pointer("/scripts").and_then(Value::as_object)
    {
        let mut missing: Vec<String> = Vec::new();
        for (subject, decl) in scripts {
            let Some(uri) = decl.get("uri").and_then(Value::as_str) else {
                continue; // inline body, nothing to check
            };
            if let Some(path) = uri.strip_prefix("file://")
                && !Path::new(path).exists()
            {
                missing.push(format!("{subject}: {uri}"));
            }
        }
        if missing.is_empty() {
            results.push(CheckResult::pass(
                "script file:// URIs",
                format!("{} script(s) verified", scripts.len()),
            ));
        } else {
            results.push(CheckResult::fail(
                "script file:// URIs",
                "SCRIPT_URI_MISSING",
                format!("missing files: {}", missing.join(", ")),
            ));
        }
    }

    // 9. SPEC §30.10.3 — lexicon coverage: surface PENDING_DEFINITION subjects.
    //    Informational: we warn (Warn) so operators can add definitions, but
    //    we do NOT change exit behaviour here. Runtime blocking (Task 3.3) is
    //    separate.
    if let Some(cfg) = &resolved_config {
        check_lexicon_coverage(&mut results, cfg);
    }

    results
}

/// SPEC §30.10.3 — scan the resolved config's workflow lexicon snapshots for
/// `PENDING_DEFINITION` placeholder entries. Each pending subject means the
/// operator references it in scripts/skills but has not yet added a
/// `lexicon: { <subject>: { definition_short: "..." } }` entry.
///
/// Reports a `LEXICON_PENDING_DEFINITIONS` warning (Warn) listing each pending
/// subject. Exit behaviour is NOT affected — doctor uses this as a soft signal;
/// Task 3.3 handles runtime blocking.
///
/// Identifies workflows whose reachable surface includes a pending subject by
/// checking each workflow's `_lexiconLibrary`.
fn check_lexicon_coverage(results: &mut Vec<CheckResult>, cfg: &Value) {
    let pending = praxec_core::lexicon::pending_subjects_from_resolved(cfg);
    if pending.is_empty() {
        // Count how many authored lexicon entries exist.
        let n_authored = cfg
            .pointer("/lexicon")
            .and_then(Value::as_object)
            .map(|m| m.len())
            .unwrap_or(0);
        results.push(CheckResult::pass(
            "lexicon coverage",
            format!("{n_authored} authored lexicon entry/entries, 0 pending"),
        ));
        return;
    }

    // Identify which workflows are "blocked" (have at least one pending subject
    // in their _lexiconLibrary snapshot). At this task we report them
    // informally; Task 3.3 will enforce blocking.
    let mut blocked_workflows: Vec<String> = Vec::new();
    if let Some(workflows) = cfg.pointer("/workflows").and_then(Value::as_object) {
        for (wf_id, def) in workflows {
            if let Some(lib) = def.get("_lexiconLibrary").and_then(Value::as_object) {
                let has_pending = lib.iter().any(|(_, entry)| {
                    entry.get("state").and_then(Value::as_str) == Some("PENDING_DEFINITION")
                });
                if has_pending {
                    blocked_workflows.push(wf_id.clone());
                }
            }
        }
    }
    blocked_workflows.sort();

    let detail = format!(
        "{} pending subject(s): {}{}",
        pending.len(),
        pending.join(", "),
        if blocked_workflows.is_empty() {
            String::new()
        } else {
            format!(
                " — would block workflow(s): {}",
                blocked_workflows.join(", ")
            )
        }
    );

    results.push(CheckResult::warn(
        "lexicon coverage",
        "LEXICON_PENDING_DEFINITIONS",
        detail,
    ));
}

fn resolve_config(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)?;
    let value: Value = serde_yaml::from_str(&raw)?;
    let resolved = praxec_core::config::resolve(value)?;
    Ok(resolved)
}

/// Load models.yaml (project then user) and add CheckResults describing
/// presence, parse status, and shadowing. Returns the project resolver
/// when one is loadable, so the caller can run the delegate-coverage
/// check on the SAME resolver the operator's `px walk` will use.
fn check_models_yaml(results: &mut Vec<CheckResult>) -> Option<Resolver> {
    let project_path = std::path::Path::new(".praxec").join("models.yaml");
    let user_path = dirs::home_dir().map(|d| d.join(".praxec").join("models.yaml"));

    let project_present = project_path.exists();
    let user_present = user_path.as_ref().is_some_and(|p| p.exists());

    if !project_present && !user_present {
        results.push(CheckResult::skip(
            "models.yaml",
            "no project (.praxec/models.yaml) or user (~/.praxec/models.yaml) file",
        ));
        return None;
    }

    // Load whichever takes precedence (project first, then user).
    let (chosen_path, chosen_source, shadowed_path) = if project_present {
        let shadow = if user_present {
            user_path.clone()
        } else {
            None
        };
        (
            project_path.clone(),
            ConfigSource::Project(project_path.clone()),
            shadow,
        )
    } else {
        // Invariant: the early `!project_present && !user_present` branch
        // returned above, so reaching the else here implies user_present
        // is true, which in turn implies user_path is Some.
        let p = user_path
            .clone()
            .expect("invariant: user_present checked at function head");
        (p.clone(), ConfigSource::User(p), None)
    };

    match ModelsFile::from_path(&chosen_path) {
        Ok(file) => {
            results.push(CheckResult::pass(
                "models.yaml",
                format!(
                    "loaded {} ({} default binding(s), {} override(s)){}",
                    chosen_path.display(),
                    file.default.len(),
                    file.overrides.len(),
                    if file.strict_specificity {
                        ", strict_specificity=true"
                    } else {
                        ""
                    },
                ),
            ));
            if let Some(s) = shadowed_path {
                results.push(CheckResult::pass(
                    "models.yaml shadow",
                    format!(
                        "project ({}) shadows user ({}) — user's bindings are NOT in effect",
                        chosen_path.display(),
                        s.display()
                    ),
                ));
            }
            Some(Resolver::from_loaded(file, chosen_source))
        }
        Err(e) => {
            results.push(CheckResult::fail(
                "models.yaml",
                "AGENTS_YAML_PARSE_FAILED",
                format!("{}: {e}", chosen_path.display()),
            ));
            None
        }
    }
}

/// Walk the resolved config's workflow definition for `delegate:`
/// strings, run each through the resolver, and emit one CheckResult
/// per delegate that names the specificity level chosen.
fn check_workflow_delegate_coverage(
    results: &mut Vec<CheckResult>,
    resolver: &Resolver,
    cfg: &Value,
    wf_name: &str,
) {
    let states = cfg
        .pointer(&format!("/workflows/{wf_name}/states"))
        .and_then(Value::as_object);
    let Some(states) = states else {
        results.push(CheckResult::skip(
            "workflow delegates",
            format!("workflow '{wf_name}' has no `states:` map"),
        ));
        return;
    };

    let mut delegates: Vec<(String, String)> = Vec::new(); // (state, delegate string)
    for (state_name, state_val) in states {
        if let Some(d) = state_val.get("delegate").and_then(Value::as_str) {
            delegates.push((state_name.clone(), d.to_string()));
        }
    }

    if delegates.is_empty() {
        results.push(CheckResult::skip(
            "workflow delegates",
            format!("workflow '{wf_name}' has no `delegate:` states"),
        ));
        return;
    }

    let mut downgrades: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (state, delegate_str) in &delegates {
        match ModelRef::parse(delegate_str) {
            Err(e) => {
                errors.push(format!("{state}: '{delegate_str}' ({e})"));
            }
            Ok(d) => match resolver.walk(&d) {
                Err(e) => {
                    errors.push(format!("{state}: '{delegate_str}' → exhausted ({e})"));
                }
                Ok((_bindings, level)) => {
                    if level == "default" {
                        downgrades.push(format!("{state}: '{delegate_str}' → default"));
                    }
                }
            },
        }
    }

    if !errors.is_empty() {
        results.push(CheckResult::fail(
            "workflow delegates",
            "WORKFLOW_DELEGATE_UNRESOLVED",
            format!("{} unresolvable: {}", errors.len(), errors.join("; ")),
        ));
    } else if !downgrades.is_empty() {
        // Soft signal: the walk succeeded but matched a less-specific
        // level than the delegate asked for. Op may have intended this;
        // we just surface it so they can verify (FMECA U1 detection).
        results.push(CheckResult::pass(
            "workflow delegates",
            format!(
                "{} delegate(s) resolved; {} fell through to default — verify intent: {}",
                delegates.len(),
                downgrades.len(),
                downgrades.join("; ")
            ),
        ));
    } else {
        results.push(CheckResult::pass(
            "workflow delegates",
            format!(
                "{} delegate(s) resolved to explicit overrides",
                delegates.len()
            ),
        ));
    }
}

/// Cache freshness check + optional refresh. When `refresh` is false,
/// reads the cache only and emits one `CheckResult` per (provider,
/// model) describing the stored status and how stale it is. When
/// `refresh` is true, re-probes every binding in `file`, writes the
/// fresh cache, and emits the updated results.
///
/// Cache missing / corrupt → emits a `Skip` with "run doctor
/// --refresh-agents to populate". Stale (>`PROBE_STALE_AFTER_DAYS`)
/// → the per-binding entry's status becomes a `Fail` so the operator
/// sees the staleness alongside the cached verdict.
async fn check_agents_probe_cache(
    results: &mut Vec<CheckResult>,
    file: &ModelsFile,
    refresh: bool,
) {
    let Some(cache_path) = default_cache_path() else {
        results.push(CheckResult::skip(
            "models.yaml live-probe cache",
            "no platform cache directory",
        ));
        return;
    };

    let cache = if refresh {
        let fresh = refresh_cache(file).await;
        if let Err(e) = write_cache(&fresh, &cache_path) {
            results.push(CheckResult::fail(
                "models.yaml live-probe cache",
                "CACHE_WRITE_FAILED",
                format!("failed to write {}: {e}", cache_path.display()),
            ));
            return;
        }
        results.push(CheckResult::pass(
            "models.yaml live-probe cache",
            format!(
                "refreshed ({} entries) at {}",
                fresh.entries.len(),
                cache_path.display()
            ),
        ));
        fresh
    } else {
        match read_cache(&cache_path) {
            Ok(Some(c)) => c,
            Ok(None) => {
                results.push(CheckResult::skip(
                    "models.yaml live-probe cache",
                    "no cache yet (run `px doctor --refresh-agents` to populate)",
                ));
                return;
            }
            Err(e) => {
                results.push(CheckResult::fail(
                    "models.yaml live-probe cache",
                    "CACHE_READ_FAILED",
                    format!("read {} failed: {e}", cache_path.display()),
                ));
                return;
            }
        }
    };

    // Surface cache age as its own check so the operator sees one
    // line summarizing "your cache is N days old."
    let stale_threshold = std::time::Duration::from_secs(PROBE_STALE_AFTER_DAYS * 24 * 60 * 60);
    match cache.age() {
        Some(age) if age > stale_threshold => {
            results.push(CheckResult::fail(
                "models.yaml probe age",
                "CACHE_STALE",
                format!(
                    "{} days since last probe (>{}d threshold). Re-run \
                     `px doctor --refresh-agents` to verify bindings.",
                    age.as_secs() / 86_400,
                    PROBE_STALE_AFTER_DAYS,
                ),
            ));
        }
        Some(age) => {
            results.push(CheckResult::pass(
                "models.yaml probe age",
                format!(
                    "{}h since last probe (under {}d threshold)",
                    age.as_secs() / 3600,
                    PROBE_STALE_AFTER_DAYS,
                ),
            ));
        }
        None => {} // empty cache already surfaced above
    }

    // Per-binding entries from the cache.
    for entry in &cache.entries {
        let name = format!("probe: {}/{}", entry.provider, entry.model);
        match &entry.status {
            ProbeStatus::Ok => {
                results.push(CheckResult::pass(name, entry.detail.clone()));
            }
            ProbeStatus::Skipped => {
                results.push(CheckResult::skip(name, entry.detail.clone()));
            }
            ProbeStatus::NoCredential => {
                results.push(CheckResult::fail(
                    name,
                    "PROBE_NO_CREDENTIAL",
                    entry.detail.clone(),
                ));
            }
            ProbeStatus::AuthFailed => {
                results.push(CheckResult::fail(
                    name,
                    "PROBE_AUTH_FAILED",
                    entry.detail.clone(),
                ));
            }
            ProbeStatus::ModelNotListed => {
                results.push(CheckResult::fail(
                    name,
                    "PROBE_MODEL_NOT_LISTED",
                    entry.detail.clone(),
                ));
            }
            ProbeStatus::Unreachable => {
                results.push(CheckResult::fail(
                    name,
                    "PROBE_UNREACHABLE",
                    entry.detail.clone(),
                ));
            }
            ProbeStatus::UnexpectedResponse => {
                results.push(CheckResult::fail(
                    name,
                    "PROBE_UNEXPECTED_RESPONSE",
                    entry.detail.clone(),
                ));
            }
        }
    }

    // Touch the unused import warning when refresh is false (the
    // `doctor_probe_cache::ProbeCache` type is reachable via the
    // `Cache` re-export; this no-op silences unused-import in the
    // refresh=false code path).
    let _ = std::any::type_name::<ProbeCache>();
    let _ = std::any::type_name::<doctor_probe_cache::BindingProbeRecord>();
}

fn provider_env_var(provider: &str) -> &'static str {
    // Single source of truth: core's `api_key_env_for_slug` (CMP-005 /
    // CMP-026). Local-only providers without an API key fall back to
    // their host var; truly unknown providers get the sentinel.
    if let Some(env) = praxec_core::model_resolver::api_key_env_for_slug(provider) {
        return env;
    }
    match provider {
        "ollama" => "OLLAMA_HOST", // local; not actually a key but the host needs to be set
        _ => "(unknown_provider_env_var)",
    }
}

/// VER-005 — strip ASCII/Unicode control bytes (ESC/CSI/OSC introducers, C0,
/// C1, DEL) from strings that originate in untrusted provider responses or
/// model ids before they reach a TTY, so a hostile value (e.g. a crafted model
/// id or `/v1/models` error body) cannot inject terminal escape sequences into
/// the operator's terminal.
fn sanitize_terminal(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Render results as human-readable text. ANSI color when stdout is a TTY.
pub fn render_results(results: &[CheckResult]) -> String {
    let use_color = atty_stdout();
    let mut out = String::new();
    let mut failed = 0;
    let mut warned = 0;
    for r in results {
        // The check `name`/`detail` can carry provider- or model-supplied text;
        // neutralize terminal escapes before they hit the TTY.
        let name = sanitize_terminal(&r.name);
        let detail = sanitize_terminal(&r.detail);
        let (mark, color) = match &r.status {
            CheckStatus::Pass => ("✓", "\x1b[32m"),
            CheckStatus::Warn(_) => {
                warned += 1;
                ("⚠", "\x1b[33m")
            }
            CheckStatus::Fail(_) => {
                failed += 1;
                ("✗", "\x1b[31m")
            }
            CheckStatus::Skip(_) => ("-", "\x1b[90m"),
        };
        let reset = if use_color { "\x1b[0m" } else { "" };
        let color = if use_color { color } else { "" };
        let prefix = format!("{color}{mark}{reset}");
        match &r.status {
            CheckStatus::Pass => {
                out.push_str(&format!("  {prefix} {name:<35} {detail}\n"));
            }
            CheckStatus::Warn(code) => {
                out.push_str(&format!("  {prefix} {name:<35} {code}: {detail}\n"));
            }
            CheckStatus::Fail(code) => {
                out.push_str(&format!("  {prefix} {name:<35} {code}: {detail}\n"));
            }
            CheckStatus::Skip(reason) => {
                let reason = sanitize_terminal(reason);
                out.push_str(&format!("  {prefix} {name:<35} (skipped: {reason})\n"));
            }
        }
    }
    out.push('\n');
    if failed == 0 && warned == 0 {
        out.push_str("doctor: all checks passed.\n");
    } else if failed == 0 {
        out.push_str(&format!(
            "doctor: {warned} advisory warning(s). Run `px walk` at your own risk.\n"
        ));
    } else {
        out.push_str(&format!(
            "doctor: {failed} check(s) failed. Resolve the above before running `px walk`.\n"
        ));
    }
    out
}

fn atty_stdout() -> bool {
    // Cheap TTY detection without an extra crate dep. `isatty(1)`
    // returns non-zero on a TTY.
    use std::os::fd::AsRawFd;
    let fd = std::io::stdout().as_raw_fd();
    libc_isatty(fd)
}

// Tiny FFI shim to avoid pulling in the `libc` crate just for this.
unsafe extern "C" {
    fn isatty(fd: i32) -> i32;
}
fn libc_isatty(fd: i32) -> bool {
    // SAFETY: libc::isatty just reads fd flags, no side effects.
    unsafe { isatty(fd) != 0 }
}

/// Counts how many checks failed. Caller uses this to set exit code.
pub fn count_failures(results: &[CheckResult]) -> usize {
    results
        .iter()
        .filter(|r| matches!(r.status, CheckStatus::Fail(_)))
        .count()
}

// Suppress unused-import warnings when libc-shim path is taken.
#[allow(dead_code)]
fn _drop_unused() {
    let _: Option<HashMap<String, String>> = None;
    let _: Option<PathBuf> = None;
}
