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
//!
//! # D7 — the learned selector policy (v0.0.18, mechanism #3)
//!
//! The evidence term above *decorates* a candidate with its success rate. The
//! **policy** ([`SelectorPolicy`]) turns that accrued evidence into an active
//! selection rule: once a `(task_class, template)` pair has enough evidence
//! runs, its evidence component stops being the bare success rate and becomes
//! a **cost-adjusted value**
//!
//! ```text
//! value = success_rate × (1 − POLICY_COST_WEIGHT × cost_premium)
//! cost_premium = 1 − (cheapest_proven_cost_in_class / this_template's_mean_cost)
//! ```
//!
//! so that among templates *proven* for the same task-class, a cheaper one is
//! preferred at equal success, and an expensive one must earn its price in
//! success rate. It is a deterministic function over recorded evidence — no
//! model, no training, no fitted weights. Ask it "why did A win?" and it
//! answers with `n` runs, a success rate, and a mean cost.
//!
//! **The cold-start guard is the deliverable, as much as the policy is**
//! (plan risk #2: a policy that acts on thin evidence selects *worse* than the
//! evidence-annotation it replaces). Below [`SelectorPolicy::min_runs`] the
//! policy does not activate and ranking is byte-for-byte the pre-D7 ranking:
//! same scores, same order, same `why`. A fresh install — no evidence — sees
//! **zero** behavioural change. Activation is never silent: an activated
//! candidate's `why` names the policy, the evidence volume that cleared the
//! bar, and the success/cost the decision rests on. And because the threshold
//! is checked per `(task_class, template)`, a well-evidenced template can be
//! policy-ranked while a thin one falls through *in the same query*.

use std::collections::BTreeMap;

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

/// D7 — how far cost may discount a *proven* success rate. `0.25`: the
/// dearest proven template in a task-class forfeits at most a quarter of its
/// success rate, so a rival must be within ~25% relative success to beat it on
/// price alone. Cost trades against success; it never overturns a wide success
/// gap (a 90%-success template is not displaced by a 40% one for being cheap).
///
/// Deliberately a constant, not a knob: the plan mandates exactly one tunable
/// for this policy — the *activation* bar ([`SelectorPolicy::min_runs`]) — and
/// the trade-off itself is the policy's published, auditable rule. A knob here
/// would make the ranking unexplainable across installs for no decision the
/// operator actually needs to make.
pub const POLICY_COST_WEIGHT: f64 = 0.25;

/// The cost premium assigned to a policy-active template whose evidence runs
/// were never priced (no `agent.completed` cost recorded). The midpoint, for
/// the same reason as [`NEUTRAL_EVIDENCE_COMPONENT`]: an unpriced template is
/// neither the cheapest (a reward it did not earn) nor the dearest (a penalty
/// the evidence does not support). Absence is never fabricated into a number.
pub const NEUTRAL_COST_PREMIUM: f64 = 0.5;

/// The D7 policy's one tunable: the **evidence-volume threshold** at which the
/// learned policy takes over ranking for a `(task_class, template)` pair.
///
/// Below it the pair falls through to the pre-policy blend, unchanged — which
/// is the whole guard against plan risk #2. `min_runs` is compared against the
/// pair's *evidence runs* (missions with ≥1 declared outcome — see
/// [`IntentStats::evidence_runs`](crate::intent_index::IntentStats)), the same
/// currency the annotator's `intent.min_runs` gate counts.
///
/// Poka-yoke: the annotator has *already* dropped any pair below
/// `intent.min_runs`, so a `policy_min_runs` set *below* the annotation bar
/// cannot make the policy act on evidence the system refuses to show — the
/// stricter of the two always wins, by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorPolicy {
    /// Evidence runs a `(task_class, template)` pair must have before the
    /// policy re-ranks it. Configured by `intent.policy_min_runs` in the
    /// tuning file. Clamped to ≥1 at use: a `0` threshold must never be read
    /// as "act on no evidence".
    pub min_runs: usize,
}

impl SelectorPolicy {
    /// Load the threshold from the active tuning (override-aware) — the
    /// production entry point, mirroring
    /// [`IntentParams::from_tuning`](crate::intent_index::IntentParams::from_tuning).
    pub fn from_tuning() -> Self {
        Self {
            min_runs: crate::tuning::tuning().intent.policy_min_runs,
        }
    }

    /// The kill switch: a threshold no evidence volume can clear, so ranking
    /// is exactly the pre-D7 (v0.0.17) evidence-annotation blend. This is what
    /// `intent.policy_min_runs` set arbitrarily high buys you, named.
    pub fn disabled() -> Self {
        Self {
            min_runs: usize::MAX,
        }
    }

