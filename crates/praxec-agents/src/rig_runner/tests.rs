use super::*;
use futures::stream::{self, BoxStream};
use praxec_llm_executor::stream_event::{StopReason, ToolCallRequest};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

// C5 (testing-strategy) — the rmcp `Tool` → rig `ToolDefinition` mapping, now
// pinned in the lib (was untested in the binary's McpToolHost).
fn schema(pairs: &[(&str, serde_json::Value)]) -> Arc<serde_json::Map<String, serde_json::Value>> {
    let mut m = serde_json::Map::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), v.clone());
    }
    Arc::new(m)
}

#[test]
fn tool_definition_carries_the_name() {
    let t = rmcp::model::Tool::new("do_thing", "does a thing", schema(&[]));
    assert_eq!(tool_definition_from(&t).name, "do_thing");
}

#[test]
fn a_description_less_tool_maps_to_an_empty_description() {
    let t = rmcp::model::Tool::new_with_raw("bare", None, schema(&[]));
    assert_eq!(tool_definition_from(&t).description, "");
}

#[test]
fn normal_path_tool_set_is_byte_stable_across_turns() {
    // Prompt-cache poka-yoke: the non-`force_final` tool set (base + final_answer
    // + spill_read) must be byte-identical turn-to-turn so it stays in the
    // cacheable prefix. Assembling it twice from the same base must match exactly —
    // this fails loudly if `final_answer_tool`/`spill_read_tool` ever gain per-call
    // variance (a uuid, a timestamp) that would silently invalidate the cache.
    let base = vec![tool_definition_from(&rmcp::model::Tool::new(
        "read_file",
        "reads",
        schema(&[]),
    ))];
    let assemble = || {
        let mut t = base.clone();
        t.push(RigSessionRunner::final_answer_tool());
        t.push(crate::tool_budget::spill_read_tool());
        t
    };
    assert_eq!(
        serde_json::to_string(&assemble()).unwrap(),
        serde_json::to_string(&assemble()).unwrap(),
        "normal-path tool set must be byte-stable turn-to-turn (cache prefix)"
    );
}

#[test]
fn a_present_description_is_carried_through() {
    let t = rmcp::model::Tool::new("t", "the description", schema(&[]));
    assert_eq!(tool_definition_from(&t).description, "the description");
}

#[test]
fn the_input_schema_becomes_the_parameters() {
    let t = rmcp::model::Tool::new("t", "d", schema(&[("type", json!("object"))]));
    assert_eq!(
        tool_definition_from(&t).parameters,
        json!({ "type": "object" })
    );
}

/// A ProviderFactory that replays one canned event-stream per `stream` call
/// (turn-by-turn). No network/model.
struct ScriptedFactory {
    turns: Mutex<std::collections::VecDeque<Vec<Result<StreamEvent, String>>>>,
}
impl ScriptedFactory {
    fn new(turns: Vec<Vec<Result<StreamEvent, String>>>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
        }
    }
}
#[async_trait]
impl ProviderFactory for ScriptedFactory {
    async fn stream(
        &self,
        _model: &str,
        _turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::pin(stream::iter(events)))
    }
}

struct RecordingHost {
    calls: Mutex<Vec<(String, String, String)>>, // (connection, tool, args)
}
#[async_trait]
impl ToolHost for RecordingHost {
    async fn tools(
        &self,
        connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        let conn = connections
            .first()
            .cloned()
            .unwrap_or_else(|| "conn".into());
        Ok(vec![(
            ToolDefinition {
                name: "lookup".into(),
                description: "look something up".into(),
                parameters: json!({ "type": "object" }),
            },
            conn,
        )])
    }
    async fn call(&self, connection: &str, name: &str, args: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push((connection.into(), name.into(), args.into()));
        Ok(json!({ "found": true }).to_string())
    }
}

fn session(tools: Vec<String>) -> AgentSession {
    AgentSession {
        model: "anthropic:claude-sonnet-4-6".into(),
        system_prompt: None,
        user_prompt: "do the thing".into(),
        tools,
        reasoning_effort: None,
        timeout: Duration::from_secs(5),
        stall_timeout: Duration::from_secs(5),
        expected_output_keys: Vec::new(),
        expected_output_types: Default::default(),
        await_enabled: false,
        identity: Default::default(),
    }
}

fn session_with_keys(tools: Vec<String>, keys: Vec<String>) -> AgentSession {
    AgentSession {
        expected_output_keys: keys,
        expected_output_types: Default::default(),
        ..session(tools)
    }
}

/// Records the prompt (`Message`) handed to the provider each turn, then
/// replays scripted events — so a test can assert what feedback the model saw.
struct PromptCapturingFactory {
    turns: Mutex<std::collections::VecDeque<Vec<Result<StreamEvent, String>>>>,
    prompts: Mutex<Vec<String>>,
}
impl PromptCapturingFactory {
    fn new(turns: Vec<Vec<Result<StreamEvent, String>>>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            prompts: Mutex::new(Vec::new()),
        }
    }
}
#[async_trait]
impl ProviderFactory for PromptCapturingFactory {
    async fn stream(
        &self,
        _model: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        self.prompts
            .lock()
            .unwrap()
            .push(format!("{:?}", turn.prompt));
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::pin(stream::iter(events)))
    }
}

/// Captures the system message the provider received each turn, then replays
/// scripted events — so a test can assert the completion protocol is present.
struct SystemCapturingFactory {
    turns: Mutex<std::collections::VecDeque<Vec<Result<StreamEvent, String>>>>,
    systems: Mutex<Vec<Option<String>>>,
}
#[async_trait]
impl ProviderFactory for SystemCapturingFactory {
    async fn stream(
        &self,
        _model: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        self.systems.lock().unwrap().push(turn.system.clone());
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::pin(stream::iter(events)))
    }
}

/// AGENT_NO_RESULT structural fix: the model MUST be told — in the
/// authoritative system message, every turn — that it finishes by calling
/// `final_answer`. A session with NO skills (system_prompt: None) must still
/// carry the completion protocol, or a tool-looping model never converges.
#[tokio::test]
async fn the_completion_protocol_is_always_in_the_system_message() {
    let factory = Arc::new(SystemCapturingFactory {
        turns: Mutex::new(
            vec![vec![
                Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
                Ok(done()),
            ]]
            .into(),
        ),
        systems: Mutex::new(Vec::new()),
    });
    // No tools, no skills — the bare case that used to ship the model nothing
    // but the goal + the tool's one-line description.
    let report = RigSessionRunner::new(factory.clone())
        .run(session(vec![]))
        .await
        .unwrap();
    assert!(matches!(report.outcome, AgentRunOutcome::Completed(_)));
    let systems = factory.systems.lock().unwrap();
    let sys = systems[0]
        .as_deref()
        .expect("system message must be present");
    assert!(
        sys.contains("final_answer") && sys.contains("\"status\"") && sys.contains("\"output\""),
        "the system message must state the final_answer completion contract, got: {sys}"
    );
}

fn final_answer(args: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCallRequest {
        id: "f1".into(),
        name: "final_answer".into(),
        arguments: args.into(),
    })
}
fn done() -> StreamEvent {
    StreamEvent::Done {
        stop_reason: Some(StopReason::ToolCalls),
    }
}

