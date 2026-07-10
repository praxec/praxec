//! D6 — the selector: deterministic, explainable annotate/rank over discovery
//! candidates (docs/design-0.0.17-tool-source-ecosystem.md §D6).
//!
//! Combines THREE already-computed signals into one auditable score:
//!
//! 1. **Relevance** — the search hit's lexical/semantic score, normalized
//!    against the best hit in the batch.
//! 2. **Evidence** — the intent-index track record already attached to
//!    `kind: workflow` hits by
//!    [`annotate_hits_with_evidence`](crate::intent_index::annotate_hits_with_evidence).
//!    That annotator enforces the tuning `intent.min_runs` gate, so a present
//!    [`IntentEvidence`](crate::intent_index::IntentEvidence) has *already*
//!    cleared the evidence bar; a thin/absent sample arrives here as `None`
//!    and contributes a **neutral** term — absence of evidence is never read
//!    as failure, and the selector never fabricates a rate (FM6).
//! 3. **Topology** — the D4b registry crossmatrix: a candidate workflow that
//!    registry tools suggest (via `suggested_workflows` / crossmatrix edges)
//!    gets a saturating boost, and the linked tool ids are surfaced as an
//!    annotation.
//!
//! The rank is a **pure function** — a compiled tool doing the math, never an
//! LLM judgment (deterministic tools own selection; models own generation).
//! Each [`RankedCandidate`] carries its component sub-scores plus a rendered
//! `why` string, so the ordering is auditable end to end. And following the
//! shipped "annotation, never a filter" pattern
//! (`handlers.rs::attach_intent_evidence`), ranking never drops a candidate:
//! the output is a permutation of the input, one ranked entry per hit.

use serde::Serialize;

use crate::discovery::{DiscoveryKind, SearchHit};
use crate::registry_v3::Registry;

/// Weight of the normalized lexical/semantic relevance component.
pub const RELEVANCE_WEIGHT: f64 = 0.5;
/// Weight of the intent-index evidence component.
pub const EVIDENCE_WEIGHT: f64 = 0.3;
/// Weight of the registry-topology component.
pub const TOPOLOGY_WEIGHT: f64 = 0.2;

/// The evidence component contributed when a candidate has NO evidence at or
/// above the annotator's `min_runs` gate. Deliberately the midpoint — an
/// unproven candidate is neither rewarded as proven-good (a proven
/// `success_rate` above this lifts a template) nor punished as proven-bad
/// (a proven `success_rate` below this sinks one). Never `0.0`: that would
/// read absence as a 0% success rate, which is exactly the fabricated
/// evidence FM6 forbids.
pub const NEUTRAL_EVIDENCE_COMPONENT: f64 = 0.5;

/// Linked-tool count at which the topology component saturates at `1.0`.
/// Adoption beyond this many registry tools stops adding signal — the
/// boost rewards "composed by the ecosystem", not raw fan-in.
pub const TOPOLOGY_SATURATION: usize = 3;

/// The evidence signal for one candidate — a closed enum: either the
/// intent-index track record that cleared the annotator's `min_runs` gate,
/// or `Absent`. There is no "thin" variant on purpose: the annotator omits
/// sub-`min_runs` samples entirely, so by construction the selector can
/// never mistake noise for evidence (FM6).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EvidenceSignal {
    /// The candidate has `runs >= min_runs` evidence runs.
    Proven {
        runs: usize,
        success_rate: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        mean_cost_usd: Option<f64>,
    },
    /// No evidence cleared the bar — the normal state of a fresh system.
    Absent,
}

impl EvidenceSignal {
    /// The `[0, 1]` evidence component: the proven success rate, or the
    /// neutral midpoint when absent.
    pub fn component(&self) -> f64 {
        match self {
            EvidenceSignal::Proven { success_rate, .. } => success_rate.clamp(0.0, 1.0),
            EvidenceSignal::Absent => NEUTRAL_EVIDENCE_COMPONENT,
        }
    }

    /// The mean realized cost used as the deterministic tie-breaker
    /// (`success_rate desc, mean_cost_usd asc` among evidence-clearing
    /// candidates, per §D6). Unpriced/absent sorts after priced.
    fn tie_break_cost(&self) -> f64 {
        match self {
            EvidenceSignal::Proven {
                mean_cost_usd: Some(cost),
                ..
            } => *cost,
            EvidenceSignal::Proven {
                mean_cost_usd: None,
                ..
            }
            | EvidenceSignal::Absent => f64::INFINITY,
        }
    }
}

/// The registry-topology signal for one candidate: which registry tools link
/// to it (via `suggested_workflows` / crossmatrix edges) and the saturating
/// `[0, 1]` component that count produces.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopologySignal {
    /// Registry tool ids linked to this candidate, in registry `tools`
    /// order — the annotation a caller uses to see *what composes this
    /// workflow*.
    pub linked_tools: Vec<String>,
    /// `min(linked_tools.len(), TOPOLOGY_SATURATION) / TOPOLOGY_SATURATION`.
    pub component: f64,
}

impl TopologySignal {
    fn from_linked_tools(linked_tools: Vec<String>) -> Self {
        let component =
            linked_tools.len().min(TOPOLOGY_SATURATION) as f64 / TOPOLOGY_SATURATION as f64;
        Self {
            linked_tools,
            component,
        }
    }
}

