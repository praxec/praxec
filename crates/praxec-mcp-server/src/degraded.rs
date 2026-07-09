//! Degraded serve — misconfiguration as a first-class, live, self-healing state.
//!
//! A configuration fault (parse error, a validation lint like
//! `SLOT_KEY_ENGINE_OWNED`, a non-durable store) used to abort `serve` **before**
//! the MCP transport came up, so the operator's client saw an opaque transport
//! `-32000` with no diagnosis. Instead, `serve` now brings up a [`DegradedServer`]
//! that completes the MCP handshake and answers **every** call with a precise,
//! machine-actionable [`HealthReport`] — the exact fault, where it is, and how to
//! fix it — so an LLM operator can self-heal (typically by running the declarative
//! `meta/flow.repair-workflow-health` workflow) and then reconnect.
//!
//! This is deliberately NOT a fallback: the degraded server does **zero**
//! governance work. It refuses everything, loudly and precisely. The repair
//! intelligence is declarative (praxec-meta); this module is only the minimal
//! engine primitive: stay alive, report exactly, point at the fix.

use rmcp::ErrorData as McpError;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, InitializeResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Value, json};

/// One concrete remediation step: what to do and exactly how.
#[derive(Clone, Debug)]
pub struct Remedy {
    pub what: String,
    pub how: String,
}

/// A self-documenting description of why the gateway is degraded and how to fix
/// it — rendered on every degraded call (as the error message + structured
/// `data`) so both an LLM and a human get an actionable diagnosis.
#[derive(Clone, Debug)]
pub struct HealthReport {
    /// Machine-stable code parsed from the fault (e.g. `SLOT_KEY_ENGINE_OWNED`),
    /// or `CONFIG_INVALID` when the fault carries no coded prefix.
    pub code: String,
    /// The first line of the fault — the one-line "what's wrong".
    pub summary: String,
    /// The full error chain (`{err:#}`), for the complete picture.
    pub detail: String,
    /// Best-effort `workflow '…' state '…' transition '…'` locus, when present.
    pub location: Option<String>,
    /// The config file the degraded gateway was launched against.
    pub config_path: String,
    /// Ordered, concrete steps to resolve and resume.
    pub remedies: Vec<Remedy>,
    /// How the fix takes effect (reconnect semantics).
    pub reload: String,
}

impl HealthReport {
    /// Build a report from a config-load/validation fault and the config path.
    ///
    /// Remedies are intentionally GENERIC for now (per the agreed rollout): the
    /// full diagnostic via `praxec check`, the declarative repair workflow, and
    /// the reconnect-to-resume path. Per-code remedy detail can be layered on
    /// later without changing this shape.
    pub fn from_config_error(err: &anyhow::Error, config_path: &str) -> Self {
        let detail = format!("{err:#}");
        let summary = detail.lines().next().unwrap_or(&detail).trim().to_string();
        let code = extract_code(&summary).unwrap_or_else(|| "CONFIG_INVALID".to_string());
        let location = extract_location(&summary);
        let remedies = vec![
            Remedy {
                what: "See the full diagnostic".to_string(),
                how: format!("praxec check --config {config_path}"),
            },
            Remedy {
                what: "Repair the configuration".to_string(),
                how: "run the declarative meta workflow `meta/flow.repair-workflow-health` \
                      — it reads this report, fixes the offending workflow, and re-runs \
                      `praxec check` to confirm"
                    .to_string(),
            },
            Remedy {
                what: "Resume governed operation".to_string(),
                how: "once `praxec check` reports 0 errors, reconnect the MCP server; a fresh \
                      process loads the corrected config and comes up healthy"
                    .to_string(),
            },
        ];
        Self {
            code,
            summary,
            detail,
            location,
            config_path: config_path.to_string(),
            remedies,
            reload: "Fix the config (or run `meta/flow.repair-workflow-health`), then reconnect \
                     the MCP server — a fresh process loads the corrected config."
                .to_string(),
        }
    }

    /// The structured payload — attached as the MCP error `data` so a caller can
    /// act on the fields programmatically.
    pub fn to_json(&self) -> Value {
        json!({
            "status": "degraded",
            "code": self.code,
            "summary": self.summary,
            "detail": self.detail,
            "location": self.location,
            "config_path": self.config_path,
            "remedies": self.remedies.iter().map(|r| json!({ "what": r.what, "how": r.how })).collect::<Vec<_>>(),
            "reload": self.reload,
        })
    }

    /// The full, human- and LLM-readable message rendered on every degraded call.
    pub fn to_message(&self) -> String {
        let mut m = String::new();
        m.push_str(
            "praxec gateway is DEGRADED — the configuration is invalid, so NO governed work \
             will run until it is fixed.\n\n",
        );
        m.push_str(&format!("  code:     {}\n", self.code));
        if let Some(loc) = &self.location {
            m.push_str(&format!("  where:    {loc}\n"));
        }
        m.push_str(&format!("  problem:  {}\n", self.summary));
        m.push_str(&format!("  config:   {}\n\n", self.config_path));
        m.push_str("  detail:\n");
        for line in self.detail.lines() {
            m.push_str(&format!("    {line}\n"));
        }
        m.push_str("\n  how to fix:\n");
        for (i, r) in self.remedies.iter().enumerate() {
            m.push_str(&format!("    {}. {} — {}\n", i + 1, r.what, r.how));
        }
        m.push_str(&format!("\n  resume: {}\n", self.reload));
        m
    }