#[tokio::test]
async fn single_turn_final_answer_completes() {
    let factory = ScriptedFactory::new(vec![vec![
        Ok(final_answer(
            r#"{"status":"success","output":{"answer":42}}"#,
        )),
        Ok(done()),
    ]]);
    let report = RigSessionRunner::new(Arc::new(factory))
        .run(session(vec![]))
        .await
        .unwrap();
    match report.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["answer"], 42),
        other => panic!("expected Completed, got {other:?}"),
    }
}

/// Text-only-stalls (no final_answer, no tool call — the deepseek/glm
/// AGENT_NO_RESULT signature) on any turn that still offers its base tools,
/// and complies with a `final_answer` the moment the runner restricts the
/// turn to `final_answer` only. Records the (tool_choice, tools-offered) per
/// turn so the test can prove the steer — AND that it is done WITHOUT a forced
/// tool_choice (which thinking-mode providers reject with a 400).
struct ForceProbeFactory {
    seen: Mutex<Vec<(Option<rig::message::ToolChoice>, usize)>>,
}
#[async_trait]
impl ProviderFactory for ForceProbeFactory {
    async fn stream(
        &self,
        _model: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        // The forced turn is the one restricted to ONLY `final_answer` (no
        // base tools) — that single-tool restriction is the steer now, NOT a
        // forced tool_choice.
        let forced = turn.tools.len() == 1;
        self.seen
            .lock()
            .unwrap()
            .push((turn.tool_choice.clone(), turn.tools.len()));
        let events = if forced {
            vec![
                Ok(final_answer(
                    r#"{"status":"success","output":{"answer":1}}"#,
                )),
                Ok(done()),
            ]
        } else {
            vec![
                Ok(StreamEvent::Text {
                    chunk: "thinking, not answering".into(),
                }),
                Ok(done()),
            ]
        };
        Ok(Box::pin(stream::iter(events)))
    }
}

/// The AGENT_NO_RESULT poka-yoke: a model that only ever emits text (never
/// calling `final_answer`) would otherwise exhaust `max_turns` → NoResult.
/// After the first text-only stall the runner MUST restrict the next turn to
/// `final_answer` only (steering it to terminate), salvaging the run — and it
/// must do so WITHOUT pinning `tool_choice` (a forced tool_choice 400s
/// thinking-mode models on OpenRouter, the regression this guards against).
#[tokio::test]
async fn a_text_only_stall_restricts_to_final_answer_without_forcing_tool_choice() {
    let factory = Arc::new(ForceProbeFactory {
        seen: Mutex::new(Vec::new()),
    });
    // A wired ToolHost gives turn 0 a real base tool (`lookup`) so the forced
    // turn's single-tool restriction is observable (3 tools → 1 tool).
    let report = RigSessionRunner::new(factory.clone())
        .with_tool_host(Arc::new(RecordingHost {
            calls: Mutex::new(Vec::new()),
        }))
        .run(session(vec!["lookup".into()]))
        .await
        .unwrap();
    match report.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["answer"], 1),
        other => panic!("expected Completed via the final_answer restriction, got {other:?}"),
    }
    let seen = factory.seen.lock().unwrap();
    assert!(
        seen.len() >= 2,
        "expected a stall turn then a restricted turn, saw {}",
        seen.len()
    );
    // Turn 0: base tool + final_answer + spill_read, provider-default tool_choice.
    assert_eq!(seen[0].0, None, "turn 0 must not pin a tool choice");
    assert_eq!(
        seen[0].1, 3,
        "turn 0 must offer the base tool + final_answer + spill_read"
    );
    // Turn 1: RESTRICTED after the stall — ONLY final_answer, and CRUCIALLY
    // still provider-default tool_choice (NEVER Required — that 400s thinking
    // models). The single-tool restriction is the entire steer.
    assert_eq!(
        seen[1].0, None,
        "the restricted turn must NOT pin tool_choice (Required 400s thinking models)"
    );
    assert_eq!(
        seen[1].1, 1,
        "the restricted turn must offer ONLY final_answer"
    );
}

/// AGENT_MALFORMED_RESULT salvage: the model calls `final_answer` but emits
/// the output content at top level WITHOUT the `{status, output}` envelope
/// (missing `status`). The runner must SALVAGE it (wrap as Success + output)
/// and accept it when it conforms — not permanently fail the whole run.
#[tokio::test]
async fn a_final_answer_missing_the_status_envelope_is_salvaged() {
    let factory = ScriptedFactory::new(vec![vec![
        Ok(final_answer(r#"{"verdict":"approved","findings":[]}"#)),
        Ok(done()),
    ]]);
    let report = RigSessionRunner::new(Arc::new(factory))
        .run(session_with_keys(vec![], vec!["verdict".into()]))
        .await
        .unwrap();
    match report.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["verdict"], "approved"),
        other => panic!("expected salvaged Completed, got {other:?}"),
    }
}

#[test]
fn accumulate_usage_sums_turns_and_ignores_zero_turns() {
    let total = accumulate_usage([
        TurnUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
        },
        TurnUsage::default(), // a turn the provider reported no usage for
        TurnUsage {
            prompt_tokens: 250,
            completion_tokens: 60,
        },
    ]);
    assert_eq!(total.prompt_tokens, 350);
    assert_eq!(total.completion_tokens, 80);
}

fn usage(input: u64, output: u64) -> StreamEvent {
    StreamEvent::Usage(praxec_llm_executor::TokenUsage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: None,
    })
}

#[tokio::test]
async fn report_sums_usage_across_turns_and_carries_model() {
    // Turn 0: tool call (usage 100/20). Turn 1: final_answer (usage 250/60).
    // The report must sum both turns, including the final-answer turn.
    let factory = ScriptedFactory::new(vec![
        vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: "t1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":"x"}"#.into(),
            })),
            Ok(usage(100, 20)),
            Ok(done()),
        ],
        vec![
            Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
            Ok(usage(250, 60)),
            Ok(done()),
        ],
    ]);
    let runner = RigSessionRunner::new(Arc::new(factory)).with_tool_host(Arc::new(RecordingHost {
        calls: Mutex::new(Vec::new()),
    }));
    let report = runner.run(session(vec!["conn".into()])).await.unwrap();
    assert_eq!(report.prompt_tokens, 350);
    assert_eq!(report.completion_tokens, 80);
    assert_eq!(report.model, "anthropic:claude-sonnet-4-6");
}

#[tokio::test]
async fn report_usage_defaults_to_zero_when_provider_reports_none() {
    // No StreamEvent::Usage on the turn → zero tokens, never a failure.
    let factory = ScriptedFactory::new(vec![vec![
        Ok(final_answer(
            r#"{"status":"success","output":{"answer":1}}"#,
        )),
        Ok(done()),
    ]]);
    let report = RigSessionRunner::new(Arc::new(factory))
        .run(session(vec![]))
        .await
        .unwrap();
    assert_eq!(report.prompt_tokens, 0);
    assert_eq!(report.completion_tokens, 0);
}

