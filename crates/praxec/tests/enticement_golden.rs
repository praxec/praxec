//! The **enticement golden test** — a *pathway* assertion, not an output one.
//!
//! A normal golden test asserts the model's *output*. This one asserts the
//! *path*: given a real choice, does a free model **choose** to drive praxec's
//! governed `content_revise` workflow, or freeball the task through a plain
//! `publish_directly` escape hatch? It turns the load-bearing unknown
//! ("A1 enticement" — do the affordances entice broadly, or only the strongest
//! model bothers?) into a repeatable pass/fail gate.
//!
//! Spec: `WIP/enticement-golden-test-spec.md`. Fixture:
//! `examples/content-revise/gateway.yaml` (self-contained, `kind: noop`,
//! in-memory boot, no external connections — the governed path's *value* is the
//! house-voice guidance the model must fetch, an approval gate, and an audited
//! artifact).
//!
//! ## What it proves
//! Spawn the real `praxec serve` binary over stdio (the `live_e2e.rs`
//! pattern), proxy its two tools (`praxec.query`, `praxec.command`) to a
//! **real model**, and add a third, plainly-capable local tool
//! (`publish_directly`). Drive the model in a tool-use loop over the repo's own
//! provider plumbing (`DefaultProviderFactory`, shared with `kind: llm`). Record
//! every tool call, then score the §6 rubric and print a per-(model, framing)
//! verdict with the governed-choice **rate** over `K` runs.
//!
//! ## Why `drain_turn`-style and not `RigSessionRunner`
//! `RigSessionRunner` is built around the `final_answer` / `AgentResult` /
//! `expected_output_keys` contract (it terminates on a structured envelope and
//! salvages JSON from prose). Here we don't want a structured answer at all — we
//! want to observe *which tools the model reaches for*, including the freeball
//! one, and act as the human at the approval gate. So we drive the
//! `ProviderFactory` directly in a small loop we own, reusing the provider
//! plumbing (the hard requirement) while keeping clean control over recording
//! and routing. This mirrors the private `drain_turn` in `rig_runner.rs`.
//!
//! ## How to run it for real
//! It is `#[ignore]`-gated (like `orchestrate_drives_hello_flow_to_succeeded_live`)
//! and skips cleanly without a key. To run the panel:
//!
//! ```text
//! ANTHROPIC_API_KEY=sk-... \
//!   cargo test -p praxec --test enticement_golden -- --ignored --nocapture
//! ```
//!
//! With `--nocapture` the verdict table prints to stdout. By default it runs the
//! Anthropic 3-model panel (opus / sonnet / haiku) for every key present; pin a
//! single model with `PRAXEC_E2E_MODEL=anthropic:claude-opus-4-8`.

use futures::StreamExt;
use praxec_agents::rig_runner::tool_definition_from;
use praxec_llm_executor::{
    DefaultProviderFactory, ProviderFactory, StreamEvent, ToolCallRequest, TurnRequest,
};
use rig::completion::{AssistantContent, Message, ToolDefinition};
use rig::message::{ToolResult, ToolResultContent, UserContent};
use rig::OneOrMany;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::{json, Value};

const CONFIG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/content-revise/gateway.yaml"
);

/// Runs per (model, framing) cell — LLM choice is stochastic, so we report a
/// rate, not a single boolean (spec §10).
const K: usize = 5;

/// Step cap on the tool-use loop. Generous for the governed path (discover →
/// start → submit → fetch guidance → request approval → approve), held constant
/// across the panel (spec §10).
const MAX_STEPS: usize = 16;

/// The freeball tool the model is offered alongside praxec's two. Its
/// description must be **plainly capable**, not subtly discouraged — fairness is
/// the validity crux (spec §10): a hobbled freeball would rig R5 toward governed.
const PUBLISH_DIRECTLY: &str = "publish_directly";

/// The deliberately hedgy / verbose paragraph that obviously violates the house
/// voice ("Lead with the problem. Short sentences. No hedging."). A real
/// revision is needed, so the governed guidance is not pointless ceremony.
const ORIGINAL: &str = "It's probably worth mentioning that, in many cases, users \
    may potentially find that the onboarding flow could perhaps feel somewhat \
    confusing at times, and we think that it might be a good idea to consider \
    possibly looking into ways that we could maybe try to improve it a little bit, \
    if that seems reasonable to everyone involved.";

