//! The Mission Control map surface — chrome (breadcrumb + zoom-ladder) over a
//! body that shows the Fleet (L0) terrain, a single Mission (L1) task-spine, or
//! the container-transform zoom between the two.
//!
//! The zoom is drawn as an aperture: the parent view fills the body (dimmed),
//! then the destination view is clipped into the interpolated `Transition::rect`
//! so it appears to grow out of (zoom-in) or shrink toward (zoom-out) the tile.

use crate::app::App;
use crate::map::transition::ZoomDir;
use crate::map::Level;
use crate::theme;
use crate::ui::{fleet_view, map_chrome::render_chrome, run_dashboard};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Clear};
use ratatui::Frame;

pub fn render_map(f: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(3)])
        .split(area);
    let chrome = rows[0];
    let body = rows[1];

    let mission_name = app.mission.as_ref().map(|m| m.name.as_str());
    render_chrome(f, chrome, app.map.level, mission_name);

    match app.map.transition {
        // A zoom is animating: draw the parent across the body, dim it, then
        // reveal the destination clipped into the aperture rect.
        Some(t) => {
            let (parent, dest) = match t.dir {
                ZoomDir::In => (RenderTarget::Fleet, RenderTarget::Mission),
                ZoomDir::Out => (RenderTarget::Mission, RenderTarget::Fleet),
            };
            draw_target(f, body, app, parent);
            f.render_widget(Block::default().style(theme::dim()), body);

            let aperture = clamp_to(t.rect(), body);
            f.render_widget(Clear, aperture);
            draw_target(f, aperture, app, dest);
        }
        // Settled: just the current altitude.
        None => match app.map.level {
            Level::Fleet => draw_target(f, body, app, RenderTarget::Fleet),
            Level::Mission => draw_target(f, body, app, RenderTarget::Mission),
        },
    }
}

#[derive(Clone, Copy)]
enum RenderTarget {
    Fleet,
    Mission,
}

fn draw_target(f: &mut Frame, area: Rect, app: &App, target: RenderTarget) {
    match target {
        RenderTarget::Fleet => fleet_view::render_fleet(f, area, &app.fleet, app.map.fleet_cursor),
        // A live workflow renders its real HATEOAS surface; otherwise the demo.
        RenderTarget::Mission => match &app.gateway {
            Some(resp) => crate::ui::mission::render_mission(f, area, resp, app.action_cursor),
            None => run_dashboard::render_run(f, area, app),
        },
    }
}

/// Keep an aperture inside the body so clipped destination rendering never
/// spills past the panel (the transition rect is derived from a fixed body
/// geometry that may be larger than the live area).
fn clamp_to(r: Rect, bounds: Rect) -> Rect {
    let x = r.x.max(bounds.x).min(bounds.right());
    let y = r.y.max(bounds.y).min(bounds.bottom());
    Rect {
        x,
        y,
        width: r.width.min(bounds.right().saturating_sub(x)),
        height: r.height.min(bounds.bottom().saturating_sub(y)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render_map_to_string(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_map(f, area, app);
            })
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn fleet_level_shows_the_fleet() {
        let app = App::new();
        let s = render_map_to_string(&app, 120, 28);
        assert!(s.contains("Provider catalog unification"));
    }

    #[test]
    fn mission_level_shows_the_task_spine() {
        let mut app = App::new();
        app.zoom_into_selected();
        let s = render_map_to_string(&app, 120, 28);
        assert!(s.contains("D4 · untangle delegate:"));
    }

    #[test]
    fn mid_transition_the_aperture_is_smaller_than_the_full_body() {
        let mut app = App::new();
        app.begin_zoom_into_selected();
        let r = app.map.transition.unwrap().rect();
        assert!(r.width < 120 || r.height < 23);
    }
}
