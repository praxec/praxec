//! D7 — the learned selector policy: intent-index evidence stops decorating
//! the ranking and starts driving it, above a configurable evidence-volume
//! threshold.
//!
//! The deliverable is really two things, and this file tests both:
//!
//! 1. **The policy** — among templates *proven* for the same task-class, the
//!    evidence component becomes a cost-adjusted value,
//!    `success_rate × (1 − POLICY_COST_WEIGHT × cost_premium)`, so a cheaper
//!    template is preferred at equal success and a dear one must earn its
//!    price. Deterministic arithmetic over recorded evidence; no model.
//! 2. **The cold-start guard** (plan risk #2) — below the threshold the policy
//!    does not exist: same scores, same order, same `why` as pre-D7. That is
//!    the property most of these cases attack, because a policy that fires on
//!    thin evidence selects *worse* than the annotation it replaces.

use praxec_core::audit::AuditEvent;
use praxec_core::cost_report::AGENT_COMPLETED;
use praxec_core::discovery::{
    DiscoveryItem, DiscoveryKind, EVIDENCE_WEIGHT, POLICY_COST_WEIGHT, PROCESS_TAG_PREFIX,
    RELEVANCE_WEIGHT, SearchHit, SelectorPolicy, TOPOLOGY_WEIGHT, rank_candidates,
};
use praxec_core::intent_index::{
    IntentEvidence, OUTCOME_RECORDED, aggregate, annotate_hits_with_evidence,
    observations_from_audit, outcome_recorded_payload,
};

// ── fixtures ──────────────────────────────────────────────────────────────

const CLASS: &str = "engineering";

/// A workflow hit tagged with a task-class — the only shape the intent index
/// can key evidence on, and therefore the only shape the policy can act on.
fn hit(id: &str, score: f32, task_class: &str) -> SearchHit {
    SearchHit {
        score,
        item: DiscoveryItem {
            id: id.into(),
            kind: DiscoveryKind::Workflow,
            title: id.into(),
            description: String::new(),
            tags: vec![format!("{PROCESS_TAG_PREFIX}{task_class}")],
            examples: vec![],
            aliases: vec![],
            text: String::new(),
            links: vec![],
            verb: None,
            body: None,
            source: None,
            structural_fingerprint: None,
        },
        evidence: None,
    }
}

fn proven(id: &str, score: f32, runs: usize, rate: f64, cost: Option<f64>) -> SearchHit {
    SearchHit {
        evidence: Some(IntentEvidence {
            runs,
            success_rate: rate,
            mean_cost_usd: cost,
        }),
        ..hit(id, score, CLASS)
    }
}

fn policy(min_runs: usize) -> SelectorPolicy {
    SelectorPolicy { min_runs }
}

fn ids(ranked: &[praxec_core::discovery::RankedCandidate]) -> Vec<&str> {
    ranked.iter().map(|r| r.id.as_str()).collect()
}

// ── the cold-start guard (plan risk #2 — the deliverable) ─────────────────

/// **The single most important test in this file.** Evidence exists, but not
/// enough of it: the policy must not merely "not crash" — it must be *absent*.
/// Assert bit-for-bit equality of the whole ranking (scores, order, components,
/// AND the rendered `why`) against the ranking with the policy switched off.
/// A user whose system has not yet accrued evidence sees zero behaviour change.
#[test]
fn below_threshold_ranking_is_bit_for_bit_the_pre_policy_ranking() {
    let hits = vec![
        proven("flow.thin-a", 5.0, 6, 0.9, Some(0.01)),
        proven("flow.thin-b", 4.0, 9, 0.4, Some(1.00)),
        hit("flow.no-evidence", 3.0, CLASS),
    ];

    // `disabled()` IS the pre-D7 selector: a threshold no evidence can clear.
    let pre_policy = rank_candidates(&hits, None, &SelectorPolicy::disabled());
    let with_policy = rank_candidates(&hits, None, &policy(10));

    assert_eq!(
        with_policy, pre_policy,
        "below the threshold, ranking is identical to pre-D7 — scores, order, and why"
    );
    for r in &with_policy {
        assert!(r.policy.is_none(), "{}: policy must not have fired", r.id);
        assert!(
            !r.why.contains("policy"),
            "{}: the explanation must not mention a policy that did not fire: {}",
            r.id,
            r.why
        );
        // And the score is still the pre-D7 weighted sum over the *plain*
        // evidence component (not a cost-adjusted value).
        let expected = RELEVANCE_WEIGHT * r.relevance
            + EVIDENCE_WEIGHT * r.evidence.component()
            + TOPOLOGY_WEIGHT * r.topology.component;
        assert!((r.score - expected).abs() < 1e-12, "{}: {}", r.id, r.why);
    }
}

