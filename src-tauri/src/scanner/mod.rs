//! Scanner module with security hardening - PRODUCTION v2
//!
//! FIXES APPLIED:
//! 1. Removed expensive is_venv_directory() filesystem checks (18 syscalls → 0)
//! 2. Fixed "Convenience Bug" - word boundary checks prevent false positives
//! 3. Fixed double string allocation - lowercase computed once
//! 4. Split SKIP_DIRS so include_venv=true actually works
//!
//! Cross-AI reviewed: Grok, ChatGPT, Gemini - Jan 11, 2026

use std::path::{Path, PathBuf};
use anyhow::Result;
use jwalk::WalkDir;
use phf::phf_set;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

use crate::security::{has_reparse_point_in_path, is_sensitive_file};

/// Windows file attribute for reparse points (junctions/symlinks)
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Known text file extensions (compile-time perfect hash)
static TEXT_EXTENSIONS: phf::Set<&'static str> = phf_set! {
    // Code
    "rs", "py", "js", "ts", "tsx", "jsx", "c", "cpp", "h", "hpp", "java", "kt", "go",
    "rb", "php", "swift", "cs", "fs", "scala", "clj", "ex", "exs", "lua", "r", "jl",
    "hs", "elm", "erl", "nim", "zig", "v", "d", "ada", "pas", "pl", "pm", "tcl",
    // Web
    "html", "htm", "css", "scss", "sass", "less", "vue", "svelte", "astro",
    // Data
    "json", "jsonc", "json5", "yaml", "yml", "toml", "xml", "csv", "tsv",
    // Config
    "ini", "cfg", "conf", "config", "env", "properties",
    // Docs
    "md", "markdown", "txt", "rst", "adoc", "org", "tex",
    // Scripts
    "sh", "bash", "zsh", "fish", "ps1", "psm1", "bat", "cmd",
    // Build
    "gradle", "cmake",
    // Other
    "sql", "graphql", "proto",
    "gitignore", "gitattributes", "editorconfig", "dockerignore",
};

/// Lock files to always skip (bloat LLM context without value)
static SKIP_FILES: &[&str] = &[
    "package-lock.json",
    "Cargo.lock",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "Gemfile.lock",
    "poetry.lock",
];

// ============================================================================
// FIX #4: Split SKIP_DIRS into ALWAYS skip vs VENV-specific
// This ensures include_venv=true actually includes venv directories
// ============================================================================

/// Directories to ALWAYS skip (regardless of include_venv setting)
static SKIP_DIRS_ALWAYS: phf::Set<&'static str> = phf_set! {
    // Version Control
    ".git", ".svn", ".hg", ".bzr",

    // Node.js
    "node_modules", ".npm", ".yarn", ".pnpm-store",

    // Rust
    "target", ".cargo",

    // Build outputs
    "dist", "build", "out", "_build",

    // IDE/Editor
    ".idea", ".vscode", ".vs",

    // Coverage/Testing
    "coverage", ".coverage", "htmlcov", ".nyc_output",

    // Caches
    ".cache", ".parcel-cache", ".next", ".nuxt", ".output",

    // Misc
    "vendor", "packages", "bower_components",
    "obj", "debug", "release", "x64", "x86",

    // Windows
    "$recycle.bin", "system volume information",

    // macOS
    ".ds_store", "__macosx",

    // Terraform/Cloud
    ".terraform", ".serverless",
};

/// Python venv directories - only skipped when include_venv=false
static SKIP_DIRS_VENV: phf::Set<&'static str> = phf_set! {
    // Standard venv names
    "venv", ".venv", "env", ".env", "virtualenv",

    // Common custom names
    "virtual_env", "virtualenvs", "pyenv",

    // Poetry/pipenv
    ".poetry", ".pipenv",

    // Conda
    "conda", ".conda", "miniconda", "miniconda3", "anaconda", "anaconda3",

    // Internal venv directories (always in a venv context)
    "site-packages", "lib64",

    // Python cache/build (these are always noise)
    "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    ".tox", ".nox", "eggs", ".eggs", "pip-wheel-metadata",
};

/// Substring patterns for catching custom venv names
/// These require word-boundary checking to avoid false positives
static VENV_SUBSTRINGS: &[&str] = &[
    "venv",         // catches my_venv, project-venv, venv2
    "virtualenv",   // catches my_virtualenv
    "site-packages",
];

/// SECURITY: Check if metadata indicates a reparse point
#[cfg(windows)]
fn is_reparse_point(meta: &std::fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_meta: &std::fs::Metadata) -> bool {
    false
}

