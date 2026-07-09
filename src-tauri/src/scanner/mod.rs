//! Scanner: gitignore-aware directory walking + text/binary classification.
//!
//! v7.2 rewrite:
//! - jwalk + hand-rolled skip lists replaced by the `ignore` crate (ripgrep's
//!   walker): `.gitignore` / `.ignore` / `.git/info/exclude` are honored
//!   (toggleable) plus a highest-precedence `.turbomergerignore`.
//! - Well-known dot-config files (.gitignore, .mcp.json, .github/…, …) are
//!   included by default; "Include hidden files" includes everything dotted.
//! - Every skipped file is recorded with a reason (surfaced in the output's
//!   Merge Report) instead of vanishing silently.
//! - One unreadable entry no longer aborts the scan.
//! - Content sniffing runs in parallel (rayon) instead of on the walk thread.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;
use phf::phf_set;
use rayon::prelude::*;
use serde::Serialize;

use crate::security::{has_reparse_point_in_path, sensitive_reason};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// OneDrive/cloud placeholder: reading it would force a download (hydration)
#[cfg(windows)]
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// Number of bytes to read for content-based binary detection
const SNIFF_SIZE: usize = 8192;

/// Max line length before a file is considered minified/binary
const MAX_LINE_LENGTH: usize = 1000;

/// If >10% of bytes are control chars (0x01-0x08, 0x0E-0x1F), treat as binary
const CONTROL_CHAR_THRESHOLD_PCT: usize = 10;

/// If >40% of bytes have high bit set (0x80-0xFF), treat as binary/encoded
const NON_ASCII_THRESHOLD_PCT: usize = 40;

/// Files larger than this with unknown extensions are skipped (500KB)
const LARGE_FILE_UNKNOWN_EXT: u64 = 524_288;

// ============================================================================
// EXTENSION SETS
// ============================================================================

/// Known text file extensions (compile-time perfect hash)
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
    // Databases + their journals (cookies.sqlite-wal leaked in v7.1 because
    // these compound extensions were missing and the WAL content is mostly ASCII)
    "db", "sqlite", "sqlite3", "mdb",
    "sqlite-wal", "sqlite-shm", "sqlite-journal",
    "db-wal", "db-shm", "db-journal",
    // Disk images
    "iso", "img", "dmg", "vmdk", "qcow2", "vhd",
    // Packages
    "deb", "rpm", "apk", "ipa", "snap", "flatpak",
    // Other binary
    "swf",
    // Build artifacts & caches
    "map", "lock", "lockb", "tsbuildinfo", "eslintcache", "stylelintcache",
};

/// Files to always skip by exact name (context bloat or Windows system noise)
static SKIP_FILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "cargo.lock",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "composer.lock",
    "gemfile.lock",
    "poetry.lock",
    "packages.lock.json",
    "gradle.lockfile",
    "go.sum",
    "desktop.ini",
    "thumbs.db",
    "ntuser.dat",
];

// ============================================================================
// SKIP DIRECTORY SETS
// ============================================================================

/// Directories to ALWAYS skip (regardless of include_venv / include_hidden)
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
    ".svelte-kit", ".turbo",
    // Deployment
    ".vercel", ".netlify",
    // Generated / docs
    "__generated__", ".docusaurus", "storybook-static",
    // Misc
    "vendor", "packages", "bower_components",
    "obj", "debug", "release", "x64", "x86",
    // Windows
    "$recycle.bin", "system volume information",
    // macOS
    ".ds_store", "__macosx",
    // Terraform/Cloud
    ".terraform", ".serverless",
    // Credential directories
    ".ssh", ".aws", ".gnupg",
};

/// Python venv directories - only skipped when include_venv=false
static SKIP_DIRS_VENV: phf::Set<&'static str> = phf_set! {
    "venv", ".venv", "env", ".env", "virtualenv",
    "virtual_env", "virtualenvs", "pyenv",
    ".poetry", ".pipenv",
    "conda", ".conda", "miniconda", "miniconda3", "anaconda", "anaconda3",
    "site-packages", "lib64",
    "__pycache__", ".pytest_cache", ".mypy_cache", ".ruff_cache",
    ".tox", ".nox", "eggs", ".eggs", "pip-wheel-metadata",
};