    /// Does this pair's evidence volume clear the activation bar?
    fn activates_at(&self, runs: usize) -> bool {
        runs >= self.min_runs.max(1)
    }
}

/// The D7 policy signal — present on a [`RankedCandidate`] **only** when the
/// policy actually fired. Carries the evidence the decision rests on, so an
/// activated re-rank is auditable rather than oracular.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PolicySignal {
    /// Evidence runs that cleared the threshold.
    pub runs: usize,
    /// The *effective* threshold they cleared — [`SelectorPolicy::min_runs`]
    /// after the ≥1 clamp. Echoed so the `why` is self-contained.
    pub min_runs: usize,
    pub success_rate: f64,
    /// Mean realized USD over this pair's priced runs; `None` when unpriced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_cost_usd: Option<f64>,
    /// `[0, 1]` — how much dearer this template is than the cheapest *proven*
    /// template for the same task-class. `0` = it IS the cheapest; → `1` as
    /// its cost grows without bound. [`NEUTRAL_COST_PREMIUM`] when unpriced.
    pub cost_premium: f64,
    /// The cost-adjusted value that REPLACES the plain success-rate evidence
    /// component: `success_rate × (1 − POLICY_COST_WEIGHT × cost_premium)`.
    pub value: f64,
}

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
    /// `RELEVANCE_WEIGHT·relevance + EVIDENCE_WEIGHT·evidence_component + TOPOLOGY_WEIGHT·topology`,
    /// where the evidence term is [`Self::evidence_component`] — the D7 policy
    /// value when the policy fired, the plain success rate otherwise.
    pub score: f64,
    /// Lexical/semantic relevance normalized to `[0, 1]` against the best
    /// hit in this batch.
    pub relevance: f64,
    pub evidence: EvidenceSignal,
    pub topology: TopologySignal,
    /// D7 — present **only** when the learned policy actually fired for this
    /// candidate (evidence at/above [`SelectorPolicy::min_runs`]). `None` is
    /// the cold-start/fall-through state, in which this candidate's score and
    /// `why` are exactly what pre-D7 ranking produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<PolicySignal>,
    /// The rendered explanation of the score arithmetic — deterministic for
    /// identical inputs.
    pub why: String,
}

impl RankedCandidate {
    /// The `[0, 1]` evidence term that actually entered this candidate's score:
    /// the D7 policy's cost-adjusted value when the policy fired, else the
    /// pre-policy evidence component. Note the policy replaces *what the
    /// evidence term says*, never how much it weighs — [`EVIDENCE_WEIGHT`] is
    /// the same in both regimes, so activation can reorder candidates but
    /// cannot let evidence outshout relevance.
    pub fn evidence_component(&self) -> f64 {
        match &self.policy {
            Some(p) => p.value,
            None => self.evidence.component(),
        }
    }
}

/// The bucket a hit's evidence lands in when it declares no `process:`
/// task-class — the same key [`aggregate`](crate::intent_index::aggregate)
/// rolls unclassified observations under, so the policy's cohort and the
/// index's cohort are the same cohort.
const UNCLASSIFIED_CLASS: &str = "(unclassified)";

/// What the D7 policy may do with one candidate. Decided in a first pass over
/// the batch, before scoring, because the cost term is a *comparison* and the
/// cohort it compares within must be built from the eligible set alone.
enum Candidacy<'h> {
    /// Evidence clears the volume bar and is usable — the policy will rank it.
    Eligible {
        /// The `(task_class, template)` cohort this evidence was accrued under.
        class: &'h str,
        runs: usize,
        success_rate: f64,
        /// Mean realized USD; `None` ⇒ no priced run ⇒ neutral premium.
        cost: Option<f64>,
    },
    /// Evidence clears the volume bar but is *unusable* (non-finite rate, a
    /// negative cost — corruption nothing legitimate produces). The policy
    /// declines to act and the reason is rendered into `why`: a fall-through
    /// on bad data is never silent, and a bad number is never repaired into a
    /// plausible one.
    Withheld(&'static str),
    /// No evidence, or evidence below the threshold — **the cold-start path**.
    /// Carries nothing on purpose: scoring and `why` must be byte-identical to
    /// pre-D7 here (plan risk #2).
    Inactive,
}