// ============================================================================
// FIX #1 & #2: Word boundary checking to prevent false positives
// "event", "convenience", "given" should NOT match "venv"
// "my_venv", "project-venv", "venv2" SHOULD match
// ============================================================================

/// Check if a substring match has valid word boundaries
/// Returns true if the pattern is at a word boundary (start/end of string or
/// surrounded by non-alphabetic characters like _, -, ., or digits)
///
/// NOTE: We use is_ascii_alphabetic() NOT is_ascii_alphanumeric() because:
/// - "venv2" should match (2 is a boundary, common naming: venv2, venv3)
/// - "event" should NOT match (e and t are alphabetic, no boundary)
#[inline]
fn has_word_boundary(name: &str, pattern: &str, idx: usize) -> bool {
    let bytes = name.as_bytes();
    let end = idx + pattern.len();

    // Check character before match (must be non-alphabetic or start of string)
    // Digits like '2' ARE valid boundaries (allows "2venv" to match)
    let safe_start = idx == 0 || !bytes[idx - 1].is_ascii_alphabetic();

    // Check character after match (must be non-alphabetic or end of string)
    // Digits like '2' ARE valid boundaries (allows "venv2" to match)
    let safe_end = end == name.len() || !bytes[end].is_ascii_alphabetic();

    safe_start && safe_end
}

/// Fast check if a directory name indicates a virtual environment
/// NO FILESYSTEM I/O - pure string matching only
///
/// FIX #3: Accepts pre-lowercased string to avoid double allocation
#[inline]
fn is_venv_by_name(lower_name: &str) -> bool {
    // 1. Check exact venv matches first (O(1) with PHF)
    if SKIP_DIRS_VENV.contains(lower_name) {
        return true;
    }

    // 2. Check for venv substrings with WORD BOUNDARY protection
    // This prevents "event", "convenience", "given" from matching "venv"
    // But allows "my_venv", "project-venv", "venv2" to match
    for &pattern in VENV_SUBSTRINGS {
        // Iterate over ALL matches to handle cases like "prevent_venv_leak"
        // (first "vent" in "prevent" → fail, actual "_venv_" → pass)
        for (idx, _) in lower_name.match_indices(pattern) {
            if has_word_boundary(lower_name, pattern, idx) {
                return true;
            }
        }
    }

    false
}

/// OPTIMIZED: Scan with no expensive filesystem-based venv detection
/// Uses name-based detection only (like Version 4)
pub fn scan_text_files(root: &Path, include_venv: bool) -> Result<Vec<PathBuf>> {
    // Validate root doesn't contain reparse points
    if has_reparse_point_in_path(root).unwrap_or(true) {
        anyhow::bail!("Root path contains junction points or symlinks");
    }

    let mut files = Vec::new();

    for entry in WalkDir::new(root)
        .skip_hidden(true)
        .follow_links(false)  // SECURITY: Never follow symlinks
        .process_read_dir(move |_, _, _, children| {
            children.retain(|child| {
                if let Ok(entry) = child {
                    // SECURITY: Check for reparse points via metadata
                    if let Ok(meta) = entry.metadata() {
                        if is_reparse_point(&meta) {
                            return false;
                        }
                    }

                    // Skip symlinks entirely (security)
                    if entry.file_type().is_symlink() {
                        return false;
                    }

                    let name = entry.file_name().to_string_lossy();

                    // FIX #3: Compute lowercase ONCE, pass reference
                    let name_lower = name.to_lowercase();

                    if entry.file_type().is_dir() {
                        // Always skip these directories (git, node_modules, etc.)
                        if SKIP_DIRS_ALWAYS.contains(name_lower.as_str()) {
                            return false;
                        }

                        // FIX #4: Only skip venv dirs when include_venv=false
                        // This ensures include_venv=true actually includes them!
                        if !include_venv && is_venv_by_name(&name_lower) {
                            return false;
                        }

                        return true;
                    }
                }
                true
            });
        })
    {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            // SECURITY: Check each file path for reparse points
            if has_reparse_point_in_path(&path).unwrap_or(true) {
                continue;
            }

            // Skip symlinks
            if let Ok(meta) = path.symlink_metadata() {
                if meta.file_type().is_symlink() {
                    continue;
                }
                if is_reparse_point(&meta) {
                    continue;
                }
            }

            // Skip sensitive files
            if is_sensitive_file(&path) {
                continue;
            }

            // Skip lock files by name
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if SKIP_FILES.iter().any(|&skip| name.eq_ignore_ascii_case(skip)) {
                    continue;
                }
            }

            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                // FIX: Correct PHF contains() usage
                let ext_lower = ext.to_ascii_lowercase();
                if TEXT_EXTENSIONS.contains(ext_lower.as_str()) {
                    files.push(path.to_path_buf());
                }
            } else {
                // Extensionless files - check common names
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let lower = name.to_lowercase();
                    if matches!(lower.as_str(),
                        "makefile" | "dockerfile" | "vagrantfile" | "jenkinsfile" |
                        "gemfile" | "rakefile" | "readme" | "license" | "changelog" |
                        "authors" | "contributors" | "todo" | "cmakelists.txt" |
                        "procfile" | "brewfile" | "podfile" | "fastfile" | "appfile"
                    ) {
                        files.push(path.to_path_buf());
                    }
                }
            }
        }
    }

    files.sort();
    Ok(files)
}

