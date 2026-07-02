//! The cockpit **operation surface** (ADR-0005).
//!
//! Every navigation the user can perform is a named [`CockpitOp`]. Keyboard
//! handlers produce these and call [`crate::app::App::apply`]; in Increment 3
//! the chat LLM produces the *same* ops via an in-process MCP server. So the
//! human (keys) and the LLM (MCP) drive **one identical surface** — the human
//! can see, do, and undo everything the LLM does (legible agency,
//! mixed-initiative).
//!
//! These are **navigation** ops — local view-state, ungoverned (looking
//! around). *Action* ops (act on a HITL ask, start/advance a mission) are added
//! later and route through the runtime's governed `praxec.command`.

use crate::map::fleet::Fleet;
use crate::map::Level;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitOp {
    /// Move the Fleet cursor by a signed delta (pan among missions).
    Pan(isize),
    /// Zoom into the mission at the given fleet index (container transform).
    ZoomInto(usize),
    /// Zoom out of the current mission back to the Fleet.
    ZoomOut,
    /// Quit the cockpit.
    Quit,
}

/// Parse a typed chat command into a navigation op + a one-line response. This
/// is the deterministic driver for Increment 2; the LLM replaces it in
/// Increment 3 (emitting the same ops via MCP). Matching is intentionally
/// forgiving — substring on mission name, a few intent words.
pub fn parse_command(text: &str, fleet: &Fleet, level: Level) -> (Option<CockpitOp>, String) {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return (None, String::new());
    }
    if t == "quit" || t == "exit" {
        return (Some(CockpitOp::Quit), "bye.".into());
    }
    if t == "back" || t == "out" || t == "fleet" || t == "zoom out" {
        return if level == Level::Mission {
            (Some(CockpitOp::ZoomOut), "← back to the Fleet.".into())
        } else {
            (None, "already at the Fleet.".into())
        };
    }
    if t.contains("need") {
        return match fleet.missions.iter().position(|m| m.pins.needs_you > 0) {
            Some(i) => (
                Some(CockpitOp::ZoomInto(i)),
                format!("→ {} (needs you).", fleet.missions[i].name),
            ),
            None => (None, "nothing needs you right now.".into()),
        };
    }
    // Mission-name substring match (strip a leading "zoom"/"go to"/"open").
    let needle = t
        .trim_start_matches("zoom into")
        .trim_start_matches("zoom")
        .trim_start_matches("go to")
        .trim_start_matches("open")
        .trim();
    let needle = if needle.is_empty() {
        t.as_str()
    } else {
        needle
    };
    match fleet
        .missions
        .iter()
        .position(|m| m.name.to_lowercase().contains(needle))
    {
        Some(i) => (
            Some(CockpitOp::ZoomInto(i)),
            format!("→ {}.", fleet.missions[i].name),
        ),
        None => (None, format!("don't know how to \"{}\" yet.", text.trim())),
    }
}

// ── the operation surface as LLM tools (ADR-0005 §2) ─────────────────────────
//
// The same `CockpitOp`s the keyboard drives are exposed to the chat LLM as
// named tools. The LLM selects one per turn; [`op_from_tool_call`] maps its
// choice back to a `CockpitOp` — the inverse of [`op_tools`]. This dispatch
// core is transport-agnostic: the in-process chat loop calls it directly, and a
// future stdio MCP server's `call_tool` can wrap the same mapper to let an
// external agent drive the cockpit (rmcp 1.7 has no in-process transport, so
// the in-process path is a direct call, not a real MCP round-trip).