/// Decide what the policy may do with one candidate's evidence.
fn classify<'h>(
    hit: &'h SearchHit,
    evidence: &EvidenceSignal,
    policy: &SelectorPolicy,
) -> Candidacy<'h> {
    let EvidenceSignal::Proven {
        runs,
        success_rate,
        mean_cost_usd,
    } = evidence
    else {
        return Candidacy::Inactive;
    };
    if !policy.activates_at(*runs) {
        // THE GUARD. Thin evidence ⇒ the policy does not exist for this pair.
        return Candidacy::Inactive;
    }
    if !success_rate.is_finite() {
        return Candidacy::Withheld("unusable success_rate");
    }
    let cost = match mean_cost_usd {
        // A value that is not a finite, non-negative number is not a cost.
        Some(c) if !c.is_finite() || *c < 0.0 => {
            return Candidacy::Withheld("unusable mean_cost_usd");
        }
        other => *other,
    };
    Candidacy::Eligible {
        class: hit.item.task_class().unwrap_or(UNCLASSIFIED_CLASS),
        runs: *runs,
        success_rate: *success_rate,
        cost,
    }
}

/// How much dearer this template is than the cheapest *proven* template in its
/// task-class: `1 − cheapest/cost`, in `[0, 1)`. Ratio-based, not min-max, so
/// the discount tracks the *magnitude* of the premium — being 10% dearer costs
/// ~10% of the trade-off, being 50× dearer nearly all of it — and the cheapest
/// template is never penalized. Unpriced ⇒ [`NEUTRAL_COST_PREMIUM`].
fn cost_premium(cost: Option<f64>, cheapest: Option<f64>) -> f64 {
    match (cost, cheapest) {
        // A free template (cost 0) cannot be dearer than anything.
        (Some(c), Some(cheapest)) if c > 0.0 => (1.0 - cheapest / c).clamp(0.0, 1.0),
        (Some(_), _) => 0.0,
        (None, _) => NEUTRAL_COST_PREMIUM,
    }
}