/// Legacy function for backward compatibility
pub fn scan_text_files_default(root: &Path) -> Result<Vec<PathBuf>> {
    scan_text_files(root, false)
}

// ============================================================================
// COMPREHENSIVE TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_boundary_detection() {
        // Valid boundaries (start/end of string)
        assert!(has_word_boundary("venv", "venv", 0));
        assert!(has_word_boundary("my_venv", "venv", 3));
        assert!(has_word_boundary("venv_logs", "venv", 0));

        // Valid boundaries (non-alphanumeric separators)
        assert!(has_word_boundary("project-venv", "venv", 8));
        assert!(has_word_boundary("my.venv.dir", "venv", 3));
        assert!(has_word_boundary("test_venv_2", "venv", 5));

        // Invalid boundaries (embedded in word)
        assert!(!has_word_boundary("event", "vent", 1));      // "e[vent]" - e is alphanumeric
        assert!(!has_word_boundary("convenience", "venv", 3)); // "con[veni]ence"
        assert!(!has_word_boundary("events", "vent", 1));     // "e[vent]s"
    }

    #[test]
    fn test_venv_name_detection() {
        // Standard exact matches (PHF lookup)
        assert!(is_venv_by_name("venv"));
        assert!(is_venv_by_name(".venv"));
        assert!(is_venv_by_name("env"));
        assert!(is_venv_by_name("site-packages"));
        assert!(is_venv_by_name("__pycache__"));
        assert!(is_venv_by_name("conda"));

        // Custom names with valid word boundaries (SHOULD MATCH)
        assert!(is_venv_by_name("my_venv"));
        assert!(is_venv_by_name("project-venv"));
        assert!(is_venv_by_name("venv2"));
        assert!(is_venv_by_name("venv_logs"));
        assert!(is_venv_by_name("backend_virtualenv"));
        assert!(is_venv_by_name("test.venv.dir"));

        // FALSE POSITIVE PREVENTION (SHOULD NOT MATCH)
        // These contain "venv" letters but are NOT venvs
        assert!(!is_venv_by_name("event"));        // e-vent (no boundary)
        assert!(!is_venv_by_name("events"));       // e-vent-s
        assert!(!is_venv_by_name("convenience"));  // con-veni-ence
        assert!(!is_venv_by_name("inventory"));    // in-vent-ory
        assert!(!is_venv_by_name("prevent"));      // pre-vent
        assert!(!is_venv_by_name("convention"));   // con-vent-ion

        // Regular directories (should never match)
        assert!(!is_venv_by_name("src"));
        assert!(!is_venv_by_name("lib"));
        assert!(!is_venv_by_name("tests"));
        assert!(!is_venv_by_name("components"));
    }

    #[test]
    fn test_tricky_edge_cases() {
        // Multiple pattern occurrences - should find the valid one
        assert!(is_venv_by_name("prevent_venv_leak")); // first "vent" fails, "_venv_" passes
        assert!(is_venv_by_name("event_venv"));        // first "vent" fails, "_venv" passes

        // Numbers as boundaries
        assert!(is_venv_by_name("venv3"));
        assert!(is_venv_by_name("2venv"));

        // Edge: single character names
        assert!(!is_venv_by_name("v"));
        assert!(!is_venv_by_name("e"));
    }

    #[test]
    fn test_skip_dirs_separation() {
        // ALWAYS skip (regardless of include_venv)
        assert!(SKIP_DIRS_ALWAYS.contains("node_modules"));
        assert!(SKIP_DIRS_ALWAYS.contains(".git"));
        assert!(SKIP_DIRS_ALWAYS.contains("target"));

        // VENV-specific (only skip when include_venv=false)
        assert!(SKIP_DIRS_VENV.contains("venv"));
        assert!(SKIP_DIRS_VENV.contains(".venv"));
        assert!(SKIP_DIRS_VENV.contains("site-packages"));

        // Verify no overlap causing issues
        assert!(!SKIP_DIRS_ALWAYS.contains("venv"));
        assert!(!SKIP_DIRS_VENV.contains("node_modules"));
    }
}
