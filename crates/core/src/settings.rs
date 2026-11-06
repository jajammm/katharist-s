//! User settings stored as TOML in `%APPDATA%\Katharistis\settings.toml`.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Auto,
    English,
    Indonesian,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub language: Language,
    pub theme: Theme,
    pub stale_days: u32,
    /// Folders skipped by every scan; feed into `ScanOptions::excluded`.
    pub excluded: Vec<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            language: Language::Auto,
            theme: Theme::System,
            stale_days: 365,
            excluded: Vec::new(),
        }
    }
}

/// A corrupt or missing file yields the defaults.
pub fn load_from(path: &Path) -> Settings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes to a `.tmp` sibling, then renames over the target.
pub fn save_to(path: &Path, s: &Settings) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(s).map_err(io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn default_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Katharistis").join("settings.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("settings.toml");
        let s = Settings {
            language: Language::Indonesian,
            theme: Theme::Light,
            stale_days: 90,
            excluded: vec![PathBuf::from(r"C:\Games"), PathBuf::from(r"D:\Data Lama")],
        };
        save_to(&path, &s).unwrap();
        assert_eq!(load_from(&path), s);
        assert!(!dir.path().join("sub").join("settings.toml.tmp").exists());
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "not toml {{").unwrap();
        let s = load_from(&path);
        assert_eq!(s, Settings::default());
        assert_eq!(s.stale_days, 365);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_from(&dir.path().join("nope.toml")),
            Settings::default()
        );
    }

    #[test]
    fn partial_file_fills_missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "theme = \"dark\"\n").unwrap();
        let s = load_from(&path);
        assert_eq!(s.theme, Theme::Dark);
        assert_eq!(s.stale_days, 365);
    }
}
