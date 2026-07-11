//! v0.0.18 mechanism #2 — the canonical **structural fingerprint** of a
//! workflow's control-flow graph, plus exact/near-duplicate detection over a
//! catalog of them.
//!
//! This is *not* semantic search (mechanism #1). Semantic search asks "what does
//! this flow say it does" (descriptions → embeddings). This asks "**what machine
//! is this**" (states, transitions, executor topology) so praxec-meta can
//! compare / cluster / dedup / merge flows by their real graph, feeding
//! `flow.optimize-*`. The two are complementary on purpose: prose is exactly the
//! thing this mechanism throws away, because two flows that drive the same
//! machine under different prose ARE the merge candidate we want surfaced.
//!
//! ## What is in the fingerprint (and what is not)
//!
//! The fingerprint is the **identity of the control-flow graph**, hashed through
//! the [`contract_hash`](crate::contract_hash) canonicalization (sorted object
//! keys at every depth, SHA-256, `sha256:<hex>`) — one canonicalizer for the
//! whole codebase, never a second one that can drift.
//!
//! **Does NOT change the fingerprint** (cosmetic / non-graph):
//! - Declaration **order** — of states, of transitions, of keys anywhere. States
//!   and transitions are JSON objects; the canonicalizer sorts keys at every
//!   depth. Reordering two independent transitions must not move the hash or the
//!   whole mechanism is worthless for dedup.
//! - **Prose**: `title`, `description`, `goal`, `guidance` on the workflow, its
//!   states, its transitions, its guards. Model-facing prose steers behavior, but
//!   it is not graph structure; a fingerprint that moved with it could never find
//!   "the same flow, reworded" — the single most likely duplicate in a corpus
//!   grown by minting.
//! - **Non-graph declaration blocks**: `tags`, `aliases`, `examples`, `process` /
//!   `taskClass`, `inputs`, `blackboard`, `outcomes`, `version`, `linkFilter`.
//!   These describe, type, or classify the flow; none of them is an edge.
//! - Whitespace/formatting (we hash a Value, never source text).
//!
//! **DOES change the fingerprint** (structure):
//! - The `initialState` (where the machine starts).
//! - The **set of states**, keyed by name, with each state's `terminal` /
//!   `outcome` / `actor` (who is allowed to move it — a human gate is a
//!   structural fact about the machine, not a cosmetic one).
//! - The **set of transitions** per state, keyed by name, each with its `target`,
//!   `actor`, `guards` (as a set — see below), `executor`, and everything else
//!   the transition declares (`inputSchema`, `output`, `evidence`, `reliability`,
//!   `branches`, `hop_slot`, `prefill`, …).
//! - The **executor**, verbatim minus prose — its `kind` AND what it composes
//!   (`definitionId`, `capability`, `subject`, `connection`, …) AND its data
//!   wiring (`use.inputs` / `use.outputs`).
//!
//! Note the asymmetry in that last bullet, which is deliberate: **state and
//! transition names are identity, not cosmetics.** A caller submits
//! `praxec.command {transition}` by name, HATEOAS links carry state names, guards
//! and mappings reference them. Renaming a state changes the flow's external
//! contract, so it changes the fingerprint. Blackboard slot names, by contrast,
//! are internal — but they still ride along inside the executor's `use:` block,
//! and that is the **fail-safe** direction: everything inside the graph is
//! included unless it is explicitly listed as prose. A key we have never seen
//! (a new executor kind's new field) therefore *widens* the fingerprint rather
//! than being silently dropped. The failure it prevents is the one that actually
//! costs something: a **false** exact-duplicate that drives a wrong merge. The
//! opposite miss (two flows that are "really" the same but wired through
//! differently-named slots) is not lost either — it lands in
//! [`near_duplicate_pairs`] with a high similarity.
//!
//! Equal fingerprint therefore means: *identical control-flow graph, identical
//! composition, identical wiring — differing at most in prose and ordering.* It
//! is a screening signal for dedup/merge, not a proof of semantic equivalence;
//! at today's corpus scale the consumer reads the two YAMLs before merging.
//!
//! ## Near-duplicates
//!
//! No embedding (explicitly out of scope for 0.0.18 — the learned structural
//! embedding earns its place only at corpus scale). Instead: a deterministic,
//! explainable **Jaccard similarity over the graph's feature set** —
//! [`features`] emits one readable string per structural fact (`state:x`,
//! `edge:a-go->b`, `exec:a.go:workflow`, `composes:cap.plan.draft`, …), and
//! similarity is `|A ∩ B| / |A ∪ B|`. You can always answer *why* two flows are
//! near-duplicates by diffing their feature sets.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::contract_hash::canonical_json_string;

