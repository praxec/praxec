//! g6 — first-ever integration coverage of the HITL elicitation push/resume
//! path: a REAL [`PraxecServer`] served over an in-process
//! `tokio::io::duplex` transport, driven by a REAL rmcp client whose
//! `ClientHandler::create_elicitation` is scripted per scenario.
//!
//! The workflow under test carries the full E1–E3 contract of the HITL
//! elicitation plan: context candidates seeded via `input` (the engine's
//! input→context seeding), `presents` over them (g1 projection), `choices`
//! yielding `chosen_id` (g1/g5 titled single-select), an `inputSchema`
//! requiring `chosen_id`, and the `pick` output operator (g3) mapping the
//! FULL selected object into `context.chosen`, all fenced by the
//! submit-time CHOICE_MISMATCH guard (g4).
//!
//! Every acceptance assertion is on ENGINE side-effects — store reads of
//! state/version/context — never on the scripted client's own echo. The one
//! thing asserted about the push itself is what the SERVER sent (message +
//! form schema), which is engine output.

use std::sync::{Arc, Mutex, OnceLock};

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::ports::{ExecutorRegistry, WorkflowStore};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_mcp_server::PraxecServer;
use rmcp::ErrorData as McpError;
use rmcp::handler::client::ClientHandler;
use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, CreateElicitationRequestParams,
    CreateElicitationResult, ElicitationAction, ElicitationCapability, FormElicitationCapability,
    Implementation, JsonObject,
};
use rmcp::service::{Peer, RequestContext, RunningService};
use rmcp::transport::async_rw::AsyncRwTransport;
use rmcp::{RoleClient, ServiceExt, serve_server};
use serde_json::{Value, json};

// ── Test server ───────────────────────────────────────────────────────────────

struct NoopRegistry;
impl ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

/// Registry for the parent/child scenario: serves ONLY `kind: workflow` (the
/// real [`WorkflowExecutor`], late-installed to break the registry↔runtime
/// construction cycle — same pattern as praxec-executors' own tests).
struct SubflowRegistry {
    workflow: OnceLock<Arc<praxec_executors::workflow::WorkflowExecutor>>,
}
impl SubflowRegistry {
    fn new() -> Self {
        Self {
            workflow: OnceLock::new(),
        }
    }
    fn install(&self, e: Arc<praxec_executors::workflow::WorkflowExecutor>) {
        self.workflow.set(e).map_err(|_| ()).expect("install once");
    }
}
impl ExecutorRegistry for SubflowRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        match kind {
            "workflow" => self
                .workflow
                .get()
                .map(|w| w.clone() as Arc<dyn praxec_core::Executor>),
            _ => None,
        }
    }
}