/// Substring patterns for catching custom venv names (word-boundary checked)
static VENV_SUBSTRINGS: &[&str] = &["venv", "virtualenv", "site-packages"];

/// Dot-DIRECTORIES that are included even when hidden files are off
const DOT_DIR_ALLOWLIST: &[&str] = &[".github", ".devcontainer"];

/// Well-known dot-FILES included even when hidden files are off. These are the
/// "missed key files" class: a code reviewer needs them.
fn is_allowlisted_dotfile(name_lower: &str) -> bool {
    matches!(
        name_lower,
        ".gitignore"
            | ".gitattributes"
            | ".gitmodules"
            | ".dockerignore"
            | ".editorconfig"
            | ".nvmrc"
            | ".node-version"
            | ".python-version"
            | ".ruby-version"
            | ".tool-versions"
            | ".env.example"
            | ".env.sample"
            | ".env.template"
            | ".mcp.json"
            | ".eslintignore"
            | ".prettierignore"
            | ".gitlab-ci.yml"
            | ".travis.yml"
            | ".flake8"
            | ".pylintrc"
            | ".pre-commit-config.yaml"
            | ".clang-format"
            | ".clang-tidy"
    ) || name_lower.starts_with(".eslintrc")
        || name_lower.starts_with(".prettierrc")
        || name_lower.starts_with(".stylelintrc")
        || name_lower.starts_with(".babelrc")
}

// ============================================================================
// SCAN OPTIONS, STATS, AND RESULT TYPES
// ============================================================================

/// Options controlling scanner behavior, built from UI options + config file
pub struct ScanOptions {
    pub include_venv: bool,
    pub content_sniff: bool,
    pub include_hidden: bool,
    pub respect_gitignore: bool,
    /// Absolute per-file cap in bytes (config `max_file_size_mb`, default 2 MB)
    pub max_file_size: u64,
    pub extra_text_exts: Vec<String>,
    pub extra_skip_exts: Vec<String>,
    pub extra_binary_exts: Vec<String>,
    /// Whitelist globs — if non-empty, ONLY matching files are kept.
    pub include_globs: Vec<String>,
    /// Blacklist globs — matching files are dropped.
    pub exclude_globs: Vec<String>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_venv: false,
            content_sniff: true,
            include_hidden: false,
            respect_gitignore: true,
            max_file_size: 2 * 1024 * 1024,
            extra_text_exts: Vec::new(),
            extra_skip_exts: Vec::new(),
            extra_binary_exts: Vec::new(),
            include_globs: Vec::new(),
            exclude_globs: Vec::new(),
        }
    }
}

/// Statistics about how files were detected during scanning
#[derive(Debug, Clone, Default, Serialize)]
pub struct ScanStats {
    pub by_extension: usize,
    pub by_content: usize,
    pub skipped_binary: usize,
    pub unreadable: usize,
}

/// A skipped file plus the reason — feeds the output's Merge Report so nothing
/// is ever dropped invisibly.
#[derive(Debug, Clone, Serialize)]
pub struct SkipEntry {
    pub path: String,
    pub reason: String,
}

/// Complete scan result
pub struct ScanResult {
    pub files: Vec<PathBuf>,
    pub stats: ScanStats,
    pub skipped: Vec<SkipEntry>,
}

// ============================================================================
// NAME CLASSIFICATION HELPERS
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

/// Fast check if a directory name indicates a virtual environment (no I/O)
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

/// Minified/generated compound extensions
#[inline]
fn is_minified_filename(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
        || lower.ends_with(".min.mjs")
        || lower.ends_with(".chunk.js")
        || lower.ends_with(".bundle.js")
}

/// Previous TurboMerger outputs — re-merging them snowballs dumps-into-dumps.
/// Covers every output format and split parts; watch mode's stable
/// `*_watch_merged.*` name matches too.
#[inline]
fn is_own_output(name_lower: &str) -> bool {
    let Some(pos) = name_lower.rfind("_merged.") else {
        return false;
    };
    let tail = &name_lower[pos + "_merged.".len()..];
    matches!(tail, "md" | "xml" | "json" | "txt")
        || (tail.starts_with("part")
            && (tail.ends_with(".md")
                || tail.ends_with(".xml")
                || tail.ends_with(".json")
                || tail.ends_with(".txt")))
}