/// One ranked, annotated candidate. Carries every component that produced
/// its position — the WHY, not just the order — so a caller (human or model)
/// can audit the selection instead of trusting a bare number.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RankedCandidate {
    /// The discovery item id (for workflows, the `definition_id`).
    pub id: String,
    pub kind: DiscoveryKind,
    /// The combined score:
    /// `RELEVANCE_WEIGHT·relevance + EVIDENCE_WEIGHT·evidence + TOPOLOGY_WEIGHT·topology`.
    pub score: f64,
    /// Lexical/semantic relevance normalized to `[0, 1]` against the best
    /// hit in this batch.
    pub relevance: f64,
    pub evidence: EvidenceSignal,
    pub topology: TopologySignal,
    /// The rendered explanation of the score arithmetic — deterministic for
    /// identical inputs.
    pub why: String,
}

/// Rank discovery candidates by relevance + evidence + topology.
///
/// The public D6 entry point. `hits` are search results as produced by a
/// [`DiscoveryIndex`](crate::discovery::DiscoveryIndex) and (for evidence)
/// already annotated by
/// [`annotate_hits_with_evidence`](crate::intent_index::annotate_hits_with_evidence);
/// `registry` is the loaded D4b [`Registry`] topology, or `None` when no
/// registry is configured (every candidate then gets a uniform zero
/// topology term — no reordering, no penalty).
///
/// Guarantees:
/// - **Deterministic**: same inputs ⇒ same order, same scores, same `why`
///   strings. Ties break by evidence `mean_cost_usd` ascending (unpriced
///   last), then id ascending.
/// - **Total**: one output per input hit — annotation, never gatekeeping.
/// - **No fabricated evidence**: a hit without attached evidence scores the
///   neutral midpoint, never a success rate.
pub fn rank_candidates(hits: &[SearchHit], registry: Option<&Registry>) -> Vec<RankedCandidate> {
    let max_relevance = hits
        .iter()
        .map(|h| f64::from(h.score))
        .fold(0.0_f64, f64::max);

    let mut ranked: Vec<RankedCandidate> = hits
        .iter()
        .map(|hit| {
            let relevance = if max_relevance > 0.0 {
                (f64::from(hit.score) / max_relevance).clamp(0.0, 1.0)
            } else {
                0.0
            };

            let evidence = match &hit.evidence {
                Some(ev) => EvidenceSignal::Proven {
                    runs: ev.runs,
                    success_rate: ev.success_rate,
                    mean_cost_usd: ev.mean_cost_usd,
                },
                None => EvidenceSignal::Absent,
            };

            // Only workflow candidates appear in the crossmatrix `workflow`
            // column — exhaustive match so a new discovery kind forces a
            // deliberate topology decision here.
            let linked_tools: Vec<String> = match hit.item.kind {
                DiscoveryKind::Workflow => registry
                    .map(|r| {
                        r.tools_for_workflow(&hit.item.id)
                            .into_iter()
                            .map(|tool| tool.id.clone())
                            .collect()
                    })
                    .unwrap_or_default(),
                DiscoveryKind::Capability
                | DiscoveryKind::Connection
                | DiscoveryKind::Guidance
                | DiscoveryKind::Script
                | DiscoveryKind::Agent => Vec::new(),
            };
            let topology = TopologySignal::from_linked_tools(linked_tools);

            let score = RELEVANCE_WEIGHT * relevance
                + EVIDENCE_WEIGHT * evidence.component()
                + TOPOLOGY_WEIGHT * topology.component;
            let why = render_why(relevance, &evidence, &topology, score);

            RankedCandidate {
                id: hit.item.id.clone(),
                kind: hit.item.kind,
                score,
                relevance,
                evidence,
                topology,
                why,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| {
                a.evidence
                    .tie_break_cost()
                    .total_cmp(&b.evidence.tie_break_cost())
            })
            .then_with(|| a.id.cmp(&b.id))
    });
    ranked
}

/// Render the score arithmetic as one deterministic line.
fn render_why(
    relevance: f64,
    evidence: &EvidenceSignal,
    topology: &TopologySignal,
    score: f64,
) -> String {
    let evidence_part = match evidence {
        EvidenceSignal::Proven {
            runs,
            success_rate,
            mean_cost_usd,
        } => {
            let cost = match mean_cost_usd {
                Some(c) => format!(", mean ${c:.4}"),
                None => String::new(),
            };
            format!(
                "evidence {:.3} ({runs} evidence run(s), {:.1}% success{cost})",
                evidence.component(),
                success_rate * 100.0,
            )
        }
        EvidenceSignal::Absent => format!(
            "evidence {NEUTRAL_EVIDENCE_COMPONENT:.3} (none at/above min_runs — neutral, not failure)"
        ),
    };
    let topology_part = if topology.linked_tools.is_empty() {
        format!(
            "topology {:.3} (no registry tool links)",
            topology.component
        )
    } else {
        format!(
            "topology {:.3} ({} registry tool(s): {})",
            topology.component,
            topology.linked_tools.len(),
            topology.linked_tools.join(", "),
        )
    };
    format!(
        "relevance {relevance:.3}×{RELEVANCE_WEIGHT} + {evidence_part}×{EVIDENCE_WEIGHT} \
         + {topology_part}×{TOPOLOGY_WEIGHT} = {score:.3}"
    )
}
