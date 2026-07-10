//! SPEC §33 D8 — cost catalog with freshness gates.
//!
//! Maps `"provider:model-id"` → USD-per-token pricing so the executor
//! can compute a real `cost_usd` for each turn (D6/D7 plumbed the slot
//! and audit field as `None`; D8 fills it in).
//!
//! Per FMECA F8 (mitigation: never silently accept `cost_usd: null`
//! against a configured `max_cost_usd` budget cap), validation runs at
//! **workflow load time**:
//!
//! - Missing catalog entry + `max_cost_usd` set →
//!   `CostCatalogError::Missing`, surfaced by the doctor as
//!   `COST_CATALOG_MISSING_ENTRY`.
//! - Stale catalog entry (verified more than 90 days before `today`)
//!   plus `max_cost_usd` set → `CostCatalogError::Stale`, surfaced as
//!   `COST_CATALOG_STALE`.
//!
//! Without a `max_cost_usd` cap, both conditions degrade to a doctor
//! `Warning` rather than an `Error`: the operator sees the catalog drift
//! signal, but workflow load is not blocked because no budget can be
//! silently bypassed.
//!
//! Runtime semantics: when `compute_cost_usd` fails (missing or stale),
//! the executor logs a `tracing::warn!` and leaves `cost_usd: None` on
//! the audit event. The load-time check is the budget-cap guarantee;
//! runtime is best-effort.
//!
//! The catalog is **data**, not code: `data/model_costs.json`, loaded at
//! runtime via the shared `core::catalog` loader (operator-overridable) so
//! prices update without a Praxec release.

use std::collections::HashMap;
use std::sync::LazyLock;

use chrono::NaiveDate;
use praxec_core::validate::Diagnostic;
use serde_json::Value;

/// A sync affinity → `"provider:model-id"` resolver. The binary builds this
/// off `models.yaml` (D9) and passes it into the doctor; the doctor stays
/// decoupled from the agent-resolver types behind this `Fn`.
type AffinityResolveFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Sentinel date string for the catalog as a whole. Bumped whenever
/// any entry is re-verified; operators can read this off the binary to
/// answer "how fresh is your shipped cost catalog?". Format: ISO 8601
/// date (`YYYY-MM-DD`).
pub const LAST_VERIFIED: &str = "2026-05-29";

/// Maximum age (in days) of a catalog entry's `verified_at` before
/// load-time validation considers it stale. Per SPEC §33 plan FMECA F8.
pub const STALENESS_THRESHOLD_DAYS: i64 = 90;

/// Diagnostic code emitted at workflow load when the model is not in
/// the catalog AND a `max_cost_usd` budget cap is configured.
pub const COST_CATALOG_MISSING_ENTRY: &str = "COST_CATALOG_MISSING_ENTRY";

/// Diagnostic code emitted at workflow load when the model's catalog
/// entry was verified more than `STALENESS_THRESHOLD_DAYS` ago AND a
/// `max_cost_usd` budget cap is configured.
pub const COST_CATALOG_STALE: &str = "COST_CATALOG_STALE";

/// One catalog entry — USD per million tokens for input + output plus
/// the ISO-8601 date the price was last re-verified against the
/// provider's public pricing page. Loaded from data, not hard-coded.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct ModelCost {
    /// USD per 1,000,000 input tokens.
    pub input_usd_per_million_tokens: f64,
    /// USD per 1,000,000 output tokens.
    pub output_usd_per_million_tokens: f64,
    /// ISO 8601 `YYYY-MM-DD` date the pricing was last verified.
    pub verified_at: String,
}

impl ModelCost {
    /// Parse `verified_at` into a `NaiveDate`. Returns `None` if the
    /// value is malformed — treated as an "infinitely stale" entry by
    /// `validate_for_workflow`.
    pub fn verified_at_date(&self) -> Option<NaiveDate> {
        NaiveDate::parse_from_str(&self.verified_at, "%Y-%m-%d").ok()
    }
}

/// Typed catalog errors. Mapped to doctor diagnostics by
/// `validate_for_workflow` and to `tracing::warn!` lines at runtime by
/// the executor's `compute_cost_usd` call site.
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum CostCatalogError {
    /// Model is not present in the catalog at all.
    #[error("model '{model}' is not in the cost catalog")]
    Missing { model: String },

    /// Model is present but its `verified_at` is more than
    /// `STALENESS_THRESHOLD_DAYS` days before `today`.
    #[error(
        "model '{model}' has stale catalog entry (verified {verified_at}, threshold {threshold_days} days)"
    )]
    Stale {
        model: String,
        verified_at: String,
        threshold_days: i64,
    },
}

