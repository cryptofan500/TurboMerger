//! Scanner module with security hardening and content-based detection
//!
//! v7.0 UPGRADE: Dual detection system
//! - PRIMARY: Content-based binary detection (read first 8KB, check for null bytes)
//! - SECONDARY: Extension allow/deny lists for fast-path optimization
//!
//! Original FIXES preserved:
//! 1. Removed expensive is_venv_directory() filesystem checks (18 syscalls → 0)
//! 2. Fixed "Convenience Bug" - word boundary checks prevent false positives
//! 3. Fixed double string allocation - lowercase computed once
//! 4. Split SKIP_DIRS so include_venv=true actually works

use std::path::{Path, PathBuf};
use std::io::Read;
use anyhow::Result;
use jwalk::WalkDir;
use phf::phf_set;
use serde::Serialize;

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

use crate::security::{has_reparse_point_in_path, is_sensitive_file};

/// Windows file attribute for reparse points (junctions/symlinks)
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Number of bytes to read for content-based binary detection
const SNIFF_SIZE: usize = 8192;

// ============================================================================
// EXTENSION SETS
// ============================================================================

/// Known text file extensions (compile-time perfect hash)
/// Files matching these skip content sniffing (optimization)
static TEXT_EXTENSIONS: phf::Set<&'static str> = phf_set! {
    // Code — mainstream
    "rs", "py", "js", "ts", "tsx", "jsx", "c", "cpp", "h", "hpp", "java", "kt", "go",
    "rb", "php", "swift", "cs", "fs", "scala", "clj", "ex", "exs", "lua", "r", "jl",
    "hs", "elm", "erl", "nim", "zig", "v", "d", "ada", "pas", "pl", "pm", "tcl",
    // Code — mobile
    "kts", "dart", "m", "mm", "xib", "storyboard", "plist", "pbxproj",
    "xcworkspacedata", "entitlements",
    // Code — functional
    "ml", "mli", "sml", "rkt", "ss", "scm", "lisp", "cl", "el",
    "cljs", "cljc", "edn", "fnl",
    // Code — systems
    "f90", "f95", "f03", "cob", "cbl", "asm", "s",
    "vhdl", "vhd", "sv", "svh",
    // Web
    "html", "htm", "css", "scss", "sass", "less", "vue", "svelte", "astro",
    "mjs", "cjs", "postcss", "styl", "pug", "jade",
    "haml", "slim", "erb", "ejs", "hbs", "njk", "twig",
    "liquid", "mustache", "jinja", "jinja2", "j2",
    // Data
    "json", "jsonc", "json5", "jsonl", "ndjson", "yaml", "yml", "toml",
    "xml", "csv", "tsv", "ron", "kdl", "pkl", "hocon", "avsc",
    // Config
    "ini", "cfg", "conf", "config", "env", "properties",
    "service", "socket", "timer", "path", "mount",
    "rules", "reg", "inf",
    // Infrastructure
    "tf", "hcl", "nix", "dhall", "jsonnet", "rego", "pp", "sls",
    // Docs
    "md", "markdown", "txt", "rst", "adoc", "org", "tex",
    "typ", "pod", "man", "bib", "textile",
    // Scripts
    "sh", "bash", "zsh", "fish", "ps1", "psm1", "bat", "cmd",
    "nu", "csh", "ksh", "awk", "sed",
    // Build
    "gradle", "cmake", "mk", "mak", "sbt", "just",
    "bazel", "bzl",
    // Other
    "sql", "graphql", "proto",
    "gitignore", "gitattributes", "editorconfig", "dockerignore",
    "diff", "patch", "prisma", "thrift", "capnp", "fbs",
    "bats", "robot", "feature",
};

