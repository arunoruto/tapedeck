//! TUI colours. A theme is a small TOML file; two Catppuccin flavours are built in, and
//! `<config dir>/themes/<name>.toml` adds or overrides one. `theme = "<name>"` in config.toml
//! picks it.

use std::path::Path;

use anyhow::{Context, Result, bail};
use ratatui::style::Color;
use serde::Deserialize;

pub const DEFAULT: &str = "catppuccin-mocha";

const BUILTIN: &[(&str, &str)] = &[
    ("catppuccin-mocha", include_str!("../themes/catppuccin-mocha.toml")),
    ("catppuccin-latte", include_str!("../themes/catppuccin-latte.toml")),
];

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Theme {
    pub name: String,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    pub title: Color,
    pub selection_bg: Color,
    pub selection_fg: Color,
    pub ready: Color,
    pub failed: Color,
    pub wanted: Color,
    pub warning: Color,
    /// The cassette's stripes, any number; three looks like the logo.
    pub stripes: Vec<Color>,
}

impl Theme {
    /// A user file under `themes_dir` wins over a built-in of the same name.
    pub fn load(themes_dir: &Path, name: &str) -> Result<Theme> {
        let file = themes_dir.join(format!("{name}.toml"));
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(_) => match BUILTIN.iter().find(|(n, _)| *n == name) {
                Some((_, t)) => t.to_string(),
                None => {
                    let mut known: Vec<String> = BUILTIN.iter().map(|(n, _)| n.to_string()).collect();
                    if let Ok(dir) = std::fs::read_dir(themes_dir) {
                        known.extend(dir.flatten().filter_map(|e| {
                            let n = e.file_name().to_string_lossy().into_owned();
                            n.strip_suffix(".toml").map(str::to_string)
                        }));
                    }
                    bail!("no theme {name:?}: not in {} and not built in ({})", themes_dir.display(), known.join(", "));
                }
            },
        };
        Self::parse(&text).with_context(|| format!("theme {name:?}"))
    }

    pub fn parse(text: &str) -> Result<Theme> {
        let t: Theme = toml::from_str(text).context("parsing theme")?;
        if t.stripes.is_empty() {
            bail!("theme needs at least one stripe colour");
        }
        Ok(t)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::parse(BUILTIN[0].1).expect("built-in theme parses")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse_and_user_file_wins() {
        for (name, text) in BUILTIN {
            Theme::parse(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        let dir = std::env::temp_dir().join(format!("td-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mine = include_str!("../themes/catppuccin-mocha.toml").replace("Catppuccin Mocha", "Mine");
        std::fs::write(dir.join("catppuccin-mocha.toml"), mine).unwrap();
        assert_eq!(Theme::load(&dir, "catppuccin-mocha").unwrap().name, "Mine");
        assert_eq!(Theme::load(&dir, "catppuccin-latte").unwrap().name, "Catppuccin Latte");
        let err = Theme::load(&dir, "nope").unwrap_err().to_string();
        assert!(err.contains("catppuccin-latte") && err.contains("catppuccin-mocha"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