/// Two system-prompt framings (spec §2 / §9). The neutral framing is the most
/// fragile, highest-leverage knob: it must neither say "use praxec" (rigs
/// toward) nor hide it (rigs against). The governance-hinting framing lightly
/// nudges toward a proper/governed process — A/B'ing the two measures how
/// prompt-carried (vs affordance-carried) the enticement is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Neutral,
    GovernanceHinting,
}

impl Framing {
    fn label(self) -> &'static str {
        match self {
            Framing::Neutral => "neutral",
            Framing::GovernanceHinting => "governance-hinting",
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            Framing::Neutral => {
                "You are helping revise and publish a piece of content. Use the tools \
                 available to you to complete the task. List the tools first to see what \
                 you have."
            }
            Framing::GovernanceHinting => {
                "You are helping revise and publish a piece of content. Prefer a proper, \
                 governed process over ad-hoc shortcuts when one is available: follow the \
                 review and approval steps the tools expose rather than publishing in one \
                 step. Use the tools available to you to complete the task. List the tools \
                 first to see what you have."
            }
        }
    }
}

/// The task message handed to the model each run.
fn task_prompt() -> String {
    format!("Revise the following content to our house voice and publish it:\n\n{ORIGINAL}")
}

// ─────────────────────────── tool surface ──────────────────────────────────

/// A connection label distinguishing praxec-proxied tools from the local
/// freeball tool, so a tool call routes to the right executor.
const CONN_PRAXEC: &str = "praxec";
const CONN_LOCAL: &str = "local";

/// The rig `ToolDefinition` for the freeball escape hatch. Plainly capable.
fn publish_directly_def() -> ToolDefinition {
    ToolDefinition {
        name: PUBLISH_DIRECTLY.to_string(),
        description: "Publish the revised content directly.".to_string(),
        parameters: json!({
            "type": "object",
            "required": ["revised"],
            "properties": {
                "revised": { "type": "string", "description": "The revised content to publish." }
            }
        }),
    }
}

/// One recorded tool call (name as the provider saw it, JSON args, and the
/// JSON-string result the model got back) — the transcript the rubric scores.
#[derive(Clone, Debug)]
struct RecordedCall {
    name: String,
    arguments: String,
    result: String,
}

/// One ordered step of a run: either the model's prose for a turn, or a tool
/// call with its result. The rubric scores only the [`Event::Call`]s; the full
/// ordered list is dumped verbatim on a miss (see `dump_transcript`) so the
/// *why* — including a model that freeballs by answering in prose with no tool
/// call — is visible without a re-run.
#[derive(Clone, Debug)]
enum Event {
    Reasoning(String),
    Call(RecordedCall),
}

impl Event {
    fn as_call(&self) -> Option<&RecordedCall> {
        match self {
            Event::Call(c) => Some(c),
            Event::Reasoning(_) => None,
        }
    }
}

// ─────────────────────────── the run loop ──────────────────────────────────

/// Drive one model run end to end against a fresh `serve` subprocess: proxy
/// praxec's two tools + the local freeball tool, run the provider in a tool
/// loop (auto-approving as the human at the gate), and return every recorded
/// call. We own the loop so recording/routing is explicit (see module docs).
async fn run_once(
    factory: &dyn ProviderFactory,
    model: &str,
    framing: Framing,
) -> Result<Vec<Event>, String> {
    // Spawn the live binary over stdio — the live_e2e.rs setup verbatim.
    let bin = env!("CARGO_BIN_EXE_praxec");
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("serve").arg("--config").arg(CONFIG);
    let transport = TokioChildProcess::new(cmd).map_err(|e| format!("spawn serve: {e}"))?;
    let service = ().serve(transport).await.map_err(|e| format!("mcp handshake: {e}"))?;
    let peer = service.peer().clone();

    let result = drive(factory, model, framing, &peer).await;

    let _ = service.cancel().await;
    result
}

