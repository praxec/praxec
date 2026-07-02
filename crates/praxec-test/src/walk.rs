//! Deterministic structural walk of a workflow definition: every (state ->
//! transition -> target) edge, plus reachability from initialState and the
//! orphaned states. Mirrors the reachability BFS in
//! crates/praxec-core/src/validate.rs (~line 296).

use serde_json::Value;
use std::collections::{HashSet, VecDeque};

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub struct Edge {
    pub state: String,
    pub transition: String,
    pub target: String,
}

pub struct WalkResult {
    pub edges: Vec<Edge>,
    pub reachable_states: HashSet<String>,
    pub orphan_states: Vec<String>,
}

/// `definition` is one resolved workflow definition JSON.
pub fn walk(definition: &Value) -> WalkResult {
    let mut edges = Vec::new();
    let mut reachable_states = HashSet::new();

    // Extract states object
    let states = match definition.get("states").and_then(Value::as_object) {
        Some(s) => s,
        None => {
            return WalkResult {
                edges,
                reachable_states,
                orphan_states: Vec::new(),
            };
        }
    };

    // Collect all state names for membership checks
    let state_names: HashSet<String> = states.keys().map(|s| s.to_string()).collect();

    // Collect edges: for each state and each transition, extract target(s)
    for (state_name, state_def) in states {
        if let Some(transitions) = state_def.get("transitions").and_then(Value::as_object) {
            for (t_name, t_def) in transitions {
                // Direct target
                if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                    edges.push(Edge {
                        state: state_name.to_string(),
                        transition: t_name.to_string(),
                        target: target.to_string(),
                    });
                }
                // Branch targets
                if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                    for branch in branches {
                        if let Some(target) = branch.get("target").and_then(Value::as_str) {
                            edges.push(Edge {
                                state: state_name.to_string(),
                                transition: t_name.to_string(),
                                target: target.to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Reachability BFS from initialState
    if let Some(initial_state) = definition.get("initialState").and_then(Value::as_str) {
        if state_names.contains(initial_state) {
            let mut queue = VecDeque::new();
            queue.push_back(initial_state);
            reachable_states.insert(initial_state.to_string());

            while let Some(current) = queue.pop_front() {
                if let Some(state_def) = states.get(current) {
                    if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
                        for (_t_name, t_def) in ts {
                            // Direct targets
                            if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                                if state_names.contains(target)
                                    && reachable_states.insert(target.to_string())
                                {
                                    queue.push_back(target);
                                }
                            }
                            // Branch targets
                            if let Some(branches) = t_def.get("branches").and_then(Value::as_array)
                            {
                                for branch in branches {
                                    if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                                        if state_names.contains(bt)
                                            && reachable_states.insert(bt.to_string())
                                        {
                                            queue.push_back(bt);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Find orphan states: all states NOT in reachable_states, sorted
    let mut orphan_states: Vec<String> = state_names
        .iter()
        .filter(|s| !reachable_states.contains(*s))
        .map(|s| s.to_string())
        .collect();
    orphan_states.sort();

    WalkResult {
        edges,
        reachable_states,
        orphan_states,
    }
}

/// States reachable from `start` (inclusive) following transition + branch
/// targets. BFS, same edge rules as `walk`.
pub fn reachable_from(definition: &Value, start: &str) -> std::collections::HashSet<String> {
    let mut reachable = std::collections::HashSet::new();
    let states = match definition.get("states").and_then(Value::as_object) {
        Some(s) => s,
        None => return reachable,
    };
    let state_names: std::collections::HashSet<String> =
        states.keys().map(|s| s.to_string()).collect();

    if !state_names.contains(start) {
        return reachable;
    }

    let mut queue = VecDeque::new();
    queue.push_back(start.to_string());
    reachable.insert(start.to_string());

    while let Some(current) = queue.pop_front() {
        if let Some(state_def) = states.get(current.as_str()) {
            if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
                for (_t_name, t_def) in ts {
                    // Direct target
                    if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                        if state_names.contains(target) && reachable.insert(target.to_string()) {
                            queue.push_back(target.to_string());
                        }
                    }
                    // Branch targets
                    if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                        for branch in branches {
                            if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                                if state_names.contains(bt) && reachable.insert(bt.to_string()) {
                                    queue.push_back(bt.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    reachable
}

/// Detect states caught in an infinite deterministic loop with no escape.
///
/// A state `S` is in an infinite deterministic loop when:
/// 1. It is part of a cycle reachable only via `actor: deterministic` transitions.
/// 2. Following ONLY deterministic transitions from `S`, you can never reach a
///    terminal state OR a state that has at least one non-deterministic transition
///    (i.e., a state where an agent/human can take over).
///
/// The default `actor` when not specified is `"agent"` (non-deterministic),
/// so only transitions explicitly set `actor: deterministic` are included.
///
/// Returns the sorted list of state names caught in such a loop.
pub fn deterministic_loops(definition: &Value) -> Vec<String> {
    let states = match definition.get("states").and_then(Value::as_object) {
        Some(s) => s,
        None => return vec![],
    };

    // Build the deterministic-only adjacency: for each state, which states
    // can be reached by following ONLY actor:deterministic transitions.
    // We also track which states are "escape points":
    //   - terminal states, OR
    //   - states that have at least one non-deterministic transition.
    let mut det_successors: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut is_escape: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (state_name, state_def) in states {
        let mut has_non_det = false;
        let mut successors = Vec::new();

        let is_terminal = state_def
            .get("terminal")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_terminal {
            is_escape.insert(state_name.to_string());
        }

        if let Some(ts) = state_def.get("transitions").and_then(Value::as_object) {
            for (_t_name, t_def) in ts {
                let actor = t_def
                    .get("actor")
                    .and_then(Value::as_str)
                    .unwrap_or("agent");
                if actor == "deterministic" {
                    // Collect direct target
                    if let Some(target) = t_def.get("target").and_then(Value::as_str) {
                        successors.push(target.to_string());
                    }
                    // Collect branch targets
                    if let Some(branches) = t_def.get("branches").and_then(Value::as_array) {
                        for branch in branches {
                            if let Some(bt) = branch.get("target").and_then(Value::as_str) {
                                successors.push(bt.to_string());
                            }
                        }
                    }
                } else {
                    has_non_det = true;
                }
            }
        }

        if has_non_det {
            is_escape.insert(state_name.to_string());
        }

        det_successors.insert(state_name.to_string(), successors);
    }

    // For each state, compute its deterministic-only reachable set (including
    // itself). A state is a "loop state" if:
    //   1. Its deterministic closure contains NO escape state.
    //   2. It is part of a cycle (reachable from itself via deterministic edges).
    let state_names: Vec<String> = states.keys().map(|s| s.to_string()).collect();

    let mut looping_states: Vec<String> = Vec::new();

    for state in &state_names {
        // BFS/DFS over deterministic edges from `state`
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(state.clone());
        visited.insert(state.clone());

        let mut found_escape = false;
        let mut can_reach_self_again = false;

        while let Some(current) = queue.pop_front() {
            if is_escape.contains(&current) {
                found_escape = true;
                break;
            }
            if let Some(succs) = det_successors.get(&current) {
                for succ in succs {
                    if succ == state && &current != state {
                        // Can reach starting state from a different state → cycle
                        can_reach_self_again = true;
                    }
                    if !visited.contains(succ) {
                        visited.insert(succ.clone());
                        queue.push_back(succ.clone());
                    }
                }
            }
        }

        // A state is in an infinite deterministic loop if:
        // - Its deterministic closure has no escape, AND
        // - It is part of a cycle (the closure includes a back-edge to it, OR
        //   the closure contains only deterministic states in a cycle).
        //
        // Since we included `state` itself in the visited set and we walk
        // deterministic edges, a cycle exists if any state in the closure
        // has a deterministic successor that is also in the closure AND
        // we can reach `state` again, OR more generally: if the closure
        // contains a cycle. We check if any node in the closure can reach
        // `state` (cycle involving `state`).
        if !found_escape && can_reach_self_again {
            looping_states.push(state.clone());
        }
    }

    looping_states.sort();
    looping_states
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def() -> Value {
        json!({ "initialState": "a", "states": {
            "a": { "transitions": { "go": { "target": "b" } } },
            "b": { "transitions": { "fin": { "target": "c" } } },
            "c": { "terminal": true, "transitions": {} },
            "orphan": { "transitions": { "x": { "target": "c" } } }
        }})
    }

    #[test]
    fn enumerates_edges() {
        let w = walk(&def());
        assert!(w.edges.contains(&Edge {
            state: "a".into(),
            transition: "go".into(),
            target: "b".into()
        }));
        assert!(w.edges.iter().any(|e| e.transition == "fin"));
    }

    #[test]
    fn finds_orphan() {
        let w = walk(&def());
        assert!(w.reachable_states.contains("a"));
        assert!(w.reachable_states.contains("c"));
        assert_eq!(w.orphan_states, vec!["orphan".to_string()]);
    }

    #[test]
    fn reachable_from_b_contains_b_and_c_not_a() {
        let d = def();
        let r = reachable_from(&d, "b");
        assert!(r.contains("b"), "b should be reachable from b (inclusive)");
        assert!(r.contains("c"), "c should be reachable from b via fin");
        assert!(!r.contains("a"), "a is not reachable from b");
    }

    // ── deterministic_loops tests ─────────────────────────────────────────────

    /// A↔B mutual deterministic loop with no escape: both flagged.
    #[test]
    fn det_loop_ab_mutual_both_flagged() {
        let d = json!({ "initialState": "a", "states": {
            "a": { "transitions": { "ab": { "target": "b", "actor": "deterministic", "executor": { "kind": "noop" } } } },
            "b": { "transitions": { "ba": { "target": "a", "actor": "deterministic", "executor": { "kind": "noop" } } } }
        }});
        let loops = deterministic_loops(&d);
        assert!(
            loops.contains(&"a".to_string()),
            "a should be flagged: {loops:?}"
        );
        assert!(
            loops.contains(&"b".to_string()),
            "b should be flagged: {loops:?}"
        );
    }

    /// Deterministic chain A→B→done (terminal): none flagged.
    #[test]
    fn det_chain_to_terminal_not_flagged() {
        let d = json!({ "initialState": "a", "states": {
            "a": { "transitions": { "ab": { "target": "b", "actor": "deterministic", "executor": { "kind": "noop" } } } },
            "b": { "transitions": { "fin": { "target": "done", "actor": "deterministic", "executor": { "kind": "noop" } } } },
            "done": { "terminal": true, "transitions": {} }
        }});
        let loops = deterministic_loops(&d);
        assert!(
            loops.is_empty(),
            "terminating chain should not be flagged: {loops:?}"
        );
    }

    /// Deterministic state that hands off to an agent state: not flagged.
    #[test]
    fn det_state_hands_off_to_agent_not_flagged() {
        let d = json!({ "initialState": "a", "states": {
            "a": { "transitions": {
                "ab": { "target": "b", "actor": "deterministic", "executor": { "kind": "noop" } }
            }},
            "b": { "transitions": {
                // non-deterministic (agent) transition — escape point
                "review": { "target": "done", "actor": "agent", "executor": { "kind": "noop" } }
            }},
            "done": { "terminal": true, "transitions": {} }
        }});
        let loops = deterministic_loops(&d);
        assert!(
            loops.is_empty(),
            "handoff to agent state should not be flagged: {loops:?}"
        );
    }

    /// A state with both deterministic and non-deterministic transitions is itself an escape.
    #[test]
    fn state_with_mixed_transitions_is_escape() {
        let d = json!({ "initialState": "a", "states": {
            "a": { "transitions": {
                "loop": { "target": "a", "actor": "deterministic", "executor": { "kind": "noop" } },
                "done": { "target": "done", "actor": "agent", "executor": { "kind": "noop" } }
            }},
            "done": { "terminal": true, "transitions": {} }
        }});
        let loops = deterministic_loops(&d);
        assert!(
            loops.is_empty(),
            "mixed-transition state should not be flagged: {loops:?}"
        );
    }
}
