# ADR-0003: Mission Control view model — a curated view-state machine, driven by LLM and keystrokes

**Status:** Accepted

**Date:** 2026-06-09

## Context

Mission Control is a **chat-conducted cockpit**, not a chat-only surface and not
a fixed-layout TUI. The design question: how does a conversation drive what's on
screen *without* sacrificing the harness's legibility — the property
("I can trust what I see; the wrong move isn't on the menu") that the product
exists to sell?

The rejected extreme is open-ended generative UI (an LLM emitting arbitrary
widgets/layout): illegible, untestable, and off-thesis. This ADR records the
chosen model, which is *stricter* than "an LLM arranges individual widgets."

## Decision

The UI is a **curated view-state machine**: a finite set of predefined views,
switched by the LLM or by keystrokes, with every widget deterministic.

1. **Predefined view groups, not loose widgets.** The screen is always one of a
   **finite set of predefined views** (context-states). Each view is a *whole
   designed screen* — a fixed layout + a grouped set of glass widgets — authored
   as a unit, not composed widget-by-widget at runtime. (e.g. **Fleet** view,
   **Mission** view, **Detail/Ask** view, **Library** view.)

2. **The active view is a state machine.** A context-state selects the active
   view; navigation is transitions between views. This is the cockpit's core new
   abstraction.

3. **The LLM and keystrokes are co-equal drivers.** Either can transition the
   view-state — bidirectional parity. The chat can switch the view
   ("show what's blocking D4" → the Mission view focused on D4); keystrokes
   within the widgets do the same (select a mission → drill in). Neither path is
   privileged; both drive the same machine.

4. **Progressive disclosure is the spine.** Fleet (all missions, high level) →
   Mission (one mission's tasks/agents/steps) → Detail (a node / an ask). The
   view-state machine encodes these levels; the LLM *manages the disclosure*
   (jumps you to the right level on intent) and so do the keys.

5. **Every glass widget is deterministic — no LLM in any widget's read path.**
   Widgets render live runtime state directly (the status tree, glyphs/spinners,
   the typed HITL queue, the inspector). The LLM's only roles are: select/
   transition views, **narrate** the event stream, and **parse intent** into a
   governed command. It never generates widget content or layout.

6. **The NL command bar is select-only (first).** It parses intent into a *legal*
   `praxec.command` within the current legal-actions set (the visible
   `legal actions: N` leash stays pinned), per ADR-0001. Multi-step intent
   composition ("the bar may *plan*") is explicitly out of the first cut — the
   bar may *select*, not *plan*.

7. **Config/workflow authoring is deferred.** The §17 authoring write-path is
   not in the first cut; Mission Control reads the fleet and directs existing
   missions. Authoring/modifying workflows comes later.

## Consequences

- **Positive.** Legible (every view is a known, designed, testable screen — no
  surprise layouts); tractable (a finite view palette to build and snapshot-
  test); ergonomic (the LLM composes the right view so you don't hand-navigate,
  *and* keys still work). The novelty is in **composition/navigation**, while
  each widget and all execution stay boring and deterministic — harness
  legibility preserved.
- **Costs.** Design the view palette; build the view-state + transition model in
  the cockpit; bind the LLM intent ↔ view-state transitions; keep keystroke
  parity for every transition.
- **Fixes for free.** The current embedded-chat scroll bug disappears when the
  Detail view's transcript becomes a pinned-input/scrollable widget in the
  palette.

## Alternatives considered

- **Open-ended generative UI** (LLM emits arbitrary layout). Rejected —
  illegible, untestable, off-thesis.
- **LLM arranges individual widgets freely.** Rejected — the user chose stricter
  *predefined grouped views*; a finite palette is more legible and testable.
- **Fixed single layout, no chat composition.** Rejected — loses the ergonomic
  win (the chat managing progressive disclosure) that motivates the whole pivot.
- **Chat as the root surface (transcript-primary).** Rejected — a scrolling
  transcript is worse at supervising parallel governed work than a structured
  view; the substrate stays the root, the chat conducts it.

## References

- Cockpit: `crates/praxec-cockpit` (view-state machine lands here)
- Relates to: [ADR-0001](0001-headless-runtime-surfaces-attach.md),
  [ADR-0002](0002-fleet-runtime-multiplexed-mission-context.md)
- Design umbrella: `docs/architecture/mission-control-design.md`