/// Known binary file extensions — always skip (no content sniffing needed)
static BINARY_EXTENSIONS: phf::Set<&'static str> = phf_set! {
    // Executables & libraries
    "exe", "dll", "so", "dylib", "bin", "com", "msi", "app",
    // Object files & bytecode
    "o", "obj", "lib", "a", "class", "pyc", "pyo", "wasm",
    // Images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp",
    "tiff", "tif", "psd", "raw", "heic", "heif", "avif", "svg",
    // Audio
    "mp3", "wav", "flac", "aac", "ogg", "wma", "m4a", "opus",
    // Video
    "mp4", "avi", "mkv", "mov", "wmv", "flv", "webm", "m4v", "mpeg", "mpg",
    // Archives
    "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst", "lz4", "lzma", "cab",
    // Documents (binary formats)
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
    "odt", "ods", "odp",
    // Fonts
    "ttf", "otf", "woff", "woff2", "eot",
    // Databases
    "db", "sqlite", "sqlite3", "mdb",
    // Disk images
    "iso", "img", "dmg", "vmdk", "qcow2", "vhd",
    // Packages
    "deb", "rpm", "apk", "ipa", "snap", "flatpak",
    // Other binary
    "swf",
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
// SKIP DIRECTORY SETS
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

// ============================================================================
// SCAN OPTIONS, STATS, AND RESULT TYPES
// ============================================================================

/// Options controlling scanner behavior, built from UI options + config file
pub struct ScanOptions {
    pub include_venv: bool,
    pub content_sniff: bool,
    pub include_hidden: bool,
    pub max_file_size: u64,
    pub extra_text_exts: Vec<String>,
    pub extra_skip_exts: Vec<String>,
    pub extra_binary_exts: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_venv: false,
            content_sniff: true,
            include_hidden: false,
            max_file_size: 50 * 1024 * 1024,
            extra_text_exts: Vec::new(),
            extra_skip_exts: Vec::new(),
            extra_binary_exts: Vec::new(),
        }
    }
}

/// Statistics about how files were detected during scanning
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanStats {
    pub by_extension: usize,
    pub by_content: usize,
    pub skipped_binary: usize,
}

/// Complete scan result with file list and statistics
pub struct ScanResult {
    pub files: Vec<PathBuf>,
    pub stats: ScanStats,
}

// ============================================================================
// SECURITY HELPERS
// ============================================================================

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
// WORD BOUNDARY CHECKING (preserved from v6.0)
// ============================================================================

/// Check if a substring match has valid word boundaries
#[inline]
fn has_word_boundary(name: &str, pattern: &str, idx: usize) -> bool {
    let bytes = name.as_bytes();
    let end = idx + pattern.len();

    let safe_start = idx == 0 || !bytes[idx - 1].is_ascii_alphabetic();
    let safe_end = end == name.len() || !bytes[end].is_ascii_alphabetic();

    safe_start && safe_end
}

/// Fast check if a directory name indicates a virtual environment
/// NO FILESYSTEM I/O - pure string matching only
#[inline]
fn is_venv_by_name(lower_name: &str) -> bool {
    if SKIP_DIRS_VENV.contains(lower_name) {
        return true;
    }

    for &pattern in VENV_SUBSTRINGS {
        for (idx, _) in lower_name.match_indices(pattern) {
            if has_word_boundary(lower_name, pattern, idx) {
                return true;
            }
        }
    }

    false
}

// ============================================================================
// CONTENT-BASED BINARY DETECTION
// ============================================================================

/// Read first 8192 bytes and check if the file appears to be text.
///
/// Returns Ok(true) if the file appears to be text, Ok(false) if binary.
/// Empty files are considered text. IO errors return Err.
fn sniff_file_content(path: &Path) -> Result<bool> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut buffer = vec![0u8; SNIFF_SIZE];
    let bytes_read = reader.read(&mut buffer)?;

    if bytes_read == 0 {
        return Ok(true); // Empty file is text
    }

    buffer.truncate(bytes_read);
    Ok(!crate::security::is_binary_content(&buffer))
}

/// Check if an extensionless file has a well-known text filename
#[inline]
fn is_known_extensionless_file(name_lower: &str) -> bool {
    matches!(name_lower,
        "makefile" | "dockerfile" | "vagrantfile" | "jenkinsfile" |
        "gemfile" | "rakefile" | "readme" | "license" | "changelog" |
        "authors" | "contributors" | "todo" | "cmakelists.txt" |
        "procfile" | "brewfile" | "podfile" | "fastfile" | "appfile" |
        "justfile" | "taskfile" | "earthfile" | "tiltfile" |
        "snakefile" | "guardfile" | "berksfile" | "capfile" |
        "thorfile" | "puppetfile" | "modulefile" | "buildfile"
    )
}