#[tokio::test]
async fn a_conformant_json_text_answer_is_salvaged_when_final_answer_is_skipped() {
    // The model answers in TEXT (no `final_answer` call), but the text is a
    // JSON object meeting the criteria — accept it instead of NoResult.
    let factory = ScriptedFactory::new(vec![vec![
        Ok(StreamEvent::Text {
            chunk: r#"Here you go: {"verdict":"pass","score":9}"#.into(),
        }),
        Ok(done()),
    ]]);
    let report = RigSessionRunner::new(Arc::new(factory))
        .run(session_with_keys(vec![], vec!["verdict".into()]))
        .await
        .unwrap();
    match report.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["verdict"], "pass"),
        other => panic!("expected a salvaged Completed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_nonconforming_answer_is_nudged_in_session_and_can_recover() {
    // Turn 0: a text answer MISSING the required key. The runner must NOT
    // give up — it must feed back the contract and reach turn 1, where the
    // model calls final_answer correctly.
    let factory = Arc::new(PromptCapturingFactory::new(vec![
        vec![
            Ok(StreamEvent::Text {
                chunk: r#"{"wrong":"shape"}"#.into(),
            }),
            Ok(done()),
        ],
        vec![
            Ok(final_answer(
                r#"{"status":"success","output":{"verdict":"pass"}}"#,
            )),
            Ok(done()),
        ],
    ]));
    let report = RigSessionRunner::new(factory.clone())
        .run(session_with_keys(vec![], vec!["verdict".into()]))
        .await
        .unwrap();
    assert!(
        matches!(report.outcome, AgentRunOutcome::Completed(_)),
        "got {:?}",
        report.outcome
    );
    let prompts = factory.prompts.lock().unwrap();
    assert!(
        prompts.len() >= 2,
        "must run a second turn after the nudge, saw {}",
        prompts.len()
    );
    assert!(
        prompts[1].contains("final_answer"),
        "turn 1's prompt must nudge toward final_answer, got: {}",
        prompts[1]
    );
}

#[tokio::test]
async fn nonconforming_text_every_turn_is_bounded_and_ends_no_result() {
    // The feedback loop must terminate: a model that never conforms exhausts
    // the turn budget and yields NoResult (no hang).
    let looping = vec![
        Ok(StreamEvent::Text {
            chunk: r#"{"wrong":"x"}"#.into(),
        }),
        Ok(done()),
    ];
    let turns = std::iter::repeat_n(looping, DEFAULT_MAX_TURNS as usize + 2).collect();
    let report = RigSessionRunner::new(Arc::new(ScriptedFactory::new(turns)))
        .run(session_with_keys(vec![], vec!["verdict".into()]))
        .await
        .unwrap();
    assert_eq!(report.outcome, AgentRunOutcome::NoResult);
}

#[tokio::test]
async fn tool_loop_executes_tool_then_completes_on_next_turn() {
    // Turn 0: the model calls `lookup`. Turn 1: it calls `final_answer`.
    let factory = ScriptedFactory::new(vec![
        vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: "t1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":"x"}"#.into(),
            })),
            Ok(done()),
        ],
        vec![
            Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
            Ok(done()),
        ],
    ]);
    let host = Arc::new(RecordingHost {
        calls: Mutex::new(vec![]),
    });
    let runner = RigSessionRunner::new(Arc::new(factory)).with_tool_host(host.clone());
    let report = runner.run(session(vec!["conn".into()])).await.unwrap();

    match report.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["ok"], true),
        other => panic!("expected Completed after the tool loop, got {other:?}"),
    }
    let calls = host.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "the tool was executed once");
    assert_eq!(calls[0].0, "conn", "routed to the declared connection");
    assert_eq!(calls[0].1, "lookup");
    assert!(
        report.transcript.contains("[tool lookup]"),
        "tool result in transcript"
    );
}

/// A ToolHost whose every `lookup` returns a fixed sizeable payload — so the
/// history grows by a known amount each turn (the codebase-reading loop's
/// signature, minus the truncation since each chunk is under the per-result
/// cap).
struct BigResultHost {
    chunk: String,
}
#[async_trait]
impl ToolHost for BigResultHost {
    async fn tools(
        &self,
        connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        let conn = connections
            .first()
            .cloned()
            .unwrap_or_else(|| "conn".into());
        Ok(vec![(
            ToolDefinition {
                name: "lookup".into(),
                description: "look something up".into(),
                parameters: json!({ "type": "object" }),
            },
            conn,
        )])
    }
    async fn call(&self, _conn: &str, _name: &str, _args: &str) -> Result<String, String> {
        Ok(self.chunk.clone())
    }
}

/// Calls `lookup` every turn (growing history) until `answer_after` turns,
/// then `final_answer`. Records the LARGEST history (serialized) it was ever
/// handed — the request size the provider would see.
struct HistoryProbeFactory {
    calls: Mutex<u32>,
    max_history_bytes: Mutex<usize>,
    answer_after: u32,
}
#[async_trait]
impl ProviderFactory for HistoryProbeFactory {
    async fn stream(
        &self,
        _model: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        let hb = history_bytes(&turn.history);
        {
            let mut m = self.max_history_bytes.lock().unwrap();
            *m = (*m).max(hb);
        }
        let n = {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            *c
        };
        let events = if n >= self.answer_after {
            vec![
                Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
                Ok(done()),
            ]
        } else {
            vec![
                Ok(StreamEvent::ToolCall(ToolCallRequest {
                    id: format!("t{n}"),
                    name: "lookup".into(),
                    arguments: "{}".into(),
                })),
                Ok(done()),
            ]
        };
        Ok(Box::pin(stream::iter(events)))
    }
}

/// The context-window LoopGuard: a long tool-using loop must NOT let the
/// re-sent history grow without bound (the 2.77M-token live failure). Across
/// many turns of sizeable tool results, the largest request handed to the
/// provider must stay within the budget (+ the recent-context the guard keeps)
/// — far below where it would land unguarded (~answer_after × chunk).
#[tokio::test]
async fn the_history_budget_bounds_the_request_across_a_long_tool_loop() {
    const CHUNK: usize = 8 * 1024; // under MAX_TOOL_RESULT_BYTES → not truncated
    const BUDGET: usize = 32 * 1024;
    const TURNS: u32 = 16; // unguarded history would reach ~TURNS × CHUNK ≈ 128 KB
    let factory = Arc::new(HistoryProbeFactory {
        calls: Mutex::new(0),
        max_history_bytes: Mutex::new(0),
        answer_after: TURNS,
    });
    let report = RigSessionRunner::new(factory.clone())
        .with_tool_host(Arc::new(BigResultHost {
            chunk: "x".repeat(CHUNK),
        }))
        .with_max_history_bytes(BUDGET)
        .run(session(vec!["lookup".into()]))
        .await
        .unwrap();

    // Elision must not break the loop — it still terminates with a result.
    match report.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["ok"], true),
        other => panic!("expected Completed despite history elision, got {other:?}"),
    }
    // The loop actually ran long enough to require elision.
    assert!(
        *factory.calls.lock().unwrap() >= TURNS,
        "the test must drive enough turns to exercise the guard"
    );
    // The largest request stayed bounded: budget + at most one extra pair of
    // recent context. Without the guard this would exceed 100 KB.
    let max = *factory.max_history_bytes.lock().unwrap();
    assert!(
        max <= BUDGET + 2 * CHUNK,
        "history must stay within the budget (+recent margin); got {max} bytes"
    );
}