/// Prose keys stripped from the workflow / state / transition / guard /
/// executor objects. Stripped ONLY at those levels — never by a blind recursive
/// walk, which would also delete a JSON-Schema *property named* `title` or
/// `description` inside a transition's `inputSchema` and quietly change what the
/// hash covers.
const PROSE_KEYS: &[&str] = &["title", "description", "goal", "guidance"];

/// Non-graph blocks on the workflow itself. Everything except the graph is
/// dropped, so this list exists only to be explicit about what a workflow
/// declares beyond its machine; the projection keeps `initialState` + `states`
/// and nothing else.
const WORKFLOW_GRAPH_KEYS: &[&str] = &["initialState", "states"];

/// Executor keys whose value names another praxec artifact the flow *composes*.
/// Used ONLY to emit the `composes:` similarity feature (the crossmatrix view of
/// a flow). Best-effort by design: a kind whose reference key is missing here
/// loses one *similarity* feature, never a bit of the fingerprint — the
/// fingerprint includes the whole executor regardless, so this list can never
/// cause a false exact-duplicate.
const COMPOSITION_KEYS: &[&str] = &["definitionId", "capability", "subject", "connection"];

/// Default near-duplicate cut-off for [`near_duplicate_pairs`].
///
/// Derived, not guessed. The smallest realistic praxec flow is ~3 states / 3
/// edges ⇒ ~7 features (`initial:` + 3 `state:` + 3 `edge:`/`exec:` groups). A
/// clone that changes exactly ONE transition target moves 1 feature, giving
/// Jaccard `6/8 = 0.75`; larger flows move proportionally less (a one-edge edit
/// in a 12-state flow scores > 0.95). Two unrelated flows share almost nothing
/// (different state names ⇒ near-zero overlap; the shipped corpus measures < 0.2).
/// `0.70` sits in the empty valley between those populations: below the
/// worst-case single-edit clone, far above unrelated pairs.
pub const NEAR_DUPLICATE_THRESHOLD: f64 = 0.70;

/// One catalog entry's structural identity: its fingerprint (exact match) and
/// its feature set (near match). Computed once per definition — the pairwise
/// scan in [`near_duplicate_pairs`] is O(n²) in *set intersections*, not in
/// re-parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowFingerprint {
    /// The workflow's definitionId (the catalog key).
    pub id: String,
    /// `sha256:<hex>` over the canonical structural form.
    pub fingerprint: String,
    /// The structural features backing the near-duplicate measure.
    pub features: BTreeSet<String>,
}

impl FlowFingerprint {
    pub fn compute(id: impl Into<String>, definition: &Value) -> Self {
        Self {
            id: id.into(),
            fingerprint: fingerprint(definition),
            features: features(definition),
        }
    }
}

/// A structurally-similar (but not identical) pair. `similarity` is the Jaccard
/// score over the two feature sets; the caller renders the diff of
/// [`FlowFingerprint::features`] to say *why*.
#[derive(Debug, Clone, PartialEq)]
pub struct NearDuplicate {
    pub a: String,
    pub b: String,
    pub similarity: f64,
}

