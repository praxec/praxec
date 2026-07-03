//! Invariant 9 from INIT-1 §17 (updated to SPEC §32):
//! "MCP-facing tools remain stable."
//!
//! The §32 redesign collapses the 10-tool surface to exactly two tools:
//! `praxec.query` and `praxec.command`. The invariant is preserved:
//! the surface is stable across configs — all workflow and discovery
//! operations are reached by varying args, not the tool name.

use praxec_mcp_server::{STABLE_TOOL_NAMES, tool_definitions};

#[test]
fn tool_list_matches_stable_names_exactly() {
    let tools = tool_definitions();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, STABLE_TOOL_NAMES);
}

#[test]
fn stable_tool_names_are_the_documented_two() {
    assert_eq!(
        STABLE_TOOL_NAMES,
        &[
            // SPEC §32 — two-tool surface.
            "praxec.query",
            "praxec.command",
        ]
    );
}

#[test]
fn every_tool_has_an_input_schema() {
    for tool in tool_definitions() {
        assert!(
            !tool.input_schema.is_empty(),
            "tool '{}' missing inputSchema",
            tool.name
        );
        assert_eq!(
            tool.input_schema.get("type").and_then(|v| v.as_str()),
            Some("object"),
            "tool '{}' inputSchema must be type=object",
            tool.name
        );
    }
}
