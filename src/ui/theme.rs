//! Color palette for the diff view, separate from syntect's syntax colors.

use ratatui::style::Color;

use crate::config::Mode;

/// Background/accent colors used to paint diff lines.
#[derive(Debug, Clone, Copy)]
pub struct DiffPalette {
    /// Subtle background for inserted lines.
    pub add_bg: Color,
    /// Subtle background for removed lines.
    pub del_bg: Color,
    /// Background for context lines (usually terminal default).
    pub ctx_bg: Color,
    /// Stronger background for the changed span within an inserted line.
    pub add_emph_bg: Color,
    /// Stronger background for the changed span within a removed line.
    pub del_emph_bg: Color,
    /// Foreground for the line-number gutter.
    pub gutter_fg: Color,
    /// Foreground for hunk headers (`@@ ... @@`).
    pub hunk_fg: Color,
}

impl DiffPalette {
    pub fn for_mode(mode: Mode) -> Self {
        match mode {
            Mode::Dark => DiffPalette {
                add_bg: Color::Rgb(18, 38, 22),
                del_bg: Color::Rgb(44, 20, 22),
                ctx_bg: Color::Reset,
                add_emph_bg: Color::Rgb(30, 84, 40),
                del_emph_bg: Color::Rgb(102, 36, 38),
                gutter_fg: Color::Rgb(110, 110, 122),
                hunk_fg: Color::Rgb(126, 148, 210),
            },
            Mode::Light => DiffPalette {
                add_bg: Color::Rgb(220, 242, 222),
                del_bg: Color::Rgb(250, 222, 222),
                ctx_bg: Color::Reset,
                add_emph_bg: Color::Rgb(160, 222, 168),
                del_emph_bg: Color::Rgb(244, 178, 178),
                gutter_fg: Color::Rgb(140, 140, 150),
                hunk_fg: Color::Rgb(60, 90, 170),
            },
        }
    }
}

/// Color for a change-kind glyph in the tree.
pub fn kind_color(kind: crate::core::model::ChangeKind) -> Color {
    use crate::core::model::ChangeKind::*;
    match kind {
        Added => Color::Green,
        Modified => Color::Yellow,
        Deleted => Color::Red,
    }
}