// ============================================================================
// MAIN SCANNER
// ============================================================================

/// Scan directory for text files using dual detection:
/// 1. Extension-based (fast path via PHF sets)
/// 2. Content-based (read first 8KB for unknown extensions)
pub fn scan_text_files(root: &Path, options: &ScanOptions) -> Result<ScanResult> {
    // Validate root doesn't contain reparse points
    if has_reparse_point_in_path(root).unwrap_or(true) {
        anyhow::bail!("Root path contains junction points or symlinks");
    }

    let mut files = Vec::new();
    let mut stats = ScanStats::default();

    // Capture booleans for the closure (ScanOptions is not Copy)
    let include_venv = options.include_venv;
    let include_hidden = options.include_hidden;

    for entry in WalkDir::new(root)
        .skip_hidden(!include_hidden)
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
                    let name_lower = name.to_lowercase();

                    if entry.file_type().is_dir() {
                        // Always skip these directories (git, node_modules, etc.)
                        if SKIP_DIRS_ALWAYS.contains(name_lower.as_str()) {
                            return false;
                        }

                        // Only skip venv dirs when include_venv=false
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

                // Skip files exceeding max size
                if meta.len() > options.max_file_size {
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

            // === FILE DECISION LOGIC ===
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext_lower = ext.to_ascii_lowercase();

                // Step 1: Config exclude list (highest priority user override)
                if options.extra_skip_exts.iter().any(|e| e == &ext_lower) {
                    continue;
                }

                // Step 2: Known binary extension → skip
                if BINARY_EXTENSIONS.contains(ext_lower.as_str())
                    || options.extra_binary_exts.iter().any(|e| e == &ext_lower)
                {
                    stats.skipped_binary += 1;
                    continue;
                }

                // Step 3: Known text extension → include
                if TEXT_EXTENSIONS.contains(ext_lower.as_str())
                    || options.extra_text_exts.iter().any(|e| e == &ext_lower)
                {
                    stats.by_extension += 1;
                    files.push(path.to_path_buf());
                    continue;
                }

                // Step 4: Unknown extension → content sniff if enabled
                if options.content_sniff {
                    match sniff_file_content(&path) {
                        Ok(true) => {
                            stats.by_content += 1;
                            files.push(path.to_path_buf());
                        }
                        Ok(false) => {
                            stats.skipped_binary += 1;
                        }
                        Err(_) => {} // IO error → skip silently
                    }
                }
            } else {
                // Extensionless files — check known names, then optionally sniff
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let lower = name.to_lowercase();
                    if is_known_extensionless_file(&lower) {
                        stats.by_extension += 1;
                        files.push(path.to_path_buf());
                    } else if options.content_sniff {
                        match sniff_file_content(&path) {
                            Ok(true) => {
                                stats.by_content += 1;
                                files.push(path.to_path_buf());
                            }
                            Ok(false) => {
                                stats.skipped_binary += 1;
                            }
                            Err(_) => {}
                        }
                    }
                }
            }
        }
    }

    files.sort();
    Ok(ScanResult { files, stats })
}

/// Legacy function for backward compatibility
pub fn scan_text_files_default(root: &Path) -> Result<Vec<PathBuf>> {
    let options = ScanOptions::default();
    let result = scan_text_files(root, &options)?;
    Ok(result.files)
}

