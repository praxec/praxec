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
//! Connection secrets generalize the same stance one layer down: a `kind: mcp`
//! connection may declare `required_secrets: [ENV_VAR, ...]` — the env vars its
//! own `env:` block references at runtime (never the secret values). Like
//! tools, a missing one is REPORTED but never blocks — it only fails the steps
//! that use that connection, not the whole drive (see
//! [`check_connection_secrets_with`]).
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

/// One `kind: mcp` connection's `required_secrets` check: which of the env
/// vars it declares it needs (referenced from its own `env:` block) fail to
/// resolve. Mirrors [`CredCheck`] one layer down — a provider credential gates
/// every model call, a connection secret gates only that connection's steps,
/// which is why this is ADVISORY (see [`PreflightReport::connection_secrets`]).
pub struct ConnSecretCheck {
    pub connection: String,
    /// The connection's declared `required_secrets` — every env var name it
    /// says it needs at runtime.
    pub required: Vec<String>,
    /// The subset of `required` not resolvable in the environment.
    pub missing: Vec<String>,
}

impl ConnSecretCheck {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }
}

/// The typed preflight result. `ok` is false iff a REQUIRED credential is
/// missing — missing tools are warnings (they fail loud at invocation and
/// only affect the steps that use them), but a missing model key means
/// nothing can run.
pub struct PreflightReport {
    pub credentials: Vec<CredCheck>,
    /// `kind: mcp` connections' `required_secrets` findings — ADVISORY (see
    /// [`check_connection_secrets_with`]): a missing connection secret only
    /// fails the steps that use that connection, never the whole drive, so it
    /// never flips [`PreflightReport::ok`]. The provisioning flow's own
    /// validate step is the hard gate for these.
    pub connection_secrets: Vec<ConnSecretCheck>,
    pub tools: ProvisionReport,
    /// Where the provider-keys file resolves on this machine (for messaging;
    /// its contents are already loaded into env at startup).
    pub keys_file: Option<PathBuf>,
    /// Present iff `praxec.agents.auto_drive` is enabled; `ok()` false iff its
    /// affinity resolves to no model (a doomed drive). See [`AutoDriveModelCheck`].
    pub auto_drive_model: Option<AutoDriveModelCheck>,
    /// Reasoning-config findings: for the effort each binding will actually run
    /// at, whether the reasoning param forms + is supported (see
    /// [`check_reasoning_config`]). All advisory — never flips `ok`.
    pub reasoning: Vec<ReasoningDiagnostic>,
    /// WS2 cost-gate findings: frontier (over-cap) bindings and whether they are
    /// allowlisted (see [`check_frontier_cost_config`]). Advisory — never flips
    /// `ok` (the runtime gate is the enforcement point).
    pub cost: Vec<ReasoningDiagnostic>,
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

/// The pure connection-secrets check core: for each `connections:` entry that
/// declares a non-empty `required_secrets` (the env-var names its own `env:`
/// block references, never the secret values), which of those env vars does
/// `has_env` fail to resolve? Connections with no `required_secrets` (or an
/// empty list) are not checked — a `kind: mcp` connection with no declared
/// secrets has nothing to verify here. Injectable lookup, exactly like
/// [`check_credentials_with`], so the decision logic is unit-testable without
/// touching the process env.
pub fn check_connection_secrets_with(
    config: &Value,
    has_env: impl Fn(&str) -> bool,
) -> Vec<ConnSecretCheck> {
    let Some(connections) = config.pointer("/connections").and_then(Value::as_object) else {
        return Vec::new();
    };
    connections
        .iter()
        .filter_map(|(name, conn)| {
            let required: Vec<String> = conn
                .get("required_secrets")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if required.is_empty() {
                return None; // nothing declared — nothing to check
            }
            let missing = required.iter().filter(|v| !has_env(v)).cloned().collect();
            Some(ConnSecretCheck {
                connection: name.clone(),
                required,
                missing,
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
    let credentials = check_credentials_with(&referenced_providers(config), &has_env);
    let connection_secrets = check_connection_secrets_with(config, &has_env);
    let tools = provision::detect(&provision_config_from(config));
    let auto_drive_model = check_auto_drive_model(config);
    let mut reasoning = check_reasoning_config(config);
    // WS1‑B G3 — per-phase (`states.reasoning_effort`) coverage, appended to the
    // same advisory findings.
    reasoning.extend(check_state_reasoning_config(config));
    let cost = check_frontier_cost_config(config);
    // Reasoning findings are advisory (warn/info) — the catalog is data and a
    // mis-mapped effort degrades quality, it doesn't fail the run — so they do
    // NOT gate `ok`.
    let ok = credentials.iter().all(CredCheck::ok)
        && auto_drive_model
            .as_ref()
            .is_none_or(AutoDriveModelCheck::ok);
    PreflightReport {
        credentials,
        connection_secrets,
        tools,
        keys_file: praxec_core::provider_keys::resolve_path().ok(),
        auto_drive_model,
        reasoning,
        cost,
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
         Every model call against them would fail at dispatch. Fix it with any of: \
         `export OPENROUTER_API_KEY=...`, re-run `praxec init` (it captures the key), or add \
         the key to {keys_file} directly, then retry. \
         Run `praxec doctor --config <path>` for the full preflight report."
    )
}

// ── reasoning-config validator (v0.0.31 WS1-A) ──────────────────────────────

/// Severity of a [`ReasoningDiagnostic`]. All advisory — a warn is a real
/// footgun (effort silently dropped, or a level the model can't do), an info is
/// context (non-reasoning model, or a model absent from the catalog).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningSeverity {
    Warn,
    Info,
}

/// One reasoning-config finding for a `(vendor, model)` binding.
#[derive(Debug, Clone)]
pub struct ReasoningDiagnostic {
    /// Stable code (`REASONING_LEVEL_UNSUPPORTED`, `REASONING_VENDOR_UNMAPPED`,
    /// `REASONING_ON_NONREASONING_MODEL`, `REASONING_MODEL_UNKNOWN`).
    pub code: &'static str,
    pub severity: ReasoningSeverity,
    /// The runtime model string (`vendor:model`) the finding is about.
    pub model: String,
    /// The first binding scope this fired for (`default` / `activity:<name>` /
    /// `override:<key>`) — findings are de-duplicated per `(code, model)` so a
    /// model bound in many pools reports once.
    pub scope: String,
    pub message: String,
}

/// Statically verify that, for the effort each model binding will actually run
/// at, the reasoning param is formed and honored. This closes the gap the
/// auto-drive path leaves open: `reasoning_for` maps the global
/// `tuning.default_effort` to a vendor param with NO check that (a) the vendor
/// is one [`praxec_core::tuning::reasoning_params`] maps (unmapped vendors —
/// Fireworks, the OpenAI-compatible fleet, custom — silently get no param and
/// run at the provider default), or (b) the model's catalog `reasoning_levels`
/// advertises the requested level (e.g. asking a `[none,low,medium]` model for
/// `high`). Mirrors [`check_credentials`]' binding walk over `default` +
/// `activity` + `overrides`.
///
/// Scope (this pass, WS1-A): the GLOBAL default effort × every binding — the
/// auto-drive reality today. Per-step `reasoning_effort` overrides and the
/// future per-activity effort (WS1-B) are out of scope. All findings are
/// advisory; the catalog is data, so a stale/absent entry yields an info, never
/// a block.
pub fn check_reasoning_config(config: &Value) -> Vec<ReasoningDiagnostic> {
    let level = praxec_core::tuning::tuning()
        .reasoning
        .default_effort
        .trim()
        .to_string();
    check_reasoning_config_with(config, &level)
}

/// [`check_reasoning_config`] with the effort level injected (test seam — the
/// production entry reads the global `tuning.default_effort`).
pub fn check_reasoning_config_with(config: &Value, level: &str) -> Vec<ReasoningDiagnostic> {
    use praxec_core::model_resolver::config::Binding;

    let mut out = Vec::new();
    // The GLOBAL default effort — the fallback for a binding that declares no
    // effort of its own. WS1-B: a binding's own `effort` (the model-paired
    // level) wins over this, so we do NOT early-return when the global is
    // `medium`/empty — a paired binding still needs validating.
    let global_level = level.trim();
    let Some(path) = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    else {
        return out;
    };
    let Ok(file) = ModelsFile::from_path(std::path::Path::new(path)) else {
        return out;
    };
    let catalog = praxec_core::model_catalog::model_catalog();

    // Every binding, labeled by its scope. Override keys are rendered readably.
    let mut scoped: Vec<(String, &Binding)> = Vec::new();
    scoped.extend(file.default.iter().map(|b| ("default".to_string(), b)));
    for (name, pool) in &file.activity {
        scoped.extend(pool.members.iter().map(|b| (format!("activity:{name}"), b)));
    }
    for (key, pool) in &file.overrides {
        let label = match (key.affinity, key.tier) {
            (Some(a), Some(t)) => format!("override:{a}-{t}"),
            (Some(a), None) => format!("override:{a}"),
            (None, Some(t)) => format!("override:{t}"),
            (None, None) => "override".to_string(),
        };
        scoped.extend(pool.members.iter().map(|b| (label.clone(), b)));
    }

    let mut seen: std::collections::BTreeSet<(&'static str, String)> =
        std::collections::BTreeSet::new();
    for (scope, b) in scoped {
        // WS1-B — the effort THIS binding will actually run at: its own paired
        // `effort` (the model-specific level) wins over the global default. This
        // is resolved per-binding and validated per-binding, exactly matching
        // the runtime's per-hop resolution.
        let level: &str = b
            .effort
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(global_level);
        // `medium`/empty sends NO reasoning param (provider default) — nothing
        // to validate for this binding.
        if level.is_empty() || level.eq_ignore_ascii_case("medium") {
            continue;
        }

        // Vendor slug is derived EXACTLY as the runtime does — the runtime
        // `provider:model` string uses `Provider::display_name`, which is the
        // token `reasoning_for` later splits on. (FMECA FM-1: a validator that
        // guessed the vendor differently would validate a call that never runs.)
        let vendor = b.provider.display_name();
        let model_string = format!("{vendor}:{}", b.model);

        let finding: Option<(&'static str, ReasoningSeverity, String)> =
            if praxec_core::tuning::reasoning_params(vendor, level).is_none() {
                Some((
                    "REASONING_VENDOR_UNMAPPED",
                    ReasoningSeverity::Warn,
                    format!(
                        "effort '{level}' is not sent to vendor '{vendor}' — only \
                         anthropic/openai/openrouter/gemini map a reasoning param, so this \
                         binding runs at the provider default (reasoning NOT engaged)."
                    ),
                ))
            } else {
                match catalog.models.iter().find(|m| m.model == b.model) {
                    None => Some((
                        "REASONING_MODEL_UNKNOWN",
                        ReasoningSeverity::Info,
                        format!(
                            "model not in the catalog — cannot verify it supports reasoning \
                             level '{level}'."
                        ),
                    )),
                    Some(entry) => {
                        let levels = &entry.reasoning_levels;
                        if levels.is_empty()
                            || levels.iter().all(|l| l.eq_ignore_ascii_case("none"))
                        {
                            Some((
                                "REASONING_ON_NONREASONING_MODEL",
                                ReasoningSeverity::Info,
                                format!(
                                    "model advertises no reasoning levels; effort '{level}' will \
                                     be sent but ignored."
                                ),
                            ))
                        } else if !levels.iter().any(|l| l.eq_ignore_ascii_case(level)) {
                            Some((
                                "REASONING_LEVEL_UNSUPPORTED",
                                ReasoningSeverity::Warn,
                                format!(
                                    "model advertises reasoning levels [{}] but is asked for \
                                     '{level}' — the provider may reject or silently downgrade the \
                                     call.",
                                    levels.join(", ")
                                ),
                            ))
                        } else {
                            None // level supported — nothing to report
                        }
                    }
                }
            };

        if let Some((code, severity, message)) = finding {
            if seen.insert((code, model_string.clone())) {
                out.push(ReasoningDiagnostic {
                    code,
                    severity,
                    model: model_string,
                    scope,
                    message,
                });
            }
        }
    }
    out
}

/// WS1‑B G3 — validate each workflow state's `reasoning_effort` against the
/// models its affinity resolves to. `check_reasoning_config` covers the
/// `models.yaml` bindings (binding ?? global); this covers the PER-PHASE
/// override a capability state declares (`states.<state>.reasoning_effort`,
/// applied by the auto-drive composer).
///
/// A state's effort reaches a model only when that model has no paired effort of
/// its own (a binding's `effort` wins — already covered above), so a pool member
/// WITH its own effort is skipped here. The static affinity is
/// `states.<state>.affinity ?? gateway auto_drive_affinity`; context/input
/// `affinity_override`s are runtime-only and out of static scope. Advisory
/// (`STATE_REASONING_EFFORT_UNSUPPORTED`, warn) — the runtime chain-walk
/// fail-fast is the backstop.
pub fn check_state_reasoning_config(config: &Value) -> Vec<ReasoningDiagnostic> {
    use crate::affinity_resolver::{AgentsYamlAffinityResolver, resolve_affinity_to_chain};

    let mut out = Vec::new();
    let Some(path) = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    else {
        return out;
    };
    let Ok(resolver) = AgentsYamlAffinityResolver::from_path(std::path::Path::new(path)) else {
        return out;
    };
    let global_affinity = config
        .pointer("/praxec/agents/auto_drive_affinity")
        .and_then(Value::as_str)
        .unwrap_or("reasoning");
    let Some(workflows) = config.pointer("/workflows").and_then(Value::as_object) else {
        return out;
    };

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (wf_id, def) in workflows {
        let Some(states) = def.pointer("/states").and_then(Value::as_object) else {
            continue;
        };
        for (state_name, state) in states {
            let Some(effort) = state.get("reasoning_effort").and_then(Value::as_str) else {
                continue;
            };
            let effort = effort.trim();
            // `medium`/empty send no reasoning param — nothing to validate.
            if effort.is_empty() || effort.eq_ignore_ascii_case("medium") {
                continue;
            }
            let affinity = state
                .get("affinity")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(global_affinity);
            for (model, binding_effort) in resolve_affinity_to_chain(resolver.resolver(), affinity)
            {
                // A model that pairs its OWN effort overrides the state effort —
                // its pairing is validated by `check_reasoning_config`, so it is
                // out of scope here.
                if binding_effort.is_some() {
                    continue;
                }
                if !praxec_core::model_catalog::effort_supported(&model, effort) {
                    let scope = format!("{wf_id}/{state_name}");
                    if seen.insert(format!("{scope}|{model}")) {
                        out.push(ReasoningDiagnostic {
                            code: "STATE_REASONING_EFFORT_UNSUPPORTED",
                            severity: ReasoningSeverity::Warn,
                            model: model.clone(),
                            scope: scope.clone(),
                            message: format!(
                                "state `{state_name}` declares reasoning_effort `{effort}`, but \
                                 model `{model}` (reached via affinity `{affinity}`) does not \
                                 advertise it — the run will fail fast here. Pair that model with \
                                 a supported effort in models.yaml, or change the state's effort."
                            ),
                        });
                    }
                }
            }
        }
    }
    out
}

/// WS2 — the $/M cost gate at preflight. A model is FRONTIER when its catalog
/// output $/M is at/over `gateway.cost.frontier_cap_usd_per_m` (default
/// [`DEFAULT_FRONTIER_CAP_USD_PER_M`], $5/M). Each frontier binding NOT in the
/// `gateway.cost.approve_frontier` allowlist is a `FRONTIER_LEAD` warning — the
/// config would let auto-drive spend on a premium model no human approved (the
/// $120-burn class). A frontier binding that IS allowlisted is an info (a human
/// deliberately opted that specific model in). Uses the shared
/// [`praxec_core::model_catalog::is_frontier`] predicate the runtime gate also
/// uses, so preflight and runtime never disagree.
pub fn check_frontier_cost_config(config: &Value) -> Vec<ReasoningDiagnostic> {
    use praxec_core::model_catalog::{
        DEFAULT_FRONTIER_CAP_USD_PER_M, is_frontier, output_usd_per_million,
    };

    let mut out = Vec::new();
    let Some(path) = config
        .pointer("/gateway/models_yaml")
        .and_then(Value::as_str)
    else {
        return out;
    };
    let Ok(file) = ModelsFile::from_path(std::path::Path::new(path)) else {
        return out;
    };
    let cap = config
        .pointer("/gateway/cost/frontier_cap_usd_per_m")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_FRONTIER_CAP_USD_PER_M);
    let approved: std::collections::BTreeSet<String> = config
        .pointer("/gateway/cost/approve_frontier")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let all = file
        .default
        .iter()
        .chain(file.overrides.values().flatten())
        .chain(file.activity.values().flatten());
    for b in all {
        let model_string = format!("{}:{}", b.provider.display_name(), b.model);
        if !is_frontier(&model_string, cap) {
            continue;
        }
        if !seen.insert(model_string.clone()) {
            continue;
        }
        let usd = output_usd_per_million(&model_string).unwrap_or(0.0);
        if approved.contains(&model_string) {
            out.push(ReasoningDiagnostic {
                code: "FRONTIER_APPROVED",
                severity: ReasoningSeverity::Info,
                model: model_string.clone(),
                scope: "cost".to_string(),
                message: format!(
                    "frontier model (${usd:.2}/M output ≥ ${cap:.2} cap) — allowlisted in \
                     gateway.cost.approve_frontier, so the runtime gate permits it."
                ),
            });
        } else {
            out.push(ReasoningDiagnostic {
                code: "FRONTIER_LEAD",
                severity: ReasoningSeverity::Warn,
                model: model_string.clone(),
                scope: "cost".to_string(),
                message: format!(
                    "frontier model (${usd:.2}/M output ≥ ${cap:.2} cap) is bound but NOT in \
                     gateway.cost.approve_frontier — auto-drive would fail fast rather than spend \
                     on it. Add it to the allowlist to approve (a human decision), or bind a \
                     commodity model."
                ),
            });
        }
    }
    out
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
    if !report.connection_secrets.is_empty() {
        out.push_str(
            "connection secrets (env vars each kind: mcp connection declares it needs):\n",
        );
        for c in &report.connection_secrets {
            if c.ok() {
                out.push_str(&format!(
                    "  ok       {}: {}\n",
                    c.connection,
                    c.required.join(", ")
                ));
            } else {
                out.push_str(&format!(
                    "  MISSING  {}: {}\n",
                    c.connection,
                    c.missing.join(", ")
                ));
            }
        }
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
    if !report.reasoning.is_empty() {
        out.push_str("reasoning config (effort each binding will actually run at — advisory):\n");
        for d in &report.reasoning {
            let tag = match d.severity {
                ReasoningSeverity::Warn => "warn",
                ReasoningSeverity::Info => "info",
            };
            out.push_str(&format!(
                "  {tag}  {}  {} — {}\n",
                d.code, d.model, d.message
            ));
        }
    }
    if !report.cost.is_empty() {
        out.push_str("cost gate (frontier models — $/M output cap, advisory):\n");
        for d in &report.cost {
            let tag = match d.severity {
                ReasoningSeverity::Warn => "warn",
                ReasoningSeverity::Info => "info",
            };
            out.push_str(&format!(
                "  {tag}  {}  {} — {}\n",
                d.code, d.model, d.message
            ));
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

    // ── reasoning-config validator ──────────────────────────────────────────

    fn write_models(name: &str, yaml: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "praxec_reasoning_{name}_{}.yaml",
            std::process::id()
        ));
        std::fs::write(&path, yaml).unwrap();
        path
    }

    #[test]
    fn reasoning_validator_flags_a_level_the_model_cannot_do() {
        // qwen3-coder advertises [none,low,medium] — asking `high` must warn.
        let path = write_models(
            "unsup",
            concat!(
                // `default:` is required by the models.yaml schema; glm-5.2
                // supports `high`, so it is silent — only the qwen activity fires.
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: z-ai/glm-5.2\n",
                "activity:\n",
                "  coding:\n",
                "    - provider: { name: openrouter }\n",
                "      model: qwen/qwen3-coder\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_reasoning_config_with(&cfg, "high");
        std::fs::remove_file(&path).ok();
        assert!(
            diags.iter().any(|d| d.code == "REASONING_LEVEL_UNSUPPORTED"
                && d.model == "openrouter:qwen/qwen3-coder"
                && d.severity == ReasoningSeverity::Warn),
            "{diags:?}"
        );
    }

    #[test]
    fn reasoning_validator_passes_a_supported_level_silently() {
        // deepseek-v4-pro advertises [none,high] — `high` is supported → no finding.
        let path = write_models(
            "ok",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: deepseek/deepseek-v4-pro\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_reasoning_config_with(&cfg, "high");
        std::fs::remove_file(&path).ok();
        assert!(
            diags.is_empty(),
            "a supported level must be silent, got {diags:?}"
        );
    }

    #[test]
    fn reasoning_validator_flags_an_unmapped_vendor_dropping_effort() {
        // A reasoning effort on a vendor `reasoning_params` doesn't map (fireworks)
        // is silently dropped at dispatch — must warn.
        let path = write_models(
            "unmapped",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: fireworks }\n",
                "    model: accounts/fireworks/models/x\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_reasoning_config_with(&cfg, "high");
        std::fs::remove_file(&path).ok();
        assert!(
            diags.iter().any(|d| d.code == "REASONING_VENDOR_UNMAPPED"),
            "{diags:?}"
        );
    }

    #[test]
    fn reasoning_validator_is_silent_at_medium_and_empty() {
        // `medium`/empty send NO reasoning param (provider default) → nothing to validate.
        let path = write_models(
            "medium",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: qwen/qwen3-coder\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        assert!(check_reasoning_config_with(&cfg, "medium").is_empty());
        assert!(check_reasoning_config_with(&cfg, "").is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reasoning_validator_dedups_a_model_bound_in_many_pools() {
        // qwen in two activities → ONE finding, not two.
        let path = write_models(
            "dedup",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: z-ai/glm-5.2\n",
                "activity:\n",
                "  coding:\n",
                "    - provider: { name: openrouter }\n",
                "      model: qwen/qwen3-coder\n",
                "  uifix:\n",
                "    - provider: { name: openrouter }\n",
                "      model: qwen/qwen3-coder\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_reasoning_config_with(&cfg, "high");
        std::fs::remove_file(&path).ok();
        let n = diags
            .iter()
            .filter(|d| {
                d.code == "REASONING_LEVEL_UNSUPPORTED" && d.model == "openrouter:qwen/qwen3-coder"
            })
            .count();
        assert_eq!(n, 1, "must dedup per (code, model), got {diags:?}");
    }

    #[test]
    fn reasoning_validator_honors_a_binding_paired_effort_over_the_global() {
        // Global is `low` (which deepseek-v4-pro can't do), but the binding
        // PAIRS deepseek with `high` (which it can) → no finding. The pair wins
        // over the global — this is the WS1-B fix for the live config bug.
        let path = write_models(
            "paired-ok",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: deepseek/deepseek-v4-pro\n",
                "    effort: high\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_reasoning_config_with(&cfg, "low");
        std::fs::remove_file(&path).ok();
        assert!(
            diags.is_empty(),
            "a valid paired effort must override the global and be silent, got {diags:?}"
        );
    }

    #[test]
    fn reasoning_validator_flags_an_invalid_binding_paired_effort() {
        // Binding pairs qwen (max `medium`) with `high` → LEVEL_UNSUPPORTED even
        // though the global (`medium`) alone would have been silent. Proves the
        // paired effort is validated per-binding.
        let path = write_models(
            "paired-bad",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: qwen/qwen3-coder\n",
                "    effort: high\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_reasoning_config_with(&cfg, "medium");
        std::fs::remove_file(&path).ok();
        assert!(
            diags.iter().any(|d| d.code == "REASONING_LEVEL_UNSUPPORTED"
                && d.model == "openrouter:qwen/qwen3-coder"),
            "{diags:?}"
        );
    }

    #[test]
    fn g3_flags_a_state_effort_the_affinity_pool_cannot_do() {
        // affinity `coding` → qwen (max `medium`); a state asks for `high`.
        let path = write_models(
            "g3bad",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: z-ai/glm-5.2\n",
                "activity:\n",
                "  coding:\n",
                "    - provider: { name: openrouter }\n",
                "      model: qwen/qwen3-coder\n",
            ),
        );
        let cfg = json!({
            "gateway": { "models_yaml": path.to_str().unwrap() },
            "workflows": { "wf": { "states": {
                "hard": { "affinity": "coding", "reasoning_effort": "high" }
            } } }
        });
        let diags = check_state_reasoning_config(&cfg);
        std::fs::remove_file(&path).ok();
        assert!(
            diags
                .iter()
                .any(|d| d.code == "STATE_REASONING_EFFORT_UNSUPPORTED"
                    && d.model == "openrouter:qwen/qwen3-coder"
                    && d.scope == "wf/hard"),
            "{diags:?}"
        );
    }

    #[test]
    fn g3_is_silent_when_the_state_effort_is_supported() {
        // affinity `coding` → glm-5.2 (advertises `high`) → no finding.
        let path = write_models(
            "g3ok",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: z-ai/glm-5.2\n",
                "activity:\n",
                "  coding:\n",
                "    - provider: { name: openrouter }\n",
                "      model: z-ai/glm-5.2\n",
            ),
        );
        let cfg = json!({
            "gateway": { "models_yaml": path.to_str().unwrap() },
            "workflows": { "wf": { "states": {
                "hard": { "affinity": "coding", "reasoning_effort": "high" }
            } } }
        });
        let diags = check_state_reasoning_config(&cfg);
        std::fs::remove_file(&path).ok();
        assert!(
            diags.is_empty(),
            "a supported state effort must be silent, got {diags:?}"
        );
    }

    #[test]
    fn frontier_lead_warns_for_an_unapproved_over_cap_model() {
        // claude-opus-4-8 is $25/M output — over the $5 cap, not allowlisted.
        let path = write_models(
            "frontier",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: anthropic }\n",
                "    model: claude-opus-4-8\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_frontier_cost_config(&cfg);
        std::fs::remove_file(&path).ok();
        assert!(
            diags.iter().any(|d| d.code == "FRONTIER_LEAD"
                && d.model == "anthropic:claude-opus-4-8"
                && d.severity == ReasoningSeverity::Warn),
            "{diags:?}"
        );
    }

    #[test]
    fn frontier_approved_is_an_info_not_a_warning_when_allowlisted() {
        let path = write_models(
            "frontok",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: anthropic }\n",
                "    model: claude-opus-4-8\n",
            ),
        );
        let cfg = json!({
            "gateway": {
                "models_yaml": path.to_str().unwrap(),
                "cost": { "approve_frontier": ["anthropic:claude-opus-4-8"] }
            }
        });
        let diags = check_frontier_cost_config(&cfg);
        std::fs::remove_file(&path).ok();
        assert!(
            diags.iter().any(|d| d.code == "FRONTIER_APPROVED"),
            "{diags:?}"
        );
        assert!(
            !diags.iter().any(|d| d.code == "FRONTIER_LEAD"),
            "an allowlisted model must not also warn: {diags:?}"
        );
    }

    #[test]
    fn a_commodity_model_trips_no_cost_finding() {
        // glm-5.2 is $3/M — under the $5 cap.
        let path = write_models(
            "commodity",
            concat!(
                "version: 1\n",
                "default:\n",
                "  - provider: { name: openrouter }\n",
                "    model: z-ai/glm-5.2\n",
            ),
        );
        let cfg = json!({ "gateway": { "models_yaml": path.to_str().unwrap() } });
        let diags = check_frontier_cost_config(&cfg);
        std::fs::remove_file(&path).ok();
        assert!(
            diags.is_empty(),
            "a commodity model must be silent, got {diags:?}"
        );
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
        // The remedy must be a GATEWAY-native command (not `px`, which a gateway-
        // only install does not have): re-run `praxec init` or export the key.
        assert!(err.contains("praxec init"), "{err}");
        assert!(
            !err.contains("px set-provider-keys"),
            "must not point at the px-only path: {err}"
        );
    }

    #[test]
    fn guard_passes_when_the_key_is_present() {
        let cfg = json!({});
        assert!(
            guard_provider_credentials_with(&cfg, &["openrouter:m"], |v| v == "OPENROUTER_API_KEY")
                .is_ok()
        );
    }

    // ── connection secrets ───────────────────────────────────────────────────

    /// A config whose only model reference is fine, and one `kind: mcp`
    /// connection declaring `required_secrets`.
    fn config_with_connection_secrets(required: &[&str]) -> Value {
        json!({
            "workflows": {
                "wf": { "states": { "s": { "transitions": {
                    "go": { "target": "done", "executor": {
                        "kind": "llm", "model": "ollama:llama3", "prompt": "p"
                    } }
                } } } }
            },
            "connections": {
                "figma": { "kind": "mcp", "command": "figma-mcp", "required_secrets": required }
            }
        })
    }

    #[test]
    fn missing_connection_secret_is_reported_but_stays_ok() {
        let cfg = config_with_connection_secrets(&["FIGMA_TOKEN"]);
        let report = preflight_with(&cfg, |_| false);
        assert!(
            report.ok,
            "a missing connection secret must be advisory, not a hard failure"
        );
        assert_eq!(report.connection_secrets.len(), 1);
        let check = &report.connection_secrets[0];
        assert_eq!(check.connection, "figma");
        assert_eq!(check.missing, vec!["FIGMA_TOKEN"]);
        let rendered = format_report(&report);
        assert!(rendered.contains("MISSING  figma"), "{rendered}");
        assert!(rendered.contains("FIGMA_TOKEN"), "{rendered}");
    }

    #[test]
    fn all_connection_secrets_present_is_ok() {
        let cfg = config_with_connection_secrets(&["FIGMA_TOKEN", "SOME_PAT"]);
        let report = preflight_with(&cfg, |_| true);
        assert!(report.ok);
        assert_eq!(report.connection_secrets.len(), 1);
        assert!(report.connection_secrets[0].ok());
        let rendered = format_report(&report);
        assert!(rendered.contains("ok       figma"), "{rendered}");
    }

    #[test]
    fn connection_with_no_required_secrets_is_not_listed() {
        let cfg = config_with_connection_secrets(&[]);
        let report = preflight_with(&cfg, |_| false);
        assert!(report.ok);
        assert!(
            report.connection_secrets.is_empty(),
            "a connection declaring no required_secrets must not appear"
        );

        let cfg_absent = json!({
            "connections": { "figma": { "kind": "mcp", "command": "figma-mcp" } }
        });
        let report_absent = preflight_with(&cfg_absent, |_| false);
        assert!(report_absent.connection_secrets.is_empty());
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
