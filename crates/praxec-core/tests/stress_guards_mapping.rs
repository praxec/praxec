//! Stress scenarios that exercise declarative guards, link filtering,
//! and output-mapping operators (set, add, concat, starts_with, contains,
//! jsonpath compares). Each test pins a declarative replacement for a
//! previously procedural workaround.

use std::sync::Arc;

use serde_json::json;

mod common;
use common::*;

/// **S-01. Bounded loop with a counter.** "Remediate up to 3 times, then
/// you must escalate." This requires three things, each individually a
/// declarative gap:
///
/// - **A way to seed the counter.** The original failure was: the very
///   first remediate's guard `$.context.attempts < 3` evaluated against
///   missing `attempts`, returned false, and blocked the loop before it
///   started. Fixed by `initialContext: {...}` on the workflow
///   definition — a declarative way to seed instance state without an
///   `onEnter` that would also fire on every self-transition.
/// - **Arithmetic in output mappings.** Without `{ add: [a, b] }` the
///   only way to write `count + 1` was a custom executor — a procedural
///   workaround. Fixed by the operator object form in
///   `mapping::resolve_value`.
/// - **Scope-aware reads.** The mapping value `$.context.attempts` has
///   to resolve against the *workflow context* even though the mapping
///   normally reads `$.output.*`. Fixed by routing through
///   `read_in_scopes`.
#[tokio::test]
async fn s01_bounded_loop_counter() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            initialContext:
              attempts: 0
            states:
              open:
                transitions:
                  remediate:
                    target: open
                    guards:
                      - { kind: jsonpath, expr: "$.context.attempts < 3" }
                    executor: { kind: noop }
                    output:
                      attempts: { add: ["$.context.attempts", 1] }
                  escalate:
                    target: escalated
                    guards:
                      - { kind: jsonpath, expr: "$.context.attempts >= 3" }
                    executor: { kind: noop }
              escalated:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    assert_eq!(s.last()["context"]["attempts"], 0);

    for i in 1..=3 {
        s.submit("remediate", json!({}), anon()).await;
        assert_eq!(s.last()["context"]["attempts"], i);
        assert_eq!(s.last()["workflow"]["state"], "open");
    }

    // After 3 remediations, the remediate guard fails and only `escalate`
    // makes progress.
    s.submit("remediate", json!({}), anon()).await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");

    s.submit("escalate", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert_eq!(s.last()["workflow"]["state"], "escalated");
}

/// **S-02. Schema defaults are applied to arguments before validation.**
/// "If the caller omits `priority`, default to `normal`." Standard JSON
/// Schema feature; without it, every executor has to null-check the field.
///
/// Fix: walk the inputSchema's `properties` and fill in any `default` for
/// missing keys before validating + dispatching.
#[tokio::test]
async fn s02_schema_defaults_applied_to_arguments() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    inputSchema:
                      type: object
                      required: [priority, ticket]
                      properties:
                        priority: { type: string, default: "normal" }
                        ticket:   { type: string }
                      additionalProperties: false
                    executor: { kind: noop }
                    output:
                      priority: "$.arguments.priority"   # echoes resolved default
                      ticket:   "$.arguments.ticket"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    // Caller omits `priority`; the runtime must apply the schema default
    // and pass validation.
    s.submit("go", json!({ "ticket": "T-1" }), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert_eq!(s.last()["context"]["priority"], "normal");
    assert_eq!(s.last()["context"]["ticket"], "T-1");
}

/// **S-04. Nested schema defaults.** Defaults aren't only top-level —
/// nested object properties should fill in too. Without recursion in the
/// default-application walk, complex shapes can't declare a default for a
/// nested field.
#[tokio::test]
async fn s04_nested_schema_defaults() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    inputSchema:
                      type: object
                      required: [request]
                      properties:
                        request:
                          type: object
                          properties:
                            priority: { type: string, default: "normal" }
                            channel:  { type: string, default: "email" }
                            ticket:   { type: string }
                    executor: { kind: noop }
                    output:
                      priority: "$.arguments.request.priority"
                      channel:  "$.arguments.request.channel"
                      ticket:   "$.arguments.request.ticket"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({ "request": { "ticket": "T-1" } }), anon())
        .await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
    assert_eq!(s.last()["context"]["priority"], "normal");
    assert_eq!(s.last()["context"]["channel"], "email");
    assert_eq!(s.last()["context"]["ticket"], "T-1");
}