#[tokio::test]
async fn turn_budget_exhaustion_is_no_result() {
    // The model keeps calling the tool, never final_answer → budget runs out.
    let looping = vec![
        Ok(StreamEvent::ToolCall(ToolCallRequest {
            id: "t".into(),
            name: "lookup".into(),
            arguments: "{}".into(),
        })),
        Ok(done()),
    ];
    let turns = std::iter::repeat_n(looping, DEFAULT_MAX_TURNS as usize + 2).collect();
    let factory = ScriptedFactory::new(turns);
    let host = Arc::new(RecordingHost {
        calls: Mutex::new(vec![]),
    });
    let runner = RigSessionRunner::new(Arc::new(factory)).with_tool_host(host);
    let report = runner.run(session(vec!["conn".into()])).await.unwrap();
    assert_eq!(report.outcome, AgentRunOutcome::NoResult);
}

#[tokio::test]
async fn a_malformed_final_answer_fails_fast_not_silently() {
    // AGENTS-02: a `final_answer` whose payload is not a valid envelope must
    // abort the run with AGENT_MALFORMED_RESULT — never get `.ok()`-swallowed
    // into a hollow NoResult that hides the terminating tool call's failure.
    let factory =
        ScriptedFactory::new(vec![vec![Ok(final_answer("this is not json")), Ok(done())]]);
    let err = RigSessionRunner::new(Arc::new(factory))
        .run(session(vec![]))
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("AGENT_MALFORMED_RESULT"),
        "expected AGENT_MALFORMED_RESULT, got {err:?}"
    );
}

#[tokio::test]
async fn a_provider_error_event_propagates_not_buried() {
    // AGENTS-03: a provider-side Error event (rate-limit/503/auth) must
    // surface as a typed AGENT_PROVIDER_ERROR, not be appended to the
    // transcript while the run proceeds to a misleading NoResult.
    let factory = ScriptedFactory::new(vec![vec![
        Ok(StreamEvent::Text {
            chunk: "working...".into(),
        }),
        Ok(StreamEvent::Error {
            message: "429 rate limited".into(),
        }),
    ]]);
    let err = RigSessionRunner::new(Arc::new(factory))
        .run(session(vec![]))
        .await
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("AGENT_PROVIDER_ERROR"),
        "expected AGENT_PROVIDER_ERROR, got {msg}"
    );
    assert!(
        msg.contains("429 rate limited"),
        "cause preserved, got {msg}"
    );
}

/// A host that can't reach the declared connection.
struct UnreachableHost;
#[async_trait]
impl ToolHost for UnreachableHost {
    async fn tools(
        &self,
        _connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        Err(ExecutorError::Connection(
            "MCP_TOOLS_UNREACHABLE: connection 'editor' could not be reached".into(),
        ))
    }
    async fn call(&self, _c: &str, _n: &str, _a: &str) -> Result<String, String> {
        unreachable!("no tool should be called when listing failed")
    }
}

#[tokio::test]
async fn an_unreachable_declared_tool_fails_the_run_rather_than_running_toolless() {
    // Decision: a declared tool whose connection can't be reached must fail
    // the agent step (fail-fast), not silently run the agent without it.
    let factory = ScriptedFactory::new(vec![vec![
        Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
        Ok(done()),
    ]]);
    let runner = RigSessionRunner::new(Arc::new(factory)).with_tool_host(Arc::new(UnreachableHost));
    let err = runner
        .run(session(vec!["editor".into()]))
        .await
        .expect_err("an unreachable declared tool must fail the run");
    assert!(
        format!("{err:?}").contains("MCP_TOOLS_UNREACHABLE"),
        "the connection error must propagate, got {err:?}"
    );
}

/// A factory that records the tool names exposed to the provider each turn,
/// then replays scripted events — so a test can assert what the model saw.
struct CapturingFactory {
    turns: Mutex<std::collections::VecDeque<Vec<Result<StreamEvent, String>>>>,
    seen_tool_names: Mutex<Vec<String>>,
}
#[async_trait]
impl ProviderFactory for CapturingFactory {
    async fn stream(
        &self,
        _model: &str,
        turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        self.seen_tool_names
            .lock()
            .unwrap()
            .extend(turn.tools.iter().map(|t| t.name.clone()));
        let events = self.turns.lock().unwrap().pop_front().unwrap_or_default();
        Ok(Box::pin(stream::iter(events)))
    }
}

/// A host whose tool returns a payload far larger than any model context —
/// stands in for a filesystem/structureos tool dumping a whole repo.
struct HugeResultHost;
#[async_trait]
impl ToolHost for HugeResultHost {
    async fn tools(
        &self,
        _connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        Ok(vec![(
            ToolDefinition {
                name: "lookup".into(),
                description: "look something up".into(),
                parameters: json!({ "type": "object" }),
            },
            "conn".into(),
        )])
    }
    async fn call(&self, _conn: &str, _name: &str, _args: &str) -> Result<String, String> {
        Ok("x".repeat(2_000_000)) // ~2 MB — would overflow the context window
    }
}

#[tokio::test]
async fn an_oversized_tool_result_is_spilled_before_it_reaches_the_model() {
    let factory = ScriptedFactory::new(vec![
        vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: "t1".into(),
                name: "lookup".into(),
                arguments: "{}".into(),
            })),
            Ok(done()),
        ],
        vec![
            Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
            Ok(done()),
        ],
    ]);
    let runner = RigSessionRunner::new(Arc::new(factory)).with_tool_host(Arc::new(HugeResultHost));
    let report = runner.run(session(vec!["conn".into()])).await.unwrap();

    assert!(
        matches!(report.outcome, AgentRunOutcome::Completed(_)),
        "got {:?}",
        report.outcome
    );
    // The spill handle is compact JSON containing "spilled":true — the model
    // sees the handle, not the raw 2 MB payload.
    assert!(
        report.transcript.contains("\"spilled\":true"),
        "an oversized tool result must be spilled, not truncated; transcript:\n{}",
        report.transcript
    );
    assert!(
        report.transcript.len() < 500_000,
        "the 2 MB payload must not be appended in full; transcript was {} bytes",
        report.transcript.len()
    );
}

/// A host exposing a dotted MCP tool name (e.g. the plan server's
/// `plan.submit`) — invalid under the provider tool-name pattern.
struct DottedHost {
    calls: Mutex<Vec<(String, String, String)>>,
}
#[async_trait]
impl ToolHost for DottedHost {
    async fn tools(
        &self,
        _connections: &[String],
    ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
        Ok(vec![(
            ToolDefinition {
                name: "plan.submit".into(),
                description: "submit a plan".into(),
                parameters: json!({ "type": "object" }),
            },
            "planner".into(),
        )])
    }
    async fn call(&self, conn: &str, name: &str, args: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push((conn.into(), name.into(), args.into()));
        Ok(json!({ "ok": true }).to_string())
    }
}

// The provider tool-name rule has ONE source of truth — the runner's
// `tool_budget::is_valid_tool_name`, which `run`'s name guard enforces. These
// sanitize tests pin their output against that same predicate (not a duplicate)
// so the test and the runtime guard can never drift.
use crate::tool_budget::is_valid_tool_name as valid_provider_tool_name;

