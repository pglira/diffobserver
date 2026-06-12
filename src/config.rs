//! Optional TOML configuration, loaded from the user config dir.
//!
//! Everything has a sensible default, so the app runs with no config file.

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Explicit syntect theme name; empty means pick by `mode`.
    pub theme: String,
    /// Light or dark; selects a default theme when `theme` is empty.
    pub mode: Mode,
    /// Files larger than this many bytes are listed but not content-diffed.
    pub size_cap_bytes: u64,
    /// Tree pane width as a percentage of total width.
    pub tree_width_percent: u16,
    /// Editor command override; falls back to $VISUAL/$EDITOR.
    pub editor: Option<String>,
    /// Whether to syntax-highlight diff content.
    pub syntax_highlight: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: String::new(),
            mode: Mode::Dark,
            size_cap_bytes: 2 * 1024 * 1024,
            tree_width_percent: 30,
            editor: None,
            syntax_highlight: true,
        }
    }
}

impl Config {
    /// Load config from disk, falling back to defaults on any error.
    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Config::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Config::default();
        };
        match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("diffobserver: ignoring invalid config {}: {e}", path.display());
                Config::default()
            }
        }
    }

    pub fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("diffobserver").join("config.toml"))
    }

    /// The editor command: config override, else $VISUAL, $EDITOR, else `vi`.
    pub fn editor_cmd(&self) -> String {
        self.editor
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("VISUAL").ok().filter(|s| !s.is_empty()))
            .or_else(|| std::env::var("EDITOR").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "vi".to_string())
    }

    /// The syntect theme name to use.
    pub fn theme_name(&self) -> &str {
        if !self.theme.is_empty() {
            &self.theme
        } else if self.mode == Mode::Light {
            "InspiredGitHub"
        } else {
            "base16-ocean.dark"
        }
    }

    pub fn tree_width_percent(&self) -> u16 {
        self.tree_width_percent.clamp(10, 80)
    }
}