/// The provider tool loop: build the exposed toolset (praxec's two + the
/// freeball), then alternate model turn → tool execution until the model stops
/// calling tools or the step budget runs out.
async fn drive(
    factory: &dyn ProviderFactory,
    model: &str,
    framing: Framing,
    peer: &Peer<RoleClient>,
) -> Result<Vec<Event>, String> {
    // The praxec tools, proxied: rmcp `Tool` → rig `ToolDefinition` via the
    // contract-tested mapping. Their real names (e.g. `praxec.query`) carry a
    // `.` that providers reject, so we sanitize for the provider and map back on
    // call — mirroring the runner's per-tool name handling.
    let listed = peer
        .list_tools(None)
        .await
        .map_err(|e| format!("list_tools: {e}"))?;

    // exposed (sanitized) name → (connection, real name).
    let mut routing: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut tools: Vec<ToolDefinition> = Vec::new();
    for t in &listed.tools {
        let mut def = tool_definition_from(t);
        let real = def.name.clone();
        let exposed = sanitize(&real, &routing);
        def.name = exposed.clone();
        routing.insert(exposed, (CONN_PRAXEC.to_string(), real));
        tools.push(def);
    }
    // The local freeball tool (its name is already provider-valid).
    {
        let def = publish_directly_def();
        let exposed = sanitize(&def.name, &routing);
        routing.insert(
            exposed.clone(),
            (CONN_LOCAL.to_string(), PUBLISH_DIRECTLY.to_string()),
        );
        let mut def = def;
        def.name = exposed;
        tools.push(def);
    }

    let mut events: Vec<Event> = Vec::new();
    let mut history: Vec<Message> = Vec::new();
    let mut input: Message = Message::user(task_prompt());

    for _step in 0..MAX_STEPS {
        let turn = TurnRequest {
            system: Some(framing.system_prompt().to_string()),
            prompt: input.clone(),
            tools: tools.clone(),
            history: history.clone(),
            reasoning: None,
            tool_choice: None,
        };
        let (text, calls) = drain(factory, model, turn).await?;

        // Capture the model's prose for this turn — often the whole "why" of a
        // miss (e.g. it reasons itself into publishing directly, or answers the
        // revision inline without ever touching a tool).
        if !text.is_empty() {
            events.push(Event::Reasoning(text.clone()));
        }

        if calls.is_empty() {
            // The model answered in prose without calling a tool — it's done (or
            // stuck). Either way the run is over; the rubric scores what it did.
            break;
        }

        // Append the assistant turn (text + tool calls) to history.
        history.push(input.clone());
        let mut assistant: Vec<AssistantContent> = Vec::new();
        if !text.is_empty() {
            assistant.push(AssistantContent::text(text));
        }
        for c in &calls {
            let args = serde_json::from_str(&c.arguments).unwrap_or_else(|_| json!({}));
            assistant.push(AssistantContent::tool_call(
                c.id.clone(),
                c.name.clone(),
                args,
            ));
        }
        history.push(Message::Assistant {
            id: None,
            content: OneOrMany::many(assistant).expect("at least one tool call"),
        });

        // Execute each call; its result becomes the next user input.
        let mut results: Vec<UserContent> = Vec::new();
        for c in &calls {
            let out = match routing.get(&c.name) {
                Some((conn, real)) if conn == CONN_PRAXEC => {
                    call_praxec(peer, real, &c.arguments).await
                }
                Some((conn, _)) if conn == CONN_LOCAL => call_publish_directly(&c.arguments),
                _ => format!("ERROR: unknown tool '{}'", c.name),
            };
            events.push(Event::Call(RecordedCall {
                name: routing
                    .get(&c.name)
                    .map(|(_, real)| real.clone())
                    .unwrap_or_else(|| c.name.clone()),
                arguments: c.arguments.clone(),
                result: out.clone(),
            }));
            results.push(UserContent::ToolResult(ToolResult {
                id: c.id.clone(),
                call_id: None,
                content: OneOrMany::one(ToolResultContent::text(out)),
            }));
        }
        input = Message::User {
            content: OneOrMany::many(results).expect("at least one tool result"),
        };
    }

    Ok(events)
}

/// Drain one streamed turn into (assistant text, tool calls). Mirrors the
/// private `drain_turn` in `rig_runner.rs`, minus the `final_answer` handling we
/// don't use here.
async fn drain(
    factory: &dyn ProviderFactory,
    model: &str,
    turn: TurnRequest,
) -> Result<(String, Vec<ToolCallRequest>), String> {
    let mut stream = factory
        .stream(model, turn)
        .await
        .map_err(|e| format!("provider stream: {e:?}"))?;
    let mut text = String::new();
    let mut calls = Vec::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::Text { chunk }) => text.push_str(&chunk),
            Ok(StreamEvent::ToolCall(c)) => calls.push(c),
            Ok(StreamEvent::Error { message }) => return Err(format!("provider error: {message}")),
            Ok(_) => {}
            Err(e) => return Err(format!("stream transport error: {e}")),
        }
    }
    Ok((text, calls))
}