#[test]
fn sanitize_disambiguates_colliding_names() {
    let mut taken = std::collections::HashMap::new();
    let a = sanitize_tool_name("plan.submit", &taken);
    assert_eq!(a, "plan_submit");
    assert!(valid_provider_tool_name(&a));
    taken.insert(a.clone(), ("c".to_string(), "plan.submit".to_string()));
    // A different real name that sanitizes to the same base must NOT collide
    // (else its routing entry would clobber the first).
    let b = sanitize_tool_name("plan/submit", &taken);
    assert_ne!(b, a, "a colliding sanitized name must be disambiguated");
    assert!(valid_provider_tool_name(&b));
}

#[test]
fn truncate_leaves_small_results_untouched() {
    let s = "a small tool result".to_string();
    assert_eq!(truncate_tool_result(s.clone()), s);
}

#[test]
fn truncate_respects_utf8_boundaries_and_marks_the_cut() {
    // Multi-byte chars whose total length exceeds the cap: truncation must
    // not panic on a mid-char boundary, and must mark the cut.
    let s = "é".repeat(MAX_TOOL_RESULT_BYTES); // 2 bytes each → 2× the cap
    let out = truncate_tool_result(s);
    assert!(out.contains("truncated"), "must mark the truncation");
    assert!(
        out.len() < MAX_TOOL_RESULT_BYTES + 512,
        "kept content stays near the cap"
    );
}

#[test]
fn sanitize_does_not_shadow_the_reserved_final_answer() {
    let taken = std::collections::HashMap::new();
    // A tool literally named `final.answer` sanitizes to `final_answer`,
    // which would shadow the session terminator — must be disambiguated.
    let n = sanitize_tool_name("final.answer", &taken);
    assert_ne!(n, "final_answer");
    assert!(valid_provider_tool_name(&n));
}

#[test]
fn sanitize_does_not_shadow_the_reserved_spill_read() {
    let taken = std::collections::HashMap::new();
    // A host tool literally named `spill_read` would shadow the injected
    // spill-pager (its handler intercepts the name before host routing) — it
    // must be disambiguated so the host tool stays reachable.
    let n = sanitize_tool_name("spill_read", &taken);
    assert_ne!(n, "spill_read");
}

#[tokio::test]
async fn mcp_tool_names_are_sanitized_for_the_provider_and_mapped_back_on_call() {
    // Turn 0: the model calls the SANITIZED name; turn 1: final_answer.
    let factory = Arc::new(CapturingFactory {
        turns: Mutex::new(
            vec![
                vec![
                    Ok(StreamEvent::ToolCall(ToolCallRequest {
                        id: "t1".into(),
                        name: "plan_submit".into(),
                        arguments: "{}".into(),
                    })),
                    Ok(done()),
                ],
                vec![
                    Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
                    Ok(done()),
                ],
            ]
            .into_iter()
            .collect(),
        ),
        seen_tool_names: Mutex::new(Vec::new()),
    });
    let host = Arc::new(DottedHost {
        calls: Mutex::new(Vec::new()),
    });
    let runner = RigSessionRunner::new(factory.clone()).with_tool_host(host.clone());

    let report = runner.run(session(vec!["planner".into()])).await.unwrap();
    assert!(
        matches!(report.outcome, AgentRunOutcome::Completed(_)),
        "got {:?}",
        report.outcome
    );

    // Every name the provider saw must satisfy its tool-name pattern.
    let seen = factory.seen_tool_names.lock().unwrap();
    assert!(
        seen.iter().all(|n| valid_provider_tool_name(n)),
        "exposed tool names must match ^[A-Za-z0-9_-]{{1,128}}$, got {seen:?}"
    );
    // The call must route to the REAL (un-sanitized) name + its connection.
    let calls = host.calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "the dotted tool must actually be invoked");
    assert_eq!(calls[0].0, "planner");
    assert_eq!(
        calls[0].1, "plan.submit",
        "host.call must receive the real tool name"
    );
}

#[tokio::test]
async fn tools_declared_without_a_host_fail_fast() {
    let factory = ScriptedFactory::new(vec![]);
    let err = RigSessionRunner::new(Arc::new(factory))
        .run(session(vec!["github".into()]))
        .await
        .unwrap_err();
    assert!(
        format!("{err:?}").contains("RIG_TOOLS_UNSUPPORTED"),
        "{err:?}"
    );
}

#[tokio::test]
async fn test_tool_setup_timeout() {
    /// A ToolHost that hangs indefinitely when tools() is called.
    struct HangingToolHost;

    #[async_trait::async_trait]
    impl ToolHost for HangingToolHost {
        async fn tools(
            &self,
            _connections: &[String],
        ) -> Result<Vec<(ToolDefinition, String)>, ExecutorError> {
            // Hang forever (3600s, well beyond the 60s timeout)
            sleep(Duration::from_secs(3600)).await;
            unreachable!()
        }
        async fn call(&self, _conn: &str, _name: &str, _args: &str) -> Result<String, String> {
            unreachable!()
        }
    }

    let session = AgentSession {
        model: "test:model".into(),
        system_prompt: None,
        user_prompt: "do the thing".into(),
        tools: vec!["conn".into()],
        reasoning_effort: None,
        timeout: Duration::from_secs(5),
        stall_timeout: Duration::from_secs(5),
        expected_output_keys: Vec::new(),
        expected_output_types: Default::default(),
        await_enabled: false,
        identity: Default::default(),
    };
    let runner = RigSessionRunner::new(Arc::new(ScriptedFactory::new(vec![])))
        .with_tool_host(Arc::new(HangingToolHost));
    let result = runner.run(session).await;
    assert!(matches!(result, Err(ExecutorError::Timeout(60))));
}

/// A factory whose stream hangs forever without ever yielding an event —
/// stands in for a "thinking" model that stalls at first token. Counts how
/// many times `stream` is invoked so the test can prove a stall escalates
/// (one attempt) rather than wastefully re-running the same hung model.
struct StallingFactory {
    calls: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl ProviderFactory for StallingFactory {
    async fn stream(
        &self,
        _model: &str,
        _turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Box::pin(stream::pending()))
    }
}

