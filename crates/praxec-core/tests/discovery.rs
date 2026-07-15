//! Tests for the INIT-2 discovery layer.

use praxec_core::discovery::{
    DiscoveryIndex, DiscoveryKind, InMemoryDiscoveryIndex, SearchRequest,
};
use serde_json::json;

fn sample_config() -> serde_json::Value {
    json!({
        "version": "1.0.0",
        "connections": {
            "github": { "kind": "mcp", "command": "github-mcp-server" }
        },
        "proxy": {
            "expose": [
                {
                    "name": "github.list_issues",
                    "title": "List GitHub issues",
                    "description": "List issues from a repository.",
                    "tags": ["github", "issues", "read"],
                    "executor": { "kind": "mcp", "connection": "github", "tool": "list_issues" }
                },
                {
                    "name": "dotnet.test",
                    "title": "Run .NET tests",
                    "description": "Run dotnet test in the configured project.",
                    "tags": ["dotnet", "test", "cli"],
                    "executor": { "kind": "cli", "connection": "dotnet" }
                }
            ]
        },
        "workflows": {
            "engineering_change": {
                "description": "Plan, risk-review, approve, execute, and verify.",
                "tags": ["governed", "change-management"],
                "initialState": "planning",
                "states": {
                    "planning": {
                        "transitions": {
                            "submit_plan": { "target": "risk_review", "title": "Submit plan" }
                        }
                    },
                    "risk_review": {},
                    "done": { "terminal": true }
                }
            }
        }
    })
}

#[tokio::test]
async fn home_returns_search_and_list_links() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    let home = idx.home().await.unwrap();
    let rels: Vec<&str> = home["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["rel"].as_str().unwrap())
        .collect();
    assert!(rels.contains(&"search"));
    assert!(rels.contains(&"list_workflows"));
    assert!(rels.contains(&"list_capabilities"));
}

#[tokio::test]
async fn search_finds_workflow_by_tag() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    let hits = idx
        .search(SearchRequest {
            query: "governed".into(),
            kind: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(!hits.is_empty(), "expected hits for 'governed'");
    let first = &hits[0].item;
    assert_eq!(first.id, "engineering_change");
    assert_eq!(first.kind, DiscoveryKind::Workflow);
    let rels: Vec<&str> = first.links.iter().map(|l| l.rel.as_str()).collect();
    assert!(
        rels.contains(&"start"),
        "workflow item must include a start link"
    );
}

#[tokio::test]
async fn search_finds_capability_by_name() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    let hits = idx
        .search(SearchRequest {
            query: "issues".into(),
            kind: Some(DiscoveryKind::Capability),
            limit: 5,
        })
        .await
        .unwrap();
    assert!(hits.iter().any(|h| h.item.id == "github.list_issues"));
    let cap = hits
        .iter()
        .find(|h| h.item.id == "github.list_issues")
        .unwrap();
    assert!(
        cap.item
            .links
            .iter()
            .any(|l| l.rel == "start_proxy_session"),
        "capability link should be start_proxy_session"
    );
}

#[tokio::test]
async fn search_kind_filter_excludes_others() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    let hits = idx
        .search(SearchRequest {
            query: "".into(),
            kind: Some(DiscoveryKind::Workflow),
            limit: 100,
        })
        .await
        .unwrap();
    assert!(hits.iter().all(|h| h.item.kind == DiscoveryKind::Workflow));
}

#[tokio::test]
async fn describe_returns_item_by_id() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    let item = idx.describe("dotnet.test").await.unwrap();
    assert!(item.is_some());
    assert_eq!(item.unwrap().kind, DiscoveryKind::Capability);
}