    /// The report as an MCP error: rich message + structured `data`. Returned
    /// from every degraded call so the fault is impossible to miss or misread.
    pub fn as_mcp_error(&self) -> McpError {
        McpError::invalid_params(self.to_message(), Some(self.to_json()))
    }
}

/// Extract the first `SCREAMING_SNAKE` diagnostic code that is immediately
/// followed by `:` — anywhere in the message, so a wrapped fault like
/// `loading config <path>: SLOT_KEY_ENGINE_OWNED: …` still yields the real code
/// the meta repair workflow keys on, not the outer `loading config` prose.
fn extract_code(summary: &str) -> Option<String> {
    let segs: Vec<&str> = summary.split(':').collect();
    // Only segments FOLLOWED by a colon are candidates (skip the trailing one).
    for seg in &segs[..segs.len().saturating_sub(1)] {
        // The code is the last whitespace-delimited token before the colon.
        let cand = seg.split_whitespace().next_back().unwrap_or("");
        let is_code = cand.len() >= 3
            && cand.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && cand
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if is_code {
            return Some(cand.to_string());
        }
    }
    None
}

/// Best-effort `workflow '…' state '…' transition '…'` locus from the message.
fn extract_location(summary: &str) -> Option<String> {
    let start = summary.find("workflow '")?;
    let tail = &summary[start..];
    // Cut at the first backtick (start of the remedy prose in our messages) or a
    // reasonable window, then trim trailing whitespace + a single trailing colon
    // — WITHOUT stripping the transition's closing quote.
    let end = tail.find('`').unwrap_or_else(|| tail.len().min(160));
    Some(
        tail[..end]
            .trim_end()
            .trim_end_matches(':')
            .trim_end()
            .to_string(),
    )
}

/// The minimal MCP server served in place of the real gateway when config load
/// fails. It advertises the normal tool surface (so a client's ordinary call
/// reaches the handler) and answers every call with the [`HealthReport`].
pub struct DegradedServer {
    report: HealthReport,
}

impl DegradedServer {
    pub fn new(report: HealthReport) -> Self {
        Self { report }
    }
}

impl ServerHandler for DegradedServer {
    fn get_info(&self) -> ServerInfo {
        let mut server_info =
            Implementation::new("praxec".to_string(), env!("CARGO_PKG_VERSION").to_string());
        server_info.title = Some("praxec (degraded)".to_string());
        server_info.description = Some(
            "praxec gateway — DEGRADED: configuration invalid. Every call returns a health \
             report describing the fault and how to fix it."
                .to_string(),
        );
        let mut info = InitializeResult::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = server_info;
        info.instructions = Some(self.report.to_message());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // Advertise the normal two-tool surface so a client's ordinary
        // `praxec.query` / `praxec.command` call reaches this handler and gets
        // the health report, rather than "unknown tool".
        Ok(ListToolsResult::with_all_items(
            crate::tools::tool_definitions(),
        ))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        // Every call, whatever it is, reflects the health issue + the fix.
        Err(self.report.as_mcp_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(msg: &str) -> HealthReport {
        HealthReport::from_config_error(&anyhow::anyhow!("{msg}"), "/etc/praxec/gateway.yaml")
    }

    #[test]
    fn parses_coded_fault_with_location() {
        // The real fault is WRAPPED by anyhow context: `loading config <path>:
        // SLOT_KEY_ENGINE_OWNED: …` — the parser must still surface the inner code.
        let r = report(
            "loading config /home/mc/.config/praxec/gateway.yaml: SLOT_KEY_ENGINE_OWNED: \
             workflow 'cognitive-max/cap.verify.ts' state 'ready' transition 'run': \
             `output:` writes the engine-owned slot key",
        );
        assert_eq!(r.code, "SLOT_KEY_ENGINE_OWNED");
        let loc = r
            .location
            .as_deref()
            .expect("a workflow/state/transition locus");
        assert!(
            loc.contains("cap.verify.ts") && loc.contains("state 'ready'"),
            "{loc}"
        );
        // The transition's closing quote is preserved (not stripped by trimming).
        assert!(
            loc.ends_with("transition 'run'"),
            "closing quote preserved: {loc}"
        );
        // The message is fully self-documenting: code, where, and all remedies.
        let m = r.to_message();
        assert!(m.contains("SLOT_KEY_ENGINE_OWNED"));
        assert!(m.contains("meta/flow.repair-workflow-health"));
        assert!(m.contains("reconnect"));
        // The structured data carries the same, for programmatic self-heal.
        let j = r.to_json();
        assert_eq!(j["code"], "SLOT_KEY_ENGINE_OWNED");
        assert_eq!(j["status"], "degraded");
        assert_eq!(j["remedies"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn uncoded_fault_gets_generic_code_and_no_false_location() {
        let r = report("failed to parse config: unexpected character at line 4");
        assert_eq!(r.code, "CONFIG_INVALID");
        assert!(r.location.is_none());
        assert!(r.to_message().contains("CONFIG_INVALID"));
    }
}