/// The no-progress watchdog: a model that produces NO stream event must be
/// caught by the per-turn `stall_timeout` (30s) — NOT left to burn the whole
/// 600s total budget — and the stall must escalate (surface as a Timeout the
/// chain-walk treats as infrastructure), running the hung model exactly once
/// rather than re-issuing it through the same-model transient-retry path.
#[tokio::test(start_paused = true)]
async fn a_stalled_stream_is_caught_by_the_no_progress_watchdog_and_does_not_retry() {
    let factory = Arc::new(StallingFactory {
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let mut s = session(vec![]); // no tools → tool setup skipped
    s.timeout = Duration::from_secs(600); // total budget — must NOT be what fires
    s.stall_timeout = Duration::from_secs(30); // the watchdog window
    let runner = RigSessionRunner::new(factory.clone());
    let result = runner.run(s).await;
    // Fires at the 30s stall window, not the 600s wall, and surfaces as a
    // Timeout (→ NetworkTimeout → chain-walk escalates to the next model).
    assert!(
        matches!(result, Err(ExecutorError::Timeout(30))),
        "a stalled stream must time out at the 30s stall window, got {result:?}"
    );
    // Escalate, don't re-hang: the same-model retry path must NOT re-issue a
    // stalled model (that would burn another stall window per attempt).
    assert_eq!(
        factory.calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a stalled model must escalate after one attempt, not retry in-session"
    );
}

#[test]
fn conforms_enforces_declared_output_types() {
    let keys = vec!["spec".to_string()];
    let mut types = BTreeMap::new();
    types.insert("spec".to_string(), "string".to_string());
    // Right key, right type → conforms.
    assert!(super::conforms(
        &json!({ "spec": "a string" }),
        &keys,
        &types
    ));
    // Right key, WRONG type (object where string declared) → does NOT
    // conform (this is the run-1/run-3 spec-type class; the runner
    // re-prompts instead of wasting the run on a post-hoc contract failure).
    assert!(!super::conforms(
        &json!({ "spec": { "k": "v" } }),
        &keys,
        &types
    ));
    // Missing key → does not conform.
    assert!(!super::conforms(&json!({ "other": "x" }), &keys, &types));
    // No declared type for a key → type not enforced (keys-only).
    let no_types = BTreeMap::new();
    assert!(super::conforms(
        &json!({ "spec": { "k": "v" } }),
        &keys,
        &no_types
    ));
}

#[test]
fn is_transient_retries_only_transient_classes() {
    // Retried (blip worth a same-model retry):
    assert!(super::is_transient(&ExecutorError::Timeout(5)));
    assert!(super::is_transient(&ExecutorError::Connection("x".into())));
    assert!(super::is_transient(&ExecutorError::Transient("x".into())));
    assert!(super::is_transient(&ExecutorError::RateLimited("x".into())));
    // NOT retried (chain-walk / surface instead):
    assert!(!super::is_transient(&ExecutorError::Permanent("x".into())));
    assert!(!super::is_transient(&ExecutorError::Auth("x".into())));
}

// ═══ P12 R1.4 — durable suspend / resume (docs/await-resume-architecture.md) ═══

use praxec_core::store::SqliteParkedSessionStore;

fn await_call(id: &str, prompt: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCallRequest {
        id: id.into(),
        name: AWAIT_HUMAN_TOOL.into(),
        arguments: json!({ "prompt": prompt }).to_string(),
    })
}

fn await_session(tools: Vec<String>) -> AgentSession {
    AgentSession {
        await_enabled: true,
        ..session(tools)
    }
}

fn parked_store() -> Arc<SqliteParkedSessionStore> {
    Arc::new(SqliteParkedSessionStore::open_in_memory().unwrap())
}

/// (a) The suspend signal: an `await_human` call STOPS the loop with a
/// first-class Suspended outcome (not an error, not NoResult) AND the parked
/// conversation is persisted in the store, keyed by the correlation_id.
#[tokio::test]
async fn an_await_human_call_suspends_and_durably_parks_the_conversation() {
    let store = parked_store();
    let factory = ScriptedFactory::new(vec![vec![Ok(await_call("a1", "ship it?")), Ok(done())]]);
    let runner = RigSessionRunner::new(Arc::new(factory)).with_parked_store(store.clone());
    let report = runner.run(await_session(vec![])).await.unwrap();

    let AgentRunOutcome::Suspended(s) = report.outcome else {
        panic!("expected Suspended, got {:?}", report.outcome);
    };
    assert_eq!(s.prompt, "ship it?");
    assert!(!s.correlation_id.is_empty());
    let rec = store
        .load(&s.correlation_id)
        .await
        .unwrap()
        .expect("a parked row must exist for the reported correlation_id");
    assert_eq!(rec.prompt, "ship it?");
    let conv: ParkedConversation = serde_json::from_value(rec.conversation).unwrap();
    // Exactly one awaited slot (the hole the reply fills), and the history
    // carries the goal + the awaited turn.
    assert_eq!(conv.pending.iter().filter(|p| p.text.is_none()).count(), 1);
    let history_json = serde_json::to_string(&conv.history).unwrap();
    assert!(history_json.contains("do the thing"), "goal in history");
    assert!(
        history_json.contains(AWAIT_HUMAN_TOOL),
        "awaited turn in history"
    );
    assert_eq!(conv.turns_used, 1);
}

/// (b) Persist-then-reload round-trips: the reloaded conversation and session
/// re-serialize to exactly the persisted JSON (the durable fixpoint).
#[tokio::test]
async fn a_parked_session_reloads_faithfully_from_the_store() {
    let store = parked_store();
    let factory = ScriptedFactory::new(vec![vec![Ok(await_call("a1", "approve?")), Ok(done())]]);
    let runner = RigSessionRunner::new(Arc::new(factory)).with_parked_store(store.clone());
    let report = runner.run(await_session(vec![])).await.unwrap();
    let AgentRunOutcome::Suspended(s) = report.outcome else {
        panic!("expected Suspended, got {:?}", report.outcome);
    };

    let rec = store.load(&s.correlation_id).await.unwrap().unwrap();
    let conv: ParkedConversation = serde_json::from_value(rec.conversation.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&conv).unwrap(),
        rec.conversation,
        "reloaded conversation must re-serialize to exactly what was persisted"
    );
    let sess: AgentSession = serde_json::from_value(rec.session.clone()).unwrap();
    assert_eq!(sess.user_prompt, "do the thing");
    assert_eq!(sess.model, "anthropic:claude-sonnet-4-6");
    assert!(sess.await_enabled);
    assert_eq!(
        serde_json::to_value(&sess).unwrap(),
        rec.session,
        "reloaded session must re-serialize to exactly what was persisted"
    );
}

/// (c) Resume: the human reply is injected as the awaited call's tool result,
/// the loop continues from the parked turn to final_answer, the parked turn's
/// OTHER tool is never re-executed, and the consumed frame is removed.
#[tokio::test]
async fn resume_injects_the_reply_and_continues_to_final_answer() {
    let store = parked_store();
    let factory = Arc::new(PromptCapturingFactory::new(vec![
        // Turn 0: a real tool call AND the await, same turn.
        vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: "t1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":"x"}"#.into(),
            })),
            Ok(await_call("a1", "approve?")),
            Ok(done()),
        ],
        // The resume turn: the model answers.
        vec![
            Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
            Ok(done()),
        ],
    ]));
    let host = Arc::new(RecordingHost {
        calls: Mutex::new(vec![]),
    });
    let runner = RigSessionRunner::new(factory.clone())
        .with_tool_host(host.clone())
        .with_parked_store(store.clone());

    let report = runner
        .run(await_session(vec!["conn".into()]))
        .await
        .unwrap();
    let AgentRunOutcome::Suspended(susp) = report.outcome else {
        panic!("expected Suspended, got {:?}", report.outcome);
    };
    assert_eq!(
        host.calls.lock().unwrap().len(),
        1,
        "the parked turn's non-await tool executes ONCE, at park time"
    );

    let resumed = runner
        .resume(&susp.correlation_id, "yes, approved")
        .await
        .unwrap();
    match resumed.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["ok"], true),
        other => panic!("expected Completed after resume, got {other:?}"),
    }
    // The resume turn's input carried the human reply as the awaited tool's
    // result AND the pre-park lookup's realized result (not re-run).
    {
        let prompts = factory.prompts.lock().unwrap();
        let resume_prompt = prompts.last().unwrap();
        assert!(
            resume_prompt.contains("yes, approved"),
            "the reply must arrive as the awaited tool result, got: {resume_prompt}"
        );
        assert!(
            resume_prompt.contains("found"),
            "the parked turn's realized tool result must ride along, got: {resume_prompt}"
        );
    } // prompts guard dropped here — before the .await below (clippy::await_holding_lock)
    assert_eq!(
        host.calls.lock().unwrap().len(),
        1,
        "resume must NOT re-execute the parked turn's tools"
    );
    // The consumed frame is gone.
    assert!(
        store.load(&susp.correlation_id).await.unwrap().is_none(),
        "a completed resume must remove its parked frame"
    );
}