/// One cockpit operation exposed as an LLM tool: a name, a one-line description,
/// and a JSON-schema for its arguments.
pub struct OpTool {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub const TOOL_ZOOM_INTO: &str = "zoom_into";
pub const TOOL_ZOOM_OUT: &str = "zoom_out";
pub const TOOL_PAN: &str = "pan";
pub const TOOL_QUIT: &str = "quit";

/// The navigation tools the chat LLM may call (one per turn). Mirrors the
/// keyboard's reach over the map.
pub fn op_tools() -> Vec<OpTool> {
    vec![
        OpTool {
            name: TOOL_ZOOM_INTO,
            description: "Zoom the map into a mission, found by a name fragment.",
            schema: json!({
                "type": "object",
                "properties": {
                    "mission": { "type": "string", "description": "A mission name or fragment." }
                },
                "required": ["mission"],
                "additionalProperties": false
            }),
        },
        OpTool {
            name: TOOL_ZOOM_OUT,
            description: "Zoom the map back out to the Fleet.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
        OpTool {
            name: TOOL_PAN,
            description: "Move the Fleet cursor by a signed number of tiles (negative is left/up).",
            schema: json!({
                "type": "object",
                "properties": { "delta": { "type": "integer", "description": "Tiles to move." } },
                "required": ["delta"],
                "additionalProperties": false
            }),
        },
        OpTool {
            name: TOOL_QUIT,
            description: "Quit the cockpit.",
            schema: json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        },
    ]
}

/// Map a tool call (name + parsed JSON args) to a `CockpitOp`, resolving a
/// mission name to its fleet index. `Err` carries a one-line reason to narrate
/// back. The single dispatch point both the in-process loop and a future MCP
/// server share.
pub fn op_from_tool_call(
    name: &str,
    args: &Value,
    fleet: &Fleet,
    level: Level,
) -> Result<CockpitOp, String> {
    match name {
        TOOL_ZOOM_INTO => {
            let needle = args
                .get("mission")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            if needle.is_empty() {
                return Err("zoom_into needs a mission name.".into());
            }
            match fleet
                .missions
                .iter()
                .position(|m| m.name.to_lowercase().contains(&needle))
            {
                Some(i) => Ok(CockpitOp::ZoomInto(i)),
                None => Err(format!("no mission matches \"{needle}\".")),
            }
        }
        TOOL_ZOOM_OUT => {
            if level == Level::Mission {
                Ok(CockpitOp::ZoomOut)
            } else {
                Err("already at the Fleet.".into())
            }
        }
        TOOL_PAN => {
            let delta = args
                .get("delta")
                .and_then(|v| v.as_i64())
                .ok_or("pan needs an integer delta.")?;
            Ok(CockpitOp::Pan(delta as isize))
        }
        TOOL_QUIT => Ok(CockpitOp::Quit),
        other => Err(format!("unknown tool \"{other}\".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::fleet::Fleet;

    #[test]
    fn a_mission_name_substring_zooms_into_it() {
        let f = Fleet::demo();
        let (op, _) = parse_command("postgres", &f, Level::Fleet);
        assert_eq!(op, Some(CockpitOp::ZoomInto(2)));
    }

    #[test]
    fn back_zooms_out_when_in_a_mission() {
        let (op, _) = parse_command("back", &Fleet::demo(), Level::Mission);
        assert_eq!(op, Some(CockpitOp::ZoomOut));
    }

    #[test]
    fn needs_me_goes_to_a_needs_you_mission() {
        let (op, _) = parse_command("what needs me", &Fleet::demo(), Level::Fleet);
        assert_eq!(op, Some(CockpitOp::ZoomInto(0)));
    }

    #[test]
    fn unknown_command_returns_no_op() {
        let (op, _) = parse_command("xyzzy", &Fleet::demo(), Level::Fleet);
        assert_eq!(op, None);
    }

    #[test]
    fn back_at_the_fleet_is_a_no_op_and_says_so() {
        let (op, msg) = parse_command("back", &Fleet::demo(), Level::Fleet);
        assert_eq!(op, None);
        assert!(msg.contains("already at the Fleet"));
    }

    // ── the LLM tool surface ─────────────────────────────────────────────────

    #[test]
    fn op_tools_expose_the_four_navigation_ops() {
        let names: Vec<_> = op_tools().iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![TOOL_ZOOM_INTO, TOOL_ZOOM_OUT, TOOL_PAN, TOOL_QUIT]
        );
    }

    #[test]
    fn tool_call_zoom_into_resolves_a_mission_name() {
        let op = op_from_tool_call(
            TOOL_ZOOM_INTO,
            &json!({ "mission": "postgres" }),
            &Fleet::demo(),
            Level::Fleet,
        );
        assert_eq!(op, Ok(CockpitOp::ZoomInto(2)));
    }

    #[test]
    fn tool_call_zoom_into_unknown_mission_errors() {
        let op = op_from_tool_call(
            TOOL_ZOOM_INTO,
            &json!({ "mission": "nope" }),
            &Fleet::demo(),
            Level::Fleet,
        );
        assert!(op.is_err());
    }

    #[test]
    fn tool_call_pan_reads_the_delta() {
        let op = op_from_tool_call(
            TOOL_PAN,
            &json!({ "delta": -2 }),
            &Fleet::demo(),
            Level::Fleet,
        );
        assert_eq!(op, Ok(CockpitOp::Pan(-2)));
    }

    #[test]
    fn tool_call_zoom_out_at_the_fleet_errors() {
        let op = op_from_tool_call(TOOL_ZOOM_OUT, &json!({}), &Fleet::demo(), Level::Fleet);
        assert!(op.is_err());
    }

    #[test]
    fn tool_call_unknown_name_errors() {
        let op = op_from_tool_call("frobnicate", &json!({}), &Fleet::demo(), Level::Fleet);
        assert!(op.is_err());
    }
}