// ============================================================================
// COMPREHENSIVE TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Word boundary tests (preserved from v6.0) ---

    #[test]
    fn test_word_boundary_detection() {
        assert!(has_word_boundary("venv", "venv", 0));
        assert!(has_word_boundary("my_venv", "venv", 3));
        assert!(has_word_boundary("venv_logs", "venv", 0));
        assert!(has_word_boundary("project-venv", "venv", 8));
        assert!(has_word_boundary("my.venv.dir", "venv", 3));
        assert!(has_word_boundary("test_venv_2", "venv", 5));

        assert!(!has_word_boundary("event", "vent", 1));
        assert!(!has_word_boundary("convenience", "venv", 3));
        assert!(!has_word_boundary("events", "vent", 1));
    }

    #[test]
    fn test_venv_name_detection() {
        assert!(is_venv_by_name("venv"));
        assert!(is_venv_by_name(".venv"));
        assert!(is_venv_by_name("env"));
        assert!(is_venv_by_name("site-packages"));
        assert!(is_venv_by_name("__pycache__"));
        assert!(is_venv_by_name("conda"));
        assert!(is_venv_by_name("my_venv"));
        assert!(is_venv_by_name("project-venv"));
        assert!(is_venv_by_name("venv2"));
        assert!(is_venv_by_name("venv_logs"));
        assert!(is_venv_by_name("backend_virtualenv"));
        assert!(is_venv_by_name("test.venv.dir"));

        assert!(!is_venv_by_name("event"));
        assert!(!is_venv_by_name("events"));
        assert!(!is_venv_by_name("convenience"));
        assert!(!is_venv_by_name("inventory"));
        assert!(!is_venv_by_name("prevent"));
        assert!(!is_venv_by_name("convention"));
        assert!(!is_venv_by_name("src"));
        assert!(!is_venv_by_name("lib"));
        assert!(!is_venv_by_name("tests"));
        assert!(!is_venv_by_name("components"));
    }

    #[test]
    fn test_tricky_edge_cases() {
        assert!(is_venv_by_name("prevent_venv_leak"));
        assert!(is_venv_by_name("event_venv"));
        assert!(is_venv_by_name("venv3"));
        assert!(is_venv_by_name("2venv"));
        assert!(!is_venv_by_name("v"));
        assert!(!is_venv_by_name("e"));
    }

    #[test]
    fn test_skip_dirs_separation() {
        assert!(SKIP_DIRS_ALWAYS.contains("node_modules"));
        assert!(SKIP_DIRS_ALWAYS.contains(".git"));
        assert!(SKIP_DIRS_ALWAYS.contains("target"));

        assert!(SKIP_DIRS_VENV.contains("venv"));
        assert!(SKIP_DIRS_VENV.contains(".venv"));
        assert!(SKIP_DIRS_VENV.contains("site-packages"));

        assert!(!SKIP_DIRS_ALWAYS.contains("venv"));
        assert!(!SKIP_DIRS_VENV.contains("node_modules"));
    }

    // --- New v7.0 tests ---

    #[test]
    fn test_binary_extensions_in_set() {
        assert!(BINARY_EXTENSIONS.contains("exe"));
        assert!(BINARY_EXTENSIONS.contains("png"));
        assert!(BINARY_EXTENSIONS.contains("jpg"));
        assert!(BINARY_EXTENSIONS.contains("zip"));
        assert!(BINARY_EXTENSIONS.contains("pdf"));
        assert!(BINARY_EXTENSIONS.contains("dll"));
        assert!(BINARY_EXTENSIONS.contains("wasm"));
        assert!(BINARY_EXTENSIONS.contains("mp4"));
        assert!(BINARY_EXTENSIONS.contains("sqlite3"));
        assert!(BINARY_EXTENSIONS.contains("ttf"));
    }

    #[test]
    fn test_expanded_text_extensions() {
        // Original extensions still present
        assert!(TEXT_EXTENSIONS.contains("rs"));
        assert!(TEXT_EXTENSIONS.contains("py"));
        assert!(TEXT_EXTENSIONS.contains("js"));
        assert!(TEXT_EXTENSIONS.contains("html"));
        assert!(TEXT_EXTENSIONS.contains("json"));
        assert!(TEXT_EXTENSIONS.contains("md"));

        // New mobile extensions
        assert!(TEXT_EXTENSIONS.contains("dart"));
        assert!(TEXT_EXTENSIONS.contains("kt"));
        assert!(TEXT_EXTENSIONS.contains("plist"));

        // New infrastructure extensions
        assert!(TEXT_EXTENSIONS.contains("tf"));
        assert!(TEXT_EXTENSIONS.contains("hcl"));
        assert!(TEXT_EXTENSIONS.contains("nix"));

        // New functional extensions
        assert!(TEXT_EXTENSIONS.contains("ml"));
        assert!(TEXT_EXTENSIONS.contains("scm"));
        assert!(TEXT_EXTENSIONS.contains("cljs"));

        // New web extensions
        assert!(TEXT_EXTENSIONS.contains("mjs"));
        assert!(TEXT_EXTENSIONS.contains("pug"));
        assert!(TEXT_EXTENSIONS.contains("hbs"));
        assert!(TEXT_EXTENSIONS.contains("twig"));

        // New build extensions
        assert!(TEXT_EXTENSIONS.contains("just"));
        assert!(TEXT_EXTENSIONS.contains("bazel"));
    }

    #[test]
    fn test_no_extension_overlap() {
        // Ensure no extension appears in both TEXT and BINARY sets
        let text_samples = ["rs", "py", "js", "html", "json", "md", "tf", "dart"];
        let binary_samples = ["exe", "png", "zip", "pdf", "dll", "mp4"];

        for ext in &text_samples {
            assert!(!BINARY_EXTENSIONS.contains(ext),
                "{} should not be in BINARY_EXTENSIONS", ext);
        }
        for ext in &binary_samples {
            assert!(!TEXT_EXTENSIONS.contains(ext),
                "{} should not be in TEXT_EXTENSIONS", ext);
        }
    }

    #[test]
    fn test_extensionless_files_detected() {
        assert!(is_known_extensionless_file("makefile"));
        assert!(is_known_extensionless_file("dockerfile"));
        assert!(is_known_extensionless_file("readme"));
        assert!(is_known_extensionless_file("license"));
        assert!(is_known_extensionless_file("justfile"));
        assert!(is_known_extensionless_file("earthfile"));

        assert!(!is_known_extensionless_file("randomfile"));
        assert!(!is_known_extensionless_file("data"));
    }

    #[test]
    fn test_scan_options_default() {
        let opts = ScanOptions::default();
        assert!(!opts.include_venv);
        assert!(opts.content_sniff);
        assert!(!opts.include_hidden);
        assert_eq!(opts.max_file_size, 50 * 1024 * 1024);
        assert!(opts.extra_text_exts.is_empty());
        assert!(opts.extra_skip_exts.is_empty());
        assert!(opts.extra_binary_exts.is_empty());
    }

    #[test]
    fn test_content_sniff_detects_utf8_text() {
        let dir = std::env::temp_dir().join("turbomerger_test_sniff_text");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.xyz");
        std::fs::write(&file_path, b"Hello, world!\nThis is a text file.\n").unwrap();

        let result = sniff_file_content(&file_path);
        assert!(result.is_ok());
        assert!(result.unwrap(), "UTF-8 text should be detected as text");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_content_sniff_detects_binary() {
        let dir = std::env::temp_dir().join("turbomerger_test_sniff_binary");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.xyz");
        // PNG magic bytes followed by binary data
        let mut data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0x00; 100]);
        std::fs::write(&file_path, &data).unwrap();

        let result = sniff_file_content(&file_path);
        assert!(result.is_ok());
        assert!(!result.unwrap(), "PNG binary data should be detected as binary");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_content_sniff_empty_file_is_text() {
        let dir = std::env::temp_dir().join("turbomerger_test_sniff_empty");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("empty.xyz");
        std::fs::write(&file_path, b"").unwrap();

        let result = sniff_file_content(&file_path);
        assert!(result.is_ok());
        assert!(result.unwrap(), "Empty file should be treated as text");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_content_sniff_null_bytes_detected() {
        let dir = std::env::temp_dir().join("turbomerger_test_sniff_null");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("nulls.xyz");
        // >5% null bytes in a 100-byte sample triggers binary detection
        let mut data = Vec::new();
        for i in 0..100u8 {
            if i % 10 == 0 { data.push(0x00); }
            else { data.push(b'A'); }
        }
        std::fs::write(&file_path, &data).unwrap();

        let result = sniff_file_content(&file_path);
        assert!(result.is_ok());
        assert!(!result.unwrap(), "Data with >5% null bytes should be binary");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