/// (c′) Durability semantics: a DIFFERENT runner instance (fresh factory, no
/// shared in-process state — the in-process stand-in for a restart) resumes
/// from the store alone.
#[tokio::test]
async fn a_fresh_runner_resumes_from_the_store_alone() {
    let store = parked_store();
    let factory_a = ScriptedFactory::new(vec![vec![Ok(await_call("a1", "go?")), Ok(done())]]);
    let runner_a = RigSessionRunner::new(Arc::new(factory_a)).with_parked_store(store.clone());
    let report = runner_a.run(await_session(vec![])).await.unwrap();
    let AgentRunOutcome::Suspended(susp) = report.outcome else {
        panic!("expected Suspended, got {:?}", report.outcome);
    };
    drop(runner_a); // nothing of the suspending runner survives

    let factory_b = ScriptedFactory::new(vec![vec![
        Ok(final_answer(
            r#"{"status":"success","output":{"done":true}}"#,
        )),
        Ok(done()),
    ]]);
    let runner_b = RigSessionRunner::new(Arc::new(factory_b)).with_parked_store(store.clone());
    let resumed = runner_b.resume(&susp.correlation_id, "go").await.unwrap();
    match resumed.outcome {
        AgentRunOutcome::Completed(r) => assert_eq!(r.output["done"], true),
        other => panic!("expected Completed via a fresh runner, got {other:?}"),
    }
}

/// (d) A session that does NOT opt in never sees the suspend tool — the tool
/// set the provider receives is exactly the pre-P12 one, so a normal run
/// cannot suspend by accident.
#[tokio::test]
async fn await_human_is_not_offered_without_opt_in() {
    let factory = Arc::new(CapturingFactory {
        turns: Mutex::new(
            vec![vec![
                Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
                Ok(done()),
            ]]
            .into_iter()
            .collect(),
        ),
        seen_tool_names: Mutex::new(Vec::new()),
    });
    // Even with a parked store wired, no opt-in ⇒ no await tool.
    let runner = RigSessionRunner::new(factory.clone()).with_parked_store(parked_store());
    let report = runner.run(session(vec![])).await.unwrap();
    assert!(matches!(report.outcome, AgentRunOutcome::Completed(_)));
    let seen = factory.seen_tool_names.lock().unwrap();
    assert!(
        !seen.iter().any(|n| n == AWAIT_HUMAN_TOOL),
        "await_human must not be offered without await_enabled; saw {seen:?}"
    );
}

/// (d′) An await-enabled run that never calls the tool is unaffected: it
/// reaches final_answer normally and parks nothing.
#[tokio::test]
async fn an_await_enabled_run_that_never_awaits_completes_and_parks_nothing() {
    let store = parked_store();
    let factory = ScriptedFactory::new(vec![vec![
        Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
        Ok(done()),
    ]]);
    let runner = RigSessionRunner::new(Arc::new(factory)).with_parked_store(store.clone());
    let report = runner.run(await_session(vec![])).await.unwrap();
    assert!(
        matches!(report.outcome, AgentRunOutcome::Completed(_)),
        "got {:?}",
        report.outcome
    );
    assert!(
        store.list().await.unwrap().is_empty(),
        "no suspend signal ⇒ nothing parked"
    );
}

/// (e) Resume with an unknown correlation_id is a TYPED error — no panic.
#[tokio::test]
async fn resume_with_an_unknown_correlation_id_is_a_typed_error() {
    let runner = RigSessionRunner::new(Arc::new(ScriptedFactory::new(vec![])))
        .with_parked_store(parked_store());
    let err = runner.resume("no-such-id", "hello").await.unwrap_err();
    assert!(
        format!("{err:?}").contains("AGENT_UNKNOWN_CORRELATION"),
        "got {err:?}"
    );
}

/// A parked row whose payload can't be reconstituted is a TYPED corruption
/// error — no panic, no silent resume of garbage.
#[tokio::test]
async fn resume_of_a_corrupt_parked_payload_is_a_typed_error() {
    let store = parked_store();
    store
        .park(praxec_core::model::ParkedAgentSession {
            correlation_id: "corr-x".into(),
            prompt: "?".into(),
            session: json!("this is not an AgentSession"),
            conversation: json!({ "also": "not a conversation" }),
            parked_at: chrono::Utc::now(),
        })
        .await
        .unwrap();
    let runner =
        RigSessionRunner::new(Arc::new(ScriptedFactory::new(vec![]))).with_parked_store(store);
    let err = runner.resume("corr-x", "hi").await.unwrap_err();
    assert!(
        format!("{err:?}").contains("AGENT_PARKED_SESSION_CORRUPT"),
        "got {err:?}"
    );
}

/// Poka-yoke: an await-capable session with no durable park fails FAST at run
/// start — a suspend whose conversation couldn't persist must be impossible.
#[tokio::test]
async fn an_await_enabled_session_without_a_parked_store_fails_fast() {
    let runner = RigSessionRunner::new(Arc::new(ScriptedFactory::new(vec![])));
    let err = runner.run(await_session(vec![])).await.unwrap_err();
    assert!(
        format!("{err:?}").contains("AGENT_AWAIT_UNSUPPORTED"),
        "got {err:?}"
    );
}

/// resume() without a parked store is the same typed fail-fast.
#[tokio::test]
async fn resume_without_a_parked_store_is_a_typed_error() {
    let runner = RigSessionRunner::new(Arc::new(ScriptedFactory::new(vec![])));
    let err = runner.resume("any", "hi").await.unwrap_err();
    assert!(
        format!("{err:?}").contains("AGENT_AWAIT_UNSUPPORTED"),
        "got {err:?}"
    );
}

/// A host tool literally named `await_human` must not shadow the reserved
/// suspend signal (its handler intercepts the name before host routing).
#[test]
fn sanitize_does_not_shadow_the_reserved_await_human() {
    let taken = std::collections::HashMap::new();
    let n = sanitize_tool_name(AWAIT_HUMAN_TOOL, &taken);
    assert_ne!(n, AWAIT_HUMAN_TOOL);
    assert!(valid_provider_tool_name(&n));
}

// ═══ Observability — the `agent.heartbeat` liveness pulse ═══════════════════

use praxec_core::audit::MemoryAuditSink;