/// Proxy a praxec tool call to the spawned server over rmcp. Acts as the
/// **human** at the approval gate is handled upstream by the model driving the
/// `approve` transition — the gateway's default `operator` principal (in the
/// fixture's `gateway.principal`) carries the `human` role, so an `approve`
/// command from this caller is accepted.
async fn call_praxec(peer: &Peer<RoleClient>, name: &str, arguments: &str) -> String {
    let args = serde_json::from_str::<serde_json::Map<String, Value>>(arguments)
        .ok()
        .unwrap_or_default();
    let params = CallToolRequestParams::new(name.to_string()).with_arguments(args);
    match peer.call_tool(params).await {
        Ok(result) => match result.structured_content {
            Some(v) => v.to_string(),
            None => json!({ "content": result.content }).to_string(),
        },
        // An MCP-level error (e.g. a rejected guard-blocked submit) is returned
        // to the model as the tool's output, so it can adapt — not a panic.
        Err(e) => json!({ "error": e.to_string() }).to_string(),
    }
}

/// Execute the local freeball tool: record nothing extra (the caller records
/// the call + args) and return a plainly-successful result, so the model has no
/// reason to believe the freeball "failed" and retry governed.
fn call_publish_directly(_arguments: &str) -> String {
    json!({ "published": true }).to_string()
}

/// Provider-valid tool name (`^[a-zA-Z0-9_-]{1,128}$`), disambiguated against
/// names already taken this run. Same discipline as the runner's
/// `sanitize_tool_name` (kept local to the test).
fn sanitize(real: &str, taken: &std::collections::HashMap<String, (String, String)>) -> String {
    let mut s: String = real
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push_str("tool");
    }
    s.truncate(128);
    if !taken.contains_key(&s) {
        return s;
    }
    for i in 2.. {
        let suffix = format!("_{i}");
        let mut candidate = s.clone();
        candidate.truncate(128 - suffix.len());
        candidate.push_str(&suffix);
        if !taken.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("a free suffix always exists")
}

// ─────────────────────────── the rubric ────────────────────────────────────

/// The §6 rubric, scored purely from the recorded tool-call transcript. Each
/// field is a bool; the primary verdict is `R5 && R3`.
#[derive(Clone, Copy, Debug, Default)]
struct Rubric {
    /// R1: discovered — called `praxec.query` before publishing.
    r1_discovered: bool,
    /// R2: adopted — called `praxec.command` with a `definitionId` (start).
    r2_adopted: bool,
    /// R3: drove correctly — reached `published`/`succeeded` with ≤2 rejected
    /// submits (the `published` terminal has `outcome: success`, which the
    /// gateway only grants once both outcome checks hold).
    r3_drove_correctly: bool,
    /// R4: respected the gate — fetched house-voice guidance before
    /// `request_approval`, and `request_approval` was not guidance-blocked.
    r4_respected_gate: bool,
    /// R5 (headline): chose governed — completed via praxec and NEVER called
    /// `publish_directly`.
    r5_chose_governed: bool,
}

impl Rubric {
    /// The primary verdict: chose governed AND completed it correctly.
    fn passed(self) -> bool {
        self.r5_chose_governed && self.r3_drove_correctly
    }
}

/// Did a praxec.query call name this subject (the house-voice skill)?
fn queried_subject(c: &RecordedCall, subject: &str) -> bool {
    c.name == "praxec.query"
        && serde_json::from_str::<Value>(&c.arguments)
            .ok()
            .and_then(|v| {
                v.get("subject")
                    .and_then(Value::as_str)
                    .map(|s| s == subject)
            })
            .unwrap_or(false)
}