/// Check if an extensionless file has a well-known text filename
#[inline]
fn is_known_extensionless_file(name_lower: &str) -> bool {
    matches!(
        name_lower,
        "makefile"
            | "dockerfile"
            | "vagrantfile"
            | "jenkinsfile"
            | "gemfile"
            | "rakefile"
            | "readme"
            | "license"
            | "changelog"
            | "authors"
            | "contributors"
            | "todo"
            | "cmakelists.txt"
            | "procfile"
            | "brewfile"
            | "podfile"
            | "fastfile"
            | "appfile"
            | "justfile"
            | "taskfile"
            | "earthfile"
            | "tiltfile"
            | "snakefile"
            | "guardfile"
            | "berksfile"
            | "capfile"
            | "thorfile"
            | "puppetfile"
            | "modulefile"
            | "buildfile"
            | "codeowners"
    )
}

// ============================================================================
// CONTENT-BASED BINARY DETECTION
// ============================================================================

/// Read first 8192 bytes and check if the file appears to be text.
///
/// Detection pipeline (in order):
/// 1. Magic bytes + null byte ratio (via security::is_binary_content)
/// 2. Control character ratio >10% → binary
/// 3. Non-ASCII byte ratio >40% → binary/encoded
/// 4. Any line >1000 chars → minified/binary
fn sniff_file_content(path: &Path) -> Result<bool> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut buffer = vec![0u8; SNIFF_SIZE];
    let bytes_read = reader.read(&mut buffer)?;

    if bytes_read == 0 {
        return Ok(true); // Empty file is text
    }

    buffer.truncate(bytes_read);

    if crate::security::is_binary_content(&buffer) {
        return Ok(false);
    }

    let control_count = buffer
        .iter()
        .filter(|&&b| (0x01..=0x08).contains(&b) || (0x0E..=0x1F).contains(&b))
        .count();
    if control_count * 100 > bytes_read * CONTROL_CHAR_THRESHOLD_PCT {
        return Ok(false);
    }

    let high_byte_count = buffer.iter().filter(|&&b| b >= 0x80).count();
    if high_byte_count * 100 > bytes_read * NON_ASCII_THRESHOLD_PCT {
        return Ok(false);
    }

    let max_line_len = buffer
        .split(|&b| b == b'\n')
        .map(|line| line.len())
        .max()
        .unwrap_or(0);
    if max_line_len > MAX_LINE_LENGTH {
        return Ok(false);
    }

    Ok(true)
}

// ============================================================================
// WALK FILTTERING + CLASSIFICATION
// ============================================================================

/// Root-relative display path with forward slashes (shared across modules).
pub fn relative_display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

/// Directory/file filter applied during the walk (cheap name checks only).
fn keep_entry(entry: &ignore::DirEntry, include_venv: bool, include_hidden: bool) -> bool {
    if entry.path_is_symlink() {
        return false;
    }

    let name = entry.file_name().to_string_lossy();
    let name_lower = name.to_lowercase();
    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

    if is_dir {
        // Junction points masquerade as plain dirs — reject via attributes
        #[cfg(windows)]
        if let Ok(meta) = entry.metadata() {
            if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return false;
            }
        }
        if SKIP_DIRS_ALWAYS.contains(name_lower.as_str()) {
            return false;
        }
        if !include_venv && is_venv_by_name(&name_lower) {
            return false;
        }
        if name_lower.starts_with('.')
            && !include_hidden
            && !DOT_DIR_ALLOWLIST.contains(&name_lower.as_str())
        {
            return false;
        }
        return true;
    }

    // Files: hidden gate (dot-prefix) with the config-file allowlist. Sensitive
    // files (.env, *.pem, id_rsa…) are let through even when hidden so they get
    // RECORDED as skipped-with-reason in classify(), never dropped silently.
    if name_lower.starts_with('.')
        && !include_hidden
        && !is_allowlisted_dotfile(&name_lower)
        && sensitive_reason(entry.path()).is_none()
    {
        return false;
    }

    true
}

enum Verdict {
    TextByExt,
    TextByContent,
    Skip(String),
    SkipBinary(String),
    Unreadable,
}

