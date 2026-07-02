//! Headless render-to-text, so the UI can be iterated on without a live
//! terminal: render the cockpit into a fixed-size buffer and dump the glyphs
//! row by row. Drives `praxec-cockpit --snapshot`.

use crate::app::App;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// Render `app` at `width`×`height` and return the screen as newline-separated
/// rows (glyphs only — styling is not encoded in text output).
pub fn render_to_text(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
    let buf = terminal.backend().buffer();

    let mut out = String::with_capacity((width as usize + 1) * height as usize);
    for y in 0..height {
        for x in 0..width {
            let idx = (y as usize) * (width as usize) + (x as usize);
            out.push_str(buf.content[idx].symbol());
        }
        out.push('\n');
    }
    out
}
