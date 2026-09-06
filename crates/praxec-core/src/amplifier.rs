//! Coarse deliverables with explicit execution ownership and tool verification.
//!
//! This opt-in profile proves graph properties, not the semantic adequacy of
//! intent, model output, or operator-authored checks. Executors remain trusted.
use anyhow::{Result, bail, ensure};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};

const INPUTS: [&str; 4] = ["intent", "context", "deliverable", "acceptance"];

/// Validate immutable task input even when an embedding bypasses config loading.
pub(crate) fn validate_input(def: &Value, input: &Value) -> Result<()> {
    if def.get("amplifier").is_none() {
        return Ok(());
    }
    validate(def)?;
    for key in INPUTS {
        ensure!(
            input[key].as_str().is_some_and(|s| !s.trim().is_empty()),
            "AMPLIFIER_INPUT_INVALID: {key} must be a nonempty string"
        );
    }
    Ok(())
}

/// Fail closed on unclassified work, hidden execution, cycles, and unchecked
/// completion. Graph analysis considers every branch regardless of its guard.
pub fn validate(def: &Value) -> Result<()> {
    let Some(policy) = def.get("amplifier") else {
        return Ok(());
    };
    let budget = policy["max_model_steps"].as_u64().unwrap_or(0);
    ensure!(
        policy.as_object().is_some_and(|p| p.len() == 1) && (1..=16).contains(&budget),
        "AMPLIFIER_INVALID: declare only max_model_steps (1..=16)"
    );
    for key in INPUTS {
        let schema = &def["inputSchema"];
        ensure!(
            schema["type"] == "object"
                && schema["required"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|v| v == key))
                && schema["properties"][key]["type"] == "string",
            "AMPLIFIER_INVALID: inputSchema must require string {key}"
        );
    }
    for slot in ["candidate", "verification"] {
        ensure!(
            def["blackboard"][slot]["type"] == "object",
            "AMPLIFIER_INVALID: declare a typed object blackboard slot for {slot}"
        );
    }
    let states = def["states"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("AMPLIFIER_INVALID: states required"))?;
    let initial = def["initialState"].as_str().unwrap_or("");
    ensure!(
        states.contains_key(initial),
        "AMPLIFIER_INVALID: unknown initialState"
    );
    let mut graph: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (name, state) in states {
        ensure!(
            state.get("onEnter").is_none()
                && state.get("delegate").is_none()
                && state.get("while").is_none()
                && state.get("slots").is_none(),
            "AMPLIFIER_INVALID: {name} has hidden onEnter/delegate/while/slots effects; use classified acyclic transitions"
        );
        if state["terminal"] == true {
            ensure!(
                matches!(state["outcome"].as_str(), Some("success" | "failure")),
                "AMPLIFIER_INVALID: terminal {name} requires explicit success|failure outcome"
            );
            ensure!(
                state["transitions"]
                    .as_object()
                    .is_none_or(|ts| ts.is_empty()),
                "AMPLIFIER_INVALID: terminal {name} must not have transitions"
            );
        }
        let mut edges = Vec::new();
        for (transition_name, t) in state["transitions"].as_object().into_iter().flatten() {
            // This reserved command cancels in dispatch_once before any executor
            // or target can run. It must remain available to halt long work.
            if transition_name == crate::config::HALT_TRANSITION {
                ensure!(
                    t.get("executor").is_none()
                        && t.get("output").is_none()
                        && t["actor"] == "agent"
                        && t["lightweight"] == true
                        && t["target"] == name.as_str(),
                    "AMPLIFIER_INVALID: halt must be a caller-driven, lightweight, effect-free self-loop"
                );
                continue;
            }
            let role = t["work_kind"].as_str().unwrap_or("");
            let executor = &t["executor"];
            if name == initial {
                // An older runtime must reject a real, unknown guard before any
                // paid work, rather than silently ignoring profile metadata.
                ensure!(
                    role == "deterministic"
                        && t["actor"] == "deterministic"
                        && executor["kind"] == "noop"
                        && t.get("output").is_none()
                        && t["guards"].as_array().is_some_and(|gs| gs
                            .iter()
                            .any(|g| g == &serde_json::json!({"kind":"amplifier_contract"}))),
                    "AMPLIFIER_INVALID: initial transitions require an effect-free deterministic noop with amplifier_contract guard"
                );
            }
            ensure!(
                t.pointer("/reliability/fallback").is_none(),
                "AMPLIFIER_INVALID: fallback executors can bypass classified work; use explicit transitions"
            );
            let kind = executor["kind"].as_str().unwrap_or("");
            let actor = t["actor"].as_str().unwrap_or("");
            match role {
                "generative" => {
                    ensure!(
                        executor.get("untrusted").is_none_or(|v| v == false),
                        "AMPLIFIER_INVALID: untrusted agent mode does not enforce this profile's grounding/budget contract"
                    );
                    ensure!(
                        t.pointer("/reliability/retry/maxAttempts")
                            .is_none_or(|v| v == 1),
                        "AMPLIFIER_INVALID: model retries belong within the agent step budget or a new task attempt"
                    );
                    ensure!(
                        actor == "agent" && kind == "agent",
                        "AMPLIFIER_INVALID: {name}/{transition_name} generative work requires an agent executor"
                    );
                    ensure!(
                        executor["enforce_input_grounding"] == true,
                        "AMPLIFIER_INVALID: generative work must preserve input grounding"
                    );
                    ensure!(
                        t["output"]["candidate"] == "$.output",
                        "AMPLIFIER_INVALID: preserve the generated candidate as complete executor output"
                    );
                    let seconds = executor["max_seconds"].as_u64().unwrap_or(0);
                    let total = executor["step_budget_seconds"].as_u64().unwrap_or(0);
                    ensure!(
                        seconds > 0 && seconds <= total && total <= 3600,
                        "AMPLIFIER_INVALID: generative work requires 0 < max_seconds <= step_budget_seconds <= 3600"
                    );
                    let goal = executor["goal"].as_str().unwrap_or("");
                    for key in INPUTS {
                        ensure!(
                            goal.contains(&format!("{{{{ $.workflow.input.{key} }}}}")),
                            "AMPLIFIER_INVALID: generative goal must include immutable task input {key}"
                        );
                    }
                }
                "deterministic" | "verification" => {
                    ensure!(
                        actor == "deterministic",
                        "AMPLIFIER_INVALID: {name}/{transition_name} mechanical work must be auto-driven"
                    );
                    ensure!(
                        matches!(kind, "script" | "mcp" | "noop"),
                        "AMPLIFIER_INVALID: mechanical work requires a direct script, mcp, or noop executor; opaque/composite work is unsupported"
                    );
                    if role == "verification" {
                        ensure!(
                            kind == "script" && executor["treatNonZeroAsFailure"] == true,
                            "AMPLIFIER_INVALID: verification requires a script with treatNonZeroAsFailure: true"
                        );
                        ensure!(
                            t["output"]["verification"] == "$.output"
                                && t["output"].as_object().is_some_and(|o| o.len() == 1),
                            "AMPLIFIER_INVALID: preserve complete tool output in verification, not a caller/model pass claim"
                        );
                        let args = executor["args"].as_array();
                        for binding in [
                            "{{ $.context.candidate }}",
                            "{{ $.workflow.input.acceptance }}",
                        ] {
                            ensure!(
                                args.is_some_and(|a| a.iter().any(|v| v == binding)),
                                "AMPLIFIER_INVALID: verification args must bind {binding}"
                            );
                        }
                    }
                }
                "intent" => {
                    ensure!(
                        actor == "human"
                            && t.get("executor").is_none()
                            && t.get("output").is_none(),
                        "AMPLIFIER_INVALID: intent decisions must be executor-free human transitions without context writes"
                    );
                }
                _ => bail!(
                    "AMPLIFIER_INVALID: {name}/{transition_name} requires work_kind generative|deterministic|verification|intent"
                ),
            }
            if kind == "script" {
                ensure!(
                    executor["subject"]
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && !s.contains("{{")),
                    "AMPLIFIER_INVALID: scripts must reference an operator-defined subject"
                );
            }
            let target = t["target"].as_str().unwrap_or("");
            ensure!(
                states.contains_key(target),
                "AMPLIFIER_INVALID: unknown target {target}"
            );
            edges.push((target, role));
            for branch in t["branches"].as_array().into_iter().flatten() {
                let target = branch["target"].as_str().unwrap_or("");
                ensure!(
                    states.contains_key(target),
                    "AMPLIFIER_INVALID: unknown branch target {target}"
                );
                edges.push((target, role));
            }
        }
        ensure!(
            state["terminal"] == true || !edges.is_empty(),
            "AMPLIFIER_INVALID: nonterminal {name} has no classified continuation"
        );
        graph.insert(name, edges);
    }
    if let Some(timeout) = def.get("onTimeout") {
        let target = timeout["target"].as_str().unwrap_or("");
        ensure!(
            timeout.as_object().is_some_and(|t| t.len() == 1)
                && states
                    .get(target)
                    .is_some_and(|s| s["terminal"] == true && s["outcome"] == "failure"),
            "AMPLIFIER_INVALID: timeout may only enter a failure terminal without execution"
        );
    }
    // Kahn's algorithm bounds work without enumerating paths exponentially.
    let mut indegree: HashMap<&str, usize> = states.keys().map(|s| (s.as_str(), 0)).collect();
    for edges in graph.values() {
        for (target, _) in edges {
            *indegree.get_mut(target).expect("checked target") += 1;
        }
    }
    let mut ready: VecDeque<_> = indegree
        .iter()
        .filter_map(|(s, n)| (*n == 0).then_some(*s))
        .collect();
    let mut visited = 0;
    while let Some(state) = ready.pop_front() {
        visited += 1;
        for (target, _) in &graph[state] {
            let count = indegree.get_mut(target).expect("checked target");
            *count -= 1;
            if *count == 0 {
                ready.push_back(target);
            }
        }
    }
    ensure!(
        visited == states.len(),
        "AMPLIFIER_INVALID: cycles require a new bounded task attempt, not an unbounded model loop"
    );
    let mut queue = VecDeque::from([(initial, 0u64, false)]);
    let mut seen = HashSet::new();
    let mut success = false;
    while let Some((state, models, verified)) = queue.pop_front() {
        if !seen.insert((state, models, verified)) {
            continue;
        }
        ensure!(
            models <= budget,
            "AMPLIFIER_INVALID: path exceeds max_model_steps"
        );
        if states[state]["terminal"] == true && states[state]["outcome"] == "success" {
            ensure!(
                models > 0 && verified,
                "AMPLIFIER_INVALID: success path must produce a deliverable then verify it after the last mechanical/generative change"
            );
            success = true;
        }
        for (target, role) in &graph[state] {
            let next_models = models + u64::from(*role == "generative");
            let next_verified = match *role {
                "verification" => models > 0,
                "intent" => verified,
                _ => false,
            };
            queue.push_back((target, next_models, next_verified));
        }
    }
    ensure!(
        success,
        "AMPLIFIER_INVALID: no reachable verified success terminal"
    );
    Ok(())
}
