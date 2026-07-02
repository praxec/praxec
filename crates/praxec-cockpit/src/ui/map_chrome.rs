//! Persistent map chrome: the you-are-here breadcrumb + the zoom-ladder.

use crate::map::Level;
use crate::theme;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// The breadcrumb text. `Some(name)` = zoomed into that mission.
pub fn chrome_line(mission: Option<&str>) -> String {
    match mission {
        Some(name) => format!("Fleet ▸ {name}"),
        None => "Fleet".to_string(),
    }
}

fn ladder(level: Level) -> Line<'static> {
    let rung = |label: &'static str, lit: bool| {
        Span::styled(
            format!("{} {label}  ", if lit { "◉" } else { "○" }),
            if lit { theme::accent() } else { theme::dim() },
        )
    };
    Line::from(vec![
        rung("Fleet", level == Level::Fleet),
        rung("Mission", level == Level::Mission),
        rung("Task", false),
        rung("Detail", false),
    ])
}

pub fn render_chrome(f: &mut Frame, area: Rect, level: Level, mission: Option<&str>) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(44)])
        .split(area);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" {}", chrome_line(mission)),
            theme::value(),
        )),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(ladder(level)).alignment(Alignment::Right),
        cols[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breadcrumb_shows_the_zoomed_mission_name() {
        let s = chrome_line(Some("Complete alignment + caching"));
        assert!(s.contains("Fleet"));
        assert!(s.contains("Complete alignment + caching"));
    }

    #[test]
    fn fleet_level_breadcrumb_is_just_fleet() {
        assert!(!chrome_line(None).contains("▸"));
    }
}