/// A completely fresh system — no evidence at all — is the commonest cold
/// start. The policy must be inert there too.
#[test]
fn no_evidence_at_all_is_unchanged_by_the_policy() {
    let hits = vec![hit("flow.a", 5.0, CLASS), hit("flow.b", 2.0, CLASS)];
    assert_eq!(
        rank_candidates(&hits, None, &policy(1)),
        rank_candidates(&hits, None, &SelectorPolicy::disabled()),
        "no evidence ⇒ nothing for any threshold to activate on"
    );
}

/// E2E-3 step 4 — the kill switch. `intent.policy_min_runs` set out of reach
/// reverts to the 0.0.17 evidence-annotation behaviour even with *abundant*
/// evidence on hand.
#[test]
fn kill_switch_reverts_to_annotation_even_with_abundant_evidence() {
    let hits = vec![
        proven("flow.cheap", 3.0, 500, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, 500, 0.95, Some(1.00)),
    ];
    let off = rank_candidates(&hits, None, &SelectorPolicy::disabled());
    assert_eq!(
        ids(&off),
        ["flow.dear", "flow.cheap"],
        "policy off ⇒ the plain success rate decides, as in 0.0.17"
    );
    assert!(off.iter().all(|r| r.policy.is_none()));
}

// ── activation: the policy changes the answer ─────────────────────────────

/// D7-T3 + the flip. Same task-class, identical relevance, both templates well
/// past the threshold. `flow.dear` has the *higher* success rate (95% vs 85%)
/// so pre-D7 it wins — but it costs 100× more per mission. The policy discounts
/// its proven success by its proven price and the order **flips**, in the
/// direction the cost evidence implies.
#[test]
fn above_threshold_cost_evidence_flips_the_order() {
    let hits = vec![
        proven("flow.cheap", 3.0, 12, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, 12, 0.95, Some(1.00)),
    ];

    let before = rank_candidates(&hits, None, &SelectorPolicy::disabled());
    assert_eq!(
        ids(&before),
        ["flow.dear", "flow.cheap"],
        "pre-policy: the higher success rate wins outright, price ignored"
    );

    let after = rank_candidates(&hits, None, &policy(10));
    assert_eq!(
        ids(&after),
        ["flow.cheap", "flow.dear"],
        "policy: 100× the cost for 10 points of success is not worth it"
    );

    // The arithmetic, exactly: the cheapest proven template pays no premium.
    let cheap = &after[0];
    let p = cheap.policy.as_ref().expect("policy fired on flow.cheap");
    assert_eq!(p.cost_premium, 0.0, "the cheapest is never penalized");
    assert!((p.value - 0.85).abs() < 1e-12);

    let dear = &after[1];
    let p = dear.policy.as_ref().expect("policy fired on flow.dear");
    assert!((p.cost_premium - 0.99).abs() < 1e-12, "1 − 0.01/1.00");
    let expected = 0.95 * (1.0 - POLICY_COST_WEIGHT * 0.99);
    assert!((p.value - expected).abs() < 1e-12, "{}", dear.why);
    assert!(p.value < 0.85, "the discount must actually reorder");
}

