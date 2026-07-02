//! Rendering. `render` is the single entry point used by the event loop and
//! by render tests (via `ratatui::backend::TestBackend`).

pub mod chat;
pub mod chat_setup;
pub mod embedding_setup;
pub mod fleet_view;
pub mod inbox;
pub mod library;
pub mod map_chrome;
pub mod map_view;
pub mod mission;
pub mod priorities_setup;
pub mod run_dashboard;
pub mod settings;
pub mod shell;

use crate::app::App;
use ratatui::Frame;

/// Draw the whole cockpit for the current frame.
pub fn render(f: &mut Frame, app: &App) {
    shell::render_shell(f, app);
}

#[cfg(test)]
pub(crate) fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
    buf.content.iter().map(|c| c.symbol()).collect()
}