/// **S-05.** `set:` operator for declaring literal values in output
/// mappings (useful for status flags, bookmarks, etc.).
#[tokio::test]
async fn s05_output_mapping_set_literal() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  mark_reviewed:
                    target: done
                    executor: { kind: noop }
                    output:
                      status: { set: "reviewed" }
                      reviewer_count: { set: 1 }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("mark_reviewed", json!({}), anon()).await;
    assert_eq!(s.last()["context"]["status"], "reviewed");
    assert_eq!(s.last()["context"]["reviewer_count"], 1);
}

/// **S-11. Link filtering by guards.** When a workflow declares
/// `linkFilter: byGuards`, the response's `links` array only shows
/// transitions whose guards would currently pass — the LLM never sees
/// transitions it can't take. Reduces wasted submit attempts.
#[tokio::test]
async fn s11_link_filter_byguards() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: triaged
            linkFilter: byGuards
            initialContext:
              risk: 30
            states:
              triaged:
                transitions:
                  auto_approve:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.context.risk <= 50" }
                    executor: { kind: noop }
                  manual_review:
                    target: review
                    guards:
                      - { kind: expr, expr: "$.context.risk > 50" }
                    executor: { kind: noop }
              review:
                terminal: true
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    // risk=30 ⇒ only auto_approve is reachable.
    let rels = s.link_rels();
    assert_eq!(rels, vec!["auto_approve"]);
}

/// **S-12. Per-state link filter overrides workflow-level.** The flag
/// can be opt-in for one tricky state without committing the whole
/// workflow. The state-level setting wins.
#[tokio::test]
async fn s12_link_filter_per_state_override() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: open
            linkFilter: all
            initialContext:
              risk: 90
            states:
              open:
                linkFilter: byGuards
                transitions:
                  go_safe:
                    target: open
                    guards:
                      - { kind: expr, expr: "$.context.risk < 50" }
                    executor: { kind: noop }
                  go_risky:
                    target: open
                    guards:
                      - { kind: expr, expr: "$.context.risk >= 50" }
                    executor: { kind: noop }
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    // risk=90 ⇒ only go_risky is reachable, even though the workflow's
    // top-level linkFilter is "all" (the state's "byGuards" wins).
    assert_eq!(s.link_rels(), vec!["go_risky"]);
}

/// **S-15. Composite guards (`all_of`, `any_of`, `not`).** "Tests
/// passed AND coverage didn't drop" is one logical condition; without
/// composition you'd need an intermediate state. The composite guard
/// kinds let the workflow author state the rule directly.
#[tokio::test]
async fn s15_composite_guards() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              tests_passed: true
              coverage_dropped: false
            states:
              s:
                transitions:
                  ship_when_all_clear:
                    target: shipped
                    guards:
                      - kind: all_of
                        guards:
                          - { kind: expr, expr: "$.context.tests_passed == true" }
                          - kind: not
                            guard: { kind: expr, expr: "$.context.coverage_dropped == true" }
                    executor: { kind: noop }
                  ship_when_any_emergency:
                    target: shipped
                    guards:
                      - kind: any_of
                        guards:
                          - { kind: expr, expr: "$.context.always_false == true" }
                          - { kind: expr, expr: "$.context.tests_passed == true" }
                    executor: { kind: noop }
              shipped:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));

    // all_of: tests passed AND coverage didn't drop → ship_when_all_clear works
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship_when_all_clear", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");

    // any_of: at least one passes → ship_when_any_emergency works
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec.clone())));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship_when_any_emergency", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
}

#[tokio::test]
async fn s15b_composite_guard_blocks_when_any_clause_fails() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              tests_passed: true
              coverage_dropped: true            # this should sink all_of
            states:
              s:
                transitions:
                  ship:
                    target: shipped
                    guards:
                      - kind: all_of
                        guards:
                          - { kind: expr, expr: "$.context.tests_passed == true" }
                          - kind: not
                            guard: { kind: expr, expr: "$.context.coverage_dropped == true" }
                    executor: { kind: noop }
              shipped:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship", json!({}), anon()).await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
}

