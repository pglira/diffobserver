//! Syntax highlighting via syntect, mapped to ratatui colors.

use std::path::Path;

use ratatui::style::Color;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Owns syntect's syntax/theme data and highlights file contents.
pub struct Highlighter {
    syntaxes: SyntaxSet,
    theme: Theme,
    enabled: bool,
}

impl Highlighter {
    pub fn new(theme_name: &str, enabled: bool) -> Self {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get(theme_name)
            .or_else(|| themes.themes.get("base16-ocean.dark"))
            .cloned()
            .expect("a default syntect theme must exist");
        Highlighter {
            syntaxes,
            theme,
            enabled,
        }
    }

    /// Highlight `text` using the language inferred from `path`. Returns, per
    /// line, the list of `(foreground, substring)` spans (newline stripped).
    pub fn highlight(&self, path: &Path, text: &str) -> Vec<Vec<(Color, String)>> {
        if !self.enabled {
            return text
                .lines()
                .map(|l| vec![(Color::Reset, l.to_string())])
                .collect();
        }
        let syntax = self
            .syntaxes
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .unwrap_or_else(|| self.syntaxes.find_syntax_plain_text());

        let mut hl = HighlightLines::new(syntax, &self.theme);
        let mut out = Vec::new();
        for line in LinesWithEndings::from(text) {
            let spans = match hl.highlight_line(line, &self.syntaxes) {
                Ok(ranges) => ranges
                    .into_iter()
                    .map(|(style, piece)| (syn_color(style), trim_eol(piece).to_string()))
                    .filter(|(_, s)| !s.is_empty())
                    .collect(),
                Err(_) => vec![(Color::Reset, trim_eol(line).to_string())],
            };
            out.push(spans);
        }
        out
    }
}

fn syn_color(style: SynStyle) -> Color {
    let c = style.foreground;
    Color::Rgb(c.r, c.g, c.b)
}

fn trim_eol(s: &str) -> &str {
    let s = s.strip_suffix('\n').unwrap_or(s);
    s.strip_suffix('\r').unwrap_or(s)
}
