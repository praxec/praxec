//! Praxec TUI theme.
//!
//! Dark terminal palette — deep indigo/slate with amber accents.
//! Distinct from Aether's default and from FrontRails' navy scheme.

use crossterm::style::Color;
use tui::Theme;

/// Build the Praxec dark theme.
///
/// Dark indigo background with warm amber accents. High contrast for
/// readability in prolonged TUI sessions.
pub fn praxec_theme() -> Theme {
    // Brand palette
    const FG: Color = Color::Rgb {
        r: 0xE8,
        g: 0xE4,
        b: 0xDC,
    }; // warm off-white
    const BG: Color = Color::Rgb {
        r: 0x14,
        g: 0x15,
        b: 0x1A,
    }; // deep indigo-black
    const ACCENT: Color = Color::Rgb {
        r: 0xD4,
        g: 0xA3,
        b: 0x3A,
    }; // amber
    const GREEN: Color = Color::Rgb {
        r: 0x7A,
        g: 0xC4,
        b: 0x6E,
    };
    const RED: Color = Color::Rgb {
        r: 0xE8,
        g: 0x6C,
        b: 0x75,
    };
    const ORANGE: Color = Color::Rgb {
        r: 0xE8,
        g: 0xA8,
        b: 0x4C,
    };
    const MUTED: Color = Color::Rgb {
        r: 0x7A,
        g: 0x7C,
        b: 0x8A,
    };
    const SIDEBAR_BG: Color = Color::Rgb {
        r: 0x1A,
        g: 0x1C,
        b: 0x22,
    };

    Theme::builder()
        .fg(FG)
        .bg(BG)
        .accent(ACCENT)
        .highlight_bg(Color::Rgb {
            r: 0x28,
            g: 0x2A,
            b: 0x36,
        })
        .highlight_fg(FG)
        .text_secondary(MUTED)
        .code_fg(Color::Rgb {
            r: 0xB0,
            g: 0xD0,
            b: 0xB0,
        })
        .code_bg(Color::Rgb {
            r: 0x1A,
            g: 0x1E,
            b: 0x28,
        })
        .heading(ACCENT)
        .link(Color::Rgb {
            r: 0x8C,
            g: 0xBA,
            b: 0xDC,
        })
        .blockquote(Color::Rgb {
            r: 0xA0,
            g: 0x88,
            b: 0xCC,
        })
        .muted(MUTED)
        .success(GREEN)
        .warning(ORANGE)
        .error(RED)
        .info(ACCENT)
        .secondary(Color::Rgb {
            r: 0xA0,
            g: 0x88,
            b: 0xCC,
        })
        .sidebar_bg(SIDEBAR_BG)
        .diff_added_fg(GREEN)
        .diff_removed_fg(RED)
        .diff_added_bg(Color::Rgb {
            r: 0x1A,
            g: 0x30,
            b: 0x1A,
        })
        .diff_removed_bg(Color::Rgb {
            r: 0x30,
            g: 0x1A,
            b: 0x1A,
        })
        .build()
        .expect("Praxec theme has all required fields")
}
