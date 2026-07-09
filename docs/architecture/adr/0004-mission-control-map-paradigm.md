# ADR-0004: Mission Control is a semantic map (zoom/pan/search), with literal-map readability

**Status:** Accepted

**Date:** 2026-06-09

## Context

[ADR-0003](0003-mission-control-view-model.md) fixed the UI as a curated
view-state machine. This ADR fixes the **traversal paradigm** for that machine —
how a human (and the chat) move through it — and the readability bar it must
meet.

The decided paradigm is a **map**: a single deterministic spatial substrate —
your work as terrain — traversed by semantic zoom and pan, navigable identically
by hand or by chat (the chat is the map's search box). This leans on innate
spatial-navigation machinery (cognitive maps) and the universal Google-Maps
schema, so the mental model forms with *no instruction*.

Two constraints were settled in discussion:
1. **Semantic, not literal.** It is structured terminal layouts, not a pannable
   pixel canvas — but it must read *as instantly* as a literal map would.
2. **Zoom must work in a terminal.** A terminal cannot scale text, so literal
   pinch-zoom does not translate. The resolution (below) is the **container
   transform**, not scaling.

## Decision

**Mission Control is a semantic zoomable map.** The view-state machine's levels
are the map's zoom altitudes, traversed by zoom + pan + search.

1. **Altitudes (zoom levels) = the IA.** Fleet (L0, all missions) → Mission (L1,
   one mission's CPM plan) → Task (L2, a deliverable's agent+steps) → Detail (L3,
   a step / a HITL ask). Zoom-in descends; zoom-out ascends; pan moves among
   siblings; search teleports.

2. **Five always-on readability affordances** carry literal-map intuitiveness
   into the terminal. Each is mandatory; dropping them degrades the map to a
   tree-with-modes:
   - **You-are-here breadcrumb + a zoom-ladder** (the scale bar): always shows
     the current altitude and location, and that you can go up/down.
   - **A persistent minimap** of the parent/whole with the current region
     highlighted (focus+context / fisheye) — you never lose the whole.
   - **Object-constant zoom transitions** (see §4) — you see you went *into* a
     thing, never a teleport.
   - **Stable terrain**: fixed positions/ordering per level; colour = state;
     preattentive attention pins (needs-you ◆ / blocked ⏸ / failed ✗),
     aggregated upward.
   - **Roads**: at L1, dependency edges + the critical-path spine drawn as
     connectors, so relationships are spatial.

3. **Dual, co-equal navigation.** Every zoom/pan/search transition is driven
   *identically* by keystrokes and by the chat (search box + turn-by-turn
   narrator), per ADR-0003. Neither path is privileged.

4. **Zoom mechanism = container transform (shared-element transition), NOT
   pinch-zoom.** Text is never scaled. The destination level's view is revealed
   through an **aperture** — a `Rect` that interpolates from the selected tile's
   rectangle to the full viewport over ~180 ms, eased, driven by the existing
   frame/tick loop; the parent dims behind it. The destination content is always
   at final resolution and *emerges from the source location*; that spatial
   continuity is what delivers object constancy. Zoom-out reverses (the view
   shrinks back toward the tile's spot). A **reduced-motion / slow-terminal
   fallback** collapses the tween to instant while staying object-constant (the
   destination still appears at the source location).

5. **Acceptance test (the "no instruction" bar, made testable).** A first-time
   user, given zero instruction, reads any screen and within seconds knows:
   *"This is a map of my work. I'm here. That's the whole. Those pins need me. I
   can zoom into anything — or type where I want to go."* A screen that fails
   this read-aloud test is not done.

## Consequences

- **Positive.** Instruction-free via the borrowed map schema; spatial memory
  from stable terrain; never-lost via object-constant transitions + the minimap;
  fleet-scale attention via preattentive pins; recognition over recall. And the
  map stays **deterministic** (positions and state are real, no LLM in any read
  path) — trustworthy the way the harness demands; the chat only flies and
  narrates.
- **Costs.** The view-state machine must carry the five affordances (breadcrumb/
  zoom-ladder, minimap, the container-transform animation on the frame loop,
  stable-layout engine, road connectors). The transition runs the draw loop at
  ~60 fps for ~180 ms per move.

## Alternatives considered

- **Literal pinch-zoom (scale everything).** Impossible — a terminal is a fixed
  character grid; text cannot scale.
- **Literal 2-D pannable pixel canvas.** Rejected for the terminal (hard,
  gimmick-prone); it is the natural home of a future *graphical* surface, not
  this one.
- **Discrete mode-switch views with no map affordances.** Rejected — degrades to
  a tree-with-modes and fails the readability bar; the affordances in §2 are what
  make it a map.

## Status update (2026-07)

Of the five "mandatory" readability affordances in Decision §2, **three are built**
and **two are not yet**:

- **Built:** you-are-here breadcrumb + zoom-ladder
  (`crates/praxec-cockpit/src/ui/map_chrome.rs`), object-constant zoom transitions
  (`src/map/transition.rs`), and stable terrain (fixed layout + state colour + pins).
- **Not yet built:** the **persistent minimap** (focus+context view of the
  parent/whole) and the L1 **roads** (dependency edges + critical-path spine drawn
  as connectors). Neither has an implementation under `crates/praxec-cockpit/src/`.

So the map currently satisfies the "no instruction" read for altitude/location and
object constancy, but not yet the never-lose-the-whole (minimap) and
spatial-relationships (roads) affordances.

## References

- Cockpit (the map lands here): `crates/praxec-cockpit`
- Relates to: [ADR-0001](0001-headless-runtime-surfaces-attach.md),
  [ADR-0002](0002-fleet-runtime-multiplexed-mission-context.md),
  [ADR-0003](0003-mission-control-view-model.md)
- Design umbrella: `docs/architecture/mission-control-design.md`
