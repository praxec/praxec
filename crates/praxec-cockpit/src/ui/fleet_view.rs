//! The Fleet (L0) renderer — every mission as a stable tile of terrain.

use crate::map::fleet::{Fleet, Health, Mission};
use crate::theme;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use ratatui::Frame;

const TILE_W: u16 = 34;
const TILE_H: u16 = 5;
const GAP: u16 = 1;

/// Pure tile geometry: a stable left-to-right, top-to-bottom grid. Stable order
/// is what gives the fleet spatial memory.
pub fn tile_rects(area: Rect, n: usize) -> Vec<Rect> {
    let cols = ((area.width + GAP) / (TILE_W + GAP)).max(1);
    (0..n as u16)
        .map(|i| {
            let (r, c) = (i / cols, i % cols);
            Rect {
                x: area.x + c * (TILE_W + GAP),
                y: area.y + r * (TILE_H + GAP),
                width: TILE_W.min(area.width.saturating_sub(c * (TILE_W + GAP))),
                height: TILE_H,
            }
        })
        .collect()
}

pub fn selected_tile_rect(area: Rect, n: usize, idx: usize) -> Rect {
    tile_rects(area, n).get(idx).copied().unwrap_or(area)
}

fn health_style(h: Health) -> Style {
    match h {
        Health::Running => theme::state_running(),
        Health::NeedsYou => theme::state_needs(),
        Health::Blocked => theme::state_blocked(),
        Health::Failed => theme::state_failed(),
        Health::Done => theme::state_done(),
    }
}

pub fn render_fleet(f: &mut Frame, area: Rect, fleet: &Fleet, cursor: usize) {
    let rects = tile_rects(area, fleet.missions.len());
    for (i, (m, r)) in fleet.missions.iter().zip(rects).enumerate() {
        render_tile(f, r, m, i == cursor);
    }
}

fn render_tile(f: &mut Frame, area: Rect, m: &Mission, selected: bool) {
    let border = if selected {
        theme::selected()
    } else {
        theme::border()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(" ● ", health_style(m.health)))
        .padding(Padding::new(1, 1, 0, 0));
    let mut lines = vec![Line::from(Span::styled(
        m.name.clone(),
        if selected {
            theme::selected()
        } else {
            theme::value()
        },
    ))];
    let mut pins = Vec::new();
    if m.pins.needs_you > 0 {
        pins.push(Span::styled(
            format!("◆{} ", m.pins.needs_you),
            theme::state_needs(),
        ));
    }
    if m.pins.blocked > 0 {
        pins.push(Span::styled(
            format!("⏸{} ", m.pins.blocked),
            theme::state_blocked(),
        ));
    }
    if pins.is_empty() {
        pins.push(Span::styled(m.orchestrator.clone(), theme::dim()));
    }
    lines.push(Line::from(pins));
    f.render_widget(Paragraph::new(Text::from(lines)).block(block), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::fleet::Fleet;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;
    use ratatui::Terminal;

    fn render_fleet_to_string(fleet: &Fleet, cursor: usize, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_fleet(f, area, fleet, cursor);
            })
            .unwrap();
        crate::ui::buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn tile_rects_are_one_per_mission_and_inside_the_area() {
        let area = Rect::new(0, 0, 80, 24);
        let rects = tile_rects(area, 4);
        assert_eq!(rects.len(), 4);
        assert!(rects
            .iter()
            .all(|r| r.right() <= area.right() && r.bottom() <= area.bottom()));
    }

    #[test]
    fn selected_tile_rect_is_stable_for_a_given_index() {
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(tile_rects(area, 4)[2], selected_tile_rect(area, 4, 2));
    }

    #[test]
    fn fleet_renders_every_mission_name_and_a_needs_you_pin() {
        let s = render_fleet_to_string(&Fleet::demo(), 0, 100, 24);
        assert!(s.contains("Complete alignment + caching"));
        assert!(s.contains("Provider catalog unification"));
        assert!(s.contains("◆")); // a pin
    }
}