/// The E1–E3 declared contract on a minimal human `pick` gate. The prompt is
/// resolved through the chain's context link (the `prompt` string seeded via
/// `input`); candidates are likewise input-seeded so `presents`/`choices`
/// project them from live context at gate time.
fn gate_config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "pick_shape": {
                "version": "1.0.0",
                "initialState": "picking",
                "states": {
                    "picking": {
                        "transitions": {
                            "pick": {
                                "target": "done",
                                "actor": "human",
                                "presents": ["$.context.candidates"],
                                "choices": {
                                    "field": "chosen_id",
                                    "from": "$.context.candidates",
                                    "value": "id",
                                    "title": "name"
                                },
                                "inputSchema": {
                                    "type": "object",
                                    "required": ["chosen_id"],
                                    "properties": {
                                        "chosen_id": { "type": "string" },
                                        "rationale": { "type": "string" }
                                    }
                                },
                                "output": { "chosen": { "pick": {
                                    "from": "$.context.candidates",
                                    "by": "id",
                                    "eq": "$.arguments.chosen_id"
                                }}}
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

/// The start args every scenario fires: candidates + the caller-seeded
/// `prompt` (the E1 chain's context link).
fn start_args() -> Value {
    json!({
        "definitionId": "pick_shape",
        "input": {
            "prompt": "Pick a shape for the capability.",
            "candidates": [
                { "id": "monolith", "name": "Monolith", "tradeoffs": "simple but coupled" },
                { "id": "split", "name": "Split", "tradeoffs": "clean seams, more files" }
            ]
        }
    })
}

/// A real `PraxecServer` over the gate config, plus the SHARED store handle
/// the scenarios read engine side-effects from.
fn server_with_store() -> (PraxecServer, Arc<InMemoryWorkflowStore>) {
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&gate_config())),
        store.clone(),
        Arc::new(NoopRegistry),
        Arc::new(DefaultGuardEvaluator::new()),
        audit as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    (PraxecServer::new(runtime), store)
}

/// Finding #11 — parent/child split of the same E1–E3 gate. The parent's
/// deterministic `spawn` transition runs a `kind: workflow` child whose
/// `use.outputs` maps `candidates` up (the `flow.author-capability`
/// `composing`-state shape); the parent then advances into `gating`, the
/// SAME full-contract human gate as [`gate_config`]. The child is a bare
/// agent-driven cap: `ready → submit → done`, its `submit` landing the
/// candidates in its terminal context for the projection.
///
/// Resolved through `praxec_core::config::resolve` because the `use:` block
/// is a config-resolve-time expansion (synthesized transition `output:` +
/// embedded `_snippetOutputs`) — a raw definition map would silently skip
/// the output projection.
fn parent_child_config() -> Value {
    praxec_core::config::resolve(json!({
        "version": "1.0.0",
        "workflows": {
            "parent_flow": {
                "version": "1.0.0",
                "initialState": "spawning",
                "states": {
                    "spawning": {
                        "transitions": {
                            "spawn": {
                                "target": "gating",
                                "actor": "deterministic",
                                "executor": {
                                    "kind": "workflow",
                                    "definitionId": "child_cap",
                                    "use": {
                                        "outputs": { "$.context.candidates": "candidates" }
                                    }
                                }
                            }
                        }
                    },
                    "gating": {
                        "transitions": {
                            "pick": {
                                "target": "done",
                                "actor": "human",
                                "presents": ["$.context.candidates"],
                                "choices": {
                                    "field": "chosen_id",
                                    "from": "$.context.candidates",
                                    "value": "id",
                                    "title": "name"
                                },
                                "inputSchema": {
                                    "type": "object",
                                    "required": ["chosen_id"],
                                    "properties": {
                                        "chosen_id": { "type": "string" },
                                        "rationale": { "type": "string" }
                                    }
                                },
                                "output": { "chosen": { "pick": {
                                    "from": "$.context.candidates",
                                    "by": "id",
                                    "eq": "$.arguments.chosen_id"
                                }}}
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            },
            "child_cap": {
                "version": "1.0.0",
                "initialState": "ready",
                "states": {
                    "ready": {
                        "transitions": {
                            "submit": {
                                "target": "done",
                                "actor": "agent",
                                "output": { "candidates": "$.arguments.candidates" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    }))
    .expect("parent/child config resolves")
}

/// A real `PraxecServer` over the parent/child config with the REAL
/// `WorkflowExecutor` wired for `kind: workflow`, plus the shared store
/// handle the scenario reads engine side-effects from.
fn server_with_parent_child() -> (PraxecServer, Arc<InMemoryWorkflowStore>) {
    let store = Arc::new(InMemoryWorkflowStore::new());
    let audit: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());
    let registry = Arc::new(SubflowRegistry::new());
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&parent_child_config())),
        store.clone(),
        registry.clone() as Arc<dyn ExecutorRegistry>,
        Arc::new(DefaultGuardEvaluator::new()),
        audit.clone(),
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    registry.install(Arc::new(praxec_executors::workflow::WorkflowExecutor::new(
        runtime.clone(),
        audit,
    )));
    (PraxecServer::new(runtime), store)
}

// ── Scripted elicitation client ───────────────────────────────────────────────

/// An elicitation-capable rmcp client whose `create_elicitation` answer is
/// scripted per scenario. Records every push the server sends (message +
/// requested schema as JSON) so a test can assert on the SERVER's output.
struct ScriptedClient {
    action: ElicitationAction,
    content: Option<Value>,
    pushed: Arc<Mutex<Vec<(String, Value)>>>,
}

impl ClientHandler for ScriptedClient {
    // These rmcp capability/info structs are `#[non_exhaustive]`; Default +
    // field assignment is the only construction path (same shape as
    // praxec-executors' RelayClientHandler).
    #[allow(clippy::field_reassign_with_default)]
    fn get_info(&self) -> ClientInfo {
        let mut elicitation = ElicitationCapability::default();
        elicitation.form = Some(FormElicitationCapability {
            schema_validation: Some(false),
        });
        let mut capabilities = ClientCapabilities::default();
        capabilities.elicitation = Some(elicitation);
        let mut client_info = Implementation::default();
        client_info.name = "scripted-elicitation-client".into();
        client_info.version = "0.0.0".into();
        let mut info = ClientInfo::default();
        info.capabilities = capabilities;
        info.client_info = client_info;
        info
    }

    async fn create_elicitation(
        &self,
        params: CreateElicitationRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, McpError> {
        if let CreateElicitationRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = &params
        {
            self.pushed.lock().expect("pushed lock").push((
                message.clone(),
                serde_json::to_value(requested_schema).expect("schema serializes"),
            ));
        }
        Ok(CreateElicitationResult {
            action: self.action.clone(),
            content: self.content.clone(),
            meta: None,
        })
    }
}

// ── Duplex session harness ────────────────────────────────────────────────────

/// Serve `server` over one end of an in-process duplex and connect `handler`
/// as the rmcp client on the other. Returns the running client (the caller
/// drives it via `.peer()`) and the spawned server task.
async fn connect<H: ClientHandler + Send + 'static>(
    server: PraxecServer,
    handler: H,
) -> (RunningService<RoleClient, H>, tokio::task::JoinHandle<()>) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (cr, cw) = tokio::io::split(client_io);
    let (sr, sw) = tokio::io::split(server_io);
    let server_task = tokio::spawn(async move {
        let running = serve_server(server, AsyncRwTransport::new_server(sr, sw))
            .await
            .expect("server initialize");
        let _ = running.waiting().await;
    });
    let client = ServiceExt::serve(handler, AsyncRwTransport::new_client(cr, cw))
        .await
        .expect("client initialize");
    (client, server_task)
}

/// Fire one tool call through the wire and hand back its structured content.
async fn call(peer: &Peer<RoleClient>, tool: &'static str, args: Value) -> Value {
    let m: JsonObject = args.as_object().cloned().expect("args are an object");
    let result = peer
        .call_tool(CallToolRequestParams::new(tool).with_arguments(m))
        .await
        .expect("tool call over duplex");
    result
        .structured_content
        .expect("praxec responses are structured")
}

/// Normalize a command response for byte-identical comparison across two
/// DIFFERENT instances of the same workflow: the instance's own id (which
/// also appears in `run_ref`-derived fields and links) becomes `<WF>`, and
/// every RFC3339 timestamp string becomes `<TS>`. Everything else — every
/// key, every value, every ordering — must match byte-for-byte.
fn normalized(resp: &Value) -> String {
    fn blank_timestamps(v: &mut Value) {
        match v {
            Value::String(s) => {
                if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
                    *s = "<TS>".to_string();
                }
            }
            Value::Array(items) => items.iter_mut().for_each(blank_timestamps),
            Value::Object(map) => map.values_mut().for_each(blank_timestamps),
            _ => {}
        }
    }
    let id = resp["workflow"]["id"]
        .as_str()
        .expect("command response carries workflow.id")
        .to_string();
    let mut v = resp.clone();
    blank_timestamps(&mut v);
    serde_json::to_string(&v)
        .expect("response serializes")
        .replace(&id, "<WF>")
}

// ── Scenarios ─────────────────────────────────────────────────────────────────

/// (a) Accept with a valid choice: the elicitation answer resumes the mission
/// IN-BAND — the store shows the advanced state/version, and `context.chosen`
/// is the FULL candidate object. Proves g1 render → g5 form → resume submit →
/// g4 guard pass → g3 pick output, end to end.
#[tokio::test]
async fn accept_with_valid_choice_advances_and_maps_the_full_object() {
    let (server, store) = server_with_store();
    let pushed = Arc::new(Mutex::new(Vec::new()));
    let handler = ScriptedClient {
        action: ElicitationAction::Accept,
        content: Some(json!({ "chosen_id": "split", "rationale": "seams first" })),
        pushed: pushed.clone(),
    };
    let (client, server_task) = connect(server, handler).await;

    let resp = call(client.peer(), "praxec.command", start_args()).await;

    // Engine side-effects (store reads), never the mock's echo.
    let id = resp["workflow"]["id"].as_str().expect("workflow.id");
    let instance = store.load(id).await.expect("instance persisted");
    assert_eq!(
        instance.state, "done",
        "the accepted elicitation must resume and advance the mission"
    );
    assert!(
        instance.version > 0,
        "the resume submit must bump the persisted version, got {}",
        instance.version
    );
    assert_eq!(
        instance.context["chosen"],
        json!({ "id": "split", "name": "Split", "tradeoffs": "clean seams, more files" }),
        "the g3 pick output must land the FULL selected object, not the bare id"
    );

    // The push the server sent carried the rendered decision context (g1→g5):
    // prompt first, presented candidates, and the titled single-select form.
    // Scoped so the std MutexGuard drops before the awaits below
    // (clippy::await_holding_lock is deny-level in CI).
    {
        let pushed = pushed.lock().expect("pushed lock");
        assert_eq!(pushed.len(), 1, "exactly one elicitation push");
        let (message, schema) = &pushed[0];
        assert!(
            message.starts_with("Pick a shape for the capability."),
            "the caller-seeded prompt must lead the message, got: {message}"
        );
        assert!(
            message.contains("— $.context.candidates —") && message.contains("Monolith"),
            "the presented candidates must be rendered into the message, got: {message}"
        );
        assert_eq!(
            schema["properties"]["chosen_id"]["oneOf"],
            json!([
                { "const": "monolith", "title": "Monolith" },
                { "const": "split", "title": "Split" }
            ]),
            "the choice field must be the titled single-select over the live candidates"
        );
    }

    let _ = client.cancel().await;
    server_task.abort();
}

/// (b) Decline: the mission stays parked — state and version untouched in the
/// store, and a fresh `get` still surfaces the `pending_human` gate.
#[tokio::test]
async fn decline_leaves_the_mission_parked() {
    let (server, store) = server_with_store();
    let handler = ScriptedClient {
        action: ElicitationAction::Decline,
        content: None,
        pushed: Arc::new(Mutex::new(Vec::new())),
    };
    let (client, server_task) = connect(server, handler).await;

    let resp = call(client.peer(), "praxec.command", start_args()).await;

    // The declined command hands back the parked result — the pull handle
    // survives.
    assert!(
        resp["pending_human"].is_object(),
        "a declined elicitation must keep the pending_human block, got: {resp}"
    );
    let id = resp["workflow"]["id"]
        .as_str()
        .expect("workflow.id")
        .to_string();
    let instance = store.load(&id).await.expect("instance persisted");
    assert_eq!(instance.state, "picking", "decline must not advance");
    assert_eq!(instance.version, 0, "decline must not touch the version");
    assert!(
        instance.context.get("chosen").is_none(),
        "no output mapping may run on a declined gate"
    );

    // A fresh get (a query — never elicitation-driven) still shows the gate.
    let got = call(client.peer(), "praxec.query", json!({ "workflowId": id })).await;
    assert!(
        got["pending_human"].is_object(),
        "a fresh get must still surface pending_human, got: {got}"
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// (c) A client WITHOUT the elicitation capability gets the parked response
/// untouched — byte-identical (modulo instance identity and timestamps) to a
/// reference call that never goes near the elicitation path at all
/// (`dispatch_call`, which by construction cannot push).
#[tokio::test]
async fn a_capabilityless_client_gets_the_untouched_parked_response() {
    let (server, store) = server_with_store();

    // Reference: the transport-free dispatch entry — provably elicitation-free.
    let m: JsonObject = start_args().as_object().cloned().expect("object");
    let reference = server
        .dispatch_call(CallToolRequestParams::new("praxec.command").with_arguments(m))
        .await
        .expect("reference dispatch");

    // The `()` client handler advertises NO capabilities (rmcp default).
    let (client, server_task) = connect(server.clone(), ()).await;
    let resp = call(client.peer(), "praxec.command", start_args()).await;

    assert!(
        resp["pending_human"].is_object(),
        "the parked gate must reach a capabilityless client, got: {resp}"
    );
    assert_eq!(
        normalized(&resp),
        normalized(&reference),
        "a capabilityless client's response must be byte-identical to the \
         elicitation-free reference (modulo instance id/timestamps)"
    );

    // And nothing advanced: both instances are parked at version 0.
    let id = resp["workflow"]["id"].as_str().expect("workflow.id");
    let instance = store.load(id).await.expect("instance persisted");
    assert_eq!(instance.state, "picking");
    assert_eq!(instance.version, 0);

    let _ = client.cancel().await;
    server_task.abort();
}

/// (d) An out-of-set choice on the PUSH path is fenced by the g4
/// CHOICE_MISMATCH guard: the resume submit is rejected typed and the mission
/// stays parked — state, version, and context all untouched.
#[tokio::test]
async fn an_out_of_set_choice_is_rejected_and_the_mission_stays_parked() {
    let (server, store) = server_with_store();
    let handler = ScriptedClient {
        action: ElicitationAction::Accept,
        content: Some(json!({ "chosen_id": "not-a-candidate" })),
        pushed: Arc::new(Mutex::new(Vec::new())),
    };
    let (client, server_task) = connect(server, handler).await;

    let resp = call(client.peer(), "praxec.command", start_args()).await;

    // The engine's typed rejection is the command's final answer.
    assert_eq!(
        resp["error"]["code"].as_str(),
        Some("CHOICE_MISMATCH"),
        "the push-path resume must hit the same submit guard as pull, got: {resp}"
    );

    let id = resp["workflow"]["id"].as_str().expect("workflow.id");
    let instance = store.load(id).await.expect("instance persisted");
    assert_eq!(
        instance.state, "picking",
        "a rejected out-of-set choice must not advance the mission"
    );
    assert_eq!(
        instance.version, 0,
        "a rejection must not touch the version"
    );
    assert!(
        instance.context.get("chosen").is_none(),
        "no output mapping may run on a rejected submit: {:#}",
        instance.context
    );

    let _ = client.cancel().await;
    server_task.abort();
}

/// (e) Finding #11 — a child's completing submit re-drives the parent
/// asynchronously; the parent's gate parks with no command RESPONSE carrying
/// `pending_human`, so the capability-negotiated push never fired until an
/// operator re-poked. The fix: the SAME completing call scans the ancestor
/// chain and pushes the parked gate, push-once per `(workflow_id,
/// expected_version)`.
#[tokio::test]
async fn an_async_parked_parent_gate_is_pushed_in_the_completing_call() {
    // parent: spawning -> (kind: workflow child) -> gating(actor human, full E1-E3 contract)
    // child:  ready -> submit(agent) -> done
    // Drive: start parent (parks waiting on child) -> praxec.command submit on the CHILD.
    let (server, store) = server_with_parent_child();
    let pushed = Arc::new(Mutex::new(Vec::new()));
    let handler = ScriptedClient {
        action: ElicitationAction::Accept,
        content: Some(json!({ "chosen_id": "split", "rationale": "seams first" })),
        pushed: pushed.clone(),
    };
    let (client, server_task) = connect(server, handler).await;

    // Start the parent: its deterministic chain spawns the child and durably
    // parks on it (`_subworkflow_wait`). No human gate exists yet — no push.
    let start = call(
        client.peer(),
        "praxec.command",
        json!({
            "definitionId": "parent_flow",
            "input": { "prompt": "Pick a shape for the capability." }
        }),
    )
    .await;
    let parent_id = start["workflow"]["id"]
        .as_str()
        .expect("parent workflow.id")
        .to_string();
    let parent = store.load(&parent_id).await.expect("parent persisted");
    assert_eq!(
        parent.state, "spawning",
        "the parent must park waiting on the child, got: {start}"
    );
    let child_id = parent
        .context
        .pointer("/_subworkflow_wait/child_workflow_id")
        .and_then(Value::as_str)
        .expect("recorded child workflow id")
        .to_string();
    let child = store.load(&child_id).await.expect("child persisted");
    assert_eq!(
        pushed.lock().expect("pushed lock").len(),
        0,
        "no gate exists before the child completes — nothing to push"
    );

    // The completing call: submit on the CHILD. The child response itself
    // carries no pending_human; the parent's gate parks in the async
    // re-drive. THE SAME command call must still deliver exactly one
    // elicitation push for the parent's gate, and the scripted Accept must
    // resume the parent through the governed submit.
    let candidates = json!([
        { "id": "monolith", "name": "Monolith", "tradeoffs": "simple but coupled" },
        { "id": "split", "name": "Split", "tradeoffs": "clean seams, more files" }
    ]);
    let resp = call(
        client.peer(),
        "praxec.command",
        json!({
            "workflowId": child_id,
            "expectedVersion": child.version,
            "transition": "submit",
            "arguments": { "candidates": candidates }
        }),
    )
    .await;

    assert_eq!(
        pushed.lock().expect("pushed lock").len(),
        1,
        "the completing call must deliver exactly one push for the parent's \
         async-parked gate, got response: {resp}"
    );

    // Engine side-effects (store reads), never the mock's echo: the parent
    // advanced past the gate and the g3 pick output landed the FULL object.
    let parent = store.load(&parent_id).await.expect("parent persisted");
    assert_eq!(
        parent.state, "done",
        "the pushed-and-accepted ancestor gate must advance the parent"
    );
    assert_eq!(
        parent.context["chosen"],
        json!({ "id": "split", "name": "Split", "tradeoffs": "clean seams, more files" }),
        "the resume submit must run the governed pick output on the parent"
    );

    // Same-call-resume contract: the completing call's answer is the RESUMED
    // parent's response, not the child's terminal echo.
    assert_eq!(
        resp["workflow"]["id"].as_str(),
        Some(parent_id.as_str()),
        "the resumed parent's response must replace the child's, got: {resp}"
    );

    // Then: a SECOND no-op command (rejected transition — the child is
    // terminal and its version moved on) must NOT re-push the same
    // (workflow_id, expected_version) — pushes.len() stays 1.
    let second = call(
        client.peer(),
        "praxec.command",
        json!({
            "workflowId": child_id,
            "expectedVersion": child.version,
            "transition": "submit",
            "arguments": {}
        }),
    )
    .await;
    assert!(
        second.get("error").is_some(),
        "the stale re-submit must be rejected typed, got: {second}"
    );
    assert_eq!(
        pushed.lock().expect("pushed lock").len(),
        1,
        "a later command must not re-push an already-pushed gate version"
    );

    let _ = client.cancel().await;
    server_task.abort();
}