/// The canonical structural fingerprint of ONE workflow definition (the value
/// under `workflows.<id>`), as `sha256:<64 hex>`.
///
/// See the module docs for exactly what does and does not move this hash.
pub fn fingerprint(definition: &Value) -> String {
    let canonical = canonical_json_string(&structural_form(definition));
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

/// The canonical structural form that [`fingerprint`] hashes — the workflow with
/// prose and non-graph declarations projected away and guard sets normalized.
///
/// Public because "why do these two hash the same?" must be answerable without a
/// debugger: diff the two structural forms.
pub fn structural_form(definition: &Value) -> Value {
    let mut out = Map::new();
    for key in WORKFLOW_GRAPH_KEYS {
        match *key {
            "states" => {
                let states = definition
                    .get("states")
                    .and_then(Value::as_object)
                    .map(|states| {
                        Value::Object(
                            states
                                .iter()
                                .map(|(name, state)| (name.clone(), state_form(state)))
                                .collect(),
                        )
                    })
                    .unwrap_or_else(|| json!({}));
                out.insert("states".into(), states);
            }
            k => {
                if let Some(v) = definition.get(k) {
                    out.insert(k.to_string(), v.clone());
                }
            }
        }
    }
    Value::Object(out)
}

/// A state, minus prose, with its transitions normalized. Everything else the
/// state declares (`terminal`, `outcome`, `actor`, `onEnter`, `skills`, …) rides
/// along — include-by-default is the fail-safe direction (see module docs).
fn state_form(state: &Value) -> Value {
    let Some(obj) = state.as_object() else {
        // A malformed state (not an object) is not this function's problem to
        // diagnose — `praxec check` rejects it. Hash it verbatim rather than
        // inventing a shape.
        return state.clone();
    };
    let mut out = strip_prose(obj);
    if let Some(transitions) = obj.get("transitions").and_then(Value::as_object) {
        out.insert(
            "transitions".into(),
            Value::Object(
                transitions
                    .iter()
                    .map(|(name, t)| (name.clone(), transition_form(t)))
                    .collect(),
            ),
        );
    }
    Value::Object(out)
}

/// A transition, minus prose, with its guard array normalized to a *set*.
fn transition_form(transition: &Value) -> Value {
    let Some(obj) = transition.as_object() else {
        return transition.clone();
    };
    let mut out = strip_prose(obj);
    if let Some(guards) = obj.get("guards").and_then(Value::as_array) {
        // Guards on a transition are ANDed (`runtime_chain`: `all_pass`), so
        // their declaration order carries no semantics — canonicalize the array
        // into a sorted set so a reordered conjunction hashes the same. (The
        // `branches:` array is NOT sorted: those are first-match-wins, so their
        // order IS the semantics.)
        let mut normalized: Vec<Value> = guards
            .iter()
            .map(|g| match g.as_object() {
                Some(o) => Value::Object(strip_prose(o)),
                None => g.clone(),
            })
            .collect();
        normalized.sort_by_cached_key(canonical_json_string);
        out.insert("guards".into(), Value::Array(normalized));
    }
    if let Some(executor) = obj.get("executor") {
        out.insert("executor".into(), executor_form(executor));
    }
    Value::Object(out)
}

/// An executor, minus prose. Its `branches:` (SPEC §24 parallel fan-out) run
/// concurrently, so — unlike a transition's first-match-wins `branches:` — their
/// declaration order carries no semantics and is normalized away; each branch is
/// itself an executor (recursive).
fn executor_form(executor: &Value) -> Value {
    let Some(obj) = executor.as_object() else {
        return executor.clone();
    };
    let mut out = strip_prose(obj);
    if let Some(branches) = obj.get("branches").and_then(Value::as_array) {
        let mut normalized: Vec<Value> = branches.iter().map(executor_form).collect();
        normalized.sort_by_cached_key(canonical_json_string);
        out.insert("branches".into(), Value::Array(normalized));
    }
    Value::Object(out)
}

fn strip_prose(obj: &Map<String, Value>) -> Map<String, Value> {
    obj.iter()
        .filter(|(k, _)| !PROSE_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The structural features of a workflow — the basis of the near-duplicate
/// measure, and the explanation of any score it produces.
///
/// One readable string per structural fact:
/// - `initial:<state>`
/// - `state:<name>` / `terminal:<name>`
/// - `edge:<from>-<transition>-><to>`
/// - `exec:<from>.<transition>:<kind>`
/// - `composes:<artifact>` — which caps/scripts/connections the flow composes
///   (the crossmatrix view; see [`COMPOSITION_KEYS`])
/// - `guard:<from>.<transition>:<kind>`
///
/// Names are part of the features on purpose (same reasoning as the
/// fingerprint): a state name is the flow's addressable identity. The honest
/// consequence — stated rather than hidden — is that a clone with *every* state
/// renamed scores near zero and is NOT reported as a near-duplicate. Finding
/// those needs rename-invariant canonical labeling, which is the corpus-scale
/// work this release deliberately defers.
pub fn features(definition: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    if let Some(initial) = definition.get("initialState").and_then(Value::as_str) {
        out.insert(format!("initial:{initial}"));
    }
    let Some(states) = definition.get("states").and_then(Value::as_object) else {
        return out;
    };
    for (state_name, state) in states {
        out.insert(format!("state:{state_name}"));
        if state.get("terminal").and_then(Value::as_bool) == Some(true) {
            out.insert(format!("terminal:{state_name}"));
        }
        let Some(transitions) = state.get("transitions").and_then(Value::as_object) else {
            continue;
        };
        for (t_name, t) in transitions {
            if let Some(target) = t.get("target").and_then(Value::as_str) {
                out.insert(format!("edge:{state_name}-{t_name}->{target}"));
            }
            if let Some(executor) = t.get("executor") {
                if let Some(kind) = executor.get("kind").and_then(Value::as_str) {
                    out.insert(format!("exec:{state_name}.{t_name}:{kind}"));
                }
                for key in COMPOSITION_KEYS {
                    if let Some(reference) = executor.get(*key).and_then(Value::as_str) {
                        out.insert(format!("composes:{reference}"));
                    }
                }
            }
            let guards = t
                .get("guards")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            for guard in guards {
                if let Some(kind) = guard.get("kind").and_then(Value::as_str) {
                    out.insert(format!("guard:{state_name}.{t_name}:{kind}"));
                }
            }
        }
    }
    out
}

/// Jaccard similarity `|A ∩ B| / |A ∪ B|` over two feature sets. `1.0` for
/// identical structure, `0.0` for no shared structure. Two empty sets score
/// `1.0` — they are, structurally, the same nothing.
pub fn similarity(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        return 1.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Fingerprint every workflow in a catalog (the `workflows:` map of a gateway
/// config, or any `{id → definition}` view of it).
pub fn fingerprint_catalog(workflows: &BTreeMap<String, Value>) -> Vec<FlowFingerprint> {
    workflows
        .iter()
        .map(|(id, def)| FlowFingerprint::compute(id.clone(), def))
        .collect()
}

/// Exact duplicates: groups of ≥2 definitionIds sharing one fingerprint — the
/// same machine cataloged twice. Groups are id-sorted, and the group list is
/// ordered by first id, so the report is stable across runs.
pub fn exact_duplicate_groups(catalog: &[FlowFingerprint]) -> Vec<Vec<String>> {
    let mut by_hash: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for f in catalog {
        by_hash
            .entry(f.fingerprint.as_str())
            .or_default()
            .push(f.id.as_str());
    }
    let mut groups: Vec<Vec<String>> = by_hash
        .into_values()
        .filter(|ids| ids.len() > 1)
        .map(|ids| {
            let mut ids: Vec<String> = ids.into_iter().map(str::to_string).collect();
            ids.sort();
            ids
        })
        .collect();
    groups.sort();
    groups
}

/// Near-duplicates: pairs whose structures overlap by at least `threshold` but
/// whose fingerprints differ (an exact duplicate is reported by
/// [`exact_duplicate_groups`], never here — the two reports never double-count).
///
/// Ordered strongest-first, ties broken by id, so the report is stable.
///
/// # Panics
/// If `threshold` is not in `(0.0, 1.0]`. A zero/negative threshold would pair
/// every flow with every other and a > 1.0 threshold could never match: both are
/// caller bugs, and silently "correcting" them would hand back a plausible-looking
/// but meaningless report.
pub fn near_duplicate_pairs(catalog: &[FlowFingerprint], threshold: f64) -> Vec<NearDuplicate> {
    assert!(
        threshold > 0.0 && threshold <= 1.0,
        "near-duplicate threshold must be in (0.0, 1.0], got {threshold}"
    );
    let mut out = Vec::new();
    for (i, a) in catalog.iter().enumerate() {
        for b in &catalog[i + 1..] {
            if a.fingerprint == b.fingerprint {
                continue;
            }
            let score = similarity(&a.features, &b.features);
            if score >= threshold {
                let (a, b) = if a.id <= b.id { (a, b) } else { (b, a) };
                out.push(NearDuplicate {
                    a: a.id.clone(),
                    b: b.id.clone(),
                    similarity: score,
                });
            }
        }
    }
    out.sort_by(|x, y| {
        y.similarity
            .total_cmp(&x.similarity)
            .then_with(|| (&x.a, &x.b).cmp(&(&y.a, &y.b)))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow() -> Value {
        json!({
            "initialState": "start",
            "states": {
                "start": { "transitions": { "go": { "target": "done", "executor": { "kind": "noop" } } } },
                "done":  { "terminal": true }
            }
        })
    }

    #[test]
    fn fingerprint_is_sha256_prefixed_and_deterministic() {
        let h = fingerprint(&flow());
        assert!(h.starts_with("sha256:"));
        assert_eq!(h.len(), "sha256:".len() + 64);
        assert_eq!(h, fingerprint(&flow()));
    }

    #[test]
    fn guard_declaration_order_does_not_move_the_hash() {
        // Guards are ANDed — a reordered conjunction is the same conjunction.
        let mk = |guards: Value| {
            json!({
                "initialState": "s",
                "states": { "s": { "transitions": { "go": { "target": "s", "guards": guards } } } }
            })
        };
        let a = mk(
            json!([{ "kind": "role", "role": "admin" }, { "kind": "expr", "expr": "$.context.ok == true" }]),
        );
        let b = mk(
            json!([{ "kind": "expr", "expr": "$.context.ok == true" }, { "kind": "role", "role": "admin" }]),
        );
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn parallel_branch_order_does_not_move_the_hash() {
        // SPEC §24 branches run concurrently; their order is not semantics.
        let mk = |branches: Value| {
            json!({
                "initialState": "s",
                "states": { "s": { "transitions": { "go": {
                    "target": "s",
                    "executor": { "kind": "parallel", "branches": branches }
                } } } }
            })
        };
        let a = mk(json!([{ "kind": "noop" }, { "kind": "script", "subject": "check.x" }]));
        let b = mk(json!([{ "kind": "script", "subject": "check.x" }, { "kind": "noop" }]));
        assert_eq!(fingerprint(&a), fingerprint(&b));
    }

    #[test]
    fn json_schema_property_named_title_survives_prose_stripping() {
        // The prose strip must not reach into an inputSchema's *properties* — a
        // property named `title` is a contract field, not prose.
        let with_title = json!({
            "initialState": "s",
            "states": { "s": { "transitions": { "go": {
                "target": "s",
                "inputSchema": { "type": "object", "properties": { "title": { "type": "string" } } }
            } } } }
        });
        let without = json!({
            "initialState": "s",
            "states": { "s": { "transitions": { "go": {
                "target": "s",
                "inputSchema": { "type": "object", "properties": {} }
            } } } }
        });
        assert_ne!(fingerprint(&with_title), fingerprint(&without));
    }

    #[test]
    fn similarity_of_a_flow_with_itself_is_one() {
        assert_eq!(similarity(&features(&flow()), &features(&flow())), 1.0);
    }

    #[test]
    #[should_panic(expected = "near-duplicate threshold")]
    fn a_zero_threshold_is_a_caller_bug_not_a_wide_net() {
        near_duplicate_pairs(&[FlowFingerprint::compute("a", &flow())], 0.0);
    }
}