/// The shipped default cost catalog — a dated snapshot kept as **data**
/// (`data/model_costs.json`), so prices update without a code release.
const DEFAULT_MODEL_COSTS: &str = include_str!("../data/model_costs.json");

/// First-lookup-builds-the-map cache, loaded from the catalog data file. An
/// operator override is honoured (`$PRAXEC_MODEL_COSTS_FILE`, then
/// `~/.praxec/model_costs.json`, then `.praxec/model_costs.json`),
/// else the shipped default. Entries that genuinely can't be confirmed are
/// omitted — an operator with a `max_cost_usd` cap on an unknown model gets
/// `CostCatalogError::Missing` at load time (correct FMECA F8 behavior).
static CATALOG: LazyLock<HashMap<String, ModelCost>> = LazyLock::new(|| {
    praxec_core::catalog::load_catalog(
        "PRAXEC_MODEL_COSTS_FILE",
        "model_costs.json",
        DEFAULT_MODEL_COSTS,
    )
});

/// Look up a model's catalog entry.
///
/// Returns `Err(CostCatalogError::Missing)` if the model is unknown.
/// Staleness is NOT enforced here — runtime computation is best-effort
/// (a stale entry is still better than a `null`) and the load-time
/// `validate_for_workflow` is the budget-cap guarantee.
pub fn lookup(model: &str) -> Result<ModelCost, CostCatalogError> {
    CATALOG
        .get(model)
        .cloned()
        .ok_or_else(|| CostCatalogError::Missing {
            model: model.to_string(),
        })
}

/// Compute the USD cost for a single turn given the model and token
/// counts. Multiplies catalog rates by `tokens / 1_000_000` for each
/// of input + output and sums.
///
/// Returns `Err(CostCatalogError::Missing)` if the model is unknown;
/// callers (the executor) log a warning and leave `cost_usd: None`.
pub fn compute_cost_usd(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> Result<f64, CostCatalogError> {
    let entry = lookup(model)?;
    let per_million = 1_000_000_f64;
    let input_cost = (input_tokens as f64) * entry.input_usd_per_million_tokens / per_million;
    let output_cost = (output_tokens as f64) * entry.output_usd_per_million_tokens / per_million;
    Ok(input_cost + output_cost)
}

/// Doctor-side load-time validation per FMECA F8.
///
/// - Model in catalog AND fresh (verified within
///   `STALENESS_THRESHOLD_DAYS` days of `today`) → `Ok(())`.
/// - Model not in catalog AND `has_budget_cap == true` →
///   `Err(CostCatalogError::Missing)`.
/// - Model in catalog but stale AND `has_budget_cap == true` →
///   `Err(CostCatalogError::Stale)`.
/// - Model not in catalog AND `has_budget_cap == false` → `Ok(())`
///   (caller is expected to surface a Warning via [`doctor_check`]).
/// - Model in catalog but stale AND `has_budget_cap == false` →
///   `Ok(())` (same — Warning via [`doctor_check`]).
///
/// `today` is injected so unit tests can drive a deterministic clock
/// without leaning on `chrono::Utc::now()`.
pub fn validate_for_workflow(
    model: &str,
    has_budget_cap: bool,
    today: NaiveDate,
) -> Result<(), CostCatalogError> {
    match lookup(model) {
        Err(CostCatalogError::Missing { model: m }) => {
            if has_budget_cap {
                Err(CostCatalogError::Missing { model: m })
            } else {
                Ok(())
            }
        }
        Err(other) => Err(other),
        Ok(entry) => {
            // Treat malformed `verified_at` constants as infinitely
            // stale — a build-time mistake that should still flag to
            // the operator rather than silently pass.
            let verified = entry
                .verified_at_date()
                .unwrap_or(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or(today));
            let age_days = today.signed_duration_since(verified).num_days();
            if age_days > STALENESS_THRESHOLD_DAYS && has_budget_cap {
                Err(CostCatalogError::Stale {
                    model: model.to_string(),
                    verified_at: entry.verified_at.to_string(),
                    threshold_days: STALENESS_THRESHOLD_DAYS,
                })
            } else {
                Ok(())
            }
        }
    }
}

/// Returns `true` iff the catalog entry for `model` is stale relative
/// to `today` (verified more than `STALENESS_THRESHOLD_DAYS` ago).
/// Returns `false` if the model is missing — callers distinguish via
/// `lookup` if they need to.
pub fn is_stale(model: &str, today: NaiveDate) -> bool {
    let Ok(entry) = lookup(model) else {
        return false;
    };
    let Some(verified) = entry.verified_at_date() else {
        // Malformed constant — flag as stale.
        return true;
    };
    today.signed_duration_since(verified).num_days() > STALENESS_THRESHOLD_DAYS
}

/// Walk every `kind: llm` executor block in a workflow registry JSON
/// (the same shape consumed by `validate::validate_workflows`) and
/// emit per-executor diagnostics for catalog gaps and staleness.
///
/// Discovery semantics:
///
/// - Looks at `config.workflows[*].states[*].transitions[*].executor`
///   AND `config.workflows[*].states[*].onEnter.executor` (the two
///   places an executor block can live in the existing schema).
/// - Skips entries that don't carry `kind: "llm"`.
/// - For entries whose `config.affinity` is set without a `config.model`:
///   if the caller supplies a `resolve_affinity` closure (the binary
///   builds one off `models.yaml`) and it resolves the affinity to a
///   concrete `provider:model`, THAT model is validated against the
///   catalog exactly as a literal `model:` would be (D9 — an uncatalogued
///   affinity-resolved model under a cap now Errors at load). With no
///   closure, or when the closure declines, the entry degrades to the
///   warn-only behavior (load-time can't validate; runtime F8 enforces).
/// - Otherwise extracts `config.model` + `config.max_cost_usd` and
///   calls [`validate_for_workflow`].
///
/// The error/warning shape per the brief:
///
/// - `validate_for_workflow` returns `Err(_)` → `Diagnostic::Error`
///   with the wire code prefix (`COST_CATALOG_MISSING_ENTRY:` /
///   `COST_CATALOG_STALE:`) in the message.
/// - When `has_budget_cap == false`, an unknown OR stale model
///   surfaces as `Diagnostic::Warning`.
///
/// `resolve_affinity` is an optional SYNC closure mapping an `affinity:`
/// string → an optional concrete `"provider:model"`. It keeps this crate
/// decoupled from the `model_resolver` types: the binary builds the
/// closure off `models.yaml` and passes it in. `None` (or a closure that
/// returns `None`) preserves the warn-only affinity fallback.
pub fn doctor_check(
    workflow_registry: &Value,
    today: NaiveDate,
    resolve_affinity: Option<AffinityResolveFn<'_>>,
) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    let Some(workflows) = workflow_registry
        .pointer("/workflows")
        .and_then(Value::as_object)
    else {
        return out;
    };

    // CMP-046 — share core's single executor-site walker rather than
    // re-implementing the states/onEnter/transitions traversal.
    for (wf_id, wf_def) in workflows {
        praxec_core::validate::for_each_executor_site(wf_def, |site| {
            check_executor(
                wf_id,
                site.state.unwrap_or(""),
                site.transition.unwrap_or("onEnter"),
                site.executor,
                today,
                resolve_affinity,
                &mut out,
            );
        });
    }

    out
}

