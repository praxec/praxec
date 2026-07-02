//! The Mission Control map view-state machine (ADR-0004). Increment 1 carries
//! two altitudes — Fleet (L0) and Mission (L1) — and the container-transform
//! zoom between them. Pure state; rendering reads it.

pub mod fleet;
pub mod transition;

use ratatui::layout::Rect;
use transition::{Transition, ZoomDir};

/// One transition lasts ~180ms; the draw loop advances `progress` by
/// `dt = frame_dt / TRANSITION_SECS` each frame.
pub const TRANSITION_SECS: f32 = 0.18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Fleet,
    Mission,
}

pub struct MapState {
    pub level: Level,
    /// Selected mission tile at the Fleet level (spatial memory: stable order).
    pub fleet_cursor: usize,
    /// Which mission we are zoomed into, if any.
    pub mission: Option<usize>,
    pub transition: Option<Transition>,
}

impl MapState {
    pub fn new() -> Self {
        Self {
            level: Level::Fleet,
            fleet_cursor: 0,
            mission: None,
            transition: None,
        }
    }

    pub fn is_transitioning(&self) -> bool {
        self.transition.is_some()
    }

    /// Move the fleet cursor by `delta`, clamped to `[0, len)`.
    pub fn pan(&mut self, delta: isize, len: usize) {
        if len == 0 {
            return;
        }
        let max = (len - 1) as isize;
        let next = (self.fleet_cursor as isize + delta).clamp(0, max);
        self.fleet_cursor = next as usize;
    }

    /// Fleet → Mission. `from` is the selected tile's rect, `to` the body rect;
    /// the destination grows from the tile (container transform).
    pub fn zoom_in(&mut self, from: Rect, to: Rect) {
        if self.level != Level::Fleet || self.is_transitioning() {
            return;
        }
        self.level = Level::Mission;
        self.mission = Some(self.fleet_cursor);
        self.transition = Some(Transition {
            dir: ZoomDir::In,
            from,
            to,
            progress: 0.0,
        });
    }

    /// Mission → Fleet. The view shrinks back toward the tile's spot.
    pub fn zoom_out(&mut self, tile: Rect, body: Rect) {
        if self.level != Level::Mission || self.is_transitioning() {
            return;
        }
        self.level = Level::Fleet;
        self.transition = Some(Transition {
            dir: ZoomDir::Out,
            from: tile,
            to: body,
            progress: 0.0,
        });
    }

    /// Advance an in-flight transition; settle (clear) it at completion.
    pub fn tick_transition(&mut self, dt: f32) {
        if let Some(t) = self.transition.as_mut() {
            t.progress += dt / TRANSITION_SECS;
            if t.progress >= 1.0 {
                self.transition = None;
            }
        }
    }
}

impl Default for MapState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn tile() -> Rect {
        Rect::new(4, 4, 12, 5)
    }
    fn body() -> Rect {
        Rect::new(0, 2, 80, 22)
    }

    #[test]
    fn starts_at_the_fleet_level() {
        assert_eq!(MapState::new().level, Level::Fleet);
    }

    #[test]
    fn pan_moves_the_fleet_cursor_within_bounds() {
        let mut m = MapState::new();
        m.pan(1, 4);
        assert_eq!(m.fleet_cursor, 1);
        m.pan(-5, 4);
        assert_eq!(m.fleet_cursor, 0); // clamped
    }

    #[test]
    fn zoom_in_enters_the_selected_mission_and_starts_a_transition() {
        let mut m = MapState::new();
        m.fleet_cursor = 2;
        m.zoom_in(tile(), body());
        assert_eq!(m.level, Level::Mission);
        assert_eq!(m.mission, Some(2));
        assert!(m.is_transitioning());
    }

    #[test]
    fn ticking_a_transition_to_completion_settles_it() {
        let mut m = MapState::new();
        m.zoom_in(tile(), body());
        for _ in 0..100 {
            m.tick_transition(0.05);
        }
        assert!(!m.is_transitioning());
    }

    #[test]
    fn zoom_out_returns_to_the_fleet() {
        let mut m = MapState::new();
        m.zoom_in(tile(), body());
        for _ in 0..100 {
            m.tick_transition(0.05);
        }
        m.zoom_out(tile(), body());
        for _ in 0..100 {
            m.tick_transition(0.05);
        }
        assert_eq!(m.level, Level::Fleet);
    }
}
