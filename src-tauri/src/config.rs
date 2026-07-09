//! Configuration module for optional turbomerger.toml support
//!
//! Loads per-project configuration from a `turbomerger.toml` file in the
//! scanned directory root. All fields are optional with sensible defaults.

use serde::Deserialize;
use std::path::Path;

/// Top-level configuration loaded from turbomerger.toml
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct TurboMergerConfig {
    pub extensions: ExtensionConfig,
    pub scanning: ScanningConfig,
}

/// Extension override configuration
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct ExtensionConfig {
    /// Additional extensions to always treat as text (e.g., ["myformat", "custom1"])
    pub include: Vec<String>,
    /// Extensions to always skip even if they contain text (e.g., ["log", "tmp"])
    pub exclude: Vec<String>,
    /// Additional extensions to treat as binary (e.g., ["dat"])
    pub binary: Vec<String>,
}

/// Scanning behavior configuration
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct ScanningConfig {
    /// Include hidden files and directories (.dotfiles, .dotfolders)
    pub include_hidden: bool,
    /// Include virtual environments (venv, node_modules, __pycache__, etc.)
    pub include_venvs: bool,
    /// Skip files larger than this (in MB). This is the REAL absolute cap
    /// (v7.1 silently overrode anything above a hardcoded 2 MB).
    pub max_file_size_mb: u64,
    /// Enable content-based binary detection for unknown extensions
    pub content_sniff: bool,
}

impl Default for ScanningConfig {
    fn default() -> Self {
        Self {
            include_hidden: false,
            include_venvs: false,
            max_file_size_mb: 2,
            content_sniff: true,
        }
    }
}

/// Load configuration from a `turbomerger.toml` file in the given directory.
///
/// Returns default configuration if the file does not exist.
/// Returns default configuration (with eprintln warning) if the file is malformed.
pub fn load_from_dir(dir: &Path) -> TurboMergerConfig {
    let config_path = dir.join("turbomerger.toml");

    if !config_path.exists() {
        return TurboMergerConfig::default();
    }

    match std::fs::read_to_string(&config_path) {
        Ok(content) => match toml::from_str(&content) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("Warning: Failed to parse turbomerger.toml: {}", e);
                TurboMergerConfig::default()
            }
        },
        Err(e) => {
            eprintln!("Warning: Failed to read turbomerger.toml: {}", e);
            TurboMergerConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TurboMergerConfig::default();
        assert!(config.extensions.include.is_empty());
        assert!(config.extensions.exclude.is_empty());
        assert!(config.extensions.binary.is_empty());
        assert!(!config.scanning.include_hidden);
        assert!(!config.scanning.include_venvs);
        assert_eq!(config.scanning.max_file_size_mb, 2);
        assert!(config.scanning.content_sniff);
    }

    #[test]
    fn test_parse_full_config() {
        let toml_str = r#"
[extensions]
include = ["myformat", "custom1"]
exclude = ["log", "tmp"]
binary = ["dat"]

[scanning]
include_hidden = true
include_venvs = true
max_file_size_mb = 10
content_sniff = false
"#;
        let config: TurboMergerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.extensions.include, vec!["myformat", "custom1"]);
        assert_eq!(config.extensions.exclude, vec!["log", "tmp"]);
        assert_eq!(config.extensions.binary, vec!["dat"]);
        assert!(config.scanning.include_hidden);
        assert!(config.scanning.include_venvs);
        assert_eq!(config.scanning.max_file_size_mb, 10);
        assert!(!config.scanning.content_sniff);
    }

    #[test]
    fn test_parse_partial_config() {
        let toml_str = r#"
[scanning]
max_file_size_mb = 25
"#;
        let config: TurboMergerConfig = toml::from_str(toml_str).unwrap();
        // Specified field
        assert_eq!(config.scanning.max_file_size_mb, 25);
        // Defaults for everything else
        assert!(config.extensions.include.is_empty());
        assert!(!config.scanning.include_hidden);
        assert!(config.scanning.content_sniff);
    }

    #[test]
    fn test_missing_file_returns_default() {
        let config = load_from_dir(Path::new("/nonexistent/path/that/does/not/exist"));
        assert!(config.scanning.content_sniff);
        assert_eq!(config.scanning.max_file_size_mb, 2);
    }

    #[test]
    fn test_empty_toml_returns_default() {
        let config: TurboMergerConfig = toml::from_str("").unwrap();
        assert!(config.scanning.content_sniff);
        assert!(config.extensions.include.is_empty());
    }
}