/// Rank discovery candidates by relevance + evidence + topology, under the D7
/// learned selector policy.
///
/// The public D6/D7 entry point. `hits` are search results as produced by a
/// [`DiscoveryIndex`](crate::discovery::DiscoveryIndex) and (for evidence)
/// already annotated by
/// [`annotate_hits_with_evidence`](crate::intent_index::annotate_hits_with_evidence);
/// `registry` is the loaded D4b [`Registry`] topology, or `None` when no
/// registry is configured (every candidate then gets a uniform zero
/// topology term — no reordering, no penalty); `policy` is the D7 activation
/// threshold, normally [`SelectorPolicy::from_tuning`] (the caller supplies it
/// exactly as `handlers.rs` supplies `IntentParams::from_tuning().min_runs` to
/// the annotator — the ranking math itself stays a pure function of its
/// arguments, so both sides of the threshold are testable).
///
/// Guarantees:
/// - **Deterministic**: same inputs ⇒ same order, same scores, same `why`
///   strings. Ties break by evidence `mean_cost_usd` ascending (unpriced
///   last), then id ascending.
/// - **Total**: one output per input hit — annotation, never gatekeeping.
/// - **No fabricated evidence**: a hit without attached evidence scores the
///   neutral midpoint, never a success rate.
/// - **Cold-start safe**: with every candidate below `policy.min_runs` the
///   output is byte-for-byte the pre-D7 ranking — scores, order, and `why`.
pub fn rank_candidates(
    hits: &[SearchHit],
    registry: Option<&Registry>,
    policy: &SelectorPolicy,
) -> Vec<RankedCandidate> {
    let max_relevance = hits
        .iter()
        .map(|h| f64::from(h.score))
        .fold(0.0_f64, f64::max);

    let evidence: Vec<EvidenceSignal> = hits
        .iter()
        .map(|hit| match &hit.evidence {
            Some(ev) => EvidenceSignal::Proven {
                runs: ev.runs,
                success_rate: ev.success_rate,
                mean_cost_usd: ev.mean_cost_usd,
            },
            None => EvidenceSignal::Absent,
        })
        .collect();

    // D7 pass 1 — who may the policy act on? (Per `(task_class, template)`, so
    // a well-evidenced template is policy-ranked while a thin one falls through
    // in the SAME batch.)
    let candidacy: Vec<Candidacy<'_>> = hits
        .iter()
        .zip(&evidence)
        .map(|(hit, ev)| classify(hit, ev, policy))
        .collect();

    // D7 pass 2 — the cheapest proven template per task-class cohort: the
    // baseline every premium is measured against. "Cheaper" is only meaningful
    // among templates competing for the same task-class, which is exactly the
    // key the intent index accrues evidence under. With no contrast to exploit
    // (a lone proven template, or none priced) the cheapest IS the candidate,
    // its premium is 0, and the policy degenerates to the plain success rate.
    let mut cheapest: BTreeMap<&str, f64> = BTreeMap::new();
    for c in &candidacy {
        if let Candidacy::Eligible {
            class,
            cost: Some(cost),
            ..
        } = c
        {
            let slot = cheapest.entry(class).or_insert(*cost);
            if cost < slot {
                *slot = *cost;
            }
        }
    }

    let mut ranked: Vec<RankedCandidate> = hits
        .iter()
        .zip(evidence)
        .zip(&candidacy)
        .map(|((hit, evidence), candidacy)| {
            let relevance = if max_relevance > 0.0 {
                (f64::from(hit.score) / max_relevance).clamp(0.0, 1.0)
            } else {
                0.0
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
                // A tool is the *other* column of the crossmatrix, never the
                // `workflow` one: asking "which tools does this tool link to"
                // is not a question the topology answers.
                DiscoveryKind::Capability
                | DiscoveryKind::Connection
                | DiscoveryKind::Guidance
                | DiscoveryKind::Script
                | DiscoveryKind::Agent
                | DiscoveryKind::Tool => Vec::new(),
            };
            let topology = TopologySignal::from_linked_tools(linked_tools);

            // The policy value REPLACES the evidence component for an activated
            // candidate — same weight, different (cost-adjusted) claim. For
            // everyone else this is the pre-D7 component, unchanged.
            let (policy_signal, withheld) = match candidacy {
                Candidacy::Eligible {
                    class,
                    runs,
                    success_rate,
                    cost,
                } => {
                    let premium = cost_premium(*cost, cheapest.get(class).copied());
                    let value = success_rate.clamp(0.0, 1.0) * (1.0 - POLICY_COST_WEIGHT * premium);
                    (
                        Some(PolicySignal {
                            runs: *runs,
                            // The *effective* bar (clamped), not the raw config
                            // — the `why` must state the threshold that was
                            // actually applied, not the one on paper.
                            min_runs: policy.min_runs.max(1),
                            success_rate: *success_rate,
                            mean_cost_usd: *cost,
                            cost_premium: premium,
                            value,
                        }),
                        None,
                    )
                }
                Candidacy::Withheld(reason) => (None, Some(*reason)),
                Candidacy::Inactive => (None, None),
            };
            let evidence_component = match &policy_signal {
                Some(p) => p.value,
                None => evidence.component(),
            };

            let score = RELEVANCE_WEIGHT * relevance
                + EVIDENCE_WEIGHT * evidence_component
                + TOPOLOGY_WEIGHT * topology.component;
            let why = render_why(
                relevance,
                &evidence,
                policy_signal.as_ref(),
                withheld,
                &topology,
                score,
            );

            RankedCandidate {
                id: hit.item.id.clone(),
                kind: hit.item.kind,
                score,
                relevance,
                evidence,
                topology,
                policy: policy_signal,
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
///
/// With `policy: None` and `withheld: None` this emits *exactly* the pre-D7
/// line — the cold-start guarantee reaches the explanation too, not just the
/// number.
fn render_why(
    relevance: f64,
    evidence: &EvidenceSignal,
    policy: Option<&PolicySignal>,
    withheld: Option<&'static str>,
    topology: &TopologySignal,
    score: f64,
) -> String {
    // An activated candidate's evidence term IS the policy — say so, with the
    // evidence it rests on (runs, threshold cleared, success, cost), so a
    // re-ranking is never silent.
    let evidence_part = if let Some(p) = policy {
        let cost = match p.mean_cost_usd {
            Some(c) => format!(", mean ${c:.4}, cost premium {:.3}", p.cost_premium),
            None => format!(", unpriced (neutral cost premium {:.3})", p.cost_premium),
        };
        format!(
            "policy {:.3} (learned policy active: {} evidence run(s) ≥ threshold {}, {:.1}% success{cost})",
            p.value,
            p.runs,
            p.min_runs,
            p.success_rate * 100.0,
        )
    } else {
        match evidence {
            EvidenceSignal::Proven {
                runs,
                success_rate,
                mean_cost_usd,
            } => {
                let cost = match mean_cost_usd {
                    Some(c) => format!(", mean ${c:.4}"),
                    None => String::new(),
                };
                // A fall-through on *unusable* evidence says so; a fall-through on
                // *thin* evidence is the designed cold-start path and must keep the
                // pre-D7 line byte-identical.
                let declined = match withheld {
                    Some(reason) => format!(" [learned policy withheld: {reason}]"),
                    None => String::new(),
                };
                format!(
                    "evidence {:.3} ({runs} evidence run(s), {:.1}% success{cost}){declined}",
                    evidence.component(),
                    success_rate * 100.0,
                )
            }
            EvidenceSignal::Absent => format!(
                "evidence {NEUTRAL_EVIDENCE_COMPONENT:.3} (none at/above min_runs — neutral, not failure)"
            ),
        }
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
