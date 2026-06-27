//! Configuration (PRD §13).
//!
//! `margin` reads `.margin/config.toml` from the repository root. Every field is
//! optional; a missing or malformed file degrades to defaults with a warning
//! rather than failing. v1 wires the base branch, fallback count, and theme; the
//! remaining PRD §13 keys (keybindings) are reserved for later.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Parsed `.margin/config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Default base branch for the sidebar.
    pub base: Option<String>,
    /// Recent-commit count when no base resolves.
    pub fallback: Option<usize>,
    /// `"light"` or `"dark"` to force a theme.
    pub theme: Option<String>,
    /// `"git"` or `"jj"` to force a backend instead of auto-detecting.
    pub vcs: Option<String>,
}

impl Config {
    /// Load `.margin/config.toml` under `repo_root`, or defaults when absent.
    /// A malformed file is reported to stderr and treated as empty.
    pub fn load(repo_root: impl AsRef<Path>) -> Config {
        let path = Self::path(repo_root.as_ref());

        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Config::default(),
            Err(error) => {
                eprintln!("margin: could not read {}: {error}", path.display());
                return Config::default();
            }
        };

        toml::from_str(&contents).unwrap_or_else(|error| {
            eprintln!("margin: ignoring malformed {}: {error}", path.display());
            Config::default()
        })
    }

    fn path(repo_root: &Path) -> PathBuf {
        repo_root.join(".margin").join("config.toml")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_is_default() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(dir.path());
        assert!(config.base.is_none());
        assert!(config.theme.is_none());
    }

    #[test]
    fn parses_known_fields() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".margin")).unwrap();
        std::fs::write(
            dir.path().join(".margin/config.toml"),
            "base = \"develop\"\ntheme = \"light\"\n",
        )
        .unwrap();

        let config = Config::load(dir.path());
        assert_eq!(config.base.as_deref(), Some("develop"));
        assert_eq!(config.theme.as_deref(), Some("light"));
    }

    #[test]
    fn malformed_config_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".margin")).unwrap();
        std::fs::write(dir.path().join(".margin/config.toml"), "base = = =").unwrap();
        assert!(Config::load(dir.path()).base.is_none());
    }
}