/// **S-13. String + bool comparison in jsonpath.** "Continue only if the
/// last test run reported success." Without bool literals + string
/// equality, the only options were custom guards or proxying every
/// boolean as 1/0.
///
/// Fix: `eval_tiny_numeric_expr` now supports string literals
/// (`"foo"` / `'foo'`), bool literals (`true` / `false`), `null`, and
/// path-to-path / path-to-literal `==` / `!=`.
#[tokio::test]
async fn s13_jsonpath_string_and_bool_compare() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              ok: true
              status: "ready"
            states:
              s:
                transitions:
                  go_when_ok:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.context.ok == true" }
                      - { kind: expr, expr: "$.context.status == 'ready'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("go_when_ok", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
}

#[tokio::test]
async fn s13b_jsonpath_path_to_path_compare() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            initialContext:
              before: 5
              after: 7
            states:
              s:
                transitions:
                  improved:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.context.after > $.context.before" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("improved", json!({}), anon()).await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
}

/// **S-06.** Output mapping reads from the broader scopes (workflow input,
/// arguments, context) — not just the executor's output. Without this you
/// can't pass a transition argument straight into context for later steps.
#[tokio::test]
async fn s06_output_mapping_reads_arguments_and_input() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            inputSchema:
              type: object
              properties: { project: { type: string } }
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    output:
                      caller_note: "$.arguments.note"
                      project:     "$.workflow.input.project"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));

    s.start("demo", json!({ "project": "cool-app" }), anon())
        .await;
    s.submit("go", json!({ "note": "shipping after lunch" }), anon())
        .await;
    assert_eq!(s.last()["context"]["caller_note"], "shipping after lunch");
    assert_eq!(s.last()["context"]["project"], "cool-app");
}

#[tokio::test]
async fn s17_concat_in_output_mapping() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    output:
                      message:
                        concat:
                          - "branch="
                          - "$.arguments.branch"
                          - " (pr "
                          - "$.arguments.pr"
                          - ")"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({ "branch": "feat/login", "pr": 42 }), anon())
        .await;
    assert_eq!(s.last()["context"]["message"], "branch=feat/login (pr 42)");
}

#[tokio::test]
async fn s17b_concat_with_null_element() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  go:
                    target: done
                    executor: { kind: noop }
                    output:
                      message:
                        concat:
                          - "before="
                          - "$.arguments.missing"
                          - "=after"
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("go", json!({}), anon()).await;
    assert_eq!(s.last()["context"]["message"], "before=null=after");
}

#[tokio::test]
async fn s17c_starts_with_guard_passes() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  ship:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.branch starts_with 'feat/'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship", json!({ "branch": "feat/login" }), anon())
        .await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
}

#[tokio::test]
async fn s17d_starts_with_guard_rejects() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  ship:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.branch starts_with 'feat/'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("ship", json!({ "branch": "fix/leak" }), anon())
        .await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
}

#[tokio::test]
async fn s17e_contains_guard_passes() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  retry:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.error contains 'timeout'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit(
        "retry",
        json!({ "error": "upstream connection timeout after 30s" }),
        anon(),
    )
    .await;
    assert_eq!(s.last()["result"]["status"], "succeeded");
}

#[tokio::test]
async fn s17f_contains_guard_rejects() {
    let yaml = r#"
        version: "1.0.0"
        workflows:
          demo:
            initialState: s
            states:
              s:
                transitions:
                  retry:
                    target: done
                    guards:
                      - { kind: expr, expr: "$.arguments.error contains 'timeout'" }
                    executor: { kind: noop }
              done:
                terminal: true
    "#;
    let exec = Arc::new(FixedExecutor::new(json!({})));
    let mut s = Scenario::build(yaml, Arc::new(AnyKind(exec)));
    s.start("demo", json!({}), anon()).await;
    s.submit("retry", json!({ "error": "not found" }), anon())
        .await;
    assert_eq!(s.last()["error"]["code"], "GUARD_REJECTED");
}
