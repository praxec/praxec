//! PR3 — exercise the three praxec-meta production scripts via the
//! real ScriptExecutor (not the fixture short-circuit). The meta
//! flows e2e bypasses these via CapShortCircuit; this file
//! is the only place their bash bodies are tested against the real
//! script-executor surface.
//!
//! Strategy: spin up TcpListener-backed mock servers per provider,
//! point the scripts at them via the *_BASE_URL escape hatches, exec
//! the script via the real `ScriptExecutor`, assert JSON envelope.
//!
//! Wiring note: `CARGO_BIN_EXE_px` only resolves in the crate that
//! *owns* the binary. Since the `px` binary belongs to
//! `praxec-tui` (a separate crate), we locate it at runtime via
//! `praxec_bin_path()`, which navigates up to the workspace root and
//! finds `target/debug/praxec` (or `target/release/praxec` if present).
//! Build the binary before running this test file in isolation:
//!   cargo build -p praxec-tui --bin px
//! Running `cargo test --workspace` does this automatically.

#![cfg(unix)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::ScriptExecutor;
use serde_json::{json, Value};

/// Locate the `px` binary. This test lives in `praxec-tui`, the
/// crate that OWNS the `px` binary, so Cargo sets `CARGO_BIN_EXE_px`
/// to the freshly-built path — no manual target/ navigation needed.
fn praxec_bin_path() -> String {
    env!("CARGO_BIN_EXE_px").to_string()
}

/// Spawn a one-shot HTTP server on an ephemeral port. Returns the
/// bound URL and a JoinHandle (the test should join the handle after
/// the script call returns). The server responds to ANY request with
/// `status` + `body`, then closes.
///
/// The mock thread self-terminates after 10 s if no connection arrives,
/// so `handle.join()` never blocks forever when a script crashes before
/// making its HTTP request.
fn spawn_mock(status: u16, body: &'static str) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr: SocketAddr = listener.local_addr().expect("addr");
    let url = format!("http://{}", addr);
    let body_owned = body.to_string();
    listener.set_nonblocking(true).expect("nonblocking");
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((mut sock, _)) => {
                    // Bound the read-from-curl phase so a slow client
                    // can't hang the mock thread forever.
                    let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    let status_line = match status {
                        200 => "200 OK",
                        401 => "401 Unauthorized",
                        404 => "404 Not Found",
                        429 => "429 Too Many Requests",
                        _ => "500 Internal Server Error",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body_owned}",
                        body_owned.len()
                    );
                    let _ = sock.write_all(resp.as_bytes());
                    return;
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        eprintln!("spawn_mock: no connection within 10s deadline; exiting");
                        return;
                    }
                    // yield_now lets the OS scheduler run the accept
                    // thread as soon as the socket has a pending
                    // connection — eliminates the 50ms latency window
                    // that raced curl's connection budget under heavy
                    // parallel load (~1-in-3 flake on cargo test
                    // --workspace).
                    std::thread::yield_now();
                }
                Err(e) => {
                    eprintln!("spawn_mock: accept error: {e}");
                    return;
                }
            }
        }
    });
    (url, handle)
}

/// Load the script body from the fixture copy of the praxec-meta
/// scripts-library. Fixture filenames keep the original dots
/// (e.g. `fetch.provider-model-inventory.yaml`).
fn script_body(subject: &str) -> String {
    // The praxec-meta script fixtures live in the `praxec-core`
    // crate's test tree (also exercised by the fixture-based flow
    // tests). Both crates are in the same repo.
    // CARGO_MANIFEST_DIR = <repo>/crates/praxec-tui.
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/
    path.pop(); // repo root
    path.push("crates/praxec-core/tests/fixtures/praxec-meta/scripts-library");
    path.push(format!("{}.yaml", subject));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read script yaml at {}: {e}", path.display()));
    let doc: Value = serde_yaml::from_str(&raw).expect("parse yaml");
    doc["scripts"][subject]["body"]
        .as_str()
        .expect("body field")
        .to_string()
}

