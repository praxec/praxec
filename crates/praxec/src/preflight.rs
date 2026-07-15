//! P15 — fail-fast credential/tooling preflight.
//!
//! A non-interactive drive (`orchestrate`, `serve` under auto_drive) used to
//! fail LATE and opaquely when a provider API key was missing: the run booted,
//! did work, and then died deep inside a model call. This module turns that
//! into a fail-fast at start: enumerate the providers the resolved config's
//! model bindings actually reference, check each one's credential is
//! resolvable (env — which [`praxec_core::provider_keys`] has already loaded
//! the providers.env file into at startup), and refuse to start with a message
//! naming the provider, the env var, and the file the operator should edit.
//!
//! Tools (`kind: mcp` connection binaries, via [`crate::provision::detect`])
//! are REPORTED but never block: a missing tool fails loud at invocation time
//! and only affects the steps that use it, whereas a missing model key means
//! nothing agentic can run at all.
//!
//! Lives in the `praxec` crate (not core) because it needs BOTH
//! `praxec_core::provider_keys`/`providers` AND `crate::provision` — putting
//! it in core would invert the dependency on `provision`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use praxec_core::model_resolver::{ModelsFile, config::Provider};
use praxec_core::providers::ProviderId;
use praxec_core::validate::for_each_executor_site;
use serde_json::Value;

use crate::provision::{self, ProvisionReport};

/// One provider's credential check: which of its env vars are missing.
/// Only providers that NEED a credential appear (keyless local providers
/// like ollama have nothing to check).
pub struct CredCheck {
    pub provider: ProviderId,
    /// Every env var the provider requires (bedrock needs its AWS triplet).
    pub required_vars: Vec<&'static str>,
    /// The subset of `required_vars` not resolvable in the environment.
    pub missing_vars: Vec<&'static str>,
}

impl CredCheck {
    pub fn ok(&self) -> bool {
        self.missing_vars.is_empty()
    }
}

/// The typed preflight result. `ok` is false iff a REQUIRED credential is
/// missing — missing tools are warnings (they fail loud at invocation and
/// only affect the steps that use them), but a missing model key means
/// nothing can run.
pub struct PreflightReport {
    pub credentials: Vec<CredCheck>,
    pub tools: ProvisionReport,
    /// Where the provider-keys file resolves on this machine (for messaging;
    /// its contents are already loaded into env at startup).
    pub keys_file: Option<PathBuf>,
    /// Present iff `praxec.agents.auto_drive` is enabled; `ok()` false iff its
    /// affinity resolves to no model (a doomed drive). See [`AutoDriveModelCheck`].
    pub auto_drive_model: Option<AutoDriveModelCheck>,
    pub ok: bool,
}

/// Enumerate the curated providers this resolved config's models actually
/// reference — the keys a drive will need. Sources:
/// - every binding in `gateway.models_yaml` (default / overrides / activity),
///   when the key is set and the file loads (an unloadable file is the
///   existing `MODELS_YAML_LOAD_FAILED` doctor's concern, not duplicated here);
/// - every executor site with an explicit `model: "provider:id"` /
///   `"provider/id"` pin.
///
/// Providers no model references are NOT checked; unknown/custom prefixes are
/// skipped (the custom OpenAI-compatible escape hatch carries no curated key).
pub fn referenced_providers(config: &Value) -> BTreeSet<ProviderId> {
    let mut out = BTreeSet::new();

    if let Some(path) = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    {
        if let Ok(file) = ModelsFile::from_path(std::path::Path::new(path)) {
            let all = file
                .default
                .iter()
                .chain(file.overrides.values().flatten())
                .chain(file.activity.values().flatten());
            for binding in all {
                if let Provider::Known(id) = binding.provider {
                    out.insert(id);
                }
            }
        }
    }

    if let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) {
        for def in workflows.values() {
            for_each_executor_site(def, |site| {
                if let Some(model) = site.executor.get("model").and_then(Value::as_str) {
                    if let Some(id) = provider_of_model_str(model) {
                        out.insert(id);
                    }
                }
            });
        }
    }

    out
}