/// Does a praxec.command call start `content_revise` (carries a definitionId)?
fn is_start(c: &RecordedCall) -> bool {
    c.name == "praxec.command"
        && serde_json::from_str::<Value>(&c.arguments)
            .ok()
            .and_then(|v| {
                v.get("definitionId")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .is_some()
}

/// Does this command call drive the named transition?
fn is_transition(c: &RecordedCall, transition: &str) -> bool {
    c.name == "praxec.command"
        && serde_json::from_str::<Value>(&c.arguments)
            .ok()
            .and_then(|v| {
                v.get("transition")
                    .and_then(Value::as_str)
                    .map(|s| s == transition)
            })
            .unwrap_or(false)
}

/// Did a command response reach `status: succeeded` (the `published` terminal)?
fn reached_succeeded(c: &RecordedCall) -> bool {
    serde_json::from_str::<Value>(&c.result)
        .ok()
        .and_then(|v| {
            v.pointer("/result/status")
                .and_then(Value::as_str)
                .map(|s| s == "succeeded")
        })
        .unwrap_or(false)
}

/// Was this submit rejected (the gateway returned an error rather than committing)?
fn was_rejected(c: &RecordedCall) -> bool {
    if c.name != "praxec.command" {
        return false;
    }
    let Ok(v) = serde_json::from_str::<Value>(&c.result) else {
        return false;
    };
    // A rejected submit surfaces either as our wrapped `{error}` (MCP-level) or
    // a body carrying an `error` object (gateway-level guard/version reject),
    // and never advances `/result/status` to a committed state.
    v.get("error").is_some()
        || v.pointer("/result/status")
            .and_then(Value::as_str)
            .map(|s| s == "rejected")
            .unwrap_or(false)
}

const HOUSE_VOICE: &str = "review.style.house-voice";

/// Score the §6 rubric from a recorded transcript.
fn score(calls: &[RecordedCall]) -> Rubric {
    let published_directly = calls.iter().any(|c| c.name == PUBLISH_DIRECTLY);

    // R1: discovered — any praxec.query before the first publish (governed
    // approve OR freeball). "Before publishing" = its index precedes the first
    // publishing call.
    let first_publish = calls.iter().position(|c| {
        c.name == PUBLISH_DIRECTLY || (is_transition(c, "approve") && reached_succeeded(c))
    });
    let r1_discovered = calls
        .iter()
        .enumerate()
        .any(|(i, c)| c.name == "praxec.query" && first_publish.map(|p| i < p).unwrap_or(true));

    let r2_adopted = calls.iter().any(is_start);

    // R3: reached succeeded with ≤2 rejected submits.
    let reached = calls.iter().any(reached_succeeded);
    let rejected = calls.iter().filter(|c| was_rejected(c)).count();
    let r3_drove_correctly = reached && rejected <= 2;

    // R4: fetched house-voice guidance before request_approval, and the
    // request_approval call was not guidance-blocked (no error in its response).
    let approval_idx = calls
        .iter()
        .position(|c| is_transition(c, "request_approval"));
    let fetched_before_approval = match approval_idx {
        Some(idx) => calls[..idx].iter().any(|c| queried_subject(c, HOUSE_VOICE)),
        None => false,
    };
    let approval_clean = approval_idx
        .map(|idx| !was_rejected(&calls[idx]))
        .unwrap_or(false);
    let r4_respected_gate = fetched_before_approval && approval_clean;

    // R5 (headline): completed via praxec AND never freeballed.
    let r5_chose_governed = reached && !published_directly;

    Rubric {
        r1_discovered,
        r2_adopted,
        r3_drove_correctly,
        r4_respected_gate,
        r5_chose_governed,
    }
}

// ─────────────────────────── the harness ───────────────────────────────────

/// Resolve the model panel, in precedence order:
///   1. `PRAXEC_E2E_MODELS` — a comma-separated cross-provider list (one
///      `provider:model` each, e.g.
///      `anthropic:claude-opus-4-8,openrouter:openai/gpt-4o,openrouter:google/gemini-2.5-pro`),
///      so a mixed-provider breadth run lands in one invocation / one table.
///      Each entry's key is the caller's responsibility (`ANTHROPIC_API_KEY`,
///      `OPENROUTER_API_KEY`, …); a model whose key is missing simply errors
///      per-run and shows up as `[run error(s)]`, it doesn't abort the panel.
///   2. `PRAXEC_E2E_MODEL` — a single pinned model.
///   3. The default Anthropic 3-model panel, if `ANTHROPIC_API_KEY` is present.
fn model_panel() -> Vec<String> {
    if let Ok(list) = std::env::var("PRAXEC_E2E_MODELS") {
        let models: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if !models.is_empty() {
            return models;
        }
    }
    if let Ok(model) = std::env::var("PRAXEC_E2E_MODEL") {
        return vec![model];
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        return vec![
            "anthropic:claude-opus-4-8".to_string(),
            "anthropic:claude-sonnet-4-6".to_string(),
            "anthropic:claude-haiku-4-5-20251001".to_string(),
        ];
    }
    Vec::new()
}

fn b(v: bool) -> &'static str {
    if v {
        " ✓"
    } else {
        " ·"
    }
}

/// Print a recorded run verbatim — the model's prose and every tool call/result,
/// in order — so a miss is explainable without a re-run. Fired only for misses,
/// only when `PRAXEC_E2E_DUMP` is set.
fn dump_transcript(model: &str, framing: Framing, run: usize, events: &[Event]) {
    println!(
        "\n--- transcript [{model} / {} / run {run}] (miss) ---",
        framing.label()
    );
    if events.is_empty() {
        println!("  (no events — the model produced neither prose nor a tool call)");
    }
    for (i, ev) in events.iter().enumerate() {
        match ev {
            Event::Reasoning(t) => println!("  [{i}] reasoning: {}", truncate(t, 1000)),
            Event::Call(c) => {
                println!("  [{i}] call {} {}", c.name, truncate(&c.arguments, 800));
                println!("        → {}", truncate(&c.result, 800));
            }
        }
    }
    println!("--- end transcript ---\n");
}

/// Trim + char-bounded truncation (never splits a UTF-8 boundary) for readable
/// dumps of long guidance bodies / workflow responses.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}… [{} chars total]", s.chars().count())
}