/// Build an ExecuteRequest that mirrors what the runtime stamps when it
/// invokes a `kind: script` executor. The script body is placed in
/// `workflow.definition._scriptsLibrary` — that is where ScriptExecutor
/// looks per SPEC §22 / src/script.rs.
fn exec_request(
    subject: &str,
    args: Vec<Value>,
    env: Vec<(&str, &str)>,
    treat_nonzero_as_failure: bool,
) -> ExecuteRequest {
    let body = script_body(subject);
    let env_obj: serde_json::Map<String, Value> = env
        .into_iter()
        .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
        .collect();
    let definition = json!({
        "_scriptsLibrary": {
            subject: { "body": body, "hash": "sha256:test" }
        }
    });
    ExecuteRequest {
        workflow: WorkflowInstance {
            id: "test-meta-script".into(),
            definition_id: subject.to_string(),
            definition_version: "0".into(),
            definition,
            state: "running".into(),
            version: 0,
            input: json!({}),
            context: json!({}),
            started_at: Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: Some("run".into()),
        arguments: json!({}),
        executor_config: json!({
            "kind": "script",
            "subject": subject,
            "args": args,
            "env": env_obj,
            "treatNonZeroAsFailure": treat_nonzero_as_failure,
        }),
        idempotency_key: None,
        correlation_id: None,
    }
}

// ── Test 1: fetch.provider-model-inventory — 200 path ────────────────────

#[tokio::test]
async fn fetch_provider_inventory_emits_anthropic_models_on_200() {
    let body = r#"{"data":[{"id":"claude-sonnet-4-6","type":"model"},{"id":"claude-opus-4-7","type":"model"}]}"#;
    let (url, handle) = spawn_mock(200, body);

    let req = exec_request(
        "fetch.provider-model-inventory",
        vec![json!("anthropic")],
        vec![
            ("ANTHROPIC_BASE_URL", url.as_str()),
            ("ANTHROPIC_API_KEY", "test-key"),
        ],
        true,
    );
    let executor = ScriptExecutor::new();
    let result = executor.execute(req).await.expect("script runs");
    let _ = handle.join();

    let parsed = &result.output["json"];
    assert!(
        parsed["inventory"]["anthropic"].is_array(),
        "expected anthropic models array; got: {result:#?}"
    );
    assert_eq!(
        parsed["inventory"]["anthropic"][0]["id"],
        json!("claude-sonnet-4-6"),
        "first model id should pass through; got: {result:#?}"
    );
    let models = parsed["inventory"]["anthropic"].as_array().expect("array");
    assert_eq!(
        models.len(),
        2,
        "expected 2 anthropic models in mocked response; got: {result:#?}"
    );
    assert_eq!(
        models[1]["id"],
        json!("claude-opus-4-7"),
        "second model id should pass through; got: {result:#?}"
    );
    assert!(
        parsed["errors"].as_array().unwrap_or(&vec![]).is_empty(),
        "no errors on 200; got: {result:#?}"
    );
}

// ── Test 2: fetch.provider-model-inventory — 401 path ────────────────────

#[tokio::test]
async fn fetch_provider_inventory_classifies_anthropic_401_as_auth_error() {
    let (url, handle) = spawn_mock(401, r#"{"error":{"message":"invalid api key"}}"#);

    let req = exec_request(
        "fetch.provider-model-inventory",
        vec![json!("anthropic")],
        vec![
            ("ANTHROPIC_BASE_URL", url.as_str()),
            ("ANTHROPIC_API_KEY", "wrong-key"),
        ],
        true,
    );
    let executor = ScriptExecutor::new();
    let result = executor.execute(req).await.expect("script runs");
    let _ = handle.join();

    let parsed = &result.output["json"];
    let errors = parsed["errors"].as_array().expect("errors array");
    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error; got: {result:#?}"
    );
    assert_eq!(errors[0]["provider"], json!("anthropic"));
    assert_eq!(errors[0]["kind"], json!("Auth401"));
    // Inventory should be empty/absent for that provider on failure
    assert!(
        parsed["inventory"]["anthropic"].is_null()
            || parsed["inventory"]["anthropic"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false),
        "no anthropic inventory on 401; got: {result:#?}"
    );
}

// ── Test 3: install.agents-config — atomic write + rollback ──────────────

#[tokio::test]
async fn install_agents_config_writes_atomically_and_rolls_back_on_invalid() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let target = tmpdir.path().join("models.yaml");
    let praxec_bin = praxec_bin_path();

    // Valid YAML — should write and round-trip OK
    let valid_yaml =
        "version: 1\ndefault:\n  - provider:\n      name: anthropic\n    model: claude-sonnet-4-6\n";
    let req = exec_request(
        "install.agents-config",
        vec![json!(valid_yaml), json!(target.to_str().unwrap())],
        vec![("PRAXEC_BIN", praxec_bin.as_str())],
        true,
    );
    let executor = ScriptExecutor::new();
    let result = executor.execute(req).await.expect("script runs");
    let parsed = &result.output["json"];
    assert_eq!(
        parsed["round_trip_ok"],
        json!(true),
        "valid YAML round-trips; got: {result:#?}"
    );
    assert!(target.exists(), "file written to disk");

    // Invalid YAML — should NOT replace the on-disk file
    let prior_content = std::fs::read_to_string(&target).expect("read existing file");
    // Missing 'default:' field → MISSING_DEFAULT error
    let invalid_yaml = "version: 1\n# default field intentionally absent\n";
    let req2 = exec_request(
        "install.agents-config",
        vec![json!(invalid_yaml), json!(target.to_str().unwrap())],
        vec![("PRAXEC_BIN", praxec_bin.as_str())],
        // Must be false so ScriptExecutor doesn't propagate the exit-1 as Err
        false,
    );
    let result2 = executor
        .execute(req2)
        .await
        .expect("script runs (exit 1 allowed)");
    let parsed2 = &result2.output["json"];
    assert_eq!(
        parsed2["round_trip_ok"],
        json!(false),
        "invalid YAML rejected; got: {result2:#?}"
    );
    // The on-disk content must be unchanged — rollback contract
    let after_content = std::fs::read_to_string(&target).expect("read after");
    assert_eq!(
        prior_content, after_content,
        "rollback preserved prior file content"
    );
}

// ── Test 4: verify.auth-only-smoke-test — mixed 200+401 bindings ─────────

#[tokio::test]
async fn auth_only_smoke_test_classifies_per_binding() {
    let tmpdir = tempfile::tempdir().expect("tempdir");
    let agents = tmpdir.path().join("models.yaml");
    std::fs::write(
        &agents,
        "version: 1\ndefault:\n  - provider:\n      name: anthropic\n    model: claude-sonnet-4-6\n  - provider:\n      name: openai\n    model: gpt-5\n",
    )
    .expect("write models.yaml");

    let (anth_url, anth_handle) = spawn_mock(200, r#"{"id":"msg_1","content":[{"text":"."}]}"#);
    let (oai_url, oai_handle) = spawn_mock(401, r#"{"error":{"message":"bad key"}}"#);

    let req = exec_request(
        "verify.auth-only-smoke-test",
        vec![json!(agents.to_str().unwrap())],
        vec![
            ("ANTHROPIC_BASE_URL", anth_url.as_str()),
            ("OPENAI_BASE_URL", oai_url.as_str()),
            ("ANTHROPIC_API_KEY", "k1"),
            ("OPENAI_API_KEY", "k2"),
        ],
        true,
    );
    let executor = ScriptExecutor::new();
    let result = executor.execute(req).await.expect("script runs");
    let _ = anth_handle.join();
    let _ = oai_handle.join();

    let parsed = &result.output["json"];
    let results = parsed["results"].as_array().expect("results array");
    assert_eq!(
        results.len(),
        2,
        "two bindings → two results; got: {result:#?}"
    );

    let anth = results
        .iter()
        .find(|r| {
            r["binding"]
                .as_str()
                .unwrap_or("")
                .starts_with("anthropic/")
        })
        .expect("anthropic row");
    assert_eq!(
        anth["auth_ok"],
        json!(true),
        "anthropic auth_ok; got: {anth:#?}"
    );
    assert_eq!(
        anth["class"],
        json!("Ok"),
        "anthropic class; got: {anth:#?}"
    );

    let oai = results
        .iter()
        .find(|r| r["binding"].as_str().unwrap_or("").starts_with("openai/"))
        .expect("openai row");
    assert_eq!(
        oai["auth_ok"],
        json!(false),
        "openai auth_ok; got: {oai:#?}"
    );
    assert_eq!(
        oai["class"],
        json!("Auth401"),
        "openai class; got: {oai:#?}"
    );

    assert!(
        parsed["disclaimer"]
            .as_str()
            .unwrap_or("")
            .contains("CAPABILITY NOT TESTED"),
        "disclaimer must name the limitation; got: {result:#?}"
    );
}
