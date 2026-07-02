//! REST executor against a real HTTP mock server.

use std::sync::Arc;

use praxec_core::error::ExecutorError;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::rest::{RestConnections, RestExecutor};
use serde_json::json;
use wiremock::{
    matchers::{body_json, header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

fn make_executor(server: &MockServer) -> RestExecutor {
    let cfg = json!({
        "connections": {
            "api": {
                "kind": "rest",
                "baseUrl": server.uri(),
                "headers": { "X-Trace": "praxec" }
            }
        }
    });
    RestExecutor::new(Arc::new(RestConnections::from_config(&cfg)))
}

fn make_request(
    executor_config: serde_json::Value,
    arguments: serde_json::Value,
) -> ExecuteRequest {
    ExecuteRequest {
        workflow: WorkflowInstance {
            id: "wf_test".into(),
            definition_id: "demo".into(),
            definition_version: "1.0.0".into(),
            definition: json!({"initialState": "ready", "states": {}}),
            state: "ready".into(),
            version: 0,
            input: json!({}),
            context: json!({}),
            started_at: chrono::Utc::now(),
            trace_id: None,
            run_id: None,
            cancelled_at: None,
            cancelled_reason: None,
            depth: 0,
            parent: None,
        },
        transition: None,
        arguments,
        executor_config,
        idempotency_key: None,
        correlation_id: None,
    }
}

#[tokio::test]
async fn get_with_path_and_query_interpolation() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner1/repo1/issues"))
        .and(query_param("state", "open"))
        .and(header("X-Trace", "praxec"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "items": [1, 2, 3] })))
        .expect(1)
        .mount(&server)
        .await;

    let exec = make_executor(&server);
    let request = make_request(
        json!({
            "kind": "rest",
            "connection": "api",
            "method": "GET",
            "path": "/repos/{owner}/{repo}/issues",
            "query": { "state": "open" }
        }),
        json!({ "owner": "owner1", "repo": "repo1" }),
    );
    let result = exec.execute(request).await.expect("ok");
    assert_eq!(result.output["status"], 200);
    assert_eq!(result.output["body"]["items"], json!([1, 2, 3]));
}

#[tokio::test]
async fn post_resolves_body_template_from_arguments() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/pulls"))
        .and(body_json(json!({
            "title": "Fix bug",
            "head": "feature-x",
            "base": "main"
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 42 })))
        .expect(1)
        .mount(&server)
        .await;

    let exec = make_executor(&server);
    let request = make_request(
        json!({
            "kind": "rest",
            "connection": "api",
            "method": "POST",
            "path": "/pulls",
            "body": {
                "title": "$.arguments.title",
                "head": "$.arguments.head",
                "base": "main"
            }
        }),
        json!({ "title": "Fix bug", "head": "feature-x" }),
    );
    let result = exec.execute(request).await.expect("ok");
    assert_eq!(result.output["status"], 201);
    assert_eq!(result.output["body"]["id"], 42);
}

#[tokio::test]
async fn rate_limit_is_classified_for_retry() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/limited"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let exec = make_executor(&server);
    let request = make_request(
        json!({
            "kind": "rest",
            "connection": "api",
            "method": "GET",
            "path": "/limited"
        }),
        json!({}),
    );
    let err = exec.execute(request).await.unwrap_err();
    matches!(err, ExecutorError::RateLimited(_))
        .then_some(())
        .expect("rate-limited classification");
}

#[tokio::test]
async fn server_error_is_transient() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/oops"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let exec = make_executor(&server);
    let request = make_request(
        json!({
            "kind": "rest",
            "connection": "api",
            "method": "GET",
            "path": "/oops"
        }),
        json!({}),
    );
    let err = exec.execute(request).await.unwrap_err();
    matches!(err, ExecutorError::Transient(_))
        .then_some(())
        .expect("transient classification");
}

#[tokio::test]
async fn client_error_is_permanent() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/bad"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let exec = make_executor(&server);
    let request = make_request(
        json!({
            "kind": "rest",
            "connection": "api",
            "method": "GET",
            "path": "/bad"
        }),
        json!({}),
    );
    let err = exec.execute(request).await.unwrap_err();
    matches!(err, ExecutorError::Permanent(_))
        .then_some(())
        .expect("permanent classification");
}
