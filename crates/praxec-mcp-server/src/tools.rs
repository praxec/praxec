//! Tool-list construction. SPEC §32 — the stable MCP surface is exactly two
//! tools: `praxec.query` and `praxec.command`. All operations are reached
//! by varying args, not the tool name.
//!
//! The optional skills/scripts search tools are no longer separate MCP tools;
//! they are gated-access paths within `praxec.query` (kind="skill" /
//! kind="script").

use std::borrow::Cow;

use praxec_core::discovery::DiscoveryKind;
use rmcp::model::Tool;

use crate::args::{schema_for_args, CommandArgs, QueryArgs};
use crate::{TOOL_COMMAND, TOOL_QUERY};

pub(crate) fn parse_kind(s: &str) -> Option<DiscoveryKind> {
    match s {
        "workflow" => Some(DiscoveryKind::Workflow),
        "capability" => Some(DiscoveryKind::Capability),
        "connection" => Some(DiscoveryKind::Connection),
        "agent" => Some(DiscoveryKind::Agent),
        _ => None,
    }
}

pub fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool::new(
            Cow::Borrowed(TOOL_QUERY),
            Cow::Borrowed(
                "SPEC §32 read tool. Dispatches by present-field shape: \
                 {} → home; query → search; subject → describe; \
                 workflowId → get; workflowId+transition → explain. \
                 Add kind='skill'|'script'|'lexicon' to scope search results.",
            ),
            schema_for_args::<QueryArgs>(&[]),
        ),
        Tool::new(
            Cow::Borrowed(TOOL_COMMAND),
            Cow::Borrowed(
                "SPEC §32 write tool. Dispatches by present-field shape: \
                 definitionId → start; workflowId+expectedVersion+transition → submit; \
                 subject='lexicon:<term>'+definition → define (requires lexicon writes enabled).",
            ),
            schema_for_args::<CommandArgs>(&[]),
        ),
    ]
}

pub(crate) fn instructions() -> &'static str {
    r#"This is the praxec gateway. SPEC §32 two-tool surface.

The tool surface is exactly two tools, stable across configs:
  praxec.query   — read: home, search, describe, get, explain
  praxec.command — write: start, submit, define

Dispatch by present-field shape:
  praxec.query {}                          → home (HATEOAS links)
  praxec.query { query }                   → search (add kind= to filter)
  praxec.query { subject }                 → describe
  praxec.query { workflowId }              → get
  praxec.query { workflowId, transition }  → explain

  praxec.command { definitionId }                                    → start
  praxec.command { workflowId, expectedVersion, transition }         → submit
  praxec.command { subject: "lexicon:<term>", definition: { definition_short: "..." } }  → define

Typical flow:
1. Call praxec.query {} to get the discovery home with HATEOAS links.
2. Call praxec.query { query: "..." } to find workflows or capabilities.
3. Follow a start link: praxec.command { definitionId: "...", input: {} }.
4. Read the workflow response's `links` array — each is a legal next transition.
5. Call praxec.command { workflowId, expectedVersion, transition, arguments }.
6. Stop when the mission resolves — result.status is 'succeeded' or 'failed'
   (a failure carries result.reason). While in process it is 'running' or
   'waiting'. When the mission declares `outcomes`, the response lists them with
   live `met` flags — the deterministic definition of done.

Invalid calls always return the current legal links so you can recover."#
}