/// Inspect one executor block. Skips non-`llm` kinds and missing model/affinity.
/// An `affinity:`-only config is resolved via the optional `resolve_affinity`
/// closure (D9) and validated as if it were a literal `model:`; when no closure
/// is supplied or the affinity doesn't resolve, it falls back to warn-only.
fn check_executor(
    wf_id: &str,
    state_name: &str,
    site: &str,
    executor: &Value,
    today: NaiveDate,
    resolve_affinity: Option<AffinityResolveFn<'_>>,
    out: &mut Vec<Diagnostic>,
) {
    if executor.get("kind").and_then(Value::as_str) != Some("llm") {
        return;
    }
    // SPEC §33 D9 — the runtime hands the WHOLE `executor:` block to the
    // LLM executor (see lib.rs `execute()`), so `model` and
    // `max_cost_usd` sit at the top level of the executor block in
    // workflow YAML. We accept a legacy nested `config:` wrapper for
    // backward compatibility with any draft authoring that used it.
    let cfg = executor.get("config").unwrap_or(executor);
    let has_budget_cap = cfg.get("max_cost_usd").and_then(Value::as_f64).is_some();
    // Owns the resolved model string (when affinity resolution succeeds)
    // so the `&str` model binding below can borrow from either the JSON
    // value or this local without lifetime juggling.
    let resolved: Option<String>;
    let model = match cfg.get("model").and_then(Value::as_str) {
        Some(m) => m,
        None => {
            let affinity = cfg.get("affinity").and_then(Value::as_str);
            // SPEC §33 D9 — if the binary handed us a SYNC affinity
            // resolver (built off models.yaml) AND it resolves this
            // affinity to a concrete `provider:model`, validate THAT
            // model against the catalog exactly as a literal `model:`
            // would be. An uncatalogued affinity-resolved model under a
            // cap now ERRORS, closing the F8 gap at load time.
            resolved = match (affinity, resolve_affinity) {
                (Some(a), Some(f)) => f(a),
                _ => None,
            };
            match resolved.as_deref() {
                Some(m) => m,
                None => {
                    // No resolver, or it declined to resolve: keep the
                    // warn-only behavior. Load-time can't validate; the
                    // runtime F8 path still enforces. (F6 STUB-009: an
                    // operator who set `max_cost_usd` on an affinity-only
                    // executor must see the gap rather than false
                    // confidence that the cap was load-validated.)
                    if affinity.is_some() && has_budget_cap {
                        out.push(Diagnostic::Warning(format!(
                            "workflow '{wf_id}': state '{state_name}' executor at '{site}' uses \
                             `affinity:` resolution with a `max_cost_usd` cap, but no models.yaml \
                             affinity resolver is configured (or the affinity did not resolve), so \
                             the load-time cost catalog gate cannot validate the resolved model; \
                             the cap will rely on RUNTIME enforcement (FMECA F8 runtime path)"
                        )));
                    }
                    return;
                }
            }
        }
    };
    match validate_for_workflow(model, has_budget_cap, today) {
        Ok(()) => {
            // Even when validation passes, emit a Warning for the no-cap
            // unknown / stale case so the operator can see catalog drift
            // before they ever try to add a `max_cost_usd` cap.
            if lookup(model).is_err() {
                out.push(Diagnostic::Warning(format!(
                    "workflow '{wf_id}': state '{state_name}' executor at '{site}' uses \
                     model '{model}' which is not in the cost catalog; no `max_cost_usd` \
                     cap is set so load is allowed, but cost_usd will be null at runtime"
                )));
            } else if is_stale(model, today) {
                out.push(Diagnostic::Warning(format!(
                    "workflow '{wf_id}': state '{state_name}' executor at '{site}' uses \
                     model '{model}' whose cost catalog entry is older than \
                     {STALENESS_THRESHOLD_DAYS} days; no `max_cost_usd` cap is set so \
                     load is allowed, but pricing may have drifted"
                )));
            }
        }
        Err(CostCatalogError::Missing { model: m }) => {
            out.push(Diagnostic::Error(format!(
                "{COST_CATALOG_MISSING_ENTRY}: workflow '{wf_id}' state '{state_name}' \
                 executor at '{site}' uses model '{m}' which is not in the cost catalog; \
                 `max_cost_usd` is set so this would silently bypass budget enforcement \
                 (SPEC §33 FMECA F8)"
            )));
        }
        Err(CostCatalogError::Stale {
            model: m,
            verified_at,
            threshold_days,
        }) => {
            out.push(Diagnostic::Error(format!(
                "{COST_CATALOG_STALE}: workflow '{wf_id}' state '{state_name}' executor \
                 at '{site}' uses model '{m}' whose cost catalog entry is older than \
                 {threshold_days} days (verified {verified_at}); `max_cost_usd` is set \
                 so silently outdated pricing would weaken budget enforcement \
                 (SPEC §33 FMECA F8)"
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_verified_parses_as_date() {
        NaiveDate::parse_from_str(LAST_VERIFIED, "%Y-%m-%d")
            .expect("LAST_VERIFIED must be ISO 8601 YYYY-MM-DD");
    }

    #[test]
    fn shipped_catalog_parses_and_dates_are_valid() {
        // Accessing CATALOG forces the data-file load (panics if the shipped
        // default JSON is malformed — a build-time mistake caught here).
        assert!(
            !CATALOG.is_empty(),
            "the shipped cost catalog must not be empty"
        );
        for (model, cost) in CATALOG.iter() {
            assert!(
                cost.verified_at_date().is_some(),
                "{model}: verified_at '{}' is not ISO 8601",
                cost.verified_at
            );
        }
    }

    #[test]
    fn compute_cost_usd_for_zero_tokens_is_zero() {
        let result = compute_cost_usd("anthropic:claude-sonnet-4-6", 0, 0)
            .expect("known model with zero tokens must compute");
        assert!(result.abs() < f64::EPSILON);
    }

    #[test]
    fn validate_for_workflow_missing_no_cap_is_ok() {
        let today = NaiveDate::from_ymd_opt(2026, 5, 29).expect("static date");
        validate_for_workflow("vendor:unknown", false, today)
            .expect("unknown model without budget cap must pass");
    }
}