/// Decide whether a single candidate file is merged. Runs on rayon threads.
fn classify(path: &Path, len: u64, options: &ScanOptions) -> Verdict {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return Verdict::Skip("unrepresentable file name".into()),
    };
    let name_lower = name.to_lowercase();

    if SKIP_FILES.iter().any(|&s| s == name_lower) {
        return Verdict::Skip("lock/system file (context bloat)".into());
    }
    if is_own_output(&name_lower) {
        return Verdict::Skip("previous TurboMerger output".into());
    }
    if is_minified_filename(name) {
        return Verdict::SkipBinary("minified/bundled".into());
    }
    if let Some(reason) = sensitive_reason(path) {
        return Verdict::Skip(reason.to_string());
    }
    if len > options.max_file_size {
        return Verdict::Skip(format!("too large ({} KB)", len / 1024));
    }

    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();

        // Step 1: Config exclude list (highest priority user override)
        if options.extra_skip_exts.iter().any(|e| e == &ext_lower) {
            return Verdict::Skip("excluded by turbomerger.toml".into());
        }

        // Step 2: Known binary extension → skip
        if BINARY_EXTENSIONS.contains(ext_lower.as_str())
            || options.extra_binary_exts.iter().any(|e| e == &ext_lower)
        {
            return Verdict::SkipBinary("binary extension".into());
        }

        let known_text = TEXT_EXTENSIONS.contains(ext_lower.as_str())
            || options.extra_text_exts.iter().any(|e| e == &ext_lower);

        // Large files must be known text extensions
        if len > LARGE_FILE_UNKNOWN_EXT && !known_text {
            return Verdict::SkipBinary("large file with unknown extension".into());
        }

        // Step 3: Known text extension → include
        if known_text {
            return Verdict::TextByExt;
        }

        // Step 4: Unknown extension → content sniff if enabled
        if options.content_sniff {
            return match sniff_file_content(path) {
                Ok(true) => Verdict::TextByContent,
                Ok(false) => Verdict::SkipBinary("binary content".into()),
                Err(_) => Verdict::Unreadable,
            };
        }
        Verdict::Skip("unknown extension (content detection off)".into())
    } else {
        // Extensionless files — known names, then optional sniff
        if len > LARGE_FILE_UNKNOWN_EXT && !is_known_extensionless_file(&name_lower) {
            return Verdict::SkipBinary("large file with unknown extension".into());
        }
        if is_known_extensionless_file(&name_lower) {
            Verdict::TextByExt
        } else if options.content_sniff {
            match sniff_file_content(path) {
                Ok(true) => Verdict::TextByContent,
                Ok(false) => Verdict::SkipBinary("binary content".into()),
                Err(_) => Verdict::Unreadable,
            }
        } else {
            Verdict::Skip("no extension (content detection off)".into())
        }
    }
}

// ============================================================================
// MAIN SCANNER
// ============================================================================