#[tokio::test]
async fn describe_unknown_returns_none() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    assert!(idx.describe("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn search_finds_capability_by_alias() {
    let cfg = json!({
        "version": "1.0.0",
        "proxy": {
            "expose": [
                {
                    "name": "release.promote",
                    "title": "Promote a release",
                    "description": "Promote a release candidate to production.",
                    "tags": ["release"],
                    "aliases": ["deploy", "ship"],
                    "executor": { "kind": "noop" }
                }
            ]
        }
    });
    let idx = InMemoryDiscoveryIndex::from_config(&cfg).unwrap();
    let hits = idx
        .search(SearchRequest {
            query: "deploy".into(),
            kind: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.item.id == "release.promote"),
        "alias 'deploy' should find 'release.promote'"
    );
}

#[tokio::test]
async fn search_prefix_matches() {
    let idx = InMemoryDiscoveryIndex::from_config(&sample_config()).unwrap();
    let hits = idx
        .search(SearchRequest {
            query: "engin".into(),
            kind: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.item.id == "engineering_change"),
        "prefix 'engin' should match 'engineering_change'"
    );
}

#[tokio::test]
async fn search_fuzzy_matches_typo() {
    let cfg = json!({
        "version": "1.0.0",
        "proxy": {
            "expose": [
                {
                    "name": "deploy.prod",
                    "title": "Deploy to production",
                    "description": "Deploy the current build to prod.",
                    "tags": ["deploy", "production"],
                    "executor": { "kind": "noop" }
                }
            ]
        }
    });
    let idx = InMemoryDiscoveryIndex::from_config(&cfg).unwrap();
    let hits = idx
        .search(SearchRequest {
            query: "deply".into(),
            kind: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.item.id == "deploy.prod"),
        "fuzzy 'deply' should match 'deploy.prod'"
    );
}

#[tokio::test]
async fn discovery_include_filter_respected() {
    let cfg = json!({
        "version": "1.0.0",
        "discovery": { "include": ["workflows"] },
        "proxy": {
            "expose": [
                { "name": "x", "executor": { "kind": "noop" } }
            ]
        },
        "workflows": {
            "demo": { "initialState": "s", "states": { "s": {} } }
        }
    });
    let idx = InMemoryDiscoveryIndex::from_config(&cfg).unwrap();
    let all = idx.list(None).await.unwrap();
    assert!(all.iter().all(|i| i.kind == DiscoveryKind::Workflow));
    assert!(all.iter().any(|i| i.id == "demo"));
}

// FB-6 — a declared `connections:` entry is a wired tool (reachable by executor
// transitions and auto-driven agent leaves). It must also be DISCOVERABLE via
// `praxec.query` search when `discovery.include` is left unset — the default
// now includes "connections". Regression for the dogfood report where a
// `log-analyzer` connection returned no search match.
#[tokio::test]
async fn connection_indexed_and_searchable_by_default() {
    let cfg = json!({
        "version": "1.0.0",
        // NOTE: no `discovery.include` — exercise the default.
        "connections": {
            "log-analyzer": { "kind": "mcp", "command": "log-analyzer" }
        }
    });
    let idx = InMemoryDiscoveryIndex::from_config(&cfg).unwrap();

    let all = idx.list(None).await.unwrap();
    assert!(
        all.iter()
            .any(|i| i.id == "connection:log-analyzer" && i.kind == DiscoveryKind::Connection),
        "declared connection should be in the default index; got {:?}",
        all.iter().map(|i| &i.id).collect::<Vec<_>>()
    );

    let hits = idx
        .search(SearchRequest {
            query: "log-analyzer".into(),
            kind: None,
            limit: 10,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().any(|h| h.item.id == "connection:log-analyzer"),
        "search for 'log-analyzer' should match the connection by default"
    );
}

// CMP-031 — a typo'd discovery.include token (`workflow` for `workflows`)
// would silently drop a whole category from the index. The indexer must reject
// it with INVALID_DISCOVERY_INCLUDE rather than building a partial index.
#[test]
fn discovery_include_unknown_token_is_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "discovery": { "include": ["workflow"] }, // typo: should be "workflows"
        "workflows": {
            "demo": { "initialState": "s", "states": { "s": {} } }
        }
    });
    let err = match InMemoryDiscoveryIndex::from_config(&cfg) {
        Ok(_) => panic!("expected INVALID_DISCOVERY_INCLUDE for an unknown include token"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("INVALID_DISCOVERY_INCLUDE"),
        "expected INVALID_DISCOVERY_INCLUDE, got: {err}"
    );
}

#[tokio::test]
async fn guidance_excluded_from_default_search_but_describable() {
    // SPEC §12: guidance fragments are fetched by known subject via
    // `gateway.describe`, not searched. They must stay in the index (so
    // describe can find them) but must NOT appear as ranked search hits
    // when the caller hasn't explicitly asked for `kind=guidance`.
    let cfg = json!({
        "version": "1.0.0",
        "skills": {
            "review.style.house-voice": { "verb": "review", "lifecycle": "stable", "body": "Lead with the reader's problem." }
        },
        "workflows": {
            "demo": { "initialState": "s", "states": { "s": { "terminal": true } } }
        }
    });
    let idx = InMemoryDiscoveryIndex::from_config(&cfg).unwrap();

    // Untargeted search: guidance must not surface.
    let hits = idx
        .search(SearchRequest {
            query: String::new(),
            kind: None,
            limit: 20,
        })
        .await
        .unwrap();
    assert!(
        hits.iter().all(|h| h.item.kind != DiscoveryKind::Guidance),
        "default search must hide guidance; got: {:?}",
        hits.iter().map(|h| h.item.id.as_str()).collect::<Vec<_>>()
    );

    // Targeted search with kind=guidance must surface them.
    let targeted = idx
        .search(SearchRequest {
            query: String::new(),
            kind: Some(DiscoveryKind::Guidance),
            limit: 20,
        })
        .await
        .unwrap();
    assert!(
        targeted
            .iter()
            .any(|h| h.item.id == "review.style.house-voice"),
        "explicit kind=guidance search must include the fragment"
    );

    // Describe must always find it regardless of kind filtering.
    let described = idx.describe("review.style.house-voice").await.unwrap();
    assert!(
        described.is_some(),
        "describe must always resolve a declared subject"
    );

    // list(None) must also exclude guidance.
    let listed = idx.list(None).await.unwrap();
    assert!(
        listed.iter().all(|i| i.kind != DiscoveryKind::Guidance),
        "default list must hide guidance"
    );
}
