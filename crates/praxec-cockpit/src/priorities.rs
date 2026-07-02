//! **What matters most to you** — the user's recommendation *stance*, the lens
//! every model recommendation is made through (ADR-0005 / the semantic-catalog
//! design).
//!
//! No two engineers want the same thing: some are cost-blind and quality-maximal,
//! some are on a tight budget, some need fast turns. Rather than ask users to
//! introspect numeric weights they don't have (constructed-preference theory says
//! those self-reports are noise), we offer a few **stances they recognise** plus
//! the two **hard constraints** that actually filter the field — a budget ceiling
//! and local-only/private. The stance *ranks*; the constraints *filter*. Pick a
//! stance, watch the recommendation move; that's preference revealed by choice,
//! not elicited as a form.
//!
//! Set once at first run (skippable → `Balanced`), then editable forever from
//! Settings. Persisted next to the other cockpit choices.

use praxec_embeddings::CostMagnitude;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The four recognisable stances. Each is a different utility function over the
/// same model metrics (capability / cost / speed) — see `crate::chat_catalog`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Stance {
    /// A strong model at a fair price — best value above a reliability bar.
    #[default]
    Balanced,
    /// Capability first; cost is secondary.
    BestResults,
    /// The cheapest model that still does the job.
    KeepCostsLow,
    /// Fastest responses.
    Fastest,
}

impl Stance {
    pub const ALL: [Stance; 4] = [
        Stance::Balanced,
        Stance::BestResults,
        Stance::KeepCostsLow,
        Stance::Fastest,
    ];

    /// Short label for the menu.
    pub fn label(self) -> &'static str {
        match self {
            Stance::Balanced => "Balanced",
            Stance::BestResults => "Best results",
            Stance::KeepCostsLow => "Keep costs low",
            Stance::Fastest => "Fastest responses",
        }
    }

    /// One-line gloss shown beside the label.
    pub fn blurb(self) -> &'static str {
        match self {
            Stance::Balanced => "a strong model at a fair price",
            Stance::BestResults => "capability first, cost aside",
            Stance::KeepCostsLow => "cheapest that does the job",
            Stance::Fastest => "speed first",
        }
    }

    /// The value-selection knobs `(price_sensitivity β, marginal_band ε)` this
    /// stance feeds to
    /// [`suggest_by_value`](praxec_core::model_catalog::suggest_by_value),
    /// which scores `value = fit ÷ blended_cost^β` and, within `ε` of the best
    /// value, prefers the *more capable* model.
    ///
    /// - `BestResults` → `(0.0, 0.0)` — β=0 cancels cost entirely, so the most
    ///   capable model wins outright.
    /// - `Balanced` → `(0.5, 0.15)` — a strong model at a fair price (the tuning
    ///   defaults): cost matters, but a marginally stronger model still wins.
    /// - `KeepCostsLow` → `(1.5, 0.05)` — heavy cost weight + a tight band: the
    ///   cheapest model that still clears the capability floor.
    ///
    /// [`Stance::Fastest`] is **orthogonal** to this capability/cost value axis —
    /// it ranks on raw `speed_tps` and never routes through `suggest_by_value`, so
    /// it has no value params (returns `None`).
    pub fn value_params(self) -> Option<(f64, f64)> {
        match self {
            Stance::BestResults => Some((0.0, 0.0)),
            Stance::Balanced => Some((0.5, 0.15)),
            Stance::KeepCostsLow => Some((1.5, 0.05)),
            Stance::Fastest => None,
        }
    }
}

/// The budget-ceiling choices the panel cycles through (low→high; `None` = no
/// cap). A coarse ladder — orders of magnitude, the same scale the cost is shown
/// in — not a dollar field.
pub const BUDGET_CAPS: &[Option<CostMagnitude>] = &[
    None,
    Some(CostMagnitude::TensOfCents),
    Some(CostMagnitude::Dollars),
    Some(CostMagnitude::TensOfDollars),
    Some(CostMagnitude::HundredsOfDollars),
    Some(CostMagnitude::ThousandsOfDollars),
];

/// Human label for a budget ceiling (`None` → "no cap").
pub fn budget_label(cap: Option<CostMagnitude>) -> String {
    match cap {
        None => "no cap".to_string(),
        Some(m) => format!("≤ {}", m.label()),
    }
}

/// The full stance + the two hard constraints. This is what persists and what the
/// recommenders consult.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Priorities {
    pub stance: Stance,
    /// Never recommend a model whose cost exceeds this magnitude (`None` = no cap).
    /// A *hard filter*, evaluated at the user's chosen request volume.
    #[serde(default)]
    pub budget_cap: Option<CostMagnitude>,
    /// Only recommend models that run locally (privacy / air-gapped).
    #[serde(default)]
    pub local_only: bool,
}

impl Priorities {
    /// On-disk path. Precedence: `$PRAXEC_PRIORITIES_FILE`, then
    /// `~/.praxec/priorities.json`, then a CWD fallback.
    pub fn path() -> PathBuf {
        if let Ok(p) = std::env::var("PRAXEC_PRIORITIES_FILE") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        match dirs::home_dir() {
            Some(d) => d.join(".praxec").join("priorities.json"),
            None => PathBuf::from("praxec-priorities.json"),
        }
    }

    /// Load the persisted stance, or `None` (not yet decided → show the panel).
    pub fn load() -> Option<Priorities> {
        let raw = std::fs::read_to_string(Self::path()).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Persist (creating the config dir if needed).
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_stance_is_balanced() {
        assert_eq!(Priorities::default().stance, Stance::Balanced);
        assert!(Priorities::default().budget_cap.is_none());
        assert!(!Priorities::default().local_only);
    }

    #[test]
    fn round_trips_through_json() {
        let p = Priorities {
            stance: Stance::KeepCostsLow,
            budget_cap: Some(CostMagnitude::Dollars),
            local_only: true,
        };
        let raw = serde_json::to_string(&p).unwrap();
        let back: Priorities = serde_json::from_str(&raw).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn value_params_map_each_capability_cost_stance_and_fastest_is_orthogonal() {
        // β=0 cancels cost ⇒ most capable; tighter band + heavier β ⇒ cheaper.
        assert_eq!(Stance::BestResults.value_params(), Some((0.0, 0.0)));
        assert_eq!(Stance::Balanced.value_params(), Some((0.5, 0.15)));
        assert_eq!(Stance::KeepCostsLow.value_params(), Some((1.5, 0.05)));
        // Speed is orthogonal to the value axis: no value params.
        assert_eq!(Stance::Fastest.value_params(), None);
    }

    #[test]
    fn all_stances_have_distinct_labels() {
        let labels: Vec<_> = Stance::ALL.iter().map(|s| s.label()).collect();
        let mut deduped = labels.clone();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(labels.len(), deduped.len());
    }
}