/// D7-T3 as the test plan words it: A (90% success, cheap) is recommended over
/// B (40%, expensive) for the same task-class.
#[test]
fn high_success_low_cost_template_is_recommended_over_the_empirically_worse_one() {
    let hits = vec![
        proven("flow.b", 3.0, 20, 0.40, Some(0.90)),
        proven("flow.a", 3.0, 20, 0.90, Some(0.02)),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    assert_eq!(ids(&ranked), ["flow.a", "flow.b"]);
    assert!(ranked.iter().all(|r| r.policy.is_some()));
}

/// Cost trades against success — it does not *overturn* success. A template
/// that is merely a little cheaper must not displace a far more reliable one:
/// the discount is capped at `POLICY_COST_WEIGHT` of the success rate.
#[test]
fn cost_cannot_overturn_a_wide_success_gap() {
    let hits = vec![
        proven("flow.reliable", 3.0, 20, 0.95, Some(1.00)),
        proven("flow.junk", 3.0, 20, 0.50, Some(0.0001)),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    assert_eq!(
        ids(&ranked),
        ["flow.reliable", "flow.junk"],
        "even at a ~100% cost premium the 95% template keeps ≥75% of its rate (0.7125 > 0.50)"
    );
}

/// The premium is ratio-based, so it tracks the *magnitude* of the price
/// difference: being 10% dearer costs ~10% of the trade-off, not all of it.
/// (A min-max normalization would punish a trivially-dearer template as
/// harshly as a 100×-dearer one.)
#[test]
fn cost_premium_scales_with_the_size_of_the_premium() {
    let hits = vec![
        proven("flow.base", 3.0, 12, 0.90, Some(0.010)),
        proven("flow.bit-dearer", 3.0, 12, 0.90, Some(0.011)),
        proven("flow.way-dearer", 3.0, 12, 0.90, Some(1.000)),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    let premium = |id: &str| {
        ranked
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.policy.as_ref())
            .map(|p| p.cost_premium)
            .expect(id)
    };
    assert_eq!(premium("flow.base"), 0.0);
    assert!(
        (premium("flow.bit-dearer") - (1.0 - 0.010 / 0.011)).abs() < 1e-12,
        "≈0.09 — a 10% premium is a 10% premium"
    );
    assert!(premium("flow.way-dearer") > 0.98, "1 − 0.01/1.0");
    // …and equal success rates still order cheapest-first.
    assert_eq!(
        ids(&ranked),
        ["flow.base", "flow.bit-dearer", "flow.way-dearer"]
    );
}

// ── per-template activation, inside ONE query ─────────────────────────────

/// The threshold is checked per `(task_class, template)`, not per query: a
/// well-evidenced template is policy-ranked while a thin one falls through in
/// the same batch. And the thin template is not in the cost cohort either —
/// proven here by `flow.a`'s premium staying `0.0` even though the *thin*
/// template is the cheapest thing in the batch.
#[test]
fn one_query_mixes_policy_ranked_and_fall_through_templates() {
    let hits = vec![
        proven("flow.a", 3.0, 12, 0.85, Some(0.010)),
        proven("flow.b", 3.0, 12, 0.95, Some(1.000)),
        proven("flow.thin", 3.0, 6, 1.00, Some(0.001)), // annotated, but sub-threshold
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    let by_id = |id: &str| ranked.iter().find(|r| r.id == id).expect(id);

    let thin = by_id("flow.thin");
    assert!(
        thin.policy.is_none(),
        "6 runs < threshold 10 ⇒ fall through"
    );
    assert!(!thin.why.contains("learned policy"), "{}", thin.why);
    // Its score is the pre-D7 one: the plain 100% success rate, undiscounted.
    assert!((thin.evidence_component() - 1.0).abs() < 1e-12);
    assert!((thin.score - (RELEVANCE_WEIGHT + EVIDENCE_WEIGHT)).abs() < 1e-12);

    let a = by_id("flow.a");
    let p = a.policy.as_ref().expect("flow.a is policy-ranked");
    assert_eq!(
        p.cost_premium, 0.0,
        "the sub-threshold template's $0.001 must NOT set the cohort's cheapest \
         price — unproven evidence cannot move a proven template's score"
    );
    assert!(by_id("flow.b").policy.is_some());
}

/// Cohorts are per task-class: the intent index keys evidence on
/// `(task_class, template)`, so "cheaper" is only asked within a class. A cheap
/// template in *another* class must not set this class's baseline.
#[test]
fn cost_cohorts_do_not_leak_across_task_classes() {
    let other = SearchHit {
        evidence: Some(IntentEvidence {
            runs: 12,
            success_rate: 1.0,
            mean_cost_usd: Some(0.0001),
        }),
        ..hit("flow.other-class", 3.0, "research")
    };
    let hits = vec![proven("flow.eng", 3.0, 12, 0.9, Some(0.50)), other];
    let ranked = rank_candidates(&hits, None, &policy(10));
    let eng = ranked
        .iter()
        .find(|r| r.id == "flow.eng")
        .expect("flow.eng");
    let p = eng.policy.as_ref().expect("policy fired");
    assert_eq!(
        p.cost_premium, 0.0,
        "flow.eng is the cheapest proven template *in engineering* — a research \
         template's price is not its baseline"
    );
}

// ── the threshold is a real, tunable knob ─────────────────────────────────

/// D7-T5 — both sides of the boundary. The knob is not decorative: at
/// `min_runs == runs` the policy fires and reorders; one run higher it does not
/// exist, and the ranking is exactly the pre-policy one.
#[test]
fn threshold_activation_boundary_is_exact_on_both_sides() {
    let hits = vec![
        proven("flow.cheap", 3.0, 10, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, 10, 0.95, Some(1.00)),
    ];

    let at = rank_candidates(&hits, None, &policy(10)); // runs (10) >= min_runs (10)
    assert_eq!(ids(&at), ["flow.cheap", "flow.dear"], "fires at the bar");
    assert!(at.iter().all(|r| r.policy.is_some()));

    let above = rank_candidates(&hits, None, &policy(11)); // 10 < 11
    assert_eq!(
        above,
        rank_candidates(&hits, None, &SelectorPolicy::disabled()),
        "one run short of the bar ⇒ exactly the pre-policy ranking"
    );
    assert_eq!(ids(&above), ["flow.dear", "flow.cheap"]);
}

/// The shipped default is the *conservative* one: the policy demands strictly
/// more evidence to act than the annotator needs to display. A pair sitting
/// between the two bars shows its track record but does not yet steer.
#[test]
fn shipped_default_threshold_is_stricter_than_the_annotation_bar() {
    let annotate_bar = praxec_core::intent_index::IntentParams::from_tuning().min_runs;
    let act_bar = SelectorPolicy::from_tuning().min_runs;
    assert!(
        act_bar > annotate_bar,
        "acting on evidence must demand more of it than showing it: {act_bar} vs {annotate_bar}"
    );
    let between = (annotate_bar + act_bar) / 2;
    let hits = vec![
        proven("flow.cheap", 3.0, between, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, between, 0.95, Some(1.00)),
    ];
    assert_eq!(
        rank_candidates(&hits, None, &SelectorPolicy::from_tuning()),
        rank_candidates(&hits, None, &SelectorPolicy::disabled()),
        "between the bars: annotated, but not yet acted on"
    );
}

/// Poka-yoke: a pathological `policy_min_runs: 0` must not read as "act on no
/// evidence". It is clamped to 1, and the annotator's own gate has already
/// dropped every pair below `intent.min_runs` — so the stricter of the two bars
/// always wins by construction.
#[test]
fn zero_threshold_still_requires_evidence_to_exist() {
    let hits = vec![hit("flow.none", 3.0, CLASS)];
    let ranked = rank_candidates(&hits, None, &policy(0));
    assert!(
        ranked[0].policy.is_none(),
        "no evidence ⇒ no activation, whatever the threshold says"
    );
    assert!(ranked[0].why.contains("neutral, not failure"));
}

// ── explainability ────────────────────────────────────────────────────────

/// Activation is never silent: the `why` names the policy and carries the
/// evidence it rests on — runs, the threshold cleared, the success rate, and
/// the cost — so a re-ranking is auditable, not oracular.
#[test]
fn why_names_the_policy_activation_and_its_evidence() {
    let hits = vec![
        proven("flow.cheap", 3.0, 12, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, 14, 0.95, Some(1.00)),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));

    let why = &ranked[0].why;
    assert!(why.contains("learned policy active"), "{why}");
    assert!(why.contains("12 evidence run(s)"), "{why}");
    assert!(why.contains("threshold 10"), "{why}");
    assert!(why.contains("85.0% success"), "{why}");
    assert!(why.contains("mean $0.0100"), "{why}");
    assert!(why.contains("cost premium"), "{why}");
    // The rendered score still matches the number that ordered the batch.
    assert!(why.contains(&format!("{:.3}", ranked[0].score)), "{why}");

    let dear = &ranked[1].why;
    assert!(dear.contains("14 evidence run(s)"), "{dear}");
    assert!(dear.contains("mean $1.0000"), "{dear}");
}

/// The score stays auditable arithmetic under the policy: the policy replaces
/// *what the evidence term claims*, never how much it weighs.
#[test]
fn score_is_the_documented_weighted_sum_with_the_policy_value_substituted() {
    let hits = vec![
        proven("flow.cheap", 4.0, 12, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, 12, 0.95, Some(1.00)),
        hit("flow.plain", 5.0, CLASS),
    ];
    for r in rank_candidates(&hits, None, &policy(10)) {
        let expected = RELEVANCE_WEIGHT * r.relevance
            + EVIDENCE_WEIGHT * r.evidence_component()
            + TOPOLOGY_WEIGHT * r.topology.component;
        assert!((r.score - expected).abs() < 1e-12, "{}: {}", r.id, r.why);
    }
}

/// D7-T4 — same evidence ⇒ same recommendation, every time.
#[test]
fn policy_ranking_is_deterministic() {
    let hits = vec![
        proven("flow.cheap", 3.0, 12, 0.85, Some(0.01)),
        proven("flow.dear", 3.0, 12, 0.95, Some(1.00)),
        proven("flow.unpriced", 3.0, 12, 0.85, None),
        hit("flow.plain", 3.0, CLASS),
    ];
    let first = rank_candidates(&hits, None, &policy(10));
    let second = rank_candidates(&hits, None, &policy(10));
    assert_eq!(first, second, "same evidence ⇒ same order, scores, and why");
}

// ── unusable / absent evidence: fall through, loudly, never panic ─────────

/// An unpriced-but-proven template is neither treated as the cheapest (a reward
/// it did not earn) nor as the dearest (a penalty the evidence cannot support):
/// it takes the neutral premium, exactly as absent evidence takes the neutral
/// component. No fabricated cost.
#[test]
fn unpriced_evidence_takes_the_neutral_cost_premium() {
    let hits = vec![
        proven("flow.cheap", 3.0, 12, 0.90, Some(0.01)),
        proven("flow.unpriced", 3.0, 12, 0.90, None),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    let unpriced = ranked
        .iter()
        .find(|r| r.id == "flow.unpriced")
        .expect("ranked");
    let p = unpriced.policy.as_ref().expect("policy fired");
    assert_eq!(p.cost_premium, praxec_core::discovery::NEUTRAL_COST_PREMIUM);
    assert!(unpriced.why.contains("unpriced"), "{}", unpriced.why);
    // Equal success, but a *known*-cheap template beats an unknown-cost one.
    assert_eq!(ids(&ranked), ["flow.cheap", "flow.unpriced"]);
}

/// Corrupt evidence (nothing legitimate produces a negative mean cost) must not
/// be repaired into a plausible number and must not panic. The policy declines,
/// the candidate falls through to the pre-D7 blend, and the `why` says so — a
/// fall-through on bad data is never silent.
#[test]
fn corrupt_cost_withholds_the_policy_and_falls_through_loudly() {
    let hits = vec![
        proven("flow.bad-cost", 3.0, 12, 0.90, Some(-1.0)),
        proven("flow.ok", 3.0, 12, 0.50, Some(0.01)),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    let bad = ranked
        .iter()
        .find(|r| r.id == "flow.bad-cost")
        .expect("never dropped");
    assert!(bad.policy.is_none(), "the policy refuses to act on it");
    assert!(
        bad.why
            .contains("learned policy withheld: unusable mean_cost_usd"),
        "{}",
        bad.why
    );
    // Fall-through means the pre-D7 blend, unchanged — the plain success rate.
    assert!((bad.evidence_component() - 0.90).abs() < 1e-12);
    assert!((bad.score - (RELEVANCE_WEIGHT + EVIDENCE_WEIGHT * 0.90)).abs() < 1e-12);
}

/// A non-finite success rate is unusable evidence, not a 0% record. Withhold,
/// don't panic, don't fabricate.
#[test]
fn nonfinite_success_rate_withholds_the_policy_without_panicking() {
    let hits = vec![
        proven("flow.nan", 3.0, 12, f64::NAN, Some(0.01)),
        proven("flow.ok", 3.0, 12, 0.90, Some(0.01)),
    ];
    let ranked = rank_candidates(&hits, None, &policy(10));
    assert_eq!(ranked.len(), 2, "ranking is total — nothing is dropped");
    let nan = ranked
        .iter()
        .find(|r| r.id == "flow.nan")
        .expect("never dropped");
    assert!(nan.policy.is_none());
    assert!(
        nan.why
            .contains("learned policy withheld: unusable success_rate"),
        "{}",
        nan.why
    );
}

// ── D7-T1 / D7-T6 — the evidence really comes from the audit ──────────────

/// D7-T1 — `aggregate()` splits evidence per `(task_class, template)`; the
/// policy's cohort key and the index's cohort key are the same key.
#[test]
fn aggregate_keys_evidence_per_task_class_and_template() {
    let events: Vec<AuditEvent> = (0..4)
        .flat_map(|i| {
            [
                mission(&format!("eng{i}"), "flow.x", "engineering", true, 0.01),
                mission(&format!("res{i}"), "flow.x", "research", i == 0, 0.50),
            ]
        })
        .flatten()
        .collect();
    let stats = aggregate(&observations_from_audit(&events, &[]));
    assert_eq!(stats.len(), 2, "one row per (task_class, template) pair");
    let eng = stats
        .iter()
        .find(|s| s.task_class == "engineering")
        .expect("engineering");
    assert_eq!(eng.evidence_runs, 4);
    assert_eq!(eng.success_rate, 1.0);
    assert!((eng.mean_cost_usd.unwrap() - 0.01).abs() < 1e-9);
    let res = stats
        .iter()
        .find(|s| s.task_class == "research")
        .expect("research");
    assert_eq!(res.success_rate, 0.25, "1 of 4 met");
    assert!((res.mean_cost_usd.unwrap() - 0.50).abs() < 1e-9);
}

/// D7-T6 / E2E-3 — the whole loop on real audit events, no synthetic
/// `IntentEvidence` shortcut: `outcome.recorded` + `agent.completed` →
/// `observations_from_audit` → `aggregate` → `annotate_hits_with_evidence` →
/// `rank_candidates`. `flow.b` succeeds *more often* (12/12 vs 10/12) but costs
/// 100× more; the policy picks `flow.a`, and pre-policy ranking picks `flow.b`.
#[test]
fn audit_evidence_drives_the_policy_end_to_end() {
    let mut events = Vec::new();
    for i in 0..12 {
        // flow.a — 10/12 met, $0.01 a mission.
        events.extend(mission(&format!("a{i}"), "flow.a", CLASS, i < 10, 0.01));
        // flow.b — 12/12 met, $1.00 a mission.
        events.extend(mission(&format!("b{i}"), "flow.b", CLASS, true, 1.00));
    }
    let stats = aggregate(&observations_from_audit(&events, &[]));

    let mut hits = vec![hit("flow.a", 3.0, CLASS), hit("flow.b", 3.0, CLASS)];
    // The production annotator, at the production annotation bar.
    annotate_hits_with_evidence(
        &mut hits,
        &stats,
        praxec_core::intent_index::IntentParams::from_tuning().min_runs,
    );
    let a_ev = hits[0].evidence.as_ref().expect("flow.a annotated");
    assert_eq!(a_ev.runs, 12);
    assert!((a_ev.success_rate - 10.0 / 12.0).abs() < 1e-9);
    assert!((a_ev.mean_cost_usd.unwrap() - 0.01).abs() < 1e-9);

    let before = rank_candidates(&hits, None, &SelectorPolicy::disabled());
    assert_eq!(
        ids(&before),
        ["flow.b", "flow.a"],
        "0.0.17 annotation-only: the higher success rate wins, cost only a tie-break"
    );

    let after = rank_candidates(&hits, None, &SelectorPolicy::from_tuning());
    assert_eq!(
        ids(&after),
        ["flow.a", "flow.b"],
        "D7: 100× the price for 2 more successes in 12 is not worth it"
    );
    // …and the recommendation surfaces the evidence it rests on (auditable).
    let why = &after[0].why;
    assert!(why.contains("learned policy active"), "{why}");
    assert!(why.contains("12 evidence run(s)"), "{why}");
    assert!(why.contains("83.3% success"), "{why}");
    assert!(why.contains("mean $0.0100"), "{why}");
}

/// One terminated mission: the `outcome.recorded` terminal plus its priced
/// agent step, exactly as the runtime emits them.
fn mission(wf: &str, template: &str, task_class: &str, met: bool, cost: f64) -> Vec<AuditEvent> {
    vec![
        AuditEvent::new(OUTCOME_RECORDED)
            .with_workflow(wf)
            .with_payload(outcome_recorded_payload(
                template,
                Some(task_class),
                met,
                2,
                if met { "succeeded" } else { "failed" },
                None,
            )),
        AuditEvent::new(AGENT_COMPLETED)
            .with_workflow(wf)
            .with_payload(serde_json::json!({
                "model": "openrouter:z-ai/glm-5.2",
                "prompt_tokens": 1000,
                "completion_tokens": 200,
                "cost_usd": cost,
            })),
    ]
}