/// A session stamped with the identity the runtime puts on `agent.invoked`,
/// so the heartbeat's correlation join can be asserted.
fn identified_session(tools: Vec<String>) -> AgentSession {
    AgentSession {
        identity: crate::session::RunIdentity {
            workflow_id: Some("wf-hb".into()),
            correlation_id: Some("cor-hb".into()),
            transition: Some("do_work".into()),
        },
        ..session(tools)
    }
}

fn heartbeats(sink: &MemoryAuditSink) -> Vec<praxec_core::audit::AuditEvent> {
    sink.snapshot()
        .into_iter()
        .filter(|e| e.event_type == AGENT_HEARTBEAT_EVENT)
        .collect()
}

/// Per-turn liveness: a multi-turn run emits one boundary `agent.heartbeat`
/// per tool-loop turn, carrying the turn index, model, elapsed time, and the
/// SAME workflow/correlation identity `agent.invoked` carries — so an operator
/// tailing the audit log sees turn-by-turn progress between the boundary
/// events instead of silence.
#[tokio::test]
async fn a_multi_turn_run_emits_a_boundary_heartbeat_per_turn() {
    // Turn 0: the model calls `lookup`. Turn 1: it calls `final_answer`.
    let factory = ScriptedFactory::new(vec![
        vec![
            Ok(StreamEvent::ToolCall(ToolCallRequest {
                id: "t1".into(),
                name: "lookup".into(),
                arguments: r#"{"q":"x"}"#.into(),
            })),
            Ok(done()),
        ],
        vec![
            Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
            Ok(done()),
        ],
    ]);
    let host = Arc::new(RecordingHost {
        calls: Mutex::new(vec![]),
    });
    let sink = MemoryAuditSink::new();
    let runner = RigSessionRunner::new(Arc::new(factory))
        .with_tool_host(host)
        .with_audit_sink(Arc::new(sink.clone()));
    let report = runner
        .run(identified_session(vec!["conn".into()]))
        .await
        .unwrap();
    assert!(matches!(report.outcome, AgentRunOutcome::Completed(_)));

    let beats = heartbeats(&sink);
    assert_eq!(
        beats.len(),
        2,
        "one boundary heartbeat per turn (2 turns), got {beats:?}"
    );
    for (i, beat) in beats.iter().enumerate() {
        assert_eq!(beat.payload["turn"], json!(i as u64), "turn index");
        assert_eq!(beat.payload["phase"], json!("turn"));
        assert_eq!(beat.payload["model"], json!("anthropic:claude-sonnet-4-6"));
        assert_eq!(beat.payload["transition"], json!("do_work"));
        assert!(
            beat.payload["elapsed_ms"].is_u64(),
            "elapsed_ms present: {beat:?}"
        );
        // The correlation join with agent.invoked / agent.completed.
        assert_eq!(beat.workflow_id.as_deref(), Some("wf-hb"));
        assert_eq!(beat.correlation_id, "cor-hb");
    }
}

/// Non-spammy: a fast single-turn run emits exactly ONE boundary heartbeat
/// and no within-turn `waiting_on_model` pulse.
#[tokio::test]
async fn a_fast_run_emits_only_the_boundary_heartbeat() {
    let factory = ScriptedFactory::new(vec![vec![
        Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#)),
        Ok(done()),
    ]]);
    let sink = MemoryAuditSink::new();
    let runner = RigSessionRunner::new(Arc::new(factory)).with_audit_sink(Arc::new(sink.clone()));
    let report = runner.run(identified_session(vec![])).await.unwrap();
    assert!(matches!(report.outcome, AgentRunOutcome::Completed(_)));

    let beats = heartbeats(&sink);
    assert_eq!(beats.len(), 1, "exactly one boundary heartbeat: {beats:?}");
    assert_eq!(beats[0].payload["phase"], json!("turn"));
    assert_eq!(beats[0].payload["turn"], json!(0));
}

/// A factory whose single turn goes silent for 70s (paused clock) before the
/// model answers — the "slow reasoning call" that is 0-CPU from outside and
/// used to be indistinguishable from a hang.
struct SlowFirstTokenFactory;
#[async_trait]
impl ProviderFactory for SlowFirstTokenFactory {
    async fn stream(
        &self,
        _model: &str,
        _turn: TurnRequest,
    ) -> Result<BoxStream<'static, Result<StreamEvent, String>>, ExecutorError> {
        let delayed = stream::once(async {
            sleep(Duration::from_secs(70)).await;
            Ok(final_answer(r#"{"status":"success","output":{"ok":true}}"#))
        });
        Ok(Box::pin(delayed.chain(stream::iter(vec![Ok(done())]))))
    }
}

/// The within-turn pulse (the actual 0-CPU case): while ONE model call is
/// silent, the stall watchdog's tick emits a `waiting_on_model` heartbeat
/// every HEARTBEAT_INTERVAL — BEFORE the stall window fires — so a slow
/// reasoning turn shows a pulse ("still alive, Ns since last output")
/// instead of dead air. 70s of silence with a 30s interval and a 120s stall
/// window ⇒ pulses at 30s and 60s, then a normal completion.
#[tokio::test(start_paused = true)]
async fn a_long_silent_model_call_pulses_within_turn_heartbeats() {
    let sink = MemoryAuditSink::new();
    let runner = RigSessionRunner::new(Arc::new(SlowFirstTokenFactory))
        .with_audit_sink(Arc::new(sink.clone()));
    let mut s = identified_session(vec![]);
    s.timeout = Duration::from_secs(600);
    s.stall_timeout = Duration::from_secs(120); // watchdog must NOT fire at 70s
    let report = runner.run(s).await.unwrap();
    assert!(
        matches!(report.outcome, AgentRunOutcome::Completed(_)),
        "the slow-but-alive turn completes normally: {:?}",
        report.outcome
    );

    let beats = heartbeats(&sink);
    let waiting: Vec<_> = beats
        .iter()
        .filter(|b| b.payload["phase"] == json!("waiting_on_model"))
        .collect();
    assert_eq!(
        waiting.len(),
        2,
        "70s of silence at a 30s interval pulses at 30s and 60s: {beats:?}"
    );
    assert_eq!(waiting[0].payload["seconds_since_last_output"], json!(30));
    assert_eq!(waiting[1].payload["seconds_since_last_output"], json!(60));
    // The identity join rides the within-turn pulse too.
    assert_eq!(waiting[0].correlation_id, "cor-hb");
    // And the boundary heartbeat for the (single) turn is still there.
    assert!(
        beats
            .iter()
            .any(|b| b.payload["phase"] == json!("turn") && b.payload["turn"] == json!(0)),
        "boundary heartbeat present: {beats:?}"
    );
}

/// Emit-only: with NO audit sink wired (the default), a run emits nothing and
/// behaves exactly as before — the heartbeat is a pure observability addition.
#[tokio::test(start_paused = true)]
async fn without_a_sink_the_slow_call_still_completes_with_no_emission() {
    let runner = RigSessionRunner::new(Arc::new(SlowFirstTokenFactory));
    let mut s = session(vec![]);
    s.timeout = Duration::from_secs(600);
    s.stall_timeout = Duration::from_secs(120);
    let report = runner.run(s).await.unwrap();
    assert!(matches!(report.outcome, AgentRunOutcome::Completed(_)));
}
