//! Container-transform geometry: a destination view is revealed through an
//! aperture (a `Rect`) that interpolates from the selected tile to the full
//! viewport. No text scaling — only the rectangle animates (ADR-0004 §4).

use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomDir {
    In,
    Out,
}

/// An in-flight zoom. `progress` runs 0.0 → 1.0 on the draw loop.
#[derive(Debug, Clone, Copy)]
pub struct Transition {
    pub dir: ZoomDir,
    pub from: Rect,
    pub to: Rect,
    pub progress: f32,
}

impl Transition {
    /// The current aperture rectangle (eased).
    pub fn rect(&self) -> Rect {
        lerp_rect(self.from, self.to, ease_out_cubic(self.progress))
    }
}

/// Decelerating ease — fast out of the gate, settling at the end.
pub fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let u = 1.0 - t;
    1.0 - u * u * u
}

/// Linear interpolation of a rectangle, rounded to whole cells.
pub fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u16, y: u16| -> u16 { (x as f32 + (y as f32 - x as f32) * t).round() as u16 };
    Rect {
        x: lerp(a.x, b.x),
        y: lerp(a.y, b.y),
        width: lerp(a.width, b.width),
        height: lerp(a.height, b.height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ease_out_is_clamped_at_the_ends() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
    }

    #[test]
    fn ease_out_is_ahead_of_linear_in_the_middle() {
        assert!(ease_out_cubic(0.5) > 0.5); // decelerating → past halfway at t=0.5
    }

    #[test]
    fn lerp_rect_at_zero_is_the_source() {
        let a = Rect::new(2, 3, 10, 4);
        let b = Rect::new(0, 0, 80, 24);
        assert_eq!(lerp_rect(a, b, 0.0), a);
    }

    #[test]
    fn lerp_rect_at_one_is_the_destination() {
        let a = Rect::new(2, 3, 10, 4);
        let b = Rect::new(0, 0, 80, 24);
        assert_eq!(lerp_rect(a, b, 1.0), b);
    }

    #[test]
    fn transition_rect_at_zero_progress_sits_on_the_source_tile() {
        let t = Transition {
            dir: ZoomDir::In,
            from: Rect::new(5, 5, 12, 5),
            to: Rect::new(0, 0, 80, 24),
            progress: 0.0,
        };
        assert_eq!(t.rect(), Rect::new(5, 5, 12, 5));
    }

    #[test]
    fn transition_rect_at_full_progress_reaches_the_destination() {
        let t = Transition {
            dir: ZoomDir::In,
            from: Rect::new(5, 5, 12, 5),
            to: Rect::new(0, 0, 80, 24),
            progress: 1.0,
        };
        assert_eq!(t.rect(), Rect::new(0, 0, 80, 24));
    }
}