/// Parse the provider slug off a concrete model string — the resolved
/// `provider:model-id` form (orchestrate `--model`, models.yaml resolution)
/// or the `provider/model` form a `kind: llm` `model:` pin uses. `None` for
/// unknown/custom prefixes.
pub fn provider_of_model_str(model: &str) -> Option<ProviderId> {
    let prefix = model.split(['/', ':']).next()?;
    ProviderId::from_slug(prefix)
}

/// The pure credential-check core: for each provider that needs a credential,
/// which of its env vars does `has_env` fail to resolve? Keyless providers
/// (ollama, llamacpp) are skipped — nothing to check. Injectable lookup so the
/// decision logic is unit-testable without touching the process env.
pub fn check_credentials_with(
    providers: &BTreeSet<ProviderId>,
    has_env: impl Fn(&str) -> bool,
) -> Vec<CredCheck> {
    providers
        .iter()
        .filter_map(|&provider| {
            let required_vars = provider.credentials().env_vars();
            if required_vars.is_empty() {
                return None; // keyless / local — nothing to check
            }
            let missing_vars = required_vars
                .iter()
                .copied()
                .filter(|v| !has_env(v))
                .collect();
            Some(CredCheck {
                provider,
                required_vars,
                missing_vars,
            })
        })
        .collect()
}

/// Whether an auto-drive-enabled config has a model its agents can actually use.
///
/// `praxec.agents.auto_drive: true` means every auto-drivable `actor: agent` leaf
/// is driven against the `auto_drive_affinity` (default `reasoning`), resolved
/// through `gateway.models_yaml`. If that affinity resolves to NO model — the
/// `models_yaml` key is unset, the file won't load, or it defines no binding for
/// the affinity — the drive is doomed: every agent leaf fails at runtime with no
/// model, AFTER burning setup + wall-clock. That is a silent fail-open on a
/// runtime binding — the exact class as a coding leaf handed an empty
/// `repo_root`. This surfaces it as a LOUD preflight failure (the model analog of
/// `REPO_ROOT_REQUIRED`). `Some` iff auto-drive is enabled (so the check applies).
pub struct AutoDriveModelCheck {
    pub affinity: String,
    pub models_yaml: Option<String>,
    /// `Some(model)` iff the affinity resolves to a concrete model.
    pub resolved_model: Option<String>,
}

impl AutoDriveModelCheck {
    pub fn ok(&self) -> bool {
        self.resolved_model.is_some()
    }
}

/// `Some` iff `praxec.agents.auto_drive` is on; the inner `resolved_model` is
/// `Some` iff the `auto_drive_affinity` resolves through `gateway.models_yaml`.
pub fn check_auto_drive_model(config: &Value) -> Option<AutoDriveModelCheck> {
    let auto_drive = config
        .pointer("/praxec/agents/auto_drive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !auto_drive {
        return None;
    }
    let affinity = config
        .pointer("/praxec/agents/auto_drive_affinity")
        .and_then(Value::as_str)
        .unwrap_or("reasoning")
        .to_string();
    let models_yaml = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
        .map(str::to_string);
    let resolved_model = models_yaml.as_deref().and_then(|path| {
        crate::affinity_resolver::AgentsYamlAffinityResolver::from_path(std::path::Path::new(path))
            .ok()
            .and_then(|loaded| {
                crate::affinity_resolver::resolve_affinity_to_model(loaded.resolver(), &affinity)
            })
    });
    Some(AutoDriveModelCheck {
        affinity,
        models_yaml,
        resolved_model,
    })
}

/// Assemble the full report with an injectable env lookup (test seam).
pub fn preflight_with(config: &Value, has_env: impl Fn(&str) -> bool) -> PreflightReport {
    let credentials = check_credentials_with(&referenced_providers(config), has_env);
    let tools = provision::detect(&provision_config_from(config));
    let auto_drive_model = check_auto_drive_model(config);
    let ok = credentials.iter().all(CredCheck::ok)
        && auto_drive_model
            .as_ref()
            .is_none_or(AutoDriveModelCheck::ok);
    PreflightReport {
        credentials,
        tools,
        keys_file: praxec_core::provider_keys::resolve_path().ok(),
        auto_drive_model,
        ok,
    }
}

