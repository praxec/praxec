//! Production [`CatalogIo`] (Phase 1, §T6): a bounded, best-effort GitHub
//! REST client + a generic JSON GET. Mirrors
//! [`crate`]'s sibling pattern in `praxec::currency::RealCurrencyIo` — every
//! probe degrades to `Err(String)` on any failure (network down, non-200, bad
//! JSON) rather than panicking or hanging. `assemble` (§T5) folds an `Err`
//! into a warning, never an abort, so a rate-limited or unreachable registry
//! simply contributes zero candidates.
//!
//! Both timeouts are short: this seam is read-only discovery, not something
//! any workflow transition blocks on, so a slow host must fail fast rather
//! than hang a caller.

use super::registry::{CatalogIo, GhRepo};
use serde_json::Value;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn build_client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("praxec/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("building HTTP client: {e}"))
}

/// Pure mapping from a GitHub `GET /orgs/<org>/repos` JSON array to
/// [`GhRepo`]s. Split out of [`RealCatalogIo::github_org_repos`] so the
/// parsing is unit-testable without a network call. `None` when `body` isn't
/// a JSON array (e.g. GitHub's `{"message": "Not Found"}` on a bad org).
fn parse_gh_repos(body: &Value) -> Option<Vec<GhRepo>> {
    let repos = body.as_array()?;
    Some(
        repos
            .iter()
            .filter_map(|r| {
                let name = r.get("name")?.as_str()?.to_string();
                let description = r
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let topics = r
                    .get("topics")
                    .and_then(Value::as_array)
                    .map(|t| {
                        t.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                Some(GhRepo {
                    name,
                    description,
                    topics,
                })
            })
            .collect(),
    )
}

/// The production [`CatalogIo`]: a bounded, best-effort GitHub REST client +
/// generic JSON GET. Every method degrades to `Err(String)` on any failure —
/// never panics, never blocks past its timeout budget.
pub struct RealCatalogIo;

impl CatalogIo for RealCatalogIo {
    fn github_org_repos(&self, org: &str) -> Result<Vec<GhRepo>, String> {
        // `topics` is included on the standard repos-list response (no
        // preview header needed since GitHub GA'd the topics field).
        let url = format!("https://api.github.com/orgs/{org}/repos?per_page=100");
        let body = self.fetch_json(&url)?;
        parse_gh_repos(&body).ok_or_else(|| {
            format!("github org '{org}' repos response was not a JSON array: {body}")
        })
    }

    fn fetch_json(&self, url: &str) -> Result<Value, String> {
        let client = build_client()?;
        let resp = client
            .get(url)
            .send()
            .map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GET {url} returned HTTP {status}"));
        }
        resp.json::<Value>()
            .map_err(|e| format!("parsing JSON from {url}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gh_repos_maps_name_description_topics() {
        let body = serde_json::json!([
            { "name": "fmeca", "description": "FMECA MCP", "topics": ["mcp", "review"] },
            { "name": "no-desc", "description": null, "topics": [] },
        ]);
        let repos = parse_gh_repos(&body).expect("array body parses");
        assert_eq!(repos.len(), 2);
        assert_eq!(repos[0].name, "fmeca");
        assert_eq!(repos[0].description, "FMECA MCP");
        assert_eq!(
            repos[0].topics,
            vec!["mcp".to_string(), "review".to_string()]
        );
        assert_eq!(repos[1].name, "no-desc");
        assert_eq!(repos[1].description, "");
        assert!(repos[1].topics.is_empty());
    }

    #[test]
    fn parse_gh_repos_skips_entries_missing_a_name() {
        let body = serde_json::json!([
            { "description": "no name here" },
            { "name": "ok" },
        ]);
        let repos = parse_gh_repos(&body).expect("array body parses");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].name, "ok");
    }

    #[test]
    fn parse_gh_repos_rejects_non_array_body() {
        // GitHub's error shape for a bad org, e.g. `{"message": "Not Found"}`.
        let body = serde_json::json!({ "message": "Not Found" });
        assert!(parse_gh_repos(&body).is_none());
    }
}