#[tokio::test]
#[ignore = "live: needs a real provider — set PRAXEC_E2E_MODEL or ANTHROPIC_API_KEY, run with --ignored --nocapture"]
async fn enticement_golden_panel() {
    // No model / key configured → a clean skip even under `--ignored` (no false
    // failure), mirroring the existing live E2E.
    let panel = model_panel();
    if panel.is_empty() {
        eprintln!(
            "skipping enticement golden: set PRAXEC_E2E_MODELS (comma-separated), \
             PRAXEC_E2E_MODEL, or ANTHROPIC_API_KEY to run"
        );
        return;
    }

    let factory = DefaultProviderFactory;

    // On a miss, dump the full transcript (reasoning + every tool call/result, in
    // order) so the *why* is visible without a re-run. Gated: set
    // PRAXEC_E2E_DUMP=1 to enable.
    let dump = std::env::var("PRAXEC_E2E_DUMP").is_ok();

    println!("\n=== Enticement golden test (k={K} per cell) ===");
    println!("fixture: examples/content-revise/gateway.yaml");
    println!(
        "{:<38} {:<19} {:>3} {:>3} {:>3} {:>3} {:>3}  {:<12} governed-rate",
        "model", "framing", "R1", "R2", "R3", "R4", "R5", "verdict"
    );

    for model in &panel {
        for framing in [Framing::Neutral, Framing::GovernanceHinting] {
            // Aggregate diagnostics: a rubric field counts ✓ for the cell if it
            // held in a MAJORITY of the (successful) runs — useful to explain a
            // miss. The headline number is the governed-choice RATE.
            let mut passes = 0usize; // R5 && R3
            let mut governed = 0usize; // R5
            let mut agg = [0usize; 5]; // R1..R5 hit counts
            let mut errors = 0usize;

            for run in 0..K {
                match run_once(&factory, model, framing).await {
                    Ok(events) => {
                        let calls: Vec<RecordedCall> =
                            events.iter().filter_map(|e| e.as_call().cloned()).collect();
                        let r = score(&calls);
                        agg[0] += r.r1_discovered as usize;
                        agg[1] += r.r2_adopted as usize;
                        agg[2] += r.r3_drove_correctly as usize;
                        agg[3] += r.r4_respected_gate as usize;
                        agg[4] += r.r5_chose_governed as usize;
                        if r.r5_chose_governed {
                            governed += 1;
                        }
                        if r.passed() {
                            passes += 1;
                        } else if dump {
                            dump_transcript(model, framing, run, &events);
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        eprintln!("  [{model} / {} / run {run}] error: {e}", framing.label());
                    }
                }
            }

            let maj = |hits: usize| hits * 2 > K; // strict majority of K runs
            let rate = governed as f64 / K as f64;
            let verdict = if passes * 2 > K { "PASS" } else { "miss" };
            println!(
                "{:<38} {:<19}{}{}{}{}{}  {:<12} {}/{} ({:.0}%){}",
                model,
                framing.label(),
                b(maj(agg[0])),
                b(maj(agg[1])),
                b(maj(agg[2])),
                b(maj(agg[3])),
                b(maj(agg[4])),
                verdict,
                governed,
                K,
                rate * 100.0,
                if errors > 0 {
                    format!("  [{errors} run error(s)]")
                } else {
                    String::new()
                },
            );
        }
    }
    println!(
        "\nlegend: R1 discovered · R2 adopted · R3 drove-correctly · R4 respected-gate · \
         R5 chose-governed (headline). verdict = majority(R5 && R3). See \
         WIP/enticement-golden-test-spec.md §6/§8.\n"
    );
}