/// Production preflight over the process env (the providers.env file was
/// loaded into env at startup, so this sees file + env keys).
pub fn preflight(config: &Value) -> PreflightReport {
    preflight_with(config, |v| std::env::var(v).is_ok())
}

/// Project the resolved config's `connections:` into the shape
/// [`provision::detect`] takes. URL-based connections have no local binary,
/// so only command-bearing entries are carried.
fn provision_config_from(config: &Value) -> provision::Config {
    let connections = config
        .pointer("/connections")
        .and_then(Value::as_object)
        .map(|conns| {
            conns
                .values()
                .filter_map(|conn| {
                    let kind = conn.get("kind").and_then(Value::as_str)?;
                    let command = conn.get("command").and_then(Value::as_str)?;
                    Some(provision::Connection {
                        kind: kind.to_string(),
                        command: command.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    provision::Config { connections }
}

/// Fail-fast gate for `orchestrate` / `serve`: refuse to start when a provider
/// key the config's models (or `extra_models`, e.g. orchestrate's `--model`)
/// need is missing. Missing TOOLS never block here — they fail loud at
/// invocation time. Returns the clear operator-facing error naming the
/// provider, the env var(s), and the file to fix.
pub fn guard_provider_credentials(config: &Value, extra_models: &[&str]) -> anyhow::Result<()> {
    guard_provider_credentials_with(config, extra_models, |v| std::env::var(v).is_ok())
}

/// [`guard_provider_credentials`] with an injectable env lookup (test seam).
fn guard_provider_credentials_with(
    config: &Value,
    extra_models: &[&str],
    has_env: impl Fn(&str) -> bool,
) -> anyhow::Result<()> {
    let mut providers = referenced_providers(config);
    for model in extra_models {
        if let Some(id) = provider_of_model_str(model) {
            providers.insert(id);
        }
    }
    let checks = check_credentials_with(&providers, has_env);
    let missing: Vec<&CredCheck> = checks.iter().filter(|c| !c.ok()).collect();
    if missing.is_empty() {
        return Ok(());
    }

    let keys_file = praxec_core::provider_keys::resolve_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unresolvable — set $PRAXEC_PROVIDER_KEYS_FILE>".to_string());
    let findings = missing
        .iter()
        .map(|c| {
            format!(
                "  - provider `{}` ({}): env var(s) not set: {}",
                c.provider.slug(),
                c.provider.display(),
                c.missing_vars.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::bail!(
        "PREFLIGHT_MISSING_CREDENTIAL: refusing to start — this config's model bindings \
         reference provider(s) whose API key is not resolvable:\n{findings}\n\n\
         Every model call against them would fail at dispatch. Set the env var(s), or add \
         the key to {keys_file} (`px set-provider-keys`), then retry. \
         Run `praxec doctor --config <path>` for the full preflight report."
    )
}

/// Human-readable report for `praxec doctor`.
pub fn format_report(report: &PreflightReport) -> String {
    let mut out = String::new();
    out.push_str("credentials (providers the config's models reference):\n");
    if report.credentials.is_empty() {
        out.push_str("  (none required — no keyed provider is referenced)\n");
    }
    for c in &report.credentials {
        if c.ok() {
            out.push_str(&format!(
                "  ok       {} ({})\n",
                c.provider.slug(),
                c.required_vars.join(", ")
            ));
        } else {
            out.push_str(&format!(
                "  MISSING  {} — env var(s) not set: {}\n",
                c.provider.slug(),
                c.missing_vars.join(", ")
            ));
        }
    }
    if let Some(path) = &report.keys_file {
        out.push_str(&format!(
            "  keys file: {} (env vars win over the file)\n",
            path.display()
        ));
    }
    out.push_str("tools (kind: mcp connection binaries on PATH):\n");
    if report.tools.present.is_empty() && report.tools.missing.is_empty() {
        out.push_str("  (no kind: mcp connections configured)\n");
    }
    for t in &report.tools.present {
        out.push_str(&format!("  ok       {t}\n"));
    }
    for t in &report.tools.missing {
        out.push_str(&format!(
            "  missing  {t} — not on PATH (warning: steps using this connection \
             will fail at invocation)\n"
        ));
    }
    if let Some(adm) = &report.auto_drive_model {
        out.push_str("auto-drive model (praxec.agents.auto_drive is on):\n");
        match &adm.resolved_model {
            Some(model) => out.push_str(&format!(
                "  ok       affinity '{}' -> {model}\n",
                adm.affinity
            )),
            None => {
                let why = match &adm.models_yaml {
                    None => "gateway.models_yaml is unset".to_string(),
                    Some(p) => format!("'{p}' defines no binding for it (or failed to load)"),
                };
                out.push_str(&format!(
                    "  MISSING  AUTO_DRIVE_NO_MODEL: affinity '{}' resolves to no model — {why}. \
                     Set gateway.models_yaml to a bindings file defining '{}' (or a 'default' \
                     chain); without it every auto-driven agent leaf fails at runtime with no \
                     model.\n",
                    adm.affinity, adm.affinity
                ));
            }
        }
    }
    out.push_str(if report.ok {
        "preflight: ok\n"
    } else {
        "preflight: FAILED — see the MISSING line(s) above (a required provider credential, \
         or auto-drive has no model)\n"
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A config whose only model reference is an explicit executor pin.
    fn config_with_model_pin(model: &str) -> Value {
        json!({
            "workflows": {
                "wf": { "states": { "s": { "transitions": {
                    "go": { "target": "done", "executor": {
                        "kind": "llm", "model": model, "prompt": "p"
                    } }
                } } } }
            },
            "connections": {
                "build": { "kind": "mcp", "command": "cargo" }
            }
        })
    }

    #[test]
    fn all_keys_present_and_tools_present_is_ok() {
        let cfg = config_with_model_pin("openrouter:z-ai/some-model");
        let report = preflight_with(&cfg, |_| true);
        assert!(report.ok);
        assert_eq!(report.credentials.len(), 1);
        assert!(report.credentials[0].ok());
        assert_eq!(report.tools.present, vec!["cargo"]);
        assert!(report.tools.missing.is_empty());
    }

    #[test]
    fn missing_referenced_key_fails_naming_provider_and_env_var() {
        let cfg = config_with_model_pin("openrouter:z-ai/some-model");
        let report = preflight_with(&cfg, |_| false);
        assert!(!report.ok);
        let check = &report.credentials[0];
        assert_eq!(check.provider, ProviderId::Openrouter);
        assert_eq!(check.missing_vars, vec!["OPENROUTER_API_KEY"]);
        let rendered = format_report(&report);
        assert!(rendered.contains("MISSING  openrouter"), "{rendered}");
        assert!(rendered.contains("OPENROUTER_API_KEY"), "{rendered}");
    }

    #[test]
    fn missing_tool_is_a_warning_not_a_failure() {
        let mut cfg = config_with_model_pin("openrouter:z-ai/some-model");
        cfg["connections"]["build"]["command"] = json!("nonexistent_command_xyz");
        let report = preflight_with(&cfg, |_| true);
        assert!(report.ok, "a missing tool must not flip ok");
        assert_eq!(report.tools.missing, vec!["nonexistent_command_xyz"]);
    }

    #[test]
    fn unreferenced_providers_are_not_checked() {
        let cfg = config_with_model_pin("anthropic:claude-x");
        let report = preflight_with(&cfg, |v| v == "ANTHROPIC_API_KEY");
        assert!(report.ok);
        let checked: Vec<ProviderId> = report.credentials.iter().map(|c| c.provider).collect();
        assert_eq!(
            checked,
            vec![ProviderId::Anthropic],
            "only the referenced provider is checked"
        );
    }

    #[test]
    fn keyless_local_providers_have_nothing_to_check() {
        let cfg = config_with_model_pin("ollama:llama3");
        let report = preflight_with(&cfg, |_| false);
        assert!(report.ok, "keyless providers cannot fail the preflight");
        assert!(report.credentials.is_empty());
    }

    #[test]
    fn model_str_provider_parses_both_pin_forms() {
        assert_eq!(
            provider_of_model_str("openrouter:z-ai/glm"),
            Some(ProviderId::Openrouter)
        );
        assert_eq!(
            provider_of_model_str("openai/gpt-4o"),
            Some(ProviderId::Openai)
        );
        assert_eq!(provider_of_model_str("custom:whatever"), None);
    }

    #[test]
    fn models_yaml_bindings_contribute_referenced_providers() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "praxec_preflight_models_{}.yaml",
            std::process::id()
        ));
        let yaml = concat!(
            "version: 1\n",
            "default:\n",
            "  - provider: { name: openrouter }\n",
            "    model: z-ai/some-model\n",
            "activity:\n",
            "  review:\n",
            "    - provider: { name: anthropic }\n",
            "      model: claude-x\n",
        );
        std::fs::write(&path, yaml).unwrap();
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let providers = referenced_providers(&cfg);
        std::fs::remove_file(&path).ok();
        assert!(providers.contains(&ProviderId::Openrouter));
        assert!(providers.contains(&ProviderId::Anthropic));
        assert_eq!(providers.len(), 2);
    }

    #[test]
    fn guard_message_names_provider_env_var_and_fix() {
        let cfg = json!({});
        let err = guard_provider_credentials_with(&cfg, &["openrouter:z-ai/some-model"], |_| false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("PREFLIGHT_MISSING_CREDENTIAL"), "{err}");
        assert!(err.contains("`openrouter`"), "{err}");
        assert!(err.contains("OPENROUTER_API_KEY"), "{err}");
        assert!(err.contains("set-provider-keys"), "{err}");
    }

    #[test]
    fn guard_passes_when_the_key_is_present() {
        let cfg = json!({});
        assert!(
            guard_provider_credentials_with(&cfg, &["openrouter:m"], |v| v == "OPENROUTER_API_KEY")
                .is_ok()
        );
    }

    // ── AUTO_DRIVE_NO_MODEL poka-yoke (the dogfood finding) ──────────────────

    /// The exact misconfig that passed doctor silently: auto-drive on, no
    /// `gateway.models_yaml`. Now a loud preflight FAILURE.
    #[test]
    fn auto_drive_without_models_yaml_fails_preflight() {
        let cfg = json!({ "praxec": { "agents": { "auto_drive": true } } });
        let report = preflight_with(&cfg, |_| true);
        assert!(
            !report.ok,
            "auto-drive with no models_yaml must fail preflight"
        );
        let adm = report
            .auto_drive_model
            .as_ref()
            .expect("check applies when auto_drive on");
        assert!(!adm.ok());
        assert_eq!(adm.affinity, "reasoning"); // the default
        assert!(format_report(&report).contains("AUTO_DRIVE_NO_MODEL"));
    }

    /// Not applicable when auto-drive is off — a config with no agents to drive
    /// needs no model, and preflight reflects only credentials.
    #[test]
    fn auto_drive_disabled_is_not_flagged() {
        let cfg = json!({ "praxec": { "agents": { "auto_drive": false } } });
        let report = preflight_with(&cfg, |_| true);
        assert!(report.auto_drive_model.is_none());
        assert!(report.ok);
    }

    /// Passes when `models_yaml` resolves the affinity to a concrete model.
    #[test]
    fn auto_drive_with_resolvable_models_yaml_passes() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.yaml");
        std::fs::write(
            &models,
            "version: 1\n\
             default:\n\
             \x20 - provider: { name: openrouter }\n\
             \x20   model: openrouter/base\n\
             activity:\n\
             \x20 reasoning:\n\
             \x20   - provider: { name: openrouter }\n\
             \x20     model: openrouter/reasoning\n",
        )
        .unwrap();
        let cfg = json!({
            "gateway": { "models_yaml": models.to_str().unwrap() },
            "praxec": { "agents": { "auto_drive": true, "auto_drive_affinity": "reasoning" } }
        });
        let report = preflight_with(&cfg, |_| true);
        let adm = report.auto_drive_model.as_ref().expect("check applies");
        assert!(adm.ok(), "affinity must resolve: {:?}", adm.resolved_model);
        assert!(report.ok);
    }
}