/// Scan directory for text files: gitignore-aware walk, then parallel
/// classification with per-file skip reasons.
pub fn scan_text_files(root: &Path, options: &ScanOptions) -> Result<ScanResult> {
    if has_reparse_point_in_path(root).unwrap_or(true) {
        anyhow::bail!("Root path contains junction points or symlinks");
    }

    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false) // hidden handling is ours (dot allowlist in keep_entry)
        .require_git(false) // honor .gitignore even outside a git repo
        .git_ignore(options.respect_gitignore)
        .git_exclude(options.respect_gitignore)
        .ignore(options.respect_gitignore)
        .parents(options.respect_gitignore)
        .git_global(false); // deterministic: user-global excludes don't apply
    builder.add_custom_ignore_filename(".turbomergerignore");

    // User include/exclude globs via ripgrep's override layer. A non-empty
    // whitelist means only matching files survive; `!glob` entries are excludes.
    if !options.include_globs.is_empty() || !options.exclude_globs.is_empty() {
        let mut ob = ignore::overrides::OverrideBuilder::new(root);
        for g in &options.include_globs {
            ob.add(g)
                .map_err(|e| anyhow::anyhow!("bad include glob '{}': {}", g, e))?;
        }
        for g in &options.exclude_globs {
            let pat = if g.starts_with('!') {
                g.clone()
            } else {
                format!("!{}", g)
            };
            ob.add(&pat)
                .map_err(|e| anyhow::anyhow!("bad exclude glob '{}': {}", g, e))?;
        }
        let overrides = ob
            .build()
            .map_err(|e| anyhow::anyhow!("glob build failed: {}", e))?;
        builder.overrides(overrides);
    }

    let include_venv = options.include_venv;
    let include_hidden = options.include_hidden;
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        keep_entry(entry, include_venv, include_hidden)
    });

    // Phase 1 (sequential, heavily pruned): collect candidate files.
    let mut candidates: Vec<(PathBuf, u64)> = Vec::new();
    let mut skipped: Vec<SkipEntry> = Vec::new();
    let mut stats = ScanStats::default();

    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(err) => {
                stats.unreadable += 1;
                skipped.push(SkipEntry {
                    path: err.to_string(),
                    reason: "unreadable during walk".into(),
                });
                continue;
            }
        };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                stats.unreadable += 1;
                skipped.push(SkipEntry {
                    path: relative_display(root, &path),
                    reason: "metadata unreadable".into(),
                });
                continue;
            }
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        #[cfg(windows)]
        {
            let attrs = meta.file_attributes();
            if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                continue;
            }
            if attrs & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0 {
                skipped.push(SkipEntry {
                    path: relative_display(root, &path),
                    reason: "cloud placeholder (not downloaded locally)".into(),
                });
                continue;
            }
        }
        candidates.push((path, meta.len()));
    }

    // Phase 2 (parallel): classify candidates (includes content sniffing).
    let verdicts: Vec<(PathBuf, Verdict)> = candidates
        .into_par_iter()
        .map(|(path, len)| {
            let v = classify(&path, len, options);
            (path, v)
        })
        .collect();

    let mut files = Vec::new();
    for (path, verdict) in verdicts {
        match verdict {
            Verdict::TextByExt => {
                stats.by_extension += 1;
                files.push(path);
            }
            Verdict::TextByContent => {
                stats.by_content += 1;
                files.push(path);
            }
            Verdict::SkipBinary(reason) => {
                stats.skipped_binary += 1;
                skipped.push(SkipEntry {
                    path: relative_display(root, &path),
                    reason,
                });
            }
            Verdict::Skip(reason) => {
                skipped.push(SkipEntry {
                    path: relative_display(root, &path),
                    reason,
                });
            }
            Verdict::Unreadable => {
                stats.unreadable += 1;
                skipped.push(SkipEntry {
                    path: relative_display(root, &path),
                    reason: "unreadable".into(),
                });
            }
        }
    }

    files.sort();
    skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(ScanResult {
        files,
        stats,
        skipped,
    })
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_boundary_detection() {
        assert!(has_word_boundary("venv", "venv", 0));
        assert!(has_word_boundary("my_venv", "venv", 3));
        assert!(has_word_boundary("venv_logs", "venv", 0));
        assert!(has_word_boundary("project-venv", "venv", 8));
        assert!(!has_word_boundary("event", "vent", 1));
        assert!(!has_word_boundary("convenience", "venv", 3));
    }

    #[test]
    fn test_venv_name_detection() {
        assert!(is_venv_by_name("venv"));
        assert!(is_venv_by_name(".venv"));
        assert!(is_venv_by_name("site-packages"));
        assert!(is_venv_by_name("__pycache__"));
        assert!(is_venv_by_name("my_venv"));
        assert!(is_venv_by_name("venv2"));

        assert!(!is_venv_by_name("event"));
        assert!(!is_venv_by_name("convenience"));
        assert!(!is_venv_by_name("inventory"));
        assert!(!is_venv_by_name("src"));
    }

    #[test]
    fn test_skip_dirs_separation() {
        assert!(SKIP_DIRS_ALWAYS.contains("node_modules"));
        assert!(SKIP_DIRS_ALWAYS.contains(".git"));
        assert!(SKIP_DIRS_ALWAYS.contains(".ssh"));
        assert!(SKIP_DIRS_VENV.contains("venv"));
        assert!(!SKIP_DIRS_ALWAYS.contains("venv"));
    }

    #[test]
    fn test_binary_extensions_cover_db_journals() {
        for ext in [
            "exe",
            "png",
            "sqlite3",
            "sqlite-wal",
            "sqlite-shm",
            "db-wal",
            "db-shm",
            "lockb",
        ] {
            assert!(BINARY_EXTENSIONS.contains(ext), "{} missing", ext);
        }
    }

    #[test]
    fn test_dotfile_allowlist() {
        assert!(is_allowlisted_dotfile(".gitignore"));
        assert!(is_allowlisted_dotfile(".mcp.json"));
        assert!(is_allowlisted_dotfile(".env.example"));
        assert!(is_allowlisted_dotfile(".eslintrc.json"));
        assert!(!is_allowlisted_dotfile(".env"));
        assert!(!is_allowlisted_dotfile(".npmrc")); // sensitive: may hold auth tokens
        assert!(!is_allowlisted_dotfile(".secret"));
    }

    #[test]
    fn test_own_output_detection() {
        assert!(is_own_output("apartment_2026-07-09_merged.md"));
        assert!(is_own_output("repo_merged.part1-of-3.md"));
        assert!(is_own_output("repo_merged.xml"));
        assert!(is_own_output("repo_merged.json"));
        assert!(is_own_output("repo_merged.part2-of-3.json"));
        assert!(is_own_output("myrepo_watch_merged.md"));
        assert!(!is_own_output("merged_results.md"));
        assert!(!is_own_output("notes.md"));
        assert!(!is_own_output("data_merged.csv"));
    }

    #[test]
    fn test_extensionless_files_detected() {
        assert!(is_known_extensionless_file("makefile"));
        assert!(is_known_extensionless_file("dockerfile"));
        assert!(is_known_extensionless_file("justfile"));
        assert!(is_known_extensionless_file("codeowners"));
        assert!(!is_known_extensionless_file("randomfile"));
    }

    #[test]
    fn test_scan_options_default() {
        let opts = ScanOptions::default();
        assert!(!opts.include_venv);
        assert!(opts.content_sniff);
        assert!(!opts.include_hidden);
        assert!(opts.respect_gitignore);
        assert_eq!(opts.max_file_size, 2 * 1024 * 1024);
    }

    #[test]
    fn test_content_sniff_detects_utf8_text() {
        let dir = std::env::temp_dir().join("turbomerger_test_sniff_text");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.xyz");
        std::fs::write(&file_path, b"Hello, world!\nThis is a text file.\n").unwrap();
        assert!(sniff_file_content(&file_path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_content_sniff_detects_binary() {
        let dir = std::env::temp_dir().join("turbomerger_test_sniff_binary");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("test.xyz");
        let mut data = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        data.extend_from_slice(&[0x00; 100]);
        std::fs::write(&file_path, &data).unwrap();
        assert!(!sniff_file_content(&file_path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sniff_rejects_minified_js() {
        let dir = std::env::temp_dir().join("turbomerger_test_minified");
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join("bundle.xyz");
        std::fs::write(&file_path, "a".repeat(5000).as_bytes()).unwrap();
        assert!(!sniff_file_content(&file_path).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_minified_extension_patterns() {
        assert!(is_minified_filename("vendor.min.js"));
        assert!(is_minified_filename("app.chunk.js"));
        assert!(is_minified_filename("VENDOR.MIN.JS"));
        assert!(!is_minified_filename("app.js"));
    }

    #[test]
    fn test_gitignore_respected_in_scan() {
        let dir = std::env::temp_dir().join("turbomerger_test_gitignore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".gitignore"), "profiles/\n").unwrap();
        std::fs::write(dir.join("profiles/cookies.txt"), "cf_clearance=abc\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let result = scan_text_files(&dir, &ScanOptions::default()).unwrap();
        let names: Vec<String> = result
            .files
            .iter()
            .map(|f| relative_display(&dir, f))
            .collect();
        assert!(names.contains(&"src/main.rs".to_string()), "{:?}", names);
        assert!(
            names.contains(&".gitignore".to_string()),
            "dot-config allowlist should include .gitignore: {:?}",
            names
        );
        assert!(
            !names.iter().any(|n| n.starts_with("profiles/")),
            "gitignored profiles/ must be excluded: {:?}",
            names
        );

        // and with respect_gitignore=false the cookie file comes back
        let opts = ScanOptions {
            respect_gitignore: false,
            ..Default::default()
        };
        let result2 = scan_text_files(&dir, &opts).unwrap();
        let names2: Vec<String> = result2
            .files
            .iter()
            .map(|f| relative_display(&dir, f))
            .collect();
        assert!(
            names2.iter().any(|n| n.starts_with("profiles/")),
            "{:?}",
            names2
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
