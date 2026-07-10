//! Navigation facets for each mode, plus working-memory chunk boundaries.
//! The live mission **Tree** subsumes the old Plan + Flow facets (structure and
//! state are the same tree, static vs animated).

/// Run-mode facets.
///
/// COCKPIT-02 / poka-yoke — only facets with a real renderer are advertised.
/// The live mission **Status** view (the Tree) is the one implemented Run
/// dashboard; the Mission/Agents/Blackboard/Trace/Artifacts facets were
/// selectable but rendered a dead "— coming soon" placeholder, so the nav let
/// the user step onto targets that do nothing. They're removed until they
/// render. Re-add a label here only when its dashboard exists.
pub const RUN: [&str; 1] = ["Status"];

/// Build-mode facets (the library you configure).
pub const BUILD: [&str; 7] = [
    "Sources",
    "Flows",
    "Capabilities",
    "Skills",
    "Tools",
    "Connections",
    "Agents",
];

/// New-chunk start indices for Run nav. A single facet has no chunk breaks.
pub const RUN_CHUNKS: [usize; 0] = [];

/// New-chunk start indices for Build nav.
pub const BUILD_CHUNKS: [usize; 2] = [2, 5];

/// Index of the Tree (Status) facet within [`RUN`] — the Run home, and the
/// only Run facet with a real renderer (COCKPIT-02).
pub const TREE: usize = 0;

/// Facet count for a mode (Run and Build differ).
pub fn count(is_build: bool) -> usize {
    if is_build { BUILD.len() } else { RUN.len() }
}
